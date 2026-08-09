-- Crabsoup test script: DSP chain (compress -> normalize -> amplify).
--
-- Exercises the Phase 2 effect operators end-to-end on a synthetic tone,
-- no media files needed. Broadcast as `tone-dsp` or preview locally.
--
-- Run: ./target/release/crabsoup -c examples/crabsoup.dsp.lua --preview

set("sample_rate", 44100)
set("channels", 2)
set("frames_per_buffer", 4096)

set("crossfade_seconds", 3.0)
set("fade_curve", 1.0)
set("duck_seconds", 1.5)

server.telnet({host = "127.0.0.1", port = 1234})

tone = sine({freq = 440, duration = 60, amplitude = 1.0})
tone = compress(tone, {threshold = -12, ratio = 3, attack = 0.005, release = 0.1})
tone = normalize(tone, {target = -6, attack = 3, release = 0.5})
tone = amplify(tone, 0.9)

output.icecast({host = "localhost", port = 8000,
                mount = "/tone-dsp.mp3", format = "mp3", bitrate = 128000,
                source_password = "hackme", name = "Crabsoup DSP test"},
               tone)
