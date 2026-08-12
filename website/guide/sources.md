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

## `input.harbor({...})`

The live DJ harbor — an Icecast source-protocol listener:

```lua
live = input.harbor({host = "0.0.0.0", port = 8005,
                     mount = "/live", password = "dj"})
```

DJs `PUT` their stream to this mount; the playlist ducks out while they are
live and fades back in on disconnect. MP3/Vorbis/AAC uploads decode via
symphonia; Opus takes the native decode path. The value composes as a marker
in `fallback`.

## `input.soundcard({device})`

cpal capture bridged into the bus via an SPSC ring. The device is opened
synchronously at script evaluation, so a missing/broken device fails fast
(`--check` is hardware-dependent for scripts that use it).

## `smart_crossfade({...})`

A `playlist` whose transition window is chosen by the outgoing track's
measured tail level: a loud tail gets a full `fade_out` crossfade, a quiet
tail only a short `fade_mid` fade (no point dragging a crossfade over
silence; per-track `annotate:`/`cue_cut` fade overrides still win):

```lua
smart_crossfade({directory = "./media",
                 fade_out = 3.0, fade_mid = 1.5, threshold = -30})
```

`fade_out` defaults to `crossfade_seconds`, `fade_mid` to half of it,
`threshold` (dBFS, default -30) decides "quiet".

## `add({a, b}, {weights = {0.5, 1.0}})`

N-source sample-wise sum with optional per-source weights — background bed +
voice-over. Exhausts only when every child exhausts, so a looping bed keeps a
finite voice-over mix alive.

## `cue_cut(src, {cue_in, cue_out, fade_in, fade_out})`

Skips `cue_in` seconds into each track and ends it at `cue_out`; per-track
`fade_in`/`fade_out` overrides the global `crossfade_seconds` for that
track's crossfades. The window re-arms on every track boundary.

Next: [DSP & metadata operators](/guide/dsp).