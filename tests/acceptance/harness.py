"""Shared acceptance harness: isolated scopes, VM ownership, cleanup, callers.

Layering (no suite imports another suite):
  provenance.py   source/build identity and read-only stock-SBX inventory
  harness.py      this module: one isolated-scope implementation
                  (`IsolatedScope`), the product scope every scenario suite
                  drives (`ProductScope`), the scenario driver
                  (`ScenarioSuite`) and the shared caller/assertion helpers
  suites          smoke.py, run.py, workspaces_uat.py, processes_uat.py,
                  shells_uat.py, acp_uat.py, mcp_gateway_uat.py, ...

VM safety: a scope removes only stock VMs named in its own daemon's ownership
map (or created by the harness itself) whose stable stock IDs it learned
through authenticated status; every pre-existing VM must stay unchanged.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import pathlib
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
from typing import Any, Callable

from provenance import (candidate_environment, host_only_path, remove_owned_stock_vm, source_identity, stock_baseline,
                        stock_cleanup_errors, stock_vm_inventory, stock_vm_names, verify_candidate)


IMAGE = re.compile(r"^[a-z0-9._:/][a-z0-9._:/-]*@sha256:[0-9a-f]{64}$")
CONTAINER = re.compile(r"^[0-9a-f]{64}$")
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


ACP_FIXTURE_FILES = ("acp-fixture.yaml", "acp-fixture.dockerfile", "agent.mjs")


def stage_acp_fixture(destination: pathlib.Path) -> pathlib.Path:
    """The ACP fixture Kit source plus the canonical packaging/image-base inputs
    its Dockerfile assembles from, staged under DESTINATION in a directory
    named by their content: changed inputs get a new path (a registry change),
    so a warm daemon's captured source is never rewritten under it."""
    here = pathlib.Path(__file__).resolve().parent
    image_base = here.parents[1] / "packaging" / "image-base"
    files = {name: (here / "acp-fixture" / name).read_bytes() for name in ACP_FIXTURE_FILES}
    files.update({"image-base/" + path.name: path.read_bytes()
                  for path in sorted(image_base.iterdir()) if path.is_file()})
    hasher = hashlib.sha256()
    for name in sorted(files):
        hasher.update(name.encode() + b"\0" + hashlib.sha256(files[name]).digest())
    kit = destination / hasher.hexdigest()[:16] / "acp-fixture"
    if not kit.exists():
        destination.mkdir(parents=True, exist_ok=True)
        staging = pathlib.Path(tempfile.mkdtemp(prefix=".acp-fixture-", dir=destination))
        for name, data in files.items():
            (staging / name).parent.mkdir(parents=True, exist_ok=True)
            (staging / name).write_bytes(data)
        kit.parent.mkdir(exist_ok=True)
        os.rename(staging, kit)
    return kit


def disposable_root(prefix: str) -> pathlib.Path:
    """Create mutable acceptance state outside the user's natural home tree."""
    parent = pathlib.Path("/private/tmp") if sys.platform == "darwin" else pathlib.Path(tempfile.gettempdir())
    # tests/regress.py keeps each suite's roots in its own directory so it can
    # find the suite's ownership maps without touching anyone else's VMs.
    parent = pathlib.Path(os.environ.get("MARSH_UAT_ROOT") or parent).resolve()
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


