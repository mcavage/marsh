#!/usr/bin/env python3
"""Stamp one version into this checkout's Cargo.toml and Cargo.lock.

Release CI runs this on its own checkout before building, so a tag (or a
nightly's derived version) is the version that `marsh --version` reports. The
result is never committed. Every workspace crate must inherit the workspace
version, so the root manifest is the only place a version is written.
"""
from __future__ import annotations

import argparse
import pathlib
import re
import sys

SEMVER = re.compile(r"\d+\.\d+\.\d+(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?")


def members(root: pathlib.Path) -> list[str]:
    text = (root / "Cargo.toml").read_text()
    match = re.search(r"^members\s*=\s*\[(.*?)\]", text, re.S | re.M)
    if not match:
        raise SystemExit("stamp-version: no workspace members in Cargo.toml")
    names = []
    for path in re.findall(r'"([^"]+)"', match.group(1)):
        manifest = (root / path / "Cargo.toml").read_text()
        package = manifest.split("[package]", 1)[1].split("\n[", 1)[0]
        if not re.search(r"^version\.workspace\s*=\s*true\s*$", package, re.M):
            raise SystemExit(f"stamp-version: {path}/Cargo.toml must use version.workspace = true")
        names.append(re.search(r'^name\s*=\s*"([^"]+)"', package, re.M).group(1))
    return names


def stamp(root: pathlib.Path, version: str) -> str:
    names = set(members(root))
    manifest = root / "Cargo.toml"
    text = manifest.read_text()
    head, sep, tail = text.partition("[workspace.package]")
    if not sep:
        raise SystemExit("stamp-version: no [workspace.package] in Cargo.toml")
    found = re.search(r'^version = "([^"]+)"', tail, re.M)
    if not found:
        raise SystemExit("stamp-version: no version in [workspace.package]")
    old = found.group(1)
    tail = tail[: found.start()] + f'version = "{version}"' + tail[found.end() :]
    manifest.write_text(head + sep + tail)

    lock = root / "Cargo.lock"
    blocks = lock.read_text().split("\n[[package]]\n")
    stamped = 0
    for index, block in enumerate(blocks):
        name = re.search(r'^name = "([^"]+)"', block, re.M)
        if name and name.group(1) in names and not re.search(r"^source = ", block, re.M):
            blocks[index], n = re.subn(r'^version = "[^"]+"', f'version = "{version}"', block, count=1, flags=re.M)
            stamped += n
    if stamped != len(names):
        raise SystemExit(f"stamp-version: Cargo.lock lists {stamped} of {len(names)} workspace crates")
    lock.write_text("\n[[package]]\n".join(blocks))
    return old


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version", help="X.Y.Z or X.Y.Z-prerelease")
    parser.add_argument("--root", type=pathlib.Path, default=pathlib.Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    version = args.version.removeprefix("v")
    if not SEMVER.fullmatch(version):
        parser.error("version must be X.Y.Z or X.Y.Z-prerelease")
    old = stamp(args.root, version)
    print(f"stamped {old} -> {version}", file=sys.stderr)


if __name__ == "__main__":
    main()
