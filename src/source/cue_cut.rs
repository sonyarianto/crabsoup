//! Per-track cue-point trimming (Liquidsoap `annotate:` / `cue_cut`).
//!
//! Wraps a child and cuts its pull window to the track's cue points:
//! `cue_in` seconds are skipped at the start of each track, and `cue_out`
//! is treated as early exhaustion so the parent (crossfade mixer or
//! fallback) advances at the cue point instead of the file's natural end.
//! The window is re-applied at every track boundary (the child's `label`
//! changing), so a `cue_cut` around a playlist trims every track.
//!
//! All positions are counted in integer samples (cue seconds rounded once
//! to samples per track) so the truncated window lands on the exact sample,
//! never off-by-one from float rounding.

use crate::request::TrackCues;
use crate::source::AudioSource;

pub struct CueCutSource {
    child: Box<dyn AudioSource>,
    cues: Option<TrackCues>,
    /// The child's label as of the last boundary check.
    last_label: Option<String>,
    /// Samples of `cue_in` still to skip in the current track.
    skip_left_samples: usize,
    /// Samples of audible audio emitted past `cue_in`.
    emitted_samples: usize,
    /// `cue_out` was reached: the window is over, report exhaustion.
    done: bool,
    sample_rate: u32,
    channels: usize,
}

impl CueCutSource {
    pub fn new(
        child: Box<dyn AudioSource>,
        cues: TrackCues,
        sample_rate: u32,
        channels: usize,
    ) -> Self {
        let mut this = Self {
            child,
            cues: Some(cues),
            last_label: None,
            skip_left_samples: 0,
            emitted_samples: 0,
            done: false,
            sample_rate,
            channels,
        };
        this.reset_track();
        this
    }

    /// Samples per second of the interleaved buffer.
    fn samples_per_sec(&self) -> usize {
        self.sample_rate as usize * self.channels
    }

    /// Length of the audible window in samples (`cue_out - cue_in`,
    /// rounded once), or 0 when there is no `cue_out`.
    fn window_samples(&self) -> usize {
        let Some(cues) = self.cues else {
            return 0;
        };
        let Some(out) = cues.cue_out else {
            return 0;
        };
        let spps = self.samples_per_sec() as f64;
        let start = (cues.cue_in.max(0.0) * spps).round() as usize;
        let end = (out.max(0.0) * spps).round() as usize;
        end.saturating_sub(start)
    }

    /// Re-arm the window for a new track (the child's label changed).
    fn reset_track(&mut self) {
        let spps = self.samples_per_sec() as f64;
        let skip = self.cues.map_or(0.0, |c| c.cue_in.max(0.0));
        self.skip_left_samples = (skip * spps).round() as usize;
        self.emitted_samples = 0;
        self.done = false;
    }

    /// Emit up to `n` samples, truncating at `cue_out`. Without a `cue_out`
    /// this is a passthrough.
    fn emit(&mut self, buffer: &mut [f32], n: usize) -> usize {
        let Some(cues) = self.cues else {
            return n;
        };
        if cues.cue_out.is_none() {
            return n;
        }
        let window = self.window_samples();
        let remaining = window.saturating_sub(self.emitted_samples);
        if n <= remaining {
            self.emitted_samples += n;
            return n;
        }
        // The window ends inside this buffer (or is empty: cue_out <=
        // cue_in, so the track emits nothing).
        let keep = remaining;
        self.emitted_samples += keep;
        self.done = true;
        if keep < buffer.len() {
            buffer[keep..].fill(0.0);
        }
        keep
    }
}

