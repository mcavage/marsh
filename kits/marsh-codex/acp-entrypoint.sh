#!/bin/sh
# ACP mode of the codex Kit: `codex --acp` (packaging/agents.json
# codex-session) runs the official codex-acp adapter on stdio.
set -eu
umask 077
temporary=
trap 'test -z "$temporary" || rm -f "$temporary"' EXIT

selected=${MARSH_SELECTED_HOME:-${HOME:-/home/agent}}
case "$selected" in
  /*) HOME=$selected; export HOME ;;
  *) echo "codex kit (ACP): MARSH_SELECTED_HOME must be absolute" >&2; exit 125 ;;
esac
# ACP sessions keep their own Codex home, separate from the CLI's .codex.
CODEX_HOME="$HOME/.codex-acp"
export CODEX_HOME
# The adapter must never try to open a login browser.
NO_BROWSER=1
export NO_BROWSER
mkdir -p "$CODEX_HOME"
# The selected home is a host mount; Codex app-server's SQLite runtime needs
# native Linux filesystem semantics. Its session files still use CODEX_HOME.
CODEX_SQLITE_HOME=$(mktemp -d /tmp/marsh-codex-sqlite.XXXXXX)
export CODEX_SQLITE_HOME

mode=${SBX_CRED_OPENAI_MODE:-none}
printf 'marsh-auth-mode=%s\n' "$mode" >&2
config="$CODEX_HOME/config.toml"
if [ ! -e "$config" ] && [ ! -L "$config" ]; then
  project=$(node -p 'JSON.stringify(process.cwd())')
  temporary=$(mktemp "$CODEX_HOME/.config.XXXXXX")
  {
    printf 'approval_policy = "never"\nsandbox_mode = "danger-full-access"\n'
    case "$mode" in
      oauth)
        printf 'model_provider = "sandboxd"\n\n[model_providers.sandboxd]\n'
        printf 'name = "Sandbox Proxy"\nbase_url = "https://chatgpt.com/backend-api/codex"\n'
        printf 'experimental_bearer_token = "oai-oat01-proxy-managed"\nrequires_openai_auth = false\n'
        ;;
    esac
    printf '\n[projects.%s]\ntrust_level = "trusted"\n' "$project"
  } > "$temporary"
  if ! ln "$temporary" "$config" 2>/dev/null && [ ! -f "$config" ]; then
    echo "codex kit (ACP): atomic config creation failed" >&2
    exit 125
  fi
  rm -f "$temporary"
fi
if [ -L "$config" ] || [ ! -f "$config" ]; then
  echo "codex kit (ACP): config.toml must be a regular file" >&2
  exit 125
fi
if grep -Eq '^[[:space:]]*sqlite_home[[:space:]]*=' "$config"; then
  echo "codex kit (ACP): sqlite_home override can place SQLite on the host mount" >&2
  exit 125
fi
case "$mode" in
  apikey) export OPENAI_API_KEY=proxy-managed ;;
esac
case "$mode" in
  apikey|oauth)
    if [ ! -e "$CODEX_HOME/auth.json" ] && [ ! -L "$CODEX_HOME/auth.json" ]; then
      temporary=$(mktemp "$CODEX_HOME/.auth.XXXXXX")
      printf '%s\n' '{"OPENAI_API_KEY":"proxy-managed"}' > "$temporary"
      if ! ln "$temporary" "$CODEX_HOME/auth.json" 2>/dev/null &&
         [ ! -f "$CODEX_HOME/auth.json" ]; then
        echo "codex kit (ACP): atomic auth creation failed" >&2
        exit 125
      fi
      rm -f "$temporary"
    fi
    ;;
esac
if [ -L "$CODEX_HOME/auth.json" ] ||
   { [ -e "$CODEX_HOME/auth.json" ] && [ ! -f "$CODEX_HOME/auth.json" ]; }; then
  echo "codex kit (ACP): auth.json must be a regular file" >&2
  exit 125
fi

# The job container is the sandbox boundary, and Codex's bubblewrap sandbox
# cannot create namespaces in it. codex-acp ignores config.toml's
# sandbox_mode/approval_policy and starts every session in its "agent" mode
# (workspace-write, on-request), so start in its full-access mode (never,
# danger-full-access) unless the caller chose an initial mode. The ACP client
# can still switch modes per session.
INITIAL_AGENT_MODE=${INITIAL_AGENT_MODE:-agent-full-access}
export INITIAL_AGENT_MODE

# In an marsh job, /run/marsh/context.md (how to start child jobs) becomes the
# sessions' developer instructions through codex-acp's CODEX_CONFIG (merged
# into a caller's CODEX_CONFIG object); nothing is written to CODEX_HOME.
if [ -r /run/marsh/context.md ]; then
  CODEX_CONFIG=$(node -e '
const fs = require("fs");
let config = {};
if (process.env.CODEX_CONFIG) {
  config = JSON.parse(process.env.CODEX_CONFIG);
  if (config === null || Array.isArray(config) || typeof config !== "object") {
    throw new Error("codex kit (ACP): CODEX_CONFIG must be a JSON object");
  }
}
if (config.developer_instructions === undefined) {
  config.developer_instructions = fs.readFileSync(process.argv[1], "utf8");
}
process.stdout.write(JSON.stringify(config));
' /run/marsh/context.md)
  export CODEX_CONFIG
fi

exec node /opt/marsh/codex-acp/mcp-gateway-proxy.mjs /opt/marsh/codex-acp/node_modules/@agentclientprotocol/codex-acp/dist/index.js "$@"
