#!/usr/bin/python3
"""External FIFO/stop fixture for bash_jobs_cli.py; never implements shell logic."""
import json
import os
from pathlib import Path
import signal
import sys
import time


if os.getpid() <= 1:
    raise RuntimeError('fixture must be an owned child, never PID1')

if len(sys.argv) > 1 and sys.argv[1] == '--from-parent-dir':
    del sys.argv[1]
    os.chdir('..')


def identity():
    try:
        foreground = os.tcgetpgrp(0)
    except OSError:
        foreground = None
    return {'pid': os.getpid(), 'ppid': os.getppid(), 'pgid': os.getpgrp(),
            'sid': os.getsid(0), 'foreground': foreground}


if sys.argv[1] == '--observe-cats':
    proof_path = Path('pid-proof.json')
    proof = json.loads(proof_path.read_bytes()) if proof_path.exists() else {}
    expected = os.stat('/usr/bin/cat')
    reported = [os.fsencode(arg) for arg in sys.argv[2:]] if len(sys.argv) > 2 else Path('pids').read_bytes().splitlines()
    for text in reported:
        pid = int(text)
        if str(pid) in proof:
            continue  # This earlier latched child was already independently witnessed.
        if pid <= 1 or pid == os.getppid():
            raise RuntimeError('reported job PID is not a child')
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            try:
                fields = Path(f'/proc/{pid}/stat').read_bytes().rsplit(b') ', 1)[1].split()
                executable = os.stat(f'/proc/{pid}/exe')
            except FileNotFoundError:
                time.sleep(.005)
                continue
            if int(fields[3]) != os.getsid(0):
                raise RuntimeError('reported job PID belongs to another session')
            if (executable.st_dev, executable.st_ino) == (expected.st_dev, expected.st_ino):
                proof[str(pid)] = {'pid': pid, 'ppid': int(fields[1]), 'pgid': int(fields[2]),
                                   'sid': int(fields[3]), 'starttime': int(fields[19])}
                break
            time.sleep(.005)
        else:
            raise RuntimeError('reported job PID did not execute the cat fixture')
    proof_path.write_text(json.dumps(proof))
    sys.exit(0)

if sys.argv[1] == '--fg-pipe':
    _, stage, variant, status, gate = sys.argv[1:]
    Path(f'pipe-{stage}.json').write_text(json.dumps(identity()))
    os.kill(os.getpid(), signal.SIGSTOP)
    Path(f'fg-{stage}-resumed.json').write_text(json.dumps(identity()))
    if (variant == 'last' and stage == 'right') or (variant == 'first' and stage == 'left'):
        Path(f'fg-{stage}-restop').write_bytes(b'ready')
        os.kill(os.getpid(), signal.SIGSTOP)
        Path(f'pipe-{stage}-after.json').write_text(json.dumps(identity()))
    if gate != '-':
        with open(gate, 'rb') as fifo:
            data = fifo.read()
        Path(f'pipe-{stage}-input').write_bytes(data)
    os._exit(int(status))

if sys.argv[1] in ('--pipe', '--pipe-stop'):
    mode, stage, status, gate = sys.argv[1:]
    Path(f'pipe-{stage}.json').write_text(json.dumps(identity()))
    if mode == '--pipe-stop':
        # Stop only after the other stage has really exited. This is an OS
        # observation barrier, not a sleep-based bet about pipeline scheduling.
        peer = 'left' if stage == 'right' else 'right'
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            try:
                peer_info = json.loads(Path(f'pipe-{peer}.json').read_bytes())
            except (FileNotFoundError, json.JSONDecodeError):
                time.sleep(.005)
                continue
            if peer_info['pid'] <= 1 or peer_info['sid'] != os.getsid(0):
                raise RuntimeError('not an owned pipeline peer')
            try:
                state = Path(f'/proc/{peer_info["pid"]}/stat').read_bytes().rsplit(b') ', 1)[1].split()[0]
            except FileNotFoundError:
                break
            if state == b'Z':
                break
            time.sleep(.005)
        else:
            raise RuntimeError('peer did not exit before stop')
        os.kill(os.getpid(), signal.SIGSTOP)
        Path(f'pipe-{stage}-after.json').write_text(json.dumps(identity()))
    with (os.fdopen(os.dup(0), 'rb') if gate == '-' else open(gate, 'rb')) as source:
        data = source.read()
    if mode == '--pipe':
        os.write(1, data)
    else:
        Path(f'pipe-{stage}-input').write_bytes(data)
    os._exit(int(status))

