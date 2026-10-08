#!/usr/bin/env python3
"""Owned primary EOF lexical policy, prompt expansion and exit-notice callers."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location('readline_cli', ROOT / 'tests/acceptance/bash_readline_cli.py')
h = importlib.util.module_from_spec(spec); spec.loader.exec_module(h)
binary = Path(sys.argv[2]).resolve(); brush = len(sys.argv) > 3 and sys.argv[3] == 'brush'
out = Path(sys.argv[1]); out.mkdir(parents=True, exist_ok=False); rows = []
cases = {'odd': b'printf B\\', 'even': b'printf E\\\\', 'double': b'printf "D\\',
         'single': b"printf 'S\\", 'continuation': b'printf C\\\n',
         'ordinary': b'printf END', 'line': b'printf END\n'}
for editing in (False, True):
    for backend in ('minimal', 'basic', 'reedline'):
        for name, source in cases.items():
            name = f'{backend}-{int(editing)}-{name}'; work = out / name; work.mkdir()
            args = ([b'--no-config', b'--input-backend', backend.encode()] if brush else [])
            args += [b'--noprofile', b'--norc'] + ([] if editing else [b'--noediting']) + [b'-i', b'-s']
            env = {b'PATH': b'/usr/bin:/bin', b'HOME': os.fsencode(work), b'HISTFILE': b'/dev/null',
                   b'INPUTRC': b'/dev/null', b'LC_ALL': os.fsencode(os.environ.get('MARSH_TEST_LOCALE', 'C')),
                   b'PS1': h.PROMPT, b'PS2': b'$(printf Q >>ps2)' + h.SECONDARY,
                   b'PROMPT_COMMAND': b'printf P >>pc'}
            child = h.Owned(binary, args, work, env, False); error = None; stdout = stderr = b''
            try:
                stdout, stderr = child.child.communicate(source, timeout=8)
            except (OSError, subprocess.TimeoutExpired) as exc:
                error = str(exc)
            finally:
                cleanup = child.close()
            file = lambda name: (work / name).read_bytes() if (work / name).exists() else b''
            rows.append(dict(name=name, source=source.hex(), status=child.child.returncode, stdout=stdout.hex(),
                             stderr=stderr.hex(), pc=file('pc').hex(), ps2=file('ps2').hex(), error=error, cleanup=cleanup))
            print(name, child.child.returncode, repr(stdout), repr(stderr), repr(file('pc')), repr(file('ps2')), cleanup['clean'], flush=True)
(out / 'receipts.json').write_text(json.dumps(dict(binary=str(binary), sha256=h.sha(binary), receipts=rows), indent=2) + '\n')
raise SystemExit(0 if all(row['cleanup']['clean'] and row['error'] is None for row in rows) else 1)
