#!/usr/bin/env python3
"""Decode a record.txt (hex per direction per connection) into requests."""
import json, sys, collections

def parse_requests(buf):
    out = []
    i = 0
    while i < len(buf):
        j = buf.find(b"\r\n\r\n", i)
        if j < 0:
            break
        head = buf[i:j].decode("latin1")
        lines = head.split("\r\n")
        try:
            method, target, ver = lines[0].split(" ")
        except ValueError:
            break
        hdrs = []
        for l in lines[1:]:
            k, _, v = l.partition(":")
            hdrs.append((k.strip(), v.strip()))
        h = {k.lower(): v for k, v in hdrs}
        body = b""
        k = j + 4
        if h.get("transfer-encoding", "").lower() == "chunked":
            while True:
                e = buf.find(b"\r\n", k)
                if e < 0:
                    break
                n = int(buf[k:e].split(b";")[0], 16)
                k = e + 2
                if n == 0:
                    k += 2
                    break
                body += buf[k:k + n]
                k += n + 2
        else:
            n = int(h.get("content-length", "0"))
            body = buf[k:k + n]
            k += n
        out.append({"method": method, "target": target, "headers": hdrs, "body": body})
        i = k
        if h.get("upgrade"):
            break
    return out

def main(path):
    conns = collections.defaultdict(bytes)
    for line in open(path):
        cid, d, hexs = line.split()
        if d == ">":
            conns[int(cid)] += bytes.fromhex(hexs)
    reqs = []
    for cid in sorted(conns):
        for r in parse_requests(conns[cid]):
            r["conn"] = cid
            reqs.append(r)
    return reqs

if __name__ == "__main__":
    reqs = main(sys.argv[1])
    mode = sys.argv[2] if len(sys.argv) > 2 else "summary"
    for r in reqs:
        b = r["body"]
        if mode == "summary":
            print(r["conn"], r["method"], r["target"][:110], len(b))
        elif mode == "corpus":
            import base64
            print(json.dumps({"method": r["method"], "target": r["target"], "headers": r["headers"], "body_b64": base64.b64encode(b).decode()}))
        elif mode == "create" and "/containers/create" in r["target"]:
            print(r["target"]); print(json.dumps(json.loads(b), indent=1)[:6000]); print("----")
        elif mode == "json" and b[:1] == b"{":
            print(r["method"], r["target"][:80]); print(b.decode()[:1500]); print()
