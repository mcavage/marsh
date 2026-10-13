"""Hermetic checks of the reach harness: canned replies graded against
fakemarsh and recording shims. No model, VM, or network.
Run: python3 -W error -m unittest discover -s evals/reach"""
from __future__ import annotations

import json
import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import run  # noqa: E402

BAD = {
    "R1-install": "```sh\npip install marsh\nmarsh login\n```\n",
    "R2-agent-job": "Run `marsh` and then ask Claude.\n",
    "R3-fanout": "```sh\nmarsh split -n -b lint='make lint' -b test='make test' | marsh join\n```\n",
    "R4-race": "```sh\nmarsh race claude codex -- make test\n```\n",
    "R5-race-apply": ("```sh\nmarsh fanout -n -b claude=\"claude -p 'fix the test'\" "
                      "-b codex=\"codex exec 'fix the test'\" | marsh collect\n```\n"),
    "R6-results": "```sh\nmarsh history --last\n```\n",
    "R7-stop": "```sh\nsbx rm --all\n```\n",
}


class Reach(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = pathlib.Path(tempfile.mkdtemp(prefix="marsh-reach-test-"))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.llms = self.tmp / "llms.txt"
        self.llms.write_text("# marsh\n> fixture index\n")

    def trial(self, responses: pathlib.Path) -> dict[str, dict]:
        out = self.tmp / "out"
        for command in (["ask", "--out", str(out), "--responder", f"fixture:{responses}", "--models", "m/x",
                         "--conditions", "closed", "--reps", "1", "--llms", str(self.llms)],
                        ["grade", "--out", str(out), "--backend", "fake"]):
            done = subprocess.run([sys.executable, "-W", "error", str(HERE / "run.py"), *command],
                                  capture_output=True, text=True, timeout=300)
            self.assertEqual(done.returncode, 0, done.stderr)
        return {p.parent.parent.name: json.loads(p.read_text()) for p in out.glob("*/*/*/rep0/grade.json")}

    def test_documented_answers_pass(self) -> None:
        grades = self.trial(HERE / "fixtures" / "responses")
        self.assertEqual(len(grades), 7)
        for task, grade in grades.items():
            self.assertTrue(grade["success"], (task, grade))

    def test_wrong_answers_fail_for_the_right_reason(self) -> None:
        responses = self.tmp / "bad"
        responses.mkdir()
        for task, text in BAD.items():
            (responses / f"{task}.md").write_text(text)
        grades = self.trial(responses)
        for task, grade in grades.items():
            self.assertFalse(grade["success"], task)
        failed = {task: {c["why"] for c in g["checks"] if not c["passed"]} for task, g in grades.items()}
        self.assertIn("no invented or unnecessary installers", failed["R1-install"])
        self.assertEqual(grades["R2-agent-job"]["reason"], "no script in the reply")
        self.assertIn("uses fanout, not split, for read-only checks", failed["R3-fanout"])
        self.assertEqual(grades["R4-race"]["unknown_cli"], ["race"])
        # Agents in a fanout write the user's real files: the fix lands, but
        # the race was not private and nothing was gated.
        self.assertIn("uses split", failed["R5-race-apply"])
        self.assertIn("does not remove VMs out from under marsh or delete state", failed["R7-stop"])

    def test_destructive_scripts_are_not_run(self) -> None:
        responses = self.tmp / "evil"
        responses.mkdir()
        (responses / "R7-stop.md").write_text("```sh\nrm -rf ~\n```\n")
        grades = self.trial(responses)
        self.assertEqual(grades["R7-stop"]["reason"], "refused: destructive command")

    def test_script_extraction_and_cli_words(self) -> None:
        self.assertEqual(run.extract_script("x\n```console\n$ marsh stop\n```\n"), "marsh stop\n")
        self.assertIsNone(run.extract_script("no code"))
        words = run.marsh_words("# marsh is great\nmarsh split -n | marsh join\nfoo | msh results\n")
        self.assertEqual(words, ["split", "join", "results"])
        known = run.known_subcommands()
        self.assertTrue({"split", "join", "fanout", "collect", "results", "stop", "run"} <= known)
        self.assertNotIn("race", known)

    def test_wilson(self) -> None:
        lo, hi = run.wilson(7, 7)
        self.assertAlmostEqual(hi, 1.0)
        self.assertAlmostEqual(lo, 0.6457, places=3)


if __name__ == "__main__":
    unittest.main()
