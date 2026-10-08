#!/usr/bin/env python3
"""Collect locked Rust package notices for the supported Mac/Linux builds."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tarfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cargo", default="cargo")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    overrides_root = root / "packaging/notice-overrides"
    overrides = json.loads((overrides_root / "sources.json").read_text())
    native = json.loads((overrides_root / "embedded-native.json").read_text())
    included, packages, workspace = set(), {}, set()
    for target in ["aarch64-apple-darwin", "aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"]:
        metadata = json.loads(subprocess.check_output([
            args.cargo, "metadata", "--locked", "--format-version", "1",
            "--filter-platform", target,
        ], cwd=root))
        packages.update((package["id"], package) for package in metadata["packages"])
        workspace.update(metadata["workspace_members"])
        nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
        pending = list(metadata["workspace_members"])
        seen = set(pending)
        while pending:
            for dependency in nodes[pending.pop()]["deps"]:
                if any(kind["kind"] != "dev" for kind in dependency["dep_kinds"]):
                    if dependency["pkg"] not in seen:
                        seen.add(dependency["pkg"])
                        pending.append(dependency["pkg"])
        included.update(seen)

    texts, records = {}, []
    for identity in sorted(included - workspace,
                           key=lambda key: (packages[key]["name"], packages[key]["version"])):
        package = packages[identity]
        directory = Path(package["manifest_path"]).parent
        sources = []
        for path in sorted(directory.rglob("*")):
            if path.is_file() and path.name.upper().startswith((
                    "LICENSE", "LICENCE", "COPYING", "COPYRIGHT", "NOTICE", "UNLICENSE")):
                sources.append((str(path.relative_to(directory)), path.read_bytes(), None))
        if package.get("license_file"):
            path = directory / package["license_file"]
            sources.append((package["license_file"], path.read_bytes(), None))
        # Vendored Brush members inherit the repository's MIT license.
        if not sources and directory.is_relative_to(root / "vendor/brush"):
            sources.append(("vendor/brush/LICENSE", (root / "vendor/brush/LICENSE").read_bytes(), None))
        key = package["name"] + "@" + package["version"]
        if package["name"] == native["crate"]:
            archive = directory / "duckdb.tar.gz"
            if (package["version"] != native["version"] or
                    hashlib.sha256(archive.read_bytes()).hexdigest() != native["archive_sha256"]):
                raise SystemExit("Bundled DuckDB changed; review embedded native notices")
            with tarfile.open(archive) as source:
                components = sorted({name.split("/")[2] for name in source.getnames()
                                     if name.startswith("duckdb/third_party/")})
            if components != native["components"]:
                raise SystemExit("Bundled DuckDB component inventory changed")
            if not set(components) <= {item["component"] for item in overrides.get(key, [])}:
                raise SystemExit("Missing bundled DuckDB component notices")
        for override in overrides.get(key, []):
            data = (overrides_root / override["file"]).read_bytes()
            if hashlib.sha256(data).hexdigest() != override["sha256"]:
                raise SystemExit("Notice override checksum changed: " + override["file"])
            sources.append((override["file"], data, override))
        if not sources:
            raise SystemExit("Missing license text for " + key)
        licenses = []
        for name, data, provenance in sources:
            digest = hashlib.sha256(data).hexdigest()
            texts[digest] = data.decode("utf-8")
            licenses.append({"name": name, "sha256": digest, "provenance": provenance})
        records.append({"name": package["name"], "version": package["version"],
                        "license": package["license"], "repository": package["repository"],
                        "authors": package["authors"], "notices": licenses})

    header = (
        "marsh third-party Rust package notices\n\n"
        "Generated from Cargo.lock for normal and build dependencies on macOS ARM64 "
        "and Linux ARM64/AMD64. These notices include upstream license alternatives; "
        "they are not an image SBOM or a provider CLI redistribution clearance. "
        "Separately distributed stock SBX, system/VM images, provider CLIs, and "
        "native libraries bundled by dependencies retain their own notices and terms. "
        "The libduckdb-sys section includes the exact DuckDB source archive's "
        "26 bundled native components; see embedded-native-notices.json.\n\n"
    )
    body = [header]
    for record in records:
        body.append(f"{record['name']} {record['version']}\n")
        body.append(f"Declared license: {record['license']}\n")
        body.append(f"Source: {record['repository']}\n")
        body.append("Authors: " + "; ".join(record["authors"]) + "\n")
        for notice in record["notices"]:
            body.append(f"  {notice['name']}: text sha256:{notice['sha256']}\n")
            if notice["provenance"]:
                body.append("  Retrieved from: " + notice["provenance"]["url"] + "\n")
                if notice["provenance"].get("selection"):
                    body.append("  " + notice["provenance"]["selection"] + "\n")
        body.append("\n")
    for digest, text in sorted(texts.items()):
        body.append("=" * 72 + f"\nLicense/notice text sha256:{digest}\n\n{text}\n")
    args.output.mkdir(parents=True, exist_ok=True)
    (args.output / "THIRD-PARTY-NOTICES.txt").write_text("".join(body))
    (args.output / "rust-package-notices.json").write_text(json.dumps({
        "cargo_lock_sha256": hashlib.sha256((root / "Cargo.lock").read_bytes()).hexdigest(),
        "packages": records,
    }, indent=2) + "\n")
    (args.output / "embedded-native-notices.json").write_text(json.dumps(native, indent=2) + "\n")
    print(f"Wrote {len(records)} package notices and {len(texts)} distinct license texts to {args.output}")


if __name__ == "__main__":
    main()
