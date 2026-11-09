#!/usr/bin/env python3
"""The services E2E fixtures' negative self-checks (no VM, no marsh, no network): each known-bad
input to a checker must be caught, and each good input accepted."""
import pathlib
import subprocess
import sys
import unittest

SELFCHECK = pathlib.Path(__file__).with_name("services") / "fixtures" / "selfcheck.py"


class FixtureSelfChecks(unittest.TestCase):
    def test_every_defect_is_caught(self):
        done = subprocess.run([sys.executable, str(SELFCHECK)], capture_output=True, text=True, timeout=300)
        self.assertEqual(done.returncode, 0, done.stdout[-3000:] + done.stderr[-1000:])
        self.assertIn("self-checks as expected", done.stdout)


if __name__ == "__main__":
    unittest.main()
