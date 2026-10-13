#!/usr/bin/env python3
"""Scaffold benchmark tasks from SWE-bench Verified (DESIGN.md, "Tasks").

    curate_swebench.py pool [--repo sympy/sympy]... [--seed 20261012]
        Print eligible instance ids in a seeded random order. Take them in
        that order; skip (and record why) any that fail validation.
    curate_swebench.py fetch --tasks DIR INSTANCE_ID...
        Write DIR/ID/{task.json, hidden.patch, gold.patch}. The visible gate
        (gate.patch, gate.cmd) and the setup command are written by a curator.
    curate_swebench.py validate --tasks DIR --marsh marsh ID...
        In the marsh shell VM, with no agent: the gate fails at the base and
        passes with the gold patch, and the hidden command fails at the base
        and passes with the gold patch. Prints one JSON line per task.

`pool` and `fetch` read the public Hugging Face datasets server; `validate`
uses the shell VM only and bills no model.
"""
from __future__ import annotations

import argparse
import json
import pathlib
import random
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from typing import Any

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import run  # noqa: E402

DATASET = "princeton-nlp/SWE-bench_Verified"
SERVER = "https://datasets-server.huggingface.co/rows"
INSTRUCTION = ("\n\n---\nFix the issue above in this repository. The check `{gate}` reproduces it and fails now; "
               "make it pass without editing it or any other test. Keep the change minimal.")


def all_rows() -> list[dict[str, Any]]:
    """Every row of the dataset (500), paged from the datasets server's
    /rows endpoint with retries; filtering happens locally."""
    found: list[dict[str, Any]] = []
    while True:
        query = urllib.parse.urlencode({"dataset": DATASET, "config": "default", "split": "test",
                                        "offset": len(found), "length": 50})
        for attempt in range(6):
            try:
                with urllib.request.urlopen(f"{SERVER}?{query}", timeout=60) as response:
                    page = json.load(response)
                break
            except urllib.error.HTTPError as error:
                error.close()
                if attempt == 5:
                    raise
                time.sleep(2 ** attempt)
        found += [r["row"] for r in page["rows"]]
        if not page["rows"] or len(found) >= page["num_rows_total"]:
            return found


def rows(**match: str) -> list[dict[str, Any]]:
    return [row for row in all_rows() if all(row[k] == v for k, v in match.items())]


def pool(args: argparse.Namespace) -> int:
    eligible = []
    for row in all_rows():
        if row["repo"] in args.repo:
            if row["difficulty"] in args.difficulty and len(json.loads(row["FAIL_TO_PASS"])) <= args.max_f2p:
                eligible.append(row["instance_id"])
    eligible.sort()
    random.Random(args.seed).shuffle(eligible)
    print("\n".join(eligible))
    return 0


def test_files(patch: str) -> list[str]:
    return sorted({line[len("+++ b/"):] for line in patch.splitlines() if line.startswith("+++ b/")})


def fetch(args: argparse.Namespace) -> int:
    for instance in args.ids:
        found = rows(instance_id=instance)
        if not found:
            print(f"{instance}: not found", file=sys.stderr)
            return 1
        row = found[0]
        target = args.tasks / instance
        target.mkdir(parents=True, exist_ok=True)
        (target / "hidden.patch").write_text(row["test_patch"])
        (target / "gold.patch").write_text(row["patch"])
        files = " ".join(test_files(row["test_patch"]))
        task = {
            "id": instance, "source": "swebench-verified", "difficulty": row["difficulty"],
            "repo": f"https://github.com/{row['repo']}.git", "base": row["base_commit"],
            "setup": "TODO: e.g. sudo apt-get install -y python3-pytest python3-mpmath",
            "prompt": row["problem_statement"].strip() + INSTRUCTION,
            "gate": {"cmd": "TODO", "patch": "gate.patch"},
            "hidden": {"cmd": f"python3 -m pytest -q -p no:cacheprovider {files}", "patch": "hidden.patch",
                       "fail_to_pass": json.loads(row["FAIL_TO_PASS"]),
                       "pass_to_pass": json.loads(row["PASS_TO_PASS"])},
            "status": "needs-gate",
        }
        (target / "task.json").write_text(json.dumps(task, indent=2) + "\n")
        print(target)
    return 0


def validate(args: argparse.Namespace) -> int:
    marsh = run.Marsh(args.marsh, dict(__import__("os").environ), 1800)
    failures = 0
    for instance in args.ids:
        task_dir = (args.tasks / instance).resolve()
        task = json.loads((task_dir / "task.json").read_text())
        work = pathlib.Path(args.work).resolve() / instance
        template = run.prepare_template(task_dir, task, work.parent)
        if task.get("setup"):
            marsh.call(["-c", task["setup"]], template)
        gold = (task_dir / "gold.patch").read_text()

        def gate(patch: str) -> bool:
            project = work / "gate"
            run.fresh_clone(template, project)
            if patch:
                run.git(project, "apply", "-", stdin=patch.encode())
            return marsh.call(["-c", task["gate"]["cmd"]], project).returncode == 0

        cache: dict[str, Any] = {}
        result = {
            "id": instance,
            "gate_fails_at_base": not gate(""),
            "gate_passes_with_gold": gate(gold),
            "hidden_fails_at_base": not run.score_hidden(marsh, task_dir, task, template, "", work, cache)["passed"],
            "hidden_passes_with_gold": run.score_hidden(marsh, task_dir, task, template, gold, work, cache)["passed"],
        }
        result["valid"] = all(v for k, v in result.items() if k != "id")
        failures += not result["valid"]
        print(json.dumps(result))
    return 1 if failures else 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("pool")
    p.add_argument("--repo", action="append", default=None)
    p.add_argument("--difficulty", action="append", default=None)
    p.add_argument("--max-f2p", type=int, default=5)
    p.add_argument("--seed", type=int, default=20261012)
    f = sub.add_parser("fetch")
    f.add_argument("--tasks", type=pathlib.Path, required=True)
    f.add_argument("ids", nargs="+")
    v = sub.add_parser("validate")
    v.add_argument("--tasks", type=pathlib.Path, required=True)
    v.add_argument("--marsh", default="marsh")
    v.add_argument("--work", default="target/eval-validate")
    v.add_argument("ids", nargs="+")
    args = parser.parse_args(argv)
    if args.command == "pool":
        args.repo = args.repo or ["sympy/sympy", "pytest-dev/pytest"]
        args.difficulty = args.difficulty or ["<15 min fix", "15 min - 1 hour"]
        return pool(args)
    return fetch(args) if args.command == "fetch" else validate(args)


if __name__ == "__main__":
    sys.exit(main())
