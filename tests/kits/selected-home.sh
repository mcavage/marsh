#!/bin/sh
set -eu

root=$(mktemp -d "${TMPDIR:-/tmp}/marsh-kit-home.XXXXXX")
trap 'rm -rf "$root"' EXIT HUP INT TERM
bin="$root/bin"
mkdir -p "$bin"

make_fake() {
  name=$1
  cat >"$bin/$name" <<'EOF'
#!/bin/sh
printf '%s\n' "$HOME" >"$MARSH_TEST_CAPTURE/home"
printf '%s\n' "$@" >"$MARSH_TEST_CAPTURE/args"
printf '%s\n' "${ANTHROPIC_API_KEY:-}" >"$MARSH_TEST_CAPTURE/anthropic-key"
EOF
  chmod 755 "$bin/$name"
}

make_fake claude
make_fake codex
make_fake pi

# The Codex adapter execs its pinned /usr/local/bin/codex, never PATH. Test a
# copy whose pin names the fake; the adapter logic is otherwise byte-identical.
codex_entry="$root/codex-entrypoint.sh"
sed "s#/usr/local/bin/codex#$bin/codex#g" kits/marsh-codex/marsh-entrypoint.sh >"$codex_entry"
adapter() {
  if [ "$1" = codex ]; then printf "%s\n" "$codex_entry"; else printf "%s\n" "kits/marsh-$1/marsh-entrypoint.sh"; fi
}

run_kit() {
  kit=$1
  home="$root/$kit-home"
  capture="$root/$kit-capture"
  mkdir -p "$home" "$capture"
  status=0
  PATH="$bin:$PATH" \
    MARSH_TEST_CAPTURE="$capture" \
    MARSH_SELECTED_HOME="$home" \
    SBX_CRED_ANTHROPIC_MODE=apikey \
    SBX_CRED_OPENAI_MODE=oauth \
  sh "$(adapter "$kit")" --version || status=$?
  test "$status" -eq 0 || return "$status"
  test "$(cat "$capture/home")" = "$home"
  test "$(tail -n 1 "$capture/args")" = "--version"
}

run_kit claude
run_kit codex
run_kit pi

test -f "$root/claude-home/.claude/settings.json"
grep -q 'proxy-managed' "$root/claude-home/.claude/settings.json"
node -e '
  const config = require(process.argv[1]);
  if (config.projects?.[process.argv[2]]?.hasTrustDialogAccepted !== true) process.exit(1);
' "$root/claude-home/.claude.json" "$PWD"
test ! -e "$root/claude-home/.claude/.credentials.json"
test -f "$root/codex-home/.codex/config.toml"
grep -Fqx "[projects.\"$PWD\"]" "$root/codex-home/.codex/config.toml"
grep -Fqx 'trust_level = "trusted"' "$root/codex-home/.codex/config.toml"
test "$(cat "$root/codex-home/.codex/auth.json")" = \
  '{"OPENAI_API_KEY":"proxy-managed"}'
test -d "$root/pi-home/.pi"
test "$(head -n 1 "$root/pi-capture/args")" = --approve
test "$(cat "$root/pi-capture/anthropic-key")" = proxy-managed

# The stock Gateway registration belongs to the selected home, not the Kit
# lifecycle home. It is merged under the same lock as project trust. Repeated
# jobs refresh a stale stock endpoint without replacing other user settings.
MCP_GATEWAY_URL=http://mcp-gateway.docker.internal/mcp \
  MCP_SENTINEL_TOKEN_NAME=proxy-managed run_kit claude
MCP_GATEWAY_URL=http://mcp-gateway.docker.internal/mcp \
  MCP_SENTINEL_TOKEN_NAME=proxy-managed run_kit codex
node -e '
  const config = require(process.argv[1]);
  const gateway = config.mcpServers?.["mcp-gateway"];
  if (gateway?.type !== "http" || gateway.url !== process.argv[2] ||
      gateway.headers?.Authorization !== "Bearer proxy-managed") process.exit(1);
