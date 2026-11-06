#!/usr/bin/env python3
"""Live check: upgrading marsh is replacing its files, nothing else.

A throwaway install (a copy of an installed product: version A) serves a
throwaway MARSH_HOME/control scope with the fixture Kit registered as
`fixture`.

1. A fixture job runs; meanwhile marsh's binaries are replaced (version B:
   `--next DIR` holding another build's marsh, marshd and marsh-mcp, else
   fresh copies of A's). The job keeps running on the same daemon, and
   `marsh status` says `update pending: restarts on B when idle`.
2. Once the job ends, the daemon restarts on B by itself (new pid, version
   B, no update pending) and keeps the scope's VMs.
3. Kit descriptors survive upgrades: with Docker refused access to the
   fixture's registry (a `MARSH_DOCKER` wrapper, as for a private image),
   `fixture` still starts, from the descriptor saved earlier, and from an
   older marsh's saved reading (`kit-agent-v4/`) once that copy is gone;
   `marsh status --json` says where it came from. A fresh home that never
   read it fails with `can't read IMAGE: registry access denied` and names
   `sbx secret set --registry`.
4. `marsh stop` with the daemon down starts it, removes the scope's VMs and
   exits 0; then `marsh stop` says there is nothing to stop.

Every VM this run creates is removed; pre-existing VMs are left alone.

    make dev DEV_PREFIX=~/dev/tmp-upgrade/prefix
    python3 tests/acceptance/upgrade_in_place.py --prefix ~/dev/tmp-upgrade/prefix \\
        --kit "$(cat target/fixture-ref)" [--next DIR] [--root ~/dev/tmp-upgrade/run]
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import shutil
import signal
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from harness import disposable_root, product_owned_vm_names  # noqa: E402
from launch_env import clean_environment  # noqa: E402

DENIED = ("ERROR: pull access denied, repository does not exist or may require authorization: "
          "server message: insufficient_scope: authorization failed")
DOCKER_WRAPPER = """\
#!{python}
import os, sys
args = sys.argv[1:]
if os.path.exists({deny!r}) and args[:3] == ["buildx", "imagetools", "inspect"]:
    sys.stderr.write({denied!r} + "\\n")
    sys.exit(1)
