"""File selection for CI builds, model checking and image publication."""
import pathlib
import re
import subprocess
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[2] / "scripts/ci-changes.sh"
LANES = ("code", "host", "tools", "model", "images", "renovate")


def classify(*files):
    result = subprocess.run(["sh", str(SCRIPT)], input="".join(f + "\n" for f in files),
                            capture_output=True, text=True, timeout=30, check=True)
    return dict(line.split("=") for line in result.stdout.split())


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
        self.check(["kits/marsh-claude/claude.yaml"], "tools", "images")
        self.check(["packaging/shell/Dockerfile", "kits/marsh-pi/package-lock.json",
                    "tests/acceptance/fixture/fixture.dockerfile",
                    "tests/acceptance/acp-fixture/acp-fixture.dockerfile"], "tools", "images")
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
