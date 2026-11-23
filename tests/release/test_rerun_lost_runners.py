#!/usr/bin/env python3
"""scripts/rerun-lost-runners.py against a fake GitHub API, as the
rerun-lost-runners workflow runs it."""
import json
import os
import pathlib
import subprocess
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import urlparse

ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/rerun-lost-runners.py"
NOT_ACQUIRED = "The job was not acquired by Runner of type hosted even after multiple attempts"
CAPACITY = "Due to capacity constraints, jobs targeting macOS arm64 runners may experience longer queue times."


def job(name, conclusion, runner="GitHub Actions 1000025269", notes=()):
    return {"name": name, "conclusion": conclusion, "runner_name": runner, "notes": list(notes)}


def lost(name):
    """A job GitHub cancelled because no hosted runner took it (as in run 45201529455)."""
    return job(name, "cancelled", runner="", notes=[NOT_ACQUIRED, CAPACITY, CAPACITY])


def ci_attempt(macos):
    return [job("Linux unit", "success"), macos, job("docs", "skipped"),
            job("CI passed", "success" if macos["conclusion"] == "success" else "failure")]


class FakeGitHub(BaseHTTPRequestHandler):
    attempts = []   # every attempt so far: a list of jobs
    pending = []    # the attempts each re-run will produce, in order
    reruns = 0
    queued_polls = 0

    def log_message(self, *args):
        pass

    def reply(self, code, body=None):
        raw = b"" if body is None else json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def jobs(self):
        out = []
        for number, jobs in enumerate(type(self).attempts, 1):
            for index, item in enumerate(jobs):
                out.append((number * 100 + index, number, item))
        return out

    def do_GET(self):
        cls = type(self)
        if self.headers.get("Authorization") != "Bearer token":
            return self.reply(401, {"message": "unauthorized"})
        url = urlparse(self.path)
        parts = url.path.split("/")
        if url.path == "/repos/acme/marsh/actions/runs/42":
            status = "completed"
            if cls.queued_polls:
                cls.queued_polls -= 1
                status = "queued"
            last = cls.attempts[-1]
            failed = any(item["conclusion"] not in ("success", "skipped") for item in last)
            return self.reply(200, {"status": status, "run_attempt": len(cls.attempts),
                                    "conclusion": None if status != "completed" else "failure" if failed else "success"})
        if url.path.startswith("/repos/acme/marsh/actions/runs/42/attempts/") and parts[-1] == "jobs":
            attempt = int(parts[-2])
            query = dict(pair.split("=") for pair in url.query.split("&"))
            page, size = int(query["page"]), 2  # a small page to exercise pagination
            jobs = [{"id": ident, "name": item["name"], "conclusion": item["conclusion"],
                     "runner_name": item["runner_name"]} for ident, number, item in self.jobs() if number == attempt]
            return self.reply(200, {"total_count": len(jobs), "jobs": jobs[(page - 1) * size:page * size]})
        if url.path.startswith("/repos/acme/marsh/check-runs/") and parts[-1] == "annotations":
            ident = int(parts[-2])
            item = next(item for i, _, item in self.jobs() if i == ident)
            return self.reply(200, [{"annotation_level": "failure", "message": note} for note in item["notes"]])
        self.reply(404, {"message": "not found"})

    def do_POST(self):
        cls = type(self)
        if self.path != "/repos/acme/marsh/actions/runs/42/rerun-failed-jobs" or not cls.pending:
            return self.reply(403, {"message": "cannot re-run"})
        cls.reruns += 1
        cls.attempts.append(cls.pending.pop(0))
        cls.queued_polls = 2
        self.reply(201, {})


