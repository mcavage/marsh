#!/usr/bin/env python3
"""Portable R6 boundary: real adapter verifier/require, actual public npm members.

Policy paths and ownership are relocated to an owned fixture. Installed npm
metadata is synthetic. No namespace/image/agent claim or package execution.
"""
import argparse
import ast
import base64
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sys

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
sys.path.insert(0, str(HERE))
from public_sources import members, digest


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--work', type=Path, required=True)
    p.add_argument('--cache', type=Path, required=True)
    p.add_argument('--policy', type=Path, help='Archived policy for defect RED')
    p.add_argument('--profiles-policy', type=Path, help='Archived native profile policy for defect RED')
    a = p.parse_args()
    w = a.work.absolute()
    w.mkdir(mode=0o700, exist_ok=False)
    bundle = w / 'bundle'
    shutil.copytree(HERE / 'collected', bundle)
    spec = importlib.util.spec_from_file_location('actual_verifier', bundle / 'verify.py')
    v = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(v)
    # Compile the actual nested require body verbatim, supplying its actual
    # bundle_tree observation. No replacement/stub of the mandatory check.
    parsed = ast.parse((bundle / 'verify.py').read_text())
    require_node = next(n for n in ast.walk(parsed) if isinstance(n, ast.FunctionDef) and n.name == 'require')
    code = compile(ast.Module(body=[require_node], type_ignores=[]), str(bundle / 'verify.py'), 'exec')
    policies = json.loads((a.policy or HERE / 'collected/adapter-payloads.json').read_text())['adapters']
    policy = copy.deepcopy(policies['codex-acp'])
    original_root = policy['root']
    installed = w / 'installed'
    installed.mkdir()
    policy['root'] = str(installed)
    selected = policy['architectures']['arm64']
    def relocate(value):
        return str(installed) + value[len(original_root):]
    shutil.copy2(ROOT / 'kits/marsh-codex/package-lock.json', installed / 'package-lock.json')
    retained = w / 'retained'
    (retained / 'texts').mkdir(parents=True)
    policy['notice_root'] = str(retained)
    rows = []
    for package in selected['packages']:
        acquisition = package['acquisition']
        archive = a.cache / hashlib.sha256(acquisition['url'].encode()).hexdigest()
        assert digest(archive) == acquisition['sha256']
        assert base64.b64encode(bytes.fromhex(digest(archive, 'sha512'))).decode() == acquisition['integrity'].split('-', 1)[1]
        destination = Path(relocate(package['root']))
        destination.mkdir(parents=True)
        for name, data, mode in members(archive, acquisition['sha256']):
            path = destination / Path(name).relative_to('package')
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
            path.chmod(mode & 0o777)
        for entry in package['payload_entries']:
            entry.update(path=relocate(entry['path']), uid=os.getuid(), gid=os.getgid())
            if entry['type'] == 'directory':
                Path(entry['path']).chmod(entry['mode'])
        package['root'] = str(destination)
        notices = package['notices'] or [selected['required_notices'][0]]
        for notice in notices:
            shutil.copy2(bundle / notice['file'], retained / 'texts' / (notice['sha256'] + '.txt'))
        rows.append({'name': package['package'], 'version': package['version'],
                     'path': str(destination.relative_to(installed / 'node_modules')),
                     'package_json_sha256': v.sha(destination / 'package.json'),
                     'notices': [{'sha256': n['sha256']} for n in notices]})
    for link in selected['links']:
        link.update(path=relocate(link['path']), uid=os.getuid(), gid=os.getgid())
        path = Path(link['path'])
        path.parent.mkdir(parents=True, exist_ok=True)
        path.symlink_to(link['target'])
        if path.lstat().st_mode & 0o777 != link['mode']:
            path.lchmod(link['mode'])
    (retained / 'npm-package-notices.json').write_text(json.dumps({
        'schema': 'marsh.installed-npm-notices.v1', 'platform': 'linux', 'architecture': 'arm64',
        'package_lock_sha256': policy['lock_sha256'], 'packages': rows}) + '\n')
    results = []
    def call(name, expected, required_file='provider-notices/codex-0.156.1-third_party-wezterm-LICENSE'):
        before, _ = v.bundle_tree(bundle)
        environment = dict(vars(v), before=before, indexed={})
        exec(code, environment)
        try:
            observed = v.verify_adapter(policy, 'arm64', environment['require'])
            success, error = True, None
        except Exception as exception:
            success, error, observed = False, str(exception), None
        right_reason = expected or error is not None and ('required notice missing/changed: ' + required_file) in error
        results.append({'control': name, 'passed': success, 'expected_success': expected,
                        'as_expected': success == expected and right_reason, 'error': error, 'observed': observed})
    call('actual-public-package-positive', True)
    notice = bundle / 'provider-notices/codex-0.156.1-third_party-wezterm-LICENSE'
    original = notice.read_bytes()
    notice.unlink()
    call('missing-exact-wezterm-refused', False)
    notice.write_bytes(original + b'changed')
    call('changed-exact-wezterm-refused', False)
    notice.write_bytes(original)
    call('restored-exact-wezterm-positive', True)
    providers = json.loads((bundle / 'agent-notices.json').read_text())
    profiles = json.loads((a.profiles_policy or HERE / 'collected/derived-profiles.json').read_text())
    # Execute the native profile admission block verbatim, before any payload
    # effects. The adapter positive above uses actual SHA512-verified npm bytes.
    profile_node = next(n for n in ast.walk(parsed) if isinstance(n, ast.If)
                        and ast.unparse(n.test) == "args.profile != 'base'")
    profile_code = compile(ast.Module(body=[profile_node], type_ignores=[]), str(bundle / 'verify.py'), 'exec')
    from types import SimpleNamespace
    for version in ['0.155.1', '0.156.1']:
        for sample in ['imagegen-LICENSE.txt', 'openai-docs-LICENSE.txt', 'skill-creator-license.txt', 'skill-installer-LICENSE.txt']:
            filename = 'provider-notices/codex-' + version + '-codex-rs-skills-src-assets-samples-' + sample
            notice = bundle / filename
            original = notice.read_bytes()
            # Coherent provider index + file deletion evades the global provider
            # loop; only the separate selected-version requirements catch it.
            removed = dict(providers, notices=[r for r in providers['notices'] if 'provider-notices/' + r['file'] != filename])
            (bundle / 'agent-notices.json').write_text(json.dumps(removed))
            notice.unlink()
            before, _ = v.bundle_tree(bundle)
            for arch in ['arm64', 'amd64']:
                environment = dict(vars(v), before=before, indexed={}, profiles=profiles, architecture=arch,
                    args=SimpleNamespace(profile='codex-kit', version='0.155.1'))
                exec(code, environment)
                try:
                    for row in removed['notices']:
                        environment['require']([dict(row, file='provider-notices/' + row['file'])])
                    if version == '0.155.1':
                        exec(profile_code, environment)
                    else:
                        environment['require'](policies['codex-acp']['architectures'][arch]['required_notices'])
                    error = None
                except Exception as exception:
                    error = str(exception)
                results.append({'control': 'coherent-skill-removal-' + version + '-' + arch + '-' + sample,
                    'error': error, 'as_expected': error == 'required notice missing/changed: ' + filename,
                    'scope': 'Actual require/native admission body; no image or AMD64 runtime qualification'})
            if version == '0.156.1':
                call('actual-adapter-coherent-skill-removal-' + sample, False, filename)
            notice.write_bytes(original)
            (bundle / 'agent-notices.json').write_text(json.dumps(providers))
    call('all-skills-restored-positive', True)
    record = {'scope': __doc__, 'verifier_sha256': v.sha(bundle / 'verify.py'),
              'policy_sha256': v.sha(a.policy or HERE / 'collected/adapter-payloads.json'),
              'controls': results}
    (w / 'results.json').write_text(json.dumps(record, indent=2) + '\n')
    print(json.dumps(record, indent=2))
    return int(not all(r['as_expected'] for r in results))


if __name__ == '__main__':
    sys.dont_write_bytecode = True
    raise SystemExit(main())
