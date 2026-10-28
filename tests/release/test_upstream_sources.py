"""Versioned OS package names resolve to reviewed upstream repositories only."""
import importlib.util
import json
import pathlib
import sys
import unittest

HERE = pathlib.Path(__file__).resolve().parents[2] / "packaging/dhi-notices"
sys.path.insert(0, str(HERE))
spec = importlib.util.spec_from_file_location("marsh_notice_inventory", HERE / "inventory.py")
inventory = importlib.util.module_from_spec(spec)
spec.loader.exec_module(inventory)


class UpstreamSources(unittest.TestCase):
    def setUp(self):
        self.sources = json.loads((HERE / "upstream-sources.json").read_text())

    def test_node_major_versions_share_the_reviewed_source(self):
        for name in ("nodejs-24", "nodejs-26", "nodejs-28"):
            with self.subTest(name=name):
                source = inventory.upstream_source(name, self.sources)
                self.assertEqual(source["repository"], "nodejs/node")
                self.assertEqual(source["files"], ["LICENSE"])
                self.assertEqual(source["tag"].format(upstream=inventory.upstream_version("26.12.1-1")),
                                 "v26.12.1")

    def test_similar_unreviewed_names_are_not_implicitly_trusted(self):
        for name in ("nodejs", "nodejs-26-extra", "other-nodejs-26", "nodejs-next", "unknown"):
            with self.subTest(name=name):
                self.assertIsNone(inventory.upstream_source(name, self.sources))

    def test_exact_rules_override_families(self):
        self.sources["packages"]["nodejs-26"] = {"repository": "reviewed/override"}
        self.assertEqual(inventory.upstream_source("nodejs-26", self.sources)["repository"],
                         "reviewed/override")

    def test_ambiguous_families_fail_closed(self):
        self.sources["package_patterns"].append(self.sources["package_patterns"][0])
        with self.assertRaisesRegex(ValueError, "ambiguous"):
            inventory.upstream_source("nodejs-26", self.sources)


if __name__ == "__main__":
    unittest.main()
