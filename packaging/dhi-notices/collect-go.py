#!/usr/bin/env python3
"""Collect exact public Go module notices with checksum-database verification.

This preparation tool does not compile or execute downloaded package code.
Image builds copy its already collected output and require no network access.
"""
import argparse
import concurrent.futures
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
import tempfile
import zipfile
from notice_rules import notice_document


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--go", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    source = Path(__file__).with_name("go-input.json")
    document = json.loads(source.read_text())
    args.output.mkdir(parents=True, exist_ok=False)
    texts = args.output / "texts"
    texts.mkdir()
    with tempfile.TemporaryDirectory(prefix="marsh-dhi-go-notices-") as cache:
        env = {
            "PATH": "/usr/bin:/bin",
            "HOME": cache,
            "GOENV": "off",
            "GOTOOLCHAIN": "local",
            "GOWORK": "off",
            "GOPATH": cache,
            "GOMODCACHE": str(Path(cache) / "mod"),
            "GOCACHE": str(Path(cache) / "build"),
            "GOPROXY": "https://proxy.golang.org",
            "GOSUMDB": "sum.golang.org",
            "GOPRIVATE": "",
            "GONOSUMDB": "",
        }

        def collect(module):
            spec = module["path"] + "@" + module["version"]
            if not re.fullmatch(r"[A-Za-z0-9._~+/!@-]+", spec):
                raise ValueError("unexpected public module identifier")
            process = subprocess.run(
                [str(args.go), "mod", "download", "-json", spec],
                cwd=cache, env=env, capture_output=True, text=True, timeout=180,
            )
            if process.returncode:
                raise RuntimeError(f"download failed for {spec}: {process.stdout} {process.stderr}")
            item = json.loads(process.stdout)
            if item.get("Error") or not item.get("Sum", "").startswith("h1:"):
                raise RuntimeError(f"unverified module {spec}")
            archive = Path(item["Zip"])
            notices = []
            with zipfile.ZipFile(archive) as bundle:
                for entry in bundle.infolist():
                    path = PurePosixPath(entry.filename)
                    if path.is_absolute() or ".." in path.parts:
                        raise ValueError("unsafe module archive path")
                    if entry.is_dir() or not notice_document(entry.filename):
                        continue
                    if entry.file_size > 4 * 1024 * 1024:
                        raise ValueError(f"oversized notice in {spec}")
                    data = bundle.read(entry)
                    digest = hashlib.sha256(data).hexdigest()
                    destination = texts / (digest + ".txt")
                    # Identical bytes may be published concurrently by modules.
                    try:
                        with destination.open("xb") as output:
                            output.write(data)
                    except FileExistsError:
                        pass
                    notices.append({"source_path": entry.filename, "sha256": digest,
                                    "file": "texts/" + destination.name})
            result = dict(module, sum=item["Sum"], go_mod_sum=item.get("GoModSum"),
                          archive_sha256=hashlib.sha256(archive.read_bytes()).hexdigest(),
                          notices=notices)
            print(json.dumps({"module": spec, "notices": len(notices)}), flush=True)
            return result

        results = []
        failures = []
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            pending = {pool.submit(collect, module): module for module in document["modules"]}
            for future in concurrent.futures.as_completed(pending):
                try:
                    results.append(future.result())
                except Exception as error:
                    failures.append({"module": pending[future], "error": str(error)})
        record = {
            "schema": 1, "image": document["image"], "scope": document["scope"],
            "input_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
            "checksum_database": "sum.golang.org",
            "modules": sorted(results, key=lambda row: (row["path"], row["version"])),
            "failures": failures,
            "missing_notice": [row["path"] + "@" + row["version"] for row in results if not row["notices"]],
        }
        (args.output / "go-module-notices.json").write_text(json.dumps(record, indent=2) + "\n")
        if failures or record["missing_notice"]:
            raise SystemExit("notice inventory incomplete; inspect retained manifest")


if __name__ == "__main__":
    main()
