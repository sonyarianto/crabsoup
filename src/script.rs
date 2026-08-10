//! Liquidsoap-style scripting via Lua (`crabsoup.lua`).
//!
//! The script is Lua: it builds a source graph and configures services,
//! mirroring Liquidsoap's model. All configuration (stream spec, mixer,
//! outputs, services) flows through this layer — there is no YAML anymore.
//!
//! ```lua
//! set("sample_rate", 44100)
//! set("crossfade_seconds", 3.0)
//!
//! pl = playlist({directory = "./media", shuffle = true})
//! j  = jingles({directory = "./jingles"})
//! live = input.harbor({mount = "/live", password = "dj"})
//! server.telnet({port = 1234})
//!
//! output.icecast({host = "localhost", mount = "/crabsoup.ogg",
//!                 format = "opus", bitrate = 128000,
//!                 password = "hackme"}, fallback({j, pl}))
//! ```
//!
//! Sources are first-class Lua values (userdata wrapping
//! `Arc<Mutex<Box<dyn AudioSource>>>`, so a source can be composed into
//! several graphs). `output.icecast`/`output.preview` choose the root source
//! of the engine chain.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mlua::{FromLua, Lua, Table, UserData, Value as LValue};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use symphonia::core::audio::SignalSpec;

use crate::config::{
    collect_audio, ControlConfig, FileOutputConfig, HlsOutputConfig, LiveConfig, MixerConfig,
    OutputConfig, OutputFormat, StreamConfig,
};
use crate::engine::effects::{Agc, Amplify, Compressor, EffectSource};
use crate::engine::mixer::CrossfadeMixer;
use crate::request::{resolve, RequestConfig, RequestUri};
use crate::source::playlist::Playlist;
use crate::source::replaygain::ReplayGainSource;
use crate::source::request::{RequestQueue, RequestQueueSource};
use crate::source::{AudioSource, BlankSource, SilenceSource, SineSource};

/// Everything the engine needs after a `.lua` script finishes evaluating.
pub struct ScriptResult {
    pub stream: StreamConfig,
    pub mixer: MixerConfig,
    pub jingles: Vec<PathBuf>,
    pub harbor: Option<LiveConfig>,
    pub control: Option<ControlConfig>,
    pub outputs: Vec<OutputConfig>,
    pub file_outputs: Vec<FileOutputConfig>,
    pub hls_outputs: Vec<HlsOutputConfig>,
    /// Shared state of the `request.queue` source, handed to the telnet
    /// server for `queue.push`/`queue.list`/`queue.clear`/`queue.skip`.
    pub request_queue: Option<Arc<RequestQueue>>,
    /// Names of `server.register` commands, in registration order (same
    /// order as the runtime's `custom_commands`), for the control port's
    /// routing table.
    pub custom_commands: Vec<String>,
    /// The engine's root source, taken from `output.icecast`.
    pub root: Option<Box<dyn AudioSource>>,
    /// The root source from `output.preview` (used when no icecast output).
    pub preview: Option<Box<dyn AudioSource>>,
}

/// State mutated by `set()` calls and populated by service constructors.
#[derive(Default)]
struct ScriptState {
    stream: StreamConfig,
    mixer: MixerConfig,
    /// Request/download settings for URI-resolving sources.
    request: RequestConfig,
    jingles: Vec<PathBuf>,
    harbor: Option<LiveConfig>,
    control: Option<ControlConfig>,
    outputs: Vec<OutputConfig>,
    file_outputs: Vec<FileOutputConfig>,
    hls_outputs: Vec<HlsOutputConfig>,
    request_queue: Option<Arc<RequestQueue>>,
    /// Named telnet commands registered by `server.register(name, fn)`;
    /// the names mirror into `ScriptResult` for the control port.
    custom_commands: Vec<(String, mlua::Function)>,
    /// The shared root source graph. First `output.icecast` call steals the
    /// box; later calls must pass the same `Arc` (checked via `ptr_eq`).
    root: Option<Box<dyn AudioSource>>,
    root_arc: Option<Arc<Mutex<Box<dyn AudioSource>>>>,
    preview: Option<Box<dyn AudioSource>>,
    /// Lua callbacks registered by `on_metadata`; indexed by hook id, live
    /// on the Lua-owning thread only.
    metadata_hooks: Vec<mlua::Function>,
    /// Lua callbacks registered by `on_track`; indexed by hook id. Kept
    /// separate from `metadata_hooks` so a `Track` event never calls an
    /// `on_metadata` callback (and vice versa).
    track_hooks: Vec<mlua::Function>,
}

/// Events sent from engine threads to the Lua-owning thread. The payload is
/// always owned and `Send` — `Lua`/`Function`/`Table` never cross threads.
pub enum ScriptEvent {
    /// A source wrapped by `on_metadata` observed a track-label change.
    Metadata { hook_id: usize, title: String },
    /// A source wrapped by `on_track` started a new track (boundary
    /// detected, metadata not required).
    Track { hook_id: usize },
    /// A telnet `server.register` command: run the Lua handler with `args`
    /// and send the reply back to the control port (which blocks on it).
    Custom {
        index: usize,
        args: String,
        reply: mpsc::Sender<Result<String, String>>,
    },
    /// Request the engine stop (future `server.register` use).
    Shutdown,
}

/// The evaluated script's runtime: the `Lua` instance (which now outlives
/// script evaluation) and the event loop that invokes callbacks on it.
/// Must stay on the thread that created it.
pub struct ScriptRuntime {
    pub lua: Lua,
    event_rx: mpsc::Receiver<ScriptEvent>,
    event_tx: mpsc::Sender<ScriptEvent>,
    metadata_hooks: Vec<mlua::Function>,
    track_hooks: Vec<mlua::Function>,
    custom_commands: Vec<(String, mlua::Function)>,
}

impl ScriptRuntime {
    /// Read a Lua global (used by tests; later `server.register`).
    pub fn global<T: mlua::FromLua>(&self, name: &str) -> mlua::Result<T> {
        self.lua.globals().get(name)
    }

    /// Sender used by the control port to ask the event loop to run a
    /// `server.register` handler (and by tests).
    pub fn event_tx(&self) -> mpsc::Sender<ScriptEvent> {
        self.event_tx.clone()
    }

