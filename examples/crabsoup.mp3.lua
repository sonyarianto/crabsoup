-- Crabsoup test script: MP3 broadcast.
--
-- Run: ./target/release/crabsoup -c examples/crabsoup.mp3.lua
-- Verify: ffprobe http://localhost:8000/crabsoup.mp3

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

output.icecast({host = "localhost", port = 8000,
                mount = "/crabsoup.mp3", format = "mp3", bitrate = 192000,
                source_user = "source", source_password = "hackme",
                name = "Crabsoup", description = "Crabsoup MP3 test stream",
                genre = "Test", reconnect = 5},
               fallback({j, live, pl}))
