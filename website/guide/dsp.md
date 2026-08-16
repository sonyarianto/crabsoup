# DSP & metadata operators

These run inline in the pull chain — DSP effects stay inline, nothing
finer-grained than one thread per output plus one puller thread.

## `amplify(source, gain)`

Constant gain, e.g. `amplify(src, 0.5)`.

## `compress(source, opts)`

Compressor: `compress(src, {threshold = -12, ratio = 2})`.

## `normalize(source, opts)`

AGC: `normalize(src, {target = -13})`.

## `pitch(source, opts)` / `stretch(source, opts)`

Time/pitch effects (Part I, pure-Rust WSOLA — no FFI): `pitch` shifts
semitones, `stretch` changes the tempo ratio. Pitch is WSOLA tempo by
`1/2^(s/12)` then a resample step read back from the internal
interpolator, so pitch-shifting a track keeps its duration:

```lua
pitch(src, {semitones = -2})   -- a whole tone down
stretch(src, {ratio = 1.25})   -- 25 % faster, pitch intact
```

## `echo(source, opts)`

Multi-tap echo/feedback: `{delay, ping, feedback}` plus optional
`delay2/ping2` and `delay3/ping3` for a second and third tap (up to 8 s
of delay storage, e.g. `echo(src, {delay = 0.25, ping = 0.4, feedback =
0.35})`).

## `reverb(source, {ir, wet, dry})`

Convolution reverb over an impulse-response file (`ir` is required).
Uniformly partitioned overlap-save with zero added latency — reverb of a
transient starts exactly at the transient:

```lua
reverb(src, {ir = "./media/hall.wav", wet = 0.3, dry = 0.7})
```

## `eq(source, {bands})` / `filter(source, opts)`

RBJ-cookbook biquads (Direct Form 1): `eq` chains `bands` per channel in
series, `filter` is a single band. Types: `lowpass`, `highpass`,
`bandpass`, `notch`, `peaking`, `lowshelf`, `highshelf`; `freq` must be
below Nyquist, `q > 0`, peaking/shelves take `gain` in dB:

```lua
eq(src, {bands = {{type = "lowpass", freq = 15000},
                  {type = "peaking", freq = 1000, gain = 3, q = 1.0}}})
filter(src, {type = "highpass", freq = 80})
```

## `stereo(source, {pan, width})` (+ `stereo.pan`, `stereo.widen`)

Balance panning and mid-side width. Pan is a *balance* — the channel the
image moves toward stays at unity, the far channel fades on a cos/sin
quarter wave — so `pan = 0` is an exact passthrough and ±1 hard-cuts to
one channel. Width is mid-side: 1 passes, 0 collapses to mono, > 1
widens. `stereo` is a callable table, so the method forms compose:

```lua
stereo(src, {pan = -0.25, width = 1.4})
stereo.widen(src, 1.4)
```

## `vocalremover(source, {strength, crossover})`

The karaoke trick: cancels the centre channel (vocals) above the
`crossover` (default 150 Hz) while keeping the low band as a mono sum,
so the bass survives. `strength` 0 is an exact passthrough, 1 is full
centre-cancel:

```lua
vocalremover(src, {strength = 1, crossover = 150})
```

## `bpm(path)` / `key(path)`

Offline analysis (pure-Rust, no FFI): `bpm()` returns the tempo in BPM
(spectral-flux onsets + autocorrelation), `key()` returns a
`"A major"`-style name (chromagram correlated with Krumhansl–Kessler
profiles):

```lua
tempo = bpm("./media/track.mp3")   -- e.g. 123.4
k = key("./media/track.mp3")       -- e.g. "A major"
```

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

## `on_metadata(src, fn)` / `on_track(src, fn)` / `on_next_metadata(src, fn)`

Fire-and-forget Lua hooks: `on_metadata` gets the track's title table;
`on_track` fires at any track boundary. `on_next_metadata` gets the
*upcoming* track's title table before it starts — the engine already knows
it from the crossfade preload or the next queued request:

```lua
on_next_metadata(src, function(m) print("up next: " .. m.title) end)
```

## Lua `json` helpers

`json.stringify(value)` / `json.parse(text)` convert between Lua values and
JSON — handy for machine-readable side files (now/next-playing.txt) and
telnet handlers:

```lua
on_metadata(src, function(m)
    json.stringify({title = m.title, next = next_title})
end)
```

## Request URIs: `annotate:` and `http(s)://`

Any request URI can carry per-track annotations — cue points, crossfade
fades, gain (linear or dB), and an earlier start for the next track:

```
annotate:cue_in="30",cue_out="180":/path/track.mp3
annotate:fade_in="2",fade_out="3":http://example.com/track.opus
annotate:amplify="0.7":/path/quiet.mp3
annotate:amplify="-8.2 dB":/path/loud.mp3
annotate:start_next="5":/path/next-begins-early.mp3
```

`start_next` overrides how early the next track begins (the crossfade
margin) for that track — e.g. to ramp the next track in over a 5 s outro
instead of the global window. The fade window compresses to fit when the
track ends sooner.

Per-track followers (`append`/`prepend`) are set the same way — either
per request, or for every track of a source with the `annotated`
operator:

```
annotate:append="jingles/stinger.mp3":/path/track.mp3
annotate:append="false":/path/no-stinger.mp3
src = annotated(playlist({...}), {append = "jingles/stinger.mp3",
                                  prepend = "jingles/intro.mp3"})
```

The follower plays after (`append`) or before (`prepend`) the track, is
resolved lazily at each boundary (a missing file logs and skips), and
`"false"` inhibits the default for a single track.

`http://` / `https://` requests are download-then-play with retry/timeout;
temp files are auto-removed. HTTPS uses rustls (redirects may cross scheme).