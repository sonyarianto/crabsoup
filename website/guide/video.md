# Video

Video support turns a Crabsoup install into a file → HLS(video) engine: a
video source feeds a decode thread that paces frames to their PTS on a
shared fan-out tap, and `output.hls({video = ...})` muxes those frames —
encoded live to H.264 — into the same MPEG-TS segments as the audio. The
result is a master playlist (`index.m3u8`) plus the usual per-segment
media playlist, all keyframe-aligned so players can join mid-stream.

Video is compiled in with the `video` cargo feature and needs the FFmpeg
dev packages at build time (they are pulled via pkg-config):

```sh
sudo apt install libavcodec-dev libavformat-dev libavutil-dev libswscale-dev
cargo build --release --features video
```

Everything else (audio decode, mixing, Icecast) is unchanged and works in
a plain build — video is purely additive.

## Video sources

Three operators register video tracks. Each one validates its files at
script evaluation (fail fast) and returns an opaque marker table for
`output.hls`.

```lua
video.video("media/clip.mp4")            -- one file, plays once
video.single("media/clip.mp4")           -- same, registered as a sequence

vpl = video.playlist({directory = "./media/video", shuffle = true})  -- loops by default
vpl = video.playlist({files = {"a.mp4", "b.mp4"}, loop = false})     -- plays once
```

- `video.playlist` mirrors the audio `playlist`: a recursive `directory`
  scan (mp4, mov, mkv, webm, ts, …) and/or an explicit `files` list,
  `shuffle` and `loop` (default `true`). Tracks play one at a time on a
  single decode thread with a **continuous PTS timeline** — no jump back
  when the next file starts.
- **All tracks in one playlist must share a resolution**: video outputs
  open their encoders at the first track's spec, and a differently-sized
  frame would kill the encode. Unreadable files are skipped with a
  warning; a playlist with no valid files fails the script.
- `video.single(path)` is a one-track playlist that never loops.

### The audio side

The audio of each video file is **not** played by these operators — the
audio graph is untouched. Feed it separately, over the same files:

```lua
pl  = playlist({directory = "./media/video", loop = true})  -- audio side
vpl = video.playlist({directory = "./media/video", loop = true})  -- video side
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
