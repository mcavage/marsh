#!/usr/bin/env python3
"""Inventory built marsh images for the canonical notice bundle; host-only, read-only.

For each local image (an image-stage build of a Kit, the fixture or the shell
VM template), this streams `docker export` of an unstarted container and never
runs image code. It writes:

- installed-package-notices.json: one row per installed package that ships no
  Debian copyright file (the DHI-built Engine, Node.js and Python packages),
  with its own in-image legal documents (including those inside the wheels it
  installs) and the upstream texts retained for it in --previous (reused only
  for the identical package version);
- go-input.json: the Go dependency modules of every Go binary the OS packages
  install, with each binary's path and hash (collect-go.py acquires their
  notices);
- source-records/dhi-base-image-legal.json: the DHI image-level license file
  each image carries in place.

A package without its own notice and without a reviewed upstream row fails the
run: acquire its upstream legal texts first. Results are evidence inputs, not a
claim of complete legal compliance.
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

from notice_rules import notice_document

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
            self.wheels = {}
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
        result = {}
        if "var/lib/dpkg/status" in self.dpkg:
            for fields in stanzas(self.dpkg["var/lib/dpkg/status"]):
                if fields.get("Status", "").split()[-1:] == ["installed"]:
                    result[fields["Package"]] = fields["Version"]
        for name, data in sorted(self.dpkg.items()):
            if name.startswith("var/lib/dpkg/status.d/") and not name.endswith(".md5sums"):
                for fields in stanzas(data):
                    result.setdefault(fields["Package"], fields["Version"])
        return result

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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--docker", default="docker")
    parser.add_argument("--previous", type=Path, required=True,
                        help="reviewed installed-package-notices rows (upstream texts) to reuse by exact version")
    parser.add_argument("images", nargs="+", help="local image references to inventory")
    args = parser.parse_args()
    previous = json.loads(args.previous.read_text())
    seeds = {(row["package"], row["version"]): row for row in previous["packages"]}
    rows, binaries, modules, legal, observed = {}, {}, {}, [], []
    for image in args.images:
        image_id = subprocess.check_output([args.docker, "image", "inspect", "--format", "{{.Id}}", image],
                                           text=True).strip()
        observed.append({"image": image, "id": image_id})
        exported = Exported(args.docker, image)
        if exported.license is None:
            raise ValueError(f"{image}: no DHI image-level license at /{LEGAL_FILE}")
        legal.append({"image": image, "id": image_id, "path": "/" + LEGAL_FILE, "sha256": exported.license})
        for package, version in sorted(exported.packages().items()):
            if exported.has_copyright(package):
                continue
            notices = []
            for path in exported.listed(package):
                name = path.lstrip("/")
                resolved = exported.resolve(name)
                if resolved in exported.legal and notice_document(name):
                    row = store(resolved, exported.legal[resolved])
                    notices.append(dict(row, path=path))
            archived = []
            for path in exported.listed(package):
                resolved = exported.resolve(path.lstrip("/"))
                if resolved in exported.wheels:
                    with zipfile.ZipFile(io.BytesIO(exported.wheels[resolved])) as wheel:
                        for member in sorted(wheel.namelist()):
                            if notice_document(member) and not member.endswith("/"):
                                row = store(resolved, wheel.read(member))
                                archived.append(dict(row, path=path, member=member))
            seed = seeds.get((package, version), {})
            for notice in seed.get("notices", []):
                if not notice["path"].startswith("/"):
                    continue
                resolved = exported.resolve(notice["path"].lstrip("/"))
                if resolved in exported.legal and hashlib.sha256(exported.legal[resolved]).hexdigest() == notice["sha256"]:
                    if notice["path"] not in {n["path"] for n in notices}:
                        notices.append({k: notice[k] for k in ("path", "sha256", "file")})
            upstream = seed.get("upstream_notices", [])
            if not notices and not archived and not upstream:
                raise ValueError(f"{image}: {package} {version} ships no notice and has no reviewed upstream row")
            row = {"package": package, "version": version,
                   "notices": sorted(notices, key=lambda n: n["path"]),
                   "archive_notices": archived, "upstream_notices": upstream}
            if rows.setdefault((package, version), row) != row:
                raise ValueError(f"{package} {version}: notices differ between images")
        for name, (digest, deps) in sorted(exported.go.items()):
            path = "/" + name
            if binaries.setdefault(path, digest) != digest:
                raise ValueError(f"{path}: differing Go binaries across images")
            for dep in deps:
                key = (dep["path"], dep["version"])
                modules.setdefault(key, set()).add(path)
        print(json.dumps({"image": image, "indexed_packages": len(rows),
                          "go_binaries": len(exported.go)}), file=sys.stderr)
    document = {
        "schema": 2,
        "scope": "Installed packages that ship no /usr/share/doc/PACKAGE/copyright file, by exact version: their own in-image legal documents and the upstream legal texts retained for them. Every other installed package carries its own Debian copyright file in the image.",
        "images": observed,
        "packages": [rows[key] for key in sorted(rows)],
    }
    (COLLECTED / "installed-package-notices.json").write_text(json.dumps(document, indent=2) + "\n")
    go_input = {
        "images": observed,
        "scope": "Go dependency modules from actual compiled binary build information of OS-package Go binaries; main components are indexed with their packages",
        "binaries": [{"path": path, "sha256": binaries[path]} for path in sorted(binaries)],
        "modules": [{"path": path, "version": version, "binaries": sorted(paths)}
                    for (path, version), paths in sorted(modules.items())],
    }
    (HERE / "go-input.json").write_text(json.dumps(go_input, indent=2) + "\n")
    (COLLECTED / "source-records" / "dhi-base-image-legal.json").write_text(json.dumps({
        "schema": 1,
        "scope": "The DHI image-level license file each built image carries in place, observed by inventory.py from docker export; it covers DHI definitions, patches and build scripts, not the included packages.",
        "observations": legal,
    }, indent=2) + "\n")


if __name__ == "__main__":
    main()
