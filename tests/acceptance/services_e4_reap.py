#!/usr/bin/env python3
"""E4 evidence: a job's services containers are reaped when its attempt ends (SVC-EP-03 c3-c4, SVC-EP-09 c2),
through REAL marsh jobs of the H1b `nfx` Kit on real sbx, with the E2 driver's harness (`services_e2_endpoint.py`:
one scope per scenario, a daemon this script starts, cleanup by exact names).

    vmlock.py --run e4 --shared -- python3 tests/acceptance/services_e4_reap.py \
        --prefix ~/.marsh-dev-E4 [--breaks-prefix ~/.marsh-dev-E4-breaks] --fixtures tests/acceptance/services/fixtures \
        [--only reap,cancel,crash,restart,shellexit,hold2] [--break MODE[,MODE]]

Scenarios (each prints one JSON line `{"scenario", "breaks", "checks", "ok", ...}`):
  reap       normal exit: one-off containers and a named volume of the job; a restart-policy container and a Compose
             project's container (kept); a concurrent job's container and the shell's container (untouched)
  cancel     the job is cancelled (SIGINT to `marsh run`) with a `docker build` running: its one-offs are reaped,
             the build process is gone, its restart-policy and Compose containers stay
  crash      the job's shell is killed (`kill -9 $$`) after it created a one-off container: reaped
  restart    the daemon is killed (`kill -9`) while a job holds containers, then started again: the records it left
             are reaped by the new daemon; restart-policy and Compose containers stay
  shellexit  EP-09 c2: a container made from the shell survives the shell's exit and an unrelated job's end
  hold2      SVC-EP-07 step 3 with suggestion 1: a second publish of the same job also reaches its netns hold

Only the scope's own directories and the VMs its ownership map records are touched (cleanup as in E2); nothing is
removed by pattern. Synthetic data only.
"""
from __future__ import annotations

import argparse
import json
import os
import pathlib
import secrets
import shutil
import signal
import subprocess
import sys
import time

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import services_e2_endpoint as e2  # noqa: E402  (E2's harness: scopes, daemon start, cleanup, checks)

e2.SCOPE_TAG = "nfx-e4"
Scope, check, poll, shell, stage_fixture, jline = e2.Scope, e2.check, e2.poll, e2.shell, e2.stage_fixture, e2.jline
BUSYBOX, NGINX, COMPOSE_UP = e2.BUSYBOX, e2.NGINX, e2.COMPOSE_UP

SCENARIOS: dict = {}


def scenario(name):
    def register(fn):
        SCENARIOS[name] = fn
        return fn
    return register


HOLD_JOB = r"""
set -u
docker volume create e4vol-{tag} >/dev/null
docker run -d --name e4-orphan-{tag} {busybox} sleep 9999 >/dev/null
docker run -d --name e4-keepr-{tag} --restart unless-stopped {busybox} sleep 9999 >/dev/null
docker run -d --name e4-vhold-{tag} -v e4vol-{tag}:/data {busybox} sleep 9999 >/dev/null
docker run -d --name e4-sneaky-{tag} --label com.docker.compose.project=e4 {busybox} sleep 9999 >/dev/null
{compose}
{build}
echo READY > .ready-{tag}
while [ ! -e .release-{tag} ]; do sleep 1; done
echo released
"""

BUILD_STEP = (
    "mkdir -p e4-build && printf 'FROM {busybox}\\nRUN sleep 300\\n' > e4-build/Dockerfile && "
    "(docker build -t e4/slow-{tag} e4-build > build-{tag}.log 2>&1 &) ; sleep 8"
)

def inventory(scope, project, vm=None) -> dict:
    """Containers (name -> state) and volumes of the project's services engine, read through `vx` only."""
    ps = scope.vx("docker ps -a --format '{{.Names}} {{.State}}'", cwd=project, vm=vm)
    vols = scope.vx("docker volume ls -q", cwd=project, vm=vm)
    states = {}
    for line in ps.stdout.splitlines():
        parts = line.split()
        if len(parts) == 2:
            states[parts[0]] = parts[1]
    return {"states": states, "volumes": vols.stdout.split(), "ok": ps.returncode == 0 and vols.returncode == 0,
            "stderr": (ps.stderr + vols.stderr)[-300:]}


def wait_for(path: pathlib.Path, ceiling: float = 900.0) -> bool:
    return bool(poll(lambda: path.exists(), ceiling, 1))


