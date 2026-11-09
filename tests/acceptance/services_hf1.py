#!/usr/bin/env python3
"""Live witnesses for HF1 (services hardening follow-ups), against real sbx.

Built on E1's script (`services_e1_relay.py`: the relay test driver, the real-`marshd`
helper, owned-sandbox bookkeeping, cleanup), which must be next to this file. Two builds
are needed, as there: a normal one and one with `--features test-breaks`.

  sweep-race         item 1: a pin racing the reconciler's sweep is never lost
                     (break: sweep-pins-by-source-age)
  sweep-orphan       item 1: a pin that was mounted and never recorded is still swept
                     (break: bind-pin-leak)
  protected-identity item 4: a `.git` renamed in place stays protected
                     (break: protected-identity-off)
  gate-by-id         item 7: the Internet gate follows the recorded container id, not the name
                     (break: gate-by-name)
  watch-flood        item 6: MiBs of VM output without a newline do not grow the daemon, and
                     VM text in a reason is plain (break: engine-watch-unbounded)
  quarantine         item 5: the recovery message and log say what `services start` does
                     (needs E1's relay: the message is E1 code)

Item 2 (PERF-05 b) and its refusals are E1's own `--only perf` and `--only forge`
against this guest build.

Run under the VM lock (this script takes none):
  vmlock.py --run hf1 --shared -- python3 services_hf1.py ... --only sweep-race,...
  vmlock.py --run hf1-break -- python3 services_hf1.py ... --only sweep-race --breaks sweep-pins-by-source-age

Every sandbox this run creates is recorded by name and UUID (E1's helpers); nothing is killed
or removed by pattern. Every check prints `PASS name: detail` or `FAIL name: detail`.
"""
from __future__ import annotations

import argparse
import os
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import services_e1_relay as e1  # noqa: E402
from services_e1_relay import (  # noqa: E402
    FAILURES, Daemon, Driver, Run, check, eng, record_vm, run, say, vx,
)

PROD = "docker -H unix:///run/marsh-svc/docker.sock"
HOLDS = "/run/marsh-test-holds"


def engine_exec(vm: str, container: str, command: str, timeout: int = 120) -> str:
    """`sh -c COMMAND` in a container of the VM's boot engine, addressed by id."""
    result = run(["sbx", "exec", "-u", "root", vm, "docker", "exec", container, "sh", "-c", command], timeout)
    return (result.stdout + result.stderr).strip()


def pull_busybox(driver: Driver) -> None:
    pulled = driver.docker("POST", "/images/create?fromImage=busybox&tag=1.36")
    check("fixture/pull-busybox", pulled["status"] == 200, str(pulled["body"])[:100])


def create_with_bind(driver: Driver, name: str, source: str, target: str = "/d") -> dict:
    body = {"Image": "busybox:1.36", "Cmd": ["sleep", "3600"], "HostConfig": {"Binds": [f"{source}:{target}"]}}
    return driver.docker("POST", f"/containers/create?name={name}", body)


def pin_source(vm: str, name: str) -> str:
    return eng(vm, f"{PROD} inspect -f '{{{{range .Mounts}}}}{{{{.Source}}}}{{{{end}}}}' {name}").strip()


def old_project_dir(run_: Run, name: str) -> Path:
    """A folder under project A whose modification time is two hours old, with a file in it."""
    folder = run_.projects["pA"] / name
    folder.mkdir(exist_ok=True)
    (folder / "hello.txt").write_text("hello\n")
    old = time.time() - 7200
    os.utime(folder / "hello.txt", (old, old))
    os.utime(folder, (old, old))
    return folder


# ------------------------------------------------------------------ item 1


