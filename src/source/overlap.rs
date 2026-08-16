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
/// Both gates are applied to the *K-weighted* signal, exactly like the
/// standard's loudness measurement (see the K-weighting specs below).
const GATE_ABSOLUTE_DB: f32 = -70.0;
const GATE_RELATIVE_LU: f32 = 10.0;

/// ITU-R BS.1770-4 Annex 2 K-weighting filter specs: a second-order
/// high-pass at 38.135 Hz (Q 0.5003) in series with a +4 dB high-shelf
/// at 1681.974 Hz (Q 0.7072). The biquads are derived from these analog
/// prototypes at the bus rate with the RBJ audio-EQ cookbook — the
/// published ITU coefficient table is only valid at 48 kHz, while
/// crabsoup's bus rate is configurable. K-weighting keeps a high-
/// frequency decay (cymbals, hi-hats, breaths) audible to the gate,
/// where raw mean-square would underweight it and fade too early.
const K_HP_F0: f64 = 38.135_470_827_274;
const K_HP_Q: f64 = 0.500_327_037_323_877;
const K_SHELF_DB: f64 = 4.0;
const K_SHELF_F0: f64 = 1_681.974_450_955_53;
const K_SHELF_Q: f64 = 0.707_175_236_955_419;

/// One direct-form-I biquad section of the K-weighting cascade.
#[derive(Clone, Copy)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl Biquad {
    /// One sample through the section (state is per channel).
    fn process(&self, state: &mut BiquadState, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * state.x1 + self.b2 * state.x2
            - self.a1 * state.y1
            - self.a2 * state.y2;
        state.x2 = state.x1;
        state.x1 = x;
        state.y2 = state.y1;
        state.y1 = y;
        y
    }
}

