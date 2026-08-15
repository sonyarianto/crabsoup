//! Time-stretch / pitch-shift effects (Part I1).
//!
//! `stretch` changes tempo with pitch preserved (WSOLA time-stretching);
//! `pitch` shifts pitch with the tempo preserved, using the SoundTouch
//! composition: WSOLA-stretch by the pitch factor, then resample by the
//! inverse factor to restore the duration. Both are in-process pure-Rust
//! DSP (the `wsola` crate) wrapping a child source inside the pull chain,
//! so core builds stay free of system dependencies.

use wsola::TimeStretch;

use crate::resample::SincResampler;
use crate::source::AudioSource;

/// The pitch/tempo operation a [`PitchSource`] applies.
#[derive(Clone, Copy, Debug)]
pub enum PitchMode {
    /// Tempo multiplier: `1.0` unchanged, `1.5` is 50% faster (shorter).
    /// Pitch is preserved.
    Tempo(f32),
    /// Pitch shift in semitones (fractional); duration is preserved.
    Semitones(f32),
}

/// Wraps `child` in the pull chain with a WSOLA time-stretcher and, for the
/// semitone mode, a resampler that restores the duration after the stretch.
/// Output is buffered (`pending`) because a stretch produces a different
/// number of samples than it consumes.
pub struct PitchSource {
    child: Box<dyn AudioSource>,
    stretch: TimeStretch,
    channels: usize,
    /// Present only in semitone mode: resamples the stretched signal back to
    /// the original duration (`step = 1 / pitch_factor`).
    resampler: Option<SincResampler>,
    /// Interleaved output ready for the caller.
    pending: Vec<f32>,
    /// Scratch combining carried input with the child's latest pull, so the
    /// hot path allocates nothing per buffer.
    feedbuf: Vec<f32>,
    /// Trailing samples of a non-frame-aligned child pull, prepended to the
    /// next feed.
    carry: Vec<f32>,
    /// Child output buffer (one full interleaved engine buffer).
    inbuf: Vec<f32>,
    eof: bool,
    stretched_eof: bool,
    resample_eof: bool,
}

impl PitchSource {
    pub fn new(
        child: Box<dyn AudioSource>,
        mode: PitchMode,
        sample_rate: u32,
        channels: usize,
    ) -> crate::Result<Self> {
        let mut stretch =
            TimeStretch::new(sample_rate, channels as u16).map_err(|e| format!("wsola: {e}"))?;
        let resampler = match mode {
            PitchMode::Tempo(tempo) => {
                stretch.set_tempo(tempo);
                None
            }
            PitchMode::Semitones(semitones) => {
                let factor = 2f32.powf(semitones / 12.0);
                // Stretch by the inverse factor (slow to raise pitch, speed
                // up to lower it), then resample by the factor to restore the
                // original duration.
                stretch.set_tempo(1.0 / factor);
                // Read back the clamped tempo so the resample leg undoes
                // exactly what the stretch did: step = 1/tempo = pitch factor.
                let tempo = stretch.tempo();
                let mut rs = SincResampler::new(0, sample_rate, sample_rate, channels);
                rs.set_step(1.0 / tempo as f64);
                Some(rs)
            }
        };
        Ok(Self {
            child,
            stretch,
            channels,
            resampler,
            pending: Vec::new(),
            feedbuf: Vec::new(),
            carry: Vec::new(),
            inbuf: vec![0.0; 8192],
            eof: false,
            stretched_eof: false,
            resample_eof: false,
        })
    }

    fn drain_pending(&mut self, out: &mut [f32]) -> usize {
        let frames = (out.len() / self.channels).min(self.pending.len() / self.channels);
        let n = frames * self.channels;
        if n > 0 {
            out[..n].copy_from_slice(&self.pending[..n]);
            self.pending.drain(..n);
        }
        n
    }