    /// Drive the Lua-owning event loop until the engine signals completion.
    ///
    /// The channel never disconnects on its own while the script runtime is
    /// alive (the `on_metadata` closure in the Lua registry keeps a `Sender`
    /// for the whole process), so the loop polls with a timeout and exits
    /// when `end` is set, on a [`ScriptEvent::Shutdown`], or when every
    /// sender really is gone. Call only from the thread that owns `Lua`.
    pub fn run_event_loop(&self, end: &std::sync::atomic::AtomicBool) {
        use std::sync::mpsc::RecvTimeoutError;
        while !end.load(std::sync::atomic::Ordering::SeqCst) {
            match self.event_rx.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => self.handle_event(event, end),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    /// Process any already-pending events without blocking (tests).
    pub fn drain_metadata(&self) {
        while let Ok(event) = self.event_rx.try_recv() {
            self.handle_event(event, &std::sync::atomic::AtomicBool::new(false));
        }
    }

    fn handle_event(&self, event: ScriptEvent, end: &std::sync::atomic::AtomicBool) {
        match event {
            ScriptEvent::Metadata { hook_id, title } => {
                let Some(cb) = self.metadata_hooks.get(hook_id) else {
                    return;
                };
                let table = match self.lua.create_table() {
                    Ok(t) => t,
                    Err(e) => {
                        log::warn!("on_metadata callback: table error: {e}");
                        return;
                    }
                };
                if let Err(e) = table.set("title", title.as_str()) {
                    log::warn!("on_metadata callback: {e}");
                    return;
                }
                if let Err(e) = cb.call::<()>(table) {
                    log::warn!("on_metadata callback error: {e}");
                }
            }
            ScriptEvent::Track { hook_id } => {
                let Some(cb) = self.track_hooks.get(hook_id) else {
                    return;
                };
                if let Err(e) = cb.call::<()>(()) {
                    log::warn!("on_track callback error: {e}");
                }
            }
            ScriptEvent::Custom { index, args, reply } => {
                let Some((_, cb)) = self.custom_commands.get(index) else {
                    let _ = reply.send(Err(format!("no such custom command ({index})")));
                    return;
                };
                match cb.call::<String>(args) {
                    Ok(text) => {
                        let _ = reply.send(Ok(text));
                    }
                    Err(e) => {
                        log::warn!("custom command callback error: {e}");
                        let _ = reply.send(Err(e.to_string()));
                    }
                }
            }
            ScriptEvent::Shutdown => {
                end.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
}

/// Wrapper source that emits [`ScriptEvent::Metadata`] when its child's
/// label changes. Only the label string crosses the channel; the callback
/// itself stays on the Lua-owning thread.
struct OnMetadataSource {
    child: Box<dyn AudioSource>,
    tx: mpsc::Sender<ScriptEvent>,
    hook_id: usize,
    last_label: Option<String>,
}

impl OnMetadataSource {
    fn new(child: Box<dyn AudioSource>, tx: mpsc::Sender<ScriptEvent>, hook_id: usize) -> Self {
        Self {
            child,
            tx,
            hook_id,
            last_label: None,
        }
    }
}

impl AudioSource for OnMetadataSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        let n = self.child.next_buffer(buffer);
        let label = self.child.label();
        if label != self.last_label {
            if let Some(title) = &label {
                let _ = self.tx.send(ScriptEvent::Metadata {
                    hook_id: self.hook_id,
                    title: title.clone(),
                });
            }
            self.last_label = label;
        }
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

    fn skip(&mut self) {
        self.child.skip();
    }
}

/// Wrapper source that emits [`ScriptEvent::Track`] at track boundaries —
/// the child's label changed, or it produces audio again after having been
/// silent (exhausted or paused). Unlike [`OnMetadataSource`], a boundary is
/// reported even when the new track carries no label.
struct OnTrackSource {
    child: Box<dyn AudioSource>,
    tx: mpsc::Sender<ScriptEvent>,
    hook_id: usize,
    last_label: Option<String>,
    /// The child returned no audio since the last boundary.
    silent: bool,
}

impl OnTrackSource {
    fn new(child: Box<dyn AudioSource>, tx: mpsc::Sender<ScriptEvent>, hook_id: usize) -> Self {
        Self {
            child,
            tx,
            hook_id,
            last_label: None,
            silent: false,
        }
    }
}

impl AudioSource for OnTrackSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        let n = self.child.next_buffer(buffer);
        if n == 0 {
            // Silence: remember it, but only report the boundary when audio
            // comes back (a permanently exhausted child is no new track).
            if !self.child.is_exhausted() {
                self.silent = true;
            }
            return n;
        }
        if self.silent || self.last_label != self.child.label() {
            let _ = self.tx.send(ScriptEvent::Track {
                hook_id: self.hook_id,
            });
        }
        self.last_label = self.child.label();
        self.silent = false;
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

    fn skip(&mut self) {
        self.child.skip();
    }
}

impl ScriptState {
    fn set(&mut self, key: &str, value: LValue) -> Result<(), mlua::Error> {
        macro_rules! num {
            ($field:expr, $target:ty) => {
                match value {
                    LValue::Integer(i) => $field = i as $target,
                    LValue::Number(f) => $field = f as $target,
                    _ => {
                        return Err(mlua::Error::runtime(format!(
                            "set(\"{key}\") expects a number"
                        )))
                    }
                }
            };
        }
        match key {
            "sample_rate" => num!(self.stream.sample_rate, u32),
            "channels" => num!(self.stream.channels, u16),
            "frames_per_buffer" => num!(self.stream.frames_per_buffer, usize),
            "crossfade_seconds" => num!(self.mixer.crossfade_seconds, f64),
            "fade_curve" => num!(self.mixer.fade_curve, f64),
            "duck_seconds" => num!(self.mixer.duck_seconds, f64),
            "request_timeout" => num!(self.request.timeout_secs, u64),
            "request_retries" => num!(self.request.retries, u32),
            other => {
                return Err(mlua::Error::runtime(format!("unknown setting \"{other}\"")))
            }
        }
        Ok(())
    }
}

/// A `Box<dyn AudioSource>` as a first-class, cloneable Lua value.
#[derive(Clone)]
struct LuaSource(Arc<Mutex<Box<dyn AudioSource>>>);

impl LuaSource {
    fn new(src: Box<dyn AudioSource>) -> Self {
        Self(Arc::new(Mutex::new(src)))
    }

    /// Steal the wrapped source, leaving silence in its place. Works even if
    /// the value is still shared with other Lua references: the first
    /// `output.icecast` call consumes the source; later calls only share the
    /// same `Arc` via the engine tap.
    fn take(&mut self) -> Box<dyn AudioSource> {
        let mut guard = self.0.lock().unwrap();
        std::mem::replace(&mut *guard, Box::new(SilenceSource::new()))
    }
}

impl UserData for LuaSource {}

impl FromLua for LuaSource {
    fn from_lua(value: LValue, _lua: &Lua) -> mlua::Result<Self> {
        match value {
            LValue::UserData(ud) => ud
                .borrow_mut::<LuaSource>()
                .map(|m| LuaSource(m.0.clone()))
                .map_err(mlua::Error::runtime),
            _ => Err(mlua::Error::runtime("expected a source")),
        }
    }
}

/// Selects children in script order: the first available child wins, and
/// availability is re-checked from the top on every pull (Liquidsoap's
/// `fallback` / `sequence`). A child that exhausts is skipped forever; a
/// child that becomes available again later (a `request.queue` receiving a
/// push) preempts the current one.
struct FallbackSource {
    children: Vec<LuaSource>,
    current: usize,
}

impl FallbackSource {
    fn new(children: Vec<LuaSource>) -> Self {
        Self { children, current: 0 }
    }

    /// Index of the first child that can still produce audio.
    fn active(&self) -> Option<usize> {
        (0..self.children.len()).find(|&i| {
            let child = self.children[i].0.lock().unwrap();
            !child.is_exhausted()
        })
    }
}

impl AudioSource for FallbackSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        for i in 0..self.children.len() {
            let n = self.children[i].0.lock().unwrap().next_buffer(buffer);
            if n > 0 {
                self.current = i;
                return n;
            }
            if !self.children[i].0.lock().unwrap().is_exhausted() {
                // First available child is temporarily silent: wait for it.
                return 0;
            }
        }
        0
    }

    fn is_exhausted(&self) -> bool {
        self.active().is_none()
    }

    fn remaining_seconds(&self) -> Option<f64> {
        self.active()
            .and_then(|i| self.children[i].0.lock().unwrap().remaining_seconds())
    }

    fn label(&self) -> Option<String> {
        if let Some(i) = self.active() {
            return self
                .children[i]
                .0
                .lock()
                .unwrap()
                .label()
                .or_else(|| Some("(no source)".into()));
        }
        // Everything exhausted: keep reporting the last track's label so
        // metadata hooks do not see a spurious "(no source)" transition.
        self.children
            .get(self.current)
            .and_then(|c| c.0.lock().unwrap().label())
            .or_else(|| Some("(no source)".into()))
    }

    fn replaygain_db(&self) -> Option<f32> {
        if let Some(i) = self.active() {
            return self.children[i].0.lock().unwrap().replaygain_db();
        }
        self.children
            .get(self.current)
            .and_then(|c| c.0.lock().unwrap().replaygain_db())
    }
}

/// Picks a random remaining child each time one ends (Liquidsoap's `random`).
struct RandomSource {
    children: Vec<LuaSource>,
    /// Shuffled indices of children not yet exhausted.
    order: Vec<usize>,
    rng: SmallRng,
}

impl RandomSource {
    fn new(children: Vec<LuaSource>) -> Self {
        let mut this = Self {
            children,
            order: Vec::new(),
            rng: SmallRng::from_entropy(),
        };
        this.refill();
        this
    }

    fn refill(&mut self) {
        let mut order: Vec<usize> = (0..self.children.len()).collect();
        for i in (1..order.len()).rev() {
            let j = self.rng.gen_range(0..=i);
            order.swap(i, j);
        }
        self.order = order;
    }
}

impl AudioSource for RandomSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        loop {
            if self.order.is_empty() {
                self.refill();
            }
            let idx = self.order[0];
            let n = self.children[idx].0.lock().unwrap().next_buffer(buffer);
            if n > 0 {
                return n;
            }
            if self.children[idx].0.lock().unwrap().is_exhausted() {
                log::debug!("random: child {} done, picking another", idx);
                self.order.remove(0);
                continue;
            }
            return 0;
        }
    }

    fn is_exhausted(&self) -> bool {
        false
    }

    fn remaining_seconds(&self) -> Option<f64> {
        self.order
            .first()
            .and_then(|&i| self.children[i].0.lock().unwrap().remaining_seconds())
    }

    fn label(&self) -> Option<String> {
        self.order
            .first()
            .and_then(|&i| self.children[i].0.lock().unwrap().label())
    }

    fn replaygain_db(&self) -> Option<f32> {
        self.order
            .first()
            .and_then(|&i| self.children[i].0.lock().unwrap().replaygain_db())
    }
}

/// A wall-clock instant for `switch` predicates. Weekday uses the C
/// convention 0 = Sunday .. 6 = Saturday; `minutes` is minutes since
/// midnight in the local timezone.
#[derive(Clone, Copy, Debug)]
struct LocalTime {
    weekday: u8,
    minutes: u32,
}

impl LocalTime {
    fn now() -> Self {
        use chrono::{Datelike, Timelike};
        let now = chrono::Local::now();
        Self {
            weekday: now.weekday().num_days_from_sunday() as u8,
            minutes: (now.hour() * 60 + now.minute()),
        }
    }
}

/// A `switch` child schedule: weekday set and/or a `[from, to)` window in
/// minutes since midnight. An omitted window means "always" (the default
/// child). `from > to` wraps past midnight (e.g. 22:00-06:00); `from == to`
/// is an empty window that never matches.
#[derive(Clone, Debug)]
struct TimePredicate {
    days: Option<Vec<u8>>,
    from: Option<u32>,
    to: Option<u32>,
}

impl TimePredicate {
    fn always() -> Self {
        Self {
            days: None,
            from: None,
            to: None,
        }
    }

    fn is_always(&self) -> bool {
        self.days.is_none() && self.from.is_none() && self.to.is_none()
    }

    fn matches(&self, t: &LocalTime) -> bool {
        if let Some(days) = &self.days
            && !days.contains(&t.weekday)
        {
            return false;
        }
        match (self.from, self.to) {
            (Some(f), Some(to)) if f == to => false,
            (Some(f), Some(to)) if f < to => t.minutes >= f && t.minutes < to,
            (Some(f), Some(to)) => t.minutes >= f || t.minutes < to,
            (Some(f), None) => t.minutes >= f,
            (None, Some(to)) => t.minutes < to,
            (None, None) => true,
        }
    }
}

/// One `switch` slot: a predicate plus its child index.
struct SwitchSlot {
    when: TimePredicate,
    child: usize,
}

/// How a [`ScheduleSource`] picks its next child at a track boundary.
enum ScheduleKind {
    /// Liquidsoap `switch`: the first slot whose predicate matches (and
    /// whose child still has audio) wins; slots are re-checked from the top
    /// at every boundary, so a slot's window opening grabs the next track.
    Switch(Vec<SwitchSlot>),
    /// Liquidsoap `rotate`: round-robin with per-child weights. A weight of
    /// `w` keeps the child for `w` consecutive tracks.
    Rotate {
        weights: Vec<usize>,
        cursor: usize,
        spins: usize,
    },
}

