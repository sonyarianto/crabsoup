# Roadmap

The live plan and ship history live in
[`ROADMAP.md`](https://github.com/sonyarianto/crabsoup/blob/main/ROADMAP.md)
in the repo; this page is a condensed status snapshot.

## Done (verified end-to-end)

- YAML config (replaced by `.lua` scripting)
- Playlist scheduling (directory scan, loop, shuffle) and gapless crossfades
- MP3 -> Icecast broadcast, 192 kbps, title metadata
- Opus + built-in Ogg muxer end-to-end (spec-correct CRC-32, live-verified)
  and AAC/ADTS via fdk-aac
- `.lua` scripting replaces YAML entirely (Liquidsoap-flavoured Lua stdlib
  evaluated by mlua, vendored Lua 5.4)
- Native Icecast source protocol (no libshout); live DJ harbor end-to-end
  with mixer ducking; native Opus decode for files and DJ streams
- Engine tap (single puller, multi-consumer fan-out) + Lua-owning event loop
- Request queue (`queue.push`), HTTP/HTTPS requests, `request.dynamic`
- Scripting parity: `mksafe`, `annotate:` cue points + `cue_cut` (per-track
  fade overrides), `add()` weighted mixing, `request.dynamic`, level-aware
  `smart_crossfade`, `switch`/`rotate` scheduling, `blank.detect` dead-air
  guard, `map_metadata` title rewriting, `server.register` custom commands
- External process pipeline (`pipe()`) with bypass + restart on process death
- `output.file`, multi-mount `output.icecast`, HLS output (MPEG-TS muxer +
  segmenter), `input.soundcard` / `output.soundcard` via SPSC rings
- ReplayGain support (ID3v2 + Vorbis comments)
- Performance baseline with criterion (`cargo bench --bench engine`): the
  crossfade mixer mixes a 92.9 ms buffer in ~107 µs (0.12 % of real time)

## Known limitations

- Icecast 2.4.4 shows no Opus titles; 2.5+ shows the stream-start title only.
- Shoutcast v1/v2 is planned only if a concrete need shows up.

## Next up

Liquidsoap parity and performance, landing as independently-shipping phases
(inline tests, verified live) while beating Liquidsoap on CPU/memory per
concurrent output — via real OS threads and allocation-free hot paths.

Per-phase principles:

- No `Vec::new()`/`vec![...]` inside a `next_buffer` hot path
- Lock once per call, not per method
- One thread per output plus one puller thread; DSP effects stay inline
- Benchmarks for mixers, resampler, and encode path with recorded baselines
- SIMD only after the benchmark harness shows it is the bottleneck

### Performance baseline (criterion 0.5, release build)

Machine: dev box. One 4096-frame stereo buffer = 92.9 ms of audio at 44.1 kHz.
Compare within one session (`criterion --save-baseline`/`--load-baseline`).

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

Full path (crossfade + compressor/agc/amplify + resample + encode) ≈ 2.6 ms
per 92.9 ms buffer ≈ 2.8 % of one core.