//! Native Ogg/Opus decode path.
//!
//! symphonia 0.5 has no Opus codec, so DJ uploads of Opus (and `.opus` /
//! `.ogg` files) air silence in the old path. This module reads Ogg pages,
//! reassembles Opus packets from the lacing table (including packets split
//! across pages), and decodes them with `audiopus` at 48 kHz, normalising to
//! the bus spec with the shared [`PcmConverter`].

use std::collections::VecDeque;
use std::io::Read;

use audiopus::coder::Decoder as OpusDecoder;
use audiopus::packet::Packet;
use audiopus::{Channels as OpusChannels, MutSignals, SampleRate};
use log::warn;
use symphonia::core::audio::{Channels, SignalSpec};

use crate::Result;
use crate::output::ogg_mux::crc32;
use crate::source::{AudioSource, PcmConverter};

/// Longest decodable Opus frame (120 ms at 48 kHz, stereo).
const MAX_OPUS_SAMPLES: usize = 5760 * 2;

/// A streaming Ogg page reader + Opus packet assembler over any blocking
/// `Read` (file or TCP stream). Yields *audio* packets; the OpusHead /
/// OpusTags header packets are consumed internally.
pub struct OggOpusDemux<R: Read> {
    reader: R,
    /// Serial of the logical stream being decoded; a new BOS page with a
    /// different serial ends the stream (chained Ogg).
    serial: Option<u32>,
    /// An unfinished packet (last lace used was 255), carried to the next page.
    carry: Vec<u8>,
    /// Completed audio packets waiting to be returned. A FIFO: real-world
    /// Ogg files pack many packets per page (ffmpeg: ~50), and every
    /// completed packet must survive until `next_packet` yields it.
    pending: VecDeque<Vec<u8>>,
    /// Channels from OpusHead (1 or 2; other channel mappings are rejected).
    channels: u8,
    /// Pre-skip in samples at 48 kHz, declared in OpusHead.
    preskip: u16,
    /// Granule position of the last completed page.
    granule: i64,
    /// True once the EOS page was drained (or EOF / chained BOS hit).
    eos: bool,
    started: bool,
}

/// One parsed Ogg page (header fields absent the checksum, which is
/// verified on read).
struct Page {
    flags: u8,
    granule: i64,
    serial: u32,
    body: Vec<u8>,
    laces: Vec<u8>,
}

