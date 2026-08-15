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
      recovers full level). MP3 uploads decode via symphonia; Opus uploads
      take the native decode path (next entry).
- [x] Native Opus decode (`src/source/opus.rs`): symphonia 0.5 has no Opus
      codec, so the built-in Ogg demuxer (page CRC against the shared
      MPEG-2 table) + libopus via `audiopus` handle `.opus` files and live
      DJ streams. `request.rs::open_audio` probes symphonia first (MP3 /
      Vorbis / AAC), falling back to `OpusSource`; the harbor sniffs the
      DJ stream's first Ogg page (fed back with `PrependReader`) and takes
      the Opus path directly. Verified live: 96 kbps Opus DJ via ffmpeg
      decoded clean end-to-end, full-file `curl -T` Opus uploads played,
      real `.opus` files air through `single`/playlist. Also fixed bounded
      uploads (`curl -T file`): the harbor deferred its final 200 until the
      Content-Length body was fully consumed — curl aborts a transfer the
      moment a complete response arrives mid-upload; streaming sources
      (ffmpeg, ices) still get the 200 at connect.

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

### Performance baseline (criterion 0.5, `cargo bench --bench engine`, release build)

Machine: dev box (no CPU model recorded); ~5 % run-to-run variance between
sessions, so compare *within one session* (criterion `--save-baseline` /
`--load-baseline`) rather than against these absolute numbers. One 4096-frame
stereo buffer = 92.9 ms of audio at 44.1 kHz. These are the "seems fine"
anchors for later phases.

| benchmark | per 92.9 ms buffer | vs real-time |
|---|---|---|
| mixers/crossfade/passthrough | 1.2 µs | 0.001 % |
| mixers/crossfade/mixing (worst case: always crossfading) | 107 µs | 0.12 % |
| smart_crossfade/passthrough+measuring | 9.3 µs | 0.01 % |
| smart_crossfade/mixing (always crossfading) | 103 µs | 0.11 % |
| mixers/priority/passthrough | 7 µs | 0.008 % |
| mixers/priority/ducking (SetLive per buffer) | 9 µs | 0.01 % |
| effects/compressor+agc+amplify | 604 µs | 0.65 % |
| resampler/sinc16/44k_to_48k | 831 µs | 0.89 % |
| resampler/sinc16/48k_to_44k1 | 795 µs | 0.86 % |
| encode/mp3 (192 kbps) | 501 µs | 0.54 % |
| encode/opus (128 kbps) | 1114 µs | 1.20 % |
| encode/aac (128 kbps) | 269 µs | 0.29 % |

