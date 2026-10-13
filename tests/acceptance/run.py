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
import struct
import subprocess
import sys
import time
import platform
import pwd
from typing import Any, Callable

from harness import (APT_UPDATE_READY, CONTAINER, IsolatedScope, disposable_root, kit_profile_matches, now,
                     resolve_executable, resolve_kit_reference, scoped_control_home, unexpected_failure,
                     verify_container_deleted)
from launch_env import clean_environment
from provenance import (candidate_arguments, candidate_environment, host_only_path, source_identity, verify_candidate,
                        stock_vm_inventory, stock_baseline, stock_cleanup_errors)


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
JOB_LIMITS = {
    "cpu_millis": 500,
    "memory_bytes": 128 * 1024 * 1024,
    "pids": 32,
    "writable_bytes": 16 * 1024 * 1024,
    "output_bytes": 1024 * 1024,
    # Gated containers are held while the harness runs `sbx exec docker inspect`
    # and status calls; beside concurrent suites those take seconds, and a 10 s
    # limit then expired mid-gate. The enforcement check still observes the
    # limit, just at 30 s.
    "wall_seconds": 30,
}
JOB_LIMIT_ENV = {
    "cpu_millis": "MARSH_JOB_CPU_MILLIS",
    "memory_bytes": "MARSH_JOB_MEMORY_BYTES",
    "pids": "MARSH_JOB_PIDS",
    "writable_bytes": "MARSH_JOB_WRITABLE_BYTES",
    "output_bytes": "MARSH_JOB_OUTPUT_BYTES",
    "wall_seconds": "MARSH_JOB_WALL_SECONDS",
}

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


class Harness(IsolatedScope):
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
        self.environment = clean_environment()
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
                cleanup_errors.extend(self.stock_leftover_errors(after))
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
        self.stock_before = stock_baseline(self.sbx)
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
            errors = stock_cleanup_errors(self.stock_before, preserved, owned=set())
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