class IsolatedScope:
    """Track and remove only resources authenticated by one isolated daemon."""

    def initialize_scope_cleanup(self) -> None:
        self.scope_started = False
        self.owned_scope_id: str | None = None
        self.owned_daemon_id: str | None = None
        self.owned_daemon_pid: int | None = None
        self.owned_daemon_process_identity: tuple[int, str] | None = None
        self.owned_daemon_control_token: str | None = None
        self.owned_vms: set[str] = set()
        self.owned_names_seen: set[str] = set()
        self.extra_owned_names: set[str] = set()

    def owned_stock_names(self) -> set[str]:
        """Names of every stock VM this scope owns: its daemon's ownership map
        (read live while it exists, remembered after the root is removed), the
        identities learned from authenticated status, and VMs the harness
        itself created (extra_owned_names)."""
        names = set(getattr(self, "owned_names_seen", ())) | set(getattr(self, "extra_owned_names", ()))
        names |= set(getattr(self, "owned_vm_identities", {}))
        try:
            names |= product_owned_vm_names(getattr(self, "control_home", None))
        except (OSError, AssertionError, ValueError):
            pass
        self.owned_names_seen = set(names) - set(getattr(self, "extra_owned_names", ()))
        return names

    def stock_leftover_errors(self, after: dict[str, str]) -> list[str]:
        """Pre-existing VMs unchanged and none of this scope's own remaining."""
        return stock_cleanup_errors(self.stock_before, after, owned=self.owned_stock_names())

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
        IsolatedScope.remember_owned_status(self, status)
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
                    refresh_error = IsolatedScope.refresh_owned_status_before_cleanup(self)
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
        self.owned_stock_names()  # remember them: the root (and its map) is removed below
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
                errors.extend(self.stock_leftover_errors(after))
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


def unexpected_failure(error: BaseException) -> str:
    qualified = f"{type(error).__module__}.{type(error).__qualname__}"
    detail = str(error)
    heading = f"acceptance failed unexpectedly: {qualified}"
    if detail:
        heading += f": {detail}"
    trace = "".join(traceback.format_exception(type(error), error, error.__traceback__)).strip()
    return f"{heading}\n{trace}"


def sha256(path: pathlib.Path) -> str:
    return hashlib.sha256(pathlib.Path(path).read_bytes()).hexdigest()


class Fail(AssertionError):
    pass


def require(condition: object, message: str) -> None:
    if not condition:
        raise Fail(message)


def text(value: bytes | None) -> str:
    return (value or b"").decode(errors="replace")


def find_records(document: Any, predicate: Callable[[dict[str, Any]], bool]) -> list[dict[str, Any]]:
    found: list[dict[str, Any]] = []
    stack = [document]
    while stack:
        value = stack.pop()
        if isinstance(value, dict):
            if predicate(value):
                found.append(value)
            stack.extend(value.values())
        elif isinstance(value, list):
            stack.extend(value)
    return found


def wait_until(check, description: str, timeout: float = 15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.1)
    raise AssertionError(f"timed out waiting for {description}")


class ProductScope(IsolatedScope):
    """One isolated MARSH_HOME/control scope driving an installed or candidate product."""

    EVIDENCE = "smoke.json"
    ROOT_PREFIX = "marsh-smoke-"

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
        destination = host_only_path(self.evidence / self.EVIDENCE, pathlib.Path(arguments.source_tree))
        destination.unlink(missing_ok=True)
        self.evidence.mkdir(parents=True, mode=0o700, exist_ok=True)
        source_identity_value = source_identity(
            pathlib.Path(arguments.source_tree), arguments.source_revision
        )
        self.root = disposable_root(self.ROOT_PREFIX)
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

    def scope_cleanup_errors(self) -> list[str]:
        """Remove this scope's own VMs and root; prove pre-existing VMs unchanged."""
        cleanup_errors = self.cleanup_isolated_scope()
        if self.stock_before is not None:
            try:
                after = stock_vm_inventory(self.sbx)
                self.source["stock_after"] = after
                cleanup_errors.extend(self.stock_leftover_errors(after))
            except Exception as caught:
                cleanup_errors.append(f"independent stock cleanup unavailable: {caught}")
        return cleanup_errors


GIT_ENV = {"GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1", "GIT_OPTIONAL_LOCKS": "0"}


