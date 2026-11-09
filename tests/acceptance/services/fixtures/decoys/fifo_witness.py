#!/usr/bin/env python3
"""A Mac-side FIFO that detects any reader (CAP-10 c2, SEC-24 c2).

    fifo_witness.py start PATH LOG [--timeout S]   # create the FIFO, watch in the background
    fifo_witness.py status LOG                     # print the verdict JSON; exit 1 if a reader was seen
    fifo_witness.py stop LOG                       # stop watching, remove the FIFO

A watcher process polls a non-blocking write-open of PATH: that open only
succeeds when some process has the FIFO open for reading. The first success is
a "reader seen" verdict (time and nothing else is recorded). The FIFO holds no
data and no reader of the harness's own ever opens it. Standard library only.
"""
import json
import os
import signal
import subprocess
import sys
import time


def watch(path: str, log: str, timeout: float) -> None:
    state = {"path": path, "pid": os.getpid(), "started": time.time(), "reader_seen": False}
    stop = {"now": False}
    signal.signal(signal.SIGTERM, lambda *_: stop.update(now=True))
    deadline = time.monotonic() + timeout

    def save():
        with open(log, "w") as handle:
            json.dump(state, handle)

    save()
    while not stop["now"] and time.monotonic() < deadline:
        try:
            fd = os.open(path, os.O_WRONLY | os.O_NONBLOCK)
        except OSError:
            time.sleep(0.02)
            continue
        os.close(fd)
        state.update(reader_seen=True, seen_at=time.time())
        save()
        break
    state["ended"] = time.time()
    save()


def main() -> int:
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    action = sys.argv[1]
    if action == "start":
        path, log = sys.argv[2], sys.argv[3]
        timeout = float(sys.argv[sys.argv.index("--timeout") + 1]) if "--timeout" in sys.argv else 3600
        if os.path.lexists(path):
            os.unlink(path)
        os.mkfifo(path, 0o600)
        subprocess.Popen([sys.executable, __file__, "_watch", path, log, str(timeout)], start_new_session=True,
                         stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(100):  # the watcher writes its first record
            if os.path.exists(log):
                break
            time.sleep(0.02)
        return 0
    if action == "_watch":
        watch(sys.argv[2], sys.argv[3], float(sys.argv[4]))
        return 0
    state = json.load(open(sys.argv[2]))
    if action == "status":
        print(json.dumps(state))
        return 1 if state["reader_seen"] else 0
    if action == "stop":
        try:
            os.kill(state["pid"], signal.SIGTERM)
        except ProcessLookupError:
            pass
        if os.path.lexists(state["path"]):
            os.unlink(state["path"])
        return 0
    raise SystemExit(__doc__)


if __name__ == "__main__":
    sys.exit(main())
