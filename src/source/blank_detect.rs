//! Dead-air detection (Liquidsoap `blank.detect`).
//!
//! Wraps a child and watches the RMS level of everything it produces. When
//! the level stays below `threshold_db` for `duration_secs`, the source
//! enters a blank state: it emits silence, fires an optional `on_blank`
//! callback once per episode, and (by default) reports `is_exhausted() ==
//! true` so a `fallback` composed around it hands over automatically — the
//! zero-configuration dead-air guard. After `restart_secs` the child is
//! re-checked; audio above the threshold brings it back.
//!
//! The envelope measurement is the same per-buffer RMS shape the DSP
//! effects use (Part F4 of the plan), kept deterministic for exact-sample
//! tests: no smoothing state across buffers, just a running sub-threshold
//! timer.

use crate::source::AudioSource;

/// Dead-air detection settings (`blank.detect(src, opts)`).
pub struct BlankDetectConfig {
    /// RMS level (dBFS) below which audio counts as silence.
    pub threshold_db: f32,
    /// Continuous silence (seconds) before the source goes blank.
    pub duration_secs: f32,
    /// Blank time (seconds) before the child is re-checked for recovery.
    pub restart_secs: f32,
    /// While blank, report `is_exhausted() == true` so a `fallback` hands
    /// over (default true). When false, the child's real state is reported
    /// and the blank just produces silence.
    pub exhaust_while_blank: bool,
    /// Called (on the audio thread, once per blank episode) when silence is
    /// detected. Typically forwards to the Lua-owning event loop.
    pub on_blank: Option<Box<dyn FnMut() + Send>>,
    pub sample_rate: u32,
    pub channels: usize,
}

/// A silence-watching wrapper around a child source.
pub struct BlankDetectSource {
    child: Box<dyn AudioSource>,
    threshold_db: f32,
    duration_secs: f32,
    restart_secs: f32,
    exhaust_while_blank: bool,
    on_blank: Option<Box<dyn FnMut() + Send>>,
    sample_rate: u32,
    channels: usize,
    /// Continuous sub-threshold time while measuring.
    blank_secs: f32,
    /// Blank time remaining before recovery is re-checked.
    restart_left: f32,
    /// Currently blank (silence, exhausted, fallback hands over).
    blank: bool,
    /// `on_blank` already fired for this episode.
    fired: bool,
    /// Reusable scratch for consuming the child while blank.
    scratch: Vec<f32>,
}

impl BlankDetectSource {
    pub fn new(child: Box<dyn AudioSource>, config: BlankDetectConfig) -> Self {
        Self {
            child,
            threshold_db: config.threshold_db,
            duration_secs: config.duration_secs,
            restart_secs: config.restart_secs,
            exhaust_while_blank: config.exhaust_while_blank,
            on_blank: config.on_blank,
            sample_rate: config.sample_rate,
            channels: config.channels,
            blank_secs: 0.0,
            restart_left: 0.0,
            blank: false,
            fired: false,
            scratch: Vec::new(),
        }
    }

    /// Seconds of audio a pull of `n` samples represents; a zero-length pull
    /// (temporarily silent child) still counts as one full buffer, or dead
    /// air on an empty child would never accumulate.
    fn buffer_seconds(&self, n: usize, buffer_len: usize) -> f32 {
        let per_second = (self.sample_rate as f32 * self.channels as f32).max(1.0);
        if n > 0 {
            n as f32 / per_second
        } else {
            buffer_len as f32 / per_second
        }
    }

    fn enter_blank(&mut self) {
        self.blank = true;
        self.restart_left = self.restart_secs;
        if !self.fired {
            if let Some(cb) = self.on_blank.as_mut() {
                cb();
            }
            self.fired = true;
        }
    }
}

/// Per-buffer RMS in dBFS (`-inf` for silence).
fn buffer_rms_db(buf: &[f32]) -> f32 {
    if buf.is_empty() {
        return f32::NEG_INFINITY;
    }
    let mut sum = 0.0f64;
    for &s in buf {
        sum += (s as f64) * (s as f64);
    }
    (20.0 * (sum / buf.len() as f64).sqrt().max(1e-9).log10()) as f32
}