class ScenarioSuite(ProductScope):
    """A named-scenario suite (workspaces, processes, shells) over one ProductScope.

    Subclasses set LABEL, EVIDENCE, SCENARIOS (id -> method) and ORDER.
    """

    LABEL = "scenarios"
    SCENARIOS: dict[str, str] = {}
    ORDER: list[str] = []

    def __init__(self, arguments: argparse.Namespace) -> None:
        super().__init__(arguments)
        self.prefix_bin = str(pathlib.Path(self.marsh).parent)
        self.selected = [s for s in self.ORDER if not arguments.only or s in arguments.only]
        self.fail_fast = arguments.fail_fast
        self.preflight_enabled = not getattr(arguments, "no_preflight", True)
        self.results: list[dict[str, Any]] = []
        self.live: list[subprocess.Popen[bytes]] = []
        self.host_env = {**self.environment, **GIT_ENV,
                         "PATH": f"{self.prefix_bin}:{self.environment.get('PATH', '/usr/bin:/bin')}",
                         "LC_ALL": "C"}

    # ---- callers -------------------------------------------------------
    def _record(self, kind: str, argv: Any, cwd: pathlib.Path, started: float,
                status: int | None, stdout: bytes, stderr: bytes) -> None:
        self.records.append({
            "caller": kind, "argv": argv, "cwd": str(cwd), "status": status,
            "elapsed_ms": round((time.monotonic() - started) * 1000),
            "stdout": text(stdout)[-8000:], "stderr": text(stderr)[-8000:],
        })

    def call(self, kind: str, argv: list[str], cwd: pathlib.Path, *, stdin: bytes = b"",
             timeout: float = 240) -> subprocess.CompletedProcess[bytes]:
        self.scope_started = True
        started = time.monotonic()
        process = subprocess.Popen(argv, cwd=cwd, env=self.host_env, stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   start_new_session=True)
        try:
            stdout, stderr = process.communicate(input=stdin, timeout=timeout)
        except subprocess.TimeoutExpired:
            self.kill(process)  # the whole process group: no orphaned callers
            self._record(kind, argv, cwd, started, None, b"", b"")
            raise Fail(f"{kind} timed out after {timeout}s: {shlex.join(argv)[:300]}") from None
        finally:
            self.kill(process)
        self._record(kind, argv, cwd, started, process.returncode, stdout, stderr)
        return subprocess.CompletedProcess(argv, process.returncode, stdout, stderr)

    def bash(self, script: str, cwd: pathlib.Path, **kw: Any) -> subprocess.CompletedProcess[bytes]:
        """Plain host bash invoking the installed CLI (`marsh` on PATH)."""
        return self.call("host-bash", ["/bin/bash", "-c", script], cwd, **kw)

    def brush(self, script: str, cwd: pathlib.Path, **kw: Any) -> subprocess.CompletedProcess[bytes]:
        """One marsh session (Brush in the shell VM)."""
        return self.call("marsh-session", [self.marsh, "-c", script], cwd, **kw)

    def cli(self, args: list[str], cwd: pathlib.Path, **kw: Any) -> subprocess.CompletedProcess[bytes]:
        return self.call("host-cli", [self.marsh, *args], cwd, **kw)

    def spawn(self, argv: list[str], cwd: pathlib.Path) -> subprocess.Popen[bytes]:
        self.scope_started = True
        process = subprocess.Popen(argv, cwd=cwd, env=self.host_env, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   start_new_session=True)
        self.live.append(process)
        self.records.append({"caller": "spawn", "argv": argv, "cwd": str(cwd), "pid": process.pid})
        return process

    def reap(self, process: subprocess.Popen[bytes], timeout: float) -> tuple[int, bytes, bytes]:
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.kill(process)
            raise Fail(f"process {process.args!r} did not exit within {timeout}s") from None
        self.records.append({"caller": "reap", "pid": process.pid, "status": process.returncode,
                             "stdout": text(stdout)[-8000:], "stderr": text(stderr)[-8000:]})
        return process.returncode, stdout, stderr

    @staticmethod
    def kill(process: subprocess.Popen[bytes]) -> None:
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                process.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                pass

    def wait_until(self, predicate: Callable[[], bool], timeout: float, what: str,
                   processes: tuple[subprocess.Popen[bytes], ...] = ()) -> None:
        deadline = time.monotonic() + timeout
        while not predicate():
            for process in processes:
                if process.poll() is not None:
                    _, out, err = self.reap(process, 5)
                    raise Fail(f"{what}: caller exited {process.returncode} first: "
                               f"stdout={text(out)[-400:]!r} stderr={text(err)[-800:]!r}")
            if time.monotonic() > deadline:
                raise Fail(f"timed out after {timeout}s waiting for {what}")
            time.sleep(0.25)

    # ---- projects and observation --------------------------------------
    def git(self, cwd: pathlib.Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[bytes]:
        completed = subprocess.run(
            ["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid",
             "-c", "commit.gpgsign=false", *args],
            cwd=cwd, env={**os.environ, **GIT_ENV, "HOME": str(self.root)},
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            timeout=60, check=False)
        if check and completed.returncode != 0:
            raise RuntimeError(f"git {args}: {text(completed.stderr)}")
        return completed

    def new_repo(self, name: str, files: dict[str, bytes | str], *, commit: bool = True) -> pathlib.Path:
        project = self.root / name
        project.mkdir()
        for relative, content in files.items():
            path = project / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content.encode() if isinstance(content, str) else content)
        if commit:
            self.git(project, "init", "-q", "-b", "main")
            self.git(project, "add", "-A")
            self.git(project, "commit", "-q", "-m", "seed")
        return project


    @staticmethod
    def tree_state(root: pathlib.Path) -> dict[str, tuple[Any, ...]]:
        """Every user path (not `.git`/`.marsh`): kind, exec bit, bytes digest or link target."""
        state: dict[str, tuple[Any, ...]] = {}
        for path in sorted(root.rglob("*")):
            relative = path.relative_to(root)
            if relative.parts[0] in {".git", ".marsh"}:
                continue
            if path.is_symlink():
                state[str(relative)] = ("l", os.readlink(path))
            elif path.is_file():
                state[str(relative)] = ("f", bool(path.stat().st_mode & 0o100),
                                        hashlib.sha256(path.read_bytes()).hexdigest())
        return state

    def user_state(self, project: pathlib.Path) -> dict[str, Any]:
        """The user's tree plus `.git` index bytes, refs, objects, config, hooks."""
        state: dict[str, Any] = {"tree": self.tree_state(project)}
        git_dir = project / ".git"
        if git_dir.is_dir():
            index = git_dir / "index"
            state["index"] = (hashlib.sha256(index.read_bytes()).hexdigest(), index.stat().st_mtime_ns) \
                if index.exists() else None
            state["head"] = (git_dir / "HEAD").read_bytes()
            state["config"] = (git_dir / "config").read_bytes()
            state["refs"] = sorted(str(p.relative_to(git_dir)) + ":" + p.read_text()
                                   for p in (git_dir / "refs").rglob("*") if p.is_file())
            state["packed"] = (git_dir / "packed-refs").read_bytes() if (git_dir / "packed-refs").exists() else b""
            state["objects"] = sorted(str(p.relative_to(git_dir)) for p in (git_dir / "objects").rglob("*")
                                      if p.is_file())
            state["hooks"] = sorted(p.name for p in (git_dir / "hooks").iterdir()) \
                if (git_dir / "hooks").is_dir() else []
            state["worktrees"] = (git_dir / "worktrees").exists()
        return state

    def assert_user_unchanged(self, project: pathlib.Path, before: dict[str, Any], scenario: str) -> None:
        after = self.user_state(project)
        changed = sorted(key for key in before if before[key] != after.get(key))
        require(not changed, f"{scenario}: the user's {changed} changed (marsh never writes the "
                             "caller's tree or .git; workspaces.md s1, s8)")

    @staticmethod
    def split_root(project: pathlib.Path) -> pathlib.Path:
        return project / ".marsh" / "split"

    def split_ids(self, project: pathlib.Path) -> set[str]:
        root = self.split_root(project)
        return {p.name for p in root.iterdir() if p.is_dir()} if root.is_dir() else set()

    def out(self, project: pathlib.Path, split_id: str) -> pathlib.Path:
        return self.split_root(project) / split_id / "out"

    def handle(self, completed: subprocess.CompletedProcess[bytes] | bytes, what: str) -> str:
        stdout = completed if isinstance(completed, bytes) else completed.stdout
        lines = [line for line in stdout.splitlines() if line.strip()]
        try:
            document = json.loads(lines[-1])
        except (IndexError, json.JSONDecodeError):
            stderr = "" if isinstance(completed, bytes) else text(completed.stderr)
            raise Fail(f"{what}: no handle line {{\"marsh_split\":1,\"id\":...}} on stdout "
                       f"(s2): stdout={text(stdout)[-300:]!r} stderr={stderr[-600:]!r}") from None
        require(isinstance(document, dict) and document.get("marsh_split") == 1
                and isinstance(document.get("id"), str) and set(document) == {"marsh_split", "id"},
                f"{what}: malformed handle {lines[-1]!r}")
        require(re.fullmatch(r"[A-Za-z0-9_-]{1,64}", document["id"]),
                f"{what}: split id is not a safe path component: {document['id']!r}")
        return document["id"]

    @staticmethod
    def handle_line(split_id: str) -> bytes:
        return json.dumps({"marsh_split": 1, "id": split_id}, separators=(",", ":")).encode() + b"\n"

    def splits(self, cwd: pathlib.Path, split_id: str | None = None) -> Any:
        completed = self.cli(["splits", "--json", *([split_id] if split_id else [])], cwd, timeout=30)
        require(completed.returncode == 0, f"`marsh splits --json` failed ({completed.returncode}): "
                                           f"{text(completed.stderr)[-600:]}")
        return json.loads(completed.stdout)

    def split_record(self, cwd: pathlib.Path, split_id: str) -> dict[str, Any]:
        document = self.splits(cwd, split_id)
        records = find_records(document, lambda d: d.get("id") == split_id and "state" in d)
        require(records, f"`marsh splits --json {split_id}` lacks the split: {document!r}"[:800])
        return records[0]

    @staticmethod
    def branch_status(project: pathlib.Path, split_id: str, label: str) -> str:
        path = project / ".marsh" / "split" / split_id / "out" / label / "status"
        require(path.is_file(), f"out/{label}/status missing for split {split_id} (s5)")
        return path.read_text().strip()

    def fixture_job_ids(self) -> set[str]:
        return {summary["job_id"] for summary in self.document("jobs", "--json")["jobs"]
                if summary.get("command") == "fixture"}

    def new_receipts(self, before: set[str], count: int, timeout: float = 120,
                     finished: bool = True, states: tuple[str, ...] = ("finished",)) -> list[dict[str, Any]]:
        deadline = time.monotonic() + timeout
        while True:
            fresh = [self.document("jobs", "show", job, "--json")
                     for job in sorted(self.fixture_job_ids() - before)]
            done = [r for r in fresh if r.get("state") in states] if finished else fresh
            if len(done) >= count and (not finished or len(done) == len(fresh)):
                return fresh
            if time.monotonic() > deadline:
                raise Fail(f"expected {count} {'finished ' if finished else ''}fixture jobs, got "
                           f"{[(r.get('job_id'), r.get('state')) for r in fresh]}")
            time.sleep(0.5)

    def verify_deleted(self, receipts: list[dict[str, Any]]) -> None:
        for receipt in receipts:
            container = receipt.get("container_id")
            require(receipt.get("cleanup") == "verified" and CONTAINER.fullmatch(container or ""),
                    f"Kit container is not verified deleted: {receipt!r}"[:800])
            verify_container_deleted(lambda argv: self.run(argv, timeout=30, check=False),
                                     [self.sbx, "exec", "-u", "root", receipt["vm_id"]], container)


    def fixture_capable(self, *blobs: bytes | str) -> None:
        joined = "".join(b if isinstance(b, str) else text(b) for b in blobs)
        mode = re.search(r"unknown fixture mode: (\S+)", joined)
        if mode:
            raise Fail(f"fixture Kit lacks the `{mode.group(1)}` mode; pass --kit tests/acceptance/fixture "
                       "or republish the fixture from this checkout")


    # ---- receipts and lineage ---------------------------------------------
    def job_ids(self) -> set[str]:
        return {s["job_id"] for s in self.document("jobs", "--json")["jobs"]}

    def show(self, job: str) -> dict[str, Any]:
        return self.document("jobs", "show", job, "--json")

    def settle(self, before: set[str], count: int, *, timeout: float = 120, exact: bool = True,
               verified: bool = True) -> list[dict[str, Any]]:
        """Wait until `count` new jobs are terminal (and cleanup verified); with `exact`, no more appear."""
        deadline = time.monotonic() + timeout
        while True:
            fresh = [self.show(j) for j in sorted(self.job_ids() - before)]
            done = [r for r in fresh if r.get("state") not in ("running", "queued", "submitted")
                    and (not verified or r.get("cleanup") == "verified")]
            if len(fresh) >= count and len(done) == len(fresh):
                break
            if time.monotonic() > deadline:
                raise Fail(f"want {count} settled new jobs, got "
                           f"{[(r.get('command'), r.get('state'), r.get('cleanup')) for r in fresh]}")
            time.sleep(0.5)
        if exact:
            time.sleep(3)
            fresh = [self.show(j) for j in sorted(self.job_ids() - before)]
            require(len(fresh) == count, f"want exactly {count} new jobs, got {len(fresh)}: "
                                         f"{[(r.get('command'), self.lin(r, strict=False)) for r in fresh]}"[:900])
        return fresh

    def running(self, before: set[str]) -> list[dict[str, Any]]:
        return [r for r in (self.show(j) for j in self.job_ids() - before) if r.get("state") == "running"]

    @staticmethod
    def lin(receipt: dict[str, Any], strict: bool = True) -> dict[str, Any]:
        lineage = receipt.get("lineage")
        if strict:
            require(isinstance(lineage, dict) and {"parent", "root", "depth"} <= set(lineage),
                    f"receipt lacks lineage {{parent, root, depth, spawn}} (s9): {receipt!r}"[:800])
        return lineage if isinstance(lineage, dict) else {}

    def kv(self, stdout: bytes) -> dict[str, str]:
        return dict(line.split("=", 1) for line in text(stdout).splitlines() if "=" in line)

    def identity(self, stdout: bytes) -> list[dict[str, Any]]:
        found = []
        for line in text(stdout).splitlines():
            if line.startswith("{\"cwd\""):
                found.append(json.loads(line))
        return found


    @staticmethod
    def pty_expect(master: int, output: bytearray, pattern: str, timeout: float, start: int = 0) -> re.Match[str]:
        """Read (answering cursor queries) until `pattern` appears in output[start:]."""
        deadline = time.monotonic() + timeout
        while True:
            found = re.search(pattern, text(bytes(output[start:])))
            if found:
                return found
            if time.monotonic() > deadline:
                raise Fail(f"terminal never showed {pattern!r}: {text(bytes(output))[-1500:]!r}")
            ready, _, _ = select.select([master], [], [], 0.25)
            if ready:
                try:
                    chunk = os.read(master, 65536)
                except OSError:
                    chunk = b""
                if not chunk:
                    raise Fail(f"terminal closed before {pattern!r}: {text(bytes(output))[-1500:]!r}")
                if b"\x1b[6n" in chunk:
                    os.write(master, b"\x1b[1;1R")
                output.extend(chunk)


    # ---- driver ------------------------------------------------------------
    def blocked(self, reason: str, what: str) -> None:
        for scenario in self.selected:
            self.results.append({"scenario": scenario, "outcome": "blocked", "failure": reason})
            print(f"{self.LABEL}: {scenario}: blocked: {reason}", flush=True)
        raise Fail(f"preflight: {what} unavailable: {reason}")

    def run_cases(self, cases: list[tuple[str, Callable[[], None]]]) -> None:
        """Run each case in order; a failure is recorded and the run continues unless --fail-fast."""
        failures = []
        for label, body in cases:
            began = time.monotonic()
            try:
                body()
                outcome, failure = "passed", None
            except Fail as error:
                outcome, failure = "failed", str(error)
            except Exception as error:  # an unexpected harness/product break is still a failure
                outcome, failure = "failed", f"{type(error).__name__}: {error}"
                self.records.append({"scenario": label, "traceback": traceback.format_exc()})
            finally:
                for process in self.live:
                    self.kill(process)
                self.live.clear()
            self.results.append({"scenario": label, "outcome": outcome, "failure": failure,
                                 "seconds": round(time.monotonic() - began, 1)})
            print(f"{self.LABEL}: {label}: {outcome}" + (f": {failure[:500]}" if failure else ""), flush=True)
            if failure:
                failures.append(label)
                if self.fail_fast:
                    break
        self.document("status", "--json")
        if failures:
            raise Fail(f"{len(failures)}/{len(cases)} scenarios failed: {', '.join(failures)}")

    def run_scenarios(self) -> None:
        self.run_cases([(scenario, getattr(self, self.SCENARIOS[scenario])) for scenario in self.selected])

    def finish(self, error: Exception | None) -> int:
        for process in self.live:
            self.kill(process)
        cleanup_errors = self.scope_cleanup_errors()
        if cleanup_errors:
            cleanup = RuntimeError("; ".join(dict.fromkeys(cleanup_errors)))
            error = cleanup if error is None else RuntimeError(f"{error}; cleanup: {cleanup}")
        destination = self.evidence / self.EVIDENCE
        destination.write_text(json.dumps({
            "outcome": "failed" if error else "passed", "failure": str(error) if error else None,
            "scenarios": self.results, "root": str(self.root), "environment": self.source,
            "records": self.records}, indent=2, default=str) + "\n", encoding="utf-8")
        destination.chmod(0o600)
        passed = sum(r["outcome"] == "passed" for r in self.results)
        print(f"{self.LABEL}: {passed}/{len(self.results)} passed; "
              f"{'failed' if error else 'passed'}; evidence: {destination}")
        if error:
            print(f"{self.LABEL}: {error}")
        return 1 if error else 0


