#!/usr/bin/env python3
"""Real collector/verifier CLI on public ACP npm bytes; base-only facts are stubs.

No npm install/package scripts, native agent, Docker or SDK execution. Namespace
fixtures are not baked image qualification. Every work directory is retained.
"""

import argparse
import base64
import ctypes
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

HERE = Path(__file__).absolute().parent
ROOT = HERE.parent.parent
sys.path.insert(0, str(HERE))
from public_sources import digest, members


def dump(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def prepare(work, cache, arch):
    bundle = work / "share/licenses/marsh-dhi"
    shutil.copytree(HERE / "collected", bundle)
    for directory in ["bin", "home", "opt", "stubbin", "project"]:
        (work / directory).mkdir()

    def file_row(path, data):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        return {
            "path": str(path),
            "type": "file",
            "uid": 0,
            "gid": 0,
            "mode": 0o644,
            "size": len(data),
            "sha256": hashlib.sha256(data).hexdigest(),
        }

    for name in ["clipboard-bridge", "claude"]:
        file_row(work / "bin" / name, ("BASE STUB " + name).encode())
    core = work / "corepack.cjs"
    core.write_bytes(b"BASE STUB Corepack")
    inventory_name = (
        "installed-package-notices.json"
        if arch == "arm64"
        else "installed-package-notices-amd64.json"
    )
    installed = json.loads((bundle / inventory_name).read_text())
    installed["binaries"] = []
    dump(bundle / inventory_name, installed)
    js = json.loads((bundle / "bundled-js-notices.json").read_text())
    for row in js:
        row.update(path=str(core), source_sha256=digest(core))
    dump(bundle / "bundled-js-notices.json", js)
    deltas = json.loads((bundle / "agent-payload-deltas.json").read_text())
    deltas["architectures"][arch]["clipboard_variants"].append(
        {"image": "BASE-FIXTURE", "sha256": digest(work / "bin/clipboard-bridge")}
    )
    dump(bundle / "agent-payload-deltas.json", deltas)
    agents = json.loads((bundle / "agent-notices.json").read_text())
    obligations = json.loads((bundle / "payload-obligations.json").read_text())
    for name, base in agents["agents_by_architecture"][arch].items():
        marker = Path(base["identity_marker"])
        local = (
            work / "share" / marker.relative_to("/usr/local/share")
            if name == "codex"
            else work / "home" / marker.relative_to("/home")
        )
        row = file_row(local, b"BASE STUB maintained marker")
        row["path"] = str(marker)
        base["payload_entries"] = [row]
        if name == "codex":
            base["payload_entries"].insert(
                0,
                {
                    "path": str(marker.parent),
                    "type": "directory",
                    "mode": 0o755,
                    "uid": 0,
                    "gid": 0,
                },
            )
        obligations["agents"][arch][name]["binaries"] = []
    dump(bundle / "agent-notices.json", agents)
    dump(bundle / "payload-obligations.json", obligations)
    primary = json.loads(
        (bundle / "source-records/public-image-primary-receipt.json").read_text()
    )
    for row in primary["files"]:
        file_row(
            work / "opt" / Path(row["path"]).relative_to("/opt"),
            base64.b64decode(row["base64"], validate=True),
        )
    policy = json.loads((bundle / "adapter-payloads.json").read_text())["adapters"]
    for adapter in ["codex-acp", "claude-acp"]:
        root = work / "opt" / Path(policy[adapter]["root"]).relative_to("/opt")
        root.mkdir(parents=True, exist_ok=True)
        for name in ["package-lock.json", "notices/overrides.json"]:
            target = root / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / "kits" / ("marsh-" + adapter) / name, target)
        shutil.copy2(ROOT / "scripts/npm-notices.mjs", root / "collect-notices.mjs")
        for package in policy[adapter]["architectures"][arch]["packages"]:
            record = package["acquisition"]
            source = cache / hashlib.sha256(record["url"].encode()).hexdigest()
            if (
                digest(source) != record["sha256"]
                or base64.b64encode(bytes.fromhex(digest(source, "sha512"))).decode()
                != record["integrity"].split("-", 1)[1]
            ):
                raise ValueError("public archive identity changed")
            destination = work / "opt" / Path(package["root"]).relative_to("/opt")
            for name, data, mode in members(source, record["sha256"]):
                path = destination / Path(name).relative_to("package")
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(data)
                path.chmod(mode & 0o777)
        for row in policy[adapter]["architectures"][arch]["links"]:
            path = work / "opt" / Path(row["path"]).relative_to("/opt")
            path.parent.mkdir(parents=True, exist_ok=True)
            path.symlink_to(row["target"])
    (work / "stubbin/dpkg").write_text('#!/bin/sh\nprintf "' + arch + '\\n"\n')
    (work / "stubbin/pkgs").write_text(
        "".join(
            r["package"] + "\t" + r["version"] + "\n" for r in installed["packages"]
        )
    )
    (work / "stubbin/dpkg-query").write_text(
        '#!/bin/sh\nexec /bin/cat "' + str(work / "stubbin/pkgs") + '"\n'
    )
    for name in ["dpkg", "dpkg-query"]:
        (work / "stubbin" / name).chmod(0o755)
    dump(
        work / "fixture-scope.json",
        {
            "scope": "Unmodified verifier and npm collector; actual selected public ACP package bytes and primary legal texts. Maintained base agent/common binary facts are explicit stubs. Collector architecture field normalized for cross-architecture filesystem tests only; no actual image/npm install/agent execution.",
            "architecture": arch,
            "verifier_sha256": digest(bundle / "verify.py"),
            "collector_sha256": digest(ROOT / "scripts/npm-notices.mjs"),
        },
    )


