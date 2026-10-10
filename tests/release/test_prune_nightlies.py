#!/usr/bin/env python3
"""scripts/prune-nightlies.py against a fake Docker Hub, as CI runs it."""
import json
import os
import pathlib
import subprocess
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import parse_qs, urlparse

ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/prune-nightlies.py"


def nightly(n):
    return f"0.1.3-nightly.202610{n:02d}120000.g{n:07x}"


class FakeHub(BaseHTTPRequestHandler):
    tags = {}
    deleted = []
    fail = set()

    def log_message(self, *args):
        pass

    def reply(self, code, body=None):
        raw = b"" if body is None else json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if self.path == "/v2/users/login" and body == {"username": "bot", "password": "secret"}:
            return self.reply(200, {"token": "jwt"})
        self.reply(401, {"message": "bad login"})

    def do_GET(self):
        url = urlparse(self.path)
        query = parse_qs(url.query)
        page, size = int(query.get("page", ["1"])[0]), int(query["page_size"][0])
        if url.path == "/v2/namespaces/acme/repositories":
            items = [{"name": name} for name in ["pix", *self.tags]]
        elif url.path.startswith("/v2/namespaces/acme/repositories/") and url.path.endswith("/tags"):
            repository = url.path.split("/")[5]
            items = [{"name": name} for name in self.tags.get(repository, [])]
        else:
            return self.reply(404, {"message": "not found"})
        chunk = items[(page - 1) * size : page * size]
        more = page * size < len(items)
        nxt = f"http://127.0.0.1:{self.server.server_port}{url.path}?page={page + 1}&page_size={size}" if more else None
        self.reply(200, {"count": len(items), "next": nxt, "results": chunk})

    def do_DELETE(self):
        if self.headers.get("Authorization") != "Bearer jwt":
            return self.reply(401, {"message": "unauthorized"})
        _, _, _, namespace, repository, _, tag, _ = self.path.split("/")
        if (repository, tag) in self.fail:
            return self.reply(500, {"message": "boom"})
        self.deleted.append((repository, tag))
        self.reply(204)


class PruneNightlies(unittest.TestCase):
    def setUp(self):
        FakeHub.deleted = []
        FakeHub.fail = set()
        protected = ["nightly", "latest", "release", "0.1.2", "0.1.3-nightly.bad", "0.1.3-rc.1"]
        FakeHub.tags = {
            "marsh-shell": protected + [nightly(n) for n in range(1, 16)],
            "marsh-pi": protected + [nightly(n) for n in range(1, 6)],
            "other-thing": [nightly(n) for n in range(1, 16)],
        }
        self.server = HTTPServer(("127.0.0.1", 0), FakeHub)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)

    def run_script(self, *extra, env=None):
        environment = {**os.environ, "DOCKERHUB_USERNAME": "bot", "DOCKERHUB_TOKEN": "secret", **(env or {})}
        return subprocess.run(
            [sys.executable, SCRIPT, "--namespace", "acme", "--keep", "10",
             "--api", f"http://127.0.0.1:{self.server.server_port}", *extra],
            capture_output=True, text=True, env=environment)

    def test_dry_run_deletes_nothing(self):
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(FakeHub.deleted, [])
        self.assertIn("would delete 5", result.stdout)

    def test_apply_keeps_the_newest_ten_and_everything_else(self):
        result = self.run_script("--apply")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        # The oldest five of 15 go (across a paginated listing); a repository
        # with only five nightlies, other prefixes and every non-nightly tag stay.
        self.assertEqual(sorted(FakeHub.deleted), sorted(("marsh-shell", nightly(n)) for n in range(1, 6)))

    def test_one_failed_delete_does_not_stop_the_rest_but_fails_the_run(self):
        FakeHub.fail = {("marsh-shell", nightly(2))}
        result = self.run_script("--apply")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(len(FakeHub.deleted), 4)
        self.assertIn("1 failed", result.stdout)

    def test_apply_requires_credentials(self):
        result = self.run_script("--apply", env={"DOCKERHUB_TOKEN": ""})
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(FakeHub.deleted, [])

    def test_refuses_to_keep_fewer_than_three(self):
        result = subprocess.run([sys.executable, SCRIPT, "--namespace", "acme", "--keep", "1", "--apply"],
                                capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
