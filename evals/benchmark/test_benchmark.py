"""Hermetic checks of the benchmark runner and report against fakemarsh:
no VM, no sbx, no model. Run: python3 -W error -m unittest discover -s evals/benchmark"""
from __future__ import annotations

import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import report  # noqa: E402
import run  # noqa: E402

FAKE = HERE.parent / "common" / "fakemarsh.py"
TASK = HERE / "fixtures" / "toy-slugify"


class DryRun(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = pathlib.Path(tempfile.mkdtemp(prefix="marsh-bench-"))
        self.addCleanup(subprocess.run, ["rm", "-rf", str(self.tmp)])

    def dry_run(self, plan: dict) -> dict:
        (self.tmp / "plan.json").write_text(json.dumps(plan))
        env = dict(os.environ, FAKEMARSH_STATE=str(self.tmp / "state"), FAKEMARSH_PLAN=str(self.tmp / "plan.json"))
        out = self.tmp / "out"
        done = subprocess.run([sys.executable, "-W", "error", str(HERE / "run.py"),
                               "--config", str(HERE / "config.example.json"), "--tasks", str(HERE / "fixtures"),
                               "--only", "toy-slugify", "--out", str(out), "--marsh", str(FAKE), "--reps", "1"],
                              env=env, capture_output=True, text=True, timeout=120)
        self.assertEqual(done.returncode, 0, done.stderr)
        return json.loads((out / "toy-slugify.rep0.json").read_text())

    def test_arms_are_scored_on_hidden_tests_not_the_gate(self) -> None:
        record = self.dry_run({
            "claude@claude": [{"do": "apply", "patch": str(TASK / "overfit.patch")}],
            "codex@codex": [{"do": "apply", "patch": str(TASK / "solution.patch")}],
            "claude@retry": [{"do": "say", "text": "looked"}, {"do": "apply", "patch": str(TASK / "solution.patch")}],
            "pi": [{"do": "say", "text": "codex\nmore general"}],
        })
        arms = record["arms"]
        # The overfit patch passes the visible gate and fails the hidden tests.
        self.assertTrue(arms["a_claude"]["gate_pass"])
        self.assertFalse(arms["a_claude"]["hidden_pass"])
        self.assertTrue(arms["a_codex"]["hidden_pass"])
        self.assertEqual(arms["b_retry"]["attempts"], 2)
        self.assertTrue(arms["b_retry"]["hidden_pass"])
        self.assertTrue(record["judge"]["invoked"])
        self.assertEqual(record["judge"]["pick"], "codex")
        self.assertTrue(arms["d_judge"]["hidden_pass"])
        self.assertEqual(record["branches"]["claude"]["files"], ["M\tslug.py"])
        self.assertEqual(record["branches"]["codex"]["touched_tests"], [])
        self.assertGreater(arms["c_race"]["cost_usd"], arms["a_claude"]["cost_usd"])

    def test_no_passing_branch_skips_the_judge_and_exhausts_retries(self) -> None:
        record = self.dry_run({"claude": [{"do": "say", "text": "nothing"}],
                               "codex": [{"do": "fail"}]})
        self.assertFalse(record["judge"]["invoked"])
        self.assertEqual(record["arms"]["b_retry"]["attempts"], 3)
        self.assertEqual(record["arms"]["c_race"]["label"], "claude")
        self.assertFalse(any(arm["hidden_pass"] for arm in record["arms"].values()))
        self.assertEqual(record["branches"]["codex"]["attempts"][0]["exit"], 1)

    def test_gate_tampering_cannot_change_the_gate_command(self) -> None:
        # The agent rewrites the visible test to pass; the gate still runs the
        # embedded command (which now passes), but the hidden tests restore
        # the test files and fail the unfixed code.
        weakened = "import unittest\n\n\nclass GateTest(unittest.TestCase):\n    def test_nothing(self):\n        pass\n"
        record = self.dry_run({"claude": [{"do": "write", "path": "tests/test_gate.py", "text": weakened}],
                               "codex": [{"do": "say", "text": "nothing"}]})
        claude = record["arms"]["a_claude"]
        self.assertTrue(claude["gate_pass"])
        self.assertFalse(claude["hidden_pass"])
        self.assertEqual(record["branches"]["claude"]["touched_tests"], ["tests/test_gate.py"])


class Parsing(unittest.TestCase):
    def test_usage_from_claude_and_codex_streams(self) -> None:
        claude = run.usage_from(['{"type":"result","total_cost_usd":0.5,"usage":{"input_tokens":10,'
                                 '"cache_read_input_tokens":90,"output_tokens":5}}'])
        self.assertEqual((claude["input"], claude["cached"], claude["reported_cost_usd"]), (10, 90, 0.5))
        codex = run.usage_from(["noise", '{"type":"turn.completed","usage":{"input_tokens":100,'
                                '"cached_input_tokens":60,"output_tokens":7}}'])
        self.assertEqual((codex["input"], codex["cached"], codex["output"]), (40, 60, 7))
        price = {"input": 2.0, "cache_read": 0.1, "output": 10.0}
        self.assertAlmostEqual(run.cost(codex, price), (40 * 2 + 60 * 0.1 + 7 * 10) / 1e6)

    def test_estimate_fits_the_budget_in_expectation(self) -> None:
        config = json.loads((HERE / "config.example.json").read_text())
        estimate = run.estimate(config, 16, 2)
        self.assertLess(estimate["expected_total_usd"] + estimate["calibration_usd"], config["budget_usd"])


class Scoring(unittest.TestCase):
    def test_project_cannot_reach_the_upstream_fix(self) -> None:
        tmp = pathlib.Path(tempfile.mkdtemp(prefix="marsh-bench-up-"))
        self.addCleanup(subprocess.run, ["rm", "-rf", str(tmp)])
        upstream = tmp / "upstream"
        shutil.copytree(TASK / "repo", upstream)
        run.git(upstream, "init", "-q", "-b", "main")
        run.git(upstream, "add", "-A")
        run.git(upstream, "commit", "-q", "-m", "base")
        base = run.git(upstream, "rev-parse", "HEAD").stdout.decode().strip()
        run.git(upstream, "apply", str(TASK / "solution.patch"))
        run.git(upstream, "commit", "-q", "-am", "the fix")
        fix = run.git(upstream, "rev-parse", "HEAD").stdout.decode().strip()
        task = {"id": "t", "repo": str(upstream), "base": base, "gate": {"patch": "gate.patch"}}
        (tmp / "cache").mkdir()
        template = run.prepare_template(TASK, task, tmp / "cache")
        log = run.git(template, "log", "--all", "--format=%s").stdout.decode().split("\n")
        self.assertEqual([line for line in log if line], ["eval: visible gate", "base"])
        self.assertNotEqual(run.git(template, "cat-file", "-e", fix, check=False).returncode, 0)
        self.assertNotIn("re.findall", (template / "slug.py").read_text())

    def test_swebench_ids_match_junit_cases(self) -> None:
        outcomes = [("testing.test_x.TestA", "test_b[1]", "passed"), ("sympy.core.tests.test_numbers",
                    "test_zero_not_false", "failed"), ("testing.test_x", "test_c", "skipped")]
        self.assertEqual(run.test_status("testing/test_x.py::TestA::test_b[1]", outcomes), "passed")
        self.assertEqual(run.test_status("test_zero_not_false", outcomes), "failed")
        self.assertEqual(run.test_status("testing/test_x.py::test_c", outcomes), "skipped")
        self.assertEqual(run.test_status("testing/test_x.py::TestA::test_gone", outcomes), "missing")


class Statistics(unittest.TestCase):
    def test_mcnemar_exact(self) -> None:
        self.assertEqual(report.mcnemar_exact(0, 0), 1.0)
        self.assertAlmostEqual(report.mcnemar_exact(8, 0), 2 / 256)
        self.assertAlmostEqual(report.mcnemar_exact(3, 3), 1.0)

    def test_sign_flip_is_exact_over_tasks(self) -> None:
        self.assertEqual(report.sign_flip_p([1.0, 1.0, 1.0]), 2 / 8)
        self.assertEqual(report.sign_flip_p([0.0, 0.0]), 1.0)
        self.assertAlmostEqual(report.sign_flip_p([0.5, -0.5, 1.0, 1.0]), 6 / 16)

    def test_holm(self) -> None:
        self.assertEqual(report.holm([0.01, 0.04, 0.03]), [0.03, 0.06, 0.06])

    def test_cluster_bootstrap_resamples_tasks(self) -> None:
        mean, lo, hi = report.cluster_bootstrap([[1.0, 1.0], [0.0, 0.0]])
        self.assertEqual(mean, 0.5)
        self.assertEqual((lo, hi), (0.0, 1.0))


if __name__ == "__main__":
    unittest.main()
