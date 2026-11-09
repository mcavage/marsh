#!/usr/bin/env python3
"""Services E2E harness core (TEST-PLAN A.3-A.6, B, C.0): the scenario catalogue, an isolated scope with its own
VM prefix, the VM lock, cleanup scoped to this run with a leak check, the independent observer, holds, drivers,
timing, per-scenario evidence and the break guards and protocol. Scenarios beyond HARNESS-01/02/03 are registered
`pending` until the tasks that gate them implement them.

    services_uat.py --list | --list-breaks
    services_uat.py --tier smoke|full|perf|soak [--only ID]... [--driver internal|product]
    services_uat.py --break MODE[,MODE] --only ID   one break run: with a defect, its verification (pass 3);
                                                    holds/faults only, the pass-2 baseline on the breaks build
    services_uat.py --tier break [--only ID]...     the three-pass protocol for every cell of the scenario
    services_uat.py --gc [--dry-run]                remove resources of dead runs (takes the VM lock)
"""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import json
import os
import pathlib
import re
import secrets
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import traceback
from typing import Any, Callable

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))
sys.path.insert(0, str(HERE))

import breaks  # noqa: E402
from breaks import (HARNESS_BREAKS, agent_break_error, expected_checks, guard_error, infer_defect,  # noqa: E402
                    list_breaks_text, marsh_modes, normalize_modes, prerequisites, verdict)
from catalogue import BY_ID, CATALOGUE, NO_AGENT_SCENARIOS, Entry  # noqa: E402
from drivers import DRIVERS, make_driver  # noqa: E402
from evidence import SCHEMA_ID, Recorder, Series, Stopwatch, load_synthetic_secrets, validate_evidence  # noqa: E402
from harness import (Fail, ScenarioSuite, now, resolve_arguments, run_harness, scenario_parser,  # noqa: E402
                     stable_process_identity, text)
from holds import HoldPoints  # noqa: E402
from nfx_gc import RunRecord, collect, load_records, claimed, runs_dir  # noqa: E402
from nfx_host import (DISK_SETTING, REMOVED, VM_PREFIX, PolicyRecorder, baseline_oracle, foreign_names,  # noqa: E402
                      host_facts, is_mine, policy_rules, read_setting, remove_sandbox, setting_errors)
import nfx_lock  # noqa: E402
from nfx_lock import EXIT_LOCK_HELD, LockHeld, LockUnavailable  # noqa: E402
from observer import Observer, Pending  # noqa: E402
from provenance import SharedBaseline, stock_baseline, stock_vm_inventory  # noqa: E402
from scenarios_harness import Blocked, CheckFailed, HarnessScenarios  # noqa: E402
from scopes import OwnedLeftovers, SideScope, launch_daemon, map_document, map_uuid, reap  # noqa: E402

TIERS = ("smoke", "full", "perf", "soak", "break")
MIN_FREE_BYTES = 60 * 1024**3
FIXTURES = HERE / "fixtures"
# Task each blocked scenario waits for. Remove the row when the task merges. HARNESS-02 needs H2's launch path,
# its `check-no-test-breaks.sh` and `tests/breaks/guest_env.sh`; HARNESS-03 needs H1b's `fixtures/harness03.py`.
BLOCKED_ON: dict[str, str] = {"HARNESS-02": "H2"}
if not (FIXTURES / "harness03.py").is_file():
    BLOCKED_ON["HARNESS-03"] = "H1b"
IMPLEMENTED = {"HARNESS-01": "harness_01", "HARNESS-02": "harness_02", "HARNESS-03": "harness_03"}
SCENARIOS = {entry.id: IMPLEMENTED.get(entry.id, "pending_scenario") for entry in CATALOGUE}
ORDER = [entry.id for entry in CATALOGUE]


def new_run_id() -> str:
    return time.strftime("%y%m%d%H%M%S") + secrets.token_hex(2)


def select(tier: str, only: list[str] | None) -> list[Entry]:
    if only:
        return [entry for entry in CATALOGUE if entry.id in only]
    if tier == "smoke":
        return [e for e in CATALOGUE if e.smoke and e.runner == "marsh"]
    if tier == "full":
        return [e for e in CATALOGUE if e.tier in ("smoke", "full") and e.runner == "marsh"]
    return [e for e in CATALOGUE if e.tier == tier and e.runner == "marsh"]


def status_of(entry: Entry) -> str:
    if entry.runner == "narwhal" or SCENARIOS[entry.id] == "pending_scenario":
        return "pending"
    return f"blocked-on:{BLOCKED_ON[entry.id]}" if entry.id in BLOCKED_ON else "implemented"


