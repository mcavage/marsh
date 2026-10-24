#!/usr/bin/env python3
"""License delta between two trees' pinned components, with a review verdict.

    license-delta.py --before DIR [--after DIR] [--json FILE] [--markdown FILE]

Each tree is a checkout (or a `git archive` of one) holding the records
`make refresh-pins` regenerates. Components are every installed OS package of
every image (packaging/dhi-notices/image-packages.json), the Engine's Go
modules, the Rust crates of the shipped Codex releases, the npm packages of
every lock, compiled-in components, and the native agents. Each one's license
families come from license_families.py.

Verdict (exit status):

  0 OK           only new versions of reviewed components, and new components
                 whose license families the tree already has
  2 NEEDS-OWNER  a new license family; a changed license text of a proprietary
                 component (qualification-boundaries.json
                 `proprietary_components`); or a new proprietary or
                 binary-only component

The report lists every added, removed and version-changed component with its
families before and after. It labels evidence for a reviewer; it is not a
license determination.
"""
from __future__ import annotations

import argparse
import fnmatch
import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "packaging/dhi-notices"))
from license_families import declared_families, text_families  # noqa: E402

LOCKS = ("kits/marsh-claude/package-lock.json", "kits/marsh-codex/package-lock.json",
         "kits/marsh-pi/package-lock.json", "packaging/shell/pi/package-lock.json")
VERSION = re.compile(r'^args:\n  version:\n    default: "([^"]+)"', re.M)
# collect-adapter-payloads.py keeps a README next to the legal files so the
# record shows what the package says about itself. Its prose changes with
# releases, so it is not a license text and must not trip the comparison.
DOCUMENTATION = re.compile(r"^readme(\.|$)", re.I)


class Tree:
    def __init__(self, root: pathlib.Path):
        self.root = root
        self.bundle = root / "packaging/dhi-notices/collected"
        self.cache = {}

    def json(self, relative, default=None):
        path = self.root / relative
        if not path.is_file():
            if default is not None:
                return default
            raise ValueError(f"{self.root}: missing {relative}")
        return json.loads(path.read_text())

    def families(self, file):
        if file not in self.cache:
            path = self.bundle / file
            self.cache[file] = text_families(path.read_bytes()) if path.is_file() else set()
        return self.cache[file]


def components(tree: Tree) -> dict:
    found = {}

    def add(kind, name, version, families=(), texts=()):
        row = found.setdefault((kind, name), {"versions": set(), "families": set(), "texts": set()})
        row["versions"].add(version)
        row["families"] |= set(families)
        row["texts"] |= set(texts)

    for rows in tree.json("packaging/dhi-notices/image-packages.json", {"images": {}})["images"].values():
        for row in rows:
            add("os", row["package"], row["version"], row["families"], row["notices"])
    for row in tree.json("packaging/dhi-notices/collected/go-module-notices.json")["modules"]:
        files = [n["file"] for n in row["notices"]]
        add("go", row["path"], row["version"], set().union(*(tree.families(f) for f in files)),
            [n["sha256"] for n in row["notices"]])
    for index in ("rust-notices.json", "selected-rust-notices.json"):
        for collection in tree.json("packaging/dhi-notices/collected/" + index)["collections"]:
            source = tree.json("packaging/dhi-notices/collected/" + collection["source_inventory"])
            rows = source if isinstance(source, list) else source.get("archives", source.get("supplements", []))
            for row in rows:
                files = [n["file"] for n in row["notices"]]
                families = set().union(*(tree.families(f) for f in files)) if files else set()
                for package in row.get("packages", []):
                    add("rust", package["name"], package.get("version", "?"), families,
                        [n["sha256"] for n in row["notices"]])
    adapter_texts = {}
    for adapter in tree.json("packaging/dhi-notices/collected/adapter-payloads.json")["adapters"].values():
        for selected in adapter["architectures"].values():
            for package in selected["packages"]:
                name = package["package"]
                adapter_texts.setdefault(name, set()).update(
                    n["sha256"] for n in package["notices"]
                    if not DOCUMENTATION.match(pathlib.PurePosixPath(n.get("member", "")).name))
    for lock_path in LOCKS:
        lock = tree.json(lock_path, {"packages": {}})
        for path, row in lock["packages"].items():
            if not path or "version" not in row:
                continue
            name = row.get("name") or path.rsplit("node_modules/", 1)[-1]
            add("npm", name, row["version"], declared_families(row.get("license") or "UNDECLARED"),
                adapter_texts.get(name, ()))
    for row in tree.json("packaging/dhi-notices/collected/main-component-notices.json"):
        files = [n["file"] for n in row["notices"]]
        add("component", row["package"], row.get("revision") or row.get("version", "?"),
            set().union(*(tree.families(f) for f in files)), [n["sha256"] for n in row["notices"]])
    profiles = tree.json("packaging/dhi-notices/collected/derived-profiles.json")
    for kit, name in (("claude-kit", "claude-code"), ("codex-kit", "codex")):
        for version, architectures in profiles.get(kit, {}).items():
            for profile in architectures.values():
                files = [n["file"] for n in profile["required_notices"]]
                legal = [f for f in files if re.search(r"(license|copying)", f, re.I)] or files
                add("agent", name, version, set().union(*(tree.families(f) for f in legal)),
                    [n["sha256"] for n in profile["required_notices"] if n["file"] in legal])
    result = {}
    for key, row in found.items():
        families = row["families"] or {"Unclassified"}
        result[key] = {"version": ", ".join(sorted(row["versions"])), "families": sorted(families),
                       "texts": sorted(row["texts"])}
    return result


