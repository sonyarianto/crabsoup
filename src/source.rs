pub mod file;
pub mod opus;
pub mod playlist;
pub mod replaygain;
pub mod request;

use symphonia::core::audio::SignalSpec;

use crate::resample::SincResampler;

/// A uniform pull-based interface for any audio input.
///
/// Implementations fill the provided buffer with interleaved `f32` samples in
/// the target `SignalSpec` and report how many frames were written.
pub trait AudioSource: Send {
    /// Fills the provided buffer with interleaved f32 PCM samples.
    /// Returns the number of frames actually read.
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize;

    /// Returns true if the source is exhausted or disconnected.
    fn is_exhausted(&self) -> bool;

    /// Seconds of audio still remaining, if known. Used to schedule
    /// crossfades *before* the end of a track.
    fn remaining_seconds(&self) -> Option<f64> {
        None
    }

    /// Human-readable label (e.g. current track title) for Icecast metadata.
    fn label(&self) -> Option<String> {
        None
    }

    /// Per-track ReplayGain baseline in dB (from `REPLAYGAIN_TRACK_GAIN` /
    /// `REPLAYGAIN_ALBUM_GAIN` tags), if the source can determine one.
    /// `None` = no tags (0 dB applied). Read once per track, at
    /// construction.
    fn replaygain_db(&self) -> Option<f32> {
        None
    }

    /// Advance to the next item immediately, where meaningful (telnet
    /// `skip`). Sources without a notion of "next" ignore it.
    fn skip(&mut self) {}
}

/// Supplies the *next* source on demand so a crossfade can be preloaded.
pub trait SourceProvider: Send {
    /// Returns the next source to play together with a display label.
    fn next_source(&mut self) -> (Box<dyn AudioSource>, String);

    /// Whether another source is available.
    fn has_next(&self) -> bool;
}

/// A silent source, used as a safe fallback when a file cannot be opened.
pub struct SilenceSource {
    exhausted: bool,
}

impl SilenceSource {
    pub fn new() -> Self {
        Self { exhausted: false }
    }
}

impl AudioSource for SilenceSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        let frames = buffer.len();
        buffer.fill(0.0);
        if self.exhausted {
            0
        } else {
            self.exhausted = true;
            frames
        }
    }

    fn is_exhausted(&self) -> bool {
        self.exhausted
    }
}

impl Default for SilenceSource {
    fn default() -> Self {
        Self::new()
    }
}

/// A silence test source (Liquidsoap `blank`). With a duration it exhausts
/// after that many seconds, letting `fallback`/`sequence` move on.
pub struct BlankSource {
    /// Samples remaining, `None` = infinite.
    samples_left: Option<usize>,
}

impl BlankSource {
    pub fn new() -> Self {
        Self { samples_left: None }
    }

    pub fn with_duration(seconds: f64, sample_rate: u32) -> Self {
        Self {
            samples_left: Some((seconds * sample_rate as f64) as usize),
        }
    }
}

impl Default for BlankSource {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioSource for BlankSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        let total = buffer.len();
        let n = self.samples_left.map(|l| l.min(total)).unwrap_or(total);
        buffer[..n].fill(0.0);
        if let Some(left) = self.samples_left.as_mut() {
            *left -= n;
        }
        n
    }

    fn is_exhausted(&self) -> bool {
        self.samples_left == Some(0)
    }

    fn label(&self) -> Option<String> {
        Some("blank".into())
    }
}

/// A sine test tone (Liquidsoap `sine`). With a duration it exhausts after
/// that many seconds.
pub struct SineSource {
    freq: f32,
    amplitude: f32,
    sample_rate: u32,
    channels: usize,
    phase: f64,
    frames_left: Option<usize>,
}

impl SineSource {
    pub fn new(
        freq: f32,
        duration: Option<f64>,
        amplitude: f32,
        sample_rate: u32,
        channels: usize,
    ) -> Self {
        Self {
            freq,
            amplitude,
            sample_rate,
            channels,
            phase: 0.0,
            frames_left: duration.map(|d| (d * sample_rate as f64) as usize),
        }
    }
}

impl AudioSource for SineSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        let frames = buffer.len() / self.channels;
        let take = self.frames_left.map(|l| l.min(frames)).unwrap_or(frames);
        if take == 0 {
            return 0;
        }
        let step = 2.0 * std::f64::consts::PI * self.freq as f64 / self.sample_rate as f64;
        for f in 0..take {
            let v = ((self.phase + step * f as f64).sin() * self.amplitude as f64) as f32;
            for ch in 0..self.channels {
                buffer[f * self.channels + ch] = v;
            }
        }
        if take < frames {
            buffer[take * self.channels..].fill(0.0);
        }
        self.phase = (self.phase + step * take as f64) % (2.0 * std::f64::consts::PI);
        if let Some(left) = self.frames_left.as_mut() {
            *left -= take;
        }
        take * self.channels
    }

    fn is_exhausted(&self) -> bool {
        self.frames_left == Some(0)
    }

    fn remaining_seconds(&self) -> Option<f64> {
        self.frames_left
            .map(|l| l as f64 / self.sample_rate as f64)
    }

    fn label(&self) -> Option<String> {
        Some(format!("sine {:.0} Hz", self.freq))
    }
}

