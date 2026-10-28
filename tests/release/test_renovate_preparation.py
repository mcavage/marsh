"""Exercise the workflow's exact-head preparation decisions with real Git refs."""
import os
import json
import pathlib
import subprocess
import tempfile
import textwrap
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
WORKFLOW = ROOT / ".github/workflows/renovate-refresh.yml"
BOT = "refresh-pins@users.noreply.github.com"


def step_script(step, workflow=WORKFLOW):
    tail = workflow.read_text().split(f"id: {step}\n", 1)[1]
    body = tail.split("        run: |\n", 1)[1]
    lines = []
    for line in body.splitlines():
        if line and not line.startswith("          "):
            break
        lines.append(line)
    return textwrap.dedent("\n".join(lines))


class Preparation(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = pathlib.Path(self.temp.name)
        self.work = self.root / "work"
        self.work.mkdir()
        self.env = {"PATH": os.defpath, "HOME": str(self.root), "LANG": "C.UTF-8",
                    "GIT_CONFIG_NOSYSTEM": "1"}
        self.git("init", "-q", "-b", "renovate/fixture")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "human@example.invalid")
        subprocess.run(["git", "init", "--bare", "-q", str(self.root / "origin.git")],
                       env=self.env, check=True, capture_output=True)
        self.git("remote", "add", "origin", str(self.root / "origin.git"))
        self.bin = self.root / "bin"
        self.bin.mkdir()
        gh = self.bin / "gh"
        gh.write_text('#!/bin/sh\nif [ -n "${GH_CALLS:-}" ]; then printf "%s\\n" "$@" >> "$GH_CALLS"; fi\nprintf "%s" "$VERDICT"\nexit "${API_FAILURE:-0}"\n')
        gh.chmod(0o755)
        self.env.update(PATH=f"{self.bin}:{os.defpath}", BOT_EMAIL=BOT,
                        GITHUB_REPOSITORY="fixture/marsh", BRANCH="renovate/fixture",
                        GITHUB_OUTPUT=str(self.root / "output"), VERDICT="")

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.work, env=self.env,
                              check=True, capture_output=True, text=True).stdout.strip()

    def commit(self, email="human@example.invalid"):
        self.git("-c", f"user.email={email}", "commit", "-q", "--allow-empty", "-m", "fixture")
        self.git("push", "-q", "origin", "HEAD:renovate/fixture")
        return self.git("rev-parse", "HEAD")

    def inspect(self, expected, verdict="", api_failure="0"):
        self.env.update(EXPECTED_SHA=expected, VERDICT=verdict, API_FAILURE=api_failure)
        done = subprocess.run(["bash", "-e", "-o", "pipefail", "-c", step_script("inspect")],
                              cwd=self.work, env=self.env, capture_output=True, text=True)
        output = self.root / "output"
        return done, dict(line.split("=", 1) for line in output.read_text().splitlines()) if output.exists() else {}

    def test_raw_head_requires_preparation_even_if_old_verdict_is_green(self):
        done, output = self.inspect(self.commit(), "success")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(output, {"required": "true"})

    def test_generated_head_reuses_completed_license_verdict(self):
        sha = self.commit(BOT)
        for verdict in ("success", "failure"):
            with self.subTest(verdict=verdict):
                done, output = self.inspect(sha, verdict)
                self.assertEqual(done.returncode, 0, done.stderr)
                self.assertEqual(output, {"required": "false", "ready": "true"})

    def test_missing_or_errored_verdict_is_repaired_even_on_generated_head(self):
        sha = self.commit(BOT)
        for verdict in ("", "error", "pending"):
            with self.subTest(verdict=verdict):
                done, output = self.inspect(sha, verdict)
                self.assertEqual(done.returncode, 0, done.stderr)
                self.assertEqual(output["required"], "true")

    def test_superseded_head_never_prepares_or_validates(self):
        old = self.commit()
        self.commit(BOT)
        self.git("checkout", "-q", old)
        done, output = self.inspect(old, api_failure="1")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(output, {"required": "false", "ready": "false"})

    def test_github_failure_cannot_mark_records_ready(self):
        done, output = self.inspect(self.commit(BOT), api_failure="1")
        self.assertNotEqual(done.returncode, 0)
        self.assertNotIn("ready", output)

    def test_validation_runs_only_on_the_prepared_revision(self):
        for already, generated, expected, ready in (
                ("true", "", "a", "true"), ("", "a", "a", "true"),
                ("", "b", "a", "false"), ("false", "", "a", "false")):
            with self.subTest(already=already, generated=generated):
                output = self.root / "ready"
                output.write_text("")
                env = dict(self.env, GITHUB_OUTPUT=str(output), ALREADY_READY=already,
                           GENERATED_SHA=generated, EXPECTED_SHA=expected)
                subprocess.run(["bash", "-e", "-c", step_script("ready")], env=env,
                               check=True, capture_output=True)
                self.assertEqual(output.read_text().strip(), f"ready={ready}")


    def test_validation_status_covers_failure_skip_and_supersession(self):
        workflow = ROOT / ".github/workflows/ci.yml"
        for preparation, ready, linux, expected in (
                ("success", "true", "success", "success"),
                ("success", "true", "skipped", "success"),
                ("success", "true", "failure", "failure"),
                ("failure", "", "skipped", "failure"),
                ("success", "", "skipped", "failure"),
                ("success", "false", "skipped", "superseded")):
            with self.subTest(preparation=preparation, ready=ready, linux=linux):
                calls = self.root / "gh-calls"
                calls.unlink(missing_ok=True)
                results = {"prepare": {"result": preparation, "outputs": {"ready": ready}},
                           "linux": {"result": linux}, "docs": {"result": "success"}}
                env = dict(self.env, GH_CALLS=str(calls), SHA="abc", RESULTS=json.dumps(results),
                           GITHUB_SERVER_URL="https://example.invalid", GITHUB_RUN_ID="1")
                done = subprocess.run(["bash", "-e", "-c", step_script("verdict", workflow)],
                                      env=env, capture_output=True, text=True)
                if expected == "superseded":
                    self.assertEqual(done.returncode, 0, done.stderr)
                    self.assertFalse(calls.exists())
                else:
                    self.assertEqual(done.returncode == 0, expected == "success", done.stderr)
                    self.assertIn(f"state={expected}", calls.read_text().splitlines())


if __name__ == "__main__":
    unittest.main()
