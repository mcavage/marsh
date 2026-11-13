#!/usr/bin/env python3
"""The release gate in .github/workflows/release.yml and ci.yml.

GitHub Actions cannot run here, so this checks the two things a broken edit
would silently lose: every build and publish job waits for a full CI run on
the commit it builds, and the version step (run for real, against a scratch
repository) accepts and refuses the right triggers. make test-tools runs it
with the publisher venv, which has PyYAML.
"""
import os
import pathlib
import re
import shutil
import subprocess
import tempfile
import unittest

try:
    import yaml
except ImportError:  # the system interpreter; make test-tools also runs it under the publisher venv
    yaml = None

ROOT = pathlib.Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github/workflows"


def load(name):
    # PyYAML reads the bare `on:` key as True.
    data = yaml.safe_load((WORKFLOWS / name).read_text())
    data["on"] = data.pop(True, data.get("on"))
    return data


def needs(job):
    value = job.get("needs", [])
    return [value] if isinstance(value, str) else list(value)


def evaluate(expression, context):
    """Evaluate the small subset of GitHub expressions the conditions and groups use.

    Python's and/or return an operand, like the expression language's && and
    ||, and '' and None are falsy in both. A context name not given is null.
    """
    text = expression.strip()
    if text.startswith("${{") and text.endswith("}}"):
        text = text[3:-2]
    text = text.replace("!cancelled()", "True")
    text = re.sub(r"\b(needs|github)\.[\w.-]+", lambda match: repr(context.get(match.group(0))), text)
    text = text.replace("&&", " and ").replace("||", " or ").replace("format(", "fmt(")
    if re.search(r"[!$]|\b(inputs|steps)\.", text):
        raise AssertionError(f"unsupported expression: {expression}")
    return eval(text, {"__builtins__": {}}, {"True": True, "fmt": str.format})  # noqa: S307 - our own workflow text


def group(template, context):
    """A concurrency group template with each ${{ }} evaluated."""
    return re.sub(r"\$\{\{(.*?)\}\}", lambda match: str(evaluate(match.group(1), context)), template)


def step_script(workflow, job, name):
    """The run: script of a workflow step, found by its id or name."""
    for step in load(workflow)["jobs"][job]["steps"]:
        if name in (step.get("id"), step.get("name")):
            assert "${{" not in step["run"], f"{workflow} {job} {name} must take its inputs from env"
            return step["run"]
    raise AssertionError(f"no step {name} in {workflow} job {job}")


def run_bash(script, cwd, env):
    """Run a step script the way GitHub runs a bash step."""
    path = pathlib.Path(cwd) / ".step.sh"
    path.write_text(script)
    return subprocess.run(["bash", "--noprofile", "--norc", "-eo", "pipefail", str(path)], cwd=cwd,
                          env={"PATH": os.environ["PATH"], "HOME": str(cwd), "GIT_CONFIG_SYSTEM": os.devnull,
                               "GIT_CONFIG_GLOBAL": os.devnull, **env},
                          capture_output=True, text=True, timeout=60)


def git(cwd, *args):
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True,
                          env={**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_SYSTEM": os.devnull,
                               "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.com",
                               "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.com"}).stdout.strip()


