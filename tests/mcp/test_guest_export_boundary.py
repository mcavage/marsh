"""Linux must reject host-only exporter startup before invoking a product command."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
from binary_preflight import required_binary

MCP = required_binary("MARSH_MCP_BIN")


@unittest.skipUnless(sys.platform.startswith("linux"), "Linux guest boundary only")
class GuestExportBoundary(unittest.TestCase):
    def test_host_export_modes_fail_closed_before_any_product_effect(self):
        with tempfile.TemporaryDirectory(prefix="marsh-guest-export-") as temporary:
            # Canonical root: marsh admits only symlink-free project paths.
            root = Path(os.path.realpath(temporary))
            project = root / "project"
            home = root / "selected-home"
            project.mkdir(mode=0o700)
            home.mkdir(mode=0o700)
            canary = root / "called"
            product = root / "marsh"
            product.write_text(f"#!/bin/sh\nprintf called > '{canary}'\n")
            product.chmod(0o700)
            declaration = root / "declaration.json"
            declaration.write_text("{}")
            declaration.chmod(0o600)
            for mode in ("export-serve", "acp-export-serve"):
                with self.subTest(mode=mode):
                    result = subprocess.run(
                        [str(MCP), mode, "--workspace", str(project), "--home", str(home),
                         "--marsh", str(product), "--sbx", str(product),
                         "--declaration", str(declaration)],
                        input=b"", capture_output=True, timeout=5,
                    )
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(result.stdout, b"")
                    self.assertIn(b"requires the macOS host", result.stderr)
                    self.assertFalse(canary.exists(), "guest mode invoked host control")


if __name__ == "__main__":
    unittest.main()
