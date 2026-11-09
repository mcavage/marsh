#!/usr/bin/env python3
"""Regenerate the byte-exact rawdocker cases (benign.json, encodings.json,
framing.json) so every Content-Length is right.

The JSON files are the artifact; this keeps them reviewable. Segments are
latin-1 text (so \\r and \\n are exact), {"hex": ...}, {"fill": N, "text": T},
{"trickle": T, "bytes": B, "delay_ms": D} or {"delay_ms": D}.

Expectations describe the *filtered* endpoint (TEST-PLAN SEC-02 / SEC-03):
a local refusal is 403 with a `marsh: ` body, framing and parser anomalies are
400 (431 where sized) and the connection closes, and nothing named nfxraw-*
may exist afterwards. E3 may refine individual rows with the QA lead's
sign-off; against a plain engine run with --calibrate.

Run from anywhere: python3 generate.py
"""
import json
import pathlib
import tarfile
import io

HERE = pathlib.Path(__file__).resolve().parent
IMAGE = "busybox:1.37@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec"
HOST = "Host: d\r\n"
JSON_T = "Content-Type: application/json\r\n"
REFUSE = [403]
PARSE = [[400, 431]]
EITHER = [[400, 403]]

counter = 0


def next_name():
    global counter
    counter += 1
    return f"nfxraw-{counter}"


def create_body(host_config=None, **top):
    doc = {"Image": IMAGE, "Cmd": ["true"], "HostConfig": host_config if host_config is not None else {"Privileged": True}}
    doc.update(top)
    return json.dumps(doc, separators=(",", ":"))


def blen(body):
    return len(body.encode("latin-1"))


def req(method, path, body="", extra="", lf="\r\n", version="HTTP/1.1", length=True):
    head = f"{method} {path} {version}{lf}Host: d{lf}"
    if body or method in ("POST", "PUT"):
        head += f"Content-Type: application/json{lf}"
    head += extra.replace("\r\n", lf)
    if length and (body or method in ("POST", "PUT")):
        head += f"Content-Length: {blen(body)}{lf}"
    return head + lf + body


def post(path, body, extra="", lf="\r\n"):
    return req("POST", path, body, extra, lf)


def smuggled(name):
    return post(f"/containers/create?name={name}", create_body())


def case(cid, description, send, expect, responses=None, cleanup=()):
    out = {"id": cid, "description": description, "send": send}
    if responses is not None:
        out["responses"] = responses
    out["expect"] = expect
    out["cleanup"] = list(cleanup)
    return out


def rm(*names):
    return [{"method": "DELETE", "path": f"/containers/{n}?force=1"} for n in names]


def write(name, doc):
    (HERE / name).write_text(json.dumps(doc, indent=1, ensure_ascii=True) + "\n")


# ---- benign ------------------------------------------------------------------
write("benign.json", {
    "description": "Benign requests: a plain engine and a filtering endpoint both answer these.",
    "cases": [
        case("B01-ping", "GET /_ping", ["GET /_ping HTTP/1.1\r\nHost: d\r\n\r\n"],
             {"statuses": [200], "closed": False}, 1),
        case("B02-version-prefix-ping", "GET /v1.47.0/_ping (versioned path)",
             ["GET /v1.47.0/_ping HTTP/1.1\r\nHost: d\r\n\r\n"], {"statuses": [200], "closed": False}, 1),
        case("B03-ping-then-ping", "two pipelined pings on one connection",
             ["GET /_ping HTTP/1.1\r\nHost: d\r\n\r\nGET /_ping HTTP/1.1\r\nHost: d\r\n\r\n"],
             {"statuses": [200, 200], "closed": False}, 2),
    ],
})

# ---- encodings (SEC-02 corpus) ---------------------------------------------------
enc = []


def encoding(slug, description, send_for, statuses=REFUSE, create=True, closed=None, plan="", **kw):
    """send_for(name) -> list of segments (or one string)."""
    name = next_name()
    send = send_for(name)
    send = [send] if isinstance(send, str) else send
    expect = {"statuses": statuses, "marsh_refusals": True}
    if closed is not None:
        expect["closed"] = closed
    if any(isinstance(x, list) and 400 in x for x in statuses):
        expect["close_on_400"] = True
    if create:
        expect["forbidden_objects"] = [name]
    enc.append(case(f"E{len(enc) + 1:02d}-{slug}", description, send, expect, 1, rm(name) if create else ()))


