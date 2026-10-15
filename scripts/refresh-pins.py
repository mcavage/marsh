#!/usr/bin/env python3
"""`make refresh-pins`: regenerate every record the image gate checks from the pins in the tree.

The pins are the Kit descriptors' versions (kits/*/*.yaml), the npm locks, the
Dockerfiles' base digests, the image-base package lists and the hand-pinned
DHI packages. From them this regenerates, in order:

  1. kits/*/release-checksums.txt          scripts/refresh-agent-checksums.py
  2. agent release records                 packaging/dhi-notices/add-claude-release.py,
                                           add-codex-release.py (the Codex Kit's release
                                           and the one codex-acp locks); records of
                                           releases no longer shipped are retired
  3. kits/*/notices/overrides.json         exact-version npm notice overrides follow a
                                           bumped package to its new version's source
  4. adapter-payloads.json                 collect-adapter-payloads.py
  5. image records                         builds each image's `image` stage, then
                                           inventory.py and (if the Go modules moved)
                                           collect-go.py
  6. unreferenced texts are deleted; the DHI legal receipt hash is updated
  7. the license delta against --base      scripts/license-delta.py

Records of an already-recorded release are immutable and are not re-acquired,
so a run on a tree whose records are current changes nothing. Exit status: 0
when the license delta is OK, 2 when it NEEDS-OWNER, 1 when a step failed.
Needs network, Docker Buildx with dhi.io access (step 5), git and Go.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[1]
NOTICES = ROOT / "packaging/dhi-notices"
BUNDLE = NOTICES / "collected"
sys.path.insert(0, str(NOTICES))
import bundle_refs  # noqa: E402
import importlib.util  # noqa: E402

_spec = importlib.util.spec_from_file_location("marsh_inventory", NOTICES / "inventory.py")
inventory = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(inventory)
from notice_rules import notice_document  # noqa: E402

VERSION = re.compile(r'^args:\n  version:\n    default: "([^"]+)"', re.M)
RAW = re.compile(r"^https://raw\.githubusercontent\.com/([^/]+/[^/]+)/([0-9a-f]{40})/(.+)$")
NPM_KITS = ("marsh-claude", "marsh-codex", "marsh-pi")


def log(message):
    print(f"refresh-pins: {message}", file=sys.stderr, flush=True)


def run(*argv, **kwargs):
    log("+ " + " ".join(map(str, argv)))
    subprocess.run([str(a) for a in argv], check=True, cwd=kwargs.pop("cwd", ROOT), **kwargs)


def load(path):
    return json.loads(pathlib.Path(path).read_text())


def dump(path, value):
    pathlib.Path(path).write_text(json.dumps(value, indent=2) + "\n")


def private(path: pathlib.Path) -> pathlib.Path:
    path.mkdir(parents=True, exist_ok=True)
    path.chmod(0o700)
    return pathlib.Path(os.path.realpath(path))


def declared(kit, descriptor):
    match = VERSION.search((ROOT / "kits" / kit / descriptor).read_text())
    if not match:
        raise ValueError(f"kits/{kit}/{descriptor} declares no args.version.default")
    return match.group(1)


# 2. Agent release records -------------------------------------------------

def codex_versions():
    kit = declared("marsh-codex", "codex.yaml")
    lock = load(ROOT / "kits/marsh-codex/package-lock.json")
    adapter = lock["packages"]["node_modules/@openai/codex"]["version"]
    return kit, adapter


def recorded_codex():
    names = {c["name"] for c in load(BUNDLE / "selected-rust-notices.json")["collections"]}
    return {m.group(1) for n in names if (m := re.fullmatch(r"codex-(.+)-rust", n))}


def retire_codex(version):
    log(f"retiring Codex {version} records")
    index = load(BUNDLE / "selected-rust-notices.json")
    gone = {f"codex-{version}-rust", f"codex-{version}-rust-supplements"}
    index["collections"] = [c for c in index["collections"] if c["name"] not in gone]
    dump(BUNDLE / "selected-rust-notices.json", index)
    for name in [f"codex-{version}-rust.json", f"codex-{version}-rust-supplements.json",
                 f"{version}-Cargo.lock", f"{version}-lock-acquisition.json", f"{version}-source-acquisition.json"]:
        (BUNDLE / "source-records" / name).unlink(missing_ok=True)
    retire_provider(f"codex-{version}-")


def retire_provider(prefix, acquisitions=()):
    agents = load(BUNDLE / "agent-notices.json")
    agents["notices"] = [n for n in agents["notices"] if not n["file"].startswith(prefix)]
    dump(BUNDLE / "agent-notices.json", agents)
    for path in (BUNDLE / "provider-notices").glob(prefix + "*"):
        path.unlink()
    if acquisitions:
        records = load(BUNDLE / "source-records/public-acquisitions.json")
        for key in acquisitions:
            records.pop(key, None)
        dump(BUNDLE / "source-records/public-acquisitions.json", records)


def agent_records(cache, work):
    kit, adapter = codex_versions()
    present = recorded_codex()
    profile = load(BUNDLE / "derived-profiles.json")
    for version in sorted({kit, adapter}):
        kit_profile = version == kit and version not in profile.get("codex-kit", {})
        if version in present and not kit_profile:
            continue
        run(sys.executable, NOTICES / "add-codex-release.py", "--version", version, "--cache", cache,
            "--work", work / f"codex-{version}", *(["--kit-profile"] if version == kit else []))
    for version in sorted(recorded_codex() - {kit, adapter}):
        retire_codex(version)
    claude = declared("marsh-claude", "claude.yaml")
    profile = load(BUNDLE / "derived-profiles.json")
    if claude not in profile.get("claude-kit", {}) or \
            not (BUNDLE / f"provider-notices/claude-{claude}-package-LICENSE.md").is_file():
        # A changed proprietary license text is recorded so the tree is
        # complete; the license delta then holds the change for the owner.
        run(sys.executable, NOTICES / "add-claude-release.py", "--version", claude, "--cache", cache,
            "--manifest-record", "claude-kit-manifest", "--kit-profile", "--accept-license-change")
    for path in sorted((BUNDLE / "provider-notices").glob("claude-*-package-LICENSE.md")):
        other = path.name[len("claude-"):-len("-package-LICENSE.md")]
        if other != claude:
            retire_provider(f"claude-{other}-package-", [f"claude-{other}-legal-archive"])


# 3. npm notice overrides --------------------------------------------------

def fetch(url, limit=16 * 1024 * 1024):
    request = urllib.request.Request(url, headers={"User-Agent": "marsh-refresh-pins"})
    with urllib.request.urlopen(request, timeout=60) as response:
        data = response.read(limit + 1)
    if len(data) > limit:
        raise ValueError("oversized download: " + url)
    return data


def npm_metadata(name, version):
    return json.loads(fetch(f"https://registry.npmjs.org/{name.replace('/', '%2F')}/{version}", 4 * 1024 * 1024))


def lock_versions(lock):
    """{package name: {versions}} as installed (aliases resolve to their real version)."""
    result = {}
    for path, row in lock["packages"].items():
        if path and "version" in row:
            name = row.get("name") or path.rsplit("node_modules/", 1)[-1]
            result.setdefault(name, set()).add(row["version"])
    return result


def provider_index():
    return {n["file"]: n for n in load(BUNDLE / "agent-notices.json")["notices"]}


def place_provider_text(name, data, url):
    """Map a legal text to an existing identical canonical file, or add it."""
    digest = hashlib.sha256(data).hexdigest()
    index = provider_index()
    for file, row in sorted(index.items()):
        if row["sha256"] == digest:
            return "provider-notices/" + file, digest
    (BUNDLE / "provider-notices" / name).write_bytes(data)
    agents = load(BUNDLE / "agent-notices.json")
    agents["notices"].append({"file": name, "url": url, "sha256": digest})
    agents["notices"].sort(key=lambda n: n["file"])
    dump(BUNDLE / "agent-notices.json", agents)
    return "provider-notices/" + name, digest


def release_tag_commit(repository, version):
    """The commit of the release's tag (vVERSION, then VERSION), if there is one."""
    for tag in (f"v{core(version)}", core(version)):
        try:
            return inventory.tag_commit(repository, tag)
        except ValueError:
            continue
    return None


