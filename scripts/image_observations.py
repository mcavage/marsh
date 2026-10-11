"""Shared, bounded observation of locally produced Docker/OCI archives.

An archive/import receipt is not VM attestation. Local references retain the
exact repository AND full platform manifest digest; no arbitrary tags enter the
packaged runtime contract. Registry publication is handled only by explicit opt-in.
"""
from __future__ import annotations
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import tarfile
import owned_process as PROCESS
from stock_sdk import inspect_image

DIGEST = re.compile(r"sha256:[0-9a-f]{64}\Z")
PLATFORM = "linux/arm64"
LOCAL_REPOSITORIES = {"shell": "docker.io/library/marsh-shell-local"}
MAX_ARCHIVE_BYTES = 16 * 1024**3
MAX_METADATA_BYTES = 4 * 1024**2
MAX_MEMBERS = 20000


def file_hash(path):
    value = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024**2), b""):
            value.update(chunk)
    return "sha256:" + value.hexdigest()


def archive_identity(path: Path) -> dict:
    if path.is_symlink() or not path.is_file() or path.stat().st_size > MAX_ARCHIVE_BYTES:
        raise ValueError("expected bounded regular local image archive")
    with tarfile.open(path, "r:*") as archive:
        members = {}
        size = 0
        for member in archive:
            name = member.name
            if (name in members or name.startswith("/") or ".." in PurePosixPath(name).parts
                    or not (member.isfile() or member.isdir())):
                raise ValueError("image archive has duplicate/unsafe/special members")
            members[name] = member
            size += member.size
            if len(members) > MAX_MEMBERS or size > MAX_ARCHIVE_BYTES:
                raise ValueError("image archive exceeds observation bounds")

        def metadata(name):
            member = members.get(name)
            if member is None or not member.isfile() or member.size > MAX_METADATA_BYTES:
                raise ValueError(f"missing/bounded image metadata required: {name}")
            with archive.extractfile(member) as stream:
                return stream.read()

        def blob(descriptor):
            digest = descriptor.get("digest", "")
            if not isinstance(digest, str) or not DIGEST.fullmatch(digest):
                raise ValueError("image descriptor has invalid digest")
            name = "blobs/sha256/" + digest[7:]
            member = members.get(name)
            if member is None or not member.isfile() or member.size != descriptor.get("size"):
                raise ValueError("image descriptor size/member mismatch")
            value = hashlib.sha256()
            with archive.extractfile(member) as stream:
                for chunk in iter(lambda: stream.read(1024**2), b""):
                    value.update(chunk)
            if "sha256:" + value.hexdigest() != digest:
                raise ValueError("image blob digest does not match bytes")
            return name

        index = json.loads(metadata("index.json"))
        descriptors = index.get("manifests", [])
        if len(descriptors) != 1:
            raise ValueError("local image must contain exactly one platform manifest")
        manifest_name = blob(descriptors[0])
        manifest = json.loads(metadata(manifest_name))
        config_name = blob(manifest["config"])
        config = json.loads(metadata(config_name))
        if (config.get("os"), config.get("architecture")) != ("linux", "arm64"):
            raise ValueError("local image is not the selected Linux ARM64 platform")
        layers = manifest.get("layers")
        if not isinstance(layers, list) or len(layers) > 256:
            raise ValueError("invalid local image layer manifest")
        layer_names = [blob(layer) for layer in layers]
        docker = json.loads(metadata("manifest.json"))
        if (len(docker) != 1 or docker[0].get("Config") != config_name
                or docker[0].get("Layers") != layer_names):
            raise ValueError("Docker and OCI archive manifests disagree")
        if len(config.get("rootfs", {}).get("diff_ids", [])) != len(layers):
            raise ValueError("image config and manifest layer counts disagree")
        return {"platform": PLATFORM, "config_digest": manifest["config"]["digest"],
                "platform_manifest_digest": descriptors[0]["digest"],
                "layer_digests": [layer["digest"] for layer in layers],
                "archive_sha256": file_hash(path), "archive_bytes": path.stat().st_size,
                "repository_tags": docker[0].get("RepoTags", [])}


def canonical_local_tags(tags, kind: str, config_digest: str) -> list[str]:
    """Normalize Docker Hub spelling ONLY for the one produced config tag.

    This is not general image parsing or mutable-reference admission. The archive
    bytes/hash and config/platform/layer identities are never rewritten.
    """
    repository = LOCAL_REPOSITORIES[kind]
    if not DIGEST.fullmatch(config_digest):
        raise ValueError("invalid produced config digest")
    leaf = repository.removeprefix("docker.io/library/")
    tag = ":sha256-" + config_digest[7:]
    equivalent = {leaf + tag, "library/" + leaf + tag, "docker.io/" + leaf + tag,
                  repository + tag, "index.docker.io/library/" + leaf + tag}
    if not isinstance(tags, list) or len(tags) != 1 or not isinstance(tags[0], str) or tags[0] not in equivalent:
        raise ValueError("local archive does not name the exact import repository/config-derived tag")
    return [repository + tag]