def on_create(body_fn, path="/containers/create"):
    return lambda n: post(f"{path}?name={n}", body_fn())


# 1 key casing
def hc(**kv):
    return create_body(kv)


encoding("key-case-lower", "hostconfig/privileged lowercase (Go's decoder matches case-insensitively)",
         lambda n: post(f"/containers/create?name={n}", json.dumps(
             {"image": IMAGE, "cmd": ["true"], "hostconfig": {"privileged": True}}, separators=(",", ":"))))
encoding("key-case-hostconfig-title", "hostconfig.Privileged",
         lambda n: post(f"/containers/create?name={n}", json.dumps(
             {"Image": IMAGE, "hostconfig": {"Privileged": True}}, separators=(",", ":"))))
encoding("key-case-upper", "HOSTCONFIG/PRIVILEGED",
         lambda n: post(f"/containers/create?name={n}", json.dumps(
             {"IMAGE": IMAGE, "CMD": ["true"], "HOSTCONFIG": {"PRIVILEGED": True}}, separators=(",", ":"))))
encoding("key-case-binds-lower", "HostConfig.binds lowercase mounting /",
         lambda n: post(f"/containers/create?name={n}", json.dumps(
             {"Image": IMAGE, "HostConfig": {"binds": ["/:/h"]}}, separators=(",", ":"))))
encoding("key-case-binds-mixed", "hostCONFIG.bINDS mounting /",
         lambda n: post(f"/containers/create?name={n}", json.dumps(
             {"Image": IMAGE, "hostCONFIG": {"bINDS": ["/:/h"]}}, separators=(",", ":"))))
# 2 duplicates incl. mixed case
encoding("duplicate-key-last-wins", "HostConfig twice: benign first, privileged last",
         lambda n: post(f"/containers/create?name={n}",
                        '{"Image":"%s","HostConfig":{},"HostConfig":{"Privileged":true}}' % IMAGE))
encoding("duplicate-key-first-wins", "HostConfig twice: privileged first, benign last",
         lambda n: post(f"/containers/create?name={n}",
                        '{"Image":"%s","HostConfig":{"Privileged":true},"HostConfig":{}}' % IMAGE))
encoding("duplicate-key-mixed-case", "HostConfig benign, hostconfig privileged",
         lambda n: post(f"/containers/create?name={n}",
                        '{"Image":"%s","HostConfig":{},"hostconfig":{"Privileged":true}}' % IMAGE))
encoding("duplicate-inner-key-mixed-case", "Privileged false, then PRIVILEGED true",
         lambda n: post(f"/containers/create?name={n}",
                        '{"Image":"%s","HostConfig":{"Privileged":false,"PRIVILEGED":true}}' % IMAGE))
# 3 types
for slug, value in (("string-true", '"true"'), ("number-one", "1"), ("null", "null"), ("array-true", "[true]")):
    encoding(f"type-privileged-{slug}", f"Privileged: {value}",
             (lambda v: lambda n: post(f"/containers/create?name={n}",
                                       '{"Image":"%s","HostConfig":{"Privileged":%s}}' % (IMAGE, v)))(value),
             statuses=EITHER)
# 4 bodies
encoding("trailing-xyz", "valid privileged JSON followed by xyz",
         lambda n: post(f"/containers/create?name={n}", create_body() + "xyz"), statuses=EITHER)
encoding("two-documents", "a benign JSON value, then a privileged one",
         lambda n: post(f"/containers/create?name={n}", '{"Image":"%s"}\n%s' % (IMAGE, create_body())),
         statuses=EITHER)
encoding("bom-prefixed", "UTF-8 byte order mark before a privileged body",
         lambda n: post(f"/containers/create?name={n}", "\u00ef\u00bb\u00bf" + create_body()), statuses=EITHER)


def utf16(n):
    body = create_body().encode("utf-16")
    head = (f"POST /containers/create?name={n} HTTP/1.1\r\n{HOST}Content-Type: application/json; charset=utf-16\r\n"
            f"Content-Length: {len(body)}\r\n\r\n")
    return [head, {"hex": body.hex()}]


