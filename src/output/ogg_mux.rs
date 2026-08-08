//! Minimal Ogg page muxer used to wrap Opus packets into an Ogg/Opus stream
//! accepted by libshout (and Icecast). Implements the Ogg framing format with
//! the exact CRC-32 variant used by libogg so pages validate on the wire.

/// Builds Ogg pages from whole packets. Packets are never split across pages.
pub struct OggMuxer {
    serial: u32,
    seq: u32,
    /// Cumulative granule position of the last packet completed in the page
    /// currently being assembled.
    page_granule: i64,
    /// True while the page under construction holds the first (BOS) packet.
    bos: bool,
    page_body: Vec<u8>,
    page_segs: Vec<u8>,
    pending: Vec<u8>,
}

impl OggMuxer {
    pub fn new(serial: u32) -> Self {
        Self {
            serial,
            seq: 0,
            page_granule: 0,
            bos: true,
            page_body: Vec::new(),
            page_segs: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// Write one complete packet. `granule` is the granule position of the
    /// last sample encoded by this packet. `is_bos` marks the first page.
    pub fn write_packet(&mut self, data: &[u8], granule: i64) {
        let segs = segment_table(data.len());
        if self.page_segs.len() + segs.len() > 255 {
            self.flush_page(false);
        }
        self.page_body.extend_from_slice(data);
        self.page_segs.extend_from_slice(&segs);
        self.page_granule = granule;
    }

    /// Force-flush the page under construction (no EOS). Used to isolate the
    /// Opus header packets into their own pages.
    pub fn flush(&mut self) {
        if !self.page_body.is_empty() || !self.page_segs.is_empty() {
            self.flush_page(false);
        }
    }

    /// Flush the final page (EOS).
    pub fn finish(&mut self) {
        if !self.page_body.is_empty() || !self.page_segs.is_empty() {
            self.flush_page(true);
        }
    }

    /// Take the bytes of all completed pages (headers + audio).
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    fn flush_page(&mut self, eos: bool) {
        let mut header = Vec::with_capacity(27 + self.page_segs.len());
        header.extend_from_slice(b"OggS");
        header.push(0); // stream structure version
        let flags = (if self.bos { 0x02 } else { 0 }) | (if eos { 0x04 } else { 0 });
        header.push(flags);
        header.extend_from_slice(&self.page_granule.to_le_bytes());
        header.extend_from_slice(&self.serial.to_le_bytes());
        header.extend_from_slice(&self.seq.to_le_bytes());
        header.extend_from_slice(&[0, 0, 0, 0]); // checksum placeholder
        header.push(self.page_segs.len() as u8);
        header.extend_from_slice(&self.page_segs);

        let crc = crc32(&header, &self.page_body);
        header[22..26].copy_from_slice(&crc.to_le_bytes());

        self.pending.extend_from_slice(&header);
        self.pending.extend_from_slice(&self.page_body);

        self.seq += 1;
        self.page_body.clear();
        self.page_segs.clear();
        self.page_granule = 0;
        self.bos = false;
    }
}

/// Split a packet into its 255-byte segment table entries. A packet whose
/// length is an exact multiple of 255 gets a trailing zero segment.
fn segment_table(len: usize) -> Vec<u8> {
    let mut segs = Vec::new();
    let mut n = len;
    while n >= 255 {
        segs.push(255);
        n -= 255;
    }
    segs.push(n as u8);
    segs
}

/// The Ogg CRC-32 (poly 0x04c11db7, no reflection, no final xor).
///
/// Table-driven MSB-first update: the next input byte xors into the *index*
/// (the crc's top byte), not into the result.
fn crc32(header: &[u8], body: &[u8]) -> u32 {
    let table = CRC_TABLE.get_or_init(crc_table);
    let mut crc: u32 = 0;
    for &b in header.iter().chain(body) {
        let idx = (((crc >> 24) ^ b as u32) & 0xff) as usize;
        crc = table[idx] ^ (crc << 8);
    }
    crc
}

use std::sync::OnceLock;

static CRC_TABLE: OnceLock<[u32; 256]> = OnceLock::new();

fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    for (i, entry) in table.iter_mut().enumerate() {
        let mut r = (i as u32) << 24;
        for _ in 0..8 {
            r = if r & 0x8000_0000 != 0 {
                (r << 1) ^ 0x04c1_1db7
            } else {
                r << 1
            };
        }
        *entry = r;
    }
    table
}

/// Build an `OpusHead` identification packet (pre-skip 0, no channel mapping).
pub fn opus_head_packet(channels: u16) -> Vec<u8> {
    let mut p = Vec::with_capacity(19);
    p.extend_from_slice(b"OpusHead");
    p.push(1); // version
    p.push(channels as u8);
    p.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
    p.extend_from_slice(&48_000u32.to_le_bytes()); // input sample rate
    p.extend_from_slice(&0i16.to_le_bytes()); // output gain
    p.push(0); // channel mapping family
    p
}

/// Build an `OpusTags` comment packet.
pub fn opus_tags_packet(title: &str) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(b"OpusTags");
    let vendor = "Crabsoup";
    p.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    p.extend_from_slice(vendor.as_bytes());

