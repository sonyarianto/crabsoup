-- Crabsoup test script: decode + mix locally, no broadcast.
--
-- Exercises the full chain (playlist, crossfade, jingles, harbor ducking)
-- without an Icecast server or network I/O.
--
-- Run: ./target/release/crabsoup -c examples/crabsoup.preview.lua

set("sample_rate", 44100)
set("channels", 2)
set("frames_per_buffer", 4096)   -- ~93 ms at 44100 Hz

set("crossfade_seconds", 3.0)
set("fade_curve", 1.0)
set("duck_seconds", 1.5)

pl = playlist({directory = "./media", shuffle = false, loop = true})
j = jingles({directory = "./jingles"})
live = input.harbor({host = "0.0.0.0", port = 8005,
                     mount = "/live", password = "dj"})
server.telnet({host = "127.0.0.1", port = 1234})

output.preview(fallback({j, live, pl}))
