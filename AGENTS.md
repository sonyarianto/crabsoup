# AGENTS.md

Guidance for AI agents working on this repository. Implementation details
live in `docs/ARCHITECTURE.md`; the plan and ship history in `ROADMAP.md`.

## Project

Crabsoup: Rust audio streaming engine (Rust 2024 edition) — gapless playlist,
crossfades, live DJ ducking, one-shot jingles, MP3/Opus/AAC broadcast to Icecast.

## Commands

```sh
cargo build                    # debug build
cargo build --release          # release build
cargo test --lib               # run all tests (inline #[cfg(test)] modules)
cargo clippy -- -D warnings    # lint
./target/release/crabsoup -c internal/scripts/crabsoup.lua   # run (dev scripts live in internal/scripts/)
./target/release/crabsoup --check                  # evaluate script, print config, exit
RUST_LOG=crabsoup=info ./target/release/crabsoup   # run with logging
```

`crabsoup.lua.example` is the tracked reference script; local dev/test scripts
live in `internal/scripts/` (gitignored). `internal/` (media/, jingles/, tools/)
is gitignored — audio files stay local, tests
that use them skip when absent.

## Conventions

- Rust 2024 edition; no `unsafe` outside FFI (`src/output/encoder.rs`
  LAME bindings).
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

## Critical gotchas

Short list that has burned previous work — full context in
`docs/ARCHITECTURE.md`.

- Ogg CRC is CRC-32/MPEG-2 (MSB-first, init 0, poly 0x04c11db7, no final
  xor). The input byte xors into the table **index**
  (`idx = ((crc >> 24) ^ b) & 0xff`), not into the result;
  `crc_matches_external_reference` guards it.
- Opus requires 48 kHz sample rate — never feed it the bus rate directly.
- The `on_metadata` closure stays in the Lua registry for the process
  lifetime, keeping its channel `Sender` alive: `run_event_loop` must exit on
  the shared end-flag the tap sets when the engine stops, never on channel
  disconnection.
- icecast2 on localhost:8000 is the usual end-to-end target (source/admin
  password `hackme`). Verify on-air streams with `ffprobe` against the mount.
