# Crabsoup architecture

Implementation-level companion to `AGENTS.md`: module wiring, threading
model, and the gotchas that have burned previous work. User-facing docs live
in `README.md`; the plan and ship history in `ROADMAP.md`.

## Pipeline

`Playlist -> CrossfadeMixer -> PriorityMixer -> EngineTap -> [Encoder -> Icecast]*`
via the native source-protocol client. The `.lua` script's root source (e.g.
`fallback({j, live, pl})`) is that Playlist input; all sources are normalised
to the PCM bus (`set("sample_rate", ...)`, `set("channels", ...)`,
`frames_per_buffer`).

## Threading model

One engine thread plus one thread per output:

- `EngineTap` (`src/engine/tap.rs`) owns the root source and pulls at
  wall-clock pace, publishing each buffer as `Arc<AudioFrame { pcm, label }>`
  to N bounded `sync_channel(4)` taps.
- Outputs are pure consumers (`for frame in rx { encode + send }`) with
  independent reconnect loops — one stalled mount drops frames instead of
  stalling the pull or the other outputs.
- Frames return their `pcm` to a preallocated pool (`4 * tap_count + 2`
  buffers) via `Drop`; allocation only happens on the degraded stall path.
- The tap paces the stream; no output sleeps (`IcecastOutput::run` has no
  pacing code).
- When the tap stops for any reason it sets the shared end-flag, which wakes
  the Lua-owning main thread's event loop.

## Script layer (`src/script.rs`)

- Registers the Liquidsoap-flavoured Lua stdlib: `playlist`, `single`,
  `blank`, `sine`, `amplify`, `compress`, `normalize`, `jingles`,
  `fallback`/`sequence`/`random`, `input.harbor`, `output.icecast`,
  `output.file`, `output.preview`, `server.telnet`, `on_metadata`, `set`,
  `log`.
- Sources are Lua userdata wrapping `Arc<Mutex<Box<dyn AudioSource>>>` so they
  compose; `LuaSource::take` steals the box via `mem::replace` (mlua keeps a
  clone on the stack during the call, so `Arc::try_unwrap` would fail).
- Returns `mlua::Result` — `mlua::Error` is `!Send`, so the crate `Result`
  alias cannot hold it; main maps it to a string.
- `set` keys: `sample_rate`, `channels`, `frames_per_buffer`,
  `crossfade_seconds`, `fade_curve`, `duck_seconds`.
- `script::run` returns `(ScriptRuntime, ScriptResult)`; `ScriptResult`
  carries `outputs: Vec<OutputConfig>` — multiple `output.icecast` calls are
  accepted iff they share the same source graph (`Arc::ptr_eq` check).
- `ScriptRuntime` owns the `Lua` state, which now outlives script evaluation.
  `on_metadata(callback, source)` wraps the source in `OnMetadataSource`,
  which sends `ScriptEvent::Metadata { hook_id, title }` over an `mpsc`
  channel; the Lua-owning main thread invokes the callback.

## Mixer control (`src/engine/mixer.rs`)

- `MixCommand` is the mixer control channel (`SetLive`, `ClearLive`,
  `PlayJingle(PathBuf)`, `Skip`, `Shutdown`) over `std::sync::mpsc`; the
  harbor and control port send into it. `Skip` calls `AudioSource::skip()`
  (trait default no-op; `CrossfadeMixer` advances); `Shutdown` makes
  `PriorityMixer` return 0/exhausted so every pump loop exits.
- `StatusHandle` is the shared label/uptime cell the tap consumers update and
  the telnet `status`/`uptime` read.
- `PriorityMixer` crossfades between `main` and an override with a gain ramp
  over `duck_seconds`; the override audio is `m*(1-gain) + o*gain`.
- Both mixers keep reusable scratch `Vec<f32>` fields (sized on buffer-size
  change) so `next_buffer` never allocates.

## Opus path

`SincResampler` (16-tap Hann-windowed sinc, 256-phase table) bus -> 48 kHz,
encode 20 ms frames, mux one Ogg page per packet, flush per packet so audio
reaches Icecast promptly.

## Icecast client (`src/output/icecast_client.rs`)

- Native source-protocol client (no libshout): one authenticated `SOURCE`
  request, then raw encoded bytes; titles go out on separate authenticated
  `/admin/metadata` GETs. One request per operation — no libshout capability
  negotiation, no unauthenticated 401 probe, no `!POKE`.
- Opus titles ride the stream header: Icecast rejects URL metadata updates
  for Opus mounts (HTTP 200 + "Mountpoint will not accept URL updates"), so
  the initial OpusTags carries the first track's title (`set_title` replaces
  it until the headers flush, then no-ops). Icecast 2.4.4 never parses
  OpusTags titles at all; 2.5+ parses only the stream-start header and
  requires type-less packets (no RFC 7845 packet-type byte — ffmpeg writes
  them type-less too). Never inject comment pages mid-stream: Icecast
  forwards them to listeners as audio, producing decoder warnings.

## Live harbor (`src/live/harbor.rs`)

Decodes DJ uploads with symphonia, which has no Opus codec (confirmed through
0.6.0): MP3 uploads decode and air; Opus uploads log "cannot create decoder:
unsupported codec" and air silence for the ducked window while the duck
control still runs.

## Gotchas that have burned previous work

- Ogg CRC is CRC-32/MPEG-2 (MSB-first, init 0, poly 0x04c11db7, no final
  xor). The input byte xors into the table **index**
  (`idx = ((crc >> 24) ^ b) & 0xff`), not into the result. A previous bug
  corrupted every page silently; `crc_matches_external_reference` guards it.
- Opus requires 48 kHz sample rate — never feed it the bus rate directly.
- The `on_metadata` closure stays in the Lua registry for the process
  lifetime, keeping its channel `Sender` alive: the event loop can never wait
  for channel disconnection, so it polls `recv_timeout` and exits on the
  shared end-flag the tap sets when the engine stops. `drain_metadata()` is
  the non-blocking variant for tests.
