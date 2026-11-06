#!/usr/bin/env python3
"""Live check: a Kit's bundled skills win over the shared sbx skills store.

A throwaway scope registers a local Kit (built on the fixture Kit) that
bundles one skill (`agent-skill@1`) under a unique name, and a throwaway
skills folder holds a skill of that name.

1. An earlier marsh's import: with the Kit not yet registered, the mirror
   imports the folder's skill into the store. Once the Kit is registered,
   the daemon removes it again and lists the clash; the Kit VM creates and
   the job sees the Kit's copy.
2. A store skill marsh did not import (as `sbx skills add` leaves) stays;
   the Kit VM create falls back to `--skills off`, the job sees the Kit's
   copy, and `marsh status` says the Kit runs without shared skills.

Stock SBX consults the store only when the VM's skills mode is not off, so
the sbx this scope uses passes `--skills readonly` to a create that names
no mode (the stock default when `skills.defaultMode` is unset). The store is
the user's global one: only the unique test name is added, and it is removed
at the end.

    python3 tests/acceptance/skills_clash_uat.py --prefix DEV_PREFIX --kit FIXTURE_REF
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import secrets
import shutil
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from launch_env import clean_environment  # noqa: E402

KIT_YAML = """\
# syntax=docker/sandbox-kit:3@sha256:11bb68806aef6a68d45c10c0c3b3dc86c2f794a296b5a933661d9dadf760b753
schemaVersion: "3"
displayName: marsh skills clash fixture
kind: workload
provides: ["marsh-skills-clash-fixture@1.0.0"]
dockerfile: ./clashkit.dockerfile
capabilities:
  - type: com.docker.sandbox/sbx@1
  - type: com.docker.sandbox/agent-skills@1
    config:
      path: /opt/clashkit/skills
  - type: com.docker.sandbox/agent-skill@1
    config:
      path: /opt/clashkit/bundled/{name}
"""

SBX_WRAPPER = """\
#!{python}
import os, sys
real = {real!r}
args = sys.argv[1:]
if args[:1] == ["create"] and not any(a == "--skills" or a.startswith("--skills=") for a in args):
    args = ["create", "--skills", "readonly", *args[1:]]