def inside(work, arch):
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.mount(b"none", b"/", None, (1 << 18) | 16384, None):
        raise OSError(ctypes.get_errno(), "private mounts")
    for source, target in [
        (work / "share", "/usr/local/share"),
        (work / "bin", "/usr/local/bin"),
        (work / "home", "/home"),
        (work / "opt", "/opt"),
    ]:
        if libc.mount(str(source).encode(), target.encode(), None, 4096, None):
            raise OSError(ctypes.get_errno(), "fixture bind")
    bundle = work / "share/licenses/marsh-dhi"
    env = {
        "PATH": str(work / "stubbin") + ":/usr/bin:/bin",
        "HOME": str(work / "project"),
    }
    results = []
    for adapter in ["codex-acp", "claude-acp"]:
        root = Path("/opt/marsh") / adapter
        notices = Path("/usr/local/share/licenses") / ("marsh-" + adapter)
        argv = [
            "/usr/bin/node",
            str(root / "collect-notices.mjs"),
            str(root / "node_modules"),
            str(notices),
        ]
        p = subprocess.run(argv, env=env, capture_output=True, timeout=60)
        (work / (adapter + "-collector.stdout")).write_bytes(p.stdout)
        (work / (adapter + "-collector.stderr")).write_bytes(p.stderr)
        if p.returncode:
            raise ValueError(p.stderr.decode())
        path = notices / "npm-package-notices.json"
        d = json.loads(path.read_text())
        d["architecture"] = "arm64" if arch == "arm64" else "x64"
        dump(path, d)

        def call(label, expected):
            cmd = [
                sys.executable,
                "-I",
                "-S",
                "-B",
                str(bundle / "verify.py"),
                "--adapter",
                adapter,
                "--receipt",
            ]
            p = subprocess.run(cmd, env=env, capture_output=True, timeout=120)
            (work / (label + ".stdout")).write_bytes(p.stdout)
            (work / (label + ".stderr")).write_bytes(p.stderr)
            results.append(
                {
                    "control": label,
                    "argv": cmd,
                    "exit": p.returncode,
                    "expected_success": expected,
                    "as_expected": (p.returncode == 0) == expected,
                    "stderr_sha256": hashlib.sha256(p.stderr).hexdigest(),
                }
            )
            if p.returncode == 0:
                r = json.loads(p.stdout)
                assert (
                    r["stage"] == "final" and r["scope_kind"] == "adapter-final-scoped"
                )
            return p.returncode

        if call(adapter + "-actual-public-positive", True):
            continue
        package = (
            root
            / "node_modules"
            / (
                "@openai/codex-linux-" + ("arm64" if arch == "arm64" else "x64")
                if adapter == "codex-acp"
                else "@anthropic-ai/claude-agent-sdk-linux-"
                + ("arm64" if arch == "arm64" else "x64")
            )
        )
        legal = (
            next(package.rglob("LGPL-2.1.txt"))
            if adapter == "codex-acp"
            else package / "LICENSE.md"
        )
        held = work / (adapter + "-held-legal")
        local_legal = work / "opt" / legal.relative_to("/opt")
        local_legal.rename(held)
        call(adapter + "-missing-installed-legal", False)
        held.rename(local_legal)
        native = (
            next(package.rglob("bin/codex"))
            if adapter == "codex-acp"
            else package / "claude"
        )
        mode = native.stat().st_mode & 0o7777
        native.chmod(0o4755)
        call(adapter + "-setuid-denied", False)
        native.chmod(mode)
        extra = package / "unindexed.js"
        extra.write_text("not executed")
        call(adapter + "-extra-member-denied", False)
        extra.unlink()
        notice_index = notices / "npm-package-notices.json"
        saved = notice_index.read_bytes()
        d = json.loads(saved)
        d["package_lock_sha256"] = "0" * 64
        dump(notice_index, d)
        call(adapter + "-stale-collector-lock", False)
        notice_index.write_bytes(saved)
        required = json.loads((bundle / "adapter-payloads.json").read_text())[
            "adapters"
        ][adapter]["architectures"][arch]["required_notices"]
        legal_ref = next(r for r in required if r["sha256"] == digest(legal))
        source = bundle / legal_ref["file"]
        saved_text = source.read_bytes()
        source.unlink()
        call(adapter + "-missing-canonical-required-text", False)
        source.write_bytes(saved_text)
        call(adapter + "-restored-positive", True)
    dump(work / "results.json", results)
    print(json.dumps(results, indent=2))
    return int(not all(r["as_expected"] for r in results))


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--cache", type=Path)
    p.add_argument("--work", type=Path, required=True)
    p.add_argument("--architecture", choices=["arm64", "amd64"], default="arm64")
    p.add_argument("--inside", action="store_true")
    a = p.parse_args()
    work = a.work.absolute()
    if a.inside:
        return inside(work, a.architecture)
    work.mkdir(mode=0o700, exist_ok=False)
    prepare(work, a.cache.absolute(), a.architecture)
    return subprocess.run(
        [
            "unshare",
            "-rm",
            sys.executable,
            "-I",
            "-B",
            str(Path(__file__).absolute()),
            "--inside",
            "--work",
            str(work),
            "--architecture",
            a.architecture,
        ],
        timeout=1200,
    ).returncode


if __name__ == "__main__":
    raise SystemExit(main())