@unittest.skipIf(yaml is None, "needs PyYAML (make test-tools runs this under the publisher venv)")
class ReleaseGate(unittest.TestCase):
    def setUp(self):
        self.release = load("release.yml")
        self.ci = load("ci.yml")
        self.jobs = self.release["jobs"]

    def test_the_ci_job_runs_every_lane_on_the_built_commit(self):
        job = self.jobs["ci"]
        self.assertEqual(job["uses"], "./.github/workflows/ci.yml")
        self.assertIs(job["with"]["all"], True)
        self.assertEqual(job["with"]["ref"], "${{ needs.version.outputs.sha }}")
        self.assertEqual(needs(job), ["version"])
        for stages in ("all", "images", "none"):
            self.assertEqual(evaluate(job["if"], {"needs.version.outputs.stages": stages}), stages == "all")

    def test_nothing_is_built_or_published_before_ci_passes(self):
        def upstream(name, seen=None):
            seen = set() if seen is None else seen
            for parent in needs(self.jobs[name]):
                if parent not in seen:
                    seen.add(parent)
                    upstream(parent, seen)
            return seen
        for name in self.jobs:
            if name not in ("version", "ci"):
                self.assertIn("ci", upstream(name), f"release.yml job {name} does not wait for the ci job")

    def test_no_build_job_condition_runs_past_a_failed_ci(self):
        # GitHub adds success() to a condition without a status function, so
        # a failed or cancelled ci skips the job and everything after it.
        # always(), !cancelled() or failure() would run it anyway; images is
        # the one exception and has its own truth table below.
        for name, job in self.jobs.items():
            if name in ("version", "images"):
                continue
            condition = str(job.get("if", ""))
            self.assertIsNone(re.search(r"\b(always|cancelled|failure)\s*\(", condition), f"{name}: {condition}")
            self.assertNotIn("continue-on-error", job, name)

    def test_only_a_per_commit_run_publishes_images_without_ci(self):
        condition = self.jobs["images"]["if"]
        for stages in ("all", "images", "none"):
            for ci in ("success", "skipped", "failure", "cancelled"):
                context = {"needs.version.result": "success", "needs.version.outputs.stages": stages,
                           "needs.ci.result": ci}
                expected = (stages, ci) in {("all", "success"), ("images", "skipped")}
                self.assertEqual(evaluate(condition, context), expected, context)
        self.assertFalse(evaluate(condition, {"needs.version.result": "skipped", "needs.version.outputs.stages": "",
                                              "needs.ci.result": "skipped"}))

    def test_stable_tags_share_one_queue_that_keeps_every_pending_run(self):
        concurrency = self.release["concurrency"]
        template = concurrency["group"]
        # One run at a time, none cancelled, and every pending run kept (the
        # default queue keeps one pending run and drops the older one).
        self.assertIs(concurrency["cancel-in-progress"], False)
        self.assertEqual(concurrency.get("queue"), "max")
        groups = {
            "tag 1.2.3": group(template, {"github.event_name": "push", "github.ref_name": "v1.2.3"}),
            "tag 1.2.4": group(template, {"github.event_name": "push", "github.ref_name": "v1.2.4"}),
            "rehearsal": group(template, {"github.event_name": "workflow_dispatch",
                                          "github.event.inputs.version": "1.2.5", "github.ref_name": "main"}),
            "manual nightly": group(template, {"github.event_name": "workflow_dispatch",
                                               "github.event.inputs.version": "", "github.ref_name": "main"}),
            "nightly": group(template, {"github.event_name": "schedule", "github.ref_name": "main"}),
            "commit 7": group(template, {"github.event_name": "workflow_run", "github.run_id": 7}),
            "commit 8": group(template, {"github.event_name": "workflow_run", "github.run_id": 8}),
        }
        self.assertEqual(groups["tag 1.2.3"], "release-stable")
        self.assertEqual(groups["tag 1.2.4"], groups["tag 1.2.3"])
        self.assertEqual(groups["manual nightly"], groups["nightly"])
        distinct = [groups[name] for name in ("tag 1.2.3", "rehearsal", "nightly", "commit 7", "commit 8")]
        self.assertEqual(len(set(distinct)), len(distinct), groups)

    def test_a_call_checks_out_the_requested_commit_in_every_ci_job(self):
        call = self.ci["on"]["workflow_call"]["inputs"]
        self.assertEqual(call["all"]["type"], "boolean")
        self.assertEqual(call["ref"]["type"], "string")
        for name, job in self.ci["jobs"].items():
            for step in job.get("steps", []):
                if str(step.get("uses", "")).startswith("actions/checkout@"):
                    self.assertEqual(step.get("with", {}).get("ref"), "${{ inputs.ref }}", f"ci.yml job {name}")

    def test_ci_passed_counts_every_lane(self):
        jobs = self.ci["jobs"]
        self.assertEqual(sorted(needs(jobs["passed"])), sorted(set(jobs) - {"changes", "passed"}))

    def test_a_called_ci_run_has_its_own_concurrency_group(self):
        group = self.ci["concurrency"]["group"]
        self.assertIn("inputs.ref && format('release-{0}', github.run_id)", group)


