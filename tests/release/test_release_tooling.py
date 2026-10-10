#!/usr/bin/env python3
"""Real callers of the release scripts that run in the nightly and stable pipelines."""
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
NIGHTLY = "0.1.3-nightly.20261009171916.gb1f7e8b"
SHA = "a" * 64


def run(*argv, check=True):
    return subprocess.run([sys.executable, *map(str, argv)], capture_output=True, text=True, check=check)


class StampVersion(unittest.TestCase):
    def checkout(self):
        tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp)
        for manifest in [ROOT / "Cargo.toml", ROOT / "Cargo.lock", *ROOT.glob("crates/*/Cargo.toml")]:
            target = tmp / manifest.relative_to(ROOT)
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(manifest, target)
        return tmp

    def test_stamps_the_workspace_and_only_the_workspace(self):
        tmp = self.checkout()
        before = (tmp / "Cargo.lock").read_text()
        run(ROOT / "scripts/stamp-version.py", NIGHTLY, "--root", tmp)
        self.assertRegex((tmp / "Cargo.toml").read_text(), rf'(?m)^version = "{re.escape(NIGHTLY)}"$')
        after = (tmp / "Cargo.lock").read_text()
        changed = [line for line in after.splitlines() if NIGHTLY in line]
        members = len(list((tmp / "crates").glob("*/Cargo.toml")))
        self.assertEqual(len(changed), members)
        # Every other lock line is untouched, including registry crates that
        # happen to share the old version number.
        self.assertEqual(len(before.splitlines()), len(after.splitlines()))
        differing = [1 for a, b in zip(before.splitlines(), after.splitlines()) if a != b]
        self.assertEqual(len(differing), members)

    def test_refuses_a_crate_with_its_own_version(self):
        tmp = self.checkout()
        manifest = tmp / "crates/marsh/Cargo.toml"
        manifest.write_text(manifest.read_text().replace("version.workspace = true", 'version = "9.9.9"', 1))
        result = run(ROOT / "scripts/stamp-version.py", "1.2.3", "--root", tmp, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("version.workspace", result.stderr)

    def test_refuses_a_malformed_version(self):
        tmp = self.checkout()
        for bad in ("1.2", "1.2.3+build", "latest"):
            result = run(ROOT / "scripts/stamp-version.py", bad, "--root", tmp, check=False)
            self.assertNotEqual(result.returncode, 0, bad)


class RenderFormula(unittest.TestCase):
    def render(self, channel, version, check=True):
        out = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, out)
        result = run(ROOT / "scripts/render-formula.py", "--channel", channel, "--version", version,
                     "--sha256", SHA, "--output", out / "f.rb", check=check)
        return result, out / "f.rb"

    def test_stable_formula_is_marsh(self):
        _, path = self.render("stable", "0.1.3")
        text = path.read_text()
        self.assertIn("class Marsh < Formula", text)
        self.assertIn('conflicts_with "marsh-nightly"', text)
        self.assertIn("/releases/download/v0.1.3/marsh-0.1.3-darwin-arm64.tar.gz", text)
        self.assertNotIn("\n  version ", text)
        self.assertNotRegex(text, r"@[A-Z_]+@")

    def test_nightly_formula_is_marsh_nightly_with_an_explicit_version(self):
        _, path = self.render("nightly", NIGHTLY)
        text = path.read_text()
        self.assertIn("class MarshNightly < Formula", text)
        self.assertIn('conflicts_with "marsh",', text)
        self.assertIn(f'version "{NIGHTLY}"', text)
        self.assertIn(f"/releases/download/v{NIGHTLY}/marsh-{NIGHTLY}-darwin-arm64.tar.gz", text)
        self.assertNotRegex(text, r"@[A-Z_]+@")

    def test_each_channel_refuses_the_other_channels_version(self):
        self.assertNotEqual(self.render("stable", NIGHTLY, check=False)[0].returncode, 0)
        self.assertNotEqual(self.render("nightly", "0.1.3", check=False)[0].returncode, 0)


if __name__ == "__main__":
    unittest.main()
