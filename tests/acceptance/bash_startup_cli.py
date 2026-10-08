#!/usr/bin/env python3
"""Byte-exact startup differential through actual public shell executables.

No shell implementation modules are imported. Supply explicit immutable executable
paths, with the GNU oracle first; no executable is found through ambient PATH.
Default oracle requirement is GNU 5.3.20. An older oracle can support investigation
only with --oracle-version explicitly changed and is recorded as such.

Example:
  python3 tests/acceptance/bash_startup_cli.py \
    --shell gnu=/absolute/bash-5.3.20 --shell pristine=/absolute/brush \
    --shell candidate=/absolute/marsh-local --brush pristine --brush candidate \
    --evidence target/marsh-evidence/bash-startup/final --require-exact candidate

All stdin/argv/env/stdout/stderr/effects remain bytes. JSON uses hex, never a
replacement decoder or normalized comparison. Every child starts a new session;
timeouts kill only its tracked process group. No VM, provider, network or signal
semantics are exercised. The only signal is bounded cleanup of our own timeout.
"""
from __future__ import annotations

import argparse
import atexit
import dataclasses
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time


@dataclasses.dataclass
class Case:
    name: str
    args: list[bytes]
    stdin: bytes = b""
    env: dict[bytes, bytes] = dataclasses.field(default_factory=dict)
    files: dict[bytes, bytes] = dataclasses.field(default_factory=dict)
    argv0: bytes = b"startup-test"
    category: str = "behavior"


DATA = b'''printf 'zero=<%s> count=%s\n' "$0" "$#"; printf 'arg=<%s>\n' "$@"'''
FLAGS = b'''printf 'flags=%s\n' "$-"'''
BODY = b"printf body > body.effects; printf 'BODY\\n'"
ENV_FILE = b"printf env > env.effects; printf 'ENV\\n'"


