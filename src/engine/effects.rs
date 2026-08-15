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

pub(crate) fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

fn gain_to_db(gain: f32) -> f32 {
    20.0 * gain.max(1e-6).log10()
}

/// One-pole smoothing coefficient for an attack/release time constant.
/// `seconds <= 0` means "instant".
fn env_coef(seconds: f32, sample_rate: u32) -> f32 {
    if seconds <= 0.0 {
        1.0
    } else {
        1.0 - (-1.0 / (seconds * sample_rate as f32)).exp()
    }
}

/// A feed-forward dynamics compressor (Liquidsoap `compress`).
///
/// Tracks a smoothed envelope of the signal; whenever the envelope sits
/// above `threshold_db`, the gain is reduced so the over-threshold portion
/// passes at `1 / ratio` dB per dB. `makeup_db` is a constant boost applied
/// after the gain reduction.
pub struct Compressor {
    threshold_db: f32,
    ratio: f32,
    makeup_db: f32,
    attack: f32,
    release: f32,
    sample_rate: u32,
    /// Smoothed absolute level (envelope follower).
    env: f32,
}

impl Compressor {
    pub fn new(
        threshold_db: f32,
        ratio: f32,
        attack: f32,
        release: f32,
        makeup_db: f32,
        sample_rate: u32,
    ) -> Self {
        Self {
            threshold_db,
            ratio,
            makeup_db,
            attack,
            release,
            sample_rate,
            env: 0.0,
        }
    }
}

impl Effect for Compressor {
    fn process(&mut self, buf: &mut [f32], _channels: usize) {
        let up = env_coef(self.attack, self.sample_rate);
        let down = env_coef(self.release, self.sample_rate);
        let makeup = db_to_gain(self.makeup_db);
        let ratio_inv = 1.0 / self.ratio;
        for s in buf {
            let level = s.abs();
            if level > self.env {
                self.env += up * (level - self.env);
            } else {
                self.env += down * (level - self.env);
            }
            let over = gain_to_db(self.env) - self.threshold_db;
            let gain_db = if over > 0.0 {
                over * (ratio_inv - 1.0)
            } else {
                0.0
            };
            *s *= db_to_gain(gain_db) * makeup;
        }
    }
}

/// A live gain rider (Liquidsoap `normalize`).
///
/// Same envelope-follower shape as [`Compressor`], but instead of limiting
/// above a threshold it moves the envelope toward `target_db`: quiet input
/// is boosted, loud input is cut. The gain itself is smoothed (boost slow,
/// cut fast) so silence does not pump the level up; `max_boost_db` /
/// `max_cut_db` bound the excursion.
pub struct Agc {
    target_db: f32,
    /// Time constant (seconds) for gain rising toward a boost (slow).
    attack: f32,
    /// Time constant (seconds) for gain dropping toward a cut (fast).
    release: f32,
    max_boost_db: f32,
    max_cut_db: f32,
    sample_rate: u32,
    env: f32,
    /// Currently applied gain, in dB.
    gain_db: f32,
}

/// The level-measurement stage rides the signal much faster than the gain
/// does: 10 ms to notice loudness, 500 ms to forget it.
const ENV_ATTACK_SECS: f32 = 0.01;
const ENV_RELEASE_SECS: f32 = 0.5;

impl Agc {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        target_db: f32,
        attack: f32,
        release: f32,
        max_boost_db: f32,
        max_cut_db: f32,
        sample_rate: u32,
    ) -> Self {
        Self {
            target_db,
            attack,
            release,
            max_boost_db,
            max_cut_db,
            sample_rate,
            env: 0.0,
            gain_db: 0.0,
        }
    }
}

impl Effect for Agc {
    fn process(&mut self, buf: &mut [f32], _channels: usize) {
        let env_up = env_coef(ENV_ATTACK_SECS, self.sample_rate);
        let env_down = env_coef(ENV_RELEASE_SECS, self.sample_rate);
        let gain_up = env_coef(self.attack, self.sample_rate);
        let gain_down = env_coef(self.release, self.sample_rate);
        for s in buf {
            let level = s.abs();
            if level > self.env {
                self.env += env_up * (level - self.env);
            } else {
                self.env += env_down * (level - self.env);
            }
            let target =
                (self.target_db - gain_to_db(self.env)).clamp(-self.max_cut_db, self.max_boost_db);
            // Rise toward a boost slowly (avoids pumping on silence), drop
            // toward a cut quickly (loud transients get clamped fast).
            let alpha = if target > self.gain_db {
                gain_up
            } else {
                gain_down
            };
            self.gain_db += alpha * (target - self.gain_db);
            *s *= db_to_gain(self.gain_db);
        }
    }
}