The D5 level-aware rows (recorded in the session that landed `smart_crossfade`,
so compare against the plain rows *within* that session's variance):
`passthrough+measuring` is 9.3 µs vs the plain passthrough's 1.2 µs — the
rolling tail-level accumulation (sum of squares + VecDeque eviction) costs
~8 µs per buffer, ~0.009 % of a core, and the always-mixing smart row (103 µs)
is statistically identical to the plain mixing row (107 µs) because the
measurement pauses while a fade is in progress.

Full path (crossfade + compressor/agc/amplify + resample + encode) ≈ 2.6 ms
per 92.9 ms buffer ≈ 2.8 % of one core. The one-time hotspot —
`CrossfadeMixer`'s mixing loop, two `f64::powf` per sample making the mix
path ~200x the copy path — is fixed: the gain curve is now a 2048-entry
lookup table with linear interpolation (built once at construction), cutting
mixing from 390 µs to 107 µs per buffer (~3.6x). SIMD on the
resampler/effects loops is not justified until these are the bottleneck
(they are far from it: total chain ≈ 3 % of a core).

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
- [x] Feed ReplayGain/R128 tags into the Phase 2 `Agc`/`normalize` gain
      baseline. Only once the envelope follower exists. (Landed — see Done
      section.)

### Part C — output-format & delivery track (parallel with Part B)

Touches `src/output/` and `src/live/`, not `script.rs` source composition —
no contention with Part B.

- [x] **C1 — `output.file` + multi-mount `output.icecast`** (needs A1;
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
- [x] **C4 — Shoutcast v1/v2**: alternate handshakes inside
      `icecast_client.rs`, exposed as `protocol = "icecast" | "shoutcast-v1"
      | "shoutcast-v2"` (alias `"shoutcast"` = v2) on `output.icecast`'s
      config table. Both versions speak the legacy ICY source protocol
      (password line + `icy-*` headers, LF endings; accepts the bare `OK2`
      reply) — the DNAS v2 accepts ICY sources on both source ports, and the
      native "uvox2" handshake is undocumented/encrypted. v1 is MP3-only;
      v2 adds **AAC** streamed as HE-AAC ("AAC+", fdk-aac AOT 5) with the
      `audio/aacp` content type, and targets named streams by appending
      `:#N` to the password. Opus is rejected for both. Titles ride
      `/admin.cgi?mode=updinfo` with the source password (the ICY mechanism;
      the DNAS re-serves them as in-stream metadata for listeners).
      Verified end-to-end against a real DNAS 2.6.1 — see Done section.

### Part D — scripting-operator parity (new track)

Everything in this section is new scope, identified by comparing crabsoup's
registered Lua operators against the operators real Liquidsoap scripts lean
on most heavily. Ranked by how often you'd actually hit the gap in
production use, not by build effort.

- [x] **D1 — `mksafe`** (cheapest item in the whole plan): wraps any source
      so it never fails outright — when the child exhausts (or a request
      source fails to resolve), an infinite `blank` produces silence instead
      of propagating an error up to the output; the child is re-checked from
      the top on every pull, so a `request.queue` that receives a push later
      preempts the silence again. Pure composition of existing pieces
      (`fallback([src, blank()])`). (Landed — see Done section.)
- [x] **D2 — per-track cue points (`annotate:` prefix + `cue_cut`)**: parse
      `annotate:liq_cue_in="30",liq_cue_out="180":/path/track.mp3` in
      playlist entries, carry cue points as per-track metadata alongside
      `label()`, and add a `cue_cut` wrapper that skips ahead to `cue_in` and
      treats `cue_out` as early exhaustion. Plus the per-track
      `(fade_in, fade_out)` override in `CrossfadeMixer` (a real API change
      — default behavior unchanged when no override is present). (Landed —
      see Done section.)
- [x] **D3 — `add()`**: general N-source sample-wise additive mixing as a
      Lua operator (background bed + voice-over, layered intros); the
      primitive Liquidsoap's own `smart_crossfade` is built on. Optional
      per-source `weights` scale each child before summing (default 1.0,
      mismatched counts rejected). (Landed — see Done section.)
- [x] **D4 — `request.dynamic`**: needs its own prefetch design; touches the
      same audio-thread/Lua-thread boundary as A2. (Landed — see Done
      section.)
- [x] **D5 — level-aware smart crossfade**: builds on D2; do last.
      (Landed — see Done section.)

### Part E — external process pipeline (`pipe()`)

**Status: shipped — see Done (cont.) for the landing note.**

Runs outboard broadcast processors (Thimeo Stereo Tool being the concrete
case) as a pipeline stage, generic like Liquidsoap's own
`pipe(process=..., input)`: closed-source processors have no safe C ABI to
bind (see the LADSPA non-goal), so a subprocess is the isolation boundary —
a crash kills the processor, not crabsoup, and no new `unsafe` exists here.
Liquidsoap users already run Stereo Tool exactly this way in production
(`pipe(process='/stereo_tool_cmd_64 - - -s /mySettings.sts -q -k
"<LICENSE>"', input)` — stdin/stdout as `-`/`-`, no named pipes).

Design:

- Two bridge threads per instance: a writer pulls the child source and
  feeds the subprocess's stdin as raw little-endian PCM (s16le default,
  s24le optional; the clamp/convert math matches the LAME path in
  `encoder.rs`); a reader drains stdout, decodes complete frames back to
  f32, and pushes into a bounded queue. Not lock-step — outboard
  processors have real look-ahead latency, so the queue decouples the two
  streams; backpressure runs end-to-end (queue full -> reader blocks ->
  the process blocks -> the writer blocks), so the child source advances
  at the consumption rate and nothing buffers unboundedly.
- `AudioSource` on top: `next_buffer` accumulates queue chunks into the
  caller's buffer (short poll; zero = silence while the process catches
  up), drains queued audio to a clean end when the child exhausts, and
  falls back to **bypass** (raw child audio, unprocessed) the moment the
  process dies — restarting with a fixed backoff (`restart_backoff`,
  default 500 ms, the Icecast reconnect philosophy), never blocking the
  pull loop on a respawn. `mksafe(pipe(...))` is the documented
  deployment shape.
- Lua: `pipe({process = "...", format = "s16le"|"s24le",
  restart_backoff = 500}, src)`. The wrapped child is shared (`Arc`) with
  the bridge threads — unlike other operators, `pipe` does not consume the
  source.

Real caveats (documented, not blockers): the processor is an unbundled,
separately-licensed binary the operator installs themselves; a
`-k "<LICENSE>"` argument is visible in `ps aux`; per-output processing
means N subprocesses — run one `pipe()` on the shared root unless
per-output processing is a real requirement.

Acceptance: a script piping a source through a real external command (the
test suite uses `cat` and `head -c N` — dependency-free passthroughs; the
Stereo-Tool-specific live check needs a licensed binary, so it is manual)
produces correctly-shaped output; killing the subprocess mid-stream
triggers bypass rather than a hang or panic.

### Part F — real-deployment gaps

**Status: F1, F2, F4, F3 all shipped — see Done (cont.).** From auditing the
shipped code against what real deployments hit (not a fixed Liquidsoap
checklist): HTTPS and soundcard I/O were the gaps most likely to actually
block someone in production; `map_metadata` and `blank.detect` close the
metadata/dead-air gaps.

- [x] **F1 — HTTPS support**: `rustls` (ring provider, pure Rust) wraps the
      hand-rolled HTTP client's `TcpStream` — a wrap-the-transport change:
      the status/header/body parsing never knows which transport carried
      the bytes. `http://` keeps a plain socket; `https://` completes the
      TLS handshake and feeds the same byte stream through. Redirects that
      cross scheme (`https` -> `http` and back) re-open the transport per
      hop (`HttpUrl` now carries its `Scheme`, default ports 80/443, and
      `join()` preserves the scheme). Trust store is the webpki-roots
      Mozilla set, built once; tests inject a self-signed `rcgen` cert as
      the root store against a local `rustls` server, so the TLS fetch test
      needs no live internet. The old
      `https_is_rejected_with_a_clear_message` test is replaced by
      `https_downloads_a_body_from_a_local_tls_server` (Content-Length
      body over TLS) and `https_redirect_to_http_swaps_the_transport`
      (302 from the TLS server to a plain server). `single`/`playlist`/
      `queue.push`/`request.dynamic` all accept `https://` now.