def proprietary_patterns(tree: Tree):
    boundaries = tree.json("packaging/dhi-notices/collected/qualification-boundaries.json")
    return [(row["kind"], row["name"]) for row in boundaries.get("proprietary_components", [])]


def is_listed(key, patterns):
    return any(key[0] == kind and fnmatch.fnmatchcase(key[1], name) for kind, name in patterns)


def compare(before: dict, after: dict, patterns) -> dict:
    present = set().union(*(set(row["families"]) for row in before.values())) if before else set()
    added = sorted(key for key in after if key not in before)
    removed = sorted(key for key in before if key not in after)
    changed = sorted(key for key in after if key in before and after[key]["version"] != before[key]["version"])
    reasons = []
    for key in added + changed:
        row = after[key]
        label = f"{key[0]} {key[1]} {row['version']}"
        new = sorted(set(row["families"]) - present)
        if new:
            reasons.append(f"new license family {', '.join(new)}: {label}")
        proprietary = is_listed(key, patterns) or "Proprietary" in row["families"]
        if proprietary and key not in before:
            reasons.append(f"new proprietary or binary-only component: {label}")
        elif "Proprietary" in row["families"] and not is_listed(key, patterns) \
                and "Proprietary" not in before[key]["families"]:
            reasons.append(f"component became proprietary: {label}")
    for key in after:
        if key in before and is_listed(key, patterns) and after[key]["texts"] != before[key]["texts"]:
            reasons.append(f"proprietary license text changed: {key[0]} {key[1]} "
                           f"{before[key]['version']} -> {after[key]['version']}")
    text_changes = sorted(key for key in changed if after[key]["texts"] != before[key]["texts"]
                          and before[key]["texts"])
    return {"verdict": "NEEDS-OWNER" if reasons else "OK", "reasons": sorted(set(reasons)),
            "added": [dict(kind=k[0], name=k[1], **pick(after[k])) for k in added],
            "removed": [dict(kind=k[0], name=k[1], **pick(before[k])) for k in removed],
            "changed": [{"kind": k[0], "name": k[1], "before": before[k]["version"],
                         "after": after[k]["version"], "families_before": before[k]["families"],
                         "families_after": after[k]["families"],
                         "license_text_changed": k in text_changes} for k in changed],
            "families": sorted(set().union(*(set(r["families"]) for r in after.values()))) if after else []}


def pick(row):
    return {"version": row["version"], "families": row["families"]}


ROW_LIMIT = 150  # per section: a PR comment holds 65536 characters; the JSON has all


def markdown(report: dict) -> str:
    lines = ["<!-- marsh-refresh-pins -->", "## License delta", "",
             f"**Verdict: {report['verdict']}**", ""]
    for reason in report["reasons"][:ROW_LIMIT]:
        lines.append(f"- {reason}")
    if len(report["reasons"]) > ROW_LIMIT:
        lines.append(f"- ... and {len(report['reasons']) - ROW_LIMIT} more in license-delta.json")
    if report["reasons"]:
        lines.append("")
    counts = {}
    for section in ("added", "removed", "changed"):
        for row in report[section]:
            counts.setdefault(row["kind"], {"added": 0, "removed": 0, "changed": 0})[section] += 1
    if not counts:
        lines.append("No component was added, removed or changed.")
    else:
        lines += ["| kind | added | removed | version changed |", "|---|---|---|---|"]
        for kind in sorted(counts):
            c = counts[kind]
            lines.append(f"| {kind} | {c['added']} | {c['removed']} | {c['changed']} |")
        lines.append("")
        for section, title in (("added", "Added"), ("removed", "Removed"), ("changed", "Version changed")):
            rows = report[section]
            if not rows:
                continue
            hidden = max(0, len(rows) - ROW_LIMIT)
            rows = rows[:ROW_LIMIT]
            lines += [f"<details><summary>{title} ({len(rows)})</summary>", ""]
            if section == "changed":
                lines += ["| kind | name | before | after | families before | families after | license text |",
                          "|---|---|---|---|---|---|---|"]
                for r in rows:
                    lines.append(f"| {r['kind']} | `{r['name']}` | {r['before']} | {r['after']} | "
                                 f"{', '.join(r['families_before'])} | {', '.join(r['families_after'])} | "
                                 f"{'changed' if r['license_text_changed'] else 'same'} |")
            else:
                lines += ["| kind | name | version | families |", "|---|---|---|---|"]
                for r in rows:
                    lines.append(f"| {r['kind']} | `{r['name']}` | {r['version']} | {', '.join(r['families'])} |")
            if hidden:
                lines.append(f"\n... and {hidden} more in license-delta.json.")
            lines += ["", "</details>", ""]
    lines.append(f"License families in the tree: {', '.join(report['families'])}.")
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--before", type=pathlib.Path, required=True)
    parser.add_argument("--after", type=pathlib.Path, default=ROOT)
    parser.add_argument("--json", type=pathlib.Path)
    parser.add_argument("--markdown", type=pathlib.Path)
    args = parser.parse_args()
    before_tree, after_tree = Tree(args.before), Tree(args.after)
    report = compare(components(before_tree), components(after_tree), proprietary_patterns(before_tree)
                     + proprietary_patterns(after_tree))
    text = markdown(report)
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(report, indent=2) + "\n")
    if args.markdown:
        args.markdown.parent.mkdir(parents=True, exist_ok=True)
        args.markdown.write_text(text)
    print(text, end="")
    return 2 if report["verdict"] == "NEEDS-OWNER" else 0


if __name__ == "__main__":
    sys.exit(main())
