#!/usr/bin/env python3
"""Successful hijack variants through the filtered endpoint (B1: raw only after the
engine itself switched). Runs in the sandbox VM against SCOPE_SOCKET.

  - attach without an Upgrade header (the engine answers 200 raw-stream)
  - attach with Upgrade: tcp, with and without a TTY
  - exec start without Upgrade, with a TTY
  - exec start with Upgrade
  - a request that asks for an Upgrade on a route that must not hijack stays HTTP
  - a client that goes away mid-stream does not leave the agent stuck (a new
    request still works at once)

Each variant sends bytes and requires them back from the container, so a pump
that carries output only (break `filter-no-upgrade`) fails the echo checks.

  hijack.py SCOPE_SOCKET
"""
import json, socket, sys, time

SOCK = sys.argv[1]
ok_all = True


def check(ok, label, detail=""):
    global ok_all
    ok_all &= bool(ok)
    print(("ok   " if ok else "BAD  ") + label, detail)


def conn(t=8):
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(t)
    s.connect(SOCK)
    return s


def http(method, path, body=None):
    s = conn()
    b = json.dumps(body).encode() if body is not None else b""
    ct = b"Content-Type: application/json\r\n" if body is not None else b""
    s.sendall(f"{method} /v1.47{path} HTTP/1.1\r\nHost: d\r\n".encode() + ct + b"Content-Length: %d\r\nConnection: close\r\n\r\n" % len(b) + b)
    buf = b""
    while True:
        try:
            d = s.recv(65536)
        except socket.timeout:
            break
        if not d:
            break
        buf += d
    s.close()
    head, _, rest = buf.partition(b"\r\n\r\n")
    status = int(head.split(b" ", 2)[1]) if head.startswith(b"HTTP/") else 0
    if b"chunked" in head.lower():
        out = b""
        while rest:
            n, _, rest = rest.partition(b"\r\n")
            try:
                k = int(n, 16)
            except ValueError:
                break
            if k == 0:
                break
            out += rest[:k]
            rest = rest[k + 2:]
        rest = out
    return status, rest


def hijack(path, upgrade, body=None, send=b"", want=b"", wait=4.0, half_close=False):
    """Open a hijacked stream, write `send`, collect until `want` shows or `wait` passes."""
    s = conn(wait)
    b = json.dumps(body).encode() if body is not None else b""
    ct = b"Content-Type: application/json\r\n" if body is not None else b""
    up = b"Connection: Upgrade\r\nUpgrade: tcp\r\n" if upgrade else b""
    s.sendall(f"POST /v1.47{path} HTTP/1.1\r\nHost: d\r\n".encode() + ct + up + b"Content-Length: %d\r\n\r\n" % len(b) + b)
    buf = b""
    while b"\r\n\r\n" not in buf:
        d = s.recv(65536)
        if not d:
            break
        buf += d
    head, _, rest = buf.partition(b"\r\n\r\n")
    status = int(head.split(b" ", 2)[1]) if head.startswith(b"HTTP/") else 0
    if send:
        s.sendall(send)
        if half_close:
            s.shutdown(socket.SHUT_WR)
    t0 = time.time()
    data = rest
    while want not in data and time.time() - t0 < wait:
        try:
            d = s.recv(65536)
        except socket.timeout:
            break
        if not d:
            break
        data += d
    s.close()
    return status, head, data


def create(name, tty, cmd):
    http("DELETE", f"/containers/{name}?force=1")
    st, b = http("POST", f"/containers/create?name={name}", {
        "Image": "busybox", "Cmd": cmd, "Tty": tty, "OpenStdin": True, "AttachStdin": True, "AttachStdout": True, "AttachStderr": True})
    assert st == 201, (st, b)
    st, b = http("POST", f"/containers/{name}/start")
    assert st == 204, (st, b)


