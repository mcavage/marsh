#!/usr/bin/env python3
"""Per-user registry of warm test scopes, and `make check-gc`.

A warm scope (`make check`: target/check/{home,control} under a checkout) keeps
a daemon and VMs alive between runs. Its ownership map lives in its control
home, so deleting the checkout (`git worktree remove`) used to delete the only
proof of which VMs it owned while they and its daemon lived on.

Each scope is therefore registered OUTSIDE any checkout, in one owner-only
directory per scope under the registry root (MARSH_SCOPE_REGISTRY, else
~/Library/Application Support/marsh-dev/scopes on macOS and
${XDG_STATE_HOME:-~/.local/state}/marsh-dev/scopes elsewhere):

  scope.json         written by the harness before any daemon starts: the
                     canonical home, control root, install prefix, creator.
  vm-ownership.json  written by the scope's daemon itself (marshd mirrors
                     every write of its map there, intents included, before
                     any `sbx create`: MARSH_OWNERSHIP_MIRROR).

`gc` acts only on registered scopes whose home no longer exists. For each it
stops the scope's own processes (exact command lines built from the recorded
prefix and home: `<prefix>/bin/marshd --home <home>` and `<prefix>/bin/marsh-mcp
... --home <home>`), then removes each mirrored VM strictly by recorded name and
UUID: only when stock lists that name with that exact UUID. A name present with
another UUID is foreign and never touched; an intent (no UUID) whose name is
present cannot be proven and is reported. The entry is deleted only once every
recorded VM is gone and no scope process remains. Nothing outside the registry
is ever touched.
"""
from __future__ import annotations

import argparse
import contextlib
import dataclasses
import fcntl
import hashlib
import json
import os
import pathlib
import shutil
import signal
import subprocess
import sys
import time
from typing import Any, Callable, Iterator

SCHEMA = "marsh.test-scope/v1"
REGISTRY_ENV = "MARSH_SCOPE_REGISTRY"
MIRROR_ENV = "MARSH_OWNERSHIP_MIRROR"
SCOPE_FILE = "scope.json"
MIRROR_FILE = "vm-ownership.json"

# (argv) -> completed `sbx` call; (()) -> [(pid, uid, command)]; (pid, signal) -> None
SbxRun = Callable[[list[str]], subprocess.CompletedProcess]
ProcessList = Callable[[], list[tuple[int, int, str]]]
Signal = Callable[[int, int], None]


def registry_root(environ: dict[str, str] | None = None) -> pathlib.Path:
    environ = os.environ if environ is None else environ
    if environ.get(REGISTRY_ENV):
        return pathlib.Path(environ[REGISTRY_ENV])
    home = pathlib.Path(environ.get("HOME") or pathlib.Path.home())
    if sys.platform == "darwin":
        return home / "Library" / "Application Support" / "marsh-dev" / "scopes"
    state = pathlib.Path(environ.get("XDG_STATE_HOME") or home / ".local" / "state")
    return state / "marsh-dev" / "scopes"


def scope_id(home: pathlib.Path) -> str:
    return hashlib.sha256(os.fsencode(home)).hexdigest()[:32]


def _private_directory(path: pathlib.Path) -> None:
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    metadata = path.lstat()
    if not path.is_dir() or path.is_symlink() or metadata.st_uid != os.getuid():
        raise RuntimeError(f"scope registry path is not this user's directory: {path}")
    path.chmod(0o700)


def _write_json(path: pathlib.Path, value: Any) -> None:
    temporary = path.with_name(f".{path.name}.tmp-{os.getpid()}")
    with contextlib.suppress(FileNotFoundError):
        temporary.unlink()
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "w") as handle:
        handle.write(json.dumps(value, indent=2, sort_keys=True) + "\n")
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)


def register(root: pathlib.Path, *, home: pathlib.Path, control_root: pathlib.Path, prefix: pathlib.Path,
             creator: str, checkout: pathlib.Path, seed_map: pathlib.Path | None = None) -> pathlib.Path:
    """Record a scope before its daemon starts; returns the daemon's mirror path.

    `home` must already exist (its canonical path names the scope). An existing
    ownership map (a scope warm from before the registry) seeds the mirror; the
    daemon replaces it with its own copy when it next starts.
    """
    home = home.resolve(strict=True)
    _private_directory(root)
    entry = root / scope_id(home)
    _private_directory(entry)
    _write_json(entry / SCOPE_FILE, {
        "schema": SCHEMA, "home": str(home), "control_root": str(control_root.resolve()),
        "prefix": str(prefix.resolve()), "creator": creator, "checkout": str(checkout.resolve()),
        "registered": time.time()})
    mirror = entry / MIRROR_FILE
    if seed_map is not None and seed_map.is_file() and not mirror.exists():
        _write_json(mirror, json.loads(seed_map.read_text()))
    return mirror


