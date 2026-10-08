#!/bin/sh
# CLAUDE_CODE_EXECUTABLE for the ACP adapter: the Agent SDK's own native
# Claude Code build, with Claude Code's Bash sandbox disabled.
#
# The job container is the sandbox boundary. Bubblewrap cannot create
# namespaces in it, so a project or user `sandbox.enabled` written for the Mac
# would fail every first Bash call. Command-line settings outrank user and
# project settings and are not written anywhere. When the SDK passes its own
# --settings (a JSON object or a file path), merge `sandbox.enabled=false` into
# that value instead of replacing it.
set -eu

case "$(uname -m)" in
  aarch64|arm64) arch=arm64 ;;
  x86_64|amd64) arch=x64 ;;
  *) echo "claude kit: unsupported architecture $(uname -m)" >&2; exit 125 ;;
esac
# MARSH_* is reserved: a user shell can never forward this test override.
sdk=${MARSH_TEST_CLAUDE_SDK:-/opt/marsh/claude-acp/node_modules/@anthropic-ai}
cli=
for candidate in "$sdk/claude-agent-sdk-linux-$arch/claude" \
                 "$sdk/claude-agent-sdk-linux-$arch-musl/claude"; do
  if [ -x "$candidate" ]; then cli=$candidate; break; fi
done
if [ -z "$cli" ]; then
  echo "claude kit: the Agent SDK's Claude Code build is missing" >&2
  exit 125
fi

# Subcommands (e.g. the adapter's `auth status --json`) take no --settings;
# only the agent session, whose argv starts with options, is changed.
case "${1:-}" in
  ''|-*) ;;
  *) exec "$cli" "$@" ;;
esac

merge() {
  node -e '
const fs = require("fs");
const value = process.argv[1];
let settings;
try { settings = JSON.parse(value); } catch (_) {
  settings = JSON.parse(fs.readFileSync(value, "utf8"));
}
if (settings === null || Array.isArray(settings) || typeof settings !== "object") {
  throw new Error("claude kit: --settings must be a JSON object");
}
const sandbox = settings.sandbox !== null && typeof settings.sandbox === "object" &&
  !Array.isArray(settings.sandbox) ? settings.sandbox : {};
settings.sandbox = {...sandbox, enabled: false};
process.stdout.write(JSON.stringify(settings));
' "$1"
}

# In an marsh job, /run/marsh/context.md (how to start child jobs) is appended
# to the system prompt: added to the SDK's own --append-system-prompt value,
# or passed as one. Nothing is written anywhere.
context=
if [ -r /run/marsh/context.md ]; then
  context=$(cat /run/marsh/context.md)
fi

count=$#
found=
next=
appended=
for argument do
  if [ "$next" = settings ]; then
    argument=$(merge "$argument")
    next=
  elif [ "$next" = append ]; then
    if [ -n "$context" ]; then
      argument=$(printf '%s\n\n%s' "$argument" "$context")
    fi
    next=
  else
    case "$argument" in
      --settings) next=settings; found=1 ;;
      --settings=*) argument="--settings=$(merge "${argument#--settings=}")"; found=1 ;;
      --append-system-prompt) next=append; appended=1 ;;
      --append-system-prompt=*)
        if [ -n "$context" ]; then
          argument=$(printf '%s\n\n%s' "$argument" "$context")
        fi
        appended=1 ;;
    esac
  fi
  set -- "$@" "$argument"
done
shift "$count"
if [ -z "$found" ]; then
  set -- "$@" --settings '{"sandbox":{"enabled":false}}'
fi
if [ -z "$appended" ] && [ -n "$context" ]; then
  set -- "$@" --append-system-prompt "$context"
fi
exec "$cli" "$@"
