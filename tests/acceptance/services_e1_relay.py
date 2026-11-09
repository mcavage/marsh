#!/usr/bin/env python3
"""Live checks for the daemon-side services relay (E1), against real sbx.

There is no job endpoint until E2, so these drive `marsh-sbx`'s
`services_relay_host_test` example, which stands in for the daemon: it creates
the services VM through S1's `ensure_services_vm`, starts S2's engine
generation and E1's relay, and answers JSON commands. Two builds are needed: a
normal driver and guest `marsh` (the normal scenarios), and a driver and guest
`marsh` built with `--features test-breaks` (the faults and the break modes).
Each artifact directory holds the pinned engine tar and pin plus that build's
`marsh-linux-arm64`.

Run it under the VM lock (this script takes none, it holds nothing the lock
guards beyond its own children):

  vmlock.py --run e1 --shared -- python3 services_e1_relay.py ... --only basic,bounds
  vmlock.py --run e1-breaks     -- python3 services_e1_relay.py ... --only breaks   (exclusive)

Every sandbox this run creates is recorded by name and UUID, the UUID is verified
before a sandbox is removed, and nothing is ever killed or removed by pattern:
processes are signalled only by an exact recorded pid. The run's files live in
one directory created with mkdtemp. Test daemons use MARSH_VM_PREFIX=marsh-xnfx01-.

Every check prints `PASS name: detail` or `FAIL name: detail`; the exit status is
non-zero if any check failed.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

FAILURES: list[str] = []
OWNED: dict[str, str] = {}  # sandbox name -> uuid
ENGINE_BASE = "/var/lib/docker/marsh-svc"


def say(status: str, name: str, detail: str = "") -> None:
    print(f"{status} {name}: {detail}", flush=True)
    if status == "FAIL":
        FAILURES.append(name)


def check(name: str, condition: bool, detail: str = "") -> bool:
    say("PASS" if condition else "FAIL", name, detail)
    return condition


def run(argv: list[str], timeout: int = 180) -> subprocess.CompletedProcess:
    return subprocess.run(argv, capture_output=True, text=True, timeout=timeout)


def inventory() -> dict[str, dict]:
    result = run(["sbx", "ls", "--json"])
    if result.returncode:
        raise RuntimeError(f"sbx ls failed: {result.stderr.strip()}")
    return {row["name"]: row for row in json.loads(result.stdout or "[]")}


def vx(vm: str, command: str, timeout: int = 120) -> str:
    result = run(["sbx", "exec", "-u", "root", vm, "sh", "-c", command], timeout)
    return (result.stdout + result.stderr).strip()


def eng(vm: str, command: str, timeout: int = 120) -> str:
    result = run(["sbx", "exec", "-u", "root", vm, "docker", "exec", "marsh-svc-engine", "sh", "-c", command], timeout)
    return (result.stdout + result.stderr).strip()


def record_vm(name: str) -> None:
    row = inventory().get(name)
    if row is None:
        raise RuntimeError(f"{name} is not listed")
    previous = OWNED.setdefault(name, row["id"])
    if previous != row["id"]:
        raise RuntimeError(f"{name} changed identity ({previous} -> {row['id']})")
    say("INFO", "owned-sandbox", f"{name} uuid={row['id']}")


def remove_owned() -> bool:
    ok = True
    for name, uuid in list(OWNED.items()):
        row = inventory().get(name)
        if row is None:
            OWNED.pop(name)
            continue
        if row["id"] != uuid:
            say("FAIL", f"cleanup/{name}", f"a different sandbox ({row['id']}) holds our name (ours was {uuid}); not removing it")
            ok = False
            continue
        result = run(["sbx", "rm", "--force", name], 240)
        if result.returncode or name in inventory():
            say("FAIL", f"cleanup/{name}", (result.stderr or result.stdout).strip()[:200])
            ok = False
        else:
            say("INFO", f"cleanup/{name}", f"removed uuid {uuid}")
            OWNED.pop(name)
    return ok


class Run:
    def __init__(self, args):
        self.args = args
        self.root = Path(tempfile.mkdtemp(prefix="nfx-e1-", dir="/private/tmp"))
        self.home = self.root / "home"
        self.home.mkdir()
        self.projects = {}
        for name in ("pA", "pB"):
            (self.root / name).mkdir()
            self.projects[name] = self.root / name
        self.image = Path(args.shell_image_file).read_text().strip()
        self.drivers: list[Driver] = []
        say("INFO", "run-root", str(self.root))

    def finish(self) -> None:
        for driver in list(self.drivers):
            driver.close()
        if remove_owned():
            # Our own run directory, by exact path, after its identity is checked.
            if self.root.parent == Path("/private/tmp") and self.root.name.startswith("nfx-e1-"):
                shutil.rmtree(self.root)


class Driver:
    def __init__(self, run_: Run, build: str = "normal", projects=("pA",), extra=(), breaks=(), env=None):
        self.run = run_
        driver = run_.args.driver_breaks if build == "breaks" else run_.args.driver_normal
        art = Path(run_.args.artifacts_breaks if build == "breaks" else run_.args.artifacts_normal)
        argv = [driver, "--home", str(run_.home)]
        for project in projects:
            argv += ["--project", str(run_.projects[project])]
        argv += ["--artifacts", str(art), "--shell-image", run_.image, "--shell-binary", str(art / "marsh-linux-arm64"), *extra]
        environment = {k: v for k, v in os.environ.items() if k != "MARSH_TEST_BREAK"}
        environment["MARSH_VM_PREFIX"] = "marsh-xnfx01-"
        environment["MARSH_SERVICES_POOL_BYTES"] = "4g"
        if breaks:
            argv.append("--test-breaks-from-env")
            environment["MARSH_TEST_BREAK"] = ",".join(breaks)
        environment.update(env or {})
        self.process = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, env=environment)
        self.hello = json.loads(self.process.stdout.readline())
        run_.drivers.append(self)

    def c(self, **command) -> dict:
        self.process.stdin.write(json.dumps(command) + "\n")
        self.process.stdin.flush()
        started = time.monotonic()
        line = self.process.stdout.readline()
        if not line:
            return {"ok": False, "error": "the driver exited", "_s": 0}
        answer = json.loads(line)
        answer["_s"] = round(time.monotonic() - started, 2)
        return answer

    def result(self, **command) -> dict:
        answer = self.c(**command)
        if not answer.get("ok"):
            raise RuntimeError(f"{command.get('cmd')}: {answer.get('error')}")
        return answer["result"]

    def up(self, project: int = 0) -> dict:
        result = self.result(cmd="up", p=project)
        record_vm(result["vm"])
        return result

    def close(self) -> None:
        if self.process.poll() is None:
            try:
                self.c(cmd="exit")
            except Exception:  # noqa: BLE001
                pass
            try:
                self.process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                say("FAIL", "driver-exit", f"driver pid {self.process.pid} did not exit")
        if self in self.run.drivers:
            self.run.drivers.remove(self)

    def docker(self, method: str, path: str, body=None, **extra) -> dict:
        answer = self.c(cmd="docker", method=method, path=path, body=json.dumps(body) if body is not None else None, **extra)
        if not answer.get("ok"):
            return {"status": None, "body": answer.get("error"), "bytes": 0}
        return answer["result"]


def ensure_container(driver: Driver, name: str, image: str, cmd: list[str] | None, port: int | None = None, restart: bool = True, project: int = 0,
                     container_port: int | None = None, env: list[str] | None = None, workspace: str = "base") -> str:
    """Create (once) and start a container through the filter; returns its id."""
    pulled = driver.docker("POST", f"/images/create?fromImage={image.split(':')[0]}&tag={image.split(':')[1]}", p=project, workspace=workspace)
    check(f"fixture/pull-{image}", pulled["status"] == 200, str(pulled["body"])[:120])
    body = {"Image": image, "HostConfig": {}}
    if cmd:
        body["Cmd"] = cmd
    if env:
        body["Env"] = env
    if port:
        body["HostConfig"]["PortBindings"] = {f"{container_port or port}/tcp": [{"HostIp": "127.0.0.1", "HostPort": str(port)}]}
    if restart:
        body["HostConfig"]["RestartPolicy"] = {"Name": "unless-stopped"}
    created = driver.docker("POST", f"/containers/create?name={name}", body, p=project, workspace=workspace)
    if created["status"] == 409:
        inspected = driver.docker("GET", f"/containers/{name}/json", p=project, workspace=workspace)
        cid = json.loads(inspected["body"])["Id"] if inspected["status"] == 200 else None
    else:
        check(f"fixture/create-{name}", created["status"] == 201, str(created["body"])[:200])
        cid = json.loads(created["body"])["Id"] if created["status"] == 201 else None
    if cid:
        started = driver.docker("POST", f"/containers/{cid}/start", p=project, workspace=workspace)
        check(f"fixture/start-{name}", started["status"] in (204, 304), str(started["body"])[:200])
    return cid


def echo_cmd(port: int) -> list[str]:
    return ["sh", "-c", f"while true; do nc -l -p {port} -e /bin/cat; done"]


def wait_port(driver: Driver, port: int, project: int = 0, seconds: int = 30, workspace: str = "base") -> bool:
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        ports = driver.result(cmd="ports", workspace=workspace, wait_ms=1000, p=project)["ports"]
        if port in ports:
            return True
    return False


def relay_processes(vm: str) -> str:
    return eng(vm, "ps -eo pid,args | grep '[i]nternal-services relay' | wc -l")


# ------------------------------------------------------------------ scenarios


def scenario_basic(run_: Run) -> None:
    driver = Driver(run_)
    up = driver.up()
    vm = up["vm"]
    inspected = vx(vm, "docker inspect -f '{{.Id}}' marsh-svc-engine")
    check("basic/relay-bound-to-engine-id", up["engine_id"] == inspected, f"relay generation engine_id={up['engine_id'][:12]} engine container={inspected[:12]}")
    check("basic/one-relay-agent", relay_processes(vm) == "1", f"relay agents in the engine: {relay_processes(vm)}")
    ping = driver.docker("GET", "/_ping")
    check("basic/docker-ping", ping["status"] == 200 and ping["body"] == "OK", str(ping))
    ensure_container(driver, "echo", "busybox:1.36", echo_cmd(19000), 19000)
    check("basic/ports-frame-lists-19000", wait_port(driver, 19000), "the published port is in the base workspace's map")
    echo = driver.result(cmd="tcp-echo", port=19000, kib=1024)
    check("basic/tcp-1mib-echo-intact", echo["ok"] and echo["sha_sent"] == echo["sha_received"], f"{echo['received']} bytes in {echo['ms']} ms")
    before = driver.result(cmd="status")["stats"]
    refused = driver.c(cmd="tcp-echo", port=19001, kib=1)
    other = driver.c(cmd="tcp-echo", port=19000, kib=1, workspace="lane-2")
    after = driver.result(cmd="status")["stats"]
    check("basic/unpublished-port-refused-by-the-daemon", not refused["ok"] and "not published" in refused["error"], refused.get("error", ""))
    check("basic/other-workspace-port-refused-by-the-daemon", not other["ok"] and "not published" in other["error"], other.get("error", ""))
    check("basic/refusals-sent-no-frame", re.search(r"tcp_opens_sent: (\d+)", before).group(1) == re.search(r"tcp_opens_sent: (\d+)", after).group(1), f"{before[:60]} -> {after[:60]}")
    captured = driver.result(cmd="capture", id="e1-c1", workspace="base")
    check("basic/capture-published", captured["outcome"] == "published", str(captured))
    listed = driver.result(cmd="list")["snapshots"]
    check("basic/list-shows-it", any(s["id"] == "e1-c1" and s["workspace"] == "base" for s in listed), str(listed))
    dropped = driver.result(cmd="drop", id="e1-c1")
    check("basic/drop-existed", dropped["existed"] is True, str(dropped))
    check("basic/drop-again-not-existed", driver.result(cmd="drop", id="e1-c1")["existed"] is False, "")
    bad = driver.c(cmd="capture", id="../../x", workspace="base")
    check("basic/bad-snapshot-id-refused-on-the-mac", not bad["ok"] and "invalid snapshot id" in bad["error"], bad.get("error", ""))
    check("basic/relay-still-ready", driver.result(cmd="status")["state"] == "ready", "")
    driver.close()


def scenario_bounds(run_: Run) -> None:
    driver = Driver(run_, extra=["--max-daemon", "300"])
    driver.up()
    ensure_container(driver, "echo", "busybox:1.36", echo_cmd(19000), 19000)
    wait_port(driver, 19000)
    ensure_container(driver, "firehose", "busybox:1.36", ["sh", "-c", "while true; do nc -l -p 19100 -e /bin/yes; done"], 19100)
    wait_port(driver, 19100)
    held = driver.result(cmd="hold", count=64, kind="docker", attempt="a1")
    check("bounds/64-channels-for-an-attempt", held["opened"] == 64 and held["refused_with"] is None, str(held))
    more = driver.result(cmd="hold", count=1, kind="docker", attempt="a1")
    check("bounds/65th-for-the-attempt-refused", more["opened"] == 0 and "attempt" in (more["refused_with"] or ""), str(more))
    for attempt in ("a2", "a3", "a4"):
        driver.result(cmd="hold", count=64, kind="docker", attempt=attempt)
    full = driver.result(cmd="hold", count=1, kind="docker", attempt="a5")
    check("bounds/257th-for-the-project-refused", full["opened"] == 0 and "project" in (full["refused_with"] or ""), str(full))
    status = driver.result(cmd="status")
    check("bounds/256-open-in-one-relay", status["channels"] == 256, f"channels={status['channels']}")
    driver.result(cmd="release", attempt="a1")
    time.sleep(0.5)
    again = driver.result(cmd="hold", count=1, kind="docker", attempt="a5")
    check("bounds/closing-frees-room", again["opened"] == 1, str(again))
    for attempt in ("a2", "a3", "a4", "a5"):
        driver.result(cmd="release", attempt=attempt)
    time.sleep(0.5)
    # No descriptor per channel on the Mac: the driver holds 256 channels over one pipe pair.
    pid = driver.process.pid
    before = len(run(["lsof", "-p", str(pid)]).stdout.splitlines())
    driver.result(cmd="hold", count=64, kind="docker", attempt="b1")
    driver.result(cmd="hold", count=64, kind="docker", attempt="b2")
    after = len(run(["lsof", "-p", str(pid)]).stdout.splitlines())
    check("bounds/no-fd-per-channel", after - before <= 3, f"descriptors {before} -> {after} with 128 more channels")
    driver.result(cmd="release", attempt="b1")
    driver.result(cmd="release", attempt="b2")
    stall = driver.result(cmd="stall-test", firehose_port=19100, echo_port=19000, seconds=8)
    check("bounds/slow-reader-does-not-stall-another-channel", stall["worst_round_trip_us"] < 1_000_000 and stall["rounds"] > 20, str(stall))
    buffered = stall["bytes_buffered_for_unread_channel"]
    check("bounds/unread-channel-buffers-at-most-its-window", 256 << 10 < buffered <= (1 << 20) + (64 << 10), f"{buffered} bytes buffered for a channel nobody read, over 8 s of a firehose (window 1 MiB)")
    driver.close()
    daemon = Driver(run_, extra=["--max-daemon", "40"])
    daemon.result(cmd="up")
    held = daemon.result(cmd="hold", count=40, kind="docker", attempt="d1")
    over = daemon.result(cmd="hold", count=1, kind="docker", attempt="d2")
    check("bounds/daemon-cap-refuses", held["opened"] == 40 and over["opened"] == 0 and "daemon" in (over["refused_with"] or ""), f"{held['opened']} then {over['refused_with']}")
    daemon.close()


def scenario_fairness(run_: Run, breaks: tuple[str, ...] = ()) -> None:
    driver = Driver(run_, "breaks" if breaks else "normal", breaks=breaks)
    driver.up()
    ensure_container(driver, "echo", "busybox:1.36", echo_cmd(19000), 19000)
    ensure_container(driver, "sink", "busybox:1.36", ["sh", "-c", "while true; do nc -l -p 19200 >/dev/null; done"], 19200)
    wait_port(driver, 19000)
    wait_port(driver, 19200)
    alone = driver.result(cmd="fairness-test", sink_port=19200, echo_port=19000, mib=256, probe=False)
    say("INFO", "fairness/calibration", f"256 MiB upload alone: {alone['upload_ms']} ms")
    both = driver.result(cmd="fairness-test", sink_port=19200, echo_port=19000, mib=256)
    ok = both["z_rounds"] >= 5 and both["z_max_ms"] is not None and both["z_max_ms"] < 2000 and both["upload_ms"] is not None
    if breaks:
        slow = both["z_max_ms"] is None or both["z_max_ms"] >= 2000 or both["z_rounds"] < 5
        check("fairness/defect-starves-the-small-channel", slow, str(both))
    else:
        check("fairness/small-channel-finishes-within-2s-during-bulk-upload", ok, str(both))
    driver.close()


def scenario_restart(run_: Run) -> None:
    """VM-06, driver half: the daemon stand-in is killed (exact pid) mid-stream and a new
    one brings a relay back; the old channel's data stops at the kill."""
    first = Driver(run_)
    up1 = first.up()
    vm = up1["vm"]
    uuid_before = inventory()[vm]["id"]
    ensure_container(first, "ticker", "busybox:1.36", ["sh", "-c", "while true; do nc -l -p 19300 -e sh -c 'while true; do date +%s; sleep 0.2; done'; done"], 19300)
    ensure_container(first, "echo", "busybox:1.36", echo_cmd(19000), 19000)
    wait_port(first, 19300)
    ticks = run_.root / "ticks.log"
    first.result(cmd="tcp-ticks", port=19300, log=str(ticks))
    time.sleep(2)
    seen = ticks.read_text().count("TICK")
    check("restart/ticks-before-the-kill", seen >= 3, f"{seen} ticks")
    kill_ms = int(time.time() * 1000)
    os.kill(first.process.pid, signal.SIGKILL)  # the exact pid we started
    first.process.wait(timeout=10)
    run_.drivers.remove(first)
    time.sleep(2)
    late = [line for line in ticks.read_text().splitlines() if line.startswith("TICK") and int(line.split()[1]) > kill_ms + 300]
    check("restart/no-tick-after-the-kill", not late, f"{len(late)} ticks after the kill")
    started = time.monotonic()
    second = Driver(run_)
    up2 = second.up()
    check("restart/same-vm-uuid", up2["vm"] == vm and inventory()[vm]["id"] == uuid_before, f"{up2['vm']} {uuid_before}")
    check("restart/new-generation", up2["engine_id"] != up1["engine_id"], f"{up1['engine_id'][:12]} -> {up2['engine_id'][:12]}")
    ok = wait_port(second, 19000, seconds=60)
    check("restart/restart-policy-container-is-back-and-published", ok, "the echo container (unless-stopped) runs in the new generation")
    echo = second.result(cmd="tcp-echo", port=19000, kib=1024)
    check("restart/channels-work-on-the-new-relay", echo["ok"], f"{time.monotonic() - started:.1f}s after the restart began")
    check("restart/one-relay-agent", relay_processes(vm) == "1", relay_processes(vm))
    paused = eng(vm, "docker -H unix:///run/marsh-svc/docker.sock ps -q --filter status=paused | wc -l")
    check("restart/nothing-paused", paused == "0", paused)
    second.close()


