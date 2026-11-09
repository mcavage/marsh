#!/usr/bin/env python3
"""Build the `marsh-svc-engine` image (packaging/svc-engine) into a tar and a
digest pin, both shipped in the guest artifacts.

The services VM loads the tar into its boot engine with `docker load` and runs
the image by id, never by tag, never pulled. The pin file records the image id
(manifest digest), its config id and the tar's sha256; marsh verifies all three
before running anything. dockerd and containerd must be at the versions the
shell image pins (packaging/shell/Dockerfile); this script refuses otherwise.

  prepare-svc-engine-image.py --output DIR [--docker docker]
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]
DIGEST = re.compile(r"sha256:[0-9a-f]{64}\Z")
IMMUTABLE = re.compile(r"(?![a-z]+://)[a-z0-9][a-z0-9._:/-]*@sha256:[0-9a-f]{64}\Z")
PINNED = re.compile(r"(docker-ce|docker-ce-cli|containerd\.io)=([^\s\\]+)")
ARCHIVE = "svc-engine.tar"
PIN = "svc-engine-image.json"


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def engine_pins(dockerfile: Path) -> dict[str, str]:
    return dict(PINNED.findall(dockerfile.read_text()))


def check_versions() -> dict[str, str]:
    shell = engine_pins(ROOT / "packaging/shell/Dockerfile")
    engine = engine_pins(ROOT / "packaging/svc-engine/Dockerfile")
    if not shell or engine != shell:
        raise ValueError(
            f"svc-engine must pin the shell image's dockerd and containerd: shell={shell} engine={engine}")
    return engine


def oci_ids(archive: Path) -> tuple[str, str]:
    """(manifest digest, config digest) of the one image in an OCI layout tar."""
    with tarfile.open(archive) as tar:
        def blob(name: str) -> bytes:
            member = tar.extractfile(name)
            if member is None:
                raise ValueError(f"{name} missing from {archive}")
            return member.read()
        index = json.loads(blob("index.json"))
        manifests = index.get("manifests", [])
        if len(manifests) != 1:
            raise ValueError("expected exactly one image in the archive")
        image_id = manifests[0]["digest"]
        manifest_blob = blob("blobs/sha256/" + image_id.split(":", 1)[1])
        if "sha256:" + hashlib.sha256(manifest_blob).hexdigest() != image_id:
            raise ValueError("archive manifest does not match its digest")
        config_id = json.loads(manifest_blob)["config"]["digest"]
    if not DIGEST.fullmatch(image_id) or not DIGEST.fullmatch(config_id):
        raise ValueError("archive carries malformed digests")
    return image_id, config_id


def atomic_write(path: Path, data: bytes) -> None:
    temporary = path.with_name(f".{path.name}.{os.getpid()}")
    temporary.write_bytes(data)
    os.replace(temporary, path)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--docker", default="docker")
    parser.add_argument("--output", type=Path, required=True,
                        help="directory receiving svc-engine.tar and svc-engine-image.json")
    args = parser.parse_args()
    base = (ROOT / "packaging/shell-image").read_text().strip()
    if not IMMUTABLE.fullmatch(base):
        raise ValueError("packaging/shell-image must pin the upstream DHI input by digest")
    versions = check_versions()
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="marsh-svc-engine-") as temporary:
        context = Path(temporary) / "context"
        context.mkdir()
        (context / "Dockerfile").write_bytes((ROOT / "packaging/svc-engine/Dockerfile").read_bytes())
        built = Path(temporary) / ARCHIVE
        # An OCI archive without a name: the image has no tag to resolve.
        command = [args.docker, "buildx", "build", "--platform", "linux/arm64",
                   "--provenance=false", "--build-arg", f"SVC_ENGINE_BASE={base}",
                   "--output", f"type=oci,dest={built}", str(context)]
        print("+ " + " ".join(command), file=sys.stderr, flush=True)
        subprocess.run(command, check=True, stdout=sys.stderr)
        image_id, config_id = oci_ids(built)
        pin = {"schema": "marsh.svc-engine-image/v1", "platform": "linux/arm64",
               "image_id": image_id, "config_id": config_id,
               "archive": ARCHIVE, "archive_sha256": sha256_file(built),
               "archive_bytes": built.stat().st_size, "base": base,
               "versions": {"docker-ce": versions["docker-ce"],
                            "containerd.io": versions["containerd.io"]}}
        staged = args.output / f".{ARCHIVE}.{os.getpid()}"
        shutil.copyfile(built, staged)
        os.replace(staged, args.output / ARCHIVE)
        atomic_write(args.output / PIN, (json.dumps(pin, indent=2) + "\n").encode())
    print(image_id)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        raise SystemExit(f"prepare-svc-engine-image: {error}") from error
