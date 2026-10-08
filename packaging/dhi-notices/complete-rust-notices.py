#!/usr/bin/env python3
"""Preserve exact archived licence declarations and original copyright headers.

Apache-2.0 is elected only where the publisher's exact source/manifest explicitly
permits it. The standard conditions are NOT used as an invented grant or copyright.
MIT-only packages without original licence text remain unresolved.
"""
import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import tomllib
from public_sources import members, read_input, private_output
from notice_rules import legal_header, is_source_code, excluded_material

APACHE='cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30'


def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--gaps',type=Path,required=True)
    p.add_argument('--cache',type=Path,required=True)
    p.add_argument('--bundle',type=Path,required=True)
    p.add_argument('--output',type=Path,required=True)
    a=p.parse_args()
    input_bytes=read_input(a.gaps,4*1024**2)
    gaps=json.loads(input_bytes)
    if len(gaps)>256:raise ValueError('gap count bound')
    standard=read_input(a.bundle/'texts'/(APACHE+'.txt'))
    if hashlib.sha256(standard).hexdigest()!=APACHE:raise ValueError('Apache conditions changed')
    a.output=private_output(a.output);(a.output/'texts').mkdir()
    results=[]
    for gap in gaps:
        path=a.cache/hashlib.sha256(gap['url'].encode()).hexdigest()
        expected=gap['packages'][0]['checksum'];refs=[];headers=[];manifest=None;original=None
        def save(data,source_path,whole_sha,byte_range=None):
            digest=hashlib.sha256(data).hexdigest();target=a.output/'texts'/(digest+'.txt')
            if not target.exists():target.write_bytes(data)
            row={'file':'texts/'+target.name,'sha256':digest,'source_path':source_path,'source_file_sha256':whole_sha}
            if byte_range is not None:
                row.update(byte_range=byte_range, material_type='source-notice-excerpt')
            return row
        for name,data,mode in members(path,expected):
            parts=PurePosixPath(name).parts
            if len(parts)==2 and parts[-1]=='Cargo.toml':
                manifest=tomllib.loads(data.decode())['package']
                refs.append(save(data,name,hashlib.sha256(data).hexdigest()))
                if original is None:original=(name,data)
            if len(parts)==2 and parts[-1]=='Cargo.toml.orig':original=(name,data)
            if not excluded_material(name) and is_source_code(name):
                header = legal_header(data)
                # This completion layer collects licence declarations and
                # copyright attribution. A generic API comment about Unix
                # permissions is not either; keep the former content threshold
                # while sharing all path classification and excerpt boundaries.
                if header and (b'copyright' in header.lower() or b'license' in header.lower() or b'licence' in header.lower()):
                    headers.append(save(header,name,hashlib.sha256(data).hexdigest(),[0,len(header)]))
            if (not excluded_material(name) and not is_source_code(name)
                    and len(parts) == 2 and parts[-1].lower().startswith('readme')
                    and len(data)<1024**2 and b'license' in data.lower()):
                refs.append(save(data,name,hashlib.sha256(data).hexdigest()))
        if not manifest or not original:raise ValueError('package lacks original manifest')
        name,data=original
        refs.append(save(data,name,hashlib.sha256(data).hexdigest()));refs.extend(headers)
        expression=manifest.get('license','')
        apache=bool(re.fullmatch(r'(?:MIT\s*(?:/|OR)\s*)?Apache-2\.0(?:\s*(?:/|OR)\s*MIT)?',expression) or expression=='MIT OR Apache-2.0 OR Zlib')
        # Preserve all author/rights-holder bytes exactly. Never turn authors into
        # invented copyright statements or fill a generic MIT template.
        row={'crate_url':gap['url'],'crate_sha256':expected,'name':manifest['name'],'version':manifest['version'],
             'license_expression':expression,'notices':refs,'source_header_count':len(headers),
             'resolution':('Apache-2.0 declared by the package (not an alternative election); original source headers and full standard conditions retained' if expression == 'Apache-2.0' else 'Apache-2.0 alternative elected from exact publisher declaration, with original source attributions and full standard conditions') if apache else 'Original publisher declaration retained; informational standard permission text is not an original upstream grant or rights disposition',
             'notice_material_resolved':apache,'copyright_added':False}
        if apache:
            row['notices'].append({'file':'texts/'+APACHE+'.txt','sha256':APACHE,'role':'standard Apache-2.0 conditions; grant/declaration and attribution are the exact crate source above, not this generic text'})
        elif expression == 'MIT':
            row['notices'].append({'file':'provider-notices/MIT-template.txt',
                'sha256':'b05785f9f18e6716bab63424b11454513b9943a222595b70411009202fc592b5',
                'role':'informational standard conditions only; same npm/Rust policy, not an independently acquired upstream grant or copyright'})
        results.append(row)
    (a.output/'inventory.json').write_text(json.dumps({'schema':'marsh.exact-source-notice-completions/v1','packages':results,
        'unresolved':[r['crate_url'] for r in results if not r['notice_material_resolved']],
        'scope':'Source-grounded publisher declarations (plain licences or expressly offered alternatives) and retained original headers, not inferred author attribution, enabled-feature proof, or a legal opinion.'},indent=2)+'\n')
    if read_input(a.gaps,4*1024**2)!=input_bytes:raise ValueError('gap source changed')
    print(json.dumps({r['name']:r['notice_material_resolved'] for r in results},indent=2))


if __name__=='__main__':main()