os.execv({real!r}, [{real!r}, *args])
"""
BINARIES = ("marsh", "marshd", "marsh-mcp")


def require(condition: object, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def vm_names(sbx: str) -> set[str]:
    listed = json.loads(subprocess.run([sbx, "ls", "--json"], capture_output=True, check=True, timeout=60).stdout)
    return {item["name"] for item in (listed["sandboxes"] if isinstance(listed, dict) else listed)}


def wait(what: str, check, timeout: float = 300, interval: float = 1):
    deadline = time.monotonic() + timeout
    while True:
        value = check()
        if value:
            return value
        require(time.monotonic() < deadline, f"timed out: {what}")
        time.sleep(interval)


class Scope:
    def __init__(self, root: pathlib.Path, name: str, prefix: pathlib.Path, environment: dict[str, str],
                 kit: str) -> None:
        self.home = root / name / "home"
        self.project = root / name / "project"
        control_root = root / name / "control"
        for directory in (self.home, self.project, control_root):
            directory.mkdir(parents=True, mode=0o700)
        self.control = control_root / hashlib.sha256(os.fsencode(self.home.resolve())).hexdigest()
        self.control.mkdir(mode=0o700)
        (self.control / "commands.json").write_text(json.dumps({"fixture": kit}) + "\n")
        self.marsh = str(prefix / "bin" / "marsh")
        self.environment = {**environment, "MARSH_HOME": str(self.home), "MARSH_CONTROL_HOME": str(control_root)}

    def run(self, *args: str, timeout: float = 900) -> subprocess.CompletedProcess[str]:
        return subprocess.run([self.marsh, *args], cwd=self.project, env=self.environment, text=True,
                              capture_output=True, timeout=timeout, check=False)

    def start(self, *args: str) -> subprocess.Popen[str]:
        return subprocess.Popen([self.marsh, *args], cwd=self.project, env=self.environment, text=True,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def status(self) -> dict:
        result = self.run("status", "--json", timeout=120)
        require(result.returncode == 0, f"marsh status: {result.stderr}")
        return json.loads(result.stdout)

    def text_status(self) -> str:
        return self.run("status", timeout=120).stdout

    def fixture(self, status: dict) -> dict:
        return next(command for command in status.get("commands", []) if command["name"] == "fixture")

    def owned(self) -> set[str]:
        return product_owned_vm_names(self.control)


def install(source: pathlib.Path, bin_directory: pathlib.Path) -> None:
    """Replace each binary the way an installer does: a new file renamed over the old."""
    for name in BINARIES:
        staged = bin_directory / f".{name}.new"
        shutil.copy2(source / name, staged)
        os.replace(staged, bin_directory / name)


def kill_daemon(pid: int) -> None:
    os.kill(pid, signal.SIGTERM)
    def gone() -> bool:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return True
        return False
    wait("the daemon to exit", gone, timeout=60, interval=0.2)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--prefix", required=True, type=pathlib.Path, help="installed product (version A)")
    parser.add_argument("--kit", required=True, help="immutable fixture Kit OCI reference (make fixture-ref)")
    parser.add_argument("--next", type=pathlib.Path, help="directory with version B's marsh, marshd, marsh-mcp")
    parser.add_argument("--root", type=pathlib.Path, help="parent of the throwaway state (default: a temporary one)")
    parser.add_argument("--sbx", default=shutil.which("sbx") or "sbx")
    args = parser.parse_args()
    sbx = str(pathlib.Path(shutil.which(args.sbx) or args.sbx).resolve())
    docker = shutil.which("docker")
    require(docker, "docker is not on PATH")
    if args.root:
        args.root.expanduser().mkdir(parents=True, exist_ok=True)
        root = pathlib.Path(tempfile.mkdtemp(prefix="upgrade-", dir=args.root.expanduser())).resolve()
    else:
        root = disposable_root("marsh-upgrade-")
    print(f"state: {root}")
    prefix = root / "prefix"
    source = args.prefix.expanduser().resolve(strict=True)
    for part in ("bin", "libexec"):
        shutil.copytree(source / part, prefix / part, symlinks=True)
    next_build = root / "next"
    next_build.mkdir()
    for name in BINARIES:
        shutil.copy2((args.next.expanduser() if args.next else prefix / "bin") / name, next_build / name)
    version_b = subprocess.run([str(next_build / "marshd"), "--version"], capture_output=True, text=True,
                               check=True).stdout.split()[1]
    deny = root / "deny-registry"
    fake = root / "fakebin"
    fake.mkdir()
    (fake / "docker").write_text(DOCKER_WRAPPER.format(python=sys.executable, deny=str(deny), denied=DENIED,
                                                       real=docker))
    (fake / "docker").chmod(0o755)
    environment = clean_environment()
    environment.update({
        "MARSH_SBX": sbx,
        "MARSH_PLACE": "local",
        "MARSH_KIT_IMAGE_CACHE": str(root / "kit-images"),
        "MARSH_DOCKER": str(fake / "docker"),
    })
    scope = Scope(root, "upgraded", prefix, environment, args.kit)
    # Its own empty image cache: no local archive of the Kit either.
    fresh = Scope(root, "fresh", prefix, {**environment, "MARSH_KIT_IMAGE_CACHE": str(root / "fresh-kit-images")},
                  args.kit)
    before = vm_names(sbx)
    failure: BaseException | None = None
    owned: set[str] = set()
    try:
        # 1. A job runs while the install is replaced.
        first = scope.status()
        version_a, pid_a = first["version"], first["endpoint_owner"]["pid"]
        warm = scope.run("-c", "fixture exec true", timeout=1800)
        require(warm.returncode == 0, f"first fixture job: {warm.stderr[-2000:]}")
        require(list((scope.control / "kit-descriptors").glob("*.json")), "no descriptor was saved")
        owned |= scope.owned()
        job = scope.start("-c", "fixture exec sh -c 'sleep 40; echo job-done'")
        wait("the job to run", lambda: scope.status()["processes"]["running"] >= 1, timeout=600)
        install(next_build, prefix / "bin")
        deny.touch()
        pending = wait("update pending", lambda: scope.status().get("update_pending"), timeout=120)
        require(pending["version"] == version_b, f"pending {pending}, expected {version_b}")
        require(f"update pending: restarts on {version_b} when idle" in scope.text_status(),
                f"text status: {scope.text_status()}")
        during = scope.status()
        require(during["endpoint_owner"]["pid"] == pid_a, "the daemon restarted under a running job")
        require(job.poll() is None, "the job ended early")
        print(f"ok: job runs on {version_a} (pid {pid_a}); status: update pending, restarts on {version_b}")

        # 2. The job ends; the daemon restarts on B and keeps the VMs.
        out, err = job.communicate(timeout=600)
        require(job.returncode == 0 and "job-done" in out, f"the job: {job.returncode} {out!r} {err[-2000:]!r}")
        after = wait("the restart on B", lambda: (lambda s: s if s["endpoint_owner"]["pid"] != pid_a
                                                  and not s.get("update_pending") else None)(scope.status()),
                     timeout=120)
        require(after["version"] == version_b, f"restarted on {after.get('version')}, expected {version_b}")
        require(owned <= vm_names(sbx), f"VMs lost in the restart: {sorted(owned - vm_names(sbx))}")
        require(owned <= scope.owned(), "VMs dropped from the ownership map")
        print(f"ok: restarted on {version_b} (pid {after['endpoint_owner']['pid']}); kept VMs {sorted(owned)}")

        # 3. Descriptors survive with the registry refused.
        again = scope.run("-c", "fixture exec echo after-upgrade", timeout=900)
        require(again.returncode == 0 and "after-upgrade" in again.stdout, f"fixture after: {again.stderr[-2000:]}")
        descriptor = scope.fixture(scope.status()).get("descriptor") or {}
        require(descriptor.get("source") == "fresh", f"descriptor: {descriptor}")
        sessions = scope.fixture(scope.status()).get("sessions")
        # What 0.3.10 and 0.3.11 kept: only their reading (kit-agent-v4/v5).
        digest = hashlib.sha256(args.kit.encode()).hexdigest()
        shutil.rmtree(scope.control / "kit-descriptors")
        shutil.rmtree(root / "kit-images", ignore_errors=True)
        legacy = scope.control / "kit-agent-v4"
        legacy.mkdir(mode=0o700)
        (legacy / f"{digest}.json").write_text(json.dumps({"marsh": "0.3.9", "agent": {"sessions": sessions}}))
        kill_daemon(after["endpoint_owner"]["pid"])
        cached = scope.run("-c", "fixture exec echo from-cache", timeout=900)
        require(cached.returncode == 0 and "from-cache" in cached.stdout, f"fixture cached: {cached.stderr[-2000:]}")
        descriptor = scope.fixture(scope.status()).get("descriptor") or {}
        require(descriptor.get("source") == "cached" and "insufficient_scope" in descriptor.get("error", ""),
                f"descriptor: {descriptor}")
        require("its Kit descriptor could not be read" in scope.text_status(), "no status warning")
        print(f"ok: registry refused; fixture starts from the saved descriptor, then from an older reading: {descriptor}")
        unread = fresh.run("-c", "fixture exec true", timeout=900)
        message = unread.stdout + unread.stderr
        require(unread.returncode != 0, "a never-read private Kit started")
        require(f"can't read {args.kit}: registry access denied" in message
                and "sbx secret set --registry docker.io" in message, f"fresh home: {message[-2000:]}")
        print("ok: a fresh home says: " + next(line for line in message.splitlines() if "can't read" in line)[:240])
        deny.unlink()
        owned |= scope.owned() | fresh.owned()

        # 4. `marsh stop` with the daemon down.
        kill_daemon(scope.status()["endpoint_owner"]["pid"])
        recorded = scope.owned()
        require(recorded, "nothing recorded to stop")
        stopped = scope.run("stop", timeout=600)
        require(stopped.returncode == 0, f"marsh stop (daemon down): {stopped.returncode} {stopped.stderr[-2000:]}")
        require(not (recorded & vm_names(sbx)), f"left: {sorted(recorded & vm_names(sbx))}")
        again = scope.run("stop", timeout=120)
        require(again.returncode == 0 and "nothing to stop" in again.stderr, f"second stop: {again.stderr}")
        print(f"ok: marsh stop with the daemon down removed {sorted(recorded)} and exited 0; then: {again.stderr.strip()}")
    except BaseException as error:  # noqa: BLE001
        failure = error
    finally:
        if deny.exists():
            deny.unlink()
        if "job" in locals() and job.poll() is None:
            job.kill()
            job.wait()
        problems = []
        for each in (scope, fresh):
            owned |= each.owned()
            result = each.run("stop", timeout=600)
            if result.returncode != 0:
                problems.append(f"marsh stop {each.home}: {result.stderr[-800:]}")
        now = vm_names(sbx)
        if owned & now:
            problems.append(f"this run's VMs remain: {sorted(owned & now)}")
        if before - now:
            # Never this run's doing (it removes only VMs its own maps record).
            print(f"note: other VMs went away meanwhile: {sorted(before - now)}")
        print("cleanup:", "verified" if not problems else problems)
        if failure is None and problems:
            failure = AssertionError("; ".join(problems))
    if failure is not None:
        print(f"FAILED: {failure!r}", file=sys.stderr)
        return 1
    print("ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
