#!/usr/bin/env python3
"""Agent-reach harness (DESIGN.md): can a model that has only llms.txt
install and use marsh correctly?

    run.py ask    --out DIR [--models M,...] [--conditions closed,open] [--reps 3]
                  [--responder opencode|fixture:DIR] [--llms URL|PATH] [--only TASK]...
    run.py grade  --out DIR --backend fake|real [--marsh PATH] [--marsh-home DIR] [--yes]
    run.py report --out DIR [--json]

`ask` gets one sh script per (model, condition, task, rep) from `opencode run`
with a generated agent that may use no tools (closed) or only fetch runmar.sh
and the marsh GitHub repository (open). `grade` runs each script in a scratch
project with recording shims for package managers, downloaders, sbx, docker
and sudo (nothing is really installed), against fakemarsh (`fake`) or a real marsh with an isolated
MARSH_HOME (`real`, which runs real agents and bills them), then applies the
task's checks. `report` tabulates.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import pathlib
import random
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
from typing import Any

HERE = pathlib.Path(__file__).resolve().parent
REPO = HERE.parents[1]
FAKE = HERE.parent / "common" / "fakemarsh.py"
DEFAULT_MODELS = ("anthropic/claude-sonnet-5-5", "openai/gpt-6.1-sol", "google/gemini-3.8-flash",
                  "opencode/mimo-v2.6-flash-free")
SHIMMED = ("brew", "curl", "wget", "sbx", "sudo", "pip", "pip3", "pipx", "uv", "npm", "npx", "cargo", "gem", "port", "docker")
DENY = re.compile(r"rm\s+-rf\s+(/|~|\$HOME)(\s|$)|diskutil|launchctl|\bdd\s+if=|mkfs|security\s+(delete|add)")


# ------------------------------------------------------------------ ask

def load_llms(source: str) -> str:
    if re.match(r"https?://", source):
        with urllib.request.urlopen(source, timeout=60) as response:
            return response.read().decode()
    return pathlib.Path(source).read_text()


def agent_file(condition: str) -> str:
    rules = ['  - action: "*"', '    resource: "*"', "    effect: deny"]
    if condition == "open":
        for resource in ("https://runmar.sh/*", "https://github.com/mcavage/marsh*",
                         "https://raw.githubusercontent.com/mcavage/marsh/*"):
            rules += ["  - action: webfetch", f'    resource: "{resource}"', "    effect: allow"]
    return "\n".join(["---", f"description: marsh reach eval, {condition} book", "mode: primary", "steps: 12",
                      "permissions:", *rules, "---", "",
                      "Answer the user's request. You have no shell and cannot run anything yourself."]) + "\n"


def prompt_for(tasks: dict[str, Any], task: dict[str, Any], docs: dict[str, str], condition: str) -> str:
    preamble = task.get("preamble", tasks["preamble"])
    if condition == "reference":
        return (f"{preamble}\n\nYour only documentation is marsh's whole manual, below. You cannot fetch "
                f"anything.\n\n<llms-full.txt>\n{docs['full']}\n</llms-full.txt>\n\nRequest: {task['prompt']}\n")
    access = ("You cannot fetch anything; the index below is all you have." if condition == "closed" else
              "You may fetch the pages the index links to (runmar.sh) with your web fetch tool.")
    return (f"{preamble}\n\nYour only documentation is marsh's llms.txt. {access}\n\n"
            f"<llms.txt>\n{docs['index']}\n</llms.txt>\n\nRequest: {task['prompt']}\n")


def extract_script(response: str) -> str | None:
    blocks = re.findall(r"```(?:sh|bash|shell|zsh|console)?[ \t]*\n(.*?)```", response, re.S)
    if not blocks:
        return None
    script = max(blocks, key=len)
    # A console block's "$ " prompts are not part of the commands.
    return "\n".join(re.sub(r"^\s*[$%] ", "", line) for line in script.splitlines()) + "\n"


def ask_opencode(model: str, condition: str, prompt: str, timeout: int) -> dict[str, Any]:
    with tempfile.TemporaryDirectory(prefix="marsh-reach-") as scratch:
        agents = pathlib.Path(scratch) / ".opencode" / "agents"
        agents.mkdir(parents=True)
        (agents / f"reach-{condition}.md").write_text(agent_file(condition))  # reference: no tools
        started = time.time()
        done = subprocess.run(["opencode", "run", "-m", model, "--agent", f"reach-{condition}", "--format", "json",
                               prompt], cwd=scratch, capture_output=True, text=True, timeout=timeout)
        texts, cost, tools = [], 0.0, []
        for line in done.stdout.splitlines():
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            part = event.get("part") or {}
            if part.get("type") == "text" and part.get("text"):
                texts.append(part["text"])
            elif part.get("type") not in (None, "text", "step-start"):
                tools.append(part.get("tool") or part.get("type"))
            if isinstance(part.get("cost"), (int, float)):
                cost += part["cost"]
        return {"response": "\n".join(texts) if texts else done.stdout, "transcript": done.stdout,
                "stderr_tail": done.stderr[-2000:], "exit": done.returncode, "wall_s": time.time() - started,
                "reported_cost_usd": cost, "parts": tools}


def ask(args: argparse.Namespace) -> int:
    tasks = json.loads((HERE / "tasks.json").read_text())
    conditions = args.conditions.split(",")
    docs = {"index": load_llms(args.llms)}
    if "reference" in conditions:
        docs["full"] = load_llms(args.llms_full)
    llms_sha = {k: hashlib.sha256(v.encode()).hexdigest() for k, v in docs.items()}
    # Task-major order with models innermost, so a provider's slow hour or a
    # site change does not fall on one model.
    for task in tasks["tasks"]:
        if args.only and task["id"] not in args.only:
            continue
        for rep in range(args.reps):
            for condition in conditions:
                models = args.models.split(",")
                random.Random(f"{task['id']}:{rep}:{condition}").shuffle(models)
                for model in models:
                    trial = args.out / model.replace("/", "__") / condition / task["id"] / f"rep{rep}"
                    if (trial / "meta.json").exists():
                        continue
                    trial.mkdir(parents=True, exist_ok=True)
                    prompt = prompt_for(tasks, task, docs, condition)
                    if args.responder.startswith("fixture:"):
                        source = pathlib.Path(args.responder[len("fixture:"):]) / f"{task['id']}.md"
                        reply = {"response": source.read_text() if source.exists() else "", "exit": 0, "wall_s": 0}
                    else:
                        reply = ask_opencode(model, condition, prompt, args.timeout)
                    script = extract_script(reply["response"])
                    (trial / "prompt.txt").write_text(prompt)
                    (trial / "response.txt").write_text(reply["response"])
                    if reply.get("transcript"):
                        # Includes fetched page bodies in the open condition.
                        (trial / "transcript.jsonl").write_text(reply["transcript"])
                    if script is not None:
                        (trial / "script.sh").write_text(script)
                    meta = {"model": model, "condition": condition, "task": task["id"], "rep": rep,
                            "llms_sha256": llms_sha, "has_script": script is not None,
                            "prompt_chars": len(prompt), "response_chars": len(reply["response"]),
                            **{k: v for k, v in reply.items() if k not in ("response", "transcript")}}
                    (trial / "meta.json").write_text(json.dumps(meta, indent=2) + "\n")
                    print(f"{model} {condition} {task['id']} rep{rep}: script={script is not None}", file=sys.stderr)
    return 0


# ------------------------------------------------------------------ grade

SHIM = """#!{python}
import json, os, sys
name = os.path.basename(sys.argv[0])
with open(os.environ["REACH_LOG"], "a") as log:
    log.write(json.dumps({{"argv": [name, *sys.argv[1:]]}}) + "\\n")