- [x] **F2 — soundcard I/O**: `cpal` for both directions. `input.soundcard`
      captures via the harbor-style bridge: cpal's realtime callback
      (never blocking or allocating — stack-chunk conversion, `push_slice`
      into the ring) feeds an SPSC ring, and the `AudioSource` half drains,
      converts channels, and resamples to the bus spec (exact passthrough
      when rates match). `output.soundcard` mirrors it in reverse: a tap
      consumer resamples/converts and pushes into a ring drained by the
      device callback (silence on underrun). `cpal::Stream` is `!Send` on
      ALSA, so both sides keep the stream on a parked driver thread
      (`std::thread::park`, woken on drop) and only the ring halves cross
      threads. Device open is a synchronous bounded handshake — a missing
      device fails at script evaluation (`input.soundcard`) or at startup
      (`output.soundcard` connect, like `output.file`). Hardware tests
      don't exist in CI; the bridge math is unit-tested with a synthetic
      producer, and the manual verification steps live in
      `docs/ARCHITECTURE.md`.
- [x] **F4 — `blank.detect`**: dead-air detection reusing the DSP
      envelope/RMS shape. Per-buffer RMS vs `threshold_db` (default -40);
      sub-threshold for `duration` seconds (default 2) -> blank state:
      silence, one `on_blank` event per episode (a new `ScriptEvent::Blank`
      over the A2 bridge), and (default `exhaust_while_blank = true`)
      `is_exhausted() == true` so a `fallback` composed around it hands
      over automatically. After `restart` seconds the child is re-checked;
      audio above the threshold brings the source back (and clears the
      exhausted flag). `blank` is now a callable table:
      `blank({duration})` and `blank.detect(src, {threshold, duration,
      restart, exhaust_while_blank, on_blank})`. Exact-sample unit tests
      (detection timing, recovery, fire-once-per-episode,
      exhaust-while-blank toggle) plus script tests (fallback handover +
      Lua counter, no false positive on healthy audio).
- [x] **F3 — `map_metadata`**: metadata rewrite hook. Wraps the source;
      when the child's label changes (even to none, so scripts can add
      titles), the Lua callback runs on the A2 event loop with
      `{ title = ... }` and returns a table whose `title` replaces the
      label everywhere downstream (Icecast announce, status). Unlike
      `on_metadata` the reply must reach the output, so
      `MapMetadataSource` polls it for a bounded pull budget (~0.7 s,
      never blocking the audio thread) and falls back to the original
      label on timeout, callback error, or `nil`. `map_metadata(src,
      function(m) return {title = ...} end)` registered next to
      `on_metadata`. Script tests: rewrite in order across a sequence,
      `nil` keeps the original, callback error keeps the original, and the
      bounded wait expires to the raw label when Lua never replies.

### Suggested execution order

1. Buffer-reuse pass in `CrossfadeMixer`/`PriorityMixer` (perf, low risk, no
   API change) — **done** alongside Phase 1.
2. Phase 1 (primitives) — **done** (see Done section).
3. Part A (A1 + A2 together — they share the "who owns the pull loop"
   decision, easier to land as one architectural PR than two) — **done**
   (see Done section).
4. Track B, sequential: Phase 2 (DSP) — **done**; next Phase 3 → Phase 5 →
   Phase 6 (needs A2) → Phase 7 → Phase 8 (stretch).
5. Track C once A1 lands: C1 → C2 → C3 (needs C2) — all **done** (see
   Done section); C4 (Shoutcast v1/v2) — **done** (see Done section).
6. Part D — **complete**: D1 (`mksafe`), D3 (`add()`), D2
   (`annotate:`/`cue_cut` + per-track fades), D4 (`request.dynamic`),
   D5 (level-aware smart crossfade) — all **done** (see Done section).
7. Part E (`pipe()` external-process operator — needs no A1/A2; the
   audio-path resilience design is the real work, not the plumbing) —
   **done** (see Done section).
8. Part F (real-deployment gaps) — **done**: F1 (HTTPS via rustls) → F2
   (soundcard I/O via cpal) → F4 (`blank.detect`) → F3 (`map_metadata`),
   per the plan's suggested order (see Done (cont.)).

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
- [x] Structured (JSON) control-port replies: `src/control.rs` — dispatch
      now produces a structured `CommandReply` (success / error / custom /
      status / uptime / queued / list / playing), rendered to either the
      human-readable telnet protocol (byte-identical to before) or a single
      line of JSON. Any command prefixed with `json ` (e.g. `json status`,
      `json queue.list`) replies with one JSON object per line:
      `{"ok": true, ...}` on success, `{"ok": false, "error": "..."}` on
      failure, custom Lua replies wrapped as `{"ok": true, "reply":
      "..."}`. Escaping is serde_json's (new `serde_json` dep), so
      arbitrary track titles / paths / Lua text are always well-formed;
      the `json` name is reserved (cannot be a `server.register` command),
      and a bare `json` replies with a usage error. This is the machine-
      readable contract a web backend can parse without regex-scraping the
      prose replies. Inline tests (204 -> 216): text replies unchanged,
      JSON single-line + parseable with the expected fields, structured
      round-trips (`queue`, `playing`/`uptime_seconds`, `queued`/`length`),
      quote/newline/backslash escaping in hostile titles, and
      `split_json_prefix` (whitespace boundary so `jsonify` is untouched).
      README + website control-port guide updated.
