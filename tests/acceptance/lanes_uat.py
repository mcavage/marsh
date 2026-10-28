#!/usr/bin/env python3
"""Stock-SBX open lanes: a checkpoint, `split --open --from`, ACP lane jobs
confined to their forks (also through the job capability socket),
creator-only admission, close and capture, apply, cancel (also of a lane job
still being started), a close that times out on a live lane job and the
removal it refuses, a restart during a close, storage, and the events stream
(`docs/design/workspaces.md` s14)."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import threading
import time
import traceback

from harness import AcpScope as Scope
from provenance import source_identity


class Acp:
    """`marsh run --acp fixture-acp` as an ACP client sees it: stdio JSON-RPC."""

    def __init__(self, scope: Scope, cwd: Path) -> None:
        self.process = subprocess.Popen(
            [scope.marsh, "run", "--acp", "fixture-acp"], cwd=cwd, env=scope.environment,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        self.next = 0

    def call(self, method: str, params: dict, timeout: float = 600) -> tuple[dict, list[str]]:
        self.next += 1
        self.process.stdin.write((json.dumps({"jsonrpc": "2.0", "id": self.next, "method": method,
                                              "params": params}) + "\n").encode())
        self.process.stdin.flush()
        texts = []
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            line = self.process.stdout.readline()
            assert line, f"agent ended: {self.process.stderr.read().decode(errors='replace')}"
            message = json.loads(line)  # stdout carries JSON-RPC only
            update = message.get("params", {}).get("update", {})
            if update.get("sessionUpdate") == "agent_message_chunk":
                texts.append(update["content"]["text"])
            if message.get("id") == self.next and "method" not in message:
                assert "result" in message, message
                return message["result"], texts
        raise TimeoutError(method)

    def session(self, cwd: Path) -> str:
        self.call("initialize", {"protocolVersion": 1, "clientCapabilities": {}})
        return self.call("session/new", {"cwd": str(cwd), "mcpServers": []})[0]["sessionId"]

    def prompt(self, session: str, text: str) -> list[str]:
        return self.call("session/prompt", {"sessionId": session,
                                            "prompt": [{"type": "text", "text": text}]})[1]

    def finish(self, timeout: float = 120) -> int:
        self.process.stdin.close()
        return self.process.wait(timeout=timeout)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--marsh", required=True)
    parser.add_argument("--sbx", required=True)
    parser.add_argument("--guest-artifacts", required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--source-tree", type=Path, required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--build-receipt", default=None, help="optional observed-build receipt")
    parser.add_argument("--kit", required=True, help="the published acceptance fixture Kit reference")
    args = parser.parse_args()
    args.evidence.mkdir(parents=True, mode=0o700, exist_ok=True)
    report = {"schema": "marsh.lanes-uat/v1", "outcome": "failed",
              "source_identity": source_identity(args.source_tree.resolve(strict=True), args.source_revision)}
    scope = Scope(args)
    commands = json.loads((scope.control_home / "commands.json").read_text())
    (scope.control_home / "commands.json").write_text(json.dumps({**commands, "fixture": args.kit}) + "\n")
    project = scope.project
    events: list[dict] = []
    subscriber = None

    def host(*words: str, cwd: Path = project, timeout: float = 600) -> subprocess.CompletedProcess[bytes]:
        return subprocess.run([scope.marsh, *words], cwd=cwd, env=scope.environment, stdin=subprocess.DEVNULL,
                              capture_output=True, timeout=timeout, start_new_session=True)

    def json_of(*words: str, cwd: Path = project) -> dict:
        result = host(*words, cwd=cwd)
        assert result.returncode == 0, (words, result.stderr.decode(errors="replace"))
        return json.loads(result.stdout)

    def background(*words: str, cwd: Path) -> subprocess.Popen:
        return subprocess.Popen([scope.marsh, *words], cwd=cwd, env=scope.environment, stdin=subprocess.DEVNULL,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)

    def wait_state(split: str, state: str, timeout: float = 180) -> dict:
        deadline = time.monotonic() + timeout
        while True:
            shown = json_of("splits", "--json", split)["splits"][0]
            if shown["state"] == state:
                return shown
            assert time.monotonic() < deadline, shown
            time.sleep(1)

    def git(*words: str) -> str:
        return subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@x", "-c", "commit.gpgsign=false",
                               *words], cwd=project, check=True, capture_output=True, text=True).stdout

    try:
        status = scope.public_status()
        listed = {command["name"]: command for command in status["commands"]}
        assert listed["fixture-acp"]["acp"] is True and listed["fixture"]["acp"] is False, status["commands"]
        assert {"events-v1", "lanes-v1", "checkpoints-v1", "acp-descriptors-v1"} <= set(status["features"])
        subscriber = subprocess.Popen([scope.marsh, "events", "--json"], cwd=project, env=scope.environment,
                                      stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, start_new_session=True)
        threading.Thread(target=lambda: events.extend(json.loads(line) for line in subscriber.stdout),
                         daemon=True).start()

        (project / "README").write_text("before\n")
        git("init", "-q", "-b", "main")
        git("add", "README")
        git("commit", "-q", "-m", "init")
        checkpoint = json_of("checkpoint", "--label", "turn-1", "--json")["checkpoint"]
        (project / "README").write_text("after the checkpoint\n")
        opened = json_of("split", "--open", "--from", checkpoint, "a", "b")
        split = opened["id"]
        lane_a, lane_b = Path(opened["lanes"]["a"]), Path(opened["lanes"]["b"])
        assert (lane_a / "README").read_text() == "before\n"

        # An ACP lane job edits its fork and nothing else.
        agent = Acp(scope, lane_a)
        session = agent.session(lane_a)
        assert "fixture:wrote" in agent.prompt(session, "write lane.txt from-a")
        assert "fixture:read:before\n" in agent.prompt(session, f"read {lane_a / 'README'}")
        for probe in (f"read {project / 'README'}", f"write {project / 'escape.txt'} x",
                      f"write {lane_b / 'x.txt'} x", "write .git x",
                      f"read {project / '.marsh/checkpoints' / checkpoint / 'tree/README'}"):
            texts = agent.prompt(session, probe)
            assert any(text.startswith("fixture:refused:") for text in texts), (probe, texts)
        assert agent.finish() == 0
        # A second lane job in the same lane.
        second = Acp(scope, lane_a)
        assert "fixture:wrote" in second.prompt(second.session(lane_a), "write lane2.txt second")
        assert second.finish() == 0
        assert not (project / "escape.txt").exists()

        # Only the creator (the host) starts lane jobs: a shell session cannot.
        refused = host("-c", f"cd {lane_b} && marsh run fixture-acp </dev/null", timeout=900)
        assert refused.returncode != 0 and b"only its creator" in refused.stderr, refused.stderr

        closed = json_of("split", "close", split, "--json")
        assert closed["state"] == "kept" and closed["from"] == checkpoint, closed
        lanes = {lane["label"]: lane for lane in closed["branches"]}
        assert lanes["a"]["state"] == "captured" and lanes["a"]["files"] == 2, lanes
        assert lanes["b"]["state"] == "captured" and lanes["b"]["files"] == 0, lanes
        files = (Path(closed["out"]) / "a/files").read_text()
        assert files == "A\tlane.txt\nA\tlane2.txt\n", files
        late = Acp(scope, lane_a)
        assert late.process.wait(timeout=600) == 125
        assert b"closed" in late.process.stderr.read()
        applied = host("split", "apply", split, "a", "--3way", timeout=900)
        assert applied.returncode == 0, applied.stderr.decode(errors="replace")
        assert (project / "lane.txt").read_text() == "from-a"
        assert (project / "README").read_text() == "after the checkpoint\n"

        # Cancelling an open split ends its live lane jobs and removes it.
        open_c = json_of("split", "--open", "c")
        lane_c = Path(open_c["lanes"]["c"])
        held = Acp(scope, lane_c)
        held.session(lane_c)
        cancelled = host("splits", "cancel", open_c["id"])
        assert cancelled.returncode == 0, cancelled.stderr
        assert held.process.wait(timeout=120) != 0
        deadline = time.monotonic() + 120
        while True:
            shown = json_of("splits", "--json", open_c["id"])["splits"][0]
            if shown["state"] == "cancelled":
                break
            assert time.monotonic() < deadline, shown
            time.sleep(1)
        assert "every lane ended" in shown["reason"], shown
        assert not lane_c.exists()

        # A lane job still being started (its Kit VM is cold) is cancelled
        # with its split: it never runs.
        pending = json_of("split", "--open", "p")
        lane_p = Path(pending["lanes"]["p"])
        starting = background("run", "fixture", "project-write", "marker.txt", "x", cwd=lane_p)
        time.sleep(3)
        assert starting.poll() is None, "the Kit VM was already warm"
        assert host("splits", "cancel", pending["id"]).returncode == 0
        assert starting.wait(timeout=600) != 0
        shown = wait_state(pending["id"], "cancelled")
        assert "every lane ended" in shown["reason"], shown
        ran = [job for job in json_of("jobs", "--json")["jobs"]
               if job.get("parent") == f"split:{pending['id']}/p"]
        assert all(job["state"] != "finished" for job in ran), ran

        # A lane job cannot reach the project through its capability socket,
        # even from the project's own path.
        guarded = json_of("split", "--open", "g")
        lane_g = Path(guarded["lanes"]["g"])
        (project / "secret.txt").write_text("project only\n")
        escape = host("run", "fixture", "exec", "sh", "-c",
                      f"cd {project} && marsh split -n ::: x fixture project-read secret.txt; echo status=$?",
                      cwd=lane_g)
        assert b"a job in a lane cannot start a split" in escape.stderr, escape.stderr
        assert b"project only" not in escape.stdout, escape.stdout
        assert not list((project / ".marsh/split").glob("*/x")), "a split was made from a lane job"

        # A close that times out on a live lane job keeps the lane uncertain;
        # removal is refused until the job is ended.
        holder = background("run", "fixture", "hold", "600", cwd=lane_g)
        assert holder.stdout.readline().strip() == b"READY"
        timed_out = host("split", "close", guarded["id"], "--json")
        assert timed_out.returncode == 1, timed_out.stderr
        closed = json.loads(timed_out.stdout)
        assert closed["state"] == "kept" and closed["branches"][0]["state"] == "uncertain", closed
        refused = host("splits", "rm", guarded["id"])
        assert refused.returncode != 0 and b"running lane job" in refused.stderr, refused.stderr
        assert lane_g.exists()
        assert host("splits", "cancel", guarded["id"]).returncode == 0
        assert holder.wait(timeout=120) != 0
        deadline = time.monotonic() + 60
        while (removed := host("splits", "rm", guarded["id"])).returncode != 0:
            assert time.monotonic() < deadline, removed.stderr
            time.sleep(1)
        assert not lane_g.exists()

        # A restart while a close waits leaves the lane uncertain, its fork
        # in place, and admits nothing more.
        restarted = json_of("split", "--open", "r")
        lane_r = Path(restarted["lanes"]["r"])
        holder = background("run", "fixture", "hold", "600", cwd=lane_r)
        assert holder.stdout.readline().strip() == b"READY"
        closing = background("split", "close", restarted["id"], cwd=project)
        time.sleep(3)
        scope.public_status()
        daemon = scope.owned_daemon_pid
        subprocess.run(["kill", "-KILL", str(daemon)], check=True)
        closing.wait(timeout=60)
        # This scope's daemon died on purpose; the next request starts its
        # successor, which this scope then owns.
        scope.owned_daemon_id = scope.owned_daemon_pid = None
        scope.owned_daemon_process_identity = scope.owned_daemon_control_token = None
        scope.public_status()
        holder.kill()
        holder.wait()
        shown = wait_state(restarted["id"], "kept", timeout=60)
        assert shown["branches"][0]["state"] == "uncertain", shown
        assert "restarted while closing" in shown["reason"], shown
        assert lane_r.exists()
        refused = host("run", "fixture", "identity", cwd=lane_r)
        assert refused.returncode == 125, refused.stderr
        refused = host("splits", "rm", restarted["id"])
        assert refused.returncode != 0 and b"not confirmed" in refused.stderr, refused.stderr
        reset = host("workers", "reset", "all", timeout=900)
        assert reset.returncode == 0, reset.stderr
        removed = host("splits", "rm", restarted["id"])
        assert removed.returncode == 0, removed.stderr
        assert not lane_r.exists()

        storage = json_of("storage", "--json")["projects"][0]
        assert storage["checkpoints"]["count"] == 1 and storage["kept_splits"]["count"] == 1, storage
        assert storage["volume"]["free_bytes"] > 0
        seqs = [event["seq"] for event in events if "seq" in event]
        assert seqs == sorted(seqs) and events[0]["type"] == "hello", events[:3]
        kinds = {event["type"] for event in events}
        assert {"job", "split", "checkpoint"} <= kinds, kinds
        lane_jobs = [event["job"] for event in events
                     if event["type"] == "job" and event["job"].get("parent") == f"split:{split}/a"]
        assert lane_jobs, "lane jobs are recorded under their lane"
        report.update(outcome="passed", split=split, checkpoint=checkpoint, lane_files=files,
                      events=len(events))
    except Exception:
        report["failure"] = traceback.format_exc()
    finally:
        if subscriber is not None:
            subscriber.kill()
            subscriber.wait()
        report["cleanup_errors"] = scope.cleanup_isolated_scope()
        if report["cleanup_errors"]:
            report["outcome"] = "failed"
        (args.evidence / "lanes-uat.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"outcome": report["outcome"], "evidence": str(args.evidence / "lanes-uat.json")}))
    return 0 if report["outcome"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
