"""Package identity and private host-path checks shared by tools and acceptance.

These checks do not attest a build. The host-owned build receipt and its creation
remain separate from inspecting the files a retained drive is about to use.
"""
from __future__ import annotations

import hashlib
import os
from pathlib import Path
import re
import stat

HOST_NAMES = ("marsh", "marshd", "marsh-mcp", "marsh-local")
LICENSE_NAMES = ("LICENSE", "THIRD-PARTY-NOTICES.txt", "rust-package-notices.json",
                 "embedded-native-notices.json")
GUEST_NAMES = ("marsh-linux-arm64", "marsh-worker-linux-arm64",
               "marsh-relay-linux-arm64", "marshd-linux-arm64", "marsh-local-linux-arm64", "marsh-byte-exec-linux-arm64",
               "commands.json", "agents.json", "shell-image", "shell-image.build.json")


def file_digest(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return "sha256:" + digest.hexdigest()


def host_only_path(path: Path, *mounts: Path) -> Path:
    """Validate an absolute, nonsymlink path in a private host-owned directory."""
    if not path.is_absolute():
        raise ValueError(f"host evidence path must be absolute: {path}")
    absolute = Path(os.path.abspath(path))
    resolved = path.resolve()
    if absolute != resolved:
        raise ValueError(f"host evidence path traverses a symlink: {path}")
    if any(resolved.is_relative_to(mount.resolve()) for mount in mounts):
        raise ValueError(f"host evidence must be outside mounted project/home: {path}")
    parent = resolved.parent
    parent.mkdir(parents=True, mode=0o700, exist_ok=True)
    metadata = parent.stat()
    if metadata.st_uid != os.getuid() or stat.S_IMODE(metadata.st_mode) & 0o077:
        raise ValueError(f"host evidence directory must be owner-only: {parent}")
    if resolved.exists():
        metadata = resolved.lstat()
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid() or metadata.st_mode & 0o077:
            raise ValueError(f"host evidence file must be owner-only and regular: {resolved}")
    return resolved


def artifact_manifest(marsh: Path, guest_artifacts: Path) -> dict:
    marsh = marsh.resolve(strict=True)
    guest_artifacts = guest_artifacts.resolve(strict=True)
    paths = [marsh.with_name(name) for name in HOST_NAMES]
    paths += [guest_artifacts / name for name in GUEST_NAMES]
    paths += [guest_artifacts / "licenses/marsh" / name for name in LICENSE_NAMES]
    kits = guest_artifacts / "kits"
    if not kits.is_dir():
        raise ValueError(f"packaged Kits missing: {kits}")
    # Include every exported guest artifact, including new executables such as
    # marsh-local-linux-arm64. A fixed three-file list silently misses additions.
    for path in guest_artifacts.rglob("*"):
        mode = path.lstat().st_mode
        if stat.S_ISLNK(mode) or not (stat.S_ISREG(mode) or stat.S_ISDIR(mode)):
            raise ValueError(f"packaged artifact path must be a regular file/directory: {path}")
        if path.is_file() and path not in paths:
            paths.append(path)
    manifest = {str(guest_artifacts): {"type": "directory", "mode": stat.S_IMODE(guest_artifacts.stat().st_mode)}}
    for directory in guest_artifacts.rglob("*"):
        if directory.is_dir():
            manifest[str(directory)] = {"type": "directory", "mode": stat.S_IMODE(directory.stat().st_mode)}
    for path in paths:
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"artifact must be a regular, nonsymlink file: {path}")
        if path.name in HOST_NAMES + GUEST_NAMES[:6] and not os.access(path, os.X_OK):
            raise ValueError(f"artifact is not executable: {path}")
        manifest[str(path)] = {"sha256": file_digest(path), "mode": stat.S_IMODE(path.stat().st_mode)}
    shell_image = (guest_artifacts / "shell-image").read_text().strip()
    if not re.fullmatch(r"[^\s]+@sha256:[0-9a-f]{64}", shell_image):
        raise ValueError("packaged shell image must be an immutable OCI digest")
    return manifest


def relocated_manifest(receipt: dict, marsh: Path, guest: Path) -> dict:
    """Rebind a verified input manifest to destinations; never rewrite its receipt."""
    original_host = Path(receipt["marsh"]).parent
    original_guest = Path(receipt["guest_artifacts"])
    manifest = {}
    for name, identity in receipt["artifacts"].items():
        path = Path(name)
        if path.is_relative_to(original_guest):
            destination = guest / path.relative_to(original_guest)
        elif path.parent == original_host and path.name in HOST_NAMES:
            destination = marsh.with_name(path.name)
        else:
            raise ValueError(f"unrecognized input artifact path: {path}")
        if str(destination) in manifest:
            raise ValueError("overlapping relocation destinations")
        manifest[str(destination)] = identity
    return manifest


def verify_relocated_package(proof: dict, marsh: Path, guest: Path) -> None:
    if (set(path.name for path in marsh.parent.iterdir()) != set(HOST_NAMES) or
            proof.get("schema") != "marsh.package-relocation/v1" or
            proof.get("marsh") != str(marsh) or proof.get("guest_artifacts") != str(guest) or
            proof.get("artifacts") != artifact_manifest(marsh, guest)):
        raise ValueError("relocated package drift or missing host copy proof; refusing execution")
