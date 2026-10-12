#!/usr/bin/env python3
"""Run every VM-free Bash differential suite: marsh's shell against GNU Bash 5.3.

Each bash_*.py suite drives the built `marsh-local` binary (Brush with marsh's
patches, no VM, no daemon) and a separately installed GNU Bash 5.3 through
their public command lines and real PTYs, and compares bytes, statuses and file
effects. GNU Bash is only the oracle; marsh never runs it.

  cargo build --locked -p marsh --bins
  python3 tests/acceptance/bash_suites.py --candidate target/debug/marsh-local \\
      --oracle "$(command -v bash)" --evidence /tmp/bash-suites

Needs Linux or macOS, GNU Bash 5.3 (`--oracle`, never found on PATH implicitly
by the suites), python3 and a C compiler (`cc`, for the byte-builtins native
child). Each suite's evidence lands in EVIDENCE/<suite>/; the summary lists
every suite's verdict. Exit status is nonzero when any suite fails.
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


def pair(flag_candidate: str = "--candidate", flag_oracle: str = "--oracle", flag_evidence: str = "--evidence"):
    def build(candidate: str, oracle: str, evidence: pathlib.Path) -> list[str]:
        return [flag_candidate, candidate, flag_oracle, oracle, flag_evidence, str(evidence)]
    return build


def labelled(candidate: str, oracle: str, evidence: pathlib.Path) -> list[str]:
    # The first --shell is the oracle; --brush labels get Brush's --no-config.
    return ["--shell", f"gnu={oracle}", "--shell", f"candidate={candidate}", "--brush", "candidate",
            "--require-exact", "candidate", "--evidence", str(evidence)]


SUITES: dict[str, tuple[str, object]] = {
    "byte-builtins": ("bash_byte_builtins.py", pair()),
    "byte-matchers": ("bash_byte_matchers.py", pair()),
    "completion": ("bash_completion_cli.py", pair(flag_oracle="--reference")),
    "core-completion": ("bash_core_completion.py", pair()),
    "disown": ("bash_disown.py", pair(flag_evidence="--out")),
    "external-redirects": ("bash_external_redirects.py", pair(flag_oracle="--gnu")),
    "history-input": ("bash_history_input_cli.py", labelled),
    "jobs": ("bash_jobs_cli.py", labelled),
    "lossless-bytes": ("bash_lossless_bytes.py", pair()),
    "native-state": ("bash_native_state.py", pair(flag_evidence="--out")),
    "nul-input": ("bash_nul_input.py", pair()),
    "prompt": ("bash_prompt_cli.py", labelled),
    "readiness": ("bash_readiness.py", pair()),
    "readline": ("bash_readline_cli.py", labelled),
    "regex-limits": ("bash_regex_limits.py", pair()),
    "startup": ("bash_startup_cli.py", labelled),
}


# Suites that take the oracle's exact version (they record whether it is the
# 5.3.20 their cases were first written against).
ORACLE_VERSION_FLAG = {"prompt", "readline", "startup"}


def run_suite(name: str, candidate: str, oracle: str, version: str, root: pathlib.Path, timeout: float) -> dict:
    script, build = SUITES[name]
    evidence = root / name
    log = root / f"{name}.log"
    argv = [sys.executable, str(DIRECTORY / script), *build(candidate, oracle, evidence)]
    if name in ORACLE_VERSION_FLAG:
        argv += ["--oracle-version", version]
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
    seconds = round(time.monotonic() - started, 1)
    tail = [line for line in log.read_text(errors="replace").splitlines() if line.strip()][-3:]
    return {"suite": name, "script": script, "status": status, "seconds": seconds,
            "outcome": "passed" if status == 0 else "timeout" if status is None else "failed",
            "log": str(log), "tail": tail}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--candidate", required=True, help="built marsh-local")
    parser.add_argument("--oracle", required=True, help="GNU Bash 5.3 executable")
    parser.add_argument("--evidence", required=True, type=pathlib.Path, help="fresh directory for evidence")
    parser.add_argument("--only", action="append", choices=sorted(SUITES), help="repeatable")
    parser.add_argument("--timeout", type=float, default=2400, help="per-suite seconds")
    parser.add_argument("--jobs", type=int, default=4, help="suites run concurrently (default 4)")
    args = parser.parse_args()
    candidate = str(pathlib.Path(args.candidate).resolve(strict=True))
    oracle = shutil.which(args.oracle)
    if oracle is None:
        parser.error(f"--oracle is not executable: {args.oracle}")
    oracle = str(pathlib.Path(oracle).resolve())
    version = subprocess.run([oracle, "--version"], capture_output=True, timeout=10).stdout
    match = re.match(rb"GNU bash, version (5\.3\.\d+)\(", version)
    if match is None:
        parser.error(f"--oracle must be GNU Bash 5.3: {version[:60]!r}")
    number = match.group(1).decode()
    root = args.evidence.resolve()
    if root.exists() and any(root.iterdir()):
        parser.error(f"--evidence must be a fresh directory: {root}")
    root.mkdir(parents=True, exist_ok=True)
    results = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        futures = [pool.submit(run_suite, name, candidate, oracle, number, root, args.timeout)
                   for name in args.only or SUITES]
        for future in concurrent.futures.as_completed(futures):
            result = future.result()
            results.append(result)
            print(f"{result['outcome'].upper():7} {result['suite']:20} {result['seconds']:7.1f}s"
                  + ("" if result["outcome"] == "passed" else f"  {result['log']}"), flush=True)
    results.sort(key=lambda r: list(SUITES).index(r["suite"]))
    failed = [r["suite"] for r in results if r["outcome"] != "passed"]
    (root / "summary.json").write_text(json.dumps({"oracle_version": version.decode(errors="replace").splitlines()[0],
                                                   "candidate": candidate, "results": results}, indent=2) + "\n")
    print(f"bash suites: {len(results) - len(failed)}/{len(results)} passed; evidence: {root}")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
