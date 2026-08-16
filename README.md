# Crabsoup

[![Sponsor](https://img.shields.io/badge/Sponsor-GitHub%20Sponsors-ea4aaa.svg)](https://github.com/sponsors/sonyarianto)
[![License: AGPL-3.0](https://img.shields.io/badge/License-AGPL--3.0-blue.svg)](LICENSE)
[![CI](https://github.com/sonyarianto/crabsoup/actions/workflows/ci.yml/badge.svg)](https://github.com/sonyarianto/crabsoup/actions/workflows/ci.yml)

An audio/video streaming engine in one binary: the Lua runtime, scheduler,
mixers and outputs are all compiled in — the only system dependencies are
the codec libraries (LAME, libopus, fdk-aac; see Building). You describe
your station in a Lua script (Liquidsoap-flavoured, real Lua), and Crabsoup
decodes the files, mixes in live DJ input and one-shot jingles with
crossfades, and broadcasts the result — MP3, Opus or AAC to an Icecast
server, or H.264 + AAC to HLS, RTMP or MP4 with video on the same script.

It was written as a replacement for a Liquidsoap stack: same workflow, no
OCaml. The script language will feel familiar if you know Liquidsoap.

## What it looks like

```lua
set("sample_rate", 44100)
set("channels", 2)
set("crossfade_seconds", 3.0)    -- track-to-track overlap
set("fade_curve", 1.0)           -- 1.0 linear, 2.0 equal-ish power
set("duck_seconds", 1.5)         -- live DJ / jingle fade time

pl = playlist({directory = "./media", shuffle = false, loop = true})
j  = jingles({directory = "./jingles"})        -- telnet-triggered clips
live = input.harbor({port = 8005, mount = "/live", password = "dj"})
server.telnet({host = "127.0.0.1", port = 1234})

output.icecast({host = "localhost", port = 8000,
                mount = "/crabsoup.opus", format = "opus", bitrate = 128000,
                source_password = "hackme", name = "Crabsoup"},
               fallback({j, live, pl}))
```

That is the whole station. The script returns a source graph; `fallback`
picks the first child that has audio — jingle while triggered, live DJ while
connected, playlist otherwise. Every source runs on a shared PCM bus
(`set("sample_rate", ...)`, `set("channels", ...)`), so sources built for
different rates and channel counts just work.

## Running

```sh
cp crabsoup.lua.example crabsoup.lua   # or write your own (see above)
RUST_LOG=crabsoup=info ./target/release/crabsoup -c crabsoup.lua
```

With an `output.icecast` call it connects to Icecast as a source. Without one
it runs in preview mode (decodes and mixes, nothing leaves the machine).
`crabsoup --check` evaluates the script, prints the resolved configuration,
and exits — useful for debugging a script without starting a broadcast.

Per-format test scripts live in `examples/`:

```sh
./target/release/crabsoup -c examples/crabsoup.opus.lua    # Opus -> /crabsoup.opus
./target/release/crabsoup -c examples/crabsoup.mp3.lua     # MP3  -> /crabsoup.mp3
./target/release/crabsoup -c examples/crabsoup.aac.lua     # AAC  -> /crabsoup.aac
./target/release/crabsoup -c examples/crabsoup.shoutcast.lua # SHOUTcast v1/v2
./target/release/crabsoup -c examples/crabsoup.pipe.lua    # external processor pipeline (preview)
./target/release/crabsoup -c examples/crabsoup.dsp.lua     # compress -> normalize -> amplify
./target/release/crabsoup -c examples/crabsoup.tone.lua    # blank/sine test tone, no media needed
./target/release/crabsoup -c examples/crabsoup.preview.lua # no broadcast
./target/release/crabsoup -c examples/crabsoup.video.lua    # file -> HLS(video)
./target/release/crabsoup -c examples/crabsoup.rtmp.lua     # file -> RTMP (nginx-rtmp)
./target/release/crabsoup -c examples/crabsoup.mp4.lua      # file -> MP4 recording
```

## Building

Requires Rust (edition 2024) and the dev packages for the native codecs:

```sh
sudo apt install libmp3lame-dev libopus-dev   # Debian/Ubuntu
# fdk-aac has no Debian/Ubuntu package: build from source into /usr/local
# (build.rs links against it there):
#   git clone https://github.com/mstorsjo/fdk-aac && cd fdk-aac
#   ./autogen.sh && ./configure --prefix=/usr/local && make && sudo make install
cargo build --release
```

Video (HLS + RTMP) adds the FFmpeg dev packages and the `video` feature:

```sh
sudo apt install libavcodec-dev libavformat-dev libavutil-dev libswscale-dev
cargo build --release --features video
```

RTMP publishing adds `librtmp-dev` and the `rtmp` feature:

```sh
sudo apt install librtmp-dev
cargo build --release --features rtmp          # audio-only RTMP
cargo build --release --features rtmp,video    # h264 + aac RTMP (and HLS)
```

Lua 5.4 is vendored and compiled at build time (needs a C compiler).

## What's in the box

- Playlist scheduling: recursive directory scan, explicit file lists, loop
  and shuffle, gapless crossfades (`crossfade_seconds`, `fade_curve`)
- Live DJ harbor: an Icecast source-protocol listener (`PUT /live`); the
  playlist ducks out while a DJ is live and fades back in on disconnect
- Jingles: one-shot clips played over the music, triggered from a
  Liquidsoap-style telnet control port
- Outputs: MP3 (LAME), Ogg/Opus (libopus + a built-in Ogg muxer with
  spec-correct CRC-32), AAC/ADTS (fdk-aac), soundcard, file, HLS, RTMP,
  MP4 recording
- Video (Part H, `--features video`): `video.video`, `video.single`,
  `video.playlist` and `video.slideshow` (stills with optional crossfades)
  feed a decode thread that publishes PTS-paced frames to a shared tap;
  `video.scale`/`video.fade` wrap any source as per-source effects (Part H3);
  `output.hls({video = ...})` live-encodes to H.264 and muxes into
  keyframe-aligned MPEG-TS segments with a variant master playlist. See
  `examples/crabsoup.video.lua`
- RTMP (Part H5, `--features rtmp`): `output.rtmp` publishes the stream as
  FLV (raw AAC, optional H.264 with `video = marker`) to nginx-rtmp, YouTube
  Live or similar, with the Icecast-style reconnect loop
- MP4 recording (Part H4, `--features video`): `output.mp4` records to a
  seekable MP4 file (AAC-LC, optional H.264) via FFmpeg's mov muxer; the
  file is opened at script start and finalized with the moov trailer on
  shutdown

## Control port

```sh
printf 'jingles.play\n' | nc localhost 1234   # random jingle
printf 'jingles.play trance\n' | nc localhost 1234  # by substring
printf 'jingles.list\n' | nc localhost 1234   # index + path per line
printf 'skip\n' | nc localhost 1234           # skip the current track
printf 'status\n' | nc localhost 1234         # current track + uptime
printf 'shutdown\n' | nc localhost 1234
```

Commands: `jingles.list`, `jingles.play [n|substr]`, `skip`, `status`,
`uptime`, `shutdown`, `exit`, `help`, plus anything you register in the
script with `server.register("name", function(args) return reply end)` —
the handler gets the rest of the line as one string and its return value is
sent back; a Lua error becomes an `ERROR: ...` reply.

Prefix any command with `json ` for a machine-readable reply: a single line
of JSON, `{"ok": true, ...}` on success and `{"ok": false, "error": "..."}`
on failure (`json status` → `{"ok":true,"playing":"...","uptime_seconds":N,
"harbor_connected":bool}`). The name `json` is reserved. Machine clients
should pass `banner = false` to `server.telnet` so the connection starts
with replies instead of the prose welcome line. Plain-text `status` also
shows live-DJ state as a `live: true|false` line.

`server.telnet({http_port = N})` serves the same commands over HTTP:
`GET /status`, `GET /uptime`, `GET /queue`, `GET /jingles`, and
`POST /cmd` with `{"command": "..."}` — same JSON envelope, 400 on
`{"ok": false}`, 404/405 for bad routes and methods.
`examples/control_api.py` is a worked backend using both transports. See
[the control-port guide](website/guide/control-port.md) for the full field
table.

## Source and DSP reference

Named options are Lua tables; most have defaults. `format` is `"mp3"`,
`"opus"`, or `"aac"`.

**Inputs** (live sources):

- `input.harbor({port, mount, password})` — live DJ via Icecast source protocol
- `input.soundcard({device})` — capture from a sound card (cpal) via an SPSC ring
- `input.http(url, {reconnect_backoff})` — continuous relay/pull source;
  reconnects on drop and exhausts while disconnected, so
  `fallback({relay, local})` composes

**Outputs** (all consume the shared tap, any number at once):

- `output.icecast({...}, src)` — MP3/Opus/AAC to Icecast (or SHOUTcast v2)
- `output.soundcard({device}, src)` — play through a device
- `output.file({path, format}, src)` — encode to a local file
- `output.hls({...}, src)` / `output.mp4({...}, src)` / `output.rtmp({...}, src)` —
  HLS segments, MP4 recording, FLV publishing (video feature)
- `output.preview(...)` — decode and mix, broadcast nowhere

**Sources** (composable — any source works where a source is expected):

- `playlist({directory|files, shuffle, loop})` / `single(path)` — media playback
- `jingles({directory})` — one-shot clips, telnet-triggered
- `fallback({a, b})` — first child with audio; `sequence({a, b})` — strict order
- `random({a, b})` — shuffle without repeats
- `switch({...})` — dayparting: slots with a `when` predicate (weekday
  `days` as names or 0-6, `from`/`to` in `"HH:MM"`, overnight windows wrap;
  `from == to` never matches) are checked at each track boundary; the last
  slot must be a default without `when`. `track_sensitive = false`
  re-checks every buffer and cuts mid-track.
- `rotate({a, b}, {weights = {1, 2}})` — weighted round-robin, holding a
  child for `weights[n]` consecutive tracks
- `blank({duration})` / `sine({freq, duration})` — test tones
- `mksafe(src)` — silence covers an exhausted or failed child
- `blank.detect(src, {threshold, duration, restart, exhaust_while_blank,
  on_blank})` — dead-air guard: after `duration` seconds of sub-`threshold`
  silence it reports exhausted (so a `fallback` around it hands over);
  `on_blank` fires a Lua callback once per episode and the source recovers
  when audio returns
- `map_metadata(src, function(m) return {title = ...} end)` — rewrites each
  track's title through a Lua callback before it reaches the output (the
  original is kept on nil/error/timeout)
- `add({a, b}, {weights = {...}})` — sample-wise sum with optional per-source
  weights (background bed + voice-over)
- `cue_cut(src, {cue_in, cue_out, fade_in, fade_out})` — skips `cue_in`
  seconds into each track and ends it at `cue_out`; per-track
  `fade_in`/`fade_out` override the global `crossfade_seconds` for that
  track's crossfades
- `request.dynamic(function() return uri_or_nil end)` — plays whatever the
  callback returns, one request ahead of the current track (nil ends the
  source) — a live-programming scheduler without a playlist file
- `smart_crossfade({directory, ...})` — a `playlist` whose transition window
  is chosen by the outgoing track's measured tail level: a loud tail gets a
  full `fade_out` crossfade, a quiet tail only a short `fade_mid` fade.
  `fade_out` defaults to `crossfade_seconds`, `fade_mid` to half of it,
  `threshold` (dBFS, default -30) decides "quiet". Per-track
  `annotate:`/`cue_cut` fade overrides still win.

**DSP** (run inline in the pull chain):

- `amplify(src, gain)`, `compress(src, opts)`, `normalize(src, opts)`
- `replaygain(src, opts)` — per-track constant gain from the file's
  `REPLAYGAIN_TRACK_GAIN` tag (`REPLAYGAIN_ALBUM_GAIN` as fallback; MP3
  ID3v2 and Ogg Vorbis comments), clamped to ±`max_boost`/`max_cut` dB
  (default 12 each, unity when untagged). Compose
  `normalize(replaygain(src))` to feed AGC a consistent loudness baseline.
- `pipe({process, format = "s16le"|"s24le", restart_backoff = 500}, src)` —
  route the mix through an external raw-PCM processor (e.g. Thimeo Stereo
  Tool): the child is fed to the process's stdin and stdout is decoded back
  into the graph. If the process dies it is restarted after
  `restart_backoff` ms while audio bypasses to the unprocessed child, so the
  broadcast never drops. The child is *shared*, not consumed, so
  `mksafe(pipe(...))` composes.
- `output.preview(...)` — decode and mix without broadcasting

```lua
daytime = playlist({directory = "./media/day"})
overnight = playlist({directory = "./media/night"})
pl = switch({{when = {days = {"mon", "tue", "wed", "thu", "fri"},
                      from = "09:00", to = "17:00"}, src = daytime},
             {src = overnight}})
```

## Liquidsoap parity map

Approximate `.liq` → `.lua` equivalents. Everything in this table is
shipped and verified; the status of the rest of the project is tracked in
[ROADMAP.md](ROADMAP.md).

| Liquidsoap (.liq) | Crabsoup (.lua) |
| --- | --- |
| `playlist("dir")` | `playlist({directory = "./media", shuffle = true})` |
| `single("file")` | `single("path")` |
| `fallback([...])`, `sequence([...])` | `fallback({...})`, `sequence({...})` |
| `random([...])` | `random({...})` |
| `blank(duration)` | `blank({duration = 2.0})` |
| `sine(freq, duration)` | `sine({freq = 440, duration = 60, amplitude = 0.5})` |
| `amplify(src, gain)` | `amplify(src, 0.5)` |
| `compress(threshold, ratio, ...)`, `normalize(target, ...)` | `compress(src, {threshold = -12, ratio = 2})`, `normalize(src, {target = -13})` |
| replaygain (liq `amplify` + RG tags) | `replaygain(src, {max_boost = 6, max_cut = 6})` |
| `input.harbor(...)` | `input.harbor({...})` |
| `input.soundcard()` | `input.soundcard({device = nil})` — cpal capture bridged into the bus via an SPSC ring |
| `input.http(...)` | `input.http(url, {reconnect_backoff = 500})` — continuous relay/pull source, reconnects on drop, exhausts while disconnected so `fallback({relay, local})` composes |
| `output.icecast(...)` | `output.icecast({...}, src)` — multiple outputs share one source via the tap |
| `output.soundcard()` | `output.soundcard({device = nil}, src)` — tap consumer playing through the device |
| `output.file(...)` | `output.file({path, format}, src)` |
| `server.telnet(...)` | `server.telnet({port = 1234})` |
| telnet `skip` / `status` / `uptime` | same |
| live DJ ducking (`mksafe`/`switch` + request scheduling) | `input.harbor` + `PriorityMixer` ducking (harbor connect/disconnect drives the mixer) |
| one-shot jingles (`switch` + request scheduling) | `jingles({directory})` + telnet `jingles.play` |
| `request.queue` + telnet `queue.push` | same |
| `switch` (dayparting), `rotate` | `switch({ {when = {days, from, to}, src = ...}, {src = default} })`, `rotate({...}, {weights = ...})` |
| `on_metadata` / `on_track` | `on_metadata(src, fn)` (title table), `on_track(src, fn)` (boundary, no args) |
| `http://` / `https://` request resolution | `single("http(s)://...")`, `playlist(entries)`, `queue.push <url>` — download-then-play with retry/timeout, temp files auto-removed; HTTPS via rustls (redirects may cross scheme) |
| `server.register` custom telnet commands | `server.register("name", function(args) return reply end)` |
| `mksafe(src)` | `mksafe(src)` — composes `fallback({src, blank()})` |
| `add([...])` | `add({a, b}, {weights = {0.5, 1.0}})` — sample-wise sum, bed + voice-over |
| `request.dynamic(fn)` | `request.dynamic(function() return "media/track.mp3" end)` — callback-driven requests, nil ends |
| `annotate:` cue points + `cue_cut(src)` | `annotate:liq_cue_in="30",liq_cue_out="180":/path/track.mp3` on any request URI; `cue_cut(src, {cue_in, cue_out})` |
| per-track crossfade (`liq_fade_in`/`liq_fade_out`) | `annotate:liq_fade_in="2",liq_fade_out="3":...` or `cue_cut(src, {fade_in, fade_out})` — overrides `crossfade_seconds` per track |
| `smart_crossfade` (level-aware transitions) | `smart_crossfade({directory, fade_out, fade_mid, threshold})` — outgoing tail loudness picks the fade window |
| `pipe(process, src)` (external processor) | `pipe({process = "...", format = "s16le"\|"s24le", restart_backoff = 500}, src)` — stdin/stdout raw PCM bridge; bypass + restart on death |
| `blank.detect(src)` (dead-air detection) | `blank.detect(src, {threshold = -40, duration = 2, restart = 1, on_blank = fn})` — silence -> blank + exhausted so `fallback` hands over |
| `map_metadata(f, src)` (title rewrite) | `map_metadata(src, function(m) return {title = ...} end)` — rewritten title reaches the output; original kept on nil/error |

## Architecture

Implementation-level wiring (engine tap, threading model, gotchas) lives in
`docs/ARCHITECTURE.md`; this is the user-facing summary.

```
media/ + jingles/   (decoded via symphonia)
   ▼
root source ──► CrossfadeMixer ──► PriorityMixer ──► TAP (one puller thread)
                (track overlap)    (live/jingle       │  shared PCM + title
                                    override)         ├─► encoder → Icecast (MP3/Opus/AAC)
                                                      ├─► encoder → file / soundcard
                                                      ├─► HLS  (H.264 + AAC)
                                                      ├─► MP4  (AAC-LC ± H.264)
                                                      ├─► RTMP (FLV ± H.264)
                                                      └─► preview (broadcasts nowhere)
```

The script's root source (e.g. `fallback({jingle, live, playlist})`) becomes
the crossfade+priority chain's input; one puller thread feeds every output
from a shared tap, so a stalled output drops frames instead of stalling the
others. The `PriorityMixer` fades from the main source to an override (live
DJ or jingle) over `duck_seconds`; a live DJ always wins over a jingle.

Source layout:

| Path | Purpose |
| --- | --- |
| `src/main.rs` | CLI, wiring, preview mode |
| `src/script.rs` | Lua stdlib + script evaluation |
| `src/config.rs` | Configuration data types (filled by the script) |
| `src/control.rs` | Telnet control port (jingles, shutdown) |
| `src/source/` | `FileSource`, `Playlist` (scheduling), source composition |
| `src/source/pipe.rs` | `PipeSource` — external-process pipeline (`pipe`) |
| `src/engine/` | `CrossfadeMixer`, `PriorityMixer`, `MixCommand` |
| `src/live/` | DJ harbor (Icecast source protocol listener) |
| `src/output/` | `encoder.rs` (LAME/libopus/fdk-aac), `ogg_mux.rs`, `icecast_client.rs` (native source protocol), `icecast.rs` (pump + reconnect) |
| `src/request.rs` | `RequestUri` (local path or `http://` URL), download-then-play HTTP client, `RequestConfig` |

## Testing

```sh
cargo test --lib
cargo bench --bench engine   # hot-path baselines (mixers, resampler, effects, encoders)
```

Tests are inline `#[cfg(test)]` modules per source file. A few tests use real
files from `media/` and `jingles/` and skip when they are absent. The
`CRABSOUP_DUMP=/path/out.ogg` env var makes the Opus end-to-end test persist
the encoded stream for external inspection (ffprobe, curl to Icecast).

## Roadmap

See [ROADMAP.md](ROADMAP.md).

## License

AGPL-3.0