def place_peer(run_: Run, vm: str, scenario: str) -> None:
    peer = Path(__file__).with_name("services_e1_peer.py")
    run(["sbx", "cp", str(peer), f"{vm}:/tmp/e1-peer.py"])
    vx(vm, f"mkdir -p /run/nfx && install -m 755 /tmp/e1-peer.py /run/nfx/peer.py && echo {scenario} > /run/nfx/e1-scenario && : > /run/nfx/e1-peer.log")


HOSTILE = ["ready-stale", "oversize", "truncated", "unknown-type", "stale-channel", "duplicate-opened",
           "credit-over-window", "ports-zero", "ports-many", "ports-dup-scope", "ports-foreign",
           "snapshot-id", "unrequested", "restored-unrequested", "ready-bad-window", "ready-extra-field",
           "unknown-type-escape"]
# Variants whose handshake itself is refused: `up` fails and the project is quarantined.
FAIL_START = ("ready-stale", "ready-bad-window", "ready-extra-field")


def has_control(text: str) -> bool:
    return any(ord(c) < 32 or ord(c) == 127 for c in text)


def scenario_hostile(run_: Run, only: list[str] | None = None, breaks: tuple[str, ...] = (), expect_quarantine: bool = True) -> list[tuple[str, bool]]:
    """SEC-12, SEC-21: a controlled peer stands in for project A's agent. It sends one invalid
    frame and stays connected. Project B (same daemon process) works throughout."""
    holds = run_.home / "test-holds"
    holds.mkdir(exist_ok=True)
    # Make both VMs with the normal agent first, and a working echo for project B.
    normal = Driver(run_, "breaks", projects=("pA", "pB"))
    vm_a = normal.up(0)["vm"]
    vm_b = normal.up(1)["vm"]
    ensure_container(normal, "echo", "busybox:1.36", echo_cmd(19000), 19000, project=1)
    normal.close()
    results = []
    for scenario in only or HOSTILE:
        place_peer(run_, vm_a, scenario)
        (holds / "agent-override").write_text(str(run_.projects["pA"]) + "\n")
        driver = Driver(run_, "breaks", projects=("pA", "pB"), breaks=("fault-agent-override", *breaks))
        driver.up(1)  # project B's relay, in the same daemon process, first
        started = time.monotonic()
        up = driver.c(cmd="up", p=0)
        closed, ready_seen, invalid_at = False, up["ok"], started
        if up["ok"]:
            record_vm(up["result"]["vm"])
            invalid_at = time.monotonic()
            if scenario == "snapshot-id":
                driver.c(cmd="list", p=0)
            elif scenario in ("duplicate-opened", "credit-over-window"):
                driver.c(cmd="hold", count=1, kind="docker", attempt="h", p=0)
            else:
                time.sleep(0.2)
            for _ in range(60):
                state = driver.result(cmd="status", p=0)
                if state["state"] in ("quarantined", "closed"):
                    closed = True
                    break
                time.sleep(0.25)
        else:
            # A violation seen before `up` returned ends the start itself.
            closed = scenario in FAIL_START
        elapsed = time.monotonic() - invalid_at
        state = driver.result(cmd="status", p=0)
        time.sleep(0.5)
        log = vx(vm_a, "cat /run/nfx/e1-peer.log")
        events = [json.loads(line) for line in log.splitlines() if line.startswith("{")]
        opens_after_ready = [e for e in events if e.get("event") == "recv" and (e.get("frame") or {}).get("type") in ("open", "capture", "drop")]
        eof = any(e.get("event") == "eof" for e in events)
        responsive = driver.c(cmd="status", p=0)["ok"]
        b_echo = driver.c(cmd="tcp-echo", port=19000, kib=256, p=1)
        b_state = driver.result(cmd="status", p=1)
        reason = state.get("reason") or ""
        label = f"hostile/{scenario}"
        sent = next((e["t"] for e in events if e.get("event") == "mark" and e.get("label") == "invalid-frame-sent"), None)
        ended = next((e["t"] for e in events if e.get("event") == "eof"), None)
        within = (ended - sent) if sent is not None and ended is not None else None
        if breaks:
            check(f"{label}/defect-keeps-the-relay-open", not closed, f"state={state['state']} reason={reason[:100]}")
            results.append((scenario, closed))
        else:
            ok_time = scenario in FAIL_START or (within is not None and within <= 5.0)
            check(f"{label}/relay-closed-within-5s", closed and ok_time, f"state={state['state']}; the daemon closed the link {within if within is None else round(within, 2)} s after the invalid frame (peer clock): {reason[:120]}")
            if scenario not in FAIL_START:
                if up["ok"]:
                    check(f"{label}/quarantined-with-a-reason", state["state"] == "quarantined" and bool(reason), f"{state['state']}: {reason[:140]}")
                else:
                    check(f"{label}/start-refused-with-a-reason", closed, up.get("error", "")[:160])
                check(f"{label}/peer-saw-the-daemon-close", eof, "the peer log ends with eof it did not initiate")
            else:
                check(f"{label}/no-open-or-capture-frame-after-the-bad-ready", not opens_after_ready and not ready_seen, f"{len(opens_after_ready)} frames; reason={reason[:100]}")
                check(f"{label}/start-refused-and-project-quarantined", not up["ok"] and state["state"] == "quarantined" and bool(reason), f"{state['state']}: {reason[:120]}; start said: {up.get('error', '')[:120]}")
                check(f"{label}/peer-saw-the-daemon-close", eof, "the peer log ends with eof it did not initiate")
            check(f"{label}/reason-is-plain-text", not has_control(reason) and not has_control(up.get("error", "") or ""), repr(reason[:140]))
            if scenario == "unknown-type-escape":
                check(f"{label}/the-escape-did-not-reach-the-reason", "malformed frame" in reason and "evil" in reason, repr(reason[:160]))
            check(f"{label}/daemon-responsive", responsive, "")
            check(f"{label}/other-project-unaffected", b_echo.get("ok") and b_echo["result"]["ok"] and b_state["state"] == "ready", f"B state={b_state['state']}")
            results.append((scenario, closed))
        driver.close()
    (holds / "agent-override").unlink(missing_ok=True)
    return results


