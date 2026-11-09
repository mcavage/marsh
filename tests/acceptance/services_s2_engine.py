#!/usr/bin/env python3
"""Live checks for the services engine (S2), against real sbx.

Drives `marsh-sbx`'s `services_engine_drive` example (the daemon-side engine
lifecycle) against hand-made sandboxes until S1's `ensure_services_vm` exists.
Two builds are needed: a normal driver and guest `marsh` (the normal scenarios),
and a driver and guest `marsh` built with `--features test-breaks` (the holds
and the break modes). Their artifact directories each hold the pinned engine tar
and pin plus that build's `marsh-linux-arm64`.

The whole run holds the exclusive VM flock (every `sbx` command here can create,
start, stop or remove a VM). Sandboxes are named `nfx-s2-<run id>-*`, recorded as
this run's: the name **and the sandbox UUID** are recorded at create, the UUID is
verified before a scenario uses a VM and before it is removed, and the policy rule
IDs this run added are the only rules removed. A name that already exists aborts
the run, a removal that fails (or that cannot be proven) is an error and keeps its
record, and nothing is ever killed or removed by pattern: processes are signalled
only by an exact recorded pid.

  services_s2_engine.py --driver-normal D --artifacts-normal A \
      --driver-breaks D --artifacts-breaks A --project DIR [--only NAME[,NAME]]

Every check prints `PASS name: detail` or `FAIL name: detail`. The exit status is
non-zero if any check failed.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import secrets
import subprocess
import sys
import tempfile
import threading
import time

sys.path.insert(0, str(Path.home() / "dev/narwhal-fork-spike/harness"))
import vmlock  # noqa: E402

FAILURES: list[str] = []
OWNED: dict[str, "Sandbox"] = {}
PROD_SOCKET = "unix:///run/marsh-svc/docker.sock"
STATE = "/var/lib/docker/marsh-svc"


def say(status: str, name: str, detail: str = "") -> None:
    print(f"{status} {name}: {detail}", flush=True)
    if status == "FAIL":
        FAILURES.append(name)


def check(name: str, condition: bool, detail: str = "") -> bool:
    say("PASS" if condition else "FAIL", name, detail)
    return condition


def run(argv: list[str], timeout: int = 180, stdin: str | None = None) -> subprocess.CompletedProcess:
    return subprocess.run(argv, capture_output=True, text=True, timeout=timeout, input=stdin)


def vx(vm: str, command: str, timeout: int = 120) -> str:
    """Root shell command in the VM (the boot engine's docker is on PATH)."""
    result = run(["sbx", "exec", "-u", "root", vm, "sh", "-c", command], timeout)
    return (result.stdout + result.stderr).strip()


def eng(vm: str, command: str, timeout: int = 180) -> str:
    """A shell command inside the engine container."""
    result = run(["sbx", "exec", "-u", "root", vm, "docker", "exec", "marsh-svc-engine", "sh", "-c", command], timeout)
    return (result.stdout + result.stderr).strip()


def prod(vm: str, command: str, timeout: int = 180) -> str:
    """A production-engine docker command, run in the engine container."""
    return eng(vm, f"docker -H {PROD_SOCKET} {command}", timeout)


def inventory() -> dict[str, dict]:
    """`sbx ls --json` by name: {name: {id, status}}. Reading it never starts a VM."""
    result = run(["sbx", "ls", "--json"])
    if result.returncode:
        raise RuntimeError(f"sbx ls failed: {result.stderr.strip()}")
    return {row["name"]: row for row in json.loads(result.stdout or "[]")}


def vm_state(name: str) -> str:
    """`running`/`stopped`/`absent`."""
    row = inventory().get(name)
    return "absent" if row is None else ("running" if row["status"] == "running" else "stopped")


class Sandbox:
    def __init__(self, name: str, template: str, project: Path):
        self.name, self.template, self.project = name, template, project
        self.uuid: str | None = None
        self.rule_id: str | None = None

    def create(self) -> None:
        if self.name in inventory():
            raise RuntimeError(f"{self.name} already exists; not ours, aborting")
        OWNED[self.name] = self
        result = run(["sbx", "create", "--name", self.name, "--pull", "never", "--skills", "off",
                      "--template", self.template, "shell", str(self.project)], 600)
        row = inventory().get(self.name)
        if result.returncode or row is None:
            raise RuntimeError(f"create {self.name}: {result.stderr.strip()}")
        self.uuid = row["id"]
        say("INFO", "owned-sandbox", f"{self.name} uuid={self.uuid}")

    def verify(self) -> None:
        """The sandbox under our name is still the one we created."""
        row = inventory().get(self.name)
        if row is None or row["id"] != self.uuid:
            raise RuntimeError(f"{self.name} is not our sandbox any more (recorded {self.uuid}, now {row and row['id']}); refusing to use it")

    def deny_all(self) -> None:
        self.verify()
        result = run(["sbx", "policy", "deny", "network", "--sandbox", self.name, "**"])
        added = [word for word in result.stdout.split() if len(word) == 36 and word.count("-") == 4]
        check(f"{self.name}/policy-rule-added", result.returncode == 0 and "Added rule" in result.stdout and len(added) == 1, result.stdout.strip()[:160])
        self.rule_id = added[0] if added else None
        verdict = run(["sbx", "policy", "check", "network", "--sandbox", self.name, "registry-1.docker.io"])
        text = (verdict.stdout + verdict.stderr).lower()
        check(f"{self.name}/policy-denies-registry", "deny" in text or "denied" in text, text.strip()[:200])

    def remove(self) -> bool:
        """Remove what this run created, and prove it. Returns False (and keeps the record) on any failure."""
        ok = True
        if self.uuid is not None:
            row = inventory().get(self.name)
            if row is not None and row["id"] != self.uuid:
                say("FAIL", f"cleanup/{self.name}", f"a different sandbox ({row['id']}) holds our name (ours was {self.uuid}); not removing it")
                return False
        if self.rule_id is not None:
            result = run(["sbx", "policy", "rm", "network", "--force", "--sandbox", self.name, "--id", self.rule_id])
            if result.returncode:
                say("FAIL", f"cleanup/{self.name}/rule", f"removing our rule {self.rule_id} failed: {(result.stderr or result.stdout).strip()[:200]}")
                ok = False
            else:
                self.rule_id = None
        if self.uuid is not None:
            result = run(["sbx", "rm", "--force", self.name], 180)
            gone = self.name not in inventory()
            if result.returncode or not gone:
                say("FAIL", f"cleanup/{self.name}", f"sbx rm failed or the sandbox is still listed: {(result.stderr or result.stdout).strip()[:200]}")
                ok = False
            else:
                say("INFO", f"cleanup/{self.name}", f"removed uuid {self.uuid}")
                self.uuid = None
        if ok:
            OWNED.pop(self.name, None)
        return ok


class Build:
    """A driver and the artifact directory it ships to the VM."""

    def __init__(self, driver: str, artifacts: Path, project: Path):
        self.driver, self.artifacts, self.project = driver, artifacts, project


class Driver:
    """The engine lifecycle driver, with its JSON event lines kept and timed."""

    def __init__(self, build: Build, vm: str, extra: list[str] | None = None, breaks: list[str] | None = None, pool: str = "4g"):
        self.events: list[dict] = []
        argv = [build.driver, "--vm", vm, "--project", str(build.project), "--artifacts", str(build.artifacts),
                "--pool-bytes", pool, *(extra or [])]
        env = {key: value for key, value in os.environ.items() if key != "MARSH_TEST_BREAK"}
        if breaks:
            argv.append("--test-breaks-from-env")
            env["MARSH_TEST_BREAK"] = ",".join(breaks)
        self.process = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, env=env)
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self) -> None:
        for line in self.process.stdout:
            try:
                event = json.loads(line)
            except ValueError:
                event = {"raw": line.strip()}
            self.events.append(event)

    def wait(self, predicate, timeout: float, after: int = 0) -> dict | None:
        deadline = time.monotonic() + timeout
        seen = after
        while time.monotonic() < deadline:
            for event in self.events[seen:]:
                if predicate(event):
                    return event
            seen = max(seen, len(self.events))
            time.sleep(0.05)
        return None

    def states(self) -> list[str]:
        return [e["state"] for e in self.events if "state" in e]

    def gate_events(self) -> list[dict]:
        return [e for e in self.events if e.get("probe") == "channel-gate"]

    def gate_open_t(self) -> int | None:
        """Driver-clock time (ms) the gate first opened; waits for the probe."""
        event = self.wait(lambda e: e.get("probe") == "channel-gate" and e["open"], 5)
        return event["t_ms"] if event else None

    def stop(self) -> None:
        self.process.terminate()
        try:
            self.process.wait(10)
        except subprocess.TimeoutExpired:
            self.process.kill()


