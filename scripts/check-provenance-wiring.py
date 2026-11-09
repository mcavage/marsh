#!/usr/bin/env python3
"""Every executable the workspace ships records build provenance.

Each binary target (except the listed non-shipped ones) must live in a crate
with a build script and call `marsh_provenance::embed!` in its source, so the
build, not any code that uses the feature, records `test-breaks`
(crates/marsh-provenance; docs/design/test-breaks.md, "Releases").
"""

import json
import pathlib
import subprocess
import sys

NOT_SHIPPED = {"marsh-acp-fake-agent", "marsh-brush-test-driver", "e3-harness"}

metadata = json.loads(
    subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--no-deps"],
        check=True, capture_output=True, text=True,
    ).stdout
)
problems = []
for package in metadata["packages"]:
    kinds = [target["kind"] for target in package["targets"]]
    has_build_script = ["custom-build"] in kinds
    for target in package["targets"]:
        if target["kind"] != ["bin"] or target["name"] in NOT_SHIPPED:
            continue
        source = pathlib.Path(target["src_path"]).read_text()
        if not has_build_script:
            problems.append(f"{target['name']}: crate {package['name']} has no build.rs")
        if "marsh_provenance::embed!" not in source:
            problems.append(f"{target['name']}: {target['src_path']} does not call marsh_provenance::embed!()")
        else:
            print(f"check-provenance-wiring: ok {target['name']}")
for problem in problems:
    print(f"check-provenance-wiring: {problem}", file=sys.stderr)
sys.exit(1 if problems else 0)