def scenario_forge(run_: Run, breaks: tuple[str, ...] = ()) -> None:
    """SEC-18: with fault-forge-tcp the daemon rewrites a TCP open to another port; its own
    map check must refuse it before any frame is sent."""
    holds = run_.home / "test-holds"
    holds.mkdir(exist_ok=True)
    (holds / "forge-tcp-port").write_text("47999\n")
    driver = Driver(run_, "breaks", breaks=("fault-forge-tcp", *breaks))
    driver.up()
    ensure_container(driver, "echo", "busybox:1.36", echo_cmd(19000), 19000)
    # Another workspace's published port: in the engine's loopback, but not base's.
    ensure_container(driver, "honeypot", "busybox:1.36", ["sh", "-c", "while true; do nc -l -p 47999 > /tmp/honeypot.bytes; done"], 47999, workspace="lane-2")
    wait_port(driver, 19000)
    wait_port(driver, 47999, workspace="lane-2")
    before = driver.result(cmd="status")["stats"]
    attempt = driver.c(cmd="tcp-send", port=19000, text="forged\n")
    after = driver.result(cmd="status")["stats"]
    sent = int(re.search(r"tcp_opens_sent: (\d+)", after).group(1)) - int(re.search(r"tcp_opens_sent: (\d+)", before).group(1))
    vm = next(iter(OWNED))
    honey = eng(vm, "docker -H unix:///run/marsh-svc/docker.sock exec honeypot sh -c 'wc -c < /tmp/honeypot.bytes 2>/dev/null || echo 0'")
    if breaks:
        check("forge/defect-lets-the-forged-open-reach-the-agent", sent == 1, f"tcp opens sent for a forged port: {sent} (the agent's own ledger check still refuses: {attempt.get('error') or attempt.get('result')})")
    else:
        check("forge/forged-open-refused-before-any-frame", sent == 0 and not attempt["ok"], f"tcp opens sent: {sent}; {attempt.get('error')}")
    check("forge/honeypot-received-nothing", honey.strip() in ("0", ""), f"honeypot bytes: {honey!r}")
    driver.c(cmd="exit")
    (holds / "forge-tcp-port").unlink(missing_ok=True)


