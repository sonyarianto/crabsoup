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

#[cfg(feature = "video")]
use crate::config::collect_video;
use crate::config::{
    ControlConfig, FileOutputConfig, HlsOutputConfig, LiveConfig, MixerConfig, OutputConfig,
    OutputFormat, OutputProtocol, SoundcardOutputConfig, StreamConfig, collect_audio,
};
use crate::engine::effects::{Agc, Amplify, Compressor, EffectSource};
use crate::engine::mixer::{CrossfadeMixer, SmartFade};
use crate::request::{RequestConfig, RequestUri, TrackCues, resolve};
use crate::source::blank_detect::{BlankDetectConfig, BlankDetectSource};
use crate::source::cue_cut::CueCutSource;
use crate::source::pipe::{PcmFormat, PipeConfig, PipeSource};
use crate::source::playlist::Playlist;
use crate::source::replaygain::ReplayGainSource;
use crate::source::request::{RequestQueue, RequestQueueSource};
use crate::source::soundcard::{SoundcardInputConfig, SoundcardInputSource};
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
    pub soundcard_outputs: Vec<SoundcardOutputConfig>,
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
    /// Video tracks registered via `video.video(path)` (Part H).
    #[cfg(feature = "video")]
    pub video: Vec<crate::video::VideoConfig>,
    /// Video playlists registered via `video.playlist(...)`/`video.single`
    /// (Part H7).
    #[cfg(feature = "video")]
    pub video_playlists: Vec<crate::video::VideoPlaylistConfig>,
    /// The shared video fan-out tap for the engine's video decode threads.
    #[cfg(feature = "video")]
    pub video_tap: Option<Arc<crate::video::VideoTap>>,
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
    soundcard_outputs: Vec<SoundcardOutputConfig>,
    request_queue: Option<Arc<RequestQueue>>,
    /// Named telnet commands registered by `server.register(name, fn)`;
    /// the names mirror into `ScriptResult` for the control port.
    custom_commands: Vec<(String, mlua::Function)>,
    /// The shared root source graph. First `output.icecast` call steals the
    /// box; later calls must pass the same `Arc` (checked via `ptr_eq`).
    root: Option<Box<dyn AudioSource>>,
    root_arc: Option<Arc<Mutex<Box<dyn AudioSource>>>>,
    preview: Option<Box<dyn AudioSource>>,
    /// Video tracks registered via `video.video(path)` (Part H).
    #[cfg(feature = "video")]
    video: Vec<crate::video::VideoConfig>,
    /// Video playlists registered via `video.playlist(...)`/`video.single`
    /// (Part H7).
    #[cfg(feature = "video")]
    video_playlists: Vec<crate::video::VideoPlaylistConfig>,
    /// The shared video fan-out tap for the engine's video decode threads.
    #[cfg(feature = "video")]
    video_tap: Option<Arc<crate::video::VideoTap>>,
    /// Lua callbacks registered by `on_metadata`; indexed by hook id, live
    /// on the Lua-owning thread only.
    metadata_hooks: Vec<mlua::Function>,
    /// Lua callbacks registered by `on_track`; indexed by hook id. Kept
    /// separate from `metadata_hooks` so a `Track` event never calls an
    /// `on_metadata` callback (and vice versa).
    track_hooks: Vec<mlua::Function>,
    /// Lua callbacks registered by `request.dynamic`; indexed by hook id.
    /// Each returns the next request URI or nil to end the source.
    dynamic_hooks: Vec<mlua::Function>,
    /// Lua callbacks registered by `blank.detect`'s `on_blank` option;
    /// indexed by hook id, invoked when a wrapped source goes blank.
    blank_hooks: Vec<mlua::Function>,
    /// Lua callbacks registered by `map_metadata`; indexed by hook id, each
    /// rewrites a track title (returning a table or nil).
    map_metadata_hooks: Vec<mlua::Function>,
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
    /// `request.dynamic`: ask the Lua callback for the next request URI
    /// (`Some(uri)`) or the end of the stream (`None`). The audio thread
    /// never blocks on the reply — it polls and returns silence meanwhile.
    NextRequest {
        index: usize,
        reply: mpsc::Sender<Option<String>>,
    },
    /// A `blank.detect` source went blank (dead air detected).
    Blank { hook_id: usize },
    /// `map_metadata`: rewrite a track's title. The callback runs on the
    /// Lua-owning thread with `{ title = ... }` and replies with the
    /// rewritten title (or `None` to keep the original).
    MapMetadata {
        hook_id: usize,
        title: String,
        reply: mpsc::Sender<Option<String>>,
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
    dynamic_hooks: Vec<mlua::Function>,
    blank_hooks: Vec<mlua::Function>,
    map_metadata_hooks: Vec<mlua::Function>,
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
            ScriptEvent::NextRequest { index, reply } => {
                let Some(cb) = self.dynamic_hooks.get(index) else {
                    let _ = reply.send(None);
                    return;
                };
                let next = match cb.call::<Option<String>>(()) {
                    Ok(Some(uri)) => Some(uri),
                    Ok(None) => None,
                    Err(e) => {
                        log::warn!("request.dynamic callback error: {e}");
                        None
                    }
                };
                let _ = reply.send(next);
            }
            ScriptEvent::Blank { hook_id } => {
                let Some(cb) = self.blank_hooks.get(hook_id) else {
                    return;
                };
                if let Err(e) = cb.call::<()>(()) {
                    log::warn!("blank.detect callback error: {e}");
                }
            }
            ScriptEvent::MapMetadata {
                hook_id,
                title,
                reply,
            } => {
                let Some(cb) = self.map_metadata_hooks.get(hook_id) else {
                    let _ = reply.send(None);
                    return;
                };
                let table = match self.lua.create_table() {
                    Ok(t) => t,
                    Err(e) => {
                        log::warn!("map_metadata callback: table error: {e}");
                        let _ = reply.send(None);
                        return;
                    }
                };
                if let Err(e) = table.set("title", title.as_str()) {
                    log::warn!("map_metadata callback: {e}");
                    let _ = reply.send(None);
                    return;
                }
                // The callback returns a (possibly modified) table; a `title`
                // field replaces the original, anything else keeps it.
                let out = match cb.call::<Option<Table>>(table) {
                    Ok(Some(t)) => t.get::<Option<String>>("title").ok().flatten(),
                    Ok(None) => None,
                    Err(e) => {
                        log::warn!("map_metadata callback error: {e}");
                        None
                    }
                };
                let _ = reply.send(out);
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

    fn crossfade_overrides(&self) -> Option<(Option<f64>, Option<f64>)> {
        self.child.crossfade_overrides()
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

    fn crossfade_overrides(&self) -> Option<(Option<f64>, Option<f64>)> {
        self.child.crossfade_overrides()
    }

    fn skip(&mut self) {
        self.child.skip();
    }
}

/// Pull budget while a `map_metadata` rewrite is outstanding: at ~92.9 ms
/// per 4096-frame buffer this is ~0.7 s of wall time, well past the event
/// loop's 100 ms poll, without ever blocking the audio thread.
const MAP_METADATA_PULL_BUDGET: u32 = 8;

/// Wrapper source that rewrites its child's label through a Lua callback
/// (Liquidsoap `map_metadata`). Unlike [`OnMetadataSource`] (fire-and-forget)
/// the rewrite actually has to *reach* the output, so the source polls the
/// Lua reply for a bounded number of pulls and falls back to the original
/// label on timeout or callback error. The reply is carried over the same
/// A2 event-loop bridge as every other callback — only owned `Send` strings
/// cross threads.
struct MapMetadataSource {
    child: Box<dyn AudioSource>,
    event_tx: mpsc::Sender<ScriptEvent>,
    hook_id: usize,
    /// Raw child label of the current track.
    raw: Option<String>,
    /// Rewritten label from the Lua callback; `Some` once a reply lands.
    mapped: Option<String>,
    /// Reply channel of the in-flight rewrite, while one is outstanding.
    pending: Option<mpsc::Receiver<Option<String>>>,
    /// Pulls left before giving up on the reply (raw label wins).
    pulls_left: u32,
}

impl MapMetadataSource {
    fn new(
        child: Box<dyn AudioSource>,
        event_tx: mpsc::Sender<ScriptEvent>,
        hook_id: usize,
    ) -> Self {
        Self {
            child,
            event_tx,
            hook_id,
            raw: None,
            mapped: None,
            pending: None,
            pulls_left: 0,
        }
    }

    /// Ask the Lua callback for a rewrite of the current (raw) label.
    fn request_rewrite(&mut self) {
        let (reply_tx, reply_rx) = mpsc::channel();
        let _ = self.event_tx.send(ScriptEvent::MapMetadata {
            hook_id: self.hook_id,
            title: self.raw.clone().unwrap_or_default(),
            reply: reply_tx,
        });
        self.pending = Some(reply_rx);
        self.pulls_left = MAP_METADATA_PULL_BUDGET;
    }

    /// Collect an outstanding reply without blocking; `None` on give-up.
    fn poll_reply(&mut self) {
        let Some(rx) = self.pending.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(Some(title)) => self.mapped = Some(title),
            Ok(None) => {}
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if self.pulls_left > 0 {
                    self.pulls_left -= 1;
                    self.pending = Some(rx);
                }
                // Budget exhausted: keep the raw label.
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
        }
    }
}

impl AudioSource for MapMetadataSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        let n = self.child.next_buffer(buffer);
        let raw = self.child.label();
        if raw != self.raw {
            // Track boundary: fire a rewrite for the new title. Fires even
            // when the child carries no label, so a script can add titles to
            // unlabeled tracks.
            self.raw = raw;
            self.mapped = None;
            self.request_rewrite();
        }
        self.poll_reply();
        n
    }

    fn is_exhausted(&self) -> bool {
        self.child.is_exhausted()
    }

    fn remaining_seconds(&self) -> Option<f64> {
        self.child.remaining_seconds()
    }

    fn label(&self) -> Option<String> {
        match &self.mapped {
            Some(mapped) => Some(mapped.clone()),
            None => self.raw.clone().or_else(|| self.child.label()),
        }
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

/// Liquidsoap `request.dynamic`: plays requests returned by a Lua callback,
/// one ahead of the current track.
///
/// The callback (invoked on the Lua-owning thread through the A2 event
/// loop) returns the next request URI as a string, or nil to end the
/// source. The audio thread never blocks on the Lua reply: it sends a
/// [`ScriptEvent::NextRequest`], polls the reply channel without waiting,
/// and returns silence (or the current track's audio) until the answer
/// lands. The next URI is requested as soon as a track is promoted, so a
/// fast callback makes handovers gapless. Requests resolve like any other
/// URI (`annotate:` prefixes, `http://` downloads, retries); a request that
/// fails to resolve is logged and skipped, and the callback is asked again.
/// While a reply is outstanding the source is *not* exhausted (it does not
/// yet know if more tracks are coming), so a `fallback` holds on it rather
/// than falling through — like a temporarily-silent child, unlike
/// `request.queue` which reports exhausted when empty.
struct DynamicRequestSource {
    /// Channel to the Lua-owning thread's event loop.
    event_tx: mpsc::Sender<ScriptEvent>,
    /// Index into the runtime's `dynamic_hooks`.
    index: usize,
    request: RequestConfig,
    target: SignalSpec,
    frames_per_buffer: usize,
    /// The track currently playing.
    current: Option<Box<dyn AudioSource>>,
    current_uri: Option<RequestUri>,
    /// Resolved-but-not-yet-promoted next track (the prefetch).
    next_uri: Option<RequestUri>,
    /// A `NextRequest` reply outstanding; `Some(rx)` while we wait.
    pending_reply: Option<mpsc::Receiver<Option<String>>>,
    /// The callback returned nil: no tracks after the current one.
    no_more: bool,
}

impl DynamicRequestSource {
    fn new(
        event_tx: mpsc::Sender<ScriptEvent>,
        index: usize,
        request: RequestConfig,
        target: SignalSpec,
        frames_per_buffer: usize,
    ) -> Self {
        Self {
            event_tx,
            index,
            request,
            target,
            frames_per_buffer,
            current: None,
            current_uri: None,
            next_uri: None,
            pending_reply: None,
            no_more: false,
        }
    }

    /// Ask the Lua callback for the next request URI.
    fn request_next(&mut self) {
        let (reply_tx, reply_rx) = mpsc::channel();
        match self.event_tx.send(ScriptEvent::NextRequest {
            index: self.index,
            reply: reply_tx,
        }) {
            Ok(()) => self.pending_reply = Some(reply_rx),
            // Event loop is gone: nothing more will ever come.
            Err(_) => self.no_more = true,
        }
    }

    /// Collect an outstanding Lua reply without blocking. Returns true when
    /// a reply was consumed.
    fn poll_reply(&mut self) -> bool {
        let Some(rx) = self.pending_reply.take() else {
            return false;
        };
        match rx.try_recv() {
            Ok(Some(uri)) => {
                self.next_uri = Some(RequestUri::new(&uri));
                true
            }
            Ok(None) => {
                self.no_more = true;
                true
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                // Still waiting; check again on the next pull.
                self.pending_reply = Some(rx);
                false
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.no_more = true;
                true
            }
        }
    }
}

impl AudioSource for DynamicRequestSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        loop {
            // Collect an outstanding reply, then ensure a request is in
            // flight whenever we do not know the next track.
            self.poll_reply();
            if self.next_uri.is_none() && self.pending_reply.is_none() && !self.no_more {
                self.request_next();
            }
            // Promote a known next track into the current slot and prefetch
            // the one after it while it plays.
            if self.current.is_none() {
                if let Some(uri) = self.next_uri.take() {
                    match resolve(&uri, &self.request, self.target, self.frames_per_buffer) {
                        Ok(src) => {
                            log::info!("request.dynamic: playing {}", uri.display());
                            self.current = Some(src);
                            self.current_uri = Some(uri);
                            if self.next_uri.is_none()
                                && self.pending_reply.is_none()
                                && !self.no_more
                            {
                                self.request_next();
                            }
                        }
                        Err(e) => {
                            log::warn!("request.dynamic: cannot play {}: {e}", uri.display());
                            // Skip the bad request and ask the callback again.
                            continue;
                        }
                    }
                } else {
                    // No next track known: wait for Lua (or the end).
                    return 0;
                }
            }
            let Some(current) = self.current.as_mut() else {
                return 0;
            };
            let n = current.next_buffer(buffer);
            if n > 0 {
                return n;
            }
            if current.is_exhausted() {
                log::info!(
                    "request.dynamic: finished {}",
                    self.current_uri
                        .as_ref()
                        .map(|u| u.display())
                        .unwrap_or_default()
                );
                self.current = None;
                self.current_uri = None;
                continue;
            }
            // Current track is temporarily silent; hold on it.
            return 0;
        }
    }

    fn is_exhausted(&self) -> bool {
        self.current.is_none() && self.next_uri.is_none() && self.no_more
    }

    fn remaining_seconds(&self) -> Option<f64> {
        self.current.as_ref().and_then(|c| c.remaining_seconds())
    }

    fn label(&self) -> Option<String> {
        self.current_uri.as_ref().map(|uri| uri.display())
    }

    fn replaygain_db(&self) -> Option<f32> {
        self.current.as_ref().and_then(|c| c.replaygain_db())
    }

    fn crossfade_overrides(&self) -> Option<(Option<f64>, Option<f64>)> {
        self.current.as_ref().and_then(|c| c.crossfade_overrides())
    }

    fn skip(&mut self) {
        // Advance to the next prefetched track (a skip is a boundary).
        self.current = None;
        self.current_uri = None;
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
            other => return Err(mlua::Error::runtime(format!("unknown setting \"{other}\""))),
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
        Self {
            children,
            current: 0,
        }
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
            return self.children[i]
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

    fn crossfade_overrides(&self) -> Option<(Option<f64>, Option<f64>)> {
        if let Some(i) = self.active() {
            return self.children[i].0.lock().unwrap().crossfade_overrides();
        }
        self.children
            .get(self.current)
            .and_then(|c| c.0.lock().unwrap().crossfade_overrides())
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

    fn crossfade_overrides(&self) -> Option<(Option<f64>, Option<f64>)> {
        self.order
            .first()
            .and_then(|&i| self.children[i].0.lock().unwrap().crossfade_overrides())
    }
}

/// Sums N children sample-by-sample (Liquidsoap `add`): a background bed
/// plus a voice-over, layered intros, etc. Optional per-child `weights`
/// scale each source before summing (default 1.0). The mix is not
/// normalized — clipping is the caller's concern (use `weights`).
struct AddSource {
    children: Vec<LuaSource>,
    weights: Vec<f32>,
    /// Reusable scratch for children after the first, sized on demand so
    /// `next_buffer` never allocates on the hot path.
    scratch: Vec<f32>,
}

impl AddSource {
    fn new(children: Vec<LuaSource>, weights: Vec<f32>) -> Self {
        Self {
            children,
            weights,
            scratch: Vec::new(),
        }
    }
}

impl AudioSource for AddSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        let mut out_len = 0;
        for (i, child) in self.children.iter().enumerate() {
            let w = self.weights[i];
            if i == 0 {
                let n = child.0.lock().unwrap().next_buffer(buffer);
                // Samples beyond the first child's fill are undefined; the
                // other children are added into them below.
                if n < buffer.len() {
                    buffer[n..].fill(0.0);
                }
                for s in &mut buffer[..n] {
                    *s *= w;
                }
                out_len = n;
            } else {
                if self.scratch.len() != buffer.len() {
                    self.scratch.resize(buffer.len(), 0.0);
                }
                let n = child.0.lock().unwrap().next_buffer(&mut self.scratch);
                for (out, s) in buffer[..n].iter_mut().zip(&self.scratch[..n]) {
                    *out += *s * w;
                }
                out_len = out_len.max(n);
            }
        }
        out_len
    }

    fn is_exhausted(&self) -> bool {
        // The sum ends when every child has ended; a looping bed keeps the
        // mix alive indefinitely.
        self.children
            .iter()
            .all(|c| c.0.lock().unwrap().is_exhausted())
    }

    fn remaining_seconds(&self) -> Option<f64> {
        self.children
            .first()
            .and_then(|c| c.0.lock().unwrap().remaining_seconds())
    }

    fn label(&self) -> Option<String> {
        self.children
            .first()
            .and_then(|c| c.0.lock().unwrap().label())
    }

    fn replaygain_db(&self) -> Option<f32> {
        self.children
            .first()
            .and_then(|c| c.0.lock().unwrap().replaygain_db())
    }

    fn crossfade_overrides(&self) -> Option<(Option<f64>, Option<f64>)> {
        self.children
            .first()
            .and_then(|c| c.0.lock().unwrap().crossfade_overrides())
    }

    fn skip(&mut self) {
        for child in &self.children {
            child.0.lock().unwrap().skip();
        }
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
            if child.is_exhausted() {
                None
            } else {
                Some(slot.child)
            }
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
        self.children
            .iter()
            .all(|c| c.0.lock().unwrap().is_exhausted())
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

    fn crossfade_overrides(&self) -> Option<(Option<f64>, Option<f64>)> {
        self.children
            .get(self.current)
            .and_then(|c| c.0.lock().unwrap().crossfade_overrides())
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
        _ => {
            return Err(mlua::Error::runtime(format!(
                "switch: bad time {value:?}, use \"HH:MM\""
            )));
        }
    };
    let h: u32 = h
        .parse()
        .map_err(|_| mlua::Error::runtime(format!("switch: bad time {value:?}")))?;
    let m: u32 = m
        .parse()
        .map_err(|_| mlua::Error::runtime(format!("switch: bad time {value:?}")))?;
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
            )));
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
                        ));
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

