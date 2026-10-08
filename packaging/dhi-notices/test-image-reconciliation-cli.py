#!/usr/bin/env python3
"""Real reconciliation CLI controls using the supplied root public-image receipt."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys

HERE=Path(__file__).absolute().parent
ROOT=HERE.parent.parent


def main():
    p=argparse.ArgumentParser(description=__doc__);p.add_argument('--work',type=Path,required=True);a=p.parse_args()
    work=a.work.absolute();work.mkdir(mode=0o700,exist_ok=False)
    original=ROOT/'target/marsh-evidence/public-dhi-primary-bytes-20261001';source=work/'primary'
    shutil.copytree(original,source)
    results=[]
    def run(label,expected):
        argv=[sys.executable,'-I',str(HERE/'reconcile-image-legal.py'),'--receipt',str(source/'receipt.json'),'--bundle',str(HERE/'collected'),'--output',str(work/label)]
        r=subprocess.run(argv,capture_output=True,timeout=30)
        (work/(label+'.stdout')).write_bytes(r.stdout);(work/(label+'.stderr')).write_bytes(r.stderr)
        results.append({'control':label,'argv':argv,'exit':r.returncode,'expected_success':expected,'as_expected':(r.returncode==0)==expected,'stdout_sha256':hashlib.sha256(r.stdout).hexdigest(),'stderr_sha256':hashlib.sha256(r.stderr).hexdigest()})
        return r
    result=run('actual-primary-positive',True)
    if result.returncode==0:
        d=json.loads(result.stdout);assert d['x_sys']['spdx_purl'].endswith('@v0.46.0')
        assert d['x_sys']['vendor_spdx_inconsistencies']['versionInfo']==d['clipboard_commit']
        assert d['spdx_declared_license']=='LicenseRef-Proprietary'
    receipt=source/'receipt.json';saved=receipt.read_bytes();d=json.loads(saved)
    d['files'][0]['base64']='AAAA';receipt.write_text(json.dumps(d));run('changed-decoded-source-denied',False);receipt.write_bytes(saved)
    raw=source/'collector.stdout';data=raw.read_bytes();raw.write_bytes(data+b'changed');run('changed-root-output-denied',False);raw.write_bytes(data)
    d=json.loads(saved);d['image_id']='sha256:'+'0'*64;receipt.write_text(json.dumps(d));run('mismatched-image-id-denied',False);receipt.write_bytes(saved)
    real=work/'real-receipt.json';receipt.rename(real);receipt.symlink_to(real);run('symlink-receipt-denied',False);receipt.unlink();real.rename(receipt)
    run('restored-primary-positive',True)
    (work/'results.json').write_text(json.dumps({'scope':'Actual reconciliation CLI and root-provided public bytes, no Docker execution in this worker','controls':results},indent=2)+'\n')
    print(json.dumps(results,indent=2));return int(not all(r['as_expected'] for r in results))


if __name__=='__main__':raise SystemExit(main())
