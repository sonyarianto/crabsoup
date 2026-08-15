//! Parametric EQ and simple filters — biquads from the RBJ Audio EQ
//! Cookbook, in Direct Form 1. Each band is one biquad per channel, chained
//! in series; state lives on the struct, so the hot path allocates nothing.

use crate::engine::effects::Effect;

/// Filter shapes available to a band.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EqType {
    LowPass,
    HighPass,
    BandPass,
    Notch,
    Peaking,
    LowShelf,
    HighShelf,
}

/// A band descriptor: shape, centre frequency (Hz), gain (dB, peaking and
/// shelves only) and Q (bandwidth).
#[derive(Clone, Copy, Debug)]
pub struct EqBand {
    pub kind: EqType,
    pub freq: f32,
    pub gain_db: f32,
    pub q: f32,
}

/// One second-order section, Direct Form 1:
/// `y[n] = b0·x[n] + b1·x[n-1] + b2·x[n-2] − a1·y[n-1] − a2·y[n-2]`.
#[derive(Clone, Debug)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    /// RBJ cookbook coefficients, normalized by `a0`. `freq` must be in
    /// `(0, fs/2)`; `q > 0`; gain only meaningful for peaking/shelves.
    pub fn new(kind: EqType, freq: f32, gain_db: f32, q: f32, fs: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * freq / fs;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a = 10f32.powf(gain_db / 40.0);
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
        let two_cos = -2.0 * cos;

        let (mut b0, mut b1, mut b2, a0, a1, a2) = match kind {
            EqType::LowPass => (
                (1.0 - cos) / 2.0,
                1.0 - cos,
                (1.0 - cos) / 2.0,
                1.0 + alpha,
                two_cos,
                1.0 - alpha,
            ),
            EqType::HighPass => (
                (1.0 + cos) / 2.0,
                -(1.0 + cos),
                (1.0 + cos) / 2.0,
                1.0 + alpha,
                two_cos,
                1.0 - alpha,
            ),
            EqType::BandPass => (alpha, 0.0, -alpha, 1.0 + alpha, two_cos, 1.0 - alpha),
            EqType::Notch => (1.0, two_cos, 1.0, 1.0 + alpha, two_cos, 1.0 - alpha),
            EqType::Peaking => (
                1.0 + alpha * a,
                two_cos,
                1.0 - alpha * a,
                1.0 + alpha / a,
                two_cos,
                1.0 - alpha / a,
            ),
            EqType::LowShelf => (
                a * ((a + 1.0) - (a - 1.0) * cos + two_sqrt_a_alpha),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - two_sqrt_a_alpha),
                (a + 1.0) + (a - 1.0) * cos + two_sqrt_a_alpha,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - two_sqrt_a_alpha,
            ),
            EqType::HighShelf => (
                a * ((a + 1.0) + (a - 1.0) * cos + two_sqrt_a_alpha),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                a * ((a + 1.0) + (a - 1.0) * cos - two_sqrt_a_alpha),
                (a + 1.0) - (a - 1.0) * cos + two_sqrt_a_alpha,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - two_sqrt_a_alpha,
            ),
        };
        b0 /= a0;
        b1 /= a0;
        b2 /= a0;
        Self {
            b0,
            b1,
            b2,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    #[inline]
    pub fn tick(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// A chain of biquads applied per channel (`filters[c][band]`). Bands run
/// in series inside each channel.
pub struct Eq {
    filters: Vec<Vec<Biquad>>,
    channels: usize,
}

impl Eq {
    /// Build an EQ with one chain of `bands` per channel at `fs` Hz. Returns
    /// an error string if a band is unusable (freq outside `(0, fs/2)` or
    /// `q <= 0`).
    pub fn new(bands: &[EqBand], channels: usize, fs: f32) -> Result<Self, String> {
        if channels == 0 {
            return Err("eq: channels must be non-zero".into());
        }
        let mut built = Vec::with_capacity(bands.len());
        for b in bands {
            if !b.freq.is_finite() || b.freq <= 0.0 || b.freq >= fs / 2.0 {
                return Err(format!(
                    "eq: band frequency {:.1} Hz must be in (0, {:.0} Hz)",
                    b.freq,
                    fs / 2.0
                ));
            }
            if !b.q.is_finite() || b.q <= 0.0 {
                return Err("eq: q must be a positive finite number".into());
            }
            if !b.gain_db.is_finite() {
                return Err("eq: gain must be a finite number".into());
            }
            let per_channel: Vec<Biquad> = (0..channels)
                .map(|_| Biquad::new(b.kind, b.freq, b.gain_db, b.q, fs))
                .collect();
            built.push(per_channel);
        }
        let mut filters = Vec::with_capacity(channels);
        for c in 0..channels {
            filters.push(built.iter().map(|b| b[c].clone()).collect());
        }
        Ok(Self { filters, channels })
    }
}

impl Effect for Eq {
    fn process(&mut self, buf: &mut [f32], channels: usize) {
        debug_assert_eq!(channels, self.channels);
        let ch = self.channels;
        let frames = buf.len() / ch;
        if frames == 0 || ch == 0 {
            return;
        }
        for c in 0..ch {
            let chain = &mut self.filters[c];
            for f in 0..frames {
                let mut x = buf[f * ch + c];
                for b in chain.iter_mut() {
                    x = b.tick(x);
                }
                buf[f * ch + c] = x;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::effects::EffectSource;
    use crate::source::AudioSource;

    const RATE: f32 = 44_100.0;

    fn sine(freq: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| (i as f32 * 2.0 * std::f32::consts::PI * freq / RATE).sin())
            .collect()
    }

    /// Steady-state RMS after skipping the transient.
    fn rms(sig: &[f32]) -> f32 {
        let skip = sig.len() / 4;
        let tail = &sig[skip..];
        (tail.iter().map(|&s| s * s).sum::<f32>() / tail.len() as f32).sqrt()
    }

    fn run_eq(bands: &[EqBand], freq: f32) -> f32 {
        let mut fx = Eq::new(bands, 1, RATE).unwrap();
        let input = sine(freq, 4096);
        let mut buf = input.clone();
        fx.process(&mut buf, 1);
        rms(&buf) / rms(&input)
    }

    #[test]
    fn lowpass_blocks_high_freq_and_passes_low() {
        let lp = [EqBand {
            kind: EqType::LowPass,
            freq: 400.0,
            gain_db: 0.0,
            q: 0.707,
        }];
        assert!(run_eq(&lp, 10_000.0) < 0.02, "10 kHz blocked");
        let ratio = run_eq(&lp, 50.0);
        assert!(ratio > 0.9, "50 Hz passed, ratio {ratio}");
    }

    #[test]
    fn highpass_blocks_low_freq_and_passes_high() {
        let hp = [EqBand {
            kind: EqType::HighPass,
            freq: 400.0,
            gain_db: 0.0,
            q: 0.707,
        }];
        assert!(run_eq(&hp, 40.0) < 0.02, "40 Hz blocked");
        let ratio = run_eq(&hp, 5_000.0);
        assert!(ratio > 0.9, "5 kHz passed, ratio {ratio}");
    }

    #[test]
    fn peaking_boosts_its_centre_frequency() {
        let band = [EqBand {
            kind: EqType::Peaking,
            freq: 1_000.0,
            gain_db: 6.0,
            q: 2.0,
        }];
        let ratio = run_eq(&band, 1_000.0);
        assert!((1.6..2.4).contains(&ratio), "~6 dB boost, ratio {ratio}");
    }

    #[test]
    fn zero_gain_peaking_is_passthrough() {
        let band = [EqBand {
            kind: EqType::Peaking,
            freq: 1_000.0,
            gain_db: 0.0,
            q: 1.0,
        }];
        let input = sine(440.0, 2048);
        let mut buf = input.clone();
        Eq::new(&band, 1, RATE).unwrap().process(&mut buf, 1);
        for (i, (o, r)) in buf.iter().zip(&input).enumerate() {
            assert!((o - r).abs() < 1e-3, "sample {i}: {o} vs {r}");
        }
    }

    #[test]
    fn stereo_channels_are_independent() {
        let band = [EqBand {
            kind: EqType::HighShelf,
            freq: 3_000.0,
            gain_db: -12.0,
            q: 0.707,
        }];
        let mut fx = Eq::new(&band, 2, RATE).unwrap();
        // Left carries 100 Hz, right carries 10 kHz; the shelf must not
        // smear one channel into the other.
        let mut buf = Vec::new();
        for i in 0..2048 {
            let (l, r) = (
                (i as f32 * 2.0 * std::f32::consts::PI * 100.0 / RATE).sin(),
                (i as f32 * 2.0 * std::f32::consts::PI * 10_000.0 / RATE).sin(),
            );
            buf.push(l);
            buf.push(r);
        }
        fx.process(&mut buf, 2);
        let left: Vec<f32> = buf.iter().step_by(2).copied().collect();
        let right: Vec<f32> = buf.iter().skip(1).step_by(2).copied().collect();
        // 100 Hz is far below the 3 kHz shelf corner: nearly untouched.
        let left_ratio = rms(&left) / rms(&sine(100.0, 2048));
        assert!(left_ratio > 0.9, "left (100 Hz) passed: {left_ratio}");
        // 10 kHz is 10 dB+ down on the -12 dB shelf.
        let right_ratio = rms(&right) / rms(&sine(10_000.0, 2048));
        assert!(right_ratio < 0.35, "right (10 kHz) cut: {right_ratio}");
    }

    #[test]
    fn rejects_bad_bands() {
        let bad_freq = [EqBand {
            kind: EqType::LowPass,
            freq: 22_050.0,
            gain_db: 0.0,
            q: 1.0,
        }];
        assert!(Eq::new(&bad_freq, 2, RATE).is_err(), "Nyquist rejected");
        let bad_q = [EqBand {
            kind: EqType::Peaking,
            freq: 1_000.0,
            gain_db: 3.0,
            q: 0.0,
        }];
        assert!(Eq::new(&bad_q, 2, RATE).is_err(), "q=0 rejected");
    }

    #[test]
    fn output_stays_bounded_under_noise() {
        // Feed broadband noise through a resonant peaking stage; a stable
        // biquad keeps the output finite.
        let mut rng = 0x1234_5678u32;
        let band = [EqBand {
            kind: EqType::Peaking,
            freq: 500.0,
            gain_db: 12.0,
            q: 8.0,
        }];
        let mut fx = Eq::new(&band, 1, RATE).unwrap();
        let mut buf = vec![0.0f32; 4096];
        for s in &mut buf {
            rng = rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *s = (rng >> 16) as f32 / 32768.0 - 1.0;
        }
        fx.process(&mut buf, 1);
        let peak = buf.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        assert!(peak.is_finite() && peak < 10.0, "stable: peak {peak}");
    }

    #[test]
    fn effect_source_chain_runs_alloc_free_path() {
        let band = [EqBand {
            kind: EqType::Peaking,
            freq: 1_000.0,
            gain_db: 3.0,
            q: 1.0,
        }];
        let child: Box<dyn AudioSource> =
            Box::new(crate::source::SineSource::new(440.0, None, 0.5, 44_100, 2));
        let mut chain = EffectSource::new(child, Eq::new(&band, 2, 44_100.0).unwrap(), 2);
        let mut buf = vec![0.0f32; 8192];
        for _ in 0..4 {
            let n = chain.next_buffer(&mut buf);
            assert!(n > 0);
            assert!(buf[..n].iter().all(|s| s.is_finite()));
        }
    }
}
