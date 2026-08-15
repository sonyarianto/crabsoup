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

- Registers the Liquidsoap-flavoured Lua stdlib: `playlist`,
  `smart_crossfade`, `single`, `blank` (+ `blank.detect`), `sine`,
  `amplify`, `compress`, `normalize`, `replaygain`, `stretch`, `pitch`,
  `echo`, `reverb`, `eq`, `filter`, `pipe`, `jingles`,
  `fallback`/`sequence`/`random`, `switch`, `rotate`, `mksafe`, `add`,
  `cue_cut`,   `map_metadata`, `request.queue`, `request.dynamic`,
  `input.harbor`, `input.soundcard`, `input.http`, `output.icecast`,
  `output.file`,
  `output.preview`, `output.soundcard`, `output.hls`, `server.telnet`,
  `server.register`, `on_metadata`, `on_track`, `http_post`, `set`, `log`.
- `blank` is a *callable table* (a `__call` metamethod) so it can carry
  `blank.detect`; Lua calls a table's `__call` as `f(self, args...)`, so
  the closure takes a leading `_self` parameter.
- `blank.detect(src, opts)` (`src/source/blank_detect.rs`) watches the
  wrapped source's per-buffer RMS level; `duration` seconds (default 2)
  below `threshold` dBFS (default -40) puts it into a blank state: it
  returns silence and, by default, `is_exhausted() == true` so a
  `fallback` around it hands over automatically (the zero-configuration
  dead-air guard). An optional `on_blank` Lua callback fires once per
  episode via `ScriptEvent::Blank`; after `restart` seconds the child is
  re-checked and audio above the threshold recovers the source. The
  `exhaust_while_blank` option must be read as `Option<bool>` — mlua maps
  a missing `bool` field to `false` (nil is falsy), which would silently
  flip the safe default.
- `map_metadata(src, fn)` (`MapMetadataSource`, same file) rewrites each
  track's title through a Lua callback on the A2 event loop
  (`ScriptEvent::MapMetadata { hook_id, title, reply }`): the callback
  receives `{ title = ... }` and returns a table whose `title` replaces
  the label for every downstream consumer. Unlike fire-and-forget
  `on_metadata`, the reply must reach the output, so the source polls it
  for a bounded pull budget (8 pulls, ~0.7 s) and falls back to the
  original label on timeout, nil, or callback error — the audio thread
  never blocks. Fires even when the child's label is `None`, so scripts
  can add titles to unlabeled tracks. Known limit: a label change while a
  rewrite is still in flight replaces it, so a source whose label changes
  faster than the budget (sub-second tracks, or the `FallbackSource`
  label-jump quirk) mostly falls back to raw labels — fine for real
  minute-long tracks, not for audio-rate metadata.
- `add({a, b}, {weights = {0.5, 1.0}})` sums N children sample-by-sample
  (`AddSource`): the first child writes straight into the output buffer,
  the rest pull into a reusable scratch that is added in (no per-call
  allocation); optional weights scale each child, mismatched counts are
  rejected. Exhausts only when every child exhausts, so a looping bed keeps
  a finite voice-over mix alive; label/remaining/replaygain come from the
  first child, `skip` forwards to all children.
- `pipe({process, format, restart_backoff}, src)` runs an external
  raw-PCM processor (Stereo Tool etc.) as a pipeline stage — see
  `src/source/pipe.rs`. A writer thread pulls the child source and feeds
  the subprocess's stdin (`sh -c`, stderr inherited) as little-endian
  PCM; a reader thread decodes stdout back into a bounded 8-chunk queue.
  Backpressure runs end-to-end (queue full -> reader blocks -> the
  process blocks -> the writer blocks), so the child advances at the
  consumption rate. The audio side (`PipeSource`) accumulates queue
  chunks into the consumer's buffer; on process death the reader flips
  the pipe to **bypass** (raw child audio) while the supervisor restarts
  the process with a fixed `restart_backoff` (Icecast reconnect-style,
  forever), never blocking the pull loop. A child-source exhaustion
  drains the queue (`Draining`) before ending. Unlike other operators,
  `pipe` does not consume its child (`Arc` shared with the writer), so
  bypass can pull it directly and `mksafe(pipe(...))` composes.
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
- `request.dynamic(fn)` extends the same event-loop pattern to live
  request scheduling: `DynamicRequestSource` asks the Lua-owning thread for
  the next request URI via `ScriptEvent::NextRequest { index, reply }` and
  polls the reply with `try_recv` (the audio thread never blocks — it
  returns silence while a reply is outstanding). The next URI is requested
  as soon as a track is promoted, so a fast callback gives gapless
  handovers; nil ends the source; unresolvable requests are skipped and
  re-asked. Request URIs resolve through `resolve()` like every other
  request (annotate, http download), so a `request.dynamic` callback can
  feed paths or URLs.
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

