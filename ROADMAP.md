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

### Part A — architectural prerequisites (land A1 + A2 as one PR)

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
must hand back the `Lua` instance, which now outlives script evaluation —
**a real shape change; call it out in AGENTS.md when it lands.** Event rate
is metadata-rate (per track), not per buffer; audio-rate callbacks need
their own budget/backpressure story.

### Part B — feature phases

#### Phase 3 — request queue
- [ ] FIFO source pushed at runtime via telnet `queue.push <path>`, plays
      when non-empty, exhausts when empty (composes in `fallback` before the
      playlist, like `request.queue`); `queue.list`, `queue.clear`; `skip`
      wired to the current track (playlist skip).
- [ ] `server.register` (Lua API for custom telnet commands) — natural once
      A2's event-loop pattern exists; until then, custom commands that do not
      call back into Lua only.

#### Phase 4 — multi-output + file recording (needs A1)
- [ ] `output.icecast` callable more than once (different mounts/formats).
- [ ] `output.file({path, format}, source)`: same consumer shape as
      `IcecastOutput` with a file sink; verified by ffprobe-decoding the
      recorded file.

#### Phase 5 — scheduling (dayparting)
- [ ] `switch` source with time-based brackets (liq `switch` semantics:
      weekday/hour ranges, default child).
- [ ] `rotate` source (sequential/even rotation over children).

#### Phase 6 — metadata hooks (needs A2)
- [ ] `on_metadata(callback, source)`: A2 event loop, metadata table
      (title, duration, path) per track start.
- [ ] `on_track(callback, source)`: second `ScriptEvent` variant, fires on
      track boundary without full metadata.

#### Phase 7 — request protocols (`http://` resolution, biggest lift)
- [ ] `request` abstraction resolving URIs through pluggable protocols.
      Scope down to download-then-play first (retry/timeout, temp-file
      lifecycle) before attempting a streaming-decode path.

#### Phase 8 — loudness (replaygain / R128, stretch)
- [ ] Feed ReplayGain/R128 tags into the Phase 2 `Agc`/`normalize` gain
      baseline. Only once the envelope follower exists.

### Part C — output-format & delivery track (parallel with Part B)

Touches `src/output/` and `src/live/`, not `script.rs` source composition —
no contention with Part B.

- [ ] **C1 — `output.file` + multi-mount `output.icecast`** (needs A1; same
      work as Phase 4, no separate effort).
- [ ] **C2 — AAC encoder** (no dependency, can start immediately): new
      `Encoder` impl, FFI to `fdk-aac` following the LAME `unsafe extern
      "C"` template (opaque handle, explicit `Drop`, `unsafe impl Send` with
      justification). Raw ADTS framing over the existing `IcecastClient`;
      `audio/aac` content-type branch next to the MP3/Opus branches. Low
      risk, runnable alongside Phase 2.
- [ ] **C3 — HLS output** (needs C2): new module — segmenter rotating the
      encoder output into fixed-length chunks (4–6s), `.m3u8` media playlist
      writer, segment lifecycle (naming, retention window, cleanup). AAC is
      the practical HLS codec, so sequence after C2. Acceptance: a real HLS
      player (hls.js/VLC/Safari) plays a live segment window.
- [ ] **C4 — Shoutcast v1/v2** (only if a concrete need shows up): alternate
      handshake inside `icecast_client.rs`, exposed as `protocol =
      "icecast" | "shoutcast"` on `output.icecast`'s config table.

### Suggested execution order

1. Buffer-reuse pass in `CrossfadeMixer`/`PriorityMixer` (perf, low risk, no
   API change) — **done** alongside Phase 1.
2. Phase 1 (primitives) — **done** (see Done section).
3. Part A (A1 + A2 together — they share the "who owns the pull loop"
   decision, easier to land as one architectural PR than two) — **next**.
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
