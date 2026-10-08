#!/usr/bin/env python3
"""Collect public exact-lock Rust/source notices without cargo/build script execution.

This is a conservative lockfile superset, NOT proof of enabled features or of
all statically linked native inputs. Unavailable/missing grants remain explicit.
"""
import argparse
import concurrent.futures
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import tomllib
import threading
from public_sources import acquire, digest, members

from notice_rules import notice_material, is_source_code, legal_header, excluded_material


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--lock", type=Path, required=True)
    p.add_argument("--lock-sha256", required=True)
    p.add_argument("--lock-url", required=True)
    p.add_argument("--source-url", required=True)
    p.add_argument("--source-sha256", required=True)
    p.add_argument("--cache", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    args = p.parse_args()
    if digest(args.lock) != args.lock_sha256:
        raise ValueError("lock identity changed")
    document = tomllib.loads(args.lock.read_text())
    if len(document["package"]) > 5000:
        raise ValueError("lock package bound")
    args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    texts = args.output / "texts"
    texts.mkdir()
    text_lock = threading.Lock()
    groups = {}
    for package in document["package"]:
        source = package.get("source", "")
        checksum = None
        if source.startswith("registry+"):
            if source != "registry+https://github.com/rust-lang/crates.io-index":
                raise ValueError("unreviewed registry")
            name, version = package["name"], package["version"]
            if not re.fullmatch(r"[A-Za-z0-9_-]+", name) or not re.fullmatch(r"[A-Za-z0-9.+_-]+", version):
                raise ValueError("invalid crate identity")
            url = f"https://static.crates.io/crates/{name}/{name}-{version}.crate"
            checksum = package["checksum"]
        elif source.startswith("git+"):
            match = re.fullmatch(r"git\+https://github.com/([^?#]+)(?:\?[^#]+)?#([0-9a-f]{40})", source)
            if not match:
                raise ValueError("unreviewed git source: " + source)
            repository, revision = match.groups()
            url = f"https://codeload.github.com/{repository.removesuffix('.git')}/tar.gz/{revision}"
        elif not source:
            url, checksum = args.source_url, args.source_sha256
        else:
            raise ValueError("unreviewed lock source")
        group = groups.setdefault(url, {"url": url, "expected_sha256": checksum, "packages": []})
        group["packages"].append({k: package[k] for k in ("name", "version", "source", "checksum") if k in package})

    def collect(group):
        try:
            path, acquisition = acquire(group["url"], args.cache, sha256=group["expected_sha256"])
            notices, manifests, fallback = [], [], []
            for name, data, mode in members(path, acquisition["sha256"]):
                leaf = PurePosixPath(name).name
                if leaf == "Cargo.toml" and len(data) < 1024**2:
                    manifest = tomllib.loads(data.decode())
                    info = manifest.get("package", {})
                    if info:
                        manifests.append({"path": name, "sha256": hashlib.sha256(data).hexdigest(),
                                          **{k: info[k] for k in ("name", "version", "license", "license-file", "repository") if k in info}})
                # Names such as V8's copying-phase.cc are source code, not
                # licence documents. Exact source-header grants are handled by
                # complete-rust-notices.py, with explicit declaration provenance.
                material = notice_material(name, data)
                if material:
                    if len(data) > 16 * 1024**2:
                        raise ValueError("notice byte bound")
                    notices.append((name, material[0], material[1].get("source_file_sha256")))
                elif not excluded_material(name) and is_source_code(name) and (header := legal_header(data)) and (
                    b'Permission is hereby granted' in header or b'Redistribution and use in source and binary forms' in header
                ):
                    fallback.append((name, header, hashlib.sha256(data).hexdigest()))
                elif (not excluded_material(name) and not is_source_code(name)
                      and len(PurePosixPath(name).parts) <= 2
                      and leaf.lower().startswith('readme') and len(data) < 1024**2
                      and (b"Permission is hereby granted" in data or b"Redistribution and use in source and binary forms" in data)):
                    fallback.append((name, data, None))
            # Preserve actual inline/readme grants where archives omit a license file.
            if not notices:
                notices = fallback
            refs = []
            for name, data, source_hash in notices:
                sha = hashlib.sha256(data).hexdigest()
                target = texts / (sha + ".txt")
                with text_lock:
                    if target.exists():
                        if target.read_bytes() != data:
                            raise ValueError("notice content collision")
                    else:
                        with target.open("xb") as out:
                            out.write(data)
                reference = {"source_path": name, "sha256": sha, "file": "texts/" + target.name}
                if source_hash:
                    reference.update(material_type='source-notice-excerpt', source_file_sha256=source_hash, byte_range=[0, len(data)])
                refs.append(reference)
            return dict(group, acquisition=acquisition, package_manifests=manifests, notices=refs)
        except Exception as error:
            return dict(group, error=str(error), notices=[])

    results = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        for result in pool.map(collect, groups.values()):
            results.append(result)
            print(json.dumps({"url": result["url"], "notices": len(result["notices"]), "error": result.get("error")}), flush=True)
    record = {"schema": "marsh.public-rust-lock-notices/v1", "lock_url": args.lock_url,
              "lock_sha256": args.lock_sha256, "package_count": len(document["package"]),
              "scope": "All public packages in exact release lock, including optional, build, test and other-platform crates; not a computed enabled-feature graph or a complete native/static dependency inventory. No cargo/build scripts executed.",
              "archives": sorted(results, key=lambda r: r["url"]),
              "failures": [r["url"] for r in results if r.get("error")],
              "missing_notices": [r["url"] for r in results if not r["notices"]]}
    (args.output / "inventory.json").write_text(json.dumps(record, indent=2) + "\n")
    if record["missing_notices"]:
        raise SystemExit("notice gaps retained; qualification blocked")


if __name__ == "__main__":
    main()
