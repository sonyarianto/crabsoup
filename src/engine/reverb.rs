//! Convolution reverb (Part I3): IR-file convolution via uniformly
//! partitioned overlap-save FFT convolution (rustfft), inline in the pull
//! chain. All scratch is sized at construction — the hot path allocates
//! nothing.
//!
//! The IR is split into `P`-sample partitions and each partition's spectrum
//! is precomputed once. Per input block the FFT of the last `2P` samples is
//! ring-buffered, accumulated against every partition (overlap-save), and
//! inverse-FFT'd; the block's valid half is the output. The output position
//! `mP + i` is produced by the block containing input `mP + i`, so there is
//! no added latency beyond the `P`-sample block granularity.

use std::path::Path;
use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use symphonia::core::audio::{SampleBuffer, SignalSpec};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions, ReadOnlySource};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::engine::effects::Effect;
use crate::resample::SincResampler;

/// Partition sizes (power of two) considered for the IR length; the chosen
/// one balances FFT cost against per-partition multiply-adds.
const PARTITION_SIZES: [usize; 7] = [512, 1024, 2048, 4096, 8192, 16384, 32768];

/// Decode `path` to per-channel mono IRs at `sample_rate`: a mono file
/// yields one IR (applied to every output channel), a stereo file two.
/// Extra channels are dropped; a file not at the bus rate is resampled.
/// One-time cost at operator-call time, not on the hot path.
pub fn load_ir(path: &str, sample_rate: u32) -> crate::Result<Vec<Vec<f32>>> {
    let file = std::fs::File::open(path).map_err(|e| format!("reverb: {path}: {e}"))?;
    let mut hint = Hint::new();
    if let Some(ext) = Path::new(path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mss = MediaSourceStream::new(
        Box::new(ReadOnlySource::new(file)),
        MediaSourceStreamOptions::default(),
    );
    let mut probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("reverb: {path}: cannot probe: {e}"))?;
    let track = probed
        .format
        .default_track()
        .cloned()
        .ok_or("reverb: no audio track")?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("reverb: {path}: {e}"))?;

    let mut interleaved: Vec<f32> = Vec::new();
    let mut file_spec: Option<SignalSpec> = None;
    while let Ok(packet) = probed.format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(e) => {
                log::warn!("reverb: {path}: skipping packet: {e}");
                continue;
            }
        };
        let spec = *decoded.spec();
        let frames = decoded.frames();
        if frames == 0 {
            continue;
        }
        let mut sample_buf = SampleBuffer::<f32>::new(frames as u64, spec);
        sample_buf.copy_interleaved_ref(decoded);
        interleaved.extend_from_slice(sample_buf.samples());
        if file_spec.is_none() {
            file_spec = Some(spec);
        }
    }
    let spec = file_spec.ok_or_else(|| format!("reverb: {path}: no audio decoded"))?;

    let file_channels = spec.channels.count().max(1);
    let mut out: Vec<Vec<f32>> = Vec::new();
    for c in 0..file_channels.min(2) {
        let mut channel: Vec<f32> = interleaved
            .iter()
            .skip(c)
            .step_by(file_channels)
            .copied()
            .collect();
        if spec.rate != sample_rate {
            let mut rs = SincResampler::new(0, spec.rate, sample_rate, 1);
            channel = rs.resample(&channel).to_vec();
        }
        out.push(channel);
    }
    if out.iter().all(|c| c.is_empty()) {
        return Err(format!("reverb: {path}: empty impulse response").into());
    }
    Ok(out)
}

/// Pick the partition size: the largest power-of-two candidate that divides
/// `frames_per_buffer` (so full buffers are whole blocks and nothing is
/// dropped mid-buffer), falling back to 2048.
fn pick_partition(frames_per_buffer: usize) -> usize {
    let fpb = frames_per_buffer.max(1);
    PARTITION_SIZES
        .iter()
        .copied()
        .rev()
        .find(|&p| fpb >= p && fpb.is_multiple_of(p))
        .unwrap_or(2048)
}

