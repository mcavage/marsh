#!/usr/bin/env python3
"""Offline payload/notice correspondence, not a redistribution or source-completeness grant."""

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import stat
import subprocess
import tempfile

JSON_LIMIT = 32 * 1024**2
FILE_LIMIT = 1024**3
TREE_LIMIT = 256 * 1024**2
COUNT_LIMIT = 20000
RECEIPTS = {"image-verification.json", "base-image-verification.json"}


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
    fd, before = regular(path, JSON_LIMIT)
    with os.fdopen(fd, "rb") as source:
        data = source.read(JSON_LIMIT + 1)
        if len(data) != before.st_size or identity(
            os.fstat(source.fileno())
        ) != identity(before):
            raise ValueError("JSON changed")
    return json.loads(data, object_pairs_hook=unique)


def no_link_parents(path):
    for parent in path.parents:
        if not stat.S_ISDIR(parent.lstat().st_mode):
            raise ValueError("non-directory/symlink payload ancestor: " + str(parent))


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
        if relative in RECEIPTS:
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


def check_entry(row, *, copied=False):
    path = Path(row["path"])
    if not path.is_absolute() or ".." in path.parts:
        raise ValueError("invalid payload path")
    no_link_parents(path)
    metadata = path.lstat()
    expected_uid, expected_gid = (0, 0) if copied else (row["uid"], row["gid"])
    if (metadata.st_uid, metadata.st_gid) != (expected_uid, expected_gid):
        raise ValueError(
            f"payload ownership changed: {path}: expected {expected_uid}:{expected_gid}, got {metadata.st_uid}:{metadata.st_gid}"
        )
    if stat.S_IMODE(metadata.st_mode) != row["mode"]:
        raise ValueError(
            f"payload mode changed: {path}: expected {row['mode']:04o}, got {stat.S_IMODE(metadata.st_mode):04o}"
        )
    kind = row["type"]
    if kind == "file":
        if (
            not stat.S_ISREG(metadata.st_mode)
            or metadata.st_size != row["size"]
            or sha(path) != row["sha256"]
        ):
            raise ValueError("payload file changed: " + str(path))
    elif kind == "symlink":
        if not stat.S_ISLNK(metadata.st_mode) or str(path.readlink()) != row["target"]:
            raise ValueError("payload symlink changed: " + str(path))
    elif kind != "directory" or not stat.S_ISDIR(metadata.st_mode):
        raise ValueError("payload type changed: " + str(path))


