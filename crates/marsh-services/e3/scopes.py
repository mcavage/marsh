#!/usr/bin/env python3
"""SEC-05 / SEC-16 / RST-11: workspace scoping, from lane `a` against objects of
`base` and `b`. Runs in the sandbox VM; ground truth through the engine's own
socket (docker exec nfx-e3-engine docker -H ...).

  scopes.py SOCKDIR      (expects s-base.sock, s-a.sock, s-b.sock)
"""
import http.client, json, os, socket, subprocess, sys, time

D = sys.argv[1]
SOCK = {s: f"{D}/s-{s}.sock" for s in ("base", "a", "b")}
PASS = FAIL = 0


def prod(*a):
    r = subprocess.run(["docker", "exec", "nfx-e3-engine", "docker", "-H", "unix:///var/run/docker.sock", *a], capture_output=True, text=True)
    return r.stdout.strip()


def cli(scope, *a):
    env = dict(os.environ, DOCKER_HOST=f"unix://{SOCK[scope]}")
    r = subprocess.run(["docker", *a], capture_output=True, text=True, env=env)
    return r.returncode, (r.stdout + r.stderr).strip()


class UC(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost")
        self.path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(8)
        self.sock.connect(self.path)


def req(scope, method, path, body=None, headers=None):
    c = UC(SOCK[scope])
    h = dict(headers or {})
    data = None
    if body is not None:
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        h.setdefault("Content-Type", "application/json")
    try:
        c.request(method, path, body=data, headers=h)
        r = c.getresponse()
        return r.status, r.read()
    except Exception as e:  # noqa: BLE001
        return 0, str(e).encode()
    finally:
        c.close()


def check(ok, label, detail=""):
    global PASS, FAIL
    if ok:
        PASS += 1
        print(f"ok   {label}")
    else:
        FAIL += 1
        print(f"BAD  {label} {detail}")


WS = {"Connection": "Upgrade", "Upgrade": "websocket", "Sec-WebSocket-Version": "13", "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ=="}


def own(scope):
    return sorted(prod("ps", "-aq", "--filter", f"label=dev.marsh.scope={scope}").split())


# ---- setup ---------------------------------------------------------------------
for s in SOCK:
    cli(s, "pull", "-q", "busybox:latest")
for s in ("base", "b", "a"):
    cli(s, "rm", "-f", f"{s}-db", f"{s}-x")
    cli(s, "volume", "rm", f"{s}-vol")
    cli(s, "network", "rm", f"{s}-net")
for s in ("base", "b"):
    cli(s, "network", "create", f"{s}-net")
    cli(s, "volume", "create", f"{s}-vol")
    rc, out = cli(s, "run", "-d", "--name", f"{s}-db", "-v", f"{s}-vol:/d", "--network", f"{s}-net", "busybox", "sh", "-c", f"echo {s}-secret > /d/secret; sleep 1000")
    assert rc == 0, out
rc, out = cli("a", "run", "-d", "--name", "a-db", "busybox", "sleep", "1000")
assert rc == 0, out
FULL = {s: prod("inspect", f"{s}-db", "--format", "{{.Id}}") for s in ("base", "b")}
before = {s: (FULL[s], prod("inspect", f"{s}-db", "--format", "{{.State.Status}} {{.RestartCount}}")) for s in FULL}
check(prod("inspect", "base-db", "--format", '{{index .Config.Labels "dev.marsh.scope"}}') == "base", "base's container carries scope=base")
# exec ids: b creates one that a will try to steal
st, body = req("b", "POST", f"/v1.47/containers/b-db/exec", {"Cmd": ["echo", "b-exec"], "AttachStdout": True})
B_EXEC = json.loads(body)["Id"] if st == 201 else None
check(B_EXEC is not None, "b can create an exec in its own container")

# ---- SEC-16 / SEC-05 c1: the route matrix from a ------------------------------------
routes = [
    ("GET", "/containers/{c}/json", None), ("GET", "/containers/{c}/top", None), ("GET", "/containers/{c}/logs?stdout=1", None),
    ("GET", "/containers/{c}/changes", None), ("GET", "/containers/{c}/export", None), ("GET", "/containers/{c}/stats?stream=0", None),
    ("GET", "/containers/{c}/archive?path=/d", None), ("HEAD", "/containers/{c}/archive?path=/d", None),
    ("PUT", "/containers/{c}/archive?path=/d", b""),
    ("POST", "/containers/{c}/start", None), ("POST", "/containers/{c}/stop?t=0", None), ("POST", "/containers/{c}/restart?t=0", None),
    ("POST", "/containers/{c}/kill", None), ("POST", "/containers/{c}/pause", None), ("POST", "/containers/{c}/unpause", None),
    ("POST", "/containers/{c}/rename?name=stolen", None), ("POST", "/containers/{c}/update", {"Memory": 536870912}),
    ("POST", "/containers/{c}/exec", {"Cmd": ["id"]}), ("POST", "/containers/{c}/attach?stream=1&stdout=1", None),
    ("POST", "/containers/{c}/resize?h=1&w=1", None), ("DELETE", "/containers/{c}?force=1", None),
    ("POST", "/commit?container={c}&repo=stolenimg", None),
    # the wait and websocket-attach cells (the stream routes that answer late)
    ("POST", "/containers/{c}/wait", None), ("POST", "/containers/{c}/wait?condition=removed", None),
    ("GET", "/containers/{c}/attach/ws?stream=1&stdout=1", None, WS),
    ("POST", "/commit?container={c}&repo=lane-a%2Fstolen", None),
]
EXPECT_ROWS = 6 * len(routes)
refused_ok = True
rows = 0
for s in ("base", "b"):
    full = FULL[s]
    for form, ref in (("full", full), ("short", full[:12]), ("name", f"{s}-db")):
        for m, p, body, *hd in routes:
            st, rb = req("a", m, "/v1.47" + p.replace("{c}", ref), body, hd[0] if hd else None)
            rows += 1
            if st not in (403, 404) or (st == 403 and b"marsh:" not in rb and m != "HEAD"):
                refused_ok = False
                print(f"     LEAK {s} {form} {m} {p} -> {st} {rb[:80]!r}")
check(rows == EXPECT_ROWS and rows >= 140, f"c1: the whole matrix ran ({rows} cells)")
check(refused_ok, f"c1: {rows} container route x id form x foreign-scope cells are refused or 'No such'")
now_ids = {s: (prod("inspect", f"{s}-db", "--format", "{{.Id}}"), prod("inspect", f"{s}-db", "--format", "{{.State.Status}} {{.RestartCount}}")) for s in FULL}
check(now_ids == before, "c1: foreign containers unchanged (same id, still running, no restart)")
check(prod("exec", "base-db", "cat", "/d/secret") == "base-secret" and prod("images", "-q", "stolenimg") == "", "c1: foreign data intact; nothing committed")
# exec by stolen id
for m, p, b in [("POST", f"/v1.47/exec/{B_EXEC}/start", {"Detach": False, "Tty": False}), ("GET", f"/v1.47/exec/{B_EXEC}/json", None), ("POST", f"/v1.47/exec/{B_EXEC}/resize?h=1&w=1", None)]:
    st, rb = req("a", m, p, b)
    check(st in (403, 404), f"c1: stolen exec id refused: {m} {p.split('/')[-1]}", f"{st} {rb[:60]!r}")
# networks and volumes
for s in ("base", "b"):
    nid = prod("network", "inspect", f"{s}-net", "--format", "{{.Id}}")
    for m, p, b in [("GET", f"/v1.47/networks/{nid}", None), ("GET", f"/v1.47/networks/{s}-net", None), ("POST", f"/v1.47/networks/{s}-net/connect", {"Container": "a-db"}),
                    ("POST", f"/v1.47/networks/{nid}/disconnect", {"Container": f"{s}-db", "Force": True}), ("DELETE", f"/v1.47/networks/{s}-net", None),
                    ("GET", f"/v1.47/volumes/{s}-vol", None), ("DELETE", f"/v1.47/volumes/{s}-vol?force=1", None)]:
        st, rb = req("a", m, p, b)
        check(st in (403, 404), f"c1: foreign {m} {p.split('/v1.47')[1][:34]}", f"{st} {rb[:60]!r}")
check(prod("volume", "ls", "-q", "--filter", "name=base-vol") == "base-vol" and prod("network", "ls", "-q", "--filter", "name=base-net") != "", "c1: foreign volume and network still exist")
# create-time references
for label, args in [
    ("-v base-vol", ["run", "--rm", "-v", "base-vol:/d", "busybox", "ls", "/d"]),
    ("--volumes-from base-db", ["run", "--rm", "--volumes-from", "base-db", "busybox", "ls", "/d"]),
    ("--network container:base-db", ["run", "--rm", "--network", "container:base-db", "busybox", "true"]),
    ("--pid container:base-db", ["run", "--rm", "--pid", "container:base-db", "busybox", "true"]),
    ("--network base-net", ["run", "--rm", "--network", "base-net", "busybox", "true"]),
    ("--label dev.marsh.scope=base", ["run", "--rm", "--label", "dev.marsh.scope=base", "busybox", "true"]),
]:
    rc, out = cli("a", *args)
    check(rc != 0 and ("marsh:" in out or "not found" in out or "No such" in out) and "base-secret" not in out, f"c1: a cannot {label}", out[:100])
# ---- lists and events -------------------------------------------------------------------
rc, out = cli("a", "ps", "-a", "--format", "{{.Names}}")
check(out.split() == ["a-db"], "c1: `docker ps -a` from a lists only a's objects", out)
rc, out = cli("a", "volume", "ls", "-q")
check("base-vol" not in out and "b-vol" not in out, "c1: `docker volume ls` hides other scopes", out)
rc, out = cli("a", "network", "ls", "--format", "{{.Name}}")
check("base-net" not in out and "b-net" not in out and "bridge" not in out and "host" not in out, "c1: `docker network ls` hides other scopes and the built-ins", out)
t0 = int(time.time())
cli("base", "run", "--rm", "--name", "base-x", "busybox", "true")
cli("b", "run", "--rm", "--name", "b-x", "busybox", "true")
cli("a", "run", "--rm", "--name", "a-x", "busybox", "true")
time.sleep(1)
rc, out = cli("a", "events", "--since", str(t0), "--until", str(int(time.time()) + 1))
check("a-x" in out and "base-x" not in out and "b-x" not in out and "base-db" not in out, "c1: events are filtered to a's scope", out[:200])
# c2: the "inspect by full id" cell for own objects works; own controls (c4)
a_full = prod("inspect", "a-db", "--format", "{{.Id}}")
st, rb = req("a", "GET", f"/v1.47/containers/{a_full}/json")
check(st == 200, "c4: own inspect by full id works")
rc, out = cli("a", "exec", "a-db", "echo", "own-exec")
check(rc == 0 and "own-exec" in out, "c4: own exec works")
cli("a", "run", "--name", "a-w", "busybox", "true")
rc, out = cli("a", "wait", "a-w")
check(rc == 0 and out == "0", "c4: own wait works", out)
st, rb = req("a", "GET", f"/v1.47/containers/{a_full}/attach/ws?stream=1&stdout=1", headers=WS)
check(st in (101, 200), "c4: own websocket attach is admitted", f"{st} {rb[:60]!r}")
cli("a", "rm", "-f", "a-w")
# ---- RST-11: destructive commands in a lane ---------------------------------------------
cli("a", "run", "-d", "--name", "a-y", "-v", "a-vol:/d", "busybox", "sleep", "1000")
cli("a", "network", "create", "a-net")
rc1, o1 = cli("a", "rm", "-f", *cli("a", "ps", "-aq")[1].split())
rc2, o2 = cli("a", "volume", "prune", "-af")
rc3, o3 = cli("a", "network", "prune", "-f")
rc4, o4 = cli("a", "system", "prune", "-af", "--volumes")
print("   system prune:", o4[:160].replace("\n", " | "))
check("marsh:" in o4 or rc4 != 0, "c3: `system prune` (images/builder) is refused with marsh:")
check(own("a") == [], "c2: a's own containers are gone", str(own("a")))
for s in ("base", "b"):
    check(prod("inspect", f"{s}-db", "--format", "{{.Id}}") == FULL[s] and prod("exec", f"{s}-db", "cat", "/d/secret") == f"{s}-secret", f"c1: {s} keeps its container id and sentinel after a's destructive commands")
    check(prod("volume", "ls", "-q", "--filter", f"name={s}-vol") == f"{s}-vol" and prod("network", "ls", "-q", "--filter", f"name={s}-net") != "", f"c1: {s}'s volume and network survive")
# cleanup
for s in ("base", "b"):
    cli(s, "rm", "-f", f"{s}-db")
    cli(s, "volume", "rm", f"{s}-vol")
    cli(s, "network", "rm", f"{s}-net")
print(f"== pass={PASS} fail={FAIL}")
sys.exit(1 if FAIL else 0)
