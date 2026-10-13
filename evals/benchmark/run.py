#!/usr/bin/env python3
"""Workflow A/B benchmark runner (DESIGN.md). Drives marsh from the outside,
the way a user does from a plain shell: `marsh split ... | marsh join --keep`
for the attempts, `marsh -c` for setup, the judge, and hidden-test scoring.

    run.py --config config.json --tasks tasks --out results/run1 [--only ID]... [--reps N]

One JSON file per (task, rep) lands in --out; an existing file is skipped, so
an interrupted run resumes. `--estimate` prints the cost model and exits.
Point --marsh at evals/common/fakemarsh.py for a dry run with fake agents.
"""
from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import os
import random
import pathlib
import re
import shlex
import shutil
import subprocess
import sys
import time
from typing import Any
from xml.etree import ElementTree

ARMS = ("a_claude", "a_codex", "b_retry", "c_race", "d_judge", "e_fallback")
MARK = "@@eval"


def load_json(path: pathlib.Path) -> Any:
    return json.loads(path.read_text())


def sh_quote(text: str) -> str:
    return shlex.quote(text)


def git(cwd: pathlib.Path, *args: str, check: bool = True, stdin: bytes | None = None) -> subprocess.CompletedProcess[bytes]:
    env = dict(os.environ, GIT_AUTHOR_DATE="2000-01-01T00:00:00Z", GIT_COMMITTER_DATE="2000-01-01T00:00:00Z")
    return subprocess.run(["git", "-c", "user.name=marsh-eval", "-c", "user.email=eval@example.invalid",
                           "-c", "commit.gpgsign=false", *args], cwd=cwd, env=env, input=stdin,
                          capture_output=True, check=check)


# ---------------------------------------------------------------- branch scripts

def attempt_script(agent_cmd: str, prompt: str, gate_cmd: str, max_attempts: int, agent: str,
                   timeout_s: int = 0) -> str:
    """A shell-branch script: run the agent, then the gate; on a gate failure
    retry with the gate's output appended, up to max_attempts. It prints
    `@@eval` marker lines that run.py parses out of the branch's stdout.

    The gate command is embedded here rather than read from the fork, so an
    agent that edits files cannot change which command is the gate.
    """
    command = "$T " + agent_cmd.replace("{prompt}", '"$P$FB"')
    return "\n".join([
        "T=",
        # An agent that overruns is stopped; whatever it left is scored.
        f"[ {int(timeout_s)} -gt 0 ] && command -v timeout >/dev/null 2>&1 && T='timeout {int(timeout_s)}'",
        f"P={sh_quote(prompt)}",
        f"G={sh_quote(gate_cmd)}",
        "FB=''",
        "i=1",
        "while :; do",
        f"  echo \"{MARK} begin {agent} $i $(date +%s)\"",
        f"  {command} </dev/null",
        "  rc=$?",
        f"  echo \"{MARK} end {agent} $i $rc $(date +%s)\"",
        "  out=$( (eval \"$G\") 2>&1 )",
        "  g=$?",
        f"  echo \"{MARK} gate $i $g $(date +%s)\"",
        "  printf '%s\\n' \"$out\" | tail -n 40",
        "  [ \"$g\" -eq 0 ] && exit 0",
        f"  [ \"$i\" -ge {int(max_attempts)} ] && exit 1",
        "  FB=\"",
        "",
        "Your previous attempt is still in the working tree, but the check \\`$G\\` still fails."
        " Fix the code, not the check. The last lines of its output:",
        "$(printf '%s\\n' \"$out\" | tail -n 60)\"",
        "  i=$((i+1))",
        "done",
    ])


def parse_branch(stdout: str) -> dict[str, Any]:
    """Attempts, gate results and token usage from a branch's stdout."""
    attempts: list[dict[str, Any]] = []
    current: dict[str, Any] | None = None
    body: list[str] = []
    for line in stdout.splitlines():
        if line.startswith(MARK + " "):
            words = line.split()
            if words[1] == "begin":
                current = {"agent": words[2], "i": int(words[3]), "t_start": int(words[4])}
                body = []
            elif words[1] == "end" and current is not None:
                current.update(exit=int(words[4]), t_end=int(words[5]), usage=usage_from(body))
                attempts.append(current)
                current = None
            elif words[1] == "gate" and attempts:
                attempts[-1].update(gate=int(words[3]), t_gate=int(words[4]))
        elif current is not None:
            body.append(line)
    gate_passed = bool(attempts) and attempts[-1].get("gate") == 0
    return {"attempts": attempts, "gate_passed": gate_passed,
            "t_start": attempts[0]["t_start"] if attempts else None,
            "t_end": attempts[-1].get("t_gate", attempts[-1].get("t_end")) if attempts else None}


