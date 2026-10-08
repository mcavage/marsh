#!/usr/bin/env python3
"""Observed primary GNU history preprocessing/status/storage policy."""
import importlib.util, json, os, subprocess, sys
from pathlib import Path
ROOT=Path(__file__).resolve().parents[3]
s=importlib.util.spec_from_file_location('h',ROOT/'tests/acceptance/bash_readline_cli.py'); h=importlib.util.module_from_spec(s);s.loader.exec_module(h)
binary=Path(sys.argv[2]).resolve() if len(sys.argv)>2 else (ROOT/'target/marsh-evidence/bash-jobs/gnu-bash-5.3.20').resolve()
brush=len(sys.argv)>3 and sys.argv[3]=='brush'
cases={
 'error-after-zero':b'true\n!missing_xyz\nprintf "STATUS:%s\\n" "$?"\n',
 'error-after-one':b'false\n!missing_xyz\nprintf "STATUS:%s\\n" "$?"\n',
 'print-only':b'history -s "printf SEED"\n!!:p\nprintf "STATUS:%s\\n" "$?"\nhistory -w saved\n',
 'print-only-incomplete':b'history -s "printf SEED"\nprintf "%s\\n" "first\n!!:p\nlast"\nprintf "STATUS:%s\\n" "$?"\nhistory -w saved\n',
 'error-incomplete':b'printf "%s\\n" "first\n!missing_xyz\nlast"\nprintf "STATUS:%s\\n" "$?"\nhistory -w saved\n',
 'banghash-incomplete':b'printf "%s\\n" "first\n!#\nlast"\nhistory -w saved\n',
 'literalblank-incomplete':b'printf "%s\\n" "first\n\nlast"\nhistory -w saved\n',
 'double-whole-posix':b'set -o posix\nhistory -s SEED\nprintf "%s\\n" "!!"\n',
 'history-off':b'history -s SEED\nset +o history\nprintf "%s\\n" !!\n',
 'expansion-off':b'history -s SEED\nset +H\nprintf "%s\\n" !!\n',
 'lineno-print':b'history -s :\n!!:p\nprintf "LINE:%s\\n" "$LINENO"\n',
 'lineno-error':b'!missing_xyz\nprintf "LINE:%s\\n" "$LINENO"\n',
 'lineno-incomplete':b'history -s PRINTED\nprintf "%s\\n" "first:$LINENO\n!!:p\nlast:$LINENO"\nprintf "LINE:%s\\n" "$LINENO"\n',
}
out=Path(sys.argv[1]);out.mkdir(parents=True,exist_ok=False);rows=[]
for name,body in cases.items():
 work=out/name;work.mkdir(); source=b'set -H\n'+body+b'exit 7\n'
 c=h.Owned(binary,([b'--no-config',b'--input-backend',b'minimal'] if brush else [])+[b'--noprofile',b'--norc',b'-i',b'-s'],work,{b'PATH':b'/usr/bin:/bin',b'HOME':os.fsencode(work),b'HISTFILE':b'/dev/null',b'INPUTRC':b'/dev/null',b'PS1':b'',b'PS2':b'',b'LC_ALL':os.fsencode(os.environ.get('MARSH_TEST_LOCALE','C'))},False)
 stdout=stderr=b'';error=None
 try:stdout,stderr=c.child.communicate(source,timeout=8)
 except (OSError,subprocess.TimeoutExpired) as e:error=str(e)
 finally:cleanup=c.close()
 saved=(work/'saved').read_bytes() if (work/'saved').exists() else b''
 rows.append(dict(name=name,source=source.hex(),stdout=stdout.hex(),stderr=stderr.hex(),history=saved.hex(),status=c.child.returncode,error=error,cleanup=cleanup))
 print(name,repr(stdout),repr(saved),c.child.returncode,cleanup['clean'])
(out/'receipts.json').write_text(json.dumps(dict(binary=str(binary),sha256=h.sha(binary),receipts=rows),indent=2)+'\n')
raise SystemExit(0 if all(r['cleanup']['clean'] and not r['error'] for r in rows) else 1)
