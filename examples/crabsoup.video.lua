-- Crabsoup test script: file -> HLS(video).
--
-- Video build only: cargo build --release --features video
-- Run: ./target/release/crabsoup -c examples/crabsoup.video.lua
-- Verify:
--   ffprobe -show_entries stream=codec_type,codec_name /var/www/hls/seg-000000.ts
--   ffprobe http://localhost/example.com/hls/index.m3u8
--
-- The audio side of the files flows through the normal audio graph; the
-- video side rides its own decode thread and both interleave in the
-- segments by PTS. Keep both playlists over the same files, in the same
-- order, for a/v sync.

set("sample_rate", 44100)
set("channels", 2)
set("frames_per_buffer", 4096)   -- ~93 ms at 44100 Hz

set("crossfade_seconds", 3.0)
set("fade_curve", 1.0)
set("duck_seconds", 1.5)

pl = playlist({directory = "./media/video", shuffle = false, loop = true})
j = jingles({directory = "./jingles"})
live = input.harbor({host = "0.0.0.0", port = 8005,
                     mount = "/live", password = "dj"})
server.telnet({host = "127.0.0.1", port = 1234})

-- Video side: same files as `pl`, same order, same loop setting.
vpl = video.playlist({directory = "./media/video", shuffle = false, loop = true})

-- Slideshow variant: swap `video = vpl` for `video = ss` below to stream
-- still images instead of a video playlist. Images are decoded once at
-- script evaluation and crossfade over transition_seconds.
-- ss = video.slideshow({directory = "./art", seconds_per_image = 5,
--                        transition = "fade", transition_seconds = 1})

root = fallback({j, live, pl})

-- Serve /var/www/hls with any web server; clients point at index.m3u8
-- (the master playlist), which references playlist.m3u8 (the media
-- playlist) and the seg-*.ts segments.
output.hls({directory = "/var/www/hls", video = vpl,
            segment_seconds = 4, retention = 12}, root)
