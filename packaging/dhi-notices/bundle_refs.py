"""The texts the canonical bundle's indexes reference, as verify.py reads them.

refresh-pins uses this to delete texts no index references after a record is
retired; verify.py itself fails on any unreferenced or missing text.
"""

import json
from pathlib import Path


def referenced(bundle: Path) -> set:
    def load(name):
        return json.loads((bundle / name).read_text())

    refs = []
    for row in load("installed-package-notices.json")["packages"]:
        refs += row["notices"] + row["archive_notices"] + row["upstream_notices"]
    for row in load("go-module-notices.json")["modules"]:
        refs += row["notices"]
    refs += load("upstream-license-supplements.json")
    for name in ("image-primary-notices.json", "main-component-notices.json",
                 "public-component-notices.json", "native-build-notices.json"):
        for row in load(name):
            refs += row["notices"] + row["required_notices"]
    for name in ("rust-notices.json", "selected-rust-notices.json"):
        for collection in load(name)["collections"]:
            refs += collection["notices"]
    primary = load("primary-source-notices.json")
    for collection in primary["collections"]:
        refs += collection["notices"]
    refs += primary["required_runtime_notices"]
    for adapter in load("adapter-payloads.json")["adapters"].values():
        for selected in adapter["architectures"].values():
            refs += selected["required_notices"]
            for package in selected["packages"]:
                refs += package["notices"]
    for row in load("agent-notices.json")["notices"]:
        refs.append({"file": "provider-notices/" + row["file"]})
    for kit in load("derived-profiles.json").values():
        for version in kit.values():
            for profile in version.values():
                refs += profile["required_notices"]
    return {row["file"] for row in refs}


def prune(bundle: Path) -> list:
    """Delete unreferenced texts; return what was removed. Missing ones raise."""
    wanted = referenced(bundle)
    removed = []
    for directory in ("texts", "provider-notices"):
        for path in sorted((bundle / directory).iterdir()):
            if f"{directory}/{path.name}" not in wanted:
                path.unlink()
                removed.append(f"{directory}/{path.name}")
    missing = sorted(name for name in wanted if not (bundle / name).is_file())
    if missing:
        raise ValueError("indexed notice texts are missing: " + ", ".join(missing[:10]))
    return removed
