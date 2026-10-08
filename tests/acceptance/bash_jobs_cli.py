#!/usr/bin/env python3
"""Byte-exact public CLI jobs/fg/bg differential, with FIFO-latched children.

Linux evidence runner (uses /proc start times and subreaping for exact cleanup).
The first --shell must be GNU Bash 5.3.20. No candidate internals are imported.
Job-control cases get a real controlling PTY, with stdout/stderr kept separate.
Only reported child PID identities are normalized, never command/status bytes.
"""
import argparse
import ctypes
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import termios
import time
from dataclasses import dataclass


@dataclass
class Case:
    name: str
    probes: list[bytes]
    children: int = 1
    tty: bool = False
    raw_name: bool = False
    lifecycle: str = ''
    child_mode: bytes = b'stop'
    child_status: bytes = b'7'
    monitor: bool = False
    directory: bytes | None = None
    home_mode: str = 'root'
    pipefail: bool = True


def corpus():
    def substitution(command):
        return b'v=$(' + command + b'); printf "SUBSTATUS:%s:<%s>\\n" "$?" "$v"'
    cases = [
        Case('tty-fg-all-stage-restop-last-no-pipefail', [b'jobs -l', b'bg %1', b'jobs -n'], 2, True, lifecycle='fg-restop-last', pipefail=False),
        Case('tty-fg-all-stage-restop-first-no-pipefail', [b'jobs -l', b'bg %1', b'jobs -n'], 2, True, lifecycle='fg-restop-first', pipefail=False),
        Case('tty-fg-all-stage-restop-last', [b'jobs -l', b'bg %1', b'jobs -n'], 2, True, lifecycle='fg-restop-last'),
        Case('tty-fg-all-stage-restop-first', [b'jobs -l', b'bg %1', b'jobs -n'], 2, True, lifecycle='fg-restop-first'),
        Case('job-id-reuse', [b'jobs', b'printf done >a; wait "$p0"', b'cat a >/dev/null & p0=$!; printf "%s\\n" "$p0" >>pids', b'jobs', b'cat b >/dev/null & p1=$!; printf "%s\\n" "$p1" >>pids', b'printf done >a; wait "$p0"', b'cat a >/dev/null & p0=$!; printf "%s\\n" "$p0" >>pids', b'jobs', b'printf done >a; wait "$p0"; printf done >b; wait "$p1"', b'cat a >/dev/null & p0=$!; printf "%s\\n" "$p0" >>pids', b'jobs'], 2, lifecycle='reuse'),
        Case('listed-done-wait-next', [b'jobs', b'wait -n', b'wait "$p0"'], lifecycle='done'),
        Case('listed-done-job-id-reuse', [b'jobs', b'wait -n', b'wait "$p0"', b'cat a >/dev/null & p1=$!; printf "%s\\n" "$p1" >>pids', b'jobs', b'printf done >a; wait "$p1"', b'wait "$p0"'], lifecycle='done'),
        *[Case('signal-label-' + sig, [b'jobs -l', b'jobs', b'wait "$p0"'], lifecycle='signaled', child_mode=b'signal-' + sig.encode(), child_status=b'0') for sig in ('TERM', 'PIPE')],
        *[Case('exit-label-' + code, [b'jobs -l', b'jobs', b'wait "$p0"'], lifecycle='done', child_status=code.encode()) for code in ('141', '143')],
        Case('completed-pipefail-unlisted', [b'wait "$p0"', b'wait "$p0"'], lifecycle='completed-pipeline'),
        Case('completed-pipefail-normal', [b'jobs', b'wait "$p0"'], lifecycle='completed-pipeline'),
        Case('completed-pipefail-long', [b'jobs -l', b'wait "$p0"'], lifecycle='completed-pipeline'),
        Case('first-done-last-running', [b'jobs -l', b'jobs', b'jobs -p'], lifecycle='first-done-pipeline'),
        *[Case('directory-list-' + label, [b'jobs', b'jobs -l', b'jobs -p', b'jobs -n'], directory=directory, home_mode=home)
          for label, directory, home in [('home', b'origin space', 'root'), ('absolute', b'origin space', 'empty'), ('boundary', b'origin space', 'prefix'), ('raw', b'origin-\xff\xfe', 'root')]],
        *[Case('directory-' + action + '-' + label, [b'jobs -l', action.encode() + b' %1', b'jobs -n'], tty=True, lifecycle=action, directory=directory)
          for action in ('fg', 'bg') for label, directory in [('text', b'origin space'), ('raw', b'origin-\xff\xfe')]],
        Case('nonmonitor-substitution-context', [substitution(c) for c in (b'fg -z', b'bg -z', b'set -m; fg -z', b'set -m; bg -z', b'set -m; fg %1', b'set -m; bg %1', b'set -m; fg %missing', b'set -m; (fg %1)')]),
        Case('tty-substitution-context', [substitution(c) for c in (b'fg -z', b'bg -z', b'fg %missing', b'bg %missing', b'set +m; fg %1', b'set -m; (fg %1)')], tty=True),
        Case('tty-partial-pipeline-last-stopped', [b'jobs -l', b'bg %1'], tty=True, lifecycle='partial-last'),
        Case('tty-partial-pipeline-first-stopped', [b'jobs -l', b'bg %1'], tty=True, lifecycle='partial-first'),
        Case('two-done-wait-retention', [b'jobs -p', b'jobs %1', b'wait "$p0"', b'wait -n', b'wait "$p1"', b'jobs'], 2, lifecycle='done-pair'),
        Case('tty-stopped-current-priority', [b'jobs', b'jobs %', b'jobs %-', b'bg %1', b'jobs'], 2, True, lifecycle='bg-with-other'),
        Case('markers-after-completion', [b'jobs', b'printf done >c; wait "$p2"', b'jobs', b'jobs %', b'jobs %-', b'printf done >b; wait "$p1"', b'jobs', b'jobs %-'], 3, lifecycle='markers'),
        Case('tty-fg-stops-again', [b'fg %1', b'jobs -n', b'bg %1', b'jobs -n'], tty=True, lifecycle='bg', child_mode=b'stop-twice'),
        *[Case('tty-stopped-' + sig, [b'jobs -s', b'jobs -l', b'jobs -n', b'bg %1'], tty=True, lifecycle='bg', child_mode=b'stop-' + sig.encode()) for sig in ('TSTP', 'TTIN', 'TTOU')],
        Case('pipeline-listing', [b'jobs', b'jobs -l', b'jobs -p', b'jobs -n', b'jobs %?left', b'jobs %?right', b'jobs %?LEFT', b'jobs %./LATCH'], lifecycle='pipeline'),
        Case('tty-pipeline-listing', [b'jobs', b'jobs -l', b'jobs -p', b'jobs -n', b'jobs %?left', b'jobs %?right', b'jobs %?LEFT', b'jobs %./LATCH'], tty=True, lifecycle='pipeline'),
        Case('tty-completed-fg-bg', [b'fg %1', b'bg %1', substitution(b'fg %1'), substitution(b'bg %1'), b'jobs', b'wait "$p0"'], tty=True, lifecycle='done'),
        Case('done-notification-wait', [b'jobs -n', b'jobs -n', b'jobs', b'wait "$p0"', b'jobs'], lifecycle='done'),
        Case('done-pid-preserves-notification', [b'jobs -p', b'jobs -n', b'wait "$p0"'], lifecycle='done'),
        Case('done-filter-preserves-notification', [b'jobs -r', b'jobs -n', b'wait "$p0"'], lifecycle='done'),
        Case('monitor-fg-without-terminal', [b'fg %1', b'jobs'], lifecycle='fg', monitor=True),
        Case('monitor-bg-without-terminal', [b'bg %1', b'jobs -n'], lifecycle='bg', monitor=True),
        Case('tty-fg-exit130-not-signal', [b'fg %1', b'jobs'], tty=True, lifecycle='fg', child_status=b'130'),
        Case('tty-fg-background-operand', [b"fg %1 '&'", b'jobs -n'], tty=True, lifecycle='bg'),
        Case('tty-stop-bg-wait', [b'jobs -n', b'jobs -s', b'bg %1', b'jobs -n', b'jobs -n', b'bg %1'], tty=True, lifecycle='bg'),
        Case('tty-stop-fg-write-error', [b'fg %1 >/dev/full', b'jobs'], tty=True, lifecycle='fg'),
        Case('tty-stop-bg-write-error', [b'bg %1 >/dev/full', b'jobs -n'], tty=True, lifecycle='bg'),
        Case('tty-stop-fg-status', [b'jobs -n', b'fg %1', b'jobs', b'jobs -n'], tty=True, lifecycle='fg'),
        Case('tty-stop-fg-signal', [b'fg %1', b'jobs'], tty=True, lifecycle='signal'),
        Case('notification-write-errors', [b'jobs -n >/dev/full', b'jobs -n', b'jobs 1>&-', b'jobs -n']),
        Case('inherited-notifications', [b'jobs -n | /usr/bin/cat', b'jobs -n', b'jobs -n', b'v=$(jobs); printf "%s\\n" "$v"', b'jobs -n']),
        Case('notification-once', [b'jobs -n', b'jobs -n', b'jobs', b'jobs -n']),
        Case('notification-after-pid', [b'jobs -p', b'jobs -n', b'jobs -n']),
        Case('notification-after-long', [b'jobs -l', b'jobs -n']),
        Case('notification-mode-order', [b'jobs -pn', b'jobs -pn', b'jobs -np', b'jobs -nl', b'jobs -ln', b'jobs -n']),
        Case('selector-boundaries', [b'jobs %1tail', b'jobs %01tail', b'jobs %+tail', b'jobs %-tail', b'jobs %%tail', b'jobs -- +', b'jobs -- -', b'jobs 1tail', b'jobs %?', b'jobs -- -zzz']),
        Case('notification-filtered', [b'jobs -ns', b'jobs -n', b'jobs -n']),
        Case('notification-selected', [b'jobs -n %1', b'jobs -n %1', b'jobs -n %2', b'jobs -n'], 2),
        Case('selection', [b'jobs %', b'jobs %%', b'jobs %+', b'jobs %-', b'jobs %1', b'jobs 1', b'jobs 01', b'jobs %01', b'jobs cat', b'jobs %cat', b'jobs %?a', b'jobs ?a', b'jobs -n %1 %1']),
        Case('ambiguity-order', [b'jobs %cat', b'jobs %?cat', b'jobs %?', b'jobs %cat %1', b'jobs %1 %cat', b'jobs %1 %1', b'jobs %2 %1'], 2),
        Case('missing', [b'jobs missing', b'jobs %missing', b'jobs %0', b'jobs %99999999999999999999999999', b'jobs ""', b'jobs %-', b'jobs %1 missing %1']),
        Case('format-filter-order', [b'jobs -lp', b'jobs -pl', b'jobs -rs', b'jobs -sr', b'jobs -s %1', b'jobs -r %1', b'jobs -p %1', b'jobs -nn', b'jobs -pp', b'jobs %1 -l']),
        Case('empty', [b'jobs', b'jobs -n', b'jobs %', b'jobs %%', b'jobs %+', b'jobs %-', b'fg', b'bg'], 0),
        Case('disabled', [b'fg', b'fg %1', b'fg missing', b'bg', b'bg %1', b'bg missing']),
        Case('disabled-options', [b'fg -z', b'bg -z', b'fg -- %1', b'fg %1 extra', b'bg %1 %1']),
        Case('invalid-options', [b'jobs -z', b'jobs --bad', b'jobs -p -z', b'jobs -- %1', b'jobs -', b'jobs +p']),
        Case('raw-command-and-spec', [b'jobs', b'jobs -l', b'jobs %./cat-\xff', b'jobs %?\xff', b'jobs %?\xfe', b'jobs -n'], raw_name=True),
        Case('raw-error-spec', [b'jobs %\xff', b'jobs \xfe', b'fg %\xff', b'bg %\xfe']),
        Case('x-substitution', [b'jobs -x /usr/bin/printf "<%s>\\n" %1', b'jobs -x /usr/bin/printf "<%s>\\n" %% %+', b'jobs -x /usr/bin/printf "<%s>\\n" ordinary', b'jobs -x /usr/bin/printf "<%s>\\n" %missing', b'jobs -px /usr/bin/true', b'jobs -x', b'jobs -x false', b'jobs -x no-such-job-command', b'jobs -x "\xff"', b'jobs -x /usr/bin/printf "<%s>\\n" \'$(touch should-not-exist)\' \'; touch should-not-exist\'']),
        Case('tty-stop-two-background', [b'jobs -s', b'jobs -n', b'bg %1', b'jobs', b'bg %2', b'jobs'], 2, True, lifecycle='stop-background'),
        Case('tty-uncontrolled-job', [b'set -m', b'fg %1', b'bg %1', b'jobs'], tty=True, lifecycle='uncontrolled'),
        Case('tty-options', [b'fg -z', b'bg -z', b'fg -- %missing', b'bg -- %missing', b'fg -', b'bg -', b'fg %missing ignored', b'bg %missing %other'], 0, True),
        Case('x-boundaries', [b'jobs -x /usr/bin/printf "<%s>\\\\n" %cat', b'jobs -x /usr/bin/printf "<%s>\\\\n" %?a', b'jobs -x /usr/bin/printf "<%s>\\\\n" %cat %missing', b'jobs -x /usr/bin/printf "<%s>\\\\n" 1', b'jobs -nx /usr/bin/true', b'jobs -rx /usr/bin/true'], 2),
        Case('tty-empty', [b'fg', b'bg', b'fg %1', b'bg %1', b'fg 1 extra', b'bg %-'], 0, True),
        Case('tty-inherited-control', [b'(fg %1)', b'(bg %1)', b'v=$(fg %1); printf "SUBSTATUS:%s:<%s>\\n" "$?" "$v"', b'v=$(bg %1); printf "SUBSTATUS:%s:<%s>\\n" "$?" "$v"', b'bg %1 | /usr/bin/cat', b'jobs'], tty=True),
        Case('tty-running-bg', [b'bg', b'bg %1', b'bg 1', b'bg %missing %1', b'jobs -n'], tty=True),
        Case('tty-select-errors', [b'fg %cat', b'bg %cat', b'fg %?cat', b'bg %?cat', b'fg %missing', b'bg %missing'], 2, True),
        Case('tty-raw', [b'jobs', b'bg', b'bg %?\xff', b'fg %\xfe'], tty=True, raw_name=True),
    ]
    return cases


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def normalize_output(data, stream, case, pid_map):
    """Normalize captured identities only in declared PID-bearing output fields.

    In particular PID9 is never a license to rewrite STATUS9, Exit9, job9,
    command arguments, filenames, diagnostics, or arbitrary printf output.
    Invalid long-list PID padding is retained AND fails a separate invariant.
    """
    probe = None
    output_line = 0
    errors = []
    lines = []
    for line in data.splitlines(keepends=True):
        begin = re.fullmatch(rb'BEGIN ([0-9]+)\n', line)
        if stream == 'stdout' and begin:
            probe = int(begin[1])
            output_line = 0
            lines.append(line)
            continue
        if stream == 'stdout' and re.fullmatch(rb'STATUS [0-9]+\n', line):
            probe = None
        mode = None
        if probe is not None and probe < len(case.probes):
            words = case.probes[probe].split()
            if words and words[0] == b'jobs':
                for word in words[1:]:
                    if word == b'--' or not word.startswith(b'-') or word == b'-':
                        break
                    for option in word[1:]:
                        if option in b'lnp':
                            mode = chr(option)
        replacement = line
        if stream == 'stderr':
            launch = re.fullmatch(rb'(\[[0-9]+\] )([0-9]+)\n', line)
            if launch and launch[2] in pid_map:
                replacement = launch[1] + pid_map[launch[2]] + b'\n'
        elif mode == 'p' and line.endswith(b'\n') and line[:-1] in pid_map:
            replacement = pid_map[line[:-1]] + b'\n'
        elif mode == 'l':
            field = re.fullmatch(rb'(\[[0-9]+\][+ -] |     )( *[0-9]+)( [^\n]*)\n', line)
            if field and field[2].lstrip(b' ') in pid_map:
                pid = field[2].lstrip(b' ')
                if field[2] != pid.rjust(5, b' '):
                    errors.append({'probe': probe, 'line_hex': line.hex(), 'invalid_pid_width': True})
                else:
                    replacement = field[1] + pid_map[pid] + field[3] + b'\n'
        elif case.name == 'x-substitution' and probe in (0, 1):
            # These two fixture probes have precisely one/two %-jobspec argv
            # entries. Other printf operands (even literal9) are never PIDs.
            field = re.fullmatch(rb'<([0-9]+)>\n', line)
            if output_line < (1 if probe == 0 else 2) and field and field[1] in pid_map:
                replacement = b'<' + pid_map[field[1]] + b'>\n'
        lines.append(replacement)
        if stream == 'stdout' and probe is not None:
            output_line += 1
    return b''.join(lines), errors


