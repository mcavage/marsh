#!/usr/bin/env python3
"""Generate CONTRACT.md's scenario index from the suites' own scenario tables.

`--write` rewrites the generated section; running without options exits 1 when
CONTRACT.md disagrees with the code. tests/acceptance/test_contract_index.py
runs the check in CI.
"""
from __future__ import annotations

import argparse
import pathlib
import re
import sys

DIRECTORY = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(DIRECTORY))

CONTRACT = DIRECTORY / "CONTRACT.md"
BEGIN = "<!-- BEGIN GENERATED SCENARIO INDEX -->"
END = "<!-- END GENERATED SCENARIO INDEX -->"


def suites() -> list[tuple[str, str, list[tuple[str, str]]]]:
    """(heading, suite file, [(id, summary)]) for every scenario suite."""
    import run
    from harness import scenario_list
    from processes_uat import LIVE, Processes
    from shells_uat import Shells
    from workspaces_uat import Workspaces

    processes = [(scenario, summary + (" (billed; only with `--live`)" if scenario in LIVE else ""))
                 for scenario, summary in sorted(scenario_list(Processes))]
    return [
        ("Acceptance gate checks", "run.py", [(check, "") for check in run.CHECKS]),
        ("Workspaces", "workspaces_uat.py", sorted(scenario_list(Workspaces))),
        ("Processes", "processes_uat.py", processes),
        ("Shells", "shells_uat.py", sorted(scenario_list(Shells))),
    ]


def render() -> str:
    lines = [BEGIN]
    for heading, script, rows in suites():
        lines += ["", f"### {heading} (`{script}`, {len(rows)})", ""]
        lines += [f"- `{name}`" + (f": {summary}" if summary else "") for name, summary in rows]
    lines += ["", END]
    return "\n".join(lines)


def current(text: str) -> str:
    match = re.search(re.escape(BEGIN) + r".*?" + re.escape(END), text, re.S)
    if match is None:
        raise SystemExit(f"{CONTRACT}: generated scenario index markers are missing")
    return match.group(0)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write", action="store_true", help="rewrite CONTRACT.md's generated section")
    args = parser.parse_args()
    text = CONTRACT.read_text(encoding="utf-8")
    wanted = render()
    if args.write:
        CONTRACT.write_text(text.replace(current(text), wanted), encoding="utf-8")
        return 0
    if current(text) != wanted:
        print(f"{CONTRACT} is stale: run python3 tests/acceptance/contract_index.py --write", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
