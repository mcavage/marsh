#!/usr/bin/env python3
"""`make test-rust`: fail if a process the tests started outlived them.

A test process is one running an executable under --bin (the test build
directory: marsh, marsh-local, marshd, test binaries, ...) or one whose
environment carries MARSH_TEST_RUN=--token (set on the test runs and inherited
by everything they spawn, GNU Bash included, unless a test clears the
environment; macOS also hides the environment of its own platform binaries).
It has leaked when nothing that predates the run (--since) owns it: every
ancestor below init started during the run (a test, or a helper such as
script(1) that a test started). A process under an older program (a concurrent
harness under make, a user's terminal) is not this run's leak. Under a Linux
subreaper other than init an orphan is not seen.

The leak's whole orphaned tree is reported (helpers and descendants without
the marker included). Survivors get a short grace period to finish exiting, then are printed,
SIGKILLed, and the script exits 1. Zombies are not running and are ignored.
"""
from __future__ import annotations

import argparse
import ctypes
import os
import signal
import subprocess
import sys
import time

MARKER = "MARSH_TEST_RUN"


def linux_table() -> dict[int, tuple[int, float, bool]]:
    """pid -> (ppid, start time, zombie)."""
    ticks = os.sysconf("SC_CLK_TCK")
    with open("/proc/stat", encoding="utf-8") as stat:
        boot = next(int(line.split()[1]) for line in stat if line.startswith("btime "))
    table = {}
    for entry in os.listdir("/proc"):
        if entry.isdigit():
            try:
                with open(f"/proc/{entry}/stat", encoding="utf-8", errors="replace") as stat:
                    fields = stat.read().rsplit(") ", 1)[1].split()
            except (OSError, IndexError):
                continue
            table[int(entry)] = (int(fields[1]), boot + int(fields[19]) / ticks, fields[0] == "Z")
    return table


def linux_identity(pid: int) -> tuple[str, list[str]]:
    try:
        exe = os.readlink(f"/proc/{pid}/exe").removesuffix(" (deleted)")
        with open(f"/proc/{pid}/environ", "rb") as environ:
            env = environ.read().decode(errors="replace").split("\0")
    except OSError:
        return "", []
    return exe, env


def mac_table() -> dict[int, tuple[int, float, bool]]:
    rows = subprocess.run(
        ["ps", "-A", "-o", "pid=,ppid=,stat=,etime="], capture_output=True, text=True, check=True
    ).stdout
    now = time.time()
    table = {}
    for row in rows.splitlines():
        pid, ppid, stat, etime = row.split()
        days, _, clock = etime.rpartition("-")
        seconds = 0
        for part in clock.split(":"):
            seconds = seconds * 60 + int(part)
        table[int(pid)] = (int(ppid), now - seconds - int(days or 0) * 86400, stat.startswith("Z"))
    return table


LIBC = ctypes.CDLL(None) if sys.platform == "darwin" else None


def mac_identity(pid: int) -> tuple[str, list[str]]:
    path = ctypes.create_string_buffer(4096)
    if LIBC.proc_pidpath(pid, path, len(path)) <= 0:
        return "", []
    # KERN_PROCARGS2: argc, exec path, padding, argv, environment, then
    # an empty string before the loader's own strings.
    size = ctypes.c_size_t(1 << 20)
    buffer = ctypes.create_string_buffer(size.value)
    mib = (ctypes.c_int * 3)(1, 49, pid)
    if LIBC.sysctl(mib, 3, buffer, ctypes.byref(size), None, 0) != 0:
        return path.value.decode(errors="replace"), []
    data = buffer.raw[: size.value]
    argc = int.from_bytes(data[:4], sys.byteorder)
    rest = data[4:].split(b"\0", 1)[1].lstrip(b"\0").split(b"\0")
    env = []
    for item in rest[argc:]:
        if not item:
            break
        env.append(item.decode(errors="replace"))
    return path.value.decode(errors="replace"), env


def leaked(test_bin: str, token: str, since: float) -> list[int]:
    table = linux_table() if sys.platform.startswith("linux") else mac_table()
    identity = linux_identity if sys.platform.startswith("linux") else mac_identity
    mine = {os.getpid()}
    pid = os.getpid()
    while pid in table and pid > 1:
        pid = table[pid][0]
        mine.add(pid)
    prefix = os.path.join(os.path.abspath(test_bin), "")
    marker = f"{MARKER}={token}"
    tests = set()
    for pid, (_, _, zombie) in table.items():
        if zombie or pid in mine:
            continue
        exe, env = identity(pid)
        if exe.startswith(prefix) or marker in env:
            tests.add(pid)
    def owned(pid: int) -> bool:
        ancestor = table[pid][0]
        while ancestor > 1 and ancestor in table:
            if table[ancestor][1] < since:
                return True
            ancestor = table[ancestor][0]
        return False

    # A leak's whole orphaned tree goes: from its topmost ancestor below
    # init (perhaps a helper such as script(1)) down to every descendant.
    roots = set()
    for pid in tests:
        if not owned(pid):
            while table[pid][0] > 1 and table[pid][0] in table:
                pid = table[pid][0]
            roots.add(pid)
    tree = set()
    while roots:
        pid = roots.pop()
        tree.add(pid)
        roots.update(child for child, entry in table.items() if entry[0] == pid and child not in tree)
    return sorted(pid for pid in tree if not table[pid][2])


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--bin", required=True, help="test build directory (TEST_BIN)")
    parser.add_argument("--token", required=True, help=f"this run's {MARKER} value")
    parser.add_argument(
        "--since", type=float, required=True, help="when the run began (seconds since the epoch)"
    )
    parser.add_argument("--grace", type=float, default=5.0, help="seconds to let survivors exit")
    args = parser.parse_args()
    deadline = time.monotonic() + args.grace
    survivors = leaked(args.bin, args.token, args.since - 2)
    while survivors and time.monotonic() < deadline:
        time.sleep(0.25)
        survivors = leaked(args.bin, args.token, args.since - 2)
    if not survivors:
        return 0
    print(f"leaked test processes still running after the tests ({len(survivors)}):", file=sys.stderr)
    subprocess.run(
        ["ps", "-o", "pid,ppid,pgid,etime,command", "-p", ",".join(map(str, survivors))],
        stdout=sys.stderr,
        check=False,
    )
    for pid in survivors:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    print("killed them; a test spawned a process it did not reap (tests/leaked_processes.py)", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
