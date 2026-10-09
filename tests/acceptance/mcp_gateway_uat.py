#!/usr/bin/env python3
"""Disposable stock Gateway UAT for published MCP pipelines."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import pathlib
import shlex
import signal
import subprocess
import sys
import time
import traceback
import uuid

from provenance import host_only_path, stock_baseline, stock_vm_names, verify_candidate, remove_owned_stock_vm


GATEWAY_PROBE = r'''
import base64, json, os, sys, urllib.request
url = os.environ["MCP_GATEWAY_URL"]
name, encoded_input, list_only = sys.argv[1:]
input_text = base64.b64decode(encoded_input[1:]).decode("utf-8")
headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream", "MCP-Protocol-Version": "2025-06-18"}
def post(message):
    req = urllib.request.Request(url, data=json.dumps(message).encode(), method="POST", headers=headers)
    with urllib.request.urlopen(req, timeout=25) as response:
        session = response.headers.get("Mcp-Session-Id")
        if session:
            headers["Mcp-Session-Id"] = session
        body = response.read(1024 * 1024).decode()
    if body.startswith("event:"):
        for line in body.splitlines():
            if line.startswith("data: "):
                return json.loads(line[6:])
    return json.loads(body) if body else None
post({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"marsh-gateway-uat","version":"1"}}})
post({"jsonrpc":"2.0","method":"notifications/initialized"})
listed = post({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})
tools = [tool["name"] for tool in listed.get("result",{}).get("tools",[])]
called = post({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":name,"arguments":{"input":input_text}}}) if name in tools and list_only != "1" else None
print(json.dumps({"tools":tools,"call":called},sort_keys=True))
'''

GATEWAY_CANCEL_PROBE = r'''
import json, os, sys, threading, time, urllib.request
url = os.environ["MCP_GATEWAY_URL"]
name = sys.argv[1]
headers = {"Content-Type":"application/json", "Accept":"application/json, text/event-stream", "MCP-Protocol-Version":"2025-06-18"}
def post(message, timeout=20):
    req = urllib.request.Request(url, data=json.dumps(message).encode(), method="POST", headers=headers)
    with urllib.request.urlopen(req, timeout=timeout) as response:
        session = response.headers.get("Mcp-Session-Id")
        if session:
            headers["Mcp-Session-Id"] = session
        body = response.read(1024 * 1024).decode()
    if body.startswith("event:"):
        for line in body.splitlines():
            if line.startswith("data: "):
                return json.loads(line[6:])
    return json.loads(body) if body else None
post({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"marsh-cancel-uat","version":"1"}}})
post({"jsonrpc":"2.0","method":"notifications/initialized"})
state = {}
started = time.monotonic()
def call():
    try:
        state["call"] = post({"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":name,"arguments":{"input":""}}}, 20)
    except Exception as error:
        state["call_error"] = str(error)
thread = threading.Thread(target=call, daemon=True)
thread.start()
sys.stdin.readline()
try:
    state["cancel_response"] = post({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":13,"reason":"disposable UAT"}}, 10)
except Exception as error:
    state["cancel_error"] = str(error)
thread.join(12)
state["call_pending"] = thread.is_alive()
state["elapsed_seconds"] = round(time.monotonic() - started, 3)
print(json.dumps(state,sort_keys=True))
'''


def sha256(path: pathlib.Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class CoreJourneyComplete(Exception):
    """Internal control flow after a successful core-only journey."""


def wait_marker(path: pathlib.Path, child: subprocess.Popen[bytes], timeout: float = 120) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.is_file():
            return
        if child.poll() is not None:
            raise AssertionError(f"publishing shell exited before {path.name}: {child.returncode}")
        time.sleep(0.1)
    raise TimeoutError(f"publishing shell did not produce {path.name}")


def wait_until(check, description: str, timeout: float = 15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.1)
    raise AssertionError(f"timed out waiting for {description}")


def gateway_data(probe: dict, name: str, expected: bytes) -> dict:
    assert name in probe["tools"], f"Gateway did not discover {name}: {probe['tools']}"
    response = probe["call"]
    assert response is not None and "result" in response, response
    result = response["result"]
    assert result.get("isError") is not True, result
    data = result["structuredContent"]["data"]
    assert data["outcome"] == "success" and data["exit_code"] == 0, data
    assert base64.b64decode(data["stdout_base64"]) == expected, data
    return data


def gateway_call(sbx: str, sandbox: str, tool: str, input_text: str, *, cwd: pathlib.Path, env: dict[str, str], timeout: float = 90, list_only: bool = False) -> dict:
    """Call one loaded tool through an existing stock sandbox; own no scope state."""
    encoded_input = "x" + base64.b64encode(input_text.encode("utf-8")).decode("ascii")
    command = [sbx, "exec", sandbox, "python3", "-c", GATEWAY_PROBE, tool, encoded_input, "1" if list_only else "0"]
    result = subprocess.run(command, cwd=cwd, env=env, stdin=subprocess.DEVNULL, capture_output=True, timeout=timeout)
    if result.returncode:
        raise AssertionError(f"Gateway probe failed: {result.stderr.decode(errors='replace')}")
    return json.loads(result.stdout)


def gateway_cancel(sbx: str, sandbox: str, tool: str, *, cwd: pathlib.Path, env: dict[str, str], start_marker: pathlib.Path | None = None) -> dict:
    command = [sbx, "exec", sandbox, "python3", "-c", GATEWAY_CANCEL_PROBE, tool]
    process = subprocess.Popen(command, cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    try:
        if start_marker is None:
            time.sleep(1.5)
            started_marker_seen = None
        else:
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline and not start_marker.exists() and process.poll() is None:
                time.sleep(0.05)
            started_marker_seen = start_marker.exists()
        output, errors = process.communicate(input=b"go\n", timeout=35)
    except BaseException:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
        process.communicate()
        raise
    if process.returncode:
        raise AssertionError(f"Gateway cancel probe failed: {errors.decode(errors='replace')}")
    result = json.loads(output)
    result["started_marker_seen"] = started_marker_seen
    return result


class Scope:
    def __init__(self, args: argparse.Namespace) -> None:
        from run import IsolatedScopeCleanup, disposable_root, resolve_executable, scoped_control_home

        self._cleanup_cls = IsolatedScopeCleanup
        self.marsh = pathlib.Path(args.marsh).resolve(strict=True)
        self.guest_artifacts, self.build_receipt = verify_candidate(args, str(self.marsh))
        self.marshd = self.marsh.with_name("marshd").resolve(strict=True)
        self.mcp = self.marsh.with_name("marsh-mcp").resolve(strict=True)
        self.sbx = resolve_executable(args.sbx)
        self.stock_before = stock_baseline(self.sbx)
        self.stock_after: dict[str, str] | None = None
        self.root = disposable_root("marsh-mcp-gateway-uat-")
        self.real_home = pathlib.Path(os.environ["HOME"]).resolve(strict=True)
        self.home = self.root / "selected-home"
        self.control_root = self.root / "control"
        self.project = self.root / "project"
        for path in (self.home, self.project, self.control_root):
            path.mkdir(mode=0o700)
        self.control_home = scoped_control_home(self.control_root, self.home)
        commands = getattr(args, "commands", None)
        if commands is not None:
            # Installed before the first public command starts the isolated
            # daemon, which freezes its command registry at startup.
            (self.control_home / "commands.json").write_bytes(pathlib.Path(commands).read_bytes())
        self.environment = os.environ.copy()
        shell_history = self.project / ".marsh-uat-history"
        shell_history.touch(mode=0o600, exist_ok=False)
        self.environment["HISTFILE"] = str(shell_history)
        self.environment.update({"HOME":str(self.real_home), "MARSH_HOME":str(self.home), "MARSH_CONTROL_HOME":str(self.control_root), "MARSH_SBX":self.sbx, "MARSH_GUEST_ARTIFACTS":str(self.guest_artifacts)})
        self.tag = uuid.uuid4().hex[:8]
        self.sandbox = f"marsh-mcp-rec-{self.tag}"
        self.tools = [f"{kind}_{self.tag}" for kind in ("rec", "private", "restart", "binary", "pipefail", "inputcap", "outputcap", "cancel")]
        self.published: set[str] = set()
        self.sandbox_created = False
        self.sandbox_id = None
        self.child: subprocess.Popen[bytes] | None = None
        self.log_out = None
        self.log_err = None
        self.scope_started = False
        self.owned_scope_id = None
        self.owned_daemon_id = None
        self.owned_daemon_pid = None
        self.owned_daemon_process_identity = None
        self.owned_daemon_control_token = None
        self.owned_vms: set[str] = set()
        self.owned_names_seen: set[str] = set()
        self.extra_owned_names: set[str] = {self.sandbox}  # created directly by this harness

    def owned_stock_names(self) -> set[str]:
        return self._cleanup_cls.owned_stock_names(self)

    def stock_leftover_errors(self, after: dict[str, str]) -> list[str]:
        return self._cleanup_cls.stock_leftover_errors(self, after)

    def command(self, *args: str, timeout: float = 180) -> subprocess.CompletedProcess[bytes]:
        return subprocess.run([str(self.marsh), *args], cwd=self.project, env=self.environment, stdin=subprocess.DEVNULL, capture_output=True, timeout=timeout)

    def status(self) -> dict:
        result = self.command("status", "--json", timeout=30)
        if result.returncode:
            raise AssertionError(f"isolated status failed: {result.stderr.decode(errors='replace')}")
        document = json.loads(result.stdout)
        self._cleanup_cls.remember_owned_status(self, document)
        return document

    def gateway(self, tool: str, text: str, *, list_only: bool = False) -> dict:
        return gateway_call(self.sbx, self.sandbox, tool, text, cwd=self.root, env=self.environment, list_only=list_only)

    def gateway_ready(self, tool: str, text: str) -> dict:
        def discovered():
            probe = self.gateway(tool, text)
            return probe if tool in probe["tools"] else None
        return wait_until(
            discovered,
            f"Gateway discovery of {tool}",
            20,
        )

    def sandbox_boot_id(self) -> str:
        result = subprocess.run([self.sbx, "exec", self.sandbox, "cat", "/proc/sys/kernel/random/boot_id"], cwd=self.root, env=self.environment, stdin=subprocess.DEVNULL, capture_output=True, timeout=30)
        assert result.returncode == 0, result.stderr.decode(errors="replace")
        boot_id = result.stdout.decode().strip()
        uuid.UUID(boot_id)
        return boot_id

    def publish_pipeline(self, tool: str, pipeline: str) -> None:
        script = f"mcp publish {shlex.quote(tool)} --sandbox {shlex.quote(self.sandbox)} -- {shlex.quote(pipeline)}"
        result = self.command("-c", script, timeout=180)
        assert result.returncode == 0, result.stderr.decode(errors="replace")
        self.published.add(tool)

    def create_sandbox(self) -> None:
        result = subprocess.run([self.sbx, "create", "--name", self.sandbox, "shell"], cwd=self.root, env=self.environment, stdin=subprocess.DEVNULL, capture_output=True, timeout=180)
        if result.returncode:
            raise AssertionError(f"cannot create disposable agent sandbox: {result.stderr.decode(errors='replace')}")
        self.sandbox_created = True
        self.sandbox_id = stock_vm_names(self.sbx).get(self.sandbox)
        if not self.sandbox_id or self.sandbox_id in self.stock_before.values():
            raise ValueError("created Gateway sandbox lacks a new stable stock identity")

    def start_publishing_shell(self, evidence: pathlib.Path) -> None:
        tool = self.tools[0]
        script = f'''mcp publish {tool} --description "Gateway UAT uppercase" --sandbox {self.sandbox} -- 'cat | tr a-z A-Z' || exit 41
printf ready > .uat-ready
read _uat_phase || exit 42
set +o history || exit 43
printf private > .uat-private
while [ ! -f .uat-finish ]; do sleep 0.2; done
mcp unpublish {tool} || exit 45
'''
        self.log_out = (evidence / "publishing-shell.stdout").open("wb")
        self.log_err = (evidence / "publishing-shell.stderr").open("wb")
        self.child = subprocess.Popen([str(self.marsh), "-c", script], cwd=self.project, env=self.environment, stdin=subprocess.PIPE, stdout=self.log_out, stderr=self.log_err, start_new_session=True)
        wait_marker(self.project / ".uat-ready", self.child)
        self.published.add(tool)

    def advance_shell(self, marker: str) -> None:
        assert self.child is not None
        if marker:
            assert self.child.stdin is not None
            self.child.stdin.write(b"continue\n")
            self.child.stdin.flush()
            wait_marker(self.project / marker, self.child, 30)
        else:
            (self.project / ".uat-finish").write_text("finish\n", encoding="utf-8")
            self.child.wait(timeout=30)
            assert self.child.returncode == 0, f"publishing shell exited {self.child.returncode}"

    def restart_daemon(self) -> None:
        from run import request_authenticated_daemon_shutdown, stable_process_identity
        if self.owned_daemon_pid is None or self.owned_daemon_control_token is None:
            raise AssertionError("isolated daemon ownership unavailable")
        pid = self.owned_daemon_pid
        identity = self.owned_daemon_process_identity
        request_authenticated_daemon_shutdown(self.home, self.owned_daemon_control_token)
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            current = stable_process_identity(pid)
            if current is None:
                break
            if current != identity:
                raise AssertionError("daemon PID identity changed during restart")
            time.sleep(0.1)
        else:
            raise AssertionError("isolated daemon did not stop")
        self.owned_daemon_id = None
        self.owned_daemon_pid = None
        self.owned_daemon_process_identity = None
        self.owned_daemon_control_token = None
        self.status()

    def server_name(self, tool: str) -> str:
        key = hashlib.sha256(os.fsencode(self.project.resolve()) + b"\0" + os.fsencode((self.home / "home").resolve())).hexdigest()
        return f"marsh-pub-{key[:12]}-{tool}"

    def registration_absent(self, tool: str) -> bool:
        result = subprocess.run([self.sbx,"mcp","inspect",self.server_name(tool),"--json"], cwd=self.root, env=self.environment, stdin=subprocess.DEVNULL, capture_output=True, timeout=20)
        return result.returncode != 0

    def cleanup(self) -> list[str]:
        errors: list[str] = []
        if self.child is not None and self.child.poll() is None:
            try:
                if not (self.project / ".uat-private").is_file():
                    self.advance_shell(".uat-private")
                self.advance_shell("")
            except Exception as error:
                errors.append(f"publishing shell did not exit: {error}")
                if self.child.poll() is None:
                    try:
                        os.killpg(self.child.pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass
                try:
                    self.child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    try:
                        os.killpg(self.child.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    self.child.wait(timeout=5)
        for stream in (self.log_out, self.log_err):
            if stream is not None:
                stream.close()
        for tool in sorted(self.published):
            result = self.command("mcp", "unpublish", tool, timeout=90)
            if result.returncode:
                errors.append(f"could not unpublish {tool}: {result.stderr.decode(errors='replace')}")
        if self.sandbox_created:
            try:
                remove_owned_stock_vm(self.sbx, self.sandbox, self.sandbox_id, self.stock_before)
            except (OSError, ValueError, subprocess.SubprocessError) as error:
                errors.append(f"could not remove owned Gateway sandbox: {error}")
        if errors:
            errors.append(f"isolated root preserved for manual cleanup: {self.root}")
        else:
            errors.extend(self._cleanup_cls.cleanup_isolated_scope(self))
        try:
            self.stock_after = stock_vm_names(self.sbx)
            errors.extend(self.stock_leftover_errors(self.stock_after))
        except Exception as error:
            errors.append(f"independent stock cleanup could not be verified: {error}")
        return errors


def exercise_gateway_cancel(scope: Scope, results: dict, *, sequence_after_interrupt: bool = False, cancel_after_start: bool = False) -> None:
    cancel_tool = scope.tools[7]
    late_marker = scope.project / ".uat-late-marker"
    pipeline = "sleep 15; printf late > .uat-late-marker" if sequence_after_interrupt else "sleep 15 && printf late > .uat-late-marker"
    start_marker = scope.project / ".uat-started" if cancel_after_start else None
    if start_marker is not None:
        pipeline = "printf started > .uat-started; " + pipeline
    scope.publish_pipeline(cancel_tool, pipeline)
    wait_until(lambda: cancel_tool in scope.gateway(cancel_tool, "", list_only=True)["tools"], f"Gateway discovery of {cancel_tool}", 20)
    probe_started = time.monotonic()
    cancellation = gateway_cancel(scope.sbx, scope.sandbox, cancel_tool, cwd=scope.root, env=scope.environment, start_marker=start_marker)
    if start_marker is not None:
        assert cancellation["started_marker_seen"], "published pipeline did not start before cancellation"
    assert not cancellation.get("cancel_error") and not cancellation["call_pending"], cancellation
    call = cancellation.get("call")
    assert call is not None, cancellation
    if "result" in call:
        cancel_result = call["result"]
        assert cancel_result.get("isError") is True, cancel_result
        cancel_data = cancel_result["structuredContent"]["data"]
        assert cancel_data["outcome"] == "cancelled" and cancel_data["output_complete"] is False, cancel_data
        assert cancel_data["cleanup_certainty"] == "uncertain", cancel_data
        cancellation_result = {"gateway_response":"structured_cancelled", "cleanup_certainty":cancel_data["cleanup_certainty"]}
    else:
        error = call.get("error", {})
        assert "cancel" in error.get("message", "").lower(), call
        cancellation_result = {"gateway_response":"jsonrpc_context_canceled", "error_code":error.get("code"), "cleanup_certainty":"not_exposed_by_gateway"}
    cancellation_result["request_id"] = call.get("id")
    cancellation_result["started_marker_seen"] = cancellation["started_marker_seen"]
    cancellation_result["pipeline"] = pipeline
    cancellation_result["probe_elapsed_seconds"] = cancellation["elapsed_seconds"]
    results["cancellation"] = cancellation_result
    marker_seen_at = None
    while time.monotonic() - probe_started < 30:
        if late_marker.exists() and marker_seen_at is None:
            marker_seen_at = round(time.monotonic() - probe_started, 3)
        time.sleep(0.1)
    cancellation_result["marker_observation_seconds"] = round(time.monotonic() - probe_started, 3)
    assert cancellation_result["marker_observation_seconds"] >= 15, "cancel probe did not outwait the late marker"
    cancellation_result["late_marker_elapsed_seconds"] = marker_seen_at
    cancellation_result["late_marker_absent"] = not late_marker.exists()
    assert cancellation_result["late_marker_absent"], "cancelled pipeline continued to its late marker"


def exercise_gateway_edges(scope: Scope, results: dict) -> None:
    """Qualify fixed Brush pipelines through the stock Gateway in one live sandbox."""

    binary_tool = scope.tools[3]
    scope.publish_pipeline(binary_tool, r"printf '\000\377\200A'")
    binary = scope.gateway_ready(binary_tool, "")
    binary_data = gateway_data(binary, binary_tool, b"\x00\xff\x80A")
    assert binary_data["stdout_text_state"] == "lossy", binary_data
    assert binary_data["output_complete"] is True and not binary_data["stdout_truncated"], binary_data
    results["binary_output"] = {"stdout_base64":binary_data["stdout_base64"], "text_state":binary_data["stdout_text_state"], "output_complete":binary_data["output_complete"]}

    pipefail_tool = scope.tools[4]
    scope.publish_pipeline(pipefail_tool, "set -o pipefail; false | cat")
    pipefail = scope.gateway_ready(pipefail_tool, "")
    assert pipefail_tool in pipefail["tools"], pipefail
    pipefail_result = pipefail["call"]["result"]
    assert pipefail_result.get("isError") is True, pipefail_result
    pipefail_data = pipefail_result["structuredContent"]["data"]
    assert pipefail_data["outcome"] == "exit_nonzero" and pipefail_data["exit_code"] == 1, pipefail_data
    assert base64.b64decode(pipefail_data["stdout_base64"]) == b"", pipefail_data
    assert pipefail_data["output_complete"] is True, pipefail_data
    results["pipefail"] = {"outcome":pipefail_data["outcome"], "exit_code":pipefail_data["exit_code"], "output_complete":pipefail_data["output_complete"]}

    input_tool = scope.tools[5]
    scope.publish_pipeline(input_tool, "cat")
    accepted = scope.gateway_ready(input_tool, "x" * 65_536)
    accepted_data = gateway_data(accepted, input_tool, b"x" * 65_536)
    assert accepted_data["output_complete"] is True, accepted_data
    rejected = scope.gateway(input_tool, "x" * 65_537)
    rejected_result = rejected["call"]["result"]
    assert rejected_result.get("isError") is True, rejected_result
    rejected_body = rejected_result["structuredContent"]
    assert rejected_body["ok"] is False and "maximum limit" in rejected_body["error"], rejected_body
    results["input_bound"] = {"accepted_bytes":65_536, "accepted_stdout_sha256":hashlib.sha256(b"x" * 65_536).hexdigest(), "rejected_bytes":65_537, "rejection":rejected_body["error"]}

    output_tool = scope.tools[6]
    scope.publish_pipeline(output_tool, "head -c 262200 /dev/zero")
    oversized = scope.gateway_ready(output_tool, "")
    oversized_data = gateway_data(oversized, output_tool, b"\0" * 262_144)
    assert oversized_data["stdout_truncated"] is True and oversized_data["output_complete"] is False, oversized_data
    results["output_bound"] = {"captured_bytes":len(base64.b64decode(oversized_data["stdout_base64"])), "stdout_truncated":oversized_data["stdout_truncated"], "output_complete":oversized_data["output_complete"]}

    exercise_gateway_cancel(scope, results, sequence_after_interrupt=True, cancel_after_start=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-receipt", type=pathlib.Path,
                        help="optional observed-build receipt; absent records source identity only")
    parser.add_argument("--source-tree", required=True)
    parser.add_argument("--source-revision", required=True, help="exact 40-character clean Git HEAD used for this candidate")
    parser.add_argument("--marsh", required=True)
    parser.add_argument("--sbx", default="sbx")
    parser.add_argument("--guest-artifacts", required=True)
    parser.add_argument("--evidence", required=True)
    parser.add_argument("--commands", type=pathlib.Path,
                        help="optional isolated commands.json overlay (e.g. staged Kit sources or OCI digests)")
    parser.add_argument("--exercise-restart", action="store_true")
    parser.add_argument("--cancel-only", action="store_true", help="diagnose one stock Gateway cancellation")
    parser.add_argument("--cancel-semicolon", action="store_true", help="with --cancel-only, diagnose whether Brush executes a following command after interrupted sleep")
    parser.add_argument("--cancel-after-start", action="store_true", help="with --cancel-only, wait for a pipeline start marker before sending cancellation")
    parser.add_argument("--core-only", action="store_true", help="deprecated; the core journey is the whole gate")
    parser.add_argument("--real-home", action="store_true", help="deprecated; the login account HOME is always used")
    args = parser.parse_args()
    if (args.cancel_semicolon or args.cancel_after_start) and not args.cancel_only:
        parser.error("cancel diagnostic options require --cancel-only")
    verify_candidate(args, str(pathlib.Path(args.marsh).resolve(strict=True)))
    if sys.platform != "darwin":
        parser.error("stock Gateway UAT requires macOS")
    source_tree = pathlib.Path(args.source_tree).resolve(strict=True)
    from run import source_identity

    try:
        identity = source_identity(source_tree, args.source_revision)
    except (ValueError, subprocess.CalledProcessError) as error:
        parser.error(str(error))
    guest_artifacts = pathlib.Path(args.guest_artifacts).resolve(strict=True)
    guest_sha256 = {
        str(path): sha256(path)
        for path in (
            (guest_artifacts / name).resolve(strict=True)
            for name in ("marsh-linux-arm64", "marsh-worker-linux-arm64", "marsh-relay-linux-arm64")
        )
    }
    evidence = pathlib.Path(args.evidence).resolve()
    report_path = evidence / "mcp-gateway-uat.json"
    host_only_path(report_path, source_tree).unlink(missing_ok=True)
    scope = Scope(args)
    report: dict = {"schema":"marsh.mcp-gateway-uat/v1", "mode":"cancel-only" if args.cancel_only else "core", "outcome":"failed", "isolated_root":str(scope.root), "selected_home":str(scope.home), "host_home_mode":"real", "sandbox":scope.sandbox, "tool_names":scope.tools, "source_identity":identity, "binary_sha256":{str(path):sha256(path) for path in (scope.marsh,scope.marshd,scope.mcp)}, "guest_artifact_sha256":guest_sha256, "harness_sha256":sha256(pathlib.Path(__file__))}
    report["verified_build_receipt"] = scope.build_receipt
    report["stock_before"] = scope.stock_before
    try:
        scope.scope_started = True
        report["scope_status"] = scope.status()
        scope.create_sandbox()
        report["sandbox_boot_id_before"] = scope.sandbox_boot_id()
        if args.cancel_only:
            report["gateway_edges"] = {}
            exercise_gateway_cancel(scope, report["gateway_edges"], sequence_after_interrupt=args.cancel_semicolon, cancel_after_start=args.cancel_after_start)
            report["sandbox_boot_id_after"] = scope.sandbox_boot_id()
            assert report["sandbox_boot_id_after"] == report["sandbox_boot_id_before"], "Gateway sandbox restarted during cancellation"
            report["outcome"] = "passed_partial"
            report["qualification"] = "cancellation diagnostic only"
            raise CoreJourneyComplete
        scope.start_publishing_shell(evidence)
        publisher_shells = [shell for shell in scope.status()["shells"] if shell["pid"] == scope.child.pid]
        assert len(publisher_shells) == 1, f"cannot identify attached publishing shell: {publisher_shells}"
        report["publisher_session_id"] = publisher_shells[0]["session_id"]
        first_input = f"mcp_capture_{scope.tag}\n"
        first = scope.gateway_ready(scope.tools[0], first_input)
        first_data = gateway_data(first, scope.tools[0], first_input.upper().encode())
        assert first_data["cleanup_certainty"] == "uncertain", first_data
        assert first_data["host_cleanup_certainty"] == "verified", first_data
        assert any("sandbox_cleanup=uncertain" in block.get("text", "")
                   and "host_process_group_cleanup=verified" in block.get("text", "")
                   for block in first["call"]["result"]["content"]), first["call"]
        report["sandbox_boot_id_at_first_call"] = scope.sandbox_boot_id()
        assert report["sandbox_boot_id_at_first_call"] == report["sandbox_boot_id_before"], "Gateway discovery required a sandbox restart"
        report["core_gateway"] = {"tools":first["tools"],"stdout_base64":first_data["stdout_base64"],"outcome":first_data["outcome"],"exit_code":first_data["exit_code"],"output_complete":first_data["output_complete"],"cleanup_certainty":first_data["cleanup_certainty"],"host_cleanup_certainty":first_data["host_cleanup_certainty"]}
        invalid_name = scope.command("-c", f"mcp publish '-bad' --sandbox {scope.sandbox} -- 'cat'")
        assert invalid_name.returncode != 0 and b"published tool name" in invalid_name.stderr, invalid_name.stderr
        invalid_host_name = scope.command("mcp", "publish", "-bad", "--", "cat")
        assert invalid_host_name.returncode != 0 and b"published tool name" in invalid_host_name.stderr, invalid_host_name.stderr
        assert scope.registration_absent("-bad"), "invalid tool name reached host registration"
        assert scope.child.poll() is None, "publishing shell exited after invalid tool name"
        report["leading_dash_name_rejected"] = True
        ephemeral = scope.command(
            "--ephemeral-home", "-c",
            f"mcp publish {scope.tools[0]} --sandbox {scope.sandbox} -- 'cat | tr a-z A-Z'",
        )
        assert ephemeral.returncode != 0 and b"requires a persistent home" in ephemeral.stderr, ephemeral.stderr
        assert b"teardown was not confirmed" not in ephemeral.stderr, ephemeral.stderr
        assert scope.child.poll() is None, "publishing shell exited during ephemeral publish rejection"
        home_check = scope.command("-c", 'test -d "$HOME" && printf persistent-home-ok')
        assert home_check.returncode == 0 and home_check.stdout == b"persistent-home-ok", home_check.stderr
        report["ephemeral_publish_rejected"] = True
        ephemeral_unpublish = scope.command("--ephemeral-home", "-c", f"mcp unpublish {scope.tools[0]}")
        assert ephemeral_unpublish.returncode != 0 and b"requires a persistent home" in ephemeral_unpublish.stderr, ephemeral_unpublish.stderr
        assert b"teardown was not confirmed" not in ephemeral_unpublish.stderr, ephemeral_unpublish.stderr
        assert scope.child.poll() is None, "publishing shell exited during ephemeral unpublish rejection"
        home_check = scope.command("-c", 'test -d "$HOME" && printf persistent-home-ok')
        assert home_check.returncode == 0 and home_check.stdout == b"persistent-home-ok", home_check.stderr
        report["ephemeral_unpublish_rejected"] = True
        report["publisher_home_survived_ephemeral_shells"] = True
        scope.advance_shell(".uat-private")
        scope.advance_shell("")
        scope.published.remove(scope.tools[0])
        assert scope.registration_absent(scope.tools[0]), "attached-shell unpublish left stock registration"
        report["attached_unpublish_registration_absent"] = True
        if args.exercise_restart:
            scope.publish_pipeline(scope.tools[2], "cat | tr a-z A-Z")
            before_input = f"mcp_before_restart_{scope.tag}\n"
            gateway_data(scope.gateway_ready(scope.tools[2], before_input), scope.tools[2], before_input.upper().encode())
            scope.restart_daemon()
            after_input = f"mcp_after_restart_{scope.tag}\n"
            gateway_data(scope.gateway_ready(scope.tools[2], after_input), scope.tools[2], after_input.upper().encode())
            report["publication_survives_daemon_restart"] = True
        report["gateway_edges"] = {}
        exercise_gateway_edges(scope, report["gateway_edges"])
        report["sandbox_boot_id_after"] = scope.sandbox_boot_id()
        report["outcome"] = "passed"
    except CoreJourneyComplete:
        pass
    except Exception:
        report["failure"] = traceback.format_exc()
    finally:
        report["cleanup_errors"] = scope.cleanup()
        report["stock_after"] = scope.stock_after
        if report["cleanup_errors"]:
            report["outcome"] = "failed"
        report_path.write_text(json.dumps(report,indent=2,sort_keys=True)+"\n",encoding="utf-8")
        report_path.chmod(0o600)
    print(json.dumps({"outcome":report["outcome"],"evidence":str(report_path)}))
    return 0 if report["outcome"] == "passed" else 1

if __name__ == "__main__":
    raise SystemExit(main())