- [x] Control HTTP endpoint + `banner` flag: `src/control.rs` — a minimal
      HTTP/1.1 status/control server (`ControlHttpServer`) on the same
      host as the telnet port, enabled with `server.telnet({http_port =
      N})`: `GET /status` / `/uptime` / `/queue` / `/jingles` and
      `POST /cmd` with `{"command": "..."}` (any control command). Every
      response reuses the JSON envelope (200 on `{"ok": true}`, 400 on
      `{"ok": false}`, 404 unknown route, 405 wrong method); framing is
      hand-rolled on tokio like the harbor (header cap 16 KiB, body cap 64
      KiB, `Content-Length`, no keep-alive). `server.telnet({banner =
      false})` skips the text welcome line so machine clients get replies
      from byte zero. Script config parsing + main.rs wiring; inline tests
      (216 -> 219): request-line parsing, case-insensitive Content-Length,
      and `http_route` (GETs, POST success/error, malformed bodies, 404/
      405, `exit` ack). `examples/control_api.py` is a worked
       Crabcast-style backend (stdlib only) consuming both transports;
       README + website control-port guide updated.
- [x] C7 (outbound `http_post` webhook): `src/script.rs` — a global
      `http_post(url, payload_table)` operator that POSTs the payload as
      JSON to a fixed backend URL (Crabcast track-change events), fired
      from an `on_metadata` callback. Fire-and-forget: the call spawns a
      thread so the Lua event loop never blocks on the network; the payload
      is a Lua table converted to JSON (`table_to_json`/`value_to_json` —
      string/integer keys, string/number/boolean/table values, array-shaped
      tables become JSON arrays; unsupported value types error), nested
      tables included. `src/request.rs` — `http_post_json`: one-shot `POST`
      of the JSON body reusing the `http_get` transport (http/https),
      no redirects followed (a webhook target is a fixed URL), response
      body discarded; non-2xx surfaces as an error. Failures log
      (`http_post to {url} failed: ...`) rather than erroring the script.
      Inline tests (219 -> 222): flat + nested + array table conversion,
      unsupported-value rejection, POST body + 2xx accepted, non-2xx
      reported. Signature fix: `on_metadata`/`on_track` take `(source,
      callback)` — README + website updated.
- [x] C8 (harbor per-streamer passwords + on-air status): `input.harbor`
      gains `extra_passwords = {"dj2", ...}` — per-streamer (DJ) source
      passwords that all authenticate on the shared mount alongside the
      main `password` (Basic auth matches the mount password or any extra).
      The harbor's occupied flag is now a shared `Arc<AtomicBool>` owned by
      the status handle, so the control port can report live-DJ state:
      telnet `status` adds a `live: true|false` line and `json status`
      adds `"harbor_connected": true|false` for a control consumer to show
      when a DJ is on air. Inline tests (222 -> 224): extra passwords
      accepted for auth alongside the mount password. README + website
      control-port guide updated.
