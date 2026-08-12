# Control port

The telnet control port is Liquidsoap-style: command + newline, reply returned.
Point `nc` at the port configured in `server.telnet({...})` (1234 in the
[example script](/guide/example-script)):

```sh
printf 'jingles.play\n' | nc localhost 1234   # random jingle
printf 'jingles.play trance\n' | nc localhost 1234  # by substring
printf 'jingles.list\n' | nc localhost 1234   # index + path per line
printf 'skip\n' | nc localhost 1234           # skip the current track
printf 'status\n' | nc localhost 1234         # current track + uptime
printf 'shutdown\n' | nc localhost 1234
```

## Built-in commands

| Command | Effect |
| --- | --- |
| `jingles.list` | Index + path per line |
| `jingles.play [n\|substr]` | Play a random, indexed, or substring-matched jingle |
| `queue.push <uri>` | Push a request (path or `http(s)://` URL) onto the request queue |
| `skip` | Skip the current track |
| `status` | Current track + uptime |
| `uptime` | Engine uptime |
| `shutdown` | Graceful shutdown |
| `exit` | Disconnect |
| `help` | List commands |

## Custom commands

Register your own handlers in the script with
`server.register("name", function(args) return reply end)` — the handler
receives the rest of the line as one string and its return value is sent
back; a Lua error becomes an `ERROR: ...` reply:

```lua
server.register("next_genre", function(args)
  return "switched to: " .. args
end)
```

If the script registers `server.telnet`, no further moving parts are needed —
custom commands are routed straight to the Lua event loop.