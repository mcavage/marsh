#!/usr/bin/env python3
"""Raw-byte prompt/trace differential through public CLI and real Unix PTYs.

Requires an explicit GNU 5.3.20 oracle by default. --oracle-version 5.3.0 is
investigative evidence only, never a qualification substitute. The first --shell
is the oracle; --brush labels get Brush's no-config/minimal-backend switches.
No implementation modules, decoded output, or output normalizers are used.
"""
from __future__ import annotations

import argparse
import atexit
import dataclasses
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import selectors
import shutil
import signal
import subprocess
import tempfile
import termios
import time


@dataclasses.dataclass
class Case:
    name: str
    script: bytes = b""
    env: dict[bytes, bytes] = dataclasses.field(default_factory=dict)
    files: dict[bytes, bytes] = dataclasses.field(default_factory=dict)
    tty: bool = False
    cwd_name: bytes | None = None


def corpus() -> list[Case]:
    cases = []
    for name, prefix in {
        "literal-fffe": b"\xff\xfe ",
        "ascii-led-fffe": b"+\xff\xfe ",
        "ascii-led-variable": b"+$PROMPT_BYTES ",
        "literal-unicode-control": "\ufffd\ue000 ".encode(),
        "variable": b"$PROMPT_BYTES ",
        "command-substitution": br'''$(printf '\377\376') ''',
        "octal-fffe": br"\377\376 ",
        "octal-exact-three": br"\7\77\777 ",
        "octal-nul": br"A\000B ",
        "literal-control": b"\x01A\x02 ",
        "octal-control": br"\001A\002 ",
        "nonprinting-markers": br"\[A\] ",
        "quotes": b"'\" ",
        "quotes-variable": b"'\"$PROMPT_BYTES\"' ",
        "parameter-single-quotes": b"${unset:-'Q'} ",
        "parameter-double-quotes": b'${unset:-"Q"} ',
        "braces-tilde-glob": b"{a,b} ~ * ",
        "backslash-variable": br"\\$PROMPT_BYTES ",
        "octal-dollar": br"\044PROMPT_BYTES ",
        "octal-backslash": br"\134$PROMPT_BYTES ",
        "backslash-invalid": b"\\\xff ",
        "date-raw-literals": b"\\D{\xff%Y\xfe} ",
        "date-protected-variable": br"\D{$PROMPT_BYTES} ",
        "date-unknown": br"\D{%Q-%f-%%} ",
    }.items():
        # Assign through an environment VALUE so quotes inside the prefix are
        # data, not newly quoted/reparsed source assembled by the test runner.
        for promptvars in (True, False):
            script = b"PS4=$PREFIX; "
            if not promptvars:
                script += b"shopt -u promptvars; "
            script += b"set -x; : traced; set +x"
            cases.append(Case(name + ("-expand" if promptvars else "-literal"), script,
                              {b"PREFIX": prefix, b"PROMPT_BYTES": b"\xff\xfe" if b"PROMPT_BYTES" in prefix else b"ASCII"}))
    cases.extend([
        Case("raw-source-prefix", b"PS4='\xff\xfe '; set -x; : traced; set +x"),
        Case("trace-status", br'''PS4='$(printf "\377")$? '; set -x; false; printf '%s\n' "$?"; set +x'''),
        Case("trace-arithmetic", br'''n=0; PS4='$((n+=1)) '; set -x; :; :; set +x; printf '%s\n' "$n"'''),
        Case("marker-adjacency", br"x=A; xtail=B; PS4='$x\[tail\] '; set -x; :; set +x"),
        Case("cwd-metadata-literal", br"PS4='\w|\W '; set -x; :; set +x", cwd_name=b"\xff-$PROMPT_BYTES-$(printf hit>injected.effects)-\""),
        Case("cwd-visible-controls", br"PS4='\w|\W '; set -x; :; set +x", cwd_name=b"A\x01B\nC\tD\x7fE\xff"),
    ])
    for prefix in (b"\xff\xfe>", b"\xc3\xa9\xff>", b"\xc3X\xfe>"):
        for destination in ("stderr", "file"):
            script = b"PS4=$PREFIX; "
            if destination == "file":
                script += b"exec 9>trace.effects; BASH_XTRACEFD=9; "
            script += b"set -x; . ./source; : outer; set +x"
            cases.append(Case("repeat-" + prefix.hex() + "-" + destination, script,
                              {b"PREFIX": prefix}, {b"source": b": sourced\n"}))
    for name, primary, secondary in (
        ("literal", b"[\xff\xfe]P1> ", b"[\xfe\xff]P2> "),
        ("variable", b"[$PROMPT_BYTES]P1> ", b"[${PROMPT_BYTES}]P2> "),
        ("command-substitution", br"[$(printf '\377\376')]P1> ", br"[$(printf '\376\377')]P2> "),
        ("octal", br"[\377\376]P1> ", br"[\376\377]P2> "),
        ("markers", br"[\[\377\376\]]P1> ", br"[\[\376\377\]]P2> "),
    ):
        cases.append(Case("tty-" + name, env={b"PS1": primary, b"PS2": secondary,
                                             b"PROMPT_BYTES": b"\xff\xfe" if name == "variable" else b"ASCII"}, tty=True))
    return cases