- `RequestUri` (`Local(PathBuf, Option<TrackCues>)` | `Url(String,
  Option<TrackCues>)`): `single`, `playlist` entries, and the telnet
  request queue all carry URIs and resolve at play time via `resolve()`.
  `new()` parses a Liquidsoap-style `annotate:` prefix (`liq_cue_in`,
  `liq_cue_out`, `liq_fade_in`, `liq_fade_out`) into a `TrackCues`;
  malformed prefixes fall back to the plain URI. `TrackCues` holds `f64`s,
  so `RequestUri`'s `Eq`/`Ord` are hand-written (`None` < `Some`, values
  by `total_cmp`). `Local` opens a `FileSource`; `Url` downloads to
  `$TMPDIR/crabsoup-requests/{fnv1a}-{n}.part` (stable per-URL name +
  per-process counter so concurrent same-URL downloads can't collide, and a
  playlist loop re-requesting the URL re-downloads it) and wraps the file
  in `DownloadSource`, whose `Drop` removes the temp file — a killed process
  leaks `.part` files by design.
- Requests carrying cue points are wrapped in `CueCutSource`
  (`src/source/cue_cut.rs`) by `resolve()`: it skips `cue_in` seconds at
  each track start and reports exhaustion at `cue_out`, so playlists,
  `single`, and `queue.push` all honor `annotate:` windows with no extra
  wiring. The `cue_cut(src, opts)` operator wraps any source the same way;
  the window re-arms on every child label change (one `cue_cut` around a
  playlist trims every track). `CueCutSource` also reports per-track
  crossfade overrides via `AudioSource::crossfade_overrides()` (its
  `fade_in`/`fade_out`), and `apply_cues` wraps even when *only* fades are
  set, so `annotate:liq_fade_in=...` alone reaches the mixer.
- The HTTP client is hand-rolled on a `Transport` enum — a plain
  `TcpStream` for `http://`, a `rustls::StreamOwned` (ring provider,
  webpki-roots Mozilla store) for `https://` — and the status/header/chunked
  parsing is byte-identical over both (Part F1 wrapped the transport
  without touching the HTTP logic). Single connect per attempt, up to 4
  redirects (`Location` resolved absolute/root/relative against the request
  URL, preserving the scheme — a redirect may cross `https` <-> `http` and
  re-opens the transport per hop), `Content-Length` and chunked bodies,
  connection-close fallback, connect+read `request_timeout`. `download()`
  retries `request_retries` times with a 500 ms backoff and removes the
  partial file if all attempts fail. The trust store is overridable (tests
  inject a self-signed `rcgen` cert against a local `rustls` server).
- `show_error`-style fallback semantics: a failed `resolve()` surfaces as an
  error the caller decides on — the request queue drops the bad request and
  plays the next one; the playlist plays silence for that slot.

## Loudness / ReplayGain (`src/source/replaygain.rs`)

- `AudioSource::replaygain_db() -> Option<f32>` (default `None`) reports the
  current track's ReplayGain in dB. Every wrapper forwards it to the active
  child exactly like `label()`; only `FileSource` overrides it.
- `FileSource` reads `REPLAYGAIN_TRACK_GAIN`, falling back to
  `REPLAYGAIN_ALBUM_GAIN`, at open. symphonia 0.5 ships no ID3v2 reader
  (MP3 metadata is empty), so MP3s go through the hand-rolled
  `id3_replaygain` (ID3v2.3 non-syncsafe and v2.4 syncsafe frame sizes,
  TXXX frames, encodings 0/1/2/3 — a UTF-16 key keeps a `\0` byte between
  ASCII characters, so the first byte is dropped to recover the key;
  U+2212 minus and a trailing `" dB"` are normalized away). The symphonia
  path survives as the Ogg/Vorbis-comments fallback.
- `ReplayGainSource` applies a constant per-track gain: it re-reads the
  child's `replaygain_db()` whenever the label changes and clamps to
  ±`max_boost`/`max_cut` (12 dB default). Untagged tracks get unity gain.
  `normalize(replaygain(src))` feeds AGC the RG baseline; `replaygain`
  logs `replaygain: track gain {raw:.1} dB (applying {gain:.1} dB)` on
  change.

## Pitch / tempo (`src/engine/pitch.rs`)