@unittest.skipIf(yaml is None, "needs PyYAML (make test-tools runs this under the publisher venv)")
class VersionStep(unittest.TestCase):
    """Runs release.yml's version step against a scratch repository."""

    def setUp(self):
        step = next(step for step in load("release.yml")["jobs"]["version"]["steps"] if step.get("id") == "v")
        self.assertNotIn("${{", step["run"], "the step must take its inputs from env")
        self.tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        self.script = self.tmp / "version.sh"
        self.script.write_text(step["run"])
        origin = self.tmp / "origin.git"
        work = self.tmp / "work"
        git(self.tmp, "init", "-q", "--bare", "-b", "main", str(origin))
        git(self.tmp, "clone", "-q", str(origin), str(work))
        git(work, "commit", "-q", "--allow-empty", "-m", "one")
        git(work, "tag", "v1.2.2")
        git(work, "commit", "-q", "--allow-empty", "-m", "two")
        git(work, "push", "-q", "origin", "HEAD:main", "--tags")
        self.main = git(work, "rev-parse", "HEAD")
        git(work, "checkout", "-q", "-b", "side")
        git(work, "commit", "-q", "--allow-empty", "-m", "side")
        self.side = git(work, "rev-parse", "HEAD")
        git(work, "fetch", "-q", "origin")
        self.work = work
        # The paths tested here never ask the GitHub API.
        bin_dir = self.tmp / "bin"
        bin_dir.mkdir()
        (bin_dir / "gh").write_text("#!/bin/sh\necho 'unexpected gh call' >&2\nexit 99\n")
        (bin_dir / "gh").chmod(0o755)
        self.path = f"{bin_dir}{os.pathsep}{os.environ['PATH']}"

    def run_step(self, commit, event, ref, version=""):
        git(self.work, "checkout", "-q", "--detach", commit)
        output = self.tmp / "output"
        output.write_text("")
        env = {"PATH": self.path, "HOME": str(self.tmp), "GIT_CONFIG_GLOBAL": os.devnull,
               "GIT_CONFIG_SYSTEM": os.devnull, "GITHUB_EVENT_NAME": event, "GITHUB_REF": ref,
               "GITHUB_REF_NAME": ref.rsplit("/", 1)[-1], "GITHUB_REPOSITORY": "example/marsh",
               "GITHUB_OUTPUT": str(output), "GITHUB_STEP_SUMMARY": str(self.tmp / "summary"),
               "RUNNER_TEMP": str(self.tmp), "INPUT": version, "CI_RUN_ID": "", "GH_TOKEN": "unused"}
        result = subprocess.run(["bash", "--noprofile", "--norc", "-eo", "pipefail", str(self.script)],
                                cwd=self.work, env=env, capture_output=True, text=True, timeout=60)
        outputs = dict(line.split("=", 1) for line in output.read_text().splitlines() if "=" in line)
        return result, outputs

    def test_a_plain_tag_on_main_starts_a_stable_release(self):
        git(self.work, "tag", "v1.2.3", self.main)
        result, out = self.run_step(self.main, "push", "refs/tags/v1.2.3")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((out["channel"], out["version"], out["draft"], out["stages"], out["sha"]),
                         ("stable", "1.2.3", "false", "all", self.main))
        self.assertEqual(out["extra_tags"], "1.2.3 latest")
        self.assertEqual(out["newest"], "true")

    def test_a_tag_off_main_is_refused(self):
        result, out = self.run_step(self.side, "push", "refs/tags/v1.2.3")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("is not on main", result.stdout + result.stderr)
        self.assertNotIn("stages", out)

    def test_an_older_version_tagged_later_moves_nothing_users_follow(self):
        git(self.work, "tag", "v1.2.4", self.main)
        git(self.work, "tag", "v1.2.3", self.main)
        _, newest = self.run_step(self.main, "push", "refs/tags/v1.2.4")
        self.assertEqual((newest["newest"], newest["image_tag"], newest["extra_tags"]), ("true", "", "1.2.4 latest"))
        result, older = self.run_step(self.main, "push", "refs/tags/v1.2.3")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((older["channel"], older["stages"], older["newest"]), ("stable", "all", "false"))
        # Its images get only its version (no :latest, :release or :repaired).
        self.assertEqual((older["image_tag"], older["extra_tags"]), ("1.2.3", "1.2.3"))
        self.assertIn("older than v1.2.4", result.stdout)

    def test_a_tag_that_is_not_a_version_is_refused(self):
        result, _ = self.run_step(self.main, "push", "refs/tags/v1.2.3.4")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("bad version", result.stdout + result.stderr)