class Services(HarnessScenarios, OwnedLeftovers, ScenarioSuite):
    LABEL = "services"
    EVIDENCE = "services.json"
    SCENARIOS = SCENARIOS
    ORDER = ORDER

    def __init__(self, arguments: argparse.Namespace) -> None:
        self.run_id = arguments.run
        self.args = arguments
        self.tier = arguments.tier
        self.driver_name = arguments.driver
        super().__init__(arguments)
        modes = normalize_modes(arguments.break_modes)
        self.requested = modes
        self.marsh_requested = marsh_modes(modes)
        self.harness_breaks = [m for m in modes if m in HARNESS_BREAKS]
        self.defect = arguments.break_defect or infer_defect(modes)
        self.breaks_build = arguments.build == "breaks"
        self.variant = arguments.variant
        self.leak_on_purpose = "leak-on-purpose" in self.harness_breaks
        for environment in (self.environment, self.host_env):
            environment["MARSH_VM_PREFIX"] = VM_PREFIX
            environment.pop("MARSH_TEST_BREAK", None)  # only an explicitly launched daemon gets it
        self.evidence_base = self.evidence
        self.evidence = self.evidence_base / self.run_id
        self.evidence.mkdir(mode=0o700, exist_ok=True)
        self.entries = select(self.tier, arguments.only)
        self.explicit = bool(arguments.only)
        self.recorder = Recorder()
        self.notes: list[str] = []
        self.fixtures = pathlib.Path(arguments.fixtures_dir)
        self.load_redaction()
        self.driver = make_driver(self.driver_name, self)
        self.observer = Observer(self.sbx, self._observe, self.recorder, self.observed_names, self.listed_names,
                                 uuid_of=self.uuid_of, owned_uuid=self.map_uuid_of,
                                 engine_generation=self.engine_generation)
        self.holds = HoldPoints(self.home, self.marsh_requested, self._agent_exec)
        self.probe_name = f"nfx-{self.run_id}-probe"
        self.raw_ids: dict[str, str] = {}
        self.side_scopes: list[SideScope] = []
        self.policy_before: dict[str, dict[str, Any]] = {}
        self.setting_before: dict[str, Any] = {}
        self.policy = PolicyRecorder(self.sbx, None)
        self.record: RunRecord | None = None
        self.active_breaks: dict[str, Any] = {"daemon": [], "agent": None}
        self.versions: dict[str, Any] = {}
        self.preamble: list[dict[str, Any]] = []
        self.foreign: list[str] = []
        self.current: Entry | None = None
        self.lock: dict[str, Any] | None = getattr(arguments, "lock", None)  # who holds the VM lock for this run
        self.outcomes: list[dict[str, Any]] = []
        self.break_failures: list[str] = []
        self.daemon_process: subprocess.Popen[bytes] | None = None
        self.features: list[str] = []
        self.daemon_pid: int | None = None
        self.tolerate = bool(arguments.tolerate_baseline_changes)
        self.tolerated: list[str] = []  # baseline changes accepted without proof: the result is non-qualifying
        self.agent_attested = False

    # ---- fixtures: redaction ----------------------------------------------------
    def load_redaction(self) -> None:
        """Fixture-defined synthetic-secret patterns (H1b's `redaction.json`, or `synthetic-secrets.json`)."""
        for name in ("synthetic-secrets.json", "redaction.json"):
            path = self.fixtures / name
            if path.is_file():
                literals, patterns = load_synthetic_secrets(path)
                for literal in literals:
                    self.recorder.add_secret(literal)
                for regex, replacement in patterns:
                    self.recorder.add_pattern(regex, replacement)
                self.notes.append(f"redaction patterns from {path.name}: {len(patterns)}")
                return

    # ---- scope naming (H-1) --------------------------------------------------
    def make_root(self) -> pathlib.Path:
        parent = pathlib.Path(os.environ.get("MARSH_UAT_ROOT") or "/private/tmp").resolve()
        if not parent.is_dir():
            raise ValueError(f"system temporary directory is unavailable: {parent}")
        root = parent / f"nfx-{self.run_id}"
        root.mkdir(mode=0o700)
        if root.resolve().is_relative_to(pathlib.Path.home().resolve()):
            shutil.rmtree(root)
            raise ValueError(f"disposable root unexpectedly falls under the user home: {root}")
        return root.resolve()

    # ---- commands and observation (H-7, H-9, H-10) -------------------------
    def _exec(self, argv: list[str], timeout: float = 120, *, cwd: pathlib.Path | str | None = None,
              env: dict[str, str] | None = None, stdin: bytes = b"") -> tuple[int, bytes, bytes, int, int]:
        self.scope_started = self.scope_started or argv[0] == self.marsh
        started = time.monotonic_ns()
        process = subprocess.Popen(argv, cwd=cwd or self.project, env=env or self.host_env, stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, process_group=0)
        try:
            out, err = process.communicate(input=stdin, timeout=timeout)
        except subprocess.TimeoutExpired:
            self.kill(process)
            elapsed = time.monotonic_ns() - started
            self.recorder.commands.append(self.recorder.command(argv, cwd, None, started, elapsed, b"", b"",
                                                                note=f"timed out after {timeout}s"))
            raise Fail(f"timed out after {timeout}s: {' '.join(argv)[:300]}") from None
        finally:
            self.kill(process)
        return process.returncode, out, err, started, time.monotonic_ns() - started

    def xrun(self, argv: list[str], *, timeout: float = 120, cwd: pathlib.Path | str | None = None,
             env: dict[str, str] | None = None, stdin: bytes = b"", check: bool = False,
             note: str | None = None, sink: list[dict[str, Any]] | None = None) -> subprocess.CompletedProcess[bytes]:
        """Run ARGV, recording argv, exit, monotonic_ns timing and byte-exact output (a public-interface call)."""
        code, out, err, started, elapsed = self._exec(argv, timeout, cwd=cwd, env=env, stdin=stdin)
        extra = {"note": note} if note else {}
        (self.recorder.commands if sink is None else sink).append(
            self.recorder.command(argv, cwd or self.project, code, started, elapsed, out, err, **extra))
        if check and code != 0:
            raise Fail(f"{' '.join(argv)[:200]} failed ({code}): {text(err)[-400:]}")
        return subprocess.CompletedProcess(argv, code, out, err)

    def _observe(self, argv: list[str], timeout: float) -> tuple[int, bytes, bytes, int, int]:
        return self._exec(argv, timeout, cwd=self.root)

    def popen(self, argv: list[str]) -> "subprocess.Popen[bytes]":
        """A long-lived helper (the event watcher), killed with the scenario."""
        process = subprocess.Popen(argv, cwd=self.root, env=self.host_env, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, process_group=0)
        self.live.append(process)
        return process

    def _agent_exec(self, command: str, write: bool) -> tuple[int, str]:
        done = self.observer.vx(command, engine="prod", write=write)
        return done.returncode, done.stdout.decode(errors="replace")

    def listed_names(self) -> set[str]:
        return set(stock_vm_inventory(self.sbx, env=self.environment))

    def observed_names(self) -> set[str]:
        return self.owned_stock_names() | set(self.raw_ids)

    def uuid_of(self, name: str) -> str | None:
        return stock_vm_inventory(self.sbx, env=self.environment).get(name)

    def map_uuid_of(self, name: str) -> str | None:
        return map_uuid(self.control_home, name)

    def engine_generation(self, svc: str) -> str | None:
        """`generation.engine` of the project whose VM is SVC, from `marsh services status --json` (None when the
        product cannot say, which makes every production-engine probe refuse)."""
        done = subprocess.run([self.marsh, "services", "status", "--json"], cwd=self.project, env=self.environment,
                              stdin=subprocess.DEVNULL, capture_output=True, timeout=60, check=False)
        if done.returncode != 0:
            return None
        try:
            projects = json.loads(done.stdout).get("projects") or []
        except ValueError:
            return None
        for project in projects:
            if project.get("vm") == svc:
                engine = (project.get("generation") or {}).get("engine")
                return engine if isinstance(engine, str) and engine else None
        return None

    def check(self, check: str, condition: object, detail: str = "") -> None:
        self.recorder.checks.append({"id": check, "outcome": "passed" if condition else "failed",
                                     "detail": str(detail)[:2000] if not condition else "ok"})
        if not condition:
            raise CheckFailed(check, str(detail))

    def skip_check(self, check: str, why: str) -> None:
        self.recorder.checks.append({"id": check, "outcome": "skipped", "detail": why[:2000]})

    def collect(self, series: Series, attempt: Any) -> Series:
        """H-9: run ATTEMPT until the series has N retained samples (N+3 with warm-ups). ATTEMPT returns None, or
        raises Fail for a failed attempt, or returns "gapped". Every attempt's monotonic_ns is recorded."""
        while not series.complete():
            with Stopwatch() as watch:
                try:
                    result, ok = attempt(), True
                except Fail:
                    result, ok = None, False
            series.add(watch.ns, ok=ok, gapped=result == "gapped")
        series.record(self.recorder)
        return series

    # ---- host state (H-3) -------------------------------------------------
    def preflight(self) -> None:
        free = shutil.disk_usage("/private/tmp").free
        if free < MIN_FREE_BYTES and not self.args.no_disk_check:
            raise Fail(f"preflight: {free / 1024**3:.0f} GB free, a services run needs {MIN_FREE_BYTES // 1024**3}")

    def take_baselines(self) -> None:
        self.preflight()
        self.stock_before = SharedBaseline(stock_baseline(self.sbx))
        self.source["stock_before"] = dict(self.stock_before)
        self.policy_before = policy_rules(self.sbx, self.environment)
        self.setting_before = read_setting(self.sbx, DISK_SETTING, self.environment)
        self.record = RunRecord(runs_dir(nfx_lock.lock_path()), self.run_id, self.tier, self.root,
                                self.control_home / "vm-ownership.json")
        self.record.save(exclusive=True)  # a run ID is never reused: an existing record is somebody's evidence
        self.foreign = foreign_names(dict(self.stock_before), self.stock_before, self.run_id, set())
        facts = host_facts(self.sbx, self.environment)
        self.versions = {
            "marsh": {"revision": self.source.get("source_revision"), "dirty": self.source.get("source_dirty"),
                      "binary": self.marsh, "build": "breaks" if self.marsh_requested or self.breaks_build
                      else "normal",
                      "binary_sha256": hashlib.sha256(pathlib.Path(self.marsh).read_bytes()).hexdigest()},
            "sbx": facts["sbx_version"], "macos": facts["macos"], "ram_bytes": facts["ram_bytes"],
            "free_disk_bytes": facts["free_disk_bytes"], "free_disk_end_bytes": facts["free_disk_bytes"],
            "host_load_start": facts["load"], "host_load_end": facts["load"],
            "services_engine": None, "kit": self.kit}

    def sync_record(self) -> None:
        """Persist the ownership map's names and UUIDs into the run record (a collector needs them after the root is
        gone)."""
        if self.record is not None:
            self.record.sync_map(map_document(self.control_home))

    def create_sandbox(self, name: str, workspace: pathlib.Path, *, record: RunRecord | None = None,
                       agent: str = "shell", note: str | None = None) -> subprocess.CompletedProcess[bytes]:
        """Create a sandbox this run will be allowed to remove. The name must be free (a collision is refused, never
        adopted); the intent is persisted before the effect; the stable ID is recorded only after `sbx create`
        succeeded and the sandbox exists. A failed or ambiguous create (the name appeared though create failed) is
        never adopted: it raises, leaves the sandbox in place and keeps the intent in the record for the collector.
        RECORD is the run record that owns it (this run's by default)."""
        record = record or self.record
        before = stock_vm_inventory(self.sbx, env=self.environment)
        if name in before:
            raise Fail(f"refusing to create {name}: a sandbox of that name already exists ({before[name]}); "
                       "a name collision is never adopted")
        if record is not None:
            record.intend(name)
        done = self.xrun([self.sbx, "create", "--name", name, agent, str(workspace)], timeout=600, note=note)
        after = stock_vm_inventory(self.sbx, env=self.environment)
        if done.returncode != 0:
            detail = " (and a sandbox of that name now exists: ambiguous, left in place)" if name in after else ""
            raise Fail(f"sbx create {name} failed ({done.returncode}): {text(done.stderr)[-300:]!r}{detail}")
        if name not in after:
            raise Fail(f"sbx create {name} succeeded but the sandbox is not in `sbx ls`: not adopting anything")
        if record is not None:
            record.identify(name, after[name])
        if record is self.record:
            self.raw_ids[name] = after[name]
        return done

    def policy_add(self, *argv: str) -> subprocess.CompletedProcess[bytes]:
        """`sbx policy allow|deny network ...` through the recorder: the rule ID this call created is recorded for
        exact rollback."""
        done = self.policy.add(*argv)
        if self.record is not None:
            self.record.policy(self.policy.rules)
        return done

    def mine(self, name: str) -> bool:
        return is_mine(name, self.run_id, self.owned_stock_names())

    def proven_uuid(self, name: str) -> str | None:
        """The stable ID that proves NAME is this run's: the one recorded at creation, else the ownership map's."""
        return self.raw_ids.get(name) or self.map_uuid_of(name)

    def other_claims(self) -> dict[str, str | None]:
        """Names other runs' records list, with the stable ID each recorded (None: an intent without an ID). A new VM
        under such a name is foreign only if its live ID equals the recorded one; the oracle checks that."""
        found: dict[str, str | None] = {}
        for run, record in load_records(runs_dir(nfx_lock.lock_path())).items():
            if run != self.run_id:
                found.update(claimed(record))
        return found

    def h3_cleanup(self) -> list[str]:
        """Remove what this run created on stock SBX, proven by name AND recorded stable ID: its `nfx-<run>-*`
        sandboxes (ID recorded at creation) and the VMs of its ownership map (UUID in the map). A name without proof,
        or whose live ID differs from the proof (an impostor), is left in place and reported. Policy rules go by
        recorded ID. The global `sandbox.disk.dockerVolume` setting is never written, restored or unset here (r3.3 delta 22):
        `h3_verify` only observes it. Foreign resources are never touched."""
        errors: list[str] = []
        inventory = stock_vm_inventory(self.sbx, env=self.environment)
        for name in sorted(inventory):
            if name in self.stock_before or not self.mine(name):
                continue
            if self.leak_on_purpose and name == self.probe_name:
                continue
            proof = self.proven_uuid(name)
            if proof is None:
                errors.append(f"{name} looks like this run's but no recorded ID proves it; left in place")
                continue
            try:
                remove_sandbox(self.sbx, name, proof, self.stock_before, env=self.environment)
            except (OSError, ValueError, subprocess.SubprocessError) as error:
                errors.append(f"could not remove {name}: {error}")
        errors += self.policy.rollback()
        return errors

    def h3_verify(self) -> list[str]:
        """The baseline-equality and leak oracle (`baseline_oracle`), plus the global setting. Lists foreign
        resources; unproven changes fail the run unless --tolerate-baseline-changes accepts them (then noted)."""
        after = stock_vm_inventory(self.sbx, env=self.environment)
        owned = self.owned_stock_names()
        errors, notes = baseline_oracle(self.stock_before, after, self.run_id, owned, self.other_claims(), REMOVED,
                                        self.policy_before, policy_rules(self.sbx, self.environment), self.tolerate)
        self.notes.extend(n for n in notes if n not in self.notes)
        self.tolerated = sorted(set(self.tolerated) | {n for n in notes if "[tolerated]" in n})
        self.foreign = foreign_names(after, self.stock_before, self.run_id, owned)
        return errors + setting_errors(self.sbx, self.setting_before, self.environment)

    def stop_scope(self) -> None:
        """`marsh stop` for the scope while its daemon lives (the base cleanup then verifies and removes the rest)."""
        pid = self.owned_daemon_pid
        if not self.scope_started or pid is None:
            return
        try:
            if stable_process_identity(pid) is None:
                return
            self.refresh_owned_status_before_cleanup()
            done = subprocess.run([self.marsh, "stop", "--json"], cwd=self.project, env=self.environment,
                                  stdin=subprocess.DEVNULL, capture_output=True, timeout=300, check=False)
            self.notes.append(f"marsh stop exited {done.returncode}")
        except Exception as error:  # the authenticated shutdown and VM removal below still run
            self.notes.append(f"marsh stop could not run: {error}")

    def scope_cleanup_errors(self) -> list[str]:
        errors: list[str] = []
        try:
            self.sync_record()
            if self.daemon_process is not None and self.owned_daemon_pid is None:
                reap(self.daemon_process)  # launched, never authenticated: nothing to clean up by, kill what we spawned
                self.scope_started = False
            for side in list(self.side_scopes):
                side.abort_unowned_daemon()
                errors += side.cleanup_isolated_scope()
            errors += self.h3_cleanup()
            errors += super().scope_cleanup_errors()
            errors += self.h3_verify()
        except Exception as error:
            errors.append(f"cleanup failed: {error}")
        for process in (self.daemon_process, *(s.daemon for s in self.side_scopes)):
            if process is not None:
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    errors.append(f"the explicitly launched daemon {process.pid} did not exit")
        return errors

    # ---- break guards (A.6) ---------------------------------------------------
    def guard(self) -> None:
        """Every break, fault and hold run (and a breaks-build pass 2 even with no modes) launches its daemon
        explicitly. The daemon that answers `marsh status --json` must be that process; the field must be exactly
        the requested modes (present and equal), or entirely absent for a normal run."""
        explicit = bool(self.marsh_requested) or self.breaks_build
        if explicit:
            marshd = pathlib.Path(self.marsh).with_name("marshd")
            self.daemon_process = launch_daemon(self, marshd, self.marsh_requested, self.root / "marshd-explicit.log")
            self.preamble.append({"launch": [str(marshd), "--home", str(self.home), "--test-breaks-from-env"],
                                  "MARSH_TEST_BREAK": self.marsh_requested, "pid": self.daemon_process.pid})
        done = self.xrun([self.marsh, "status", "--json"], timeout=60, sink=self.preamble, check=True)
        status = json.loads(done.stdout)
        self.remember_owned_status(status)
        self.daemon_pid = (status.get("endpoint_owner") or {}).get("pid")
        if explicit and self.daemon_pid != self.daemon_process.pid:
            raise Fail(f"the daemon answering status (pid {self.daemon_pid}) is not the one launched explicitly "
                       f"(pid {self.daemon_process.pid})")
        problem = guard_error(status, self.marsh_requested)
        if problem:
            raise Fail(problem)
        self.active_breaks = {"daemon": list(status.get("test_breaks") or []), "agent": None}
        self.features = list(status.get("features") or [])

    def agent_projects(self) -> list[dict[str, Any]] | None:
        """`marsh services status --json` projects, or None when the product cannot answer (never an empty list)."""
        done = subprocess.run([self.marsh, "services", "status", "--json"], cwd=self.project, env=self.environment,
                              stdin=subprocess.DEVNULL, capture_output=True, timeout=60, check=False)
        if done.returncode != 0:
            return None
        try:
            projects = json.loads(done.stdout).get("projects")
        except ValueError:
            return None
        return list(projects) if isinstance(projects, list) else None

    def attest_agent(self) -> None:
        """From wave 1, a scenario of a break run calls this right after it starts its services VM and BEFORE the
        operation the defect changes: every ready project must report `test_breaks_agent` equal to the requested set
        (present and exact, or absent when none was requested). Unavailable or malformed status, or no ready project,
        fails closed."""
        projects = self.agent_projects()
        if projects is None:
            raise Fail("agent attestation unavailable: `marsh services status --json` did not answer with projects")
        ready = [p for p in projects if p.get("state") == "ready"]
        if not ready:
            raise Fail("agent attestation impossible: no services project is ready")
        for project in ready:
            problem = agent_break_error(project, self.marsh_requested)
            if problem:
                raise Fail(problem)
        self.active_breaks["agent"] = list(ready[0].get("test_breaks_agent") or [])
        self.agent_attested = True

    def agent_gate(self, entry: Entry) -> str | None:
        """The agent claim every non-HARNESS scenario must back, on every run and whatever the outcome (normal, zero-mode
        breaks build, defect): attested (`test_breaks_agent` exactly the requested set, absent for none) or proved
        absent. None when it holds; otherwise why not."""
        if entry.id.startswith("HARNESS"):
            return None  # HARNESS-02 c6 makes this claim itself, for the daemon it launches
        try:
            self.prove_no_agent() if entry.id in NO_AGENT_SCENARIOS else self.check_agent_breaks()
        except Fail as error:
            return str(error)
        return None

    def prove_no_agent(self) -> None:
        """A scenario that needs no services VM (NO_AGENT_SCENARIOS) proves that no agent exists instead of attesting
        one: from wave 1 the isolated scope's `marsh services status --json` must answer with no project at all."""
        if "services-v1" not in self.features:
            return
        projects = self.agent_projects()
        if projects is None:
            raise Fail("cannot prove that no services agent exists: `marsh services status --json` did not answer")
        if projects:
            raise Fail(f"this scenario needs no services VM, but the scope reports projects "
                       f"{[p.get('state') for p in projects]}: an agent exists that no attestation covered")

    def check_agent_breaks(self) -> None:
        """After every scenario of every run, whatever its outcome and whether or not modes were requested (the empty
        set is the claim `test_breaks_agent` is absent): the attestation must have happened (it gates the defect's
        operation) and must still hold now. A named check failure never turns a missing attestation into a verified
        break; unavailable or malformed status fails closed."""
        if "services-v1" not in self.features:
            return
        if not self.agent_attested:
            raise Fail("the scenario never attested the services agent's modes before the defect's operation")
        self.attest_agent()

    # ---- the run -----------------------------------------------------------------
    def run_all(self) -> None:
        self.take_baselines()
        if any(e.id == "HARNESS-03" for e in self.entries) and (self.fixtures / "harness03.py").is_file():
            self.prepare_nfx_kits()
        try:
            self.guard()
        except Fail as error:
            for entry in self.entries:
                self.recorder.reset()
                self.write_evidence(entry.id, self.evidence_document(entry, "blocked", str(error), None, None,
                                                                     now(), 0))
            raise
        for entry in self.entries:
            self.run_entry(entry)

    def run_entry(self, entry: Entry) -> None:
        self.current = entry
        self.recorder.reset()
        self.live.clear()
        started_at, began = now(), time.monotonic()
        outcome, failure, failed_check, blocked_on = "passed", None, None, None
        skip = None
        if entry.runner == "narwhal":
            skip = "pending", "runs in Narwhal's Swift journey tests, not here"
        elif entry.id in BLOCKED_ON and not self.explicit:
            skip = "blocked", f"blocked-on:{BLOCKED_ON[entry.id]}"
            blocked_on = BLOCKED_ON[entry.id]
        if skip:
            outcome, failure = skip
        else:
            try:
                self.agent_attested = False
                getattr(self, SCENARIOS[entry.id])()
            except Pending as error:
                outcome, failure = "pending", str(error)
            except Blocked as error:
                outcome, failure = "blocked", str(error)
                found = re.match(r"blocked-on:(\w+)", str(error))
                blocked_on = found.group(1) if found else None
            except CheckFailed as error:
                outcome, failure, failed_check = "failed", str(error), error.check
            except Fail as error:
                outcome, failure = "failed", str(error)
            except Exception as error:
                outcome, failure = "failed", f"{type(error).__name__}: {error}"
                self.records.append({"scenario": entry.id, "traceback": traceback.format_exc()})
            finally:
                for process in self.live:
                    self.kill(process)
                self.live.clear()
                self.sync_record()
            if outcome in ("passed", "failed"):
                problem = self.agent_gate(entry)
                if problem:  # fail closed, on every path, including a named check failure
                    outcome, failed_check = "failed", None
                    failure = f"agent attestation: {problem}" + (f" (the scenario had failed: {failure})" if failure else "")
        document = self.evidence_document(entry, outcome, failure, failed_check, blocked_on, started_at,
                                          round((time.monotonic() - began) * 1000))
        self.write_evidence(entry.id, document)
        kind = document["break"]["verdict"]
        executing = self.defect or self.requested or self.breaks_build or self.tier == "break" \
            or self.args.require_executed
        failing = (kind in ("invalid-negative-control", "not-detected", "baseline-failed")
                   or (kind == "normal" and outcome == "failed") or (outcome == "blocked" and not skip)
                   or (outcome == "pending" and not skip and executing))
        if failing and not (skip and outcome == "blocked"):
            self.break_failures.append(entry.id)
        self.outcomes.append({"scenario": entry.id, "outcome": outcome, "failed_check": failed_check,
                              "failure": failure, "verdict": kind, "skipped": bool(skip),
                              "seconds": round(time.monotonic() - began, 1)})
        print(f"{self.LABEL}: {entry.id}: {outcome}" + (f" at {failed_check}" if failed_check else "")
              + (f" [{kind}]" if kind != "normal" else "") + (f": {failure[:300]}" if failure else ""), flush=True)
        if self.defect and not skip:
            print(f"{self.LABEL}: break verdict {entry.id} {self.defect}: {kind}", flush=True)

    # ---- evidence ----------------------------------------------------------------
    def evidence_document(self, entry: Entry, outcome: str, failure: str | None, failed_check: str | None,
                          blocked_on: str | None, started_at: str, duration_ms: int) -> dict[str, Any]:
        versions = {**self.versions, "free_disk_end_bytes": shutil.disk_usage("/private/tmp").free,
                    "host_load_end": host_facts(self.sbx, self.environment)["load"]}
        expected = expected_checks(entry, self.defect, self.variant)
        run_verdict = verdict(self.defect, self.marsh_requested, outcome, failed_check, expected,
                              breaks_build=self.breaks_build)
        passno = 3 if self.defect else (2 if self.marsh_requested or self.breaks_build else None)
        return {
            "schema": SCHEMA_ID, "run": self.run_id, "scenario": entry.id, "title": entry.title, "tier": self.tier,
            "driver": self.driver_name, "outcome": outcome, "failure": failure, "failed_check": failed_check,
            "blocked_on": blocked_on, "started_at": started_at, "finished_at": now(), "duration_ms": duration_ms,
            "break": {"kind": "break" if self.requested or self.breaks_build else "normal", "modes": self.requested,
                      "expected_checks": sorted(expected), "verified": run_verdict == "verified" if self.defect
                      else None, "variant": self.variant, "pass": passno, "verdict": run_verdict},
            "oracle": "inconclusive-tolerated" if self.tolerated else "equal",
            "active_breaks": self.active_breaks, "checks": list(self.recorder.checks),
            "commands": list(self.recorder.commands), "observations": list(self.recorder.observations),
            "timings": list(self.recorder.timings), "versions": versions, "foreign": list(self.foreign)}

    def write_evidence(self, scenario: str, document: dict[str, Any]) -> pathlib.Path:
        document = self.recorder.redact_document(document)
        validate_evidence(document)
        destination = self.evidence / f"{scenario}.json"
        staging = destination.with_suffix(".tmp")
        staging.write_text(json.dumps(document, indent=2) + "\n", encoding="utf-8")
        staging.chmod(0o600)
        staging.replace(destination)
        return destination

    def validate_all_evidence(self) -> list[str]:
        errors = []
        for path in sorted(self.evidence.glob("*.json")):
            if path.name == "summary.json":
                continue
            try:
                validate_evidence(json.loads(path.read_text(encoding="utf-8")))
            except (OSError, ValueError) as error:
                errors.append(f"evidence {path.name} does not validate: {error}")
        return errors

    # ---- finishing -----------------------------------------------------------------
    def finish(self, error: Exception | None) -> int:
        for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            signal.signal(number, signal.SIG_IGN)
        for process in self.live:
            self.kill(process)
        cleanup_errors = self.scope_cleanup_errors() if self.stock_before is not None else self.early_cleanup()
        expected_leak: list[str] = []
        if self.leak_on_purpose:
            # Only the one expected error (the deliberate probe is still there) is set aside. Anything else that
            # mentions the probe (a policy rule scoped to it, a changed setting) stays an error.
            expected_leak, cleanup_errors = split_expected_leak(cleanup_errors, self.probe_name)
            cleanup_errors += self.reap_deliberate_leak()
        validation = self.validate_all_evidence()
        failed = [o["scenario"] for o in self.outcomes
                  if o["outcome"] == "failed" or (o["outcome"] == "blocked" and not o["skipped"])]
        failed += [s for s in self.break_failures if s not in failed]
        if self.defect:
            failed = list(self.break_failures)
        if self.args.strict_pending:
            failed += [o["scenario"] for o in self.outcomes if o["outcome"] == "pending"]
        problems = ([str(error)] if error else []) + cleanup_errors + validation
        code = 1 if failed or problems else 0
        if self.tolerated:
            self.notes.append("NON-QUALIFYING: baseline changes were tolerated, so this run did not prove baseline "
                              "equality (a break verdict from it is not accepted by the matrix)")
        if not cleanup_errors and self.record is not None:
            self.record.remove()  # everything this run created is verifiably gone: no record for the collector
        summary = {"run": self.run_id, "tier": self.tier, "driver": self.driver_name, "break": self.requested,
                   "defect": self.defect, "variant": self.variant, "root": str(self.root), "outcomes": self.outcomes,
                   "failed": failed, "problems": problems, "expected_leak_errors": expected_leak,
                   "foreign": self.foreign, "notes": self.notes, "tolerated_baseline_changes": self.tolerated,
                   "qualifying": not self.tolerated,
                   "lock": self.lock, "preamble": self.preamble,
                   "environment": self.source}
        self.evidence.mkdir(mode=0o700, exist_ok=True)
        destination = self.evidence / "summary.json"
        destination.write_text(json.dumps(self.recorder.redact_document(summary), indent=2, default=str) + "\n",
                               encoding="utf-8")
        destination.chmod(0o600)
        counts = {name: sum(o["outcome"] == name for o in self.outcomes)
                  for name in ("passed", "failed", "pending", "blocked")}
        print(f"{self.LABEL}: run {self.run_id}: {counts}; {'FAILED' if code else 'ok'}; evidence: {self.evidence}")
        for problem in problems:
            print(f"{self.LABEL}: {problem}")
        for note in self.notes:
            print(f"{self.LABEL}: note: {note}")
        if self.foreign:
            print(f"{self.LABEL}: foreign resources (left alone; `make services-e2e-gc` collects them once their "
                  f"run is dead): {self.foreign}")
        return code

    def early_cleanup(self) -> list[str]:
        """Failed before the baselines existed: nothing was created beyond the scope's root."""
        errors = []
        try:
            shutil.rmtree(self.root)
        except OSError as caught:
            errors.append(f"could not remove isolated root {self.root}: {caught}")
        return errors

    def reap_deliberate_leak(self) -> list[str]:
        """The break verification is recorded; do not leave its leaked sandbox on the Mac. Returns the errors (a
        probe that cannot be removed fails the run and keeps the record)."""
        try:
            inventory = stock_vm_inventory(self.sbx, env=self.environment)
            if self.probe_name in inventory:
                remove_sandbox(self.sbx, self.probe_name, self.raw_ids.get(self.probe_name), self.stock_before,
                               env=self.environment)
                self.notes.append(f"deliberately leaked {self.probe_name} removed after the break was recorded")
            if self.probe_name in stock_vm_inventory(self.sbx, env=self.environment):
                return [f"the deliberately leaked {self.probe_name} is still there"]
            remaining = self.policy.rollback() + [e for e in self.h3_verify()]  # policy, setting, inventory, again
            if remaining:
                return remaining
            shutil.rmtree(self.root, ignore_errors=True)
            return []
        except Exception as error:
            return [f"could not remove the deliberate leak {self.probe_name}: {error}"]

    def pending_scenario(self) -> None:
        """Registered, not implemented yet."""
        raise Pending(f"{self.current.id if self.current else '?'}: not implemented yet (registered pending)")