- `stretch({ratio = 1.0}, src)` and `pitch({semitones = 0.0}, src)` wrap the
  child in a `PitchSource` over pure-Rust `wsola` 0.1.0 (no C, no system
  dependency; Part I1's bench decided it over a `soundtouch-sys` FFI path —
  the per-buffer FFI proxy row is ~0.2 µs vs ~17–29 ms of WSOLA DSP). `ratio`
  must be a positive finite number; `semitones` finite.
- Tempo is a straight WSOLA stretch (`set_tempo(ratio)`), preserving pitch.
- Pitch composes two legs to preserve duration: a WSOLA stretch by
  `1 / 2^(s/12)` (slower to raise, faster to lower), then a `SincResampler`
  step of `2^(s/12)` restoring the length. The resample step is derived from
  the *read-back* clamped tempo (`1 / stretch.tempo()`), so the legs undo
  each other exactly even at the WSOLA clamp limits.
- `TimeStretch::pull` allocates a fresh `Vec` per call; `PitchSource` reuses
  it via the `pending` buffer so the pull chain stays allocation-free in
  steady state. Child buffers are pre-folded into `feedbuf` to be
  frame-aligned for `push`; a `carry` keeps a trailing partial frame.
- `remaining_seconds()` scales the child's by `1 / tempo` (the read-back
  value), so crossfade preload timing stays correct for stretched tracks;
  `label`, `replaygain_db`, `crossfade_overrides`, and `skip` forward to the
  child. At child EOF the stretch flush tail is drained before `is_exhausted`
  reports true.

## Echo / delay (`src/engine/effects.rs`)

- `echo(src, {delay, ping, feedback, delay2, ping2, delay3, ping3,
  max_delay})` is an `Echo` effect: a shared circular delay line sized to
  `max_delay` (default 2 s), with up to three taps. Each tap adds
  `ping × line[read]` to the dry signal; the line is written with
  `dry + feedback × tapped`, so a single tap rings down at `feedback` gain
  (feedback 0 emits exactly one copy).
- Delays are expressed in frames (`seconds × rate`, × channels in the
  interleaved line) so multichannel stays phase-aligned; taps over
  `max_delay` are clamped to it. `process` walks the buffer sample by sample
  and a tap's read position always trails its write, so no sample is read
  before it is written even when the delay is shorter than the buffer; a tap
  delayed exactly `max_delay` reads the slot about to be overwritten, which
  holds the value written one full line ago — the correct full-delay echo.
- The delay line rings down only while the child keeps producing: the last
  `delay` worth of audio is dropped at child EOF (effects do not extend the
  track), and `EffectSource` forwards `remaining_seconds`/`label`/`skip`
  unchanged.

## Convolution reverb (`src/engine/reverb.rs`)

- `reverb(src, {ir, wet = 0.3, dry = 0.7})` wraps the source in a
  `ConvReverb` — a uniformly partitioned overlap-save convolver. `ir` is a
  file path, decoded once at operator-call time by `load_ir(path, rate)`
  via symphonia (mono → one IR applied to every output channel, stereo →
  two; extra channels dropped; a file not at the bus rate is resampled
  with `SincResampler`). Errors surface as `reverb: {path}: {reason}`.
- Partition `P` is the largest power of two in `[512, 32768]` that divides
  the bus frames-per-buffer (else 2048), so one `process` call usually
  yields exactly one block; FFT size `N = 2P`. Each block `m` keeps a ring
  of `K = ceil(ir_len / P)` spectra and accumulates
  `Σ_a ring[(m − a) mod K] · H_a` (partition `a` of the IR), then serves
  the last `P` IFFT samples — the alias-free overlap-save region. Output
  position `j` corresponds to input position `j`, so the effect adds zero
  latency (it is a pure filter, unlike a delay).
- Hot path allocates nothing: history window, spectra ring, IFFT
  accumulator, per-channel block staging, the `out_ring` serving block
  output, and the FFT scratch are all sized at construction and reused;
  the per-buffer `Vec::clear()`/`mem::take` bookkeeping keeps capacity.
  The multi-partition direct-convolution test and the partition-boundary
  delta tests (1023/1024/2047/2048) pin the block alignment; rustfft's
  inverse FFT is unnormalized, so the IFFT output is scaled by `1/N`.

## EQ / filters (`src/engine/eq.rs`)

- `eq(src, {bands = {{type, freq, gain, q}, ...}})` and
  `filter(src, {type, freq, q, gain})` wrap the source in an `Eq` — a
  bank of RBJ Audio EQ Cookbook biquads in Direct Form 1
  (`y[n] = b0·x + b1·x1 + b2·x2 − a1·y1 − a2·y2`). Bands
  (lowpass/highpass/bandpass/notch/peaking/lowshelf/highshelf) run in
  series inside each channel; `filters[c][band]` keeps per-channel,
  per-band state.
