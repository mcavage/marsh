#!/bin/sh
set -eu
# marsh names the home; Hermes keeps its state in ~/.hermes there.
export HOME="${MARSH_SELECTED_HOME:-$HOME}"
mkdir -p "$HOME/.hermes" "$HOME/.agents/skills"
# Each marsh job is a fresh container: a gateway socket left in the shared home
# belongs to an earlier, finished job, and Hermes would refuse it as unsafe.
rm -f "$HOME/.hermes/gateway.sock" "$HOME/.hermes/gateway.sock.path" "$HOME"/.hermes/state/*.sock
# SBX replaces these placeholders with your real key at the provider's API.
provider=
if [ "${SBX_CRED_GOOGLE_MODE:-none}" != none ]; then
  export GEMINI_API_KEY=proxy-managed; provider=gemini
fi
if [ "${SBX_CRED_ANTHROPIC_MODE:-none}" != none ]; then
  export ANTHROPIC_API_KEY=proxy-managed; provider=anthropic
fi
# First run: start from the defaults, using the provider of a key SBX manages.
# Afterwards the config is yours.
config="$HOME/.hermes/config.yaml"
if [ ! -e "$config" ]; then
  if [ -n "$provider" ]; then
    printf 'model:\n  provider: %s\n\n' "$provider" > "$config"
  fi
  cat /opt/hermes/defaults/config.yaml >> "$config"
fi
# The sandbox's MCP gateway (your work tools): keep an `mcp-gateway` entry
# pointing at it. The bearer is SBX's sentinel name; SBX swaps in the real
# credential. A customized entry of that name is left alone.
if [ "${MCP_GATEWAY_URL:-}" = http://mcp-gateway.docker.internal/mcp ] &&
   printf '%s' "${MCP_SENTINEL_TOKEN_NAME:-}" | grep -Eq '^[A-Za-z0-9._-]{1,128}$'; then
  /opt/hermes/src/.venv/bin/python /opt/hermes/defaults/gateway.py "$config" ||
    echo 'hermes kit: could not add the MCP gateway to config.yaml' >&2
fi
# Per-run instructions: marsh's job context, and the shared-skills rule.
rule='Skills you create go to the shared folder that every agent in this marsh home reads. Create them flat, with no category.
To use other agents or models (a second opinion, parallel work, a different vendor), start them as marsh child jobs as described above, e.g. codex exec, claude -p, pix -p, with fanout or split; do not simulate them yourself.'
if [ -r /run/marsh/context.md ]; then
  HERMES_EPHEMERAL_SYSTEM_PROMPT="$(cat /run/marsh/context.md)

$rule"
else
  HERMES_EPHEMERAL_SYSTEM_PROMPT=$rule
fi
export HERMES_EPHEMERAL_SYSTEM_PROMPT
exec /opt/hermes/src/.venv/bin/hermes "$@"