def build_parser() -> argparse.ArgumentParser:
    parser = scenario_parser(__doc__, SCENARIOS, kit_help="fixture Kit (default: the acceptance fixture source)",
                             preflight=False)
    parser.set_defaults(kit=str(HERE.parent / "fixture"))
    parser.add_argument("--tier", choices=TIERS, default="smoke")
    parser.add_argument("--driver", choices=DRIVERS, default="product", help="H-8; recorded in every evidence file")
    parser.add_argument("--break", dest="break_modes", default=None, metavar="MODE[,MODE]",
                        help="requested modes: defects, `hold-<point>` barriers and `fault-*` faults")
    parser.add_argument("--break-defect", default=None, help="the defect under test (default: the one defect named)")
    parser.add_argument("--variant", default=None, help="the scenario variant a break cell names")
    parser.add_argument("--build", choices=("normal", "breaks"), default="normal",
                        help="the install under test (any marsh mode needs `breaks`)")
    parser.add_argument("--leak-on-purpose", action="store_true", help="the HARNESS-01 break (skips removing the probe)")
    parser.add_argument("--breaks-prefix", default=None, help="the `make dev DEV_FEATURES=test-breaks` install "
                        "(default: PREFIX-breaks)")
    parser.add_argument("--h2-tree", default=None, help="a checkout holding H2's scripts/check-no-test-breaks.sh and "
                        "tests/breaks/guest_env.sh (default: --source-tree)")
    parser.add_argument("--fixtures-dir", default=str(FIXTURES), help="H1b's fixtures (harness03.py, redaction.json)")
    parser.add_argument("--list-breaks", action="store_true", help="print the break registry, one JSON cell per line")
    parser.add_argument("--lock-wait", type=float, default=0,
                        help="seconds to wait for the VM lock (negative: wait as long as it takes)")
    parser.add_argument("--lock-probe", action="store_true", help="take the VM lock, say so, release it, exit")
    parser.add_argument("--gc", action="store_true", help="remove resources whose run is dead (takes the VM lock)")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--strict-pending", action="store_true", help="a pending scenario fails the run")
    parser.add_argument("--require-executed", action="store_true",
                        help="a scenario that is pending when run fails the run (the break protocol sets this)")
    parser.add_argument("--tolerate-baseline-changes", action="store_true",
                        help="accept sandboxes/policy rules of other tools changing during the run (recorded as notes; "
                             "without it a baseline change is an inconclusive oracle and fails the run)")
    parser.add_argument("--no-disk-check", action="store_true")
    return parser