encoding("utf16-body", "a privileged body encoded as UTF-16 with a BOM", utf16, statuses=EITHER)


def bad_utf8(n):
    body = (create_body()[:-1] + ',"X":"').encode() + b"\xff\xfe\xc0" + b'"}'
    head = f"POST /containers/create?name={n} HTTP/1.1\r\n{HOST}{JSON_T}Content-Length: {len(body)}\r\n\r\n"
    return [head, {"hex": body.hex()}]


encoding("invalid-utf8", "a privileged body with invalid UTF-8 inside a string", bad_utf8, statuses=EITHER)


def deep(n, depth=10000):
    prefix = '{"Image":"%s","HostConfig":{"Privileged":true},"Deep":' % IMAGE
    suffix = "}"
    length = len(prefix) + 2 * depth + len(suffix)
    return [f"POST /containers/create?name={n} HTTP/1.1\r\n{HOST}{JSON_T}Content-Length: {length}\r\n\r\n{prefix}",
            {"fill": depth, "text": "["}, {"fill": depth, "text": "]"}, suffix]


encoding("depth-10000", "privileged create with nesting depth 10000", deep, statuses=EITHER)


def sixteen_mib(n):
    prefix = '{"Image":"%s","HostConfig":{"Privileged":true},"Pad":"' % IMAGE
    suffix = '"}'
    total = 16 * 1024 * 1024
    return [f"POST /containers/create?name={n} HTTP/1.1\r\n{HOST}{JSON_T}Content-Length: {total}\r\n\r\n{prefix}",
            {"fill": total - len(prefix) - len(suffix), "text": "x"}, suffix]


encoding("body-16-mib", "a 16 MiB privileged create", sixteen_mib, statuses=[[400, 403, 413]])
# 5 routes
for slug, path, method in (
        ("unversioned", "/containers/create", "POST"),
        ("v1.24-below-minimum", "/v1.24/containers/create", "POST"),
        ("v1.47", "/v1.47/containers/create", "POST"),
        ("v1.47.0", "/v1.47.0/containers/create", "POST"),
        ("v01.47", "/v01.47/containers/create", "POST"),
        ("double-slash", "//containers/create", "POST"),
        ("versioned-double-slash", "/v1.47.0//containers/create", "POST"),
        ("dot-segment", "/./containers/create", "POST"),
        ("pct-encoded-letter", "/containers/%63reate", "POST"),
        ("pct-encoded-slash", "/containers%2Fcreate", "POST"),
        ("pct-encoded-versioned", "/v1.47.0/containers/%63reate", "POST"),
        ("absolute-form", "http://d/containers/create", "POST"),
        ("trailing-slash", "/containers/create/", "POST"),
        ("lowercase-method", "/containers/create", "post"),
        ("put-method", "/containers/create", "PUT")):
    encoding(f"route-{slug}", f"privileged create via {method} {path}",
             (lambda p, m: lambda n: req(m, f"{p}?name={n}", create_body()))(path, method),
             statuses=REFUSE if slug in ("unversioned", "v1.47", "v1.47.0") else EITHER)
# 6 body on a GET
encoding("get-with-body", "a body on GET /containers/json",
         lambda n: req("GET", "/containers/json", create_body()), statuses=EITHER, create=False)
# 7 legacy start / update
encoding("legacy-start-hostconfig", "POST /v1.23/containers/X/start carrying a HostConfig",
         lambda n: post("/v1.23/containers/nfx-none/start", '{"Privileged":true,"Binds":["/:/h"]}'),
         statuses=EITHER, create=False)
encoding("update-devices", "POST /containers/X/update with Resources.Devices",
         lambda n: post("/containers/nfx-none/update", '{"Devices":[{"PathOnHost":"/dev/loop0","PathInContainer":"/dev/x","CgroupPermissions":"rwm"}]}'),
         statuses=EITHER, create=False)
encoding("update-above-maxima", "POST /containers/X/update with huge limits",
         lambda n: post("/containers/nfx-none/update", '{"Memory":9223372036854771712,"NanoCpus":999999999999,"PidsLimit":-1}'),
         statuses=EITHER, create=False)
# 8 images and registry routes


