#!/usr/bin/env python3
"""Real npm-notice collector CLI against canonical texts and synthetic package metadata.

No npm install, package scripts, agent or Docker execution. Does not qualify a Kit.
"""
import argparse
import ctypes
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys

ROOT=Path(__file__).absolute().parents[2]
KITS=['marsh-pi','marsh-codex','marsh-claude']


def sha(p):return hashlib.sha256(p.read_bytes()).hexdigest()


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--work',type=Path,required=True);p.add_argument('--inside',action='store_true');a=p.parse_args();w=a.work.absolute()
    if not a.inside:
        w.mkdir(mode=0o700,exist_ok=False)
        canonical=w/'share/licenses/marsh-dhi'
        shutil.copytree(ROOT/'packaging/dhi-notices/collected',canonical)
        for name in KITS:
            context=w/name;context.mkdir();(context/'notices').mkdir();modules=context/'node_modules';modules.mkdir()
            shutil.copy2(ROOT/'scripts/npm-notices.mjs',context/'collect-notices.mjs')
            shutil.copy2(ROOT/'kits'/name/'notices/overrides.json',context/'notices/overrides.json')
            (context/'package-lock.json').write_text('{"fixture":"synthetic installed metadata, no package code"}\n')
            overrides=json.loads((context/'notices/overrides.json').read_text())
            for key,items in overrides.items():
                if key.startswith('$'):continue
                package,version=key.rsplit('@',1);directory=modules/package;directory.mkdir(parents=True,exist_ok=True)
                (directory/'package.json').write_text(json.dumps({'name':package,'version':version}))
        command=['unshare','-rm',sys.executable,'-I',str(Path(__file__).absolute()),'--inside','--work',str(w)]
        result=subprocess.run(command,timeout=120)
        (w/'runner.json').write_text(json.dumps({'argv':command,'exit':result.returncode,'collector_sha256':sha(ROOT/'scripts/npm-notices.mjs')},indent=2)+'\n')
        return result.returncode
    libc=ctypes.CDLL(None,use_errno=True)
    if libc.mount(b'none',b'/',None,(1<<18)|16384,None) or libc.mount(str(w/'share').encode(),b'/usr/local/share',None,4096,None):
        raise OSError(ctypes.get_errno(),'isolated canonical fixture mount failed')
    outcomes=[]
    def run(name,label,expected):
        c=w/name;argv=['/usr/bin/node',str(c/'collect-notices.mjs'),str(c/'node_modules'),str(c/label)]
        result=subprocess.run(argv,env={'PATH':'/usr/bin:/bin'},capture_output=True,timeout=30)
        (c/(label+'.stdout')).write_bytes(result.stdout);(c/(label+'.stderr')).write_bytes(result.stderr)
        outcomes.append({'kit':name,'control':label,'argv':argv,'exit':result.returncode,'expected_success':expected,'as_expected':(result.returncode==0)==expected,'collector_sha256':sha(c/'collect-notices.mjs')})
        if result.returncode==0:
            document=json.loads((c/label/'npm-package-notices.json').read_text())
            for package in document['packages']:
                for notice in package['notices']:
                    assert sha(c/label/'texts'/(notice['sha256']+'.txt'))==notice['sha256']
        return result
    for name in KITS:run(name,'positive-canonical-no-local-texts',True)
    c=w/'marsh-claude';overrides=c/'notices/overrides.json';original=overrides.read_bytes();d=json.loads(original)
    d['$canonical_notice_files']['standardwebhooks-1.1.1-LICENSE']='../../outside';overrides.write_text(json.dumps(d))
    run('marsh-claude','negative-traversal-map',False);overrides.write_bytes(original)
    canonical=w/'share/licenses/marsh-dhi/provider-notices/standardwebhooks-1.1.1-LICENSE';data=canonical.read_bytes()
    canonical.write_bytes(data+b'\ncorrupt\n');run('marsh-claude','negative-canonical-hash',False);canonical.write_bytes(data)
    saved=w/'saved-notice';canonical.rename(saved);run('marsh-claude','negative-missing-canonical',False);saved.rename(canonical)
    run('marsh-claude','restored-positive',True)
    (w/'results.json').write_text(json.dumps({'scope':'real collector CLI; synthetic package manifests, real source-attributed canonical legal bytes; not image/npm install proof','controls':outcomes},indent=2)+'\n')
    print(json.dumps(outcomes,indent=2))
    return int(not all(r['as_expected'] for r in outcomes))


if __name__=='__main__':raise SystemExit(main())