def proc(pid):
    try:
        fields = Path(f'/proc/{pid}/stat').read_bytes().rsplit(b') ', 1)[1].split()
        return {'pid': pid, 'state': fields[0].decode('ascii'), 'ppid': int(fields[1]),
                'pgid': int(fields[2]), 'sid': int(fields[3]), 'starttime': int(fields[19])}
    except FileNotFoundError:
        return None


def session_members(sid):
    return [p for entry in Path('/proc').iterdir() if entry.name.isdecimal()
            and (p := proc(int(entry.name))) and p['sid'] == sid]


def teardown(sid, known):
    """No pattern/group/global signals: PID + starttime + owned SID validation."""
    leaked = session_members(sid)
    for p in leaked:
        known[p['pid']] = p
        current = proc(p['pid'])
        if p['pid'] <= 1 or not current or current['sid'] != sid or current['starttime'] != p['starttime']:
            raise RuntimeError('cleanup identity changed')
        # pidfd pins identity across signal delivery; a reaped PID cannot be reused.
        try:
            fd = os.pidfd_open(p['pid'])
            current = proc(p['pid'])
            if current and current['sid'] == sid and current['starttime'] == p['starttime']:
                signal.pidfd_send_signal(fd, signal.SIGKILL)
            os.close(fd)
        except ProcessLookupError:
            pass
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        for pid in known:
            try:
                os.waitpid(pid, os.WNOHANG)
            except ChildProcessError:
                pass
        if not session_members(sid):
            break
        time.sleep(.005)
    survivors = session_members(sid)
    return {'needed_teardown': leaked, 'survivors': survivors,
            'clean': not leaked and not survivors}