def usage_from(lines: list[str]) -> dict[str, Any]:
    """Token usage from `claude -p --output-format json` or `codex exec --json`."""
    usage = {"input": 0, "cached": 0, "cache_write": 0, "output": 0, "reported_cost_usd": None}
    for line in lines:
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("type") == "result" and "usage" in event:  # claude
            u = event["usage"]
            usage["input"] += int(u.get("input_tokens", 0))
            usage["cached"] += int(u.get("cache_read_input_tokens", 0))
            usage["cache_write"] += int(u.get("cache_creation_input_tokens", 0))
            usage["output"] += int(u.get("output_tokens", 0))
            if event.get("total_cost_usd") is not None:
                usage["reported_cost_usd"] = (usage["reported_cost_usd"] or 0) + float(event["total_cost_usd"])
        elif event.get("type") == "turn.completed" and "usage" in event:  # codex
            u = event["usage"]
            cached = int(u.get("cached_input_tokens", 0))
            usage["input"] += int(u.get("input_tokens", 0)) - cached
            usage["cached"] += cached
            usage["output"] += int(u.get("output_tokens", 0))
    return usage


def cost(usage: dict[str, Any], price: dict[str, float]) -> float:
    if usage.get("reported_cost_usd") is not None:
        return float(usage["reported_cost_usd"])
    return (usage["input"] * price["input"] + usage["cached"] * price["cache_read"]
            + usage["cache_write"] * price.get("cache_write", price["input"])
            + usage["output"] * price["output"]) / 1e6


# ---------------------------------------------------------------- marsh calls

class Marsh:
    def __init__(self, executable: str, env: dict[str, str], timeout: int) -> None:
        self.executable, self.env, self.timeout = executable, env, timeout

    def call(self, args: list[str], cwd: pathlib.Path, stdin: bytes = b"",
             timeout: int | None = None) -> subprocess.CompletedProcess[bytes]:
        return subprocess.run([self.executable, *args], cwd=cwd, input=stdin, capture_output=True,
                              env=self.env, timeout=timeout or self.timeout)

    def split_join(self, cwd: pathlib.Path, branches: dict[str, str]) -> dict[str, Any]:
        """`marsh split -n -b L=S... | marsh join --keep`; returns the join
        rendering, the kept results directory and wall time."""
        args = ["split", "-n"]
        for label, script in branches.items():
            args += ["-b", f"{label}={script}"]
        started = time.time()
        split = self.call(args, cwd)
        handle = next((line for line in split.stdout.decode().splitlines() if line.startswith("{")), None)
        if handle is None:
            raise RuntimeError(f"split printed no handle: {split.stderr.decode()[-2000:]}")
        joined = self.call(["join", "--keep"], cwd, stdin=(handle + "\n").encode())
        wall = time.time() - started
        split_id = json.loads(handle)["id"]
        out = cwd / ".marsh" / "split" / split_id / "out"
        return {"id": split_id, "out": out, "join": joined.stdout.decode(errors="replace"),
                "wall_s": wall, "status": split.returncode}


def branch_result(out: pathlib.Path, label: str, price_for: dict[str, dict[str, float]]) -> dict[str, Any]:
    target = out / label
    stdout = (target / "stdout").read_text(errors="replace") if (target / "stdout").exists() else ""
    parsed = parse_branch(stdout)
    patch = (target / "diff.patch").read_bytes() if (target / "diff.patch").exists() else b""
    files = (target / "files").read_text().splitlines() if (target / "files").exists() else []
    for attempt in parsed["attempts"]:
        attempt["cost_usd"] = cost(attempt["usage"], price_for[attempt["agent"]])
    return {**parsed, "status": (target / "status").read_text().strip() if (target / "status").exists() else "missing",
            "files": files, "patch": patch.decode(errors="replace"),
            "patch_sha": hashlib.sha256(patch).hexdigest()[:16] if patch else None,
            "touched_tests": [f.split("\t")[-1] for f in files
                              if re.search(r"(^|/)(tests?|testing)/|(^|/)test_|_test\.", f.split("\t")[-1])],
            "cost_usd": sum(a["cost_usd"] for a in parsed["attempts"]),
            "wall_s": (parsed["t_end"] - parsed["t_start"]) if parsed["t_start"] is not None else None}