def scenario_sweep_race(run_: Run, breaks: tuple[str, ...] = ()) -> None:
    """A create racing the reconciler's tick (every 5 s) never loses its pin. Two shapes:
    a loop of creates with a 200 ms gap between mounting a pin and recording it, and one create
    held between the two for 12 s. The source folder is old, so the unfixed sweep's age test (the
    source's mtime) passes at once."""
    folder = old_project_dir(run_, "sweepdata")
    driver = Driver(run_, "breaks", breaks=("slow-pin-record", *breaks))
    vm = driver.up()["vm"]
    pull_busybox(driver)
    count, created = 120, 0
    started = time.monotonic()
    for i in range(count):
        answer = create_with_bind(driver, f"sw{i}", str(folder))
        created += answer["status"] == 201
    took = time.monotonic() - started
    names = " ".join(f"sw{i}" for i in range(count))
    script = (
        f"for n in {names}; do s=$({PROD} inspect -f '{{{{range .Mounts}}}}{{{{.Source}}}}{{{{end}}}}' $n 2>/dev/null); "
        f"[ -n \"$s\" ] && [ -e \"$s/hello.txt\" ] || echo MISSING $n; done"
    )
    missing = [line for line in eng(vm, script, 300).splitlines() if line.startswith("MISSING")]
    say("INFO", "sweep-race/loop", f"{created} of {count} creates answered 201 in {took:.0f} s (about {took / 5:.0f} reconciler ticks)")
    if breaks:
        check("sweep-race/loop-loses-pins-under-the-defect", len(missing) > 0, f"{len(missing)} of {created} pins gone: {missing[:3]}")
    else:
        check("sweep-race/loop-never-loses-a-pin", created == count and not missing, f"{created} created, {len(missing)} missing")
    driver.close()
    driver = Driver(run_, "breaks", breaks=("hold-pin-before-ledger", *breaks))
    vm = driver.up()["vm"]
    result: list[dict] = []
    thread = threading.Thread(target=lambda: result.append(create_with_bind(driver, "held", str(folder))))
    thread.start()
    reached = False
    for _ in range(60):
        if "pin-before-ledger.reached" in eng(vm, f"ls {HOLDS} 2>/dev/null"):
            reached = True
            break
        time.sleep(0.5)
    check("sweep-race/hold-reached", reached, "the agent is between mounting the pin and recording it")
    time.sleep(12)
    eng(vm, f"touch {HOLDS}/pin-before-ledger.release")
    thread.join(timeout=60)
    answer = result[0] if result else {"status": None, "body": "no answer"}
    source = pin_source(vm, "held") if answer["status"] == 201 else ""
    alive = bool(source) and "hello.txt" in eng(vm, f"ls {source} 2>&1")
    if breaks:
        check("sweep-race/hold-loses-the-pin-under-the-defect", answer["status"] == 201 and not alive, f"status {answer['status']}, pin {source or '-'} lists hello.txt: {alive}")
    else:
        check("sweep-race/held-create-keeps-its-pin", answer["status"] == 201 and alive, f"status {answer['status']}, pin {source or '-'} lists hello.txt: {alive}")
    driver.close()


def scenario_sweep_orphan(run_: Run, breaks: tuple[str, ...] = ()) -> None:
    """A pin mounted and never recorded (the agent died in between: `fault-pin-orphan`) is swept by
    a later tick, even though its source folder is new and the pin is young."""
    folder = run_.projects["pA"] / "orphandata"
    folder.mkdir(exist_ok=True)
    (folder / "hello.txt").write_text("hello\n")
    driver = Driver(run_, "breaks", breaks=("fault-pin-orphan", *breaks))
    vm = driver.up()["vm"]
    pull_busybox(driver)
    def pins() -> set[str]:
        return set(eng(vm, "ls /var/lib/marsh/binds/base 2>/dev/null").split())

    before = pins()
    answer = create_with_bind(driver, "orphan", str(folder))
    orphans = pins() - before
    check("sweep-orphan/the-orphan-exists", answer["status"] == 500 and len(orphans) == 1, f"create answered {answer['status']}; new pin paths: {sorted(orphans)}")
    gone = False
    deadline = time.monotonic() + 25
    while time.monotonic() < deadline:
        if not (pins() & orphans):
            gone = True
            break
        time.sleep(1)
    if breaks:
        check("sweep-orphan/defect-keeps-the-orphan", not gone, f"orphan still mounted after 25 s: {sorted(pins() & orphans)}")
    else:
        check("sweep-orphan/orphan-swept-within-25s", gone, "the unrecorded pin is unmounted and removed")
    driver.close()


# ------------------------------------------------------------------ item 4


def mount_options(vm: str, path: str) -> str:
    return eng(vm, f"awk -v p={path} '$5==p {{print $6}}' /proc/self/mountinfo").strip()


def can_write(vm: str, directory: str) -> bool:
    out = eng(vm, f"if echo x > {directory}/hf1-probe 2>/dev/null; then rm -f {directory}/hf1-probe; echo WROTE; else echo EROFS; fi")
    return out.strip() == "WROTE"


