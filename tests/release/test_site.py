"""User guides must reach the website navigation and the downloadable manual."""

from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class SitePublicationTest(unittest.TestCase):
    def test_guides_are_published_and_an_unlisted_guide_blocks_the_build(self):
        with tempfile.TemporaryDirectory(prefix="marsh-site-") as temporary:
            root = Path(temporary)
            for directory in ("guides", "design", "man", "site"):
                shutil.copytree(ROOT / "docs" / directory, root / "docs" / directory)
            for source in (ROOT / "docs").glob("*.md"):
                shutil.copy2(source, root / "docs" / source.name)
            (root / "scripts").mkdir()
            for name in ("sitegen.py", "install.sh"):
                shutil.copy2(ROOT / "scripts" / name, root / "scripts" / name)
            out = root / "published"
            command = [sys.executable, str(root / "scripts/sitegen.py"), "--out", str(out)]
            built = subprocess.run(command, capture_output=True, text=True, timeout=30)
            self.assertEqual(built.returncode, 0, built.stderr)
            manual = (out / "llms-full.txt").read_text()
            index = (out / "llms.txt").read_text()
            for source in (root / "docs/guides").glob("*.md"):
                name = "index" if source.stem == "README" else source.stem
                page = (out / "docs/guides" / f"{name}.html").read_text()
                self.assertIn(f"https://runmar.sh/docs/guides/{name}.md", manual)
                self.assertIn(f"https://runmar.sh/docs/guides/{name}.md", index)
                self.assertIn(f'href="{name}.html" aria-current="page"', page)
                self.assertIn('class="pager"', page)
            home = (out / "docs/guides/set-up-your-home.html").read_text()
            self.assertIn('content="Set up agent skills, dotfiles, MCP connections,', home)
            self.assertNotIn("doc-test:", manual)

            (root / "docs/guides/new-guide.md").write_text("# New guide\n\nUseful instructions.\n")
            rejected = subprocess.run(command, capture_output=True, text=True, timeout=30)
            self.assertNotEqual(rejected.returncode, 0)
            self.assertIn("manual pages missing navigation", rejected.stderr)
            self.assertIn("docs/guides/new-guide.html", rejected.stderr)
            self.assertEqual((out / "llms-full.txt").read_text(), manual)


if __name__ == "__main__":
    unittest.main()
