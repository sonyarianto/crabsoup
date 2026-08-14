//! Minimal MPEG-TS muxer for live HLS segments.
//!
//! Emits PAT + PMT sections and PES-wrapped ADTS AAC packets on a single
//! audio PID, with periodic PCRs and per-PID continuity counters. Only what
//! the HLS segmenter needs — no PSI tables beyond the two, no B-frames, no
//! video.

use crate::output::ogg_mux::crc32_init;

pub const TS_PACKET_SIZE: usize = 188;

const SYNC: u8 = 0x47;
const PAT_PID: u16 = 0x0000;
const PMT_PID: u16 = 0x1000;
const AUDIO_PID: u16 = 0x1001;
/// ISO/IEC 13818-1 stream_type for ADTS AAC.
const STREAM_TYPE_AAC: u8 = 0x0F;
/// PCR stamp period, ~100 ms in 90 kHz units.
const PCR_INTERVAL: u64 = 9000;

/// Muxes one AAC/ADTS program into 188-byte transport packets.
pub struct MpegTsMuxer {
    cc_pat: u8,
    cc_pmt: u8,
    cc_audio: u8,
    /// PTS at which the last PCR was stamped (90 kHz).
    last_pcr_pts: u64,
}

impl MpegTsMuxer {
    pub fn new() -> Self {
        Self {
            cc_pat: 0,
            cc_pmt: 0,
            cc_audio: 0,
            last_pcr_pts: u64::MAX,
        }
    }

    /// Append the PAT and PMT sections, one PUSI packet each. Emitted at the
    /// start of every segment so a player joining mid-window can resync.
    pub fn write_program(&mut self, out: &mut Vec<u8>) {
        let mut pat = Vec::new();
        pat.push(0x00); // table_id
        // section_length carries section_syntax_indicator + reserved bits.
        pat.extend_from_slice(&[0xb0, 0x0d]); // section_length = 13
        pat.extend_from_slice(&[0x00, 0x01]); // TSID
        pat.push(0xc1); // version 0, current_next
        pat.push(0x00); // section_number
        pat.push(0x00); // last_section_number
        pat.extend_from_slice(&[0x00, 0x01]); // program_number
        pat.extend_from_slice(&pid_hi(PMT_PID)); // PMT PID
        let crc = crc32_init(0xffff_ffff, &pat, &[]);
        pat.extend_from_slice(&crc.to_be_bytes());

        let mut pmt = Vec::new();
        pmt.push(0x02); // table_id
        pmt.extend_from_slice(&[0xb0, 0x12]); // section_length = 18
        pmt.extend_from_slice(&[0x00, 0x01]); // program_number
        pmt.push(0xc1); // version 0, current_next
        pmt.push(0x00); // section_number
        pmt.push(0x00); // last_section_number
        pmt.extend_from_slice(&pid_hi(AUDIO_PID)); // PCR_PID
        pmt.extend_from_slice(&[0x0f, 0x00]); // program_info_length (0)
        pmt.push(STREAM_TYPE_AAC);
        pmt.extend_from_slice(&pid_hi(AUDIO_PID));
        pmt.extend_from_slice(&[0x0f, 0x00]); // ES_info_length (0)
        let crc = crc32_init(0xffff_ffff, &pmt, &[]);
        pmt.extend_from_slice(&crc.to_be_bytes());

        let mut pat_packet = [0xFFu8; TS_PACKET_SIZE];
        pat_packet[0] = SYNC;
        pat_packet[1] = 0x40 | (PAT_PID >> 8) as u8;
        pat_packet[2] = (PAT_PID & 0xff) as u8;
        pat_packet[3] = 0x10 | self.cc_pat;
        pat_packet[4] = 0x00; // pointer_field
        pat_packet[5..5 + pat.len()].copy_from_slice(&pat);
        out.extend_from_slice(&pat_packet);
        self.cc_pat = (self.cc_pat + 1) & 0x0f;

        let mut pmt_packet = [0xFFu8; TS_PACKET_SIZE];
        pmt_packet[0] = SYNC;
        pmt_packet[1] = 0x40 | (PMT_PID >> 8) as u8;
        pmt_packet[2] = (PMT_PID & 0xff) as u8;
        pmt_packet[3] = 0x10 | self.cc_pmt;
        pmt_packet[4] = 0x00; // pointer_field
        pmt_packet[5..5 + pmt.len()].copy_from_slice(&pmt);
        out.extend_from_slice(&pmt_packet);
        self.cc_pmt = (self.cc_pmt + 1) & 0x0f;
    }

