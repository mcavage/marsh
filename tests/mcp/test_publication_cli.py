#!/usr/bin/env python3
"""Real host CLI publication transactions against a disposable fake SBX."""

import hashlib
import json
import sys
import os
import pwd
from pathlib import Path
import subprocess
import socket
import struct
import threading
import tempfile
import time
import unittest
import uuid


ROOT = Path(__file__).resolve().parents[2]
MARSH = Path(os.environ["MARSH_BINARY"]).resolve(strict=True)
FAKE_SBX = r'''#!/usr/bin/env python3
import json, os, pathlib, sys, time
root = pathlib.Path(os.environ["FAKE_SBX_ROOT"])
args = sys.argv[1:]
registry = root / ("acp-registry.json" if args[2].startswith("marsh-acp-") else "registry.json")
with (root / "calls.log").open("a") as log:
    log.write(" ".join(args[:3]) + "\n")
if args[:2] == ['inspect', '--json']:
    if args[2] == 'missing':
        sys.stderr.write('sandbox not found\n'); sys.exit(7)
    print(json.dumps({'name': args[2], 'state': 'running'}))
elif args[:2] == ["mcp", "inspect"]:
    if registry.exists():
        print(registry.read_text())
    else:
        sys.stderr.write(f'error: mcp server "{args[2]}" not found: mcp server not found\n  try: sbx mcp ls\n')
        sys.exit(1)
elif args[:2] == ["mcp", "add"]:
    if (root / "foreign-on-add").exists():
        (root / "foreign-on-add").unlink()
        registry.write_text(json.dumps({"name": args[2], "type": "local",
                                        "resolved_command": "/bin/foreign",
                                        "command": ["/bin/foreign"]}))
        sys.stderr.write("registration raced with another owner\n")
        sys.exit(2)
    if registry.exists():
        sys.stderr.write("already registered\n")
        sys.exit(2)
    command = args[args.index("--command") + 1]
    server_args = args[args.index("--args") + 1].split(",")
    registry.write_text(json.dumps({"name": args[2], "type": "local",
                                    "resolved_command": command,
                                    "command": [command, *server_args]}))
elif args[:2] == ["mcp", "load"]:
    sandbox = args[args.index("--sandbox") + 1]
    if sandbox in ("missing", "load-fails"):
        sys.stderr.write("sandbox load failed\n")
        sys.exit(7)
    if sandbox == "hold":
        (root / "load-entered").touch()
        for _ in range(400):
            if (root / "release-load").exists():
                break
            time.sleep(0.025)
        else:
            sys.exit(8)
elif args[:2] == ["mcp", "rm"]:
    if (root / "fail-rm-once").exists():
        (root / "fail-rm-once").unlink()
        sys.stderr.write("temporary remove failure\n")
        sys.exit(9)
    if registry.exists():
        registry.unlink()
    else:
        sys.stderr.write(f'error: MCP server "{args[-1]}" not found\n')
        sys.exit(1)
else:
    sys.exit(64)
'''


