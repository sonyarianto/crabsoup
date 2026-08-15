use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

use symphonia::core::audio::SignalSpec;

use crate::config::MixerConfig;
use crate::source::file::FileSource;
use crate::source::{AudioSource, SilenceSource, SourceProvider};

/// State of a crossfade that was promoted to the active source before the
/// fade had completed (e.g. the outgoing track ended early). The active source
/// keeps ramping in to full gain over the remainder of the window.
struct Tail {
    start_gain: f64,
    remaining: usize,
    total: usize,
}

/// Points in the fade-curve lookup table. 2048 samples with linear
/// interpolation is smooth enough for any fade curve and replaces the two
/// `powf` calls per sample that made the mixing path ~200x the copy path.
const CURVE_TABLE_SIZE: usize = 2048;

/// Level-aware crossfade settings (Liquidsoap `smart_crossfade`): the
/// outgoing track's measured tail level picks the transition window — a
/// loud tail gets a full `fade_out` crossfade, a quiet tail only a short
/// `fade_mid` fade (no point dragging a crossfade over silence). A
/// `fade_mid` longer than `fade_out` is accepted (not rejected) and just
/// degrades into the existing tail ramp.
#[derive(Clone, Copy, Debug)]
pub struct SmartFade {
    /// Crossfade window (and preload margin) when the outgoing tail is
    /// loud, in seconds.
    pub fade_out: f64,
    /// Shorter window used when the outgoing tail is quiet, in seconds.
    pub fade_mid: f64,
    /// RMS level (dBFS) below which an outgoing tail counts as quiet.
    pub threshold_db: f32,
}

/// A gapless track-to-track crossfade mixer.
///
/// Holds the currently-playing source and lazily preloads the *next* source
/// (from a [`SourceProvider`]) as soon as the active track enters the final
/// `crossfade_seconds`. Both sources are then mixed with a gain ramp:
///
/// ```text
/// MixedSample = SampleA * GainA + SampleB * GainB
/// ```
///
/// where `GainA: 1 -> 0` and `GainB: 0 -> 1` over the overlap window.
pub struct CrossfadeMixer {
    provider: Box<dyn SourceProvider>,
    active: Box<dyn AudioSource>,
    next: Option<Box<dyn AudioSource>>,
    active_label: String,
    next_label: Option<String>,
    /// Global crossfade window (seconds); per-track overrides replace it.
    crossfade_seconds: f64,
    /// The current transition's overlap window (frames): a per-track
    /// `(fade_in, fade_out)` override replaces the global when present,
    /// re-derived at every preload.
    fade_frames: usize,
    sample_rate: u32,
    /// `f(t) = t^fade_curve` sampled at `CURVE_TABLE_SIZE + 1` points.
    curve_table: Vec<f32>,
    channels: usize,
    fade_pos: usize,
    tail: Option<Tail>,
    started: bool,
    /// Level-aware fade selection (smart mode); `None` = plain crossfade.
    smart: Option<SmartFade>,
    /// Rolling sum of squares of the active track's recent audio, trimmed
    /// to the last `fade_out` seconds (the tail-level measurement).
    tail_sum_sq: f64,
    tail_samples: f64,
    /// Per-buffer energy chunks backing the rolling window.
    tail_chunks: VecDeque<(f64, f64)>,
    /// Reusable scratch buffers, sized on demand so `next_buffer` never
    /// allocates on the hot path.
    scratch_a: Vec<f32>,
    scratch_b: Vec<f32>,
}

impl CrossfadeMixer {
    pub fn new(
        provider: Box<dyn SourceProvider>,
        config: &MixerConfig,
        sample_rate: u32,
        channels: usize,
    ) -> Self {
        let cf = (config.crossfade_seconds * sample_rate as f64).max(1.0) as usize;
        Self {
            provider,
            active: Box::new(SilenceSource::new()),
            next: None,
            active_label: String::new(),
            next_label: None,
            crossfade_seconds: config.crossfade_seconds,
            fade_frames: cf,
            sample_rate,
            curve_table: (0..=CURVE_TABLE_SIZE)
                .map(|k| {
                    let t = k as f64 / CURVE_TABLE_SIZE as f64;
                    t.powf(config.fade_curve) as f32
                })
                .collect(),
            channels,
            fade_pos: 0,
            tail: None,
            started: false,
            smart: None,
            tail_sum_sq: 0.0,
            tail_samples: 0.0,
            tail_chunks: VecDeque::new(),
            scratch_a: Vec::new(),
            scratch_b: Vec::new(),
        }
    }

    /// Enable level-aware fade selection (smart mode).
    pub fn with_smart_fade(mut self, smart: SmartFade) -> Self {
        self.smart = Some(smart);
        self
    }

    /// Seconds before the active track's end at which the next track must be
    /// preloaded: the active track's `fade_out` override, else the global
    /// window (or the smart `fade_out` margin in smart mode).
    fn preload_margin(&self) -> f64 {
        self.active
            .crossfade_overrides()
            .and_then(|(_, fo)| fo)
            .unwrap_or_else(|| {
                self.smart
                    .map(|s| s.fade_out)
                    .unwrap_or(self.crossfade_seconds)
            })
    }

    /// Fold the active track's latest buffer into the rolling tail-level
    /// window (the last `fade_out` seconds of audio). Runs only in smart
    /// mode and only between transitions (the caller guards on `next` being
    /// None; the tail ramp after a mid-fade promotion is fine to measure
    /// too — the active track is the promoted one by then). If a single
    /// buffer exceeds the whole window (huge `frames_per_buffer` or tiny
    /// `fade_out`), each push immediately evicts itself and the reading
    /// falls back to "assume loud" — safe, but the quiet path never
    /// engages in that configuration.
    fn accumulate_tail(&mut self, sum_sq: f64, count: f64) {
        let Some(smart) = self.smart else {
            return;
        };
        self.tail_chunks.push_back((sum_sq, count));
        self.tail_sum_sq += sum_sq;
        self.tail_samples += count;
        let window = (smart.fade_out * self.sample_rate as f64 * self.channels as f64).max(1.0);
        while self.tail_samples > window {
            let (sq, n) = self.tail_chunks.pop_front().expect("chunks non-empty");
            self.tail_sum_sq -= sq;
            self.tail_samples -= n;
        }
    }