def follow_override(name, old, new, entries, canonical):
    """The override entries for NAME@NEW, from those reviewed for NAME@OLD."""
    result, mappings = [], {}
    metadata = None
    for entry in entries:
        file = entry["file"]
        match = RAW.match(entry.get("url", ""))
        if match and core(old) in file:
            new_file = file.replace(core(old), core(new))
            index = provider_index()
            if new_file in index:  # e.g. Codex's legal files, recorded with its release
                row = index[new_file]
                result.append({"file": new_file, "sha256": row["sha256"], "url": row["url"]})
                mappings[new_file] = "provider-notices/" + new_file
                continue
            metadata = metadata or npm_metadata(name, new)
            commit = metadata.get("gitHead") or release_tag_commit(match.group(1), new)
            if not commit:
                raise ValueError(f"{name}@{new}: neither a gitHead nor a v{core(new)} tag in "
                                 f"{match.group(1)} to acquire {match.group(3)} from")
            url = f"https://raw.githubusercontent.com/{match.group(1)}/{commit}/{match.group(3)}"
            target, digest = place_provider_text(new_file, fetch(url), url)
            result.append({"file": new_file, "sha256": digest, "url": url})
            mappings[new_file] = target
        elif not match:
            # A standard text selected by the package's own declaration.
            result.append(entry)
            if file in canonical:
                mappings[file] = canonical[file]
        else:
            raise ValueError(f"{name}@{old}: override {file} does not name its version")
    return result, mappings