' "$root/claude-home/.claude.json" http://mcp-gateway.docker.internal/mcp
for file in "$root/codex-home/.codex/config.toml"; do
  grep -Fqx 'url = "http://mcp-gateway.docker.internal/mcp"' "$file"
  grep -Fqx '[mcp_servers.mcp-gateway.http_headers]' "$file"
  grep -Fqx 'Authorization = "Bearer proxy-managed"' "$file"
done
# The sentinel name is stock-provided, not fixed by the kit. A changed valid
# sentinel must refresh the previously generated entry on both clients.
MCP_GATEWAY_URL=http://mcp-gateway.docker.internal/mcp \
  MCP_SENTINEL_TOKEN_NAME=proxy-managed-2 run_kit claude
MCP_GATEWAY_URL=http://mcp-gateway.docker.internal/mcp \
  MCP_SENTINEL_TOKEN_NAME=proxy-managed-2 run_kit codex
node -e '
  const config = require(process.argv[1]);
  if (config.mcpServers?.["mcp-gateway"]?.headers?.Authorization !==
      "Bearer proxy-managed-2") process.exit(1);
' "$root/claude-home/.claude.json"
grep -Fqx 'Authorization = "Bearer proxy-managed-2"' \
  "$root/codex-home/.codex/config.toml"
MCP_GATEWAY_URL=http://mcp-gateway.docker.internal/mcp \
  MCP_SENTINEL_TOKEN_NAME=proxy-managed run_kit claude
MCP_GATEWAY_URL=http://mcp-gateway.docker.internal/mcp \
  MCP_SENTINEL_TOKEN_NAME=proxy-managed run_kit codex
node -e '
  const config = require(process.argv[1]);
  if (config.mcpServers?.["mcp-gateway"]?.headers?.Authorization !==
      "Bearer proxy-managed") process.exit(1);
' "$root/claude-home/.claude.json"
grep -Fqx 'Authorization = "Bearer proxy-managed"' \
  "$root/codex-home/.codex/config.toml"

# A same-destination stanza with an unrecognized shape may contain client-owned
# state. Fail closed and leave it byte-identical instead of launching with a
# stale or unauthenticated stock Gateway registration.
cp "$root/claude-home/.claude.json" "$root/claude-gateway.valid"
cp "$root/codex-home/.codex/config.toml" "$root/codex-gateway.valid"
node -e '
  const fs = require("fs");
  const file = process.argv[1];
  const config = require(file);
  config.mcpServers["mcp-gateway"].extra = true;
  fs.writeFileSync(file, JSON.stringify(config, null, 2) + "\n");
' "$root/claude-home/.claude.json"
sed -i.bak 's/\.http_headers]/.headers]/' "$root/codex-home/.codex/config.toml"
rm "$root/codex-home/.codex/config.toml.bak"
cp "$root/claude-home/.claude.json" "$root/claude-stale.before"
cp "$root/codex-home/.codex/config.toml" "$root/codex-stale.before"
if MCP_GATEWAY_URL=http://mcp-gateway.docker.internal/mcp \
  MCP_SENTINEL_TOKEN_NAME=proxy-managed run_kit claude; then
  echo 'stale Claude Gateway shape was accepted' >&2
  exit 1
fi
if MCP_GATEWAY_URL=http://mcp-gateway.docker.internal/mcp \
  MCP_SENTINEL_TOKEN_NAME=proxy-managed run_kit codex; then
  echo 'stale Codex Gateway shape was accepted' >&2
  exit 1
fi
cmp "$root/claude-stale.before" "$root/claude-home/.claude.json"
cmp "$root/codex-stale.before" "$root/codex-home/.codex/config.toml"
cp "$root/claude-gateway.valid" "$root/claude-home/.claude.json"
cp "$root/codex-gateway.valid" "$root/codex-home/.codex/config.toml"