def unregister(root: pathlib.Path, home: pathlib.Path) -> None:
    """Drop a scope its owner stopped and deleted itself (`make check-reset`)."""
    entry = root / scope_id(home.resolve(strict=False))
    if entry.is_dir():
        shutil.rmtree(entry)


# Decisions --------------------------------------------------------------------

def recorded_vms(mirror: dict[str, Any]) -> dict[str, str | None]:
    """name -> recorded UUID (None for an intent) from a mirrored map."""
    if not isinstance(mirror, dict) or not isinstance(mirror.get("vms"), dict):
        raise ValueError("malformed ownership mirror")
    vms: dict[str, str | None] = {}
    for name, entry in mirror["vms"].items():
        uuid = entry.get("uuid") if isinstance(entry, dict) else entry  # v2 entries; v1 name -> uuid
        vms[name] = uuid if isinstance(uuid, str) and uuid else None
    for grant in (mirror.get("grants") or {}).values():
        for name, uuid in (grant.get("names") or {}).items():
            vms[name] = uuid if isinstance(uuid, str) and uuid else None
    return vms


def scope_processes(scope: dict[str, Any], processes: list[tuple[int, int, str]]) -> list[tuple[int, str]]:
    """This scope's own processes: exact command lines from its recorded prefix and home."""
    prefix, home = scope["prefix"], scope["home"]
    daemon = f"{prefix}/bin/marshd --home {home}"
    mcp = f"{prefix}/bin/marsh-mcp "
    mine = []
    for pid, uid, command in processes:
        if uid != os.getuid() or pid in (os.getpid(), 1):
            continue
        if command == daemon or (command.startswith(mcp) and
                                 (f" --home {home} " in command or command.endswith(f" --home {home}"))):
            mine.append((pid, command))
    return mine


@dataclasses.dataclass
class VmAction:
    name: str
    uuid: str | None
    action: str  # remove | gone | replaced | unproven-intent
    detail: str = ""


def plan_vms(recorded: dict[str, str | None], inventory: dict[str, str]) -> list[VmAction]:
    """What to do with each recorded VM given the stock name -> UUID inventory."""
    by_id = {uuid: name for name, uuid in inventory.items()}
    actions = []
    for name, uuid in sorted(recorded.items()):
        present = inventory.get(name)
        if uuid is None:
            actions.append(VmAction(name, None, "gone" if present is None else "unproven-intent",
                                    "" if present is None else f"name present as {present} but no UUID was recorded"))
        elif present == uuid:
            actions.append(VmAction(name, uuid, "remove"))
        elif uuid in by_id:
            actions.append(VmAction(name, uuid, "replaced", f"recorded UUID now named {by_id[uuid]}; not touched"))
        elif present is not None:
            actions.append(VmAction(name, uuid, "replaced", f"name now has UUID {present}; not touched"))
        else:
            actions.append(VmAction(name, uuid, "gone"))
    return actions


# Effects ----------------------------------------------------------------------

def system_processes() -> list[tuple[int, int, str]]:
    out = subprocess.run(["ps", "-axww", "-o", "pid=,uid=,command="], capture_output=True, text=True,
                         check=True, timeout=30).stdout
    rows = []
    for line in out.splitlines():
        parts = line.strip().split(None, 2)
        if len(parts) == 3 and parts[0].isdigit() and parts[1].isdigit():
            rows.append((int(parts[0]), int(parts[1]), parts[2]))
    return rows


def stop_processes(scope: dict[str, Any], list_processes: ProcessList, send: Signal,
                   grace: float = 10, poll: float = 0.2) -> list[str]:
    """SIGTERM, then SIGKILL, each process re-identified by its command line right before a signal."""
    notes = []
    for number in (signal.SIGTERM, signal.SIGKILL):
        targets = scope_processes(scope, list_processes())
        if not targets:
            return notes
        for pid, command in targets:
            with contextlib.suppress(ProcessLookupError):
                send(pid, number)
                notes.append(f"{signal.Signals(number).name} {pid} {command.split(' ', 1)[0]}")
        deadline = time.monotonic() + grace
        while time.monotonic() < deadline and scope_processes(scope, list_processes()):
            time.sleep(poll)
    remaining = scope_processes(scope, list_processes())
    if remaining:
        notes.append(f"processes remain: {[pid for pid, _ in remaining]}")
    return notes


def stock_inventory(sbx_run: SbxRun) -> dict[str, str]:
    from provenance import observe_stock_inventory
    return observe_stock_inventory(sbx_run)


@contextlib.contextmanager
def registry_lock(root: pathlib.Path) -> Iterator[None]:
    _private_directory(root)
    with open(root / ".lock", "a") as handle:
        fcntl.flock(handle, fcntl.LOCK_EX)
        yield