def check_locks():
    """Locks resolve from the public registry: a private mirror's URLs would
    be unreachable for the image build and every other caller."""
    for lock in sorted(ROOT.glob("kits/*/package-lock.json")) + [ROOT / "packaging/shell/pi/package-lock.json"]:
        for path, row in json.loads(lock.read_text())["packages"].items():
            resolved = row.get("resolved", "")
            if resolved and not resolved.startswith("https://registry.npmjs.org/"):
                raise ValueError(f"{lock.relative_to(ROOT)}: {path} resolves from {resolved.split('/-/')[0]}, "
                                 "not https://registry.npmjs.org/; regenerate the lock against the public registry")


def refresh_overrides(base):
    for kit in NPM_KITS:
        path = ROOT / "kits" / kit / "notices/overrides.json"
        overrides = load(path)
        installed = lock_versions(load(ROOT / "kits" / kit / "package-lock.json"))
        declared_before = license_fields(base, kit)
        declared_now = license_fields(ROOT, kit)
        canonical = overrides.pop("$canonical_notice_files", {})
        updated, used = {}, {}
        for key, entries in sorted(overrides.items()):
            name, version = key.rsplit("@", 1)
            versions = installed.get(name, set())
            if version in versions:
                updated[key] = entries
                used.update({e["file"]: canonical[e["file"]] for e in entries if e["file"] in canonical})
                continue
            if not versions:
                log(f"{kit}: {key} is no longer installed; override dropped")
                continue
            suffix = version[len(core(version)):]
            matching = [v for v in versions if v[len(core(v)):] == suffix]
            if not matching:
                raise ValueError(f"{kit}: {key}: no installed {name} version like it to follow")
            new = max(matching, key=natural)
            if any("selection" in e for e in entries) and \
                    declared_before.get((name, version)) != declared_now.get((name, new)):
                raise ValueError(f"{kit}: {name} {version} -> {new} changed its declared license; "
                                 "review the standard text it selects")
            log(f"{kit}: {name} override follows {version} -> {new}")
            entries, mappings = follow_override(name, version, new, entries, canonical)
            updated[f"{name}@{new}"] = entries
            used.update(mappings)
        document = {"$canonical_notice_files": dict(sorted(used.items()))}
        document.update(sorted(updated.items()))
        dump(path, document)


