#!/usr/bin/env python3
"""A7 bounds against the running agent: head and field limits, JSON limit,
per-attempt connection cap, and the agent's FD/RSS growth. Runs in the VM.

  bounds.py SCOPE_SOCKET
"""
import json, os, socket, subprocess, sys, time

SOCK = sys.argv[1]
ok_all = True


def check(ok, label, detail=""):
    global ok_all
    ok_all &= bool(ok)
    print(("ok   " if ok else "BAD  ") + label, detail)


def conn():
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(5)
    s.connect(SOCK)
    return s


def status(raw, chunked_ok=False, with_body=False):
    s = conn()
    try:
        s.sendall(raw)
    except OSError:
        pass  # the agent answered and closed before the whole body went out
    buf = b""
    try:
        while b"\r\n\r\n" not in buf:
            d = s.recv(65536)
            if not d:
                break
            buf += d
    except socket.timeout:
        pass
    s.close()
    st = int(buf.split(b" ", 2)[1]) if buf.startswith(b"HTTP/") else 0
    return (st, buf.partition(b"\r\n\r\n")[2]) if with_body else st


def agent_stats():
    out = subprocess.run(["docker", "exec", "nfx-e3-engine", "sh", "-c",
                          "p=$(pgrep -f 'e3-harness serve --state /pool/marsh' | head -1); echo $(ls /proc/$p/fd | wc -l) $(grep VmRSS /proc/$p/status | tr -s ' ' | cut -d' ' -f2)"],
                         capture_output=True, text=True).stdout.split()
    return int(out[0]), int(out[1])


fd0, rss0 = agent_stats()
line = b"GET /_ping HTTP/1.1\r\n"
def head(total, fields=0):
    h = line + b"Host: d\r\n" + b"".join(b"X-%d: v\r\n" % i for i in range(fields))
    fill = total - len(h) - len(b"A: ") - 4
    if fill > 0:
        h += b"A: " + b"a" * fill + b"\r\n"
    h += b"\r\n"
    return h

h = head(32768); assert len(h) == 32768
check(status(h) == 200, "head of exactly 32 KiB is accepted")
h = head(32769); check(len(h) == 32769 and status(h) == 431, "head of 32 KiB + 1 gets 431")
f = line + b"".join(b"X-%d: v\r\n" % i for i in range(100)) + b"\r\n"
check(status(f) == 200, "100 header fields are accepted")
f = line + b"".join(b"X-%d: v\r\n" % i for i in range(101)) + b"\r\n"
check(status(f) == 431, "101 header fields get 431")


def create_body(total, field):
    """A create body of exactly `total` bytes whose bulk sits in one field."""
    base = {"Image": "nfx-no-such-image"}
    if field == "labels":
        base["Labels"] = {"pad": ""}
    elif field == "env":
        base["Env"] = ["PAD="]
    else:
        base["Cmd"] = ["true", ""]
    n = total - len(json.dumps(base, separators=(",", ":")))
    if field == "labels":
        base["Labels"]["pad"] = "x" * n
    elif field == "env":
        base["Env"][0] += "x" * n
    else:
        base["Cmd"][1] = "x" * n
    b = json.dumps(base, separators=(",", ":")).encode()
    assert len(b) == total, (len(b), total)
    return b


def post(b):
    return b"POST /v1.47/containers/create?name=bounds-x HTTP/1.1\r\nHost: d\r\nContent-Type: application/json\r\nContent-Length: %d\r\n\r\n" % len(b) + b


# The oracle for "accepted" is the ENGINE's own answer (404: no such image, which it
# gives only to a body it parsed and admitted), never a status the policy itself
# produced (403 would also be "not 413").
admitted = []
for field in ("cmd", "env", "labels"):
    st, body = status(post(create_body(1048576, field)), with_body=True)
    real = st == 404 and b"No such image" in body and b"marsh:" not in body
    print(f"     1 MiB create, bulk in {field}: status {st} {body[:70]!r}")
    if real:
        admitted.append(field)
