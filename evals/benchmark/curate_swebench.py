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
import functools
import json
import os
import pathlib
import random
import re
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


@functools.cache
def all_rows() -> list[dict[str, Any]]:
    """Every row of the dataset (500), paged from the datasets server's
    /rows endpoint with retries; filtering happens locally. A saved copy can
    be named by SWEBENCH_VERIFIED_JSON."""
    saved = os.environ.get("SWEBENCH_VERIFIED_JSON")
    if saved:
        return json.loads(pathlib.Path(saved).read_text())
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


PYTEST_STACK = ["pytest==8.3.5", "pluggy==1.5.0", "iniconfig==2.0.0", "packaging==24.2"]
# Pure-Python test dependencies per repository version, after SWE-bench's
# environment specs, pinned to wheels that import on Python 3.13 (the agent
# and shell Kits' python3). pytest-dev/pytest tests itself from src/.
PYTEST_REPO_DEPS = {
    "4": ["atomicwrites==1.4.0", "attrs==23.1.0", "more-itertools==10.1.0", "pluggy==0.13.1", "py==1.11.0",
          "six==1.16.0", "wcwidth==0.2.6", "packaging==23.1"],
    "5": ["atomicwrites==1.4.0", "attrs==23.1.0", "more-itertools==10.1.0", "pluggy==0.13.1", "py==1.11.0",
          "wcwidth==0.2.6", "packaging==23.1"],
    "6": ["attrs==23.1.0", "iniconfig==2.0.0", "more-itertools==10.1.0", "packaging==23.1", "pluggy==0.13.1",
          "py==1.11.0", "toml==0.10.2"],
}


SITECUSTOMIZE = ("# Process this directory's .pth files (setuptools' distutils shim), as an\n"
                 "# installed site-packages would.\nimport os\nimport site\n\n"
                 "site.addsitedir(os.path.dirname(os.path.abspath(__file__)))\n")


def pinned(requirement: str) -> dict[str, str]:
    """{"req", "sha256"} for the requirement's pure-Python wheel, from PyPI."""
    name, _, version = requirement.partition("==")
    with urllib.request.urlopen(f"https://pypi.org/pypi/{name}/{version}/json", timeout=60) as response:
        files = json.load(response)["urls"]
    for wanted in ("-py3-none-any.whl", "-py2.py3-none-any.whl"):
        for item in files:
            if item["filename"].endswith(wanted):
                return {"req": requirement, "sha256": item["digests"]["sha256"]}
    raise ValueError(f"{requirement}: no pure-Python wheel on PyPI")


def environment(row: dict[str, Any]) -> tuple[dict[str, Any], str]:
    """The task's `env` (vendored deps, generated files) and the PYTHONPATH
    prefix its test commands use."""
    if row["repo"] == "django/django":
        return ({"deps": [pinned(r) for r in ["asgiref==3.8.1", "sqlparse==0.5.3"]]}, "PYTHONPATH=.:.eval-deps")
    if row["repo"] == "pytest-dev/pytest":
        major = row["version"].split(".")[0]
        files = {"src/_pytest/_version.py": f"version = \"{row['version']}.0\"\n"}
        return ({"deps": [pinned(r) for r in PYTEST_REPO_DEPS[major]], "files": files},
                "PYTHONPATH=src:.eval-deps")
    # Older sympy imports distutils (gone since Python 3.12): setuptools'
    # distutils shim is enabled by its .pth file, which sitecustomize loads.
    return ({"deps": [pinned(r) for r in ["mpmath==1.3.0", "setuptools==75.8.0", "py==1.11.0", *PYTEST_STACK]],
             "files": {".eval-deps/sitecustomize.py": SITECUSTOMIZE}}, "PYTHONPATH=.eval-deps")


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
        env, path = environment(row)
        hidden_cmd = f"{path} python3 -m pytest -q -p no:cacheprovider {files}"
        gate_cmd = f"{path} python3 -m pytest -q -p no:cacheprovider TODO"
        test_format = "junit"
        if row["repo"] == "django/django":
            labels = " ".join(f.removeprefix("tests/").removesuffix(".py").replace("/", ".")
                              for f in test_files(row["test_patch"]))
            runner = f"{path} python3 tests/runtests.py --settings=test_sqlite --parallel 1 -v 2"
            hidden_cmd, gate_cmd, test_format = f"{runner} {labels}", f"{runner} TODO", "unittest-verbose"
        task = {
            "id": instance, "source": "swebench-verified", "difficulty": row["difficulty"],
            "version": row["version"],
            "repo": f"https://github.com/{row['repo']}.git", "base": row["base_commit"],
            "env": env,
            "prompt": row["problem_statement"].strip() + INSTRUCTION,
            "gate": {"cmd": gate_cmd, "patch": "gate.patch"},
            "hidden": {"cmd": hidden_cmd, "patch": "hidden.patch", "format": test_format,
                       "fail_to_pass": json.loads(row["FAIL_TO_PASS"]),
                       "pass_to_pass": json.loads(row["PASS_TO_PASS"])},
            "status": "needs-gate",
        }
        (target / "task.json").write_text(json.dumps(task, indent=2) + "\n")
        print(target)
    return 0