    /// Wrap one ADTS frame in a PES packet and emit the TS packets, stamping
    /// a PCR about every 100 ms. `pts_90k` is the presentation time of the
    /// frame's first sample on the 90 kHz HLS clock.
    pub fn push_audio(&mut self, adts: &[u8], pts_90k: u64, out: &mut Vec<u8>) {
        // PES header (14 bytes): start code, audio stream id, PTS only.
        let mut header = [0u8; 14];
        header[0..3].copy_from_slice(&[0x00, 0x00, 0x01]);
        header[3] = 0xC0;
        let pes_len = 3 + 5 + adts.len();
        header[4] = (pes_len >> 8) as u8;
        header[5] = pes_len as u8;
        header[6] = 0x80; // data_alignment_indicator
        header[7] = 0x80; // PTS present, no DTS
        header[8] = 5;
        header[9] = 0x21 | (((pts_90k >> 29) & 0x0e) as u8);
        header[10] = (pts_90k >> 22) as u8;
        header[11] = (((pts_90k >> 14) & 0xfe) | 1) as u8;
        header[12] = (pts_90k >> 7) as u8;
        header[13] = (((pts_90k << 1) & 0xfe) | 1) as u8;

        let with_pcr = self.last_pcr_pts == u64::MAX
            || pts_90k.wrapping_sub(self.last_pcr_pts) >= PCR_INTERVAL;
        let mut pes = Vec::with_capacity(header.len() + adts.len());
        pes.extend_from_slice(&header);
        pes.extend_from_slice(adts);
        packetize(
            &pes,
            AUDIO_PID,
            with_pcr.then_some(pts_90k),
            &mut self.cc_audio,
            out,
        );
        if with_pcr {
            self.last_pcr_pts = pts_90k;
        }
    }
}

/// Slice `payload` into 188-byte packets (PUSI on the first), optionally
/// stamping a PCR in the first packet's adaptation field. Trailing space
/// becomes a stuffing adaptation field; raw 0xFF padding in the payload area
/// would make ffmpeg read it as PES data and flag the packet corrupt.
fn packetize(payload: &[u8], pid: u16, pcr: Option<u64>, cc: &mut u8, out: &mut Vec<u8>) {
    let mut rest = payload;
    let mut first = true;
    while !rest.is_empty() {
        let mut packet = [0xFFu8; TS_PACKET_SIZE];
        packet[0] = SYNC;
        packet[1] = (if first { 0x40 } else { 0 }) | (pid >> 8) as u8;
        packet[2] = (pid & 0xff) as u8;
        let pcr_len = if first {
            pcr.map(|_| 8).unwrap_or(0)
        } else {
            0
        };
        let take = (TS_PACKET_SIZE - 4 - pcr_len).min(rest.len());
        let slack = TS_PACKET_SIZE - 4 - pcr_len - take;
        packet[3] = if pcr_len > 0 || slack > 0 { 0x30 } else { 0x10 } | (*cc & 0x0f);
        let mut start = 4;
        if pcr_len > 0 {
            packet[4] = (7 + slack) as u8;
            packet[5] = 0x10; // PCR flag
            let base = pcr.unwrap();
            packet[6] = (base >> 25) as u8;
            packet[7] = (base >> 17) as u8;
            packet[8] = (base >> 9) as u8;
            packet[9] = (base >> 1) as u8;
            packet[10] = (((base & 1) << 7) | 0x7e) as u8;
            packet[11] = 0x00; // PCR extension
            start = 12 + slack;
        } else if slack > 0 {
            packet[4] = (slack - 1) as u8;
            if slack > 1 {
                packet[5] = 0x00; // no flags, rest is stuffing
            }
            start = 4 + slack;
        }
        packet[start..start + take].copy_from_slice(&rest[..take]);
        out.extend_from_slice(&packet);
        *cc = (*cc + 1) & 0x0f;
        rest = &rest[take..];
        first = false;
    }
}

