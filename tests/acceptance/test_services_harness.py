#!/usr/bin/env python3
"""The services E2E harness's own decision tables (tests/acceptance/services/): the catalogue, the VM lock,
the observer's read-only guard and audit rules, evidence validation and the host-state checks. The scenarios
themselves run on a Mac against stock SBX (`make services-e2e`)."""

from __future__ import annotations

import contextlib
import dataclasses
import fcntl
import json
import os
import pathlib
import signal
import subprocess
import sys
import tempfile
import time
import unittest
import unittest.mock

DIRECTORY = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(DIRECTORY))
sys.path.insert(0, str(DIRECTORY / "services"))

import breaks  # noqa: E402
import evidence  # noqa: E402
import holds  # noqa: E402
import nfx_gc  # noqa: E402
import nfx_host  # noqa: E402
import nfx_lock  # noqa: E402
import observer  # noqa: E402
import scopes  # noqa: E402
import services_uat  # noqa: E402
import truth as truth_module  # noqa: E402
import witness  # noqa: E402
from catalogue import BY_ID, CATALOGUE, HOLD_POINTS  # noqa: E402

SERVICES = DIRECTORY / "services" / "services_uat.py"


def _closed(descriptor: int) -> bool:
    try:
        os.fstat(descriptor)
    except OSError:
        return True
    return False

PLAN = json.loads((DIRECTORY / "services" / "plan-r3.3-breaks.json").read_text())


class CatalogueTests(unittest.TestCase):
    def test_every_scenario_of_test_plan_c_is_registered_and_listed(self) -> None:
        counts: dict[str, int] = {}
        for entry in CATALOGUE:
            family = entry.id.rsplit("-", 1)[0]
            counts[family] = counts.get(family, 0) + 1
        self.assertEqual(counts, {"HARNESS": 3, "SVC-VM": 10, "SVC-EP": 10, "SVC-SEC": 24, "SVC-CAP": 12,
                                  "SVC-RST": 13, "SVC-SUPA": 4, "NW": 9, "REL": 1, "DOC": 1, "PERF": 11})
        listed = subprocess.run([sys.executable, str(SERVICES), "--list"], capture_output=True, text=True,
                                check=True, stdin=subprocess.DEVNULL).stdout.splitlines()
        self.assertEqual([line.split("\t")[0] for line in listed], [entry.id for entry in CATALOGUE])
        status = {line.split("\t")[0]: line.split("\t")[1] for line in listed}
        self.assertEqual(status["HARNESS-01"], "implemented")
        self.assertTrue(status["HARNESS-02"].startswith("blocked-on:H2"))
        self.assertTrue(all(v == "pending" for k, v in status.items()
                            if k not in ("HARNESS-01", "HARNESS-02", "HARNESS-03")))

    def test_smoke_set_is_the_plan_b_t1_row(self) -> None:
        smoke = {entry.id for entry in CATALOGUE if entry.smoke}
        self.assertEqual(smoke, {"HARNESS-01", "HARNESS-02", "HARNESS-03", "SVC-EP-01", "SVC-EP-03", "SVC-SEC-01",
                                 "SVC-CAP-01", "SVC-RST-01", "SVC-RST-09", "NW-04"})

    def test_break_cells_name_one_check_and_a_known_kind(self) -> None:
        cells = list(breaks.list_cells())
        self.assertGreater(len(cells), 120)
        for cell in cells:
            with self.subTest(cell=cell):
                self.assertRegex(cell["failed_check"], r"^(c\d+|perf)$")
                self.assertIn(cell["kind"], {"marsh", "narwhal", "harness", "fixture", "patch", "positive-control"})
                for need in cell["prerequisites"]:  # holds and faults, plus PERF-10's paired control
                    self.assertTrue(need.startswith(("hold-", "fault-")) or need == "slow-freeze-long", cell)

    def test_the_catalogue_matches_an_independent_revision_3_3_fixture(self) -> None:
        """plan-r3.3-breaks.json was parsed from the plan's §A.6 table by code that shares nothing with the catalogue
        generator, and its prerequisites and variants were audited by hand against each scenario's text."""
        cells = {(c["scenario"], c["mode"], c["failed_check"]) for c in breaks.list_cells() if c["kind"] == "marsh"}
        self.assertEqual(cells, {tuple(c) for c in PLAN["cells"]})
        needs = {(c["scenario"], c["mode"]): sorted(c["prerequisites"]) for c in breaks.list_cells()
                 if c["kind"] == "marsh" and c["prerequisites"]}
        self.assertEqual(needs, {(s, m, ): sorted(p) for s, m, p in PLAN["prerequisites"]} if False else
                         {(s, m): sorted(p) for s, m, p in PLAN["prerequisites"]})
        variants = {(c["scenario"], c["mode"]): c["variant"] for c in breaks.list_cells() if c["variant"]}
        self.assertEqual(variants, {(s, m): v for s, m, v in PLAN["variants"]})
        # the defects r3.1 and r3.2 changed
        modes = {m for _, m, _ in cells}
        for gone in ("shell-forward-cross-project", "restore-share-volumes", "capture-skip-unpause", "internet-noop"):
            self.assertNotIn(gone, modes)
        for new in ("shell-forward-last-claimant", "reset-skip-confirmation", "reset-include-services-noop",
                    "services-stop-deletes-data"):
            self.assertIn(new, modes)
        self.assertIn(("SVC-CAP-03", "capture-allow-overlap", "c3"), cells)
        self.assertEqual(BY_ID["SVC-SEC-12"].breaks[0].prerequisites, ("fault-agent-override",))

    def test_every_hold_a_cell_needs_is_registered(self) -> None:
        for cell in breaks.list_cells():
            for need in cell["prerequisites"]:
                if need.startswith("hold-"):
                    self.assertIn(need.removeprefix("hold-"), HOLD_POINTS)

    def test_list_breaks_prints_one_json_cell_per_line(self) -> None:
        out = subprocess.run([sys.executable, str(SERVICES), "--list-breaks"], capture_output=True, text=True,
                             check=True, stdin=subprocess.DEVNULL).stdout.splitlines()
        self.assertEqual([json.loads(line) for line in out], list(breaks.list_cells()))

    def test_the_paired_perf_10_control_is_one_cell_that_can_be_inferred(self) -> None:
        cell = next(c for c in breaks.list_cells() if c["scenario"] == "PERF-10")
        self.assertEqual(cell["mode"], "capture-block-next-prompt")
        self.assertEqual(cell["expected_modes"]["daemon"], ["capture-block-next-prompt", "slow-freeze-long"])
        self.assertEqual(breaks.expected_checks(BY_ID["PERF-10"], "capture-block-next-prompt"), {"perf"})


class BreakProtocolTests(unittest.TestCase):
    def test_break_run_needs_exactly_the_requested_modes_and_normal_run_needs_none(self) -> None:
        guard = breaks.guard_error
        self.assertIsNone(guard({}, []))
        self.assertIsNone(guard({"features": []}, []))
        self.assertIsNone(guard({"test_breaks": ["demo"]}, ["demo"]))
        self.assertIsNone(guard({"test_breaks": ["b", "a"]}, ["a", "b"]))
        for status, requested in [({"test_breaks": ["demo"]}, []), ({"test_breaks": []}, []), ({"test_breaks": None}, []),
                                  ({}, ["demo"]), ({"test_breaks": None}, ["demo"]), ({"test_breaks": []}, ["demo"]),
                                  ({"test_breaks": ["demo"]}, ["demo", "x"]), ({"test_breaks": ["demo", "x"]}, ["demo"]),
                                  ({"test_breaks": "demo"}, ["demo"]), ({"test_breaks": ["demo", "demo"]}, ["demo"]),
                                  ({"test_breaks": [1]}, ["demo"])]:
            with self.subTest(status=status, requested=requested):
                self.assertIn("refusing", guard(status, requested))
        agent = breaks.agent_break_error
        self.assertIsNone(agent({"test_breaks_agent": ["demo"]}, ["demo"]))
        self.assertIsNone(agent({}, []))
        self.assertIn("no `test_breaks_agent`", agent({}, ["demo"]))
        self.assertIn("refusing", agent({"test_breaks_agent": None}, []))
        self.assertIn("refusing", agent({"test_breaks_agent": ["other"]}, ["demo"]))

    def test_validate_break_demands_the_complete_cell(self) -> None:
        parser = services_uat.build_parser()

        def parse(*argv: str):
            arguments = parser.parse_args(["--only", "SVC-CAP-02", *argv])
            services_uat.validate_break(parser, arguments)
            return arguments

        arguments = parse("--break", "capture-skip-unpause-on-error", "--variant", "b")
        self.assertEqual(arguments.break_modes, "capture-skip-unpause-on-error,fault-capture-fail-after-pause")
        self.assertEqual(arguments.break_defect, "capture-skip-unpause-on-error")
        for argv in (["--break", "capture-skip-unpause-on-error,hold-capture-frozen", "--variant", "b"],  # wrong prerequisite
                     ["--break", "capture-skip-unpause-on-error,fault-capture-fail-after-pause,unpause-recovery-off",
                      "--break-defect", "capture-skip-unpause-on-error"],  # an extra defect
                     ["--break", "hold-capture-frozen", "--break-defect", "unpause-recovery-off"],  # defect not activated
                     ["--break", "port-nonsense"]):  # not registered
            with self.subTest(argv=argv), self.assertRaises(SystemExit):
                parse(*argv)
        parse("--break", "hold-capture-frozen")  # pass 2: prerequisites only
        with self.assertRaises(SystemExit):
            services_uat.validate_break(parser, parser.parse_args(["--break", "demo"]))  # --only is required

    def test_revision_3_3_registers_disk_size_env_ignored_and_retires_the_setting_hold_and_break(self) -> None:
        cell = [b for b in BY_ID["SVC-VM-10"].breaks if b.check == "c2"]
        self.assertEqual([(b.mode, b.prerequisites) for b in cell], [("disk-size-env-ignored", ())])
        self.assertNotIn("disk-setting-set", HOLD_POINTS)
        for retired in ("hold-disk-setting-set", "disk-setting-recovery-off"):
            self.assertNotIn(retired, json.dumps([PLAN["cells"], PLAN["prerequisites"]]))
            self.assertNotIn(retired, " ".join(c["mode"] + " ".join(c["prerequisites"]) for c in breaks.list_cells()))
        parser = services_uat.build_parser()
        arguments = parser.parse_args(["--tier", "break", "--only", "SVC-VM-10", "--break", "disk-size-env-ignored"])
        services_uat.validate_break(parser, arguments)  # the actual CLI accepts the cell
        with self.assertRaises(SystemExit):
            services_uat.validate_break(parser, parser.parse_args(["--tier", "break", "--only", "SVC-VM-10", "--break",
                                                                   "disk-setting-recovery-off"]))

    def test_a_pass_3_child_that_exits_nonzero_is_never_verified(self) -> None:
        verdict = services_uat.pass3_result
        self.assertEqual(verdict(0, "services: break verdict A m: verified\n", "A", "m"), "verified")
        self.assertEqual(verdict(1, "services: break verdict A m: verified\n", "A", "m"), "invalid-negative-control")
        self.assertEqual(verdict(-9, "timed out", "A", "m"), "invalid-negative-control")
        self.assertEqual(verdict(0, "no verdict line", "A", "m"), "invalid-negative-control")
        self.assertEqual(verdict(1, "services: break verdict A m: not-detected\n", "A", "m"), "not-detected")

    def test_modes_classify_and_a_single_defect_is_inferred(self) -> None:
        modes = breaks.normalize_modes("hold-capture-frozen, capture-skip-unpause,fault-capture-fail,,")
        self.assertEqual(breaks.classify(modes), {"holds": ["hold-capture-frozen"],
                                                  "faults": ["fault-capture-fail"], "companions": [],
                                                  "defects": ["capture-skip-unpause"]})
        # PERF-10's +3 s freeze rides along with its defect; it is a companion, not a second defect
        perf = ["capture-block-next-prompt", "slow-freeze-long"]
        self.assertEqual(breaks.classify(perf)["companions"], ["slow-freeze-long"])
        self.assertEqual(breaks.infer_defect(perf), "capture-block-next-prompt")
        self.assertEqual(breaks.infer_defect(modes), "capture-skip-unpause")
        self.assertIsNone(breaks.infer_defect(["hold-capture-frozen", "fault-x"]))
        self.assertIsNone(breaks.infer_defect(["a", "b"]))
        self.assertEqual(breaks.marsh_modes(["leak-on-purpose", "hold-x", "ports-off"]), ["hold-x", "ports-off"])

    def test_a_break_verifies_only_at_the_named_check(self) -> None:
        entry = BY_ID["SVC-RST-03"]
        self.assertEqual(breaks.expected_checks(entry, "restore-share-uploads"), {"c2"})
        self.assertEqual(breaks.expected_checks(entry, "lane-ports-unmapped"), {"c1"})
        self.assertEqual(breaks.expected_checks(BY_ID["HARNESS-01"], "restore-share-volumes"), set())
        cap = BY_ID["SVC-CAP-02"]
        self.assertEqual(breaks.expected_checks(cap, "unpause-recovery-off", "c"), {"c3"})
        self.assertEqual(breaks.prerequisites(BY_ID["SVC-CAP-11"], "snapshot-publish-early"),
                         ["hold-capture-before-publish"])
        self.assertEqual(breaks.prerequisites(BY_ID["SVC-CAP-11"], "capture-api-gate-off"),
                         ["hold-capture-frozen"])

    def test_verdicts(self) -> None:
        verdict = breaks.verdict
        expected = {"c2"}
        for args, want in [
            (("m", ["m"], "failed", "c2", expected), "verified"),
            (("m", ["m"], "failed", "c1", expected), "invalid-negative-control"),
            (("m", ["m"], "failed", None, expected), "invalid-negative-control"),  # setup, error or timeout
            (("m", ["m"], "blocked", None, expected), "invalid-negative-control"),
            (("m", ["m"], "passed", None, expected), "not-detected"),
            ((None, ["hold-x"], "passed", None, set()), "pass-2-ok"),
            ((None, ["hold-x"], "failed", "c1", set()), "baseline-failed"),
            ((None, [], "passed", None, set()), "normal")]:
            with self.subTest(args=args):
                self.assertEqual(verdict(*args, breaks_build=False), want)
        self.assertEqual(verdict(None, [], "passed", None, set(), breaks_build=True), "pass-2-ok")