- [x] Part F3 (`map_metadata`): `src/script.rs` — `MapMetadataSource` wraps
      a child and rewrites its label through a Lua callback (Liquidsoap
      `map_metadata`). On a label change (even to none, so scripts can add
      titles) it sends `ScriptEvent::MapMetadata { hook_id, title, reply }`
      over the A2 bridge; the Lua-owning event loop calls the callback with
      `{ title = ... }` and replies with the returned table's `title`, or
      `None` (nil / error / missing field) to keep the original. Unlike
      fire-and-forget `on_metadata`, the rewrite has to *reach the output*,
      so the source polls the reply for a bounded pull budget
      (`MAP_METADATA_PULL_BUDGET` = 8 pulls, ~0.7 s — never blocks the
      audio thread, well past the event loop's 100 ms poll) and falls back
      to the raw label when the budget expires or the callback errors.
      `map_metadata(src, function(m) return {title = ...} end)` registered
      next to `on_metadata`; README + ARCHITECTURE + parity map updated.
      Inline tests (203 -> 207): rewrite in order across a 0.2 s sine
      sequence (reply lands between pulls), `nil` keeps the original,
      callback error keeps the original, and the bounded wait expires to
      the raw label when the event loop never replies (audio keeps
      flowing throughout).
- [x] Part F4 (`blank.detect`): `src/source/blank_detect.rs` —
      `BlankDetectSource`, a silence/ded-air guard (Liquidsoap
      `blank.detect`). Per-buffer RMS in dBFS (`buffer_rms_db`, the DSP
      effects' envelope shape); sub-threshold for `duration_secs` (default
      2 s) puts the source into a blank state: it emits silence (returns 0)
      and, by default, reports `is_exhausted() == true` so a `fallback`
      composed around it hands over automatically — the zero-configuration
      dead-air guard. After `restart_secs` (default 1 s) the child is
      re-checked on every pull; audio above the threshold recovers the
      source (and clears the exhausted flag). An optional `on_blank`
      closure fires once per episode; script.rs wires it to a new
      `ScriptEvent::Blank { hook_id }` (new `blank_hooks` vec) so a script
      can log/alert/skip. `blank` is now a callable table (`__call`
      metamethod) carrying `blank.detect`; Lua gotcha: mlua maps a
      *missing* `exhaust_while_blank` field to `false` for a plain `bool`
      target (nil is falsy), so the option is read as `Option<bool>` with
      a true default. Inline tests (207 -> 218): unit tests for detection
      timing (loud never triggers; 0.2 s of silence goes blank and
      exhausts), recovery after the restart window, fire-once-per-episode,
      and `exhaust_while_blank = false` reporting the child state; script
      tests for fallback handover + Lua counter (a `tone-then-silence`
      source trips the detector and the fallback switches to the backup
      child) and no false positive on healthy audio.
- [x] Part F2 (soundcard I/O): `src/source/soundcard.rs` +
      `src/output/soundcard.rs` — `cpal` for both directions, using the
      harbor-style ring bridge. `input.soundcard({device = nil})` opens the
      device at script evaluation (synchronous bounded handshake — a
      missing/broken device fails with a clear error); cpal's realtime
      callback converts in stack chunks and `push_slice`s into an SPSC
      ring (never blocks/allocates), and the `AudioSource` half drains,
      converts channels, and resamples to the bus spec on the pull thread
      (exact passthrough when rates match). `output.soundcard({device =
      nil}, src)` is a tap consumer (claims the root like `output.file`):
      the consumer thread resamples/converts (reusable scratch) and pushes
      into a ring the device callback drains (silence on underrun); the
      device + stream open at `connect()` in main so a missing device
      fails fast at startup. `cpal::Stream` is `!Send` on ALSA, so both
      sides park a driver thread owning the stream (`std::thread::park`,
      unparked on drop) and only the ring halves cross threads. Inline
      tests (204 -> 207): the ring+resampler bridge with a synthetic
      producer (same-rate passthrough, 22.05k mono -> 44.1k stereo bus
      upsample, drop-oldest stale-window cap), the channel-conversion
      helper, and script registration (output.soundcard registers without
      a device; input.soundcard opens or fails gracefully). Manual
      verification steps (loopback round-trip) documented in
      `docs/ARCHITECTURE.md`; README + parity map updated.
- [x] Part F1 (HTTPS): `src/request.rs` — the hand-rolled HTTP client now
      speaks `https://` by wrapping its `TcpStream` in a `rustls` client
      (ring provider; webpki-roots Mozilla trust store built once in a
      `OnceLock`). `Transport` (plain `TcpStream` | `rustls::StreamOwned`)
      implements `Read + Write`, so the status/header/chunked/redirect
      parsing is byte-identical over both transports; `HttpUrl` carries a
      `Scheme` (default ports 80/443), and each redirect hop re-opens the
      transport so scheme-crossing `Location`s (`https` -> `http` and
      back) work. `download` takes an optional root-store override (tests
      inject a self-signed `rcgen` cert against a local `rustls` server —
      no live internet needed). The old
      `https_is_rejected_with_a_clear_message` test is replaced by
      `https_downloads_a_body_from_a_local_tls_server` and
      `https_redirect_to_http_swaps_the_transport`; `request_uri_classifies`
      now covers `https://` too. `single("https://...")`, `playlist`
      entries, `queue.push`, and `request.dynamic` all resolve HTTPS now.
      Cargo.toml: `rustls` (no default features, ring+std+tls12),
      `webpki-roots`, dev-dep `rcgen`. README + ARCHITECTURE + parity map
      updated.
- [x] Part E (`pipe`): `src/source/pipe.rs` — `PipeSource`, a source that
      runs an external raw-PCM processor (Liquidsoap `pipe(process=...,
      input)`) as a pipeline stage. A writer thread pulls the child
      (shared `Arc<Mutex<Box<dyn AudioSource>>>` — not consumed, bypass
      needs it too), encodes f32 -> s16le (default) or s24le (the same
      clamp formula as the LAME path in `encoder.rs`), and writes the
      subprocess's stdin (`sh -c`, stderr inherited so processor errors
      are visible); a reader thread decodes stdout back to f32 in complete
      frames and pushes into a bounded 8-chunk `sync_channel`, giving
      end-to-end backpressure (queue full -> reader blocks -> the process
      blocks -> the writer blocks) so the child advances at the
      consumption rate and buffering stays bounded. `next_buffer` pulls
      the queue (50 ms poll; zero = silence while the process catches up,
      so a stalled processor never stalls the engine), drains queued audio
      on graceful child exhaustion (`Draining` -> `Ended`, with a
      1-second stall cap for processes that never close stdout), and on
      process death switches to **bypass** — pulls the child directly —
      while the supervisor restarts the process with a fixed
      `restart_backoff` (default 500 ms, Icecast-reconnect style, retries
      forever; the backoff sleep is interruptible on drop). Drop kills the
      child so a torn engine never orphans a processor. `pipe({process,
      format, restart_backoff}, src)` registered in `script.rs`;
      README + ARCHITECTURE + parity map + `examples/crabsoup.pipe.lua`
      updated. Inline tests (177 -> 186): s16le and s24le `cat`
      passthroughs reproduce an independent reference sine within
      quantization error (the reference is aligned by one buffer for the
      startup chunk — the child advances 128 frames per pull); `head -c N`
      death with a long backoff falls back to bypass and keeps playing the
      raw child; a fast-dying process with a 10 ms backoff restarts
      repeatedly without hanging or silencing; a 0.2 s finite child drains
      cleanly (only the sub-buffer tail is dropped) and then exhausts; a
      broken command bypasses immediately; an empty `process` is rejected
      at spawn; script-level `pipe` + `mksafe` composition plays a real
      source and stays alive.      (Caught during bring-up: a `try_send` of a
      `mem::take`d chunk dropped one buffer of audio every time the queue
      was full — the value is now restored on `Full`; and the reference
      alignment bug above. Review hardening: the dead process is reaped
      (`try_wait`, bounded) on every respawn so restarts never accumulate
      zombies, and `pipe` consumes its source like every other operator
      (wrapped in a fresh `Arc` for the bridge threads). Caveat: at
      end-of-stream the drain drops the sub-buffer tail and gives up after
      1 s of silence, so a processor with more than ~1.7 s of internal
      latency loses its tail — acceptable for v1.)
