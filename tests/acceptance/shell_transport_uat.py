#!/usr/bin/env python3
"""Real CLI/Unix-socket/PTY/pipe callers, no stock/Cloud effects.

The private daemon peer tests the public controller boundary, not guest cleanup.
The backend real-process regression and final stock journey supply that evidence.
Every subprocess is in an owned session; only exact tracked PIDs are signalled.
"""
from __future__ import annotations
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import select
import shlex
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time


def receive(conn):
    def exact(size):
        out = bytearray()
        while len(out) < size:
            data = conn.recv(size - len(out))
            if not data:
                raise EOFError("peer disconnected")
            out.extend(data)
        return out
    size, = struct.unpack(">I", exact(4))
    if not 0 < size <= 1024 * 1024:
        raise AssertionError(f"invalid frame length {size}")
    return json.loads(exact(size))


def send(conn, frame):
    wire = json.dumps(frame, separators=(",", ":")).encode()
    conn.sendall(struct.pack(">I", len(wire)) + wire)


class Peer:
    def __init__(self, root, behavior):
        self.path = root / "s"
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.bind(str(self.path)); self.path.chmod(0o600)
        self.socket.listen(); self.socket.settimeout(0.1)
        self.stopped = threading.Event()
        self.errors = []
        self.frames = []
        self.requests = []
        self.tasks = []
        self.connections = []
        self.preparing = False
        self.cleanup = threading.Event()
        self.behavior = behavior
        self.task = threading.Thread(target=self.serve)
        self.task.start()

    def serve(self):
        while not self.stopped.is_set():
            try:
                conn, _ = self.socket.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            self.connections.append(conn)
            task = threading.Thread(target=self.handle, args=(conn,))
            self.tasks.append(task); task.start()

    def handle(self, conn):
        try:
            with conn:
                conn.settimeout(25)
                request = receive(conn)["body"]
                self.requests.append(request)
                kind = request["type"]
                if kind in ("attach_shell", "attach_pinned_shell"):
                    if getattr(self.behavior, "reject_attach", False):
                        send(conn, {"type": "error", "code": "internal", "message": "shell attachment unavailable"})
                    else:
                        send(conn, {"type": "shell_attached", "session_id": "transport-uat"})
                elif kind == "detach_shell":
                    assert not self.preparing or self.cleanup.is_set(), "CLI detached before cancelled preparation cleanup"
                    send(conn, {"type": "detached"})
                elif kind == "status":
                    if getattr(self.behavior, "drop_status", False):
                        return
                    send(conn, {"type": "status", "schema": "marsh.status/v1", "features": [],
                        "scope_id": "owned", "daemon_id": "owned", "endpoint_owner": {"uid": os.getuid(), "pid": os.getpid()},
                        "current_session_id": "transport-uat", "control_home": str(self.path.parent), "job_defaults": None,
                        "workers": [], "shells": [{"session_id": "transport-uat", "daemon_id": "owned", "pid": os.getpid(),
                        "project": str(self.path.parent), "state": "detached" if self.cleanup.is_set() else "attached"}]})
                elif kind == "open_shell":
                    send(conn, {"type": "shell_accepted"})
                    self.behavior(conn, request, self)
                else:
                    raise AssertionError(f"unexpected request: {kind}")
        except Exception as error:
            self.errors.append(repr(error))

    def close(self):
        self.stopped.set(); self.socket.close(); self.task.join(timeout=2)
        for conn in self.connections:
            try:
                conn.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        for task in self.tasks:
            task.join(timeout=3)
        assert not self.task.is_alive() and not any(t.is_alive() for t in self.tasks), "peer thread leak"


def reap(child):
    if child.poll() is None:
        child.send_signal(signal.SIGCONT)
        child.terminate()
        try:
            child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            child.kill(); child.wait(timeout=3)


def cleanup_owned(children, peer, descriptors=()):
    # Do not strand the non-daemon listener if product reaping itself times out.
    # Preserve the primary caller failure; report every additional cleanup fault.
    pending = sys.exc_info()[0] is not None
    failures = []
    # Closing our PTY endpoints also releases Darwin's exit-time tty drain.
    for fd in descriptors:
        if fd is not None:
            try: os.close(fd)
            except OSError as error: failures.append(f"owned FD {fd}: {error!r}")
    for child in children:
        if child is not None:
            try: reap(child)
            except Exception as error: failures.append(f"owned PID {child.pid}: {error!r}")
    if peer is not None:
        try: peer.close()
        except Exception as error: failures.append(f"peer shutdown: {error!r}")
    if failures:
        print("additional cleanup faults: "+repr(failures), file=sys.stderr)
        if not pending: raise AssertionError(failures)


