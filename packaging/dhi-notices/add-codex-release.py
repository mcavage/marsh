#!/usr/bin/env python3
"""Record one Codex release's exact-lock Rust notices and provider legal files.

Version-driven: resolves the immutable `rust-vVERSION` tag commit, acquires its
Cargo.lock and source archive, runs the canonical collect-rust-lock.py and
supplement-rust-notices.py collectors, and adds the results to the bundle:

- source-records/VERSION-{Cargo.lock,lock-acquisition.json,source-acquisition.json}
- source-records/codex-VERSION-rust{,-supplements}.json and their texts
- two collections in --index (rust-notices.json or selected-rust-notices.json)
- provider-notices/codex-VERSION-* for every legal document in the source tree
  (notice_rules.notice_document), with agent-notices.json rows

With --kit-profile it also makes this release the codex-kit profile in
derived-profiles.json: the release archives listed in
kits/marsh-codex/release-checksums.txt are acquired and checksum-verified, and
the native codex and codex-code-mode-host members become the profile's
files (recorded in public-acquisitions.json as kit-ARCH-BINARY).

It prints the provider required_notices list for the profile or payload record
that ships this release. It executes no downloaded code. Re-running for the same
version replaces that version's records only.
"""
import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import sys
import urllib.request

from notice_rules import notice_document
from public_sources import acquire, members

HERE = Path(__file__).absolute().parent
REPOSITORY = "openai/codex"


def api(path):
    request = urllib.request.Request("https://api.github.com/repos/" + REPOSITORY + path,
                                     headers={"User-Agent": "marsh-public-notice-collector",
                                              "Accept": "application/vnd.github+json"})
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)


def tag_commit(version):
    target = api("/git/ref/tags/rust-v" + version)["object"]
    while target["type"] == "tag":
        target = api("/git/tags/" + target["sha"])["object"]
    if target["type"] != "commit" or len(target["sha"]) != 40:
        raise ValueError("tag does not resolve to a commit")
    return target["sha"]


def encode(value):
    return (json.dumps(value, indent=2) + "\n").encode()


def copy_texts(source, bundle):
    for text in (source / "texts").iterdir():
        destination = bundle / "texts" / text.name
        data = text.read_bytes()
        if hashlib.sha256(data).hexdigest() + ".txt" != text.name:
            raise ValueError("collector text is not content-addressed: " + text.name)
        if not destination.exists():
            destination.write_bytes(data)