/// A multi-tap echo/delay (Liquidsoap `echo`).
///
/// Each tap reads the shared delay line at its own offset and adds
/// `ping × read` to the dry signal; the value written back into the line is
/// `dry + feedback × tapped`, so the echoes ring down at `feedback` gain.
/// A tap's read position always trails its write position, so nothing in the
/// buffer is ever read before it is written; a tap delayed exactly
/// `max_delay` reads the slot about to be overwritten, which is the value
/// written one full line ago — the correct full-delay echo.
pub struct Echo {
    /// `(delay in interleaved samples, ping)`.
    taps: Vec<(usize, f32)>,
    feedback: f32,
    /// Circular delay line, one interleaved sample per slot.
    line: Vec<f32>,
    pos: usize,
}

impl Echo {
    /// `taps` is `(delay seconds, ping)`; `max_delay` bounds the line and
    /// any tap beyond it is clamped to it. `channels` sizes the delay in
    /// interleaved samples (a `delay` of one frame = `channels` slots).
    pub fn new(
        taps: &[(f64, f32)],
        feedback: f32,
        max_delay: f64,
        sample_rate: u32,
        channels: usize,
    ) -> Self {
        let line_frames = ((max_delay * sample_rate as f64).round() as usize).max(1);
        let line = vec![0.0f32; line_frames * channels.max(1)];
        let taps = taps
            .iter()
            .map(|&(secs, ping)| {
                let frames = ((secs * sample_rate as f64).round() as usize).max(1);
                ((frames * channels.max(1)).min(line.len()), ping)
            })
            .collect();
        Self {
            taps,
            feedback,
            line,
            pos: 0,
        }
    }
}

