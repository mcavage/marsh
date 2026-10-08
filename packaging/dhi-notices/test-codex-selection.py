#!/usr/bin/env python3
"""Actual shell lookup/Kit entrypoint with sentinel executables, not agent/image proof."""

import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys

ROOT = Path(__file__).absolute().parents[2]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--inside", action="store_true")
    args = parser.parse_args()
    work = args.work.absolute()
    if not args.inside:
        work.mkdir(mode=0o700, exist_ok=False)
        for name in ["local-bin", "share/npm-global/bin", "project"]:
            (work / name).mkdir(parents=True)
        for path, version in [
            (work / "local-bin/codex", "0.155.1"),
            (work / "share/npm-global/bin/codex", "0.159.2"),
        ]:
            path.write_text('#!/bin/sh\nprintf "codex-cli ' + version + '\\n"\n')
            path.chmod(0o755)
        shutil.copy2(
            ROOT / "kits/marsh-codex/marsh-entrypoint.sh", work / "entrypoint.sh"
        )
        shutil.copy2(ROOT / "kits/marsh-codex/codex.dockerfile", work / "Dockerfile")
        command = [
            "unshare",
            "-rm",
            sys.executable,
            "-I",
            "-B",
            str(Path(__file__).absolute()),
            "--inside",
            "--work",
            str(work),
        ]
        return subprocess.run(command, timeout=60).returncode
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.mount(b"none", b"/", None, (1 << 18) | 16384, None):
        raise OSError(ctypes.get_errno(), "private mount namespace")
    for src, dst in [
        (work / "local-bin", "/usr/local/bin"),
        (work / "share", "/usr/local/share"),
    ]:
        if libc.mount(str(src).encode(), dst.encode(), None, 4096, None):
            raise OSError(ctypes.get_errno(), "owned fixture bind")
    # Exact Config.Env PATH in the retained root codex-arm64 public-image proof.
    inherited = "/home/agent/.local/bin:/opt/python/bin:/go/bin:/usr/local/go/bin:/usr/local/share/npm-global/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
    recipe = (work / "Dockerfile").read_text()
    prefix = re.findall(r"^ENV PATH=([^\s]+)$", recipe, re.M)
    image_path = prefix[-1].replace("$PATH", inherited) if prefix else inherited
    results = []
    for name, argv, path, input_bytes in [
        (
            "public-command",
            ["/bin/sh", "-c", "command -v codex; codex --version"],
            image_path,
            b"",
        ),
        (
            "entrypoint-version",
            ["/bin/sh", str(work / "entrypoint.sh"), "--version"],
            image_path,
            b"",
        ),
        (
            "entrypoint-pipe",
            ["/bin/sh", str(work / "entrypoint.sh"), "--prompt", "fixture instruction"],
            image_path,
            b"fixture stdin\n",
        ),
        (
            "entrypoint-poison-PATH",
            ["/bin/sh", str(work / "entrypoint.sh"), "--version"],
            inherited,
            b"",
        ),
    ]:
        home = work / name
        home.mkdir()
        env = {
            "PATH": path,
            "HOME": str(home),
            "MARSH_SELECTED_HOME": str(home),
            "SBX_CRED_OPENAI_MODE": "none",
        }
        p = subprocess.run(
            argv,
            input=input_bytes,
            env=env,
            cwd=work / "project",
            capture_output=True,
            timeout=30,
        )
        stdout = p.stdout.decode()
        (work / (name + ".stdout")).write_bytes(p.stdout)
        (work / (name + ".stderr")).write_bytes(p.stderr)
        expected = "codex-cli 0.155.1"
        good = p.returncode == 0 and stdout.splitlines()[-1:] == [expected]
        if name == "public-command":
            good = good and stdout.splitlines()[0] == "/usr/local/bin/codex"
        results.append(
            {
                "control": name,
                "argv": argv,
                "PATH": path,
                "selected_version": "0.155.1",
                "actual_output": stdout,
                "exit": p.returncode,
                "passed": good,
            }
        )
    record = {
        "scope": "Actual shell lookup and unchanged Kit entrypoint; TWO VERSION SENTINELS, not actual native agent execution or baked-image proof.",
        "entrypoint_sha256": hashlib.sha256(
            (work / "entrypoint.sh").read_bytes()
        ).hexdigest(),
        "dockerfile_sha256": hashlib.sha256(
            (work / "Dockerfile").read_bytes()
        ).hexdigest(),
        "results": results,
    }
    (work / "results.json").write_text(json.dumps(record, indent=2) + "\n")
    print(json.dumps(record, indent=2))
    return int(not all(r["passed"] for r in results))


if __name__ == "__main__":
    raise SystemExit(main())