class HoldTests(unittest.TestCase):
    def test_daemon_holds_are_files_under_marsh_home_and_need_their_mode(self) -> None:
        home = pathlib.Path(tempfile.mkdtemp(prefix="nfx-hold-test-"))
        self.addCleanup(lambda: __import__("shutil").rmtree(home, ignore_errors=True))
        points = holds.HoldPoints(home, ["hold-lane-files-write"])
        with self.assertRaisesRegex(holds.HoldError, "not among the modes"):
            points.release("restore-after-lease")
        with self.assertRaisesRegex(holds.HoldError, "unknown hold point"):
            points.release("nope")
        self.assertFalse(points.reached("lane-files-write"))
        (home / "test-holds").mkdir()
        (home / "test-holds" / "lane-files-write.reached").touch()
        self.assertTrue(points.reached("lane-files-write"))
        points.wait_reached("lane-files-write", 1)
        points.release("lane-files-write")
        self.assertTrue((home / "test-holds" / "lane-files-write.release").exists())
        points.clear("lane-files-write")
        self.assertEqual(sorted(p.name for p in (home / "test-holds").iterdir()), [])
        with self.assertRaisesRegex(holds.HoldError, "within"):
            points.wait_reached("lane-files-write", 0.2)

    def test_agent_holds_live_inside_the_engine_container(self) -> None:
        calls: list[str] = []
        points = holds.HoldPoints(pathlib.Path("/nonexistent"), ["hold-capture-frozen"],
                                  lambda command, write: (calls.append(command) or (0, ""), (0, ""))[1])
        self.assertTrue(points.reached("capture-frozen"))
        points.release("capture-frozen")
        points.clear("capture-frozen")
        self.assertEqual(calls[0], "test -e /run/marsh-test-holds/capture-frozen.reached")
        self.assertIn("touch /run/marsh-test-holds/capture-frozen.release", calls[1])
        self.assertIn("rm -f /run/marsh-test-holds/capture-frozen.reached", calls[2])
        with self.assertRaisesRegex(holds.HoldError, "belongs to narwhal"):
            holds.HoldPoints(pathlib.Path("/x"), ["hold-removal"]).release("removal")


