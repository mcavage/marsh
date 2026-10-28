"""The release notice copier retains original files without versioned ledgers."""
import json
import io
import tarfile
import pathlib
import subprocess
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]


class NoticeCopy(unittest.TestCase):
    def test_copies_original_bytes_without_requiring_records_for_new_versions(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            package = root / "crate"
            package.mkdir()
            license_text = b"Copyright Fixture Authors\nPermission fixture \xff\n"
            (package / "LICENSE").write_bytes(license_text)
            native_notice = b"Native component original attribution\n"
            with tarfile.open(package / "native.tar.gz", "w:gz") as archive:
                member = tarfile.TarInfo("native/third_party/lib/LICENSE")
                member.size = len(native_notice)
                archive.addfile(member, io.BytesIO(native_notice))
            unknown = root / "new-crate"
            unknown.mkdir()
            metadata = {"packages": [
                {"name": "example", "version": "99.0.0", "manifest_path": str(package / "Cargo.toml")},
                {"name": "new-dependency", "version": "1.0.0", "manifest_path": str(unknown / "Cargo.toml")}]}
            cargo = root / "cargo"
            cargo.write_text("#!/bin/sh\ncat <<'JSON'\n" + json.dumps(metadata) + "\nJSON\n")
            cargo.chmod(0o755)
            output = root / "notices"
            done = subprocess.run([sys.executable, ROOT / "scripts/third-party-notices.py",
                                   "--cargo", cargo, "--output", output], capture_output=True, text=True)
            self.assertEqual(done.returncode, 0, done.stderr)
            self.assertEqual([p.name for p in output.iterdir()], ["THIRD-PARTY-NOTICES.txt"])
            bundled = (output / "THIRD-PARTY-NOTICES.txt").read_bytes()
            self.assertIn(license_text, bundled)
            self.assertIn(native_notice, bundled)


if __name__ == "__main__":
    unittest.main()
