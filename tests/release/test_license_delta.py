"""scripts/license-delta.py, driven as `make refresh-pins` runs it, on small trees."""
import hashlib
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/license-delta.py"
MIT = b"Copyright (c) 2026 A. Author\n\nPermission is hereby granted, free of charge, to any person ...\n"
APACHE = b"                                 Apache License\n                           Version 2.0, January 2004\n"
GPL = b"GNU GENERAL PUBLIC LICENSE\nVersion 3, 29 June 2007\n"
CLAUDE = b"\xc2\xa9 Anthropic PBC. All rights reserved. Use is subject to the Legal Agreements.\n"


class Tree:
    def __init__(self, root: pathlib.Path):
        self.root = root
        self.bundle = root / "packaging/dhi-notices/collected"
        (self.bundle / "texts").mkdir(parents=True)
        (self.bundle / "provider-notices").mkdir()
        self.os, self.locks, self.profiles = [], {}, {}
        self.adapters = {}

    def text(self, data):
        digest = hashlib.sha256(data).hexdigest()
        (self.bundle / "texts" / (digest + ".txt")).write_bytes(data)
        return {"file": f"texts/{digest}.txt", "sha256": digest}

    def package(self, name, version, families, data=MIT):
        self.os.append({"package": name, "version": version, "families": families,
                        "notices": [hashlib.sha256(data).hexdigest()]})

    def npm(self, name, version, license):
        self.locks.setdefault("kits/marsh-pi/package-lock.json", {"packages": {}})["packages"][
            "node_modules/" + name] = {"version": version, "license": license}

    def claude(self, version, data):
        notice = self.text(data)
        notice["file"] = f"provider-notices/claude-{version}-package-LICENSE.md"
        (self.bundle / notice["file"]).write_bytes(data)
        self.profiles["claude-kit"] = {version: {"arm64": {"required_notices": [notice], "files": []}}}

    def write(self):
        def put(relative, value):
            path = self.root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps(value))

        put("packaging/dhi-notices/image-packages.json", {"images": {"marsh-shell": self.os}})
        put("packaging/dhi-notices/collected/go-module-notices.json", {"modules": []})
        put("packaging/dhi-notices/collected/rust-notices.json", {"collections": []})
        put("packaging/dhi-notices/collected/selected-rust-notices.json", {"collections": []})
        put("packaging/dhi-notices/collected/adapter-payloads.json", {"adapters": self.adapters})
        put("packaging/dhi-notices/collected/main-component-notices.json", [])
        put("packaging/dhi-notices/collected/derived-profiles.json", self.profiles)
        put("packaging/dhi-notices/collected/qualification-boundaries.json", {"proprietary_components": [
            {"kind": "agent", "name": "claude-code"},
            {"kind": "npm", "name": "@anthropic-ai/claude-agent-sdk*"}]})
        for relative, lock in self.locks.items():
            put(relative, lock)
        return self.root


def base(tree):
    tree.package("bash", "5.2-1", ["GPL"], GPL)
    tree.package("zlib1g", "1.3-1", ["Zlib"])
    tree.npm("left-pad", "1.0.0", "MIT")
    tree.npm("@anthropic-ai/claude-agent-sdk", "0.3.1", "SEE LICENSE IN README.md")
    tree.claude("2.1.0", CLAUDE)


class LicenseDelta(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="marsh-license-delta-")
        self.addCleanup(self.temporary.cleanup)
        root = pathlib.Path(self.temporary.name)
        self.before, self.after = Tree(root / "before"), Tree(root / "after")
        base(self.before)
        base(self.after)

    def run_delta(self):
        self.before.write()
        self.after.write()
        report = self.after.root / "report.json"
        result = subprocess.run([sys.executable, str(SCRIPT), "--before", str(self.before.root),
                                 "--after", str(self.after.root), "--json", str(report)],
                                capture_output=True, text=True, timeout=60)
        return result, json.loads(report.read_text())

    def test_identical_trees_are_ok_and_report_nothing(self):
        result, report = self.run_delta()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((report["verdict"], report["added"], report["removed"], report["changed"]),
                         ("OK", [], [], []))
        self.assertIn("No component was added, removed or changed.", result.stdout)

    def test_new_versions_and_new_deps_under_present_families_are_ok(self):
        self.after.os = [row for row in self.after.os if row["package"] != "bash"]
        self.after.package("bash", "5.2-2", ["GPL"], GPL)
        self.after.package("libfoo1", "2.0-1", ["MIT"])
        self.after.npm("left-pad", "1.0.1", "MIT")
        self.after.npm("right-pad", "3.0.0", "(MIT OR Zlib)")
        result, report = self.run_delta()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual(report["verdict"], "OK")
        self.assertEqual({(r["kind"], r["name"]) for r in report["added"]},
                         {("os", "libfoo1"), ("npm", "right-pad")})
        self.assertEqual({(r["kind"], r["name"], r["before"], r["after"]) for r in report["changed"]},
                         {("os", "bash", "5.2-1", "5.2-2"), ("npm", "left-pad", "1.0.0", "1.0.1")})

    def test_new_license_family_needs_owner(self):
        self.after.package("libmpl1", "1.0-1", ["MPL-2.0"], b"Mozilla Public License Version 2.0\n")
        result, report = self.run_delta()
        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["reasons"], ["new license family MPL-2.0: os libmpl1 1.0-1"])
        self.assertIn("**Verdict: NEEDS-OWNER**", result.stdout)

    def test_changed_proprietary_license_text_needs_owner(self):
        self.after.claude("2.2.0", CLAUDE.replace(b"Agreements", b"Terms"))
        result, report = self.run_delta()
        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["reasons"], ["proprietary license text changed: agent claude-code 2.1.0 -> 2.2.0"])

    def test_new_release_of_proprietary_component_with_same_text_is_ok(self):
        self.after.claude("2.2.0", CLAUDE)
        result, report = self.run_delta()
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual([(r["name"], r["before"], r["after"]) for r in report["changed"]],
                         [("claude-code", "2.1.0", "2.2.0")])

    def test_new_proprietary_component_needs_owner_even_in_a_present_family(self):
        self.after.npm("vendor-binary", "1.0.0", "UNLICENSED")
        result, report = self.run_delta()
        self.assertEqual(result.returncode, 2)
        self.assertEqual(report["reasons"], ["new proprietary or binary-only component: npm vendor-binary 1.0.0"])


if __name__ == "__main__":
    unittest.main()
