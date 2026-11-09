#!/usr/bin/env python3
"""The services E2E image cache (H-11).

  image_cache.py save [--out DIR] [--extra FILE] [--rebuild-local] [--update-pin]
  image_cache.py load [TARGET] [--out DIR] [--extra FILE] [--force]
  image_cache.py list [--extra FILE]

save   pulls (linux/arm64) every image in images.lock, plus any in --extra, with
       the Mac's Docker, and writes one tar per image to DIR (default
       <repo>/target/nfx/images/). Existing tars are kept.
load   streams each tar into a services engine with `docker load`, skipping
       images the engine already has (--force loads anyway). The write is
       documented setup, not an oracle: it goes through the harness's `vx`
       authority, never through a job's Docker endpoint (the tool refuses a
       DOCKER_HOST that names one), and each load is recorded with --record.
list   prints the image set and the egress image check.

TARGET selects the engine that receives the images; the socket is a parameter
because the production engine's path is fixed by the services VM design:

  --vm VM             run inside this sandbox: `sbx exec -i VM docker ...`
  --engine NAME       and inside this container of the VM: `docker exec -i NAME docker ...`
                      (the production engine in marsh-svc-engine)
  --socket URL        pass `-H URL` to that docker (e.g. unix:///run/prod.sock)
  --sudo              run the VM-side command as root (`sbx exec -i VM sudo ...`, as `vx` does)
  --record FILE       append one JSON line per load (the setup-write record)
  --sbx PATH          the sbx binary (default: sbx)
  --cmd 'ARGV...'     alternatively, any command line ending in the docker CLI that
                      reaches the engine (split like a shell); `-H SOCKET`, if
                      given, and the docker subcommand are appended

With no TARGET option, `docker load` runs on the Mac's Docker (a dry run).
Examples:
  image_cache.py load --vm $SVC --sudo --engine marsh-svc-engine --socket unix:///run/prod.sock
  image_cache.py load --vm nfx-h1b-plain
"""
import argparse
import io
import json
import os
import pathlib
import re
import shlex
import subprocess
import sys
import tarfile
import time

HERE = pathlib.Path(__file__).resolve().parent
REPO = HERE.parents[3]
DEFAULT_OUT = REPO / "target/nfx/images"
REF = re.compile(r"[a-z0-9][a-z0-9._/:-]*@sha256:[0-9a-f]{64}\Z")


def read_refs(path: pathlib.Path) -> list[str]:
    refs = []
    for line in path.read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        if not REF.fullmatch(line):
            raise SystemExit(f"image_cache: not a digest-pinned reference: {line!r} ({path})")
        refs.append(line)
    return refs


def egress_image() -> str:
    for line in (HERE / "egress.env").read_text().splitlines():
        if line.startswith("EGRESS_ABSENT_IMAGE="):
            return line.split("=", 1)[1].strip()
    raise SystemExit("image_cache: egress.env has no EGRESS_ABSENT_IMAGE")


def image_set(extra: pathlib.Path | None) -> list[str]:
    refs = read_refs(HERE / "images.lock") + (read_refs(extra) if extra else [])
    uncached = egress_image()
    if uncached in refs or uncached.split("@")[1] in {ref.split("@")[1] for ref in refs}:
        raise SystemExit(f"image_cache: the egress image must not be in the cached set: {uncached}")
    return refs


def local_images() -> list[dict]:
    return json.loads((HERE / "local-images.json").read_text())["images"]


def local_tar_name(tag: str) -> str:
    return re.sub(r"[^A-Za-z0-9._-]", "_", tag) + ".tar"


def tar_digest(path: pathlib.Path) -> str:
    """The digest of the image a `docker save` tar holds (its index.json entry)."""
    try:
        with tarfile.open(path) as archive:
            return json.load(archive.extractfile("index.json"))["manifests"][0]["digest"]
    except (tarfile.TarError, KeyError, ValueError, OSError) as error:
        raise SystemExit(f"image_cache: {path.name} is not a readable docker-save tar ({type(error).__name__}); delete it and save again")


def verify_tar(path: pathlib.Path, expected: str) -> None:
    found = tar_digest(path)
    if found != expected:
        raise SystemExit(f"image_cache: {path.name} holds {found}, pinned {expected}: stale or changed; delete it and save again")


