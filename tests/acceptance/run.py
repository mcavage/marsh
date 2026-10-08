#!/usr/bin/env python3
"""Black-box acceptance harness for the assembled local direct-command product."""

from __future__ import annotations

import argparse
import base64
import datetime as dt
import fcntl
import hashlib
import json
import os
import pathlib
import pty
import re
import select
import shlex
import shutil
import signal
import socket
import stat
import struct
import subprocess
import sys
import tempfile
import time
import traceback
import platform
import pwd
from typing import Any, Callable

from provenance import (candidate_arguments, candidate_environment, host_only_path, source_identity, verify_candidate,
                        stock_vm_inventory, stock_cleanup_errors, remove_owned_stock_vm)


CHECKS = [
    "cold-parallel-prewarm",
    "prewarm-ready-and-subsecond",
    "shared-daemon-two-shells",
    "shell-sudo-apt-install",
    "shell-private-docker-without-sudo",
    "reused-vm-distinct-containers",
    "concurrent-containers-one-vm",
    "client-disconnect-preserves-worker",
    "capacity-overflow-visible",
    "natural-project-write",
    "descendant-registered-command",
    "persistent-and-ephemeral-home",
    "exact-streams-argv-status",
    "fanout-collect-composition",
    "fanout-collect-bounds",
    "durable-results-view",
    "background-pipeline-job-control",
    "pty-color-resize-interrupt",
    "blocked-stdin-interrupt",
    "resource-and-wall-enforcement",
    "verified-container-deletion",
    "uncertain-cleanup-quarantine",
    "stock-sbx-client-coexistence",
    "no-runtime-authority",
    "useful-phase-timing",
]
PHASES = {
    "vm_prepare",
    "admission",
    "mount_prepare",
    "worker_start",
    "execution",
    "output_drain",
    "result_capture",
    "cleanup",
}
REQUIRED_MILESTONES = {
    "request_received",
    "worker_progress",
    "process_exit",
    "output_drained",
    "completed",
}
OPTIONAL_MILESTONES = {"first_output"}
RESULT_SUMMARY_FIELDS = {
    "cursor", "job_id", "command", "placement", "cleanup", "state", "exit_code", "wall_ms",
    "parent",  # lineage.parent (docs/design/processes.md s9)
}
RESULT_RECEIPT_FIELDS = {
    "schema",
    "cursor",
    "job_id",
    "attempt_id",
    "session_id",
    "command",
    "state",
    "placement",
    "kit_profile",
    "image",
    "mounts",
    "worker_id",
    "vm_id",
    "container_id",
    "execution",
    "exit",
    "output_complete",
    "cleanup",
    "timing",
    "created_unix_ms",
    "finished_unix_ms",
    # docs/design/processes.md s9: every receipt carries {parent, root, depth, spawn}.
    "lineage",
}
# Display argv for `jobs` listings (lossy UTF-8, bounded); absent when empty.
RESULT_RECEIPT_OPTIONAL = {"args"}
IMAGE = re.compile(r"^[a-z0-9._:/][a-z0-9._:/-]*@sha256:[0-9a-f]{64}$")
CONTAINER = re.compile(r"^[0-9a-f]{64}$")
REVISION = re.compile(r"^[0-9a-f]{40}$")
JOB_LIMITS = {
    "cpu_millis": 500,
    "memory_bytes": 128 * 1024 * 1024,
    "pids": 32,
    "writable_bytes": 16 * 1024 * 1024,
    "output_bytes": 1024 * 1024,
    "wall_seconds": 10,
}
JOB_LIMIT_ENV = {
    "cpu_millis": "MARSH_JOB_CPU_MILLIS",
    "memory_bytes": "MARSH_JOB_MEMORY_BYTES",
    "pids": "MARSH_JOB_PIDS",
    "writable_bytes": "MARSH_JOB_WRITABLE_BYTES",
    "output_bytes": "MARSH_JOB_OUTPUT_BYTES",
    "wall_seconds": "MARSH_JOB_WALL_SECONDS",
}

# Stock SBX may still be running its initial guest apt refresh when the shell
# opens. Retry that transient lock while keeping the actual sudo/package check.
APT_UPDATE_READY = (
    '(attempt=0; until sudo -n apt-get update -qq; do '
    'attempt=$((attempt+1)); [ "$attempt" -lt 60 ] || exit 100; '
    'sleep 2; done)'
)


def resolve_executable(value: str) -> str:
    """Resolve a configured executable exactly once for probes and the product."""
    resolved = shutil.which(value)
    if resolved is None:
        raise ValueError(f"--sbx executable was not found or is not executable: {value}")
    return str(pathlib.Path(resolved).resolve())


def disposable_root(prefix: str) -> pathlib.Path:
    """Create mutable acceptance state outside the user's natural home tree."""
    parent = pathlib.Path("/private/tmp") if sys.platform == "darwin" else pathlib.Path(tempfile.gettempdir())
    parent = parent.resolve()
    if not parent.is_dir():
        raise ValueError(f"system temporary directory is unavailable: {parent}")
    root = pathlib.Path(tempfile.mkdtemp(prefix=prefix, dir=parent)).resolve()
    if root.is_relative_to(pathlib.Path.home().resolve()):
        shutil.rmtree(root)
        raise ValueError(f"disposable acceptance root unexpectedly falls under the user home: {root}")
    return root


def scoped_control_home(control_root: pathlib.Path, selected_home: pathlib.Path) -> pathlib.Path:
    """Mirror the product's per-selected-home directory under a private override root."""
    scope = hashlib.sha256(os.fsencode(selected_home.resolve(strict=True))).hexdigest()
    control_home = control_root.resolve(strict=True) / scope
    control_home.mkdir(mode=0o700)
    return control_home


def shell_vm_name(home: pathlib.Path) -> str:
    """Return the exact shell VM identity for one isolated selected home."""
    home = home.resolve()
    metadata = home.stat(follow_symlinks=False)
    digest = hashlib.sha256()
    digest.update(os.fsencode(home))
    digest.update(
        struct.pack(
            ">QQII", metadata.st_dev, metadata.st_ino, metadata.st_uid, metadata.st_gid
        )
    )
    return f"marsh-shell-{os.getuid()}-{digest.hexdigest()[:12]}"


def product_owned_vm_names(control_home: pathlib.Path | None) -> set[str]:
    """Names the isolated daemon recorded in its own VM ownership map.

    Stock VM names are random; the product's per-daemon ownership map, not a
    name pattern, says which VMs this scope created.
    """
    if control_home is None:
        return set()
    path = pathlib.Path(control_home) / "vm-ownership.json"
    try:
        document = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        return set()
    vms = document.get("vms") if isinstance(document, dict) else None
    if not isinstance(vms, dict):
        raise AssertionError(f"malformed product VM ownership map: {path}")
    return {name for name in vms if isinstance(name, str)}


def stable_process_identity(pid: int) -> tuple[int, str] | None:
    """Return UID plus an OS process birth/executable identity for one PID."""
    if sys.platform.startswith("linux"):
        process = pathlib.Path("/proc") / str(pid)
        try:
            stat = (process / "stat").read_text(encoding="utf-8")
            closing = stat.rfind(")")
            fields = stat[closing + 2 :].split()
            start_ticks = fields[19]
            executable = os.readlink(process / "exe")
            uid = process.stat().st_uid
        except (FileNotFoundError, ProcessLookupError):
            return None
        except (IndexError, OSError, ValueError) as error:
            raise RuntimeError(f"could not identify process {pid}: {error}") from error
        return uid, f"linux-start={start_ticks};exe={executable}"

    completed = subprocess.run(
        ["/bin/ps", "-p", str(pid), "-o", "uid=,lstart=,comm="],
        env={**os.environ, "LC_ALL": "C"},
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=5,
        check=False,
    )
    if completed.returncode != 0 or not completed.stdout.strip():
        return None
    try:
        uid_text, token = completed.stdout.decode("utf-8").strip().split(maxsplit=1)
        return int(uid_text), token
    except (UnicodeDecodeError, ValueError) as error:
        raise RuntimeError(f"could not identify process {pid}") from error


def daemon_endpoint_paths(home: pathlib.Path) -> tuple[pathlib.Path, pathlib.Path]:
    canonical = home.resolve(strict=True)
    scope = hashlib.sha256(os.fsencode(canonical)).hexdigest()[:16]
    runtime = pathlib.Path("/tmp") / f"marsh-{os.getuid()}" / scope
    return runtime / "s", runtime / "t"


def authenticated_daemon_request(
    home: pathlib.Path, token: str | None, body: dict[str, Any]
) -> tuple[dict[str, Any], str]:
    """Issue one owner-authenticated request through the selected home's endpoint."""
    endpoint, token_path = daemon_endpoint_paths(home)
    for path, expected in ((endpoint, "socket"), (token_path, "file")):
        metadata = path.lstat()
        valid_type = (
            stat.S_ISSOCK(metadata.st_mode)
            if expected == "socket"
            else stat.S_ISREG(metadata.st_mode)
        )
        if (
            stat.S_ISLNK(metadata.st_mode)
            or not valid_type
            or metadata.st_uid != os.getuid()
            or metadata.st_mode & 0o077
        ):
            raise RuntimeError(f"unsafe daemon {expected}: {path}")
    if token is None:
        try:
            token = token_path.read_text(encoding="ascii")
        except UnicodeError as error:
            raise RuntimeError("daemon token is not ASCII") from error
    if len(token) != 64 or not all(character in "0123456789abcdefABCDEF" for character in token):
        raise RuntimeError("daemon token is malformed")
    envelope = json.dumps(
        {"protocol": "marsh.daemon/v1", "token": token, "body": body},
        separators=(",", ":"),
    ).encode("utf-8")
    if len(envelope) > 1024 * 1024:
        raise RuntimeError("daemon request frame is too large")
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(5)
        connection.connect(os.fspath(endpoint))
        connection.sendall(struct.pack(">I", len(envelope)) + envelope)
        header = _receive_exact(connection, 4)
        length = struct.unpack(">I", header)[0]
        if length > 1024 * 1024:
            raise RuntimeError("daemon reply frame is too large")
        try:
            reply = json.loads(_receive_exact(connection, length))
        except (UnicodeError, json.JSONDecodeError) as error:
            raise RuntimeError("daemon returned malformed JSON") from error
    if not isinstance(reply, dict):
        raise RuntimeError("daemon returned a malformed reply")
    return reply, token


def _receive_exact(connection: socket.socket, length: int) -> bytes:
    chunks: list[bytes] = []
    remaining = length
    while remaining:
        chunk = connection.recv(remaining)
        if not chunk:
            raise RuntimeError("daemon closed an incomplete reply")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def authenticate_daemon_control(home: pathlib.Path, daemon_id: str, pid: int) -> str:
    reply, token = authenticated_daemon_request(home, None, {"type": "ping"})
    if (
        reply.get("type") != "pong"
        or reply.get("daemon_id") != daemon_id
        or reply.get("pid") != pid
    ):
        raise RuntimeError("daemon control endpoint does not match authenticated status")
    return token


def request_authenticated_daemon_shutdown(home: pathlib.Path, token: str) -> None:
    reply, _ = authenticated_daemon_request(home, token, {"type": "shutdown"})
    if reply.get("type") != "shutting_down":
        message = reply.get("message", "unexpected daemon shutdown reply")
        raise RuntimeError(str(message))


