#!/usr/bin/env python3
"""Focused tests for dependency-free acceptance evidence validation."""

from __future__ import annotations

import importlib.util
import json
import pathlib
import unittest


DIRECTORY = pathlib.Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("marsh_acceptance", DIRECTORY / "run.py")
assert SPEC is not None and SPEC.loader is not None
RUN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUN)
SCHEMA = json.loads((DIRECTORY / "result.schema.json").read_text(encoding="utf-8"))


def valid_result() -> dict[str, object]:
    return {
        "schema": "marsh.acceptance-result/v1",
        "started_at": "2026-09-20T12:00:00Z",
        "finished_at": "2026-09-20T12:00:01Z",
        "outcome": "passed",
        "environment": {
            "host_platform": "macOS-arm64",
            "source_revision": "a" * 40,
            "source_dirty": True,
            "source_tree_sha256": f"sha256:{'e' * 64}",
            "marsh_binary": "/tmp/marsh",
            "marsh_binary_sha256": f"sha256:{'b' * 64}",
            "guest_artifact_sha256": {
                name: f"sha256:{'f' * 64}"
                for name in ("marsh-linux-arm64", "marsh-worker-linux-arm64", "marsh-relay-linux-arm64")
            },
            "sbx_binary": "/opt/homebrew/bin/sbx",
            "sbx_version": "0.45.0",
            "fixture_kit_ref": f"example/fixture@sha256:{'c' * 64}",
            "command_mapping_sha256": f"sha256:{'d' * 64}",
            "project": "/tmp/project",
            "marsh_home": "/tmp/home",
            "job_limits": dict(RUN.JOB_LIMITS),
        },
        "checks": [{"name": "example", "outcome": "passed"}],
        "commands": [],
        "snapshots": [],
    }


