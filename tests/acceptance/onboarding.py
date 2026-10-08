#!/usr/bin/env python3
"""Credential-free subprocess checks of the shipped CLI and the Brush artifact.

This does not create a VM, contact a provider, or qualify the Mac/stock-SBX
product. marsh-local runs with the caller's OS authority in a temporary project.
"""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import platform
import signal
import subprocess
import tempfile
import time


DEMO = "printf 'pear\\napple\\npear\\n' | sort -u | tee sorted.txt"


def identity(path):
    return {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}


def run_case(name, argv, cwd, env, expected_code, stdout, stderr=b"", stdin=b""):
    started = time.monotonic()
    # The tested programs have bounded output. A new process group permits
    # cleanup of descendants too if a shell regression hangs the journey.
    with subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.PIPE,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          start_new_session=True) as child:
        timed_out = False
        try:
            out, err = child.communicate(stdin, timeout=20)
        except subprocess.TimeoutExpired:
            timed_out = True
            os.killpg(child.pid, signal.SIGKILL)
            out, err = child.communicate()
        result = {
            "name": name, "argv": [str(arg) for arg in argv],
            "exit_code": child.returncode, "expected_exit_code": expected_code,
            "stdout_base64": base64.b64encode(out).decode(),
            "stderr_base64": base64.b64encode(err).decode(),
            "stdin_sha256": hashlib.sha256(stdin).hexdigest(),
            "elapsed_seconds": time.monotonic() - started,
            "timed_out": timed_out,
        }
    result["passed"] = (not timed_out and result["exit_code"] == expected_code
                        and (stdout(out) if callable(stdout) else out == stdout)
                        and (stderr(err) if callable(stderr) else err == stderr))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--marsh", type=Path, required=True, help="built host CLI")
    parser.add_argument("--shell", type=Path, required=True, help="built marsh-local (Brush) artifact")
    parser.add_argument("--evidence", type=Path, help="write exact observed streams and binary hashes")
    args = parser.parse_args()
    marsh, shell = args.marsh.resolve(strict=True), args.shell.resolve(strict=True)
    for binary in (marsh, shell):
        if not binary.is_file() or not os.access(binary, os.X_OK):
            parser.error(f"not an executable file: {binary}")
    report = {
        "schema": "marsh.onboarding/v1", "platform": platform.platform(),
        "qualification": "credential-free CLI and Brush-artifact subprocesses only; no VM or live provider",
        "binaries": {"marsh": identity(marsh), "shell": identity(shell)},
        "cases": [],
    }
    with tempfile.TemporaryDirectory(prefix="marsh-onboarding-") as temporary:
        root = Path(temporary)
        home, project = root / "home", root / "project with spaces"
        home.mkdir()
        project.mkdir()
        # Do not inherit BASH_ENV, MARSH session credentials, provider secrets,
        # real dotfiles, or a developer's command shims.
        env = {"HOME": str(home), "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
               "LC_ALL": "C", "TERM": "dumb", "TMPDIR": str(root),
               "MARSH_HOME": str(root / "unused-scope")}
        cases = report["cases"]
        cases.append(run_case("public-help", [marsh, "--help"], project, env, 0,
                              lambda out: out.startswith(b"marsh - ") and b"Usage:" in out))
        cases.append(run_case("public-version", [marsh, "--version"], project, env, 0,
                              lambda out: out.startswith(b"marsh ") and out.endswith(b"\n")))
        cases.append(run_case("missing-load-argument", [marsh, "--load"],
                              project, env, 2, b"",
                              lambda err: b"--load requires KIT[,KIT...] or all" in err))
        prefix = [shell, "--no-config", "--noprofile", "--norc", "-c"]
        cases.append(run_case("ordinary-pipeline-demo", prefix + [DEMO], project, env,
                              0, b"apple\npear\n"))
        sorted_file = project / "sorted.txt"
        cases[-1]["file_matches_stdout"] = (sorted_file.is_file()
                                                 and sorted_file.read_bytes() == b"apple\npear\n")
        cases[-1]["passed"] &= cases[-1]["file_matches_stdout"]
        cases.append(run_case("separate-streams-and-status", prefix + [
            "printf 'result\\n'; printf 'diagnostic\\n' >&2; exit 23"],
            project, env, 23, b"result\n", b"diagnostic\n"))
        payload = bytes(range(256)) * 257
        cases.append(run_case("binary-stdin-pipeline", prefix + ["cat | cat"],
                              project, env, 0, payload, stdin=payload))
        for pipefail, code in ((False, 0), (True, 23)):
            script = ("set -o pipefail; " if pipefail else "") + \
                     "(printf 'partial\\n'; exit 23) | cat"
            cases.append(run_case(f"pipeline-pipefail-{pipefail}", prefix + [script],
                                  project, env, code, b"partial\n"))
        cases.append(run_case("background-wait-status", prefix + [
            "(sleep 0.01; exit 7) & child=$!; wait \"$child\"; code=$?; "
            "printf 'wait=%s\\n' \"$code\"; exit \"$code\""],
            project, env, 7, b"wait=7\n"))
        cases.append(run_case("fanout-ordered-bytes", prefix + [
            "printf 'input\\n' | fanout { copy: cat; upper: tr a-z A-Z } | collect"],
            project, env, 0,
            b"== copy (complete) ==\ninput\n\n== upper (complete) ==\nINPUT\n\n"))
        cases.append(run_case("paths-and-selected-test-home", prefix + [
            "printf '%s\\n%s\\n' \"$PWD\" \"$HOME\""], project, env, 0,
            f"{project}\n{home}\n".encode()))
        report["host_cli_created_no_scope"] = not (root / "unused-scope").exists()
    report["passed"] = report["host_cli_created_no_scope"] and all(
        case["passed"] for case in report["cases"])
    for case in report["cases"]:
        print(f"{'PASS' if case['passed'] else 'FAIL'} {case['name']} (exit {case['exit_code']})")
        if not case["passed"]:
            print(json.dumps(case, indent=2))
    if args.evidence:
        args.evidence.parent.mkdir(parents=True, exist_ok=True)
        args.evidence.write_text(json.dumps(report, indent=2) + "\n")
    print(report["qualification"])
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
