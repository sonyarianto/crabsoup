//! The `crossfade` operator: a delayed-ring fade over the consecutive
//! tracks of any wrapped source.
//!
//! A crossfading playlist can only fade between tracks it preloads ahead of
//! the boundary, so a `rotate`'s children (plain sequential sources) cannot
//! fade internally — the preloaded fade would start seconds before the
//! scheduler sees the track change, and the child would freeze mid-fade
//! while the scheduler pulls another child. This wrapper fades *between*
//! whatever the child produces instead, with the stream held in a delay
//! ring: the output trails the input by the fade window, so when the
//! child's label changes (the next track starts) the outgoing track's
//! final seconds are *not yet heard* — the fade plays them out exactly
//! once, ramped down.
//!
//! The fade geometry is why nothing repeats and nothing goes silent: the
//! ring's tail is scanned for its last audible frame and the fade-out spans
//! only that audible part (floored at `MIN_FADE_SECONDS`), then the
//! remaining unplayed ring frames — the outgoing track's trailing silence,
//! which real songs and jingles end in — are skipped outright, and the
//! incoming track ramps in. Each track's ending is therefore heard once
//! (fading), never replayed after a gap, and the next track reaches full
//! level exactly when the previous one stops being audible.
//!
//! The price is a constant delay of the configured fade duration: the first
//! `fade_frames` output frames are silence while the ring fills. A
//! `rotate`/`switch` handover is sample-exact, so every label change the
//! wrapper sees is a real track change with a clean head/tail split.

use crate::source::AudioSource;

/// Sampled fade curve resolution, matching `engine::mixer`.
const CURVE_TABLE_SIZE: usize = 1024;

/// RMS window and hop for the fade-point scan.
const RMS_WINDOW_SECONDS: f64 = 0.05;
const RMS_HOP_SECONDS: f64 = 0.01;

/// BS.1770/EBU R128 loudness gates: windows whose mean-square is below
/// −70 dBFS are absolute silence, and a window counts as audible only if
/// it is also within `GATE_RELATIVE_LU` LU of the mean of the windows
/// that pass the absolute gate — the track's own loudness defines its
/// audibility floor, so quiet tracks are handled as well as loud ones.
const GATE_ABSOLUTE_DB: f32 = -70.0;
const GATE_RELATIVE_LU: f32 = 10.0;

/// Shortest allowed fade-out window in seconds: below this a cut would
/// click, above it a fade over near-silence just delays the next track.
const MIN_FADE_SECONDS: f64 = 0.15;