def program(case):
    lines = [b'set -m'] if case.monitor else []
    helper = b'../latch --from-parent-dir' if case.directory else b'./latch'
    if case.directory:
        lines.append(b"cd -- '" + case.directory.replace(b"'", b"'\\''") + b"'")
    if case.lifecycle.startswith('fg-restop-'):
        variant = b'last' if case.lifecycle.endswith('last') else b'first'
        right_gate = b'b' if variant == b'last' else b'-'
        pipeline = b'./latch --fg-pipe left ' + variant + b' 7 a | ./latch --fg-pipe right ' + variant + b' 0 ' + right_gate + b' >/dev/null'
        lines += [b'set -o pipefail' if case.pipefail else b'set +o pipefail', pipeline, b'printf "STOP %s\\n" "$?"', b'jobs -p >pids',
                  b'./latch --observe-both-stopped || exit 93', b'fg %1', b'fgstatus=$?',
                  b'printf "%s\\n" "$fgstatus" >fg-returned', b'printf "FGRETURN %s\\n" "$fgstatus"',
                  b'IFS= read -r token <after-return || exit 93']
    elif case.lifecycle.startswith('partial-'):
        pipeline = (b'./latch --pipe left 7 empty | ./latch --pipe-stop right 0 a >/dev/null'
                    if case.lifecycle == 'partial-last' else
                    b'./latch --pipe-stop left 7 a | ./latch --pipe right 0 empty >/dev/null')
        lines += [b'set -o pipefail', pipeline, b'printf "STOP %s\\n" "$?"', b'jobs -p >pids']
    elif case.lifecycle == 'done-pair':
        lines += [b'./latch --pipe left 7 a >/dev/null & p0=$!', b'printf "%s\\n" "$p0" >pids',
                  b'./latch --pipe right 9 b >/dev/null & p1=$!', b'printf "%s\\n" "$p1" >>pids',
                  b'./latch --observe-pipeline', b'printf done >a', b'printf done >b',
                  b'./latch --observe-exit-stage left "$p0"', b'./latch --observe-exit-stage right "$p1"']
    elif case.lifecycle in ('pipeline', 'completed-pipeline', 'first-done-pipeline'):
        if case.lifecycle == 'pipeline':
            pipeline_source = b'./latch --pipe left 7 a | ./latch --pipe right 9 - >/dev/null & p0=$!'
        elif case.lifecycle == 'completed-pipeline':
            lines.append(b'set -o pipefail')
            pipeline_source = b'./latch --pipe left 7 a | ./latch --pipe right 0 - >/dev/null & p0=$!'
        else:
            lines.append(b'set -o pipefail')
            pipeline_source = b'./latch --pipe left 7 empty | ./latch --pipe right 0 a >/dev/null & p0=$!'
        lines += [pipeline_source, b'printf "%s\\n" "$p0" >pids', b'./latch --observe-pipeline']
        if case.lifecycle == 'completed-pipeline':
            lines += [b'printf done >a', b'./latch --observe-all-exited || exit 93']
        elif case.lifecycle == 'first-done-pipeline':
            lines += [b'./latch --observe-exit-stage left || exit 93']
    elif case.lifecycle in ('done', 'signaled'):
        mode = case.child_mode if case.lifecycle == 'signaled' else b'run'
        lines += [b'./latch a ' + case.child_status + b' ' + mode + b' & p0=$!', b'printf "%s\\n" "$p0" >pids',
                  b'printf done >a', b'./latch --observe-exit "$p0"']
    elif case.lifecycle and case.lifecycle not in ('uncontrolled', 'stop-background', 'markers', 'reuse'):
        lines += [helper + b' a ' + case.child_status + b' ' + case.child_mode, b'printf "STOP %s\\n" "$?"', b'jobs -p >pids']
    if case.lifecycle == 'bg-with-other':
        lines += [b'cat b >/dev/null & p1=$!', b'printf "%s\\n" "$p1" >>pids',
                  helper + b' --observe-cats "$p1" || exit 93']
    if case.lifecycle == 'uncontrolled':
        lines.append(b'set +m')
    initial_children = 1 if case.lifecycle == 'reuse' else (0 if case.lifecycle and case.lifecycle not in ('uncontrolled', 'stop-background', 'markers') else case.children)
    for n in range(initial_children):
        name = b'./cat-\xff' if case.raw_name else b'cat'
        gate = bytes([ord('a') + n])
        lines.append(name + b' ' + gate + b' >/dev/null & p' + str(n).encode() + b'=$!')
        lines.append(b'printf "%s\\n" "$p' + str(n).encode() + b'" >>pids')
    if initial_children:
        lines.append(helper + b' --observe-cats || exit 93')
    if case.lifecycle == 'stop-background':
        for n in range(case.children):
            pid = b'"$p' + str(n).encode() + b'"'
            lines += [b'kill -STOP ' + pid, b'./latch --observe-stop ' + pid]
    if case.directory:
        lines.append(b'cd ../elsewhere')
    for n, probe in enumerate(case.probes):
        lines += [b"printf 'BEGIN " + str(n).encode() + b"\\n'", probe,
                  b"printf 'STATUS %s\\n' \"$?\""]
        if b'cat a >/dev/null &' in probe or b'cat b >/dev/null &' in probe:
            variable = re.search(rb'(p[0-9]+)=\$!', probe)[1]
            lines.append(helper + b' --observe-cats "$' + variable + b'" || exit 93')
        resumed = probe.startswith(b'bg %') or probe == b"fg %1 '&'"
        if resumed and case.lifecycle in ('bg', 'bg-with-other'):
            lines.append(helper + b' --observe-resumed || exit 93')
        elif resumed and case.lifecycle.startswith('partial-'):
            stage = b'left' if case.lifecycle == 'partial-first' else b'right'
            lines.append(b'./latch --observe-resumed-stage ' + stage + b' || exit 93')
        elif resumed and case.lifecycle.startswith('fg-restop-'):
            stage = b'right' if case.lifecycle.endswith('last') else b'left'
            lines.append(b'./latch --observe-resumed-stage ' + stage + b' || exit 93')
        elif resumed and case.lifecycle == 'stop-background':
            index = int(probe.split()[1][1:]) - 1
            lines.append(b'./latch --observe-running "$p' + str(index).encode() + b'" || exit 93')
    if case.lifecycle.startswith('fg-restop-'):
        gate = b'b' if case.lifecycle.endswith('last') else b'a'
        lines += [b'printf done >' + gate, b'wait %1', b'printf "WAIT %s\\n" "$?"']
    elif case.lifecycle in ('bg', 'bg-with-other', 'partial-first', 'partial-last'):
        lines += [b'printf done >a', b'wait %1', b'printf "WAIT %s\\n" "$?"']
        if case.lifecycle == 'bg-with-other':
            lines += [b'printf done >b', b'wait "$p1"', b'printf "WAIT %s\\n" "$?"']
    elif case.lifecycle in ('pipeline', 'first-done-pipeline', 'markers', 'reuse'):
        lines += [b'printf done >a', b'wait "$p0"', b'printf "WAIT %s\\n" "$?"']
    elif not case.lifecycle or case.lifecycle in ('uncontrolled', 'stop-background'):
        for n in range(case.children):
            lines.append(b'printf done >' + bytes([ord('a') + n]))
        for n in range(case.children):
            lines += [b'wait "$p' + str(n).encode() + b'"', b'printf "WAIT %s\\n" "$?"']
    lines += [b'jobs', b'printf "END\\n"']
    return b'\n'.join(lines)


