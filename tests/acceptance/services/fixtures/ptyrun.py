#!/usr/bin/env python3
"""Run a command on a pseudo-terminal, for `docker run -it` / `docker exec -it`
controls (SEC-03, EP-02). Python 3 standard library only; runs in a job.

    ptyrun.py [--input TEXT] [--timeout S] [--rows R --cols C] -- CMD [ARG...]

Starts CMD with a PTY as its stdin/stdout/stderr, optionally types TEXT once
CMD has produced output (or after 0.5 s), and relays everything it prints to
stdout. TEXT understands \\n, \\r, \\t, \\\\ and \\xNN escapes. The exit status
is CMD's (128+signal when killed, 124 on timeout). `script -qc` does the same
job where util-linux is wanted; this needs nothing but python3.
"""
import argparse
import codecs
import fcntl
import os
import pty
import select
import signal
import struct
import sys
import termios
import time


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--input", default="")
    parser.add_argument("--timeout", type=float, default=60)
    parser.add_argument("--rows", type=int, default=24)
    parser.add_argument("--cols", type=int, default=80)
    parser.add_argument("cmd", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.cmd[1:] if args.cmd[:1] == ["--"] else args.cmd
    if not command:
        parser.error("a command is required")
    typed = codecs.decode(args.input, "unicode_escape").encode("latin-1") if args.input else b""

    pid, fd = pty.fork()
    if pid == 0:
        try:
            os.execvp(command[0], command)
        except OSError as error:
            os.write(2, f"ptyrun: {error}\n".encode())
            os._exit(127)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", args.rows, args.cols, 0, 0))
    start = time.monotonic()
    sent = not typed
    status = None
    while True:
        remaining = args.timeout - (time.monotonic() - start)
        if remaining <= 0:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
            return 124
        ready, _, _ = select.select([fd], [], [], min(0.25, remaining))
        if ready:
            try:
                data = os.read(fd, 65536)
            except OSError:
                data = b""
            if not data:
                break
            sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()
        if not sent and (ready or time.monotonic() - start > 0.5):
            os.write(fd, typed)
            sent = True
    _, status = os.waitpid(pid, 0)
    return os.waitstatus_to_exitcode(status) if os.waitstatus_to_exitcode(status) >= 0 else 128 - os.waitstatus_to_exitcode(status)


if __name__ == "__main__":
    sys.exit(main())
