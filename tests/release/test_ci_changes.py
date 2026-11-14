"""File selection for CI builds, model checking and image publication."""
import os
import pathlib
import re
import subprocess
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/ci-changes.sh"
LANES = ("code", "host", "tools", "model", "images", "renovate")

# Files a Rust source names at compile time or in a test: include_str!/
# include_bytes! relative to the source file, and paths built from
# CARGO_MANIFEST_DIR relative to the crate.
INCLUDE = re.compile(r'include_(?:str|bytes)!\(\s*"([^"]+)"')
MANIFEST_CONCAT = re.compile(r'env!\("CARGO_MANIFEST_DIR"\)\s*,\s*"([^"]+)"')
MANIFEST_JOIN = re.compile(r'env!\("CARGO_MANIFEST_DIR"\)\)\s*\.join\(\s*"([^"]+)"')


def classify(*files):
    result = subprocess.run(["sh", str(SCRIPT)], input="".join(f + "\n" for f in files),
                            capture_output=True, text=True, timeout=30, check=True)
    return dict(line.split("=") for line in result.stdout.split())


def crate_dir(source):
    for parent in source.parents:
        if (parent / "Cargo.toml").is_file():
            return parent
    raise AssertionError(f"{source} is not in a crate")


def rust_test_inputs():
    """Every repository file the workspace's Rust sources read by path."""
    found = {}
    for source in sorted((ROOT / "crates").rglob("*.rs")):
        text = source.read_text(encoding="utf-8", errors="replace")
        targets = [source.parent / rel for rel in INCLUDE.findall(text)]
        crate = crate_dir(source)
        targets += [crate / rel.lstrip("/") for rel in MANIFEST_CONCAT.findall(text) + MANIFEST_JOIN.findall(text)]
        for target in targets:
            target = pathlib.Path(os.path.normpath(target))
            if not target.exists():
                continue
            files = [target] if target.is_file() else [path for path in target.rglob("*") if path.is_file()]
            for path in files:
                found.setdefault(path.relative_to(ROOT).as_posix(), source.relative_to(ROOT).as_posix())
    return found


class CiChanges(unittest.TestCase):
    def check(self, files, *lanes):
        expected = {lane: str(lane in lanes).lower() for lane in LANES}
        self.assertEqual(classify(*files), expected, files)

    def test_documentation_needs_no_build_or_model_lane(self):
        self.check(["docs/split.md", "README.md", "CONTRIBUTING.md"])
        self.check(["docs/model/README.md"])

    def test_renovate_configuration_runs_tooling_and_its_validator(self):
        for path in (".github/renovate.json5", ".github/renovate-version",
                     "tests/release/renovate-policy.mjs"):
            self.check([path], "tools", "renovate")

    def test_model_inputs_run_only_the_model_lane(self):
        for path in ("docs/model/Worker.tla", "docs/model/Worker.cfg", "docs/model/check.sh"):
            self.check([path], "model")
        self.check([".github/workflows/model.yml"], "tools", "model")

    def test_image_only_updates_skip_rust_and_models(self):
        self.check(["packaging/shell/Dockerfile", "tests/acceptance/fixture/fixture.dockerfile"], "tools", "images")
        self.check(["scripts/publish-kits.py"], "tools", "images")
        self.check(["scripts/third-party-notices.py"], "tools")

    def test_services_engine_inputs_build_the_engine_image(self):
        for path in ("packaging/svc-engine/Dockerfile", "scripts/prepare-svc-engine-image.py"):
            self.check([path], "tools", "images")
        workflow = (SCRIPT.parents[1] / ".github/workflows/ci.yml").read_text()
        job = re.search(r"(?ms)^  images:\n(.*?)(?=^  \S)", workflow)
        self.assertIsNotNone(job, "ci.yml has no images job")
        self.assertRegex(job.group(1), r"run: python3 scripts/prepare-svc-engine-image\.py --docker docker --output ")
        self.assertNotRegex(job.group(1), r"--push\b|docker push|kit-publish|--repository")

    def test_kit_and_command_inputs_the_rust_tests_read_run_the_rust_lanes(self):
        # marsh-contracts reads every Kit file and the packaged command
        # tables; marsh-acp reads the ACP fixture. A Renovate bump of one of
        # them must not report "CI passed" without those tests.
        for path in ("kits/marsh-claude/claude.yaml", "kits/marsh-pi/package-lock.json",
                     "tests/acceptance/acp-fixture/acp-fixture.dockerfile"):
            self.check([path], "code", "host", "tools", "images")
        self.check(["kits/marsh-tools/Dockerfile"], "code", "host", "tools")
        for path in ("packaging/commands.json", "tests/acceptance/fixture/commands.json"):
            self.check([path], "code", "host", "tools", "images")
        self.check(["packaging/drive-commands.json"], "code", "host", "tools")

    def test_every_file_a_rust_test_reads_runs_the_rust_lanes(self):
        inputs = rust_test_inputs()
        # The scan must keep finding the known readers.
        for path in ("packaging/commands.json", "tests/acceptance/acp-fixture/host.py",
                     "tests/acceptance/acp-fixture/agent.mjs", "kits/marsh-claude/claude.yaml"):
            self.assertIn(path, inputs)
        missed = {}
        for path, reader in inputs.items():
            lanes = classify(path)
            if lanes["code"] != "true" or lanes["host"] != "true":
                missed[path] = reader
        self.assertEqual(missed, {}, "files read by Rust sources that do not select the Rust lanes "
                         "(add them to code= in scripts/ci-changes.sh)")

    def test_rust_and_real_binary_callers_run_both_platforms(self):
        for path in ("crates/marsh-sbx/src/lib.rs", "Cargo.lock", "Cargo.toml", "rust-toolchain.toml",
                     "vendor/brush/src/lib.rs", "docs/upstream/brush/0042-x.patch", ".config/nextest.toml", "tests/mcp/test_stdio.py",
                     "tests/acceptance/onboarding.py", "tests/acceptance/acp-fixture/typed_host_caller.py",
                     "tests/acceptance/bash_suites.py"):
            self.check([path], "code", "host", "tools")
        self.check(["Makefile"], "code", "host", "tools", "model", "images")

    def test_tool_self_tests_do_not_compile_rust(self):
        self.check(["tests/release/test_site.py", "tests/acceptance/test_image_preparation.py"], "tools")

    def test_ci_itself_runs_every_lane(self):
        self.check([".github/workflows/ci.yml"], *LANES)
        self.check(["scripts/ci-changes.sh"], *LANES)

    def test_combined_changes_run_the_union_of_lanes(self):
        self.check(["docs/split.md", "crates/marsh/src/main.rs", "packaging/shell-image"],
                   "code", "host", "tools", "images")

    def test_release_workflow_runs_tooling_without_building_images(self):
        self.check([".github/workflows/release.yml"], "tools")

    def test_an_empty_list_runs_everything(self):
        self.check([], *LANES)


if __name__ == "__main__":
    unittest.main()