def jload(text: str) -> dict:
    try:
        return json.loads(text)
    except ValueError:
        return {}


def ready(event: dict) -> bool:
    return event.get("state") == "ready"


def unavailable(event: dict) -> bool:
    return event.get("state") == "unavailable"


def pinned_id(artifacts: Path) -> str:
    return json.loads((artifacts / "svc-engine-image.json").read_text())["image_id"]


def boot_names(vm: str) -> list[str]:
    return vx(vm, "docker ps -a --format '{{.Names}}'").split()


def boot_image(vm: str) -> str:
    return vx(vm, "docker inspect --format '{{.Image}}' marsh-svc-engine")


HOSTILE_MARKER = f"{STATE}/pool/hostile-ran"
HOSTILE_ENTRYPOINT = ('ENTRYPOINT ["/bin/sh","-c","echo hostile-engine-ran > /var/lib/marsh-svc/hostile-ran; '
                      'exec /marsh/marsh --internal-services engine-up \\"$@\\"","hostile"]')


def make_hostile_engine(vm: str, pin: str) -> None:
    """A *functional* hostile engine image in the PRODUCTION engine, tagged as the engine's reference.

    It is the shipped engine image with a poison label and an entrypoint that first writes a marker
    on the VM's pool directory and then runs the real engine-up, so it would start successfully and
    leave evidence if anything ran it (SEC-11 c1)."""
    vx(vm, f"docker exec -i marsh-svc-engine docker -H {PROD_SOCKET} load < {STATE}/artifacts/svc-engine.tar")
    prod(vm, "rm -f hostile-src >/dev/null 2>&1; true")
    out = eng(vm, f"docker -H {PROD_SOCKET} create --name hostile-src {pin} >/dev/null && "
                  f"docker -H {PROD_SOCKET} commit --change 'LABEL hostile=1' --change '{HOSTILE_ENTRYPOINT}' hostile-src marsh-svc-engine:latest && "
                  f"docker -H {PROD_SOCKET} rm hostile-src >/dev/null")
    check("hostile-engine/built-in-production", "sha256:" in out, out[-120:])
    vx(vm, f"rm -f {HOSTILE_MARKER}")


