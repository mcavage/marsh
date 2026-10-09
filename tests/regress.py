#!/usr/bin/env python3
"""`make verify` / `make regress`: run the sbx-backed suites concurrently.

Every suite is an existing harness run exactly as `make regress-serial` runs it
(real installed dev product, real stock `sbx`, no fakes); this file only
schedules them. Each suite gets its own evidence directory and its own
disposable root (the harnesses create an isolated MARSH_HOME, control home,
project and daemon per process), so suites share nothing but the host's image
caches and stock SBX itself.

Concurrency is bounded (`--jobs`, default 3) and adaptive: a new suite does not
start while the 1-minute load average exceeds 2x the core count or free disk is
below 20 GB (a running suite is never killed for that; with nothing running one
suite always starts so the run cannot stall).

VM safety under concurrency. Harnesses used to fail when *any* new marsh-* VM
appeared. The runner snapshots the stock inventory once, before the first suite
starts (MARSH_REGRESS_BASELINE); each suite then proves (a) every pre-existing VM
is unchanged by name and stable ID and (b) none of the VMs in its OWN ownership
map remain. After the last suite the runner repeats the strict global check:
no marsh-* VM outside the baseline may exist.

Kept apart on purpose (`Suite.groups` / `Suite.avoid`):
  mcp-registry  mcp_load_uat publishes to the per-user stock MCP registry
                (`sbx mcp`, one refresh lock for every scope of this user):
                one holder at a time.
  cpu-heavy     self_dev compiles marsh inside a VM with every core (host load
                50+). acceptance asserts latency budgets (a two-branch fanout
                under 1 s: 0.3 s alone, 1.1 s beside the compile; `docker
                inspect` of a held container racing its 1 s hold), so it never
                overlaps that compile.
  quiet         timing-budget scenarios (split-overhead) are meaningless under
                load from sibling suites: they run alone, after the rest.
"""
from __future__ import annotations

import argparse
import collections
import dataclasses
import hashlib
import datetime
import json
import os
import pathlib
import shutil
import signal
import subprocess
import sys
import time
from typing import Any

CHECKOUT = pathlib.Path(__file__).resolve().parents[1]
ACCEPTANCE = CHECKOUT / "tests" / "acceptance"
ROOT = CHECKOUT / "target" / "regress"
MIN_FREE_BYTES = 20 * 1024 ** 3
BASELINE_ENV = "MARSH_REGRESS_BASELINE"
sys.path.insert(0, str(ACCEPTANCE))

# Billed live-agent scenarios are never part of a regression run.
LIVE_SCENARIOS = {"P32-live-claude-codex"}
# Timing-budget scenarios: they assert a latency relative to a warm baseline, so
# sibling suites competing for the same cores would make them flaky or vacuous.
QUIET_SCENARIOS = {"W24-performance"}

# The high-signal scenarios `make verify` runs (one extra scope per family, cold
# start included): the split/join and nested-process lifecycle, the confinement
# and capability boundaries, and cancellation. `make regress` runs all of them.
VERIFY_CORE = {
    "workspaces": ["W01-plain-bash", "W03-snapshot-fidelity", "W05-patch-roundtrip", "W07-status-pipefail",
                   "W08-join-after-failure", "W11-kit-confinement", "W12-admin-tamper", "W17-nested-from-kit",
                   "W19-interrupt-subtree", "W21-daemon-restart"],
    "processes": ["P01-job-surface", "P02-self-spawn-bash-c", "P03-direct-execve", "P07-run-three-callers",
                  "P08-fanout-in-job", "P17-branch-confinement", "P19-ctrl-c-tree", "P21-daemon-restart",
                  "P22-cap-socket-cap", "P38-fanout-cli"],
}

# Seconds each scenario took in an unloaded run (used only to balance shards;
# a scenario missing here is assumed to take DEFAULT_SCENARIO_S). Regenerate
# with tests/regress.py --learn-weights after a run.
DEFAULT_SCENARIO_S = 15.0
WEIGHTS_FILE = pathlib.Path(__file__).with_name("regress-weights.json")