def print_list() -> None:
    for entry in CATALOGUE:
        smoke = "+smoke" if entry.smoke and entry.tier != "smoke" else ""
        print(f"{entry.id}\t{status_of(entry)}\t{entry.tier}{smoke}\t{entry.owner}\t{entry.title}")


def strip_options(argv: list[str], names: set[str]) -> list[str]:
    kept, skip = [], False
    for argument in argv:
        if skip:
            skip = False
        elif argument in names:
            skip = True
        elif not any(argument.startswith(name + "=") for name in names):
            kept.append(argument)
    return kept


def validate_break(parser: argparse.ArgumentParser, arguments: argparse.Namespace) -> None:
    """--break needs named scenarios and a complete, exact cell: the defect is registered for each scenario (and
    variant), and the requested modes are exactly the defect plus that cell's holds, faults and paired controls.
    Naming only the defect adds those for you."""
    modes = normalize_modes(arguments.break_modes)
    if arguments.leak_on_purpose:
        modes = normalize_modes(modes + ["leak-on-purpose"])
    if not modes:
        if arguments.break_defect:
            parser.error("--break-defect needs --break")
        return
    if not arguments.only:
        parser.error("--break needs --only ID: a break is verified against the scenarios that register it")
    defect = arguments.break_defect or infer_defect(modes)
    if defect:
        if defect not in modes:
            parser.error(f"--break-defect {defect!r} is not among the activated modes {modes}")
        for scenario in arguments.only:
            entry = BY_ID[scenario]
            if not breaks.cells(entry, defect, arguments.variant):
                registered = sorted({b.mode for b in entry.breaks})
                parser.error(f"{scenario} registers no break {defect!r} (it has: {registered})")
            needed = sorted({defect, *prerequisites(entry, defect, arguments.variant)})
            if modes == [defect]:
                modes = needed
            elif modes != needed:
                parser.error(f"{scenario}: the cell ({defect}, variant {arguments.variant}) needs exactly {needed}, "
                             f"not {modes}: a defect run activates the defect and its prerequisites and nothing else")
    elif arguments.break_defect is None and breaks.classify(modes)["defects"]:
        parser.error(f"--break names several defects {breaks.classify(modes)['defects']}: name the one under test "
                     "with --break-defect")
    arguments.break_modes = ",".join(modes)
    arguments.break_defect = defect


