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
}