# A manually configured server of the same name at another URL is user state,
# not a stock Gateway stanza to overwrite.
node -e '
  const fs = require("fs");
  const file = process.argv[1];
  const config = require(file);
  config.mcpServers["mcp-gateway"] = {type: "http", url: "https://mine.example/mcp",
    headers: {Authorization: "Bearer my-private-value"}};
  fs.writeFileSync(file, JSON.stringify(config, null, 2) + "\n");
' "$root/claude-home/.claude.json"
cp "$root/claude-home/.claude.json" "$root/claude-user-mcp.before"
node -e '
  const fs = require("fs");
  const file = process.argv[1];
  const config = fs.readFileSync(file, "utf8");
  fs.writeFileSync(file, config.replace("http://mcp-gateway.docker.internal/mcp",
    "https://mine.example/mcp"));
' "$root/codex-home/.codex/config.toml"
cp "$root/codex-home/.codex/config.toml" "$root/codex-user-mcp.before"
MCP_GATEWAY_URL=http://mcp-gateway.docker.internal/mcp \
  MCP_SENTINEL_TOKEN_NAME=proxy-managed run_kit claude
MCP_GATEWAY_URL=http://mcp-gateway.docker.internal/mcp \
  MCP_SENTINEL_TOKEN_NAME=proxy-managed run_kit codex
cmp "$root/claude-user-mcp.before" "$root/claude-home/.claude.json"
cmp "$root/codex-user-mcp.before" "$root/codex-home/.codex/config.toml"

# A dangling user-owned settings symlink is rejected without creating or
# overwriting its target outside the selected Claude directory.
claude_symlink_home="$root/claude-symlink-home"
claude_symlink_capture="$root/claude-symlink-capture"
claude_settings_target="$root/claude-settings-target"
mkdir -p "$claude_symlink_home/.claude" "$claude_symlink_capture"
ln -s "$claude_settings_target" "$claude_symlink_home/.claude/settings.json"
if PATH="$bin:$PATH" \
  MARSH_TEST_CAPTURE="$claude_symlink_capture" \
  MARSH_SELECTED_HOME="$claude_symlink_home" \
  SBX_CRED_ANTHROPIC_MODE=apikey \
  sh kits/marsh-claude/marsh-entrypoint.sh --version \
  >"$root/claude-symlink.out" 2>"$root/claude-symlink.err"; then
  echo 'Claude settings symlink unexpectedly accepted' >&2
  exit 1
fi
test -L "$claude_symlink_home/.claude/settings.json"
test ! -e "$claude_settings_target"
grep -Fq 'must be a regular file; preserved unchanged' "$root/claude-symlink.err"

# Existing valid settings retain unrelated fields and explicit user choices,
# while a resolved SBX credential installs the exact proxy helper needed by
# Claude Code.
cat >"$root/claude-home/.claude/settings.json" <<'EOF'
{
  "userValue": "preserved",
  "themeId": 7,
  "permissions": { "defaultMode": "plan", "extra": true },
  "skipDangerousModePermissionPrompt": false
}
EOF
printf '%s\n' '{"userValue":"preserved"}' >"$root/claude-home/.claude.json"
printf '%s\n' 'user_value = "preserved"' >"$root/codex-home/.codex/config.toml"
printf '%s\n' user-auth >"$root/codex-home/.codex/auth.json"
run_kit claude
run_kit codex
node -e '
  const settings = require(process.argv[1]);
  if (settings.userValue !== "preserved" || settings.themeId !== 7 ||
      settings.permissions?.defaultMode !== "plan" ||
      settings.permissions?.extra !== true ||
      settings.skipDangerousModePermissionPrompt !== false ||
      settings.apiKeyHelper !== "echo proxy-managed") process.exit(1);
' "$root/claude-home/.claude/settings.json"
node -e '
  const config = require(process.argv[1]);
  if (config.userValue !== "preserved" ||
      config.projects?.[process.argv[2]]?.hasTrustDialogAccepted !== true) process.exit(1);