def environment(root, peer):
    token = root / "token"
    token.write_text("0" * 64); token.chmod(0o600)
    home = root / "home"; home.mkdir()
    control = root / "control"; control.mkdir()
    return dict(PATH=os.environ.get("PATH", "/usr/bin:/bin"), HOME=os.environ["HOME"],
                USER=os.environ.get("USER", "node"), LOGNAME=os.environ.get("LOGNAME", "node"),
                MARSH_HOME=str(home), MARSH_CONTROL_HOME=str(control),
                MARSH_DAEMON_SOCKET=str(peer.path), MARSH_DAEMON_TOKEN=str(token))


def drain_pty(master):
    out = bytearray()
    while select.select([master], [], [], 0.2)[0]:
        try:
            part = os.read(master, 65536)
        except OSError:
            break
        if not part:
            break
        out.extend(part)
    return bytes(out)


def communicate_with_ttys(child, descriptors, timeout):
    # Darwin can wait for the controlling terminal's queued output during exit.
    # A real terminal reads concurrently; waiting before reading deadlocks GNU
    # Bash and Python too. These owned readers stop and join on every path.
    stopped = threading.Event()
    outputs = {fd: bytearray() for fd in descriptors}
    failures = []

    def read(fd):
        try:
            while not stopped.is_set():
                if not select.select([fd], [], [], 0.02)[0]:
                    continue
                try:
                    part = os.read(fd, 65536)
                except BlockingIOError:
                    continue
                except OSError as error:
                    if error.errno == 5:  # PTY peer closed (EIO).
                        return
                    raise
                if not part:
                    return
                outputs[fd].extend(part)
                assert len(outputs[fd]) <= 8 * 1024 * 1024, "private PTY output bound"
        except Exception as error:
            failures.append(repr(error))

    tasks = []
    for fd in descriptors:
        fcntl.fcntl(fd, fcntl.F_SETFL, fcntl.fcntl(fd, fcntl.F_GETFL) | os.O_NONBLOCK)
        task = threading.Thread(target=read, args=(fd,))
        tasks.append(task)
        task.start()
    try:
        stdout, stderr = child.communicate(timeout=timeout)
    finally:
        stopped.set()
        for task in tasks:
            task.join(timeout=2)
        assert not any(task.is_alive() for task in tasks), "owned PTY reader leak"
    assert not failures, failures
    return stdout, stderr, outputs