/// Fades between the consecutive tracks of a wrapped source.
pub struct OverlapSource {
    child: Box<dyn AudioSource>,
    /// Zero fade = pure passthrough (no ring, no delay).
    fade_frames: usize,
    /// Delay ring, `fade_frames * channels` interleaved samples.
    ring: Vec<f32>,
    /// Next input frame slot to write.
    write_pos: usize,
    /// Frames written but not yet output (the current delay). The output
    /// reads the oldest unread frame, `(write_pos - filled) mod cap`.
    filled: usize,
    /// True until the ring first fills (startup: the output is silence).
    filling: bool,
    /// True while the outgoing tail is being ramped down.
    fading: bool,
    /// Frames into the current fade-out.
    fade_pos: usize,
    /// Fade-out length: the ring's audible tail, floored and capped.
    fade_window: usize,
    /// `write_pos` when the fade started — the incoming track's first
    /// frame; the output jumps here to skip the trailing silence.
    fade_write_start: usize,
    /// Ramp length for the incoming track's head (10% of the fade).
    fade_in_frames: usize,
    /// Frames into the incoming ramp.
    fade_in_pos: usize,
    /// True only while the incoming track's head is ramping up (set at a
    /// fade-out jump — never during startup).
    fade_in_active: bool,
    /// True when the last fade-out jump happened inside `drain`: the held
    /// frames are the incoming head, so they pass at full level instead of
    /// a fresh end-of-stream ramp.
    drain_after_jump: bool,
    /// `f(t) = t^curve` sampled at `CURVE_TABLE_SIZE + 1` points.
    curve_table: Vec<f32>,
    channels: usize,
    /// Reusable pull buffer, sized on demand so `next_buffer` never
    /// allocates on the hot path.
    scratch: Vec<f32>,
    /// Per-hop mean-square sums and per-window energies for the fade-point
    /// gate scan, sized on demand like `scratch`.
    hop_energy: Vec<f32>,
    block_energy: Vec<f32>,
    /// True once the child has produced its first buffer.
    primed: bool,
    last_label: Option<String>,
    /// Frames of the current track consumed so far (reset on label change).
    track_frames: usize,
    sample_rate: u32,
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
            filled: 0,
            filling: true,
            fading: false,
            fade_pos: 0,
            fade_window: 0,
            fade_write_start: 0,
            fade_in_frames: (fade_frames / 10).max(1),
            fade_in_pos: 0,
            fade_in_active: false,
            drain_after_jump: false,
            curve_table: (0..=CURVE_TABLE_SIZE)
                .map(|k| {
                    let t = k as f64 / CURVE_TABLE_SIZE as f64;
                    t.powf(curve) as f32
                })
                .collect(),
            channels,
            scratch: Vec::new(),
            hop_energy: Vec::new(),
            block_energy: Vec::new(),
            primed: false,
            last_label: None,
            track_frames: 0,
            sample_rate,
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

    /// Begin a fade-out of the outgoing tail. The window spans the ring's
    /// audible part at most — floored at `MIN_FADE_SECONDS`, capped by the
    /// configured fade, the outgoing track's last quarter, and the frames
    /// actually held — so the fade never drags over trailing silence and
    /// the rest of the ring (silence) is skipped at the fade-out's end.
    fn start_fade(&mut self) {
        let cap = self.fade_frames;
        let span = self.filled.min(self.track_frames);
        let last_audible = self.gated_last_audible(span);
        let win_cap = cap.min(self.track_frames.max(1) / 4).max(1);
        let min_win = (MIN_FADE_SECONDS * self.sample_rate as f64).round() as usize;
        let window = last_audible
            .map_or(min_win, |i| i + 1)
            .max(min_win)
            .min(win_cap)
            .min(self.filled)
            .max(1);
        log::info!(
            "crossfade: fading out {:.2}s of tail, next track fades in",
            window as f64 / self.sample_rate as f64
        );
        self.fading = true;
        self.fade_pos = 0;
        self.fade_window = window;
        self.fade_write_start = self.write_pos;
        self.filling = false;
        self.track_frames = 0;
    }