def _brief(value: str) -> str:
    return value.encode("utf-8", errors="replace")[:2048].decode("utf-8", errors="replace")


def _archive_summary(value: dict) -> dict:
    result = {key: value[key] for key in ("platform", "config_digest", "platform_manifest_digest",
              "layer_digests", "archive_sha256", "archive_bytes")}
    tags = value.get("repository_tags")
    result["repository_tags"] = [_brief(tag) if isinstance(tag, str) else "<non-string>" for tag in tags[:4]] if isinstance(tags, list) else "<non-array>"
    result["repository_tag_count"] = len(tags) if isinstance(tags, list) else None
    return result


class LocalImageImportError(ValueError):
    def __init__(self, error: Exception, observations: dict):
        super().__init__(_brief(str(error)))
        self.observations = observations


def import_local_image(docker, sbx, temporary_tag, directory: Path, kind: str) -> tuple[str, dict]:
    records = []
    trail = {"phase": "save-built-image", "temporary_tag": temporary_tag,
             "sdk_import_attempted": False, "archive_retained": False, "commands": records}
    def run(argv):
        record = {"argv": argv, "status": None}
        records.append(record)
        result = PROCESS.run(argv, capture_output=True, text=True, timeout=600)
        record.update(status=result.returncode, stderr=_brief(result.stderr),
                      stdout_sha256="sha256:" + hashlib.sha256(result.stdout.encode()).hexdigest())
        if argv[1:] == ["template", "ls", "--json"]:
            record.update(stdout="", stdout_selection="unrelated catalog entries omitted")
        else:
            record.update(stdout=_brief(result.stdout), stdout_bytes=len(result.stdout.encode()),
                          stderr_bytes=len(result.stderr.encode()), stdout_truncated=len(result.stdout.encode()) > 2048,
                          stderr_truncated=len(result.stderr.encode()) > 2048)
        if result.returncode:
            raise ValueError(f"local image command failed: {argv[0:2]}: {_brief(result.stderr.strip())}")
        return result
    try:
        archive = directory / "image.tar"
        run([docker, "save", "--platform", PLATFORM, "--output", str(archive), temporary_tag])
        trail["phase"] = "verify-built-archive"
        built = archive_identity(archive)
        trail["built_archive"] = _archive_summary(built)
        repository = LOCAL_REPOSITORIES[kind]
        tag = "sha256-" + built["config_digest"][7:]
        named = f"{repository}:{tag}"
        trail.update(phase="name-import-image", runtime_reference=named)
        run([docker, "tag", temporary_tag, named])
        run([docker, "save", "--platform", PLATFORM, "--output", str(archive), named])
        trail["phase"] = "verify-import-archive"
        imported = archive_identity(archive)
        trail["imported_archive"] = _archive_summary(imported)
        for key in ("config_digest", "platform_manifest_digest", "layer_digests", "platform"):
            if imported[key] != built[key]:
                raise ValueError("local image identity changed while assigning import repository")
        trail["phase"] = "verify-import-name"
        raw_tags = imported["repository_tags"]
        imported["repository_tags"] = canonical_local_tags(raw_tags, kind, built["config_digest"])
        imported["repository_tags_observed"] = raw_tags
        trail.update(phase="sdk-import", sdk_import_attempted=True)
        run([sbx, "template", "load", str(archive)])
        trail["phase"] = "sdk-template-list"
        inventory = json.loads(run([sbx, "template", "ls", "--json"]).stdout)
        # Stock SBX has emitted both {"images": [...]} and a bare [...].
        if isinstance(inventory, dict):
            inventory = inventory.get("images", [])
        if not isinstance(inventory, list):
            raise ValueError("stock template listing is not a JSON array or images object")
        rows = [row for row in inventory if isinstance(row, dict)
                and row.get("repository") == repository and row.get("tag") == tag]
        records[-1]["stdout"] = json.dumps({"images": [{key: row.get(key) for key in ("repository", "tag", "id")} for row in rows[:4]]})
        if len(rows) != 1 or rows[0].get("id") != imported["platform_manifest_digest"][7:19]:
            raise ValueError("stock template listing differs from the produced/imported image")
        trail["phase"] = "sdk-full-image-identity"
        observation = inspect_image(named)
        trail["sdk_observation"] = observation
        if observation["image_id"] != imported["platform_manifest_digest"]:
            raise ValueError("FULL SDK image ID differs from the produced platform manifest")
        # Stock SBX resolves locally loaded templates by tag only; the digest
        # remains the identity the product verifies after creation.
        reference = named + "@" + imported["platform_manifest_digest"]
        trail["phase"] = "remove-owned-build-tag"
        # Only the unique helper-created tag, never an image by ID/user name.
        run([docker, "image", "rm", temporary_tag])
        return reference, {"schema": "marsh.local-image-import/v1", "reference": reference,
                           "runtime_reference": named, "archive": imported, "template": rows[0], "commands": records,
                           "sdk_observation": observation,
                           "runtime_identity": "not observed; exact VM check is a separate live gate"}
    except Exception as error:
        # Retain identities/commands rather than copying a potentially16GiB archive
        # or exposing config/env/notice payloads. Helpers persist this trail BEFORE
        # deleting their owned temporary directory; no success receipt is issued.
        raise LocalImageImportError(error, trail) from error


