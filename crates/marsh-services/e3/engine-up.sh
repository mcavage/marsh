#!/usr/bin/env bash
# Mac side: (re)create the engine container in sandbox $SB and wait for it.
set -euo pipefail
SB=${SB:-nfx-e3-a}
PROJ=${PROJ:-/private/tmp/nfx-e3/proj}
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT:-/private/var/folders/zr/1nbpv_812d17xnm1kdwwvq1m0000gn/T/opencode/e3-out}
mkdir -p "$PROJ/.e3"
cp "$HERE/corpus/cli.jsonl" "$PROJ/.e3/corpus.jsonl"
cp "$OUT/e3-harness" "$HERE"/engine-init.sh "$HERE"/*.py "$PROJ/.e3/"
chmod +x "$PROJ/.e3/"*
sbx exec "$SB" sh -c "
  docker rm -f nfx-e3-engine >/dev/null 2>&1 || true
  sudo mkdir -p /var/lib/nfx-e3
  docker run -d --name nfx-e3-engine --privileged \
    -e E3_BREAKS='${E3_BREAKS:-}' -e E3_FRESH=1 \
    -v /var/lib/nfx-e3:/var/lib/nfx-e3 \
    -v $PROJ:$PROJ -v $PROJ/.e3:/marsh:ro \
    --entrypoint sh docker:29-dind /marsh/engine-init.sh >/dev/null
  for i in \$(seq 1 300); do docker exec nfx-e3-engine docker -H unix:///var/run/docker.sock info >/dev/null 2>&1 && break; sleep 0.3; done
  docker exec nfx-e3-engine docker -H unix:///var/run/docker.sock info --format 'engine up: {{.ServerVersion}} {{.Driver}} {{.DockerRootDir}}'
  docker exec nfx-e3-engine cat /var/log/e3-agent.log | tail -3
"
