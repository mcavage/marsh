#!/usr/bin/env python3
"""scripts/refresh-agent-checksums.py against fake publishers, as Renovate runs it."""
import json
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer

ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/refresh-agent-checksums.py"


def sha(n):
    return f"{n:064x}"


class Publisher(BaseHTTPRequestHandler):
    manifest_version = "2.0.0"
    drop_asset = None

    def log_message(self, *args):
        pass

    def reply(self, code, body):
        raw = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        if self.path == "/claude/2.0.0/manifest.json":
            return self.reply(200, {"version": self.manifest_version, "platforms": {
                "darwin-arm64": {"checksum": sha(9)},
                "linux-arm64": {"checksum": sha(1)},
                "linux-x64": {"checksum": sha(2)}}})
        if self.path == "/github/repos/openai/codex/releases/tags/rust-v3.0.0":
            names = [f"{b}-{t}.tar.gz" for b in ("codex", "codex-code-mode-host", "codex-app-server", "bwrap")
                     for t in ("aarch64-unknown-linux-musl", "x86_64-unknown-linux-musl")]
            return self.reply(200, {"assets": [
                {"name": n, "digest": f"sha256:{sha(100 + i)}"} for i, n in enumerate(names) if n != self.drop_asset]})
        self.reply(404, {"message": "not found"})


class RefreshChecksums(unittest.TestCase):
    def setUp(self):
        Publisher.manifest_version = "2.0.0"
        Publisher.drop_asset = None
        self.server = HTTPServer(("127.0.0.1", 0), Publisher)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.root = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root)
        for kit, descriptor, version in (("marsh-claude", "claude.yaml", "2.0.0"), ("marsh-codex", "codex.yaml", "3.0.0")):
            directory = self.root / "kits" / kit
            directory.mkdir(parents=True)
            source = (ROOT / "kits" / kit / descriptor).read_text()
            rewritten, count = re.subn(r'(?m)^(  version:\n    default: )"[^"]*"', rf'\g<1>"{version}"', source)
            if count != 1:
                raise AssertionError(f"kits/{kit}/{descriptor}: no args.version.default to rewrite")
            (directory / descriptor).write_text(rewritten)
            shutil.copy(ROOT / "kits" / kit / "release-checksums.txt", directory)

    def run_script(self, *extra):
        port = self.server.server_port
        return subprocess.run(
            [sys.executable, SCRIPT, "--root", self.root, "--claude-base", f"http://127.0.0.1:{port}/claude",
             "--github-api", f"http://127.0.0.1:{port}/github", *extra], capture_output=True, text=True)

    def test_rewrites_stale_files_in_the_committed_format_and_is_idempotent(self):
        self.assertEqual(self.run_script("--check").returncode, 1)
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        claude = (self.root / "kits/marsh-claude/release-checksums.txt").read_text()
        self.assertEqual(claude, "# Publisher metadata: " f"http://127.0.0.1:{self.server.server_port}/claude/2.0.0/manifest.json\n"
                         f"{sha(1)}  2.0.0/linux-arm64/claude\n{sha(2)}  2.0.0/linux-x64/claude\n")
        codex = (self.root / "kits/marsh-codex/release-checksums.txt").read_text().splitlines()
        self.assertEqual([line.split("  ")[1] for line in codex[1:]], [
            "3.0.0/codex-aarch64-unknown-linux-musl.tar.gz", "3.0.0/codex-x86_64-unknown-linux-musl.tar.gz",
            "3.0.0/codex-code-mode-host-aarch64-unknown-linux-musl.tar.gz",
            "3.0.0/codex-code-mode-host-x86_64-unknown-linux-musl.tar.gz"])
        self.assertEqual(self.run_script("--check").returncode, 0)

    def test_refuses_a_manifest_for_another_version(self):
        Publisher.manifest_version = "2.0.1"
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("describes '2.0.1'", result.stderr)

    def test_refuses_a_release_missing_a_binary_digest(self):
        Publisher.drop_asset = "codex-code-mode-host-x86_64-unknown-linux-musl.tar.gz"
        result = self.run_script()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no sha256 digest for codex-code-mode-host-x86_64", result.stderr)


if __name__ == "__main__":
    unittest.main()