def invoke(binary, case, work, brush):
    source = program(case)
    env = {b'PATH': b'/usr/bin:/bin', b'HOME': os.fsencode(work), b'LC_ALL': b'C',
           b'TERM': b'dumb', b'PS1': b'', b'PS2': b''}
    for n in range(case.children):
        os.mkfifo(work / chr(ord('a') + n))
    if case.lifecycle.startswith('fg-restop-'):
        os.mkfifo(work / 'after-return')
    if case.lifecycle.startswith('partial-') or case.lifecycle == 'first-done-pipeline':
        (work / 'empty').write_bytes(b'')
    if case.raw_name:
        os.symlink(b'/usr/bin/cat', os.fsencode(work) + b'/cat-\xff')
    shutil.copyfile(Path(__file__).with_name('bash_jobs_child.py'), work / 'latch')
    (work / 'latch').chmod(0o700)
    directory_paths = set()
    if case.directory:
        base = os.fsencode(work)
        for directory in (case.directory, b'elsewhere'):
            os.mkdir(base + b'/' + directory)
            directory_paths.add(directory)
            for name in (b'a', b'pids'):
                os.symlink(b'../' + name, base + b'/' + directory + b'/' + name)
                directory_paths.add(directory + b'/' + name)
        if case.home_mode == 'empty':
            env[b'HOME'] = b''
        elif case.home_mode == 'prefix':
            env[b'HOME'] = base + b'/ori'
    argv = [b'jobs-test', *([b'--no-config'] if brush else []), b'--noprofile', b'--norc',
            *([b'-i'] if case.tty else []), b'-c', source]
    master = slave = None
    setup = None
    if case.tty:
        master, slave = pty.openpty()
        def setup():
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)
    child = subprocess.Popen(argv, executable=os.fsencode(binary), cwd=work, env=env,
                             stdin=slave if case.tty else subprocess.DEVNULL,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                             start_new_session=True, preexec_fn=setup)
    if slave is not None:
        os.close(slave)
    known = {child.pid: proc(child.pid)}
    stdout = stderr = b''
    timeout = False
    deadline = time.monotonic() + 8
    released = False
    hold_started = None
    return_acknowledged = False
    protocol_checks = {}
    protocol_events = []
    protocol_error = None
    try:
        while True:
            for member in session_members(child.pid):
                known[member['pid']] = member
            if case.lifecycle.startswith('fg-restop-'):
                def read_identity(name):
                    try:
                        return json.loads((work / name).read_bytes())
                    except (FileNotFoundError, json.JSONDecodeError):
                        return None
                def send_latch(name, payload):
                    try:
                        fd = os.open(work / name, os.O_WRONLY | os.O_NONBLOCK)
                    except OSError as error:
                        if error.errno != 6:
                            raise
                        return False
                    try:
                        os.write(fd, payload)
                    finally:
                        os.close(fd)
                    return True
                left = read_identity('pipe-left.json')
                right = read_identity('pipe-right.json')
                stopped_stage = 'right' if case.lifecycle.endswith('last') else 'left'
                stopped = right if stopped_stage == 'right' else left
                observed = proc(stopped['pid']) if stopped else None
                if observed and observed['sid'] != child.pid:
                    raise RuntimeError('pipeline stop not owned by test session')
                if case.lifecycle.endswith('last') and not released and observed and observed['state'] == 'T' and (work / 'fg-right-restop').exists() and (work / 'fg-left-resumed.json').exists():
                    first = proc(left['pid'])
                    if first and first['state'] not in ('T', 't', 'Z'):
                        if hold_started is None:
                            hold_started = time.monotonic()
                            protocol_checks['no_early_fg_return'] = True
                            protocol_checks['retains_job_terminal'] = True
                            protocol_events.append({'event': 'last_stopped_first_held', 'first': first, 'last': observed})
                        protocol_checks['no_early_fg_return'] &= not (work / 'fg-returned').exists()
                        protocol_checks['retains_job_terminal'] &= os.tcgetpgrp(master) == left['pgid']
                        # Bounded absence window AFTER causal readiness, not a
                        # sleep used to guess whether the stop happened.
                        if time.monotonic() - hold_started >= .15:
                            released = send_latch('a', b'first-finished')
                            if released:
                                protocol_events.append({'event': 'release_first', 'held_seconds': time.monotonic() - hold_started})
                ready_to_ack = (released if case.lifecycle.endswith('last') else observed and observed['state'] == 'T' and (work / 'fg-left-restop').exists())
                if ready_to_ack and (work / 'fg-returned').exists() and not return_acknowledged:
                    protocol_checks.setdefault('shell_reclaimed_terminal', True)
                    protocol_checks['shell_reclaimed_terminal'] &= os.tcgetpgrp(master) == child.pid
                    protocol_events.append({'event': 'foreground_returned', 'status_hex': (work / 'fg-returned').read_bytes().hex(), 'terminal_pgid': os.tcgetpgrp(master)})
                    return_acknowledged = send_latch('after-return', b'go\n')
            if case.lifecycle in ('fg', 'signal') and not released and (work / 'child-after.json').exists():
                raw_identity = (work / 'child-after.json').read_bytes()
                try:
                    identity = json.loads(raw_identity)
                except json.JSONDecodeError:
                    identity = None  # Observe only the completed fixture record.
                if identity and (master is None or os.tcgetpgrp(master) == identity['pgid']):
                    try:
                        fd = os.open(work / 'a', os.O_WRONLY | os.O_NONBLOCK)
                    except OSError as error:
                        if error.errno != 6:  # ENXIO: reader has not reached its latch yet.
                            raise
                    else:
                        try:
                            os.write(fd, b'INT' if case.lifecycle == 'signal' else b'foreground-input')
                        finally:
                            os.close(fd)
                        released = True
            try:
                stdout, stderr = child.communicate(timeout=.02)
                break
            except subprocess.TimeoutExpired:
                if time.monotonic() > deadline:
                    timeout = True
                    break
    except Exception as error:
        protocol_error = repr(error)
    finally:
        cleanup = teardown(child.pid, known)
        if timeout or protocol_error:
            stdout, stderr = child.communicate(timeout=3)
        if master is not None:
            os.close(master)
    pids = (work / 'pids').read_bytes().splitlines() if (work / 'pids').exists() else []
    pipeline = {}
    for stage in ('left', 'right'):
        if (work / f'pipe-{stage}.json').exists():
            pipeline[stage] = json.loads((work / f'pipe-{stage}.json').read_bytes())
    normalized_pids = [(pid, b'<PID' + str(n).encode() + b'>') for n, pid in enumerate(pids)]
    if pipeline:
        normalized_pids = [(str(p['pid']).encode(), b'<PIPE-' + stage.encode() + b'>') for stage, p in pipeline.items()]
    pid_map = {pid: placeholder for pid, placeholder in normalized_pids if pid.isdigit()}
    normalized_stdout, stdout_errors = normalize_output(stdout, 'stdout', case, pid_map)
    normalized_stderr, stderr_errors = normalize_output(stderr, 'stderr', case, pid_map)
    effects = {}
    identities = {}
    for name in ('child-input', 'child-before.json', 'child-after.json'):
        if (work / name).exists():
            data = (work / name).read_bytes()
            effects[name] = data.hex()
            if name.endswith('.json'):
                identities[name] = json.loads(data)
    lifecycle_checks = dict(protocol_checks)
    pid_proof = json.loads((work / 'pid-proof.json').read_bytes()) if (work / 'pid-proof.json').exists() else {}
    generic_cats = case.children and case.lifecycle in ('', 'uncontrolled', 'stop-background', 'markers', 'reuse')
    if generic_cats:
        expected_count = (1 if case.lifecycle == 'reuse' else case.children) + sum(b'cat a >/dev/null &' in probe or b'cat b >/dev/null &' in probe for probe in case.probes)
        lifecycle_checks['reported_pid_count'] = len(pids) == expected_count
        lifecycle_checks['reported_pids_are_real_owned_cats'] = bool(pids) and all(p.isdigit() and str(int(p)) in pid_proof and pid_proof[str(int(p))]['sid'] == child.pid for p in pids)
    if case.lifecycle.startswith('fg-restop-'):
        lifecycle_checks['return_barrier'] = return_acknowledged
        lifecycle_checks['both_stages'] = set(pipeline) == {'left', 'right'}
        lifecycle_checks['owned_session'] = bool(pipeline) and all(p['sid'] == child.pid for p in pipeline.values())
        lifecycle_checks['owned_pgid'] = bool(pipeline.get('left')) and all(p['pgid'] == pipeline['left']['pid'] for p in pipeline.values())
        lifecycle_checks['reported_leader'] = bool(pipeline.get('left')) and pids == [str(pipeline['left']['pid']).encode()]
        if case.lifecycle.endswith('last'):
            lifecycle_checks['first_release_barrier'] = released
        stage = 'right' if case.lifecycle.endswith('last') else 'left'
        after_path = work / f'pipe-{stage}-after.json'
        after = json.loads(after_path.read_bytes()) if after_path.exists() else {}
        lifecycle_checks['resumed_identity'] = bool(after and pipeline.get(stage)) and all(after[k] == pipeline[stage][k] for k in ('pid', 'pgid', 'sid'))
        input_path = work / f'pipe-{stage}-input'
        lifecycle_checks['bytes'] = input_path.exists() and input_path.read_bytes() == b'done'
        effects['resumed_stage'] = after
    elif case.lifecycle.startswith('partial-'):
        lifecycle_checks['both_stages'] = set(pipeline) == {'left', 'right'}
        lifecycle_checks['owned_session'] = bool(pipeline) and all(p['sid'] == child.pid for p in pipeline.values())
        lifecycle_checks['reported_leader'] = bool(pipeline.get('left')) and pids == [str(pipeline['left']['pid']).encode()]
        lifecycle_checks['owned_pgid'] = bool(pipeline.get('left')) and all(p['pgid'] == pipeline['left']['pid'] for p in pipeline.values())
        stage = 'left' if case.lifecycle == 'partial-first' else 'right'
        after_path = work / f'pipe-{stage}-after.json'
        after = json.loads(after_path.read_bytes()) if after_path.exists() else {}
        lifecycle_checks['resumed_identity'] = bool(after and pipeline.get(stage)) and all(after[k] == pipeline[stage][k] for k in ('pid', 'pgid', 'sid'))
        input_path = work / f'pipe-{stage}-input'
        lifecycle_checks['bytes'] = input_path.exists() and input_path.read_bytes() == b'done'
        effects['resumed_stage'] = after
        effects['resumed_input'] = input_path.read_bytes().hex() if input_path.exists() else None
    elif case.lifecycle in ('pipeline', 'done-pair', 'completed-pipeline', 'first-done-pipeline'):
        lifecycle_checks['both_stages'] = set(pipeline) == {'left', 'right'}
        lifecycle_checks['owned_session'] = bool(pipeline) and all(p['sid'] == child.pid for p in pipeline.values())
        expected_pids = [str(pipeline[stage]['pid']).encode() for stage in ('left', 'right') if stage in pipeline] if case.lifecycle == 'done-pair' else [str(pipeline['right']['pid']).encode()] if 'right' in pipeline else []
        lifecycle_checks['last_pid'] = bool(expected_pids) and pids == expected_pids
        if case.tty:
            lifecycle_checks['owned_pgid'] = bool(pipeline.get('left')) and all(p['pgid'] == pipeline['left']['pid'] for p in pipeline.values())
    elif case.lifecycle and case.lifecycle not in ('uncontrolled', 'stop-background', 'markers', 'reuse'):
        before, after = identities.get('child-before.json'), identities.get('child-after.json')
        lifecycle_checks['stable_identity'] = bool(before and after and all(before[k] == after[k] for k in ('pid', 'pgid', 'sid')))
        lifecycle_checks['owned_session'] = bool(before and before['sid'] == child.pid)
        lifecycle_checks['real_job_pid'] = bool(before and pids and pids[0] == str(before['pid']).encode())
        if len(pids) > 1:
            lifecycle_checks['additional_pids_are_real_owned_cats'] = all(p.isdigit() and str(int(p)) in pid_proof and pid_proof[str(int(p))]['sid'] == child.pid for p in pids[1:])
        if case.tty:
            lifecycle_checks['foreground_restored'] = bool(after and ((after['foreground'] == after['pgid']) == (case.lifecycle not in ('bg', 'bg-with-other', 'done'))))
        lifecycle_checks['bytes'] = bytes.fromhex(effects.get('child-input', '')) == (b'done' if case.lifecycle in ('bg', 'bg-with-other', 'done', 'signaled') else b'INT' if case.lifecycle == 'signal' else b'foreground-input')
    if case.lifecycle in ('bg', 'bg-with-other', 'partial-first', 'partial-last'):
        lifecycle_checks['continued_barrier'] = (work / 'resumed-barrier').exists()
    unexpected_files = {}
    protocol_files = {b'pids', b'latch', b'cat-\xff', b'child-input', b'child-before.json', b'child-after.json', b'pipe-left.json', b'pipe-right.json', b'pipe-left-after.json', b'pipe-right-after.json', b'pipe-left-input', b'pipe-right-input', b'empty', b'resumed-barrier', b'fg-left-resumed.json', b'fg-right-resumed.json', b'fg-left-restop', b'fg-right-restop', b'fg-returned', b'after-return', b'pid-proof.json'}
    protocol_file_bytes = {}
    for name in protocol_files - {b'latch'}:
        path = os.fsencode(work) + b'/' + name
        try:
            mode = os.lstat(path).st_mode
        except FileNotFoundError:
            continue
        if stat.S_ISREG(mode):
            with open(path, 'rb') as file:
                protocol_file_bytes[name.hex()] = file.read().hex()
    for root, dirs, files in os.walk(os.fsencode(work)):
        for name in dirs + files:
            path = os.path.join(root, name)
            relative = os.path.relpath(path, os.fsencode(work))
            if relative in protocol_files or relative in directory_paths:
                continue
            mode = os.lstat(path).st_mode
            if stat.S_ISREG(mode):
                with open(path, 'rb') as file:
                    value = {'bytes': file.read().hex()}
            elif stat.S_ISLNK(mode):
                value = {'symlink': os.readlink(path).hex()}
            elif stat.S_ISFIFO(mode) and relative in (b'a', b'b', b'c'):
                continue  # Known latch, never read a FIFO during evidence collection.
            else:
                value = {'mode': stat.S_IFMT(mode)}
            unexpected_files[relative.hex()] = value
    return {'source_hex': source.hex(), 'argv_hex': [a.hex() for a in argv], 'status': child.returncode,
            'stdout_hex': stdout.hex(), 'stderr_hex': stderr.hex(),
            'comparable_stdout_hex': normalized_stdout.hex(), 'comparable_stderr_hex': normalized_stderr.hex(),
            'normalization_errors': stdout_errors + stderr_errors, 'protocol_events': protocol_events,
            'protocol_file_bytes': protocol_file_bytes, 'pid_proof': pid_proof,
            'pids': [int(p) if p.isdigit() else None for p in pids], 'observed_processes': list(known.values()),
            'timeout': timeout, 'protocol_error': protocol_error, 'cleanup': cleanup, 'pty': case.tty,
            'files': {'pids': [p.decode('ascii') for p in pids], **effects}, 'pipeline': pipeline, 'unexpected_files': unexpected_files,
            'lifecycle_checks': lifecycle_checks}