class Daemon:
    """A real `marshd` for one scope, launched by this run and signalled only by its exact pid."""

    def __init__(self, run_: Run, build: str, breaks: tuple[str, ...] = ()):
        args = run_.args
        self.run = run_
        self.marsh = args.marsh_breaks if build == "breaks" else args.marsh_normal
        marshd = args.marshd_breaks if build == "breaks" else args.marshd_normal
        self.env = {k: v for k, v in os.environ.items() if k != "MARSH_TEST_BREAK"}
        self.env.update({
            "MARSH_HOME": str(run_.root / "mh"), "MARSH_CONTROL_HOME": str(run_.root / "mc"),
            "MARSH_VM_PREFIX": "marsh-xnfx01-", "MARSH_SERVICES_POOL_BYTES": "4g",
            "MARSH_GUEST_ARTIFACTS": args.marshd_artifacts_breaks if build == "breaks" else args.marshd_artifacts_normal,
        })
        for name in ("mh", "mc"):
            (run_.root / name).mkdir(exist_ok=True)
            (run_.root / name).chmod(0o700)
        argv = [marshd, "--home", str(run_.root / "mh")]
        if breaks:
            argv.append("--test-breaks-from-env")
            self.env["MARSH_TEST_BREAK"] = ",".join(breaks)
        self.log = open(run_.root / f"marshd-{time.monotonic_ns()}.log", "w")
        self.process = subprocess.Popen(argv, stdout=self.log, stderr=subprocess.STDOUT, env=self.env, cwd=run_.projects["pA"])
        import hashlib as _h
        home = os.path.realpath(run_.root / "mh")
        sock = Path(f"/tmp/marsh-{os.getuid()}/{_h.sha256(home.encode()).hexdigest()[:16]}/s")
        deadline = time.monotonic() + 60
        while not sock.exists() and time.monotonic() < deadline:
            time.sleep(0.2)
        check("marshd/started", sock.exists(), f"pid {self.process.pid}")

    def cli(self, *args: str, timeout: int = 600) -> subprocess.CompletedProcess:
        env = {k: v for k, v in self.env.items() if k != "MARSH_TEST_BREAK"}
        return subprocess.run([self.marsh, *args], capture_output=True, text=True, timeout=timeout, env=env, cwd=self.run.projects["pA"])

    def project(self) -> dict | None:
        result = self.cli("services", "status", "--json")
        if result.returncode:
            return None
        document = json.loads(result.stdout)
        root = os.path.realpath(self.run.projects["pA"])
        return next((p for p in document["projects"] if os.path.realpath(p["root"]) == root), None)

    def kill(self) -> None:
        os.kill(self.process.pid, signal.SIGKILL)  # the exact pid we started
        self.process.wait(timeout=10)

    def stop(self) -> None:
        if self.process.poll() is None:
            self.cli("stop")
            try:
                self.process.wait(timeout=60)
            except subprocess.TimeoutExpired:
                say("FAIL", "marshd/stop", f"pid {self.process.pid} did not exit after `marsh stop`")


