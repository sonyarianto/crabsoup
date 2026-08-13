-- Crabsoup test script: SHOUTcast broadcast (v1 or v2).
--
-- Run: ./target/release/crabsoup -c examples/crabsoup.shoutcast.lua
-- Verify: ffprobe http://localhost:8000/  (v2 default stream path)
--
-- `protocol` is "shoutcast-v1" (legacy ICY, MP3 only) or "shoutcast-v2"
-- ("shoutcast" is an alias for v2). v2 also accepts format = "aac", which
-- streams HE-AAC ("AAC+") with the audio/aacp content type.
--
-- Both versions speak the DNAS's legacy ICY source protocol (password line +
-- icy-* headers); the native v2 "uvox2" handshake is undocumented/encrypted.
-- Titles go out via /admin.cgi?mode=updinfo. Verified against DNAS 2.6.1:
-- MP3 works end-to-end, but this DNAS build corrupts AAC listener relays, so
-- MP3 is the reliable format.
--
-- Ports on a v2 DNAS: v2 sources connect to `portbase` (8000), v1 sources to
-- `portbase + 1` (8001).

set("sample_rate", 44100)
set("channels", 2)
set("frames_per_buffer", 4096)   -- ~93 ms at 44100 Hz

set("crossfade_seconds", 3.0)
set("fade_curve", 1.0)
set("duck_seconds", 1.5)

pl = playlist({directory = "./media", shuffle = false, loop = true})
j = jingles({directory = "./jingles"})
live = input.harbor({host = "0.0.0.0", port = 8005,
                     mount = "/live", password = "dj"})
server.telnet({host = "127.0.0.1", port = 1234})

-- SHOUTcast v2, MP3. For v2, `mount` is the stream path: "/" for the
-- default stream or "/stream/N" for a named one.
output.icecast({host = "localhost", port = 8000, mount = "/",
                format = "mp3", bitrate = 128000, protocol = "shoutcast-v2",
                source_password = "hackme",
                name = "Crabsoup", description = "Crabsoup SHOUTcast test",
                genre = "Test", reconnect = 5},
               fallback({j, live, pl}))

-- AAC+ (HE-AAC) variant for v2 — note DNAS 2.6.1 corrupts AAC listener
-- relays (verified), so MP3 is the reliable SHOUTcast format:
-- output.icecast({host = "localhost", port = 8000, mount = "/",
--                 format = "aac", bitrate = 64000, protocol = "shoutcast-v2",
--                 source_password = "hackme",
--                 name = "Crabsoup", genre = "Test", reconnect = 5},
--                fallback({j, live, pl}))

-- Legacy v1 variant (MP3 only; on a v2 DNAS use portbase + 1):
-- output.icecast({host = "localhost", port = 8001, mount = "/",
--                 format = "mp3", bitrate = 128000, protocol = "shoutcast-v1",
--                 source_password = "hackme",
--                 name = "Crabsoup", genre = "Test", reconnect = 5},
--                fallback({j, live, pl}))
