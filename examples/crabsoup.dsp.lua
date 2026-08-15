-- Crabsoup test script: DSP chain (compress -> normalize -> amplify).
--
-- Exercises the Phase 2 effect operators end-to-end on a synthetic tone,
-- no media files needed. Broadcast as `tone-dsp` or preview locally.
-- `pitch`/`stretch` (Part I1, pure-Rust wsola), `echo` (Part I2),
-- `reverb` (Part I3, convolution), `eq`/`filter` (Part I4, biquads) and
-- `stereo` (Part I5, pan + mid-side width) are shown in the alternate
-- chains below — uncomment to exercise them.
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

-- tone = pitch(tone, {semitones = -2})   -- key down a whole tone
-- tone = stretch(tone, {ratio = 1.25})   -- 25 % faster, pitch intact
-- tone = echo(tone, {delay = 0.25, ping = 0.4, feedback = 0.35,
--                    delay2 = 0.5, ping2 = 0.2})  -- multi-tap echo
-- tone = reverb(tone, {ir = "./media/hall.wav", wet = 0.3, dry = 0.7})
-- tone = eq(tone, {bands = {{type = "lowpass", freq = 15000},
--                           {type = "peaking", freq = 1000, gain = 3, q = 1.0}}})
-- tone = filter(tone, {type = "highpass", freq = 80})
-- tone = stereo(tone, {pan = -0.25, width = 1.4})
-- tone = stereo.widen(tone, 1.4)

output.icecast({host = "localhost", port = 8000,
                mount = "/tone-dsp.mp3", format = "mp3", bitrate = 128000,
                source_password = "hackme", name = "Crabsoup DSP test"},
               tone)
