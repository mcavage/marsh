import argparse,hashlib,json,os,pathlib,signal,subprocess,tempfile,time
p=argparse.ArgumentParser();p.add_argument('--candidate');p.add_argument('--oracle',default='/opt/homebrew/bin/bash');p.add_argument('--case',action='append');p.add_argument('--out',required=True);a=p.parse_args()
cases={
'no-jobs':b'disown; printf "R:%s\\n" "$?"',
'help':b'disown --help; printf "R:%s\\n" "$?"',
'invalid-option':b'disown -z; printf "R:%s\\n" "$?"',
'option-cluster':b'disown -azh; printf "R:%s\\n" "$?"',
'all-empty':b'disown -a; printf "R:%s\\n" "$?"',
'running-empty':b'disown -r; printf "R:%s\\n" "$?"',
'hup-empty':b'disown -h; printf "R:%s\\n" "$?"',
'all-hup-empty':b'disown -ah; printf "R:%s\\n" "$?"',
'bad-job':b'disown %88; printf "R:%s\\n" "$?"',
'bad-pid':b'disown 99999999; printf "R:%s\\n" "$?"',
'bad-text':b'disown abc; printf "R:%s\\n" "$?"',
'raw-text':b'disown "\xff\xfe"; printf "R:%s\\n" "$?"',
'empty':b'disown ""; printf "R:%s\\n" "$?"',
'zero':b'disown 0; printf "R:%s\\n" "$?"',
'negative':b'disown -- -1; printf "R:%s\\n" "$?"',
'current':b'/bin/sleep .02 & p=$!; disown; printf "D:%s\\n" "$?"; jobs; wait "$p"; printf "W:%s\\n" "$?"',
'hup-retained':b'/bin/sleep .02 & p=$!; disown -h; printf "D:%s\\n" "$?"; jobs -p >/dev/null; wait "$p"; printf "W:%s\\n" "$?"',
'pid':b'/bin/sleep .02 & p=$!; disown "$p"; printf "D:%s\\n" "$?"; jobs; wait "$p"; printf "W:%s\\n" "$?"',
'number-job':b'/bin/sleep .02 & p=$!; disown 1; printf "D:%s\\n" "$?"; jobs -p >/dev/null; wait "$p"; printf "W:%s\\n" "$?"',
'percent-job':b'/bin/sleep .02 & p=$!; disown %1; printf "D:%s\\n" "$?"; jobs; wait "$p"; printf "W:%s\\n" "$?"',
'prefix-job':b'/bin/sleep .02 & p=$!; disown %/bin/sl; printf "D:%s\\n" "$?"; jobs; wait "$p"; printf "W:%s\\n" "$?"',
'substring-job':b'/bin/sleep .02 & p=$!; disown %?sleep; printf "D:%s\\n" "$?"; jobs; wait "$p"; printf "W:%s\\n" "$?"',
'all':b'/bin/sleep .02 & /bin/sleep .03 & disown -a; printf "D:%s\\n" "$?"; jobs; wait; printf "W:%s\\n" "$?"',
'all-operand':b'/bin/sleep .02 & disown -a %88; printf "D:%s\\n" "$?"; jobs; wait',
'running-operand':b'/bin/sleep .02 & disown -r %88; printf "D:%s\\n" "$?"; jobs; wait',
'mixed-valid-invalid':b'/bin/sleep .02 & disown %1 %88; printf "D:%s\\n" "$?"; jobs; wait',
'duplicate':b'/bin/sleep .02 & disown %1 %1; printf "D:%s\\n" "$?"; jobs; wait',
'after-wait':b'/usr/bin/true & p=$!; wait "$p"; disown %1; printf "D:%s\\n" "$?"',
'command-substitution':b'/bin/sleep .02 & printf "[%s]\\n" "$(disown; printf R:%s "$?")"; wait',
'subshell':b'/bin/sleep .02 & (disown; printf "R:%s\\n" "$?"); wait',
'function':b'f(){ /bin/sleep .02 & }; f; disown; printf "D:%s\\n" "$?"; jobs; wait',
}
cases.update({
'after-wait-default':b'/usr/bin/true & p=$!; wait "$p"; disown; printf "D:%s\\n" "$?"',
'after-jobs':b'/usr/bin/true & p=$!; wait "$p"; jobs; disown %1; printf "D:%s\\n" "$?"',
'after-double-disown':b'/usr/bin/true & p=$!; wait "$p"; disown %1; printf "A:%s\\n" "$?"; disown %1; printf "B:%s\\n" "$?"',
'after-wait-pid':b'/usr/bin/true & p=$!; wait "$p"; disown "$p"; printf "D:%s\\n" "$?"',
'after-false-wait':b'(exit 7) & p=$!; wait "$p"; disown %1; printf "D:%s\\n" "$?"',
'disowned-wait-status':b'(exit 7) & p=$!; disown; wait "$p"; printf "W:%s\\n" "$?"',
})
cases.update({
 'after-wait-hup-repeat':b'/usr/bin/true & p=$!; wait "$p"; disown -h %1; printf "A:%s\\n" "$?"; disown %1; printf "B:%s\\n" "$?"',
 'after-wait-prefix':b'/usr/bin/true & p=$!; wait "$p"; disown %/usr/bin/tr; printf "R:%s\\n" "$?"',
 'after-wait-substring':b'/usr/bin/true & p=$!; wait "$p"; disown %?true; printf "R:%s\\n" "$?"',
 'after-wait-all':b'/usr/bin/true & wait; disown %1; printf "R:%s\\n" "$?"',
 'after-wait-jobs-r':b'/usr/bin/true & p=$!; wait "$p"; jobs -r; disown %1; printf "R:%s\\n" "$?"',
 'after-wait-disown-all':b'/usr/bin/true & p=$!; wait "$p"; disown -a; disown %1; printf "R:%s\\n" "$?"',
 'after-wait-disown-running':b'/usr/bin/true & p=$!; wait "$p"; disown -r; disown %1; printf "R:%s\\n" "$?"',
 'after-wait-foreground':b'/usr/bin/true & p=$!; wait "$p"; /usr/bin/true; disown %1; printf "R:%s\\n" "$?"',
 'after-wait-twice':b'/usr/bin/true & p=$!; wait "$p"; wait "$p"; disown %1; printf "R:%s\\n" "$?"',
})
if a.case:cases={name:body for name,body in cases.items() if name in a.case}
bins={'GNU':str(pathlib.Path(a.oracle).resolve())}
if a.candidate:bins['candidate']=str(pathlib.Path(a.candidate).resolve())
if os.uname().sysname == 'Linux':
 import ctypes
 libc=ctypes.CDLL(None,use_errno=True)
 if libc.prctl(36,1,0,0,0) != 0:raise OSError(ctypes.get_errno(),'PR_SET_CHILD_SUBREAPER')