@dataclasses.dataclass
class Suite:
    name: str
    family: str
    argv: list[str]
    est: float
    tiers: tuple[str, ...] = ("regress",)
    groups: tuple[str, ...] = ()  # resource groups this suite holds while running
    avoid: tuple[str, ...] = ()  # never overlap a running suite holding one of these groups
    after: tuple[str, ...] = ()  # suites that must have finished first
    status: str = "pending"
    started: float = 0.0
    seconds: float = 0.0
    process: subprocess.Popen[bytes] | None = None
    log: pathlib.Path | None = None
    evidence: pathlib.Path | None = None
    returncode: int | None = None
    note: str = ""


def run_text(argv: list[str], timeout: float = 60) -> str:
    return subprocess.run(argv, capture_output=True, text=True, timeout=timeout, check=True,
                          stdin=subprocess.DEVNULL).stdout


def list_scenarios(script: str) -> list[str]:
    out = run_text([sys.executable, str(ACCEPTANCE / script), "--list"])
    return [line.split("\t")[0] for line in out.splitlines() if line.strip()]


def load_weights() -> dict[str, float]:
    try:
        return {k: float(v) for k, v in json.loads(WEIGHTS_FILE.read_text()).items()}
    except (OSError, ValueError):
        return {}


def partition(items: list[str], weights: dict[str, float], parts: int) -> list[list[str]]:
    """Greedy longest-first balanced partition; every item lands in exactly one part."""
    bins: list[list[str]] = [[] for _ in range(parts)]
    totals = [0.0] * parts
    for item in sorted(items, key=lambda i: -weights.get(i, DEFAULT_SCENARIO_S)):
        index = totals.index(min(totals))
        bins[index].append(item)
        totals[index] += weights.get(item, DEFAULT_SCENARIO_S)
    return [b for b in bins if b]