def scenario_protected_identity(run_: Run, breaks: tuple[str, ...] = ()) -> None:
    """`.git` renamed to `.gx` (a job with write access can do that) is still protected: a pin of the new name,
    and a pin of the project root, are read-only where it matters."""
    project = run_.projects["pA"]
    (project / ".git").mkdir(exist_ok=True)
    (project / ".git" / "HEAD").write_text("ref: refs/heads/main\n")
    (project / "src").mkdir(exist_ok=True)
    driver = Driver(run_, "breaks" if breaks else "normal", breaks=breaks)
    vm = driver.up()["vm"]
    pull_busybox(driver)
    seed = create_with_bind(driver, "idseed", str(project / "src"))
    check("protected-identity/seed", seed["status"] == 201, f"status {seed['status']}: the agent has now looked at the project root")
    try:
        os.rename(project / ".git", project / ".gx")
        named = create_with_bind(driver, "idgx", str(project / ".gx"), "/g")
        check("protected-identity/renamed-git-can-be-bound", named["status"] == 201, f"status {named['status']} {str(named['body'])[:100]}")
        if named["status"] == 201:
            pin = pin_source(vm, "idgx")
            options = mount_options(vm, pin)
            writable = can_write(vm, pin)
            if breaks:
                check("protected-identity/defect-leaves-the-renamed-git-writable", writable, f"pin {pin} options {options!r}")
            else:
                check("protected-identity/renamed-git-pin-is-read-only", "ro" in options.split(",") and not writable, f"pin {pin} options {options!r}, write {'worked' if writable else 'refused'}")
        root = create_with_bind(driver, "idroot", str(project), "/p")
        check("protected-identity/root-can-be-bound", root["status"] == 201, f"status {root['status']} {str(root['body'])[:100]}")
        if root["status"] == 201:
            pin = pin_source(vm, "idroot")
            gx, marsh, src = f"{pin}/.gx", f"{pin}/.marsh", f"{pin}/src"
            gx_writable = can_write(vm, gx)
            if breaks:
                check("protected-identity/defect-leaves-the-renamed-git-writable-in-a-root-pin", gx_writable, f"{gx}: {mount_options(vm, gx)!r}")
            else:
                check("protected-identity/root-pin-carries-the-renamed-git-read-only", not gx_writable and "ro" in mount_options(vm, gx).split(","), f"{gx}: {mount_options(vm, gx)!r}")
                check("protected-identity/root-pin-carries-marsh-read-only", not can_write(vm, marsh), f"{marsh}: {mount_options(vm, marsh)!r}")
            check("protected-identity/control-other-folders-stay-writable", can_write(vm, src), f"{src} accepts a write")
    finally:
        if (project / ".gx").exists() and not (project / ".git").exists():
            os.rename(project / ".gx", project / ".git")
        driver.close()


# ------------------------------------------------------------------ item 7


