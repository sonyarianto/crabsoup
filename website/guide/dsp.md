# DSP & metadata operators

These run inline in the pull chain — DSP effects stay inline, nothing
finer-grained than one thread per output plus one puller thread.

## `amplify(source, gain)`

Constant gain, e.g. `amplify(src, 0.5)`.

## `compress(source, opts)`

Compressor: `compress(src, {threshold = -12, ratio = 2})`.

## `normalize(source, opts)`

AGC: `normalize(src, {target = -13})`.

## `replaygain(source, opts)`

Per-track constant gain from the file's `REPLAYGAIN_TRACK_GAIN` tag
(`REPLAYGAIN_ALBUM_GAIN` as fallback; MP3 ID3v2 and Ogg Vorbis comments),
clamped to ±`max_boost`/`max_cut` dB (default 12 each, unity when untagged).
Compose `normalize(replaygain(src))` to feed AGC the loudness baseline:

```lua
replaygain(src, {max_boost = 6, max_cut = 6})
```

## `pipe({process, format, restart_backoff}, src)`

Runs an external raw-PCM processor (e.g. Thimeo Stereo Tool — Liquidsoap's
own `pipe`). A writer thread feeds the child source to the process's stdin
as little-endian PCM (`format = "s16le"` or `"s24le"`); a reader thread
decodes stdout back into the graph. Not lock-step — a bounded queue decouples
the two streams and backpressure paces the child at the consumption rate.

If the process dies it is restarted after `restart_backoff` ms
(Icecast-reconnect style) while audio bypasses to the unprocessed child, so
the broadcast never drops. The child is *shared*, not consumed, so
`mksafe(pipe(...))` composes. A `-k "<LICENSE>"`-style argument is visible in
`ps aux` — expected for shelling out to a licensed binary.

```lua
pipe({process = "stereotool ... -", format = "s16le"}, src)
```

## `blank.detect(src, {...})`

The dead-air guard. Watches the wrapped source's RMS level; after `duration`
seconds (default 2) of sub-`threshold` silence (default -40 dBFS) it goes
blank — by default reporting exhausted so a `fallback` around it hands over
automatically. `on_blank` fires a Lua callback once per episode and the source
recovers when audio returns:

```lua
blank.detect(src, {threshold = -40, duration = 2, restart = 1,
                   on_blank = function() log("dead air!") end})
```

## `map_metadata(src, fn)`

Rewrites each track's title through a Lua callback before it reaches the
output — the original is kept on nil/error/timeout:

```lua
map_metadata(src, function(m) return {title = "Artist - " .. m.title} end)
```

## `on_metadata(src, fn)` / `on_track(src, fn)`

Fire-and-forget Lua hooks: `on_metadata` gets the track's title table;
`on_track` fires at any track boundary.

## Request URIs: `annotate:` and `http(s)://`

Any request URI can carry Liquidsoap-style cue points and fade overrides:

```
annotate:liq_cue_in="30",liq_cue_out="180":/path/track.mp3
annotate:liq_fade_in="2",liq_fade_out="3":http://example.com/track.opus
```

`http://` / `https://` requests are download-then-play with retry/timeout;
temp files are auto-removed. HTTPS uses rustls (redirects may cross scheme).