def hostile_ran(vm: str) -> bool:
    return vx(vm, f"test -e {HOSTILE_MARKER} && echo yes || echo no").endswith("yes")


def clear_engine(vm: str) -> None:
    """Remove the old engine container, so a hold file seen next is from the new generation."""
    vx(vm, "docker rm -f marsh-svc-engine >/dev/null 2>&1; true")


def hold_reached(vm: str, timeout: float = 120) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if eng(vm, "test -e /run/marsh-svc/hold-reached && echo yes || echo no") == "yes":
            return True
        time.sleep(0.5)
    return False


def release(vm: str, what: str = "release") -> None:
    eng(vm, f"touch /run/marsh-svc/hold-{what}")


# --- scenarios ---------------------------------------------------------------

def scenario_fresh(ctx, sandbox: Sandbox) -> None:
    """A fresh VM, no pulls possible: the engine comes up from artifacts only."""
    build = ctx.normal
    pin = pinned_id(build.artifacts)
    sandbox.create()
    sandbox.deny_all()
    probe = vx(sandbox.name, "curl -sS -m 8 -o /dev/null -w 'code=%{http_code}' https://registry-1.docker.io/v2/ 2>/dev/null; echo \" exit=$?\"")
    check("fresh/offline-refused", "code=000" in probe and "exit=0" not in probe, probe)
    check("fresh/no-images-before", vx(sandbox.name, "docker image ls -aq | wc -l").endswith("0"))
    driver = Driver(build, sandbox.name)
    event = driver.wait(ready, 240)
    check("fresh/ready", event is not None, json.dumps(event))
    if event:
        say("INFO", "engine_ready_ms", f"{event['since_start_ms']} ms daemon-observed from start (artifact copy, docker load, pool creation included)")
        opened = driver.gate_open_t()
        check("fresh/gate-closed-until-ready", opened is not None and opened >= event["t_ms"], f"ready at t={event['t_ms']} ms, gate opened at t={opened} ms (driver clock)")
        boot_id = vx(sandbox.name, "cat /proc/sys/kernel/random/boot_id")
        engine_id = vx(sandbox.name, "docker inspect --format '{{.Id}}' marsh-svc-engine")
        check("fresh/generation-is-boot-id-and-container", event["boot_id"] == boot_id and event["engine_id"] == engine_id)
        target = driver.wait(lambda e: e.get("probe") == "relay-target", 5)
        check("fresh/relay-targets-the-production-socket", target is not None and target["container"] == "marsh-svc-engine"
              and target["socket"] == "/run/marsh-svc/docker.sock", json.dumps(target))
    say("INFO", "prod-docker-info", prod(sandbox.name, "info --format 'version={{.ServerVersion}} driver={{.Driver}} DockerRootDir={{.DockerRootDir}}'"))
    check("fresh/data-root-on-pool", prod(sandbox.name, "info --format '{{.DockerRootDir}}'") == "/pool/docker")
    check("fresh/pool-is-xfs-reflink", "xfs" in eng(sandbox.name, "findmnt -n -o FSTYPE /pool") and "reflink=1" in eng(sandbox.name, "xfs_info /pool"))
    check("fresh/boot-engine-holds-only-the-engine", boot_names(sandbox.name) == ["marsh-svc-engine"] and boot_image(sandbox.name) == pin,
          f"{boot_names(sandbox.name)} {boot_image(sandbox.name)}")
    say("INFO", "boot-docker-ps-a", vx(sandbox.name, "docker ps -a"))
    check("fresh/entitlements-off", jload(eng(sandbox.name, "cat /etc/docker/daemon.json")).get("builder", {}).get("entitlements")
          == {"network-host": False, "security-insecure": False})
    check("fresh/socket-only-in-engine", vx(sandbox.name, "test -e /run/marsh-svc/docker.sock && echo yes || echo no") == "no"
          and eng(sandbox.name, "test -S /run/marsh-svc/docker.sock && echo yes || echo no") == "yes")
    check("fresh/no-tcp-listener-in-engine", eng(sandbox.name, "cat /proc/net/tcp /proc/net/tcp6 | grep -c ' 0A ' || true") == "0")
    check("fresh/no-apt-lists-or-package-processes", eng(sandbox.name, "ls /var/lib/apt/lists | grep -vc lock || true") == "0"
          and eng(sandbox.name, "ps aux | grep -c '[a]pt\\|[d]pkg' || true") == "0")
    log = run(["sbx", "policy", "log", sandbox.name]).stdout
    blocked = [line for line in log.splitlines() if sandbox.name in line]
    check("fresh/policy-log-only-the-probe", blocked and all("registry-1.docker.io" in line for line in blocked), f"{len(blocked)} blocked line(s)")
    prod(sandbox.name, "volume create before-restart")
    eng(sandbox.name, "echo marker > /pool/docker/volumes/before-restart/_data/marker")
    check("fresh/volume-on-pool", eng(sandbox.name, "findmnt -n -o FSTYPE -T /pool/docker/volumes/before-restart/_data") == "xfs")
    driver.stop()


