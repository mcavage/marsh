#!/usr/bin/env python3
"""Read-only producer-proof / real Kit preparation caller for a CURRENT host build.

No build, Docker, SBX, Cloud, registry or credential command is invoked. Requires
an actual current observed build receipt; never adopts historical snapshots or
creates a synthetic successful receipt. Outputs are new private test evidence.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / 'scripts'))
from build_inputs import file_record, read_file
from owned_process import run as run_owned


def sha(path):
    return file_record(path)['sha256']


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source-tree', type=Path, required=True)
    parser.add_argument('--shell-image', type=Path, required=True)
    parser.add_argument('--shell-build-receipt', type=Path, required=True)
    parser.add_argument('--evidence', type=Path, required=True, help='new private directory outside source/export')
    args = parser.parse_args()
    for path in (args.source_tree, args.shell_image, args.shell_build_receipt):
        if not path.is_absolute() or path.resolve(strict=True) != path:
            parser.error('use canonical absolute current producer paths')
    receipt = json.loads(read_file(args.shell_build_receipt, 16 * 1024 * 1024))
    guest = Path(receipt['guest_artifacts'])
    host = Path(receipt['marsh']).parent
    evidence = Path(os.path.abspath(args.evidence))
    if (evidence.exists() or evidence.is_symlink() or evidence.parent.resolve(strict=True) != evidence.parent
            or any(evidence.is_relative_to(path) for path in (args.source_tree, guest, host))):
        parser.error('evidence must be new, canonical and outside producer source/exports')
    os.umask(0o077)
    evidence.mkdir(mode=0o700)
    output = evidence / 'prepared-inputs.json'
    commands = args.source_tree / 'tests/acceptance/fixture/commands.json'
    base = [sys.executable, str(ROOT / 'scripts/prepare-kit-inputs.py'),
            '--source-tree', str(args.source_tree), '--commands', str(commands),
            '--output', str(output), '--shell-image', str(args.shell_image),
            '--shell-build-receipt', str(args.shell_build_receipt)]
    watched = [args.shell_image, args.shell_image.with_name('shell-image.build.json'),
               args.shell_build_receipt, ROOT / 'scripts/prepare-kit-inputs.py',
               ROOT / 'scripts/publish-kits.py', ROOT / 'scripts/image_observations.py',
               ROOT / 'scripts/owned_process.py', ROOT / 'scripts/package_observations.py',
               ROOT / 'scripts/build_inputs.py', ROOT / 'tests/acceptance/provenance.py']
    before = {str(path): sha(path) for path in watched}
    records = []
    def call(label, extra=()):
        result = run_owned([*base, *map(str, extra)],
                                env={**os.environ, 'PYTHONDONTWRITEBYTECODE': '1'},
                                capture_output=True, text=True, timeout=180)
        record = {'case': label, 'argv': result.args, 'status': result.returncode,
                  'stdout': result.stdout, 'stderr': result.stderr}
        records.append(record)
        (evidence / (label + '.json')).write_text(json.dumps(record, indent=2) + '\n')
        return result
    summary = {'schema': 'marsh.strong-shell-consumer-uat/v1', 'outcome': 'failed',
               'scope': 'current host-observed receipt and real prepare CLI only; no image/runtime qualification',
               'source_before': before, 'cases': records}
    try:
        result = call('current-observed-positive')
        if result.returncode:
            raise ValueError('current producer proof failed; no historical/stub fallback: ' + result.stderr)
        prepared_hash = sha(output)
        document = json.loads(output.read_text())
        request, = document['receipt']['shell_images']
        if (request['image_file'] != str(args.shell_image)
                or request['build_receipt'] != str(args.shell_build_receipt)):
            raise ValueError('prepared document did not bind exact producer proof paths')
        copied = evidence / 'copied'
        copied.mkdir()
        shutil.copy2(args.shell_image, copied / 'shell-image')
        shutil.copy2(args.shell_image.with_name('shell-image.build.json'), copied / 'shell-image.build.json')
        alias = evidence / 'alias-image'
        alias.symlink_to(args.shell_image)
        for label, extra in (
                ('copied-path-refusal', ['--shell-image', copied / 'shell-image']),
                ('symlink-alias-refusal', ['--shell-image', alias]),
                ('helper-only-refusal', ['--shell-build-receipt', copied / 'shell-image.build.json'])):
            result = call(label, extra)
            if result.returncode == 0 or sha(output) != prepared_hash:
                raise ValueError('negative caller accepted or changed existing prepared output: ' + label)
        result = call('current-observed-revalidation')
        if result.returncode or sha(output) != prepared_hash:
            raise ValueError('current proof revalidation differs after negative controls')
        summary.update(outcome='passed', prepared_sha256=prepared_hash,
                       prepared_bytes=output.stat().st_size)
    finally:
        summary['source_after'] = {str(path): sha(path) for path in watched}
        if summary['source_after'] != before:
            summary['outcome'] = 'failed-source-fence'
        (evidence / 'result.json').write_text(json.dumps(summary, indent=2) + '\n')
    print(evidence / 'result.json')
    return 0 if summary['outcome'] == 'passed' else 1


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (ValueError, OSError, KeyError, subprocess.SubprocessError) as error:
        raise SystemExit(f'strong-provenance-uat: {error}') from error