@unittest.skipUnless(nfx_lock.helper_path().is_file(), "the shared VM lock helper (NFX_VMLOCK) is not on this machine")
class LockTests(unittest.TestCase):
    """The lock is the shared helper's flock and nothing else (captain's VM lock rules): the kernel decides, nothing is
    stolen or recovered, the lock file is never deleted, renamed, truncated or replaced, `lock.info` is display, and a
    holder keeps the lock until its child has been reaped. Scratch lock files only, through VMLOCK_TEST_PATH."""

    def setUp(self) -> None:
        self.directory = pathlib.Path(tempfile.mkdtemp(prefix="nfx-lock-test-"))
        self.addCleanup(lambda: __import__("shutil").rmtree(self.directory, ignore_errors=True))
        self.path = self.directory / "lock"
        self.path.touch()  # tools never create a lock file
        self.env = {**os.environ, "VMLOCK_TEST_PATH": str(self.path)}
        patcher = unittest.mock.patch.dict(os.environ, {"VMLOCK_TEST_PATH": str(self.path)})
        patcher.start()
        self.addCleanup(patcher.stop)

    @staticmethod
    def dead_pid() -> int:
        process = subprocess.Popen(["/usr/bin/true"])
        process.wait()
        return process.pid

    def holder(self, *command: str, run: str = "holder") -> subprocess.Popen:
        process = subprocess.Popen([sys.executable, str(nfx_lock.helper_path()), "--run", run, "--", *command],
                                   env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        self.addCleanup(lambda: (process.poll() is None and os.killpg(process.pid, signal.SIGKILL),
                                 process.communicate()))
        for _ in range(100):
            record = nfx_lock.holder_record(self.path)
            if record and record.get("pid") == process.pid:
                return process
            time.sleep(0.1)
        self.fail("the holder never took the lock")

    def test_the_harness_uses_the_shared_helper_and_has_no_lock_logic_of_its_own(self) -> None:
        self.assertEqual(nfx_lock.lock_path(), self.path)  # VMLOCK_TEST_PATH is honored, nothing else decides the path
        for name in ("nfx_lock.py", "services_uat.py", "scenarios_harness.py", "lock_contender.py"):
            import io
            import tokenize
            text = (DIRECTORY / "services" / name).read_text()
            source = " ".join(t.string for t in tokenize.generate_tokens(io.StringIO(text).readline)
                              if t.type not in (tokenize.COMMENT, tokenize.STRING, tokenize.FSTRING_MIDDLE))
            for forbidden in ("fcntl", "flock", "O_TRUNC", "holder_is_dead", "process_token", "HarnessLock", "lock.recover"):
                if name == "nfx_lock.py" and forbidden in ("fcntl", "flock"):
                    continue  # `hold_proof` makes ONE non-blocking shared-lock probe: an observation, never an acquisition
                self.assertNotIn(forbidden, source, f"{name} mentions {forbidden}")
            if name == "nfx_lock.py":
                self.assertEqual(source.count("flock"), 1)
                self.assertIn("fcntl . flock ( descriptor , fcntl . LOCK_SH | fcntl . LOCK_NB )", source)
                self.assertNotIn("LOCK_EX", source)
        with unittest.mock.patch.dict(os.environ, {"NFX_VMLOCK": str(self.directory / "missing.py")}):
            with self.assertRaises(nfx_lock.LockUnavailable):
                nfx_lock.hold("x", 0)

    def test_second_holder_is_refused_naming_the_first_from_lock_info(self) -> None:
        holder = self.holder("sleep", "30", run="run1")
        record = nfx_lock.holder_record(self.path)
        self.assertEqual((record["pid"], record["run"]), (holder.pid, "run1"))
        self.assertEqual(self.path.read_bytes(), b"")  # the lock file itself is never written
        with self.assertRaises(nfx_lock.LockHeld) as held:
            nfx_lock.hold("run2", 0).__enter__()
        self.assertIn(f'"pid": {holder.pid}', str(held.exception))
        self.assertIn("display only", str(held.exception))

    def test_a_live_holder_is_never_displaced_even_if_lock_info_names_a_dead_pid(self) -> None:
        holder = self.holder("sleep", "60", run="live")
        self.path.write_text("older tool wrote this into the lock file")
        info = nfx_lock.info_path(self.path)
        info.write_text(json.dumps({"pid": self.dead_pid(), "run": "old", "mode": "exclusive"}))
        before = (self.path.stat().st_ino, self.path.read_bytes())
        for _ in range(3):
            with self.assertRaises(nfx_lock.LockHeld) as held:
                nfx_lock.hold("new", 0).__enter__()
            self.assertIn('"run": "old"', str(held.exception))  # reported from lock.info, as display
        with self.assertRaises(nfx_lock.LockHeld):  # also with a wait: it times out, it does not take over
            nfx_lock.hold("new", 1.2).__enter__()
        cli = subprocess.run([sys.executable, str(nfx_lock.helper_path()), "--timeout", "1", "--", "true"], env=self.env,
                             capture_output=True, text=True, stdin=subprocess.DEVNULL)
        self.assertEqual(cli.returncode, 75)
        self.assertEqual((self.path.stat().st_ino, self.path.read_bytes()), before)  # not replaced or truncated
        self.assertIsNone(holder.poll())
        os.kill(holder.pid, 0)  # the holder was never signalled
        self.assertEqual(json.loads(info.read_text())["run"], "old")  # lock.info untouched

    def test_a_lock_nobody_holds_is_taken_whatever_lock_info_says_and_the_file_is_untouched(self) -> None:
        self.path.write_text("older tool text")
        nfx_lock.info_path(self.path).write_text(json.dumps({"pid": self.dead_pid(), "run": "old"}))
        before = (self.path.stat().st_ino, self.path.read_bytes())
        with nfx_lock.hold("new", 0):
            self.assertEqual(nfx_lock.holder_record(self.path)["run"], "new")
            self.assertEqual((self.path.stat().st_ino, self.path.read_bytes()), before)
        self.assertEqual((self.path.stat().st_ino, self.path.read_bytes()), before)

    def test_a_missing_lock_file_is_an_error_and_is_never_created(self) -> None:
        self.path.unlink()
        with self.assertRaises(nfx_lock.LockUnavailable):
            nfx_lock.hold("x", 0)
        with self.assertRaises(nfx_lock.LockUnavailable):
            nfx_lock.supervise(["true"], "x", 0)
        self.assertFalse(self.path.exists())

    def test_two_contenders_for_one_free_lock_never_both_win(self) -> None:
        contender = DIRECTORY / "services" / "lock_contender.py"
        for attempt in range(3):
            self.path.write_text("")
            nfx_lock.info_path(self.path).write_text(json.dumps({"pid": self.dead_pid(), "run": "old"}))
            gate = self.directory / f"go{attempt}"
            runners = [subprocess.Popen([sys.executable, str(contender), str(gate)], env=self.env,
                                        stdout=subprocess.PIPE, text=True) for _ in range(3)]
            time.sleep(0.5)
            gate.write_text("go")
            results = sorted(r.communicate(timeout=60)[0].strip() for r in runners)
            self.assertEqual(results, ["acquired", "held", "held"], f"attempt {attempt}: {results}")

    CHILD = ("import os,signal,sys,time\nopen(sys.argv[1]+'.pid','w').write(str(os.getpid()))\n"
             "signal.signal(signal.SIGTERM, lambda *a: open(sys.argv[1]+'.term','w').write('x'))\n"
             "time.sleep(float(sys.argv[2]))\nopen(sys.argv[1]+'.done','w').write('x')")

    def wait_for(self, path: pathlib.Path) -> None:
        for _ in range(100):
            if path.exists():
                return
            time.sleep(0.1)
        self.fail(f"{path.name} never appeared")

    def test_a_holder_keeps_the_lock_while_its_child_works_after_sigterm(self) -> None:
        marker = self.directory / "child"
        holder = self.holder(sys.executable, "-c", self.CHILD, str(marker), "4")
        self.wait_for(self.directory / "child.pid")
        holder.send_signal(signal.SIGTERM)
        self.wait_for(self.directory / "child.term")  # forwarded to the child, which keeps working
        with self.assertRaises(nfx_lock.LockHeld):
            nfx_lock.hold("probe", 0).__enter__()
        self.assertFalse((self.directory / "child.done").exists())
        holder.communicate(timeout=60)
        self.assertTrue((self.directory / "child.done").exists())
        with nfx_lock.hold("probe", 0):  # reaped: free again
            pass

    def test_negative_control_a_holder_that_dies_on_sigterm_frees_the_lock_under_live_work(self) -> None:
        marker = self.directory / "bad"
        script = ("import subprocess,sys\nsys.path.insert(0,%r)\nimport nfx_lock\n"
                  "with nfx_lock.hold('bad', 0):\n    subprocess.Popen([sys.executable,'-c',sys.argv[1],sys.argv[2],'4'])\n"
                  "    import time; time.sleep(60)" % str(DIRECTORY / "services"))
        victim = subprocess.Popen([sys.executable, "-c", script, self.CHILD, str(marker)], env=self.env,
                                  start_new_session=True)
        self.wait_for(self.directory / "bad.pid")
        child_pid = int((self.directory / "bad.pid").read_text())
        victim.send_signal(signal.SIGTERM)
        victim.wait(timeout=30)
        with nfx_lock.hold("probe", 0):  # the lock is free although the child still works: the defect the probe detects
            self.assertFalse((self.directory / "bad.done").exists())
        os.kill(child_pid, signal.SIGKILL)  # our own control child

    SERVICES_DIR = str(DIRECTORY / "services")

    def worker(self, body: str, run: str = "r1", *extra: str) -> subprocess.Popen:
        """A python worker started as the child of the helper's CLI for RUN (scratch lock): it prints what BODY prints."""
        script = f"import sys,os,json\nsys.path.insert(0,{self.SERVICES_DIR!r})\nimport services_uat, nfx_lock\n{body}"
        process = subprocess.Popen(nfx_lock.helper_command(run, 30, [sys.executable, "-c", script, *extra]), env=self.env,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, process_group=0)
        self.addCleanup(lambda: (process.poll() is None and os.killpg(process.pid, signal.SIGKILL), process.communicate()))
        return process

    def test_the_worker_proves_a_live_holder_from_kernel_facts_and_a_forged_claim_proves_nothing(self) -> None:
        proof = self.worker("print(json.dumps(nfx_lock.hold_proof('r1')))", "r1")
        out, _ = proof.communicate(timeout=60)
        self.assertEqual(json.loads(out), [])  # parent is the helper for r1, the lock is held, leader of its session
        wrong = self.worker("print(json.dumps(nfx_lock.hold_proof('another-run')))", "r1")
        problems = json.loads(wrong.communicate(timeout=60)[0])
        self.assertTrue(any("not the shared lock helper started for run another-run" in p for p in problems), problems)
        self.assertTrue(any("lock's info does not name" in p for p in problems), problems)
        # the forged case: a process that merely claims it was started by a holder (this test process is its parent)
        forged = subprocess.run([sys.executable, "-c", f"import sys,json\nsys.path.insert(0,{self.SERVICES_DIR!r})\n"
                                 "import nfx_lock\nprint(json.dumps(nfx_lock.hold_proof('forged')))"],
                                env={**self.env, nfx_lock.WORKER_RUN: "forged"}, capture_output=True, text=True, timeout=60)
        problems = json.loads(forged.stdout)
        self.assertGreaterEqual(len(problems), 3, problems)  # no helper parent, nobody holds this scratch lock, no info
        self.assertTrue(any("nobody holds the VM lock" in p for p in problems), problems)

    def test_a_forged_worker_claim_cannot_run_gc_while_a_live_holder_holds_the_lock(self) -> None:
        holder = self.holder("sleep", "60", run="live-holder")
        sbx = self.directory / "sbx"
        log = self.directory / "sbx.log"
        sbx.write_text(f"#!/bin/sh\necho \"$@\" >> '{log}'\nexit 0\n")
        sbx.chmod(0o755)
        runs = self.directory / "runs"
        runs.mkdir()
        record = nfx_gc.RunRecord(runs, "250101000000beef", "smoke", self.directory / "root")
        record.data["created"] = {"nfx-250101000000beef-x": "id1"}
        record.save()
        command = [sys.executable, str(SERVICES), "--gc", "--lock-wait", "0", "--sbx", str(sbx)]
        plain = subprocess.run(command, env=self.env, capture_output=True, text=True, timeout=60, stdin=subprocess.DEVNULL)
        self.assertEqual(plain.returncode, 4, plain.stderr)  # ordinary invocation: refused, the lock is held
        for forged in ({nfx_lock.WORKER_RUN: "forged"}, {nfx_lock.WORKER_RUN: "forged", "NFX_LOCKED_BY": str(os.getpid())}):
            done = subprocess.run(command, env={**self.env, **forged}, capture_output=True, text=True, timeout=60,
                                  stdin=subprocess.DEVNULL)
            self.assertEqual(done.returncode, 2, done.stderr)  # fails closed
            self.assertIn("refusing to run", done.stderr)
        self.assertFalse(log.exists(), "a forged claim reached sbx")
        self.assertTrue((runs / "250101000000beef.json").exists())  # the record was neither used nor cleared
        self.assertIsNone(holder.poll())

    def test_services_uat_runs_its_body_as_the_helpers_child_and_the_lock_outlives_a_signal_to_the_launcher(self) -> None:
        marker = self.directory / "uat"
        original = [sys.executable, "-c", self.CHILD, str(marker), "4"]
        runner = (f"import sys\nsys.path.insert(0,{self.SERVICES_DIR!r})\nimport services_uat\n"
                  "args = type('A', (), {'run': 'uatrun', 'lock_wait': 0})()\n"
                  f"sys.exit(services_uat.reexec_locked(args, [], worker={original!r}))")
        launcher = subprocess.Popen([sys.executable, "-c", runner], env=self.env, process_group=0,
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.addCleanup(lambda: (launcher.poll() is None and os.killpg(launcher.pid, signal.SIGKILL), launcher.communicate()))
        self.wait_for(self.directory / "uat.pid")
        record = nfx_lock.holder_record(self.path)
        self.assertEqual(record["run"], "uatrun")
        self.assertNotEqual(record["pid"], launcher.pid)  # the helper's CLI holds it, not the launcher
        launcher.send_signal(signal.SIGTERM)
        self.wait_for(self.directory / "uat.term")  # forwarded launcher -> helper -> child
        with self.assertRaises(nfx_lock.LockHeld):
            nfx_lock.hold("probe", 0).__enter__()
        self.assertEqual(launcher.communicate(timeout=60)[0], b"")
        self.assertTrue((self.directory / "uat.done").exists())
        with nfx_lock.hold("probe", 0):
            pass

    def test_nothing_the_harness_spawns_leaves_the_helpers_session(self) -> None:
        import io
        import tokenize
        for path in sorted((DIRECTORY / "services").glob("*.py")):
            source = path.read_text()
            code = " ".join(t.string for t in tokenize.generate_tokens(io.StringIO(source).readline)
                            if t.type not in (tokenize.COMMENT, tokenize.STRING, tokenize.FSTRING_MIDDLE))
            for forbidden in ("start_new_session", "setsid", "preexec_fn", "daemonize"):
                self.assertNotIn(forbidden, code, f"{path.name} spawns outside the helper's session ({forbidden})")

    def test_a_worker_crash_does_not_release_the_lock_under_the_harnesss_own_children(self) -> None:
        marker = self.directory / "grandchild"
        body = ("import pathlib, threading, tempfile\n"
                "from evidence import Recorder\n"
                "suite = object.__new__(services_uat.Services)\n"
                "suite.marsh, suite.scope_started = 'marsh', False\n"
                "suite.project = pathlib.Path(tempfile.gettempdir()); suite.host_env = dict(os.environ)\n"
                "suite.recorder = Recorder()\n"
                "threading.Timer(1.0, lambda: os._exit(9)).start()  # the worker crashes mid-command\n"
                "child = 'import os,sys,time\\nopen(sys.argv[1],\"w\").write(str(os.getpid()))\\ntime.sleep(60)'\n"
                "suite._exec([sys.executable, '-c', child, sys.argv[1]], timeout=120)\n")
        process = self.worker(body, "crash", str(marker))
        self.wait_for(marker)
        child = int(marker.read_text())
        time.sleep(3)  # the worker died at about one second; its child still works
        os.kill(child, 0)
        with self.assertRaises(nfx_lock.LockHeld):  # the helper still holds the lock
            nfx_lock.hold("probe", 0).__enter__()
        process.communicate(timeout=120)  # the helper drains the session: grace, then TERM/KILL by exact pid
        with self.assertRaises(ProcessLookupError):
            os.kill(child, 0)
        with nfx_lock.hold("probe", 0):  # only now is the lock free
            pass

    def test_the_start_handshake_is_created_exclusively_by_the_worker_and_starts_the_timeout(self) -> None:
        import io
        path = self.directory / "handshake"
        with unittest.mock.patch.dict(os.environ, {nfx_lock.STARTED_FILE: str(path)}):
            services_uat.announce_start()
            self.assertTrue(path.exists())
            self.assertNotIn(nfx_lock.STARTED_FILE, os.environ)
        with unittest.mock.patch.dict(os.environ, {nfx_lock.STARTED_FILE: str(path)}), \
                self.assertRaises(FileExistsError):
            services_uat.announce_start()  # never silently reused
        # the controlled child of the review: works 1.5 s with a 0.1 s timeout; the timeout starts at the handshake
        later = self.directory / "later"
        child = (f"import pathlib,signal,sys,time\nsignal.signal(signal.SIGINT, lambda *a: sys.exit(0))\n"
                 f"time.sleep(0.3)\npathlib.Path({str(later)!r}).write_text('x')\ntime.sleep(60)")
        with contextlib.redirect_stdout(io.StringIO()):
            code, output = services_uat.run_child([sys.executable, "-c", child], timeout=0.1, budget=30,
                                                  started=later.exists)
        self.assertEqual(code, -9)
        self.assertIn("interrupted and cleaned up", output)
        with contextlib.redirect_stdout(io.StringIO()):  # a worker that never announces itself holds nothing: untimed
            code, output = services_uat.run_child([sys.executable, "-c", "import time\ntime.sleep(1.5)\nprint('ok')"],
                                                  timeout=0.1, started=lambda: False)
        self.assertEqual((code, output.strip()), (0, "ok"))

    def test_the_matrix_coordinator_forwards_cancellation_to_its_pass_child_and_waits(self) -> None:
        child = self.directory / "pass-child"
        for number, name, started in ((signal.SIGTERM, "term", "False"), (signal.SIGINT, "int", "True")):
            script = (f"import signal,sys,time\nname={name!r}\n"
                      "def got(n, f):\n    open(sys.argv[1] + '.' + name, 'w').write(str(n)); time.sleep(1); sys.exit(0)\n"
                      "signal.signal(signal.SIGTERM, got); signal.signal(signal.SIGINT, got)\n"
                      "open(sys.argv[1] + '.up', 'w').write('x')\ntime.sleep(60)")
            coordinator = (f"import signal,sys\nsys.path.insert(0,{self.SERVICES_DIR!r})\nimport services_uat\n"
                           "for n in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):\n"
                           "    signal.signal(n, services_uat.forward_cancellation)\n"
                           f"code, out = services_uat.run_child([sys.executable, '-c', {script!r}, {str(child)!r}], 120, "
                           f"started=lambda: {started})\n"
                           "print('child exited', code, services_uat.CANCELLED)")
            process = subprocess.Popen([sys.executable, "-c", coordinator], stdout=subprocess.PIPE, text=True,
                                       process_group=0)
            self.addCleanup(lambda p=process: (p.poll() is None and os.killpg(p.pid, signal.SIGKILL), p.communicate()))
            self.wait_for(self.directory / "pass-child.up")
            process.send_signal(number)  # the exact PID of the coordinator, as a review does
            out = process.communicate(timeout=60)[0]
            self.assertEqual(int((self.directory / f"pass-child.{name}").read_text()), int(signal.SIGTERM if name == "term"
                                                                                           else signal.SIGINT))
            self.assertIn("child exited 0", out)  # the coordinator waited for the child's own exit
            (self.directory / "pass-child.up").unlink()


class ObserverTests(unittest.TestCase):
    def test_read_only_guard_is_an_allowlist(self) -> None:
        readers = ["docker ps -aq", "docker inspect $(docker ps -aq)", "docker events --since 1 --format '{{json .}}'",
                   "findmnt -rn -o TARGET,FSTYPE", "cat /etc/docker/daemon.json 2>/dev/null", "hostname -I",
                   "docker exec c psql -U postgres -Atc 'select count(*) from items'",
                   "docker exec c sh -c 'ls /uploads | wc -l'", "docker volume ls -q", "find /pool -type f -name x",
                   "sed -n '1p' /etc/hostname", "sha256sum /etc/hostname | cut -d' ' -f1",
                   "docker exec c redis-cli get k", "docker exec c redis-cli -h localhost ping",
                   "pgrep -fc 'internal-services relay'", "docker ps --filter status=paused -q",
                   "docker exec c cat /d/marker", "docker -D ps", "docker exec -it c ls", "docker exec -u root -w /x c ls",
                   "docker exec c psql -Upostgres -d app -Atc 'select 1'", "docker exec c psql --command='select 1' --no-align",
                   "sort -n /etc/hostname", "sort -u -k2 /etc/hostname", "sort -t, -k2 /etc/hostname", "sed -n 1p /etc/hostname",
                   "hostname -s", "ss -ltn"]
        writers = ["python3 -c 'open(\"/pool/snapshots/x\",\"w\").write(\"oops\")'", "docker rm -f x",
                   "docker exec c sh -c 'rm -rf /x'", "cat x > y", "echo x | tee y", "find / -delete",
                   "find . -exec rm {} +", "sed -i s/a/b/ f", "sed -n 'w out' f", "docker cp a b",
                   "docker exec c psql -c 'drop table t'", "docker exec c psql -c 'select 1; drop table t'",
                   "touch /x", "docker compose up -d", "xargs rm", "dd of=/x", "sort -o f f", "sh -c 'echo > f'",
                   "mount /dev/x /y", "env rm x", "date -s 1", "docker run busybox true", "$(rm x)", "cat <<EOF",
                   "docker exec c redis-cli set k v", "sbx stop x", "perl -e 'unlink 1'", "awk 'BEGIN{system(\"rm x\")}'",
                   "docker exec c psql -c 'select pg_terminate_backend(1)'", "ls; rm x", "ls && touch y",
                   "docker exec c python3 -c 'print(1)'", "cat x\nrm y", "echo `rm x`", "tail -f x | tee z",
                   # round 3: an override that moves the oracle off its own engine, and the writes a SELECT can make
                   "DOCKER_HOST=unix:///run/marsh/docker.sock docker ps", "docker -H unix:///run/marsh/docker.sock ps",
                   "docker --host=unix:///x ps", "docker --context other ps", "docker --config /tmp/c ps",
                   "docker exec -e DOCKER_HOST=unix:///x c docker ps", "docker exec --env=FOO=1 c env",
                   "FOO=1 docker ps", "export DOCKER_HOST=unix:///x; docker ps",
                   "docker exec c psql -c \"select nextval('seq')\"", "docker exec c psql -c \"select setval('seq', 1)\"",
                   "docker exec c psql -c 'select * into t from x'", "docker exec c psql -c 'select 1 for update'",
                   "docker exec c psql -c 'select set_config($$a$$,$$b$$,false)'",
                   "docker exec c psql -c 'select lo_import($$/etc/passwd$$)'",
                   "docker exec c psql -c 'select dblink_exec($$x$$)'", "docker exec c psql -c 'select mystery_fn(1)'",
                   "docker exec c psql -o /pool/x -c 'select 1'", "docker exec c psql -L /pool/x -c 'select 1'",
                   "docker exec c psql -c '\\dt \\! rm x'", "docker exec c psql -c 'select 1' -c 'select 2'",
                   "date -s2020-01-01", "date -s '2020-01-01'", "date 010112002020", "date --set=2020-01-01",
                   "hostname foo", "uniq a b", "ss -K", "docker search x",
                   # round 4: attached short forms and write forms Docker, GNU sort, sed and psql accept
                   "docker -Hunix:///run/marsh/docker.sock ps", "docker -cjob ps",
                   "docker exec -eDOCKER_HOST=unix:///run/marsh/docker.sock target docker ps",
                   "docker --context=job ps", "docker -D -Hunix:///x ps", "docker exec -d c ls", "docker exec --privileged c ls",
                   "sort --output=/tmp/out /tmp/in", "sort -o/tmp/out /tmp/in", "sort --o=/tmp/out /tmp/in",
                   "sort -uo/tmp/out /tmp/in", "sort --compress-program=sh /tmp/in",
                   "docker exec c psql -c 'select 1' -o/tmp/out", "docker exec c psql -c 'select 1' -L/tmp/x",
                   "docker exec c psql -Atc 'select 1' -ofile", "docker exec c psql -ffile", "docker exec c psql -c 'select 1' -s",
                   "docker exec c psql --output=/tmp/x -c 'select 1'", "docker exec c psql --log-file=/tmp/x -c 'select 1'",
                   "sed -ni s/a/b/p f", "sed -n --in-place=.b s/a/b/p f", "sed -n -f script f",
                   "hostname -Fhosts", "hostname -b x", "ss -tK", "docker exec c psql -c 'select 1' extra"]
        for command in readers:
            with self.subTest(reader=command):
                self.assertIsNone(observer.readonly_violation(command), command)
        for command in writers:
            with self.subTest(writer=command):
                self.assertIsNotNone(observer.readonly_violation(command), command)
        self.assertIsNone(observer.readonly_violation(observer.SNAPSHOT_LISTING))
        self.assertIsNone(observer.readonly_violation(observer.EventWatcher.SCRIPT))

    def test_vx_modes_are_explicit(self) -> None:
        self.assertEqual(observer.vx_argv("sbx", "svc", "hostname", "boot"),
                         ["sbx", "exec", "svc", "sudo", "sh", "-c", "hostname"])
        prod = observer.vx_argv("sbx", "svc", "docker ps", "prod")
        self.assertEqual(prod[:8], ["sbx", "exec", "svc", "sudo", "docker", "exec", "marsh-svc-engine", "sh"])
        self.assertIn(f"DOCKER_HOST=unix://{observer.PROD_DOCKER_SOCKET}", prod[-1])
        self.assertTrue(prod[-1].endswith("docker ps"))
        with self.assertRaises(ValueError):
            observer.vx_argv("sbx", "svc", "x", "default")

    def clean_container(self, **host: object) -> dict:
        return {"Name": "/db", "HostConfig": {"MaskedPaths": ["/proc/kcore"], "ReadonlyPaths": ["/proc/sys"],
                                              "PortBindings": {"5432/tcp": [{"HostIp": "127.0.0.1", "HostPort": "5432"}]},
                                              "LogConfig": {"Type": "json-file"}, **host},
                "Mounts": [{"Type": "bind", "Source": "/Users/me/p/app"}, {"Type": "volume", "Name": "v"}]}

    def test_vaudit_rules(self) -> None:
        roots = ["/Users/me/p"]
        self.assertEqual(observer.audit_container(self.clean_container(), roots), [])
        for host, fragment in [
            ({"Privileged": True}, "Privileged"), ({"CapAdd": ["SYS_ADMIN"]}, "CapAdd SYS_ADMIN"),
            ({"CapAdd": ["CAP_NET_RAW"]}, None), ({"Devices": [{"PathOnHost": "/dev/loop0"}]}, "Devices"),
            ({"DeviceCgroupRules": ["b 7:* rwm"]}, "DeviceCgroupRules"), ({"NetworkMode": "host"}, "NetworkMode: host"),
            ({"PidMode": "host"}, "PidMode"), ({"SecurityOpt": ["seccomp=unconfined"]}, "unconfined"),
            ({"SecurityOpt": ["seccomp=/x/allow-all.json"]}, "seccomp"), ({"MaskedPaths": []}, "MaskedPaths emptied"),
            ({"ReadonlyPaths": None}, "ReadonlyPaths emptied"),
            ({"PortBindings": {"80/tcp": [{"HostIp": "0.0.0.0", "HostPort": "80"}]}}, "bound to 0.0.0.0"),
            ({"PortBindings": {"80/tcp": [{"HostIp": "", "HostPort": "80"}]}}, "all interfaces"),
            ({"LogConfig": {"Type": "syslog"}}, "log driver syslog")]:
            with self.subTest(host=host):
                found = observer.audit_container(self.clean_container(**host), roots)
                self.assertEqual(bool(found), fragment is not None, found)
                if fragment:
                    self.assertTrue(any(fragment in line for line in found), found)
        outside = self.clean_container()
        outside["Mounts"].append({"Type": "bind", "Source": "/var/lib/docker"})
        outside["Mounts"].append({"Type": "bind", "Source": "/Users/me/p-evil"})
        self.assertEqual(len(observer.audit_container(outside, roots)), 2)

    def test_volume_audit_rules(self) -> None:
        self.assertEqual(observer.audit_volume({"Name": "a", "Driver": "local", "Options": None}), [])
        self.assertEqual(observer.audit_volume({"Name": "t", "Driver": "local",
                                                "Options": {"type": "tmpfs", "device": "tmpfs", "o": "size=64m,mode=1777"}}), [])
        for volume in [{"Name": "n", "Driver": "nfs"},
                       {"Name": "b", "Driver": "local", "Options": {"type": "none", "o": "bind", "device": "/"}},
                       {"Name": "t", "Driver": "local", "Options": {"type": "tmpfs", "o": "exec"}}]:
            self.assertTrue(observer.audit_volume(volume), volume)

    def test_freeze_window_is_last_unpause_minus_first_pause(self) -> None:
        events = [json.dumps({"Action": "pause", "timeNano": 1_000_000_000}),
                  json.dumps({"Action": "pause", "timeNano": 1_010_000_000}),
                  json.dumps({"Action": "unpause", "timeNano": 1_040_000_000}),
                  json.dumps({"Action": "unpause", "timeNano": 1_057_700_000}), json.dumps({"Action": "start"}), ""]
        window = observer.freeze_window(events)
        self.assertEqual((window["pauses"], window["unpauses"]), (2, 2))
        self.assertAlmostEqual(window["window_ms"], 57.7)
        self.assertIsNone(observer.freeze_window([json.dumps({"Action": "start", "timeNano": 1})]))

    def test_snapshot_listing(self) -> None:
        self.assertEqual(observer.parse_snapshot_listing("c1 abc\nc2 def\n\n"), {"c1": "abc", "c2": "def"})

    def make_observer(self, **state):
        recorder = evidence.Recorder()
        calls: list[list[str]] = []
        state = {"names": {"mine"}, "uuid": "u1", "mapped": "u1", "generation": "abc123", "engine": "abc123", **state}

        def run(argv, timeout):
            calls.append(argv)
            return (0, state["engine"].encode() + b"\n", b"", 1, 2)

        helper = observer.Observer("sbx", run, recorder, lambda: set(state["names"]), lambda: set(state["names"]),
                                   uuid_of=lambda n: state["uuid"] if n in state["names"] else None,
                                   owned_uuid=lambda n: state["mapped"],
                                   engine_generation=lambda n: state["generation"])
        return helper, recorder, calls, state

    def test_va_and_vb_are_stubs_later_tasks_fill_in(self) -> None:
        helper, *_ = self.make_observer()
        with self.assertRaises(observer.Pending):
            helper.vA("p")
        with self.assertRaises(observer.Pending):
            helper.vB("b")

    def test_vx_fails_closed_on_ownership_and_never_writes_unmarked(self) -> None:
        helper, recorder, calls, state = self.make_observer()
        with self.assertRaises(observer.TargetMismatch):
            helper.bind("absent")
        state["mapped"] = None
        with self.assertRaises(observer.TargetMismatch):  # in sbx ls, in the owned set, but no UUID anywhere
            helper.bind("mine")
        state["mapped"] = "u1"
        helper.bind("mine")
        with self.assertRaises(observer.ReadOnlyViolation):
            helper.vx("docker rm -f x")
        with self.assertRaises(observer.ReadOnlyViolation):
            helper.vx("python3 -c 'open(\"/pool/snapshots/x\",\"w\")'", engine="prod")
        self.assertEqual(calls, [])
        helper.vx("docker ps -aq", engine="prod")
        helper.vx("touch /etc/canary", write=True)
        self.assertEqual([o["write"] for o in recorder.observations if o["argv"][-1].endswith(("-aq", "canary"))],
                         [False, True])
        self.assertNotIn("check_target", __import__("inspect").signature(helper.vx).parameters)
        # the VM leaves the ownership map between probes: the next probe refuses
        state["names"] = set()
        with self.assertRaises(observer.TargetMismatch):
            helper.vx("hostname")

    def test_a_sandbox_the_harness_made_is_bound_by_its_recorded_id(self) -> None:
        helper, recorder, calls, state = self.make_observer(mapped=None)
        helper.bind("mine", direct_uuid="u1")
        helper.vx("hostname")
        state["uuid"] = "u2"  # replaced under the same name
        with self.assertRaisesRegex(observer.TargetMismatch, "replaced"):
            helper.vx("hostname")


class EvidenceTests(unittest.TestCase):
    def document(self) -> dict:
        recorder = evidence.Recorder()
        return {"schema": evidence.SCHEMA_ID, "run": "2611061039191b2c", "scenario": "HARNESS-01", "title": "t",
                "tier": "smoke", "driver": "product", "outcome": "passed", "failure": None, "failed_check": None,
                "blocked_on": None, "started_at": "2026-11-06T10:00:00Z", "finished_at": "2026-11-06T10:00:01Z",
                "duration_ms": 1000, "break": {"kind": "normal", "modes": [], "expected_checks": [], "verified": None,
                                               "variant": None, "pass": None, "verdict": "normal"},
                "oracle": "equal", "active_breaks": {"daemon": [], "agent": None}, "foreign": [], "checks": [{"id": "c1", "outcome": "passed", "detail": ""}],
                "commands": [recorder.command(["sbx", "ls"], "/tmp", 0, 1, 2, b"out", b"")], "observations": [],
                "timings": [{"label": "x", "ns": 5}],
                "versions": {"marsh": {}, "sbx": "v", "macos": "26", "ram_bytes": 1, "free_disk_bytes": 1,
                             "free_disk_end_bytes": 1, "services_engine": None, "kit": "k",
                             "host_load_start": {"load1": 1.5, "load5": 2.0, "load15": 2.5, "cpus": 8,
                                                 "running_sandboxes": 3},
                             "host_load_end": {"load1": 1.5, "load5": 2.0, "load15": 2.5, "cpus": 8,
                                               "running_sandboxes": 3}}}

    def test_a_complete_document_validates_and_gaps_are_named(self) -> None:
        evidence.validate_evidence(self.document())
        for mutate, message in [(lambda d: d.pop("failed_check"), "failed_check"),
                                (lambda d: d.update(outcome="skipped"), "enum"),
                                (lambda d: d.update(extra=1), "extra"),
                                (lambda d: d["checks"][0].update(id="x"), "match"),
                                (lambda d: d["commands"][0]["stdout"].update(sha256="no"), "match"),
                                (lambda d: d["break"].pop("verified"), "verified"),
                                (lambda d: d["break"].update(verdict="maybe"), "enum"),
                                (lambda d: d.pop("foreign"), "foreign"),
                                (lambda d: d.pop("oracle"), "oracle"),
                                (lambda d: d.update(oracle="maybe"), "enum"),
                                (lambda d: d["checks"][0].update(outcome="unknown"), "enum"),
                                (lambda d: d["versions"].pop("host_load_end"), "host_load_end"),
                                (lambda d: d["versions"]["host_load_start"].pop("cpus"), "cpus")]:
            document = self.document()
            mutate(document)
            with self.subTest(message=message), self.assertRaisesRegex(ValueError, message):
                evidence.validate_evidence(document)

    def test_streams_are_byte_exact_and_secrets_are_redacted(self) -> None:
        recorder = evidence.Recorder()
        recorder.add_secret("s3cr3t-token")
        record = recorder.command(["x", "--key=s3cr3t-token"], None, 0, 0, 0, b"a s3cr3t-token b\xff", b"")
        self.assertNotIn("s3cr3t-token", json.dumps(record))
        self.assertEqual(record["stdout"]["encoding"], "base64")
        self.assertEqual(record["stdout"]["bytes"], len(b"a [redacted] b\xff"))
        plain = recorder.stream("héllo".encode())
        self.assertEqual((plain["encoding"], plain["data"], plain["truncated"]), ("utf-8", "héllo", False))
        big = recorder.stream(b"x" * (evidence.STREAM_LIMIT + 5))
        self.assertEqual((big["bytes"], big["truncated"], len(big["data"])),
                         (evidence.STREAM_LIMIT + 5, True, evidence.STREAM_LIMIT))

    def test_series_collects_n_plus_three_and_keeps_failures_and_cold_starts(self) -> None:
        series = evidence.Series("restore", 20)
        self.assertEqual(series.wanted, 23)
        for ns in [10_000, 10_000, 10_000, *range(1, 21)]:
            series.add(ns)
        self.assertTrue(series.complete())
        self.assertEqual(len(series.retained()), 20)
        self.assertEqual(series.percentile(0.95), 19)
        self.assertEqual(series.percentile(0.5), 10)
        failing = evidence.Series("x", 4)
        for ns, ok in [(1, True)] * 3 + [(9, True), (50, False), (7, True), (8, True)]:
            failing.add(ns, ok=ok)
        self.assertEqual((len(failing.retained()), failing.failures()), (4, 1))
        self.assertEqual(failing.percentile(1.0), 9)  # the failed attempt is counted, not timed
        cold = evidence.Series("cold", 5, cold=True)
        self.assertEqual((cold.warmups, cold.wanted), (0, 5))
        for ns in (3, 1, 2, 5, 4):
            cold.add(ns)
        self.assertEqual(len(cold.retained()), 5)
        self.assertEqual(cold.percentile(0.95), 5)
        recorder = evidence.Recorder()
        series.record(recorder)
        self.assertEqual([t["retained"] for t in recorder.timings].count(False), 3)
        self.assertEqual(evidence.percentile([4, 1, 3, 2], 0.5), 2)
        with self.assertRaises(ValueError):
            evidence.percentile([], 0.5)

    def test_fixture_defined_patterns_redact_every_evidence_byte(self) -> None:
        directory = pathlib.Path(tempfile.mkdtemp(prefix="nfx-secrets-test-"))
        self.addCleanup(lambda: __import__("shutil").rmtree(directory, ignore_errors=True))
        path = directory / "synthetic-secrets.json"
        path.write_text(json.dumps({"literals": ["devpass"], "patterns": ["svc_key_[0-9a-f]{8}"]}))
        recorder = evidence.Recorder()
        literals, patterns = evidence.load_synthetic_secrets(path)
        for literal in literals:
            recorder.add_secret(literal)
        for regex, replacement in patterns:
            recorder.add_pattern(regex, replacement)
        document = {"failure": "psql postgresql://postgres:devpass@localhost svc_key_deadbeef",
                    "svc_key_0badcafe": {"nested": {"NFX_SECRET_abcd1234": "x"}},
                    "commands": [recorder.command(["x", "NFX_SECRET_abcd1234"], None, 0, 0, 0,
                                                  b"svc_key_0123abcd \xff\xfe", b"")]}
        dumped = json.dumps(recorder.redact_document(document))
        for secret in ("devpass", "svc_key_deadbeef", "svc_key_0badcafe", "svc_key_0123abcd", "NFX_SECRET_abcd1234"):
            self.assertNotIn(secret, dumped)
        self.assertEqual(document["commands"][0]["stdout"]["encoding"], "base64")  # the encoded stream is redacted too
        raw = __import__("base64").b64decode(document["commands"][0]["stdout"]["data"])
        self.assertNotIn(b"svc_key_0123abcd", raw)
        collided = recorder.redact_document({"svc_key_11111111": 1, "svc_key_22222222": 2, "[redacted]": 3})
        self.assertEqual(sorted(collided.values()), [1, 2, 3])  # a key collision never loses a value
        self.assertEqual(evidence.load_synthetic_secrets(directory / "absent.json"), ([], []))
        path.write_text(json.dumps({"patterns": ["("]}))
        with self.assertRaises(Exception):
            evidence.load_synthetic_secrets(path)

    def test_h1bs_redaction_json_format_is_understood(self) -> None:
        directory = pathlib.Path(tempfile.mkdtemp(prefix="nfx-redaction-test-"))
        self.addCleanup(lambda: __import__("shutil").rmtree(directory, ignore_errors=True))
        path = directory / "redaction.json"
        path.write_text(json.dumps({"replacement": "[redacted:nfx]", "patterns": [
            {"name": "canary", "regex": "NFX-CANARY-SECRET-[0-9a-f]{8}"},
            {"name": "url password", "regex": "(postgres(?:ql)?://[^:/@\\s]+:)[^@\\s]+(@)", "replace": "\\1[redacted]\\2"}]}))
        literals, patterns = evidence.load_synthetic_secrets(path)
        self.assertEqual((literals, len(patterns)), ([], 2))
        recorder = evidence.Recorder()
        for regex, replacement in patterns:
            recorder.add_pattern(regex, replacement)
        out = recorder.redact(b"NFX-CANARY-SECRET-0123abcd postgresql://postgres:devpass@localhost/app")
        self.assertEqual(out, b"[redacted:nfx] postgresql://postgres:[redacted]@localhost/app")


class DiagnosticsTests(unittest.TestCase):
    SHIPPED = "marsh/svc-engine@sha256:" + "a" * 64

    def test_boot_audit_needs_the_shipped_digest_and_exactly_the_engine_container(self) -> None:
        engine = {"Names": "marsh-svc-engine", "Image": self.SHIPPED}
        self.assertEqual(observer.boot_audit([engine], self.SHIPPED), [])
        with self.assertRaises(observer.Inconclusive):
            observer.boot_audit([engine], "")  # no shipped digest: the audit proves nothing
        with self.assertRaises(observer.Inconclusive):
            observer.boot_audit([engine], "marsh/svc-engine:latest")
        self.assertTrue(observer.boot_audit([], self.SHIPPED))
        self.assertTrue(observer.boot_audit([engine, {"Names": "postgres", "Image": "postgres:17"}], self.SHIPPED))
        self.assertTrue(observer.boot_audit([{**engine, "Image": "marsh/svc-engine@sha256:" + "b" * 64}], self.SHIPPED))
        self.assertTrue(observer.boot_audit([{**engine, "Image": "marsh/svc-engine:latest"}], self.SHIPPED))

    def test_vboot_audit_is_inconclusive_without_the_shipped_digest(self) -> None:
        helper = ObserverTests.make_observer(ObserverTests())[0]
        helper.bind("mine")
        with self.assertRaises(observer.Inconclusive):
            helper.vboot_audit()

    def test_freeze_pairs_are_ordered_per_container(self) -> None:
        def event(action: str, ident: str, nano: int) -> str:
            return json.dumps({"Action": action, "Actor": {"ID": ident * 64}, "timeNano": nano})
        lines = [event("pause", "a", 1_000_000_000), event("pause", "b", 1_005_000_000),
                 event("unpause", "a", 1_040_000_000), event("pause", "c", 5_000_000_000),
                 event("unpause", "b", 1_050_000_000)]
        window = observer.freeze_pairs(lines, {"a" * 64, "b" * 64})
        self.assertAlmostEqual(window["window_ms"], 50.0)
        self.assertEqual(window["unpaired"], [])
        self.assertEqual(observer.freeze_pairs(lines, None)["unpaired"], ["c" * 12])
        self.assertIsNone(observer.freeze_pairs(lines, {"d" * 64}))
        # pause, unpause, unpause, pause: two unmatched events, which a count of pauses and unpauses cannot see
        reordered = [event("pause", "e", 1), event("unpause", "e", 2), event("unpause", "e", 3), event("pause", "e", 4)]
        self.assertEqual(observer.freeze_pairs(reordered, None)["unpaired"], ["e" * 12])
        self.assertEqual(observer.freeze_pairs([event("pause", "f", 1), event("pause", "f", 2), event("unpause", "f", 3)],
                                               None)["unpaired"], ["f" * 12])  # a second pause while frozen
        self.assertEqual(observer.freeze_pairs([event("unpause", "g", 1)], None)["unpaired"], ["g" * 12])

    def test_canary_is_a_durable_write_the_case_would_make(self) -> None:
        directory = pathlib.Path(tempfile.mkdtemp(prefix="nfx-canary-test-"))
        self.addCleanup(lambda: __import__("shutil").rmtree(directory, ignore_errors=True))
        command = observer.canary_command("priv case/1", "/h")
        self.assertIn("/h/.nfx-canary/priv_case_1", command)
        self.assertFalse(observer.canary_present(directory, "priv case/1"))
        (directory / ".nfx-canary").mkdir()
        (directory / ".nfx-canary" / "priv_case_1").write_text("escaped\n")
        self.assertTrue(observer.canary_present(directory, "priv case/1"))

    def test_production_probes_need_a_generation_and_the_engine_container_must_match_it(self) -> None:
        helper, recorder, calls, state = ObserverTests.make_observer(ObserverTests())
        helper.bind("mine")
        helper.vx("docker ps -aq", engine="prod")
        state["generation"] = "zzz999"
        with self.assertRaisesRegex(observer.TargetMismatch, "generation.engine"):
            helper.vx("docker ps -aq", engine="prod")
        state["generation"] = None  # the product cannot say: the production engine cannot be observed
        with self.assertRaisesRegex(observer.TargetMismatch, "no generation.engine"):
            helper.vx("docker ps -aq", engine="prod")
        helper.vx("hostname", engine="boot")  # the boot engine needs no generation
        state["generation"] = "abc123"
        state["mapped"] = "u9"
        with self.assertRaisesRegex(observer.TargetMismatch, "replaced"):
            helper.vx("hostname")

    def watcher_fixture(self):
        helper, *_ = ObserverTests.make_observer(ObserverTests())
        helper.bind("mine")
        keep: list[object] = []
        fakes: list[object] = []

        class Fake:
            """A subscription: LINES are written to a pipe the watcher reads; CLOSE ends the stream."""

            def __init__(self, lines: list[str], close: bool = False) -> None:
                read, write = os.pipe()
                os.write(write, ("\n".join(lines) + "\n").encode())
                self.stdout = os.fdopen(read, "rb")
                self.writer, self.pid, self.returncode, self.status = write, 4242, None, None
                fakes.append(self)
                if close:
                    os.close(write)
                else:
                    keep.append(write)
                keep.append(self.stdout)

            def poll(self):
                return None

            def end(self) -> None:
                """The subscriber exits: its stream ends."""
                if self.writer in keep:
                    keep.remove(self.writer)
                    os.close(self.writer)

            def wait(self, timeout=None):
                return 0 if self.status is None else self.status

        def killed(pid: int, number: int) -> None:
            # never a real signal: the fake subscription's "process group" is only a pipe, which this closes
            assert pid == 4242 and number == signal.SIGKILL, (pid, number)
            for fake in fakes:
                if fake.status is None:
                    fake.status = -signal.SIGKILL  # the kill ended it with exactly that signal
            for item in keep:
                if isinstance(item, int):
                    with contextlib.suppress(OSError):
                        os.close(item)
            keep[:] = [k for k in keep if not isinstance(k, int)]

        patcher = unittest.mock.patch.object(observer.os, "killpg", side_effect=killed)
        patcher.start()
        self.addCleanup(patcher.stop)
        def tidy() -> None:  # end every stream first so no reader thread is blocked when its file is closed
            for item in [k for k in keep if isinstance(k, int) and not _closed(k)]:
                os.close(item)
            for item in [k for k in keep if not isinstance(k, int)]:
                item.close()

        self.addCleanup(tidy)
        return helper, Fake

    @staticmethod
    def marker_lines(marker: str) -> list[str]:
        event = json.dumps({"Type": "network", "Action": "create", "Actor": {"ID": "n" * 64, "Attributes": {"name": marker}}})
        return [f"E {event}", f"I network {'n' * 64} {json.dumps([{'Name': marker, 'Id': 'n' * 64}])}"]

    def test_the_event_watcher_waits_for_the_subscription_and_never_reports_a_clean_audit_it_cannot_back(self) -> None:
        helper, Fake = self.watcher_fixture()
        create = json.dumps({"Type": "container", "Action": "create", "Actor": {"ID": "a" * 64, "Attributes": {
            "name": "web", "dev.marsh.scope": "A"}}})
        other = json.dumps({"Type": "container", "Action": "create", "Actor": {"ID": "b" * 64, "Attributes": {
            "name": "db", "dev.marsh.scope": "B"}}})
        volume = json.dumps({"Type": "volume", "Action": "create", "Actor": {"ID": "v1", "Attributes": {"name": "pg"}}})
        network = json.dumps({"Type": "network", "Action": "create", "Actor": {"ID": "n2" * 32, "Attributes": {
            "name": "backnet", "dev.marsh.scope": "A"}}})
        inspect = [{"Id": "a" * 64, "Name": "/web", "HostConfig": {"Privileged": True, "MaskedPaths": ["x"],
                                                                    "ReadonlyPaths": ["y"]},
                    "Config": {"Labels": {"dev.marsh.scope": "A"}}}]

        def lines(marker: str) -> list[str]:
            return ["R 1", *self.marker_lines(marker), f"E {create}", f"I container {'a' * 64} {json.dumps(inspect)}",
                    f"E {other}", f"I container {'b' * 64} []", f"E {volume}", f"I volume v1 []",
                    f"E {network}", f"I network {'n2' * 32} {json.dumps([{'Name': 'backnet'}])}"]

        def start(scope=None, close=False, timeout=30):
            watcher = observer.EventWatcher(helper, "boot", scope=scope)
            return watcher.start(lambda argv: Fake(lines(watcher.marker), close), timeout=timeout)

        watcher = start("A")
        for _ in range(100):
            if len(watcher.uninspected) == 2 and "backnet" in {v.get("Name") for v in watcher.inspected["network"].values()}:
                break
            time.sleep(0.02)
        self.assertEqual(watcher.objects("volume"), {"pg"})
        self.assertEqual(watcher.objects("container", scoped=True), {"web"})
        self.assertNotIn(watcher.marker, watcher.objects("network"))  # the readiness marker is ours, never reported
        self.assertEqual(watcher.objects("network"), {"backnet"})
        violations = watcher.audit([], scoped=True)
        self.assertTrue(any("Privileged" in v for v in violations))
        self.assertFalse(any("bbbb" in v for v in violations))  # B's container is out of scope
        unscoped = start()
        for _ in range(100):
            if unscoped.uninspected:
                break
            time.sleep(0.02)
        self.assertTrue(any("inconclusive" in v and "bbbbbbbbbbbb" in v for v in unscoped.audit([])))
        self.assertFalse(any(unscoped.marker in v or "nnnnnnnnnnnn" in v for v in unscoped.audit([])))

    def test_the_event_watcher_is_live_only_after_its_own_marker_event_came_back(self) -> None:
        helper, Fake = self.watcher_fixture()
        # R but no marker echo: the subscription never proved itself live
        with self.assertRaisesRegex(observer.Inconclusive, "marker event never came back"):
            observer.EventWatcher(helper, "boot").start(lambda argv: Fake(["R 1"]), timeout=1)
        # an event for a DIFFERENT marker name does not count
        stray = self.marker_lines("nfx-ready-someone-else")
        with self.assertRaisesRegex(observer.Inconclusive, "marker event never came back"):
            observer.EventWatcher(helper, "boot").start(lambda argv: Fake(["R 1", *stray]), timeout=1)
        # never subscribed at all
        with self.assertRaisesRegex(observer.Inconclusive, "did not start"):
            observer.EventWatcher(helper, "boot").start(lambda argv: Fake(["not a ready line"], close=True), timeout=1)

    def test_a_subscription_that_dies_before_stop_is_inconclusive_not_clean(self) -> None:
        helper, Fake = self.watcher_fixture()
        watcher = observer.EventWatcher(helper, "boot")
        dead = watcher.start(lambda argv: Fake(["R 1", *self.marker_lines(watcher.marker)], close=True))
        time.sleep(0.3)  # the stream ended before stop(): the record is inconclusive, not clean
        for query in (lambda: dead.audit([]), lambda: dead.objects("container"), lambda: dead.actions("create")):
            with self.assertRaises(observer.Inconclusive):
                query()
        dead.stop()
        self.assertFalse(dead.healthy())  # a stop after the death does not bless it

    def test_only_a_kill_the_watcher_delivered_is_a_clean_stop(self) -> None:
        helper, Fake = self.watcher_fixture()

        def started() -> observer.EventWatcher:
            watcher = observer.EventWatcher(helper, "boot")
            return watcher.start(lambda argv: Fake(["R 1", *self.marker_lines(watcher.marker)]))

        clean = started()
        clean.stop()  # SIGKILL delivered, the subscriber ended with exactly -SIGKILL
        self.assertTrue(clean.healthy())
        clean.require_healthy()
        # the subscriber exits on its own (status 9) in the instant between the checks and the kill: the group is
        # gone, so the kill finds nothing
        raced = started()
        raced.process.status = 9

        def gone(*arguments: object) -> None:
            raced.process.end()  # it exits right here, after stop() decided to kill
            raise ProcessLookupError

        with unittest.mock.patch.object(observer.os, "killpg", side_effect=gone):
            raced.stop()
        self.assertFalse(raced.healthy())
        with self.assertRaisesRegex(observer.Inconclusive, "ended on its own before it was stopped \\(exit 9\\)"):
            raced.audit([])
        # ... or the group still exists as a zombie so the kill "succeeds", but the status is not -SIGKILL
        zombie = started()
        zombie.process.status = 9
        with unittest.mock.patch.object(observer.os, "killpg", side_effect=lambda *a: zombie.process.end()):
            zombie.stop()
        self.assertFalse(zombie.healthy())
        with self.assertRaises(observer.Inconclusive):
            zombie.actions("create")
        # and a death that the reader saw first is never cleared by a later stop()
        early = started()
        early.error = "the event subscription ended before it was stopped"
        early.stop()
        self.assertFalse(early.healthy())

    def test_a_marker_is_created_and_removed_as_a_documented_write(self) -> None:
        helper, recorder, calls, state = ObserverTests.make_observer(ObserverTests())
        helper.bind("mine")
        _, Fake = self.watcher_fixture()
        watcher = observer.EventWatcher(helper, "boot")
        watcher.start(lambda argv: Fake(["R 1", *self.marker_lines(watcher.marker)]))
        writes = [o["argv"][-1] for o in recorder.observations if o["write"]]
        self.assertTrue(any(f"docker network create {watcher.marker}" in w for w in writes), writes)
        self.assertTrue(any(f"docker network rm {watcher.marker}" in w for w in writes), writes)


class TruthTests(unittest.TestCase):
    def manifest(self, **kw) -> truth_module.Manifest:
        return truth_module.Manifest.record(
            {"db": {"items": 1000, "note": True, "nonce": "n1"}, "app": {"seed": "h1"}},
            {"redis": {"k": "v1"}, "mail": {"messages": 1}}, kw.get("unsupported", ()), kw.get("missing", ()),
            kw.get("volume_only", {"keeper": {"container": "keeper", "reported": "notes", "sentinels": {"m": "n1"}}}))

    F, V = truth_module.ForkService, truth_module.ForkVolume

    def good_fork(self) -> dict:
        return {"db": self.F(True, True, {"items": 1000, "note": True, "nonce": "n1"}),
                "app": self.F(True, True, {"seed": "h1"}), "redis": self.F(True, True, {"k": None}),
                "mail": self.F(True, True, {"messages": 0})}

    def good_volumes(self) -> dict:
        return {"notes": self.V(True, {"m": "n1"})}

    def lists(self, **kw) -> truth_module.ProductLists:
        return truth_module.ProductLists(frozenset(kw.get("restored", {"db", "app"})),
                                         frozenset(kw.get("fresh", {"redis", "mail"})),
                                         frozenset(kw.get("gaps", set())), kw.get("reported", {"keeper": "notes"}))

    def judge(self, fork=None, manifest=None, lists=None, volumes=None, **extra) -> truth_module.Truth:
        return truth_module.truth(self.good_fork() if fork is None else fork, manifest or self.manifest(),
                                  lists or self.lists(), volumes=self.good_volumes() if volumes is None else volumes,
                                  **extra)

    def test_a_fresh_service_needs_a_positive_read_showing_the_sentinel_absent(self) -> None:
        for observed in ({}, {"error": "boom"}, {"k": {"error": "psql: connection refused"}}):
            fork = self.good_fork()
            fork["redis"] = self.F(True, True, observed)
            result = self.judge(fork=fork)
            self.assertFalse(result.ok, observed)
            self.assertTrue(any("no positive observation" in r for r in result.reasons), result.reasons)
        fork = self.good_fork()
        fork["redis"] = self.F(True, True, {"k": None})
        self.assertTrue(self.judge(fork=fork).ok)  # an explicit None read is a positive observation of absence

    def test_tuples_and_lists_compare_equal(self) -> None:
        self.assertTrue(truth_module.equal({"a": [1, 2]}, {"a": (1, 2)}))
        self.assertTrue(truth_module.equal({"a": {"x": (1, [2])}}, {"a": {"x": [1, (2,)]}}))
        self.assertFalse(truth_module.equal({"a": [1, 2]}, {"a": (2, 1)}))
        self.assertEqual(truth_module.normalize({3, 1, 2}), [1, 2, 3])

    def test_a_faithful_restore_is_true(self) -> None:
        verdict = self.judge()
        self.assertTrue(verdict.ok, verdict.reasons)

    def test_every_rule_can_fail(self) -> None:
        F = self.F
        cases = {
            "1 absent": dict(fork={k: v for k, v in self.good_fork().items() if k != "db"}),
            "1 no containers": dict(fork={}),
            "1 not healthy": dict(fork={**self.good_fork(), "app": F(True, False, {"seed": "h1"})}),
            "1 wrong sentinel": dict(fork={**self.good_fork(), "db": F(True, True, {"items": 500, "note": False,
                                                                                      "nonce": "n1"})}),
            "2 volume-only data absent": dict(volumes={"notes": self.V(False)}),
            "2 volume-only wrong data": dict(volumes={"notes": self.V(True, {"m": "other"})}),
            "2 volume-only not under the reported name": dict(volumes={"elsewhere": self.V(True, {"m": "n1"})}),
            "2 volume-only container started": dict(fork={**self.good_fork(), "keeper": F(True, True, {})}),
            "3 fresh kept data": dict(fork={**self.good_fork(), "redis": F(True, True, {"k": "v1"})}),
            "3 fresh not running": dict(fork={**self.good_fork(), "mail": F(False, False, {})}),
            "4 unsupported started with data": dict(manifest=self.manifest(unsupported={"keeper2"}),
                                                    lists=self.lists(gaps={"keeper2"}),
                                                    started_with_data={"keeper2"}),
            "5 restored list empty": dict(lists=self.lists(restored=set())),
            "5 restored list too long": dict(lists=self.lists(restored={"db", "app", "x"})),
            "5 fresh list omits": dict(lists=self.lists(fresh={"redis"})),
            "5 unsupported not in gaps": dict(manifest=self.manifest(unsupported={"keeper2"})),
            "5 missing not in gaps": dict(manifest=self.manifest(missing={"db"})),
        }
        for name, arguments in cases.items():
            with self.subTest(name):
                self.assertFalse(self.judge(**arguments).ok)

    def test_nothing_is_subtracted_from_the_expected_set_and_missing_data_forces_false(self) -> None:
        manifest = self.manifest(missing={"db"})
        self.assertEqual(set(manifest.restorable), {"db", "app"})  # the expected set keeps db
        self.assertEqual(manifest.missing, frozenset({"db"}))
        # a product that, honestly, restored only app and listed db as a gap still gets truth == false (rule 6),
        # and so `ok` must be false
        fork = {k: v for k, v in self.good_fork().items() if k != "db"}
        verdict = self.judge(fork=fork, manifest=manifest, lists=self.lists(restored={"app"}, gaps={"db"}))
        self.assertFalse(verdict.ok)
        self.assertIn("observed missing captures", " ".join(verdict.reasons))
        self.assertIn("restorable db is absent", " ".join(verdict.reasons))

    def test_the_recorded_manifest_cannot_be_adjusted_afterwards(self) -> None:
        declared = {"db": {"items": 1000, "tags": ["a"]}}
        manifest = truth_module.Manifest.record(declared, {})
        declared["db"]["items"] = 1  # the caller's own dict is not the manifest's
        self.assertEqual(manifest.restorable["db"]["items"], 1000)
        def add_service() -> None:
            manifest.restorable["x"] = {}  # type: ignore[index]

        def change_sentinel() -> None:
            manifest.restorable["db"]["items"] = 5  # type: ignore[index]

        def clear_missing() -> None:
            manifest.missing = frozenset({"db"})  # type: ignore[misc]

        for mutate in (add_service, change_sentinel):
            with self.assertRaises(TypeError):
                mutate()
        with self.assertRaises(dataclasses.FrozenInstanceError):
            clear_missing()
        self.assertEqual(manifest.restorable["db"]["tags"], ("a",))
        self.assertEqual(manifest.to_json()["restorable"], {"db": {"items": 1000, "tags": ["a"]}})

    def test_load_bounds_use_a_comparator_and_ok_must_equal_truth(self) -> None:
        manifest = truth_module.Manifest.record({"app": {"tokens": 100, "files": 100}}, {})
        fork = {"app": self.F(True, True, {"tokens": 140, "files": 141})}
        lists = truth_module.ProductLists(frozenset({"app"}), frozenset(), frozenset())
        at_least = lambda actual, expected: (actual["tokens"] >= expected["tokens"]
                                             and 0 <= actual["files"] - actual["tokens"] <= 1)
        self.assertFalse(truth_module.truth(fork, manifest, lists).ok)
        verdict = truth_module.truth(fork, manifest, lists, compare={"app": at_least})
        self.assertTrue(verdict.ok)
        self.assertIsNone(truth_module.ok_agrees(True, verdict))
        self.assertIn("ok=False", truth_module.ok_agrees(False, verdict))
        self.assertIn("ok=True", truth_module.ok_agrees(True, truth_module.Truth(False, ("x",))))

    def test_truth_requires_the_products_lists(self) -> None:
        with self.assertRaises(TypeError):
            truth_module.truth(self.good_fork(), self.manifest())  # type: ignore[call-arg]


class WitnessTests(unittest.TestCase):
    def test_process_sampler_flags_marsh_loads_and_the_engine_only(self) -> None:
        clean = ("  1 Fri Nov  6 10:00:00 2026 /sbin/init\n 55 Fri Nov  6 10:00:01 2026 dockerd -H unix:///var/run/docker.sock\n"
                 "123 Fri Nov  6 10:00:02 2026 sh -c while :; do ps -eo pid,lstart,args; echo '--- sample'; sleep 0.1; done\n"
                 "--- sample\n 77 Fri Nov  6 10:00:03 2026 marshmallow-test\n")
        self.assertEqual(witness.scan_process_samples(clean), [])
        for line in ["90 Fri Nov  6 10:00:00 2026 /usr/local/bin/marsh --internal-services relay",
                     "91 Fri Nov  6 10:00:00 2026 /marsh/marshd --home /x", "92 Fri Nov  6 10:00:00 2026 docker load -i x.tar",
                     "93 Fri Nov  6 10:00:00 2026 docker exec marsh-svc-engine sh", "94 Fri Nov  6 10:00:00 2026 marsh"]:
            with self.subTest(line=line):
                self.assertEqual(witness.scan_process_samples(clean + line + "\n"), [line.strip()])

    def test_event_log_flags_the_engine_loads_and_created_containers(self) -> None:
        engine = json.dumps({"Type": "container", "Action": "create",
                             "Actor": {"ID": "a" * 64, "Attributes": {"name": "marsh-svc-engine", "image": "x@sha256:1"}}})
        load = json.dumps({"Type": "image", "Action": "load", "Actor": {"ID": "sha256:1", "Attributes": {}}})
        other = json.dumps({"Type": "container", "Action": "create", "Actor": {"ID": "b" * 64, "Attributes": {"name": "db"}}})
        pull = json.dumps({"Type": "image", "Action": "pull", "Actor": {"ID": "busybox", "Attributes": {}}})
        self.assertEqual(witness.scan_events([pull, "not json"]), [])
        self.assertEqual(len(witness.scan_events([engine, load, other, pull])), 3)

    def test_controlled_peer_helpers(self) -> None:
        home = pathlib.Path(tempfile.mkdtemp(prefix="nfx-peer-test-"))
        self.addCleanup(lambda: __import__("shutil").rmtree(home, ignore_errors=True))
        witness.set_agent_override(home, pathlib.Path("/private/tmp/p1"))
        self.assertEqual(witness.agent_override_file(home).read_text(), "/private/tmp/p1\n")
        self.assertEqual(witness.agent_override_file(home).parent, home / "test-holds")
        witness.clear_agent_override(home)
        self.assertFalse(witness.agent_override_file(home).exists())
        command = witness.peer_install_command("print('it\\'s')")
        self.assertIn("/run/nfx/peer.py", command)
        self.assertIn("'\\''", command)
        self.assertEqual(witness.peer_frames('{"t": 1}\nnot json\n'), [{"t": 1}, "not json"])


class CollectorTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = pathlib.Path(tempfile.mkdtemp(prefix="nfx-gc-test-"))
        self.addCleanup(lambda: __import__("shutil").rmtree(self.directory, ignore_errors=True))
        self.alive_run, self.dead_run = "250101000000aaaa", "250101000000bbbb"

    def record(self, run: str, created: dict, map_vms: dict | None = None) -> None:
        record = nfx_gc.RunRecord(self.directory, run, "smoke", self.directory / f"nfx-{run}")
        record.data["created"] = created
        record.data["map_vms"] = map_vms or {}
        record.save()

    def decide(self, inventory: dict, alive: set | None = None) -> dict:
        report = nfx_gc.collect("sbx", self.directory, dry_run=True, inventory=inventory, alive=alive or set())
        return {row["name"]: (row["action"], row["why"]) for row in report}

    def test_only_affirmative_finished_run_evidence_removes_anything(self) -> None:
        self.record(self.alive_run, {f"nfx-{self.alive_run}-probe": "id1"})
        self.record(self.dead_run, {f"nfx-{self.dead_run}-probe": "id2", f"nfx-{self.dead_run}-half": None},
                    {"marsh-xnfx01-k-deadbeef": "id3"})
        inventory = {
            f"nfx-{self.alive_run}-probe": "id1", f"nfx-{self.dead_run}-probe": "id2",           # alive / dead, IDs match
            f"nfx-{self.dead_run}-half": "idx",                                                    # intent without an ID
            "marsh-xnfx01-k-deadbeef": "id3",                                                      # dead run's map VM, ID matches
            "marsh-xnfx01-k-unknown0": "id4",                                                      # no record lists it
            "nfx-250101000000cccc-orphan": "id5",                                                  # encodes a run with no record
            "nfx-h1b-plain": "id6", "marsh-k-user0001": "id7"}                                     # not ours at all
        report = self.decide(inventory, alive={self.alive_run})  # the caller says this run is alive (it holds no lock)
        self.assertEqual(report[f"nfx-{self.alive_run}-probe"][0], "kept")
        self.assertIn("alive", report[f"nfx-{self.alive_run}-probe"][1])
        self.assertEqual(report[f"nfx-{self.dead_run}-probe"][0], "would remove")
        self.assertEqual(report["marsh-xnfx01-k-deadbeef"][0], "would remove")
        self.assertEqual(report[f"nfx-{self.dead_run}-half"][0], "kept")
        self.assertIn("ownership unproven", report[f"nfx-{self.dead_run}-half"][1])
        self.assertEqual(report["marsh-xnfx01-k-unknown0"][0], "kept")  # unknown owner is left alone, even with no live run
        self.assertEqual(report["nfx-250101000000cccc-orphan"][0], "kept")  # a name encoding a recordless run is not proof
        self.assertNotIn("nfx-h1b-plain", report)
        self.assertNotIn("marsh-k-user0001", report)

    def test_a_pid_in_a_record_decides_nothing(self) -> None:
        self.record(self.dead_run, {f"nfx-{self.dead_run}-probe": "id2"})
        path = self.directory / f"{self.dead_run}.json"
        for pid in (os.getpid(), 1, 0):  # a live pid, init, no pid: the record's pid is display only
            data = json.loads(path.read_text())
            data["pid"] = pid
            path.write_text(json.dumps(data))
            self.assertEqual(self.decide({f"nfx-{self.dead_run}-probe": "id2"})[f"nfx-{self.dead_run}-probe"][0],
                             "would remove")
        self.assertNotIn("identity", nfx_gc.RunRecord(self.directory, "x", "smoke", self.directory).data)

    def test_a_replacement_under_a_recorded_name_is_never_removed(self) -> None:
        self.record(self.dead_run, {f"nfx-{self.dead_run}-probe": "id2"})
        report = self.decide({f"nfx-{self.dead_run}-probe": "impostor-id"})
        self.assertEqual(report[f"nfx-{self.dead_run}-probe"][0], "kept")
        self.assertIn("replacement", report[f"nfx-{self.dead_run}-probe"][1])

    def test_no_records_means_nothing_is_removed(self) -> None:
        report = self.decide({"marsh-xnfx01-k-aaaaaaaa": "1", "nfx-250101000000dddd-x": "2"})
        self.assertEqual({a for a, _ in report.values()}, {"kept"})

    def test_a_failed_removal_keeps_the_record(self) -> None:
        self.record(self.dead_run, {f"nfx-{self.dead_run}-probe": "id2"})
        inventory = {f"nfx-{self.dead_run}-probe": "id2"}
        import unittest.mock as mock
        with mock.patch.object(nfx_gc, "remove_sandbox", side_effect=ValueError("sbx rm failed")):
            report = nfx_gc.collect("sbx", self.directory, inventory=inventory)
        self.assertEqual(report[0]["action"], "failed")
        self.assertTrue((self.directory / f"{self.dead_run}.json").exists())
        self.assertEqual([r["action"] for r in report if r["name"].startswith("run ")], [])  # record not cleared
        # a record is cleared only when everything it lists is gone
        again = nfx_gc.collect("sbx", self.directory, dry_run=True, inventory={})
        self.assertEqual(again[-1]["action"], "would clear record")

    def test_run_records_are_persisted_intent_first_and_never_overwritten(self) -> None:
        record = nfx_gc.RunRecord(self.directory, "261106000000aaaa", "smoke", self.directory)
        record.intend("nfx-261106000000aaaa-probe")
        self.assertEqual(json.loads((self.directory / "261106000000aaaa.json").read_text())["created"],
                         {"nfx-261106000000aaaa-probe": None})
        record.identify("nfx-261106000000aaaa-probe", "u1")
        self.assertEqual(json.loads((self.directory / "261106000000aaaa.json").read_text())["created"],
                         {"nfx-261106000000aaaa-probe": "u1"})
        duplicate = nfx_gc.RunRecord(self.directory, "261106000000aaaa", "smoke", self.directory)
        with self.assertRaises(FileExistsError):
            duplicate.save(exclusive=True)
        self.assertEqual(json.loads((self.directory / "261106000000aaaa.json").read_text())["created"],
                         {"nfx-261106000000aaaa-probe": "u1"})
        record.sync_map({"vms": {"marsh-xnfx01-k-aaaaaaaa": {"uuid": "u2"}, "marsh-xnfx01-s-bbbbbbbb": {}}})
        self.assertEqual(json.loads((self.directory / "261106000000aaaa.json").read_text())["map_vms"],
                         {"marsh-xnfx01-k-aaaaaaaa": "u2", "marsh-xnfx01-s-bbbbbbbb": None})


class HostStateTests(unittest.TestCase):
    RUN = "2611061039191b2c"

    def rule(self, rule_id: str, applies: str = "all", decision: str = "allow", resources=("x",)) -> dict:
        return {"id": rule_id, "name": rule_id, "scope": "global", "applies_to": applies,
                "resource_type": "network", "decision": decision, "resources": list(resources)}

    def test_names_this_harness_may_touch(self) -> None:
        for name, ours in [(f"nfx-{self.RUN}-probe", True), ("marsh-xnfx01-k-abcdefgh", True),
                           ("nfx-h1b-plain", False), ("marsh-k-abcdefgh", False), ("nfx-261106103919ffff-probe", False)]:
            self.assertEqual(nfx_host.is_nfx_name(name, self.RUN), ours, name)
        self.assertTrue(nfx_host.is_nfx_name("nfx-261106103919ffff-probe"))
        self.assertFalse(nfx_host.is_nfx_name("nfx-h1b-plain"))

    def test_policy_rules_we_left_are_errors_and_baseline_changes_are_inconclusive_unless_tolerated(self) -> None:
        before = {"a": self.rule("a"), "u": self.rule("u", "sandbox:marsh-k-user")}
        after = {"a": self.rule("a"), "u": self.rule("u", "sandbox:marsh-k-user"),
                 "b": self.rule("b", f"sandbox:nfx-{self.RUN}-probe"), "c": self.rule("c"),
                 "d": self.rule("d", "sandbox:marsh-k-other")}
        errors, notes = nfx_host.policy_errors(before, after, self.RUN)
        self.assertEqual(sorted(e.split()[4] for e in errors), ["b", "c"])
        self.assertEqual(len(notes), 1)  # d belongs to someone else: noted, not ours
        self.assertEqual(nfx_host.policy_errors(before, before, self.RUN), ([], []))
        errors, notes = nfx_host.policy_errors(before, {"a": before["a"]}, self.RUN)  # the foreign-scoped rule vanished
        self.assertEqual((len(errors), notes), (1, []))
        errors, notes = nfx_host.policy_errors(before, {"a": before["a"]}, self.RUN, tolerate_baseline_changes=True)
        self.assertEqual((errors, len(notes)), ([], 1))
        errors, _ = nfx_host.policy_errors(before, {"u": before["u"]}, self.RUN, tolerate_baseline_changes=True)
        self.assertEqual(len(errors), 1)  # a global baseline rule is never excused

    def test_policy_rules_are_attributed_by_exact_attributes_not_by_a_diff(self) -> None:
        request = nfx_host.parse_policy_request(("deny", "network", "--sandbox", "nfx-x-svc", "example.com,b.org"))
        self.assertEqual(request, {"decision": "deny", "applies_to": "sandbox:nfx-x-svc",
                                   "resources": ["b.org", "example.com"]})
        mine = self.rule("ours", "sandbox:nfx-x-svc", "deny", ("example.com", "b.org"))
        concurrent = self.rule("foreign-id", "sandbox:marsh-k-user", "allow", ("other.example",))
        self.assertEqual(nfx_host.attribute_policy_rules(request, {"foreign-id": concurrent, "ours": mine}), ["ours"])
        self.assertEqual(nfx_host.attribute_policy_rules(request, {"foreign-id": concurrent}), [])
        twin = {**mine, "id": "twin"}
        with self.assertRaisesRegex(ValueError, "indistinguishable"):
            nfx_host.attribute_policy_rules(request, {"ours": mine, "twin": twin})
        self.assertEqual(nfx_host.parse_policy_request(("allow", "network", "a.org"))["applies_to"], "all")
        with self.assertRaises(ValueError):
            nfx_host.parse_policy_request(("rm", "network", "--id", "x"))

    def test_removal_needs_a_recorded_id_that_still_matches(self) -> None:
        import unittest.mock as mock
        name = f"nfx-{self.RUN}-probe"
        with self.assertRaisesRegex(ValueError, "no recorded stable ID"):
            nfx_host.remove_sandbox("sbx", name, None, {})
        with mock.patch.object(nfx_host, "stock_vm_inventory", return_value={name: "impostor"}), \
                self.assertRaisesRegex(ValueError, "replaced"):
            nfx_host.remove_sandbox("sbx", name, "recorded", {})
        with self.assertRaisesRegex(ValueError, "pre-existing"):
            nfx_host.remove_sandbox("sbx", name, "recorded", {name: "recorded"})
        with self.assertRaisesRegex(ValueError, "not an nfx sandbox"):
            nfx_host.remove_sandbox("sbx", "marsh-k-user", "recorded", {})

    def test_the_baseline_oracle_is_not_vacuous(self) -> None:
        import scenarios_harness
        self.assertEqual(scenarios_harness.oracle_selftest(), [])
        before = {"user-vm": "id1"}
        oracle = lambda after, **kw: nfx_host.baseline_oracle(before, after, self.RUN, set(), {}, set(), {}, {}, **kw)
        self.assertEqual(oracle(dict(before)), ([], []))
        self.assertTrue(oracle({})[0])  # a disappeared baseline VM is an inconclusive oracle, an error
        self.assertTrue(oracle({"user-vm": "other"})[0])
        self.assertEqual(oracle({}, tolerate=True)[0], [])
        self.assertTrue(oracle({**before, "marsh-xnfx01-k-aaaaaaaa": "z"})[0])  # nobody's: not provably foreign
        vm = "marsh-xnfx01-k-aaaaaaaa"
        claimed = lambda claims, live="z": nfx_host.baseline_oracle(before, {**before, vm: live}, self.RUN, set(), claims,
                                                                    set(), {}, {})[0]
        self.assertFalse(claimed({vm: "z"}))  # another run's record lists this name with this very ID
        self.assertTrue(claimed({vm: "recorded-other"}))  # same name, a different ID: not the recorded VM
        self.assertTrue(claimed({vm: None}))  # the record has the intent but no ID: ownership unproven
        self.assertFalse(oracle({**before, "nfx-250101000000dead-decoy": "z"})[0])  # foreign by its run id
        self.assertEqual(nfx_host.foreign_names({**before, "nfx-250101000000dead-decoy": "z", f"nfx-{self.RUN}-p": "q"},
                                                before, self.RUN, set()), ["nfx-250101000000dead-decoy"])


class OwnershipAndControlTests(unittest.TestCase):
    """Round 3: nothing is adopted without proof, a negative control that cannot be trusted is invalid, a baseline
    that was only tolerated is non-qualifying, and a timed-out child is interrupted, never killed."""

    def services(self, **attributes):
        suite = object.__new__(services_uat.Services)
        directory = pathlib.Path(tempfile.mkdtemp(prefix="nfx-own-test-"))
        self.addCleanup(lambda: __import__("shutil").rmtree(directory, ignore_errors=True))
        suite.sbx, suite.environment, suite.raw_ids = "sbx", {}, {}
        suite.record = nfx_gc.RunRecord(directory, "261106000000aaaa", "smoke", directory)
        suite.directory = directory
        for key, value in attributes.items():
            setattr(suite, key, value)
        return suite

    def run_create(self, suite, name, before, after, returncode):
        inventories = iter([before, after])
        done = subprocess.CompletedProcess([], returncode, b"", b"name taken" if returncode else b"")
        suite.xrun = lambda argv, **kw: done
        with unittest.mock.patch.object(services_uat, "stock_vm_inventory", lambda *a, **k: next(inventories)):
            return suite.create_sandbox(name, suite.directory)

    def test_a_sandbox_is_recorded_only_after_a_successful_create(self) -> None:
        name = "nfx-261106000000aaaa-probe"
        suite = self.services()
        self.run_create(suite, name, {"other": "u0"}, {"other": "u0", name: "u1"}, 0)
        self.assertEqual(suite.record.data["created"], {name: "u1"})
        self.assertEqual(suite.raw_ids, {name: "u1"})

    def test_a_name_collision_is_refused_and_never_adopted(self) -> None:
        name = "nfx-261106000000aaaa-probe"
        suite = self.services()
        with self.assertRaisesRegex(services_uat.Fail, "never adopted"):
            self.run_create(suite, name, {name: "foreign-id"}, {name: "foreign-id"}, 0)
        self.assertEqual(suite.record.data["created"], {})  # not even an intent: nothing was attempted
        self.assertEqual(suite.raw_ids, {})

    def test_a_failed_or_ambiguous_create_is_not_adopted_and_keeps_the_intent(self) -> None:
        name = "nfx-261106000000aaaa-probe"
        for after, fragment in [({}, "failed"), ({name: "appeared-anyway"}, "ambiguous")]:
            suite = self.services()
            with self.assertRaisesRegex(services_uat.Fail, fragment):
                self.run_create(suite, name, {}, after, 1)
            self.assertEqual(suite.record.data["created"], {name: None})  # intent kept, no ID: the collector keeps it
            self.assertEqual(suite.raw_ids, {})
        suite = self.services()
        with self.assertRaisesRegex(services_uat.Fail, "not in `sbx ls`"):  # rc 0 but nothing there
            self.run_create(suite, name, {}, {}, 0)
        self.assertEqual(suite.raw_ids, {})

    def test_another_runs_record_is_used_for_its_decoy_not_ours(self) -> None:
        suite = self.services()
        other = nfx_gc.RunRecord(suite.directory, "250101000000beef", "smoke", suite.directory)
        name = "nfx-250101000000beef-decoy"
        inventories = iter([{}, {name: "d1"}])
        suite.xrun = lambda argv, **kw: subprocess.CompletedProcess([], 0, b"", b"")
        with unittest.mock.patch.object(services_uat, "stock_vm_inventory", lambda *a, **k: next(inventories)):
            suite.create_sandbox(name, suite.directory, record=other)
        self.assertEqual(other.data["created"], {name: "d1"})
        self.assertEqual(suite.record.data["created"], {})
        self.assertEqual(suite.raw_ids, {})  # this run's removal authority does not include it

    def test_the_expected_leak_is_exactly_one_error_and_nothing_that_merely_mentions_the_probe(self) -> None:
        probe = "nfx-261106000000aaaa-probe"
        exact = f"this run's sandboxes remain: ['{probe}']"
        expected, rest = services_uat.split_expected_leak(
            [exact, f"policy rule r1 scoped to sandbox:{probe} remains", "setting changed", f"{exact} and more"], probe)
        self.assertEqual(expected, [exact])
        self.assertEqual(len(rest), 3)
        self.assertEqual(services_uat.split_expected_leak([f"this run's sandboxes remain: ['{probe}', 'x']"], probe)[0], [])

    def project(self, modes=None, state="ready"):
        return {"state": state, **({"test_breaks_agent": modes} if modes is not None else {})}

    def test_agent_attestation_fails_closed(self) -> None:
        suite = self.services(marsh_requested=["capture-block-next-prompt"], active_breaks={}, features={"services-v1"},
                              agent_attested=False)
        for projects, fragment in [(None, "unavailable"), ([], "no services project is ready"),
                                   ([self.project(state="starting")], "no services project is ready"),
                                   ([self.project([])], "test_breaks_agent"),
                                   ([self.project(["other"])], "test_breaks_agent"),
                                   ([self.project(["capture-block-next-prompt"]), self.project([])], "test_breaks_agent")]:
            suite.agent_projects = lambda p=projects: p
            with self.subTest(projects=projects), self.assertRaisesRegex(services_uat.Fail, fragment):
                suite.attest_agent()
            self.assertFalse(suite.agent_attested)
        suite.agent_projects = lambda: [self.project(["capture-block-next-prompt"])]
        suite.attest_agent()
        self.assertTrue(suite.agent_attested)

    def test_a_scenario_that_never_attested_cannot_verify_a_break(self) -> None:
        suite = self.services(marsh_requested=["m"], active_breaks={}, features={"services-v1"}, agent_attested=False)
        suite.agent_projects = lambda: [self.project(["m"])]
        with self.assertRaisesRegex(services_uat.Fail, "never attested"):
            suite.check_agent_breaks()  # the status is fine now, but the defect's operation was not gated by it
        suite.attest_agent()
        suite.check_agent_breaks()
        suite.agent_projects = lambda: None  # and it must still hold afterwards
        with self.assertRaisesRegex(services_uat.Fail, "unavailable"):
            suite.check_agent_breaks()
        self.services(marsh_requested=["m"], active_breaks={}, features=set(), agent_attested=False).check_agent_breaks()

    def test_zero_mode_runs_need_the_agent_attested_or_proved_absent(self) -> None:
        for features, requested in (({"services-v1"}, []), ({"services-v1"}, ["m"])):  # normal / pass 2 / pass 3
            suite = self.services(marsh_requested=requested, active_breaks={}, features=features, agent_attested=False)
            suite.agent_projects = lambda: [self.project(requested or None)]
            problem = suite.agent_gate(BY_ID["SVC-VM-01"])
            self.assertIn("never attested", problem or "", (features, requested))  # an agent scenario that did not attest
            suite.attest_agent()  # the scenario attested after starting its services VM
            self.assertIsNone(suite.agent_gate(BY_ID["SVC-VM-01"]))
        suite = self.services(marsh_requested=[], active_breaks={}, features={"services-v1"}, agent_attested=True)
        suite.agent_projects = lambda: [self.project(["unexpected"])]  # a mode nobody asked for
        self.assertIn("test_breaks_agent", suite.agent_gate(BY_ID["SVC-VM-01"]) or "")
        suite.agent_projects = lambda: None  # status unavailable: fail closed
        self.assertIn("unavailable", suite.agent_gate(BY_ID["SVC-VM-01"]) or "")
        suite.agent_projects = lambda: [self.project(None, state="starting")]
        self.assertIn("no services project is ready", suite.agent_gate(BY_ID["SVC-VM-01"]) or "")
        # a scenario that needs no VM proves that no agent exists instead
        suite = self.services(marsh_requested=[], active_breaks={}, features={"services-v1"}, agent_attested=False)
        suite.agent_projects = lambda: []
        self.assertIsNone(suite.agent_gate(BY_ID["REL-01"]))
        suite.agent_projects = lambda: [self.project(None)]
        self.assertIn("needs no services VM", suite.agent_gate(BY_ID["REL-01"]) or "")
        suite.agent_projects = lambda: None
        self.assertIn("cannot prove", suite.agent_gate(BY_ID["REL-01"]) or "")
        # before wave 1 and for the HARNESS scenarios there is nothing to attest
        none = self.services(marsh_requested=[], active_breaks={}, features=set(), agent_attested=False)
        self.assertIsNone(none.agent_gate(BY_ID["SVC-VM-01"]))
        self.assertIsNone(suite.agent_gate(BY_ID["HARNESS-01"]))

    def injection_stub(self, document: str, status: int, label: str = "job"):
        import scenarios_harness

        class Stub(scenarios_harness.HarnessScenarios):
            def check(self, name, condition, detail):
                if not condition:
                    raise scenarios_harness.CheckFailed(name, detail)

            def xrun(self, argv, **kwargs):
                script = argv[-1]
                nonce = script.split(f"nfx-{label}-ran-")[1].split(";")[0]
                out = (f"nfx-{label}-ran-{nonce}\nMARSH_TEST_BREAK=demo\nnfx-{label}-probe-{nonce}\n{document}\n"
                       f"nfx-{label}-end-{nonce}:{status}\n")
                return subprocess.CompletedProcess(argv, 0, out.encode(), b"")

            def side_status(self, side, note, env=None):
                return {"endpoint_owner": {"pid": 7}}

        side = type("Side", (), {"project": pathlib.Path("/tmp"), "environment": {}})()
        return Stub(), side

    def test_the_injected_marsh_operation_must_exit_zero_not_just_print_a_document(self) -> None:
        import scenarios_harness
        good = json.dumps({"schema": "marsh.jobs/v1", "jobs": []})
        stub, side = self.injection_stub(good, 0)
        stub.injection(side, "job", 7, ["marsh", "run", "{script}"], probe="marsh jobs --json")  # a clean probe passes
        stub, side = self.injection_stub(good, 7)  # the same valid document, but the operation failed
        with self.assertRaisesRegex(scenarios_harness.CheckFailed, "exited '7', not 0"):
            stub.injection(side, "job", 7, ["marsh", "run", "{script}"], probe="marsh jobs --json")
        stub, side = self.injection_stub(good, 0)  # the status marker missing altogether is a failure too
        original = stub.xrun
        stub.xrun = lambda argv, **kw: subprocess.CompletedProcess(
            argv, 0, original(argv).stdout.replace(b":0", b""), b"")
        with self.assertRaises(scenarios_harness.CheckFailed):
            stub.injection(side, "job", 7, ["marsh", "run", "{script}"], probe="marsh jobs --json")
        stub, side = self.injection_stub("not json", 0)
        with self.assertRaisesRegex(scenarios_harness.CheckFailed, "no JSON object"):
            stub.injection(side, "job", 7, ["marsh", "run", "{script}"], probe="marsh jobs --json")

    def test_the_matrix_runs_pass_2_for_every_cell_with_its_companions(self) -> None:
        steps = breaks.plan_commands(BY_ID["PERF-10"], ["--prefix", "p"], "s.py", "py")
        self.assertEqual([s["pass"] for s in steps], [1, 2, 3])
        self.assertIn("slow-freeze-long", steps[1]["argv"])
        self.assertEqual(steps[1]["argv"][steps[1]["argv"].index("--break") + 1], "slow-freeze-long")
        self.assertNotIn("--break-defect", steps[1]["argv"])
        self.assertEqual(steps[2]["argv"][steps[2]["argv"].index("--break") + 1], "capture-block-next-prompt,slow-freeze-long")
        self.assertEqual(steps[2]["argv"][steps[2]["argv"].index("--break-defect") + 1], "capture-block-next-prompt")
        for scenario in ("HARNESS-01", "HARNESS-02", "HARNESS-03"):
            entry = BY_ID[scenario]
            cells = [c for c in entry.breaks if c.kind in breaks.VERIFIABLE_HERE]
            plan = breaks.plan_commands(entry, [], "s.py", "py")
            self.assertEqual(len([p for p in plan if p["pass"] == 3]), len(cells), scenario)
            for cell in cells:
                self.assertTrue(any(p["pass"] == 2 and p["cell"] is not None and p["cell"].mode in (cell.mode, c.mode)
                                    for p in plan for c in cells if c.prerequisites == cell.prerequisites), scenario)

    def test_a_tolerated_baseline_is_non_qualifying_everywhere_the_matrix_looks(self) -> None:
        self.assertTrue(services_uat.child_ok(0, "all good"))
        self.assertFalse(services_uat.child_ok(0, "NON-QUALIFYING: baseline changes were tolerated"))
        self.assertFalse(services_uat.child_ok(1, "x"))
        verdict = services_uat.pass3_result
        self.assertEqual(verdict(0, "break verdict A m: verified\nNON-QUALIFYING: x", "A", "m"), "non-qualifying")
        errors, notes = nfx_host.baseline_oracle({"a": "1"}, {}, "2611061039191b2c", set(), {}, set(), {}, {}, True)
        self.assertEqual(errors, [])
        self.assertTrue(any("[tolerated]" in n for n in notes), notes)  # the suite turns this into non-qualifying
        errors, _ = nfx_host.baseline_oracle({"a": "1"}, {}, "2611061039191b2c", set(), {}, set(), {}, {}, False)
        self.assertTrue(errors)  # not tolerated: an error
        schema = json.loads((DIRECTORY / "services" / "evidence.schema.json").read_text())
        self.assertIn("oracle", schema["required"])
        self.assertEqual(schema["properties"]["oracle"]["enum"], ["equal", "inconclusive-tolerated"])

    def test_a_timed_out_child_is_interrupted_and_never_killed(self) -> None:
        import io
        child = ("import signal,sys,time\n"
                 "signal.signal(signal.SIGINT, lambda *a: (print('cleaning up',flush=True), time.sleep(1), sys.exit(0)))\n"
                 "print('started',flush=True)\ntime.sleep(60)")
        with unittest.mock.patch.object(subprocess.Popen, "kill", side_effect=AssertionError("a child must never be killed")), \
                contextlib.redirect_stdout(io.StringIO()):
            code, output = services_uat.run_child([sys.executable, "-c", child], timeout=1, budget=30)
        self.assertEqual(code, -9)  # a timeout is never a success
        self.assertIn("interrupted and cleaned up", output)
        self.assertIn("cleaning up", output)
        stubborn = ("import signal,time\nsignal.signal(signal.SIGINT, signal.SIG_IGN)\nprint('started',flush=True)\n"
                    "time.sleep(60)")
        with unittest.mock.patch.object(subprocess.Popen, "kill", side_effect=AssertionError("a child must never be killed")), \
                contextlib.redirect_stdout(io.StringIO()):
            code, output = services_uat.run_child([sys.executable, "-c", stubborn], timeout=1, budget=1)
        self.assertEqual(code, -9)
        self.assertIn("terminated after", output)  # SIGTERM after the cleanup budget; still no kill

    def test_a_changed_global_setting_fails_the_run_and_is_never_repaired(self) -> None:
        directory = pathlib.Path(tempfile.mkdtemp(prefix="nfx-setting-test-"))
        self.addCleanup(lambda: __import__("shutil").rmtree(directory, ignore_errors=True))
        state, log = directory / "state", directory / "log"
        state.write_text('{"value": "8", "source": "default"}')
        fake = directory / "sbx"
        fake.write_text(f"#!/bin/sh\necho \"$@\" >> '{log}'\n"
                        f"case \"$1 $2\" in 'settings get') cat '{state}';; *) echo \"unexpected $*\" >&2; exit 3;; esac\n")
        fake.chmod(0o755)
        suite = self.services(sbx=str(fake), stock_before={}, leak_on_purpose=False, run_id="261106000000aaaa",
                              tolerate=False, notes=[], tolerated=[], policy_before={}, foreign=[])
        suite.policy = type("P", (), {"rollback": lambda self: []})()
        suite.setting_before = nfx_host.read_setting(str(fake))
        suite.owned_stock_names = lambda: set()
        suite.other_claims = lambda: {}
        with unittest.mock.patch.object(services_uat, "stock_vm_inventory", lambda *a, **k: {}), \
                unittest.mock.patch.object(services_uat, "policy_rules", lambda *a, **k: {}):
            self.assertEqual(suite.h3_cleanup(), [])
            self.assertEqual(suite.h3_verify(), [])  # unchanged: clean
            state.write_text('{"value": "16", "source": "override"}')  # someone changed the global setting
            self.assertEqual(suite.h3_cleanup(), [])  # cleanup does not touch it
            errors = suite.h3_verify()
        self.assertTrue(any("sandbox.disk.dockerVolume" in e and "'16'" in e for e in errors), errors)
        self.assertEqual(json.loads(state.read_text())["value"], "16")  # not repaired
        self.assertTrue(all(line.startswith("settings get") for line in log.read_text().splitlines()), log.read_text())
        self.assertFalse(hasattr(nfx_host, "restore_setting"))

    def acp_context(self, mode: str):
        import scenarios_harness
        directory = pathlib.Path(tempfile.mkdtemp(prefix="nfx-acp-test-"))
        self.addCleanup(lambda: __import__("shutil").rmtree(directory, ignore_errors=True))
        marsh = directory / "marsh"
        marsh.write_text(f"#!{sys.executable}\n" + """import json,sys
mode = %r
def send(o): print(json.dumps({"jsonrpc": "2.0", **o}), flush=True)
for line in sys.stdin:
    m = json.loads(line)
    if m.get("method") == "initialize":
        send({"id": 1, "result": {}} if mode != "init-error" else {"id": 1, "error": {"code": -1, "message": "no"}})
    elif m.get("method") == "session/new":
        send({"id": 2, "result": {"sessionId": "s"}})
    elif m.get("method") == "session/prompt":
        send({"method": "session/update", "params": {"update": {"sessionUpdate": "agent_message_chunk", "content": {"text": "hello"}}}})
        if mode == "prompt-error":
            send({"id": 3, "error": {"code": -2, "message": "boom"}})
        else:
            send({"id": 3, "result": {"stopReason": "cancelled" if mode == "cancelled" else "end_turn"}})
        break
""" % mode)
        marsh.chmod(0o755)
        suite = type("Suite", (), {"marsh": str(marsh), "project": directory, "environment": dict(os.environ), "sbx": "sbx",
                                   "root": directory})()
        return scenarios_harness._Context(suite, None)

    def test_acp_exec_rejects_json_rpc_errors_and_any_stop_reason_but_end_turn(self) -> None:
        self.assertEqual(self.acp_context("ok").agent_exec("agent", "true", 30), ["hello"])
        for mode, fragment in [("init-error", "request 1 failed"), ("prompt-error", "request 3 failed"),
                               ("cancelled", "stopReason")]:
            with self.subTest(mode=mode), self.assertRaisesRegex(RuntimeError, fragment):
                self.acp_context(mode).agent_exec("agent", "true", 30)


class ScopeTests(unittest.TestCase):
    def test_a_sandbox_is_adopted_only_if_its_uuid_matches_the_ownership_map(self) -> None:
        directory = pathlib.Path(tempfile.mkdtemp(prefix="nfx-scope-test-"))
        self.addCleanup(lambda: __import__("shutil").rmtree(directory, ignore_errors=True))

        class Base:
            def __init__(self) -> None:
                self.owned_vm_identities = {"marsh-xnfx01-k-aaaaaaaa": "live-uuid"}
                self.control_home = directory

            def remember_owned_status(self, status) -> None:
                pass

        class Scope(scopes.OwnedLeftovers, Base):
            pass

        (directory / "vm-ownership.json").write_text(json.dumps({"version": 2, "vms": {
            "marsh-xnfx01-k-aaaaaaaa": {"purpose": "kit", "key": "k", "uuid": "live-uuid"}}}))
        Scope().remember_owned_status({})  # name and UUID match: adopted
        for document, message in [
                ({"vms": {"marsh-xnfx01-k-aaaaaaaa": {"uuid": "recorded-uuid"}}}, "differs"),   # an impostor under the name
                ({"vms": {"marsh-xnfx01-k-aaaaaaaa": {}}}, "differs"),                          # the map has no UUID yet
                ({"vms": {}}, "differs")]:                                                      # the map does not list it
            (directory / "vm-ownership.json").write_text(json.dumps(document))
            scope = Scope()
            started = time.monotonic()
            with self.assertRaisesRegex(AssertionError, message):
                scope.remember_owned_status({})
            self.assertNotIn("marsh-xnfx01-k-aaaaaaaa", scope.owned_vm_identities)
        self.assertLess(time.monotonic() - started, 30)

    def test_the_directly_launched_daemon_is_reaped_when_it_cannot_be_used(self) -> None:
        process = subprocess.Popen(["/bin/sleep", "60"], start_new_session=True)
        self.assertTrue(scopes.reap(process))
        self.assertIsNotNone(process.poll())
        self.assertTrue(scopes.reap(None))
        self.assertTrue(scopes.reap(process))

    def test_the_break_launch_passes_the_explicit_flag_only_when_asked(self) -> None:
        directory = pathlib.Path(tempfile.mkdtemp(prefix="nfx-launch-test-"))
        self.addCleanup(lambda: __import__("shutil").rmtree(directory, ignore_errors=True))
        fake = directory / "marshd"
        fake.write_text("#!/bin/sh\necho \"$@\" > '%s/args'; echo \"${MARSH_TEST_BREAK-unset}\" > '%s/env'\nexit 3\n"
                        % (directory, directory))
        fake.chmod(0o755)
        (directory / "home").mkdir()
        scope = type("S", (), {"environment": {"MARSH_TEST_BREAK": "stale", "PATH": "/usr/bin:/bin"},
                               "home": directory / "home", "project": directory, "scope_started": False})()
        for modes, flag, args, env in [(["demo", "x"], True, "--home %s --test-breaks-from-env" % (directory / "home"), "demo,x"),
                                       ([], True, "--home %s --test-breaks-from-env" % (directory / "home"), "unset"),
                                       (["demo"], False, "--home %s" % (directory / "home"), "demo")]:
            with self.assertRaises(scopes.Fail):  # the fake exits at once: launch reports it
                scopes.launch_daemon(scope, fake, modes, directory / "log", flag=flag)
            self.assertFalse(scope.scope_started)  # nothing answered, so nothing is owned and cleanup can proceed
            self.assertEqual((directory / "args").read_text().strip(), args)
            self.assertEqual((directory / "env").read_text().strip(), env)


if __name__ == "__main__":
    unittest.main()