- The cookbook formulas are normalized by `a0` (they are usually quoted
  unnormalized) and every coefficient is baked into the `Biquad` at
  construction — the hot path is one `tick()` per sample per band, no
  trig, no allocation. Validation at operator-call time: `freq ∈ (0,
  fs/2)` and `q > 0` (the module returns `eq: ...` errors); gain is
  peaking/shelf-only and defaults to 0.
- A zero-gain band reduces to an exact passthrough (pinned by a
  sample-exact test); a resonant peaking driven with noise stays bounded
  (stability test) rather than ringing into overflow.

## Mixer control (`src/engine/mixer.rs`)

- `CrossfadeMixer` sizes each transition's overlap window at preload time:
  the incoming track's `fade_in` override, else the outgoing track's
  `fade_out`, else the global `crossfade_seconds` (frames re-derived per
  preload into a `fade_frames` field). The preload margin uses the
  outgoing track's `fade_out` too, so an annotated track starts its fade
  early enough; a `fade_in` longer than the margin degrades into the tail
  ramp, same as a track ending mid-fade. No override ⇒ global window,
  byte-identical to before.
- `smart_crossfade(opts)` enables level-aware window selection via
  `CrossfadeMixer::with_smart_fade(SmartFade { fade_out, fade_mid,
  threshold_db })`. While no fade is in progress the mixer folds each
  buffer into a rolling running sum of squares covering the active track's
  last `fade_out` seconds (chunked `VecDeque`, trimmed per buffer — no
  allocation), and at preload the RMS dBFS reading picks the window: a
  loud tail (≥ `threshold_db`, default -30) gets the full `fade_out`, a
  quiet one the short `fade_mid` (no point dragging a crossfade over
  silence). Per-track `fade_in`/`fade_out` overrides still win over the
  smart window, and the preload margin stays at `fade_out`, so a
  quiet-tail fade simply completes early — the loud and quiet paths are
  both exact integer-frame fades, covered by mixer tests that mirror the
  override-case sample values.
- `MixCommand` is the mixer control channel (`SetLive`, `ClearLive`,
  `PlayJingle(PathBuf)`, `Skip`, `Shutdown`) over `std::sync::mpsc`; the
  harbor and control port send into it. `Skip` calls `AudioSource::skip()`
  (trait default no-op; `CrossfadeMixer` advances); `Shutdown` makes
  `PriorityMixer` return 0/exhausted so every pump loop exits.
