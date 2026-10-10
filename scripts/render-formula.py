#!/usr/bin/env python3
"""Render packaging/homebrew/marsh.rb.in for one release (stable or nightly)."""
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
    parser.add_argument("--channel", choices=("stable", "nightly"), default="stable",
                        help="stable renders marsh; nightly renders marsh-nightly")
    parser.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args()
    version = args.version.removeprefix("v")
    stable = args.channel == "stable"
    pattern = r"\d+\.\d+\.\d+" if stable else r"\d+\.\d+\.\d+-nightly\.[0-9A-Za-z.]+"
    if not re.fullmatch(pattern, version):
        parser.error("--version must be X.Y.Z" if stable else "--version must be X.Y.Z-nightly.STAMP")
    if not re.fullmatch(r"[0-9a-f]{64}", args.sha256):
        parser.error("--sha256 must be 64 lowercase hex digits")
    if not re.fullmatch(r"[A-Za-z0-9-]+/[A-Za-z0-9._-]+", args.repository):
        parser.error("--repository must be OWNER/NAME")
    text = (ROOT / "packaging/homebrew/marsh.rb.in").read_text()
    values = {
        "VERSION": version,
        "SHA256": args.sha256,
        "REPOSITORY": args.repository,
        "FORMULA": "marsh" if stable else "marsh-nightly",
        "CLASS": "Marsh" if stable else "MarshNightly",
        "CONFLICTS_WITH": "marsh-nightly" if stable else "marsh",
        "DESC_SUFFIX": "" if stable else " (nightly build)",
        # Homebrew cannot always derive a prerelease version from the URL.
        "VERSION_LINE": "" if stable else f'  version "{version}"\n',
        "CAVEAT_EXTRA": "" if stable else (
            "\n      This is the nightly channel: every green commit on main, no stability promise.\n"
            "      It shares ~/.marsh with a stable install. Stable: brew install mcavage/tap/marsh"),
    }
    for key, value in values.items():
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
