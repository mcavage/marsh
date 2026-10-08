#!/usr/bin/env python3
"""Owned literal bracketed-paste versus physical-input history behavior."""
import importlib.util,json,os,subprocess,sys
from pathlib import Path
ROOT=Path(__file__).resolve().parents[3]
s=importlib.util.spec_from_file_location('h',ROOT/'tests/acceptance/bash_readline_cli.py');h=importlib.util.module_from_spec(s);s.loader.exec_module(h)
binary=Path(sys.argv[2]).resolve();brush=len(sys.argv)>3 and sys.argv[3]=='brush'
out=Path(sys.argv[1]);out.mkdir(parents=True,exist_ok=False);rows=[]
cases={'plain':b"printf '<%s>\\n' !!",'single':b"printf '<%s>\\n' 'first\n!!\nlast'",'double':b'printf "<%s>\\n" "first\n!!\nlast"','heredoc':b'cat <<EOF\n!!\nEOF',
'flags':b'set +H\nprintf "<%s>\\n" !!','history':b'history -s CHANGED\nprintf "<%s>\\n" !!','alias':b"alias pp='printf ALIAS'\npp",'close':b'exec 0<&-\nprintf BUFFERED\nexit 7','redirect':b'exec 0<replacement\nprintf BUFFERED'}
for posix in (False,True):
 for basic in (False,True):
  for name,payload in cases.items():
   work=out/(name+('-posix' if posix else '')+('-rawprompt' if basic else ''));work.mkdir()
   (work/'inputrc').write_bytes(b'set enable-bracketed-paste on\nset input-meta on\nset convert-meta off\n')
   setup=b'exec >command.stdout\nset -H\n'+(b'set -o posix\n' if posix else b'')+b'history -s SEED\nprintf READY >ready\n'
   (work/'setup').write_bytes(setup)
   (work/'replacement').write_bytes(b"printf 'NEW\\n'\nexit 7\n")
   args=([b'--no-config',b'--input-backend',b'reedline'] if brush else [])+[b'--noprofile',b'--norc',b'-i']
   env={b'PATH':b'/usr/bin:/bin',b'HOME':os.fsencode(work),b'HISTFILE':b'/dev/null',b'INPUTRC':os.fsencode((work/'inputrc').resolve()),b'LC_ALL':os.fsencode(os.environ.get('MARSH_TEST_LOCALE','C')),b'TERM':b'xterm-256color',b'PS1':(b'\xff' if basic else b'')+h.PROMPT,b'PS2':b'$(printf Q >>ps2)'+h.SECONDARY,b'PROMPT_COMMAND':b'printf P >>pc'}
   c=h.Owned(binary,args,work,env,True);error=None;before=b''
   try:
    c.wait(lambda:h.PROMPT in c.raw);c.send(b'source setup\n');c.wait(lambda:(work/'ready').exists());c.settle()
    c.send(b'\x1b[200~'+payload+b'\x1b[201~');c.settle();before=(work/'command.stdout').read_bytes()
    c.send(b'\nprintf DONE >done\nexit 7\n');c.finish()
   except (TimeoutError,RuntimeError,OSError,subprocess.TimeoutExpired) as e:error=str(e)
   finally:cleanup=c.close()
   stdout=(work/'command.stdout').read_bytes() if (work/'command.stdout').exists() else b''
   rows.append(dict(name=name,posix=posix,basic=basic,source=payload.hex(),setup=setup.hex(),before=before.hex(),stdout=stdout.hex(),pc=(work/'pc').read_bytes().hex() if (work/'pc').exists() else '',ps2=(work/'ps2').read_bytes().hex() if (work/'ps2').exists() else '',status=c.child.returncode,error=error,cleanup=cleanup,pty=bytes(c.raw).hex(),steps=c.steps))
   print(name,posix,basic,repr(before),repr(stdout),'PS2='+rows[-1]['ps2'],'PC='+rows[-1]['pc'],c.child.returncode,cleanup['clean'],error,flush=True)
(out/'receipts.json').write_text(json.dumps(dict(binary=str(binary),sha256=h.sha(binary),receipts=rows),indent=2)+'\n')
raise SystemExit(0 if all(r['cleanup']['clean'] and not r['error'] for r in rows) else 1)
