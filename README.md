# Crabsoup

A Liquidsoap-inspired audio streaming engine in Rust: schedules a gapless
playlist, mixes it with live DJ input and one-shot jingles using crossfades,
and broadcasts the result (MP3 or Opus) to an Icecast server.

- Playlist scheduling: recursive directory scan, explicit file lists, loop and shuffle
- Gapless crossfades with configurable overlap and fade curve
- Live DJ harbor: an Icecast source-protocol listener (`PUT /live`); the
  playlist ducks out while a DJ is live and fades back in on disconnect
- Jingles: one-shot clips played over the music, triggered from a
  Liquidsoap-style telnet control port
- Output: MP3 (via LAME) or Ogg/Opus (via libopus + a built-in Ogg muxer with
  spec-correct CRC-32)
- YAML configuration, graceful Ctrl-C shutdown

## Architecture

```
media/ + jingles/
   │   (decoded via symphonia)
   ▼
Playlist ──► CrossfadeMixer ──► PriorityMixer ──► Encoder ──► libshout ──► Icecast
               (track overlap)  (live/jingle     (LAME or     (icecast source
                                 override)        libopus+Ogg)  protocol)
```

Every source is resampled/converted to a shared PCM bus (`stream.sample_rate`,
`stream.channels`). The `PriorityMixer` fades from the playlist to an override
(live DJ or jingle) over `duck_seconds`; a live DJ always wins over a jingle.

Source layout:

| Path | Purpose |
| --- | --- |
| `src/main.rs` | CLI, wiring, preview mode |
| `src/config.rs` | YAML config types + file resolution |
| `src/control.rs` | Telnet control port (jingles, shutdown) |
| `src/source/` | `FileSource`, `Playlist` (scheduling), `PcmConverter` |
| `src/engine/` | `CrossfadeMixer`, `PriorityMixer`, `MixCommand` |
| `src/live/` | DJ harbor (Icecast source protocol listener) |
| `src/output/` | `encoder.rs` (LAME/libopus), `ogg_mux.rs`, `shout.rs` (libshout FFI), `icecast.rs` (pump + reconnect) |

## Building

Requires Rust (edition 2024) and the dev packages for the native codecs:

```sh
sudo apt install libmp3lame-dev libopus-dev   # Debian/Ubuntu
cargo build --release
```

## Running

```sh
cp crabsoup.yaml.example crabsoup.yaml   # or write your own (see below)
RUST_LOG=crabsoup=info ./target/release/crabsoup -c crabsoup.yaml
```

With an `output:` section it connects to Icecast as a source. Without one it
runs in preview mode (decodes and mixes, no broadcast). `crabsoup --check`
validates and prints the resolved config.

### Example config

```yaml
stream:
  sample_rate: 44100
  channels: 2
  frames_per_buffer: 4096

mixer:
  crossfade_seconds: 3.0    # track-to-track overlap
  fade_curve: 1.0           # 1.0 linear, 2.0 equal-ish power
  duck_seconds: 1.5         # live DJ / jingle fade time

playlist:
  directory: ./media
  loop_playlist: true
  shuffle: false

output:
  host: localhost
  port: 8000
  mount: /crabsoup.ogg
  source_user: source
  source_password: hackme
  format: opus              # mp3 | opus
  bitrate: 128000
  name: Crabsoup

live:                       # optional DJ harbor
  host: 0.0.0.0
  port: 8005
  mount: /live
  password: dj

jingles:                    # optional one-shot clips
  directory: ./jingles

control:                    # optional telnet port
  host: 127.0.0.1
  port: 1234
```

### Control port

```sh
printf 'jingles.play\n' | nc localhost 1234   # random jingle
printf 'jingles.play trance\n' | nc localhost 1234  # by substring
printf 'jingles.list\n' | nc localhost 1234   # index + path per line
printf 'shutdown\n' | nc localhost 1234
```

Commands: `jingles.list`, `jingles.play [n|substr]`, `shutdown`, `exit`, `help`.

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
