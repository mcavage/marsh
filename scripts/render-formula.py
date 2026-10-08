#!/usr/bin/env python3
"""Render packaging/homebrew/marsh.rb.in for one release."""
from __future__ import annotations

import argparse
import pathlib
import re

ROOT = pathlib.Path(__file__).resolve().parents[1]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--sha256", required=True)
    parser.add_argument("--repository", default="mcavage/marsh")
    parser.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args()
    version = args.version.removeprefix("v")
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        parser.error("--version must be X.Y.Z")
    if not re.fullmatch(r"[0-9a-f]{64}", args.sha256):
        parser.error("--sha256 must be 64 lowercase hex digits")
    if not re.fullmatch(r"[A-Za-z0-9-]+/[A-Za-z0-9._-]+", args.repository):
        parser.error("--repository must be OWNER/NAME")
    text = (ROOT / "packaging/homebrew/marsh.rb.in").read_text()
    for key, value in {"VERSION": version, "SHA256": args.sha256, "REPOSITORY": args.repository}.items():
        text = text.replace(f"@{key}@", value)
    # The caveats must print the same sbx install command as marsh's startup error.
    hint = re.search(r'SBX_INSTALL_HINT: &str =\s*"([^"]+)"', (ROOT / "crates/marsh/src/client.rs").read_text())
    if not hint or hint.group(1) not in text:
        raise SystemExit("render-formula: caveats must contain SBX_INSTALL_HINT from crates/marsh/src/client.rs")
    if re.search(r"@[A-Z0-9_]+@", text):
        raise SystemExit("render-formula: unreplaced placeholder")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(text)
    print(args.output)


if __name__ == "__main__":
    main()