def corpus() -> list[Case]:
    cases: list[Case] = [
        Case("default-stdin", [], FLAGS + b"\n"),
        Case("interactive-command", [b"-ic", FLAGS], env={b"BASH_ENV": b"./startup-env"}, files={b"startup-env": ENV_FILE}),
        Case("argv0-login", [b"-c", b"shopt -q login_shell; printf 'login=%s\\n' $?"], argv0=b"-startup-test"),
        Case("one-read-unit", [b"-st"], b"printf 'first\\n'; printf 'same\\n'\nprintf 'next\\n'\n"),
        Case("noexec-off", [b"-n", b"+n", b"-c", BODY]),
        Case("noexec-on", [b"-n", b"-c", BODY]),
        Case("raw-long-alias-path", [b"--init-file", b"./rc-\xff\xfe", b"-c", FLAGS]),
    ]
    for group in (b"-ce", b"-ec", b"-ceu", b"-euc", b"-c", b"+c"):
        cases.append(Case("group-" + group.hex(), [group, DATA, b"zero", b"one", b"+O", b"-x"]))
    for suffix in ([b"-e"], [b"+e"], [b"-e", b"+e"], [b"-s"], [b"-c"], [b"+c"]):
        cases.append(Case("after-c-" + b"_".join(suffix).hex(), [b"-c", *suffix, FLAGS]))
    for group in (b"-cs", b"-sc", b"+sc", b"-s", b"+s"):
        args = [group, FLAGS, b"zero"] if b"c" in group else [group, b"zero", b"arg"]
        cases.append(Case("input-" + group.hex(), args, DATA + b"; " + FLAGS + b"\n"))
    for prefix in ([b"-c", b"--"], [b"-ce", b"--"], [b"-c", b"-"], [b"-c", b"+"], [b"+", b"-c"]):
        cases.append(Case("terminator-" + b"_".join(prefix).hex(), [*prefix, DATA, b"--", b"+e", b"-c"]))
    for value in (b"+O", b"+Oextglob", b"+o", b"+onounset", b"-ce", b"--", b"", b"\xff\xfe", "\ufffd\ue000".encode()):
        cases.append(Case("command-data-" + value.hex(), [b"-c", DATA, b"zero", value]))
    cases.append(Case("raw-argv0", [b"-c", DATA], argv0=b"startup-\xff\xfe"))
    cases.append(Case("raw-zero", [b"-c", DATA, b"zero-\xff\xfe", b"one-\xfe\xff"]))
    cases.append(Case("raw-command", [b"-c", b"printf '<%s>\\n' '\xff\xfe'"]))
    for flags in (
        [b"-e", b"+e"], [b"+e", b"-e"], [b"-o", b"errexit", b"+e"],
        [b"+o", b"errexit", b"-e"], [b"-e", b"+o", b"errexit"],
        [b"-o", b"errexit", b"+o", b"errexit", b"-o", b"errexit"],
        [b"-oe", b"errexit"], [b"-eo", b"errexit"],
        [b"-oo", b"errexit", b"nounset"],
    ):
        cases.append(Case("ordered-e-" + b"_".join(flags).hex(), [*flags, b"-c", b"false; printf 'SURVIVED\\n'"]))
    for short, named in ((b"u", b"nounset"), (b"f", b"noglob"), (b"C", b"noclobber"), (b"B", b"braceexpand"), (b"a", b"allexport"), (b"h", b"hashall"), (b"E", b"errtrace"), (b"T", b"functrace")):
        for flags in ([b"-" + short, b"+o", named], [b"+" + short, b"-o", named], [b"-o", named, b"+" + short]):
            cases.append(Case("ordered-short-" + b"_".join(flags).hex(), [*flags, b"-c", FLAGS]))
    for flags in ([b"-O", b"extglob", b"+O", b"extglob"], [b"+O", b"extglob", b"-O", b"extglob"], [b"-O", b"extglob", b"+O", b"extglob", b"-O", b"extglob"]):
        cases.append(Case("ordered-shopt-" + b"_".join(flags).hex(), [*flags, b"-c", b"shopt -q extglob; printf '%s\\n' $?"]))
    for flags in ([b"--posix", b"+o", b"posix"], [b"--verbose", b"+v"]):
        cases.append(Case("ordered-long-" + b"_".join(flags).hex(), [*flags, b"-c", FLAGS]))
    for flags in ([b"-x", b"+x"], [b"-v", b"+v"], [b"-i", b"+i"], [b"+i"], [b"-l"], [b"+l"]):
        cases.append(Case("invocation-" + b"_".join(flags).hex(), [*flags, b"-c", FLAGS + b"; shopt -q login_shell; printf 'login=%s\\n' $?"]))
    for filename in (b"script", b"-script", b"+e", b"-", b"script-\xff\xfe"):
        cases.append(Case("script-" + filename.hex(), [b"--", filename, b"+O", b"-e", b"\xff\xfe"], files={filename: DATA + b"\n"}))
    cases.append(Case("script-boundary", [b"script", b"+O", b"extglob", b"-e", b"--"], files={b"script": DATA + b"\n"}))
    cases.append(Case("stdin-boundary", [b"-s", b"--", b"-script", b"\xff\xfe", b"+e"], DATA + b"\n"))
    cases.append(Case("bare-plus-is-option", [b"+", b"-c", BODY], files={b"+": b"printf WRONG\n"}))
    for mode in ([b"-c", BODY], [b"-s"], [b"script"]):
        cases.append(Case("bash-env-" + mode[0].hex(), mode, BODY + b"\n", {b"BASH_ENV": b"./startup-env"}, {b"startup-env": ENV_FILE, b"script": BODY}))
    cases.append(Case("bash-env-raw-path", [b"-c", BODY], env={b"BASH_ENV": b"./env-\xff\xfe"}, files={b"env-\xff\xfe": ENV_FILE}))
    cases.append(Case("inherited-ifs-reset", [b"-c", b"printf '<%s>\\n' \"$IFS\"; x='a:b'; printf '<%s>\\n' $x"], env={b"IFS": b":"}))
    cases.append(Case("raw-env", [b"-c", b"printf '<%s>\\n' \"$PAYLOAD\""], env={b"PAYLOAD": b"\xff\xfe"}))
    cases.append(Case("startup-ifs-used", [b"-c", b"x='a:b'; printf '<%s>\\n' $x"], env={b"BASH_ENV": b"./startup-env"}, files={b"startup-env": b"IFS=:\n"}))
    for flags in ([b"-z"], [b"+z"], [b"-ez"], [b"-cz"], [b"-o", b"not_an_option"], [b"+o", b"not_an_option"], [b"-O", b"not_an_option"], [b"-o", b"--"], [b"-O", b"--"], [b"-o", b"\xff\xfe"], [b"+onounset"], [b"-onounset"], [b"+Oextglob"], [b"-Oextglob"], [b"-e", b"--noprofile"], [b"-o", b"bad", b"+o", b"bad"], [b"-O", b"bad", b"-z"], [b"-O", b"bad", b"-o", b"bad2"], [b"--+o=nounset"], [b"-\xff"]):
        cases.append(Case("invalid-" + b"_".join(flags).hex(), [*flags, b"-c", BODY], BODY, {b"BASH_ENV": b"./startup-env"}, {b"startup-env": ENV_FILE}, category="invalid-before-effects"))
    for flags in ([b"-c"], [b"-ce", b"--"], [b"-c", b"-"], [b"-O", b"bad", b"-c"]):
        cases.append(Case("missing-" + b"_".join(flags).hex(), flags, BODY, {b"BASH_ENV": b"./startup-env"}, {b"startup-env": ENV_FILE}, category="invalid-before-effects"))
    for flags in ([b"-o"], [b"+o"], [b"-O"], [b"+O"], [b"-eo"], [b"-co"]):
        cases.append(Case("listing-" + b"_".join(flags).hex(), flags, b"printf 'STDIN\\n'\n", category="listing"))
    # Parse-time output must survive a later failure without loading BASH_ENV.
    for flags in ([b"-coe"], [b"-eco"], [b"-coez"], [b"-coO"], [b"-ico"],
                  [b"-O", b"extglob", b"-cO"], [b"-o", b"posix", b"-cO"],
                  [b"-o", b"posix", b"+o", b"posix", b"-cO"],
                  [b"--noediting", b"-co"], [b"-e\xff"], [b"+\xc3\xa9"],
                  [b"-O", b"\xff\xfe", b"-c", BODY]):
        cases.append(Case("typed-startup-" + b"_".join(flags).hex(), flags, BODY,
                          {b"BASH_ENV": b"./startup-env"}, {b"startup-env": ENV_FILE},
                          category="invalid-before-effects"))
    return cases


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(binary: Path, args: list[bytes], cwd: Path, env: dict[bytes, bytes], stdin: bytes, argv0: bytes) -> dict:
    started = time.monotonic()
    child = subprocess.Popen([argv0, *args], executable=os.fsencode(binary), cwd=cwd, env=env,
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             start_new_session=True)
    timed_out = False
    try:
        stdout, stderr = child.communicate(stdin, timeout=8)
    except subprocess.TimeoutExpired:
        timed_out = True
        os.killpg(child.pid, signal.SIGKILL)
        stdout, stderr = child.communicate()
    return {"status": child.returncode, "stdout": stdout.hex(), "stderr": stderr.hex(),
            "timed_out": timed_out, "pid": child.pid, "pgid": child.pid,
            "elapsed_seconds": time.monotonic() - started}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--shell", action="append", required=True, metavar="LABEL=PATH")
    parser.add_argument("--brush", action="append", default=[], help="labels needing --no-config")
    parser.add_argument("--oracle-version", default="5.3.20")
    parser.add_argument("--evidence", required=True, type=Path)
    parser.add_argument("--require-exact", action="append", default=[])
    parser.add_argument("--case", action="append", default=[], help="run only these case names")
    args = parser.parse_args()
    shells = dict(item.split("=", 1) for item in args.shell)
    binaries = {label: Path(path).resolve(strict=True) for label, path in shells.items()}
    if not set(args.brush + args.require_exact).issubset(binaries):
        parser.error("unknown shell label")
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=False)
    # The host bind mount can reject non-UTF8 filenames even on Linux. Exercise
    # byte paths on a native temporary filesystem, never weaken the byte cases.
    work = Path(tempfile.mkdtemp(prefix="marsh-startup-cli-"))
    atexit.register(shutil.rmtree, work, ignore_errors=True)
    environment = {b"PATH": b"/usr/bin:/bin", b"HOME": os.fsencode(work / "home"), b"LC_ALL": b"C", b"TERM": b"dumb"}
    identities = {label: {"path": str(path), "sha256": digest(path), "size": path.stat().st_size}
                  for label, path in binaries.items()}
    oracle = next(iter(binaries))
    version = run(binaries[oracle], [b"--version"], work, environment, b"", b"startup-test")
    identities[oracle]["version"] = version
    if (b"GNU bash, version " + args.oracle_version.encode() + b"(") not in bytes.fromhex(version["stdout"]):
        (evidence / "identities.json").write_text(json.dumps(identities, indent=2) + "\n")
        parser.error("oracle version does not match explicit requirement; evidence retained")
    cases = [case for case in corpus() if not args.case or case.name in args.case]
    if not cases:
        parser.error("no matching cases")
    records = []
    for case in cases:
        original = {"name": case.name, "category": case.category, "argv0": case.argv0.hex(),
                    "args": [value.hex() for value in case.args], "stdin": case.stdin.hex(),
                    "env": {key.hex(): value.hex() for key, value in case.env.items()},
                    "files": {key.hex(): value.hex() for key, value in case.files.items()}}
        outputs = {}
        for label, binary in binaries.items():
            shutil.rmtree(work)
            work.mkdir(mode=0o700)
            (work / "home").mkdir()
            for name, contents in case.files.items():
                with open(os.fsencode(work) + b"/" + name, "wb") as fixture:
                    fixture.write(contents)
            invocation = ([b"--no-config"] if label in args.brush else []) + [b"--noprofile", b"--norc", *case.args]
            result = run(binary, invocation, work, environment | case.env, case.stdin, case.argv0)
            effects = {}
            for root, _, files in os.walk(os.fsencode(work)):
                for name in files:
                    path = root + b"/" + name
                    relative = os.path.relpath(path, os.fsencode(work))
                    with open(path, "rb") as file:
                        contents = file.read()
                    if relative not in case.files or contents != case.files[relative]:
                        effects[relative.hex()] = contents.hex()
            for name in case.files:
                if not os.path.exists(os.fsencode(work) + b"/" + name):
                    effects[name.hex()] = None
            result["effects"] = effects
            result["invalid_has_no_effects"] = not effects if case.category == "invalid-before-effects" else None
            result["actual_argv"] = [case.argv0.hex(), *(value.hex() for value in invocation)]
            outputs[label] = result
            raw = evidence / "raw" / case.name
            raw.mkdir(parents=True, exist_ok=True)
            for stream in ("stdout", "stderr"):
                (raw / (label + "." + stream)).write_bytes(bytes.fromhex(result[stream]))
        keys = ("status", "stdout", "stderr", "effects", "timed_out")
        exact = {label: all(result[key] == outputs[oracle][key] for key in keys)
                 and not result["timed_out"] for label, result in outputs.items()}
        records.append(original | {"outputs": outputs, "exact": exact})
        with (evidence / "results.jsonl").open("a") as log:
            log.write(json.dumps(records[-1]) + "\n")
    metadata = {}
    for label in args.brush:
        metadata[label] = {}
        for option in (b"--help", b"--version"):
            result = run(binaries[label], [b"--no-config", option], work, environment, b"", b"startup-test")
            result["surface_ok"] = (result["status"] == 0 and bool(result["stdout"])
                                    and not result["stderr"] and not result["timed_out"])
            if option == b"--help":
                result["surface_ok"] &= b"--no-config" in bytes.fromhex(result["stdout"])
                result["surface_ok"] &= b"--input-backend" in bytes.fromhex(result["stdout"])
            metadata[label][option.decode("ascii")] = result
    stable = all(digest(binary) == identities[label]["sha256"] for label, binary in binaries.items())
    manifest = {"oracle": oracle, "oracle_version_required": args.oracle_version,
                "qualification_oracle": args.oracle_version == "5.3.20", "identities": identities,
                "harness_sha256": digest(Path(__file__)), "platform": sys.platform,
                "environment": {key.hex(): value.hex() for key, value in environment.items()},
                "binary_stable": stable, "brush_metadata": metadata,
                "count": len(records), "exact": {label: sum(r["exact"][label] for r in records) for label in binaries}}
    (evidence / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    shutil.rmtree(work)
    print(json.dumps({key: manifest[key] for key in ("count", "exact", "qualification_oracle")}, indent=2))
    return int(not stable or any(not r["exact"][label] for r in records for label in args.require_exact)
               or any(o["timed_out"] for r in records for o in r["outputs"].values())
               or any(not result["surface_ok"] for checks in metadata.values() for result in checks.values()))


if __name__ == "__main__":
    raise SystemExit(main())
