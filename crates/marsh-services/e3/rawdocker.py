#!/usr/bin/env python3
"""Byte-exact attack cases against the filtered endpoint (SEC-02, SEC-03, SEC-23).

Runs in the sandbox VM. Ground truth is the production engine, read through
`docker exec nfx-e3-engine docker -H unix:///var/run/docker.sock`, never the
filter. Every attack creates (if it gets through) a container named evil-*.

  rawdocker.py SCOPE_SOCKET [--calibrate]

--calibrate expects the filter to be off (break mode filter-off): it reports
which rows the engine itself would have accepted (the witnesses).
"""
import json, socket, subprocess, sys, time

SOCK = sys.argv[1]
CALIBRATE = "--calibrate" in sys.argv
# Inside a socket holder there is no way to ask the engine directly: statuses
# and markers are checked, "created" is left to the caller (no_evil in lib.sh).
NO_TRUTH = "--no-truth" in sys.argv
N = [0]


def prod(*args):
    if NO_TRUTH:
        return ""
    return subprocess.run(["docker", "exec", "nfx-e3-engine", "docker", "-H", "unix:///var/run/docker.sock", *args],
                          capture_output=True, text=True).stdout


def evil_names():
    return sorted(n for n in prod("ps", "-a", "--format", "{{.Names}}").split() if n.startswith("evil-"))


def parse_responses(buf):
    out, i = [], 0
    while True:
        j = buf.find(b"\r\n\r\n", i)
        if j < 0 or not buf[i:i + 5] == b"HTTP/":
            break
        head = buf[i:j].decode("latin1").split("\r\n")
        status = int(head[0].split()[1])
        h = {k.lower(): v.strip() for k, _, v in (l.partition(":") for l in head[1:])}
        k = j + 4
        body = b""
        if h.get("transfer-encoding", "").lower() == "chunked":
            while True:
                e = buf.find(b"\r\n", k)
                if e < 0:
                    break
                n = int(buf[k:e], 16)
                k = e + 2
                if n == 0:
                    k += 2
                    break
                body += buf[k:k + n]
                k += n + 2
        elif "content-length" in h:
            n = int(h["content-length"])
            body = buf[k:k + n]
            k += n
        else:
            body = buf[k:]
            k = len(buf)
        out.append((status, h, body))
        i = k
    return out


def send(chunks, wait=1.5, delay=0.0):
    s = socket.socket(socket.AF_UNIX)
    s.settimeout(wait)
    s.connect(SOCK)
    if isinstance(chunks, bytes):
        chunks = [chunks]
    for c in chunks:
        try:
            s.sendall(c)
        except OSError:
            break
        if delay:
            time.sleep(delay)
    buf = b""
    closed = False
    t0 = time.time()
    while time.time() - t0 < wait:
        try:
            d = s.recv(65536)
        except socket.timeout:
            break
        except OSError:
            closed = True
            break
        if not d:
            closed = True
            break
        buf += d
    s.close()
    return buf, closed


def create_body(extra_hc=None, top=None, raw=None):
    if raw is not None:
        return raw
    hc = {"Privileged": True}
    hc.update(extra_hc or {})
    b = {"Image": "busybox", "Cmd": ["true"], "HostConfig": hc}
    b.update(top or {})
    return json.dumps(b).encode()


def post(path, body, extra_headers="", method="POST"):
    return (f"{method} {path} HTTP/1.1\r\nHost: d\r\nContent-Type: application/json\r\n{extra_headers}"
            f"Content-Length: {len(body)}\r\n\r\n").encode() + body


def evil_create(path="/v1.47/containers/create", **kw):
    N[0] += 1
    body = create_body(top={"HostConfig": {"Privileged": True}}, **kw) if "raw" not in kw else kw["raw"]
    return post(f"{path}?name=evil-{N[0]}", body)


results = []
for _n in evil_names():
    prod("rm", "-f", _n)  # leftovers of an earlier run would hide this run's creates