def scenario_gate_by_id(run_: Run, breaks: tuple[str, ...] = ()) -> None:
    """The VM renames the real engine container and puts a decoy under the name `marsh-svc-engine`. The
    daemon's `internet on|off` must reach the real engine (by the id recorded at creation), never the decoy."""
    daemon = Daemon(run_, "breaks" if breaks else "normal", breaks)
    try:
        on = daemon.cli("config", "services", "on")
        check("gate-by-id/services-on", on.returncode == 0, (on.stdout + on.stderr).strip()[:160])
        started = daemon.cli("services", "start", "--json")
        project = daemon.project()
        ok = started.returncode == 0 and bool(project) and project["relay"]["state"] == "ready"
        check("gate-by-id/ready", ok, str(project and project["relay"])[:200])
        if not ok:
            return
        vm = project["vm"]
        record_vm(vm)
        real = project["relay"]["generation"]["engine_id"]
        fake = "#!/bin/sh\necho \"$@\" >> /tmp/decoy.log\necho '{\"open\":true,\"verified\":true,\"error\":null}'\n"
        vx(vm, f"cat > /tmp/fake-marsh <<'EOF'\n{fake}EOF\nchmod 755 /tmp/fake-marsh; : > /tmp/decoy.log")
        vx(vm, "docker rename marsh-svc-engine marsh-svc-renamed")
        image = vx(vm, f"docker inspect -f '{{{{.Image}}}}' {real}")
        decoy = vx(vm, f"docker run -d --name marsh-svc-engine --entrypoint sleep -v /tmp/fake-marsh:/marsh/marsh:ro -v /tmp:/tmp {image} 3600").splitlines()[-1]
        holder = vx(vm, "docker inspect -f '{{.Id}}' marsh-svc-engine")
        check("gate-by-id/decoy-holds-the-name", holder == decoy and decoy != real, f"real {real[:12]}, name now resolves to {holder[:12]}")
        say("INFO", "gate-by-id/decoy", f"real engine {real[:12]} renamed; decoy {decoy[:12]} now holds the name")
        if holder != decoy:
            return

        def gate_state() -> str:
            return engine_exec(vm, real, "/marsh/marsh --internal-services internet status").replace('":true', '": true').replace('":false', '": false')

        check("gate-by-id/real-gate-starts-closed", '"open": false' in gate_state(), gate_state()[:100])
        turned_on = daemon.cli("services", "internet", "on", "--json")
        say("INFO", "gate-by-id/on", f"rc={turned_on.returncode} {(turned_on.stdout + turned_on.stderr).strip()[:140]}")
        opened = '"open": true' in gate_state()
        called = vx(vm, "cat /tmp/decoy.log").strip()
        if breaks:
            check("gate-by-id/defect-reaches-the-decoy-and-not-the-engine", bool(called) and not opened, f"decoy was asked {called!r}; real gate open: {opened}")
        else:
            check("gate-by-id/on-reaches-the-real-engine", turned_on.returncode == 0 and opened, f"rc={turned_on.returncode}; real gate open: {opened}")
            check("gate-by-id/decoy-never-asked", not called, f"decoy log: {called!r}")
            turned_off = daemon.cli("services", "internet", "off", "--json")
            closed = '"open": false' in gate_state()
            check("gate-by-id/off-reaches-the-real-engine", turned_off.returncode == 0 and closed, f"rc={turned_off.returncode}; real gate closed: {closed}")
            check("gate-by-id/decoy-still-never-asked", not vx(vm, "cat /tmp/decoy.log").strip(), "")
        vx(vm, f"docker rm -f {decoy}")
    finally:
        daemon.cli("services", "rm", "--yes")
        daemon.stop()


# ------------------------------------------------------------------ item 6


def scenario_watch_flood(run_: Run, breaks: tuple[str, ...] = ()) -> None:
    """The engine container (VM-controlled output) writes 16 MiB to its stdout and 16 MiB to its stderr, each
    without a newline. The daemon's memory stays flat and it stays up; and an event whose reason carries
    escape characters shows plain text."""
    daemon = Daemon(run_, "breaks" if breaks else "normal", breaks)
    try:
        on = daemon.cli("config", "services", "on")
        started = daemon.cli("services", "start", "--json")
        project = daemon.project()
        ok = on.returncode == 0 and started.returncode == 0 and bool(project) and project["relay"]["state"] == "ready"
        check("watch-flood/ready", ok, str(project and project["relay"])[:200])
        if not ok:
            return
        vm = project["vm"]
        record_vm(vm)
        real = project["relay"]["generation"]["engine_id"]
        pid = daemon.process.pid
        time.sleep(2)
        rss0 = e1.rss_kib(pid)
        peak = rss0
        floods = [
            threading.Thread(target=lambda fd=fd: engine_exec(vm, real, f"head -c 16777216 /dev/zero | tr '\\000' A > /proc/1/fd/{fd}", 300))
            for fd in (1, 2)
        ]
        for flood in floods:
            flood.start()
        deadline = time.monotonic() + 240
        while any(flood.is_alive() for flood in floods) and time.monotonic() < deadline:
            peak = max(peak, e1.rss_kib(pid))
            time.sleep(0.25)
        for _ in range(20):
            peak = max(peak, e1.rss_kib(pid))
            time.sleep(0.25)
        grown = (peak - rss0) / 1024
        alive = daemon.process.poll() is None
        state = (daemon.project() or {}).get("relay", {}).get("state")
        say("INFO", "watch-flood/rss", f"marshd RSS {rss0 / 1024:.1f} MiB before, peak {peak / 1024:.1f} MiB (+{grown:.1f} MiB) over 32 MiB of unterminated output")
        if breaks:
            check("watch-flood/defect-grows-the-daemon", grown > 24, f"+{grown:.1f} MiB")
        else:
            check("watch-flood/daemon-memory-stays-flat", grown < 8, f"+{grown:.1f} MiB (limit 8)")
            check("watch-flood/daemon-up-and-engine-still-ready", alive and state == "ready", f"alive={alive}, relay state {state}")
        event = '{"event":"lost","reason":"a\\u001b[31mred\\u001b]0;title\\u0007 b\\u202eC"}'
        engine_exec(vm, real, f"printf '\\n%s\\n' '{event}' > /proc/1/fd/1")
        reason = ""
        for _ in range(40):
            relay = (daemon.project() or {}).get("relay", {})
            reason = relay.get("reason") or ""
            if relay.get("state") in ("closed", "quarantined") and reason:
                break
            time.sleep(0.5)
        bad = [c for c in reason if ord(c) < 32 or 0x7F <= ord(c) <= 0x9F or c == "\u202e"]
        if breaks:
            check("watch-flood/defect-keeps-escape-characters", bool(bad) and "red" in reason, repr(reason[:120]))
        else:
            check("watch-flood/reason-is-plain-text", "red" in reason and not bad, repr(reason[:160]))
    finally:
        daemon.cli("services", "rm", "--yes")
        daemon.stop()


