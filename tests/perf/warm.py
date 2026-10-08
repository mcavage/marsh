#!/usr/bin/env python3
"""Developer probe: warm `marsh -c true` and `marsh -c 'fixture identity'` latency.

Runs against a `make dev` build in an isolated MARSH_HOME. Every stock `sbx`
call is routed through a logging wrapper so each warm invocation also reports
how many `sbx` child processes it started. Removes only VMs that appeared
during the run and stops its own daemon scope.

    python3 tests/perf/warm.py --marsh target/release/marsh \
        --guest-artifacts target/libexec/marsh --kit <fixture ref>
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import pathlib
import shlex
import shutil
import subprocess
import sys
import tempfile
import time


def percentile(values: list[float], proportion: float) -> float:
    ordered = sorted(values)
    return ordered[max(0, math.ceil(proportion * len(ordered)) - 1)]


def sbx_names(sbx: str) -> set[str]:
    listing = subprocess.run([sbx, "ls", "--json"], capture_output=True, check=True, timeout=60)
    document = json.loads(listing.stdout or b"[]")
    rows = document.get("vms", document.get("sandboxes", [])) if isinstance(document, dict) else document
    return {row.get("name") for row in rows if isinstance(row, dict) and row.get("name")}


# The split phases replayed with the same plumbing, timed inside the shell VM.
SPLIT_PHASES = r"""
cd "$1"; t() { date +%s%N; }; idx=$(mktemp); g="git -c core.hooksPath=/dev/null"
s0=$(t); cp "$(git rev-parse --git-path index)" "$idx"
I=$(GIT_INDEX_FILE=$idx $g write-tree); GIT_INDEX_FILE=$idx $g add -A -- :/
S=$(GIT_INDEX_FILE=$idx $g write-tree); H=$(git rev-parse HEAD); s1=$(t)
for b in a b; do W=.marsh/split/perf/$b; $g worktree add -q --detach --no-checkout "$W" "$H"
  $g -C "$W" read-tree "$S"; $g -C "$W" checkout-index -a -q; $g -C "$W" read-tree "$I"
  $g -C "$W" update-index -q --refresh >/dev/null; done; s2=$(t)
for b in a b; do W=.marsh/split/perf/$b; c=$(mktemp); cp "$(git -C "$W" rev-parse --path-format=absolute --git-path index)" "$c"
  GIT_INDEX_FILE=$c $g -C "$W" add -A -- :/; GIT_INDEX_FILE=$c $g -C "$W" write-tree >/dev/null; rm -f "$c"; done; s3=$(t)