def validate(args: argparse.Namespace) -> int:
    """SWE-bench's own conditions on the hidden tests (at the base every
    FAIL_TO_PASS test fails and every PASS_TO_PASS test passes; with the gold
    patch all pass) plus the gate's (fails at the base, passes with gold).
    Writes one evidence record per task to --evidence."""
    config = {"test_runner": args.runner}
    marsh = run.Marsh(args.marsh, dict(os.environ), 1800)
    failures = 0
    evidence = []
    for instance in args.ids:
        task_dir = (args.tasks / instance).resolve()
        task = json.loads((task_dir / "task.json").read_text())
        work = pathlib.Path(args.work).resolve() / "runs" / instance
        template = run.prepare_template(task_dir, task, pathlib.Path(args.work).resolve() / "templates")
        gold = (task_dir / "gold.patch").read_text()
        started = time.time()

        def gate(patch: str) -> dict[str, Any]:
            # A new path per call: the daemon refuses a mount that overlaps one
            # it has not released yet.
            project = work / run.fresh_name("gate-gold" if patch else "gate-base")
            run.fresh_clone(template, project)
            if patch:
                run.git(project, "apply", "-", stdin=patch.encode())
            done = marsh.call(["-c", run.wrap(config, task["gate"]["cmd"])], project, timeout=900)
            return {"exit": done.returncode, "tail": (done.stdout + done.stderr).decode(errors="replace")[-1500:]}

        cache: dict[str, Any] = {}
        base = run.score_hidden(marsh, task_dir, task, template, "", work, cache, config)
        fixed = run.score_hidden(marsh, task_dir, task, template, gold, work, cache, config)
        f2p = task["hidden"].get("fail_to_pass") or []
        result: dict[str, Any] = {
            "id": instance,
            "hidden_f2p_all_fail_at_base": set(base.get("fail_to_pass", {})) == set(f2p),
            "hidden_p2p_pass_at_base": not base.get("pass_to_pass") and not base.get("infra"),
            "hidden_passes_with_gold": bool(fixed.get("passed")),
        }
        if not args.hidden_only:
            at_base, with_gold = gate(""), gate(gold)
            result.update(gate_fails_at_base=at_base["exit"] != 0, gate_passes_with_gold=with_gold["exit"] == 0)
            result["gate_tails"] = {"base": at_base["tail"], "gold": with_gold["tail"]}
        result["valid"] = all(v for k, v in result.items() if isinstance(v, bool))
        result["details"] = {"base": {k: base.get(k) for k in ("fail_to_pass", "pass_to_pass", "infra", "exit")},
                             "gold": {k: fixed.get(k) for k in ("fail_to_pass", "pass_to_pass", "infra", "exit")},
                             "gold_tail": fixed.get("tail", "")[-1500:] if not fixed.get("passed") else ""}
        result["wall_s"] = round(time.time() - started, 1)
        failures += not result["valid"]
        evidence.append(result)
        print(json.dumps({k: v for k, v in result.items() if k not in ("details", "gate_tails")}), flush=True)
    if args.evidence:
        args.evidence.write_text(json.dumps({"runner": args.runner, "marsh": args.marsh_version or args.marsh,
                                             "validated": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                                             "tasks": evidence}, indent=2) + "\n")
    return 1 if failures else 0


def junit_ids(junit: pathlib.Path, files: list[str]) -> dict[str, str]:
    """{`path.py::Class::name`: status} from a pytest junit file."""
    modules = {f.removesuffix(".py").replace("/", "."): f for f in files}
    ids = {}
    for classname, name, status in run.junit_outcomes(junit):
        module = max((m for m in modules if classname == m or classname.startswith(m + ".")), key=len, default=None)
        if module is None:
            continue
        rest = classname[len(module):].lstrip(".")
        ids["::".join([modules[module], *([rest] if rest else []), name])] = status
    return ids