' "$root/claude-home/.claude.json" "$PWD"
test "$(head -n 1 "$root/codex-home/.codex/config.toml")" = 'user_value = "preserved"'
test "$(grep -Fxc "[projects.\"$PWD\"]" "$root/codex-home/.codex/config.toml")" = 1
test "$(cat "$root/codex-home/.codex/auth.json")" = user-auth

# Credential mode none must not synthesize apiKeyHelper. Other defaults still
# make a fresh selected home noninteractive.
claude_none_home="$root/claude-none-home"
claude_none_capture="$root/claude-none-capture"
mkdir -p "$claude_none_home" "$claude_none_capture"
PATH="$bin:$PATH" \
  MARSH_TEST_CAPTURE="$claude_none_capture" \
  MARSH_SELECTED_HOME="$claude_none_home" \
  SBX_CRED_ANTHROPIC_MODE=none \
  sh kits/marsh-claude/marsh-entrypoint.sh --version
node -e '
  const settings = require(process.argv[1]);
  if (Object.hasOwn(settings, "apiKeyHelper") ||
      settings.permissions?.defaultMode !== "bypassPermissions") process.exit(1);
' "$claude_none_home/.claude/settings.json"

# Malformed settings fail closed and remain byte-for-byte unchanged without
# leaking their contents in diagnostics.
printf '%s\n' '{private-malformed-settings' >"$claude_none_home/.claude/settings.json"
cp "$claude_none_home/.claude/settings.json" "$root/claude-settings.before"
if PATH="$bin:$PATH" \
  MARSH_TEST_CAPTURE="$claude_none_capture" \
  MARSH_SELECTED_HOME="$claude_none_home" \
  SBX_CRED_ANTHROPIC_MODE=apikey \
  sh kits/marsh-claude/marsh-entrypoint.sh --version \
  >"$root/claude-settings.out" 2>"$root/claude-settings.err"; then
  echo 'malformed Claude settings unexpectedly accepted' >&2
  exit 1
fi
cmp "$root/claude-settings.before" "$claude_none_home/.claude/settings.json"
grep -Fq 'is malformed JSON; preserved unchanged' "$root/claude-settings.err"
if grep -Fq private-malformed-settings "$root/claude-settings.err"; then
  echo 'malformed Claude settings content leaked in diagnostics' >&2
  exit 1
fi

printf '%s\n' '[]' >"$claude_none_home/.claude/settings.json"
if PATH="$bin:$PATH" \
  MARSH_TEST_CAPTURE="$claude_none_capture" \
  MARSH_SELECTED_HOME="$claude_none_home" \
  SBX_CRED_ANTHROPIC_MODE=apikey \
  sh kits/marsh-claude/marsh-entrypoint.sh --version \
  >"$root/claude-settings.out" 2>"$root/claude-settings.err"; then
  echo 'non-object Claude settings unexpectedly accepted' >&2
  exit 1
fi
test "$(cat "$claude_none_home/.claude/settings.json")" = '[]'
grep -Fq 'must contain a JSON object; preserved unchanged' \
  "$root/claude-settings.err"

# Repeated starts are byte-for-byte idempotent once this project is trusted.
cp "$root/codex-home/.codex/config.toml" "$root/codex-config.before"
run_kit codex
cmp "$root/codex-config.before" "$root/codex-home/.codex/config.toml"

