-- Crabsoup test script: file -> MP4 recording (H.264 + AAC).
--
-- Video build only: cargo build --release --features video
-- Run: ./target/release/crabsoup -c examples/crabsoup.mp4.lua
-- Verify:
--   ffprobe -show_entries stream=codec_type,codec_name,width,height /tmp/out.mp4
--   ffmpeg -v error -i /tmp/out.mp4 -f null -
--
-- The audio side of the files flows through the normal audio graph; the
-- video side rides its own decode thread and both interleave in the MP4
-- by PTS. Keep both playlists over the same files, in the same order,
-- for a/v sync. Drop `video = vpl` and the `vpl` line for audio-only.

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

output.mp4({file = "/tmp/out.mp4", bitrate = 128000, video = vpl}, root)