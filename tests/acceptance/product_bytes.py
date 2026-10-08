#!/usr/bin/env python3
"""Exact-byte shell/product probes; stock/Cloud execution is explicit and host-only.

Local oracle/support run:
  python3 tests/acceptance/product_bytes.py --shell GNU=/path/bash \
      --shell pristine=/path/brush --work-root /tmp --evidence /tmp/bytes.json

Public product run (root integration owner; prepared isolated scope/fixture):
  python3 tests/acceptance/product_bytes.py --public-marsh /path/marsh \
      --scope-home /path/scope --control-home /path/control \
      --work-root /path/approved-project --fixture fixture \
      --evidence /path/evidence.json [--cloud]

No text normalization. This does not create/publish Kit mappings or enable Cloud.
A public run may boot stock VMs and --cloud may incur charges. The invoking owner
must prepare/clean those exact resources and bind evidence to candidate images.
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
import time


PAYLOAD = b"\xff\xfe\xc3\xa9\xef\xbf\xbd\xee\x80\x80= line\nend"
ALL_BYTES = bytes(range(1, 256))


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def frame(values):
    return b"".join(len(value).to_bytes(4, "big") + value for value in values)


def cases(fixture=None, cloud=False, native=False):
    result = [
        ("raw-positionals", [b"-c", b"printf '%s' \"$1\"", b"arg0-\xfe", PAYLOAD], PAYLOAD, {}, {}),
        ("raw-c-source", [b"-c", b"printf '%s' '\xff\xfe'"], b"\xff\xfe", {}, {}),
        ("raw-output-nonzero-status", [b"-c", b"printf '%s' \"$1\"; exit 37", b"arg0", PAYLOAD], PAYLOAD, {}, {}, 37),
        ("all-non-nul-positionals", [b"-c", b"printf '%s' \"$1\"", b"arg0", ALL_BYTES], ALL_BYTES, {}, {}),
        ("raw-script-filename", [b"script-\xfe", PAYLOAD], PAYLOAD, {}, {b"script-\xfe": b"printf '%s' \"$1\"\n"}),
        ("native-subshell-env", [b"-c", b"export PROBE_VALUE=$1; ( /usr/bin/printenv PROBE_VALUE )", b"arg0", PAYLOAD], PAYLOAD + b"\n", {}, {}),
        ("native-pipeline-argv", [b"-c", b"/usr/bin/printf '%s' \"$1\" | /usr/bin/cat", b"arg0", ALL_BYTES], ALL_BYTES, {}, {}),
        ("command-substitution", [b"-c", b"export PROBE_VALUE=$1; v=$(/usr/bin/printenv PROBE_VALUE); printf '%s' \"$v\"", b"arg0", PAYLOAD], PAYLOAD, {}, {}),
        ("raw-output-filename", [b"-c", b"printf '%s' \"$1\" > 'output-\xff'; /usr/bin/cat 'output-\xff'", b"arg0", PAYLOAD], PAYLOAD, {}, {}),
        ("raw-script-body", [b"script-\xff"], b"\xff\xfe", {}, {b"script-\xff": b"printf '%s' '\xff\xfe'\n"}),
        ("native-child-state", [b"-c", b"v=$1; ( printf '%s' \"$v\"; /usr/bin/printf '%s' \"$v\" )", b"arg0", PAYLOAD], PAYLOAD * 2, {}, {}),
    ]
    if native:
        result += [
            ("inherited-raw-value", [b"-c", b"printf '%s' \"$PROBE_VALUE\""], PAYLOAD, {b"PROBE_VALUE": PAYLOAD}, {}),
            ("inherited-raw-name", [b"-c", b"/usr/bin/printenv 'NAME_\xff'"], PAYLOAD + b"\n", {b"NAME_\xff": PAYLOAD}, {}),
            ("inherited-nonidentifier-name", [b"-c", b"( /usr/bin/printenv '1.not-a-variable' )"], PAYLOAD + b"\n", {b"1.not-a-variable": PAYLOAD}, {}),
        ]
    if fixture:
        name = fixture.encode("ascii")
        for placement in ([b"local", b"cloud"] if cloud else [b"local"]):
            for route, command in [("shim", name), ("descendant-path", b"/usr/bin/env " + name)]:
                source = b"export PROBE_VALUE=$1 MARSH_PLACE=" + placement + b"; " + command + b" raw-bytes \"$1\" '' \"$2\""
                result.append((placement.decode() + "-" + route + "-raw-env-argv", [b"-c", source, b"arg0", PAYLOAD, ALL_BYTES], frame([PAYLOAD, b"", ALL_BYTES, PAYLOAD]), {}, {}))
    return result


def run_case(binary, prefix, case, directory, environment, timeout):
    name, arguments, expected, extra_env, files = case[:5]
    expected_status = case[5] if len(case) == 6 else 0
    for path, data in files.items():
        with open(os.path.join(os.fsencode(directory), path), "wb") as output:
            output.write(data)
    started = time.monotonic()
    timed_out = False
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        process = subprocess.Popen([os.fsencode(binary), *prefix, *arguments], cwd=directory,
                                   env=environment | extra_env, stdin=subprocess.DEVNULL,
                                   stdout=stdout, stderr=stderr, start_new_session=True)
        try:
            process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=10)
        sizes = [stdout.tell(), stderr.tell()]
        stdout.seek(0)
        stderr.seek(0)
        out, err = stdout.read(65537), stderr.read(65537)
    effects = {}
    for path in os.listdir(os.fsencode(directory)):
        full = os.path.join(os.fsencode(directory), path)
        if os.path.isfile(full):
            with open(full, "rb") as source:
                effects[path.hex()] = source.read(65537).hex()
    expected_effects = {path.hex(): data.hex() for path, data in files.items()}
    if name == "raw-output-filename":
        expected_effects[b"output-\xff".hex()] = PAYLOAD.hex()
    # Public stock startup progress is retained, not rewritten to match GNU.
    # All ordinary support/oracle cases require exact empty stderr.
    pass_bytes = out == expected and effects == expected_effects
    return {"case": name, "argv_hex": [arg.hex() for arg in arguments],
            "status": process.returncode, "expected_status": expected_status,
            "stdout_hex": out.hex(), "stderr_hex": err.hex(),
            "stdout_bytes": sizes[0], "stderr_bytes": sizes[1], "files": effects,
            "expected_stdout_hex": expected.hex(), "expected_files": expected_effects,
            "timeout": timed_out, "seconds": time.monotonic() - started,
            "bytes_and_files_exact": pass_bytes,
            "pass": not timed_out and process.returncode == expected_status and pass_bytes
                    and sizes[0] <= 65536 and sizes[1] <= 65536
                    and (bool(prefix) or not err)}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--shell", action="append", default=[], metavar="NAME=PATH")
    parser.add_argument("--public-marsh", type=Path)
    parser.add_argument("--scope-home", type=Path)
    parser.add_argument("--control-home", type=Path)
    parser.add_argument("--fixture")
    parser.add_argument("--cloud", action="store_true")
    parser.add_argument("--work-root", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--timeout", type=int, default=60)
    args = parser.parse_args()
    if args.public_marsh and (sys.platform != "darwin" or not args.scope_home or not args.control_home):
        parser.error("public stock runs require the macOS host and explicit isolated scope/control roots")
    if (args.cloud or args.fixture) and not args.public_marsh:
        parser.error("fixture/Cloud wire proof requires --public-marsh")
    if args.cloud and not args.fixture:
        parser.error("--cloud requires an explicitly prepared --fixture")
    if args.fixture and not re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_-]{0,127}", args.fixture):
        parser.error("fixture must be a checked ASCII Kit command name")
    if not args.shell and not args.public_marsh:
        parser.error("provide --shell NAME=PATH or --public-marsh")
    args.work_root = args.work_root.resolve(strict=True)
    shells = []
    for item in args.shell:
        name, separator, path = item.partition("=")
        if not separator or not name:
            parser.error("--shell requires NAME=PATH")
        shells.append((name, Path(path).resolve(strict=True), False))
    if args.public_marsh:
        shells.append(("public-marsh", args.public_marsh.resolve(strict=True), True))
    report = {"schema": 1, "harness_sha256": sha(__file__), "runs": []}
    for name, binary, public in shells:
        before = sha(binary)
        run = {"name": name, "binary": str(binary), "sha256_before": before, "public_stock": public, "cases": []}
        with tempfile.TemporaryDirectory(prefix="product-bytes-", dir=args.work_root) as root:
            home = Path(root) / "home"
            home.mkdir()
            env = {b"PATH": b"/usr/bin:/bin", b"HOME": os.fsencode(home), b"LC_ALL": b"C"}
            prefix = []
            if public:
                # Host identity/auth stays at its normal host-only boundary. No
                # environment values are written to the evidence document.
                env = os.environb.copy()
                env[b"MARSH_HOME"] = os.fsencode(args.scope_home.resolve())
                env[b"MARSH_CONTROL_HOME"] = os.fsencode(args.control_home.resolve())
                prefix = []
            for index, case in enumerate(cases(args.fixture if public else None, args.cloud and public, not public)):
                directory = Path(root) / str(index)
                directory.mkdir()
                run["cases"].append(run_case(binary, prefix, case, directory, env, args.timeout))
        run["sha256_after"] = sha(binary)
        run["binary_stable"] = before == run["sha256_after"]
        run["pass"] = run["binary_stable"] and all(case["pass"] for case in run["cases"])
        report["runs"].append(run)
        print(f"{name}: {sum(case['pass'] for case in run['cases'])}/{len(run['cases'])} exact")
    args.evidence.parent.mkdir(parents=True, exist_ok=True)
    with args.evidence.open("x") as output:
        json.dump(report, output, indent=2)
        output.write("\n")
    return int(not all(run["pass"] for run in report["runs"]))


if __name__ == "__main__":
    sys.exit(main())
