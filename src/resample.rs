//! A small streaming windowed-sinc (polyphase) resampler.
//!
//! Symphonia 0.5.5 removed its built-in resampler, so this provides the same
//! job with minimal dependencies. A 16-tap Hann-windowed sinc kernel with 256
//! fractional phases beats linear interpolation for the Opus path
//! (bus -> 48 kHz), where the ratio is not a small integer (e.g. 147/160).
//! Taps near chunk edges and stream boundaries are padded with zeros and the
//! kernel renormalized, so DC passes through with unity gain everywhere.

/// Number of FIR taps per output sample (even).
const TAPS: usize = 16;
/// Fractional positions between consecutive input samples.
const PHASES: usize = 256;
const HALF: f64 = (TAPS / 2) as f64;

/// Precomputed Hann-windowed sinc kernel: `kernel[phase][k]` is the tap weight
/// for input frame `floor(pos) - TAPS/2 + 1 + k`.
fn build_kernel() -> Box<[[f32; TAPS]; PHASES]> {
    let mut table = Box::new([[0.0f32; TAPS]; PHASES]);
    for (ph, row) in table.iter_mut().enumerate() {
        let frac = ph as f64 / PHASES as f64;
        for (k, c) in row.iter_mut().enumerate() {
            let d = frac + HALF - 1.0 - k as f64;
            if d.abs() >= HALF {
                continue;
            }
            let windowed = if d == 0.0 {
                1.0
            } else {
                let s = (std::f64::consts::PI * d).sin() / (std::f64::consts::PI * d);
                s * (0.5 + 0.5 * (std::f64::consts::PI * d / HALF).cos())
            };
            *c = windowed as f32;
        }
    }
    table
}

pub struct SincResampler {
    in_rate: u32,
    out_rate: u32,
    nch: usize,
    kernel: Box<[[f32; TAPS]; PHASES]>,
    /// Input-sample position (global) of the next output sample.
    pos: f64,
    /// Global input-frame index of the first frame of the current chunk.
    base: f64,
    /// Last `TAPS` input frames of the previous chunk, for window continuity.
    ring: Vec<f32>,
    ring_frames: usize,
    outbuf: Vec<f32>,
    /// Step multiplier for drift compensation: `1.0` nominal, nudged by a
    /// few PPM by the soundcard clock-drift control loops (Part G2).
    step_mult: f64,
}

impl SincResampler {
    /// `delay` is accepted for compatibility with the removed symphonia API;
    /// the filter latency is TAPS/2 input frames.
    pub fn new(_delay: usize, in_rate: u32, out_rate: u32, nch: usize) -> Self {
        Self {
            in_rate,
            out_rate,
            nch,
            kernel: build_kernel(),
            pos: 0.0,
            base: 0.0,
            ring: vec![0.0; TAPS * nch.max(1)],
            ring_frames: 0,
            outbuf: Vec::new(),
            step_mult: 1.0,
        }
    }

    /// Nudge the conversion ratio by `ppm` parts per million of the input
    /// rate: positive yields fewer output samples per input sample (the
    /// soundcard bridges use it to absorb device-clock drift, Part G2).
    pub fn set_ppm(&mut self, ppm: f64) {
        self.step_mult = 1.0 + ppm / 1_000_000.0;
    }

    /// The current PPM nudge (for tests).
    pub fn ppm(&self) -> f64 {
        (self.step_mult - 1.0) * 1_000_000.0
    }

