#!/usr/bin/env python3
"""Public image-helper/Make callers with controlled Docker/SBX, not image qualification."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[2]

# This is an independent tiny Docker/OCI archive producer, not the verifier.
PEER = r'''
import hashlib,io,json,pathlib,sys,tarfile
root=pathlib.Path(__file__).parent; args=sys.argv[1:]
with (root/'calls.jsonl').open('a') as log: log.write(json.dumps([pathlib.Path(__file__).name,*args])+'\n')
if args in (['--version'],['version'],['buildx','version']): print('controlled peer v1');sys.exit()
def sha(data):return 'sha256:'+hashlib.sha256(data).hexdigest()
layer=b'controlled layer bytes'
config=json.dumps({'os':'linux','architecture':'arm64','rootfs':{'type':'layers','diff_ids':[sha(layer)]}},sort_keys=True).encode()
manifest=json.dumps({'schemaVersion':2,'config':{'digest':sha(config),'size':len(config)},'layers':[{'digest':sha(layer),'size':len(layer)}]},sort_keys=True).encode()
if args[:2]==['buildx','build']:
 context=pathlib.Path(args[-1]); inventory={str(p.relative_to(context)):{'mode':p.stat().st_mode&0o777,'sha256':sha(p.read_bytes())} for p in context.rglob('*') if p.is_file()}
 (root/'context.json').write_text(json.dumps(inventory))
 if (root/'fail-build').exists():sys.exit(42)
 if (root/'mutate-source').exists():
  p=pathlib.Path((root/'mutate-source').read_text());p.write_bytes(p.read_bytes()+b'changed during build')
 pathlib.Path(args[args.index('--metadata-file')+1]).write_text(json.dumps({'containerimage.digest':sha(manifest)}))
elif args[:1]==['save']:
 ref=args[-1]; payloads={'blobs/sha256/'+sha(config)[7:]:config,'blobs/sha256/'+sha(layer)[7:]:layer,'blobs/sha256/'+sha(manifest)[7:]:manifest}
 tags=[ref]
 if ref.startswith('docker.io/library/') and (root/'save-tag-mode').exists():
  mode=(root/'save-tag-mode').read_text()
  if mode=='familiar':tags=[ref.removeprefix('docker.io/library/')]
  elif mode=='library':tags=[ref.removeprefix('docker.io/')]
  elif mode=='foreign':tags=[ref.replace('docker.io/','foreign.invalid/',1)]
  elif mode=='wrong-tag':tags=[ref[:-1]+('0' if ref[-1]!='0' else '1')]
  elif mode=='extra':tags=[ref,'foreign.invalid/other:latest']
  elif mode=='empty':tags=[]
 payloads['manifest.json']=json.dumps([{'Config':'blobs/sha256/'+sha(config)[7:],'Layers':['blobs/sha256/'+sha(layer)[7:]],'RepoTags':tags}]).encode()
 payloads['index.json']=json.dumps({'manifests':[{'digest':sha(manifest),'size':len(manifest)}]}).encode()
 with tarfile.open(args[args.index('--output')+1],'w') as tar:
  for name,data in payloads.items():
   member=tarfile.TarInfo(name);member.size=len(data);tar.addfile(member,io.BytesIO(data))
elif args[:1]==['tag'] or args[:2]==['image','rm']:pass
elif args[:2]==['template','load']:
 with tarfile.open(args[2]) as tar: name=json.load(tar.extractfile('manifest.json'))[0]['RepoTags'][0]
 if '/' not in name:name='docker.io/library/'+name
 elif name.startswith('library/'):name='docker.io/'+name
 repository,tag=name.rsplit(':',1)
 row={'repository':repository,'tag':tag,'id':sha(manifest)[7:19],'full_id':sha(manifest)}
 (root/'template.json').write_text(json.dumps(row))
 all_images=json.loads((root/'templates.json').read_text()) if (root/'templates.json').exists() else {}
 all_images[name]=row;(root/'templates.json').write_text(json.dumps(all_images))
elif args==['template','ls','--json']:
 rows=list(json.loads((root/'templates.json').read_text()).values())
 print(json.dumps(rows if (root/'bare-template-list').exists() else {'images':rows}))
else:sys.exit(99)
'''


def sdk_peer(root, *, socket_path=None):
    """A real owned Unix HTTP caller peer, never the user's stock daemon."""
    directory = None
    if socket_path is None:
        directory = tempfile.TemporaryDirectory(prefix='marsh-sdk-peer-', dir='/private/tmp' if sys.platform == 'darwin' else '/tmp')
        socket_path = Path(directory.name).resolve() / 'sdk.sock'
    else:
        socket_path.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
        if socket_path.exists() or socket_path.is_symlink():
            raise ValueError('SDK peer socket path already exists')
    script = r'''
import http.server,json,pathlib,socketserver,sys,urllib.parse
root=pathlib.Path(sys.argv[1])
class Handler(http.server.BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def do_GET(self):
  with (root/'sdk-calls.jsonl').open('a') as out:out.write(json.dumps(self.path)+'\n')
  query=urllib.parse.urlsplit(self.path)
  ref=urllib.parse.parse_qs(query.query).get('name',[])
  images=json.loads((root/'templates.json').read_text())
  template=images.get(ref[0]) if len(ref)==1 else None
  if query.path!='/docker/images/inspect' or template is None:
   self.send_error(404);return
  identity=(root/'sdk-override').read_text() if (root/'sdk-override').exists() else template['full_id']
  data=json.dumps({'id':identity}).encode()
  self.send_response(200);self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
with socketserver.UnixStreamServer(sys.argv[2],Handler) as server:server.serve_forever()
'''
    process = subprocess.Popen([sys.executable, '-c', script, str(root), str(socket_path)],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    try:
        deadline = time.monotonic() + 5
        while not socket_path.exists():
            if process.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError('owned SDK peer failed to start')
            time.sleep(.01)
    except BaseException:
        if process.poll() is None:os.killpg(process.pid, signal.SIGKILL);process.wait(timeout=5)
        if directory is not None:directory.cleanup()
        raise
    def close():
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            try:process.wait(timeout=5)
            except subprocess.TimeoutExpired:os.killpg(process.pid, signal.SIGKILL);process.wait(timeout=5)
        if directory is not None:directory.cleanup()
        else:socket_path.unlink(missing_ok=True)
    return 'unix://' + str(socket_path), close


class ImageCallerTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="marsh-image-caller-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.source = self.root / "source"
        (self.source / "scripts").mkdir(parents=True)
        for name in ("prepare-shell-image.py", "image_observations.py", "build_inputs.py", "owned_process.py", "stock_sdk.py"):
            shutil.copy2(ROOT / "scripts" / name, self.source / "scripts" / name)
        for name in ("shell",):
            directory = self.source / "packaging" / name
            directory.mkdir(parents=True)
            (directory / "Dockerfile").write_text("FROM controlled@sha256:" + "a" * 64 + "\n")
        (self.source / "packaging/shell/pi").mkdir()
        (self.source / "packaging/shell/pi/package.json").write_text("{}\n")
        (self.source / "packaging/shell-image").write_text("example/base@sha256:" + "b" * 64 + "\n")
        for name in ("image-repair", "dhi-notices/collected"):
            directory = self.source / "packaging" / name
            directory.mkdir(parents=True)
            (directory / "NOTICE").write_text("controlled notice\n")
        for name in ("docker", "sbx"):
            peer = self.root / name
            peer.write_text(f"#!{sys.executable}\n" + PEER)
            peer.chmod(0o700)
        self.output = self.root / "output/shell-image"
        address, close = sdk_peer(self.root)
        self.addCleanup(close)
        self.env = {**os.environ, "MARSH_SBX": str(self.root / "sbx"), 'DOCKER_SANDBOXES_API': address}

    def invoke(self, *extra, script="prepare-shell-image.py"):
        return subprocess.run([sys.executable, str(self.source / "scripts" / script),
                               "--docker", str(self.root / "docker"), "--sbx", str(self.root / "sbx"),
                               "--output", str(self.output), *extra], cwd=self.source, env=self.env,
                              capture_output=True, text=True, timeout=30, start_new_session=True)

    def calls(self):
        path = self.root / "calls.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def test_public_make_local_default_imports_exact_archive_without_push(self):
        shutil.copy2(ROOT / "Makefile", self.source / "Makefile")
        result = subprocess.run(["make", "prepare-shell-image", f"DOCKER={self.root / 'docker'}",
                                 f"PYTHON={sys.executable}", f"GUEST_ARTIFACTS={self.output.parent}"],
                                cwd=self.source, env=self.env, capture_output=True, text=True,
                                timeout=30, start_new_session=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        proof = json.loads(self.output.with_name("shell-image.build.json").read_text())
        self.assertEqual(proof["publication"], "local-template")
        self.assertEqual(proof["publication_effects"], [])
        self.assertEqual(proof["reference"], self.output.read_text().strip())
        self.assertEqual(proof["local_image"]["archive"]["platform"], "linux/arm64")
        self.assertTrue(any(row[1:3] == ["template", "load"] for row in self.calls()))
        self.assertFalse(any("push=true" in word for row in self.calls() for word in row))
        self.assertFalse(any(row[1:2] in (["create"], ["run"]) for row in self.calls()))

    def test_docker_familiar_repository_spellings_keep_full_identity_in_both_producers(self):
        for script in ('prepare-shell-image.py',):
            for spelling in ('familiar', 'library'):
                with self.subTest(script=script, spelling=spelling):
                    (self.root/'save-tag-mode').write_text(spelling)
                    result = self.invoke(script=script)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    proof = json.loads(self.output.with_name('shell-image.build.json').read_text())
                    local = proof['local_image']; archive = local['archive']
                    self.assertEqual(archive['repository_tags'], [local['runtime_reference']])
                    self.assertNotEqual(archive['repository_tags_observed'], archive['repository_tags'])
                    self.assertEqual(local['sdk_observation']['image_id'], archive['platform_manifest_digest'])
                    self.assertTrue(local['runtime_reference'].endswith(':sha256-' + archive['config_digest'][7:]))
                    self.assertEqual(proof['publication_effects'], [])

    def test_bare_array_template_listing_from_newer_sbx_imports_the_exact_template(self):
        (self.root/'bare-template-list').write_text('')
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        proof = json.loads(self.output.with_name('shell-image.build.json').read_text())
        local = proof['local_image']
        self.assertEqual(local['template']['id'], local['archive']['platform_manifest_digest'][7:19])
        self.assertEqual(proof['reference'], self.output.read_text().strip())

    def test_foreign_wrong_extra_and_missing_archive_tags_fail_before_sdk_with_bounded_evidence(self):
        for script in ('prepare-shell-image.py',):
            for mode in ('foreign', 'wrong-tag', 'extra', 'empty'):
                with self.subTest(script=script, mode=mode):
                    (self.root/'save-tag-mode').write_text(mode)
                    result = self.invoke(script=script)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn('exact import repository', result.stderr)
                    failure_path = self.output.with_name('shell-image.failed-build.json')
                    self.assertTrue(failure_path.is_file(), result.stderr)
                    self.assertLess(failure_path.stat().st_size, 256 * 1024)
                    failure = json.loads(failure_path.read_text())
                    self.assertEqual(failure['outcome'], 'failed')
                    self.assertFalse(failure['publication_may_have_occurred'])
                    observed = failure['local_image_observations']
                    self.assertEqual(observed['phase'], 'verify-import-name')
                    self.assertFalse(observed['sdk_import_attempted'])
                    self.assertIn('config_digest', observed['built_archive'])
                    self.assertIn('repository_tags', observed['imported_archive'])
                    self.assertFalse(self.output.exists())
                    self.assertFalse(self.output.with_name('shell-image.build.json').exists())
                    self.assertFalse(any(row[1:2] == ['template'] for row in self.calls()))
                    self.assertFalse((self.root/'sdk-calls.jsonl').exists())

    def test_explicit_unix_sdk_overrides_unused_relative_storage_in_real_helper_cli(self):
        self.env['SANDBOXES_STORAGE_ROOT'] = 'unsupported-relative-root'
        self.env['XDG_STATE_HOME'] = 'also-relative'
        result = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        proof = json.loads(self.output.with_name('shell-image.build.json').read_text())
        self.assertEqual(proof['local_image']['sdk_observation']['endpoint'], self.env['DOCKER_SANDBOXES_API'])
        self.assertTrue((self.root/'sdk-calls.jsonl').is_file())

    def test_ambiguous_raw_and_url_sdk_characters_fail_before_any_tool_or_socket(self):
        original = self.env['DOCKER_SANDBOXES_API'].removeprefix('unix://')
        for prefix in ('', 'unix://'):
            for character in ('%', '?', '#', '\r', '\n'):
                with self.subTest(prefix=prefix, character=repr(character)):
                    self.env['DOCKER_SANDBOXES_API'] = prefix + original + character
                    result = self.invoke()
                    self.assertNotEqual(result.returncode, 0, result.stdout)
                    self.assertEqual(self.calls(), [])
                    self.assertFalse((self.root/'sdk-calls.jsonl').exists())
                    self.assertFalse(self.output.exists())

    def test_same_short_prefix_sdk_substitution_cannot_publish_local_proof(self):
        first = self.invoke()
        self.assertEqual(first.returncode, 0, first.stderr)
        proof_path = self.output.with_name('shell-image.build.json')
        previous = (self.output.read_bytes(), proof_path.read_bytes())
        digest = json.loads(proof_path.read_text())['local_image']['archive']['platform_manifest_digest']
        (self.root/'sdk-override').write_text(digest[:19] + ('a' if digest[19] != 'a' else 'b') + digest[20:])
        rejected = self.invoke()
        self.assertNotEqual(rejected.returncode, 0)
        self.assertIn('FULL SDK image ID differs', rejected.stderr)
        self.assertEqual(previous, (self.output.read_bytes(), proof_path.read_bytes()))

    def test_owned_temporary_context_handles_system_style_tmpdir_alias_without_accepting_input_links(self):
        real = self.root / 'real-temporary'
        real.mkdir(mode=0o700)
        alias = self.root / 'temporary-alias'
        alias.symlink_to(real, target_is_directory=True)
        self.env['TMPDIR'] = str(alias)
        for script in ('prepare-shell-image.py',):
            result = self.invoke('--validate-only', script=script)
            self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(list(real.iterdir()), [])

    def test_private_umask_notices_are_readable_after_both_real_helpers_stage_them(self):
        notices = self.source / "packaging/dhi-notices/collected"
        notices.chmod(0o700); (notices / "NOTICE").chmod(0o600)
        for script in ("prepare-shell-image.py",):
            result = self.invoke("--validate-only", script=script)
            self.assertEqual(result.returncode, 0, result.stderr)
            row = json.loads((self.root / "context.json").read_text())["dhi-notices/NOTICE"]
            self.assertEqual(row["mode"], 0o644)
            self.assertEqual(row["sha256"], "sha256:" + hashlib.sha256((notices / "NOTICE").read_bytes()).hexdigest())
        self.assertFalse(any(row[1:2] == ["template"] for row in self.calls()))

    def test_redirected_or_world_writable_input_refused_before_docker_and_outputs_preserved(self):
        self.output.parent.mkdir(); self.output.write_text("old reference\n")
        notices = self.source / "packaging/dhi-notices/collected"
        for script in ("prepare-shell-image.py",):
            (notices / "NOTICE").chmod(0o666)
            result = self.invoke(script=script)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(self.calls(), [])
            (notices / "NOTICE").chmod(0o600)
            self.assertEqual(self.output.read_text(), "old reference\n")
        moved = self.root / "external-notices"
        (notices.parent).rename(moved)
        (self.source / "packaging/dhi-notices").symlink_to(moved, target_is_directory=True)
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.calls(), [])

    def test_oversized_or_hardlinked_notice_is_refused_before_builder(self):
        notice = self.source / 'packaging/dhi-notices/collected/NOTICE'
        original = notice.read_bytes()
        with notice.open('wb') as stream:
            stream.truncate(128 * 1024 * 1024 + 1)  # sparse, no large allocation
        result = self.invoke('--validate-only')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.calls(), [])
        notice.write_bytes(original)
        os.link(notice, self.root / 'outside-link')
        result = self.invoke('--validate-only')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.calls(), [])

    def test_output_collision_is_refused_without_build_or_previous_state_change(self):
        self.output.mkdir(parents=True)
        (self.output / 'sentinel').write_text('keep')
        for script in ('prepare-shell-image.py',):
            result = self.invoke(script=script)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual((self.output / 'sentinel').read_text(), 'keep')
            self.assertEqual(self.calls(), [])

    def test_failed_build_retains_prior_reference_and_proof_and_explicit_push_is_disclosed(self):
        for script in ("prepare-shell-image.py",):
            good = self.invoke("--repository", "example/published", script=script)
            self.assertEqual(good.returncode, 0, good.stderr)
            previous = (self.output.read_bytes(), self.output.with_name("shell-image.build.json").read_bytes())
            (self.root / "fail-build").touch()
            failed = self.invoke("--repository", "example/published", script=script)
            self.assertNotEqual(failed.returncode, 0)
            self.assertEqual(previous, (self.output.read_bytes(), self.output.with_name("shell-image.build.json").read_bytes()))
            (self.root / "fail-build").unlink()
            target = self.source / "packaging/dhi-notices/collected/NOTICE"
            (self.root / "mutate-source").write_text(str(target))
            failed = self.invoke("--repository", "example/published", script=script)
            self.assertNotEqual(failed.returncode, 0)
            self.assertIn("publication may have occurred", failed.stderr)
            self.assertEqual(previous, (self.output.read_bytes(), self.output.with_name("shell-image.build.json").read_bytes()))
            (self.root / "mutate-source").unlink()

    def test_interrupted_builder_is_forwarded_to_its_owned_group_and_keeps_previous_output(self):
        peer = self.root / 'docker'
        slow = '''
if args[:2] == ['buildx','build']:
 import os,signal,time
 (root/'started.json').write_text(json.dumps({'pid':os.getpid(),'pgid':os.getpgrp(),'parent':os.getppid()}))
 def stop(number,frame):
  (root/'terminated').write_text(str(number));sys.exit(128+number)
 signal.signal(signal.SIGTERM,stop)
 while True:time.sleep(.1)
'''
        peer.write_text(peer.read_text().replace('def sha(data):', slow + '\ndef sha(data):'))
        self.output.parent.mkdir(); self.output.write_text('previous reference')
        process = subprocess.Popen([sys.executable, str(self.source/'scripts/prepare-shell-image.py'),
                   '--docker', str(peer), '--repository', 'example/published', '--output', str(self.output)],
                   cwd=self.source, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        child = None
        try:
            deadline = time.monotonic() + 10
            while not (self.root/'started.json').exists():
                self.assertIsNone(process.poll())
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.05)
            child = json.loads((self.root/'started.json').read_text())
            self.assertEqual(child['parent'], process.pid)
            self.assertEqual(child['pgid'], child['pid'])
            os.killpg(process.pid, signal.SIGTERM)
            _, errors = process.communicate(timeout=15)
            self.assertNotEqual(process.returncode, 0, errors)
            self.assertEqual((self.root/'terminated').read_text(), str(int(signal.SIGTERM)))
            self.assertEqual(self.output.read_text(), 'previous reference')
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL); process.wait(timeout=5)
            if child and not (self.root/'terminated').exists():
                self.assertGreater(child['pid'], 1)
                self.assertNotEqual(child['pgid'], os.getpgrp())
                try:os.killpg(child['pgid'], signal.SIGKILL)
                except ProcessLookupError:pass

    def test_corrupt_local_archive_cannot_be_imported_or_get_a_receipt(self):
        peer = self.root / "docker"
        peer.write_text(peer.read_text().replace("member.size=len(data);tar.addfile(member,io.BytesIO(data))",
                                 "member.size=len(data);tar.addfile(member,io.BytesIO(b'x'*len(data) if name.endswith(sha(layer)[7:]) else data))"))
        result = self.invoke()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("blob digest", result.stderr)
        self.assertFalse(any(row[1:3] == ["template", "load"] for row in self.calls()))
        self.assertFalse(self.output.exists())


if __name__ == "__main__":
    unittest.main()
