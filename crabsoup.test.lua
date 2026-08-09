-- Temporary test config: HLS output only
pl = playlist({directory = "./media", shuffle = false, loop = true})
j = jingles({directory = "./jingles"})
live = input.harbor({host = "0.0.0.0", port = 8005, mount = "/live", password = "dj"})

output.hls({directory = "/tmp/opencode/hls", segment_seconds = 4, retention = 12},
           fallback({j, live, pl}))
