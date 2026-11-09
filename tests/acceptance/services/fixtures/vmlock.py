#!/usr/bin/env python3
"""The VM lock: a thin shim over the ONE shared helper (the captain's `harness/vmlock.py`).

    vmlock.py [--run NAME] [--timeout S] -- COMMAND ...    exec the shared helper
    vmlock.run(cmd, run="name", **popen_kwargs)              hold the lock until the child is reaped (signals forwarded)
    with vmlock.held("name"): ...                           in-process work with NO child processes only

The shared helper locates itself by the environment variable NFX_VMLOCK (a path to its
`vmlock.py`), defaulting to ~/dev/narwhal-fork-spike/harness/vmlock.py: this test-only tree
does not ship a lock implementation of its own. The shared helper holds `flock` on
`harness/lock` and nothing else (never creates, writes, truncates, renames or deletes it),
checks after acquiring that the held inode is still the one at the path (retrying on the
new file otherwise), keeps the lock until its child is reaped while forwarding signals, and
writes display-only holder information to `lock.info`. What this shim adds is the read-only
`LockGuard`: the lock file's device, inode and contents snapshotted after acquisition and
re-verified later, used by HARNESS-03 to refuse to trust (or delete evidence after) a lock
that changed under the run.
"""
import hashlib
import importlib.util
import os
import pathlib
import sys

SHARED = pathlib.Path(os.environ.get("NFX_VMLOCK") or pathlib.Path.home() / "dev/narwhal-fork-spike/harness/vmlock.py")
_module = None


def shared():
    """The shared helper module (imported lazily, so VMLOCK_TEST_PATH set by a test applies)."""
    global _module
    if _module is None:
        if not SHARED.is_file():
            raise FileNotFoundError(f"the shared VM lock helper {SHARED} does not exist (set NFX_VMLOCK)")
        spec = importlib.util.spec_from_file_location("nfx_shared_vmlock", SHARED)
        _module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(_module)
    return _module


def held(run: str = "unnamed", timeout=None):
    """`with held("run"):` holds the lock for the block (the shared helper's context manager)."""
    return shared().held(run, timeout)


def run(cmd, run: str = "unnamed", timeout=None, **popen_kwargs):
    """Run CMD holding the lock until the child is reaped; INT/TERM/HUP are forwarded to the child
    (the shared helper's `run`). Use this, never `held()` around a subprocess."""
    return shared().run(cmd, run=run, timeout=timeout, **popen_kwargs)


def lock_path() -> pathlib.Path:
    return pathlib.Path(shared().LOCK)


def lock_error():
    return shared().LockError


class LockGuard:
    """The lock file's device, inode and contents, snapshotted (read-only) and re-checked later."""

    def __init__(self, path: pathlib.Path):
        self.path = pathlib.Path(path)
        stat = os.stat(self.path)
        self.dev, self.ino = stat.st_dev, stat.st_ino
        self.digest = hashlib.sha256(self.path.read_bytes()).hexdigest()

    def verify(self) -> list[str]:
        """Problems (empty = the same inode with the same contents)."""
        try:
            stat = os.stat(self.path)
        except OSError as error:
            return [f"the lock file is gone or unreadable ({type(error).__name__}: {error})"]
        problems = []
        if (stat.st_dev, stat.st_ino) != (self.dev, self.ino):
            problems.append(f"the lock file was replaced (inode {self.ino} -> {stat.st_ino})")
        elif hashlib.sha256(self.path.read_bytes()).hexdigest() != self.digest:
            problems.append("the lock file's contents changed")
        return problems


if __name__ == "__main__":
    os.execv(sys.executable, [sys.executable, str(SHARED), *sys.argv[1:]])
