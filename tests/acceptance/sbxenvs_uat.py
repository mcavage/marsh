#!/usr/bin/env python3
"""Home-level native MCP/mount additions through real marsh and stock SBX."""
from __future__ import annotations

import base64
import json
import pathlib
import shlex
import sys
import uuid

from harness import ScenarioSuite, require, scenario_parser, suite_main
from mcp_harness import GATEWAY_PROBE
from provenance import stock_baseline, stock_vm_inventory

SCENARIOS = {
    "E01-per-kit-mounts": "mounts",
    "E02-child-inherits-only-parent": "nested",
    "E03-split-omits-extra-mounts": "split",
    "E04-mcp-and-worker-reset": "mcp_reset",
    "E05-snapshot-and-restart": "snapshot",
    "E06-invalid-config-no-vm-effects": "invalid",
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

    def snapshot(self):
        """Live edits do not change authority; a stopped/restarted daemon applies them."""
        (self.private / "before-restart").write_text("preserved")
        self.fixture_document["additionalWorkspaces"].append({"path": str(self.changed), "readOnly": True})
        self.write(self.layers / "fixture.yaml", self.fixture_document)
        result, _ = self.job(["project-kind", str(self.changed / "sentinel")])
        require(result.stdout == b"missing\n", "editing a live environment changed its authority")
        stopped = self.cli(["stop"], self.project)
        require(stopped.returncode == 0, f"stop before restart: {stopped.stderr!r}")
        result, receipt = self.job(["project-read", str(self.changed / "sentinel")])
        require(result.stdout == b"new-data" and self.targets(receipt).get(str(self.changed)) == "read_only", "restart did not apply the new snapshot")
        require((self.private / "before-restart").read_text() == "preserved", "restart lost external data")

    def invalid(self):
        """Unsupported fields and managed-source overlaps cannot change stock VMs."""
        require(self.cli(["stop"], self.project).returncode == 0, "cannot stop before refusal probes")
        original = (self.home / "sbxenv.yaml").read_text()
        original_kit = (self.layers / "fixture.yaml").read_text()
        baseline = stock_vm_inventory(self.sbx)
        try:
            for fields in ({"agent": "shell"}, {"additionalWorkspaces": [{"path": str(self.project)}]},
                           {"additionalWorkspaces": [{"path": str(self.guest_home)}]}):
                self.write(self.home / "sbxenv.yaml", fields)
                result = self.brush("true", self.project)
                require(result.returncode != 0, f"unsafe declaration admitted: {fields}")
                require(stock_vm_inventory(self.sbx) == baseline, "refusal created or changed a VM")
            (self.home / "sbxenv.yaml").write_text(original)
            self.write(self.layers / "fixture.yaml", {"additionalWorkspaces": [{"path": str(self.project)}]})
            result = self.cli(["--load", "fixture", "-c", "true"], self.project)
            require(result.returncode != 0, "unsafe Kit-only mount admitted during prewarm")
            require(stock_vm_inventory(self.sbx) == baseline, "bad prewarm mount created a shell or Kit VM")
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
            inspected = self.run([self.sbx, "mcp", "inspect", self.server, "--json"], check=False)
            if inspected.returncode == 0:
                value = json.loads(inspected.stdout)
                expected = [str(pathlib.Path(sys.executable).resolve()), str(pathlib.Path(__file__).resolve().with_name("mcp-fixture.py")), self.tool]
                require(value.get("command") == expected, "refusing to remove a registration not created by this suite")
                removed = self.run([self.sbx, "mcp", "rm", "--force", self.server], check=False)
                require(removed.returncode == 0, f"fixture registration cleanup failed: {removed.stderr!r}")
        except Exception as caught:
            error = RuntimeError(f"fixture registration cleanup: {caught}; {error}")
        return super().finish(error)


if __name__ == "__main__":
    raise SystemExit(suite_main(Environments, scenario_parser(__doc__, SCENARIOS, preflight=False,
                                                           kit_help="published acceptance fixture Kit")))