def verified_shell_image(image_file: Path, *, source_tree: Path,
                         build_receipt: Path | None, require_publication: bool = True) -> tuple[str, list[Path]]:
    """Consume a HOST observed-build receipt, not an image helper's self-report.

    Revalidate immediately before effects and after publication. The image-only
    producer JSON is consistency evidence; it cannot bless a historical image.
    Standalone repaired shell images have no candidate/compiler payload: that
    relation lives in the full build receipt, which binds host/Linux executables,
    licenses and this proof. This is a trusted-host observation, not a signature.
    """
    if build_receipt is None:
        raise ValueError("--shell-build-receipt is required with --shell-image; helper JSON alone is not build provenance")
    import sys
    sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tests/acceptance"))
    from provenance import host_only_path, verify_build_receipt
    from build_inputs import read_file
    source = source_tree.resolve(strict=True)
    receipt_path = host_only_path(build_receipt, source)
    value = json.loads(read_file(receipt_path, 16 * 1024 * 1024))
    if (not isinstance(value, dict) or not isinstance(value.get("guest_artifacts"), str)
            or not isinstance(value.get("marsh"), str) or not isinstance(value.get("source_after"), dict)
            or not isinstance(value["source_after"].get("source_revision"), str)):
        raise ValueError("observed candidate receipt lacks source/artifact identities")
    guest = Path(value["guest_artifacts"])
    if image_file.is_symlink() or image_file.resolve(strict=True) != guest / "shell-image":
        raise ValueError("prepared shell image must be the exact observed package path, not a copied/relabeled reference")
    verified = verify_build_receipt(receipt_path, source_tree=source,
                                   revision=value["source_after"]["source_revision"],
                                   marsh=Path(value["marsh"]), guest_artifacts=guest)
    proof = verified["shell_image_proof"]
    if require_publication and proof["publication"] != "explicit-registry":
        raise ValueError("a distributable Kit FROM needs an explicitly published shell; local template proof is not a registry source")
    return proof["reference"], [image_file, image_file.with_name("shell-image.build.json"), receipt_path]


def verify_local_image_proof(proof: dict, reference: str, kind: str) -> None:
    image = proof.get("local_image", {})
    archive = image.get("archive", {})
    repository = LOCAL_REPOSITORIES[kind]
    digest, config = archive.get("platform_manifest_digest", ""), archive.get("config_digest", "")
    if (image.get("schema") != "marsh.local-image-import/v1" or not DIGEST.fullmatch(digest)
            or not DIGEST.fullmatch(config) or archive.get("platform") != PLATFORM
            or reference != repository + "@" + digest or image.get("reference") != reference
            or not DIGEST.fullmatch(archive.get("archive_sha256", ""))):
        raise ValueError("invalid produced local image identity/proof")
    if (type(archive.get("archive_bytes")) is not int or not 0 < archive["archive_bytes"] <= MAX_ARCHIVE_BYTES
            or not isinstance(archive.get("layer_digests"), list) or len(archive["layer_digests"]) > 256
            or any(not isinstance(item, str) or not DIGEST.fullmatch(item) for item in archive["layer_digests"])):
        raise ValueError("invalid produced archive size/layer identities")
    # The canonical field is the existing native schema; retain and validate the
    # original Docker spelling separately, without relabeling archive bytes.
    if "repository_tags_observed" in archive and canonical_local_tags(archive["repository_tags_observed"], kind, config) != archive.get("repository_tags"):
        raise ValueError("observed Docker archive spelling differs from its canonical produced tag")
    expected_tag = "sha256-" + config[7:]
    observation = image.get("sdk_observation", {})
    if (observation.get("image_id") != digest or observation.get("reference") != image.get("runtime_reference")
            or not isinstance(observation.get("endpoint"), str) or not observation["endpoint"].startswith("unix:///")
            or type(observation.get("observed_unix_ns")) is not int or observation["observed_unix_ns"] <= 0):
        raise ValueError("local producer lacks a full SDK image identity observation")
    template = image.get("template", {})
    if (template.get("repository") != repository or template.get("tag") != expected_tag
            or template.get("id") != digest[7:19]
            or image.get("runtime_reference") != repository + ":" + expected_tag
            or archive.get("repository_tags") != [repository + ":" + expected_tag]):
        raise ValueError("local template identity/proof mismatch")
    commands = image.get("commands", [])
    if (not commands or any(row.get("status") != 0 for row in commands)
            or not any(row.get("argv", [])[1:3] == ["template", "load"] for row in commands)
            or not any(row.get("argv", [])[1:] == ["template", "ls", "--json"] for row in commands)):
        raise ValueError("local image lacks successful import/inventory observations")
