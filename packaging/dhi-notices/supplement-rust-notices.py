#!/usr/bin/env python3
"""Acquire omitted crate notices from the exact .cargo_vcs_info commit.

Original checksum-verified crate bytes and their manifest supply repository/path.
No mutable branch/tag fallback; failures remain in the output for qualification.
"""
import argparse
import concurrent.futures
import hashlib
import json
from pathlib import Path, PurePosixPath
import posixpath
import re
import threading
import tomllib
from public_sources import acquire, members
from notice_rules import notice_document


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inventory", type=Path, required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    document = json.loads(args.inventory.read_text())
    args.output.mkdir(mode=0o700, exist_ok=False)
    (args.output / "texts").mkdir()
    lock = threading.Lock()

    def collect(row):
        if row["notices"]:
            return None
        result = {"crate_url": row["url"], "packages": row["packages"], "notices": []}
        try:
            path, receipt = acquire(row["url"], args.cache, sha256=row["expected_sha256"])
            vcs, package = None, None
            inline = []
            for name, data, mode in members(path, receipt["sha256"]):
                if PurePosixPath(name).name == ".cargo_vcs_info.json":
                    vcs = json.loads(data)
                if len(PurePosixPath(name).parts) == 2 and name.endswith("/Cargo.toml"):
                    package = tomllib.loads(data.decode())["package"]
                if notice_document(name):
                    inline.append((name, data))
            def save(data, info):
                sha = hashlib.sha256(data).hexdigest()
                target = args.output / "texts" / (sha + ".txt")
                with lock:
                    if not target.exists():
                        target.write_bytes(data)
                result["notices"].append(dict(info, sha256=sha, file="texts/" + target.name))
            if inline:
                for name, data in inline:
                    save(data, {"url": row["url"], "member": name})
                return result
            if not vcs or not package:
                raise ValueError("crate omits exact VCS metadata or package manifest")
            revision = vcs["git"]["sha1"]
            if not re.fullmatch(r"[0-9a-f]{40}", revision):
                raise ValueError("invalid VCS commit")
            repo = package.get("repository", "").removesuffix("/").removesuffix(".git")
            m = re.fullmatch(r"https?://github.com/([^/]+/[^/]+)(?:/.*)?", repo)
            if not m:
                raise ValueError("no supported public exact-commit repository")
            base = vcs.get("path_in_vcs", "")
            result.update(vcs=vcs, repository=repo, crate_sha256=receipt["sha256"])
            candidates = []
            if package.get("license-file"):
                candidates.append(posixpath.normpath(posixpath.join(base, package["license-file"])))
            for prefix in [base, ""]:
                for leaf in ["LICENSE", "LICENSE-MIT", "LICENSE-APACHE", "LICENSE.md", "LICENSE.txt", "COPYING", "NOTICE", "COPYRIGHT", "UNLICENSE"]:
                    candidates.append(posixpath.join(prefix, leaf))
            for name in dict.fromkeys(candidates):
                if name.startswith("../") or name.startswith("/"):
                    continue
                url = f"https://raw.githubusercontent.com/{m[1]}/{revision}/{name}"
                try:
                    source, acquisition = acquire(url, args.cache, limit=16 * 1024**2)
                except Exception as error:
                    result.setdefault("attempts", []).append({"url": url, "error": str(error)})
                    continue
                save(source.read_bytes(), acquisition)
            if not result["notices"]:
                result["error"] = "exact upstream notice not acquired"
        except Exception as error:
            result["error"] = str(error)
        return result

    # Crates in the same workspace share raw URLs. Group requests by sequential
    # archive processing to avoid partial-cache publication races.
    results = []
    for row in document["archives"]:
        result = collect(row)
        if result is not None:
            results.append(result)
            print(json.dumps({"url": result["crate_url"], "notices": len(result["notices"]), "error": result.get("error")}), flush=True)
    (args.output / "inventory.json").write_text(json.dumps({
        "schema": "marsh.exact-crate-upstream-notices/v1",
        "input_sha256": hashlib.sha256(args.inventory.read_bytes()).hexdigest(),
        "supplements": results,
        "unresolved": [r["crate_url"] for r in results if not r["notices"]],
    }, indent=2) + "\n")


if __name__ == "__main__":
    main()