os.execv(real, [real, *args])
"""


def skill(directory: pathlib.Path, name: str, body: str) -> None:
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "SKILL.md").write_text(f"---\nname: {name}\ndescription: clash fixture\n---\n{body}\n")


class Scope:
    def __init__(self, root: pathlib.Path, prefix: pathlib.Path, sbx: str, kit: str) -> None:
        self.root = root
        self.sbx = sbx
        self.marsh = str(prefix / "bin" / "marsh")
        self.name = f"mskc-{secrets.token_hex(4)}"
        self.home = root / "home"
        self.skills_home = root / "skills-home"
        self.project = root / "project"
        self.kit_dir = root / "clashkit"
        for directory in (self.home, self.project):
            directory.mkdir(parents=True)
        control_root = root / "control"
        self.control = control_root / hashlib.sha256(os.fsencode(self.home.resolve())).hexdigest()
        self.control.mkdir(parents=True)
        for directory in (control_root, self.control):
            directory.chmod(0o700)
        skill(self.skills_home / ".agents/skills" / self.name, self.name, "HOST COPY")
        skill(self.kit_dir / "bundled" / self.name, self.name, "KIT COPY")
        (self.kit_dir / "clashkit.yaml").write_text(KIT_YAML.format(name=self.name))
        (self.kit_dir / "clashkit.dockerfile").write_text(f"FROM {kit}\nCOPY bundled /opt/clashkit/bundled\n")
        wrapper = root / "bin" / "sbx"
        wrapper.parent.mkdir()
        wrapper.write_text(SBX_WRAPPER.format(python=sys.executable, real=sbx))
        wrapper.chmod(0o755)
        self.environment = clean_environment()
        self.environment.update({
            "MARSH_HOME": str(self.home),
            "MARSH_CONTROL_HOME": str(control_root),
            "MARSH_SBX": str(wrapper),
            "MARSH_GUEST_ARTIFACTS": str(prefix / "libexec" / "marsh"),
            "MARSH_SKILLS_HOME": str(self.skills_home),
        })

    def run(self, *args: str, timeout: float = 120) -> subprocess.CompletedProcess[str]:
        return subprocess.run([self.marsh, *args], cwd=self.project, env=self.environment, text=True,
                              capture_output=True, timeout=timeout, check=False)

    def register(self, commands: dict[str, str]) -> None:
        stopped = self.run("stop", timeout=300)
        require(stopped.returncode == 0, f"marsh stop: {stopped.stderr}")
        (self.control / "commands.json").write_text(json.dumps(commands) + "\n")

    def mirror(self) -> dict:
        status = self.run("status", "--json")
        require(status.returncode == 0, f"marsh status: {status.stderr}")
        return json.loads(status.stdout).get("skills_mirror") or {}

    def wait_mirror(self, done, what: str, timeout: float = 120) -> dict:
        deadline = time.monotonic() + timeout
        while True:
            mirror = self.mirror()
            if done(mirror):
                return mirror
            require(time.monotonic() < deadline, f"{what}; skills_mirror: {mirror}")
            time.sleep(2)

    def store(self) -> dict[str, str]:
        listed = json.loads(subprocess.run([self.sbx, "skills", "ls", "--json"], capture_output=True,
                                           text=True, check=True, timeout=60).stdout)
        directory = pathlib.Path(listed["store"])
        return {name: (directory / name / "SKILL.md").read_text() for name in listed["skills"]
                if name == self.name}

    def job_reads_kit_copy(self) -> None:
        """The job starts (its VM was created), and the skills folder stock
        SBX prepared in that VM, which a published Kit's jobs get bound
        read-only (a local source Kit's jobs get no Kit skills), holds the
        Kit's copy."""
        job = self.run("-c", "clashkit exec true", timeout=1800)
        require(job.returncode == 0, f"clashkit job exited {job.returncode}: {job.stderr[-2000:]}")
        status = json.loads(self.run("status", "--json").stdout)
        vms = [worker["vm_id"] for worker in status.get("workers", []) if "clashkit" in worker.get("kits", [])]
        require(len(vms) == 1, f"no single clashkit worker: {status.get('workers')}")
        seen = subprocess.run([self.sbx, "exec", vms[0], "cat", f"/opt/clashkit/skills/{self.name}/SKILL.md"],
                              capture_output=True, text=True, timeout=60, check=False)
        require("KIT COPY" in seen.stdout, f"the Kit VM's skills folder: {seen.stdout!r} {seen.stderr!r}")

    def add_unrecorded_store_copy(self) -> None:
        """What `sbx skills add` leaves: a store entry marsh never imported."""
        stage = self.root / "add-home"
        skill(self.root / "added" / self.name, self.name, "ADDED COPY")
        (stage / ".agents/skills").mkdir(parents=True)
        (stage / ".agents/skills" / self.name).symlink_to(self.root / "added" / self.name)
        store = pathlib.Path(json.loads(subprocess.run([self.sbx, "skills", "ls", "--json"], capture_output=True,
                                                       text=True, check=True, timeout=60).stdout)["store"])
        relative = store.parent.relative_to(pathlib.Path.home())
        (stage / relative).parent.mkdir(parents=True, exist_ok=True)
        (stage / relative).symlink_to(store.parent)
        subprocess.run([self.sbx, "skills", "import", "--force", "--no-input"], env={**os.environ, "HOME": str(stage)},
                       capture_output=True, check=True, timeout=120)

    def cleanup(self) -> None:
        self.run("stop", timeout=300)
        if self.store():
            subprocess.run([self.sbx, "skills", "rm", "--force", "--no-input", self.name], capture_output=True,
                           timeout=60, check=False)


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--prefix", required=True, help="installed dev product (make dev DEV_PREFIX=...)")
    parser.add_argument("--kit", required=True, help="immutable fixture Kit OCI reference (make fixture-ref)")
    parser.add_argument("--sbx", default=shutil.which("sbx") or "sbx")
    args = parser.parse_args()
    root = pathlib.Path(tempfile.mkdtemp(prefix="marsh-skills-clash-"))
    scope = Scope(root, pathlib.Path(args.prefix).expanduser().resolve(), args.sbx, args.kit)
    try:
        # 1. An earlier import of the name, then the Kit that bundles it.
        scope.register({})
        scope.wait_mirror(lambda _: scope.store(), "the mirror never imported the folder's skill")
        require("HOST COPY" in scope.store()[scope.name], "the store holds another copy")
        scope.register({"clashkit": str(scope.kit_dir)})
        clash = f"{scope.name} (bundled by clashkit)"
        mirror = scope.wait_mirror(lambda m: clash in m.get("clashes", []) and not scope.store(),
                                   "the earlier import was not removed")
        scope.job_reads_kit_copy()
        mirror = scope.mirror()
        require(not mirror.get("errors"), f"unexpected errors: {mirror}")
        print(f"ok: earlier import removed, clash listed ({clash}), Kit VM created, its skills folder has the Kit's copy")

        # 2. A store copy marsh did not import: the VM create falls back.
        scope.register({"clashkit": str(scope.kit_dir)})
        scope.add_unrecorded_store_copy()
        scope.job_reads_kit_copy()
        require("ADDED COPY" in scope.store().get(scope.name, ""), "the unrecorded store copy was touched")
        note = f"clashkit runs without shared skills: name clash with {scope.name}"
        mirror = scope.mirror()
        require(note in mirror.get("errors", []), f"no fallback note: {mirror}")
        print(f"ok: store copy kept, Kit VM created with --skills off, status: {note!r}")
        return 0
    finally:
        scope.cleanup()
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
