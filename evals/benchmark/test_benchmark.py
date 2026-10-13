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
        env = dict(os.environ, FAKEMARSH_STATE=str(self.tmp / "state"), FAKEMARSH_PLAN=str(self.tmp / "plan.json"),
                   FAKEMARSH_STDIN_LOG=str(self.tmp / "judge-input"))
        out = self.tmp / "out"
        done = subprocess.run([sys.executable, "-W", "error", str(HERE / "run.py"),
                               "--config", str(HERE / "config.example.json"), "--tasks", str(HERE / "fixtures"),
                               "--only", "toy-slugify", "--out", str(out), "--marsh", str(FAKE), "--reps", "1"],
                              env=env, capture_output=True, text=True, timeout=120)
        self.assertEqual(done.returncode, 0, done.stderr)
        return json.loads((out / "toy-slugify.rep0.json").read_text())

    def test_arms_are_scored_on_hidden_tests_not_the_gate(self) -> None:
        config = json.loads((HERE / "config.example.json").read_text())
        labels = run.race_labels(config, "toy-slugify", 0)
        codex_label = next(label for label, agent in labels.items() if agent == "codex")
        record = self.dry_run({
            **{f"claude@{label}": [{"do": "apply", "patch": str(TASK / "overfit.patch")}] for label in "ab"},
            **{f"codex@{label}": [{"do": "apply", "patch": str(TASK / "solution.patch")}] for label in "ab"},
            "claude@retry": [{"do": "say", "text": "looked"}, {"do": "apply", "patch": str(TASK / "solution.patch")}],
            "pi": [{"do": "say", "text": f"{codex_label}\nmore general"}],
        })
        self.assertEqual(record["race_labels"], labels)
        arms = record["arms"]
        # The overfit patch passes the visible gate and fails the hidden tests.
        self.assertTrue(arms["a_claude"]["gate_pass"])
        self.assertFalse(arms["a_claude"]["hidden_pass"])
        self.assertTrue(arms["a_codex"]["hidden_pass"])
        self.assertEqual(arms["b_retry"]["attempts"], 2)
        self.assertTrue(arms["b_retry"]["hidden_pass"])
        self.assertTrue(record["judge"]["invoked"])
        self.assertEqual(record["judge"]["pick"], codex_label)
        self.assertTrue(arms["d_judge"]["hidden_pass"])
        self.assertEqual(record["branches"]["claude"]["files"], ["M\tslug.py"])
        self.assertEqual(record["branches"]["codex"]["touched_tests"], [])
        self.assertGreater(arms["c_race"]["cost_usd"], arms["a_claude"]["cost_usd"])
        # The judge's input names neither agent: markers and agent output
        # go to stderr, which join hides for passing branches.
        judged = (pathlib.Path(self.tmp) / "judge-input").read_text()
        self.assertNotIn("claude", judged)
        self.assertNotIn("codex", judged)

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


class BudgetCap(unittest.TestCase):
    """The runner launches a run only if the spend so far plus the worst case
    for one more run (reserve_per_run_usd) stays within budget_usd."""

    def attempt(self, spent: float) -> tuple[int, list[str]]:
        tmp = pathlib.Path(tempfile.mkdtemp(prefix="marsh-bench-cap-"))
        self.addCleanup(subprocess.run, ["rm", "-rf", str(tmp)])
        out = tmp / "out"
        out.mkdir()
        (out / "earlier.rep0.json").write_text(json.dumps(
            {"branches": {"claude": {"cost_usd": spent}}, "judge": {"invoked": False}}))
        (tmp / "plan.json").write_text(json.dumps({}))
        env = dict(os.environ, FAKEMARSH_STATE=str(tmp / "state"), FAKEMARSH_PLAN=str(tmp / "plan.json"))
        done = subprocess.run([sys.executable, "-W", "error", str(HERE / "run.py"),
                               "--config", str(HERE / "config.example.json"), "--tasks", str(HERE / "fixtures"),
                               "--out", str(out), "--marsh", str(FAKE), "--reps", "1"],
                              env=env, capture_output=True, text=True, timeout=120)
        return done.returncode, sorted(p.name for p in out.glob("*.rep*.json"))

    def test_refuses_a_run_it_cannot_afford_in_the_worst_case(self) -> None:
        config = json.loads((HERE / "config.example.json").read_text())
        just_over = config["budget_usd"] - config["reserve_per_run_usd"] + 0.01
        code, records = self.attempt(just_over)
        self.assertEqual(code, 3)
        self.assertEqual(records, ["earlier.rep0.json"])

    def test_launches_a_run_it_can_afford(self) -> None:
        config = json.loads((HERE / "config.example.json").read_text())
        code, records = self.attempt(config["budget_usd"] - config["reserve_per_run_usd"] - 0.5)
        self.assertEqual(code, 0)
        self.assertEqual(records, ["earlier.rep0.json", "toy-slugify.rep0.json"])


class FakeAgentPlan(unittest.TestCase):
    def test_concurrent_calls_to_one_agent_each_take_the_next_action(self) -> None:
        # Race and retry branches call the same fake agent at once; every call
        # must get its own step of the plan (and none may crash).
        tmp = pathlib.Path(tempfile.mkdtemp(prefix="marsh-bench-plan-"))
        self.addCleanup(subprocess.run, ["rm", "-rf", str(tmp)])
        n = 24
        (tmp / "plan.json").write_text(json.dumps({"claude": [{"do": "say", "text": f"step{i}"} for i in range(n)]}))
        env = dict(os.environ, FAKEMARSH_STATE=str(tmp / "state"), FAKEMARSH_PLAN=str(tmp / "plan.json"))
        calls = [subprocess.Popen([sys.executable, str(FAKE), str(tmp / "bin" / "claude"), "-p", "x"], cwd=tmp, env=env,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) for _ in range(n)]
        outputs = [call.communicate(timeout=60) for call in calls]
        self.assertEqual([call.returncode for call in calls], [0] * n, [err for _, err in outputs])
        steps = sorted(out.strip() for out, _ in outputs)
        self.assertEqual(steps, sorted(f"step{i}" for i in range(n)))


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
        self.assertLess(estimate["expected_total_usd"], config["budget_usd"])
        # The reserve the runner holds back covers a run whose every agent
        # call runs to the timeout at the highest calibrated spend rate.
        self.assertTrue(estimate["reserve_covers_worst_case"])


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