# ---------------------------------------------------------------- tasks

def prepare_template(task_dir: pathlib.Path, task: dict[str, Any], cache: pathlib.Path) -> pathlib.Path:
    """A repository holding only the task's base tree (one `base` commit, no
    upstream history, so the fix commit is not reachable from the project)
    plus one `eval: visible gate` commit. Projects are clones of it."""
    template = cache / task["id"]
    if (template / ".git").exists():
        return template
    repo = task["repo"]
    if repo.startswith("local:"):
        shutil.copytree(task_dir / repo[len("local:"):], template)
    else:
        mirror = cache / f"{task['id']}.upstream"
        if not mirror.exists():
            git(cache, "clone", "-q", "--no-checkout", repo, str(mirror))
        template.mkdir(parents=True)
        archive = git(mirror, "archive", "--format=tar", task["base"]).stdout
        subprocess.run(["tar", "-x", "-f", "-"], cwd=template, input=archive, check=True)
    git(template, "init", "-q", "-b", "main")
    git(template, "add", "-A", "-f")
    git(template, "commit", "-q", "-m", "base")
    if task["gate"].get("patch"):
        git(template, "apply", str(task_dir / task["gate"]["patch"]))
        git(template, "add", "-A")
        git(template, "commit", "-q", "-m", "eval: visible gate")
    return template


def fresh_clone(template: pathlib.Path, dest: pathlib.Path) -> None:
    if dest.exists():
        shutil.rmtree(dest)
    dest.parent.mkdir(parents=True, exist_ok=True)
    git(dest.parent, "clone", "-q", str(template), str(dest))


def score_hidden(marsh: Marsh, task_dir: pathlib.Path, task: dict[str, Any], template: pathlib.Path,
                 patch: str, work: pathlib.Path, cache: dict[str, Any]) -> dict[str, Any]:
    """Apply a candidate patch to a clean clone, restore and apply the hidden
    tests (as SWE-bench does: test files the candidate touched are reset
    first), and run the hidden command in the shell VM. No agent runs here."""
    key = hashlib.sha256(patch.encode()).hexdigest()[:16] if patch else "empty"
    if key in cache:
        return cache[key]
    scoring = work / "score" / key
    fresh_clone(template, scoring)
    result: dict[str, Any] = {"patch_sha": key}
    if patch:
        applied = git(scoring, "apply", "--whitespace=nowarn", "-", check=False, stdin=patch.encode())
        if applied.returncode:
            result.update(passed=False, reason="patch does not apply", whole_file_pass=False)
            cache[key] = result
            return result
    hidden_patch = task_dir / task["hidden"]["patch"]
    touched = git(scoring, "apply", "--numstat", str(hidden_patch)).stdout.decode().split("\n")
    for line in touched:
        path = line.split("\t")[-1] if line.strip() else ""
        if path and git(scoring, "cat-file", "-e", f"HEAD:{path}", check=False).returncode == 0:
            git(scoring, "checkout", "-q", "HEAD", "--", path)
        elif path and (scoring / path).exists():
            (scoring / path).unlink()
    git(scoring, "apply", str(hidden_patch))
    hidden = task["hidden"]
    lists = {"fail_to_pass": hidden.get("fail_to_pass") or [], "pass_to_pass": hidden.get("pass_to_pass") or []}
    command = hidden["cmd"]
    junit = scoring / ".eval-junit.xml"
    if any(lists.values()):
        command += f" --junitxml={junit.name}"
    started = time.time()
    try:
        done = marsh.call(["-c", command], scoring, timeout=task.get("hidden_timeout_s", 900))
        code, tail = done.returncode, (done.stdout + done.stderr).decode(errors="replace")[-3000:]
    except subprocess.TimeoutExpired:
        code, tail = 124, "timeout"
    result.update(exit=code, tail=tail, wall_s=time.time() - started, whole_file_pass=code == 0)
    if any(lists.values()):
        # The primary endpoint, as in SWE-bench: every FAIL_TO_PASS test
        # passes and no PASS_TO_PASS test fails. The whole-file exit status
        # is kept as a secondary, stricter endpoint.
        outcomes = junit_outcomes(junit) if junit.exists() else []
        if not outcomes:
            result["infra"] = "the hidden run produced no test results"
        f2p = {t: test_status(t, outcomes) for t in lists["fail_to_pass"]}
        p2p = {t: test_status(t, outcomes) for t in lists["pass_to_pass"]}
        result.update(fail_to_pass={t: v for t, v in f2p.items() if v != "passed"},
                      pass_to_pass={t: v for t, v in p2p.items() if v not in ("passed", "skipped")},
                      passed=bool(outcomes) and all(v == "passed" for v in f2p.values())
                      and all(v in ("passed", "skipped") for v in p2p.values()))
    else:
        result["passed"] = code == 0
    cache[key] = result
    shutil.rmtree(scoring, ignore_errors=True)
    return result