    /// Combine the carry with the child's latest pull, push a frame-aligned
    /// prefix into the stretcher, and collect whatever output it yields.
    fn feed(&mut self, n: usize) {
        let total = self.carry.len() + n;
        self.feedbuf.clear();
        self.feedbuf.extend_from_slice(&self.carry);
        self.feedbuf.extend_from_slice(&self.inbuf[..n]);
        self.carry.clear();
        let usable = total / self.channels * self.channels;
        self.stretch.push(&self.feedbuf[..usable]);
        self.carry.extend_from_slice(&self.feedbuf[usable..total]);
        let out = self.stretch.pull(4096 * self.channels);
        if out.is_empty() {
            return;
        }
        if let Some(rs) = &mut self.resampler {
            let resampled = rs.resample(&out).to_vec();
            self.pending.extend_from_slice(&resampled);
        } else if self.pending.is_empty() {
            self.pending = out;
        } else {
            self.pending.extend_from_slice(&out);
        }
    }

    /// Feed a stretched tail slice through the resample leg (or straight
    /// into `pending` in tempo mode).
    fn push_output(&mut self, data: &[f32]) {
        if data.is_empty() {
            return;
        }
        if let Some(rs) = &mut self.resampler {
            let resampled = rs.resample(data).to_vec();
            self.pending.extend_from_slice(&resampled);
        } else {
            self.pending.extend_from_slice(data);
        }
    }
}

