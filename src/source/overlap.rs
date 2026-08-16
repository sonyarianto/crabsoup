//! The `crossfade` operator: a ring-buffer overlap crossfade over the
//! consecutive tracks of any wrapped source.
//!
//! A crossfading playlist can only fade between tracks it preloads ahead of
//! the boundary, so a `rotate`'s children (plain sequential sources) cannot
//! fade internally — the preloaded fade would start seconds before the
//! scheduler sees the track change, and the child would freeze mid-fade
//! while the scheduler pulls another child. This wrapper fades *between*
//! whatever the child produces instead: a delay ring holds the current
//! track's tail, and when the child's label changes (the next track
//! starts), the tail is mixed with the new track's head over the fade
//! window.
//!
//! The output is a passthrough otherwise — the ring is written continuously
//! but only *read* during a fade — so the stream is never delayed, never
//! starts with silence, and never replays a track: the tail is heard only
//! while the next track fades in over it. The one approximation: a child
//! that changes label *mid-buffer* (a `queue` advancing inside a single
//! pull) starts the fade at the buffer start, briefly double-voicing the
//! old track's final frames.

use crate::source::AudioSource;

/// Sampled fade curve resolution, matching `engine::mixer`.
const CURVE_TABLE_SIZE: usize = 1024;

/// Overlap-crossfades the consecutive tracks of a wrapped source.
pub struct OverlapSource {
    child: Box<dyn AudioSource>,
    /// Zero fade = pure passthrough (the ring is never touched).
    fade_frames: usize,
    /// Delay ring, `fade_frames * channels` interleaved samples.
    ring: Vec<f32>,
    /// Next frame slot to write. While the ring is full of unread frames
    /// this is also the oldest frame, so a fade reads here too.
    write_pos: usize,
    /// Ring frames not yet consumed by a fade read.
    unread: usize,
    fading: bool,
    /// Frames into the current fade window.
    fade_pos: usize,
    /// `f(t) = t^curve` sampled at `CURVE_TABLE_SIZE + 1` points.
    curve_table: Vec<f32>,
    channels: usize,
    /// Reusable pull buffer, sized on demand so `next_buffer` never
    /// allocates on the hot path.
    scratch: Vec<f32>,
    /// True once the child has produced its first buffer.
    primed: bool,
    last_label: Option<String>,
}

impl OverlapSource {
    pub fn new(
        child: Box<dyn AudioSource>,
        duration_seconds: f64,
        curve: f64,
        sample_rate: u32,
        channels: usize,
    ) -> Self {
        let fade_frames = (duration_seconds.max(0.0) * sample_rate as f64).round() as usize;
        let cap = fade_frames.max(1);
        Self {
            child,
            fade_frames,
            ring: vec![0.0; cap * channels],
            write_pos: 0,
            unread: 0,
            fading: false,
            fade_pos: 0,
            curve_table: (0..=CURVE_TABLE_SIZE)
                .map(|k| {
                    let t = k as f64 / CURVE_TABLE_SIZE as f64;
                    t.powf(curve) as f32
                })
                .collect(),
            channels,
            scratch: Vec::new(),
            primed: false,
            last_label: None,
        }
    }

    /// Interpolated fade-curve value at `t` in `[0, 1]` (the mixer's table
    /// shape: the outgoing tail gets `1 - t`, the incoming track `t`).
    fn curve_gain(&self, t: f32) -> f32 {
        let pos = t * CURVE_TABLE_SIZE as f32;
        let i = pos as usize;
        if i >= CURVE_TABLE_SIZE {
            return 1.0;
        }
        let frac = pos - i as f32;
        let a = self.curve_table[i];
        let b = self.curve_table[i + 1];
        a + (b - a) * frac
    }

    /// Mix one pull: fade frames blend the ring tail with the fresh input
    /// (read from `self.scratch`), all other frames pass through, and every
    /// frame is written to the ring so the rolling tail stays current.
    fn mix_frames(&mut self, frames: usize, out: &mut [f32]) {
        let chans = self.channels;
        let cap = self.fade_frames;
        for f in 0..frames {
            let base = f * chans;
            if self.fading && self.fade_pos < cap {
                let t = self.fade_pos as f32 / cap as f32;
                let gb = self.curve_gain(t);
                let ga = self.curve_gain(1.0 - t);
                let tail = &self.ring[self.write_pos * chans..(self.write_pos + 1) * chans];
                for ch in 0..chans {
                    out[base + ch] = tail[ch] * ga + self.scratch[base + ch] * gb;
                }
                self.unread -= 1;
                self.fade_pos += 1;
            } else {
                self.fading = false;
                out[base..base + chans].copy_from_slice(&self.scratch[base..base + chans]);
            }
            let slot = self.write_pos * chans;
            for ch in 0..chans {
                self.ring[slot + ch] = self.scratch[base + ch];
            }
            self.write_pos = (self.write_pos + 1) % cap;
            if self.unread < cap {
                self.unread += 1;
            }
        }
    }

