pub mod file;
pub mod playlist;

use symphonia::core::audio::SignalSpec;

use crate::resample::LinearResampler;

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
    resampler: Option<LinearResampler>,
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
            self.resampler = Some(LinearResampler::new(24, spec.rate, self.target.rate, to_ch));
            self.in_rate = spec.rate;
        }
        self.resampler.as_mut().unwrap().resample(&converted).to_vec()
    }

    /// Drain any samples remaining inside the resampler (call at EOF).
    /// Linear interpolation has no tail, so this is a no-op.
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
}
