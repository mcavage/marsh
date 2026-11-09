"""H-2: the harness's use of the one shared VM lock.

The lock is implemented once, for every agent and harness on this Mac, by `~/dev/narwhal-fork-spike/harness/vmlock.py`
(override the path with `NFX_VMLOCK`). This module only loads that helper and adapts its errors; it holds no lock
logic of its own: no `flock` here, no stale-lock detection, no stealing, no recovery, and nothing here ever opens,
creates, deletes, renames, truncates or writes the lock file. Who holds the lock is in `<lock>.info`, written by the
helper for display only.

Locked work runs as the child of the helper's own CLI (`supervise`): the helper starts it in its own session and holds
the lock until every process of that session has exited, and INT/TERM/HUP are forwarded to it. The worker proves it
is under a live holder from kernel and process-table facts (`hold_proof`), never from an environment variable.
Nothing the harness spawns may leave that session (no `start_new_session`, no `setsid`): children use their own
process group (`process_group=0`) inside it.
"""

from __future__ import annotations

import contextlib
import fcntl
import importlib.util
import os
import pathlib
import signal
import subprocess
import sys
from types import ModuleType
from typing import Any, ContextManager

DEFAULT_HELPER = pathlib.Path.home() / "dev" / "narwhal-fork-spike" / "harness" / "vmlock.py"
EXIT_LOCK_HELD = 4
HELPER_TIMEOUT = 75  # the helper CLI's exit status when the wait for the lock ran out
WORKER_RUN = "NFX_WORKER_RUN"  # a hint that this process was started as a worker (never proof: see `hold_proof`)
STARTED_FILE = "NFX_STARTED_FILE"  # a path the worker creates once it holds the lock and starts (the watchdog's cue)


class LockUnavailable(Exception):
    """The helper or the lock file is missing: nothing is created to make up for it."""


class LockHeld(Exception):
    def __init__(self, holders: str, waited: float) -> None:
        self.holders = holders
        self.waited = waited
        super().__init__(f"VM lock held (per lock.info, display only): {holders}"
                         + (f"; waited {waited:.0f}s" if waited else ""))


def helper_path() -> pathlib.Path:
    return pathlib.Path(os.environ.get("NFX_VMLOCK") or DEFAULT_HELPER).expanduser()


def load() -> ModuleType:
    """The helper module, loaded fresh so that `VMLOCK_TEST_PATH` in the environment now is the lock it uses."""
    path = helper_path()
    if not path.is_file():
        raise LockUnavailable(f"the shared VM lock helper {path} does not exist (set NFX_VMLOCK)")
    spec = importlib.util.spec_from_file_location(f"vmlock_{os.getpid()}_{id(object())}", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def lock_path() -> pathlib.Path:
    return pathlib.Path(load().LOCK)


def info_path(lock: pathlib.Path) -> pathlib.Path:
    return pathlib.Path(str(lock) + ".info")


def _checked(module: ModuleType) -> None:
    if not pathlib.Path(module.LOCK).is_file():
        raise LockUnavailable(f"the lock file {module.LOCK} does not exist; tools never create it")


def _held(module: ModuleType, error: Exception, wait: float | None) -> LockHeld:
    return LockHeld(module.holder(), wait or 0)


def hold(run: str, wait: float | None = 0) -> ContextManager[None]:
    """In-process locked work with no child that outlives the block. WAIT seconds to wait (None: forever)."""
    module = load()
    _checked(module)
    inner = module.held(run, timeout=wait)

    class _Hold:
        def __enter__(self) -> None:
            try:
                inner.__enter__()
            except module.LockError as error:
                raise _held(module, error, wait) from None

        def __exit__(self, *exc: Any) -> Any:
            return inner.__exit__(*exc)

    return _Hold()


def helper_command(run: str, wait: float | None, command: list[str]) -> list[str]:
    """The helper's own CLI: `vmlock.py --run RUN [--timeout N] -- COMMAND`."""
    argv = [sys.executable, str(helper_path()), "--run", run]
    if wait is not None:
        argv += ["--timeout", str(wait)]
    return [*argv, "--", *command]


def supervise(command: list[str], run: str, wait: float | None = 0, env: dict[str, str] | None = None) -> int:
    """Run COMMAND as the child of the shared helper's CLI (`vmlock.py --run RUN -- COMMAND`), which takes the lock,
    starts the child in its own session and holds the lock until every process of that session has exited. INT, TERM
    and HUP sent to this process are forwarded to the helper, which forwards them to the child. Returns the child's
    exit status; raises LockHeld (naming the holder from lock.info) when WAIT runs out first."""
    module = load()
    _checked(module)
    process = subprocess.Popen(helper_command(run, wait, command), env=env, process_group=0)

    def forward(number: int, frame: Any) -> None:
        with contextlib.suppress(ProcessLookupError):
            process.send_signal(number)

    previous = {number: signal.signal(number, forward) for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)}
    try:
        while True:
            try:
                code = process.wait()
                break
            except InterruptedError:
                continue
    finally:
        for number, handler in previous.items():
            signal.signal(number, handler)
    if code == HELPER_TIMEOUT:
        raise LockHeld(module.holder(), wait or 0)
    return 128 - code if code < 0 else code


def hold_proof(run: str) -> list[str]:
    """Why this process is NOT the worker child of a live helper holding the VM lock for RUN (empty: it is). Every
    item is a kernel or process-table fact, none is a caller-supplied claim: (1) the parent process is the shared
    helper's CLI started for exactly this run; (2) the lock file is flock-held right now (a non-blocking shared lock
    on it is refused); (3) this process leads the session the helper created for it; (4) the helper's `lock.info`
    names that parent and this run (display data, but the helper writes it only after acquiring: a mismatch fails
    closed). A forged environment variable proves none of these."""
    problems: list[str] = []
    parent = os.getppid()
    try:
        command = subprocess.run(["ps", "-o", "command=", "-p", str(parent)], capture_output=True, text=True,
                                 timeout=15, check=False).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        command = ""
    if str(helper_path()) not in command.split() or f"--run {run} " not in command + " ":
        problems.append(f"the parent process {parent} is not the shared lock helper started for run {run} "
                        f"({command[:160]!r})")
    try:
        lock = lock_path()
        descriptor = os.open(lock, os.O_RDONLY | os.O_CLOEXEC)
    except (OSError, LockUnavailable) as error:
        problems.append(f"the lock file cannot be opened: {error}")
    else:
        try:
            fcntl.flock(descriptor, fcntl.LOCK_SH | fcntl.LOCK_NB)
        except BlockingIOError:
            pass  # held by someone, as it must be
        else:
            problems.append(f"nobody holds the VM lock {lock}: a shared lock on it was granted")
        finally:
            os.close(descriptor)  # drops any shared lock just taken
        record = holder_record(lock)
        if not record or record.get("pid") != parent or record.get("run") != run:
            problems.append(f"the lock's info does not name the parent {parent} and run {run}: {record}")
    if os.getsid(0) != os.getpid():
        problems.append("this process does not lead the session the helper created for its child")
    return problems


def holder_record(lock: pathlib.Path | None = None) -> dict[str, Any] | None:
    """`lock.info` for display (and for the harness's own evidence that the helper named this run)."""
    import json
    try:
        record = json.loads(info_path(lock or lock_path()).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    return record if isinstance(record, dict) else None
