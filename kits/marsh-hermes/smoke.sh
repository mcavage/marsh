#!/bin/sh
# Offline checks of a built marsh-hermes image, run by the Kit workflow before
# anything is pushed (and on pull requests). Usage: smoke.sh IMAGE VERSION
set -eu
image=$1
version=$2
# Hermes names its own release (the install stamp), not "unknown".
docker run --rm --entrypoint /opt/hermes/src/.venv/bin/hermes "$image" --version | tee /dev/stderr | grep -q "Hermes Agent v$version "
# marsh updates the Kit; `hermes update` is refused with exit 125.
rc=0; docker run --rm --entrypoint /usr/local/bin/hermes "$image" update || rc=$?
[ "$rc" = 125 ]
# Approval prompts are off unless the caller sets HERMES_YOLO_MODE.
probe='sed "s|^exec .*|echo YOLO=\$HERMES_YOLO_MODE|" /usr/local/bin/hermes > /tmp/probe && sh /tmp/probe'
[ "$(docker run --rm -e HOME=/tmp/h --entrypoint sh "$image" -c "$probe")" = YOLO=1 ]
[ "$(docker run --rm -e HOME=/tmp/h -e HERMES_YOLO_MODE=0 --entrypoint sh "$image" -c "$probe")" = YOLO=0 ]
docker run --rm --entrypoint /opt/hermes/src/.venv/bin/python "$image" -c 'import mcp, httpx2'
# The Kit's entrypoint seeds a working config from an SBX-managed key.
docker run --rm -e HOME=/tmp/h -e SBX_CRED_ANTHROPIC_MODE=apikey --entrypoint sh "$image" \
  -c '/usr/local/bin/hermes --version >/dev/null && grep -q "provider: anthropic" /tmp/h/.hermes/config.yaml && grep -q "create_dir: ~/.agents/skills" /tmp/h/.hermes/config.yaml'
# With SBX's MCP gateway present, the config points Hermes at it.
docker run --rm -e HOME=/tmp/h -e MCP_GATEWAY_URL=http://mcp-gateway.docker.internal/mcp \
  -e MCP_SENTINEL_TOKEN_NAME=ci-sentinel --entrypoint sh "$image" \
  -c '/usr/local/bin/hermes --version >/dev/null && grep -q "Authorization: Bearer ci-sentinel" /tmp/h/.hermes/config.yaml'