/// Collect and validate the audio requests for a `playlist`-style source
/// table (`directory` and/or `files`), sorted and deduped.
fn playlist_requests(opts: &Table) -> mlua::Result<Vec<RequestUri>> {
    let directory: Option<String> = opts.get("directory").ok().flatten();
    let files: Vec<String> = opts.get("files").ok().unwrap_or_default();
    let mut requests = Vec::new();
    if let Some(dir) = &directory {
        let mut paths = Vec::new();
        collect_audio(&PathBuf::from(dir), &mut paths);
        requests.extend(
            paths
                .into_iter()
                .map(|p| RequestUri::new(p.to_str().unwrap_or_default())),
        );
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
    Ok(requests)
}

/// Collect and validate the video tracks for a `video.playlist` table
/// (`directory` and/or `files`), sorted and deduped. Every file is probed
/// at script evaluation; unreadable files are skipped with a warning.
/// All tracks must share one resolution: video outputs open their encoders
/// at the first track's spec, and a differently-sized frame would kill the
/// encode. `frame_rate` may differ (the encoder is PTS-driven).
#[cfg(feature = "video")]
fn video_playlist_configs(opts: &Table) -> mlua::Result<Vec<crate::video::VideoConfig>> {
    let directory: Option<String> = opts.get("directory").ok().flatten();
    let files: Vec<String> = opts.get("files").ok().unwrap_or_default();
    let mut paths = Vec::new();
    if let Some(dir) = &directory {
        collect_video(&PathBuf::from(dir), &mut paths);
    }
    paths.extend(files.iter().map(PathBuf::from));
    paths.sort();
    paths.dedup();
    let mut tracks = Vec::new();
    for path in paths {
        match crate::video::VideoSource::validate(&path) {
            Ok(spec) => tracks.push(crate::video::VideoConfig { path, spec }),
            Err(e) => log::warn!("video playlist skipping {}: {e}", path.display()),
        }
    }
    if tracks.is_empty() {
        return Err(mlua::Error::runtime(
            "video.playlist: no video files found (check `directory`/`files`)",
        ));
    }
    let (w, h) = (tracks[0].spec.width, tracks[0].spec.height);
    if let Some(bad) = tracks
        .iter()
        .find(|t| t.spec.width != w || t.spec.height != h)
    {
        return Err(mlua::Error::runtime(format!(
            "video.playlist: all tracks must share one resolution \
             ({w}x{h}), got {}x{} in {}",
            bad.spec.width,
            bad.spec.height,
            bad.path.display()
        )));
    }
    Ok(tracks)
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
    Box::new(CrossfadeMixer::new(
        Box::new(playlist),
        &mixer_cfg,
        spec.rate,
        chans,
    ))
}

/// Evaluate a `.lua` script and return the runtime plus the engine wiring.
/// Convert a Lua table into a JSON value for `http_post`. Supports
/// string/integer keys with string/number/boolean/table values, and
/// array-shaped tables (consecutive integer keys from 1).
fn table_to_json(t: &mlua::Table) -> Result<serde_json::Value, String> {
    let mut obj = serde_json::Map::new();
    let mut is_array = true;
    let mut next_index = 1usize;
    for pair in t.clone().pairs::<mlua::Value, mlua::Value>() {
        let (key, value) = pair.map_err(|e| e.to_string())?;
        let json = value_to_json(&value)?;
        match key {
            mlua::Value::Integer(i) if i >= 1 => {
                if is_array && i as usize == next_index {
                    obj.insert(next_index.to_string(), json);
                    next_index += 1;
                } else {
                    is_array = false;
                    obj.insert(i.to_string(), json);
                }
            }
            mlua::Value::String(s) => {
                is_array = false;
                obj.insert(s.to_string_lossy(), json);
            }
            other => {
                is_array = false;
                obj.insert(other.to_string().map_err(|e| e.to_string())?, json);
            }
        }
    }
    if is_array && next_index > 1 {
        let mut items = Vec::with_capacity(next_index - 1);
        for i in 1..next_index {
            items.push(
                obj.remove(&i.to_string())
                    .unwrap_or(serde_json::Value::Null),
            );
        }
        Ok(serde_json::Value::Array(items))
    } else {
        Ok(serde_json::Value::Object(obj))
    }
}

fn value_to_json(v: &mlua::Value) -> Result<serde_json::Value, String> {
    Ok(match v {
        mlua::Value::Nil => serde_json::Value::Null,
        mlua::Value::Boolean(b) => serde_json::Value::Bool(*b),
        mlua::Value::Integer(i) => serde_json::Value::Number((*i).into()),
        mlua::Value::Number(n) => serde_json::Value::from(*n),
        mlua::Value::String(s) => serde_json::Value::String(s.to_string_lossy()),
        mlua::Value::Table(t) => table_to_json(t)?,
        other => {
            return Err(format!(
                "unsupported Lua value in http_post payload: {}",
                other.type_name()
            ));
        }
    })
}

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

    // ---- outbound webhook (Crabcast track-change events) ---------------
    // Fire-and-forget: spawn a thread so the Lua event loop never blocks
    // on the network. The payload is a Lua table serialized to JSON.
    globals.set(
        "http_post",
        lua.create_function(|_, (url, payload): (String, mlua::Table)| {
            let value = table_to_json(&payload).map_err(mlua::Error::runtime)?;
            let body = serde_json::to_string(&value)
                .map_err(|e| mlua::Error::runtime(format!("http_post: {e}")))?;
            std::thread::spawn(move || {
                let timeout = Duration::from_secs(5);
                if let Err(e) = crate::request::http_post_json(&url, &body, timeout, None) {
                    log::warn!("http_post to {url} failed: {e}");
                }
            });
            Ok(())
        })?,
    )?;

    // ---- source constructors -------------------------------------------
    let pl_state = state.clone();
    globals.set(
        "playlist",
        lua.create_function(move |_, opts: Table| {
            let requests = playlist_requests(&opts)?;
            let shuffle: bool = opts.get("shuffle").unwrap_or(false);
            // Option-bool: mlua converts a missing key to Ok(false), so a
            // plain unwrap_or(true) would default to *not* looping.
            let loop_playlist: bool = opts.get("loop").ok().flatten().unwrap_or(true);
            let src = crossfading_playlist(requests, shuffle, loop_playlist, &pl_state);
            Ok(LuaSource::new(src))
        })?,
    )?;

    // ---- smart crossfade (Liquidsoap `smart_crossfade`) ---------------------
    // A `playlist` whose per-transition window is chosen by the outgoing
    // track's measured tail level: a loud tail gets a full `fade_out`
    // crossfade, a quiet tail only a short `fade_mid` fade. `fade_out`
    // defaults to the global `crossfade_seconds`; `fade_mid` to half of it;
    // `threshold` (dBFS, default -30) decides "quiet".
    let smart_state = state.clone();
    globals.set(
        "smart_crossfade",
        lua.create_function(move |_, opts: Table| {
            let requests = playlist_requests(&opts)?;
            let shuffle: bool = opts.get("shuffle").unwrap_or(false);
            // Option-bool: mlua converts a missing key to Ok(false), so a
            // plain unwrap_or(true) would default to *not* looping.
            let loop_playlist: bool = opts.get("loop").ok().flatten().unwrap_or(true);
            let global = smart_state.borrow().mixer.crossfade_seconds;
            let fade_out: f64 = opts.get::<Option<f64>>("fade_out")?.unwrap_or(global);
            let fade_mid: f64 = opts
                .get::<Option<f64>>("fade_mid")?
                .unwrap_or(fade_out / 2.0);
            let threshold_db: f32 = opts.get("threshold").unwrap_or(-30.0);
            let (spec, fpb) = bus(&smart_state);
            let chans = spec.channels.count();
            let mixer_cfg = smart_state.borrow().mixer.clone();
            let request = smart_state.borrow().request;
            let playlist =
                Playlist::new(requests, shuffle, loop_playlist, request, spec, fpb, None);
            let mixer = CrossfadeMixer::new(Box::new(playlist), &mixer_cfg, spec.rate, chans)
                .with_smart_fade(SmartFade {
                    fade_out,
                    fade_mid,
                    threshold_db,
                });
            Ok(LuaSource::new(Box::new(mixer)))
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

    // ---- dynamic requests (Liquidsoap `request.dynamic`) -----------------
    // The callback runs on the Lua-owning event loop (A2 bridge); it
    // returns the next request URI as a string, or nil to end the source.
    let dyn_state = state.clone();
    let dyn_tx = event_tx.clone();
    request.set(
        "dynamic",
        lua.create_function(move |_, callback: mlua::Function| {
            let (spec, fpb) = bus(&dyn_state);
            let request = dyn_state.borrow().request;
            let index = dyn_state.borrow().dynamic_hooks.len();
            dyn_state.borrow_mut().dynamic_hooks.push(callback);
            let src = DynamicRequestSource::new(dyn_tx.clone(), index, request, spec, fpb);
            Ok(LuaSource::new(Box::new(src)))
        })?,
    )?;
    globals.set("request", request)?;

    // ---- test sources (Liquidsoap `blank`, `sine`) -----------------------
    // `blank` is a callable table: `blank({duration})` makes a silence source
    // and `blank.detect(src, opts)` wraps a source against dead air (Part
    // F4). Lua calls a table's `__call` as `f(self, args...)`, hence the
    // leading `_self` parameter.
    let blank_state = state.clone();
    let blank_fn = lua.create_function(move |_, (_self, opts): (Table, Option<Table>)| {
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
    })?;
    let blank_detect_state = state.clone();
    let blank_detect_tx = event_tx.clone();
    let blank_detect_fn =
        lua.create_function(move |_, (mut source, opts): (LuaSource, Option<Table>)| {
            let threshold = opt_f64(&opts, "threshold", -40.0)?;
            let duration = opt_f64(&opts, "duration", 2.0)?;
            let restart = opt_f64(&opts, "restart", 1.0)?;
            // Read as Option<bool>: mlua maps a *missing* field to `false`
            // for a plain `bool` target (nil is falsy), which would flip the
            // safe default.
            let exhaust_while_blank: bool = match &opts {
                Some(t) => t
                    .get::<Option<bool>>("exhaust_while_blank")?
                    .unwrap_or(true),
                None => true,
            };
            let on_blank: Option<mlua::Function> = match &opts {
                Some(t) => t.get("on_blank")?,
                None => None,
            };
            let (spec, _) = bus(&blank_detect_state);
            let child = source.take();
            let on_blank = match on_blank {
                Some(cb) => {
                    let hook_id = blank_detect_state.borrow().blank_hooks.len();
                    blank_detect_state.borrow_mut().blank_hooks.push(cb);
                    let tx = blank_detect_tx.clone();
                    Some(Box::new(move || {
                        let _ = tx.send(ScriptEvent::Blank { hook_id });
                    }) as Box<dyn FnMut() + Send>)
                }
                None => None,
            };
            let wrapped = BlankDetectSource::new(
                child,
                BlankDetectConfig {
                    threshold_db: threshold as f32,
                    duration_secs: duration as f32,
                    restart_secs: restart as f32,
                    exhaust_while_blank,
                    on_blank,
                    sample_rate: spec.rate,
                    channels: spec.channels.count(),
                },
            );
            Ok(LuaSource::new(Box::new(wrapped)))
        })?;
    let blank = lua.create_table()?;
    blank.set("detect", blank_detect_fn)?;
    let blank_mt = lua.create_table()?;
    blank_mt.set("__call", blank_fn)?;
    blank.set_metatable(Some(blank_mt));
    globals.set("blank", blank)?;

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
            let src = EffectSource::new(child, Amplify::new(gain as f32), spec.channels.count());
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
            other => return Err(mlua::Error::runtime(format!("unknown composer {other}"))),
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

    // ---- additive mix (Liquidsoap `add`) ----------------------------------
    globals.set(
        "add",
        lua.create_function(|_lua, (children, opts): (Table, Option<Table>)| {
            let sources = source_list(&children)?;
            let n = sources.len();
            let weights: Vec<f32> = match &opts {
                Some(t) => t.get("weights").unwrap_or_default(),
                None => Vec::new(),
            };
            let weights = if weights.is_empty() {
                vec![1.0; n]
            } else {
                if weights.len() != n {
                    return Err(mlua::Error::runtime(
                        "add: `weights` must have one entry per source",
                    ));
                }
                weights
            };
            let composed: Box<dyn AudioSource> = Box::new(AddSource::new(sources, weights));
            Ok(LuaSource::new(composed))
        })?,
    )?;

    // ---- cue cutting (Liquidsoap `annotate:`/`cue_cut`) --------------------
    let cue_state = state.clone();
    globals.set(
        "cue_cut",
        lua.create_function(move |_, (mut source, opts): (LuaSource, Option<Table>)| {
            let cue_in = opt_f64(&opts, "cue_in", 0.0)?;
            let cue_out = match &opts {
                Some(t) => t.get::<Option<f64>>("cue_out")?,
                None => None,
            };
            let fade_in = match &opts {
                Some(t) => t.get::<Option<f64>>("fade_in")?,
                None => None,
            };
            let fade_out = match &opts {
                Some(t) => t.get::<Option<f64>>("fade_out")?,
                None => None,
            };
            let (spec, _) = bus(&cue_state);
            let child = source.take();
            let cues = TrackCues {
                cue_in,
                cue_out,
                fade_in,
                fade_out,
            };
            let wrapped = CueCutSource::new(child, cues, spec.rate, spec.channels.count());
            Ok(LuaSource::new(Box::new(wrapped)))
        })?,
    )?;

    // ---- external process pipeline (Liquidsoap `pipe`) --------------------
    // Shells out to an external raw-PCM processor (stdin/stdout): a writer
    // thread feeds the child source to the process and a reader thread
    // decodes its output back into the graph (see `src/source/pipe.rs`).
    // The source is consumed like any other operator and wrapped in a fresh
    // Arc that the bridge threads share (bypass mode pulls it directly).
    let pipe_state = state.clone();
    globals.set(
        "pipe",
        lua.create_function(move |_, (opts, mut source): (Table, LuaSource)| {
            let process: String = opts
                .get("process")
                .map_err(|_| mlua::Error::runtime("pipe: `process` is required"))?;
            let format = opts
                .get::<Option<String>>("format")?
                .unwrap_or_else(|| "s16le".into());
            let format = match format.as_str() {
                "s16le" => PcmFormat::S16Le,
                "s24le" => PcmFormat::S24Le,
                other => {
                    return Err(mlua::Error::runtime(format!(
                        "pipe: unknown format {other:?} (use \"s16le\" or \"s24le\")"
                    )));
                }
            };
            let config = PipeConfig {
                format,
                restart_backoff_ms: opts.get("restart_backoff").unwrap_or(500),
            };
            let (spec, fpb) = bus(&pipe_state);
            let src = PipeSource::spawn(
                &process,
                Arc::new(Mutex::new(source.take())),
                spec.channels.count(),
                fpb,
                config,
            )
            .map_err(mlua::Error::runtime)?;
            Ok(LuaSource::new(Box::new(src)))
        })?,
    )?;

    // ---- mksafe (Liquidsoap defensive wrapper) ---------------------------
    // Wraps any source so it never fails outright: when the child exhausts
    // (or is a request source that failed to resolve), an infinite blank
    // produces silence instead of the engine erroring out. The child is
    // re-checked from the top on every pull, so a `request.queue` that
    // receives a push later preempts the silence again.
    globals.set(
        "mksafe",
        lua.create_function(|_, source: LuaSource| {
            let silence = LuaSource::new(Box::new(BlankSource::new()));
            Ok(LuaSource::new(Box::new(FallbackSource::new(vec![
                source, silence,
            ]))))
        })?,
    )?;

    // ---- daypart scheduling (Liquidsoap `switch`, `rotate`) --------------
    globals.set(
        "switch",
        lua.create_function(|_lua, (slots, opts): (Table, Option<Table>)| {
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
        })?,
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
                ScheduleKind::Rotate {
                    weights,
                    cursor: 0,
                    spins: 0,
                },
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
            let requests = paths
                .into_iter()
                .map(|p| RequestUri::Local(p, None))
                .collect();
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
            extra_passwords: opts.get("extra_passwords").ok().unwrap_or_default(),
        };
        harbor_state.borrow_mut().harbor = Some(cfg);
        // The harbor drives the priority mixer via MixCommand; the value
        // is a marker that exhausts immediately when composed.
        Ok(LuaSource::new(Box::new(SilenceSource::new())))
    })?;
    // ---- soundcard capture (Liquidsoap `input.soundcard`) -------------------
    // Opens the device at script evaluation (fail fast); the cpal stream runs
    // on its own realtime thread and the source drains it on the pull thread.
    let sc_state = state.clone();
    let soundcard_fn = lua.create_function(move |_, opts: Option<Table>| {
        let device: Option<String> = match &opts {
            Some(t) => t.get("device")?,
            None => None,
        };
        let (spec, _) = bus(&sc_state);
        let src = SoundcardInputSource::open(
            &SoundcardInputConfig { device },
            spec.rate,
            spec.channels.count(),
        )
        .map_err(mlua::Error::runtime)?;
        Ok(LuaSource::new(Box::new(src)))
    })?;
    // ---- relay/pull-stream source (Liquidsoap `input.http`) --------------
    // A network thread GETs the URL and decodes the live body into a ring;
    // while disconnected the source exhausts, so a
    // `fallback({input.http(...), local})` covers the gap automatically.
    let http_state = state.clone();
    let http_fn = lua.create_function(move |_, (url, opts): (String, Option<Table>)| {
        let (spec, fpb) = bus(&http_state);
        let backoff_ms: u64 = match &opts {
            Some(t) => t.get("reconnect_backoff").unwrap_or(500),
            None => 500,
        };
        let timeout_secs = http_state.borrow().request.timeout_secs;
        crate::source::http::HttpSource::spawn(
            &url,
            spec,
            fpb,
            Duration::from_secs(timeout_secs),
            Duration::from_millis(backoff_ms),
        )
        .map_err(mlua::Error::runtime)
        .map(|src| LuaSource::new(Box::new(src)))
    })?;
    let input = lua.create_table()?;
    input.set("harbor", harbor_fn)?;
    input.set("soundcard", soundcard_fn)?;
    input.set("http", http_fn)?;
    globals.set("input", input)?;

    // ---- video (Part H) ----------------------------------------------------
    // `video.video(path)` registers a video track: the path is validated at
    // script evaluation (fail fast), the audio side plays through the normal
    // audio graph (`single`/playlist), and the engine spawns a dedicated
    // decode thread that publishes PTS-paced frames to the shared video tap.
    // `video.playlist(opts)`/`video.single(path)` (Part H7) register a
    // sequence played one file at a time on one decode thread with a
    // continuous PTS timeline. The return values are opaque markers for
    // video outputs.
    #[cfg(feature = "video")]
    {
        let video_state = state.clone();
        let video_fn = lua.create_function(move |lua, path: String| {
            let spec = crate::video::VideoSource::validate(std::path::Path::new(&path))
                .map_err(mlua::Error::runtime)?;
            let mut s = video_state.borrow_mut();
            s.video_tap
                .get_or_insert_with(|| Arc::new(crate::video::VideoTap::new()));
            s.video.push(crate::video::VideoConfig {
                path: path.clone().into(),
                spec,
            });
            let info = lua.create_table()?;
            info.set("path", path)?;
            info.set("width", spec.width)?;
            info.set("height", spec.height)?;
            Ok(info)
        })?;

        let pl_state = state.clone();
        let playlist_fn = lua.create_function(move |lua, opts: Table| {
            let tracks = video_playlist_configs(&opts)?;
            let shuffle: bool = opts.get("shuffle").unwrap_or(false);
            // Option-bool: mlua converts a missing key to Ok(false), so a
            // plain unwrap_or(true) would never fire.
            let loop_playlist: bool = opts.get("loop").ok().flatten().unwrap_or(true);
            let spec = tracks.first().expect("non-empty").spec;
            let count = tracks.len();
            let mut s = pl_state.borrow_mut();
            s.video_tap
                .get_or_insert_with(|| Arc::new(crate::video::VideoTap::new()));
            s.video_playlists.push(crate::video::VideoPlaylistConfig {
                tracks,
                shuffle,
                loop_playlist,
                seed: None,
            });
            let info = lua.create_table()?;
            info.set("count", count)?;
            info.set("width", spec.width)?;
            info.set("height", spec.height)?;
            Ok(info)
        })?;

        let single_state = state.clone();
        let single_fn = lua.create_function(move |lua, path: String| {
            let spec = crate::video::VideoSource::validate(std::path::Path::new(&path))
                .map_err(mlua::Error::runtime)?;
            let mut s = single_state.borrow_mut();
            s.video_tap
                .get_or_insert_with(|| Arc::new(crate::video::VideoTap::new()));
            s.video_playlists.push(crate::video::VideoPlaylistConfig {
                tracks: vec![crate::video::VideoConfig {
                    path: path.clone().into(),
                    spec,
                }],
                shuffle: false,
                loop_playlist: false,
                seed: None,
            });
            let info = lua.create_table()?;
            info.set("path", path)?;
            info.set("width", spec.width)?;
            info.set("height", spec.height)?;
            Ok(info)
        })?;
        let video = lua.create_table()?;
        video.set("video", video_fn)?;
        video.set("playlist", playlist_fn)?;
        video.set("single", single_fn)?;
        globals.set("video", video)?;
    }

    let telnet_state = state.clone();
    let telnet_fn = lua.create_function(move |_, opts: Table| {
        let cfg = ControlConfig {
            host: opts.get("host").unwrap_or_else(|_| "127.0.0.1".into()),
            port: opts.get("port").unwrap_or(1234),
            banner: opts.get("banner").unwrap_or(true),
            http_port: opts.get("http_port").ok().flatten(),
        };
        telnet_state.borrow_mut().control = Some(cfg);
        Ok(())
    })?;
    let reg_state = state.clone();
    let register_fn =
        lua.create_function(move |_, (name, callback): (String, mlua::Function)| {
            let trimmed = name.trim();
            if trimmed.is_empty() || trimmed.split_whitespace().count() > 1 {
                return Err(mlua::Error::runtime(
                    "server.register: name must be a single non-empty word",
                ));
            }
            reg_state
                .borrow_mut()
                .custom_commands
                .push((trimmed.to_string(), callback));
            Ok(())
        })?;
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

    /// Parse the source protocol string (`"icecast"` is the default;
    /// `"shoutcast"` is an alias for SHOUTcast v2).
    fn parse_protocol(value: &str) -> mlua::Result<OutputProtocol> {
        match value {
            "icecast" => Ok(OutputProtocol::Icecast),
            "shoutcast" | "shoutcast-v2" => Ok(OutputProtocol::ShoutcastV2),
            "shoutcast-v1" => Ok(OutputProtocol::ShoutcastV1),
            other => Err(mlua::Error::runtime(format!(
                "unknown protocol {other:?} (use \"icecast\", \"shoutcast\" or \"shoutcast-v1\")"
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
            protocol: opts
                .get::<Option<String>>("protocol")?
                .map(|p| parse_protocol(&p))
                .transpose()?
                .unwrap_or_default(),
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
            let video: Option<Table> = opts.get("video")?;
            let has_video = video.is_some();
            if has_video {
                // `video` must be a marker returned by `video.video(path)`,
                // `video.playlist(...)` or `video.single(path)`; that call
                // also created the shared tap the output subscribes to at
                // startup.
                let s = hls_state.borrow();
                #[cfg(feature = "video")]
                if s.video_tap.is_none() || (s.video.is_empty() && s.video_playlists.is_empty()) {
                    return Err(mlua::Error::runtime(
                        "output.hls({video = ...}) requires a video.video/video.playlist \
                         source registered first",
                    ));
                }
                #[cfg(not(feature = "video"))]
                {
                    let _ = s;
                    return Err(mlua::Error::runtime(
                        "output.hls({video = ...}) needs a video build (--features video)",
                    ));
                }
            }
            let cfg = HlsOutputConfig {
                directory: directory.into(),
                segment_seconds: opts.get("segment_seconds").unwrap_or(5.0),
                retention: opts.get("retention").unwrap_or(12),
                video: has_video,
            };
            let mut s = hls_state.borrow_mut();
            claim_root(&mut s, &mut source)?;
            s.hls_outputs.push(cfg);
            Ok(())
        })?,
    )?;
    let sc_out_state = state.clone();
    output.set(
        "soundcard",
        lua.create_function(move |_, (opts, mut source): (Table, LuaSource)| {
            let device: Option<String> = opts.get("device")?;
            let mut s = sc_out_state.borrow_mut();
            claim_root(&mut s, &mut source)?;
            s.soundcard_outputs.push(SoundcardOutputConfig { device });
            Ok(())
        })?,
    )?;
    globals.set("output", output)?;

    // ---- metadata hooks (Liquidsoap `on_metadata`, `on_track`) ---------------
    let meta_state = state.clone();
    let meta_tx = event_tx.clone();
    globals.set(
        "on_metadata",
        lua.create_function(
            move |_, (mut source, callback): (LuaSource, mlua::Function)| {
                let hook_id = meta_state.borrow().metadata_hooks.len();
                let child = source.take();
                meta_state.borrow_mut().metadata_hooks.push(callback);
                let wrapped = OnMetadataSource::new(child, meta_tx.clone(), hook_id);
                Ok(LuaSource::new(Box::new(wrapped)))
            },
        )?,
    )?;

    let track_state = state.clone();
    let track_tx = event_tx.clone();
    globals.set(
        "on_track",
        lua.create_function(
            move |_, (mut source, callback): (LuaSource, mlua::Function)| {
                let hook_id = track_state.borrow().track_hooks.len();
                let child = source.take();
                track_state.borrow_mut().track_hooks.push(callback);
                let wrapped = OnTrackSource::new(child, track_tx.clone(), hook_id);
                Ok(LuaSource::new(Box::new(wrapped)))
            },
        )?,
    )?;

    // ---- metadata rewrite (Liquidsoap `map_metadata`) ---------------------
    // Unlike `on_metadata`, the callback's return value *reaches the output*:
    // the wrapped source asks for a rewrite when the child's label changes
    // and reports the rewritten title (or the original on timeout/error).
    let map_state = state.clone();
    let map_tx = event_tx.clone();
    globals.set(
        "map_metadata",
        lua.create_function(
            move |_, (mut source, callback): (LuaSource, mlua::Function)| {
                let hook_id = map_state.borrow().map_metadata_hooks.len();
                let child = source.take();
                map_state.borrow_mut().map_metadata_hooks.push(callback);
                let wrapped = MapMetadataSource::new(child, map_tx.clone(), hook_id);
                Ok(LuaSource::new(Box::new(wrapped)))
            },
        )?,
    )?;

    // ---- evaluate ---------------------------------------------------------
    lua.load(src).set_name("crabsoup.lua").exec()?;

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
        soundcard_outputs: std::mem::take(&mut s.soundcard_outputs),
        request_queue: s.request_queue.take(),
        custom_commands: s.custom_commands.iter().map(|(n, _)| n.clone()).collect(),
        root: s.root.take(),
        preview: s.preview.take(),
        #[cfg(feature = "video")]
        video: std::mem::take(&mut s.video),
        #[cfg(feature = "video")]
        video_playlists: std::mem::take(&mut s.video_playlists),
        #[cfg(feature = "video")]
        video_tap: s.video_tap.take(),
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
        dynamic_hooks: std::mem::take(&mut s.dynamic_hooks),
        blank_hooks: std::mem::take(&mut s.blank_hooks),
        map_metadata_hooks: std::mem::take(&mut s.map_metadata_hooks),
    };
    Ok((runtime, result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_sets_settings_via_lua() {
        let (_rt, res) = run(r#"
            set("sample_rate", 48000)
            set("channels", 1)
            set("crossfade_seconds", 4.5)
            input.harbor({port = 8006})
            output.preview(input.harbor({port = 8007}))
            "#)
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

    #[cfg(feature = "video")]
    #[test]
    fn video_video_registers_a_track_and_tap() {
        use crate::video::testutil::render_test_clip;
        let Some(path) = render_test_clip("script") else {
            return;
        };
        let path_str = path.display().to_string();
        let (_rt, res) = run(&format!(
            r#"
            set("sample_rate", 44100)
            output.preview(sine({{freq = 440, duration = 1}}))
            local v = video.video("{path_str}")
            assert(v.width == 320, "marker width")
            assert(v.height == 240, "marker height")
            "#
        ))
        .expect("script runs");
        assert_eq!(res.video.len(), 1, "one video track registered");
        assert!(res.video_tap.is_some(), "shared video tap created");
        std::fs::remove_file(&path).ok();
    }

    #[cfg(feature = "video")]
    #[test]
    fn video_playlist_and_single_register_sequences() {
        use crate::video::testutil::render_test_clip;
        let Some(a) = render_test_clip("pl-script-a") else {
            return;
        };
        let Some(b) = render_test_clip("pl-script-b") else {
            return;
        };
        let (a_str, b_str) = (a.display().to_string(), b.display().to_string());
        let (_rt, res) = run(&format!(
            r#"
            set("sample_rate", 44100)
            output.preview(sine({{freq = 440, duration = 1}}))
            local p = video.playlist({{files = {{"{a_str}", "{b_str}"}}}})
            assert(p.count == 2, "playlist marker count")
            assert(p.width == 320 and p.height == 240, "playlist marker spec")
            local s = video.single("{a_str}")
            assert(s.width == 320, "single marker spec")
            "#
        ))
        .expect("script runs");
        assert_eq!(res.video_playlists.len(), 2, "two sequences registered");
        assert_eq!(res.video_playlists[0].tracks.len(), 2);
        assert!(
            res.video_playlists[0].loop_playlist,
            "loop defaults to true"
        );
        assert!(!res.video_playlists[1].loop_playlist, "single never loops");
        assert_eq!(res.video_playlists[1].tracks.len(), 1);
        assert!(res.video_tap.is_some(), "shared video tap created");
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }

    #[cfg(feature = "video")]
    #[test]
    fn video_playlist_rejects_mixed_resolutions_fast() {
        use crate::video::testutil::render_test_clip_size;
        let Some(a) = render_test_clip_size("pl-mix-a", 320, 240) else {
            return;
        };
        let Some(b) = render_test_clip_size("pl-mix-b", 640, 480) else {
            return;
        };
        let (a_str, b_str) = (a.display().to_string(), b.display().to_string());
        let err = run(&format!(
            r#"
            set("sample_rate", 44100)
            output.preview(sine({{freq = 440, duration = 1}}))
            video.playlist({{files = {{"{a_str}", "{b_str}"}}}})
            "#
        ))
        .err()
        .expect("mixed resolutions must fail at evaluation");
        assert!(
            err.to_string().contains("must share one resolution"),
            "{err}"
        );
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }

    #[cfg(feature = "video")]
    #[test]
    fn video_playlist_empty_directory_fails() {
        let dir = std::env::temp_dir().join(format!("crabsoup-vpl-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let err = run(&format!(
            r#"
            set("sample_rate", 44100)
            output.preview(sine({{freq = 440, duration = 1}}))
            video.playlist({{directory = "{}"}})
            "#,
            dir.display()
        ))
        .err()
        .expect("empty playlist must fail at evaluation");
        assert!(err.to_string().contains("no video files found"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn server_register_runs_the_lua_handler_and_replies() {
        let (rt, res) = run(r#"
            server.register("ping", function(args) return "pong [" .. args .. "]" end)
            output.preview(sine({freq = 440, duration = 1}))
            "#)
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
        let (rt, _res) = run(r#"
            server.register("boom", function() error("kaput") end)
            output.preview(sine({freq = 440, duration = 1}))
            "#)
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
            assert!(err.to_string().contains(detail), "{name:?}: {err}");
        }
    }

    #[test]
    fn compose_sources_without_files() {
        let (_rt, res) = run(r#"
            live = input.harbor({})
            backup = input.harbor({})
            output.preview(fallback({live, backup}))
            "#)
        .expect("script runs");
        assert!(res.preview.is_some());
    }

    #[test]
    fn input_http_relays_when_up_and_falls_back_when_down() {
        use std::io::Write;
        use std::net::TcpListener;

        // A bound-but-silent listener first (relay down), then a server that
        // serves a 660 Hz WAV per connection (relay up). The script wraps the
        // relay in a fallback with a local 440 Hz sine.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let url = format!("http://{addr}/feed.wav");
        let script = format!(
            "set(\"request_timeout\", 1)\n\
             output.preview(fallback({{input.http(\"{url}\", {{reconnect_backoff = 20}}),\n\
                                      sine({{freq = 440, duration = 5, amplitude = 0.5}})}}))\n"
        );
        let (_rt, res) = run(&script).expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];

        // Sign changes per second across a window: 2*f for a sine.
        let crossings_per_sec = |slice: &[f32]| {
            let n = slice
                .windows(2)
                .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
                .count() as f64;
            n / (slice.len() as f64 / 44_100.0 / 2.0)
        };

        // Phase 1 — relay down: the 440 Hz sine covers the gap.
        let mut got = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while got.len() < 44_100 && std::time::Instant::now() < deadline {
            let n = root.next_buffer(&mut buf);
            got.extend_from_slice(&buf[..n]);
            std::thread::sleep(Duration::from_millis(2));
        }
        let cps = crossings_per_sec(&got);
        assert!(
            (cps - 880.0).abs() / 880.0 < 0.25,
            "expected 440 Hz fallback, got {cps} cps"
        );

        // Phase 2 — relay up: the relay preempts the fallback.
        let wav = sine_wav_bytes(660.0, 0.3, 44_100);
        let wav_len = wav.len();
        std::thread::spawn(move || {
            for _ in 0..64 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {wav_len}\r\n\r\n"
                );
                if stream.write_all(head.as_bytes()).is_err() {
                    continue;
                }
                if stream.write_all(&wav).is_err() {
                    continue;
                }
                let _ = stream.flush();
            }
        });
        let mut saw_relay = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            let n = root.next_buffer(&mut buf);
            got.extend_from_slice(&buf[..n]);
            let window = &got[got.len().saturating_sub(44_100)..];
            let cps = crossings_per_sec(window);
            if cps > 1000.0 {
                saw_relay = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            saw_relay,
            "relay must preempt the fallback once it connects"
        );
    }

    /// A minimal RIFF/WAVE sine (PCM 16-bit stereo) for the relay feed.
    fn sine_wav_bytes(freq: f64, seconds: f64, rate: u32) -> Vec<u8> {
        let n = (seconds * rate as f64) as usize;
        let mut data = Vec::with_capacity(n * 4);
        for i in 0..n {
            let t = i as f64 / rate as f64;
            let s = (2.0 * std::f64::consts::PI * freq * t).sin() as f32 * 0.5;
            let sample = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            for _ in 0..2 {
                data.extend_from_slice(&sample.to_le_bytes());
            }
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 4).to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    #[test]
    fn output_soundcard_registers_without_opening_a_device() {
        // Unlike `input.soundcard`, the device is opened only at connect()
        // time in main, so registration is deterministic anywhere.
        let (_rt, res) = run(r#"
            output.soundcard({}, sine({freq = 440, duration = 1}))
            "#)
        .expect("script runs");
        assert_eq!(res.soundcard_outputs.len(), 1);
        assert!(res.root.is_some(), "output.soundcard claims the root");
    }

    #[test]
    fn input_soundcard_opens_or_fails_gracefully_without_a_device() {
        // The capture device opens at script evaluation, so this is
        // environment-dependent by design: with a device present a real
        // stream is created (and closed on drop); without one the error is
        // clear and actionable rather than a panic.
        match run("output.preview(input.soundcard({}))") {
            Ok(_) => {}
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("no default input device")
                        || msg.contains("no default input config")
                        || msg.contains("cannot open device")
                        || msg.contains("cannot start stream"),
                    "{msg}"
                );
            }
        }
    }

    #[test]
    fn sine_with_duration_drives_exactly_one_second_of_frames() {
        let (_rt, res) = run(r#"
            output.preview(amplify(sine({freq = 220, duration = 1}), 0.5))
            "#)
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
        let (_rt, res) = run(r#"
            output.preview(fallback({blank({duration = 0.1}), sine({duration = 1, freq = 100})}))
            "#)
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
    fn mksafe_never_exhausts_and_plays_silence_after_the_child_ends() {
        let (_rt, res) = run(r#"
            output.preview(mksafe(sine({freq = 440, duration = 0.05})))
            "#)
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        // The sine plays first...
        let n = root.next_buffer(&mut buf);
        assert!(n > 0);
        assert!(buf[..n].iter().any(|&s| s.abs() > 0.01));
        // ...then silence forever: the combined source never exhausts.
        for _ in 0..10 {
            let n = root.next_buffer(&mut buf);
            assert_eq!(n, buf.len(), "mksafe must keep producing buffers");
            assert!(
                buf.iter().all(|&s| s == 0.0),
                "mksafe fallback must be silence"
            );
            assert!(!root.is_exhausted(), "mksafe must never exhaust");
        }
    }

    #[test]
    fn pipe_passthrough_plays_and_composes_with_mksafe() {
        let (_rt, res) = run(r#"
            output.preview(mksafe(pipe({process = "cat", format = "s16le"},
                                       sine({freq = 440, duration = 0.2}))))
            "#)
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        let mut non_silent = 0;
        for _ in 0..200 {
            let n = root.next_buffer(&mut buf);
            non_silent += buf[..n].iter().filter(|&&s| s.abs() > 0.01).count();
            if root.is_exhausted() {
                break;
            }
        }
        // 0.2 s at 44100 Hz through the pipe (a couple of 4096-frame chunks),
        // then the mksafe blank covers the exhausted pipe with silence.
        assert!(
            non_silent >= 8000,
            "pipe produced only {non_silent} samples"
        );
        assert!(
            !root.is_exhausted(),
            "mksafe must keep the pipe source alive"
        );
    }

    #[test]
    fn pipe_requires_process_and_valid_format() {
        let err = match run("output.preview(pipe({format = \"s16le\"}, sine({freq = 440})))") {
            Ok(_) => panic!("pipe without `process` must fail"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("process"));

        let err = match run(
            "output.preview(pipe({process = \"cat\", format = \"s32le\"}, sine({freq = 440})))",
        ) {
            Ok(_) => panic!("pipe with a bad format must fail"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("s16le"));
    }

    #[test]
    fn mksafe_covers_a_failed_request_resolution_with_silence() {
        let (_rt, res) = run(r#"
            q = request.queue()
            output.preview(mksafe(q))
            "#)
        .expect("script runs");
        let queue = res.request_queue.as_ref().expect("queue registered");
        queue.push(RequestUri::new("/definitely/not/here.mp3"));
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        let n = root.next_buffer(&mut buf);
        assert_eq!(
            n,
            buf.len(),
            "a failed resolve must yield silence, not an engine error"
        );
        assert!(buf[..n].iter().all(|&s| s == 0.0));
        assert!(!root.is_exhausted(), "mksafe must never exhaust");
    }

    #[test]
    fn add_sums_sources_sample_wise() {
        let (_rt, res) = run(r#"
            output.preview(add({sine({freq = 440, amplitude = 0.5}),
                                 sine({freq = 440, amplitude = 0.5})}))
            "#)
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        root.next_buffer(&mut buf);
        // Two in-phase 0.5-amplitude sines sum to a ~1.0 peak (a doubling or
        // quadrupling bug would move it to 2.0/4.0 and must fail here).
        let peak = buf.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(
            (peak - 1.0).abs() < 0.01,
            "add did not sum the sines (peak {peak})"
        );
    }

    #[test]
    fn add_applies_per_source_weights() {
        let (_rt, res) = run(r#"
            output.preview(add({sine({freq = 440, amplitude = 0.5}),
                                 sine({freq = 440, amplitude = 0.5})},
                                {weights = {0.5, 1.0}}))
            "#)
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        root.next_buffer(&mut buf);
        // 0.5 * 0.5 + 1.0 * 0.5 -> ~0.75 peak.
        let peak = buf.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!((peak - 0.75).abs() < 0.01, "weighted sum peak {peak}");
    }

    #[test]
    fn add_keeps_playing_after_a_short_child_ends() {
        // A short child over an infinite one: when the short child exhausts,
        // the sum continues with the remaining child and never exhausts.
        let (_rt, res) = run(r#"
            output.preview(add({sine({freq = 440, duration = 0.05, amplitude = 0.5}),
                                 sine({freq = 220, amplitude = 0.5})}))
            "#)
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        // Drain the short child's 4410 samples (0.05 s stereo at 44.1 kHz),
        // then pull again and require the bed to still be audible.
        let mut drained = 0usize;
        while drained < 4410 {
            let n = root.next_buffer(&mut buf);
            assert!(n > 0, "add stalled while the short child was live");
            drained += n;
        }
        let n = root.next_buffer(&mut buf);
        assert!(n > 0, "add must keep producing the infinite child");
        assert!(
            buf[..n].iter().any(|&s| s.abs() > 0.01),
            "bed must be audible after the short child ends"
        );
        assert!(
            !root.is_exhausted(),
            "add must not exhaust while one child lives"
        );
    }

    #[test]
    fn add_exhausts_when_every_child_ends() {
        let (_rt, res) = run(r#"
            output.preview(add({sine({freq = 440, duration = 0.05, amplitude = 0.5}),
                                 sine({freq = 220, duration = 0.05, amplitude = 0.5})}))
            "#)
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        let mut total = 0usize;
        while !root.is_exhausted() {
            let n = root.next_buffer(&mut buf);
            if n == 0 {
                break;
            }
            total += n;
        }
        assert!(
            root.is_exhausted(),
            "add must exhaust once all children end"
        );
        // 0.05 s stereo at 44.1 kHz = 4410 samples, no matter the splits.
        assert_eq!(total, 4410);
    }

    #[test]
    fn add_rejects_bad_weight_counts() {
        let err = run(r#"
            output.preview(add({sine({freq = 440}), sine({freq = 220})},
                                {weights = {0.5}}))
            "#)
        .err()
        .expect("script fails");
        assert!(err.to_string().contains("weights"), "{}", err.to_string());
    }

    #[test]
    fn add_rejects_an_empty_source_list() {
        let err = run("output.preview(add({}))").err().expect("script fails");
        assert!(
            err.to_string().contains("list of sources"),
            "{}",
            err.to_string()
        );
    }

    #[test]
    fn cue_cut_skips_and_truncates_a_sine() {
        let (_rt, res) = run(r#"
            output.preview(cue_cut(sine({freq = 100, duration = 1, amplitude = 1.0}),
                                    {cue_in = 0.05, cue_out = 0.15}))
            "#)
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        let mut total = 0usize;
        while !root.is_exhausted() {
            let n = root.next_buffer(&mut buf);
            if n == 0 {
                break;
            }
            total += n;
        }
        // Window [0.05, 0.15) = 0.1 s stereo at 44.1 kHz = 8820 samples.
        assert_eq!(total, 8820, "cue_cut window mismatch ({total})");
    }

    #[test]
    fn annotate_uri_plays_through_single_with_cue_points() {
        // A real file with an annotate: cue window; skipped when media/ is
        // absent, like the other real-file tests.
        let real = PathBuf::from("media/sunset-house-grooves-deep-house-sunset-538759.mp3");
        if !real.exists() {
            return;
        }
        let script = format!(
            "output.preview(single(\"annotate:liq_cue_in=\\\"1\\\",liq_cue_out=\\\"2\\\":{}\"))",
            real.display()
        );
        let (_rt, res) = run(&script).expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        let mut total = 0usize;
        let mut non_silent = 0usize;
        while !root.is_exhausted() {
            let n = root.next_buffer(&mut buf);
            if n == 0 {
                break;
            }
            total += n;
            if buf[..n].iter().any(|&s| s.abs() > 0.01) {
                non_silent += n;
            }
        }
        // Window [1, 2) = 1 s stereo at 44.1 kHz = 88200 samples; the track
        // must end there, not at its natural length.
        assert_eq!(total, 88200, "annotate window not respected ({total})");
        assert!(non_silent > 0, "no audio through the annotated single");
    }

    #[test]
    fn cue_cut_exposes_fade_overrides_to_the_mixer() {
        // The `fade_in`/`fade_out` options are the D2 step-2 per-track
        // crossfade override: the mixer reads them via `crossfade_overrides`
        // instead of the global `crossfade_seconds`.
        let (_rt, res) = run(r#"
            output.preview(cue_cut(sine({freq = 100, duration = 1, amplitude = 1.0}),
                                    {cue_in = 0.05, cue_out = 0.15,
                                     fade_in = 2, fade_out = 3}))
            "#)
        .expect("script runs");
        let root = res.preview.expect("preview source");
        assert_eq!(
            root.crossfade_overrides(),
            Some((Some(2.0), Some(3.0))),
            "cue_cut must report fade overrides to the crossfade mixer"
        );
        // Without fades, no overrides are reported (global window applies).
        let (_rt, res) = run(r#"
            output.preview(cue_cut(sine({freq = 100, duration = 1}),
                                    {cue_in = 0.05, cue_out = 0.15}))
            "#)
        .expect("script runs");
        let root = res.preview.expect("preview source");
        assert_eq!(root.crossfade_overrides(), None);
    }

    #[test]
    fn annotate_fade_keys_reach_the_resolved_source() {
        // An `annotate:` prefix with only fade keys (no cue points) must
        // still wrap the source so the mixer sees the overrides.
        let real = PathBuf::from("media/sunset-house-grooves-deep-house-sunset-538759.mp3");
        if !real.exists() {
            return;
        }
        let script = format!(
            "output.preview(single(\"annotate:liq_fade_in=\\\"2\\\",liq_fade_out=\\\"3\\\":{}\"))",
            real.display()
        );
        let (_rt, res) = run(&script).expect("script runs");
        let root = res.preview.expect("preview source");
        assert_eq!(
            root.crossfade_overrides(),
            Some((Some(2.0), Some(3.0))),
            "annotate fades must reach the mixer"
        );
    }

    #[test]
    fn smart_crossfade_plays_a_real_directory_with_level_aware_fades() {
        // Real media dir; skipped when absent, like the other real-file
        // tests. The operator builds a level-aware crossfading playlist
        // that must produce audio (non-silent) and exhaust with the files.
        // `loop = false` keeps the drain bounded (playlists default to
        // looping).
        let media = PathBuf::from("media");
        if !media.exists() {
            return;
        }
        let (_rt, res) = run(r#"
            output.preview(smart_crossfade({directory = "./media",
                                            fade_out = 1.0, fade_mid = 0.2,
                                            loop = false}))
            "#)
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        let mut total = 0usize;
        let mut non_silent = 0usize;
        while !root.is_exhausted() {
            let n = root.next_buffer(&mut buf);
            if n == 0 {
                break;
            }
            total += n;
            if buf[..n].iter().any(|&s| s.abs() > 0.01) {
                non_silent += n;
            }
        }
        assert!(total > 0, "smart_crossfade produced no audio ({total})");
        assert!(non_silent > 0, "smart_crossfade output was silent");
    }

    #[test]
    fn request_dynamic_invokes_the_lua_callback_until_nil() {
        // Two unresolvable requests are skipped, then nil ends the source.
        // The audio thread polls the reply (never blocks), so the test
        // drives the Lua event loop with drain_metadata on silent pulls.
        let (rt, res) = run(r#"
            n = 0
            d = request.dynamic(function()
                n = n + 1
                if n <= 2 then return "/definitely/not/here.mp3" else return nil end
            end)
            output.preview(d)
            "#)
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        let mut pulls = 0;
        while pulls < 500 && !root.is_exhausted() {
            let n = root.next_buffer(&mut buf);
            pulls += 1;
            if n == 0 {
                rt.drain_metadata();
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        assert!(
            root.is_exhausted(),
            "source must end when the callback returns nil"
        );
        assert_eq!(
            rt.global::<i64>("n").expect("lua n"),
            3,
            "callback must run once per request, ending on the nil"
        );
    }

    #[test]
    fn request_dynamic_plays_resolved_requests_in_order() {
        // A real (generated) Opus file plays, then the callback's nil ends
        // the source after the file finishes.
        let dest = std::env::temp_dir().join("crabsoup-test-dynamic.opus");
        write_test_opus(&dest, 0.3);
        let script = format!(
            r#"
            n = 0
            d = request.dynamic(function()
                n = n + 1
                if n == 1 then return "{path}" else return nil end
            end)
            output.preview(d)
            "#,
            path = dest.display()
        );
        let (rt, res) = run(&script).expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        let mut total = 0usize;
        let mut silent_pulls = 0;
        while !root.is_exhausted() && silent_pulls < 50 {
            let n = root.next_buffer(&mut buf);
            if n == 0 {
                silent_pulls += 1;
                rt.drain_metadata();
                std::thread::sleep(Duration::from_millis(1));
            } else {
                silent_pulls = 0;
            }
            total += n;
        }
        let _ = std::fs::remove_file(&dest);
        // 0.3 s stereo at 44.1 kHz = 26460 samples; the whole window plays.
        assert!(
            total >= 26_000,
            "generated track not played (total {total})"
        );
        assert!(root.is_exhausted(), "source must end after the nil");
        // Exactly two callback invocations: the file request and the nil.
        assert_eq!(
            rt.global::<i64>("n").expect("lua n"),
            2,
            "callback must run for the file and then the nil"
        );
    }

    /// Encode a short sine into an Opus file at `path` (test helper).
    fn write_test_opus(path: &std::path::Path, seconds: f64) {
        use crate::output::encoder::{Encoder, OpusEncoder};
        let mut enc = OpusEncoder::new(44_100, 2, 128_000, "test").expect("encoder");
        let frames = (seconds * 44_100.0) as usize;
        let mut out = Vec::new();
        let mut pcm = Vec::with_capacity(1024);
        for f in 0..frames {
            let v = (f as f64 * 2.0 * std::f64::consts::PI * 440.0 / 44_100.0).sin() as f32 * 0.5;
            pcm.extend_from_slice(&[v, v]);
            if pcm.len() >= 1024 {
                out.extend_from_slice(&enc.encode(&pcm));
                pcm.clear();
            }
        }
        if !pcm.is_empty() {
            out.extend_from_slice(&enc.encode(&pcm));
        }
        out.extend_from_slice(&enc.finish());
        std::fs::write(path, out).expect("write opus");
    }

    #[test]
    fn request_queue_registers_and_is_shared_with_the_control_port() {
        let (_rt, res) = run(r#"
            q = request.queue()
            output.preview(fallback({q, sine({freq = 440, duration = 1})}))
            "#)
        .expect("script runs");
        let queue = res.request_queue.expect("queue registered");
        assert!(queue.is_empty());
        queue.push(RequestUri::new("/tmp/x.mp3"));
        assert_eq!(queue.list(), vec![RequestUri::new("/tmp/x.mp3")]);
    }

    #[test]
    fn request_queue_preempts_a_playing_playlist_when_pushed() {
        let real = PathBuf::from("media/sunset-house-grooves-deep-house-sunset-538759.mp3");
        if !real.exists() {
            return;
        }
        let (_rt, res) = run(r#"
            q = request.queue()
            output.preview(fallback({q, sine({freq = 440, duration = 2})}))
            "#)
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
        queue.push(RequestUri::Local(real.clone(), None));
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
        let (_rt, res) = run(r#"
            output.preview(compress(sine({freq = 440, duration = 1, amplitude = 1.0}),
                                    {threshold = -12, ratio = 2, attack = 0, release = 0}))
            "#)
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
        let (_rt, res) = run(r#"
            output.preview(replaygain(sine({freq = 440, duration = 1, amplitude = 0.5}),
                                       {max_boost = 6, max_cut = 6}))
            "#)
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
        let (_rt, res) = run(r#"
            titles = {}
            src = on_metadata(sequence({sine({freq = 440, duration = 0.1}),
                                       sine({freq = 880, duration = 0.1})}),
                              function(m) titles[#titles + 1] = m.title end)
            output.preview(src)
            "#)
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
        let (_rt, res) = run(r#"
            tracks = 0
            src = on_track(sequence({sine({freq = 440, duration = 0.1}),
                                     sine({freq = 880, duration = 0.1})}),
                           function() tracks = tracks + 1 end)
            output.preview(src)
            "#)
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
        let (_rt, res) = run(r#"
            tracks = {}
            src = on_track(blank({duration = 0.1}),
                           function() tracks[#tracks + 1] = true end)
            output.preview(src)
            "#)
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        root.next_buffer(&mut buf);
        drop(root);
        _rt.drain_metadata();
        let tracks: mlua::Table = _rt.global("tracks").expect("tracks table");
        assert_eq!(tracks.raw_len(), 1, "blank labels itself, so one boundary");
    }

    // ---- Part F4: blank.detect -----------------------------------------

    #[test]
    fn blank_detect_hands_off_to_fallback_and_fires_on_blank() {
        // A source that goes silent partway (tone then forever-silence via
        // `add`) must trip the detector: the Lua `on_blank` fires once, and
        // the `fallback` composed around it switches to the backup child.
        let (_rt, res) = run(r#"
            blanked = 0
            src = blank.detect(add({sine({freq = 440, duration = 0.3}), blank()}),
                               {threshold = -60, duration = 0.1, restart = 0.2,
                                on_blank = function() blanked = blanked + 1 end})
            output.preview(fallback({src, sine({freq = 880})}))
            "#)
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        // 0.3 s tone (~3.2 buffers at 4096 frames/44.1 kHz), then ~0.1 s of
        // silence before detection: 8 buffers drives well past the handover.
        for _ in 0..8 {
            root.next_buffer(&mut buf);
        }
        assert_eq!(
            root.label().as_deref(),
            Some("sine 880 Hz"),
            "fallback must have taken over from the blank source"
        );
        drop(root);
        _rt.drain_metadata();
        let blanked: u64 = _rt.global("blanked").expect("blanked counter");
        assert_eq!(blanked, 1, "on_blank fired once per episode");
    }

    // ---- Part F3: map_metadata -----------------------------------------

    #[test]
    fn map_metadata_rewrites_titles_in_order() {
        let (_rt, res) = run(r#"
            src = map_metadata(sequence({sine({freq = 440, duration = 0.2}),
                                         sine({freq = 880, duration = 0.2})}),
                               function(m) return {title = "Rewritten: " .. m.title} end)
            output.preview(src)
            "#)
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        // The rewrite is requested on the pull that observes the boundary;
        // the Lua reply lands on the next pull. (A `sequence` label jumps to
        // the next child the moment the current one exhausts, so the first
        // track is long enough that the rewrite lands before that happens.)
        root.next_buffer(&mut buf);
        _rt.drain_metadata();
        root.next_buffer(&mut buf);
        assert_eq!(root.label().as_deref(), Some("Rewritten: sine 440 Hz"));
        // Drive through the 0.2 s first track into the second.
        for _ in 0..3 {
            root.next_buffer(&mut buf);
        }
        _rt.drain_metadata();
        root.next_buffer(&mut buf);
        assert_eq!(root.label().as_deref(), Some("Rewritten: sine 880 Hz"));
    }

    #[test]
    fn map_metadata_nil_keeps_the_original_title() {
        let (_rt, res) = run(r#"
            src = map_metadata(sine({freq = 440}), function(m) return nil end)
            output.preview(src)
            "#)
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        root.next_buffer(&mut buf);
        _rt.drain_metadata();
        root.next_buffer(&mut buf);
        assert_eq!(root.label().as_deref(), Some("sine 440 Hz"));
    }

    #[test]
    fn map_metadata_callback_error_keeps_the_original_title() {
        let (_rt, res) = run(r#"
            src = map_metadata(sine({freq = 440}), function() error("boom") end)
            output.preview(src)
            "#)
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        root.next_buffer(&mut buf);
        _rt.drain_metadata();
        root.next_buffer(&mut buf);
        assert_eq!(root.label().as_deref(), Some("sine 440 Hz"));
    }

    #[test]
    fn map_metadata_falls_back_to_raw_when_lua_never_replies() {
        // The event loop is never drained: the rewrite cannot land, so the
        // bounded pull budget must give up and report the raw label instead
        // of stalling the audio thread.
        let (_rt, res) = run(r#"
            src = map_metadata(sine({freq = 440}), function(m) return {title = "never"} end)
            output.preview(src)
            "#)
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        for _ in 0..10 {
            assert!(root.next_buffer(&mut buf) > 0, "audio keeps flowing");
        }
        assert_eq!(
            root.label().as_deref(),
            Some("sine 440 Hz"),
            "raw label after the bounded wait expires"
        );
    }

    #[test]
    fn blank_detect_leaves_healthy_audio_alone() {
        let (_rt, res) = run(r#"
            blanked = 0
            output.preview(blank.detect(sine({freq = 440}),
                                        {duration = 0.2,
                                         on_blank = function() blanked = blanked + 1 end}))
            "#)
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        for _ in 0..4 {
            assert!(
                root.next_buffer(&mut buf) > 0,
                "healthy audio keeps flowing"
            );
        }
        assert_eq!(root.label().as_deref(), Some("sine 440 Hz"));
        assert!(!root.is_exhausted());
        drop(root);
        _rt.drain_metadata();
        let blanked: u64 = _rt.global("blanked").expect("blanked counter");
        assert_eq!(blanked, 0, "no false positive on a healthy source");
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
            Self {
                label,
                burst1: 100,
                pause_pulls: 1,
                burst2: 100,
                state: 0,
            }
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
        let (_rt, res) = run(r#"
            src = sine({freq = 440, duration = 1})
            output.icecast({mount = "/a.mp3", format = "mp3"}, src)
            output.icecast({mount = "/b.ogg", format = "opus"}, src)
            "#)
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
        let err = match run(r#"
            output.icecast({mount = "/a.mp3"}, sine({freq = 440}))
            output.icecast({mount = "/b.mp3"}, sine({freq = 880}))
            "#)
        {
            Ok(_) => panic!("second output with a different root must fail"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("share the same root source"));
    }

    #[test]
    fn file_output_registers_and_shares_root() {
        let (_rt, res) = run(r#"
            src = sine({freq = 440, duration = 1})
            output.file({path = "/tmp/crabsoup-c1.mp3", format = "mp3", bitrate = 64000}, src)
            output.icecast({mount = "/x.mp3"}, src)
            "#)
        .expect("script runs");
        assert_eq!(res.file_outputs.len(), 1);
        assert_eq!(res.outputs.len(), 1);
        assert!(res.root.is_some());
        assert_eq!(res.file_outputs[0].format, OutputFormat::Mp3);
        assert_eq!(
            res.file_outputs[0].path.to_str(),
            Some("/tmp/crabsoup-c1.mp3")
        );
        assert_eq!(res.file_outputs[0].bitrate, 64_000);
    }

    #[test]
    fn file_output_requires_path_and_shared_root() {
        let err = match run(r#"
            output.file({format = "mp3"}, sine({freq = 440}))
            "#)
        {
            Ok(_) => panic!("output.file without path must fail"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("path is required"));

        let err = match run(r#"
            output.file({path = "/tmp/a.mp3"}, sine({freq = 440}))
            output.file({path = "/tmp/b.mp3"}, sine({freq = 880}))
            "#)
        {
            Ok(_) => panic!("second output.file with a different root must fail"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("share the same root source"));
    }

    #[test]
    fn hls_output_registers_and_defaults() {
        let (_rt, res) = run(r#"
            src = sine({freq = 440, duration = 1})
            output.hls({directory = "/tmp/crabsoup-hls"}, src)
            output.icecast({mount = "/x.mp3"}, src)
            "#)
        .expect("script runs");
        assert_eq!(res.hls_outputs.len(), 1);
        assert_eq!(res.outputs.len(), 1);
        assert!(res.root.is_some());
        assert_eq!(
            res.hls_outputs[0].directory.to_str(),
            Some("/tmp/crabsoup-hls")
        );
        assert_eq!(res.hls_outputs[0].segment_seconds, 5.0);
        assert_eq!(res.hls_outputs[0].retention, 12);
    }

    #[test]
    fn hls_output_requires_directory() {
        let err = match run(r#"
            output.hls({}, sine({freq = 440}))
            "#)
        {
            Ok(_) => panic!("output.hls without directory must fail"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("directory is required"));
    }

    #[test]
    fn normalize_boosts_a_quiet_tone() {
        let (_rt, res) = run(r#"
            output.preview(normalize(sine({freq = 440, duration = 1, amplitude = 0.02}),
                                     {target = -6, attack = 0, release = 0}))
            "#)
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
        assert!(always.matches(&LocalTime {
            weekday: 3,
            minutes: 0
        }));

        let window = TimePredicate {
            days: Some(vec![1, 2, 3, 4, 5]),
            from: Some(9 * 60),
            to: Some(17 * 60),
        };
        let t = LocalTime {
            weekday: 2,
            minutes: 12 * 60,
        };
        assert!(window.matches(&t));
        assert!(!window.matches(&LocalTime {
            weekday: 6,
            minutes: 12 * 60
        }));
        assert!(!window.matches(&LocalTime {
            weekday: 2,
            minutes: 8 * 60
        }));
        assert!(!window.matches(&LocalTime {
            weekday: 2,
            minutes: 17 * 60
        }));
        // `to` is exclusive.
        assert!(!window.matches(&LocalTime {
            weekday: 2,
            minutes: 17 * 60
        }));
    }

    #[test]
    fn time_predicate_wraps_past_midnight() {
        let overnight = TimePredicate {
            days: None,
            from: Some(22 * 60),
            to: Some(6 * 60),
        };
        assert!(overnight.matches(&LocalTime {
            weekday: 0,
            minutes: 23 * 60
        }));
        assert!(overnight.matches(&LocalTime {
            weekday: 0,
            minutes: 3 * 60
        }));
        assert!(!overnight.matches(&LocalTime {
            weekday: 0,
            minutes: 12 * 60
        }));
        // from == to is an empty window.
        let empty = TimePredicate {
            days: None,
            from: Some(0),
            to: Some(0),
        };
        assert!(!empty.matches(&LocalTime {
            weekday: 0,
            minutes: 0
        }));
    }

    #[test]
    fn switch_stays_in_a_window_and_moves_to_the_default_when_it_closes() {
        let clock = Arc::new(Mutex::new(LocalTime {
            weekday: 1,
            minutes: 9 * 60,
        }));
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
                SwitchSlot {
                    when: TimePredicate::always(),
                    child: 1,
                },
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
        let clock = Arc::new(Mutex::new(LocalTime {
            weekday: 1,
            minutes: 9 * 60,
        }));
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
                SwitchSlot {
                    when: TimePredicate::always(),
                    child: 1,
                },
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
        let clock = Arc::new(Mutex::new(LocalTime {
            weekday: 1,
            minutes: 9 * 60,
        }));
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
                SwitchSlot {
                    when: TimePredicate::always(),
                    child: 1,
                },
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
            ScheduleKind::Rotate {
                weights: vec![1, 1],
                cursor: 0,
                spins: 0,
            },
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
        assert_eq!(
            seen,
            vec![
                0.25, 0.25, 0.25, 0.75, 0.75, 0.75, 0.25, 0.25, 0.25, 0.75, 0.75, 0.75
            ]
        );
    }

    #[test]
    fn rotate_with_weights_keeps_a_child_for_more_tracks() {
        let fpb = 100;
        let a = LuaSource::new(Box::new(LabelCycler::new(0.25, &["a1", "a2"], 300)));
        let b = LuaSource::new(Box::new(LabelCycler::new(0.75, &["b1", "b2"], 300)));
        let mut src = ScheduleSource::new(
            ScheduleKind::Rotate {
                weights: vec![1, 2],
                cursor: 0,
                spins: 0,
            },
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
                0.25, 0.25, 0.25, 0.75, 0.75, 0.75, 0.75, 0.75, 0.75, 0.25, 0.25, 0.25, 0.75, 0.75,
                0.75,
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
            ScheduleKind::Rotate {
                weights: vec![1, 1],
                cursor: 0,
                spins: 0,
            },
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
        let clock = Arc::new(Mutex::new(LocalTime {
            weekday: 1,
            minutes: 9 * 60,
        }));
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
                SwitchSlot {
                    when: TimePredicate::always(),
                    child: 1,
                },
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
        let err = run(r#"
            output.preview(switch({
                {when = {from = "09:00", to = "17:00"}, src = sine({freq = 440})}
            }))
            "#)
        .err()
        .expect("script fails");
        assert!(err.to_string().contains("default child"));
    }

    #[test]
    fn switch_rejects_bad_times_and_weekdays() {
        let err = run(r#"
            output.preview(switch({
                {when = {from = "25:00", to = "17:00"}, src = sine({freq = 440})},
                {src = sine({freq = 880})}
            }))
            "#)
        .err()
        .expect("script fails");
        assert!(err.to_string().contains("bad time"));

        let err = run(r#"
            output.preview(switch({
                {when = {days = {"funday"}}, src = sine({freq = 440})},
                {src = sine({freq = 880})}
            }))
            "#)
        .err()
        .expect("script fails");
        assert!(err.to_string().contains("unknown weekday"));
    }

    #[test]
    fn rotate_registers_in_lua() {
        let (_rt, res) = run(r#"
            a = sine({freq = 440, duration = 0.2})
            b = sine({freq = 880, duration = 0.2})
            output.preview(rotate({a, b}, {weights = {1, 2}}))
            "#)
        .expect("script runs");
        let mut root = res.preview.expect("preview source");
        let mut buf = vec![0f32; 4096 * 2];
        root.next_buffer(&mut buf);
        assert_eq!(root.label().as_deref(), Some("sine 440 Hz"));
    }

    #[test]
    fn table_to_json_converts_flat_and_nested_tables() {
        let lua = Lua::new();
        let t: mlua::Table = lua
            .load("return {title = \"Some track.mp3\", started_at = \"now\", loud = true}")
            .eval()
            .unwrap();
        let v = table_to_json(&t).unwrap();
        assert_eq!(v["title"], "Some track.mp3");
        assert_eq!(v["started_at"], "now");
        assert_eq!(v["loud"], true);

        let arr: mlua::Table = lua.load("return {\"a\", \"b\", \"c\"}").eval().unwrap();
        let v = table_to_json(&arr).unwrap();
        assert_eq!(v, serde_json::json!(["a", "b", "c"]));

        let mixed: mlua::Table = lua.load("return {1, 2, name = \"x\"}").eval().unwrap();
        let v = table_to_json(&mixed).unwrap();
        assert_eq!(v["name"], "x");
        assert_eq!(v["1"], 1);
    }

    #[test]
    fn table_to_json_rejects_unsupported_values() {
        let lua = Lua::new();
        let t: mlua::Table = lua.load("return {fn = function() end}").eval().unwrap();
        assert!(table_to_json(&t).is_err());
    }
}
