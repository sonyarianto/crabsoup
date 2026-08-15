-- Crabsoup test script: file -> RTMP (H.264 + AAC).
--
-- RTMP build only: cargo build --release --features rtmp,video
-- Run: ./target/release/crabsoup -c examples/crabsoup.rtmp.lua
-- Verify (nginx-rtmp on localhost:1935):
--   timeout 10 rtmpdump -r rtmp://localhost/live/stream -o /tmp/cap.flv
--   ffprobe /tmp/cap.flv
--
-- The audio side of the files flows through the normal audio graph; the
-- video side rides its own decode thread and both interleave in the FLV
-- by PTS. Keep both playlists over the same files, in the same order,
-- for a/v sync. Drop `video = vpl` and the `vpl` line for audio-only
-- (a plain `--features rtmp` build).

set("sample_rate", 44100)
set("channels", 2)
set("frames_per_buffer", 4096)   -- ~93 ms at 44100 Hz

set("crossfade_seconds", 3.0)
set("fade_curve", 1.0)
set("duck_seconds", 1.5)

pl = playlist({directory = "./media/video", shuffle = false, loop = true})

-- Video side: same files as `pl`, same order, same loop setting.
vpl = video.playlist({directory = "./media/video", shuffle = false, loop = true})

root = fallback({pl})

output.rtmp({url = "rtmp://localhost/live/stream", bitrate = 128000,
             video = vpl}, root)