def case(name, req, expect_status=None, expect_close=None, any_of=None, delay=0.0, wait=1.5):
    """expect_status: list of statuses, one per response in order; None entries
    are not checked. any_of: allowed first statuses."""
    before = set(evil_names())
    buf, closed = send(req, wait=wait, delay=delay)
    rs = parse_responses(buf)
    st = [r[0] for r in rs]
    msgs = [r[2][:80].decode("latin1", "replace") for r in rs]
    after = set(evil_names())
    created = sorted(after - before)
    ok = True
    why = []
    if expect_status is not None:
        if len(st) < len(expect_status) or any(e is not None and e != s and not (isinstance(e, tuple) and s in e) for e, s in zip(expect_status, st)):
            ok = False
            why.append(f"status {st} != {expect_status}")
    if any_of is not None and (not st or st[0] not in any_of):
        ok = False
        why.append(f"status {st} not in {any_of}")
    if expect_close is not None and closed != expect_close:
        ok = False
        why.append(f"close={closed} != {expect_close}")
    refusal_marker = all(m.startswith('{"message":"marsh:') for s, m in zip(st, msgs) if s in (403, 413, 417, 431, 429) or (s == 400 and "marsh" in m))
    if not refusal_marker:
        ok = False
        why.append("refusal without marsh: marker")
    if created:
        ok = False
        why.append(f"CREATED {created}")
    results.append({"case": name, "ok": ok, "status": st, "closed": closed, "created": created, "why": why})
    return created


