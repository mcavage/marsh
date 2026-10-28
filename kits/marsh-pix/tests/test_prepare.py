import json
import os
from pathlib import Path
import subprocess
import shutil
import tempfile
import unittest
import uuid

KIT = Path(__file__).resolve().parents[1]
# The public skills, roles and themes come from a checkout of the Pix repository.
PIX = Path(os.environ["PIX_SOURCE"]).resolve()

def tracked(relative):
    raw = subprocess.check_output(["git", "ls-files", "-z", "--", relative], cwd=PIX)
    return [Path(p.decode()) for p in raw.split(b"\0") if p]

class Preparation(unittest.TestCase):
    def prepare(self, target):
        return subprocess.run(["python3", str(KIT / "prepare.py"), "--pix-source", str(PIX), str(target)],
                              cwd=KIT, text=True, capture_output=True)

    def test_export_contains_public_resources_not_local_files(self):
        # Fake non-secret canaries, never real credentials. Probe untracked
        # files both beside a selected public skill and in the new Kit source.
        canaries = [PIX / "skills/analyze" / ("private-" + uuid.uuid4().hex + ".md"),
                    KIT / "pix/local-mcp.json",
                    KIT / "package/local-secrets.json"]
        for canary in canaries:
            self.assertFalse(canary.exists())
            canary.write_text("PUBLIC_BOUNDARY_TEST_CANARY")
        try:
            with tempfile.TemporaryDirectory(prefix="pix-public-") as temp:
                target = Path(temp) / "kit"
                result = self.prepare(target)
                self.assertEqual(result.returncode, 0, result.stderr)
                # The content lives in the Pix repository and changes without this Kit:
                # the counts follow the selection and the checkout, not fixed numbers.
                # The skills are the selection from the Pix repository plus this Kit's own.
                selected = json.loads((KIT / "skills.json").read_text())
                own = {p.parent.name for p in (KIT / "package/skills").glob("*/SKILL.md")}
                exported = {p.parent.name for p in (target / "package/skills").glob("*/SKILL.md")}
                self.assertEqual(exported, set(selected) | own)
                roles = [p for p in tracked("agents") if p.suffix == ".md"]
                self.assertEqual(len(list((target / "package/roles").glob("*.md"))), len(roles))
                for excluded in ["environments", "healthcheck", "ingest", "onboarding", "pix-dev", "promote"]:
                    self.assertFalse((target / "package/skills" / excluded).exists())
                self.assertFalse((target / "package/themes/host.json").exists())
                all_bytes = b"\n".join(p.read_bytes() for p in target.rglob("*") if p.is_file())
                self.assertNotIn(b"PUBLIC_BOUNDARY_TEST_CANARY", all_bytes)
                for marker in [b"/Users/", b"op://", b"host-state.json", b"secrets.env", b"gw.docker.com"]:
                    self.assertNotIn(marker, all_bytes)
                inventory = json.loads((target / "PUBLIC-SOURCES.json").read_text())
                self.assertIn("package/extensions/ui.mjs", inventory["files_sha256"])
                self.assertIn("marsh-orchestration", (target / "package/skills/deliver/SKILL.md").read_text())
        finally:
            for canary in canaries: canary.unlink()

    def test_public_verifier_works_without_removed_base_license_records(self):
        with tempfile.TemporaryDirectory(prefix="pix-public-check-") as temporary:
            root = Path(temporary)
            snapshot = root / "snapshot"
            prepared = self.prepare(snapshot)
            self.assertEqual(prepared.returncode, 0, prepared.stderr)
            image = root / "image"
            package = image / "opt/pix/package"
            shutil.copytree(snapshot / "package", package)
            notices = image / "usr/local/share/licenses/pix"
            notices.mkdir(parents=True)
            for source, target in (("LICENSE", "LICENSE"), ("PUBLIC-SOURCES.json", "inputs.json"),
                                   ("verify-public.py", "verify-public.py")):
                shutil.copy2(snapshot / source, notices / target)
            entrypoint = image / "usr/local/bin/marsh-pix"
            entrypoint.parent.mkdir(parents=True)
            shutil.copy2(snapshot / "entrypoint.sh", entrypoint)
            entrypoint.chmod(0o755)
            command = ["python3", str(notices / "verify-public.py"), "--root", str(image)]
            checked = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(checked.returncode, 0, checked.stderr)
            self.assertFalse((notices / "base-check").exists())
            self.assertNotIn("base_receipt_sha256", json.loads(checked.stdout))
            with (package / "instructions.md").open("a") as changed:
                changed.write("\ntampered\n")
            rejected = subprocess.run(command, capture_output=True, text=True)
            self.assertNotEqual(rejected.returncode, 0)
            self.assertIn("differs", rejected.stderr)

    def test_existing_destination_is_preserved(self):
        with tempfile.TemporaryDirectory(prefix="pix-public-") as temp:
            target = Path(temp) / "kit"
            target.mkdir()
            file = target / "keep.txt"
            file.write_text("existing work")
            result = self.prepare(target)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(file.read_text(), "existing work")
            self.assertEqual(list(target.iterdir()), [file])

    def test_checkout_destination_is_refused(self):
        for inside in (KIT / "test-output", PIX / "test-output"):
            result = self.prepare(inside)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(inside.exists())

if __name__ == "__main__": unittest.main()