def equivalent(actual, expected):
    return all(actual[k] == expected[k] for k in ('status', 'comparable_stdout_hex', 'comparable_stderr_hex', 'unexpected_files')) \
        and not actual['timeout'] and not expected['timeout'] \
        and not actual['protocol_error'] and not expected['protocol_error'] \
        and not actual.get('normalization_errors') and not expected.get('normalization_errors') \
        and actual['cleanup']['clean'] and expected['cleanup']['clean'] \
        and all(actual['lifecycle_checks'].values()) and all(expected['lifecycle_checks'].values())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--shell', action='append', required=True, metavar='LABEL=PATH')
    parser.add_argument('--brush', action='append', default=[])
    parser.add_argument('--require-exact', action='append', default=[])
    parser.add_argument('--evidence', type=Path, required=True)
    parser.add_argument('--case', action='append', default=[])
    args = parser.parse_args()
    if sys.platform != 'linux' or not hasattr(os, 'pidfd_open'):
        parser.error('requires Linux /proc and pidfd cleanup; no unowned-process fallback')
    # Adopt only descendants orphaned by our own test shells; reap exact recorded PIDs.
    if ctypes.CDLL(None, use_errno=True).prctl(36, 1, 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), 'PR_SET_CHILD_SUBREAPER')
    binaries = {label: Path(path).resolve(strict=True) for label, path in
                (item.split('=', 1) for item in args.shell)}
    if not set(args.brush + args.require_exact).issubset(binaries):
        parser.error('unknown label')
    oracle = next(iter(binaries))
    identities = {label: {'path': str(path), 'sha256': digest(path)} for label, path in binaries.items()}
    version = subprocess.run([binaries[oracle], '--version'], env={'LC_ALL': 'C', 'PATH': '/usr/bin:/bin'},
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True, start_new_session=True)
    if b'GNU bash, version 5.3.20(' not in version.stdout:
        parser.error('the independent oracle must be actual GNU Bash 5.3.20')
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=False)
    records = []
    cases = [c for c in corpus() if not args.case or c.name in args.case]
    if not cases:
        parser.error('no cases selected')
    for case in cases:
        outputs = {}
        # Every executable sees the SAME owned absolute fixture path. Directory
        # annotations therefore compare raw bytes without a path normalizer.
        with tempfile.TemporaryDirectory(prefix='marsh-jobs-cli-') as directory:
            work = Path(directory) / 'case'
            for label, binary in binaries.items():
                work.mkdir(mode=0o700)
                result = invoke(binary, case, work, label in args.brush)
                outputs[label] = result
                for stream in ('stdout', 'stderr'):
                    path = evidence / 'raw' / case.name / (label + '.' + stream)
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_bytes(bytes.fromhex(result[stream + '_hex']))
                shutil.rmtree(work)
        exact = {label: equivalent(result, outputs[oracle]) for label, result in outputs.items()}
        record = {'name': case.name, 'outputs': outputs, 'exact': exact}
        records.append(record)
        with (evidence / 'results.jsonl').open('a') as log:
            log.write(json.dumps(record) + '\n')
        print(case.name, exact, flush=True)
    # Mutation controls target real observed bytes/status, not implementation mirrors.
    first = records[0]['outputs'][oracle]
    mutations = [dict(first, comparable_stdout_hex='00' + first['comparable_stdout_hex']),
                 dict(first, comparable_stderr_hex='ff' + first['comparable_stderr_hex']),
                 dict(first, status=123), dict(first, cleanup={'clean': False}),
                 dict(first, unexpected_files={'63616e617279': {'bytes': 'ff'}})]
    negative_controls = [not equivalent(m, first) for m in mutations]
    stable = all(digest(p) == identities[label]['sha256'] for label, p in binaries.items())
    manifest = {'identities': identities, 'oracle_version_hex': version.stdout.hex(),
                'harness_sha256': digest(__file__),
                'child_fixture_sha256': digest(Path(__file__).with_name('bash_jobs_child.py')),
                'count': len(records), 'stable': stable,
                'exact': {label: sum(r['exact'][label] for r in records) for label in binaries},
                'negative_controls': negative_controls,
                'cleanup_failures': sum(not o['cleanup']['clean'] for r in records for o in r['outputs'].values())}
    (evidence / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    print(json.dumps(manifest, indent=2))
    return int(not stable or not all(negative_controls) or manifest['cleanup_failures'] != 0
               or any(not r['exact'][label] for r in records for label in [oracle, *args.require_exact]))


if __name__ == '__main__':
    raise SystemExit(main())