# Concurrent fresh containers sharing one home must retain every project trust
# entry exactly once. This also exercises concurrent first initialization.
rm -rf "$root/codex-home" "$root/codex-capture"
mkdir -p "$root/codex-home" "$root/codex-capture"
codex_pids=
codex_index=1
while [ "$codex_index" -le 8 ]; do
  project="$root/codex-project-$codex_index"
  capture="$root/codex-capture-$codex_index"
  mkdir -p "$project" "$capture"
  project=$(cd "$project" && pwd -P)
  (
    cd "$project"
    PATH="$bin:$PATH" \
      MARSH_TEST_CAPTURE="$capture" \
      MARSH_SELECTED_HOME="$root/codex-home" \
      SBX_CRED_OPENAI_MODE=oauth \
      sh "$codex_entry" --version
  ) &
  codex_pids="$codex_pids $!"
  codex_index=$((codex_index + 1))
done
for codex_pid in $codex_pids; do
  wait "$codex_pid"
done
codex_index=1
while [ "$codex_index" -le 8 ]; do
  project="$root/codex-project-$codex_index"
  project=$(cd "$project" && pwd -P)
  test "$(grep -Fxc "[projects.\"$project\"]" \
    "$root/codex-home/.codex/config.toml")" = 1
  codex_index=$((codex_index + 1))
done
test "$(grep -c '^\[projects\.' "$root/codex-home/.codex/config.toml")" = 8

# A crashed writer's old protocol-owned lock is reclaimed without disturbing
# the completed config beside it.
codex_stale_lock="$root/codex-home/.codex/config.toml.marsh-lock"
mkdir "$codex_stale_lock"
printf '%s\n' 1 >"$codex_stale_lock/created"
printf '%s\n' interrupted >"$codex_stale_lock/config"
run_kit codex
test ! -e "$codex_stale_lock"

# A malformed config path is user state: fail closed without following or
# replacing it, and do not expose its contents in the diagnostic.
rm -f "$root/codex-home/.codex/config.toml"
printf '%s\n' sensitive-user-config >"$root/codex-config-target"
ln -s "$root/codex-config-target" "$root/codex-home/.codex/config.toml"
if PATH="$bin:$PATH" \
  MARSH_TEST_CAPTURE="$root/codex-capture" \
  MARSH_SELECTED_HOME="$root/codex-home" \
  SBX_CRED_OPENAI_MODE=oauth \
  sh "$codex_entry" --version \
  >"$root/codex-malformed.out" 2>"$root/codex-malformed.err"; then
  echo 'malformed Codex config path unexpectedly accepted' >&2
  exit 1
fi
test "$(cat "$root/codex-config-target")" = sensitive-user-config
test -L "$root/codex-home/.codex/config.toml"
grep -Fq 'must be a regular file; preserved unchanged' "$root/codex-malformed.err"
if grep -Fq sensitive-user-config "$root/codex-malformed.err"; then
  echo 'Codex config content leaked in diagnostics' >&2
  exit 1
fi

# Invalid TOML bytes are likewise preserved byte-for-byte and never echoed.
rm "$root/codex-home/.codex/config.toml"
printf 'private\000value\n' >"$root/codex-home/.codex/config.toml"
cp "$root/codex-home/.codex/config.toml" "$root/codex-malformed.before"
if PATH="$bin:$PATH" \
  MARSH_TEST_CAPTURE="$root/codex-capture" \
  MARSH_SELECTED_HOME="$root/codex-home" \
  SBX_CRED_OPENAI_MODE=oauth \
  sh "$codex_entry" --version \
  >"$root/codex-malformed.out" 2>"$root/codex-malformed.err"; then
  echo 'malformed Codex TOML unexpectedly accepted' >&2
  exit 1
fi
cmp "$root/codex-malformed.before" "$root/codex-home/.codex/config.toml"
grep -Fq 'is malformed TOML; preserved unchanged' "$root/codex-malformed.err"
if grep -Fq private "$root/codex-malformed.err"; then
  echo 'malformed Codex TOML content leaked in diagnostics' >&2
  exit 1
fi

