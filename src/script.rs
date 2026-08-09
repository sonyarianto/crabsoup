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
use std::sync::{Arc, Mutex};

use mlua::{FromLua, Lua, Table, UserData, Value as LValue};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use symphonia::core::audio::SignalSpec;

use crate::config::{
    collect_audio, ControlConfig, LiveConfig, MixerConfig, OutputConfig, OutputFormat, StreamConfig,
};
use crate::engine::effects::{Agc, Amplify, Compressor, EffectSource};
use crate::engine::mixer::CrossfadeMixer;
use crate::source::file::FileSource;
use crate::source::playlist::Playlist;
use crate::source::{AudioSource, BlankSource, SilenceSource, SineSource};

/// Everything the engine needs after a `.lua` script finishes evaluating.
pub struct ScriptResult {
    pub stream: StreamConfig,
    pub mixer: MixerConfig,
    pub jingles: Vec<PathBuf>,
    pub harbor: Option<LiveConfig>,
    pub control: Option<ControlConfig>,
    pub output: Option<OutputConfig>,
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
    jingles: Vec<PathBuf>,
    harbor: Option<LiveConfig>,
    control: Option<ControlConfig>,
    output: Option<OutputConfig>,
    root: Option<Box<dyn AudioSource>>,
    preview: Option<Box<dyn AudioSource>>,
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
    /// the value is still shared with other Lua references (single-chain
    /// engine consumes the source at the output).
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

/// Selects children in script order: the first non-exhausted child wins
/// (Liquidsoap's `fallback` / `sequence`).
struct FallbackSource {
    children: Vec<LuaSource>,
    current: usize,
}

impl FallbackSource {
    fn new(children: Vec<LuaSource>) -> Self {
        Self { children, current: 0 }
    }
}

impl AudioSource for FallbackSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        while self.current < self.children.len() {
            let n = self.children[self.current].0.lock().unwrap().next_buffer(buffer);
            if n > 0 {
                return n;
            }
            if self.children[self.current].0.lock().unwrap().is_exhausted() {
                log::debug!("fallback: child {} done, switching", self.current);
                self.current += 1;
                continue;
            }
            return 0;
        }
        0
    }

    fn is_exhausted(&self) -> bool {
        self.current >= self.children.len()
            || self.children[self.current..]
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
            .or_else(|| Some("(no source)".into()))
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

/// Signal spec + frames-per-buffer from the current script settings.
fn bus(state: &Rc<RefCell<ScriptState>>) -> (SignalSpec, usize) {
    let s = state.borrow();
    (s.stream.signal_spec(), s.stream.frames_per_buffer)
}

/// A playlist whose tracks crossfade into each other, presented as a plain
/// source so it composes inside fallback/random.
fn crossfading_playlist(
    paths: Vec<PathBuf>,
    shuffle: bool,
    loop_playlist: bool,
    state: &Rc<RefCell<ScriptState>>,
) -> Box<dyn AudioSource> {
    let (spec, fpb) = bus(state);
    let chans = spec.channels.count();
    let mixer_cfg = state.borrow().mixer.clone();
    let playlist = Playlist::new(paths, shuffle, loop_playlist, spec, fpb, None);
    Box::new(CrossfadeMixer::new(Box::new(playlist), &mixer_cfg, spec.rate, chans))
}

/// Evaluate a `.lua` script and return the engine wiring it describes.
pub fn run(src: &str) -> mlua::Result<ScriptResult> {
    let lua = Lua::new();
    let globals = lua.globals();
    let state = Rc::new(RefCell::new(ScriptState::default()));

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

            let mut paths = Vec::new();
            if let Some(dir) = &directory {
                collect_audio(&PathBuf::from(dir), &mut paths);
            }
            paths.extend(files.iter().map(PathBuf::from));
            paths.sort();
            paths.dedup();
            if paths.is_empty() {
                return Err(mlua::Error::runtime(
                    "playlist: no audio files found (check `directory`/`files`)",
                ));
            }
            let src = crossfading_playlist(paths, shuffle, loop_playlist, &pl_state);
            Ok(LuaSource::new(src))
        })?,
    )?;

    let single_state = state.clone();
    globals.set(
        "single",
        lua.create_function(move |_, path: String| {
            let (spec, fpb) = bus(&single_state);
            let src = FileSource::open(&PathBuf::from(&path), spec, fpb)
                .map_err(mlua::Error::runtime)?;
            Ok(LuaSource::new(Box::new(src)))
        })?,
    )?;

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

    // ---- effects (Liquidsoap `amplify`, `compress`, `normalize`) ---------
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
            // composes like any other source.
            let src = crossfading_playlist(paths, false, false, &jingle_state);
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
    let server = lua.create_table()?;
    server.set("telnet", telnet_fn)?;
    globals.set("server", server)?;

    // ---- outputs ----------------------------------------------------------
    let out_state = state.clone();
    let make_output = lua.create_function(move |_, (opts, mut source): (Table, LuaSource)| {
        let format = opts
            .get::<Option<String>>("format")?
            .map(|f| match f.as_str() {
                "mp3" => Ok(OutputFormat::Mp3),
                "opus" => Ok(OutputFormat::Opus),
                other => Err(mlua::Error::runtime(format!(
                    "unknown output format {other:?} (use \"mp3\" or \"opus\")"
                ))),
            })
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
        if s.root.is_some() {
            return Err(mlua::Error::runtime(
                "only one output.icecast per script (single-chain engine)",
            ));
        }
        s.root = Some(source.take());
        s.output = Some(cfg);
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
    globals.set("output", output)?;

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
        output: s.output.take(),
        root: s.root.take(),
        preview: s.preview.take(),
    };
    if result.root.is_none() && result.preview.is_none() {
        return Err(mlua::Error::runtime(
            "script defines no output: add output.icecast(...) or output.preview(...)",
        ));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_sets_settings_via_lua() {
        let res = run(
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
    fn compose_sources_without_files() {
        let res = run(
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
        let res = run(
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
        let res = run(
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
    fn compress_reduces_a_loud_tone() {
        let res = run(
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
    fn normalize_boosts_a_quiet_tone() {
        let res = run(
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
}
