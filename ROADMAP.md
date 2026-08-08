# Crabsoup roadmap

## Done (verified end-to-end)
- [x] YAML config (`-c crabsoup.yaml`)
- [x] Playlist scheduling (directory scan, loop, shuffle)
- [x] Gapless crossfades (`crossfade_seconds`, `fade_curve`)
- [x] MP3 -> Icecast broadcast, 192 kbps, title metadata
- [x] Graceful Ctrl-C shutdown
- [x] Unit tests (37), incl. resampler multi-chunk regression
- [x] Opus end-to-end: encoder + Ogg mux -> Icecast verified live (ffprobe decodes
      mount, listeners get audio). Fixed Ogg page CRC bug (byte must xor into
      the table index, not into the result) that silently corrupted every page.
- [x] Playlist auto-advance + crossfade verified live (track 2 loads at t=3:32)
- [x] Ogg checksum regression test vs. external reference value
- [x] Jingles: playable via the telnet control port (`jingles.list`, `jingles.play
      [n|substr]`, `shutdown`). Verified end-to-end with the 3 staged jingles.
- [x] `.lua` scripting replaces YAML entirely: Liquidsoap-flavoured Lua stdlib
      (`playlist`, `single`, `jingles`, `fallback`/`sequence`/`random`,
      `input.harbor`, `output.icecast`, `output.preview`, `server.telnet`, `set`,
      `log`) evaluated by mlua (vendored Lua 5.4). Scripts define everything:
      stream/mixer settings, sources, services, output. Verified live with
      `crabsoup.lua.example` (Opus mount, telnet jingle trigger).

## Next up
- [ ] Live DJ harbor end-to-end verification (PUT a real stream at `/live`, check auto-duck on connect/disconnect)
- [ ] Opus stream title: Icecast rejects admin URL title updates for Opus mounts; libshout's in-stream OpusTags update didn't appear mid-stream during a 4-minute soak. Decide: insert OpusTags comment page ourselves, or accept title-less Opus
- [ ] The 401-then-200 double login on every connect (libshout sends an unauthenticated probe first) — cosmetic, benign
- [ ] Preview mode via `--preview`? (currently only by omitting `output.icecast` in the script)
- [ ] Opus resampler is linear-interp only (48 kHz fixed); note: Opus path resamples bus -> 48 kHz
