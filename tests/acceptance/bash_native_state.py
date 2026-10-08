#!/usr/bin/env python3
"""Real growing-state and raw-byte native substitution callers."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time

parser = argparse.ArgumentParser()
parser.add_argument('--candidate', required=True)
parser.add_argument('--oracle', required=True)
parser.add_argument('--out', required=True)
args = parser.parse_args()
programs = {name: Path(value).resolve() for name, value in
            [('candidate', args.candidate), ('GNU', args.oracle)]}
identities = {name: hashlib.sha256(path.read_bytes()).hexdigest()
              for name, path in programs.items()}
raw_digest = hashlib.sha256(b'\xff\xfeXY' * 5_000_000).hexdigest()
cases = {
    '3000-growing-variable-substitutions': (
        b'for i in $(seq 1 3000); do eval "v$i=\\$(printf %0100d $i)"; done; '
        b'( echo ${v2999:0:3} )', b'000\n'),
    '20MB-raw-parameter-native-boundaries': (
        b'''value=$(python3 -c 'import sys; sys.stdout.buffer.write(b"\\xff\\xfeXY" * 5000000)')
(printf '%s' "$value") > bytes
printf '%s\\n' "${#value}" "$(printf '%s' "${#value}")"
''', b'20000000\n20000000\n'),
}
rows = []
for name, (source, expected) in cases.items():
    for label, binary in programs.items():
        with tempfile.TemporaryDirectory(prefix='marsh-native-state-') as temporary:
            work = Path(temporary)
            history = work / 'history'
            history.touch(mode=0o600)
            env = {'HOME': temporary, 'HISTFILE': str(history),
                   'PATH': '/opt/homebrew/bin:/usr/bin:/bin', 'LC_ALL': 'C'}
            started = time.monotonic()
            child = subprocess.Popen(
                [str(binary), '--noprofile', '--norc', '-c', source, 'native-state'],
                cwd=work, env=env, stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
            forced = False
            try:
                stdout, stderr = child.communicate(timeout=180)
            except subprocess.TimeoutExpired:
                forced = True
                os.killpg(child.pid, signal.SIGKILL)
                stdout, stderr = child.communicate(timeout=3)
            elapsed = time.monotonic() - started
            table = subprocess.check_output(['/bin/ps', '-axo', 'pid=,pgid='])
            survivors = [int(fields[0]) for line in table.splitlines()
                         if len(fields := line.split()) == 2 and int(fields[1]) == child.pid]
            for pid in survivors:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            effects = {p.name: {'length': p.stat().st_size,
                               'sha256': hashlib.sha256(p.read_bytes()).hexdigest()}
                       for p in work.iterdir() if p.is_file()}
            expected_effects = {'history': {'length': 0, 'sha256': hashlib.sha256(b'').hexdigest()}}
            if name.startswith('20MB'):
                expected_effects['bytes'] = {'length': 20_000_000, 'sha256': raw_digest}
            row = {'case': name, 'label': label, 'source_hex': source.hex(),
                   'seconds': elapsed, 'status': child.returncode,
                   'stdout_hex': stdout.hex(), 'stderr_hex': stderr.hex(),
                   'timeout': forced, 'survivors': survivors, 'effects': effects,
                   'passed': not forced and not survivors and child.returncode == 0
                             and stdout == expected and not stderr and effects == expected_effects}
            rows.append(row)
            print(json.dumps(row), flush=True)
            Path(args.out).write_text(json.dumps({'identities': identities, 'rows': rows}, indent=2) + '\n')
assert identities == {name: hashlib.sha256(path.read_bytes()).hexdigest()
                      for name, path in programs.items()}
result = {'identities': identities, 'rows': rows, 'passed': all(row['passed'] for row in rows),
          'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
Path(args.out).write_text(json.dumps(result, indent=2) + '\n')
raise SystemExit(not result['passed'])