def run_case(binary, root, behavior, arguments, expected, *, terminal=False, redirect=False, interrupt=False, separate_tty=False):
    peer = Peer(root, behavior)
    env = environment(root, peer)
    master = slave = error_master = error_slave = None
    if terminal:
        master, slave = pty.openpty()
    # Darwin also exposes a kernel-maintained bit after a controlling TTY is
    # written (our GNU/Python controls observe it too). Compare the caller's
    # access and mutable I/O status flags, including the nonblocking flag.
    status_flags = os.O_ACCMODE | os.O_NONBLOCK | os.O_APPEND | os.O_ASYNC | os.O_SYNC | os.O_DSYNC
    input_flags = fcntl.fcntl(slave, fcntl.F_GETFL) & status_flags if terminal else None
    if separate_tty:
        error_master, error_slave = pty.openpty()
    argv = [str(binary), *arguments]
    if terminal:
        # start_new_session creates our exact PID/PGID; acquire this owned PTY
        # as controlling terminal before exec so ^C is a real keyboard signal.
        argv = [sys.executable, "-I", "-S", "-c",
                "import fcntl,os,sys,termios; fcntl.ioctl(0,termios.TIOCSCTTY,0); os.execv(sys.argv[1],sys.argv[1:])",
                *argv]
    started = time.monotonic()
    child = subprocess.Popen(argv, env=env, cwd=root,
                             stdin=slave if terminal else subprocess.DEVNULL,
                             stdout=slave if terminal else subprocess.PIPE,
                             stderr=error_slave if separate_tty else subprocess.PIPE if redirect or not terminal else slave,
                             start_new_session=True)
    try:
        if interrupt:
            deadline = time.monotonic() + 10
            while not peer.requests or not any(r["type"] == "open_shell" for r in peer.requests):
                assert child.poll() is None and time.monotonic() < deadline, peer.errors
                time.sleep(0.01)
            # ColdBoot has arrived, but no readiness is permitted by this peer.
            assert select.select([master], [], [], 5)[0], "missing cold startup progress"
            assert termios.tcgetattr(slave)[3] & termios.ICANON, "raw mode entered before readiness"
            assert os.tcgetpgrp(master) == child.pid, "fixture has no owned foreground process group"
            os.write(master, b"\x03")
        if terminal:
            stdout, stderr, tty_outputs = communicate_with_ttys(
                child, [master, *([error_master] if separate_tty else [])], timeout=15)
        else:
            stdout, stderr = child.communicate(timeout=15)
        controller_finished = time.monotonic()
        if terminal:
            assert fcntl.fcntl(slave, fcntl.F_GETFL) & status_flags == input_flags, "controller changed parent's terminal I/O status flags"
            os.close(slave); slave = None
            stdout = bytes(tty_outputs[master]) + drain_pty(master)
        if separate_tty:
            os.close(error_slave); error_slave = None
            stderr = bytes(tty_outputs[error_master]) + drain_pty(error_master)
        assert child.returncode == expected, (child.returncode, expected, stdout, stderr, peer.errors)
        peer.close()
        assert not peer.errors, peer.errors
        return dict(code=child.returncode, seconds=controller_finished-started,
                    harness_seconds=time.monotonic()-started,
                    measurement="private-peer controller spawn through communicate completion; PTY drain/peer joins excluded, NOT stock latency",
                    stdout=(stdout or b"").decode(errors="replace"),
                    stderr=(stderr or b"").decode(errors="replace"), frames=peer.frames,
                    request_types=[r["type"] for r in peer.requests],
                    terminal=[r["session"]["terminal"] for r in peer.requests if r["type"] == "open_shell"])
    finally:
        cleanup_owned([child], peer, (slave, master, error_slave, error_master))


def terminal_peer(code):
    def behavior(conn, request, peer):
        send(conn, {"type": "shell_ready"})
        send(conn, {"type": "stdout", "bytes": list(b"out\n")})
        send(conn, {"type": "stderr", "bytes": list(b"err\n")})
        # Actual controller EOF must not disable output/terminal reception.
        peer.frames.append(receive(conn))
        send(conn, {"type": "stdin_closed"})
        send(conn, {"type": "exited", "code": code})
    return behavior


def paused_pager(binary, root, expected):
    ready = threading.Event()
    release = threading.Event()
    def behavior(conn, request, peer):
        send(conn, {"type": "shell_ready"})
        frame = receive(conn); peer.frames.append(frame)
        assert frame == {"type": "stdin_eof"}, frame
        ready.set(); assert release.wait(10)
        for _ in range(244):
            send(conn, {"type": "stdout", "bytes": [0] * 16384})
        send(conn, {"type": "stdout", "bytes": [0] * (4_000_000 - 244 * 16384)})
        send(conn, {"type": "exited", "code": expected})
    peer = Peer(root, behavior)
    env = environment(root, peer)
    child = subprocess.Popen([str(binary), "-c", "fixture"], cwd=root, env=env,
                             stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             start_new_session=True)
    data = root / "pager-output"
    pager = subprocess.Popen([sys.executable, "-c", "import pathlib,sys; pathlib.Path(sys.argv[1]).write_bytes(sys.stdin.buffer.read())", str(data)],
                             stdin=child.stdout, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
                             start_new_session=True)
    child.stdout.close(); child.stdout = None
    try:
        assert ready.wait(10), peer.errors
        pager.send_signal(signal.SIGSTOP)
        release.set()
        time.sleep(7)
        assert child.poll() is None, "healthy paused pager lost controller"
        pager.send_signal(signal.SIGCONT)
        _, stderr = child.communicate(timeout=20)
        pager_error = pager.communicate(timeout=10)[1]
        peer.close()
        assert not peer.errors and not pager_error and pager.returncode == 0, (peer.errors, pager_error)
        received = data.read_bytes()
        assert len(received) == 4_000_000 and received == bytes(4_000_000), len(received)
        assert child.returncode == expected, (child.returncode, stderr)
        return dict(code=child.returncode, bytes=len(received), sha256=hashlib.sha256(received).hexdigest(),
                    pause_seconds=7, controller_pid=child.pid, pager_pid=pager.pid, frames=peer.frames)
    finally:
        release.set()
        cleanup_owned([pager, child], peer)


