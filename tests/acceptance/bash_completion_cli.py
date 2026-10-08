#!/usr/bin/env python3
"""Compare filename and command completion through the public shell CLI."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


CASES = {
    "tilde-directory": b"compgen -f -- '~/i' | /usr/bin/sort",
    "parameter-directory": b"compgen -f -- '$HOME/i' | /usr/bin/sort",
    "braced-directory": b"compgen -f -- '${HOME}/i' | /usr/bin/sort",
    "quoted-directory": b"compgen -f -- '\"$HOME\"/i' | /usr/bin/sort",
    "spaced-directory": b"compgen -f -- '~/sub dir/' | /usr/bin/sort",
    "escaped-directory": b"compgen -f -- 'sub\\ dir/' | /usr/bin/sort",
    "literal-directory-wins": (
        b"mkdir '$HOME'; touch '$HOME/literal'; "
        b"compgen -f -- '$HOME/' | /usr/bin/sort"
    ),
    "tilde-directory-beats-literal": (
        b"mkdir '~'; touch '~/literal'; compgen -f -- '~/i' | /usr/bin/sort"
    ),
    "literal-basename": b"compgen -f -- 'a\\' | /usr/bin/sort",
    "literal-escaped-basename": b"compgen -f -- 'a\\b' | /usr/bin/sort",
    "unexpanded-basename": b"compgen -f -- '${HOME}'; printf 'status:%s\\n' \"$?\"",
    "no-command-substitution": (
        b"compgen -f -- '$(touch BAD)/'; printf 'status:%s\\n' \"$?\"; "
        b"test ! -e BAD"
    ),
    "no-backquote-substitution": (
        b"compgen -f -- '`touch BAD`/'; printf 'status:%s\\n' \"$?\"; "
        b"test ! -e BAD"
    ),
    "command-names": (
        b"alias zzcomplete_alias=':'; zzcomplete_function(){ :; }; "
        b"PATH=./bin; compgen -A command zzcomplete | /usr/bin/sort"
    ),
    "disabled-command": (
        b"PATH=./bin; enable -n echo; compgen -A command echo; "
        b"printf 'status:%s\\n' \"$?\"; compgen -b echo"
    ),
}


def run(binary, source, directory):
    directory.mkdir()
    (directory / "item1").touch()
    (directory / "sub dir").mkdir()
    (directory / "sub dir" / "child").touch()
    (directory / "a\\b").touch()
    commands = directory / "bin"
    commands.mkdir()
    executable = commands / "zzcomplete_executable"
    executable.write_bytes(b"#!/bin/sh\nexit 0\n")
    executable.chmod(0o755)
    (commands / "zzcomplete_link").symlink_to(executable.name)
    (commands / "zzcomplete_directory").mkdir()
    (commands / "zzcomplete_directory_link").symlink_to("zzcomplete_directory")
    (commands / "zzcomplete_nonexecutable").touch()
    result = subprocess.run(
        [os.fsencode(binary), b"--noprofile", b"--norc", b"-c", source],
        cwd=directory,
        env={"HOME": str(directory), "PATH": "/usr/bin:/bin", "LC_ALL": "C"},
        capture_output=True,
        timeout=10,
    )
    return {
        "status": result.returncode,
        "stdout_hex": result.stdout.hex(),
        "stderr_hex": result.stderr.hex(),
        "unexpected_command_effect": (directory / "BAD").exists(),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--reference", required=True, type=Path)
    parser.add_argument("--evidence", required=True, type=Path)
    args = parser.parse_args()
    binaries = {name: path.resolve(strict=True) for name, path in
                {"candidate": args.candidate, "reference": args.reference}.items()}
    report = {"artifact_sha256": {
        name: hashlib.sha256(path.read_bytes()).hexdigest()
        for name, path in binaries.items()
    }, "cases": []}
    with tempfile.TemporaryDirectory(prefix="marsh-completion-cli-") as work:
        for name, source in CASES.items():
            observations = {
                label: run(binary, source, Path(work) / f"{name}-{label}")
                for label, binary in binaries.items()
            }
            report["cases"].append({
                "name": name, "source_hex": source.hex(),
                "observations": observations,
                "passed": observations["candidate"] == observations["reference"],
            })
    report["passed"] = all(row["passed"] for row in report["cases"])
    args.evidence.parent.mkdir(parents=True, exist_ok=True)
    args.evidence.write_text(json.dumps(report, indent=2) + "\n")
    for row in report["cases"]:
        print(row["name"], "PASS" if row["passed"] else "FAIL")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