    /// Resample interleaved `input` (nch channels) to the output rate.
    /// The returned slice is valid until the next call.
    pub fn resample(&mut self, input: &[f32]) -> &[f32] {
        self.outbuf.clear();
        let nch = self.nch;
        let frames = input.len() / nch;
        if frames == 0 || nch == 0 {
            return &self.outbuf;
        }

        let ratio = self.in_rate as f64 / self.out_rate as f64;
        let base = self.base;
        let end = base + frames as f64;
        let ring_start = base - self.ring_frames as f64;

        while self.pos < end {
            let p = self.pos;
            let i0 = p.floor() as i64;
            let phase = ((p - i0 as f64) * PHASES as f64) as usize % PHASES;
            let row = &self.kernel[phase];

            for ch in 0..nch {
                let mut acc = 0.0f64;
                let mut norm = 0.0f64;
                for (k, &c) in row.iter().enumerate() {
                    let g = i0 - (TAPS / 2) as i64 + 1 + k as i64;
                    let v = if g < base as i64 - self.ring_frames as i64 {
                        continue;
                    } else if g < base as i64 {
                        self.ring[((g as f64 - ring_start) as usize) * nch + ch] as f64
                    } else if g < base as i64 + frames as i64 {
                        input[((g as f64 - base) as usize) * nch + ch] as f64
                    } else {
                        continue;
                    };
                    // Only taps that see real audio count toward the DC
                    // normalization, or chunk-edge outputs overshoot.
                    acc += v * c as f64;
                    norm += c as f64;
                }
                self.outbuf.push((acc / norm.max(1e-9)) as f32);
            }
            self.pos += ratio * self.step_mult;
        }

        // Keep the last TAPS frames for the next chunk's left-edge window.
        // Slice to `frames * nch` so a trailing partial frame (odd input
        // length) cannot overflow the ring copy.
        let keep = frames.min(TAPS);
        let start = (frames - keep) * nch;
        self.ring[..keep * nch].copy_from_slice(&input[start..frames * nch]);
        self.ring_frames = keep;
        self.base += frames as f64;

        &self.outbuf
    }