    /// RMS level (dBFS) of the active track's tail window.
    fn tail_level_db(&self) -> Option<f32> {
        if self.tail_samples <= 0.0 {
            return None;
        }
        let rms = (self.tail_sum_sq / self.tail_samples).sqrt();
        Some(20.0 * rms.max(1e-9).log10() as f32)
    }

    /// The level-aware outgoing window: a quiet tail gets a short `fade_mid`
    /// fade, a loud tail the full `fade_out`. `None` outside smart mode;
    /// with no measurement yet, assume loud (full window).
    fn smart_window(&self) -> Option<f64> {
        let smart = self.smart?;
        let loud = self
            .tail_level_db()
            .map(|db| db >= smart.threshold_db)
            .unwrap_or(true);
        Some(if loud { smart.fade_out } else { smart.fade_mid })
    }

    /// Drop the tail-level window (a new track became active).
    fn reset_tail(&mut self) {
        self.tail_chunks.clear();
        self.tail_sum_sq = 0.0;
        self.tail_samples = 0.0;
    }

    fn ensure_started(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        if self.provider.has_next() {
            let (src, label) = self.provider.next_source();
            self.active = src;
            self.active_label = label;
            self.reset_tail();
        }
    }
    fn preload_next(&mut self) {
        if self.next.is_none() && self.provider.has_next() {
            let (src, label) = self.provider.next_source();
            log::info!("crossfade: preloading next track");
            // Per-track fade override: the incoming track's `fade_in` wins,
            // then the outgoing track's `fade_out` (or, in smart mode, the
            // level-chosen window), then the global window.
            let window = src
                .crossfade_overrides()
                .and_then(|(fi, _)| fi)
                .or_else(|| {
                    self.active
                        .crossfade_overrides()
                        .and_then(|(_, fo)| fo)
                        .or_else(|| self.smart_window())
                })
                .unwrap_or(self.crossfade_seconds);
            self.fade_frames = (window * self.sample_rate as f64).max(1.0) as usize;
            self.next = Some(src);
            self.next_label = Some(label);
            self.fade_pos = 0;
            self.tail = None;
        }
    }

    /// Interpolated fade-curve value at `t` in `[0, 1]`. For a linear curve
    /// this is exact; for curved fades the interpolation error is far below
    /// audibility.
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
}

impl AudioSource for CrossfadeMixer {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        loop {
            self.ensure_started();

            // Preload the next source once the active track nears its end
            // (at the per-track fade-out margin, or the global window).
            if self.next.is_none() {
                let due = match self.active.remaining_seconds() {
                    Some(rem) => rem <= self.preload_margin(),
                    None => self.active.is_exhausted(),
                };
                if due {
                    self.preload_next();
                }
            }

            let wanted = buffer.len();
            if self.scratch_a.len() != wanted {
                self.scratch_a.resize(wanted, 0.0);
                self.scratch_b.resize(wanted, 0.0);
            }
            let n_a = self.active.next_buffer(&mut self.scratch_a);

            // The active track ended before a successor was preloaded (e.g. it
            // consumed its last data mid-fade or `remaining_seconds` was
            // unavailable). Pull the next track immediately instead of
            // stalling on silence.
            if n_a == 0 && self.active.is_exhausted() && self.next.is_none() {
                if self.provider.has_next() {
                    let (src, label) = self.provider.next_source();
                    log::info!("crossfade: track ended early, advancing to {label}");
                    self.active = src;
                    self.active_label = label;
                    self.reset_tail();
                    continue;
                }
                return 0;
            }

            // Level-aware tail measurement (smart mode): fold the active
            // track's latest audio into the rolling window while no fade is
            // in progress.
            if self.smart.is_some() && self.next.is_none() && n_a > 0 {
                let (sum_sq, count) = {
                    let samples = &self.scratch_a[..n_a];
                    (
                        samples.iter().map(|&s| s as f64 * s as f64).sum::<f64>(),
                        samples.len() as f64,
                    )
                };
                self.accumulate_tail(sum_sq, count);
            }

            // Tail ramp: a crossfade was promoted mid-way; keep fading the
            // new active source in to full gain, frame by frame.
            if let Some(tail) = self.tail.as_mut() {
                let chans = self.channels;
                let frames = n_a / chans;
                for f in 0..frames {
                    let progress = 1.0 - tail.remaining as f64 / tail.total.max(1) as f64;
                    let ramp = tail.start_gain + (1.0 - tail.start_gain) * progress;
                    for ch in 0..chans {
                        buffer[f * chans + ch] =
                            (self.scratch_a[f * chans + ch] as f64 * ramp) as f32;
                    }
                    tail.remaining = tail.remaining.saturating_sub(1);
                }
                if tail.remaining == 0 {
                    self.tail = None;
                }
                return n_a;
            }

