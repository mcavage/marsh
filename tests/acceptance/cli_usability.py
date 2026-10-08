#!/usr/bin/env python3
"""Linux credential-free real CLI/daemon checks; NOT Mac/stock-SBX qualification.

Requires explicit binaries. Uses the actual daemon/config loaders/Unix socket,
with a stock stand-in accepting only `version` and `create --help`; any workload effect is a failure.
Every public CLI is in a new session, and cleanup signals only observed own PIDs.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import base64
import hashlib
import json
import os
from pathlib import Path
import pwd
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time


def fingerprint(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def tree(root):
    return {str(p.relative_to(root)): {'mode':p.stat().st_mode,
            'sha256':fingerprint(p) if p.is_file() else None}
            for p in root.rglob('*') if not p.is_symlink()}


def process_start(pid):
    try:
        return Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()[19]
    except (OSError, IndexError):
        return None


class Probe:
    def __init__(self, root, binary, evidence):
        self.root, self.binary, self.evidence = root, binary, evidence
        self.children = {}
        self.records = []
        self.scopes = {root/name for name in ['scope', 'fresh-scope', 'overridden-scope', 'parallel-scope', 'stderr-scope', 'orphan-scope']}
        self.environment = {
            'HOME': pwd.getpwuid(os.getuid()).pw_dir,
            'USER': pwd.getpwuid(os.getuid()).pw_name,
            'PATH': '/usr/bin:/bin', 'LC_ALL': 'C',
            'MARSH_HOME': str(root / 'scope'),
            'MARSH_CONTROL_HOME': str(root / 'control'),
            'MARSH_GUEST_ARTIFACTS': str(root / 'artifacts'),
            'MARSH_SBX': str(root / 'stock-version-only'),
            'MARSH_SHELL_IMAGE': 'example/shell@sha256:' + 'a'*64,
        }
        for directory in ['control', 'artifacts', 'project', 'not-login-home']:
            (root / directory).mkdir(mode=0o700)
        (root/'artifacts/commands.json').write_text(json.dumps({'fixture':'example/fixture@sha256:'+'a'*64}))
        (root/'stock-version-only').write_text(
            '#!' + sys.executable + '\nimport sys\nfrom pathlib import Path\n'
            + f'with Path({str(root / "stock.calls")!r}).open("a") as out: out.write(repr(sys.argv[1:])+"\\n")\n'
            + 'if sys.argv[1:] == ["version"]:\n print("sbx version: v0.45.0 ' + 'a'*40 + '")\n sys.exit(0)\n'
            + 'if sys.argv[1:] == ["create", "--help"]:\n print("sandbox kit reference")\n sys.exit(0)\nsys.exit(99)\n')
        (root/'stock-version-only').chmod(0o700)

    def observe_children(self, pid):
        try:
            ids = Path(f'/proc/{pid}/task/{pid}/children').read_text().split()
        except OSError:
            return
        for raw in ids:
            child = int(raw)
            stamp = process_start(child)
            if stamp:
                try:
                    pgid = os.getpgid(child)
                except ProcessLookupError:
                    continue
                try: executable=str(Path(f'/proc/{child}/exe').readlink())
                except OSError: executable=None
                self.children[child] = {'pid': child, 'pgid': pgid, 'start': stamp, 'executable':executable}
                self.observe_children(child)

    def call(self, name, args, timeout=4, env=None, executable=None, interrupt_when=None):
        before = tree(self.root)
        start = time.monotonic()
        stdout = self.evidence / (name+'.stdout')
        stderr = self.evidence / (name+'.stderr')
        with stdout.open('wb') as out, stderr.open('wb') as err:
            child = subprocess.Popen([str(executable or self.binary), *args], cwd=self.root/'project',
                env={**self.environment, **(env or {})}, stdin=subprocess.DEVNULL,
                stdout=out, stderr=err, start_new_session=True)
            pid, pgid = child.pid, os.getpgid(child.pid)
            timed_out = False
            interrupted = False
            while child.poll() is None:
                self.observe_children(child.pid)
                if interrupt_when is not None and interrupt_when.exists():
                    interrupted = True
                    os.killpg(pgid, signal.SIGKILL)
                    break
                if time.monotonic()-start > timeout:
                    timed_out = True
                    os.killpg(pgid, signal.SIGKILL)
                    break
                time.sleep(.005)
            child.wait(timeout=3)
        elapsed = time.monotonic()-start
        data, diagnostic = stdout.read_bytes(), stderr.read_bytes()
        after = tree(self.root)
        record = {'name':name,'args':args,'pid':pid,'pgid':pgid,'status':child.returncode,
            'elapsed_seconds':elapsed,'timeout':timed_out,'intentional_interruption':interrupted,
            'stdout_base64':base64.b64encode(data).decode(),'stderr_base64':base64.b64encode(diagnostic).decode(),
            'changed_files':{key:value for key,value in after.items() if before.get(key)!=value},
            'removed_files':sorted(set(before)-set(after))}
        self.records.append(record)
        return record, data, diagnostic

    def cleanup(self):
        # Only PID identities actually descended from this harness's own calls.
        for entry in self.children.values():
            pid = entry['pid']
            if process_start(pid) != entry['start']:
                continue
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        end = time.monotonic()+3
        while time.monotonic()<end and any(process_start(e['pid'])==e['start'] for e in self.children.values()):
            time.sleep(.02)
        for entry in self.children.values():
            if process_start(entry['pid']) == entry['start']:
                try:
                    os.kill(entry['pid'], signal.SIGKILL)
                except ProcessLookupError:
                    pass
        # Exact private scope runtime only; never remove shared runtime root.
        for scope in self.scopes:
            key=hashlib.sha256(os.fsencode(scope.resolve())).hexdigest()[:16]
            runtime=Path('/tmp')/f'marsh-{os.getuid()}'/key
            if runtime.exists():
                shutil.rmtree(runtime)
            runtime.with_suffix('.startup-lock').unlink(missing_ok=True)
        survivors=[]
        for entry in self.children.values():
            if process_start(entry['pid']) != entry['start']:
                continue
            try:
                state=Path(f'/proc/{entry["pid"]}/stat').read_text().rsplit(')',1)[1].split()[0]
            except OSError:
                continue
            if state!='Z': survivors.append(entry)
        (self.evidence/'owned-children.json').write_text(json.dumps({'observed':list(self.children.values()),'live_after_cleanup':survivors},indent=2)+'\n')
        if survivors:
            raise RuntimeError('owned probe processes survived exact-PID cleanup')


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--marsh',type=Path,required=True)
    parser.add_argument('--evidence',type=Path,required=True)
    parser.add_argument('--baseline',action='store_true',help='retain expected RED observations, do not waive assertions')
    args=parser.parse_args()
    if sys.platform!='linux':
        parser.error('this controlled harness uses Linux process identities; run the distinct exact-host UAT on macOS')
    binary=args.marsh.resolve(strict=True)
    daemon=binary.with_name('marshd')
    if not os.access(binary,os.X_OK) or not daemon.is_file():
        parser.error('explicit executable marsh and sibling marshd required')
    evidence=args.evidence.resolve(); evidence.mkdir(parents=True,exist_ok=True)
    checks=[]
    def check(name, condition): checks.append({'check':name,'passed':bool(condition)})
    with tempfile.TemporaryDirectory(prefix='marsh-cli-usability-') as temp:
        probe=Probe(Path(temp),binary,evidence)
        try:
            # Pure help must work even with bad HOME and no viable daemon config.
            for name, words in [('top-help',['--help']),('cloud-help',['cloud','--help'])]:
                r,out,err=probe.call(name,words,env={'HOME':str(probe.root/'not-login-home')})
                check(name, r['status']==0 and not err and b'Usage:' in out and not r['changed_files'])
                if name=='top-help':
                    check('complete top help',all(text in out for text in [b'acp install-published',b'--dev',b'--all-unchanged',b'cloud recovery release']))
            r,out,err=probe.call('invalid-kit-reference',['kit','install','fixture','--from','bar'],env={'HOME':str(probe.root/'not-login-home')})
            check('invalid Kit reference before effects',r['status']==2 and b'sha256' in err and not out and not r['changed_files'])
            r,out,err=probe.call('absent-scope',['stop','--json'],env={'MARSH_HOME':str(probe.root/'fresh-scope')})
            try: report=json.loads(out)
            except ValueError: report={}
            check('absent scope reports nothing running without start',r['status']==0 and report.get('cleanup_complete') is True and report.get('components')==[] and err.startswith(b'marsh: nothing running for ') and not r['changed_files'] and not (probe.root/'fresh-scope').exists())
            # The real CLI with its daemon sibling deliberately absent.
            isolated=probe.root/'isolated'; isolated.mkdir(mode=0o700)
            missing=isolated/'marsh'; shutil.copy2(binary,missing)
            r,out,err=probe.call('missing-daemon',['status','--json'],executable=missing)
            check('missing daemon fails without workload effects',r['status']==1 and not out and bool(err) and r['elapsed_seconds']<1 and not (probe.root/'stock.calls').exists())
            # Known real-child failure must not wait for endpoint timeout.
            r,out,err=probe.call('invalid-home',['status','--json'],timeout=33,env={'HOME':str(probe.root/'not-login-home')})
            check('immediate real startup failure',r['status']==1 and r['elapsed_seconds']<1 and b'HOME' in err and b'exit status: 1' in err and b'inconsistent' not in err and not out)
            scope=probe.root/'scope'; scope.mkdir(mode=0o700,exist_ok=True)
            control=probe.root/'control'/hashlib.sha256(os.fsencode(scope.resolve())).hexdigest()
            control.mkdir(mode=0o700,exist_ok=True)
            config=control/'commands.json'; config.write_text('{"fixture": "secret-canary-malformed-JSON'); config.chmod(0o600)
            r,out,err=probe.call('malformed-registry',['status','--json'],timeout=33)
            check('safe actionable registry failure',r['status']==1 and r['elapsed_seconds']<1 and os.fsencode(config) in err and b'secret-canary' not in err and not out)
            config.unlink()
            control.chmod(0o755)
            r,out,err=probe.call('unsafe-control',['status','--json'],timeout=33)
            check('unsafe control immediate actionable',r['status']==1 and r['elapsed_seconds']<1 and os.fsencode(control) in err and not out)
            control.chmod(0o700)
            cloud=control/'cloud.yaml'; cloud.write_text('version: 1\nenabled: secret-canary-yaml\n'); cloud.chmod(0o600)
            r,out,err=probe.call('malformed-cloud',['status','--json'],timeout=33)
            check('safe actionable cloud failure',r['status']==1 and r['elapsed_seconds']<1 and os.fsencode(cloud) in err and b'secret-canary' not in err and not out)
            cloud.unlink()
            os.mkfifo(cloud,0o600)
            r,out,err=probe.call('nonregular-cloud-config',['status','--json'])
            check('nonregular config cannot block startup',r['status']==1 and r['elapsed_seconds']<1 and os.fsencode(cloud) in err and b'regular file' in err and not out)
            cloud.unlink()
            r,out,err=probe.call('invalid-job-limit',['status','--json'],timeout=33,env={'MARSH_JOB_PIDS':'0'})
            check('invalid limits immediate',r['status']==1 and r['elapsed_seconds']<1 and b'MARSH_JOB_PIDS' in err and b'inconsistent' not in err and not out)
            # An adversarial child fixture supplements (does not replace) the
            # real-daemon failures above: overflow must retain a bounded reason
            # and the actual child exit, without a blocked reader thread.
            noisy=probe.root/'noisy'; noisy.mkdir(mode=0o700)
            shutil.copy2(binary,noisy/'marsh')
            noisy_daemon=noisy/'marshd'
            noisy_daemon.write_text('#!'+sys.executable+'\nimport sys\nsys.stderr.buffer.write(b"x"*262144+b"\\nBOOT_FAIL_FIXTURE\\n")\nsys.stderr.buffer.flush()\nsys.exit(37)\n')
            noisy_daemon.chmod(0o700)
            r,out,err=probe.call('bounded-child-diagnostic',['status','--json'],executable=noisy/'marsh',env={'MARSH_HOME':str(probe.root/'stderr-scope')})
            check('bounded diagnostic actual child status',r['status']==1 and r['elapsed_seconds']<1 and b'exit status: 37' in err and b'BOOT_FAIL_FIXTURE' in err and len(err)<9000 and not out)
            # Real authenticated daemon; only compatibility introspection is permitted.
            r,out,err=probe.call('status-json',['status','--json'])
            try: status=json.loads(out)
            except ValueError: status={}
            expected={'cpu_millis':4000,'memory_bytes':8589934592,'pids':4096,'writable_bytes':10737418240,'output_bytes':268435456,'wall_seconds':86400}
            check('status actual path/defaults',r['status']==0 and not err and status.get('control_home')==str(control) and status.get('job_defaults',{}).get('resources')==expected)
            r,out,err=probe.call('status-human',['status'])
            check('default human status',r['status']==0 and not err and os.fsencode(control) in out and all(str(n).encode() in out for n in expected.values()))
            r,out,err=probe.call('resident-env-conflict',['status','--json'],env={'MARSH_JOB_PIDS':'23'})
            check('later conflicting settings rejected',r['status']==1 and b'startup snapshot' in err and not out)
            r,out,err=probe.call('resident-still-stable',['status','--json'])
            try: repeated=json.loads(out)
            except ValueError: repeated={}
            check('resident identity preserved',r['status']==0 and repeated.get('daemon_id')==status.get('daemon_id') and repeated.get('endpoint_owner')==status.get('endpoint_owner'))
            r,out,err=probe.call('first-launch-override',['status','--json'],env={'MARSH_HOME':str(probe.root/'overridden-scope'),'MARSH_JOB_PIDS':'23'})
            try: overridden=json.loads(out)
            except ValueError: overridden={}
            check('effective startup override and origin',r['status']==0 and not err and overridden.get('job_defaults',{}).get('resources',{}).get('pids')==23 and overridden.get('job_defaults',{}).get('environment_overrides')==['MARSH_JOB_PIDS'])
            before=(probe.root/'stock.calls').read_text().splitlines() if (probe.root/'stock.calls').exists() else []
            barrier=threading.Barrier(2)
            def concurrent_start(index):
                barrier.wait(timeout=2)
                return probe.call('simultaneous-start-'+str(index),['status','--json'],env={'MARSH_HOME':str(probe.root/'parallel-scope')})
            with ThreadPoolExecutor(max_workers=2) as pool:
                pairs=list(pool.map(concurrent_start,[0,1]))
            simultaneous=[]
            for record,out,err in pairs:
                try: document=json.loads(out)
                except ValueError: document={}
                simultaneous.append(document)
                check(record['name'],record['status']==0 and not err and bool(document.get('daemon_id')))
            calls=(probe.root/'stock.calls').read_text().splitlines() if (probe.root/'stock.calls').exists() else []
            check('one simultaneous startup owner/effect sequence',len(calls)-len(before)==2 and bool(simultaneous[0].get('daemon_id')) and simultaneous[0].get('daemon_id')==simultaneous[1].get('daemon_id'))
            # Kill only the first client after a real daemon reaches a delayed,
            # read-only compatibility probe. Its startup ownership must survive.
            marker=probe.root/'startup-observed'
            delayed=probe.root/'delayed-stock'
            script=(probe.root/'stock-version-only').read_text().replace('import sys\n','import sys, time\n')
            script=script.replace('if sys.argv[1:] == ["version"]:\n',f'if sys.argv[1:] == ["version"]:\n Path({str(marker)!r}).write_text("started")\n time.sleep(2)\n')
            delayed.write_text(script); delayed.chmod(0o700)
            env={'MARSH_HOME':str(probe.root/'orphan-scope'),'MARSH_SBX':str(delayed)}
            before_children=set(probe.children)
            before_calls=len(calls)
            first,_,_=probe.call('interrupted-first-launch',['status','--json'],env=env,interrupt_when=marker)
            check('interruption occurs after actual startup',first['intentional_interruption'] and first['status']==-signal.SIGKILL)
            second,out,err=probe.call('rejoin-interrupted-startup',['status','--json'],env=env)
            try: rejoined=json.loads(out)
            except ValueError: rejoined={}
            new_daemons=[entry['pid'] for pid,entry in probe.children.items() if pid not in before_children and entry.get('executable')==str(daemon)]
            calls=(probe.root/'stock.calls').read_text().splitlines() if (probe.root/'stock.calls').exists() else []
            check('client death preserves one startup owner',second['status']==0 and not err and len(new_daemons)==1 and rejoined.get('endpoint_owner',{}).get('pid')==new_daemons[0] and len(calls)-before_calls==2)
            check('zero workload/Cloud/provider effects',all(line in ["['version']", "['create', '--help']"] for line in calls))
            (evidence/'stock.calls').write_text('\n'.join(calls)+'\n')
        finally:
            probe.cleanup()
            result={'binary':str(binary),'binary_sha256':fingerprint(binary),'daemon_sha256':fingerprint(daemon),
                'baseline':args.baseline,'checks':checks,'calls':probe.records,'passed':all(c['passed'] for c in checks)}
            (evidence/'result.json').write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps({'passed':result['passed'],'checks':checks},indent=2))
    return 0 if result['passed'] else 1


if __name__=='__main__':
    sys.exit(main())