def self_made(args: argparse.Namespace) -> int:
    """A task from an upstream bug-fix commit: base = the fix's parent,
    hidden.patch = its test-file changes, gold.patch = the rest. The
    FAIL_TO_PASS / PASS_TO_PASS lists are computed SWE-bench's way, by running
    the hidden test files at the base and with gold (here, with --runner and
    --marsh; do it on the same Python as the Kits), then frozen in task.json."""
    target = args.tasks / args.id
    target.mkdir(parents=True, exist_ok=True)
    cache = pathlib.Path(args.work).resolve()
    mirror = cache / "mirrors" / re.sub(r"[^A-Za-z0-9]+", "_", args.repo)
    if not mirror.exists():
        mirror.parent.mkdir(parents=True, exist_ok=True)
        run.git(mirror.parent, "clone", "-q", "--filter=blob:none", "--no-checkout", args.repo, str(mirror))
    base = run.git(mirror, "rev-parse", f"{args.fix}^").stdout.decode().strip()
    changed = run.git(mirror, "diff", "--name-only", base, args.fix).stdout.decode().split()
    tests = [f for f in changed if any(f.startswith(p) for p in args.tests)]
    code = [f for f in changed if f not in tests]
    (target / "hidden.patch").write_bytes(run.git(mirror, "diff", "--binary", base, args.fix, "--", *tests).stdout)
    (target / "gold.patch").write_bytes(run.git(mirror, "diff", "--binary", base, args.fix, "--", *code).stdout)
    path = f"PYTHONPATH={args.pythonpath}"
    task: dict[str, Any] = {
        "id": args.id, "source": "self-made", "repo": args.repo, "base": base, "fix": args.fix,
        "merged": run.git(mirror, "show", "-s", "--format=%cs", args.fix).stdout.decode().strip(),
        "env": {"deps": [pinned(r) for r in [*args.deps, *PYTEST_STACK]]},
        "prompt": args.prompt.read_text().strip() + INSTRUCTION,
        "gate": {"cmd": f"{path} python3 -m pytest -q -p no:cacheprovider TODO", "patch": "gate.patch"},
        "hidden": {"cmd": f"{path} python3 -m pytest -q -p no:cacheprovider {' '.join(tests)}",
                   "patch": "hidden.patch", "format": "junit"},
        "status": "needs-gate",
    }
    (target / "task.json").write_text(json.dumps(task, indent=2) + "\n")
    config = {"test_runner": args.runner}
    marsh = run.Marsh(args.marsh, dict(os.environ), 1800)
    template = run.prepare_template(target.resolve(), task, cache / "templates")
    statuses = {}
    for label, patch in (("base", ""), ("gold", (target / "gold.patch").read_text())):
        scoring = cache / "runs" / args.id / run.fresh_name(label)
        run.fresh_clone(template, scoring)
        if patch:
            run.git(scoring, "apply", "-", stdin=patch.encode())
        run.git(scoring, "apply", str((target / "hidden.patch").resolve()))
        marsh.call(["-c", run.wrap(config, task["hidden"]["cmd"] + " --junitxml=.eval-junit.xml")], scoring)
        statuses[label] = junit_ids(scoring / ".eval-junit.xml", tests)
    base_s, gold_s = statuses["base"], statuses["gold"]
    task["hidden"]["fail_to_pass"] = sorted(t for t, s in gold_s.items() if s == "passed" and base_s.get(t) != "passed")
    task["hidden"]["pass_to_pass"] = sorted(t for t, s in gold_s.items() if s == "passed" and base_s.get(t) == "passed")
    (target / "task.json").write_text(json.dumps(task, indent=2) + "\n")
    print(json.dumps({"id": args.id, "base": base, "fail_to_pass": task["hidden"]["fail_to_pass"],
                      "pass_to_pass": len(task["hidden"]["pass_to_pass"]),
                      "gold_failures": sorted(t for t, s in gold_s.items() if s == "failed")}))
    return 0


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
    v.add_argument("--runner", default="shell -c {cmd}", help="test_runner, as in the run config")
    v.add_argument("--hidden-only", action="store_true", help="skip the gate (screening before gates exist)")
    v.add_argument("--evidence", type=pathlib.Path)
    v.add_argument("--marsh-version", help="recorded in the evidence")
    v.add_argument("ids", nargs="+")
    m = sub.add_parser("self-made")
    m.add_argument("--tasks", type=pathlib.Path, required=True)
    m.add_argument("--id", required=True)
    m.add_argument("--repo", required=True)
    m.add_argument("--fix", required=True, help="the upstream fix commit")
    m.add_argument("--tests", action="append", required=True, help="test path prefix (repeatable)")
    m.add_argument("--deps", action="append", default=[], help="extra pinned test dependency (repeatable)")
    m.add_argument("--pythonpath", default=".eval-deps")
    m.add_argument("--prompt", type=pathlib.Path, required=True, help="issue-style problem statement")
    m.add_argument("--marsh", default="marsh")
    m.add_argument("--runner", default="shell -c {cmd}")
    m.add_argument("--work", default="target/eval-validate")
    args = parser.parse_args(argv)
    if args.command == "self-made":
        return self_made(args)
    if args.command == "pool":
        args.repo = args.repo or ["sympy/sympy", "pytest-dev/pytest"]
        args.difficulty = args.difficulty or ["<15 min fix", "15 min - 1 hour"]
        return pool(args)
    return fetch(args) if args.command == "fetch" else validate(args)


if __name__ == "__main__":
    sys.exit(main())