def license_fields(root, kit):
    path = pathlib.Path(root) / "kits" / kit / "package-lock.json"
    if not path.is_file():
        return {}
    result = {}
    for key, row in json.loads(path.read_text())["packages"].items():
        if key and "version" in row:
            name = row.get("name") or key.rsplit("node_modules/", 1)[-1]
            result[(name, row["version"])] = row.get("license")
    return result


def core(version):
    match = re.match(r"\d+(\.\d+)*", version)
    return match.group(0) if match else version


def natural(version):
    return [int(p) if p.isdigit() else p for p in re.split(r"[.-]", version)]


# 4. Adapter payloads ------------------------------------------------------

def adapter_payloads(cache, work):
    output = work / "adapters"
    run(sys.executable, NOTICES / "collect-adapter-payloads.py", "--source-root", os.path.realpath(ROOT),
        "--cache", cache, "--output", output)
    for text in (output / "texts").iterdir():
        target = BUNDLE / "texts" / text.name
        if not target.exists():
            shutil.copyfile(text, target)
    shutil.copyfile(output / "adapter-payloads.json", BUNDLE / "adapter-payloads.json")


# 5. Images ----------------------------------------------------------------

def build_args(descriptor: pathlib.Path):
    """--build-arg NAME=DEFAULT for each descriptor arg that names a buildArg."""
    text = descriptor.read_text()
    result = []
    for block in re.finditer(r"^  (\w+):\n((?:    .*\n)+)", text, re.M):
        body = block.group(2)
        arg = re.search(r"^    buildArg: (\S+)", body, re.M)
        default = re.search(r'^    default: "?([^"\n]+)"?', body, re.M)
        if arg and default:
            result += ["--build-arg", f"{arg.group(1)}={default.group(1)}"]
    return result


def copy_tree(source, destination):
    shutil.copytree(source, destination, ignore=shutil.ignore_patterns("__pycache__", "*.pyc"),
                    dirs_exist_ok=True)


def images(docker, work):
    stage = work / "stage"
    run(sys.executable, ROOT / "scripts/stage-kits.py", "--commands", ROOT / "packaging/commands.json",
        "--output", stage)
    contexts = []
    for kit in ("marsh-shell", "marsh-pi", "marsh-claude", "marsh-codex"):
        context = stage / "kits" / kit
        name = kit.split("-", 1)[1]
        contexts.append((kit, context, context / f"{name}.dockerfile", build_args(context / f"{name}.yaml")))
    fixture = work / "fixture"
    copy_tree(ROOT / "tests/acceptance/fixture", fixture)
    copy_tree(BUNDLE, fixture / "dhi-notices")
    copy_tree(ROOT / "packaging/image-base", fixture / "image-base")
    contexts.append(("marsh-fixture", fixture, fixture / "fixture.dockerfile",
                     build_args(fixture / "fixture.yaml")))
    shell = work / "shell"
    copy_tree(ROOT / "packaging/shell", shell)
    copy_tree(ROOT / "packaging/image-repair", shell / "image-repair")
    copy_tree(ROOT / "packaging/image-base", shell / "image-base")
    copy_tree(BUNDLE, shell / "dhi-notices")
    base = (ROOT / "packaging/shell-image").read_text().strip()
    contexts.append(("marsh-shell-template", shell, shell / "Dockerfile",
                     ["--build-arg", f"SHELL_BASE_IMAGE={base}"]))
    references = []
    for name, context, dockerfile, arguments in contexts:
        tag = f"marsh-refresh-pins-{name}:image"
        run(docker, "buildx", "build", "--platform", "linux/arm64", "--load", "--target", "image",
            "--provenance=false", *arguments, "-t", tag, "-f", dockerfile, context)
        references.append(f"{name}={tag}")
    return references


