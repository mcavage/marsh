"""scripts/cut-release.py: the checks, the regression verdict, and the tag it makes."""
import contextlib
import importlib.util
import io
import pathlib
import subprocess
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[2] / "scripts/cut-release.py"
spec = importlib.util.spec_from_file_location("cut_release", SCRIPT)
cut = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cut)


def git(cwd, *argv):
    return subprocess.run(["git", *argv], cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()


def passing(sha, suites=3):
    return {"tier": "regress", "ok": True, "source_revision": sha, "wall_seconds": 5400,
            "suites": [{"name": f"s{i}", "status": "PASS"} for i in range(suites)]}


class CutRelease(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="marsh-cut-release-")
        self.addCleanup(self.temporary.cleanup)
        base = pathlib.Path(self.temporary.name)
        self.origin, self.work = base / "origin.git", base / "work"
        git(base, "init", "-q", "--bare", "-b", "main", str(self.origin))
        git(base, "clone", "-q", str(self.origin), str(self.work))
        for key, value in (("user.email", "t@t"), ("user.name", "t"), ("commit.gpgsign", "false"),
                           ("tag.gpgsign", "false")):
            git(self.work, "config", key, value)
        (self.work / "a").write_text("a")
        git(self.work, "add", "-A")
        git(self.work, "commit", "-qm", "one")
        git(self.work, "branch", "-M", "main")
        git(self.work, "push", "-q", "-u", "origin", "main")
        self.sha = git(self.work, "rev-parse", "HEAD")
        original = cut.ROOT
        cut.ROOT = self.work
        self.addCleanup(setattr, cut, "ROOT", original)

    def release(self, *argv, summary=None, ci=lambda sha: None, answer="y", after=lambda: None):
        calls = []

        def regress():
            calls.append("regress")
            after()
            return summary if summary is not None else passing(self.sha)

        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            code = cut.release(list(argv), machine=lambda: None, ci=ci, run_regress=regress, ask=lambda prompt: answer)
        return code, calls

    def test_a_release_runs_the_regression_then_makes_an_annotated_tag_that_records_it(self):
        code, calls = self.release("1.2.3", "--yes")
        self.assertEqual((code, calls), (0, ["regress"]))
        self.assertEqual(git(self.work, "cat-file", "-t", "refs/tags/v1.2.3"), "tag")
        body = git(self.work, "tag", "-l", "--format=%(contents)", "v1.2.3")
        self.assertIn(f"Regression: passed on {self.sha[:7]} (3 suites, 90 min)", body.splitlines())
        self.assertEqual(git(self.work, "rev-list", "-n1", "v1.2.3"), self.sha)
        self.assertIn("refs/tags/v1.2.3", git(self.work, "ls-remote", "--tags", "origin"))

    def test_a_bad_version_is_refused_before_anything_runs(self):
        for version in ("1.2", "v1.2.3", "1.2.3-rc1", "1.2.3.4"):
            code, calls = self.release(version, "--yes")
            self.assertEqual((code, calls), (1, []), version)

    def test_a_dirty_tree_off_main_or_out_of_sync_is_refused(self):
        (self.work / "stray").write_text("x")
        self.assertEqual(self.release("1.2.3", "--yes"), (1, []))
        (self.work / "stray").unlink()
        git(self.work, "checkout", "-q", "-b", "topic")
        self.assertEqual(self.release("1.2.3", "--yes"), (1, []))
        git(self.work, "checkout", "-q", "main")
        (self.work / "b").write_text("b")
        git(self.work, "add", "-A")
        git(self.work, "commit", "-qm", "unpushed")
        self.assertEqual(self.release("1.2.3", "--yes"), (1, []))

    def test_an_existing_tag_is_refused_locally_or_on_origin(self):
        git(self.work, "tag", "v1.2.3")
        self.assertEqual(self.release("1.2.3", "--yes"), (1, []))
        git(self.work, "push", "-q", "origin", "v1.2.3")
        git(self.work, "tag", "-d", "v1.2.3")
        self.assertEqual(self.release("1.2.3", "--yes"), (1, []))

    def test_a_commit_without_passing_ci_is_refused(self):
        def red(sha):
            raise cut.Refusal("CI has not passed")
        self.assertEqual(self.release("1.2.3", "--yes", ci=red), (1, []))

    def test_a_failing_incomplete_or_wrong_commit_regression_is_refused(self):
        failing = passing(self.sha)
        failing["ok"] = False
        failing["suites"][1]["status"] = "FAIL"
        for summary in (failing, passing(self.sha, suites=0), passing("0" * 40), {**passing(self.sha), "tier": "verify"}):
            code, calls = self.release("1.2.3", "--yes", summary=summary)
            self.assertEqual((code, calls), (1, ["regress"]), summary)
            self.assertEqual(git(self.work, "tag", "--list", "v1.2.3"), "")

    def test_main_moving_during_the_regression_is_refused(self):
        def move():
            other = pathlib.Path(self.temporary.name) / "other"
            git(self.temporary.name, "clone", "-q", str(self.origin), str(other))
            for key, value in (("user.email", "t@t"), ("user.name", "t"), ("commit.gpgsign", "false")):
                git(other, "config", key, value)
            (other / "c").write_text("c")
            git(other, "add", "-A")
            git(other, "commit", "-qm", "moved")
            git(other, "push", "-q", "origin", "main")
        code, calls = self.release("1.2.3", "--yes", after=move)
        self.assertEqual((code, calls), (1, ["regress"]))
        self.assertEqual(git(self.work, "tag", "--list", "v1.2.3"), "")

    def test_a_dry_run_tags_nothing_and_declining_tags_nothing(self):
        self.assertEqual(self.release("1.2.3", "--dry-run"), (0, ["regress"]))
        self.assertEqual(git(self.work, "tag", "--list", "v1.2.3"), "")
        self.assertEqual(self.release("1.2.3", answer="n"), (1, ["regress"]))
        self.assertEqual(git(self.work, "tag", "--list", "v1.2.3"), "")


if __name__ == "__main__":
    unittest.main()
