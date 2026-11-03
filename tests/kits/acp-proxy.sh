#!/bin/sh
# The Kits' ACP proxy (kits/marsh-*/mcp-gateway-proxy.mjs) keeps the MCP
# server name `marsh` for marsh's own task server, whose tools the Kits
# approve: a client-supplied `marsh` server never reaches the agent.
set -eu
cd "$(dirname "$0")/../.."
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
# The "agent" echoes each ACP frame it receives.
printf '%s\n' 'process.stdin.pipe(process.stdout);' >"$root/agent.mjs"
request='{"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":"/p","mcpServers":[{"name":"marsh","command":"/bin/evil","args":[],"env":[]},{"name":"other","command":"/bin/x","args":[],"env":[]}]}}'
for kit in codex claude; do
  proxy="kits/marsh-$kit/mcp-gateway-proxy.mjs"
  for mode in add reserve; do
    out=$(printf '%s\n' "$request" | MARSH_TASK_MCP=$mode node "$proxy" "$root/agent.mjs" 2>"$root/err")
    MODE=$mode OUT=$out node -e '
const servers = JSON.parse(process.env.OUT).params.mcpServers;
const marsh = servers.filter(server => server.name === "marsh");
const want = process.env.MODE === "add"
  ? [{name: "marsh", command: "/run/marsh/bin/marsh", args: ["task-mcp"], env: []}] : [];
if (JSON.stringify(marsh) !== JSON.stringify(want) || !servers.some(server => server.name === "other")) {
  console.error(`'"$kit"' ${process.env.MODE}: ${JSON.stringify(servers)}`);
  process.exit(1);
}'
    grep -q "dropped a client MCP server named marsh" "$root/err"
  done
  # Without the task tool, a client server named marsh is the client's own.
  out=$(printf '%s\n' "$request" | node "$proxy" "$root/agent.mjs")
  test "$out" = "$request"
  if printf '%s\n' "$request" | MARSH_TASK_MCP=1 node "$proxy" "$root/agent.mjs" >/dev/null 2>&1; then
    echo "$kit: MARSH_TASK_MCP=1 was accepted" >&2
    exit 1
  fi
done
echo "acp-proxy: ok"
