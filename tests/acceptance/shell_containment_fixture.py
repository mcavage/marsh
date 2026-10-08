#!/usr/bin/env python3
"""Credential-free process fixture, NOT a cleanup implementation or oracle.

All forks retain this unique script/token in argv, so the independent host /proc
inventory can find even reparented and late-TERM descendants without consulting
production cgroups, session records or receipts. Only test-owned PIDs are used.
"""
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

mode, directory, token = sys.argv[1:]
root = Path(directory)
signal.signal(signal.SIGTERM, signal.SIG_IGN)
signal.signal(signal.SIGHUP, signal.SIG_IGN)


def pulse(role):
    # These observations are selectors, not trusted success evidence. The host
    # independently reads kernel PID/starttime/ancestry/SID/UID and file bytes.
    (root / f"{token}-{os.getpid()}.identity").write_text(json.dumps({
        "pid": os.getpid(), "parent": os.getppid(), "uid": os.getuid(),
        "session": os.getsid(0), "role": role,
    }))
    print(f"fixture-ready:{role}", flush=True)
    pulse_path = root / f"{token}-{os.getpid()}.pulse"
    pulse_path.touch()
    if mode == "uid-change-term":
        pulse_path.chmod(0o666)  # This disposable fixture must keep writing after setuid.
        (root / f"{token}.root-ready").write_text(str(os.getpid()))
    while True:
        with pulse_path.open("ab") as stream:
            stream.write(b"x")
        time.sleep(0.05)


if mode == "root-foreground":
    signal.signal(signal.SIGTTOU, signal.SIG_IGN)
    # sudo with pipe/null stdio keeps the command in this session. The root
    # command creates its OWN PGID; no nonroot sudo monitor can mask the old
    # helper's UID-filter bug by forwarding our signal on its behalf.
    child = subprocess.Popen(["sudo", "-n", sys.executable, "-I", "-S", __file__,
                              "root-foreground-child", directory, token],
                             stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                             stderr=subprocess.DEVNULL)
    marker = root / f"{token}.root-foreground-ready"
    while not marker.exists():
        if child.poll() is not None:
            raise RuntimeError("sudo foreground fixture failed before readiness")
        time.sleep(0.02)
    os.tcsetpgrp(0, int(marker.read_text()))
    (root / f"{token}.foreground-ready").write_text(marker.read_text())
    os._exit(child.wait())
elif mode == "root-foreground-child":
    if os.getuid() != 0:
        raise RuntimeError("root foreground member requires real sudo")
    os.setpgid(0, 0)
    def interrupted(_signal, _frame):
        (root / f"{token}.interrupted").write_bytes(b"INT\x00received\n")
        os._exit(42)
    signal.signal(signal.SIGINT, interrupted)
    (root / f"{token}.root-foreground-ready").write_text(str(os.getpgrp()))
    pulse("root-foreground")
elif mode == "foreground-nonleader":
    signal.signal(signal.SIGTTOU, signal.SIG_IGN)
    group = os.fork()
    if group == 0:
        os.setpgid(0, 0)
        if os.fork() != 0:
            os._exit(0)  # PGID leader reaped; foreground member still lives.
        def interrupted(_signal, _frame):
            (root / f"{token}.interrupted").write_bytes(b"INT\x00received\n")
            os._exit(42)
        signal.signal(signal.SIGINT, interrupted)
        pulse("foreground-nonleader")
    os.waitpid(group, 0)
    os.tcsetpgrp(0, group)
    (root / f"{token}.foreground-ready").write_text(str(group))
    while not (root / f"{token}.interrupted").exists():
        time.sleep(0.02)
    os._exit(42)
elif mode == "sudo-control":
    def interrupted(_signal, _frame):
        (root / f"{token}.interrupted").write_bytes(b"INT\x00received\n")
        os._exit(42)
    signal.signal(signal.SIGINT, interrupted)
    pulse("sudo-control")
elif mode == "late-term":
    def late_term(_signal, _frame):
        if os.fork() == 0:
            os.setsid()
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            pulse("late-term-setsid")
        os._exit(0)
    signal.signal(signal.SIGTERM, late_term)
    pulse("term-parent")
elif mode in ("setsid", "doublefork", "leader-first"):
    if os.fork() == 0:
        os.setsid()
        if mode == "doublefork" and os.fork() != 0:
            os._exit(0)
        pulse(mode)
    if mode == "leader-first":
        # The host observes initial ancestry, then releases the leader. Cleanup
        # must still find the escaped child after leader exit, not confuse that
        # exit with all descendants being gone.
        while not (root / f"{token}.release").exists():
            time.sleep(0.02)
        os._exit(23)
    pulse("parent")
elif mode == "uid-change-term":
    if os.getuid() != 0:
        raise RuntimeError("uid-change-term fixture must start through real sudo")
    def change_uid(_signal, _frame):
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        os.setgid(int(os.environ["SUDO_GID"]))
        os.setuid(int(os.environ["SUDO_UID"]))
        (root / f"{token}.uid-changed").write_text(str(os.getuid()))
    signal.signal(signal.SIGTERM, change_uid)
    pulse("root-until-term")
elif mode == "uid-change":
    # Run through real sudo, then change real/effective/saved UID while retaining
    # process/kernel identity and cgroup. No ptrace/no_new_privs approximations.
    if os.getuid() != 0:
        raise RuntimeError("uid-change fixture must start through real sudo")
    (root / f"{token}.root-ready").write_text(str(os.getpid()))
    while not (root / f"{token}.change-uid").exists():
        time.sleep(0.02)
    os.setgid(int(os.environ["SUDO_GID"]))
    os.setuid(int(os.environ["SUDO_UID"]))
    pulse("uid-changed")
else:
    pulse(mode)