class Plan:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.prefix = pathlib.Path(args.prefix).expanduser().resolve(strict=True)
        self.marsh = self.prefix / "bin" / "marsh"
        self.guest = self.prefix / "libexec" / "marsh"
        self.sbx = shutil.which(args.sbx) or args.sbx
        self.kit = args.kit
        self.revision = self.head()
        self.run_id = datetime.datetime.now().strftime("%Y%m%d-%H%M%S")
        self.run_dir = ROOT / self.run_id
        tmp = pathlib.Path("/private/tmp" if sys.platform == "darwin" else "/tmp").resolve()
        # Outside the checkout (evidence must not sit in a guest-visible mount) and
        # free of symlinks; the harnesses refuse anything else.
        self.evidence_root = tmp / f"marsh-regress-{os.getuid()}" / self.run_id
        self.baseline_file = self.run_dir / "stock-baseline.json"

    @staticmethod
    def head() -> str:
        return run_text(["git", "-C", str(CHECKOUT), "rev-parse", "HEAD"]).strip()

    def common(self) -> list[str]:
        return ["--sbx", self.sbx, "--source-revision", self.revision, "--source-tree", str(CHECKOUT)]

    def build(self) -> list[Suite]:
        weights = load_weights()
        python = sys.executable
        suites: list[Suite] = []

        def candidate(script: str, name: str, est: float, tiers: tuple[str, ...], extra: list[str] = (),
                      groups: tuple[str, ...] = (), avoid: tuple[str, ...] = ()) -> None:
            suites.append(Suite(name, name, [python, str(ACCEPTANCE / script), "--marsh", str(self.marsh),
                                              "--guest-artifacts", str(self.guest), *self.common(), *extra],
                                est, tiers, groups, avoid))

        def prefixed(script: str, family: str, scenarios: list[str], shards: int, tiers: tuple[str, ...],
                     per_shard_extra: list[list[str]] | None = None) -> None:
            normal = [s for s in scenarios if s not in QUIET_SCENARIOS]
            quiet = [s for s in scenarios if s in QUIET_SCENARIOS]
            core = [s for s in scenarios if s in VERIFY_CORE.get(family, ())]
            if core:
                missing = sorted(set(VERIFY_CORE[family]) - set(scenarios))
                if missing:
                    raise SystemExit(f"regress: VERIFY_CORE names scenarios {family} no longer has: {missing}")
                suites.append(Suite(f"{family}-core", f"{family}-core",
                                    [python, str(ACCEPTANCE / script), "--prefix", str(self.prefix), "--kit", self.kit,
                                     *self.common(), *[x for s in core for x in ("--only", s)]],
                                    sum(weights.get(s, DEFAULT_SCENARIO_S) for s in core) + 60, ("verify",)))
            parts = partition(normal, weights, shards)
            if quiet:
                parts.append(quiet)
            for number, part in enumerate(parts, 1):
                only = [x for s in part for x in ("--only", s)]
                name = f"{family}-{number}" if len(parts) > 1 else family
                is_quiet = part == quiet and bool(quiet)
                est = sum(weights.get(s, DEFAULT_SCENARIO_S) for s in part) + 60
                suites.append(Suite(name, family,
                                    [python, str(ACCEPTANCE / script), "--prefix", str(self.prefix), "--kit", self.kit,
                                     *self.common(), *only],
                                    est, tiers, ("quiet",) if is_quiet else ()))

        candidate("smoke.py", "smoke", 50, ("verify", "regress"), ["--kit", self.kit])
        candidate("run.py", "acceptance", 530, ("regress",), ["--kit", self.kit], avoid=("cpu-heavy",))
        ws = [s for s in list_scenarios("workspaces_uat.py")]
        prefixed("workspaces_uat.py", "workspaces", ws, self.args.workspace_shards, ("regress",))
        ps = [s for s in list_scenarios("processes_uat.py") if s not in LIVE_SCENARIOS]
        prefixed("processes_uat.py", "processes", ps, self.args.process_shards, ("regress",))
        for shell in ("bash", "zsh"):
            suites.append(Suite(f"shells-{shell}", "shells",
                                [python, str(ACCEPTANCE / "shells_uat.py"), "--prefix", str(self.prefix),
                                 "--kit", self.kit, *self.common(), "--shell", shell],
                                80, ("regress",) if shell == "zsh" else ("verify", "regress")))
        suites.append(Suite("acp", "acp",
                            [python, str(ACCEPTANCE / "acp_uat.py"), "--marsh", str(self.marsh), "--guest-artifacts",
                             str(self.guest), *self.common()], 120, ("verify", "regress")))
        suites.append(Suite("mcp-load", "mcp_load",
                            [python, str(ACCEPTANCE / "mcp_load_uat.py"), "--marsh", str(self.marsh),
                             "--guest-artifacts", str(self.guest), *self.common(), "--kit", "fixture"],
                            190, ("regress",), ("mcp-registry",), ("mcp-registry",)))
        suites.append(Suite("self-dev", "self_dev",
                            [python, str(ACCEPTANCE / "self_dev.py"), "--prefix", str(self.prefix),
                             "--sbx", self.sbx, "--kit", self.kit], 310, ("regress",), ("cpu-heavy",)))
        # Quiet shards wait for everything else.
        others = tuple(s.name for s in suites if "quiet" not in s.groups)
        for suite in suites:
            if "quiet" in suite.groups:
                suite.after = others
        return suites


def select(suites: list[Suite], tier: str, only: list[str], skip: list[str]) -> list[Suite]:
    def matches(suite: Suite, tokens: list[str]) -> bool:
        # A family token selects the family's suites of this tier; a suite name always selects it.
        return any(t == suite.name or (t == suite.family and tier in suite.tiers) for t in tokens)

    chosen = [s for s in suites if tier in s.tiers] if not only else [s for s in suites if matches(s, only)]
    chosen = [s for s in chosen if not matches(s, skip)]
    known = {t for s in suites for t in (s.name, s.family)}
    unknown = [t for t in only + skip if t not in known]
    if unknown:
        raise SystemExit(f"regress: unknown suite(s) {unknown}; known: {sorted(known)}")
    return chosen


