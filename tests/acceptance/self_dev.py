#!/usr/bin/env python3
"""E2E 1 for docs/design/self-development.md: develop marsh from marsh --dev.

From the Mac: open `marsh --dev` in this checkout, build a Linux candidate
inside the dev shell, run `fixture identity` through it (its VMs go through
the host broker), probe refusals, exit, and check that no child VM, grant, or
disposable scratch remains and that pre-existing VMs are unchanged.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import pathlib
import shlex
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from provenance import SharedBaseline, stock_baseline  # noqa: E402

REPO = pathlib.Path(__file__).resolve().parents[2]
# VM-name prefixes of every dev grant this run opened (outer and relay-kill sessions).
PREFIXES: set[str] = set()


def stock(sbx: str) -> dict[str, dict]:
    out = subprocess.run([sbx, "ls", "--json"], check=True, capture_output=True, text=True).stdout
    listing = json.loads(out)
    rows = listing if isinstance(listing, list) else listing.get("sandboxes", [])
    return {row["name"]: row for row in rows}


def owned_names(control: pathlib.Path) -> set[str]:
    """VM names in this run's own daemon ownership map (empty once the scope is stopped)."""
    names: set[str] = set()
    for path in control.glob("*/vm-ownership.json"):
        try:
            names |= set(json.loads(path.read_text()).get("vms", {}))
        except (OSError, ValueError):
            pass
    return names


def new_vms(before: dict, after: dict, owned: set[str]) -> list[str]:
    """VMs that appeared and are this run's: alone, any new VM; beside concurrent
    suites (SharedBaseline), only those in our ownership map or under a grant prefix."""
    fresh = set(after) - set(before)
    if isinstance(before, SharedBaseline):
        fresh = {n for n in fresh if n in owned or any(n.startswith(p) for p in PREFIXES if p)}
    return sorted(fresh)


def stage(prefix_dir: str, sbx: str, prefix: str) -> tuple[str, dict, pathlib.Path]:
    """Run the installed dev product (`make dev`) with a dedicated home/control/cache.

    Returns (installed marsh, environment, control home). The install lives
    outside the checkout (the daemon refuses guest mounts overlapping its own
    artifacts) and enables `--dev` through its libexec/marsh/dev-enabled
    marker (the one shell image carries the dev tooling), so no dev-scope
    variable is set here.
    """
    install = pathlib.Path(prefix_dir).expanduser().resolve()
    if install.is_relative_to(REPO):
        raise SystemExit(f"{install} is inside the checkout; run make dev and pass its DEV_PREFIX")
    if not (install / "libexec/marsh/dev-enabled").is_file():
        raise SystemExit(f"{install} is not a dev install (no libexec/marsh/dev-enabled); run make dev")
    tmp = pathlib.Path(os.environ.get("MARSH_UAT_ROOT") or tempfile.gettempdir()).resolve()
    root = pathlib.Path(tempfile.mkdtemp(prefix=prefix, dir=tmp)).resolve()
    home, control, cache = root / "home", root / "control", root / "devcache"
    for path in (home, control, cache):
        path.mkdir(mode=0o700)
    env = dict(os.environ, MARSH_HOME=str(home), MARSH_CONTROL_HOME=str(control),
               MARSH_SBX=sbx, MARSH_DEV_CACHE_ROOT=str(cache))
    for name in ("MARSH_DAEMON_SOCKET", "MARSH_DAEMON_TOKEN", "MARSH_VM_PREFIX", "MARSH_DEV_DEPTH",
                 "MARSH_GUEST_ARTIFACTS", "MARSH_ENABLE_DEV_SCOPES", "MARSH_DEV_SHELL_IMAGE"):
        env.pop(name, None)
    return str(install / "bin/marsh"), env, control


