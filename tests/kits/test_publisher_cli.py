#!/usr/bin/env python3
"""Real publisher CLI callers; Docker is a recording subprocess, not live Buildx."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
PUBLISHER = ROOT / "scripts/publish-kits.py"
PREPARER = ROOT / "scripts/prepare-kit-inputs.py"
PIN = "reg.example/base@sha256:" + "a" * 64


class PublisherCli(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="marsh-publisher-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.source = self.root / "source with spaces"
        self.source.mkdir()
        subprocess.run(["git", "init", "-q", str(self.source)], check=True)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.log = self.root / "buildx.jsonl"
        self.commands = self.root / "commands.json"
        self.inputs = self.root / "inputs.json"
        self.output = self.root / "out" / "commands.json"
        docker = self.bin / "docker"
        docker.write_text(f"#!{sys.executable}\n" + '''
import hashlib, json, os, pathlib, sys
args = sys.argv[1:]
assert args[:2] == ['buildx', 'build'], args
# Model the actual pinned frontend's lack of subrequests, not a guessed API.
assert not any(a.startswith('--call') for a in args), 'unsupported frontend subrequest'
context = pathlib.Path(args[-1])
record = {'args': args, 'context': str(context), 'mode': context.stat().st_mode & 0o777,
          'parent_mode': context.parent.stat().st_mode & 0o777,
          'directories': {str(p.relative_to(context)): p.stat().st_mode & 0o777
                          for p in context.rglob('*') if p.is_dir()},
          'file_sizes': {str(p.relative_to(context)): p.stat().st_size
                         for p in context.rglob('*') if p.is_file()},
          'file_modes': {str(p.relative_to(context)): p.stat().st_mode & 0o777
                         for p in context.rglob('*') if p.is_file()},
          'files': {str(p.relative_to(context)): hashlib.sha256(p.read_bytes()).hexdigest()
                    for p in context.rglob('*') if p.is_file() and not p.is_symlink()}}
with open(os.environ['BUILDX_LOG'], 'a') as log:
    log.write(json.dumps(record) + '\\n')
if os.environ.get('MUTATE_INPUT') and (not os.environ.get('MUTATE_ON_PUSH') or any('push=true' in a for a in args)):
    p = pathlib.Path(os.environ['MUTATE_INPUT'])
    p.write_bytes(p.read_bytes() + b'changed during build')
if os.environ.get('MUTATE_CONTEXT'):
    (context / 'added-after-staging').write_text('mutation')
if os.environ.get('SWAP_OUTPUT_PARENT'):
    parent = pathlib.Path(os.environ['SWAP_OUTPUT_PARENT'])
    parent.rename(str(parent) + '-moved')
    parent.symlink_to(os.environ['OUTSIDE'])
if os.environ.get('FAIL_BUILDX') or (context / 'native-invalid').exists() or (os.environ.get('NATIVE_PATTERN_FAILURE') and 'type=cacheonly' in args):
    raise SystemExit(42)
metadata = pathlib.Path(args[args.index('--metadata-file') + 1])
metadata.write_text(json.dumps({'containerimage.digest': 'sha256:' + 'b' * 64}))
''')
        docker.chmod(0o755)
        home = self.root / "home"
        home.mkdir()
        self.env = {"PATH": str(self.bin) + os.pathsep + os.environ["PATH"],
                    "HOME": str(home), "TMPDIR": str(self.root), "LC_ALL": "C",
                    "BUILDX_LOG": str(self.log), "PYTHONDONTWRITEBYTECODE": "1"}
        # Only the test's parser installation path may cross the boundary;
        # provider credentials, Docker config and real home are not inherited.
        if "PYTHONPATH" in os.environ:
            self.env["PYTHONPATH"] = os.environ["PYTHONPATH"]
        self.kit("first")
        self.kit("last")
        self.commands.write_text(json.dumps({"a": "first", "z": "last"}))
        self.inputs.write_text("{}")

    def kit(self, name, arguments=""):
        path = self.source / name
        path.mkdir(exist_ok=True)
        (path / "kit.yaml").write_text('schemaVersion: "3"\nkind: workload\ndisplayName: Fixture\ndockerfile: ./kit.dockerfile\n' + arguments)
        (path / "kit.dockerfile").write_text("FROM scratch\nCOPY payload /payload\n")
        (path / "payload").write_text(name)
        return path

    def observed(self, result):
        if destination := os.environ.get('PUBLISHER_TEST_EVIDENCE'):
            directory = Path(destination) / getattr(self, '_evidence_label', self.id())
            directory.mkdir(mode=0o700, parents=True, exist_ok=True)
            number = len(list(directory.glob('*.json')))
            record = {'scope': 'real CLI / controlled image and Docker peers; not image qualification',
                      'argv': [str(word) for word in result.args], 'status': result.returncode,
                      'stdout': result.stdout, 'stderr': result.stderr,
                      'docker_records': self.records()}
            with (directory / f'{number:03d}.json').open('x') as stream:
                json.dump(record, stream, indent=2)
        return result

    def call(self, *extra, timeout=120):
        return self.observed(subprocess.run([sys.executable, str(getattr(self, "publisher", PUBLISHER)), "--source-root", str(self.source),
                               "--commands", str(self.commands), "--build-inputs", str(self.inputs),
                               "--repository-prefix", "reg.example/test", "--output", str(self.output),
                               *extra], env=self.env, cwd=getattr(self, "cwd", None),
                              capture_output=True, text=True, timeout=timeout))

    def records(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def refused_without_build(self):
        result = self.call()
        self.assertNotEqual(result.returncode, 0, result)
        self.assertTrue(result.stderr.strip(), result)
        self.assertEqual(self.records(), [], result)
        self.assertFalse(self.output.exists(), result)
        return result

    def test_all_sources_are_private_and_ignore_stale_files(self):
        first = self.source / "first"
        (first / ".gitignore").write_text("artifacts/\nignored.txt\n")
        (first / "artifacts").mkdir()
        (first / "artifacts" / "stale").write_text("not an admitted artifact")
        (first / "ignored.txt").write_text("not source")
        (first / "image-repair").mkdir()
        (first / "image-repair" / "repair.py").write_text("# security owner's untracked source\n")
        result = self.call()
        self.assertEqual(result.returncode, 0, result)
        records = self.records()
        self.assertEqual(len(records), 4)
        self.assertTrue(all('type=cacheonly' in row['args'] for row in records[:2]))
        self.assertTrue(all(any('push=true' in arg for arg in row['args']) for row in records[2:]))
        self.assertTrue(all(not any(a.startswith('--call') for a in row['args']) for row in records))
        self.assertEqual(records[0]['context'], records[2]['context'])
        self.assertEqual(records[1]['files'], records[3]['files'])
        self.assertIn('Untracked build input', result.stderr)
        self.assertIn('payload', result.stderr)
        for record in records:
            self.assertFalse(Path(record["context"]).is_relative_to(self.source), record)
            self.assertEqual(record["parent_mode"], 0o700)
            self.assertEqual(record["mode"], 0o755)
            self.assertTrue(all(mode == 0o755 for mode in record["directories"].values()))
            self.assertTrue(all(mode & 0o444 == 0o444 for mode in record["file_modes"].values()))
            self.assertIn("payload", record["files"])
            self.assertFalse(any("stale" in p or p == "ignored.txt" for p in record["files"]))
        self.assertIn("image-repair/repair.py", records[0]["files"])
        registry = json.loads(self.output.read_text())
        self.assertEqual(set(registry), {"a", "z"})
        self.assertTrue(all("@sha256:" in ref for ref in registry.values()))
        self.assertEqual(self.output.stat().st_mode & 0o777, 0o600)
        self.assertTrue((first / "artifacts" / "stale").exists())

    def test_json_fifos_refuse_promptly(self):
        for filename in (self.commands, self.inputs):
            with self.subTest(path=filename.name):
                old = filename.read_bytes()
                filename.unlink()
                os.mkfifo(filename)
                try:
                    self.refused_without_build()
                finally:
                    filename.unlink()
                    filename.write_bytes(old)

    def test_json_symlinks_writable_oversize_and_duplicates_refuse(self):
        for filename in (self.commands, self.inputs):
            original = filename.read_bytes()
            with self.subTest(path=filename.name, kind="symlink"):
                target = self.root / "real-json"
                target.write_bytes(original)
                filename.unlink()
                filename.symlink_to(target)
                self.refused_without_build()
                filename.unlink()
                filename.write_bytes(original)
            with self.subTest(path=filename.name, kind="writable"):
                filename.chmod(0o666)
                self.refused_without_build()
                filename.chmod(0o600)
            with self.subTest(path=filename.name, kind="oversize"):
                filename.write_bytes(b" " * (4 * 1024 * 1024 + 1))
                self.refused_without_build()
                filename.write_bytes(original)
            with self.subTest(path=filename.name, kind="duplicate"):
                filename.write_text('{"a":"first","a":"last"}')
                self.refused_without_build()
                filename.write_bytes(original)

    def test_last_descriptor_required_arg_blocks_all_builds(self):
        self.kit("last", "args:\n  base:\n    pattern: '^reg[.]example/.+@sha256:[a-f0-9]{64}$'\n    buildArg: RAW_BASE\n")
        self.refused_without_build()

    def test_unknown_raw_build_arg_blocks_all_docker_calls(self):
        self.kit("last", "args:\n  version:\n    default: '1.2.3'\n    pattern: '^[0-9]+[.][0-9]+[.][0-9]+$'\n    buildArg: VERSION\n")
        self.inputs.write_text(json.dumps({"last": {"args": {"VERSION": "bypass"}}}))
        self.refused_without_build()

    def test_bad_last_descriptor_and_artifact_block_all_builds(self):
        (self.source / "last" / "kit.yaml").write_text("not: [valid YAML\n")
        self.refused_without_build()
        self.kit("last")
        fifo = self.root / "artifact"
        os.mkfifo(fifo)
        self.inputs.write_text(json.dumps({"last": {"files": {"artifacts/shell": str(fifo)}}}))
        self.refused_without_build()

    def test_canonical_aliases_share_inputs_and_reject_conflicts(self):
        self.kit("first", "args:\n  base:\n    buildArg: RAW_BASE\n")
        self.commands.write_text(json.dumps({"a": "first", "alias": "first/", "dot": "./first"}))
        self.inputs.write_text(json.dumps({"first/": {"args": {"base": PIN}}}))
        result = self.call()
        self.assertEqual(result.returncode, 0, result)
        self.assertEqual(len(self.records()), 2)
        self.assertIn("base=" + PIN, self.records()[0]["args"])
        self.assertEqual(len(set(json.loads(self.output.read_text()).values())), 1)
        self.log.unlink()
        self.output.unlink()
        self.inputs.write_text(json.dumps({"first": {"args": {"base": PIN}},
                                          "./first": {"args": {"base": "different"}}}))
        self.refused_without_build()

    def test_explicit_files_and_default_arguments_are_staged(self):
        self.kit("first", "args:\n  version:\n    default: '1.2.3'\n    pattern: '^[0-9]+[.][0-9]+[.][0-9]+$'\n")
        artifact = self.root / "shell"
        artifact.write_bytes(b"explicit artifact bytes")
        artifact.chmod(0o600)
        self.inputs.write_text(json.dumps({"first": {"files": {"artifacts/shell": str(artifact)}}}))
        result = self.call()
        self.assertEqual(result.returncode, 0, result)
        record = self.records()[0]
        self.assertIn("version=1.2.3", record["args"])
        self.assertEqual(record["files"]["artifacts/shell"], hashlib.sha256(artifact.read_bytes()).hexdigest())
        self.assertEqual(record["file_modes"]["artifacts/shell"], 0o644)
        self.assertEqual(record["directories"]["artifacts"], 0o755)
        self.assertFalse((self.source / "first" / "artifacts").exists())

    def test_invalid_output_and_prefix_refuse_before_creating_directories(self):
        for extra, diagnostic in ((["--output", "."], "registry file"),
                                  (["--repository-prefix", "INVALID"], "repository-prefix")):
            with self.subTest(extra=extra):
                result = self.call(*extra)
                self.assertNotEqual(result.returncode, 0, result)
                self.assertIn(diagnostic, result.stderr)
                self.assertEqual(self.records(), [])
                self.assertFalse(self.output.parent.exists())

    def test_output_cannot_replace_explicit_or_configuration_inputs(self):
        artifact = self.root / 'owned-artifact'
        artifact.write_bytes(b'preserve explicit source')
        self.inputs.write_text(json.dumps({'first': {'files': {'artifacts/tool': str(artifact)}}}))
        for destination in (artifact, self.inputs, self.commands):
            with self.subTest(output=str(destination)):
                previous = destination.read_bytes()
                result = self.call('--output', destination)
                self.assertNotEqual(result.returncode, 0, result)
                self.assertIn('overwrite', result.stderr)
                self.assertEqual(destination.read_bytes(), previous)
                self.assertEqual(self.records(), [])

    def test_explicit_docker_ignore_controls_refuse_before_build(self):
        artifact = self.root / "ignore"
        artifact.write_text("**\n")
        for name in (".dockerignore", "kit.dockerfile.dockerignore", "nested/.dockerignore",
                     ".DOCKERIGNORE", "kit.yaml.DockerIgnore", ".doc\u212aerignore"):
            with self.subTest(name=name):
                self.inputs.write_text(json.dumps({"first": {"files": {name: str(artifact)}}}))
                result = self.refused_without_build()
                self.assertIn("Docker ignore", result.stderr)

    def test_native_pattern_admission_uses_full_cacheonly_and_no_python_translation(self):
        # Recorder proves routing/order, NOT frontend dialect semantics. The
        # maintained opt-in native_frontend_uat.py exercises the actual parser.
        for pattern in (r'^[0-9]+\z', '^[[:digit:]]+$', r'^(a)\1$', '^(a+)+$'):
            with self.subTest(pattern=pattern):
                self.kit('last', "args:\n  v:\n    default: '123'\n    pattern: '" + pattern + "'\n")
                self.log.unlink(missing_ok=True)
                result = self.call('--validate-only')
                self.assertEqual(result.returncode, 0, result)
                self.assertEqual(len(self.records()), 2)
                self.assertTrue(all('type=cacheonly' in row['args'] for row in self.records()))
        self.env['NATIVE_PATTERN_FAILURE'] = '1'
        self.log.unlink()
        result = self.call()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(self.records()), 1)
        self.assertIn('type=cacheonly', self.records()[0]['args'])
        self.assertFalse(self.output.exists())

    def test_native_effective_override_is_forwarded_without_python_pattern_checks(self):
        self.kit('last', "args:\n  value:\n    default: 'bad'\n    pattern: '^[0-9]+$'\n")
        self.inputs.write_text(json.dumps({'last': {'args': {'value': '123'}}}))
        result = self.call('--validate-only')
        self.assertEqual(result.returncode, 0, result)
        self.assertIn('value=123', self.records()[-1]['args'])
        self.assertNotIn('value=bad', self.records()[-1]['args'])

    def test_actual_frontend_harness_requires_explicit_opt_in_before_any_tool_call(self):
        evidence = self.root / 'native-evidence'
        result = subprocess.run([sys.executable, str(ROOT / 'tests/kits/native_frontend_uat.py'),
                                 '--docker', str(self.bin / 'docker'), '--buildx-plugin', str(self.bin / 'docker'),
                                 '--docker-host', 'unix:///not-contacted.sock', '--evidence', str(evidence)],
                                env=self.env, capture_output=True, text=True, timeout=8)
        self.assertNotEqual(result.returncode, 0, result)
        self.assertIn('--run-native', result.stderr)
        self.assertEqual(self.records(), [])
        self.assertFalse(evidence.exists())

    def test_shared_registry_rules_names_capacity_and_document_bound(self):
        rules = json.loads((ROOT / 'crates/marsh-contracts/src/command_registry_rules.json').read_text())
        for name in [*rules['reserved_names'], '-bad', 'has.dot', 'has space', 'é', 'a' * 129]:
            with self.subTest(name=name):
                self.commands.write_text(json.dumps({name: 'first'}))
                self.refused_without_build()
        allowed = {'1numeric': 'first', '_underscore': 'first', 'with-hyphen': 'first'}
        self.commands.write_text(json.dumps(allowed))
        result = self.call('--validate-only')
        self.assertEqual(result.returncode, 0, result)
        self.log.unlink()
        self.commands.write_text(json.dumps({f'cmd{i}': 'first' for i in range(rules['max_commands'])}))
        result = self.call('--validate-only')
        self.assertEqual(result.returncode, 0, result)
        self.assertEqual(len(self.records()), 1)
        self.log.unlink()
        self.commands.write_text(json.dumps({f'cmd{i}': 'first' for i in range(rules['max_commands'] + 1)}))
        self.refused_without_build()
        self.commands.write_bytes(b' ' * (rules['max_document_bytes'] + 1))
        self.refused_without_build()

    def test_personal_git_excludes_do_not_change_staged_source(self):
        (self.source / ".git" / "info" / "exclude").write_text("payload\n")
        config = Path(self.env["HOME"]) / ".config" / "git"
        config.mkdir(parents=True)
        (config / "ignore").write_text("payload\n")
        self.env["XDG_CONFIG_HOME"] = str(config.parent)
        result = self.call()
        self.assertEqual(result.returncode, 0, result)
        self.assertTrue(all("payload" in record["files"] for record in self.records()))

    def test_output_symlink_parent_refuses_before_build(self):
        outside = self.root / "outside"
        outside.mkdir()
        self.output.parent.symlink_to(outside)
        self.refused_without_build()
        self.assertEqual(list(outside.iterdir()), [])

    def test_output_parent_swap_cannot_redirect_atomic_write(self):
        self.commands.write_text(json.dumps({"a": "first"}))
        self.output.parent.mkdir(mode=0o700)
        outside = self.root / "outside"
        outside.mkdir()
        self.env.update(SWAP_OUTPUT_PARENT=str(self.output.parent), OUTSIDE=str(outside))
        result = self.call()
        self.assertEqual(list(outside.iterdir()), [], result)
        self.assertNotEqual(result.returncode, 0, result)
        self.assertTrue(all('type=cacheonly' in row['args'] for row in self.records()))
        self.assertFalse((Path(str(self.output.parent) + '-moved') / self.output.name).exists())

    def test_native_last_context_failure_never_pushes_an_image(self):
        # The recording Buildx models a validation failure that only the native
        # frontend knows (e.g. capability schema or a missing Dockerfile COPY).
        (self.source / 'last' / 'native-invalid').write_text('bad native input')
        result = self.call()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len(self.records()), 2)
        self.assertTrue(all('type=cacheonly' in row['args'] for row in self.records()))
        self.assertFalse(self.output.exists())

    def test_yaml_duplicates_and_aliases_refuse_before_build(self):
        path = self.source / 'last' / 'kit.yaml'
        original = path.read_text()
        cases = [
            ('args: {}\nargs: {}\n', 'duplicate field'),
            ('args: &args {base: *args}\n', 'uses aliases'),
        ]
        for extra, diagnostic in cases:
            with self.subTest(diagnostic=diagnostic):
                path.write_text(original + extra)
                result = self.refused_without_build()
                self.assertIn(diagnostic, result.stderr)

    def test_buildx_failure_preserves_existing_registry(self):
        self.output.parent.mkdir(mode=0o700)
        self.output.write_bytes(b"existing registry\n")
        self.env["FAIL_BUILDX"] = "1"
        result = self.call()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.output.read_bytes(), b"existing registry\n")
        self.assertFalse(list(self.output.parent.glob(".commands-*")))

    def test_validate_only_has_no_push_or_registry(self):
        result = self.call("--validate-only")
        self.assertEqual(result.returncode, 0, result)
        self.assertEqual(len(self.records()), 2)
        self.assertFalse(self.output.exists())
        for record in self.records():
            self.assertIn("type=cacheonly", record["args"])
            self.assertFalse(any("push=true" in arg for arg in record["args"]))

    def canonical_checkout(self):
        """Actual product sources in a private Git checkout, not empty-map wiring."""
        self.publisher = self.source / 'scripts/publish-kits.py'
        for relative in ('scripts/publish-kits.py', 'scripts/prepare-kit-inputs.py',
                         'scripts/build_inputs.py', 'scripts/npm-notices.mjs', '.gitignore',
                         'crates/marsh-contracts/src/command_registry_rules.json',
                         'packaging/commands.json', 'packaging/dhi-notices/collected',
                         'packaging/image-repair', 'tests/acceptance/fixture',
                         *['kits/' + name for name in ('marsh-shell', 'marsh-claude',
                           'marsh-codex', 'marsh-pi')]):
            origin, dest = ROOT / relative, self.source / relative
            dest.parent.mkdir(parents=True, exist_ok=True)
            if origin.is_dir():
                shutil.copytree(origin, dest, ignore=shutil.ignore_patterns('__pycache__', '*.pyc'))
            else:
                shutil.copy2(origin, dest)
        subprocess.run(['git', '-C', str(self.source), 'add', '-A'], check=True)
        self.commands = self.source / 'packaging/commands.json'

    def prepare(self, *extra):
        return self.observed(subprocess.run([sys.executable, str(getattr(self, 'preparer', self.source / 'scripts/prepare-kit-inputs.py')),
                               '--commands', str(self.commands), '--output', str(self.inputs), *map(str, extra)],
                              env=self.env, capture_output=True, text=True, timeout=120))

    def assert_canonical_bytes(self, records):
        # Independent oracle: hash the actual canonical files, never receipt claims.
        for prefix, tree in (('dhi-notices', 'packaging/dhi-notices/collected'),
                             ('image-repair', 'packaging/image-repair')):
            expected = {prefix + '/' + str(path.relative_to(self.source / tree)):
                        hashlib.sha256(path.read_bytes()).hexdigest()
                        for path in (self.source / tree).rglob('*') if path.is_file()}
            for record in records:
                actual = {name: digest for name, digest in record['files'].items()
                          if name.startswith(prefix + '/')}
                self.assertEqual(actual, expected)
                self.assertTrue(all(record['file_modes'][name] == 0o644 for name in actual))
                self.assertTrue(all(mode == 0o755 for mode in record['directories'].values()))
        collector = hashlib.sha256((self.source / 'scripts/npm-notices.mjs').read_bytes()).hexdigest()
        self.assertTrue(all(row['files']['collect-notices.mjs'] == collector
                            for row in records if 'collect-notices.mjs' in row['files']))

    def test_real_default4_and_fixture_canonical_prepare_publish_composition(self):
        self.canonical_checkout()
        for commands, count in ((self.commands, 4),
                                (self.source / 'tests/acceptance/fixture/commands.json', 1)):
            with self.subTest(commands=str(commands)):
                self.commands = commands
                result = self.prepare()
                self.assertEqual(result.returncode, 0, result)
                self.assertFalse(Path(str(self.inputs) + '.sources.json').exists())
                document = json.loads(self.inputs.read_text())
                self.assertEqual(document['schema'], 'marsh.prepared-kit-inputs/v2')
                self.assertLess(self.inputs.stat().st_size, 4 * 1024 * 1024)
                self.assertLessEqual(self.inputs.read_bytes().count(os.fsencode(self.source)), 32)
                self.log.unlink(missing_ok=True)
                result = self.call('--validate-only')
                self.assertEqual(result.returncode, 0, result)
                records = self.records()
                self.assertEqual(len(records), count)
                self.assert_canonical_bytes(records)
                self.assertFalse(self.output.exists())

    def test_bundled_empty_map_refused_without_docker(self):
        self.canonical_checkout()
        result = self.refused_without_build()
        self.assertIn('prepare-kit-inputs.py', result.stderr)

    def test_prepared_stale_actual_sources_refuse_before_docker_and_preserve_outputs(self):
        self.canonical_checkout()
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result)
        self.output.parent.mkdir(mode=0o700)
        self.output.write_bytes(b'previous registry\n')
        prior = self.inputs.read_bytes()
        changed = ['packaging/dhi-notices/collected/README.md',
                   'packaging/image-repair/artifacts.json', 'scripts/npm-notices.mjs',
                   'kits/marsh-shell/shell.yaml', 'kits/marsh-shell/marsh-entrypoint.sh',
                   'scripts/prepare-kit-inputs.py', 'scripts/build_inputs.py',
                   'scripts/publish-kits.py', 'packaging/commands.json',
                   'crates/marsh-contracts/src/command_registry_rules.json']
        for relative in changed:
            path = self.source / relative
            old = path.read_bytes()
            with self.subTest(modified=relative):
                path.write_bytes(old + b'\n')
                result = self.call()
                path.write_bytes(old)
                self.assertNotEqual(result.returncode, 0, result)
                self.assertEqual(self.records(), [], result)
                self.assertEqual(self.output.read_bytes(), b'previous registry\n')
                self.assertEqual(self.inputs.read_bytes(), prior)
        for relative in ('packaging/dhi-notices/collected/added.txt',
                         'packaging/image-repair/added.txt', 'kits/marsh-shell/scratch.env'):
            path = self.source / relative
            with self.subTest(added=relative):
                path.write_text('not a credential')
                result = self.call()
                path.unlink()
                self.assertNotEqual(result.returncode, 0, result)
                self.assertEqual(self.records(), [])
        for relative in ('packaging/dhi-notices/collected/README.md',
                         'kits/marsh-shell/marsh-entrypoint.sh'):
            path = self.source / relative
            old = path.read_bytes()
            with self.subTest(deleted=relative):
                path.unlink()
                result = self.call()
                path.write_bytes(old)
                self.assertNotEqual(result.returncode, 0, result)
                self.assertEqual(self.records(), [])

    def test_canonical_namespace_additions_hardlinks_and_uncontrolled_inputs_refuse(self):
        self.canonical_checkout()
        self.assertEqual(self.prepare().returncode, 0)
        prior = self.inputs.read_bytes()
        extra = self.root / 'extra.json'
        artifact = self.root / 'artifact'
        artifact.write_text('not secret')
        for name in ('dhi-notices/extra', 'image-repair/extra', 'DHI-NOTICES/extra'):
            extra.write_text(json.dumps({'kits/marsh-shell': {'files': {name: str(artifact)}}}))
            result = self.prepare('--extra-inputs', extra)
            self.assertNotEqual(result.returncode, 0, result)
            self.assertIn('canonical namespace', result.stderr)
            self.assertEqual(self.inputs.read_bytes(), prior)
        notices = self.source / 'packaging/dhi-notices/collected'
        for kind in ('hardlink', 'symlink', 'fifo', 'writable', 'oversize'):
            path = notices / 'uncontrolled'
            with self.subTest(kind=kind):
                if kind == 'hardlink':
                    os.link(artifact, path)
                elif kind == 'symlink':
                    path.symlink_to(artifact)
                elif kind == 'fifo':
                    os.mkfifo(path)
                else:
                    path.write_text('input')
                    if kind == 'writable':
                        path.chmod(0o666)
                    else:
                        with path.open('wb') as stream:
                            stream.truncate(128 * 1024 * 1024 + 1)
                result = self.prepare()
                path.unlink()
                self.assertNotEqual(result.returncode, 0, result)
                self.assertEqual(self.inputs.read_bytes(), prior)
                self.assertEqual(self.records(), [])
        notices.chmod(0o777)
        result = self.prepare()
        notices.chmod(0o755)
        self.assertNotEqual(result.returncode, 0, result)
        self.assertEqual(self.inputs.read_bytes(), prior)

    def test_preparation_output_private_outside_sources_and_atomic_document(self):
        self.canonical_checkout()
        self.assertEqual(self.prepare().returncode, 0)
        prior = self.inputs.read_bytes()
        blocker = self.root / 'blocker'
        blocker.mkdir()
        (blocker / 'keep').write_text('preserve')
        for output in (blocker, self.source / 'kits/marsh-shell/generated.json'):
            result = self.prepare('--output', output)
            self.assertNotEqual(result.returncode, 0, result)
            self.assertEqual(self.inputs.read_bytes(), prior)
            self.assertFalse(Path(str(output) + '.sources.json').exists())
        public = self.root / 'public'
        public.mkdir(mode=0o755)
        public.chmod(0o755)
        result = self.prepare('--output', public / 'map.json')
        self.assertNotEqual(result.returncode, 0, result)
        self.assertEqual(list(public.iterdir()), [])
        result = self.call('--output', self.source / 'kits/marsh-shell/new-dir/registry.json')
        self.assertNotEqual(result.returncode, 0, result)
        self.assertFalse((self.source / 'kits/marsh-shell/new-dir').exists())
        self.assertEqual(self.records(), [])
        canonical = self.source / 'packaging/dhi-notices/collected'
        canonical.chmod(0o700)  # parent mode alone must not be the refusal reason
        original = (canonical / 'README.md').read_bytes()
        result = self.prepare('--output', canonical / 'README.md')
        self.assertNotEqual(result.returncode, 0, result)
        self.assertIn('outside', result.stderr)
        self.assertEqual((canonical / 'README.md').read_bytes(), original)

    def test_publication_receipt_binds_real_bytes_args_sources_and_immutable_digest(self):
        self.canonical_checkout()
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result)
        result = self.call()
        self.assertEqual(result.returncode, 0, result)
        self.assertEqual(len(self.records()), 8)
        self.assert_canonical_bytes(self.records())
        proof_path, = self.output.parent.glob('commands.json.publication-*.json')
        proof = json.loads(proof_path.read_text())
        self.assertEqual(proof['registry'], json.loads(self.output.read_text()))
        self.assertEqual(proof['prepared_document_sha256'], 'sha256:' + hashlib.sha256(self.inputs.read_bytes()).hexdigest())
        for row in self.records()[:7]:
            descriptor = next(name for name in row['files'] if name.endswith('.yaml'))
            source = next(value for value in proof['state']['sources'].values() if value['descriptor'] == descriptor)
            actual = {name: {'sha256': 'sha256:' + digest, 'mode': row['file_modes'][name],
                            'size': row['file_sizes'][name]}
                      for name, digest in row['files'].items()}
            # Actual sizes/modes/hashes come from the recorder, not the receipt.
            digest = hashlib.sha256((json.dumps(actual, sort_keys=True, indent=2) + '\n').encode()).hexdigest()
            self.assertEqual(source['staged_tree'], 'sha256:' + digest)
        before_registry, before_proof = self.output.read_bytes(), proof_path.read_bytes()
        self.env.update(MUTATE_INPUT=str(self.source / 'packaging/image-repair/artifacts.json'), MUTATE_ON_PUSH='1')
        self.log.unlink()
        result = self.call()
        self.assertNotEqual(result.returncode, 0, result)
        self.assertIn('publication may have occurred', result.stderr)
        self.assertTrue(any(any('push=true' in arg for arg in row['args']) for row in self.records()))
        self.assertEqual(self.output.read_bytes(), before_registry)
        self.assertEqual(proof_path.read_bytes(), before_proof)

    def test_prepared_receipt_tampering_and_argument_changes_refuse_before_docker(self):
        self.canonical_checkout()
        self.assertEqual(self.prepare().returncode, 0)
        original = self.inputs.read_bytes()
        for kind in ('args', 'staged-hash', 'binding'):
            with self.subTest(kind=kind):
                document = json.loads(original)
                if kind == 'args':
                    document['inputs']['kits/marsh-pi']['args']['version'] = '0.99.0'
                elif kind == 'staged-hash':
                    document['receipt']['state']['sources']['kits/marsh-shell']['staged_tree'] = 'sha256:' + '0' * 64
                else:
                    document['receipt']['bindings'].pop(str(self.source / 'scripts/prepare-kit-inputs.py'))
                self.inputs.write_text(json.dumps(document))
                result = self.call()
                self.assertNotEqual(result.returncode, 0, result)
                self.assertEqual(self.records(), [])
                self.assertFalse(self.output.parent.exists())
        self.inputs.write_bytes(original)

    def test_umask077_source_readability_and_actual_tree_entry_bound(self):
        self.canonical_checkout()
        for tree in ('packaging/dhi-notices/collected', 'packaging/image-repair', 'kits'):
            for path in (self.source / tree).rglob('*'):
                path.chmod(0o700 if path.is_dir() else 0o600)
            (self.source / tree).chmod(0o700)
        self.assertEqual(self.prepare().returncode, 0)
        result = self.call('--validate-only')
        self.assertEqual(result.returncode, 0, result)
        self.assert_canonical_bytes(self.records())
        self.log.unlink()
        previous = self.inputs.read_bytes()
        crowded = self.source / 'packaging/image-repair/crowded'
        crowded.mkdir()
        for number in range(10001):
            (crowded / str(number)).mkdir()
        result = self.prepare()
        self.assertNotEqual(result.returncode, 0, result)
        self.assertIn('count limit', result.stderr)
        self.assertEqual(self.inputs.read_bytes(), previous)
        self.assertEqual(self.records(), [])

    def test_unproven_or_local_only_shell_reference_refused_without_docker(self):
        self.canonical_checkout()
        self.commands = self.source / 'tests/acceptance/fixture/commands.json'
        self.assertEqual(self.prepare().returncode, 0)
        prior = self.inputs.read_bytes()
        reference = self.root / 'shell-image'
        reference.write_text(PIN + '\n')
        proof = Path(str(reference) + '.build.json')
        for document in (None, {'schema': 'marsh.prepared-shell-image/v1', 'reference': PIN},
                         {'schema': 'marsh.prepared-shell-image/v1', 'reference': PIN,
                          'publication': 'local-template', 'platform': 'linux/arm64'}):
            with self.subTest(proof=document):
                if document is not None:
                    proof.write_text(json.dumps(document))
                result = self.prepare('--shell-image', reference)
                self.assertNotEqual(result.returncode, 0, result)
                self.assertEqual(self.records(), [])
                self.assertEqual(self.inputs.read_bytes(), prior)

    def test_source_or_private_context_change_during_validation_prevents_push(self):
        for kind in ('source', 'context'):
            with self.subTest(kind=kind):
                self.log.unlink(missing_ok=True)
                self.env.pop('MUTATE_INPUT', None)
                self.env.pop('MUTATE_CONTEXT', None)
                self.env['MUTATE_INPUT' if kind == 'source' else 'MUTATE_CONTEXT'] = str(self.source / 'first/payload')
                result = self.call()
                self.assertNotEqual(result.returncode, 0, result)
                self.assertTrue(all(not any('push=true' in arg for arg in row['args']) for row in self.records()))
                self.assertFalse(self.output.exists())

    def test_source_case_aliases_use_actual_inode_on_case_insensitive_volume(self):
        alias = self.source / 'FIRST'
        if not alias.exists():
            self.skipTest('case-sensitive volume; actual APFS caller remains root gate')
        self.assertTrue(os.path.samefile(alias, self.source / 'first'))
        self.commands.write_text(json.dumps({'a': 'FIRST', 'b': 'first'}))
        result = self.call()
        self.assertEqual(result.returncode, 0, result)
        self.assertEqual(len(self.records()), 2)

    def test_long_checkout_does_not_repeat_absolute_origin_per_notice_per_kit(self):
        self.canonical_checkout()
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result)
        compact_size = self.inputs.stat().st_size
        compact_path_bytes = len(os.fsencode(self.source))
        deep = self.root.joinpath(*['d' * 170] * 4)
        deep.parent.mkdir(parents=True)
        self.source.rename(deep)
        self.source = deep
        self.commands = deep / 'packaging/commands.json'
        self.publisher = deep / 'scripts/publish-kits.py'
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result)
        self.assertLess(self.inputs.stat().st_size, 4 * 1024 * 1024)
        self.assertLessEqual(self.inputs.read_bytes().count(os.fsencode(deep)), 32)
        self.assertLessEqual(self.inputs.stat().st_size - compact_size,
                             32 * (len(os.fsencode(deep)) - compact_path_bytes))
        result = self.call('--validate-only')
        self.assertEqual(result.returncode, 0, result)
        self.assert_canonical_bytes(self.records())


class StrongProvenanceCli(unittest.TestCase):
    """Fresh real tiny Cargo/build-candidate observation + controlled image peer.

    This exercises the REAL strong verifier, never a patched verifier/stale
    receipt adoption. The image digest/ARM names are controlled protocol peers,
    NOT a registry, ARM executable, or shipping candidate qualification.
    """
    def setUp(self):
        rustc = shutil.which('rustc')
        if not rustc or not subprocess.check_output([rustc, '--version'], text=True).startswith('rustc 1.95.'):
            self.skipTest('fresh observed producer fixture requires official Rust 1.95')
        acceptance = ROOT / 'tests/acceptance'
        if not (acceptance / 'test_evidence_oracles.py').is_file():
            self.skipTest('the build-receipt producer fixture (test_evidence_oracles.py) was retired')
        sys.path.insert(0, str(acceptance))
        spec = importlib.util.spec_from_file_location('publisher_observed_fixture', acceptance / 'test_evidence_oracles.py')
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        self.producer = module.BuildReceiptCallerTests('test_source_change_rejected_before_acceptance_or_smoke_effects')
        self.producer.setUp()
        self.addCleanup(self.producer.doCleanups)
        self.caller = PublisherCli('test_validate_only_has_no_push_or_registry')
        self.caller.setUp()
        self.caller._evidence_label = self.id()
        self.addCleanup(self.caller.doCleanups)
        p, c = self.producer, self.caller
        # Candidate and trusted consumer code may be in distinct worktrees; both
        # identities must be bound instead of guessing paths from the candidate.
        c.preparer = PREPARER
        for name in ('scripts/prepare-kit-inputs.py', 'scripts/publish-kits.py',
                     'scripts/build_inputs.py', 'scripts/image_observations.py',
                     'scripts/owned_process.py', 'scripts/package_observations.py',
                     'tests/acceptance/provenance.py',
                     'crates/marsh-contracts/src/command_registry_rules.json'):
            destination = p.source / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / name, destination)
        fixture = p.source / 'tests/acceptance/fixture'
        fixture.mkdir(parents=True, exist_ok=True)
        (fixture / 'fixture.yaml').write_text('schemaVersion: "3"\nkind: workload\ndockerfile: ./fixture.dockerfile\n'
                                            'args:\n  shellImage:\n    buildArg: SHELL_IMAGE\n')
        (fixture / 'fixture.dockerfile').write_text('FROM scratch\n# controlled protocol fixture, never native image qualification\n')
        c.commands = fixture / 'commands.json'
        c.commands.write_text(json.dumps({'fixture': 'tests/acceptance/fixture'}))
        c.source, c.publisher = p.source, PUBLISHER
        (p.source / 'Makefile').write_text(p.makefile.replace('build --release --locked',
                                                             'build --offline --release --locked'))
        p.environment = {**c.env, 'CARGO_HOME': str(c.root / 'cargo-home'),
                         'CARGO_BUILD_JOBS': '1',
                         'CARGO_INCREMENTAL': '0', 'CARGO_PROFILE_RELEASE_DEBUG': '0'}
        p.build()
        self.image = p.guest / 'shell-image'
        self.producer_receipt_bytes = p.receipt.read_bytes()

    def prepare(self, *extra):
        return self.caller.prepare('--source-tree', self.producer.source,
                                   '--shell-image', self.image,
                                   '--shell-build-receipt', self.producer.receipt, *extra)

    def assert_pre_effect_refusal(self, result):
        self.assertNotEqual(result.returncode, 0, result)
        self.assertEqual(self.caller.records(), [], result)
        self.assertFalse(self.caller.output.exists(), result)
        self.assertFalse(self.producer.calls.exists(), 'no image/product/stock peer allowed')
        self.assertFalse(self.producer.product_calls.exists(), 'no product execution allowed')

    def test_fresh_observed_build_public_prepare_and_publish_receipt(self):
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result)
        document = json.loads(self.caller.inputs.read_text())
        request, = document['receipt']['shell_images']
        self.assertEqual(request, {'source': 'tests/acceptance/fixture', 'source_tree': str(self.producer.source),
                                  'image_file': str(self.image), 'build_receipt': str(self.producer.receipt)})
        for name in ('scripts/image_observations.py', 'scripts/owned_process.py',
                     'scripts/package_observations.py', 'tests/acceptance/provenance.py'):
            self.assertIn(str(ROOT / name), document['receipt']['bindings'])
        result = self.caller.call()
        self.assertEqual(result.returncode, 0, result)
        self.assertEqual(len(self.caller.records()), 2)
        self.assertEqual(self.producer.receipt.read_bytes(), self.producer_receipt_bytes)
        proof, = self.caller.output.parent.glob('commands.json.publication-*.json')
        publication = json.loads(proof.read_text())
        self.assertEqual(publication['prepared_receipt']['shell_images'], [request])
        evidence = self.caller.root / 'readonly-proof-evidence'
        harness = subprocess.run([sys.executable, str(ROOT / 'tests/kits/strong_provenance_uat.py'),
                                  '--source-tree', str(self.producer.source), '--shell-image', str(self.image),
                                  '--shell-build-receipt', str(self.producer.receipt), '--evidence', str(evidence)],
                                 env=self.caller.env, capture_output=True, text=True, timeout=120)
        self.caller.observed(harness)
        self.assertEqual(harness.returncode, 0, harness)
        self.assertEqual(json.loads((evidence / 'result.json').read_text())['outcome'], 'passed')
        self.assertFalse(self.producer.calls.exists())
        self.assertFalse(self.producer.product_calls.exists())

    def test_missing_image_only_copied_aliased_and_guest_owned_proofs_refuse(self):
        c, p = self.caller, self.producer
        c.inputs.write_bytes(b'old prepared output')
        result = c.prepare('--source-tree', p.source, '--shell-image', self.image)
        self.assert_pre_effect_refusal(result)
        self.assertIn('--shell-build-receipt', result.stderr)
        copied_image = c.root / 'shell-image'
        shutil.copy2(self.image, copied_image)
        shutil.copy2(p.guest / 'shell-image.build.json', copied_image.with_name('shell-image.build.json'))
        self.assert_pre_effect_refusal(self.prepare('--shell-image', copied_image))
        link = c.root / 'alias-image'
        link.symlink_to(self.image)
        self.assert_pre_effect_refusal(self.prepare('--shell-image', link))
        link.unlink()
        os.link(self.image, link)
        try:
            self.assert_pre_effect_refusal(self.prepare('--shell-image', link))
        finally:
            link.unlink()
        receipt_in_guest = p.guest / 'copied-receipt.json'
        shutil.copy2(p.receipt, receipt_in_guest)
        try:
            self.assert_pre_effect_refusal(self.prepare('--shell-build-receipt', receipt_in_guest))
        finally:
            receipt_in_guest.unlink()
        self.assert_pre_effect_refusal(self.prepare('--source-tree', p.source / 'src/..'))
        helper_only = c.root / 'helper-only.json'
        helper_only.write_bytes((p.guest / 'shell-image.build.json').read_bytes())
        self.assert_pre_effect_refusal(self.prepare('--shell-build-receipt', helper_only))
        self.assertEqual(c.inputs.read_bytes(), b'old prepared output')

    def test_outputs_cannot_overwrite_observed_proof_inputs_or_exports(self):
        self.assertEqual(self.prepare().returncode, 0)
        c, p = self.caller, self.producer
        destinations = (p.receipt, self.image, p.guest / 'shell-image.build.json',
                        p.guest / 'commands.json', p.target / 'release/marsh')
        original = {path: path.read_bytes() for path in destinations}
        for destination in destinations:
            with self.subTest(output=str(destination)):
                self.assert_pre_effect_refusal(self.prepare('--output', destination))
                self.assert_pre_effect_refusal(c.call('--output', destination))
                self.assertEqual({path: path.read_bytes() for path in original}, original)

    def test_stale_export_or_candidate_rejected_even_when_receipt_bytes_unchanged(self):
        self.assertEqual(self.prepare().returncode, 0)
        c, p = self.caller, self.producer
        original = c.inputs.read_bytes()
        for path in (p.guest / 'marsh-local-linux-arm64', p.target / 'release/marsh',
                     p.source / 'src/main.rs', p.guest / 'licenses/marsh/LICENSE'):
            before = path.read_bytes()
            with self.subTest(path=str(path)):
                path.write_bytes(before + b'\nchanged after prepared receipt\n')
                self.assert_pre_effect_refusal(c.call())
                path.write_bytes(before)
                self.assertEqual(p.receipt.read_bytes(), self.producer_receipt_bytes)
                self.assertEqual(c.inputs.read_bytes(), original)
        document = json.loads(original)
        document['receipt'].pop('shell_images')
        c.inputs.write_text(json.dumps(document))
        self.assert_pre_effect_refusal(c.call())

    def test_semantic_revalidation_before_push_and_after_publication(self):
        self.assertEqual(self.prepare().returncode, 0)
        c, p = self.caller, self.producer
        artifact = p.guest / 'marsh-local-linux-arm64'
        original = artifact.read_bytes()
        for phase in ('cacheonly', 'push'):
            with self.subTest(phase=phase):
                c.log.unlink(missing_ok=True)
                c.output.parent.mkdir(mode=0o700, exist_ok=True)
                c.output.write_bytes(b'previous registry')
                c.env['MUTATE_INPUT'] = str(artifact)
                if phase == 'push':
                    c.env['MUTATE_ON_PUSH'] = '1'
                else:
                    c.env.pop('MUTATE_ON_PUSH', None)
                result = c.call()
                self.assertNotEqual(result.returncode, 0, result)
                pushes = [row for row in c.records() if any('push=true' in arg for arg in row['args'])]
                self.assertEqual(len(pushes), 1 if phase == 'push' else 0)
                if phase == 'push':
                    self.assertIn('publication may have occurred', result.stderr)
                self.assertEqual(c.output.read_bytes(), b'previous registry')
                self.assertEqual(p.receipt.read_bytes(), self.producer_receipt_bytes)
                artifact.write_bytes(original)


if __name__ == "__main__":
    unittest.main()
