#!/usr/bin/env python3
"""Differential for recorded container-create bodies (the claim is narrow: create
bodies of real clients, not every route; exact wire images of other requests are
asserted by canon.sh and the unit corpus).

Each recorded create is sent to the engine directly and through the filter. Both
must SUCCEED (a recorded allowed operation that the engine itself refuses means
the environment is wrong, and a refusal by the filter is a bug). The two engine
objects are then compared field by field:

  - everything the policy does not rewrite is identical (ordinary values retained);
  - each documented rewrite is asserted on its own, against the request body:
      labels      = the client's + the scope's two reserved labels
      limits      = the client's value, or the default when it asked for none
      networks    = the scope network for the default, the client's otherwise
      ports       = same host ports, every host IP forced to 127.0.0.1
      mounts      = same destinations/types/RW; bind sources are pins under
                    /var/lib/marsh/binds/<scope>/ (read-only may only be added)

  differential.py CORPUS.jsonl PROD_SOCK FILTER_SOCK PROJECT
"""
import base64, http.client, json, os, socket, sys

corpus, prod_sock, filt_sock, proj = sys.argv[1:5]
OLD = "/private/tmp/nfx-e3/proj"
DEF = {"Memory": 4 << 30, "NanoCpus": 4_000_000_000, "PidsLimit": 4096}


class UC(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost"); self.path = path
    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX); self.sock.settimeout(30); self.sock.connect(self.path)


def call(sock, method, target, body=None):
    c = UC(sock)
    c.request(method, target, body=body, headers={"Content-Type": "application/json"} if body else {})
    r = c.getresponse(); b = r.read(); c.close()
    return r.status, b


def inspect(cid):
    st, b = call(prod_sock, "GET", f"/v1.56/containers/{cid}/json")
    return json.loads(b)


def anon(n):
    return "<anonymous>" if len(n) == 64 and all(c in "0123456789abcdef" for c in n) else n


def rest(j):
    cfg = dict(j["Config"]); hc = dict(j["HostConfig"])
    cfg["Labels"] = {k: v for k, v in (cfg.get("Labels") or {}).items() if not k.startswith("dev.marsh.")}
    cfg.pop("Hostname", None)
    for k in ("Binds", "NetworkMode", "Memory", "MemorySwap", "NanoCpus", "PidsLimit", "PortBindings", "Mounts"):
        hc.pop(k, None)
    return {"Config": cfg, "HostConfig": hc}


def binds(j):
    return sorted((b.split(":")[1], ":".join(b.split(":")[2:])) for b in (j["HostConfig"].get("Binds") or []))


def mounts(j):
    return sorted((m["Destination"], m["Type"], m["RW"], anon(m.get("Name", "")) if m["Type"] == "volume" else "") for m in j["Mounts"])


def ports(j):
    pb = j["HostConfig"].get("PortBindings") or {}
    return {k: sorted(b["HostPort"] for b in v) for k, v in pb.items()}


def hostips(j):
    return sorted({b["HostIp"] for v in (j["HostConfig"].get("PortBindings") or {}).values() for b in v})


counts = {k: 0 for k in ("creates", "rest", "labels", "limits", "networks", "ports", "mounts", "pins")}
bad = []


def need(ok, i, what, detail=""):
    if not ok:
        bad.append((i, what, str(detail)[:900]))


for p in ("d", "stack-a/app"):
    os.makedirs(f"{proj}/{p}", exist_ok=True)