def scenario_restart(ctx, sandbox: Sandbox) -> None:
    """sbx stop, then start: revocation first, no wake-up, a new generation, data kept, the canary never EMPTY."""
    sandbox.verify()
    build = ctx.normal
    pin = pinned_id(build.artifacts)
    canary_dir = ctx.project / "canary-log"
    canary_dir.mkdir(exist_ok=True)
    boots = canary_dir / "boots"
    boots.write_text("")
    driver = Driver(build, sandbox.name, ["--follow"])
    first = driver.wait(ready, 120)
    check("restart/first-ready", first is not None)
    # The restart-policy canary (SVC-VM-03 c3): a restart-always container reading a volume marker.
    vx(sandbox.name, f"docker exec -i marsh-svc-engine docker -H {PROD_SOCKET} load < {STATE}/artifacts/svc-engine.tar")
    prod(sandbox.name, "volume create cv")
    eng(sandbox.name, "echo canary > /pool/docker/volumes/cv/_data/marker")
    prod(sandbox.name, f"run -d --name canary --restart always -v cv:/cv -v {canary_dir}:/log --entrypoint sh {pin} "
         "-c 'while true; do if [ -f /cv/marker ]; then echo OK >> /log/boots; else echo EMPTY >> /log/boots; fi; sleep 0.3; done'")
    time.sleep(2)
    check("restart/canary-running", "OK" in boots.read_text(), boots.read_text()[-40:])
    with boots.open("a") as handle:
        handle.write("--- stop\n")
    stopped_at = time.monotonic()
    run(["sbx", "stop", sandbox.name], 120)
    lost = driver.wait(unavailable, 60)
    check("restart/unavailable-on-stop", lost is not None, json.dumps(lost))
    # Readiness was revoked from what was known; nothing may then wake the VM.
    time.sleep(8)
    check("restart/watcher-does-not-wake-the-vm", vm_state(sandbox.name) != "running",
          f"sbx ls after {int(time.monotonic() - stopped_at)} s: {vm_state(sandbox.name)}")
    diagnostic = driver.wait(lambda e: "diagnostic" in e, 30)
    say("INFO", "restart/diagnostic", json.dumps(diagnostic))
    started = time.monotonic()
    run(["sbx", "exec", sandbox.name, "true"], 120)
    second = driver.wait(lambda e: ready(e) and e.get("engine_id") != (first or {}).get("engine_id"), 180)
    check("restart/new-generation-ready", second is not None, json.dumps(second))
    if first and second:
        say("INFO", "restart_ready_ms", f"{int((time.monotonic() - started) * 1000)} ms from `sbx exec` start to the new ready")
        check("restart/new-boot-and-engine-id", second["boot_id"] != first["boot_id"] and second["engine_id"] != first["engine_id"],
              f"boot {first['boot_id'][:8]}->{second['boot_id'][:8]} engine {first['engine_id'][:12]}->{second['engine_id'][:12]}")
    # SVC-VM-03 c3: after the stop marker the canary never reports EMPTY, and a new OK follows.
    deadline = time.monotonic() + 90
    after = []
    while time.monotonic() < deadline:
        lines = boots.read_text().splitlines()
        after = lines[lines.index("--- stop") + 1:] if "--- stop" in lines else []
        if "OK" in after:
            break
        time.sleep(1)
    check("restart/canary-ok-after-stop", "OK" in after, f"{len(after)} line(s) after the stop marker")
    check("restart/canary-never-EMPTY", "EMPTY" not in after, f"{after[:3]}...")
    check("restart/volume-and-marker-survive", "before-restart" in prod(sandbox.name, "volume ls -q").split()
          and eng(sandbox.name, "cat /pool/docker/volumes/before-restart/_data/marker") == "marker")
    check("restart/data-root-on-pool-xfs", prod(sandbox.name, "info --format '{{.DockerRootDir}}'") == "/pool/docker"
          and eng(sandbox.name, "findmnt -n -o FSTYPE /pool") == "xfs")
    check("restart/boot-engine-holds-only-the-engine", boot_names(sandbox.name) == ["marsh-svc-engine"] and boot_image(sandbox.name) == pin)
    say("INFO", "readiness-transitions", " -> ".join(f"{e['state']}@{e['t_ms']}ms" for e in driver.events if "state" in e))
    check("restart/transitions-ordered", driver.states() == ["starting", "ready", "unavailable", "starting", "ready"], str(driver.states()))
    driver.stop()
    prod(sandbox.name, "rm -f canary")


def scenario_hold(ctx, sandbox: Sandbox) -> None:
    """hold-engine-before-ready: what is reachable at the production-ready boundary (VM-03 c1, c2, c4)."""
    sandbox.verify()
    build = ctx.breaks
    pin = pinned_id(build.artifacts)
    clear_engine(sandbox.name)
    driver = Driver(build, sandbox.name, breaks=["hold-engine-before-ready"])
    reached = hold_reached(sandbox.name)
    check("hold/reached", reached)
    if reached:
        check("hold/production-engine-already-answers", prod(sandbox.name, "info --format '{{.DockerRootDir}}'") == "/pool/docker")
        check("hold/data-root-on-xfs-pool", eng(sandbox.name, "findmnt -n -o FSTYPE -T /pool/docker") == "xfs")
        check("hold/boot-engine-holds-only-the-pinned-engine", boot_names(sandbox.name) == ["marsh-svc-engine"] and boot_image(sandbox.name) == pin)
        time.sleep(3)
        gate = driver.gate_events()
        check("hold/gate-closed-and-state-starting", all(not e["open"] for e in gate) and driver.states() == ["starting"],
              f"states={driver.states()} gate={[e['open'] for e in gate]}")
        before = [e for e in driver.events if ready(e)]
        release(sandbox.name)
        event = driver.wait(ready, 60)
        check("hold/released-then-ready", event is not None and not before)
        opened = driver.gate_open_t()
        check("hold/gate-opens-only-after-ready", event is not None and opened is not None and opened >= event["t_ms"],
              f"ready t={event and event['t_ms']}, gate t={opened}")
    driver.stop()
    # Abort: the engine fails at the hold, readiness never happens.
    clear_engine(sandbox.name)
    driver = Driver(build, sandbox.name, breaks=["hold-engine-before-ready"])
    if hold_reached(sandbox.name):
        release(sandbox.name, "abort")
        lost = driver.wait(unavailable, 60)
        check("hold/abort-is-unavailable-never-ready", lost is not None and "ready" not in driver.states()
              and all(not e["open"] for e in driver.gate_events()), json.dumps(lost))
    driver.stop()


