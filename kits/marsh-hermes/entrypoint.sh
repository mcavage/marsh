#!/bin/sh
set -eu
# marsh names the home; Hermes keeps its state in ~/.hermes there.
export HOME="${MARSH_SELECTED_HOME:-$HOME}"
mkdir -p "$HOME/.hermes" "$HOME/.agents/skills"
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
# Per-run instructions: marsh's job context, and the shared-skills rule.
rule='Skills you create go to the shared folder that every agent in this marsh home reads. Create them flat, with no category.'
if [ -r /run/marsh/context.md ]; then
  HERMES_EPHEMERAL_SYSTEM_PROMPT="$(cat /run/marsh/context.md)

$rule"
else
  HERMES_EPHEMERAL_SYSTEM_PROMPT=$rule
fi
export HERMES_EPHEMERAL_SYSTEM_PROMPT
exec /opt/hermes/src/.venv/bin/hermes "$@"