def held_job(scope, project, tag: str, build: bool = False, compose: bool = True):
    # Only one job of the project runs `compose up` (two at once race on the same project).
    script = HOLD_JOB.format(tag=tag, busybox=BUSYBOX, compose=COMPOSE_UP if compose else "",
                             build=BUILD_STEP.format(tag=tag, busybox=BUSYBOX) if build else "")
    return scope.popen(["run", "nfx", "exec", "sh", "-c", script], cwd=project, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)


def stack_running(inv: dict) -> bool:
    return any(name.endswith("-db-1") and state == "running" for name, state in inv["states"].items())


def gone_within(scope, project, name: str, ceiling: float = 60.0):
    return poll(lambda: name not in inventory(scope, project)["states"] or None, ceiling, 1)


def finish(proc, timeout=120):
    try:
        return proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        proc.kill()
        return proc.wait(timeout=30)


def release(project: pathlib.Path, tag: str) -> None:
    (project / f".release-{tag}").write_text("")


@scenario("reap")
def reap(args, scope: Scope) -> dict:
    """SVC-EP-03 c3-c4 at a normal exit, with the concurrent job and the shell (their objects stay)."""
    checks: dict = {}
    scope.start()
    project = stage_fixture(args, "stack-a", scope.project / "app")
    scope.enable_services(project)
    j1 = held_job(scope, project, "j1")
    j2 = held_job(scope, project, "j2", compose=False)
    try:
        checks["j1_holds_its_containers"] = check(wait_for(project / ".ready-j1"), "J1 READY", {})
        checks["j2_holds_its_container"] = check(wait_for(project / ".ready-j2"), "J2 READY", {})
        made = shell(scope, project, f"docker run -d --name e4-svcobj-sh {BUSYBOX} sleep 9999 >/dev/null; echo MADE")
        checks["shell_made_an_object"] = check(made[0] == 0 and "MADE" in made[1], "MADE", {"exit": made[0], "err": made[2][-200:]})
        before = inventory(scope, project)
        checks["before_release_j1_objects_are_running"] = check(
            all(before["states"].get(n) == "running" for n in ("e4-orphan-j1", "e4-keepr-j1", "e4-vhold-j1", "e4-sneaky-j1", "e4-orphan-j2", "e4-svcobj-sh")),
            "every object created so far is running", before)
        release(project, "j1")
        code = finish(j1)
        checks["j1_ended_normally"] = check(code == 0, "exit 0", {"exit": code, "err": (j1.stderr.read() or "")[-300:]})
        # The reap is done when the records file no longer names the attempt (the daemon's own record).
        records = scope.home / "services-reaps.json"
        poll(lambda: not records.exists() or not json.loads(records.read_text()).get("records"), 150, 2)
        after = poll(lambda: (lambda inv: inv if "e4-orphan-j1" not in inv["states"] and "e4-vhold-j1" not in inv["states"] else None)(inventory(scope, project)), 150, 2) or inventory(scope, project)
        checks["ep03_c3_one_off_container_reaped"] = check("e4-orphan-j1" not in after["states"], "e4-orphan-j1 gone", after["states"])
        checks["ep03_c3_one_off_using_a_volume_reaped"] = check("e4-vhold-j1" not in after["states"], "e4-vhold-j1 gone", after["states"])
        checks["ep03_c3_named_volume_kept"] = check("e4vol-j1" in after["volumes"], "e4vol-j1 still a volume", after["volumes"])
        checks["ep03_c4_restart_policy_kept"] = check(after["states"].get("e4-keepr-j1") == "running", "e4-keepr-j1 running", after["states"].get("e4-keepr-j1"))
        checks["ep03_c4_compose_label_kept"] = check(after["states"].get("e4-sneaky-j1") == "running", "e4-sneaky-j1 running", after["states"].get("e4-sneaky-j1"))
        checks["ep03_c4_compose_stack_kept"] = check(stack_running(after), "the stack's db running", {k: v for k, v in after["states"].items() if "db" in k})
        checks["ep03_concurrent_job_untouched"] = check(after["states"].get("e4-orphan-j2") == "running", "e4-orphan-j2 running", after["states"].get("e4-orphan-j2"))
        checks["ep03_shell_object_untouched"] = check(after["states"].get("e4-svcobj-sh") == "running", "e4-svcobj-sh running", after["states"].get("e4-svcobj-sh"))
        status = scope.services_status(project)
        relay = next(p for p in status["projects"] if p["root"] == str(project.resolve())).get("relay", {})
        checks["ep03_c3_relay_channels_idle"] = check(relay.get("channels", 99) <= 1, "no job channel open (the status read's own at most)", relay)
    finally:
        release(project, "j1")
        release(project, "j2")
        finish(j1)
        finish(j2)
    return checks