impl AudioSource for PitchSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        let mut written = 0;
        while written < buffer.len() {
            written += self.drain_pending(&mut buffer[written..]);
            if written == buffer.len() || self.eof {
                break;
            }
            let child_done = self.child.is_exhausted();
            let got = if child_done {
                0
            } else {
                self.child.next_buffer(&mut self.inbuf)
            };
            if got > 0 {
                self.feed(got);
            } else if child_done {
                if !self.stretched_eof {
                    self.stretched_eof = true;
                    let tail = self.stretch.flush();
                    self.push_output(&tail);
                } else if self.resampler.is_some() && !self.resample_eof {
                    self.resample_eof = true;
                    let tail = self.resampler.as_mut().unwrap().flush().to_vec();
                    self.push_output(&tail);
                } else {
                    self.eof = true;
                }
            } else {
                // A child that returns nothing without being exhausted would
                // spin the loop; stop rather than hang the output thread.
                self.eof = true;
                break;
            }
        }
        written
    }

    fn is_exhausted(&self) -> bool {
        self.eof && self.pending.is_empty()
    }

    fn remaining_seconds(&self) -> Option<f64> {
        // Tempo scales the child's remaining duration on the output clock.
        self.child
            .remaining_seconds()
            .map(|r| r / self.stretch.tempo() as f64)
    }

    fn label(&self) -> Option<String> {
        self.child.label()
    }

    fn replaygain_db(&self) -> Option<f32> {
        self.child.replaygain_db()
    }

    fn crossfade_overrides(&self) -> Option<(Option<f64>, Option<f64>)> {
        self.child.crossfade_overrides()
    }

    fn skip(&mut self) {
        self.child.skip();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SineSource;

    fn stretch_one_buffer(
        mode: PitchMode,
        freq: f32,
        rate: u32,
        seconds: f64,
        frames: usize,
    ) -> (Vec<f32>, usize) {
        let child: Box<dyn AudioSource> =
            Box::new(SineSource::new(freq, Some(seconds), 0.5, rate, 1));
        let mut src = PitchSource::new(child, mode, rate, 1).unwrap();
        let mut out = Vec::new();
        let mut buf = vec![0f32; frames];
        loop {
            let n = src.next_buffer(&mut buf);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            if src.is_exhausted() {
                break;
            }
        }
        let len = out.len();
        (out, len)
    }

    #[test]
    fn tempo_one_is_approximately_passthrough() {
        let (out, _) = stretch_one_buffer(PitchMode::Tempo(1.0), 440.0, 44_100, 1.0, 4096);
        // 1 s in, ~1 s out; RMS close to the 0.5 amplitude source.
        assert!(out.len() > 30_000, "output too short: {}", out.len());
        let rms = (out.iter().map(|&s| s * s).sum::<f32>() / out.len() as f32).sqrt();
        assert!((rms - 0.35).abs() < 0.15, "rms {rms}");
    }

    #[test]
    fn tempo_speeds_up_without_changing_pitch() {
        let rate = 44_100;
        let (out, _) = stretch_one_buffer(PitchMode::Tempo(1.5), 440.0, rate, 1.0, 4096);
        // 1.5x faster: ~0.67 s of output for 1 s of input.
        let secs = out.len() as f64 / rate as f64;
        assert!(secs > 0.6 && secs < 0.75, "duration {secs}");
        // Pitch preserved: ~440 zero-crossings per second.
        let crossings = out
            .windows(2)
            .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
            .count() as f64;
        let hz = crossings / 2.0 / secs;
        assert!((hz - 440.0).abs() < 15.0, "measured {hz} Hz");
    }

    #[test]
    fn tempo_slows_down_and_pitch_stays() {
        let rate = 44_100;
        let (out, _) = stretch_one_buffer(PitchMode::Tempo(0.5), 440.0, rate, 1.0, 4096);
        let secs = out.len() as f64 / rate as f64;
        assert!((secs - 2.0).abs() < 0.15, "duration {secs}");
    }

    #[test]
    fn pitch_shifts_frequency_without_changing_duration() {
        let rate = 44_100;
        // +12 semitones = exactly one octave up, same duration.
        let (out, _) = stretch_one_buffer(PitchMode::Semitones(12.0), 440.0, rate, 1.0, 4096);
        let secs = out.len() as f64 / rate as f64;
        assert!((secs - 1.0).abs() < 0.15, "duration {secs}");
        let crossings = out
            .windows(2)
            .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
            .count() as f64;
        let hz = crossings / 2.0 / secs;
        assert!((hz - 880.0).abs() < 30.0, "measured {hz} Hz");
    }

    #[test]
    fn pitch_shifts_down_an_octave() {
        let rate = 44_100;
        let (out, _) = stretch_one_buffer(PitchMode::Semitones(-12.0), 440.0, rate, 1.0, 4096);
        let secs = out.len() as f64 / rate as f64;
        assert!((secs - 1.0).abs() < 0.15, "duration {secs}");
        let crossings = out
            .windows(2)
            .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
            .count() as f64;
        let hz = crossings / 2.0 / secs;
        assert!((hz - 220.0).abs() < 15.0, "measured {hz} Hz");
    }

    #[test]
    fn finite_source_exhausts_cleanly() {
        let child: Box<dyn AudioSource> =
            Box::new(SineSource::new(440.0, Some(0.5), 0.5, 44_100, 1));
        let mut src = PitchSource::new(child, PitchMode::Tempo(1.0), 44_100, 1).unwrap();
        let mut buf = vec![0f32; 4096];
        let mut total = 0;
        loop {
            let n = src.next_buffer(&mut buf);
            if n == 0 {
                break;
            }
            total += n;
        }
        assert!(src.is_exhausted());
        // ~0.5 s of output (plus the stretcher's windowed tail).
        assert!(
            (total as f64 / 44_100.0 - 0.5).abs() < 0.15,
            "total {total}"
        );
    }

    #[test]
    fn remaining_seconds_scales_with_tempo() {
        let child: Box<dyn AudioSource> =
            Box::new(SineSource::new(440.0, Some(4.0), 0.5, 44_100, 1));
        let src = PitchSource::new(child, PitchMode::Tempo(2.0), 44_100, 1).unwrap();
        let r = src.remaining_seconds().unwrap();
        assert!((r - 2.0).abs() < 1e-3, "remaining {r}");
    }
}
