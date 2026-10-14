"""Run the actual dev-session probes against both stock inventory JSON shapes."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
from self_dev import probes  # noqa: E402


class SelfDevInventory(unittest.TestCase):
    def run_probes(self, wrapped, include_run=True):
        with tempfile.TemporaryDirectory(prefix="marsh-selfdev-probes-") as temporary:
            root = Path(temporary)
            for name in ("bin", "tmp", "control"):
                (root / name).mkdir()
            prefix = "marsh-test0000-"
            child, run = prefix + "s-00000001", prefix + "r-00000002"
            rows = [{"name": child}, {"name": "foreign-r-00000003"}]
            if include_run:
                rows.append({"name": run})
            inventory = root / "inventory.json"
            inventory.write_text(json.dumps({"sandboxes": rows} if wrapped else rows))
            sbx = root / "bin/sbx"
            sbx.write_text(f"""#!{sys.executable}
import pathlib, sys
args = sys.argv[1:]
if args == ['ls', '--json']:
    print(pathlib.Path({str(inventory)!r}).read_text())
elif args == ['run', '-d', 'shell']:
    pass
elif args[:2] == ['exec', {run!r}]:
    pass
else:
    print('sbx: refused fixture operation', file=sys.stderr)
    sys.exit(125)
""")
            sbx.chmod(0o700)
            make = root / "bin/make"
            make.write_text("#!/bin/sh\nexit 0\n")
            make.chmod(0o700)
            environment = {**os.environ, "PATH": str(root / "bin") + os.pathsep + os.environ["PATH"],
                           "HOME": str(root), "MARSH_DEV_SCRATCH": str(root),
                           "MARSH_VM_PREFIX": prefix, "MARSH_DEV_DEPTH": "1"}
            done = subprocess.run(["/bin/sh", "-c", probes("fixture@sha256:" + "a" * 64, False, "outer")],
                                  env=environment, capture_output=True, text=True, timeout=30)
            self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
            self.assertNotIn("Traceback", done.stderr)
            self.assertIn(f"CHILD={child}\n", done.stdout)
            return done.stdout, run

    def test_array_and_wrapped_inventories_identify_the_grants_run_child(self):
        for wrapped in (False, True):
            with self.subTest(wrapped=wrapped):
                output, run = self.run_probes(wrapped)
                self.assertIn(f"RUNVM={run}\n", output)
                self.assertIn("RUN_EXEC_EXIT=0\n", output)

    def test_foreign_vm_is_not_a_substitute_for_a_missing_run_child(self):
        for wrapped in (False, True):
            with self.subTest(wrapped=wrapped):
                output, _ = self.run_probes(wrapped, include_run=False)
                self.assertIn("RUNVM=none\n", output)
                self.assertIn("RUN_EXEC_EXIT=125\n", output)


if __name__ == "__main__":
    unittest.main()
