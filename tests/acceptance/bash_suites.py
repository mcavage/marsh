#!/usr/bin/env python3
"""Run every VM-free Bash differential suite and gate on classified divergences.

Each bash_*.py suite drives the built `marsh-local` binary (Brush with marsh's
patches, no VM, no daemon) and GNU Bash through their public command lines and
real PTYs, compares bytes, statuses and file effects case by case, and writes a
verdict per case (bash_report.py). GNU Bash is only the oracle; marsh never
runs it.

  cargo build --locked -p marsh --bins
  python3 tests/acceptance/bash_suites.py --candidate target/debug/marsh-local \\
      --oracle "$(command -v bash)" --evidence /tmp/bash-suites

The run passes only when every mismatching case is covered by an entry in
bash_divergences.json (whose ids match docs/design/bash-compatibility.md), no
entry lists a case that now matches (stale), GNU Bash meets every case's own
expectation, and no suite reports a harness error. --draft PATH writes the
unclassified mismatches in the divergences file's shape, to classify them.

Linux only (the suites create non-UTF-8 file names, which macOS rejects).
The oracle is pinned to GNU Bash 5.3.0, the release CI builds; python3 and a C
compiler (`cc`) are also needed.
"""
from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import pathlib
import re
import shutil
import signal
import subprocess
import sys
import time

DIRECTORY = pathlib.Path(__file__).resolve().parent
DIVERGENCES = DIRECTORY / "bash_divergences.json"
ORACLE_VERSION = "5.3.0"


def plain(candidate: str, oracle: str, evidence: pathlib.Path) -> list[str]:
    return ["--candidate", candidate, "--oracle", oracle, "--evidence", str(evidence)]


def labelled(candidate: str, oracle: str, evidence: pathlib.Path) -> list[str]:
    # The first --shell is the oracle; --brush labels get Brush's --no-config.
    return ["--shell", f"gnu={oracle}", "--shell", f"candidate={candidate}", "--brush", "candidate",
            "--require-exact", "candidate", "--evidence", str(evidence)]


def versioned(candidate: str, oracle: str, evidence: pathlib.Path) -> list[str]:
    return labelled(candidate, oracle, evidence) + ["--oracle-version", ORACLE_VERSION]


# suite -> (script, argv builder, shards: extra arguments per concurrently run shard)
SUITES: dict[str, tuple[str, object, dict[str, list[str]]]] = {
    "bytes": ("bash_lossless_bytes.py", plain, {}),
    "byte-builtins": ("bash_byte_builtins.py", plain, {}),
    "byte-matchers": ("bash_byte_matchers.py", plain, {}),
    "completion": ("bash_completion_cli.py", plain, {}),
    "disown": ("bash_disown.py", plain, {}),
    "external-redirects": ("bash_external_redirects.py", plain, {}),
    "history-input": ("bash_history_input_cli.py", labelled, {
        "context": ["--suite", "context-pipe", "--suite", "context-pty", "--suite", "context-basic"],
        "edit": ["--suite", "policy", "--suite", "edit", "--suite", "paste"],
        "hup": ["--suite", "hup"],
        "input": ["--suite", "nul", "--suite", "eof"],
    }),
    "jobs": ("bash_jobs_cli.py", labelled, {f"{k}": ["--shard", f"{k}/3"] for k in range(3)}),
    "nul-input": ("bash_nul_input.py", plain, {}),
    "prompt": ("bash_prompt_cli.py", versioned, {}),
    "readiness": ("bash_readiness.py", plain, {}),
    "readline": ("bash_readline_cli.py", versioned, {f"{k}": ["--shard", f"{k}/4"] for k in range(4)}),
    "regex": ("bash_regex.py", plain, {}),
    "startup": ("bash_startup_cli.py", versioned, {}),
    "tck": ("bash_tck.py", plain, {}),
}


def run_shard(name: str, shard: str | None, extra: list[str], candidate: str, oracle: str,
              root: pathlib.Path, timeout: float) -> dict:
    script, build, _ = SUITES[name]
    label = name if shard is None else f"{name}.{shard}"
    evidence = root / name if shard is None else root / name / shard
    log = root / f"{label}.log"
    argv = [sys.executable, str(DIRECTORY / script), *build(candidate, oracle, evidence), *extra]
    started = time.monotonic()
    with log.open("wb") as output:
        process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT,
                                   start_new_session=True)
        try:
            status = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            status = None
    verdicts, errors = {}, []
    report = evidence / "cases.json"
    if status is None:
        errors.append(f"timed out after {timeout:.0f}s")
    elif not report.exists():
        errors.append(f"exited {status} without a case report")
    else:
        data = json.loads(report.read_text())
        verdicts, errors = data["cases"], data["errors"]
        if status not in (0, 1):
            errors.append(f"exited {status}")
    return {"suite": name, "shard": shard, "seconds": round(time.monotonic() - started, 1),
            "log": str(log), "verdicts": verdicts, "errors": errors}