if sys.argv[1] in ('--observe-pipeline', '--observe-all-exited', '--observe-both-stopped'):
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        try:
            stages = [json.loads(Path(f'pipe-{stage}.json').read_bytes()) for stage in ('left', 'right')]
        except (FileNotFoundError, json.JSONDecodeError):
            time.sleep(.005)
            continue
        if not all(p['sid'] == os.getsid(0) and p['pid'] > 1 for p in stages):
            raise RuntimeError('not the owned pipeline')
        if sys.argv[1] == '--observe-both-stopped':
            states = [Path(f'/proc/{p["pid"]}/stat').read_bytes().rsplit(b') ', 1)[1].split()[0] for p in stages]
            if not all(state == b'T' for state in states):
                time.sleep(.005)
                continue
        if sys.argv[1] == '--observe-all-exited':
            finished = True
            for child in stages:
                try:
                    state = Path(f'/proc/{child["pid"]}/stat').read_bytes().rsplit(b') ', 1)[1].split()[0]
                except FileNotFoundError:
                    continue
                finished &= state == b'Z'
            if not finished:
                time.sleep(.005)
                continue
        sys.exit(0)
    raise RuntimeError('pipeline observation did not finish')

if sys.argv[1] in ('--observe-resumed', '--observe-resumed-stage'):
    stage = sys.argv[2] if len(sys.argv) > 2 else None
    name = f'pipe-{stage}-after.json' if stage else 'child-after.json'
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        try:
            child = json.loads(Path(name).read_bytes())
        except (FileNotFoundError, json.JSONDecodeError):
            time.sleep(.005)
            continue
        if child['pid'] <= 1 or child['sid'] != os.getsid(0):
            raise RuntimeError('not an owned resumed child')
        fields = Path(f'/proc/{child["pid"]}/stat').read_bytes().rsplit(b') ', 1)[1].split()
        if int(fields[3]) != os.getsid(0):
            raise RuntimeError('resumed child session changed')
        if fields[0] not in (b'T', b't', b'Z'):
            Path('resumed-barrier').write_bytes(b'continued')
            sys.exit(0)
        time.sleep(.005)
    raise RuntimeError('child did not acknowledge continuation')

if sys.argv[1] in ('--observe-exit', '--observe-stop', '--observe-exit-stage', '--observe-running'):
    stage = sys.argv[2] if sys.argv[1] == '--observe-exit-stage' else None
    pid = (json.loads(Path(f'pipe-{stage}.json').read_bytes())['pid'] if stage and len(sys.argv) == 3
           else int(sys.argv[3] if stage else sys.argv[2]))
    stopped = sys.argv[1] == '--observe-stop'
    running = sys.argv[1] == '--observe-running'
    if pid <= 1:
        raise RuntimeError('not an owned fixture PID')
    if stopped or running:
        if os.getsid(pid) != os.getsid(0):
            raise RuntimeError('not an owned fixture session')
    else:
        before = json.loads(Path(f'pipe-{stage}.json' if stage else 'child-before.json').read_bytes())
        if before['pid'] != pid or before['sid'] != os.getsid(0):
            raise RuntimeError('not the latched child')
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        try:
            state = Path(f'/proc/{pid}/stat').read_bytes().rsplit(b') ', 1)[1].split()[0]
        except FileNotFoundError:
            break
        if (running and state not in (b'T', b't', b'Z')) or (not running and state == (b'T' if stopped else b'Z')):
            break
        time.sleep(.005)
    else:
        raise RuntimeError('fixture did not exit')
    sys.exit(0)

gate, status, mode = sys.argv[1:]
Path('child-before.json').write_text(json.dumps(identity()))
if mode == 'stop' or mode == 'stop-twice':
    os.kill(os.getpid(), signal.SIGSTOP)
    if mode == 'stop-twice':
        os.kill(os.getpid(), signal.SIGSTOP)
elif mode.startswith('stop-'):
    os.kill(os.getpid(), getattr(signal, 'SIG' + mode.removeprefix('stop-')))
Path('child-after.json').write_text(json.dumps(identity()))
with open(gate, 'rb') as fifo:
    data = fifo.read()
Path('child-input').write_bytes(data)
if mode.startswith('signal-'):
    sig = getattr(signal, 'SIG' + mode.removeprefix('signal-'))
    signal.signal(sig, signal.SIG_DFL)
    os.kill(os.getpid(), sig)
if data == b'INT':
    # Restore default explicitly: background jobs otherwise inherit ignored SIGINT.
    signal.signal(signal.SIGINT, signal.SIG_DFL)
    os.kill(os.getpid(), signal.SIGINT)
os._exit(int(status))