def host_load_ok(cores: int) -> tuple[bool, str]:
    load1 = os.getloadavg()[0]
    free = shutil.disk_usage("/").free
    if load1 > 2 * cores:
        return False, f"load1 above 2x{cores} cores ({load1:.0f})"
    if free < MIN_FREE_BYTES:
        return False, "free disk below 20 GB"
    return True, ""


# A timing-budget suite waits (polling the load average, not sleeping a fixed
# time) for the host to settle below this many load units per core, up to
# QUIET_WAIT_S, then runs regardless and reports the load it ran under.
QUIET_LOAD_PER_CORE = 0.5
QUIET_WAIT_S = 180

# Failure signatures of host-wide contention that is not about the product.
EXTERNAL_SIGNATURES = {
    "docker hub refresh lock held by another process":
        "stock `sbx mcp` registry lock held by another process on this host (not a marsh failure; rerun)",
}


def stock_inventory(sbx: str) -> dict[str, str]:
    from provenance import stock_vm_inventory
    return stock_vm_inventory(sbx)


def stamp() -> str:
    return time.strftime("%H:%M:%S")


def tail_line(path: pathlib.Path | None) -> str:
    try:
        with path.open("rb") as handle:  # type: ignore[union-attr]
            handle.seek(0, os.SEEK_END)
            handle.seek(max(0, handle.tell() - 600))
            lines = [line.strip() for line in handle.read().decode(errors="replace").splitlines() if line.strip()]
        return lines[-1][:110] if lines else ""
    except (OSError, AttributeError):
        return ""


def terminate(suite: Suite, grace: float = 150) -> None:
    """SIGINT the suite's process group (harnesses clean up their own VMs on
    interrupt), then SIGKILL after `grace` seconds."""
    process = suite.process
    if process is None or process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGINT)
    except ProcessLookupError:
        return
    deadline = time.monotonic() + grace
    while process.poll() is None and time.monotonic() < deadline:
        time.sleep(0.5)
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()


def observe_owned(plan: Plan, seen: set[str]) -> None:
    """Accumulate the VM names in every suite's own ownership map (read-only).

    The harnesses delete their roots (and maps) on success, so the runner
    remembers what each map named while it lived and can check afterwards that
    none of those VMs remain, whatever the suite itself concluded.
    """
    for path in plan.evidence_root.glob("*/roots/*/control/*/vm-ownership.json"):
        try:
            seen.update(json.loads(path.read_text()).get("vms", {}))
        except (OSError, ValueError):
            pass  # being rewritten or already removed


def stop_preserved_scopes(plan: Plan) -> list[str]:
    """`marsh stop` every scope a failed suite preserved for debugging.

    A harness that fails before its own cleanup keeps its isolated root (and
    that scope's daemon and VMs). Those are the suite's own: the root's control
    directory names the scope, and `marsh stop` removes only the VMs in that
    scope's ownership map. The evidence and logs stay.
    """
    notes: list[str] = []
    for control in sorted(plan.evidence_root.glob("*/roots/*/control/*/vm-ownership.json")):
        scope = control.parent
        root = scope.parent.parent
        home = next((root / name for name in ("home", "selected-home")
                     if (root / name).is_dir()
                     and hashlib.sha256(os.fsencode((root / name).resolve())).hexdigest() == scope.name), None)
        project = next((root / name for name in ("project", "natural-project") if (root / name).is_dir()), root)
        if home is None:
            notes.append(f"preserved scope {root} has no matching home; stop it by hand")
            continue
        environment = {k: v for k, v in os.environ.items() if not k.startswith("MARSH_")}
        environment.update({"MARSH_HOME": str(home), "MARSH_CONTROL_HOME": str(scope.parent), "MARSH_SBX": plan.sbx})
        # Orphaned sessions of a killed harness make the daemon refuse a stop
        # until they end: poll for the scope to become stoppable.
        deadline = time.monotonic() + 120
        while True:
            done = subprocess.run([str(plan.marsh), "stop"], cwd=project, env=environment, capture_output=True,
                                  text=True, timeout=300, stdin=subprocess.DEVNULL)
            if done.returncode == 0 or time.monotonic() > deadline:
                break
            time.sleep(3)
        notes.append(f"stopped preserved scope {root.name}: " + (done.stderr or done.stdout).strip()[-200:])
    return notes


