"""Host-issued LOCAL shell capability for the candidate shell image.

Issuance revalidates current source/build/tool/package inputs. Consumption binds
captured evidence and the built package, never the mutable development worktree.
This is same-host authority, not signing or hostile-host attestation.
"""
from __future__ import annotations
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import stat
import sys
import time

import build_inputs as INPUTS
from image_observations import verify_local_image_proof
from package_observations import artifact_manifest, file_digest, relocated_manifest
from stock_sdk import endpoint, inspect_image

ROOT = Path(__file__).resolve().parents[1]
SCHEMA = "marsh.local-shell-authority/v1"
MAX_DOCUMENT = 16 * 1024 * 1024
FIELDS = {"schema", "kind", "reference", "runtime_reference", "platform", "config_digest",
          "platform_manifest_digest", "layer_digests", "archive_sha256", "archive_bytes", "source_identity",
          "build_receipt", "image_proof", "image_reference", "package_marsh", "package_guest_artifacts",
          "sdk_endpoint", "sdk_image_id", "issued_unix_ns"}


def authority_path(build_receipt: Path, name: str | None = None) -> Path:
    """A private sibling metadata directory, never a package ancestor/root."""
    return build_receipt.parent / "authorities" / (name or build_receipt.name + ".local-shell-authority.json")


def _provenance():
    sys.path.insert(0, str(ROOT / "tests/acceptance"))
    import provenance
    return provenance


def _outside(path: Path, excluded) -> None:
    if not path.is_absolute() or path != path.resolve():
        raise ValueError("authority paths must be absolute, canonical and no-follow")
    if any(path.is_relative_to(root.resolve()) for root in excluded):
        raise ValueError("authority must be outside source, package and guest exports")


def _private_directory(path: Path, *, create=False):
    with INPUTS.directory(path, create=create) as fd:
        info = os.fstat(fd)
        if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
            raise ValueError("authority directory must be caller-owned mode0700")


def _read_private(path: Path) -> bytes:
    _outside(path, [])
    _private_directory(path.parent)
    with INPUTS.regular_file(path, MAX_DOCUMENT) as (stream, metadata):
        if stat.S_IMODE(metadata.st_mode) != 0o600:
            raise ValueError("authority/snapshot must have mode0600")
        return stream.read(MAX_DOCUMENT + 1)


def _json(raw: bytes):
    def unique(rows):
        result = {}
        for key, value in rows:
            if key in result:
                raise ValueError("duplicate authority/evidence JSON key")
            result[key] = value
        return result
    return json.loads(raw, object_pairs_hook=unique)


def _snapshot(parent: Path, name: str, data: bytes) -> dict:
    digest = "sha256:" + hashlib.sha256(data).hexdigest()
    _private_directory(parent / "observations", create=True)
    directory = parent / "observations" / digest[7:]
    _private_directory(directory, create=True)
    destination = directory / name
    lock = os.open(directory / ".lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        info = os.fstat(lock)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_nlink != 1 or stat.S_IMODE(info.st_mode) != 0o600:
            raise ValueError("unsafe observation lock")
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if destination.exists() or destination.is_symlink():
            if _read_private(destination) != data:
                raise ValueError("content-addressed observation collision")
        else:
            temporary = directory / (".pending-" + os.urandom(16).hex())
            try:
                with os.fdopen(os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600), "wb") as stream:
                    stream.write(data); stream.flush(); os.fsync(stream.fileno())
                temporary.rename(destination)
            finally:
                temporary.unlink(missing_ok=True)
            if _read_private(destination) != data:
                raise ValueError("observation snapshot differs after publication")
    finally:
        os.close(lock)
    return {"path": str(destination), "sha256": digest}


def _verify_build_inputs(value):
    build = value["build"]
    if file_digest(Path(build["registry"])) != build["registry_sha256"]:
        raise ValueError("selected build registry changed before authority issuance")
    for record in build["tools"].values():
        if "path" in record and file_digest(Path(record["path"])) != record.get("sha256"):
            raise ValueError("build tool/compiler changed before authority issuance")
    for name, expected in build["external_inputs"].items():
        path = Path(name)
        actual = file_digest(path) if path.is_file() else None
        if actual != expected:
            raise ValueError("compiler/configuration input changed before authority issuance")


def _captured_image_inputs(receipt, proof, kind):
    """Cross-bind producer inputs to captured records, without live source IO."""
    records = receipt.get("source_files")
    inputs = proof.get("inputs")
    if not isinstance(records, dict) or not isinstance(inputs, dict):
        raise ValueError("captured producer/source records missing")
    def check(path, digest, mode=None):
        record = records.get(path, {})
        if record.get("kind") != "file" or record.get("sha256") != digest or (mode is not None and record.get("mode") != mode):
            raise ValueError("producer source input differs from captured build source")
    def tree(prefix, rows):
        if not isinstance(rows, dict):
            raise ValueError("producer source tree inventory missing")
        for name, row in rows.items():
            INPUTS.parts(name)
            check(prefix + "/" + name, row["sha256"], row["mode"])
    support = inputs.get("support_scripts")
    if not isinstance(support, dict) or set(support) != {"build_inputs.py", "image_observations.py", "owned_process.py", "stock_sdk.py"}:
        raise ValueError("producer support script bindings missing")
    for name, digest in support.items():
        check("scripts/" + name, digest)
    if kind != "shell":
        raise ValueError("LOCAL authority covers only the shell image")
    for name in ("packaging/shell-image", "packaging/shell/Dockerfile", "scripts/prepare-shell-image.py"):
        check(name, inputs.get(name))
    for prefix in ("packaging/image-repair", "packaging/dhi-notices/collected", "packaging/shell/pi"):
        tree(prefix, inputs.get(prefix))