def build_local(entry: dict, update_pin: bool) -> None:
    """Reproducible build (timestamps rewritten, exact inputs); its image ID must equal the pin."""
    import shutil
    import tempfile
    with tempfile.TemporaryDirectory(prefix="nfx-build-") as context:
        context = pathlib.Path(context)
        shutil.copytree(HERE / entry["context"], context, dirs_exist_ok=True)
        for source, destination in entry.get("copy", {}).items():
            origin = HERE / source
            target = context / destination
            target.parent.mkdir(parents=True, exist_ok=True)
            if origin.is_dir():
                shutil.copytree(origin, target, ignore=shutil.ignore_patterns("generate.py", "__pycache__"))
            else:
                shutil.copy2(origin, target)
        subprocess.run(["docker", "buildx", "build", "--platform", "linux/arm64", "--provenance=false", "--sbom=false",
                        "--build-arg", "SOURCE_DATE_EPOCH=0", "--output",
                        f"type=docker,name={entry['tag']},rewrite-timestamp=true", "-q", str(context)],
                       check=True, stdout=subprocess.DEVNULL, env={**os.environ, "SOURCE_DATE_EPOCH": "0"})
    built = subprocess.run(["docker", "image", "inspect", "--format", "{{.Id}}", entry["tag"]],
                           capture_output=True, text=True, check=True).stdout.strip()
    if built != entry.get("id"):
        if update_pin:
            pin_local(entry["tag"], built)
            entry["id"] = built
        else:
            raise SystemExit(f"image_cache: {entry['tag']} built as {built}, pinned {entry.get('id')}: an input changed; "
                             "review the change, then rerun save with --update-pin")


def pin_local(tag: str, image_id: str) -> None:
    path = HERE / "local-images.json"
    doc = json.loads(path.read_text())
    for entry in doc["images"]:
        if entry["tag"] == tag:
            entry["id"] = image_id
            entry["ref"] = f"{tag}@{image_id}"
    path.write_text(json.dumps(doc, indent=1) + "\n")


def tar_name(ref: str) -> str:
    name, digest = ref.split("@sha256:")
    return re.sub(r"[^A-Za-z0-9._-]", "_", name) + "-" + digest[:12] + ".tar"


def name_and_tag(ref: str) -> tuple[str, str]:
    """('docker.io/library/postgres', '17') for 'postgres:17@sha256:...'."""
    name = ref.split("@", 1)[0]
    tag = "latest"
    last = name.rsplit("/", 1)[-1]
    if ":" in last:
        name, tag = name.rsplit(":", 1)
    first = name.split("/", 1)[0]
    if "/" not in name:
        name = "docker.io/library/" + name
    elif "." not in first and ":" not in first and first != "localhost":
        name = "docker.io/" + name
    return name, tag


def name_tar(path: pathlib.Path, ref: str) -> None:
    """`docker save REF@digest` writes an unnamed image, which a digest reference
    (compose `image: postgres:17@sha256:...`) cannot find after `docker load`.
    Add the name and tag to index.json and manifest.json, keeping the index digest."""
    name, tag = name_and_tag(ref)
    rewritten = path.with_suffix(".tar.named")
    with tarfile.open(path) as source, tarfile.open(rewritten, "w") as target:
        for member in source:
            data = source.extractfile(member) if member.isfile() else None
            if member.name == "index.json":
                index = json.load(data)
                for entry in index["manifests"]:
                    entry.setdefault("annotations", {}).update({
                        "io.containerd.image.name": f"{name}:{tag}",
                        "org.opencontainers.image.ref.name": tag})
                raw = json.dumps(index, separators=(",", ":")).encode()
                member.size = len(raw)
                target.addfile(member, io.BytesIO(raw))
            elif member.name == "manifest.json":
                manifest = json.load(data)
                short = name.removeprefix("docker.io/library/").removeprefix("docker.io/")
                for entry in manifest:
                    entry["RepoTags"] = [f"{short}:{tag}"]
                raw = json.dumps(manifest, separators=(",", ":")).encode()
                member.size = len(raw)
                target.addfile(member, io.BytesIO(raw))
            else:
                target.addfile(member, data)
    rewritten.replace(path)


def docker_prefix(args: argparse.Namespace) -> list[str]:
    docker = ["docker"] + (["-H", args.socket] if args.socket else [])
    if args.cmd:
        return shlex.split(args.cmd) + (["-H", args.socket] if args.socket else [])
    if not args.vm:
        if args.engine:
            raise SystemExit("image_cache: --engine needs --vm")
        return docker
    inner = (["docker", "exec", "-i", args.engine] if args.engine else []) + docker
    return [args.sbx, "exec", "-i", args.vm, *(["sudo"] if args.sudo else []), *inner]


