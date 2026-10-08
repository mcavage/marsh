#!/usr/bin/env python3
"""Signal only owned primary-reader shells; retain exact effects and cleanup."""
import importlib.util,json,os,signal,subprocess,sys,time
from pathlib import Path
ROOT=Path(__file__).resolve().parents[3]
s=importlib.util.spec_from_file_location('h',ROOT/'tests/acceptance/bash_readline_cli.py');h=importlib.util.module_from_spec(s);s.loader.exec_module(h)
binary=Path(sys.argv[2]).resolve();brush=len(sys.argv)>3 and sys.argv[3]=='brush'
out=Path(sys.argv[1]);out.mkdir(parents=True,exist_ok=False);rows=[]
modes=[('reedline',True,False),('basic',True,False),('reedline-noedit',True,True),('basic-noedit',True,True),('pipe-edit',False,False),('pipe-noedit',False,True),('script',False,True)]
for mode,tty,noedit in modes:
 for disposition in ('default','trapped','ignored'):
  for raw in (False,True):
   name=f'{mode}-{disposition}-'+('raw' if raw else 'ascii')
   if len(sys.argv)>4 and sys.argv[4] not in name:continue
   work=out/name;work.mkdir()
   (work/'inputrc').write_bytes(b'set input-meta on\nset convert-meta off\n')
   interactive=mode!='script';backend=b'basic' if mode.startswith('basic') else (b'reedline' if tty else b'minimal')
   args=([b'--no-config',b'--input-backend',backend] if brush else [])+[b'--noprofile',b'--norc']+([b'--noediting'] if noedit else [])+([b'-i'] if interactive else [])+[b'-s']
   env={b'PATH':b'/usr/bin:/bin',b'HOME':os.fsencode(work),b'HISTFILE':os.fsencode((work/'history').resolve()),b'INPUTRC':os.fsencode((work/'inputrc').resolve()),b'LC_ALL':os.fsencode(os.environ.get('MARSH_TEST_LOCALE','C')),b'TERM':b'xterm-256color',b'PS1':(b'\xff' if raw else b'')+h.PROMPT,b'PS2':h.SECONDARY,b'PROMPT_COMMAND':b'printf P >>prompts'}
   setup=(b'set -x; ' if len(sys.argv)>6 and sys.argv[6]=='trace' else b'')+b'''trap 'printf "EXIT:%s" "$?" >exit' EXIT; '''
   if len(sys.argv)>5:
    (work/'x').write_bytes(b'printf X\n')
    setup+=b'f(){ printf F; }; '
   if disposition=='trapped':
    handler=bytes.fromhex(sys.argv[5]) if len(sys.argv)>5 else b'printf "HUP:%s" "$?" >hup'
    setup+=b"trap '"+handler.replace(b"'",b"'\\''")+b"' HUP; "
   if disposition=='ignored':setup+=b"trap '' HUP; "
   setup+=b'printf READY >ready; (exit 17)\n'
   partial=b"printf '"+(b'\xffZ' if raw else b'PART')+b"' >partial"
   tail=b'\nprintf "STATUS:%s" "$?" >status; exit 7\n'
   c=h.Owned(binary,args,work,env,tty);error=None;stdout=stderr=b'';early={};survived=None;sent=[]
   def effects():return {p.name:p.read_bytes().hex() for p in work.iterdir() if p.is_file() and p.name!='inputrc'}
   def send(data):
    sent.append(data.hex())
    if tty:c.send(data)
    else:c.child.stdin.write(data);c.child.stdin.flush()
   def settle(seconds):
    end=time.monotonic()+seconds
    while time.monotonic()<end:
     if tty:c.pump(.02)
     else:c.track();time.sleep(.01)
   try:
    if tty:c.wait(lambda:h.PROMPT in c.raw)
    send(setup);end=time.monotonic()+3
    while not (work/'ready').exists():
     settle(.02)
     if time.monotonic()>end or c.child.poll() is not None:raise RuntimeError('setup handshake')
    settle(.08);send(partial);settle(.08)
    if c.child.poll() is not None:raise RuntimeError('shell exited before owned HUP')
    os.kill(c.child.pid,signal.SIGHUP)
    settle(.2);survived=c.child.poll() is None;early=effects()
    if survived:send(tail)
    if tty:c.finish();stderr=bytes(c.raw)
    else:stdout,stderr=c.child.communicate(timeout=5)
   except (TimeoutError,RuntimeError,OSError,subprocess.TimeoutExpired) as exc:error=str(exc)
   finally:cleanup=c.close()
   row=dict(name=name,mode=mode,disposition=disposition,raw=raw,argv=[x.hex() for x in args],source=setup.hex(),partial=partial.hex(),tail=tail.hex(),sent=sent,signal_target=c.child.pid,status=c.child.returncode,survived=survived,early=early,effects=effects(),stdout=stdout.hex(),stderr=stderr.hex(),pty=bytes(c.raw).hex(),steps=c.steps,error=error,cleanup=cleanup)
   rows.append(row);print(name,c.child.returncode,survived,{k:bytes.fromhex(v) for k,v in row['effects'].items() if k not in ('history','ready')},cleanup['clean'],error,flush=True)
(out/'receipts.json').write_text(json.dumps(dict(binary=str(binary),sha256=h.sha(binary),receipts=rows),indent=2)+'\n')
raise SystemExit(0 if all(r['cleanup']['clean'] and r['error'] is None for r in rows) else 1)
