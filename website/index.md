---
layout: home

hero:
  name: Crabsoup
  text: Streaming engine for audio, video, live DJ, and automation
  tagline: "Scriptable using Lua, gapless playlists, crossfades, live DJ ducking, with support for Icecast, HLS, RTMP, MP4, and more."
  actions:
    - theme: brand
      text: Get started
      link: /guide/getting-started
    - theme: alt
      text: View on GitHub
      link: https://github.com/sonyarianto/crabsoup

features:
  - title: .lua scripting
    details: Real Lua with Liquidsoap-style functions — playlist, single, fallback, switch, rotate, jingles, mksafe, add, cue_cut, pipe and more.
  - title: Gapless crossfades
    details: Track-to-track overlap with configurable window and fade curve, ending each fade at the track's audible tail via a BS.1770-gated fade point.
  - title: Live DJ ducking
    details: An Icecast source-protocol harbor (PUT /live). While a DJ is live the playlist ducks out, then fades back in on disconnect.
  - title: Jingles & control
    details: One-shot clips over the music and a Liquidsoap-style control port — telnet or HTTP — for jingles.play, skip, status, queues and custom commands.
  - title: Broadcast anywhere
    details: Icecast, SHOUTcast, HLS, RTMP, MP4, file, or soundcard — one source graph, every output at once, fed from a shared tap.
  - title: Video built in
    details: video.video, video.playlist and video.slideshow sources with video.scale/video.fade effects, muxed as H.264 + AAC in the same script.
  - title: Studio DSP
    details: External processors via pipe (Stereo Tool, etc.), plus reverb, EQ, filters, vocalremover, replaygain, and pitch/stretch — all in the pull chain.
  - title: Dead-air safe
    details: blank.detect silence guarding with fallback handover, mksafe never-fails sources, and request queues with retry and timeout.
---

## What it looks like

A station is a Lua script. This is the whole thing:

```lua
-- crabsoup.lua
set("sample_rate", 44100)
set("channels", 2)
set("crossfade_seconds", 3.0)    -- track-to-track overlap
set("duck_seconds", 1.5)         -- live DJ / jingle fade time

pl = playlist({directory = "./media", shuffle = false, loop = true})
j  = jingles({directory = "./jingles"})        -- telnet-triggered clips
live = input.harbor({port = 8005, mount = "/live", password = "dj"})
server.telnet({host = "127.0.0.1", port = 1234})

output.icecast({host = "localhost", port = 8000,
                mount = "/crabsoup.opus", format = "opus", bitrate = 128000,
                source_password = "hackme", name = "Crabsoup"},
               fallback({j, live, pl}))
```

`fallback` picks the first child that has audio — jingles while triggered,
live DJ while connected, playlist otherwise. The [getting started
guide](/guide/getting-started) walks through the pieces.