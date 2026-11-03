#!/usr/bin/env python3
"""Snapshot https://models.dev/api.json into crates/marsh-daemon/data/models-dev.json.

marsh classifies a model it has no curated pattern for (peer review,
docs/peer-review.md) by the output price models.dev lists for it. marsh never
fetches models.dev at run time: this script, run by the scheduled
`models-dev` workflow (or by hand), writes a trimmed copy that is compiled in.

Kept per model: provider, model id, family, vendor (the model's maker,
canonicalized with data/vendor-aliases.json; "" when unknown), output price
(USD per million tokens), open weights, release date (picks the newest model
of a family). Only chat models that call tools and list an output price are kept,
one row per (vendor, model name).

    python3 scripts/update-models-dev.py              # fetch and write
    python3 scripts/update-models-dev.py --input api.json
"""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
import urllib.request

URL = "https://models.dev/api.json"
OUTPUT = pathlib.Path(__file__).resolve().parent.parent / "crates/marsh-daemon/data/models-dev.json"


ALIASES = pathlib.Path(__file__).resolve().parent.parent / "crates/marsh-daemon/data/vendor-aliases.json"


def aliases() -> dict[str, str]:
    return json.loads(ALIASES.read_text())["aliases"]


def canonical(name: str, table: dict[str, str]) -> str:
    name = name.strip().lower().lstrip("~")
    return table.get(name, name)


def name_key(model_id: str) -> str:
    """A model's name across hosts: the last path segment, without a host
    suffix (`@eu`, `:free`), with `.` and `-` treated alike."""
    name = model_id.rsplit("/", 1)[-1].lower()
    for mark in "@:":
        name = name.split(mark, 1)[0]
    return name.replace(".", "-")


def trim(api: dict) -> dict:
    """Chat models that call tools and list an output price, each with its
    vendor: the model's maker, never the host that lists it. The maker is,
    in order: `canonical_model_id`'s prefix; the id's own prefix when that
    names a known maker; the provider when it is a known maker (its own
    listing); the one known maker of the same model name on other hosts;
    the one known maker of its family. Otherwise the vendor is "" (unknown:
    marsh will not pair it without an explicit vendor). Maker spellings are
    canonicalized (vendor-aliases.json). One row per (vendor, model name):
    the vendor's own listing when there is one."""
    table = aliases()
    listed = []
    for provider_id in sorted(api):
        for model_id, model in sorted(api[provider_id].get("models", {}).items()):
            cost = (model.get("cost") or {}).get("output")
            output = (model.get("modalities") or {}).get("output") or []
            if not isinstance(cost, (int, float)) or "text" not in output or not model.get("tool_call"):
                continue
            listed.append((provider_id, model_id, model, round(float(cost), 4)))
    makers = {canonical(model["canonical_model_id"].split("/", 1)[0], table)
              for _, _, model, _ in listed if "/" in (model.get("canonical_model_id") or "")}
    makers |= set(table.values())

    def direct(provider_id: str, model_id: str, model: dict) -> str:
        named = model.get("canonical_model_id") or ""
        if "/" in named:
            return canonical(named.split("/", 1)[0], table)
        if "/" in model_id:
            prefix = canonical(model_id.split("/", 1)[0], table)
            if prefix in makers:
                return prefix
        provider = canonical(provider_id, table)
        return provider if provider in makers else ""

    vendors = [direct(provider_id, model_id, model) for provider_id, model_id, model, _ in listed]
    by_name: dict[str, set[str]] = {}
    by_family: dict[str, set[str]] = {}
    for (_, model_id, model, _), vendor in zip(listed, vendors):
        if vendor:
            by_name.setdefault(name_key(model_id), set()).add(vendor)
            if model.get("family"):
                by_family.setdefault(model["family"].lower(), set()).add(vendor)
    kept: dict[tuple[str, str], list] = {}
    for (provider_id, model_id, model, cost), vendor in zip(listed, vendors):
        if not vendor:
            for found in (by_name.get(name_key(model_id)), by_family.get((model.get("family") or "").lower())):
                if found and len(found) == 1:
                    vendor = next(iter(found))
                    break
        row = [provider_id, model_id, model.get("family") or "", vendor, cost,
               bool(model.get("open_weights")), model.get("release_date") or ""]
        key = (vendor or f"?{provider_id}", name_key(model_id))
        own = canonical(provider_id, table) == vendor
        if key not in kept or (own and canonical(kept[key][0], table) != vendor):
            kept[key] = row
    rows = sorted(kept.values(), key=lambda row: (row[0], row[1]))
    return {
        "source": URL,
        "fields": ["provider", "id", "family", "vendor", "output", "open_weights", "released"],
        "models": rows,
    }


def render(snapshot: dict) -> str:
    # One model per line: small diffs in the bot's pull requests.
    lines = [
        "{",
        f'"source": {json.dumps(snapshot["source"])},',
        f'"fields": {json.dumps(snapshot["fields"])},',
        '"models": [',
    ]
    rows = [json.dumps(row, ensure_ascii=False, separators=(",", ":")) for row in snapshot["models"]]
    lines.append(",\n".join(rows))
    lines += ["]", "}"]
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--input", type=pathlib.Path, help="read this api.json instead of fetching")
    parser.add_argument("--output", type=pathlib.Path, default=OUTPUT)
    args = parser.parse_args()
    if args.input:
        api = json.loads(args.input.read_text())
    else:
        request = urllib.request.Request(URL, headers={"User-Agent": "marsh-models-dev-snapshot"})
        with urllib.request.urlopen(request, timeout=60) as response:
            api = json.load(response)
    snapshot = trim(api)
    if len(snapshot["models"]) < 100:
        print(f"refusing a snapshot of {len(snapshot['models'])} models", file=sys.stderr)
        return 1
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(render(snapshot))
    print(f"{args.output}: {len(snapshot['models'])} models")
    return 0


if __name__ == "__main__":
    sys.exit(main())
