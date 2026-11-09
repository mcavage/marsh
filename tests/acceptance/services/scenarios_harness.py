"""The wave-0 scenarios: HARNESS-01 (lock, naming, cleanup scope), HARNESS-02 (break modes come only from an
explicitly launched host daemon) and HARNESS-03 (H1b's fixture self-test, wired through its `REGISTRATION`).

A mixin of the suite in services_uat.py: it uses the suite's recorder, observer, cleanup and run record.
"""

from __future__ import annotations

import contextlib
import importlib.util
import json
import os
import pathlib
import random
import secrets
import signal
import selectors
import shutil
import subprocess
import sys
import tempfile
import time
from typing import Any, Iterator

from harness import IMAGE, Fail, daemon_endpoint_paths, now, text
from launch_env import clean_environment
from nfx_gc import RunRecord, collect, runs_dir
from nfx_host import VM_PREFIX, baseline_oracle, is_mine, remove_sandbox
import nfx_lock
from nfx_lock import EXIT_LOCK_HELD, info_path
from evidence import Stopwatch, validate_evidence
from provenance import stock_vm_inventory
from scopes import SideScope, launch_daemon, reap

HERE = pathlib.Path(__file__).resolve().parent


class CheckFailed(Fail):
    """A named oracle check of the scenario failed (recorded as `failed_check`)."""

    def __init__(self, check: str, detail: str) -> None:
        self.check = check
        super().__init__(f"{check}: {detail}")


class Blocked(Exception):
    """The scenario cannot run here (a task it needs has not landed)."""


def new_decoy_run() -> str:
    """A run id that is not this run's and is not reused: `250101` + six random digits + four hex."""
    return "250101" + "".join(random.choice("0123456789") for _ in range(6)) + secrets.token_hex(2)


def oracle_selftest() -> list[str]:
    """The baseline-equality oracle against adversarial inventories and policy tables: it must catch each defect and
    stay quiet on foreign decoys. Returns the cases it got wrong (empty: not vacuous)."""
    run, other = "2611061039191b2c", "250101123456beef"
    before = {"base-vm": "u1", "marsh-k-user0001": "u2"}
    rule = {"id": "r1", "name": "r1", "scope": "sandbox:marsh-k-user0001", "applies_to": "sandbox:marsh-k-user0001",
            "resource_type": "network", "decision": "allow", "resources": ["x"]}
    rules = {"r1": rule}
    wrong: list[str] = []

    def oracle(after: dict[str, str], after_rules: dict[str, Any] | None = None, *, owned: set[str] = frozenset(),
               claims: dict[str, str | None] | None = None, removed: set[str] = frozenset(), tolerate: bool = False):
        return baseline_oracle(before, after, run, set(owned), claims or {}, set(removed), rules,
                               rules if after_rules is None else after_rules, tolerate)

    cases = {
        "a removed baseline sandbox is an error": (lambda: oracle({"marsh-k-user0001": "u2"})[0], True),
        "a replaced baseline sandbox is an error": (lambda: oracle({**before, "base-vm": "u9"})[0], True),
        "a sandbox of this run left behind is an error": (lambda: oracle({**before, f"nfx-{run}-probe": "p"})[0], True),
        "an owned product VM left behind is an error": (
            lambda: oracle({**before, "marsh-xnfx01-k-aaaaaaaa": "v"}, owned={"marsh-xnfx01-k-aaaaaaaa"})[0], True),
        "a new product VM in nobody's map is an error": (
            lambda: oracle({**before, "marsh-xnfx01-k-bbbbbbbb": "v"})[0], True),
        "a vanished baseline policy rule is an error": (lambda: oracle(before, {})[0], True),
        "a policy rule of ours left behind is an error": (lambda: oracle(before, {**rules, "r2": {
            **rule, "id": "r2", "applies_to": "all", "scope": "global"}})[0], True),
        "another run's sandbox is foreign, not an error": (
            lambda: oracle({**before, f"nfx-{other}-decoy": "d"})[0], False),
        "another run's product VM with its recorded ID is foreign, not an error": (
            lambda: oracle({**before, "marsh-xnfx01-k-cccccccc": "v"}, claims={"marsh-xnfx01-k-cccccccc": "v"})[0], False),
        "a same-name product VM with another ID than the record's is an error": (
            lambda: oracle({**before, "marsh-xnfx01-k-cccccccc": "v2"}, claims={"marsh-xnfx01-k-cccccccc": "v"})[0], True),
        "a claimed name whose record has no ID is an error": (
            lambda: oracle({**before, "marsh-xnfx01-k-cccccccc": "v"}, claims={"marsh-xnfx01-k-cccccccc": None})[0], True),
        "an unchanged host is clean": (lambda: oracle(dict(before))[0], False),
        "tolerated churn is a note, not an error": (lambda: oracle({"base-vm": "u1"}, tolerate=True)[0], False),
        "tolerated churn is still recorded": (lambda: oracle({"base-vm": "u1"}, tolerate=True)[1], True),
        "a removal this harness issued is an error even when tolerated": (
            lambda: oracle({"base-vm": "u1"}, removed={"marsh-k-user0001"}, tolerate=True)[0], True),
    }
    for name, (probe, expect_errors) in cases.items():
        if bool(probe()) != expect_errors:
            wrong.append(name)
    return wrong


