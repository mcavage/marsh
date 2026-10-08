#!/usr/bin/env python3
"""OPT-IN actual pinned Kit frontend regression, not a Docker recorder.

Uses a local Docker endpoint and an isolated credential-free CLI configuration.
Only cache-only builds: no push/load, workload execution, SBX or Cloud. Builds
can resolve public images and change builder cache; this is NOT read-only.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import stat
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
FRONTEND = 'docker/sandbox-kit:3@sha256:11bb68806aef6a68d45c10c0c3b3dc86c2f794a296b5a933661d9dadf760b753'
BASE = 'dhi.io/sbx-templates:shell-docker@sha256:36fd3782db091cec28ddfcb28458fbc96ed540435b91d910c1a00b07ec50d5a2'
CASES = (
    ('re2-end-anchor', r'^[0-9]+\z', '1', None, None),
    ('posix-digit', '^[[:digit:]]+$', '123', None, None),
    ('invalid-backreference', r'^(a)\1$', 'aa', None, 'invalid pattern:'),
    ('invalid-default-valid-override', '^[0-9]+$', 'bad', '123', None),
    ('nested-quantifier-mismatch', '^(a+)+$', 'a' * 20000 + 'b', None, 'does not match pattern'),
)
DOCKERFILE = f'FROM {BASE}\nARG RAW_VALUE\nUSER agent\nENTRYPOINT ["/bin/true"]\n'


def descriptor(pattern, default):
    return (f'# syntax={FRONTEND}\nschemaVersion: "3"\n'
            'displayName: Native pattern regression\nkind: workload\n'
            'provides: ["proof@1.0.0"]\ndockerfile: ./proof.dockerfile\n'
            'args:\n  value:\n    description: native regex proof\n'
            f'    default: {json.dumps(default)}\n    pattern: {json.dumps(pattern)}\n'
            '    buildArg: RAW_VALUE\ncapabilities:\n  - type: com.docker.sandbox/sbx@1\n')


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(chunk)
    return 'sha256:' + h.hexdigest()


def inventory(paths):
    return {str(path): digest(path) for path in paths}


def invoke(argv, env, evidence, label, timeout):
    """Finite owned process group; never signal any unrelated process."""
    out, err = evidence / (label + '.stdout'), evidence / (label + '.stderr')
    started = time.monotonic()
    with out.open('xb') as stdout, err.open('xb') as stderr:
        child = subprocess.Popen(argv, env=env, stdout=stdout, stderr=stderr, start_new_session=True)
        expired = False
        try:
            status = child.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            expired = True
            os.killpg(child.pid, signal.SIGKILL)  # only our new session, exact retained child
            status = child.wait()
    if max(out.stat().st_size, err.stat().st_size) > 8 * 1024 * 1024:
        raise ValueError('native evidence log exceeds 8 MiB; not accepted')
    return {'argv': argv, 'status': status, 'timed_out': expired,
            'seconds': time.monotonic() - started,
            'stdout_sha256': digest(out), 'stderr_sha256': digest(err)}, err.read_text(errors='replace')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--run-native', action='store_true', help='explicitly permit local cache-only builds')
    parser.add_argument('--docker', type=Path, required=True, help='actual Docker CLI executable')
    parser.add_argument('--buildx-plugin', type=Path, required=True, help='actual docker-buildx executable')
    parser.add_argument('--docker-host', required=True, help='explicit local unix:///path/to/docker.sock')
    parser.add_argument('--evidence', type=Path, required=True, help='new owner-private evidence directory')
    parser.add_argument('--timeout', type=int, default=120, help='per-command deadline, 1..600 seconds')
    args = parser.parse_args()
    if not args.run_native:
        parser.error('actual frontend execution requires --run-native; no Docker was invoked')
    if not args.docker_host.startswith('unix:///') or not 1 <= args.timeout <= 600:
        parser.error('select an explicit local Unix Docker endpoint and bounded deadline')
    docker, buildx = args.docker.resolve(strict=True), args.buildx_plugin.resolve(strict=True)
    for path in (docker, buildx):
        info = path.stat()
        if (not stat.S_ISREG(info.st_mode) or info.st_mode & 0o022
                or info.st_uid not in (0, os.getuid()) or not os.access(path, os.X_OK)):
            parser.error('tool binaries must be controlled executable regular files')
    evidence = Path(os.path.abspath(args.evidence))
    if evidence.exists() or evidence.is_symlink() or evidence.parent.resolve() != evidence.parent:
        parser.error('choose a new evidence directory with canonical existing parent')
    os.umask(0o077)
    evidence.mkdir(mode=0o700)
    # Pin the CLI's selected plugin in its own configuration, never read/copy the
    # user's Docker config, credential helpers, tokens, or provider environment.
    config, plugins, home = evidence / 'docker-config', evidence / 'plugins', evidence / 'home'
    for path in (config, plugins, home):
        path.mkdir(mode=0o700)
    (plugins / 'docker-buildx').symlink_to(buildx)
    (config / 'config.json').write_text(json.dumps({'cliPluginsExtraDirs': [str(plugins)]}) + '\n')
    env = {'PATH': str(docker.parent) + os.pathsep + '/usr/local/bin:/usr/bin:/bin',
           'HOME': str(home), 'DOCKER_CONFIG': str(config), 'DOCKER_HOST': args.docker_host,
           'LC_ALL': 'C', 'PYTHONDONTWRITEBYTECODE': '1'}
    # The Python parser installation may be supplied explicitly by the caller;
    # no Docker/provider credentials or global environment are inherited.
    if 'PYTHONPATH' in os.environ:
        env['PYTHONPATH'] = os.environ['PYTHONPATH']
    sources = [Path(__file__).resolve(), ROOT / 'scripts/publish-kits.py',
               ROOT / 'scripts/prepare-kit-inputs.py', ROOT / 'scripts/build_inputs.py',
               ROOT / 'crates/marsh-contracts/src/command_registry_rules.json',
               ROOT / 'scripts/requirements-publish.txt', Path(sys.executable).resolve(), docker, buildx]
    before = inventory(sources)
    result = {'schema': 'marsh.native-frontend-regression/v1', 'frontend': FRONTEND, 'base': BASE,
              'scope': 'actual local cache-only builds; public resolves/cache writes possible; no push/load/run/VM/Cloud',
              'python_version': sys.version, 'source_tools_before': before,
              'tools': [], 'cases': [], 'outcome': 'failed'}
    try:
        for label, argv in (('docker-version', [str(docker), 'version']),
                            ('buildx-version', [str(docker), 'buildx', 'version'])):
            row, _ = invoke(argv, env, evidence, label, args.timeout)
            result['tools'].append(row)
            if row['status'] or row['timed_out']:
                raise ValueError(f'{label} failed; no semantic proof')
        worktree = evidence / 'sources'
        worktree.mkdir()
        git = shutil.which('git')
        if git is None:
            raise ValueError('Git is required for the real publisher caller')
        subprocess.run([git, 'init', '-q', str(worktree)], env=env, check=True, timeout=15)
        contexts = []
        for name, pattern, default, override, diagnostic in CASES:
            context = worktree / name
            context.mkdir()
            (context / 'proof.yaml').write_text(descriptor(pattern, default))
            (context / 'proof.dockerfile').write_text(DOCKERFILE)
            contexts.append((context, override, diagnostic))
        frozen = inventory([p for context, _, _ in contexts for p in context.iterdir()])
        result['contexts_before'] = frozen
        for context, override, diagnostic in contexts:
            # Direct native case retains the exact 20,001-byte pathological
            # value; publisher's existing 4,096-byte bound is not weakened.
            argv = [str(docker), 'buildx', 'build', '--platform', 'linux/arm64',
                    '--file', str(context / 'proof.yaml'), '--output', 'type=cacheonly',
                    '--progress', 'plain',
                    *(['--build-arg', 'value=' + override] if override is not None else []), str(context)]
            row, stderr = invoke(argv, env, evidence, context.name + '-native', args.timeout)
            row.update(case=context.name, route='direct-native', expected_diagnostic=diagnostic)
            result['cases'].append(row)
            if (row['timed_out'] or (row['status'] == 0) != (diagnostic is None)
                    or (diagnostic and diagnostic not in stderr)):
                raise ValueError(f'{context.name}: native result differs (setup errors are not regex refusal proof)')
            commands, inputs = evidence / 'commands.json', evidence / 'inputs.json'
            commands.write_text(json.dumps({'probe': context.name}))
            inputs.write_text(json.dumps({context.name: {'args': {'value': override}}} if override is not None else {}))
            argv = [sys.executable, str(ROOT / 'scripts/publish-kits.py'), '--validate-only',
                    '--source-root', str(worktree), '--commands', str(commands), '--build-inputs', str(inputs),
                    '--output', str(evidence / 'must-not-exist-registry.json')]
            row, stderr = invoke(argv, env, evidence, context.name + '-publisher', args.timeout)
            row.update(case=context.name, route='actual-publisher-cacheonly')
            result['cases'].append(row)
            expected = 'invalid Kit argument value' if context.name == 'nested-quantifier-mismatch' else diagnostic
            if (row['timed_out'] or (row['status'] == 0) != (expected is None)
                    or (expected and expected not in stderr)):
                raise ValueError(f'{context.name}: publisher result differs')
            if (evidence / 'must-not-exist-registry.json').exists():
                raise ValueError('validate-only unexpectedly published a command registry')
            if inventory([Path(p) for p in frozen]) != frozen or inventory(sources) != before:
                raise ValueError('source/tool/context changed during native replay')
        result['outcome'] = 'passed'
    finally:
        result['source_tools_after'] = inventory(sources)
        if result['source_tools_after'] != before:
            result['outcome'] = 'failed-source-fence'
        (evidence / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    print(evidence / 'result.json')
    return 0 if result['outcome'] == 'passed' else 1


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        raise SystemExit(f'native-frontend-uat: {error}') from error
