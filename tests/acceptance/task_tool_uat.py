#!/usr/bin/env python3
"""The task tool (`marsh task-mcp`, docs/agents.md "The task tool") black-box acceptance.

Drives only public callers: the host CLI, a `shell` Kit job that speaks MCP to
`marsh task-mcp` on stdio (T01, no model calls), and real `claude -p` jobs that
are given the server with `--mcp-config` and call its tools (T02-T04, billed:
only with --live). Observes the tool results, the user's files, `marsh jobs`
lineage and `marsh splits`. Runs in an isolated MARSH_HOME/control scope and
removes only VMs from its own ownership map.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import shlex
import sys
from typing import Any

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from harness import ScenarioSuite, require, scenario_parser, suite_main, text  # noqa: E402
from provenance import stock_baseline  # noqa: E402

SCENARIOS = {
    "T01-protocol-and-refusals": "protocol_and_refusals",
    "T02-live-shared": "live_shared",
    "T03-live-copy-apply": "live_copy_apply",
    "T04-live-background": "live_background",
}
ORDER = list(SCENARIOS)
LIVE = {"T02-live-shared", "T03-live-copy-apply", "T04-live-background"}
MCP_CONFIG = json.dumps({"mcpServers": {"marsh": {"command": "/run/marsh/bin/marsh", "args": ["task-mcp"]}}})

# A minimal MCP client, run inside a `shell` job against `marsh task-mcp`.
CLIENT = r"""
import json, subprocess, sys
server = subprocess.Popen(["marsh", "task-mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
def rpc(n, method, params):
    server.stdin.write(json.dumps({"jsonrpc": "2.0", "id": n, "method": method, "params": params}) + "\n")
    server.stdin.flush()
    while True:
        reply = json.loads(server.stdout.readline())
        if reply.get("id") == n:
            return reply
out = {"init": rpc(1, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                     "clientInfo": {"name": "uat", "version": "0"}})["result"]}
server.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
out["tools"] = rpc(2, "tools/list", {})["result"]["tools"]
out["calls"] = [rpc(3 + i, "tools/call", {"name": name, "arguments": arguments})["result"]
                for i, (name, arguments) in enumerate(json.loads(sys.argv[1]))]
server.stdin.close()
out["exit"] = server.wait(timeout=60)
print("@@" + json.dumps(out))
"""


class TaskTool(ScenarioSuite):
    LABEL = "task-tool"
    EVIDENCE = "task-tool.json"
    SCENARIOS = SCENARIOS
    ORDER = ORDER

    def __init__(self, arguments: argparse.Namespace) -> None:
        super().__init__(arguments)
        self.selected = [s for s in ORDER if (not arguments.only or s in arguments.only)
                         and (s not in LIVE or arguments.live)]
        registry = self.control_home / "commands.json"
        mapping = json.loads(registry.read_text())
        for name in ("claude", "codex", "shell"):
            mapping.setdefault(name, str(arguments.guest_artifacts / "kits" / f"marsh-{name}"))
        registry.write_text(json.dumps(mapping, sort_keys=True) + "\n", encoding="utf-8")

    def drive(self, calls: list[list[Any]], *, prefix: str = "") -> dict[str, Any]:
        """One `shell` job: an MCP client making `calls` to `marsh task-mcp`."""
        script = f"{prefix}python3 -c {shlex.quote(CLIENT)} {shlex.quote(json.dumps(calls))}"
        completed = self.cli(["run", "shell", "-c", script], self.project, timeout=600)
        out = text(completed.stdout)
        require(completed.returncode == 0 and "@@" in out,
                f"MCP client job failed ({completed.returncode}): {out[-600:]!r} {text(completed.stderr)[-800:]!r}")
        return json.loads(out.split("@@", 1)[1])

    def claude(self, prompt: str, timeout: float = 900) -> str:
        """A real claude job given the task server; returns its final message."""
        completed = self.cli(["run", "claude", "--mcp-config", MCP_CONFIG, "-p", prompt], self.project,
                             timeout=timeout)
        out = text(completed.stdout)
        require(completed.returncode == 0, f"claude job failed ({completed.returncode}): "
                                           f"{out[-800:]!r} {text(completed.stderr)[-1200:]!r}")
        return out

    def notes(self) -> list[str]:
        return (self.project / "notes.txt").read_text().splitlines()

    def child_of_top_claude(self, before: set[str], command: str) -> dict[str, Any]:
        fresh = self.settle(before, 2, timeout=180, exact=False)
        tops = [r for r in fresh if r["command"] == "claude" and "session:" in json.dumps(self.lin(r)["parent"])]
        require(len(tops) == 1, f"want one top-level claude job: {[(r['command'], self.lin(r)) for r in fresh]}")
        children = [r for r in fresh if f"job:{tops[0]['job_id']}" == self.lin(r)["parent"]
                    and r["command"] == command]
        require(children, f"no {command} child under the claude job: "
                          f"{[(r['command'], self.lin(r)['parent']) for r in fresh]}"[:900])
        return children[0]

    # ---- scenarios ---------------------------------------------------------
    def protocol_and_refusals(self) -> None:
        """T01: tools/list carries the contract; refusals are tool errors that say why."""
        out = self.drive([["task", {"description": "x", "prompt": "x", "agent": "nope"}],
                          ["task", {"description": "x", "prompt": "x"}],
                          ["task", {"prompt": "x", "agent": "claude", "files": "fork"}],
                          ["task_output", {"id": "t9"}]])
        require(out["init"]["serverInfo"]["name"] == "marsh" and out["exit"] == 0, f"init/exit: {out!r}"[:600])
        tools = {tool["name"]: tool for tool in out["tools"]}
        require(set(tools) == {"task", "task_output", "task_apply"}, f"tools: {sorted(tools)}")
        description = tools["task"]["description"]
        require(description.startswith("Hand work to another agent. Fast and cheap: use it whenever you would "
                                        "hand work off") and "Agents: claude, codex." in description,
                f"task description: {description!r}")
        properties = tools["task"]["inputSchema"]["properties"]
        require(set(properties) == {"description", "prompt", "agent", "files", "background"}
                and properties["files"]["enum"] == ["shared", "copy"]
                and properties["agent"]["enum"] == ["claude", "codex"], f"task schema: {properties!r}"[:800])
        texts = [(call.get("isError"), call["content"][0]["text"]) for call in out["calls"]]
        require(texts[0] == (True, "`nope` is not an agent this job may start; available: claude, codex"),
                f"unknown agent: {texts[0]!r}")
        require(texts[1][0] and "set `agent`: you (shell) are not one this job may start" in texts[1][1],
                f"no default agent for a shell job: {texts[1]!r}")
        require(texts[2][0] and '"shared" or "copy"' in texts[2][1], f"bad files: {texts[2]!r}")
        require(texts[3][0] and texts[3][1].startswith("no task t9"), f"unknown id: {texts[3]!r}")
        narrowed = self.drive([["task", {"prompt": "x", "agent": "claude"}]], prefix="MARSH_SPAWN=codex ")
        call = narrowed["calls"][0]
        require(call.get("isError") and "available: codex" in call["content"][0]["text"]
                and "Agents: codex." in narrowed["tools"][0]["description"],
                f"MARSH_SPAWN narrowing: {narrowed!r}"[:800])
        status = self.document("status", "--json")
        require("task-tool-v1" in status.get("features", []), f"status features: {status.get('features')}")

    def live_shared(self) -> None:
        """T02: a claude job's `task` (default agent, shared files) edits the caller's folder; the child is its job."""
        before = self.job_ids()
        out = self.claude(
            "Call the mcp__marsh__task tool exactly once, with description \"append gamma\" and prompt "
            "\"Append one line containing exactly gamma to notes.txt in the current directory, then reply "
            "with exactly DONE-GAMMA.\" Do not edit any file yourself. Then reply with the tool's result "
            "text, verbatim.")
        require("DONE-GAMMA" in out and "files shared) exited 0" in out, f"task result: {out[-1500:]!r}")
        require(self.notes() == ["alpha", "beta", "gamma"], f"notes.txt: {self.notes()}")
        child = self.child_of_top_claude(before, "claude")
        require(child["job_id"] in out, f"the result does not name the child job {child['job_id']}: {out[-800:]!r}")
        self.git(self.project, "checkout", "-q", "notes.txt")

    def live_copy_apply(self) -> None:
        """T03: `files: "copy"` returns a diff and leaves the folder alone until `task_apply` applies it."""
        before = self.job_ids()
        out = self.claude(
            "Step 1: call the mcp__marsh__task tool with description \"append delta\", files \"copy\" and "
            "prompt \"Append one line containing exactly delta to notes.txt in the current directory, then "
            "reply with exactly DONE-DELTA.\". Step 2: run `wc -l < notes.txt` with your Bash tool and "
            "write NOTES-BEFORE-APPLY=<the number it printed>. Step 3: call mcp__marsh__task_apply with the "
            "task id from step 1. Do not edit any file yourself. Finally reply with the step 1 and step 3 "
            "tool results, verbatim.")
        require("DONE-DELTA" in out and "+delta" in out and "files copy) exited 0" in out,
                f"copy result: {out[-2000:]!r}")
        require(re.search(r"NOTES-BEFORE-APPLY=\s*2\b", out), f"the folder changed before task_apply: {out[-1500:]!r}")
        require("apply clean" in out, f"task_apply result: {out[-1200:]!r}")
        require(self.notes() == ["alpha", "beta", "delta"], f"notes.txt after apply: {self.notes()}")
        child = self.child_of_top_claude(before, "claude")
        require(self.lin(child).get("split"), f"the copy task's job is not a split branch: {self.lin(child)}")
        self.git(self.project, "checkout", "-q", "notes.txt")

    def live_background(self) -> None:
        """T04: `background: true` returns an id at once; `task_output(wait)` returns the result (codex child)."""
        before = self.job_ids()
        out = self.claude(
            "Call the mcp__marsh__task tool with description \"count lines\", agent \"codex\", background "
            "true and prompt \"Reply with only the number of lines in notes.txt.\". Report its result text "
            "after the words STARTED-TEXT. Then call mcp__marsh__task_output with that task id and wait "
            "true, and reply with its result text verbatim after the words OUTPUT-TEXT.")
        started = out.split("STARTED-TEXT", 1)[-1].split("OUTPUT-TEXT", 1)[0]
        require("started in the background" in started, f"background start: {out[-1500:]!r}")
        output = out.split("OUTPUT-TEXT", 1)[-1]
        require("(codex, \"count lines\", files shared) exited 0" in output and "2" in output.split(
            "final message", 1)[-1], f"task_output: {output[-1200:]!r}")
        self.child_of_top_claude(before, "codex")

    def run_all(self) -> None:
        self.stock_before = stock_baseline(self.sbx)
        self.source["stock_before"] = self.stock_before
        (self.project / "notes.txt").write_text("alpha\nbeta\n")
        self.git(self.project, "init", "-q", "-b", "main")
        self.git(self.project, "add", "-A")
        self.git(self.project, "commit", "-q", "-m", "seed")
        self.document("status", "--json")
        self.run([self.marsh, "--load", "shell", "-c", "true"], timeout=900)
        if LIVE & set(self.selected):
            self.run([self.marsh, "--load", "claude,codex", "-c", "true"], timeout=1800)
        self.document("status", "--json")
        self.run_scenarios()


def main() -> int:
    parser = scenario_parser(
        __doc__, SCENARIOS, preflight=False,
        kit_help="fixture Kit: immutable OCI ref, native v3 source dir, or a commands.json whose \"fixture\" key "
                 "holds the ref (registered as the harness requires; the scenarios use claude, codex and shell)")
    parser.add_argument("--live", action="store_true",
                        help="also run the billed scenarios (T02-T04: real claude and codex turns)")
    return suite_main(TaskTool, parser)


if __name__ == "__main__":
    raise SystemExit(main())
