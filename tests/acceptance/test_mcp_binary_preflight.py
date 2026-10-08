"""MCP suite entrypoints must reject missing binaries before creating a scope."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SUITES = ("test_stdio_protocol.py", "test_export_stdio_protocol.py",
          "test_guest_export_boundary.py")
PROBE = r'''
import runpy, sys
from pathlib import Path
sys.path.insert(0, str(Path(sys.argv[1]).parent))
def audit(event, arguments):
    if event in ("subprocess.Popen", "tempfile.mkdtemp", "os.mkdir"):
        raise AssertionError("effect before preflight: " + event)
sys.addaudithook(audit)
values = runpy.run_path(sys.argv[1], run_name="preflight_probe")
print(values.get("MCP_BINARY", values.get("MCP")))
'''


class McpBinaryPreflight(unittest.TestCase):
    def probe(self, suite, selected):
        environment = {"PATH": os.defpath, "PYTHONDONTWRITEBYTECODE": "1"}
        if selected is not None:
            environment["MARSH_MCP_BIN"] = str(selected)
        return subprocess.run(
            [sys.executable, "-B", "-c", PROBE, str(ROOT / "tests/mcp" / suite)],
            env=environment, capture_output=True, text=True, timeout=10,
            start_new_session=True,
        )

    def test_missing_or_invalid_selection_fails_before_effects(self):
        with tempfile.TemporaryDirectory(prefix="marsh-binary-preflight-") as temporary:
            root = Path(temporary)
            plain = root / "not-executable"
            plain.write_text("not an executable\n")
            plain.chmod(0o600)
            cases = ((None, "set MARSH_MCP_BIN"),
                     (root / "absent", "FileNotFoundError"),
                     (root, "executable regular file"),
                     (plain, "executable regular file"))
            for suite in SUITES:
                for selected, diagnostic in cases:
                    with self.subTest(suite=suite, selected=selected):
                        result = self.probe(suite, selected)
                        self.assertNotEqual(result.returncode, 0)
                        self.assertEqual(result.stdout, "")
                        self.assertIn(diagnostic, result.stderr)
                        self.assertNotIn("effect before preflight", result.stderr)

    def test_explicit_executable_is_selected_without_running_it(self):
        # Import/preflight only. This does not qualify Python as an MCP server.
        for suite in SUITES:
            with self.subTest(suite=suite):
                result = self.probe(suite, sys.executable)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), str(Path(sys.executable).resolve()))


if __name__ == "__main__":
    unittest.main()
