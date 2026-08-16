#!/usr/bin/env python3
"""Crabcast-style control backend: drive crabsoup's JSON control surface.

Three transports, one contract. Every reply is a single JSON object on one
line: {"ok": true, ...} on success, {"ok": false, "error": "..."} on
failure — so a backend just checks the "ok" field, never parses prose.

1. HTTP (recommended):  server.telnet({port = 1234, http_port = 8080})
     GET  /status /uptime /queue /jingles
     POST /cmd            body {"command": "jingles.play trance"}
2. Telnet JSON:          server.telnet({port = 1234, banner = false})
     send "json <command>" per line; reply is one line of JSON.
3. WebSocket:            server.telnet({port = 1234, ws_port = 8081})
     each text frame is one command (bare or {"command": "..."});
     the reply is the JSON envelope as the next text frame.

Start crabsoup first, then run this script:

    ./target/release/crabsoup -c crabsoup.lua
    python3 examples/control_api.py
"""

import json
import socket
import sys
import urllib.error
import urllib.request

HTTP_BASE = "http://127.0.0.1:8080"     # http_port in server.telnet
TELNET_ADDR = ("127.0.0.1", 1234)       # port in server.telnet


def http_get(path):
    """GET a status endpoint; returns the parsed JSON envelope."""
    try:
        with urllib.request.urlopen(HTTP_BASE + path, timeout=5) as r:
            return json.loads(r.read())
    except urllib.error.HTTPError as e:
        return json.loads(e.read())


def http_cmd(command):
    """POST a control command; same envelope as `json <command>` on telnet."""
    req = urllib.request.Request(
        HTTP_BASE + "/cmd",
        data=json.dumps({"command": command}).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=5) as r:
            return json.loads(r.read())
    except urllib.error.HTTPError as e:
        return json.loads(e.read())


def telnet_json(command):
    """Send `json <command>` over the telnet port and read one JSON line.

    Requires banner = false so the connection starts with replies, not the
    prose welcome line. Lines are newline-terminated, so one readline() per
    command is all the framing a backend needs.
    """
    with socket.create_connection(TELNET_ADDR, timeout=5) as sock:
        sock.sendall(f"json {command}\n".encode())
        return json.loads(sock.makefile().readline())


WS_ADDR = ("127.0.0.1", 8081)          # ws_port in server.telnet


def ws_command(command):
    """Send one command over WebSocket; return the JSON envelope.

    Uses the `websocket-client` package if installed (pip install
    websocket-client); without it, falls back to a hand-rolled RFC 6455
    client (SHA-1 accept, masked frames) so the example runs stdlib-only.
    """
    try:
        import websocket  # type: ignore

        ws = websocket.create_connection(f"ws://127.0.0.1:{WS_ADDR[1]}", timeout=5)
        ws.send(command)
        reply = ws.recv()
        ws.close()
        return json.loads(reply)
    except ImportError:
        pass

    import base64
    import hashlib
    import os
    import struct

    with socket.create_connection(WS_ADDR, timeout=5) as sock:
        key = base64.b64encode(os.urandom(16)).decode()
        sock.sendall(
            f"GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n"
            f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
            f"Sec-WebSocket-Version: 13\r\n\r\n".encode()
        )
        while b"\r\n\r\n" not in sock.recv(4096):
            pass
        mask = os.urandom(4)
        payload = command.encode()
        sock.sendall(bytes([0x81, 0x80 | len(payload)]) + mask
                     + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))
        b0 = sock.recv(1)[0]
        b1 = sock.recv(1)[0]
        length = b1 & 0x7F
        if length == 126:
            length = struct.unpack(">H", sock.recv(2))[0]
        elif length == 127:
            length = struct.unpack(">Q", sock.recv(8))[0]
        assert b0 & 0x0F == 0x1, "expected a text reply frame"
        return json.loads(sock.recv(length))


def check(envelope, label):
    ok = envelope.get("ok", False)
    print(f"  [{'ok' if ok else 'FAIL'}] {label}: {envelope}")
    return ok


def main():
    try:
        status = http_get("/status")
    except Exception as e:  # urllib.error.URLError, ConnectionRefusedError
        print(f"cannot reach crabsoup at {HTTP_BASE}: {e}")
        print("start it with server.telnet({port = 1234, http_port = 8080}) and retry")
        sys.exit(1)

    print("== HTTP endpoint ==")
    check(status, "GET /status")
    check(http_get("/queue"), "GET /queue")
    check(http_get("/jingles"), "GET /jingles")
    check(http_cmd("queue.push /tmp/drop.mp3"), "POST /cmd queue.push")
    check(http_get("/queue"), "GET /queue (after push)")
    check(http_cmd("skip"), "POST /cmd skip")
    bad = http_cmd("not-a-command")
    assert bad.get("ok") is False, f"unknown command should fail: {bad}"
    print(f"  [ok] POST /cmd error envelope: {bad}")

    print("== Telnet JSON mode (banner = false) ==")
    check(telnet_json("status"), "json status")
    check(telnet_json("uptime"), "json uptime")

    print("== WebSocket endpoint (ws_port) ==")
    check(ws_command("status"), "ws status")
    check(ws_command('{"command": "uptime"}'), "ws JSON command")

    print("\nbackend: all good — rely on \"ok\" and the fields, never on prose")


if __name__ == "__main__":
    main()