class TypedHostPeer:
    """Controlled authenticated-daemon peer for the real private host CLI.

    This only speaks the framed protocol. The CLI owns all publication validation
    and stock effects. Actual guest/daemon admission is covered by the Rust relay
    journey; this peer must not be cited as that authority proof.
    """
    def __init__(self, argv, cwd, env, context, text=False):
        self.text = text
        self.cwd = cwd
        self.env = env
        self.outcome = None
        self.failure = None
        parent, child = socket.socketpair()
        parent.settimeout(90)
        self.channel = parent
        self.process = subprocess.Popen(argv, cwd=cwd, env={**env, 'MARSH_PUBLICATION_CHANNEL': '1'},
            stdin=child, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        child.close()
        self.send(context)
        self.reader = threading.Thread(target=self.drive, daemon=True)
        self.reader.start()

    def __getattr__(self, key):
        return getattr(self.process, key)

    def send(self, value):
        data = json.dumps(value).encode()
        self.channel.sendall(struct.pack('!I', len(data)) + data)

    def receive(self, length):
        data = b''
        while len(data) < length:
            part = self.channel.recv(length - len(data))
            if not part:
                raise AssertionError('host closed without a typed publication result')
            data += part
        return data

    def drive(self):
        try:
            while True:
                size, = struct.unpack('!I', self.receive(4))
                if size > 1048576:
                    raise AssertionError('oversized publication frame')
                event = json.loads(self.receive(size))
                if event['type'] in ('begin_commit', 'begin_rollback'):
                    self.send({'Ok': None})
                elif event['type'] == 'run_stock':
                    result = subprocess.run([self.env['MARSH_SBX'], *event['data']['arguments']],
                        cwd=self.cwd, env=self.env, capture_output=True, timeout=65, start_new_session=True)
                    self.send({'Ok': {'exit_code': result.returncode, 'stdout': list(result.stdout), 'stderr': list(result.stderr)}})
                elif event['type'] == 'prepare_kit':
                    self.send({'Err': 'controlled host peer has no Kit backend'})
                elif event['type'] == 'complete':
                    self.outcome = event['data']
                    break
                else:
                    raise AssertionError(event)
        except Exception as error:
            self.failure = error
        finally:
            self.channel.close()

    def communicate(self, timeout=None):
        output, error = self.process.communicate(timeout=timeout)
        self.reader.join(timeout=2)
        if self.failure:
            raise self.failure
        if self.reader.is_alive() or self.outcome is None:
            raise AssertionError('missing typed publication outcome')
        # Presentation of a typed commit, not stdout parsing. Verify that no
        # control sentinel or human output was substituted for the private wire.
        if output:
            raise AssertionError(f'private host transaction wrote stdout: {output!r}')
        if self.outcome['status'] == 'committed':
            output = (self.outcome['message'] + '\n').encode()
        return (output.decode(), error.decode()) if self.text else (output, error)


class PublicationCliTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.binary_identity = {}
        for name in ("marsh", "marsh-mcp", "marshd"):
            path = MARSH.with_name(name)
            if not path.is_file() or not os.access(path, os.X_OK):
                raise AssertionError(f"build ALL host binaries before this test: missing/nonexecutable {path}")
            cls.binary_identity[str(path)] = hashlib.sha256(path.read_bytes()).hexdigest()
        print("MCP CLI binaries: " + json.dumps(cls.binary_identity, sort_keys=True), file=sys.stderr)
        if evidence := os.environ.get("MARSH_MCP_TEST_EVIDENCE"):
            directory = Path(evidence)
            directory.mkdir(parents=True, exist_ok=True)
            (directory / "publication-cli-binaries.json").write_text(json.dumps(cls.binary_identity, indent=2) + "\n")

    @classmethod
    def tearDownClass(cls):
        for path, sha in cls.binary_identity.items():
            if hashlib.sha256(Path(path).read_bytes()).hexdigest() != sha:
                raise AssertionError(f"binary changed during MCP CLI suite: {path}")

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="marsh-publish-cli-")
        self.addCleanup(self.tmp.cleanup)
        # marsh admits only symlink-free project paths (macOS /var and /tmp are
        # symlinks), so projects live under the canonical temp root. The
        # selected home deliberately keeps the as-created alias: the daemon
        # accepts a home under a symlinked ancestor and compares canonically.
        self.root = Path(os.path.realpath(self.tmp.name))
        self.project = self.root / "project"
        self.home = Path(self.tmp.name) / "selected-home"
        self.host_home = self.root / "host-home"
        for path in (self.project, self.home, self.host_home):
            path.mkdir(mode=0o700)
        self.sbx = self.root / "sbx"
        self.sbx.write_text(FAKE_SBX)
        self.sbx.chmod(0o700)
        self.env = os.environ.copy()
        username = pwd.getpwuid(os.getuid()).pw_name
        self.env.update({"HOME": str(self.host_home), "MARSH_HOME": str(self.home),
                         "USER": username, "LOGNAME": username,
                         "MARSH_SBX": str(self.sbx), "FAKE_SBX_ROOT": str(self.root)})
        for name in ("MARSH_DAEMON_SOCKET", "MARSH_DAEMON_TOKEN"):
            self.env.pop(name, None)

    def spawn(self, *args, protocol='mcp', text=False, context_changes=None):
        argv = [str(MARSH), protocol, *args]
        if args[0] in ('publish', 'host-publish'):
            backing = self.home / 'home'
            backing.mkdir(mode=0o700, exist_ok=True)
            metadata = self.project.stat()
            context = {'session': {'session_id': 'controlled-attached-host-peer', 'username': self.env['USER'],
                'uid': os.geteuid(), 'gid': os.getegid(), 'launch_directory': str(self.project),
                'guest_home': str(self.host_home), 'home_backing': str(backing), 'ephemeral_home': False,
                'terminal': False, 'terminal_size': None}, 'project_identity': [metadata.st_dev, metadata.st_ino],
                'kind': protocol, 'name': args[1], 'operation': 'publish', 'kit': None,
                'scope_admitted': False,
                'sandbox': args[args.index('--sandbox')+1] if '--sandbox' in args else None,
                'agent_session_id': args[2] if protocol == 'acp' else None,
                'generation': args[3] if protocol == 'acp' else None}
            if context_changes:
                context_changes(context)
            return TypedHostPeer(argv, self.project, self.env, context, text=text)
        return subprocess.Popen(argv, cwd=self.project, env=self.env, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=text, start_new_session=True)

    def command(self, *args):
        child = self.spawn(*args, text=True)
        output, error = child.communicate(timeout=90)
        result = subprocess.CompletedProcess(child.args, child.returncode, output, error)
        result.publication_outcome = getattr(child, 'outcome', None)
        return result

    def declaration(self, name):
        matches = list(self.host_home.rglob(f"{name}.json"))
        self.assertEqual(len(matches), 1)
        return matches[0]

    def test_missing_names_have_actionable_errors_and_no_per_name_files(self):
        for index in range(24):
            result = self.command("load", f"missing_{index}", "--sandbox", "target")
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(f"no publication named missing_{index}", result.stderr)
            self.assertIn("open `marsh`", result.stderr)
            self.assertIn("mcp publish", result.stderr)
        self.assertFalse((self.root / "calls.log").exists())
        self.assertEqual(list(self.host_home.rglob("*.lock")), [])
        self.assertEqual(list(self.host_home.rglob("published-acp")), [])
        self.assertEqual(self.command("publish", "stable", "--", "cat").returncode, 0)
        for index in range(24):
            self.command("load", f"missing_{index}", "--sandbox", "target")
        self.assertEqual(len(list(self.host_home.rglob("*.lock"))), 1)

    def test_supported_load_and_revoke_hints(self):
        published = self.command("publish", "hints", "--", "cat")
        self.assertEqual(published.returncode, 0, published.stderr)
        self.assertIn("mcp load hints --sandbox SANDBOX", published.stdout)
        self.assertIn("mcp load hints --kit KIT", published.stdout)
        self.assertNotIn("sbx mcp load", published.stdout)
        self.assertIn("Revoke: mcp unpublish hints", published.stdout)
        loaded = self.command("load", "hints", "--sandbox", "target")
        self.assertIn("Revoke: marsh mcp unpublish hints", loaded.stdout)
        (self.root / "registry.json").unlink()
        missing = self.command("load", "hints", "--sandbox", "target")
        self.assertIn("Republish", missing.stderr)

    def test_orphan_other_protocol_registration_blocks_load(self):
        self.assertEqual(self.command("publish", "same", "--", "cat").returncode, 0)
        (self.root / "acp-registry.json").write_text(json.dumps({"command": ["/other/publisher"]}))
        result = self.command("load", "same", "--sandbox", "target")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("other protocol", result.stderr)
        self.assertFalse(any(line.startswith("mcp load") for line in (self.root / "calls.log").read_text().splitlines()))

    def test_cross_protocol_host_transactions_share_scope_admission(self):
        # Actual host CLI processes, controlled stock service. The ACP host
        # dispatch context models the host daemon (not a Kit credential).
        first = self.spawn('publish', 'race', '--sandbox', 'hold', '--', 'cat')
        second = None
        try:
            deadline = time.monotonic() + 10
            while not (self.root / "load-entered").exists():
                self.assertIsNone(first.poll())
                self.assertLess(time.monotonic(), deadline)
                time.sleep(0.025)
            second = self.spawn('host-publish', 'race', str(uuid.uuid4()), str(uuid.uuid4()), protocol='acp')
            time.sleep(0.25)
            self.assertIsNone(second.poll(), "ACP bypassed MCP's scope admission lock")
            (self.root / "release-load").touch()
            out, err = first.communicate(timeout=15)
            self.assertEqual(first.returncode, 0, err)
            out, err = second.communicate(timeout=15)
            self.assertNotEqual(second.returncode, 0)
            self.assertIn(b"other protocol", err)
            self.assertFalse((self.root / "acp-registry.json").exists())
        finally:
            (self.root / "release-load").touch()
            for child in (first, second):
                if child is not None and child.poll() is None:
                    child.kill()
                    child.communicate()

    def test_host_direct_publish_is_rejected_before_any_effect(self):
        before = sorted(str(p.relative_to(self.root)) for p in self.root.rglob('*'))
        denied = subprocess.run([str(MARSH), 'mcp', 'publish', 'host-denied', '--', 'cat'],
                                cwd=self.project, env=self.env, capture_output=True,
                                text=True, timeout=20, start_new_session=True)
        self.assertNotEqual(denied.returncode, 0, denied.stdout)
        self.assertIn('attached', denied.stderr)
        self.assertEqual(sorted(str(p.relative_to(self.root)) for p in self.root.rglob('*')), before)
        self.assertFalse((self.root / 'calls.log').exists())

    def test_private_open_rejections_are_typed_and_effect_free(self):
        changes = [
            lambda context: context.update(name='different-name'),
            lambda context: context['session'].update(home_backing=str(self.root / 'foreign-home')),
            lambda context: context.update(project_identity=[context['project_identity'][0], context['project_identity'][1] + 1]),
        ]
        for change in changes:
            before = sorted(str(p.relative_to(self.host_home)) for p in self.host_home.rglob('*'))
            child = self.spawn('publish', 'context-bound', '--', 'cat', text=True, context_changes=change)
            _, error = child.communicate(timeout=20)
            self.assertNotEqual(child.returncode, 0, error)
            self.assertEqual(child.outcome['status'], 'rejected_before_effect')
            self.assertEqual(sorted(str(p.relative_to(self.host_home)) for p in self.host_home.rglob('*')), before)
            self.assertFalse((self.root / 'calls.log').exists())

    def test_forged_channel_environment_is_not_a_socket_or_grant(self):
        env = {**self.env, 'MARSH_PUBLICATION_CHANNEL': '1'}
        result = subprocess.run([str(MARSH), 'mcp', 'publish', 'forged', '--', 'cat'], cwd=self.project,
            env=env, stdin=subprocess.DEVNULL, capture_output=True, timeout=20, start_new_session=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b'private channel', result.stderr)
        self.assertFalse((self.root / 'calls.log').exists())
        self.assertEqual(list(self.host_home.rglob('*')), [])

    def test_missing_sandbox_is_rejected_before_registration(self):
        result = self.command('publish', 'missing-target', '--sandbox', 'missing', '--', 'cat')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.publication_outcome['status'], 'rejected_before_effect')
        calls = (self.root / 'calls.log').read_text().splitlines()
        self.assertFalse(any(line.startswith(('mcp add', 'mcp rm', 'mcp load')) for line in calls), calls)
        self.assertFalse((self.root / 'registry.json').exists())
        self.assertEqual(list(self.host_home.rglob('missing-target.json')), [])

    def test_dot_publication_name_uses_the_same_scope_and_validation(self):
        result = self.command('publish', 'team.review', '--', 'cat')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.command('load', 'team.review', '--sandbox', 'target').returncode, 0)
        self.assertEqual(self.command('unpublish', 'team.review').returncode, 0)

    def test_load_preserves_generation(self):
        published = self.command("publish", "stable", "--", "cat")
        self.assertEqual(published.returncode, 0, published.stderr)
        self.assertEqual(published.publication_outcome['status'], 'committed')
        declaration = self.declaration("stable")
        before = declaration.read_bytes()
        registry = self.root / "registry.json"
        registered = registry.read_bytes()
        loaded = self.command("load", "stable", "--sandbox", "second-target")
        self.assertEqual(loaded.returncode, 0, loaded.stderr)
        self.assertIn(json.loads(registered)["name"], loaded.stdout)
        self.assertIn("second-target", loaded.stdout)
        self.assertEqual(declaration.read_bytes(), before)
        self.assertEqual(registry.read_bytes(), registered)
        calls = (self.root / "calls.log").read_text().splitlines()
        self.assertEqual(sum(line.startswith("mcp add") for line in calls), 1)
        self.assertFalse(any(line.startswith("mcp rm") for line in calls))
        failed = self.command("load", "stable", "--sandbox", "missing")
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn("sandbox `missing` is unavailable", failed.stderr)
        self.assertEqual(declaration.read_bytes(), before)
        self.assertEqual(registry.read_bytes(), registered)
        self.assertEqual(self.command("unpublish", "stable").returncode, 0)
        effects = (self.root / "calls.log").read_bytes()
        self.assertNotEqual(self.command("load", "stable", "--sandbox", "second-target").returncode, 0)
        self.assertEqual((self.root / "calls.log").read_bytes(), effects)

    def test_load_rejects_stale_collision_and_project_replacement_without_mutation(self):
        self.assertEqual(self.command("publish", "pinned", "--", "cat").returncode, 0)
        path = self.declaration("pinned")
        original = path.read_bytes()
        registry = self.root / "registry.json"
        registered = registry.read_bytes()
        for change in ("generation", "command"):
            value = json.loads(registered)
            if change == "generation":
                value["command"][-1] = str(uuid.uuid4())
            else:
                value["command"][0] = "/bin/foreign"
            registry.write_text(json.dumps(value))
            loaded = self.command("load", "pinned", "--sandbox", "target")
            self.assertNotEqual(loaded.returncode, 0)
            self.assertIn("does not match", loaded.stderr)
            self.assertEqual(json.loads(registry.read_text()), value)
            self.assertEqual(path.read_bytes(), original)
        registry.write_bytes(registered)
        other = Path(str(path).replace("/published-mcp/", "/published-acp/"))
        other.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
        other.write_text("{}")
        effects = (self.root / "calls.log").read_bytes()
        self.assertIn("other protocol", self.command("load", "pinned", "--sandbox", "target").stderr)
        self.assertEqual((self.root / "calls.log").read_bytes(), effects)
        other.unlink()
        self.project.rename(self.root / "old-project")
        self.project.mkdir(mode=0o700)
        self.assertIn("project identity", self.command("load", "pinned", "--sandbox", "target").stderr)
        self.assertEqual((self.root / "calls.log").read_bytes(), effects)

    def test_existing_load_fences_concurrent_unpublish(self):
        self.existing_load_fences(["unpublish", "locked"])

    def test_existing_load_fences_concurrent_republish(self):
        self.existing_load_fences(["publish", "locked", "--", "printf replacement"])

    def existing_load_fences(self, mutation):
        self.assertEqual(self.command("publish", "locked", "--", "cat").returncode, 0)
        first = self.spawn('load', 'locked', '--sandbox', 'hold')
        second = None
        try:
            deadline = time.monotonic() + 10
            while not (self.root / "load-entered").exists() and time.monotonic() < deadline:
                self.assertIsNone(first.poll(), "load exited before stock effect")
                time.sleep(0.025)
            self.assertTrue((self.root / "load-entered").exists())
            second = self.spawn(*mutation)
            time.sleep(0.25)
            self.assertIsNone(second.poll(), "publication mutation bypassed existing load lock")
            self.assertTrue(self.declaration("locked").is_file())
            (self.root / "release-load").touch()
            self.assertEqual(first.communicate(timeout=15)[1], b"")
            self.assertEqual(first.returncode, 0)
            self.assertEqual(second.communicate(timeout=15)[1], b"")
            self.assertEqual(second.returncode, 0)
        finally:
            (self.root / "release-load").touch()
            for process in (first, second):
                if process is not None and process.poll() is None:
                    process.kill()
                    process.communicate()

    def test_missing_declaration_recovers_only_exact_registration(self):
        self.assertEqual(self.command("publish", "first", "--", "cat").returncode, 0)
        declaration = self.declaration("first")
        (self.root / "fail-rm-once").touch()
        self.assertNotEqual(self.command("unpublish", "first").returncode, 0)
        self.assertFalse(declaration.exists())
        self.assertTrue((self.root / "registry.json").exists())
        self.assertEqual(self.command("unpublish", "first").returncode, 0)
        self.assertFalse((self.root / "registry.json").exists())

        (self.root / "fail-rm-once").touch()
        failed = self.command("publish", "rollback", "--sandbox", "load-fails", "--", "cat")
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn("sandbox load failed", failed.stderr)
        self.assertIn("rollback failed", failed.stderr)
        self.assertTrue((self.root / "registry.json").exists())
        self.assertEqual(self.command("unpublish", "rollback").returncode, 0)
        self.assertFalse((self.root / "registry.json").exists())

        self.assertEqual(self.command("publish", "foreign", "--", "cat").returncode, 0)
        declaration = self.declaration("foreign")
        registry = self.root / "registry.json"
        content = json.loads(registry.read_text())
        content["command"] = ["/bin/foreign"]
        registry.write_text(json.dumps(content))
        result = self.command("unpublish", "foreign")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not match", result.stderr)
        self.assertTrue(declaration.exists())
        self.assertEqual(json.loads(registry.read_text()), content)
        declaration.unlink()
        self.assertIn("does not match", self.command("unpublish", "foreign").stderr)
        self.assertEqual(json.loads(registry.read_text()), content)

    def test_publication_lock_rejects_unsafe_file(self):
        self.assertEqual(self.command("publish", "safe", "--", "cat").returncode, 0)
        declaration = self.declaration("safe")
        lock = declaration.parents[2] / "publication-locks" / (declaration.parent.name + ".lock")
        lock.chmod(0o644)
        self.assertIn("owner-only real file", self.command("unpublish", "safe").stderr)
        self.assertTrue(declaration.exists())
        lock.unlink()
        sentinel = self.root / "sentinel"
        sentinel.write_text("unchanged")
        lock.symlink_to(sentinel)
        self.assertIn("cannot open MCP publication lock", self.command("publish", "safe", "--", "false").stderr)
        self.assertEqual(sentinel.read_text(), "unchanged")
        self.assertEqual(json.loads(declaration.read_text())["pipeline"], "cat")

    def test_failed_add_keeps_foreign_same_name_registration(self):
        (self.root / "foreign-on-add").touch()
        failed = self.command("publish", "raced", "--", "cat")
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn("rollback failed", failed.stderr)
        self.assertEqual(failed.publication_outcome['status'], 'uncertain')
        self.assertEqual(json.loads((self.root / "registry.json").read_text())["command"], ["/bin/foreign"])
        self.assertEqual(list(self.host_home.rglob("raced.json")), [])

    def test_interrupted_republish_and_legacy_registration_recover_exact_scope(self):
        self.assertEqual(self.command("publish", "interrupted", "--", "cat").returncode, 0)
        declaration = self.declaration("interrupted")
        document = json.loads(declaration.read_text())
        document["publication_generation"] = str(uuid.uuid4())
        declaration.write_text(json.dumps(document))
        self.assertEqual(self.command("publish", "interrupted", "--", "printf updated").returncode, 0)
        document = json.loads(declaration.read_text())
        registered = json.loads((self.root / "registry.json").read_text())
        self.assertEqual(registered["command"][-2:], ["--expected-generation", document["publication_generation"]])
        document["publication_generation"] = str(uuid.uuid4())
        declaration.write_text(json.dumps(document))
        self.assertEqual(self.command("unpublish", "interrupted").returncode, 0)
        self.assertFalse((self.root / "registry.json").exists())

        self.assertEqual(self.command("publish", "legacy", "--", "cat").returncode, 0)
        registry = self.root / "registry.json"
        legacy = json.loads(registry.read_text())
        legacy["command"] = legacy["command"][:-2]
        registry.write_text(json.dumps(legacy))
        self.assertEqual(self.command("unpublish", "legacy").returncode, 0)
        self.assertFalse(registry.exists())

    def test_concurrent_host_publish_waits_through_load(self):
        first = self.spawn('publish', 'shared', '--sandbox', 'hold', '--', 'printf first', text=True)
        try:
            deadline = time.monotonic() + 15
            while not (self.root / "load-entered").exists() and time.monotonic() < deadline:
                self.assertIsNone(first.poll(), "first publisher exited before load")
                time.sleep(0.025)
            self.assertTrue((self.root / "load-entered").exists(), "first publisher did not reach load")
            second = self.spawn('publish', 'shared', '--', 'printf second', text=True)
            try:
                time.sleep(5)
                self.assertIsNone(second.poll(), "second publisher bypassed the first lock")
                self.assertEqual(json.loads(self.declaration("shared").read_text())["pipeline"], "printf first")
                calls = (self.root / "calls.log").read_text().splitlines()
                self.assertEqual(sum(line.startswith("mcp inspect") for line in calls), 2)
                (self.root / "release-load").touch()
                self.assertEqual(first.communicate(timeout=15)[0].count("Published MCP tool: shared"), 1)
                self.assertEqual(first.returncode, 0)
                self.assertEqual(second.communicate(timeout=15)[0].count("Published MCP tool: shared"), 1)
                self.assertEqual(second.returncode, 0)
                self.assertEqual(json.loads(self.declaration("shared").read_text())["pipeline"], "printf second")
                self.assertTrue((self.root / "registry.json").exists())
                self.assertEqual(sum(line.startswith("mcp add") for line in (self.root / "calls.log").read_text().splitlines()), 2)
            finally:
                if second.poll() is None:
                    second.kill()
                    second.communicate()
        finally:
            (self.root / "release-load").touch()
            if first.poll() is None:
                first.kill()
                first.communicate()


if __name__ == "__main__":
    unittest.main()
