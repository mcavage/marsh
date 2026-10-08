#!/usr/bin/env python3
"""Ordinary ps/top run as system commands without a working marsh daemon."""

import argparse
import os
from pathlib import Path
import subprocess
import tempfile


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--marsh", type=Path, required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="marsh-process-pass-") as home:
        env = os.environ.copy()
        env.update(MARSH_HOME=f"{home}/missing", MARSH_PLACE="invalid", BIG="x" * 20_000)
        handoff = ["--marsh-guest", "--marsh-session",
                   "11111111-1111-4111-8111-111111111111"]
        system = subprocess.run(["/bin/ps", "-o", "pid=", "-p", str(os.getpid())],
                                capture_output=True, check=True)
        shell = subprocess.run([str(args.marsh), "--invoke-bundled", "ps", *handoff,
                                "-o", "pid=", "-p", str(os.getpid())],
                               capture_output=True, env=env, timeout=10)
        assert shell.returncode == 0, shell.stderr
        assert shell.stdout == system.stdout, (shell.stdout, system.stdout)
        assert shell.stderr == b"", shell.stderr
        top = subprocess.run([str(args.marsh), "--invoke-bundled", "top", *handoff,
                              "-l", "1", "-n", "0"],
                             capture_output=True, env=env, timeout=15)
        assert top.returncode == 0, top.stderr
        assert top.stdout, "system top produced no output"
        assert top.stderr == b"", top.stderr
    print("ordinary ps/top passed without a daemon or valid placement")


if __name__ == "__main__":
    main()
