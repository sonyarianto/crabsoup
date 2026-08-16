//! Per-track gain scaling from the `amplify` annotation.
//!
//! Wraps a resolved source and applies one constant multiplier to every
//! sample, like the `amplify(src, gain)` operator but scoped to a single
//! track (the wrapper is built per request in `request::apply_cues`). The
//! gain comes from the annotation's plain factor (`"0.7"`) or dB value
//! (`"-8.2 dB"`), converted to linear during parsing.

use crate::source::AudioSource;

pub struct TrackGainSource {
    child: Box<dyn AudioSource>,
    gain: f32,
}

impl TrackGainSource {
    pub fn new(child: Box<dyn AudioSource>, gain: f32) -> Self {
        Self { child, gain }
    }
}

impl AudioSource for TrackGainSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        let n = self.child.next_buffer(buffer);
        if self.gain != 1.0 {
            for s in &mut buffer[..n] {
                *s *= self.gain;
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

    fn next_label(&self) -> Option<String> {
        self.child.next_label()
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

    const RATE: u32 = 100;
    const CHANS: usize = 1;

    #[test]
    fn scales_every_sample_by_the_gain() {
        let child: Box<dyn AudioSource> = Box::new(SineSource::new(25.0, None, 0.5, RATE, CHANS));
        let mut src = TrackGainSource::new(child, 0.5);
        let mut buf = vec![0f32; 100];
        let n = src.next_buffer(&mut buf);
        // 25 Hz at 100 Hz: samples are 0.5, 0, -0.5, 0, ...
        assert!((buf[0]).abs() < 1e-6);
        assert!((buf[1] - 0.25).abs() < 1e-6, "half amplitude");
        assert!((buf[3] + 0.25).abs() < 1e-6, "half amplitude (negative)");
        assert_eq!(n, 100);
    }

    #[test]
    fn unit_gain_passes_samples_through_unchanged() {
        let child: Box<dyn AudioSource> = Box::new(SineSource::new(25.0, None, 0.5, RATE, CHANS));
        let mut src = TrackGainSource::new(child, 1.0);
        let mut buf = vec![1.0f32; 10];
        src.next_buffer(&mut buf);
        assert!((buf[1] - 0.5).abs() < 1e-6, "no scaling at gain 1.0");
    }

    #[test]
    fn forwards_crossfade_overrides_and_label() {
        let child: Box<dyn AudioSource> = Box::new(SineSource::new(25.0, None, 0.5, RATE, CHANS));
        let src = TrackGainSource::new(child, 2.0);
        assert_eq!(src.label().as_deref(), Some("sine 25 Hz"));
        assert_eq!(src.crossfade_overrides(), None);
    }
}
