"""Isolated daemons the harness launches itself, and the ownership proofs their cleanup relies on.

`launch_daemon` starts `marshd --home H --test-breaks-from-env` directly (H2 round 3: only a daemon started with that
flag honors `MARSH_TEST_BREAK`; a CLI-started daemon never does). `OwnedLeftovers` makes a scope's cleanup judge only
what it owns, and refuses to adopt a sandbox whose live UUID differs from the ownership map's.
"""

from __future__ import annotations

import json
import pathlib
import subprocess
import threading
import time
from typing import Any

from harness import Fail, IsolatedScope, daemon_endpoint_paths, scoped_control_home

TEST_BREAKS_FLAG = "--test-breaks-from-env"


def launch_daemon(scope: Any, marshd: pathlib.Path, modes: list[str], log: pathlib.Path, *,
                  flag: bool = True) -> subprocess.Popen[bytes]:
    """Start SCOPE's daemon explicitly: `marshd --home H [--test-breaks-from-env]` with MARSH_TEST_BREAK=MODES in its
    own environment (unset when MODES is empty). Every break, fault and hold run launches this way, even with no
    modes (pass 2 without prerequisites). FLAG=False launches without the flag, to prove the variable is then
    ignored. Raises Fail (after reaping the process) if the endpoint never appears."""
    environment = {**scope.environment}
    environment.pop("MARSH_TEST_BREAK", None)
    if modes:
        environment["MARSH_TEST_BREAK"] = ",".join(modes)
    argv = [str(marshd), "--home", str(scope.home), *([TEST_BREAKS_FLAG] if flag else [])]
    with log.open("wb") as output:
        process = subprocess.Popen(argv, cwd=scope.project, env=environment, stdin=subprocess.DEVNULL, stdout=output,
                                   stderr=output, process_group=0)
    scope.scope_started = True
    # Reap it the moment it exits: a zombie would look like a reused pid to the shutdown check.
    threading.Thread(target=process.wait, daemon=True).start()
    endpoint, _ = daemon_endpoint_paths(scope.home)
    deadline = time.monotonic() + 90
    while not endpoint.exists():
        if process.poll() is not None:
            scope.scope_started = False  # it is gone and answered nothing: there is no ownership to clean up by
            raise Fail(f"the explicitly launched daemon exited ({process.returncode}): "
                       f"{log.read_text(errors='replace')[-400:].strip()}")
        if time.monotonic() > deadline:
            reap(process)
            scope.scope_started = False
            raise Fail("the explicitly launched daemon did not publish its endpoint in 90 s")
        time.sleep(0.1)
    return process


def reap(process: subprocess.Popen[bytes] | None) -> bool:
    """Kill and wait for a process this harness spawned (it holds the Popen, so the pid cannot have been reused
    while it is unreaped). True when it is gone."""
    if process is None or process.poll() is not None:
        return True
    try:
        import os
        import signal
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    deadline = time.monotonic() + 15
    while process.poll() is None and time.monotonic() < deadline:
        time.sleep(0.05)
    return process.poll() is not None


def map_document(control_home: pathlib.Path) -> dict[str, Any] | None:
    try:
        return json.loads((pathlib.Path(control_home) / "vm-ownership.json").read_text(encoding="utf-8"))
    except FileNotFoundError:
        return {"vms": {}}
    except (OSError, ValueError):
        return None  # present but unreadable: ownership is unprovable


def map_uuid(control_home: pathlib.Path, name: str) -> str | None:
    document = map_document(control_home)
    entry = ((document or {}).get("vms") or {}).get(name)
    if isinstance(entry, dict):
        entry = entry.get("uuid")
    return entry if isinstance(entry, str) and entry else None


class OwnedLeftovers:
    """Cleanup judges only what this scope owns (baseline equality is the suite's oracle, not the base class's), and a
    sandbox is adopted as this scope's only if its live UUID equals the ownership map's."""

    def stock_leftover_errors(self, after: dict[str, str]) -> list[str]:
        owned = self.owned_stock_names()
        left = sorted(n for n in after if n in owned and n not in self.stock_before)
        return [f"owned stock VMs remain: {left}"] if left else []

    def remember_owned_status(self, status: dict[str, Any]) -> None:
        super().remember_owned_status(status)
        for name, live in dict(getattr(self, "owned_vm_identities", {})).items():
            deadline = time.monotonic() + 5
            while True:
                recorded = map_uuid(self.control_home, name)
                if recorded is not None or time.monotonic() > deadline:
                    break
                time.sleep(0.2)  # the map's entry gains its UUID just after `sbx create` returns
            if recorded != live:
                self.owned_vm_identities.pop(name, None)
                raise AssertionError(f"owned VM {name}: live UUID {live} differs from the ownership map's "
                                     f"{recorded}; refusing to adopt it")


class SideScope(OwnedLeftovers, IsolatedScope):
    """A second isolated daemon inside the run's root (HARNESS-02 compares installs)."""

    def __init__(self, parent: Any, name: str, prefix: pathlib.Path, kit: str | None = None) -> None:
        self.marsh = str(prefix / "bin" / "marsh")
        self.marshd = prefix / "bin" / "marshd"
        self.sbx = parent.sbx
        self.root = parent.root / name
        self.root.mkdir(mode=0o700)
        self.home = self.root / "home"
        self.home.mkdir(mode=0o700)
        self.control_root = self.root / "control"
        self.control_root.mkdir(mode=0o700)
        self.control_home = scoped_control_home(self.control_root, self.home)
        self.project = self.root / "project"
        self.project.mkdir()
        if kit:
            (self.control_home / "commands.json").write_text(json.dumps({"fixture": kit}) + "\n", encoding="utf-8")
        self.environment = {key: value for key, value in parent.environment.items() if key != "MARSH_TEST_BREAK"}
        self.environment.update({"MARSH_HOME": str(self.home), "MARSH_CONTROL_HOME": str(self.control_root),
                                 "MARSH_GUEST_ARTIFACTS": str(prefix / "libexec" / "marsh")})
        self.initialize_scope_cleanup()
        self.stock_before = parent.stock_before
        self.daemon: subprocess.Popen[bytes] | None = None

    def abort_unowned_daemon(self) -> None:
        """A daemon launched here whose status never authenticated has no ownership to clean up by: kill the process
        this harness spawned, and let cleanup proceed (it created no VM: a VM needs a request)."""
        if self.daemon is not None and self.owned_daemon_pid is None:
            reap(self.daemon)
            self.scope_started = False
