# Example script

Crabsoup scripts are plain Lua with Liquidsoap-flavoured names; `{key = value}`
tables are how named options are passed:

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
`"mp3"` or `"opus"`.

## What's happening

1. `set()` fixes the PCM bus spec — every source is resampled/converted to
   this before mixing.
2. The `playlist` scans `./media` recursively for audio files. A `jingles`
   source stages one-shot clips for the control port.
3. `input.harbor` opens an Icecast source-protocol listener on port 8005; a
   DJ who `PUT`s to `/live` ducks the playlist out via the mixer.
4. `server.telnet` exposes the control port on 1234.
5. The engine's **root source** is `fallback({j, live, pl})`: it switches to
   the first child that still has audio — jingles first, then a live DJ, then
   the playlist. The engine wraps it in a `PriorityMixer` and feeds a shared
   tap that every output consumes.

The script's root source becomes the engine's input.
`PriorityMixer` fades from the main source to an override (live DJ or jingle)
over `duck_seconds`; a live DJ always wins over a jingle.

## Configuration table

| Key | Default | Meaning |
| --- | --- | --- |
| `sample_rate` | 44100 | PCM bus rate |
| `channels` | 2 | PCM bus channels |
| `frames_per_buffer` | 4096 | Pull buffer size (~93 ms at 44.1 kHz) |
| `crossfade_seconds` | 3.0 | Track-to-track overlap window |
| `fade_curve` | 1.0 | 1.0 linear, 2.0 equal-ish power |
| `duck_seconds` | 1.5 | Fade time into/out of a live DJ or jingle |
| `request_timeout` | | HTTP download connect+read timeout |
| `request_retries` | | HTTP download retries |

## Decoding locally

To decode + mix without broadcasting, replace the `output.icecast` call with:

```lua
output.preview(fallback({j, live, pl}))
```

Next: the [sources reference](/guide/sources).