class IsolatedScopeCleanup:
    """Track and remove only resources authenticated by one isolated daemon."""

    def initialize_scope_cleanup(self) -> None:
        self.scope_started = False
        self.owned_scope_id: str | None = None
        self.owned_daemon_id: str | None = None
        self.owned_daemon_pid: int | None = None
        self.owned_daemon_process_identity: tuple[int, str] | None = None
        self.owned_daemon_control_token: str | None = None
        self.owned_vms: set[str] = set()

    def remember_owned_status(self, status: dict[str, Any]) -> None:
        owner = status.get("endpoint_owner", {})
        scope_id = status.get("scope_id")
        daemon_id = status.get("daemon_id")
        pid = owner.get("pid")
        if (
            status.get("schema") != "marsh.status/v1"
            or not isinstance(scope_id, str)
            or not scope_id
            or not isinstance(daemon_id, str)
            or not daemon_id
            or owner.get("uid") != os.getuid()
            or not isinstance(pid, int)
            or pid <= 1
            or pid == os.getpid()
        ):
            raise AssertionError("isolated status lacks valid same-user ownership")
        process_identity = stable_process_identity(pid)
        if process_identity is None or process_identity[0] != os.getuid():
            raise AssertionError("isolated daemon process identity is unavailable or changed")
        control_token = authenticate_daemon_control(self.home, daemon_id, pid)
        if (
            self.owned_scope_id not in (None, scope_id)
            or self.owned_daemon_id not in (None, daemon_id)
            or self.owned_daemon_pid not in (None, pid)
            or self.owned_daemon_process_identity not in (None, process_identity)
            or self.owned_daemon_control_token not in (None, control_token)
        ):
            raise AssertionError("isolated daemon ownership changed during acceptance")
        self.owned_scope_id = scope_id
        self.owned_daemon_id = daemon_id
        self.owned_daemon_pid = pid
        self.owned_daemon_process_identity = process_identity
        self.owned_daemon_control_token = control_token
        for worker in status.get("workers", []):
            if worker.get("scope_id") != scope_id:
                continue
            vm_id = worker.get("vm_id")
            if not isinstance(vm_id, str) or not vm_id:
                raise AssertionError("owned worker lacks its VM identity")
            self.owned_vms.add(vm_id)
        # Product ownership discovers resources; only stock stable IDs authorize
        # subsequent removal. Never turn a same-name replacement into our VM.
        inventory = stock_vm_inventory(self.sbx)
        baseline = getattr(self, "stock_before", None)
        if not isinstance(baseline, dict):
            raise AssertionError("stable stock baseline must precede product effects")
        identities = getattr(self, "owned_vm_identities", {})
        for reference in {*self.owned_vms, *product_owned_vm_names(getattr(self, "control_home", None))}:
            matches = [(name, identifier) for name, identifier in inventory.items()
                       if reference in (name, identifier)]
            for name, identifier in matches:
                if name in baseline or identifier in baseline.values():
                    raise AssertionError("product attributed a pre-existing stock VM to the test scope")
                if name in identities and identities[name] != identifier:
                    raise AssertionError("owned stock VM was replaced during acceptance")
                identities[name] = identifier
        self.owned_vm_identities = identities

    def refresh_owned_status_before_cleanup(self) -> str | None:
        """Re-read authenticated status so post-status VMs gain stable IDs."""
        marsh = getattr(self, "marsh", None)
        if marsh is None:
            return None
        try:
            completed = subprocess.run(
                [marsh, "status", "--json"],
                cwd=getattr(self, "project", self.root),
                env=self.environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=15,
                check=False,
            )
            if completed.returncode != 0:
                return f"could not refresh owned status before cleanup: exit {completed.returncode}"
            status = json.loads(completed.stdout)
        except (OSError, subprocess.SubprocessError, UnicodeDecodeError, json.JSONDecodeError) as error:
            return f"could not refresh owned status before cleanup: {error}"
        if not isinstance(status, dict):
            return "could not refresh owned status before cleanup: malformed status"
        IsolatedScopeCleanup.remember_owned_status(self, status)
        return None

    def cleanup_isolated_scope(self) -> list[str]:
        errors: list[str] = []
        if self.scope_started and (
            self.owned_scope_id is None or self.owned_daemon_pid is None
        ):
            errors.append("isolated daemon started without authenticated status ownership")
            return errors
        if self.owned_daemon_pid is not None:
            try:
                current = stable_process_identity(self.owned_daemon_pid)
            except (OSError, RuntimeError, subprocess.SubprocessError) as error:
                errors.append(
                    f"could not revalidate isolated daemon {self.owned_daemon_pid}: {error}"
                )
                return errors
            if current is not None and current != self.owned_daemon_process_identity:
                errors.append(
                    f"isolated daemon PID {self.owned_daemon_pid} was reused; refusing cleanup"
                )
                return errors
            if current is not None:
                # A failed first product effect (e.g. a cold Create) can leave
                # VMs whose stable IDs no prior status observed. Learn them
                # through the same authenticated status path while the owned
                # daemon still lives; changed ownership aborts cleanup.
                try:
                    refresh_error = IsolatedScopeCleanup.refresh_owned_status_before_cleanup(self)
                except AssertionError as error:
                    errors.append(f"owned status changed before cleanup: {error}")
                    return errors
                if refresh_error is not None:
                    errors.append(refresh_error)
                try:
                    if self.owned_daemon_control_token is None:
                        raise RuntimeError("daemon control authentication is unavailable")
                    request_authenticated_daemon_shutdown(
                        self.home, self.owned_daemon_control_token
                    )
                except (OSError, RuntimeError, socket.timeout) as error:
                    errors.append(
                        f"could not request authenticated daemon shutdown: {error}"
                    )
                    return errors
            if current is not None:
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    try:
                        current = stable_process_identity(self.owned_daemon_pid)
                    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
                        errors.append(
                            f"could not verify isolated daemon termination: {error}"
                        )
                        return errors
                    if current is None:
                        break
                    if current != self.owned_daemon_process_identity:
                        errors.append(
                            f"isolated daemon PID {self.owned_daemon_pid} was reused during shutdown"
                        )
                        return errors
                    time.sleep(0.05)
                else:
                    errors.append(
                        f"isolated daemon {self.owned_daemon_pid} did not exit after shutdown"
                    )
                    return errors
        vm_ids = set()
        if self.owned_scope_id is not None:
            try:
                inventory = stock_vm_inventory(self.sbx)
                identities = getattr(self, "owned_vm_identities", {})
                for reference in {*self.owned_vms, *product_owned_vm_names(getattr(self, "control_home", None))}:
                    for name, identifier in inventory.items():
                        if reference not in (name, identifier):
                            continue
                        if identities.get(name) != identifier:
                            raise ValueError(f"owned VM identity unavailable/replaced: {name}; refusing removal")
                        vm_ids.add(identifier)
                # Include learned identities even when product worker references
                # are names that disappeared; stable IDs authorize cleanup.
                vm_ids.update(identities.values())
            except (OSError, ValueError, subprocess.SubprocessError) as error:
                return [f"could not revalidate owned stock VM identities: {error}"]
        for vm_id in sorted(vm_ids):
            try:
                names = [name for name, identifier in identities.items() if identifier == vm_id]
                if len(names) != 1:
                    raise ValueError("owned stable ID requires one retained stock name")
                remove_owned_stock_vm(self.sbx, names[0], vm_id, self.stock_before,
                                      env=self.environment, cwd=self.root)
            except (OSError, ValueError, subprocess.SubprocessError) as error:
                errors.append(f"could not remove owned VM {vm_id}: {error}")
                continue
        if self.owned_scope_id is not None:
            try:
                after = stock_vm_inventory(self.sbx)
                if vm_ids.intersection(after.values()):
                    errors.append("owned stock VM IDs remain after removal")
                errors.extend(stock_cleanup_errors(self.stock_before, after))
            except (OSError, ValueError, subprocess.SubprocessError) as error:
                errors.append(f"stock cleanup inventory unavailable: {error}")
        if not errors:
            try:
                shutil.rmtree(self.root)
            except OSError as error:
                errors.append(f"could not remove isolated root {self.root}: {error}")
        return errors


def resolve_kit_reference(value: str) -> tuple[str, str]:
    """Return the commands.json value and stable product identity for a Kit."""
    if IMAGE.fullmatch(value):
        return value, value
    source = pathlib.Path(value).expanduser().resolve()
    if not source.is_dir():
        raise ValueError("--kit must be a native v3 source directory or immutable OCI digest")
    return str(source), f"local-v3:{source}"


def kit_profile_matches(profile: object, reference: str) -> bool:
    if profile == reference:
        return True
    return (
        isinstance(profile, str)
        and reference.startswith("local-v3:")
        and re.fullmatch(rf"{re.escape(reference)}@sha256:[0-9a-f]{{64}}", profile)
        is not None
    )


def now() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat().replace("+00:00", "Z")


def validate_schema(value: Any, schema: dict[str, Any], path: str = "$") -> None:
    """Validate the dependency-free JSON-Schema subset used by result.schema.json."""
    expected = schema.get("type")
    if expected is not None:
        names = expected if isinstance(expected, list) else [expected]
        checks = {
            "object": lambda item: isinstance(item, dict),
            "array": lambda item: isinstance(item, list),
            "string": lambda item: isinstance(item, str),
            "integer": lambda item: isinstance(item, int) and not isinstance(item, bool),
            "boolean": lambda item: isinstance(item, bool),
            "null": lambda item: item is None,
        }
        if not any(name in checks and checks[name](value) for name in names):
            raise ValueError(f"{path}: expected {' or '.join(names)}")
    if "const" in schema and value != schema["const"]:
        raise ValueError(f"{path}: expected constant {schema['const']!r}")
    if "enum" in schema and value not in schema["enum"]:
        raise ValueError(f"{path}: value is outside the declared enum")
    if isinstance(value, str):
        if "pattern" in schema and re.search(schema["pattern"], value) is None:
            raise ValueError(f"{path}: string does not match {schema['pattern']!r}")
        if schema.get("format") == "date-time":
            try:
                dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
            except ValueError as error:
                raise ValueError(f"{path}: invalid date-time") from error
    if isinstance(value, int) and not isinstance(value, bool):
        if "minimum" in schema and value < schema["minimum"]:
            raise ValueError(f"{path}: value is below minimum {schema['minimum']}")
    if isinstance(value, dict):
        required = schema.get("required", [])
        missing = [name for name in required if name not in value]
        if missing:
            raise ValueError(f"{path}: missing required properties {missing!r}")
        properties = schema.get("properties", {})
        if schema.get("additionalProperties") is False:
            extras = sorted(set(value) - set(properties))
            if extras:
                raise ValueError(f"{path}: unexpected properties {extras!r}")
        for name, child in value.items():
            if name in properties:
                validate_schema(child, properties[name], f"{path}.{name}")
    if isinstance(value, list) and "items" in schema:
        for index, child in enumerate(value):
            validate_schema(child, schema["items"], f"{path}[{index}]")


def validate_concurrent_fanout_timing(report: dict[str, Any]) -> None:
    branches = report.get("branches", [])
    durations = [branch.get("duration_ms") for branch in branches]
    total_ms = report.get("total_ms")
    if (
        [branch.get("label") for branch in branches] != ["first", "second"]
        or not all(isinstance(value, int) and value >= 800 for value in durations)
        or not isinstance(total_ms, int)
        or total_ms < max(durations)
        or total_ms >= sum(durations) - 400
        or total_ms > max(durations) + 1_000
    ):
        raise AssertionError(f"fanout timing did not describe concurrent elapsed work: {report!r}")


def validate_structural_results(document: dict[str, Any], receipt: dict[str, Any]) -> None:
    jobs = document.get("jobs", [])
    cursors = [job.get("cursor") for job in jobs]
    job_ids = [job.get("job_id") for job in jobs]
    if (
        document.get("schema") != "marsh.jobs/v1"
        or not jobs
        or not all(type(cursor) is int and cursor > 0 for cursor in cursors)
        or any(newer <= older for newer, older in zip(cursors, cursors[1:]))
        or not all(isinstance(job_id, str) and job_id for job_id in job_ids)
        or len(set(job_ids)) != len(job_ids)
        or any(set(job) != RESULT_SUMMARY_FIELDS for job in jobs)
    ):
        raise AssertionError(f"durable results are not newest-first structural summaries: {jobs!r}")
    if (
        receipt.get("schema") != "marsh.job/v1"
        or type(receipt.get("cursor")) is not int
        or receipt.get("cursor") != jobs[0].get("cursor")
        or receipt.get("job_id") != jobs[0].get("job_id")
        or receipt.get("placement") != "local"
        or not RESULT_RECEIPT_FIELDS <= set(receipt) <= RESULT_RECEIPT_FIELDS | RESULT_RECEIPT_OPTIONAL
    ):
        raise AssertionError(
            f"durable result lookup is inconsistent or exposes non-structural content: {receipt!r}"
        )


STOCK_EXEC_NOTICE = re.compile(
    rb"Sandbox [a-z0-9][a-z0-9-]* started successfully"
    rb"|WARN: docker hub refresh lock held by another process: [ -~]+"
)


def verify_container_deleted(
    run: Callable[[list[str]], subprocess.CompletedProcess[bytes]],
    exec_prefix: list[str],
    container_id: str,
) -> None:
    """Stock exec failure alone cannot prove that a container was deleted."""
    inspected = run([*exec_prefix, "docker", "inspect", container_id])
    if inspected.returncode == 0:
        raise AssertionError(f"verified-deleted container {container_id} still exists")
    # Docker CLI versions differ only in capitalization of these messages.
    absent = {
        f"error: no such object: {container_id}".encode(),
        f"error response from daemon: no such container: {container_id}".encode(),
    }
    # Stock `sbx exec` may print its own notices first (restarting a stopped VM,
    # a Docker Hub refresh lock held by a concurrent sbx process). Only those
    # recognized notices may precede Docker's absent message.
    lines = inspected.stderr.strip().splitlines()
    notices_ok = all(STOCK_EXEC_NOTICE.fullmatch(line.strip()) for line in lines[:-1])
    if (inspected.returncode != 1 or not lines or not notices_ok
            or lines[-1].strip().lower() not in absent
            or inspected.stdout.strip() not in {b"", b"[]"}):
        raise AssertionError(
            f"container deletion is unverified for {container_id}: "
            f"inspect returned {inspected.returncode}, stderr={inspected.stderr!r}"
        )
    # Probe the same VM and authority independently after the absent response.
    # A dead Engine, denied stock exec, or empty successful reply is uncertainty.
    health = run([*exec_prefix, "docker", "info", "--format", "{{.ID}}"])
    if health.returncode != 0 or not re.fullmatch(rb"[A-Za-z0-9][A-Za-z0-9:-]*", health.stdout.strip()):
        raise AssertionError(
            f"container deletion is unverified for {container_id}: "
            f"Engine/stock transport health probe failed ({health.returncode}), "
            f"stdout={health.stdout!r}, stderr={health.stderr!r}"
        )


def failed_check_summary(checks: list[dict[str, Any]]) -> str | None:
    failed = [check for check in checks if check.get("outcome") == "failed"]
    if not failed:
        return None
    first = failed[0]
    detail = str(first.get("detail") or "no failure detail was recorded")
    remaining = len(failed) - 1
    not_run = sum(check.get("outcome") == "not-run" for check in checks)
    suffix: list[str] = []
    if remaining:
        suffix.append(f"{remaining} additional failed")
    if not_run:
        suffix.append(f"{not_run} not run")
    counts = f" ({', '.join(suffix)})" if suffix else ""
    return f"check {first.get('name', '<unknown>')} failed: {detail}{counts}"


def unexpected_failure(error: BaseException) -> str:
    qualified = f"{type(error).__module__}.{type(error).__qualname__}"
    detail = str(error)
    heading = f"acceptance failed unexpectedly: {qualified}"
    if detail:
        heading += f": {detail}"
    trace = "".join(traceback.format_exception(type(error), error, error.__traceback__)).strip()
    return f"{heading}\n{trace}"


