#!/usr/bin/env python3
"""Controlled VM peer: stands in for the services agent for ONE disposable
project (SEC-12, SEC-21). The daemon launches it, under `fault-agent-override`,
as `sbx exec -i -u root $SVC python3 /run/nfx/peer.py`; it speaks the relay's
frame protocol on stdin/stdout from a script, STAYS CONNECTED after every
invalid frame, and logs every frame it receives.

Files (all under /run/nfx, override with NFX_PEER_DIR):
  peer-script.json    the script (below)
  peer.log            JSON lines, one per event:
      {"t", "event": "start"|"recv"|"send"|"mark"|"expect_timeout"|"eof"|"exit"|...}
      recv:  {"n": index, "bytes": N, "frame": <parsed JSON or null>, "raw_b64": "..."}
      eof:   the daemon closed its end (the peer did not initiate it)
  <name>              marker files the harness creates to release `wait_for_file`

Script: {"framing": "u32be" | "lines", "stay": true, "steps": [STEP, ...]}. The
frame BODIES (type and field names) belong to the relay protocol (E1) and the
script supplies them; this program only frames, sends, receives and logs.
  {"op": "send", "frame": {...}}                one frame
  {"op": "send", "hex": "..."}                  raw bytes
  {"op": "send", "frame": {... "@PAD@" ...}, "pad_bytes": N, "pad_byte": "x"}
                                                an oversize frame (@PAD@ replaced by N bytes)
  {"op": "send", "frame": {...}, "truncate": K} the frame minus its last K bytes
  {"op": "send", "frame": {...}, "length": L}   (u32be) claim length L
  {"op": "expect", "name": "hello", "where": {"t": "hello"}, "timeout": 30}
                                                wait for a received frame containing WHERE
                                                (subset match); keep it as NAME
  {"op": "wait", "seconds": S}
  {"op": "wait_for_file", "name": "go-g2", "timeout": 120}
  {"op": "mark", "label": "after-stale-ready"}  a labelled log line (orders the evidence)
  {"op": "close"}                               close stdout and exit
String values in a sent frame of the form "$name.a.b" are replaced by that
field of the frame kept as NAME (e.g. echo the daemon's generation).
After the last step the peer keeps reading and logging until EOF (stay: false
exits instead). Standard library only (python3 in the services VM).
"""
import base64
import json
import os
import struct
import sys
import threading
import time

DIR = os.environ.get("NFX_PEER_DIR", "/run/nfx")
LOG = os.path.join(DIR, "peer.log")
SPEC = json.load(open(os.path.join(DIR, "peer-script.json")))
FRAMING = SPEC.get("framing", "u32be")
LOCK = threading.Lock()
RECEIVED: list[dict] = []
KEPT: dict[str, dict] = {}
EOF_SEEN = threading.Event()
CONDITION = threading.Condition()


def log(**entry) -> None:
    entry["t"] = time.time()
    with LOCK, open(LOG, "a") as handle:
        handle.write(json.dumps(entry, sort_keys=True) + "\n")


def read_exact(count: int) -> bytes | None:
    data = b""
    while len(data) < count:
        chunk = os.read(0, count - len(data))
        if not chunk:
            return None
        data += chunk
    return data


def read_frame() -> bytes | None:
    if FRAMING == "u32be":
        head = read_exact(4)
        if head is None:
            return None
        size = struct.unpack(">I", head)[0]
        return read_exact(size) if size else b""
    line = b""
    while True:
        byte = os.read(0, 1)
        if not byte:
            return None
        if byte == b"\n":
            return line
        line += byte


def reader() -> None:
    index = 0
    while True:
        raw = read_frame()
        if raw is None:
            break
        try:
            frame = json.loads(raw)
        except ValueError:
            frame = None
        log(event="recv", n=index, bytes=len(raw), frame=frame, raw_b64=base64.b64encode(raw[:4096]).decode())
        with CONDITION:
            RECEIVED.append({"n": index, "frame": frame})
            CONDITION.notify_all()
        index += 1
    log(event="eof", note="the daemon closed its end of the link")
    EOF_SEEN.set()
    with CONDITION:
        CONDITION.notify_all()


def frame_bytes(payload: bytes, length: int | None = None) -> bytes:
    if FRAMING == "u32be":
        return struct.pack(">I", len(payload) if length is None else length) + payload
    return payload + b"\n"


def substitute(value):
    if isinstance(value, dict):
        return {k: substitute(v) for k, v in value.items()}
    if isinstance(value, list):
        return [substitute(v) for v in value]
    if isinstance(value, str) and value.startswith("$") and value[1:].split(".")[0] in KEPT:
        node = KEPT[value[1:].split(".")[0]]
        for part in value[1:].split(".")[1:]:
            node = node[part]
        return node
    return value


def emit(data: bytes) -> None:
    try:
        sys.stdout.buffer.write(data)
        sys.stdout.buffer.flush()
    except (BrokenPipeError, OSError):
        log(event="send_failed", note="stdout closed")
        raise SystemExit(0)


def matches(frame, where) -> bool:
    return isinstance(frame, dict) and all(frame.get(k) == v for k, v in where.items())


def do_send(step) -> None:
    if "hex" in step:
        data = bytes.fromhex(step["hex"])
    else:
        text = json.dumps(substitute(step["frame"]), separators=(",", ":"))
        if "pad_bytes" in step:
            text = text.replace("@PAD@", step.get("pad_byte", "x") * step["pad_bytes"])
        data = frame_bytes(text.encode(), step.get("length"))
        if "truncate" in step:
            data = data[: len(data) - step["truncate"]]
    log(event="send", bytes=len(data), step=step.get("note") or step.get("frame", {}).get("t") if "frame" in step else "hex")
    emit(data)


def run_steps() -> None:
    cursor = 0
    for step in SPEC["steps"]:
        op = step["op"]
        if op == "send":
            do_send(step)
        elif op == "expect":
            deadline = time.monotonic() + step.get("timeout", 30)
            found = None
            with CONDITION:
                while found is None:
                    for item in RECEIVED[cursor:]:
                        if matches(item["frame"], step.get("where", {})):
                            found = item
                            break
                    if found is not None:
                        break
                    remaining = deadline - time.monotonic()
                    if remaining <= 0 or EOF_SEEN.is_set():
                        break
                    CONDITION.wait(min(remaining, 0.5))
            if found is None:
                log(event="expect_timeout", step=step)
                return
            cursor = found["n"] + 1
            if step.get("name"):
                KEPT[step["name"]] = found["frame"]
        elif op == "wait":
            time.sleep(step["seconds"])
        elif op == "wait_for_file":
            path = os.path.join(DIR, step["name"])
            deadline = time.monotonic() + step.get("timeout", 120)
            while not os.path.exists(path) and time.monotonic() < deadline:
                time.sleep(0.1)
            log(event="file_released" if os.path.exists(path) else "wait_for_file_timeout", name=step["name"])
        elif op == "mark":
            log(event="mark", label=step.get("label", ""))
        elif op == "close":
            log(event="exit", reason="close step")
            return os._exit(0)
        else:
            log(event="bad_step", step=step)


def main() -> int:
    log(event="start", argv=sys.argv[1:], framing=FRAMING)
    threading.Thread(target=reader, daemon=True).start()
    run_steps()
    log(event="script_done")
    if SPEC.get("stay", True):
        EOF_SEEN.wait()
    log(event="exit", reason="eof" if EOF_SEEN.is_set() else "script done")
    return 0


if __name__ == "__main__":
    sys.exit(main())