impl<R: Read> OggOpusDemux<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            serial: None,
            carry: Vec::new(),
            pending: VecDeque::new(),
            channels: 0,
            preskip: 0,
            granule: 0,
            eos: false,
            started: false,
        }
    }

    /// Next audio packet, skipping headers. `Ok(None)` == end of stream.
    /// Returns an error on framing violations (not an Opus stream); I/O
    /// errors propagate as `Err`.
    pub fn next_packet(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if let Some(p) = self.pending.pop_front() {
                return Ok(Some(p));
            }
            if self.eos {
                return Ok(None);
            }
            let page = match self.read_page()? {
                Some(p) => p,
                None => {
                    // EOF before the EOS page (e.g. a DJ dropped the
                    // connection mid-stream). An unterminated packet
                    // cannot be decoded.
                    self.eos = true;
                    if !self.carry.is_empty() {
                        warn!("opus source: truncated packet at end of stream");
                        self.carry.clear();
                    }
                    return Ok(None);
                }
            };
            let (flags, granule, serial, body, laces) = {
                let Page {
                    flags,
                    granule,
                    serial,
                    body,
                    laces,
                } = page;
                (flags, granule, serial, body, laces)
            };
            self.granule = granule;

            match self.serial {
                Some(s) if flags & 0x02 != 0 && serial != s => {
                    // Chained Ogg: stop at the next logical stream.
                    self.eos = true;
                }
                None => {
                    if flags & 0x02 == 0 {
                        return Err("not an Ogg stream (no BOS page)".into());
                    }
                    self.serial = Some(serial);
                }
                _ => {}
            }
            if flags & 0x04 != 0 {
                self.eos = true;
            }

            self.assemble(flags, body, &laces)?;
        }
    }

    /// Channels declared in OpusHead (valid after the first audio packet was
    /// produced, i.e. once the header packet was consumed).
    pub fn channels(&self) -> u8 {
        self.channels
    }

    /// Pre-skip in samples (OpusHead field): the decoder output starts this
    /// many samples into the stream.
    pub fn preskip(&self) -> u16 {
        self.preskip
    }

    /// Total stream length in samples at 48 kHz, once the final page (EOS)
    /// has been seen.
    pub fn total_granule(&self) -> Option<u64> {
        (self.eos && self.granule > 0).then_some(self.granule as u64)
    }

    /// Read one page, retrying past pages whose CRC does not match (a
    /// corrupt page is dropped rather than fed to the decoder). `Ok(None)`
    /// on clean EOF.
    fn read_page(&mut self) -> Result<Option<Page>> {
        loop {
            let mut hdr = [0u8; 27];
            let mut got = 0;
            while got < 27 {
                match self.reader.read(&mut hdr[got..]) {
                    Ok(0) => return Ok(None),
                    Ok(n) => got += n,
                    Err(e) => return Err(e.into()),
                }
            }
            if &hdr[0..4] != b"OggS" || hdr[4] != 0 {
                return Err("not an Ogg stream (bad page magic)".into());
            }
            let nsegs = hdr[26] as usize;
            let mut laces = vec![0u8; nsegs];
            let mut got = 0;
            while got < nsegs {
                match self.reader.read(&mut laces[got..]) {
                    Ok(0) => {
                        warn!("opus source: truncated page (lacing table)");
                        return Ok(None);
                    }
                    Ok(n) => got += n,
                    Err(e) => return Err(e.into()),
                }
            }
            let body_len: usize = laces.iter().map(|&l| l as usize).sum();
            let mut body = vec![0u8; body_len];
            let mut got = 0;
            while got < body_len {
                match self.reader.read(&mut body[got..]) {
                    Ok(0) => {
                        warn!("opus source: truncated page (body)");
                        return Ok(None);
                    }
                    Ok(n) => got += n,
                    Err(e) => return Err(e.into()),
                }
            }

            // CRC-32/MPEG-2 over the header (checksum field zeroed) + lacing
            // table + body — mirroring OggMuxer's computation.
            let mut crc_data = Vec::with_capacity(27 + laces.len());
            crc_data.extend_from_slice(&hdr[..22]);
            crc_data.extend_from_slice(&[0, 0, 0, 0]);
            crc_data.push(nsegs as u8);
            crc_data.extend_from_slice(&laces);
            let crc = crc32(&crc_data, &body);
            let stored = u32::from_le_bytes(hdr[22..26].try_into().unwrap());
            if crc != stored {
                warn!("opus source: Ogg page CRC mismatch, skipping page");
                continue;
            }

            let flags = hdr[5];
            let granule = i64::from_le_bytes(hdr[6..14].try_into().unwrap());
            let serial = u32::from_le_bytes(hdr[14..18].try_into().unwrap());
            return Ok(Some(Page {
                flags,
                granule,
                serial,
                body,
                laces,
            }));
        }
    }

    /// Run one page's lacing table through the packet assembler.
    fn assemble(&mut self, flags: u8, body: Vec<u8>, laces: &[u8]) -> Result<()> {
        let continued = flags & 0x01 != 0;
        if continued && self.carry.is_empty() {
            // Continued-lacing flag with nothing carried is a client bug;
            // treat the laces as a fresh packet rather than dropping the page.
            warn!("opus source: spurious continued-packet flag in Ogg page");
        }
        if !continued && !self.carry.is_empty() {
            // A packet cannot resume without the continued flag.
            warn!("opus source: unterminated packet across a non-continued page");
            self.carry.clear();
        }
        let mut pos = 0usize;
        for &lace in laces {
            let end = (pos + lace as usize).min(body.len());
            self.carry.extend_from_slice(&body[pos..end]);
            pos = end;
            if lace < 255 {
                self.complete_packet()?;
            }
        }
        Ok(())
    }

    /// A lacing-table run finished a packet; consume the Opus header
    /// packets in-stream and park audio packets for `next_packet`.
    fn complete_packet(&mut self) -> Result<()> {
        let pkt = std::mem::take(&mut self.carry);
        if !self.started {
            if !pkt.starts_with(b"OpusHead") {
                return Err("not an Opus stream (no OpusHead)".into());
            }
            if pkt.len() < 19 {
                return Err("truncated OpusHead".into());
            }
            self.channels = pkt[9];
            self.preskip = u16::from_le_bytes([pkt[10], pkt[11]]);
            if self.channels > 2 {
                return Err(format!("Opus channel mapping {} not supported", self.channels).into());
            }
            self.started = true;
            return Ok(());
        }
        if pkt.starts_with(b"OpusTags") {
            return Ok(());
        }
        if !pkt.is_empty() {
            self.pending.push_back(pkt);
        }
        Ok(())
    }
}

