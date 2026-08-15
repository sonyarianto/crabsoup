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

## JSON mode

The plain-text replies above are for humans. For a program (e.g. a web
backend), prefix any command with `json ` to get a machine-readable reply:
each is a single line of JSON, so a line-oriented reader needs no parsing
beyond `read line -> JSON.parse`.

```sh
printf 'json status\n' | nc localhost 1234
# {"ok":true,"playing":"Some track.mp3","uptime_seconds":123,"harbor_connected":false}
```

Every reply is `{"ok": true, ...}` on success and
`{"ok": false, "error": "..."}` on failure (the `error` text is the same
message the plain-text protocol prints). Notable fields:

| Command | JSON reply |
| --- | --- |
| `json status` | `{"ok":true,"playing":"...","uptime_seconds":N,"harbor_connected":bool}` |
| `json uptime` | `{"ok":true,"uptime_seconds":N}` |
| `json queue.list` | `{"ok":true,"queue":["...", ...]}` |
| `json jingles.list` | `{"ok":true,"jingles":["...", ...]}` |
| `json queue.push <uri>` | `{"ok":true,"queued":"...","length":N}` |
| `json skip` / `json shutdown` | `{"ok":true,"message":"..."}` |
| `json <custom command>` | `{"ok":true,"reply":"..."}` |
| any error | `{"ok":false,"error":"..."}` |

The name `json` is reserved and cannot be used as a `server.register`
command name. `json` with no command replies
`{"ok":false,"error":"usage: json <command>"}`.

Plain-text `status` also carries the live-DJ state as a `live: true|false`
line (the same flag as `harbor_connected`), so a listener can see when a DJ
is on air without parsing JSON.

Machine clients should also set `banner = false` in `server.telnet` so the
connection starts with replies instead of the prose welcome line:

```lua
server.telnet({port = 1234, banner = false})
```

## HTTP endpoint

`server.telnet({http_port = N})` serves the same command surface over HTTP
on the same host — no telnet framing, no banner, plain request/response.
Every response body is the JSON envelope above.

```lua
server.telnet({port = 1234, http_port = 8080})
```

```sh
curl http://localhost:8080/status
# {"ok":true,"playing":"Some track.mp3","uptime_seconds":123,"harbor_connected":false}
curl http://localhost:8080/queue
curl http://localhost:8080/jingles
curl -X POST -H 'Content-Type: application/json' \
     -d '{"command":"jingles.play trance"}' http://localhost:8080/cmd
```

| Route | Effect |
| --- | --- |
| `GET /status` | `status` |
| `GET /uptime` | `uptime` |
| `GET /queue` | `queue.list` |
| `GET /jingles` | `jingles.list` |
| `POST /cmd` `{"command": "..."}` | any control command; reply identical to `json <command>` on telnet |

HTTP status codes: 200 for `{"ok": true}`, 400 for `{"ok": false}`
(unknown command, bad usage, malformed body), 404 for unknown routes,
405 for wrong methods. See `examples/control_api.py` for a worked
Crabcast-style backend using both transports.

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