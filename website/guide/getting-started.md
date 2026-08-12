# Getting started

Crabsoup is a Liquidsoap-inspired audio streaming engine in Rust: it evaluates
a `.lua` script (Liquidsoap-flavoured Lua) that builds a source graph, mixes in
live DJ input and one-shot jingles with crossfades, and broadcasts the result
(MP3, Opus, or AAC) to an Icecast server.

## Features

- **`.lua` scripting**: real Lua with Liquidsoap-style functions —
  `playlist`, `smart_crossfade`, `single`, `blank`, `sine`, `amplify`,
  `compress`, `normalize`, `pipe`, `fallback`, `sequence`, `random`, `switch`,
  `rotate`, `jingles`, `mksafe`, `add`, `cue_cut`, `request.queue`,
  `request.dynamic`, `blank.detect`, `map_metadata`, `input.harbor`,
  `input.soundcard`, `output.icecast`, `output.preview`, `output.soundcard`,
  `server.telnet`, `set`, `log`
- **Playlist scheduling**: recursive directory scan, explicit file lists,
  loop and shuffle
- **Gapless crossfades** with configurable overlap and fade curve
- **Live DJ harbor**: an Icecast source-protocol listener (`PUT /live`); the
  playlist ducks out while a DJ is live and fades back in on disconnect
- **Jingles**: one-shot clips played over the music, triggered from a
  Liquidsoap-style telnet control port
- **Output**: MP3 (via LAME), Ogg/Opus (via libopus + a built-in Ogg muxer
  with spec-correct CRC-32), or AAC/ADTS (via fdk-aac)
- **Graceful Ctrl-C shutdown**

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
cp crabsoup.lua.example crabsoup.lua   # or write your own
RUST_LOG=crabsoup=info ./target/release/crabsoup -c crabsoup.lua
```

With an `output.icecast` call it connects to Icecast as a source. Without one
it runs in preview mode (decodes and mixes, no broadcast).
`crabsoup --check` evaluates the script, prints the resolved configuration,
and exits.

Per-format test scripts live in `examples/`:

```sh
./target/release/crabsoup -c examples/crabsoup.opus.lua    # Opus -> /crabsoup.opus
./target/release/crabsoup -c examples/crabsoup.mp3.lua     # MP3  -> /crabsoup.mp3
./target/release/crabsoup -c examples/crabsoup.aac.lua     # AAC  -> /crabsoup.aac
./target/release/crabsoup -c examples/crabsoup.pipe.lua    # external processor pipeline (preview)
./target/release/crabsoup -c examples/crabsoup.preview.lua # no broadcast
```

## Testing

```sh
cargo test --lib
cargo bench --bench engine   # hot-path baselines (mixers, resampler, effects, encoders)
```

Tests are inline `#[cfg(test)]` modules per source file. A few tests use real
files from `media/` and `jingles/` and skip when they are absent. The
`CRABSOUP_DUMP=/path/out.ogg` env var makes the Opus end-to-end test persist
the encoded stream for external inspection (ffprobe, curl to Icecast).

Next: the [example script](/guide/example-script).

## Development status

Feature work is tracked in the repo's
[ROADMAP.md](https://github.com/sonyarianto/crabsoup/blob/main/ROADMAP.md) —
shipped and verified phases are marked done; the open item (Shoutcast v1/v2)
is planned only if a concrete need shows up.