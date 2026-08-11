//! Per-track ReplayGain scaling (Liquidsoap `replaygain`).
//!
//! Wraps a child and applies a constant gain taken from the current track's
//! `REPLAYGAIN_*` tags. The gain is re-read at every track boundary (the
//! child's `label` changing), so a playlist of differently-mastered files
//! gets a consistent loudness baseline before any AGC/normalize stage that
//! follows. Tracks without tags play at 0 dB.

use crate::engine::effects::db_to_gain;
use crate::source::AudioSource;

pub struct ReplayGainSource {
    child: Box<dyn AudioSource>,
    /// Gain applied to the current track, in dB.
    gain_db: f32,
    /// The child's label as of the last boundary check.
    last_label: Option<String>,
    /// Applied gain is clamped to `[-max_cut_db, +max_boost_db]`.
    max_boost_db: f32,
    max_cut_db: f32,
}

impl ReplayGainSource {
    pub fn new(child: Box<dyn AudioSource>, max_boost_db: f32, max_cut_db: f32) -> Self {
        Self {
            child,
            gain_db: 0.0,
            last_label: None,
            max_boost_db,
            max_cut_db,
        }
    }

    /// Re-read the track gain when the child's label changed (a new track).
    /// Returns true when the gain actually changed.
    fn refresh(&mut self) -> bool {
        let label = self.child.label();
        if label == self.last_label {
            return false;
        }
        self.last_label = label;
        let raw = self.child.replaygain_db().unwrap_or(0.0);
        let gain_db = raw.clamp(-self.max_cut_db, self.max_boost_db);
        if (gain_db - self.gain_db).abs() > 1e-3 {
            log::info!("replaygain: track gain {raw:.1} dB (applying {gain_db:.1} dB)");
            self.gain_db = gain_db;
            true
        } else {
            false
        }
    }
}

impl AudioSource for ReplayGainSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        let n = self.child.next_buffer(buffer);
        self.refresh();
        if self.gain_db != 0.0 {
            let gain = db_to_gain(self.gain_db);
            for s in &mut buffer[..n] {
                *s *= gain;
            }
        }
        n
    }

    fn is_exhausted(&self) -> bool {
        self.child.is_exhausted()
    }

    fn remaining_seconds(&self) -> Option<f64> {
        self.child.remaining_seconds()
    }

    fn label(&self) -> Option<String> {
        self.child.label()
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
    use std::sync::{Arc, Mutex};

    /// A child that plays `value` per buffer and reports a track label and
    /// per-track replaygain, so boundary behaviour is fully controlled.
    struct GainCycler {
        tracks: Vec<(String, Option<f32>)>,
        index: usize,
        value: f32,
    }

    impl GainCycler {
        fn advance(&mut self) {
            self.index = (self.index + 1).min(self.tracks.len() - 1);
        }
    }

    impl AudioSource for GainCycler {
        fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
            buffer.fill(self.value);
            buffer.len()
        }
        fn is_exhausted(&self) -> bool {
            false
        }
        fn label(&self) -> Option<String> {
            Some(self.tracks[self.index].0.clone())
        }
        fn replaygain_db(&self) -> Option<f32> {
            self.tracks[self.index].1
        }
    }

    #[test]
    fn applies_track_gain_and_switches_per_track() {
        let tracks = Arc::new(Mutex::new(GainCycler {
            tracks: vec![
                ("quiet".into(), Some(-6.0)),
                ("loud".into(), Some(6.0)),
                ("untagged".into(), None),
            ],
            index: 0,
            value: 0.5,
        }));
        let child: Box<dyn AudioSource> = Box::new(ArcSource(tracks.clone()));
        let mut src = ReplayGainSource::new(child, 12.0, 12.0);
        let mut buf = vec![0f32; 4];

        let n = src.next_buffer(&mut buf);
        let expected = 0.5 * db_to_gain(-6.0);
        assert!(buf[..n].iter().all(|&s| (s - expected).abs() < 1e-6));

        let first: Vec<f32> = buf[..n].to_vec();
        tracks.lock().unwrap().advance();
        let n = src.next_buffer(&mut buf);
        let expected = 0.5 * db_to_gain(6.0);
        assert!(buf[..n].iter().all(|&s| (s - expected).abs() < 1e-6));
        assert!((buf[0] - first[0]).abs() > 0.05, "gain must change");

        tracks.lock().unwrap().advance();
        let n = src.next_buffer(&mut buf);
        assert!(
            buf[..n].iter().all(|&s| (s - 0.5).abs() < 1e-6),
            "untagged track plays at unity"
        );
    }

    #[test]
    fn clamps_the_applied_gain() {
        let tracks = Arc::new(Mutex::new(GainCycler {
            tracks: vec![("huge".into(), Some(30.0))],
            index: 0,
            value: 0.5,
        }));
        let child: Box<dyn AudioSource> = Box::new(ArcSource(tracks));
        let mut src = ReplayGainSource::new(child, 6.0, 6.0);
        let mut buf = vec![0f32; 4];
        src.next_buffer(&mut buf);
        let expected = 0.5 * db_to_gain(6.0);
        assert!((buf[0] - expected).abs() < 1e-6);
    }

    #[test]
    fn gain_is_unchanged_within_a_track() {
        let child: Box<dyn AudioSource> = Box::new(ArcSource(Arc::new(Mutex::new(
            GainCycler {
                tracks: vec![("a".into(), Some(-3.0))],
                index: 0,
                value: 0.5,
            },
        ))));
        let mut src = ReplayGainSource::new(child, 12.0, 12.0);
        let mut buf = vec![0f32; 4];
        let first = {
            src.next_buffer(&mut buf);
            buf.clone()
        };
        src.next_buffer(&mut buf);
        assert_eq!(buf, first);
    }

    /// Boxable test wrapper delegating to a shared cycler.
    struct ArcSource(Arc<Mutex<GainCycler>>);

    impl AudioSource for ArcSource {
        fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
            self.0.lock().unwrap().next_buffer(buffer)
        }
        fn is_exhausted(&self) -> bool {
            self.0.lock().unwrap().is_exhausted()
        }
        fn label(&self) -> Option<String> {
            self.0.lock().unwrap().label()
        }
        fn replaygain_db(&self) -> Option<f32> {
            self.0.lock().unwrap().replaygain_db()
        }
    }
}