def junit_outcomes(path: pathlib.Path) -> list[tuple[str, str, str]]:
    """(classname, name, passed|failed|skipped) for each pytest testcase."""
    outcomes = []
    for case in ElementTree.parse(path).iter("testcase"):
        tags = {child.tag for child in case}
        status = "failed" if tags & {"failure", "error"} else "skipped" if "skipped" in tags else "passed"
        outcomes.append((case.get("classname", ""), case.get("name", ""), status))
    return outcomes


def test_status(test: str, outcomes: list[tuple[str, str, str]]) -> str:
    """Match a SWE-bench test id to junit testcases. `path.py::Class::name`
    ids match by module, class, and name; bare names (sympy) match by name.
    A test that did not run is `missing`, which counts as a failure."""
    if "::" in test:
        path, *middle, name = test.split("::")
        classname = ".".join([path.removesuffix(".py").replace("/", "."), *middle])
        found = [s for c, n, s in outcomes if n == name and (c == classname or c.endswith("." + classname)
                                                            or classname.endswith("." + c) or c == "")]
    else:
        found = [s for _, n, s in outcomes if n == test]
    if not found:
        return "missing"
    return "failed" if "failed" in found else "passed" if "passed" in found else "skipped"


# ---------------------------------------------------------------- one run

def judge(marsh: Marsh, config: dict[str, Any], project: pathlib.Path, join_text: str) -> dict[str, Any]:
    prompt = config["judge"]["prompt"]
    command = config["judge"]["cmd"].replace("{prompt}", sh_quote(prompt))
    started = time.time()
    done = marsh.call(["-c", command], project, stdin=join_text.encode())
    text = done.stdout.decode(errors="replace")
    first = next((line.strip().strip("`*").lower() for line in text.splitlines() if line.strip()), "")
    pick = first if first in ("claude", "codex") else None
    tokens_in = len(join_text) // 4 + len(prompt) // 4
    tokens_out = len(text) // 4
    price = config["prices"][config["judge"]["price"]]
    return {"invoked": True, "exit": done.returncode, "first_line": first[:80], "pick": pick,
            "wall_s": time.time() - started,
            "cost_usd": (tokens_in * price["input"] + tokens_out * price["output"]) / 1e6,
            "cost_estimated_from_chars": True}


