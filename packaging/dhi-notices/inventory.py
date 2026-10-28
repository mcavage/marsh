#!/usr/bin/env python3
"""Inventory built marsh images for the canonical notice bundle; host-only, read-only.

For each local image (an image-stage build of a Kit, the fixture or the shell
VM template, given as NAME=REFERENCE), this streams `docker export` of an
unstarted container and never runs image code. It writes:

- installed-package-notices.json: one row per installed package that ships no
  Debian copyright file (the DHI-built Engine, Node.js and Python packages):
  its own in-image legal documents (including those inside the wheels it
  installs), or those of the packages built from the same source at the same
  version, and the upstream texts at the exact source revision. Rows in
  --previous are reused for the identical package version; a new version of a
  package in upstream-sources.json has its upstream texts acquired from the
  release tag's commit;
- go-input.json: the Go dependency modules of every Go binary the OS packages
  install, with each binary's path and hash (collect-go.py acquires their
  notices);
- main-component-notices.json: a component compiled into a package (runc in
  containerd.io) follows the commit its binary embeds;
- source-records/dhi-base-image-legal.json: the DHI image-level license file
  each image carries in place;
- image-packages.json: every installed package of every image with its
  license families (license_families.py), for the license-delta report.

A package without its own notice and with no reviewed or acquirable upstream
texts fails the run. Results are evidence inputs, not a claim of complete legal
compliance.
"""
import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import subprocess
import sys
import tarfile
import zipfile
import io

from license_families import copyright_families, text_families
from notice_rules import notice_document
import urllib.error
import urllib.request
import re

HERE = Path(__file__).resolve().parent
COLLECTED = HERE / "collected"
GO_MAGIC = b"\xff Go buildinf:"
GO_SCAN = ("usr/bin/", "usr/sbin/", "usr/libexec/", "usr/lib/docker/")
LEGAL_FILE = "opt/docker/.license.txt"
TEXT_LIMIT = 16 * 1024 * 1024


def stanzas(data):
    for block in data.decode("utf-8", "replace").split("\n\n"):
        fields, key = {}, None
        for line in block.splitlines():
            if line[:1] in (" ", "\t") and key:
                continue
            if ":" in line:
                key, value = line.split(":", 1)
                fields[key] = value.strip()
        if "Package" in fields:
            yield fields


def varint(data, offset):
    value = shift = 0
    while True:
        byte = data[offset]
        offset += 1
        value |= (byte & 0x7F) << shift
        if byte < 0x80:
            return value, offset
        shift += 7


def parse_buildinfo(data):
    start = data.find(GO_MAGIC)
    header = data[start:start + 32]
    if len(header) < 32 or not header[15] & 2:
        raise ValueError("unsupported Go build information format")
    length, offset = varint(data, start + 32)
    offset += length  # Go version
    length, offset = varint(data, offset)
    info = data[offset:offset + length]
    if len(info) > 32:
        info = info[16:-16]
    modules, last = [], None
    for line in info.decode("utf-8", "replace").splitlines():
        fields = line.split("\t")
        if fields[0] == "dep" and len(fields) >= 3:
            last = {"path": fields[1], "version": fields[2]}
            modules.append(last)
        elif fields[0] == "=>" and len(fields) >= 3 and last is not None:
            last["path"], last["version"] = fields[1], fields[2]
    return modules