def terminal_reference(binary, bash, root):
    """Controlling-TTY GNU/candidate observations; this peer is not stock."""
    probe = ("import os,sys,json; f={'stdin_tty':os.isatty(0),'stdout_tty':os.isatty(1),'stderr_tty':os.isatty(2)}; "
             "f['foreground']=(os.tcgetpgrp(0)==os.getpgrp()) if os.isatty(0) else None; "
             "print('PROBE_READY',flush=True); s=sys.stdin.buffer.readline(); f['input_hex']=s.hex(); "
             "print('FACTS:'+json.dumps(f),flush=True); sys.stderr.buffer.write(b'err:'+s); sys.exit(29)")
    def behavior(conn, request, peer):
        assert request['session']['terminal'] is False
        child = subprocess.Popen([sys.executable, '-I', '-S', '-c', probe], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        try:
            send(conn, {'type':'shell_ready'})
            send(conn, {'type':'stdout','bytes':list(child.stdout.readline())})
            frame = receive(conn); assert frame['type']=='stdin', frame
            child.stdin.write(bytes(frame['bytes'])); child.stdin.close(); child.stdin=None
            out, err = child.communicate(timeout=10)
            send(conn, {'type':'stdout','bytes':list(out)}); send(conn, {'type':'stderr','bytes':list(err)})
            send(conn, {'type':'exited','code':child.returncode})
        finally:
            reap(child)
    peer = Peer(root, behavior); env = environment(root, peer); results = {}
    try:
        for label, argv in [('GNU', [str(bash), '--noprofile', '--norc', '-c', 'exec '+shlex.join([sys.executable,'-I','-S','-c',probe])]),
                            ('candidate-private-peer', [str(binary), '-c', 'tty-facts-fixture'])]:
            master, slave = pty.openpty()
            launcher = [sys.executable,'-I','-S','-c', 'import fcntl,os,sys,termios; fcntl.ioctl(0,termios.TIOCSCTTY,0); os.execv(sys.argv[1],sys.argv[1:])',*argv]
            child = subprocess.Popen(launcher, cwd=root, env=env, stdin=slave,stdout=slave,stderr=subprocess.PIPE,start_new_session=True)
            stdout = bytearray(); started=time.monotonic()
            try:
                deadline=time.monotonic()+10
                while b'PROBE_READY' not in stdout:
                    left=deadline-time.monotonic()
                    assert left>0 and select.select([master],[],[],left)[0], 'TTY probe not ready'
                    stdout.extend(os.read(master,65536))
                assert os.tcgetpgrp(master)==child.pid
                os.write(master,b'witness\n')
                _, stderr, tty_outputs=communicate_with_ttys(child, [master], timeout=15)
                os.close(slave);slave=None
                stdout.extend(tty_outputs[master]);stdout.extend(drain_pty(master))
                facts=json.loads(next(line[6:] for line in bytes(stdout).splitlines() if line.startswith(b'FACTS:')))
                assert child.returncode==29 and facts['input_hex']==b'witness\n'.hex() and stderr==b'err:witness\n', (facts,stderr,child.returncode)
                results[label]=dict(facts=facts,status=child.returncode,stdout_hex=bytes(stdout).hex(),stderr_hex=stderr.hex(),seconds=time.monotonic()-started)
            finally:
                cleanup_owned([child], None, (slave, master))
        assert results['GNU']['facts']=={'stdin_tty':True,'stdout_tty':True,'stderr_tty':False,'foreground':True,'input_hex':b'witness\n'.hex()}
        assert results['candidate-private-peer']['facts']=={'stdin_tty':False,'stdout_tty':False,'stderr_tty':False,'foreground':None,'input_hex':b'witness\n'.hex()}
        assert not peer.errors, peer.errors
        results['classification']='deliberate divergence: redirected stderr uses pipes, not terminal parity; bytes/actual29 preserved; not stock proof'
        results['GNU_sha256']=hashlib.sha256(Path(bash).read_bytes()).hexdigest()
        return results
    finally:
        peer.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--marsh", required=True)
    parser.add_argument("--evidence", required=True)
    parser.add_argument("--bash", default="/bin/bash", help="GNU reference executable for controlling-TTY classification")
    args = parser.parse_args()
    binary = Path(args.marsh).resolve(strict=True)
    evidence = Path(args.evidence).resolve(); evidence.mkdir(parents=True, exist_ok=True)
    report = dict(kind="real-cli-private-peer-no-stock", binary=str(binary),
                  sha256=hashlib.sha256(binary.read_bytes()).hexdigest(), outcome="failed", cases={})
    try:
        # Darwin's long default TMPDIR can exceed sockaddr_un.sun_path after
        # adding a descriptive journey name. The private caller tree stays short.
        with tempfile.TemporaryDirectory(prefix="marsh-transport-", dir="/tmp") as tmp:
            root = Path(tmp)
            def case(name, behavior, arguments, expected, **options):
                folder = root / name; folder.mkdir()
                report["cases"][name] = run_case(binary, folder, behavior, arguments, expected, **options)
            for code in [0, 1, 42]:
                case(f"actual-{code}", terminal_peer(code), ["-c", "fixture"], code)
            for code in [0, 1, 42]:
                def immediate(conn, request, peer, code=code):
                    send(conn, {"type": "shell_ready"})
                    send(conn, {"type": "exited", "code": code})
                for attempt in range(3):
                    case(f"early-close-{code}-{attempt}", immediate, ["-c", "fixture"], code)
            for code in ["unsupported_signal", "signal_delivery"]:
                def control_error(conn, request, peer, code=code):
                    send(conn, {"type": "shell_ready"})
                    send(conn, {"type": "control_error", "code": code, "operation": "signal", "message": "control rejected, shell remains attached"})
                    send(conn, {"type": "exited", "code": 42})
                case(code, control_error, ["-c", "fixture"], 42)
                assert "control rejected" in report["cases"][code]["stderr"]
            for name in ["cleanup-uncertain", "quarantined"]:
                def failed(conn, request, peer, name=name):
                    send(conn, {"type": "failed", "message": name})
                case(name, failed, ["-c", "fixture"], 125)
            def no_open(conn, request, peer):
                raise AssertionError("unexpected shell launch after rejected attachment")
            no_open.reject_attach = True
            case("attach-failure", no_open, ["-c", "fixture"], 125)
            no_open.drop_status = True
            case("inspection-error-not-125", no_open, ["status", "--json"], 1)
            def truncated(conn, request, peer):
                conn.sendall(struct.pack(">I", 100) + b'{"type":')
            case("transport-loss", truncated, ["-c", "fixture"], 125)
            def redirect(conn, request, peer):
                assert request["session"]["terminal"] is False, "stderr redirection selected PTY"
                send(conn, {"type": "shell_ready"})
                send(conn, {"type": "stdout", "bytes": list(b"out\n")})
                send(conn, {"type": "stderr", "bytes": list(b"err\n")})
                send(conn, {"type": "exited", "code": 0})
            case("terminal-stderr-redirect", redirect, ["-c", "fixture"], 0, terminal=True, redirect=True)
            r = report["cases"]["terminal-stderr-redirect"]
            assert "err" not in r["stdout"] and r["stderr"] == "err\n", r
            case("different-tty-stderr-redirect", redirect, ["-c", "fixture"], 0, terminal=True, separate_tty=True)
            r = report["cases"]["different-tty-stderr-redirect"]
            assert "err" not in r["stdout"] and r["stderr"] == "err\r\n", r
            def cold(conn, request, peer):
                peer.preparing = True
                send(conn, {"type": "cold_boot", "kit": "shell"})
                try:
                    frame = receive(conn)
                    raise AssertionError(f"preparation queued future guest input/control: {frame}")
                except EOFError:
                    peer.frames.append({"preparation_aborted": True})
                    time.sleep(0.2)
                    peer.cleanup.set()
            case("cold-ctrl-c", cold, [], 130, terminal=True, interrupt=True)
            case("usage", terminal_peer(0), ["--load"], 2)
            for code in [42, 0]:
                folder = root / f"pager-{code}"; folder.mkdir()
                report["cases"][f"pager-{code}"] = paused_pager(binary, folder, code)
            folder=root/'tty-reference'; folder.mkdir()
            report['cases']['tty-reference']=terminal_reference(binary, Path(args.bash).resolve(strict=True), folder)
        for name, result in report["cases"].items():
            if "open_shell" in result.get("request_types", []):
                assert "detach_shell" not in result["request_types"], (name, "client detached server-owned attachment")
        report["outcome"] = "passed"
    except Exception as error:
        report["error"] = repr(error)
    finally:
        (evidence / "result.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return 0 if report["outcome"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