def sha(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def child_session_terminal() -> None:
    # Popen's start_new_session has already created this child's session. Only
    # its own slave PTY becomes its controlling terminal; no parent terminal IO.
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def invoke(binary: Path, args: list[bytes], cwd: bytes, env: dict[bytes, bytes], tty: bool) -> dict:
    transcript = bytearray()
    steps = []
    timed_out = False
    master = slave = None
    selector = None
    if tty:
        master, slave = pty.openpty()
        attrs = termios.tcgetattr(slave)
        attrs[1] &= ~termios.OPOST
        attrs[3] &= ~(termios.ECHO | termios.ECHONL)
        termios.tcsetattr(slave, termios.TCSANOW, attrs)
    child = subprocess.Popen([b"prompt-test", *args], executable=os.fsencode(binary), cwd=cwd, env=env,
                             stdin=slave if tty else subprocess.DEVNULL,
                             stdout=subprocess.PIPE, stderr=slave if tty else subprocess.PIPE,
                             start_new_session=True,
                             preexec_fn=child_session_terminal if tty else None)
    if slave is not None:
        os.close(slave)
    try:
        if tty:
            assert master is not None
            os.set_blocking(master, False)
            selector = selectors.DefaultSelector()
            selector.register(master, selectors.EVENT_READ)
            deadline = time.monotonic() + 8
            cursor = 0
            for marker, send in ((b"]P1> ", b"printf '%s' '\n"),
                                 (b"]P2> ", b"DATA'\n"),
                                 (b"]P1> ", b"exit\n")):
                while marker not in transcript[cursor:]:
                    if time.monotonic() >= deadline:
                        raise subprocess.TimeoutExpired(args, 8)
                    if not selector.select(0.05):
                        continue
                    try:
                        data = os.read(master, 65536)
                    except OSError as error:
                        if error.errno == errno.EIO:
                            data = b""
                        else:
                            raise
                    if not data:
                        break
                    transcript.extend(data)
                found = marker in transcript[cursor:]
                steps.append({"marker": marker.hex(), "observed": found, "transcript_end": len(transcript), "sent": send.hex() if found else ""})
                if not found:
                    break
                cursor = len(transcript)
                os.write(master, send)
            stdout, _ = child.communicate(timeout=max(0.1, deadline - time.monotonic()))
            while True:
                try:
                    data = os.read(master, 65536)
                except OSError as error:
                    if error.errno in (errno.EIO, errno.EAGAIN):
                        break
                    raise
                if not data:
                    break
                transcript.extend(data)
            stderr = bytes(transcript)
        else:
            stdout, stderr = child.communicate(timeout=8)
    except subprocess.TimeoutExpired:
        timed_out = True
        os.killpg(child.pid, signal.SIGKILL)
        stdout, captured_stderr = child.communicate()
        stderr = bytes(transcript) if tty else captured_stderr
    finally:
        if selector is not None:
            selector.close()
        if master is not None:
            os.close(master)
    return {"status": child.returncode, "stdout": stdout.hex(), "stderr": stderr.hex(),
            "timed_out": timed_out, "pid": child.pid, "pgid": child.pid, "tty_steps": steps}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--shell", action="append", required=True, metavar="LABEL=PATH")
    parser.add_argument("--brush", action="append", default=[])
    parser.add_argument("--oracle-version", default="5.3.20")
    parser.add_argument("--utf8-locale", default="C.utf8")
    parser.add_argument("--evidence", required=True, type=Path)
    parser.add_argument("--require-exact", action="append", default=[])
    parser.add_argument("--case", action="append", default=[])
    args = parser.parse_args()
    binaries = {name: Path(path).resolve(strict=True) for name, path in (entry.split("=", 1) for entry in args.shell)}
    if not set(args.brush + args.require_exact).issubset(binaries):
        parser.error("unknown label")
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=False)
    work = Path(tempfile.mkdtemp(prefix="marsh-prompt-cli-"))
    atexit.register(shutil.rmtree, work, ignore_errors=True)
    environment = {b"PATH": b"/usr/bin:/bin", b"HOME": os.fsencode(work), b"LC_ALL": b"C",
                   b"TERM": b"dumb", b"HISTFILE": b""}
    identities = {label: {"path": str(binary), "sha256": sha(binary)} for label, binary in binaries.items()}
    oracle = next(iter(binaries))
    version = invoke(binaries[oracle], [b"--version"], os.fsencode(work), environment, False)
    (evidence / "oracle-version.json").write_text(json.dumps(version, indent=2) + "\n")
    if (b"GNU bash, version " + args.oracle_version.encode() + b"(") not in bytes.fromhex(version["stdout"]):
        parser.error("actual oracle does not match required version; evidence retained")
    records = []
    for locale in (b"C", args.utf8_locale.encode("ascii")):
        for case in corpus():
            if args.case and case.name not in args.case:
                continue
            results = {}
            for label, binary in binaries.items():
                shutil.rmtree(work)
                work.mkdir(mode=0o700)
                cwd = os.fsencode(work)
                if case.cwd_name is not None:
                    cwd += b"/" + case.cwd_name
                    os.mkdir(cwd)
                for name, contents in case.files.items():
                    with open(cwd + b"/" + name, "wb") as file:
                        file.write(contents)
                invocation = ([b"--no-config", b"--input-backend", b"minimal"] if label in args.brush else [])
                invocation += [b"--noprofile", b"--norc"]
                invocation += [b"--noediting", b"-i"] if case.tty else [b"-c", case.script]
                result = invoke(binary, invocation, cwd, environment | case.env | {b"LC_ALL": locale}, case.tty)
                effects = {}
                for directory, _, files in os.walk(os.fsencode(work)):
                    for name in files:
                        path = directory + b"/" + name
                        relative = os.path.relpath(path, cwd)
                        with open(path, "rb") as file:
                            content = file.read()
                        if relative not in case.files or content != case.files[relative]:
                            effects[relative.hex()] = content.hex()
                result["effects"] = effects
                result["actual_argv"] = [b"prompt-test".hex(), *(arg.hex() for arg in invocation)]
                results[label] = result
                raw = evidence / "raw" / locale.decode("ascii") / case.name
                raw.mkdir(parents=True, exist_ok=True)
                for stream in ("stdout", "stderr"):
                    (raw / (label + "." + stream)).write_bytes(bytes.fromhex(result[stream]))
            compare = ("stdout", "stderr", "status", "effects", "timed_out")
            oracle_valid = (results[oracle]["status"] == 0 and not results[oracle]["timed_out"]
                            and all(step["observed"] for step in results[oracle]["tty_steps"]))
            raw_equal = {label: all(result[key] == results[oracle][key] for key in compare)
                         for label, result in results.items()}
            # A crashing oracle is an invalid reference, never a target behavior
            # or a green self-comparison that a matching candidate can satisfy.
            exact = {label: oracle_valid and not result["timed_out"] and raw_equal[label]
                     for label, result in results.items()}
            record = {"case": case.name, "locale": locale.hex(), "tty": case.tty,
                      "oracle_valid": oracle_valid, "raw_equal": raw_equal,
                      "script": case.script.hex(), "env": {k.hex(): v.hex() for k, v in case.env.items()},
                      "files": {k.hex(): v.hex() for k, v in case.files.items()},
                      "cwd_name": None if case.cwd_name is None else case.cwd_name.hex(),
                      "results": results, "exact": exact}
            records.append(record)
            with (evidence / "results.jsonl").open("a") as log:
                log.write(json.dumps(record) + "\n")
    stable = all(sha(binary) == identities[label]["sha256"] for label, binary in binaries.items())
    summary = {"count": len(records), "exact": {label: sum(r["exact"][label] for r in records) for label in binaries},
               "qualification_oracle": args.oracle_version == "5.3.20", "oracle_version": version,
               "oracle_defects": [{"case": r["case"], "locale": r["locale"], "status": r["results"][oracle]["status"]}
                                  for r in records if not r["oracle_valid"]],
               "identities": identities, "binary_stable": stable, "harness_sha256": sha(Path(__file__)),
               "timeouts": {label: sum(r["results"][label]["timed_out"] for r in records) for label in binaries}}
    (evidence / "manifest.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps({key: summary[key] for key in ("count", "exact", "qualification_oracle", "timeouts")}, indent=2))
    return int(not records or not stable or bool(summary["oracle_defects"]) or any(summary["timeouts"].values())
               or any(not r["exact"][label] for r in records for label in args.require_exact))


if __name__ == "__main__":
    raise SystemExit(main())
