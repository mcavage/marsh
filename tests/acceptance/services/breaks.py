"""A.6: break modes, holds and faults; the generated registry and the three-pass protocol.

Pure functions over the catalogue; services_uat.py applies them. Mode classes:

- **defect** (everything else): the behavior a scenario must fail at its named check without;
- **hold** (`hold-<point>`): a barrier the harness releases (H-14); never a defect;
- **fault** (`fault-*`): a simulated real-world failure; its defect mode removes the recovery, never the fault.

Three passes per cell `(scenario, variant, defect)`: (1) normal build; (2) breaks build with only the holds and
faults the cell needs; (3) the same plus the defect. Pass 3 verifies the break only if the scenario fails at the
one check the cell names. A setup failure, an unexpected error, a timeout or a failure at another check is an
**invalid negative control**; a pass is **not detected**. Neither is ever reported as verified.
"""

from __future__ import annotations

import json
from typing import Any, Iterator

from catalogue import CATALOGUE, Break, Entry

HARNESS_BREAKS = {b.mode for entry in CATALOGUE for b in entry.breaks if b.kind in ("harness", "fixture")}
VERIFIABLE_HERE = ("marsh", "harness", "fixture")  # narwhal and patch cells are verified by their own runners


def normalize_modes(value: str | list[str] | None) -> list[str]:
    parts = value if isinstance(value, list) else (value or "").split(",")
    return sorted({part.strip() for part in parts if part.strip()})


# Controls that ride along with a defect without being one (PERF-10's `slow-freeze-long` is the +3 s freeze the
# measurement must see; in pass 2 it is on alone).
COMPANIONS = {p for entry in CATALOGUE for b in entry.breaks for p in b.prerequisites
              if not p.startswith(("hold-", "fault-"))}


def classify(modes: list[str]) -> dict[str, list[str]]:
    return {"holds": [m for m in modes if m.startswith("hold-")],
            "faults": [m for m in modes if m.startswith("fault-")],
            "companions": [m for m in modes if m in COMPANIONS],
            "defects": [m for m in modes if not m.startswith(("hold-", "fault-")) and m not in COMPANIONS]}


def marsh_modes(modes: list[str]) -> list[str]:
    """What the daemon's environment carries: defects of the product, holds and faults (not harness or fixture breaks)."""
    return [m for m in modes if m not in HARNESS_BREAKS]


def cells(entry: Entry, defect: str, variant: str | None = None) -> list[Break]:
    return [b for b in entry.breaks if b.mode == defect and (variant is None or b.variant in (None, variant))]


def expected_checks(entry: Entry, defect: str | None, variant: str | None = None) -> set[str]:
    return {b.check for b in cells(entry, defect, variant)} if defect else set()


def prerequisites(entry: Entry, defect: str, variant: str | None = None) -> list[str]:
    found: set[str] = set()
    for cell in cells(entry, defect, variant):
        found |= set(cell.prerequisites)
    return sorted(found)


def infer_defect(modes: list[str]) -> str | None:
    defects = classify(modes)["defects"]
    return defects[0] if len(defects) == 1 else None


def _exact(document: dict[str, Any], field: str, requested: list[str], who: str) -> str | None:
    """FIELD must be present and exactly the requested modes (a list of strings, no duplicates, same set); with
    nothing requested it must be absent altogether (a present null or empty list is not absent)."""
    if requested:
        if field not in document:
            return f"refusing a break run: {who} reports no `{field}` field, requested {requested!r}"
        value = document[field]
        if (not isinstance(value, list) or not all(isinstance(m, str) for m in value)
                or len(set(value)) != len(value) or sorted(value) != requested):
            return f"refusing a break run: {who} reports {field}={value!r}, requested {requested!r}"
    elif field in document:
        return f"refusing a run with no modes: {who} reports {field}={document[field]!r}"
    return None