def setup_cases():
    c = evil_create
    pr = b"{" + b'"Image":"busybox","HostConfig":{"Privileged":true}' + b"}"
    # --- SEC-03 framing ---
    case("ping+Upgrade tcp, then create", b"GET /_ping HTTP/1.1\r\nHost: d\r\nUpgrade: tcp\r\nConnection: Upgrade\r\n\r\n" + c(), [200, 403])
    case("attach unknown with Upgrade, then create",
         b"POST /v1.47/containers/nonexistent/attach?stream=1&stdout=1 HTTP/1.1\r\nHost: d\r\nUpgrade: tcp\r\nConnection: Upgrade\r\n\r\n" + c(), [(403, 404), 403])
    case("exec start bad id with Upgrade, then create",
         b"POST /v1.47/exec/" + b"0" * 64 + b"/start HTTP/1.1\r\nHost: d\r\nUpgrade: tcp\r\nConnection: Upgrade\r\nContent-Length: 2\r\n\r\n{}" + c(), [(403, 404), 403])
    case("ping and create pipelined in one write", b"GET /_ping HTTP/1.1\r\nHost: d\r\n\r\n" + c(), [200, 403])
    case("session with Upgrade h2c on a bad route, then create", b"POST /v1.47/containers/create HTTP/1.1\r\nHost: d\r\nUpgrade: h2c\r\nConnection: Upgrade\r\nContent-Length: 0\r\n\r\n" + c(), any_of=[400, 403], expect_close=True)
    base = b"POST /v1.47/containers/create?name=evil-x HTTP/1.1\r\nHost: d\r\n"
    for name, hdr in [
        ("CL and TE", b"Content-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n"),
        ("two CL", b"Content-Length: 4\r\nContent-Length: 4\r\n\r\nabcd"),
        ("TE chunked, identity", b"Transfer-Encoding: chunked, identity\r\n\r\n0\r\n\r\n"),
        ("TE xchunked", b"Transfer-Encoding: xchunked\r\n\r\n0\r\n\r\n"),
        ("TE gzip, chunked", b"Transfer-Encoding: gzip, chunked\r\n\r\n0\r\n\r\n"),
        ("chunk size +10", b"Transfer-Encoding: chunked\r\n\r\n+a\r\n0123456789\r\n0\r\n\r\n"),
        ("chunk size -1", b"Transfer-Encoding: chunked\r\n\r\n-1\r\nx\r\n0\r\n\r\n"),
        ("chunk size overflow", b"Transfer-Encoding: chunked\r\n\r\nfffffffffffffffff\r\nx\r\n0\r\n\r\n"),
        ("trailers", b"Transfer-Encoding: chunked\r\n\r\n0\r\nContent-Length: 5\r\n\r\n"),
        ("obs-fold", b"X-A: b\r\n c\r\nContent-Length: 0\r\n\r\n"),
        ("space before colon", b"X-A : b\r\nContent-Length: 0\r\n\r\n"),
        ("NUL in header", b"X-A: b\x00c\r\nContent-Length: 0\r\n\r\n"),
        ("bare LF header", b"X-A: b\nContent-Length: 0\r\n\r\n"),
        ("33 KiB header", b"X-A: " + b"a" * 33000 + b"\r\nContent-Length: 0\r\n\r\n"),
        ("101 fields", b"".join(b"X-%d: v\r\n" % i for i in range(101)) + b"Content-Length: 0\r\n\r\n"),
    ]:
        case(name, base + hdr, any_of=[400, 431], expect_close=True)
    case("bare-LF smuggling of a privileged create",
         b"GET /_ping HTTP/1.1\nHost:d\n\nPOST /v1.47/containers/create?name=evil-lf HTTP/1.1\nHost:d\nContent-Length: %d\n\n" % len(pr) + pr
         + b"\r\n\r\nGET /_ping HTTP/1.1\r\nHost: d\r\n\r\n", any_of=[400], expect_close=True)
    case("policy 403 then an allowed request", c() + b"GET /_ping HTTP/1.1\r\nHost: d\r\n\r\n", [403, 200])
    case("create sent 1 byte per 5 ms", c(), [403], delay=0.005, wait=6)
    case("Expect: 100-continue", post("/v1.47/containers/create?name=evil-e", pr, "Expect: 100-continue\r\n"), [417], expect_close=True)
    case("HTTP/1.0 pipelining", c().replace(b"HTTP/1.1", b"HTTP/1.0") + c().replace(b"HTTP/1.1", b"HTTP/1.0"), [403])
    case("h2 prior-knowledge preface", b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n", any_of=[400, 403], expect_close=True)
    case("CONNECT", b"CONNECT /containers/create HTTP/1.1\r\nHost: d\r\n\r\n", any_of=[400, 403])
    case("h2c on a plain route", b"GET /v1.47/_ping HTTP/1.1\r\nHost: d\r\nUpgrade: h2c\r\nConnection: Upgrade, HTTP2-Settings\r\nHTTP2-Settings: AAMAAABkAAQAoAAAAAIAAAAA\r\n\r\n", any_of=[400, 403], expect_close=True)
    # --- SEC-02 routes ---
    for p in ["/containers/create", "/v1.47/containers/create", "/v1.47.0/containers/create", "/v01.47/containers/create", "/v1.24/containers/create",
              "//containers/create", "/v1.47//containers/create", "/v1.47/./containers/create", "/v1.47/../v1.47/containers/create",
              "/v1.47/containers/%63reate", "/v1.47/containers%2Fcreate", "/v1.47/containers/create/", "/v1.47/%2e%2e/containers/create"]:
        N[0] += 1
        r = post(f"{p}?name=evil-r{N[0]}", pr)
        if p in ("/containers/create", "/v1.47/containers/create"):
            case(f"route {p} (privileged body)", r, [403])
        else:
            case(f"route {p}", r, any_of=[400, 403])
    case("absolute-form target", post("http://x/v1.47/containers/create?name=evil-abs", pr), any_of=[400, 403])
    case("lowercase method", post("/v1.47/containers/create?name=evil-lc", pr, method="post"), any_of=[400, 403])
    case("PUT create", post("/v1.47/containers/create?name=evil-put", pr, method="PUT"), [403])
    case("body on GET", post("/v1.47/containers/json", b"{}", method="GET"), [403])
    # --- body semantics ---
    P = b'"Image":"busybox"'
    for name, body in [
        ("W-case HostConfig.privileged", b'{' + P + b',"HostConfig":{"privileged":true}}'),
        ("hostconfig.Privileged", b'{' + P + b',"hostconfig":{"Privileged":true}}'),
        ("PRIVILEGED", b'{' + P + b',"HostConfig":{"PRIVILEGED":true}}'),
        ("binds lower-case", b'{' + P + b',"HostConfig":{"binds":["/:/h"]}}'),
        ("duplicate Privileged", b'{' + P + b',"HostConfig":{"Privileged":false,"Privileged":true}}'),
        ("duplicate mixed case", b'{' + P + b',"HostConfig":{"Privileged":false,"privileged":true}}'),
        ("duplicate HostConfig", b'{' + P + b',"HostConfig":{},"HostConfig":{"Privileged":true}}'),
        ("Privileged as string", b'{' + P + b',"HostConfig":{"Privileged":"true"}}'),
        ("Privileged as 1", b'{' + P + b',"HostConfig":{"Privileged":1}}'),
        ("Privileged as null", b'{' + P + b',"HostConfig":{"Privileged":null}}'),
        ("Privileged as [true]", b'{' + P + b',"HostConfig":{"Privileged":[true]}}'),
        ("W-trailing", b'{' + P + b',"HostConfig":{"Privileged":true}} xyz'),
        ("two documents", b'{' + P + b',"HostConfig":{}}{"HostConfig":{"Privileged":true}}'),
        ("BOM", b'\xef\xbb\xbf{' + P + b',"HostConfig":{"Privileged":true}}'),
        ("UTF-16", '{"Image":"busybox","HostConfig":{"Privileged":true}}'.encode("utf-16")),
        ("invalid UTF-8", b'{' + P + b',"HostConfig":{"Privileged":true},"x":"\xff"}'),
        ("depth 10000", b"[" * 10000 + b"]" * 10000),
        ("non-object", b"[1]"),
        ("kelvin-sign key", '{"Image":"busybox","HostConfig":{"Privileged":true},"\u212aey":1}'.encode()),
        ("long-s HostConfig", '{"Image":"busybox","Ho\u017ftConfig":{"Privileged":true}}'.encode()),
    ]:
        N[0] += 1
        case(f"body: {name}", post(f"/v1.47/containers/create?name=evil-b{N[0]}", body), [403])
    case("legacy start with HostConfig", post("/v1.23/containers/x/start", b'{"HostConfig":{"Privileged":true}}'), [403])
    case("start with a body (discarded, unknown container)", post("/v1.47/containers/x/start", b'{"HostConfig":{"Privileged":true}}'), [(403, 404)])
    case("update with Devices", post("/v1.47/containers/x/update", b'{"Devices":[{"PathOnHost":"/dev/sda"}]}'), [(403, 404)])
    for path in ["/v1.47/images/load", "/v1.47/images/create?fromSrc=http://x/y", "/v1.47/images/x/push", "/v1.47/auth", "/v1.47/commit?container=x&repo=y",
                 "/v1.47/build?remote=http://x/ctx.tar", "/v1.47/plugins", "/v1.47/swarm", "/v1.47/services", "/v1.47/secrets", "/v1.47/configs", "/v1.47/nodes", "/v1.47/system/df"]:
        for m in ("GET", "POST", "PUT", "DELETE"):
            if m != "POST" and ("images" in path or "auth" in path or "commit" in path or "build" in path):
                continue
            case(f"refused route {m} {path}", post(path, b"", method=m) if m == "POST" else f"{m} {path} HTTP/1.1\r\nHost: d\r\n\r\n".encode(), [(403, 404)])


def controls():
    ok = []
    for name, req, want in [
        ("ping", b"GET /_ping HTTP/1.1\r\nHost: d\r\n\r\n", 200),
        ("version", b"GET /v1.47/version HTTP/1.1\r\nHost: d\r\n\r\n", 200),
        ("list", b"GET /v1.47/containers/json?all=1 HTTP/1.1\r\nHost: d\r\n\r\n", 200),
    ]:
        buf, _ = send(req)
        rs = parse_responses(buf)
        ok.append({"control": name, "ok": bool(rs) and rs[0][0] == want, "status": [r[0] for r in rs]})
    return ok


setup_cases()
ctl = controls()
bad = [r for r in results if not r["ok"]]
witness = [r for r in results if r["created"]]
if CALIBRATE:
    print(json.dumps({"calibrate": True, "witness_rows": [r["case"] for r in witness], "total": len(results)}, indent=1))
else:
    for r in results:
        if not r["ok"]:
            print("FAIL", r["case"], r["status"], r["closed"], r["why"])
    print(json.dumps({"total": len(results), "failed": len(bad), "created_evil": evil_names(), "controls": ctl}))
    for c in ctl:
        print(("ok   " if c["ok"] else "BAD  ") + f"control {c['control']}: status {c['status']}")
    print(("ok   " if not bad else "BAD  ") + f"{len(results) - len(bad)} of {len(results)} raw cases refused or answered as the table says")
    sys.exit(1 if bad or any(not c["ok"] for c in ctl) else 0)
