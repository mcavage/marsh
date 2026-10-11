#!/usr/bin/env python3
"""Regenerate the Claude and Codex Kits' release-checksums.txt from the publishers.

Each Kit declares the CLI version it installs in its descriptor
(kits/marsh-claude/claude.yaml, kits/marsh-codex/codex.yaml, `args.version.default`)
and verifies the downloaded bytes against its release-checksums.txt before
installing anything (docs/design/supply-chain.md). When the declared version
changes, this rewrites the checksum file from the publisher's own metadata:

  claude  https://downloads.claude.ai/claude-code-releases/VERSION/manifest.json
  codex   https://api.github.com/repos/openai/codex/releases/tags/rust-vVERSION

Renovate runs it after bumping a descriptor. The output is a diff for a human
to review against the publisher; it does not replace that review. Files already
matching their version are left alone. Stdlib only.

  refresh-agent-checksums.py            update both Kits
  refresh-agent-checksums.py --check    exit 1 if a file is stale; write nothing
"""
from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import sys
import urllib.error
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[1]
CLAUDE_BASE = "https://downloads.claude.ai/claude-code-releases"
GITHUB_API = "https://api.github.com"
SEMVER = re.compile(r"\d+\.\d+\.\d+")
SHA256 = re.compile(r"[0-9a-f]{64}")
CODEX_TARGETS = ("aarch64-unknown-linux-musl", "x86_64-unknown-linux-musl")
CODEX_BINARIES = ("codex", "codex-code-mode-host")


def fetch_json(url: str) -> dict:
    request = urllib.request.Request(url, headers={"Accept": "application/json", "User-Agent": "marsh-refresh-checksums"})
    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    if token and url.startswith(GITHUB_API):
        request.add_header("Authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        raise SystemExit(f"refresh-agent-checksums: {url}: HTTP {error.code}") from error


def declared_version(descriptor: pathlib.Path) -> str:
    match = re.search(r'^args:\n  version:\n    default: "([^"]+)"', descriptor.read_text(), re.M)
    if not match or not SEMVER.fullmatch(match.group(1)):
        raise SystemExit(f"refresh-agent-checksums: no args.version.default in {descriptor}")
    return match.group(1)


def claude_lines(version: str, base: str) -> tuple[str, list[str]]:
    url = f"{base}/{version}/manifest.json"
    manifest = fetch_json(url)
    if manifest.get("version") != version:
        raise SystemExit(f"refresh-agent-checksums: {url} describes {manifest.get('version')!r}, not {version}")
    lines = []
    for platform in ("linux-arm64", "linux-x64"):
        checksum = manifest.get("platforms", {}).get(platform, {}).get("checksum")
        if not isinstance(checksum, str) or not SHA256.fullmatch(checksum):
            raise SystemExit(f"refresh-agent-checksums: {url} has no sha256 for {platform}")
        lines.append(f"{checksum}  {version}/{platform}/claude")
    return url, lines


def codex_lines(version: str, api: str) -> tuple[str, list[str]]:
    url = f"{api}/repos/openai/codex/releases/tags/rust-v{version}"
    release = fetch_json(url)
    digests = {asset["name"]: asset.get("digest") for asset in release.get("assets", [])}
    lines = []
    for binary in CODEX_BINARIES:
        for target in CODEX_TARGETS:
            name = f"{binary}-{target}.tar.gz"
            digest = digests.get(name) or ""
            if not digest.startswith("sha256:") or not SHA256.fullmatch(digest[7:]):
                raise SystemExit(f"refresh-agent-checksums: {url} has no sha256 digest for {name}")
            lines.append(f"{digest[7:]}  {version}/{name}")
    # The committed order is both CLI archives, then both code-mode hosts.
    return url, lines


def render(url: str, lines: list[str]) -> str:
    return f"# Publisher metadata: {url}\n" + "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--check", action="store_true", help="report stale files; do not write")
    parser.add_argument("--root", type=pathlib.Path, default=ROOT)
    parser.add_argument("--claude-base", default=CLAUDE_BASE, help=argparse.SUPPRESS)
    parser.add_argument("--github-api", default=GITHUB_API, help=argparse.SUPPRESS)
    args = parser.parse_args()

    kits = (
        ("claude", "kits/marsh-claude", "claude.yaml", lambda v: claude_lines(v, args.claude_base)),
        ("codex", "kits/marsh-codex", "codex.yaml", lambda v: codex_lines(v, args.github_api)),
    )
    stale = False
    for name, directory, descriptor, source in kits:
        version = declared_version(args.root / directory / descriptor)
        target = args.root / directory / "release-checksums.txt"
        expected = render(*source(version))
        if target.read_text() == expected:
            print(f"{name} {version}: release-checksums.txt is current")
            continue
        stale = True
        if args.check:
            print(f"{name} {version}: release-checksums.txt is stale")
        else:
            target.write_text(expected)
            print(f"{name} {version}: updated {target.relative_to(args.root)}")
    return 1 if args.check and stale else 0


if __name__ == "__main__":
    sys.exit(main())
