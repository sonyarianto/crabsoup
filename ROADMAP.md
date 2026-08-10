# Crabsoup roadmap

## Done (verified end-to-end)
- [x] YAML config (`-c crabsoup.yaml`)
- [x] Playlist scheduling (directory scan, loop, shuffle)
- [x] Gapless crossfades (`crossfade_seconds`, `fade_curve`)
- [x] MP3 -> Icecast broadcast, 192 kbps, title metadata
- [x] Graceful Ctrl-C shutdown
- [x] Unit tests (50), incl. resampler multi-chunk regression, Ogg CRC, Opus tags
- [x] Opus end-to-end: encoder + Ogg mux -> Icecast verified live (ffprobe decodes
      mount, listeners get audio). Fixed Ogg page CRC bug (byte must xor into
      the table index, not into the result) that silently corrupted every page.
- [x] Playlist auto-advance + crossfade verified live (track 2 loads at t=3:32)
- [x] Ogg checksum regression test vs. external reference value
- [x] Jingles: playable via the telnet control port (`jingles.list`, `jingles.play
      [n|substr]`, `shutdown`). Verified end-to-end with the 3 staged jingles.
- [x] `.lua` scripting replaces YAML entirely: Liquidsoap-flavoured Lua stdlib
      (`playlist`, `single`, `jingles`, `fallback`/`sequence`/`random`,
      `input.harbor`, `output.icecast`, `output.preview`, `server.telnet`, `set`,
      `log`) evaluated by mlua (vendored Lua 5.4). Scripts define everything:
      stream/mixer settings, sources, services, output. Verified live with
      `crabsoup.lua.example` (Opus mount, telnet jingle trigger).
- [x] Native Icecast source protocol replaces libshout (`icecast_client.rs`):
      single authenticated `SOURCE` request, raw encoded bytes, separate
      `/admin/metadata` GETs for titles. No libshout dependency, no capability
      negotiation, no 401-then-200 double login. Verified live: one `SOURCE`
      request per connect (HTTP 200), title updates reach the mount (status-json
      shows the track), ffprobe decodes the stream.

- [x] Live DJ harbor end-to-end: PUT + Basic auth (200 OK), mixer duck control
      verified (connect/disconnect events; broadcast RMS dips to ~25% then
      recovers full level). Caveat: symphonia 0.5/0.6 has no Opus codec, so
      MP3 uploads decode and air, while Opus uploads log "cannot create
      decoder: unsupported codec" and air silence for the ducked window.

