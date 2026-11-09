"""Run records and `make services-e2e-gc`.

Every run writes `runs/<run>.json` beside the VM lock: its pid (display only), its root, the names it intends to
create and, once they exist, their stable IDs (intent first, so a crash between the two is visible), and the policy
rule IDs it recorded. A harness removes only what its own run recorded.

Whether a run is dead is never guessed from a pid or a process identity. A run record is written only by a process
that holds the VM lock, and the kernel releases that lock when the process dies, so a collector that holds the lock
knows that no other run is alive: every record it finds belongs to a finished or crashed run. The collector
therefore removes a resource only when a run record lists it by name AND by the stable ID the resource still has,
and the caller holds the lock (or names the runs it knows are alive in `alive`, as HARNESS-01's decoy check does).
A resource without a record, with an ID that does not match, or owned by a run named alive is left alone and
reported. A record is kept until everything it lists is gone.
"""

from __future__ import annotations

import json
import os
import pathlib
import shutil
import subprocess
from typing import Any

from nfx_host import GENERIC_SANDBOX, is_nfx_name, remove_policy_rules, remove_sandbox
from provenance import stock_vm_inventory


def runs_dir(lock_path: pathlib.Path) -> pathlib.Path:
    return pathlib.Path(lock_path).parent / "runs"


class RunRecord:
    """What one run owns, saved on every change so a crash leaves an accurate record."""

    def __init__(self, directory: pathlib.Path, run: str, tier: str, root: pathlib.Path,
                 ownership_map: pathlib.Path | None = None) -> None:
        self.path = directory / f"{run}.json"
        self.data: dict[str, Any] = {"run": run, "pid": os.getpid(), "tier": tier,
                                     "root": str(root), "ownership_map": str(ownership_map) if ownership_map else None,
                                     "created": {}, "map_vms": {}, "policy_rules": {}}

    def save(self, exclusive: bool = False) -> None:
        """Atomically write the record. EXCLUSIVE refuses to replace an existing record (raises FileExistsError)."""
        self.path.parent.mkdir(parents=True, exist_ok=True)
        staging = self.path.with_suffix(f".tmp{os.getpid()}")
        staging.write_text(json.dumps(self.data, indent=1) + "\n", encoding="utf-8")
        if exclusive:
            try:
                os.link(staging, self.path)  # fails if the record exists, never overwrites it
            finally:
                staging.unlink(missing_ok=True)
        else:
            staging.replace(self.path)

    def intend(self, name: str) -> None:
        """Persist the intent to create NAME before any effect; its ID is recorded by `identify`."""
        self.data["created"].setdefault(name, None)
        self.save()

    def identify(self, name: str, uuid: str) -> None:
        self.data["created"][name] = uuid
        self.save()

    def sync_map(self, document: dict[str, Any] | None) -> None:
        """Copy the scope's ownership map {name: uuid} into the record so a later collector has the stable IDs even
        after the scope's root (and its map) is gone."""
        vms = {}
        for name, entry in ((document or {}).get("vms") or {}).items():
            uuid = entry.get("uuid") if isinstance(entry, dict) else None
            vms[name] = uuid if isinstance(uuid, str) else None
        if vms != self.data["map_vms"]:
            self.data["map_vms"] = vms
            self.save()

    def policy(self, rules: dict[str, dict[str, Any]]) -> None:
        self.data["policy_rules"] = rules
        self.save()

    def remove(self) -> None:
        self.path.unlink(missing_ok=True)


def load_records(directory: pathlib.Path) -> dict[str, dict[str, Any]]:
    records = {}
    for path in sorted(directory.glob("*.json")) if directory.is_dir() else []:
        try:
            record = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
        if isinstance(record, dict) and isinstance(record.get("run"), str):
            records[record["run"]] = record
    return records


def claimed(record: dict[str, Any]) -> dict[str, str | None]:
    """name -> recorded stable ID (None: intent without an ID) for everything the record lists."""
    names: dict[str, str | None] = dict(record.get("map_vms") or {})
    names.update(record.get("created") or {})
    path = record.get("ownership_map")
    if path:
        try:
            document = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
            for name, entry in (document.get("vms") or {}).items():
                uuid = entry.get("uuid") if isinstance(entry, dict) else None
                if isinstance(uuid, str):
                    names[name] = uuid
        except (OSError, ValueError, AttributeError):
            pass
    return names