def traversal_tar():
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w") as tar:
        info = tarfile.TarInfo("../../nfx-escape")
        info.size = 4
        tar.addfile(info, io.BytesIO(b"nfx\n"))
    return buffer.getvalue()


def images_load(n):
    body = traversal_tar()
    return [f"POST /images/load HTTP/1.1\r\n{HOST}Content-Type: application/x-tar\r\nContent-Length: {len(body)}\r\n\r\n",
            {"hex": body.hex()}]


encoding("images-load", "POST /images/load with a traversal tar", images_load, statuses=EITHER, create=False)
encoding("images-create-fromsrc", "POST /images/create?fromSrc=",
         lambda n: post("/images/create?fromSrc=http%3A%2F%2Fexample.invalid%2Fx.tar&repo=nfx%2Fx", ""),
         statuses=EITHER, create=False)
encoding("images-push", "POST /images/busybox/push",
         lambda n: post("/images/busybox/push", ""), statuses=EITHER, create=False)
encoding("auth", "POST /auth", lambda n: post("/auth", '{"username":"u","password":"p","serveraddress":"example.invalid"}'),
         statuses=EITHER, create=False)
encoding("commit-other-scope", "POST /commit?repo=other-scope",
         lambda n: post("/commit?container=nfx-none&repo=nfx-other-scope%2Fx", ""), statuses=EITHER, create=False)
# 9 every method on swarm/plugin routes
for route in ("/plugins", "/swarm", "/services", "/secrets", "/configs", "/nodes"):
    for method in ("GET", "POST", "PUT", "DELETE"):
        encoding(f"{route.strip('/')}-{method.lower()}", f"{method} {route}",
                 (lambda r, m: lambda n: req(m, r, "{}" if m in ("POST", "PUT") else ""))(route, method),
                 statuses=EITHER, create=False)

write("encodings.json", {
    "description": "SEC-02 corpus: each case is a forbidden request on its own connection. Expect a local "
                   "refusal (403 `marsh: ` or 400) and no object named nfxraw-* afterwards.",
    "cases": enc})

# ---- framing (SEC-03) -------------------------------------------------------------
fr = []


def framing(slug, description, send_for, statuses, closed=None, responses=None, objects=1):
    names = [next_name() for _ in range(objects)]
    send = send_for(*names) if objects else send_for()
    send = [send] if isinstance(send, str) else send
    expect = {"statuses": statuses}
    if closed is not None:
        expect["closed"] = closed
    if any(s == 403 or (isinstance(s, list) and 403 in s) for s in statuses):
        expect["marsh_refusals"] = True
    if names:
        expect["forbidden_objects"] = names
    fr.append(case(f"F{len(fr) + 1:02d}-{slug}", description, send, expect, responses, rm(*names)))


PING_UPGRADE = "GET /_ping HTTP/1.1\r\nHost: d\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n"
framing("ping-upgrade-tcp", "GET /_ping with Upgrade: tcp stays HTTP; a pipelined create is refused",
        lambda a: PING_UPGRADE + smuggled(a), [200, 403], False, 2, 1)
framing("ping-upgrade-then-create-later", "the same, with the create sent as a later segment",
        lambda a: [PING_UPGRADE, {"delay_ms": 300}, smuggled(a)], [200, 403], False, 2, 1)
framing("attach-nonexistent-upgrade", "attach to a nonexistent ID with Upgrade: engine 404, stays HTTP, follow-up refused",
        lambda a: ["POST /containers/nfx-none/attach?stream=1&stdout=1 HTTP/1.1\r\nHost: d\r\nConnection: Upgrade\r\n"
                   "Upgrade: tcp\r\nContent-Length: 0\r\n\r\n" + smuggled(a)], [404, 403], False, 2, 1)
framing("exec-bad-start-upgrade", "/exec/<bad>/start with Upgrade: 404, follow-up refused",
        lambda a: [f"POST /exec/nfx-bad/start HTTP/1.1\r\nHost: d\r\n{JSON_T}Connection: Upgrade\r\nUpgrade: tcp\r\n"
                   f"Content-Length: 2\r\n\r\n{{}}" + smuggled(a)], [404, 403], False, 2, 1)
framing("ping-create-pipelined", "_ping and a privileged create pipelined in one write: ping 200, create 403",
        lambda a: "GET /_ping HTTP/1.1\r\nHost: d\r\n\r\n" + smuggled(a), [200, 403], False, 2, 1)
