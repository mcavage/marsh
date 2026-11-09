#!/usr/bin/env python3
"""Live witnesses for the services Internet gate (E5, SECURITY-DESIGN A5), through the product.

Drives the built `marsh` and `marshd` (a daemon per scope, `MARSH_VM_PREFIX=marsh-xnfx01-`) against
real stock sbx. A services engine is started in the product-created services VM with the
`services_engine_drive` example (the daemon's own engine start is wired by S2c/E1), reading the
project's grant from the scope's `services-internet.json` at every start, as that wiring will.

Scenarios (each prints `PASS name: detail` or `FAIL name: detail`; the exit status is non-zero if any
check failed):

  main      VM-08 c1-c3 and SEC-17 c2/c1: default off, on, off; a pinned pull works; the gate's rules;
            header names echoed by httpbin with Internet on; VM-09 (an explicit sbx rule governs:
            both toggles refuse, status says so, `sbx policy` is unchanged, egress stays denied);
            `services rm` forgets the grant.
  failclosed  governance that cannot be read counts as governed (an `sbx` wrapper fails `policy ls`).
  registry  the registry allowlist follows the gate: a pull through the real relay -> E3 filter path in the
            production engine, one relay process across `internet on` and `off`.
  job       SEC-09: a job's `marsh services internet on` is refused by the daemon and changes nothing.
  break:<mode>  the same flow on a test-breaks daemon, which must fail the named check.

Everything the checks print is a status code, a header NAME, a count or a boolean: never a header
value, a credential or a response body. The explicit sbx rule the test adds is scoped to the services
VM and is removed by its exact id. Sandboxes belong to the scope's ownership map and go through
`marsh services rm` / `marsh stop` of that scope only.

  services_e5_internet.py --prefix-normal P --prefix-breaks P --driver-normal D --driver-breaks D \
      --artifacts A --fixture-ref REF --work DIR [--only NAME[,NAME]]
"""
from __future__ import annotations

import argparse
import json
import os
import re
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import threading
import time

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(Path.home() / "dev/narwhal-fork-spike/harness"))
import services_s2_engine as s2  # noqa: E402
from launch_env import clean_environment  # noqa: E402
from harness import daemon_endpoint_paths  # noqa: E402
from services_s2_engine import Build, Driver, check, eng, prod, ready, run, say  # noqa: E402

BUSYBOX = "busybox@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec"
CURL = "curlimages/curl@sha256:c1fe1679c34d9784c1b0d1e5f62ac0a79fca01fb6377cdd33e90473c6f9f9a69"
SBX = shutil.which("sbx") or "sbx"
RULES: dict[str, str] = {}  # rule id -> sandbox, the sbx rules this run added (removed by exact id)


def broken(name: str, condition: bool, detail: str = "") -> None:
    """The named check of a scenario, run under its break: it must FAIL there (`condition` is what a
    passing check would say), anything else is an invalid negative control."""
    say("PASS" if not condition else "FAIL", name,
        ("fails under the break, as it must: " if not condition else "the break did not break this check: ") + detail)


def sbx(*arguments: str, timeout: int = 120) -> subprocess.CompletedProcess:
    return run([SBX, *arguments], timeout)