def scenario_revocation(ctx, sandbox: Sandbox) -> None:
    """Losing the production engine, the pool mount or the container revokes readiness.

    Every assertion keys on events after a cursor taken immediately before the loss is caused,
    and the gate must first be observed open, so the initial closed-gate event can never satisfy it."""
    sandbox.verify()
    for repeat in range(1, ctx.revoke_repeats + 1):
        for what, command in (
            # Signal the exact dockerd pid this engine recorded in its own pidfile.
            ("production-dockerd-killed", "docker exec marsh-svc-engine sh -c 'kill -KILL \"$(cat /run/marsh-svc/dockerd.pid)\"'"),
            ("pool-unmounted", "docker exec marsh-svc-engine umount -l /pool"),
            ("engine-container-removed", "docker rm -f marsh-svc-engine"),
        ):
            tag = f"revoke/{what}#{repeat}"
            driver = Driver(ctx.normal, sandbox.name)
            up = driver.wait(ready, 120)
            opened = driver.wait(lambda e: e.get("probe") == "channel-gate" and e["open"], 10)
            if not check(f"{tag}/ready-and-gate-open-first", up is not None and opened is not None):
                driver.stop()
                continue
            cursor = len(driver.events)
            mark = time.monotonic()
            vx(sandbox.name, command)
            lost = driver.wait(unavailable, 30, after=cursor)
            check(f"{tag}/unavailable-after-the-loss", lost is not None, json.dumps(lost))
            closed = driver.wait(lambda e: e.get("probe") == "channel-gate" and not e["open"], 10, after=cursor)
            later = [e for e in driver.events[cursor:] if e.get("probe") == "channel-gate"]
            check(f"{tag}/gate-closed-after-the-loss", closed is not None and later and later[-1]["open"] is False and "ready" not in
                  [e["state"] for e in driver.events[cursor:] if "state" in e],
                  f"{int((time.monotonic() - mark) * 1000)} ms after the loss was caused; gate events after the loss: {[e['open'] for e in later]}")
            note = driver.wait(lambda e: "diagnostic" in e, 90, after=cursor)
            say("INFO", f"{tag}/diagnostic", json.dumps(note))
            driver.stop()


def scenario_poison(ctx, sandbox: Sandbox) -> None:
    """Hostile images and containers: in the boot engine, and the production-only witness (SEC-11)."""
    sandbox.verify()
    build = ctx.normal
    pin = pinned_id(build.artifacts)
    # Boot-engine hardening: a hostile tag and a hostile container of the engine's name.
    vx(sandbox.name, "docker rm -f marsh-svc-engine >/dev/null 2>&1; "
       f"docker create --name hostile-src {pin} >/dev/null && "
       "docker commit --change 'LABEL hostile=1' hostile-src marsh-svc-engine:latest >/dev/null && "
       "docker rm hostile-src >/dev/null; docker create --name marsh-svc-engine marsh-svc-engine:latest >/dev/null")
    driver = Driver(build, sandbox.name)
    up = driver.wait(ready, 180)
    check("poison/boot-tag/ready", up is not None, json.dumps(up))
    check("poison/boot-tag/runs-the-pinned-image", boot_image(sandbox.name) == pin)
    check("poison/boot-tag/not-the-hostile-label", "hostile" not in vx(sandbox.name, "docker inspect --format '{{.Config.Labels}}' marsh-svc-engine"))
    driver.stop()
    vx(sandbox.name, "docker rmi -f marsh-svc-engine:latest >/dev/null 2>&1; true")
    # SEC-11 c1/c3: a functional marked engine image tagged as the engine's reference in the PRODUCTION engine only.
    driver = Driver(build, sandbox.name)
    driver.wait(ready, 120)
    make_hostile_engine(sandbox.name, pin)
    check("poison/production/marked-image-in-production",
          "marsh-svc-engine:latest" in prod(sandbox.name, "image ls --format '{{.Repository}}:{{.Tag}}'"))
    driver.stop()
    driver = Driver(build, sandbox.name)  # a new generation: the engine is replaced
    up = driver.wait(ready, 180)
    check("poison/production/ready-again", up is not None, json.dumps(up))
    check("poison/production/boot-engine-has-no-marked-image",
          vx(sandbox.name, "docker image inspect marsh-svc-engine:latest >/dev/null 2>&1 && echo present || echo absent").endswith("absent"))
    check("poison/production/boot-runs-the-pinned-image", boot_names(sandbox.name) == ["marsh-svc-engine"] and boot_image(sandbox.name) == pin)
    check("poison/production/hostile-engine-never-ran", not hostile_ran(sandbox.name)
          and "hostile" not in vx(sandbox.name, "docker inspect --format '{{.Config.Labels}}' marsh-svc-engine"))
    check("poison/production/marked-image-survives-in-production-only",
          "marsh-svc-engine:latest" in prod(sandbox.name, "image ls --format '{{.Repository}}:{{.Tag}}'"))
    driver.stop()
    # The audit: a stray container in the boot engine makes the engine unavailable before ready.
    vx(sandbox.name, f"docker create --name stray {pin} >/dev/null")
    driver = Driver(build, sandbox.name)
    lost = driver.wait(unavailable, 120)
    check("audit/stray-container-makes-unavailable", lost is not None and "holds more than" in lost.get("reason", "") and "ready" not in driver.states(),
          json.dumps(lost))
    driver.stop()
    vx(sandbox.name, "docker rm -f stray >/dev/null; true")


