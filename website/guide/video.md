# Video

Video support turns a Crabsoup install into a file → HLS(video), file →
RTMP, and file → MP4 recording engine: a video source feeds a decode thread
that paces frames to their PTS on a shared fan-out tap, and
`output.hls({video = ...})` muxes those frames — encoded live to H.264 —
into the same MPEG-TS segments as the audio. The result is a master
playlist (`index.m3u8`) plus the usual per-segment media playlist, all
keyframe-aligned so players can join mid-stream. `output.rtmp({video =
...})` muxes the same frames into an FLV stream (H.264 + raw AAC) and
publishes it to an RTMP server such as nginx-rtmp. `output.mp4({video =
...})` muxes them into a seekable MP4 recording.

Video is compiled in with the `video` cargo feature and needs the FFmpeg
dev packages at build time (they are pulled via pkg-config):

```sh
sudo apt install libavcodec-dev libavformat-dev libavutil-dev libswscale-dev
cargo build --release --features video
```

RTMP additionally needs the `rtmp` feature and `librtmp-dev`:

```sh
sudo apt install librtmp-dev
cargo build --release --features rtmp,video    # or just --features rtmp for audio-only
```

Everything else (audio decode, mixing, Icecast) is unchanged and works in
a plain build — video is purely additive.

## Video sources

Four operators register video tracks. Each one validates its files at
script evaluation (fail fast) and returns an opaque marker table for
`output.hls`.

```lua
video.video("internal/media/clip.mp4")            -- one file, plays once
video.single("internal/media/clip.mp4")           -- same, registered as a sequence

vpl = video.playlist({directory = "./internal/media/video", shuffle = true})  -- loops by default
vpl = video.playlist({files = {"a.mp4", "b.mp4"}, loop = false})     -- plays once

ss = video.slideshow({directory = "./art", seconds_per_image = 5,
                      transition = "fade", transition_seconds = 1})
```

- `video.playlist` mirrors the audio `playlist`: a recursive `directory`
  scan (mp4, mov, mkv, webm, ts, …) and/or an explicit `files` list,
  `shuffle` and `loop` (default `true`). Tracks play one at a time on a
  single decode thread with a **continuous PTS timeline** — no jump back
  when the next file starts.
- `video.slideshow` plays still images (jpg, png, webp, bmp, gif, tif)
  instead of video files: each image is shown for `seconds_per_image`
  (default 5 s) at `fps` (default 25), optionally crossfading into the
  previous picture over `transition_seconds` with `transition = "fade"`
  (default `"none"`). The pictures are decoded once at script evaluation,
  so the render thread only re-publishes them — a slideshow cannot fail
  mid-run.
- **All tracks in one playlist (or slideshow) must share a resolution**:
  video outputs open their encoders at the first track's spec, and a
  differently-sized frame would kill the encode. Unreadable files are
  skipped with a warning; a sequence with no valid files fails the script.
- `video.single(path)` is a one-track playlist that never loops.

### Effects (Part H3): `video.scale`, `video.fade`

Both operators wrap any `video.*` marker and return an updated marker
(they compose, in any order):

```lua
src   = video.video("internal/media/clip.mp4")
scaled = video.scale({width = 1280, height = 720}, src)
faded  = video.fade({fade_in = 2, fade_out = 3}, scaled)

-- or straight from a playlist / slideshow:
vpl = video.fade({fade_in = 1},
                 video.scale({width = 640, height = 360},
                             video.playlist({directory = "./internal/media/video"})))
```

- `video.scale({width, height}, marker)` rescales to the target size
  (odd dimensions round up to even — YUV420P chroma is half resolution).
  The marker's `width`/`height` update, so outputs encode at the scaled
  size.
- `video.fade({fade_in, fade_out}, marker)` fades to/from black over the
  first/last N seconds of the source's timeline (both optional; seconds).
  For playlists and slideshows the fade-out anchors on each track's
  duration (playlists) or the whole show's length (slideshows); looping
  sources have no end, so `fade_out` is ignored for them.
- Effects run on the source's render thread (scale first, then fade), so
  every output — HLS, RTMP, or MP4 — gets the processed frames.

### The audio side

The audio of each video file is **not** played by these operators — the
audio graph is untouched. Feed it separately, over the same files:

```lua
pl  = playlist({directory = "./internal/media/video", loop = true})  -- audio side
vpl = video.playlist({directory = "./internal/media/video", loop = true})  -- video side
```

