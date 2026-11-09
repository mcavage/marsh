#!/usr/bin/env python3
"""One contender for a lock file (HARNESS-01 c1 and the lock tests): wait for the gate file, try the shared VM lock
helper once on VMLOCK_TEST_PATH, print `acquired` or `held`, and keep a won lock for a moment so a second winner would
be visible."""
import pathlib
import sys
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))

import nfx_lock  # noqa: E402

gate = pathlib.Path(sys.argv[1])
while not gate.exists():
    time.sleep(0.005)
try:
    with nfx_lock.hold("contender", 0):
        print("acquired", flush=True)
        time.sleep(3)
except nfx_lock.LockHeld:
    print("held", flush=True)