def split_expected_leak(errors: list[str], probe: str) -> tuple[list[str], list[str]]:
    """(expected, rest): only the exact error naming the deliberate probe as this run's sole remaining sandbox is
    the expected leak. Anything else that merely mentions the probe stays an error."""
    expected = f"this run's sandboxes remain: ['{probe}']"
    return [e for e in errors if e == expected], [e for e in errors if e != expected]


CLEANUP_BUDGET = 600.0  # seconds a timed-out child gets, after SIGINT, to finish its own cleanup


ACTIVE_CHILD: subprocess.Popen[str] | None = None
ACTIVE_STARTED: Callable[[], bool] | None = None
CANCELLED: list[int] = []  # signals the matrix coordinator received (it then starts no further pass)


def run_child(command: list[str], timeout: float, budget: float = CLEANUP_BUDGET,
              started: Callable[[], bool] | None = None, env: dict[str, str] | None = None) -> tuple[int, str]:
    """Run one protocol child. TIMEOUT counts from when the child says it is working (STARTED(): the worker creates
    its handshake file only after it holds the VM lock and has proved that), never while it waits for other agents'
    runs: waiting is not working. A child that never says so is not timed, because it holds nothing and owns nothing.
    On timeout the child is INTERRUPTED (SIGINT: its harness then runs its cleanup and verification) and given BUDGET
    seconds, then asked to terminate; it is never killed, because a kill would orphan the daemons and VMs it owns. If
    it will not finish, its run record under harness/runs keeps the exact ownership evidence and the result says so.
    A signal the coordinator receives is forwarded to the child (see `forward_cancellation`) and awaited."""
    global ACTIVE_CHILD, ACTIVE_STARTED
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                               process_group=0, env=env)
    ACTIVE_CHILD, ACTIVE_STARTED = process, started
    clock = time.monotonic() if started is None else None
    note = ""
    try:
        while True:
            try:
                output, _ = process.communicate(timeout=1.0)
                code = process.returncode
                break
            except subprocess.TimeoutExpired:
                if clock is None and started():
                    clock = time.monotonic()
                if clock is None or time.monotonic() - clock < timeout:
                    continue
            process.send_signal(signal.SIGINT)
            try:
                output, _ = process.communicate(timeout=budget)
                note = f"timed out after {timeout:.0f}s; interrupted and cleaned up (exit {process.returncode})"
            except subprocess.TimeoutExpired:
                process.terminate()
                try:
                    output, _ = process.communicate(timeout=120)
                    note = f"timed out after {timeout:.0f}s; terminated after {budget:.0f}s of cleanup"
                except subprocess.TimeoutExpired:
                    output = ""
                    note = (f"timed out after {timeout:.0f}s and child pid {process.pid} is STILL RUNNING (not killed); "
                            "its run record under harness/runs lists exactly what it owns")
            code, output = -9, f"{note}\n{output or ''}"
            break
    finally:
        ACTIVE_CHILD = ACTIVE_STARTED = None
    sys.stdout.write(output)
    sys.stdout.flush()
    return code, output


