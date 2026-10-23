#!/usr/bin/env python3
"""scripts/promote-stable-kits.py against a fake `docker buildx imagetools`."""
import importlib.util
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import textwrap
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/promote-stable-kits.py"
SPEC = importlib.util.spec_from_file_location("promote_stable_kits", SCRIPT)
PROMOTE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PROMOTE)

ARM = "sha256:" + "a" * 64
FAKE_DOCKER = textwrap.dedent(
    """\
    #!/usr/bin/env python3
    import json, os, sys
    state = json.load(open(os.environ["FAKE_REGISTRY"]))
    args = sys.argv[1:]
    assert args[:2] == ["buildx", "imagetools"], args
    args = args[2:]
    if args[0] == "create":
        with open(os.environ["FAKE_CREATED"], "a") as log:
            log.write(" ".join(args[1:]) + "\\n")
        sys.exit(0)
    if args[1] == "--raw":
        reference = args[2]
        if reference not in state["raw"]:
            print(f"ERROR: {reference}: not found", file=sys.stderr)
            sys.exit(1)
        print(json.dumps(state["raw"][reference]))
    else:
        print(json.dumps(state["image"][args[1]]))
    """
)


def descriptor(version):
    return json.dumps({"provides": [f"pi@{version}", "deb/bash@5.2"]})


class Decisions(unittest.TestCase):
    def test_the_stable_range_is_read_from_kit_source(self):
        source = (ROOT / "crates/marsh-contracts/src/kit_source.rs").read_text()
        self.assertIsNotNone(PROMOTE.supported(source))
        self.assertIsNone(PROMOTE.supported("pub const OTHER: u32 = 1;"))

    def test_only_a_supported_contract_with_a_new_descriptor_moves(self):
        self.assertTrue(PROMOTE.decide(1, (1, 1), "new", "old")[0])
        self.assertTrue(PROMOTE.decide(1, (1, 2), "new", None)[0])
        self.assertFalse(PROMOTE.decide(2, (1, 1), "new", "old")[0])
        self.assertFalse(PROMOTE.decide(None, (1, 1), "new", "old")[0])
        self.assertFalse(PROMOTE.decide(1, None, "new", "old")[0])
        self.assertFalse(PROMOTE.decide(1, (1, 1), "same", "same")[0])

    def test_repositories_keep_registry_ports(self):
        self.assertEqual(PROMOTE.repository("localhost:5000/a/b:latest"), "localhost:5000/a/b")
        self.assertEqual(PROMOTE.repository(f"docker.io/a/b@{ARM}"), "docker.io/a/b")


class Promote(unittest.TestCase):
    def run_script(self, registry, *extra):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            docker = root / "docker"
            docker.write_text(FAKE_DOCKER)
            docker.chmod(0o755)
            (root / "registry.json").write_text(json.dumps(registry))
            commands = root / "commands.json"
            commands.write_text(json.dumps({name: ref for name, ref in self.refs.items()}))
            created = root / "created"
            created.touch()
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "promote", "--commands", str(commands),
                 "--supported", "1-1", "--docker", str(docker), *extra],
                capture_output=True, text=True, check=False,
                env={**os.environ, "FAKE_REGISTRY": str(root / "registry.json"),
                     "FAKE_CREATED": str(created)},
            )
            return result, created.read_text().splitlines()

    def setUp(self):
        digest = lambda n: f"sha256:{n * 64}"
        self.refs = {
            "pi": f"docker.io/mcavage/marsh-pi@{digest('1')}",
            "codex": f"docker.io/mcavage/marsh-codex@{digest('2')}",
            "claude": f"docker.io/mcavage/marsh-claude@{digest('3')}",
        }
        index = {"manifests": [{"digest": ARM, "platform": {"os": "linux", "architecture": "arm64"}}]}
        labelled = lambda contract: {"config": {"Labels": {"org.marsh.kit.contract": contract}}}
        manifest = lambda version: {"annotations": {PROMOTE.DESCRIPTOR: descriptor(version)}}
        self.registry = {
            "image": {
                self.refs["pi"]: labelled("1"),
                self.refs["codex"]: labelled("1"),
                self.refs["claude"]: {"linux/arm64": labelled("2")},
            },
            "raw": {
                # pi: a new agent version; :latest is older.
                self.refs["pi"]: index,
                f"docker.io/mcavage/marsh-pi@{ARM}": manifest("1.4.0"),
                "docker.io/mcavage/marsh-pi:latest": manifest("1.3.0"),
                # codex: rebuilt, nothing new.
                self.refs["codex"]: manifest("0.163.0"),
                "docker.io/mcavage/marsh-codex:latest": manifest("0.163.0"),
                # claude: needs a newer marsh.
                self.refs["claude"]: manifest("2.1.400"),
            },
        }

    def test_a_dry_run_moves_nothing(self):
        result, created = self.run_script(self.registry)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(created, [])
        self.assertIn("would move docker.io/mcavage/marsh-pi:latest", result.stdout)

    def test_apply_moves_only_new_compatible_kits(self):
        result, created = self.run_script(self.registry, "--apply")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(created, [f"--tag docker.io/mcavage/marsh-pi:latest {self.refs['pi']}"])
        self.assertIn("kept docker.io/mcavage/marsh-codex:latest: same Kit descriptor", result.stdout)
        self.assertIn("kept docker.io/mcavage/marsh-claude:latest: contract 2 is outside 1-1", result.stdout)

    def test_a_missing_build_fails_after_trying_the_rest(self):
        del self.registry["raw"][self.refs["codex"]]
        result, created = self.run_script(self.registry, "--apply")
        self.assertEqual(result.returncode, 1)
        self.assertIn("failed docker.io/mcavage/marsh-codex:latest", result.stderr)
        self.assertEqual(len(created), 1)


if __name__ == "__main__":
    unittest.main()