            if let Some(next) = self.next.as_mut() {
                let n_b = next.next_buffer(&mut self.scratch_b);

                let out_len = n_a.max(n_b);
                // A source that ended mid-buffer leaves stale samples in
                // the tail of its scratch buffer; zero them so the fade
                // cannot mix a ~one-buffer repeat of earlier audio in.
                if n_a < out_len {
                    self.scratch_a[n_a..out_len].fill(0.0);
                }
                if n_b < out_len {
                    self.scratch_b[n_b..out_len].fill(0.0);
                }
                let chans = self.channels;
                let frames_out = out_len / chans;
                let cf = self.fade_frames.max(1) as f64;
                for (i, out) in buffer.iter_mut().take(out_len).enumerate() {
                    let f = i / chans;
                    let t = ((self.fade_pos + f) as f64 / cf).clamp(0.0, 1.0) as f32;
                    let gain_b = self.curve_gain(t);
                    let gain_a = self.curve_gain(1.0 - t);
                    *out = (self.scratch_a[i] as f64 * gain_a as f64
                        + self.scratch_b[i] as f64 * gain_b as f64)
                        as f32;
                }
                self.fade_pos += frames_out;

                if self.active.is_exhausted() {
                    let promoted = self.next.take().expect("next must exist");
                    self.active = promoted;
                    self.active_label = self.next_label.take().unwrap_or_default();
                    self.reset_tail();
                    if self.fade_pos < self.fade_frames {
                        let remaining = self.fade_frames - self.fade_pos;
                        let t_end = self.fade_pos as f64 / cf;
                        let gain_b = self.curve_gain(t_end as f32);
                        self.tail = Some(Tail {
                            start_gain: gain_b as f64,
                            remaining,
                            total: remaining,
                        });
                    }
                }
                return out_len;
            }

            // No crossfade in progress: plain passthrough.
            buffer[..n_a].copy_from_slice(&self.scratch_a[..n_a]);
            return n_a;
        }
    }

    fn is_exhausted(&self) -> bool {
        if !self.started {
            return !self.provider.has_next();
        }
        self.active.is_exhausted()
            && self.next.as_ref().map(|n| n.is_exhausted()).unwrap_or(true)
            && !self.provider.has_next()
    }

    fn label(&self) -> Option<String> {
        Some(self.active_label.clone())
    }

    fn replaygain_db(&self) -> Option<f32> {
        self.active.replaygain_db()
    }

    fn crossfade_overrides(&self) -> Option<(Option<f64>, Option<f64>)> {
        self.active.crossfade_overrides()
    }

    /// Advance to the next track immediately, abandoning the current one.
    /// Used by the telnet `skip` command.
    fn skip(&mut self) {
        self.ensure_started();
        if self.next.is_some() {
            let promoted = self.next.take().expect("next must exist");
            self.active = promoted;
            self.active_label = self.next_label.take().unwrap_or_default();
            self.reset_tail();
        } else if self.provider.has_next() {
            let (src, label) = self.provider.next_source();
            log::info!("crossfade: skip to {label}");
            self.active = src;
            self.active_label = label;
            self.reset_tail();
        } else {
            log::info!("crossfade: skip ignored, nothing next");
            return;
        }
        self.fade_pos = 0;
        self.tail = None;
        log::info!("crossfade: skipped to {}", self.active_label);
    }
}

/// Commands sent to the [`PriorityMixer`] from the live harbor / jingle trigger.
pub enum MixCommand {
    SetLive(Box<dyn AudioSource>),
    ClearLive,
    PlayJingle(PathBuf),
    /// Skip the current playlist track (telnet `skip`).
    Skip,
    Shutdown,
}

/// Shared engine status consumed by the control port (`status`, `uptime`).
/// The pump loop updates `current`; the telnet server reads it.
#[derive(Clone)]
pub struct StatusHandle {
    started: std::time::Instant,
    current: Arc<std::sync::Mutex<String>>,
    /// True while a live DJ holds the harbor (the playlist is ducked).
    /// Shared with the harbor, which toggles it on connect/disconnect.
    harbor_connected: Arc<AtomicBool>,
}