- `StatusHandle` is the shared label/uptime cell the tap consumers update and
  the telnet `status`/`uptime` read; it also owns the harbor's occupied
  `Arc<AtomicBool>`, so `status`/`json status` report live-DJ on-air state.
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
- SHOUTcast v1/v2 (`protocol = "shoutcast-v1" | "shoutcast-v2"`): both
  speak the legacy ICY handshake — the password as the first line plus
  `icy-*` headers with LF line endings; the reply is a bare `OK2` line or an
  HTTP-style head, so the head read is bounded by a short timeout. The DNAS
  v2 accepts ICY sources on both source ports; the native "uvox2" handshake
  is undocumented (and encrypted), and DNAS 2.6.1 rejects `SOURCE`/
  `POST`/`PUT` outright, so ICY is the interoperable path. v1 is MP3-only;
  v2 adds AAC as HE-AAC ("AAC+", fdk-aac AOT 5) announced with the
  `audio/aacp` content type — the Icecast/HLS AAC-LC profile stays
  untouched — and targets named streams by appending `:#N` to the password
  (the DNAS's documented v2.4.7+ way for ICY sources to pick a stream).
  Titles go out as `/admin.cgi?mode=updinfo&pass=<pw>&song=<title>` GETs
  with the source password — the ICY-source mechanism; the DNAS re-serves
  them as in-stream metadata to listeners, so no in-stream blocks are sent
  by the client (they'd be relayed as audio by the DNAS). Verified
  end-to-end against DNAS 2.6.1: MP3 (v1 on 8001, v2 on 8000) connects,
  `SONGTITLE` updates stick via updinfo, and listeners decode cleanly with
  `icy-metaint` metadata; AAC connects and is sniffed correctly but the
  DNAS corrupts the AAC listener relay (its legacy-path frame parser
  rewrites ADTS headers), so MP3 is the reliable SHOUTcast format.
- Opus titles ride the stream header: Icecast rejects URL metadata updates
  for Opus mounts (HTTP 200 + "Mountpoint will not accept URL updates"), so
  the initial OpusTags carries the first track's title (`set_title` replaces
  it until the headers flush, then no-ops). Icecast 2.4.4 never parses
  OpusTags titles at all; 2.5+ parses only the stream-start header and
  requires type-less packets (no RFC 7845 packet-type byte — ffmpeg writes
  them type-less too). Never inject comment pages mid-stream: Icecast
  forwards them to listeners as audio, producing decoder warnings.

## Soundcard I/O (`input.soundcard`, `output.soundcard`)

Both directions bridge through the same SPSC-ring pattern as the live
harbor, because cpal's callbacks run on a realtime audio thread that must
never block or allocate:

- `input.soundcard` (`src/source/soundcard.rs`): a small driver thread owns
  the `cpal::Stream` (`cpal::Stream` is `!Send` on ALSA), opens the device,
  and parks on `std::thread::park` (woken on drop). Its realtime callback
  converts device samples to `f32` in stack chunks and `push_slice`s them
  into the ring. The `AudioSource` half drains on the pull thread,
  converts channels, and resamples to the bus spec — exact passthrough
  when the device rate matches the bus; a pull-side cap drops the stale
  window when the consumer lags. The device open is a synchronous bounded
  handshake at script evaluation, so a missing/broken device fails fast —
  note this makes `--check` hardware-dependent for scripts that register
  `input.soundcard` (the source must exist before the engine pulls it;
  deferring the open would need a lazy bridge, so it fails loudly instead).
- `output.soundcard` (`src/output/soundcard.rs`): a tap consumer (claims
  the root like `output.file`); its thread resamples/converts into a
  reusable scratch and pushes into a ring the device callback drains
  (silence on underrun). The device and stream open in `connect()` at
  startup, so a missing device fails before the tap pulls. Supported
   device channel counts are 1/2 (mirroring the bus); f32/i16/u16/i32/f64
   sample formats are converted with the same scaling cpal/dasp uses.

**Clock-drift compensation** (Part G2): the device's hardware sample clock
never exactly matches the bus pacing — over a long run a few tens of PPM
drift fills or drains the ring until under/overrun. Both halves run the
same proportional control loop on the ring fill:

- `ppm = clamp(gain * EWMA(fill - target))`, with `gain = 1e-5` (fraction
  per sample; ~2 s control time constant at 44.1 kHz, steady fill offset
  `drift/gain`), clamp ±1 %, and EWMA α = 0.01 (a ~4 s window). The fill
  saws a full pull's worth between pulls, and that deterministic sawtooth
  must be smoothed out before it hits the ratio, or the ratio would jerk
  every pull.
- `SincResampler` gained a PPM-nudged step (`set_ppm`/`ppm`); the hot loop
  advances `pos += ratio * step_mult`.
- Input (`src/source/soundcard.rs`): pulls `capacity * (D/B) * (1 + ppm)`
  with a fractional pull-debt accumulator (integer pops would bias
  consumption by ~1 frame/pull ≈ 500 PPM at 2048-frame pulls); passthrough
  truncates the excess instead of buffering it. The estimate converges to
  `+skew` (device fast → fill high → consume more).
- Output (`src/output/soundcard.rs`): nudges the step so production tracks
  the drain; the estimate converges to `-skew`. Same loop as the input,
  opposite sign, because a fast device *drains* the output ring.
- Real devices run ±20–200 PPM, so the clamp only bites on transients
  (startup, device hotplug). The loop's job is to hold the fill mid-ring
  for hours; the drop-oldest cap (input) and underrun silence (output)
  remain as last-resort guards.

**Manual verification** (hardware is not reliably unit-testable in CI, so
this is documented rather than automated):

1. Record a known tone: `output.file({path = "/tmp/in.ogg", format =
   "opus"}, input.soundcard({}))` — play a 440 Hz sine (or a loopback of
   a test file) into the line/mic input and confirm `/tmp/in.ogg` decodes
   to a 440 Hz tone (`ffprobe` + an FFT viewer, or compare the zero
   crossing rate as `docs/` tests do for the resampler).
2. Play a known tone: `output.soundcard({}, single("examples/tone.mp3"))`
   — confirm the tone is audible/recordable at the output.
3. Round-trip: `output.soundcard({}, input.soundcard({}))` with a
   physical (or virtual/loopback) device — the output reproduces the
   input; device-rate mismatch exercises the resampler.
4. Drift soak (G2 acceptance): run the round-trip (or
   `input.soundcard` → `output.file`) for at least 2–4 hours and watch the
   control-port/`status` output — the ring fill must stay mid-ring, not
   drift toward empty/full (underruns sound like gaps; overruns trip the
   drop-oldest cap, audible as skips). A short run cannot prove the loop;
   the simulated-skew unit tests cover the convergence math, the soak
   covers the real clock.

## Live harbor (`src/live/harbor.rs`)

Decodes DJ uploads to target-spec PCM: MP3/Vorbis/AAC via symphonia, Opus via
the native `OpusSource` path (symphonia 0.5 has no Opus codec) after
sniffing the first Ogg page. Auth is source-protocol Basic auth against the
mount `password` or any per-streamer `extra_passwords` (all share the same
mount). The harbor's `occupied` flag is a shared `Arc<AtomicBool>` owned by
the `StatusHandle`, so the control port reports live-DJ state: telnet
`status` prints a `live:` line and `json status` carries
`harbor_connected`. The PCM crosses to the audio thread through a
lock-free SPSC ring (`ringbuf::HeapRb`, sized at `2 * MAX_LIVE_FRAMES`):
`LiveSink` (the decode thread) pushes with `push_slice`, `LiveSource` (the
mixer) pops with `pop_slice` and enforces the 5 s drop-oldest latency cap by
skipping anything older on pull — the same lag the old `Arc<Mutex<VecDeque>>`
drain-on-push kept, but ~13x faster on the pull (benchmarked in
`live_handoff`; see ROADMAP). When the ring is full the sink applies
backpressure (waits for the consumer to drain) instead of dropping, so the
newest audio is never silently lost — a fast `curl -T` upload throttles to
real time and plays completely, as before.

## Relay input (`input.http`, `src/source/http.rs`)

Pulls a continuous remote stream (syndicated/affiliate feed) with the same
shape as the harbor, inverted: a detached network thread `GET`s the URL and
decodes the live body into an SPSC ring (`ringbuf::HeapRb`, `2 * 5 s` of
audio, drop-oldest cap on pull), and the `AudioSource` half pops PCM
lock-free — the audio thread never touches the network or blocks on it.
Connection lifecycle is a reconnect loop mirroring `IcecastOutput`:
`http_get_stream` (new in `src/request.rs`: streaming body — content-length
bounded, chunked, or connection-close delimited — with `Icy-MetaData: 0`
so Icecast/DNAS keep in-stream metadata out of the audio, up to 4
redirects re-opening the transport per hop) → sniff the first Ogg page
(fed back with `PrependReader`, the harbor trick) → OpusHead relays take
the native `OpusSource` path; anything else goes to symphonia
probe/`MediaSourceStream` over `ReadOnlySource` (no seek-back on a live
stream), hinted by the response `Content-Type`. The decode pushes through
`Sink` with backpressure on a full ring (never drops newest audio);
connection end (clean EOF or read error) resets `connected` and sleeps an
interruptible backoff (default 500 ms, `reconnect_backoff` option), broken
early by the shared shutdown flag when the source is dropped. While
disconnected, `is_exhausted()` is `true` (ring empty + not connected), so
`fallback({input.http(url), local})` covers the gap with no script-side
handling — the relay preempts, the local source plays the gap. URLs are
shape-validated at script evaluation (scheme + non-empty host, no DNS);
resolution happens per reconnect attempt.

## Video path (Parts H1/H2/H5/H6/H7, `--features video`)

Video is a parallel side-channel to the PCM bus, compiled out by default
(`video = ["dep:ffmpeg-next"]`; all FFmpeg `unsafe` stays inside
`ffmpeg-next` — no raw FFI in the tree).

- **Carrier**: `VideoFrame` (YUV420P planes + `pts_us`) rides its own
  fan-out `VideoTap` (`src/video/tap.rs`, bounded `sync_channel(4)`,
  `try_send` drop-oldest). The audio hot path never sees it.
- **Decode**: `src/video/ffi.rs` `VideoDecoder` (ffmpeg-next, swscale to
  YUV420P, B-frame reorder via a `pending` queue). One decode thread per
  `video.video(path)` track; a `video.playlist`/`video.single` sequence
  runs on one thread that plays tracks one at a time and carries an
  accumulated PTS offset across files (`VideoSource::spawn_playlist`), so
  the published timeline is continuous — the wall-clock pace is
  `start + published_pts`, and at track end the offset snaps to elapsed
  time. `loop` (default true) re-cycles and re-shuffles.
- **Slideshow** (Part H2): `video.slideshow(...)` decodes each image to a
  YUV420P `VideoFrame` at script evaluation (`VideoSource::decode_image`),
  so `VideoSource::spawn_slideshow` only re-publishes decoded planes at a
  chosen `fps` — the render thread cannot fail mid-run. The PTS model is
  the playlist's (accumulated offset, wall-clock paced). A `"fade"`
  transition crossfades the previous picture into the current one over
  `transition_seconds` by blending whole planes element-wise with an
  integer alpha 0..=256 (`blend_planes` in `src/video/effect.rs`); the
  first picture has no transition (no `prev`).
- **Effects (Part H3)**: `video.scale({width, height}, marker)` and
  `video.fade({fade_in, fade_out}, marker)` wrap any `video.*` marker
  (the operator mutates the wrapped source's `VideoEffects` config via an
  opaque `__src` registry key on the marker table) and are applied on the
  source render threads — scale first, then fade — so every output sees
  processed frames. `src/video/effect.rs` is pure Rust: scale is a
  half-pixel-centered fixed-point bilinear YUV420P resampler (deterministic
  across platforms, no new FFmpeg surface), fades blend whole planes toward
  black. The fade windows anchor on the source's own timeline
  (`VideoDecoder::duration_us()` for files, per-track duration in
  playlists, total show length for slideshows; looping sources skip
  fade-out). The marker's spec follows `VideoEffects::scaled_spec`, so
  `first_video_spec` (main.rs) opens encoders at the scaled resolution.
- **Mux**: `src/output/hls.rs` `VideoTrack` encodes to H.264 (libx264 via
  ffmpeg-next: baseline/ultrafast/zerolatency, 90 kHz time base, PTS==DTS,
  `AV_CODEC_FLAG_CLOSED_GOP` + `scenecut=0`) and muxes into the same
  MPEG-TS segments as the AAC audio. Segment rotation defers to a forced
  IDR (`frame.pict_type = I` per push — a reused frame must be reset to
  `None` after each `send_frame`), so every segment starts with
  SPS/PPS/IDR and is independently joinable; `feed` rotates immediately
  once the tap is gone or stalled one extra window. The encoder opens at
  the first registered track's spec; `video.playlist` and
  `video.slideshow` therefore enforce one resolution per list at script
  evaluation.
- **RTMP (Part H5, `--features rtmp`, optional `video`)**: `output.rtmp`
  (`src/output/rtmp.rs`) consumes the audio tap, encodes raw AAC (fdk-aac
  `TT_MP4_RAW` transport — the ASC from `aacEncInfo` becomes the FLV AAC
  sequence header) and, with `video = marker`, subscribes to the `VideoTap`
  like HLS: a `RtmpVideoTrack` encodes H.264 (the H6 encoder) and holds
  access units in a PTS-ordered `pending` queue until the audio clock
  catches up (`pts_90k/90 <= audio_ts_ms`), emitting IDR-first.
  `src/output/flv.rs` is the pure-Rust FLV muxer: `@setDataFrame`
  onMetaData as an ECMA array, 24-bit+ext-byte timestamps, AVCC sequence
  header from SPS/PPS + 4-byte-length NAL repack (`avcc_nalus`). Bytes are
  pushed through a thin `unsafe extern "C"` librtmp FFI (`RtmpSession` in
  `rtmp.rs` — the only new `unsafe` surface, next to the encoder FFI; a
  `RtmpSink` trait lets tests inject a byte-collecting `CaptureSink` via
  `connect_to`). The FLV includes the 4-byte PreviousTagSize trailer per
  tag. Reconnect mirrors Icecast (main.rs spawn loop). `CRABSOUP_DUMP`
  persists the captured FLV for ffprobe in the inline tests.
- **Master playlist**: `index.m3u8` (`#EXT-X-STREAM-INF`, RESOLUTION from
  the track spec, static CODECS) is written next to `playlist.m3u8` when
  `output.hls({video = ...})` is configured.
- **MP4 recording (Part H4, `--features video`)**: `output.mp4`
  (`src/output/mp4.rs`) records the tap to a seekable MP4 through
  ffmpeg-next's `mov` muxer. Audio is FDK-AAC on the raw transport (raw
  access units, no ADTS; the AudioSpecificConfig from `aacEncInfo` becomes
  the `esds` codecpar extradata). With `video = marker` it subscribes to
  the `VideoTap` like HLS/RTMP and muxes H.264 access units gated on the
  audio clock, with periodic forced IDRs (~2 s) keeping the recording
  seekable. The mov muxer needs the avcC in the stream's codecpar (first
  byte 0x01) and length-prefixed samples (`ff_isom_write_avcc` writes
  packet data as-is then), so `mp4.rs` builds avcC from the first access
  unit's SPS/PPS via the FLV helpers and repacks each AU with
  `flv::avcc_nalus`. The container header is deferred until those
  parameter sets exist (audio AUs park in `pending_audio`); audio-only
  files start the header at `connect`. ffmpeg-next exposes no safe setters
  for the codec id or codecpar extradata, so the module carries a small FFI
  shim (`av_malloc`'d buffers into `AVCodecContext`/`AVCodecParameters` —
  alongside the encoder FFI, the only `unsafe` surface). The file is
  opened up front in `connect` so a bad path fails at startup; `run` writes
  the trailer (moov) when the tap closes or on shutdown.