def launch(suite: Suite, plan: Plan, environment: dict[str, str]) -> None:
    assert plan.evidence_root is not None
    suite.evidence = plan.evidence_root / suite.name
    suite.evidence.mkdir(parents=True, mode=0o700)
    roots = suite.evidence / "roots"
    roots.mkdir(mode=0o700)
    environment = {**environment, "MARSH_UAT_ROOT": str(roots)}
    suite.log = plan.run_dir / f"{suite.name}.log"
    argv = list(suite.argv)
    # A commit landing mid-run (another worktree user) moves HEAD; the harnesses
    # check --source-revision against it, so bind each suite to HEAD at its start.
    if "--source-revision" in argv:
        argv[argv.index("--source-revision") + 1] = plan.head()
    # Harness flag names differ: self_dev takes a file, the rest a directory.
    evidence_arg = suite.evidence / "self-dev.json" if suite.family == "self_dev" else suite.evidence
    argv += ["--evidence", str(evidence_arg)]
    if suite.family == "mcp_load":
        commands = suite.evidence / "fixture-commands.json"
        commands.write_text(json.dumps({"fixture": plan.kit}) + "\n")
        argv += ["--commands", str(commands)]
    out = suite.log.open("wb")
    suite.started = time.monotonic()
    suite.process = subprocess.Popen(argv, cwd=CHECKOUT, env=environment, stdin=subprocess.DEVNULL,
                                     stdout=out, stderr=subprocess.STDOUT, start_new_session=True)
    suite.status = "running"
    suite.note = ""


def final_stock_check(sbx: str, baseline: dict[str, str], owned: set[str]) -> tuple[list[str], list[str]]:
    """(errors, notes). Every suite is done: none of the VMs the suites owned may
    remain, and the pre-existing ones are unchanged. Other new marsh-* VMs are
    reported, not failed: on a shared host another marsh run may own them."""
    from provenance import stock_cleanup_errors
    after = stock_inventory(sbx)
    from provenance import SharedBaseline
    errors = stock_cleanup_errors(SharedBaseline(baseline), after, owned=owned)
    others = sorted(n for n in after.keys() - baseline.keys() if n.startswith("marsh-") and n not in owned)
    notes = [f"new marsh VMs not owned by these suites (another marsh run?): {others}"] if others else []
    return errors, notes