def _package(value, marsh: Path, guest: Path) -> dict:
    if marsh.name != "marsh" or not marsh.is_absolute() or not guest.is_absolute():
        raise ValueError("authority requires canonical package paths")
    if marsh.resolve() != marsh or guest.resolve() != guest:
        raise ValueError("authority package cannot traverse a symlink")
    expected = relocated_manifest(value, marsh, guest)
    if artifact_manifest(marsh, guest) != expected:
        raise ValueError("authority package differs from the exact observed build/relocation")
    return expected


def issue(*, build_receipt: Path, source_tree: Path, revision: str,
          marsh: Path, guest_artifacts: Path, image_file: Path, output: Path,
          package_marsh: Path | None = None, package_guest_artifacts: Path | None = None,
          excluded_roots=()) -> dict:
    """Issue only after full observation; no VM/create/provider command is run."""
    source = source_tree.resolve(strict=True)
    binary = package_marsh or marsh
    guest = package_guest_artifacts or guest_artifacts
    excluded = [source, marsh.parent, guest_artifacts, binary.parent, guest, *excluded_roots]
    _outside(output, excluded)
    _private_directory(output.parent, create=True)
    if any(root.resolve().is_relative_to(output.parent) for root in excluded):
        raise ValueError("authority directory must not contain source, package or guest exports")
    if output.exists() or output.is_symlink():
        raise ValueError("authority output already exists; never overwrite/adopt a capability")
    provenance = _provenance()
    before = provenance.verify_build_receipt(build_receipt, source_tree=source, revision=revision,
                                            marsh=marsh, guest_artifacts=guest_artifacts)
    receipt_bytes = _read_private(build_receipt)
    if _json(receipt_bytes) != before:
        raise ValueError("observed build receipt changed during admission")
    _verify_build_inputs(before)
    manifest = _package(before, binary, guest)
    proof_file = image_file.with_name(image_file.name + ".build.json")
    if image_file != guest_artifacts / "shell-image":
        raise ValueError("LOCAL authority covers only the candidate's shell image")
    kind = "shell"
    proof = before["shell_image_proof"]
    reference_bytes = INPUTS.read_file(image_file)
    proof_bytes = INPUTS.read_file(proof_file, MAX_DOCUMENT)
    if _json(proof_bytes) != proof or reference_bytes.decode().strip() != proof["reference"]:
        raise ValueError("selected image proof/reference changed during admission")
    if proof.get("publication") != "local-template" or proof.get("publication_effects") != []:
        raise ValueError("LOCAL authority requires an actual local producer, not published or historical helper JSON")
    verify_local_image_proof(proof, proof["reference"], kind)
    _captured_image_inputs(before, proof, kind)
    local = proof["local_image"]
    archive = local["archive"]
    selected_endpoint = endpoint()
    if local["sdk_observation"]["endpoint"] != selected_endpoint:
        raise ValueError("LOCAL producer and authority select different SDK endpoints")
    # All stale/forged-path/source/tool/package/proof rejection above is before
    # this ONE read-only SDK request. It is not template-ls prefix authority.
    observed = inspect_image(local["runtime_reference"])
    if observed["image_id"] != archive["platform_manifest_digest"]:
        raise ValueError("FULL SDK image identity changed before authority issuance")
    after = provenance.verify_build_receipt(build_receipt, source_tree=source, revision=revision,
                                           marsh=marsh, guest_artifacts=guest_artifacts)
    _verify_build_inputs(after)
    if (after != before or artifact_manifest(binary, guest) != manifest
            or INPUTS.read_file(image_file) != reference_bytes
            or INPUTS.read_file(proof_file, MAX_DOCUMENT) != proof_bytes
            or _read_private(build_receipt) != receipt_bytes):
        raise ValueError("candidate/image inputs changed during authority issuance")
    value = {"schema": SCHEMA, "kind": kind, "reference": proof["reference"],
             "runtime_reference": local["runtime_reference"], "platform": archive["platform"],
             **{key: archive[key] for key in ("config_digest", "platform_manifest_digest", "layer_digests", "archive_sha256", "archive_bytes")},
             "source_identity": before["source_after"],
             "build_receipt": _snapshot(output.parent, "build.json", receipt_bytes),
             "image_proof": _snapshot(output.parent, "image-proof.json", proof_bytes),
             "image_reference": _snapshot(output.parent, "image-reference", reference_bytes),
             "package_marsh": str(binary), "package_guest_artifacts": str(guest),
             "sdk_endpoint": observed["endpoint"], "sdk_image_id": observed["image_id"],
             "issued_unix_ns": time.time_ns()}
    # Fresh exclusive publication; snapshots remain reusable if publication races.
    data = (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()
    fd = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "wb") as stream:
        stream.write(data); stream.flush(); os.fsync(stream.fileno())
    return value


