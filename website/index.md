---
layout: home

hero:
  name: Crabsoup
  text: Streaming engine for audio, video, live DJ, and automation
  tagline: "A scriptable streaming engine for audio and video: gapless playlists, crossfades, live DJ ducking, with support for Icecast, HLS, RTMP, MP4, and more."
  actions:
    - theme: brand
      text: Get started
      link: /guide/getting-started
    - theme: alt
      text: View on GitHub
      link: https://github.com/sonyarianto/crabsoup

features:
  - title: .lua scripting
    details: Real Lua with Liquidsoap-style functions — playlist, smart_crossfade, fallback, switch, rotate, jingles, mksafe, add, cue_cut, pipe and more.
  - title: Gapless crossfades
    details: Track-to-track overlap with configurable window and fade curve, plus a level-aware smart_crossfade that picks the window from the outgoing track's tail.
  - title: Live DJ ducking
    details: An Icecast source-protocol harbor (PUT /live). While a DJ is live the playlist ducks out, then fades back in on disconnect.
  - title: One-shot jingles
    details: Clips played over the music, triggered from a Liquidsoap-style telnet control port (jingles.play, jingles.list).
  - title: Broadcast to Icecast
    details: MP3 (LAME), Ogg/Opus (libopus + a built-in spec-correct Ogg muxer), and AAC/ADTS (fdk-aac) via a native source-protocol client.
  - title: Dead-air safe
    details: blank.detect silence guarding with fallback handover, mksafe never-fails sources, and request queues with retry and timeout.
---