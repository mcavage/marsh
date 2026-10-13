#!/usr/bin/env python3
"""Offline payload/notice correspondence, not a redistribution or source-completeness grant.

Checks one assembled image root (default /; a read-only bind of the image stage
for runtime images that ship no Python) against this canonical bundle, which
must be installed in that root at usr/local/share/licenses/marsh-dhi.
"""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import stat
import tempfile
import zipfile

JSON_LIMIT = 32 * 1024**2
FILE_LIMIT = 1024**3
TREE_LIMIT = 256 * 1024**2
COUNT_LIMIT = 20000
RECEIPT = "image-verification.json"
BUNDLE = PurePosixPath("usr/local/share/licenses/marsh-dhi")
GO_MAGIC = b"\xff Go buildinf:"
# OS-package executable locations scanned for Go build information. Agent and
# npm trees carry their own indexed payloads and are not OS packages.
GO_SCAN = ("usr/bin", "usr/sbin", "usr/libexec", "usr/lib/docker")
LINK_LIMIT = 40


def identity(st):
    return (
        st.st_dev,
        st.st_ino,
        st.st_mode,
        st.st_uid,
        st.st_gid,
        st.st_size,
        st.st_mtime_ns,
        st.st_ctime_ns,
    )


def regular(path, limit=FILE_LIMIT):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    st = os.fstat(fd)
    if not stat.S_ISREG(st.st_mode) or st.st_size > limit:
        os.close(fd)
        raise ValueError("not a bounded regular file: " + str(path))
    return fd, st


def read_bytes(path, limit=JSON_LIMIT):
    fd, before = regular(path, limit)
    with os.fdopen(fd, "rb") as source:
        data = source.read(limit + 1)
        if len(data) != before.st_size or identity(os.fstat(source.fileno())) != identity(
            before
        ):
            raise ValueError("file changed: " + str(path))
    return data


def sha(path):
    h = hashlib.sha256()
    fd, before = regular(path)
    with os.fdopen(fd, "rb") as source:
        count = 0
        while chunk := source.read(1024 * 1024):
            count += len(chunk)
            if count > before.st_size:
                raise ValueError("file grew: " + str(path))
            h.update(chunk)
        if count != before.st_size or identity(os.fstat(source.fileno())) != identity(
            before
        ):
            raise ValueError("file changed: " + str(path))
    return h.hexdigest()


