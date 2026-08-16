# Sources

Composable sources are the heart of a Crabsoup script. Every source is
resampled/converted to the shared PCM bus (`set("sample_rate", ...)`,
`set("channels", ...)`) before mixing.

## `playlist({...})`

Recursively scanned directory and/or explicit file lists, with loop and
shuffle:

```lua
pl = playlist({directory = "./media", shuffle = false, loop = true})
```

## `single("path")`

Plays one file (or `http(s)://` URL) once.

## `jingles({directory})`

One-shot clips played over the music, triggered from the
[telnet control port](/guide/control-port):

```lua
j = jingles({directory = "./jingles"})
```

## `fallback({...})` / `sequence({...})`

`fallback` switches to the first child that still has audio (the dead-air
handover used everywhere); `sequence` plays children one after another.

```lua
root = fallback({j, live, pl})
```

## `random({...})`

Non-repeating shuffle of its children.

## `switch({...})` — dayparting

Slots with a `when` predicate (weekday `days` as names or 0-6, `from`/`to`
in `"HH:MM"`, overnight windows wrap; `from == to` never matches) are checked
at each track boundary; the last slot must be a default without `when`.
`track_sensitive = false` re-checks every buffer and cuts mid-track.

```lua
daytime = playlist({directory = "./media/day"})
overnight = playlist({directory = "./media/night"})
pl = switch({{when = {days = {"mon", "tue", "wed", "thu", "fri"},
                      from = "09:00", to = "17:00"}, src = daytime},
             {src = overnight}})
```

## `rotate({...}, {weights = {1, 2}})`

Holds a child for `weights[n]` consecutive tracks — a weighted round-robin.

## `blank({duration})` / `sine({freq, duration, amplitude})`

Test tones and silence. Both accept an optional `duration`. `blank` is also
a *callable table* carrying `blank.detect` (see [DSP & metadata
operators](/guide/dsp)).

## `mksafe(src)`

Never fails outright: silence covers an exhausted or failed child. Composes
`fallback({src, blank()})`.

## `request.queue` and `queue.push`

Play requests pushed over the telnet port (`queue.push <uri>`) — the
Liquidsoap-style request queue.

## `request.dynamic(fn)`

Plays the requests its Lua callback returns, one ahead of the current track
(nil ends the source) — a live-programming scheduler without a playlist file:

```lua
request.dynamic(function() return "media/track.mp3" end)
```

The next URI is requested as soon as a track is promoted, so a fast callback
gives gapless handovers. Unresolvable requests are skipped and re-asked.

## `http_get(url)` — control-plane GET

Fetches a URL and returns the response body as a string (synchronous,
16 MiB cap, raises on failure — wrap in `pcall` for transient daemon
hiccups). Paired with `json.parse` and `request.dynamic` it drives remote
playlists that never live on disk, e.g. a Deezer playlist served by a local
"Deezco" downloader daemon:

```lua
deezco = "http://127.0.0.1:9001"
playlist_id = "1234567890"
songs = request.dynamic(function()
    local ok, t = pcall(function()
        return json.parse(http_get(deezco .. "/playlists/" .. playlist_id .. "/next"))
    end)
    if not ok or t == nil or t.url == nil then return nil end
    return "annotate:title=\"" .. t.title .. "\":" .. t.url
end)
jr = crossfade(rotate({songs, jingles_src}, {weights = {3, 1}}), {duration = 3.0})
```

Each `http(s)://` URL is downloaded to a temp file, played, and the file is
deleted when the track ends — only the current track plus the prefetched
next one ever touch the disk, whatever the playlist size.

## `input.harbor({...})`

The live DJ harbor — an Icecast source-protocol listener:

```lua
live = input.harbor({host = "0.0.0.0", port = 8005,
                     mount = "/live", password = "dj",
                     extra_passwords = {"alice", "bob"}})
```

DJs `PUT` their stream to this mount; the playlist ducks out while they are
live and fades back in on disconnect. MP3/Vorbis/AAC uploads decode via
symphonia; Opus takes the native decode path. The value composes as a marker
in `fallback`.

`extra_passwords` (optional) adds per-streamer accounts: any password in
the list authenticates on the shared mount alongside `password`, so each DJ
can have their own credentials. The on-air state is visible on the [control
port](/guide/control-port) — `status` shows `live: true|false` and
`json status` adds `"harbor_connected"`.

## `input.soundcard({device})`

cpal capture bridged into the bus via an SPSC ring. The device is opened
synchronously at script evaluation, so a missing/broken device fails fast
(`--check` is hardware-dependent for scripts that use it).

## `input.http(url, {reconnect_backoff = 500})`

A continuous relay/pull-stream source — Liquidsoap's `input.http`: the
stream at `url` is `GET`ed and decoded live (MP3/Opus/Vorbis/AAC, format
sniffed from the stream with the response `Content-Type` as a hint), so it
keeps playing indefinitely while the upstream is up and reconnects with a
backoff when the connection drops:

```lua
relay = input.http("https://feed.example.net/affiliate.mp3")
output.preview(fallback({relay, playlist({directory = "./media"})}))
```

While disconnected the relay reports exhausted, so a `fallback` around it
plays the local source in the gap and the relay preempts it the moment it
reconnects — the "syndicated feed during the day, local automation
overnight" shape, with no script-side handling. `reconnect_backoff` is the
milliseconds between attempts (default 500). The connection timeout follows
`set("request_timeout", N)` (default 30 s).

## `crossfade(src, {duration, curve})`

A top-level overlap crossfade over the consecutive tracks of any source
(Liquidsoap's `crossfade`): a delay ring holds the outgoing track's tail and
blends it with the incoming track's head at every label change. Pair with
plain children — `playlist({..., crossfade = false})` (no internal
fade/preload) — inside `rotate`/`fallback` for the classic radio recipe:

```lua
output.preview(crossfade(rotate({songs, jingles}, {weights = {3, 1}}),
                         {duration = 3.0}))
```

No start delay and no tail replay; `duration` defaults to
`crossfade_seconds`, `curve` to `fade_curve`. The fade window ends at the
outgoing track's audible tail, found with BS.1770-style gating (absolute
−70 dBFS, then −10 LU below the track's own gated loudness), so quiet
tracks and encoder-noise decays fade exactly where they stop being audible —
this replaces the level-aware `smart_crossfade` operator, which was removed.

## `add({a, b}, {weights = {0.5, 1.0}})`

N-source sample-wise sum with optional per-source weights — background bed +
voice-over. Exhausts only when every child exhausts, so a looping bed keeps a
finite voice-over mix alive.

## `cue_cut(src, {cue_in, cue_out, fade_in, fade_out})`

Skips `cue_in` seconds into each track and ends it at `cue_out`; per-track
`fade_in`/`fade_out` overrides the global `crossfade_seconds` for that
track's crossfades. The window re-arms on every track boundary.

Next: [DSP & metadata operators](/guide/dsp).