/// Track-sensitive scheduling of child sources (Liquidsoap `switch` /
/// `rotate`). The active child plays to the end of its current track; a new
/// child is selected only when a track boundary is observed (the child's
/// `label` changes, or it exhausts). With `track_sensitive = false` the
/// schedule is re-evaluated on every pull and children are cut abruptly.
///
/// Boundary detection is sample-accurate enough for scheduling: a label
/// change is noticed on the pull that returns the new track's first buffer,
/// so the re-pick happens on the following pull (at most one buffer of the
/// next track comes from the old child).
struct ScheduleSource {
    children: Vec<LuaSource>,
    kind: ScheduleKind,
    current: usize,
    /// A boundary was observed; re-select at the start of the next pull.
    pending: bool,
    /// First pull of the freshly-selected child: record its label without
    /// treating it as a boundary.
    primed: bool,
    last_label: Option<String>,
    track_sensitive: bool,
    /// Injectable wall clock for tests.
    now: Box<dyn Fn() -> LocalTime + Send + Sync>,
}

impl ScheduleSource {
    fn new(kind: ScheduleKind, children: Vec<LuaSource>, track_sensitive: bool) -> Self {
        ScheduleSource {
            children,
            kind,
            current: 0,
            pending: true,
            primed: false,
            last_label: None,
            track_sensitive,
            now: Box::new(LocalTime::now),
        }
    }

    /// First `switch` slot whose predicate matches and whose child still
    /// produces audio.
    fn first_matching(&self) -> Option<usize> {
        let now = (self.now)();
        let ScheduleKind::Switch(slots) = &self.kind else {
            return None;
        };
        slots.iter().find_map(|slot| {
            if !slot.when.matches(&now) {
                return None;
            }
            let child = self.children.get(slot.child)?.0.lock().unwrap();
            if child.is_exhausted() { None } else { Some(slot.child) }
        })
    }

    /// Pick the next child as the schedule dictates and reset the label
    /// priming state.
    fn select(&mut self) {
        match &mut self.kind {
            ScheduleKind::Switch(_) => {
                if let Some(i) = self.first_matching() {
                    self.current = i;
                }
            }
            ScheduleKind::Rotate {
                weights,
                cursor,
                spins,
            } => {
                let n = self.children.len();
                let mut guard = 0;
                loop {
                    let c = *cursor % n;
                    if !self.children[c].0.lock().unwrap().is_exhausted() {
                        self.current = c;
                        break;
                    }
                    *spins += 1;
                    if *spins >= weights[c] {
                        *spins = 0;
                        *cursor = (*cursor + 1) % n;
                    }
                    guard += 1;
                    if guard > n {
                        // Everything exhausted; keep whatever is current.
                        break;
                    }
                }
                // Advance the rotation so the next boundary moves on, and
                // a weight of `w` keeps the child for `w` consecutive tracks.
                if guard <= n && !self.children.is_empty() {
                    *spins += 1;
                    if *spins >= weights[*cursor % n] {
                        *spins = 0;
                        *cursor = (*cursor + 1) % n;
                    }
                }
            }
        }
        self.primed = false;
        self.last_label = None;
    }

    /// Re-pick, honouring a boundary noticed at a previous pull.
    fn choose(&mut self) {
        if self.pending {
            self.select();
            self.pending = false;
        }
    }
}

impl AudioSource for ScheduleSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        self.choose();
        for _ in 0..self.children.len() {
            if !self.track_sensitive {
                // Immediate scheduling: switch children mid-track as soon as
                // the predicates change.
                if let ScheduleKind::Switch(_) = self.kind
                    && let Some(i) = self.first_matching()
                    && i != self.current
                {
                    self.current = i;
                    self.primed = false;
                }
            }
            let (n, ended) = {
                let mut child = self.children[self.current].0.lock().unwrap();
                let n = child.next_buffer(buffer);
                if n == 0 {
                    (0, child.is_exhausted())
                } else {
                    let label = child.label();
                    let started_new = self.primed && label != self.last_label;
                    self.last_label = label;
                    self.primed = true;
                    (n, started_new)
                }
            };
            if n > 0 {
                if ended {
                    // A new track started inside this buffer; re-pick at the
                    // next pull so the next buffer comes from the right child.
                    self.pending = true;
                }
                return n;
            }
            if !ended {
                // Active child is temporarily silent; hold until it speaks.
                return 0;
            }
            // Active child exhausted: pick the next child right away so this
            // pull can hand over seamlessly.
            self.select();
            self.pending = false;
        }
        0
    }

    fn is_exhausted(&self) -> bool {
        self.children.iter().all(|c| c.0.lock().unwrap().is_exhausted())
    }

    fn remaining_seconds(&self) -> Option<f64> {
        self.children
            .get(self.current)
            .and_then(|c| c.0.lock().unwrap().remaining_seconds())
    }

    fn label(&self) -> Option<String> {
        self.children
            .get(self.current)
            .and_then(|c| c.0.lock().unwrap().label())
    }

    fn replaygain_db(&self) -> Option<f32> {
        self.children
            .get(self.current)
            .and_then(|c| c.0.lock().unwrap().replaygain_db())
    }

    fn skip(&mut self) {
        // A skip is a track boundary too: the current track ends now, and
        // the schedule re-picks on the next pull.
        self.pending = true;
        if let Some(child) = self.children.get_mut(self.current) {
            child.0.lock().unwrap().skip();
        }
    }
}

/// Read an optional numeric table field with a default.
fn opt_f64(opts: &Option<Table>, key: &str, default: f64) -> mlua::Result<f64> {
    match opts {
        Some(t) => Ok(t.get::<Option<f64>>(key)?.unwrap_or(default)),
        None => Ok(default),
    }
}

/// Parse an audio source list from a Lua array table of `LuaSource`s.
fn source_list(table: &Table) -> mlua::Result<Vec<LuaSource>> {
    let mut sources = Vec::new();
    let len = table.len()?;
    for i in 1..=len {
        let child: LuaSource = table.get(i)?;
        sources.push(child);
    }
    if sources.is_empty() {
        return Err(mlua::Error::runtime("expected a list of sources"));
    }
    Ok(sources)
}

/// Parse a `"HH:MM"` clock time into minutes since midnight.
fn parse_hhmm(value: &str) -> mlua::Result<u32> {
    let mut parts = value.split(':');
    let (h, m) = match (parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(m), None) => (h, m),
        _ => return Err(mlua::Error::runtime(format!("switch: bad time {value:?}, use \"HH:MM\""))),
    };
    let h: u32 = h.parse().map_err(|_| mlua::Error::runtime(format!("switch: bad time {value:?}")))?;
    let m: u32 = m.parse().map_err(|_| mlua::Error::runtime(format!("switch: bad time {value:?}")))?;
    if h > 23 || m > 59 {
        return Err(mlua::Error::runtime(format!("switch: bad time {value:?}")));
    }
    Ok(h * 60 + m)
}

/// Weekday name ("mon".."sun" or full names) to 0=Sunday..6=Saturday.
fn weekday_number(name: &str) -> mlua::Result<u8> {
    let n = match name.to_ascii_lowercase().as_str() {
        "sun" | "sunday" => 0,
        "mon" | "monday" => 1,
        "tue" | "tuesday" => 2,
        "wed" | "wednesday" => 3,
        "thu" | "thursday" => 4,
        "fri" | "friday" => 5,
        "sat" | "saturday" => 6,
        other => {
            return Err(mlua::Error::runtime(format!(
                "switch: unknown weekday {other:?} (use \"mon\"..\"sun\" or 0=Sunday..6=Saturday)"
            )))
        }
    };
    Ok(n)
}

/// Parse a `switch` slot's `when` table: optional `days` (names or 0-6,
/// 0=Sunday), optional `from`/`to` (`"HH:MM"`). An empty table means
/// always (the default child).
fn parse_when(table: &Table) -> mlua::Result<TimePredicate> {
    let days = match table.get::<Option<Table>>("days")? {
        Some(days) => {
            let mut out = Vec::new();
            for i in 1..=days.len()? {
                match days.get::<LValue>(i)? {
                    LValue::String(s) => out.push(weekday_number(&s.to_str()?)?),
                    LValue::Integer(n) if (0..=6).contains(&n) => out.push(n as u8),
                    _ => {
                        return Err(mlua::Error::runtime(
                            "switch: `days` entries must be weekday names or 0-6",
                        ))
                    }
                }
            }
            Some(out)
        }
        None => None,
    };
    let from = match table.get::<Option<String>>("from")? {
        Some(s) => Some(parse_hhmm(&s)?),
        None => None,
    };
    let to = match table.get::<Option<String>>("to")? {
        Some(s) => Some(parse_hhmm(&s)?),
        None => None,
    };
    Ok(TimePredicate { days, from, to })
}

/// Signal spec + frames-per-buffer from the current script settings.
fn bus(state: &Rc<RefCell<ScriptState>>) -> (SignalSpec, usize) {
    let s = state.borrow();
    (s.stream.signal_spec(), s.stream.frames_per_buffer)
}

/// A playlist whose tracks crossfade into each other, presented as a plain
/// source so it composes inside fallback/random.
fn crossfading_playlist(
    requests: Vec<RequestUri>,
    shuffle: bool,
    loop_playlist: bool,
    state: &Rc<RefCell<ScriptState>>,
) -> Box<dyn AudioSource> {
    let (spec, fpb) = bus(state);
    let chans = spec.channels.count();
    let mixer_cfg = state.borrow().mixer.clone();
    let request = state.borrow().request;
    let playlist = Playlist::new(requests, shuffle, loop_playlist, request, spec, fpb, None);
    Box::new(CrossfadeMixer::new(Box::new(playlist), &mixer_cfg, spec.rate, chans))
}