def owner_of(name: str, records: dict[str, dict[str, Any]]) -> str | None:
    """The run whose record lists NAME (never inferred from the name alone)."""
    return next((run for run, record in records.items() if name in claimed(record)), None)


def decide(name: str, current_id: str, records: dict[str, dict[str, Any]],
           alive: set[str] | frozenset[str] = frozenset()) -> tuple[str, str, str | None]:
    """(action, why, owner): "remove" only on affirmative evidence: a record lists the name and the live ID matches,
    and the owning run is not named alive (the caller holds the VM lock, so no other run is)."""
    owner = owner_of(name, records)
    encoded = name.split("-")[1] if GENERIC_SANDBOX.match(name) else None
    if owner is None:
        hint = f" (its name encodes run {encoded}, which has no record)" if encoded else ""
        return "kept", f"no run record lists it: ownership unproven{hint}", None
    record = records[owner]
    if owner in alive:
        return "kept", f"run {owner} is alive", owner
    recorded = claimed(record)[name]
    if recorded is None:
        return "kept", f"run {owner} recorded the intent but not a stable ID: ownership unproven", owner
    if recorded != current_id:
        return "kept", f"its ID {current_id} is not the one run {owner} recorded ({recorded}): a replacement", owner
    return "remove", f"run {owner} is finished (no run holds the VM lock but the caller) and recorded this name and ID", owner


def collect(sbx: str, directory: pathlib.Path, *, dry_run: bool = False, env: dict[str, str] | None = None,
            inventory: dict[str, str] | None = None, only: set[str] | None = None,
            alive: set[str] | frozenset[str] = frozenset()) -> list[dict[str, str]]:
    """Remove harness-prefix resources only on affirmative evidence (see `decide`). The caller holds the VM lock, so
    no other run is alive, except the runs it names in ALIVE. ONLY limits it to those names (no record or root is
    cleared then). A record is cleared only when everything it lists is gone; a failed removal keeps it. Returns one
    report row per resource."""
    records = load_records(directory)
    inventory = stock_vm_inventory(sbx, env=env) if inventory is None else inventory
    report: list[dict[str, str]] = []
    failed_runs: set[str] = set()
    for name in sorted(n for n in inventory if is_nfx_name(n) and (only is None or n in only)):
        action, why, owner = decide(name, inventory[name], records, alive)
        row = {"name": name, "owner": owner or "unknown", "action": action, "why": why}
        if action == "remove":
            if dry_run:
                row["action"] = "would remove"
            else:
                try:
                    remove_sandbox(sbx, name, claimed(records[owner])[name], {}, env=env)
                    row["action"] = "removed"
                except (OSError, ValueError, subprocess.SubprocessError) as error:
                    row.update(action="failed", why=str(error))
                    failed_runs.add(owner)
        report.append(row)
    if only is not None:
        return report
    for run, record in records.items():
        if run in alive or run in failed_runs:
            continue
        remaining = [n for n in claimed(record) if n in inventory and (not any(
            r["name"] == n and r["action"] in ("removed", "would remove") for r in report))]
        if remaining:
            report.append({"name": f"run {run}", "owner": run, "action": "kept record",
                           "why": f"still lists existing resources: {sorted(remaining)}"})
            continue
        if not dry_run:
            rules = list((record.get("policy_rules") or {}).values())
            if rules and remove_policy_rules(sbx, rules, env):
                report.append({"name": f"run {run}", "owner": run, "action": "kept record",
                               "why": "its policy rules could not be removed"})
                continue
            root = pathlib.Path(record.get("root") or "")
            if root.name == f"nfx-{run}" and root.is_dir():
                shutil.rmtree(root, ignore_errors=True)
            pathlib.Path(directory / f"{run}.json").unlink(missing_ok=True)
        report.append({"name": f"run {run}", "owner": run, "action": "would clear record" if dry_run else
                       "cleared record and root", "why": "the run is finished and everything it listed is gone"})
    return report
