#!/usr/bin/env python3
"""Record one Claude Code release's publisher legal files and native manifest.

Version-driven: acquires the integrity-checked npm package
@anthropic-ai/claude-code@VERSION and the native release manifest, writes
provider-notices/claude-VERSION-package-{LICENSE,README}.md with
agent-notices.json rows, and records both acquisitions in
source-records/public-acquisitions.json. With --manifest-record NAME it also
stores the manifest bytes as source-records/NAME (claude-manifest for the base
template's release, claude-kit-manifest for the Kit's). --kit-profile makes this
release the claude-kit profile in derived-profiles.json: the native binary from
the publisher manifest, its versions/ symlink, and the versions/ tree it shares
with the base template's own release.

Claude Code is proprietary: the script refuses a release whose LICENSE.md
differs from the already reviewed text unless --accept-license-change is given.
It prints the required_notices and the linux binary checksums from the manifest.
It executes no downloaded code.
"""
import argparse
import hashlib
import json
from pathlib import Path
import urllib.request

from public_sources import acquire, members

HERE = Path(__file__).absolute().parent
PACKAGE = "@anthropic-ai/claude-code"
REVIEWED_LICENSE = "8ce94b9478bb9868f9641f818e06cd722fbe55d4c22e2d2ed11971b20146173a"


def encode(value):
    return (json.dumps(value, indent=2) + "\n").encode()


def write_kit_profile(bundle, version, manifest, providers, required):
    """The Kit installs the selected binary beside the template's own release."""
    versions = "/home/agent/.local/share/claude/versions"
    profiles_path = bundle / "derived-profiles.json"
    profiles = json.loads(profiles_path.read_text())
    profile = {}
    for architecture, platform in (("arm64", "linux-arm64"), ("amd64", "linux-x64")):
        base = providers["agents_by_architecture"][architecture]["claude"]["identity_marker"]
        if not base.startswith(versions + "/"):
            raise ValueError("unexpected base Claude identity marker: " + base)
        binary = manifest["platforms"][platform]
        selected = versions + "/" + version
        profile[architecture] = {
            "base_agent": "claude",
            "required_notices": required,
            "replacements": {"/home/agent/.local/bin/claude": {
                "path": "/home/agent/.local/bin/claude", "type": "symlink", "mode": 0o777,
                "uid": 1000, "gid": 1000, "target": selected}},
            "additions": [{"path": selected, "type": "file", "mode": 0o755, "uid": 1000, "gid": 1000,
                           "sha256": binary["checksum"], "size": binary["size"]}],
            "exact_trees": [{"path": versions, "entries": sorted({selected, base})}],
        }
    profiles["claude-kit"] = {version: profile}
    profiles_path.write_bytes(encode(profiles))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--cache", type=Path, required=True, help="owner-private acquisition cache")
    parser.add_argument("--bundle", type=Path, default=HERE / "collected")
    parser.add_argument("--manifest-record", choices=["claude-manifest", "claude-kit-manifest"])
    parser.add_argument("--kit-profile", action="store_true")
    parser.add_argument("--accept-license-change", action="store_true")
    args = parser.parse_args()
    version, bundle = args.version, args.bundle.absolute()
    request = urllib.request.Request(f"https://registry.npmjs.org/{PACKAGE}/{version}",
                                     headers={"User-Agent": "marsh-public-notice-collector"})
    with urllib.request.urlopen(request, timeout=60) as response:
        metadata = json.load(response)
    tarball, integrity = metadata["dist"]["tarball"], metadata["dist"]["integrity"]
    path, archive = acquire(tarball, args.cache, integrity=integrity)
    manifest_url = f"https://downloads.claude.ai/claude-code-releases/{version}/manifest.json"
    manifest_path, manifest_record = acquire(manifest_url, args.cache, limit=1024**2)
    manifest = json.loads(manifest_path.read_bytes())
    if manifest.get("version") != version:
        raise ValueError("manifest version mismatch")

    providers_path = bundle / "agent-notices.json"
    providers = json.loads(providers_path.read_text())
    prefix = "claude-" + version + "-package-"
    providers["notices"] = [n for n in providers["notices"] if not n["file"].startswith(prefix)]
    required = []
    wanted = {"package/LICENSE.md", "package/README.md"}
    for name, data, _mode in members(path, archive["sha256"]):
        if name not in wanted:
            continue
        wanted.discard(name)
        digest = hashlib.sha256(data).hexdigest()
        if name == "package/LICENSE.md" and digest != REVIEWED_LICENSE and not args.accept_license_change:
            raise SystemExit("Claude Code LICENSE.md changed from the reviewed text; owner review required")
        file = prefix + name.split("/", 1)[1]
        (bundle / "provider-notices" / file).write_bytes(data)
        providers["notices"].append({"file": file, "url": tarball, "member": name,
                                     "package_integrity": integrity,
                                     "archive_sha256": archive["sha256"], "sha256": digest})
        required.append({"file": "provider-notices/" + file, "sha256": digest})
    if wanted:
        raise ValueError("package lacks legal members: " + ", ".join(sorted(wanted)))
    providers_path.write_bytes(encode(providers))

    acquisitions_path = bundle / "source-records/public-acquisitions.json"
    acquisitions = json.loads(acquisitions_path.read_text())
    acquisitions["claude-" + version + "-legal-archive"] = archive
    if args.manifest_record:
        acquisitions[args.manifest_record] = manifest_record
        (bundle / "source-records" / args.manifest_record).write_bytes(manifest_path.read_bytes())
    acquisitions_path.write_bytes(encode(acquisitions))
    if args.kit_profile:
        write_kit_profile(bundle, version, manifest, providers, required)
    print(json.dumps({"version": version, "required_notices": sorted(required, key=lambda r: r["file"]),
                      "linux": {p: manifest["platforms"][p] for p in ("linux-arm64", "linux-x64")}},
                     indent=2))


if __name__ == "__main__":
    main()
