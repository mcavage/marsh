#!/usr/bin/env python3
"""Offline real-CLI fixture controls for Rust archive fallback and completion.

Synthetic public-shaped archives, never package execution or a legal grant.
"""
import argparse
import hashlib
import io
import json
from pathlib import Path
import subprocess
import sys
import tarfile

HERE = Path(__file__).resolve().parent


def sha(data):
    return hashlib.sha256(data).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--work', type=Path, required=True)
    parser.add_argument('--source', type=Path, default=HERE, help='Collector directory, including archived RED sources')
    args = parser.parse_args()
    work = args.work.absolute()
    work.mkdir(mode=0o700)
    cache = work / 'cache'
    cache.mkdir(mode=0o700)
    results = []
    manifest = b'[package]\nname = "fixture"\nversion = "1.0.0"\nlicense = "Apache-2.0"\n'
    grant = b'Copyright Fixture Author\nPermission is hereby granted (synthetic scanner control, not a licence grant).\n'
    header = b'/* Copyright Fixture Author; license: Apache-2.0.\n' + grant + b'*/'
    url = 'https://static.crates.io/crates/fixture/fixture-1.0.0.crate'
    key = sha(url.encode())

    def archive(files):
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode='w:gz') as stream:
            for name, data in {'Cargo.toml': manifest, **files}.items():
                member = tarfile.TarInfo('fixture-1.0.0/' + name)
                member.size, member.mode = len(data), 0o644
                stream.addfile(member, io.BytesIO(data))
        data = buffer.getvalue()
        (cache / key).write_bytes(data)
        return sha(data)

    def run(name, argv, expected_exit, inspect):
        proc = subprocess.run([str(x) for x in argv], capture_output=True, timeout=120)
        (work / (name + '.stdout')).write_bytes(proc.stdout)
        (work / (name + '.stderr')).write_bytes(proc.stderr)
        detail = None
        try:
            inspect()
        except Exception as error:
            detail = repr(error)
        results.append({'control': name, 'argv': [str(x) for x in argv], 'exit': proc.returncode,
                        'expected_exit': expected_exit, 'detail': detail,
                        'as_expected': proc.returncode == expected_exit and detail is None})

    for i, (path, data, accepted) in enumerate([
        ('testdata/README.md', grant, False),
        ('tests/files/readme.txt', grant, False),
        ('nested/README.md', grant, False),
        ('README.dat', grant, False),
        ('README.rs', grant, False),
        ('README.md', grant, True),
        ('testcases/license.pm', header + b'\nprint 1;', False),
        ('src/lib.pm', header + b'\nprint 1;', True),
    ]):
        archive_sha = archive({path: data})
        lock = work / 'Cargo.lock'
        lock.write_text('version = 4\n[[package]]\nname = "fixture"\nversion = "1.0.0"\n'
                        'source = "registry+https://github.com/rust-lang/crates.io-index"\nchecksum = "' + archive_sha + '"\n')
        out = work / ('lock-' + str(i))
        def check_lock():
            document = json.loads((out / 'inventory.json').read_bytes())
            notices = document['archives'][0]['notices']
            assert not document['failures'], document
            assert bool(notices) == accepted, notices
            assert bool(document['missing_notices']) != accepted, document
            if accepted and path.endswith('.pm'):
                assert notices[0]['material_type'] == 'source-notice-excerpt', notices
                assert notices[0]['byte_range'] == [0, len(header)], notices
                assert (out / notices[0]['file']).read_bytes() == header
        run('lock-' + str(i), [sys.executable, '-B', args.source / 'collect-rust-lock.py', '--lock', lock,
            '--lock-sha256', sha(lock.read_bytes()), '--lock-url', 'https://raw.githubusercontent.com/example/fixture/' + '0'*40 + '/Cargo.lock',
            '--source-url', url, '--source-sha256', archive_sha, '--cache', cache, '--output', out],
            0 if accepted else 1, check_lock)

    files = {'testdata/README.md': grant + b'license', 'tests/files/readme.txt': grant + b'license',
             'nested/README.md': b'license', 'README.dat': b'license', 'README.rs': b'license',
             'README.md': b'Root license declaration: Apache-2.0\n',
             'src/permissions.rs': b'//! Unix permissions API example, not legal attribution.\nfn f() {}',
             'testdata/lib.rs': header + b'\nfn f() {}', 'tests/files/lib.cpp': header + b'\nint f() {}'}
    for ext in ['rs', 'c', 'h', 'cpp', 'pm', 'pl', 'star', 'JS']:
        files['src/lib.' + ext] = header + b'\nCODE_MUST_NOT_BE_IN_EXCERPT;\n'
    archive_sha = archive(files)
    gaps = work / 'gaps.json'
    gaps.write_text(json.dumps([{'url': url, 'packages': [{'name': 'fixture', 'version': '1.0.0', 'checksum': archive_sha}]}]))
    out = work / 'completion'
    def check_completion():
        document = json.loads((out / 'inventory.json').read_bytes())
        row = document['packages'][0]
        excerpts = [n for n in row['notices'] if 'byte_range' in n]
        assert len(excerpts) == 8, excerpts
        for n in excerpts:
            assert n['material_type'] == 'source-notice-excerpt', n
            assert n['source_path'].startswith('fixture-1.0.0/src/'), n
            assert n['byte_range'] == [0, len(header)], n
            assert (out / n['file']).read_bytes() == header
            assert n['source_file_sha256'] == sha(files[n['source_path'].split('/', 1)[1]])
        origins = [n.get('source_path', '') for n in row['notices']]
        assert all(not any(x in n for x in ['/testdata/', '/tests/files/', '/nested/', 'README.dat', 'README.rs', '/permissions.rs']) for n in origins), origins
        assert 'fixture-1.0.0/README.md' in origins, origins
    run('completion-exclusions-and-shared-excerpts', [sys.executable, '-B', args.source / 'complete-rust-notices.py',
        '--gaps', gaps, '--cache', cache, '--bundle', HERE / 'collected', '--output', out], 0, check_completion)
    record = {'scope': __doc__, 'controls': results}
    (work / 'results.json').write_text(json.dumps(record, indent=2) + '\n')
    print(json.dumps(record, indent=2))
    return int(not all(r['as_expected'] for r in results))


if __name__ == '__main__':
    raise SystemExit(main())