class Scope:
    """One daemon scope: home, control root, project, and the marsh built for it."""

    def __init__(self, work: Path, name: str, prefix: Path, breaks: list[str] | None = None, sbx_path: str | None = None):
        self.name, self.prefix, self.breaks = name, prefix, breaks or []
        self.root = work / name
        self.home, self.control, self.project = self.root / "home", self.root / "control", self.root / "project"
        for path in (self.root, self.home, self.home / "home", self.control, self.project):
            path.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.marsh = str(prefix / "bin" / "marsh")
        self.env = clean_environment()
        self.env.update({
            "MARSH_HOME": str(self.home), "MARSH_CONTROL_HOME": str(self.control), "MARSH_VM_PREFIX": "marsh-xnfx01-",
            "MARSH_SBX": sbx_path or SBX, "MARSH_GUEST_ARTIFACTS": str(prefix / "libexec" / "marsh"),
            "MARSH_OWNERSHIP_MIRROR": str(self.root / "mirror.json"),
        })
        self.daemon: subprocess.Popen | None = None
        self.log = self.root / "marshd.log"

    def start_daemon(self) -> None:
        """A break daemon is launched explicitly, the only way a mode arms (A10); a normal one autostarts."""
        if not self.breaks:
            return
        self.control_home().mkdir(mode=0o700, exist_ok=True)
        env = {**self.env, "MARSH_TEST_BREAK": ",".join(self.breaks)}
        self.daemon = subprocess.Popen([str(self.prefix / "bin" / "marshd"), "--home", str(self.home), "--test-breaks-from-env"],
                                       env=env, stdin=subprocess.DEVNULL, stdout=self.log.open("wb"), stderr=subprocess.STDOUT,
                                       start_new_session=True)
        say("INFO", f"{self.name}/daemon", f"pid {self.daemon.pid} with breaks {self.breaks}")
        # Wait for the daemon's own endpoint, never through the CLI (which would autostart a normal daemon
        # beside it), and refuse to go on if it died.
        endpoint, _ = daemon_endpoint_paths(self.home)
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            if self.daemon.poll() is not None:
                raise RuntimeError(f"{self.name}: the break daemon exited ({self.daemon.returncode}); see {self.log}")
            if endpoint.exists():
                return
            time.sleep(0.5)
        raise RuntimeError(f"{self.name}: the break daemon did not listen; see {self.log}")

    def run(self, *arguments: str, timeout: int = 600, cwd: Path | None = None) -> subprocess.CompletedProcess:
        return subprocess.run([self.marsh, *arguments], cwd=cwd or self.project, env=self.env, capture_output=True, text=True,
                              timeout=timeout, stdin=subprocess.DEVNULL)

    def services(self, *arguments: str) -> subprocess.CompletedProcess:
        return self.run("services", *arguments)

    def status(self) -> dict:
        document = json.loads(self.services("status", "--json").stdout)
        return next(p for p in document["projects"] if p["root"] == str(self.project))

    def internet(self) -> dict:
        return self.status()["internet"]

    def control_home(self) -> Path:
        import hashlib
        return self.control / hashlib.sha256(os.fsencode(self.home.resolve(strict=True))).hexdigest()

    def owned(self) -> list[str]:
        try:
            return sorted(json.loads((self.control_home() / "vm-ownership.json").read_text()).get("vms", {}))
        except (OSError, ValueError):
            return []

    def teardown(self) -> None:
        """`marsh stop` of this scope removes its VMs (services VMs it stops; `rm` removes them), then the daemon."""
        for rule, vm in list(RULES.items()):
            remove_rule(vm, rule)
        self.services("rm", "--yes")
        stopped = self.run("stop", timeout=300)
        gone = not set(self.owned()) & set(s2.inventory())
        check(f"{self.name}/cleanup", stopped.returncode == 0 and gone, f"marsh stop rc={stopped.returncode}; scope VMs left: {sorted(set(self.owned()) & set(s2.inventory()))}")
        if self.daemon is not None and self.daemon.poll() is None:
            try:
                self.daemon.wait(20)
            except subprocess.TimeoutExpired:
                self.daemon.terminate()  # the exact pid this run recorded


def add_deny(vm: str, host: str) -> str:
    result = sbx("policy", "deny", "network", "--sandbox", vm, host)
    ids = [word for word in result.stdout.split() if len(word) == 36 and word.count("-") == 4]
    if result.returncode or len(ids) != 1:
        raise RuntimeError(f"sbx policy deny failed: {(result.stdout + result.stderr).strip()[:200]}")
    RULES[ids[0]] = vm
    return ids[0]


def remove_rule(vm: str, rule: str) -> bool:
    result = sbx("policy", "rm", "network", "--force", "--sandbox", vm, "--id", rule)
    if result.returncode == 0:
        RULES.pop(rule, None)
    else:
        say("FAIL", f"cleanup/rule/{rule}", (result.stdout + result.stderr).strip()[:200])
    return result.returncode == 0


def rules_of(vm: str) -> str:
    """The `rules` of `sbx policy ls --json VM`, canonical (the document's sync time is not a rule)."""
    return json.dumps(json.loads(sbx("policy", "ls", "--json", vm).stdout)["rules"], sort_keys=True)


def single_host_global_denies() -> list[str]:
    """The exact hosts that the user's own (not built-in) global deny rules block. Read-only: this run never
    adds or changes a global rule, so VM-09 c4 uses the one the host already has."""
    hosts: list[str] = []
    for rule in json.loads(sbx("policy", "ls", "--json").stdout)["rules"]:
        if (rule["resource_type"] == "network" and rule["decision"] == "deny" and rule["scope"] == "global"
                and rule["origin"] == "local" and not rule["id"].startswith("default-") and rule.get("status") == "active"
                and all(set(r) <= set("abcdefghijklmnopqrstuvwxyz0123456789.-:") for r in rule["resources"])):
            hosts += rule["resources"]
    return hosts


