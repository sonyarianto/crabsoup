//! Inline DSP effects wrapping a child source.
//!
//! Effects run inside the pull chain — no separate threads — and reuse the
//! caller's buffer, so a fully-chained effect stack allocates nothing per
//! `next_buffer` call.

use crate::source::AudioSource;

/// An in-place DSP effect over one buffer of interleaved PCM.
pub trait Effect: Send {
    fn process(&mut self, buf: &mut [f32], channels: usize);
}

/// Applies `E` to every buffer pulled from `child`.
pub struct EffectSource<E: Effect> {
    child: Box<dyn AudioSource>,
    effect: E,
    channels: usize,
}

impl<E: Effect> EffectSource<E> {
    pub fn new(child: Box<dyn AudioSource>, effect: E, channels: usize) -> Self {
        Self {
            child,
            effect,
            channels,
        }
    }
}

impl<E: Effect> AudioSource for EffectSource<E> {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        let n = self.child.next_buffer(buffer);
        self.effect.process(&mut buffer[..n], self.channels);
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

    fn skip(&mut self) {
        self.child.skip();
    }
}

/// Constant-gain multiplication (Liquidsoap `amplify`).
pub struct Amplify {
    gain: f32,
}

impl Amplify {
    pub fn new(gain: f32) -> Self {
        Self { gain }
    }
}

impl Effect for Amplify {
    fn process(&mut self, buf: &mut [f32], _channels: usize) {
        for s in buf {
            *s *= self.gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeSource {
        value: f32,
    }

    impl AudioSource for FakeSource {
        fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
            buffer.fill(self.value);
            buffer.len()
        }
        fn is_exhausted(&self) -> bool {
            false
        }
    }

    #[test]
    fn amplify_scales_every_sample() {
        let child: Box<dyn AudioSource> = Box::new(FakeSource { value: 0.5 });
        let mut src = EffectSource::new(child, Amplify::new(0.5), 2);
        let mut buf = vec![0f32; 8];
        let n = src.next_buffer(&mut buf);
        assert_eq!(n, 8);
        assert!(buf.iter().all(|&s| (s - 0.25).abs() < 1e-6));
    }

    #[test]
    fn amplify_zero_mutes_and_negative_gain_flips_sign() {
        let child: Box<dyn AudioSource> = Box::new(FakeSource { value: 1.0 });
        let mut muted = EffectSource::new(child, Amplify::new(0.0), 1);
        let mut buf = vec![1.0f32; 4];
        muted.next_buffer(&mut buf);
        assert!(buf.iter().all(|&s| s == 0.0));

        let child: Box<dyn AudioSource> = Box::new(FakeSource { value: 1.0 });
        let mut flipped = EffectSource::new(child, Amplify::new(-1.0), 1);
        let mut buf = vec![0f32; 4];
        flipped.next_buffer(&mut buf);
        assert!(buf.iter().all(|&s| (s + 1.0).abs() < 1e-6));
    }

    #[test]
    fn effect_source_forwards_lifecycle_and_label() {
        let child: Box<dyn AudioSource> = Box::new(FakeSource { value: 1.0 });
        let mut src = EffectSource::new(child, Amplify::new(1.0), 2);
        assert!(!src.is_exhausted());
        assert_eq!(src.label(), None);
        src.skip();
    }
}