- [x] Opus stream title: investigated to the source. Icecast 2.4.4 never parses
      OpusTags titles (format_opus.c counts header packets only) and rejects URL
      metadata updates for Opus mounts (HTTP 200 + "Mountpoint will not accept
      URL updates"); Icecast 2.5+/master parses only the initial OpusTags header
      (type-less packets only; set_tag is NULL for Ogg there too). So the Opus
      encoder sends OpusHead+OpusTags stream headers containing the first
      track's title (replaced via set_title before the first flush), and never
      injects mid-stream comment pages (Icecast forwards those to listeners as
      audio). Verified live: ffprobe reads `title=` from the stream's first
      OpusTags; MP3 keeps live URL titles; icecast 2.4.4 status-json stays
      title-less for Opus (documented server limitation).

## Known limitations
- DJ uploads must be MP3 for now (symphonia has no Opus codec; 0.6.0 feature
  list confirmed ogg/vorbis/mp3 but no opus). A native Opus decode path
  (audiopus + our ogg demuxer) is future work.
- Icecast 2.4.4 shows no Opus titles (see Done section); 2.5+ shows the
  stream-start title only.

## Next up: Liquidsoap parity + performance

Target: close the gap between a production `.liq` script and `crabsoup.lua`,
landed as independently-shipping phases (inline tests, verified live) while
beating Liquidsoap on CPU/memory per concurrent output and worst-case latency
jitter — via real OS threads and allocation-free hot paths, not "Rust alone".
This section is the plan; the Done sections above stay the source of truth
for what shipped.

### Performance principles (apply to every phase)

- No `Vec::new()`/`vec![...]` inside a `next_buffer` hot path. Scratch
  buffers are sized at construction and resized only if the buffer size
  actually changes.
- Lock once per call, not per method: one `next_buffer` never takes the same
  child `Mutex` twice (`FallbackSource`/`RandomSource` today; future
  `EffectSource`/`OnMetadataSource` wrappers must not either).
- One thread per output plus one puller thread; nothing finer-grained (DSP
  effects stay inline in the pull chain).
- `benches/` with criterion covering the mixers, resampler, and encode path
  once the buffer-reuse + tap work lands; record baseline numbers in
  ROADMAP.md so later phases check against them, not "seems fine".
- SIMD is a later lever (sinc convolution, effect loops) — only after the
  benchmark harness shows it is the bottleneck.

### Part A — architectural prerequisites (landed as one PR, see Done)

A1 and A2 shipped together: the engine tap (single puller, fan-out) and the
Lua-owning event loop. Details in the Done section below.

#### A1 — Engine tap (single puller, multi-consumer fan-out)
Two outputs cannot both call `next_buffer()` on the same root — that is why
`output.icecast` is single-call today and why `on_metadata` cannot hook in
cleanly. Fix: one thread owns the root source and pulls at wall-clock pace
(the loop `IcecastOutput::run` has today), publishing each buffer as an
`Arc<AudioFrame { pcm, label }>` to N bounded `sync_channel(4)` taps.
Outputs (`IcecastOutput`, future `output.file`) become pure consumers
(`for frame in rx { encode + send }`) with independent reconnect loops — one
stalled mount cannot stall the pull or the other outputs.
`ScriptResult.output: Option<OutputConfig>` becomes
`outputs: Vec<OutputConfig>`; a second `output.icecast` call is accepted
when it shares the same source graph (check via `Arc::ptr_eq`).
**Allocation note:** the tap must not allocate per pull either — keep a
preallocated frame pool sized `4 * tap_count + 2` (bounded channels mean the
pool never runs out on the steady-state path) with a fresh-Vec fallback.
Acceptance: two mounts stream concurrently; killing one Icecast connection
does not glitch the other; inline test with two fake consumers reading the
same tap and asserting identical frame sequences.

#### A2 — Lua-owning event loop (unblocks `on_metadata`, `server.register`)
`mlua` is built without the `send` feature (deliberate — see `Cargo.toml`),
so `Lua`/`Function`/`Table` are `!Send` and callable only on the thread that
created them. After A1 the puller is a different thread, so wrapper sources
send owned `Send` events (`ScriptEvent::Metadata { hook_id, title }`,
`Shutdown`) over a channel; the Lua-owning thread runs the event loop and
invokes hooks stored in `Vec<mlua::Function>` on `ScriptState`. `script::run`
hands back the `Lua` instance (`ScriptRuntime`), which now outlives script
evaluation. **Gotcha when it landed:** the `on_metadata` closure stays in the
Lua registry for the process lifetime, so its captured channel `Sender` never
drops — the event loop must poll with `recv_timeout` and exit on a shared
end-flag (the tap sets it when the engine stops) instead of waiting for
channel disconnection. Event rate is metadata-rate (per track), not per
buffer; audio-rate callbacks need their own budget/backpressure story.

### Part B — feature phases

#### Phase 3 — request queue
- [x] FIFO source pushed at runtime via telnet `queue.push <path>`, plays
      when non-empty, exhausts when empty (composes in `fallback` before the
      playlist, like `request.queue`); `queue.list`, `queue.clear`; `skip`
      wired to the current track (playlist skip).
- [x] `server.register` (Lua API for custom telnet commands) — natural once
      A2's event-loop pattern exists; until then, custom commands that do not
      call back into Lua only.

#### Phase 4 — multi-output + file recording (needs A1)
- [x] `output.icecast` callable more than once (different mounts/formats,
      same source graph) — part of Part A.
- [x] `output.file({path, format, bitrate}, source)`: tap consumer with a
      file sink — part of C1.

#### Phase 5 — scheduling (dayparting)
- [x] `switch` source with time-based brackets (liq `switch` semantics:
      weekday/hour ranges, default child).
- [x] `rotate` source (sequential/even rotation over children).

#### Phase 6 — metadata hooks (needs A2)
- [x] `on_metadata(callback, source)`: A2 event loop, metadata table
      (title) per track start — part of Part A.
- [x] `on_track(callback, source)`: second `ScriptEvent` variant, fires on
      track boundary without full metadata.

#### Phase 7 — request protocols (`http://` resolution, biggest lift)
- [x] `request` abstraction resolving URIs through pluggable protocols.
      Scope down to download-then-play first (retry/timeout, temp-file
      lifecycle) before attempting a streaming-decode path.

#### Phase 8 — loudness (replaygain / R128, stretch)
- [ ] Feed ReplayGain/R128 tags into the Phase 2 `Agc`/`normalize` gain
      baseline. Only once the envelope follower exists.

### Part C — output-format & delivery track (parallel with Part B)

Touches `src/output/` and `src/live/`, not `script.rs` source composition —
no contention with Part B.

- [ ] **C1 — `output.file` + multi-mount `output.icecast`** (needs A1;
      multi-mount landed with Part A; `output.file` landed as its own
      commit — see Done section).
- [x] **C2 — AAC encoder** (no dependency, can start immediately): new
      `Encoder` impl, FFI to `fdk-aac` following the LAME `unsafe extern
      "C"` template (opaque handle, explicit `Drop`, `unsafe impl Send` with
      justification). Raw ADTS framing over the existing `IcecastClient`;
      `audio/aac` content-type branch next to the MP3/Opus branches. Low
      risk, runnable alongside Phase 2. (Landed — see Done section.)
- [x] **C3 — HLS output** (needs C2): `src/output/hls.rs` + `src/output/mpegts.rs`
      — minimal MPEG-TS muxer (PAT/PMT sections per segment, PES-wrapped
      ADTS on one audio PID, PCR every ~100 ms, per-PID continuity
      counters; section CRC reuses the shared MPEG-2/Ogg table) and a
      segmenter that rotates the tap's AAC stream into `seg-NNNNNN.ts`
      files plus `playlist.m3u8` (`#EXT-X-VERSION:3`, sliding
      `MEDIA-SEQUENCE`, `TARGETDURATION`, `ENDLIST` on graceful shutdown).
      `output.hls({directory, segment_seconds, retention}, source)` in
      script.rs; directory prepared in `connect()` so a bad path fails at
      startup; one tap-consumer thread per HLS output. Acceptance:
      `ffmpeg -f hls -i playlist.m3u8` decodes a live window cleanly (sine
      and real-MP3 playlist runs), ffprobe sees AAC 44.1 kHz stereo on the
      90 kHz TS clock, SIGINT closes the final segment and ends the list.
      (Note: decoding a *single* segment through the ffmpeg CLI at
      `-v warning`+ can report "Output file is empty" — an upstream
      `discard_unused_programs` race in ffmpeg 7's threaded input path,
      not a crabsoup defect; ffprobe and full-playlist decode are clean.)
- [x] **C4 — Shoutcast v1/v2** (only if a concrete need shows up): alternate
      handshake inside `icecast_client.rs`, exposed as `protocol =
      "icecast" | "shoutcast"` on `output.icecast`'s config table.

### Suggested execution order

1. Buffer-reuse pass in `CrossfadeMixer`/`PriorityMixer` (perf, low risk, no
   API change) — **done** alongside Phase 1.
2. Phase 1 (primitives) — **done** (see Done section).
3. Part A (A1 + A2 together — they share the "who owns the pull loop"
   decision, easier to land as one architectural PR than two) — **done**
   (see Done section).
4. Track B, sequential: Phase 2 (DSP) — **done**; next Phase 3 → Phase 5 →
   Phase 6 (needs A2) → Phase 7 → Phase 8 (stretch).
5. Track C once A1 lands: C1 → C2 → C3 (needs C2) → C4 (on request).

If effort is constrained to one track at a time, prioritize Track C through
C1/C2 ahead of Track B phases 5–8 — output breadth (file recording, multiple
mounts, AAC) is the highest-value/lowest-risk next step for a station in
production. Track B Phase 2 (DSP) is cheap enough to interleave regardless.

Non-goals for now: full `.liq` language compatibility (the Lua stdlib
approximates the operator surface, not the language); LADSPA plugin hosting
(revisit only if a concrete need appears); clock-synchronized multi-output
(one puller + fan-out first; revisit only if drift between outputs matters in
practice).

## Done (cont.)
- [x] Phase 3 (request queue): `src/source/request.rs` — `RequestQueue`
      (FIFO of `Arc<str>` paths, `next()` pops under one lock, `skip()`
      jumps the current request instead of waiting, `Clear` command
      flushes). `request.queue()` registered in `script.rs` as a normal
      source (returns `LuaSource`; no global state, composes anywhere
      `fallback`/`sequence`/`random` take children — the queue only
      fires when non-empty, like liq `request.queue`). `MixCommand::
      QueuePush/QueueList/QueueSkip/QueueClear` in `control.rs`; telnet
      `queue.push <path>` (queues without touching the playlist),
      `queue.list`, `queue.skip` (skips the currently-queued request),
      `queue.clear`. `ControlState.status` line shows the queued item
      while paused (telnet `status` renders it). Note: a request is
      consumed when playback *starts* (Liquidsoap semantics) — `queue.list`
      shows empty while a queued track is on air. Inline tests (99 -> 104):
      push/pop ordering, exhaust-then-silence, skip mid-request, clear,
      queue+pause status, skip-noop on an empty queue, fallback
      composition and handover back to the playlist. Verified live over
      the telnet port with a harbor + `output.preview` script: `queue.push`
      of a staged jingle preempts the looping playlist (`request queue:
      playing ...`), `queue.skip` returns to the playlist, `queue.clear`
      flushes, unknown commands are rejected.
- [x] Phase 5 (scheduling / dayparting): one `ScheduleSource` in `script.rs`
      powers both `switch` and `rotate`. The active child plays the whole
      current track; a re-pick happens only at a track boundary (the
      child's `label` changes or it exhausts) — at most one buffer of the
      next track comes from the old child (20 ms). `switch({ {when = ...,
      src = ...}, ... }, {track_sensitive = ...})`: each slot carries a
      `TimePredicate` (`days` as weekday names or 0=Sunday..6=Saturday,
      `from`/`to` as `"HH:MM"`; overnight windows wrap past midnight,
      `from == to` never matches); slots are checked from the top at every
      boundary, so a window opening grabs the next track; a slot without
      `when` is the required default child. `track_sensitive = false`
      re-evaluates the predicates every pull and cuts mid-track.
      `rotate({...}, {weights = ...})`: weighted round-robin, a weight `w`
      holds a child for `w` consecutive tracks, exhausted children are
      skipped. Boundary re-picks, exhaustion handover, and the injected
      clock (`Fn() -> LocalTime`, chrono `Local::now()` by default) are
      unit-tested with `LabelCycler` fakes (117 -> 117 suite overall,
      +14 for Phase 5: predicate windows/days/overnight/empty, track-
      sensitive hold-then-switch, non-sensitive mid-track cut, rotate
      cycling, weighted rotate, exhausted-child skip, `skip()` repick,
      Lua registration + default-child and bad-time/weekday errors).
      Verified live: `switch` with `days = {"sun"}` picked the Sunday
      branch on-air; telnet `skip` through `rotate({a, b})` alternated
      children track-by-track.
- [x] Phase 6 (on_track): `ScriptEvent::Track { hook_id }` joins
      `Metadata`; `ScriptState`/`ScriptRuntime` carry a separate
      `track_hooks` vec (a `Track` event can never hit an `on_metadata`
      callback). `OnTrackSource` wraps a child like `OnMetadataSource` but
      reports a boundary on *any* track start: the child's `label` changed
      (even to `None`), or it produces audio again after being silent
      without exhausting (a paused/request-queue child resuming — a
      boundary `on_metadata` would miss since the label may not change).
      `on_track(callback, source)` registered in `script.rs`; the callback
      runs on the Lua-owning event loop with no arguments (ROADMAP scope:
      boundary without metadata). Inline tests (117 -> 120): one event per
      track over a `sequence` of two sines, single short child, and a
      `BurstySource` fake proving a resume-after-pause fires with an
      unchanged label. Verified live: three 1.5 s sine tracks in sequence
      logged `on_track #1/2/3` from the Lua callback on-air.
- [x] Phase 7 (request protocols): `src/request.rs` — `RequestUri`
      (`Local(PathBuf)` | `Url(String)`, `new`/`raw`/`display` where
      `display` is the last path segment) and `RequestConfig`
      (`request_timeout` secs, default 30; `request_retries`, default 2)
      configurable via `set()`. `RequestQueue` and `Playlist` now hold
      `Vec<RequestUri>` and resolve at pop/next-track time through
      `resolve()`: `Local` opens a `FileSource` directly; `Url` is
      downloaded to a per-URL temp file under
      `$TMPDIR/crabsoup-requests/{fnv1a}-{n}.part` (stable name + per-process
      counter, `.part` name kept for the played file) and wrapped in a
      `DownloadSource` that removes the temp file on drop — the playlist
      loop re-requests re-downloads. The HTTP client is hand-rolled on
      `std::net` (no new crates): `http://` only (https rejected with a
      clear error), redirects up to 4 (Location joined absolute/root/relative
      against the request URL), `Content-Length` and `Transfer-Encoding:
      chunked` bodies, connection-close, per-attempt connect+read timeout;
      failed attempts retry with a 500 ms backoff and the partial file is
      removed on final failure. `single(url)`, `playlist({directory=...,
      files = {...}})` entries, and telnet `queue.push <url>` all accept
      `http://` now (on-air label = URL display name). Inline tests
      (120 -> 128): request-uri classification, content-length and chunked
      bodies against tiny `TcpListener` servers (request-draining so
      responses close cleanly without RST), relative `Location` redirect,
      404 surfacing, https rejection, retry-then-fail cleanup, temp-path
      stability/uniqueness, and queue/playlist resolution plumbing in
      `script.rs`. Verified live: single played a 5.2 MB mp3 served by
      `python3 -m http.server` (temp file appeared and was removed), on-air
      label was the URL track name, telnet `queue.push` of a second HTTP
      URL preempted and played, `queue.skip` dropped it and its temp file
      vanished.
- [x] `server.register` (custom telnet commands): `server.register(name,
      fn)` registers a named command in `script.rs`; `ScriptResult` mirrors
      the names (registration order = index into the runtime's callback vec)
      to the control port, and `ScriptRuntime.event_tx()` hands the port a
      `ScriptEvent` sender. The port routes any unrecognized first word that
      matches a registered name to the Lua-owning event loop
      (`ScriptEvent::Custom { index, args, reply }`, a fresh
      `mpsc::Sender<Result<String, String>>` per call) and blocks up to 5 s
      for the reply — the handler receives the rest of the line as one
      string and returns the reply; a Lua `error()` becomes an `ERROR: ...`
      reply with the traceback; a dead event loop replies "script event loop
      is not running". `dispatch` was refactored into a `DispatchCtx` struct
      (clippy arg-count). Inline tests (128 -> 133): Lua handler round-trip
      with args through the real event loop, callback-error reporting, bad
      names rejected (`server.register` name must be a single non-empty
      word), control-port routing over the event channel, and
      unregistered-name rejection. Also fixed a wall-clock flake:
      `switch_registers_in_lua_with_a_default_child` asserted the default
      branch, which only holds outside the weekday 09:00-17:00 window — it
      now asserts *a* child label. Verified live: `ping hello world` ->
      `pong [hello world]`, `stats 42` -> `stats: 42 track(s)`, `stats -1`
      -> `ERROR: runtime error ... negative count` with traceback, `bogus`
      still `unknown command`, `ping` -> `pong []`; `shutdown` stopped the
      process cleanly.
- [x] C1 (`output.file`): `src/output/file.rs` — `FileOutput`, a tap
      consumer mirroring `IcecastOutput` minus the network: no pacing, no
      reconnect; the encoder and file are created in `connect()` so a bad
      path fails at startup (`main.rs` fails fast before the tap starts).
      `output.file({path, format, bitrate}, source)` registered in
      `script.rs` alongside the icecast closure via a shared `claim_root`
      helper (`Arc::ptr_eq` check); `ScriptResult.file_outputs:
      Vec<FileOutputConfig>`. `main.rs` spawns one consumer thread per file
      output and now surfaces output-thread errors after unwind (first error
      becomes the process exit error). Inline tests: MP3 file written from a
      sine is decoded back with symphonia and asserted non-silent; Opus file
      asserts Ogg magic + size; script tests for registration, root sharing,
      missing path, and mixed-root rejection. Verified live: mp3 (44.1 kHz)
      + Opus (48 kHz) recordings from a 25 s broadcast decode cleanly with
      ffprobe and close on telnet shutdown; broadcast unaffected.
- [x] C2 (AAC encoder): `AacEncoder` in `src/output/encoder.rs` — FFI to
      `fdk-aac` following the LAME `unsafe extern "C"` template (opaque
      handle, explicit `Drop`, `unsafe impl Send`). Raw ADTS framing, set
      via `AACENC_TRANSMUX`/`TT_MP4_ADTS`; AAC-LC, mono/stereo, bitrate in
      bits/s, 44.1 kHz native (no resampler needed). FDK consumes at most
      one frame per `aacEncEncode` call (excess input silently dropped), so
      `encode` loops on the leftover, honoring `numInSamples`; `finish`
      drains with `numInSamples = -1` until `AACENC_ENCODE_EOF`. Built
      from source into `/usr/local` (no distro package; `build.rs` adds the
      link path). `audio/aac` content-type branch next to MP3/Opus;
      ADTS has no in-stream title mechanism, so `set_title` stays a no-op.
      `output.file` + Icecast mount verified live: ffprobe decodes the
      file and the `/c2.aac` mount at 44.1 kHz stereo 128 kbps.
- [x] Part A (engine tap + Lua event loop, one PR): `src/engine/tap.rs` —
      `EngineTap` owns the root source on its own thread and publishes each
      wall-clock-paced buffer as `Arc<AudioFrame { pcm, label }>` to N
      bounded `sync_channel(4)` taps; outputs became pure consumers with
      independent reconnect loops. `IcecastOutput` now consumes frames from
      the tap (no pull loop, no pacing); the preview loop is a tap consumer
      too. Frame recycling via a preallocated pool sized `4 * tap_count + 2`
      (fresh-Vec fallback on the degraded path only) — steady state stays
      allocation-free. `script.rs`: `ScriptResult.outputs: Vec<OutputConfig>`
      (second `output.icecast` accepted iff it shares the same source graph,
      checked via `Arc::ptr_eq`); `script::run` returns `(ScriptRuntime,
      ScriptResult)`; `ScriptRuntime` owns the `Lua` state plus the event
      loop and `drain_metadata()`; `OnMetadataSource` wrapper emits
      `ScriptEvent::Metadata { hook_id, title }` on label change; `on_metadata`
      registers a Lua callback invoked on the Lua-owning main thread. The
      event loop polls with `recv_timeout` and exits on a shared end-flag
      (the tap sets it when the engine stops for any reason) — channel
      disconnection is not observable while the runtime lives (the registry
      closure keeps a `Sender`), which initially hung the loop. Verified
      live: mp3 + opus mounts from one root stream concurrently on icecast2
      (ffprobe decodes both), metadata events reach the Lua callback, telnet
      `shutdown` exits the process cleanly, `--check` prints every output,
      full test suite 84 passed incl. tap fan-out, stalled-consumer, metadata
      ordering, and shared-root rejection tests.
- [x] Phase 2 (DSP effect chain): `src/engine/effects.rs` gains `Compressor`
      (feed-forward envelope follower, gain reduction only above
      `threshold_db`, `ratio`, `attack`/`release` time constants, `makeup`)
      and `Agc` (same follower shape with a faster measurement stage, rides
      the gain toward `target_db` with slow-boost/fast-cut smoothing,
      `max_boost_db`/`max_cut_db` clamps — backs `normalize`). Operators
      `compress(source, opts)` and `normalize(source, opts)` registered in
      `script.rs` via one `lua.create_function` each, chaining through the
      same `EffectSource` as `amplify`. Inline tests with synthetic sine
      input assert exact expected sample values (gain reduction only above
      threshold, ratio-1 transparency, makeup gain, quiet signal brought up
      toward the target, max-boost clamp, slow gain on silence so it does
      not pump). Verified live: `examples/crabsoup.dsp.lua` (compress ->
      normalize -> amplify on a 440 Hz tone) runs in preview.
- [x] Phase 1 (ops primitives): `blank`/`sine` test sources (optional
      `duration`; exhaust cleanly so `fallback` hands over), `amplify`
      operator via new `src/engine/effects.rs` (`Effect` trait +
      `EffectSource<E>`, ready for the Phase 2 DSP chain), telnet `skip`
      (`AudioSource::skip()` trait default no-op; `CrossfadeMixer` advances
      the current track; `MixCommand::Skip`), telnet `status`/`uptime` via
      a shared `StatusHandle` the pump loop keeps fresh, README parity map
      (.liq -> .lua table). Verified live: `examples/crabsoup.tone.lua`
      (60 s 440 Hz tone -> sequence -> blank) shows `sine 440 Hz` in
      `status`; telnet `skip` and `shutdown` reach the mixer; `shutdown`
      stops both pump loops (previously a no-op — `PriorityMixer::is_shutdown`
      was never consulted, so telnet shutdown only replied). Mixer hot paths
      now reuse scratch buffers (no per-`next_buffer` allocation in
      `CrossfadeMixer`/`PriorityMixer`).
- [x] Preview mode via `--preview` (forced even with `output.icecast`,
      combines with `--check`; verified live).
- [x] Opus resampler upgraded: 16-tap Hann-windowed sinc polyphase FIR (256 phases),
      DC-normalized per output sample so chunk edges and stream edges stay
      unity-gain; `PcmConverter` (bus normalization) uses the same filter.
