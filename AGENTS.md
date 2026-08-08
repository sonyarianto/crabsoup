# AGENTS.md

Guidance for AI agents working on this repository.

## Project

Crabsoup: Rust audio streaming engine (Rust 2024 edition) — gapless playlist,
crossfades, live DJ ducking, one-shot jingles, MP3/Opus broadcast to Icecast.
See `README.md` for the architecture and user-facing docs.

## Commands

```sh
cargo build                    # debug build
cargo build --release          # release build
cargo test --lib               # run all tests (inline #[cfg(test)] modules)
cargo clippy -- -D warnings    # lint
./target/release/crabsoup -c crabsoup.yaml          # run
./target/release/crabsoup --check                  # validate config and exit
RUST_LOG=crabsoup=info ./target/release/crabsoup   # run with logging
```

`crabsoup.yaml` is gitignored; `crabsoup.yaml.example` is the tracked
reference config. `media/` and `jingles/` are gitignored — audio files stay
local, tests that use them skip when absent.

## Conventions

- Rust 2024 edition; no `unsafe` outside FFI (`src/output/shout.rs`,
  `src/output/encoder.rs` LAME bindings).
- Error type is `crate::Result<T, E = Box<dyn Error + Send + Sync>>`; string
  errors via `.into()` are the norm in FFI and IO paths.
- Logging via `log` + `env_logger`; use `log::info/warn/error` with module
  context, never `println` in library code.
- Tests are inline `#[cfg(test)] mod tests` per file. Prefer real-file tests
  that skip gracefully (`if !path.exists() { return; }`).
- `CRABSOUP_DUMP=/path/out.ogg` env var persists the Opus end-to-end test's
  encoded stream for external inspection (ffprobe, curl to Icecast).
- Keep `ROADMAP.md` in sync: move verified work to the Done section.
- Do not add comments unless they explain non-obvious behavior.

## Architecture notes

Pipeline: `Playlist -> CrossfadeMixer -> PriorityMixer -> Encoder -> libshout
-> Icecast`. All sources are normalised to the PCM bus (`stream.sample_rate`,
`stream.channels`, `frames_per_buffer`).

- `MixCommand` (`src/engine/mixer.rs`) is the mixer control channel
  (`SetLive`, `ClearLive`, `PlayJingle(PathBuf)`, `Shutdown`) over
  `std::sync::mpsc`. The harbor and control port send into it.
- `PriorityMixer` crossfades between `main` and an override with a gain ramp
  over `duck_seconds`; the override audio is `m*(1-gain) + o*gain`.
- Opus path: `LinearResampler` bus -> 48 kHz, encode 20 ms frames, mux one
  Ogg page per packet, flush per packet so audio reaches Icecast promptly.
- Pump pacing in `src/output/icecast.rs` is wall-clock based:
  `next_due_us = frames_pulled * 1_000_000 / sample_rate`.

## Critical gotchas

- Ogg CRC is CRC-32/MPEG-2 (MSB-first, init 0, poly 0x04c11db7, no final
  xor). The input byte xors into the table **index**
  (`idx = ((crc >> 24) ^ b) & 0xff`), not into the result. A previous bug
  corrupted every page silently; `crc_matches_external_reference` guards it.
- Opus requires 48 kHz sample rate — never feed it the bus rate directly.
- icecast2 on localhost:8000 is the usual end-to-end target (source/admin
  password `hackme`). Verify on-air streams with `ffprobe` against the mount;
  note the mount reconnects with the usual libshout 401-then-200 double
  login (benign).