def scenario_size(ctx, sandbox: Sandbox) -> None:
    """A pool that cannot fit is refused before any allocation (VM-07)."""
    sandbox.create()
    sandbox.deny_all()
    say("INFO", "docker-disk-bytes(size,avail)", vx(sandbox.name, "df -B1 --output=size,avail /var/lib/docker | tail -1"))
    t0 = time.monotonic()
    driver = Driver(ctx.normal, sandbox.name, pool="64g")
    lost = driver.wait(unavailable, 180)
    check("size/unavailable-with-space-reason", lost is not None and lost["reason"].startswith("marsh: ") and "free" in lost["reason"], json.dumps(lost))
    say("INFO", "size/refused_in_ms", str(int((time.monotonic() - t0) * 1000)))
    check("size/no-pool-image", vx(sandbox.name, f"test -e {STATE}/pool/pool.img && echo yes || echo no").endswith("no"),
          vx(sandbox.name, f"ls -la {STATE}/pool/"))
    check("size/production-engine-never-started", vx(sandbox.name, "docker logs marsh-svc-engine 2>&1 | grep -c dockerd-started || true").endswith("0"))
    driver.stop()


def scenario_detach(ctx, sandbox: Sandbox) -> None:
    """Detach during startup is bounded and leaves no stream behind (cancellation)."""
    sandbox.create()
    sandbox.deny_all()
    for after_ms in (50, 3000):
        clear_engine(sandbox.name)
        started = time.monotonic()
        result = run([ctx.normal.driver, "--vm", sandbox.name, "--project", str(ctx.project), "--artifacts", str(ctx.normal.artifacts),
                      "--pool-bytes", "4g", "--detach-after-ms", str(after_ms)], 120)
        took = time.monotonic() - started
        event = next((jload(line) for line in result.stdout.splitlines() if "detached_in_ms" in line), {})
        check(f"detach/{after_ms}ms/returns-promptly", result.returncode == 0 and event.get("detached_in_ms", 99999) < 6000,
              f"detach took {event.get('detached_in_ms')} ms, driver ran {took:.1f} s, before={event.get('state_before')}")
        check(f"detach/{after_ms}ms/revoked", (event.get("state_after") or {}).get("state") == "unavailable", json.dumps(event.get("state_after")))
        time.sleep(15)
        in_vm = vx(sandbox.name, "ps aux | grep -c '[d]ocker logs' || true").splitlines()[-1]
        on_mac = run(["sh", "-c", f"ps aux | grep '[s]bx exec.*{sandbox.name}.*docker logs' | wc -l"]).stdout.strip()
        check(f"detach/{after_ms}ms/no-log-stream-left", in_vm == "0" and on_mac == "0", f"in the VM: {in_vm}, on the Mac: {on_mac}")
        say("INFO", f"detach/{after_ms}ms/boot-engine", vx(sandbox.name, "docker ps -a --format '{{.Names}} {{.Status}}'") or "(no containers)")
    clear_engine(sandbox.name)