if name == "curl" and any("runmar.sh/install" in a for a in sys.argv[1:]):
    print("echo marsh-installer-ran")
elif name == "marsh":
    target = os.environ["REACH_MARSH"]
    prefix = [sys.executable, target] if target.endswith(".py") else [target]
    os.execv(prefix[0], [*prefix, *sys.argv[1:]])
"""


def make_shims(directory: pathlib.Path) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    for name in (*SHIMMED, "marsh"):
        path = directory / name
        path.write_text(SHIM.format(python=sys.executable))
        path.chmod(0o755)


def make_project(name: str | None, dest: pathlib.Path) -> None:
    dest.mkdir(parents=True)
    if name is None:
        return
    shutil.copytree(HERE / "fixtures" / name, dest, dirs_exist_ok=True)
    for args in (["init", "-q", "-b", "main"], ["add", "-A"], ["commit", "-q", "-m", "base"]):
        subprocess.run(["git", "-c", "user.name=reach", "-c", "user.email=reach@example.invalid",
                        "-c", "commit.gpgsign=false", *args], cwd=dest, check=True, capture_output=True)


def known_subcommands() -> set[str]:
    page = (REPO / "docs" / "man" / "marsh.1.md").read_text()
    section = page[page.index("# COMMANDS"):page.index("# ENVIRONMENT")]
    names = set(re.findall(r"^\*\*([a-z][a-z-]*)", section, re.M)) | set(re.findall(r"\*\*([a-z][a-z-]*) \{", section))
    return names | {"collect", "-c", "-V", "--version", "-h", "--help", "--load", "--shell", "--ephemeral-home"}


def marsh_words(script: str) -> list[str]:
    """The word after each `marsh`/`msh` command in a script."""
    code = "\n".join(line for line in script.splitlines() if not line.lstrip().startswith("#"))
    return re.findall(r"(?:^|[\s|;&(`$])(?:marsh|msh)\s+([^\s'\"|;&)]+)", code)


def jobs(marsh: str, env: dict[str, str], cwd: pathlib.Path) -> list[dict[str, Any]]:
    done = subprocess.run([marsh, "jobs", "--all", "--json"], cwd=cwd, env=env, capture_output=True, text=True)
    try:
        return json.loads(done.stdout).get("jobs", [])
    except json.JSONDecodeError:
        return []


def check(spec: dict[str, Any], ctx: dict[str, Any]) -> bool:
    kind = spec["type"]
    argv = [" ".join(entry["argv"]) for entry in ctx["log"]]
    project: pathlib.Path = ctx["project"]
    if kind == "argv_any":
        return any(re.search(spec["pattern"], line) for line in argv)
    if kind == "argv_none":
        return not any(re.search(spec["pattern"], line) for line in argv)
    if kind == "any_of":
        return any(check(option, ctx) for option in spec["checks"])
    if kind == "script_regex":
        return re.search(spec["pattern"], ctx["script"]) is not None
    if kind == "script_count":
        return len(re.findall(spec["pattern"], ctx["script"])) >= spec["min"]
    if kind == "stdout_count":
        return len(re.findall(spec["pattern"], ctx["stdout"])) >= spec["min"]
    if kind == "file_regex":
        path = project / spec["path"]
        return path.is_file() and re.search(spec["pattern"], path.read_text(errors="replace")) is not None
    if kind == "stdout_regex":
        return re.search(spec["pattern"], ctx["stdout"]) is not None
    if kind == "exit_zero":
        return ctx["exit"] == 0
    if kind == "file_nonempty":
        path = project / spec["path"]
        return path.is_file() and path.stat().st_size > 0
    if kind == "known_cli":
        return not ctx["unknown_cli"]
    if kind == "agent_ran":
        return spec["agent"] in ctx["agents"]
    if kind in ("git_clean", "only_changed"):
        status = subprocess.run(["git", "status", "--porcelain", "--untracked-files=all"], cwd=project,
                                capture_output=True, text=True).stdout.splitlines()
        changed = {line[3:] for line in status if not line[3:].startswith(".marsh/")}
        changed = {c for c in changed if "__pycache__" not in c and c != "SUMMARY.md"}
        return not changed if kind == "git_clean" else bool(changed) and changed <= set(spec["paths"])
    if kind == "cmd_ok":
        return subprocess.run(spec["cmd"], shell=True, cwd=project, capture_output=True).returncode == 0
    if kind == "no_entries":
        path = project / spec["path"]
        return not path.exists() or not any(path.iterdir())
    raise ValueError(f"unknown check {kind}")


def grade_trial(trial: pathlib.Path, task: dict[str, Any], args: argparse.Namespace) -> dict[str, Any]:
    script_path = trial / "script.sh"
    if not script_path.exists():
        return {"ran": False, "reason": "no script in the reply", "success": False, "orchestration": False,
                "score": 0.0, "checks": []}
    script = script_path.read_text()
    if DENY.search(script):
        return {"ran": False, "reason": "refused: destructive command", "success": False, "orchestration": False,
                "score": 0.0, "checks": []}
    scratch = pathlib.Path(tempfile.mkdtemp(prefix="reach-grade-"))
    try:
        project, home, shims = scratch / "project", scratch / "home", scratch / "shims"
        home.mkdir()
        make_project(task.get("project"), project)
        make_shims(shims)
        fake = args.backend == "fake" or task.get("shim_only")
        log = scratch / "log.jsonl"
        log.touch()
        env = {"PATH": f"{shims}{os.pathsep}/usr/bin:/bin:/usr/sbin:/sbin", "HOME": str(home),
               "REACH_LOG": str(log), "LANG": "C.UTF-8", "TERM": "dumb",
               "REACH_MARSH": str(FAKE) if fake else args.marsh}
        if fake:
            env.update(FAKEMARSH_LOG=str(log), FAKEMARSH_STATE=str(scratch / "fakemarsh"),
                       FAKEMARSH_PLAN=str(HERE / "fixtures" / "plan.json"))
            plan = json.loads((HERE / "fixtures" / "plan.json").read_text())
            for actions in plan.values():
                for action in actions:
                    if "patch" in action:
                        action["patch"] = str(HERE / "fixtures" / action["patch"])
            (scratch / "plan.json").write_text(json.dumps(plan))
            env["FAKEMARSH_PLAN"] = str(scratch / "plan.json")
        else:
            env["MARSH_HOME"] = str(args.marsh_home.resolve())
            # The script sees a recording sbx; marsh itself keeps the real one.
            env["MARSH_SBX"] = shutil.which("sbx") or "sbx"
            before = {j.get("job_id") for j in jobs(args.marsh, env, project)}
        for seed in task.get("seed", []):
            marsh = [sys.executable, str(FAKE)] if fake else [args.marsh]
            subprocess.run([*marsh, *seed], cwd=project, env=env, capture_output=True, timeout=600)
        try:
            done = subprocess.run(["sh", str(script_path.resolve())], cwd=project, env=env, capture_output=True,
                                  text=True, timeout=60 if fake else args.timeout, stdin=subprocess.DEVNULL)
            code, stdout, stderr = done.returncode, done.stdout, done.stderr
        except subprocess.TimeoutExpired as error:
            code, stdout, stderr = 124, (error.stdout or b"").decode(errors="replace") if isinstance(error.stdout, bytes) \
                else (error.stdout or ""), "timeout"
        entries = [json.loads(line) for line in log.read_text().splitlines() if line.strip()]
        if fake:
            agents = {e["argv"][0] for e in entries if e["argv"][0] in ("claude", "codex", "pi")}
        else:
            agents = {j.get("command") for j in jobs(args.marsh, env, project) if j.get("job_id") not in before}
        known = known_subcommands()
        unknown = sorted({w for w in marsh_words(script) if w not in known and not w.startswith("$")})
        ctx = {"log": entries, "script": script, "stdout": stdout, "exit": code, "project": project,
               "agents": agents, "unknown_cli": unknown}
        results = [{"type": c["type"], "why": c["why"], "critical": c["critical"],
                    "outcome": c.get("outcome", False), "passed": check(c, ctx)} for c in task["checks"]]
        return {"ran": True, "backend": "fake" if fake else "real", "exit": code,
                "stdout_tail": stdout[-3000:], "stderr_tail": stderr[-3000:],
                "calls": [" ".join(e["argv"])[:300] for e in entries], "unknown_cli": unknown,
                "checks": results, "success": all(r["passed"] for r in results if r["critical"]),
                # Orchestration ignores checks that depend on the agents doing
                # the work (a correct script can meet an agent that fails).
                "orchestration": all(r["passed"] for r in results if r["critical"] and not r["outcome"]),
                "score": sum(r["passed"] for r in results) / len(results)}
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


def grade(args: argparse.Namespace) -> int:
    if args.backend == "real":
        if not args.marsh_home:
            raise SystemExit("--backend real needs --marsh-home (an isolated selected home)")
        if not args.yes:
            raise SystemExit("--backend real runs model-written scripts and real agents; pass --yes")
    tasks = {t["id"]: t for t in json.loads((HERE / "tasks.json").read_text())["tasks"]}
    for meta_path in sorted(args.out.glob("*/*/*/rep*/meta.json")):
        trial = meta_path.parent
        if (trial / "grade.json").exists() and not args.regrade:
            continue
        meta = json.loads(meta_path.read_text())
        result = grade_trial(trial, tasks[meta["task"]], args)
        (trial / "grade.json").write_text(json.dumps(result, indent=2) + "\n")
        print(f"{trial.relative_to(args.out)}: success={result['success']} score={result['score']:.2f}",
              file=sys.stderr)
    return 0


# ------------------------------------------------------------------ report

def wilson(successes: int, n: int) -> tuple[float, float]:
    if n == 0:
        return (math.nan, math.nan)
    z, p = 1.96, successes / n
    centre = (p + z * z / (2 * n)) / (1 + z * z / n)
    half = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / (1 + z * z / n)
    return (centre - half, centre + half)


def report(args: argparse.Namespace) -> int:
    rows: dict[tuple[str, str], list[dict[str, Any]]] = {}
    per_task: dict[tuple[str, str, str], list[bool]] = {}
    for grade_path in sorted(args.out.glob("*/*/*/rep*/grade.json")):
        meta = json.loads((grade_path.parent / "meta.json").read_text())
        result = json.loads(grade_path.read_text())
        rows.setdefault((meta["model"], meta["condition"]), []).append(result)
        per_task.setdefault((meta["model"], meta["condition"], meta["task"]), []).append(result["success"])
    summary = []
    for (model, condition), results in sorted(rows.items()):
        wins = sum(r["success"] for r in results)
        lo, hi = wilson(wins, len(results))
        summary.append({"model": model, "condition": condition, "trials": len(results), "success": wins,
                        "rate": wins / len(results), "wilson95": [lo, hi],
                        "mean_score": sum(r["score"] for r in results) / len(results),
                        "orchestration": sum(r["orchestration"] for r in results),
                        "no_script": sum(1 for r in results if not r["ran"]),
                        "invented_cli": sum(1 for r in results if r.get("unknown_cli"))})
    if args.json:
        print(json.dumps({"summary": summary, "per_task": {"|".join(k): v for k, v in per_task.items()}}, indent=2))
        return 0
    print("Estimand: average success on these seven fixed tasks. Read the per-task table first; the Wilson\n"
          "interval treats trials as independent and says nothing about tasks outside this set.\n")
    print("| model | condition | success | rate (Wilson 95%) | orchestration ok | mean check score | no script | invented CLI |")
    print("|---|---|---|---|---|---|---|---|")
    for s in summary:
        print(f"| {s['model']} | {s['condition']} | {s['success']}/{s['trials']} | {s['rate']:.2f} "
              f"[{s['wilson95'][0]:.2f}, {s['wilson95'][1]:.2f}] | {s['orchestration']}/{s['trials']} | "
              f"{s['mean_score']:.2f} | {s['no_script']} | {s['invented_cli']} |")
    print("\n| model | condition | task | successes |\n|---|---|---|---|")
    for (model, condition, task), outcomes in sorted(per_task.items()):
        print(f"| {model} | {condition} | {task} | {sum(outcomes)}/{len(outcomes)} |")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    a = sub.add_parser("ask")
    a.add_argument("--out", type=pathlib.Path, required=True)
    a.add_argument("--models", default=",".join(DEFAULT_MODELS))
    a.add_argument("--conditions", default="closed,open,reference")
    a.add_argument("--reps", type=int, default=3)
    a.add_argument("--responder", default="opencode")
    a.add_argument("--llms", default="https://runmar.sh/llms.txt")
    a.add_argument("--llms-full", default="https://runmar.sh/llms-full.txt")
    a.add_argument("--only", action="append", default=[])
    a.add_argument("--timeout", type=int, default=600)
    g = sub.add_parser("grade")
    g.add_argument("--out", type=pathlib.Path, required=True)
    g.add_argument("--backend", choices=("fake", "real"), required=True)
    g.add_argument("--marsh", default=shutil.which("marsh") or "marsh")
    g.add_argument("--marsh-home", type=pathlib.Path)
    g.add_argument("--timeout", type=int, default=900)
    g.add_argument("--regrade", action="store_true")
    g.add_argument("--yes", action="store_true")
    r = sub.add_parser("report")
    r.add_argument("--out", type=pathlib.Path, required=True)
    r.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    return {"ask": ask, "grade": grade, "report": report}[args.command](args)


if __name__ == "__main__":
    sys.exit(main())