class Exported:
    """One pass over `docker export`; keeps only what the inventory needs."""

    def __init__(self, docker, image):
        container = subprocess.check_output([docker, "create", image], text=True).strip()
        try:
            process = subprocess.Popen([docker, "export", container], stdout=subprocess.PIPE)
            self.members, self.dpkg, self.legal, self.go, self.license = {}, {}, {}, {}, None
            self.wheels, self.commits = {}, {}
            with tarfile.open(fileobj=process.stdout, mode="r|") as archive:
                for member in archive:
                    name = member.name.lstrip("./")
                    self.members[name] = (member.type, member.linkname)
                    if not member.isfile():
                        continue
                    wanted = (name.startswith("var/lib/dpkg/") or name == LEGAL_FILE or name.endswith(".whl")
                              or (notice_document(name) and member.size <= TEXT_LIMIT)
                              or (name.startswith(GO_SCAN) and member.mode & 0o111))
                    if not wanted:
                        continue
                    data = archive.extractfile(member).read()
                    if name.startswith("var/lib/dpkg/"):
                        self.dpkg[name] = data
                    if name.endswith(".whl"):
                        self.wheels[name] = data
                    if name == LEGAL_FILE:
                        self.license = hashlib.sha256(data).hexdigest()
                    if notice_document(name) and member.size <= TEXT_LIMIT:
                        self.legal[name] = data
                    if name.startswith(GO_SCAN) and member.mode & 0o111 and GO_MAGIC in data:
                        self.go[name] = (hashlib.sha256(data).hexdigest(), parse_buildinfo(data))
                        self.commits[name] = sorted(set(re.findall(
                            rb"(?<![0-9a-f])[0-9a-f]{40}(?![0-9a-f])", data)))
            if process.wait():
                raise RuntimeError("docker export failed for " + image)
        finally:
            subprocess.run([docker, "rm", container], check=True, stdout=subprocess.DEVNULL)

    def resolve(self, name):
        parts, current, hops = list(PurePosixPath(name).parts), PurePosixPath(""), 0
        while parts:
            part = parts.pop(0)
            if part in ("", ".", "/"):
                continue
            if part == "..":
                current = current.parent
                continue
            candidate = current / part
            kind = self.members.get(str(candidate))
            if kind is None:
                return None
            if kind[0] == tarfile.SYMTYPE:
                hops += 1
                if hops > 40:
                    raise ValueError("symlink loop: " + name)
                target = PurePosixPath(kind[1])
                if target.is_absolute():
                    current, parts = PurePosixPath(""), list(target.parts[1:]) + parts
                else:
                    parts = list(target.parts) + parts
                continue
            current = candidate
        return str(current)

    def packages(self):
        """{package: (version, source package)} from dpkg status, then DHI status.d."""
        result = {}

        def record(fields):
            source = fields.get("Source", fields["Package"]).split(" ")[0]
            result.setdefault(fields["Package"], (fields["Version"], source))

        if "var/lib/dpkg/status" in self.dpkg:
            for fields in stanzas(self.dpkg["var/lib/dpkg/status"]):
                if fields.get("Status", "").split()[-1:] == ["installed"]:
                    record(fields)
        for name, data in sorted(self.dpkg.items()):
            if name.startswith("var/lib/dpkg/status.d/") and not name.endswith(".md5sums"):
                for fields in stanzas(data):
                    record(fields)
        return result

    def copyright(self, package):
        resolved = self.resolve(f"usr/share/doc/{package}/copyright")
        return self.legal.get(resolved) if resolved else None

    def has_copyright(self, package):
        resolved = self.resolve(f"usr/share/doc/{package}/copyright")
        return resolved is not None and self.members[resolved][0] in (tarfile.REGTYPE, tarfile.AREGTYPE)

    def listed(self, package):
        for name in (f"var/lib/dpkg/info/{package}.list", f"var/lib/dpkg/info/{package}:arm64.list"):
            if name in self.dpkg:
                return [line for line in self.dpkg[name].decode().splitlines() if line not in ("", "/.")]
        return []


def store(text, data):
    digest = hashlib.sha256(data).hexdigest()
    destination = COLLECTED / "texts" / (digest + ".txt")
    if destination.exists():
        if destination.read_bytes() != data:
            raise ValueError("text collision: " + digest)
    else:
        destination.write_bytes(data)
    return {"path": "/" + text, "sha256": digest, "file": "texts/" + destination.name}