def run_one(marsh: Marsh, config: dict[str, Any], task_dir: pathlib.Path, task: dict[str, Any],
            rep: int, work: pathlib.Path, template: pathlib.Path) -> dict[str, Any]:
    agents = config["agents"]
    price_for = {name: config["prices"][spec["price"]] for name, spec in agents.items()}
    gate = task["gate"]["cmd"]
    prompt = task["prompt"]
    n = int(config.get("max_attempts", 3))
    base = work / task["id"] / f"rep{rep}"
    race_project, retry_project = base / "race" / task["id"], base / "retry" / task["id"]
    fresh_clone(template, race_project)
    fresh_clone(template, retry_project)
    timeout = int(config.get("agent_timeout_s", 0))
    # join renders branches in the order written; alternate it so the judge
    # does not always read Claude first.
    order = ["claude", "codex"]
    random.Random(f"{config.get('seed', 20261012)}:{task['id']}:{rep}").shuffle(order)
    race_branches = {label: attempt_script(agents[label]["cmd"], prompt, gate, 1, label, timeout) for label in order}
    retry_branches = {"retry": attempt_script(agents["claude"]["cmd"], prompt, gate, n, "claude", timeout)}
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        race_future = pool.submit(marsh.split_join, race_project, race_branches)
        retry_future = pool.submit(marsh.split_join, retry_project, retry_branches)
        race, retry = race_future.result(), retry_future.result()
    branches = {label: branch_result(race["out"], label, price_for) for label in race_branches}
    branches["retry"] = branch_result(retry["out"], "retry", price_for)

    passing = [label for label in ("claude", "codex") if branches[label]["gate_passed"]]
    # "First passing wins": the passing branch whose gate finished first.
    winner = min(passing, key=lambda label: branches[label]["t_end"]) if passing else "claude"
    judge_record: dict[str, Any] = {"invoked": False}
    judged = winner
    if len(passing) == 2:
        judge_record = judge(marsh, config, race_project, race["join"])
        judged = judge_record["pick"] or winner

    cache: dict[str, Any] = {}

    def hidden(label: str) -> dict[str, Any]:
        return score_hidden(marsh, task_dir, task, template, branches[label]["patch"], base, cache)

    # What the race would take if marsh cancelled the loser at the first pass
    # (it does not: a split waits for every branch, which is wall_s).
    starts = [branches[label]["t_start"] for label in ("claude", "codex") if branches[label]["t_start"] is not None]
    first_pass_s = branches[winner]["t_end"] - min(starts) if passing and starts else None
    arms = {
        "a_claude": {"label": "claude", "wall_s": branches["claude"]["wall_s"],
                     "cost_usd": branches["claude"]["cost_usd"]},
        "a_codex": {"label": "codex", "wall_s": branches["codex"]["wall_s"],
                    "cost_usd": branches["codex"]["cost_usd"]},
        "b_retry": {"label": "retry", "wall_s": branches["retry"]["wall_s"],
                    "cost_usd": branches["retry"]["cost_usd"], "attempts": len(branches["retry"]["attempts"])},
        "c_race": {"label": winner, "wall_s": race["wall_s"], "wall_first_pass_s": first_pass_s,
                   "cost_usd": branches["claude"]["cost_usd"] + branches["codex"]["cost_usd"]},
        # Derived, not run: Claude alone, and Codex only if Claude's gate
        # failed (same draws). Cost counts Codex only when it would have run;
        # wall time is the two in sequence.
        "e_fallback": {"label": "claude" if branches["claude"]["gate_passed"] else "codex",
                       "wall_s": (branches["claude"]["wall_s"] or 0)
                       + (0 if branches["claude"]["gate_passed"] else (branches["codex"]["wall_s"] or 0)),
                       "cost_usd": branches["claude"]["cost_usd"]
                       + (0 if branches["claude"]["gate_passed"] else branches["codex"]["cost_usd"])},
        "d_judge": {"label": judged, "wall_s": race["wall_s"] + judge_record.get("wall_s", 0),
                    "cost_usd": branches["claude"]["cost_usd"] + branches["codex"]["cost_usd"]
                    + judge_record.get("cost_usd", 0)},
    }
    for arm in arms.values():
        score = hidden(arm["label"])
        arm.update(hidden_pass=bool(score.get("passed")), patch_sha=score["patch_sha"],
                   gate_pass=branches[arm["label"]]["gate_passed"])
    for label in ("race", "retry"):
        project = race_project if label == "race" else retry_project
        split_id = race["id"] if label == "race" else retry["id"]
        marsh.call(["splits", "rm", split_id], project)
    return {"task": task["id"], "source": task.get("source"), "rep": rep, "finished": time.time(),
            "marsh_version": marsh.call(["--version"], race_project.parent).stdout.decode().strip(),
            "branches": branches,
            "race_order": order, "judge": judge_record, "arms": arms, "hidden": cache,
            "race_wall_s": race["wall_s"], "retry_wall_s": retry["wall_s"]}


# ---------------------------------------------------------------- cost model

