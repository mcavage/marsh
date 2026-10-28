#!/usr/bin/env python3
"""Bundle original dependency license files into the installed notices text."""
import argparse
import json
from pathlib import Path, PurePosixPath
import subprocess
import tarfile

NAMES = ("LICENSE", "LICENCE", "COPYING", "COPYRIGHT", "NOTICE", "UNLICENSE")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cargo", default="cargo")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    metadata = json.loads(subprocess.check_output(
        [args.cargo, "metadata", "--locked", "--format-version", "1"], cwd=root))
    texts = {}
    for package in metadata["packages"]:
        directory = Path(package["manifest_path"]).parent
        paths = {path for path in directory.rglob("*")
                 if path.is_file() and path.name.upper().startswith(NAMES)}
        if package.get("license_file"):
            paths.add(directory / package["license_file"])
        for path in sorted(paths):
            if path.is_file():
                name = path.relative_to(directory) if path.is_relative_to(directory) else path.name
                texts.setdefault(path.read_bytes(), f"{package['name']}: {name}")
        # Native crates may bundle upstream source archives. Read original
        # notice members directly without extracting paths or checking a
        # hand-maintained component/version inventory.
        for archive in sorted(directory.glob("*.tar*")):
            if not tarfile.is_tarfile(archive):
                continue
            with tarfile.open(archive) as source:
                for member in source:
                    if member.isfile() and PurePosixPath(member.name).name.upper().startswith(NAMES):
                        with source.extractfile(member) as notice:
                            texts.setdefault(notice.read(), f"{package['name']}: {member.name}")
    brush = root / "vendor/brush/LICENSE"
    texts.setdefault(brush.read_bytes(), "Brush LICENSE")
    # Retain upstream texts supplied for crates which omit them from their
    # package, including embedded native dependencies. No version ledger gates
    # whether a newer package may build.
    for path in sorted((root / "packaging/notice-overrides").rglob("*")):
        if path.is_file():
            texts.setdefault(path.read_bytes(), path.name)
    args.output.mkdir(parents=True, exist_ok=True)
    with (args.output / "THIRD-PARTY-NOTICES.txt").open("wb") as output:
        output.write(b"marsh third-party license and notice files\n\n")
        for data, name in texts.items():
            output.write(("=" * 72 + f"\n{name}\n\n").encode())
            output.write(data)
            output.write(b"\n\n")
    print(f"Bundled {len(texts)} original notice texts in {args.output}")


if __name__ == "__main__":
    main()