def verify_captured(path: Path, *, expected_reference: str, excluded_roots=()) -> dict:
    """Python caller oracle for the same native contract; NO current-source reads.

    Native runtime must independently enforce complete stock-export exclusion and
    perform live SDK/VM identity checks. This helper does not implement runtime.
    """
    _outside(path, excluded_roots)
    value = _json(_read_private(path))
    if not isinstance(value, dict) or set(value) != FIELDS or value["schema"] != SCHEMA:
        raise ValueError("invalid LOCAL authority schema/fields")
    if value["reference"] != expected_reference or value["kind"] != "shell":
        raise ValueError("LOCAL authority does not match the selected immutable image")
    _private_directory(path.parent / "observations")
    snapshots = {}
    for name in ("build_receipt", "image_proof", "image_reference"):
        record = value[name]
        if not isinstance(record, dict) or set(record) != {"path", "sha256"}:
            raise ValueError("invalid LOCAL snapshot binding")
        snapshot = Path(record["path"])
        _outside(snapshot, excluded_roots)
        if not snapshot.is_relative_to(path.parent / "observations"):
            raise ValueError("LOCAL snapshot is outside its private observation namespace")
        data = _read_private(snapshot)
        if "sha256:" + hashlib.sha256(data).hexdigest() != record["sha256"]:
            raise ValueError("LOCAL authority snapshot bytes changed")
        snapshots[name] = data
    receipt = _json(snapshots["build_receipt"])
    proof = _json(snapshots["image_proof"])
    if (receipt.get("schema") != "marsh.observed-build/v1" or receipt.get("outcome") != "passed"
            or receipt.get("source_before") != value["source_identity"] or receipt.get("source_after") != value["source_identity"]
            or not receipt.get("source_files") or receipt.get("build", {}).get("exit_status") != 0):
        raise ValueError("LOCAL authority does not bind the captured observed source/build")
    wanted = "marsh.prepared-shell-image/v1"
    if (proof.get("schema") != wanted or proof.get("publication") != "local-template"
            or proof.get("publication_effects") != [] or proof.get("reference") != expected_reference
            or snapshots["image_reference"].decode().strip() != expected_reference):
        raise ValueError("LOCAL authority image reference/proof mismatch")
    verify_local_image_proof(proof, expected_reference, value["kind"])
    _captured_image_inputs(receipt, proof, value["kind"])
    if value["kind"] == "shell" and proof != receipt.get("shell_image_proof"):
        raise ValueError("LOCAL shell proof differs from the observed package")
    local = proof["local_image"]
    for key in ("platform", "config_digest", "platform_manifest_digest", "layer_digests", "archive_sha256", "archive_bytes"):
        if value[key] != local["archive"][key]:
            raise ValueError("LOCAL authority archive/config/platform binding mismatch")
    if (value["runtime_reference"] != local["runtime_reference"]
            or value["sdk_endpoint"] != local["sdk_observation"]["endpoint"]
            or value["sdk_image_id"] != value["platform_manifest_digest"]
            or type(value["issued_unix_ns"]) is not int or value["issued_unix_ns"] <= 0):
        raise ValueError("LOCAL authority SDK/alias binding mismatch")
    binary, guest = Path(value["package_marsh"]), Path(value["package_guest_artifacts"])
    excluded = [Path(receipt["source_tree"]), binary.parent, guest, *excluded_roots]
    if any(root.resolve().is_relative_to(path.parent) for root in excluded):
        raise ValueError("authority directory must not contain source, package or guest exports")
    for protected in (path, *(Path(value[key]["path"]) for key in snapshots)):
        _outside(protected, excluded)
    _package(receipt, binary, guest)
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("build-receipt", "source-tree", "marsh", "guest-artifacts", "image", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--package-marsh", type=Path)
    parser.add_argument("--package-guest-artifacts", type=Path)
    parser.add_argument("--excluded-root", type=Path, action="append", default=[])
    args = parser.parse_args()
    if bool(args.package_marsh) != bool(args.package_guest_artifacts):
        parser.error("relocation requires both package paths")
    issue(build_receipt=args.build_receipt, source_tree=args.source_tree, revision=args.source_revision,
          marsh=args.marsh, guest_artifacts=args.guest_artifacts, image_file=args.image, output=args.output,
          package_marsh=args.package_marsh, package_guest_artifacts=args.package_guest_artifacts,
          excluded_roots=args.excluded_root)
    print(f"Host LOCAL authority: {args.output}")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, KeyError, TypeError) as error:
        raise SystemExit(f"local shell authority: {error}") from error