impl StatusHandle {
    pub fn new() -> Self {
        Self {
            started: std::time::Instant::now(),
            current: Arc::new(Mutex::new(String::new())),
            harbor_connected: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The flag the harbor writes to; hand a clone to the harbor.
    pub fn harbor_flag(&self) -> Arc<AtomicBool> {
        self.harbor_connected.clone()
    }

    pub fn harbor_connected(&self) -> bool {
        self.harbor_connected.load(Ordering::SeqCst)
    }

    pub fn set_current(&self, title: &str) {
        *self.current.lock().unwrap() = title.to_string();
    }

    pub fn current(&self) -> String {
        self.current.lock().unwrap().clone()
    }

    pub fn uptime_seconds(&self) -> u64 {
        self.started.elapsed().as_secs()
    }
}

impl Default for StatusHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// A priority mixer combining the playlist (via [`CrossfadeMixer`]) with live
/// DJ input and one-shot jingles.
///
/// When a DJ connects the output fades from the playlist into the live source
/// over `duck_seconds`; when the DJ disconnects it fades back into whatever the
/// playlist is playing. Jingles fade over the music the same way but lose to a
/// live DJ.
pub struct PriorityMixer {
    main: Box<dyn AudioSource>,
    rx: Receiver<MixCommand>,
    live: Option<Box<dyn AudioSource>>,
    jingle: Option<Box<dyn AudioSource>>,
    /// Current override gain in `[0, 1]` (0 = music, 1 = override).
    gain: f64,
    rising: bool,
    falling: bool,
    override_started: bool,
    duck_step_per_frame: f64,
    target_spec: SignalSpec,
    frames_per_buffer: usize,
    shutdown: bool,
    /// Reusable scratch buffers, sized on demand so `next_buffer` never
    /// allocates on the hot path.
    scratch_m: Vec<f32>,
    scratch_o: Vec<f32>,
}

impl PriorityMixer {
    pub fn new(
        main: Box<dyn AudioSource>,
        rx: Receiver<MixCommand>,
        config: &MixerConfig,
        target_spec: SignalSpec,
        frames_per_buffer: usize,
    ) -> Self {
        Self {
            main,
            rx,
            live: None,
            jingle: None,
            gain: 0.0,
            rising: false,
            falling: false,
            override_started: false,
            duck_step_per_frame: 1.0 / (config.duck_seconds * target_spec.rate as f64).max(1.0),
            target_spec,
            frames_per_buffer,
            shutdown: false,
            scratch_m: Vec::new(),
            scratch_o: Vec::new(),
        }
    }

    /// True after a [`MixCommand::Shutdown`] was received.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown
    }

    fn drain_commands(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(MixCommand::SetLive(src)) => {
                    log::info!("priority mixer: DJ connected");
                    self.live = Some(src);
                    self.override_started = false;
                    self.jingle = None;
                    self.rising = true;
                    self.falling = false;
                }
                Ok(MixCommand::ClearLive) => {
                    log::info!("priority mixer: DJ disconnected");
                    self.rising = false;
                    self.falling = true;
                }
                Ok(MixCommand::PlayJingle(path)) => {
                    if self.live.is_some() {
                        log::info!("priority mixer: ignoring jingle while DJ is live");
                        continue;
                    }
                    match FileSource::open(&path, self.target_spec, self.frames_per_buffer) {
                        Ok(src) => {
                            log::info!("priority mixer: playing jingle {}", path.display());
                            self.jingle = Some(Box::new(src));
                            self.override_started = false;
                            self.rising = true;
                            self.falling = false;
                        }
                        Err(e) => log::warn!("failed to open jingle {}: {e}", path.display()),
                    }
                }
                Ok(MixCommand::Skip) => {
                    log::info!("priority mixer: skip requested");
                    self.main.skip();
                }
                Ok(MixCommand::Shutdown) => {
                    log::info!("priority mixer: shutdown requested");
                    self.shutdown = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    fn step_gain(&mut self, n_out: usize) {
        let frames = n_out / self.target_spec.channels.count();
        let step = self.duck_step_per_frame * frames as f64;
        if self.rising && self.override_started {
            self.gain = (self.gain + step).min(1.0);
            if self.gain >= 1.0 {
                self.rising = false;
            }
        } else if self.falling {
            self.gain = (self.gain - step).max(0.0);
            if self.gain <= 0.0 {
                self.falling = false;
                self.live = None;
                self.jingle = None;
                self.override_started = false;
            }
        }
    }
}

impl AudioSource for PriorityMixer {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        self.drain_commands();
        if self.shutdown {
            return 0;
        }

        let wanted = buffer.len();
        if self.scratch_m.len() != wanted {
            self.scratch_m.resize(wanted, 0.0);
            self.scratch_o.resize(wanted, 0.0);
        }

        let n_m = self.main.next_buffer(&mut self.scratch_m);
        // Pull the override into the shared scratch: `read_override` is
        // inlined so its borrows stay on disjoint fields (live/jingle).
        let n_o = if let Some(src) = self.live.as_mut() {
            src.next_buffer(&mut self.scratch_o)
        } else if let Some(src) = self.jingle.as_mut() {
            src.next_buffer(&mut self.scratch_o)
        } else {
            0
        };
        if n_o > 0 {
            self.override_started = true;
        }

        // An override that has finished fades back out. Jingles end naturally;
        // a live source ends when the DJ disconnects (ClearLive is sent by the
        // harbor, but this also covers any tail of buffered audio still being
        // drained at that point).
        let override_ended = match self.live.as_ref() {
            Some(l) => l.is_exhausted(),
            None => self
                .jingle
                .as_ref()
                .map(|j| j.is_exhausted())
                .unwrap_or(false),
        };
        if override_ended && self.gain > 0.0 && !self.falling {
            self.rising = false;
            self.falling = true;
        }

        let out_len = n_m.max(n_o);
        // Same stale-tail guard as the crossfade: an override that ended
        // mid-buffer must not mix a repeat of its previous buffer in.
        if n_m < out_len {
            self.scratch_m[n_m..out_len].fill(0.0);
        }
        if n_o < out_len {
            self.scratch_o[n_o..out_len].fill(0.0);
        }
        for (i, out) in buffer.iter_mut().take(out_len).enumerate() {
            *out = (self.scratch_m[i] as f64 * (1.0 - self.gain)
                + self.scratch_o[i] as f64 * self.gain) as f32;
        }
        self.step_gain(out_len);
        out_len
    }

    fn is_exhausted(&self) -> bool {
        self.shutdown || self.main.is_exhausted()
    }

    fn label(&self) -> Option<String> {
        let override_live = self.gain > 0.5 && self.live.is_some();
        if override_live {
            Some("LIVE DJ".into())
        } else if let Some(j) = self.jingle.as_ref() {
            if self.gain > 0.5 {
                j.label()
            } else {
                self.main.label()
            }
        } else {
            self.main.label()
        }
    }

    fn replaygain_db(&self) -> Option<f32> {
        // The gain applies to the program material; a live DJ override uses
        // its own loudness.
        self.main.replaygain_db()
    }

    fn crossfade_overrides(&self) -> Option<(Option<f64>, Option<f64>)> {
        self.main.crossfade_overrides()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const RATE: usize = 100;
    const CHANS: usize = 2;

    struct FakeSource {
        value: f32,
        total_frames: usize,
        pos_frames: usize,
        fades: Option<(Option<f64>, Option<f64>)>,
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
        fn remaining_seconds(&self) -> Option<f64> {
            Some((self.total_frames - self.pos_frames) as f64 / RATE as f64)
        }
        fn label(&self) -> Option<String> {
            Some(format!("src({})", self.value))
        }
        fn crossfade_overrides(&self) -> Option<(Option<f64>, Option<f64>)> {
            self.fades
        }
    }

    struct FakeProvider {
        sources: Vec<Box<dyn AudioSource>>,
    }

    impl FakeProvider {
        fn new(values: Vec<(f32, usize)>) -> Self {
            let sources = values
                .into_iter()
                .map(|(v, total)| -> Box<dyn AudioSource> {
                    Box::new(FakeSource {
                        value: v,
                        total_frames: total,
                        pos_frames: 0,
                        fades: None,
                    })
                })
                .collect();
            Self { sources }
        }

        /// Like [`Self::new`], but each source carries a per-track fade
        /// override.
        #[allow(clippy::type_complexity)]
        fn with_fades(values: Vec<(f32, usize, Option<(Option<f64>, Option<f64>)>)>) -> Self {
            let sources = values
                .into_iter()
                .map(|(v, total, fades)| -> Box<dyn AudioSource> {
                    Box::new(FakeSource {
                        value: v,
                        total_frames: total,
                        pos_frames: 0,
                        fades,
                    })
                })
                .collect();
            Self { sources }
        }
    }

    impl SourceProvider for FakeProvider {
        fn next_source(&mut self) -> (Box<dyn AudioSource>, String) {
            let src = self.sources.remove(0);
            let label = src.label().unwrap();
            (src, label)
        }
        fn has_next(&self) -> bool {
            !self.sources.is_empty()
        }
    }

    fn mixer_config(crossfade: f64) -> MixerConfig {
        MixerConfig {
            crossfade_seconds: crossfade,
            fade_curve: 1.0,
            duck_seconds: 1.0,
        }
    }

    #[test]
    fn seamless_crossfade_with_gain_ramp() {
        // Track A: 1.0s (100 frames), track B: value 2.0.
        // Crossfade window: 0.2s (20 frames), buffers of 10 frames.
        let provider = Box::new(FakeProvider::new(vec![(1.0, 100), (2.0, 1000)]));
        let cfg = mixer_config(0.2);
        let mut mix = CrossfadeMixer::new(provider, &cfg, RATE as u32, CHANS);

        let mut buf = vec![0f32; 10 * CHANS];
        let n = mix.next_buffer(&mut buf);
        assert_eq!(n, 20);

        // Buffer 1: remaining 0.9s > 0.2s -> passthrough A.
        assert!((buf[0] - 1.0).abs() < 1e-6);

        // Buffers 2..=8: passthrough A (remaining 0.8 -> 0.2).
        for _ in 0..7 {
            mix.next_buffer(&mut buf);
        }
        assert!((buf[0] - 1.0).abs() < 1e-6);

        // Buffer 9: remaining == 0.2s -> preload B, fade begins (t=0).
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-6);

        // Buffer 10: t=0.5 -> 0.5*A + 0.5*B = 1.5.
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 1.5).abs() < 1e-6);

        // Buffer 11: t=1.0 -> B only = 2.0, A now exhausted and promoted.
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 2.0).abs() < 1e-6);

        // Buffer 12: B continues at full gain.
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 2.0).abs() < 1e-6);
    }

    #[test]
    fn tail_ramp_when_track_ends_mid_fade() {
        // A is only one buffer long (10 frames) while the crossfade window is
        // 20 frames, so it is promoted while the fade is only half done.
        let provider = Box::new(FakeProvider::new(vec![(1.0, 10), (2.0, 1000)]));
        let cfg = mixer_config(0.2);
        let mut mix = CrossfadeMixer::new(provider, &cfg, RATE as u32, CHANS);

        let mut buf = vec![0f32; 10 * CHANS];
        mix.next_buffer(&mut buf);
        // A.remaining = 0.1s <= 0.2s -> preload B. Frame 0: t=0 -> 1.0.
        assert!((buf[0] - 1.0).abs() < 1e-6);
        // Frame 9 (samples 18/19): t=0.45 -> 0.55*A + 0.45*B = 0.55 + 0.90 = 1.45.
        assert!((buf[18] - 1.45).abs() < 1e-6);
        assert!((buf[19] - 1.45).abs() < 1e-6);
        // A exhausted -> promoted with tail(start_gain=0.5, remaining=10).

        // Tail ramp 0.5 -> 1.0 across the remaining 10 frames.
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-6); // 2.0 * 0.5
        assert!((buf[18] - 1.9).abs() < 1e-6); // 2.0 * (0.5 + 0.5*0.9)
        assert!((buf[19] - 1.9).abs() < 1e-6);

        // B at full gain.
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 2.0).abs() < 1e-6);
    }

    #[test]
    fn priority_mixer_fades_dj_in_and_out() {
        let (tx, rx) = mpsc::channel();
        let main = Box::new(FakeSource {
            value: 1.0,
            total_frames: 100_000,
            pos_frames: 0,
            fades: None,
        });
        let cfg = mixer_config(0.1);
        let cfg = MixerConfig {
            duck_seconds: 0.1,
            ..cfg
        };
        let spec = symphonia::core::audio::SignalSpec::new(
            RATE as u32,
            symphonia::core::audio::Channels::FRONT_LEFT
                | symphonia::core::audio::Channels::FRONT_RIGHT,
        );
        let mut pm = PriorityMixer::new(main, rx, &cfg, spec, 10);

        // Main only.
        let mut buf = vec![0f32; 10 * CHANS];
        pm.next_buffer(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-6);

        // DJ connects (value 3.0).
        let dj = FakeSource {
            value: 3.0,
            total_frames: 100_000,
            pos_frames: 0,
            fades: None,
        };
        tx.send(MixCommand::SetLive(Box::new(dj))).unwrap();

        // duck_seconds=0.1 -> duck_frames=10, one buffer is 10 frames.
        // The gain is stepped *after* mixing, so the first buffer after the
        // command still mixes at gain 0 (playlist), then full takeover.
        pm.next_buffer(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-6);
        pm.next_buffer(&mut buf);
        assert!((buf[0] - 3.0).abs() < 1e-6);

        // DJ disconnects: the buffer after ClearLive still mixes at gain 1.0
        // (pre-step), then the gain drops to 0 and the playlist returns.
        tx.send(MixCommand::ClearLive).unwrap();
        pm.next_buffer(&mut buf);
        assert!((buf[0] - 3.0).abs() < 1e-6);
        pm.next_buffer(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn skip_advances_to_the_next_track() {
        let provider = Box::new(FakeProvider::new(vec![(1.0, 1000), (2.0, 1000)]));
        let cfg = mixer_config(0.2);
        let mut mix = CrossfadeMixer::new(provider, &cfg, RATE as u32, CHANS);
        let mut buf = vec![0f32; 10 * CHANS];
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-6);

        mix.skip();
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 2.0).abs() < 1e-6);
        assert_eq!(mix.label().as_deref(), Some("src(2)"));
    }

    #[test]
    fn skip_is_a_noop_with_nothing_next() {
        let provider = Box::new(FakeProvider::new(vec![(1.0, 10)]));
        let cfg = mixer_config(0.2);
        let mut mix = CrossfadeMixer::new(provider, &cfg, RATE as u32, CHANS);
        mix.skip();
        let mut buf = vec![0f32; 10 * CHANS];
        assert_eq!(mix.next_buffer(&mut buf), 20);
        assert!((buf[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn priority_mixer_forwards_skip_to_the_crossfade() {
        let provider = Box::new(FakeProvider::new(vec![(1.0, 1000), (2.0, 1000)]));
        let cfg = mixer_config(0.2);
        let cross = CrossfadeMixer::new(provider, &cfg, RATE as u32, CHANS);
        let (tx, rx) = mpsc::channel();
        let spec = symphonia::core::audio::SignalSpec::new(
            RATE as u32,
            symphonia::core::audio::Channels::FRONT_LEFT
                | symphonia::core::audio::Channels::FRONT_RIGHT,
        );
        let mut pm = PriorityMixer::new(Box::new(cross), rx, &cfg, spec, 10);
        let mut buf = vec![0f32; 10 * CHANS];
        pm.next_buffer(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-6);

        tx.send(MixCommand::Skip).unwrap();
        pm.next_buffer(&mut buf);
        assert!((buf[0] - 2.0).abs() < 1e-6);
    }

    #[test]
    fn fade_out_override_moves_the_preload_and_lengthens_the_fade() {
        // Global window is 0.2s (20 frames). Track A overrides fade_out to
        // 0.4s, so the next track is preloaded 0.4s early (not 0.2s) and the
        // overlap spans 40 frames. Buffers are 10 frames (0.1s).
        let provider = Box::new(FakeProvider::with_fades(vec![
            (1.0, 1000, Some((None, Some(0.4)))),
            (2.0, 1000, None),
        ]));
        let cfg = mixer_config(0.2);
        let mut mix = CrossfadeMixer::new(provider, &cfg, RATE as u32, CHANS);

        let mut buf = vec![0f32; 10 * CHANS];
        // Buffers 1..=96: A has more than 0.4s left -> passthrough.
        for _ in 0..96 {
            mix.next_buffer(&mut buf);
            assert!((buf[0] - 1.0).abs() < 1e-6);
        }
        // Buffer 97: A has 0.4s left -> preload, fade begins (t=0).
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-6);
        // Buffer 98: t=0.25 -> 0.75*A + 0.25*B = 1.25. (With the global 20
        // frame window this would already be t=0.5 -> 1.5.)
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 1.25).abs() < 1e-6);
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 1.5).abs() < 1e-6);
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 1.75).abs() < 1e-6);
        // Buffer 101: fade complete (40 frames), B at full gain.
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 2.0).abs() < 1e-6);
    }

    #[test]
    fn incoming_fade_in_override_lengthens_the_fade_into_a_tail_ramp() {
        // Track B overrides fade_in to 0.4s while A keeps the global 0.2s
        // margin: the fade window is 40 frames but A only has 20 frames left
        // when it starts, so the remainder is finished by the tail ramp.
        let provider = Box::new(FakeProvider::with_fades(vec![
            (1.0, 1000, None),
            (2.0, 1000, Some((Some(0.4), None))),
        ]));
        let cfg = mixer_config(0.2);
        let mut mix = CrossfadeMixer::new(provider, &cfg, RATE as u32, CHANS);

        let mut buf = vec![0f32; 10 * CHANS];
        for _ in 0..98 {
            mix.next_buffer(&mut buf);
        }
        // Buffer 99: A has 0.2s left -> preload, fade begins (t=0).
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-6);
        // Buffer 100: t=0.25 -> 1.25 (40-frame window; a 20-frame window
        // would give 1.5). A exhausts here -> promoted mid-fade.
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 1.25).abs() < 1e-6);
        // Buffer 101: tail ramp starts at gain 0.5 (t=0.5 when promoted).
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-6);
        // Tail finished, B at full gain.
        mix.next_buffer(&mut buf);
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 2.0).abs() < 1e-6);
    }

    #[test]
    fn loud_tail_gets_the_full_smart_crossfade_window() {
        // Track A is loud (0 dBFS): the tail measurement picks the full
        // `fade_out` window (0.4s = 40 frames at RATE 100), not `fade_mid`.
        let provider = Box::new(FakeProvider::new(vec![(1.0, 1000), (2.0, 1000)]));
        let cfg = mixer_config(0.2);
        let mix = CrossfadeMixer::new(provider, &cfg, RATE as u32, CHANS);
        let mut mix = mix.with_smart_fade(SmartFade {
            fade_out: 0.4,
            fade_mid: 0.1,
            threshold_db: -30.0,
        });

        let mut buf = vec![0f32; 10 * CHANS];
        // Buffers 1..=96: A has more than 0.4s left -> passthrough.
        for _ in 0..96 {
            mix.next_buffer(&mut buf);
            assert!((buf[0] - 1.0).abs() < 1e-6);
        }
        // Buffer 97: A has 0.4s left -> preload, fade begins (t=0). The
        // loud tail picked the full 40-frame window, so the ramp matches
        // the explicit fade_out=0.4 override case exactly.
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-6);
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 1.25).abs() < 1e-6);
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 1.5).abs() < 1e-6);
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 1.75).abs() < 1e-6);
        // Buffer 101: fade complete (40 frames), B at full gain.
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 2.0).abs() < 1e-6);
    }

    #[test]
    fn quiet_tail_shortens_to_the_smart_fade_mid_window() {
        // Track A is quiet (-40 dBFS): the fade window collapses to
        // `fade_mid` (0.1s = 10 frames) even though the preload margin
        // stays at `fade_out` (0.4s), so the transition is done a full
        // buffer earlier than a loud tail would give.
        let provider = Box::new(FakeProvider::new(vec![(0.01, 1000), (2.0, 1000)]));
        let cfg = mixer_config(0.2);
        let mix = CrossfadeMixer::new(provider, &cfg, RATE as u32, CHANS);
        let mut mix = mix.with_smart_fade(SmartFade {
            fade_out: 0.4,
            fade_mid: 0.1,
            threshold_db: -30.0,
        });

        let mut buf = vec![0f32; 10 * CHANS];
        for _ in 0..96 {
            mix.next_buffer(&mut buf);
            assert!((buf[0] - 0.01).abs() < 1e-6);
        }
        // Buffer 97: preload fires at the 0.4s margin, but the quiet tail
        // means the fade spans only 10 frames: t ramps 0 -> 1 in this one
        // buffer.
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 0.01).abs() < 1e-6); // t=0: A only
        assert!((buf[18] - 1.801).abs() < 1e-4); // t=0.9: 0.1*A + 0.9*B
        // Buffer 98: fade complete -> B at full gain (a loud tail would
        // still be at t=0.25 -> 0.25*0.01 + 0.75*2.0 = 1.5025 here).
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 2.0).abs() < 1e-6);
    }

    #[test]
    fn partial_final_buffer_of_a_does_not_repeat_stale_audio() {
        // A is 15 frames: buffer 1 pulls a full 10, buffer 2 pulls the
        // partial 5-frame tail while B fills the whole 10-frame buffer.
        // Without the stale-tail guard, the fade would mix A's previous
        // buffer into frames 5..10 as a repeat.
        let provider = Box::new(FakeProvider::new(vec![(1.0, 15), (2.0, 1000)]));
        let cfg = mixer_config(0.2);
        let mut mix = CrossfadeMixer::new(provider, &cfg, RATE as u32, CHANS);

        let mut buf = vec![0f32; 10 * CHANS];
        // Buffer 1: preload fires (A has 0.15s <= 0.2s), fade t=0..0.5.
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-6);
        assert!((buf[18] - 1.45).abs() < 1e-6); // t=0.45: 0.55 + 0.90

        // Buffer 2: n_a = 5, n_b = 10, out_len = 10. Frames 5..9 must be
        // B's fade-in only (2.0 * gain_b) — a stale repeat of A would add
        // (1 - gain_b) * 1.0, e.g. frame 5: 0.25 + 1.5 = 1.75 instead of 1.5.
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 1.5).abs() < 1e-6); // frame 0: t=0.5
        assert!((buf[8] - 1.7).abs() < 1e-6); // frame 4: t=0.7, A still live
        assert!((buf[10] - 1.5).abs() < 1e-6); // frame 5: t=0.75 -> B only
        assert!((buf[19] - 1.9).abs() < 1e-6); // frame 9: t=0.95 -> B only

        // A exhausted, fade complete (20 frames) -> B at full gain.
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 2.0).abs() < 1e-6);
    }

    #[test]
    fn partial_final_buffer_of_override_does_not_repeat_stale_audio() {
        // The DJ override is 15 frames: full buffer, then a 5-frame tail at
        // full duck gain. Without the stale-tail guard, the tail's second
        // half would repeat the override's first buffer at volume 3.0.
        let (tx, rx) = mpsc::channel();
        let main = Box::new(FakeSource {
            value: 1.0,
            total_frames: 100_000,
            pos_frames: 0,
            fades: None,
        });
        let cfg = mixer_config(0.2);
        let cfg = MixerConfig {
            duck_seconds: 0.1, // duck_frames = 10 -> gain reaches 1.0 in one buffer
            ..cfg
        };
        let spec = symphonia::core::audio::SignalSpec::new(
            RATE as u32,
            symphonia::core::audio::Channels::FRONT_LEFT
                | symphonia::core::audio::Channels::FRONT_RIGHT,
        );
        let mut pm = PriorityMixer::new(main, rx, &cfg, spec, 10);
        let dj = FakeSource {
            value: 3.0,
            total_frames: 15,
            pos_frames: 0,
            fades: None,
        };
        tx.send(MixCommand::SetLive(Box::new(dj))).unwrap();

        let mut buf = vec![0f32; 10 * CHANS];
        // Buffer 1: gain 0 (stepped after mixing) -> main.
        pm.next_buffer(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-6);
        // Buffer 2: gain 1.0; override pulls only 5 frames. Frames 5..9
        // must be silent (main ducked, override gone) — without the guard
        // they would repeat the override at 3.0.
        pm.next_buffer(&mut buf);
        assert!((buf[0] - 3.0).abs() < 1e-6);
        assert!((buf[9] - 3.0).abs() < 1e-6);
        assert!((buf[10] - 0.0).abs() < 1e-6);
        assert!((buf[19] - 0.0).abs() < 1e-6);
        // Buffer 3: override ended, gain falls back -> main returns.
        pm.next_buffer(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn status_handle_reports_current_and_uptime() {
        let status = StatusHandle::new();
        status.set_current("a track");
        assert_eq!(status.current(), "a track");
        assert!(status.uptime_seconds() < 1000);
    }

    #[test]
    fn shutdown_ends_the_stream() {
        let main = Box::new(FakeSource {
            value: 1.0,
            total_frames: 100_000,
            pos_frames: 0,
            fades: None,
        });
        let cfg = mixer_config(0.2);
        let spec = symphonia::core::audio::SignalSpec::new(
            RATE as u32,
            symphonia::core::audio::Channels::FRONT_LEFT
                | symphonia::core::audio::Channels::FRONT_RIGHT,
        );
        let (tx, rx) = mpsc::channel();
        let mut pm = PriorityMixer::new(main, rx, &cfg, spec, 10);
        let mut buf = vec![0f32; 10 * CHANS];
        assert_eq!(pm.next_buffer(&mut buf), 20);

        tx.send(MixCommand::Shutdown).unwrap();
        assert_eq!(pm.next_buffer(&mut buf), 0);
        assert!(pm.is_exhausted());
        assert!(pm.is_shutdown());
    }

    #[test]
    fn plays_a_real_jingle_file() {
        use std::path::PathBuf;
        let jingle = PathBuf::from("jingles/mrwashingt0n-radio-for-all-trance-505921.mp3");
        if !jingle.exists() {
            return; // not available in CI-less env; skip
        }
        let provider = Box::new(FakeProvider::new(vec![(0.5, 100000)]));
        let cfg = mixer_config(0.2);
        let cross = CrossfadeMixer::new(provider, &cfg, RATE as u32, CHANS);
        let (tx, rx) = mpsc::channel();
        let spec = symphonia::core::audio::SignalSpec::new(
            RATE as u32,
            symphonia::core::audio::Channels::FRONT_LEFT
                | symphonia::core::audio::Channels::FRONT_RIGHT,
        );
        let mut pm = PriorityMixer::new(Box::new(cross), rx, &cfg, spec, 100);
        tx.send(MixCommand::PlayJingle(jingle)).unwrap();
        // Duck ramp is 1.0s (10 buffers), jingle is ~12s (124 buffers):
        // buffers 20..120 are pure jingle (music fully ducked).
        let mut buf = vec![0f32; 100 * CHANS];
        let mut seen_label = false;
        for i in 0..140 {
            pm.next_buffer(&mut buf);
            if let Some(l) = pm.label()
                && l.contains("mrwashingt0n")
            {
                seen_label = true;
            }
            if (20..120).contains(&i) {
                let e: f32 = buf.iter().map(|v| v * v).sum::<f32>() / buf.len() as f32;
                if i == 40 {
                    assert!(e > 0.01, "jingle window silent (energy {e})");
                }
            }
        }
        assert!(seen_label, "mixer never reached jingle label");
    }

    #[test]
    fn jingle_reaches_the_opus_encoder_end_to_end() {
        use std::path::PathBuf;
        let jingle = PathBuf::from("jingles/mrwashingt0n-radio-for-all-trance-505921.mp3");
        if !jingle.exists() {
            return; // repo-checkout only
        }
        // Playlist of the real media dir -> crossfade -> priority mixer -> opus.
        let spec = symphonia::core::audio::SignalSpec::new(
            44100,
            symphonia::core::audio::Channels::FRONT_LEFT
                | symphonia::core::audio::Channels::FRONT_RIGHT,
        );
        let media = std::path::Path::new("media");
        let mut files: Vec<_> = std::fs::read_dir(media)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "mp3").unwrap_or(false))
            .collect();
        files.sort();
        let files: Vec<_> = files
            .into_iter()
            .map(|p| crate::request::RequestUri::Local(p, None))
            .collect();
        let playlist = crate::source::playlist::Playlist::new(
            files,
            false,
            true,
            crate::request::RequestConfig::default(),
            spec,
            4096,
            None,
        );
        let cfg = mixer_config(0.2);
        let cross = CrossfadeMixer::new(Box::new(playlist), &cfg, 44100, 2);
        let (tx, rx) = mpsc::channel();
        let mut pm = PriorityMixer::new(Box::new(cross), rx, &cfg, spec, 4096);
        tx.send(MixCommand::PlayJingle(jingle)).unwrap();

        let mut enc = crate::output::encoder::create_encoder(
            crate::config::OutputFormat::Opus,
            44100,
            2,
            128_000,
            "e2e",
        )
        .unwrap();
        let mut buf = vec![0f32; 4096 * 2];
        let mut all = Vec::new();
        let mut jingle_audio = false;
        for _ in 0..600 {
            pm.next_buffer(&mut buf);
            let out = enc.encode(&buf);
            if out.is_empty() {
                continue;
            }
            let jingle_active = pm
                .label()
                .map(|l| l.contains("mrwashingt0n"))
                .unwrap_or(false);
            if jingle_active {
                let e: f32 = buf.iter().map(|v| v * v).sum::<f32>() / buf.len() as f32;
                if e > 0.005 {
                    jingle_audio = true;
                }
            }
            all.extend_from_slice(&out);
        }
        all.extend_from_slice(&enc.finish());
        assert!(jingle_audio, "no audible jingle audio reached the encoder");
        assert!(all.len() > 100_000);
        if let Some(out) = std::env::var_os("CRABSOUP_DUMP") {
            std::fs::write(out, &all).unwrap();
        }
    }
}
