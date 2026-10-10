#!/usr/bin/env python3
"""Build/import the one shell template (repaired DHI shell + Rust + Pi) locally;
registry publication is opt-in.

The produced archive, config, platform manifest, and import are recorded.
--validate-only emits no runnable reference. Neither route creates a sandbox or
includes profile/credential files. Runtime identity is a separate live gate.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import uuid

import build_inputs as INPUTS
import owned_process as PROCESS
from image_observations import import_local_image, LocalImageImportError
from stock_sdk import validate_local_environment

ROOT = Path(__file__).resolve().parents[1]
DIGEST = re.compile(r"sha256:[0-9a-f]{64}\Z")
IMMUTABLE = re.compile(r"(?![a-z]+://)[a-z0-9][a-z0-9._:/-]*@sha256:[0-9a-f]{64}\Z")
REPOSITORY = re.compile(r"[a-z0-9][a-z0-9.-]*(?::[0-9]{1,5})?/[a-z0-9][a-z0-9._/-]*\Z")
TAG = re.compile(r"[A-Za-z0-9_][A-Za-z0-9._-]{0,127}\Z")


def file_hash(path: Path) -> str:
    return INPUTS.file_record(path)["sha256"]


def input_tree(root: Path) -> dict:
    """Use the same bounded, no-follow authority checks as Kit publication."""
    return {name: {"sha256": row["sha256"], "mode": row["mode"]}
            for name, row in INPUTS.tree_inventory(root)["files"].items()}


def copy_inputs(source: Path, destination: Path, budget=None) -> dict:
    before = input_tree(source)
    tree = INPUTS.tree_inventory(source)
    budget = budget if budget is not None else [INPUTS.STAGE_LIMIT]
    destination.mkdir(mode=0o755)
    for name in tree["directories"]:
        (destination / name).mkdir(mode=0o755, parents=True, exist_ok=True)
    for name in before:
        INPUTS.copy_regular(source / name, destination / name, budget)
    for path in [destination, *[p for p in destination.rglob("*") if p.is_dir()]]:
        path.chmod(0o755)
    expected = {name: {"sha256": row["sha256"], "mode": 0o644 | (row["mode"] & 0o111)}
                for name, row in before.items()}
    if input_tree(source) != before or input_tree(destination) != expected:
        raise ValueError(f"build inputs changed while staging: {source}")
    return before


def atomic_write(path: Path, value: str) -> None:
    with INPUTS.directory(path.parent, create=True) as parent:
        temporary = ".marsh-image-" + uuid.uuid4().hex
        try:
            fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=parent)
            with os.fdopen(fd, "w") as stream:
                stream.write(value)
                stream.flush()
                os.fsync(stream.fileno())
                os.fchmod(stream.fileno(), 0o644)
            os.replace(temporary, path.name, src_dir_fd=parent, dst_dir_fd=parent)
            os.fsync(parent)
        finally:
            try:
                os.unlink(temporary, dir_fd=parent)
            except FileNotFoundError:
                pass


def retain_failure(output: Path | None, *, phase: str, command: list[str], metadata: Path,
                   temporary_tag: str, inputs: dict, staged_inputs: dict, error: Exception,
                   publication: bool) -> None:
    """Bounded diagnostic trail, never a successful receipt or raw payload dump."""
    if output is None:
        return
    observed = {}
    try:
        value = json.loads(INPUTS.read_file(metadata))
        for key in ("containerimage.digest", "containerimage.config.digest"):
            digest = value.get(key)
            if isinstance(digest, str) and DIGEST.fullmatch(digest):
                observed[key] = digest
    except (ValueError, OSError, TypeError):
        pass
    record = {"schema": "marsh.image-preparation-failure/v1", "outcome": "failed", "phase": phase,
              "publication_may_have_occurred": publication, "build_argv": command,
              "owned_temporary_tag": temporary_tag, "buildx_metadata": observed,
              "source_inputs_sha256": INPUTS.sha(INPUTS.json_bytes(inputs)),
              "staged_inputs_sha256": INPUTS.sha(INPUTS.json_bytes(staged_inputs)),
              "failure_type": type(error).__name__, "failure": str(error)[:4096],
              "previous_output_untouched": phase != "publish-proof",
              "archive_retained": False,
              "remedy": "Retain this diagnostic and the named owned image. Retry the actual producer in a NEW output directory; never adopt this failure as a successful build."}
    if isinstance(error, LocalImageImportError):
        record["local_image_observations"] = error.observations
    encoded = json.dumps(record, indent=2) + "\n"
    if len(encoded.encode()) > 256 * 1024:
        # Commands and digests remain actionable even if a failing CLI produced
        # verbose output. Do not export arbitrary/unrelated stdout payloads.
        for entry in record.get("local_image_observations", {}).get("commands", []):
            entry.pop("stdout", None); entry.pop("stderr", None)
            entry["output_selection"] = "omitted to bound failure evidence"
        encoded = json.dumps(record, indent=2) + "\n"
    if len(encoded.encode()) > 256 * 1024:
        raise ValueError("failure metadata exceeded its bound; preserve the host tool log") from error
    atomic_write(output.with_name(output.name + ".failed-build.json"), encoded)


def validate_outputs(output: Path | None) -> None:
    if output is None:
        return
    for path in (output, output.with_name(output.name + ".build.json"), output.with_name(output.name + ".failed-build.json")):
        absolute = Path(os.path.abspath(path))
        if any(absolute.is_relative_to(ROOT / name) for name in ("scripts", "packaging", "kits", "crates", "vendor")):
            raise ValueError("image outputs must not overwrite source/build inputs")
        with INPUTS.directory(path.parent, create=True) as parent:
            try:
                info = os.stat(path.name, dir_fd=parent, follow_symlinks=False)
            except FileNotFoundError:
                continue
            if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                    or info.st_mode & 0o022 or info.st_nlink != 1):
                raise ValueError("image output must be an owner-controlled regular file")


def run(argv: list[str]) -> None:
    print("+ " + " ".join(argv), file=sys.stderr, flush=True)
    PROCESS.run(argv, cwd=ROOT, stdout=sys.stderr, check=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--docker", default="docker")
    parser.add_argument("--sbx", default="sbx")
    parser.add_argument("--repository", help="explicit registry/repository, without tag or digest")
    parser.add_argument("--tag", default="repaired",
                        help="moving tag to push with --repository (default: repaired); consumers pin by digest")
    parser.add_argument("--output", type=Path, help="packaged immutable shell-image file")
    parser.add_argument("--insecure-registry", action="store_true",
                        help="explicitly allow a local HTTP test registry")
    parser.add_argument("--validate-only", action="store_true")
    args = parser.parse_args()
    if not args.validate_only and not args.output:
        parser.error("--output is required; registry publication additionally requires explicit --repository")
    if not TAG.fullmatch(args.tag):
        parser.error("--tag must be a valid image tag")
    if args.repository is not None and not REPOSITORY.fullmatch(args.repository):
        parser.error("--repository must be registry/repository without a tag or digest")
    if args.insecure_registry and not args.repository:
        parser.error("--insecure-registry requires an explicit --repository")
    if not args.validate_only and not args.repository and not shutil.which(args.sbx):
        parser.error("local shell preparation requires stock SBX on PATH or --sbx /absolute/sbx")
    if not args.repository and not args.validate_only:
        validate_local_environment()
    base_file = ROOT / "packaging/shell-image"
    base = INPUTS.read_file(base_file).decode().strip()
    if not IMMUTABLE.fullmatch(base):
        raise ValueError("packaging/shell-image must pin the upstream DHI input by digest")
    source_recipe = ROOT / "packaging/shell/Dockerfile"
    recipe_hash = file_hash(source_recipe)
    base_hash = file_hash(base_file)
    script_hash = file_hash(Path(__file__))
    support_hashes = {name: file_hash(ROOT / "scripts" / name)
                      for name in ("build_inputs.py", "image_observations.py", "owned_process.py", "stock_sdk.py")}
    validate_outputs(args.output)
    with tempfile.TemporaryDirectory(prefix="marsh-shell-image-") as temporary:
        temporary_root = Path(temporary).resolve(strict=True)
        context = temporary_root / "context"
        context.mkdir()
        budget = [INPUTS.STAGE_LIMIT]
        INPUTS.copy_regular(source_recipe, context / "Dockerfile", budget)
        if file_hash(context / "Dockerfile") != recipe_hash:
            raise ValueError("shell Dockerfile changed while staging")
        repair_inputs = copy_inputs(ROOT / "packaging/image-repair", context / "image-repair", budget)
        notice_inputs = copy_inputs(ROOT / "packaging/dhi-notices/collected", context / "dhi-notices", budget)
        pi_inputs = copy_inputs(ROOT / "packaging/shell/pi", context / "pi", budget)
        staged_inputs = INPUTS.tree_inventory(context)
        metadata = temporary_root / "metadata.json"
        temporary_tag = "marsh-shell-build:" + uuid.uuid4().hex
        if args.validate_only:
            output = ["--output", "type=cacheonly"]
        elif args.repository:
            output = ["--output", f"type=image,name={args.repository}:{args.tag},push=true" +
                      (",registry.insecure=true" if args.insecure_registry else "")]
        else:
            output = ["--load", "--tag", temporary_tag]
        command = [args.docker, "buildx", "build", "--platform", "linux/arm64",
                   "--provenance=mode=max" if args.repository else "--provenance=false",
                   *(["--sbom=true"] if args.repository else []),
                   "--build-arg", f"SHELL_BASE_IMAGE={base}",
                   "--file", str(context / "Dockerfile"), *output,
                   "--metadata-file", str(metadata), str(context)]
        inputs = {"packaging/shell-image": base_hash, "packaging/shell/Dockerfile": recipe_hash,
                  "scripts/prepare-shell-image.py": script_hash, "packaging/image-repair": repair_inputs,
                  "packaging/dhi-notices/collected": notice_inputs, "packaging/shell/pi": pi_inputs,
                  "support_scripts": support_hashes}
        phase = "build"
        try:
            run(command)
            phase = "source-fence"
            if (file_hash(base_file) != base_hash or file_hash(source_recipe) != recipe_hash
                    or file_hash(Path(__file__)) != script_hash
                    or input_tree(ROOT / "packaging/image-repair") != repair_inputs
                    or input_tree(ROOT / "packaging/dhi-notices/collected") != notice_inputs
                    or input_tree(ROOT / "packaging/shell/pi") != pi_inputs
                    or support_hashes != {name: file_hash(ROOT / "scripts" / name) for name in support_hashes}):
                raise ValueError("shell image inputs changed during build")
            if args.validate_only:
                print("Validated repaired shell image; no publication or runnable reference emitted", file=sys.stderr)
                return
            phase = "read-buildx-metadata"
            observed = json.loads(INPUTS.read_file(metadata))
            digest = observed.get("containerimage.digest")
            if not isinstance(digest, str) or not DIGEST.fullmatch(digest):
                raise ValueError("Buildx did not report an immutable repaired-image manifest")
            local = None
            if args.repository:
                reference = f"{args.repository}@{digest}"
            else:
                phase = "local-import"
                reference, local = import_local_image(args.docker, args.sbx, temporary_tag, temporary_root, "shell")
            proof = {"schema": "marsh.prepared-shell-image/v1", "reference": reference,
                     "platform": "linux/arm64", "upstream_image": base,
                     "publication": "explicit-registry" if args.repository else "local-template",
                     "publication_effects": [reference] if args.repository else [], "local_image": local,
                     "insecure_registry": args.insecure_registry, "inputs": inputs,
                     "staged_inputs": staged_inputs, "build_argv": command, "buildx_metadata": observed}
            phase = "publish-proof"
            atomic_write(args.output.with_name(args.output.name + ".build.json"), json.dumps(proof, indent=2) + "\n")
            atomic_write(args.output, reference + "\n")
            args.output.with_name(args.output.name + ".failed-build.json").unlink(missing_ok=True)
            print(reference)
        except (ValueError, OSError, subprocess.SubprocessError, KeyError, TypeError) as error:
            retain_failure(args.output, phase=phase, command=command, metadata=metadata,
                           temporary_tag=temporary_tag, inputs=inputs, staged_inputs=staged_inputs,
                           error=error, publication=bool(args.repository and not args.validate_only))
            detail = "previous output preserved" if phase != "publish-proof" else "output publication incomplete; do not use it"
            raise ValueError(f"{phase} failed: {error}; {detail}" +
                             ("; registry publication may have occurred" if args.repository and not args.validate_only else "")) from error


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        raise SystemExit(f"prepare-shell-image: {error}") from error