fn opus_spec(channels: u16) -> SignalSpec {
    let ch = if channels == 1 {
        Channels::FRONT_CENTRE
    } else {
        Channels::FRONT_LEFT | Channels::FRONT_RIGHT
    };
    SignalSpec::new(48_000, ch)
}

fn opus_ch(channels: u16) -> OpusChannels {
    match channels {
        1 => OpusChannels::Mono,
        _ => OpusChannels::Stereo,
    }
}

/// Decode one Opus packet to interleaved f32 PCM at 48 kHz, trimming any
/// pre-skip still due. Returns the frames decoded (pre-trim) or `None` on a
/// packet error (the stream continues).
fn decode_packet(
    decoder: &mut OpusDecoder,
    channels: u16,
    packet: &[u8],
    preskip_left: &mut u64,
    out: &mut Vec<f32>,
) -> Option<usize> {
    let mut scratch = [0f32; MAX_OPUS_SAMPLES];
    let p: Packet = match packet.try_into() {
        Ok(p) => p,
        Err(e) => {
            warn!("opus source: bad opus packet: {e}");
            return None;
        }
    };
    let signals = match MutSignals::try_from(&mut scratch[..]) {
        Ok(s) => s,
        Err(e) => {
            warn!("opus source: output buffer: {e}");
            return None;
        }
    };
    let frames = match decoder.decode_float(Some(p), signals, false) {
        Ok(n) => n,
        Err(e) => {
            warn!("opus source: opus decode failed, skipping packet: {e}");
            return None;
        }
    };
    let skip = (*preskip_left as usize).min(frames);
    *preskip_left -= skip as u64;
    out.clear();
    out.extend_from_slice(&scratch[skip * channels as usize..frames * channels as usize]);
    Some(frames)
}

/// Opus-in-Ogg as an [`AudioSource`], normalised to the bus spec.
/// `single()`/`playlist()` and the live harbor fall back to this when
/// symphonia cannot decode a stream.
pub struct OpusSource<R: Read + Send> {
    demux: OggOpusDemux<R>,
    decoder: OpusDecoder,
    channels: u16,
    converter: PcmConverter,
    preskip_left: u64,
    /// Samples decoded so far, in the 48 kHz stream clock.
    elapsed_48k: u64,
    total_48k: Option<u64>,
    /// First audio packet, held back while the decoder was created.
    readahead: Option<Vec<u8>>,
    buf: Vec<f32>, // bus-spec interleaved samples
    pos: usize,
    frames_per_buffer: usize,
    eof: bool,
    label: String,
}

impl<R: Read + Send> OpusSource<R> {
    /// Open an Opus stream. Consumes header packets until the first audio
    /// packet so channels/pre-skip are known before playback.
    pub fn open(
        reader: R,
        target: SignalSpec,
        frames_per_buffer: usize,
        label: String,
    ) -> Result<Self> {
        let mut demux = OggOpusDemux::new(reader);
        let readahead = match demux.next_packet()? {
            Some(p) => p,
            None => return Err("stream ended before the first Opus audio packet".into()),
        };
        let channels = demux.channels() as u16;
        let preskip = demux.preskip() as u64;
        let decoder = OpusDecoder::new(SampleRate::Hz48000, opus_ch(channels))
            .map_err(|e| format!("failed to create opus decoder: {e}"))?;
        let converter = PcmConverter::new(target);
        Ok(Self {
            demux,
            decoder,
            channels,
            converter,
            preskip_left: preskip,
            elapsed_48k: 0,
            total_48k: None,
            readahead: Some(readahead),
            buf: Vec::new(),
            pos: 0,
            frames_per_buffer,
            eof: false,
            label,
        })
    }

    /// Total stream duration in seconds at the 48 kHz clock, once the final
    /// page has been read.
    pub fn total_seconds(&self) -> Option<f64> {
        self.total_48k.map(|g| g as f64 / 48_000.0)
    }