for b in a b; do $g worktree remove --force .marsh/split/perf/$b; done; rm -rf .marsh/split/perf "$idx"; s4=$(t)
echo "{\"snapshot_ms\":$(( (s1-s0)/1000000 )),\"allocate_ms\":$(( (s2-s1)/1000000 )),\"capture_ms\":$(( (s3-s2)/1000000 )),\"release_ms\":$(( (s4-s3)/1000000 ))}"
"""


def split_samples(run, directory: pathlib.Path, repo: str, samples: int) -> dict:
    """Two-branch split vs. fanout of the same no-op branches, plus phase replay."""
    files = subprocess.run(["git", "-C", str(directory), "ls-files"], capture_output=True,
                           check=True).stdout.count(b"\n")
    (directory / ".split-perf-untracked").write_text("untracked\n")
    cd = f"cd {directory} && "
    run("-c", cd + "split { a: true, b: true } | join >/dev/null")  # warmup
    split_ms, fanout_ms, manifest_ms, phases = [], [], [], []
    for _ in range(max(3, samples // 4)):
        elapsed, completed = run("-c", cd + "split { a: true, b: true } | join --json")
        split_ms.append(elapsed)
        manifest_ms.append(json.loads(completed.stdout)["total_ms"])
        elapsed, _ = run("-c", cd + "fanout { a: true, b: true } | collect >/dev/null")
        fanout_ms.append(elapsed)
    for _ in range(3):
        _, completed = run("-c", "sh -c " + shlex.quote(SPLIT_PHASES) + " phases " + str(directory))
        phases.append(json.loads(completed.stdout))
    return {"repo": repo, "tracked_files": files,
            "split_p50_ms": round(percentile(split_ms, 0.5)),
            "fanout_p50_ms": round(percentile(fanout_ms, 0.5)),
            "split_manifest_total_p50_ms": round(percentile(manifest_ms, 0.5)),
            "phases_p50_ms": {key: round(percentile([p[key] for p in phases], 0.5))
                              for key in phases[0]}}


def copy_split_samples(run, directory: pathlib.Path, source: str, samples: int) -> dict:
    """Two-branch copy split (plain directory) vs. fanout, plus a Kit branch."""
    files = sum(1 for path in directory.rglob("*") if not path.is_dir())
    size = sum(path.stat().st_size for path in directory.rglob("*") if path.is_file())
    cd = f"cd {directory} && "
    run("-c", cd + "split { a: true, b: true } | join >/dev/null")  # warmup
    split_ms, fanout_ms, manifest_ms, kit_ms = [], [], [], []
    for _ in range(max(3, samples // 4)):
        elapsed, completed = run("-c", cd + "split { a: true, b: true } | join --json")
        split_ms.append(elapsed)
        document = json.loads(completed.stdout)
        if document.get("kind") != "copy":
            raise RuntimeError(f"expected a copy split: {document!r}")
        manifest_ms.append(document["total_ms"])
        elapsed, _ = run("-c", cd + "fanout { a: true, b: true } | collect >/dev/null")
        fanout_ms.append(elapsed)
        elapsed, _ = run("-c", cd + "split { a: fixture identity } | join >/dev/null")
        kit_ms.append(elapsed)
    return {"source": source, "files": files, "bytes": size,
            "split_p50_ms": round(percentile(split_ms, 0.5)),
            "fanout_p50_ms": round(percentile(fanout_ms, 0.5)),
            "split_manifest_total_p50_ms": round(percentile(manifest_ms, 0.5)),
            "kit_branch_split_p50_ms": round(percentile(kit_ms, 0.5))}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--marsh", required=True)
    parser.add_argument("--guest-artifacts", required=True)
    parser.add_argument("--kit", required=True)
    parser.add_argument("--sbx", default=shutil.which("sbx") or "sbx")
    parser.add_argument("--samples", type=int, default=20)
    parser.add_argument("--split-repo", action="append", default=[],
                        help="Git repository to clone into the project and time a "
                             "two-branch split over (repeatable; first one is the project)")
    parser.add_argument("--copy-split", metavar="DIR",
                        help="copy DIR (without .git, target, node_modules) as a plain "
                             "project and time a two-branch copy split over it")
    arguments = parser.parse_args()
    marsh = str(pathlib.Path(arguments.marsh).resolve())
    sbx = str(pathlib.Path(arguments.sbx).resolve())
    before = sbx_names(sbx)
    root = pathlib.Path(tempfile.mkdtemp(prefix="marsh-warm-", dir="/private/tmp"))
    home, project, control = root / "home", root / "project", root / "control"
    home.mkdir(mode=0o700)
    if arguments.split_repo:
        subprocess.run(["git", "clone", "-q", "--local", arguments.split_repo[0], str(project)],
                       check=True)
    elif arguments.copy_split:
        shutil.copytree(arguments.copy_split, project, symlinks=True,
                        ignore=shutil.ignore_patterns(".git", "target", "node_modules", ".marsh"))
    else:
        project.mkdir()
    if len(arguments.split_repo) > 1:
        with open(project / ".git" / "info" / "exclude", "a", encoding="utf-8") as exclude:
            exclude.write(".split-perf-*\n")
    for index, repo in enumerate(arguments.split_repo[1:], start=1):
        subprocess.run(["git", "clone", "-q", "--local", repo, str(project / f".split-perf-{index}")],
                       check=True)
    control.mkdir(mode=0o700)
    scope = control / hashlib.sha256(os.fsencode(home.resolve())).hexdigest()
    scope.mkdir(mode=0o700)
    (scope / "commands.json").write_text(json.dumps({"fixture": arguments.kit}) + "\n")
    log = root / "sbx-calls.log"
    wrapper = root / "sbx"
    wrapper.write_text(
        "#!/bin/sh\n"
        f"printf '%s %s\\n' \"$(date +%s)\" \"$*\" >> '{log}'\n"
        f"exec '{sbx}' \"$@\"\n"
    )
    wrapper.chmod(0o755)
    environment = dict(os.environ, MARSH_HOME=str(home), MARSH_CONTROL_HOME=str(control),
                       MARSH_SBX=str(wrapper), MARSH_GUEST_ARTIFACTS=str(
                           pathlib.Path(arguments.guest_artifacts).resolve()),
                       MARSH_PLACE="local")

    def run(*argv: str, timeout: float = 120) -> tuple[float, subprocess.CompletedProcess[bytes]]:
        started = time.monotonic()
        completed = subprocess.run([marsh, *argv], cwd=project, env=environment,
                                   stdin=subprocess.DEVNULL, capture_output=True, timeout=timeout)
        elapsed = (time.monotonic() - started) * 1000
        if completed.returncode != 0:
            raise RuntimeError(f"{argv!r} failed {completed.returncode}: "
                               f"{completed.stderr.decode(errors='replace')}")
        return elapsed, completed

    def calls() -> list[str]:
        return log.read_text().splitlines() if log.exists() else []

    status = 0
    report: dict = {}
    try:
        cold, _ = run("--load", "fixture", "-c", "true", timeout=600)
        report["cold_load_ms"] = round(cold)
        for label, argv in (("true", ("-c", "true")),
                            ("fixture_identity", ("-c", "fixture identity"))):
            run(*argv)  # discard one warmup
            times, counts, last = [], [], []
            for _ in range(arguments.samples):
                mark = len(calls())
                elapsed, completed = run(*argv)
                new = calls()[mark:]
                times.append(elapsed)
                counts.append(len(new))
                last = new
                if label == "fixture_identity":
                    identity = json.loads(completed.stdout)
                    if identity.get("cwd") != str(project):
                        raise RuntimeError(f"unexpected identity {identity!r}")
            report[label] = {"p50_ms": round(percentile(times, 0.5)),
                             "p95_ms": round(percentile(times, 0.95)),
                             "sbx_calls_p50": percentile(counts, 0.5),
                             "sbx_calls_max": max(counts),
                             "last_sbx_calls": [line.split(" ", 1)[1][:160] for line in last]}
        for index, repo in enumerate(arguments.split_repo):
            report.setdefault("split", []).append(split_samples(
                run, project if index == 0 else project / f".split-perf-{index}", repo,
                arguments.samples))
        if arguments.copy_split and not arguments.split_repo:
            report["copy_split"] = copy_split_samples(run, project, arguments.copy_split,
                                                      arguments.samples)
    except Exception as error:  # noqa: BLE001 - report and clean up
        report["error"] = str(error)
        status = 1
    finally:
        subprocess.run([marsh, "stop", "--json"], cwd=project, env=environment,
                       stdin=subprocess.DEVNULL, capture_output=True, timeout=300)
        # Only VMs this scope's daemon recorded as its own: never another
        # client's VM that happened to appear during the run.
        try:
            owned = set(json.loads((scope / "vm-ownership.json").read_text()).get("vms", {}))
        except (OSError, ValueError):
            owned = set()
        leaked = sorted((sbx_names(sbx) - before) & owned)
        for name in leaked:
            subprocess.run([sbx, "rm", "--force", name], capture_output=True, timeout=120)
        report["removed_leftover_vms"] = leaked
        report["root"] = str(root)
    print(json.dumps(report, indent=2))
    return status


if __name__ == "__main__":
    sys.exit(main())