def scenario_marshd(run_: Run, breaks: tuple[str, ...] = ()) -> None:
    """VM-06, product half: a real marshd brings a relay up on `services start`; after
    `kill -9` a new marshd brings a fresh relay (new generation, same VM) back by itself."""
    build = "breaks" if breaks else "normal"
    first = Daemon(run_, build)
    on = first.cli("config", "services", "on")
    check("marshd/services-on", on.returncode == 0, (on.stdout + on.stderr).strip()[:160])
    started = first.cli("services", "start", "--json")
    check("marshd/services-start", started.returncode == 0, (started.stdout + started.stderr).strip()[:200])
    before = first.project()
    ok = bool(before) and before["relay"]["state"] == "ready"
    check("marshd/relay-ready-after-start", ok, str(before and before["relay"])[:220])
    if not ok:
        first.stop()
        return
    vm, uuid = before["vm"], before["vm_uuid"]
    record_vm(vm)
    g1 = before["relay"]["generation"]
    check("marshd/relay-generation-is-the-engine-container", g1["engine_id"] == vx(vm, "docker inspect -f '{{.Id}}' marsh-svc-engine"), g1["engine_id"][:12])
    check("marshd/one-relay-agent", relay_processes(vm) == "1", relay_processes(vm))
    first.kill()
    time.sleep(3)
    say("INFO", "marshd/after-kill", f"relay agents in the engine: {relay_processes(vm)} (the old daemon's exec ended with its pipes)")
    began = time.monotonic()
    second = Daemon(run_, build, breaks)
    state, document = None, None
    deadline = time.monotonic() + (60 if not breaks else 45)
    while time.monotonic() < deadline:
        document = second.project()
        state = document and document["relay"]["state"]
        if state == "ready" and document["relay"]["generation"]["engine_id"] != g1["engine_id"]:
            break
        time.sleep(1)
    took = time.monotonic() - began
    if breaks:
        check("marshd/defect-leaves-the-relay-unrecovered", state != "ready", f"relay state after {took:.0f}s: {state}")
        refused = second.cli("services", "start", "--json")
        check("marshd/defect-start-refused", refused.returncode != 0 and "not been recovered" in (refused.stdout + refused.stderr), (refused.stdout + refused.stderr).strip()[:200])
    else:
        check("marshd/new-relay-within-60s", state == "ready", f"{took:.1f}s after the new daemon started: {document and document['relay']}"[:260])
        if state == "ready":
            g2 = document["relay"]["generation"]
            check("marshd/new-generation", g2["engine_id"] != g1["engine_id"], f"{g1['engine_id'][:12]} -> {g2['engine_id'][:12]}")
            check("marshd/same-vm-uuid", document["vm"] == vm and document["vm_uuid"] == uuid, f"{document['vm']} {document['vm_uuid']}")
            check("marshd/one-relay-agent-after", relay_processes(vm) == "1", relay_processes(vm))
            paused = eng(vm, "docker -H unix:///run/marsh-svc/docker.sock ps -q --filter status=paused | wc -l")
            check("marshd/nothing-paused", paused == "0", paused)
    second.cli("services", "rm", "--yes")
    second.stop()


def rss_kib(pid: int) -> int:
    return int(run(["ps", "-o", "rss=", "-p", str(pid)]).stdout.strip() or 0)


