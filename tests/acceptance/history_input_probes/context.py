#!/usr/bin/env python3
"""Owned GNU physical-input context probes; raw receipts, no candidate pass claim."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[3]
spec = importlib.util.spec_from_file_location("readline_cli", ROOT / "tests/acceptance/bash_readline_cli.py")
h = importlib.util.module_from_spec(spec)
spec.loader.exec_module(h)
binary = Path(sys.argv[2]).resolve() if len(sys.argv) > 2 else (ROOT / "target/marsh-evidence/bash-jobs/gnu-bash-5.3.20").resolve()
brush = len(sys.argv) > 3 and sys.argv[3] == 'brush'
mode = sys.argv[4] if len(sys.argv) > 4 else 'pipe'
tty = mode != 'pipe'
cases = {
    "single": b"printf '<%s>\\n' 'first\n!!\nlast'\n",
    "double": b'printf "<%s>\\n" "first\n!!\nlast"\n',
    "cmdsub_single": b'''printf '<%s>\\n' "$(printf '%s' 'first\n!!\nlast')"\n''',
    "cmdsub_double": b'''printf '<%s>\\n' "$(printf '%s' "first\n!!\nlast")"\n''',
    "case_cmdsub_single": b'''printf '<%s>\\n' "$(case x in x) printf '%s' 'first\n!!\nlast';; esac)"\n''',
    "parameter_single_outer_double": b'''printf '<%s>\\n' "${unset:-'first\n!!\nlast'}"\n''',
    "parameter_single_unquoted": b'''printf '<%s>\\n' ${unset:-'first\n!!\nlast'}\n''',
    "backtick_single": b'''printf '<%s>\\n' "`printf '%s' 'first\n!!\nlast'`"\n''',
    "backtick_double": b'''printf '<%s>\\n' "`printf '%s' "first\n!!\nlast"`"\n''',
    "heredoc": b"cat <<EOF\n!!\nEOF\n",
    "quoted_heredoc": b"cat <<'EOF'\n!!\nEOF\n",
    "alias_quote": b'''alias a="printf '<%s>\\\\n' 'first"\na\n!!\nlast'\n''',
}
out = Path(sys.argv[1]); out.mkdir(parents=True, exist_ok=False)
receipts = []
for posix in (False, True):
    for name, source in cases.items():
        work = out / (name + ("-posix" if posix else "")); work.mkdir()
        source = b"set -H\n" + (b"set -o posix\n" if posix else b"") + b"history -s SEED\n" + source + b"exit 7\n"
        if tty: source = b'exec >command.stdout\n' + source
        backend = b'basic' if mode == 'basic-pty' else (b'reedline' if tty else b'minimal')
        child = h.Owned(binary, ([b'--no-config', b'--input-backend', backend] if brush else []) + [b"--noprofile", b"--norc", b"-i", b"-s"], work,
                        {b"PATH": b"/usr/bin:/bin", b"HOME": os.fsencode(work), b"HISTFILE": b"/dev/null", b"INPUTRC": b"/dev/null", b"PS1": h.PROMPT if tty else b"", b"PS2": h.SECONDARY if tty else b"", b"LC_ALL": os.fsencode(os.environ.get('MARSH_TEST_LOCALE', 'C')), b'TERM':b'xterm-256color'}, tty)
        stdout = stderr = b""; error = None
        try:
            if tty:
                child.wait(lambda:h.PROMPT in child.raw);child.send(source);child.finish()
                stdout=(work/'command.stdout').read_bytes() if (work/'command.stdout').exists() else b''
                stderr=bytes(child.raw)
            else:
                stdout, stderr = child.child.communicate(source, timeout=8)
        except (TimeoutError, RuntimeError, OSError, subprocess.TimeoutExpired) as exc:
            error = str(exc)
        finally:
            cleanup = child.close()
        receipts.append(dict(name=name, posix=posix, mode=mode, pty=bytes(child.raw).hex(), steps=child.steps, source=source.hex(), stdout=stdout.hex(), stderr=stderr.hex(), status=child.child.returncode, error=error, cleanup=cleanup))
        print(name, posix, repr(stdout), child.child.returncode, cleanup["clean"])
(out / "receipts.json").write_text(json.dumps({"binary":str(binary), "sha256":h.sha(binary), "receipts":receipts}, indent=2)+"\n")
raise SystemExit(0 if all(r["cleanup"]["clean"] and r["error"] is None for r in receipts) else 1)
