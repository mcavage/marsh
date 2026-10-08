#!/usr/bin/env python3
"""Immutable, interleaved Mac process-cold callers; no cache flushing or retries."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument('--output', required=True)
parser.add_argument('--samples', type=int, default=21)
parser.add_argument('--candidate', required=True)
parser.add_argument('--baseline', required=True)
parser.add_argument('--oracle', required=True)
args = parser.parse_args()
programs = {
    'baseline': Path(args.baseline).resolve(),
    'candidate': Path(args.candidate).resolve(),
    'gnu': Path(args.oracle).resolve(),
}
expected_hashes = {label: hashlib.sha256(path.read_bytes()).hexdigest() for label, path in programs.items()}
def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

assert {label: digest(path) for label, path in programs.items()} == expected_hashes
observer = b'; printf "%s:%s" "$i" "$x" >effect; printf "%s:%s" "$i" "$x"'
cases = {
    'nested-substitution10': (b'for i in $(seq 10); do x=$(y=$(printf a); printf %s "$y"); done' + observer, b'10:a'),
    'nested-regex10': (b'for i in $(seq 10); do x=$(y=$(printf a); [[ a =~ (a) ]]; printf "%s%s" "$y" "${BASH_REMATCH[1]}"); done' + observer, b'10:aa'),
    'cold-shebang': (b'x=$(printf a); printf %s "$x" >effect; printf %s "$x"\n', b'a'),
    'enoexec-after-paren': (b'(true); exec ./textscript', b'text'),
}
rows = []
labels = list(programs)
output = Path(args.output)
timeout = 10

def group_members(pgid):
    observed = subprocess.run(['/bin/ps', '-axo', 'pid=,pgid=,comm='], capture_output=True, check=True)
    members = []
    for line in observed.stdout.splitlines():
        fields = line.split(None, 2)
        if len(fields) >= 2 and fields[1].isdigit() and int(fields[1]) == pgid:
            members.append({'pid': int(fields[0]), 'pgid': pgid, 'command_hex': fields[2].hex() if len(fields) == 3 else ''})
    return members

def quantile_interval(selected, q):
    rank = math.ceil(q * len(selected)) - 1
    lower = sorted(row['censor_lower_bound_seconds'] if row['forced'] else row['seconds'] for row in selected)[rank]
    upper = sorted(float('inf') if row['forced'] else row['seconds'] for row in selected)[rank]
    return {'seconds': lower if lower == upper else None, 'lower_seconds': lower,
            'upper_seconds': None if math.isinf(upper) else upper, 'rank': rank + 1}

def write_result(final=False):
    summary = {}
    for name in cases:
        summary[name] = {}
        for label in labels:
            selected = [row for row in rows if row['case'] == name and row['label'] == label]
            if selected:
                summary[name][label] = {'samples': len(selected), 'timeouts': sum(row['forced'] for row in selected),
                    'median': quantile_interval(selected, .5), 'p95': quantile_interval(selected, .95)}
    result = {'identities': {label: {'path': str(path), 'sha256': expected_hashes[label]} for label, path in programs.items()},
        'harness_sha256': digest(Path(__file__)), 'scope': 'Mac process-cold, filesystem/kernel caches not flushed',
        'timeout_seconds': timeout, 'summary': summary, 'rows': rows, 'finished': final,
        'passed': final and all(row['passed'] for row in rows)}
    output.write_text(json.dumps(result, indent=2) + '\n')
    return result

for iteration in range(args.samples):
    for name, (source, expected) in cases.items():
        for label in labels[iteration % 3:] + labels[:iteration % 3]:
            with tempfile.TemporaryDirectory(prefix='native-performance-owned-') as directory:
                work = Path(directory)
                history = work / 'history'
                history.touch(mode=0o600)
                fixture = None
                if name == 'cold-shebang':
                    fixture = b'#!' + os.fsencode(programs[label]) + b'\n' + source
                    (work / 'script').write_bytes(fixture)
                    (work / 'script').chmod(0o700)
                    command = [str(work / 'script')]
                else:
                    command = [str(programs[label]), '--noprofile', '--norc', '-c', source]
                    if name == 'enoexec-after-paren':
                        fixture = b'printf text >effect; printf text\n'
                        (work / 'textscript').write_bytes(fixture)
                        (work / 'textscript').chmod(0o700)
                environment = {'HOME': directory, 'HISTFILE': str(history), 'PATH': '/usr/bin:/bin', 'LC_ALL': 'C'}
                started = time.monotonic()
                child = subprocess.Popen(command, cwd=work, env=environment, stdin=subprocess.DEVNULL,
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
                launch_seconds = time.monotonic() - started
                forced = False
                try:
                    try:
                        stdout, stderr = child.communicate(timeout=timeout)
                    except subprocess.TimeoutExpired:
                        forced = True
                        # This exact session/group was created above and retained throughout.
                        os.killpg(child.pid, signal.SIGKILL)
                        stdout, stderr = child.communicate(timeout=3)
                finally:
                    if child.poll() is None:
                        os.killpg(child.pid, signal.SIGKILL)
                        child.wait(timeout=3)
                elapsed = time.monotonic() - started
                members = group_members(child.pid)
                effects = {path.name: path.read_bytes().hex() for path in work.iterdir() if path.is_file()}
                expected_files = {'history': '', 'effect': expected.hex()}
                if fixture is not None:
                    expected_files['script' if name == 'cold-shebang' else 'textscript'] = fixture.hex()
                row = dict(iteration=iteration, case=name, label=label, source_hex=source.hex(),
                    fixture_hex=None if fixture is None else fixture.hex(), pid=child.pid, pgid=child.pid,
                    seconds=elapsed, launch_seconds=launch_seconds, censor_lower_bound_seconds=launch_seconds + timeout,
                    status=child.returncode, stdout_hex=stdout.hex(), stderr_hex=stderr.hex(), forced=forced,
                    effects=effects, expected_files=expected_files, post_reap_group_members=members,
                    passed=not forced and child.returncode == 0 and stdout == expected and stderr == b''
                        and effects == expected_files and not members)
                rows.append(row)
                write_result()
                print(json.dumps({key: row[key] for key in ['iteration', 'case', 'label', 'seconds', 'forced', 'passed']}), flush=True)
assert {label: digest(path) for label, path in programs.items()} == expected_hashes
result = write_result(final=True)
print(json.dumps({'summary': result['summary'], 'passed': result['passed']}, indent=2), flush=True)
raise SystemExit(not result['passed'])