# --- attach ---------------------------------------------------------------------------
create("hj-plain", False, ["cat"])
st, head, data = hijack("/containers/hj-plain/attach?stream=1&stdin=1&stdout=1", upgrade=False, send=b"plain-no-upgrade\n", want=b"plain-no-upgrade")
check(st == 200 and (b"raw-stream" in head.lower() or b"multiplexed" in head.lower()), "attach without Upgrade: the engine switches to a raw stream", f"status={st}")
check(b"plain-no-upgrade" in data, "attach without Upgrade: stdin reaches the container and its output comes back", repr(data[:60]))
st, head, data = hijack("/containers/hj-plain/attach?stream=1&stdin=1&stdout=1", upgrade=True, send=b"plain-upgrade\n", want=b"plain-upgrade")
check(st == 101, "attach with Upgrade: tcp gets 101", f"status={st}")
check(b"plain-upgrade" in data, "attach with Upgrade: bytes both ways", repr(data[:60]))
http("DELETE", "/containers/hj-plain?force=1")

create("hj-tty", True, ["cat"])
st, head, data = hijack("/containers/hj-tty/attach?stream=1&stdin=1&stdout=1", upgrade=True, send=b"tty-upgrade\n", want=b"tty-upgrade")
check(b"tty-upgrade" in data, "PTY attach with Upgrade: the terminal echoes what was typed", repr(data[:60]))
st, head, data = hijack("/containers/hj-tty/attach?stream=1&stdin=1&stdout=1", upgrade=False, send=b"tty-no-upgrade\n", want=b"tty-no-upgrade")
check(b"tty-no-upgrade" in data, "PTY attach without Upgrade: the terminal echoes what was typed", repr(data[:60]))
# Half-close: stdin ends, the output is still delivered
http("DELETE", "/containers/hj-tty?force=1")

# --- exec -----------------------------------------------------------------------------
create("hj-exec", False, ["sleep", "300"])
for tty in (False, True):
    for up in (True, False):
        st, b = http("POST", "/containers/hj-exec/exec", {"Cmd": ["sh", "-c", "read x; echo got-$x"], "AttachStdin": True, "AttachStdout": True, "AttachStderr": True, "Tty": tty})
        eid = json.loads(b)["Id"]
        st, head, data = hijack(f"/exec/{eid}/start", upgrade=up, body={"Detach": False, "Tty": tty}, send=b"ping\n", want=b"got-ping")
        check(b"got-ping" in data, f"exec start (tty={tty}, upgrade={up}): stdin in, output out", f"status={st} {data[:50]!r}")
        stj, bj = http("GET", f"/exec/{eid}/json")
        check(stj == 200 and json.loads(bj).get("ExitCode") == 0, f"exec (tty={tty}, upgrade={up}): exits 0", bj[:60])
http("DELETE", "/containers/hj-exec?force=1")

# --- a route that must not hijack stays plain HTTP --------------------------------------
create("hj-nohijack", False, ["sleep", "300"])
s = conn()
s.sendall(b"GET /v1.47/containers/hj-nohijack/json HTTP/1.1\r\nHost: d\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n")
raw = s.recv(65536)
s.close()
check(raw.startswith(b"HTTP/1.1 200") and b"upgrade" not in raw.lower().split(b"\r\n\r\n")[0] and b"101" not in raw[:12],
      "an Upgrade request on a route that does not hijack is answered as plain HTTP (the header is ignored, never forwarded, never switched)", raw[:30].decode("latin1"))
http("DELETE", "/containers/hj-nohijack?force=1")

# --- disconnect: a client that vanishes mid-stream leaves nothing stuck --------------------
create("hj-gone", False, ["sleep", "300"])
st, b = http("POST", "/containers/hj-gone/exec", {"Cmd": ["sleep", "60"], "AttachStdout": True})
eid = json.loads(b)["Id"]
for _ in range(5):
    s = conn()
    body = b'{"Detach":false,"Tty":false}'
    s.sendall(f"POST /v1.47/exec/{eid}/start HTTP/1.1\r\nHost: d\r\nContent-Type: application/json\r\nConnection: Upgrade\r\nUpgrade: tcp\r\nContent-Length: {len(body)}\r\n\r\n".encode() + body)
    s.recv(4096)
    s.close()
time.sleep(1)
t0 = time.time()
st, b = http("GET", "/containers/json")
check(st == 200 and time.time() - t0 < 3, "after clients vanish mid-stream the endpoint still answers at once", f"status={st}")
http("DELETE", "/containers/hj-gone?force=1")
sys.exit(0 if ok_all else 1)
