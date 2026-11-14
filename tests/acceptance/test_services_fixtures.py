#!/usr/bin/env python3
"""The services E2E fixtures' negative self-checks (no VM, no marsh, no network): each known-bad
input to a checker must be caught, and each good input accepted."""
import pathlib
import subprocess
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).with_name("services")))
import nfx_lock  # noqa: E402

SELFCHECK = pathlib.Path(__file__).with_name("services") / "fixtures" / "selfcheck.py"


@unittest.skipUnless(nfx_lock.helper_path().is_file() and pathlib.Path("/private/tmp").is_dir(),
                     "needs the Mac's shared VM lock helper (NFX_VMLOCK) and /private/tmp")
class FixtureSelfChecks(unittest.TestCase):
    def test_every_defect_is_caught(self):
        done = subprocess.run([sys.executable, str(SELFCHECK)], capture_output=True, text=True, timeout=300)
        unexpected = [line for line in done.stdout.splitlines() if line.startswith("NOT ") or "NOT AS EXPECTED" in line]
        self.assertEqual(done.returncode, 0, "\n".join(unexpected) + "\n" + done.stdout[-3000:] + done.stderr[-1000:])
        self.assertIn("self-checks as expected", done.stdout)
        skipped = [line for line in done.stdout.splitlines() if line.startswith("SKIPPED ")]
        if skipped:
            self.skipTest("the other checks passed; not run: " + "; ".join(skipped))


if __name__ == "__main__":
    unittest.main()