- [x] Harbor → mixer handoff is now a lock-free SPSC ring (`ringbuf`
      0.4.8): `src/live/source.rs` — `LiveSource` holds a `HeapCons<f32>`
      and pops with `pop_slice`, enforcing the drop-oldest cap on pull
      (`skip` anything older than `MAX_LIVE_FRAMES`, so the played window
      stays the most recent 5 s, same lag as the old
      `Arc<Mutex<VecDeque>>` drain-on-push); `LiveSink` (new) holds the
      `HeapProd<f32>` half and pushes with `push_slice`, and **applies
      backpressure when the ring is full** (waits for the consumer to
      drain) so the newest audio is never silently dropped — a fast
      `curl -T` upload throttles to real time and plays completely, like
      the old code, instead of losing the middle of the file. `src/live/
      harbor.rs` — `handle_connection` splits a `HeapRb` sized at
      `2 * MAX_LIVE_FRAMES` (headroom absorbs fast uploads / brief
      consumer stalls without ever blocking the decode thread); both
      decode paths (`decode_live_stream_inner`, `decode_opus_live`) push
      through the sink; the now-dead `exhausted` plumbing was removed from
      the inner decoders. The swap was **measurement-gated per the plan**: a
      `live_handoff` comparison bench (same high-rate producer workload —
      8 chunk-pushes + 1 buffer-pull per iteration — against both
      implementations) showed 43.8 µs (`mutex_vecdeque`) vs 3.2 µs
      (`spsc_ring`) per iteration, a **~13.6x win**, so the dependency was
      justified and added. Inline tests (175 -> 176): a concurrent
      producer/consumer test proving the newest sample survives a full
      ring (the non-blocking variant would lose it) and the played window
      stays in order, consumer-side drop-oldest semantics (push 6, cap 4
      -> pull returns the newest window `[3,4,5,6]`), and the
      frames-then-silence-then-exhausted sequence. `ringbuf = "0.4"`
      moved to `[dependencies]`.
- [x] Opus demux keeps every packet: `OggOpusDemux::pending` was a single
      `Option<Vec<u8>>` overwritten on each completed packet, so real-world
      Ogg files that pack many packets per page (ffmpeg: ~50 per page)
      silently dropped all but the last packet of every page — an 8 s
      ffmpeg Opus file decoded to ~0.17 s, and a fast `curl -T` Opus DJ
      upload aired ~0.17 s before "end of stream". `pending` is now a
      `VecDeque` FIFO (`push_back` on completion, `pop_front` on
      `next_packet`; pending is drained before the EOS check, so packets
      from the EOS page still come out). Caught live during the ring-swap
      verification: the drain-wait held only 0 s because the ring was
      already empty — the decode itself had produced 15,304 of ~706,000
      samples. Inline test (176 -> 177): one page carrying three complete
      packets must yield all three in order (fails on the old code — it
      returned only the last). The drain-wait in `decode_live_stream`
      (up to `DRAIN_WAIT_SECS` = 15 s, named const) delays `ClearLive`
      until the consumer drains the ring, so a fast upload's buffered
      tail plays out; verified live: 706,792 samples decoded, "LIVE DJ"
      on status for the full 8 s tone, 660 Hz band -0.3 dB max in the
      recording's DJ window vs -23 dB control.
- [x] D5 (level-aware smart crossfade): `src/engine/mixer.rs` —
      `SmartFade { fade_out, fade_mid, threshold_db }` and
      `CrossfadeMixer::with_smart_fade` (builder; `None` = plain
      crossfade). A rolling window keeps the running sum of squares of the
      active track's last `fade_out` seconds (chunked `VecDeque` so the
      window evicts per buffer, no reallocation); `tail_level_db` turns it
      into an RMS dBFS reading, and at preload `smart_window()` picks the
      full `fade_out` for a loud tail or the short `fade_mid` for a quiet
      one (below `threshold_db`, default -30). The preload margin stays at
      `fade_out` so a quiet-tail fade simply completes early (no audible
      gap) while a loud tail gets the full overlap. Per-track `fade_in`/
      `fade_out` overrides (D2) still win over the smart window. `src/
      script.rs` — `playlist_requests` helper extracted (request collection
      for `directory`/`files`, sorted/deduped) and shared by `playlist`
      and the new `smart_crossfade(opts)` operator (`fade_out` defaults to
      the global `crossfade_seconds`, `fade_mid` to half of it,
      `threshold` in dBFS) which builds a level-aware crossfading playlist.
      README + ARCHITECTURE updated. Inline tests (172 -> 175): a loud
      (0 dBFS) tail gets the exact 40-frame `fade_out` ramp (same sample
      values as the explicit override case), a quiet (-40 dBFS) tail
      collapses to the 10-frame `fade_mid` window (complete a full buffer
      earlier), and a script-level `smart_crossfade({directory = "./media"})`
      plays a real directory with audio (skips when `media/` absent).
