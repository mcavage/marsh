#!/usr/bin/env bash
# SEC-23 c1: the engine receives a canonical request only. A recording proxy sits
# between the agent and the engine; the exact bytes are asserted (canon_check.py)
# for a ping control and for a create sent fixed-length and chunked, each with
# hostile client headers and a messy body.
source "$(dirname "$0")/lib.sh"
H=$(cd "$(dirname "$0")" && pwd)
REC=$OUT/canon.rec
# Helpers are started with their pid recorded and stopped by that pid, never by a pattern.
stop_helpers() { vx sh -c 'for f in /tmp/canon-rec.pid /tmp/canon-agent.pid; do [ -f $f ] && kill $(cat $f) 2>/dev/null; rm -f $f; done; rm -f /var/lib/nfx-e3/socks/rec2.sock /var/lib/nfx-e3/socks/s-canon.sock'; }
stop_helpers
vx rm -f /tmp/canon.rec
sbx exec $SB docker exec -d nfx-e3-engine sh -c "echo \$\$ > /tmp/canon-rec.pid; exec /marsh/e3-harness record --listen /var/lib/nfx-e3/socks/rec2.sock --upstream /var/run/docker.sock --out /tmp/canon.rec"
sbx exec $SB docker exec -d nfx-e3-engine sh -c "echo \$\$ > /tmp/canon-agent.pid; exec /marsh/e3-harness serve --state /tmp/canon-state --upstream /var/lib/nfx-e3/socks/rec2.sock --listen /var/lib/nfx-e3/socks/s-canon.sock --root $PROJ --workspace canon --attempt a1 ${E3_BREAKS:+--break $E3_BREAKS}"
for i in $(seq 1 40); do vx test -S /var/lib/nfx-e3/socks/s-canon.sock && vx test -S /var/lib/nfx-e3/socks/rec2.sock && break; sleep 0.25; done
prod rm -f canon-fixed canon-chunked >/dev/null 2>&1
sbx exec $SB python3 - <<PY
import socket
def talk(raw_chunks, delay=0.0):
    import time
    s = socket.socket(socket.AF_UNIX); s.settimeout(8); s.connect("$(sock canon)")
    for c in raw_chunks:
        s.sendall(c)
        if delay: time.sleep(delay)
    buf = b""
    try:
        while b"\r\n\r\n" not in buf:
            d = s.recv(65536)
            if not d: break
            buf += d
    except socket.timeout:
        pass
    s.close()
    return buf.split(b"\r\n", 1)[0]
HOSTILE = b"Host: evil\r\nX-Evil: smuggled\r\nAccept-Encoding: gzip\r\nUser-Agent: Docker-Client/29\r\nX-Forwarded-For: 6.6.6.6\r\n"
print("ping:", talk([b"GET /_ping HTTP/1.1\r\n" + HOSTILE + b"\r\n"]))
body = b'{ "Image" : "busybox",\n  "Cmd":["true"], "Env":["CANON_MARK=1"],\t"Tty" : false }'
print("fixed:", talk([b"POST /v1.47/containers/create?name=canon-fixed&Image=evil-from-query HTTP/1.1\r\n" + HOSTILE +
      b"Content-Type: application/json; charset=utf-8\r\nContent-Length: %d\r\n\r\n" % len(body) + body]))
parts = [body[:20], body[20:41], body[41:]]
chunked = b"".join(b"%x\r\n" % len(p) + p + b"\r\n" for p in parts) + b"0\r\n\r\n"
print("chunked:", talk([b"POST /v1.47/containers/create?name=canon-chunked&Image=evil-from-query HTTP/1.1\r\n" + HOSTILE +
      b"Content-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n", chunked[:30], chunked[30:]], delay=0.05))
PY
sleep 1
vx cat /tmp/canon.rec > $REC
echo "   recording: $(wc -l < $REC) lines"
python3 $H/canon_check.py $REC
rc=$?
[ $rc -eq 0 ] && PASS=$((PASS+1)) || FAIL=$((FAIL+1))
prod rm -f canon-fixed canon-chunked >/dev/null 2>&1
stop_helpers
summary
