"""Host-owned build observations, not signatures or historical-binary attestations.

The trusted host runs build-candidate.py and keeps its receipt outside all guest
mounts. An agent that can execute arbitrary host code can forge this evidence;
checkout write access alone must not confer that authority.
"""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import threading
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))
from package_observations import (HOST_NAMES, LICENSE_NAMES, GUEST_NAMES,
                                  file_digest, host_only_path, artifact_manifest,
                                  relocated_manifest, verify_relocated_package)

from build_inputs import tree_inventory
from image_observations import verify_local_image_proof
from owned_process import run as run_owned

SCHEMA = "marsh.observed-build/v1"
RUST_IMAGE = "rust:1.95-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1"
def _source_snapshot(source_tree: Path, revision: str) -> tuple[dict, dict]:
    """One scanner for source identity and the private per-file edit fence."""
    source_tree = source_tree.resolve(strict=True)
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("--source-revision must be a complete lowercase Git revision")
    git_environment = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    git_environment["GIT_OPTIONAL_LOCKS"] = "0"
    def git(*args: str) -> bytes:
        return subprocess.check_output(["git", "-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", *args],
                                       cwd=source_tree, stderr=subprocess.PIPE, env=git_environment)
    if Path(os.fsdecode(git("rev-parse", "--show-toplevel").strip())).resolve() != source_tree:
        raise ValueError("--source-tree must name the Git worktree root")
    if git("rev-parse", "HEAD").decode().strip() != revision:
        raise ValueError("--source-revision does not match source-tree HEAD")
    paths = set(git("ls-files", "--cached", "--others", "--exclude-standard", "-z").split(b"\0")) - {b""}
    # Ignored source/config files still affect Cargo/BuildKit. Include them in
    # build-input roots; do not follow source symlinks outside the checkout.
    for name in ("crates", "vendor", "packaging", "kits", "scripts", ".cargo"):
        root = source_tree / name
        if root.is_dir():
            for directory, dirs, files in os.walk(root, followlinks=False):
                dirs[:] = [entry for entry in dirs if entry not in ("target", ".git", "__pycache__")]
                for entry in files + [entry for entry in dirs if (Path(directory) / entry).is_symlink()]:
                    paths.add(os.fsencode((Path(directory) / entry).relative_to(source_tree)))
    digest = hashlib.sha256()
    records = {}
    for encoded in sorted(paths):
        path = source_tree / os.fsdecode(encoded)
        digest.update(encoded + b"\0")
        try:
            metadata = path.lstat()
        except FileNotFoundError:
            digest.update(b"missing\0")
            records[os.fsdecode(encoded)] = {"kind": "missing"}
            continue
        record = {"mode": stat.S_IMODE(metadata.st_mode)}
        records[os.fsdecode(encoded)] = record
        digest.update(f"{stat.S_IMODE(metadata.st_mode):04o}".encode() + b"\0")
        if path.is_symlink():
            target = os.readlink(path)
            digest.update(b"symlink\0" + os.fsencode(target))
            record.update(kind="symlink", target=target)
            # An external build input cannot be attested by a checkout digest.
            if not path.resolve().is_relative_to(source_tree):
                raise ValueError(f"source symlink escapes checkout: {path}")
        elif path.is_file():
            content_hash = file_digest(path)
            digest.update(b"file\0" + content_hash.encode())
            record.update(kind="file", sha256=content_hash)
        elif path.is_dir():
            raise ValueError(f"unfingerprinted directory/submodule input: {path}")
        else:
            raise ValueError(f"unsupported source input: {path}")
        digest.update(b"\0")
    return {"source_revision": revision,
            "source_dirty": bool(git("status", "--porcelain=v1", "-z", "--untracked-files=all")),
            "source_tree_sha256": "sha256:" + digest.hexdigest()}, records


def capture_source(source_tree: Path, revision: str) -> tuple[dict, dict]:
    """Capture identity and its per-file hash map in one observation."""
    return _source_snapshot(source_tree, revision)


def source_identity(source_tree: Path, revision: str) -> dict:
    """Fingerprint current source. This alone says nothing about a binary's origin."""
    return _source_snapshot(source_tree, revision)[0]


def source_file_records(source_tree: Path, revision: str) -> dict:
    """Private UAT edit fence; not a build receipt or a verifier override."""
    return _source_snapshot(source_tree, revision)[1]


