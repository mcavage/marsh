#!/usr/bin/env python3
"""Build provenance receipts (docs/design/test-breaks.md, "Releases").

The receipt is taken from the build itself. `build` runs the cargo command it is
given with `--message-format=json-render-diagnostics` and records what cargo
reports: every unit it compiled with the features it enabled for that unit
(`compiler-artifact` messages, fresh or not), and the executable each binary
target produced, with its sha256. Nothing about features or artifacts is
supplied by the caller. `verify` (scripts/dist.sh) refuses to package an
artifact unless the receipt is well formed, matches the artifact's hash, and no
unit of its build enabled a forbidden feature.

  scripts/provenance.py build --manifest FILE [--reset] [--label TEXT] \
      [--rename BIN=NAME]... -- cargo build ...
  scripts/provenance.py verify --manifest FILE ARTIFACT_PATH...

Build groups are content-addressed (the id is a hash of the units and their
features) and are never rewritten, so an artifact cannot be relabelled by a
later build. Without `--reset`, `build` refuses to add an artifact name the
manifest already has. `--reset` starts a new manifest (the first build of a
set). `verify` fails closed: an unknown version, a missing or inconsistent
group, a missing field, or a group whose id does not match its content refuses
the artifacts that depend on it.
"""

import argparse
import hashlib
import json
import os
import subprocess
import sys

FORBIDDEN_FEATURES = ("test-breaks", "test_breaks")
VERSION = 2


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def group_id(units):
    canonical = json.dumps(units, sort_keys=True, separators=(",", ":"))
    return "g-" + hashlib.sha256(canonical.encode()).hexdigest()[:16]


def forbidden_in(units):
    return sorted(f"{name}/{feature}" for name, features in units.items() for feature in features if feature in FORBIDDEN_FEATURES)


def package_name(package_id):
    """Package name from a cargo package id (both the current and the older form)."""
    if "#" in package_id:
        location, fragment = package_id.rsplit("#", 1)
        if "@" in fragment:
            return fragment.split("@", 1)[0]
        return location.rstrip("/").rsplit("/", 1)[-1]
    return package_id.split(" ", 1)[0]


def build(arguments, command):
    if not command:
        sys.exit("provenance: build needs a cargo command after --")
    if arguments.reset:
        try:
            os.remove(arguments.manifest)
        except FileNotFoundError:
            pass
    manifest = {"version": VERSION, "groups": {}, "artifacts": {}}
    if os.path.exists(arguments.manifest):
        try:
            with open(arguments.manifest) as handle:
                manifest = json.load(handle)
        except ValueError:
            sys.exit(f"provenance: {arguments.manifest} is not valid JSON; use --reset to start over")
        if manifest.get("version") != VERSION or not isinstance(manifest.get("groups"), dict) or not isinstance(manifest.get("artifacts"), dict):
            sys.exit(f"provenance: {arguments.manifest} is not a version {VERSION} manifest; use --reset to start over")
    renames = dict(item.split("=", 1) for item in arguments.rename)
    units = {}
    executables = []
    process = subprocess.Popen(command + ["--message-format=json-render-diagnostics"], stdout=subprocess.PIPE, text=True)
    for line in process.stdout:
        try:
            message = json.loads(line)
        except ValueError:
            sys.stdout.write(line)
            continue
        if message.get("reason") != "compiler-artifact":
            continue
        name = package_name(message["package_id"])
        units.setdefault(name, set()).update(message.get("features", []))
        if message.get("executable") and "bin" in message["target"]["kind"]:
            executables.append((message["target"]["name"], message["executable"], name))
    status = process.wait()
    if status != 0:
        return status
    if not units or not executables:
        sys.exit("provenance: the build reported no units or no executables; no receipt written")
    units = {name: sorted(features) for name, features in sorted(units.items())}
    gid = group_id(units)
    manifest["groups"].setdefault(gid, {"label": arguments.label or "", "command": " ".join(command), "units": units})
    for binary, path, package in executables:
        name = renames.get(binary, os.path.basename(path))
        if name in manifest["artifacts"]:
            sys.exit(f"provenance: {arguments.manifest} already has an artifact named {name}; refusing to rebind it (use --reset to start a new manifest)")
        manifest["artifacts"][name] = {"sha256": sha256_file(path), "group": gid, "package": package, "bin": binary}
    os.makedirs(os.path.dirname(os.path.abspath(arguments.manifest)), exist_ok=True)
    temporary = arguments.manifest + ".tmp"
    with open(temporary, "w") as handle:
        json.dump(manifest, handle, indent=2, sort_keys=True)
        handle.write("\n")
    os.replace(temporary, arguments.manifest)
    forbidden = forbidden_in(units)
    verdict = f"forbidden features enabled: {', '.join(forbidden)}" if forbidden else "no forbidden features"
    print(f"provenance: {arguments.manifest}: {len(executables)} executable(s), group {gid}: {verdict}", file=sys.stderr)
    return 0


