# Outputs

An output is where the engine's root source ends up. The pipeline is:

```
root source (script graph, crossfades inside) -> PriorityMixer -> TAP -> [outputs]*
```

One puller thread feeds every output from a shared tap, so a stalled output
drops frames instead of stalling the others. `PriorityMixer` fades from the
main source to an override (live DJ or jingle) over `duck_seconds`.

## `output.icecast({...}, src)`

Broadcasts the source to Icecast or SHOUTcast via native source protocols (no
libshout). The `protocol` key picks the server type: `"icecast"` (default),
`"shoutcast-v1"` (legacy ICY), or `"shoutcast-v2"` (`"shoutcast"` is an
alias for v2):

```lua
output.icecast({host = "localhost", port = 8000,
                mount = "/crabsoup.opus", format = "opus", bitrate = 128000,
                source_user = "source", source_password = "hackme",
                name = "Crabsoup", description = "Crabsoup stream",
                genre = "Various", reconnect = 5},
               fallback({j, live, pl}))
```

- `format` is `"mp3"`, `"opus"`, or `"aac"`; `bitrate` is in bits/s.
- For AAC, `aac_profile` selects the fdk-aac profile: `"lc"` (default for
  Icecast/Icy), `"he"` (SBR, the "AAC+" that SHOUTcast v2 uses by default),
  or `"heaacv2"` (SBR + parametric stereo — the efficient 64 kbit/s
  stereo option; stereo input only).
- Opus is always encoded at 48 kHz (the bus rate is never fed directly).
- The encoder writes one Ogg page per 20 ms Opus packet so audio reaches
  Icecast promptly.
- With `reconnect`, the output reconnects Icecast-reconnect style if the
  server drops the connection.
- For Opus mounts the title rides the initial OpusTags stream header (Icecast
  rejects URL metadata updates for Opus; 2.4.4 never parses Opus titles at
  all, 2.5+ only the stream-start one).

### SHOUTcast specifics

For SHOUTcast the output speaks the DNAS's legacy ICY source protocol — the
password as the first line plus `icy-*` headers — for both `shoutcast-v1` and
`shoutcast-v2`, because the DNAS v2 accepts ICY sources on both its source
ports and the native "uvox2" handshake is undocumented (and encrypted). The
DNAS replies with a bare `OK2` line. v1 is **MP3-only**; v2 adds **AAC**, sent
as HE-AAC ("AAC+") with the `audio/aacp` content type the SHOUTcast platform
expects. Track titles go out as `/admin.cgi?mode=updinfo` requests with the
source password — the mechanism ICY sources use — which the DNAS then
re-serves to listeners as in-stream metadata.

```lua
output.icecast({host = "radio.example.com", port = 8000,
                mount = "/", format = "mp3", bitrate = 128000,
                protocol = "shoutcast-v2",
                source_password = "changeme",
                name = "Crabsoup", genre = "Various", reconnect = 5},
               fallback({j, live, pl}))
```

- `mount` is ignored by v1; for v2 it is the stream path — `/` for the
  default stream or `/stream/N` for a named one (which selects that DNAS
  stream by appending `:#N` to the password, the DNAS's documented way for
  ICY sources to target a stream).
- `source_user` is ignored by both versions (the ICY protocol has no user
  concept).
- On a v2 DNAS, v1 sources connect to `portbase + 1` (e.g. 8001) while v2
  sources use `portbase` (8000); a standalone v1 DNAS uses its single port
  directly.
- AAC on v2 is HE-AAC (SBR) — a sensible `bitrate` for AAC+ is around
  32–96 kbps, not the 128+ typical of MP3. Verified end-to-end against
  DNAS 2.6.1: MP3 connects, populates `SONGTITLE`, and decodes cleanly for
  listeners. AAC connects and the DNAS sniffs it correctly, but this DNAS
  build corrupts the AAC relay to listeners (its legacy-path frame parser
  rewrites ADTS headers), so MP3 is the reliable choice until the native
  uvox2 protocol is supported.

## `output.preview(...)`

Decodes and mixes locally with no broadcast.

## `output.file({path, format}, src)`

Encodes the root source to a file (e.g. Opus) instead of broadcasting.

## `output.soundcard({device}, src)`

A tap consumer playing through the device via the same SPSC-ring pattern as
the harbor: its thread resamples into a reusable scratch and pushes into a
ring the device callback drains (silence on underrun). Device and stream open
at startup, so a missing device fails before the tap pulls.

## `output.hls({directory, segment_seconds, retention, ...}, src)`

Segments the source into an HLS (HTTP Live Streaming) playlist — the
"station on any device" output: any HLS player (iOS, Android, VLC, hls.js)
can play it over plain HTTP. Segments are AAC in MPEG-TS, rotated on
`segment_seconds` (default 5 s) with a sliding window of `retention`
(default 12) completed segments.

```lua
output.hls({directory = "/var/www/hls",
            segment_seconds = 5,
            retention = 12}, root)
```

- Each completed `seg-000000.ts` … is appended to `playlist.m3u8`
  (`#EXT-X-VERSION:3`, sliding `MEDIA-SEQUENCE`); on graceful shutdown the
  final segment closes and the list ends with `#EXT-X-ENDLIST`.
- `segment_name` (default `"seg-{n}.ts"`) is a template — `{n}` is the
  zero-padded sequence number, `{t}` the unix seconds of the segment's
  start. Timestamped names: `segment_name = "seg-{t}-{n}.ts"`.
- `persist_at = "/var/lib/crabsoup/hls-state.json"` makes runs resumable:
  the next segment counter and retained window are written on every
  rotation, so killing crabsoup mid-segment and restarting continues the
  playlist (no renumbering, no gap/drift) instead of clearing the
  directory. Delete the state file to force a fresh start.
- `fallible = true` keeps the engine alive with silence when the source
  exhausts (the root is wrapped in `fallback([root, blank()])`), so the
  playlist stays live instead of ending — the "station never goes off air"
  switch. Applies to every output sharing the root.
- Serve the directory with any static file server (nginx, caddy,
  `python3 -m http.server`) — HLS needs no server-side magic.

### Multi-rendition ABR (`renditions`)

Fan the one tap into several AAC encodes, each in its own subdirectory,
tied together by a variant master playlist — the 64/128/320 set a live
station serves so clients pick the rendition their bandwidth can carry:

```lua
output.hls({
    directory = "/var/www/hls",
    segment_seconds = 5,
    renditions = {
        {bitrate = 64000,  name = "64k"},
        {bitrate = 128000, name = "128k"},
        {bitrate = 320000, name = "320k"},
    },
}, root)
```

- Each rendition gets `<directory>/<name>/` with its own `playlist.m3u8`
  and segments; `index.m3u8` lists one `#EXT-X-STREAM-INF` per rendition
  (BANDWIDTH = bitrate + 10 % container overhead, CODECS `mp4a.40.2`,
  NAME = the rendition name). Point clients at `index.m3u8`. A rendition
  without an explicit `name` defaults to `<kbps>k`.
- Add `video_bitrate` (and optionally `width`/`height`) for a rendition
  with its own H.264 encode — see the [video guide](/guide/video).

Video sources (`video = marker`) and RTMP publishing are covered in the
[video guide](/guide/video); `output.rtmp`/`output.mp4` are documented
there too.

## Live DJ harbor vs. output

The `input.harbor` listener is the *input* side of a live DJ; the fade in and
out happens in `PriorityMixer`, driven by harbor connect/disconnect.