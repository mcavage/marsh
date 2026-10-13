#!/usr/bin/env python3
"""Prepare exact locked ACP native package identities/notices without npm or agent execution."""

import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath

from public_sources import acquire, members, private_output, read_input
from notice_rules import notice_document

# One Kit per agent: each ACP adapter's lock lives in its agent's Kit. The
# selected version is the one that lock pins for the first listed package.
ADAPTERS = {
    "codex-acp": {
        "kit": "marsh-codex",
        "root": "/opt/marsh/codex-acp",
        "packages": ["@openai/codex", "@openai/codex-linux-{arch}"],
    },
    "claude-acp": {
        "kit": "marsh-claude",
        "root": "/opt/marsh/claude-acp",
        "packages": [
            "@anthropic-ai/claude-agent-sdk",
            "@anthropic-ai/claude-agent-sdk-linux-{arch}",
        ],
    },
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-root", type=Path, required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    output = private_output(args.output)
    (output / "texts").mkdir()
    bundle = args.source_root / "packaging/dhi-notices/collected"
    providers = json.loads(read_input(bundle / "agent-notices.json"))
    components = json.loads(read_input(bundle / "main-component-notices.json"))
    zsh = next(r for r in components if r["package"] == "codex-patched-zsh")
    zsh_artifacts = next(
        r
        for r in json.loads(read_input(bundle / "public-component-notices.json"))
        if r["package"] == "codex-patched-zsh"
    )["artifacts"]
    rg = next(r for r in components if r["package"] == "codex-bundled-ripgrep")
    records = {}
    sources = {}
    for adapter, spec in ADAPTERS.items():
        lock_path = args.source_root / "kits" / spec["kit"] / "package-lock.json"
        lock_bytes = read_input(lock_path)
        lock = json.loads(lock_bytes)
        spec = dict(spec, version=lock["packages"]["node_modules/" + spec["packages"][0]]["version"])
        result = {
            "root": spec["root"],
            "lock_sha256": hashlib.sha256(lock_bytes).hexdigest(),
            "notice_root": "/usr/local/share/licenses/marsh-" + adapter,
            "selected_version": spec["version"],
            "architectures": {},
        }
        for arch, npm_arch in [("arm64", "arm64"), ("amd64", "x64")]:
            packages = []
            required = []
            if adapter == "codex-acp":
                # Provider notices for the selected release come from
                # add-codex-release.py (provider-notices/codex-VERSION-*).
                prefix = "codex-" + spec["version"] + "-"
                provider = [
                    {"file": "provider-notices/" + r["file"], "sha256": r["sha256"]}
                    for r in providers["notices"]
                    if r["file"].startswith(prefix)
                ]
                if not provider:
                    raise ValueError("no provider notices recorded for Codex " + spec["version"])
                required += provider + zsh["required_notices"] + rg["required_notices"]
                # LGPL bubblewrap source text is byte-identical in every reviewed
                # release source; don't use a symlink's "COPYING" as grant.
                if not any(r["file"].endswith("-codex-rs-vendor-bubblewrap-COPYING") for r in provider):
                    required.append(
                        {
                            "file": "provider-notices/codex-0.155.1-codex-rs-vendor-bubblewrap-COPYING",
                            "sha256": "b7993225104d90ddd8024fd838faf300bea5e83d91203eab98e29512acebd69c",
                        }
                    )
            for template in spec["packages"]:
                alias = template.format(arch=npm_arch)
                relative = "node_modules/" + alias
                locked = lock["packages"][relative]
                path, acquisition = acquire(
                    locked["resolved"], args.cache, integrity=locked["integrity"]
                )
                entries, directories, notices, package = [], set(), [], None
                for name, data, mode in members(path, acquisition["sha256"]):
                    member = PurePosixPath(name)
                    if not member.is_relative_to("package"):
                        raise ValueError("unexpected npm archive root")
                    rel = str(member.relative_to("package"))
                    full = spec["root"] + "/" + relative + "/" + rel
                    for parent in PurePosixPath(rel).parents:
                        if str(parent) != ".":
                            directories.add(
                                spec["root"] + "/" + relative + "/" + str(parent)
                            )
                    digest = hashlib.sha256(data).hexdigest()
                    if adapter == "codex-acp" and rel.endswith("/zsh/bin/zsh"):
                        expected = next(
                            r["binary_sha256"]
                            for r in zsh_artifacts
                            if r["architecture"] == arch
                        )
                        if digest != expected:
                            raise ValueError(
                                "selected ACP zsh requires separate source/licence correspondence"
                            )
                    if adapter == "codex-acp" and rel.endswith("/codex-path/rg"):
                        platform = (
                            "linux-aarch64" if arch == "arm64" else "linux-x86_64"
                        )
                        expected = next(
                            r["binary_sha256"]
                            for r in rg["artifacts"]
                            if r["platform"] == platform
                        )
                        if digest != expected:
                            raise ValueError(
                                "selected ACP ripgrep requires separate release correspondence"
                            )
                    entries.append(
                        {
                            "path": full,
                            "type": "file",
                            "sha256": digest,
                            "size": len(data),
                            "mode": mode & 0o777,
                            "uid": 0,
                            "gid": 0,
                            "npm_member": name,
                        }
                    )
                    if rel == "package.json":
                        package = json.loads(data)
                    if notice_document(name) or (
                        adapter == "claude-acp" and rel == "README.md"
                    ):
                        target = output / "texts" / (digest + ".txt")
                        if not target.exists():
                            target.write_bytes(data)
                        notices.append(
                            {
                                "file": "texts/" + target.name,
                                "sha256": digest,
                                "url": locked["resolved"],
                                "package_integrity": locked["integrity"],
                                "member": name,
                            }
                        )
                if not package or package["version"] != locked["version"] or not package["version"].startswith(spec["version"]):
                    raise ValueError("unexpected locked package version")
                directory = spec["root"] + "/" + relative
                entries += [
                    {"path": n, "type": "directory", "mode": 0o755, "uid": 0, "gid": 0}
                    for n in sorted(directories | {directory})
                ]
                packages.append(
                    {
                        "alias": alias,
                        "root": directory,
                        "package": package["name"],
                        "version": package["version"],
                        "acquisition": acquisition,
                        "payload_entries": sorted(entries, key=lambda r: r["path"]),
                        "notices": notices,
                    }
                )
                required += [
                    {"file": n["file"], "sha256": n["sha256"]} for n in notices
                ]
                sources[locked["resolved"]] = acquisition
            links = []
            if adapter == "codex-acp":
                links.append(
                    {
                        "path": spec["root"] + "/node_modules/.bin/codex",
                        "type": "symlink",
                        "mode": 0o777,
                        "uid": 0,
                        "gid": 0,
                        "target": "../@openai/codex/bin/codex.js",
                    }
                )
            result["architectures"][arch] = {
                "packages": packages,
                "links": links,
                "required_notices": list({n["file"]: n for n in required}.values()),
            }
        if read_input(lock_path) != lock_bytes:
            raise ValueError("selected lock changed during collection")
        records[adapter] = result
    # Pi packages are covered by their own post-repair npm collector, not called
    # a complete byte inventory by this base/native verifier.
    # The pi Kit's one lock carries Pi and the pi-acp adapter.
    for adapter, root in [("pi", "/opt/marsh/pi-gateway")]:
        lock_path = (
            args.source_root / "kits" / ("marsh-" + adapter) / "package-lock.json"
        )
        records[adapter] = {
            "root": root,
            "lock_sha256": hashlib.sha256(read_input(lock_path)).hexdigest(),
            "notice_root": "/usr/local/share/licenses/marsh-" + adapter,
            "architectures": {
                arch: {"packages": [], "links": [], "required_notices": []}
                for arch in ["arm64", "amd64"]
            },
        }
    document = {
        "schema": "marsh.adapter-notice-payloads/v1",
        "adapters": records,
        "scope": "Exact locked public npm members for selected ACP native payloads; other installed npm packages are metadata/notice checked, not full-code or link/rights approval. No package scripts/agents executed.",
    }
    (output / "adapter-payloads.json").write_text(json.dumps(document, indent=2) + "\n")
    print(
        json.dumps(
            {"adapters": list(records), "public_archives": len(sources)}, indent=2
        )
    )


if __name__ == "__main__":
    main()