    /// The filter has no tail beyond the last emitted sample.
    pub fn flush(&mut self) -> &[f32] {
        self.outbuf.clear();
        &self.outbuf
    }

    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }

    pub fn out_rate(&self) -> u32 {
        self.out_rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_rate_passthrough_is_approximately_identity() {
        let mut r = SincResampler::new(0, 44100, 44100, 1);
        let out = r.resample(&[0.5, 0.25, 0.125]).to_vec();
        assert!((out[0] - 0.5).abs() < 1e-3);
        assert!((out[1] - 0.25).abs() < 1e-3);
        assert!((out[2] - 0.125).abs() < 1e-3);
    }

    #[test]
    fn ppm_nudge_shifts_output_length_proportionally() {
        // +1000 ppm must yield ~0.1% fewer output samples per input.
        let mut r = SincResampler::new(0, 44100, 44100, 1);
        r.set_ppm(1000.0);
        let input: Vec<f32> = (0..44100).map(|i| (i % 2) as f32).collect();
        let out = r.resample(&input);
        let expected = (44100.0 / 1.001) as usize;
        assert!(
            (out.len() as i64 - expected as i64).abs() <= 2,
            "ppm output {} vs expected {expected}",
            out.len()
        );
        // And a negative nudge yields proportionally more.
        let mut r = SincResampler::new(0, 44100, 44100, 1);
        r.set_ppm(-1000.0);
        let out = r.resample(&input);
        let expected = (44100.0 / 0.999) as usize;
        assert!(
            (out.len() as i64 - expected as i64).abs() <= 2,
            "negative ppm output {} vs expected {expected}",
            out.len()
        );
    }

    #[test]
    fn dc_passes_through_with_unity_gain() {
        let mut r = SincResampler::new(0, 44100, 48000, 2);
        let input = vec![1.0f32; 4410 * 2];
        let out = r.resample(&input);
        assert!(!out.is_empty());
        for s in out {
            assert!((s - 1.0).abs() < 1e-3, "dc gain drifted: {s}");
        }
    }

    #[test]
    fn upsample_half_rate_doubles_length() {
        let mut r = SincResampler::new(0, 22050, 44100, 1);
        let out = r.resample(&[0.0, 2.0]).to_vec();
        assert_eq!(out.len(), 4);
        assert!((out[0] - 0.0).abs() < 1e-3);
        assert!((out[2] - 2.0).abs() < 1e-3);
    }

    #[test]
    fn downsample_double_rate_halves_length() {
        let mut r = SincResampler::new(0, 44100, 22050, 1);
        let out = r.resample(&[0.0, 1.0, 2.0, 3.0]).to_vec();
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn sine_survives_44100_to_48000() {
        // A 1 kHz sine must stay a 1 kHz sine after bus -> Opus rate: zero
        // crossing rate of the output at 48 kHz is 2000/48000.
        let mut r = SincResampler::new(0, 44100, 48000, 1);
        let mut input = Vec::with_capacity(44100);
        for i in 0..44100 {
            let t = i as f64 / 44100.0;
            input.push((2.0 * std::f64::consts::PI * 1000.0 * t).sin() as f32);
        }
        let out = r.resample(&input).to_vec();
        // Skip the first 100 samples (filter lead-in), then count sign changes
        // over one full second of output.
        let crossings = out[100..]
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        let zcr = crossings as f64 / (out.len() - 100) as f64;
        assert!(
            (zcr - 2000.0 / 48000.0).abs() < 0.002,
            "zero crossing rate {zcr:.5} too far from 2000/48000"
        );
    }

    #[test]
    fn downsample_continues_across_many_chunks() {
        // Regression: 48 kHz -> 44.1 kHz (ratio > 1) in repeated 1152-frame
        // chunks — exactly how an MP3 is fed. Every chunk after the first used
        // to produce zero output, stalling the whole stream.
        let mut r = SincResampler::new(24, 48000, 44100, 2);
        let mut chunk = vec![0f32; 1152 * 2];
        for (i, s) in chunk.iter_mut().enumerate() {
            *s = (i % 7) as f32 / 7.0;
        }
        let mut total = 0usize;
        for _ in 0..24 {
            let out = r.resample(&chunk);
            assert!(
                out.len() >= 1000,
                "chunk produced only {} samples (expected ~2118)",
                out.len()
            );
            total += out.len();
        }
        // 24 chunks * 1152 frames * 2ch * (44100/48000) ~ 50781.
        assert!(total > 50_000, "total too small: {total}");
    }

    #[test]
    fn seamless_across_chunk_boundaries() {
        // A 1 kHz sine fed in two halves must not glitch at the seam.
        let mut r = SincResampler::new(0, 44100, 48000, 1);
        let mut joined = Vec::new();
        for half in 0..2 {
            let mut input = Vec::with_capacity(22050);
            for i in 0..22050 {
                let t = (half * 22050 + i) as f64 / 44100.0;
                input.push((2.0 * std::f64::consts::PI * 1000.0 * t).sin() as f32);
            }
            joined.extend(r.resample(&input).iter().copied());
        }
        // No sample should jump beyond the sine's natural amplitude range.
        let peak = joined.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak < 1.02, "overshoot at chunk seam: {peak}");
        assert!(peak > 0.98, "sine collapsed across chunks: {peak}");
    }

    #[test]
    fn ppm_production_math() {
        // Same-rate passthrough, negative PPM: the step shrinks, so MORE
        // output samples per input.
        let mut r = SincResampler::new(0, 44_100, 44_100, 2);
        r.set_ppm(-8000.0);
        let mut total_out = 0usize;
        for _ in 0..848 {
            total_out += r.resample(&[0.5f32; 52]).len();
        }
        // 848 * 26 frames * 2ch / 0.992 ~ 44444.
        assert!(total_out > 44_300, "expected ~44444, got {total_out}");
        // Same, but set_ppm every call (the drift loop's pattern).
        let mut r = SincResampler::new(0, 44_100, 44_100, 2);
        let mut total2 = 0usize;
        for _ in 0..848 {
            r.set_ppm(-8000.0);
            total2 += r.resample(&[0.5f32; 52]).len();
        }
        assert!(
            total2 > 44_300,
            "per-call set_ppm: expected ~44444, got {total2}"
        );
    }
}