def forward_cancellation(number: int, frame: object) -> None:
    """The matrix coordinator holds no lock, but its pass child may be working under one: pass the signal on (a child
    that has not started working yet gets SIGTERM: it holds nothing, and a backgrounded SIGINT may be ignored), start
    no further pass, and let `run_child` wait for the child to finish its own cleanup."""
    CANCELLED.append(number)
    child, started = ACTIVE_CHILD, ACTIVE_STARTED
    if child is not None and child.poll() is None:
        if number == signal.SIGINT and started is not None and not started():
            number = signal.SIGTERM
        with contextlib.suppress(ProcessLookupError):
            child.send_signal(number)


def child_ok(code: int, output: str) -> bool:
    """A baseline pass counts only if the child exited 0 and its oracle was qualifying (no tolerated changes)."""
    return code == 0 and "NON-QUALIFYING" not in output


def pass3_result(code: int, output: str, scenario: str, mode: str) -> str:
    """The matrix verdict of a pass-3 child: `verified` only if the child printed that verdict, exited 0 and was
    qualifying. A child that exited nonzero (cleanup, validation, setup, timeout) is an invalid negative control
    whatever it printed, and a run with tolerated baseline changes is non-qualifying."""
    found = re.search(rf"break verdict {re.escape(scenario)} {re.escape(mode)}: (\S+)", output)
    if "NON-QUALIFYING" in output:
        return "non-qualifying"
    if code != 0:
        return "invalid-negative-control" if not found or found.group(1) == "verified" else found.group(1)
    return found.group(1) if found else "invalid-negative-control"


