#!/usr/bin/env bash
# CAP-11 c1: while a capture holds a scope frozen, state-changing requests wait or are refused.
source "$(dirname "$0")/lib.sh"
S=${1:-base}
dk $S docker rm -f frz >/dev/null 2>&1
dk $S docker run -d --name frz busybox sleep 300 >/dev/null
vx touch /pool/marsh/scopes/$S.freeze; sleep 1.5
expect refused "mutation during freeze: docker pause" -- dk $S docker pause frz
expect refused "mutation during freeze: docker run" -- dk $S docker run --rm --name evil-frz busybox true
expect allowed "reads during freeze: docker ps" -- dk $S docker ps
st=$(prod inspect frz --format '{{.State.Paused}}'); [ "$st" = false ] && { echo "ok   nothing was paused or changed during the freeze"; PASS=$((PASS+1)); } || { echo "BAD  state changed"; FAIL=$((FAIL+1)); }
vx rm /pool/marsh/scopes/$S.freeze; sleep 1.5
expect allowed "after the freeze: docker pause" -- dk $S docker pause frz
dk $S docker unpause frz >/dev/null 2>&1
dk $S docker rm -f frz >/dev/null 2>&1
echo "== acquisition waits for a mutation whose body is still arriving (independent observer: the engine itself)"
FZ=/pool/marsh/scopes
dk $S docker rm -f frz-slow >/dev/null 2>&1
vx rm -f $FZ/$S.freeze $FZ/$S.frozen
sbx exec $SB python3 - > $OUT/frz-slow.out 2>&1 <<PY &
import json, socket, time
body = json.dumps({"Image": "busybox", "Cmd": ["sleep", "300"]}).encode()
s = socket.socket(socket.AF_UNIX); s.settimeout(30); s.connect("$(sock $S)")
s.sendall(b"POST /v1.47/containers/create?name=frz-slow HTTP/1.1\r\nHost: d\r\nContent-Type: application/json\r\nContent-Length: %d\r\n\r\n" % len(body))
time.sleep(6)          # the head has been admitted; the body is late
s.sendall(body)
print(s.recv(4096).split(b"\r\n", 1)[0].decode())
PY
SLOW=$!
sleep 2
T0=$(date +%s)
vx touch $FZ/$S.freeze
for i in $(seq 1 60); do vx test -e $FZ/$S.frozen && break; sleep 0.5; done
T1=$(date +%s)
have=$(prod ps -aq --filter name=^frz-slow$ | grep -c . || true)
vx test -e $FZ/$S.frozen && held=yes || held=no
echo "   freeze held=$held after $((T1-T0))s; frz-slow exists in the engine at that moment: $have"
want "the freeze was granted" "$held" yes
want "the engine already had the late create when the freeze was granted" "$have" 1
[ $((T1-T0)) -ge 3 ] && pass "the freeze waited for the in-flight create ($((T1-T0))s)" || fail "the freeze was granted after $((T1-T0))s, while the create's body had not arrived"
wait $SLOW
want "the late create was answered 201" "$(sed -n 's/^HTTP\/1.1 \([0-9]*\).*/\1/p' $OUT/frz-slow.out | head -1)" 201
expect refused "while frozen: docker run" -- dk $S docker run --rm --name evil-frz2 busybox true
vx rm -f $FZ/$S.freeze
for i in $(seq 1 40); do vx test -e $FZ/$S.frozen || break; sleep 0.5; done
expect allowed "after release: docker run" -- dk $S docker run --rm --name frz-after busybox true
dk $S docker rm -f frz-slow >/dev/null 2>&1
summary
