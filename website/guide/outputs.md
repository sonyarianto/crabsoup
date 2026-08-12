# Outputs

An output is where the engine's root source ends up. The pipeline is:

```
Playlist ──► CrossfadeMixer ──► PriorityMixer ──► Encoder ──► Icecast
               (track overlap)  (live/jingle     (LAME,
                                 override)        libopus+Ogg,
                                                  or fdk-aac)
```

## `output.icecast({...}, src)`

Broadcasts the source to Icecast via the native source protocol (no libshout —
one authenticated `SOURCE` request, then raw encoded bytes; titles go out on
separate `/admin/metadata` GETs for MP3/AAC):

```lua
output.icecast({host = "localhost", port = 8000,
                mount = "/crabsoup.opus", format = "opus", bitrate = 128000,
                source_user = "source", source_password = "hackme",
                name = "Crabsoup", description = "Crabsoup stream",
                genre = "Various", reconnect = 5},
               fallback({j, live, pl}))
```

- `format` is `"mp3"`, `"opus"`, or `"aac"`; `bitrate` is in bits/s.
- Opus is always encoded at 48 kHz (the bus rate is never fed directly).
- The encoder writes one Ogg page per 20 ms Opus packet so audio reaches
  Icecast promptly.
- With `reconnect`, the output reconnects Icecast-reconnect style if the
  server drops the connection.
- For Opus mounts the title rides the initial OpusTags stream header (Icecast
  rejects URL metadata updates for Opus; 2.4.4 never parses Opus titles at
  all, 2.5+ only the stream-start one).

## `output.preview(...)`

Decodes and mixes locally with no broadcast.

## `output.file({path, format}, src)`

Encodes the root source to a file (e.g. Opus) instead of broadcasting.

## `output.soundcard({device}, src)`

A tap consumer playing through the device via the same SPSC-ring pattern as
the harbor: its thread resamples into a reusable scratch and pushes into a
ring the device callback drains (silence on underrun). Device and stream open
at startup, so a missing device fails before the tap pulls.

## Live DJ harbor vs. output

The `input.harbor` listener is the *input* side of a live DJ; the fade in and
out happens in `PriorityMixer`, driven by harbor connect/disconnect.