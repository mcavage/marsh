#!/usr/bin/env python3

import importlib.util
import pathlib
import unittest


MODULE_PATH = pathlib.Path(__file__).with_name("run.py")
SPEC = importlib.util.spec_from_file_location("marsh_perf", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
RUN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUN)


class PerfHelpersTest(unittest.TestCase):
    def test_nearest_rank_summary_preserves_observed_values(self) -> None:
        values = list(range(1, 21))
        self.assertEqual(RUN.summary(values), {"p50": 10, "p95": 19})



if __name__ == "__main__":
    unittest.main()