Run both lists in the same order and the two sides stay in sync; a/v sync
is held per-file by PTS at mux time, exactly as with a single `video.video`
track.

## `output.hls` with video

Pass any video source's marker to `output.hls` and the output becomes a
video segmenter:

```lua
output.hls({directory = "/var/www/hls", video = vpl, segment_seconds = 4}, root)
```

- Segments start on an **IDR** (the cut is deferred until the video track
  holds a keyframe), so every `seg-*.ts` is independently joinable —
  verified with ffprobe in the test suite.
- A variant master playlist `index.m3u8` is written next to
  `playlist.m3u8`, describing the stream as H.264 baseline + AAC-LC at the
  first track's resolution. Point clients at `index.m3u8`.
- The H.264 encoder is H.264/AVC baseline profile, `ultrafast`,
  zero-latency, closed-GOP with scene-cut detection off — a regular
  keyframe cadence driven only by segment rotation.

## `output.rtmp` (H.264 + AAC over RTMP)

Pass any video source's marker to `output.rtmp` to publish an FLV stream —
video optional, audio is always AAC-LC:

```lua
-- audio-only
output.rtmp({url = "rtmp://localhost/live/stream", bitrate = 128000}, root)

-- h264 + aac
output.rtmp({url = "rtmp://localhost/live/stream", video = vpl,
             bitrate = 128000}, root)
```

- `url` is required (`rtmp://host/app/stream`); `format` accepts only
  `"aac"` (raw AAC in the FLV AAC container — no ADTS).
- `reconnect` (default 5 s) is the retry interval if the server is down
  or drops the connection; the output waits in a loop like `output.icecast`.
- The video side is identical to HLS: subscribed to the shared `VideoTap`,
  encoded to H.264 baseline, held until the audio clock catches up, and
  muxed with a FLV AVCDecoderConfigurationRecord + 4-byte-length NALs.
- A sequence header (AAC config / AVCDCR) is sent once at stream start so
  players can join mid-stream.
- Live streams end abruptly (no FLV tail) — that is the RTMP live model.

A minimal nginx-rtmp server for local testing (`/etc/nginx/rtmp.conf`,
included after the modules-enabled include in `nginx.conf`):

```nginx
rtmp {
    server {
        listen 1935;
        application live { live on; record off; }
    }
}
```

Verify a running stream with `rtmpdump` (the `librtmp` CLI):

```sh
timeout 10 rtmpdump -r rtmp://localhost/live/stream -o /tmp/cap.flv
ffprobe -show_entries stream=codec_name,width,height /tmp/cap.flv
```

## `output.mp4` (H.264 + AAC recording)

Pass any video source's marker to `output.mp4` to record the tap into a
seekable MP4 (video optional, audio is always AAC-LC); `file` is required:

```lua
-- audio-only recording
output.mp4({file = "shows/tonight.mp4", bitrate = 128000}, root)

-- h264 + aac recording
output.mp4({file = "shows/tonight.mp4", video = vpl,
            bitrate = 128000}, root)
```

- The file is opened at script start (a bad path fails fast) and finalized
  with the `moov` trailer when the tap ends or on shutdown — the recording
  is always seekable.
- The video side is identical to HLS/RTMP: subscribed to the shared
  `VideoTap`, encoded to H.264 baseline, held until the audio clock
  catches up. A periodic forced keyframe (~2 s) keeps long recordings
  seekable.
- The container is FFmpeg's `mov` muxer via ffmpeg-next, fed raw AAC
  access units (no ADTS) plus length-prefixed H.264 — the same elementary
  streams HLS and RTMP carry, so a recording and a live stream stay in
  sync.

Verify a recording the way the tests do:

```sh
ffprobe -show_entries stream=codec_type,codec_name,width,height show.mp4
ffmpeg -v error -i show.mp4 -f null -     # decodes without errors
```

## Example

`examples/crabsoup.video.lua` is a complete file → HLS(video) script.

Verify a running stream the same way the tests do:

```sh
ffprobe -show_entries stream=codec_type,codec_name /var/www/hls/seg-000000.ts
ffprobe http://localhost/example.com/hls/index.m3u8
```

## Limitations

- The HLS encoder is opened once at the **first** registered track's spec;
  `video.video` tracks that differ in resolution from each other are not
  rotated — use one track, or a same-resolution playlist.
- Audio and video playlists are independent; drift between the two lists
  shows up as a/v skew, so keep the file order identical.
