//! Stereo imaging: balance-style panning and mid-side width control.
//!
//! Pan (`-1` hard left … `+1` hard right) is a balance: the far channel
//! fades with a cos/sin curve that is exactly unity at center and zero at
//! the hard extreme, so `pan = 0` is an exact passthrough (important when
//! chaining effects without level surprises). Width is mid-side: the
//! signal is decoded to mid/side, the side is scaled by `width` and the
//! pair re-encoded — `width = 1` passes, `0` collapses to mono, `> 1`
//! widens. Per-frame arithmetic only, no allocation.

use crate::engine::effects::Effect;

const FRAC_PI_2: f32 = std::f32::consts::FRAC_PI_2;

/// Stereo image processor: balance pan + mid-side width.
pub struct Stereo {
    pan: f32,
    width: f32,
}

impl Stereo {
    /// `pan` must be finite and in `[-1, 1]`; `width` finite and `>= 0`.
    pub fn new(pan: f32, width: f32) -> Result<Self, String> {
        if !pan.is_finite() || !(-1.0..=1.0).contains(&pan) {
            return Err(format!("stereo: pan {pan} must be in [-1, 1]"));
        }
        if !width.is_finite() || width < 0.0 {
            return Err(format!(
                "stereo: width {width} must be a finite non-negative number"
            ));
        }
        Ok(Self { pan, width })
    }
}

impl Effect for Stereo {
    fn process(&mut self, buf: &mut [f32], channels: usize) {
        if channels != 2 {
            return;
        }
        // Balance law: the channel the image moves toward stays at unity,
        // the far channel fades on a cos/sin quarter wave (unity at centre,
        // zero at the hard extreme).
        let (gl, gr) = if self.pan <= 0.0 {
            (1.0, ((self.pan + 1.0) * FRAC_PI_2).sin())
        } else {
            (((1.0 - self.pan) * FRAC_PI_2).sin(), 1.0)
        };
        let w = self.width;
        for pair in buf.chunks_exact_mut(2) {
            let l = pair[0];
            let r = pair[1];
            let mid = (l + r) * 0.5;
            let side = (l - r) * 0.5;
            let (lw, rw) = (mid + w * side, mid - w * side);
            pair[0] = lw * gl;
            pair[1] = rw * gr;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::effects::EffectSource;
    use crate::source::AudioSource;

    /// Left/right tone pair (different freqs so channels are distinguishable).
    fn stereo_pair(frames: usize) -> Vec<f32> {
        let mut buf = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let t = i as f32 / 44_100.0;
            buf.push((2.0 * std::f32::consts::PI * 220.0 * t).sin());
            buf.push((2.0 * std::f32::consts::PI * 440.0 * t).sin());
        }
        buf
    }

    fn run(pan: f32, width: f32, input: &[f32]) -> Vec<f32> {
        let mut fx = Stereo::new(pan, width).unwrap();
        let mut buf = input.to_vec();
        fx.process(&mut buf, 2);
        buf
    }

    #[test]
    fn center_is_exact_passthrough() {
        let input = stereo_pair(512);
        let out = run(0.0, 1.0, &input);
        for (i, (o, r)) in out.iter().zip(&input).enumerate() {
            assert!((o - r).abs() < 1e-6, "sample {i}: {o} vs {r}");
        }
    }

    #[test]
    fn hard_left_keeps_only_the_left_channel() {
        let input = stereo_pair(512);
        let out = run(-1.0, 1.0, &input);
        for i in 0..512 {
            assert!((out[i * 2] - input[i * 2]).abs() < 1e-6, "left kept at {i}");
            assert!(out[i * 2 + 1].abs() < 1e-6, "right muted at {i}");
        }
    }

    #[test]
    fn hard_right_keeps_only_the_right_channel() {
        let input = stereo_pair(512);
        let out = run(1.0, 1.0, &input);
        for i in 0..512 {
            assert!(out[i * 2].abs() < 1e-6, "left muted at {i}");
            assert!(
                (out[i * 2 + 1] - input[i * 2 + 1]).abs() < 1e-6,
                "right kept at {i}"
            );
        }
    }

    #[test]
    fn pan_midpoint_fades_the_far_channel() {
        // pan = -0.5 → the right channel fades to sin(π/4) ≈ 0.707, the left
        // stays at unity.
        let input = stereo_pair(512);
        let out = run(-0.5, 1.0, &input);
        let expected = (0.5f32 * FRAC_PI_2).sin();
        for i in 0..512 {
            assert!(
                (out[i * 2] - input[i * 2]).abs() < 1e-6,
                "left unity at {i}"
            );
            assert!(
                (out[i * 2 + 1] - input[i * 2 + 1] * expected).abs() < 1e-5,
                "right faded at {i}"
            );
        }
    }

    #[test]
    fn zero_width_collapses_to_mono_sum() {
        let input = stereo_pair(512);
        let out = run(0.0, 0.0, &input);
        for i in 0..512 {
            let mid = (input[i * 2] + input[i * 2 + 1]) * 0.5;
            assert!((out[i * 2] - mid).abs() < 1e-6, "left is mid at {i}");
            assert!((out[i * 2 + 1] - mid).abs() < 1e-6, "right is mid at {i}");
        }
    }

    #[test]
    fn width_two_doubles_the_side_component() {
        let input = stereo_pair(512);
        let out = run(0.0, 2.0, &input);
        for i in 0..512 {
            let l = input[i * 2];
            let r = input[i * 2 + 1];
            let side = (l - r) * 0.5;
            assert!(
                (out[i * 2] - (l + side)).abs() < 1e-6,
                "side doubled at {i}"
            );
            assert!(
                (out[i * 2 + 1] - (r - side)).abs() < 1e-6,
                "side doubled at {i}"
            );
        }
    }

    #[test]
    fn rejects_out_of_range_pan_and_negative_width() {
        assert!(Stereo::new(-1.01, 1.0).is_err(), "pan below -1 rejected");
        assert!(Stereo::new(1.5, 1.0).is_err(), "pan above 1 rejected");
        assert!(Stereo::new(f32::NAN, 1.0).is_err(), "NaN pan rejected");
        assert!(Stereo::new(0.0, -0.1).is_err(), "negative width rejected");
    }

    #[test]
    fn mono_bus_passes_through() {
        let mut fx = Stereo::new(0.5, 2.0).unwrap();
        let mut buf = vec![0.25f32; 64];
        fx.process(&mut buf, 1);
        assert!(buf.iter().all(|&s| s == 0.25), "mono untouched");
    }

    #[test]
    fn effect_source_chain_runs_alloc_free_path() {
        let child: Box<dyn AudioSource> =
            Box::new(crate::source::SineSource::new(440.0, None, 0.5, 44_100, 2));
        let mut chain = EffectSource::new(child, Stereo::new(-0.25, 1.4).unwrap(), 2);
        let mut buf = vec![0.0f32; 8192];
        for _ in 0..4 {
            let n = chain.next_buffer(&mut buf);
            assert!(n > 0);
            assert!(buf[..n].iter().all(|s| s.is_finite()));
        }
    }
}