/// Convert interleaved samples from `from` channels to `to` channels.
///
/// Mono -> stereo duplicates; stereo -> mono averages; anything with more
/// channels is folded to stereo using the front pair.
pub fn convert_channels(samples: &[f32], from: usize, to: usize) -> Vec<f32> {
    match (from, to) {
        (1, 1) => samples.to_vec(),
        (2, 2) => samples.to_vec(),
        (1, 2) => samples.iter().flat_map(|&s| [s, s]).collect(),
        (2, 1) => samples.chunks(2).map(|c| (c[0] + c[1]) * 0.5).collect(),
        (n, 2) if n > 2 => {
            let mut out = Vec::with_capacity(samples.len() / n * 2);
            for frame in samples.chunks(n) {
                out.extend_from_slice(&frame[0..2]);
            }
            out
        }
        _ => samples.to_vec(),
    }
}

/// A reusable resampler + channel converter that normalises arbitrary decoded
/// PCM into the target bus `SignalSpec`.
pub struct PcmConverter {
    target: SignalSpec,
    resampler: Option<SincResampler>,
    in_rate: u32,
}

impl PcmConverter {
    pub fn new(target: SignalSpec) -> Self {
        Self {
            target,
            resampler: None,
            in_rate: 0,
        }
    }

    pub fn target_channels(&self) -> usize {
        self.target.channels.count()
    }

    pub fn target_rate(&self) -> u32 {
        self.target.rate
    }

    /// Convert interleaved samples with the given native spec into the target spec.
    pub fn convert(&mut self, samples: &[f32], spec: &SignalSpec) -> Vec<f32> {
        let to_ch = self.target.channels.count();
        let converted = convert_channels(samples, spec.channels.count(), to_ch);
        if spec.rate == self.target.rate {
            return converted;
        }
        if self.resampler.is_none() || self.in_rate != spec.rate {
            self.resampler = Some(SincResampler::new(24, spec.rate, self.target.rate, to_ch));
            self.in_rate = spec.rate;
        }
        self.resampler.as_mut().unwrap().resample(&converted).to_vec()
    }

    /// Drain any samples remaining inside the resampler (call at EOF).
    /// The sinc filter emits every sample during `convert`, so this is a no-op.
    pub fn flush(&mut self) -> Vec<f32> {
        self.resampler
            .as_mut()
            .map(|r| r.flush().to_vec())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_to_stereo_duplicates() {
        let out = convert_channels(&[1.0, 2.0], 1, 2);
        assert_eq!(out, vec![1.0, 1.0, 2.0, 2.0]);
    }

    #[test]
    fn stereo_to_mono_averages() {
        let out = convert_channels(&[1.0, 3.0], 2, 1);
        assert_eq!(out, vec![2.0]);
    }

    #[test]
    fn blank_is_infinite_silence() {
        let mut b = BlankSource::new();
        let mut buf = [0.5f32; 10];
        let n = b.next_buffer(&mut buf);
        assert_eq!(n, 10);
        assert!(buf.iter().all(|&s| s == 0.0));
        assert!(!b.is_exhausted());
    }

    #[test]
    fn blank_with_duration_exhausts() {
        let mut b = BlankSource::with_duration(0.5, 100);
        let mut buf = vec![0f32; 30];
        assert_eq!(b.next_buffer(&mut buf), 30);
        assert_eq!(b.next_buffer(&mut buf), 20);
        assert!(b.is_exhausted());
        assert_eq!(b.next_buffer(&mut buf), 0);
    }

    #[test]
    fn sine_generates_a_tone_at_the_requested_frequency() {
        // 25 Hz at a 100 Hz rate: one cycle every 4 samples.
        let mut s = SineSource::new(25.0, None, 0.5, 100, 1);
        let mut buf = vec![0f32; 4];
        s.next_buffer(&mut buf);
        assert!((buf[0]).abs() < 1e-6);
        assert!((buf[1] - 0.5).abs() < 1e-6);
        assert!((buf[2]).abs() < 1e-6);
        assert!((buf[3] + 0.5).abs() < 1e-6);
    }

    #[test]
    fn sine_stereo_duplicates_the_tone_across_channels() {
        let mut s = SineSource::new(25.0, None, 0.5, 100, 2);
        let mut buf = vec![0f32; 8];
        s.next_buffer(&mut buf);
        for f in 0..4 {
            assert_eq!(buf[f * 2], buf[f * 2 + 1]);
        }
    }

    #[test]
    fn sine_with_duration_exhausts() {
        let mut s = SineSource::new(50.0, Some(0.1), 0.5, 100, 2);
        let mut buf = vec![0f32; 10];
        assert_eq!(s.next_buffer(&mut buf), 10);
        assert!(!s.is_exhausted());
        assert_eq!(s.next_buffer(&mut buf), 10);
        assert!(s.is_exhausted());
        assert_eq!(s.remaining_seconds(), Some(0.0));
    }
}