class RerunLostRunners(unittest.TestCase):
    def setUp(self):
        FakeGitHub.attempts, FakeGitHub.pending, FakeGitHub.reruns, FakeGitHub.queued_polls = [], [], 0, 0
        self.server = HTTPServer(("127.0.0.1", 0), FakeGitHub)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)

    def run_script(self, *extra, attempt=1):
        env = {**os.environ, "GH_TOKEN": "token", "GITHUB_API_URL": f"http://127.0.0.1:{self.server.server_port}"}
        return subprocess.run([sys.executable, SCRIPT, "--repo", "acme/marsh", "--run", "42",
                               "--attempt", str(attempt), "--poll", "0.01", *extra],
                              capture_output=True, text=True, env=env, timeout=60)

    def test_a_lost_macos_runner_is_re_run_and_waited_for(self):
        FakeGitHub.attempts = [ci_attempt(lost("macOS arm64 (MCP and ACP tests)"))]
        FakeGitHub.pending = [ci_attempt(job("macOS arm64 (MCP and ACP tests)", "success"))]
        result = self.run_script("--apply")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(FakeGitHub.reruns, 1)
        self.assertIn("no hosted runner took: macOS arm64 (MCP and ACP tests)", result.stdout)
        self.assertIn("attempt 2: completed success; nothing to do", result.stdout)

    def test_a_release_whose_host_job_is_lost_twice_is_re_run_twice_then_left(self):
        release = lambda host: [job("version", "success"), job("ci / CI passed", "success"), host,
                                job("release", "skipped"), job("tap", "skipped")]
        FakeGitHub.attempts = [release(lost("host"))]
        FakeGitHub.pending = [release(lost("host")), release(lost("host")), release(job("host", "success"))]
        result = self.run_script("--apply", "--max-attempts", "3")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(FakeGitHub.reruns, 2)
        self.assertIn("::warning::run 42 attempt 3: no hosted runner after 3 attempts", result.stdout)

    def test_a_real_failure_is_never_re_run(self):
        # As in run 45254541818 attempt 2: images failed, the rest skipped.
        FakeGitHub.attempts = [[job("images", "failure"), lost("ci / macOS arm64 (clippy)"),
                                job("ci / CI passed", "failure"), job("host", "skipped")]]
        FakeGitHub.pending = [ci_attempt(job("x", "success"))]
        result = self.run_script("--apply")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(FakeGitHub.reruns, 0)
        self.assertIn("not re-run: images ended failure", result.stdout)

    def test_other_cancels_and_timeouts_are_never_re_run(self):
        cases = {
            "a cancel after a runner took it": job("macOS arm64", "cancelled"),
            "a cancel while queued, no annotation": job("macOS arm64", "cancelled", runner="", notes=[CAPACITY]),
            "a timeout": job("macOS arm64", "timed_out"),
        }
        for name, macos in cases.items():
            with self.subTest(name):
                FakeGitHub.attempts, FakeGitHub.reruns = [ci_attempt(macos)], 0
                FakeGitHub.pending = [ci_attempt(job("macOS arm64", "success"))]
                result = self.run_script("--apply")
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(FakeGitHub.reruns, 0)
                self.assertIn("not re-run: macOS arm64 ended", result.stdout)

    def test_only_the_latest_attempt_is_acted_on(self):
        FakeGitHub.attempts = [ci_attempt(lost("macOS arm64")), ci_attempt(lost("macOS arm64"))]
        FakeGitHub.pending = [ci_attempt(job("macOS arm64", "success"))]
        result = self.run_script("--apply", attempt=1)
        self.assertEqual(FakeGitHub.reruns, 0)
        self.assertIn("attempt 2 is newer; leaving it", result.stdout)

    def test_dry_run_re_runs_nothing(self):
        FakeGitHub.attempts = [ci_attempt(lost("macOS arm64"))]
        FakeGitHub.pending = [ci_attempt(job("macOS arm64", "success"))]
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(FakeGitHub.reruns, 0)
        self.assertIn("would re-run its failed jobs (dry run)", result.stdout)

    def test_a_refused_re_run_fails(self):
        FakeGitHub.attempts = [ci_attempt(lost("macOS arm64"))]
        result = self.run_script("--apply")
        self.assertEqual(result.returncode, 1)
        self.assertIn("rerun-failed-jobs", result.stderr)


if __name__ == "__main__":
    unittest.main()
