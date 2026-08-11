# Crabsoup

A Liquidsoap-inspired audio streaming engine in Rust: evaluates a `.lua`
script (Liquidsoap-flavoured Lua) that builds a source graph, mixes in live DJ
input and one-shot jingles with crossfades, and broadcasts the result (MP3 or
Opus) to an Icecast server.

- `.lua` scripting: real Lua with Liquidsoap-style functions — `playlist`,
  `smart_crossfade`, `single`, `blank`, `sine`, `amplify`, `compress`,
  `normalize`, `fallback`, `sequence`, `random`, `switch`, `rotate`,
  `jingles`, `mksafe`, `add`, `cue_cut`, `request.queue`, `request.dynamic`,
  `input.harbor`, `output.icecast`, `output.preview`, `server.telnet`,
  `set`, `log`
- Playlist scheduling: recursive directory scan, explicit file lists, loop and shuffle
- Gapless crossfades with configurable overlap and fade curve
- Live DJ harbor: an Icecast source-protocol listener (`PUT /live`); the
  playlist ducks out while a DJ is live and fades back in on disconnect
- Jingles: one-shot clips played over the music, triggered from a
  Liquidsoap-style telnet control port
- Output: MP3 (via LAME), Ogg/Opus (via libopus + a built-in Ogg muxer with
  spec-correct CRC-32), or AAC/ADTS (via fdk-aac)
- Graceful Ctrl-C shutdown

## Architecture

Implementation-level wiring (engine tap, threading model, gotchas) lives in
`docs/ARCHITECTURE.md`; this is the user-facing summary.

```
media/ + jingles/
   │   (decoded via symphonia)
   ▼
Playlist ──► CrossfadeMixer ──► PriorityMixer ──► Encoder ──► Icecast
               (track overlap)  (live/jingle     (LAME,      (native source
                                  override)       libopus+Ogg,  protocol)
                                                  or fdk-aac)
```

The script's root source (e.g. `fallback({jingle, live, playlist})`) becomes
the crossfade+priority chain's input. Every source is resampled/converted to a
shared PCM bus (`set("sample_rate", ...)`, `set("channels", ...)`). The
`PriorityMixer` fades from the main source to an override (live DJ or jingle)
over `duck_seconds`; a live DJ always wins over a jingle.

Source layout:

| Path | Purpose |
| --- | --- |
| `src/main.rs` | CLI, wiring, preview mode |
| `src/script.rs` | Lua stdlib + script evaluation |
| `src/config.rs` | Configuration data types (filled by the script) |
| `src/control.rs` | Telnet control port (jingles, shutdown) |
| `src/source/` | `FileSource`, `Playlist` (scheduling), source composition |
| `src/engine/` | `CrossfadeMixer`, `PriorityMixer`, `MixCommand` |
| `src/live/` | DJ harbor (Icecast source protocol listener) |
| `src/output/` | `encoder.rs` (LAME/libopus/fdk-aac), `ogg_mux.rs`, `icecast_client.rs` (native source protocol), `icecast.rs` (pump + reconnect) |
| `src/request.rs` | `RequestUri` (local path or `http://` URL), download-then-play HTTP client, `RequestConfig` |

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

Lua 5.4 is vendored and compiled at build time (needs a C compiler).

## Running

```sh
cp crabsoup.lua.example crabsoup.lua   # or write your own (see below)
RUST_LOG=crabsoup=info ./target/release/crabsoup -c crabsoup.lua
```

Per-format test scripts live in `examples/`:

```sh
./target/release/crabsoup -c examples/crabsoup.opus.lua    # Opus -> /crabsoup.opus
./target/release/crabsoup -c examples/crabsoup.mp3.lua     # MP3  -> /crabsoup.mp3
./target/release/crabsoup -c examples/crabsoup.aac.lua     # AAC  -> /crabsoup.aac
./target/release/crabsoup -c examples/crabsoup.preview.lua # no broadcast
```

With an `output.icecast` call it connects to Icecast as a source. Without one
it runs in preview mode (decodes and mixes, no broadcast). `crabsoup --check`
evaluates the script, prints the resolved configuration, and exits.

### Example script

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

