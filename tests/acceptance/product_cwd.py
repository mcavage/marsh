#!/usr/bin/env python3
"""Actual Mac public cwd routing proof; the caller supplies an isolated prepared fixture.

Uses public marsh/Kit paths only. --cloud is explicit and billed; this script does
not publish Kits or enable Cloud. The caller owns exact scope/VM cleanup afterward.
A raw-name filesystem failure is retained as FAIL, never normalized or waived.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import uuid


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def frame(values):
    return b"".join(len(value).to_bytes(4, "big") + value for value in values)


def run(binary, argv, directory, environment, timeout=120):
    with tempfile.TemporaryFile() as output, tempfile.TemporaryFile() as errors:
        try:
            child = subprocess.Popen([os.fsencode(binary), *argv], cwd=directory, env=environment,
                                     stdin=subprocess.DEVNULL, stdout=output, stderr=errors,
                                     start_new_session=True)
        except OSError as error:
            return {"status": None, "timeout": False, "lengths": [0, 0], "bounded": True,
                    "stdout_hex": "", "stderr_hex": "", "spawn_errno": error.errno}
        timed_out = False
        try:
            child.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            os.killpg(child.pid, signal.SIGKILL)
            child.wait(timeout=10)
        lengths = [output.tell(), errors.tell()]
        output.seek(0)
        errors.seek(0)
        stdout, stderr = output.read(1024 * 1024), errors.read(1024 * 1024)
    return {"status": child.returncode, "timeout": timed_out, "lengths": lengths,
            "stdout_hex": stdout.hex(), "stderr_hex": stderr.hex(),
            "bounded": max(lengths) <= 1024 * 1024}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--marsh", type=Path, required=True)
    parser.add_argument("--build-proof", type=Path, required=True)
    parser.add_argument("--scope-home", type=Path, required=True)
    parser.add_argument("--control-home", type=Path, required=True)
    parser.add_argument("--work-root", type=Path, required=True)
    parser.add_argument("--fixture", required=True)
    parser.add_argument("--cloud", action="store_true")
    parser.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("public stock/Cloud proof is root-run on the Mac host, not this VM")
    if not re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_-]{0,127}", args.fixture):
        parser.error("fixture must be a checked ASCII registered command")
    binary = args.marsh.resolve(strict=True)
    proof = args.build_proof.resolve(strict=True)
    before = sha(binary)
    environment = os.environb.copy()
    environment[b"MARSH_HOME"] = os.fsencode(args.scope_home.resolve())
    environment[b"MARSH_CONTROL_HOME"] = os.fsencode(args.control_home.resolve())
    prefix = [b"--noprofile", b"--norc"]
    marker = ("cwd-proof-" + uuid.uuid4().hex).encode("ascii")
    payload = b"cwd-roundtrip-\xff\xfe\xef\xbf\xbd"
    report = {"schema": 1, "harness_sha256": sha(__file__), "binary_before": before,
              "build_proof": str(proof), "build_proof_sha256": sha(proof),
              "cases": [], "queries": [], "cloud_requested": args.cloud,
              "cleanup": "operator owns isolated stock scope; only test-created project is temporary"}
    with tempfile.TemporaryDirectory(prefix="product-cwd-", dir=args.work_root.resolve(strict=True)) as directory:
        project = os.fsencode(Path(directory).resolve())
        for placement in ([b"local", b"cloud"] if args.cloud else [b"local"]):
            for location in [b"project", b"home"]:
                for kind, leaf in [("ascii", b"child"), ("raw", b"child-\xff\xfe")]:
                    for route in [b"shim", b"descendant"]:
                        unique = marker + b"-" + placement + b"-" + location + b"-" + route + b"-" + kind.encode()
                        child_dir = unique + b"/" + leaf
                        filename = b"result" if kind == "ascii" else b"result-\xfe"
                        root = project if location == b"project" else environment[b"HOME"]
                        expected_cwd = root + b"/" + child_dir
                        command = b'"$4"' if route == b"shim" else b'/usr/bin/env "$4"'
                        source = (b'set -e; if [ "$1" = home ]; then base=$HOME; else base=$(pwd -P); fi; '
                                  b'mkdir -p -- "$base/$2"; cd -- "$base/$2"; '
                                  b'export PWD=/deliberately-forged-cwd MARSH_PLACE=$3; '
                                  + command + b' raw-cwd "$5" "$6"; /usr/bin/cat -- "$5"; '
                                  b'/usr/bin/rm -- "$5"; cd -- "$base"; /usr/bin/rmdir -- "$2" "${2%/*}"')
                        argv = prefix + [b"-c", source, b"probe", location, child_dir,
                                         placement, args.fixture.encode(), filename, payload]
                        result = run(binary, argv, directory, environment)
                        expected = frame([expected_cwd, payload]) + payload
                        passed = result["bounded"] and not result["timeout"] and result["status"] == 0 and bytes.fromhex(result["stdout_hex"]) == expected
                        report["cases"].append({"case": b"/".join([placement, location, route, kind.encode()]).decode(),
                                                "expected_cwd_hex": expected_cwd.hex(),
                                                "expected_stdout_hex": expected.hex(), "pass": passed, **result})
        # No outside host path canary: if wrongly admitted, this fixture mode
        # only echoes synthetic args/env. No file-writing mode is requested.
        def jobs():
            result = run(binary, [b"jobs", b"--json"], directory, environment)
            report["queries"].append(result)
            if result["status"] != 0 or result["timeout"] or not result["bounded"]:
                return None
            try:
                return {job["job_id"] for job in json.loads(bytes.fromhex(result["stdout_hex"]))["jobs"]}
            except (ValueError, KeyError, TypeError):
                return None
        for placement in ([b"local", b"cloud"] if args.cloud else [b"local"]):
            previous = jobs()
            source = b'cd /tmp || exit; export PROBE_VALUE=synthetic MARSH_PLACE=$1; "$2" raw-bytes denied'
            result = run(binary, prefix + [b"-c", source, b"probe", placement, args.fixture.encode()], directory, environment)
            after = jobs()
            report["cases"].append({"case": placement.decode() + "/outside-grants", "pass": result["status"] == 125 and not result["timeout"] and result["stdout_hex"] == "" and previous is not None and after is not None and previous == after,
                                    "new_job_ids": sorted(after - previous) if previous is not None and after is not None else None, **result})
        # Smoke equal ordinary shell bounds. Failures belong to bridge/root,
        # never a byte-only quota waiver. These do not invoke a Kit or Cloud.
        for kind, raw in [("utf8", False), ("raw", True)]:
            source = b"#" + b"A" * 20_000 + (b"\xff\xfe" if raw else b"AA") + b"\nprintf '%s' large-ok"
            result = run(binary, prefix + [b"-c", source], directory, environment)
            report["cases"].append({"case": kind + "/ordinary-c-20KiB", "pass": result["status"] == 0 and bytes.fromhex(result["stdout_hex"]) == b"large-ok" and not result["timeout"], **result})
            words = [str(index).encode() + b"A" * (32 * 1024 - 2) + (b"\xff" if raw else b"A") for index in range(5)]
            source = b"printf '%s:' \"$#\"; printf '%s' \"$@\" | /usr/bin/sha256sum"
            expected = b"5:" + hashlib.sha256(b"".join(words)).hexdigest().encode() + b"  -\n"
            result = run(binary, prefix + [b"-c", source, b"probe", *words], directory, environment)
            report["cases"].append({"case": kind + "/ordinary-argv-160KiB", "expected_stdout_hex": expected.hex(), "pass": result["status"] == 0 and bytes.fromhex(result["stdout_hex"]) == expected and not result["timeout"], **result})
        report["remaining_project_entries_hex"] = sorted(os.fsencode(path.name).hex() for path in Path(directory).iterdir())
    report["binary_after"] = sha(binary)
    report["binary_stable"] = before == report["binary_after"]
    report["pass"] = report["binary_stable"] and all(case["pass"] for case in report["cases"]) and not report["remaining_project_entries_hex"]
    args.evidence.parent.mkdir(parents=True, exist_ok=True)
    with args.evidence.open("x") as output:
        json.dump(report, output, indent=2)
        output.write("\n")
    print(f"{sum(case['pass'] for case in report['cases'])}/{len(report['cases'])} passed; inspect exact evidence")
    return int(not report["pass"])


if __name__ == "__main__":
    sys.exit(main())
