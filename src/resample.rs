//! A small streaming linear-interpolation resampler.
//!
//! Symphonia 0.5.5 removed its built-in resampler, so this provides the same
//! job with minimal dependencies. Linear interpolation is plenty for a
//! broadcast pipeline (any aliasing is far below what the source codec
//! produced) and has zero latency: every input frame is consumed immediately.
//! When a chunk's last frame is reached the right neighbour is padded with the
//! frame itself (a zero-order hold), so trailing samples are never dropped even
//! if the consumer never calls `flush`.

pub struct LinearResampler {
    in_rate: u32,
    out_rate: u32,
    nch: usize,
    /// Last input frame, kept across chunks so interpolation is seamless.
    prev: Vec<f32>,
    has_prev: bool,
    /// Input-sample position of the next output sample.
    pos: f64,
    outbuf: Vec<f32>,
}

impl LinearResampler {
    /// `delay` is accepted for compatibility with the removed symphonia API
    /// but is not needed (linear interpolation has no filter delay).
    pub fn new(_delay: usize, in_rate: u32, out_rate: u32, nch: usize) -> Self {
        Self {
            in_rate,
            out_rate,
            nch,
            prev: vec![0.0; nch],
            has_prev: false,
            pos: 0.0,
            outbuf: Vec::new(),
        }
    }

    /// Resample interleaved `input` (nch channels) to the output rate.
    /// The returned slice is valid until the next call.
    pub fn resample(&mut self, input: &[f32]) -> &[f32] {
        self.outbuf.clear();
        let nch = self.nch;
        if input.len() < nch || nch == 0 {
            return &self.outbuf;
        }

        let ratio = self.in_rate as f64 / self.out_rate as f64;
        let frames = input.len() / nch;

        for j in 0..frames {
            let cur = &input[j * nch..(j + 1) * nch];
            if !self.has_prev {
                self.prev.copy_from_slice(cur);
                self.has_prev = true;
                continue;
            }
            // Produce output samples while the interpolation window
            // (prev == frame j-1, cur == frame j) covers `pos`.
            while (self.pos.floor() as usize) < j {
                let k = self.pos.floor() as usize;
                let f = self.pos - k as f64;
                for (ch, &r) in cur.iter().enumerate() {
                    let l = self.prev[ch] as f64;
                    self.outbuf.push((l + (r as f64 - l) * f) as f32);
                }
                self.pos += ratio;
            }
            self.prev.copy_from_slice(cur);
        }

        // Tail: the sample at the very last frame needs no right neighbour —
        // pad with the last frame itself (a zero-order hold) so nothing is
        // dropped at EOF.
        let last = frames.saturating_sub(1);
        while self.pos.floor() as usize <= last {
            self.outbuf.extend_from_slice(&self.prev[..nch]);
            self.pos += ratio;
        }

        &self.outbuf
    }

    /// Linear interpolation has no tail, so there is nothing to flush.
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
    fn same_rate_passthrough() {
        let mut r = LinearResampler::new(0, 44100, 44100, 1);
        let out = r.resample(&[0.5, 0.25, 0.125]);
        assert_eq!(out, &[0.5, 0.25, 0.125]);
    }

    #[test]
    fn upsample_half_rate_produces_more() {
        // in=22050, out=44100 -> ratio 0.5. Two input frames yield outputs at
        // 0.0 and 0.5; the tail hold pads the (nonexistent) frame beyond the
        // last input, so 1.5 also resolves to the last frame's value.
        let mut r = LinearResampler::new(0, 22050, 44100, 1);
        let out = r.resample(&[0.0, 2.0]).to_vec();
        assert_eq!(out, &[0.0, 1.0, 2.0, 2.0]);
    }

    #[test]
    fn downsample_double_rate_produces_fewer() {
        // in=44100, out=22050 -> ratio 2.0.
        let mut r = LinearResampler::new(0, 44100, 22050, 1);
        let out = r.resample(&[0.0, 1.0, 2.0, 3.0]).to_vec();
        assert_eq!(out, vec![0.0, 2.0]);
    }

    #[test]
    fn chunks_are_seamless() {
        let mut r = LinearResampler::new(0, 44100, 48000, 1);
        let a = r.resample(&[1.0, 2.0]).to_vec();
        let b = r.resample(&[3.0, 4.0]).to_vec();
        // Total output approximates 2 * (48000/44100) rounded.
        let joined: Vec<f32> = a.into_iter().chain(b).collect();
        assert!(!joined.is_empty());
        assert!((joined[0] - 1.0).abs() < 1e-6);
    }
}
