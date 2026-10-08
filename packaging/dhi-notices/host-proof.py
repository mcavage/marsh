#!/usr/bin/env python3
"""Host-only local-Docker proof: reviewed-source bind OR actual baked payload.

Its CLI tests run no Docker, registry or agent. The caller supplies
primary index-capture receipts; this driver uses an empty credential config and
never pulls. Optional Codex version query explicitly executes the pinned CLI.
"""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import resource
import shlex
import stat
import subprocess
import sys
from recipe_instructions import recipe_instructions

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
MAX_OUTPUT = 128 * 1024**2
RECEIPTS = {"image-verification.json", "base-image-verification.json"}


def sha(path):
    h = hashlib.sha256()
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as source:
        metadata = os.fstat(source.fileno())
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > 1024**3:
            raise ValueError("bounded regular proof input required")
        while data := source.read(1024**2):
            h.update(data)
    return h.hexdigest()


def unique(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON key")
        result[key] = value
    return result


def read_json(path):
    if path.is_symlink() or path.stat().st_size > 32 * 1024**2:
        raise ValueError("bounded no-follow JSON required")
    return json.loads(path.read_bytes(), object_pairs_hook=unique)


def bundle_records(directory):
    """Independent host implementation of the documented v2 tree format."""
    result = {}
    total = 0
    for path in sorted(directory.rglob("*")):
        relative = str(path.relative_to(directory))
        metadata = path.lstat()
        if stat.S_ISDIR(metadata.st_mode):
            continue
        if not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1:
            raise ValueError("nonregular/hardlinked reviewed bundle input")
        if relative in RECEIPTS:
            continue
        total += metadata.st_size
        if total > 256 * 1024**2 or len(result) >= 20000:
            raise ValueError("reviewed bundle bounds")
        result[relative] = {
            "sha256": sha(path),
            "size": metadata.st_size,
            "mode": stat.S_IMODE(metadata.st_mode),
        }
    return result


def tree_digest(records):
    return hashlib.sha256(
        json.dumps(records, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()


def reviewed_bundle(manifest_path, baked):
    reviewed = read_json(manifest_path)
    prefix = "packaging/dhi-notices/collected/"
    records = {
        name.removeprefix(prefix): record
        for name, record in reviewed.items()
        if name.startswith(prefix) and name.removeprefix(prefix) not in RECEIPTS
    }
    if not records or bundle_records(HERE / "collected") != records:
        raise ValueError("current bundle is not the externally reviewed source tree")
    if baked:
        # Shared staging contract: read bits plus original execute bits. Source
        # permissions still had to match the reviewed manifest above.
        records = {
            name: dict(row, mode=0o644 | (row["mode"] & 0o111))
            for name, row in records.items()
        }
    return records


def reviewed_fence(before, reviewed):
    """Review is an external anchor, including all driver/repair source inputs."""
    expected = {name: row for name, row in reviewed.items() if fenced_name(name)}
    if before != expected:
        changed = sorted(name for name in before.keys() | expected.keys()
                         if before.get(name) != expected.get(name))
        raise ValueError("current source fence is not the externally reviewed source tree: "
                         + ", ".join(changed[:8]))


def recipe_references(text, shell_base):
    aliases, required = set(), set()
    shell_arg = False
    for line in recipe_instructions(text):
        match = re.match(r"^\s*(FROM|ARG)\s+(.+)$", line, re.I)
        if not match:
            continue
        instruction, rest = match.groups()
        if instruction.upper() == "ARG":
            shell_arg |= rest.split("=", 1)[0].strip() == "SHELL_BASE_IMAGE"
            continue
        tokens = shlex.split(rest)
        if tokens and tokens[0].startswith("--platform="):
            tokens.pop(0)
        if len(tokens) not in (1, 3) or (len(tokens) == 3 and tokens[1].upper() != "AS"):
            raise ValueError("unsupported recipe FROM instruction")
        ref = tokens[0]
        if ref == "${SHELL_BASE_IMAGE}" and shell_arg:
            if not shell_base:
                raise ValueError("dynamic shell recipe requires exact producer base index input")
            ref = shell_base
        if re.fullmatch(r"[A-Za-z0-9._:/-]+@sha256:[0-9a-f]{64}", ref):
            required.add(ref.rsplit("@", 1)[1])
        elif ref.lower() not in aliases:
            raise ValueError("unpinned/unknown recipe FROM: " + ref)
        if len(tokens) == 3:
            alias = tokens[2].lower()
            if not re.fullmatch(r"[a-z][a-z0-9_.-]*", alias) or alias in aliases:
                raise ValueError("invalid/duplicate recipe stage alias")
            aliases.add(alias)
    if not required:
        raise ValueError("no pinned recipe FROM")
    return required


def index_bindings(paths, platform, docker_hash):
    """Validate root-observed raw index AND platform bytes, not restated metadata."""
    bindings = []
    for path in paths:
        document = read_json(path)
        if (
            document["schema"] not in {"marsh.primary-index-capture/v1", "marsh.primary-index-capture/v2"}
            or document.get("error")
            or document["docker_sha256"] != docker_hash
        ):
            raise ValueError("wrong index-capture schema/tool")
        if document["schema"].endswith("/v2") and (
            not re.fullmatch(r"[0-9a-f]{64}", document.get("buildx_sha256", ""))
            or not Path(document.get("buildx_path", "")).is_absolute()
            or not Path(document.get("docker_home", "")).is_absolute()
        ):
            raise ValueError("explicit index-capture tool/home binding required")
        ref = document["reference"]
        expected_index = ref.rsplit("@sha256:", 1)[-1]
        raw_path = path.parent / document["index_file"]
        child_path = path.parent / document["platform_file"]
        for item in [raw_path, child_path]:
            if (
                item.parent != path.parent
                or item.is_symlink()
                or item.stat().st_size > 4 * 1024**2
            ):
                raise ValueError("unsafe raw index evidence")
        if sha(raw_path) != expected_index or sha(raw_path) != document["index_sha256"]:
            raise ValueError("raw index does not hash to FROM pin")
        index = read_json(raw_path)
        os_name, architecture = platform.split("/")
        selected = [
            r
            for r in index["manifests"]
            if r.get("platform", {}).get("os") == os_name
            and r.get("platform", {}).get("architecture") == architecture
        ]
        if len(selected) != 1:
            raise ValueError("index platform selection is ambiguous/missing")
        descriptor = selected[0]
        if (
            sha(child_path) != descriptor["digest"].removeprefix("sha256:")
            or child_path.stat().st_size != descriptor["size"]
        ):
            raise ValueError("primary platform bytes do not match index descriptor")
        if (
            document["platform_digest"] != descriptor["digest"]
            or document["platform"] != platform
        ):
            raise ValueError("index-capture platform assertion mismatch")
        expected_refs = [ref, ref.split("@", 1)[0] + "@" + descriptor["digest"]]
        for command, expected_ref, raw in zip(
            document["commands"], expected_refs, [raw_path, child_path], strict=True
        ):
            expected_argv = ([document["buildx_path"]] if document["schema"].endswith("/v2")
                             else [command["argv"][0], "buildx"])
            expected_argv += ["imagetools", "inspect", "--raw", expected_ref]
            if command["status"] != 0 or command["argv"] != expected_argv:
                raise ValueError("not an observed read-only imagetools capture")
            if command["stdout_sha256"] != sha(raw):
                raise ValueError("raw index command output mismatch")
        if len(document["commands"]) != 2:
            raise ValueError("index capture command count")
        bindings.append(
            {
                "reference": ref,
                "index_sha256": sha(raw_path),
                "platform_digest": descriptor["digest"],
                "platform_sha256": sha(child_path),
                "capture_receipt_sha256": sha(path),
                "capture_receipt": str(path),
                "commands": document["commands"],
                "tool_execution_scope": ("explicit buildx executable" if document["schema"].endswith("/v2")
                                         else "historical Docker plugin dispatch; selected plugin not proven"),
                "base_layer_ancestry": "not checked; no platform config/diff_id capture or built-image layer comparison",
            }
        )
    return bindings


def source_fence():
    for directory in (HERE, ROOT / "packaging/image-repair"):
        for path in directory.rglob("*"):
            if "__pycache__" not in path.parts and path.is_symlink():
                raise ValueError("symlink in reviewed source fence: " + str(path))
    paths = [p for p in HERE.rglob("*") if p.is_file() and "__pycache__" not in p.parts]
    paths += list((ROOT / "kits").glob("*/*.dockerfile"))
    paths += list((ROOT / "kits").glob("*/notices/overrides.json"))
    paths += [
        ROOT / name
        for name in [
            "packaging/shell/Dockerfile",
            "scripts/npm-notices.mjs",
            "kits/marsh-codex/marsh-entrypoint.sh",
            "kits/marsh-codex/release-checksums.txt",
            "kits/marsh-claude/release-checksums.txt",
            "packaging/shell-image",
            "packaging/image-repair/repair.py",
            "packaging/image-repair/artifacts.json",
            "packaging/image-repair/base-images.json",
        ]
    ]
    paths += [p for p in (ROOT / "packaging/image-repair").rglob("*")
              if p.is_file() and "__pycache__" not in p.parts]
    return {
        str(p.relative_to(ROOT)): {
            "sha256": sha(p),
            "size": p.stat().st_size,
            "mode": stat.S_IMODE(p.lstat().st_mode),
        }
        for p in sorted(set(paths))
    }


def fenced_name(name):
    path = Path(name)
    return (name.startswith(("packaging/dhi-notices/", "packaging/image-repair/"))
            and "__pycache__" not in path.parts) or name in {
                "packaging/shell/Dockerfile",
                "scripts/npm-notices.mjs", "kits/marsh-codex/marsh-entrypoint.sh",
                "kits/marsh-codex/release-checksums.txt", "kits/marsh-claude/release-checksums.txt",
                "packaging/shell-image",
            } or (len(path.parts) >= 3 and path.parts[0] == "kits"
                  and (len(path.parts) == 3 and path.suffix == ".dockerfile"
                       or len(path.parts) == 4 and path.parts[2:] == ("notices", "overrides.json")))


# Read known receipt files only; no bundle code is imported/executed by this reader.
READ_BAKED = b"""import os,stat,json,base64,hashlib
root='/usr/local/share/licenses/'
paths=['marsh-dhi/base-image-verification.json','marsh-dhi/image-verification.json','marsh-image-repair/repair-receipt.json','marsh-image-repair/base-images.json']
rows={}
for name in paths:
 try: fd=os.open(root+name,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK)
 except FileNotFoundError: continue
 with os.fdopen(fd,'rb') as f:
  s=os.fstat(f.fileno())
  if not stat.S_ISREG(s.st_mode) or s.st_size>8*1024*1024: raise ValueError('receipt bound/type')
  data=f.read(8*1024*1024+1)
  if len(data)!=s.st_size: raise ValueError('receipt changed')
  rows[name]={'sha256':hashlib.sha256(data).hexdigest(),'base64':base64.b64encode(data).decode()}
print(json.dumps(rows))
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--docker", type=Path, required=True)
    parser.add_argument("--docker-host", required=True)
    parser.add_argument("--image", required=True)
    parser.add_argument(
        "--platform", choices=["linux/arm64", "linux/amd64"], required=True
    )
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--reviewed-source", type=Path, required=True)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument(
        "--baked",
        action="store_true",
        help="NO bundle bind; compare fresh and baked receipts externally",
    )
    mode.add_argument(
        "--supporting-base-bind",
        action="store_true",
        help="Supporting public-base evidence only",
    )
    mode.add_argument("--filesystem", action="store_true")
    mode.add_argument("--legal-files", action="store_true")
    parser.add_argument("--index-record", type=Path, action="append", default=[])
    parser.add_argument("--recipe", type=Path)
    parser.add_argument(
        "--shell-base-index",
        help="Exact producer SHELL_BASE_IMAGE input for the dynamic repaired-shell recipe; producer validation is image_observations.verified_shell_image's",
    )
    parser.add_argument(
        "--build-receipt",
        type=Path,
        help="Retain hash; producer validation is image_observations.verified_shell_image's",
    )
    parser.add_argument("--copied-agents", action="store_true")
    parser.add_argument("--profile", choices=["claude-kit", "codex-kit"])
    parser.add_argument("--version")
    parser.add_argument(
        "--adapter", choices=["claude-acp", "codex-acp", "pi"]
    )
    parser.add_argument(
        "--expect-agent", choices=["auto", "none", "claude", "codex"], default="auto"
    )
    parser.add_argument(
        "--check-codex-selection",
        action="store_true",
        help="Executes only codex --version; root-owned effect",
    )
    args = parser.parse_args()
    if not args.docker_host.startswith("unix:///") or "\n" in args.docker_host:
        parser.error("explicit local Unix Docker endpoint required")
    if not re.fullmatch(r"(?:[A-Za-z0-9._:/-]+@)?sha256:[0-9a-f]{64}", args.image):
        parser.error("immutable image reference/ID required")
    if (
        bool(args.profile) != bool(args.version)
        or sum(bool(x) for x in [args.profile, args.copied_agents]) > 1
        or args.adapter and args.copied_agents
        or args.adapter
        and args.profile
        and {"claude-kit": "claude-acp", "codex-kit": "codex-acp"}[args.profile]
        != args.adapter
    ):
        parser.error(
            "choose one scope (a Kit profile may add its own adapter); native Kit profile needs exact version"
        )
    if args.version is not None and not re.fullmatch(
        r"[0-9]+\.[0-9]+\.[0-9]+", args.version
    ):
        parser.error("numeric exact version required")
    if args.baked and not (args.recipe and args.build_receipt and args.index_record):
        parser.error(
            "baked proof requires recipe, producer receipt and raw index capture(s)"
        )
    if args.check_codex_selection and not (args.baked and args.profile == "codex-kit"):
        parser.error("version execution is only for an explicit baked codex-kit proof")
    output = args.evidence.absolute()
    for parent in output.parents:
        if not stat.S_ISDIR(parent.lstat().st_mode):
            raise ValueError("symlink/non-directory evidence ancestor")
    output.mkdir(mode=0o700, exist_ok=False)
    if (
        output.stat().st_uid != os.getuid()
        or stat.S_IMODE(output.stat().st_mode) != 0o700
    ):
        raise ValueError("evidence must be private")
    try:
        docker = args.docker.resolve(strict=True)
        python = Path(sys.executable).resolve()
        tool = {"path": str(docker), "sha256": sha(docker)}
        driver = {
            "argv": sys.argv,
            "sha256": sha(Path(__file__).resolve()),
            "python": str(python),
            "python_sha256": sha(python),
            "python_version": sys.version,
        }
        before = source_fence()
        reviewed_sha = sha(args.reviewed_source)
        reviewed = read_json(args.reviewed_source)
        reviewed_fence(before, reviewed)
        expected = reviewed_bundle(args.reviewed_source, args.baked)
        expected_hash = tree_digest(expected)
    except Exception as exception:
        # No external command has run. Keep a typed refusal separate from success
        # proof fields so callers cannot adopt a partial observation.
        refusal = {
            "schema": "marsh.canonical-host-proof/v2", "passed": False,
            "phase": "pre-effect-source-admission", "error": str(exception),
            "requested_image": args.image, "platform": args.platform,
            "baked": args.baked, "commands": [], "actual_image": None,
            "reviewed_source_sha256": locals().get("reviewed_sha"),
            "source_before": locals().get("before"),
        }
        (output / "proof.json").write_text(json.dumps(refusal, indent=2) + "\n")
        print(json.dumps({"proof": str(output / "proof.json"), "passed": False}))
        return 1
    config = output / "empty-docker-config"
    config.mkdir(mode=0o700)
    environment = {
        "PATH": "/usr/bin:/bin",
        "HOME": str(output),
        "DOCKER_CONFIG": str(config),
        "DOCKER_HOST": args.docker_host,
    }
    commands = []

    def limit_output():
        resource.setrlimit(resource.RLIMIT_FSIZE, (MAX_OUTPUT, MAX_OUTPUT))

    def run(label, argv, stdin=None):
        stdout, stderr = output / (label + ".stdout"), output / (label + ".stderr")
        with stdout.open("xb") as out, stderr.open("xb") as err:
            result = subprocess.run(
                argv,
                input=stdin,
                stdout=out,
                stderr=err,
                env=environment,
                timeout=900,
                preexec_fn=limit_output,
            )
        if stdout.stat().st_size > MAX_OUTPUT or stderr.stat().st_size > MAX_OUTPUT:
            raise ValueError("output bound")
        commands.append(
            {
                "argv": argv,
                "exit": result.returncode,
                "stdin_sha256": hashlib.sha256(stdin).hexdigest() if stdin else None,
                "stdout": stdout.name,
                "stdout_sha256": sha(stdout),
                "stderr": stderr.name,
                "stderr_sha256": sha(stderr),
            }
        )
        if result.returncode:
            raise ValueError("Docker command failed: " + label)
        return stdout.read_bytes()

    error, image, bindings, result = None, None, [], None
    bound_files = {}
    baked_repair_inputs = {}
    try:
        bindings = index_bindings(args.index_record, args.platform, tool["sha256"])
        if args.baked:
            recipe = args.recipe.resolve(strict=True)
            if not recipe.is_relative_to(ROOT):
                raise ValueError("recipe must be in the reviewed source tree")
            recipe_name = str(recipe.relative_to(ROOT))
            reviewed = read_json(args.reviewed_source)
            if reviewed.get(recipe_name, {}).get("sha256") != sha(recipe):
                raise ValueError("recipe is not the externally reviewed Dockerfile")
            required = recipe_references(recipe.read_text(), args.shell_base_index)
            captured = {row["reference"].rsplit("@", 1)[1] for row in bindings}
            if not required or not required.issubset(captured):
                raise ValueError(
                    "primary index evidence does not cover every pinned recipe FROM"
                )
        for number, path in enumerate(args.index_record):
            document = read_json(path)
            directory = output / ("index-" + str(number))
            directory.mkdir(mode=0o700)
            for name in [
                "index-proof.json",
                document["index_file"],
                document["platform_file"],
            ]:
                origin = path if name == "index-proof.json" else path.parent / name
                (directory / name).write_bytes(origin.read_bytes())
        for path in [args.recipe, args.build_receipt, *args.index_record]:
            if path:
                bound_files[str(path)] = sha(path)
        run("docker-version", [str(docker), "version", "--format", "{{json .}}"])
        inspected = json.loads(
            run("image-inspect", [str(docker), "image", "inspect", args.image])
        )
        if len(inspected) != 1:
            raise ValueError("ambiguous image")
        image = inspected[0]
        if image["Os"] + "/" + image[
            "Architecture"
        ] != args.platform or not re.fullmatch(r"sha256:[0-9a-f]{64}", image["Id"]):
            raise ValueError("actual image identity/platform mismatch")
        common = [
            str(docker),
            "run",
            "--rm",
            "--pull",
            "never",
            "--network",
            "none",
            "--read-only",
            "--user",
            "0",
            "--platform",
            args.platform,
            "--entrypoint",
            "python3",
        ]
        if args.filesystem or args.legal_files:
            collector = (
                "filesystem-inventory.py"
                if args.filesystem
                else "image-legal-inventory.py"
            )
            result = json.loads(
                run(
                    "inventory",
                    common + ["-i", image["Id"], "-I", "-S", "/dev/stdin"],
                    (HERE / collector).read_bytes(),
                )
            )
            if not result.get("entries" if args.filesystem else "files"):
                raise ValueError("empty public inventory")
        else:
            argv = list(common)
            if args.supporting_base_bind:
                if "," in str(HERE):
                    raise ValueError("mount delimiter in bundle path")
                argv += [
                    "--mount",
                    f"type=bind,src={HERE / 'collected'},dst=/usr/local/share/licenses/marsh-dhi,readonly",
                ]
            argv += [
                image["Id"],
                "-I",
                "-S",
                "/usr/local/share/licenses/marsh-dhi/verify.py",
                "--expect-agent",
                args.expect_agent,
            ]
            if args.copied_agents:
                argv += ["--copied-agents"]
            if args.profile:
                argv += ["--profile", args.profile, "--version", args.version]
            if args.adapter:
                argv += ["--adapter", args.adapter]
            result = json.loads(run("verification", argv))
            if (
                result.get("schema") != "marsh.dhi-notices-verification/v2"
                or result["verifier_sha256"] != expected["verify.py"]["sha256"]
                or result["bundle_tree_sha256"] != expected_hash
            ):
                raise ValueError(
                    "in-container verifier/bundle does not match externally reviewed tree"
                )
            if args.baked:
                saved = json.loads(
                    run(
                        "baked-receipts",
                        common + ["-i", image["Id"], "-I", "-S", "/dev/stdin"],
                        READ_BAKED,
                    )
                )
                name = (
                    "base-image-verification.json"
                    if result["stage"] == "base"
                    else "image-verification.json"
                )
                if "marsh-dhi/" + name not in saved:
                    raise ValueError("required baked receipt missing: marsh-dhi/" + name)
                row = saved["marsh-dhi/" + name]
                data = base64.b64decode(row["base64"], validate=True)
                if hashlib.sha256(data).hexdigest() != row["sha256"]:
                    raise ValueError("baked receipt output changed")
                baked = json.loads(data)
                for key in [
                    "schema",
                    "stage",
                    "profile",
                    "selected_version",
                    "scope_kind",
                    "verifier_sha256",
                    "bundle_tree_sha256",
                    "adapter_scope",
                ]:
                    if baked.get(key) != result.get(key):
                        raise ValueError("baked receipt is stale/wrong scope: " + key)
                if "marsh-image-repair/repair-receipt.json" not in saved:
                    raise ValueError("required baked receipt missing: marsh-image-repair/repair-receipt.json")
                repair_row = saved["marsh-image-repair/repair-receipt.json"]
                repair_data = base64.b64decode(repair_row["base64"], validate=True)
                if hashlib.sha256(repair_data).hexdigest() != repair_row["sha256"]:
                    raise ValueError("repair receipt output changed")
                repair = json.loads(repair_data)
                if (
                    repair["repair_script_sha256"]
                    != reviewed["packaging/image-repair/repair.py"]["sha256"]
                    or repair["manifest_sha256"]
                    != reviewed["packaging/image-repair/artifacts.json"]["sha256"]
                ):
                    raise ValueError(
                        "repair receipt is not from reviewed repair inputs"
                    )
                base_name = "marsh-image-repair/base-images.json"
                if base_name not in saved:
                    raise ValueError("required baked repair input missing: " + base_name)
                base_row = saved[base_name]
                base_data = base64.b64decode(base_row["base64"], validate=True)
                base_hash = hashlib.sha256(base_data).hexdigest()
                if base_hash != base_row["sha256"]:
                    raise ValueError("baked base-images output changed")
                if base_hash != reviewed["packaging/image-repair/base-images.json"]["sha256"]:
                    raise ValueError("baked base-images is not from reviewed repair inputs")
                baked_repair_inputs[base_name] = {"sha256": base_hash, "size": len(base_data)}
            if args.check_codex_selection:
                command = (
                    'command -v codex; test "$(command -v codex)" = /usr/local/bin/codex; actual=$(codex --version); printf "%s\\n" "$actual"; test "$actual" = "codex-cli '
                    + args.version
                    + '"'
                )
                run(
                    "selected-codex-version",
                    [
                        str(docker),
                        "run",
                        "--rm",
                        "--pull",
                        "never",
                        "--network",
                        "none",
                        "--read-only",
                        "--platform",
                        args.platform,
                        "--entrypoint",
                        "/bin/sh",
                        image["Id"],
                        "-ec",
                        command,
                    ],
                )
        after_image = json.loads(
            run("image-inspect-after", [str(docker), "image", "inspect", image["Id"]])
        )
        if len(after_image) != 1 or any(
            after_image[0][key] != image[key] for key in ["Id", "Os", "Architecture"]
        ):
            raise ValueError("image identity changed")
    except Exception as exception:
        error = str(exception)
    after = source_fence()
    try:
        if index_bindings(args.index_record, args.platform, tool["sha256"]) != bindings:
            error = "primary index evidence changed during host proof"
    except Exception as exception:
        error = str(exception)
    if (
        before != after
        or sha(docker) != tool["sha256"]
        or sha(python) != driver["python_sha256"]
        or sha(args.reviewed_source) != reviewed_sha
        or any(sha(Path(p)) != h for p, h in bound_files.items())
    ):
        error = "source/tool/evidence binding changed"
    proof = {
        "schema": "marsh.canonical-host-proof/v2",
        "requested_image": args.image,
        "actual_image": image,
        "actual_image_identity": {
            "id": image.get("Id") if image else None,
            "descriptor_media_type": image.get("Descriptor", {}).get("mediaType") if image else None,
            "scope": "Runtime-reported image ID; classify only from an observed descriptor media type. IDs can name an index, platform manifest or config. No layer ancestry inferred.",
        },
        "platform": args.platform,
        "tool": tool,
        "driver": driver,
        "commands": commands,
        "source_before": before,
        "source_after": after,
        "reviewed_source_sha256": reviewed_sha,
        "expected_bundle_tree_sha256": expected_hash,
        "raw_index_bindings": bindings,
        "bound_files": bound_files,
        "baked_repair_inputs": baked_repair_inputs,
        "baked": args.baked,
        "verification_scope": result.get("scope_kind") if result else None,
        "agent_version_query_executed": args.check_codex_selection,
        "error": error,
        "passed": error is None,
        "scope": "Baked mode uses no source mount and externally binds fresh/baked verifier and bundle hashes. Base mode is supporting only. Repair receipt source hashes are checked, not all rewritten tree bytes; build/candidate validation is separate. Neither mode is whole-image runtime/legal approval.",
    }
    (output / "proof.json").write_text(json.dumps(proof, indent=2) + "\n")
    print(
        json.dumps(
            {
                "proof": str(output / "proof.json"),
                "sha256": sha(output / "proof.json"),
                "passed": error is None,
            }
        )
    )
    return int(error is not None)


if __name__ == "__main__":
    raise SystemExit(main())
