#!/usr/bin/env python3
"""The Docker Compose version: one pin, and every copy of it agrees.

The source of truth is the services engine's DHI package pin,
`docker-compose-plugin=X.Y.Z-R` in packaging/svc-engine/Dockerfile (Renovate's
deb manager moves it). The binary that package installs reports `vX.Y.Z`. The
Kit images fetch Compose's release binary for the same X.Y.Z, pinned by
SHA-256, and restore falls back to X.Y.Z when it cannot ask the engine's
compose for its version.
"""
import os
import pathlib
import re
import shutil
import subprocess
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]
ENGINE = "packaging/svc-engine/Dockerfile"
PIN = re.compile(r"(?:^|[\s\\])docker-compose-plugin=(\S+)")
# Any `name=version` apt pin (the shape Renovate's deb manager matches).
APT_PIN = re.compile(r"(?:^|[\s\\])[a-z0-9][a-z0-9.+-]*=[0-9]\S*")
DOTTED = re.compile(r"\d+\.\d+\.\d+")
# The copies of Compose's upstream version, each exactly once in its file.
COPIES = {
    "packaging/image-base/docker-compose.sh": re.compile(r"(?m)^COMPOSE_VERSION=(\S+)$"),
    "kits/marsh-docker-cli/marsh-docker-cli.dockerfile": re.compile(r"(?m)^ARG COMPOSE_VERSION=(\S+)$"),
    "kits/marsh-docker-cli/marsh-docker-cli.yaml": re.compile(r"docker-compose-plugin@([^\"\s]+)"),
    "kits/README.md": re.compile(r"`/usr/local/lib/docker/cli-plugins/docker-compose`, v([^)\s]+)\)"),
    "crates/marsh-services/src/restore.rs": re.compile(r'const COMPOSE_FALLBACK_VERSION: &str = "([^"]+)";'),
}
# The two fetches of Compose's release binary: the same asset, the same digests.
FETCHES = ("packaging/image-base/docker-compose.sh", "kits/marsh-docker-cli/marsh-docker-cli.dockerfile")
# Text that names a Compose version to install; a new file with one must be added to COPIES.
MENTIONS = re.compile(r"docker-compose-plugin[=@]|COMPOSE_VERSION=|docker/compose/releases/download/")
SKIP = ("vendor/", "crates/marsh-services/e3/corpus/", "tests/release/test_compose_pins.py")


def read(path):
    return (ROOT / path).read_text()


def engine_pin():
    pins = PIN.findall(read(ENGINE))
    if len(pins) != 1:
        raise AssertionError(f"{ENGINE} must pin docker-compose-plugin exactly once, found {pins}")
    return pins[0]


def upstream(deb_version):
    """`[epoch:]upstream[-revision]` -> upstream."""
    version = deb_version.split(":", 1)[-1]
    return version.rpartition("-")[0] or version


def engine_check():
    """The Compose commands of the engine's RUN, as the build's shell runs them."""
    text = read(ENGINE)
    run = re.search(r"(?ms)^RUN (.*?)(?<!\\)\n", text)
    assert run, f"{ENGINE} has no RUN"
    script = re.sub(r"\\\n", " ", run.group(1))
    steps = [step.strip() for step in script.split("&&")]
    # The install and `test -x` of the plugin's path need the image; the checks run anywhere.
    return " && ".join(s for s in steps if "compose" in s and not s.startswith(("apt-get", "test -x")))


class ComposePins(unittest.TestCase):
    def test_every_copy_has_the_engine_pins_upstream_version(self):
        expected = upstream(engine_pin())
        for path, pattern in COPIES.items():
            found = pattern.findall(read(path))
            self.assertEqual(found, [expected], f"{path}: Compose version must be {expected} (the "
                             f"upstream version of {ENGINE}'s docker-compose-plugin pin)")

    def test_restore_names_the_fallback_once(self):
        text = read("crates/marsh-services/src/restore.rs")
        expected = upstream(engine_pin())
        self.assertEqual(text.count(f'"{expected}"'), 1, "restore.rs: use COMPOSE_FALLBACK_VERSION")

    def test_both_release_binary_fetches_pin_the_same_digests(self):
        digests = {path: sorted(re.findall(r"\b[0-9a-f]{64}\b", read(path))) for path in FETCHES}
        first, second = digests.values()
        self.assertEqual(len(first), 2, f"{FETCHES[0]}: one Compose digest per architecture")
        self.assertTrue(set(first) <= set(second), digests)

    def test_no_other_file_pins_compose(self):
        listed = subprocess.run(["git", "ls-files", "-z"], cwd=ROOT, capture_output=True, check=True)
        known = {ENGINE, *COPIES}
        strays = []
        for name in listed.stdout.decode().split("\0"):
            if not name or name in known or name.startswith(SKIP):
                continue
            try:
                text = (ROOT / name).read_text()
            except (UnicodeDecodeError, FileNotFoundError, IsADirectoryError):
                continue
            if MENTIONS.search(text):
                strays.append(name)
        self.assertEqual(strays, [], "a new copy of the Compose version: add it to COPIES")

    def test_engine_check_has_no_version_of_its_own(self):
        rest = APT_PIN.sub(" ", read(ENGINE))
        self.assertEqual(DOTTED.findall(rest), [],
                         f"{ENGINE}: versions belong in apt pins; checks derive from them")

    def run_check(self, installed, reported):
        """Run the engine's Compose check with stand-ins for dpkg-query and docker."""
        tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp)
        (tmp / "dpkg-query").write_text(
            "#!/bin/sh\n[ \"$1 $2 $3\" = '-W -f=${Version} docker-compose-plugin' ] || exit 2\n"
            f"printf %s '{installed}'\n")
        (tmp / "docker").write_text(
            "#!/bin/sh\n"
            f'[ "$*" = "compose version --short" ] && {{ echo "{reported}"; exit 0; }}\n'
            f'[ "$*" = "compose version" ] && {{ echo "Docker Compose version v{reported.lstrip("v")}"; exit 0; }}\n'
            "exit 2\n")
        for tool in ("dpkg-query", "docker"):
            (tmp / tool).chmod(0o755)
        env = {**os.environ, "PATH": f"{tmp}:{os.environ['PATH']}"}
        return subprocess.run(["sh", "-c", engine_check()], env=env, capture_output=True, timeout=30).returncode

    def test_engine_check_accepts_the_pinned_package(self):
        pin = engine_pin()
        self.assertIn("docker compose version", engine_check())
        self.assertEqual(self.run_check(pin, "v" + upstream(pin)), 0)
        self.assertEqual(self.run_check(pin, upstream(pin)), 0)
        # A rebuilt package (new revision) or an epoch changes nothing upstream.
        self.assertEqual(self.run_check("1:" + upstream(pin) + "-9", upstream(pin)), 0)

    def test_engine_check_refuses_a_binary_of_another_version(self):
        pin = engine_pin()
        self.assertNotEqual(self.run_check(pin, "9.9.9"), 0)
        self.assertNotEqual(self.run_check(pin, upstream(pin) + "1"), 0)

    def test_upstream_version(self):
        self.assertEqual(upstream("5.7.1-0"), "5.7.1")
        self.assertEqual(upstream("5:29.9.3-2"), "29.9.3")
        self.assertEqual(upstream("5.7.1"), "5.7.1")


if __name__ == "__main__":
    unittest.main()