@scenario("cancel")
def cancel(args, scope: Scope) -> dict:
    """SVC-EP-03 c3-c4 on cancel: `marsh run` is interrupted with a build running; the build process ends too."""
    checks: dict = {}
    scope.start()
    project = stage_fixture(args, "stack-a", scope.project / "app")
    scope.enable_services(project)
    j1 = held_job(scope, project, "c1", build=True)
    try:
        checks["c1_holds_its_containers_and_a_build"] = check(wait_for(project / ".ready-c1"), "READY", {})
        before = inventory(scope, project)
        checks["before_cancel_objects_running"] = check(before["states"].get("e4-orphan-c1") == "running", "orphan running", before["states"])
        j1.send_signal(signal.SIGINT)
        code = finish(j1, timeout=180)
        checks["c1_cancelled"] = check(code is not None, "the job ended", {"exit": code, "err": (j1.stderr.read() or "")[-300:]})
        after = poll(lambda: (lambda inv: inv if "e4-orphan-c1" not in inv["states"] else None)(inventory(scope, project)), 90, 1) or inventory(scope, project)
        checks["ep03_c3_cancel_reaps_one_off"] = check("e4-orphan-c1" not in after["states"] and "e4-vhold-c1" not in after["states"],
                                                       "e4-orphan-c1 gone", after["states"])
        vm = scope.svc_vm(project)
        busy = poll(lambda: (lambda done: done if done.stdout.strip() == "0" else None)(
            subprocess.run([args.sbx, "exec", vm, "sh", "-c", "ps -eo args | grep -c '[s]leep 300'"], capture_output=True, text=True, timeout=120)),
            60, 2) or subprocess.run([args.sbx, "exec", vm, "sh", "-c", "ps -eo args | grep -c '[s]leep 300'"], capture_output=True, text=True, timeout=120)
        checks["ep03_c3_build_process_gone"] = check(busy.stdout.strip() == "0", "no `sleep 300` left in the services VM",
                                                     {"count": busy.stdout.strip(), "err": busy.stderr[-200:]})
        checks["ep03_c4_restart_policy_and_compose_kept_after_cancel"] = check(
            after["states"].get("e4-keepr-c1") == "running" and after["states"].get("e4-sneaky-c1") == "running" and stack_running(after),
            "keepr, sneaky and the stack running", {k: v for k, v in after["states"].items()})
    finally:
        release(project, "c1")
        finish(j1)
    return checks


@scenario("crash")
def crash(args, scope: Scope) -> dict:
    """A job whose shell dies (`kill -9 $$`) after it made a one-off container: the container is reaped."""
    checks: dict = {}
    scope.start()
    project = stage_fixture(args, "stack-a", scope.project / "app")
    scope.enable_services(project)
    code, out, err = scope.job(f"docker run -d --name e4-orphan-cr {BUSYBOX} sleep 9999 >/dev/null; "
                               f"docker run -d --name e4-keepr-cr --restart unless-stopped {BUSYBOX} sleep 9999 >/dev/null; kill -9 $$",
                               cwd=project, timeout=900)
    after = poll(lambda: (lambda inv: inv if "e4-orphan-cr" not in inv["states"] else None)(inventory(scope, project)), 90, 1) or inventory(scope, project)
    checks["job_died"] = check(code != 0, "a non-zero exit (the shell was killed)", {"exit": code, "err": err[-200:]})
    checks["ep03_crash_reaps_one_off"] = check("e4-orphan-cr" not in after["states"], "e4-orphan-cr gone", after["states"])
    checks["ep03_crash_keeps_restart_policy"] = check(after["states"].get("e4-keepr-cr") == "running", "e4-keepr-cr running", after["states"].get("e4-keepr-cr"))
    return checks


