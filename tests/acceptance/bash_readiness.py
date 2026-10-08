#!/usr/bin/env python3
"""Finite GNU comparisons for character-device input readiness and logical high FDs.

Actual OS descriptor2048 coverage lives in the exact-production poll component
probe; shell logical descriptor2048 need not map to the same OS descriptor.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

CASES = {
    'stdin-null': None,
    'read-null': b'read -r x </dev/null; printf "R:%s:%s\\n" "$?" "$x"',
    'timed-null': b'read -r -t .02 x </dev/null; printf "R:%s:%s\\n" "$?" "$x"',
    'zero-delimiter': b'read -r -d "" -t 1 x </dev/zero; printf "R:%s:%s\\n" "$?" "$x"',
    'high-null-ready': b'ulimit -n 4096; exec 2048</dev/null; read -t 0 -u 2048 x; printf "R:%s\\n" "$?"',
    'high-zero-ready': b'ulimit -n 4096; exec 2048</dev/zero; read -t 0 -u 2048 x; printf "R:%s\\n" "$?"',
    'high-closed': b'ulimit -n 4096; exec 2048</dev/null; exec 2048<&-; read -t 0 -u 2048 x; printf "R:%s\\n" "$?"',
}

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--candidate',type=Path,required=True)
    parser.add_argument('--oracle',type=Path,required=True)
    parser.add_argument('--evidence',type=Path,required=True)
    args=parser.parse_args(); args.evidence.mkdir(parents=True,exist_ok=True)
    binaries={'gnu':args.oracle.resolve(),'candidate':args.candidate.resolve()}
    rows=[]
    with tempfile.TemporaryDirectory(prefix='bash-readiness-') as temporary:
        root=Path(temporary)
        for name,script in CASES.items():
            records={}
            for who,shell in binaries.items():
                home=root/who;home.mkdir(exist_ok=True)
                env={'PATH':'/usr/bin:/bin','LC_ALL':'C','HOME':str(home),'HISTFILE':str(home/'history')}
                argv=[os.fsencode(shell),b'--noprofile',b'--norc']
                argv += [b'-s'] if script is None else [b'-c',script,b'readiness']
                start=time.monotonic()
                process=subprocess.Popen(argv,stdin=subprocess.DEVNULL,stdout=subprocess.PIPE,stderr=subprocess.PIPE,cwd=root,env=env,start_new_session=True)
                timed_out=False
                try: stdout,stderr=process.communicate(timeout=3)
                except subprocess.TimeoutExpired:
                    timed_out=True
                    process.kill()  # This retained owned process; cases contain no external children.
                    stdout,stderr=process.communicate(timeout=2)
                records[who]={'pid':process.pid,'status':process.returncode,'stdout':stdout.hex(),'stderr':stderr.hex(),'timeout':timed_out,'elapsed':time.monotonic()-start}
            keys=('status','stdout','stderr','timeout')
            rows.append({'case':name,'runs':records,'exact':all(records['gnu'][k]==records['candidate'][k] for k in keys)})
    identity={who:{'path':str(path),'sha256':hashlib.sha256(path.read_bytes()).hexdigest()} for who,path in binaries.items()}
    identity['probe_sha256']=hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    summary={'total':len(rows),'exact':sum(row['exact'] for row in rows),'timeouts':sum(run['timeout'] for row in rows for run in row['runs'].values())}
    for name,value in [('identity',identity),('observations',rows),('summary',summary)]:
        (args.evidence/(name+'.json')).write_text(json.dumps(value,indent=2)+'\n')
    print(json.dumps(summary))
    return int(summary['exact']!=summary['total'] or summary['timeouts']!=0)

if __name__=='__main__':
    raise SystemExit(main())