@unittest.skipIf(yaml is None, "needs PyYAML (make test-tools runs this under the publisher venv)")
class Publication(unittest.TestCase):
    """The release and tap steps never move "Latest" or the formula back to an older version."""

    def setUp(self):
        self.tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)

    def release(self, newest, draft="false"):
        log = self.tmp / "gh.log"
        log.write_text("")
        bin_dir = self.tmp / "bin"
        bin_dir.mkdir(exist_ok=True)
        (bin_dir / "gh").write_text(f'#!/bin/sh\nprintf "%s\\n" "$*" >> "{log}"\n'
                                    '[ "$1 $2" = "release create" ]\n')
        (bin_dir / "gh").chmod(0o755)
        result = run_bash(step_script("release.yml", "release", "Create the GitHub Release"), self.tmp, {
            "PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}", "GITHUB_REPOSITORY": "example/marsh",
            "VERSION": "1.2.3", "CHANNEL": "stable", "FORMULA": "marsh", "DRAFT": draft, "NEWEST": newest,
            "SHA": "a" * 40, "GH_TOKEN": "unused"})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return log.read_text().split()

    def test_only_the_newest_version_is_marked_latest(self):
        self.assertIn("--latest", self.release("true"))
        args = self.release("false")
        self.assertIn("--latest=false", args)
        self.assertNotIn("--latest", args)

    def tap(self, version, newest):
        dist = self.tmp / f"run-{version}" / "dist"
        dist.mkdir(parents=True)
        (dist / "marsh.rb").write_text(f'class Marsh < Formula\n  version "{version}"\nend\n')
        result = run_bash(step_script("release.yml", "tap", "Update mcavage/homebrew-tap"), dist.parent, {
            "VERSION": version, "FORMULA": "marsh", "NEWEST": newest, "TAP_TOKEN": "token",
            "GIT_CONFIG_GLOBAL": str(self.gitconfig)})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def test_an_older_release_finishing_last_leaves_the_newer_formula(self):
        # The tap step itself, against a local bare repository standing in for
        # mcavage/homebrew-tap.
        tap = self.tmp / "tap.git"
        git(self.tmp, "init", "-q", "--bare", "-b", "main", str(tap))
        seed = self.tmp / "seed"
        git(self.tmp, "clone", "-q", str(tap), str(seed))
        (seed / "README").write_text("tap\n")
        git(seed, "add", "README")
        git(seed, "commit", "-q", "-m", "seed")
        git(seed, "push", "-q", "origin", "HEAD:main")
        self.gitconfig = self.tmp / "gitconfig"
        self.gitconfig.write_text(f'[url "{tap}"]\n\tinsteadOf = https://x-access-token:token@github.com/mcavage/homebrew-tap.git\n'
                                  '[advice]\n\tdetachedHead = false\n')
        self.tap("1.2.4", "true")
        result = self.tap("1.2.3", "false")
        self.assertIn("formula stays", result.stdout)
        formula = subprocess.run(["git", "--git-dir", str(tap), "show", "main:Formula/marsh.rb"],
                                 capture_output=True, text=True, check=True).stdout
        self.assertIn('version "1.2.4"', formula)


@unittest.skipIf(yaml is None, "needs PyYAML (make test-tools runs this under the publisher venv)")
class ModelCache(unittest.TestCase):
    """A cold TLA+ sweep can outlast the model job; its verdicts must still be kept."""

    def test_verdicts_are_saved_even_when_the_sweep_fails_or_times_out(self):
        job = load("ci.yml")["jobs"]["model"]
        steps = job["steps"]
        uses = [str(step.get("uses", "")).split("@")[0] for step in steps]
        check = next(i for i, step in enumerate(steps) if "make test-model" in str(step.get("run", "")))
        restore, save = uses.index("actions/cache/restore"), uses.index("actions/cache/save")
        self.assertNotIn("actions/cache", uses, "actions/cache saves only after a successful job")
        self.assertLess(restore, check)
        self.assertLess(check, save)
        self.assertEqual(steps[save].get("if"), "always()")
        # The sweep stops before the job's limit, so the save step still runs.
        self.assertLess(steps[check]["timeout-minutes"], job["timeout-minutes"])
        cache = "${{ runner.temp }}/tlc-cache"
        self.assertEqual(steps[check]["env"]["TLC_CACHE"], cache)
        self.assertEqual(steps[restore]["with"]["path"], cache)
        self.assertEqual(steps[save]["with"]["path"], cache)
        # A new key per run (caches are immutable), restored by prefix.
        key = steps[save]["with"]["key"]
        self.assertEqual(steps[restore]["with"]["key"], key)
        self.assertIn("${{ github.run_id }}", key)
        prefixes = steps[restore]["with"]["restore-keys"].splitlines()
        self.assertTrue(all(key.startswith(prefix) for prefix in prefixes), prefixes)
        self.assertIn("tlc-rest-java21-", prefixes)


if __name__ == "__main__":
    unittest.main()
