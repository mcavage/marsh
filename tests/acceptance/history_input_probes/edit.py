#!/usr/bin/env python3
"""Owned histverify/histreedit submission and pasted-tail buffer/counter receipts."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location('readline_cli', ROOT / 'tests/acceptance/bash_readline_cli.py')
h = importlib.util.module_from_spec(spec); spec.loader.exec_module(h)
binary = Path(sys.argv[2]).resolve()
brush = len(sys.argv) > 3 and sys.argv[3] == 'brush'
out = Path(sys.argv[1]); out.mkdir(parents=True, exist_ok=False)
rows = []
for kind in ('verify', 'no-verify', 'reedit', 'no-reedit', 'print-only', 'verify-double'):
    for raw in (False, True):
        for paste in ((True,) if kind == 'verify-double' else (False, True)):
            name = kind + ('-raw' if raw else '-utf8') + ('-paste' if paste else '')
            work = out / name; work.mkdir()
            seed = b"printf '" + (b'\xff' if raw else 'é'.encode()) + b"' >command.stdout"
            setup = b'set -H; shopt -u histverify histreedit\n'
            if kind in ('verify', 'verify-double'): setup += b'shopt -s histverify\n'
            if kind == 'reedit': setup += b'shopt -s histreedit\n'
            setup += b'''bind -x '"\\C-g":printf "%s\\n%s\\n%s\\n" "$?" "$READLINE_POINT" "$READLINE_LINE" >buffer; printf G >>callbacks'\n'''
            setup += b"history -s '" + seed.replace(b"'", b"'\\''") + b"'\nprintf READY >ready\n(exit 7)\n"
            (work / 'setup').write_bytes(setup)
            (work / 'inputrc').write_bytes(b'set enable-bracketed-paste on\nset input-meta on\nset convert-meta off\n')
            env = {b'PATH': b'/usr/bin:/bin', b'HOME': os.fsencode(work), b'HISTFILE': b'/dev/null',
                   b'INPUTRC': os.fsencode((work / 'inputrc').resolve()), b'PS1': h.PROMPT, b'PS2': h.SECONDARY,
                   b'LC_ALL': os.fsencode(os.environ.get('MARSH_TEST_LOCALE', 'C')),
                   b'TERM': b'xterm-256color', b'PROMPT_COMMAND': b'printf P >>pc'}
            args = ([b'--no-config', b'--input-backend', b'reedline'] if brush else [])
            args += [b'--noprofile', b'--norc', b'-i']
            child = h.Owned(binary, args, work, env, True)
            error = None; before = {}
            event = b'!!:s/printf/echo/\n!!' if kind == 'verify-double' else (b'!!' if 'verify' in kind else (b'!!:p' if kind == 'print-only' else b'!missing_xyz'))
            submitted = b'\x1b[200~' + event + b'\nprintf AFTER >>after\x1b[201~\n' if paste else event + b'\n'
            try:
                child.wait(lambda: h.PROMPT in child.raw)
                child.send(b'source setup\n'); child.wait(lambda: (work / 'ready').exists()); child.settle()
                child.send(submitted); child.settle()
                before = {'executed': (work / 'command.stdout').exists(), 'after': (work / 'after').exists(),
                          'pc': (work / 'pc').read_bytes().hex()}
                child.send(b'\x07'); child.wait(lambda: (work / 'buffer').exists()); child.settle()
                if kind in ('verify', 'verify-double', 'reedit'):
                    child.send(b'\n' if kind in ('verify', 'verify-double') else b'\x01\x0bprintf RECOVER >command.stdout\n')
                    child.wait(lambda: (work / 'command.stdout').exists())
                    if paste: child.wait(lambda: (work / 'after').exists())
                    child.settle()
                child.send(b'\x01\x0bprintf "S:%s" "$?" >done-status; exit 7\n'); child.finish()
            except (TimeoutError, RuntimeError, OSError, subprocess.TimeoutExpired) as exc:
                error = str(exc)
            finally:
                cleanup = child.close()
            file = lambda name: (work / name).read_bytes() if (work / name).exists() else b''
            row = dict(name=name, setup=setup.hex(), seed=seed.hex(), source=submitted.hex(), before=before,
                       buffer=file('buffer').hex(), callbacks=file('callbacks').hex(), pc=file('pc').hex(),
                       stdout=file('command.stdout').hex(), after=file('after').hex(), final_status=file('done-status').hex(),
                       status=child.child.returncode, pty=bytes(child.raw).hex(), steps=child.steps, error=error, cleanup=cleanup)
            rows.append(row)
            print(name, before, repr(file('buffer')), repr(file('after')), cleanup['clean'], error, flush=True)
(out / 'receipts.json').write_text(json.dumps(dict(binary=str(binary), sha256=h.sha(binary), receipts=rows), indent=2) + '\n')
raise SystemExit(0 if all(row['cleanup']['clean'] and row['error'] is None for row in rows) else 1)
