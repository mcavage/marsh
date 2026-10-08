#!/usr/bin/env python3
"""Build and drive the MCP publication callers with one source/binary identity.

Owns temporary Cargo caches only. Host binaries are retained before that cache
is retired; caller compilation uses a second empty cache. No stock SBX is run.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[2]


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--cargo', default='cargo')
    parser.add_argument('--target-dir', required=True, type=Path)
    parser.add_argument('--evidence', required=True, type=Path)
    parser.add_argument('--long-prepare', action='store_true')
    parser.add_argument('--long-rollback', action='store_true')
    parser.add_argument('--acp', action='store_true', help='include all real Node/CLI/shared-publication cases, including long deadlines')
    args = parser.parse_args()
    output = args.target_dir.resolve() / 'mcp-callers'
    output.mkdir(parents=True, exist_ok=True)
    evidence = args.evidence.resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    binary_dir = output / 'bin'
    binary_dir.mkdir(exist_ok=True)
    git_root = Path(subprocess.check_output(['git', 'rev-parse', '--show-toplevel'], cwd=ROOT, text=True).strip()).resolve()
    if git_root == ROOT:
        names = {os.fsdecode(name) for name in subprocess.check_output(
            ['git', 'ls-files', '-co', '--exclude-standard', '-z'], cwd=ROOT).split(b'\0') if name}
    else:
        # A retained immutable source capsule lives below the ignored evidence
        # directory. git ls-files there is empty, never an acceptable fence.
        excluded = {'.git', 'target', '__pycache__', 'node_modules'}
        names = {str(path.relative_to(ROOT)) for path in ROOT.rglob('*') if path.is_file()
                 and not excluded.intersection(path.relative_to(ROOT).parts)}
    sources = {name: sha(ROOT / name) for name in names if (ROOT / name).is_file()}
    assert 'Cargo.toml' in sources and 'crates/marsh/src/main.rs' in sources and len(sources) > 100, 'missing source fence'
    (evidence / 'source-before.json').write_text(json.dumps(sources, sort_keys=True, indent=2))
    env = os.environ.copy()
    env.update(CARGO_INCREMENTAL='0', CARGO_BUILD_JOBS='1', CARGO_PROFILE_DEV_DEBUG='0',
               CARGO_PROFILE_TEST_DEBUG='0', CARGO_PROFILE_DEV_CODEGEN_UNITS='1', CARGO_PROFILE_TEST_CODEGEN_UNITS='1',
               CARGO_PROFILE_DEV_OPT_LEVEL='s', CARGO_PROFILE_TEST_OPT_LEVEL='s',
               CARGO_PROFILE_DEV_STRIP='symbols', CARGO_PROFILE_TEST_STRIP='symbols',
               MARSH_BINARY=str(binary_dir / 'marsh'), MARSH_MCP_BIN=str(binary_dir / 'marsh-mcp'),
               MARSH_LOAD_TEST_BIN=str(binary_dir / 'marsh'), MARSH_MCP_TEST_EVIDENCE=str(evidence))
    results = []

    def run(label, command, extra=None, timeout=1200):
        entry = {'label': label, 'argv': command, 'started': time.time()}
        with (evidence / (label + '.log')).open('w') as log:
            child = subprocess.Popen(command, cwd=ROOT, env={**env, **(extra or {})}, stdout=log,
                                     stderr=subprocess.STDOUT, start_new_session=True)
            entry.update(pid=child.pid, pgid=child.pid)
            (evidence / (label + '-process.json')).write_text(json.dumps(entry, indent=2))
            try:
                deadline = time.monotonic() + timeout
                peak = 0
                while child.poll() is None:
                    usage = subprocess.run(['du', '-sk', env['CARGO_TARGET_DIR']], capture_output=True, text=True)
                    size = int(usage.stdout.split()[0]) * 1024 if usage.stdout.strip() else 0
                    peak = max(peak, size)
                    if size > 1_500_000_000:
                        raise RuntimeError(f'owned Cargo cache exceeded 1.5 GB: {size}')
                    if time.monotonic() >= deadline:
                        raise TimeoutError(f'owned caller deadline exceeded: {label}')
                    time.sleep(1)
                entry['cache_peak_bytes'] = peak
                entry['exit'] = child.wait()
            except BaseException:
                if child.poll() is None:
                    os.killpg(child.pid, signal.SIGTERM)
                    try:
                        child.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(child.pid, signal.SIGKILL)
                        child.wait()
                raise
            finally:
                entry['ended'] = time.time()
                (evidence / (label + '-process.json')).write_text(json.dumps(entry, indent=2))
        results.append(entry)
        (evidence / 'results.json').write_text(json.dumps(results, indent=2))
        print(f'{label}: exit {entry["exit"]}', flush=True)
        if entry['exit']:
            print((evidence / (label + '.log')).read_text())
            raise SystemExit(entry['exit'])

    # TMPDIR may select an owned native filesystem in a VM. Never delete
    # a contributor's shared TARGET_DIR or another invocation's cache.
    cache_root = os.environ.get('MARSH_MCP_CACHE_ROOT')
    if cache_root:
        Path(cache_root).mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='marsh-mcp-host-', dir=cache_root) as cache:
        env['CARGO_TARGET_DIR'] = cache
        run('build-host', [args.cargo, 'build', '--locked', '-p', 'marsh', '-p', 'marsh-mcp',
            '-p', 'marsh-backend', '--bin', 'marsh', '--bin', 'marsh-mcp', '--bin', 'marshd'])
        for name in ('marsh', 'marsh-mcp', 'marshd'):
            source = Path(cache) / 'debug' / name
            destination = binary_dir / name
            shutil.copy2(source, destination)
            assert sha(destination) == sha(source), name
    identities = {name: sha(binary_dir / name) for name in ('marsh', 'marsh-mcp', 'marshd')}
    print(json.dumps({'host_binaries': identities}, sort_keys=True), flush=True)
    (evidence / 'host-binaries-before.json').write_text(json.dumps(identities, indent=2))
    tests = {}
    with tempfile.TemporaryDirectory(prefix='marsh-mcp-callers-', dir=cache_root) as cache:
        env['CARGO_TARGET_DIR'] = cache
        run('compile-mcp', [args.cargo, 'test', '--locked', '-p', 'marsh-mcp', '--test', 'mcp_load',
            '--test', 'export_integration', '--no-run', '--message-format=json'])
        run('compile-daemon', [args.cargo, 'test', '--locked', '-p', 'marsh-daemon', '--lib',
            '--no-run', '--message-format=json'])
        if args.acp:
            run('compile-acp', [args.cargo, 'test', '--locked', '-p', 'marsh-acp', '--test', 'fixture_caller',
                '--no-run', '--message-format=json'])
        test_dir = output / 'tests'
        test_dir.mkdir(exist_ok=True)
        for label in ('compile-mcp', 'compile-daemon', *(['compile-acp'] if args.acp else [])):
            for line in (evidence / (label + '.log')).read_text().splitlines():
                try:
                    item = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if item.get('reason') != 'compiler-artifact' or not item.get('executable') or not item.get('profile', {}).get('test'):
                    continue
                name = item['target']['name']
                if name not in ('mcp_load', 'export_integration', 'marsh_daemon', 'fixture_caller'):
                    continue
                source = Path(item['executable'])
                destination = test_dir / name
                shutil.copy2(source, destination)
                tests[name] = str(destination)
                assert sha(source) == sha(destination)
        assert set(tests) == {'mcp_load', 'export_integration', 'marsh_daemon'} | ({'fixture_caller'} if args.acp else set()), tests
    test_identities = {name: {'path': path, 'sha256': sha(Path(path))} for name, path in tests.items()}
    (evidence / 'test-binaries.json').write_text(json.dumps(test_identities, indent=2))
    expected = {
        'mcp_load': ['cli_load_keeps_original_mcp_caller_valid_until_unpublish'],
        'marsh_daemon': ['mcp_load_tests::load_via_relay_prepares_only_after_validation_and_preserves_publication',
                         'mcp_load_tests::publication_transport_distinguishes_pre_dispatch_rejection_and_lost_reply'],
        'export_integration': ['observed_worker_outcome_survives_real_mcp_and_daemon_sockets'],
    }
    for name, required in expected.items():
        run('list-' + name, [tests[name], '--list'])
        listed = (evidence / ('list-' + name + '.log')).read_text()
        assert all(case + ': test' in listed for case in required), f'required caller absent from {name}'
    run('mcp-caller', [tests['mcp_load'], '--ignored', '--nocapture'])
    run('relay-callers', [tests['marsh_daemon'], 'mcp_load_tests', '--include-ignored', '--nocapture'])
    run('typed-execution', [tests['export_integration'], 'observed_worker_outcome_survives_real_mcp_and_daemon_sockets', '--nocapture'])
    for name in ('attached_relay_waits_for_slow_mcp_host_reply', 'concurrent_mcp_publish_binds_declaration_to_its_actual_publisher'):
        run(name, [tests['marsh_daemon'], name, '--nocapture'])
    run('publication-cli', ['python3', 'tests/mcp/test_publication_cli.py'])
    if args.long_prepare:
        run('long-prepare', [tests['marsh_daemon'], 'load_via_relay_prepares_only_after_validation_and_preserves_publication',
            '--ignored', '--nocapture'], {'MARSH_MCP_LONG_PREPARE': '1'}, timeout=450)
    if args.long_rollback:
        run('long-rollback', [tests['marsh_daemon'], 'load_via_relay_prepares_only_after_validation_and_preserves_publication',
            '--ignored', '--nocapture'], {'MARSH_MCP_LONG_ROLLBACK': '1'}, timeout=500)
    if args.acp:
        with tempfile.TemporaryDirectory(prefix='marsh-mcp-acp-home-') as host_home:
            run('acp-shared-callers', [tests['fixture_caller'], '--include-ignored', '--nocapture', '--test-threads=1'],
                {'MARSH_ACP_CALLER_BIN': str(binary_dir / 'marsh'), 'HOME': host_home}, timeout=2400)
        run('acp-real-host-peer', ['python3', 'tests/acceptance/acp-fixture/typed_host_caller.py', '--marsh', str(binary_dir / 'marsh')])
    assert identities == {name: sha(binary_dir / name) for name in identities}, 'binaries changed during callers'
    (evidence / 'host-binaries-after.json').write_text(json.dumps(identities, indent=2))
    assert test_identities == {name: {'path': path, 'sha256': sha(Path(path))} for name, path in tests.items()}, 'compiled caller changed during execution'
    (evidence / 'test-binaries-after.json').write_text(json.dumps(test_identities, indent=2))
    after = {name: sha(ROOT / name) for name in sources}
    (evidence / 'source-after.json').write_text(json.dumps(after, sort_keys=True, indent=2))
    assert sources == after, 'source changed during gate; rebuild the candidate'
    print('MCP controlled publication gate passed; not stock/Cloud qualification.')


if __name__ == '__main__':
    main()
