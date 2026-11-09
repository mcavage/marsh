#!/usr/bin/env python3
"""SEC-23 oracle: read the recording proxy's file (`ID > hex` lines are the bytes the
agent sent to the engine) and assert the exact wire image of what the engine got.

  canon_check.py REC   [--expect-ping] [--expect-create-fixed] [--expect-create-chunked]

Prints ok/BAD lines; exit 1 if any BAD.
"""
import json, re, sys
from collections import defaultdict

rec = sys.argv[1]
conns = defaultdict(bytes)
for line in open(rec):
    parts = line.split()
    if len(parts) == 3 and parts[1] == ">":
        conns[parts[0]] += bytes.fromhex(parts[2])
bad = 0


def check(ok, label, detail=""):
    global bad
    print(("ok   " if ok else "BAD  ") + label, detail)
    bad += 0 if ok else 1


def split(raw):
    head, _, body = raw.partition(b"\r\n\r\n")
    lines = head.decode("latin1").split("\r\n")
    hdr = [tuple(l.split(": ", 1)) for l in lines[1:]]
    return lines[0], hdr, body


check(bool(conns) and sum(len(v) for v in conns.values()) > 0, "the recorder saw bytes from the agent to the engine (a recording with nothing in it proves nothing)", f"{len(conns)} connections")
reqs = [split(v) for v in conns.values() if v]

# --- ping control: the exact canonical GET -------------------------------------------------
pings = [r for r in reqs if r[0].split(" ")[1].endswith("/_ping")]
check(len(pings) >= 1, "control: the ping reached the engine")
for first, hdr, body in pings[:1]:
    names = [k.lower() for k, _ in hdr]
    check(first == "GET /v1.47/_ping HTTP/1.1" or first == "GET /_ping HTTP/1.1", "control: ping request line is canonical", first)
    check(hdr[0] == ("Host", "docker") and ("Connection", "close") in hdr, "control: ping has Host: docker and Connection: close")
    check(not (set(names) - {"host", "user-agent", "connection"}), "control: ping carries no other header", str(names))

# --- the creates -----------------------------------------------------------------------------
creates = [r for r in reqs if "/containers/create" in r[0] and "name=canon-" in r[0]]
for tag in ("fixed", "chunked"):
    mine = [r for r in creates if f"name=canon-{tag}" in r[0]]
    check(len(mine) == 1, f"the {tag} create reached the engine exactly once", str(len(mine)))
    if not mine:
        continue
    first, hdr, body = mine[0]
    names = [k.lower() for k, _ in hdr]
    check(first == f"POST /v1.47/containers/create?name=canon-{tag} HTTP/1.1", f"{tag}: request line is the canonical rebuild (query holds `name` only)", first)
    check(not (set(names) - {"host", "user-agent", "content-type", "connection", "content-length"}), f"{tag}: no header the client chose reached the engine", str(names))
    check(("Host", "docker") in hdr and ("Content-Type", "application/json") in hdr and ("Connection", "close") in hdr, f"{tag}: Host, Content-Type and Connection are the filter's own")
    cl = [v for k, v in hdr if k.lower() == "content-length"]
    check(len(cl) == 1 and int(cl[0]) == len(body), f"{tag}: one Content-Length, equal to the body ({len(body)} bytes)", str(cl))
    check("transfer-encoding" not in names, f"{tag}: no Transfer-Encoding upstream (the body was not forwarded as the client framed it)")
    try:
        j = json.loads(body)
    except ValueError as e:
        check(False, f"{tag}: the body is JSON", str(e))
        continue
    check(body == json.dumps(j, separators=(",", ":"), ensure_ascii=False).encode(), f"{tag}: the body is the compact canonical serialization (no client whitespace)", body[:80].decode("latin1"))
    check(j.get("Labels", {}).get("dev.marsh.scope") == "canon", f"{tag}: enrichment: the scope label was added")
    check("Image=" not in first and "evil" not in first.lower(), f"{tag}: a body field repeated in the query did not survive")
    hc = j.get("HostConfig", {})
    check(re.fullmatch(r"[0-9a-f]{64}", hc.get("NetworkMode", "")) is not None, f"{tag}: enrichment: NetworkMode is the scope network's ID", hc.get("NetworkMode", "")[:20])
    check(hc.get("Memory") == 4 << 30 and hc.get("NanoCpus") == 4_000_000_000 and hc.get("PidsLimit") == 4096, f"{tag}: enrichment: default limits")
    check(j.get("Env") == ["CANON_MARK=1"], f"{tag}: ordinary client values survive", str(j.get("Env")))
sys.exit(1 if bad else 0)
