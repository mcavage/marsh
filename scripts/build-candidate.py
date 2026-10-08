#!/usr/bin/env python3
"""Observe an actual make build and issue a host-only source-to-package receipt.

Not a signing service: run this trusted wrapper on the host, never in a workload
with write access to the evidence directory. Existing binaries cannot be adopted.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time
import uuid
import owned_process as PROCESS

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tests/acceptance"))
from provenance import (SCHEMA, artifact_manifest, file_digest, host_only_path,
                        capture_source, verify_build_receipt, verify_shell_image_proof)

RUST_IMAGE = "rust:1.95-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("source-tree", "target-dir", "marsh", "guest-artifacts", "registry", "receipt"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--toolchain", default="1.95.0")
    parser.add_argument("--cargo", default="cargo")
    parser.add_argument("--rustc", default="rustc")
    parser.add_argument("--make", default="make")
    parser.add_argument("--docker", default="docker")
    parser.add_argument("--sbx", default="sbx", help="stock CLI for local template import (no VM allocation)")
    parser.add_argument("--host-features", default="")
    parser.add_argument("--rust-image", default=RUST_IMAGE)
    parser.add_argument("--shell-image-repository",
                        help="opt in to this publication repository; default is observed local import")
    parser.add_argument("--shell-image-insecure-registry", "--shell-image-insecure", dest="shell_image_insecure", action="store_true",
                        help="explicitly allow the selected local HTTP test registry")
    parser.add_argument("--verify-only", action="store_true")
    args = parser.parse_args()
    args.shell_image_repository = args.shell_image_repository or None
    source = args.source_tree.resolve()
    receipt = host_only_path(args.receipt, source)
    if args.verify_only:
        value = verify_build_receipt(receipt, source_tree=source, revision=args.source_revision,
                                     marsh=args.marsh, guest_artifacts=args.guest_artifacts)
        if (args.marsh.resolve() != args.target_dir.resolve() / "release/marsh" or
                str(args.registry.resolve(strict=True)) != value["build"]["registry"] or
                file_digest(args.registry) != value["build"]["registry_sha256"] or
                args.shell_image_repository != value["build"]["shell_image_repository"] or
                args.shell_image_insecure != value["build"]["shell_image_insecure"] or
                args.rust_image != value["build"]["rust_image"] or
                args.toolchain != value["build"]["environment"]["RUSTUP_TOOLCHAIN"] or
                f"HOST_FEATURES={args.host_features}" not in value["build"]["argv"]):
            raise ValueError("verify-only target/registry/build inputs mismatch")
        print(f"Verified observed build: {receipt}")
        return 0
    # Invalidate first, including toolchain, input, build, and publication failures.
    receipt.unlink(missing_ok=True)
    if args.shell_image_insecure and not args.shell_image_repository:
        raise ValueError("--shell-image-insecure-registry requires explicit publication repository")
    source.resolve(strict=True)
    target = args.target_dir.resolve()
    guest = args.guest_artifacts.resolve()
    marsh = args.marsh.resolve()
    if marsh != target / "release/marsh":
        raise ValueError("--marsh must be --target-dir/release/marsh")
    if any(path.is_relative_to(source) or source.is_relative_to(path) for path in (target, guest)):
        raise ValueError("observed package outputs must be outside the guest-writable source tree")
    # No historical binary adoption or caller-owned incremental cache. BuildKit
    # remains a trusted content-addressed builder; Cargo gets an empty target.
    for path in (target, guest):
        if path.exists() and any(path.iterdir()):
            raise ValueError(f"build output must be empty (choose a fresh directory): {path}")
    registry = args.registry.resolve(strict=True)
    if args.shell_image_repository and not re.fullmatch(r"[a-z0-9][a-z0-9.-]*(?::[0-9]{1,5})?/[a-z0-9][a-z0-9._/-]*", args.shell_image_repository):
        raise ValueError("--shell-image-repository must name an explicit registry/repository without tag or digest")
    if not re.fullmatch(r"(?![a-z]+://)[a-z0-9][a-z0-9._:/-]*@sha256:[0-9a-f]{64}", args.rust_image):
        raise ValueError("--rust-image must be a digest-pinned Docker reference")
    if args.host_features and not re.fullmatch(r"[A-Za-z0-9_./:+?-]+(?:[ ,]+[A-Za-z0-9_./:+?-]+)*", args.host_features):
        raise ValueError("--host-features must be a Cargo feature list, not shell syntax")
    if args.shell_image_repository is None:
        from stock_sdk import validate_local_environment
        validate_local_environment()
    # Reject environment-selected compiler wrappers/configuration. Explicit flags
    # and the checkout's fingerprinted .cargo config are the build inputs.
    fixed_values = {"CARGO_INCREMENTAL": "0", "CARGO_PROFILE_DEV_DEBUG": "0",
                    "CARGO_PROFILE_RELEASE_DEBUG": "0", "CARGO_BUILD_JOBS": "1",
                    "RUSTUP_TOOLCHAIN": args.toolchain}
    selected_rustc = shutil.which(args.rustc)
    if selected_rustc:
        fixed_values["RUSTC"] = os.path.abspath(selected_rustc)
    forbidden = [name for name in os.environ if name.startswith(("CARGO_", "RUST"))
                 and name not in ("CARGO_HOME", "RUSTUP_HOME")
                 and not (name in fixed_values and os.environ[name] == fixed_values[name])]
    if forbidden:
        raise ValueError(f"unset implicit build overrides: {', '.join(sorted(forbidden))}")
    environment = {key: value for key, value in os.environ.items()
                   if key in ("PATH", "HOME", "TMPDIR", "CARGO_HOME", "RUSTUP_HOME",
                              "DOCKER_HOST", "DOCKER_CONTEXT", "DOCKER_CONFIG", "SSH_AUTH_SOCK",
                              "DOCKER_SANDBOXES_API", "DOCKER_SANDBOXES_APP_NAME", "SANDBOXES_STORAGE_ROOT", "XDG_STATE_HOME")}
    for key in ("CARGO_HOME", "RUSTUP_HOME"):
        if key in environment:
            path = Path(environment[key])
            environment[key] = str((source / path if not path.is_absolute() else path).resolve())
    environment.update({"RUSTUP_TOOLCHAIN": args.toolchain, "LC_ALL": "C",
                        "CARGO_INCREMENTAL": "0", "CARGO_PROFILE_DEV_DEBUG": "0",
                        "CARGO_PROFILE_RELEASE_DEBUG": "0", "CARGO_BUILD_JOBS": "1", "BUILDKIT_PROGRESS": "plain"})
    def observe_tool(argv):
        return PROCESS.run(argv, cwd=source, env=environment, text=True, stdout=subprocess.PIPE,
                           stderr=subprocess.STDOUT, timeout=30, check=True).stdout

    tools = {}
    selected_tools = [("cargo", args.cargo), ("rustc", args.rustc), ("make", args.make), ("docker", args.docker)]
    if args.shell_image_repository is None:
        selected_tools.append(("sbx", args.sbx))
    for name, requested in selected_tools:
        executable = shutil.which(requested)
        if executable is None:
            raise ValueError(f"missing {name}: {requested}")
        # Preserve rustup proxy basename; resolving its symlink would run rustup.
        executable = os.path.abspath(executable)
        version_args = ["-vV"] if name == "rustc" else ["version"] if name == "sbx" else ["--version"]
        version = observe_tool([executable, *version_args])
        tools[name] = {"path": executable, "sha256": file_digest(Path(executable)), "version": version}
    if (not re.search(r"^release: " + re.escape(args.toolchain) + r"$", tools["rustc"]["version"], re.M)
            or not tools["cargo"]["version"].startswith(f"cargo {args.toolchain} ")):
        raise ValueError(f"actual Cargo/rustc must both be {args.toolchain}; PATH/toolchain drift rejected")
    environment["RUSTC"] = tools["rustc"]["path"]
    if "sbx" in tools:
        environment["MARSH_SBX"] = tools["sbx"]["path"]
    # Toolchain sysroot compiler is a build input even when rustc is a rustup proxy.
    sysroot = Path(observe_tool([environment["RUSTC"], "--print", "sysroot"]).strip()).resolve(strict=True)
    tools["compiler"] = {"path": str(sysroot / "bin/rustc"),
                         "sha256": file_digest(sysroot / "bin/rustc")}
    tools["python"] = {"path": os.path.abspath(sys.executable),
                       "sha256": file_digest(Path(sys.executable)), "version": sys.version}
    tool_hashes = {value["path"]: value["sha256"] for value in tools.values()}
    tools["buildx"] = {"version": observe_tool([tools["docker"]["path"], "buildx", "version"])}
    cargo_home = Path(environment.get("CARGO_HOME", Path.home() / ".cargo"))
    extra_paths = [cargo_home / "config", cargo_home / "config.toml", sysroot / "bin/rustc", sysroot / "bin/cargo"]
    extra_paths += [parent / ".cargo" / name for parent in source.parents
                    for name in ("config", "config.toml")]
    # Bind the actual reported sysroot, not a guessed installation or just its
    # version banner. Membership as well as bytes is fenced before/after build.
    def observed_sysroot_files():
        return sorted(path for path in (sysroot / "lib").rglob("*") if path.is_file())
    sysroot_files = observed_sysroot_files()
    extra_paths += sysroot_files
    external_inputs = {str(path): file_digest(path) if path.is_file() else None for path in extra_paths}
    before, source_files_before = capture_source(source, args.source_revision)
    registry_hash = file_digest(registry)
    command = [tools["make"]["path"], "build", f"CARGO={tools['cargo']['path']}",
               f"PYTHON={tools['python']['path']}",
               f"DOCKER={tools['docker']['path']}", f"TARGET_DIR={target}",
               f"GUEST_ARTIFACTS={guest}", f"KIT_COMMANDS={registry}",
               f"HOST_FEATURES={args.host_features}", f"RUST_IMAGE={args.rust_image}",
               f"SHELL_IMAGE_REPOSITORY={args.shell_image_repository or ''}",
               f"SHELL_IMAGE_INSECURE_REGISTRY={int(args.shell_image_insecure)}",
               f"BUILD_CACHE_ID=marsh-observed-{uuid.uuid4().hex}"]
    started = time.time_ns()
    result = PROCESS.run(command, cwd=source, env=environment, check=False, timeout=7200)
    finished = time.time_ns()
    after, source_files_after = capture_source(source, args.source_revision)
    if result.returncode != 0:
        raise ValueError(f"build failed with exit {result.returncode}; no receipt issued")
    if (before != after or source_files_before != source_files_after or file_digest(registry) != registry_hash or
            tool_hashes != {path: file_digest(Path(path)) for path in tool_hashes} or
            sysroot_files != observed_sysroot_files() or
            external_inputs != {str(path): file_digest(path) if path.is_file() else None for path in extra_paths}):
        raise ValueError("source/registry/compiler/tool inputs changed during build; no receipt issued")
    if file_digest(guest / "commands.json") != registry_hash:
        raise ValueError("packaged registry differs from selected build input")
    image_proof = verify_shell_image_proof(source, guest, args.shell_image_repository, args.shell_image_insecure)
    value = {"schema": SCHEMA, "outcome": "passed", "source_tree": str(source),
             "source_before": before, "source_after": after, "source_files": source_files_after,
             "marsh": str(marsh), "guest_artifacts": str(guest),
             "artifacts": artifact_manifest(marsh, guest), "shell_image_proof": image_proof,
             "build": {"argv": command, "cwd": str(source), "environment": environment,
                       "tools": tools, "external_inputs": external_inputs,
                       "rust_image": args.rust_image,
                       "shell_image": (guest / "shell-image").read_text().strip(),
                       "shell_image_repository": args.shell_image_repository,
                       "shell_image_insecure": args.shell_image_insecure,
                       "publication_effects": image_proof["publication_effects"],
                       "registry": str(registry), "registry_sha256": registry_hash,
                       "started_unix_ns": started, "finished_unix_ns": finished,
                       "exit_status": result.returncode}}
    # Exclusive publication. Failure leaves no stale success receipt.
    descriptor = os.open(receipt, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(descriptor, "w") as stream:
            json.dump(value, stream, indent=2)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
    except BaseException:
        receipt.unlink(missing_ok=True)
        raise
    print(f"Observed build receipt: {receipt}")
    if image_proof["publication"] == "local-template":
        from local_shell_authority import issue, authority_path
        authority = authority_path(receipt)
        issue(build_receipt=receipt, source_tree=source, revision=args.source_revision,
              marsh=marsh, guest_artifacts=guest, image_file=guest / "shell-image", output=authority)
        print(f"Host LOCAL authority: {authority}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print(f"build-candidate: {error}", file=sys.stderr)
        raise SystemExit(1)
