#!/usr/bin/env python3
"""scripts/install.sh (runmar.sh/install) against a fake GitHub Releases.

Hermetic: curl, uname, shasum, pgrep and sbx are fakes on PATH, and curl
serves files from a local release directory, so the network, the latest-tag
redirect and the version pin run without GitHub, on Linux CI as on a Mac.
"""
import hashlib
import os
import pathlib
import shutil
import subprocess
import sys
import tarfile
import tempfile
import textwrap
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
INSTALL = ROOT / "scripts/install.sh"
REPO = "example/marsh"

CURL = r'''
import os, pathlib, shutil, sys
args = sys.argv[1:]
url = next(a for a in args if a.startswith("https://"))
releases = pathlib.Path(os.environ["FAKE_RELEASES"])
with open(os.environ["FAKE_CURL_LOG"], "a") as log:
    log.write(url + "\n")
prefix = "https://github.com/%s/releases/" % os.environ["FAKE_REPO"]
if not url.startswith(prefix):
    sys.exit(6)
rest = url[len(prefix):]
if rest == "latest":
    latest = (releases / "LATEST").read_text().strip() if (releases / "LATEST").exists() else ""
    sys.stdout.write(prefix + ("tag/" + latest if latest else ""))
    sys.exit(0)
if not rest.startswith("download/"):
    sys.exit(22)
source = releases / rest[len("download/"):]
if not source.is_file():
    sys.exit(22)
shutil.copy(source, args[args.index("-o") + 1])
'''

SHASUM = r'''
import hashlib, sys
args = sys.argv[1:]
assert args[:3] == ["-a", "256", "-c"], args
ok = True
for line in open(args[3]):
    digest, name = line.split(None, 1)
    ok &= hashlib.sha256(open(name.strip().lstrip("*"), "rb").read()).hexdigest() == digest
sys.exit(0 if ok else 1)
'''


def tool(directory, name, body, python=False):
    path = directory / name
    path.write_text(f"#!{sys.executable}\n{body}" if python else f"#!/bin/sh\n{body}")
    path.chmod(0o755)


def release(releases, version, corrupt=False):
    """A marsh-VERSION-darwin-arm64.tar.gz and its .sha256 under releases/vVERSION/."""
    name = f"marsh-{version}-darwin-arm64"
    stage = pathlib.Path(tempfile.mkdtemp()) / name
    for sub in ("bin", "libexec/marsh", "share/man/man1", "share/licenses/marsh"):
        (stage / sub).mkdir(parents=True)
    (stage / "bin/marsh").write_text(f"#!/bin/sh\necho 'marsh {version}'\n")
    for binary in ("marshd", "marsh-mcp"):
        (stage / "bin" / binary).write_text("#!/bin/sh\nexit 0\n")
    for binary in ("marsh", "marshd", "marsh-mcp"):
        (stage / "bin" / binary).chmod(0o755)
    (stage / "libexec/marsh/marsh-worker-linux-arm64").write_text(version)
    (stage / "share/man/man1/marsh.1").write_text(".TH MARSH 1\n")
    (stage / "share/licenses/marsh/LICENSE").write_text("license\n")
    out = releases / f"v{version}"
    out.mkdir(parents=True)
    tarball = out / f"{name}.tar.gz"
    with tarfile.open(tarball, "w:gz") as tar:
        tar.add(stage, arcname=name)
    shutil.rmtree(stage.parent)
    digest = hashlib.sha256(tarball.read_bytes()).hexdigest()
    if corrupt:
        digest = "0" * 64
    (out / f"{name}.tar.gz.sha256").write_text(f"{digest}  {name}.tar.gz\n")
    return tarball


