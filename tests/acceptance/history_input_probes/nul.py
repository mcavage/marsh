#!/usr/bin/env python3
"""Owned stdin NUL callers for all three public backends, including -v/history paths."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location('readline_cli', ROOT / 'tests/acceptance/bash_readline_cli.py')
h = importlib.util.module_from_spec(spec); spec.loader.exec_module(h)
spec = importlib.util.spec_from_file_location('nul_cases', ROOT / 'tests/acceptance/bash_nul_input.py')
vectors = importlib.util.module_from_spec(spec); spec.loader.exec_module(vectors)
binary = Path(sys.argv[2]).resolve()
brush = len(sys.argv) > 3 and sys.argv[3] == 'brush'
out = Path(sys.argv[1]); out.mkdir(parents=True, exist_ok=False)
rows = []
cases = {name: source for name, source in vectors.cases.items()
         if not name.startswith(('source-', 'startup-', 'admission-'))}
cases.update({'only-nul-eof': b'\0' * 513, 'nul-between-units': b'printf A\n\0\0printf B\n'})
for interactive in (False, True):
    for backend in ('minimal', 'basic', 'reedline'):
        for name, raw in cases.items():
            for variant, source in (('raw', raw), ('without-nul', raw.replace(b'\0', b''))):
                label = f'{backend}-{int(interactive)}-{name}-{variant}'
                work = out / label; work.mkdir()
                args = ([b'--no-config', b'--input-backend', backend.encode()] if brush else [])
                args += [b'--noprofile', b'--norc']
                if interactive: args += [b'--noediting', b'-i']
                args += [b'-s']
                env = {b'PATH': b'/usr/bin:/bin', b'HOME': os.fsencode(work),
                       b'HISTFILE': b'/dev/null', b'INPUTRC': b'/dev/null',
                       b'PS1': h.PROMPT, b'PS2': h.SECONDARY,
                       b'LC_ALL': os.fsencode(os.environ.get('MARSH_TEST_LOCALE', 'C'))}
                child = h.Owned(binary, args, work, env, False)
                stdout = stderr = b''; error = None
                try:
                    stdout, stderr = child.child.communicate(source, timeout=8)
                except (OSError, subprocess.TimeoutExpired) as exc:
                    error = str(exc)
                finally:
                    cleanup = child.close()
                rows.append(dict(name=label, source=source.hex(), status=child.child.returncode,
                                 stdout=stdout.hex(), stderr=stderr.hex(), error=error, cleanup=cleanup))
                print(label, child.child.returncode, cleanup['clean'], error, flush=True)
(out / 'receipts.json').write_text(json.dumps(dict(binary=str(binary), sha256=h.sha(binary), receipts=rows), indent=2) + '\n')
raise SystemExit(0 if all(row['cleanup']['clean'] and row['error'] is None for row in rows) else 1)