## Gotchas that have burned previous work

- Ogg CRC is CRC-32/MPEG-2 (MSB-first, init 0, poly 0x04c11db7, no final
  xor). The input byte xors into the table **index**
  (`idx = ((crc >> 24) ^ b) & 0xff`), not into the result. A previous bug
  corrupted every page silently; `crc_matches_external_reference` guards it.
- Opus requires 48 kHz sample rate — never feed it the bus rate directly.
- librtmp publish gotchas (Part H5): `RTMP_EnableWrite` must run *before*
  `RTMP_ConnectStream` — otherwise librtmp never sends ReleaseStream/
  FCPublish/`publish`, and a server like nginx-rtmp logs `play:` but never
  `publish:`, relaying 0 bytes to subscribers. `RTMP_ConnectStream` returns
  FALSE (0), not negative, on failure. `RTMP_Write` parses the FLV stream
  itself: it skips a leading 13-byte "FLV" header write, reads one message
  header per tag (so each tag must be a full message, not a chunk),
  skips 4-byte PreviousTagSize after each tag, prepends `@setDataFrame` +
  16 bytes to INFO messages, uses channel 0x04 and msid = stream id, and
  returns `size-4` when a trailing PreviousTagSize is missing — our tags
  always carry it so the size matches. The nginx-rtmp `codec: error
  parsing data frame` line is benign (its codec module warns while
  extracting metadata; relaying is unaffected).
