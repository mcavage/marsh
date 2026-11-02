// marsh's task tool for Pi. In a marsh job whose marsh has `marsh task-mcp`
// (docs/agents.md, "The task tool"), register it as this session's `marsh` MCP
// server with its tools declared to the model (`mcp__marsh__task`). It is
// registered for the session only (Pi's `registerMcpServer`); nothing is
// written to ~/.pi or the project. Pi has no subagent tool of its own.
import { existsSync } from "node:fs";
import { spawnSync } from "node:child_process";

const marsh = "/run/marsh/bin/marsh";

export default function (pi) {
  if (!existsSync("/run/marsh/job.json") || typeof pi.registerMcpServer !== "function") return;
  if (spawnSync(marsh, ["task-mcp", "--help"], { stdio: "ignore" }).status !== 0) return;
  pi.registerMcpServer("marsh", {
    command: marsh,
    args: ["task-mcp"],
    exposure: "direct",
    // A task can run as long as the job; the server also sends progress.
    timeout: 86400,
    description: "Hand work to another agent as a marsh child job (task, task_output, task_apply)",
  });
}