    let comment = format!("title={}", sanitize_comment(title));
    p.extend_from_slice(&1u32.to_le_bytes()); // one comment
    p.extend_from_slice(&(comment.len() as u32).to_le_bytes());
    p.extend_from_slice(comment.as_bytes());
    p
}

fn sanitize_comment(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_valid_ogg_pages() {
        let mut mux = OggMuxer::new(0x1234_5678);
        mux.write_packet(&opus_head_packet(2), 0);
        mux.flush();
        mux.write_packet(&opus_tags_packet("Crabsoup"), 0);
        mux.flush();

        // Two 20ms audio packets -> 960 samples each at 48 kHz.
        mux.write_packet(&[0u8; 60], 960);
        mux.write_packet(&[0u8; 60], 1920);
        mux.finish();

        let out = mux.take_output();
        let pages = split_pages(&out);
        // Head page, tags page, then one audio page (both packets coalesced
        // into a single page; the encoder flushes per packet separately).
        assert_eq!(pages.len(), 3);
        // BOS flag on the first page, EOS on the last.
        assert_eq!(pages[0][5], 0x02);
        assert_eq!(pages[2][5] & 0x04, 0x04);
        // The audio page's granule position reflects 2 audio packets.
        let granule = u64::from_le_bytes(pages[2][6..14].try_into().unwrap());
        assert_eq!(granule, 1920);
        // Non-zero CRC on every page (correctly computed, not all-zero).
        for p in &pages {
            let crc = u32::from_le_bytes(p[22..26].try_into().unwrap());
            assert_ne!(crc, 0);
        }
    }

    fn split_pages(bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut pages = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            assert_eq!(&bytes[i..i + 4], b"OggS");
            let nsegs = bytes[i + 26] as usize;
            let segs = &bytes[i + 27..i + 27 + nsegs];
            let body_len: usize = segs.iter().map(|&s| s as usize).sum();
            let page = &bytes[i..i + 27 + nsegs + body_len];
            pages.push(page.to_vec());
            i += page.len();
        }
        assert_eq!(i, bytes.len());
        pages
    }

    #[test]
    fn segment_table_handles_255_boundary() {
        assert_eq!(segment_table(254), vec![254]);
        assert_eq!(segment_table(255), vec![255, 0]);
        assert_eq!(segment_table(256), vec![255, 1]);
        assert_eq!(segment_table(300), vec![255, 45]);
    }

    #[test]
    fn crc_matches_external_reference() {
        // Page 1 of a real stream: header (checksum zeroed) + 19-byte OpusHead.
        // Expected CRC computed with an independent implementation (verified
        // against ffmpeg-produced Ogg files).
        let hdr = hex("4f676753000200000000000000004342414300000000000000000113");
        let body = hex("4f707573486561640102000080bb0000000000");
        let crc = crc32(&hdr, &body);
        assert_eq!(crc, 0xae3e_7e5f, "CRC does not match external reference");
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}