/// A partitioned-overlap-save convolution of the input with `ir` per
/// channel, mixed `wet × conv + dry × in`.
pub struct ConvReverb {
    channels: usize,
    partition: usize,
    n_fft: usize,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    fft_scratch: Vec<Complex<f32>>,
    ifft_scratch: Vec<Complex<f32>>,
    wet: f32,
    dry: f32,
    /// `[channel][partition][bin]` — the IR's precomputed spectra.
    kernels: Vec<Vec<Vec<Complex<f32>>>>,
    state: Vec<ChannelState>,
    /// Copy of the incoming interleaved buffer, for the dry mix.
    orig: Vec<f32>,
    /// Per-channel deinterleaved input scratch for one `process` call.
    in_ch: Vec<Vec<f32>>,
    /// Channels with a block ready to convolve, in per-channel order.
    pending: Vec<usize>,
}

struct ChannelState {
    /// The last `2P` input samples (the window ending at the current block).
    history: Vec<f32>,
    /// Ring of FFT'd windows; entry `(m - a) mod K` holds the window from
    /// `a` blocks ago.
    ring: Vec<Vec<Complex<f32>>>,
    /// Spectrum accumulator for the current block.
    acc: Vec<Complex<f32>>,
    /// Input samples left over from the previous call (fewer than `P`).
    carry: Vec<f32>,
    /// The current block's input (`P` samples), staged out of `carry`.
    block_buf: Vec<f32>,
    /// Current block index `m`.
    block: usize,
    /// In-place FFT buffer (reused: real input in, spectrum out).
    fft_in: Vec<Complex<f32>>,
    /// Completed convolution output, served as samples are pulled.
    out_ring: Vec<f32>,
    out_head: usize,
}

impl ConvReverb {
    /// `ir` is one mono IR per channel (at the bus rate); a mono IR is
    /// applied to every channel. `frames_per_buffer` sizes the partition.
    pub fn new(
        ir: &[Vec<f32>],
        wet: f32,
        dry: f32,
        channels: usize,
        frames_per_buffer: usize,
    ) -> Self {
        let partition = pick_partition(frames_per_buffer);
        let n_fft = 2 * partition;
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(n_fft);
        let ifft = planner.plan_fft_inverse(n_fft);
        let mut fft_scratch = vec![Complex::new(0.0, 0.0); fft.get_inplace_scratch_len()];
        let ifft_scratch = vec![Complex::new(0.0, 0.0); ifft.get_inplace_scratch_len()];

        let mut kernels = Vec::with_capacity(channels);
        let mut kernel_buf = vec![Complex::new(0.0, 0.0); n_fft];
        for ch in 0..channels {
            let irc: &[f32] = match ir.len() {
                0 => &[],
                _ => &ir[ch.min(ir.len() - 1)],
            };
            let k = irc.len().div_ceil(partition).max(1);
            let mut chk = Vec::with_capacity(k);
            for part in 0..k {
                kernel_buf.fill(Complex::new(0.0, 0.0));
                for (i, slot) in kernel_buf[..partition].iter_mut().enumerate() {
                    let src = part * partition + i;
                    if src < irc.len() {
                        *slot = Complex::new(irc[src], 0.0);
                    }
                }
                fft.process_with_scratch(&mut kernel_buf, &mut fft_scratch);
                chk.push(kernel_buf.clone());
            }
            kernels.push(chk);
        }

        let k = kernels.first().map(|c| c.len()).unwrap_or(1);
        let state = (0..channels)
            .map(|_| ChannelState {
                history: vec![0.0; n_fft],
                ring: (0..k)
                    .map(|_| vec![Complex::new(0.0, 0.0); n_fft])
                    .collect(),
                acc: vec![Complex::new(0.0, 0.0); n_fft],
                carry: Vec::new(),
                block_buf: vec![0.0; partition],
                block: 0,
                fft_in: vec![Complex::new(0.0, 0.0); n_fft],
                out_ring: Vec::new(),
                out_head: 0,
            })
            .collect();

        Self {
            channels,
            partition,
            n_fft,
            fft,
            ifft,
            fft_scratch,
            ifft_scratch,
            wet,
            dry,
            kernels,
            state,
            orig: Vec::new(),
            in_ch: vec![Vec::new(); channels],
            pending: Vec::new(),
        }
    }

