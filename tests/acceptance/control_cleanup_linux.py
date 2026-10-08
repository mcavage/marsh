#!/usr/bin/env python3
"""Causal control-carrier observations inside an exclusively owned Linux PID namespace.

Run as PID 1 via root-owned `unshare --pid --fork --mount-proc`. No process
inventory is used as a cleanup proof. Only this namespace's exact children are
signaled; exiting its init supplies final kernel teardown for escaping fixtures.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


def write_json(path, document):
    path.write_text(json.dumps(document, sort_keys=True) + "\n")


def wait_path(path, timeout=5):
    deadline = time.monotonic() + timeout
    while not path.exists():
        if time.monotonic() >= deadline:
            raise RuntimeError("fixture handshake deadline: " + path.name)
        time.sleep(0.002)


def member(mode, directory):
    if mode in ("setsid", "escaped-pipe"):
        os.setsid()
    if mode != "escaped-pipe":
        for fd in (0, 1, 2):
            try:
                os.close(fd)
            except OSError:
                pass
    write_json(directory / "member-ready", {
        "pid": os.getpid(), "pgid": os.getpgrp(), "sid": os.getsid(0),
        "uid": os.getuid(), "euid": os.geteuid(),
    })
    if mode == "fork":
        # Strictly bounded fanout; children inherit the original process group.
        for _ in range(24):
            if os.fork() == 0:
                break
            time.sleep(0.002)
    wait_path(directory / "caller-returned", 8)
    (directory / ("effect-" + str(os.getpid()))).write_text("after caller return\n")
    time.sleep(5)


def leader(mode, directory):
    write_json(directory / "leader-ready", {
        "pid": os.getpid(), "pgid": os.getpgrp(), "uid": os.getuid(),
    })
    if mode == "sudo":
        result = subprocess.run(["/usr/bin/sudo", "-n", "/usr/bin/id", "-u"], check=False)
        raise SystemExit(result.returncode)
    argv = [sys.executable, "-I", "-S", str(Path(__file__).resolve()), "member", mode, str(directory)]
    if mode == "changed-uid":
        argv = ["/usr/bin/sudo", "-n", "-u", "nobody", *argv]
    subprocess.Popen(argv, stdin=subprocess.DEVNULL,
                     stdout=None if mode == "escaped-pipe" else subprocess.DEVNULL,
                     stderr=subprocess.DEVNULL)
    wait_path(directory / "member-ready")
    if mode == "timeout":
        time.sleep(10)


def run_suite(binary, root):
    if os.getpid() != 1 or os.getuid() != 0:
        raise RuntimeError("requires an exclusively owned root PID namespace init")
    root.mkdir(mode=0o777)
    root.chmod(0o777)
    suite = {"kernel": os.uname().release, "pid_namespace": os.readlink("/proc/self/ns/pid"),
             "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
             "fixture_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
             "cases": []}
    canary = subprocess.Popen(["/usr/bin/sleep", "60"])
    for mode in ("same-group", "setsid", "changed-uid", "fork", "timeout", "escaped-pipe", "sudo"):
        directory = root / mode
        directory.mkdir(mode=0o777)
        directory.chmod(0o777)
        timeout = "300" if mode in ("timeout", "escaped-pipe") else "3000"
        command = ["/usr/bin/setpriv", "--reuid=1000", "--regid=1000", "--clear-groups",
                   str(binary), str(Path(__file__).resolve()), mode, str(directory), timeout]
        completed = subprocess.run(command, capture_output=True, text=True, timeout=8)
        assert completed.returncode == 0, (mode, completed.returncode, completed.stderr)
        result = json.loads(completed.stdout)
        time.sleep(0.15)
        result.update(mode=mode, effects=sorted(path.name for path in directory.glob("effect-*")))
        for kind in ("leader", "member"):
            path = directory / (kind + "-ready")
            if path.exists():
                result[kind] = json.loads(path.read_text())
        assert canary.poll() is None, "unrelated namespace canary was killed"
        if mode in ("same-group", "fork"):
            assert result["ok"] and result["exit_code"] == 0 and not result["effects"], result
            assert result["member"]["pgid"] == result["leader"]["pgid"], result
        elif mode in ("setsid", "changed-uid"):
            assert result["ok"] and result["effects"], result
            if mode == "setsid":
                assert result["member"]["sid"] == result["member"]["pid"], result
            else:
                assert result["member"]["uid"] == 65534, result
                assert result["member"]["pgid"] == result["leader"]["pgid"], result
        elif mode == "timeout":
            assert not result["ok"] and result["error_kind"] == "TimedOut" and not result["effects"], result
        elif mode == "escaped-pipe":
            assert not result["ok"] and result["error_kind"] == "TimedOut" and result["effects"], result
        elif mode == "sudo":
            assert result["ok"] and result["exit_code"] == 0 and bytes(result["stdout"]) == b"0\n", result
        assert result["elapsed_ms"] < 4000, result
        suite["cases"].append(result)
    suite["namespace_canary_survived"] = canary.poll() is None
    # Exact owned child, never a shared process group or enclosing VM init.
    canary.terminate()
    canary.wait(timeout=2)
    print(json.dumps(suite, sort_keys=True), flush=True)


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] in ("leader", "member"):
        {"leader": leader, "member": member}[sys.argv[1]](sys.argv[2], Path(sys.argv[3]))
    else:
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument("--binary", type=Path, required=True)
        parser.add_argument("--root", type=Path, required=True)
        args = parser.parse_args()
        run_suite(args.binary.resolve(), args.root.resolve())
