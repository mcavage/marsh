"""verify_container_deleted accepts Docker's absent answer after stock notices only."""

from __future__ import annotations

import pathlib
import subprocess
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from harness import verify_container_deleted  # noqa: E402

CID = "5ceee5d729e2e245c030bc899235ef3a5237166c372424f036c1a4f5d2ab3ef9"
ABSENT = f"error: no such object: {CID}\n".encode()
LOCK_JSON = (b'{"time":"2026-10-13T00:12:38.127272-07:00","level":"WARN",'
             b'"msg":"docker hub refresh lock held by another process",'
             b'"error":"context deadline exceeded"}\n')


def runner(stderr: bytes, code: int = 1):
    def run(argv: list[str]) -> subprocess.CompletedProcess[bytes]:
        if argv[-2:] == ["inspect", CID]:
            return subprocess.CompletedProcess(argv, code, b"[]\n", stderr)
        return subprocess.CompletedProcess(argv, 0, b"ENGINE:ID\n", b"")
    return run


class ContainerDeletedTest(unittest.TestCase):
    def test_absent_after_a_structured_refresh_lock_notice_is_deleted(self) -> None:
        verify_container_deleted(runner(LOCK_JSON + ABSENT), ["exec", "vm"], CID)

    def test_absent_after_the_text_notice_is_deleted(self) -> None:
        notice = b"WARN: docker hub refresh lock held by another process: timeout\n"
        verify_container_deleted(runner(notice + ABSENT), ["exec", "vm"], CID)

    def test_an_unrecognized_log_line_before_absent_is_uncertain(self) -> None:
        other = b'{"level":"ERROR","msg":"transport lost"}\n'
        with self.assertRaises(AssertionError):
            verify_container_deleted(runner(other + ABSENT), ["exec", "vm"], CID)

    def test_a_notice_without_the_absent_answer_is_uncertain(self) -> None:
        with self.assertRaises(AssertionError):
            verify_container_deleted(runner(LOCK_JSON), ["exec", "vm"], CID)


if __name__ == "__main__":
    unittest.main()