def execute(plan: Plan, suites: list[Suite]) -> tuple[int, dict[str, Any]]:
    args = plan.args
    cores = os.cpu_count() or 4
    plan.run_dir.mkdir(parents=True)
    plan.evidence_root.mkdir(parents=True, mode=0o700)
    (plan.evidence_root.parent).chmod(0o700)
    baseline = stock_inventory(plan.sbx)
    plan.baseline_file.write_text(json.dumps({"vms": baseline, "taken_at": datetime.datetime.now().isoformat()}))
    environment = {k: v for k, v in os.environ.items() if not k.startswith("MARSH_")}
    environment.update({BASELINE_ENV: str(plan.baseline_file), "PYTHONUNBUFFERED": "1"})
    started = time.monotonic()
    pending = list(suites)
    running: list[Suite] = []
    done: list[Suite] = []
    held: collections.Counter[str] = collections.Counter()
    last_beat = started
    owned_seen: set[str] = set()
    last_poll = 0.0
    last_launch = 0.0
    backoff_reported = ""
    quiet_since = 0.0
    interrupted = False

    def on_signal(signum: int, _frame: object) -> None:
        nonlocal interrupted
        interrupted = True

    signal.signal(signal.SIGINT, on_signal)
    signal.signal(signal.SIGTERM, on_signal)
    names = {s.name for s in suites}
    print(f"regress: {len(suites)} suites, up to {args.jobs} at a time on {cores} cores; "
          f"logs {plan.run_dir}; evidence {plan.evidence_root}", flush=True)
    try:
        while (pending or running) and not interrupted:
            for suite in list(running):
                code = suite.process.poll()  # type: ignore[union-attr]
                if code is None:
                    if args.suite_timeout and time.monotonic() - suite.started > args.suite_timeout:
                        suite.note = f"timed out after {args.suite_timeout:.0f}s"
                        terminate(suite)
                        code = suite.process.returncode  # type: ignore[union-attr]
                        suite.status = "TIMEOUT"
                    else:
                        continue
                suite.seconds = time.monotonic() - suite.started
                suite.returncode = code
                if "quiet" in suite.groups:
                    suite.note = f"ran at load1 {os.getloadavg()[0]:.0f} on {cores} cores"
                if suite.status != "TIMEOUT":
                    suite.status = "PASS" if code == 0 else "FAIL"
                running.remove(suite)
                for group in suite.groups:
                    held[group] -= 1
                done.append(suite)
                tail = "" if suite.status == "PASS" else f"  last: {tail_line(suite.log)}"
                if suite.status != "PASS":
                    blob = suite.log.read_text(errors="replace") if suite.log else ""
                    hits = [why for sig, why in EXTERNAL_SIGNATURES.items() if sig in blob]
                    if hits:
                        suite.note = "; ".join(hits)
                        tail += f"\n           note: {suite.note}"
                print(f"[{stamp()}] {len(done):>2}/{len(suites)} {suite.status:<7} {suite.name:<16} "
                      f"{suite.seconds:6.0f}s  (running: {', '.join(r.name for r in running) or '-'}){tail}",
                      flush=True)
                if suite.status == "PASS" and not args.keep_evidence:
                    shutil.rmtree(suite.evidence, ignore_errors=True)  # type: ignore[arg-type]
            now = time.monotonic()
            # Launch: longest first, subject to dependencies and exclusive groups.
            if len(running) < args.jobs and now - last_launch >= args.stagger:
                finished = {s.name for s in done}
                for suite in sorted(pending, key=lambda s: -s.est):
                    if any(dep in names and dep not in finished for dep in suite.after):
                        continue
                    if "quiet" in suite.groups and running:
                        continue
                    if any(held[g] for g in suite.avoid) or any(set(suite.groups) & set(r.avoid) for r in running):
                        continue
                    ok, why = host_load_ok(cores)
                    if ok and "quiet" in suite.groups and os.getloadavg()[0] > QUIET_LOAD_PER_CORE * cores:
                        quiet_since = quiet_since or now
                        if now - quiet_since < QUIET_WAIT_S:
                            ok, why = False, f"waiting for a quiet host before {suite.name} (load1 {os.getloadavg()[0]:.0f})"
                            if why.split(" (")[0] != backoff_reported.split(" (")[0]:
                                print(f"[{stamp()}] {why}", flush=True)
                                backoff_reported = why
                            break
                    if not ok and running:
                        if why.split(" (")[0] != backoff_reported.split(" (")[0]:
                            print(f"[{stamp()}] backing off new suites: {why}", flush=True)
                        backoff_reported = why
                        break
                    backoff_reported = ""
                    pending.remove(suite)
                    launch(suite, plan, environment)
                    running.append(suite)
                    last_launch = time.monotonic()
                    for group in suite.groups:
                        held[group] += 1
                    print(f"[{stamp()}] start   {suite.name:<16} ({len(running)} running, {len(pending)} queued)",
                          flush=True)
                    break
            if now - last_poll >= 2:
                last_poll = now
                observe_owned(plan, owned_seen)
            if args.heartbeat and now - last_beat >= args.heartbeat and running:
                last_beat = now
                for suite in running:
                    print(f"[{stamp()}]   ... {suite.name:<16} {now - suite.started:5.0f}s  {tail_line(suite.log)}",
                          flush=True)
            time.sleep(1)
    finally:
        if interrupted:
            print(f"[{stamp()}] interrupted: stopping {len(running)} suites (they clean up their own VMs)...",
                  flush=True)
        for suite in running:
            terminate(suite)
            suite.seconds = time.monotonic() - suite.started
            suite.status = "INTERRUPTED"
            done.append(suite)
        for suite in pending:
            suite.status = "SKIPPED"
            suite.note = "interrupted before start"
            done.append(suite)

    stock_errors: list[str] = []
    stock_notes: list[str] = []
    observe_owned(plan, owned_seen)
    try:
        stock_notes += stop_preserved_scopes(plan)
    except (OSError, subprocess.SubprocessError) as error:
        stock_notes.append(f"could not stop a preserved scope: {error}")
    try:
        stock_errors, more = final_stock_check(plan.sbx, baseline, owned_seen)
        stock_notes += more
    except Exception as error:  # an unreadable inventory is a failure of the gate
        stock_errors = [f"final stock inventory unavailable: {error}"]
    wall = time.monotonic() - started
    summary = {
        "run_id": plan.run_id, "tier": args.tier, "wall_seconds": round(wall, 1), "jobs": args.jobs,
        "cores": cores, "source_revision": plan.revision, "kit": plan.kit,
        "log_dir": str(plan.run_dir), "evidence_root": str(plan.evidence_root),
        "stock_check": {"errors": stock_errors, "notes": stock_notes, "owned_vms_seen": sorted(owned_seen),
                        "baseline_vms": sorted(baseline)},
        "suites": [{"name": s.name, "status": s.status, "seconds": round(s.seconds, 1),
                    "exit": s.returncode, "log": str(s.log) if s.log else None,
                    "evidence": str(s.evidence) if s.evidence else None, "note": s.note,
                    "argv": s.argv} for s in sorted(done, key=lambda s: s.name)],
    }
    failed = [s for s in done if s.status != "PASS"]
    summary["ok"] = not failed and not stock_errors and not interrupted
    return (0 if summary["ok"] else 1), summary


