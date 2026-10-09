"""The VM-safety property of the acceptance harnesses, alone and beside peers.

Alone, any new marsh-* VM is a leak. Under tests/regress.py (a SharedBaseline)
peers' VMs come and go, so only a VM this suite owns may be flagged; a changed
or vanished pre-existing VM is an error in both modes.
"""
import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from provenance import SharedBaseline, stock_cleanup_errors  # noqa: E402

PIX = {"pix-a": "id-1"}


class StockCleanup(unittest.TestCase):
    def test_alone_every_new_marsh_vm_is_a_leak(self) -> None:
        errors = stock_cleanup_errors(dict(PIX), {**PIX, "marsh-k-peer0001": "id-9"})
        self.assertEqual(len(errors), 1)
        self.assertIn("marsh-k-peer0001", errors[0])

    def test_shared_baseline_ignores_peers_but_not_own_leaks(self) -> None:
        before = SharedBaseline(PIX)
        after = {**PIX, "marsh-k-peer0001": "id-9", "marsh-k-mine0001": "id-8"}
        errors = stock_cleanup_errors(before, after, owned={"marsh-k-mine0001"})
        self.assertEqual(len(errors), 1)
        self.assertIn("marsh-k-mine0001", errors[0])
        self.assertNotIn("peer", errors[0])
        self.assertEqual(stock_cleanup_errors(before, {**PIX, "marsh-k-peer0001": "id-9"}, owned=set()), [])

    def test_shared_baseline_still_guards_pre_existing_vms(self) -> None:
        before = SharedBaseline(PIX)
        self.assertIn("disappeared", stock_cleanup_errors(before, {}, owned=set())[0])
        self.assertIn("replaced", stock_cleanup_errors(before, {"pix-a": "id-2"}, owned=set())[0])

    def test_shared_baseline_requires_the_suites_own_names(self) -> None:
        with self.assertRaises(ValueError):
            stock_cleanup_errors(SharedBaseline(PIX), dict(PIX))


if __name__ == "__main__":
    unittest.main()
