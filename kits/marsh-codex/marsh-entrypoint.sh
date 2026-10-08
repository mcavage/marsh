#!/bin/sh
set -eu
umask 077

selected=${MARSH_SELECTED_HOME:-${HOME:-/home/agent}}
case "$selected" in
  /*) ;;
  *) echo "codex kit: MARSH_SELECTED_HOME must be absolute" >&2; exit 125 ;;
esac
export HOME=$selected
# ACP mode: packaging/agents.json starts codex-session as `codex --acp`.
if [ "${1:-}" = --acp ]; then
  shift
  exec /opt/marsh/codex-acp/marsh-acp.sh "$@"
fi
export CODEX_HOME="$HOME/.codex"
mkdir -p "$CODEX_HOME" "$HOME/.agents"
# Codex's SQLite runtime needs native Linux filesystem semantics. Keep its
# configuration and sessions in the selected home, but place the database in
# this job container's private filesystem.
CODEX_SQLITE_HOME=$(mktemp -d /tmp/marsh-codex-sqlite.XXXXXX)
export CODEX_SQLITE_HOME

mode=${SBX_CRED_OPENAI_MODE:-none}
codex_config="$CODEX_HOME/config.toml"
codex_lock="$codex_config.marsh-lock"
codex_lock_acquired=
cleanup_codex_lock() {
  if [ "${codex_lock_acquired:-}" = 1 ]; then
    rm -f "$codex_lock/created" "$codex_lock/config" 2>/dev/null || :
    rmdir "$codex_lock" 2>/dev/null || :
    codex_lock_acquired=
  fi
}
trap 'cleanup_codex_lock' EXIT
trap 'cleanup_codex_lock; exit 129' HUP
trap 'cleanup_codex_lock; exit 130' INT
trap 'cleanup_codex_lock; exit 143' TERM

# Fresh job containers share the selected home. Serialize every config.toml
# mutation, bound healthy contention to ten seconds, and reclaim only the
# protocol-owned files from a writer that died at least thirty seconds ago.
codex_lock_attempt=0
while ! mkdir "$codex_lock" 2>/dev/null; do
  codex_now=$(date +%s)
  codex_created=$(cat "$codex_lock/created" 2>/dev/null || printf '0')
  case "$codex_created" in
    ''|*[!0-9]*) codex_created=0 ;;
  esac
  if [ "$codex_created" -gt 0 ] &&
     [ $((codex_now - codex_created)) -ge 30 ]; then
    rm -f "$codex_lock/created" "$codex_lock/config" 2>/dev/null || :
    rmdir "$codex_lock" 2>/dev/null || :
  fi
  codex_lock_attempt=$((codex_lock_attempt + 1))
  if [ "$codex_lock_attempt" -ge 200 ]; then
    echo "codex kit: timed out waiting for selected-home config lock" >&2
    exit 125
  fi
  sleep 0.05
done
codex_lock_acquired=1
date +%s >"$codex_lock/created"

# Codex loads project-local configuration only after the project is trusted.
# Merge that exact project and the optional MCP stanza without replacing any
# other user setting. The temporary file lives beside the destination, making
# the final rename atomic on the selected-home filesystem.
node - "$codex_config" "$codex_lock" "$PWD" "$mode" \
  "${MCP_GATEWAY_URL:-}" "${MCP_SENTINEL_TOKEN_NAME:-}" <<'NODE'
const fs = require("fs");
const path = require("path");
const [configPath, lock, project, mode, mcpUrl, mcpToken] = process.argv.slice(2);

let config;
let fileMode = 0o600;
if (fs.existsSync(configPath)) {
  const stat = fs.lstatSync(configPath);
  if (stat.isSymbolicLink() || !stat.isFile()) {
    console.error(`codex kit: ${configPath} must be a regular file; preserved unchanged`);
    process.exit(125);
  }
  config = fs.readFileSync(configPath, "utf8");
  if (config.includes("\0")) {
    console.error(`codex kit: ${configPath} is malformed TOML; preserved unchanged`);
    process.exit(125);
  }
  fileMode = stat.mode & 0o777;
} else {
  config = [
    'approval_policy = "never"',
    'sandbox_mode = "danger-full-access"',
    'mcp_oauth_credentials_store = "file"',
  ].join("\n") + "\n";
  if (mode === "oauth" || mode === "apikey") {
    config += 'forced_login_method = "api"\n';
  }
  if (mode === "oauth") {
    config += [
      "",
      'model_provider = "sandboxd"',
      "",
      "[model_providers.sandboxd]",
      'name = "Sandbox Proxy"',
      'base_url = "https://chatgpt.com/backend-api/codex"',
      'experimental_bearer_token = "oai-oat01-proxy-managed"',
      "requires_openai_auth = false",
      "",
    ].join("\n");
  }
}

let changed = !fs.existsSync(configPath);
const projectTable = `[projects.${JSON.stringify(project)}]`;
if (!config.split(/\r?\n/).includes(projectTable)) {
  config += `${config.endsWith("\n") ? "" : "\n"}\n${projectTable}\ntrust_level = "trusted"\n`;
  changed = true;
}
if (mcpUrl && mcpToken) {
  const gatewayTable = "[mcp_servers.mcp-gateway]";
  const gatewayBlock = [
    gatewayTable,
    'type = "http"',
    `url = ${JSON.stringify(mcpUrl)}`,
    "[mcp_servers.mcp-gateway.http_headers]",
    `Authorization = ${JSON.stringify(`Bearer ${mcpToken}`)}`,
    "",
  ].join("\n");
  const gatewayHeaders = [...config.matchAll(/^\[mcp_servers\.mcp-gateway\]\r?$/gm)];
  if (gatewayHeaders.length === 0) {
    config += `${config.endsWith("\n") ? "" : "\n"}\n${gatewayBlock}`;
    changed = true;
  } else {
    if (gatewayHeaders.length !== 1) {
      console.error(`codex kit: ${configPath} has duplicate mcp-gateway stanzas; preserved unchanged`);
      process.exit(125);
    }
    // Refresh only a stanza with the exact shape previously written by this
    // adapter. A different, user-owned MCP registration remains untouched.
    const generated = /^\[mcp_servers\.mcp-gateway\]\r?\ntype = "http"\r?\nurl = ("(?:[^"\\]|\\.)*")\r?\n\[mcp_servers\.mcp-gateway\.http_headers\]\r?\nAuthorization = ("(?:[^"\\]|\\.)*")\r?\n(?=\[|\r?\n|$)/m;
    const previous = config.match(generated);
    if (previous) {
      const oldUrl = JSON.parse(previous[1]);
      const oldAuth = JSON.parse(previous[2]);
      if (oldUrl === "http://mcp-gateway.docker.internal/mcp") {
        if (!/^Bearer [A-Za-z0-9._-]{1,128}$/.test(oldAuth)) {
          console.error(`codex kit: ${configPath} has an unsafe stock mcp-gateway authorization shape; preserved unchanged`);
          process.exit(125);
        }
        if (previous[0] !== gatewayBlock) {
          config = config.replace(previous[0], () => gatewayBlock);
          changed = true;
        }
      }
    } else {
      const start = gatewayHeaders[0].index;
      const tail = config.slice(start + gatewayHeaders[0][0].length);
      const next = tail.search(/^\[(?!mcp_servers\.mcp-gateway(?:\.|\]))/m);
      const stanza = next < 0 ? tail : tail.slice(0, next);
      const url = stanza.match(/^url\s*=\s*("(?:[^"\\]|\\.)*")\s*$/m);
      if (!url || JSON.parse(url[1]) === "http://mcp-gateway.docker.internal/mcp") {
        console.error(`codex kit: ${configPath} has an unsafe or stale mcp-gateway stanza; preserved unchanged`);
        process.exit(125);
      }
    }
  }
}

if (changed) {
  const temporary = path.join(lock, "config");
  try {
    fs.writeFileSync(temporary, config, {encoding: "utf8", flag: "wx", mode: fileMode});
    fs.renameSync(temporary, configPath);
  } finally {
    try { fs.unlinkSync(temporary); } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
  }
}
NODE

# This value is an SBX proxy sentinel. Never copy a host token into the file,
# and never replace a user's existing authentication file.
case "$mode" in
  oauth|apikey)
    if [ ! -e "$CODEX_HOME/auth.json" ] && [ ! -L "$CODEX_HOME/auth.json" ]; then
      printf '%s\n' '{"OPENAI_API_KEY":"proxy-managed"}' >"$CODEX_HOME/auth.json"
    fi
    ;;
esac

cleanup_codex_lock
trap - EXIT HUP INT TERM

# A pipe needs Codex's one-shot stdin form. A single non-command argument is
# an instruction regardless of whitespace; --prompt disambiguates an
# instruction that shares a name with a Codex subcommand.
codex_pipe_prompt=
codex_pipe_prompt_set=
if [ "$#" -eq 0 ] && [ ! -t 0 ]; then
  set -- exec -
elif [ "$#" -eq 2 ] && [ "$1" = --prompt ] && [ ! -t 0 ]; then
  codex_pipe_prompt=$2
  codex_pipe_prompt_set=1
  set --
elif [ "$#" -eq 1 ] && [ ! -t 0 ]; then
  case "$1" in
    -*|agents|exec|e|review|login|logout|mcp|plugin|app-server|remote-control|app|completion|update|doctor|sandbox|debug|apply|a|resume|queue|archive|delete|migrate-rollouts|unarchive|fork|cloud|exec-server|features|help) ;;
    *) codex_pipe_prompt=$1; codex_pipe_prompt_set=1; set -- ;;
  esac
fi
# In an marsh job, /run/marsh/context.md tells the agent how to start child
# jobs (docs/agents.md). It is passed as this run's developer instructions
# (a `-c` override, TOML string); config.toml and AGENTS.md are untouched.
# Management subcommands take no instructions.
set -- --dangerously-bypass-approvals-and-sandbox "$@"
case "${2:-}" in
  login|logout|mcp|plugin|app-server|remote-control|app|completion|update|doctor|sandbox|debug|apply|a|archive|delete|migrate-rollouts|unarchive|cloud|exec-server|features|help|--version|-V) ;;
  *)
    if [ -r /run/marsh/context.md ]; then
      codex_context=$(node -e 'process.stdout.write(JSON.stringify(require("fs").readFileSync(process.argv[1], "utf8")))' /run/marsh/context.md)
      set -- -c "developer_instructions=$codex_context" "$@"
    fi
    ;;
esac
if [ "${codex_pipe_prompt_set:-}" = 1 ]; then
  { printf '%s\n\n' "$codex_pipe_prompt"; cat; } | /usr/local/bin/codex "$@" exec -
  exit $?
fi

# The retained base npm launcher is newer; never select it via inherited PATH.
exec /usr/local/bin/codex "$@"