    /// Drain the unread tail after the child ends mid-fade, still ramping
    /// the fade down to silence. The drain never runs past the fade window
    /// — the rest of the ring would be silence anyway.
    fn drain(&mut self, out: &mut [f32]) -> usize {
        let chans = self.channels;
        let cap = self.fade_frames;
        let frames = (out.len() / chans)
            .min(self.unread)
            .min(cap.saturating_sub(self.fade_pos));
        for f in 0..frames {
            let base = f * chans;
            let t = (self.fade_pos as f32 / cap as f32).min(1.0);
            let ga = self.curve_gain(1.0 - t);
            let slot = self.write_pos * chans;
            for ch in 0..chans {
                out[base + ch] = self.ring[slot + ch] * ga;
            }
            self.write_pos = (self.write_pos + 1) % cap;
            self.unread -= 1;
            self.fade_pos += 1;
        }
        if self.unread == 0 || self.fade_pos >= cap {
            self.fading = false;
        }
        frames * chans
    }
}

impl AudioSource for OverlapSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        let wanted = buffer.len();
        if self.scratch.len() != wanted {
            self.scratch.resize(wanted, 0.0);
        }
        let n = self.child.next_buffer(&mut self.scratch);
        if n > 0 {
            if self.fade_frames == 0 {
                buffer[..n].copy_from_slice(&self.scratch[..n]);
                return n;
            }
            let label = self.child.label();
            if self.primed && label != self.last_label {
                log::info!("crossfade: blending into a new track");
                self.fading = true;
                self.fade_pos = 0;
            }
            self.last_label = label;
            self.primed = true;
            self.mix_frames(n / self.channels, buffer);
            return n;
        }
        if self.child.is_exhausted() && self.fading && self.unread > 0 {
            return self.drain(buffer);
        }
        0
    }

    fn is_exhausted(&self) -> bool {
        self.child.is_exhausted() && !(self.fading && self.unread > 0)
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

    fn skip(&mut self) {
        self.child.skip();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: usize = 100;
    const CHANS: usize = 2;

    struct FakeSource {
        value: f32,
        total_frames: usize,
        pos_frames: usize,
        label: String,
    }

    impl FakeSource {
        fn new(value: f32, total_frames: usize, label: &str) -> Self {
            Self {
                value,
                total_frames,
                pos_frames: 0,
                label: label.to_string(),
            }
        }
    }

    impl AudioSource for FakeSource {
        fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
            let want = buffer.len() / CHANS;
            let avail = self.total_frames.saturating_sub(self.pos_frames);
            let n_frames = avail.min(want);
            let n = n_frames * CHANS;
            buffer[..n].fill(self.value);
            self.pos_frames += n_frames;
            n
        }
        fn is_exhausted(&self) -> bool {
            self.pos_frames >= self.total_frames
        }
        fn label(&self) -> Option<String> {
            Some(self.label.clone())
        }
    }

    /// Two tracks in one source: `a` then `b`. The label flips exactly when
    /// `b`'s first audio arrives (like a plain file/playlist child).
    struct PairSource {
        a: FakeSource,
        b: FakeSource,
        on_b: bool,
    }

    impl PairSource {
        fn new(a_value: f32, a_frames: usize, b_value: f32, b_frames: usize) -> Self {
            Self {
                a: FakeSource::new(a_value, a_frames, "a"),
                b: FakeSource::new(b_value, b_frames, "b"),
                on_b: false,
            }
        }
    }

    impl AudioSource for PairSource {
        fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
            if self.on_b {
                return self.b.next_buffer(buffer);
            }
            if !self.a.is_exhausted() {
                return self.a.next_buffer(buffer);
            }
            self.on_b = true;
            self.b.next_buffer(buffer)
        }
        fn is_exhausted(&self) -> bool {
            self.a.is_exhausted() && self.b.is_exhausted()
        }
        fn label(&self) -> Option<String> {
            if self.on_b {
                self.b.label()
            } else {
                self.a.label()
            }
        }
    }

    fn wrap(source: Box<dyn AudioSource>, duration: f64) -> OverlapSource {
        OverlapSource::new(source, duration, 1.0, RATE as u32, CHANS)
    }

    /// Pull `src` for `frames` frames, returning the per-frame amplitude
    /// (sample 0 of the frame pair).
    fn pull(src: &mut dyn AudioSource, frames: usize, buf_frames: usize) -> Vec<f32> {
        let mut out = Vec::new();
        let mut buf = vec![0f32; buf_frames * CHANS];
        let mut got = 0;
        while got < frames {
            let n = src.next_buffer(&mut buf);
            if n == 0 {
                break;
            }
            let n_frames = n / CHANS;
            out.extend((0..n_frames).map(|f| buf[f * CHANS]));
            got += n_frames;
        }
        out
    }

    #[test]
    fn crossfade_blends_two_tracks_without_delay() {
        // Track A: 2.0s at 1.0, track B: 4.0s at 0.5, fade 0.2s (20 frames,
        // spanning output frames 200..219).
        let mut src = wrap(
            Box::new(PairSource::new(1.0, 200, 0.5, 400)),
            0.2,
        );
        let amps = pull(&mut src, 700, 10);

        // A plays untouched (no start delay).
        assert_eq!(amps[0], 1.0);
        assert_eq!(amps[199], 1.0);
        // The fade runs over frames 200..219: linear curve, so the midpoint
        // (t = 0.5) is 1.0 * 0.5 + 0.5 * 0.5.
        assert!((amps[210] - 0.75).abs() < 1e-6, "mid-fade was {}", amps[210]);
        assert!((amps[219] - 0.525).abs() < 1e-6, "fade end was {}", amps[219]);
        // B continues at its own level, again with no delay.
        assert_eq!(amps[220], 0.5);
        assert_eq!(amps[599], 0.5);
    }

    #[test]
    fn crossfade_never_replays_the_ring_after_exhaustion() {
        // B (10 frames) ends mid-fade: the unplayed tail drains with the
        // fade ramp (frames 210..219), and nothing of the ring is replayed
        // after that.
        let mut src = wrap(Box::new(PairSource::new(1.0, 200, 0.5, 10)), 0.2);
        let amps = pull(&mut src, 300, 10);
        assert_eq!(amps.len(), 220, "no tail replay expected");
        assert!((amps[219] - 0.05).abs() < 1e-6, "tail end was {}", amps[219]);
        assert!(src.is_exhausted());
    }

    #[test]
    fn crossfade_mid_fade_exhaustion_drains_the_tail() {
        // B is shorter still (5 frames): after its last frame the old
        // track's remaining tail (15 frames) drains, ramping to ~0.
        let mut src = wrap(Box::new(PairSource::new(1.0, 200, 0.5, 5)), 0.2);
        let amps = pull(&mut src, 300, 10);
        assert_eq!(amps.len(), 220);
        // Frame 204 is the last mix (4 frames in, t = 0.2), frame 205 the
        // first drain (tail only, t = 0.25): 1.0 * 0.75.
        assert!((amps[204] - 0.9).abs() < 1e-6, "last mix was {}", amps[204]);
        assert!((amps[205] - 0.75).abs() < 1e-6, "drain start was {}", amps[205]);
        assert!((amps[219] - 0.05).abs() < 1e-6, "tail end was {}", amps[219]);
        assert!(src.is_exhausted());
    }

    #[test]
    fn crossfade_zero_duration_is_a_passthrough() {
        let mut src = wrap(Box::new(PairSource::new(1.0, 50, 0.5, 50)), 0.0);
        let amps = pull(&mut src, 200, 10);
        assert_eq!(amps, vec![1.0f32; 50].into_iter().chain(vec![0.5; 50]).collect::<Vec<_>>());
        assert!(src.is_exhausted());
    }

    #[test]
    fn crossfade_curve_2_gives_equal_power_dip() {
        // curve = 2.0: g(t) = t^2, midpoint sum = 1*0.25 + 0.5*0.25... with
        // unequal levels the midpoint is 1.0*0.25 + 0.5*0.25 = 0.375.
        let mut src = OverlapSource::new(
            Box::new(PairSource::new(1.0, 200, 0.5, 400)),
            0.2,
            2.0,
            RATE as u32,
            CHANS,
        );
        let amps = pull(&mut src, 230, 10);
        assert!((amps[210] - 0.375).abs() < 1e-6, "mid-fade was {}", amps[210]);
    }
}