    /// Mechanical peek: the harbor sniffs the stream's first page to decide
    /// between the symphonia path (Vorbis etc.) and this native path.
    fn fill(&mut self, needed: usize) {
        while self.buf.len() - self.pos < needed && !self.eof {
            let pkt = match self.readahead.take() {
                Some(p) => p,
                None => match self.demux.next_packet() {
                    Ok(Some(p)) => p,
                    Ok(None) => {
                        self.eof = true;
                        self.total_48k = self.demux.total_granule();
                        break;
                    }
                    Err(e) => {
                        warn!("opus source: {e}");
                        self.eof = true;
                        break;
                    }
                },
            };
            let mut decoded = Vec::new();
            let Some(frames) = decode_packet(
                &mut self.decoder,
                self.channels,
                &pkt,
                &mut self.preskip_left,
                &mut decoded,
            ) else {
                continue;
            };
            self.elapsed_48k += frames as u64;
            if decoded.is_empty() {
                continue;
            }
            let spec = opus_spec(self.channels);
            let converted = self.converter.convert(&decoded, &spec);
            self.buf.extend_from_slice(&converted);
        }
    }
}

impl<R: Read + Send> AudioSource for OpusSource<R> {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        let chans = self.converter.target_channels();
        let want = self.frames_per_buffer * chans;
        self.fill(buffer.len().max(want));

