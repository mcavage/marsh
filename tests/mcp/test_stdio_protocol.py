"""Black-box tests for the public marsh-mcp stdio protocol.

These tests deliberately do not import the Rust crate or inspect daemon state.
The fake ``marsh`` executable is the only product dependency, which keeps the
protocol and host-scope checks runnable without SBX, Docker, credentials, or a
live daemon.
"""

from __future__ import annotations

import base64
import hashlib
import json
import os
from pathlib import Path
import queue
import shutil
import signal
import stat
import subprocess
import tempfile
import threading
import time
import unittest


ROOT = Path(__file__).resolve().parents[2]
from binary_preflight import required_binary

MCP_BINARY = required_binary("MARSH_MCP_BIN")
PROTOCOL_VERSION = "2025-06-18"
TOOLS = {
    "doctor",
    "status",
    "results_list",
    "result_get",
    "prewarm",
    "workers_reset",
    "shell_run",
    "qualify",
    "operation_get",
    "operation_output",
    "operation_cancel",
    "scope_start",
    "scope_run",
    "scope_status",
    "scope_reset",
    "scope_stop",
    "scope_list",
    "scope_remove",
    "scope_results_list",
    "scope_result_get",
    "scope_prewarm",
    "scope_workers_reset",
}
RELAY_ENV = (
    "MARSH_DAEMON_SOCKET",
    "MARSH_DAEMON_TOKEN",
    "MARSH_SESSION_ID",
    "MARSH_HOME_BACKING",
    "MARSH_RELAY_TOKEN",
)


FAKE_MARSH = r'''#!/bin/sh
# This executable is a deterministic stand-in for the public marsh binary.
# It logs only safe invocation metadata so the test can verify the host scope.
{
  printf 'argv1=%s\n' "${1-}"
  printf 'argv2=%s\n' "${2-}"
  printf 'pwd=%s\n' "$PWD"
  printf 'home=%s\n' "${MARSH_HOME-}"
  for name in MARSH_DAEMON_SOCKET MARSH_DAEMON_TOKEN MARSH_SESSION_ID MARSH_HOME_BACKING MARSH_RELAY_TOKEN; do
    eval "value=\${$name-}"
    if [ -n "$value" ]; then printf 'relay_present=%s\n' "$name"; fi
  done
} >> "$TMPDIR/marsh-mcp-fake.log"

case "${1-}" in
  status)
    printf '%s\n' '{"schema":"marsh.status/v1","daemon":"fake","scope":"test"}'
    ;;
  results)
    if [ -f "$MARSH_HOME/.fail-results-before-daemon" ]; then
      printf 'injected receipt read failure before daemon start\n' >&2
      exit 23
    fi
    : > "$MARSH_HOME/.fake-daemon-ready"
    if [ "${2-}" = "show" ]; then
      printf '%s\n' '{"schema":"marsh.result/v1","selector":"'"${3-}"'"}'
    else
      printf '%s\n' '{"schema":"marsh.results/v1","entries":[]}'
    fi
    ;;
  --load)
    if [ -f "$MARSH_HOME/.slow-prewarm" ]; then sleep 1; fi
    printf 'prewarm %s\n' "${2-}"
    ;;
  workers)
    printf 'reset %s\n' "${3-}"
    ;;
  -c)
    case "${2-}" in
      true)
        if [ -f "$TMPDIR/fail-next-start" ]; then
          rm -f "$TMPDIR/fail-next-start"
          printf 'injected project-shell boot failure\n' >&2
          exit 17
        fi
        : > "$MARSH_HOME/.fake-daemon-ready"
        ;;
      sleep-marker)
        sleep 30
        ;;
      write-home-sentinel)
        printf 'preserved\n' > "$MARSH_HOME/durable-result-sentinel"
        ;;
      read-home-sentinel)
        cat "$MARSH_HOME/durable-result-sentinel"
        ;;
      max-output)
        /usr/bin/head -c 262144 /dev/zero | /usr/bin/tr '\000' x
        /usr/bin/head -c 262144 /dev/zero | /usr/bin/tr '\000' y >&2
        ;;
      *)
        printf 'stdout:probe\n'
        printf 'stderr:probe\n' >&2
        ;;
    esac
    ;;
  *)
    if [ "${1-}" = reset ]; then
      if [ ! -f "$MARSH_HOME/.fake-daemon-ready" ]; then
        printf 'injected absent daemon endpoint\n' >&2
        exit 18
      fi
      rm -f "$MARSH_HOME/.fake-daemon-ready"
    fi
    if [ "${1-}" = stop ]; then
      if [ -f "$MARSH_HOME/.fail-stop" ]; then
        printf 'injected daemon shutdown failure\n' >&2
        exit 19
      fi
      rm -f "$MARSH_HOME/.fake-daemon-ready"
    fi
    printf '%s\n' '{"schema":"marsh.fake/v1"}'
    ;;
esac
'''


class JsonRpcPeer:
    """Small real stdio client; responses may arrive out of order."""

    def __init__(self, command: list[str], env: dict[str, str]):
        self.process = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            cwd=ROOT,
            bufsize=0,
        )
        assert self.process.stdin is not None
        assert self.process.stdout is not None
        assert self.process.stderr is not None
        self._stdin = self.process.stdin
        self._lines: queue.Queue[object] = queue.Queue()
        self._pending: dict[object, dict] = {}
        self._next_id = 1
        self._lock = threading.Lock()
        self._reader = threading.Thread(target=self._read_stdout, daemon=True)
        self._reader.start()
        self._stderr_reader = threading.Thread(
            target=self._read_stderr, args=(self.process.stderr,), daemon=True
        )
        self._stderr_reader.start()

    def _read_stdout(self) -> None:
        assert self.process.stdout is not None
        for line in iter(self.process.stdout.readline, b""):
            try:
                value = json.loads(line)
            except json.JSONDecodeError as error:
                self._lines.put(AssertionError(f"non-JSON stdout from MCP server: {line!r}: {error}"))
                continue
            self._lines.put(value)
        self._lines.put(EOFError("MCP server closed stdout"))

    def _read_stderr(self, stream) -> None:
        self.stderr = stream.read().decode("utf-8", "replace")

    def notify(self, method: str, params: dict | None = None) -> None:
        message = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            message["params"] = params
        self._write(message)

    def request(self, method: str, params: dict | None = None, timeout: float = 15.0) -> dict:
        with self._lock:
            request_id = self._next_id
            self._next_id += 1
        message = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            message["params"] = params
        self._write(message)
        if request_id in self._pending:
            return self._pending.pop(request_id)
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                self.fail(f"timed out waiting for JSON-RPC response {request_id}")
            try:
                item = self._lines.get(timeout=remaining)
            except queue.Empty:
                self.fail(f"timed out waiting for JSON-RPC response {request_id}")
            if isinstance(item, BaseException):
                self.fail(str(item))
            if item.get("id") == request_id:
                return item
            if "id" in item:
                self._pending[item["id"]] = item

    def _write(self, message: dict) -> None:
        self._stdin.write((json.dumps(message, separators=(",", ":")) + "\n").encode())
        self._stdin.flush()

    def initialize(self) -> dict:
        response = self.request(
            "initialize",
            {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "marsh-mcp-black-box", "version": "1"},
            },
        )
        self.notify("notifications/initialized", {})
        return response

    def fail(self, message: str) -> None:
        self.close()
        raise AssertionError(message)

    def close(self) -> None:
        if self.process.poll() is None:
            try:
                self._stdin.close()
            except OSError:
                pass
            try:
                self.process.terminate()
                self.process.wait(timeout=2)
            except (subprocess.TimeoutExpired, OSError):
                self.process.kill()
                self.process.wait(timeout=2)
        try:
            self._stdin.close()
        except (OSError, ValueError):
            pass
        for stream in (self.process.stdout, self.process.stderr):
            if stream is not None:
                stream.close()


class HostMcpBlackBoxTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory(prefix="marsh-mcp-test-")
        # Canonical root: marsh admits only symlink-free project paths.
        root = Path(os.path.realpath(self.tempdir.name))
        self.workspace = root / "workspace"
        self.workspace.mkdir()
        self.home = root / "home"
        self.fake = root / "marsh"
        self.fake.write_text(FAKE_MARSH, encoding="utf-8")
        self.fake.chmod(0o700)
        self.fake_daemon = root / "marshd"
        self.fake_daemon.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        self.fake_daemon.chmod(0o700)
        self.fake_sbx = root / "sbx"
        self.fake_sbx.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        self.fake_sbx.chmod(0o700)
        self.log = root / "marsh-mcp-fake.log"
        environment = os.environ.copy()
        # TMPDIR is intentionally in the adapter's small inherited allowlist;
        # arbitrary marker variables must not cross the host/job boundary.
        environment["TMPDIR"] = str(root)
        host_home = root / "host-home"
        host_home.mkdir()
        environment["HOME"] = str(host_home)
        environment["MARSH_HOME"] = str(root / "wrong-inherited-home")
        for name in RELAY_ENV:
            environment[name] = f"sentinel-{name.lower()}"
        self.environment = environment
        self.peer = self.start_peer(explicit_home=True)
        initialize = self.peer.initialize()
        self.assertIn("result", initialize)
        self.assertEqual(initialize["result"]["protocolVersion"], PROTOCOL_VERSION)

    def tearDown(self) -> None:
        self.peer.close()
        self.tempdir.cleanup()

    def start_peer(self, explicit_home: bool) -> JsonRpcPeer:
        args = [
            str(MCP_BINARY),
            "serve",
            "--workspace",
            str(self.workspace),
            "--marsh",
            str(self.fake),
            "--sbx",
            str(self.fake_sbx),
        ]
        if explicit_home:
            args.extend(("--home", str(self.home)))
        else:
            args.extend(("--scope-root", str(self.managed_root())))
        return JsonRpcPeer(args, self.environment)

    def broker_args(self, mode: str) -> list[str]:
        return [
            str(MCP_BINARY),
            mode,
            "--workspace",
            str(self.workspace),
            "--scope-root",
            str(Path(self.tempdir.name) / "broker-root"),
            "--marsh",
            str(self.fake),
            "--sbx",
            str(self.fake_sbx),
        ]

    def broker_state_dir(self) -> Path:
        scope_root = Path(self.tempdir.name) / "broker-root"
        key = hashlib.sha256(os.fsencode(scope_root)).hexdigest()[:16]
        return Path("/tmp") / f"marsh-mcp-{os.geteuid()}-{key}"

    def call(self, tool: str, arguments: dict | None = None) -> dict:
        response = self.peer.request(
            "tools/call", {"name": tool, "arguments": arguments or {}}
        )
        self.assertIn("result", response, response)
        return response["result"]

    def structured(self, result: dict) -> dict:
        self.assertIn("structuredContent", result, result)
        return result["structuredContent"]

    def wait_for_operation(self, operation_id: str, timeout: float = 4.0) -> dict:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            result = self.structured(
                self.call("operation_get", {"operation_id": operation_id})
            )
            state = result["data"]["state"]
            if state in {"succeeded", "failed", "cancellation_uncertain"}:
                return result["data"]
            time.sleep(0.02)
        self.fail(f"operation {operation_id} did not become terminal")

    def log_text(self) -> str:
        return self.log.read_text(encoding="utf-8") if self.log.exists() else ""

    def use_managed_scopes(self) -> None:
        """Restart the peer in normal managed-scope mode (no legacy --home)."""
        self.peer.close()
        self.peer = self.start_peer(explicit_home=False)
        initialize = self.peer.initialize()
        self.assertEqual(initialize["result"]["protocolVersion"], PROTOCOL_VERSION)

    def data_root(self) -> Path:
        import sys
        if sys.platform == "darwin":
            return Path(self.environment["HOME"]) / "Library" / "Application Support" / "marsh-mcp-scopes"
        return Path(self.environment["HOME"]) / ".local" / "share" / "marsh-mcp-scopes"

    def managed_root(self) -> Path:
        scope_hash = hashlib.sha256(os.fsencode(self.workspace.resolve())).hexdigest()
        return (self.data_root() / scope_hash).resolve()

    def start_scope(self) -> dict:
        started = self.structured(self.call("scope_start"))["data"]
        terminal = self.wait_for_operation(started["operation_id"])
        self.assertEqual(terminal["state"], "succeeded")
        self.assertEqual(terminal["scope_id"], started["scope_id"])
        return started

    def test_initialize_tools_list_and_doctor_are_scoped(self) -> None:
        listed = self.peer.request("tools/list", {})["result"]["tools"]
        self.assertEqual({tool["name"] for tool in listed}, TOOLS)
        by_name = {tool["name"]: tool for tool in listed}
        for name in (
            "doctor",
            "status",
            "results_list",
            "scope_start",
            "scope_list",
            "scope_results_list",
            "scope_result_get",
            "scope_prewarm",
            "scope_workers_reset",
        ):
            self.assertIs(
                by_name[name]["inputSchema"].get("additionalProperties"),
                False,
                (name, by_name[name]["inputSchema"]),
            )
        scope_start_description = by_name["scope_start"]["description"].lower()
        self.assertIn("shared", scope_start_description)
        self.assertIn("workspace", scope_start_description)
        self.assertIn("race", scope_start_description)
        doctor = self.structured(self.call("doctor"))
        data = doctor["data"]
        self.assertTrue(doctor["ok"])
        self.assertEqual(Path(data["workspace"]), self.workspace.resolve())
        self.assertEqual(Path(data["home"]), self.home.resolve())
        self.assertEqual(Path(data["marsh"]), self.fake.resolve())
        self.assertEqual(Path(data["sbx"]), self.fake_sbx.resolve())
        self.assertFalse(data["relay_environment_forwarded"])
        self.assertFalse(data["managed_scope_persistence"])
        self.assertEqual(data["workspace_write_isolation"], "shared")
        self.assertEqual(stat.S_IMODE(self.home.stat().st_mode), 0o700)

    def test_resident_broker_shares_scopes_and_replaces_only_when_idle(self) -> None:
        # The broker endpoint lives under the fixed /tmp root, not tempdir.
        self.addCleanup(shutil.rmtree, self.broker_state_dir(), ignore_errors=True)
        started = subprocess.run(
            self.broker_args("broker-start"),
            env=self.environment,
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(started.returncode, 0, started.stderr)
        first_pid = int((self.broker_state_dir() / "broker.pid").read_text())
        first = JsonRpcPeer(self.broker_args("connect"), self.environment)
        second = JsonRpcPeer(self.broker_args("connect"), self.environment)
        try:
            first.initialize()
            second.initialize()
            listed = first.request("tools/list", {})["result"]["tools"]
            self.assertEqual({tool["name"] for tool in listed}, TOOLS)

            created = first.request(
                "tools/call", {"name": "scope_start", "arguments": {}}
            )["result"]["structuredContent"]["data"]
            hidden = second.request(
                "tools/call",
                {
                    "name": "operation_get",
                    "arguments": {"operation_id": created["operation_id"]},
                },
            )["result"]["structuredContent"]
            self.assertFalse(hidden["ok"])
            self.assertIn("this MCP session", hidden["error"])

            deadline = time.monotonic() + 5
            while True:
                owned = first.request(
                    "tools/call",
                    {
                        "name": "operation_get",
                        "arguments": {"operation_id": created["operation_id"]},
                    },
                )["result"]["structuredContent"]
                if owned["data"]["state"] in {
                    "succeeded",
                    "failed",
                    "cancellation_uncertain",
                }:
                    break
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.02)
            scopes = second.request(
                "tools/call", {"name": "scope_list", "arguments": {}}
            )["result"]["structuredContent"]
            self.assertTrue(scopes["ok"])
            self.assertIn(
                created["scope_id"],
                {scope["scope_id"] for scope in scopes["data"]["scopes"]},
            )

            busy = subprocess.run(
                self.broker_args("broker-stop"),
                env=self.environment,
                cwd=ROOT,
                capture_output=True,
                text=True,
                timeout=10,
            )
            self.assertNotEqual(busy.returncode, 0)
            self.assertIn("attached sessions", busy.stderr)

            mismatched = subprocess.run(
                self.broker_args("broker-start") + ["--allow-full-sbx-control"],
                env=self.environment,
                cwd=ROOT,
                capture_output=True,
                text=True,
                timeout=15,
            )
            self.assertNotEqual(mismatched.returncode, 0)
            self.assertIn("attached sessions", mismatched.stderr)
            first.close()
            status = second.request(
                "tools/call", {"name": "status", "arguments": {}}
            )["result"]["structuredContent"]
            self.assertTrue(status["ok"])
        finally:
            first.close()
            second.close()

        # Changing a pinned executable changes the desired identity. An idle
        # broker is replaced automatically; the changed configuration above
        # could not replace a busy broker.
        with self.fake_sbx.open("a", encoding="utf-8") as stream:
            stream.write("# replacement identity\n")
        replaced = subprocess.run(
            self.broker_args("broker-start"),
            env=self.environment,
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(replaced.returncode, 0, replaced.stderr)
        second_pid = int((self.broker_state_dir() / "broker.pid").read_text())
        self.assertNotEqual(first_pid, second_pid)

        lost = JsonRpcPeer(self.broker_args("connect"), self.environment)
        lost.initialize()
        try:
            os.kill(second_pid, signal.SIGKILL)
            deadline = time.monotonic() + 3
            while True:
                try:
                    os.kill(second_pid, 0)
                except ProcessLookupError:
                    break
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.02)
            with self.assertRaises((AssertionError, BrokenPipeError)):
                lost.request("tools/list", {}, timeout=5)
        finally:
            lost.close()
        restarted = subprocess.run(
            self.broker_args("broker-start"),
            env=self.environment,
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(restarted.returncode, 0, restarted.stderr)
        fresh = JsonRpcPeer(self.broker_args("connect"), self.environment)
        try:
            fresh.initialize()
            self.assertEqual(
                {tool["name"] for tool in fresh.request("tools/list", {})["result"]["tools"]},
                TOOLS,
            )
        finally:
            fresh.close()
        stopped = subprocess.run(
            self.broker_args("broker-stop"),
            env=self.environment,
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=15,
        )
        self.assertEqual(stopped.returncode, 0, stopped.stderr)
        self.assertFalse((self.broker_state_dir() / "broker.sock").exists())

    def test_status_results_and_result_get_use_versioned_json(self) -> None:
        status = self.structured(self.call("status"))
        self.assertTrue(status["ok"])
        self.assertEqual(status["data"]["schema"], "marsh.status/v1")
        results = self.structured(self.call("results_list"))
        self.assertTrue(results["ok"])
        self.assertEqual(results["data"]["schema"], "marsh.results/v1")
        result = self.structured(self.call("result_get", {"selector": "519"}))
        self.assertTrue(result["ok"])
        self.assertEqual(result["data"]["schema"], "marsh.result/v1")
        self.assertIn("argv1=status", self.log_text())
        self.assertIn("argv1=results", self.log_text())
        self.assertNotIn("relay_present=", self.log_text())

    def test_generated_scope_has_results_prewarm_and_worker_reset_parity(self) -> None:
        self.use_managed_scopes()
        started = self.start_scope()
        scope_id = started["scope_id"]
        scope_home = self.managed_root() / scope_id

        listed = self.structured(
            self.call("scope_results_list", {"scope_id": scope_id})
        )
        self.assertEqual(listed["data"]["schema"], "marsh.results/v1")
        shown = self.structured(
            self.call(
                "scope_result_get", {"scope_id": scope_id, "selector": "519"}
            )
        )
        self.assertEqual(shown["data"]["schema"], "marsh.result/v1")

        (scope_home / ".slow-prewarm").touch()
        prewarm = self.structured(
            self.call(
                "scope_prewarm", {"scope_id": scope_id, "selection": "all"}
            )
        )["data"]
        started_at = time.monotonic()
        while "argv1=--load" not in self.log_text():
            self.assertLess(time.monotonic() - started_at, 1.0)
            time.sleep(0.01)
        # Read-only inspection is admitted while the same scope is prewarming.
        during = self.structured(
            self.call("scope_results_list", {"scope_id": scope_id})
        )
        self.assertTrue(during["ok"])
        self.assertLess(time.monotonic() - started_at, 0.8)
        self.assertEqual(
            self.wait_for_operation(prewarm["operation_id"], timeout=3)["state"],
            "succeeded",
        )
        (scope_home / ".slow-prewarm").unlink()

        reset = self.structured(
            self.call(
                "scope_workers_reset", {"scope_id": scope_id, "selection": "all"}
            )
        )["data"]
        self.assertEqual(
            self.wait_for_operation(reset["operation_id"])["state"], "succeeded"
        )

        stopped = self.structured(self.call("scope_stop", {"scope_id": scope_id}))[
            "data"
        ]
        self.assertEqual(
            self.wait_for_operation(stopped["operation_id"], timeout=3)["state"],
            "succeeded",
        )
        receipts = self.structured(
            self.call("scope_results_list", {"scope_id": scope_id})
        )
        self.assertTrue(receipts["ok"])
        self.assertFalse(
            (scope_home / ".fake-daemon-ready").exists(),
            "Stopped receipt inspection left its daemon live",
        )
        state = self.structured(self.call("scope_status", {"scope_id": scope_id}))
        self.assertEqual(state["data"]["state"], "stopped")
        removal = self.structured(self.call("scope_remove", {"scope_id": scope_id}))[
            "data"
        ]
        self.assertEqual(
            self.wait_for_operation(removal["operation_id"])["state"], "succeeded"
        )
        self.assertFalse(scope_home.exists())

    def test_stopped_result_read_fails_closed_when_daemon_shutdown_is_unproven(self) -> None:
        self.use_managed_scopes()
        started = self.start_scope()
        scope_id = started["scope_id"]
        scope_home = self.managed_root() / scope_id
        stopped = self.structured(self.call("scope_stop", {"scope_id": scope_id}))[
            "data"
        ]
        self.assertEqual(
            self.wait_for_operation(stopped["operation_id"])["state"], "succeeded"
        )
        (scope_home / ".fail-stop").touch()

        receipts = self.structured(
            self.call("scope_results_list", {"scope_id": scope_id})
        )
        self.assertFalse(receipts["ok"])
        self.assertIn("scope marked failed", receipts["error"])
        status = self.structured(self.call("scope_status", {"scope_id": scope_id}))
        self.assertEqual(status["data"]["state"], "failed")
        self.assertEqual(status["data"]["runtime_state"], "unknown")
        self.assertIn("daemon shutdown", status["data"]["diagnostic"])
        removal = self.structured(self.call("scope_remove", {"scope_id": scope_id}))
        self.assertFalse(removal["ok"])
        self.assertTrue(scope_home.exists())

    def test_stopped_result_read_failure_preserves_proven_absent_scope(self) -> None:
        self.use_managed_scopes()
        started = self.start_scope()
        scope_id = started["scope_id"]
        scope_home = self.managed_root() / scope_id
        stopped = self.structured(self.call("scope_stop", {"scope_id": scope_id}))[
            "data"
        ]
        self.assertEqual(
            self.wait_for_operation(stopped["operation_id"])["state"], "succeeded"
        )
        (scope_home / ".fail-results-before-daemon").touch()

        receipts = self.structured(
            self.call("scope_results_list", {"scope_id": scope_id})
        )
        self.assertFalse(receipts["ok"])
        self.assertIn("receipt read failure", receipts["error"])
        self.assertFalse((scope_home / ".fake-daemon-ready").exists())
        status = self.structured(self.call("scope_status", {"scope_id": scope_id}))
        self.assertEqual(status["data"]["state"], "stopped")
        self.assertEqual(status["data"]["runtime_state"], "absent")

    def test_shell_run_lifecycle_output_cwd_home_and_relay_scrubbing(self) -> None:
        queued = self.structured(self.call("shell_run", {"command": "probe"}))
        operation_id = queued["data"]["operation_id"]
        self.assertEqual(queued["data"]["state"], "queued")
        terminal = self.wait_for_operation(operation_id)
        self.assertEqual(terminal["state"], "succeeded")
        self.assertEqual(terminal["kind"], "shell_run")
        self.assertEqual(terminal["scope_id"], "default")
        output = self.structured(
            self.call("operation_output", {"operation_id": operation_id})
        )["data"]
        self.assertEqual(
            base64.b64decode(output["stdout"]["base64"]), b"stdout:probe\n"
        )
        self.assertEqual(
            base64.b64decode(output["stderr"]["base64"]), b"stderr:probe\n"
        )
        log = self.log_text()
        self.assertIn("argv1=-c", log)
        self.assertIn(f"pwd={self.workspace.resolve()}", log)
        self.assertIn(f"home={self.home.resolve()}", log)
        self.assertNotIn("relay_present=", log)
        self.assertNotIn("wrong-inherited-home", log)

    def test_timeout_and_cancellation_are_explicit_and_recoverable(self) -> None:
        timed_out = self.structured(
            self.call("shell_run", {"command": "sleep-marker", "timeout_ms": 30})
        )["data"]["operation_id"]
        terminal = self.wait_for_operation(timed_out)
        self.assertEqual(terminal["state"], "failed")
        timed_output = self.structured(
            self.call("operation_output", {"operation_id": timed_out})
        )["data"]
        self.assertIn(
            "exceeded",
            base64.b64decode(timed_output["stderr"]["base64"]).decode(),
        )

        cancelled = self.structured(
            self.call("shell_run", {"command": "sleep-marker", "timeout_ms": 900000})
        )["data"]["operation_id"]
        cancellation = self.structured(
            self.call("operation_cancel", {"operation_id": cancelled})
        )
        self.assertTrue(cancellation["ok"])
        terminal = self.wait_for_operation(cancelled)
        self.assertEqual(terminal["state"], "cancellation_uncertain")

        follow_up = self.structured(self.call("shell_run", {"command": "probe"}))
        self.assertEqual(self.wait_for_operation(follow_up["data"]["operation_id"])["state"], "succeeded")

    def test_maximum_captured_output_fits_bounded_stdio_frame(self) -> None:
        queued = self.structured(
            self.call("shell_run", {"command": "max-output", "timeout_ms": 5000})
        )
        operation_id = queued["data"]["operation_id"]
        self.assertEqual(self.wait_for_operation(operation_id)["state"], "succeeded")
        output = self.structured(
            self.call("operation_output", {"operation_id": operation_id})
        )["data"]
        self.assertEqual(output["stdout"]["byte_length"], 262144)
        self.assertEqual(output["stderr"]["byte_length"], 262144)
        self.assertEqual(len(base64.b64decode(output["stdout"]["base64"])), 262144)
        self.assertEqual(len(base64.b64decode(output["stderr"]["base64"])), 262144)

    def test_bad_arguments_fail_without_host_execution(self) -> None:
        unknown_field = self.call(
            "shell_run", {"command": "probe", "unexpected": "host command"}
        )
        self.assertTrue(unknown_field["isError"])
        wrong_type = self.call("shell_run", {"command": "probe", "timeout_ms": "30"})
        self.assertTrue(wrong_type["isError"])
        missing_command = self.call("shell_run", {})
        self.assertTrue(missing_command["isError"])
        short_timeout = self.structured(
            self.call("shell_run", {"command": "probe", "timeout_ms": 9})
        )
        self.assertFalse(short_timeout["ok"])
        invalid_selection = self.structured(
            self.call("prewarm", {"selection": "../escape"})
        )
        self.assertFalse(invalid_selection["ok"])
        invalid_selector = self.structured(
            self.call("result_get", {"selector": "../../secret"})
        )
        self.assertFalse(invalid_selector["ok"])
        unknown_operation = self.structured(
            self.call("operation_get", {"operation_id": "op-not-owned"})
        )
        self.assertFalse(unknown_operation["ok"])
        unknown_tool = self.peer.request(
            "tools/call", {"name": "host_exec", "arguments": {}}
        )
        self.assertEqual(unknown_tool["error"]["code"], -32602)
        self.assertEqual(self.log_text(), "")

    def test_default_serve_uses_the_cli_selected_home(self) -> None:
        """`marsh-mcp serve` as documented (no --home/--scope-root) drives the
        same selected home as the CLI (MARSH_HOME, else ~/.marsh) and writes
        nothing under the protected host product root."""
        self.peer.close()
        for marsh_home in (Path(self.environment["MARSH_HOME"]), None):
            environment = dict(self.environment)
            if marsh_home is None:
                environment.pop("MARSH_HOME")
                expected = Path(environment["HOME"]) / ".marsh"
            else:
                expected = marsh_home
            command = [
                str(MCP_BINARY), "serve", "--workspace", str(self.workspace),
                "--marsh", str(self.fake), "--sbx", str(self.fake_sbx),
            ]
            peer = JsonRpcPeer(command, environment)
            try:
                initialize = peer.initialize()
                self.assertEqual(initialize["result"]["protocolVersion"], PROTOCOL_VERSION)
                result = peer.request(
                    "tools/call", {"name": "doctor", "arguments": {}}
                )["result"]["structuredContent"]
            finally:
                peer.close()
            self.assertEqual(Path(result["data"]["home"]), expected.resolve())
            self.assertFalse(result["data"]["managed_scope_persistence"])
            self.assertEqual(stat.S_IMODE(expected.stat().st_mode), 0o700)
        product = Path(self.environment["HOME"]) / "Library" / "Application Support" / "marsh"
        self.assertFalse(product.exists(), "default serve wrote protected product state")

    def test_second_mcp_for_same_managed_root_fails_without_harming_owner(self) -> None:
        self.use_managed_scopes()
        command = [
            str(MCP_BINARY),
            "serve",
            "--workspace",
            str(self.workspace),
            "--scope-root",
            str(self.managed_root()),
            "--marsh",
            str(self.fake),
            "--sbx",
            str(self.fake_sbx),
        ]
        started_at = time.monotonic()
        completed = subprocess.run(
            command,
            cwd=ROOT,
            env=self.environment,
            input="",
            text=True,
            capture_output=True,
            timeout=3,
            check=False,
        )
        self.assertLess(time.monotonic() - started_at, 2.5)
        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(completed.stdout, "")
        self.assertIn("already controlled", completed.stderr.lower())
        doctor = self.structured(self.call("doctor"))
        self.assertTrue(doctor["ok"])
        probe = self.structured(self.call("shell_run", {"command": "probe"}))["data"]
        self.assertEqual(
            self.wait_for_operation(probe["operation_id"])["state"], "succeeded"
        )

    def test_dynamic_scopes_have_isolated_managed_homes_and_exact_routing(self) -> None:
        """Two agent scopes never share their daemon/VM pool or selected home."""
        self.use_managed_scopes()
        managed_root = self.managed_root()

        starts = [self.structured(self.call("scope_start")) for _ in range(2)]
        first, second = (started["data"] for started in starts)
        self.assertNotEqual(first["scope_id"], second["scope_id"])
        for started in (first, second):
            # Starting a scope must boot and validate its project shell rather
            # than claiming readiness after merely creating a directory.
            self.assertEqual(started["state"], "queued")
            self.assertRegex(
                started["scope_id"],
                r"^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$",
            )
            terminal = self.wait_for_operation(started["operation_id"])
            self.assertEqual(terminal["state"], "succeeded")
            self.assertEqual(terminal["scope_id"], started["scope_id"])
            expected_home = (managed_root / started["scope_id"]).resolve()
            self.assertTrue(expected_home.is_dir())
            self.assertEqual(stat.S_IMODE(expected_home.stat().st_mode), 0o700)

        start_log = self.log_text()
        self.assertIn(f"home={(managed_root / first['scope_id']).resolve()}", start_log)
        self.assertIn(f"home={(managed_root / second['scope_id']).resolve()}", start_log)

        first_run = self.structured(
            self.call(
                "scope_run",
                {"scope_id": first["scope_id"], "command": "first-probe"},
            )
        )["data"]
        second_run = self.structured(
            self.call(
                "scope_run",
                {"scope_id": second["scope_id"], "command": "second-probe"},
            )
        )["data"]
        for scope, operation in ((first, first_run), (second, second_run)):
            terminal = self.wait_for_operation(operation["operation_id"])
            self.assertEqual(terminal["scope_id"], scope["scope_id"])
            self.assertEqual(terminal["state"], "succeeded")

        # Public status calls must route through the exact selected home. The
        # fake product log is our only boundary observation; tests never read
        # daemon sockets, databases, VM IDs, or implementation state.
        for scope in (first, second):
            status = self.structured(
                self.call("scope_status", {"scope_id": scope["scope_id"]})
            )
            self.assertTrue(status["ok"])
        default_run = self.structured(
            self.call("shell_run", {"command": "default-probe"})
        )["data"]
        self.assertEqual(
            self.wait_for_operation(default_run["operation_id"])["scope_id"],
            "default",
        )
        log = self.log_text()
        self.assertIn(f"home={(managed_root / first['scope_id']).resolve()}", log)
        self.assertIn(f"home={(managed_root / second['scope_id']).resolve()}", log)
        self.assertIn(f"home={(managed_root / 'default').resolve()}", log)

    def test_scope_stop_is_isolated_preserves_home_and_fails_closed_when_busy(self) -> None:
        self.use_managed_scopes()
        first = self.structured(self.call("scope_start"))["data"]
        second = self.structured(self.call("scope_start"))["data"]
        self.assertEqual(self.wait_for_operation(first["operation_id"])["state"], "succeeded")
        self.assertEqual(self.wait_for_operation(second["operation_id"])["state"], "succeeded")

        busy_run = self.structured(
            self.call(
                "scope_run",
                {
                    "scope_id": first["scope_id"],
                    "command": "sleep-marker",
                    "timeout_ms": 900000,
                },
            )
        )["data"]
        busy_stop = self.structured(
            self.call("scope_stop", {"scope_id": first["scope_id"]})
        )
        self.assertFalse(busy_stop["ok"])
        self.assertTrue(
            {"busy", "active"} & set(busy_stop["error"].lower().split()),
            busy_stop,
        )
        self.structured(
            self.call(
                "operation_cancel", {"operation_id": busy_run["operation_id"]}
            )
        )
        self.assertEqual(
            self.wait_for_operation(busy_run["operation_id"])["state"],
            "cancellation_uncertain",
        )

        first_home = (self.managed_root() / first["scope_id"]).resolve()
        before_stop_log = self.log_text()
        stopped = self.structured(
            self.call("scope_stop", {"scope_id": first["scope_id"]})
        )["data"]
        self.assertEqual(stopped["state"], "queued")
        terminal = self.wait_for_operation(stopped["operation_id"])
        self.assertEqual(terminal["scope_id"], first["scope_id"])
        self.assertEqual(terminal["state"], "succeeded")
        stop_log = self.log_text()[len(before_stop_log):]
        self.assertIn(f"home={first_home}", stop_log)
        self.assertNotIn(second["scope_id"], stop_log)

        stopped_status = self.structured(
            self.call("scope_status", {"scope_id": first["scope_id"]})
        )
        self.assertTrue(stopped_status["ok"])
        self.assertEqual(stopped_status["data"]["state"], "stopped")
        self.assertIsNone(stopped_status["data"]["runtime"])
        stopped_run = self.structured(
            self.call(
                "scope_run",
                {"scope_id": first["scope_id"], "command": "must-not-run"},
            )
        )
        self.assertFalse(stopped_run["ok"])

        # Stopping one scope preserves its durable home and leaves a peer scope
        # usable. Resume/restart of a stopped scope is deliberately future work.
        self.assertTrue(first_home.is_dir())
        second_status = self.structured(
            self.call("scope_status", {"scope_id": second["scope_id"]})
        )
        self.assertTrue(second_status["ok"])
        second_run = self.structured(
            self.call(
                "scope_run",
                {"scope_id": second["scope_id"], "command": "still-running"},
            )
        )["data"]
        self.assertEqual(
            self.wait_for_operation(second_run["operation_id"])["state"],
            "succeeded",
        )

    def test_scope_reset_and_invalid_scope_inputs_are_fail_closed(self) -> None:
        self.use_managed_scopes()
        started = self.structured(self.call("scope_start"))["data"]
        scope_id = started["scope_id"]
        self.assertEqual(self.wait_for_operation(started["operation_id"])["state"], "succeeded")
        scope_home = (self.managed_root() / scope_id).resolve()
        sentinel_write = self.structured(
            self.call(
                "scope_run",
                {"scope_id": scope_id, "command": "write-home-sentinel"},
            )
        )["data"]
        self.assertEqual(
            self.wait_for_operation(sentinel_write["operation_id"])["state"],
            "succeeded",
        )
        before_reset_log = self.log_text()
        reset = self.structured(
            self.call("scope_reset", {"scope_id": scope_id})
        )["data"]
        self.assertEqual(reset["state"], "queued")
        terminal = self.wait_for_operation(reset["operation_id"])
        self.assertEqual(terminal["scope_id"], scope_id)
        self.assertEqual(terminal["state"], "succeeded")
        reset_log = self.log_text()[len(before_reset_log):]
        self.assertNotIn("argv1=workers", reset_log)
        self.assertIn(f"home={scope_home}", reset_log)
        first_boot_at = reset_log.find("argv1=-c\nargv2=true")
        reset_at = reset_log.find("argv1=reset\nargv2=--json", first_boot_at + 1)
        final_boot_at = reset_log.find("argv1=-c\nargv2=true", reset_at + 1)
        self.assertGreaterEqual(first_boot_at, 0, reset_log)
        self.assertGreater(reset_at, first_boot_at, reset_log)
        self.assertGreater(final_boot_at, reset_at, reset_log)
        ready = self.structured(
            self.call("scope_status", {"scope_id": scope_id})
        )
        self.assertTrue(ready["ok"])
        self.assertEqual(ready["data"]["state"], "ready")
        sentinel_read = self.structured(
            self.call(
                "scope_run",
                {"scope_id": scope_id, "command": "read-home-sentinel"},
            )
        )["data"]
        self.assertEqual(
            self.wait_for_operation(sentinel_read["operation_id"])["state"],
            "succeeded",
        )
        sentinel_output = self.structured(
            self.call(
                "operation_output",
                {"operation_id": sentinel_read["operation_id"]},
            )
        )["data"]
        self.assertEqual(
            base64.b64decode(sentinel_output["stdout"]["base64"]),
            b"preserved\n",
        )

        invalid_ids = [
            "../default",
            "DEFAULT",
            "not-a-uuid",
            "00000000-0000-4000-8000-000000000000/escape",
        ]
        for scope_id in invalid_ids:
            response = self.structured(
                self.call("scope_status", {"scope_id": scope_id})
            )
            self.assertFalse(response["ok"], scope_id)
        unknown = self.structured(
            self.call(
                "scope_status",
                {"scope_id": "00000000-0000-4000-8000-000000000000"},
            )
        )
        self.assertFalse(unknown["ok"])

        # Schemas do not admit caller-selected paths, environment, SBX/VM IDs,
        # labels, or lifecycle force flags.
        forbidden = {
            "home": "/tmp/escape",
            "env": {"TOKEN": "secret"},
            "sbx": "/tmp/sbx",
            "vm_id": "victim",
            "force": True,
        }
        for tool, arguments in (
            ("scope_start", forbidden),
            ("scope_run", {"scope_id": scope_id, "command": "probe", **forbidden}),
            ("scope_results_list", {"scope_id": scope_id, **forbidden}),
            (
                "scope_result_get",
                {"scope_id": scope_id, "selector": "1", **forbidden},
            ),
            (
                "scope_prewarm",
                {"scope_id": scope_id, "selection": "all", **forbidden},
            ),
            (
                "scope_workers_reset",
                {"scope_id": scope_id, "selection": "all", **forbidden},
            ),
            ("scope_status", {"scope_id": scope_id, **forbidden}),
            ("scope_reset", {"scope_id": scope_id, **forbidden}),
            ("scope_stop", {"scope_id": scope_id, **forbidden}),
        ):
            result = self.call(tool, arguments)
            self.assertTrue(result["isError"], (tool, result))

    def test_ready_reset_and_stop_recover_a_missing_daemon_boot_first(self) -> None:
        self.use_managed_scopes()

        reset_scope = self.start_scope()
        reset_home = self.managed_root() / reset_scope["scope_id"]
        (reset_home / ".fake-daemon-ready").unlink()
        reset_log_start = len(self.log_text())
        reset = self.structured(
            self.call("scope_reset", {"scope_id": reset_scope["scope_id"]})
        )["data"]
        self.assertEqual(self.wait_for_operation(reset["operation_id"])["state"], "succeeded")
        reset_log = self.log_text()[reset_log_start:]
        first_boot = reset_log.find("argv1=-c\nargv2=true")
        cleanup = reset_log.find("argv1=reset\nargv2=--json", first_boot + 1)
        final_boot = reset_log.find("argv1=-c\nargv2=true", cleanup + 1)
        self.assertGreaterEqual(first_boot, 0, reset_log)
        self.assertGreater(cleanup, first_boot, reset_log)
        self.assertGreater(final_boot, cleanup, reset_log)

        stop_scope = self.start_scope()
        stop_home = self.managed_root() / stop_scope["scope_id"]
        (stop_home / ".fake-daemon-ready").unlink()
        stop_log_start = len(self.log_text())
        stop = self.structured(
            self.call("scope_stop", {"scope_id": stop_scope["scope_id"]})
        )["data"]
        self.assertEqual(self.wait_for_operation(stop["operation_id"])["state"], "succeeded")
        stop_log = self.log_text()[stop_log_start:]
        boot = stop_log.find("argv1=-c\nargv2=true")
        cleanup = stop_log.find("argv1=stop\nargv2=--json", boot + 1)
        self.assertGreaterEqual(boot, 0, stop_log)
        self.assertGreater(cleanup, boot, stop_log)
        stopped = self.structured(
            self.call("scope_status", {"scope_id": stop_scope["scope_id"]})
        )
        self.assertEqual(stopped["data"]["state"], "stopped")

    def test_explicit_home_is_legacy_default_only(self) -> None:
        """An old exact --home is never reinterpreted as a dynamic scope root."""
        doctor = self.structured(self.call("doctor"))
        self.assertEqual(Path(doctor["data"]["home"]), self.home.resolve())
        rejected = self.structured(self.call("scope_start"))
        self.assertFalse(rejected["ok"])
        self.assertIn("legacy --home", rejected["error"].lower())
        self.assertEqual(list(self.home.iterdir()), [])

    def test_scope_list_rediscovers_generated_scopes_across_mcp_restart(self) -> None:
        self.use_managed_scopes()
        first = self.start_scope()
        second = self.start_scope()

        listed = self.structured(self.call("scope_list"))["data"]
        self.assertEqual(listed["schema"], "marsh.mcp.scope-list/v1")
        by_id = {entry["scope_id"]: entry for entry in listed["scopes"]}
        self.assertEqual(set(by_id), {"default", first["scope_id"], second["scope_id"]})
        self.assertEqual(by_id["default"]["state"], "ready")
        self.assertEqual(by_id[first["scope_id"]]["state"], "ready")
        # Discovery is deliberately structural: it must not disclose the host
        # home path, daemon endpoint, VM/SBX identifiers, or credentials.
        for entry in listed["scopes"]:
            self.assertEqual(
                set(entry), {"scope_id", "state", "created_unix_ms"}, entry
            )

        self.use_managed_scopes()
        rediscovered = self.structured(self.call("scope_list"))["data"]
        rediscovered_by_id = {
            entry["scope_id"]: entry for entry in rediscovered["scopes"]
        }
        self.assertEqual(set(rediscovered_by_id), set(by_id))
        self.assertEqual(rediscovered_by_id[first["scope_id"]]["state"], "ready")
        status = self.structured(
            self.call("scope_status", {"scope_id": first["scope_id"]})
        )
        self.assertTrue(status["ok"])
        self.assertEqual(status["data"]["runtime_state"], "ready")

    def test_explicit_managed_root_is_durably_bound_to_one_workspace(self) -> None:
        self.peer.close()
        root = Path(self.tempdir.name)
        scope_root = root / "explicit-scopes"
        first_command = [
            str(MCP_BINARY),
            "serve",
            "--workspace",
            str(self.workspace),
            "--scope-root",
            str(scope_root),
            "--marsh",
            str(self.fake),
            "--sbx",
            str(self.fake_sbx),
        ]
        self.peer = JsonRpcPeer(first_command, self.environment)
        self.assertIn("result", self.peer.initialize())
        self.peer.close()
        shutil.rmtree(scope_root / "default")

        other_workspace = root / "other-workspace"
        other_workspace.mkdir()
        second_command = first_command.copy()
        second_command[second_command.index(str(self.workspace))] = str(other_workspace)
        completed = subprocess.run(
            second_command,
            input="",
            text=True,
            capture_output=True,
            env=self.environment,
            timeout=3,
            check=False,
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("bound to a different canonical workspace identity", completed.stderr)
        self.assertFalse((scope_root / "default").exists())

    def test_managed_root_rejects_guest_writable_marsh_home(self) -> None:
        self.peer.close()
        unsafe_root = Path(self.environment["HOME"]) / ".marsh" / "control"
        command = [
            str(MCP_BINARY),
            "serve",
            "--workspace",
            str(self.workspace),
            "--scope-root",
            str(unsafe_root),
            "--marsh",
            str(self.fake),
            "--sbx",
            str(self.fake_sbx),
        ]
        completed = subprocess.run(
            command,
            cwd=ROOT,
            env=self.environment,
            input="",
            text=True,
            capture_output=True,
            timeout=3,
            check=False,
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(completed.stdout, "")
        self.assertIn("guest-writable marsh home", completed.stderr.lower())

    def test_overlapping_scope_root_is_rejected_without_creating_it(self) -> None:
        self.peer.close()
        unsafe_root = self.workspace / "must-not-be-created"
        command = [
            str(MCP_BINARY),
            "serve",
            "--workspace",
            str(self.workspace),
            "--scope-root",
            str(unsafe_root),
            "--marsh",
            str(self.fake),
            "--sbx",
            str(self.fake_sbx),
        ]
        completed = subprocess.run(
            command,
            cwd=ROOT,
            env=self.environment,
            input="",
            text=True,
            capture_output=True,
            timeout=3,
            check=False,
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(completed.stdout, "")
        self.assertIn("must not overlap", completed.stderr.lower())
        self.assertFalse(unsafe_root.exists())

    def test_malformed_safe_registry_record_is_isolated_failed(self) -> None:
        self.use_managed_scopes()
        started = self.start_scope()
        self.peer.close()
        registry_path = self.managed_root() / "scopes.json"
        registry = json.loads(registry_path.read_text(encoding="utf-8"))
        registry[started["scope_id"]] = {
            "state": {"malformed": True},
            "created_unix_ms": "not-a-time",
            "caller_home": "/tmp/must-not-be-trusted",
        }
        registry_path.write_text(json.dumps(registry), encoding="utf-8")
        registry_path.chmod(0o600)

        self.peer = self.start_peer(explicit_home=False)
        initialize = self.peer.initialize()
        self.assertEqual(initialize["result"]["protocolVersion"], PROTOCOL_VERSION)
        listed = self.structured(self.call("scope_list"))["data"]["scopes"]
        entry = next(item for item in listed if item["scope_id"] == started["scope_id"])
        self.assertEqual(entry["state"], "failed")
        status = self.structured(
            self.call("scope_status", {"scope_id": started["scope_id"]})
        )
        self.assertTrue(status["ok"])
        self.assertEqual(status["data"]["runtime_state"], "unknown")
        self.assertIn("malformed", status["data"]["diagnostic"].lower())
        self.assertLessEqual(len(status["data"]["diagnostic"].encode()), 4096)
        log_before = self.log_text()
        reset = self.structured(
            self.call("scope_reset", {"scope_id": started["scope_id"]})
        )
        self.assertFalse(reset["ok"])
        self.assertIn("new empty --scope-root", reset["error"])
        self.assertIn("No replacement path was modified", reset["error"])
        self.assertEqual(self.log_text(), log_before)
        self.assertTrue((self.managed_root() / started["scope_id"]).is_dir())
        self.assertNotIn("/tmp/must-not-be-trusted", self.log_text())
        doctor = self.structured(self.call("doctor"))
        self.assertTrue(doctor["ok"])
        default_probe = self.structured(
            self.call("shell_run", {"command": "probe"})
        )["data"]
        self.assertEqual(
            self.wait_for_operation(default_probe["operation_id"])["state"],
            "succeeded",
        )

    def test_semantically_corrupt_removing_records_are_isolated(self) -> None:
        self.use_managed_scopes()
        missing_identity = self.start_scope()
        wrong_identity = self.start_scope()
        healthy = self.start_scope()
        self.peer.close()

        registry_path = self.managed_root() / "scopes.json"
        registry = json.loads(registry_path.read_text(encoding="utf-8"))
        registry["default"]["state"] = "removing"
        registry[missing_identity["scope_id"]]["state"] = "removing"
        del registry[missing_identity["scope_id"]]["home_identity"]
        registry[wrong_identity["scope_id"]]["state"] = "removing"
        registry[wrong_identity["scope_id"]]["home_identity"]["inode"] = 0
        registry_path.write_text(json.dumps(registry), encoding="utf-8")
        registry_path.chmod(0o600)

        self.peer = self.start_peer(explicit_home=False)
        self.assertEqual(
            self.peer.initialize()["result"]["protocolVersion"], PROTOCOL_VERSION
        )
        listed = self.structured(self.call("scope_list"))["data"]["scopes"]
        by_id = {entry["scope_id"]: entry for entry in listed}
        self.assertEqual(by_id["default"]["state"], "failed")
        self.assertEqual(by_id[missing_identity["scope_id"]]["state"], "failed")
        self.assertEqual(by_id[wrong_identity["scope_id"]]["state"], "failed")
        self.assertEqual(by_id[healthy["scope_id"]]["state"], "ready")
        for scope_id in (missing_identity["scope_id"], wrong_identity["scope_id"]):
            status = self.structured(
                self.call("scope_status", {"scope_id": scope_id})
            )
            self.assertTrue(status["ok"])
            self.assertEqual(status["data"]["runtime_state"], "unknown")
            self.assertIn("isolated", status["data"]["diagnostic"])
            self.assertTrue((self.managed_root() / scope_id).is_dir())

    def test_replaced_removing_home_is_never_followed_or_deleted(self) -> None:
        self.use_managed_scopes()
        started = self.start_scope()
        scope_id = started["scope_id"]
        stop = self.structured(
            self.call("scope_stop", {"scope_id": scope_id})
        )["data"]
        self.assertEqual(self.wait_for_operation(stop["operation_id"])["state"], "succeeded")
        self.peer.close()

        registry_path = self.managed_root() / "scopes.json"
        registry = json.loads(registry_path.read_text(encoding="utf-8"))
        registry[scope_id]["state"] = "removing"
        registry_path.write_text(json.dumps(registry), encoding="utf-8")
        registry_path.chmod(0o600)
        scope_home = self.managed_root() / scope_id
        original_home = self.managed_root() / f"{scope_id}.original"
        scope_home.rename(original_home)
        scope_home.mkdir(mode=0o700)
        marker = scope_home / "must-survive"
        marker.write_text("replacement", encoding="utf-8")

        self.peer = self.start_peer(explicit_home=False)
        self.assertEqual(
            self.peer.initialize()["result"]["protocolVersion"], PROTOCOL_VERSION
        )
        status = self.structured(
            self.call("scope_status", {"scope_id": scope_id})
        )
        self.assertTrue(status["ok"])
        self.assertEqual(status["data"]["state"], "failed")
        self.assertIn("identity changed", status["data"]["diagnostic"])
        removal = self.structured(
            self.call("scope_remove", {"scope_id": scope_id})
        )
        self.assertFalse(removal["ok"])
        self.assertTrue(marker.is_file())
        self.assertTrue(original_home.is_dir())

    def test_scope_remove_accepts_only_generated_stopped_scope(self) -> None:
        self.use_managed_scopes()
        ready = self.start_scope()
        ready_id = ready["scope_id"]
        ready_home = (self.managed_root() / ready_id).resolve()
        rejected_ready = self.structured(
            self.call("scope_remove", {"scope_id": ready_id})
        )
        self.assertFalse(rejected_ready["ok"])
        self.assertTrue(ready_home.is_dir())

        failed_marker = Path(self.environment["TMPDIR"]) / "fail-next-start"
        failed_marker.touch()
        failed = self.structured(self.call("scope_start"))["data"]
        failed_terminal = self.wait_for_operation(failed["operation_id"])
        self.assertEqual(failed_terminal["state"], "failed")
        failed_status = self.structured(
            self.call("scope_status", {"scope_id": failed["scope_id"]})
        )
        self.assertTrue(failed_status["ok"])
        self.assertEqual(failed_status["data"]["state"], "failed")
        rejected_failed = self.structured(
            self.call("scope_remove", {"scope_id": failed["scope_id"]})
        )
        self.assertFalse(rejected_failed["ok"])

        stop = self.structured(
            self.call("scope_stop", {"scope_id": ready_id})
        )["data"]
        self.assertEqual(self.wait_for_operation(stop["operation_id"])["state"], "succeeded")
        removed = self.structured(
            self.call("scope_remove", {"scope_id": ready_id})
        )["data"]
        self.assertEqual(removed["state"], "queued")
        removal = self.wait_for_operation(removed["operation_id"])
        self.assertEqual(removal["scope_id"], ready_id)
        self.assertEqual(removal["kind"], "scope_remove")
        self.assertEqual(removal["state"], "succeeded")
        self.assertFalse(ready_home.exists())

        for scope_id in (
            "default",
            "00000000-0000-4000-8000-000000000000",
        ):
            rejected = self.structured(
                self.call("scope_remove", {"scope_id": scope_id})
            )
            self.assertFalse(rejected["ok"])
        forbidden = self.call(
            "scope_remove",
            {
                "scope_id": failed["scope_id"],
                "home": "/tmp/victim",
                "force": True,
                "vm_id": "victim",
            },
        )
        self.assertTrue(forbidden["isError"])
        forbidden_list = self.call(
            "scope_list", {"home": "/tmp/victim", "vm_id": "victim"}
        )
        self.assertTrue(forbidden_list["isError"])

        self.use_managed_scopes()
        listed = self.structured(self.call("scope_list"))["data"]["scopes"]
        self.assertNotIn(ready_id, {entry["scope_id"] for entry in listed})

    def test_stopped_default_and_alias_gating_persist_across_restart(self) -> None:
        self.use_managed_scopes()
        stop = self.structured(
            self.call("scope_stop", {"scope_id": "default"})
        )["data"]
        self.assertEqual(self.wait_for_operation(stop["operation_id"])["state"], "succeeded")

        self.use_managed_scopes()
        status = self.structured(
            self.call("scope_status", {"scope_id": "default"})
        )
        self.assertTrue(status["ok"])
        self.assertEqual(status["data"]["state"], "stopped")
        self.assertEqual(status["data"]["runtime_state"], "absent")
        self.assertIsNone(status["data"]["runtime"])

        log_before = self.log_text()
        gated_calls = (
            ("status", {}),
            ("results_list", {}),
            ("result_get", {"selector": "1"}),
            ("prewarm", {}),
            ("workers_reset", {}),
            ("shell_run", {"command": "must-not-run"}),
        )
        for tool, arguments in gated_calls:
            result = self.structured(self.call(tool, arguments))
            self.assertFalse(result["ok"], (tool, result))
        qualification = self.structured(
            self.call("qualify", {"gate": "source"})
        )
        self.assertFalse(qualification["ok"])
        self.assertIn("full SBX control is disabled", qualification["error"])
        self.assertEqual(self.log_text(), log_before)

    def test_scope_reset_does_not_bypass_sixteen_live_scope_limit(self) -> None:
        self.use_managed_scopes()
        scopes = [self.start_scope() for _ in range(16)]
        rejected = self.structured(self.call("scope_start"))
        self.assertFalse(rejected["ok"])
        self.assertIn("limit", rejected["error"].lower())

        stopped_id = scopes[0]["scope_id"]
        stop = self.structured(
            self.call("scope_stop", {"scope_id": stopped_id})
        )["data"]
        self.assertEqual(self.wait_for_operation(stop["operation_id"])["state"], "succeeded")
        replacement = self.start_scope()
        self.assertNotEqual(replacement["scope_id"], stopped_id)

        # There are again 16 live generated scopes. Resetting the retained
        # Stopped scope would make 17 live; it must fail before queuing work and
        # leave the scope Stopped.
        log_before = self.log_text()
        reset = self.structured(
            self.call("scope_reset", {"scope_id": stopped_id})
        )
        self.assertFalse(reset["ok"])
        self.assertIn("limit", reset["error"].lower())
        self.assertEqual(self.log_text(), log_before)
        stopped = self.structured(
            self.call("scope_status", {"scope_id": stopped_id})
        )
        self.assertTrue(stopped["ok"])
        self.assertEqual(stopped["data"]["state"], "stopped")

        listed = self.structured(self.call("scope_list"))["data"]["scopes"]
        generated = [entry for entry in listed if entry["scope_id"] != "default"]
        self.assertEqual(len(generated), 17)
        self.assertEqual(
            sum(entry["state"] == "ready" for entry in generated), 16
        )
        self.assertEqual(
            sum(entry["state"] == "stopped" for entry in generated), 1
        )

    def test_failed_scope_counts_live_but_can_recover_at_limit(self) -> None:
        self.use_managed_scopes()
        failed_marker = Path(self.environment["TMPDIR"]) / "fail-next-start"
        failed_marker.touch()
        failed = self.structured(self.call("scope_start"))["data"]
        self.assertEqual(
            self.wait_for_operation(failed["operation_id"])["state"], "failed"
        )
        for _ in range(15):
            self.start_scope()
        rejected = self.structured(self.call("scope_start"))
        self.assertFalse(rejected["ok"])
        self.assertIn("limit", rejected["error"].lower())

        recovery = self.structured(
            self.call("scope_reset", {"scope_id": failed["scope_id"]})
        )["data"]
        self.assertEqual(
            self.wait_for_operation(recovery["operation_id"])["state"], "succeeded"
        )

    def test_failed_status_is_unknown_and_scope_home_replacement_fails_closed(self) -> None:
        self.use_managed_scopes()
        failed_marker = Path(self.environment["TMPDIR"]) / "fail-next-start"
        failed_marker.touch()
        failed = self.structured(self.call("scope_start"))["data"]
        terminal = self.wait_for_operation(failed["operation_id"])
        self.assertEqual(terminal["state"], "failed")
        self.use_managed_scopes()
        status = self.structured(
            self.call("scope_status", {"scope_id": failed["scope_id"]})
        )
        self.assertTrue(status["ok"])
        self.assertEqual(status["data"]["state"], "failed")
        self.assertEqual(status["data"]["runtime_state"], "unknown")
        self.assertIsNone(status["data"]["runtime"])
        self.assertTrue(status["data"]["diagnostic"])
        self.assertLessEqual(len(status["data"]["diagnostic"].encode()), 4096)
        self.assertEqual(status["data"]["last_operation_id"], failed["operation_id"])

        recovery_log_start = len(self.log_text())
        recovery = self.structured(
            self.call("scope_reset", {"scope_id": failed["scope_id"]})
        )["data"]
        self.assertEqual(recovery["state"], "queued")
        recovered = self.wait_for_operation(recovery["operation_id"])
        self.assertEqual(recovered["state"], "succeeded")
        self.assertEqual(recovered["scope_id"], failed["scope_id"])
        recovered_status = self.structured(
            self.call("scope_status", {"scope_id": failed["scope_id"]})
        )
        self.assertTrue(recovered_status["ok"])
        self.assertEqual(recovered_status["data"]["state"], "ready")
        self.assertEqual(recovered_status["data"]["runtime_state"], "ready")
        recovery_log = self.log_text()[recovery_log_start:]
        first_boot_at = recovery_log.find("argv1=-c\nargv2=true")
        reset_at = recovery_log.find(
            "argv1=reset\nargv2=--json", first_boot_at + 1
        )
        final_boot_at = recovery_log.find("argv1=-c\nargv2=true", reset_at + 1)
        self.assertGreaterEqual(first_boot_at, 0, recovery_log)
        self.assertGreater(reset_at, first_boot_at, recovery_log)
        self.assertGreater(final_boot_at, reset_at, recovery_log)

        replaced = self.start_scope()
        replaced_home = self.managed_root() / replaced["scope_id"]
        original_home = replaced_home.with_name(f"{replaced['scope_id']}.original")
        replaced_home.rename(original_home)
        replaced_home.mkdir(mode=0o700)
        log_before = self.log_text()
        rejected = self.structured(
            self.call(
                "scope_run",
                {"scope_id": replaced["scope_id"], "command": "must-not-run"},
            )
        )
        self.assertFalse(rejected["ok"])
        self.assertEqual(self.log_text(), log_before)

        linked = self.start_scope()
        linked_home = self.managed_root() / linked["scope_id"]
        linked_original = linked_home.with_name(f"{linked['scope_id']}.original")
        linked_home.rename(linked_original)
        linked_home.symlink_to(linked_original, target_is_directory=True)
        log_before = self.log_text()
        rejected = self.structured(
            self.call("scope_status", {"scope_id": linked["scope_id"]})
        )
        self.assertFalse(rejected["ok"])
        self.assertEqual(self.log_text(), log_before)

        # Keep TemporaryDirectory cleanup robust after deliberately constructing
        # a symlink and displaced directories.
        linked_home.unlink()
        shutil.rmtree(linked_original)
        shutil.rmtree(replaced_home)
        original_home.rename(replaced_home)

    def test_scope_ids_require_canonical_lowercase_uuid_in_calls_and_registry(self) -> None:
        self.use_managed_scopes()
        started = self.start_scope()
        uppercase = started["scope_id"].upper()
        self.assertNotEqual(uppercase, started["scope_id"])
        rejected = self.structured(
            self.call("scope_status", {"scope_id": uppercase})
        )
        self.assertFalse(rejected["ok"])
        self.assertIn("canonical", rejected["error"].lower())

        # A corrupt/noncanonical persisted key has no safe caller-visible scope
        # identity. Startup preserves it without trusting a path, exposes a
        # bounded doctor warning, and continues serving the valid scopes.
        self.peer.close()
        root = self.managed_root()
        # On the default case-insensitive macOS filesystem the uppercase name
        # aliases the already-created lowercase home. Reuse it; the registry
        # key itself is the malformed input under test.
        registry = {
            uppercase: {
                "state": "stopped",
                "created_unix_ms": 1,
                "last_operation_id": None,
                "diagnostic": None,
            }
        }
        registry_path = root / "scopes.json"
        registry_path.write_text(json.dumps(registry), encoding="utf-8")
        registry_path.chmod(0o600)
        command = [
            str(MCP_BINARY),
            "serve",
            "--workspace",
            str(self.workspace),
            "--scope-root",
            str(self.managed_root()),
            "--marsh",
            str(self.fake),
            "--sbx",
            str(self.fake_sbx),
        ]
        self.peer = JsonRpcPeer(command, self.environment)
        initialize = self.peer.initialize()
        self.assertEqual(initialize["result"]["protocolVersion"], PROTOCOL_VERSION)
        doctor = self.structured(self.call("doctor"))
        self.assertTrue(doctor["ok"])
        diagnostics = doctor["data"]["registry_diagnostics"]
        self.assertEqual(len(diagnostics), 1)
        self.assertLessEqual(len(diagnostics[0].encode()), 4096)
        self.assertIn("noncanonical", diagnostics[0])
        self.assertNotIn(uppercase, diagnostics[0])
        listed = self.structured(self.call("scope_list"))["data"]["scopes"]
        self.assertEqual({entry["scope_id"] for entry in listed}, {"default"})
        default_probe = self.structured(
            self.call("shell_run", {"command": "probe"})
        )["data"]
        self.assertEqual(
            self.wait_for_operation(default_probe["operation_id"])["state"],
            "succeeded",
        )
        started_again = self.start_scope()
        self.assertNotEqual(started_again["scope_id"], started["scope_id"])
        persisted = json.loads(registry_path.read_text(encoding="utf-8"))
        self.assertIn(uppercase, persisted)

    def test_interrupted_async_removal_resumes_on_mcp_restart(self) -> None:
        self.use_managed_scopes()
        started = self.start_scope()
        scope_id = started["scope_id"]
        stop = self.structured(
            self.call("scope_stop", {"scope_id": scope_id})
        )["data"]
        self.assertEqual(self.wait_for_operation(stop["operation_id"])["state"], "succeeded")
        scope_home = self.managed_root() / scope_id
        self.assertTrue(scope_home.is_dir())

        # Model a crash after the durable Removing transition but before home
        # deletion. Startup must preserve a retryable transition rather than
        # resurrecting the scope as Ready/Stopped or losing its exact identity.
        self.peer.close()
        registry_path = self.managed_root() / "scopes.json"
        registry = json.loads(registry_path.read_text(encoding="utf-8"))
        registry[scope_id]["state"] = "removing"
        registry[scope_id]["diagnostic"] = "interrupted removal fixture"
        registry_path.write_text(json.dumps(registry), encoding="utf-8")
        registry_path.chmod(0o600)

        self.peer = self.start_peer(explicit_home=False)
        initialize = self.peer.initialize()
        self.assertEqual(initialize["result"]["protocolVersion"], PROTOCOL_VERSION)
        listed = self.structured(self.call("scope_list"))["data"]["scopes"]
        by_id = {entry["scope_id"]: entry for entry in listed}
        self.assertEqual(by_id[scope_id]["state"], "removing")
        transitioning = self.structured(
            self.call("scope_status", {"scope_id": scope_id})
        )
        self.assertTrue(transitioning["ok"])
        self.assertEqual(transitioning["data"]["state"], "removing")
        self.assertEqual(transitioning["data"]["runtime_state"], "transitioning")
        self.assertIsNone(transitioning["data"]["runtime"])

        resumed = self.structured(
            self.call("scope_remove", {"scope_id": scope_id})
        )["data"]
        self.assertEqual(resumed["state"], "queued")
        self.assertEqual(
            self.wait_for_operation(resumed["operation_id"])["state"],
            "succeeded",
        )
        listed = self.structured(self.call("scope_list"))["data"]["scopes"]
        self.assertNotIn(scope_id, {entry["scope_id"] for entry in listed})
        self.assertFalse(scope_home.exists())

    def test_missing_workspace_fails_before_protocol(self) -> None:
        missing = self.workspace / "does-not-exist"
        command = [
            str(MCP_BINARY),
            "serve",
            "--workspace",
            str(missing),
            "--home",
            str(self.home),
            "--marsh",
            str(self.fake),
            "--sbx",
            str(self.fake_sbx),
        ]
        completed = subprocess.run(
            command,
            cwd=ROOT,
            env=self.environment,
            input="",
            text=True,
            capture_output=True,
            timeout=3,
            check=False,
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(completed.stdout, "")
        self.assertIn("cannot canonicalize workspace", completed.stderr)


if __name__ == "__main__":
    unittest.main()