Named options are passed as Lua tables; most have defaults. `format` is
`"mp3"` or `"opus"`. Composable sources: `playlist`, `single`, `jingles`,
`fallback`/`sequence`, `random` (non-repeating shuffle), `switch`
 (dayparting), `rotate` (weighted round-robin), `blank`/`sine`
 (test tones, both accept an optional `duration`), `mksafe(src)`
 (never fails outright — silence covers an exhausted or failed child),
 `add({a, b}, {weights = {...}})`
 (N-source sample-wise sum, optional per-source weights — background bed +
 voice-over), `cue_cut(src, {cue_in, cue_out, fade_in, fade_out})`
 (skip `cue_in` seconds into each track, end it at `cue_out`; a per-track
 `fade_in`/`fade_out` overrides the global `crossfade_seconds` for that
 track's crossfades),
 and
 the DSP operators `amplify(source, gain)`, `compress(source, opts)`,
 `normalize(source, opts)`
 (run inline in the pull chain). `replaygain(source, opts)` applies a
 per-track constant gain from the file's `REPLAYGAIN_TRACK_GAIN` tag
 (`REPLAYGAIN_ALBUM_GAIN` as fallback; MP3 ID3v2 and Ogg Vorbis comments),
 clamped to ±`max_boost`/`max_cut` dB (default 12 each, unity when
 untagged) — compose `normalize(replaygain(src))` to feed AGC the loudness
 baseline. `request.dynamic(function() return uri_or_nil end)` plays the
 requests its Lua callback returns, one ahead of the current track (nil
 ends the source) — a live-programming scheduler without a playlist file.
 `smart_crossfade({directory = ...})` is a `playlist` whose transition
 window is chosen by the outgoing track's measured tail level: a loud tail
 gets a full `fade_out` crossfade, a quiet tail only a short `fade_mid`
 fade (no point dragging a crossfade over silence; per-track
 `annotate:`/`cue_cut` fade overrides still win). `fade_out` defaults to
 `crossfade_seconds`, `fade_mid` to half of it, `threshold` (dBFS, default
 -30) decides "quiet".
 `output.preview(...)` runs without broadcasting.

Dayparting with `switch` — slots with a `when` predicate (weekday `days` as
names or 0-6, `from`/`to` in `"HH:MM"`, overnight windows wrap; `from == to`
never matches) are checked at each track boundary; the last slot must be a
default without `when`. `track_sensitive = false` re-checks every buffer and
cuts mid-track. `rotate({a, b}, {weights = {1, 2}})` holds a child for
`weights[n]` consecutive tracks.

```lua
daytime = playlist({directory = "./media/day"})
overnight = playlist({directory = "./media/night"})
pl = switch({{when = {days = {"mon", "tue", "wed", "thu", "fri"},
                      from = "09:00", to = "17:00"}, src = daytime},
             {src = overnight}})
```

### Control port

```sh
printf 'jingles.play\n' | nc localhost 1234   # random jingle
printf 'jingles.play trance\n' | nc localhost 1234  # by substring
printf 'jingles.list\n' | nc localhost 1234   # index + path per line
printf 'skip\n' | nc localhost 1234           # skip the current track
printf 'status\n' | nc localhost 1234         # current track + uptime
printf 'shutdown\n' | nc localhost 1234
```

Commands: `jingles.list`, `jingles.play [n|substr]`, `skip`, `status`,
`uptime`, `shutdown`, `exit`, `help`, plus any commands registered in the
script with `server.register("name", function(args) return reply end)` —
the handler receives the rest of the line as one string and its return
value is sent back; a Lua error becomes an `ERROR: ...` reply.

## Liquidsoap parity map

Approximate `.liq` → `.lua` equivalents, status tracked against
[ROADMAP.md](ROADMAP.md) (done = shipped and verified; planned = next-up
phase).

| Liquidsoap (.liq) | Crabsoup (.lua) | Status |
| --- | --- | --- |
| `playlist("dir")` | `playlist({directory = "./media", shuffle = true})` | done |
| `single("file")` | `single("path")` | done |
| `fallback([...])`, `sequence([...])` | `fallback({...})`, `sequence({...})` | done |
| `random([...])` | `random({...})` | done |
| `blank(duration)` | `blank({duration = 2.0})` | done |
| `sine(freq, duration)` | `sine({freq = 440, duration = 60, amplitude = 0.5})` | done |
| `amplify(src, gain)` | `amplify(src, 0.5)` | done |
| `compress(threshold, ratio, ...)`, `normalize(target, ...)` | `compress(src, {threshold = -12, ratio = 2})`, `normalize(src, {target = -13})` | done |
| replaygain (liq `amplify` + RG tags) | `replaygain(src, {max_boost = 6, max_cut = 6})` | done |
| `input.harbor(...)` | `input.harbor({...})` | done |
| `output.icecast(...)` | `output.icecast({...}, src)` | done (single output; multi-mount in Phase 4) |
| `output.file(...)` | `output.file({path, format}, src)` | planned (Phase 4) |
| `server.telnet(...)` | `server.telnet({port = 1234})` | done |
| telnet `skip` / `status` / `uptime` | same | done |
| live DJ ducking (`mksafe`/`switch` + request scheduling) | `input.harbor` + `PriorityMixer` ducking (harbor connect/disconnect drives the mixer) | done |
| one-shot jingles (`switch` + request scheduling) | `jingles({directory})` + telnet `jingles.play` | done |
| `request.queue` + telnet `queue.push` | done |
| `switch` (dayparting), `rotate` | `switch({ {when = {days, from, to}, src = ...}, {src = default} })`, `rotate({...}, {weights = ...})` | done |
| `on_metadata` / `on_track` | `on_metadata(fn, src)` (title table), `on_track(fn, src)` (boundary, no args) | done |
| `http://` request resolution | `single("http://...")`, `playlist(entries)`, `queue.push <url>` — download-then-play with retry/timeout, temp files auto-removed | done |
| `server.register` custom telnet commands | `server.register("name", function(args) return reply end)` | done |
| `mksafe(src)` (never fails outright; silence fallback) | `mksafe(src)` — composes `fallback({src, blank()})` | done |
| `add([...])` (N-source additive mix) | `add({a, b}, {weights = {0.5, 1.0}})` — sample-wise sum, bed + voice-over | done |
| `request.dynamic(fn)` | `request.dynamic(function() return "media/track.mp3" end)` — callback-driven requests, nil ends | done |
| `annotate:` cue points + `cue_cut(src)` | `annotate:liq_cue_in="30",liq_cue_out="180":/path/track.mp3` on any request URI; `cue_cut(src, {cue_in, cue_out})` | done |
| per-track crossfade (`liq_fade_in`/`liq_fade_out`) | `annotate:liq_fade_in="2",liq_fade_out="3":...` or `cue_cut(src, {fade_in, fade_out})` — overrides `crossfade_seconds` per track | done |
| `smart_crossfade` (level-aware transitions) | `smart_crossfade({directory, fade_out, fade_mid, threshold})` — outgoing tail loudness picks the fade window | done |

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

MIT
