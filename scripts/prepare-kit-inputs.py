#!/usr/bin/env python3
"""Prepare one atomic, byte-bound input document for native Kit publication.

No Docker effects. Bundled canonical trees are expanded by the publisher, never
repeated as thousands of absolute file paths. Custom preparers may use map_tree/seal_prepared.
"""
from __future__ import annotations

import argparse
import importlib.util
import os
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("marsh_kit_publisher", ROOT / "scripts/publish-kits.py")
assert SPEC and SPEC.loader
PUBLISHER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PUBLISHER)
IMMUTABLE = re.compile(r"[a-z0-9][a-z0-9._:/-]*@sha256:[0-9a-f]{64}\Z")
BUNDLED = PUBLISHER.BUNDLED
FIXTURE = PUBLISHER.FIXTURE


def map_tree(source: Path, destination: str) -> tuple[dict, dict]:
    """Compatible v1 API: (destination->absolute origin, relative->hash/mode/size)."""
    source = Path(os.path.abspath(source))
    PUBLISHER.parts(destination)
    inventory = PUBLISHER.tree_inventory(source)["files"]
    if not inventory:
        raise ValueError(f"canonical build input is empty: {source}")
    return {destination + "/" + relative: str(source / relative) for relative in inventory}, inventory


def verified_shell_image(path: Path, *, source_tree: Path = ROOT,
                         build_receipt: Path | None = None) -> tuple[str, list[Path]]:
    """Stable return shape; image-only v1 consistency is no longer enough.

    The explicit host receipt is mandatory. image_observations owns the verifier;
    consumers must repeat its semantic checks before effects and after pushes.
    """
    return PUBLISHER.verified_shell_image(path, source_tree=source_tree, build_receipt=build_receipt)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-tree", type=Path, default=ROOT, help="canonical absolute candidate/Kit worktree root")
    parser.add_argument("--commands", type=Path, help="defaults to source-tree/packaging/commands.json")
    parser.add_argument("--extra-inputs", type=Path, help="extra args/files outside reserved canonical namespaces")
    parser.add_argument("--shell-image", type=Path, help="exact observed package shell-image; requires --shell-build-receipt")
    parser.add_argument("--shell-build-receipt", type=Path, help="host-private observed full build receipt outside source/exports")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if bool(args.shell_image) != bool(args.shell_build_receipt):
        parser.error("--shell-image and --shell-build-receipt must be supplied together")
    root = PUBLISHER.canonical_directory(args.source_tree)
    args.commands = args.commands or root / "packaging/commands.json"
    commands = PUBLISHER.read_commands(args.commands)
    selected = PUBLISHER.selected_sources(root, commands)
    output = PUBLISHER.check_output(args.output, set(selected.values()))
    PUBLISHER.parts(output.name)
    bindings = [Path(__file__)]
    if args.extra_inputs:
        bindings.append(args.extra_inputs)
    before = PUBLISHER.bindings_record([args.commands, *bindings])
    extra = PUBLISHER.read_json(args.extra_inputs) if args.extra_inputs else {}
    configs = PUBLISHER.configs_by_source(root, set(selected.values()), extra)
    inputs = {str(source.relative_to(root)): configs.get(source, {"args": {}, "files": {}})
              for source in set(selected.values())}
    requests = []
    if args.shell_image:
        image, shell_bindings = verified_shell_image(args.shell_image, source_tree=root,
                                                     build_receipt=args.shell_build_receipt)
        shell_before = PUBLISHER.bindings_record(shell_bindings)
        if FIXTURE not in inputs:
            raise ValueError("--shell-image requires the selected fixture Kit")
        arguments = inputs[FIXTURE]["args"]
        if "shellImage" in arguments and arguments["shellImage"] != image:
            raise ValueError("extra inputs cannot replace the prepared fixture shell image")
        arguments["shellImage"] = image
        bindings.extend(shell_bindings)
        before.update(shell_before)
        requests.append({"source": FIXTURE, "image_file": str(args.shell_image),
                         "source_tree": str(root), "build_receipt": str(args.shell_build_receipt)})
    document = PUBLISHER.seal_prepared(root, args.commands, inputs, bindings, shell_images=requests)
    if PUBLISHER.bindings_record([Path(path) for path in before]) != before:
        raise ValueError("preparation source/configuration changed")
    PUBLISHER.check_output_inputs(output, set(selected.values()), document["receipt"]["bindings"], requests,
                                  inputs=inputs, canonical=document["receipt"]["state"]["canonical"])
    if len(PUBLISHER.json_bytes(document)) > PUBLISHER.JSON_LIMIT:
        raise ValueError("prepared input document exceeds 4 MiB limit")
    PUBLISHER.check_output(output, set(selected.values()), create=True)
    with PUBLISHER.directory(output.parent) as parent:
        PUBLISHER.verify_output_parent(output.parent, parent)
        PUBLISHER.publish_registry(parent, output.name, document)
    print(f"Prepared byte-bound Kit input document: {output}")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, UnicodeError, TypeError, KeyError, RecursionError,
            subprocess.SubprocessError) as error:
        raise SystemExit(f"prepare-kit-inputs: {error}") from error
