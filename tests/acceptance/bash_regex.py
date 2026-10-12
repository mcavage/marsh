#!/usr/bin/env python3
"""[[ =~ ]] differential callers: captures across repeated and failed matches,
xtrace of quoted pieces, exec and CHLD-trap cleanup after a match.

The matcher itself (bytes, classes, locales) is covered by bash_byte_matchers.py.
marsh uses upstream Brush's matcher, with no resource limit of its own (see
docs/design/bash-compatibility.md), so there are no limit cases here.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile

import bash_report

CAPTURE = (b"[[ aa =~ (a+) ]]; [[ $2 =~ $1 ]]; s=$?; "
           b"printf '%s\\0' \"$s\" \"${BASH_REMATCH[@]}\"; "
           b"[[ ab =~ (a|ab) ]]; printf '%s\\0' \"$?\" \"${BASH_REMATCH[@]}\"")
NO_CHILDREN = (b'import os\ntry:\n p,s=os.waitpid(-1,os.WNOHANG)\n print("UNEXPECTED_CHILD",p,s)\n'
               b'except ChildProcessError:\n print("NO_CHILDREN")\n')


def cases(directory: Path) -> dict[str, tuple[bytes, list[bytes]]]:
    result = {
        "alternating-compiled-expressions": (
            b'for ((i=0;i<100;i++)); do '
            b'[[ xxababz =~ (ab|a)+ ]]; printf \'%s\\0\' "$?" "${BASH_REMATCH[@]}"; '
            b'[[ foo17 =~ ([[:alpha:]]+)([[:digit:]]+) ]]; '
            b'printf \'%s\\0\' "$?" "${BASH_REMATCH[@]}"; done', []),
        "invalid-retention": (CAPTURE, [b"[", b"a"]),
    }
    for name, expression, value in [("quoted-close-bracket", b'\"]\"', b"]"),
                                    ("quoted-close-brace", b'\"}\"', b"}"),
                                    ("quoted-repeat", b'\"a{1}\"', b"a{1}"),
                                    ("quoted-bracket-piece", b'x\"[.]\"y', b"x[.]y")]:
        result[f"trace-{name}"] = (b'set -x; [[ $1 =~ ' + expression
                                   + b' ]]; s=$?; set +x; printf \'%s\\0\' "$s" "${BASH_REMATCH[@]}"', [value])
    fallback = directory / "no-shebang"
    fallback.write_bytes(b'exec "$1" -c "$2"\n')
    fallback.chmod(0o700)
    result["exec-cleanup-external"] = (b'[[ a =~ (a) ]]; exec "$1" -c "$2"',
                                       [os.fsencode(sys.executable), NO_CHILDREN])
    result["exec-cleanup-script-fallback"] = (b'[[ a =~ (a) ]]; exec "$1" "$2" "$3"',
                                              [os.fsencode(fallback), os.fsencode(sys.executable), NO_CHILDREN])
    for name, operation in [("valid", b"[[ a =~ (a) ]]"), ("invalid", b"r='['; [[ a =~ $r ]]"),
                            ("user-child", b"[[ a =~ (a) ]]; /usr/bin/true"),
                            ("user-function-child", b"[[ a =~ (a) ]]; f() { :; }; f & wait")]:
        result[f"chld-{name}"] = (b'trap \'printf "CHLD:%s\\n" "$?"\' CHLD; ' + operation
                                  + b'; printf "DONE:%s\\n" "$?"', [])
    return result


def observe(shell: Path, script: bytes, operands: list[bytes]) -> dict:
    with tempfile.TemporaryDirectory(prefix="regex-caller-home-") as home:
        process = subprocess.Popen([os.fsencode(shell), b"--noprofile", b"--norc", b"-c", script, b"regex",
                                    *operands], env={b"PATH": b"/usr/bin:/bin", b"LC_ALL": b"C",
                                                     b"HOME": os.fsencode(home)},
                                   stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   start_new_session=True)
        try:
            out, err = process.communicate(timeout=30)
            timed_out = False
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            out, err = process.communicate()
            timed_out = True
    return {"status": process.returncode, "stdout": out.hex(), "stderr": err.hex(), "timeout": timed_out}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--oracle", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    args.evidence.mkdir(parents=True, exist_ok=True)
    rows, verdicts = [], {}
    with tempfile.TemporaryDirectory(prefix="regex-") as directory:
        for name, (script, operands) in cases(Path(directory)).items():
            gnu = observe(args.oracle, script, operands)
            candidate = observe(args.candidate, script, operands)
            keys = ("status", "stdout", "stderr", "timeout")
            equal = all(gnu[key] == candidate[key] for key in keys)
            if name.startswith("exec-cleanup"):
                equal &= candidate["stdout"] == b"NO_CHILDREN\n".hex()
            verdicts[name] = bash_report.verdict(not gnu["timeout"], equal)
            rows.append({"case": name, "script": script.hex(), "gnu": gnu, "candidate": candidate})
    (args.evidence / "observations.json").write_text(json.dumps(rows, indent=2) + "\n")
    bash_report.write(args.evidence, verdicts)
    print(json.dumps({"total": len(rows), "match": sum(v == "match" for v in verdicts.values())}))
    return int(any(v != "match" for v in verdicts.values()))


if __name__ == "__main__":
    raise SystemExit(main())