impl AudioSource for CueCutSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        // Track boundary: re-apply the window on the first pull of a new
        // track (the very first pull primes it too).
        let label = self.child.label();
        if label != self.last_label {
            self.last_label = label;
            self.reset_track();
        }
        if self.done {
            return 0;
        }
        loop {
            let n = self.child.next_buffer(buffer);
            if n == 0 {
                return 0;
            }
            // Skip-ahead to cue_in: discard whole buffers, then the partial
            // head of the buffer that crosses the boundary.
            if self.skip_left_samples > 0 {
                if n <= self.skip_left_samples {
                    self.skip_left_samples -= n;
                    continue;
                }
                let skip = self.skip_left_samples;
                let kept = n - skip;
                buffer.copy_within(skip..n, 0);
                self.skip_left_samples = 0;
                return self.emit(buffer, kept);
            }
            return self.emit(buffer, n);
        }
    }

    fn is_exhausted(&self) -> bool {
        self.done || self.child.is_exhausted()
    }

    fn remaining_seconds(&self) -> Option<f64> {
        if self.done {
            return Some(0.0);
        }
        if self.cues.is_some_and(|c| c.cue_out.is_some()) {
            let spps = self.samples_per_sec() as f64;
            let left = self.window_samples().saturating_sub(self.emitted_samples);
            return Some(left as f64 / spps);
        }
        // No cue_out: the child knows the true end (its elapsed already
        // counts whatever we pulled while skipping).
        self.child.remaining_seconds()
    }

    fn label(&self) -> Option<String> {
        self.child.label()
    }

    fn replaygain_db(&self) -> Option<f32> {
        self.child.replaygain_db()
    }

    fn crossfade_overrides(&self) -> Option<(Option<f64>, Option<f64>)> {
        let cues = self.cues?;
        if cues.fade_in.is_some() || cues.fade_out.is_some() {
            Some((cues.fade_in, cues.fade_out))
        } else {
            None
        }
    }

    fn skip(&mut self) {
        self.child.skip();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SineSource;

    const RATE: u32 = 100;
    const CHANS: usize = 2;

    fn cues(cue_in: f64, cue_out: Option<f64>) -> TrackCues {
        TrackCues {
            cue_in,
            cue_out,
            fade_in: None,
            fade_out: None,
            amplify: None,
        }
    }

    #[test]
    fn skips_cue_in_seconds_at_track_start() {
        // 25 Hz sine: at t=0.04s the first emitted sample must match the
        // sine's phase there, i.e. sin(2*pi*25*0.04) = sin(2pi) = 0.
        let child: Box<dyn AudioSource> = Box::new(SineSource::new(25.0, None, 1.0, RATE, CHANS));
        let mut src = CueCutSource::new(child, cues(0.04, None), RATE, CHANS);
        let mut buf = vec![0f32; 8];
        let n = src.next_buffer(&mut buf);
        assert_eq!(n, 8, "nothing but the head may be cut");
        assert!(buf[0].abs() < 1e-3, "expected sine phase 0, got {}", buf[0]);
    }

    #[test]
    fn cue_out_truncates_the_track() {
        // cue_in 0.02s (2 frames) + cue_out 0.04s -> window is 2 frames.
        let child: Box<dyn AudioSource> = Box::new(SineSource::new(25.0, None, 1.0, RATE, CHANS));
        let mut src = CueCutSource::new(child, cues(0.02, Some(0.04)), RATE, CHANS);
        let mut buf = vec![0f32; 40];
        let mut total = 0usize;
        let mut emitted = 0.0f64;
        while emitted < 0.1 {
            let n = src.next_buffer(&mut buf);
            total += n;
            emitted += n as f64 / (RATE as f64 * CHANS as f64);
            if n == 0 {
                break;
            }
        }
        // Window [0.02, 0.04) = 2 frames = 4 samples.
        assert_eq!(total, 4, "cue_out must end the track exactly at the window");
        assert!(src.is_exhausted());
        assert_eq!(src.remaining_seconds(), Some(0.0));
    }

    #[test]
    fn no_cue_points_is_a_passthrough() {
        let child: Box<dyn AudioSource> = Box::new(SineSource::new(25.0, None, 1.0, RATE, CHANS));
        let mut src = CueCutSource::new(child, TrackCues::default(), RATE, CHANS);
        let mut buf = vec![0f32; 8];
        let n = src.next_buffer(&mut buf);
        assert_eq!(n, 8);
        assert!(!src.is_exhausted());
    }

    #[test]
    fn reports_fade_overrides_from_the_cue_points() {
        let child: Box<dyn AudioSource> = Box::new(SineSource::new(25.0, None, 1.0, RATE, CHANS));
        let src = CueCutSource::new(
            child,
            TrackCues {
                cue_in: 0.0,
                cue_out: None,
                fade_in: Some(2.0),
                fade_out: Some(3.0),
                amplify: None,
            },
            RATE,
            CHANS,
        );
        assert_eq!(src.crossfade_overrides(), Some((Some(2.0), Some(3.0))));

        // Only cue points, no fades: no override reported.
        let child: Box<dyn AudioSource> = Box::new(SineSource::new(25.0, None, 1.0, RATE, CHANS));
        let src = CueCutSource::new(child, cues(0.02, Some(0.22)), RATE, CHANS);
        assert_eq!(src.crossfade_overrides(), None);
    }

    #[test]
    fn remaining_seconds_reports_the_cue_window() {
        let child: Box<dyn AudioSource> = Box::new(SineSource::new(25.0, None, 1.0, RATE, CHANS));
        let mut src = CueCutSource::new(child, cues(0.02, Some(0.22)), RATE, CHANS);
        // 0.2 s window (40 samples at 200/s) remains before any audio is
        // pulled.
        assert!((src.remaining_seconds().unwrap() - 0.2).abs() < 1e-9);
        let mut buf = vec![0f32; 20];
        src.next_buffer(&mut buf); // 0.02 s of the pull is the cue_in skip,
        // so 16 samples (0.08 s) of window audio
        // are consumed, leaving 24 (0.12 s).
        let left = src.remaining_seconds().unwrap();
        assert!((left - 0.12).abs() < 1e-9, "left {left}");
    }
}