    /// Convolve one `P`-sample block for channel `c` and append its output
    /// to the channel's `out_ring`.
    fn process_block(&mut self, c: usize) {
        let st = &mut self.state[c];
        let p = self.partition;
        let n = self.n_fft;
        // Slide the window: x[(m-1)P .. (m+1)P] becomes x[mP .. (m+2)P] with
        // the new block in the second half.
        st.history.copy_within(p..n, 0);
        st.history[p..n].copy_from_slice(&st.block_buf);
        for (i, &s) in st.history.iter().enumerate() {
            st.fft_in[i] = Complex::new(s, 0.0);
        }
        self.fft
            .process_with_scratch(&mut st.fft_in, &mut self.fft_scratch);
        let k = self.kernels[c].len();
        let slot = st.block % k;
        std::mem::swap(&mut st.ring[slot], &mut st.fft_in);
        st.acc.fill(Complex::new(0.0, 0.0));
        for a in 0..k {
            let x = &st.ring[(st.block + k - a) % k];
            let h = &self.kernels[c][a];
            for j in 0..n {
                st.acc[j] += x[j] * h[j];
            }
        }
        self.ifft
            .process_with_scratch(&mut st.acc, &mut self.ifft_scratch);
        // Overlap-save: the last P samples are free of circular aliasing.
        let inv = 1.0 / n as f32;
        for i in 0..p {
            st.out_ring.push(st.acc[p + i].re * inv);
        }
        st.block += 1;
    }
}