def fetch(url):
    request = urllib.request.Request(url, headers={"User-Agent": "marsh-notice-inventory"})
    with urllib.request.urlopen(request, timeout=60) as response:
        data = response.read(TEXT_LIMIT + 1)
    if len(data) > TEXT_LIMIT:
        raise ValueError("oversized upstream text: " + url)
    return data


def tag_commit(repository, tag):
    """The commit a release tag names (peeled), from the public Git server."""
    output = subprocess.check_output(
        ["git", "ls-remote", f"https://github.com/{repository}", f"refs/tags/{tag}", f"refs/tags/{tag}^{{}}"],
        text=True, timeout=120)
    refs = dict(reversed(line.split("\t")) for line in output.splitlines() if line)
    commit = refs.get(f"refs/tags/{tag}^{{}}") or refs.get(f"refs/tags/{tag}")
    if not commit or not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError(f"no release tag {tag} in {repository}")
    return commit


def upstream_version(version):
    return version.split(":", 1)[-1].rsplit("-", 1)[0]


def acquire_upstream(source, version):
    commit = tag_commit(source["repository"], source["tag"].format(upstream=upstream_version(version)))
    notices = []
    for member in source["files"]:
        url = f"https://raw.githubusercontent.com/{source['repository']}/{commit}/{member}"
        row = store("", fetch(url))
        notices.append({"url": url, "sha256": row["sha256"], "file": row["file"]})
    return notices