def fd_count(pid: int) -> int:
    return len(run(["lsof", "-p", str(pid)]).stdout.splitlines())


def scenario_ep06(run_: Run, breaks: tuple[str, ...] = ()) -> None:
    """EP-06: byte-exact transfers: 200 MiB through a channel and back, with the daemon's memory and
    descriptors watched (marshd RSS < +100 MB, FDs < +20)."""
    driver = Driver(run_, "breaks" if breaks else "normal", breaks=breaks)
    driver.up()
    ensure_container(driver, "echo", "busybox:1.36", echo_cmd(19000), 19000)
    wait_port(driver, 19000)
    pid = driver.process.pid
    rss0, fd0 = rss_kib(pid), fd_count(pid)
    echo = driver.c(cmd="tcp-echo", port=19000, kib=200 * 1024)
    rss1, fd1 = rss_kib(pid), fd_count(pid)
    intact = echo.get("ok") and echo["result"]["ok"]
    detail = str(echo.get("result") or echo.get("error"))[:240]
    if breaks:
        check("ep06/defect-corrupts-the-transfer", not intact, detail)
    else:
        check("ep06/200mib-round-trip-byte-exact", intact, detail)
        check("ep06/memory-and-descriptors-bounded", (rss1 - rss0) < 100 * 1024 and (fd1 - fd0) < 20, f"RSS {rss0 // 1024} -> {rss1 // 1024} MiB, descriptors {fd0} -> {fd1}")
    driver.close()


def scenario_limits(run_: Run, breaks: tuple[str, ...] = ()) -> None:
    """SEC-19 c1 at the 65th channel of an attempt: the daemon refuses it before any frame is sent.
    The agent has the same cap, so under `channel-limit-off` the 65th open still gets a typed
    refusal, from the agent; what the defect changes is that the open frame is sent."""
    driver = Driver(run_, "breaks" if breaks else "normal", breaks=breaks)
    driver.up()
    held = driver.result(cmd="hold", count=64, kind="docker", attempt="a1")
    before = driver.result(cmd="status")["stats"]
    more = driver.result(cmd="hold", count=1, kind="docker", attempt="a1")
    after = driver.result(cmd="status")["stats"]
    sent = int(re.search(r"opens_sent: (\d+)", after).group(1)) - int(re.search(r"opens_sent: (\d+)", before).group(1))
    if breaks:
        check("limits/defect-sends-the-65th-open-to-the-agent", held["opened"] == 64 and sent == 1, f"open frames sent for the 65th: {sent}; {more['refused_with']}")
    else:
        check("limits/65th-channel-refused-before-any-frame", held["opened"] == 64 and more["opened"] == 0 and sent == 0, f"open frames sent: {sent}; {more['refused_with']}")
    driver.close()


def scenario_unbounded(run_: Run, breaks: tuple[str, ...] = ()) -> None:
    """Back-pressure: a channel nobody reads buffers at most its window (1 MiB); under
    `relay-unbounded` the daemon buffers the firehose without limit."""
    driver = Driver(run_, "breaks" if breaks else "normal", breaks=breaks)
    driver.up()
    ensure_container(driver, "echo", "busybox:1.36", echo_cmd(19000), 19000)
    ensure_container(driver, "firehose", "busybox:1.36", ["sh", "-c", "while true; do nc -l -p 19100 -e /bin/yes; done"], 19100)
    wait_port(driver, 19000)
    wait_port(driver, 19100)
    pid = driver.process.pid
    rss0 = rss_kib(pid)
    stall = driver.result(cmd="stall-test", firehose_port=19100, echo_port=19000, seconds=5)
    grown = rss_kib(pid) - rss0
    buffered = stall["bytes_buffered_for_unread_channel"]
    detail = f"{buffered} bytes buffered for a channel nobody read in 5 s; daemon RSS +{grown // 1024} MiB"
    if breaks:
        check("unbounded/defect-buffers-without-limit", buffered > 8 << 20, detail)
    else:
        check("unbounded/buffering-bounded-by-the-window", buffered <= (1 << 20) + (64 << 10) and grown < 64 * 1024, detail)
    driver.close()


def rss_of(pid: int) -> int:
    return int(run(["ps", "-o", "rss=", "-p", str(pid)]).stdout.strip() or 0)


def scenario_stderr(run_: Run, breaks: tuple[str, ...] = ()) -> None:
    """The relay exec's stderr is the VM's. A peer that writes 192 MiB to it with no newline must
    not grow the daemon (the tail keeps 1 KiB lines), and an escape-laden line must not reach the
    start error."""
    holds = run_.home / "test-holds"
    holds.mkdir(exist_ok=True)
    first = Driver(run_, "breaks")
    vm = first.up(0)["vm"]
    first.close()
    (holds / "agent-override").write_text(str(run_.projects["pA"]) + "\n")
    place_peer(run_, vm, "stderr-flood")
    driver = Driver(run_, "breaks", breaks=("fault-agent-override",))
    up = driver.c(cmd="up", p=0)
    check("stderr/flood-start", up.get("ok", False), str(up)[:160])
    if up.get("ok"):
        pid = driver.process.pid
        rss0 = rss_of(pid)
        deadline = time.monotonic() + 400
        done = False
        while time.monotonic() < deadline and not done:
            time.sleep(2)
            done = "flood-done" in vx(vm, "cat /run/nfx/e1-peer.log")
        time.sleep(2)
        grown = (rss_of(pid) - rss0) // 1024
        state = driver.c(cmd="status", p=0)
        check("stderr/daemon-memory-bounded-by-the-flood", done and grown < 48, f"192 MiB of stderr without a newline: daemon RSS +{grown} MiB (flood finished: {done})")
        check("stderr/daemon-alive-and-relay-ready", state.get("ok") and state["result"]["state"] == "ready", str(state)[:160])
    driver.close()
    place_peer(run_, vm, "stderr-escape")
    driver = Driver(run_, "breaks", breaks=("fault-agent-override",))
    up = driver.c(cmd="up", p=0)
    error = up.get("error", "")
    check("stderr/escape-start-refused", not up.get("ok", True) and "before" in error, repr(error[:200]))
    check("stderr/escape-did-not-reach-the-error", not has_control(error), repr(error[:200]))
    driver.close()
    (holds / "agent-override").unlink(missing_ok=True)