check(bool(admitted), "a JSON body of exactly 1 MiB is admitted by the agent AND by the engine (404 No such image from the engine, not a policy 403)", f"via {admitted}")
for field in ("cmd", "env", "labels"):
    st = status(post(create_body(1048577, field)))
    check(st == 413, f"JSON body of 1 MiB + 1 byte gets 413 (bulk in {field})", f"(status {st})")

held = []
try:
    for i in range(64):
        s = conn()
        s.sendall(line + b"Host: d\r\n\r\n")
        held.append(s)
    for s in held:
        s.recv(65536)
    s = conn()
    s.sendall(line + b"Host: d\r\n\r\n")
    raw = s.recv(65536)
    check(raw.startswith(b"HTTP/1.1 429") and b"marsh:" in raw, "the 65th connection of an attempt is refused (429)", raw[:20].decode("latin1"))
    s.close()
finally:
    for s in held:
        s.close()
import time; time.sleep(1.5)
check(status(line + b"Host: d\r\n\r\n") == 200, "after the idle connections close, requests work again")

# --- A7: the project cap counts the project, not each workspace's root ----------------------
# 256 connections held across four scopes (three share the project root, stacka is bound
# to a sub-folder), then connection 257 from a fifth scope must be refused. A separate
# project (its own agent) is not held back by it.
D = os.path.dirname(SOCK)
PROJ = os.environ.get("PROJ", "/private/tmp/nfx-e3/proj")
PING = line + b"Host: d\r\n\r\n"


def conn_to(path):
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(5)
    s.connect(path)
    return s


held = []
try:
    for sc in ("base", "a", "b", "stacka"):
        for i in range(64):
            s = conn_to(f"{D}/s-{sc}.sock")
            s.sendall(PING)
            assert s.recv(65536).startswith(b"HTTP/1.1 200"), sc
            held.append(s)
    check(len(held) == 256, "256 connections are held across four workspaces with two different bind roots")
    s = conn_to(f"{D}/s-tc.sock")
    s.sendall(PING)
    raw = s.recv(65536)
    check(raw.startswith(b"HTTP/1.1 429") and b"project" in raw, "connection 257, from a fifth workspace, is refused by the project cap (429)", raw[:60].decode("latin1"))
    s.close()
    # Control: a genuinely separate project (its own agent instance) still gets served.
    subprocess.run(["docker", "exec", "nfx-e3-engine", "rm", "-f", f"{D}/s-sepproj.sock", "/tmp/sep.pid"], capture_output=True)
    subprocess.run(["docker", "exec", "-d", "nfx-e3-engine", "sh", "-c",
                    f"echo $$ > /tmp/sep.pid; exec /marsh/e3-harness serve --state /tmp/sep-state --upstream /var/run/docker.sock --listen {D}/s-sepproj.sock --root {PROJ} --workspace sepproj --attempt a1"],
                   capture_output=True)
    for _ in range(40):
        if os.path.exists(f"{D}/s-sepproj.sock"):
            break
        time.sleep(0.25)
    try:
        s = conn_to(f"{D}/s-sepproj.sock")
        s.sendall(PING)
        raw = s.recv(65536)
        check(raw.startswith(b"HTTP/1.1 200"), "control: a separate project is served while the first is at its cap", raw[:20].decode("latin1"))
        s.close()
    finally:
        subprocess.run(["docker", "exec", "nfx-e3-engine", "sh", "-c", "kill $(cat /tmp/sep.pid) 2>/dev/null; rm -f /tmp/sep.pid"], capture_output=True)
finally:
    for s in held:
        s.close()
time.sleep(1.5)
check(status(PING) == 200, "after the held connections close, requests work again")
fd1, rss1 = agent_stats()
check(fd1 - fd0 < 20, f"agent FDs did not grow ({fd0} -> {fd1})")
check(rss1 - rss0 < 150 * 1024, f"agent RSS grew < 150 MB ({rss0} -> {rss1} kB)")
sys.exit(0 if ok_all else 1)