impl Effect for ConvReverb {
    fn process(&mut self, buf: &mut [f32], channels: usize) {
        debug_assert_eq!(channels, self.channels);
        let ch = self.channels;
        let frames = buf.len() / ch;
        if frames == 0 || ch == 0 {
            return;
        }
        self.orig.resize(buf.len(), 0.0);
        self.orig.copy_from_slice(buf);
        for c in 0..ch {
            let v = &mut self.in_ch[c];
            v.clear();
            for f in 0..frames {
                v.push(buf[f * ch + c]);
            }
            let st = &mut self.state[c];
            st.carry.append(v);
            while st.carry.len() >= self.partition {
                st.block_buf.copy_from_slice(&st.carry[..self.partition]);
                st.carry.drain(..self.partition);
                self.pending.push(c);
            }
        }
        let pending = std::mem::take(&mut self.pending);
        for &c in &pending {
            self.process_block(c);
        }
        self.pending = pending;
        self.pending.clear();
        // Mix: dry input + wet convolution, serving `out_ring` in order
        // (output position j corresponds to input position j, so the ring
        // always holds the samples for this call when frames >= partition).
        for f in 0..frames {
            for c in 0..ch {
                let st = &mut self.state[c];
                let conv = if st.out_head < st.out_ring.len() {
                    let v = st.out_ring[st.out_head];
                    st.out_head += 1;
                    v
                } else {
                    0.0
                };
                buf[f * ch + c] = self.dry * self.orig[f * ch + c] + self.wet * conv;
            }
        }
        for c in 0..ch {
            let st = &mut self.state[c];
            if st.out_head > 0 && st.out_head * 2 >= st.out_ring.len().max(1) {
                st.out_ring.drain(..st.out_head);
                st.out_head = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::effects::EffectSource;
    use crate::source::AudioSource;

    /// Run `fx` over `input` mono in `fpb`-sized calls and collect the
    /// output (the `EffectSource` pull chain is not needed; call `process`
    /// directly).
    fn run_conv(ir: &[f32], wet: f32, dry: f32, input: &[f32], fpb: usize) -> Vec<f32> {
        let mut fx = ConvReverb::new(&[ir.to_vec()], wet, dry, 1, fpb);
        let mut out = Vec::new();
        let mut buf = vec![0.0f32; fpb];
        for chunk in input.chunks(fpb) {
            buf[..chunk.len()].copy_from_slice(chunk);
            fx.process(&mut buf[..chunk.len()], 1);
            out.extend_from_slice(&buf[..chunk.len()]);
        }
        out
    }

    fn sine(rate: u32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| ((i as f32) * 2.0 * std::f32::consts::PI * 440.0 / rate as f32).sin())
            .collect()
    }

    #[test]
    fn delta_ir_is_exact_passthrough() {
        let input = sine(44_100, 2048);
        let out = run_conv(&[1.0], 1.0, 0.0, &input, 1024);
        assert_eq!(out.len(), input.len());
        for (i, (o, r)) in out.iter().zip(&input).enumerate() {
            assert!((o - r).abs() < 1e-3, "sample {i}: {o} vs {r}");
        }
    }

    #[test]
    fn delta_ir_at_offset_delays() {
        let d = 100usize;
        let mut ir = vec![0.0f32; d];
        ir.push(1.0);
        let input = sine(44_100, 2048);
        let out = run_conv(&ir, 1.0, 0.0, &input, 1024);
        for (i, o) in out.iter().take(d).enumerate() {
            assert!(o.abs() < 1e-3, "pre-delay sample {i}: {o}");
        }
        for i in d..input.len() {
            assert!(
                (out[i] - input[i - d]).abs() < 1e-3,
                "sample {i}: {} vs {}",
                out[i],
                input[i - d]
            );
        }
    }

    #[test]
    fn delta_across_partition_boundaries() {
        // A delta at the edges of a partition (tail of P-1, head of P,
        // tail of 2P-1, head of 2P) must land on the exact sample no matter
        // which partition supplies it.
        for d in [1023usize, 1024, 2047, 2048] {
            let mut ir = vec![0.0f32; d];
            ir.push(1.0);
            let input = sine(44_100, 3072);
            let out = run_conv(&ir, 1.0, 0.0, &input, 1024);
            let mut worst = (0usize, 0f32);
            for n in d..input.len() {
                let err = (out[n] - input[n - d]).abs();
                if err > worst.1 {
                    worst = (n, err);
                }
            }
            assert!(worst.1 < 1e-3, "delta@{d}: worst {worst:?}");
        }
    }

    #[test]
    fn multi_partition_matches_direct_convolution() {
        // IR longer than the 1024-sample partition exercises the ring.
        let ir: Vec<f32> = (0..2500).map(|i| 0.5 * (i as f32 * 0.001).sin()).collect();
        let input = sine(44_100, 4096);
        let out = run_conv(&ir, 1.0, 0.0, &input, 1024);
        let mut worst = (0usize, 0f32);
        for n in 0..input.len() {
            let mut acc = 0.0;
            for (i, &h) in ir.iter().enumerate() {
                if n >= i {
                    acc += h * input[n - i];
                }
            }
            let d = (out[n] - acc).abs();
            if d > worst.1 {
                worst = (n, d);
            }
            assert!(
                d < 5e-3 * (1.0 + acc.abs()),
                "sample {n}: {} vs {acc} (worst {worst:?})",
                out[n]
            );
        }
    }

    #[test]
    fn wet_dry_mix_is_linear() {
        let input = sine(44_100, 1024);
        let both = run_conv(&[1.0], 1.0, 1.0, &input, 1024);
        for (i, &v) in input.iter().enumerate() {
            assert!((both[i] - 2.0 * v).abs() < 1e-3, "sample {i}");
        }
        let half = run_conv(&[1.0], 0.5, 0.5, &input, 1024);
        for (i, &v) in input.iter().enumerate() {
            assert!((half[i] - v).abs() < 1e-3, "sample {i}");
        }
    }

    #[test]
    fn stereo_channels_are_independent() {
        // Left: delta at 0 (passthrough); right: delta at P/2 (delay).
        let p = 1024;
        let mut ir = vec![vec![1.0f32]];
        let mut right = vec![0.0f32; p / 2];
        right.push(1.0);
        ir.push(right);
        let mono = sine(44_100, 1024);
        let input: Vec<f32> = mono.iter().flat_map(|&s| [s, s]).collect();
        let mut fx = ConvReverb::new(&ir, 1.0, 0.0, 2, 1024);
        let mut buf = input.clone();
        fx.process(&mut buf, 2);
        for (i, frame) in mono.iter().enumerate() {
            assert!((buf[i * 2] - frame).abs() < 1e-3, "L sample {i}");
            let want = if i >= p / 2 { mono[i - p / 2] } else { 0.0 };
            assert!((buf[i * 2 + 1] - want).abs() < 1e-3, "R sample {i}");
        }
    }

    #[test]
    fn effect_source_chain_runs_alloc_free_path() {
        struct Fake {
            value: f32,
        }
        impl AudioSource for Fake {
            fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
                buffer.fill(self.value);
                buffer.len()
            }
            fn is_exhausted(&self) -> bool {
                false
            }
        }
        let child: Box<dyn AudioSource> = Box::new(Fake { value: 0.1 });
        let mut chain =
            EffectSource::new(child, ConvReverb::new(&[vec![1.0]], 0.5, 0.5, 2, 4096), 2);
        let mut buf = vec![0.0f32; 8192];
        let n = chain.next_buffer(&mut buf);
        assert_eq!(n, 8192);
        assert!(buf.iter().all(|&s| (s - 0.1).abs() < 1e-3));
    }
}
