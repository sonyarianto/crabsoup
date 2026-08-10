use std::fs::File;
use std::path::Path;

use log::warn;
use symphonia::core::audio::SignalSpec;
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::source::{AudioSource, PcmConverter};
use crate::Result;

/// A single decoded audio file, normalised to the bus `SignalSpec`.
pub struct FileSource {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    converter: PcmConverter,
    /// Accumulated, converted samples (target spec, interleaved).
    buf: Vec<f32>,
    pos: usize,
    frames_per_buffer: usize,
    /// Total track duration in seconds (target-clock), if known.
    total_seconds: Option<f64>,
    elapsed_seconds: f64,
    eof: bool,
    label: String,
    /// ReplayGain track/album gain in dB from the file's tags, if present.
    replaygain_db: Option<f32>,
}

impl FileSource {
    pub fn open(path: &Path, target: SignalSpec, frames_per_buffer: usize) -> Result<Self> {
        let file = File::open(path)?;
        let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
        let mut probed = symphonia::default::get_probe().format(
            &Hint::new(),
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )?;

        let track = probed
            .format
            .default_track()
            .cloned()
            .ok_or("no default audio track")?;
        let track_id = track.id;
        let decoder = symphonia::default::get_codecs().make(
            &track.codec_params,
            &DecoderOptions::default(),
        )?;

        // Total duration, computed on the source's own clock.
        let total_seconds = track
            .codec_params
            .n_frames
            .filter(|n| *n > 0)
            .map(|n| n as f64 / track.codec_params.sample_rate.unwrap_or(target.rate) as f64);

        let label = title_of(probed.format.as_mut(), path);
        let replaygain_db = id3_replaygain(path).or_else(|| replaygain_of(probed.format.as_mut()));
        let converter = PcmConverter::new(target);

        Ok(Self {
            format: probed.format,
            decoder,
            track_id,
            converter,
            buf: Vec::new(),
            pos: 0,
            frames_per_buffer,
            total_seconds,
            elapsed_seconds: 0.0,
            eof: false,
            label,
            replaygain_db,
        })
    }

    /// Decode forward until at least `needed` frames are buffered (or EOF).
    fn fill(&mut self, needed: usize) {
        use symphonia::core::audio::SampleBuffer;
        use symphonia::core::errors::Error as SErr;

let to_ch = self.converter.target_channels();
        while self.buf.len() - self.pos < needed && !self.eof {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SErr::IoError(_)) => {
                    self.eof = true;
                    break;
                }
                Err(e) => {
                    warn!("skip packet: {e}");
                    continue;
                }
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(d) => d,
                Err(e) => {
                    warn!("decode error (skipping packet): {e}");
                    continue;
                }
            };

            let spec = *decoded.spec();
            let frames = decoded.frames();
            if frames == 0 {
                continue;
            }

            let mut sample_buf = SampleBuffer::<f32>::new(frames as u64, spec);
            sample_buf.copy_interleaved_ref(decoded);
            let converted = self.converter.convert(sample_buf.samples(), &spec);

            let rate = self.target_rate();
            self.elapsed_seconds += converted.len() as f64 / rate as f64 / to_ch as f64;
            self.buf.extend_from_slice(&converted);
        }
    }

    fn target_rate(&self) -> u32 {
        self.converter.target_rate()
    }
}

impl AudioSource for FileSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        let chans = self.converter.target_channels();
        let want = self.frames_per_buffer * chans;
        self.fill(buffer.len().max(want));

        let available = self.buf.len() - self.pos;
        let n = available.min(buffer.len());
        buffer[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        if n == 0 {
            log::debug!(
                "filesource: next_buffer returned 0 (eof={}, buf={}, pos={})",
                self.eof,
                self.buf.len(),
                self.pos
            );
        }

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
        let total = self.total_seconds?;
        Some((total - self.elapsed_seconds).max(0.0))
    }

    fn label(&self) -> Option<String> {
        Some(self.label.clone())
    }

    fn replaygain_db(&self) -> Option<f32> {
        self.replaygain_db
    }
}