@scenario("restart")
def restart(args, scope: Scope) -> dict:
    """The daemon is killed (`kill -9`) while a job holds a one-off container; the next daemon reaps its record."""
    checks: dict = {}
    scope.start()
    project = stage_fixture(args, "stack-a", scope.project / "app")
    scope.enable_services(project)
    j1 = held_job(scope, project, "r1")
    try:
        checks["r1_holds_its_containers"] = check(wait_for(project / ".ready-r1"), "READY", {})
        vm = scope.svc_vm(project)
        before = inventory(scope, project, vm)
        checks["before_kill_objects_running"] = check(before["states"].get("e4-orphan-r1") == "running" and before["states"].get("e4-keepr-r1") == "running",
                                                      "orphan and keepr running", before["states"])
        records = scope.home / "services-reaps.json"
        recorded = json.loads(records.read_text()) if records.exists() else {}
        checks["the_attempt_was_recorded_before_the_kill"] = check(len(recorded.get("records", [])) >= 1, "a reap record on disk",
                                                                   recorded)
        scope.daemon.kill()  # SIGKILL: the exact daemon this run started
        scope.daemon.wait(timeout=60)
        if j1.poll() is None:
            j1.wait(timeout=120)
        scope.start()
        after = poll(lambda: (lambda inv: inv if inv["ok"] and "e4-orphan-r1" not in inv["states"] else None)(inventory(scope, project, vm)), 240, 2) or inventory(scope, project, vm)
        entry = scope.project_entry(project)
        vm_state = subprocess.run([args.sbx, "ls", "--json"], capture_output=True, text=True, timeout=60).stdout[-600:]
        checks["ep03_restart_reaps_the_recorded_one_off"] = check(after["ok"] and "e4-orphan-r1" not in after["states"], "e4-orphan-r1 gone after restart (the engine was read)",
                                                                  {"ok": after["ok"], "stderr": after["stderr"], "states": after["states"],
                                                                   "status": {k: entry.get(k) for k in ("state", "reason", "relay", "vm")}, "sbx": vm_state})
        # The engine stops containers without a restart policy when its daemon restarts; keeping them means they still exist.
        checks["ep03_restart_keeps_restart_policy_and_compose"] = check(after["ok"] and
            after["states"].get("e4-keepr-r1") == "running" and "e4-sneaky-r1" in after["states"]
            and any(name.endswith("-db-1") for name in after["states"]),
            "keepr running; sneaky and the stack's db still exist", after["states"])
        left = json.loads(records.read_text()).get("records", []) if records.exists() else []
        checks["the_record_is_gone_once_reaped"] = check(left == [], "no record left", left)
    finally:
        release(project, "r1")
        if j1.poll() is None:
            j1.kill()
    return checks


@scenario("shellexit")
def shellexit(args, scope: Scope) -> dict:
    """SVC-EP-09 c2: an object made from the shell survives the shell's exit and an unrelated job's end."""
    checks: dict = {}
    scope.start()
    project = stage_fixture(args, "stack-a", scope.project / "app")
    scope.enable_services(project)
    made = shell(scope, project, f"docker run -d --name e4-svcobj-x {BUSYBOX} sleep 9999 >/dev/null; echo MADE")
    checks["shell_made_it"] = check(made[0] == 0 and "MADE" in made[1], "MADE", {"exit": made[0], "err": made[2][-200:]})
    time.sleep(3)
    scope.job("true", cwd=project)
    names = scope.vx("docker ps --format '{{.Names}}'", cwd=project).stdout.split()
    checks["ep09_c2_shell_object_survives_shell_exit_and_a_job_end"] = check("e4-svcobj-x" in names, "e4-svcobj-x running",
                                                                             {"names": names[:12]})
    scope.vx("docker rm -f e4-svcobj-x", cwd=project)
    return checks