class HarnessScenarios:
    # ---- HARNESS-01 -------------------------------------------------------------------------------
    def harness_01(self) -> None:
        """Lock, naming, cleanup scope: the lock excludes a second run, names are scoped, cleanup removes only what this run proved is its own."""
        run = self.run_id
        expected_root = pathlib.Path(os.environ.get("MARSH_UAT_ROOT") or "/private/tmp").resolve() / f"nfx-{run}"
        paths = {"root": self.root, "MARSH_HOME": self.home, "MARSH_CONTROL_HOME": self.control_root,
                 "project": self.project}
        for label, path in paths.items():
            self.check("c1", path == expected_root or path.is_relative_to(expected_root),
                       f"{label} {path} is not under {expected_root}")
        self.check("c1", self.environment["MARSH_HOME"] == str(self.home)
                   and self.environment["MARSH_CONTROL_HOME"] == str(self.control_root),
                   "the daemon environment does not carry the isolated MARSH_HOME/MARSH_CONTROL_HOME")
        self.check("c1", self.environment.get("MARSH_VM_PREFIX") == VM_PREFIX,
                   f"the daemon environment lacks MARSH_VM_PREFIX={VM_PREFIX}")
        self.check("c1", bool(self.lock) and self.lock.get("holder_pid") == os.getppid(),
                   f"this run is not a child of the process that holds the VM lock: {self.lock}")
        record = nfx_lock.holder_record(pathlib.Path(self.lock["path"]))
        self.check("c1", record is not None and record.get("pid") == os.getppid() and record.get("run") == run,
                   f"the lock's info does not name this run's holder (display only, but the helper writes it): {record}")
        script = str(HERE / "services_uat.py")
        probe = [sys.executable, script, "--lock-probe", "--tier", self.tier]
        refused = self.xrun([*probe, "--lock-wait", "0"], timeout=60, env=clean_environment(), note="second harness")
        self.check("c1", refused.returncode == EXIT_LOCK_HELD
                   and f'"pid": {os.getppid()}' in text(refused.stderr + refused.stdout),
                   f"a second harness was not refused naming the holder (pid {os.getppid()}): "
                   f"{refused.returncode} {text(refused.stderr)[-300:]!r}")
        began = time.monotonic()
        waited = self.xrun([*probe, "--lock-wait", "3"], timeout=60, env=clean_environment(),
                           note="second harness, waits 3 s")
        self.check("c1", waited.returncode == EXIT_LOCK_HELD and time.monotonic() - began >= 3
                   and "waited" in text(waited.stderr + waited.stdout),
                   f"a waiting second harness did not wait then report the holder: {waited.returncode} "
                   f"{text(waited.stderr)[-300:]!r}")
        self.lock_checks()
        # a raw sandbox the harness makes itself: the intent is persisted before the effect, the ID only after a
        # successful create (a collision or a failed create is never adopted)
        probe_dir = self.root / "probe"
        probe_dir.mkdir()
        self.check("c1", self.probe_name == f"nfx-{run}-probe" and is_mine(self.probe_name, run, set()),
                   f"probe name {self.probe_name}")
        self.create_sandbox(self.probe_name, probe_dir)
        self.check("c1", self.probe_name in self.raw_ids, f"no recorded ID for {self.probe_name}")
        ran = self.xrun([self.sbx, "exec", self.probe_name, "true"], timeout=300)
        self.check("c1", ran.returncode == 0, f"sbx exec {self.probe_name} true: {ran.returncode} "
                                              f"{text(ran.stderr)[-300:]!r}")
        self.observer.bind(self.probe_name, direct_uuid=self.raw_ids[self.probe_name])
        echoed = self.observer.vx("echo nfx-observer-ok", timeout=120)
        self.check("c1", echoed.returncode == 0 and echoed.stdout.strip() == b"nfx-observer-ok",
                   f"the observer could not read the probe: {echoed.returncode} {text(echoed.stderr)[-200:]!r}")
        # cleanup of this run's resources, then the baseline and leak oracle (and the oracle against adversaries)
        cleanup_errors = self.h3_cleanup()
        after = stock_vm_inventory(self.sbx, env=self.environment)
        leftovers = [n for n in after if n.startswith(f"nfx-{run}-")]
        verify_errors = self.h3_verify()
        self.check("c2", not leftovers and not cleanup_errors and not verify_errors,
                   f"after cleanup: leftovers={leftovers} cleanup={cleanup_errors} verify={verify_errors}")
        if self.tolerated:  # baseline equality was not proven, so the check that claims it cannot pass
            self.skip_check("c2", f"baseline changes were tolerated ({self.tolerated}); equality is unproven, "
                                  "so this run is non-qualifying")
        wrong = oracle_selftest()
        self.check("c2", not wrong, f"the baseline/leak oracle is vacuous for: {wrong}")
        self.foreign_decoy()
        provisional = self.evidence_document(self.current, "passed", None, None, None, now(), 0)
        try:
            validate_evidence(self.recorder.redact_document(provisional))
        except ValueError as error:
            self.check("c4", False, f"the evidence for this scenario does not validate: {error}")
        self.check("c4", True, "evidence validates against evidence.schema.json")

    def foreign_decoy(self) -> None:
        """c3: a sandbox of another run (a unique ID, its own record) is foreign: this run's cleanup and verification
        leave it alone with no error, and the collector keeps it while its run is named alive, keeps a replacement of
        it, and removes it only once that run is finished and its recorded name and ID match. A new VM under a name
        another record lists, but with a different ID, is an error."""
        decoy_run = new_decoy_run()
        decoy = f"nfx-{decoy_run}-decoy"
        decoy_dir = self.root / "decoy"
        decoy_dir.mkdir()
        directory = runs_dir(nfx_lock.lock_path())
        other = RunRecord(directory, decoy_run, "smoke", self.root / "decoy-run")
        try:
            other.save(exclusive=True)  # never overwrite a record that exists
        except FileExistsError:
            raise Fail(f"a run record for {decoy_run} already exists; refusing to overwrite it") from None
        try:
            self.create_sandbox(decoy, decoy_dir, record=other, note="the other run's decoy")
            identifier = other.data["created"][decoy]
            self.check("c3", bool(identifier), "the decoy has no recorded ID")
            cleanup_errors = self.h3_cleanup()
            self.check("c3", decoy in stock_vm_inventory(self.sbx, env=self.environment),
                       "this run's cleanup removed another run's sandbox")
            self.check("c3", not cleanup_errors, f"cleanup reported errors over a foreign decoy: {cleanup_errors}")
            verify_errors = self.h3_verify()
            self.check("c3", not verify_errors, f"verification failed over a foreign decoy: {verify_errors}")
            self.check("c3", decoy in self.foreign, f"the decoy is not listed as foreign: {self.foreign}")
            report = collect(self.sbx, directory, env=self.environment, only={decoy}, alive={decoy_run})
            self.check("c3", decoy in stock_vm_inventory(self.sbx, env=self.environment)
                       and report and report[0]["action"] == "kept" and "alive" in report[0]["why"],
                       f"the collector removed a sandbox whose run is alive: {report}")
            # a replacement under the same name (a different ID) is not removed even though its run is finished
            replacement = {**stock_vm_inventory(self.sbx, env=self.environment), decoy: "not-the-recorded-id"}
            report = collect(self.sbx, directory, env=self.environment, only={decoy}, inventory=replacement)
            self.check("c3", report and report[0]["action"] == "kept" and "replacement" in report[0]["why"],
                       f"the collector would remove a replacement of the recorded sandbox: {report}")
            # and the baseline oracle does not take the replacement for the recorded foreign VM either
            claims = {"marsh-xnfx01-k-zzzzzzzz": "recorded-id"}
            errors, _ = baseline_oracle({}, {"marsh-xnfx01-k-zzzzzzzz": "another-id"}, self.run_id, set(), claims, set(), {}, {})
            fine, _ = baseline_oracle({}, {"marsh-xnfx01-k-zzzzzzzz": "recorded-id"}, self.run_id, set(), claims, set(), {}, {})
            self.check("c3", errors and not fine, f"foreign claims are not checked against the recorded ID: {errors} {fine}")
            report = collect(self.sbx, directory, env=self.environment, only={decoy})
            self.check("c3", decoy not in stock_vm_inventory(self.sbx, env=self.environment)
                       and report and report[0]["action"] == "removed",
                       f"the collector did not remove the decoy once its run was finished: {report}")
        finally:
            leftover = stock_vm_inventory(self.sbx, env=self.environment)
            if decoy in leftover:  # a failed check must not leave the decoy on the Mac
                with contextlib.suppress(ValueError):
                    remove_sandbox(self.sbx, decoy, other.data["created"].get(decoy), self.stock_before,
                                   env=self.environment)
            if decoy not in stock_vm_inventory(self.sbx, env=self.environment):
                other.remove()

    def lock_checks(self) -> None:
        """The shared helper's rules, on scratch lock files (VMLOCK_TEST_PATH); the real lock is held, not touched:
        (a) a live holder is never displaced, whatever `lock.info` says, and its lock file is never deleted, replaced,
        truncated or rewritten; (b) a lock nobody holds is simply taken, whatever a stale `lock.info` says; (c) two
        contenders for one free lock: exactly one wins; (d) a holder that is sent SIGTERM keeps the lock while its
        child still works (negative control: a holder that dies on SIGTERM releases it under live work, and this probe
        sees that)."""
        scratch = pathlib.Path(tempfile.mkdtemp(prefix="lock-", dir=self.root))
        helper = nfx_lock.helper_path()
        dead = subprocess.Popen(["/usr/bin/true"])
        dead.wait()
        stale_info = {"pid": dead.pid, "run": "deadrun", "mode": "exclusive", "since": "2026-01-01T00:00:00"}
        spawned: list[subprocess.Popen[bytes]] = []

        def environment(path: pathlib.Path) -> dict[str, str]:
            return {**clean_environment(), "VMLOCK_TEST_PATH": str(path)}

        def probe(path: pathlib.Path, wait: str = "0") -> subprocess.CompletedProcess[bytes]:
            return self.xrun([sys.executable, str(HERE / "services_uat.py"), "--lock-probe", "--tier", self.tier,
                              "--lock-wait", wait], timeout=60, env=environment(path), note=f"probe {path.name}")

        def make(name: str) -> pathlib.Path:
            path = scratch / name
            path.touch()  # lock files are never created by tools: the test creates its scratch file
            return path

        def start(argv: list[str], path: pathlib.Path) -> subprocess.Popen[bytes]:
            process = subprocess.Popen(argv, env=environment(path), stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                       process_group=0)
            spawned.append(process)
            self.live.append(process)
            return process

        def wait_held(path: pathlib.Path, process: subprocess.Popen[bytes]) -> bool:
            for _ in range(100):
                record = nfx_lock.holder_record(path)
                if record and record.get("pid") == process.pid:
                    return True
                time.sleep(0.1)
            return False

        try:
            # (a) a live holder, lock.info naming a dead pid: the lock is held, nothing is displaced
            held = make("held")
            holder = start([sys.executable, str(helper), "--run", "live-holder", "--", "sleep", "300"], held)
            self.check("c1", wait_held(held, holder), "the lock-holding helper did not take the scratch lock")
            nfx_lock.info_path(held).write_text(json.dumps(stale_info))
            before = (held.stat().st_ino, held.read_bytes())
            for wait in ("0", "1"):
                refused = probe(held, wait)
                self.check("c1", refused.returncode == EXIT_LOCK_HELD and "deadrun" in text(refused.stderr),
                           f"a live holder was not respected (wait {wait}): {refused.returncode} "
                           f"{text(refused.stderr)[-300:]!r}")
            cli = self.xrun([sys.executable, str(helper), "--timeout", "1", "--run", "second", "--", "true"],
                            timeout=60, env=environment(held), note="the helper's own CLI against a live holder")
            self.check("c1", cli.returncode == 75, f"the helper CLI took a lock a live process holds: {cli.returncode}")
            self.check("c1", (held.stat().st_ino, held.read_bytes()) == before,
                       "the lock file was deleted, replaced, truncated or rewritten while its holder lived")
            self.check("c1", holder.poll() is None, "the live holder died")
            # (b) nobody holds the flock but lock.info names a dead pid: simply taken, the lock file untouched
            free = make("free")
            free.write_text("older tool text")
            nfx_lock.info_path(free).write_text(json.dumps(stale_info))
            before = (free.stat().st_ino, free.read_bytes())
            taken = probe(free)
            self.check("c1", taken.returncode == 0 and "lock acquired" in text(taken.stdout),
                       f"a lock nobody holds was not taken: {taken.returncode} {text(taken.stderr)[-300:]!r}")
            self.check("c1", (free.stat().st_ino, free.read_bytes()) == before,
                       "taking and releasing the lock changed the lock file")
            # (c) two contenders for one free lock: exactly one wins
            contested = make("contested")
            gate = scratch / "go"
            contenders = [start([sys.executable, str(HERE / "lock_contender.py"), str(gate)], contested)
                          for _ in range(2)]
            time.sleep(1)
            gate.write_text("go")
            outcomes = sorted(text(c.communicate(timeout=60)[0]).strip() for c in contenders)
            self.check("c1", outcomes == ["acquired", "held"],
                       f"two contenders for one lock did not resolve to exactly one winner: {outcomes}")
            # (d) SIGTERM to the holder does not release the lock while its child is still working
            self.sigterm_checks(scratch, make, start, probe, wait_held)
        finally:
            for process in spawned:
                self.kill(process)
                with contextlib.suppress(ValueError):
                    self.live.remove(process)
            shutil.rmtree(scratch, ignore_errors=True)

    def sigterm_checks(self, scratch, make, start, probe, wait_held) -> None:
        helper = nfx_lock.helper_path()
        child = ("import os,signal,sys,time\nopen(sys.argv[1]+'.pid','w').write(str(os.getpid()))\n"
                 "signal.signal(signal.SIGTERM, lambda *a: open(sys.argv[1]+'.term','w').write('x'))\n"
                 "time.sleep(float(sys.argv[2]))\nopen(sys.argv[1]+'.done','w').write('x')")
        # the holder is the helper's own `vmlock.run`; its child outlives the SIGTERM by a few seconds
        path = make("sigterm")
        marker = scratch / "sigterm-child"
        holder = start([sys.executable, str(helper), "--run", "sigterm-holder", "--", sys.executable, "-c", child,
                        str(marker), "6"], path)
        self.check("c1", wait_held(path, holder), "the SIGTERM holder did not take the scratch lock")
        for _ in range(100):
            if (scratch / "sigterm-child.pid").exists():
                break
            time.sleep(0.1)
        holder.send_signal(signal.SIGTERM)
        for _ in range(100):
            if (scratch / "sigterm-child.term").exists():
                break
            time.sleep(0.1)
        self.check("c1", (scratch / "sigterm-child.term").exists(), "SIGTERM was not forwarded to the holder's child")
        held_now = probe(path)
        self.check("c1", held_now.returncode == EXIT_LOCK_HELD and not (scratch / "sigterm-child.done").exists(),
                   f"the lock was released on SIGTERM while the child was still working: {held_now.returncode}")
        holder.communicate(timeout=60)
        self.check("c1", (scratch / "sigterm-child.done").exists() and probe(path).returncode == 0,
                   "the lock was not free after the child finished and was reaped")
        # negative control: a holder that dies on SIGTERM (default disposition) frees the lock under live work
        bad_path = make("sigterm-bad")
        bad_marker = scratch / "bad-child"
        bad = ("import subprocess,sys\nsys.path.insert(0,%r)\nimport nfx_lock\n"
               "with nfx_lock.hold('bad-holder', 0):\n"
               "    subprocess.Popen([sys.executable,'-c',sys.argv[1],sys.argv[2],'6'])\n"
               "    import time; time.sleep(60)" % str(HERE))
        victim = start([sys.executable, "-c", bad, child, str(bad_marker)], bad_path)
        self.check("c1", wait_held(bad_path, victim), "the control holder did not take the scratch lock")
        for _ in range(100):
            if (scratch / "bad-child.pid").exists():
                break
            time.sleep(0.1)
        victim.send_signal(signal.SIGTERM)
        victim.wait(timeout=30)
        stolen = probe(bad_path)
        self.check("c1", stolen.returncode == 0 and not (scratch / "bad-child.done").exists(),
                   "the negative control did not show the defect: a holder killed by SIGTERM should free the lock "
                   f"while its child still works ({stolen.returncode})")
        try:
            os.kill(int((scratch / "bad-child.pid").read_text()), signal.SIGKILL)  # our own control child
        except (OSError, ValueError):
            pass

    # ---- HARNESS-02 -------------------------------------------------------------------------------
    def h2_script(self, relative: str) -> pathlib.Path:
        tree = pathlib.Path(self.args.h2_tree or self.args.source_tree)
        script = tree / relative
        if not script.is_file():
            raise Blocked(f"blocked-on:H2: {script} does not exist (merge H2, or pass --h2-tree a checkout that has it)")
        return script

    def side_scope(self, name: str, prefix: pathlib.Path, *, modes: list[str] | None = None, flag: bool = True,
                   kit: str | None = None) -> SideScope:
        """An isolated daemon of install PREFIX. With MODES (even an empty list) the daemon is launched explicitly
        (`marshd --home H --test-breaks-from-env`, or without the flag when FLAG is False) with them in its
        environment; with None the first CLI call starts it, as a user's would."""
        side = SideScope(self, name, prefix, kit)
        self.side_scopes.append(side)
        if modes is not None:
            side.daemon = launch_daemon(side, side.marshd, modes, side.root / "marshd-explicit.log", flag=flag)
        return side

    def side_status(self, side: SideScope, note: str, env: dict[str, str] | None = None) -> dict[str, Any]:
        """`marsh status --json` from SIDE's daemon. If the first status cannot authenticate, the process this harness
        launched is reaped (it created no VM) so nothing is left running."""
        try:
            done = self.xrun([side.marsh, "status", "--json"], cwd=side.project, env={**side.environment, **(env or {})},
                             timeout=120, check=True, note=note)
            side.scope_started = True
            status = json.loads(done.stdout)
            side.remember_owned_status(status)
        except Exception:
            side.abort_unowned_daemon()
            raise
        return status

    def stop_side(self, side: SideScope) -> None:
        side.abort_unowned_daemon()
        errors = side.cleanup_isolated_scope()
        if side.daemon is not None:
            reap(side.daemon)
        self.side_scopes.remove(side)
        if errors:
            raise Fail(f"{side.root.name}: could not stop its daemon and VMs: {errors}")

    @staticmethod
    def answering_pid(status: dict[str, Any]) -> int | None:
        return (status.get("endpoint_owner") or {}).get("pid")

    def harness_02(self) -> None:
        """Break modes come only from an explicitly launched host daemon: launched with the mode it reports it; auto-started, launched without the flag, injected into, or run as a guest binary, it never does."""
        breaks_prefix = pathlib.Path(self.args.breaks_prefix).expanduser()
        normal = breaks_prefix if "normal-build-has-breaks" in self.harness_breaks else \
            pathlib.Path(self.args.normal_prefix).expanduser()
        if not (breaks_prefix / "bin" / "marsh").is_file():
            raise Blocked(f"blocked-on:H2: no test-breaks install at {breaks_prefix} "
                          f"(make dev DEV_FEATURES=test-breaks DEV_PREFIX={breaks_prefix}, from a checkout with H2)")
        check_script = self.h2_script("scripts/check-no-test-breaks.sh")
        guest_script = self.h2_script("tests/breaks/guest_env.sh")
        if not IMAGE.fullmatch(self.args.kit):
            raise Blocked("blocked-on:kit: c5's job injection runs a job and needs a fixture Kit image "
                          "(pass --kit a published fixture ref, e.g. `make fixture-ref`)")
        demo = ["demo"]
        # c1: a daemon the harness launches (with the flag) with the mode in its environment reports exactly it
        side = self.side_scope("breaks", breaks_prefix, modes=demo)
        try:
            status = self.side_status(side, "explicitly launched breaks daemon, MARSH_TEST_BREAK=demo")
            pid_one, launched_one = self.answering_pid(status), side.daemon.pid
            features = list(status.get("features") or [])
            agent = self.agent_report(side) if "services-v1" in features else None
        finally:
            self.stop_side(side)
        self.check("c1", pid_one == launched_one,
                   f"status was answered by pid {pid_one}, the process launched was pid {launched_one}"
                   + ("" if pid_one == launched_one else ": not the same daemon"))
        self.check("c1", status.get("test_breaks") == demo,
                   f"the explicitly launched breaks daemon (pid {pid_one}) reports test_breaks={status.get('test_breaks')!r}")
        # c2: a release marshd does not know the flag (it refuses it), and launched without the flag it ignores the
        # variable; neither ever reports a mode
        side = self.side_scope("normal-flag", normal)
        try:
            endpoint = daemon_endpoint_paths(side.home)[0]
            try:
                refused = self.xrun([str(side.marshd), "--home", str(side.home), "--test-breaks-from-env"],
                                    cwd=side.project, env={**side.environment, "MARSH_TEST_BREAK": "demo"}, timeout=30,
                                    note="normal marshd launched with --test-breaks-from-env and the variable")
            except Fail as error:
                raise CheckFailed("c2", f"the normal marshd accepted --test-breaks-from-env and kept running: {error}")
            self.check("c2", refused.returncode != 0 and "usage" in text(refused.stderr).lower() and not endpoint.exists(),
                       f"the normal marshd did not refuse --test-breaks-from-env ({refused.returncode}): "
                       f"{text(refused.stderr)[-200:]!r}")
        finally:
            self.stop_side(side)
        side = self.side_scope("normal", normal, modes=demo, flag=False)
        try:
            status = self.side_status(side, "normal marshd launched without the flag, MARSH_TEST_BREAK=demo in its environment")
            pid_two, launched_two = self.answering_pid(status), side.daemon.pid
        finally:
            self.stop_side(side)
        self.check("c2", pid_two == launched_two, f"the daemon answering status (pid {pid_two}) is not the one "
                                                  f"launched (pid {launched_two})")
        self.check("c2", "test_breaks" not in status,
                   f"the normal build (pid {pid_two}), launched with MARSH_TEST_BREAK=demo, reports "
                   f"test_breaks={status.get('test_breaks')!r}")
        # c3: H2's provenance check passes on every normal host and guest binary and fails on the breaks daemon
        binaries = [str(p) for p in sorted((normal / "bin").iterdir()) if p.is_file() and not p.is_symlink()]
        binaries += [str(p) for p in sorted((normal / "libexec" / "marsh").glob("*-linux-arm64"))]
        self.check("c3", len(binaries) >= 5, f"too few normal binaries to check: {binaries}")
        done = self.xrun(["sh", str(check_script), *binaries], timeout=300, note="H2 check-no-test-breaks.sh, normal")
        self.check("c3", done.returncode == 0, f"check-no-test-breaks.sh refused a normal binary ({done.returncode}): "
                                               f"{text(done.stderr)[-300:]!r}")
        control = self.xrun(["sh", str(check_script), str(breaks_prefix / "bin" / "marshd")], timeout=120,
                            note="the same check on the breaks marshd (must refuse)")
        self.check("c3", control.returncode != 0 and "test-breaks" in text(control.stderr),
                   "check-no-test-breaks.sh did not refuse the breaks marshd: the check proves nothing")
        # c4: with no daemon running a CLI-prefixed variable auto-starts a daemon that ignores it; a daemon launched
        # explicitly WITHOUT the flag ignores the variable in its environment
        side = self.side_scope("autostart", breaks_prefix)
        try:
            status = self.side_status(side, "CLI-prefixed MARSH_TEST_BREAK=demo, no daemon running",
                                      {"MARSH_TEST_BREAK": "demo"})
            auto_pid = self.answering_pid(status)
        finally:
            self.stop_side(side)
        self.check("c4", auto_pid is not None and auto_pid not in (pid_one, pid_two),
                   f"the auto-started daemon (pid {auto_pid}) must be its own process, not {pid_one}/{pid_two}")
        self.check("c4", "test_breaks" not in status,
                   f"the CLI auto-started daemon (pid {auto_pid}) honored MARSH_TEST_BREAK: {status.get('test_breaks')!r}")
        side = self.side_scope("noflag", breaks_prefix, modes=demo, flag=False)
        try:
            status = self.side_status(side, "marshd launched without the flag, MARSH_TEST_BREAK=demo in its environment")
            noflag_pid, launched = self.answering_pid(status), side.daemon.pid
        finally:
            self.stop_side(side)
        self.check("c4", noflag_pid == launched, f"the daemon answering (pid {noflag_pid}) is not the one launched "
                                                 f"without the flag (pid {launched})")
        self.check("c4", "test_breaks" not in status,
                   f"a daemon launched without --test-breaks-from-env (pid {noflag_pid}) honored the variable: "
                   f"{status.get('test_breaks')!r}")
        # c5: a daemon launched explicitly (with the flag) without modes stays clean under every injection, and the
        # guest binaries never read the variable
        side = self.side_scope("untrusted", breaks_prefix, modes=[], kit=self.args.kit)
        try:
            status = self.side_status(side, "explicitly launched breaks daemon, no modes")
            clean_pid = self.answering_pid(status)
            self.check("c5", clean_pid == side.daemon.pid and "test_breaks" not in status,
                       f"a daemon launched without modes (pid {clean_pid}) reports {status.get('test_breaks')!r}")
            self.injection(side, "job", clean_pid, [side.marsh, "run", "fixture", "exec", "sh", "-c", "{script}"],
                           probe="marsh jobs --json")
            self.injection(side, "shell", clean_pid, [side.marsh, "-c", "{script}"], {"MARSH_TEST_BREAK": "demo"},
                           probe="marsh status --json")
        finally:
            self.stop_side(side)
        guest = self.xrun(["sh", str(guest_script), str(breaks_prefix / "libexec" / "marsh")], timeout=900,
                          note="H2 tests/breaks/guest_env.sh on the breaks guest binaries")
        self.check("c5", guest.returncode == 0 and "PASS guest_env" in text(guest.stdout),
                   f"H2's guest_env.sh did not pass ({guest.returncode}): {text(guest.stdout + guest.stderr)[-400:]!r}")
        # c6: the services agent acknowledges (wave 1). Deferred only while the agent does not exist.
        if "services-v1" not in features:
            self.skip_check("c6", "services-v1 is not in `marsh status --json` features: `marsh services status --json` exists "
                                  "(S1, schema marsh.services/v1) but its projects carry no `test_breaks_agent` yet "
                                  "(wave 1 agent attestation); once the feature is advertised this check must run and "
                                  "may not be skipped")
        else:
            self.check("c6", agent == demo, f"services-v1 exists, so c6 must assert test_breaks_agent == {demo}, got "
                                            f"{agent!r} (start a services project for the daemon launched with demo)")

    def agent_report(self, side: SideScope) -> list[str] | None:
        done = self.xrun([side.marsh, "services", "status", "--json"], cwd=side.project, env=side.environment,
                         timeout=120, note="services status of the daemon launched with demo")
        try:
            for project in json.loads(done.stdout).get("projects") or []:
                if "test_breaks_agent" in project:
                    return list(project["test_breaks_agent"])
        except ValueError:
            pass
        return None

    def injection(self, side: SideScope, label: str, daemon_pid: int | None, argv: list[str],
                  env: dict[str, str] | None = None, probe: str = "") -> None:
        """Run a job or shell that sets MARSH_TEST_BREAK=demo in its OWN environment and proves it ran (a nonce it
        prints, and the variable visible in its own env), then ask the same daemon from the host: it must still be the
        same process and report no break. An injection that did not run proves nothing and fails. PROBE is a marsh
        call made from inside the injected environment (the in-VM client talking to the same daemon with the variable
        set): its output and its own exit status are printed between markers and both are checked."""
        nonce = secrets.token_hex(8)
        script = (f"MARSH_TEST_BREAK=demo; export MARSH_TEST_BREAK; echo nfx-{label}-ran-{nonce}; "
                  "env | grep '^MARSH_TEST_BREAK='"
                  + (f"; echo nfx-{label}-probe-{nonce}; {probe}; nfx_rc=$?; echo nfx-{label}-end-{nonce}:$nfx_rc"
                     if probe else ""))
        done = self.xrun([a.replace("{script}", script) for a in argv], cwd=side.project,
                         env={**side.environment, **(env or {})}, timeout=900, note=f"{label} injection")
        out = text(done.stdout)
        self.check("c5", done.returncode == 0 and f"nfx-{label}-ran-{nonce}" in out and "MARSH_TEST_BREAK=demo" in out,
                   f"the {label} injection did not run with the variable in its environment "
                   f"({done.returncode}): stdout={out[-200:]!r} stderr={text(done.stderr)[-200:]!r}")
        if probe:
            between, _, tail = out.partition(f"nfx-{label}-probe-{nonce}")[2].partition(f"nfx-{label}-end-{nonce}:")
            status = tail.split()[0] if tail.split() else None
            self.check("c5", status == "0", f"the {label}'s in-environment marsh call exited {status!r}, not 0: a failed "
                                            f"operation does not establish the injection boundary (output {between[-200:]!r})")
            try:
                document = json.loads(between.strip())
            except ValueError:
                document = None
            self.check("c5", isinstance(document, dict), f"the {label}'s in-environment marsh call returned no JSON "
                                                         f"object: {between[-200:]!r}")
            self.check("c5", "test_breaks" not in document and
                       (label != "job" or document.get("schema") == "marsh.jobs/v1"),
                       f"the {label}'s in-environment marsh call reports a break mode or is not the expected "
                       f"document: {sorted(document)}")
        after = self.side_status(side, f"daemon status after the {label} injection")
        self.check("c5", self.answering_pid(after) == daemon_pid,
                   f"after the {label} injection the answering daemon is pid {self.answering_pid(after)}, "
                   f"not the launched {daemon_pid}")
        self.check("c5", "test_breaks" not in after,
                   f"after the {label} injection the daemon (pid {daemon_pid}) reports test_breaks="
                   f"{after.get('test_breaks')!r}")

    # ---- HARNESS-03 -------------------------------------------------------------------------------
    def prepare_nfx_kits(self) -> None:
        """Register H1b's `nfx`, `nfx-acp` and `nfx-acp2` Kits in the scope's registry. The daemon reads the registry
        when it starts, so this runs before the guard launches it."""
        stage = self.fixtures / "nfx-kit" / "stage.py"
        if not stage.is_file():
            return
        staged = subprocess.run([sys.executable, str(stage)], capture_output=True, text=True, timeout=900, check=False)
        if staged.returncode != 0:
            raise Fail(f"nfx Kit staging failed: {staged.stderr[-300:]}")
        shutil.copy(staged.stdout.strip(), self.control_home / "commands.json")
        self.notes.append("nfx Kits registered from the fixtures' stage.py")

    def harness_03(self) -> None:
        """Fixture and fake-agent self-test (H1b's `harness03.REGISTRATION`, run against this scope)."""
        path = self.fixtures / "harness03.py"
        if not path.is_file():
            raise Blocked(f"blocked-on:H1b: {path} does not exist (merge H1b, or pass --fixtures-dir)")
        spec = importlib.util.spec_from_file_location("harness03", path)
        module = importlib.util.module_from_spec(spec)
        sys.path.insert(0, str(self.fixtures))
        try:
            spec.loader.exec_module(module)
        finally:
            sys.path.remove(str(self.fixtures))
        registration = getattr(module, "REGISTRATION", None)
        if not registration or registration.get("id") != "HARNESS-03" or not callable(registration.get("run")):
            raise Blocked("blocked-on:H1b: fixtures/harness03.py has no usable REGISTRATION")
        break_mode = "NFX_FAKE_EXEC=off" if self.defect == "NFX_FAKE_EXEC=off" else None
        result = registration["run"](_Context(self, break_mode))
        for name in ("c1", "c2", "c3", "c4", "c5"):
            entry = result["checks"].get(name) or {}
            self.recorder.checks.append({"id": name, "outcome": "passed" if entry.get("ok") else "failed",
                                         "detail": json.dumps(entry, default=str)[:1800]})
        if break_mode:
            if result.get("verdict") != "verified_break":
                raise Fail(f"invalid negative control: HARNESS-03 under {break_mode} returned "
                           f"{result.get('verdict')} (failed_check {result.get('failed_check')})")
            raise CheckFailed("c2", "the fake agent's exec verb is off, as the break requires")
        if result.get("failed_check"):
            raise CheckFailed(result["failed_check"], json.dumps(result["checks"][result["failed_check"]],
                                                                 default=str)[:600])


