#!/usr/bin/env python3
"""CONTRACT.md agrees with the code: the generated scenario index is current,
every full-gate check is declared, marked, and tagged to exactly one observation."""
from __future__ import annotations

import ast
import pathlib
import re
import sys
import unittest

DIRECTORY = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(DIRECTORY))

import contract_index  # noqa: E402
import run  # noqa: E402


class ContractIndex(unittest.TestCase):
    def test_generated_scenario_index_is_current(self) -> None:
        text = contract_index.CONTRACT.read_text(encoding="utf-8")
        self.assertEqual(contract_index.current(text), contract_index.render(),
                         "CONTRACT.md is stale: run python3 tests/acceptance/contract_index.py --write")

    def test_every_check_is_marked_once_by_the_full_gate(self) -> None:
        tree = ast.parse((DIRECTORY / "run.py").read_text(encoding="utf-8"))
        marked = {call.args[0].value for call in ast.walk(tree)
                  if isinstance(call, ast.Call) and isinstance(call.func, ast.Attribute)
                  and call.func.attr == "mark" and call.args
                  and isinstance(call.args[0], ast.Constant) and isinstance(call.args[0].value, str)}
        self.assertEqual(marked, set(run.CHECKS))
        self.assertEqual(len(run.CHECKS), len(set(run.CHECKS)))

    def test_every_check_is_tagged_to_exactly_one_observation(self) -> None:
        text = contract_index.CONTRACT.read_text(encoding="utf-8")
        observations = text.split("## Required observations", 1)[1].split("## Scenario index", 1)[0]
        tagged: list[str] = []
        for match in re.finditer(r"(?m)^\d+\. \(((?:`[a-z-]+`(?:, )?)+)\)", observations):
            tagged += re.findall(r"`([a-z-]+)`", match.group(1))
        self.assertEqual(sorted(tagged), sorted(run.CHECKS))


if __name__ == "__main__":
    unittest.main()
