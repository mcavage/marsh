#!/usr/bin/env python3
"""Live check: retired Kit images move to marsh-tools.

A throwaway MARSH_HOME/control scope is pre-seeded the way an older marsh left
it: `gog` installed from the tag docker.io/mcavage/marsh-gog:stable, `bamboohr`
by a digest of docker.io/mcavage/marsh-bamboohr, and `other` pinned to a digest
of an unrelated Kit. Starting the daemon must move the first two to
docker.io/mcavage/marsh-tools:stable (same names, one shared image, the tag
recorded so they follow it), leave `other` alone, say so once in `marsh status`,
and keep both commands working. Cleanup is `marsh stop` of that scope, then proof
that its own VMs are gone and every pre-existing VM is unchanged.

Needs network (the three images are pulled) and an installed product, for
example `make dev` and then:

    tests/acceptance/kit_retired_live.py --prefix ~/.marsh-dev
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import time

from harness import disposable_root, product_owned_vm_names, scoped_control_home
from launch_env import clean_environment

OLD_TAGS = {"gog": "docker.io/mcavage/marsh-gog:stable", "bamboohr": "docker.io/mcavage/marsh-bamboohr:stable"}
TOOLS_TAG = "docker.io/mcavage/marsh-tools:stable"
UNRELATED_TAG = "docker.io/mcavage/marsh-pix:stable"


def resolve(sbx: Path, tag: str) -> str:
    output = subprocess.run([str(sbx), "kit", "inspect", tag, "--format", "json"],
                            capture_output=True, check=True, timeout=300).stdout
    return json.loads(output)["sandbox"]["image"]


def vm_names(sbx: Path) -> set[str]:
    listing = subprocess.run([str(sbx), "ls", "--json"], capture_output=True, check=True, timeout=60).stdout
    listed = json.loads(listing)
    return {item["name"] for item in (listed["sandboxes"] if isinstance(listed, dict) else listed)}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--prefix", type=Path, required=True, help="installed product, e.g. ~/.marsh-dev")
    parser.add_argument("--sbx", type=Path, default=Path("sbx"))
    parser.add_argument("--timeout", type=int, default=900, help="seconds to wait for the move")
    args = parser.parse_args()
    prefix = args.prefix.expanduser().resolve(strict=True)
    marsh = prefix / "bin" / "marsh"
    guest = prefix / "libexec" / "marsh"
    sbx = Path(subprocess.run(["which", str(args.sbx)], capture_output=True, text=True, check=True).stdout.strip())
    before = vm_names(sbx)

    old = {name: resolve(sbx, tag) for name, tag in OLD_TAGS.items()}
    other = resolve(sbx, UNRELATED_TAG)
    tools = resolve(sbx, TOOLS_TAG)
    print(f"seeding gog={old['gog']}\n        bamboohr={old['bamboohr']}\n        other={other}\ntools now: {tools}")

    root = disposable_root("marsh-kit-retired-")
    project, home, control = root / "project", root / "scope", root / "control"
    for directory in (project, home, control):
        directory.mkdir(mode=0o700)
    (home / "home").mkdir(mode=0o700)
    control_home = scoped_control_home(control, home)
    registry = control_home / "commands.json"
    registry.write_text(json.dumps({"gog": old["gog"], "bamboohr": old["bamboohr"], "other": other}) + "\n")
    registry.chmod(0o600)
    sources = control_home / "kit-sources.json"
    sources.write_text(json.dumps({"gog": OLD_TAGS["gog"]}) + "\n")
    sources.chmod(0o600)
    environment = {**clean_environment(), "MARSH_HOME": str(home), "MARSH_CONTROL_HOME": str(control),
                   "MARSH_GUEST_ARTIFACTS": str(guest), "MARSH_SBX": str(sbx), "MARSH_PLACE": "local"}

    def run(*arguments: str, timeout: int = 600) -> subprocess.CompletedProcess[bytes]:
        return subprocess.run([str(marsh), *arguments], cwd=project, env=environment,
                              capture_output=True, timeout=timeout)

    owned: set[str] = set()
    failure = None
    try:
        # Any command starts the daemon, whose background loop makes the move.
        run("status", timeout=120)
        deadline = time.monotonic() + args.timeout
        while time.monotonic() < deadline:
            moved = json.loads(registry.read_text())
            # The tag is recorded just after each command's switch.
            if (moved["gog"] == tools and moved["bamboohr"] == tools
                    and json.loads(sources.read_text()) == {"gog": TOOLS_TAG, "bamboohr": TOOLS_TAG}):
                break
            time.sleep(5)
        else:
            raise AssertionError(f"not moved within {args.timeout}s: {registry.read_text()}")
        assert moved["other"] == other, f"an unrelated pin changed: {moved}"
        recorded = json.loads(sources.read_text())
        assert recorded == {"gog": TOOLS_TAG, "bamboohr": TOOLS_TAG}, recorded
        owned |= product_owned_vm_names(control_home)

        status = run("status").stdout.decode()
        assert "gog and bamboohr moved to marsh-tools" in status, status
        assert status.count("moved to marsh-tools") == 1, status
        document = json.loads(run("status", "--json").stdout)
        notes = [update.get("note") for update in document["kit_updates"]["recent"]]
        assert notes == ["gog and bamboohr moved to marsh-tools"] * 2, notes

        # Same names, same behaviour, now from the one marsh-tools VM.
        version = run("-c", "gog version", timeout=900)
        assert version.returncode == 0 and version.stdout.strip(), (version.stdout, version.stderr)
        refused = run("-c", "gog gmail send --to a@b.c --subject s --body b", timeout=300)
        assert b"is not enabled" in refused.stdout + refused.stderr, (refused.stdout, refused.stderr)
        denied = run("-c", "bamboohr update-employee", timeout=300)
        assert b"operation is not allowed" in denied.stdout + denied.stderr, (denied.stdout, denied.stderr)
        owned |= product_owned_vm_names(control_home)
        workers = json.loads(run("status", "--json").stdout)["workers"]
        assert any(worker.get("kit_profile") == tools for worker in workers), workers
        assert not any(worker.get("kit_profile") in old.values() for worker in workers), workers
        print("moved, noted once in marsh status, and both commands run from marsh-tools")
    except Exception as error:  # noqa: BLE001
        failure = error
    finally:
        stopped = run("stop", timeout=300)
        owned |= product_owned_vm_names(control_home)
        leftovers = (vm_names(sbx) & owned)
        extra = vm_names(sbx) - before - owned
        problems = []
        if stopped.returncode != 0:
            problems.append(f"marsh stop failed: {stopped.stderr.decode(errors='replace')}")
        if leftovers:
            problems.append(f"this scope's VMs remain: {sorted(leftovers)}")
        if before - vm_names(sbx):
            problems.append(f"pre-existing VMs disappeared: {sorted(before - vm_names(sbx))}")
        if extra:
            problems.append(f"unexplained new VMs: {sorted(extra)}")
        print("cleanup:", "verified" if not problems else problems)
        if failure is None and problems:
            failure = AssertionError("; ".join(problems))
    if failure is not None:
        print(f"FAILED: {failure}", file=sys.stderr)
        sys.exit(1)
    print("ok")


if __name__ == "__main__":
    main()