def estimate(config: dict[str, Any], tasks: int, reps: int) -> dict[str, Any]:
    """Expected and worst-case spend from per-attempt cost assumptions."""
    m = config["cost_model"]
    p_fail = m["p_gate_fail_first_attempt"]
    n = int(config.get("max_attempts", 3))
    expected_retry_attempts = sum(p_fail ** k for k in range(n))
    per_run = (m["claude_attempt_usd"] + m["codex_attempt_usd"]
               + expected_retry_attempts * m["claude_attempt_usd"] * m["retry_attempt_discount"]
               + m["p_both_pass"] * m["judge_usd"])
    worst = (m["claude_attempt_usd"] + m["codex_attempt_usd"] + n * m["claude_attempt_usd"]
             + m["judge_usd"]) * m["worst_case_multiplier"]
    runs = tasks * reps
    return {"runs": runs, "expected_retry_attempts": round(expected_retry_attempts, 2),
            "expected_per_run_usd": round(per_run, 2), "expected_total_usd": round(per_run * runs, 2),
            "calibration_usd": round(per_run * m["calibration_runs"], 2),
            "worst_case_total_usd": round(worst * runs, 2), "budget_cap_usd": config["budget_usd"]}


def spent(out: pathlib.Path) -> float:
    total = 0.0
    for path in out.glob("*.rep*.json"):
        record = load_json(path)
        total += sum(b["cost_usd"] for b in record["branches"].values()) + record["judge"].get("cost_usd", 0)
    return total


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--config", type=pathlib.Path, required=True)
    parser.add_argument("--tasks", type=pathlib.Path, required=True)
    parser.add_argument("--out", type=pathlib.Path)
    parser.add_argument("--work", type=pathlib.Path, help="scratch projects (default OUT/work)")
    parser.add_argument("--marsh", default="marsh")
    parser.add_argument("--only", action="append", default=[])
    parser.add_argument("--reps", type=int)
    parser.add_argument("--estimate", action="store_true")
    parser.add_argument("--task-count", type=int, help="with --estimate: size the run for N tasks")
    parser.add_argument("--marsh-home", type=pathlib.Path,
                        help="MARSH_HOME for the run (default: the user's own selected home)")
    args = parser.parse_args(argv)
    config = load_json(args.config)
    task_dirs = sorted(p.parent for p in args.tasks.resolve().glob("*/task.json"))
    if args.only:
        task_dirs = [d for d in task_dirs if d.name in args.only]
    reps = args.reps or int(config.get("reps", 2))
    if args.estimate:
        print(json.dumps(estimate(config, args.task_count or len(task_dirs), reps), indent=2))
        return 0
    if not args.out:
        parser.error("--out is required")
    args.out.mkdir(parents=True, exist_ok=True)
    work = (args.work or args.out / "work").resolve()
    (work / "templates").mkdir(parents=True, exist_ok=True)
    env = dict(os.environ)
    if args.marsh_home:
        env["MARSH_HOME"] = str(args.marsh_home.resolve())
    marsh = Marsh(args.marsh, env, int(config.get("timeout_s", 3600)))
    setups_done: set[str] = set()
    meta = args.out / "run-meta.json"
    if not meta.exists():
        status = marsh.call(["status", "--json"], work)
        meta.write_text(json.dumps({"started": time.time(), "config": config,
                                    "marsh_version": marsh.call(["--version"], work).stdout.decode().strip(),
                                    "marsh_status": status.stdout.decode(errors="replace")}, indent=2) + "\n")
    # Rep-major order: every task's rep 0 before any rep 1, so a budget stop
    # still leaves one complete, paired replicate.
    for rep in range(reps):
        order = list(task_dirs)
        random.Random(int(config.get("seed", 20261012)) + rep).shuffle(order)
        for task_dir in order:
            task = load_json(task_dir / "task.json")
            dest = args.out / f"{task['id']}.rep{rep}.json"
            if dest.exists():
                continue
            # Launch a run only if the budget still covers its worst case.
            if spent(args.out) + float(config.get("reserve_per_run_usd", 0)) > config["budget_usd"]:
                print(f"budget cap ${config['budget_usd']} cannot cover another run; stopping", file=sys.stderr)
                return 3
            template = prepare_template(task_dir, task, work / "templates")
            if task.get("setup") and task["id"] not in setups_done:
                marsh.call(["-c", task["setup"]], template, timeout=1800)
                setups_done.add(task["id"])
            print(f"{task['id']} rep {rep}", file=sys.stderr)
            record = run_one(marsh, config, task_dir, task, rep, work, template)
            dest.write_text(json.dumps(record, indent=2, sort_keys=True) + "\n")
            shutil.rmtree(work / task["id"] / f"rep{rep}", ignore_errors=True)
    print(f"done; spent ${spent(args.out):.2f}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
