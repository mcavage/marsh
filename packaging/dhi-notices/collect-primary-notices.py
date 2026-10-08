#!/usr/bin/env python3
"""Collect exact hash-pinned native source archive notices; execute no source code."""
import argparse
import concurrent.futures
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import threading
from public_sources import acquire, members, read_input, private_output

from notice_rules import notice_material


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--plan',type=Path,required=True)
    p.add_argument('--cache',type=Path,required=True)
    p.add_argument('--output',type=Path,required=True)
    a=p.parse_args()
    plan_bytes=read_input(a.plan,4*1024**2)
    plan=json.loads(plan_bytes)
    if len(plan)>256:raise ValueError('archive count bound')
    a.output=private_output(a.output)
    (a.output/'texts').mkdir()
    lock=threading.Lock()
    def collect(row):
        result=dict(row)
        try:
            if not re.fullmatch('[0-9a-f]{64}',row['sha256']):raise ValueError('archive SHA256 required')
            path,receipt=acquire(row['url'],a.cache,sha256=row['sha256'])
            notices=[]
            for name,data,mode in members(path,receipt['sha256']):
                material = notice_material(name, data)
                if material is None:continue
                data, provenance = material
                if len(data)>16*1024**2:raise ValueError('notice byte bound')
                digest=hashlib.sha256(data).hexdigest();destination=a.output/'texts'/(digest+'.txt')
                with lock:
                    if destination.exists():
                        if destination.read_bytes()!=data:raise ValueError('notice content collision')
                    else:destination.write_bytes(data)
                notices.append({'file':'texts/'+destination.name,'sha256':digest,'member':name, **provenance})
            result.update(acquisition=receipt,notices=notices)
        except Exception as error:result.update(error=str(error),notices=[])
        return result
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        rows=list(pool.map(collect,plan))
    (a.output/'inventory.json').write_text(json.dumps({'schema':'marsh.primary-native-notices/v1',
        'plan_sha256':hashlib.sha256(plan_bytes).hexdigest(),'archives':rows,
        'scope':'Conservative legal-document superset of exact public source archives from recorded build pins; not execution, enabled-feature proof, signature validation or a reproducible build.',
        'failures':[r['url'] for r in rows if r.get('error') or not r['notices']]},indent=2)+'\n')
    if read_input(a.plan,4*1024**2)!=plan_bytes:raise ValueError('acquisition plan changed')
    for row in rows:print(json.dumps({'url':row['url'],'notices':len(row['notices']),'error':row.get('error')}),flush=True)
    return int(any(r.get('error') or not r['notices'] for r in rows))


if __name__=='__main__':raise SystemExit(main())