class ResultSchemaTests(unittest.TestCase):
    def setUp(self) -> None:
        if self._testMethodName.startswith("test_cleanup_"):
            # These focused cleanup fixtures have no stock runtime. Explicitly
            # supply their empty baseline; never probe a real VM in a unit test.
            from unittest import mock
            baseline = mock.patch.object(RUN.IsolatedScopeCleanup, "stock_before", {}, create=True)
            inventory = mock.patch.object(RUN, "stock_vm_inventory", return_value={})
            baseline.start()
            self.stock_inventory = inventory.start()
            self.addCleanup(baseline.stop)
            self.addCleanup(inventory.stop)

    def test_daemon_endpoint_ignores_session_tmpdir(self) -> None:
        import hashlib
        import os
        import tempfile
        from unittest import mock

        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            scope = hashlib.sha256(os.fsencode(home.resolve())).hexdigest()[:16]
            expected = pathlib.Path("/tmp") / f"marsh-{os.getuid()}" / scope
            with mock.patch.object(RUN.tempfile, "gettempdir", return_value="/other-tmp"):
                self.assertEqual(RUN.daemon_endpoint_paths(home), (expected / "s", expected / "t"))

    def test_finish_reports_first_failed_check_detail(self) -> None:
        import contextlib
        import io
        import tempfile

        harness = RUN.Harness.__new__(RUN.Harness)
        harness.result = valid_result()
        harness.result["outcome"] = "failed"
        harness.result["checks"] = [
            {"name": "cold-parallel-prewarm", "outcome": "passed"},
            {
                "name": "prewarm-ready-and-subsecond",
                "outcome": "failed",
                "detail": "three cached jobs became ready in 1472 ms",
            },
            {"name": "shared-daemon-two-shells", "outcome": "not-run"},
        ]
        harness.cleanup_isolated_scope = lambda: []
        with tempfile.TemporaryDirectory() as directory:
            harness.evidence = pathlib.Path(directory)
            with contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(harness.finish(), 1)
            evidence = json.loads((harness.evidence / "result.json").read_text())
        self.assertEqual(
            evidence["failure"],
            "check prewarm-ready-and-subsecond failed: "
            "three cached jobs became ready in 1472 ms (1 not run)",
        )

    def test_empty_unexpected_exception_keeps_type_and_traceback(self) -> None:
        try:
            raise StopIteration
        except StopIteration as error:
            report = RUN.unexpected_failure(error)
        self.assertIn("builtins.StopIteration", report)
        self.assertIn("Traceback (most recent call last)", report)

    def test_full_gate_declares_every_marked_check(self) -> None:
        import ast

        tree = ast.parse((DIRECTORY / "run.py").read_text(encoding="utf-8"))
        marked = {
            call.args[0].value
            for call in ast.walk(tree)
            if isinstance(call, ast.Call)
            and isinstance(call.func, ast.Attribute)
            and call.func.attr == "mark"
            and call.args
            and isinstance(call.args[0], ast.Constant)
            and isinstance(call.args[0].value, str)
        }
        self.assertEqual(marked, set(RUN.CHECKS))
        self.assertEqual(len(RUN.CHECKS), len(set(RUN.CHECKS)))

    def test_cleanup_removes_only_status_owned_scope_resources(self) -> None:
        import os
        import shutil
        import subprocess
        import tempfile
        from unittest import mock

        class Scope(RUN.IsolatedScopeCleanup):
            pass

        scope = Scope()
        scope.root = pathlib.Path(tempfile.mkdtemp())
        scope.home = scope.root / "home"
        scope.home.mkdir()
        scope.sbx = "/test/sbx"
        scope.environment = {"MARSH_HOME": str(scope.home)}
        scope.initialize_scope_cleanup()
        scope.scope_started = True
        status = {
            "schema": "marsh.status/v1",
            "scope_id": "owned-scope",
            "daemon_id": "owned-daemon",
            "endpoint_owner": {"uid": os.getuid(), "pid": 4242},
            "workers": [
                {"scope_id": "owned-scope", "vm_id": "owned-worker"},
                {"scope_id": "other-scope", "vm_id": "user-worker"},
            ],
        }
        expected_shell = RUN.shell_vm_name(scope.home)
        inventory = {expected_shell: "shell-stable-id", "worker-name": "owned-worker"}
        self.stock_inventory.side_effect = [inventory, inventory, {}]
        commands: list[list[str]] = []
        identity = (os.getuid(), "start=1;exe=/test/marshd")

        def fake_run(argv: list[str], **_kwargs: object) -> subprocess.CompletedProcess[bytes]:
            commands.append(argv)
            return subprocess.CompletedProcess(argv, 0, b"", b"")

        def fake_remove(sbx: str, name: str, identifier: str, _baseline: object, **_kwargs: object) -> None:
            commands.append([sbx, "rm", "--force", name, identifier])

        try:
            with mock.patch.object(
                RUN,
                "stable_process_identity",
                side_effect=[identity, identity, None],
            ), mock.patch.object(
                RUN, "authenticate_daemon_control", return_value="test-token"
            ), mock.patch.object(
                RUN, "request_authenticated_daemon_shutdown"
            ), mock.patch.object(
                RUN.subprocess, "run", side_effect=fake_run
            ), mock.patch.object(
                RUN, "remove_owned_stock_vm", side_effect=fake_remove
            ), mock.patch.object(RUN.os, "kill") as kill:
                scope.remember_owned_status(status)
                self.assertEqual(scope.cleanup_isolated_scope(), [])
            kill.assert_not_called()
            # The owned worker is removed by its recorded identity; the shell
            # VM goes with the daemon's own shutdown (mocked here) or by its
            # identity. Nothing else is ever removed.
            self.assertIn(["/test/sbx", "rm", "--force", "worker-name", "owned-worker"], commands)
            self.assertTrue(all(command[-1] in {"owned-worker", "shell-stable-id"} for command in commands))
            self.assertFalse(scope.root.exists())
            self.assertNotIn("user-worker", repr(commands))
        finally:
            shutil.rmtree(scope.root, ignore_errors=True)

    def test_cleanup_rejects_same_name_replacement_before_any_stock_removal(self) -> None:
        import shutil
        import tempfile
        from unittest import mock

        scope = RUN.IsolatedScopeCleanup()
        scope.root = pathlib.Path(tempfile.mkdtemp())
        scope.home = scope.root / "home"
        scope.home.mkdir()
        scope.sbx = "/test/sbx"
        scope.environment = {}
        scope.initialize_scope_cleanup()
        # The daemon already exited; the formerly owned VM name was reused.
        scope.owned_scope_id = "test-scope"
        scope.owned_vms = {"worker-name"}
        scope.owned_vm_identities = {"worker-name": "original-id"}
        self.stock_inventory.return_value = {"worker-name": "replacement-id"}
        try:
            with mock.patch.object(RUN.subprocess, "run") as execute:
                errors = scope.cleanup_isolated_scope()
            self.assertTrue(any("replaced" in error for error in errors), errors)
            execute.assert_not_called()
            self.assertTrue(scope.root.exists(), "retain evidence on uncertain cleanup")
        finally:
            shutil.rmtree(scope.root)

    def test_cleanup_without_authenticated_status_never_touches_vms_or_pids(self) -> None:
        import shutil
        import tempfile
        from unittest import mock

        class Scope(RUN.IsolatedScopeCleanup):
            pass

        scope = Scope()
        scope.root = pathlib.Path(tempfile.mkdtemp())
        scope.home = scope.root / "home"
        scope.home.mkdir()
        scope.sbx = "/test/sbx"
        scope.environment = {"MARSH_HOME": str(scope.home)}
        scope.initialize_scope_cleanup()
        scope.scope_started = True
        try:
            with mock.patch.object(RUN.subprocess, "run") as run, mock.patch.object(
                RUN.os, "kill"
            ) as kill:
                errors = scope.cleanup_isolated_scope()
            self.assertIn("without authenticated status ownership", errors[0])
            run.assert_not_called()
            kill.assert_not_called()
            self.assertTrue(scope.root.exists())
        finally:
            shutil.rmtree(scope.root, ignore_errors=True)

    def test_cleanup_refuses_reused_daemon_pid_and_all_vm_actions(self) -> None:
        import os
        import shutil
        import tempfile
        from unittest import mock

        class Scope(RUN.IsolatedScopeCleanup):
            pass

        scope = Scope()
        scope.root = pathlib.Path(tempfile.mkdtemp())
        scope.home = scope.root / "home"
        scope.home.mkdir()
        scope.sbx = "/test/sbx"
        scope.environment = {"MARSH_HOME": str(scope.home)}
        scope.initialize_scope_cleanup()
        scope.scope_started = True
        status = {
            "schema": "marsh.status/v1",
            "scope_id": "owned-scope",
            "daemon_id": "owned-daemon",
            "endpoint_owner": {"uid": os.getuid(), "pid": 4242},
            "workers": [{"scope_id": "owned-scope", "vm_id": "owned-worker"}],
        }
        original = (os.getuid(), "start=1;exe=/test/marshd")
        reused = (os.getuid(), "start=2;exe=/test/unrelated")
        try:
            with mock.patch.object(
                RUN, "stable_process_identity", side_effect=[original, reused]
            ), mock.patch.object(
                RUN, "authenticate_daemon_control", return_value="test-token"
            ), mock.patch.object(RUN.subprocess, "run") as run, mock.patch.object(
                RUN.os, "kill"
            ) as kill:
                scope.remember_owned_status(status)
                errors = scope.cleanup_isolated_scope()
            self.assertIn("was reused", errors[0])
            run.assert_not_called()
            kill.assert_not_called()
            self.assertTrue(scope.root.exists())
        finally:
            shutil.rmtree(scope.root, ignore_errors=True)

    def test_cleanup_refuses_vm_actions_while_daemon_will_not_exit(self) -> None:
        import os
        import shutil
        import tempfile
        from unittest import mock

        class Scope(RUN.IsolatedScopeCleanup):
            pass

        scope = Scope()
        scope.root = pathlib.Path(tempfile.mkdtemp())
        scope.home = scope.root / "home"
        scope.home.mkdir()
        scope.sbx = "/test/sbx"
        scope.environment = {"MARSH_HOME": str(scope.home)}
        scope.initialize_scope_cleanup()
        scope.scope_started = True
        status = {
            "schema": "marsh.status/v1",
            "scope_id": "owned-scope",
            "daemon_id": "owned-daemon",
            "endpoint_owner": {"uid": os.getuid(), "pid": 4242},
            "workers": [{"scope_id": "owned-scope", "vm_id": "owned-worker"}],
        }
        identity = (os.getuid(), "start=1;exe=/test/marshd")
        try:
            with mock.patch.object(
                RUN,
                "stable_process_identity",
                side_effect=[identity, identity, identity],
            ), mock.patch.object(
                RUN, "authenticate_daemon_control", return_value="test-token"
            ), mock.patch.object(
                RUN, "request_authenticated_daemon_shutdown"
            ), mock.patch.object(
                RUN.time, "monotonic", side_effect=[0.0, 0.0, 6.0]
            ), mock.patch.object(RUN.time, "sleep"), mock.patch.object(
                RUN.subprocess, "run"
            ) as run, mock.patch.object(RUN.os, "kill") as kill:
                scope.remember_owned_status(status)
                errors = scope.cleanup_isolated_scope()
            self.assertIn("did not exit after shutdown", errors[0])
            kill.assert_not_called()
            run.assert_not_called()
            self.assertTrue(scope.root.exists())
        finally:
            shutil.rmtree(scope.root, ignore_errors=True)

    def test_cleanup_authentication_mismatch_has_zero_side_effects(self) -> None:
        import os
        import shutil
        import tempfile
        from unittest import mock

        class Scope(RUN.IsolatedScopeCleanup):
            pass

        scope = Scope()
        scope.root = pathlib.Path(tempfile.mkdtemp())
        scope.home = scope.root / "home"
        scope.home.mkdir()
        scope.sbx = "/test/sbx"
        scope.environment = {"MARSH_HOME": str(scope.home)}
        scope.initialize_scope_cleanup()
        scope.scope_started = True
        status = {
            "schema": "marsh.status/v1",
            "scope_id": "owned-scope",
            "daemon_id": "owned-daemon",
            "endpoint_owner": {"uid": os.getuid(), "pid": 4242},
            "workers": [{"scope_id": "owned-scope", "vm_id": "owned-worker"}],
        }
        identity = (os.getuid(), "start=1;exe=/test/marshd")
        try:
            with mock.patch.object(
                RUN, "stable_process_identity", side_effect=[identity, identity]
            ), mock.patch.object(
                RUN, "authenticate_daemon_control", return_value="test-token"
            ), mock.patch.object(
                RUN,
                "request_authenticated_daemon_shutdown",
                side_effect=RuntimeError("daemon authentication failed"),
            ), mock.patch.object(RUN.subprocess, "run") as run, mock.patch.object(
                RUN.os, "kill"
            ) as kill:
                scope.remember_owned_status(status)
                errors = scope.cleanup_isolated_scope()
            self.assertIn("daemon authentication failed", errors[0])
            kill.assert_not_called()
            run.assert_not_called()
            self.assertTrue(scope.root.exists())
        finally:
            shutil.rmtree(scope.root, ignore_errors=True)

    def test_entry_points_finish_on_interrupt_and_failure(self) -> None:
        import types
        from unittest import mock

        class FakeHarness:
            def __init__(self, error: BaseException) -> None:
                self.error = error
                self.finished: list[object] = []

            def prepare(self) -> None:
                return None

            def run_all(self) -> None:
                raise self.error

            def finish(self, failure: object = None) -> int:
                self.finished.append(failure)
                return 7

        interrupted = FakeHarness(KeyboardInterrupt())
        with mock.patch.object(RUN, "parse_args", return_value=types.SimpleNamespace()), mock.patch.object(
            RUN, "Harness", return_value=interrupted
        ):
            self.assertEqual(RUN.main(), 7)
        self.assertEqual(interrupted.finished, ["acceptance interrupted"])

        failed = FakeHarness(RuntimeError("boom"))
        with mock.patch.object(RUN, "parse_args", return_value=types.SimpleNamespace()), mock.patch.object(
            RUN, "Harness", return_value=failed
        ):
            self.assertEqual(RUN.main(), 7)
        self.assertIn("boom", str(failed.finished[0]))

        smoke = FakeHarness(KeyboardInterrupt())
        with mock.patch.object(RUN.argparse.ArgumentParser, "parse_args", return_value=types.SimpleNamespace()), mock.patch.object(
            RUN, "Smoke", return_value=smoke
        ):
            self.assertEqual(RUN.smoke_main([]), 7)
        self.assertIsInstance(smoke.finished[0], InterruptedError)

    def test_sbx_executable_is_resolved_or_rejected(self) -> None:
        import os
        import stat
        import tempfile

        with tempfile.TemporaryDirectory() as directory:
            executable = pathlib.Path(directory) / "test-sbx"
            executable.write_text("#!/bin/sh\n", encoding="utf-8")
            executable.chmod(executable.stat().st_mode | stat.S_IXUSR)
            previous = os.environ.get("PATH")
            os.environ["PATH"] = directory
            try:
                self.assertEqual(RUN.resolve_executable("test-sbx"), str(executable.resolve()))
                with self.assertRaisesRegex(ValueError, "not found or is not executable"):
                    RUN.resolve_executable("missing-sbx")
            finally:
                if previous is None:
                    os.environ.pop("PATH", None)
                else:
                    os.environ["PATH"] = previous

    def test_harnesses_export_and_record_exact_sbx_path(self) -> None:
        import shutil
        import subprocess
        import sys
        import tempfile
        import types
        from unittest import mock

        repository = DIRECTORY.parents[1]
        revision = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=repository, check=True,
            text=True, stdout=subprocess.PIPE,
        ).stdout.strip()
        # A host-evidence fixture stays outside the worktree even when the
        # caller puts compiler caches/TMPDIR beneath target/.
        with tempfile.TemporaryDirectory(dir="/private/tmp" if sys.platform == "darwin" else "/tmp") as directory:
            guest = pathlib.Path(directory) / "guest"
            guest.mkdir()
            for name in ("marsh-linux-arm64", "marsh-worker-linux-arm64", "marsh-relay-linux-arm64"):
                (guest / name).write_bytes(name.encode())
            common = {
                "marsh": "/bin/sh",
                "sbx": "/bin/sh",
                "kit": str(DIRECTORY / "fixture"),
                "source_tree": str(repository),
                "source_revision": revision,
            }
            # This test checks constructor wiring only. Real CLI receipt/mismatch
            # behavior is exercised by the acceptance harness's real build callers.
            with mock.patch.object(RUN, "verify_candidate", return_value=(guest, {"wiring_fixture": True})), mock.patch.object(
                RUN, "candidate_environment", return_value={"MARSH_LOCAL_SHELL_AUTHORITY": "/private/wiring-authority.json"}
            ):
                full = RUN.Harness(
                    types.SimpleNamespace(**common, evidence=str(pathlib.Path(directory) / "full"))
                )
                smoke = RUN.Smoke(
                    types.SimpleNamespace(**common, evidence=str(pathlib.Path(directory) / "smoke"))
                )
            try:
                expected = str(pathlib.Path("/bin/sh").resolve())
                self.assertEqual(full.environment["MARSH_LOCAL_SHELL_AUTHORITY"], "/private/wiring-authority.json")
                self.assertEqual(smoke.environment["MARSH_LOCAL_SHELL_AUTHORITY"], "/private/wiring-authority.json")
                self.assertEqual(full.environment["MARSH_SBX"], expected)
                self.assertEqual(full.result["environment"]["sbx_binary"], expected)
                self.assertEqual(len(full.result["environment"]["guest_artifact_sha256"]), 3)
                self.assertEqual(smoke.environment["MARSH_SBX"], expected)
                self.assertEqual(smoke.source["sbx_binary"], expected)
                self.assertEqual(smoke.environment["MARSH_GUEST_ARTIFACTS"], str(guest))
                self.assertEqual(full.environment["MARSH_GUEST_ARTIFACTS"], str(guest))
                system_temp = pathlib.Path(
                    "/private/tmp" if sys.platform == "darwin" else tempfile.gettempdir()
                ).resolve()
                self.assertEqual(full.root.parent, system_temp)
                self.assertEqual(smoke.root.parent, system_temp)
                self.assertFalse(full.root.is_relative_to(full.evidence))
                self.assertFalse(smoke.root.is_relative_to(smoke.evidence))
                self.assertFalse(full.project.is_relative_to(pathlib.Path.home().resolve()))
                self.assertFalse(smoke.project.is_relative_to(pathlib.Path.home().resolve()))
            finally:
                shutil.rmtree(full.root)
                shutil.rmtree(smoke.root)

    def test_source_identity_marks_dirty_content_and_changes_with_bytes(self) -> None:
        import subprocess
        import tempfile

        with tempfile.TemporaryDirectory() as directory:
            tree = pathlib.Path(directory)
            # Reuse an existing revision; tests must not create commits or ask
            # for signing/authentication. Only this disposable clone is edited.
            subprocess.run(["git", "clone", "--quiet", "--no-hardlinks", str(DIRECTORY.parents[1]), str(tree)], check=True)
            source = tree / "source.txt"
            revision = subprocess.run(
                ["git", "rev-parse", "HEAD"], cwd=tree, check=True,
                text=True, stdout=subprocess.PIPE,
            ).stdout.strip()

            clean = RUN.source_identity(tree, revision)
            self.assertFalse(clean["source_dirty"])
            source.write_text("one\n", encoding="utf-8")
            dirty = RUN.source_identity(tree, revision)
            self.assertTrue(dirty["source_dirty"])
            self.assertNotEqual(clean["source_tree_sha256"], dirty["source_tree_sha256"])
            source.write_text("two\n", encoding="utf-8")
            self.assertNotEqual(dirty["source_tree_sha256"], RUN.source_identity(tree, revision)["source_tree_sha256"])

    def test_kit_reference_defaults_to_local_v3_and_accepts_immutable_oci(self) -> None:
        mapping, identity = RUN.resolve_kit_reference(str(DIRECTORY / "fixture"))
        self.assertEqual(mapping, str((DIRECTORY / "fixture").resolve()))
        self.assertEqual(identity, f"local-v3:{mapping}")

        oci = f"example/fixture@sha256:{'c' * 64}"
        self.assertEqual(RUN.resolve_kit_reference(oci), (oci, oci))

        with self.assertRaisesRegex(ValueError, "native v3 source directory"):
            RUN.resolve_kit_reference(str(DIRECTORY / "missing"))

    def test_local_kit_profile_accepts_only_its_resolved_generation(self) -> None:
        source = "local-v3:/tmp/fixture"
        self.assertTrue(RUN.kit_profile_matches(source, source))
        self.assertTrue(
            RUN.kit_profile_matches(f"{source}@sha256:{'a' * 64}", source)
        )
        self.assertFalse(
            RUN.kit_profile_matches(
                f"local-v3:/tmp/other@sha256:{'a' * 64}", source
            )
        )
        self.assertFalse(RUN.kit_profile_matches(f"{source}@sha256:short", source))

    def test_harness_uses_public_newest_first_receipt_order(self) -> None:
        harness = RUN.Harness.__new__(RUN.Harness)
        state = {
            "runs": [
                {"command": "fixture", "job_id": "newest"},
                {"command": "fixture", "job_id": "older"},
            ]
        }
        self.assertEqual(harness.newest_run(state)["job_id"], "newest")

    def test_harness_receipts_are_newest_first_and_cache_terminal_jobs(self) -> None:
        import subprocess
        import types

        harness = RUN.Harness.__new__(RUN.Harness)
        harness.marsh = "/tmp/marsh"
        harness.receipt_cache = {}
        calls: list[list[str]] = []
        documents = {
            "newest": {
                "schema": "marsh.job/v1",
                "job_id": "newest",
                "command": "fixture",
                "state": "finished",
            },
            "older": {
                "schema": "marsh.job/v1",
                "job_id": "older",
                "command": "fixture",
                "state": "finished",
            },
        }

        def fake_run(_self: object, argv: list[str], **_kwargs: object) -> object:
            calls.append(argv)
            if argv[1:] == ["jobs", "--json"]:
                value = {
                    "schema": "marsh.jobs/v1",
                    "jobs": [
                        {"job_id": "newest", "command": "fixture"},
                        {"job_id": "older", "command": "fixture"},
                    ],
                }
            else:
                value = documents[argv[3]]
            return subprocess.CompletedProcess(argv, 0, json.dumps(value).encode(), b"")

        harness.run = types.MethodType(fake_run, harness)
        self.assertEqual(
            [receipt["job_id"] for receipt in harness.receipts()],
            ["newest", "older"],
        )
        harness.receipts()
        self.assertEqual(
            sum(1 for argv in calls if argv[1:3] == ["jobs", "show"]),
            2,
        )

    def test_quarantined_worker_blocks_only_dependent_followup_checks(self) -> None:
        import subprocess
        import types

        harness = RUN.Harness.__new__(RUN.Harness)
        harness.marsh = "/tmp/marsh"
        harness.fixture_ref = "local-v3:/tmp/fixture"
        harness.worker_blocked = None
        harness.result = {
            "checks": [
                {"name": "first", "outcome": "not-run"},
                {"name": "dependent", "outcome": "not-run"},
                {"name": "independent", "outcome": "not-run"},
            ]
        }
        status = {
            "workers": [
                {
                    "worker_id": "worker-1",
                    "kit_profile": f"{harness.fixture_ref}@sha256:{'a' * 64}",
                    "health": "quarantined",
                }
            ]
        }

        def fake_run(_self: object, _argv: list[str], **_kwargs: object) -> object:
            return subprocess.CompletedProcess(
                [], 0, json.dumps(status).encode(), b""
            )

        harness.run = types.MethodType(fake_run, harness)
        harness.mark("first", lambda: (_ for _ in ()).throw(AssertionError("boom")))
        called = False

        def dependent() -> None:
            nonlocal called
            called = True

        harness.mark("dependent", dependent)
        self.assertFalse(called)
        self.assertIn("quarantined", harness.result["checks"][1]["detail"])
        harness.mark("independent", lambda: None, requires_worker=False)
        self.assertEqual(harness.result["checks"][2]["outcome"], "passed")

    def test_current_evidence_shape_validates(self) -> None:
        RUN.validate_schema(valid_result(), SCHEMA)

    def test_missing_limit_and_extra_field_fail_closed(self) -> None:
        missing = valid_result()
        del missing["environment"]["job_limits"]["pids"]  # type: ignore[index]
        with self.assertRaisesRegex(ValueError, "pids"):
            RUN.validate_schema(missing, SCHEMA)

        extra = valid_result()
        extra["unexpected"] = True
        with self.assertRaisesRegex(ValueError, "unexpected"):
            RUN.validate_schema(extra, SCHEMA)

    def test_timing_definition_accepts_only_truthful_phase_names(self) -> None:
        timing = {
            "durations_ms": {name: 1 for name in RUN.PHASES},
            "milestones_unix_ms": {
                "request_received": 1000,
                "worker_progress": 1004,
                "first_output": 1005,
                "process_exit": 1007,
                "output_drained": 1008,
                "completed": 1010,
            },
            "wall_ms": 8,
            "orchestration_ms": 7,
        }
        definition = SCHEMA["$defs"]["timing_report"]
        RUN.validate_schema(timing, definition)

        timing["durations_ms"]["queue"] = 0
        with self.assertRaisesRegex(ValueError, "queue"):
            RUN.validate_schema(timing, definition)

    def test_fanout_timing_requires_real_overlap(self) -> None:
        RUN.validate_concurrent_fanout_timing(
            {
                "branches": [
                    {"label": "first", "duration_ms": 1000},
                    {"label": "second", "duration_ms": 1010},
                ],
                "total_ms": 1050,
            }
        )
        with self.assertRaisesRegex(AssertionError, "concurrent elapsed work"):
            RUN.validate_concurrent_fanout_timing(
                {
                    "branches": [
                        {"label": "first", "duration_ms": 1000},
                        {"label": "second", "duration_ms": 1000},
                    ],
                    "total_ms": 2000,
                }
            )

    def test_results_require_newest_first_structural_fields(self) -> None:
        summaries = []
        for cursor in (2, 1):
            summaries.append(
                {
                    "cursor": cursor,
                    "job_id": f"job-{cursor}",
                    "command": "fixture",
                    "placement": "local",
                    "cleanup": "verified",
                    "parent": None,
                    "state": "finished",
                    "exit_code": 0,
                    "wall_ms": 10,
                }
            )
        receipt = {name: None for name in RUN.RESULT_RECEIPT_FIELDS}
        receipt.update({"schema": "marsh.job/v1", "cursor": 2, "job_id": "job-2", "placement": "local"})
        RUN.validate_structural_results(
            {"schema": "marsh.jobs/v1", "jobs": summaries}, receipt
        )

        summaries.reverse()
        with self.assertRaisesRegex(AssertionError, "newest-first"):
            RUN.validate_structural_results(
                {"schema": "marsh.jobs/v1", "jobs": summaries}, receipt
            )

        summaries.reverse()
        receipt["stdout"] = "forbidden"
        with self.assertRaisesRegex(AssertionError, "non-structural"):
            RUN.validate_structural_results(
                {"schema": "marsh.jobs/v1", "jobs": summaries}, receipt
            )


if __name__ == "__main__":
    unittest.main()