def group_problem(manifest, gid):
    """Why a group cannot be trusted, or None."""
    group = manifest["groups"].get(gid)
    if not isinstance(group, dict):
        return f"its build group {gid} is missing from the manifest"
    units = group.get("units")
    if (
        not isinstance(units, dict)
        or not units
        or not all(isinstance(name, str) and isinstance(features, list) and all(isinstance(f, str) for f in features) for name, features in units.items())
    ):
        return f"its build group {gid} has no usable feature data"
    if group_id(units) != gid:
        return f"its build group {gid} does not match its recorded features"
    return None


def verify(arguments):
    paths = arguments.artifact
    manifest = None
    top_problem = None
    try:
        with open(arguments.manifest) as handle:
            manifest = json.load(handle)
    except FileNotFoundError:
        top_problem = f"there is no build receipt {arguments.manifest}"
    except ValueError:
        top_problem = f"the build receipt {arguments.manifest} is not valid JSON"
    if manifest is not None:
        if not isinstance(manifest, dict) or manifest.get("version") != VERSION:
            top_problem = f"the build receipt {arguments.manifest} has an unknown version"
        elif not isinstance(manifest.get("groups"), dict) or not isinstance(manifest.get("artifacts"), dict):
            top_problem = f"the build receipt {arguments.manifest} has no groups or artifacts"
    if top_problem:
        for path in paths:
            print(f"provenance: {path}: {top_problem}; refusing to package it", file=sys.stderr)
        return 1
    failed = False
    for gid in sorted(manifest["groups"]):
        problem = group_problem(manifest, gid)
        if problem:
            print(f"provenance: {arguments.manifest}: {problem}; refusing to package its artifacts", file=sys.stderr)
            failed = True
            continue
        forbidden = forbidden_in(manifest["groups"][gid]["units"])
        if forbidden:
            print(f"provenance: {arguments.manifest}: build group {gid} enabled {', '.join(forbidden)}; refusing to package it", file=sys.stderr)
            failed = True
    for path in paths:
        entry = manifest["artifacts"].get(os.path.basename(path))
        if not isinstance(entry, dict) or not all(isinstance(entry.get(key), str) and entry[key] for key in ("sha256", "group", "package")):
            reason = "is not in the build receipt" if entry is None else "has an unusable receipt entry"
            print(f"provenance: {path} {reason}; refusing to package it", file=sys.stderr)
            failed = True
            continue
        problem = group_problem(manifest, entry["group"])
        if problem:
            print(f"provenance: {path}: {problem}; refusing to package it", file=sys.stderr)
            failed = True
            continue
        units = manifest["groups"][entry["group"]]["units"]
        if entry["package"] not in units:
            print(f"provenance: {path}: its package {entry['package']} is not in its build group {entry['group']}; refusing to package it", file=sys.stderr)
            failed = True
            continue
        if not os.path.isfile(path):
            print(f"provenance: {path} is missing; refusing to package it", file=sys.stderr)
            failed = True
            continue
        if sha256_file(path) != entry["sha256"]:
            print(f"provenance: {path} does not match {arguments.manifest} (not the artifact that build produced); refusing to package it", file=sys.stderr)
            failed = True
            continue
        forbidden = forbidden_in(units)
        if forbidden:
            print(f"provenance: {path} was built with test-breaks (build group {entry['group']}: {', '.join(forbidden)}); refusing to package it", file=sys.stderr)
            failed = True
        else:
            print(f"provenance: ok {path}")
    return 1 if failed else 0


def main():
    argv = sys.argv[1:]
    command = []
    if "--" in argv:
        split = argv.index("--")
        argv, command = argv[:split], argv[split + 1 :]
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    builder = commands.add_parser("build")
    builder.add_argument("--manifest", required=True)
    builder.add_argument("--reset", action="store_true")
    builder.add_argument("--label", default="")
    builder.add_argument("--rename", action="append", default=[], metavar="BIN=NAME")
    verifier = commands.add_parser("verify")
    verifier.add_argument("--manifest", required=True)
    verifier.add_argument("artifact", nargs="+")
    arguments = parser.parse_args(argv)
    if arguments.command == "build":
        return build(arguments, command)
    if command:
        parser.error("verify takes no command")
    return verify(arguments)


if __name__ == "__main__":
    sys.exit(main())
