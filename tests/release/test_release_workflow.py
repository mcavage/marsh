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
import textwrap
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
    text = re.sub(r"\b(needs|github|steps|secrets|inputs)\.[\w.-]+", lambda match: repr(context.get(match.group(0))),
                  text)
    text = text.replace("&&", " and ").replace("||", " or ").replace("format(", "fmt(")
    if re.search(r"!(?!=)|\$", text):
        raise AssertionError(f"unsupported expression: {expression}")
    return eval(text, {"__builtins__": {}}, {"True": True, "fmt": str.format})  # noqa: S307 - our own workflow text


def group(template, context):
    """A concurrency group template with each ${{ }} evaluated."""
    def value(match):
        result = evaluate(match.group(1), context)
        return "" if result is None else str(result)
    return re.sub(r"\$\{\{(.*?)\}\}", value, template)


def find_step(workflow, job, name):
    """A workflow step, found by its id or name."""
    for step in load(workflow)["jobs"][job]["steps"]:
        if name in (step.get("id"), step.get("name")):
            return step
    raise AssertionError(f"no step {name} in {workflow} job {job}")


def step_script(workflow, job, name):
    """The run: script of a workflow step, found by its id or name."""
    step = find_step(workflow, job, name)
    assert "${{" not in step["run"], f"{workflow} {job} {name} must take its inputs from env"
    return step["run"]


def step_env(workflow, job, name, context):
    """The environment the workflow gives a step: the job's env, then the step's,
    each value evaluated against context (a missing name is empty, as in Actions).
    Running a script with this, not with hand-picked values, tests the bindings too."""
    env = {}
    for scope in (load(workflow)["jobs"][job].get("env", {}), find_step(workflow, job, name).get("env", {})):
        env.update({key: group(str(value), context) for key, value in scope.items()})
    return env


def shell_argv(workflow, job, name):
    """How Actions runs a step's script on Linux: an explicit `shell: bash` is
    bash --noprofile --norc -eo pipefail; no shell is bash -e (no pipefail)."""
    shell = find_step(workflow, job, name).get("shell")
    if shell is None:
        return ["bash", "-e"]
    assert shell == "bash", f"{workflow} {job} {name}: shell {shell}"
    return ["bash", "--noprofile", "--norc", "-eo", "pipefail"]