    /// Index of the last audible frame in the ring's unread span, found
    /// with BS.1770-style gating instead of a bare amplitude floor, so a
    /// track's own loudness — not a fixed −66 dBFS constant — decides what
    /// is silence: short-time mean-square windows (50 ms, 10 ms hop) are
    /// gated at −70 dBFS, then again 10 LU below the mean of the windows
    /// that passed. The last passing window is refined sample-by-sample
    /// against that gate level. `None` means nothing passed: the fade falls
    /// back to its floor.
    fn gated_last_audible(&mut self, span: usize) -> Option<usize> {
        if span == 0 {
            return None;
        }
        let chans = self.channels;
        let cap = self.fade_frames;
        let rate = self.sample_rate as f64;
        let hop = ((RMS_HOP_SECONDS * rate).round() as usize).max(1);
        let win = ((RMS_WINDOW_SECONDS * rate).round() as usize).max(hop);
        let hops_per_win = (win / hop).max(1);
        let nwin = span.div_ceil(hop);
        if self.hop_energy.len() != nwin {
            self.hop_energy.resize(nwin, 0.0);
            self.block_energy.resize(nwin, 0.0);
        }
        let abs_floor = 10f32.powf(GATE_ABSOLUTE_DB / 10.0);
        let mut sliding = 0.0f32;
        let mut gated_mean_sum = 0.0f32;
        let mut gated_count = 0usize;
        for k in 0..nwin {
            let start = k * hop;
            let end = ((k + 1) * hop).min(span);
            let mut sum = 0.0f32;
            for i in start..end {
                let base = ((self.write_pos + cap - self.filled + i) % cap) * chans;
                let s0 = self.ring[base];
                let s1 = self.ring[base + 1];
                sum += s0 * s0 + s1 * s1;
            }
            self.hop_energy[k] = sum;
            sliding += sum;
            if k >= hops_per_win {
                sliding -= self.hop_energy[k - hops_per_win];
            }
            let win_start = (k + 1).saturating_sub(hops_per_win) * hop;
            let win_end = ((k + 1) * hop).min(span);
            let energy = sliding / (2.0 * (win_end - win_start) as f32);
            self.block_energy[k] = energy;
            if energy > abs_floor {
                gated_mean_sum += energy;
                gated_count += 1;
            }
        }
        if gated_count == 0 {
            return None;
        }
        let rel_floor = gated_mean_sum / gated_count as f32 / 10f32.powf(GATE_RELATIVE_LU / 10.0);
        let gate = rel_floor.max(abs_floor);
        let sample_floor = gate.sqrt();
        for k in (0..nwin).rev() {
            if self.block_energy[k] <= gate {
                continue;
            }
            let win_start = (k + 1).saturating_sub(hops_per_win) * hop;
            let win_end = ((k + 1) * hop).min(span);
            for i in (win_start..win_end).rev() {
                let base = ((self.write_pos + cap - self.filled + i) % cap) * chans;
                if self.ring[base..base + chans]
                    .iter()
                    .any(|s| s.abs() >= sample_floor)
                {
                    return Some(i);
                }
            }
            return Some(win_end - 1);
        }
        None
    }

    /// Mix one pull. Each input frame is written to the ring; each output
    /// frame reads the oldest unread ring frame, so the stream is delayed
    /// by the ring fill. While filling (startup) the output is silence.
    /// During a fade-out the oldest unread frames are the outgoing tail,
    /// heard here for the first and only time, ramped down; at the end the
    /// read jumps to the incoming track's first frame (skipping the
    /// unplayed trailing silence) and the head ramps in.
    fn mix_frames(&mut self, frames: usize, out: &mut [f32]) {
        let chans = self.channels;
        let cap = self.fade_frames;
        for f in 0..frames {
            let base = f * chans;
            let pre_filled = self.filled;
            if self.filling {
                let slot = self.write_pos * chans;
                for ch in 0..chans {
                    self.ring[slot + ch] = self.scratch[base + ch];
                }
                self.write_pos = (self.write_pos + 1) % cap;
                self.filled = (self.filled + 1).min(cap);
                out[base..base + chans].fill(0.0);
                if self.filled == cap {
                    self.filling = false;
                }
                continue;
            }
            let gain = if self.fading {
                let t = self.fade_pos as f32 / self.fade_window as f32;
                self.fade_pos += 1;
                if self.fade_pos >= self.fade_window {
                    self.fading = false;
                    self.filled = self.fade_window;
                    self.fade_in_active = true;
                    self.fade_in_pos = 0;
                    self.drain_after_jump = false;
                }
                self.curve_gain(1.0 - t)
            } else if self.fade_in_active {
                let g = self.curve_gain(self.fade_in_pos as f32 / self.fade_in_frames as f32);
                self.fade_in_pos += 1;
                if self.fade_in_pos >= self.fade_in_frames {
                    self.fade_in_active = false;
                }
                g
            } else {
                1.0
            };
            let read_pos = (self.write_pos + cap - pre_filled) % cap;
            let slot = read_pos * chans;
            for ch in 0..chans {
                out[base + ch] = self.ring[slot + ch] * gain;
            }
            self.filled -= 1;
            let slot = self.write_pos * chans;
            for ch in 0..chans {
                self.ring[slot + ch] = self.scratch[base + ch];
            }
            self.write_pos = (self.write_pos + 1) % cap;
            if self.filled < cap {
                self.filled += 1;
            }
        }
    }

