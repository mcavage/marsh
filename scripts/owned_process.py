"""Bounded host-tool execution with signal forwarding to one owned session only."""
import os
import signal
import subprocess
import threading
import time


def run(argv, *, timeout=1800, check=False, capture_output=False, text=False, **kwargs):
    if 'start_new_session' in kwargs:
        if kwargs.pop('start_new_session') is not True:
            raise ValueError('host tools must use a new owned process session')
    if capture_output:
        if 'stdout' in kwargs or 'stderr' in kwargs:
            raise ValueError('capture_output conflicts with explicit streams')
        kwargs.update(stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    process = subprocess.Popen(argv, start_new_session=True, text=text, **kwargs)
    interrupted = []
    previous = {}
    started = time.monotonic()
    finished = False
    def forward(number, _frame):
        if not interrupted:
            interrupted.append((number, time.monotonic()))
        if process.pid <= 1 or process.pid == os.getpgrp():
            raise RuntimeError('refusing to signal a non-owned process group')
        try:
            os.killpg(process.pid, number)
        except ProcessLookupError:
            pass
    try:
        if threading.current_thread() is threading.main_thread():
            for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
                previous[number] = signal.signal(number, forward)
        while True:
            try:
                stdout, stderr = process.communicate(timeout=0.2)
                finished = True
                break
            except subprocess.TimeoutExpired:
                now = time.monotonic()
                if interrupted and now - interrupted[0][1] > 30:
                    forward(signal.SIGKILL, None)
                if now - started > timeout:
                    raise subprocess.TimeoutExpired(argv, timeout)
        result = subprocess.CompletedProcess(argv, process.returncode, stdout, stderr)
        if interrupted:
            raise InterruptedError(f'host tool interrupted by signal {interrupted[0][0]}')
        if check:
            result.check_returncode()
        return result
    finally:
        if not finished:
            forward(signal.SIGTERM, None)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                forward(signal.SIGKILL, None)
                process.wait(timeout=10)
        for number, handler in previous.items():
            signal.signal(number, handler)
