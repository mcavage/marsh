#!/usr/bin/env python3
"""Measure the assembled marsh warm registered-command path on stock SBX."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import pathlib
import platform
import shutil
import subprocess
import sys
import tempfile
import time
from typing import Any

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "acceptance"))
from provenance import (candidate_arguments, host_only_path, stock_cleanup_errors,
                        stock_vm_names, verify_candidate)
from run import CONTAINER, IMAGE, verify_container_deleted


PHASES = (
    "vm_prepare",
    "admission",
    "mount_prepare",
    "worker_start",
    "execution",
    "output_drain",
    "result_capture",
    "cleanup",
)


def percentile(values: list[float | int], proportion: float) -> float | int:
    """Return a nearest-rank percentile without external dependencies."""
    if not values:
        raise ValueError("cannot summarize an empty sample")
    ordered = sorted(values)
    index = max(0, math.ceil(proportion * len(ordered)) - 1)
    return ordered[index]


def summary(values: list[float | int]) -> dict[str, float | int]:
    return {
        "p50": percentile(values, 0.50),
        "p95": percentile(values, 0.95),
    }


def executable(value: str) -> str:
    candidate = pathlib.Path(value).expanduser()
    if candidate.parent != pathlib.Path(".") or candidate.is_absolute():
        resolved = candidate.resolve()
        if resolved.is_file() and os.access(resolved, os.X_OK):
            return str(resolved)
        raise ValueError(f"executable is missing or not executable: {resolved}")
    found = shutil.which(value)
    if found is None:
        raise ValueError(f"executable was not found on PATH: {value}")
    return str(pathlib.Path(found).resolve())


def file_digest(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return f"sha256:{digest.hexdigest()}"


class PerfRun:
    def __init__(self, arguments: argparse.Namespace) -> None:
        self.marsh = executable(arguments.marsh)
        self.arguments = arguments
        self.guest_artifacts, self.build_receipt = verify_candidate(arguments, self.marsh)
        self.sbx = executable(arguments.sbx)
        self.samples = arguments.samples
        self.warmups = arguments.warmups
        self.timeout = arguments.timeout
        self.kit_mapping = self.resolve_kit(arguments.kit)
        self.output = (
            host_only_path(pathlib.Path(arguments.output), pathlib.Path(arguments.source_tree))
            if arguments.output
            else None
        )
        if self.output is not None and self.output.exists():
            raise ValueError("choose a fresh performance evidence file")
        self.stock_before = stock_vm_names(self.sbx)
        self.stock_after: dict[str, str] | None = None
        self.stock_commands: list[dict] = []
        self.seen_containers: set[str] = set()
        self.worker_vm: str | None = None
        temporary_parent = "/private/tmp" if sys.platform == "darwin" else None
        self.root = pathlib.Path(
            tempfile.mkdtemp(prefix="marsh-perf-", dir=temporary_parent)
        )
        try:
            self.home = self.root / "home"
            self.project = self.root / "project"
            self.home.mkdir(mode=0o700)
            self.project.mkdir()
            self.control_root = self.root / "control"
            self.control_root.mkdir(mode=0o700)
            key = hashlib.sha256(os.fsencode(self.home.resolve())).hexdigest()
            control = self.control_root / key
            control.mkdir(mode=0o700)
            (control / "commands.json").write_text(
                json.dumps({"fixture": self.kit_mapping}, sort_keys=True) + "\n",
                encoding="utf-8",
            )
        except Exception:
            shutil.rmtree(self.root, ignore_errors=True)
            raise
        self.environment = os.environ.copy()
        self.environment["MARSH_HOME"] = str(self.home)
        self.environment["MARSH_CONTROL_HOME"] = str(self.control_root)
        self.environment["MARSH_SBX"] = self.sbx
        self.environment["MARSH_GUEST_ARTIFACTS"] = str(self.guest_artifacts)
        self.environment["MARSH_PLACE"] = "local"
        self.started = False

    @staticmethod
    def resolve_kit(value: str) -> str:
        if IMAGE.fullmatch(value):
            return value
        raise ValueError("performance qualification requires an immutable Kit OCI reference")

    def stock(self, arguments: list[str]) -> subprocess.CompletedProcess[bytes]:
        result = self.command([self.sbx, *arguments], check=False, timeout=45)
        self.stock_commands.append({"argv": [self.sbx, *arguments], "status": result.returncode,
                                    "stdout": result.stdout.decode(errors="replace"),
                                    "stderr": result.stderr.decode(errors="replace")})
        return result

    def command(
        self,
        arguments: list[str],
        *,
        check: bool = True,
        timeout: float | None = None,
    ) -> subprocess.CompletedProcess[bytes]:
        completed = subprocess.run(
            arguments,
            cwd=self.project,
            env=self.environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout or self.timeout,
            check=False,
        )
        if check and completed.returncode != 0:
            diagnostic = completed.stderr.decode(errors="replace").strip()
            raise RuntimeError(f"{arguments!r} failed ({completed.returncode}): {diagnostic}")
        return completed

    def json_command(self, arguments: list[str]) -> dict[str, Any]:
        completed = self.command(arguments)
        try:
            value = json.loads(completed.stdout)
        except json.JSONDecodeError as error:
            raise RuntimeError(f"{arguments!r} returned invalid JSON") from error
        if not isinstance(value, dict):
            raise RuntimeError(f"{arguments!r} did not return a JSON object")
        return value

    def refresh_ownership(self) -> dict[str, Any]:
        status = self.json_command([self.marsh, "status", "--json"])
        if status.get("schema") != "marsh.status/v1":
            raise RuntimeError("marsh status returned an unsupported schema")
        owner = status.get("endpoint_owner", {})
        if owner.get("uid") != os.getuid() or not isinstance(owner.get("pid"), int):
            raise RuntimeError("isolated daemon endpoint has an unexpected owner")
        return status

    def job_ids(self) -> set[str]:
        document = self.json_command([self.marsh, "jobs", "--json"])
        if document.get("schema") != "marsh.jobs/v1":
            raise RuntimeError("marsh jobs returned an unsupported schema")
        return {
            job["job_id"]
            for job in document.get("jobs", [])
            if isinstance(job.get("job_id"), str)
        }

    def sample(self) -> dict[str, Any]:
        before = self.job_ids()
        event_start = int(time.time()) - 1
        started = time.monotonic_ns()
        completed = self.command([self.marsh, "-c", "fixture identity"])
        elapsed_ms = (time.monotonic_ns() - started) / 1_000_000
        identity = json.loads(completed.stdout)
        if identity.get("cwd") != str(self.project):
            raise RuntimeError("warm fixture did not execute in the exact admitted project")
        if b"[starting " in completed.stderr and b" worker VM" in completed.stderr:
            raise RuntimeError("warm sample unexpectedly cold-booted a worker VM")
        after = self.json_command([self.marsh, "jobs", "--json"])
        candidates = [
            job
            for job in after.get("jobs", [])
            if job.get("job_id") not in before and job.get("command") == "fixture"
        ]
        if len(candidates) != 1:
            raise RuntimeError(f"warm sample produced {len(candidates)} new fixture receipts")
        job_id = candidates[0]["job_id"]
        receipt = self.json_command([self.marsh, "jobs", "show", job_id, "--json"])
        timing = receipt.get("timing", {})
        durations = timing.get("durations_ms", {})
        if (
            receipt.get("schema") != "marsh.job/v1"
            or receipt.get("state") != "finished"
            or receipt.get("cleanup") != "verified"
            or set(durations) != set(PHASES)
        ):
            raise RuntimeError(f"warm sample returned an incomplete receipt: {receipt!r}")
        wall_ms = timing.get("wall_ms")
        orchestration_ms = timing.get("orchestration_ms")
        if (type(wall_ms) is not int or type(orchestration_ms) is not int
                or any(type(value) is not int or value < 0 for value in durations.values())
                or sum(durations.values()) != wall_ms
                or wall_ms - durations["execution"] != orchestration_ms):
            raise RuntimeError("warm sample receipt has invalid timing totals")
        container = receipt.get("container_id")
        vm = receipt.get("vm_id")
        if (not isinstance(container, str) or not CONTAINER.fullmatch(container)
                or container in self.seen_containers or not isinstance(vm, str)
                or not vm or (self.worker_vm is not None and vm != self.worker_vm)):
            raise RuntimeError("warm samples did not reuse one VM with fresh real container identities")
        # Runtime event history proves these fast jobs actually existed. These
        # read-only observations occur after the measured client interval.
        events = self.stock(["exec", vm, "docker", "events", "--since", str(event_start),
                             "--until", str(int(time.time()) + 1), "--filter", f"container={container}",
                             "--format", "{{json .}}"])
        if events.returncode:
            raise RuntimeError("independent stock runtime event inventory failed")
        rows = [json.loads(line) for line in events.stdout.splitlines() if line.strip()]
        matching = [row for row in rows if row.get("Type") == "container"
                    and row.get("Actor", {}).get("ID") == container]
        actions = {row.get("Action") for row in matching}
        if not {"start", "destroy"}.issubset(actions):
            raise RuntimeError("stock runtime did not independently report this container start and destruction")
        verify_container_deleted(self.stock, ["exec", vm], container)
        self.seen_containers.add(container)
        self.worker_vm = vm
        return {
            "end_to_end_ms": round(elapsed_ms, 3),
            "receipt_wall_ms": wall_ms,
            "receipt_orchestration_ms": orchestration_ms,
            "outside_receipt_ms": round(elapsed_ms - wall_ms, 3),
            "durations_ms": {phase: durations[phase] for phase in PHASES},
            "job_id": job_id,
            "container_id": receipt.get("container_id"),
            "vm_id": vm,
            "runtime_lifecycle_events": matching,
            "fixture_identity": identity,
        }

    def run(self) -> dict[str, Any]:
        self.started = True
        prewarm = self.command(
            [self.marsh, "--load", "fixture", "-c", "true"],
            timeout=max(self.timeout, 300),
        )
        status = self.refresh_ownership()
        if not any(worker.get("warm") is True for worker in status.get("workers", [])):
            raise RuntimeError("prewarm did not expose a warm worker")
        for _ in range(self.warmups):
            self.sample()
        samples = [self.sample() for _ in range(self.samples)]
        self.refresh_ownership()
        metrics = {
            name: summary([sample[name] for sample in samples])
            for name in (
                "end_to_end_ms",
                "receipt_wall_ms",
                "receipt_orchestration_ms",
                "outside_receipt_ms",
            )
        }
        phase_summaries = {
            phase: summary([sample["durations_ms"][phase] for sample in samples])
            for phase in PHASES
        }
        sbx_version = self.command([self.sbx, "version"], check=False, timeout=20)
        verify_candidate(self.arguments, self.marsh)
        return {
            "schema": "marsh.perf/v1",
            "environment": {
                "platform": platform.platform(),
                "machine": platform.machine(),
                "python": platform.python_version(),
                "marsh": self.marsh,
                "marsh_sha256": file_digest(pathlib.Path(self.marsh)),
                **self.build_receipt["source_before"],
                "verified_build_receipt": self.build_receipt,
                "sbx": self.sbx,
                "sbx_version": (sbx_version.stdout + sbx_version.stderr)
                .decode(errors="replace")
                .strip(),
                "kit": self.kit_mapping,
                "samples": self.samples,
                "discarded_warmups": self.warmups,
                "prewarm_stderr": prewarm.stderr.decode(errors="replace"),
            },
            "raw_samples": samples,
            "summaries_ms": metrics,
            "receipt_phase_summaries_ms": phase_summaries,
        }

    def cleanup(self) -> list[str]:
        errors = []
        if self.started:
            try:
                stopped = self.command(
                    [self.marsh, "stop", "--json"],
                    check=False, timeout=max(self.timeout, 180),
                )
            except (OSError, subprocess.TimeoutExpired) as error:
                errors.append(f"isolated marsh stop unavailable; retained {self.root}: {error}")
            else:
                if stopped.returncode != 0:
                    errors.append("isolated marsh stop failed: " + stopped.stderr.decode(errors="replace").strip())
                else:
                    try:
                        report = json.loads(stopped.stdout)
                        if report.get("cleanup_complete") is not True:
                            errors.append(f"isolated scope cleanup incomplete: {report!r}")
                    except json.JSONDecodeError:
                        errors.append("isolated marsh stop returned invalid JSON")
        try:
            self.stock_after = stock_vm_names(self.sbx)
            errors.extend(stock_cleanup_errors(self.stock_before, self.stock_after))
            verify_candidate(self.arguments, self.marsh)
        except Exception as error:
            errors.append(f"final independent verification failed: {error}")
        if errors:
            return errors
        try:
            shutil.rmtree(self.root)
        except OSError as error:
            return [f"could not remove isolated root {self.root}: {error}"]
        return []

    def publish(self, result: dict[str, Any]) -> None:
        result["stock_before"] = self.stock_before
        result["stock_after"] = self.stock_after
        result["stock_commands"] = self.stock_commands
        rendered = json.dumps(result, indent=2, sort_keys=True) + "\n"
        if self.output is not None:
            self.output.write_text(rendered, encoding="utf-8")
            self.output.chmod(0o600)
        sys.stdout.write(rendered)


def positive(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be greater than zero")
    return parsed


def nonnegative(value: str) -> int:
    parsed = int(value)
    if parsed < 0:
        raise argparse.ArgumentTypeError("must be zero or greater")
    return parsed


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    candidate_arguments(parser)
    parser.add_argument("--source-tree", required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--marsh", required=True, help="assembled release marsh binary")
    parser.add_argument("--sbx", default="sbx", help="stock SBX executable")
    parser.add_argument(
        "--kit", required=True, help="native v3 source directory or immutable OCI digest"
    )
    parser.add_argument("--samples", type=positive, default=30)
    parser.add_argument("--warmups", type=nonnegative, default=5)
    parser.add_argument(
        "--timeout", type=positive, default=120, help="per-command timeout in seconds"
    )
    parser.add_argument("--output", help="also write the JSON report to this path")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    run: PerfRun | None = None
    result: dict[str, Any]
    status = 0
    try:
        run = PerfRun(parse_args(argv))
        result = run.run()
    except KeyboardInterrupt:
        result = {"schema": "marsh.perf/v1", "error": "interrupted"}
        status = 130
    except Exception as error:
        result = {"schema": "marsh.perf/v1", "error": str(error)}
        status = 1
    cleanup_errors = run.cleanup() if run is not None else []
    if cleanup_errors:
        result["cleanup_errors"] = cleanup_errors
        status = 1
    result["outcome"] = "passed" if status == 0 else "failed"
    if run is not None:
        run.publish(result)
    else:
        print(json.dumps(result, indent=2, sort_keys=True))
    return status


if __name__ == "__main__":
    raise SystemExit(main())
