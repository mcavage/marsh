#!/usr/bin/env python3
"""Home-level native MCP/mount additions through real marsh and stock SBX."""
from __future__ import annotations

import base64
import fcntl
import json
import os
import pathlib
import pty
import select
import shlex
import subprocess
import sys
import termios
import time
import uuid

from harness import ScenarioSuite, require, scenario_parser, suite_main, text
from mcp_harness import GATEWAY_PROBE
from provenance import SharedBaseline, stock_baseline, stock_refusal_errors, stock_vm_inventory

SCENARIOS = {
    "E01-per-kit-mounts": "mounts",
    "E02-child-inherits-only-parent": "nested",
    "E03-split-omits-extra-mounts": "split",
    "E04-mcp-and-worker-reset": "mcp_reset",
    "E05-live-mounts": "live_mounts",
    "E06-live-mcp-add-and-dropin": "live_mcp",
    "E07-invalid-change-keeps-working": "invalid_change",
    "E08-removed-server-unloaded": "removed",
    "E09-invalid-config-no-vm-effects": "invalid",
}


class Environments(ScenarioSuite):
    LABEL = "sbxenvs"
    EVIDENCE = "sbxenvs.json"
    SCENARIOS = SCENARIOS
    ORDER = list(SCENARIOS)

    def __init__(self, arguments):
        super().__init__(arguments)
        self.tag = uuid.uuid4().hex[:12]
        self.server = f"marsh-uat-env-{self.tag}"
        self.tool = f"env_echo_{self.tag}"
        self.shared = self.root / "reference"
        self.private = self.root / "fixture-data"
        self.shell_only = self.root / "shell-data"
        self.changed = self.root / "new-data"
        for directory in (self.shared, self.private, self.shell_only, self.changed):
            directory.mkdir()
            (directory / "sentinel").write_text(directory.name)
        self.layers = self.home / "sbxenvs"
        self.layers.mkdir()
        self.dropins = self.home / "sbxenv.d"
        self.live_server = f"{self.server}-live"
        self.live_tool = f"live_{self.tag}"
        self.drop_server = f"{self.server}-drop"
        self.drop_tool = f"drop_{self.tag}"
        self.write(self.home / "sbxenv.yaml", {"additionalWorkspaces": [{"path": str(self.shared), "readOnly": True}]})
        self.fixture_document = {
            "additionalWorkspaces": [{"path": str(self.private)}],
            "mcp": {"servers": [{"name": self.server, "command": str(pathlib.Path(sys.executable).resolve()),
                                  "args": [str(pathlib.Path(__file__).resolve().with_name("mcp-fixture.py")), self.tool]}]},
        }
        self.write(self.layers / "fixture.yaml", self.fixture_document)
        self.write(self.layers / "shell.yaml", {"additionalWorkspaces": [{"path": str(self.shell_only), "readOnly": True}]})

    @staticmethod
    def write(path, fields):
        # JSON is also native YAML; no optional host YAML package needed.
        path.write_text(json.dumps({"schemaVersion": "1", **fields}) + "\n")

    def job(self, argv):
        before = self.job_ids()
        result = self.brush(shlex.join(["fixture", *argv]), self.project, timeout=900)
        require(result.returncode == 0, f"fixture failed: {result.stderr[-1500:]!r}")
        receipts = self.settle(before, 1)
        self.verify_deleted(receipts)
        return result, receipts[0]

    @staticmethod
    def targets(receipt):
        return {mount["target"]: mount["access"] for mount in receipt["mounts"]}

    def mounts(self):
        """Shared and per-Kit mounts preserve bytes, access modes and receipts."""
        outer = self.brush(f"cat {shlex.quote(str(self.shared / 'sentinel'))}; "
                           f"test -r {shlex.quote(str(self.shell_only / 'sentinel'))} && "
                           f"test ! -e {shlex.quote(str(self.private / 'sentinel'))}", self.project, timeout=900)
        require(outer.returncode == 0 and outer.stdout == b"reference", f"shell additions: {outer.stderr!r}")
        result, receipt = self.job(["project-read", str(self.shared / "sentinel")])
        require(result.stdout == b"reference", "shared bytes were not preserved")
        targets = self.targets(receipt)
        require(targets.get(str(self.shared)) == "read_only", f"missing shared RO receipt: {targets}")
        require(targets.get(str(self.private)) == "read_write", f"missing Kit RW receipt: {targets}")
        require(str(self.shell_only) not in targets, "shell-only mount leaked into fixture")
        self.job(["project-write", str(self.private / "written"), "preserved"])
        require((self.private / "written").read_text() == "preserved", "RW data did not reach the host")
        result = self.brush(shlex.join(["fixture", "project-write", str(self.shared / "sentinel"), "wrong"]), self.project)
        require(result.returncode != 0 and (self.shared / "sentinel").read_text() == "reference", "read-only source was changed")

    def nested(self):
        """A cross-Kit child inherits only the parent's recorded extra mounts."""
        before = self.job_ids()
        child = f"test -r {shlex.quote(str(self.private / 'sentinel'))} && test ! -e {shlex.quote(str(self.shell_only / 'sentinel'))} && printf inherited"
        script = shlex.join(["shell", "-c", child])
        result = self.brush(shlex.join(["fixture", "pipeline", "|", "sh", "-c", script]), self.project, timeout=900)
        require(result.returncode == 0 and result.stdout == b"inherited", f"child view: {result.stdout!r} {result.stderr!r}")
        receipts = self.settle(before, 2)
        parent = next(receipt for receipt in receipts if receipt["command"] == "fixture")
        child = next(receipt for receipt in receipts if receipt["command"] == "shell")
        require(self.targets(child) == self.targets(parent), "child's Kit config widened or lost its parent's mount view")
        self.verify_deleted(receipts)

    def split(self):
        """A split job cannot see any additional mounted directory."""
        before = self.job_ids()
        script = f"test ! -e {shlex.quote(str(self.private / 'sentinel'))} && test ! -e {shlex.quote(str(self.shared / 'sentinel'))} && printf isolated"
        result = self.brush("marsh split ::: isolated " + shlex.join(["fixture", "pipeline", "|", "sh", "-c", script]) + " | marsh join", self.project)
        require(result.returncode == 0 and b"isolated" in result.stdout, f"split bypass: {result.stdout!r} {result.stderr!r}")
        receipts = self.settle(before, 1)
        require(not {str(self.shared), str(self.private), str(self.shell_only)} & set(self.targets(receipts[0])), "split receipt contains extras")
        self.verify_deleted(receipts)

    def gateway(self, name, list_only):
        result, receipt = self.job(["exec", "python3", "-c", GATEWAY_PROBE, name,
                                   "x" + base64.b64encode(b"hello-env").decode(), "1" if list_only else "0"])
        return json.loads(result.stdout), receipt

    def mcp_reset(self):
        """Declared MCP echo works through a real job and after worker recreation."""
        listed, first = self.gateway(self.tool, True)
        name = next((name for name in listed["tools"] if name.endswith(self.tool)), None)
        require(name is not None, f"declared MCP tool missing: {listed}")
        called, _ = self.gateway(name, False)
        require(called["call"]["result"]["content"][0]["text"] == "hello-env", "MCP returned wrong bytes")
        reset = self.cli(["workers", "reset", "fixture"], self.project)
        require(reset.returncode == 0, f"worker reset: {reset.stderr!r}")
        listed, recreated = self.gateway(self.tool, True)
        require(recreated["vm_id"] != first["vm_id"], "reset did not allocate a fresh owned VM")
        name = next((name for name in listed["tools"] if name.endswith(self.tool)), None)
        require(name is not None, "MCP was not restored after VM recreation")
        called, _ = self.gateway(name, False)
        require(called["call"]["result"]["content"][0]["text"] == "hello-env", "restored MCP did not work")

    def server_entry(self, name, tool):
        return {"name": name, "command": str(pathlib.Path(sys.executable).resolve()),
                "args": [str(pathlib.Path(__file__).resolve().with_name("mcp-fixture.py")), tool]}

    def sbxenv_status(self):
        # Not `document`: this suite restarts its daemon, which would look like a change of owner.
        return json.loads(self.run([self.marsh, "status", "--json"]).stdout)["sbxenv"]

    def wait_status(self, condition, message, timeout=30):
        deadline = time.monotonic() + timeout
        while True:
            status = self.sbxenv_status()
            if condition(status):
                return status
            require(time.monotonic() < deadline, f"{message}: {status}")
            time.sleep(0.5)

    def listed(self, tool):
        listed, receipt = self.gateway(tool, True)
        return any(name.endswith(tool) for name in listed["tools"]), receipt

    def live_mounts(self):
        """Editing a Kit's file applies to the next job with no restart; a restart keeps it."""
        (self.private / "before-restart").write_text("preserved")
        result, before = self.job(["project-kind", str(self.changed / "sentinel")])
        require(result.stdout == b"missing\n", "the directory was visible before it was declared")
        self.fixture_document["additionalWorkspaces"].append({"path": str(self.changed), "readOnly": True})
        self.write(self.layers / "fixture.yaml", self.fixture_document)
        result, receipt = self.job(["project-read", str(self.changed / "sentinel")])
        require(result.stdout == b"new-data" and self.targets(receipt).get(str(self.changed)) == "read_only",
                "the next job did not get the live mount")
        require(receipt["vm_id"] == before["vm_id"], "a mount change replaced the Kit VM")
        status = self.sbxenv_status()
        require(status["last_error"] is None and any(f.endswith("fixture.yaml") for f in status["files"]) and status["hash"],
                f"status does not show the active files: {status}")
        stopped = self.cli(["stop"], self.project)
        require(stopped.returncode == 0, f"stop: {stopped.stderr!r}")
        result, receipt = self.job(["project-read", str(self.changed / "sentinel")])
        require(result.stdout == b"new-data", "restart lost the mount")
        require((self.private / "before-restart").read_text() == "preserved", "restart lost external data")

    def live_mcp(self):
        """A server added to sbxenv.yaml, then one in sbxenv.d, reach the warm Kit VM without a restart."""
        found, first = self.listed(self.tool)
        require(found, "declared MCP tool missing before the live change")
        before = self.sbxenv_status()
        shared = {"additionalWorkspaces": [{"path": str(self.shared), "readOnly": True}],
                  "mcp": {"servers": [self.server_entry(self.live_server, self.live_tool)]}}
        self.write(self.home / "sbxenv.yaml", shared)
        found, second = self.listed(self.live_tool)
        require(found, "the next job does not see the server added to sbxenv.yaml")
        require(second["vm_id"] == first["vm_id"], "the live server replaced the warm Kit VM")
        listed, _ = self.gateway(self.live_tool, False)
        name = next(name for name in listed["tools"] if name.endswith(self.live_tool))
        called, _ = self.gateway(name, False)
        require(called["call"]["result"]["content"][0]["text"] == "hello-env", "live server returned wrong bytes")
        self.dropins.mkdir(exist_ok=True)
        self.write(self.dropins / "narwhal.yaml", {"mcp": {"servers": [self.server_entry(self.drop_server, self.drop_tool)]}})
        found, third = self.listed(self.drop_tool)
        require(found and third["vm_id"] == first["vm_id"], "the sbxenv.d server did not reach the warm Kit VM")
        found, _ = self.listed(self.live_tool)
        require(found, "adding a drop-in lost the sbxenv.yaml server")
        status = self.sbxenv_status()
        require(status["hash"] != before["hash"] and any(f.endswith("sbxenv.d/narwhal.yaml") for f in status["files"])
                and status["last_error"] is None, f"status after live changes: {status}")

    def pty_output(self, script):
        master, slave = pty.openpty()

        def controlling():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        process = subprocess.Popen([self.marsh, "-c", script], cwd=self.project, env=self.host_env, stdin=slave,
                                   stdout=slave, stderr=slave, preexec_fn=controlling)
        os.close(slave)
        output, deadline = bytearray(), time.monotonic() + 240
        try:
            while time.monotonic() < deadline:
                ready, _, _ = select.select([master], [], [], 0.5)
                if ready:
                    try:
                        chunk = os.read(master, 65536)
                    except OSError:
                        break
                    if not chunk:
                        break
                    output.extend(chunk)
                elif process.poll() is not None:
                    break
        finally:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=30)
            os.close(master)
        return text(bytes(output))

    def invalid_change(self):
        """A broken file keeps the last good configuration, reports itself and breaks no job."""
        good = (self.dropins / "narwhal.yaml").read_text()
        (self.dropins / "narwhal.yaml").write_text(json.dumps({"schemaVersion": "1", "bogus": True}) + "\n")
        status = self.wait_status(lambda value: value["last_error"], "the watcher did not report the broken file")
        require("narwhal.yaml" in status["last_error"]["message"], f"error does not name the file: {status}")
        found, _ = self.listed(self.drop_tool)
        require(found, "the last good configuration was dropped")
        result, receipt = self.job(["project-read", str(self.changed / "sentinel")])
        require(result.stdout == b"new-data", "a job broke while the file was invalid")
        shown = self.pty_output("true")
        require("sbxenv change ignored" in shown, f"no notice in the attached shell: {shown[-400:]!r}")
        (self.dropins / "narwhal.yaml").write_text(good)
        self.wait_status(lambda value: value["last_error"] is None, "the error did not clear after the fix")

    def removed(self):
        """A server taken out of the files is unloaded from the idle Kit VM; the others stay."""
        (self.dropins / "narwhal.yaml").unlink()
        self.wait_status(lambda value: not any(f.endswith("narwhal.yaml") for f in value["files"]), "removal not noticed")
        deadline = time.monotonic() + 30
        while True:
            found, _ = self.listed(self.drop_tool)
            if not found:
                break
            require(time.monotonic() < deadline, "the removed server is still loaded in the idle Kit VM")
            time.sleep(1)
        found, _ = self.listed(self.live_tool)
        require(found, "removing one server unloaded another")
        found, _ = self.listed(self.tool)
        require(found, "removing a drop-in unloaded the Kit's own server")

    def invalid(self):
        """Unsupported fields and managed-source overlaps cannot change stock VMs."""
        require(self.cli(["stop"], self.project).returncode == 0, "cannot stop before refusal probes")
        original = (self.home / "sbxenv.yaml").read_text()
        original_kit = (self.layers / "fixture.yaml").read_text()
        baseline = stock_vm_inventory(self.sbx)

        def unchanged(message):
            errors = stock_refusal_errors(baseline, stock_vm_inventory(self.sbx), self.owned_stock_names(),
                                          shared=isinstance(self.stock_before, SharedBaseline))
            require(not errors, f"{message}: {'; '.join(errors)}")

        try:
            for fields in ({"agent": "shell"}, {"additionalWorkspaces": [{"path": str(self.project)}]},
                           {"additionalWorkspaces": [{"path": str(self.guest_home)}]}):
                self.write(self.home / "sbxenv.yaml", fields)
                result = self.brush("true", self.project)
                require(result.returncode != 0, f"unsafe declaration admitted: {fields}")
                unchanged("refusal created or changed a VM")
            (self.home / "sbxenv.yaml").write_text(original)
            self.write(self.layers / "fixture.yaml", {"additionalWorkspaces": [{"path": str(self.project)}]})
            result = self.cli(["--load", "fixture", "-c", "true"], self.project)
            require(result.returncode != 0, "unsafe Kit-only mount admitted during prewarm")
            unchanged("bad prewarm mount created a shell or Kit VM")
        finally:
            (self.home / "sbxenv.yaml").write_text(original)
            (self.layers / "fixture.yaml").write_text(original_kit)

    def run_all(self):
        self.stock_before = stock_baseline(self.sbx)
        self.source["stock_before"] = self.stock_before
        (self.project / "README.md").write_text("environment acceptance\n")
        self.git(self.project, "init", "-q", "-b", "main")
        self.git(self.project, "add", "README.md")
        self.git(self.project, "commit", "-q", "-m", "seed")
        self.run_scenarios()

    def finish(self, error):
        try:
            for server, tool in ((self.server, self.tool), (self.live_server, self.live_tool), (self.drop_server, self.drop_tool)):
                inspected = self.run([self.sbx, "mcp", "inspect", server, "--json"], check=False)
                if inspected.returncode == 0:
                    value = json.loads(inspected.stdout)
                    expected = [str(pathlib.Path(sys.executable).resolve()), str(pathlib.Path(__file__).resolve().with_name("mcp-fixture.py")), tool]
                    require(value.get("command") == expected, "refusing to remove a registration not created by this suite")
                    removed = self.run([self.sbx, "mcp", "rm", "--force", server], check=False)
                    require(removed.returncode == 0, f"fixture registration cleanup failed: {removed.stderr!r}")
        except Exception as caught:
            error = RuntimeError(f"fixture registration cleanup: {caught}; {error}")
        return super().finish(error)


if __name__ == "__main__":
    raise SystemExit(suite_main(Environments, scenario_parser(__doc__, SCENARIOS, preflight=False,
                                                           kit_help="published acceptance fixture Kit")))