class Engine:
    """A services engine in the scope's VM, started the way the daemon's will be (the grant read at start)."""

    def __init__(self, scope: Scope, build: Build, vm: str):
        self.scope, self.vm = scope, vm
        breaks = scope.breaks or None
        self.driver = Driver(build, vm, extra=["--grants-home", str(scope.home)], breaks=breaks)
        self.ready = self.driver.wait(ready, 300)
        if self.ready is not None:
            # Pulls work with the gate closed; doing them now keeps their output out of every probe.
            for image in (BUSYBOX, CURL):
                prod(vm, f"pull {image}", 300)

    def stop(self) -> None:
        self.driver.stop()

    def probe(self) -> dict[str, str]:
        """Statuses only: curl http code and exit, name resolution, an IP literal, from containers."""
        def curl(*args: str) -> str:
            out = prod(self.vm, f"run --rm {CURL} -sS -m 8 -o /dev/null -w '%{{http_code}}' {' '.join(args)} 2>&1; echo \"/$?\"", 90)
            tail = re.search(r"(\d{3})/(\d+)\s*$", out)  # code and status together at the end
            if tail:
                return f"{tail.group(1)}/{tail.group(2)}"
            code, status = re.match(r"\s*(\d{3})", out), re.search(r"/(\d+)\s*$", out)  # code first, status after the message
            return f"{code.group(1)}/{status.group(1)}" if code and status else f"?({out[-40:]!r})"
        resolves = prod(self.vm, f"run --rm {BUSYBOX} sh -c 'nslookup example.com >/dev/null 2>&1 && echo resolves || echo fails'", 60)
        network = prod(self.vm, f"run --rm --network bridge {BUSYBOX} sh -c 'nslookup example.com >/dev/null 2>&1 && echo resolves || echo fails'", 60)
        return {"http": curl("https://example.com/"), "ip": curl("-k https://1.1.1.1/"),
                "dns": resolves.splitlines()[-1], "dns_bridge": network.splitlines()[-1]}

    def reaches(self) -> bool:
        probe = self.probe()
        return probe["http"].startswith("200") and probe["dns"] == "resolves"

    def blocked(self) -> bool:
        probe = self.probe()
        return probe["http"].startswith("000") and probe["dns"] == "fails" and probe["ip"].startswith("000")

    def gate(self) -> str:
        return eng(self.vm, "/marsh/marsh --internal-services internet status")

    def header_names(self) -> list[str]:
        out = prod(self.vm, f"run --rm {CURL} -sS -m 20 https://httpbin.org/headers", 90)
        try:
            return sorted(json.loads(out)["headers"])
        except (ValueError, KeyError):
            return [f"unparsable({len(out)} bytes)"]


