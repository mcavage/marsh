#!/usr/bin/env python3
"""Exact public-CLI regression gate for core Bash completion repairs.

Run with distinct candidate/GNU Bash 5.3.20 binaries and a candidate source receipt.
Fixtures retain their upstream group/index and annotations, but NO ignore_stdout,
ignore_stderr, known_failure, whitespace, or status waivers are applied. Interactive
cases belong to the separate owned-PTY suite. This is not release/stock qualification.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import signal
import subprocess
import tempfile
import time


FIXTURES = Path(__file__).with_name("fixtures") / "bash_core_completion.json"


def sha(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def snapshot(root: Path) -> dict:
    result = {}
    for directory, dirs, files in os.walk(os.fsencode(root)):
        for name in sorted(dirs + files):
            path = os.path.join(directory, name)
            key = os.path.relpath(path, os.fsencode(root)).hex()
            if os.path.islink(path):
                result[key] = {"symlink_hex": os.readlink(path).hex()}
            elif os.path.isfile(path):
                with open(path, "rb") as stream:
                    result[key] = {"bytes_hex": stream.read().hex(),
                                   "mode": os.stat(path).st_mode & 0o777}
            elif os.path.isdir(path):
                result[key] = {"directory_mode": os.stat(path).st_mode & 0o777}
    return result


def run(binary: Path, case: dict, work: Path) -> dict:
    # work is exclusively beneath the TemporaryDirectory created by this runner.
    if work.exists():
        shutil.rmtree(work)
    work.mkdir()
    for fixture in case.get("test_files", []):
        relative = PurePosixPath(fixture["path"])
        if relative.is_absolute() or ".." in relative.parts:
            raise ValueError("fixture escapes owned directory")
        path = work / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(bytes.fromhex(fixture["contents_hex"]) if "contents_hex" in fixture
                         else fixture.get("contents", "").encode("utf-8"))
        path.chmod(0o744 if fixture.get("executable") else 0o644)
    home = work.parent / "shell-home"
    if home.exists():
        shutil.rmtree(home)  # sibling inside this runner's owned TemporaryDirectory
    home.mkdir(mode=0o700)
    history = home / "history"
    descriptor = os.open(history, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    os.close(descriptor)
    environment = {b"PATH": b"/usr/bin:/bin", b"LC_ALL": b"C", b"PS1": b"test$ ",
                   b"HOME": os.fsencode(home), b"HISTFILE": os.fsencode(history)}
    environment.update({os.fsencode(k): os.fsencode(v) for k, v in case.get("env", {}).items()})
    if "home_dir" in case:
        environment[b"HOME"] = os.fsencode(work / case["home_dir"])
    flags = [flag for flag in ("--norc", "--noprofile")
             if flag not in case.get("removed_default_args", [])]
    arguments = ([bytes.fromhex(value) for value in case["args_hex"]] if "args_hex" in case
                 else [os.fsencode(value) for value in case.get("args", [])])
    argv = [b"marsh-tck"] + [os.fsencode(flag) for flag in flags] + arguments
    source = (bytes.fromhex(case["stdin_hex"]) if "stdin_hex" in case
              else case.get("stdin", "").encode("utf-8"))
    started = time.monotonic()
    with tempfile.TemporaryFile() as input_file:
        input_file.write(source)
        input_file.seek(0)
        process = subprocess.Popen(argv, executable=os.fsencode(binary), cwd=work,
                                   env=environment, stdin=input_file,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   start_new_session=True)
        timed_out = False
        try:
            out, err = process.communicate(timeout=12)
        except subprocess.TimeoutExpired:
            timed_out = True
            # Signal only this runner's recorded process group, never a global scan.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            out, err = process.communicate()
    return {"status": process.returncode, "stdout_hex": out.hex(), "stderr_hex": err.hex(),
            "effects": snapshot(work), "timeout": timed_out,
            "pid": process.pid, "pgid": process.pid, "elapsed": time.monotonic() - started}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--oracle", required=True, type=Path)
    parser.add_argument("--source-manifest", required=True, type=Path)
    parser.add_argument("--evidence", required=True, type=Path)
    parser.add_argument("--cases", nargs="+", help="Optional group.yaml#index subset")
    parser.add_argument("--fixtures", type=Path, default=FIXTURES)
    args = parser.parse_args()
    definitions = json.loads(args.fixtures.read_text())
    cases = definitions["cases"]
    if args.cases:
        wanted = set(args.cases)
        cases = [case for case in cases if f"{case['group']}#{case['index']}" in wanted]
        if len(cases) != len(wanted):
            parser.error("unknown or duplicate case identifiers")
    if any(case.get("pty") or "-i" in case.get("args", [])
           or "2d69" in case.get("args_hex", []) for case in cases):
        parser.error("interactive cases require the separate PTY harness")
    binaries = {"oracle": args.oracle.resolve(), "candidate": args.candidate.resolve()}
    identity = {"artifacts": {role: {"path": str(path), "sha256": sha(path)}
                              for role, path in binaries.items()},
                "harness_sha256": sha(Path(__file__)), "fixtures_sha256": sha(args.fixtures),
                "source_manifest_sha256": sha(args.source_manifest),
                "qualification": "local public CLI only; not independent review or stock approval"}
    if identity["artifacts"]["oracle"]["sha256"] == identity["artifacts"]["candidate"]["sha256"]:
        parser.error("oracle and candidate must be distinct artifacts")
    receipt = json.loads(args.source_manifest.read_bytes())
    if "sha256" in receipt and receipt["sha256"] != identity["artifacts"]["candidate"]["sha256"]:
        parser.error("candidate does not match source receipt artifact")
    if "source_before" in receipt and receipt["source_before"] != receipt.get("source_after"):
        parser.error("source receipt changed during build")
    args.evidence.mkdir(parents=True, exist_ok=False)
    (args.evidence / "source-manifest.json").write_bytes(args.source_manifest.read_bytes())
    passed = 0
    with tempfile.TemporaryDirectory(prefix="marsh-core-cli-") as directory:
        work = Path(directory) / "fixture"
        oracle_probe = run(binaries["oracle"], {"stdin": 'printf "%s" "$BASH_VERSION"\n'}, work)
        (args.evidence / "oracle-version.json").write_text(json.dumps(oracle_probe, indent=2))
        if (oracle_probe["status"] != 0 or oracle_probe["stderr_hex"]
                or oracle_probe["timeout"]
                or not bytes.fromhex(oracle_probe["stdout_hex"]).startswith(b"5.3.20(")):
            (args.evidence / "identity.json").write_text(json.dumps(identity, indent=2))
            raise RuntimeError("invalid GNU 5.3.20 oracle; observations retained")
        with (args.evidence / "observations.jsonl").open("w") as output:
            for case in cases:
                rows = {role: run(binary, case, work) for role, binary in binaries.items()}
                fields = ("status", "stdout_hex", "stderr_hex", "effects")
                differences = [field for field in fields if rows["oracle"][field] != rows["candidate"][field]]
                if any(row["timeout"] for row in rows.values()):
                    differences.append("timeout")
                passed += not differences
                output.write(json.dumps({"case": case, "runs": rows, "differences": differences}) + "\n")
                output.flush()
    stable = all(sha(binary) == identity["artifacts"][role]["sha256"] for role, binary in binaries.items())
    identity.update(total=len(cases), passed=passed, stable_binaries=stable)
    (args.evidence / "identity.json").write_text(json.dumps(identity, indent=2))
    print(json.dumps({key: identity[key] for key in ("total", "passed", "stable_binaries")}))
    return 0 if passed == len(cases) and stable else 1


if __name__ == "__main__":
    raise SystemExit(main())