impl Effect for Echo {
    fn process(&mut self, buf: &mut [f32], _channels: usize) {
        let len = self.line.len();
        if len == 0 {
            return;
        }
        for s in buf.iter_mut() {
            let dry = *s;
            let mut acc = 0.0;
            for (delay, ping) in &self.taps {
                let read = (self.pos + len - delay) % len;
                acc += ping * self.line[read];
            }
            self.line[self.pos] = dry + self.feedback * acc;
            self.pos = (self.pos + 1) % len;
            *s = dry + acc;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SineSource;

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
    fn echo_rings_down_an_impulse() {
        // 10 Hz sample clock, 0.2 s delay = 2 samples; the ping 0.5 copy
        // rings at feedback 1.0: 1, 0, 0.5, 0, 0.25, 0, 0.125, 0.
        let mut fx = Echo::new(&[(0.2, 0.5)], 1.0, 1.0, 10, 1);
        let mut buf = vec![1.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        fx.process(&mut buf, 1);
        assert_eq!(buf, vec![1.0, 0.0, 0.5, 0.0, 0.25, 0.0, 0.125, 0.0]);
    }

    #[test]
    fn echo_feedback_zero_emits_a_single_copy() {
        let mut fx = Echo::new(&[(0.2, 0.5)], 0.0, 1.0, 10, 1);
        let mut buf = vec![1.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        fx.process(&mut buf, 1);
        assert_eq!(buf, vec![1.0, 0.0, 0.5, 0.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn echo_multi_tap_emits_each_tap() {
        // Taps at 0.1 s (1 sample, ping 0.5) and 0.2 s (2 samples, ping 0.25).
        let mut fx = Echo::new(&[(0.1, 0.5), (0.2, 0.25)], 0.0, 1.0, 10, 1);
        let mut buf = vec![1.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        fx.process(&mut buf, 1);
        assert_eq!(buf[0], 1.0);
        assert_eq!(buf[1], 0.5);
        assert_eq!(buf[2], 0.25);
        assert_eq!(buf[3], 0.0);
    }

    #[test]
    fn echo_stereo_delay_counts_frames_not_samples() {
        // 0.2 s at a 10 Hz clock = 2 frames = 4 interleaved samples; the
        // mono impulse at frame 0 (L=1, R=0) echoes into frame 2's L slot.
        let mut fx = Echo::new(&[(0.2, 0.5)], 0.0, 1.0, 10, 2);
        let mut buf = vec![0.0f32; 16];
        buf[0] = 1.0;
        fx.process(&mut buf, 2);
        assert_eq!(buf[4], 0.5, "delayed L copy");
        assert_eq!(buf[5], 0.0, "delayed R stays silent");
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

    #[test]
    fn compressor_passes_signals_below_threshold_unchanged() {
        // 25 Hz / 100 Hz sine at -6 dB peak: every other sample is 0.5.
        let mut src = SineSource::new(25.0, None, 0.5, 100, 1);
        let mut fx = Compressor::new(-3.0, 2.0, 0.0, 0.0, 0.0, 100);
        let mut buf = vec![0f32; 8];
        let n = src.next_buffer(&mut buf);
        fx.process(&mut buf[..n], 1);
        assert_eq!(buf[1], 0.5);
        assert_eq!(buf[3], -0.5);
    }

    #[test]
    fn compressor_reduces_only_above_the_threshold() {
        // 0 dB peaks, -6 dB threshold, 2:1 ratio: the 6 dB over-threshold
        // portion passes at 3 dB -> gain 10^(-3/20) on the peaks.
        let mut src = SineSource::new(25.0, None, 1.0, 100, 1);
        let mut fx = Compressor::new(-6.0, 2.0, 0.0, 0.0, 0.0, 100);
        let mut buf = vec![0f32; 8];
        let n = src.next_buffer(&mut buf);
        fx.process(&mut buf[..n], 1);
        let gain = 10f32.powf(-3.0 / 20.0);
        assert!(buf[0].abs() < 1e-6);
        assert!((buf[1] - gain).abs() < 1e-6);
        assert!(buf[2].abs() < 1e-6);
        assert!((buf[3] + gain).abs() < 1e-6);
    }

    #[test]
    fn compressor_ratio_one_is_transparent() {
        let mut src = SineSource::new(25.0, None, 1.0, 100, 1);
        let mut fx = Compressor::new(-6.0, 1.0, 0.0, 0.0, 0.0, 100);
        let mut buf = vec![0f32; 8];
        let n = src.next_buffer(&mut buf);
        fx.process(&mut buf[..n], 1);
        assert_eq!(buf[1], 1.0);
        assert_eq!(buf[3], -1.0);
    }

    #[test]
    fn compressor_makeup_gain_boosts_below_threshold_too() {
        let mut src = SineSource::new(25.0, None, 0.5, 100, 1);
        let mut fx = Compressor::new(-6.0, 2.0, 0.0, 0.0, 6.0, 100);
        let mut buf = vec![0f32; 4];
        let n = src.next_buffer(&mut buf);
        fx.process(&mut buf[..n], 1);
        assert!((buf[1] - 0.5 * 10f32.powf(6.0 / 20.0)).abs() < 1e-6);
    }

    #[test]
    fn agc_boosts_a_quiet_sine_toward_the_target() {
        // 0.05 is -26 dB; targeting -6 needs +20 dB, exactly the max boost.
        let mut src = SineSource::new(25.0, None, 0.05, 100, 1);
        let mut fx = Agc::new(-6.0, 0.0, 0.0, 20.0, 20.0, 100);
        let mut buf = vec![0f32; 100];
        let n = src.next_buffer(&mut buf);
        fx.process(&mut buf[..n], 1);
        let peak = buf.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!((peak - 0.5).abs() < 1e-3, "peak {peak}");
    }

    #[test]
    fn agc_cuts_loud_input_toward_the_target() {
        // 1000 samples let the measurement envelope converge on the 2.0
        // level; with instant gain the last sample sits at the target.
        let mut fx = Agc::new(-6.0, 0.0, 0.0, 20.0, 20.0, 100);
        let mut buf = vec![2.0f32; 1000];
        fx.process(&mut buf, 1);
        let expected = 2.0 * 10f32.powf((-6.0 - 20.0 * 2.0f32.log10()) / 20.0);
        assert!((buf[999] - expected).abs() < 1e-4, "got {}", buf[999]);
    }

    #[test]
    fn agc_max_boost_clamps_for_near_silence() {
        let mut fx = Agc::new(-6.0, 0.0, 0.0, 20.0, 20.0, 100);
        let mut buf = vec![0.001f32; 4];
        fx.process(&mut buf, 1);
        assert!((buf[3] - 0.01).abs() < 1e-4, "got {}", buf[3]);
    }

    #[test]
    fn agc_gain_rides_slowly_so_silence_does_not_pump() {
        // Loud 0.5 for 50 samples, then quiet 0.05: the gain must climb
        // gradually (1 s attack), not jump to full boost.
        let mut fx = Agc::new(-6.0, 1.0, 0.1, 20.0, 20.0, 100);
        let mut buf = vec![0.5f32; 50];
        fx.process(&mut buf, 1);
        let mut buf = vec![0.05f32; 100];
        fx.process(&mut buf, 1);
        assert!(buf[0] > 0.05 - 1e-4, "gain dropped at the quiet start");
        assert!(
            buf[99] > buf[0],
            "gain should rise through the quiet segment"
        );
        assert!(
            buf[99] < 0.5,
            "gain jumped to full boost (pumping): {}",
            buf[99]
        );
    }
}