# Concurrent fresh containers may initialize the same selected home. Every
# read/merge/rename must survive, rather than losing a project trust entry to a
# last-writer-wins race.
claude_pids=
claude_index=1
while [ "$claude_index" -le 8 ]; do
  project="$root/claude-project-$claude_index"
  mkdir -p "$project"
  (
    cd "$project"
    PATH="$bin:$PATH" \
      MARSH_TEST_CAPTURE="$root/claude-capture" \
      MARSH_SELECTED_HOME="$root/claude-home" \
      SBX_CRED_ANTHROPIC_MODE=apikey \
      MCP_GATEWAY_URL=http://mcp-gateway.docker.internal/mcp \
      MCP_SENTINEL_TOKEN_NAME=proxy-managed \
      sh "$OLDPWD/kits/marsh-claude/marsh-entrypoint.sh" --version
  ) &
  claude_pids="$claude_pids $!"
  claude_index=$((claude_index + 1))
done
for claude_pid in $claude_pids; do
  wait "$claude_pid"
done
node -e '
  const config = require(process.argv[1]);
  for (let index = 1; index <= 8; index += 1) {
    const suffix = `/claude-project-${index}`;
    const project = Object.entries(config.projects || {}).find(
      ([path, value]) => path.endsWith(suffix) && value?.hasTrustDialogAccepted === true
    );
    if (!project) process.exit(1);
  }
  if (config.mcpServers?.["mcp-gateway"]?.headers?.Authorization !==
      "Bearer proxy-managed") process.exit(1);
' "$root/claude-home/.claude.json"

# A crashed lock and old randomized atomic-write directory are reclaimed. The
# lock cleanup removes only protocol-owned files, and stale-directory cleanup
# refuses symlinks, regular files, recent directories, or unexpected contents.
stale_lock="$root/claude-home/.claude.json.marsh-lock"
mkdir "$stale_lock"
printf '%s\n' 1 >"$stale_lock/created"
printf '%s\n' interrupted >"$stale_lock/config"
stale_directory="$root/claude-home/.claude.json.marsh-orphan"
mkdir "$stale_directory"
printf '%s\n' interrupted >"$stale_directory/config"
touch -t 200001010000 "$stale_directory" "$stale_directory/config"
run_kit claude
test ! -e "$stale_lock"
test ! -e "$stale_directory"

# A malformed user-owned Claude config fails with a bounded diagnostic and is
# never replaced by Kit defaults. A stale regular file with a similar name is
# user state and is not mistaken for a protocol-owned temporary directory.
printf '%s\n' '{malformed-user-config' >"$root/claude-home/.claude.json"
printf '%s\n' stale >"$root/claude-home/.claude.json.marsh-stale"
if PATH="$bin:$PATH" \
  MARSH_TEST_CAPTURE="$root/claude-capture" \
  MARSH_SELECTED_HOME="$root/claude-home" \
  SBX_CRED_ANTHROPIC_MODE=apikey \
  sh kits/marsh-claude/marsh-entrypoint.sh --version \
  >"$root/malformed.out" 2>"$root/malformed.err"; then
  echo 'malformed Claude config unexpectedly accepted' >&2
  exit 1
fi
test "$(cat "$root/claude-home/.claude.json")" = '{malformed-user-config'
test "$(cat "$root/claude-home/.claude.json.marsh-stale")" = stale
grep -Fq 'is malformed JSON; preserved unchanged' "$root/malformed.err"
if grep -Fq '{malformed-user-config' "$root/malformed.err"; then
  echo 'malformed Claude config content leaked in diagnostics' >&2
  exit 1
fi

if MARSH_SELECTED_HOME=relative sh kits/marsh-pi/marsh-entrypoint.sh >/dev/null 2>&1; then
  echo 'relative selected home unexpectedly accepted' >&2
  exit 1
fi

# Only documented proxy sentinels may be materialized.
if grep -R -E '(sk-ant-|sk-proj-|Bearer [A-Za-z0-9_-]{20,})' \
  "$root/claude-home" "$root/codex-home" "$root/pi-home" >/dev/null; then
  echo 'credential-like material appeared in a selected home' >&2
  exit 1
fi
