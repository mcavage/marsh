#!/usr/bin/env python3
"""Reconcile observed fixed public-image legal bytes; no Docker access."""
import argparse
import base64
import hashlib
import json
import os
import stat
from pathlib import Path


def digest(data):return hashlib.sha256(data).hexdigest()


def read_bytes(path):
    for parent in path.absolute().parents:
        if not stat.S_ISDIR(parent.lstat().st_mode):raise ValueError('symlink/non-directory input ancestor')
    fd=os.open(path,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK)
    with os.fdopen(fd,'rb') as source:
        st=os.fstat(source.fileno())
        if not stat.S_ISREG(st.st_mode) or st.st_size>32*1024**2:raise ValueError('bounded regular input required')
        data=source.read(32*1024**2+1)
        if len(data)!=st.st_size:raise ValueError('input changed')
    return data


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--receipt',type=Path,required=True)
    p.add_argument('--bundle',type=Path,required=True)
    p.add_argument('--output',type=Path,required=True)
    a=p.parse_args()
    raw=read_bytes(a.receipt)
    if len(raw)>1024**2:raise ValueError('receipt bound')
    r=json.loads(raw)
    if r['schema']!='marsh.public-primary-image-bytes/v1' or r['outcome']!='passed':raise ValueError('not a successful root observation')
    labels=['inspect','collector','owned-container-absence','inspect-after']
    if len(r['commands'])!=len(labels):raise ValueError('unexpected acquisition commands')
    for label,command in zip(labels,r['commands']):
        if command['status']!=0:raise ValueError('failed acquisition')
        for stream in ['stdout','stderr']:
            if digest(read_bytes(a.receipt.parent/(label+'.'+stream)))!=command[stream+'_sha256']:
                raise ValueError('root observation bytes changed')
    before=json.loads(read_bytes(a.receipt.parent/'inspect.stdout'))[0]
    after=json.loads(read_bytes(a.receipt.parent/'inspect-after.stdout'))[0]
    if any(before[k]!=after[k] for k in ['Id','Os','Architecture']) or before['Id']!=r['image_id'] or before['Os']+'/'+before['Architecture']!=r['platform']:
        raise ValueError('root image identity/platform mismatch')
    argv=r['commands'][1]['argv']
    for option,value in [('--network','none'),('--pull','never'),('--entrypoint','/usr/bin/python3')]:
        if argv[argv.index(option)+1]!=value:raise ValueError('unexpected collector authority')
    if '--read-only' not in argv or r['reference'] not in argv:raise ValueError('unbound image collector')
    if read_bytes(a.receipt.parent/'owned-container-absence.stdout').strip():raise ValueError('retained collector container')
    if json.loads(read_bytes(a.receipt.parent/'collector.stdout'))!=r['files']:raise ValueError('receipt does not match collector output')
    expected={row['path']:row['sha256'] for row in json.loads(read_bytes(a.bundle/'payload-obligations.json'))['image_files']}
    files={}
    for row in r['files']:
        data=base64.b64decode(row['base64'],validate=True)
        if len(data)!=row['bytes'] or digest(data)!=row['sha256'] or expected.get(row['path'])!=digest(data):raise ValueError('primary file mismatch')
        files[row['path']]=data
    if set(files)!=set(expected):raise ValueError('wrong fixed file set')
    spdx=json.loads(files['/opt/docker/sbom/clipboard-bridge/.spdx.clipboard-bridge.json'])
    packages={row['name']:row for row in spdx['packages']}
    commit='54a695035350bab5f126a88c892a9bc3b5ae162a'
    main_package=packages['clipboard-bridge'];sys_package=packages['golang.org/x/sys']
    if main_package['versionInfo']!=commit or main_package['licenseDeclared']!='LicenseRef-Proprietary':raise ValueError('unexpected clipboard SPDX')
    go=json.loads(read_bytes(a.bundle/'go-module-notices.json'))
    module=next(m for m in go['modules'] if m['path']=='golang.org/x/sys' and m['version']=='v0.46.0')
    if '/usr/local/bin/clipboard-bridge' not in module['binaries']:raise ValueError('Go inventory not clipboard-bound')
    purl=next(row['referenceLocator'] for row in sys_package['externalRefs'] if row['referenceType']=='purl')
    if purl!='pkg:golang/golang.org/x/sys@v0.46.0' or sys_package['licenseDeclared']!='BSD-3-Clause':raise ValueError('x/sys SPDX purl/license mismatch')
    discrepancies={key:sys_package[key] for key in ['versionInfo','downloadLocation','checksums']}
    if discrepancies['versionInfo']!=commit:raise ValueError('vendor discrepancy changed; re-review required')
    parent=a.output.absolute().parent
    for ancestor in (parent,*parent.parents):
        if not stat.S_ISDIR(ancestor.lstat().st_mode):raise ValueError('symlink/non-directory output ancestor')
    st=parent.lstat()
    if st.st_uid!=os.getuid() or st.st_mode & 0o077:raise ValueError('output parent must be owner-private')
    a.output.mkdir(mode=0o700,exist_ok=False);(a.output/'source-records').mkdir();(a.output/'texts').mkdir()
    (a.output/'source-records/public-image-primary-receipt.json').write_bytes(raw)
    (a.output/'source-records/clipboard-primary.spdx.json').write_bytes(files['/opt/docker/sbom/clipboard-bridge/.spdx.clipboard-bridge.json'])
    license_bytes=files['/opt/docker/.license.txt'];h=digest(license_bytes);(a.output/'texts'/(h+'.txt')).write_bytes(license_bytes)
    refs=[{'file':'texts/'+h+'.txt','sha256':h}]
    reconciliation={'schema':'marsh.clipboard-spdx-reconciliation/v1','receipt_sha256':digest(raw),'root_observed_image_id':r['image_id'],'platform':r['platform'],
        'spdx_sha256':digest(files['/opt/docker/sbom/clipboard-bridge/.spdx.clipboard-bridge.json']),
        'clipboard_commit':commit,'spdx_declared_license':'LicenseRef-Proprietary','spdx_concluded_license':'NOASSERTION',
        'x_sys':{'observed_build_information_version':'v0.46.0','spdx_purl':purl,'license_declared':'BSD-3-Clause','go_archive_sha256':module['archive_sha256'],'go_sum':module['sum'],'required_notices':module['notices'],
                 'vendor_spdx_inconsistencies':discrepancies,'interpretation':'versionInfo/downloadLocation/checksums repeat the main repository/commit, not the module version/source/hash. Preserve vendor document unchanged; use its correct purl corroborated by embedded Go build information and checksum-verified module archive. These fields are NOT proof of x/sys corresponding source.'},
        'image_license_scope':'DHI definitions (Dockerfiles), patches and build scripts are Apache-2.0. Included packages/binaries retain their own upstream terms. SPDX CC0-1.0 covers the document, not proprietary clipboard code.',
        'private_acquisition':'The clipboard-bridge LICENSE text is retained as published by Docker. The THIRD-PARTY-NOTICES file it refers to is not published; the golang.org/x/sys v0.46.0 notice is collected separately from the observed dependency.',
        'permission_scope':"No license grant is asserted for the proprietary clipboard-bridge component; it is distributed under Docker's terms."}
    (a.output/'source-records/clipboard-spdx-reconciliation.json').write_text(json.dumps(reconciliation,indent=2)+'\n')
    (a.output/'image-primary-notices.json').write_text(json.dumps([{'package':'dhi-image-build-definitions','notices':refs,'required_notices':refs,'acquisition_receipt':'source-records/public-image-primary-receipt.json','receipt_sha256':digest(raw),'scope':reconciliation['image_license_scope']}],indent=2)+'\n')
    print(json.dumps(reconciliation,indent=2))


if __name__=='__main__':main()
