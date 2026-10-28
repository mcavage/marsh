"""scripts/ci-changes.sh: which CI lanes a pull request's changed files need."""
import pathlib
import subprocess
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[2] / "scripts/ci-changes.sh"


def classify(*files):
    result = subprocess.run(["sh", str(SCRIPT)], input="".join(f + "\n" for f in files),
                            capture_output=True, text=True, timeout=30, check=True)
    return dict(line.split("=") for line in result.stdout.split())


class CiChanges(unittest.TestCase):
    def check(self, files, code, host):
        self.assertEqual(classify(*files), {"code": code, "host": host}, files)

    def test_documentation_and_metadata_need_no_code_lane(self):
        self.check(["docs/split.md"], "false", "false")
        self.check(["docs/design/processes.md", "docs/site/style.css", "README.md", "CONTRIBUTING.md"],
                   "false", "false")
        self.check([".github/renovate.json5", ".github/CODEOWNERS"], "false", "false")

    def test_model_and_patch_queue_under_docs_are_code(self):
        self.check(["docs/model/Worker.tla"], "true", "false")
        self.check(["docs/upstream/brush/0042-x.patch", "docs/split.md"], "true", "false")

    def test_kits_and_packaging_run_linux_but_not_the_mac_host_tests(self):
        self.check(["kits/marsh-claude/claude.yaml"], "true", "false")
        self.check(["packaging/shell/Dockerfile", "kits/marsh-pi/package-lock.json"], "true", "false")
        self.check(["scripts/third-party-notices.py"], "true", "false")

    def test_host_paths_run_the_mac_host_tests(self):
        for path in ("crates/marsh-sbx/src/lib.rs", "Cargo.lock", "Cargo.toml", "rust-toolchain.toml",
                     "vendor/brush/src/lib.rs", "Makefile", "tests/release/test_site.py",
                     ".github/workflows/ci.yml"):
            self.check([path], "true", "true")

    def test_any_code_file_beside_documentation_runs_everything_it_touches(self):
        self.check(["docs/split.md", "crates/marsh/src/main.rs"], "true", "true")

    def test_workflows_other_than_ci_are_code_but_not_host(self):
        self.check([".github/workflows/release.yml"], "true", "false")

    def test_an_empty_list_runs_everything(self):
        self.check([], "true", "true")


if __name__ == "__main__":
    unittest.main()
