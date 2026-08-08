# Crabsoup roadmap

## Done (verified end-to-end)
- [x] YAML config (`-c crabsoup.yaml`)
- [x] Playlist scheduling (directory scan, loop, shuffle)
- [x] Gapless crossfades (`crossfade_seconds`, `fade_curve`)
- [x] MP3 -> Icecast broadcast, 192 kbps, title metadata
- [x] Graceful Ctrl-C shutdown
- [x] Unit tests (50), incl. resampler multi-chunk regression, Ogg CRC, Opus tags
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
- [x] Native Icecast source protocol replaces libshout (`icecast_client.rs`):
      single authenticated `SOURCE` request, raw encoded bytes, separate
      `/admin/metadata` GETs for titles. No libshout dependency, no capability
      negotiation, no 401-then-200 double login. Verified live: one `SOURCE`
      request per connect (HTTP 200), title updates reach the mount (status-json
      shows the track), ffprobe decodes the stream.

- [x] Live DJ harbor end-to-end: PUT + Basic auth (200 OK), mixer duck control
      verified (connect/disconnect events; broadcast RMS dips to ~25% then
      recovers full level). Caveat: symphonia 0.5/0.6 has no Opus codec, so
      MP3 uploads decode and air, while Opus uploads log "cannot create
      decoder: unsupported codec" and air silence for the ducked window.

- [x] Opus stream title: investigated to the source. Icecast 2.4.4 never parses
      OpusTags titles (format_opus.c counts header packets only) and rejects URL
      metadata updates for Opus mounts (HTTP 200 + "Mountpoint will not accept
      URL updates"); Icecast 2.5+/master parses only the initial OpusTags header
      (type-less packets only; set_tag is NULL for Ogg there too). So the Opus
      encoder sends OpusHead+OpusTags stream headers containing the first
      track's title (replaced via set_title before the first flush), and never
      injects mid-stream comment pages (Icecast forwards those to listeners as
      audio). Verified live: ffprobe reads `title=` from the stream's first
      OpusTags; MP3 keeps live URL titles; icecast 2.4.4 status-json stays
      title-less for Opus (documented server limitation).

## Known limitations
- DJ uploads must be MP3 for now (symphonia has no Opus codec; 0.6.0 feature
  list confirmed ogg/vorbis/mp3 but no opus). A native Opus decode path
  (audiopus + our ogg demuxer) is future work.
- Icecast 2.4.4 shows no Opus titles (see Done section); 2.5+ shows the
  stream-start title only.

## Next up
- [ ] Preview mode via `--preview`? (currently only by omitting `output.icecast` in the script)
- [x] Opus resampler upgraded: 16-tap Hann-windowed sinc polyphase FIR (256 phases),
      DC-normalized per output sample so chunk edges and stream edges stay
      unity-gain; `PcmConverter` (bus normalization) uses the same filter.
