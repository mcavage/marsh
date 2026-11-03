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
import os
import pathlib
import re
import shlex
import signal
import sys
import time
from typing import Any

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from harness import ScenarioSuite, require, scenario_parser, suite_main, text  # noqa: E402
from provenance import stock_baseline  # noqa: E402

SCENARIOS = {
    "T01-protocol-and-refusals": "protocol_and_refusals",
    "T02-live-shared": "live_shared",
    "T03-live-copy-apply": "live_copy_apply",
    "T04-live-background": "live_background",
    "T05-spawn-narrowing-grandchild": "spawn_narrowing",
    "T06-admission-refusal": "admission_refusal",
    "T07-cross-job-ids": "cross_job_ids",
    "T08-cancel-and-reap": "cancel_and_reap",
    "T09-symlink-safe-copy": "symlink_safe_copy",
    "T10-conflicts-keep-edits": "conflicts_keep_edits",
    "T11-fan-out-limit": "fan_out_limit",
    "T12-peer-review-context-and-model": "peer_review",
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


# A scripted MCP client for the fixture agent (no model): run as
# `fixture exec python3 -c STEP_CLIENT STEPS` in a fixture job, it drives
# `marsh task-mcp` through STEPS:
#   ["call", NAME, ARGS]                     a tools/call; its result is recorded
#   ["cancel", NAME, ARGS, SECONDS]          a tools/call cancelled after SECONDS;
#                                            records whether a response came anyway
#   ["sh", SCRIPT]                           a shell step in the caller's folder
#   ["eof"]                                  close the server's stdin; record how long it took to exit
# Every task prompt is a shell script: the fixture's prompt verbs are
# `exec sh -c PROMPT`.
STEP_CLIENT = r"""
import json, subprocess, sys, threading, time
server = subprocess.Popen(["marsh", "task-mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
replies, lock = {}, threading.Condition()
def reader():
    for line in server.stdout:
        message = json.loads(line)
        if "id" in message:
            with lock:
                replies[message["id"]] = message
                lock.notify_all()
threading.Thread(target=reader, daemon=True).start()
def send(message):
    server.stdin.write(json.dumps(message) + "\n")
    server.stdin.flush()
def reply(n, timeout):
    deadline = time.time() + timeout
    with lock:
        while n not in replies and time.time() < deadline:
            lock.wait(deadline - time.time())
        return replies.get(n)
send({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18",
      "capabilities": {}, "clientInfo": {"name": "uat", "version": "0"}}})
reply(0, 30)
out = []
for n, step in enumerate(json.loads(sys.argv[1]), 1):
    if step[0] == "call":
        send({"jsonrpc": "2.0", "id": n, "method": "tools/call", "params": {"name": step[1], "arguments": step[2]}})
        got = reply(n, 600)
        out.append(got and got.get("result"))
    elif step[0] == "cancel":
        send({"jsonrpc": "2.0", "id": n, "method": "tools/call", "params": {"name": step[1], "arguments": step[2]}})
        time.sleep(step[3])
        send({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": n, "reason": "uat"}})
        out.append({"responded": reply(n, 20) is not None})
    elif step[0] == "sh":
        done = subprocess.run(["sh", "-c", step[1]], capture_output=True, text=True)
        out.append({"status": done.returncode, "stdout": done.stdout, "stderr": done.stderr})
    elif step[0] == "eof":
        began = time.time()
        server.stdin.close()
        out.append({"exit": server.wait(timeout=120), "seconds": round(time.time() - began, 1)})
if not server.stdin.closed:
    server.stdin.close()
    server.wait(timeout=120)
print("@@" + json.dumps(out))
"""


def stage_fixture(destination: pathlib.Path) -> pathlib.Path:
    """This checkout's fixture Kit with the canonical inputs publication adds
    (scripts/publish-kits.py), as a source Kit marsh builds itself."""
    import importlib.util
    root = pathlib.Path(__file__).resolve().parents[2]
    spec = importlib.util.spec_from_file_location("marsh_publish_kits", root / "scripts/publish-kits.py")
    kits = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(kits)
    source = root / "tests/acceptance/fixture"
    expanded, _ = kits.canonical_inputs(root, {source}, {})
    budget = [kits.STAGE_LIMIT]
    for relative in kits.git_files(source):
        kits.copy_regular(source / relative, destination / relative, budget)
    for relative, origin in expanded[source]["files"].items():
        kits.copy_regular(pathlib.Path(origin), destination / relative, budget)
    return destination


def text_of(result: Any) -> str:
    return (result or {}).get("content", [{}])[0].get("text", "")


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
        if arguments.stage_fixture:
            mapping["fixture"] = str(stage_fixture(self.root / "fixture-kit"))
        registry.write_text(json.dumps(mapping, sort_keys=True) + "\n", encoding="utf-8")

    def drive(self, calls: list[list[Any]], *, prefix: str = "") -> dict[str, Any]:
        """One `shell` job: an MCP client making `calls` to `marsh task-mcp`."""
        script = f"{prefix}python3 -c {shlex.quote(CLIENT)} {shlex.quote(json.dumps(calls))}"
        completed = self.cli(["run", "shell", "-c", script], self.project, timeout=600)
        out = text(completed.stdout)
        require(completed.returncode == 0 and "@@" in out,
                f"MCP client job failed ({completed.returncode}): {out[-600:]!r} {text(completed.stderr)[-800:]!r}")
        return json.loads(out.split("@@", 1)[1])

    def steps(self, steps: list[list[Any]], *, env: str = "", nest: bool = False,
              timeout: float = 900) -> list[Any]:
        """A fixture job running STEP_CLIENT (with `nest`, as a fixture job's child)."""
        client = ["exec", "sh", "-c", f"{env}exec python3 -c {shlex.quote(STEP_CLIENT)} "
                                      f"{shlex.quote(json.dumps(steps))}"]
        argv = ["run", "fixture", *client]
        if nest:
            argv = ["run", "fixture", "exec", "sh", "-c", shlex.join(["fixture", *client]) + " </dev/null"]
        completed = self.cli(argv, self.project, timeout=timeout)
        out = text(completed.stdout)
        require(completed.returncode == 0 and "@@" in out,
                f"fixture MCP client failed ({completed.returncode}): {out[-800:]!r} {text(completed.stderr)[-1200:]!r}")
        return json.loads(out.split("@@", 1)[1])

    def job_state(self, job: str, timeout: float = 120) -> dict[str, Any]:
        """A job's receipt once it is terminal with its cleanup settled."""
        deadline = time.monotonic() + timeout
        while True:
            receipt = self.show(job)
            if receipt.get("state") not in ("running", "queued", "submitted") and receipt.get("cleanup") != "pending":
                return receipt
            require(time.monotonic() < deadline, f"job {job} did not end: {receipt.get('state')}")
            time.sleep(0.5)

    @staticmethod
    def job_of(result: Any) -> str:
        found = re.search(r"job ([0-9a-f-]{36})", text_of(result))
        require(found, f"the result names no job: {text_of(result)[-600:]!r}")
        return found.group(1)

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
        # Every registered command with prompt verbs is an agent here.
        agents = sorted(command["name"] for command in self.document("status", "--json")["commands"]
                        if (command.get("sessions") or {}).get("prompt"))
        require({"claude", "codex", "fixture"} <= set(agents), f"agents with prompt verbs: {agents}")
        require(set(tools) == {"task", "task_output", "task_apply"}, f"tools: {sorted(tools)}")
        description = tools["task"]["description"]
        require(description.startswith("Hand work to another agent. Fast and cheap: use it whenever you would "
                                        "hand work off") and f"Agents: {', '.join(agents)}." in description,
                f"task description: {description!r}")
        properties = tools["task"]["inputSchema"]["properties"]
        require(set(properties) == {"description", "prompt", "agent", "model", "files", "background"}
                and properties["files"]["enum"] == ["shared", "copy"]
                and properties["agent"]["enum"] == agents, f"task schema: {properties!r}"[:800])
        texts = [(call.get("isError"), call["content"][0]["text"]) for call in out["calls"]]
        require(texts[0] == (True, f"`nope` is not an agent this job may start; available: {', '.join(agents)}"),
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

    def spawn_narrowing(self) -> None:
        """T05: MARSH_SPAWN narrowing reaches a task's child in shared and copy mode: its grandchild is refused."""
        script = "shell -c 'echo GRANDCHILD' </dev/null; echo rc=$?"
        out = self.steps([["call", "task", {"description": "try a grandchild", "prompt": script}],
                          ["call", "task", {"description": "try a grandchild", "prompt": script, "files": "copy"}]],
                         env="MARSH_SPAWN=fixture ")
        for result, mode in zip(out, ("shared", "copy")):
            body = text_of(result)
            require("rc=126" in body and "GRANDCHILD" not in body.split("final message", 1)[-1],
                    f"{mode}: the child's grandchild was not refused by the narrowed spawn set: {body[-900:]!r}")
            child = self.job_state(self.job_of(result))
            require(self.lin(child).get("spawn") == ["fixture"],
                    f"{mode}: the child's recorded spawn set is not the caller's narrowed one: {self.lin(child)}")
            require(child.get("label") == "task: try a grandchild", f"{mode}: job label: {child.get('label')!r}")
            require(not [r for r in (self.show(j) for j in self.job_ids())
                         if r["command"] == "shell" and f"job:{child['job_id']}" == self.lin(r, strict=False).get("parent")],
                    f"{mode}: a shell grandchild was started")

    def admission_refusal(self) -> None:
        """T06: admission refusals (same-Kit chain; depth, shared and copy) are tool errors with marsh's reason."""
        out = self.steps([["call", "task", {"description": "too deep", "prompt": "echo SHOULD-NOT-RUN"}]], nest=True)
        body = text_of(out[0])
        require(out[0].get("isError") and "same-Kit chain limit" in body and "SHOULD-NOT-RUN" not in body,
                f"the third fixture in a row was not refused as a tool error: {body[-900:]!r}")
        # A split's argv branch is not a same-Kit step (processes.md s6), so a
        # copy task meets the depth limit: a `shell` caller at depth 4.
        steps = [["call", "task", {"description": "too deep", "prompt": "echo NO", "agent": "fixture"}],
                 ["call", "task", {"description": "too deep", "prompt": "echo NO", "agent": "fixture",
                                   "files": "copy"}]]
        caller = ["shell", "-c", f"exec python3 -c {shlex.quote(STEP_CLIENT)} {shlex.quote(json.dumps(steps))}"]
        depth3 = ["fixture", "exec", "sh", "-c", shlex.join(caller) + " </dev/null"]
        depth2 = ["shell", "-c", shlex.join(depth3) + " </dev/null"]
        completed = self.cli(["run", "fixture", "exec", "sh", "-c", shlex.join(depth2) + " </dev/null"],
                             self.project, timeout=900)
        output = text(completed.stdout)
        require(completed.returncode == 0 and "@@" in output,
                f"depth-4 caller failed ({completed.returncode}): {output[-600:]!r} {text(completed.stderr)[-900:]!r}")
        for result, mode in zip(json.loads(output.split("@@", 1)[1]), ("shared", "copy")):
            body = text_of(result)
            require(result.get("isError") and "depth limit 4" in body, f"{mode}: depth 5 was not refused: {body[-900:]!r}")

    def cross_job_ids(self) -> None:
        """T07: a task id belongs to its own job's server: another job's server refuses it."""
        first = self.steps([["call", "task", {"description": "make a copy", "prompt": "echo x >> notes.txt; echo OK",
                                              "files": "copy"}]])
        require("files copy) exited 0" in text_of(first[0]), f"copy task: {text_of(first[0])[-600:]!r}")
        second = self.steps([["call", "task_output", {"id": "t1"}], ["call", "task_apply", {"id": "t1"}]])
        for result in second:
            require(result.get("isError") and text_of(result).startswith("no task t1"),
                    f"another job's task id was accepted: {text_of(result)!r}")
        require(self.notes() == ["alpha", "beta"], f"notes.txt changed: {self.notes()}")

    def cancel_and_reap(self) -> None:
        """T08: cancelling a call stops its child or its wait; EOF and the caller's end stop and reap every task."""
        out = self.steps([["cancel", "task", {"description": "blocking sleeper", "prompt": "sleep 300"}, 4],
                          ["call", "task", {"description": "background sleeper", "prompt": "sleep 300",
                                            "background": True}],
                          ["cancel", "task_output", {"id": "t2", "wait": True}, 2],
                          ["call", "task_output", {"id": "t2"}],
                          ["eof"]])
        require(out[0] == {"responded": False}, f"a cancelled task call got a response: {out[0]}")
        require(out[2] == {"responded": False}, f"a cancelled task_output wait got a response: {out[2]}")
        require("is running" in text_of(out[3]), f"the background task stopped with the wait: {text_of(out[3])!r}")
        require(out[4]["exit"] == 0 and out[4]["seconds"] < 30, f"the server did not exit promptly: {out[4]}")
        sleepers = [r for r in (self.show(j) for j in self.job_ids()) if r.get("label") in
                    ("task: blocking sleeper", "task: background sleeper")]
        require(len(sleepers) == 2, f"want both sleeper jobs: {[r.get('label') for r in sleepers]}")
        for receipt in sleepers:
            settled = self.job_state(receipt["job_id"])
            require(settled["state"] == "cancelled" and settled["cleanup"] == "verified",
                    f"{settled.get('label')}: not cancelled and cleaned up: {settled['state']} {settled['cleanup']}")
        # The caller's own end (its client killed) cancels a task still running.
        before = self.job_ids()
        caller = self.spawn([self.marsh, "run", "fixture", "exec", "sh", "-c",
                             f"exec python3 -c {shlex.quote(STEP_CLIENT)} " + shlex.quote(json.dumps(
                                 [["call", "task", {"description": "orphan sleeper", "prompt": "sleep 300",
                                                    "background": True}], ["sh", "sleep 600"]]))], self.project)
        self.wait_until(lambda: any(self.show(j).get("label") == "task: orphan sleeper"
                                    and self.show(j).get("state") == "running" for j in self.job_ids() - before),
                        120, "the orphan sleeper to run", (caller,))
        os.killpg(caller.pid, signal.SIGTERM)
        self.reap(caller, 60)
        for job in self.job_ids() - before:
            settled = self.job_state(job)
            require(settled["state"] in ("cancelled", "failed") and settled["cleanup"] == "verified",
                    f"{settled['command']} {settled.get('label')}: survived its caller: {settled['state']}")

    def symlink_safe_copy(self) -> None:
        """T09: a copy task's links stay links: apply makes the link, never writes through one."""
        outside = self.root / "outside.txt"
        outside.write_text("untouched\n")
        (self.project / "outlink").symlink_to(outside)
        self.git(self.project, "add", "-A")
        self.git(self.project, "commit", "-q", "-m", "outlink")
        script = ("ln -s /etc/hostname evil && (echo pwned > outlink) 2>/dev/null; "
                  "echo changed >> notes.txt; echo DONE")
        out = self.steps([["call", "task", {"description": "links", "prompt": script, "files": "copy"}],
                          ["call", "task_apply", {"id": "t1"}]])
        require("files copy) exited 0" in text_of(out[0]), f"copy task: {text_of(out[0])[-800:]!r}")
        require(not out[1].get("isError") and "apply clean" in text_of(out[1]), f"apply: {text_of(out[1])!r}")
        evil = self.project / "evil"
        require(evil.is_symlink() and os.readlink(evil) == "/etc/hostname", "the applied link is not a link")
        require(outside.read_text() == "untouched\n", "a write through the project's link reached outside it")
        require(self.notes() == ["alpha", "beta", "changed"], f"notes.txt: {self.notes()}")
        evil.unlink()
        self.git(self.project, "rm", "-q", "outlink")
        self.git(self.project, "commit", "-q", "-m", "drop outlink")
        self.git(self.project, "checkout", "-q", "notes.txt")

    def conflicts_keep_edits(self) -> None:
        """T10: task_apply never overwrites the caller's concurrent edits: unstaged it refuses, staged it names conflicts."""
        task = {"description": "upper alpha", "prompt": "sed -i s/alpha/ALPHA/ notes.txt; echo DONE", "files": "copy"}
        out = self.steps([["call", "task", task],
                          ["sh", "sed -i s/alpha/alpha-local/ notes.txt"],
                          ["call", "task_apply", {"id": "t1"}],
                          ["sh", "cat notes.txt"],
                          ["sh", "git add notes.txt"],
                          ["call", "task_apply", {"id": "t1"}],
                          ["sh", "cat notes.txt"]])
        require(out[2].get("isError") and "apply failed" in text_of(out[2]), f"unstaged apply: {text_of(out[2])!r}")
        require(out[3]["stdout"] == "alpha-local\nbeta\n", f"the refused apply changed the file: {out[3]}")
        require(not out[5].get("isError") and "apply conflicts" in text_of(out[5]) and "notes.txt" in text_of(out[5]),
                f"staged apply: {text_of(out[5])!r}")
        merged = out[6]["stdout"]
        require("alpha-local" in merged and "ALPHA" in merged and "<<<<<<<" in merged,
                f"the conflict lost an edit: {merged!r}")
        self.git(self.project, "reset", "-q", "--hard")

    def fan_out_limit(self) -> None:
        """T11: 8 live tasks: a 9th is refused (shared and copy), the caller keeps working, every result returns."""
        workers = [["call", "task", {"description": f"worker {n}", "prompt": f"sleep 8; echo RESULT-{n}",
                                     "background": True}] for n in range(1, 9)]
        out = self.steps([*workers, ["sh", "sleep 4"],
                          ["call", "task", {"description": "ninth shared", "prompt": "echo NO"}],
                          ["call", "task", {"description": "ninth copy", "prompt": "echo NO", "files": "copy"}],
                          *[["call", "task_output", {"id": f"t{n}", "wait": True}] for n in range(1, 9)],
                          ["sh", "sleep 3"],
                          ["call", "task", {"description": "after the limit", "prompt": "echo AFTER"}],
                          # Eight more, cancelled while they start (EOF at once).
                          *[["call", "task", {"description": f"early {n}", "prompt": "sleep 120",
                                              "background": True}] for n in range(8)],
                          ["eof"]])
        for mode, result in (("shared", out[9]), ("copy", out[10])):
            require(result.get("isError") and "fan-out limit 8" in text_of(result) and "uncertain" not in text_of(result),
                    f"{mode}: the 9th task was not refused cleanly: {text_of(result)[-600:]!r}")
        for n, result in enumerate(out[11:19], 1):
            require(f"exited 0" in text_of(result) and f"RESULT-{n}" in text_of(result),
                    f"t{n}'s result was lost: {text_of(result)[-600:]!r}")
        require("AFTER" in text_of(out[20]), f"the caller could not start a task after the limit: {text_of(out[20])!r}")
        require(out[-1]["exit"] == 0, f"the server did not exit: {out[-1]}")
        tasks = [r for r in (self.show(j) for j in self.job_ids()) if (r.get("label") or "").startswith(
            ("task: worker", "task: early", "task: after the limit"))]
        require(len(tasks) == 17, f"want 17 task jobs: {[r.get('label') for r in tasks]}")
        for receipt in tasks:
            settled = self.job_state(receipt["job_id"])
            require(settled["cleanup"] == "verified", f"{settled.get('label')}: cleanup {settled['cleanup']}: "
                    + json.dumps({k: settled.get(k) for k in ("state", "exit", "execution", "timing", "container_id")})[:1500])
        status = self.document("status", "--json")
        require(all(w.get("health") == "ready" for w in status.get("workers", [])),
                f"a worker was quarantined: {status.get('workers')}")

    def context_md(self, spawn: list[str] = ()) -> str:
        """A fixture job's /run/marsh/context.md (with `spawn`, started with that spawn set)."""
        narrow = ["--spawn", ",".join(spawn)] if spawn else []
        completed = self.cli(["run", *narrow, "fixture", "exec", "cat", "/run/marsh/context.md"],
                             self.project, timeout=600)
        require(completed.returncode == 0, f"context job failed: {text(completed.stderr)[-800:]!r}")
        return text(completed.stdout)

    def peer_review(self) -> None:
        """T12: the peer-review section appears only while it is on and lists only ready reviewers
        this job may start; `task(model)` reaches the agent's model option and the child's receipt."""
        dropins = self.home / "peer-review.d"
        dropins.mkdir(exist_ok=True)
        heading = "## Second opinion"
        require(heading not in self.context_md(), "the section is in context.md with no drop-in")
        status = self.document("status", "--json")
        require("peer-review-v1" in status.get("features", []), f"features: {status.get('features')}")
        require(status["peer_review"]["enabled"] is False, f"peer_review: {status.get('peer_review')}")
        # Named, but not ready: shell has no prompt verbs; nope is not registered.
        (dropins / "narwhal.yaml").write_text(
            "enabled: true\nreviewers:\n"
            "  - {agent: fixture, model: gpt-6.1-terra}\n"
            "  - {agent: claude, model: claude-sonnet-5-5}\n"
            "  - {agent: shell, model: gpt-6-luna}\n"
            "  - {agent: nope, model: gpt-6-astra}\n")
        (dropins / "broken.yaml").write_text("enabled: maybe\n")
        listed = self.document("peer-review", "ls", "--json")
        ready = [(r["class"], r["vendor"], r["agent"], r["model"]) for r in listed["reviewers"]]
        require(listed["enabled"] and ready == [("mid", "anthropic", "claude", "claude-sonnet-5-5"),
                                               ("mid", "openai", "fixture", "gpt-6.1-terra")],
                f"peer-review ls: {listed!r}"[:1200])
        reasons = {r["agent"]: r["reason"] for r in listed["skipped"]}
        require("no prompt verbs" in reasons.get("shell", "") and "not a registered command" in reasons.get("nope", ""),
                f"skipped: {listed['skipped']!r}"[:900])
        require(any("broken.yaml" in error for error in listed["errors"]), f"errors: {listed['errors']}")
        on = self.context_md()
        section = on.split(heading, 1)[-1]
        require(heading in on and '| mid | openai | agent: "fixture", model: "gpt-6.1-terra" |' in section
                and '| mid | anthropic | agent: "claude", model: "claude-sonnet-5-5" |' in section
                and "shell" not in section and "nope" not in section,
                f"context while on: {section[:1500]!r}")
        narrowed = self.context_md(["fixture"]).split(heading, 1)[-1]
        require("fixture" in narrowed and '"claude"' not in narrowed, f"MARSH_SPAWN=fixture: {narrowed[:900]!r}")
        (self.evidence / "peer-review-context.md").write_text(heading + section)
        nested = self.cli(["run", "fixture", "exec", "sh", "-c", "fixture exec cat /run/marsh/context.md </dev/null"],
                          self.project, timeout=600)
        require(nested.returncode == 0 and "## Hand work off" in text(nested.stdout)
                and heading not in text(nested.stdout), f"a child job got the section: {text(nested.stdout)[-900:]!r}")
        # task(model): the fixture's model option lands in the prompt's $0 and $1.
        out = self.steps([["call", "task", {"description": "review by fixture", "agent": "fixture",
                                            "model": "gpt-6.1-terra", "prompt": 'echo "opt=$0 model=$1"'}]])
        body = text_of(out[0])
        require("opt=--model model=gpt-6.1-terra" in body and "model gpt-6.1-terra" in body, f"task(model): {body[-900:]!r}")
        child = self.job_state(self.job_of(out[0]))
        require(child.get("model") == "gpt-6.1-terra" and child.get("label") == "task: review by fixture",
                f"child receipt: model {child.get('model')!r} label {child.get('label')!r}")
        listed_jobs = self.document("jobs", "--json")
        require(any(j.get("job_id") == child["job_id"] and j.get("model") == "gpt-6.1-terra"
                    for j in listed_jobs.get("jobs", [])), "marsh jobs --json lacks the child's model")
        # Off anywhere is off.
        (dropins / "marsh.yaml").write_text("enabled: false\n")
        require(heading not in self.context_md(), "the section is in context.md after enabled: false")
        status = self.document("status", "--json")["peer_review"]
        require(not status["enabled"] and status["disabled_by"] == [str(dropins / "marsh.yaml")],
                f"peer_review after off: {status!r}"[:900])
        for name in ("narwhal.yaml", "broken.yaml", "marsh.yaml"):
            (dropins / name).unlink()

    def run_all(self) -> None:
        self.stock_before = stock_baseline(self.sbx)
        self.source["stock_before"] = self.stock_before
        (self.project / "notes.txt").write_text("alpha\nbeta\n")
        self.git(self.project, "init", "-q", "-b", "main")
        self.git(self.project, "add", "-A")
        self.git(self.project, "commit", "-q", "-m", "seed")
        self.document("status", "--json")
        self.run([self.marsh, "--load", "fixture,shell", "-c", "true"], timeout=1800)
        sessions = {command["name"]: command.get("sessions") for command in
                    self.document("status", "--json").get("commands", [])}
        require((sessions.get("fixture") or {}).get("prompt"),
                "the fixture Kit declares no agent-sessions@1 prompt verbs; pass --stage-fixture "
                "or republish the fixture from this checkout")
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
    parser.add_argument("--stage-fixture", action="store_true",
                        help="register this checkout's fixture Kit source (with its prompt verbs) instead of --kit")
    return suite_main(TaskTool, parser)


if __name__ == "__main__":
    raise SystemExit(main())