class Install(unittest.TestCase):
    def setUp(self):
        self.tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        self.releases = self.tmp / "releases"
        self.releases.mkdir()
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        self.prefix = self.tmp / "prefix"
        self.log = self.tmp / "curl.log"
        self.log.write_text("")
        tool(self.bin, "curl", CURL, python=True)
        tool(self.bin, "shasum", SHASUM, python=True)
        tool(self.bin, "uname", 'case "$1" in -s) echo "${FAKE_OS:-Darwin}";; -m) echo "${FAKE_ARCH:-arm64}";; esac\n')
        tool(self.bin, "pgrep", "exit 1\n")
        tool(self.bin, "sbx", 'echo "sbx version: v${FAKE_SBX_VERSION:-0.50.0}"\n')
        # Only the fakes plus the basic tools the script needs (tar, mktemp,
        # cp, ...); nothing else from the host, so a real sbx, curl or sudo
        # cannot be reached.
        self.system = self.tmp / "system"
        self.system.mkdir()
        for name in ("tar", "gzip", "mktemp", "cp", "mv", "rm", "ln", "mkdir", "chmod", "cat", "sed", "head",
                     "basename", "tr", "ps", "printf", "sh", "dirname"):
            found = shutil.which(name)
            if found:
                (self.system / name).symlink_to(found)

    def install(self, *args, env=None):
        environment = {"PATH": f"{self.bin}:{self.system}", "HOME": str(self.tmp / "home"), "TMPDIR": str(self.tmp),
                       "MARSH_REPO": REPO, "FAKE_REPO": REPO, "FAKE_RELEASES": str(self.releases),
                       "FAKE_CURL_LOG": str(self.log), **(env or {})}
        return subprocess.run(["sh", str(INSTALL), "--prefix", str(self.prefix), *args], env=environment,
                              stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=60)

    def installed_version(self):
        return subprocess.run([str(self.prefix / "bin/marsh")], capture_output=True, text=True).stdout.strip()

    def test_installs_the_latest_release(self):
        release(self.releases, "1.2.2")
        release(self.releases, "1.2.3")
        (self.releases / "LATEST").write_text("v1.2.3")
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.installed_version(), "marsh 1.2.3")
        for path in ("bin/marshd", "bin/marsh-mcp", "share/man/man1/marsh.1", "share/licenses/marsh/LICENSE",
                     "libexec/marsh/marsh-worker-linux-arm64"):
            self.assertTrue((self.prefix / path).is_file(), path)
        self.assertEqual(os.readlink(self.prefix / "bin/msh"), "marsh")
        self.assertIn(f"Installed marsh 1.2.3 in {self.prefix}/bin", result.stdout)
        self.assertIn(f"{self.prefix}/bin is not on your PATH", result.stdout)
        self.assertEqual(self.log.read_text().splitlines(), [
            f"https://github.com/{REPO}/releases/latest",
            f"https://github.com/{REPO}/releases/download/v1.2.3/marsh-1.2.3-darwin-arm64.tar.gz",
            f"https://github.com/{REPO}/releases/download/v1.2.3/marsh-1.2.3-darwin-arm64.tar.gz.sha256"])

    def test_a_version_pin_skips_the_latest_lookup(self):
        release(self.releases, "1.2.2")
        release(self.releases, "1.2.3")
        (self.releases / "LATEST").write_text("v1.2.3")
        result = self.install("--version", "v1.2.2")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.installed_version(), "marsh 1.2.2")
        self.assertNotIn("/releases/latest", self.log.read_text())

    def test_an_upgrade_replaces_libexec_as_a_whole(self):
        release(self.releases, "1.2.3")
        stale = self.prefix / "libexec/marsh/from-an-old-release"
        stale.parent.mkdir(parents=True)
        stale.write_text("old")
        result = self.install("--version", "1.2.3")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(stale.exists())
        self.assertFalse((self.prefix / "libexec/marsh.old").exists())
        self.assertFalse((self.prefix / "libexec/marsh.new").exists())

    def test_a_checksum_mismatch_installs_nothing(self):
        release(self.releases, "1.2.3", corrupt=True)
        result = self.install("--version", "1.2.3")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertFalse((self.prefix / "bin/marsh").exists())

    def test_a_missing_release_or_no_release_at_all_is_an_error(self):
        result = self.install("--version", "9.9.9")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("download failed", result.stderr)
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no published release found", result.stderr)
        self.assertFalse((self.prefix / "bin/marsh").exists())

    def test_a_local_tarball_needs_no_network(self):
        tarball = release(self.tmp / "local", "1.2.4")
        result = self.install("--tarball", str(tarball))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.installed_version(), "marsh 1.2.4")
        self.assertEqual(self.log.read_text(), "")

    def test_refuses_other_platforms_and_an_old_sbx(self):
        release(self.releases, "1.2.3")
        for env, message in (({"FAKE_OS": "Linux"}, "marsh runs on macOS on Apple Silicon"),
                             ({"FAKE_ARCH": "x86_64"}, "needs an Apple Silicon Mac"),
                             ({"FAKE_SBX_VERSION": "0.46.9"}, "is too old")):
            result = self.install("--version", "1.2.3", env=env)
            self.assertNotEqual(result.returncode, 0, env)
            self.assertIn(message, result.stderr)
        self.assertFalse((self.prefix / "bin/marsh").exists())
        self.assertEqual(self.log.read_text(), "")

    def test_shellcheck_is_clean(self):
        shellcheck = shutil.which("shellcheck")
        if shellcheck is None:
            if os.environ.get("MARSH_REQUIRE_SHELLCHECK") == "1":
                self.fail("shellcheck is not on PATH")
            self.skipTest("shellcheck is not installed (CI sets MARSH_REQUIRE_SHELLCHECK=1)")
        result = subprocess.run([shellcheck, "--shell=sh", str(INSTALL)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, textwrap.indent(result.stdout, "  "))

    def test_a_required_shellcheck_that_is_missing_fails_the_lint(self):
        # CI sets MARSH_REQUIRE_SHELLCHECK=1, so a runner without shellcheck
        # fails the lint rather than skipping it.
        lint = "test_install.Install.test_shellcheck_is_clean"
        for required, verdict in (("1", f"({lint}) ... FAIL"), ("", f"({lint}) ... skipped")):
            result = subprocess.run([sys.executable, "-m", "unittest", "-v", lint], cwd=pathlib.Path(__file__).parent,
                                    env={"PATH": str(self.system), "MARSH_REQUIRE_SHELLCHECK": required},
                                    capture_output=True, text=True, timeout=60)
            self.assertIn(verdict, result.stderr, required)


if __name__ == "__main__":
    unittest.main()