def verify_adapter(policy, architecture, require):
    """Full selected native package bytes; other npm metadata/notices only."""
    root = Path(policy["root"])
    lock = root / "package-lock.json"
    if sha(lock) != policy["lock_sha256"]:
        raise ValueError("adapter lock changed: " + str(lock))
    selected = policy["architectures"][architecture]
    native = []
    for package in selected["packages"]:
        directory = Path(package["root"])
        for entry in package["payload_entries"]:
            check_entry(entry)
        if set(tree(directory)) | {str(directory)} != {
            r["path"] for r in package["payload_entries"]
        }:
            raise ValueError(
                "unindexed or missing selected adapter package: " + str(directory)
            )
        actual = load(directory / "package.json")
        if (actual.get("name"), actual.get("version")) != (
            package["package"],
            package["version"],
        ):
            raise ValueError("selected adapter package identity changed")
        native.append(
            {
                "path": str(directory),
                "version": package["version"],
                "verified_entries": len(package["payload_entries"]),
            }
        )
    for entry in selected["links"]:
        check_entry(entry)
    if selected["required_notices"]:
        require(selected["required_notices"])
    notice_root = Path(policy["notice_root"])
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
        path = root / "node_modules" / str(relative) / "package.json"
        no_link_parents(path)
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
            path = notice_root / "texts" / (digest + ".txt")
            no_link_parents(path)
            if sha(path) != digest:
                raise ValueError("adapter retained notice changed")
    for package in selected["packages"]:
        expected = str(Path(package["root"]).relative_to(root / "node_modules"))
        if expected not in seen:
            raise ValueError(
                "selected native package absent from installed notice inventory"
            )
    return {
        "root": str(root),
        "lock_sha256": policy["lock_sha256"],
        "full_payload_packages": native,
        "other_npm_package_metadata_and_notices": len(packages),
        "collector_inventory_sha256": sha(inventory_path),
        "scope": "Full member/type/mode/owner and required legal files for listed selected native packages. Other npm package manifests/retained notice bytes are checked, NOT their full executable trees or rights/link completeness. No agent execution.",
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--receipt", action="store_true")
    parser.add_argument("--copied-agents", action="store_true")
    parser.add_argument(
        "--profile", choices=["base", "claude-kit", "codex-kit"], default="base"
    )
    parser.add_argument(
        "--version", help="Required exact version for a supported final Kit profile"
    )
    parser.add_argument("--stage", choices=["base", "final"], default=None)
    parser.add_argument(
        "--adapter",
        choices=["codex-acp", "claude-acp", "pi"],
        help="Selected ACP/npm payload; may accompany the matching Kit profile",
    )
    parser.add_argument(
        "--expect-agent",
        choices=["auto", "none", "claude", "codex"],
        default="auto",
        help="Require the known public base flavor instead of presence-only detection",
    )
    args = parser.parse_args()
    if (
        args.copied_agents
        and args.profile != "base"
        or (args.profile == "base") != (args.version is None)
    ):
        parser.error(
            "copied mode and Kit repin profiles are distinct; Kit profile requires --version"
        )
    # One agent Kit ships its native CLI and its ACP adapter, so a Kit profile
    # may carry that agent's adapter scope in the same final receipt.
    kit_adapters = {"claude-kit": "claude-acp", "codex-kit": "codex-acp"}
    if args.adapter and (
        args.copied_agents
        or args.profile != "base"
        and kit_adapters[args.profile] != args.adapter
    ):
        parser.error("adapter scope must match its Kit profile; copied agents are separate")
    if args.stage is None:
        args.stage = (
            "final"
            if args.adapter or args.copied_agents or args.profile != "base"
            else "base"
        )
    root = Path(__file__).absolute().parent
    no_link_parents(root / "verify.py")
    before, bundle_hash = bundle_tree(root)
    architecture = subprocess.check_output(
        ["dpkg", "--print-architecture"], text=True, timeout=30
    ).strip()
    inventory_files = {
        "arm64": "installed-package-notices.json",
        "amd64": "installed-package-notices-amd64.json",
    }
    if architecture not in inventory_files:
        raise ValueError("unreviewed DHI architecture: " + architecture)
    installed = load(root / inventory_files[architecture])
    if installed["platform"] != "linux/" + architecture:
        raise ValueError("DHI inventory architecture mismatch")
    actual = subprocess.check_output(
        ["dpkg-query", "-W", "-f=${Package}\t${Version}\n"], text=True, timeout=30
    )
    if len(actual) > JSON_LIMIT:
        raise ValueError("package query exceeds bound")
    actual = unique(line.split("\t", 1) for line in actual.splitlines())
    for row in installed["packages"]:
        if actual.get(row["package"]) != row["version"]:
            raise ValueError("DHI package version changed: " + row["package"])
    for row in installed["binaries"]:
        path = Path(row["path"])
        resolved = path.resolve(strict=True)
        if (
            str(resolved) != row.get("resolved", row["path"])
            or sha(resolved) != row["sha256"]
        ):
            raise ValueError("DHI binary identity changed: " + row["path"])
    bundled = load(root / "bundled-js-notices.json")
    for row in bundled:
        if sha(Path(row["path"])) != row["source_sha256"]:
            raise ValueError("DHI bundled JavaScript identity changed")
    modules = load(root / "go-module-notices.json")
    supplements = load(root / "upstream-license-supplements.json")
    if modules["failures"] or set(modules["missing_notice"]) != {
        row["module"] for row in supplements
    }:
        raise ValueError("unresolved Go module notice collection")
    components = (
        load(root / "main-component-notices.json")
        + load(root / "public-component-notices.json")
        + load(root / "native-build-notices.json")
        + load(root / "image-primary-notices.json")
    )
    references = list(bundled) + list(supplements)
    # Text completeness is the union, not an accidental all-architectures equality.
    for inventory_name in inventory_files.values():
        for row in load(root / inventory_name)["packages"]:
            references.extend(row.get("notices", []) + row.get("upstream_notices", []))
    for row in modules["modules"]:
        references.extend(row["notices"])
    for row in components:
        references.extend(row["notices"])
    rust = load(root / "rust-notices.json")
    for collection in (
        rust["collections"] + load(root / "selected-rust-notices.json")["collections"]
    ):
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
        rows = (
            source
            if isinstance(source, list)
            else source.get("archives", source.get("supplements"))
        )
        acquired_notices = [notice for row in rows for notice in row["notices"]]
        if acquired_notices != collection["notices"]:
            raise ValueError(
                "Rust notice correspondence changed from exact acquired source"
            )
        references.extend(acquired_notices)
    primary = load(root / "primary-source-notices.json")
    if (
        primary["image_reconciliation"]
        != "source-records/clipboard-spdx-reconciliation.json"
    ):
        raise ValueError("unreviewed image reconciliation source path")
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
            rows = source.get(
                "archives",
                source.get(
                    "packages", source.get("components", source.get("supplements"))
                ),
            )
            acquired = [notice for row in rows for notice in row["notices"]]
        if acquired != collection["notices"]:
            raise ValueError("primary notice/source correspondence changed: " + name)
        references.extend(acquired)
    adapters = load(root / "adapter-payloads.json")["adapters"]
    for adapter in adapters.values():
        for inventory in adapter["architectures"].values():
            for package in inventory["packages"]:
                references.extend(package["notices"])
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
    obligations = load(root / "payload-obligations.json")
    # This independent source/member correspondence is not derived from the
    # provider index. Dropping index rows and text files cannot discharge it.
    for component in components:
        require(component["required_notices"])
    for row in obligations["image_files"]:
        if sha(Path(row["path"])) != row["sha256"]:
            raise ValueError(
                "image-level legal/metadata identity changed: " + row["path"]
            )
    payloads = load(root / "agent-payload-deltas.json")["architectures"][architecture]
    clipboard_hash = sha(Path("/usr/local/bin/clipboard-bridge"))
    variants = [
        row for row in payloads["clipboard_variants"] if row["sha256"] == clipboard_hash
    ]
    if not variants:
        raise ValueError("unreviewed DHI clipboard helper identity")
    profiles = load(root / "derived-profiles.json")
    profile = None
    if args.profile != "base":
        profile = profiles[args.profile].get(args.version, {}).get(architecture)
        if not profile:
            raise ValueError("unreviewed selected Kit version/architecture")
        require(profile["required_notices"])
    observed_agents = {}
    inventories = agent_notices["agents_by_architecture"][architecture]
    for name, inventory in inventories.items():
        if not os.path.lexists(inventory["identity_marker"]):
            continue
        require(inventory["required_notices"])
        identity = obligations["agents"][architecture][name]
        require(identity["required_notices"])
        rows = inventory["payload_entries"]
        changes = profile["replacements"] if profile else {}
        for row in rows:
            check_entry(
                changes.get(row["path"], row),
                copied=args.copied_agents and name == "codex",
            )
        for binary in identity["binaries"]:
            if sha(Path(binary["path"])) != binary["sha256"]:
                raise ValueError(
                    "public-source payload identity changed: " + binary["path"]
                )
            require(binary["required_notices"])
        if name == "codex":
            require(primary["required_runtime_notices"])
            package = Path(inventory["identity_marker"]).parent
            scope = (
                Path("/usr/local/share/npm-global") if args.copied_agents else package
            )
            expected = {
                row["path"] for row in rows if Path(row["path"]).is_relative_to(scope)
            }
            actual_paths = set(tree(scope))
            if not args.copied_agents:
                actual_paths.add(str(scope))
            if actual_paths != expected:
                raise ValueError("unindexed or missing npm payload: " + str(scope))
        observed_agents[name] = [row["path"] for row in rows]
    if args.expect_agent != "auto":
        expected_agents = set() if args.expect_agent == "none" else {args.expect_agent}
        if set(observed_agents) != expected_agents:
            raise ValueError(
                "expected public base agent identity marker missing or unexpected"
            )
    adapter_result = None
    if args.adapter:
        policy = adapters[args.adapter]
        if policy["base_agent"] not in observed_agents:
            raise ValueError("required adapter base agent is missing")
        adapter_result = verify_adapter(policy, architecture, require)
    copied_agents = {}
    if args.copied_agents:
        identity = obligations["agents"][architecture]["claude"]
        require(identity["required_notices"])
        require(inventories["claude"]["required_notices"])
        expected = dict(
            identity["binaries"][0],
            path="/usr/local/bin/claude",
            mode=0o755,
            uid=0,
            gid=0,
            type="file",
        )
        check_entry(expected)
        if "codex" not in observed_agents:
            raise ValueError("copied maintained Codex package is missing")
        copied_agents = {
            "claude": {"path": expected["path"], "sha256": expected["sha256"]},
            "codex": {"verified_npm_global_entries": len(observed_agents["codex"])},
        }
    if profile:
        if profile["base_agent"] not in observed_agents:
            raise ValueError("required retained Kit base agent missing")
        for row in profile["additions"]:
            check_entry(row)
        for scope in profile.get("exact_trees", []):
            if set(tree(Path(scope["path"]))) != set(scope["entries"]):
                raise ValueError("unreviewed final Kit version tree")
    after, after_hash = bundle_tree(root)
    if before != after:
        raise ValueError("canonical source changed during verification")
    result = {
        "schema": "marsh.dhi-notices-verification/v2",
        "platform": installed["platform"],
        "stage": args.stage,
        "profile": args.profile,
        "selected_version": args.version,
        "stage_semantics": "Caller-declared verification point, not whole-image approval. Known recipes enforce ordering; payload coverage is limited to the explicit scopes below.",
        "expected_base_agent": args.expect_agent,
        "adapter_scope": adapter_result,
        "scope_kind": "native-kit+adapter"
        if args.adapter and profile
        else "adapter-final-scoped"
        if args.adapter
        else "copied-agents"
        if args.copied_agents
        else "native-kit"
        if profile
        else "base-payload-only",
        "verifier_sha256": before["verify.py"]["sha256"],
        "bundle_tree_sha256": bundle_hash,
        "bundle_tree_hash_format": "SHA256 of compact sorted JSON {relative:{sha256,size,mode}}, excluding only two named receipts",
        "inventory_image": installed["image"],
        "runtime_image_id": None,
        "image_identity_boundary": "Host must bind this receipt to actual docker argv/image ID and exact Dockerfile/tool/source/evidence hashes; this process cannot infer its own image digest.",
        "packages": len(installed["packages"]),
        "additional_installed_packages": {
            name: version
            for name, version in actual.items()
            if name not in {r["package"] for r in installed["packages"]}
        },
        "observed_base_agents": observed_agents,
        "observed_copied_agents": copied_agents,
        "known_final_mutations": profile if profile else None,
        "agent_scope": "Exact listed file bytes/type/mode/uid/gid and symlink targets. Copied mode requires the entire npm-global tree, explicitly chowned root:root. Base mode requires the Codex subtree only. No agent code is executed.",
        "agent_notice_files": len(agent_notices["notices"]),
        "notice_texts": len(indexed),
        "clipboard_matching_source_images": [r["image"] for r in variants],
        "qualification": "Not evaluated by this byte/metadata verifier. Runtime host gates, shipping notice materials and external redistribution permissions are distinct; see qualification-boundaries.json.",
        "notice_materials": primary["notice_material_status"],
        "clipboard_spdx_reconciliation": load(root / primary["image_reconciliation"]),
        "external_assumptions": load(root / "qualification-boundaries.json"),
        "rust_notice_coverage": load(root / "rust-coverage-status.json"),
        "selected_rust_notice_coverage": load(
            root / "selected-rust-coverage-status.json"
        ),
    }
    data = (json.dumps(result, indent=2) + "\n").encode()
    if args.receipt:
        name = (
            "base-image-verification.json"
            if args.stage == "base"
            else "image-verification.json"
        )
        destination = root / name
        if os.path.lexists(destination):
            st = destination.lstat()
            if (
                not stat.S_ISREG(st.st_mode)
                or st.st_nlink != 1
                or st.st_uid != os.getuid()
            ):
                raise ValueError("unsafe receipt destination")
        descriptor, temporary = tempfile.mkstemp(prefix=".receipt-", dir=root)
        try:
            with os.fdopen(descriptor, "wb") as out:
                out.write(data)
                out.flush()
                os.fsync(out.fileno())
            os.replace(temporary, destination)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)
    print(data.decode(), end="")


if __name__ == "__main__":
    main()