# ------------------------------------------------------------------ item 5


def scenario_quarantine(run_: Run, breaks: tuple[str, ...] = ()) -> None:
    """A relay ended by what the VM sent: status says how to clear it and what that does, and the log says it
    when the user does."""
    holds = run_.root / "mh" / "test-holds"
    daemon = Daemon(run_, "breaks", ("fault-agent-override",))
    try:
        daemon.cli("config", "services", "on")
        first = daemon.cli("services", "start", "--json")
        project = daemon.project()
        check("quarantine/ready-with-the-real-agent", first.returncode == 0 and bool(project) and project["relay"]["state"] == "ready", str(project and project["relay"])[:160])
        if not project:
            return
        vm = project["vm"]
        record_vm(vm)
        e1.place_peer(run_, vm, "unknown-type")
        holds.mkdir(parents=True, exist_ok=True)
        (holds / "agent-override").write_text(str(run_.projects["pA"]) + "\n")
        # End the real agent's relay (its exact pid, from status); the next start binds a new relay, now to the peer.
        engine_exec(vm, project["relay"]["generation"]["engine_id"], f"kill {project['relay']['agent_pid']}")
        time.sleep(2)
        stopped = daemon.cli("services", "status")
        restarted = daemon.cli("services", "start", "--json")
        say("INFO", "quarantine/restart", f"stop rc={stopped.returncode}; start rc={restarted.returncode} {(restarted.stdout + restarted.stderr).strip()[:200]}")
        document = None
        for _ in range(60):
            document = daemon.project()
            if document and document["relay"]["state"] == "quarantined":
                break
            time.sleep(0.5)
        relay = (document or {}).get("relay", {})
        say("INFO", "quarantine/relay", str(relay)[:300])
        text = relay.get("recovery") or ""
        check("quarantine/status-says-what-start-does", relay.get("state") == "quarantined" and "new engine generation" in text and "stops every running container" in text, f"state={relay.get('state')}: {text[:200]}")
        (holds / "agent-override").unlink(missing_ok=True)
        daemon.cli("services", "start", "--json")
        log = Path(daemon.log.name).read_text()
        say("INFO", "quarantine/log-tail", " | ".join(log.splitlines()[-6:])[:600])
        lines = [line for line in log.splitlines() if "was quarantined" in line]
        check("quarantine/log-says-it-when-the-user-starts-again", any("new engine generation" in line and "stops every running container" in line for line in lines), (lines or ["no such line"])[0][:220])
    finally:
        daemon.cli("services", "rm", "--yes")
        daemon.stop()


def main() -> int:
    parser = argparse.ArgumentParser()
    for name in ("driver-normal", "driver-breaks", "artifacts-normal", "artifacts-breaks", "shell-image-file", "marsh-normal", "marshd-normal",
                 "marsh-breaks", "marshd-breaks", "marshd-artifacts-normal", "marshd-artifacts-breaks"):
        parser.add_argument(f"--{name}")
    parser.add_argument("--only", default="sweep-race,sweep-orphan,protected-identity,gate-by-id,watch-flood")
    parser.add_argument("--breaks", default="", help="the defect for this pass (comma separated)")
    args = parser.parse_args()
    run_ = Run(args)
    breaks = tuple(m for m in args.breaks.split(",") if m)
    try:
        for name in args.only.split(","):
            say("INFO", "scenario", name)
            globals()[f"scenario_{name.replace('-', '_')}"](run_, breaks)
    finally:
        run_.finish()
    print("ALL PASSED" if not FAILURES else f"FAILED: {', '.join(FAILURES)}")
    return 1 if FAILURES else 0


if __name__ == "__main__":
    sys.exit(main())