- MP4/movenc (Part H4): the mov muxer decides how to write packets from
  the codecpar extradata. When the H.264 extradata's first byte is 0x01
  (avcC) it writes packet data as-is — so feed length-prefixed access
  units (`flv::avcc_nalus`) with an avcC built from SPS/PPS
  (`flv::parameter_sets` → `flv::avcdcr`); without that it would try to
  parse Annex-B from packet data and mis-size samples. AAC packets are
  written as-is, so raw access units (no ADTS) + the ASC extradata are
  correct. ffmpeg-next 7.x offers no safe setter for codecpar extradata or
  the codec id, so `mp4.rs` pokes the `AVCodecContext`/`AVCodecParameters`
  structs directly with `av_malloc`'d buffers (FFI is the crate's one
  sanctioned `unsafe` surface).
- `FallbackSource::label()` reports the *next* child's label the moment the
  current child exhausts (even on the current track's last pull), because
  it follows `active()`. Harmless for `on_metadata` (an event fires one
  buffer early), but a `map_metadata` rewrite observed on that pull targets
  the *next* track — tests that assert a rewritten title on a short
  `sequence` track must give the rewrite time to land before the boundary.
- mlua maps a missing Lua field to `false` for a plain `bool` `get` target
  (nil is falsy), so optional bools with a non-false default must be read
  as `Option<bool>` and `unwrap_or`'d — `blank.detect`'s
  `exhaust_while_blank` defaulted to false and broke fallback handover
  until read that way. `playlist`'s `loop` had the same latent bug
  (documented default true, actual default false) until it was read the
  same way; the `video.playlist` marker test caught it.
- Pitch/tempo (Part I1): the pitch leg is a *two-stage* composition — WSOLA
  stretch by `1/p` then resample step `p` (both restore the duration; either
  leg alone inverts the effect). The resample step must come from the
  read-back clamped tempo, not the requested factor, or clamped semitones
  drift off the ideal pitch/duration; the octave-up/down unit tests guard it.
- Convolution reverb (Part I3): the pending-block `Vec` is drained into a
  local via `std::mem::take` but must be `clear()`ed after processing — a
  naive `self.pending = pending` restore replays every previously queued
  block each call (1 → 2 → 3 process_block runs per buffer), corrupting
  output at block boundaries with a ramp error; the multi-partition test
  caught it.
- The `on_metadata` closure stays in the Lua registry for the process
  lifetime, keeping its channel `Sender` alive: the event loop can never wait
  for channel disconnection, so it polls `recv_timeout` and exits on the
  shared end-flag the tap sets when the engine stops. `drain_metadata()` is
  the non-blocking variant for tests.
