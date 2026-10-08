#!/usr/bin/env python3
"""Actual rebuilder CLI controls using retained public seed/materials, no fetching.

Counts follow exact rows; shared fixture/document hashes must be refused, never
silently removed. Mutations are in fresh private fixture copies only.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys

HERE = Path(__file__).resolve().parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--work', type=Path, required=True)
    parser.add_argument('--seed', type=Path, required=True)
    parser.add_argument('--materials', type=Path, required=True)
    parser.add_argument('--source', type=Path, default=HERE, help='Archived collector directory for RED')
    args = parser.parse_args()
    work = args.work.absolute()
    work.mkdir(mode=0o700)
    seed = work / 'seed'
    materials = work / 'materials'
    shutil.copytree(args.seed, seed)
    shutil.copytree(args.materials, materials)
    seed.chmod(0o700)
    materials.chmod(0o700)
    results = []

    def run(name, expected, check):
        output = work / name
        argv = [str(x) for x in [sys.executable, '-B', args.source / 'rebuild-notice-indexes.py',
            '--seed', seed, '--materials', materials, '--output', output]]
        proc = subprocess.run(argv, capture_output=True, timeout=120)
        (work / (name + '.stdout')).write_bytes(proc.stdout)
        (work / (name + '.stderr')).write_bytes(proc.stderr)
        detail = None
        try:
            check(output, proc)
        except Exception as error:
            detail = repr(error)
        results.append({'control': name, 'argv': argv, 'exit': proc.returncode,
                        'detail': detail, 'as_expected': (proc.returncode == 0) == expected and detail is None})

    def counts(output, proc):
        rows = json.loads((output / 'source-records/primary-exact-crate-declarations-and-headers.json').read_bytes())['packages']
        selected = [r for r in rows if any(n.get('role', '').startswith('standard Apache-2.0 conditions') for n in r['notices'])]
        plain = sum(r['license_expression'] == 'Apache-2.0' for r in selected)
        status = json.loads((output / 'primary-source-notices.json').read_bytes())['notice_material_status']
        assert status['apache_declarations_and_attributions_preserved'] == len(selected), status
        assert status['apache_plain_declarations'] == plain, status
        assert status['apache_alternative_elections'] == len(selected) - plain, status
    run('positive', True, counts)
    # Alter the observed input population, not the asserted aggregate.
    path = seed / 'source-records/primary-exact-crate-declarations-and-headers.json'
    other = materials / 'exact-completion/inventory.json'
    originals = {p: p.read_bytes() for p in [path, other]}
    for p in [path, other]:
        document = json.loads(originals[p])
        drop = next(r['crate_url'] for r in document['packages'] if r['license_expression'] == 'Apache-2.0')
        document['packages'] = [r for r in document['packages'] if r['crate_url'] != drop]
        p.write_text(json.dumps(document, indent=2) + '\n')
    run('changed-declaration-population', True, counts)
    for p, data in originals.items():
        p.write_bytes(data)
    # A source fixture and a real legal document can share identical bytes. The
    # rebuilder must not erase the document while fixing the fixture reference.
    for kind, digest in [('excerpt', '04213b69dbebc8ba0dd8a6216af1d9422f9b63619c1cf2f1d8aa107b83bbb608'),
                         ('deletion', 'f5f846842cc206d49785c2a70b3cdced806490e14c313b5fa5b1e98db37078f8')]:
        name = 'texts/' + digest + '.txt'
        data = (seed / name).read_bytes()
        collision = seed / 'source-records/legitimate-shared-hash.json'
        collision.write_text(json.dumps([{'file': name, 'sha256': hashlib.sha256(data).hexdigest(),
            'source_path': 'legitimate-package/LICENSE'}]) + '\n')
        def refused(output, proc):
            assert b'source-derived replacement shares a legitimate notice hash:' in proc.stderr, proc.stderr
            assert (seed / name).read_bytes() == data
        run('shared-legitimate-hash-' + kind + '-refused', False, refused)
    record = {'scope': __doc__, 'controls': results}
    (work / 'results.json').write_text(json.dumps(record, indent=2) + '\n')
    print(json.dumps(record, indent=2))
    return int(not all(r['as_expected'] for r in results))


if __name__ == '__main__':
    raise SystemExit(main())
