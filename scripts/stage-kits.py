#!/usr/bin/env python3
"""Stage the registry's source Kits into an install's kits/ directory.

A packaged source Kit is its ignore-aware Git files plus the canonical inputs
publish-kits.py adds (repaired-image inputs, DHI notices, the npm notice
collector). Stock SBX builds the staged directory as is, so local dev installs
use the same expansion as publication. The replacement is atomic.
"""
from __future__ import annotations

import argparse
import importlib.util
import os
from pathlib import Path
import shutil
import tempfile

ROOT = Path(__file__).resolve().parents[1]
_SPEC = importlib.util.spec_from_file_location("marsh_publish_kits", ROOT / "scripts/publish-kits.py")
KITS = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(KITS)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--commands", type=Path, required=True, help="source registry (Kit paths relative to the checkout)")
    parser.add_argument("--output", type=Path, required=True, help="guest artifacts directory; writes OUTPUT/kits")
    args = parser.parse_args()
    selected = set(KITS.selected_sources(ROOT, KITS.read_commands(args.commands)).values())
    expanded, _ = KITS.canonical_inputs(ROOT, selected, {})
    args.output.mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=".kits-", dir=args.output))
    try:
        budget = [KITS.STAGE_LIMIT]
        for source in sorted(selected):
            destination = staging / source.relative_to(ROOT / "kits")
            for relative in KITS.git_files(source):
                path = source / relative
                if path.exists() or path.is_symlink():  # tracked deletions stay deleted
                    KITS.copy_regular(path, destination / relative, budget)
            for relative, origin in expanded[source]["files"].items():
                KITS.copy_regular(Path(origin), destination / relative, budget)
        staging.chmod(0o755)
        for path in staging.rglob("*"):
            if path.is_dir():
                path.chmod(0o755)
        target = args.output / "kits"
        retired = None
        if target.exists():
            retired = Path(tempfile.mkdtemp(prefix=".kits-old-", dir=args.output))
            os.rename(target, retired / "kits")
        os.rename(staging, target)
        if retired:
            shutil.rmtree(retired)
    finally:
        if staging.exists():
            shutil.rmtree(staging)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError) as error:
        raise SystemExit(f"stage-kits: {error}") from error