framing("policy-403-then-allowed", "a refused create followed by an allowed ping: the ping succeeds as itself",
        lambda a: smuggled(a) + "GET /_ping HTTP/1.1\r\nHost: d\r\n\r\n", [403, 200], False, 2, 1)
framing("slow-create-trickle", "a privileged create sent 1 byte per 5 ms (under the progress deadline)",
        lambda a: [{"trickle": smuggled(a), "bytes": 1, "delay_ms": 5}], [403], None, 1, 1)
framing("expect-100-continue", "Expect: 100-continue: 417, nothing forwarded",
        lambda a: [post(f"/containers/create?name={a}", create_body(), extra="Expect: 100-continue\r\n")],
        [417], None, 1, 1)
framing("http10-keepalive-pipelining", "HTTP/1.0 keep-alive pipelining: the create is refused",
        lambda a: "GET /_ping HTTP/1.0\r\nConnection: keep-alive\r\n\r\n"
                  + req("POST", f"/containers/create?name={a}", create_body(), "Connection: keep-alive\r\n", version="HTTP/1.0"),
        [200, 403], None, 2, 1)
# parser anomalies: 400 (431 where sized), close, nothing forwarded (the follow-up must not run)
framing("bare-lf-head", "request line and headers end in bare LF (no CR), privileged body",
        lambda a, b: [post(f"/containers/create?name={a}", create_body(), lf="\n")], PARSE, True, 1, 2)
framing("bare-lf-ping-then-create", "a bare-LF ping, then a CRLF privileged create: 400/431, close, nothing forwarded",
        lambda a, b: ["GET /_ping HTTP/1.1\nHost: d\n\n", smuggled(a)], PARSE, True, 1, 2)


def cl_te(a, b):
    chunked = "%x\r\n%s\r\n0\r\n\r\n" % (blen(create_body()), create_body())
    return f"POST /containers/create?name={a} HTTP/1.1\r\n{HOST}{JSON_T}Transfer-Encoding: chunked\r\nContent-Length: {len(chunked)}\r\n\r\n{chunked}"


framing("cl-te-chunked-privileged", "Content-Length and Transfer-Encoding: chunked together", cl_te, PARSE, True, 1, 2)


def cl_te_smuggle(a, b):
    body = "0\r\n\r\n" + smuggled(b)
    return f"POST /containers/create?name={a} HTTP/1.1\r\n{HOST}{JSON_T}Transfer-Encoding: chunked\r\nContent-Length: {len(body)}\r\n\r\n{body}"


framing("cl-te-smuggled-create", "CL covers an empty chunked body plus a privileged request; TE ends it at the 0 chunk",
        cl_te_smuggle, PARSE, True, 1, 2)


def dup_cl(first_small):
    def build(a, b):
        benign = '{"Image":"%s","Cmd":["true"]}' % IMAGE
        smug = smuggled(b)
        small, large = blen(benign), blen(benign) + blen(smug)
        lengths = (small, large) if first_small else (large, small)
        return (f"POST /containers/create?name={a} HTTP/1.1\r\n{HOST}{JSON_T}Content-Length: {lengths[0]}\r\n"
                f"Content-Length: {lengths[1]}\r\n\r\n{benign}{smug}")
    return build


framing("duplicate-cl-first-small", "two Content-Length headers, the smaller first", dup_cl(True), PARSE, True, 1, 2)
framing("duplicate-cl-first-large", "two Content-Length headers, the larger first", dup_cl(False), PARSE, True, 1, 2)


def te_variant(value, objects_follow=True):
    def build(a, b):
        chunked = "%x\r\n%s\r\n0\r\n\r\n" % (blen(create_body()), create_body())
        return (f"POST /containers/create?name={a} HTTP/1.1\r\n{HOST}{JSON_T}Transfer-Encoding: {value}\r\n\r\n"
                f"{chunked}" + smuggled(b))
    return build


for slug, value in (("chunked-identity", "chunked, identity"), ("xchunked", "xchunked"), ("gzip-chunked", "gzip, chunked"),
                    ("identity-chunked", "identity, chunked"), ("chunked-space-tab", "chunked \t")):
    framing(f"te-{slug}", f"Transfer-Encoding: {value}", te_variant(value), PARSE, True, 1, 2)