def orchestrate_break(arguments: argparse.Namespace, argv: list[str]) -> int:
    """TIER=break: for every cell, pass 1 (normal build), pass 2 (breaks build, explicitly launched daemon, with only
    the holds, faults and paired controls the cell needs; harness and fixture cells run it too), pass 3 (the same plus
    the defect). Anything but a failure at the named check in pass 3, with a clean qualifying exit, is an invalid
    negative control (or not detected), never a verified break. Every child runs with --require-executed."""
    entries = select("break", arguments.only) if arguments.only else \
        [e for e in CATALOGUE if status_of(e) == "implemented" and any(b.kind in breaks.VERIFIABLE_HERE for b in e.breaks)]
    base = strip_options(argv, {"--tier", "--only", "--break", "--break-defect", "--variant", "--build"})
    base += ["--require-executed"]
    results: list[tuple[str, str, str]] = []
    CANCELLED.clear()
    handshakes = pathlib.Path(tempfile.mkdtemp(prefix="nfx-matrix-"))
    previous = {number: signal.signal(number, forward_cancellation)
                for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)}
    try:
        code = run_matrix(entries, base, results, handshakes)
    finally:
        for number, handler in previous.items():
            signal.signal(number, handler)
        shutil.rmtree(handshakes, ignore_errors=True)
    return code


