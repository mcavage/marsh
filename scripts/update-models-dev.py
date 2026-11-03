#!/usr/bin/env python3
"""Snapshot https://models.dev/api.json into crates/marsh-daemon/data/models-dev.json.

marsh classifies a model it has no curated pattern for (peer review,
docs/peer-review.md) by the output price models.dev lists for it. marsh never
fetches models.dev at run time: this script, run by the scheduled
`models-dev` workflow (or by hand), writes a trimmed copy that is compiled in.

Kept per model: provider, model id, family, maker (the `canonical_model_id`
prefix when it names someone other than the provider), output price (USD per
million tokens), open weights, release date (picks the newest model of a
family). Only chat models that call tools and list an output price are kept,
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


def trim(api: dict) -> dict:
    """One row per (vendor, model name): the provider's own listing when it
    has one, else the first host's."""
    kept: dict[tuple[str, str], list] = {}
    for provider_id in sorted(api):
        provider = api[provider_id]
        for model_id in sorted(provider.get("models", {})):
            model = provider["models"][model_id]
            cost = (model.get("cost") or {}).get("output")
            output = (model.get("modalities") or {}).get("output") or []
            if not isinstance(cost, (int, float)) or "text" not in output or not model.get("tool_call"):
                continue
            named = model.get("canonical_model_id") or model_id
            maker = named.split("/", 1)[0].lower() if "/" in named else ""
            vendor = maker or provider_id
            row = [
                provider_id,
                model_id,
                model.get("family") or "",
                "" if maker == provider_id else maker,
                round(float(cost), 4),
                bool(model.get("open_weights")),
                model.get("release_date") or "",
            ]
            key = (vendor, model_id.rsplit("/", 1)[-1].lower())
            if key not in kept or (provider_id == vendor and kept[key][0] != vendor):
                kept[key] = row
    rows = sorted(kept.values(), key=lambda row: (row[0], row[1]))
    return {
        "source": URL,
        "fields": ["provider", "id", "family", "maker", "output", "open_weights", "released"],
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