def report(summary: dict[str, Any]) -> None:
    rows = summary["suites"]
    width = max([len(r["name"]) for r in rows] + [5])
    print(f"\n{'suite':<{width}}  {'status':<11} {'seconds':>7}")
    for row in sorted(rows, key=lambda r: -r["seconds"]):
        print(f"{row['name']:<{width}}  {row['status']:<11} {row['seconds']:>7.0f}")
    stock = summary["stock_check"]["errors"]
    print(f"{'stock-vms':<{width}}  {'FAIL' if stock else 'PASS':<11}")
    for error in stock:
        print(f"  stock: {error}")
    for note in summary["stock_check"]["notes"]:
        print(f"  note: {note}")
    for row in rows:
        if row["status"] != "PASS":
            print(f"  {row['name']}: log {row['log']}  evidence {row['evidence']}")
        if row["note"]:
            print(f"  {row['name']}: {row['note']}")
    longest = max((r["seconds"] for r in rows), default=0)
    total = sum(r["seconds"] for r in rows)
    print(f"\nregress {summary['tier']}: {'PASSED' if summary['ok'] else 'FAILED'} in "
          f"{summary['wall_seconds']:.0f}s wall ({total:.0f}s of suite time, longest {longest:.0f}s, "
          f"jobs {summary['jobs']})")