def write_kit_profile(bundle, version, required, cache):
    """The Kit overlays the selected native binaries in /usr/local/bin."""
    checksums = {}
    for line in (HERE.parent.parent / "kits/marsh-codex/release-checksums.txt").read_text().splitlines():
        if line and not line.startswith("#"):
            digest, name = line.split()
            checksums[name] = digest
    acquisitions_path = bundle / "source-records/public-acquisitions.json"
    acquisitions = json.loads(acquisitions_path.read_text())
    profile = {}
    for architecture, target in (("arm64", "aarch64-unknown-linux-musl"), ("amd64", "x86_64-unknown-linux-musl")):
        additions = []
        for binary in ("codex", "codex-code-mode-host"):
            asset = f"{binary}-{target}.tar.gz"
            url = f"https://github.com/{REPOSITORY}/releases/download/rust-v{version}/{asset}"
            path, record = acquire(url, cache, sha256=checksums[version + "/" + asset])
            content = [(n, d, m) for n, d, m in members(path, record["sha256"])]
            if [n for n, _, _ in content] != [f"{binary}-{target}"]:
                raise ValueError("unexpected release archive members: " + asset)
            name, data, mode = content[0]
            digest = hashlib.sha256(data).hexdigest()
            acquisitions[f"kit-{architecture}-{binary}"] = dict(record, members=[
                {"member": name, "sha256": digest, "size": len(data), "mode": mode & 0o777}])
            additions.append({"path": "/usr/local/bin/" + binary, "type": "file", "mode": 0o755,
                              "uid": 0, "gid": 0, "sha256": digest, "size": len(data)})
        profile[architecture] = {"required_notices": required, "files": additions}
    acquisitions_path.write_bytes(encode(acquisitions))
    profiles_path = bundle / "derived-profiles.json"
    profiles = json.loads(profiles_path.read_text())
    profiles["codex-kit"] = {version: profile}
    profiles_path.write_bytes(encode(profiles))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--cache", type=Path, required=True, help="owner-private acquisition cache")
    parser.add_argument("--work", type=Path, required=True, help="fresh owner-private work directory")
    parser.add_argument("--bundle", type=Path, default=HERE / "collected")
    parser.add_argument("--index", choices=["rust-notices.json", "selected-rust-notices.json"],
                        default="selected-rust-notices.json")
    parser.add_argument("--kit-profile", action="store_true")
    args = parser.parse_args()
    version, bundle = args.version, args.bundle.absolute()
    args.work.mkdir(mode=0o700, parents=True, exist_ok=False)
    commit = tag_commit(version)
    lock_url = f"https://raw.githubusercontent.com/{REPOSITORY}/{commit}/codex-rs/Cargo.lock"
    source_url = f"https://codeload.github.com/{REPOSITORY}/tar.gz/{commit}"
    lock_path, lock_record = acquire(lock_url, args.cache)
    source_path, source_record = acquire(source_url, args.cache)
    records = bundle / "source-records"
    (records / (version + "-Cargo.lock")).write_bytes(lock_path.read_bytes())
    (records / (version + "-lock-acquisition.json")).write_bytes(encode(lock_record))
    (records / (version + "-source-acquisition.json")).write_bytes(encode(source_record))

    rust = args.work / "rust"
    collector = subprocess.run(
        [sys.executable, str(HERE / "collect-rust-lock.py"), "--lock", str(lock_path),
         "--lock-sha256", lock_record["sha256"], "--lock-url", lock_url,
         "--source-url", source_url, "--source-sha256", source_record["sha256"],
         "--cache", str(args.cache), "--output", str(rust)],
        stdout=subprocess.DEVNULL)
    inventory = json.loads((rust / "inventory.json").read_text())
    if inventory["failures"]:
        raise SystemExit("Rust lock acquisition failures: " + json.dumps(inventory["failures"]))
    if collector.returncode and not inventory["missing_notices"]:
        raise SystemExit("collect-rust-lock.py failed")
    supplements = args.work / "supplements"
    subprocess.run([sys.executable, str(HERE / "supplement-rust-notices.py"), "--inventory",
                    str(rust / "inventory.json"), "--cache", str(args.cache), "--output",
                    str(supplements)], stdout=subprocess.DEVNULL, check=True)
    supplement = json.loads((supplements / "inventory.json").read_text())

    rust_name = "codex-" + version + "-rust"
    collections = []
    for name, directory, document, unresolved in [
        (rust_name, rust, inventory, inventory["missing_notices"]),
        (rust_name + "-supplements", supplements, supplement, supplement["unresolved"]),
    ]:
        data = encode(document)
        (records / (name + ".json")).write_bytes(data)
        copy_texts(directory, bundle)
        rows = document.get("archives", document.get("supplements"))
        collections.append({
            "name": name, "source_inventory": "source-records/" + name + ".json",
            "source_inventory_sha256": hashlib.sha256(data).hexdigest(),
            "notices": [n for row in rows for n in row["notices"]],
            "unresolved": unresolved,
        })
    index_path = bundle / args.index
    index = json.loads(index_path.read_text())
    names = {c["name"] for c in collections}
    index["collections"] = sorted([c for c in index["collections"] if c["name"] not in names] + collections,
                                  key=lambda c: c["name"])
    index_path.write_bytes(encode(index))

    providers_path = bundle / "agent-notices.json"
    providers = json.loads(providers_path.read_text())
    prefix = "codex-" + version + "-"
    providers["notices"] = [n for n in providers["notices"] if not n["file"].startswith(prefix)]
    for stale in (bundle / "provider-notices").glob(prefix + "*"):
        stale.unlink()
    required = []
    for name, data, _mode in members(source_path, source_record["sha256"]):
        member = str(PurePosixPath(*PurePosixPath(name).parts[1:]))
        if not notice_document(member):
            continue
        digest = hashlib.sha256(data).hexdigest()
        file = prefix + member.replace("/", "-")
        (bundle / "provider-notices" / file).write_bytes(data)
        providers["notices"].append({
            "file": file, "url": f"https://raw.githubusercontent.com/{REPOSITORY}/{commit}/{member}",
            "commit": commit, "member": member, "sha256": digest, "source_archive": source_record,
            "scope": "Exact source archive member; URL identifies the same immutable source, not a separate raw-URL acquisition or redistribution approval.",
        })
        required.append({"file": "provider-notices/" + file, "sha256": digest})
    providers["notices"].sort(key=lambda n: n["file"])
    providers_path.write_bytes(encode(providers))
    required.sort(key=lambda r: r["file"])
    if args.kit_profile:
        write_kit_profile(bundle, version, required, args.cache)
    shutil.rmtree(args.work)
    print(json.dumps({"version": version, "commit": commit, "lock_packages": inventory["package_count"],
                      "missing_after_lock": len(inventory["missing_notices"]),
                      "unresolved_after_supplements": supplement["unresolved"],
                      "required_notices": required}, indent=2))


if __name__ == "__main__":
    main()