- [x] D4 (`request.dynamic`): `src/script.rs` — `DynamicRequestSource`
      plays requests returned by a Lua callback, one ahead of the current
      track. The callback runs on the Lua-owning event loop through the A2
      bridge (`ScriptEvent::NextRequest { index, reply }`, a fresh
      `mpsc::Sender<Option<String>>` per call; `dynamic_hooks` vec on
      `ScriptState`/`ScriptRuntime`); it returns the next request URI as a
      string or nil to end the source. The audio thread never blocks on the
      reply — it polls `try_recv` and returns silence (or the current
      track's audio) until the answer lands — and the next URI is requested
      as soon as a track is promoted, so a fast callback makes handovers
      gapless. Requests resolve through the normal `resolve()` path
      (`annotate:` prefixes, `http://` download-then-play, retries); a
      request that fails to resolve is logged and skipped and the callback
      is asked again (one silent pull per failure, never a spin). A dead
      event loop sets `no_more` so the source ends instead of stalling.
      `skip` advances to the next prefetched track. Registered as
      `request.dynamic(fn)` on the `request` table next to `queue`;
      README + ARCHITECTURE updated. Inline tests (170 -> 172): a Lua
      callback returning two unresolvable paths then nil drives the source
      to exhaustion with exactly three invocations (verified via a Lua
      counter through the real event loop), and a generated 0.3 s Opus
      file plays end-to-end (26k+ samples) before nil ends the source.
- [x] D2 step 2 (per-track crossfade override): `AudioSource::
      crossfade_overrides() -> Option<(Option<f64>, Option<f64>)>`
      (default `None`) added to `src/source.rs` and forwarded by every
      wrapper (`EffectSource`, `FallbackSource`/`SequenceSource`/
      `RandomSource`/`ScheduleSource`/`AddSource`, `OnMetadataSource`/
      `OnTrackSource`, `ReplayGainSource`, `RequestQueueSource`,
      `DownloadSource`, `CueCutSource`, `CrossfadeMixer`/`PriorityMixer`)
      like `replaygain_db`. `CueCutSource` reports its `TrackCues`
      `fade_in`/`fade_out`. `CrossfadeMixer` sizes each transition's overlap
      window per preload: the incoming track's `fade_in`, else the outgoing
      track's `fade_out`, else the global `crossfade_seconds`; the preload
      margin uses the outgoing track's `fade_out` override too, so an
      annotated track starts its fade early enough. No override present ⇒
      behavior and frames are identical to before (global window).
      `apply_cues` now also wraps when only fades are set (no cue points),
      so `annotate:liq_fade_in=...` alone reaches the mixer. Inline tests:
      mixer `FakeSource`/`with_fades` — outgoing `fade_out=0.4` moves the
      preload from 0.2 s early to 0.4 s early and spans a 40-frame fade
      (exact sample values per frame), incoming `fade_in=0.4` over a plain
      outgoing track extends the window and finishes via the existing tail
      ramp; `CueCutSource` reports/omits overrides; script-level `cue_cut`
      with fades reports `(Some(2), Some(3))`; a real-file
      `single("annotate:liq_fade_in=...,liq_fade_out=...:path")` surfaces
      the overrides. Existing crossfade tests unchanged and green.
- [x] D2 step 1 (`annotate:`/`cue_cut`): `src/request.rs` — `RequestUri`
      variants gain an `Option<TrackCues>` (`TrackCues { cue_in, cue_out,
      fade_in, fade_out }`, `fade_*` parsed now for step 2); `new()` parses
      a Liquidsoap-style `annotate:` prefix (`liq_cue_in`, `liq_cue_out`,
      `liq_fade_in`, `liq_fade_out`; unknown keys ignored; malformed
      prefixes fall back to a plain URI with no cues). `TrackCues` holds
      `f64`s so `Eq`/`Ord` are manual (`cmp_cues`/`cmp_opt` order `None`
      before `Some`, `total_cmp` for the values). `src/source/cue_cut.rs`
      — `CueCutSource` skips `cue_in` seconds at each track start (silence)
      and treats `cue_out` as early exhaustion, so the parent (crossfade
      mixer or fallback) advances at the cue point instead of the file's
      natural end; the window is re-applied at every label change, so a
      `cue_cut` around a playlist trims every track. `resolve()` wraps in
      `CueCutSource` automatically when a request carries cues, so
      playlists/`single`/`queue.push` all honor `annotate:` without extra
      wiring. `cue_cut(src, {cue_in, cue_out, fade_in, fade_out})`
      registered in `script.rs` for direct use. Inline tests: annotate
      parsing (cue extraction, `http://` URIs + unknown-key ignoring,
      malformed fallback), `CueCutSource` trim math (skip lands on the
      right sine phase, `cue_out` ends the track at exactly the window,
      passthrough with no cues, `remaining_seconds` tracks the window),
      and script-level `cue_cut` on a sine + a real-file
      `single("annotate:...:path")` that skips when `media/` is absent.
- [x] D3 (`add`): `src/script.rs` — `AddSource` sums N children
      sample-by-sample (each child's `next_buffer` output is added into the
      shared output buffer; the first child writes directly, the rest pull
      into a reusable scratch so `next_buffer` never allocates). Optional
      `weights` per child (default 1.0) scale before summing; the sum is not
      normalized — clipping is the caller's concern, as in Liquidsoap.
      Exhausts only when every child exhausts, so a looping bed keeps a
      finite voice-over mix alive; `label`/`remaining_seconds`/
      `replaygain_db` come from the first child, `skip` forwards to all.
      `add({a, b, ...}, {weights = {...}})` registered in `script.rs`;
      README + ARCHITECTURE operator lists updated. Inline tests (148 ->
      154): sample-wise sum of two in-phase sines peaks ~1.0, per-source
      weights scale the peak (~0.75), a short child over an infinite child
      keeps playing and never exhausts, exhaustion when every child ends
      (exact sample count), and bad weight counts / empty lists are
      rejected.
- [x] D1 (`mksafe`): `src/script.rs` — pure composition of existing pieces:
      `fallback([src, blank()])` as one `lua.create_function`, so a wrapped
      source never fails outright. When the child exhausts (e.g. a request
      queue that failed to resolve its request and drained), an infinite
      `blank` produces silence instead of the engine erroring out; because
      `FallbackSource` re-checks children from the top on every pull, a
      `request.queue` that receives a push later preempts the silence again.
      Registered in `script.rs`; README parity map + operator list updated.
      Inline tests (146 -> 148): a short `sine` wrapped in `mksafe` keeps
      producing full silence buffers and never exhausts; a `request.queue`
      holding an unresolvable path yields silence rather than an error.
- [x] Crossfade gain curve de-powfed: `src/engine/mixer.rs` — the two
      `f64::powf` calls per sample in the mixing path are replaced by a
      2048-entry `t^fade_curve` lookup table with linear interpolation,
      built once at construction (`curve_gain`); the tail ramp uses it too.
      Bench (same machine, `mixers/crossfade/mixing`): 390 µs -> 107 µs per
      92.9 ms buffer (~3.6x, criterion reports ~72 % faster with p < 0.05).
      Baseline table updated above. (The passthrough row also moved, 2 µs ->
      1.2 µs, but that path never touches the curve — treat that delta as
      session variance per the caveat above, not a change from this work.)
- [x] Performance baseline harness: `benches/engine.rs` (criterion 0.5,
      `harness = false`) covering the mixers (passthrough + worst-case
      crossfade mixing + priority passthrough + ducking), the DSP chain
      (compressor + agc + amplify), both resampler directions, and the three
      encoders, on 4096-frame stereo buffers. Baselines recorded above under
      "Performance baseline". First numbers: the full mix+effects+resample+
      encode path runs ~3 % of one core per buffer — the crossfade mixing
      loop's two `f64::powf` per sample was the only notable hotspot (mix
      path ~200x the copy path); since fixed by the curve lookup table
      (entry above).
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
- [x] Phase 8 (replaygain / R128 baseline): `AudioSource::replaygain_db()`
      trait method (default `None`) added to `src/source.rs` and forwarded
      by every wrapper (`CrossfadeMixer`/`PriorityMixer`, `EffectSource`,
      `FallbackSource`/`SequenceSource`/`RandomSource`/`ScheduleSource`/
      `OnMetadataSource`, `DownloadSource`, `RequestQueue`) like `label`.
      `FileSource` parses `REPLAYGAIN_TRACK_GAIN` (fallback
      `REPLAYGAIN_ALBUM_GAIN`) at open: symphonia 0.5 has no ID3v2 reader,
      so MP3s go through a hand-rolled `id3_replaygain` (ID3v2.3
      non-syncsafe + v2.4 syncsafe frame sizes, TXXX frames, encodings
      0/1/2/3 with the UTF-16 `\0` byte dropped so ASCII keys survive,
      Unicode minus U+2212 and `" dB"` suffix normalized) while the
      symphonia path stays as the Ogg/Vorbis-comments fallback.
      `src/source/replaygain.rs` — `ReplayGainSource`, a per-track constant
      gain wrapper: re-reads the child's gain on label change, clamps to
      ±`max_boost`/`max_cut` (default 12 dB each), unity when untagged;
      `replaygain(src, {max_boost, max_cut})` registered in `script.rs`,
      composing `normalize(replaygain(src))` gives AGC the RG baseline.
      Inline tests (133 -> 140): tagged/album-fallback/no-suffix/untagged
      parsing, gain applied + switched per track, clamp, unchanged within a
      track, and Lua composition with an untagged track passing unity.
      Verified live: a playlist of an untagged track + a track tagged
      `REPLAYGAIN_TRACK_GAIN "-6.5 dB"` logged `replaygain: track gain -6.5
      dB (applying -6.5 dB)` exactly at the tagged track's boundary and
      stayed silent for the untagged one.
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
- [x] C4 (SHOUTcast v1/v2): `OutputProtocol` on the icecast output config
      (`protocol = "icecast" | "shoutcast-v1" | "shoutcast-v2"`, with
      `"shoutcast"` aliasing v2; parsed in script.rs, defaulting to
      Icecast). `icecast_client.rs` gained the legacy ICY handshake for
      both versions: the password as the first line plus
      `icy-name`/`icy-pub`/`icy-genre`/`icy-br`/`icy-sr` headers with LF
      endings, accepting either a bare `OK2` reply (bounded by a short
      head-read timeout) or an HTTP-style head. The native v2 "uvox2"
      handshake is undocumented and encrypted, so ICY on `portbase` (or
      `portbase + 1` for legacy v1 sources) is the interoperable path — the
      real DNAS 2.6.1 rejects `SOURCE`/`POST`/`PUT` outright ("Invalid
      HTTP request"). v2 targets named streams by appending `:#N` to the
      password, the DNAS's documented v2.4.7+ mechanism for ICY sources.
      Formats: v1 is MP3-only; v2 also takes AAC, encoded as HE-AAC
      (fdk-aac AOT 5 via `AacEncoder::new_he_aac`, SBR verified: ADTS
      signals the 22050 Hz core and the stream decodes with ffmpeg) and
      announced as `audio/aacp`; Opus is rejected for both. Titles ride
      `/admin.cgi?mode=updinfo&pass=<pw>&song=<title>` GETs with the source
      password — the ICY-source mechanism (the DNAS re-serves them as
      in-stream metadata to listeners) — routed from the pump loop instead
      of the Icecast `/admin/metadata` GET. Verified with fake-server tests
      (v1/v2 ICY handshake requests, bare `OK2`, `:#N` stream selection,
      format enforcement incl. AAC-only-on-v2, admin.cgi title request, and
      the HE-AAC SBR/decode test). Verified end-to-end against a real DNAS
      2.6.1: v1 MP3 on 8001 and v2 MP3 on 8000 both connect, the DNAS
      detects "MPEG v1 layer 3 stereo", `SONGTITLE` updates stick, and
      listeners decode the stream cleanly with `icy-metaint` metadata.
      AAC also connects and the DNAS sniffs it correctly ("AACv4, LC"), but
      DNAS 2.6.1 corrupts the AAC relay to listeners (its legacy-path frame
      parser rewrites ADTS headers), so MP3 is the reliable SHOUTcast
      format until the native uvox2 protocol is supported.
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