def builder_witness(ctx, sandbox: Sandbox, label: str, expect_allowed: bool) -> None:
    """SEC-07 steps 1-3 as the production builder sees them: `--security=insecure` with an effective allow."""
    pin = pinned_id(ctx.normal.artifacts)
    vx(sandbox.name, "p=$(find /usr/lib /usr/libexec /usr/local/lib -name docker-buildx -type f 2>/dev/null | head -1); "
       "test -n \"$p\" && docker exec marsh-svc-engine mkdir -p /root/.docker/cli-plugins && "
       "docker cp \"$p\" marsh-svc-engine:/root/.docker/cli-plugins/docker-buildx")
    vx(sandbox.name, f"docker exec -i marsh-svc-engine docker -H {PROD_SOCKET} load < {STATE}/artifacts/svc-engine.tar")
    prod(sandbox.name, f"tag {pin} nfx/engine:s2")
    run(["sbx", "cp", str(ctx.frontend), f"{sandbox.name}:/tmp/frontend.tar"], 180)
    vx(sandbox.name, f"docker exec -i marsh-svc-engine docker -H {PROD_SOCKET} load < /tmp/frontend.tar")
    insecure = ("rm -rf /tmp/build /tmp/out; mkdir -p /tmp/build; printf '%s\\n' '# syntax=docker/dockerfile:1-labs' 'FROM nfx/engine:s2 AS build' "
                "'RUN --security=insecure sh -c \"grep CapEff /proc/self/status > /caps\"' 'FROM scratch' 'COPY --from=build /caps /caps' > /tmp/build/Dockerfile; "
                f"DOCKER_HOST={PROD_SOCKET} docker buildx build --allow security.insecure --output type=local,dest=/tmp/out /tmp/build 2>&1 | tail -3; "
                "echo BUILD_EXIT=$?; test ! -f /tmp/out/caps || cat /tmp/out/caps")
    out = eng(sandbox.name, insecure)
    say("INFO", f"builder/{label}", out.replace("\n", " | ")[-300:])
    if expect_allowed:
        check(f"builder/{label}/insecure-build-granted", "CapEff" in out and "ffffffffff" in out, out[-120:])
    else:
        check(f"builder/{label}/insecure-build-refused", "not allowed by build daemon" in out and "CapEff" not in out, out[-160:])
        control = ("printf '%s\\n' 'FROM nfx/engine:s2 AS build' 'RUN echo normal-build-ok >/control' 'FROM scratch' 'COPY --from=build /control /control' > /tmp/build/Dockerfile; "
                   f"rm -rf /tmp/control; DOCKER_HOST={PROD_SOCKET} docker buildx build --output type=local,dest=/tmp/control /tmp/build >/dev/null 2>&1; echo CONTROL_EXIT=$?; cat /tmp/control/control")
        out = eng(sandbox.name, control)
        check("builder/allowed-build-control", "CONTROL_EXIT=0" in out and "normal-build-ok" in out, out[-100:])


def scenario_builder(ctx, sandbox: Sandbox) -> None:
    sandbox.verify()
    driver = Driver(ctx.normal, sandbox.name)
    driver.wait(ready, 120)
    builder_witness(ctx, sandbox, "normal", expect_allowed=False)
    driver.stop()


