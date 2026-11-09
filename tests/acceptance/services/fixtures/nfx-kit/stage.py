#!/usr/bin/env python3
"""Stage the nfx fixture Kits as local Kit v3 sources and write a commands.json.

    stage.py [--dest DIR] [--build]

Writes DEST/<hash>/{nfx,nfx-acp,nfx-acp2}/ (each: its descriptor, nfx.dockerfile,
agent.mjs from tests/acceptance/acp-fixture, and the canonical
packaging/image-base inputs the Dockerfile assembles from) plus
DEST/<hash>/commands.json mapping the three command names to those absolute
directories. <hash> names the inputs' content: changed inputs get a new path (a
registry change), as in harness.stage_acp_fixture. DEST defaults to
<repo>/target/nfx/kit. Prints the commands.json path.

A scope registers the Kits by copying or merging that file into its
MARSH_CONTROL_HOME/<scope>/commands.json (local Kits need kit.allowLocalKits,
which is on by default). marsh builds the image the first time a command runs,
like any local Kit. --build also runs the same build with `docker buildx build`
first (tag nfx-kit:local), to fail fast and warm the BuildKit cache.
"""
import argparse
import hashlib
import json
import os
import pathlib
import subprocess
import sys
import tempfile

HERE = pathlib.Path(__file__).resolve().parent
REPO = HERE.parents[4]
KITS = {"nfx": "nfx.yaml", "nfx-acp": "nfx-acp.yaml", "nfx-acp2": "nfx-acp2.yaml"}
SHARED = {"nfx.dockerfile": HERE / "nfx.dockerfile", "curlrc": HERE / "curlrc", "curl-wrapper": HERE / "curl-wrapper",
          "agent.mjs": REPO / "tests/acceptance/acp-fixture/agent.mjs"}


def inputs() -> dict[str, bytes]:
    files = {name: path.read_bytes() for name, path in SHARED.items()}
    files.update({"image-base/" + path.name: path.read_bytes()
                  for path in sorted((REPO / "packaging/image-base").iterdir()) if path.is_file()})
    return files


def stage(dest: pathlib.Path) -> pathlib.Path:
    shared = inputs()
    descriptors = {name: (HERE / yaml).read_bytes() for name, yaml in KITS.items()}
    hasher = hashlib.sha256()
    for name in sorted(shared):
        hasher.update(name.encode() + b"\0" + hashlib.sha256(shared[name]).digest())
    for name in sorted(descriptors):
        hasher.update(name.encode() + b"\0" + hashlib.sha256(descriptors[name]).digest())
    root = dest / hasher.hexdigest()[:16]
    commands = root / "commands.json"
    if commands.exists():
        return commands
    dest.mkdir(parents=True, exist_ok=True)
    staging = pathlib.Path(tempfile.mkdtemp(prefix=".nfx-kit-", dir=dest))
    for name, descriptor in descriptors.items():
        kit = staging / name
        for relative, data in {KITS[name]: descriptor, **shared}.items():
            (kit / relative).parent.mkdir(parents=True, exist_ok=True)
            (kit / relative).write_bytes(data)
    final = {name: str(root / name) for name in KITS}
    (staging / "commands.json").write_text(json.dumps(final, indent=1, sort_keys=True) + "\n")
    try:
        os.rename(staging, root)
    except OSError:
        if not commands.exists():
            raise
    return commands


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--dest", type=pathlib.Path, default=REPO / "target/nfx/kit")
    parser.add_argument("--build", action="store_true", help="also docker buildx build --load -t nfx-kit:local")
    args = parser.parse_args()
    commands = stage(args.dest.absolute())
    if args.build:
        kit = json.loads(commands.read_text())["nfx"]
        subprocess.run(["docker", "buildx", "build", "--load", "-t", "nfx-kit:local",
                        "-f", f"{kit}/nfx.dockerfile", kit], check=True)
    print(commands)
    return 0


if __name__ == "__main__":
    sys.exit(main())