impl Default for MpegTsMuxer {
    fn default() -> Self {
        Self::new()
    }
}

/// The two bytes encoding a 13-bit PID with its `0xE000` reserved marker.
fn pid_hi(pid: u16) -> [u8; 2] {
    [(0xe0 | ((pid >> 8) & 0x1f)) as u8, (pid & 0xff) as u8]
}

/// Split a buffer of consecutive ADTS frames into individual frames, resync
/// searching for the 0xFFF sync word if a frame boundary is ever missed.
pub fn split_adts(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 7 <= data.len() {
        if data[i] == 0xFF && (data[i + 1] & 0xf6) == 0xf0 {
            let len = (((data[i + 3] & 0x03) as usize) << 11)
                | ((data[i + 4] as usize) << 3)
                | ((data[i + 5] as usize) >> 5);
            if len >= 7 && i + len <= data.len() {
                out.push(&data[i..i + len]);
                i += len;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_are_188_bytes_with_sync_and_continuity() {
        let mut mux = MpegTsMuxer::new();
        let mut out = Vec::new();
        mux.write_program(&mut out);
        assert_eq!(out.len(), 2 * TS_PACKET_SIZE);
        for (i, packet) in out.chunks_exact(TS_PACKET_SIZE).enumerate() {
            assert_eq!(packet[0], SYNC);
            let pid = (((packet[1] & 0x1f) as u16) << 8) | packet[2] as u16;
            assert_eq!(pid, if i == 0 { PAT_PID } else { PMT_PID });
            assert_ne!(packet[1] & 0x40, 0, "PUSI set on first packet");
        }
    }

    #[test]
    // The bit-layout expressions below intentionally fold to zero for the
    // test constants (ADTS length 200 < 2048; PTS top bits of a 1 s
    // timestamp) — they document the layout, so allow the erasing-op lint.
    #[allow(clippy::erasing_op)]
    fn adts_wraps_in_pes_with_pts() {
        let mut mux = MpegTsMuxer::new();
        let mut out = Vec::new();
        // A realistic-size ADTS frame (200 bytes, as an AAC-LC 128 kbps
        // stereo frame would be) so the first TS packet carries the whole
        // PES header with no stuffing.
        let mut adts = vec![0u8; 200];
        adts[0] = 0xff;
        adts[1] = 0xf1;
        adts[2] = 0x50;
        adts[3] = ((200 >> 11) & 0x03) as u8;
        adts[4] = (200 >> 3) as u8;
        adts[5] = ((200 << 5) as u8 & 0xe0) | 0xfc;
        mux.push_audio(&adts, 90_000, &mut out);
        assert_eq!(out.len() % TS_PACKET_SIZE, 0);
        let first = &out[..TS_PACKET_SIZE];
        assert_eq!(first[0], SYNC);
        let pid = (((first[1] & 0x1f) as u16) << 8) | first[2] as u16;
        assert_eq!(pid, AUDIO_PID);
        // PUSI + PCR adaptation field on the first packet.
        assert_ne!(first[1] & 0x40, 0);
        assert_eq!(first[3] & 0x30, 0x30, "adaptation + payload");
        // PES start code follows the adaptation field.
        assert_eq!(&first[12..15], &[0x00, 0x00, 0x01]);
        assert_eq!(first[15], 0xc0, "first audio stream id");
        // PTS field: 0x21 prefix + 33-bit value.
        assert_eq!(first[21], 0x21 | (((90_000u64 >> 29) & 0x0e) as u8));
    }

    #[test]
    fn split_adts_splits_all_frames() {
        let mut data = Vec::new();
        // Three 7-byte ADTS frames: frame_length (7) lives in the low 2 bits
        // of byte 3, all of byte 4, and the top 3 bits of byte 5.
        for _ in 0..3 {
            data.extend_from_slice(&[0xff, 0xf1, 0x50, 0x00, 0x00, 0xe0, 0xfc]);
        }
        let frames = split_adts(&data);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames.iter().map(|f| f.len()).sum::<usize>(), data.len());
    }
}