def input_files(root: Path) -> dict:
    return {name: {"sha256": row["sha256"], "mode": row["mode"]}
            for name, row in tree_inventory(root)["files"].items()}


def verify_shell_image_proof(source: Path, guest: Path, repository: str, insecure: bool) -> dict:
    proof = json.loads((guest / "shell-image.build.json").read_text())
    reference = (guest / "shell-image").read_text().strip()
    digest = proof.get("buildx_metadata", {}).get("containerimage.digest")
    expected_inputs = {name: file_digest(source / name) for name in (
        "packaging/shell-image", "packaging/shell/Dockerfile", "scripts/prepare-shell-image.py")}
    expected_inputs["packaging/image-repair"] = input_files(source / "packaging/image-repair")
    expected_inputs["packaging/dhi-notices/collected"] = input_files(source / "packaging/dhi-notices/collected")
    expected_inputs["packaging/shell/pi"] = input_files(source / "packaging/shell/pi")
    expected_inputs["support_scripts"] = {name: file_digest(source / "scripts" / name)
                                          for name in ("build_inputs.py", "image_observations.py", "owned_process.py", "stock_sdk.py")}
    output = f"type=image,name={repository}:repaired,push=true" + (",registry.insecure=true" if insecure else "")
    argv = proof.get("build_argv", [])
    if (proof.get("schema") != "marsh.prepared-shell-image/v1" or proof.get("reference") != reference or
            proof.get("platform") != "linux/arm64" or proof.get("insecure_registry") is not insecure or
            proof.get("inputs") != expected_inputs or
            proof.get("upstream_image") != (source / "packaging/shell-image").read_text().strip() or
            not isinstance(argv, list)):
        raise ValueError("generated shell image proof does not match source, publication inputs or packaged reference")
    if repository:
        if (not isinstance(digest, str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", digest) or
                reference != f"{repository}@{digest}" or proof.get("publication") != "explicit-registry" or
                proof.get("publication_effects") != [reference] or argv.count("--output") != 1 or
                argv[argv.index("--output") + 1:argv.index("--output") + 2] != [output]):
            raise ValueError("generated shell image proof publication mismatch")
    else:
        if (proof.get("publication") != "local-template" or proof.get("publication_effects") != [] or insecure or
                argv.count("--load") != 1 or argv.count("--tag") != 1 or "--output" in argv):
            raise ValueError("local shell image proof cannot claim registry publication")
        verify_local_image_proof(proof, reference, "shell")
    return proof


def verify_build_receipt(receipt: Path, *, source_tree: Path, revision: str,
                         marsh: Path, guest_artifacts: Path) -> dict:
    """Reject mismatches before the caller creates a scope or invokes SBX/product."""
    receipt = host_only_path(receipt, source_tree)
    value = json.loads(receipt.read_text())
    if value.get("schema") != SCHEMA or value.get("outcome") != "passed":
        raise ValueError("missing successful observed-build receipt")
    current, source_files = capture_source(source_tree, revision)
    if value.get("source_before") != current or value.get("source_after") != current:
        raise ValueError("build receipt source mismatch; rebuild this exact source")
    if value.get("source_files") != source_files:
        raise ValueError("build receipt source file map mismatch; rebuild with the current observer")
    expected_paths = {"source_tree": str(source_tree.resolve(strict=True)),
                      "marsh": str(marsh.resolve(strict=True)),
                      "guest_artifacts": str(guest_artifacts.resolve(strict=True))}
    if any(value.get(key) != path for key, path in expected_paths.items()):
        raise ValueError("build receipt paths do not match executed harness artifacts")
    try:
        current_artifacts = artifact_manifest(marsh, guest_artifacts)
    except (ValueError, OSError) as error:
        raise ValueError(f"build receipt artifact mismatch: {error}") from error
    if value.get("artifacts") != current_artifacts:
        raise ValueError("build receipt artifact mismatch; rebuild this exact package")
    build = value.get("build", {})
    if (build.get("exit_status") != 0 or not build.get("argv") or
            not build.get("tools") or not build.get("environment") or
            not build.get("started_unix_ns") or
            build.get("finished_unix_ns", 0) < build["started_unix_ns"]):
        raise ValueError("receipt lacks an observed successful build command and inputs")
    if value.get("shell_image_proof") != verify_shell_image_proof(
            source_tree, guest_artifacts, build.get("shell_image_repository"), build.get("shell_image_insecure")):
        raise ValueError("shell image proof differs from observed build receipt")
    return value


def copy_verified_package(receipt: dict, candidate: Path, guest: Path) -> dict:
    """Host-copy an observed package, comparing ALL copied bytes/types/modes.

    Destination directories must be new. On failure the caller removes only its
    own staging root. Source and copies are checked before any product effects.
    """
    original = Path(receipt["marsh"])
    original_guest = Path(receipt["guest_artifacts"])
    destinations = (candidate, guest)
    if (any(not path.is_absolute() or path.resolve() != path for path in destinations)
            or candidate.is_relative_to(guest) or guest.is_relative_to(candidate)
            or any(path.is_relative_to(origin) or origin.is_relative_to(path)
                   for path in destinations for origin in (original.parent, original_guest))):
        raise ValueError("package relocation requires separate canonical destination trees outside its inputs")
    if artifact_manifest(original, original_guest) != receipt["artifacts"]:
        raise ValueError("input package changed before relocation")
    candidate.mkdir(mode=0o700)
    for name in HOST_NAMES:
        shutil.copy2(original.with_name(name), candidate / name)
    shutil.copytree(original_guest, guest)
    marsh = candidate / "marsh"
    expected = relocated_manifest(receipt, marsh, guest)
    if (artifact_manifest(original, original_guest) != receipt["artifacts"] or
            artifact_manifest(marsh, guest) != expected):
        raise ValueError("package bytes/modes changed during relocation")
    return {"schema": "marsh.package-relocation/v1", "source_marsh": str(original),
            "source_guest_artifacts": str(original_guest), "source_identity": receipt["source_after"],
            "marsh": str(marsh), "guest_artifacts": str(guest), "artifacts": expected}


def parse_stock_inventory(document: dict) -> dict[str, str]:
    rows = document.get("sandboxes")
    if not isinstance(rows, list):
        raise ValueError("invalid stock SBX sandbox inventory")
    inventory = {}
    for row in rows:
        if not isinstance(row, dict) or any(not isinstance(row.get(key), str) or not row[key]
                                            for key in ("name", "id")):
            raise ValueError("stock inventory requires exact names and stable VM IDs")
        if row["name"] in inventory or row["id"] in inventory.values():
            raise ValueError("duplicate stock sandbox name or stable ID")
        inventory[row["name"]] = row["id"]
    return inventory


def observe_stock_inventory(run) -> dict[str, str]:
    result = run(["ls", "--json"])
    if result.returncode:
        raise ValueError("stock SBX listing failed")
    document = json.loads(result.stdout)
    rows = document.get("sandboxes")
    if not isinstance(rows, list):
        raise ValueError("invalid stock SBX sandbox inventory")
    # Some stock versions omit IDs from ls. Obtain the exact ID through inspect,
    # never synthesize an ID from a name or trust a name as deletion evidence.
    for row in rows:
        if isinstance(row, dict) and isinstance(row.get("name"), str) and not row.get("id"):
            inspected = run(["inspect", row["name"], "--json"])
            if inspected.returncode:
                raise ValueError(f"cannot identify stock VM: {row['name']}")
            details = json.loads(inspected.stdout)
            if details.get("name") != row["name"]:
                raise ValueError("stock inspect returned a different sandbox name")
            row["id"] = details.get("id")
    return parse_stock_inventory(document)


def stock_vm_inventory(sbx: str | Path, *, env: dict | None = None) -> dict[str, str]:
    return observe_stock_inventory(lambda argv: run_owned(
        [str(sbx), *argv], capture_output=True, check=False, timeout=30,
        env=env, stdin=subprocess.DEVNULL))


# Historical caller name retained; values are stable IDs, not a set of names.
stock_vm_names = stock_vm_inventory


def remove_owned_stock_vm(sbx: str | Path, name: str, identifier: str | None,
                          baseline: dict[str, str], *, env: dict | None = None,
                          cwd: str | Path | None = None) -> None:
    """Remove a created-and-captured ID, never a new resource reusing its name."""
    if not identifier or name in baseline or identifier in baseline.values():
        raise ValueError("no exclusive stock VM identity for cleanup")
    current = stock_vm_inventory(sbx, env=env)
    if name in current and current[name] != identifier:
        raise ValueError(f"owned stock VM was replaced: {name}; refusing removal")
    if identifier in current.values():
        if current.get(name) != identifier:
            raise ValueError("owned stock VM changed names; refusing removal")
        # Local stock CLI accepts names, unlike Cloud's stable-ID interface.
        # The captured ID authorizes cleanup; a healthy inventory fences the
        # name immediately before submission and proves exact absence after it.
        result = run_owned([str(sbx), "rm", "--force", name], capture_output=True,
                           timeout=90, env=env, cwd=cwd, stdin=subprocess.DEVNULL)
        if result.returncode:
            raise ValueError(f"stock removal failed for {identifier}: {result.stderr.decode(errors='replace')}")
    if identifier in stock_vm_inventory(sbx, env=env).values():
        raise ValueError(f"owned stock VM remains: {identifier}")


BASELINE_ENV = "MARSH_REGRESS_BASELINE"


class SharedBaseline(dict):
    """Stock VMs that existed before tests/regress.py started any suite.

    Peer suites create and remove their own VMs while this one runs, so a live
    snapshot taken at this suite's start would hold peers' VMs: they would
    later "disappear" (their owner removed them) or look like leaks (they are
    not ours). The runner's pre-launch snapshot holds only resources no suite
    owns. Leak checks then cover only the VMs this suite itself owns (see
    stock_cleanup_errors); the runner repeats the strict global check once
    every suite has finished.
    """


def stock_baseline(sbx: str | Path, *, env: dict | None = None) -> dict[str, str]:
    """The pre-existing inventory a harness compares against.

    Standalone (no MARSH_REGRESS_BASELINE) this is a live snapshot, and any new
    marsh-* VM at the end fails the run. Under the runner it is the runner's
    snapshot; every entry is still required to be unchanged (same name and
    stable ID) at the end of this suite.
    """
    live = stock_vm_inventory(sbx, env=env)
    path = (env if env is not None else os.environ).get(BASELINE_ENV)
    if not path:
        return live
    document = json.loads(Path(path).read_text(encoding="utf-8"))
    vms = document.get("vms") if isinstance(document, dict) else None
    if not isinstance(vms, dict) or not all(isinstance(k, str) and isinstance(v, str)
                                            for k, v in vms.items()):
        raise ValueError(f"malformed shared stock baseline: {path}")
    return SharedBaseline(vms)


def stock_cleanup_errors(before: dict[str, str], after: dict[str, str],
                         owned: "set[str] | frozenset[str] | None" = None) -> list[str]:
    """Pre-existing VMs must be unchanged and none of this suite's own may remain.

    `owned` names the VMs this suite created or recorded in its own ownership
    map. With a SharedBaseline it is required and bounds the leak check to
    them (concurrent peers' VMs are neither pre-existing nor leaks); with a
    private snapshot every new marsh-* VM is a leak, as before.
    """
    errors = []
    if not isinstance(before, dict) or not isinstance(after, dict):
        raise ValueError("cleanup requires name-to-stable-ID inventories, not names alone")
    if set(before) - set(after):
        errors.append(f"pre-existing stock sandboxes disappeared: {sorted(set(before) - set(after))}")
    for name in before.keys() & after.keys():
        if before[name] != after[name]:
            errors.append(f"pre-existing stock sandbox was replaced: {name}: {before[name]} -> {after[name]}")
    new = after.keys() - before.keys()
    if isinstance(before, SharedBaseline):
        if owned is None:
            raise ValueError("a shared stock baseline requires the suite's own VM names")
        leaked = sorted(name for name in new if name in owned)
    else:
        # New unrelated concurrent marsh runs conservatively fail qualification;
        # inventory differences never grant authority to remove them.
        leaked = sorted(name for name in new if name.startswith("marsh-"))
    if leaked:
        errors.append(f"new marsh sandboxes remain: {leaked}")
    return errors


class DriveObserver:
    """Read-only stock-SBX witness, running on the host, never in agent checkout.

    Scope directory names are discovery hints only. Success requires an actual
    newly listed VM, candidate bytes/help executed by stock SBX, two fresh nonroot
    containers per scope with the requested argv/project, and healthy-Engine
    deletion. Guest command implementations and Docker are still trusted: this
    detects lying guest reports, not hostile-root fabrication of exec responses.
    Without expected_candidate_digest it proves staged/runtime agreement only,
    NOT a source-to-child-build relationship or equality to the host package.
    """
    def __init__(self, sbx: Path, checkout: Path, marker: str, baseline: dict[str, str], *,
                 route_marker: str | None = None, expected_candidate_digest: str | None = None):
        self.sbx, self.checkout, self.marker = sbx, checkout, marker
        self.route_marker = route_marker or marker
        self.expected_candidate_digest = expected_candidate_digest
        self.baseline = baseline
        self.vms: set[str] = set()
        self.vm_identities: dict[str, str] = {}
        self.candidates: dict[str, dict] = {}
        self.containers: dict[str, dict] = {}
        self.deleted: set[str] = set()
        self.records: list[dict] = []
        self.error: str | None = None
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self._watch, daemon=True)

    def command(self, argv):
        result = subprocess.run([str(self.sbx), *argv], capture_output=True, timeout=30, start_new_session=True)
        self.records.append({"argv": [str(self.sbx), *argv], "status": result.returncode,
                             "stdout": result.stdout.decode(errors="replace"),
                             "stderr": result.stderr.decode(errors="replace")})
        return result

    def poll(self):
        from run import verify_container_deleted
        current = observe_stock_inventory(self.command)
        for name, identifier in {**self.baseline, **self.vm_identities}.items():
            if name in current and current[name] != identifier:
                raise ValueError(f"stock sandbox identity changed during observation: {name}")
        if not self.baseline.keys() <= current.keys():
            raise ValueError("pre-existing stock sandbox disappeared during observation")
        scope_root = self.checkout / ".marsh-dev"
        if scope_root.resolve() != scope_root:
            raise ValueError("child-scope discovery path traverses a symlink")
        scopes = [path for path in scope_root.glob("*")
                  if re.fullmatch(r"[0-9a-f]{8}-[0-9a-f-]{27}", path.name)
                  and path.is_dir() and not path.is_symlink()]
        for scope in scopes:
            prefix = f"marsh-dev-{scope.name[:8]}-"
            for vm in sorted(current.keys() - self.baseline.keys()):
                if not vm.startswith(prefix):
                    continue
                self.vms.add(vm)
                self.vm_identities[vm] = current[vm]
                exec_prefix = ["exec", current[vm]]
                if vm not in self.candidates:
                    help_result = self.command([*exec_prefix, "/usr/local/bin/marsh", "--help"])
                    if help_result.returncode == 0 and ("marsh - " + self.marker).encode() in help_result.stdout.splitlines():
                        hashes = self.command([*exec_prefix, "sha256sum", "/usr/local/bin/marsh"])
                        artifact = scope / "artifacts/marsh-linux-arm64"
                        if artifact.resolve() != artifact:
                            raise ValueError("staged candidate path traverses a symlink")
                        expected = file_digest(artifact)
                        if self.expected_candidate_digest is not None and expected != self.expected_candidate_digest:
                            raise ValueError("child candidate differs from the host-observed Linux package")
                        if hashes.returncode or hashes.stdout.split()[0].decode() != expected.removeprefix("sha256:"):
                            raise ValueError("executed child candidate differs from staged artifact")
                        self.candidates[vm] = {"scope_id": scope.name, "sha256": expected,
                                               "help": help_result.stdout.decode()}
                active = self.command([*exec_prefix, "docker", "ps", "--no-trunc", "-q"])
                if active.returncode:
                    continue  # Booting VMs do not constitute proof.
                for container in active.stdout.decode().splitlines():
                    if container in self.containers or not re.fullmatch(r"[0-9a-f]{64}", container):
                        continue
                    inspected = self.command([*exec_prefix, "docker", "inspect", container])
                    if inspected.returncode:
                        continue  # May have exited before inspection; no proof recorded.
                    rows = json.loads(inspected.stdout)
                    if len(rows) != 1 or rows[0].get("Id") != container:
                        raise ValueError("runtime container identity mismatch")
                    row = rows[0]
                    attempts = [attempt for attempt in (1, 2)
                                if f"printf '{self.route_marker}_{attempt}\\n'; sleep 20" in row.get("Config", {}).get("Cmd", [])]
                    if len(attempts) != 1:
                        continue  # Not the exact challenge workload.
                    if row.get("HostConfig", {}).get("Privileged") is not False:
                        raise ValueError("challenge container has privileged/unknown authority")
                    project = str(scope / "project")
                    state = row.get("State", {})
                    if (type(state.get("Pid")) is not int or state["Pid"] <= 0
                            or not isinstance(state.get("StartedAt"), str) or not state["StartedAt"]):
                        raise ValueError("challenge container lacks live process identity")
                    if not state.get("Running") or not any(
                            mount.get("Destination") == project and mount.get("RW") is True
                            for mount in row.get("Mounts", [])):
                        raise ValueError("challenge container lacks live project identity")
                    uid = self.command([*exec_prefix, "docker", "exec", container, "id", "-u"])
                    if uid.returncode or not uid.stdout.strip().isdigit() or int(uid.stdout) == 0:
                        raise ValueError("challenge container is not independently observed nonroot")
                    self.containers[container] = {"vm": vm, "scope_id": scope.name,
                                                  "attempt": attempts[0], "uid": int(uid.stdout),
                                                  "process": {"pid": state["Pid"], "started_at": state["StartedAt"]},
                                                  "challenge": self.route_marker, "inspect": row}
        for container, row in self.containers.items():
            if container in self.deleted or row["vm"] not in current:
                continue
            prefix = ["exec", self.vm_identities[row["vm"]]]
            inspected = self.command([*prefix, "docker", "inspect", container])
            if inspected.returncode:
                verify_container_deleted(lambda argv: self.command(argv), prefix, container)
                self.deleted.add(container)

    def _watch(self):
        try:
            while not self.stop.is_set():
                self.poll()
                self.stop.wait(0.5)
        except Exception as error:
            self.error = f"{type(error).__name__}: {error}"

    def start(self):
        self.thread.start()

    def finish(self):
        self.stop.set()
        self.thread.join(timeout=65)
        if self.thread.is_alive():
            raise ValueError("host observer did not stop")

    def verify(self):
        if self.error:
            raise ValueError(self.error)
        scopes = {row["scope_id"] for row in self.candidates.values()}
        eligible = {key: row for key, row in self.containers.items() if row["scope_id"] in scopes}
        if not scopes or len(eligible) < 2:
            raise ValueError("no independent candidate runtime and two fresh container observations; agent JSON is not proof")
        if any(row["vm"] in self.candidates for row in eligible.values()):
            raise ValueError("registered challenge ran in the shell VM instead of a separate Kit worker")
        for scope in scopes:
            runs = [row for row in eligible.values() if row["scope_id"] == scope]
            if (len(runs) != 2 or len({row["vm"] for row in runs}) != 1 or
                    {row["attempt"] for row in runs} != {1, 2}):
                raise ValueError("each candidate scope must run both attempts in one reused worker VM")
            if len({(row["process"]["pid"], row["process"]["started_at"]) for row in runs}) != 2:
                raise ValueError("challenge containers lack distinct runtime process identities")
        if not set(eligible).issubset(self.deleted):
            raise ValueError("container deletion not independently verified before VM teardown")

    def evidence(self):
        return {"trust_limit": "host-issued stock probes into guest-controlled userspace, not hostile-guest attestation",
                "expected_candidate_digest": self.expected_candidate_digest,
                "route_marker": self.route_marker, "candidates": self.candidates, "containers": self.containers,
                "deleted_containers": sorted(self.deleted), "owned_vms": dict(self.vm_identities),
                "error": self.error, "commands": self.records}


def candidate_environment(args, receipt: dict | None) -> dict[str, str]:
    """Explicit host launch environment for a verified candidate.

    The product removed LOCAL shell authority: a local shell image tag needs no
    side-channel proof, so no extra launch environment is required.
    """
    return {}


def candidate_arguments(parser) -> None:
    parser.add_argument("--build-receipt", type=Path,
                        help="optional host-owned receipt from scripts/build-candidate.py; "
                             "when absent only source identity is recorded")
    parser.add_argument("--guest-artifacts", type=Path,
                        help="exact guest package used by the candidate")


def verify_candidate(args, marsh: str) -> tuple[Path, dict | None]:
    guest = Path(getattr(args, "guest_artifacts", None) or
                 os.environ.get("MARSH_GUEST_ARTIFACTS", Path(marsh).parent.parent / "libexec/marsh")).resolve()
    if getattr(args, "build_receipt", None) is None:
        return guest, None
    receipt = verify_build_receipt(Path(args.build_receipt), source_tree=Path(args.source_tree),
                                   revision=args.source_revision, marsh=Path(marsh), guest_artifacts=guest)
    return guest, receipt