    /// Drain the held ring after the child ends, ramping the remaining
    /// frames down to silence. Mid-fade this continues the fade-out curve;
    /// otherwise a fresh ramp starts over what is left. After a fade-out
    /// jump the held frames are the incoming track's head, so they ramp in
    /// instead.
    fn drain(&mut self, out: &mut [f32]) -> usize {
        let chans = self.channels;
        if self.drain_after_jump {
            if self.fade_in_active {
            let cap = self.fade_frames;
            let frames = (out.len() / chans)
                .min(self.filled)
                .min(self.fade_in_frames.saturating_sub(self.fade_in_pos));
            for f in 0..frames {
                let base = f * chans;
                let g = self.curve_gain(self.fade_in_pos as f32 / self.fade_in_frames as f32);
                let read_pos = (self.write_pos + cap - self.filled) % cap;
                for ch in 0..chans {
                    out[base + ch] = self.ring[read_pos * chans + ch] * g;
                }
                self.fade_in_pos += 1;
            }
                self.filled -= frames;
                if self.fade_in_pos >= self.fade_in_frames || self.filled == 0 {
                    self.fade_in_active = false;
                }
                return frames * chans;
            }
            let frames = (out.len() / chans).min(self.filled);
            for f in 0..frames {
                let base = f * chans;
                let read_pos = (self.write_pos + self.fade_frames - self.filled) % self.fade_frames;
                for ch in 0..chans {
                    out[base + ch] = self.ring[read_pos * chans + ch];
                }
                self.filled -= 1;
            }
            if self.filled == 0 {
                self.drain_after_jump = false;
            }
            return frames * chans;
        }
        if self.filled == 0 {
            return 0;
        }
        if !self.fading {
            self.fading = true;
            self.fade_pos = 0;
            self.fade_window = self.filled;
            self.fade_write_start = self.write_pos;
        }
        let chans = self.channels;
        let cap = self.fade_frames;
        let frames = (out.len() / chans)
            .min(self.filled)
            .min(self.fade_window.saturating_sub(self.fade_pos));
        for f in 0..frames {
            let base = f * chans;
            let t = self.fade_pos as f32 / self.fade_window as f32;
            let ga = self.curve_gain(1.0 - t);
            let read_pos = (self.write_pos + cap - self.filled) % cap;
            let slot = read_pos * chans;
            for ch in 0..chans {
                out[base + ch] = self.ring[slot + ch] * ga;
            }
            self.filled -= 1;
            self.fade_pos += 1;
        }
        if self.fade_pos >= self.fade_window {
            self.fading = false;
            self.filled = (self.write_pos + cap - self.fade_write_start) % cap;
            self.fade_in_active = true;
            self.fade_in_pos = 0;
            if self.filled > 0 {
                self.drain_after_jump = true;
            }
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
                self.start_fade();
            }
            self.last_label = label;
            self.primed = true;
            self.track_frames += n / self.channels;
            self.mix_frames(n / self.channels, buffer);
            return n;
        }
        if self.child.is_exhausted() {
            return self.drain(buffer);
        }
        0
    }

    fn is_exhausted(&self) -> bool {
        self.child.is_exhausted() && self.filled == 0 && !self.fading
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
        silence_tail_frames: usize,
        floor: f32,
        click_at: Option<usize>,
        click_amp: f32,
    }

    impl FakeSource {
        fn new(value: f32, total_frames: usize, label: &str) -> Self {
            Self {
                value,
                total_frames,
                pos_frames: 0,
                label: label.to_string(),
                silence_tail_frames: 0,
                floor: 0.0,
                click_at: None,
                click_amp: 0.0,
            }
        }

        fn with_silence_tail(mut self, frames: usize) -> Self {
            self.silence_tail_frames = frames;
            self
        }

        /// The silence tail carries a constant `floor` instead of zero,
        /// like a quiet noise floor below the music.
        fn with_floor(mut self, floor: f32) -> Self {
            self.floor = floor;
            self
        }

        /// A single `amp`-sized sample deep in the silence tail, like a
        /// click or an artifact.
        fn with_click(mut self, at_frame: usize, amp: f32) -> Self {
            self.click_at = Some(at_frame);
            self.click_amp = amp;
            self
        }
    }

    impl AudioSource for FakeSource {
        fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
            let want = buffer.len() / CHANS;
            let avail = self.total_frames.saturating_sub(self.pos_frames);
            let n_frames = avail.min(want);
            let n = n_frames * CHANS;
            let silent_from = self.total_frames.saturating_sub(self.silence_tail_frames);
            for f in 0..n_frames {
                let frame = self.pos_frames + f;
                let v = if frame >= silent_from {
                    if self.click_at == Some(frame) {
                        self.click_amp
                    } else {
                        self.floor
                    }
                } else {
                    self.value
                };
                buffer[f * CHANS..(f + 1) * CHANS].fill(v);
            }
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
            Self::from_sources(
                FakeSource::new(a_value, a_frames, "a"),
                FakeSource::new(b_value, b_frames, "b"),
            )
        }

        fn from_sources(a: FakeSource, b: FakeSource) -> Self {
            Self {
                a,
                b,
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
    fn crossfade_fades_the_tail_out_once_then_ramps_the_next_track_in() {
        // Track A: 2.0s at 1.0, track B: 4.0s at 0.5, fade 0.2s (20 frames).
        // The output trails by 20 frames: 20 frames of startup silence,
        // then A, then A's final 20 frames fading out (the only time they
        // are heard, overlapping the tail), then B ramping in over 2
        // frames, and finally the ring draining out at the stream's end.
        let mut src = wrap(Box::new(PairSource::new(1.0, 200, 0.5, 400)), 0.2);
        let amps = pull(&mut src, 700, 10);

        assert_eq!(amps.len(), 20 + 200 + 400);
        assert_eq!(amps[0], 0.0, "startup fills the ring");
        assert_eq!(amps[19], 0.0);
        assert_eq!(amps[20], 1.0);
        assert_eq!(amps[199], 1.0);
        // Fade-out over A's final 20 frames, first and only hearing: the
        // ring holds them at the label change, so they play at outputs
        // 200..219 fading 1.0 down to 0.05 (linear curve).
        assert!((amps[200] - 1.0).abs() < 1e-6, "fade start was {}", amps[200]);
        assert!((amps[210] - 0.5).abs() < 1e-6, "mid-fade was {}", amps[210]);
        assert!((amps[219] - 0.05).abs() < 1e-6, "fade end was {}", amps[219]);
        // The jump skips the trailing silence; B ramps in over 2 frames.
        assert_eq!(amps[220], 0.0);
        assert!((amps[221] - 0.25).abs() < 1e-6, "ramp was {}", amps[221]);
        assert!((amps[222] - 0.5).abs() < 1e-6);
        // B passes at full level, then the stream's tail drains out.
        assert_eq!(amps[599], 0.5);
        assert_eq!(amps[600], 0.5);
        assert!((amps[619] - 0.025).abs() < 1e-6, "drain end was {}", amps[619]);
    }

    #[test]
    fn crossfade_never_replays_the_ring_after_exhaustion() {
        // B (10 frames) ends mid-fade: the fade-out finishes over the ring
        // (output frames 200..219), B's 10 frames then ramp in once, and
        // nothing of the ring is replayed after that.
        let mut src = wrap(Box::new(PairSource::new(1.0, 200, 0.5, 10)), 0.2);
        let amps = pull(&mut src, 300, 10);
        assert_eq!(amps.len(), 20 + 200 + 10, "no tail replay expected");
        assert!((amps[219] - 0.05).abs() < 1e-6, "tail end was {}", amps[219]);
        assert_eq!(amps[220], 0.0);
        assert!((amps[221] - 0.25).abs() < 1e-6);
        assert_eq!(amps[222], 0.5);
        assert_eq!(amps[229], 0.5, "B's last frame passes once");
        assert!(src.is_exhausted());
    }

    #[test]
    fn crossfade_mid_fade_exhaustion_drains_the_tail() {
        // B is shorter still (5 frames): the fade-out's remaining 15 frames
        // drain over the ring (output frames 205..219), then B ramps in and
        // the stream ends.
        let mut src = wrap(Box::new(PairSource::new(1.0, 200, 0.5, 5)), 0.2);
        let amps = pull(&mut src, 300, 10);
        assert_eq!(amps.len(), 20 + 200 + 5);
        assert!((amps[205] - 0.75).abs() < 1e-6, "drain start was {}", amps[205]);
        assert!((amps[219] - 0.05).abs() < 1e-6, "tail end was {}", amps[219]);
        assert_eq!(amps[220], 0.0);
        assert_eq!(amps[222], 0.5);
        assert_eq!(amps[224], 0.5, "B's last frame passes once");
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
    fn crossfade_curve_2_shapes_the_fade_out() {
        // curve = 2.0: g(t) = t^2, so the outgoing tail's midpoint gain is
        // (1 - 0.5)^2 = 0.25.
        let mut src = OverlapSource::new(
            Box::new(PairSource::new(1.0, 200, 0.5, 400)),
            0.2,
            2.0,
            RATE as u32,
            CHANS,
        );
        let amps = pull(&mut src, 300, 10);
        assert!((amps[210] - 0.25).abs() < 1e-6, "mid-fade was {}", amps[210]);
    }

    #[test]
    fn crossfade_skips_the_trailing_silence() {
        // A: 2.0s loud + 0.2s silence, B starts at frame 220. The ring's
        // last 50 frames hold 30 audible + 20 silent, so the fade-out
        // spans 30 frames and the silent tail is skipped: B ramps in right
        // after A's last audible frame instead of after the silence.
        let src = PairSource::from_sources(
            FakeSource::new(1.0, 220, "a").with_silence_tail(20),
            FakeSource::new(0.5, 400, "b"),
        );
        let mut src = OverlapSource::new(Box::new(src), 0.5, 1.0, RATE as u32, CHANS);
        let amps = pull(&mut src, 700, 10);
        assert_eq!(amps.len(), 220 + 400 + 30, "A and B pass, then the ring drains");
        assert_eq!(amps[219], 1.0, "A's last audible frame, delayed");
        assert!((amps[249] - 1.0 / 30.0).abs() < 1e-6, "fade end was {}", amps[249]);
        assert_eq!(amps[250], 0.0, "B ramps in at the fade end, not after the silence");
        assert_eq!(amps[254], 0.4);
        assert_eq!(amps[255], 0.5);
        assert!((amps[649] - 0.5 / 30.0).abs() < 1e-6, "drain end was {}", amps[649]);
    }

    #[test]
    fn crossfade_fades_out_minimally_over_dead_silence() {
        // A: 2.0s loud + 1.5s silence, fade 2.0s (200 frames): the ring's
        // audible tail ends 50 frames in, so the fade-out spans 50 frames
        // and B ramps in after A's music, never after the 1.5s of dead air.
        let src = PairSource::from_sources(
            FakeSource::new(1.0, 350, "a").with_silence_tail(150),
            FakeSource::new(0.5, 200, "b"),
        );
        let mut src = OverlapSource::new(Box::new(src), 2.0, 1.0, RATE as u32, CHANS);
        let amps = pull(&mut src, 800, 10);
        assert_eq!(amps.len(), 200 + 350 + 50, "B passes, then the ring drains");
        assert_eq!(amps[349], 1.0, "A's last audible frame, delayed");
        assert!((amps[399] - 0.02).abs() < 1e-6, "fade end was {}", amps[399]);
        assert_eq!(amps[400], 0.0, "B ramps in at the fade end, never after dead air");
        assert_eq!(amps[420], 0.5, "B at full level once the 20-frame ramp ends");
        assert_eq!(amps[549], 0.5, "B's last frame passes at full level");
        assert!((amps[599] - 0.01).abs() < 1e-6, "drain end was {}", amps[599]);
    }

    #[test]
    fn crossfade_ignores_a_noise_floor_below_the_music() {
        // A: 6.5s loud + 1.5s of a −66 dBFS noise floor. A fixed −66 dB
        // sample threshold would count the floor as audio and drag the
        // fade through it; the loudness gate (10 LU below the track's own
        // mean) treats it as silence, so the fade stops at the music.
        let src = PairSource::from_sources(
            FakeSource::new(1.0, 800, "a").with_silence_tail(150).with_floor(0.0005),
            FakeSource::new(0.5, 400, "b"),
        );
        let mut src = OverlapSource::new(Box::new(src), 2.0, 1.0, RATE as u32, CHANS);
        let amps = pull(&mut src, 1300, 10);
        assert_eq!(amps.len(), 800 + 400 + 50, "B passes, then the ring drains");
        assert_eq!(amps[800], 1.0, "fade-out starts at A's last audible part");
        assert!((amps[849] - 1.0 / 50.0).abs() < 1e-6, "fade end was {}", amps[849]);
        assert_eq!(amps[850], 0.0, "B ramps in at the fade end, not after the floor");
        assert_eq!(amps[870], 0.5, "B at full level once the 20-frame ramp ends");
        assert_eq!(amps[1199], 0.5, "B's last frame passes at full level");
        assert!((amps[1249] - 0.5 / 50.0).abs() < 1e-6, "drain end was {}", amps[1249]);
    }

    #[test]
    fn crossfade_ignores_a_click_in_the_trailing_silence() {
        // A: 2.0s loud + 1.5s silence with a single −6 dB click in the
        // middle of it. A sample threshold would anchor the fade to the
        // click and drag it 1.0s past the music; the windowed gate
        // smooths the click away, so the fade still ends at the music.
        let src = PairSource::from_sources(
            FakeSource::new(1.0, 350, "a")
                .with_silence_tail(150)
                .with_click(300, 0.5),
            FakeSource::new(0.5, 400, "b"),
        );
        let mut src = OverlapSource::new(Box::new(src), 2.0, 1.0, RATE as u32, CHANS);
        let amps = pull(&mut src, 900, 10);
        assert_eq!(amps.len(), 350 + 400 + 50, "B passes, then the ring drains");
        assert!((amps[399] - 1.0 / 50.0).abs() < 1e-6, "fade end was {}", amps[399]);
        assert_eq!(amps[400], 0.0, "B ramps in at the fade end, never at the click");
        assert_eq!(amps[500], 0.5, "the click's output slot holds B at full level");
        assert!((amps[799] - 0.5 / 50.0).abs() < 1e-6, "drain end was {}", amps[799]);
    }

    #[test]
    fn crossfade_fades_out_a_quiet_track_by_its_own_level() {
        // A: 2.0s at −68 dBFS + 1.5s silence. The absolute gate (−70 dBFS)
        // still counts the quiet music as audio, so the fade spans it; a
        // fixed −66 dB threshold would have heard nothing and floored the
        // fade at 15 frames.
        let src = PairSource::from_sources(
            FakeSource::new(0.0004, 350, "a").with_silence_tail(150),
            FakeSource::new(0.5, 200, "b"),
        );
        let mut src = OverlapSource::new(Box::new(src), 2.0, 1.0, RATE as u32, CHANS);
        let amps = pull(&mut src, 700, 10);
        assert_eq!(amps.len(), 350 + 200 + 50, "B passes, then the ring drains");
        assert!((amps[399] - 0.0004 / 50.0).abs() < 1e-10, "fade end was {}", amps[399]);
        assert_eq!(amps[400], 0.0, "B ramps in at the fade end");
        assert_eq!(amps[420], 0.5, "B at full level once the 20-frame ramp ends");
        assert!((amps[599] - 0.5 / 50.0).abs() < 1e-6, "drain end was {}", amps[599]);
    }

    #[test]
    fn crossfade_falls_back_to_the_floor_when_nothing_is_audible() {
        // A is entirely silent: nothing passes the gate, so the fade is
        // floored at MIN_FADE_SECONDS and B still ramps in on time.
        let src = PairSource::from_sources(
            FakeSource::new(0.0, 200, "a"),
            FakeSource::new(0.5, 200, "b"),
        );
        let mut src = OverlapSource::new(Box::new(src), 2.0, 1.0, RATE as u32, CHANS);
        let amps = pull(&mut src, 500, 10);
        assert_eq!(amps.len(), 200 + 200 + 15, "B passes, then the ring drains");
        assert_eq!(amps[214], 0.0, "fade-out end, all silence");
        assert_eq!(amps[215], 0.0, "B ramps in at the fade end");
        assert_eq!(amps[235], 0.5, "B at full level once the 20-frame ramp ends");
        assert!((amps[414] - 0.5 / 15.0).abs() < 1e-6, "drain end was {}", amps[414]);
    }

    #[test]
    fn crossfade_duration_shorter_than_the_floor_uses_the_whole_ring() {
        // crossfade_seconds below MIN_FADE_SECONDS: the floor can't exceed
        // the ring, so the fade covers the entire (small) ring and the
        // next track still ramps in — no overrun, no dead air.
        let mut src = wrap(Box::new(PairSource::new(1.0, 200, 0.5, 400)), 0.05);
        let amps = pull(&mut src, 700, 10);
        assert_eq!(amps.len(), 200 + 400 + 5, "A, B, and the tiny ring's tail");
        assert_eq!(amps[199], 1.0, "A's last frame before the fade, delayed by 5");
        assert!((amps[204] - 0.2).abs() < 1e-6, "fade end was {}", amps[204]);
        assert_eq!(amps[205], 0.0, "B ramps in on a single frame");
        assert_eq!(amps[206], 0.5, "B at full level right after the 1-frame ramp");
        assert_eq!(amps[599], 0.5, "B's last full-level frame");
        assert!((amps[604] - 0.1).abs() < 1e-6, "drain end was {}", amps[604]);
    }

    #[test]
    fn crossfade_long_duration_caps_the_window_to_the_track() {
        // crossfade_seconds far longer than the track: the ring never fills
        // (A is shorter than 30 s), so the output is startup silence until
        // the boundary, and the fade window is capped by the outgoing
        // track's last quarter — a short track under a huge crossfade still
        // fades sanely and B comes in on time.
        let mut src = wrap(Box::new(PairSource::new(1.0, 200, 0.5, 400)), 30.0);
        let amps = pull(&mut src, 4000, 10);
        assert_eq!(amps.len(), 200 + 400 + 50, "A, B, and the capped window's tail");
        assert_eq!(amps[199], 0.0, "ring never fills — still startup silence");
        assert_eq!(amps[200], 1.0, "fade-out starts on A's head (unheard in the fill)");
        assert!((amps[249] - 0.02).abs() < 1e-6, "fade end was {}", amps[249]);
        assert_eq!(amps[250], 0.0, "B ramps in over the 300-frame fade-in");
        assert_eq!(amps[550], 0.5, "B at full level once the fade-in ends");
        assert_eq!(amps[599], 0.5, "B's last full-level frame");
        assert!((amps[649] - 0.01).abs() < 1e-6, "drain end was {}", amps[649]);
    }
}