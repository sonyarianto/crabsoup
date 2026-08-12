-- External-process pipeline: run the broadcast chain through an outboard
-- raw-PCM processor (stdin/stdout, s16le by default). `cat` is a no-op
-- passthrough for testing; swap in your processor, e.g. Stereo Tool:
--
--   process = '/opt/stereo_tool/stereo_tool_cmd_64 - - -s /mySettings.sts -q -k "<LICENSE>"'
--
-- The processor must match the bus rate/channels (set below). If the
-- process dies, audio bypasses to the raw source while crabsoup restarts
-- it (restart_backoff ms, Icecast-reconnect style); mksafe() additionally
-- guarantees the broadcast never ends.
set("sample_rate", 44100)
set("channels", 2)

src = playlist({directory = "./media", shuffle = true})
proc = pipe({process = "cat", format = "s16le"}, src)

output.preview(mksafe(proc))
