#!/usr/bin/env python3
"""Real callers of the parallel unittest runner; no binaries, VMs, or network."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

RUNNER = Path(__file__).with_name("parallel_unittest.py")


class ParallelUnittest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="marsh-parallel-tests-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def suite(self, directory, source):
        parent = self.root / directory
        parent.mkdir()
        path = parent / "test_fixture.py"
        path.write_text(source)
        return path

    def run_tests(self, *arguments):
        return subprocess.run([sys.executable, str(RUNNER), *map(str, arguments)],
                              capture_output=True, text=True, timeout=30)

    def test_same_named_suites_and_imports_are_isolated(self):
        files = []
        for name in ("Left", "Right"):
            path = self.suite(name, f"""import unittest, run
class {name}(unittest.TestCase):
    def test_identity(self):
        self.assertEqual(run.IDENTITY, {name!r})
""")
            path.with_name("run.py").write_text(f"IDENTITY = {name!r}\n")
            files.append(path)
        done = self.run_tests("-j", "2", "-W", "error", *files)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertIn("Ran 2 tests", done.stdout)
        self.assertIn("; OK", done.stdout)

    def test_workers_overlap_in_distinct_processes(self):
        files = []
        for index in range(2):
            files.append(self.suite(str(index), f"""import os, time, unittest
from pathlib import Path
class Worker(unittest.TestCase):
    def test_overlap(self):
        root = Path({str(self.root)!r})
        marker = root / 'pending-{index}'
        marker.write_text(str(os.getpid()))
        marker.rename(root / 'worker-{index}')
        peer = root / 'worker-{1 - index}'
        deadline = time.monotonic() + 5
        while not peer.exists() and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertTrue(peer.exists(), 'workers did not overlap')
        self.assertNotEqual(int(peer.read_text()), os.getpid())
"""))
        done = self.run_tests("-j", "2", *files)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        self.assertNotEqual(int((self.root / "worker-0").read_text()), os.getpid())

    def test_failure_does_not_hide_other_tests(self):
        path = self.suite("failure", """import unittest
class Fixture(unittest.TestCase):
    def test_fail(self):
        self.fail('expected failure')
    def test_pass(self):
        pass
""")
        done = self.run_tests("-j", "2", path)
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("expected failure", done.stdout)
        self.assertIn("Ran 2 tests", done.stdout)
        self.assertIn("1 FAILED", done.stdout)

    def test_empty_or_unloadable_suite_cannot_pass(self):
        for name, source in (("empty", "import unittest\n"),
                             ("broken", "raise RuntimeError('broken import')\n")):
            with self.subTest(name=name):
                done = self.run_tests(self.suite(name, source))
                self.assertNotEqual(done.returncode, 0)
                self.assertIn("cannot enumerate tests", done.stderr)

    def test_warning_policy_applies_to_discovery_and_execution(self):
        for name, source in (("discovery", "import warnings\nwarnings.warn('discovery warning')\n"),
                             ("execution", """import unittest, warnings
class Fixture(unittest.TestCase):
    def test_warning(self):
        warnings.warn('execution warning')
""")):
            with self.subTest(name=name):
                done = self.run_tests("-W", "error", self.suite(name, source))
                self.assertNotEqual(done.returncode, 0)
                self.assertIn(f"{name} warning", done.stdout + done.stderr)

    def test_nonpositive_concurrency_is_rejected(self):
        for jobs in ("0", "-1"):
            with self.subTest(jobs=jobs):
                done = self.run_tests("-j", jobs, self.root / "missing.py")
                self.assertNotEqual(done.returncode, 0)
                self.assertIn("--jobs must be positive", done.stderr)


if __name__ == "__main__":
    unittest.main()
