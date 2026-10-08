#!/usr/bin/env python3
"""Bounded helper CLI adversaries; synthetic inputs/peers, not image qualification."""
import argparse
import hashlib
import io
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile

ROOT = Path(__file__).resolve().parents[2]


def sha(data):
    return hashlib.sha256(data).hexdigest()


def dump(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--work', type=Path, required=True)
    p.add_argument('--old-source', type=Path, help='Archived helper sources for RED evidence')
    a = p.parse_args()
    w = a.work.absolute()
    w.mkdir(mode=0o700, exist_ok=False)
    rows = []

    def helper(name):
        rel = 'packaging/dhi-notices/' + name
        return (a.old_source / rel).absolute() if a.old_source and (a.old_source / rel).exists() else ROOT / rel

    def run(name, argv, expected_success, reason=None, inspect=None):
        r = subprocess.run(list(map(str, argv)), capture_output=True, timeout=120)
        (w / (name + '.stdout')).write_bytes(r.stdout)
        (w / (name + '.stderr')).write_bytes(r.stderr)
        okay = (r.returncode == 0) == expected_success
        detail = ''
        if reason and reason not in r.stderr.decode(errors='replace'):
            okay = False
        if inspect:
            try:
                inspect()
            except Exception as error:
                detail = str(error)
                okay = False
        rows.append({'control': name, 'argv': list(map(str, argv)), 'exit': r.returncode,
                     'expected_success': expected_success, 'as_expected': okay, 'detail': detail})

    # The actual nine-recipe guard CLI sees the same source plus one mutation.
    source = w / 'recipes'
    for recipe in [ROOT / 'packaging/shell/Dockerfile',
                   *(ROOT / 'kits').glob('*/*.dockerfile')]:
        target = source / recipe.relative_to(ROOT)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(recipe, target)
    target = source / 'kits/marsh-codex/codex.dockerfile'
    original = target.read_text()
    for name, mutation, expected in [
            ('receipt-order-positive', '', True),
            ('lowercase-copy-refused', '\ncopy unexpected /extra\n', False),
            ('mixedcase-add-refused', '\naDd unexpected /extra\n', False),
            ('lowercase-run-refused', '\nrun true\n', False),
            ('onbuild-refused', '\nonbuild COPY unexpected /extra\n', False),
            ('lowercase-path-refused', '\nenv PATH=/wrong\n', False),
            ('legacy-path-refused', '\nENV PATH /evil:/usr/local/bin:/usr/bin\n', False),
            ('quoted-legacy-path-refused', '\nenv "PATH" /evil\n', False),
            ('tab-path-refused', '\nENV\tPATH\t/evil\n', False),
            ('tab-run-refused', '\nrUn\ttrue\n', False),
            ('escape-directive-run-refused', '\nLABEL hidden=yes \\\nRUN true\n', False),
            ('skip-directive-refused', '', False)]:
        prefix = '# escape=`\n' if name == 'escape-directive-run-refused' else '# check=skip=all\n' if name == 'skip-directive-refused' else ''
        target.write_text(prefix + original + mutation)
        output = w / (name + '.json')
        def check_order():
            report = json.loads(output.read_text())
            codex = next(row for row in report if row['path'] == 'kits/marsh-codex/codex.dockerfile')
            if expected:
                assert all(row['passed'] for row in report), report
            else:
                assert not codex['passed'] and any(reason in codex['error'] for reason in
                    ['follows a final', 'PATH changes', 'unsupported Dockerfile parser directive']), codex
        run(name, [sys.executable, '-B', helper('check-receipt-order.py'), '--root', source, '--output', output], expected, inspect=check_order)
    target.write_text(original)

    # Real Node collector. Only its fixed installation path is relocated for this
    # portable filesystem fixture; matcher, traversal and output code are intact.
    context = w / 'npm'
    (context / 'notices').mkdir(parents=True)
    dump(context / 'notices/overrides.json', {})
    dump(context / 'package-lock.json', {'fixture': True})
    rules = (ROOT / 'packaging/dhi-notices/collected/notice-rules.json').resolve()
    origin = (a.old_source / 'scripts/npm-notices.mjs') if a.old_source else ROOT / 'scripts/npm-notices.mjs'
    source_bytes = origin.read_text()
    relocated = source_bytes.replace("'/usr/local/share/licenses/marsh-dhi/notice-rules.json'", repr(str(rules)))
    script = context / 'collector.mjs'
    script.write_text(relocated)
    package = context / 'node_modules/fixture'
    package.mkdir(parents=True)
    dump(package / 'package.json', {'name': 'fixture', 'version': '1.0.0'})
    legal = b'Original fixture notice; legitimate dot-prefixed document\n'
    (package / '.LICENSE').write_bytes(legal)
    for name in ['copyright.pm', 'license.star', 'testdata/LICENSE.txt',
                 'testcases/LICENSE', 'license_missing_name.prototext', 'license-translations.dict',
                 'licenses-tables.dat', 'tests/files/license-uris']:
        path = package / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b'nonlegal fixture bytes\x00\n')
    output = context / 'output'
    def check_npm():
        d = json.loads((output / 'npm-package-notices.json').read_text())
        actual = d['packages'][0]['notices']
        assert [(r['source'], r['sha256']) for r in actual] == [('.LICENSE', sha(legal))], actual
    run('npm-dot-license-and-nonlegal-exclusions', [shutil.which('node'), script, context / 'node_modules', output], True, inspect=check_npm)
    # Node deliberately has no excerpt parser. A source-only grant cannot turn
    # into a whole-code notice or silently close a gap. Originals stay installed;
    # such a package needs an explicit acquired notice override.
    (package / '.LICENSE').unlink()
    (package / 'license.js').write_bytes(b'/* Copyright Fixture; Permission is hereby granted */\nrun_code();\n')
    run('npm-header-only-gap-refused', [shutil.which('node'), script, context / 'node_modules', context / 'header-only-output'],
        False, reason='Missing notice text: fixture@1.0.0')
    dump(w / 'npm-relocation.json', {'original_sha256': sha(source_bytes.encode()), 'fixture_sha256': sha(relocated.encode()),
                                   'only_change': 'Fixed vocabulary installation path relocated; no matcher edits'})

    # Direct buildx selection: a separately named Docker peer refuses execution.
    platform = b'{"schemaVersion":2,"config":{"digest":"sha256:' + b'2' * 64 + b'"},"layers":[]}'
    child = 'sha256:' + sha(platform)
    index = json.dumps({'schemaVersion': 2, 'manifests': [{'digest': child, 'size': len(platform),
                     'platform': {'os': 'linux', 'architecture': 'arm64'}}]}, separators=(',', ':')).encode()
    ref = 'dhi.io/sbx-templates@sha256:' + sha(index)
    docker, buildx = w / 'docker', w / 'selected-buildx'
    docker.write_text('#!' + sys.executable + '\nraise SystemExit(91)\n')
    buildx.write_text('#!' + sys.executable + '\nimport sys\n' +
                     'assert sys.argv[1:4] == ["imagetools","inspect","--raw"]\n' +
                     'sys.stdout.buffer.write(' + repr(index) + ' if sys.argv[-1] == ' + repr(ref) + ' else ' + repr(platform) + ')\n')
    docker.chmod(0o755)
    buildx.chmod(0o755)
    capture = w / 'capture'
    def check_capture():
        doc = json.loads((capture / 'index-proof.json').read_text())
        assert doc['schema'] == 'marsh.primary-index-capture/v2', doc
        assert doc['error'] is None and len(doc['commands']) == 2, doc
        assert all(r['argv'][0] == str(buildx) for r in doc['commands']), doc
        assert doc['buildx_sha256'] == sha(buildx.read_bytes()), doc
    run('explicit-buildx-peer', [sys.executable, '-B', helper('capture-index.py'), '--docker', docker,
                               '--buildx', buildx, '--docker-config', w, '--docker-home', w,
                               '--reference', ref, '--platform', 'linux/arm64', '--evidence', capture], True, inspect=check_capture)
    dump(w / 'results.json', {'scope': 'Actual helper CLI; owned synthetic recipe/package/OCI inputs and executable peers. No Docker, network, images, agents or credentials.', 'controls': rows})
    print(json.dumps(rows, indent=2))
    return int(not all(r['as_expected'] for r in rows))


if __name__ == '__main__':
    raise SystemExit(main())
