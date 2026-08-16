# Architecture

Implementation-level wiring for contributors. The full companion document
lives in `docs/ARCHITECTURE.md` in the repo.

## Pipeline

```
root source (script graph, crossfades inside) -> PriorityMixer -> EngineTap -> [outputs]*
```

via the native source-protocol client. The `.lua` script's root source (e.g.
`fallback({j, live, pl})`) is that root input; playlists/jingles carry their
own `CrossfadeMixer` inside the script graph, and the engine wraps the root
in a `PriorityMixer` for live/jingle ducking. All sources are normalised
to the PCM bus (`set("sample_rate", ...)`, `set("channels", ...)`,
`frames_per_buffer`).

## Threading model

One engine thread plus one thread per output:

- `EngineTap` owns the root source and pulls at wall-clock pace, publishing
  each buffer as `Arc<AudioFrame { pcm, label }>` to N bounded taps.
- Outputs are pure consumers (`for frame in rx { encode + send }`) with
  independent reconnect loops — one stalled mount drops frames instead of
  stalling the pull or the other outputs.
- Frames return their PCM to a preallocated pool via `Drop`; allocation only
  happens on the degraded stall path.
- When the tap stops for any reason it sets a shared end-flag, which wakes the
  Lua-owning main thread's event loop.

## Script layer

`src/script.rs` registers the Liquidsoap-flavoured Lua stdlib. Sources are Lua
userdata wrapping `Arc<Mutex<Box<dyn AudioSource>>>` so they compose. Lua
hooks that must run Lua code (`on_metadata`, `on_track`, `map_metadata`,
`request.dynamic`, `server.register`) cross the audio thread / Lua thread
boundary as events over channels; the Lua-owning thread runs the event loop
and invokes the callbacks. The audio thread never blocks — it polls replies
with `try_recv` under a bounded budget.

## Loudness / ReplayGain

Per-track gain comes from `REPLAYGAIN_TRACK_GAIN` (falling back to
`REPLAYGAIN_ALBUM_GAIN`). MP3s go through a hand-rolled ID3v2 reader
(symphonia 0.5 has no ID3v2 support); Ogg/Vorbis comments go through
symphonia. Gains are clamped to the configured max/cut and applied as a
constant per-track gain.

## Mixer control

`MixCommand` is the mixer control channel (`SetLive`, `ClearLive`,
`PlayJingle`, `Skip`, `Shutdown`); the harbor and control port send into it.

- `CrossfadeMixer` sizes each transition's overlap window at preload time:
  the incoming track's `fade_in` override, else the outgoing track's
  `fade_out`, else the global `crossfade_seconds`.
- The overlap `crossfade` source ends each fade at the outgoing track's
  audible tail: the span is K-weighted first (BS.1770-4 Tables 1/2 at
  48 kHz, De Man biquads), then a BS.1770-style dual gate (absolute
  −70 dBFS, then −10 LU below the track's own gated mean) over
  50 ms/10 ms mean-square windows finds the last audible frame (this
  replaced the removed `smart_crossfade` operator's fixed-threshold
  window choice).
- `PriorityMixer` crossfades between the main source and an override (live DJ
  or jingle) with a gain ramp over `duck_seconds`.
- Both mixers keep reusable scratch buffers so `next_buffer` never allocates.

## Opus path

`SincResampler` (16-tap Hann-windowed sinc, 256-phase table) bus -> 48 kHz,
encode 20 ms frames, mux one Ogg page per packet, flush per packet so audio
reaches Icecast promptly. Opus *requires* 48 kHz — the bus rate is never fed
directly.

## AAC path

FDK-AAC via FFI (AAC-LC, raw ADTS transport). 44.1 kHz needs no resampler.
FDK consumes at most one frame's worth of input per encode call, so `encode`
loops on the leftover, and `finish` drains with `numInSamples = -1` until
EOF. ADTS has no in-stream title mechanism.

## Icecast client

Native source-protocol client (no libshout): one authenticated `SOURCE`
request, then raw encoded bytes; titles go out on separate authenticated
`/admin/metadata` GETs (MP3/AAC). Opus titles ride the stream header —
Icecast rejects URL metadata updates for Opus mounts, and 2.4.4 never parses
OpusTags at all.

## Live harbor

Decodes DJ uploads to target-spec PCM: MP3/Vorbis/AAC via symphonia, Opus via
the native `OpusSource` path after sniffing the first Ogg page. The PCM
crosses to the audio thread through a lock-free SPSC ring: a 5 s drop-oldest
latency cap, and backpressure instead of dropping when the ring is full.

## Gotchas that have burned previous work

- Ogg CRC is CRC-32/MPEG-2 (MSB-first, init 0, poly 0x04c11db7, no final
  xor). The input byte xors into the table **index**, not into the result.
- Opus requires 48 kHz sample rate — never feed it the bus rate directly.
- mlua maps a missing Lua field to `false` for a plain `bool` `get` target
  (nil is falsy); optional bools with a non-false default must be read as
  `Option<bool>` and `unwrap_or`'d.
- The `on_metadata` closure stays in the Lua registry for the process
  lifetime, keeping its channel `Sender` alive: the event loop polls
  `recv_timeout` and exits on the shared end-flag, never on channel
  disconnection.