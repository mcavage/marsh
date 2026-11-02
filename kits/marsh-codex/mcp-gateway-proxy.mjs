import { spawn } from "node:child_process";
import { constants } from "node:os";
import { Transform } from "node:stream";

const url = process.env.MCP_GATEWAY_URL;
const token = process.env.MCP_SENTINEL_TOKEN_NAME;
if (Boolean(url) !== Boolean(token) ||
    (url && (url !== "http://mcp-gateway.docker.internal/mcp" ||
      !/^[A-Za-z0-9._-]{1,128}$/.test(token)))) {
  throw new Error("invalid stock MCP Gateway environment");
}

// MARSH_TASK_MCP=1 (set by an adapter whose agent takes the task tool this
// way): every session also gets marsh's task server (`marsh task-mcp`).
const task = process.env.MARSH_TASK_MCP === "1";
delete process.env.MARSH_TASK_MCP;

const agent = spawn(process.execPath, process.argv.slice(2), {
  stdio: ["pipe", "inherit", "inherit"],
});
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => agent.kill(signal));
}
agent.on("exit", (code, signal) => {
  process.stdin.destroy();
  process.exitCode = code ?? (128 + (constants.signals[signal] ?? 1));
});
agent.on("error", error => {
  console.error(`ACP agent failed: ${error.message}`);
  process.stdin.destroy();
  agent.stdin.destroy();
  process.exitCode = 125;
});
agent.stdin.on("error", error => {
  if (error.code !== "EPIPE") console.error(`ACP input failed: ${error.message}`);
});

let pending = Buffer.alloc(0);
const input = new Transform({
  transform(chunk, _encoding, done) {
    pending = Buffer.concat([pending, chunk]);
    let end;
    while ((end = pending.indexOf(10)) !== -1) {
      if (end > 1024 * 1024) return done(new Error("ACP frame too large"));
      const frame = pending.subarray(0, end);
      pending = pending.subarray(end + 1);
      if (url || task) {
        let request;
        try { request = JSON.parse(frame); } catch { /* Agent handles invalid JSON. */ }
        const servers = request?.params?.mcpServers;
        const gateway = url && request?.method === "session/new" &&
          Array.isArray(servers) && servers.length === 0;
        const marsh = task && /^session\/(new|load|resume|fork)$/.test(request?.method ?? "") &&
          Array.isArray(servers) && !servers.some(server => server?.name === "marsh");
        if (gateway || marsh) {
          if (gateway) {
            servers.push({
              type: "http", name: "mcp-gateway", url,
              headers: [{ name: "Authorization", value: `Bearer ${token}` }],
            });
          }
          if (marsh) {
            servers.push({ name: "marsh", command: "/run/marsh/bin/marsh", args: ["task-mcp"], env: [] });
          }
          const rewritten = `${JSON.stringify(request)}\n`;
          if (Buffer.byteLength(rewritten) > 1024 * 1024) {
            return done(new Error("ACP frame too large after Gateway setup"));
          }
          this.push(rewritten);
          continue;
        }
      }
      this.push(frame);
      this.push("\n");
    }
    if (pending.length > 1024 * 1024) return done(new Error("ACP frame too large"));
    done();
  },
  flush(done) {
    if (pending.length) this.push(pending);
    done();
  },
});
input.on("error", error => {
  console.error(`ACP input failed: ${error.message}`);
  agent.kill("SIGTERM");
});
process.stdin.pipe(input).pipe(agent.stdin);