for i, line in enumerate(open(corpus)):
    r = json.loads(line)
    if "/containers/create" not in r["target"]:
        continue
    body = base64.b64decode(r["body_b64"]).replace(OLD.encode(), proj.encode())
    req = json.loads(body)
    reqhc = req.get("HostConfig", {})
    base = r["target"].split("?")[0]
    names = (f"dd{i}", f"df{i}")
    for nm in names:
        call(prod_sock, "DELETE", f"/v1.56/containers/{nm}?force=1")
    nets = set((req.get("NetworkingConfig", {}) or {}).get("EndpointsConfig", {}) or {})
    nm_ = reqhc.get("NetworkMode", "")
    if nm_ and not nm_.startswith("container:"):
        nets.add(nm_)
    for net in nets - {"default", "bridge", "host", "none", ""}:
        call(filt_sock, "POST", "/v1.56/networks/create", json.dumps({"Name": net}).encode())
    sf, bf = call(filt_sock, "POST", f"{base}?name={names[1]}", body)
    sd, bd = call(prod_sock, "POST", f"{base}?name={names[0]}", body)
    counts["creates"] += 1
    need(sd == 201, i, "the engine itself refused a recorded create (environment, not policy)", (sd, bd[:120]))
    need(sf == 201, i, "the filter refused a recorded create the engine accepts", (sf, bf[:120]))
    if sd != 201 or sf != 201:
        continue
    d, f = inspect(names[0]), inspect(names[1])
    ws = (f["Config"].get("Labels") or {}).get("dev.marsh.scope", "")

    counts["rest"] += 1
    rd, rf = rest(d), rest(f)
    need(rd == rf, i, "an ordinary value changed", {sec: {k: (rd[sec].get(k), rf[sec].get(k)) for k in set(rd[sec]) | set(rf[sec]) if rd[sec].get(k) != rf[sec].get(k)} for sec in rd if rd[sec] != rf[sec]})

    counts["labels"] += 1
    want_labels = dict(d["Config"].get("Labels") or {})
    want_labels["dev.marsh.scope"] = ws
    fl = f["Config"].get("Labels") or {}
    need(ws != "" and fl.get("dev.marsh.attempt") not in (None, "")
         and {k: v for k, v in fl.items() if k != "dev.marsh.attempt"} == want_labels, i, "labels", (fl, want_labels))

    counts["limits"] += 1
    for k in DEF:
        need(f["HostConfig"][k] == (reqhc.get(k) or DEF[k]), i, f"limit {k}: the client's value, or the default", (f["HostConfig"][k], reqhc.get(k)))
        need((d["HostConfig"][k] or 0) == (reqhc.get(k) or 0), i, f"the direct container's {k} is the client's", d["HostConfig"][k])
    # Swap: Docker derives it from the memory limit when the client gave none.
    need(f["HostConfig"]["MemorySwap"] == (reqhc.get("MemorySwap") or 2 * f["HostConfig"]["Memory"]), i, "MemorySwap is the client's or Docker's derived default", f["HostConfig"]["MemorySwap"])

    counts["networks"] += 1
    fnets, dnets = sorted(f["NetworkSettings"]["Networks"]), sorted(d["NetworkSettings"]["Networks"])
    if nm_ in ("default", ""):
        need(fnets == [f"marsh-scope-{ws}"], i, "the default network becomes the scope network", fnets)
    else:
        need(fnets == dnets, i, "a named network, or none, is kept", (fnets, dnets))

    counts["ports"] += 1
    need(ports(d) == ports(f), i, "published host ports kept", (ports(d), ports(f)))
    need(hostips(f) in ([], ["127.0.0.1"]), i, "every published host IP is forced to loopback", hostips(f))

    counts["mounts"] += 1
    dm, fm = mounts(d), mounts(f)
    need([(a, b) for a, b, _, _ in dm] == [(a, b) for a, b, _, _ in fm], i, "same mount destinations and types", (dm, fm))
    for (a1, b1, rw1, v1), (a2, b2, rw2, v2) in zip(dm, fm):
        need(v1 == v2 and (rw1 == rw2 or (rw1 and not rw2)), i, f"mount {a1}: volume name and RW kept (read-only may only be added)", ((a1, b1, rw1, v1), (a2, b2, rw2, v2)))
    need(binds(d) == binds(f), i, "legacy bind destinations and options kept", (binds(d), binds(f)))

    counts["pins"] += 1
    for m in f["Mounts"]:
        if m["Type"] == "bind" and m["Destination"] != "/var/run/docker.sock":
            need(m["Source"].startswith(f"/var/lib/marsh/binds/{ws}/"), i, "a bind source is a pin, not the client's path", m["Source"])
    for nm in names:
        call(prod_sock, "DELETE", f"/v1.56/containers/{nm}?force=1&v=1")

print(json.dumps(counts))
for b in bad:
    print("DIFF", b)
need(counts["creates"] >= 10, -1, "the corpus has fewer recorded creates than expected", counts["creates"])
ok = not bad
print(("ok   " if ok else "BAD  ") + f"{counts['creates']} recorded creates succeeded both ways; ordinary values identical; every rewrite asserted")
sys.exit(0 if ok else 1)
