"""Black-box stdio protocol tests for marsh-mcp export-only MCP mode (Cut A).

Verifies that in export-only mode:
- tools/list publishes ONLY the single explicitly declared tool.
- Development-control tools (shell_run, qualify, doctor, scope_*) are completely absent.
- Calling unpublished or dev tools fails closed.
- Caller-supplied unknown fields are rejected before job creation (MCP-01).
- Option injection (arguments starting with '-') is rejected before job creation (MCP-02).
- Missing/unregistered command fails closed (MCP-03).
- Fixed canonical workspace pinning is enforced.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
from binary_preflight import required_binary

MCP_BINARY = required_binary("MARSH_MCP_BIN")
from jsonrpc_peer import JsonRpcPeer


class ExportMcpProtocolTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory(prefix="marsh-export-test-")
        # Canonical root: marsh admits only symlink-free project paths.
        root = Path(os.path.realpath(self.tempdir.name))
        self.workspace = root / "workspace"
        self.workspace.mkdir()
        self.home = root / "home"
        self.home.mkdir(mode=0o700)
        self.fake_marsh = root / "marsh"
        self.fake_marsh.write_text("#!/bin/sh\nexit 0\n")
        self.fake_marsh.chmod(0o755)
        self.fake_marshd = root / "marshd"
        self.fake_marshd.write_text("#!/bin/sh\nexit 0\n")
        self.fake_marshd.chmod(0o755)
        self.fake_sbx = root / "sbx"
        self.fake_sbx.write_text("#!/bin/sh\nexit 0\n")
        self.fake_sbx.chmod(0o755)

        self.declaration_path = root / "project-test.tool.json"
        self.declaration = {
            "schema_version": "marsh.published_tool/v1",
            "tool_name": "project_test",
            "description": "Run project test suite and report results",
            "command": "test-report",
            "input_schema": {
                "type": "object",
                "properties": {
                    "filter": {"type": "string", "description": "Test name filter"},
                    "verbose": {"type": "boolean", "description": "Verbose output"},
                    "payload": {"type": "string", "description": "Standard input data"},
                },
                "required": ["filter"],
                "additionalProperties": False,
            },
            "bindings": {
                "argv": [
                    {"type": "literal", "value": "--run"},
                    {"type": "named_option", "option": "--filter", "field": "filter"},
                    {"type": "flag", "option": "--verbose", "field": "verbose"},
                ],
                "stdin": {"field": "payload", "max_bytes": 4096},
            },
            "max_output_bytes": 1024,
            "timeout_ms": 5000,
        }
        self.declaration_path.write_text(json.dumps(self.declaration))
        self.declaration_path.chmod(0o600)

        self.environment = os.environ.copy()
        self.environment["HOME"] = str(root / "host-home")
        Path(self.environment["HOME"]).mkdir()

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def start_export_peer(self, mode: str = "export-serve") -> JsonRpcPeer:
        cmd = [
            str(MCP_BINARY),
            mode,
            "--workspace",
            str(self.workspace),
            "--home",
            str(self.home),
            "--marsh",
            str(self.fake_marsh),
            "--sbx",
            str(self.fake_sbx),
            "--declaration",
            str(self.declaration_path),
        ]
        peer = JsonRpcPeer(cmd, self.environment)
        peer.initialize()
        return peer

    def test_export_requires_explicit_command_home_before_start(self) -> None:
        command = [
            str(MCP_BINARY),
            "export-serve",
            "--workspace", str(self.workspace),
            "--marsh", str(self.fake_marsh),
            "--sbx", str(self.fake_sbx),
            "--declaration", str(self.declaration_path),
        ]
        result = subprocess.run(
            command, env=self.environment, cwd=ROOT,
            capture_output=True, text=True, timeout=5, check=False,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("requires an explicit --home", result.stderr)
        self.assertNotIn("Traceback", result.stderr)

    def test_export_rejects_scope_root_and_full_control(self) -> None:
        base = [
            str(MCP_BINARY), "export-serve", "--workspace", str(self.workspace),
            "--marsh", str(self.fake_marsh), "--sbx", str(self.fake_sbx),
            "--declaration", str(self.declaration_path),
        ]
        for extra, reason in [
            (["--scope-root", str(self.home.parent)], "requires an explicit --home"),
            (["--home", str(self.home), "--allow-full-sbx-control"],
             "cannot enable full SBX control"),
        ]:
            with self.subTest(extra=extra):
                result = subprocess.run(
                    base + extra, env=self.environment, cwd=ROOT,
                    capture_output=True, text=True, timeout=5, check=False,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(reason, result.stderr)

    def test_tools_list_publishes_only_declared_tool_mcp05(self) -> None:
        peer = self.start_export_peer()
        try:
            listed = peer.request("tools/list", {})["result"]["tools"]
            self.assertEqual(len(listed), 1)
            tool = listed[0]
            self.assertEqual(tool["name"], "project_test")
            self.assertEqual(
                tool["description"], "Run project test suite and report results"
            )
            props = tool["inputSchema"]["properties"]
            self.assertIn("filter", props)
            self.assertIn("verbose", props)
            self.assertIn("payload", props)

            # Dev-control tools must be completely absent (MCP-05)
            names = {t["name"] for t in listed}
            for forbidden in [
                "shell_run",
                "qualify",
                "doctor",
                "status",
                "results_list",
                "scope_start",
                "scope_run",
                "scope_status",
                "scope_reset",
                "scope_stop",
                "scope_remove",
                "workers_reset",
                "prewarm",
            ]:
                self.assertNotIn(forbidden, names)
        finally:
            peer.close()

    def test_call_tool_rejects_unpublished_dev_tools_mcp05(self) -> None:
        peer = self.start_export_peer()
        try:
            for forbidden in ["shell_run", "qualify", "doctor", "scope_start"]:
                res = peer.request(
                    "tools/call", {"name": forbidden, "arguments": {}}
                )["result"]
                self.assertTrue(res.get("isError"))
                struct = res.get("structuredContent", {})
                self.assertFalse(struct.get("ok"))
                self.assertIn("export-only server exposes only 'project_test'", struct.get("error", ""))
        finally:
            peer.close()

    def test_call_tool_rejects_unknown_field_mcp01(self) -> None:
        peer = self.start_export_peer()
        try:
            res = peer.request(
                "tools/call",
                {
                    "name": "project_test",
                    "arguments": {
                        "filter": "test_auth",
                        "unpermitted_unknown_arg": "exploit",
                    },
                },
            )["result"]
            self.assertTrue(res.get("isError"))
            struct = res.get("structuredContent", {})
            self.assertFalse(struct.get("ok"))
            self.assertIn("unknown field 'unpermitted_unknown_arg'", struct.get("error", ""))
        finally:
            peer.close()

    def test_call_tool_rejects_option_injection_mcp02(self) -> None:
        peer = self.start_export_peer()
        try:
            res = peer.request(
                "tools/call",
                {
                    "name": "project_test",
                    "arguments": {
                        "filter": "--injected-flag",
                    },
                },
            )["result"]
            self.assertTrue(res.get("isError"))
            struct = res.get("structuredContent", {})
            self.assertFalse(struct.get("ok"))
            self.assertIn("option injection prevented", struct.get("error", ""))
        finally:
            peer.close()

    def test_call_tool_rejects_missing_required_field(self) -> None:
        peer = self.start_export_peer()
        try:
            res = peer.request(
                "tools/call",
                {
                    "name": "project_test",
                    "arguments": {
                        "verbose": True,
                    },
                },
            )["result"]
            self.assertTrue(res.get("isError"))
            struct = res.get("structuredContent", {})
            self.assertFalse(struct.get("ok"))
            self.assertIn("missing required field 'filter'", struct.get("error", ""))
        finally:
            peer.close()

    def test_serve_mode_with_export_flag(self) -> None:
        peer = self.start_export_peer(mode="serve")
        try:
            listed = peer.request("tools/list", {})["result"]["tools"]
            self.assertEqual(len(listed), 1)
            self.assertEqual(listed[0]["name"], "project_test")
        finally:
            peer.close()

    def test_workspace_pinning_mismatch_fails_startup(self) -> None:
        other_workspace = Path(self.tempdir.name) / "other_workspace"
        other_workspace.mkdir()
        pinned_decl = dict(self.declaration)
        pinned_decl["canonical_workspace"] = str(other_workspace.resolve())
        pinned_path = Path(self.tempdir.name) / "pinned.tool.json"
        pinned_path.write_text(json.dumps(pinned_decl))
        pinned_path.chmod(0o600)

        cmd = [
            str(MCP_BINARY),
            "export-serve",
            "--workspace",
            str(self.workspace),
            "--home",
            str(self.home),
            "--marsh",
            str(self.fake_marsh),
            "--sbx",
            str(self.fake_sbx),
            "--declaration",
            str(pinned_path),
        ]
        proc = subprocess.run(
            cmd, env=self.environment, capture_output=True, text=True
        )
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("pinned to workspace", proc.stderr)

    def test_export_rejects_guest_writable_declaration(self) -> None:
        for directory in (self.workspace, self.home):
            with self.subTest(directory=directory):
                declaration_path = directory / "published.json"
                declaration_path.write_text(json.dumps(self.declaration))
                result = subprocess.run(
                    [
                        str(MCP_BINARY), "export-serve", "--workspace", str(self.workspace),
                        "--home", str(self.home), "--marsh", str(self.fake_marsh),
                        "--sbx", str(self.fake_sbx), "--declaration", str(declaration_path),
                    ],
                    env=self.environment, cwd=ROOT, capture_output=True, text=True, timeout=5,
                    check=False,
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("outside the guest-mounted workspace and selected home", result.stderr)

    def test_export_rejects_world_readable_declaration(self) -> None:
        self.declaration_path.chmod(0o644)
        result = subprocess.run(
            [
                str(MCP_BINARY), "export-serve", "--workspace", str(self.workspace),
                "--home", str(self.home), "--marsh", str(self.fake_marsh),
                "--sbx", str(self.fake_sbx), "--declaration", str(self.declaration_path),
            ],
            env=self.environment, cwd=ROOT, capture_output=True, text=True, timeout=5,
            check=False,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("owner-only", result.stderr)

    def test_reject_unsupported_json_schema_keywords(self) -> None:
        decl = dict(self.declaration)
        decl["input_schema"]["properties"]["filter"]["pattern"] = "^[a-z]+$"
        bad_path = Path(self.tempdir.name) / "bad_schema.tool.json"
        bad_path.write_text(json.dumps(decl))
        bad_path.chmod(0o600)

        cmd = [
            str(MCP_BINARY),
            "export-serve",
            "--workspace",
            str(self.workspace),
            "--home",
            str(self.home),
            "--marsh",
            str(self.fake_marsh),
            "--sbx",
            str(self.fake_sbx),
            "--declaration",
            str(bad_path),
        ]
        proc = subprocess.run(
            cmd, env=self.environment, capture_output=True, text=True
        )
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("unsupported JSON Schema keyword 'pattern'", proc.stderr)

    def test_guest_home_registry_cannot_block_export_startup(self) -> None:
        decl = dict(self.declaration)
        decl["kit_identity"] = "local-v3:/expected/different-kit"
        path = Path(self.tempdir.name) / "pinned-kit.tool.json"
        path.write_text(json.dumps(decl))
        path.chmod(0o600)

        for guest_registry in (
            json.dumps({"test-report": "local-v3:/actual/kit"}),
            "{invalid json",
        ):
            with self.subTest(guest_registry=guest_registry):
                (self.home / "commands.json").write_text(guest_registry)
                peer = JsonRpcPeer(
                    [
                        str(MCP_BINARY), "export-serve", "--workspace", str(self.workspace),
                        "--home", str(self.home), "--marsh", str(self.fake_marsh),
                        "--sbx", str(self.fake_sbx), "--declaration", str(path),
                    ],
                    self.environment,
                )
                try:
                    peer.initialize()
                    listed = peer.request("tools/list", {})["result"]["tools"]
                    self.assertEqual([tool["name"] for tool in listed], ["project_test"])
                finally:
                    peer.close()

    def test_pipeline_call_preserves_bytes_status_and_literal_source(self) -> None:
        import base64
        import sys

        pipeline = "printf 'fixed source' | cat"
        declaration = dict(self.declaration)
        declaration.pop("command")
        declaration["pipeline"] = pipeline
        declaration["input_schema"] = {
            "type": "object",
            "properties": {"input": {"type": "string"}},
            "additionalProperties": False,
        }
        declaration["bindings"] = {"stdin": {"field": "input", "max_bytes": 4096}}
        self.declaration_path.write_text(json.dumps(declaration))
        self.fake_marsh.write_text(
            f"#!{sys.executable}\n"
            "import sys\n"
            f"assert sys.argv[1:] == ['-c', {pipeline!r}]\n"
            "data = sys.stdin.buffer.read()\n"
            "sys.stdout.buffer.write(b'\\xff\\x00' + data)\n"
            "sys.stderr.buffer.write(b'\\xfe')\n"
            "sys.exit(7)\n"
        )
        peer = self.start_export_peer()
        try:
            result = peer.request(
                "tools/call", {"name": "project_test", "arguments": {"input": "hello"}}
            )["result"]
            data = result["structuredContent"]["data"]
            self.assertEqual(data["outcome"], "exit_nonzero")
            self.assertEqual(data["exit_code"], 7)
            self.assertEqual(base64.b64decode(data["stdout_base64"]), b"\xff\x00hello")
            self.assertEqual(base64.b64decode(data["stderr_base64"]), b"\xfe")
            self.assertEqual(data["stdout"], "�\x00hello")
            self.assertEqual(data["stdout_text_state"], "lossy")
            self.assertEqual(data["stderr_text_state"], "lossy")
            self.assertTrue(data["output_complete"])
            self.assertIn("exit_nonzero", result["content"][0]["text"])
            self.assertIn("bytes available in structuredContent", result["content"][0]["text"])
        finally:
            peer.close()

    def test_pipeline_frame_bound_explicitly_omits_text_duplicate(self) -> None:
        import base64
        import sys

        declaration = dict(self.declaration)
        declaration.pop("command")
        declaration["pipeline"] = "printf large | cat"
        declaration["input_schema"] = {
            "type": "object", "properties": {}, "additionalProperties": False,
        }
        declaration["bindings"] = {}
        declaration["max_output_bytes"] = 262144
        self.declaration_path.write_text(json.dumps(declaration))
        self.fake_marsh.write_text(
            f"#!{sys.executable}\n"
            "import sys\n"
            "sys.stdout.buffer.write(b'x' * 250000)\n"
            "sys.stderr.buffer.write(b'y' * 250000)\n"
        )
        peer = self.start_export_peer()
        try:
            result = peer.request(
                "tools/call", {"name": "project_test", "arguments": {}}, timeout=15
            )["result"]
            data = result["structuredContent"]["data"]
            self.assertEqual(data["stdout"], "")
            self.assertEqual(data["stderr"], "")
            self.assertEqual(data["stdout_text_state"], "omitted")
            self.assertEqual(data["stderr_text_state"], "omitted")
            self.assertEqual(base64.b64decode(data["stdout_base64"]), b"x" * 250000)
            self.assertEqual(base64.b64decode(data["stderr_base64"]), b"y" * 250000)
            self.assertTrue(data["output_complete"])
        finally:
            peer.close()

    def test_pipeline_text_fallback_is_bounded_and_useful(self) -> None:
        import sys

        declaration = dict(self.declaration)
        declaration.pop("command")
        declaration["pipeline"] = "printf text | cat"
        declaration["input_schema"] = {
            "type": "object", "properties": {"input": {"type": "string"}},
            "additionalProperties": False,
        }
        declaration["bindings"] = {"stdin": {"field": "input", "max_bytes": 8}}
        declaration["max_output_bytes"] = 262144
        self.declaration_path.write_text(json.dumps(declaration))
        self.fake_marsh.write_text(
            f"#!{sys.executable}\n"
            "import sys\n"
            "sys.stdout.write('hello\\n' + 'x' * 20000)\n"
        )
        peer = self.start_export_peer()
        try:
            result = peer.request("tools/call", {
                "name": "project_test", "arguments": {"input": "x"}
            })["result"]
            content = result["content"][0]["text"]
            self.assertIn("hello", content)
            self.assertIn("exit_code=0", content)
            self.assertLess(len(content), 9000)
            self.assertEqual(len(result["structuredContent"]["data"]["stdout"]), 20006)
        finally:
            peer.close()

    def test_loaded_publication_is_revoked_on_remove_or_change(self) -> None:
        peer = self.start_export_peer()
        try:
            self.declaration_path.unlink()
            self.assertEqual(peer.request("tools/list", {})["result"]["tools"], [])
            result = peer.request(
                "tools/call", {"name": "project_test", "arguments": {"filter": "x"}}
            )["result"]
            self.assertTrue(result["isError"])
            self.assertIn("removed or changed", result["structuredContent"]["error"])
            changed = dict(self.declaration)
            changed["command"] = "another-command"
            self.declaration_path.write_text(json.dumps(changed))
            self.assertEqual(peer.request("tools/list", {})["result"]["tools"], [])
            result = peer.request(
                "tools/call", {"name": "project_test", "arguments": {"filter": "x"}}
            )["result"]
            self.assertTrue(result["isError"])
            self.assertIn("removed or changed", result["structuredContent"]["error"])
        finally:
            peer.close()

    def test_pipeline_source_canary_is_not_disclosed(self) -> None:
        canary = "CANARY_PRIVATE_LITERAL_123"
        declaration = dict(self.declaration)
        declaration.pop("command")
        declaration["pipeline"] = f"printf '%s' '{canary}' >/dev/null | cat"
        declaration["input_schema"] = {
            "type": "object",
            "properties": {"input": {"type": "string"}},
            "additionalProperties": False,
        }
        declaration["bindings"] = {"stdin": {"field": "input", "max_bytes": 8}}
        self.declaration_path.write_text(json.dumps(declaration))
        peer = self.start_export_peer()
        try:
            listed = peer.request("tools/list", {})
            result = peer.request(
                "tools/call", {"name": "project_test", "arguments": {"input": "x"}}
            )
            self.assertNotIn(canary, json.dumps(listed))
            self.assertNotIn(canary, json.dumps(result))
        finally:
            peer.close()

    def test_pipeline_input_bound_and_timeout(self) -> None:
        import sys

        declaration = dict(self.declaration)
        declaration.pop("command")
        declaration["pipeline"] = "sleep 30"
        declaration["input_schema"] = {
            "type": "object",
            "properties": {"input": {"type": "string"}},
            "additionalProperties": False,
        }
        declaration["bindings"] = {"stdin": {"field": "input", "max_bytes": 8}}
        declaration["timeout_ms"] = 100
        self.declaration_path.write_text(json.dumps(declaration))
        self.fake_marsh.write_text(
            f"#!{sys.executable}\n"
            "import time\n"
            "time.sleep(30)\n"
        )
        peer = self.start_export_peer()
        try:
            oversized = peer.request(
                "tools/call", {"name": "project_test", "arguments": {"input": "123456789"}}
            )["result"]
            self.assertTrue(oversized["isError"])
            self.assertIn("exceeds configured limit", oversized["structuredContent"]["error"])
            timed_out = peer.request(
                "tools/call", {"name": "project_test", "arguments": {"input": "x"}}, timeout=15
            )["result"]["structuredContent"]["data"]
            self.assertEqual(timed_out["outcome"], "timeout")
            self.assertEqual(timed_out["cleanup_certainty"], "uncertain")
            self.assertFalse(timed_out["output_complete"])
        finally:
            peer.close()


if __name__ == "__main__":
    unittest.main()