def collect(root: pathlib.Path, *, sbx_run: SbxRun, list_processes: ProcessList = system_processes,
            send: Signal = os.kill, dry_run: bool = False, grace: float = 10) -> list[dict[str, Any]]:
    """Retire every registered scope whose home is gone; one report row per entry acted on."""
    reports: list[dict[str, Any]] = []
    if not root.is_dir():
        return reports
    with registry_lock(root):
        for entry in sorted(p for p in root.iterdir() if p.is_dir() and not p.name.startswith(".")):
            report: dict[str, Any] = {"entry": str(entry), "processes": [], "vms": [], "errors": []}
            try:
                scope = json.loads((entry / SCOPE_FILE).read_text())
                if scope.get("schema") != SCHEMA or entry.name != scope_id(pathlib.Path(scope["home"])):
                    raise ValueError("scope.json does not name this entry")
            except (OSError, ValueError, KeyError, TypeError) as error:
                report["errors"].append(f"unreadable registration ({error}); left alone")
                reports.append(report)
                continue
            report["home"] = scope["home"]
            report["checkout"] = scope.get("checkout")
            if pathlib.Path(scope["home"]).exists():
                continue  # live: its owner manages it (make check-reset)
            try:
                recorded = recorded_vms(json.loads((entry / MIRROR_FILE).read_text()))
            except FileNotFoundError:
                recorded = {}  # no daemon ever started with the mirror
            except (OSError, ValueError) as error:
                report["errors"].append(f"unreadable ownership mirror ({error}); left alone")
                reports.append(report)
                continue
            if dry_run:
                report["processes"] = [f"would stop {pid} {command.split(' ', 1)[0]}"
                                       for pid, command in scope_processes(scope, list_processes())]
            else:
                report["processes"] = stop_processes(scope, list_processes, send, grace=grace)
                report["errors"] += [note for note in report["processes"] if note.startswith("processes remain")]
            try:
                actions = plan_vms(recorded, stock_inventory(sbx_run))
            except (ValueError, OSError, subprocess.SubprocessError) as error:
                report["errors"].append(f"stock inventory unavailable: {error}")
                reports.append(report)
                continue
            for action in actions:
                row = dataclasses.asdict(action)
                report["vms"].append(row)
                if action.action == "unproven-intent":
                    report["errors"].append(f"{action.name}: {action.detail}")
                if action.action != "remove" or dry_run:
                    continue
                # Fence the exact name+UUID immediately before the removal and prove absence after it.
                current = stock_inventory(sbx_run)
                if current.get(action.name) != action.uuid:
                    row["action"], row["detail"] = "replaced", "changed before removal; not touched"
                    continue
                removed = sbx_run(["rm", "--force", action.name])
                if removed.returncode != 0 or action.uuid in stock_inventory(sbx_run).values():
                    row["action"] = "remove-failed"
                    report["errors"].append(f"{action.name} ({action.uuid}) could not be removed")
                else:
                    row["action"] = "removed"
            if not report["errors"] and not dry_run:
                shutil.rmtree(entry)
                report["deleted"] = True
            reports.append(report)
    return reports


def sbx_runner(sbx: str) -> SbxRun:
    def run(argv: list[str]) -> subprocess.CompletedProcess:
        return subprocess.run([sbx, *argv], capture_output=True, timeout=120, check=False, stdin=subprocess.DEVNULL)
    return run


def describe(report: dict[str, Any]) -> list[str]:
    lines = [f"check-gc: scope {report.get('home', report['entry'])} (from {report.get('checkout')}) is gone"]
    lines += [f"  process: {note}" for note in report["processes"]]
    lines += [f"  vm {vm['name']} {vm['uuid'] or '(intent)'}: {vm['action']}"
              + (f" ({vm['detail']})" if vm["detail"] else "") for vm in report["vms"]]
    lines += [f"  kept entry: {error}" for error in report["errors"]]
    if report.get("deleted"):
        lines.append("  registry entry deleted")
    return lines


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("command", choices=("gc", "list"))
    parser.add_argument("--sbx", default=os.environ.get("MARSH_SBX", "sbx"))
    parser.add_argument("--registry", type=pathlib.Path, default=None)
    parser.add_argument("--dry-run", action="store_true", help="report decisions; signal and remove nothing")
    parser.add_argument("--advisory", action="store_true", help="exit 0 even when an entry must be kept")
    args = parser.parse_args()
    root = args.registry or registry_root()
    if args.command == "list":
        for entry in sorted(p for p in root.iterdir() if p.is_dir()) if root.is_dir() else []:
            with contextlib.suppress(OSError, ValueError):
                scope = json.loads((entry / SCOPE_FILE).read_text())
                state = "live" if pathlib.Path(scope["home"]).exists() else "gone"
                print(f"{entry.name} {state} {scope['home']}")
        return 0
    sbx = shutil.which(args.sbx) or args.sbx
    reports = collect(root, sbx_run=sbx_runner(sbx), dry_run=args.dry_run)
    for report in reports:
        print("\n".join(describe(report)), file=sys.stderr if report["errors"] else sys.stdout)
    return 1 if any(r["errors"] for r in reports) and not args.advisory else 0


if __name__ == "__main__":
    raise SystemExit(main())