class _Proc:
    def __init__(self, done: subprocess.CompletedProcess[bytes]) -> None:
        self.returncode, self.stdout, self.stderr = done.returncode, text(done.stdout), text(done.stderr)


class _Sandbox:
    def __init__(self, suite: Any, name: str) -> None:
        self.suite, self.name = suite, name

    def run(self, argv: list[str], timeout: float) -> _Proc:
        return _Proc(self.suite.xrun([self.suite.sbx, "exec", self.name, *argv], timeout=timeout))


class _Context:
    """The duck-typed CTX H1b's `harness03.run(ctx)` expects (fixtures/README.md), backed by this suite."""

    def __init__(self, suite: Any, break_mode: str | None) -> None:
        self.suite = suite
        self.sbx = suite.sbx
        self.workdir = suite.root
        self.project = suite.project
        self.break_mode = break_mode

    def marsh_run(self, argv: list[str], timeout: float) -> _Proc:
        return _Proc(self.suite.xrun([self.suite.marsh, *argv], timeout=timeout))

    def agent_exec(self, agent: str, script: str, timeout: float) -> list[str]:
        """One ACP prompt `exec SCRIPT` to the fake agent registered as AGENT; every read is bounded by TIMEOUT."""
        suite = self.suite
        process = subprocess.Popen([suite.marsh, "run", "--acp", agent], cwd=suite.project, env=suite.environment,
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                   process_group=0)
        deadline = time.monotonic() + timeout
        selector = selectors.DefaultSelector()
        selector.register(process.stdout, selectors.EVENT_READ)
        buffer, texts = b"", []

        def send(message: dict[str, Any]) -> None:
            process.stdin.write((json.dumps({"jsonrpc": "2.0", **message}) + "\n").encode())
            process.stdin.flush()

        try:
            send({"id": 1, "method": "initialize", "params": {"protocolVersion": 1, "clientCapabilities": {}}})
            send({"id": 2, "method": "session/new", "params": {"cwd": str(suite.project), "mcpServers": []}})
            finished = False
            while not finished:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(f"ACP prompt exceeded {timeout}s")
                if not selector.select(min(remaining, 1.0)):
                    continue
                chunk = os.read(process.stdout.fileno(), 65536)
                if not chunk:
                    raise RuntimeError("the ACP server closed before the prompt finished")
                buffer += chunk
                while b"\n" in buffer:
                    line, buffer = buffer.split(b"\n", 1)
                    if not line.strip():
                        continue
                    message = json.loads(line)
                    if message.get("id") in (1, 2, 3) and "error" in message:
                        raise RuntimeError(f"ACP request {message['id']} failed: {json.dumps(message['error'])[:200]}")
                    if message.get("id") == 2 and "result" in message:
                        send({"id": 3, "method": "session/prompt", "params": {
                            "sessionId": message["result"]["sessionId"],
                            "prompt": [{"type": "text", "text": f"exec {script}"}]}})
                    elif message.get("method") == "session/update":
                        update = message["params"]["update"]
                        if update.get("sessionUpdate") == "agent_message_chunk":
                            texts.append(update["content"]["text"])
                    elif message.get("id") == 3:
                        stop = (message.get("result") or {}).get("stopReason")
                        if stop != "end_turn":
                            raise RuntimeError(f"the ACP prompt ended with stopReason {stop!r}, not end_turn")
                        finished = True
                        break
        finally:
            selector.close()
            with contextlib.suppress(OSError):
                process.stdin.close()
            try:
                process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                reap(process)
            process.stdout.close()
        return texts

    @contextlib.contextmanager
    def sandbox(self, role: str, workspace: pathlib.Path) -> Iterator[_Sandbox]:
        suite = self.suite
        name = f"nfx-{suite.run_id}-{role}"
        try:
            suite.create_sandbox(name, workspace)
        except Fail as error:  # H1b's context expects an exception from a failed create
            raise RuntimeError(str(error)) from None
        try:
            yield _Sandbox(suite, name)
        finally:
            proof = suite.proven_uuid(name)
            if proof:
                with contextlib.suppress(ValueError):
                    remove_sandbox(suite.sbx, name, proof, suite.stock_before, env=suite.environment)