def warm_dev_shell(marsh: str, sbx: str, env: dict, control: pathlib.Path,
                   load: str | None) -> tuple[str, bool]:
    """Open a first session (creates the warm dev shell VM) and return its name.

    With `load`, the session is `--dev --load LOAD`: it must warm only the dev
    shell VM, never the ordinary shell VM. Returns (name, no ordinary shell VM).
    """
    argv = [marsh, "--dev"] + (["--load", load] if load else []) + ["-c", "true"]
    warm = subprocess.run(argv, cwd=REPO, env=env, capture_output=True,
                          text=True, timeout=900, stdin=subprocess.DEVNULL)
    if warm.returncode != 0:
        print(warm.stdout + warm.stderr)
    owned = json.loads(next(control.glob("*/vm-ownership.json")).read_text())["vms"]
    dev = next((name for name, entry in owned.items() if entry.get("key") == "dev"), "unknown")
    # The daemon reserves the ordinary shell VM's name at startup; it must
    # not have been created.
    existing = stock(sbx)
    ordinary = [name for name, entry in owned.items()
                if entry.get("purpose") == "shell" and entry.get("key") != "dev" and name in existing]
    return dev, warm.returncode == 0 and not ordinary


def depth3_script(kit: str) -> str:
    """Runs inside the depth-3 dev shell (opened by the depth-2 inner daemon)."""
    return "\n".join([
        'echo "DEPTH3=$MARSH_DEV_DEPTH PREFIX3=$MARSH_VM_PREFIX"',
        'sbx ls --json >/dev/null; echo "D3_LS_RC=$?"',
        'sbx exec -i claude-marsh true; echo "D3_FOREIGN_RC=$?"',
        # A child VM three broker hops from stock: create, exec, remove.
        # Stock resolves a local template by its tag (the product verifies
        # the digest after create).
        'D3VM="${MARSH_VM_PREFIX}s-d3d3d3d3"',
        'sbx create --name "$D3VM" --pull never --skills off'
        ' --template "${MARSH_DEV_SHELL_TEMPLATE%@*}" shell; echo "D3_CREATE_RC=$?"',
        'sbx exec "$D3VM" true; echo "D3_EXEC_RC=$?"',
        'sbx rm --force "$D3VM" >/dev/null; echo "D3_RM_RC=$?"',
        # Depth 4 is refused: a depth-3 inner daemon (MARSH_DEV_DEPTH=3)
        # refuses --dev before creating any grant or VM.
        f"cd {shlex.quote(str(REPO))} && DEV_KIT={shlex.quote(kit)} make dev-inner"
        ' >"$MARSH_DEV_SCRATCH/tmp/build.log" 2>&1; echo "D3_BUILD_EXIT=$?"',
        "make --no-print-directory dev-inner-run DEV_INNER_FLAGS=--dev CMD=true"
        ' >"$MARSH_DEV_SCRATCH/tmp/d4.out" 2>&1; echo "D4_RC=$?";'
        ' echo "D4_OUT=$(grep -v "^cd " "$MARSH_DEV_SCRATCH/tmp/d4.out" | tr "\\n" " " | cut -c1-200)"',
        'make --no-print-directory dev-inner-stop >/dev/null 2>&1; echo "D3_INNER_STOP=$?"',
    ])