def load_divergences(path: pathlib.Path) -> tuple[list[dict], dict[tuple[str, str], str], list[str]]:
    entries = json.loads(path.read_text())["divergences"]
    owner, problems = {}, []
    for entry in entries:
        if not re.fullmatch(r"[A-Z][A-Z0-9-]+", entry["id"]):
            problems.append(f"{entry['id']}: ids are upper-case words (as in the doc)")
        for suite, cases in entry["cases"].items():
            if suite not in SUITES:
                problems.append(f"{entry['id']}: unknown suite {suite}")
            for case in cases:
                if (suite, case) in owner:
                    problems.append(f"{suite} {case}: in both {owner[suite, case]} and {entry['id']}")
                owner[suite, case] = entry["id"]
    return entries, owner, problems


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--candidate", required=True, help="built marsh-local")
    parser.add_argument("--oracle", required=True, help=f"GNU Bash {ORACLE_VERSION} executable")
    parser.add_argument("--evidence", required=True, type=pathlib.Path, help="fresh directory for evidence")
    parser.add_argument("--only", action="append", choices=sorted(SUITES), help="repeatable")
    parser.add_argument("--divergences", type=pathlib.Path, default=DIVERGENCES)
    parser.add_argument("--draft", type=pathlib.Path, help="write unclassified mismatches here")
    parser.add_argument("--timeout", type=float, default=900, help="per-shard seconds")
    parser.add_argument("--jobs", type=int, default=max(2, (os.cpu_count() or 2) + 2),
                        help="shards run concurrently (default: CPUs + 2; most wait on child shells)")
    args = parser.parse_args()
    if sys.platform != "linux":
        parser.error("Linux only: the suites create non-UTF-8 file names")
    candidate = str(pathlib.Path(args.candidate).resolve(strict=True))
    oracle = shutil.which(args.oracle)
    if oracle is None:
        parser.error(f"--oracle is not executable: {args.oracle}")
    oracle = str(pathlib.Path(oracle).resolve())
    version = subprocess.run([oracle, "--version"], capture_output=True, timeout=10).stdout
    if not version.startswith(f"GNU bash, version {ORACLE_VERSION}(".encode()):
        parser.error(f"--oracle must be GNU Bash {ORACLE_VERSION} (the bash-5.3 release CI builds): "
                     f"{version[:60]!r}")
    entries, owner, problems = load_divergences(args.divergences)
    root = args.evidence.resolve()
    if root.exists() and any(root.iterdir()):
        parser.error(f"--evidence must be a fresh directory: {root}")
    root.mkdir(parents=True, exist_ok=True)
    selected = args.only or list(SUITES)
    # Longest first, so the shards that set the wall time start at once.
    work = [(name, shard, extra) for name in selected
            for shard, extra in (SUITES[name][2].items() or [(None, [])])]
    work.sort(key=lambda item: item[0] not in ("readline", "history-input", "jobs"))
    results = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        futures = [pool.submit(run_shard, name, shard, extra, candidate, oracle, root, args.timeout)
                   for name, shard, extra in work]
        for future in concurrent.futures.as_completed(futures):
            result = future.result()
            results.append(result)
            label = result["suite"] + ("" if result["shard"] is None else "." + result["shard"])
            counts = {}
            for verdict in result["verdicts"].values():
                counts[verdict] = counts.get(verdict, 0) + 1
            print(f"{label:24} {result['seconds']:7.1f}s  " + " ".join(f"{k}={v}" for k, v in sorted(counts.items()))
                  + (f"  ERRORS: {result['errors']}" if result["errors"] else ""), flush=True)

    verdicts = {}
    errors = list(problems)
    for result in results:
        for case, verdict in result["verdicts"].items():
            if (result["suite"], case) in verdicts:
                errors.append(f"{result['suite']} {case}: reported twice")
            verdicts[result["suite"], case] = verdict
        errors += [f"{result['suite']}: {error}" for error in result["errors"]]
    mismatches = {key for key, verdict in verdicts.items() if verdict == "mismatch"}
    oracle_failures = sorted(key for key, verdict in verdicts.items() if verdict == "oracle")
    unclassified = sorted(mismatches - owner.keys())
    stale = sorted(key for key in owner if key[0] in selected and key not in mismatches)
    report = {"oracle_version": version.decode(errors="replace").splitlines()[0], "candidate": candidate,
              "cases": len(verdicts), "mismatches": len(mismatches), "classified": len(mismatches & owner.keys()),
              "unclassified": [list(key) for key in unclassified], "stale": [list(key) + [owner[key]] for key in stale],
              "oracle_failures": [list(key) for key in oracle_failures], "errors": errors,
              "seconds": {r["suite"] + ("" if r["shard"] is None else "." + r["shard"]): r["seconds"] for r in results}}
    (root / "summary.json").write_text(json.dumps(report, indent=2) + "\n")
    if args.draft:
        draft = {}
        for suite, case in unclassified:
            draft.setdefault(suite, []).append(case)
        args.draft.write_text(json.dumps({"divergences": [{"id": "UNCLASSIFIED", "cases": draft}]}, indent=1) + "\n")
    for suite, case in unclassified:
        print(f"UNCLASSIFIED {suite} {case}")
    for suite, case in stale:
        print(f"STALE        {suite} {case} ({owner[suite, case]} no longer reproduces; remove it)")
    for suite, case in oracle_failures:
        print(f"ORACLE       {suite} {case} (GNU Bash fails the case's own expectation)")
    for error in errors:
        print(f"ERROR        {error}")
    failed = bool(unclassified or stale or oracle_failures or errors)
    print(f"bash suites: {len(verdicts)} cases, {len(mismatches)} classified divergences"
          if not failed else
          f"bash suites FAILED: {len(unclassified)} unclassified, {len(stale)} stale, "
          f"{len(oracle_failures)} oracle, {len(errors)} errors", f"(evidence: {root})")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
