# Crabsoup

A Liquidsoap-inspired audio streaming engine in Rust: evaluates a `.lua`
script (Liquidsoap-flavoured Lua) that builds a source graph, mixes in live DJ
input and one-shot jingles with crossfades, and broadcasts the result (MP3 or
Opus) to an Icecast server.

- `.lua` scripting: real Lua with Liquidsoap-style functions — `playlist`,
  `single`, `blank`, `sine`, `amplify`, `compress`, `normalize`, `fallback`,
  `sequence`, `random`, `switch`, `rotate`, `jingles`, `input.harbor`,
  `output.icecast`, `output.preview`, `server.telnet`, `set`, `log`
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
(test tones, both accept an optional `duration`), and the DSP operators
`amplify(source, gain)`, `compress(source, opts)`, `normalize(source, opts)`
(run inline in the pull chain). `output.preview(...)` runs without
broadcasting.

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
`uptime`, `shutdown`, `exit`, `help`.

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
| `input.harbor(...)` | `input.harbor({...})` | done |
| `output.icecast(...)` | `output.icecast({...}, src)` | done (single output; multi-mount in Phase 4) |
| `output.file(...)` | `output.file({path, format}, src)` | planned (Phase 4) |
| `server.telnet(...)` | `server.telnet({port = 1234})` | done |
| telnet `skip` / `status` / `uptime` | same | done |
| live DJ ducking (`mksafe`/`switch` + request scheduling) | `input.harbor` + `PriorityMixer` ducking (harbor connect/disconnect drives the mixer) | done |
| one-shot jingles (`switch` + request scheduling) | `jingles({directory})` + telnet `jingles.play` | done |
| `request.queue` + telnet `queue.push` | planned (Phase 3) | planned |
| `switch` (dayparting), `rotate` | `switch({ {when = {days, from, to}, src = ...}, {src = default} })`, `rotate({...}, {weights = ...})` | done |
| `on_metadata` / `on_track` | `on_metadata(fn, src)` (title table), `on_track(fn, src)` (boundary, no args) | done |
| `http://` request resolution | planned (Phase 7) | planned |

## Testing

```sh
cargo test --lib
```

Tests are inline `#[cfg(test)]` modules per source file. A few tests use real
files from `media/` and `jingles/` and skip when they are absent. The
`CRABSOUP_DUMP=/path/out.ogg` env var makes the Opus end-to-end test persist
the encoded stream for external inspection (ffprobe, curl to Icecast).

## Roadmap

See [ROADMAP.md](ROADMAP.md).

## License

MIT