def scenario_parser(description: str, scenarios: dict[str, str], *, kit_help: str,
                    preflight: bool = True) -> argparse.ArgumentParser:
    """The command line every scenario suite shares (see suite_main)."""
    parser = argparse.ArgumentParser(description=description, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--prefix", default=str(pathlib.Path.home() / ".marsh-dev"),
                        help="installed dev product (bin/marsh, libexec/marsh)")
    parser.add_argument("--sbx", default="sbx")
    parser.add_argument("--kit", default=None, help=kit_help)
    parser.add_argument("--evidence", default=None)
    parser.add_argument("--only", action="append", choices=sorted(scenarios), help="repeatable")
    parser.add_argument("--fail-fast", action="store_true", help="stop at the first failed scenario")
    if preflight:
        parser.add_argument("--no-preflight", action="store_true")
    parser.add_argument("--list", action="store_true", help="print scenario ids and exit")
    parser.add_argument("--source-tree", default=str(pathlib.Path(__file__).resolve().parents[2]))
    parser.add_argument("--source-revision", default=None)
    parser.add_argument("--build-receipt", type=pathlib.Path, default=None)
    return parser


def scenario_list(suite: type[ScenarioSuite]) -> list[tuple[str, str]]:
    """(id, first docstring line) in run order: `--list` output and CONTRACT.md's source."""
    return [(scenario, getattr(suite, suite.SCENARIOS[scenario]).__doc__.strip().splitlines()[0])
            for scenario in suite.ORDER]