def learn_weights(summary_path: pathlib.Path) -> None:
    """Fold per-scenario seconds from retained evidence into regress-weights.json."""
    document = json.loads(summary_path.read_text())
    weights = load_weights()
    for suite in document["suites"]:
        evidence = suite.get("evidence")
        if not evidence:
            continue
        for path in pathlib.Path(evidence).glob("*.json"):
            try:
                scenarios = json.loads(path.read_text()).get("scenarios", [])
            except (OSError, ValueError):
                continue
            for scenario in scenarios:
                if isinstance(scenario, dict) and "seconds" in scenario and "scenario" in scenario:
                    weights[scenario["scenario"].split("[")[0]] = round(float(scenario["seconds"]), 1)
    WEIGHTS_FILE.write_text(json.dumps(dict(sorted(weights.items())), indent=1) + "\n")
    print(f"regress: wrote {len(weights)} scenario weights to {WEIGHTS_FILE}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--tier", choices=("verify", "regress"), default="regress")
    parser.add_argument("--prefix", default=str(pathlib.Path.home() / ".marsh-dev"))
    parser.add_argument("--sbx", default="sbx")
    parser.add_argument("--kit", default=None, help="immutable fixture Kit OCI reference (make fixture-ref)")
    parser.add_argument("--only", action="append", default=[], help="suite name or family; repeatable/comma list")
    parser.add_argument("--skip", action="append", default=[], help="suite name or family; repeatable/comma list")
    parser.add_argument("--jobs", type=int, default=3, help="max concurrent suites (default 3)")
    parser.add_argument("--stagger", type=float, default=5, help="seconds between suite starts")
    parser.add_argument("--workspace-shards", type=int, default=2)
    parser.add_argument("--process-shards", type=int, default=2)
    parser.add_argument("--suite-timeout", type=float, default=3600)
    parser.add_argument("--heartbeat", type=float, default=90, help="seconds between progress lines (0 = off)")
    parser.add_argument("--keep-evidence", action="store_true", help="keep evidence of passing suites too")
    parser.add_argument("--list", action="store_true", help="print the planned suites and exit")
    parser.add_argument("--learn-weights", metavar="SUMMARY_JSON", help="update regress-weights.json from a run")
    args = parser.parse_args()
    args.only = [t for item in args.only for t in item.split(",") if t]
    args.skip = [t for item in args.skip for t in item.split(",") if t]
    if args.learn_weights:
        learn_weights(pathlib.Path(args.learn_weights))
        return 0
    if not args.kit:
        parser.error("--kit is required (run make fixture-ref, or pass an immutable fixture Kit reference)")
    if args.jobs < 1:
        parser.error("--jobs must be >= 1")
    plan = Plan(args)
    suites = select(plan.build(), args.tier, args.only, args.skip)
    if args.list:
        for suite in suites:
            print(f"{suite.name:<16} est {suite.est:5.0f}s groups={','.join(suite.groups) or '-'}  "
                  f"{' '.join(a for a in suite.argv[2:] if a.startswith(('W', 'P', '--shell')) or a in ('bash', 'zsh'))}")
        return 0
    if not suites:
        parser.error("no suites selected")
    if sys.platform != "darwin":
        parser.error("the sbx-backed suites need the macOS host")
    # `make check` first (serial): its warm scope may restart the daemon and
    # VMs when the installed product changed, so it must settle before the
    # stock baseline is taken, and a broken product fails fast.
    pre: list[dict[str, Any]] = []
    if (not args.only or "check" in args.only) and "check" not in args.skip:
        began = time.monotonic()
        check = subprocess.run([sys.executable, str(CHECKOUT / "tests" / "check.py"), "--prefix", str(plan.prefix),
                                "--sbx", plan.sbx, "--kit", plan.kit], cwd=CHECKOUT, env={
            k: v for k, v in os.environ.items() if not k.startswith("MARSH_")})
        pre.append({"name": "check", "status": "PASS" if check.returncode == 0 else "FAIL",
                    "seconds": round(time.monotonic() - began, 1), "exit": check.returncode, "log": None,
                    "evidence": str(CHECKOUT / "target" / "check" / "last.json"), "note": "", "argv": check.args})
        if check.returncode != 0:
            print("regress: make check failed; not starting the suites", file=sys.stderr)
            return 1
    code, summary = execute(plan, suites)
    summary["suites"] = pre + summary["suites"]
    summary["wall_seconds"] = round(summary["wall_seconds"] + sum(r["seconds"] for r in pre), 1)
    report(summary)
    ROOT.mkdir(parents=True, exist_ok=True)
    (plan.run_dir / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    (ROOT / "last.json").write_text(json.dumps(summary, indent=2) + "\n")
    for old in sorted(p for p in ROOT.iterdir() if p.is_dir())[:-5]:
        shutil.rmtree(old, ignore_errors=True)
    return code


if __name__ == "__main__":
    raise SystemExit(main())