def chunk_size(size_text):
    def build(a, b):
        return (f"POST /containers/create?name={a} HTTP/1.1\r\n{HOST}{JSON_T}Transfer-Encoding: chunked\r\n\r\n"
                f"{size_text}\r\n{create_body()}\r\n0\r\n\r\n" + smuggled(b))
    return build


for slug, text in (("plus-sign", "+%x" % blen(create_body())), ("negative", "-1"),
                   ("overflow", "ffffffffffffffffff"), ("underscore", "1_0")):
    framing(f"chunk-size-{slug}", f"chunk size {text!r}", chunk_size(text), PARSE, True, 1, 2)
framing("chunk-trailers-with-cl", "chunked body with trailers and a Content-Length",
        lambda a, b: (f"POST /containers/create?name={a} HTTP/1.1\r\n{HOST}{JSON_T}Transfer-Encoding: chunked\r\n"
                      f"Trailer: X-Nfx\r\nContent-Length: 0\r\n\r\n{blen(create_body()):x}\r\n{create_body()}\r\n"
                      f"0\r\nX-Nfx: {smuggled(b)!r}\r\n\r\n"), PARSE, True, 1, 2)
framing("obs-fold", "an obsolete line-folded header value",
        lambda a, b: (f"POST /containers/create?name={a} HTTP/1.1\r\n{HOST}Content-Type: application/json\r\n"
                      f" ; charset=utf-8\r\nContent-Length: {blen(create_body())}\r\n\r\n{create_body()}" + smuggled(b)),
        PARSE, True, 1, 2)
framing("nul-in-header", "a NUL byte in a header value",
        lambda a, b: post(f"/containers/create?name={a}", create_body(), extra="X-Nfx: a\x00b\r\n") + smuggled(b),
        PARSE, True, 1, 2)
framing("space-before-colon", "whitespace between a header name and the colon",
        lambda a, b: (f"POST /containers/create?name={a} HTTP/1.1\r\n{HOST}Content-Length : {blen(create_body())}\r\n"
                      f"{JSON_T}\r\n{create_body()}" + smuggled(b)), PARSE, True, 1, 2)
framing("header-33-kib", "a 33 KiB header value",
        lambda a, b: [f"POST /containers/create?name={a} HTTP/1.1\r\n{HOST}X-Nfx: ", {"fill": 33 * 1024, "text": "a"},
                      f"\r\n{JSON_T}Content-Length: {blen(create_body())}\r\n\r\n{create_body()}"], PARSE, True, 1, 2)
framing("headers-101", "101 header lines",
        lambda a, b: post(f"/containers/create?name={a}", create_body(),
                          extra="".join(f"X-Nfx-{i}: v\r\n" for i in range(100))) + smuggled(b), PARSE, True, 1, 2)
# protocol confusion
framing("h2-prior-knowledge", "the HTTP/2 prior-knowledge preface",
        lambda: ["PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"], EITHER, True, None, 0)
framing("h2c-upgrade-non-hijack", "Upgrade: h2c on a route that does not hijack",
        lambda: ["GET /containers/json HTTP/1.1\r\nHost: d\r\nConnection: Upgrade, HTTP2-Settings\r\nUpgrade: h2c\r\n"
                 "HTTP2-Settings: AAMAAABkAAQAAP__\r\n\r\n"], EITHER, True, None, 0)
framing("connect-method", "CONNECT",
        lambda: ["CONNECT d:80 HTTP/1.1\r\nHost: d:80\r\n\r\n"], EITHER, True, None, 0)

write("framing.json", {
    "description": "SEC-03 cases. Each declares its expected statuses and whether the connection closes; "
                   "cases that can carry a follow-up have a privileged create after the odd head. Nothing "
                   "named nfxraw-* may exist afterwards.",
    "cases": fr})