def suite_main(suite: type[ScenarioSuite], parser: argparse.ArgumentParser) -> int:
    arguments = parser.parse_args()
    if arguments.list:
        for scenario, summary in scenario_list(suite):
            print(f"{scenario}\t{summary}")
        return 0
    if not arguments.kit:
        parser.error("--kit is required")
    prefix = pathlib.Path(arguments.prefix).expanduser().resolve()
    arguments.marsh = str(prefix / "bin" / "marsh")
    arguments.guest_artifacts = prefix / "libexec" / "marsh"
    if not os.access(arguments.marsh, os.X_OK):
        print(f"{suite.LABEL}: no installed product at {arguments.marsh} (run `make dev`)")
        return 1
    kit = pathlib.Path(arguments.kit).expanduser()
    if kit.is_file():
        arguments.kit = json.loads(kit.read_text())["fixture"]
    if arguments.source_revision is None:
        arguments.source_revision = subprocess.run(
            ["git", "-C", arguments.source_tree, "rev-parse", "HEAD"], capture_output=True, text=True,
            timeout=30, check=True).stdout.strip()
    if arguments.evidence is None:
        arguments.evidence = f"/private/tmp/marsh-dev-{suite.LABEL}-{os.getuid()}"
    harness = suite(arguments)
    try:
        harness.run_all()
    except KeyboardInterrupt:
        return harness.finish(InterruptedError(f"{suite.LABEL} acceptance interrupted"))
    except Exception as error:
        return harness.finish(error)
    return harness.finish(None)


