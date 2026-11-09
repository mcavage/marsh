#!/usr/bin/env bash
# Testcontainers (Node) through the filter with Ryuk ON, via socket substitution.
source "$(dirname "$0")/lib.sh"
S=${1:-tc}
mkdir -p $PROJ/tc
cp ~/dev/narwhal-fork-spike/proto/tc/tc.test.js $PROJ/tc/tc.test.js
cat > $PROJ/tc/Dockerfile <<'DF'
FROM node:22-slim
WORKDIR /tc
RUN npm init -y >/dev/null && npm i testcontainers pg >/dev/null 2>&1
DF
sbx exec $SB sh -c "docker image inspect nfx-tc:local >/dev/null 2>&1 || docker build -q -t nfx-tc:local $PROJ/tc"
dk $S docker pull -q postgres:17 >/dev/null
dk $S docker pull -q testcontainers/ryuk:0.14.0 >/dev/null 2>&1 || true
run_tc() { # run_tc ENV...
  sbx exec $SB docker run --rm --network container:nfx-e3-engine -v /var/lib/nfx-e3/socks:/socks -v $PROJ/tc/tc.test.js:/tc/tc.test.js:ro \
    -e DOCKER_HOST=unix:///socks/s-$S.sock -e TESTCONTAINERS_HOST_OVERRIDE=localhost "$@" nfx-tc:local node /tc/tc.test.js
}
echo "== Ryuk disabled"
out=$(run_tc -e TESTCONTAINERS_RYUK_DISABLED=true 2>&1); echo "$out" | tail -3
echo "$out" | grep -q '"ok":true' && pass "Testcontainers passed with Ryuk disabled" || fail "Testcontainers with Ryuk disabled failed"
echo "== Ryuk ON, TESTCONTAINERS_DOCKER_SOCKET_OVERRIDE=/var/run/docker.sock"
out=$(run_tc -e TESTCONTAINERS_DOCKER_SOCKET_OVERRIDE=/var/run/docker.sock 2>&1); echo "$out" | tail -3
echo "$out" | grep -q '"ok":true' && { echo "ok   Testcontainers passed with Ryuk on (socket override)"; PASS=$((PASS+1)); } || { echo "BAD  Testcontainers with Ryuk on (override) failed"; FAIL=$((FAIL+1)); }
echo "== Ryuk ON, default env (the job's own socket path is an alias, Config.socket_aliases)"
out=$(run_tc 2>&1); echo "$out" | tail -3
echo "$out" | grep -q '"ok":true' && { echo "ok   Testcontainers passed with Ryuk on (default env)"; PASS=$((PASS+1)); } || { echo "BAD  Testcontainers with Ryuk on (default env) failed"; FAIL=$((FAIL+1)); }
echo "== Ryuk's socket mount while a test holds its container (production engine's view)"
sed 's/await c.stop();/await new Promise((r) => setTimeout(r, 15000)); await c.stop();/' $PROJ/tc/tc.test.js > $PROJ/tc/tc-hold.test.js
sbx exec $SB docker run -d --name tc-hold --network container:nfx-e3-engine -v /var/lib/nfx-e3/socks:/socks -v $PROJ/tc/tc-hold.test.js:/tc/tc.test.js:ro -e DOCKER_HOST=unix:///socks/s-$S.sock -e TESTCONTAINERS_HOST_OVERRIDE=localhost nfx-tc:local node /tc/tc.test.js >/dev/null

R=
for i in $(seq 1 30); do R=$(prod ps --format '{{.Names}}' | grep -i ryuk | head -1); [ -n "$R" ] && break; sleep 1; done
echo "   ryuk container: ${R:-none}"
if [ -z "$R" ]; then
  fail "no Ryuk container appeared while the test held its container"
else
  prod inspect "$R" --format '{{.Config.Image}} {{range .Mounts}}{{.Source}}->{{.Destination}} {{end}}'
  src=$(prod inspect "$R" --format '{{range .Mounts}}{{if eq .Destination "/var/run/docker.sock"}}{{.Source}}{{end}}{{end}}')
  case "$src" in /run/marsh/sockets/tc/*/docker.sock) pass "Ryuk's /var/run/docker.sock mount is the substituted socket ($src)";; *) fail "Ryuk mount source: $src";; esac
fi
out=$(sbx exec $SB docker wait tc-hold 2>&1 | tail -1)
want "the held test run exited 0" "$out" 0
sbx exec $SB docker logs tc-hold 2>&1 | tail -1 | grep -q '"ok":true' && pass "the held test run reported ok" || fail "the held test run did not report ok"
sbx exec $SB docker rm -f tc-hold >/dev/null 2>&1
echo "== nothing is left behind: workloads at once, Ryuk once its grace period has passed"
count() { grep -c . || true; }
work=-1; ryuk=-1
for i in $(seq 1 30); do
  work=$(prod ps -aq --filter label=org.testcontainers=true --filter label=org.testcontainers.ryuk!=true | count)
  [ "$work" = 0 ] && break; sleep 2
done
for i in $(seq 1 30); do
  ryuk=$(prod ps -aq --filter label=org.testcontainers.ryuk=true | count)
  [ "$ryuk" = 0 ] && break; sleep 2
done
echo "   leaked workload containers: $work   Ryuk containers still present: $ryuk"
prod ps -a --filter label=org.testcontainers=true --format '   left: {{.Names}} ({{.Image}}) {{.Status}}'
want "Ryuk reaped every test workload container" "$work" 0
want "Ryuk itself is gone after its grace period" "$ryuk" 0
want "no network left by Testcontainers" "$(prod network ls -q --filter label=org.testcontainers=true | count)" 0
want "no volume left by Testcontainers" "$(prod volume ls -q --filter label=org.testcontainers=true | count)" 0
echo "== refusals logged by the agent:"; vx grep refused /var/log/e3-agent.log | tail -5 | cut -c1-220
summary