rows=[]
for label,binary in bins.items():
 for name,source in cases.items():
  with tempfile.TemporaryDirectory(prefix='marsh-disown-') as td:
   d=pathlib.Path(td);(d/'history').touch(mode=0o600)
   env={'HOME':td,'HISTFILE':str(d/'history'),'PATH':'/usr/bin:/bin','LC_ALL':'C'}
   child=subprocess.Popen([os.fsencode(binary),b'--noprofile',b'--norc',b'-c',source,b'auditshell'],cwd=td,env=env,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
   forced=False
   try:out,err=child.communicate(timeout=5)
   except subprocess.TimeoutExpired:forced=True;os.killpg(child.pid,signal.SIGKILL);out,err=child.communicate(timeout=3)
   deadline=time.monotonic()+1
   survivors=[]
   descendant_receipts=[]
   while True:
    table=subprocess.check_output(['/bin/ps','-axo','pid=,pgid='],text=True)
    survivors=[int(fields[0]) for line in table.splitlines() if len(fields:=line.split())==2 and int(fields[1])==child.pid]
    for pid in survivors:
     try:
      got,status=os.waitpid(pid,os.WNOHANG)
      if got:descendant_receipts.append({'pid':got,'wait_status':status})
     except ChildProcessError:pass
    survivors=[pid for pid in survivors if pid not in {r['pid'] for r in descendant_receipts}]
    if not survivors or time.monotonic()>=deadline:break
    time.sleep(.01)
   for pid in survivors:
    try:os.kill(pid,signal.SIGKILL)
    except ProcessLookupError:pass
   rows.append({'descendant_receipts':descendant_receipts,'survivors':survivors,'label':label,'case':name,'status':child.returncode,'stdout_hex':out.hex(),'stderr_hex':err.hex(),'timeout':forced})
result={'identities':{label:hashlib.sha256(pathlib.Path(binary).read_bytes()).hexdigest() for label,binary in bins.items()},'sources':{name:source.hex() for name,source in cases.items()},'rows':rows}
pathlib.Path(a.out).write_text(json.dumps(result,indent=2)+'\n')
for r in rows:
 print(r['label'],r['case'],r['status'],bytes.fromhex(r['stdout_hex'])[:120],bytes.fromhex(r['stderr_hex'])[:160])

failed=[]
for name in cases:
 selected={row['label']:row for row in rows if row['case']==name}
 if any(row['timeout'] or row['survivors'] for row in selected.values()):failed.append(name)
 elif a.candidate and any(selected['GNU'][field]!=selected['candidate'][field] for field in ('status','stdout_hex','stderr_hex')):failed.append(name)
result['summary']={'cases':len(cases),'exact':len(cases)-len(failed),'failed':failed}
pathlib.Path(a.out).write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps(result['summary']))
raise SystemExit(bool(failed))
