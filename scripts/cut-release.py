#!/usr/bin/env python3
"""Cut a stable release from a Mac: check, run the full regression, tag, push.

    make release VERSION=X.Y.Z [DRY_RUN=1] [YES=1]

CI cannot start a microVM, so the stock-SBX regression runs here. This is the
one way to release: it checks that main is clean, matches origin and passed CI,
runs `make regress` (every sbx suite, then the full TLA+ model) on that exact
commit, and only then makes an annotated tag whose message records the result.
The Release workflow refuses a tag that lacks that record, so a tag pushed any
other way does not publish.

  DRY_RUN=1  check and run the regression, then stop without tagging.
  YES=1      do not ask before pushing the tag (which starts the release).
"""
from __future__ import annotations

import argparse
import datetime
import json
import os
import pathlib
import platform
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
VERSION = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")
SUMMARY = pathlib.Path("target/regress/last.json")


class Refusal(Exception):
    """A reason not to release; nothing has been tagged."""


def text(*argv: str, check: bool = True) -> str:
    try:
        result = subprocess.run(argv, cwd=ROOT, capture_output=True, text=True)
    except FileNotFoundError:
        if check:
            raise Refusal(f"`{argv[0]}` is not installed") from None
        return ""
    if check and result.returncode:
        detail = (result.stderr or result.stdout).strip()
        raise Refusal(f"`{' '.join(argv)}` failed: {detail}")
    return result.stdout.strip()


def head() -> str:
    return text("git", "rev-parse", "HEAD")


def check_machine() -> None:
    if platform.system() != "Darwin" or platform.machine() != "arm64":
        raise Refusal("run this on the Apple Silicon Mac: the regression starts real microVMs")
    if subprocess.run(["docker", "info"], cwd=ROOT, capture_output=True).returncode:
        raise Refusal("Docker is not running: start Docker Desktop (make dev builds the guest with Buildx)")
    text("sbx", "version")


def check_git(version: str) -> str:
    """The commit to release, once it is clean, on main, and the same as origin's."""
    if not VERSION.match(version):
        raise Refusal(f"bad version {version!r}: use X.Y.Z, without a leading v")
    branch = text("git", "rev-parse", "--abbrev-ref", "HEAD")
    if branch != "main":
        raise Refusal(f"on {branch}, not main")
    if text("git", "status", "--porcelain"):
        raise Refusal("the working tree has uncommitted or untracked files")
    text("git", "fetch", "--quiet", "--tags", "origin", "main")
    sha = head()
    if sha != text("git", "rev-parse", "origin/main"):
        raise Refusal("main differs from origin/main: pull or push first")
    tag = f"v{version}"
    if text("git", "tag", "--list", tag) or text("git", "ls-remote", "--tags", "origin", f"refs/tags/{tag}"):
        raise Refusal(f"tag {tag} already exists")
    return sha


def repository() -> str:
    explicit = os.environ.get("GITHUB_REPOSITORY")
    if explicit:
        return explicit
    url = text("git", "remote", "get-url", "origin")
    match = re.search(r"github\.com[:/](.+?)(?:\.git)?$", url)
    if not match:
        raise Refusal(f"cannot find the GitHub repository in {url}")
    return match.group(1)


def check_ci(sha: str) -> None:
    runs = text("gh", "api", f"repos/{repository()}/actions/workflows/ci.yml/runs?head_sha={sha}&event=push&status=success",
                "--jq", ".total_count")
    if int(runs or 0) < 1:
        raise Refusal(f"CI has not passed on {sha[:7]}: wait for CI on main to finish")


def regress() -> dict:
    """Run the full regression and return its summary; refuse on any failure."""
    (ROOT / SUMMARY).unlink(missing_ok=True)
    for argv in (["make", "fixture-ref"], ["make", "regress"]):
        if subprocess.run(argv, cwd=ROOT).returncode:
            raise Refusal(f"`{' '.join(argv)}` failed; nothing was tagged")
    path = ROOT / SUMMARY
    if not path.is_file():
        raise Refusal(f"the regression wrote no {SUMMARY}")
    return json.loads(path.read_text())


def verify(summary: dict, sha: str) -> None:
    if summary.get("tier") != "regress":
        raise Refusal(f"the summary is for tier {summary.get('tier')!r}, not regress")
    bad = [f"{s['name']} ({s['status']})" for s in summary.get("suites", []) if s.get("status") != "PASS"]
    if bad or not summary.get("ok") or not summary.get("suites"):
        raise Refusal("the regression did not pass: " + (", ".join(bad) or "see its summary"))
    if summary.get("source_revision") != sha:
        raise Refusal(f"the regression ran on {str(summary.get('source_revision'))[:7]}, not {sha[:7]}")


def message(version: str, sha: str, summary: dict) -> str:
    when = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    minutes = round(summary.get("wall_seconds", 0) / 60)
    sbx = re.findall(r"v[0-9]+\.[0-9]+\.[0-9]+\S*", text("sbx", "version", check=False))[0:1]
    return (f"marsh {version}\n\n"
            f"Regression: passed on {sha[:7]} ({len(summary['suites'])} suites, {minutes} min)\n"
            f"Run: {when}\n"
            f"Host: macOS {platform.mac_ver()[0]} {platform.machine()}, sbx {sbx[0] if sbx else 'unknown'}\n")


def release(argv: list[str], *, machine=check_machine, ci=check_ci, run_regress=regress, ask=input) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("version", help="X.Y.Z")
    parser.add_argument("--dry-run", action="store_true", help="run the regression but do not tag")
    parser.add_argument("--yes", action="store_true", help="push the tag without asking")
    args = parser.parse_args(argv)
    try:
        sha = check_git(args.version)
        machine()
        ci(sha)
        print(f"release {args.version}: {sha[:7]} is clean, on main, and passed CI. Running the full regression.",
              flush=True)
        summary = run_regress()
        verify(summary, sha)
        # The regression takes a long time: the commit must still be the tip of main.
        if check_git(args.version) != sha:
            raise Refusal("main moved during the regression")
        note = message(args.version, sha, summary)
    except Refusal as refusal:
        print(f"release: {refusal}", file=sys.stderr)
        return 1
    print(f"\nregression passed. Tag v{args.version}:\n\n{note}")
    if args.dry_run:
        print("dry run: not tagging.")
        return 0
    if not args.yes and ask(f"Tag v{args.version} at {sha[:7]} and push it? This starts the release. [y/N] ").strip().lower() != "y":
        print("not tagging.")
        return 1
    text("git", "tag", "-a", f"v{args.version}", "-m", note, sha)
    text("git", "push", "origin", f"v{args.version}")
    print(f"pushed v{args.version}. Watch the Release workflow: gh run watch")
    return 0


if __name__ == "__main__":
    raise SystemExit(release(sys.argv[1:]))
