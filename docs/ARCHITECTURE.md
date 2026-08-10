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
  `fallback`/`sequence`/`random`, `switch`, `rotate`, `input.harbor`,
  `output.icecast`, `output.file`, `output.preview`, `server.telnet`,
  `on_metadata`, `on_track`, `set`, `log`.
- Sources are Lua userdata wrapping `Arc<Mutex<Box<dyn AudioSource>>>` so they
  compose; `LuaSource::take` steals the box via `mem::replace` (mlua keeps a
  clone on the stack during the call, so `Arc::try_unwrap` would fail).
- Returns `mlua::Result` — `mlua::Error` is `!Send`, so the crate `Result`
  alias cannot hold it; main maps it to a string.
- `set` keys: `sample_rate`, `channels`, `frames_per_buffer`,
  `crossfade_seconds`, `fade_curve`, `duck_seconds`, `request_timeout`,
  `request_retries`.
- `script::run` returns `(ScriptRuntime, ScriptResult)`; `ScriptResult`
  carries `outputs: Vec<OutputConfig>` — multiple `output.icecast` calls are
  accepted iff they share the same source graph (`Arc::ptr_eq` check).
- `ScriptRuntime` owns the `Lua` state, which now outlives script evaluation.
  `on_metadata(callback, source)` wraps the source in `OnMetadataSource`,
  which sends `ScriptEvent::Metadata { hook_id, title }` over an `mpsc`
  channel; the Lua-owning main thread invokes the callback. `on_track`
  wraps in `OnTrackSource` (`ScriptEvent::Track { hook_id }`) and fires at
  *any* track boundary: label change (even to `None`) or a resume after a
  non-exhausted silence — both hook registries are separate vectors, so a
  `Track` event never reaches an `on_metadata` callback.
- `server.register(name, fn)` extends the same event-loop pattern to telnet:
  `ScriptResult.custom_commands` mirrors the registered names (registration
  order = index into the runtime's callback vec) into the control port's
  routing table; `ScriptRuntime.event_tx()` gives the port a
  `ScriptEvent` sender. A custom command becomes `ScriptEvent::Custom {
  index, args, reply }` (fresh `mpsc::Sender<Result<String, String>>` per
  call); the event loop runs the handler with the rest of the line as one
  string and replies. The port blocks up to 5 s on the reply — callback
  errors and a dead event loop both surface as `ERROR: ...` replies.
- `switch`/`rotate` share one `ScheduleSource`: the active child plays the
  whole track, and a new child is re-picked only at a track boundary
  (child `label` change or exhaustion). `switch` slots are `when` predicates
  (`days` names/0-6, `from`/`to` `"HH:MM"`, overnight wrap; omitted `when` =
  default child) checked from the top, so a window opening grabs the next
  track; `rotate` advances a weighted round-robin cursor per boundary.
  `track_sensitive = false` re-evaluates every pull and cuts mid-track.
  The clock is an injected `Fn() -> LocalTime` (chrono `Local::now()` by
  default) so tests pin wall time.

## Request protocols (`src/request.rs`)

- `RequestUri` (`Local(PathBuf)` | `Url(String)`): `single`, `playlist`
  entries, and the telnet request queue all carry URIs and resolve at
  play time via `resolve()`. `Local` opens a `FileSource`; `Url` downloads
  to `$TMPDIR/crabsoup-requests/{fnv1a}-{n}.part` (stable per-URL name +
  per-process counter so concurrent same-URL downloads can't collide, and a
  playlist loop re-requesting the URL re-downloads it) and wraps the file
  in `DownloadSource`, whose `Drop` removes the temp file — a killed process
  leaks `.part` files by design.
- The HTTP client is stdlib-only: single connect per attempt, up to 4
  redirects (`Location` resolved absolute/root/relative against the request
  URL), `Content-Length` and chunked bodies, connection-close fallback,
  connect+read `request_timeout`. https is rejected up front with a clear
  error. `download()` retries `request_retries` times with a 500 ms backoff
  and removes the partial file if all attempts fail.
- `show_error`-style fallback semantics: a failed `resolve()` surfaces as an
  error the caller decides on — the request queue drops the bad request and
  plays the next one; the playlist plays silence for that slot.

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

## AAC path

`AacEncoder` (`src/output/encoder.rs`) wraps FDK-AAC via FFI (same opaque
handle + explicit `Drop` template as LAME): AAC-LC, mono/stereo, bitrate in
bits/s, raw ADTS transport (`AACENC_TRANSMUX`/`TT_MP4_ADTS`). 44.1 kHz needs
no resampler. FDK consumes **at most one frame's worth of input per
`aacEncEncode` call** (`nSamplesToRead - nSamplesRead`; excess is silently
dropped), so `encode` loops on the leftover using the reported
`numInSamples`, and `finish` drains with `numInSamples = -1` until
`AACENC_ENCODE_EOF`. ADTS has no in-stream title mechanism — `set_title`
stays the trait no-op. fdk-aac has no distro package; it's built from source
into `/usr/local` and `build.rs` adds the link path.

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