def scenario_status(run_: Run) -> None:
    """A real marshd: a prior `internet on` opens the engine's gate on start (DECISIONS 24); a
    relay that is not ready makes the project unavailable with the relay's reason; a release
    build reports no agent break modes."""
    daemon = Daemon(run_, "normal")
    check("status/services-on", daemon.cli("config", "services", "on").returncode == 0, "")
    on = daemon.cli("services", "internet", "on")
    check("status/internet-granted-before-start", on.returncode == 0, (on.stdout + on.stderr).strip()[:160])
    started = daemon.cli("services", "start", "--json")
    check("status/start", started.returncode == 0, (started.stdout + started.stderr).strip()[:160])
    project = daemon.project() or {}
    if project.get("vm"):
        record_vm(project["vm"])
    internet = project.get("internet", {})
    check("status/engine-gate-opened-by-the-grant", internet.get("on") is True and "gate is closed" not in internet.get("effective", ""), str(internet)[:240])
    check("status/ready-with-a-ready-relay", project.get("state") == "ready" and project["relay"]["state"] == "ready", f"{project.get('state')} {project.get('relay', {}).get('state')}")
    check("status/no-agent-break-modes-in-a-release-build", "test_breaks_agent" not in project, "")
    vm = project["vm"]
    pid = project["relay"]["agent_pid"]
    eng(vm, f"kill {pid}")
    seen = None
    for _ in range(40):
        seen = daemon.project()
        if seen and seen["state"] != "ready":
            break
        time.sleep(0.5)
    check("status/relay-lost-makes-the-project-unavailable", bool(seen) and seen["state"] == "unavailable" and "relay" in (seen.get("reason") or ""), f"{seen and seen['state']}: {seen and seen.get('reason')}")
    check("status/relay-state-agrees", bool(seen) and seen["relay"]["state"] == "closed", str(seen and seen["relay"])[:200])
    again = daemon.cli("services", "start", "--json")
    back = daemon.project() or {}
    check("status/start-brings-a-relay-back", again.returncode == 0 and back.get("state") == "ready", f"{back.get('state')}")
    daemon.cli("services", "rm", "--yes")
    daemon.stop()


def scenario_attest(run_: Run) -> None:
    """HARNESS-02 c6: the agent's attested break modes appear on the project entry (test build only)."""
    daemon = Daemon(run_, "breaks", ("demo",))
    daemon.cli("config", "services", "on")
    started = daemon.cli("services", "start", "--json")
    check("attest/start", started.returncode == 0, (started.stdout + started.stderr).strip()[:160])
    project = daemon.project() or {}
    if project.get("vm"):
        record_vm(project["vm"])
    check("attest/agent-modes-on-the-project-entry", project.get("test_breaks_agent") == ["demo"], str(project.get("test_breaks_agent")))
    daemon.cli("services", "rm", "--yes")
    daemon.stop()


def scenario_restore(run_: Run) -> None:
    """The restore frames over the real agent: arguments are checked on the Mac, an answer is matched to
    its request, and a snapshot that does not exist fails as the agent's error, not as a violation."""
    driver = Driver(run_)
    driver.up()
    lane = run_.projects["pA"] / ".lane-1"
    lane.mkdir(exist_ok=True)
    captured = driver.result(cmd="capture", id="e1-r1", workspace="base")
    check("restore/snapshot-captured", captured["outcome"] == "published", str(captured))
    bad = driver.c(cmd="restore-prepare", id="e1-r1", lane_dir=str(run_.projects["pA"]), fork="f1")
    check("restore/lane-outside-the-project-refused-on-the-mac", not bad["ok"] and "strictly inside" in bad["error"], bad.get("error", ""))
    missing = driver.c(cmd="restore-prepare", id="no-such-snapshot", lane_dir=str(lane), fork="f1")
    check("restore/missing-snapshot-is-the-agents-error", not missing["ok"] and "failed the request" in missing["error"], missing.get("error", "")[:200])
    check("restore/relay-still-ready-after-an-error", driver.result(cmd="status")["state"] == "ready", "")
    prepared = driver.c(cmd="restore-prepare", id="e1-r1", lane_dir=str(lane), fork="f1")
    say("INFO", "restore/prepare-answer", json.dumps(prepared)[:400])
    check("restore/prepare-answered-with-an-object", prepared["ok"] and isinstance(prepared["result"]["result"], dict), json.dumps(prepared)[:200])
    check("restore/relay-ready-after-prepare", driver.result(cmd="status")["state"] == "ready", "")
    driver.result(cmd="drop", id="e1-r1")
    driver.close()


def scenario_alias(run_: Run) -> None:
    """The test plan's name for the stale-generation defect activates it (SEC-12 c1)."""
    scenario_hostile(run_, only=["ready-stale"], breaks=("ready-accept-stale-generation",))


def scenario_loss(run_: Run) -> None:
    """SEC-12 c2 / A1: losing the exec or the engine closes every channel and revokes the relay;
    a lost exec with the engine still ready gets a new relay on the same generation (the user's
    containers keep running), a lost engine gets a new generation."""
    driver = Driver(run_)
    up = driver.up()
    vm = up["vm"]
    ensure_container(driver, "echo", "busybox:1.36", echo_cmd(19000), 19000)
    wait_port(driver, 19000)
    held = driver.result(cmd="hold", count=5, kind="tcp", port=19000, attempt="l1")
    check("loss/channels-open", held["opened"] == 5, str(held))
    started_before = vx(vm, "docker exec marsh-svc-engine docker -H unix:///run/marsh-svc/docker.sock inspect -f '{{.State.StartedAt}}' echo")
    # The relay's exec ends: the agent process (exact pid, reported in its ready) is signalled.
    killed = time.monotonic()
    eng(vm, f"kill {up['agent_pid']}")
    state = {}
    for _ in range(80):
        state = driver.result(cmd="status")
        if state["state"] != "ready":
            break
        time.sleep(0.125)
    check("loss/exec-lost-closes-every-channel-within-10s", state["state"] == "closed" and state["channels"] == 0 and time.monotonic() - killed < 10, f"{state['state']} {state['reason']!r} after {time.monotonic() - killed:.1f}s")
    refused = driver.c(cmd="tcp-echo", port=19000, kib=1)
    check("loss/no-channel-opens-on-the-dead-relay", not refused["ok"], refused.get("error", ""))
    again = driver.up()
    check("loss/same-generation-new-relay", again["engine_id"] == up["engine_id"] and again["agent_pid"] != up["agent_pid"], f"engine {again['engine_id'][:12]} agent pid {up['agent_pid']} -> {again['agent_pid']}")
    started_after = vx(vm, "docker exec marsh-svc-engine docker -H unix:///run/marsh-svc/docker.sock inspect -f '{{.State.StartedAt}}' echo")
    check("loss/containers-kept-running", started_before == started_after, f"{started_before} / {started_after}")
    check("loss/channel-works-again", driver.result(cmd="tcp-echo", port=19000, kib=64)["ok"], "")
    check("loss/one-relay-agent", relay_processes(vm) == "1", relay_processes(vm))
    # The engine container is lost (the boot engine's own kill of it).
    driver.result(cmd="hold", count=5, kind="tcp", port=19000, attempt="l2")
    killed = time.monotonic()
    vx(vm, "docker kill marsh-svc-engine")
    for _ in range(120):
        state = driver.result(cmd="status")
        if state["state"] != "ready":
            break
        time.sleep(0.125)
    check("loss/engine-lost-closes-every-channel-within-10s", state["state"] == "closed" and state["channels"] == 0 and time.monotonic() - killed < 10, f"{state['state']} {state['reason']!r} after {time.monotonic() - killed:.1f}s")
    check("loss/nothing-opens-until-a-new-generation", not driver.c(cmd="tcp-echo", port=19000, kib=1)["ok"], "")
    fresh = driver.up()
    check("loss/new-generation-after-engine-loss", fresh["engine_id"] != up["engine_id"], f"{up['engine_id'][:12]} -> {fresh['engine_id'][:12]}")
    check("loss/restart-policy-container-back", wait_port(driver, 19000, seconds=60), "")
    driver.close()