def probes(kit: str, depth2: bool, shell_vm: str, depth3: bool = False) -> str:
    refuse = [
        ("exec-own-shell-vm", f"sbx exec {shlex.quote(shell_vm)} true"),
        ("exec-foreign", "sbx exec -i claude-marsh true"),
        ("mount-ssh", 'sbx mount "$CHILD" "$HOME/.ssh:/x"'),
        ("mount-symlink-escape", 'ln -sfn "$HOME" "$MARSH_DEV_SCRATCH/tmp/escape"; '
                                 'sbx mount "$CHILD" "$MARSH_DEV_SCRATCH/tmp/escape:/x"'),
        ("create-env-file", 'sbx create --env-file /etc/passwd --name "${MARSH_VM_PREFIX}s-00000000" shell'),
        ("create-foreign-name", "sbx create --name marsh-s-00000000 shell"),
        ("template", "sbx template ls"),
        ("cp-from-vm", 'sbx cp "$CHILD:/etc/passwd" "$MARSH_DEV_SCRATCH/tmp/x"'),
        ("run-cloud", "sbx run --cloud -d shell"),
        ("run-workspace-ssh", 'sbx run -d shell "$HOME/.ssh"'),
        ("run-foreign-name", "sbx run -d --name claude-marsh shell"),
    ]
    lines = [
        "set -u",
        'echo "DEV PREFIX=$MARSH_VM_PREFIX SCRATCH=$MARSH_DEV_SCRATCH DEPTH=$MARSH_DEV_DEPTH"',
        f"DEV_KIT={shlex.quote(kit)} make dev-inner >\"$MARSH_DEV_SCRATCH/tmp/build.log\" 2>&1;"
        ' echo "BUILD_EXIT=$?"; tail -5 "$MARSH_DEV_SCRATCH/tmp/build.log"',
        "make --no-print-directory dev-inner-run CMD='fixture identity'; echo \"RUN_EXIT=$?\"",
        'echo "RECEIPTS=$(find "$MARSH_DEV_SCRATCH/control" -type f | wc -l)"',
        'sbx ls --json > "$MARSH_DEV_SCRATCH/tmp/ls.json"; echo "LS_EXIT=$?"',
        'echo "LS=$(tr -d "\\n" < "$MARSH_DEV_SCRATCH/tmp/ls.json")"',
        "CHILD=$(python3 -c 'import json,sys; r=json.load(open(sys.argv[1]))[\"sandboxes\"]; "
        "print(r[0][\"name\"] if r else \"none\")' \"$MARSH_DEV_SCRATCH/tmp/ls.json\")",
        'echo "CHILD=$CHILD"',
        # `sbx run` without --name: the broker names it `${prefix}r-<8>` and
        # mounts the current (natural project) directory.
        'sbx run -d shell >"$MARSH_DEV_SCRATCH/tmp/run.out" 2>&1; echo "RUN_D_EXIT=$?"; cut -c1-200 "$MARSH_DEV_SCRATCH/tmp/run.out"',
        "RUNVM=$(sbx ls --json | python3 -c 'import json,os,sys; p=os.environ[\"MARSH_VM_PREFIX\"]+\"r-\"; "
        "print(next((r[\"name\"] for r in json.load(sys.stdin)[\"sandboxes\"] if r[\"name\"].startswith(p)), \"none\"))')",
        'echo "RUNVM=$RUNVM"',
        'sbx exec "$RUNVM" sh -c "test -f $PWD/Cargo.toml"; echo "RUN_EXEC_EXIT=$?"',
    ]
    for name, command in refuse:
        lines.append(f'( {command} ) >"$MARSH_DEV_SCRATCH/tmp/p.out" 2>&1; rc=$?; '
                     f'echo "PROBE {name} rc=$rc $(tr "\\n" " " < "$MARSH_DEV_SCRATCH/tmp/p.out" | cut -c1-200)"')
    if depth2:
        # Depth 2: the inner candidate opens its own --dev child; inside it,
        # build again and run the depth-3 daemon through both brokers.
        inner = "\n".join([
            'echo "DEPTH2=$MARSH_DEV_DEPTH PREFIX2=$MARSH_VM_PREFIX"',
            'sbx ls --json; echo "D2_LS_RC=$?"',
            'sbx exec -i claude-marsh true; echo "D2_FOREIGN_RC=$?"',
            f"cd {shlex.quote(str(REPO))} && DEV_KIT={shlex.quote(kit)} make dev-inner"
            ' >"$MARSH_DEV_SCRATCH/tmp/build.log" 2>&1; echo "D2_BUILD_EXIT=$?"; tail -3 "$MARSH_DEV_SCRATCH/tmp/build.log"',
            "make --no-print-directory dev-inner-run CMD='fixture identity'; echo \"D3_RUN_EXIT=$?\"",
            "sbx ls --json; echo",
        ] + ([
            # Depth 3: the depth-2 inner daemon opens its own --dev child.
            "make --no-print-directory dev-inner-run DEV_INNER_FLAGS=--dev "
            f"CMD='eval \"$$(echo {base64.b64encode(depth3_script(kit).encode()).decode()} | base64 -d)\"';"
            ' echo "DEPTH3_EXIT=$?"',
            "make --no-print-directory dev-inner-stop >/dev/null 2>&1; echo \"D2_INNER_STOP=$?\"",
        ] if depth3 else []))
        encoded = base64.b64encode(inner.encode()).decode()
        lines.append("make --no-print-directory dev-inner-run DEV_INNER_FLAGS=--dev "
                     f"CMD='eval \"$$(echo {encoded} | base64 -d)\"'; echo \"DEPTH2_EXIT=$?\"")
        lines.append('echo "D1_BROKER_LOG:"; cat "$MARSH_DEV_SCRATCH/tmp/dev/broker.log" 2>/dev/null | tail -20')
    lines.append("exit 0")
    return "\n".join(lines)