class Relay:
    """The services agent's relay in the production engine (`marsh --internal-services relay`), driven over its
    frame protocol (crates/marsh-services/src/frame.rs: u32 BE length, then tag 1 + JSON or tag 2 + u32 BE
    channel + data). The host half of the relay is E1's; this is the same bytes."""

    def __init__(self, vm: str):
        self.p = subprocess.Popen(
            ["sbx", "exec", "-i", "-u", "root", vm, "sh", "-c", "docker exec -i marsh-svc-engine /marsh/marsh --internal-services relay"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.frames: list[dict] = []
        self.data: dict[int, bytearray] = {}
        self.cv = threading.Condition()
        self.lock = threading.Lock()
        self.eof = False
        self.channel = 100
        threading.Thread(target=self._read, daemon=True).start()
        self.send({"type": "init", "protocol": 1, "settings": {}})
        self.ready = self.wait(lambda f: f.get("type") in ("ready", "error"), 90)

    def _read(self) -> None:
        consumed: dict[int, int] = {}
        while True:
            head = self.p.stdout.read(4)
            if len(head) < 4:
                break
            body = self.p.stdout.read(struct.unpack(">I", head)[0])
            with self.cv:
                if body[0] == 1:
                    self.frames.append(json.loads(body[1:]))
                else:
                    channel = struct.unpack(">I", body[1:5])[0]
                    self.data.setdefault(channel, bytearray()).extend(body[5:])
                    consumed[channel] = consumed.get(channel, 0) + 1
                    if consumed[channel] >= 16:
                        self.send({"type": "credit", "channel": channel, "chunks": consumed[channel]})
                        consumed[channel] = 0
                self.cv.notify_all()
        with self.cv:
            self.eof = True
            self.cv.notify_all()

    def send_raw(self, raw: bytes) -> None:
        with self.lock:
            self.p.stdin.write(raw)
            self.p.stdin.flush()

    def send(self, obj: dict) -> None:
        body = json.dumps(obj, separators=(",", ":")).encode()
        self.send_raw(struct.pack(">I", 1 + len(body)) + b"\x01" + body)

    def wait(self, predicate, timeout: float):
        deadline = time.monotonic() + timeout
        with self.cv:
            while True:
                for frame in self.frames:
                    if predicate(frame):
                        return frame
                if self.eof or time.monotonic() >= deadline:
                    return None
                self.cv.wait(0.05)

    def call(self, authority: dict, method: str, path: str, timeout: float = 240) -> tuple[int | None, str]:
        """One HTTP request through a docker channel into E3's filter: (status, body text)."""
        self.channel += 1
        channel = self.channel
        self.send({"type": "open", "channel": channel, "kind": "docker", "authority": authority})
        opened = self.wait(lambda f: f.get("channel") == channel and f["type"] in ("opened", "refused"), 20)
        if not opened or opened["type"] != "opened":
            return None, json.dumps(opened)
        request = f"{method} {path} HTTP/1.1\r\nHost: d\r\nConnection: close\r\nContent-Length: 0\r\n\r\n".encode()
        self.send_raw(struct.pack(">I", 5 + len(request)) + b"\x02" + struct.pack(">I", channel) + request)
        self.send({"type": "eof", "channel": channel})
        self.wait(lambda f: f["type"] in ("eof", "close") and f.get("channel") == channel, timeout)
        raw = bytes(self.data.get(channel, b""))
        self.send({"type": "close", "channel": channel})
        head, _, body = raw.partition(b"\r\n\r\n")
        try:
            status = int(head.split(b" ", 2)[1])
        except (IndexError, ValueError):
            status = None
        return status, body.decode("utf-8", "replace")

    def close(self) -> None:
        try:
            self.p.stdin.close()
            self.p.wait(15)
        except (OSError, subprocess.TimeoutExpired):
            self.p.kill()  # this run's own child, by its Popen handle


def start_project(scope: Scope) -> tuple[str, str]:
    scope.start_daemon()
    check(f"{scope.name}/services-on", scope.run("config", "services", "on").returncode == 0)
    started = scope.services("start", "--json")
    check(f"{scope.name}/services-start", started.returncode == 0, started.stderr.strip()[:160])
    document = json.loads(started.stdout)
    if scope.daemon is not None and scope.daemon.poll() is not None:
        raise RuntimeError(f"{scope.name}: the break daemon is not the one answering (it exited)")
    say("INFO", f"{scope.name}/owned-vm", f"{document['vm']} uuid={document['vm_uuid']}")
    return document["vm"], document["vm_uuid"]


# --- scenarios ---------------------------------------------------------------------------------

def scenario_main(ctx) -> None:
    scope = Scope(ctx.work, "main", ctx.prefix_normal)
    engine = None
    try:
        vm, uuid = start_project(scope)
        before = scope.internet()
        check("VM-08/c1 status: default on:false, not governed", before["on"] is False and before["governed"] is False, json.dumps(before))
        engine = Engine(scope, Build(ctx.driver_normal, ctx.artifacts, scope.project), vm)
        check("engine ready", engine.ready is not None)
        pull = prod(vm, f"pull {BUSYBOX}", 300)
        check("VM-08/c1 pinned pull from Docker Hub works while off", "Digest: sha256:" in pull or "up to date" in pull, pull.splitlines()[-1][-100:])
        prod(vm, f"pull {CURL}", 300)
        say("INFO", "gate/rules-while-off", eng(vm, "iptables -w -S MARSH-INTERNET | tr '\\n' ';'"))
        probe = engine.probe()
        say("INFO", "VM-08/c1 probe (off)", json.dumps(probe))
        check("VM-08/c1 HTTP fails, resolution fails, IP literal fails (default off)", engine.blocked(), json.dumps(probe))
        check("SEC-17/c2 the gate reads closed and verified", '"open":false' in engine.gate() and '"verified":true' in engine.gate(), engine.gate())
        check("status effective names the closed gate", "off:" in scope.internet()["effective"], scope.internet()["effective"])

        # container-to-container and a published port still work with the gate closed
        prod(vm, "network create e5net >/dev/null; true")
        prod(vm, f"run -d --name e5web --network e5net {BUSYBOX} sh -c 'echo ok > /www.txt; httpd -f -p 80 -h /'", 60)
        time.sleep(1)
        peer = prod(vm, f"run --rm --network e5net {BUSYBOX} wget -q -T 5 -O - http://e5web/www.txt", 60)
        check("closed gate keeps container-to-container traffic", peer.strip().endswith("ok"), peer.strip()[-40:])
        prod(vm, f"run -d --name e5pub -p 127.0.0.1:18081:80 {BUSYBOX} sh -c 'echo pub > /www.txt; httpd -f -p 80 -h /'", 60)
        time.sleep(1)
        published = eng(vm, "bash -c 'exec 3<>/dev/tcp/127.0.0.1/18081; printf \"GET /www.txt HTTP/1.0\\r\\n\\r\\n\" >&3; cat <&3' 2>&1 | tail -1")
        check("closed gate keeps published ports reachable on engine loopback", published.strip() == "pub", published.strip()[-40:])
        prod(vm, "rm -f e5web e5pub >/dev/null 2>&1; true")

        on = scope.services("internet", "on", "--json")
        check("VM-08 internet on exits 0", on.returncode == 0, on.stderr.strip()[:160])
        after = json.loads(on.stdout)["internet"] if on.returncode == 0 else {}
        check("VM-08/c2 status on:true, not governed", after.get("on") is True and after.get("governed") is False, json.dumps(after))
        probe = engine.probe()
        say("INFO", "VM-08/c2 probe (on)", json.dumps(probe))
        check("VM-08/c2 HTTP 200 and resolution succeeds after on", engine.reaches(), json.dumps(probe))
        check("SEC-17/c2 status discloses on:true with the gate open", "on:" in scope.internet()["effective"], scope.internet()["effective"])
        names = engine.header_names()
        say("INFO", "SEC-17/c1 httpbin header names (Internet on, from a services container)", json.dumps(names))
        check("SEC-17/c1 the echo carries only the client's own header names (no injected credential header)",
              set(names) <= {"Accept", "Host", "User-Agent", "X-Amzn-Trace-Id"}, json.dumps(names))

        off = scope.services("internet", "off", "--json")
        check("VM-08 internet off exits 0", off.returncode == 0, off.stderr.strip()[:160])
        probe = engine.probe()
        say("INFO", "VM-08/c3 probe (off again)", json.dumps(probe))
        check("VM-08/c3 HTTP and resolution fail again after off", engine.blocked(), json.dumps(probe))
        check("VM-08/c3 status on:false", scope.internet()["on"] is False, json.dumps(scope.internet()))
        policy = sbx("policy", "check", "network", "--sandbox", vm, "example.com")
        say("INFO", "VM-08/c3 sbx policy check (marsh adds no sbx rule: sbx cannot express default-deny with exceptions)",
            (policy.stdout + policy.stderr).strip().splitlines()[0])
        scoped = [r["id"] for r in json.loads(rules_of(vm)) if r["scope"].startswith("sandbox:")]
        check("marsh added no sbx rule to the VM", scoped == [], f"{len(scoped)} sandbox-scoped rule(s)")

        # VM-09 c4: a single-host global deny is shown, does not govern, and `internet on` still works.
        denied = single_host_global_denies()
        check("VM-09/c4 precondition: the host has a single-host global deny (read-only)", bool(denied), json.dumps(denied))
        shown = scope.internet()
        check("VM-09/c4 governed:false and `effective` names the single-host deny", shown["governed"] is False and all(h in shown["effective"] for h in denied), json.dumps(shown))
        c4_on = scope.services("internet", "on", "--json")
        check("VM-09/c4 `internet on` still works (exit 0, on:true, not governed)", c4_on.returncode == 0 and json.loads(c4_on.stdout)["internet"]["on"] is True
              and json.loads(c4_on.stdout)["internet"]["governed"] is False and all(h in json.loads(c4_on.stdout)["internet"]["effective"] for h in denied),
              c4_on.stderr.strip()[:160] or json.loads(c4_on.stdout)["internet"]["effective"])
        check("VM-09/c4 the container reaches the internet with the single-host deny listed", engine.reaches())
        check("VM-09/c4 off again", scope.services("internet", "off").returncode == 0 and engine.blocked())

        # VM-09: an explicit sbx rule governs.
        listing = rules_of(vm)
        rule = add_deny(vm, "example.com")
        listed_with_rule = rules_of(vm)
        governed = scope.internet()
        check("VM-09/c1 governed:true and effective names the rule", governed["governed"] is True and rule in governed["effective"], json.dumps(governed))
        refused = scope.services("internet", "on")
        check("VM-09/c2 internet on exits non-zero with a `marsh:` message", refused.returncode != 0 and refused.stderr.startswith("marsh:"),
              f"rc={refused.returncode} {refused.stderr.strip()[:160]}")
        refused_off = scope.services("internet", "off")
        check("VM-09 internet off is refused too", refused_off.returncode != 0 and refused_off.stderr.startswith("marsh:"), refused_off.stderr.strip()[:100])
        check("VM-09/c2 the policy list is byte-identical", rules_of(vm) == listed_with_rule)
        check("VM-09/c3 egress stays denied (grant off, gate closed)", engine.blocked(), json.dumps(engine.probe()))
        check("VM-09 the grant did not change", scope.internet()["on"] is False)
        check("VM-09 rule removed by exact id", remove_rule(vm, rule) and rules_of(vm) == listing, rule)
        check("VM-09 governance clears when the rule goes", scope.internet()["governed"] is False)
        # With the gate open the sbx rule still wins (the toggle never widens sbx policy).
        check("on again", scope.services("internet", "on").returncode == 0)
        rule = add_deny(vm, "example.com")
        probe = engine.probe()
        check("VM-09 gate open, sbx rule denies example.com", probe["http"].startswith("000"), json.dumps(probe))
        check("VM-09 gate open, other hosts still reachable (only the rule narrows)", engine.header_names()[:1] == ["Accept"], "httpbin reachable")
        check("VM-09 off refused while governed (grant stays on)", scope.services("internet", "off").returncode != 0 and scope.internet()["on"] is True)
        check("rule removed by exact id", remove_rule(vm, rule))
        check("off works again once nothing governs", scope.services("internet", "off").returncode == 0 and engine.blocked())

        # The grant belongs to the VM's data: rm forgets it.
        check("on before rm", scope.services("internet", "on").returncode == 0)
        engine.stop()
        engine = None
        removed = scope.services("rm", "--yes")
        check("rm removes the VM", removed.returncode == 0)
        grants = scope.home / "services-internet.json"
        recorded = json.loads(grants.read_text())["projects"] if grants.exists() else {}
        check("rm forgets the project's grant", str(scope.project) not in recorded, json.dumps(recorded))
        restarted = json.loads(scope.services("start", "--json").stdout)
        check("a project started again is off", scope.internet()["on"] is False and restarted["vm"] != vm, json.dumps(scope.internet()))
    finally:
        if engine is not None:
            engine.stop()
        scope.teardown()


def scenario_failclosed(ctx) -> None:
    shim = ctx.work / "sbx-unreadable-policy.sh"
    shim.write_text(f"#!/bin/sh\nif [ \"$1\" = policy ] && [ \"$2\" = ls ]; then echo 'policy store unavailable' >&2; exit 1; fi\nexec {SBX} \"$@\"\n")
    shim.chmod(0o755)
    scope = Scope(ctx.work, "failclosed", ctx.prefix_normal, sbx_path=str(shim))
    try:
        start_project(scope)
        grants = scope.home / "services-internet.json"
        status = scope.internet()
        check("fail closed: status governed:true when sbx policy cannot be read", status["governed"] is True and "treated as governed" in status["effective"], json.dumps(status))
        refused = scope.services("internet", "on")
        check("fail closed: internet on is refused with a `marsh:` message", refused.returncode != 0 and refused.stderr.startswith("marsh:") and "treated as governed" in refused.stderr, refused.stderr.strip()[:200])
        check("fail closed: nothing was recorded or opened", not grants.exists() and scope.internet()["on"] is False)
    finally:
        scope.teardown()


def scenario_registry(ctx) -> None:
    """The registry allowlist follows the gate, through the real relay -> E3 filter -> engine path."""
    scope = Scope(ctx.work, "registry", ctx.prefix_normal)
    engine = None
    relay = None
    try:
        vm, _ = start_project(scope)
        engine = Engine(scope, Build(ctx.driver_normal, ctx.artifacts, scope.project), vm)
        check("engine ready", engine.ready is not None)
        config = json.loads(eng(vm, "cat /run/marsh-svc/engine-config.json"))
        check("engine-up wrote the closed list from --internet off (one source)", config == {"registry_allow": ["docker.io", "ghcr.io"]}, json.dumps(config))
        relay = Relay(vm)
        check("relay ready", relay.ready is not None and relay.ready.get("type") == "ready", json.dumps(relay.ready))
        authority = {"project": str(scope.project), "workspace": "base", "attempt": "a1", "kind": "job"}

        def pull(reference: str, tag: str) -> tuple[int | None, str]:
            return relay.call(authority, "POST", f"/images/create?fromImage={reference}&tag={tag}")

        def pulled(result: tuple[int | None, str]) -> bool:
            return result[0] == 200 and '"errorDetail"' not in result[1] and ("Digest: sha256:" in result[1] or "up to date" in result[1])

        hub = pull("docker.io/library/busybox", "sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec")
        check("gate closed: Docker Hub pull through relay -> filter works", pulled(hub), f"{hub[0]} {hub[1][-90:]!r}")
        ghcr = pull("ghcr.io/linuxcontainers/alpine", "latest")
        check("gate closed: ghcr.io pull through relay -> filter works", pulled(ghcr), f"{ghcr[0]} {ghcr[1][-90:]!r}")
        quay = pull("quay.io/prometheus/busybox", "latest")
        check("gate closed: a pull from another registry is refused by the filter (403 marsh:)",
              quay[0] == 403 and "marsh:" in quay[1] and "registry" in quay[1], f"{quay[0]} {quay[1][-100:]!r}")
        check("gate closed: nothing was pulled from it", "quay.io/prometheus/busybox" not in prod(vm, "image ls --format '{{.Repository}}'"))

        check("host: internet on", scope.services("internet", "on").returncode == 0)
        config = json.loads(eng(vm, "cat /run/marsh-svc/engine-config.json"))
        check("internet on rewrote the engine configuration (registry_allow null)", config == {"registry_allow": None}, json.dumps(config))
        quay = pull("quay.io/prometheus/busybox", "latest")
        check("gate open, same relay process: the other registry is allowed", pulled(quay), f"{quay[0]} {quay[1][-90:]!r}")
        check("host: internet off", scope.services("internet", "off").returncode == 0)
        config = json.loads(eng(vm, "cat /run/marsh-svc/engine-config.json"))
        check("internet off restored the closed list", config == {"registry_allow": ["docker.io", "ghcr.io"]}, json.dumps(config))
        prod(vm, "rmi -f quay.io/prometheus/busybox:latest")
        quay = pull("quay.io/prometheus/busybox", "latest")
        check("gate closed again, same relay process: refused again", quay[0] == 403 and "marsh:" in quay[1], f"{quay[0]} {quay[1][-100:]!r}")
        # The supervisor holds the file to the gate: a tampered list is a lost gate.
        eng(vm, "echo '{\"registry_allow\":null}' > /run/marsh-svc/engine-config.json")
        lost = engine.driver.wait(lambda e: e.get("state") == "unavailable", 20)
        check("a configuration that disagrees with the gate is a lost gate", lost is not None and "registry_allow" in json.dumps(lost), json.dumps(lost)[:200])
    finally:
        if relay is not None:
            relay.close()
        if engine is not None:
            engine.stop()
        scope.teardown()


def scenario_job(ctx) -> None:
    """SEC-09: a job cannot widen services authority; the Kit's and the services grants are separate."""
    scope = Scope(ctx.work, "job", ctx.prefix_normal, breaks=ctx.job_breaks)
    engine = None
    try:
        # The daemon freezes its command registry at start: write it first.
        commands = scope.control_home()
        commands.mkdir(mode=0o700, exist_ok=True)
        (commands / "commands.json").write_text(json.dumps({"fixture": ctx.fixture_ref}) + "\n")
        vm, _ = start_project(scope)
        policy_before = rules_of(vm)
        status_before = scope.status()["internet"]
        say("INFO", "SEC-09/step1", json.dumps(status_before))
        job = scope.run("-c", "fixture exec sh -c 'marsh services internet on; echo rc=$?; marsh services start; echo start-rc=$?'", timeout=900)
        say("INFO", "SEC-09/step2 job output", (job.stdout + job.stderr).strip()[-400:])
        status_after = scope.status()["internet"]
        unchanged = status_after == status_before and rules_of(vm) == policy_before and status_after["on"] is False
        detail = f"{json.dumps(status_before)} -> {json.dumps(status_after)}"
        if ctx.job_breaks:
            broken("BREAK services-cli-in-job: SEC-09/c1 the snapshots are identical FAILS (the job granted Internet)", unchanged, detail)
            return
        check("SEC-09/c1 the snapshots (services status, sbx policy) are identical after the job's attempts", unchanged, detail)
        check("SEC-09 the job's `internet on` and `services start` were refused by the daemon",
              "rc=1" in job.stdout and "start-rc=1" in job.stdout and "job capability admits only" in job.stderr, (job.stdout + job.stderr).strip()[-200:])

        # Step 4: the Kit's policy denies example.com; the host grants services Internet.
        kit = next(name for name in scope.owned() if "-k-" in name)
        engine = Engine(scope, Build(ctx.driver_normal, ctx.artifacts, scope.project), vm)
        check("engine ready", engine.ready is not None)
        rule = add_deny(kit, "example.com")
        check("host: internet on", scope.services("internet", "on").returncode == 0)
        direct = scope.run("-c", "fixture exec sh -c 'curl -sS -m 10 -o /dev/null -w \"%{http_code}\" https://example.com/; echo \" curl-rc=$?\"'", timeout=300)
        say("INFO", "SEC-09/step4 job direct probe", (direct.stdout + direct.stderr).strip()[-160:])
        # sbx answers a denied request through its proxy with a 403 (or resets the TLS handshake: 000).
        code = direct.stdout.strip()[:3]
        check("SEC-09/c2 the job's direct probe is denied (403 from the sbx proxy, or no connection)", code in ("403", "000"), direct.stdout.strip()[-100:])
        probe = engine.probe()
        check("SEC-09/c2 the services container's probe succeeds", engine.reaches(), json.dumps(probe))
        check("SEC-09/c2 internet.on:true is disclosed", scope.internet()["on"] is True, json.dumps(scope.internet()))
        check("rule removed by exact id", remove_rule(kit, rule))
    finally:
        if engine is not None:
            engine.stop()
        scope.teardown()


def scenario_break(ctx, mode: str) -> None:
    """A test-breaks daemon (and driver) with one mode: the named check must fail."""
    scope = Scope(ctx.work, f"break-{mode}", ctx.prefix_breaks, breaks=[mode])
    engine = None
    try:
        vm, _ = start_project(scope)
        build = Build(ctx.driver_breaks, ctx.artifacts, scope.project)
        if mode == "services-default-on":
            engine = Engine(scope, build, vm)
            status = scope.internet()
            broken("BREAK services-default-on: VM-08/c1 `on:false` FAILS (the project starts on)", status["on"] is False, json.dumps(status))
            broken("BREAK services-default-on: SEC-17/c2 HTTP fails FAILS (a container reaches the internet)", engine.blocked(), json.dumps(engine.probe()))
        elif mode in ("internet-off-noop", "internet-noop"):
            engine = Engine(scope, build, vm)
            on = scope.services("internet", "on")
            if mode == "internet-noop":
                broken("BREAK internet-noop: VM-08/c2 HTTP 200 after on FAILS", engine.reaches(), json.dumps(engine.probe()))
            else:
                opened = engine.probe()
                check("control: on opens the gate", on.returncode == 0 and opened["http"] == "200/0" and opened["dns"] == "resolves",
                      f"rc={on.returncode} {on.stderr.strip()[:100]} {json.dumps(opened)}")
                off = scope.services("internet", "off")
                check("control: off exits 0", off.returncode == 0)
                broken("BREAK internet-off-noop: VM-08/c3 HTTP fails after off FAILS (still open)", engine.blocked(), json.dumps(engine.probe()))
                broken("BREAK internet-off-noop: VM-08/c3 status on:false FAILS", scope.internet()["on"] is False, json.dumps(scope.internet()))
        elif mode == "governance-any-deny":
            denied = single_host_global_denies()
            check("control: the host has a single-host global deny (read-only)", bool(denied), json.dumps(denied))
            shown = scope.internet()
            broken("BREAK governance-any-deny: VM-09/c4 `governed:false` FAILS (any deny governs)", shown["governed"] is False, json.dumps(shown))
            on = scope.services("internet", "on")
            broken("BREAK governance-any-deny: VM-09/c4 `internet on` still works FAILS (it refuses)", on.returncode == 0, f"rc={on.returncode} {on.stderr.strip()[:120]}")
        elif mode == "internet-ignore-governed":
            rule = add_deny(vm, "example.com")
            governed = scope.internet()
            check("control: governed:true names the rule", governed["governed"] is True and rule in governed["effective"])
            refused = scope.services("internet", "on")
            broken("BREAK internet-ignore-governed: VM-09/c2 `internet on` exits non-zero FAILS (it goes ahead)", refused.returncode != 0, f"rc={refused.returncode}")
            remove_rule(vm, rule)
    finally:
        if engine is not None:
            engine.stop()
        scope.teardown()


class Context:
    pass


def main() -> int:
    parser = argparse.ArgumentParser()
    for name in ("prefix-normal", "prefix-breaks", "driver-normal", "driver-breaks", "artifacts", "fixture-ref", "work"):
        parser.add_argument(f"--{name}", required=True)
    parser.add_argument("--only", default="")
    parser.add_argument("--job-break", default="")
    args = parser.parse_args()
    ctx = Context()
    ctx.work = Path(args.work).resolve()
    ctx.work.mkdir(parents=True, exist_ok=True)
    ctx.prefix_normal, ctx.prefix_breaks = Path(args.prefix_normal), Path(args.prefix_breaks)
    ctx.driver_normal, ctx.driver_breaks, ctx.artifacts = args.driver_normal, args.driver_breaks, Path(args.artifacts)
    ctx.fixture_ref = args.fixture_ref
    ctx.job_breaks = [args.job_break] if args.job_break else []
    ctx.prefix_normal = ctx.prefix_breaks if ctx.job_breaks else ctx.prefix_normal
    only = {name for name in args.only.split(",") if name}
    scenarios = {"main": scenario_main, "failclosed": scenario_failclosed, "registry": scenario_registry, "job": scenario_job}
    for name, function in scenarios.items():
        if only and name not in only:
            continue
        say("INFO", f"scenario/{name}", "start")
        try:
            function(ctx)
        except Exception as error:  # noqa: BLE001 - a failed scenario is a failed check, cleanup already ran
            say("FAIL", f"scenario/{name}", f"{type(error).__name__}: {error}")
    for mode in [name.removeprefix("break:") for name in sorted(only) if name.startswith("break:")]:
        say("INFO", f"scenario/break:{mode}", "start")
        try:
            scenario_break(ctx, mode)
        except Exception as error:  # noqa: BLE001
            say("FAIL", f"scenario/break:{mode}", f"{type(error).__name__}: {error}")
    print("ALL PASSED" if not s2.FAILURES else f"FAILED: {s2.FAILURES}")
    return 1 if s2.FAILURES else 0


if __name__ == "__main__":
    sys.exit(main())
