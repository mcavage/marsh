#!/bin/sh
set -eu
umask 077

selected=${MARSH_SELECTED_HOME:-${HOME:-/home/agent}}
case "$selected" in
  /*) ;;
  *) echo "claude kit: MARSH_SELECTED_HOME must be absolute" >&2; exit 125 ;;
esac
export HOME=$selected

# ACP mode: packaging/agents.json starts claude-session as `claude --acp`.
# The official adapter speaks ACP v1 on stdio and drives the Agent SDK's
# Claude Code through claude-cli.sh, which disables Claude's Bash sandbox.
if [ "${1:-}" = --acp ]; then
  shift
  printf 'marsh-auth-mode=%s\n' "${SBX_CRED_ANTHROPIC_MODE:-none}" >&2
  case "${SBX_CRED_ANTHROPIC_MODE:-none}" in
    apikey|oauth) export ANTHROPIC_API_KEY=proxy-managed ;;
  esac
  CLAUDE_CODE_EXECUTABLE=/opt/marsh/claude-acp/claude-cli.sh
  export CLAUDE_CODE_EXECUTABLE
  exec node /opt/marsh/claude-acp/mcp-gateway-proxy.mjs \
    /opt/marsh/claude-acp/node_modules/@agentclientprotocol/claude-agent-acp/dist/index.js "$@"
fi

mkdir -p "$HOME/.claude"

# Claude keeps onboarding and per-project trust in ~/.claude.json. The Kit
# lifecycle hook runs in a daemon-owned neutral home, so the selected home must
# be seeded here as well. Merge only these documented flags, preserving every
# other user setting, and replace the file atomically. Fresh job containers can
# share one selected home, so serialize the read/merge/rename sequence with a
# portable directory lock. A crashed writer's lock is reclaimable after 30
# seconds; healthy contention is bounded to ten seconds.
claude_config="$HOME/.claude.json"
claude_lock="$claude_config.marsh-lock"
claude_lock_acquired=
cleanup_claude_lock() {
  if [ "${claude_lock_acquired:-}" = 1 ]; then
    rm -f "$claude_lock/created" "$claude_lock/config" \
      "$claude_lock/settings" 2>/dev/null || :
    rmdir "$claude_lock" 2>/dev/null || :
    claude_lock_acquired=
  fi
}
trap 'cleanup_claude_lock' EXIT
trap 'cleanup_claude_lock; exit 129' HUP
trap 'cleanup_claude_lock; exit 130' INT
trap 'cleanup_claude_lock; exit 143' TERM

claude_lock_attempt=0
while ! mkdir "$claude_lock" 2>/dev/null; do
  claude_now=$(date +%s)
  claude_created=$(cat "$claude_lock/created" 2>/dev/null || printf '0')
  case "$claude_created" in
    ''|*[!0-9]*) claude_created=0 ;;
  esac
  if [ "$claude_created" -gt 0 ] &&
     [ $((claude_now - claude_created)) -ge 30 ]; then
    # Remove only files owned by this lock protocol. Unexpected contents keep
    # the directory in place and cause the bounded wait to fail closed.
    rm -f "$claude_lock/created" "$claude_lock/config" \
      "$claude_lock/settings" 2>/dev/null || :
    rmdir "$claude_lock" 2>/dev/null || :
  fi
  claude_lock_attempt=$((claude_lock_attempt + 1))
  if [ "$claude_lock_attempt" -ge 200 ]; then
    echo "claude kit: timed out waiting for selected-home config lock" >&2
    exit 125
  fi
  sleep 0.05
done
claude_lock_acquired=1
date +%s >"$claude_lock/created"

claude_settings="$HOME/.claude/settings.json"
node - "$claude_config" "$PWD" "$claude_lock" "$claude_settings" \
  "${SBX_CRED_ANTHROPIC_MODE:-none}" \
  "${MCP_GATEWAY_URL:-}" "${MCP_SENTINEL_TOKEN_NAME:-}" <<'NODE'
const fs = require("fs");
const nodePath = require("path");
const [path, project, lock, settingsPath, credentialMode, mcpUrl, mcpToken] = process.argv.slice(2);

// Older entrypoints used randomized sibling directories. Remove only stale,
// recognizable leftovers; regular files, symlinks, recent directories, and
// directories with unexpected contents are user state and remain untouched.
const parent = nodePath.dirname(path);
const temporaryPrefix = `${nodePath.basename(path)}.marsh-`;
const staleBefore = Date.now() - 30_000;
for (const name of fs.readdirSync(parent)) {
  if (!name.startsWith(temporaryPrefix) || name === nodePath.basename(lock)) continue;
  const candidate = nodePath.join(parent, name);
  const stat = fs.lstatSync(candidate);
  if (!stat.isDirectory() || stat.isSymbolicLink() || stat.mtimeMs > staleBefore) continue;
  const contents = fs.readdirSync(candidate);
  if (contents.some(entry => entry !== "config")) continue;
  const temporary = nodePath.join(candidate, "config");
  if (contents.length === 1) {
    const temporaryStat = fs.lstatSync(temporary);
    if (!temporaryStat.isFile() || temporaryStat.isSymbolicLink()) continue;
    fs.unlinkSync(temporary);
  }
  fs.rmdirSync(candidate);
}

let config = {};
if (fs.existsSync(path)) {
  const stat = fs.lstatSync(path);
  if (stat.isSymbolicLink() || !stat.isFile()) {
    console.error(`claude kit: ${path} must be a regular file; preserved unchanged`);
    process.exit(125);
  }
  try {
    config = JSON.parse(fs.readFileSync(path, "utf8"));
  } catch (_) {
    // Do not print parser excerpts: this user-owned file may contain sensitive
    // values and malformed JSON must never be replaced with Kit defaults.
    console.error(`claude kit: ${path} is malformed JSON; preserved unchanged`);
    process.exit(125);
  }
  if (config === null || Array.isArray(config) || typeof config !== "object") {
    console.error(`claude kit: ${path} must contain a JSON object; preserved unchanged`);
    process.exit(125);
  }
}
let changed = false;
if (config.bypassPermissionsModeAccepted === undefined) {
  config.bypassPermissionsModeAccepted = true;
  changed = true;
}
if (config.hasCompletedOnboarding === undefined) {
  config.hasCompletedOnboarding = true;
  changed = true;
}
if (config.projects === undefined) {
  config.projects = {};
  changed = true;
}
if (config.projects === null || Array.isArray(config.projects) ||
    typeof config.projects !== "object") {
  console.error(`claude kit: ${path}.projects must be a JSON object; preserved unchanged`);
  process.exit(125);
}
const existing = config.projects[project];
if (existing === undefined) {
  config.projects[project] = {hasTrustDialogAccepted: true};
  changed = true;
} else {
  if (existing === null || Array.isArray(existing) || typeof existing !== "object") {
    console.error(`claude kit: project trust entry must be a JSON object; ${path} preserved unchanged`);
    process.exit(125);
  }
  if (existing.hasTrustDialogAccepted !== true) {
    existing.hasTrustDialogAccepted = true;
    changed = true;
  }
}
// Register the stock gateway under the same lock as project trust. A second
// fresh job must not lose either setting in a last-writer-wins config update.
// Preserve a user-owned registration with a different destination.
if (mcpUrl && mcpToken) {
  if (config.mcpServers === undefined) {
    config.mcpServers = {};
    changed = true;
  }
  if (config.mcpServers === null || Array.isArray(config.mcpServers) ||
      typeof config.mcpServers !== "object") {
    console.error(`claude kit: ${path}.mcpServers must be a JSON object; preserved unchanged`);
    process.exit(125);
  }
  const existingGateway = config.mcpServers["mcp-gateway"];
  if (existingGateway === undefined ||
      (existingGateway?.type === "http" &&
       existingGateway.url === "http://mcp-gateway.docker.internal/mcp" &&
       /^Bearer [A-Za-z0-9._-]{1,128}$/.test(existingGateway.headers?.Authorization) &&
       Object.keys(existingGateway).every(key => ["type", "url", "headers"].includes(key)) &&
       Object.keys(existingGateway.headers).every(key => key === "Authorization"))) {
    const expected = {type: "http", url: mcpUrl,
      headers: {Authorization: `Bearer ${mcpToken}`}};
    if (JSON.stringify(existingGateway) !== JSON.stringify(expected)) {
      config.mcpServers["mcp-gateway"] = expected;
      changed = true;
    }
  } else if (typeof existingGateway?.url !== "string" ||
             existingGateway.url === "http://mcp-gateway.docker.internal/mcp") {
    console.error(`claude kit: ${path}.mcpServers.mcp-gateway has an unsafe or stale stock shape; preserved unchanged`);
    process.exit(125);
  }
}
if (changed) {
  // The lock directory is in the target directory, so this remains a
  // same-filesystem atomic replacement.
  const temporary = nodePath.join(lock, "config");
  try {
    fs.writeFileSync(temporary, `${JSON.stringify(config, null, 2)}\n`, {
      encoding: "utf8", flag: "wx", mode: 0o600,
    });
    fs.renameSync(temporary, path);
  } finally {
    try { fs.unlinkSync(temporary); } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
  }
}

let settings = {};
let settingsStat;
try {
  settingsStat = fs.lstatSync(settingsPath);
} catch (error) {
  if (error.code !== "ENOENT") throw error;
}
if (settingsStat !== undefined) {
  const stat = settingsStat;
  if (stat.isSymbolicLink() || !stat.isFile()) {
    console.error(`claude kit: ${settingsPath} must be a regular file; preserved unchanged`);
    process.exit(125);
  }
  try {
    settings = JSON.parse(fs.readFileSync(settingsPath, "utf8"));
  } catch (_) {
    console.error(`claude kit: ${settingsPath} is malformed JSON; preserved unchanged`);
    process.exit(125);
  }
  if (settings === null || Array.isArray(settings) || typeof settings !== "object") {
    console.error(`claude kit: ${settingsPath} must contain a JSON object; preserved unchanged`);
    process.exit(125);
  }
}

let settingsChanged = false;
if (settings.themeId === undefined) {
  settings.themeId = 1;
  settingsChanged = true;
}
if (settings.alwaysThinkingEnabled === undefined) {
  settings.alwaysThinkingEnabled = true;
  settingsChanged = true;
}
if (settings.permissions === undefined) {
  settings.permissions = {defaultMode: "bypassPermissions"};
  settingsChanged = true;
} else if (settings.permissions === null || Array.isArray(settings.permissions) ||
           typeof settings.permissions !== "object") {
  console.error(`claude kit: ${settingsPath}.permissions must be a JSON object; preserved unchanged`);
  process.exit(125);
} else if (settings.permissions.defaultMode === undefined) {
  settings.permissions.defaultMode = "bypassPermissions";
  settingsChanged = true;
}
if (settings.bypassPermissionsModeAccepted === undefined) {
  settings.bypassPermissionsModeAccepted = true;
  settingsChanged = true;
}
if (settings.skipDangerousModePermissionPrompt === undefined) {
  settings.skipDangerousModePermissionPrompt = true;
  settingsChanged = true;
}
// Use one proxy-managed auth path. Keep a different user-owned helper intact.
if (credentialMode !== "none" && settings.apiKeyHelper === undefined) {
  settings.apiKeyHelper = "echo proxy-managed";
  settingsChanged = true;
}
if (settingsChanged) {
  const temporary = nodePath.join(lock, "settings");
  try {
    fs.writeFileSync(temporary, `${JSON.stringify(settings, null, 2)}\n`, {
      encoding: "utf8", flag: "wx", mode: 0o600,
    });
    fs.renameSync(temporary, settingsPath);
  } finally {
    try { fs.unlinkSync(temporary); } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
  }
}
NODE

cleanup_claude_lock
trap - EXIT HUP INT TERM

# Claude otherwise treats the stock proxy sentinel as a custom key, prompting
# on first launch and warning that both authentication paths are configured.
if [ "${SBX_CRED_ANTHROPIC_MODE:-none}" != none ]; then
  unset ANTHROPIC_API_KEY
fi
# The job container is the sandbox boundary. Claude Code's own Bash sandbox
# (bubblewrap on Linux) cannot create namespaces in it, so a project or user
# setting `sandbox.enabled` written for the Mac would fail every first Bash
# call with `bwrap: No permissions to create a new namespace` and then retry
# with the sandbox disabled. Command-line settings outrank user and project
# settings and are not written anywhere.
#
# In an marsh job, /run/marsh/context.md tells the agent how to start child
# jobs (docs/agents.md). It is appended to Claude's system prompt for this run
# only; nothing is written to the project or the selected home. Subcommands
# take no prompt flags.
case "${1:-}" in
  mcp|plugin|plugins|setup-token|doctor|update|upgrade|install|migrate-installer|config|agents|auth|remote-control|help|completion) ;;
  *)
    if [ -r /run/marsh/context.md ]; then
      exec claude --dangerously-skip-permissions --settings '{"sandbox":{"enabled":false}}' \
        --append-system-prompt "$(cat /run/marsh/context.md)" "$@"
    fi
    ;;
esac
exec claude --dangerously-skip-permissions --settings '{"sandbox":{"enabled":false}}' "$@"