def scenario_breaks(ctx, sandbox: Sandbox, small: Sandbox) -> None:
    build = ctx.breaks
    pin = pinned_id(build.artifacts)
    sandbox.verify()
    small.verify()
    # entitlements-on: config granted, and the production builder honors it (SEC-07 c1).
    d = Driver(build, sandbox.name, breaks=["entitlements-on"])
    d.wait(ready, 120)
    cfg = jload(eng(sandbox.name, "cat /etc/docker/daemon.json")).get("builder", {}).get("entitlements")
    check("break/entitlements-on/config", cfg == {"network-host": True, "security-insecure": True}, str(cfg))
    builder_witness(ctx, sandbox, "entitlements-on", expect_allowed=True)
    d.stop()
    # pool-skip-on-start: ready, but the data-root is not on the XFS pool (VM-04 c1).
    d = Driver(build, sandbox.name, breaks=["pool-skip-on-start"])
    d.wait(ready, 120)
    fstype = eng(sandbox.name, "findmnt -n -o FSTYPE -T /pool/docker")
    check("break/pool-skip-on-start/detected", fstype != "xfs", f"data-root filesystem={fstype}, state={d.states()}")
    d.stop()
    # pool-no-size-check: a pool that cannot fit is accepted (VM-07 c1).
    d = Driver(build, small.name, pool="64g", breaks=["pool-no-size-check"])
    up = d.wait(lambda e: ready(e) or unavailable(e), 180)
    image = vx(small.name, f"ls -ls {STATE}/pool/pool.img 2>&1")
    check("break/pool-no-size-check/detected", up is not None and ready(up) and "No such" not in image, f"{json.dumps(up)} {image}")
    d.stop()
    # ready-before-pool (and its deprecated aliases): the gate opens at the hold, before the engine reports ready (VM-03 c2).
    for name in ("ready-before-pool", "boot-barrier-off", "endpoint-before-pool"):
        clear_engine(sandbox.name)
        d = Driver(build, sandbox.name, breaks=[name, "hold-engine-before-ready"])
        if hold_reached(sandbox.name):
            time.sleep(1)
            opened = d.gate_open_t()
            check(f"break/{name}/gate-open-at-the-hold", opened is not None,
                  f"gate open t={opened} while the engine is held before ready (a correct build keeps it closed); states={d.states()}")
            release(sandbox.name)
        else:
            check(f"break/{name}/hold-reached", False)
        d.stop()
    # relay-dial-boot-engine: the relay target is the boot engine (VM-03 c1; the wave-2 proof needs the E1/S3 relay).
    d = Driver(build, sandbox.name, breaks=["relay-dial-boot-engine"])
    d.wait(ready, 120)
    target = d.wait(lambda e: e.get("probe") == "relay-target", 5)
    check("break/relay-dial-boot-engine/target-is-the-boot-engine", target is not None and target["container"] is None
          and target["socket"] == "/var/run/docker.sock", json.dumps(target))
    d.stop()
    # bootstrap-use-user-image, boot-engine tag source.
    vx(sandbox.name, "docker rm -f marsh-svc-engine >/dev/null 2>&1; "
       f"docker create --name hostile-src {pin} >/dev/null && "
       "docker commit --change 'LABEL hostile=1' hostile-src marsh-svc-engine:latest >/dev/null && docker rm hostile-src >/dev/null")
    d = Driver(build, sandbox.name, breaks=["bootstrap-use-user-image"])
    d.wait(ready, 120)
    labels = vx(sandbox.name, "docker inspect --format '{{.Config.Labels}}' marsh-svc-engine")
    check("break/bootstrap-use-user-image/boot-tag/detected", "hostile" in labels and boot_image(sandbox.name) != pin, f"hostile label={'hostile' in labels}")
    d.stop()
    vx(sandbox.name, "docker rm -f marsh-svc-engine >/dev/null 2>&1; docker rmi -f marsh-svc-engine:latest >/dev/null 2>&1; true")
    # bootstrap-use-user-image, production source (SEC-11 c1): the functional marked engine runs.
    d = Driver(build, sandbox.name)
    d.wait(ready, 120)
    make_hostile_engine(sandbox.name, pin)
    d.stop()
    d = Driver(build, sandbox.name, breaks=["bootstrap-use-user-image"])
    up = d.wait(ready, 120)
    check("break/bootstrap-use-user-image/production-source/ready", up is not None, json.dumps(up))
    labels = vx(sandbox.name, "docker inspect --format '{{.Config.Labels}}' marsh-svc-engine")
    check("break/bootstrap-use-user-image/production-source/detected",
          up is not None and boot_image(sandbox.name) != pin and hostile_ran(sandbox.name) and "hostile" in labels,
          f"running engine image {boot_image(sandbox.name)[:19]}.. (pinned {pin[:19]}..), hostile marker present={hostile_ran(sandbox.name)}, hostile label={'hostile' in labels}")
    d.stop()
    vx(sandbox.name, f"docker rm -f marsh-svc-engine >/dev/null 2>&1; docker rmi -f marsh-svc-engine:latest >/dev/null 2>&1; rm -f {HOSTILE_MARKER}; true")
    # slow-pool: a measurement control, not a hold.
    d = Driver(build, sandbox.name, breaks=["slow-pool"])
    up = d.wait(ready, 120)
    say("INFO", "break/slow-pool", f"ready after {up and up['since_start_ms']} ms (8 s pool delay)")
    check("break/slow-pool/measurably-slower", up is not None and up["since_start_ms"] >= 8000)
    d.stop()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--driver-normal", required=True)
    parser.add_argument("--artifacts-normal", type=Path, required=True)
    parser.add_argument("--driver-breaks", required=True)
    parser.add_argument("--artifacts-breaks", type=Path, required=True)
    parser.add_argument("--project", type=Path, required=True)
    parser.add_argument("--frontend", type=Path, help="docker save of docker/dockerfile:1-labs (made if absent)")
    parser.add_argument("--template", default=None)
    parser.add_argument("--only", default="")
    parser.add_argument("--revoke-repeats", type=int, default=1)
    args = parser.parse_args()
    template = args.template or (Path.home() / ".marsh-dev/libexec/marsh/shell-image").read_text().strip()
    only = set(filter(None, args.only.split(",")))
    run_id = secrets.token_hex(3)
    scratch = Path(tempfile.mkdtemp(prefix="s2-live-"))
    ctx = argparse.Namespace(normal=Build(args.driver_normal, args.artifacts_normal, args.project),
                             breaks=Build(args.driver_breaks, args.artifacts_breaks, args.project),
                             project=args.project, revoke_repeats=args.revoke_repeats, frontend=args.frontend or scratch / "frontend.tar")
    if not args.frontend:
        run(["docker", "pull", "docker/dockerfile:1-labs"], 300)
        run(["docker", "save", "-o", str(ctx.frontend), "docker/dockerfile:1-labs"], 300)
    for label, build in (("normal", ctx.normal), ("breaks", ctx.breaks)):
        marker = run(["sh", "-c", f"strings -a '{build.artifacts / 'marsh-linux-arm64'}' | grep -c 'marsh-feature:test-breaks' || true"]).stdout.strip()
        driver_marker = run(["sh", "-c", f"strings -a '{build.driver}' | grep -c 'marsh-feature:test-breaks' || true"]).stdout.strip()
        check(f"build/{label}-provenance-marker", (marker != "0") == (label == "breaks") and (driver_marker != "0") == (label == "breaks"),
              f"guest marker lines={marker}, driver marker lines={driver_marker}")
    main_box = Sandbox(f"nfx-s2-{run_id}-fresh", template, args.project)
    detach_box = Sandbox(f"nfx-s2-{run_id}-detach", template, args.project)
    small_box = Sandbox(f"nfx-s2-{run_id}-small", template, args.project)
    say("INFO", "run", f"run id {run_id}; sandboxes {main_box.name}, {small_box.name}")
    with vmlock.held(f"S2-engine-live-{run_id}"):  # the shared helper; held for the whole run, cleanup included
        try:
            if not only or "fresh" in only:
                scenario_fresh(ctx, main_box)
            for name, scenario in (("restart", scenario_restart), ("hold", scenario_hold), ("revoke", scenario_revocation),
                                   ("poison", scenario_poison), ("builder", scenario_builder)):
                if not only or name in only:
                    scenario(ctx, main_box)
            if not only or "detach" in only:
                scenario_detach(ctx, detach_box)
            if not only or "size" in only or "breaks" in only:
                scenario_size(ctx, small_box)
            if not only or "breaks" in only:
                scenario_breaks(ctx, main_box, small_box)
        finally:
            for box in list(OWNED.values()):
                box.remove()
            say("INFO", "cleanup", f"remaining owned sandboxes: {sorted(OWNED)}")
    print("FAILED: " + ", ".join(FAILURES) if FAILURES else "ALL PASSED")
    sys.exit(1 if FAILURES else 0)


if __name__ == "__main__":
    main()