def unique(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON key: " + key)
        result[key] = value
    return result


def load(path):
    return json.loads(read_bytes(path), object_pairs_hook=unique)


class Image:
    """Absolute image paths, resolved inside ROOT; symlinks never escape it."""

    def __init__(self, root):
        self.root = Path(os.path.abspath(root))
        if not stat.S_ISDIR(self.root.lstat().st_mode):
            raise ValueError("image root is not a directory")

    def path(self, absolute):
        p = PurePosixPath(absolute)
        if not p.is_absolute() or ".." in p.parts:
            raise ValueError("invalid image path: " + str(absolute))
        return self.root.joinpath(*p.parts[1:])

    def resolve(self, absolute):
        """The image-absolute path ABSOLUTE names after every symlink, or None."""
        pending = list(PurePosixPath(absolute).parts[1:])
        current = PurePosixPath("/")
        hops = 0
        while pending:
            part = pending.pop(0)
            if part in ("", "."):
                continue
            if part == "..":
                current = current.parent
                continue
            candidate = current / part
            try:
                metadata = self.path(candidate).lstat()
            except FileNotFoundError:
                return None
            if stat.S_ISLNK(metadata.st_mode):
                hops += 1
                if hops > LINK_LIMIT:
                    raise ValueError("symlink loop: " + str(absolute))
                target = PurePosixPath(os.readlink(self.path(candidate)))
                pending = list(target.parts[1:] if target.is_absolute() else target.parts) + pending
                if target.is_absolute():
                    current = PurePosixPath("/")
                continue
            current = candidate
        return current

    def no_link_parents(self, absolute):
        for parent in self.path(absolute).parents:
            if parent == self.root.parent:
                break
            if not stat.S_ISDIR(parent.lstat().st_mode):
                raise ValueError("non-directory/symlink payload ancestor: " + str(parent))
            if parent == self.root:
                break


def tree(root):
    """No symlink recursion; a bound applies to directories as well as files."""
    pending, result = [root], {}
    while pending:
        parent = pending.pop()
        for child in sorted(parent.iterdir()):
            result[str(child)] = child.lstat()
            if len(result) > COUNT_LIMIT:
                raise ValueError("tree entry budget exceeded")
            if stat.S_ISDIR(result[str(child)].st_mode):
                pending.append(child)
    return result


def bundle_tree(root):
    result, total = {}, 0
    for name, metadata in tree(root).items():
        path = Path(name)
        relative = str(path.relative_to(root))
        if relative == RECEIPT:
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
                raise ValueError("unsafe existing receipt")
            continue
        if stat.S_ISDIR(metadata.st_mode):
            continue
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
            raise ValueError("nonregular canonical input: " + relative)
        total += metadata.st_size
        if total > TREE_LIMIT:
            raise ValueError("bundle byte budget exceeded")
        result[relative] = {
            "sha256": sha(path),
            "size": metadata.st_size,
            "mode": stat.S_IMODE(metadata.st_mode),
        }
    encoded = json.dumps(result, sort_keys=True, separators=(",", ":")).encode()
    return result, hashlib.sha256(encoded).hexdigest()


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


def installed_packages(image):
    """Every installed package: dpkg status (installed only), then DHI status.d."""
    packages, architectures = {}, set()
    status = image.path("/var/lib/dpkg/status")
    if os.path.lexists(status):
        for fields in stanzas(read_bytes(status)):
            if fields.get("Status", "").split()[-1:] == ["installed"]:
                packages[fields["Package"]] = fields["Version"]
                architectures.add(fields.get("Architecture"))
    directory = image.path("/var/lib/dpkg/status.d")
    if os.path.lexists(directory):
        for entry in sorted(directory.iterdir()):
            if entry.name.endswith(".md5sums"):
                continue
            for fields in stanzas(read_bytes(entry)):
                # A dpkg-managed status file is authoritative for what it
                # lists; status.d adds the packages only it records.
                packages.setdefault(fields["Package"], fields["Version"])
                architectures.add(fields.get("Architecture"))
    if not packages or len(packages) > COUNT_LIMIT:
        raise ValueError("installed package inventory missing or unbounded")
    architectures.discard("all")
    architectures.discard(None)
    if len(architectures) != 1:
        raise ValueError("installed packages do not share one architecture")
    return packages, architectures.pop()


def go_binaries(image):
    found = {}
    for top in GO_SCAN:
        base = image.path("/" + top)
        if not os.path.lexists(base) or not stat.S_ISDIR(base.lstat().st_mode):
            continue
        for name, metadata in tree(base).items():
            if not stat.S_ISREG(metadata.st_mode) or not metadata.st_mode & 0o111:
                continue
            fd, _ = regular(name)
            with os.fdopen(fd, "rb") as source:
                carry = b""
                while chunk := source.read(4 * 1024 * 1024):
                    if GO_MAGIC in carry + chunk:
                        found["/" + str(Path(name).relative_to(image.root))] = sha(name)
                        break
                    carry = chunk[-len(GO_MAGIC):]
    return found


def check_entry(image, row):
    absolute = row["path"]
    image.no_link_parents(absolute)
    path = image.path(absolute)
    metadata = path.lstat()
    if (metadata.st_uid, metadata.st_gid) != (row["uid"], row["gid"]):
        raise ValueError(
            f"payload ownership changed: {absolute}: expected {row['uid']}:{row['gid']}, got {metadata.st_uid}:{metadata.st_gid}"
        )
    if stat.S_IMODE(metadata.st_mode) != row["mode"]:
        raise ValueError(
            f"payload mode changed: {absolute}: expected {row['mode']:04o}, got {stat.S_IMODE(metadata.st_mode):04o}"
        )
    kind = row["type"]
    if kind == "file":
        if (
            not stat.S_ISREG(metadata.st_mode)
            or metadata.st_size != row["size"]
            or sha(path) != row["sha256"]
        ):
            raise ValueError("payload file changed: " + absolute)
    elif kind == "symlink":
        if not stat.S_ISLNK(metadata.st_mode) or os.readlink(path) != row["target"]:
            raise ValueError("payload symlink changed: " + absolute)
    elif kind != "directory" or not stat.S_ISDIR(metadata.st_mode):
        raise ValueError("payload type changed: " + absolute)


def verify_adapter(image, policy, architecture, require):
    """Full selected native package bytes; other npm metadata/notices only."""
    root = policy["root"]
    lock = image.path(root + "/package-lock.json")
    if sha(lock) != policy["lock_sha256"]:
        raise ValueError("adapter lock changed: " + str(lock))
    selected = policy["architectures"][architecture]
    native = []
    for package in selected["packages"]:
        directory = image.path(package["root"])
        for entry in package["payload_entries"]:
            check_entry(image, entry)
        actual = {"/" + str(Path(p).relative_to(image.root)) for p in tree(directory)}
        if actual | {package["root"]} != {r["path"] for r in package["payload_entries"]}:
            raise ValueError(
                "unindexed or missing selected adapter package: " + package["root"]
            )
        document = load(directory / "package.json")
        if (document.get("name"), document.get("version")) != (
            package["package"],
            package["version"],
        ):
            raise ValueError("selected adapter package identity changed")
        native.append(
            {
                "path": package["root"],
                "version": package["version"],
                "verified_entries": len(package["payload_entries"]),
            }
        )
    for entry in selected["links"]:
        check_entry(image, entry)
    if selected["required_notices"]:
        require(selected["required_notices"])
    notice_root = image.path(policy["notice_root"])
    inventory_path = notice_root / "npm-package-notices.json"
    document = load(inventory_path)
    if (
        document.get("schema") != "marsh.installed-npm-notices.v1"
        or document.get("package_lock_sha256") != policy["lock_sha256"]
        or document.get("platform") != "linux"
        or document.get("architecture")
        != ("arm64" if architecture == "arm64" else "x64")
    ):
        raise ValueError("adapter installed notice inventory/lock mismatch")
    packages = document["packages"]
    if not packages or len(packages) > COUNT_LIMIT:
        raise ValueError("adapter package inventory bound")
    seen = set()
    for package in packages:
        relative = PurePosixPath(package["path"])
        if relative.is_absolute() or ".." in relative.parts or not relative.parts:
            raise ValueError("unsafe adapter package path")
        if str(relative) in seen:
            raise ValueError("duplicate adapter package inventory entry")
        seen.add(str(relative))
        absolute = f"{root}/node_modules/{relative}/package.json"
        image.no_link_parents(absolute)
        path = image.path(absolute)
        if sha(path) != package["package_json_sha256"]:
            raise ValueError("adapter package metadata changed: " + str(relative))
        actual = load(path)
        if (actual.get("name"), actual.get("version")) != (
            package["name"],
            package["version"],
        ):
            raise ValueError("adapter installed identity changed")
        if not package["notices"]:
            raise ValueError("adapter package has no retained notice")
        for notice in package["notices"]:
            digest = notice["sha256"]
            if len(digest) != 64 or any(c not in "0123456789abcdef" for c in digest):
                raise ValueError("invalid adapter notice digest")
            text = notice_root / "texts" / (digest + ".txt")
            if sha(text) != digest:
                raise ValueError("adapter retained notice changed")
    for package in selected["packages"]:
        expected = str(PurePosixPath(package["root"]).relative_to(root + "/node_modules"))
        if expected not in seen:
            raise ValueError(
                "selected native package absent from installed notice inventory"
            )
    return {
        "root": root,
        "lock_sha256": policy["lock_sha256"],
        "full_payload_packages": native,
        "other_npm_package_metadata_and_notices": len(packages),
        "collector_inventory_sha256": sha(inventory_path),
        "scope": "Full member/type/mode/owner and required legal files for listed selected native packages. Other npm package manifests/retained notice bytes are checked, NOT their full executable trees or rights/link completeness. No agent execution.",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", default="/", help="assembled image root (default /)")
    parser.add_argument(
        "--profile", choices=["base", "claude-kit", "codex-kit"], default="base"
    )
    parser.add_argument(
        "--version", help="Required exact version for a native Kit profile"
    )
    parser.add_argument(
        "--adapter",
        choices=["codex-acp", "claude-acp", "pi"],
        help="Selected ACP/npm payload; may accompany the matching Kit profile",
    )
    parser.add_argument(
        "--receipt-dir", type=Path, help="write image-verification.json here"
    )
    args = parser.parse_args()
    if (args.profile == "base") != (args.version is None):
        parser.error("a native Kit profile requires --version, and only a profile takes one")
    kit_adapters = {"claude-kit": "claude-acp", "codex-kit": "codex-acp"}
    if args.adapter and args.profile != "base" and kit_adapters[args.profile] != args.adapter:
        parser.error("adapter scope must match its Kit profile")
    image = Image(args.root)
    root = image.path("/" + str(BUNDLE))
    if Path(__file__).resolve() != (root / "verify.py").resolve():
        raise ValueError("verifier must run from the bundle installed in the image root")
    image.no_link_parents("/" + str(BUNDLE / "verify.py"))
    before, bundle_hash = bundle_tree(root)
    packages, architecture = installed_packages(image)
    indexed = {}

    def require(notices):
        if not isinstance(notices, list) or not notices:
            raise ValueError("missing mandatory required_notices")
        for notice in notices:
            filename = notice["file"]
            p = PurePosixPath(filename)
            if (
                p.is_absolute()
                or ".." in p.parts
                or len(p.parts) != 2
                or p.parts[0] not in ("texts", "provider-notices")
            ):
                raise ValueError("unsafe notice reference")
            if (
                p.parts[0] == "texts"
                and filename != "texts/" + notice["sha256"] + ".txt"
            ):
                raise ValueError("notice content-addressed filename changed")
            if before.get(filename, {}).get("sha256") != notice["sha256"]:
                raise ValueError("required notice missing/changed: " + filename)
            if filename in indexed and indexed[filename] != notice["sha256"]:
                raise ValueError("conflicting notice references")
            indexed[filename] = notice["sha256"]

    # OS packages: the package's own Debian copyright file, or (DHI-built
    # packages ship none) the indexed notices for that exact version.
    inventory = load(root / "installed-package-notices.json")
    if inventory.get("schema") != 2:
        raise ValueError("unsupported installed package notice inventory")
    rows = {(r["package"], r["version"]): r for r in inventory["packages"]}
    if len(rows) != len(inventory["packages"]):
        raise ValueError("duplicate installed package notice row")
    debian_copyright, indexed_packages = 0, {}
    for package, version in sorted(packages.items()):
        copyright = image.resolve(f"/usr/share/doc/{package}/copyright")
        if copyright and stat.S_ISREG(image.path(copyright).lstat().st_mode):
            debian_copyright += 1
            continue
        row = rows.get((package, version))
        if row is None:
            raise ValueError(
                f"installed package has no notice: {package} {version} "
                "(no /usr/share/doc copyright file and no indexed row)"
            )
        for notice in row["notices"]:
            path = image.resolve(notice["path"])
            if path is None or sha(image.path(path)) != notice["sha256"]:
                raise ValueError(f"package notice missing/changed: {package}: {notice['path']}")
        for notice in row["archive_notices"]:
            path = image.resolve(notice["path"])
            if path is None:
                raise ValueError(f"package archive missing: {package}: {notice['path']}")
            with zipfile.ZipFile(io.BytesIO(read_bytes(image.path(path), FILE_LIMIT))) as archive:
                if hashlib.sha256(archive.read(notice["member"])).hexdigest() != notice["sha256"]:
                    raise ValueError(f"package archive notice changed: {package}: {notice['member']}")
        if not (row["notices"] or row["archive_notices"] or row["upstream_notices"]):
            raise ValueError("indexed package row has no notice: " + package)
        indexed_packages[package] = version
    references = []
    for row in inventory["packages"]:
        references.extend(row["notices"] + row["archive_notices"] + row["upstream_notices"])

    # Go dependency modules of the OS packages' compiled Go binaries.
    modules = load(root / "go-module-notices.json")
    supplements = load(root / "upstream-license-supplements.json")
    if modules["failures"] or set(modules["missing_notice"]) != {
        row["module"] for row in supplements
    }:
        raise ValueError("unresolved Go module notice collection")
    recorded = {row["path"]: row for row in modules["binaries"]}
    observed = go_binaries(image)
    for path, digest in observed.items():
        if path not in recorded:
            raise ValueError("Go binary without indexed module notices: " + path)
        if recorded[path]["sha256"] != digest:
            raise ValueError("Go binary identity changed: " + path)
    for row in modules["modules"]:
        references.extend(row["notices"])
    references.extend(supplements)

    # Image-level legal files the DHI base ships in place.
    image_primary = load(root / "image-primary-notices.json")
    image_files = []
    for component in image_primary:
        for row in component["image_files"]:
            if sha(image.path(row["path"])) != row["sha256"]:
                raise ValueError("image-level legal identity changed: " + row["path"])
            image_files.append(row["path"])
    components = (
        image_primary
        + load(root / "main-component-notices.json")
        + load(root / "public-component-notices.json")
        + load(root / "native-build-notices.json")
    )
    for row in components:
        references.extend(row["notices"])
    for name in ("rust-notices.json", "selected-rust-notices.json"):
        for collection in load(root / name)["collections"]:
            source_name = collection["source_inventory"]
            if (
                not source_name.startswith("source-records/")
                or ".." in PurePosixPath(source_name).parts
            ):
                raise ValueError("unsafe Rust source inventory reference")
            if (
                before.get(source_name, {}).get("sha256")
                != collection["source_inventory_sha256"]
            ):
                raise ValueError("Rust source inventory identity changed")
            source = load(root / source_name)
            rows_ = (
                source
                if isinstance(source, list)
                else source.get("archives", source.get("supplements"))
            )
            acquired = [notice for row in rows_ for notice in row["notices"]]
            if acquired != collection["notices"]:
                raise ValueError(
                    "Rust notice correspondence changed from exact acquired source"
                )
            references.extend(acquired)
    primary = load(root / "primary-source-notices.json")
    for collection in primary["collections"]:
        name = collection["source_inventory"]
        if not name.startswith("source-records/") or ".." in PurePosixPath(name).parts:
            raise ValueError("unsafe primary source inventory path")
        if before.get(name, {}).get("sha256") != collection["source_inventory_sha256"]:
            raise ValueError("primary source inventory changed: " + name)
        source = load(root / name)
        if isinstance(source, list):
            acquired = source
        elif "notices" in source:
            acquired = source["notices"]
        else:
            rows_ = source.get(
                "archives",
                source.get(
                    "packages", source.get("components", source.get("supplements"))
                ),
            )
            acquired = [notice for row in rows_ for notice in row["notices"]]
        if acquired != collection["notices"]:
            raise ValueError("primary notice/source correspondence changed: " + name)
        references.extend(acquired)
    adapters = load(root / "adapter-payloads.json")["adapters"]
    for adapter in adapters.values():
        for selected in adapter["architectures"].values():
            for package in selected["packages"]:
                references.extend(package["notices"])
    for reference in references:
        require([reference])
    agent_notices = load(root / "agent-notices.json")
    for row in agent_notices["notices"]:
        require([dict(row, file="provider-notices/" + row["file"])])
    actual_texts = {
        name for name in before if name.startswith(("texts/", "provider-notices/"))
    }
    if set(indexed) != actual_texts:
        raise ValueError("unindexed or missing canonical notice text")
    # Independent source/member correspondence, not derived from the indexes:
    # dropping index rows and text files cannot discharge it.
    for component in components:
        require(component["required_notices"])
    require(primary["required_runtime_notices"])

    profile = None
    if args.profile != "base":
        profile = load(root / "derived-profiles.json")[args.profile].get(
            args.version, {}
        ).get(architecture)
        if not profile:
            raise ValueError("unreviewed selected Kit version/architecture")
        require(profile["required_notices"])
        for row in profile["files"]:
            check_entry(image, row)
        for scope in profile.get("exact_trees", []):
            entries = {"/" + str(Path(p).relative_to(image.root)) for p in tree(image.path(scope["path"]))}
            if entries != set(scope["entries"]):
                raise ValueError("unreviewed final Kit version tree")
    adapter_result = None
    if args.adapter:
        adapter_result = verify_adapter(image, adapters[args.adapter], architecture, require)
    after, _ = bundle_tree(root)
    if before != after:
        raise ValueError("canonical source changed during verification")
    result = {
        "schema": "marsh.dhi-notices-verification/v3",
        "platform": "linux/" + architecture,
        "profile": args.profile,
        "selected_version": args.version,
        "scope_kind": "native-kit+adapter"
        if args.adapter and profile
        else "adapter-final-scoped"
        if args.adapter
        else "native-kit"
        if profile
        else "image",
        "verifier_sha256": before["verify.py"]["sha256"],
        "bundle_tree_sha256": bundle_hash,
        "bundle_tree_hash_format": "SHA256 of compact sorted JSON {relative:{sha256,size,mode}}, excluding only the receipt",
        "runtime_image_id": None,
        "image_identity_boundary": "Host must bind this receipt to the actual image ID and the exact Dockerfile/tool/source hashes; this process cannot infer its own image digest.",
        "installed_packages": len(packages),
        "packages_with_debian_copyright": debian_copyright,
        "packages_with_indexed_notices": indexed_packages,
        "go_binaries": sorted(observed),
        "image_legal_files": image_files,
        "adapter_scope": adapter_result,
        "native_kit_files": [row["path"] for row in profile["files"]] if profile else [],
        "notice_texts": len(indexed),
        "qualification": "Not evaluated by this byte/metadata verifier. Runtime host gates, shipping notice materials and external redistribution permissions are distinct; see qualification-boundaries.json.",
        "notice_materials": primary["notice_material_status"],
        "external_assumptions": load(root / "qualification-boundaries.json"),
        "rust_notice_coverage": load(root / "rust-coverage-status.json"),
        "selected_rust_notice_coverage": load(
            root / "selected-rust-coverage-status.json"
        ),
    }
    data = (json.dumps(result, indent=2) + "\n").encode()
    if args.receipt_dir:
        args.receipt_dir.mkdir(parents=True, exist_ok=True)
        destination = args.receipt_dir / RECEIPT
        if os.path.lexists(destination):
            st = destination.lstat()
            if (
                not stat.S_ISREG(st.st_mode)
                or st.st_nlink != 1
                or st.st_uid != os.getuid()
            ):
                raise ValueError("unsafe receipt destination")
        descriptor, temporary = tempfile.mkstemp(prefix=".receipt-", dir=args.receipt_dir)
        try:
            with os.fdopen(descriptor, "wb") as out:
                out.write(data)
                out.flush()
                os.fsync(out.fileno())
            os.chmod(temporary, 0o644)
            os.replace(temporary, destination)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)
    print(data.decode(), end="")


if __name__ == "__main__":
    main()
