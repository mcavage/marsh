#!/usr/bin/env python3
"""Live witnesses for the engine follow-ups of S2c, against real sbx.

  1. Loopback-only published ports: the production `daemon.json` has `"ip": "127.0.0.1"`, so a
     publish without a host IP binds to loopback inside the engine. Witnessed through the E3 filter
     (`docker run -p 8080:80`) and by a direct engine create (no filter), reading `ss -ltn` in the
     engine. Break `engine-ip-any`.
  2. cgroup v2 delegation: containers created with the policy's default limits (memory, CPU, PIDs)
     start and their limits are enforced (read from the engine side: the cgroup files, `docker
     inspect`, a PID-limit pressure check, an OOM kill). Break `engine-cgroup-no-delegate`.
  3. The registry policy hook: `engine-up --internet` (E5: the allowlist follows the gate) reaches `/run/marsh-svc/engine-config.json`
     (default Docker Hub plus ghcr.io) and, through the E3 harness fed from that file, decides which
     image references the filter admits.

Needs the S2 style builds (a normal driver and guest `marsh`, plus a `test-breaks` pair, each artifact
directory holding the pinned engine tar and pin) and, for the filter witnesses, `e3-harness` built with
`--features dev-harness` for linux/arm64 (`--harness`).

The whole run holds the VM lock (`harness/vmlock.py`; `--shared` for a shared slot) for its lifetime,
cleanup included. The sandbox is `nfx-s2c-<run id>-eng`, created with S1's arguments, recorded by name and
UUID, and removed by exactly that.

  services_s2c_engine.py --driver-normal D --artifacts-normal A --driver-breaks D --artifacts-breaks A \\
      --harness E3_HARNESS --project DIR [--only NAME[,NAME]] [--shared]

Every check prints `PASS name: detail` or `FAIL name: detail`; the exit status is non-zero if any failed.
Break scenarios run the same witnesses and require them to see the defect.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import secrets
import shutil
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path.home() / "dev/narwhal-fork-spike/harness"))
import services_s2_engine as s2  # noqa: E402
import services_s2b_firewall as s2b  # noqa: E402
import vmlock  # noqa: E402

from services_s2_engine import Build, Driver, check, eng, prod, ready, run, say, vx  # noqa: E402

IMAGE = "busybox:1.37"
FILTER_SOCKET = "/tmp/s2c-filter.sock"
LOOPBACK = {"127.0.0.1", "[::1]"}
DEFAULT_LIMITS = {"memory": 4 << 30, "nano_cpus": 4_000_000_000, "pids": 4096}
PROJECT = ""
CREATED: list[str] = []


def engrc(vm: str, command: str, timeout: int = 180) -> tuple[int, str]:
    result = run(["sbx", "exec", "-u", "root", vm, "docker", "exec", "marsh-svc-engine", "sh", "-c", command], timeout)
    return result.returncode, (result.stdout + result.stderr).strip()


def filtered(vm: str, command: str, timeout: int = 180) -> tuple[int, str]:
    """A docker command through the E3 filter (the harness's socket inside the engine)."""
    return engrc(vm, f"DOCKER_HOST=unix://{FILTER_SOCKET} docker {command}", timeout)


def direct(vm: str, command: str, timeout: int = 180) -> tuple[int, str]:
    """A docker command straight to the production engine: no filter."""
    return engrc(vm, f"docker -H {s2.PROD_SOCKET} {command}", timeout)


def listeners(vm: str, port: int) -> list[str]:
    """Local addresses of the listening sockets on `port` in the engine's network namespace."""
    _, out = engrc(vm, f"ss -ltnH 'sport = :{port}'")
    addresses = []
    for line in out.splitlines():
        fields = line.split()
        if len(fields) >= 4 and fields[0] == "LISTEN":
            addresses.append(fields[3].rsplit(":", 1)[0])
    return sorted(addresses)


def remove(vm: str, name: str) -> None:
    prod(vm, f"rm -f {name} >/dev/null 2>&1; true")
    if name in CREATED:
        CREATED.remove(name)


def create(path: str, name: str, vm: str, command: str) -> tuple[int, str]:
    """`docker run -d --name NAME ...` through `path` ('filter' or 'direct'); the name is recorded for cleanup."""
    remove(vm, name)
    CREATED.append(name)
    return (filtered if path == "filter" else direct)(vm, f"run -d --name {name} {command}")


def cgroup_dir(vm: str, name: str) -> str | None:
    """The container's cgroup directory in the engine (the production engine uses the cgroupfs driver)."""
    container = prod(vm, f"inspect --format '{{{{.Id}}}}' {name}")
    if len(container) != 64:
        return None
    _, out = engrc(vm, f"for d in /sys/fs/cgroup/docker/{container} /sys/fs/cgroup/{container}; do [ -d $d ] && echo $d && break; done")
    return out or None


def cgroup_file(vm: str, directory: str, name: str) -> str:
    return engrc(vm, f"cat {directory}/{name} 2>&1")[1]


def start_harness(vm: str, hosts: str | None = None) -> bool:
    """The E3 filter in front of the production engine, in the engine container (stand-in for E1's relay)."""
    registry = f"--registry {hosts}" if hosts else ""
    engrc(vm, "mkdir -p /pool/marsh-s2c/scopes /var/lib/marsh/binds /run/marsh; chmod 700 /var/lib/marsh /var/lib/marsh/binds /run/marsh")
    engrc(vm, f"rm -f {FILTER_SOCKET}")
    command = (f"echo $$ > {PROJECT}/h.pid; exec {PROJECT}/e3-harness serve --state /pool/marsh-s2c --upstream /run/marsh-svc/docker.sock "
               f"--listen {FILTER_SOCKET} --root {PROJECT} --workspace s2c --attempt a1 {registry} > {PROJECT}/h.log 2>&1")
    run(["sbx", "exec", "-u", "root", vm, "docker", "exec", "-d", "marsh-svc-engine", "sh", "-c", command])
    for _ in range(60):
        if engrc(vm, f"test -S {FILTER_SOCKET} && echo yes")[1] == "yes":
            return True
        s2.time.sleep(0.5)
    return False


def stop_harness(vm: str) -> None:
    """Stop the harness this run started, by the exact pid it recorded."""
    engrc(vm, f"kill $(cat {PROJECT}/h.pid) 2>/dev/null; rm -f {PROJECT}/h.pid {FILTER_SOCKET}")


def pull(vm: str) -> None:
    out = prod(vm, f"pull {IMAGE}")
    check("setup/image-pulled", "Digest: sha256:" in out or "Status:" in out, out.splitlines()[-1][:80] if out else "")


# --- witnesses (each returns what it saw; the scenario decides what is right) -------------------

def publish_witness(vm: str, path: str, port: int) -> dict:
    """A publish with no host IP, through `path`. `-p PORT:80` and `-P` are both used."""
    name = f"s2c-pub-{path}-{port}"
    rc, out = create(path, name, vm, f"-p {port}:80 {IMAGE} httpd -f -p 80")
    seen = {"rc": rc, "out": out[-160:], "listeners": listeners(vm, port)}
    seen["inspect_host_ip"] = prod(vm, f"inspect --format '{{{{range $p, $b := .NetworkSettings.Ports}}}}{{{{range $b}}}}{{{{.HostIp}}}} {{{{end}}}}{{{{end}}}}' {name}")
    return seen


def publish_all_witness(vm: str, path: str) -> dict:
    """`--expose 80 -P`: a random host port, no IP."""
    name = f"s2c-pubP-{path}"
    rc, out = create(path, name, vm, f"--expose 80 -P {IMAGE} httpd -f -p 80")
    ports = prod(vm, f"inspect --format '{{{{range $p, $b := .NetworkSettings.Ports}}}}{{{{range $b}}}}{{{{.HostPort}}}} {{{{end}}}}{{{{end}}}}' {name}").split()
    found = {port: listeners(vm, int(port)) for port in ports if port.isdigit()}
    return {"rc": rc, "out": out[-160:], "ports": found}


def all_loopback(addresses: list[str]) -> bool:
    return bool(addresses) and all(address in LOOPBACK for address in addresses)


def limits_witness(vm: str, path: str, name: str, flags: str) -> dict:
    """A container with these flags through `path`, read from the engine side."""
    rc, out = create(path, name, vm, f"{flags} {IMAGE} sleep 600")
    state = prod(vm, f"inspect --format '{{{{.State.Status}}}}' {name}") if rc == 0 else "not-created"
    seen = {"rc": rc, "out": out[-300:], "status": state}
    if rc == 0 and state == "running":
        seen["inspect"] = prod(vm, f"inspect --format '{{{{.HostConfig.Memory}}}} {{{{.HostConfig.NanoCpus}}}} {{{{.HostConfig.PidsLimit}}}}' {name}")
        directory = cgroup_dir(vm, name)
        seen["cgroup_dir"] = directory
        if directory:
            seen["memory.max"] = cgroup_file(vm, directory, "memory.max")
            seen["pids.max"] = cgroup_file(vm, directory, "pids.max")
            seen["cpu.max"] = cgroup_file(vm, directory, "cpu.max")
    return seen


def pids_pressure(vm: str, path: str) -> dict:
    """A fork storm in a container with `--pids-limit 64`: the cgroup never holds more than 64 and counts the refusals."""
    name = f"s2c-pids-{path}"
    storm = "i=0; while [ $i -lt 400 ]; do (sleep 120 &) 2>/dev/null; i=$((i+1)); done; exec sleep 120"
    rc, out = create(path, name, vm, f"--pids-limit 64 --memory 256m {IMAGE} sh -c '{storm}'")
    seen: dict = {"rc": rc, "out": out[-200:]}
    if rc != 0:
        return seen
    s2.time.sleep(4)
    directory = cgroup_dir(vm, name)
    seen["cgroup_dir"] = directory
    if not directory:
        seen["state"] = prod(vm, f"inspect --format '{{{{.State.Status}}}} exit={{{{.State.ExitCode}}}} oom={{{{.State.OOMKilled}}}}' {name}")
        seen["logs"] = prod(vm, f"logs --tail 3 {name} 2>&1")[-200:]
    if directory:
        current = cgroup_file(vm, directory, "pids.current")
        peak = cgroup_file(vm, directory, "pids.peak")
        events = cgroup_file(vm, directory, "pids.events")
        seen.update({"pids.max": cgroup_file(vm, directory, "pids.max"), "pids.current": current, "pids.peak": peak, "pids.events": events.replace("\n", " ")})
    return seen


def oom_witness(vm: str, path: str) -> dict:
    name = f"s2c-oom-{path}"
    rc, out = create(path, name, vm, f"--memory 64m --memory-swap 64m {IMAGE} sh -c 'tail /dev/zero'")
    seen: dict = {"rc": rc, "out": out[-200:]}
    if rc != 0:
        return seen
    s2.time.sleep(6)
    seen["state"] = prod(vm, f"inspect --format '{{{{.State.Status}}}} oom={{{{.State.OOMKilled}}}} exit={{{{.State.ExitCode}}}}' {name}")
    return seen


def is_number(text: str | None) -> int | None:
    try:
        return int(str(text).split()[0])
    except (ValueError, IndexError):
        return None


# --- scenarios -----------------------------------------------------------------------------------

def bring_up(ctx, vm, build, label: str, breaks: list[str] | None = None, extra: list[str] | None = None) -> Driver | None:
    s2.clear_engine(vm.name)
    driver = Driver(build, vm.name, extra=extra, breaks=breaks)
    up = driver.wait(ready, 300)
    opened = driver.wait(lambda e: e.get("probe") == "channel-gate" and e["open"], 10)
    if not check(f"{label}/ready", up is not None and opened is not None, json.dumps({"state": up and up.get("state")})):
        driver.stop()
        return None
    return driver


def scenario_normal(ctx, vm) -> None:
    vm.create()
    vm.verify()
    name = vm.name
    driver = bring_up(ctx, vm, ctx.normal, "normal")
    if driver is None:
        return
    check("normal/engine-has-a-private-cgroup-namespace", vx(name, "docker inspect --format '{{.HostConfig.CgroupnsMode}}' marsh-svc-engine") == "private")
    stages = vx(name, "docker logs marsh-svc-engine 2>&1 | grep -o '\"stage\":\"[a-z-]*\"' | tr '\\n' ' '")
    order = [word.split(":")[1].strip('"') for word in stages.split()]
    check("normal/stage-order-cgroups-before-dockerd", "cgroups" in order and "dockerd-started" in order and order.index("cgroups") < order.index("dockerd-started"), " ".join(order))
    subtree = eng(name, "cat /sys/fs/cgroup/cgroup.subtree_control").split()
    check("normal/controllers-delegated", all(c in subtree for c in ("cpu", "memory", "pids")), " ".join(subtree))
    check("normal/engine-processes-are-in-the-nested-cgroup", eng(name, "cat /sys/fs/cgroup/cgroup.procs | wc -l") == "0" and int(eng(name, "cat /sys/fs/cgroup/init/cgroup.procs | wc -l") or 0) > 0)
    pull(name)

    # 1. Loopback-only publishes.
    if not check("ip/daemon-json-ip-is-loopback", json.loads(eng(name, "cat /etc/docker/daemon.json")).get("ip") == "127.0.0.1"):
        pass
    if check("ip/filter-started", start_harness(name), "e3-harness listening inside the engine"):
        seen = publish_witness(name, "filter", 8080)
        say("INFO", "ip/filter-8080", json.dumps(seen, sort_keys=True))
        check("ip/filter-run-p-8080:80-created", seen["rc"] == 0, seen["out"])
        check("ip/filter-8080-listens-on-loopback-only", all_loopback(seen["listeners"]), f"ss -ltn local addresses: {seen['listeners']}")
        check("ip/filter-8080-inspect-host-ip-is-loopback", set(seen["inspect_host_ip"].split()) <= {"127.0.0.1"} and bool(seen["inspect_host_ip"]), seen["inspect_host_ip"])
    seen = publish_witness(name, "direct", 8081)
    say("INFO", "ip/direct-8081", json.dumps(seen, sort_keys=True))
    check("ip/direct-create-without-ip-created", seen["rc"] == 0, seen["out"])
    check("ip/direct-8081-listens-on-loopback-only", all_loopback(seen["listeners"]), f"ss -ltn local addresses: {seen['listeners']}")
    check("ip/direct-8081-inspect-host-ip-is-loopback", seen["inspect_host_ip"].split() and set(seen["inspect_host_ip"].split()) <= {"127.0.0.1"}, seen["inspect_host_ip"])
    seen = publish_all_witness(name, "direct")
    say("INFO", "ip/direct-P", json.dumps(seen, sort_keys=True))
    check("ip/direct-P-published-and-loopback-only", seen["rc"] == 0 and bool(seen["ports"]) and all(all_loopback(a) for a in seen["ports"].values()), json.dumps(seen["ports"]))
    everything = engrc(name, "ss -ltnH | awk '{print $4}' | sed 's/:[0-9]*$//' | sort -u | tr '\\n' ' '")[1].split()
    say("INFO", "ip/engine-listeners", f"every listening local address in the engine: {everything}")
    check("ip/no-listener-on-a-non-loopback-address", all(a in LOOPBACK for a in everything), f"{everything}")

    # 2. cgroup delegation.
    explicit = "--memory 4g --cpus 4 --pids-limit 4096"
    seen = limits_witness(name, "direct", "s2c-lim-direct", explicit)
    say("INFO", "cgroup/direct-default-limits", json.dumps(seen, sort_keys=True))
    check("cgroup/direct-create-with-the-default-limits-starts", seen["rc"] == 0 and seen["status"] == "running", f"rc={seen['rc']} status={seen['status']} {seen['out'][-120:]}")
    check("cgroup/direct-limits-in-the-cgroup", is_number(seen.get("memory.max")) == DEFAULT_LIMITS["memory"] and is_number(seen.get("pids.max")) == DEFAULT_LIMITS["pids"]
          and (seen.get("cpu.max") or "").startswith("400000 100000"), f"memory.max={seen.get('memory.max')} pids.max={seen.get('pids.max')} cpu.max={seen.get('cpu.max')}")
    if s2.check("cgroup/filter-still-up", engrc(name, f"test -S {FILTER_SOCKET} && echo yes")[1] == "yes"):
        seen = limits_witness(name, "filter", "s2c-lim-filter", "")
        say("INFO", "cgroup/filter-forced-defaults", json.dumps(seen, sort_keys=True))
        check("cgroup/filter-container-without-limit-flags-starts", seen["rc"] == 0 and seen["status"] == "running", f"rc={seen['rc']} status={seen['status']} {seen['out'][-120:]}")
        check("cgroup/filter-forced-defaults-in-the-engine-inspect", seen.get("inspect") == f"{DEFAULT_LIMITS['memory']} {DEFAULT_LIMITS['nano_cpus']} {DEFAULT_LIMITS['pids']}", str(seen.get("inspect")))
        check("cgroup/filter-forced-defaults-in-the-cgroup", is_number(seen.get("memory.max")) == DEFAULT_LIMITS["memory"] and is_number(seen.get("pids.max")) == DEFAULT_LIMITS["pids"]
              and (seen.get("cpu.max") or "").startswith("400000 100000"), f"memory.max={seen.get('memory.max')} pids.max={seen.get('pids.max')} cpu.max={seen.get('cpu.max')}")
        seen = pids_pressure(name, "filter")
        say("INFO", "cgroup/filter-pids-pressure", json.dumps(seen, sort_keys=True))
        check("cgroup/filter-pids-pressure-held-at-the-limit",
              seen.get("rc") == 0 and is_number(seen.get("pids.max")) == 64 and (is_number(seen.get("pids.peak")) or 999) <= 64 and (is_number(seen.get("pids.current")) or 0) >= 32
              and "max 0" not in (seen.get("pids.events") or "max 0"), f"pids.max={seen.get('pids.max')} current={seen.get('pids.current')} peak={seen.get('pids.peak')} events={seen.get('pids.events')}")
        seen = oom_witness(name, "filter")
        say("INFO", "cgroup/filter-oom", json.dumps(seen, sort_keys=True))
        check("cgroup/filter-memory-limit-oom-kills", seen.get("rc") == 0 and "oom=true" in seen.get("state", ""), seen.get("state", seen.get("out", "")))
    seen = pids_pressure(name, "direct")
    check("cgroup/direct-pids-pressure-held-at-the-limit",
          seen.get("rc") == 0 and is_number(seen.get("pids.max")) == 64 and (is_number(seen.get("pids.peak")) or 999) <= 64 and (is_number(seen.get("pids.events") and seen["pids.events"].split("max ")[-1]) or 0) > 0,
          f"pids.max={seen.get('pids.max')} current={seen.get('pids.current')} peak={seen.get('pids.peak')} events={seen.get('pids.events')}")

    # 3. The registry policy.
    config = json.loads(eng(name, "cat /run/marsh-svc/engine-config.json") or "{}")
    check("registry/default-is-docker-hub-and-ghcr", config.get("registry_allow") == ["docker.io", "ghcr.io"], json.dumps(config))
    hosts = ",".join(config.get("registry_allow") or [])
    stop_harness(name)
    check("registry/filter-restarted-with-the-engine-config", start_harness(name, hosts), f"--registry {hosts}")
    rc, out = filtered(name, f"run -d --name s2c-reg-ok {IMAGE} sleep 60")
    CREATED.append("s2c-reg-ok")
    check("registry/docker-hub-image-admitted", rc == 0, out[-120:])
    for ref in ("quay.io/nope/nope:1", "registry.example:5000/nope:1"):
        rc, out = filtered(name, f"create --name s2c-reg-no {ref}")
        CREATED.append("s2c-reg-no")
        check(f"registry/{ref.split('/')[0]}-refused-by-the-filter", rc != 0 and "marsh:" in out and "registry" in out, out[-140:])
        check(f"registry/{ref.split('/')[0]}-nothing-created", prod(name, "ps -a --format '{{.Names}}' | grep -c '^s2c-reg-no$' || true").splitlines()[-1] == "0")
    driver.stop()
    stop_harness(name)
    for container in list(CREATED):
        remove(name, container)

    # The list follows the Internet gate (E5): `--internet on` lifts it; there is no other registry argument.
    anything = bring_up(ctx, vm, ctx.normal, "registry-any", extra=["--internet", "on"])
    if anything is not None:
        config = json.loads(eng(name, "cat /run/marsh-svc/engine-config.json") or "{}")
        check("registry/open-gate-lifts-the-allowlist", config.get("registry_allow") is None and "registry_allow" in config, json.dumps(config))
        anything.stop()
    s2.clear_engine(name)


def scenario_ip_break(ctx, vm) -> None:
    """`engine-ip-any`: a publish with no host IP listens on every address. The filter's own loopback still holds."""
    vm.create()
    vm.verify()
    name = vm.name
    driver = bring_up(ctx, vm, ctx.breaks, "break/engine-ip-any", breaks=["engine-ip-any"])
    if driver is None:
        return
    check("break/engine-ip-any/daemon-json-ip-is-any", json.loads(eng(name, "cat /etc/docker/daemon.json")).get("ip") == "0.0.0.0")
    seen = publish_witness(name, "direct", 8081)
    say("INFO", "break/engine-ip-any/direct-8081", json.dumps(seen, sort_keys=True))
    check("break/engine-ip-any/direct-8081-witness-sees-the-defect", seen["rc"] == 0 and not all_loopback(seen["listeners"]), f"ss -ltn local addresses: {seen['listeners']} (the normal run requires loopback only)")
    check("break/engine-ip-any/inspect-host-ip-witness-sees-the-defect", "0.0.0.0" in seen["inspect_host_ip"].split(), seen["inspect_host_ip"])
    everything = engrc(name, "ss -ltnH | awk '{print $4}' | sed 's/:[0-9]*$//' | sort -u | tr '\\n' ' '")[1].split()
    check("break/engine-ip-any/listener-check-sees-the-defect", not all(a in LOOPBACK for a in everything), f"{everything}")
    if start_harness(name, None):
        seen = publish_witness(name, "filter", 8080)
        check("break/engine-ip-any/filter-layer-still-loopback", seen["rc"] == 0 and all_loopback(seen["listeners"]), f"the filter forces loopback by itself: {seen['listeners']}")
    stop_harness(name)
    driver.stop()
    for container in list(CREATED):
        remove(name, container)
    s2.clear_engine(name)


def scenario_cgroup_break(ctx, vm) -> None:
    """`engine-cgroup-no-delegate`: a container with the default limits fails to start."""
    vm.create()
    vm.verify()
    name = vm.name
    driver = bring_up(ctx, vm, ctx.breaks, "break/engine-cgroup-no-delegate", breaks=["engine-cgroup-no-delegate"])
    if driver is None:
        return
    stages = vx(name, "docker logs marsh-svc-engine 2>&1 | grep -o '\"stage\":\"[a-z-]*\"' | tr '\\n' ' '")
    check("break/engine-cgroup-no-delegate/stage-skipped", "cgroups-skipped" in stages and '"cgroups"' not in stages, stages)
    subtree = eng(name, "cat /sys/fs/cgroup/cgroup.subtree_control").split()
    check("break/engine-cgroup-no-delegate/controllers-not-delegated", "memory" not in subtree, " ".join(subtree))
    seen = limits_witness(name, "direct", "s2c-lim-direct", "--memory 4g --cpus 4 --pids-limit 4096")
    say("INFO", "break/engine-cgroup-no-delegate/direct", json.dumps(seen, sort_keys=True))
    check("break/engine-cgroup-no-delegate/default-limits-witness-sees-the-defect", seen["rc"] != 0 and seen["status"] != "running", f"rc={seen['rc']} status={seen['status']}: {seen['out'][-220:]}")
    plain = limits_witness(name, "direct", "s2c-lim-plain", "")
    check("break/engine-cgroup-no-delegate/control-container-without-limits-starts", plain["rc"] == 0 and plain["status"] == "running", f"rc={plain['rc']} status={plain['status']}")
    if start_harness(name, None):
        seen = limits_witness(name, "filter", "s2c-lim-filter", "")
        check("break/engine-cgroup-no-delegate/filter-forced-defaults-fail-to-start", seen["rc"] != 0 and seen["status"] != "running", f"rc={seen['rc']} status={seen['status']}: {seen['out'][-220:]}")
    stop_harness(name)
    driver.stop()
    for container in list(CREATED):
        remove(name, container)
    s2.clear_engine(name)


def main() -> None:
    global PROJECT
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--driver-normal", required=True)
    parser.add_argument("--artifacts-normal", type=Path, required=True)
    parser.add_argument("--driver-breaks", required=True)
    parser.add_argument("--artifacts-breaks", type=Path, required=True)
    parser.add_argument("--harness", type=Path, required=True, help="e3-harness (linux/arm64, --features dev-harness)")
    parser.add_argument("--project", type=Path, default=None)
    parser.add_argument("--template", default=None)
    parser.add_argument("--only", default="")
    parser.add_argument("--shared", action="store_true")
    args = parser.parse_args()
    template = args.template or (Path.home() / ".marsh-dev/libexec/marsh/shell-image").read_text().strip()
    only = set(filter(None, args.only.split(",")))
    run_id = secrets.token_hex(3)
    project = Path(tempfile.mkdtemp(prefix="nfx-s2c-", dir="/private/tmp")).resolve()
    PROJECT = str(project)
    s2b.PROJECT_DIR = PROJECT
    shutil.copy(args.harness, project / "e3-harness")
    (project / "e3-harness").chmod(0o755)
    ctx = type("Ctx", (), {})()
    ctx.normal = Build(args.driver_normal, args.artifacts_normal, project)
    ctx.breaks = Build(args.driver_breaks, args.artifacts_breaks, project)
    for label, build in (("normal", ctx.normal), ("breaks", ctx.breaks)):
        marker = run(["sh", "-c", f"strings -a '{build.artifacts / 'marsh-linux-arm64'}' | grep -c 'marsh-feature:test-breaks' || true"]).stdout.strip()
        driver_marker = run(["sh", "-c", f"strings -a '{build.driver}' | grep -c 'marsh-feature:test-breaks' || true"]).stdout.strip()
        check(f"build/{label}-provenance-marker", (marker != "0") == (label == "breaks") and (driver_marker != "0") == (label == "breaks"),
              f"guest marker lines={marker}, driver marker lines={driver_marker}")
    vm = s2b.Vm(f"nfx-s2c-{run_id}-eng", template, project)
    say("INFO", "run", f"run id {run_id}; sandbox {vm.name}; project {project}")
    with vmlock.held(f"S2c-engine-{run_id}", shared=args.shared):
        try:
            for name, scenario in (("normal", scenario_normal), ("ip-break", scenario_ip_break), ("cgroup-break", scenario_cgroup_break)):
                if not only or name in only:
                    scenario(ctx, vm)
        finally:
            if vm.box.uuid is not None:
                for container in list(CREATED):
                    remove(vm.name, container)
                stop_harness(vm.name)
            for box in list(s2.OWNED.values()):
                box.remove()
            say("INFO", "cleanup", f"remaining owned sandboxes: {sorted(s2.OWNED)}")
    shutil.rmtree(project, ignore_errors=True)
    print("FAILED: " + ", ".join(s2.FAILURES) if s2.FAILURES else "ALL PASSED")
    sys.exit(1 if s2.FAILURES else 0)


if __name__ == "__main__":
    main()