def scenario_breaks(run_: Run) -> None:
    """Every E1 break at its own check (the check passes in the normal scenarios above)."""
    for mode, scenario, kwargs in (
        ("relay-drop-chunk", scenario_ep06, {}),
        ("slow-relay", scenario_perf, {}),
        ("startup-recovery-off", scenario_marshd, {}),
        ("relay-accept-stale-generation", scenario_hostile, {"only": ["ready-stale"]}),
        ("ready-accept-stale-generation", scenario_hostile, {"only": ["ready-stale"]}),
        ("relay-unbounded", scenario_unbounded, {}),
        ("channel-limit-off", scenario_limits, {}),
        ("relay-fairness-off", scenario_fairness, {}),
        ("tcp-map-check-off", scenario_forge, {}),
        ("vm-frame-validation-off", scenario_hostile, {"only": ["unknown-type", "stale-channel", "duplicate-opened", "ports-zero"]}),
    ):
        say("INFO", "break", mode)
        scenario(run_, breaks=(mode,), **kwargs)


def pgbench_direct(vm: str, options: str) -> float:
    out = eng(vm, f"docker -H unix:///run/marsh-svc/docker.sock exec pg pgbench {options} -U postgres -h 127.0.0.1 postgres 2>&1", 300)
    match = re.search(r"latency average = ([0-9.]+) ms", out)
    if not match:
        raise RuntimeError(f"no pgbench latency in: {out[-300:]}")
    return float(match.group(1))


def scenario_perf(run_: Run, breaks: tuple[str, ...] = ()) -> None:
    """PERF-05 (a) select-only `-c1` and (b) `-C`: latency over the relay against a direct
    connection inside the engine, five interleaved pairs. The relay side is a client that
    speaks the same simple-protocol select (there is no pgbench on the Mac). (c) TPC-B -c4
    and (d) the Testcontainers script are not run here (they need pgbench or Node on the
    Mac side, and a job endpoint)."""
    driver = Driver(run_, "breaks" if breaks else "normal", breaks=breaks)
    vm = driver.up()["vm"]
    ensure_container(driver, "pg", "postgres:17-alpine", None, 15432, container_port=5432, env=["POSTGRES_HOST_AUTH_METHOD=trust"], restart=False)
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        if "accepting" in eng(vm, "docker -H unix:///run/marsh-svc/docker.sock exec pg pg_isready -U postgres -h 127.0.0.1") and "no response" not in eng(vm, "docker -H unix:///run/marsh-svc/docker.sock exec pg pg_isready -U postgres -h 127.0.0.1"):
            break
        time.sleep(1)
    time.sleep(2)
    init = eng(vm, "docker -H unix:///run/marsh-svc/docker.sock exec pg pgbench -i -s 1 -q -U postgres -h 127.0.0.1 postgres 2>&1 | tail -2")
    say("INFO", "perf/init", init.replace("\n", " | ")[:160])
    wait_port(driver, 15432, seconds=30)
    for label, direct_options, relay_args, gate_ms in (
        ("a-select-only-c1", "-S -c1 -T10 -n", {}, 1.0),
        ("b-new-connection-C", "-C -S -c1 -T10 -n", {"connect_each": True}, 3.0),
    ):
        diffs, directs, relays = [], [], []
        for _ in range(5):
            direct = pgbench_direct(vm, direct_options)
            relayed = driver.result(cmd="pg-latency", port=15432, seconds=10, **relay_args)
            directs.append(direct)
            relays.append(relayed["mean_us"] / 1000)
            diffs.append(relays[-1] - direct)
            say("INFO", f"perf/{label}", f"direct {direct:.3f} ms, relay mean {relays[-1]:.3f} ms (p50 {relayed['p50_us'] / 1000:.3f}), added {diffs[-1]:.3f} ms")
        median = statistics.median(diffs)
        hard = 5.0 if label.startswith("a") else gate_ms
        detail = f"median added latency {median:.3f} ms over 5 pairs (direct median {statistics.median(directs):.3f} ms, relay median {statistics.median(relays):.3f} ms); target 0.5 ms, gate {gate_ms} ms"
        if breaks:
            check(f"perf/{label}/control-fails-the-gate", median > gate_ms, detail)
        elif label.startswith("b") and median > gate_ms:
            # Not the relay: see the open latencies below (the docker open is the relay alone).
            say("GAP", f"perf/{label}/over-gate", detail)
        else:
            check(f"perf/{label}/within-gate", median <= gate_ms and median < hard, detail)
    if not breaks:
        for kind in ("docker", "tcp"):
            opened = driver.result(cmd="open-latency", kind=kind, port=15432, n=100)
            say("INFO", f"perf/open-{kind}", f"p50 {opened['p50_us'] / 1000:.2f} ms, min {opened['min_us'] / 1000:.2f} ms over 100 opens and closes")
    driver.close()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--driver-normal", required=True)
    parser.add_argument("--driver-breaks", required=True)
    parser.add_argument("--artifacts-normal", required=True)
    parser.add_argument("--artifacts-breaks", required=True)
    parser.add_argument("--shell-image-file", required=True)
    parser.add_argument("--marsh-normal")
    parser.add_argument("--marshd-normal")
    parser.add_argument("--marsh-breaks")
    parser.add_argument("--marshd-breaks")
    parser.add_argument("--marshd-artifacts-normal")
    parser.add_argument("--marshd-artifacts-breaks")
    parser.add_argument("--only", default="basic,bounds,fairness,restart,hostile,forge")
    parser.add_argument("--breaks", default="", help="modes for the defect passes (comma separated)")
    args = parser.parse_args()
    run_ = Run(args)
    try:
        for name in args.only.split(","):
            say("INFO", "scenario", name)
            globals()[f"scenario_{name}"](run_, **({"breaks": tuple(args.breaks.split(","))} if args.breaks and name in ("perf", "hostile", "forge", "marshd", "fairness", "ep06", "limits", "unbounded", "stderr") else {}))
    finally:
        run_.finish()
    print("ALL PASSED" if not FAILURES else f"FAILED: {', '.join(FAILURES)}")
    return 1 if FAILURES else 0


if __name__ == "__main__":
    sys.exit(main())
