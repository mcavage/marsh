#!/usr/bin/env python3
"""Controlled VM peer for the relay's hostile-input checks (E1: SEC-12, SEC-18,
SEC-21). It stands in for the services agent for ONE disposable project: the
daemon launches it, under the fault `fault-agent-override`, as
`sbx exec -i -u root VM python3 /run/nfx/e1-peer.py` in place of
`docker exec -i ENGINE /marsh/marsh --internal-services relay`.

It speaks the relay's frame protocol (u32 big-endian length, a tag byte, the
body: tag 1 JSON, tag 2 data = u32 channel + payload), completes a valid
handshake for the daemon's generation (or a stale one), then sends ONE invalid
frame and STAYS CONNECTED, logging everything it receives. The daemon must close
the link; the log then ends with `eof` (an end the peer did not initiate).

The scenario is the first line of /run/nfx/e1-scenario. Standard library only.
Files: /run/nfx/e1-peer.log (JSON lines).
"""
import json
import os
import struct
import sys
import threading
import time

DIR = os.environ.get("NFX_PEER_DIR", "/run/nfx")
LOG = os.path.join(DIR, "e1-peer.log")
SCENARIO = open(os.path.join(DIR, "e1-scenario")).read().strip()
LOCK = threading.Lock()
OUT = sys.stdout.buffer


def log(**entry):
    entry["t"] = round(time.time(), 3)
    with LOCK, open(LOG, "a") as handle:
        handle.write(json.dumps(entry, sort_keys=True) + "\n")


def read_exact(count):
    data = b""
    while len(data) < count:
        chunk = os.read(0, count - len(data))
        if not chunk:
            return None
        data += chunk
    return data


def read_frame():
    head = read_exact(4)
    if head is None:
        return None
    size = struct.unpack(">I", head)[0]
    body = read_exact(size) if size else b""
    return body


def send_raw(data):
    with LOCK:
        OUT.write(data)
        OUT.flush()


def send_json(value):
    body = json.dumps(value).encode()
    send_raw(struct.pack(">I", len(body) + 1) + b"\x01" + body)
    log(event="send", frame=value)


def send_data(channel, payload):
    send_raw(struct.pack(">I", 5 + len(payload)) + b"\x02" + struct.pack(">I", channel) + payload)
    log(event="send-data", channel=channel, bytes=len(payload))


def parse(body):
    if not body:
        return None
    if body[0] == 1:
        try:
            return json.loads(body[1:])
        except ValueError:
            return {"type": "unparsed"}
    if body[0] == 2:
        return {"type": "data", "channel": struct.unpack(">I", body[1:5])[0], "bytes": len(body) - 5}
    return {"type": "tag-%d" % body[0]}


def main():
    log(event="start", scenario=SCENARIO)
    init = parse(read_frame())
    log(event="recv", frame=init)
    generation = init["generation"]
    if SCENARIO in ("ready-stale", "stderr-escape"):
        generation = {"boot_id": "stale-boot", "engine_id": "0" * 64}
    if SCENARIO == "ready-extra-field":
        generation = dict(generation, extra="not allowed")
    if SCENARIO == "stderr-escape":
        sys.stderr.write("before \x1b[31mred\x1b[0m\x07 after\n")
        sys.stderr.flush()
    send_json({"type": "ready", "protocol": 1, "generation": generation, "pid": os.getpid(),
               "test_breaks_agent": [], "window_chunks": 1 if SCENARIO == "ready-bad-window" else 64,
               "chunk_bytes": 16384})
    if SCENARIO == "stderr-flood":
        def flood():
            block = b"x" * (1 << 20)
            for _ in range(int(os.environ.get("NFX_FLOOD_MIB", "192"))):
                os.write(2, block)
            log(event="mark", label="flood-done")
        threading.Thread(target=flood, daemon=True).start()
    ports = {"base": [5432]}
    if SCENARIO == "ports-zero":
        ports = {"base": [0]}
    elif SCENARIO == "ports-many":
        ports = {"base": list(range(1024, 1024 + 10000))}
    elif SCENARIO == "ports-dup-scope":
        ports = {"base": [5432], "other": [5432]}
    elif SCENARIO == "ports-foreign":
        ports = {"base": [5432], "../x": [6000]}
    send_json({"type": "ports", "scopes": [{"workspace": w, "ports": p} for w, p in ports.items()], "truncated": False})
    if SCENARIO.startswith("ports-"):
        log(event="mark", label="invalid-frame-sent")

    state = {"opens": 0, "fired": False}

    def fire():
        if state["fired"]:
            return
        state["fired"] = True
        log(event="mark", label="invalid-frame-next")
        if SCENARIO == "oversize":
            send_raw(struct.pack(">I", 2 * 1024 * 1024) + b"\x01" + b"x" * 100)
        elif SCENARIO == "truncated":
            send_raw(struct.pack(">I", 500) + b"\x01" + b'{"type":"opened","chan')
        elif SCENARIO == "unknown-type":
            send_json({"type": "exec", "channel": 1})
        elif SCENARIO == "unknown-type-escape":
            send_json({"type": "\x1b[31mevil\nline2", "channel": 1})
        elif SCENARIO == "restored-unrequested":
            send_json({"type": "restored", "request": 5, "result": {}})
        elif SCENARIO == "stale-channel":
            send_data(999, b"stale")
        elif SCENARIO == "unrequested":
            send_json({"type": "dropped", "request": 77, "id": "c1", "existed": True})
        elif SCENARIO == "credit-over-window":
            send_json({"type": "credit", "channel": 1, "chunks": 1000})
        elif SCENARIO == "duplicate-opened":
            send_json({"type": "opened", "channel": 1})
        log(event="mark", label="invalid-frame-sent")

    if SCENARIO in ("oversize", "truncated", "unknown-type", "unknown-type-escape", "stale-channel",
                    "unrequested", "restored-unrequested"):
        threading.Timer(1.5, fire).start()

    while True:
        body = read_frame()
        if body is None:
            log(event="eof")
            return
        frame = parse(body)
        log(event="recv", frame=frame)
        kind = (frame or {}).get("type")
        if kind == "open":
            state["opens"] += 1
            send_json({"type": "opened", "channel": frame["channel"]})
            if SCENARIO == "duplicate-opened" and state["opens"] == 1:
                send_json({"type": "opened", "channel": frame["channel"]})
                log(event="mark", label="invalid-frame-sent")
            if SCENARIO == "credit-over-window" and state["opens"] == 1:
                send_json({"type": "credit", "channel": frame["channel"], "chunks": 1000})
                log(event="mark", label="invalid-frame-sent")
        elif kind == "list":
            if SCENARIO == "snapshot-id":
                send_json({"type": "listed", "request": frame["request"],
                           "snapshots": [{"id": "../../x", "workspace": "base", "created_unix_ms": 1, "complete": True}]})
                log(event="mark", label="invalid-frame-sent")
            else:
                send_json({"type": "listed", "request": frame["request"], "snapshots": []})
        elif kind == "shutdown":
            return


if __name__ == "__main__":
    try:
        main()
    except Exception as error:  # the log is the evidence
        log(event="exit", error=repr(error))
        raise
