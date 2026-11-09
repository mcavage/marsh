#!/usr/bin/env bash
# Pins and substituted sockets survive an engine restart (restored before dockerd).
source "$(dirname "$0")/lib.sh"
S=${1:-base}
P=$PROJ
dk $S docker rm -f rs-sock rs-pin >/dev/null 2>&1
dk $S docker pull -q docker:29-cli >/dev/null
dk $S docker run -d --name rs-sock --restart always -v /var/run/docker.sock:/var/run/docker.sock docker:29-cli sleep 3600 >/dev/null
dk $S docker run -d --name rs-pin --restart always -v $P/d:/h busybox sh -c 'while :; do sleep 1; done' >/dev/null
SOCK=$(prod inspect rs-sock --format '{{range .Mounts}}{{.Source}}{{end}}')
echo "   socket source before: $SOCK"
sbx exec $SB docker restart nfx-e3-engine >/dev/null
for i in $(seq 1 120); do prod info >/dev/null 2>&1 && break; sleep 0.5; done
if ! prod info >/dev/null 2>&1; then echo "engine did not come back; its state and logs:"; sbx exec $SB docker ps -a --format '{{.Names}} {{.Status}}'; sbx exec $SB docker cp nfx-e3-engine:/var/log/prod-dockerd.log - 2>&1 | tail -15; fi
sleep 4
vx grep -E "restored" /var/log/e3-agent.log | tail -2
vx test -S $SOCK && { echo "ok   substituted socket is served again after the restart"; PASS=$((PASS+1)); } || { echo "BAD  socket path missing after restart"; FAIL=$((FAIL+1)); }
out=$(prod exec rs-sock docker ps --format '{{.Names}}' 2>&1 | tr '\n' ' ')
echo "   inside rs-sock after restart: $out"
echo "$out" | grep -q "rs-sock" && { echo "ok   restart-policy container uses its socket after the restart"; PASS=$((PASS+1)); } || { echo "BAD  socket container cannot reach its endpoint: $out"; FAIL=$((FAIL+1)); }
out=$(prod exec rs-pin ls /h 2>&1 | tr '\n' ' ')
echo "$out" | grep -q ok.txt && { echo "ok   pinned bind restored: rs-pin sees $out"; PASS=$((PASS+1)); } || { echo "BAD  pin not restored: $out"; FAIL=$((FAIL+1)); }
dk $S docker rm -f rs-sock rs-pin >/dev/null 2>&1
summary
