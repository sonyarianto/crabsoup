# Stereo Tool (Thimeo) — local setup

This folder holds the licensed Stereo Tool command-line processor used by
the `pipe` operator. Everything in it is gitignored (`*`); only this
README and the `.gitignore` are tracked.

## Install

1. Download the 64-bit CLI from Thimeo:
   `https://download.thimeo.com/stereo_tool_cmd_64`
2. Save it here as `stereo_tool_cmd_64` (11.x) and make it executable.
3. Activate it with your license key, e.g.
   `./stereo_tool_cmd_64 -k "<your-key>" -q -s audio.sts - -`
   (first run registers the key; the `-k` argument stays visible in
   `ps aux` — expected when shelling out to a licensed binary).
4. Put your processor settings in `audio.sts` next to the binary and
   reference it with `-s audio.sts`. Without `-s` the tool looks for
   `~/.stereo_tool.rc`, which does not exist on a fresh machine.
5. Sample-rate/format flags: `-b 16 -r <bus rate>` (e.g. `-r 44100`);
   `-q` keeps its progress output off stderr. Pipe mode is `- -`.

## Use in a script

```lua
st = pipe({process = 'tools/stereo_tool/stereo_tool_cmd_64 -q -s tools/stereo_tool/audio.sts '
                    .. '-b 16 -r 44100 -k "<your-key>" - -',
           format = "s16le"},
          pl)
```

The tool runs at roughly 35% of one core and ~250 MB RSS. While it is
down (crash, restart), `pipe` bypasses to the unprocessed source, so the
broadcast never drops.