class AcpScope(IsolatedScope):
    """An isolated scope whose registry holds only the ACP fixture agent (acp-fixture/)."""

    def __init__(self, args: argparse.Namespace) -> None:
        self.marsh = str(pathlib.Path(args.marsh).resolve(strict=True))
        self.sbx = resolve_executable(args.sbx)
        self.guest_artifacts, self.build_receipt = verify_candidate(args, self.marsh)
        evidence = pathlib.Path(args.evidence).absolute()
        evidence.mkdir(parents=True, mode=0o700, exist_ok=True)
        host_only_path(evidence / "source-binding.json", pathlib.Path(args.source_tree).resolve())
        self.stock_before = stock_baseline(self.sbx)
        self.root = disposable_root("marsh-acp-uat-")
        self.home = self.root / "home"
        self.control_root = self.root / "control"
        self.project = self.root / "project"
        self.home.mkdir()
        self.control_root.mkdir(mode=0o700)
        self.control_home = scoped_control_home(self.control_root, self.home)
        self.project.mkdir()
        kit = stage_acp_fixture(self.root / "kits")
        (self.control_home / "commands.json").write_text(
            json.dumps({"fixture-acp": str(kit)}) + "\n", encoding="utf-8"
        )
        (self.control_home / "agents.json").write_text(
            json.dumps([{
                "schema_version": 1,
                "name": "fixture-session",
                "protocol": "acp_v1",
                "command": "fixture-acp",
                "required_capabilities": [],
            }]) + "\n", encoding="utf-8"
        )
        self.environment = os.environ.copy()
        shell_history = self.project / ".marsh-uat-history"
        shell_history.touch(mode=0o600, exist_ok=False)
        self.environment["HISTFILE"] = str(shell_history)
        self.environment["MARSH_HOME"] = str(self.home)
        self.environment["MARSH_CONTROL_HOME"] = str(self.control_root)
        self.environment["MARSH_SBX"] = self.sbx
        self.environment["MARSH_GUEST_ARTIFACTS"] = str(self.guest_artifacts)
        self.initialize_scope_cleanup()

    def cleanup_isolated_scope(self) -> list[str]:
        errors = []
        # After a deliberate `marsh stop` a status call would start a new daemon.
        if self.scope_started and not getattr(self, "stopped", False):
            try:
                self.public_status()  # include Kits created since the initial snapshot
            except Exception as error:
                errors.append(f"final ownership snapshot unavailable: {error}")
        errors.extend(super().cleanup_isolated_scope())
        try:
            self.stock_after = stock_vm_names(self.sbx)
            errors.extend(self.stock_leftover_errors(self.stock_after))
        except Exception as error:
            errors.append(f"independent stock cleanup inventory unavailable: {error}")
        return errors

    def public_status(self) -> dict:
        self.scope_started = True
        result = subprocess.run(
            [self.marsh, "status", "--json"], cwd=self.project,
            env=self.environment, capture_output=True, timeout=20, check=True, start_new_session=True,
        )
        document = json.loads(result.stdout)
        self.remember_owned_status(document)
        return document