def run_step_script(workflow, job, name, cwd, env):
    """Run a step's script the way Actions runs it, with the given env."""
    path = pathlib.Path(cwd) / ".step.sh"
    path.write_text(step_script(workflow, job, name))
    return subprocess.run([*shell_argv(workflow, job, name), str(path)], cwd=cwd,
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

    def test_every_freshness_check_has_the_script_checked_out_first(self):
        for name in ("images", "release", "tap"):
            steps = self.jobs[name]["steps"]
            uses = next(i for i, step in enumerate(steps) if "scripts/newest-release.sh" in str(step.get("run", "")))
            checkouts = [step for step in steps[:uses] if str(step.get("uses", "")).startswith("actions/checkout@")]
            self.assertEqual(len(checkouts), 1, name)
            checkout = checkouts[0].get("with", {})
            self.assertEqual(checkout.get("ref"), "${{ needs.version.outputs.sha }}", name)
            self.assertIn(checkout.get("sparse-checkout", "scripts/newest-release.sh"), ("scripts/newest-release.sh",), name)

    def test_a_rehearsal_makes_no_github_release(self):
        for name in ("release", "tap"):
            condition = self.jobs[name]["if"]
            self.assertTrue(evaluate(condition, {"needs.version.outputs.draft": "false"}))
            self.assertFalse(evaluate(condition, {"needs.version.outputs.draft": "true"}))

    def test_a_call_checks_out_the_requested_commit_in_every_ci_job(self):
        call = self.ci["on"]["workflow_call"]["inputs"]
        self.assertEqual(call["all"]["type"], "boolean")
        self.assertEqual(call["ref"]["type"], "string")
        for name, job in self.ci["jobs"].items():
            for step in job.get("steps", []):
                if str(step.get("uses", "")).startswith("actions/checkout@"):
                    self.assertEqual(step.get("with", {}).get("ref"), "${{ inputs.ref }}", f"ci.yml job {name}")

    def test_kit_promotion_checks_every_rust_lane_by_its_ci_name(self):
        step = next(step for step in self.jobs["version"]["steps"] if step.get("id") == "v")["run"]
        listed = re.search(r"rust_lanes=\$'([^']*)'", step).group(1).split("\\n")
        self.assertEqual(sorted(listed), RUST_LANES)
        self.assertGreaterEqual(len(RUST_LANES), 6)

    def test_ci_passed_counts_every_lane(self):
        jobs = self.ci["jobs"]
        self.assertEqual(sorted(needs(jobs["passed"])), sorted(set(jobs) - {"changes", "passed"}))

    def test_a_called_ci_run_has_its_own_concurrency_group(self):
        group = self.ci["concurrency"]["group"]
        self.assertIn("inputs.ref && format('release-{0}', github.run_id)", group)


def rust_lanes():
    """The ci.yml jobs that a push selects with the code or host lane: the Rust tests."""
    return sorted(job["name"] for job in load("ci.yml")["jobs"].values()
                  if re.search(r"needs\.changes\.outputs\.(code|host)\b", str(job.get("if", ""))))


RUST_LANES = rust_lanes() if yaml else []


PUBLISHING_SECRETS = ("DOCKERHUB_TOKEN", "HOMEBREW_TAP_TOKEN")


@unittest.skipIf(yaml is None, "needs PyYAML (make test-tools runs this under the publisher venv)")
class PublishingSecrets(unittest.TestCase):
    def test_only_release_environment_jobs_name_a_publishing_secret(self):
        seen = 0
        for path in sorted(WORKFLOWS.glob("*.yml")):
            data = load(path.name)
            self.assertNotRegex(yaml.safe_dump(data.get("env", {})), r"secrets\.", path.name)
            for name, job in data["jobs"].items():
                text = yaml.safe_dump(job)
                used = [secret for secret in PUBLISHING_SECRETS if re.search(rf"secrets\.{secret}\b", text)]
                if not used:
                    continue
                seen += 1
                environment = job.get("environment")
                self.assertIsNotNone(environment, f"{path.name} job {name} reads {used} outside an environment")
                # Every trigger of the workflow gets the release environment,
                # except a pull request, which gets none.
                for event in data["on"]:
                    got = group(str(environment), {"github.event_name": event})
                    want = "" if event == "pull_request" else "release"
                    self.assertEqual(got, want, f"{path.name} job {name} on {event}")
        self.assertGreaterEqual(seen, 6)

    def test_pruning_uses_the_release_environments_token(self):
        data = load("prune-nightlies.yml")
        job = data["jobs"]["prune"]
        self.assertEqual(job.get("environment"), "release")
        text = yaml.safe_dump(job)
        self.assertRegex(text, r"secrets\.DOCKERHUB_TOKEN\b")
        self.assertNotIn("DOCKERHUB_PRUNE_TOKEN", text)

    def test_ci_never_reads_a_publishing_secret(self):
        text = (WORKFLOWS / "ci.yml").read_text()
        for secret in PUBLISHING_SECRETS:
            self.assertIsNone(re.search(rf"secrets\.{secret}\b", text), f"ci.yml reads {secret}")


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
        # Only a per-commit run asks the GitHub API, for its CI run's jobs:
        # this gh prints what the step's --jq makes of them (name TAB
        # conclusion), from FAKE_CI_JOBS. Any other call fails.
        bin_dir = self.tmp / "bin"
        bin_dir.mkdir()
        (bin_dir / "gh").write_text(textwrap.dedent("""\
            #!/bin/sh
            case "$*" in
              *"/actions/runs/4242/attempts/3/jobs"*) [ -n "${FAKE_CI_JOBS:-}" ] && cat "$FAKE_CI_JOBS" && exit 0 ;;
            esac
            echo "unexpected gh call: $*" >&2
            exit 99
            """))
        (bin_dir / "gh").chmod(0o755)
        self.path = f"{bin_dir}{os.pathsep}{os.environ['PATH']}"

    def run_step(self, commit, event, ref, version="", ci_jobs=None):
        git(self.work, "checkout", "-q", "--detach", commit)
        output = self.tmp / "output"
        output.write_text("")
        context = {"github.event.inputs.version": version, "github.token": "unused"}
        if ci_jobs is not None:
            context.update({"github.event.workflow_run.id": "4242", "github.event.workflow_run.run_attempt": "3"})
        env = {"PATH": self.path, "HOME": str(self.tmp), "GIT_CONFIG_GLOBAL": os.devnull,
               "GIT_CONFIG_SYSTEM": os.devnull, "GITHUB_EVENT_NAME": event, "GITHUB_REF": ref,
               "GITHUB_REF_NAME": ref.rsplit("/", 1)[-1], "GITHUB_REPOSITORY": "example/marsh",
               "GITHUB_OUTPUT": str(output), "GITHUB_STEP_SUMMARY": str(self.tmp / "summary"),
               "RUNNER_TEMP": str(self.tmp), **step_env("release.yml", "version", "v", context)}
        if ci_jobs is not None:
            jobs = self.tmp / "ci-jobs.tsv"
            jobs.write_text("".join(f"{name}\t{conclusion}\n" for name, conclusion in ci_jobs.items()))
            env["FAKE_CI_JOBS"] = str(jobs)
        result = subprocess.run([*shell_argv("release.yml", "version", "v"), str(self.script)],
                                cwd=self.work, env=env, capture_output=True, text=True, timeout=60)
        outputs = dict(line.split("=", 1) for line in output.read_text().splitlines() if "=" in line)
        return result, outputs

    def test_a_plain_tag_on_main_starts_a_stable_release(self):
        git(self.work, "tag", "v1.2.3", self.main)
        result, out = self.run_step(self.main, "push", "refs/tags/v1.2.3")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((out["channel"], out["version"], out["draft"], out["stages"], out["sha"]),
                         ("stable", "1.2.3", "false", "all", self.main))
        self.assertEqual((out["image_tag"], out["extra_tags"]), ("", "1.2.3 latest"))

    def test_a_tag_off_main_is_refused(self):
        result, out = self.run_step(self.side, "push", "refs/tags/v1.2.3")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("is not on main", result.stdout + result.stderr)
        self.assertNotIn("stages", out)

    def test_a_tag_that_is_not_a_version_is_refused(self):
        result, _ = self.run_step(self.main, "push", "refs/tags/v1.2.3.4")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("bad version", result.stdout + result.stderr)

    def test_a_manual_run_off_main_is_refused(self):
        for version in ("", "1.3.0"):
            result, out = self.run_step(self.side, "workflow_dispatch", "refs/heads/side", version)
            self.assertNotEqual(result.returncode, 0, version)
            self.assertIn("on main only", result.stdout + result.stderr)
            self.assertNotIn("stages", out)

    def test_a_rehearsal_of_a_commit_no_longer_on_main_is_refused(self):
        # The ref is main, but the checked-out commit is not on it (main was
        # force-pushed after the run started).
        result, out = self.run_step(self.side, "workflow_dispatch", "refs/heads/main", "1.3.0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("is not on main", result.stdout + result.stderr)
        self.assertNotIn("stages", out)

    def test_a_manual_run_on_main_rehearses_or_publishes_a_nightly(self):
        result, out = self.run_step(self.main, "workflow_dispatch", "refs/heads/main", "1.3.0")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((out["channel"], out["version"], out["draft"], out["image_tag"], out["extra_tags"]),
                         ("stable", "1.3.0", "true", "draft", ""))
        result, out = self.run_step(self.main, "workflow_dispatch", "refs/heads/main")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((out["channel"], out["draft"], out["stages"]), ("nightly", "false", "all"))
        self.assertRegex(out["version"], rf"^1\.2\.3-nightly\.\d{{14}}\.g{self.main[:7]}$")

    def ci_run(self, **overrides):
        """A push CI run's jobs, by name: every lane ran and passed, then overrides."""
        jobs = {job.get("name", key): "success" for key, job in load("ci.yml")["jobs"].items()}
        jobs.update(overrides)
        return jobs

    def per_commit(self, jobs):
        result, out = self.run_step(self.main, "workflow_run", "refs/heads/main", ci_jobs=jobs)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result, out

    def test_a_per_commit_run_moves_the_stable_kit_tag_only_after_the_rust_lanes_passed(self):
        _, out = self.per_commit(self.ci_run())
        self.assertEqual((out["stages"], out["channel"], out["promote_kits"]), ("images", "nightly", "true"))
        for lane in RUST_LANES:
            # Only success counts: skipped, failed, cancelled, still running
            # (no conclusion yet) or missing from the listing all keep :latest.
            for conclusion in ("skipped", "failure", "cancelled", "", None):
                jobs = self.ci_run(**{lane: conclusion})
                if conclusion is None:
                    del jobs[lane]
                result, out = self.per_commit(jobs)
                self.assertEqual((out["stages"], out["promote_kits"]), ("images", "false"), (lane, conclusion))
                self.assertIn("the stable Kit tag stays", result.stdout)

    def test_a_per_commit_run_without_an_image_build_publishes_nothing(self):
        result, out = self.per_commit(self.ci_run(**{"Kit and shell images (build, no push)": "skipped"}))
        self.assertEqual(out, {"stages": "none"})


NIGHTLY_TAG = "v1.2.5-nightly.20261101000000.gabcdef0"


@unittest.skipIf(yaml is None, "needs PyYAML (make test-tools runs this under the publisher venv)")
class Publication(unittest.TestCase):
    """The images, release and tap jobs decide when they run whether this version
    may move :latest, :release, :repaired, the GitHub "Latest" mark or the
    formula. A re-run of failed jobs reuses the version job's outputs, so these
    tests give each step the outputs of an older tag's version job (newest at
    the time) and the repository's tags as they are now."""

    def setUp(self):
        self.tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        self.tags = self.tmp / "tags"
        # The tag listing's failure mode: "" (works), "fail" (an API error
        # before any output) or "partial" (a later page fails after the
        # first lines were printed, as gh --paginate does).
        self.gh_mode = self.tmp / "gh-mode"
        self.gh_mode.write_text("")
        self.gh_log = self.tmp / "gh.log"
        self.gh_log.write_text("")
        # Call N of the listing uses tags.N and gh-mode.N when they exist.
        self.gh_calls = self.tmp / "gh-calls"
        bin_dir = self.tmp / "bin"
        bin_dir.mkdir()
        # gh: the tag listing (what --jq '.[].ref' prints) and release create.
        (bin_dir / "gh").write_text(textwrap.dedent(f"""\
            #!/bin/sh
            printf '%s\\n' "$*" >> "{self.gh_log}"
            [ -n "${{GH_TOKEN:-}}" ] || {{ echo "gh: GH_TOKEN is not set" >&2; exit 4; }}
            case "$*" in
              "api --paginate repos/example/marsh/git/matching-refs/tags/v --jq .[].ref")
                n=$(( $(cat "{self.gh_calls}" 2>/dev/null || echo 0) + 1 )); echo "$n" > "{self.gh_calls}"
                tags="{self.tags}"; [ -f "$tags.$n" ] && tags="$tags.$n"
                mode="{self.gh_mode}"; [ -f "$mode.$n" ] && mode="$mode.$n"
                case "$(cat "$mode")" in
                  fail) echo "gh: HTTP 503: rate limited" >&2; exit 1 ;;
                  partial) sed '$d; s#^#refs/tags/#' "$tags"; echo "gh: HTTP 502 on page 2" >&2; exit 1 ;;
                esac
                sed 's#^#refs/tags/#' "$tags"; exit 0 ;;
              "release create "*) exit 0 ;;
            esac
            echo "unexpected gh call: $*" >&2
            exit 99
            """))
        (bin_dir / "gh").chmod(0o755)
        self.path = f"{bin_dir}{os.pathsep}{os.environ['PATH']}"

    def workspace(self, name):
        """A job's workspace with what its checkout provides."""
        work = pathlib.Path(tempfile.mkdtemp(prefix=f"{name}-", dir=self.tmp))
        (work / "scripts").mkdir()
        shutil.copy(ROOT / "scripts/newest-release.sh", work / "scripts/newest-release.sh")
        return work

    def outputs(self, version):
        """The version job's outputs for stable vVERSION, from when it ran."""
        return {"needs.version.outputs.version": version, "needs.version.outputs.channel": "stable",
                "needs.version.outputs.draft": "false", "needs.version.outputs.formula": "marsh",
                "needs.version.outputs.image_tag": "", "needs.version.outputs.extra_tags": f"{version} latest",
                "needs.version.outputs.sha": "a" * 40, "github.token": "token",
                "secrets.GITHUB_TOKEN": "token", "secrets.HOMEBREW_TAP_TOKEN": "token"}

    def run_job_step(self, job, step, version, work, ok=True, **extra):
        # No waiting between the script's retries here (the default is tested below).
        env = {**step_env("release.yml", job, step, self.outputs(version)), "PATH": self.path,
               "GITHUB_REPOSITORY": "example/marsh", "NEWEST_RELEASE_DELAYS": "0 0 0 0", **extra}
        result = run_step_script("release.yml", job, step, work, env)
        if ok:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def images(self, version, *tags):
        """The images job's moving tags, and the env each publishing step then gets."""
        self.tags.write_text("".join(f"{tag}\n" for tag in tags))
        work = self.workspace(f"images-{version}")
        output = work / "output"
        output.write_text("")
        self.run_job_step("images", "fresh", version, work, GITHUB_OUTPUT=str(output))
        fresh = dict(line.split("=", 1) for line in output.read_text().splitlines())
        context = {f"steps.fresh.outputs.{key}": value for key, value in fresh.items()}
        steps = {name: step_env("release.yml", "images", name, context)
                 for name in ("Shell VM template", "Kits", "Acceptance fixture Kit", "Tag every image with this version")}
        return fresh, steps

    def test_an_older_tag_re_run_after_a_newer_release_pushes_its_version_tag_only(self):
        fresh, steps = self.images("1.2.3", "v1.2.2", "v1.2.3", "v1.2.4", NIGHTLY_TAG)
        self.assertEqual(fresh, {"image_tag": "1.2.3", "extra_tags": "1.2.3"})
        # Every publishing step gets the narrowed tags, not the version job's.
        for name in ("Shell VM template", "Kits", "Acceptance fixture Kit"):
            self.assertEqual(steps[name]["IMAGE_TAG"], "1.2.3", name)
        self.assertEqual(steps["Tag every image with this version"]["EXTRA_TAGS"], "1.2.3")

    def test_the_newest_tag_moves_every_channel_whatever_the_nightly_tags(self):
        # A nightly tag ranks above its base version under sort -V; it is not a release.
        fresh, steps = self.images("1.2.4", "v1.2.3", "v1.2.4", NIGHTLY_TAG)
        self.assertEqual(fresh, {"image_tag": "", "extra_tags": "1.2.4 latest"})
        self.assertEqual(steps["Kits"]["IMAGE_TAG"], "")
        self.assertEqual(steps["Tag every image with this version"]["EXTRA_TAGS"], "1.2.4 latest")

    def test_versions_compare_by_number(self):
        self.assertEqual(self.images("1.2.10", "v1.2.9", "v1.2.10")[0]["extra_tags"], "1.2.10 latest")
        self.assertEqual(self.images("1.2.9", "v1.2.9", "v1.2.10")[0]["extra_tags"], "1.2.9")

    def release(self, version, *tags):
        self.tags.write_text("".join(f"{tag}\n" for tag in tags))
        self.gh_log.write_text("")
        self.run_job_step("release", "Create the GitHub Release", version, self.workspace(f"release-{version}"))
        return next(line for line in self.gh_log.read_text().splitlines() if line.startswith("release create")).split()

    # The older 1.2.3 re-run after 1.2.4, with the listing broken three ways.
    # Each must fail its step and move nothing: answering "newest" here would
    # put 1.2.3 back on every channel.
    BROKEN_LISTINGS = (("fail", ("v1.2.2", "v1.2.3", "v1.2.4")),
                       ("partial", ("v1.2.2", "v1.2.3", "v1.2.4")),  # the page with v1.2.4 fails
                       ("", ("v1.2.2", "v1.2.4")))  # lists no v1.2.3: truncated, or the tag was deleted

    def listings(self):
        return int(self.gh_calls.read_text()) if self.gh_calls.exists() else 0

    def test_a_broken_tag_listing_fails_the_images_step_and_pushes_nothing(self):
        for mode, tags in self.BROKEN_LISTINGS:
            self.gh_mode.write_text(mode)
            self.gh_calls.unlink(missing_ok=True)
            self.tags.write_text("".join(f"{tag}\n" for tag in tags))
            work = self.workspace(f"images-broken-{mode or 'missing'}")
            output = work / "output"
            output.write_text("")
            result = self.run_job_step("images", "fresh", "1.2.3", work, ok=False, GITHUB_OUTPUT=str(output))
            self.assertNotEqual(result.returncode, 0, (mode, result.stdout))
            self.assertEqual(output.read_text(), "", mode)
            # Five tries, then the step fails.
            self.assertEqual(self.listings(), 5, mode)

    def test_a_tag_that_shows_up_on_a_later_try_is_found(self):
        # Right after the push the listing may not show the new tag yet, or
        # a call may fail; a later try sees it.
        for first in ("missing", "fail"):
            self.gh_calls.unlink(missing_ok=True)
            self.tags.write_text("v1.2.3\nv1.2.4\n")
            pathlib.Path(f"{self.tags}.1").write_text("v1.2.3\n")
            pathlib.Path(f"{self.gh_mode}.1").write_text("fail" if first == "fail" else "")
            work = self.workspace(f"images-late-{first}")
            output = work / "output"
            output.write_text("")
            self.run_job_step("images", "fresh", "1.2.4", work, GITHUB_OUTPUT=str(output))
            self.assertEqual(output.read_text().split(), ["image_tag=", "extra_tags=1.2.4", "latest"], first)
            self.assertEqual(self.listings(), 2, first)

    def test_the_listing_is_tried_five_times_over_about_a_minute_by_default(self):
        bin_dir = self.tmp / "bin"
        sleeps = self.tmp / "sleeps"
        (bin_dir / "sleep").write_text(f'#!/bin/sh\necho "$1" >> "{sleeps}"\n')
        (bin_dir / "sleep").chmod(0o755)
        self.tags.write_text("v1.2.2\n")
        result = subprocess.run(["sh", str(ROOT / "scripts/newest-release.sh"), "--github", "1.2.3"],
                                env={"PATH": self.path, "GITHUB_REPOSITORY": "example/marsh", "GH_TOKEN": "token"},
                                capture_output=True, text=True, timeout=60)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assertEqual(self.listings(), 5)
        self.assertEqual(sleeps.read_text().split(), ["4", "8", "16", "32"])

    def test_a_broken_tag_listing_fails_the_release_step_before_any_release(self):
        for mode, tags in self.BROKEN_LISTINGS:
            self.gh_mode.write_text(mode)
            self.gh_calls.unlink(missing_ok=True)
            self.tags.write_text("".join(f"{tag}\n" for tag in tags))
            self.gh_log.write_text("")
            result = self.run_job_step("release", "Create the GitHub Release", "1.2.3",
                                       self.workspace(f"release-broken-{mode or 'missing'}"), ok=False)
            self.assertNotEqual(result.returncode, 0, (mode, result.stdout))
            self.assertNotIn("release create", self.gh_log.read_text(), mode)

    def test_a_broken_tag_listing_fails_the_tap_step_before_the_tap(self):
        self.make_tap()
        for mode, tags in self.BROKEN_LISTINGS:
            self.gh_mode.write_text(mode)
            result = self.tap("1.2.3", *tags, ok=False)
            self.assertNotEqual(result.returncode, 0, (mode, result.stdout))
            listing = subprocess.run(["git", "--git-dir", str(self.tap_repo), "ls-tree", "-r", "--name-only", "main"],
                                     capture_output=True, text=True, check=True).stdout
            self.assertNotIn("Formula/marsh.rb", listing, mode)

    def test_only_the_newest_version_is_marked_latest(self):
        self.assertIn("--latest", self.release("1.2.4", "v1.2.3", "v1.2.4", NIGHTLY_TAG))
        args = self.release("1.2.3", "v1.2.3", "v1.2.4", NIGHTLY_TAG)
        self.assertIn("--latest=false", args)
        self.assertNotIn("--latest", args)
        self.assertIn("--latest", self.release("1.2.10", "v1.2.9", "v1.2.10"))

    def tap(self, version, *tags, ok=True):
        self.tags.write_text("".join(f"{tag}\n" for tag in tags))
        work = self.workspace(f"tap-{version}")
        (work / "dist").mkdir()
        (work / "dist/marsh.rb").write_text(
            "class Marsh < Formula\n"
            f'  url "https://github.com/mcavage/marsh/releases/download/v{version}/marsh-{version}-darwin-arm64.tar.gz"\n'
            "end\n")
        return self.run_job_step("tap", "Update mcavage/homebrew-tap", version, work, ok=ok,
                                 GIT_CONFIG_GLOBAL=str(self.gitconfig))

    def tap_version(self):
        formula = subprocess.run(["git", "--git-dir", str(self.tap_repo), "show", "main:Formula/marsh.rb"],
                                 capture_output=True, text=True, check=True).stdout
        return re.search(r"/download/v([^/]+)/", formula).group(1)

    def make_tap(self):
        # A local bare repository standing in for mcavage/homebrew-tap.
        self.tap_repo = self.tmp / "tap.git"
        git(self.tmp, "init", "-q", "--bare", "-b", "main", str(self.tap_repo))
        seed = self.tmp / "seed"
        git(self.tmp, "clone", "-q", str(self.tap_repo), str(seed))
        (seed / "README").write_text("tap\n")
        git(seed, "add", "README")
        git(seed, "commit", "-q", "-m", "seed")
        git(seed, "push", "-q", "origin", "HEAD:main")
        self.gitconfig = self.tmp / "gitconfig"
        self.gitconfig.write_text(f'[url "{self.tap_repo}"]\n'
                                  "\tinsteadOf = https://x-access-token:token@github.com/mcavage/homebrew-tap.git\n"
                                  "[advice]\n\tdetachedHead = false\n")

    def test_an_older_release_re_run_last_leaves_the_newer_formula(self):
        self.make_tap()
        self.tap("1.2.4", "v1.2.3", "v1.2.4", NIGHTLY_TAG)
        self.assertEqual(self.tap_version(), "1.2.4")
        result = self.tap("1.2.3", "v1.2.3", "v1.2.4", NIGHTLY_TAG)
        self.assertIn("formula stays", result.stdout)
        self.assertEqual(self.tap_version(), "1.2.4")

    def test_the_tap_never_moves_back_even_when_the_tags_disagree(self):
        # The tag listing alone would allow 1.2.3 (say the newer tag was
        # deleted); the tap's own formula still refuses the step back.
        self.make_tap()
        self.tap("1.2.10", "v1.2.9", "v1.2.10")
        result = self.tap("1.2.9", "v1.2.9")
        self.assertIn("newer than 1.2.9", result.stdout)
        self.assertEqual(self.tap_version(), "1.2.10")
        self.tap("1.2.11", "v1.2.11")
        self.assertEqual(self.tap_version(), "1.2.11")


@unittest.skipIf(yaml is None, "needs PyYAML (make test-tools runs this under the publisher venv)")
class FullRun(unittest.TestCase):
    """ci.yml's own scripts: with all, every lane is selected and a skipped lane fails the run."""

    def setUp(self):
        self.tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)

    def classify(self, all_lanes):
        repo = self.tmp / f"repo-{all_lanes}"
        repo.mkdir()
        git(repo, "init", "-q", "-b", "main")
        (repo / "scripts").mkdir()
        shutil.copy(ROOT / "scripts/ci-changes.sh", repo / "scripts/ci-changes.sh")
        git(repo, "add", ".")
        git(repo, "commit", "-q", "-m", "one")
        before = git(repo, "rev-parse", "HEAD")
        (repo / "docs").mkdir()
        (repo / "docs/guide.md").write_text("docs\n")
        git(repo, "add", ".")
        git(repo, "commit", "-q", "-m", "docs only")
        output = self.tmp / f"output-{all_lanes}"
        output.write_text("")
        result = run_step_script("ci.yml", "changes", "classify", repo, {
            "GITHUB_EVENT_NAME": "push", "GITHUB_OUTPUT": str(output), "BEFORE": before, "BASE_REF": "",
            "ALL": all_lanes})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return dict(line.split("=", 1) for line in output.read_text().split())

    def test_a_full_run_selects_every_lane_whatever_the_diff(self):
        lanes = self.classify("true")
        self.assertEqual(set(lanes.values()), {"true"}, lanes)
        # The same docs-only push without all selects no build lane.
        self.assertEqual(self.classify("")["code"], "false")

    def passed(self, results, all_lanes):
        return run_step_script("ci.yml", "passed", "Every lane passed or was skipped", self.tmp,
                        {"RESULTS": results, "ALL": all_lanes}).returncode

    def test_a_skipped_lane_fails_only_a_full_run(self):
        self.assertEqual(self.passed("success success", "true"), 0)
        self.assertNotEqual(self.passed("success skipped", "true"), 0)
        self.assertEqual(self.passed("success skipped", ""), 0)
        self.assertEqual(self.passed("success skipped", "false"), 0)
        for bad in ("failure", "cancelled"):
            for all_lanes in ("true", ""):
                self.assertNotEqual(self.passed(f"success {bad}", all_lanes), 0, (bad, all_lanes))


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