impl AudioSource for BlankDetectSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        if !self.blank {
            let n = self.child.next_buffer(buffer);
            let secs = self.buffer_seconds(n, buffer.len());
            let db = if n > 0 {
                buffer_rms_db(&buffer[..n])
            } else {
                f32::NEG_INFINITY
            };
            if db < self.threshold_db {
                self.blank_secs += secs;
                if self.duration_secs > 0.0 && self.blank_secs >= self.duration_secs {
                    self.enter_blank();
                }
            } else {
                self.blank_secs = 0.0;
            }
            n
        } else {
            // Blank: keep pulling the child (monitoring for recovery, and
            // letting a request-style child advance) but emit silence.
            if self.scratch.len() != buffer.len() {
                self.scratch.resize(buffer.len(), 0.0);
            }
            let n = self.child.next_buffer(&mut self.scratch);
            if self.restart_left > 0.0 {
                self.restart_left -= self.buffer_seconds(n, buffer.len());
                return 0;
            }
            let db = if n > 0 {
                buffer_rms_db(&self.scratch[..n])
            } else {
                f32::NEG_INFINITY
            };
            if db >= self.threshold_db {
                // Recovered: hand the freshly pulled audio through.
                self.blank = false;
                self.blank_secs = 0.0;
                self.fired = false;
                buffer[..n].copy_from_slice(&self.scratch[..n]);
                return n;
            }
            0
        }
    }

    fn is_exhausted(&self) -> bool {
        if self.blank && self.exhaust_while_blank {
            true
        } else {
            self.child.is_exhausted()
        }
    }

    fn remaining_seconds(&self) -> Option<f64> {
        self.child.remaining_seconds()
    }

    fn label(&self) -> Option<String> {
        self.child.label()
    }

    fn replaygain_db(&self) -> Option<f32> {
        self.child.replaygain_db()
    }

    fn crossfade_overrides(&self) -> Option<crate::source::CrossfadeOverrides> {
        self.child.crossfade_overrides()
    }

    fn skip(&mut self) {
        self.child.skip();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Alternates `loud_bufs` buffers of 0.5 with `quiet_bufs` buffers of
    /// silence, forever, without ever exhausting.
    struct LoudQuiet {
        loud: bool,
        count: usize,
        loud_bufs: usize,
        quiet_bufs: usize,
    }

    impl AudioSource for LoudQuiet {
        fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
            buffer.fill(if self.loud { 0.5 } else { 0.0 });
            self.count += 1;
            let cycle = if self.loud {
                self.loud_bufs
            } else {
                self.quiet_bufs
            };
            if self.count >= cycle {
                self.loud = !self.loud;
                self.count = 0;
            }
            buffer.len()
        }
        fn is_exhausted(&self) -> bool {
            false
        }
    }

    fn detect(src: Box<dyn AudioSource>, duration: f32, restart: f32) -> BlankDetectSource {
        BlankDetectSource::new(
            src,
            BlankDetectConfig {
                threshold_db: -40.0,
                duration_secs: duration,
                restart_secs: restart,
                exhaust_while_blank: true,
                on_blank: None,
                sample_rate: 100,
                channels: 1,
            },
        )
    }

    #[test]
    fn loud_audio_never_triggers() {
        let mut src = detect(
            Box::new(LoudQuiet {
                loud: true,
                count: 0,
                loud_bufs: 10,
                quiet_bufs: 0,
            }),
            0.2,
            0.1,
        );
        let mut buf = vec![0f32; 10]; // 0.1 s per buffer at 100 Hz
        for _ in 0..5 {
            assert_eq!(src.next_buffer(&mut buf), 10);
        }
        assert!(!src.is_exhausted());
        assert!(
            buf.iter().all(|&s| (s - 0.5).abs() < 1e-6),
            "audio passes through"
        );
    }

    #[test]
    fn silence_beyond_the_duration_goes_blank_and_exhausts() {
        let mut src = detect(
            Box::new(LoudQuiet {
                loud: true,
                count: 0,
                loud_bufs: 3,  // 0.3 s loud
                quiet_bufs: 8, // 0.8 s quiet
            }),
            0.2,
            0.1,
        );
        let mut buf = vec![0f32; 10];
        // 3 loud + 2 quiet buffers: after the 5th pull the 0.2 s threshold
        // has elapsed, so the next pull must be blank + exhausted.
        for _ in 0..5 {
            src.next_buffer(&mut buf);
        }
        assert!(src.is_exhausted(), "blank source reports exhausted");
        assert_eq!(src.next_buffer(&mut buf), 0, "blank emits silence");
    }

    #[test]
    fn recovers_when_audio_returns_after_the_restart_window() {
        // 3 loud / 4 quiet buffers, repeating: detection at the 5th pull,
        // one restart buffer, then the cycle flips back to loud.
        let mut src = detect(
            Box::new(LoudQuiet {
                loud: true,
                count: 0,
                loud_bufs: 3,
                quiet_bufs: 4,
            }),
            0.2,
            0.1,
        );
        let mut buf = vec![0f32; 10];
        for _ in 0..5 {
            src.next_buffer(&mut buf);
        }
        // Blank: one restart buffer elapses, the child is still quiet.
        assert_eq!(src.next_buffer(&mut buf), 0);
        assert_eq!(src.next_buffer(&mut buf), 0);
        assert!(src.is_exhausted());
        // The cycle flips back to loud: the next pull recovers and passes
        // the audio through.
        assert_eq!(src.next_buffer(&mut buf), 10, "recovered audio");
        assert!(!src.is_exhausted());
        assert!(
            buf.iter().all(|&s| (s - 0.5).abs() < 1e-6),
            "recovered sample value"
        );
    }

    #[test]
    fn on_blank_fires_once_per_episode() {
        let fired = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
        let cb_fired = fired.clone();
        let mut src = BlankDetectSource::new(
            Box::new(LoudQuiet {
                loud: true,
                count: 0,
                loud_bufs: 3,
                quiet_bufs: 4,
            }),
            BlankDetectConfig {
                threshold_db: -40.0,
                duration_secs: 0.2,
                restart_secs: 0.1,
                exhaust_while_blank: true,
                on_blank: Some(Box::new(move || {
                    cb_fired.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                })),
                sample_rate: 100,
                channels: 1,
            },
        );
        let mut buf = vec![0f32; 10];
        // Two episodes (loud -> blank -> recover -> loud -> blank): two fires.
        for _ in 0..5 {
            src.next_buffer(&mut buf);
        }
        assert_eq!(
            fired.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "first episode fired once"
        );
        for _ in 0..9 {
            src.next_buffer(&mut buf);
        }
        assert_eq!(
            fired.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "second episode fired once more"
        );
    }

    #[test]
    fn exhaust_while_blank_false_reports_the_child_state() {
        let mut src = BlankDetectSource::new(
            Box::new(LoudQuiet {
                loud: true,
                count: 0,
                loud_bufs: 1,
                quiet_bufs: 8,
            }),
            BlankDetectConfig {
                threshold_db: -40.0,
                duration_secs: 0.1,
                restart_secs: 0.1,
                exhaust_while_blank: false,
                on_blank: None,
                sample_rate: 100,
                channels: 1,
            },
        );
        let mut buf = vec![0f32; 10];
        for _ in 0..4 {
            src.next_buffer(&mut buf);
        }
        // Blank, but the (never-exhausting) child's state is reported: a
        // bare-root blank.detect keeps the engine alive on silence instead
        // of shutting it down.
        assert!(!src.is_exhausted(), "child never exhausts");
        assert_eq!(src.next_buffer(&mut buf), 0, "still blank");
    }
}
