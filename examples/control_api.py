#!/usr/bin/env python3
"""Crabcast-style control backend: drive crabsoup's JSON control surface.

Two transports, one contract. Every reply is a single JSON object on one
line: {"ok": true, ...} on success, {"ok": false, "error": "..."} on
failure — so a backend just checks the "ok" field, never parses prose.

1. HTTP (recommended):  server.telnet({port = 1234, http_port = 8080})
     GET  /status /uptime /queue /jingles
     POST /cmd            body {"command": "jingles.play trance"}
2. Telnet JSON:          server.telnet({port = 1234, banner = false})
     send "json <command>" per line; reply is one line of JSON.

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

    print("\nbackend: all good — rely on \"ok\" and the fields, never on prose")


if __name__ == "__main__":
    main()