        let available = self.buf.len() - self.pos;
        let n = available.min(buffer.len());
        buffer[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;

        // Compact once a full buffer has been consumed.
        if self.pos >= want {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        n
    }

    fn is_exhausted(&self) -> bool {
        self.eof && self.buf.len() - self.pos == 0
    }

    fn remaining_seconds(&self) -> Option<f64> {
        let total = self.total_48k?;
        Some(((total - self.elapsed_48k) as f64 / 48_000.0).max(0.0))
    }

    fn label(&self) -> Option<String> {
        Some(self.label.clone())
    }
}

/// Yield a prefix (the sniffed first page) before the rest of the stream.
/// Lets the harbor peek at the first page without consuming it.
pub(crate) struct PrependReader<R: Read> {
    prefix: Vec<u8>,
    pos: usize,
    inner: R,
}

impl<R: Read> PrependReader<R> {
    pub fn new(prefix: Vec<u8>, inner: R) -> Self {
        Self {
            prefix,
            pos: 0,
            inner,
        }
    }
}

impl<R: Read> Read for PrependReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos < self.prefix.len() {
            let n = buf.len().min(self.prefix.len() - self.pos);
            buf[..n].copy_from_slice(&self.prefix[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        self.inner.read(buf)
    }
}

/// Read the stream's first Ogg page and check whether it carries OpusHead.
/// Returns `(is_opus, bytes_read, advanced_reader)`: the caller feeds the
/// bytes back through a [`PrependReader`] — the symphony path restores the
/// exact stream, the Opus path skips ahead.
pub(crate) fn sniff_stream<R: Read>(mut reader: R) -> Result<(bool, Vec<u8>, R)> {
    let mut probe = Vec::new();
    let mut buf = [0u8; 27];
    let mut got = 0;
    while got < 27 {
        match reader.read(&mut buf[got..]) {
            Ok(0) => {
                // Truncated at the very start: not Opus, and there is not
                // enough left for symphonia either — return what we have.
                probe.extend_from_slice(&buf[..got]);
                return Ok((false, probe, reader));
            }
            Ok(n) => got += n,
            Err(e) => return Err(e.into()),
        }
    }
    if &buf[0..4] != b"OggS" || buf[4] != 0 {
        probe.extend_from_slice(&buf);
        return Ok((false, probe, reader));
    }
    probe.extend_from_slice(&buf);

    let nsegs = buf[26] as usize;
    let mut laces = vec![0u8; nsegs];
    match read_exact_into(&mut reader, &mut laces) {
        Ok(()) => {}
        Err(_) => {
            probe.extend_from_slice(&laces);
            return Ok((false, probe, reader));
        }
    }
    // The first packet of an Opus stream is the 19-byte OpusHead (laces
    // precede it in the page; the lacing limit bounds the read below).
    let first_packet = laces.first().copied().unwrap_or(0) as usize;
    let want = first_packet.min(64);
    let mut body = vec![0u8; want];
    let read = match read_upto(&mut reader, &mut body) {
        Ok(n) => n,
        Err(_) => {
            probe.extend_from_slice(&laces);
            probe.extend_from_slice(&body);
            return Ok((false, probe, reader));
        }
    };
    probe.extend_from_slice(&laces);
    probe.extend_from_slice(&body[..read]);

    let is_opus = first_packet >= 19 && body[..read].starts_with(b"OpusHead");
    Ok((is_opus, probe, reader))
}

fn read_exact_into<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<()> {
    let mut got = 0;
    while got < buf.len() {
        match reader.read(&mut buf[got..]) {
            Ok(0) => return Err("unexpected end of stream".into()),
            Ok(n) => got += n,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn read_upto<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match reader.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(got)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::encoder::{Encoder as _, OpusEncoder};
    use crate::output::ogg_mux::{opus_head_packet, opus_tags_packet};
    use std::io::Cursor;

    fn encode_tone(seconds: f64, rate: u32, channels: u16) -> Vec<u8> {
        let mut enc = OpusEncoder::new(rate, channels, 128_000, "test").unwrap();
        let frames = (seconds * rate as f64) as usize;
        let mut out = Vec::new();
        let mut pcm = Vec::with_capacity(1024);
        for f in 0..frames {
            let v =
                (f as f64 * 2.0 * std::f64::consts::PI * 440.0 / rate as f64).sin() as f32 * 0.5;
            pcm.push(v);
            pcm.push(v);
            if pcm.len() >= 1024 {
                out.extend_from_slice(&enc.encode(&pcm));
                pcm.clear();
            }
        }
        if !pcm.is_empty() {
            out.extend_from_slice(&enc.encode(&pcm));
        }
        out.extend_from_slice(&enc.finish());
        out
    }

    fn open_cursor(
        bytes: Vec<u8>,
        target: SignalSpec,
    ) -> OpusSource<Box<std::io::Cursor<Vec<u8>>>> {
        OpusSource::open(
            Box::new(Cursor::new(bytes)),
            target,
            4096,
            "test.opus".into(),
        )
        .expect("open opus source")
    }

    fn spec_stereo(rate: u32) -> SignalSpec {
        SignalSpec::new(rate, Channels::FRONT_LEFT | Channels::FRONT_RIGHT)
    }

    #[test]
    fn round_trip_decodes_a_tone() {
        let bytes = encode_tone(1.0, 44_100, 2);
        let mut src = open_cursor(bytes, spec_stereo(44_100));
        let mut buf = vec![0f32; 4096 * 2];
        let mut total = 0usize;
        let mut energy = 0f64;
        loop {
            let n = src.next_buffer(&mut buf);
            if n == 0 {
                break;
            }
            total += n;
            for &s in &buf[..n] {
                energy += (s as f64) * (s as f64);
            }
            assert!(n.is_multiple_of(2));
        }
        // ~1 s of stereo at 44.1 kHz (resampler latency excluded).
        assert!(total > 80_000 && total < 100_000, "total={total}");
        assert!(energy / total as f64 > 0.01, "energy={energy}");
        assert!(src.is_exhausted());
    }

    #[test]
    fn demux_reassembles_a_packet_split_across_pages() {
        // A 300-byte audio packet split 255 + 45 across two pages must come
        // back as one 300-byte packet. Headers ride on their own pages like
        // the encoder writes them.
        let mut stream = Vec::new();
        let mut seq = 0u32;
        let mut page =
            |stream: &mut Vec<u8>, flags: u8, granule: i64, laces: &[u8], body: &[u8]| {
                let mut hdr = Vec::new();
                hdr.extend_from_slice(b"OggS");
                hdr.push(0);
                hdr.push(flags);
                hdr.extend_from_slice(&granule.to_le_bytes());
                hdr.extend_from_slice(&1u32.to_le_bytes());
                hdr.extend_from_slice(&seq.to_le_bytes());
                hdr.extend_from_slice(&[0, 0, 0, 0]);
                hdr.push(laces.len() as u8);
                hdr.extend_from_slice(laces);
                let crc = crc32(&hdr, body);
                hdr[22..26].copy_from_slice(&crc.to_le_bytes());
                stream.extend_from_slice(&hdr);
                stream.extend_from_slice(body);
                seq += 1;
            };
        // Page 1: OpusHead (BOS), page 2: OpusTags — the conventional
        // encoder layout (each header on its own page). The 306-byte audio
        // packet is split 255 + 51 across pages 3 and 4: page 3's last lace
        // is 255 (unterminated) and page 4 carries the continuation flag.
        page(&mut stream, 0x02, 0, &[19], &opus_head_packet(2));
        let tags = opus_tags_packet("split");
        page(&mut stream, 0, 0, &[tags.len() as u8], &tags);
        let audio = vec![0xab; 306];
        page(&mut stream, 0x01, 960, &[255], &audio[..255]);
        page(&mut stream, 0x01 | 0x04, 1920, &[51], &audio[255..]);

        let mut demux = OggOpusDemux::new(Cursor::new(stream));
        let pkt = demux.next_packet().unwrap().expect("audio packet");
        assert_eq!(pkt.len(), 306);
        assert!(pkt.iter().all(|&b| b == 0xab));
        // Two packets were already encoded upstream (960 + 960 per page
        // granule), so the next packet is the end of the stream.
        assert!(demux.next_packet().unwrap().is_none());
        assert_eq!(demux.channels(), 2);
        assert_eq!(demux.total_granule(), Some(1920));
    }

    #[test]
    fn sniff_opus_detects_opus_streams() {
        let bytes = encode_tone(0.2, 44_100, 2);
        let (is_opus, prepend, mut rest) = sniff_stream(Cursor::new(bytes.clone())).unwrap();
        assert!(is_opus);
        assert!(prepend.starts_with(b"OggS"));
        // Everything after the prepend plus the prepend itself == the stream.
        let mut rebuilt = prepend.clone();
        let mut rest_bytes = Vec::new();
        rest.read_to_end(&mut rest_bytes).unwrap();
        rebuilt.extend_from_slice(&rest_bytes);
        assert_eq!(rebuilt, bytes);
    }

    #[test]
    fn demux_keeps_every_packet_on_multi_packet_pages() {
        // Real-world Ogg files pack many packets per page (ffmpeg: ~50). The
        // demux used to keep only the last completed packet of a page and
        // silently dropped the rest — an 8 s ffmpeg file decoded to ~0.17 s.
        let mut stream = Vec::new();
        let mut seq = 0u32;
        let mut page =
            |stream: &mut Vec<u8>, flags: u8, granule: i64, laces: &[u8], body: &[u8]| {
                let mut hdr = Vec::new();
                hdr.extend_from_slice(b"OggS");
                hdr.push(0);
                hdr.push(flags);
                hdr.extend_from_slice(&granule.to_le_bytes());
                hdr.extend_from_slice(&1u32.to_le_bytes());
                hdr.extend_from_slice(&seq.to_le_bytes());
                hdr.extend_from_slice(&[0, 0, 0, 0]);
                hdr.push(laces.len() as u8);
                hdr.extend_from_slice(laces);
                let crc = crc32(&hdr, body);
                hdr[22..26].copy_from_slice(&crc.to_le_bytes());
                stream.extend_from_slice(&hdr);
                stream.extend_from_slice(body);
                seq += 1;
            };
        page(&mut stream, 0x02, 0, &[19], &opus_head_packet(2));
        let tags = opus_tags_packet("multi");
        page(&mut stream, 0, 0, &[tags.len() as u8], &tags);
        // One page carrying three complete packets (three laces, none 255).
        page(
            &mut stream,
            0x04,
            2880,
            &[10, 20, 30],
            &[vec![0xaa; 10], vec![0xbb; 20], vec![0xcc; 30]].concat(),
        );

        let mut demux = OggOpusDemux::new(Cursor::new(stream));
        let p1 = demux.next_packet().unwrap().expect("packet 1");
        let p2 = demux.next_packet().unwrap().expect("packet 2");
        let p3 = demux.next_packet().unwrap().expect("packet 3");
        assert_eq!(p1, vec![0xaa; 10]);
        assert_eq!(p2, vec![0xbb; 20]);
        assert_eq!(p3, vec![0xcc; 30]);
        assert!(demux.next_packet().unwrap().is_none());
    }

    #[test]
    fn sniff_opus_rejects_non_ogg() {
        let not_ogg = b"ID3\x03\x00garbage".to_vec();
        let (is_opus, prepend, mut rest) = sniff_stream(Cursor::new(not_ogg.clone())).unwrap();
        assert!(!is_opus);
        // The sniffed prefix is preserved so symphonia sees the exact stream.
        let mut rebuilt = prepend.clone();
        let mut rest_bytes = Vec::new();
        rest.read_to_end(&mut rest_bytes).unwrap();
        rebuilt.extend_from_slice(&rest_bytes);
        assert_eq!(rebuilt, not_ogg);
    }
}
