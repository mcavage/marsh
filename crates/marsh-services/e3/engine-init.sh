#!/bin/sh
# Inside the engine container (docker:29-dind). Mirrors proto/a1-probe.sh: XFS
# pool, production dockerd on the pool, then the E3 agent in front of it.
set -e
echo "== engine-init $(date)" | tee -a /var/log/e3-agent.log /var/log/prod-dockerd.log >/dev/null
trap 'rc=$?; [ $rc -ne 0 ] && echo "engine-init exited $rc at line $LINENO" >> /var/log/prod-dockerd.log' EXIT
apk add -q --no-cache xfsprogs util-linux >/dev/null 2>&1 || true
[ -e /dev/loop-control ] || mknod /dev/loop-control c 10 237
for i in 0 1 2 3 4 5 6 7; do [ -e /dev/loop$i ] || mknod /dev/loop$i b 7 $i; done
[ -f /var/lib/nfx-e3/pool.img ] || { fallocate -l 6G /var/lib/nfx-e3/pool.img; mkfs.xfs -q -m reflink=1 /var/lib/nfx-e3/pool.img; }
# cgroup v2 delegation, as docker:dind's own entrypoint does (S2 needs this too:
# without it every container with a memory/cpu/pids limit fails to start).
if [ -f /sys/fs/cgroup/cgroup.controllers ]; then
  mkdir -p /sys/fs/cgroup/init
  xargs -rn1 < /sys/fs/cgroup/cgroup.procs > /sys/fs/cgroup/init/cgroup.procs 2>/dev/null || :
  for try in 1 2 3 4 5 6; do
    xargs -rn1 < /sys/fs/cgroup/cgroup.procs > /sys/fs/cgroup/init/cgroup.procs 2>/dev/null || :
    sed -e 's/ / +/g' -e 's/^/+/' < /sys/fs/cgroup/cgroup.controllers > /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null && break
    sleep 0.3
  done
  grep -q memory /sys/fs/cgroup/cgroup.subtree_control || echo 'engine-init: memory controller not delegated' >&2
fi
mkdir -p /pool && mount -o loop /var/lib/nfx-e3/pool.img /pool
mkdir -p /var/lib/nfx-e3/socks; chmod 777 /var/lib/nfx-e3/socks
mkdir -p /etc/docker /pool/docker /pool/marsh/scopes /var/lib/marsh/binds /run/marsh
chmod 700 /var/lib/marsh /var/lib/marsh/binds /run/marsh
# A new engine container starts with an empty ledger; a restart keeps it.
if [ -n "$E3_FRESH" ] && [ ! -e /etc/e3-fresh-done ]; then rm -f /pool/marsh/ledger.json; touch /etc/e3-fresh-done; fi
echo '{"builder":{"entitlements":{"network-host":false,"security-insecure":false}},"ip":"127.0.0.1"}' > /etc/docker/daemon.json
export MARSH_E3_LOG=1
# Agent first: it restores pins and substituted sockets before any container can start.
/marsh/e3-harness serve --state /pool/marsh --upstream /var/run/docker.sock --scope-dir /pool/marsh/scopes --verbose --socket-alias /socks/s-tc.sock,/run/marsh/docker.sock ${E3_BREAKS:+--break "$E3_BREAKS"} >>/var/log/e3-agent.log 2>&1 &
AGENT=$!
for i in $(seq 1 50); do tail -3 /var/log/e3-agent.log | grep -q "restored .* sockets" 2>/dev/null && break; sleep 0.1; done
# Production order (A3): readiness only after restoration. An agent that could not
# restore exits; the engine is then not started.
kill -0 $AGENT 2>/dev/null || { echo "engine-init: the agent did not come up (restore failed): not starting dockerd" | tee -a /var/log/prod-dockerd.log; exit 1; }
# Ground truth for the harness: the engine's own socket, outside the filter.
dockerd --data-root /pool/docker -H unix:///var/run/docker.sock -H unix:///var/lib/nfx-e3/socks/prod.sock >>/var/log/prod-dockerd.log 2>&1 &
DP=$!
(while :; do chmod 666 /var/lib/nfx-e3/socks/prod.sock 2>/dev/null; sleep 1; done) &
wait $DP
