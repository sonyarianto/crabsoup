-- Crabsoup test script: broadcast a synthetic test tone, no media files.
--
-- Exercises `blank`, `sine`, and `amplify` end-to-end. With the default
-- 60 s tone the stream can be checked with ffprobe against the mount.
--
-- Run: ./target/release/crabsoup -c examples/crabsoup.tone.lua

set("sample_rate", 44100)
set("channels", 2)
set("frames_per_buffer", 4096)

set("crossfade_seconds", 3.0)
set("fade_curve", 1.0)
set("duck_seconds", 1.5)

server.telnet({host = "127.0.0.1", port = 1234})

-- 60 s of 440 Hz at half scale, then fall through to silence (loop).
tone = amplify(sine({freq = 440, duration = 60, amplitude = 0.5}), 0.8)
rest = blank({duration = 30})

output.icecast({host = "localhost", port = 8000,
                mount = "/tone.mp3", format = "mp3", bitrate = 128000,
                source_password = "hackme", name = "Crabsoup test tone"},
               sequence({tone, rest}))