/// Evaluate a `.lua` script and return the runtime plus the engine wiring.
/// The [`ScriptRuntime`] owns the `Lua` instance, which now lives for the
/// process lifetime: callbacks are invoked from its event loop on the
/// calling thread only.
pub fn run(src: &str) -> mlua::Result<(ScriptRuntime, ScriptResult)> {
    let lua = Lua::new();
    let globals = lua.globals();
    let state = Rc::new(RefCell::new(ScriptState::default()));
    let (event_tx, event_rx) = mpsc::channel();

    // ---- settings ------------------------------------------------------
    let set_state = state.clone();
    globals.set(
        "set",
        lua.create_function(move |_, (key, value): (String, LValue)| {
            set_state.borrow_mut().set(&key, value)
        })?,
    )?;

    // ---- logging -------------------------------------------------------
    globals.set(
        "log",
        lua.create_function(|_, msg: String| {
            log::info!("script: {msg}");
            Ok(())
        })?,
    )?;

    // ---- source constructors -------------------------------------------
    let pl_state = state.clone();
    globals.set(
        "playlist",
        lua.create_function(move |_, opts: Table| {
            let directory: Option<String> = opts.get("directory").ok().flatten();
            let files: Vec<String> = opts.get("files").ok().unwrap_or_default();
            let shuffle: bool = opts.get("shuffle").unwrap_or(false);
            let loop_playlist: bool = opts.get("loop").unwrap_or(true);

            let mut requests = Vec::new();
            if let Some(dir) = &directory {
                let mut paths = Vec::new();
                collect_audio(&PathBuf::from(dir), &mut paths);
                requests.extend(paths.into_iter().map(|p| RequestUri::new(p.to_str().unwrap_or_default())));
            }
            // `files` entries may be paths or http:// URLs.
            requests.extend(files.iter().map(|f| RequestUri::new(f)));
            requests.sort();
            requests.dedup();
            if requests.is_empty() {
                return Err(mlua::Error::runtime(
                    "playlist: no audio files found (check `directory`/`files`)",
                ));
            }
            let src = crossfading_playlist(requests, shuffle, loop_playlist, &pl_state);
            Ok(LuaSource::new(src))
        })?,
    )?;

    let single_state = state.clone();
    globals.set(
        "single",
        lua.create_function(move |_, path: String| {
            let (spec, fpb) = bus(&single_state);
            let uri = RequestUri::new(&path);
            let request = single_state.borrow().request;
            let src = resolve(&uri, &request, spec, fpb).map_err(mlua::Error::runtime)?;
            Ok(LuaSource::new(src))
        })?,
    )?;

    // ---- request queue (Liquidsoap `request.queue`) ----------------------
    let queue_state = state.clone();
    let request_fn = lua.create_function(move |_, _: ()| {
        let (spec, fpb) = bus(&queue_state);
        let request = queue_state.borrow().request;
        let queue = Arc::new(RequestQueue::new());
        queue_state.borrow_mut().request_queue = Some(queue.clone());
        let src = RequestQueueSource::new(queue, request, spec, fpb);
        Ok(LuaSource::new(Box::new(src)))
    })?;
    let request = lua.create_table()?;
    request.set("queue", request_fn)?;
    globals.set("request", request)?;

    // ---- test sources (Liquidsoap `blank`, `sine`) -----------------------
    let blank_state = state.clone();
    globals.set(
        "blank",
        lua.create_function(move |_, opts: Option<Table>| {
            let duration: Option<f64> = match &opts {
                Some(t) => t.get("duration")?,
                None => None,
            };
            let (spec, _) = bus(&blank_state);
            let src: Box<dyn AudioSource> = match duration {
                Some(d) => Box::new(BlankSource::with_duration(d, spec.rate)),
                None => Box::new(BlankSource::new()),
            };
            Ok(LuaSource::new(src))
        })?,
    )?;

    let sine_state = state.clone();
    globals.set(
        "sine",
        lua.create_function(move |_, opts: Option<Table>| {
            let freq: f64 = match &opts {
                Some(t) => t.get::<Option<f64>>("freq")?.unwrap_or(440.0),
                None => 440.0,
            };
            let duration: Option<f64> = match &opts {
                Some(t) => t.get("duration")?,
                None => None,
            };
            let amplitude: f64 = match &opts {
                Some(t) => t.get::<Option<f64>>("amplitude")?.unwrap_or(0.5),
                None => 0.5,
            };
            let (spec, _) = bus(&sine_state);
            let src = SineSource::new(
                freq as f32,
                duration,
                amplitude as f32,
                spec.rate,
                spec.channels.count(),
            );
            Ok(LuaSource::new(Box::new(src)))
        })?,
    )?;

    // ---- effects (Liquidsoap `amplify`, `compress`, `normalize`, `replaygain`) ---------
    globals.set(
        "replaygain",
        lua.create_function(move |_, (mut source, opts): (LuaSource, Option<Table>)| {
            let max_boost = opt_f64(&opts, "max_boost", 12.0)?;
            let max_cut = opt_f64(&opts, "max_cut", 12.0)?;
            let child = source.take();
            let src = ReplayGainSource::new(child, max_boost as f32, max_cut as f32);
            Ok(LuaSource::new(Box::new(src)))
        })?,
    )?;

    let amp_state = state.clone();
    globals.set(
        "amplify",
        lua.create_function(move |_, (mut source, gain): (LuaSource, f64)| {
            let (spec, _) = bus(&amp_state);
            let child = source.take();
            let src =
                EffectSource::new(child, Amplify::new(gain as f32), spec.channels.count());
            Ok(LuaSource::new(Box::new(src)))
        })?,
    )?;

    let compress_state = state.clone();
    globals.set(
        "compress",
        lua.create_function(move |_, (mut source, opts): (LuaSource, Option<Table>)| {
            let threshold = opt_f64(&opts, "threshold", -12.0)?;
            let ratio = opt_f64(&opts, "ratio", 2.0)?;
            let attack = opt_f64(&opts, "attack", 0.005)?;
            let release = opt_f64(&opts, "release", 0.1)?;
            let makeup = opt_f64(&opts, "makeup", 0.0)?;
            let (spec, _) = bus(&compress_state);
            let child = source.take();
            let fx = Compressor::new(
                threshold as f32,
                ratio as f32,
                attack as f32,
                release as f32,
                makeup as f32,
                spec.rate,
            );
            Ok(LuaSource::new(Box::new(EffectSource::new(
                child,
                fx,
                spec.channels.count(),
            ))))
        })?,
    )?;

    let normalize_state = state.clone();
    globals.set(
        "normalize",
        lua.create_function(move |_, (mut source, opts): (LuaSource, Option<Table>)| {
            let target = opt_f64(&opts, "target", -13.0)?;
            let attack = opt_f64(&opts, "attack", 3.0)?;
            let release = opt_f64(&opts, "release", 0.5)?;
            let max_boost = opt_f64(&opts, "max_boost", 20.0)?;
            let max_cut = opt_f64(&opts, "max_cut", 20.0)?;
            let (spec, _) = bus(&normalize_state);
            let child = source.take();
            let fx = Agc::new(
                target as f32,
                attack as f32,
                release as f32,
                max_boost as f32,
                max_cut as f32,
                spec.rate,
            );
            Ok(LuaSource::new(Box::new(EffectSource::new(
                child,
                fx,
                spec.channels.count(),
            ))))
        })?,
    )?;

    // ---- source composition ---------------------------------------------
    let composer = lua.create_function(|_, (children, kind): (Table, String)| {
        let sources = source_list(&children)?;
        let composed: Box<dyn AudioSource> = match kind.as_str() {
            "fallback" | "sequence" => Box::new(FallbackSource::new(sources)),
            "random" => Box::new(RandomSource::new(sources)),
            other => {
                return Err(mlua::Error::runtime(format!(
                    "unknown composer {other}"
                )))
            }
        };
        Ok(LuaSource::new(composed))
    })?;
    for name in ["fallback", "sequence", "random"] {
        let comp = composer.clone();
        globals.set(
            name,
            lua.create_function(move |_lua, children: Table| {
                comp.call::<LuaSource>((children, name.to_string()))
            })?,
        )?;
    }

    // ---- daypart scheduling (Liquidsoap `switch`, `rotate`) --------------
    globals.set(
        "switch",
        lua.create_function(
            |_lua, (slots, opts): (Table, Option<Table>)| {
                let track_sensitive: bool = match &opts {
                    Some(t) => t.get("track_sensitive").unwrap_or(true),
                    None => true,
                };
                let len = slots.len()?;
                if len == 0 {
                    return Err(mlua::Error::runtime(
                        "switch: expected a list of {when = ..., src = ...} slots",
                    ));
                }
                let mut children = Vec::new();
                let mut predicates = Vec::new();
                for i in 1..=len {
                    let slot: Table = slots.get(i)?;
                    let src: LuaSource = slot.get("src")?;
                    let child = children.len();
                    children.push(src);
                    let when = match slot.get::<Option<Table>>("when")? {
                        Some(t) => parse_when(&t)?,
                        None => TimePredicate::always(),
                    };
                    predicates.push(SwitchSlot { when, child });
                }
                if !predicates.iter().any(|s| s.when.is_always()) {
                    return Err(mlua::Error::runtime(
                        "switch: expected a default child (a slot without `when`)",
                    ));
                }
                let composed: Box<dyn AudioSource> = Box::new(ScheduleSource::new(
                    ScheduleKind::Switch(predicates),
                    children,
                    track_sensitive,
                ));
                Ok(LuaSource::new(composed))
            },
        )?,
    )?;

    globals.set(
        "rotate",
        lua.create_function(|_lua, (children, opts): (Table, Option<Table>)| {
            let sources = source_list(&children)?;
            let n = sources.len();
            let weights: Vec<usize> = match &opts {
                Some(t) => t.get("weights").unwrap_or_default(),
                None => Vec::new(),
            };
            let weights = if weights.is_empty() {
                vec![1; n]
            } else {
                if weights.len() != n {
                    return Err(mlua::Error::runtime(
                        "rotate: `weights` must have one entry per child",
                    ));
                }
                weights
            };
            let composed: Box<dyn AudioSource> = Box::new(ScheduleSource::new(
                ScheduleKind::Rotate { weights, cursor: 0, spins: 0 },
                sources,
                true,
            ));
            Ok(LuaSource::new(composed))
        })?,
    )?;

    // ---- jingles ----------------------------------------------------------
    let jingle_state = state.clone();
    globals.set(
        "jingles",
        lua.create_function(move |_, opts: Table| {
            let directory: Option<String> = opts.get("directory").ok().flatten();
            let files: Vec<String> = opts.get("files").ok().unwrap_or_default();
            let mut paths = Vec::new();
            if let Some(dir) = &directory {
                collect_audio(&PathBuf::from(dir), &mut paths);
            }
            paths.extend(files.iter().map(PathBuf::from));
            paths.sort();
            paths.dedup();
            if paths.is_empty() {
                return Err(mlua::Error::runtime(
                    "jingles: no audio files found (check `directory`/`files`)",
                ));
            }
            // Registered for the telnet `jingles.play` command...
            jingle_state.borrow_mut().jingles.extend(paths.clone());
            // ...and returned as a plain (non-looped) playlist source so it
            // composes like any other source (jingles stay local paths).
            let requests = paths.into_iter().map(RequestUri::Local).collect();
            let src = crossfading_playlist(requests, false, false, &jingle_state);
            Ok(LuaSource::new(src))
        })?,
    )?;

    // ---- services ---------------------------------------------------------
    let harbor_state = state.clone();
    let harbor_fn = lua.create_function(move |_, opts: Table| {
        let cfg = LiveConfig {
            host: opts.get("host").unwrap_or_else(|_| "0.0.0.0".into()),
            port: opts.get("port").unwrap_or(8005),
            mount: opts.get("mount").unwrap_or_else(|_| "/live".into()),
            password: opts.get("password").unwrap_or_else(|_| "dj".into()),
        };
        harbor_state.borrow_mut().harbor = Some(cfg);
        // The harbor drives the priority mixer via MixCommand; the value
        // is a marker that exhausts immediately when composed.
        Ok(LuaSource::new(Box::new(SilenceSource::new())))
    })?;
    let input = lua.create_table()?;
    input.set("harbor", harbor_fn)?;
    globals.set("input", input)?;

    let telnet_state = state.clone();
    let telnet_fn = lua.create_function(move |_, opts: Table| {
        let cfg = ControlConfig {
            host: opts.get("host").unwrap_or_else(|_| "127.0.0.1".into()),
            port: opts.get("port").unwrap_or(1234),
        };
        telnet_state.borrow_mut().control = Some(cfg);
        Ok(())
    })?;
    let reg_state = state.clone();
    let register_fn = lua.create_function(
        move |_, (name, callback): (String, mlua::Function)| {
            let trimmed = name.trim();
            if trimmed.is_empty() || trimmed.split_whitespace().count() > 1 {
                return Err(mlua::Error::runtime(
                    "server.register: name must be a single non-empty word",
                ));
            }
            reg_state.borrow_mut().custom_commands.push((trimmed.to_string(), callback));
            Ok(())
        },
    )?;
    let server = lua.create_table()?;
    server.set("telnet", telnet_fn)?;
    server.set("register", register_fn)?;
    globals.set("server", server)?;

    // ---- outputs ----------------------------------------------------------
    /// Parse an output format string (`"mp3"` / `"opus"`).
    fn parse_format(value: &str) -> mlua::Result<OutputFormat> {
        match value {
            "mp3" => Ok(OutputFormat::Mp3),
            "opus" => Ok(OutputFormat::Opus),
            "aac" => Ok(OutputFormat::Aac),
            other => Err(mlua::Error::runtime(format!(
                "unknown output format {other:?} (use \"mp3\", \"opus\" or \"aac\")"
            ))),
        }
    }

    /// First output call wins the shared root; later calls must pass the
    /// same source graph (`Arc::ptr_eq`).
    fn claim_root(s: &mut ScriptState, source: &mut LuaSource) -> mlua::Result<()> {
        match &s.root_arc {
            None => {
                s.root_arc = Some(source.0.clone());
                s.root = Some(source.take());
                Ok(())
            }
            Some(existing) if Arc::ptr_eq(existing, &source.0) => Ok(()),
            Some(_) => Err(mlua::Error::runtime(
                "output calls must all share the same root source",
            )),
        }
    }

    let out_state = state.clone();
    let make_output = lua.create_function(move |_, (opts, mut source): (Table, LuaSource)| {
        let format = opts
            .get::<Option<String>>("format")?
            .map(|f| parse_format(&f))
            .transpose()?
            .unwrap_or(OutputFormat::Mp3);
        let cfg = OutputConfig {
            host: opts.get("host").unwrap_or_else(|_| "localhost".into()),
            port: opts.get("port").unwrap_or(8000),
            mount: opts.get("mount").unwrap_or_else(|_| "/crabsoup.mp3".into()),
            source_user: opts.get("user").unwrap_or_else(|_| "source".into()),
            source_password: opts.get("password").unwrap_or_else(|_| "hackme".into()),
            format,
            bitrate: opts.get("bitrate").unwrap_or(192_000),
            name: opts.get("name").unwrap_or_else(|_| "Crabsoup".into()),
            description: opts
                .get("description")
                .unwrap_or_else(|_| "Crabsoup stream".into()),
            genre: opts.get("genre").unwrap_or_else(|_| "Various".into()),
            reconnect_seconds: opts.get("reconnect").unwrap_or(5),
        };
        let mut s = out_state.borrow_mut();
        claim_root(&mut s, &mut source)?;
        s.outputs.push(cfg);
        Ok(())
    })?;
    let output = lua.create_table()?;
    output.set("icecast", make_output)?;
    let prev_state = state.clone();
    output.set(
        "preview",
        lua.create_function(move |_, mut source: LuaSource| {
            prev_state.borrow_mut().preview = Some(source.take());
            Ok(())
        })?,
    )?;
    let file_state = state.clone();
    output.set(
        "file",
        lua.create_function(move |_, (opts, mut source): (Table, LuaSource)| {
            let path: String = opts
                .get("path")
                .map_err(|_| mlua::Error::runtime("output.file: path is required"))?;
            let format = opts
                .get::<Option<String>>("format")?
                .map(|f| parse_format(&f))
                .transpose()?
                .unwrap_or(OutputFormat::Mp3);
            let cfg = FileOutputConfig {
                path: path.into(),
                format,
                bitrate: opts.get("bitrate").unwrap_or(128_000),
            };
            let mut s = file_state.borrow_mut();
            claim_root(&mut s, &mut source)?;
            s.file_outputs.push(cfg);
            Ok(())
        })?,
    )?;
    let hls_state = state.clone();
    output.set(
        "hls",
        lua.create_function(move |_, (opts, mut source): (Table, LuaSource)| {
            let directory: String = opts
                .get("directory")
                .map_err(|_| mlua::Error::runtime("output.hls: directory is required"))?;
            let cfg = HlsOutputConfig {
                directory: directory.into(),
                segment_seconds: opts.get("segment_seconds").unwrap_or(5.0),
                retention: opts.get("retention").unwrap_or(12),
            };
            let mut s = hls_state.borrow_mut();
            claim_root(&mut s, &mut source)?;
            s.hls_outputs.push(cfg);
            Ok(())
        })?,
    )?;
    globals.set("output", output)?;

    // ---- metadata hooks (Liquidsoap `on_metadata`, `on_track`) ---------------
    let meta_state = state.clone();
    let meta_tx = event_tx.clone();
    globals.set(
        "on_metadata",
        lua.create_function(move |_, (mut source, callback): (LuaSource, mlua::Function)| {
            let hook_id = meta_state.borrow().metadata_hooks.len();
            let child = source.take();
            meta_state.borrow_mut().metadata_hooks.push(callback);
            let wrapped = OnMetadataSource::new(child, meta_tx.clone(), hook_id);
            Ok(LuaSource::new(Box::new(wrapped)))
        })?,
    )?;

    let track_state = state.clone();
    let track_tx = event_tx.clone();
    globals.set(
        "on_track",
        lua.create_function(move |_, (mut source, callback): (LuaSource, mlua::Function)| {
            let hook_id = track_state.borrow().track_hooks.len();
            let child = source.take();
            track_state.borrow_mut().track_hooks.push(callback);
            let wrapped = OnTrackSource::new(child, track_tx.clone(), hook_id);
            Ok(LuaSource::new(Box::new(wrapped)))
        })?,
    )?;

    // ---- evaluate ---------------------------------------------------------
    lua.load(src)
        .set_name("crabsoup.lua")
        .exec()?;

    let mut s = state.borrow_mut();
    let result = ScriptResult {
        stream: s.stream.clone(),
        mixer: s.mixer.clone(),
        jingles: std::mem::take(&mut s.jingles),
        harbor: s.harbor.take(),
        control: s.control.take(),
        outputs: std::mem::take(&mut s.outputs),
        file_outputs: std::mem::take(&mut s.file_outputs),
        hls_outputs: std::mem::take(&mut s.hls_outputs),
        request_queue: s.request_queue.take(),
        custom_commands: s.custom_commands.iter().map(|(n, _)| n.clone()).collect(),
        root: s.root.take(),
        preview: s.preview.take(),
    };
    if result.root.is_none() && result.preview.is_none() {
        return Err(mlua::Error::runtime(
            "script defines no output: add output.icecast(...) or output.preview(...)",
        ));
    }
    let runtime = ScriptRuntime {
        lua,
        event_rx,
        event_tx,
        metadata_hooks: std::mem::take(&mut s.metadata_hooks),
        track_hooks: std::mem::take(&mut s.track_hooks),
        custom_commands: std::mem::take(&mut s.custom_commands),
    };
    Ok((runtime, result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_sets_settings_via_lua() {
        let (_rt, res) = run(
            r#"
            set("sample_rate", 48000)
            set("channels", 1)
            set("crossfade_seconds", 4.5)
            input.harbor({port = 8006})
            output.preview(input.harbor({port = 8007}))
            "#,
        )
        .expect("script runs");
        assert_eq!(res.stream.sample_rate, 48000);
        assert_eq!(res.stream.channels, 1);
        assert_eq!(res.mixer.crossfade_seconds, 4.5);
        let harbor = res.harbor.expect("harbor registered");
        assert_eq!(harbor.port, 8007);
        assert!(res.preview.is_some());
        assert!(res.root.is_none());
    }

    #[test]
    fn unknown_setting_fails() {
        let err = run("set(\"bogus\", 1)").err().expect("script fails");
        assert!(err.to_string().contains("unknown setting"));
    }

    #[test]
    fn script_without_output_fails() {
        let err = run("set(\"sample_rate\", 48000)")
            .err()
            .expect("script fails");
        assert!(err.to_string().contains("no output"));
    }

    #[test]
    fn server_register_runs_the_lua_handler_and_replies() {
        let (rt, res) = run(
            r#"
            server.register("ping", function(args) return "pong [" .. args .. "]" end)
            output.preview(sine({freq = 440, duration = 1}))
            "#,
        )
        .expect("script runs");
        assert_eq!(res.custom_commands, vec!["ping"]);
        let (reply_tx, reply_rx) = mpsc::channel();
        rt.event_tx()
            .send(ScriptEvent::Custom {
                index: 0,
                args: "x y".into(),
                reply: reply_tx,
            })
            .expect("event send");
        rt.drain_metadata();
        assert_eq!(reply_rx.recv().expect("reply").expect("ok"), "pong [x y]");
    }

    #[test]
    fn server_register_reports_callback_errors() {
        let (rt, _res) = run(
            r#"
            server.register("boom", function() error("kaput") end)
            output.preview(sine({freq = 440, duration = 1}))
            "#,
        )
        .expect("script runs");
        let (reply_tx, reply_rx) = mpsc::channel();
        rt.event_tx()
            .send(ScriptEvent::Custom {
                index: 0,
                args: "".into(),
                reply: reply_tx,
            })
            .expect("event send");
        rt.drain_metadata();
        let err = reply_rx.recv().expect("reply").expect_err("must error");
        assert!(err.contains("kaput"), "{err}");
    }

    #[test]
    fn server_register_rejects_bad_names() {
        for (name, detail) in [
            ("pong ping", "single non-empty word"),
            ("", "non-empty"),
            ("  ", "non-empty"),
        ] {
            let script = format!(
                "server.register(\"{name}\", function() return \"x\" end)\n\
                 output.preview(sine({{freq = 440, duration = 1}}))"
            );
            let err = run(&script).err().expect("script fails");
            assert!(
                err.to_string().contains(detail),
                "{name:?}: {}",
                err.to_string()
            );
        }
    }

    #[test]
    fn compose_sources_without_files() {
        let (_rt, res) = run(
            r#"
            live = input.harbor({})
            backup = input.harbor({})
            output.preview(fallback({live, backup}))
            "#,
        )
        .expect("script runs");
        assert!(res.preview.is_some());
    }

    #[test]
    fn sine_with_duration_drives_exactly_one_second_of_frames() {
        let (_rt, res) = run(
            r#"
            output.preview(amplify(sine({freq = 220, duration = 1}), 0.5))
            "#,
        )
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        let mut total = 0usize;
        let mut peak = 0.0f32;
        while !root.is_exhausted() {
            let n = root.next_buffer(&mut buf);
            total += n;
            peak = peak.max(buf[..n].iter().fold(0.0, |m, &s| m.max(s.abs())));
        }
        // 1 s at 44100 Hz stereo, amplified by 0.5 (sine amplitude 0.5 -> 0.25).
        assert_eq!(total, 44100 * 2);
        assert!(peak <= 0.26, "amplify did not scale the tone (peak {peak})");
    }

    #[test]
    fn blank_falls_through_to_the_next_child() {
        let (_rt, res) = run(
            r#"
            output.preview(fallback({blank({duration = 0.1}), sine({duration = 1, freq = 100})}))
            "#,
        )
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        let n1 = root.next_buffer(&mut buf);
        assert_eq!(n1, 4410, "blank should fill 0.1s then hand over");
        assert!(buf[..n1].iter().all(|&s| s == 0.0));
        let n2 = root.next_buffer(&mut buf);
        assert!(n2 > 0);
        assert!(
            buf[..n2].iter().any(|&s| s.abs() > 0.01),
            "fallback did not reach the sine after blank ended"
        );
    }

    #[test]
    fn request_queue_registers_and_is_shared_with_the_control_port() {
        let (_rt, res) = run(
            r#"
            q = request.queue()
            output.preview(fallback({q, sine({freq = 440, duration = 1})}))
            "#,
        )
        .expect("script runs");
        let queue = res.request_queue.expect("queue registered");
        assert!(queue.is_empty());
        queue.push(RequestUri::new("/tmp/x.mp3"));
        assert_eq!(
            queue.list(),
            vec![RequestUri::new("/tmp/x.mp3")]
        );
    }

    #[test]
    fn request_queue_preempts_a_playing_playlist_when_pushed() {
        let real = PathBuf::from("media/sunset-house-grooves-deep-house-sunset-538759.mp3");
        if !real.exists() {
            return;
        }
        let (_rt, res) = run(
            r#"
            q = request.queue()
            output.preview(fallback({q, sine({freq = 440, duration = 2})}))
            "#,
        )
        .expect("script runs");
        let queue = res.request_queue.as_ref().expect("queue registered");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];

        // Queue empty: the sine plays.
        let n = root.next_buffer(&mut buf);
        assert!(n > 0);
        assert!(buf[..n].iter().any(|&s| s.abs() > 0.01), "sine should play");
        assert_eq!(root.label().as_deref(), Some("sine 440 Hz"));

        // Pushing a request mid-stream: the queue preempts on the next pull.
        queue.push(RequestUri::Local(real.clone()));
        let n = root.next_buffer(&mut buf);
        assert!(n > 0);
        assert_eq!(
            root.label(),
            Some("sunset-house-grooves-deep-house-sunset-538759".to_string()),
            "queued track must take over"
        );

        // `queue.skip` drops the requested track; with the queue now empty
        // the fallback returns to the sine.
        queue.request_skip();
        let n = root.next_buffer(&mut buf);
        assert!(n > 0);
        assert_eq!(root.label().as_deref(), Some("sine 440 Hz"));
    }

    #[test]
    fn compress_reduces_a_loud_tone() {
        let (_rt, res) = run(
            r#"
            output.preview(compress(sine({freq = 440, duration = 1, amplitude = 1.0}),
                                    {threshold = -12, ratio = 2, attack = 0, release = 0}))
            "#,
        )
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        root.next_buffer(&mut buf);
        let peak = buf.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        // 0 dB peaks, -12 dB threshold, 2:1 -> the peak is cut by 6 dB.
        assert!((peak - 0.5).abs() < 0.01, "compressed peak {peak}");
    }

    #[test]
    fn replaygain_composes_and_leaves_untagged_tracks_alone() {
        let (_rt, res) = run(
            r#"
            output.preview(replaygain(sine({freq = 440, duration = 1, amplitude = 0.5}),
                                       {max_boost = 6, max_cut = 6}))
            "#,
        )
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        let n = root.next_buffer(&mut buf);
        // No ReplayGain tags on a sine: unity gain, samples pass untouched.
        assert_eq!(root.label().as_deref(), Some("sine 440 Hz"));
        assert!(buf[..n].iter().any(|&s| s.abs() > 0.3));
    }

    #[test]
    fn on_metadata_callback_receives_titles_in_order() {
        let (_rt, res) = run(
            r#"
            titles = {}
            src = on_metadata(sequence({sine({freq = 440, duration = 0.1}),
                                       sine({freq = 880, duration = 0.1})}),
                              function(m) titles[#titles + 1] = m.title end)
            output.preview(src)
            "#,
        )
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        // Each 0.1 s tone lasts ~1.1 buffers at 4096 frames; drive enough
        // buffers to cross both track boundaries, then drop the source and
        // drain whatever events were queued.
        for _ in 0..4 {
            root.next_buffer(&mut buf);
        }
        drop(root);
        _rt.drain_metadata();

        let titles: mlua::Table = _rt.global("titles").expect("titles table");
        assert_eq!(titles.raw_len(), 2, "expected one event per track");
        assert_eq!(titles.get::<String>(1).expect("t1"), "sine 440 Hz");
        assert_eq!(titles.get::<String>(2).expect("t2"), "sine 880 Hz");
    }

    // ---- Phase 6: on_track ----------------------------------------------

    #[test]
    fn on_track_fires_at_each_track_boundary() {
        let (_rt, res) = run(
            r#"
            tracks = 0
            src = on_track(sequence({sine({freq = 440, duration = 0.1}),
                                     sine({freq = 880, duration = 0.1})}),
                           function() tracks = tracks + 1 end)
            output.preview(src)
            "#,
        )
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        for _ in 0..4 {
            root.next_buffer(&mut buf);
        }
        drop(root);
        _rt.drain_metadata();
        let tracks: u64 = _rt.global("tracks").expect("tracks counter");
        assert_eq!(tracks, 2, "one event per track start");
    }

    #[test]
    fn on_track_fires_for_a_short_child_that_exhausts() {
        let (_rt, res) = run(
            r#"
            tracks = {}
            src = on_track(blank({duration = 0.1}),
                           function() tracks[#tracks + 1] = true end)
            output.preview(src)
            "#,
        )
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        root.next_buffer(&mut buf);
        drop(root);
        _rt.drain_metadata();
        let tracks: mlua::Table = _rt.global("tracks").expect("tracks table");
        assert_eq!(tracks.raw_len(), 1, "blank labels itself, so one boundary");
    }

    /// Audio in two bursts with a stretch of paused (non-exhausted)
    /// zero-frame pulls between them, keeping the same label throughout.
    struct BurstySource {
        label: &'static str,
        burst1: usize,
        pause_pulls: usize,
        burst2: usize,
        state: u8, // 0 = burst 1, 1 = pause, 2 = burst 2, 3 = exhausted
    }

    impl BurstySource {
        fn new(label: &'static str) -> Self {
            Self { label, burst1: 100, pause_pulls: 1, burst2: 100, state: 0 }
        }
    }

    impl AudioSource for BurstySource {
        fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
            match self.state {
                0 if self.burst1 > 0 => {
                    let take = self.burst1.min(buffer.len());
                    buffer[..take].fill(0.5);
                    self.burst1 -= take;
                    take
                }
                0 => {
                    self.state = 1;
                    0
                }
                1 if self.pause_pulls > 0 => {
                    self.pause_pulls -= 1;
                    0
                }
                1 => {
                    self.state = 2;
                    0
                }
                2 if self.burst2 > 0 => {
                    let take = self.burst2.min(buffer.len());
                    buffer[..take].fill(0.5);
                    self.burst2 -= take;
                    take
                }
                2 => {
                    self.state = 3;
                    0
                }
                _ => 0,
            }
        }

        fn is_exhausted(&self) -> bool {
            self.state == 3
        }

        fn label(&self) -> Option<String> {
            Some(self.label.into())
        }
    }

    #[test]
    fn on_track_reports_a_resume_after_a_pause_without_any_label_change() {
        let (tx, rx) = std::sync::mpsc::channel();
        let child: Box<dyn AudioSource> = Box::new(BurstySource::new("same-label"));
        let mut src = OnTrackSource::new(child, tx, 0);
        let mut buf = vec![0f32; 50];

        // Burst 1: boundary #1 (None label -> "same-label").
        assert_eq!(src.next_buffer(&mut buf), 50);
        assert_eq!(src.next_buffer(&mut buf), 50);
        // Three zero-frame pulls while not exhausted (segment rollover,
        // the pause, and the rollover back into burst 2): silent, no event.
        assert_eq!(src.next_buffer(&mut buf), 0);
        assert_eq!(src.next_buffer(&mut buf), 0);
        assert_eq!(src.next_buffer(&mut buf), 0);
        // Burst 2 resumes with the *same* label: boundary #2 must still fire.
        assert_eq!(src.next_buffer(&mut buf), 50);
        assert_eq!(src.next_buffer(&mut buf), 50);
        assert_eq!(src.next_buffer(&mut buf), 0);
        assert!(src.is_exhausted());

        let count = |rx: &std::sync::mpsc::Receiver<ScriptEvent>| -> usize {
            let mut n = 0;
            while let Ok(e) = rx.try_recv() {
                assert!(matches!(e, ScriptEvent::Track { hook_id: 0 }));
                n += 1;
            }
            n
        };
        assert_eq!(count(&rx), 2, "one event per burst, despite one label");
    }

    #[test]
    fn multiple_outputs_share_one_root_source() {
        let (_rt, res) = run(
            r#"
            src = sine({freq = 440, duration = 1})
            output.icecast({mount = "/a.mp3", format = "mp3"}, src)
            output.icecast({mount = "/b.ogg", format = "opus"}, src)
            "#,
        )
        .expect("script runs");
        assert_eq!(res.outputs.len(), 2);
        assert!(res.root.is_some());
        assert_eq!(res.outputs[0].mount, "/a.mp3");
        assert_eq!(res.outputs[1].mount, "/b.ogg");
        assert_eq!(res.outputs[0].format, OutputFormat::Mp3);
        assert_eq!(res.outputs[1].format, OutputFormat::Opus);
    }

    #[test]
    fn different_roots_for_multiple_outputs_are_rejected() {
        let err = match run(
            r#"
            output.icecast({mount = "/a.mp3"}, sine({freq = 440}))
            output.icecast({mount = "/b.mp3"}, sine({freq = 880}))
            "#,
        ) {
            Ok(_) => panic!("second output with a different root must fail"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("share the same root source"));
    }

    #[test]
    fn file_output_registers_and_shares_root() {
        let (_rt, res) = run(
            r#"
            src = sine({freq = 440, duration = 1})
            output.file({path = "/tmp/crabsoup-c1.mp3", format = "mp3", bitrate = 64000}, src)
            output.icecast({mount = "/x.mp3"}, src)
            "#,
        )
        .expect("script runs");
        assert_eq!(res.file_outputs.len(), 1);
        assert_eq!(res.outputs.len(), 1);
        assert!(res.root.is_some());
        assert_eq!(res.file_outputs[0].format, OutputFormat::Mp3);
        assert_eq!(res.file_outputs[0].path.to_str(), Some("/tmp/crabsoup-c1.mp3"));
        assert_eq!(res.file_outputs[0].bitrate, 64_000);
    }

    #[test]
    fn file_output_requires_path_and_shared_root() {
        let err = match run(
            r#"
            output.file({format = "mp3"}, sine({freq = 440}))
            "#,
        ) {
            Ok(_) => panic!("output.file without path must fail"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("path is required"));

        let err = match run(
            r#"
            output.file({path = "/tmp/a.mp3"}, sine({freq = 440}))
            output.file({path = "/tmp/b.mp3"}, sine({freq = 880}))
            "#,
        ) {
            Ok(_) => panic!("second output.file with a different root must fail"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("share the same root source"));
    }

    #[test]
    fn hls_output_registers_and_defaults() {
        let (_rt, res) = run(
            r#"
            src = sine({freq = 440, duration = 1})
            output.hls({directory = "/tmp/crabsoup-hls"}, src)
            output.icecast({mount = "/x.mp3"}, src)
            "#,
        )
        .expect("script runs");
        assert_eq!(res.hls_outputs.len(), 1);
        assert_eq!(res.outputs.len(), 1);
        assert!(res.root.is_some());
        assert_eq!(res.hls_outputs[0].directory.to_str(), Some("/tmp/crabsoup-hls"));
        assert_eq!(res.hls_outputs[0].segment_seconds, 5.0);
        assert_eq!(res.hls_outputs[0].retention, 12);
    }

    #[test]
    fn hls_output_requires_directory() {
        let err = match run(
            r#"
            output.hls({}, sine({freq = 440}))
            "#,
        ) {
            Ok(_) => panic!("output.hls without directory must fail"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("directory is required"));
    }

    #[test]
    fn normalize_boosts_a_quiet_tone() {
        let (_rt, res) = run(
            r#"
            output.preview(normalize(sine({freq = 440, duration = 1, amplitude = 0.02}),
                                     {target = -6, attack = 0, release = 0}))
            "#,
        )
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        root.next_buffer(&mut buf);
        let peak = buf.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        // 0.02 is -34 dB; reaching -6 needs +28 dB, clamped to the 20 dB
        // max boost: 0.02 * 10 = 0.2.
        assert!((peak - 0.2).abs() < 0.01, "normalized peak {peak}");
    }

    // ---- Phase 5: switch / rotate ----------------------------------------

    /// Injects a fixed wall clock into a [`ScheduleSource`].
    fn fake_clock(src: &mut ScheduleSource, time: std::sync::Arc<std::sync::Mutex<LocalTime>>) {
        src.now = Box::new(move || *time.lock().unwrap());
    }

    /// Infinite child that emits a constant level and cycles through the
    /// given labels every `frames_per_track` frames (a mock of a
    /// crossfading playlist's label behaviour).
    struct LabelCycler {
        level: f32,
        labels: Vec<&'static str>,
        frames_per_track: usize,
        emitted: usize,
    }

    impl LabelCycler {
        fn new(level: f32, labels: &[&'static str], frames_per_track: usize) -> Self {
            Self {
                level,
                labels: labels.to_vec(),
                frames_per_track,
                emitted: 0,
            }
        }
    }

    impl AudioSource for LabelCycler {
        fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
            let n = buffer.len();
            buffer.fill(self.level);
            self.emitted += n;
            n
        }

        fn is_exhausted(&self) -> bool {
            false
        }

        fn label(&self) -> Option<String> {
            Some(self.labels[(self.emitted / self.frames_per_track) % self.labels.len()].into())
        }
    }

    #[test]
    fn time_predicate_matches_windows_and_days() {
        let always = TimePredicate::always();
        assert!(always.is_always());
        assert!(always.matches(&LocalTime { weekday: 3, minutes: 0 }));

        let window = TimePredicate {
            days: Some(vec![1, 2, 3, 4, 5]),
            from: Some(9 * 60),
            to: Some(17 * 60),
        };
        let t = LocalTime { weekday: 2, minutes: 12 * 60 };
        assert!(window.matches(&t));
        assert!(!window.matches(&LocalTime { weekday: 6, minutes: 12 * 60 }));
        assert!(!window.matches(&LocalTime { weekday: 2, minutes: 8 * 60 }));
        assert!(!window.matches(&LocalTime { weekday: 2, minutes: 17 * 60 }));
        // `to` is exclusive.
        assert!(!window.matches(&LocalTime { weekday: 2, minutes: 17 * 60 }));
    }

    #[test]
    fn time_predicate_wraps_past_midnight() {
        let overnight = TimePredicate {
            days: None,
            from: Some(22 * 60),
            to: Some(6 * 60),
        };
        assert!(overnight.matches(&LocalTime { weekday: 0, minutes: 23 * 60 }));
        assert!(overnight.matches(&LocalTime { weekday: 0, minutes: 3 * 60 }));
        assert!(!overnight.matches(&LocalTime { weekday: 0, minutes: 12 * 60 }));
        // from == to is an empty window.
        let empty = TimePredicate { days: None, from: Some(0), to: Some(0) };
        assert!(!empty.matches(&LocalTime { weekday: 0, minutes: 0 }));
    }

    #[test]
    fn switch_stays_in_a_window_and_moves_to_the_default_when_it_closes() {
        let clock = Arc::new(Mutex::new(LocalTime { weekday: 1, minutes: 9 * 60 }));
        let fpb = 100;
        let a = LuaSource::new(Box::new(LabelCycler::new(0.25, &["a1", "a2"], 300)));
        let b = LuaSource::new(Box::new(LabelCycler::new(0.75, &["b1", "b2"], 300)));
        let mut src = ScheduleSource::new(
            ScheduleKind::Switch(vec![
                SwitchSlot {
                    when: TimePredicate {
                        days: None,
                        from: Some(9 * 60),
                        to: Some(17 * 60),
                    },
                    child: 0,
                },
                SwitchSlot { when: TimePredicate::always(), child: 1 },
            ]),
            vec![a.clone(), b.clone()],
            true,
        );
        fake_clock(&mut src, clock.clone());

        let mut buf = vec![0f32; fpb];
        // Inside the window: plays child A (0.25).
        assert_eq!(src.next_buffer(&mut buf), fpb);
        assert!(buf.iter().all(|&s| s == 0.25));
        // After the window closes, A's next track boundary hands over to B.
        clock.lock().unwrap().minutes = 18 * 60;
        for _ in 0..3 {
            src.next_buffer(&mut buf);
        }
        assert!(src.next_buffer(&mut buf) > 0);
        assert!(buf.iter().all(|&s| s == 0.75), "expected the default child");
        assert_eq!(src.label().as_deref(), Some("b1"));
    }

    #[test]
    fn switch_track_sensitive_holds_mid_track_until_the_boundary() {
        let clock = Arc::new(Mutex::new(LocalTime { weekday: 1, minutes: 9 * 60 }));
        let fpb = 100;
        let a = LuaSource::new(Box::new(LabelCycler::new(0.25, &["a1", "a2"], 300)));
        let b = LuaSource::new(Box::new(LabelCycler::new(0.75, &["b1", "b2"], 300)));
        let mut src = ScheduleSource::new(
            ScheduleKind::Switch(vec![
                SwitchSlot {
                    when: TimePredicate {
                        days: None,
                        from: Some(9 * 60),
                        to: Some(10 * 60),
                    },
                    child: 0,
                },
                SwitchSlot { when: TimePredicate::always(), child: 1 },
            ]),
            vec![a.clone(), b.clone()],
            true,
        );
        fake_clock(&mut src, clock.clone());

        let mut buf = vec![0f32; fpb];
        // Two pulls inside the window, then the window closes mid-track.
        src.next_buffer(&mut buf);
        src.next_buffer(&mut buf);
        clock.lock().unwrap().minutes = 10 * 60 + 1;
        // The third track still plays out from A (0.25) despite the window
        // having closed — track-sensitive.
        src.next_buffer(&mut buf);
        assert!(buf.iter().all(|&s| s == 0.25));
        // Boundary after A's track (3 pulls = 300 frames = one track): B now.
        src.next_buffer(&mut buf);
        assert!(buf.iter().all(|&s| s == 0.75));
    }

    #[test]
    fn switch_with_track_sensitive_false_cuts_mid_track() {
        let clock = Arc::new(Mutex::new(LocalTime { weekday: 1, minutes: 9 * 60 }));
        let fpb = 100;
        let a = LuaSource::new(Box::new(LabelCycler::new(0.25, &["a1", "a2"], 300)));
        let b = LuaSource::new(Box::new(LabelCycler::new(0.75, &["b1", "b2"], 300)));
        let mut src = ScheduleSource::new(
            ScheduleKind::Switch(vec![
                SwitchSlot {
                    when: TimePredicate {
                        days: None,
                        from: Some(9 * 60),
                        to: Some(10 * 60),
                    },
                    child: 0,
                },
                SwitchSlot { when: TimePredicate::always(), child: 1 },
            ]),
            vec![a.clone(), b.clone()],
            false,
        );
        fake_clock(&mut src, clock.clone());

        let mut buf = vec![0f32; fpb];
        src.next_buffer(&mut buf);
        assert!(buf.iter().all(|&s| s == 0.25));
        clock.lock().unwrap().minutes = 10 * 60 + 1;
        // The very next pull already abandons A mid-track.
        src.next_buffer(&mut buf);
        assert!(buf.iter().all(|&s| s == 0.75));
    }

    #[test]
    fn rotate_cycles_children_one_track_at_a_time() {
        let fpb = 100;
        let a = LuaSource::new(Box::new(LabelCycler::new(0.25, &["a1", "a2"], 300)));
        let b = LuaSource::new(Box::new(LabelCycler::new(0.75, &["b1", "b2"], 300)));
        let mut src = ScheduleSource::new(
            ScheduleKind::Rotate { weights: vec![1, 1], cursor: 0, spins: 0 },
            vec![a.clone(), b.clone()],
            true,
        );

        let mut buf = vec![0f32; fpb];
        let mut seen = Vec::new();
        for _ in 0..12 {
            src.next_buffer(&mut buf);
            seen.push(buf[0]);
        }
        // One track = 300 frames = 3 pulls: A, A, A, B, B, B, A, ...
        assert_eq!(seen, vec![0.25, 0.25, 0.25, 0.75, 0.75, 0.75, 0.25, 0.25, 0.25, 0.75, 0.75, 0.75]);
    }

    #[test]
    fn rotate_with_weights_keeps_a_child_for_more_tracks() {
        let fpb = 100;
        let a = LuaSource::new(Box::new(LabelCycler::new(0.25, &["a1", "a2"], 300)));
        let b = LuaSource::new(Box::new(LabelCycler::new(0.75, &["b1", "b2"], 300)));
        let mut src = ScheduleSource::new(
            ScheduleKind::Rotate { weights: vec![1, 2], cursor: 0, spins: 0 },
            vec![a.clone(), b.clone()],
            true,
        );

        let mut buf = vec![0f32; fpb];
        let mut seen = Vec::new();
        for _ in 0..15 {
            src.next_buffer(&mut buf);
            seen.push(buf[0]);
        }
        // 1x A, 2x B: A A A, B B B, B B B, A A A, B B B, B B B, A A A.
        assert_eq!(
            seen,
            vec![
                0.25, 0.25, 0.25, 0.75, 0.75, 0.75, 0.75, 0.75, 0.75,
                0.25, 0.25, 0.25, 0.75, 0.75, 0.75,
            ]
        );
    }

    #[test]
    fn rotate_skips_an_exhausted_child() {
        let fpb = 20;
        // B is exhausted from the start (zero-length blank).
        let a = LuaSource::new(Box::new(LabelCycler::new(0.25, &["a1", "a2"], 100)));
        let b = LuaSource::new(Box::new(BlankSource::with_duration(0.0, 100)));
        let mut src = ScheduleSource::new(
            ScheduleKind::Rotate { weights: vec![1, 1], cursor: 0, spins: 0 },
            vec![a.clone(), b.clone()],
            true,
        );

        let mut buf = vec![0f32; fpb];
        let mut seen = Vec::new();
        for _ in 0..6 {
            src.next_buffer(&mut buf);
            seen.push(buf[0]);
        }
        // B is skipped forever: all audio comes from A (0.25).
        assert!(seen.iter().all(|&s| s == 0.25));
    }

    #[test]
    fn switch_skip_forces_a_repick() {
        let clock = Arc::new(Mutex::new(LocalTime { weekday: 1, minutes: 9 * 60 }));
        let fpb = 100;
        // A's first track is one buffer long (100 frames).
        let a = LuaSource::new(Box::new(LabelCycler::new(0.25, &["a1", "a2"], 100)));
        let b = LuaSource::new(Box::new(LabelCycler::new(0.75, &["b1", "b2"], 100)));
        let mut src = ScheduleSource::new(
            ScheduleKind::Switch(vec![
                SwitchSlot {
                    when: TimePredicate {
                        days: None,
                        from: Some(9 * 60),
                        to: Some(10 * 60),
                    },
                    child: 0,
                },
                SwitchSlot { when: TimePredicate::always(), child: 1 },
            ]),
            vec![a.clone(), b.clone()],
            true,
        );
        fake_clock(&mut src, clock.clone());

        let mut buf = vec![0f32; fpb];
        src.next_buffer(&mut buf);
        assert!(buf.iter().all(|&s| s == 0.25));
        // A skip lands on the default child even though the window is open:
        // the schedule is re-evaluated and the first match wins.
        clock.lock().unwrap().minutes = 11 * 60;
        src.skip();
        src.next_buffer(&mut buf);
        assert!(buf.iter().all(|&s| s == 0.75));
    }

    #[test]
    fn switch_registers_in_lua_with_a_default_child() {
        let (_rt, res) = run(
            r#"
            day = sine({freq = 440})
            night = sine({freq = 880})
            output.preview(switch({
                {when = {days = {"mon", "tue", "wed", "thu", "fri"}, from = "09:00", to = "17:00"}, src = day},
                {src = night}
            }))
            "#,
        )
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        root.next_buffer(&mut buf);
        // Whichever branch the wall clock picks (the window is weekday/time
        // dependent), switch must have composed and picked one of its
        // children — deterministic branch choice is covered by the
        // ScheduleSource tests with an injected clock.
        let label = root.label().expect("label");
        assert!(
            label == "sine 440 Hz" || label == "sine 880 Hz",
            "unexpected branch: {label}"
        );
    }

    #[test]
    fn switch_requires_a_default_child() {
        let err = run(
            r#"
            output.preview(switch({
                {when = {from = "09:00", to = "17:00"}, src = sine({freq = 440})}
            }))
            "#,
        )
        .err()
        .expect("script fails");
        assert!(err.to_string().contains("default child"));
    }

    #[test]
    fn switch_rejects_bad_times_and_weekdays() {
        let err = run(
            r#"
            output.preview(switch({
                {when = {from = "25:00", to = "17:00"}, src = sine({freq = 440})},
                {src = sine({freq = 880})}
            }))
            "#,
        )
        .err()
        .expect("script fails");
        assert!(err.to_string().contains("bad time"));

        let err = run(
            r#"
            output.preview(switch({
                {when = {days = {"funday"}}, src = sine({freq = 440})},
                {src = sine({freq = 880})}
            }))
            "#,
        )
        .err()
        .expect("script fails");
        assert!(err.to_string().contains("unknown weekday"));
    }

    #[test]
    fn rotate_registers_in_lua() {
        let (_rt, res) = run(
            r#"
            a = sine({freq = 440, duration = 0.2})
            b = sine({freq = 880, duration = 0.2})
            output.preview(rotate({a, b}, {weights = {1, 2}}))
            "#,
        )
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        root.next_buffer(&mut buf);
        assert_eq!(root.label().as_deref(), Some("sine 440 Hz"));
    }
}