class Harness(IsolatedScopeCleanup):
    def __init__(self, args: argparse.Namespace) -> None:
        self.marsh = str(pathlib.Path(args.marsh).resolve())
        guest_artifacts, build_receipt = verify_candidate(args, self.marsh)
        runtime_environment = candidate_environment(args, build_receipt)
        guest_names = ("marsh-linux-arm64", "marsh-worker-linux-arm64", "marsh-relay-linux-arm64")
        if not all((guest_artifacts / name).is_file() for name in guest_names):
            raise ValueError(f"guest artifacts missing at {guest_artifacts}; set MARSH_GUEST_ARTIFACTS")
        guest_hashes = {name: self.file_digest(guest_artifacts / name) for name in guest_names}
        self.sbx = resolve_executable(args.sbx)
        self.evidence = pathlib.Path(args.evidence).resolve()
        destination = host_only_path(self.evidence / "result.json", pathlib.Path(args.source_tree))
        destination.unlink(missing_ok=True)
        self.evidence.mkdir(parents=True, mode=0o700, exist_ok=True)
        self.fixture_mapping, self.fixture_ref = resolve_kit_reference(args.kit)
        identity = source_identity(pathlib.Path(args.source_tree), args.source_revision)
        self.root = disposable_root("marsh-acceptance-")
        try:
            self.project = self.root / "natural-project"
            self.home = self.root / "home"
            self.control_root = self.root / "control"
            self.project.mkdir()
            self.home.mkdir(mode=0o700)
            self.guest_home = self.home / "home"
            self.guest_home.mkdir(mode=0o700)
            self.control_root.mkdir(mode=0o700)
            self.control_home = scoped_control_home(self.control_root, self.home)
        except Exception:
            shutil.rmtree(self.root, ignore_errors=True)
            raise
        self.environment = os.environ.copy()
        self.environment.pop("MARSH_LOCAL_SHELL_AUTHORITY", None)
        self.environment.update(runtime_environment)
        self.environment["MARSH_HOME"] = str(self.home)
        self.environment["MARSH_CONTROL_HOME"] = str(self.control_root)
        self.environment["MARSH_GUEST_ARTIFACTS"] = str(guest_artifacts)
        self.environment["MARSH_SBX"] = self.sbx
        for name, variable in JOB_LIMIT_ENV.items():
            self.environment[variable] = str(JOB_LIMITS[name])
        self.initialize_scope_cleanup()
        self.active_containers_verified: set[str] = set()
        self.deleted_containers_verified: set[str] = set()
        self.receipt_cache: dict[str, dict[str, Any]] = {}
        self.worker_blocked: str | None = None
        self.gate_sequence = 0
        self.stock_before = None
        self.result: dict[str, Any] = {
            "schema": "marsh.acceptance-result/v1",
            "started_at": now(),
            "finished_at": now(),
            "outcome": "failed",
            "environment": {
                "host_platform": platform.platform(),
                **identity,
                "marsh_binary": self.marsh,
                "marsh_binary_sha256": self.file_digest(pathlib.Path(self.marsh)),
                "guest_artifact_sha256": guest_hashes,
                "sbx_binary": self.sbx,
                "sbx_version": None,
                "fixture_kit_ref": self.fixture_ref,
                "command_mapping_sha256": None,
                "project": str(self.project),
                "marsh_home": str(self.home),
                "job_limits": dict(JOB_LIMITS),
            },
            "checks": [{"name": name, "outcome": "not-run"} for name in CHECKS],
            "commands": [],
            "snapshots": [{"label": "verified-build-receipt", "source": "host",
                           "value": build_receipt}] if build_receipt is not None else [],
        }

    @staticmethod
    def file_digest(path: pathlib.Path) -> str:
        digest = hashlib.sha256()
        with path.open("rb") as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(chunk)
        return f"sha256:{digest.hexdigest()}"

    def finish(self, failure: str | None = None) -> int:
        gates = [snapshot["value"] for snapshot in self.result["snapshots"]
                 if snapshot["label"] == "fixture-gate-startup"]
        slow_starts = sum(gate["slow_start_warning"] for gate in gates)
        timing = next((check for check in self.result["checks"]
                       if check["name"] == "useful-phase-timing"), None)
        if timing is not None:
            summary = f"fixture starts: {len(gates)}; over 15s: {slow_starts}"
            timing["detail"] = f"{timing['detail']}; {summary}" if timing.get("detail") else summary
        not_run = [item for item in self.result["checks"] if item["outcome"] == "not-run"]
        check_failure = failed_check_summary(self.result["checks"])
        if check_failure:
            self.result["failure"] = (
                f"{check_failure}; {failure}" if failure else check_failure
            )
        elif failure:
            self.result["failure"] = failure
        elif not_run:
            self.result["failure"] = f"{len(not_run)} not run"
        else:
            self.result["outcome"] = "passed"
        cleanup_errors = self.cleanup_isolated_scope()
        if getattr(self, "stock_before", None) is not None:
            try:
                after = stock_vm_inventory(self.sbx)
                self.result["snapshots"].append({"label": "stock-sbx-after-cleanup", "source": "sbx", "value": after})
                cleanup_errors.extend(stock_cleanup_errors(self.stock_before, after))
            except Exception as error:
                cleanup_errors.append(f"independent stock cleanup unavailable: {error}")
        if cleanup_errors:
            self.result["outcome"] = "failed"
            cleanup = "; ".join(cleanup_errors)
            previous = self.result.get("failure")
            self.result["failure"] = f"{previous}; cleanup: {cleanup}" if previous else f"cleanup: {cleanup}"
        self.result["finished_at"] = now()
        destination = self.evidence / "result.json"
        destination.write_text(json.dumps(self.result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        destination.chmod(0o600)
        schema_path = pathlib.Path(__file__).with_name("result.schema.json")
        try:
            validate_schema(
                json.loads(destination.read_text(encoding="utf-8")),
                json.loads(schema_path.read_text(encoding="utf-8")),
            )
        except (OSError, json.JSONDecodeError, ValueError) as error:
            self.result["outcome"] = "failed"
            self.result["failure"] = f"evidence schema validation failed: {error}"
            destination.write_text(
                json.dumps(self.result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
            )
        print(f"acceptance: {self.result['outcome']}; evidence: {destination}", file=sys.stderr)
        print(f"acceptance: fixture starts: {len(gates)}; over 15s: {slow_starts}", file=sys.stderr)
        if self.result.get("failure"):
            print(f"acceptance: {self.result['failure']}", file=sys.stderr)
        return 0 if self.result["outcome"] == "passed" else 1

    def record_command(
        self, argv: list[str], cwd: pathlib.Path, stdin: bytes, completed: subprocess.CompletedProcess[bytes], elapsed: float
    ) -> None:
        self.result["commands"].append(
            {
                "argv": argv,
                "cwd": str(cwd),
                "stdin_base64": base64.b64encode(stdin).decode(),
                "stdout_base64": base64.b64encode(completed.stdout).decode(),
                "stderr_base64": base64.b64encode(completed.stderr).decode(),
                "status": completed.returncode,
                "elapsed_ms": round(elapsed * 1000),
            }
        )

    def run(self, argv: list[str], *, stdin: bytes = b"", cwd: pathlib.Path | None = None, timeout: float = 30) -> subprocess.CompletedProcess[bytes]:
        directory = cwd or self.project
        if argv and argv[0] == self.marsh:
            self.scope_started = True
        started = time.monotonic()
        completed = subprocess.run(
            argv,
            input=stdin,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            cwd=directory,
            env=self.environment,
            timeout=timeout,
            check=False,
        )
        self.record_command(argv, directory, stdin, completed, time.monotonic() - started)
        return completed

    def shell(self, command: str, *, stdin: bytes = b"", ephemeral: bool = False, load: str | None = None, timeout: float = 30) -> subprocess.CompletedProcess[bytes]:
        argv = [self.marsh]
        if ephemeral:
            argv.append("--ephemeral-home")
        if load is not None:
            argv.extend(["--load", load])
        argv.extend(["-c", command])
        return self.run(argv, stdin=stdin, timeout=timeout)

    def gated_shell(
        self, fixture_arguments: list[str], *, stdin: bytes = b"", ephemeral: bool = False, timeout: float = 30
    ) -> subprocess.CompletedProcess[bytes]:
        gate = self.start_gated(fixture_arguments, ephemeral=ephemeral)
        self.state_snapshot(f"active-gate-{gate['token']}")
        return self.finish_gated(gate, stdin=stdin, timeout=timeout)

    def start_gated(self, fixture_arguments: list[str], *, ephemeral: bool = False) -> dict[str, Any]:
        gate = self.spawn_gated(fixture_arguments, ephemeral=ephemeral)
        # An ephemeral HOME gets its own shell and Kit VMs: a cold start that
        # includes pulling a published Kit image. The 15s signal still records it.
        self.wait_gated(gate, timeout=240 if ephemeral else 30)
        return gate

    def spawn_gated(self, fixture_arguments: list[str], *, ephemeral: bool = False) -> dict[str, Any]:
        self.gate_sequence += 1
        token = f"{self.gate_sequence:04d}"
        ready = self.project / f".marsh-ready-{token}"
        release = self.project / f".marsh-go-{token}"
        command = shlex.join(["fixture", "gate", token, *fixture_arguments])
        argv = [self.marsh]
        if ephemeral:
            argv.append("--ephemeral-home")
        argv.extend(["-c", command])
        started = time.monotonic()
        process = subprocess.Popen(
            argv,
            cwd=self.project,
            env=self.environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        return {
            "argv": argv,
            "process": process,
            "ready": ready,
            "release": release,
            "started": started,
            "token": token,
        }

    def wait_gated(self, gate: dict[str, Any], timeout: float = 30) -> None:
        # Preserve the 15s slow-start signal while allowing eventual execution.
        process = gate["process"]
        ready = gate["ready"]
        deadline = time.monotonic() + timeout
        while not ready.exists() and process.poll() is None and time.monotonic() < deadline:
            time.sleep(0.02)
        if not ready.exists():
            try:
                stdout, stderr = process.communicate(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                stdout, stderr = process.communicate(timeout=5)
            completed = subprocess.CompletedProcess(gate["argv"], process.returncode, stdout, stderr)
            self.record_command(gate["argv"], self.project, b"", completed, time.monotonic() - gate["started"])
            raise AssertionError(f"fixture did not reach its observable container gate: {stderr.decode(errors='replace')}")
        ready_ms = round((time.monotonic() - gate["started"]) * 1000)
        slow = ready_ms > 15_000
        self.result["snapshots"].append({
            "label": "fixture-gate-startup",
            "source": "host",
            "value": {"token": gate["token"], "ready_ms": ready_ms, "slow_start_warning": slow},
        })
        if slow:
            print(f"warning: fixture gate {gate['token']} reached readiness after {ready_ms} ms", file=sys.stderr)

    def finish_gated(
        self, gate: dict[str, Any], *, stdin: bytes = b"", timeout: float = 30
    ) -> subprocess.CompletedProcess[bytes]:
        gate["release"].write_bytes(b"go")
        process = gate["process"]
        try:
            stdout, stderr = process.communicate(input=stdin, timeout=timeout)
        except subprocess.TimeoutExpired:
            process.kill()
            stdout, stderr = process.communicate(timeout=5)
            completed = subprocess.CompletedProcess(
                gate["argv"], process.returncode, stdout, stderr
            )
            self.record_command(
                gate["argv"], self.project, stdin, completed,
                time.monotonic() - gate["started"],
            )
            raise
        completed = subprocess.CompletedProcess(gate["argv"], process.returncode, stdout, stderr)
        self.record_command(gate["argv"], self.project, stdin, completed, time.monotonic() - gate["started"])
        return completed

    def state_snapshot(self, label: str) -> dict[str, Any]:
        status_command = self.run([self.marsh, "status", "--json"], timeout=10)
        if status_command.returncode != 0:
            raise AssertionError(f"public status failed: {status_command.stderr.decode(errors='replace').strip()}")
        value = json.loads(status_command.stdout)
        if value.get("schema") != "marsh.status/v1":
            raise AssertionError("status returned an unsupported schema")
        self.remember_owned_status(value)
        self.result["snapshots"].append({"label": f"{label}-status", "source": "marsh", "value": value})
        scope_id = value.get("scope_id")
        for worker in value.get("workers", []):
            if worker.get("scope_id") == scope_id and kit_profile_matches(
                worker.get("kit_profile"), self.fixture_ref
            ):
                vm_id = worker.get("vm_id")
                if not isinstance(vm_id, str) or not vm_id:
                    raise AssertionError("acceptance worker is missing its VM identity")
                self.owned_vms.add(vm_id)
        jobs_command = self.run([self.marsh, "jobs", "--json"], timeout=10)
        if jobs_command.returncode != 0:
            raise AssertionError(f"public jobs list failed: {jobs_command.stderr.decode(errors='replace').strip()}")
        jobs = json.loads(jobs_command.stdout)
        if jobs.get("schema") != "marsh.jobs/v1" or not isinstance(jobs.get("jobs"), list):
            raise AssertionError("jobs returned an unsupported schema")
        self.result["snapshots"].append({"label": f"{label}-jobs", "source": "marsh", "value": jobs})
        receipts = []
        for summary in jobs["jobs"]:
            job_id = summary.get("job_id")
            if not isinstance(job_id, str):
                raise AssertionError("jobs returned a missing job identity")
            receipt = self.receipt_cache.get(job_id)
            if receipt is None or receipt.get("state") == "running":
                shown = self.run([self.marsh, "jobs", "show", job_id, "--json"], timeout=10)
                if shown.returncode != 0:
                    raise AssertionError(f"public job receipt failed for {job_id}")
                receipt = json.loads(shown.stdout)
                if receipt.get("schema") != "marsh.job/v1" or receipt.get("job_id") != job_id:
                    raise AssertionError(f"job receipt schema or identity mismatch for {job_id}")
                self.receipt_cache[job_id] = receipt
            receipts.append(receipt)
            self.result["snapshots"].append(
                {"label": f"{label}-job-{job_id}", "source": "marsh", "value": receipt}
            )
        combined = dict(value)
        combined["runs"] = receipts
        self.verify_container_evidence(combined, verify_deleted=not any(
            run.get("state") == "running" for run in receipts
        ))
        return combined

    def verify_container_evidence(self, state: dict[str, Any], *, verify_deleted: bool = True) -> None:
        scope = state.get("scope_id")
        workers = {worker.get("worker_id"): worker for worker in state.get("workers", [])}
        for run in self.runs(state):
            container_id = run.get("container_id")
            if not container_id:
                continue
            if not CONTAINER.fullmatch(container_id):
                raise AssertionError(f"runtime returned a non-container identity: {container_id!r}")
            worker = workers.get(run.get("worker_id"))
            if not worker and run.get("state") != "running" and run.get("cleanup") == "verified":
                # A retired worker (workers reset, or an ephemeral-HOME Kit at
                # session end) leaves the status view; its VM, and every
                # container in it, must be gone from stock.
                if run.get("vm_id") in stock_vm_inventory(self.sbx):
                    raise AssertionError(f"retired worker VM {run.get('vm_id')} still exists")
                self.deleted_containers_verified.add(container_id)
                continue
            if not worker or worker.get("vm_id") != run.get("vm_id"):
                raise AssertionError("run worker/VM identity is inconsistent")
            if worker.get("scope_id") != scope or not kit_profile_matches(
                worker.get("kit_profile"), self.fixture_ref
            ):
                raise AssertionError("fixture VM is outside the isolated acceptance scope")
            vm_id = run.get("vm_id")
            self.owned_vms.add(vm_id)
            if run.get("state") == "running" and container_id not in self.active_containers_verified:
                inspected = self.run(
                    [self.sbx, "exec", vm_id, "docker", "inspect", container_id],
                    cwd=self.root,
                    timeout=15,
                )
                if inspected.returncode != 0:
                    raise AssertionError(f"active container {container_id} is not real in VM {vm_id}")
                self.active_containers_verified.add(container_id)
            if verify_deleted and run.get("cleanup") == "verified" and container_id not in self.deleted_containers_verified:
                verify_container_deleted(
                    lambda argv: self.run(argv, cwd=self.root, timeout=15),
                    [self.sbx, "exec", vm_id],
                    container_id,
                )
                self.deleted_containers_verified.add(container_id)

    def mark(
        self,
        name: str,
        function: Callable[[], None],
        *,
        requires_worker: bool = True,
    ) -> None:
        item = next(
            (check for check in self.result["checks"] if check["name"] == name), None
        )
        if item is None:
            raise RuntimeError(f"acceptance check {name!r} is not declared")
        if requires_worker and self.worker_blocked is not None:
            item["detail"] = self.worker_blocked
            return
        try:
            function()
        except Exception as error:  # evidence must survive every failed assertion
            item["outcome"] = "failed"
            item["detail"] = str(error)
            blocker = self.fixture_worker_blocker()
            if blocker is not None:
                self.worker_blocked = blocker
        else:
            item["outcome"] = "passed"

    def fixture_worker_blocker(self) -> str | None:
        """Describe a poisoned shared fixture worker without masking the first failure."""
        try:
            status = self.run([self.marsh, "status", "--json"], timeout=10)
            if status.returncode != 0:
                return None
            document = json.loads(status.stdout)
        except (OSError, subprocess.SubprocessError, json.JSONDecodeError):
            return None
        blocked = [
            worker
            for worker in document.get("workers", [])
            if kit_profile_matches(worker.get("kit_profile"), self.fixture_ref)
            and worker.get("health") == "quarantined"
        ]
        if not blocked:
            return None
        identities = ", ".join(
            str(worker.get("worker_id") or worker.get("vm_id") or "unknown")
            for worker in blocked
        )
        return f"not run after the shared fixture worker was quarantined: {identities}"

    def prepare(self) -> str | None:
        self.stock_before = stock_vm_inventory(self.sbx)
        self.result["snapshots"].append({"label": "stock-sbx-before", "source": "sbx", "value": self.stock_before})
        # The first public command may start marshd, and the daemon freezes its
        # command registry at startup. Install the isolated UAT mapping before
        # that first connection so the test never depends on a stale resident
        # daemon or an unrelated packaged provider registry.
        mapping_path = self.control_home / "commands.json"
        mapping_path.write_text(
            json.dumps({"fixture": self.fixture_mapping}, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        self.result["environment"]["command_mapping_sha256"] = self.file_digest(mapping_path)

        status = self.run([self.marsh, "status", "--json"], timeout=10)
        try:
            document = json.loads(status.stdout)
        except json.JSONDecodeError:
            document = None
        if (
            status.returncode == 0
            and isinstance(document, dict)
            and document.get("schema") == "marsh.status/v1"
        ):
            self.remember_owned_status(document)
        if (
            status.returncode != 0
            or not isinstance(document, dict)
            or document.get("schema") != "marsh.status/v1"
            or "direct-command-acceptance-v1" not in document.get("features", [])
        ):
            detail = status.stderr.decode(errors="replace").strip()
            return (
                "assembled direct-command product unavailable: expected public "
                f"'marsh status --json' handshake; {detail or 'no compatible response'}"
            )
        self.result["snapshots"].append({"label": "initial-status", "source": "marsh", "value": document})

        version = self.run([self.sbx, "version"], cwd=self.root, timeout=20)
        if version.returncode != 0:
            version = self.run([self.sbx, "--version"], cwd=self.root, timeout=20)
        if version.returncode != 0:
            return f"stock sbx version is unavailable: {version.stderr.decode(errors='replace').strip()}"
        self.result["environment"]["sbx_version"] = version.stdout.decode(errors="replace").strip()
        return None

    @staticmethod
    def runs(state: dict[str, Any], command: str = "fixture") -> list[dict[str, Any]]:
        return [run for run in state.get("runs", []) if run.get("command") == command]

    def newest_run(self, state: dict[str, Any]) -> dict[str, Any]:
        runs = self.runs(state)
        if not runs:
            raise AssertionError("no fixture run in public state")
        return runs[0]

    def receipts(self) -> list[dict[str, Any]]:
        """Return fixture receipts in the public newest-first order."""
        listed = self.run([self.marsh, "jobs", "--json"], timeout=10)
        if listed.returncode != 0:
            raise AssertionError(
                f"public jobs list failed: {listed.stderr.decode(errors='replace').strip()}"
            )
        document = json.loads(listed.stdout)
        if document.get("schema") != "marsh.jobs/v1":
            raise AssertionError("jobs returned an unsupported schema")
        receipts: list[dict[str, Any]] = []
        for summary in document.get("jobs", []):
            if summary.get("command") != "fixture":
                continue
            job_id = summary.get("job_id")
            if not isinstance(job_id, str):
                raise AssertionError("jobs returned a missing job identity")
            receipt = self.receipt_cache.get(job_id)
            if receipt is None or receipt.get("state") == "running":
                shown = self.run(
                    [self.marsh, "jobs", "show", job_id, "--json"], timeout=10
                )
                if shown.returncode != 0:
                    raise AssertionError(f"public job receipt failed for {job_id}")
                receipt = json.loads(shown.stdout)
                if receipt.get("schema") != "marsh.job/v1" or receipt.get("job_id") != job_id:
                    raise AssertionError(f"job receipt schema or identity mismatch for {job_id}")
                self.receipt_cache[job_id] = receipt
            receipts.append(receipt)
        return receipts

    def run_all(self) -> None:
        sequential: list[dict[str, Any]] = []

        def cold_parallel() -> None:
            argv = [self.marsh, "--load", "fixture", "-c", "true"]
            started = time.monotonic()
            processes = [
                subprocess.Popen(
                    argv,
                    cwd=self.project,
                    env=self.environment,
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                )
                for _ in range(2)
            ]
            for process in processes:
                stdout, stderr = process.communicate(timeout=120)
                completed = subprocess.CompletedProcess(argv, process.returncode, stdout, stderr)
                self.record_command(argv, self.project, b"", completed, time.monotonic() - started)
                if process.returncode != 0:
                    raise AssertionError(
                        f"parallel cold prewarm failed: {stderr.decode(errors='replace')}"
                    )
            state = self.state_snapshot("after-parallel-cold-prewarm")
            workers = [
                worker
                for worker in state.get("workers", [])
                if kit_profile_matches(worker.get("kit_profile"), self.fixture_ref)
                and worker.get("health") == "ready"
            ]
            if len(workers) != 1:
                raise AssertionError(
                    f"parallel cold calls did not converge on one ready worker: {workers!r}"
                )

        self.mark("cold-parallel-prewarm", cold_parallel)

        def prewarm() -> None:
            for selection in ("fixture", "all"):
                completed = self.run(
                    [self.marsh, "--load", selection, "-c", "true"],
                    cwd=self.project,
                    timeout=300,
                )
                if completed.returncode != 0:
                    raise AssertionError(f"--load {selection} failed: {completed.stderr!r}")
            state = self.state_snapshot("after-prewarm")
            ready_workers = [
                worker
                for worker in state.get("workers", [])
                if kit_profile_matches(worker.get("kit_profile"), self.fixture_ref)
                and worker.get("health") == "ready"
                and worker.get("warm") is True
            ]
            if not ready_workers:
                raise AssertionError("prewarm returned before a fixture worker and image were ready")

            # Measure the path a user exercises from an already-open project
            # shell. Starting three new macOS clients here would primarily
            # measure three stock-SBX project-shell attachments, rather than
            # warm registered-command container readiness.
            gates = []
            for _ in range(3):
                self.gate_sequence += 1
                token = f"{self.gate_sequence:04d}"
                gates.append(
                    {
                        "token": token,
                        "ready": self.project / f".marsh-ready-{token}",
                        "release": self.project / f".marsh-go-{token}",
                    }
                )
            shell_ready = self.project / f".marsh-shell-ready-{gates[0]['token']}"
            shell_go = self.project / f".marsh-shell-go-{gates[0]['token']}"
            branches = []
            waits = []
            for index, gate in enumerate(gates, start=1):
                branches.append(
                    f"fixture gate {shlex.quote(gate['token'])} identity & p{index}=$!"
                )
                waits.append(f'wait "$p{index}"; s{index}=$?')
            command = "; ".join(
                [
                    f"touch {shlex.quote(str(shell_ready))}",
                    f"while test ! -e {shlex.quote(str(shell_go))}; do sleep 0.01; done",
                    *branches,
                    *waits,
                    "test \"$s1\" -eq 0 && test \"$s2\" -eq 0 && test \"$s3\" -eq 0",
                ]
            )
            argv = [self.marsh, "--load", "fixture", "-c", command]
            process_started = time.monotonic()
            process = subprocess.Popen(
                argv,
                cwd=self.project,
                env=self.environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            completed = None
            failure = None
            try:
                deadline = time.monotonic() + 15
                while not shell_ready.exists() and process.poll() is None and time.monotonic() < deadline:
                    time.sleep(0.01)
                if not shell_ready.exists():
                    raise AssertionError("project shell did not reach the warm-command start gate")

                started = time.monotonic()
                shell_go.write_bytes(b"go")
                deadline = started + 5
                while (
                    not all(gate["ready"].exists() for gate in gates)
                    and process.poll() is None
                    and time.monotonic() < deadline
                ):
                    time.sleep(0.005)
                if not all(gate["ready"].exists() for gate in gates):
                    raise AssertionError("three cached jobs did not reach their container gates")
                ready_ms = round((time.monotonic() - started) * 1000)
                self.result["snapshots"].append({
                    "label": "prewarm-three-ready",
                    "source": "host",
                    "value": {"ready_ms": ready_ms},
                })
                if ready_ms >= 1000:
                    raise AssertionError(f"three cached jobs became ready in {ready_ms} ms")
                active = self.state_snapshot("three-prewarmed-jobs-active")
                fixture_workers = [
                    worker
                    for worker in active.get("workers", [])
                    if kit_profile_matches(worker.get("kit_profile"), self.fixture_ref)
                ]
                container_ids = [
                    container_id
                    for worker in fixture_workers
                    for container_id in worker.get("active_container_ids", [])
                ]
                if len(container_ids) != 3 or len(set(container_ids)) != 3 or not all(
                    isinstance(container_id, str) and CONTAINER.fullmatch(container_id)
                    for container_id in container_ids
                ):
                    raise AssertionError(
                        f"warm commands did not create three distinct real containers: {container_ids!r}"
                    )
            except Exception as error:
                failure = error
            finally:
                shell_go.touch()
                for gate in gates:
                    gate["release"].write_bytes(b"go")
                try:
                    stdout, stderr = process.communicate(timeout=15)
                except subprocess.TimeoutExpired:
                    process.kill()
                    stdout, stderr = process.communicate(timeout=5)
                    if failure is None:
                        failure = AssertionError("warm registered commands did not exit after release")
                completed = subprocess.CompletedProcess(argv, process.returncode, stdout, stderr)
                self.record_command(
                    argv,
                    self.project,
                    b"",
                    completed,
                    time.monotonic() - process_started,
                )
            if failure is not None:
                raise failure
            assert completed is not None
            if completed.returncode != 0:
                raise AssertionError(
                    "warm registered commands failed: "
                    + completed.stderr.decode(errors="replace")
                )
            if b"[starting " in completed.stderr and b" worker VM" in completed.stderr:
                raise AssertionError("warm invocation emitted a false cold-boot message")

        self.mark("prewarm-ready-and-subsecond", prewarm)

        def shared_daemon() -> None:
            gates = [self.start_gated(["hold", "1"]) for _ in range(2)]
            try:
                state = self.state_snapshot("two-shells-active")
                shells = state.get("shells", [])
                daemon_ids = {shell.get("daemon_id") for shell in shells if shell.get("state") == "attached"}
                if len(daemon_ids) != 1 or len(shells) < 2:
                    raise AssertionError("two overlapping shells did not attach to one daemon")
            finally:
                for gate in gates:
                    gate["release"].write_bytes(b"go")
                for gate in gates:
                    self.finish_gated(gate, timeout=10)

        self.mark("shared-daemon-two-shells", shared_daemon)

        def shell_sudo_apt() -> None:
            completed = self.shell(
                f"sudo -n true && {APT_UPDATE_READY} && "
                "sudo -n env DEBIAN_FRONTEND=noninteractive "
                "apt-get install -y -qq --no-install-recommends ed >/dev/null && "
                "test -x /usr/bin/ed && "
                "test \"$(stat -c %U:%G:%a /etc/sudoers.d/marsh)\" = root:root:440 && "
                "! grep -Rqs 'http://deb.debian.org/' "
                "/etc/apt/sources.list /etc/apt/sources.list.d",
                timeout=180,
            )
            if completed.returncode != 0:
                raise AssertionError(
                    "project shell could not install a Debian package through confined sudo: "
                    f"{completed.stderr.decode(errors='replace')}"
                )

        self.mark("shell-sudo-apt-install", shell_sudo_apt)

        def shell_private_docker() -> None:
            host = subprocess.run(
                ["docker", "info", "--format", "{{.ID}}"],
                capture_output=True, timeout=15, check=True,
            ).stdout.strip()
            guest = self.shell(
                "test \"$(id -u)\" != 0 && "
                "id -nG | tr ' ' '\\n' | grep -qx docker && "
                "test \"$(stat -c %G /var/run/docker.sock)\" = docker && "
                "docker info --format '{{.ID}}'",
                timeout=30,
            )
            if guest.returncode != 0 or not guest.stdout.strip() or guest.stdout.strip() == host:
                raise AssertionError(
                    "shell user must access only its private Docker Engine without sudo: "
                    f"{guest.stderr.decode(errors='replace')}"
                )

        self.mark("shell-private-docker-without-sudo", shell_private_docker)

        def reuse() -> None:
            for _ in range(2):
                completed = self.gated_shell(["identity"])
                if completed.returncode != 0:
                    raise AssertionError("identity fixture failed")
                sequential.append(self.newest_run(self.state_snapshot("sequential-run")))
            if sequential[0].get("vm_id") != sequential[1].get("vm_id"):
                raise AssertionError("sequential runs did not reuse one VM")
            containers = [run.get("container_id", "") for run in sequential]
            if len(set(containers)) != 2 or not all(CONTAINER.fullmatch(value) for value in containers):
                raise AssertionError("sequential runs lack distinct real container IDs")

        self.mark("reused-vm-distinct-containers", reuse)

        def concurrent() -> None:
            gates = [self.start_gated(["hold", "1"]) for _ in range(2)]
            try:
                state = self.state_snapshot("concurrent-containers")
                active = [run for run in self.runs(state) if run.get("state") == "running"]
                if len(active) < 2 or len({run.get("vm_id") for run in active[:2]}) != 1:
                    raise AssertionError("two containers were not concurrent in one warm VM")
                if len({run.get("container_id") for run in active[:2]}) != 2:
                    raise AssertionError("concurrent runs did not have distinct containers")
            finally:
                for gate in gates:
                    gate["release"].write_bytes(b"go")
                for gate in gates:
                    self.finish_gated(gate, timeout=10)

        self.mark("concurrent-containers-one-vm", concurrent)

        def client_disconnect() -> None:
            gate = self.start_gated(["hold", "300"])
            before = self.state_snapshot("client-disconnect-active")
            active = [run for run in self.runs(before) if run.get("state") == "running"]
            if not active:
                raise AssertionError("disconnect probe did not reach a running container")
            interrupted = active[0]
            process = gate["process"]
            process.kill()
            stdout, stderr = process.communicate(timeout=5)
            self.record_command(
                gate["argv"], self.project, b"",
                subprocess.CompletedProcess(gate["argv"], process.returncode, stdout, stderr),
                time.monotonic() - gate["started"],
            )

            deadline = time.monotonic() + 15
            terminal = None
            while time.monotonic() < deadline:
                shown = self.run(
                    [self.marsh, "jobs", "show", interrupted["job_id"], "--json"],
                    timeout=10,
                )
                terminal = json.loads(shown.stdout)
                if terminal.get("state") != "running":
                    break
                time.sleep(0.1)
            if (
                terminal is None
                # A disconnected caller cancels the job (docs/design/processes.md s8);
                # the cause records the cancelled delivery.
                or terminal.get("state") != "cancelled"
                or "cancel" not in terminal.get("exit", {}).get("cause", "")
                or terminal.get("cleanup") != "verified"
            ):
                raise AssertionError(
                    f"client disconnect did not produce verified cancellation: {terminal!r}"
                )

            completed = self.gated_shell(["identity"])
            if completed.returncode != 0:
                raise AssertionError("worker was not reusable after client disconnect")
            after = self.newest_run(self.state_snapshot("client-disconnect-reused"))
            if (
                after.get("vm_id") != interrupted.get("vm_id")
                or after.get("worker_id") != interrupted.get("worker_id")
            ):
                raise AssertionError("client disconnect replaced a healthy worker")

        self.mark("client-disconnect-preserves-worker", client_disconnect)

        def capacity_overflow() -> None:
            state = self.state_snapshot("capacity-baseline")
            workers = [
                worker
                for worker in state.get("workers", [])
                if kit_profile_matches(worker.get("kit_profile"), self.fixture_ref)
                and worker.get("health") == "ready"
            ]
            if not workers:
                raise AssertionError("no ready fixture worker advertised capacity")
            worker = workers[-1]
            capacity = worker.get("container_capacity")
            if not isinstance(capacity, int) or not 1 <= capacity <= 16:
                raise AssertionError(f"worker advertised invalid bounded capacity: {capacity!r}")
            gates: list[dict[str, Any]] = []
            overflow: dict[str, Any] | None = None
            try:
                gates = [self.spawn_gated(["hold", "1"]) for _ in range(capacity)]
                for gate in gates:
                    self.wait_gated(gate, timeout=JOB_LIMITS["wall_seconds"])
                saturated = self.state_snapshot("capacity-saturated")
                on_worker = [
                    run
                    for run in self.runs(saturated)
                    if run.get("state") == "running" and run.get("worker_id") == worker.get("worker_id")
                ]
                if len(on_worker) != capacity:
                    raise AssertionError("worker did not admit exactly its declared container capacity")
                overflow = self.spawn_gated(["hold", "1"])
                stdout, stderr = overflow["process"].communicate(timeout=15)
                completed = subprocess.CompletedProcess(
                    overflow["argv"], overflow["process"].returncode, stdout, stderr
                )
                self.record_command(
                    overflow["argv"], self.project, b"", completed,
                    time.monotonic() - overflow["started"],
                )
                if completed.returncode == 0 or b"capacity" not in stderr.lower():
                    raise AssertionError(
                        f"overflow invocation lacked actionable capacity rejection: {stderr!r}"
                    )
                observed = self.state_snapshot("capacity-overflow-rejected")
                on_worker = [
                    run for run in self.runs(observed)
                    if run.get("state") == "running"
                    and run.get("worker_id") == worker.get("worker_id")
                ]
                if len(on_worker) > capacity:
                    raise AssertionError("worker exceeded its declared container capacity")
                # docs/design/processes.md s6 (GM 4): a pool refusal happens at
                # admission, before any record or container: no new receipt.
                admitted = {run.get("job_id") for run in self.runs(saturated)}
                if self.newest_run(observed).get("job_id") not in admitted:
                    raise AssertionError("capacity rejection left a receipt (it must precede the record)")
            finally:
                if overflow is not None and overflow["process"].poll() is None:
                    overflow["process"].kill()
                    overflow["process"].communicate(timeout=5)
                for gate in gates:
                    gate["release"].write_bytes(b"go")
                for gate in gates:
                    if gate["process"].poll() is None:
                        try:
                            self.finish_gated(gate, timeout=10)
                        except (OSError, subprocess.SubprocessError):
                            gate["process"].kill()
                            gate["process"].communicate(timeout=5)

        self.mark("capacity-overflow-visible", capacity_overflow)

        def natural_write() -> None:
            target = self.project / "direct-write.txt"
            completed = self.shell("fixture project-write direct-write.txt natural")
            if (
                completed.returncode != 0
                or json.loads(completed.stdout) != {"fsync": True, "closed": True}
                or target.read_text(encoding="utf-8") != "natural"
            ):
                raise AssertionError("direct project write was not visible at the natural host path")
            completed = self.shell("fixture project-write direct-write.txt updated")
            if completed.returncode != 0 or target.read_text(encoding="utf-8") != "updated":
                raise AssertionError("direct project update was not visible at the natural host path")
            outside = self.root / "outside-project.txt"
            escaped = self.shell(f"fixture project-write {shlex.quote(str(outside))} escaped")
            if escaped.returncode == 0 or outside.exists():
                raise AssertionError("job wrote outside the exact project/home grants")
            identity = json.loads(self.shell("fixture identity").stdout)
            if identity.get("cwd") != str(self.project):
                raise AssertionError(f"job cwd was {identity.get('cwd')}, expected {self.project}")

        self.mark("natural-project-write", natural_write)

        def descendant_registered_command() -> None:
            before = {receipt["job_id"] for receipt in self.receipts()}
            completed = self.shell("sh -c 'fixture identity'")
            if completed.returncode != 0:
                raise AssertionError(f"descendant fixture invocation failed: {completed.stderr!r}")
            identity = json.loads(completed.stdout)
            if identity.get("cwd") != str(self.project):
                raise AssertionError("descendant fixture did not retain the project path")
            new = [receipt for receipt in self.receipts() if receipt["job_id"] not in before]
            if len(new) != 1 or new[0].get("command") != "fixture" or not CONTAINER.fullmatch(new[0].get("container_id") or ""):
                raise AssertionError("descendant fixture did not create one real container")

        self.mark("descendant-registered-command", descendant_registered_command)

        def homes() -> None:
            identity = json.loads(self.gated_shell(["identity"]).stdout)
            username = pwd.getpwuid(os.getuid()).pw_name
            if identity.get("user") != username or identity.get("home") != f"/Users/{username}":
                raise AssertionError(f"selected home identity is not natural: {identity}")
            if self.gated_shell(["home-write", "persistent.txt", "kept"]).returncode != 0:
                raise AssertionError("persistent home write failed")
            if self.gated_shell(["home-read", "persistent.txt"]).stdout != b"kept":
                raise AssertionError("persistent home did not survive a fresh container")
            if (self.guest_home / "persistent.txt").read_bytes() != b"kept":
                raise AssertionError("guest home write did not reach MARSH_HOME/home")
            if (self.home / "persistent.txt").exists():
                raise AssertionError("guest home write escaped into host control scope")
            if self.gated_shell(["home-write", "ephemeral.txt", "leaked"], ephemeral=True).returncode != 0:
                raise AssertionError("ephemeral home write failed")
            if self.gated_shell(["home-read", "ephemeral.txt"]).returncode == 0:
                raise AssertionError("ephemeral home content leaked into persistent home")

        self.mark("persistent-and-ephemeral-home", homes)

        def streams() -> None:
            for payload in (b"", b"stdin\x00bytes", "snowman: \u2603\n".encode(), b"invalid:\xff"):
                completed = self.gated_shell(
                    ["streams", "", "two words", '"quoted"', "$HOME", "*"], stdin=payload
                )
                expected = b'OUT\x00["","two words","\\"quoted\\"","$HOME","*"]\n' + payload
                if completed.stdout != expected or completed.stderr != b"ERR\x00fixture\n" or completed.returncode != 23:
                    raise AssertionError("argv/stdio/status were not byte exact")

        self.mark("exact-streams-argv-status", streams)

        def composition() -> None:
            completed = self.shell(
                "printf input | fanout { "
                "first: fixture streams first | tail -c 5 | tr a-z A-Z; "
                "second: fixture streams second | tail -c 5 | sed s/input/branch/ "
                "} | collect --timing",
                load="fixture",
            )
            first = completed.stdout.find(b"== first (complete) ==\nINPUT")
            second = completed.stdout.find(b"== second (complete) ==\nbranch")
            total = re.search(rb"(?m)^  total\s+([0-9]+) ms$", completed.stdout)
            if (
                completed.returncode != 0
                or first < 0
                or second <= first
                or total is None
                or int(total.group(1)) >= 1_000
            ):
                raise AssertionError(
                    "registered-command fanout pipelines were not collected in declaration order "
                    f"within one second: {completed.stdout!r}"
                )

            concurrent = self.shell(
                "fanout { first: fixture hold 1; second: fixture hold 1 } | collect --json",
                timeout=20,
            )
            report = json.loads(concurrent.stdout)
            if concurrent.returncode != 0:
                raise AssertionError(f"concurrent registered fanout failed: {concurrent.stderr!r}")
            validate_concurrent_fanout_timing(report)

            failed = self.shell(
                "set -o pipefail; "
                "fanout { ok: fixture identity; bad: fixture streams failed } | collect | cat >&2; "
                "set -- \"$?\" \"${PIPESTATUS[*]}\"; printf '%s' \"$2\"; exit \"$1\""
            )
            # fanout.md: the failed branch's stderr is rendered in collect's
            # output right after its header (the rendering goes to stderr here).
            if (
                failed.returncode != 23
                or failed.stdout != b"23 23 0"
                or b"== bad (failed: 23) ==\n" not in failed.stderr
                or b"== bad stderr ==\nERR\x00fixture" not in failed.stderr
                or failed.stderr.index(b"== bad (failed: 23) ==") > failed.stderr.index(b"== bad stderr ==")
            ):
                raise AssertionError(
                    "failed registered fanout branch did not preserve diagnostics and pipefail status: "
                    f"stdout={failed.stdout!r} stderr={failed.stderr!r} status={failed.returncode}"
                )

        self.mark("fanout-collect-composition", composition)

        def composition_bounds() -> None:
            before = len(self.receipts())
            oversized_input = self.shell(
                "head -c 67108865 /dev/zero | fanout { branch: fixture identity } | collect",
                timeout=30,
            )
            if (
                oversized_input.returncode == 0
                or oversized_input.stdout
                or b"input exceeds 64 MiB" not in oversized_input.stderr
                or len(self.receipts()) != before
            ):
                raise AssertionError(
                    "oversized fanout input did not fail before launching a registered branch"
                )

            oversized_output = self.shell(
                "fanout { noisy: sh -c 'head -c 17825792 /dev/zero; sleep 300' } | collect",
                timeout=30,
            )
            if (
                oversized_output.returncode == 0
                or oversized_output.stdout
                or b"combined output exceeds 16 MiB" not in oversized_output.stderr
            ):
                raise AssertionError(
                    "oversized fanout output did not cancel without rendering a partial collection"
                )

        self.mark("fanout-collect-bounds", composition_bounds)

        def durable_results() -> None:
            listed = self.run([self.marsh, "results", "--json"], timeout=10)
            document = json.loads(listed.stdout)
            jobs = document.get("jobs", [])
            if listed.returncode != 0 or document.get("schema") != "marsh.jobs/v1" or not jobs:
                raise AssertionError(f"durable result list is incomplete: {document!r}")
            cursor = jobs[0].get("cursor")
            shown = self.run(
                [self.marsh, "results", "show", str(cursor), "--json"], timeout=10
            )
            receipt = json.loads(shown.stdout)
            if shown.returncode != 0:
                raise AssertionError(f"durable result lookup failed: {shown.stderr!r}")
            validate_structural_results(document, receipt)
            jobs = self.run([self.marsh, "jobs", "--json"], timeout=10)
            job = self.run(
                [self.marsh, "jobs", "show", str(cursor), "--json"], timeout=10
            )
            if (
                jobs.returncode != 0
                or job.returncode != 0
                or json.loads(jobs.stdout) != document
                or json.loads(job.stdout) != receipt
            ):
                raise AssertionError("jobs and results disagree on structural receipts")
            for command in ([self.marsh, "jobs"], [self.marsh, "jobs", "show", str(cursor)]):
                readable = self.run(command, timeout=10)
                if readable.returncode != 0 or b"fixture" not in readable.stdout:
                    raise AssertionError(f"human job view failed: {readable!r}")
            for name in ("jobs", "results"):
                missing = self.run([self.marsh, name, "show", "--json"], timeout=10)
                if missing.returncode == 0 or f"usage: marsh {name}".encode() not in missing.stderr:
                    raise AssertionError(f"{name} show accepted --json without a selector")
            # A session lists its own jobs by default; these ran in others.
            guest = self.shell("marsh jobs --all", timeout=30)
            if guest.returncode != 0 or b"fixture" not in guest.stdout:
                raise AssertionError(f"guest job view failed: {guest!r}")

        self.mark("durable-results-view", durable_results, requires_worker=False)

        def background_pipeline() -> None:
            self.gate_sequence += 1
            token = f"{self.gate_sequence:04d}"
            ready = self.project / f".marsh-ready-{token}"
            release = self.project / f".marsh-go-{token}"
            command = (
                f"fixture gate {token} hold 1 | fixture streams pipeline & "
                "bg=$!; printf 'PID=%s\\n' \"$bg\"; jobs -p; jobs -l; "
                "wait \"$bg\"; status=$?; printf 'WAIT=%s\\n' \"$status\""
            )
            argv = [self.marsh, "-c", command]
            started = time.monotonic()
            process = subprocess.Popen(
                argv,
                cwd=self.project,
                env=self.environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            try:
                deadline = time.monotonic() + 15
                while not ready.exists() and process.poll() is None and time.monotonic() < deadline:
                    time.sleep(0.02)
                if not ready.exists():
                    raise AssertionError("background pipeline did not reach its container gate")
                status = self.run([self.marsh, "status", "--json"], timeout=10)
                if status.returncode != 0:
                    raise AssertionError("public status failed during background pipeline")
                state = json.loads(status.stdout)
                self.remember_owned_status(state)
                active = [run for run in self.receipts() if run.get("state") == "running"]
                if len(active) < 2 or len({run.get("container_id") for run in active[:2]}) != 2:
                    raise AssertionError("background pipeline did not create two distinct containers")
                state["runs"] = active[:2]
                self.verify_container_evidence(state)
                release.write_bytes(b"go")
                stdout, stderr = process.communicate(timeout=15)
            finally:
                if process.poll() is None:
                    release.write_bytes(b"go")
                    process.kill()
                    stdout, stderr = process.communicate(timeout=5)
            completed = subprocess.CompletedProcess(argv, process.returncode, stdout, stderr)
            self.record_command(argv, self.project, b"", completed, time.monotonic() - started)
            pid = re.search(rb"(?m)^PID=([1-9][0-9]*)$", stdout)
            if process.returncode != 0 or pid is None or b"WAIT=23\n" not in stdout or b"OUT\x00" not in stdout:
                raise AssertionError(
                    f"background pipeline jobs/wait/status failed: stdout={stdout!r} stderr={stderr!r}"
                )
            if not re.search(rb"(?m)^[1-9][0-9]*$", stdout) or not re.search(rb"(?m)^\[[0-9]+\].*[1-9][0-9]*", stdout):
                raise AssertionError("jobs -p/-l did not both expose the background pipeline")

        self.mark("background-pipeline-job-control", background_pipeline)

        self.mark("pty-color-resize-interrupt", self.pty_check)
        self.mark("blocked-stdin-interrupt", self.blocked_stdin_interrupt, requires_worker=False)

        def resources() -> None:
            limits = json.loads(self.gated_shell(["limits"]).stdout)
            expected = {
                "cpu.max": f"{JOB_LIMITS['cpu_millis'] * 100} 100000",
                "memory.max": str(JOB_LIMITS["memory_bytes"]),
                "pids.max": str(JOB_LIMITS["pids"]),
            }
            if limits != expected:
                raise AssertionError(f"unexpected enforced cgroup limits: {limits}")
            cpu_seconds = 2
            cpu = self.gated_shell(["cpu-pressure", str(cpu_seconds)], timeout=10)
            expected_usage = (
                cpu_seconds * 1_000_000 * JOB_LIMITS["cpu_millis"] // 1000
            )
            if (
                cpu.returncode != 0
                or not cpu.stdout.strip().isdigit()
                or int(cpu.stdout) > expected_usage * 14 // 10
            ):
                raise AssertionError(f"CPU ceiling was not enforced: {cpu.stdout!r}")
            for mode, cause in (
                ("memory-pressure", "memory"),
                ("pid-pressure", "pids"),
                ("output-pressure", "output"),
                ("writable-pressure", "writable"),
                ("wall", "wall"),
            ):
                gate = self.start_gated([mode])
                self.state_snapshot(f"active-gate-{gate['token']}")
                released = time.monotonic()
                completed = self.finish_gated(
                    gate, timeout=max(20, JOB_LIMITS["wall_seconds"] + 10)
                )
                elapsed = time.monotonic() - released
                if completed.returncode == 0:
                    raise AssertionError(f"{mode} was not enforced")
                # Writable is detect-and-kill (docs/design/results.md): the
                # fixture's 1 MiB + fsync loop must be stopped well before the
                # wall limit, not merely classified afterwards.
                if cause == "writable" and elapsed > 5:
                    raise AssertionError(f"writable limit took {elapsed:.1f}s to enforce")
                run = self.newest_run(self.state_snapshot(f"limit-{mode}"))
                # Output and wall limits end delivery, which the cause records
                # after the execution outcome ("...; delivery_limit:output").
                observed = run.get("exit", {}).get("cause", "")
                if observed != f"limit:{cause}" and not (
                    cause in ("output", "wall") and observed.endswith(f"; delivery_limit:{cause}")
                ):
                    raise AssertionError(f"{mode} lacked typed limit cause")

        self.mark("resource-and-wall-enforcement", resources)

        def deletion() -> None:
            state = self.state_snapshot("verified-deletion")
            completed = [
                run
                for run in self.runs(state)
                if run.get("state") == "finished" and run.get("container_id") is not None
            ]
            if not completed or any(run.get("cleanup") != "verified" for run in completed):
                raise AssertionError("completed container runs did not report verified deletion")
            active = {container for worker in state.get("workers", []) for container in worker.get("active_container_ids", [])}
            if any(run.get("container_id") in active for run in completed):
                raise AssertionError("deleted container remained active")
            expected = {run["container_id"] for run in completed}
            missing = expected - self.deleted_containers_verified
            if missing:
                raise AssertionError(f"container deletion was not independently inspected: {sorted(missing)}")
            self.result["snapshots"].append({
                "label": "verified-container-deletion-count",
                "source": "host",
                "value": {"count": len(expected)},
            })

        self.mark("verified-container-deletion", deletion, requires_worker=False)

        def authority() -> None:
            report = json.loads(self.gated_shell(["authority"]).stdout)
            if report != {"sockets": [], "authority_env": []}:
                raise AssertionError(f"job received runtime authority: {report}")

        self.mark("no-runtime-authority", authority)

        def timings() -> None:
            started = time.monotonic()
            completed = self.gated_shell(["identity"])
            harness_wall = round((time.monotonic() - started) * 1000)
            if completed.returncode != 0:
                raise AssertionError("timing probe failed")
            run = self.newest_run(self.state_snapshot("phase-timing"))
            timing = run.get("timing", {})
            durations = timing.get("durations_ms", {})
            milestones = timing.get("milestones_unix_ms", {})
            if set(durations) != PHASES or any(
                not isinstance(value, int) or value < 0 for value in durations.values()
            ):
                raise AssertionError(f"phase durations are incomplete or invalid: {durations}")
            names = set(milestones)
            if not REQUIRED_MILESTONES.issubset(names) or not names.issubset(
                REQUIRED_MILESTONES | OPTIONAL_MILESTONES
            ) or any(not isinstance(value, int) for value in milestones.values()):
                raise AssertionError(f"timing milestones are incomplete or invalid: {milestones}")
            ordered = [
                milestones["request_received"],
                milestones["worker_progress"],
                milestones["process_exit"],
                milestones["output_drained"],
                milestones["completed"],
            ]
            if ordered != sorted(ordered):
                raise AssertionError(f"timing milestones are non-monotonic: {milestones}")
            first_output = milestones.get("first_output")
            if first_output is not None and not (
                milestones["worker_progress"] <= first_output <= milestones["output_drained"]
            ):
                raise AssertionError(f"first output is outside the observed relay interval: {milestones}")
            wall = timing.get("wall_ms")
            if not isinstance(wall, int) or abs(sum(durations.values()) - wall) > 250:
                raise AssertionError("phase durations do not reconcile with wall time")
            created = run.get("created_unix_ms")
            finished = run.get("finished_unix_ms")
            if (
                not isinstance(created, int)
                or not isinstance(finished, int)
                or finished < created
                or abs((finished - created) - wall) > 250
            ):
                raise AssertionError(
                    "receipt lifetime does not reconcile with its observed phase wall time"
                )
            # The external probe includes opening and closing a stock-SBX shell
            # attachment around the registered-command request. Those shell
            # lifecycle costs are deliberately outside the job receipt, but a
            # job's measured interval can never exceed the enclosing probe.
            if wall > harness_wall + 100:
                raise AssertionError("product wall time exceeds its enclosing harness probe")
            orchestration = sum(value for phase, value in durations.items() if phase != "execution")
            if timing.get("orchestration_ms") != orchestration:
                raise AssertionError("orchestration duration does not exclude execution exactly")

        self.mark("useful-phase-timing", timings)
        self.mark("uncertain-cleanup-quarantine", self.quarantine_check)

        def coexistence() -> None:
            after = stock_vm_inventory(self.sbx)
            self.result["snapshots"].append({"label": "stock-sbx-after-workloads", "source": "sbx", "value": after})
            # Owned VMs still live here. finish() also rejects post-cleanup leaks.
            preserved = {name: after[name] for name in self.stock_before if name in after}
            errors = stock_cleanup_errors(self.stock_before, preserved)
            if errors:
                raise AssertionError("; ".join(errors))

        self.mark("stock-sbx-client-coexistence", coexistence, requires_worker=False)

    def pty_check(self) -> None:
        self.gate_sequence += 1
        token = f"{self.gate_sequence:04d}"
        ready = self.project / f".marsh-ready-{token}"
        release = self.project / f".marsh-go-{token}"
        master, slave = pty.openpty()
        fcntl.ioctl(slave, 0x80087467, struct.pack("HHHH", 24, 80, 0, 0))
        started = time.monotonic()
        process = subprocess.Popen(
            [self.marsh, "-c", shlex.join(["fixture", "gate", token, "tty"])],
            cwd=self.project,
            env=self.environment,
            stdin=slave, stdout=slave, stderr=slave, start_new_session=True,
        )
        os.close(slave)
        output = bytearray()

        def read_until(needle: bytes, timeout: float) -> None:
            deadline = time.monotonic() + timeout
            while needle not in output and time.monotonic() < deadline:
                ready, _, _ = select.select([master], [], [], 0.1)
                if ready:
                    output.extend(os.read(master, 4096))
            if needle not in output:
                raise AssertionError(f"PTY output missing {needle!r}: {bytes(output)!r}")

        try:
            deadline = time.monotonic() + 15
            while not ready.exists() and process.poll() is None and time.monotonic() < deadline:
                time.sleep(0.02)
            if not ready.exists():
                raise AssertionError("PTY fixture did not reach its observable container gate")
            self.state_snapshot(f"active-gate-{token}")
            release.write_bytes(b"go")
            read_until(b"\x1b[32mCOLOR\x1b[0m", 10)
            os.write(master, b"pasted text\n")
            read_until(b"PASTE:pasted text", 5)
            fcntl.ioctl(master, 0x80087467, struct.pack("HHHH", 40, 120, 0, 0))
            os.killpg(process.pid, signal.SIGWINCH)
            read_until(b"SIZE=40x120", 5)
            os.killpg(process.pid, signal.SIGINT)
            status = process.wait(timeout=10)
            if status != 130:
                raise AssertionError(f"PTY Ctrl-C returned {status}, expected 130")
        finally:
            os.close(master)
            if process.poll() is None:
                process.kill()
        completed = subprocess.CompletedProcess(
            [self.marsh, "-c", shlex.join(["fixture", "gate", token, "tty"])],
            process.returncode,
            bytes(output),
            b"",
        )
        self.record_command(list(completed.args), self.project, b"", completed, time.monotonic() - started)
        self.pty_eof_check()

    def blocked_stdin_interrupt(self) -> None:
        self.gate_sequence += 1
        marker = self.project / f".marsh-blocked-input-{self.gate_sequence:04d}"
        command = f"trap 'exit 130' INT; sh -c 'touch {marker.name}; exec sleep 300'"
        argv = [self.marsh, "-c", command]
        started = time.monotonic()
        process = subprocess.Popen(
            argv,
            cwd=self.project,
            env=self.environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        sent = 0
        stdout = b""
        stderr = b""
        try:
            deadline = time.monotonic() + 15
            while not marker.exists() and process.poll() is None and time.monotonic() < deadline:
                time.sleep(0.02)
            if not marker.exists():
                raise AssertionError("nonreading shell did not reach its ready marker")

            assert process.stdin is not None
            fd = process.stdin.fileno()
            os.set_blocking(fd, False)
            blocked_since: float | None = None
            deadline = time.monotonic() + 8
            chunk = b"x" * (64 * 1024)
            while sent < 8 * 1024 * 1024 and time.monotonic() < deadline:
                try:
                    count = os.write(fd, chunk)
                    sent += count
                    blocked_since = None
                except BlockingIOError:
                    now = time.monotonic()
                    if blocked_since is None:
                        blocked_since = now
                    if now - blocked_since >= 0.25:
                        break
                    select.select([], [fd], [], 0.05)
                if process.poll() is not None:
                    raise AssertionError("nonreading shell exited before interrupt")
            if blocked_since is None or time.monotonic() - blocked_since < 0.25:
                raise AssertionError(f"shell stdin did not sustain backpressure after {sent} bytes")

            os.killpg(process.pid, signal.SIGINT)
            process.stdin.close()
            process.stdin = None
            try:
                stdout, stderr = process.communicate(timeout=10)
            except subprocess.TimeoutExpired as error:
                raise AssertionError(f"Ctrl-C behind blocked stdin timed out after {sent} bytes") from error
            if process.returncode != 130:
                raise AssertionError(
                    f"Ctrl-C behind blocked stdin returned {process.returncode}, expected 130: "
                    f"{stderr.decode(errors='replace')}"
                )
        finally:
            if process.stdin is not None:
                process.stdin.close()
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                stdout, stderr = process.communicate(timeout=5)
        self.result["snapshots"].append({
            "label": "blocked-stdin-interrupt",
            "source": "host",
            "value": {
                "argv": argv,
                "cwd": str(self.project),
                "stdin_byte": "x",
                "stdin_bytes": sent,
                "stdout_base64": base64.b64encode(stdout).decode(),
                "stderr_base64": base64.b64encode(stderr).decode(),
                "status": process.returncode,
                "elapsed_ms": round((time.monotonic() - started) * 1000),
            },
        })

    def pty_eof_check(self) -> None:
        self.gate_sequence += 1
        token = f"{self.gate_sequence:04d}"
        ready = self.project / f".marsh-ready-{token}"
        release = self.project / f".marsh-go-{token}"
        argv = [self.marsh, "-c", shlex.join(["fixture", "gate", token, "tty"])]
        master, slave = pty.openpty()
        fcntl.ioctl(slave, 0x80087467, struct.pack("HHHH", 24, 80, 0, 0))
        started = time.monotonic()
        process = subprocess.Popen(
            argv,
            cwd=self.project,
            env=self.environment,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            start_new_session=True,
        )
        os.close(slave)
        output = bytearray()
        try:
            deadline = time.monotonic() + 15
            while not ready.exists() and process.poll() is None and time.monotonic() < deadline:
                time.sleep(0.02)
            if not ready.exists():
                raise AssertionError("Ctrl-D PTY fixture did not reach its container gate")
            self.state_snapshot(f"active-gate-{token}")
            release.write_bytes(b"go")
            deadline = time.monotonic() + 10
            while b"COLOR" not in output and time.monotonic() < deadline:
                readable, _, _ = select.select([master], [], [], 0.1)
                if readable:
                    output.extend(os.read(master, 4096))
            if b"COLOR" not in output:
                raise AssertionError("Ctrl-D PTY fixture did not become interactive")
            os.write(master, b"\x04")
            deadline = time.monotonic() + 10
            terminal_eof = False
            while process.poll() is None and time.monotonic() < deadline:
                if terminal_eof:
                    time.sleep(0.02)
                    continue
                readable, _, _ = select.select([master], [], [], 0.1)
                if not readable:
                    continue
                try:
                    chunk = os.read(master, 4096)
                except OSError:
                    terminal_eof = True
                    continue
                if not chunk:
                    terminal_eof = True
                    continue
                output.extend(chunk)
            if process.poll() is None:
                raise subprocess.TimeoutExpired(argv, 10)
            status = process.returncode
            while not terminal_eof:
                readable, _, _ = select.select([master], [], [], 0)
                if not readable:
                    break
                try:
                    chunk = os.read(master, 4096)
                except OSError:
                    break
                if not chunk:
                    break
                output.extend(chunk)
            if status != 0 or b"EOF" not in output:
                raise AssertionError(f"PTY Ctrl-D returned {status} without EOF acknowledgement")
        finally:
            os.close(master)
            if process.poll() is None:
                process.kill()
        completed = subprocess.CompletedProcess(argv, process.returncode, bytes(output), b"")
        self.record_command(argv, self.project, b"", completed, time.monotonic() - started)

    def quarantine_check(self) -> None:
        gate = self.start_gated(["hold", "8"])
        process = gate["process"]
        before = self.state_snapshot("before-uncertain-cleanup")
        active = [run for run in self.runs(before) if run.get("state") == "running"]
        if not active:
            raise AssertionError("held container was not active")
        victim = active[0]
        vm_id = victim.get("vm_id")
        stopped = self.run([self.sbx, "stop", str(vm_id)], cwd=self.root, timeout=20)
        if stopped.returncode != 0:
            raise AssertionError("stock sbx could not stop the exact acceptance VM")
        stdout, stderr = process.communicate(timeout=15)
        completed = subprocess.CompletedProcess(gate["argv"], process.returncode, stdout, stderr)
        self.record_command(gate["argv"], self.project, b"", completed, time.monotonic() - gate["started"])
        after = self.state_snapshot("after-uncertain-cleanup")
        run = next((item for item in self.runs(after) if item.get("job_id") == victim.get("job_id")), None)
        worker = next((item for item in after.get("workers", []) if item.get("worker_id") == victim.get("worker_id")), None)
        if not run or run.get("cleanup") != "uncertain" or not worker or worker.get("health") != "quarantined":
            raise AssertionError("uncertain cleanup did not quarantine the affected worker")
        rejected = self.shell("fixture identity", timeout=30)
        if rejected.returncode == 0:
            raise AssertionError("a command reused the quarantined worker")
        message = rejected.stderr.decode(errors="replace").lower()
        if not any(word in message for word in ("quarantined", "unhealthy", "capacity")):
            raise AssertionError(f"quarantine rejection was not actionable: {rejected.stderr!r}")
        final = self.state_snapshot("quarantined-worker-not-reused")
        if any(
            item.get("state") == "running"
            and (item.get("vm_id") == vm_id or item.get("worker_id") == victim.get("worker_id"))
            for item in self.runs(final)
        ):
            raise AssertionError("a quarantined worker or VM was reused")



class Smoke(IsolatedScopeCleanup):
    def __init__(self, arguments: argparse.Namespace) -> None:
        self.marsh = str(pathlib.Path(arguments.marsh).resolve())
        guest_artifacts, build_receipt = verify_candidate(arguments, self.marsh)
        runtime_environment = candidate_environment(arguments, build_receipt)
        self.sbx = resolve_executable(arguments.sbx)
        kit_mapping = arguments.kit
        if IMAGE.fullmatch(kit_mapping):
            self.kit = kit_mapping
        else:
            source = pathlib.Path(kit_mapping).expanduser().resolve()
            if not source.is_dir():
                raise ValueError("--kit must be a native v3 source directory or immutable OCI digest")
            kit_mapping = str(source)
            self.kit = f"local-v3:{source}"
        self.evidence = pathlib.Path(arguments.evidence).resolve()
        destination = host_only_path(self.evidence / "smoke.json", pathlib.Path(arguments.source_tree))
        destination.unlink(missing_ok=True)
        self.evidence.mkdir(parents=True, mode=0o700, exist_ok=True)
        source_identity_value = source_identity(
            pathlib.Path(arguments.source_tree), arguments.source_revision
        )
        self.root = disposable_root("marsh-smoke-")
        try:
            self.project = self.root / "project"
            self.home = self.root / "home"
            self.control_root = self.root / "control"
            self.project.mkdir()
            self.home.mkdir(mode=0o700)
            self.guest_home = self.home / "home"
            self.guest_home.mkdir(mode=0o700)
            self.control_root.mkdir(mode=0o700)
            self.control_home = scoped_control_home(self.control_root, self.home)
            (self.control_home / "commands.json").write_text(
                json.dumps({"fixture": kit_mapping}, sort_keys=True) + "\n",
                encoding="utf-8",
            )
        except Exception:
            shutil.rmtree(self.root, ignore_errors=True)
            raise
        self.environment = os.environ.copy()
        self.environment.pop("MARSH_LOCAL_SHELL_AUTHORITY", None)
        self.environment.update(runtime_environment)
        self.environment["MARSH_HOME"] = str(self.home)
        self.environment["MARSH_CONTROL_HOME"] = str(self.control_root)
        self.environment["MARSH_SBX"] = self.sbx
        self.environment["MARSH_GUEST_ARTIFACTS"] = str(guest_artifacts)
        self.initialize_scope_cleanup()
        self.source = source_identity_value
        self.source["sbx_binary"] = self.sbx
        self.source["verified_build_receipt"] = build_receipt
        self.stock_before = None
        self.records: list[dict[str, Any]] = []

    def run(
        self,
        argv: list[str],
        *,
        stdin: bytes = b"",
        timeout: float = 120,
        check: bool = True,
    ) -> subprocess.CompletedProcess[bytes]:
        started = time.monotonic()
        if argv and argv[0] == self.marsh:
            self.scope_started = True
        completed = subprocess.run(
            argv,
            cwd=self.project,
            env=self.environment,
            input=stdin,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout,
            check=False,
        )
        self.records.append(
            {
                "argv": argv,
                "status": completed.returncode,
                "elapsed_ms": round((time.monotonic() - started) * 1000),
                "stdout": completed.stdout.decode(errors="replace"),
                "stderr": completed.stderr.decode(errors="replace"),
            }
        )
        if check and completed.returncode != 0:
            raise RuntimeError(
                f"{shlex.join(argv)} failed ({completed.returncode}): "
                f"{completed.stderr.decode(errors='replace').strip()}"
            )
        return completed

    def shell(self, script: str, *, stdin: bytes = b"", check: bool = True) -> subprocess.CompletedProcess[bytes]:
        return self.run([self.marsh, "-c", script], stdin=stdin, check=check)

    def document(self, *arguments: str) -> dict[str, Any]:
        document = json.loads(self.run([self.marsh, *arguments]).stdout)
        if list(arguments) == ["status", "--json"]:
            self.remember_owned_status(document)
        return document

    def receipts(self) -> list[dict[str, Any]]:
        jobs = self.document("jobs", "--json")
        return [
            self.document("jobs", "show", summary["job_id"], "--json")
            for summary in jobs["jobs"]
            if summary["command"] == "fixture"
        ]

    def wait_for(self, paths: list[pathlib.Path], processes: list[subprocess.Popen[bytes]]) -> None:
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if all(path.exists() for path in paths):
                return
            failed = [process.returncode for process in processes if process.poll() is not None]
            if failed:
                raise RuntimeError(f"parallel shell exited before its gate: {failed}")
            time.sleep(0.05)
        raise TimeoutError("parallel shells did not reach their container gates")

    def run_all(self) -> None:
        self.stock_before = stock_vm_inventory(self.sbx)
        self.source["stock_before"] = self.stock_before
        status = self.document("status", "--json")
        if status.get("schema") != "marsh.status/v1":
            raise AssertionError("status handshake has the wrong schema")

        cold = [
            subprocess.Popen(
                [self.marsh, "--load", "fixture", "-c", "true"],
                cwd=self.project,
                env=self.environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            for _ in range(2)
        ]
        cold_stderr = bytearray()
        for process in cold:
            _, stderr = process.communicate(timeout=300)
            cold_stderr.extend(stderr)
            if process.returncode != 0:
                raise RuntimeError(
                    f"parallel cold --load failed ({process.returncode}): "
                    f"{stderr.decode(errors='replace')}"
                )

        warm_started = time.monotonic()
        second = self.run([self.marsh, "--load", "fixture", "-c", "true"], timeout=120)
        warm_ms = round((time.monotonic() - warm_started) * 1000)
        if b"[starting fixture worker VM" in second.stderr:
            raise AssertionError("warm preflight printed a false fixture cold-boot notice")
        if warm_ms > 10_000:
            raise AssertionError(f"ready worker reuse took {warm_ms} ms")

        warm = self.document("status", "--json")
        workers = [
            worker
            for worker in warm["workers"]
            if kit_profile_matches(worker.get("kit_profile"), self.kit)
        ]
        if len(workers) != 1 or not workers[0]["warm"] or workers[0]["health"] != "ready":
            raise AssertionError(f"fixture prewarm did not expose one ready worker: {workers!r}")
        vm = workers[0]["vm_id"]

        identity = self.shell(
            'printf "cwd=%s\\nhome=%s\\nuser=%s\\n" "$PWD" "$HOME" "$USER"'
        ).stdout.decode()
        expected_home = f"/Users/{os.environ['USER']}"
        if f"cwd={self.project}\n" not in identity or f"home={expected_home}\n" not in identity:
            raise AssertionError(f"job paths are not natural: {identity!r}")

        package = self.shell(
            f"sudo -n true && {APT_UPDATE_READY} && "
            "sudo -n env DEBIAN_FRONTEND=noninteractive "
            "apt-get install -y -qq --no-install-recommends ed >/dev/null && "
            "test -x /usr/bin/ed && "
            "test \"$(stat -c %U:%G:%a /etc/sudoers.d/marsh)\" = root:root:440 && "
            "! grep -Rqs 'http://deb.debian.org/' "
            "/etc/apt/sources.list /etc/apt/sources.list.d"
        )
        if package.returncode != 0:
            raise AssertionError(
                "project shell could not install a Debian package through confined sudo: "
                f"{package.stderr.decode(errors='replace')}"
            )

        self.shell("printf natural > natural-write.txt")
        if (self.project / "natural-write.txt").read_text(encoding="utf-8") != "natural":
            raise AssertionError("job write did not reach the exact host project path")
        # ENOEXEC (no shebang) runs under marsh like Bash, not the platform
        # /bin/sh: `[[ ]]` and arrays are not POSIX sh.
        plain = self.shell(
            "printf '%s\\n' 'a=(x y); [[ ${a[1]} == y ]] && printf \"plain:%s\" \"$1\"' > no-shebang && "
            "chmod +x no-shebang && ./no-shebang ok; r=$?; rm -f no-shebang; exit $r",
            check=False,
        )
        if plain.returncode != 0 or plain.stdout != b"plain:ok":
            raise AssertionError(f"no-shebang script did not run under marsh: {plain!r}")
        self.shell('printf selected-home > "$HOME/.marsh-uat-home"')
        persisted = self.shell('cat "$HOME/.marsh-uat-home"')
        if persisted.stdout != b"selected-home":
            raise AssertionError("selected HOME did not persist across fresh job containers")
        if (self.guest_home / ".marsh-uat-home").read_bytes() != b"selected-home":
            raise AssertionError("selected HOME did not write to MARSH_HOME/home")
        if (self.home / ".marsh-uat-home").exists():
            raise AssertionError("selected HOME wrote into the host control scope")
        ephemeral = self.run(
            [
                self.marsh,
                "--ephemeral-home",
                "-c",
                'test ! -e "$HOME/.marsh-uat-home" && printf ephemeral > "$HOME/.ephemeral-only"',
            ]
        )
        if ephemeral.returncode != 0 or (self.guest_home / ".ephemeral-only").exists():
            raise AssertionError("--ephemeral-home read from or wrote to the persistent selected home")

        stream = self.shell("cat; printf err >&2; exit 23", stdin=b"stdin\x00bytes\xff", check=False)
        if stream.returncode != 23 or stream.stdout != b"stdin\x00bytes\xff" or stream.stderr != b"err":
            raise AssertionError("stdin/stdout/stderr/exit status were not byte faithful")
        pipeline = self.shell("printf pipeline | tr a-z A-Z")
        if pipeline.stdout != b"PIPELINE":
            raise AssertionError("registered command failed inside a shell pipeline")
        composition = self.shell(
            "printf input | fanout { "
            "first: cat | tr a-z A-Z; "
            "second: sed s/input/branch/ "
            "} | collect --timing"
        )
        if b"== first (complete) ==\nINPUT" not in composition.stdout or b"== second (complete) ==\nbranch" not in composition.stdout:
            raise AssertionError("fanout pipelines were not collected in declaration order")
        total = re.search(rb"(?m)^  total\s+([0-9]+) ms$", composition.stdout)
        if total is None or int(total.group(1)) >= 1_000:
            raise AssertionError(
                f"warm local fanout orchestration exceeded one second: {composition.stdout!r}"
            )
        background = self.shell(
            "sleep 1 & pid=$!; jobs -p; wait \"$pid\"; "
            "printf background > bg.out; cat bg.out"
        )
        if not re.search(rb"(?m)^[1-9][0-9]*$", background.stdout) or not background.stdout.endswith(b"background"):
            raise AssertionError("background PID/jobs/wait/output behavior failed")

        wait_next = self.shell(
            "sh -c 'sleep .05; exit 23' & first=$!; "
            "sh -c 'sleep .2; exit 17' & second=$!; "
            "wait -n -p finished \"$first\" \"$second\"; status=$?; "
            "test \"$status\" = 23 && test \"$finished\" = \"$first\" && "
            "wait \"$second\"; test \"$?\" = 17"
        )
        if wait_next.returncode != 0:
            raise AssertionError("wait -n -p lost the first child status or identity")

        wait_stopped = self.shell(
            "sh -c 'kill -STOP $$; exit 23' & pid=$!; "
            "sh -c \"sleep .1; kill -CONT $pid\" & "
            "wait -n -f -p finished \"$pid\"; status=$?; "
            "test \"$status\" = 23 && test \"$finished\" = \"$pid\""
        )
        if wait_stopped.returncode != 0:
            raise AssertionError("wait -n -f did not wait for the stopped child to terminate")

        signal_ready = self.project / ".signal-ready"
        signaled = subprocess.Popen(
            [
                self.marsh,
                "-c",
                "trap 'exit 130' INT; sh -c 'touch .signal-ready; exec sleep 300'",
            ],
            cwd=self.project,
            env=self.environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        try:
            self.wait_for([signal_ready], [signaled])
            time.sleep(0.2)
            os.killpg(signaled.pid, signal.SIGINT)
            try:
                _, signal_stderr = signaled.communicate(timeout=15)
            except subprocess.TimeoutExpired as error:
                os.killpg(signaled.pid, signal.SIGKILL)
                _, final_stderr = signaled.communicate(timeout=5)
                partial = error.stderr or b""
                raise AssertionError(
                    "Ctrl-C timed out: "
                    + (partial + final_stderr).decode(errors="replace")
                ) from error
            if signaled.returncode != 130:
                raise AssertionError(
                    f"Ctrl-C returned {signaled.returncode}, expected 130: "
                    f"{signal_stderr.decode(errors='replace')}"
                )
        finally:
            if signaled.poll() is None:
                os.killpg(signaled.pid, signal.SIGKILL)
                signaled.wait(timeout=5)

        term_ready = self.project / ".term-ready"
        terminated = subprocess.Popen(
            [
                self.marsh,
                "-c",
                "trap 'printf term-trap; exit 143' TERM; "
                "sh -c 'touch .term-ready; exec sleep 300'",
            ],
            cwd=self.project,
            env=self.environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        try:
            self.wait_for([term_ready], [terminated])
            time.sleep(0.2)
            os.killpg(terminated.pid, signal.SIGTERM)
            try:
                term_stdout, term_stderr = terminated.communicate(timeout=15)
            except subprocess.TimeoutExpired as error:
                os.killpg(terminated.pid, signal.SIGKILL)
                terminated.communicate(timeout=5)
                raise AssertionError("SIGTERM cleanup trap timed out") from error
            if terminated.returncode != 143 or term_stdout != b"term-trap":
                raise AssertionError(
                    f"SIGTERM trap returned {terminated.returncode}, "
                    f"stdout={term_stdout!r}, stderr={term_stderr!r}"
                )
        finally:
            if terminated.poll() is None:
                os.killpg(terminated.pid, signal.SIGKILL)
                terminated.wait(timeout=5)

        idle_ready = self.project / ".idle-term-ready"
        idle = subprocess.Popen(
            [self.marsh, "-s"],
            cwd=self.project,
            env=self.environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        try:
            assert idle.stdin is not None
            idle.stdin.write(
                b"trap 'printf idle-term; exit 143' TERM\n"
                b"touch .idle-term-ready\n"
            )
            idle.stdin.flush()
            self.wait_for([idle_ready], [idle])
            time.sleep(0.2)
            os.killpg(idle.pid, signal.SIGTERM)
            deadline = time.monotonic() + 15
            while idle.poll() is None and time.monotonic() < deadline:
                time.sleep(0.05)
            if idle.poll() is None:
                os.killpg(idle.pid, signal.SIGKILL)
                idle.communicate(timeout=5)
                raise AssertionError("idle SIGTERM cleanup trap timed out")
            try:
                idle_stdout, idle_stderr = idle.communicate(timeout=5)
            except subprocess.TimeoutExpired as error:
                os.killpg(idle.pid, signal.SIGKILL)
                idle.communicate(timeout=5)
                raise AssertionError("idle SIGTERM output read timed out") from error
            if idle.returncode != 143 or idle_stdout != b"idle-term":
                raise AssertionError(
                    f"idle SIGTERM trap returned {idle.returncode}, "
                    f"stdout={idle_stdout!r}, stderr={idle_stderr!r}"
                )
        finally:
            if idle.poll() is None:
                os.killpg(idle.pid, signal.SIGKILL)
                idle.wait(timeout=5)

        self.pty_resize_color()

        gates = [self.project / ".ready-a", self.project / ".ready-b"]
        release = self.project / ".release"
        scripts = [
            f"touch {ready}; while test ! -e {release}; do sleep 0.05; done; printf shell"
            for ready in gates
        ]
        processes = [
            subprocess.Popen(
                [self.marsh, "-c", script],
                cwd=self.project,
                env=self.environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                start_new_session=True,
            )
            for script in scripts
        ]
        try:
            self.wait_for(gates, processes)
            active = self.document("status", "--json")
            attached = [shell for shell in active["shells"] if shell["state"] == "attached"]
            if len(attached) < 2 or len({shell["daemon_id"] for shell in attached}) != 1:
                raise AssertionError("two parallel shells did not share one daemon")
            release.write_text("go", encoding="utf-8")
            for process in processes:
                stdout, stderr = process.communicate(timeout=30)
                if process.returncode != 0:
                    raise RuntimeError(f"parallel shell failed: {stderr.decode(errors='replace')}")
        finally:
            release.touch()
            for process in processes:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=5)

        for _ in range(2):
            identity = self.shell("fixture identity")
            if not identity.stdout.strip():
                raise AssertionError("registered fixture identity command produced no output")

        receipts = [receipt for receipt in self.receipts() if receipt["state"] == "finished"]
        if len(receipts) < 2:
            raise AssertionError("parallel runs are absent from durable jobs")
        recent = receipts[:2]
        containers = [receipt["container_id"] for receipt in recent]
        if (
            len({receipt["vm_id"] for receipt in recent}) != 1
            or len(set(containers)) != 2
            or not all(CONTAINER.fullmatch(container or "") for container in containers)
            or any(receipt["cleanup"] != "verified" for receipt in recent)
        ):
            raise AssertionError(f"fresh-container/reused-VM/cleanup evidence failed: {recent!r}")
        for container in containers:
            verify_container_deleted(
                lambda argv: self.run(argv, timeout=15, check=False),
                [self.sbx, "exec", "-u", "root", vm],
                container,
            )

        results = self.document("results", "--json")
        if results.get("schema") != "marsh.jobs/v1" or len(results.get("jobs", [])) < 2:
            raise AssertionError(f"durable result summary is incomplete: {results!r}")
        cursor = str(results["jobs"][0]["cursor"])
        shown = self.document("results", "show", cursor, "--json")
        if shown.get("cursor") != int(cursor) or shown.get("cleanup") != "verified":
            raise AssertionError(f"durable result lookup is incomplete: {shown!r}")

        # The first run may boot both worker and shell VMs; retain this useful
        # diagnostic without prescribing which one was cold on a reused host.
        self.records.append(
            {
                "cold_boot_stderr": cold_stderr.decode(errors="replace"),
                "warm_reuse_ms": warm_ms,
            }
        )

    def pty_resize_color(self) -> None:
        master, slave = pty.openpty()
        fcntl.ioctl(slave, 0x80087467, struct.pack("HHHH", 24, 80, 0, 0))
        script = (
            "printf '\\033[32mCOLOR\\033[0m\\n'; "
            "IFS= read -r barrier; "
            "while :; do "
            "size=$(stty size); "
            "if test \"$size\" = '40 120'; then printf 'SIZE=%s\\n' \"$size\"; exit 47; fi; "
            "sleep 0.05; "
            "done"
        )
        process = subprocess.Popen(
            [self.marsh, "-c", script],
            cwd=self.project,
            env=self.environment,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            start_new_session=True,
        )
        os.close(slave)
        output = bytearray()
        try:
            deadline = time.monotonic() + 20
            while b"\x1b[32mCOLOR\x1b[0m" not in output and time.monotonic() < deadline:
                readable, _, _ = select.select([master], [], [], 0.1)
                if readable:
                    chunk = os.read(master, 4096)
                    if not chunk:
                        break
                    output.extend(chunk)
            if b"\x1b[32mCOLOR\x1b[0m" not in output:
                raise AssertionError(f"PTY lost color bytes: {bytes(output)!r}")
            fcntl.ioctl(master, 0x80087467, struct.pack("HHHH", 40, 120, 0, 0))
            os.killpg(process.pid, signal.SIGWINCH)
            os.write(master, b"continue\n")
            output_deadline = time.monotonic() + 15
            while b"40 120" not in output and time.monotonic() < output_deadline:
                readable, _, _ = select.select([master], [], [], 0.1)
                if not readable:
                    continue
                try:
                    chunk = os.read(master, 4096)
                    if not chunk:
                        break
                    output.extend(chunk)
                except OSError:
                    break
            status = process.wait(timeout=5)
            if status != 47 or b"40 120" not in output:
                raise AssertionError(
                    f"PTY resize/exit status failed: status={status}, output={bytes(output)!r}"
                )
        finally:
            os.close(master)
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)

    def finish(self, error: Exception | None) -> int:
        cleanup_errors = self.cleanup_isolated_scope()
        if self.stock_before is not None:
            try:
                after = stock_vm_inventory(self.sbx)
                self.source["stock_after"] = after
                cleanup_errors.extend(stock_cleanup_errors(self.stock_before, after))
            except Exception as caught:
                cleanup_errors.append(f"independent stock cleanup unavailable: {caught}")
        if cleanup_errors:
            cleanup = RuntimeError("; ".join(cleanup_errors))
            error = cleanup if error is None else RuntimeError(f"{error}; cleanup: {cleanup}")
        destination = self.evidence / "smoke.json"
        destination.write_text(
            json.dumps(
                {
                    "outcome": "failed" if error else "passed",
                    "failure": str(error) if error else None,
                    "root": str(self.root),
                    "environment": self.source,
                    "records": self.records,
                },
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
        destination.chmod(0o600)
        print(f"acceptance smoke: {'failed' if error else 'passed'}; evidence: {destination}")
        if error:
            print(f"acceptance smoke: {error}")
        return 1 if error else 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    candidate_arguments(parser)
    parser.add_argument("--marsh", required=True)
    parser.add_argument("--sbx", required=True)
    parser.add_argument(
        "--kit", required=True, help="native v3 source directory or immutable OCI digest"
    )
    parser.add_argument("--evidence", required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--source-tree", required=True)
    return parser.parse_args()


def smoke_main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Fast fail real-product acceptance smoke gate.")
    candidate_arguments(parser)
    parser.add_argument("--marsh", required=True)
    parser.add_argument("--sbx", default="sbx")
    parser.add_argument(
        "--kit", required=True, help="native v3 source directory or immutable OCI digest"
    )
    parser.add_argument("--evidence", required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--source-tree", required=True)
    smoke = Smoke(parser.parse_args(argv))
    try:
        smoke.run_all()
    except KeyboardInterrupt:
        return smoke.finish(InterruptedError("acceptance smoke interrupted"))
    except Exception as error:  # preserve evidence from the first real failure
        return smoke.finish(error)
    return smoke.finish(None)


def main() -> int:
    harness = Harness(parse_args())
    try:
        failure = harness.prepare()
        if failure:
            return harness.finish(failure)
        harness.run_all()
    except KeyboardInterrupt:
        return harness.finish("acceptance interrupted")
    except Exception as error:
        return harness.finish(unexpected_failure(error))
    return harness.finish()


if __name__ == "__main__":
    raise SystemExit(main())