def image_records(docker, go, references, work):
    go_input = NOTICES / "go-input.json"
    before = go_input.read_bytes()
    run(sys.executable, NOTICES / "inventory.py", "--docker", docker, *references)
    modules = load(BUNDLE / "go-module-notices.json")
    if go_input.read_bytes() != before or \
            modules.get("input_sha256") != hashlib.sha256(go_input.read_bytes()).hexdigest():
        output = work / "go"
        run(sys.executable, NOTICES / "collect-go.py", "--go", go, "--output", output)
        for text in (output / "texts").iterdir():
            target = BUNDLE / "texts" / text.name
            if not target.exists():
                shutil.copyfile(text, target)
        shutil.copyfile(output / "go-module-notices.json", BUNDLE / "go-module-notices.json")


def legal_receipt():
    record = BUNDLE / "source-records/dhi-base-image-legal.json"
    primary = load(BUNDLE / "image-primary-notices.json")
    digest = hashlib.sha256(record.read_bytes()).hexdigest()
    if primary[0]["receipt_sha256"] != digest:
        primary[0]["receipt_sha256"] = digest
        dump(BUNDLE / "image-primary-notices.json", primary)


# 7. Report -----------------------------------------------------------------

def base_tree(ref, destination):
    paths = ["packaging/dhi-notices", "kits", "packaging/shell/pi"]
    archive = subprocess.run(["git", "archive", "--format=tar", ref, "--", *paths], cwd=ROOT,
                             check=True, capture_output=True).stdout
    destination.mkdir(parents=True)
    with tarfile.open(fileobj=__import__("io").BytesIO(archive)) as tar:
        tar.extractall(destination, filter="data")
    return destination


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--base", default=os.environ.get("REFRESH_BASE", "origin/main"),
                        help="git ref the license delta compares with (default origin/main)")
    parser.add_argument("--cache", type=pathlib.Path,
                        default=pathlib.Path(os.environ.get("REFRESH_CACHE", pathlib.Path.home() / ".cache/marsh-refresh-pins")))
    parser.add_argument("--report-dir", type=pathlib.Path, default=ROOT / "target/refresh-pins")
    parser.add_argument("--docker", default="docker")
    parser.add_argument("--go", default=shutil.which("go") or "go")
    parser.add_argument("--skip-images", action="store_true",
                        help="leave the image records alone (no Docker); the delta still covers the rest")
    args = parser.parse_args()
    cache = private(args.cache)
    with tempfile.TemporaryDirectory(prefix="marsh-refresh-pins-") as temporary:
        work = private(pathlib.Path(temporary))
        before = base_tree(args.base, work / "base")
        check_locks()
        run(sys.executable, ROOT / "scripts/refresh-agent-checksums.py")
        agent_records(cache, work)
        refresh_overrides(before)
        adapter_payloads(cache, work)
        if not args.skip_images:
            image_records(args.docker, args.go, images(args.docker, work), work)
        legal_receipt()
        removed = bundle_refs.prune(BUNDLE)
        if removed:
            log(f"deleted {len(removed)} unreferenced notice texts")
        for cache_dir in NOTICES.rglob("__pycache__"):
            shutil.rmtree(cache_dir)
        args.report_dir.mkdir(parents=True, exist_ok=True)
        status = subprocess.run([sys.executable, ROOT / "scripts/license-delta.py", "--before", before,
                                 "--json", args.report_dir / "license-delta.json",
                                 "--markdown", args.report_dir / "license-delta.md"], cwd=ROOT).returncode
    if status not in (0, 2):
        return 1
    return status


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"refresh-pins: {error}", file=sys.stderr)
        sys.exit(1)