/// Parse `REPLAYGAIN_TRACK_GAIN`, falling back to `REPLAYGAIN_ALBUM_GAIN`.
/// Tag values look like `"-6.5 dB"`; tools also write the Unicode minus
/// (U+2212). Returns `None` when absent or unparseable. symphonia's Ogg
/// readers surface Vorbis comments here; MP3 files have no ID3 reader in
/// symphonia 0.5, so those are handled by [`id3_replaygain`].
fn replaygain_of(format: &mut dyn FormatReader) -> Option<f32> {
    let metadata = format.metadata();
    let revision = metadata.current()?;
    let mut found: Option<f32> = None;
    for tag in revision.tags() {
        let key = tag.key.to_ascii_uppercase();
        let db = match key.as_str() {
            "REPLAYGAIN_TRACK_GAIN" => Some(parse_replaygain(tag.value.to_string())),
            "REPLAYGAIN_ALBUM_GAIN" => {
                if found.is_none() {
                    Some(parse_replaygain(tag.value.to_string()))
                } else {
                    None
                }
            }
            _ => None,
        };
        if let Some(Some(v)) = db {
            found = Some(v);
        }
    }
    found
}

/// Read the file's own ID3v2 tag (any version) and look for a `TXXX`
/// `REPLAYGAIN_TRACK_GAIN`/`REPLAYGAIN_ALBUM_GAIN` frame. symphonia 0.5
/// has no ID3 reader for MP3, so this is the MP3 path.
fn id3_replaygain(path: &Path) -> Option<f32> {
    use std::io::Read;

    let mut file = File::open(path).ok()?;
    let mut header = [0u8; 10];
    file.read_exact(&mut header).ok()?;
    if &header[..3] != b"ID3" {
        return None;
    }
    let version_major = header[3];
    // ID3v2 sizes are syncsafe (7 bits per byte), including frame sizes in
    // v2.4 but not in v2.3.
    let size = ((header[6] as usize) << 21)
        | ((header[7] as usize) << 14)
        | ((header[8] as usize) << 7)
        | (header[9] as usize);
    if size == 0 || size > 64 * 1024 * 1024 {
        return None;
    }
    let mut tag = vec![0u8; size];
    file.read_exact(&mut tag).ok()?;
    let syncsafe = version_major >= 4;

    let mut found: Option<f32> = None;
    let mut pos = 0usize;
    while pos + 10 <= tag.len() {
        let frame_id = &tag[pos..pos + 4];
        let frame_size = if syncsafe {
            ((tag[pos + 4] as usize) << 21)
                | ((tag[pos + 5] as usize) << 14)
                | ((tag[pos + 6] as usize) << 7)
                | (tag[pos + 7] as usize)
        } else {
            u32::from_be_bytes([tag[pos + 4], tag[pos + 5], tag[pos + 6], tag[pos + 7]]) as usize
        };
        if frame_size == 0 {
            break;
        }
        if pos + 10 + frame_size > tag.len() {
            break;
        }
        let payload = &tag[pos + 10..pos + 10 + frame_size];
        if frame_id == b"TXXX" {
            let text = decode_id3_text(payload);
            let (description, value) = text.split_once('\0').unwrap_or(("", &text[..]));
            let db = match description.trim().to_ascii_uppercase().as_str() {
                "REPLAYGAIN_TRACK_GAIN" => Some(parse_replaygain(value.trim().to_string())),
                "REPLAYGAIN_ALBUM_GAIN" => {
                    if found.is_none() {
                        Some(parse_replaygain(value.trim().to_string()))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some(Some(v)) = db {
                found = Some(v);
            }
        }
        pos += 10 + frame_size;
    }
    found
}

/// Decode an ID3v2 text payload (first byte = encoding) into a string.
/// UTF-16 descriptions of the ASCII keys we match are recovered by
/// dropping the interleaved `\0` bytes.
fn decode_id3_text(payload: &[u8]) -> String {
    if payload.is_empty() {
        return String::new();
    }
    let (encoding, bytes) = payload.split_at(1);
    match encoding[0] {
        1 | 2 => String::from_utf8_lossy(bytes).replace('\0', ""),
        3 => String::from_utf8_lossy(bytes).into_owned(),
        _ => {
            // Latin-1 (0) — pass through as UTF-8 lossily.
            String::from_utf8_lossy(bytes).into_owned()
        }
    }
}

fn parse_replaygain(value: String) -> Option<f32> {
    let normalized = value.replace('\u{2212}', "-");
    let digits: String = normalized
        .chars()
        .skip_while(|c| !c.is_ascii_digit() && *c != '-')
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    digits.parse::<f32>().ok()
}

/// Best-effort "Artist - Title" from embedded tags, falling back to the filename.
fn title_of(format: &mut dyn FormatReader, path: &Path) -> String {
    use symphonia::core::meta::StandardTagKey;

    let mut title = None;
    let mut artist = None;

    if let Some(revision) = format.metadata().current() {
        for tag in revision.tags() {
            match tag.std_key {
                Some(StandardTagKey::TrackTitle) => title = Some(tag.value.to_string()),
                Some(StandardTagKey::Artist) => artist = Some(tag.value.to_string()),
                _ => {}
            }
        }
    }

    match (artist, title) {
        (Some(a), Some(t)) if !a.is_empty() && !t.is_empty() => format!("{a} - {t}"),
        (_, Some(t)) if !t.is_empty() => t.to_string(),
        _ => path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_sine_wav(path: &Path, seconds: f64, rate: u32, freq: f64) {
        let n = (seconds * rate as f64) as usize;
        let mut data = Vec::with_capacity(n * 4);
        for i in 0..n {
            let t = i as f64 / rate as f64;
            let s = (2.0 * std::f64::consts::PI * freq * t).sin() as f32 * 0.5;
            let sample = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            for _ in 0..2 {
                data.extend_from_slice(&sample.to_le_bytes());
            }
        }

        // Minimal RIFF/WAVE writer (PCM 16-bit stereo).
        let mut f = File::create(path).unwrap();
        let data_len = data.len() as u32;
        f.write_all(b"RIFF").unwrap();
        f.write_all(&(36 + data_len).to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        f.write_all(&2u16.to_le_bytes()).unwrap(); // channels
        f.write_all(&rate.to_le_bytes()).unwrap();
        f.write_all(&(rate * 4).to_le_bytes()).unwrap(); // byte rate
        f.write_all(&4u16.to_le_bytes()).unwrap(); // block align
        f.write_all(&16u16.to_le_bytes()).unwrap(); // bits per sample
        f.write_all(b"data").unwrap();
        f.write_all(&data_len.to_le_bytes()).unwrap();
        f.write_all(&data).unwrap();
    }

    #[test]
    fn decodes_wav_to_frames() {
        let dir = std::env::temp_dir().join("crabsoup-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sine.wav");
        write_sine_wav(&path, 0.25, 44100, 440.0);

        let spec = symphonia::core::audio::SignalSpec::new(44100, symphonia::core::audio::Channels::FRONT_LEFT | symphonia::core::audio::Channels::FRONT_RIGHT);
        let mut src = FileSource::open(&path, spec, 4096).unwrap();

        let mut buf = vec![0f32; 44100 * 2];
        let n = src.next_buffer(&mut buf);
        // 0.25 s at 44100 Hz = 11025 frames, x2 channels = 22050 samples.
        assert_eq!(n, 22050);
        assert!(src.is_exhausted());
        // Samples are a sine wave in [-0.5, 0.5], not silence.
        assert!(buf.iter().any(|&s| s.abs() > 0.1));
        assert_eq!(src.label().as_deref(), Some("sine"));
    }

    /// Build a minimal ID3v2.3 tag containing one TXXX frame with the given
    /// description/value, and prepend it to `base` (a copy of a real audio
    /// file). Returns the path to the tagged copy.
    fn write_id3_txxx(base: &Path, description: &str, value: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join("crabsoup-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!(
            "rg-{}-{}.mp3",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let audio = std::fs::read(base).expect("base audio");

        let mut body = Vec::new();
        body.extend_from_slice(b"TXXX");
        let payload = [0u8]
            .iter()
            .chain(description.as_bytes())
            .chain([0u8].iter())
            .chain(value.as_bytes())
            .chain([0u8].iter())
            .copied()
            .collect::<Vec<_>>();
        body.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        body.extend_from_slice(&[0u8, 0u8]);
        body.extend_from_slice(&payload);

        let mut tag = Vec::new();
        tag.extend_from_slice(b"ID3");
        tag.extend_from_slice(&[3, 0, 0]);
        let size = body.len() as u32;
        let syncsafe = [
            ((size >> 21) & 0x7f) as u8,
            ((size >> 14) & 0x7f) as u8,
            ((size >> 7) & 0x7f) as u8,
            (size & 0x7f) as u8,
        ];
        tag.extend_from_slice(&syncsafe);
        tag.extend_from_slice(&body);

        let mut out = std::fs::File::create(&path).unwrap();
        out.write_all(&tag).unwrap();
        out.write_all(&audio).unwrap();
        path
    }

    #[test]
    fn parses_replaygain_from_id3_txxx_tags() {
        let real = Path::new("media/poshpony-shamanic-house-310684.mp3");
        if !real.exists() {
            return;
        }
        let tagged = write_id3_txxx(real, "REPLAYGAIN_TRACK_GAIN", "-6.5 dB");
        let spec = symphonia::core::audio::SignalSpec::new(44100, symphonia::core::audio::Channels::FRONT_LEFT | symphonia::core::audio::Channels::FRONT_RIGHT);
        let src = FileSource::open(&tagged, spec, 4096).unwrap();
        assert_eq!(src.replaygain_db(), Some(-6.5));
    }

    #[test]
    fn album_gain_is_the_fallback_and_untagged_files_report_none() {
        let real = Path::new("media/poshpony-shamanic-house-310684.mp3");
        if !real.exists() {
            return;
        }
        let tagged =
            write_id3_txxx(real, "REPLAYGAIN_ALBUM_GAIN", "\u{2212}4.25 dB");
        let spec = symphonia::core::audio::SignalSpec::new(44100, symphonia::core::audio::Channels::FRONT_LEFT | symphonia::core::audio::Channels::FRONT_RIGHT);
        let src = FileSource::open(&tagged, spec, 4096).unwrap();
        // Tools write the Unicode minus; parse must normalize it.
        assert_eq!(src.replaygain_db(), Some(-4.25));

        let dir = std::env::temp_dir().join("crabsoup-test");
        let path = dir.join("sine.wav");
        write_sine_wav(&path, 0.1, 44100, 440.0);
        let src = FileSource::open(&path, spec, 4096).unwrap();
        assert_eq!(src.replaygain_db(), None);
    }

    #[test]
    fn replaygain_tag_without_suffix_parses_too() {
        let real = Path::new("media/poshpony-shamanic-house-310684.mp3");
        if !real.exists() {
            return;
        }
        let tagged = write_id3_txxx(real, "REPLAYGAIN_TRACK_GAIN", "3.0");
        let spec = symphonia::core::audio::SignalSpec::new(44100, symphonia::core::audio::Channels::FRONT_LEFT | symphonia::core::audio::Channels::FRONT_RIGHT);
        let src = FileSource::open(&tagged, spec, 4096).unwrap();
        assert_eq!(src.replaygain_db(), Some(3.0));
    }
}

