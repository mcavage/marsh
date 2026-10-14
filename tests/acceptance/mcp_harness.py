"""Published-MCP harness: stock Gateway probes and the MCP publication scope.

Shared by mcp_gateway_uat.py, mcp_load_uat.py, published_codex_uat.py and
tests/check.py. The scope is a harness.IsolatedScope that also owns one stock
sandbox it creates itself (recorded in extra_owned_names).
"""

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
import time
import uuid

from harness import (IsolatedScope, disposable_root, request_authenticated_daemon_shutdown, resolve_executable,
                     scoped_control_home, stable_process_identity, wait_until)
from launch_env import clean_environment
from provenance import remove_owned_stock_vm, stock_baseline, stock_vm_names, verify_candidate


GATEWAY_PROBE = r'''
import base64, json, os, sys, urllib.request
url = os.environ["MCP_GATEWAY_URL"]
name, encoded_input, list_only = sys.argv[1:]
input_text = base64.b64decode(encoded_input[1:]).decode("utf-8")
headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream", "MCP-Protocol-Version": "2025-06-18"}
if os.environ.get("MCP_SENTINEL_TOKEN_NAME"):
    headers["Authorization"] = "Bearer " + os.environ["MCP_SENTINEL_TOKEN_NAME"]
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


def wait_marker(path: pathlib.Path, child: subprocess.Popen[bytes], timeout: float = 120) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.is_file():
            return
        if child.poll() is not None:
            raise AssertionError(f"publishing shell exited before {path.name}: {child.returncode}")
        time.sleep(0.1)
    raise TimeoutError(f"publishing shell did not produce {path.name}")


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


class McpScope(IsolatedScope):
    """An isolated scope plus one harness-created stock sandbox for published MCP tools."""

    def __init__(self, args: argparse.Namespace) -> None:
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
        self.environment = clean_environment()
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
        self.initialize_scope_cleanup()
        self.extra_owned_names.add(self.sandbox)  # created directly by this harness

    def command(self, *args: str, timeout: float = 180) -> subprocess.CompletedProcess[bytes]:
        return subprocess.run([str(self.marsh), *args], cwd=self.project, env=self.environment, stdin=subprocess.DEVNULL, capture_output=True, timeout=timeout)

    def status(self) -> dict:
        result = self.command("status", "--json", timeout=30)
        if result.returncode:
            raise AssertionError(f"isolated status failed: {result.stderr.decode(errors='replace')}")
        document = json.loads(result.stdout)
        self.remember_owned_status(document)
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
            errors.extend(self.cleanup_isolated_scope())
        try:
            self.stock_after = stock_vm_names(self.sbx)
            errors.extend(self.stock_leftover_errors(self.stock_after))
        except Exception as error:
            errors.append(f"independent stock cleanup could not be verified: {error}")
        return errors