def follow_component(row, exported):
    """A component compiled into a package follows the commit its binary embeds."""
    name = row["binary"].lstrip("/")
    if name not in exported.commits:
        return False
    candidates = [c.decode() for c in exported.commits[name]]
    if row["revision"] in candidates:
        return False
    resolved = []
    for commit in candidates:
        try:
            texts = {member: fetch(f"https://raw.githubusercontent.com/{row['repository']}/{commit}/{member}")
                     for member in row["files"]}
        except urllib.error.HTTPError:
            continue
        resolved.append((commit, texts))
    if len(resolved) != 1:
        raise ValueError(f"{row['package']}: cannot identify the source commit {row['binary']} embeds")
    commit, texts = resolved[0]
    notices = []
    for member, data in texts.items():
        stored = store("", data)
        notices.append({"url": f"https://raw.githubusercontent.com/{row['repository']}/{commit}/{member}",
                        "sha256": stored["sha256"], "file": stored["file"]})
    row["revision"] = commit
    row["notices"] = notices
    row["required_notices"] = [{"file": n["file"], "sha256": n["sha256"]} for n in notices]
    return True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--docker", default="docker")
    parser.add_argument("--previous", type=Path, default=COLLECTED / "installed-package-notices.json",
                        help="reviewed installed-package-notices rows to reuse by exact version")
    parser.add_argument("images", nargs="+", help="NAME=REFERENCE of each local image to inventory")
    args = parser.parse_args()
    previous = json.loads(args.previous.read_text())
    seeds = {(row["package"], row["version"]): row for row in previous["packages"]}
    sources = json.loads((HERE / "upstream-sources.json").read_text())["packages"]
    components_path = COLLECTED / "main-component-notices.json"
    components = json.loads(components_path.read_text())
    rows, binaries, modules, legal, catalog, names = {}, {}, {}, [], {}, []
    for argument in args.images:
        image, _, reference = argument.partition("=")
        if not image or not reference:
            raise ValueError("images are NAME=REFERENCE: " + argument)
        names.append(image)
        exported = Exported(args.docker, reference)
        if exported.license is None:
            raise ValueError(f"{image}: no DHI image-level license at /{LEGAL_FILE}")
        legal.append({"image": image, "path": "/" + LEGAL_FILE, "sha256": exported.license})
        installed = exported.packages()

        def own_notices(package):
            notices, archived = [], []
            for path in exported.listed(package):
                resolved = exported.resolve(path.lstrip("/"))
                if resolved in exported.legal and notice_document(path.lstrip("/")):
                    notices.append(dict(store(resolved, exported.legal[resolved]), path=path))
                if resolved in exported.wheels:
                    with zipfile.ZipFile(io.BytesIO(exported.wheels[resolved])) as wheel:
                        for member in sorted(wheel.namelist()):
                            if notice_document(member) and not member.endswith("/"):
                                archived.append(dict(store(resolved, wheel.read(member)), path=path,
                                                     member=member))
            return notices, archived

        families = {}
        for package, (version, source) in sorted(installed.items()):
            data = exported.copyright(package)
            if data is not None:
                families[package] = {"package": package, "version": version,
                                     "families": sorted(copyright_families(data)),
                                     "notices": [hashlib.sha256(data).hexdigest()]}
                continue
            notices, archived = own_notices(package)
            if not notices and not archived:
                # Packages built from one source share its legal files.
                for sibling, (sibling_version, sibling_source) in sorted(installed.items()):
                    if sibling != package and sibling_source == source and sibling_version == version \
                            and exported.copyright(sibling) is None:
                        notices = own_notices(sibling)[0]
                        if notices:
                            break
            seed = seeds.get((package, version))
            if seed is not None and seed["upstream_notices"]:
                upstream = seed["upstream_notices"]
            elif not notices and not archived and package in sources:
                upstream = acquire_upstream(sources[package], version)
            else:
                upstream = []
            if not notices and not archived and not upstream:
                raise ValueError(f"{image}: {package} {version} ships no notice and has no upstream "
                                 "source in upstream-sources.json")
            row = {"package": package, "version": version,
                   "notices": sorted(notices, key=lambda n: n["path"]),
                   "archive_notices": archived, "upstream_notices": upstream}
            if rows.setdefault((package, version), row) != row:
                raise ValueError(f"{package} {version}: notices differ between images")
            texts = [n["sha256"] for n in notices + archived + upstream]
            found = set()
            for digest in texts:
                found |= text_families((COLLECTED / "texts" / (digest + ".txt")).read_bytes())
            families[package] = {"package": package, "version": version, "families": sorted(found),
                                 "notices": sorted(set(texts))}
        catalog[image] = [families[p] for p in sorted(families)]
        for row in components:
            if "binary" in row:
                follow_component(row, exported)
        for name, (digest, deps) in sorted(exported.go.items()):
            path = "/" + name
            if binaries.setdefault(path, digest) != digest:
                raise ValueError(f"{path}: differing Go binaries across images")
            for dep in deps:
                key = (dep["path"], dep["version"])
                modules.setdefault(key, set()).add(path)
        print(json.dumps({"image": image, "packages": len(installed), "indexed_packages": len(rows),
                          "go_binaries": len(exported.go)}), file=sys.stderr)
    document = {
        "schema": 2,
        "scope": "Installed packages that ship no /usr/share/doc/PACKAGE/copyright file, by exact version: their own in-image legal documents and the upstream legal texts retained for them. Every other installed package carries its own Debian copyright file in the image.",
        "images": names,
        "packages": [rows[key] for key in sorted(rows)],
    }
    write(COLLECTED / "installed-package-notices.json", document)
    write(components_path, components)
    write(HERE / "go-input.json", {
        "images": names,
        "scope": "Go dependency modules from actual compiled binary build information of OS-package Go binaries; main components are indexed with their packages",
        "binaries": [{"path": path, "sha256": binaries[path]} for path in sorted(binaries)],
        "modules": [{"path": path, "version": version, "binaries": sorted(paths)}
                    for (path, version), paths in sorted(modules.items())],
    })
    write(COLLECTED / "source-records" / "dhi-base-image-legal.json", {
        "schema": 1,
        "scope": "The DHI image-level license file each built image carries in place, observed by inventory.py from docker export; it covers DHI definitions, patches and build scripts, not the included packages.",
        "observations": legal,
    })
    write(HERE / "image-packages.json", {
        "schema": 1,
        "scope": "Every installed package of every image, with the license families of its notices (license_families.py). Report input; the image verifier does not read it.",
        "images": catalog,
    })


def write(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


if __name__ == "__main__":
    main()