def save(args: argparse.Namespace) -> int:
    out = args.out
    out.mkdir(parents=True, exist_ok=True)
    for ref in image_set(args.extra):
        target = out / tar_name(ref)
        if target.exists():
            verify_tar(target, "sha256:" + ref.split("@sha256:")[1])
            print(f"have  {target.name} (verified {tar_digest(target)[:19]})")
            continue
        subprocess.run(["docker", "pull", "--quiet", "--platform", "linux/arm64", ref],
                       check=True, stdout=subprocess.DEVNULL)
        partial = target.with_suffix(".tar.part")
        subprocess.run(["docker", "save", "-o", str(partial), ref], check=True)
        name_tar(partial, ref)
        verify_tar(partial, "sha256:" + ref.split("@sha256:")[1])
        partial.rename(target)
        print(f"saved {target.name} ({target.stat().st_size >> 20} MiB)")
    for entry in local_images():
        target = out / local_tar_name(entry["tag"])
        if args.rebuild_local and target.exists():
            target.unlink()
        if target.exists():
            verify_tar(target, entry["id"])
            print(f"have  {target.name} (verified {entry['id'][:19]})")
            continue
        build_local(entry, args.update_pin)
        partial = target.with_suffix(".tar.part")
        subprocess.run(["docker", "save", "-o", str(partial), entry["tag"]], check=True)
        verify_tar(partial, entry["id"])
        partial.rename(target)
        print(f"built {target.name} ({target.stat().st_size >> 20} MiB)")
    return 0


def load(args: argparse.Namespace) -> int:
    endpoint = os.environ.get("DOCKER_HOST", "")
    if "/run/marsh/" in endpoint or "marsh/docker.sock" in endpoint:
        raise SystemExit("image_cache: DOCKER_HOST names a job's Docker endpoint; images are loaded "
                         "through vx (--vm/--engine/--socket), never through it")
    prefix = docker_prefix(args)
    for ref in image_set(args.extra):
        target = args.out / tar_name(ref)
        if not target.exists():
            raise SystemExit(f"image_cache: {target} is missing; run `image_cache.py save` first")
        verify_tar(target, "sha256:" + ref.split("@sha256:")[1])
        if not args.force:
            have = subprocess.run([*prefix, "image", "inspect", "--format", "{{.Id}}", ref],
                                  capture_output=True, text=True)
            if have.returncode == 0:
                print(f"have   {ref}")
                continue
        with target.open("rb") as stream:
            subprocess.run([*prefix, "load", "-q"], stdin=stream, check=True, stdout=subprocess.DEVNULL)
        print(f"loaded {ref}")
        record_load(args, prefix, ref, target)
    for entry in local_images():
        target = args.out / local_tar_name(entry["tag"])
        if not target.exists():
            raise SystemExit(f"image_cache: {target} is missing; run `image_cache.py save` first")
        verify_tar(target, entry["id"])
        have = subprocess.run([*prefix, "image", "inspect", "--format", "{{.Id}}", entry["ref"]], capture_output=True, text=True)
        if not args.force and have.returncode == 0 and have.stdout.strip() == entry["id"]:
            print(f"have   {entry['ref']}")
            continue
        with target.open("rb") as stream:
            subprocess.run([*prefix, "load", "-q"], stdin=stream, check=True, stdout=subprocess.DEVNULL)
        check = subprocess.run([*prefix, "image", "inspect", "--format", "{{.Id}}", entry["ref"]], capture_output=True, text=True)
        if check.returncode != 0 or check.stdout.strip() != entry["id"]:
            raise SystemExit(f"image_cache: after load, {entry['ref']} does not resolve to {entry['id']} in the engine")
        print(f"loaded {entry['ref']}")
        record_load(args, prefix, entry["ref"], target)
    return 0


def record_load(args, prefix, ref, target) -> None:
    if args.record:
        with args.record.open("a") as record:
            record.write(json.dumps({"setup_write": "docker load", "ref": ref, "tar": target.name,
                                     "via": prefix, "at": time.time()}) + "\n")


def list_(args: argparse.Namespace) -> int:
    for ref in image_set(args.extra):
        print(ref)
    for entry in local_images():
        print(f"{entry.get('ref', entry['tag'])}  # built here from {entry['context']}/")
    print(f"# egress image (never cached): {egress_image()}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("action", choices=["save", "load", "list"])
    parser.add_argument("--out", type=pathlib.Path, default=DEFAULT_OUT)
    parser.add_argument("--extra", type=pathlib.Path)
    parser.add_argument("--vm")
    parser.add_argument("--engine")
    parser.add_argument("--socket")
    parser.add_argument("--sudo", action="store_true")
    parser.add_argument("--record", type=pathlib.Path)
    parser.add_argument("--sbx", default="sbx")
    parser.add_argument("--cmd")
    parser.add_argument("--force", action="store_true")
    parser.add_argument("--rebuild-local", action="store_true",
                        help="rebuild the harness-built images and check them against their pins (run after editing their inputs)")
    parser.add_argument("--update-pin", action="store_true", help="accept a rebuilt local image's new ID as the pin")
    args = parser.parse_args()
    return {"save": save, "load": load, "list": list_}[args.action](args)


if __name__ == "__main__":
    sys.exit(main())