# ---- canonical (SEC-23, TEST-PLAN revision 3.1) ------------------------------------
# Recorded by `rawdocker.py --record-upstream`; checked with
#   rawdocker.py --compare-recording LOG --canonical cases/canonical.json
#                --authority '{"scope":..,"attempt":..,"network":..,"memory":..,"nano_cpus":..,"pids_limit":..}'
#                [--results RUN.jsonl]
# The authority values come from the harness's own admitted authority and settings,
# never from the product. `${...}` in `expect_upstream.body` are filled from them.
HEADERS_EXACT = ["Host", "Content-Type", "Content-Length"]  # the E3 allowlist output (plan)
WITNESS_BODY = '{ "Image" : "busybox" ,  "Cmd" : [ "true" ] }'
ENRICHED = {"Image": "busybox", "Cmd": ["true"],
            "Labels": {"dev.marsh.scope": "${scope}", "dev.marsh.attempt": "${attempt}"},
            "HostConfig": {"NetworkMode": "${network}", "Memory": "${memory}", "NanoCpus": "${nano_cpus}",
                           "PidsLimit": "${pids_limit}"}}
canon = [
    {"id": "K01-witness", "description": "the approved witness: an accepted versioned create with lowercase "
                                         "Content-Type, an off-allowlist header, Accept-Encoding and extra whitespace",
     "send": ["POST /v1.47/containers/create?name=nfxw HTTP/1.1\r\nHost: d\r\ncontent-type: application/json\r\n"
              "X-Nfx-Extra: 1\r\nAccept-Encoding: gzip\r\nContent-Length: %d\r\n\r\n%s" % (blen(WITNESS_BODY), WITNESS_BODY)],
     "responses": 1, "key": "name=nfxw", "refusal_allowed": False, "cleanup": rm("nfxw"),
     "expect_upstream": {"count": 1, "method": "POST", "target": "/v1.47/containers/create?name=nfxw",
                         "headers": {"Content-Type": "application/json", "Content-Length": "@body-length"},
                         "header_names_exact": HEADERS_EXACT, "body_compact": True, "body": ENRICHED,
                         "not_original_bytes": True}},
    {"id": "K02-control-ping", "description": "control: a canonical GET /_ping is forwarded unchanged",
     "send": ["GET /_ping HTTP/1.1\r\nHost: d\r\n\r\n"], "responses": 1, "key": "GET /_ping HTTP/1.1",
     "refusal_allowed": False, "cleanup": [],
     "expect_upstream": {"count": 1, "method": "GET", "target": "/_ping", "raw_equal": True}},
]


def extra(slug, description, send, body_expect):
    name = f"nfxcanon-{len(canon)}"
    canon.append({"id": f"K{len(canon) + 1:02d}-{slug}", "description": description, "send": send(name), "responses": 1,
                  "key": f"name={name}", "refusal_allowed": True, "cleanup": rm(name),
                  "expect_upstream": {"count": 1, "method": "POST", "target": f"/containers/create?name={name}",
                                      "header_names_exact": HEADERS_EXACT, "body_compact": True, "body": body_expect(name)}})


# Extra rows beyond the plan's witness: each either reaches the upstream in exactly the
# canonical form, or gets a verified local refusal (403 `marsh: ` / 400 + close in --results).
ENRICH_OF = lambda client: {**client, **{k: v for k, v in ENRICHED.items() if k in ("Labels", "HostConfig")}}
extra("duplicate-content-type", "two Content-Type headers",
      lambda n: [f"POST /containers/create?name={n} HTTP/1.1\r\n{HOST}Content-Type: application/json\r\n"
                 f"Content-Type: text/plain\r\nContent-Length: {blen('{\"Image\":\"busybox\"}')}\r\n\r\n" + '{"Image":"busybox"}'],
      lambda n: ENRICH_OF({"Image": "busybox"}))
extra("duplicate-user-agent", "an allowed-looking header sent twice",
      lambda n: [post(f"/containers/create?name={n}", '{"Image":"busybox"}', extra="User-Agent: nfx-a\r\nUser-Agent: nfx-b\r\n")],
      lambda n: ENRICH_OF({"Image": "busybox"}))
write("canonical.json", {
    "description": "SEC-23 (revision 3.1): the witness and control, plus two extra rows. The recording upstream "
                   "must see exactly the expected canonical form (exact header set and values, exact target, "
                   "compact body equal to the client fields plus the harness-computed enrichment). A row with "
                   "refusal_allowed passes without an upstream request only with a verified local refusal in --results.",
    "cases": canon})
print(f"{len(enc)} encodings, {len(fr)} framing, {len(canon)} canonical")