#[derive(Clone, Copy, Default)]
struct BiquadState {
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

/// The cascade's per-channel state: both stages, in series.
#[derive(Clone, Copy, Default)]
struct KState {
    hp: BiquadState,
    shelf: BiquadState,
}

/// De Man (BS.1770) high-pass section: the bilinear transform of the RLB
/// prototype, normalized to b = [1, -2, 1] — the exact form the ITU
/// publishes for 48 kHz (BS.1770-4 Table 2). `K = tan(pi f0 / rate)` is
/// the un-prewarped bilinear corner, so arbitrary bus rates get the same
/// response the standard specifies at 48 kHz.
fn biquad_high_pass(f0: f64, q: f64, rate: f64) -> Biquad {
    let k = (std::f64::consts::PI * f0 / rate).tan();
    let a0 = 1.0 + k / q + k * k;
    Biquad {
        b0: 1.0,
        b1: -2.0,
        b2: 1.0,
        a1: (2.0 * (k * k - 1.0) / a0) as f32,
        a2: ((1.0 - k / q + k * k) / a0) as f32,
    }
}

/// De Man (BS.1770) high-shelf section for the stage-1 spherical-head
/// filter: `Vh = 10^(db/20)` is the voltage gain, and `Vb = Vh^0.49967`
/// sets the mid-band tilt such that the section reproduces the ITU's
/// published 48 kHz coefficients (BS.1770-4 Table 1).
fn biquad_high_shelf(f0: f64, q: f64, db_gain: f64, rate: f64) -> Biquad {
    let k = (std::f64::consts::PI * f0 / rate).tan();
    let vh = 10f64.powf(db_gain / 20.0);
    let vb = vh.powf(0.499_666_774_155);
    let a0 = 1.0 + k / q + k * k;
    Biquad {
        b0: ((vh + vb * k / q + k * k) / a0) as f32,
        b1: (2.0 * (k * k - vh) / a0) as f32,
        b2: ((vh - vb * k / q + k * k) / a0) as f32,
        a1: (2.0 * (k * k - 1.0) / a0) as f32,
        a2: ((1.0 - k / q + k * k) / a0) as f32,
    }
}

/// The two K-weighting sections for a given bus rate.
fn k_weighting_biquads(rate: f64) -> (Biquad, Biquad) {
    (biquad_high_pass(K_HP_F0, K_HP_Q, rate), biquad_high_shelf(K_SHELF_F0, K_SHELF_Q, K_SHELF_DB, rate))
}

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
    /// K-weighted span for the gate scan, sized on demand like `scratch`.
    scan: Vec<f32>,
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
            scan: Vec::new(),
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
    /// with BS.1770/EBU R128 gating instead of a bare amplitude floor, so
    /// a track's own loudness — not a fixed −66 dBFS constant — decides
    /// what is silence. The span is first passed through the K-weighting
    /// filters, then short-time mean-square windows (50 ms, 10 ms hop) are
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
        // K-weight the span once: BS.1770 measures loudness through these
        // filters, so a high-frequency decay stays audible to the gate.
        // State is zeroed per scan; the startup transient only affects
        // windows before the fade end, which the scan never selects.
        self.scan.resize(span * chans, 0.0);
        let (hp, shelf) = k_weighting_biquads(rate);
        let mut st = vec![KState::default(); chans];
        for i in 0..span {
            let base = ((self.write_pos + cap - self.filled + i) % cap) * chans;
            for (c, state) in st.iter_mut().enumerate() {
                let x = self.ring[base + c];
                let hp_out = hp.process(&mut state.hp, x);
                self.scan[i * chans + c] = shelf.process(&mut state.shelf, hp_out);
            }
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
                let base = i * chans;
                for c in 0..chans {
                    let s = self.scan[base + c];
                    sum += s * s;
                }
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
                let base = i * chans;
                if self.scan[base..base + chans]
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

/// The fade-point gate tests, at a rate where the K-weighting filters are
/// honest: the high-shelf corner (1681.97 Hz) needs the bus above 2x that,
/// so these use 4 kHz and a 125 Hz AC tone (one cycle per 32 frames, ~0 dB
/// of K-weighting gain) instead of the DC constants the machinery tests
/// use — K-weighting kills DC outright, so DC no longer reaches the gate.
#[cfg(test)]
mod gate_tests {
    use super::*;

    const RATE: usize = 4000;
    const CHANS: usize = 2;
    const TONE_HZ: f32 = 125.0;

    /// The test tone's sample at frame `k`, phase-exact and deterministic.
    fn tone(k: usize, amp: f32) -> f32 {
        amp * (std::f32::consts::TAU * TONE_HZ * k as f32 / RATE as f32).sin()
    }

    struct ToneSource {
        amp: f32,
        total_frames: usize,
        pos_frames: usize,
        label: String,
        silence_tail_frames: usize,
        floor: f32,
        click_at: Option<usize>,
        click_amp: f32,
    }

    impl ToneSource {
        fn new(amp: f32, total_frames: usize, label: &str) -> Self {
            Self {
                amp,
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
        fn with_floor(mut self, floor: f32) -> Self {
            self.floor = floor;
            self
        }
        fn with_click(mut self, at_frame: usize, amp: f32) -> Self {
            self.click_at = Some(at_frame);
            self.click_amp = amp;
            self
        }
    }

    impl AudioSource for ToneSource {
        fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
            let n_frames = self
                .total_frames
                .saturating_sub(self.pos_frames)
                .min(buffer.len() / CHANS);
            let silent_from = self.total_frames.saturating_sub(self.silence_tail_frames);
            for f in 0..n_frames {
                let frame = self.pos_frames + f;
                let v = if frame >= silent_from {
                    if self.click_at == Some(frame) {
                        self.click_amp
                    } else {
                        tone(frame, self.floor)
                    }
                } else {
                    tone(frame, self.amp)
                };
                buffer[f * CHANS..(f + 1) * CHANS].fill(v);
            }
            self.pos_frames += n_frames;
            n_frames * CHANS
        }
        fn is_exhausted(&self) -> bool {
            self.pos_frames >= self.total_frames
        }
        fn label(&self) -> Option<String> {
            Some(self.label.clone())
        }
    }

    struct PairSource {
        a: ToneSource,
        b: ToneSource,
        on_b: bool,
    }

    impl PairSource {
        fn from_sources(a: ToneSource, b: ToneSource) -> Self {
            Self { a, b, on_b: false }
        }
    }

    impl AudioSource for PairSource {
        fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
            if !self.on_b && self.a.is_exhausted() {
                self.on_b = true;
            }
            if self.on_b {
                self.b.next_buffer(buffer)
            } else {
                self.a.next_buffer(buffer)
            }
        }
        fn is_exhausted(&self) -> bool {
            self.on_b && self.b.is_exhausted()
        }
        fn label(&self) -> Option<String> {
            if self.on_b {
                self.b.label()
            } else {
                self.a.label()
            }
        }
    }

    fn wrap(src: Box<dyn AudioSource>, duration: f64) -> OverlapSource {
        OverlapSource::new(src, duration, 1.0, RATE as u32, CHANS)
    }

    fn pull(src: &mut dyn AudioSource, frames: usize, buf_frames: usize) -> Vec<f32> {
        let mut out = Vec::new();
        let mut buf = vec![0.0; buf_frames * CHANS];
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

    fn amp(amps: &[f32], frame: usize) -> f32 {
        amps[frame]
    }

    #[test]
    #[allow(clippy::excessive_precision)]
    fn k_weighting_biquads_reproduce_the_published_48_khz_coefficients() {
        // ITU-R BS.1770-4 Tables 1 and 2 (fs = 48 kHz). The De Man
        // sections reproduce the stage-2 high-pass exactly (a's to 1e-11)
        // and the stage-1 shelf to ~5e-5 — the residual is the ITU's own
        // rounding of the published digits, and the standard states that
        // small coefficient variations do not affect the algorithm's
        // performance. Tolerance 1e-3 keeps this a real pin, not a tautology.
        let (hp, shelf) = k_weighting_biquads(48000.0);
        assert_eq!(hp.b0, 1.0);
        assert_eq!(hp.b1, -2.0);
        assert_eq!(hp.b2, 1.0);
        assert!((hp.a1 + 1.990_047_454_833_98).abs() < 1e-3, "hp.a1 {}", hp.a1);
        assert!((hp.a2 - 0.990_072_250_366_21).abs() < 1e-3, "hp.a2 {}", hp.a2);
        assert!((shelf.b0 - 1.535_124_859_586_97).abs() < 1e-3, "shelf.b0 {}", shelf.b0);
        assert!((shelf.b1 + 2.691_696_189_406_38).abs() < 1e-3, "shelf.b1 {}", shelf.b1);
        assert!((shelf.b2 - 1.198_392_810_852_85).abs() < 1e-3, "shelf.b2 {}", shelf.b2);
        assert!((shelf.a1 + 1.690_659_293_182_41).abs() < 1e-3, "shelf.a1 {}", shelf.a1);
        assert!((shelf.a2 - 0.732_480_774_215_85).abs() < 1e-3, "shelf.a2 {}", shelf.a2);
    }

    #[test]
    fn k_weighting_blocks_dc_outright() {
        // The high-pass section removes DC, which is what makes the gate
        // tests below use an AC tone: a DC floor would read as silence.
        let (hp, shelf) = k_weighting_biquads(48000.0);
        let mut st = KState::default();
        let mut last = 0.0f32;
        for _ in 0..48000 {
            last = shelf.process(&mut st.shelf, hp.process(&mut st.hp, 1.0));
        }
        assert!(last.abs() < 1e-4, "DC must settle to silence, got {last}");
    }

    #[test]
    fn crossfade_skips_the_trailing_silence() {
        // A: 2.0s loud + 0.2s silence, B starts at frame 8800. The ring's
        // last 2000 frames hold 1200 audible + 800 silent, so the fade-out
        // spans 1210 frames (the last 10 are the K-weighting high-pass
        // decay left over from the tone's abrupt end, still above the
        // gate level) and the silent tail is skipped: B ramps in right
        // after A's music instead of after the silence.
        let src = PairSource::from_sources(
            ToneSource::new(1.0, 8800, "a").with_silence_tail(800),
            ToneSource::new(0.5, 16000, "b"),
        );
        let mut src = wrap(Box::new(src), 0.5);
        let amps = pull(&mut src, 27000, 10);
        assert_eq!(amps.len(), 26010, "A and B pass, then the ring drains");
        assert_eq!(amp(&amps, 1999), 0.0, "startup ring of silence");
        assert_eq!(amp(&amps, 2000), tone(0, 1.0), "A starts right after the startup ring");
        assert!((amp(&amps, 8799) - tone(6799, 1.0)).abs() < 1e-6, "A's last full-level frame");
        assert!(
            (amp(&amps, 9999) - tone(7999, 1.0) * 11.0 / 1210.0).abs() < 1e-6,
            "fade end was {}",
            amp(&amps, 9999)
        );
        assert_eq!(amp(&amps, 10009), 0.0, "the fade's last frame sits in the tail");
        assert_eq!(amp(&amps, 10010), 0.0, "B ramps in at the fade end, not after the silence");
        assert!(
            (amp(&amps, 10209) - tone(199, 0.5) * 199.0 / 200.0).abs() < 1e-6,
            "B ramp end was {}",
            amp(&amps, 10209)
        );
        assert!((amp(&amps, 10210) - tone(200, 0.5)).abs() < 1e-6, "B at full level");
        assert!((amp(&amps, 24799) - tone(14789, 0.5)).abs() < 1e-6, "B's last full-level frame");
        assert!(
            (amp(&amps, 26009) - tone(15999, 0.5) / 1210.0).abs() < 1e-6,
            "drain end was {}",
            amp(&amps, 26009)
        );
    }

    #[test]
    fn crossfade_fades_out_minimally_over_dead_silence() {
        // A: 5.0s loud + 1.5s silence, fade 2.0s (8000 frames). The ring's
        // audible part is A's last 2000 loud frames (2000 audible + 6000
        // dead air in the held tail), so the fade-out spans ~2010 frames
        // (the high-pass decay after the tone cut included) and B ramps in
        // after A's music, never after the dead air.
        let src = PairSource::from_sources(
            ToneSource::new(1.0, 26000, "a").with_silence_tail(6000),
            ToneSource::new(0.5, 8000, "b"),
        );
        let mut src = wrap(Box::new(src), 2.0);
        let amps = pull(&mut src, 37000, 10);
        assert_eq!(amps.len(), 36010, "B passes, then the ring drains");
        assert!((amp(&amps, 25999) - tone(17999, 1.0)).abs() < 1e-6, "A's last full-level frame");
        assert!(
            (amp(&amps, 27999) - tone(19999, 1.0) * 11.0 / 2010.0).abs() < 1e-6,
            "fade end was {}",
            amp(&amps, 27999)
        );
        assert_eq!(amp(&amps, 28009), 0.0, "the fade's last frame sits in the dead air");
        assert_eq!(amp(&amps, 28010), 0.0, "B ramps in at the fade end, never after dead air");
        assert!(
            (amp(&amps, 28809) - tone(799, 0.5) * 799.0 / 800.0).abs() < 1e-6,
            "B ramp end was {}",
            amp(&amps, 28809)
        );
        assert_eq!(amp(&amps, 28810), tone(800, 0.5), "B at full level");
        assert!((amp(&amps, 33999) - tone(5989, 0.5)).abs() < 1e-6, "B's last full-level frame");
        assert!(
            (amp(&amps, 36009) - tone(7999, 0.5) / 2010.0).abs() < 1e-6,
            "drain end was {}",
            amp(&amps, 36009)
        );
    }

    #[test]
    fn crossfade_ignores_a_noise_floor_below_the_music() {
        // A: 9.5s loud + 1.5s of a −66 dBFS tone floor. The loudness gate
        // (10 LU below the track's own mean) treats the floor as silence,
        // so the fade stops at the music instead of dragging through it.
        let src = PairSource::from_sources(
            ToneSource::new(1.0, 44000, "a").with_silence_tail(6000).with_floor(0.0005),
            ToneSource::new(0.5, 16000, "b"),
        );
        let mut src = wrap(Box::new(src), 2.0);
        let amps = pull(&mut src, 63000, 10);
        assert_eq!(amps.len(), 62016, "B passes, then the ring drains");
        assert!((amp(&amps, 43983) - tone(35983, 1.0)).abs() < 1e-6, "A's last full-level frame");
        assert!(
            (amp(&amps, 45999) - tone(37999, 1.0) * 17.0 / 2016.0).abs() < 1e-6,
            "fade end was {}",
            amp(&amps, 45999)
        );
        assert!(
            (amp(&amps, 46015) - tone(38015, 0.0005) / 2016.0).abs() < 1e-6,
            "the fade's last frame is floor: {}",
            amp(&amps, 46015)
        );
        assert_eq!(amp(&amps, 46016), tone(0, 0.5), "B ramps in at the fade end, not after the floor");
        assert!(
            (amp(&amps, 46815) - tone(799, 0.5) * 799.0 / 800.0).abs() < 1e-6,
            "B ramp end was {}",
            amp(&amps, 46815)
        );
        assert_eq!(amp(&amps, 46816), tone(800, 0.5), "B at full level");
        assert!((amp(&amps, 59999) - tone(13983, 0.5)).abs() < 1e-6, "B's last full-level frame");
        assert!(
            (amp(&amps, 62015) - tone(15999, 0.5) / 2016.0).abs() < 1e-6,
            "drain end was {}",
            amp(&amps, 62015)
        );
    }

    #[test]
    fn crossfade_ignores_a_click_in_the_trailing_silence() {
        // A: 5.0s loud + 1.5s silence with a single −6 dB click deep in
        // the silence, inside the scanned span but below the relative
        // gate. A sample threshold would anchor the fade to the click;
        // the windowed gate smooths it away, so the fade still ends at
        // the music and the click (in the skipped tail) never airs.
        let src = PairSource::from_sources(
            ToneSource::new(1.0, 26000, "a")
                .with_silence_tail(6000)
                .with_click(22000, 0.5),
            ToneSource::new(0.5, 16000, "b"),
        );
        let mut src = wrap(Box::new(src), 2.0);
        let amps = pull(&mut src, 45000, 10);
        assert_eq!(amps.len(), 44011, "B passes, then the ring drains");
        assert!(
            (amp(&amps, 27999) - tone(19999, 1.0) * 12.0 / 2011.0).abs() < 1e-6,
            "fade end was {}",
            amp(&amps, 27999)
        );
        assert_eq!(amp(&amps, 28011), tone(0, 0.5), "B ramps in at the fade end, never at the click");
        assert_eq!(amp(&amps, 30000), tone(1989, 0.5), "the click's slot holds B's music, not the click");
        assert!((amp(&amps, 41999) - tone(13988, 0.5)).abs() < 1e-6, "B's last full-level frame");
        assert!(
            (amp(&amps, 44010) - tone(15999, 0.5) / 2011.0).abs() < 1e-6,
            "drain end was {}",
            amp(&amps, 44010)
        );
    }

    #[test]
    fn crossfade_fades_out_a_quiet_track_by_its_own_level() {
        // A: 5.0s at −60 dBFS (sine peak 0.001, mean-square still above
        // the −70 dBFS absolute gate) + 1.5s silence. The loudness gate
        // counts the quiet music as audio, so the fade spans it; a fixed
        // sample threshold far below would have floored the fade.
        let src = PairSource::from_sources(
            ToneSource::new(0.001, 26000, "a").with_silence_tail(6000),
            ToneSource::new(0.5, 8000, "b"),
        );
        let mut src = wrap(Box::new(src), 2.0);
        let amps = pull(&mut src, 37000, 10);
        assert_eq!(amps.len(), 36006, "B passes, then the ring drains");
        assert!(
            (amp(&amps, 27999) - tone(19999, 0.001) * 7.0 / 2006.0).abs() < 1e-5,
            "fade end was {}",
            amp(&amps, 27999)
        );
        assert_eq!(amp(&amps, 28005), 0.0, "the fade's last frame sits in the dead air");
        assert_eq!(amp(&amps, 28006), tone(0, 0.5), "B ramps in at the fade end");
        assert_eq!(amp(&amps, 28806), tone(800, 0.5), "B at full level");
        assert!(
            (amp(&amps, 36005) - tone(7999, 0.5) / 2006.0).abs() < 1e-5,
            "drain end was {}",
            amp(&amps, 36005)
        );
    }
}