def run_matrix(entries: list[Entry], base: list[str], results: list[tuple[str, str, str]],
               handshakes: pathlib.Path) -> int:
    counter = 0
    for entry in entries:
        if CANCELLED:
            break
        if status_of(entry) == "pending" or not any(b.kind in breaks.VERIFIABLE_HERE for b in entry.breaks):
            results.append((entry.id, "-", "not executed: pending or no verifiable cell"))
            continue
        timeout = max(900.0, entry.est * 180.0)
        baseline_failed = False
        pass_two: dict[tuple[str, ...], bool] = {}
        for step in breaks.plan_commands(entry, base, __file__, sys.executable):
            cell = step["cell"]
            if baseline_failed or CANCELLED:
                break
            if step["pass"] == 3:
                key = (cell.variant or "", *sorted(cell.prerequisites))
                if not pass_two.get(key, False):
                    results.append((entry.id, step["label"], "not run: its pass 2 baseline failed"))
                    continue
            print(f"services: {entry.id}: {step['label']}", flush=True)
            counter += 1
            handshake = handshakes / f"child-{counter}"
            code, output = run_child(step["argv"], timeout, started=handshake.exists,
                                     env={**os.environ, nfx_lock.STARTED_FILE: str(handshake)})
            if step["pass"] == 3:
                results.append((entry.id, step["label"], pass3_result(code, output, entry.id, cell.mode)))
                continue
            good = child_ok(code, output)
            results.append((entry.id, step["label"] if step["pass"] == 2 else "pass 1",
                            "ok" if good else ("non-qualifying" if "NON-QUALIFYING" in output else "baseline-failed")))
            if step["pass"] == 1:
                baseline_failed = not good
            else:
                pass_two[(cell.variant or "", *sorted(cell.prerequisites))] = good
    print("services: break matrix" + (f" (cancelled by signal {CANCELLED[0]}: no further pass was started)"
                                       if CANCELLED else ""))
    for scenario, label, outcome in results:
        print(f"  {scenario:12} {label:46} {outcome}")
    good_outcomes = {"ok", "verified"}
    if CANCELLED:
        return 128 + CANCELLED[0]
    return 0 if results and all(outcome in good_outcomes for _, _, outcome in results) else 1


def run_gc(arguments: argparse.Namespace) -> int:
    """Runs under the VM lock (its holder is this process's parent): every record found is a finished run's."""
    sbx = shutil.which(arguments.sbx) or arguments.sbx
    report = collect(sbx, runs_dir(nfx_lock.lock_path()), dry_run=arguments.dry_run)
    for row in report:
        print(f"services-gc: {row['action']:18} {row['name']}  ({row['why']})")
    if not report:
        print("services-gc: nothing to collect")
    return 1 if any(row["action"] == "failed" for row in report) else 0


def lock_wait(arguments: argparse.Namespace) -> float | None:
    return None if arguments.lock_wait < 0 else arguments.lock_wait


def reexec_locked(arguments: argparse.Namespace, argv: list[str], worker: list[str] | None = None) -> int:
    """Run the harness body as the child of the shared helper's CLI (`nfx_lock.supervise`): the helper holds the VM lock
    and starts the child in its own session, holds the lock until every process of that session has exited, and
    forwards INT/TERM/HUP to it so the child runs its own cleanup and verification while the lock is still held."""
    environment = {**os.environ, nfx_lock.WORKER_RUN: arguments.run}
    try:
        return nfx_lock.supervise(worker or [sys.executable, str(pathlib.Path(__file__).resolve()), *argv],
                                  run=arguments.run, wait=lock_wait(arguments), env=environment)
    except LockHeld as held:
        print(f"services: {held}", file=sys.stderr)
        return EXIT_LOCK_HELD
    except LockUnavailable as error:
        print(f"services: {error}", file=sys.stderr)
        return 2


def announce_start() -> None:
    """The worker's handshake to a matrix coordinator: it holds the lock (proved) and is starting. The file is the
    coordinator's cue to start the pass's timeout; a worker that cannot write it does not run."""
    path = os.environ.pop(nfx_lock.STARTED_FILE, None)
    if path:
        os.close(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600))


def main() -> int:
    parser = build_parser()
    argv = sys.argv[1:]
    arguments = parser.parse_args(argv)
    if arguments.list:
        print_list()
        return 0
    if arguments.list_breaks:
        sys.stdout.write(list_breaks_text())
        return 0
    if not arguments.gc:
        validate_break(parser, arguments)
    claimed_run = os.environ.pop(nfx_lock.WORKER_RUN, None)  # a hint only: the proof is `hold_proof`
    arguments.run = claimed_run or new_run_id()
    if arguments.tier == "break" and not arguments.break_modes and arguments.build == "normal" \
            and not arguments.lock_probe and not arguments.gc and not claimed_run:
        return orchestrate_break(arguments, argv)  # holds nothing: each child holds the lock for its own run
    if arguments.lock_probe:
        try:
            with nfx_lock.hold(arguments.run, lock_wait(arguments)):
                print("services: lock acquired")
        except LockHeld as held:
            print(f"services: {held}", file=sys.stderr)
            return EXIT_LOCK_HELD
        except LockUnavailable as error:
            print(f"services: {error}", file=sys.stderr)
            return 2
        return 0
    if not claimed_run:
        return reexec_locked(arguments, argv)
    problems = nfx_lock.hold_proof(arguments.run)
    if problems:  # fail closed: nothing below (not even --gc) runs without a live holder
        print("services: refusing to run: this process is not the worker of a live VM lock holder: "
              + "; ".join(problems), file=sys.stderr)
        return 2
    announce_start()
    arguments.lock = {"holder_pid": os.getppid(), "path": str(nfx_lock.lock_path()),
                      "helper": str(nfx_lock.helper_path())}
    if arguments.gc:
        return run_gc(arguments)
    arguments.normal_prefix = arguments.prefix
    arguments.breaks_prefix = arguments.breaks_prefix or f"{arguments.prefix}-breaks"
    if marsh_modes(normalize_modes(arguments.break_modes)) or arguments.build == "breaks":
        arguments.build = "breaks"
        arguments.prefix = arguments.breaks_prefix
    arguments.leak_on_purpose = False

    def interrupted(number: int, frame: object) -> None:
        raise KeyboardInterrupt

    for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(number, interrupted)
    resolved = resolve_arguments(Services, parser, arguments)
    if resolved is None:
        return 1
    return run_harness(Services(resolved))


if __name__ == "__main__":
    raise SystemExit(main())
