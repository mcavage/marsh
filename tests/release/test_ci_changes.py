"""scripts/ci-changes.sh: which CI lanes a change's files need."""
import pathlib
import subprocess
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[2] / "scripts/ci-changes.sh"
LANES = ("code", "host", "model", "images", "renovate")


def classify(*files):
    result = subprocess.run(["sh", str(SCRIPT)], input="".join(f + "\n" for f in files),
                            capture_output=True, text=True, timeout=30, check=True)
    return dict(line.split("=") for line in result.stdout.split())


class CiChanges(unittest.TestCase):
    def check(self, files, *lanes):
        expected = {lane: str(lane in lanes).lower() for lane in LANES}
        self.assertEqual(classify(*files), expected, files)

    def test_documentation_and_metadata_need_no_code_lane(self):
        self.check(["docs/split.md"])
        self.check(["docs/design/processes.md", "docs/site/style.css", "README.md", "CONTRIBUTING.md"])
        self.check([".github/CODEOWNERS"])

    def test_renovate_config_runs_only_its_validator(self):
        self.check([".github/renovate.json5"], "renovate")
        self.check([".github/renovate-version"], "code", "renovate")
        self.check(["tests/release/renovate-policy.mjs"], "code", "host", "renovate")

    def test_model_and_patch_queue_under_docs_are_code(self):
        self.check(["docs/model/Worker.tla"], "code", "model")
        self.check(["docs/model/check.sh"], "code", "model")
        self.check(["docs/upstream/brush/0042-x.patch", "docs/split.md"], "code")

    def test_kits_and_packaging_run_linux_and_images_but_not_the_mac_host_tests(self):
        self.check(["kits/marsh-claude/claude.yaml"], "code", "images")
        self.check(["packaging/shell/Dockerfile", "kits/marsh-pi/package-lock.json"], "code", "images")
        self.check(["scripts/publish-kits.py"], "code", "images")
        self.check(["scripts/third-party-notices.py"], "code")

    def test_host_paths_run_the_mac_host_tests(self):
        for path in ("crates/marsh-sbx/src/lib.rs", "Cargo.lock", "Cargo.toml", "rust-toolchain.toml",
                     "vendor/brush/src/lib.rs", "tests/release/test_site.py", ".config/nextest.toml"):
            self.check([path], "code", "host")
        self.check(["Makefile"], "code", "host", "model")

    def test_ci_itself_runs_every_lane(self):
        self.check([".github/workflows/ci.yml"], *LANES)
        self.check(["scripts/ci-changes.sh"], *LANES)

    def test_any_code_file_beside_documentation_runs_everything_it_touches(self):
        self.check(["docs/split.md", "crates/marsh/src/main.rs"], "code", "host")

    def test_workflows_other_than_ci_are_code_but_not_host(self):
        self.check([".github/workflows/release.yml"], "code")

    def test_an_empty_list_runs_everything(self):
        self.check([], *LANES)


if __name__ == "__main__":
    unittest.main()