def guard_error(status: dict[str, Any], requested: list[str]) -> str | None:
    """A.6 rule 1 on the daemon: `marsh status --json` carries `test_breaks` exactly when modes were requested, and
    then exactly those modes. Reports why the run must refuse to start (a break run on an install that does not know
    the mode, or launched without it; a normal run that finds the field)."""
    problem = _exact(status, "test_breaks", requested, "`marsh status --json`")
    return None if problem is None else problem + (
        " (is --prefix/--breaks-prefix a `make dev DEV_FEATURES=test-breaks` install that knows this mode, and was "
        "its daemon launched with `--test-breaks-from-env`?)" if requested else "")


def agent_break_error(project: dict[str, Any], requested: list[str]) -> str | None:
    """The same rule for one `marsh services status --json` project: `test_breaks_agent` (from wave 1)."""
    return _exact(project, "test_breaks_agent", requested, "the services agent")


def verdict(defect: str | None, requested: list[str], outcome: str, failed_check: str | None,
            expected: set[str], *, breaks_build: bool) -> str:
    """The evidence verdict of one run."""
    if defect:
        if outcome == "passed":
            return "not-detected"
        return "verified" if outcome == "failed" and failed_check in expected else "invalid-negative-control"
    if requested or breaks_build:
        return "pass-2-ok" if outcome == "passed" else "baseline-failed"
    return "normal"


def list_cells() -> Iterator[dict[str, Any]]:
    """Every `(scenario, variant, mode)` cell of the plan's §A.6 table, with what a harness needs to run it."""
    for entry in CATALOGUE:
        for cell in entry.breaks:
            requested = sorted({*normalize_modes(cell.mode), *cell.prerequisites}) if cell.kind == "marsh" else []
            passes = {"marsh": [1, 2, 3], "harness": [1, 2, 3], "fixture": [1, 2, 3]}.get(cell.kind, [])
            yield {"scenario": entry.id, "variant": cell.variant, "mode": cell.mode, "kind": cell.kind,
                   "failed_check": cell.check, "prerequisites": list(cell.prerequisites), "passes": passes,
                   "expected_modes": {"daemon": requested, "agent": requested if cell.kind == "marsh" else []},
                   "owner": entry.owner}


def list_breaks_text() -> str:
    return "".join(json.dumps(cell, sort_keys=True) + "\n" for cell in list_cells())


def plan_commands(entry: Entry, base: list[str], script: str, python: str) -> list[dict[str, Any]]:
    """The child invocations of the three-pass protocol for ENTRY, in order: pass 1 once, then for each verifiable
    cell pass 2 (breaks build, explicit launch, only the cell's holds/faults/companions; deduplicated) and pass 3 (the
    same plus the defect). Harness and fixture cells run all three passes too. Each item: pass, label, cell,
    argv."""
    plan: list[dict[str, Any]] = [{"pass": 1, "label": "normal build", "cell": None,
                                   "argv": [python, script, *base, "--tier", "full", "--only", entry.id]}]
    done: set[tuple[str, ...]] = set()
    for cell in entry.breaks:
        if cell.kind not in VERIFIABLE_HERE:
            continue
        label = cell.mode + (f"[{cell.variant}]" if cell.variant else "")
        variant = ["--variant", cell.variant] if cell.variant else []
        key = (cell.variant or "", *sorted(cell.prerequisites))
        if key not in done:
            done.add(key)
            extra = ["--break", ",".join(sorted(cell.prerequisites))] if cell.prerequisites else []
            plan.append({"pass": 2, "label": f"pass 2 ({label})", "cell": cell,
                         "argv": [python, script, *base, "--tier", "break", "--build", "breaks", *extra, *variant,
                                  "--only", entry.id]})
        modes = sorted({cell.mode, *cell.prerequisites}) if cell.kind == "marsh" else [cell.mode]
        plan.append({"pass": 3, "label": f"pass 3 ({label} -> {cell.check})", "cell": cell,
                     "argv": [python, script, *base, "--tier", "break", "--break", ",".join(modes),
                              "--break-defect", cell.mode, *variant, "--only", entry.id]})
    return plan