@scenario("hold2")
def hold2(args, scope: Scope) -> dict:
    """SVC-EP-07 step 3 (E2 review, suggestion 1): with `hold-netns-attach`, every publish of one job reaches the hold
    (not only the first), and both forwards answer."""
    checks: dict = {}
    scope.start()
    project = stage_fixture(args, "stack-a", scope.project / "app")
    scope.enable_services(project)
    scope.job("true", cwd=project)  # the Kit worker is up
    kits = e2.kit_vms(scope)
    marker = "/run/marsh-test-holds/netns-attach"
    script = (f"docker run -d --name e4-h1 -p 127.0.0.1:18101:80 {NGINX} >/dev/null; "
              "for i in $(seq 1 120); do c=$(curl -s -o /dev/null -w '%{http_code}' localhost:18101 || true); [ \"$c\" = 200 ] && break; sleep 0.5; done; echo H1=$c; "
              f"docker run -d --name e4-h2 -p 127.0.0.1:18102:80 {NGINX} >/dev/null; "
              "for i in $(seq 1 120); do c=$(curl -s -o /dev/null -w '%{http_code}' localhost:18102 || true); [ \"$c\" = 200 ] && break; sleep 0.5; done; echo H2=$c")
    proc = scope.popen(["run", "nfx", "exec", "sh", "-c", script], cwd=project, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    holds = 0
    release_failures: list = []
    deadline = time.monotonic() + 600

    def reached(vms):
        return [vm for vm in vms if subprocess.run([args.sbx, "exec", vm, "test", "-e", marker + ".reached"], capture_output=True).returncode == 0]

    try:
        while proc.poll() is None and time.monotonic() < deadline:
            seen = reached(kits)
            if seen:
                # The hold directory belongs to the worker (root); `sbx exec` runs as an unprivileged user by default,
                # so the release is made as root. Without it the hold only ends at its own 120 s timeout.
                for vm in seen:
                    rel = subprocess.run([args.sbx, "exec", "-u", "root", vm, "touch", marker + ".release"], capture_output=True, text=True)
                    if rel.returncode != 0:
                        release_failures.append(rel.stderr[-200:])
                    else:
                        holds += 1
                poll(lambda: not reached(seen), 30, 0.5)
            time.sleep(0.5)
        out, err = proc.communicate(timeout=120)
    finally:
        if proc.poll() is None:
            proc.kill()
    lines = out.strip().splitlines()
    checks["both_forwards_answer"] = check("H1=200" in lines and "H2=200" in lines, "H1=200 and H2=200", {"lines": lines[-4:], "err": err[-300:]})
    if "hold-netns-attach" in scope.breaks:
        checks["every_publish_reaches_the_hold"] = check(holds >= 2 and not release_failures, ">= 2 holds, each released (one per publish)",
                                                         {"holds": holds, "release_failures": release_failures})
    else:
        checks["no_hold_without_the_hold_mode"] = check(holds == 0, "no hold", {"holds": holds})
    return checks


SCENARIO_ORDER = ["reap", "cancel", "crash", "restart", "shellexit", "hold2"]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--prefix", required=True, help="a dev install (make dev DEV_PREFIX=...)")
    parser.add_argument("--breaks-prefix", help="the test-breaks install for --break runs")
    parser.add_argument("--fixtures", required=True, help="H1b's tests/acceptance/services/fixtures")
    parser.add_argument("--sbx", default="sbx")
    parser.add_argument("--images", help="the saved image tars (default: <fixtures' repo>/target/nfx/images)")
    parser.add_argument("--only", default="")
    parser.add_argument("--break", dest="breaks", default="", help="comma-separated break modes (the daemon's env)")
    parser.add_argument("--keep", action="store_true", help="keep the scope's directory even when it passes")
    args = parser.parse_args()
    args.run = secrets.token_hex(3)
    args.prefix = str(pathlib.Path(args.prefix).expanduser())
    args.fixtures = str(pathlib.Path(args.fixtures).expanduser().resolve())
    if args.breaks_prefix:
        args.breaks_prefix = str(pathlib.Path(args.breaks_prefix).expanduser())
    names = [n for n in (args.only.split(",") if args.only else SCENARIO_ORDER) if n]
    rc = 0
    for name in names:
        breaks = [b for b in args.breaks.split(",") if b]
        scope = Scope(args, name, breaks)
        result: dict = {}
        crashed = None
        try:
            result = SCENARIOS[name](args, scope)
        except BaseException as error:  # noqa: BLE001
            crashed = f"{type(error).__name__}: {error}"
        finally:
            problems = scope.cleanup()
        ok = crashed is None and not problems and all(c["ok"] for c in result.values())
        jline(scenario=name, run=args.run, breaks=breaks, ok=ok, crashed=crashed, cleanup_errors=problems, checks=result,
              scratch=str(scope.root))
        if ok and not args.keep:
            shutil.rmtree(scope.root, ignore_errors=True)  # this scope's own exact directory
        rc |= 0 if ok else 1
    return rc


if __name__ == "__main__":
    sys.exit(main())
