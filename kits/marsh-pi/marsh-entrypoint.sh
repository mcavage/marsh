#!/bin/sh
set -eu
umask 077

selected=${MARSH_SELECTED_HOME:-${HOME:-/home/agent}}
case "$selected" in
  /*) ;;
  *) echo "pi kit: MARSH_SELECTED_HOME must be absolute" >&2; exit 125 ;;
esac
export HOME=$selected
mkdir -p "$HOME/.pi"

# Pi selects its Anthropic provider only when an API-key-shaped value is in the
# job environment. The host never passes the real key: this fixed sentinel is
# replaced by stock SBX's credential proxy on the allowed Anthropic endpoint.
case "${SBX_CRED_ANTHROPIC_MODE:-none}" in
  apikey|oauth) export ANTHROPIC_API_KEY=proxy-managed ;;
esac

# ACP mode: packaging/agents.json starts pi-session as `pi --acp`. The
# community pi-acp adapter speaks ACP v1 on stdio and starts Pi in RPC mode
# through this same entrypoint (without --acp), so both modes load the same
# trusted project resources and MCP Gateway extension. Pi has no OS-level
# command sandbox of its own; the job container is the sandbox.
if [ "${1:-}" = --acp ]; then
  shift
  printf 'marsh-auth-mode=%s\n' "${SBX_CRED_ANTHROPIC_MODE:-none}" >&2
  PI_ACP_PI_COMMAND=/usr/local/bin/marsh-pi
  export PI_ACP_PI_COMMAND
  exec node /opt/marsh/pi-gateway/node_modules/pi-acp/dist/index.js "$@"
fi

# The job container is the explicit project isolation boundary, so load the
# project's Pi resources without an additional interactive trust dialog.
# In an marsh job, /run/marsh/context.md (how to start child jobs) is appended
# to Pi's system prompt for this run (also RPC mode, which pi-acp starts
# through this entrypoint); nothing is written to ~/.pi or the project.
if [ -r /run/marsh/context.md ]; then
  set -- --append-system-prompt /run/marsh/context.md "$@"
fi
exec pi --approve -e /opt/marsh/pi-gateway/mcp-gateway.mjs "$@"