RELAY_KILL_SCRIPT = "\n".join([
    "set -u",
    'echo "RK PREFIX=$MARSH_VM_PREFIX"',
    'RKVM="${MARSH_VM_PREFIX}s-rk0rk0rk"',
    'sbx create --name "$RKVM" --pull never --skills off'
    ' --template "${MARSH_DEV_SHELL_TEMPLATE%@*}" shell; echo "RK_CREATE_RC=$?"',
    # A brokered exec in flight when the relay dies.
    'sbx exec "$RKVM" sleep 97 >"$MARSH_DEV_SCRATCH/tmp/rk.out" 2>&1 & EXEC=$!',
    "sleep 3",
    # By process name: a -f pattern would also match this shell's own argv.
    'pkill -x marsh-relay; echo "RK_KILL_RC=$?"',
    'wait "$EXEC"; echo "RK_EXEC_RC=$?"',
    'echo "RK_EXEC_OUT=$(tr "\\n" " " < "$MARSH_DEV_SCRATCH/tmp/rk.out" | cut -c1-200)"',
    'sbx ls --json >/dev/null 2>&1; echo "RK_AFTER_LS_RC=$?"',
    "exit 0",
])


def relay_kill(args: argparse.Namespace, env: dict, control: pathlib.Path) -> dict[str, bool]:
    """Kill the session relay while a brokered `sbx exec` runs; the call fails
    visibly, the grant is revoked at session end, and the daemon stays healthy."""
    session = subprocess.run([args.marsh, "--dev", "-c", RELAY_KILL_SCRIPT], cwd=REPO, env=env,
                             capture_output=True, text=True, timeout=600, stdin=subprocess.DEVNULL)
    out = session.stdout + session.stderr
    print(out)
    marks = {}
    for line in out.splitlines():
        if line.startswith("RK PREFIX="):
            marks["PREFIX"] = line.split("=", 1)[1]
        for key in ("RK_CREATE_RC", "RK_KILL_RC", "RK_EXEC_RC", "RK_EXEC_OUT", "RK_AFTER_LS_RC"):
            if line.startswith(key + "="):
                marks[key] = line.split("=", 1)[1]
    prefix = marks.get("PREFIX", "")
    PREFIXES.add(prefix)
    time.sleep(2)
    stray = subprocess.run(["pgrep", "-f", "sbx exec .*sleep 97"], capture_output=True, text=True).stdout
    ownership = list(control.glob("*/vm-ownership.json"))
    grants = json.loads(ownership[0].read_text()).get("grants", {}) if ownership else {}
    # Relay loss leaves the shell VM cleanup-uncertain (quarantined): the
    # daemon still answers, and an explicit marsh reset recovers it.
    status = subprocess.run([args.marsh, "status", "--json"], cwd=REPO, env=env, capture_output=True,
                            text=True, timeout=60, stdin=subprocess.DEVNULL)
    reset = subprocess.run([args.marsh, "reset", "--json"], cwd=REPO, env=env,
                           capture_output=True, text=True, timeout=600, stdin=subprocess.DEVNULL)
    print("RK_STATUS", status.returncode, status.stdout[-600:], status.stderr[-600:])
    print("RK_RESET", reset.returncode, reset.stdout[-1500:], reset.stderr[-600:])
    healthy = subprocess.run([args.marsh, "--dev", "-c", "sbx ls --json >/dev/null && echo HEALTHY"],
                             cwd=REPO, env=env, capture_output=True, text=True, timeout=600,
                             stdin=subprocess.DEVNULL)
    print("RK_HEALTHY", healthy.returncode, healthy.stdout[-600:], healthy.stderr[-600:])
    return {
        "relay_kill_child_created": marks.get("RK_CREATE_RC") == "0",
        "relay_kill_killed": marks.get("RK_KILL_RC") == "0",
        "relay_kill_exec_failed_visibly": marks.get("RK_EXEC_RC") not in (None, "0")
        and bool(marks.get("RK_EXEC_OUT", "").strip()),
        "relay_kill_no_broker_after": marks.get("RK_AFTER_LS_RC") not in (None, "0"),
        "relay_kill_no_stray_host_exec": not stray.strip(),
        "relay_kill_no_prefixed_vm": bool(prefix) and not [n for n in stock(args.sbx) if n.startswith(prefix)],
        "relay_kill_grant_revoked": not grants,
        "relay_kill_daemon_answers": status.returncode == 0,
        "relay_kill_scope_reset_recovers": reset.returncode == 0,
        "relay_kill_daemon_healthy": healthy.returncode == 0 and "HEALTHY" in healthy.stdout,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prefix", default=str(pathlib.Path.home() / ".marsh-dev"),
                        help="dev product installed by make dev (DEV_PREFIX)")
    parser.add_argument("--sbx", required=True)
    parser.add_argument("--kit", required=True)
    parser.add_argument("--evidence", type=pathlib.Path)
    parser.add_argument("--depth2", action="store_true")
    parser.add_argument("--depth3", action="store_true",
                        help="--depth2, then a depth-3 dev child (depth 4 must be refused)")
    parser.add_argument("--relay-kill", action="store_true",
                        help="also kill the relay during a brokered exec in a second session")
    parser.add_argument("--only-relay-kill", action="store_true", help="skip the main session")
    parser.add_argument("--load", default="shell",
                        help="installed command the first session loads (--dev --load); empty to skip")
    args = parser.parse_args()
    args.depth2 = args.depth2 or args.depth3

    before = stock_baseline(args.sbx)  # name -> stable id
    args.marsh, env, control = stage(args.prefix, args.sbx, "marsh-selfdev-")
    root = control.parent
    # A first session creates the warm dev shell VM; its name is a probe target.
    shell_vm, only_dev_shell = warm_dev_shell(args.marsh, args.sbx, env, control, args.load or None)
    checks: dict[str, bool] = {}
    if args.load:
        checks["dev_load_warms_only_dev_shell"] = only_dev_shell
    marks: dict[str, str] = {}
    probe_lines: list[str] = []
    elapsed = 0.0
    if not args.only_relay_kill:
        started = time.monotonic()
        session = subprocess.run([args.marsh, "--dev", "-c", probes(args.kit, args.depth2, shell_vm, args.depth3)], cwd=REPO, env=env,
                                 capture_output=True, text=True, timeout=3600, stdin=subprocess.DEVNULL)
        elapsed = time.monotonic() - started
        out = session.stdout + session.stderr
        print(out)
        for line in out.splitlines():
            for key in ("BUILD_EXIT", "RUN_EXIT", "RECEIPTS", "LS_EXIT", "CHILD", "DEPTH2_EXIT", "D2_LS_RC",
                        "D2_FOREIGN_RC", "D2_BUILD_EXIT", "D3_RUN_EXIT", "RUN_D_EXIT", "RUNVM", "RUN_EXEC_EXIT",
                        "D3_LS_RC", "D3_FOREIGN_RC", "D3_CREATE_RC", "D3_EXEC_RC", "D3_RM_RC", "D4_RC", "D3_BUILD_EXIT",
                        "D4_OUT", "DEPTH3_EXIT"):
                if line.startswith(key + "="):
                    marks[key] = line.split("=", 1)[1]
            if line.startswith("DEV PREFIX="):
                marks["PREFIX"] = line.split()[1].split("=", 1)[1]
                marks["SCRATCH"] = line.split()[2].split("=", 1)[1]
        prefix, scratch = marks.get("PREFIX", ""), pathlib.Path(marks.get("SCRATCH", "/nonexistent"))
        PREFIXES.add(prefix)
        checks["session_exit_0"] = session.returncode == 0
        checks["build"] = marks.get("BUILD_EXIT") == "0"
        checks["run"] = marks.get("RUN_EXIT") == "0"
        checks["identity_output"] = any(line.startswith('{"cwd":') and str(REPO) in line for line in out.splitlines())
        checks["receipt_in_scratch_control"] = int(marks.get("RECEIPTS", "0") or 0) > 0
        ls_line = next((line[3:] for line in out.splitlines() if line.startswith("LS=")), "{}")
        try:
            listing = json.loads(ls_line)
            rows = listing if isinstance(listing, list) else listing.get("sandboxes", [])
        except json.JSONDecodeError:
            rows = None
        checks["child_vms_have_grant_prefix"] = bool(rows) and bool(prefix) and all(
            row["name"].startswith(prefix) for row in rows)
        probe_lines = [line for line in out.splitlines() if line.startswith("PROBE ")]
        checks["run_detached"] = marks.get("RUN_D_EXIT") == "0"
        checks["run_child_named_under_prefix"] = bool(prefix) and marks.get("RUNVM", "").startswith(prefix + "r-")
        checks["run_child_exec_sees_project"] = marks.get("RUN_EXEC_EXIT") == "0"
        checks["probes_ran"] = len(probe_lines) == 11
        checks["probes_refused"] = bool(probe_lines) and all(
            " rc=0 " not in line + " " and "refused" in line for line in probe_lines)
        if args.depth2:
            checks["depth2_ls"] = marks.get("D2_LS_RC") == "0"
            checks["depth2_foreign_refused"] = marks.get("D2_FOREIGN_RC") == "125"
            checks["depth2_build"] = marks.get("D2_BUILD_EXIT") == "0"
            checks["depth3_daemon_run"] = marks.get("D3_RUN_EXIT") == "0" and sum(
                line.startswith('{"cwd":') for line in out.splitlines()) >= 2
        if args.depth3:
            depth3 = next((line for line in out.splitlines() if line.startswith("DEPTH3=")), "")
            checks["depth3_session"] = marks.get("DEPTH3_EXIT") == "0" and depth3.startswith("DEPTH3=3 ")
            checks["depth3_ls"] = marks.get("D3_LS_RC") == "0"
            checks["depth3_foreign_refused"] = marks.get("D3_FOREIGN_RC") == "125"
            checks["depth3_child_vm"] = (marks.get("D3_CREATE_RC"), marks.get("D3_EXEC_RC"),
                                         marks.get("D3_RM_RC")) == ("0", "0", "0")
            checks["depth3_build"] = marks.get("D3_BUILD_EXIT") == "0"
            checks["depth4_refused"] = marks.get("D4_RC") not in (None, "0") and \
                "refused at nesting depth 3" in marks.get("D4_OUT", "")
        after = stock(args.sbx)
        checks["no_prefixed_vm_after_exit"] = bool(prefix) and not [n for n in after if n.startswith(prefix)]
        ownership = list(control.glob("*/vm-ownership.json"))
        grants = json.loads(ownership[0].read_text()).get("grants", {}) if ownership else {}
        checks["no_grant_record"] = not grants
        checks["disposable_scratch_removed"] = scratch.is_dir() and not any(
            (scratch / leaf).exists() for leaf in ("home", "control", "tmp"))
        checks["artifacts_kept"] = (scratch / "artifacts/bin/marsh").is_file()
        checks["baseline_unchanged"] = all(after.get(name, {}).get("id") == vm_id for name, vm_id in before.items())
    if args.relay_kill or args.only_relay_kill:
        checks.update(relay_kill(args, env, control))
        after = stock(args.sbx)
        checks["baseline_unchanged"] = all(after.get(name, {}).get("id") == vm_id
                                           for name, vm_id in before.items())
    # Retire the dev shell VM this run created (outer marsh stop).
    owned = owned_names(control)
    stop = subprocess.run([args.marsh, "stop"], cwd=REPO, env=env, capture_output=True, text=True,
                          timeout=600)
    final = stock(args.sbx)
    leftovers = new_vms(before, final, owned | owned_names(control))
    checks["scope_stop_leaves_no_new_vm"] = stop.returncode == 0 and not leftovers
    report = {"schema": "marsh.self-dev-e2e/v1", "passed": all(checks.values()), "checks": checks,
              "marks": marks, "probes": probe_lines, "elapsed_s": round(elapsed, 1), "leftovers": leftovers,
              "scope_stop": stop.stdout[-2000:] + stop.stderr[-2000:], "root": str(root)}
    if args.evidence:
        args.evidence.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
