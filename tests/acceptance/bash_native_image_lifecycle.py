import argparse, hashlib, json, os, pathlib, signal, subprocess, tempfile, time
p=argparse.ArgumentParser();p.add_argument('--candidate',required=True);p.add_argument('--out',required=True);p.add_argument('--interpose',required=True);p.add_argument('--short',action='store_true');p.add_argument('--case',action='append');a=p.parse_args()
exe=pathlib.Path(a.candidate).resolve(); interpose=pathlib.Path(a.interpose).resolve(); rows=[]
cases=[('direct-exec','',b'exec /usr/bin/true',0,b'',0),('pipeline-exec','',b'/usr/bin/printf OK | /bin/cat',0,b'OK',1),('umask-native','',b'umask 777; printf "<%s>" "$(printf RAW)"; (printf SUB)',0,b'<RAW>SUB',1),('umask-copy','force-copy',b'umask 777; (printf COPY)',0,b'COPY',1),('persistent-cache','',b'for n in {1..30}; do (printf x); done',0,b'x'*30,1),('shared-regex-native','',b'[[ a =~ (a) ]]; printf \"%s\" \"${BASH_REMATCH[1]}\"; (printf B); [[ c =~ (c) ]]; printf \"%s\" \"${BASH_REMATCH[1]}\"',0,b'aBc',1),('setup-failure','setup-fail',b'(printf BAD)',None,b'',1),('clone-failure','clone-fail',b'(printf BAD)',None,b'',1),('enoexec','',b'exec ./script',0,b'SCRIPT',1),('failed-exec-return','exec-fail',b'shopt -s execfail; exec ./script; printf RESTORED',0,b'RESTORED',1)]
cases += [('creation-uncertain','create-cleanup-fail',b'(printf BAD)',125,b'',1),('creation-uncertain-exec-guard','create-cleanup-fail',b'shopt -s execfail; exec ./script; (printf BAD); printf DONE',125,b'',1),('setup-uncertain','setup-cleanup-fail',b'(printf BAD)',125,b'',1)]
if not a.short:cases += [('malformed-ready','bad-ready',b'(printf BAD)',None,b'',1),('cleanup-uncertain','cleanup-fail',b'(printf BAD)',125,b'BAD',1),('early-child-exit','early-exit',b'(printf BAD)',None,b'',1),('startup-deadline','delay',b'(printf BAD)',None,b'',1),('startup-interrupt','cancel',b'(printf BAD)',-2,b'',1)]
cases += [('wrong-delegated-link','wrong-link',b'(printf BAD)',None,b'',2)]
if a.case:cases=[c for c in cases if c[0] in a.case]
for name,mode,source,want_status,want_out,want_dirs in cases:
 with tempfile.TemporaryDirectory(prefix='brush-native-lifecycle-') as temp:
  root=pathlib.Path(temp);(root/'history').touch(mode=0o600);log=root/'paths';log.touch(mode=0o600);script=root/'script';script.write_bytes(b'printf SCRIPT\n');script.chmod(0o755);wrong=root/'wrong';wrong.write_bytes(b'WRONG');wrong.chmod(0o700)
  env={'PATH':'/usr/bin:/bin','HOME':temp,'HISTFILE':temp+'/history','LC_ALL':'C','DYLD_INSERT_LIBRARIES':str(interpose),'BRUSH_PROBE_LOG':str(log),'BRUSH_PROBE_MODE':mode,'BRUSH_PROBE_WRONG':str(wrong)}
  start=time.monotonic();child=subprocess.Popen([str(exe),'--noprofile','--norc','-c',source],cwd=temp,env=env,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True);forced=False
  try:
   if mode=='cancel':
    until=time.monotonic()+4
    while b'CHILD ' not in log.read_bytes() and child.poll() is None and time.monotonic()<until:time.sleep(.01)
    if child.poll() is None:os.killpg(child.pid,signal.SIGINT)
   out,err=child.communicate(timeout=12)
  except subprocess.TimeoutExpired:
   forced=True;os.killpg(child.pid,signal.SIGKILL);out,err=child.communicate(timeout=3)
  finally:
   if child.poll() is None:
    os.killpg(child.pid,signal.SIGKILL);child.wait(timeout=3)
  elapsed=time.monotonic()-start
  events=[line.split(' ',2)for line in log.read_text().splitlines()]; dirs=[pathlib.Path(e[2])for e in events if e[0]=='DIR'];remaining=[];cleanups=[]
  for directory in dirs:
   if directory.exists():
    st=directory.lstat(); remaining.append({'path':str(directory),'dev':st.st_dev,'ino':st.st_ino,'uid':st.st_uid,'mode':st.st_mode})
    assert directory.parent==pathlib.Path('/private/tmp') and directory.name.startswith('brush-regex-image-') and st.st_uid==os.getuid()
    # Only the exact path emitted by this owned session is eligible; no glob.
    directory.chmod(0o700); entries=list(directory.iterdir());assert all(e.name=='image' for e in entries)
    for entry in entries:entry.unlink()
    directory.rmdir();cleanups.append(str(directory))
  # A delegated child now has its own independent name. The old one-directory
  # count would reject the intended lifecycle. Require exact cleanup or explicit
  # uncertainty for every retained artifact, and count actual clone operations.
  uncertain=mode in ('cleanup-fail','create-cleanup-fail','setup-cleanup-fail','wrong-link')
  remaining_ok=not remaining if not uncertain else (bool(remaining) and all(record['path'].encode() in err for record in remaining))
  clone_attempts=sum(event[0]=='CLONE_ATTEMPT' for event in events)
  clone_ok=(clone_attempts==1) if mode=='' and want_dirs else True
  if name=='direct-exec':clone_ok=clone_attempts==0 and not dirs
  status_ok=child.returncode==want_status if want_status is not None else child.returncode!=0
  rows.append({'case':name,'status':child.returncode,'stdout_hex':out.hex(),'stderr_hex':err.hex(),'elapsed':elapsed,'forced':forced,'events':events,'clone_attempts':clone_attempts,'remaining_before_fixture_cleanup':remaining,'fixture_cleanup':cleanups,'pass':status_ok and out==want_out and not forced and remaining_ok and clone_ok and (name!='creation-uncertain-exec-guard' or b'cache admission is closed' in err)})
result={'candidate':str(exe),'candidate_sha256':hashlib.sha256(exe.read_bytes()).hexdigest(),'probe_sha256':hashlib.sha256(pathlib.Path(__file__).read_bytes()).hexdigest(),'interpose_sha256':hashlib.sha256(interpose.read_bytes()).hexdigest(),'rows':rows};pathlib.Path(a.out).write_text(json.dumps(result,indent=2)+'\n');print(json.dumps([{k:r[k]for k in ['case','status','elapsed','pass']}for r in rows],indent=2))

failed=[row['case'] for row in rows if not row['pass']]
result['summary']={'cases':len(rows),'passed':len(rows)-len(failed),'failed':failed}
pathlib.Path(a.out).write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps(result['summary']))
raise SystemExit(bool(failed))
