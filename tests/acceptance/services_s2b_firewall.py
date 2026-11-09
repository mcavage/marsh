#!/usr/bin/env python3
"""Live witnesses for the engine egress firewall (S2b, SECURITY-DESIGN A12), against real sbx.

sbx attaches Mark's global credentials and the MCP gateway to every sandbox, reachable through
one proxy (`gateway.docker.internal:3128`). `engine-up` drops every TCP connection to the
gateway's addresses (except port 53) before the production engine starts, for the engine itself
and for every container network. These checks prove it from the positions that matter and prove
that the same checks detect the hole under the break mode `engine-firewall-off`.

Nothing here prints a credential, a URL or a response body: every observation is an HTTP status
code, a curl exit code, a count or a boolean. The only authenticated calls are read-only
`GET api.github.com/user` (status only) and MCP `initialize` + `tools/list` (a tool count only; no
tool is ever called).

Needs the same two builds as `services_s2_engine.py` (a normal driver and guest `marsh`, and a
`test-breaks` pair), plus `--legacy-artifacts`: an artifact directory whose engine predates the
firewall (the S2 engine), to show that the daemon never makes such an engine ready.

The whole run holds the exclusive VM lock (`harness/vmlock.py`, as a library) for its whole
lifetime, cleanup included. The sandbox is `nfx-s2b-<run id>-fw`, created with S1's arguments
(`DOCKER_SANDBOXES_DOCKER_SIZE=20g sbx create --name N --pull never --skills off --static-mcp none
--template T shell`, then `sbx mount` of the project); its name and UUID are recorded at create
and only that sandbox is removed.

  services_s2b_firewall.py --driver-normal D --artifacts-normal A --driver-breaks D \
      --artifacts-breaks A --legacy-artifacts A --project DIR [--only NAME[,NAME]]

Every check prints `PASS name: detail` or `FAIL name: detail`; the exit status is non-zero if any
failed.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import secrets
import sys
import tempfile
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path.home() / "dev/narwhal-fork-spike/harness"))
import services_s2_engine as s2  # noqa: E402  (shared helpers: sandboxes, driver, checks)
import vmlock  # noqa: E402

from services_s2_engine import (  # noqa: E402
    Build, Driver, Sandbox, check, eng, jload, prod, ready, run, say, unavailable, vx,
)

CURL_IMAGE = "curlimages/curl:8.11.1"
PYTHON_IMAGE = "python:3.13-alpine"
PULL_IMAGE = "busybox:1.37"
BUSY_IMAGE = PULL_IMAGE
USER_NET = "nfx-fw-net"
SCRATCH_IN_VM = "/tmp/nfx-fw"

# Runs inside curlimages/curl. Prints `key=<http code>/<curl exit>`: statuses only.
CURL_WITNESS = r"""#!/bin/sh
code() { c=$(curl -s -o /dev/null -w '%{http_code}' --connect-timeout 5 -m 8 "$@" 2>/dev/null); rc=$?; echo "$c/$rc"; }
bare() { curl -s -o /dev/null --connect-timeout 4 -m 5 "$@" 2>/dev/null; echo "$?"; }
echo "github_by_name=$(code -k -x http://gateway.docker.internal:3128 https://api.github.com/user)"
echo "github_by_ipv4=$(code -k -x http://$GW4:3128 https://api.github.com/user)"
if [ -n "$GW6" ]; then echo "github_by_ipv6=$(code -k -x "http://[$GW6]:3128" https://api.github.com/user)"; fi
echo "github_port80=$(code -k -x http://$GW4:80 https://api.github.com/user)"
echo "github_port8080=$(code -k -x http://$GW4:8080 https://api.github.com/user)"
echo "github_port443=$(code -k -x http://$GW4:443 https://api.github.com/user)"
if [ -n "$GWALT" ]; then echo "github_by_engine_default_gw=$(code -k -x http://$GWALT:3128 https://api.github.com/user)"; fi
echo "tcp3128_exit=$(bare http://$GW4:3128/)"
echo "tcp53_exit=$(bare http://$GW4:53/)"
echo "example_direct=$(code https://example.com/)"
if getent hosts example.com >/dev/null 2>&1; then echo "dns_example=ok"; else echo "dns_example=fail"; fi
"""

# Runs inside python:3.13-alpine. MCP `initialize` + `tools/list` through the proxy, as a hostile
# container would. Prints a tool count, an HTTP status or an exception class name: never a URL,
# header or body. No tool is called.
MCP_WITNESS = r"""
import json, os, sys, urllib.error, urllib.request
url = os.environ["MCPU"]
proxy = "http://gateway.docker.internal:3128"
opener = urllib.request.build_opener(urllib.request.ProxyHandler({"http": proxy, "https": proxy}))
def post(body, session=None):
    headers = {"content-type": "application/json", "accept": "application/json, text/event-stream"}
    if session:
        headers["mcp-session-id"] = session
    request = urllib.request.Request(url, data=json.dumps(body).encode(), headers=headers, method="POST")
    response = opener.open(request, timeout=8)
    return response.status, response.headers.get("mcp-session-id"), response.read().decode("utf-8", "replace")
def payload(text):
    for line in text.splitlines():
        if line.startswith("data:"):
            text = line[5:].strip()
            break
    return json.loads(text)
try:
    status, session, _ = post({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "nfx-fw-witness", "version": "0"}}})
    post({"jsonrpc": "2.0", "method": "notifications/initialized"}, session)
    status, _, text = post({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}, session)
    print("mcp_tools=%d" % len(payload(text)["result"]["tools"]))
except urllib.error.HTTPError as error:
    print("mcp_http=%d" % error.code)
except Exception as error:
    print("mcp_blocked=%s" % type(error).__name__)
"""


class Vm:
    """The firewall run's one sandbox, created the way S1 creates a services VM."""

    def __init__(self, name: str, template: str, project: Path):
        self.box = Sandbox(name, template, project)
        self.name = name

    def create(self) -> None:
        box = self.box
        if box.uuid is not None:
            return
        if box.name in s2.inventory():
            raise RuntimeError(f"{box.name} already exists; not ours, aborting")
        s2.OWNED[box.name] = box
        env = {**os.environ, "DOCKER_SANDBOXES_DOCKER_SIZE": "20g"}
        result = s2.subprocess.run(
            ["sbx", "create", "--name", box.name, "--pull", "never", "--skills", "off", "--static-mcp", "none",
             "--template", box.template, "shell"], capture_output=True, text=True, timeout=600, env=env)
        row = s2.inventory().get(box.name)
        if result.returncode or row is None:
            raise RuntimeError(f"create {box.name}: {result.stderr.strip()[:200]}")
        box.uuid = row["id"]
        say("INFO", "owned-sandbox", f"{box.name} uuid={box.uuid}")
        mounted = run(["sbx", "mount", box.name, str(box.project)])
        check("vm/project-mounted", mounted.returncode == 0, (mounted.stderr or mounted.stdout).strip()[:120])
        put(box.name, "fw-curl.sh", CURL_WITNESS)
        put(box.name, "fw-mcp.py", MCP_WITNESS)

    def verify(self) -> None:
        self.box.verify()


def put(vm: str, name: str, text: str) -> str:
    """A file in the project mount (visible in the VM, the engine and its containers at the same path)."""
    path = f"{PROJECT_DIR}/{name}"
    Path(path).write_text(text)
    return path


def table(output: str) -> dict[str, str]:
    result = {}
    for line in output.splitlines():
        if "=" in line:
            key, _, value = line.partition("=")
            result[key.strip()] = value.strip()
    return result


def status_of(value: str | None) -> tuple[str, int]:
    """`200/0` -> (`200`, 0); anything unparsable -> (`?`, -1)."""
    try:
        code, rc = (value or "").split("/")
        return code, int(rc)
    except ValueError:
        return "?", -1


def reported_gateway(driver: Driver) -> tuple[str, str, list[str]]:
    """The gateway addresses the engine reported in its `ready` (first IPv4, first IPv6, all).

    The engine resolves with getaddrinfo (A and AAAA); `getent ahosts` hides AAAA records in a
    container with no IPv6 address, so it is not used as the source."""
    event = driver.wait(lambda e: e.get("probe") == "firewall", 10)
    addresses = list(((event or {}).get("report") or {}).get("addresses", []))
    v4 = [a for a in addresses if a.count(".") == 3]
    v6 = [a for a in addresses if ":" in a]
    return (v4[0] if v4 else "", v6[0] if v6 else "", addresses)


def drop_hits(vm: str, protocol: str | None = None) -> int | None:
    """Packets the DROP rules of both families have counted, optionally for one protocol
    (None if the chain is absent)."""
    total, seen = 0, False
    match = f" && $4 == \"{protocol}\"" if protocol else ""
    for tool in ("iptables", "ip6tables"):
        out = eng(vm, f"{tool} -w -L MARSH-EGRESS -n -v -x 2>/dev/null | awk '$3 == \"DROP\"{match} {{print $1}}'")
        for word in out.split():
            if word.isdigit():
                total, seen = total + int(word), True
    return total if seen else None


def zero_counters(vm: str) -> None:
    eng(vm, "iptables -w -Z MARSH-EGRESS 2>/dev/null; ip6tables -w -Z MARSH-EGRESS 2>/dev/null; true")


def run_curl_witness(vm: str, network: str, v4: str, v6: str) -> dict[str, str]:
    script = f"{PROJECT_DIR}/fw-curl.sh"
    # The engine's own default gateway (the boot engine's bridge): another address a proxy might hide behind.
    alt = eng(vm, "ip route show default | awk '{print $3; exit}'")
    alt = alt if alt.count(".") == 3 else ""
    out = prod(vm, f"run --rm --network {network} -v {PROJECT_DIR}:{PROJECT_DIR}:ro -e GW4={v4} -e GW6={v6} -e GWALT={alt} "
                   f"{CURL_IMAGE} sh {script}", 240)
    return table(out)


def run_mcp_witness(vm: str, network: str) -> str:
    """`mcp_tools=N`, `mcp_http=N` or `mcp_blocked=Class`. The URL travels in a variable, never printed."""
    script = f"{PROJECT_DIR}/fw-mcp.py"
    result = run(["sbx", "exec", "-u", "root", vm, "sh", "-c",
                  f"docker exec -e MCPU=\"$MCP_GATEWAY_URL\" marsh-svc-engine docker -H {s2.PROD_SOCKET} run --rm "
                  f"--network {network} -v {PROJECT_DIR}:{PROJECT_DIR}:ro -e MCPU {PYTHON_IMAGE} python3 {script} 2>&1 | grep -E '^mcp_' | tail -1"],
                 240)
    return result.stdout.strip() or "mcp_none"


def run_udp_witness(vm: str, network: str, v4: str) -> None:
    """Datagrams to non-DNS gateway ports from a container (what the UDP DROP rules count)."""
    prod(vm, f"run --rm --network {network} {BUSY_IMAGE} sh -c "
             f"'for port in 3128 8080 443; do echo x | nc -u -w 1 {v4} $port; done; true'", 240)


def engine_netns_raw(vm: str, v4: str, v6: str) -> dict[str, str]:
    """TCP connects made by a process of the engine container itself (its own netns): `connected`,
    `refused`, `timeout` (what a DROP looks like) or `unreachable`."""
    attempts = {"tcp3128_v4": (v4, 3128), "tcp80_v4": (v4, 80), "tcp8080_v4": (v4, 8080), "tcp53_v4": (v4, 53)}
    if v6:
        attempts["tcp3128_v6"] = (v6, 3128)
    out = {}
    for key, (address, port) in attempts.items():
        # bash's /dev/tcp: a connect and nothing else. 124 is `timeout` killing a hung connect.
        text = eng(vm, f"timeout 4 bash -c 'exec 3<>/dev/tcp/{address}/{port}' 2>/dev/null; echo $?", 30)
        code = text.splitlines()[-1] if text else "?"
        out[key] = {"0": "connected", "124": "timeout", "1": "refused-or-unreachable"}.get(code, f"exit{code}")
    return out


class Witness:
    """The observations from every position, with the firewall expected up (blocked) or down."""

    def __init__(self, vm: str, label: str, blocked: bool):
        self.vm, self.label, self.blocked = vm, label, blocked

    def tag(self, name: str) -> str:
        return f"{self.label}/{name}"

    def position(self, name: str, network: str, v4: str, v6: str) -> None:
        vm = self.vm
        zero_counters(vm)
        seen = run_curl_witness(vm, network, v4, v6)
        hits = drop_hits(vm)
        say("INFO", self.tag(f"{name}/observations"), json.dumps(seen, sort_keys=True))
        gh = {key: status_of(value) for key, value in seen.items() if key.startswith("github_")}
        if self.blocked:
            for key in ("github_by_name", "github_by_ipv4"):
                code, rc = gh.get(key, ("?", -1))
                check(self.tag(f"{name}/{key}-blocked"), code == "000" and rc == 28, f"status {code}, curl exit {rc} (28 = dropped)")
            if v6:
                code, rc = gh.get("github_by_ipv6", ("?", -1))
                check(self.tag(f"{name}/github_by_ipv6-blocked"), code != "200" and rc != 0, f"status {code}, curl exit {rc}")
            for key in ("github_port80", "github_port8080", "github_port443"):
                code, rc = gh.get(key, ("?", -1))
                check(self.tag(f"{name}/{key}-blocked"), code == "000" and rc == 28, f"status {code}, curl exit {rc} (28 = dropped)")
            check(self.tag(f"{name}/tcp3128-dropped"), seen.get("tcp3128_exit") == "28", f"curl exit {seen.get('tcp3128_exit')}")
            check(self.tag(f"{name}/drop-rules-counted-the-attempts"), hits is not None and hits > 0, f"DROP packet count > 0: {hits is not None and hits > 0}")
        else:
            code, rc = gh.get("github_by_name", ("?", -1))
            check(self.tag(f"{name}/github_by_name-acts-as-the-user"), code == "200", f"status {code}, curl exit {rc} (the witness detects the hole)")
            code, rc = gh.get("github_by_ipv4", ("?", -1))
            check(self.tag(f"{name}/github_by_ipv4-acts-as-the-user"), code == "200", f"status {code}, curl exit {rc}")
            check(self.tag(f"{name}/tcp3128-connects"), seen.get("tcp3128_exit") not in (None, "28", "7"), f"curl exit {seen.get('tcp3128_exit')}")
        code, rc = gh.get("github_by_engine_default_gw", ("-", 0))
        check(self.tag(f"{name}/engine-default-gateway-is-not-a-proxy"), code != "200", f"status {code}, curl exit {rc} (the boot engine's bridge address is not a second way to the proxy)")
        # Not firewalled in either mode: DNS over TCP to the gateway, normal egress, name resolution.
        check(self.tag(f"{name}/tcp53-not-dropped"), seen.get("tcp53_exit") not in (None, "28"), f"curl exit {seen.get('tcp53_exit')} (28 would be a drop)")
        code, rc = status_of(seen.get("example_direct"))
        check(self.tag(f"{name}/example.com-works"), code == "200" and rc == 0, f"status {code}, curl exit {rc}")
        check(self.tag(f"{name}/dns-works"), seen.get("dns_example") == "ok")

    def udp(self, name: str, network: str, v4: str) -> None:
        zero_counters(self.vm)
        run_udp_witness(self.vm, network, v4)
        hits = drop_hits(self.vm, "udp")
        if self.blocked:
            check(self.tag(f"{name}/udp-to-gateway-dropped"), hits is not None and hits > 0, f"UDP DROP packet count > 0: {hits is not None and hits > 0}")
        else:
            check(self.tag(f"{name}/udp-not-dropped-without-the-firewall"), hits is None, "no rules, so no counters")

    def mcp(self, name: str, network: str) -> None:
        result = run_mcp_witness(self.vm, network)
        say("INFO", self.tag(f"{name}/mcp"), result)
        if self.blocked:
            check(self.tag(f"{name}/mcp-tools-list-blocked"), result.startswith("mcp_blocked="), result)
        else:
            check(self.tag(f"{name}/mcp-tools-list-reachable"), result.startswith("mcp_tools=") and int(result.split("=")[1]) > 0, result)

    def engine_itself(self, v4: str, v6: str) -> None:
        zero_counters(self.vm)
        eng(self.vm, f"timeout 4 bash -c 'exec 3<>/dev/udp/{v4}/3128; echo x >&3' 2>/dev/null; true", 30)
        hits = drop_hits(self.vm, "udp")
        if self.blocked:
            check(self.tag("engine/udp-to-gateway-dropped"), hits is not None and hits > 0, f"UDP DROP packet count > 0: {hits is not None and hits > 0}")
        else:
            check(self.tag("engine/udp-not-dropped-without-the-firewall"), hits is None)
        raw = engine_netns_raw(self.vm, v4, v6)
        say("INFO", self.tag("engine/raw-connects"), json.dumps(raw, sort_keys=True))
        if self.blocked:
            for key in ("tcp3128_v4", "tcp80_v4", "tcp8080_v4"):
                check(self.tag(f"engine/{key}-dropped"), raw[key] == "timeout", raw[key])
            if v6:
                check(self.tag("engine/tcp3128_v6-not-connected"), raw["tcp3128_v6"] != "connected", raw["tcp3128_v6"])
            check(self.tag("engine/tcp53_v4-not-dropped"), raw["tcp53_v4"] != "timeout", raw["tcp53_v4"])
        else:
            check(self.tag("engine/tcp3128_v4-connects"), raw["tcp3128_v4"] == "connected", raw["tcp3128_v4"])


def rules_present(vm: str, addresses: list[str]) -> dict[str, bool]:
    """Booleans about the rules inside the engine, per family and per reported address."""
    first = "iptables -w -S {chain} | grep -- '^-A' | sed -n 1p"
    result = {
        "chain-v4": eng(vm, "iptables -w -S MARSH-EGRESS >/dev/null 2>&1 && echo yes || echo no") == "yes",
        "output-jump-v4": eng(vm, first.format(chain="OUTPUT")) == "-A OUTPUT -j MARSH-EGRESS",
        "docker-user-jump-v4": eng(vm, first.format(chain="DOCKER-USER")) == "-A DOCKER-USER -j MARSH-EGRESS",
        "forward-reaches-v4": eng(vm, first.format(chain="FORWARD")) in ("-A FORWARD -j DOCKER-USER", "-A FORWARD -j MARSH-EGRESS"),
    }
    if any(":" in a for a in addresses):
        v6 = first.replace("iptables", "ip6tables")
        result["chain-v6"] = eng(vm, "ip6tables -w -S MARSH-EGRESS >/dev/null 2>&1 && echo yes || echo no") == "yes"
        result["output-jump-v6"] = eng(vm, v6.format(chain="OUTPUT")) == "-A OUTPUT -j MARSH-EGRESS"
        result["docker-user-jump-v6"] = eng(vm, v6.format(chain="DOCKER-USER")) == "-A DOCKER-USER -j MARSH-EGRESS"
        result["forward-reaches-v6"] = eng(vm, v6.format(chain="FORWARD")) in ("-A FORWARD -j DOCKER-USER", "-A FORWARD -j MARSH-EGRESS")
    for address in addresses:
        tool = "ip6tables" if ":" in address else "iptables"
        for protocol in ("tcp", "udp"):
            result[f"{protocol}-drop-rule-for-{'v6' if ':' in address else 'v4'}-address"] = eng(
                vm, f"{tool} -w -C MARSH-EGRESS -d {address} -p {protocol} ! --dport 53 -j DROP && echo yes || echo no") == "yes"
    return result


def full_battery(ctx, vm: str, label: str, blocked: bool, v4: str, v6: str) -> None:
    say("INFO", f"{label}/gateway", f"ipv4 resolved={bool(v4)}, ipv6 resolved={bool(v6)}")
    check(f"{label}/gateway-resolved", bool(v4))
    witness = Witness(vm, label, blocked)
    # The default bridge: a container with no network option.
    witness.position("default-bridge", "bridge", v4, v6)
    witness.udp("default-bridge", "bridge", v4)
    witness.mcp("default-bridge", "bridge")
    # A user-defined network (its own bridge, its own subnet).
    prod(vm, f"network rm {USER_NET} >/dev/null 2>&1; true")
    created = prod(vm, f"network create {USER_NET}")
    check(f"{label}/user-network-created", len(created.strip()) == 64, "network id returned")
    witness.position("user-network", USER_NET, v4, v6)
    witness.udp("user-network", USER_NET, v4)
    witness.mcp("user-network", USER_NET)
    prod(vm, f"network rm {USER_NET} >/dev/null 2>&1; true")
    # The engine itself: its own processes, and a container sharing its netns (host mode in the
    # production engine is the engine container's netns).
    witness.engine_itself(v4, v6)
    witness.position("engine-netns-container", "host", v4, v6)
    witness.udp("engine-netns-container", "host", v4)
    witness.mcp("engine-netns-container", "host")


# --- scenarios ---------------------------------------------------------------

def vm_proxy_reaches_github(vm: str) -> str:
    """Baseline from the VM itself (its sbx proxy environment): the GitHub secret is attached. Status only."""
    out = vx(vm, "c=$(curl -s -o /dev/null -w '%{http_code}' -k -m 10 https://api.github.com/user 2>/dev/null); echo \"code=$c\"")
    return out.splitlines()[-1] if out else "code=?"


def scenario_firewall(ctx, vm: Vm) -> None:
    build = ctx.normal
    vm.create()
    name = vm.name
    baseline = vm_proxy_reaches_github(name)
    check("baseline/vm-sbx-proxy-acts-as-the-user", baseline == "code=200",
          f"{baseline} (the VM's own proxy environment; if this is not 200 the 'blocked' checks below prove nothing)")
    driver = Driver(build, name)
    up = driver.wait(ready, 300)
    opened = driver.wait(lambda e: e.get("probe") == "channel-gate" and e["open"], 10)
    check("firewall/ready", up is not None and opened is not None, json.dumps({"state": up and up.get("state")}))
    v4, v6, addresses = reported_gateway(driver)
    report = driver.wait(lambda e: e.get("probe") == "firewall", 5)
    body = (report or {}).get("report") or {}
    check("firewall/ready-report-covers-the-gateway", bool(body) and body.get("verified") is True and body.get("connect_blocked") is True and bool(v4),
          f"verified={body.get('verified')} connect_blocked={body.get('connect_blocked')} ipv4 addresses={sum(a.count('.') == 3 for a in addresses)} ipv6 addresses={sum(':' in a for a in addresses)}")
    vm_v4 = vx(name, "getent ahostsv4 gateway.docker.internal | awk 'NR==1{print $1}'")
    check("firewall/gateway-resolved-per-start-matches-the-vm", vm_v4 == v4, "the engine's reported IPv4 equals the VM's own resolution of the gateway")
    present = rules_present(name, addresses)
    check("firewall/rules-present-for-every-reported-address", all(present.values()) and len(present) >= 6, json.dumps(present, sort_keys=True))
    log = vx(name, "docker logs marsh-svc-engine 2>&1 | grep -o '\"stage\":\"[a-z-]*\"' | tr '\\n' ' '")
    order = [word.split(":")[1].strip('"') for word in log.split()]
    check("firewall/stage-order-firewall-before-dockerd", "firewall" in order and "dockerd-started" in order and order.index("firewall") < order.index("dockerd-started"), " ".join(order))
    # The live witnesses, firewall up.
    full_battery(ctx, name, "firewall-on", True, v4, v6)
    pulled = prod(name, f"pull {PULL_IMAGE}")
    check("firewall-on/production-image-pull-works", "Digest: sha256:" in pulled or "Status:" in pulled, pulled.splitlines()[-1][:80] if pulled else "")
    check("firewall-on/pulled-image-listed", PULL_IMAGE in prod(name, "image ls --format '{{.Repository}}:{{.Tag}}'"))
    check("firewall-on/vm-level-proxy-path-untouched", baseline == "code=200" and vm_proxy_reaches_github(name) == "code=200",
          "the VM's own proxy path (outside the engine) is untouched: the firewall is in the engine's netns only")
    still = driver.wait(lambda e: e.get("state") == "unavailable", 1)
    check("firewall-on/engine-stays-ready-under-the-witnesses", still is None and driver.process.poll() is None)
    driver.stop()


def scenario_loss(ctx, vm: Vm) -> None:
    """Losing or narrowing the rules revokes readiness like losing the engine: each case on a fresh generation.

    The narrowing cases replace a real DROP rule by one that no longer matches the traffic of user
    containers (FORWARD) though it may still drop the engine's own (OUTPUT): the shape a search for
    "some rule mentioning the gateway" would accept."""
    vm.create()
    vm.verify()
    name = vm.name
    engine_ip = "$(ip -4 -o addr show dev eth0 | awk '{print $4}' | cut -d/ -f1)"
    count = "iptables -w -S MARSH-EGRESS | grep -c -- '{marker}'"
    cases = (
        ("chain-flushed", "iptables -w -F MARSH-EGRESS", None),
        ("output-jump-removed", "iptables -w -D OUTPUT -j MARSH-EGRESS", None),
        ("docker-user-jump-removed", "iptables -w -D DOCKER-USER -j MARSH-EGRESS", None),
        ("chain-rule-deleted-by-address", "iptables -w -D MARSH-EGRESS 1", None),
        ("ipv6-chain-flushed", "ip6tables -w -F MARSH-EGRESS", None),
        ("source-restricted-rule", f"iptables -w -R MARSH-EGRESS 1 -s {engine_ip}/32 -d {{gw}} -p tcp ! --dport 53 -j DROP; " + count.format(marker="-s "), "recover"),
        ("nonmatching-source-rule", "iptables -w -R MARSH-EGRESS 1 -s 192.0.2.1/32 -d {gw} -p tcp ! --dport 53 -j DROP; " + count.format(marker="-s 192.0.2.1"), None),
        ("interface-restricted-rule", "iptables -w -R MARSH-EGRESS 1 -d {gw} -o eth0 -p tcp ! --dport 53 -j DROP; " + count.format(marker="-o eth0"), None),
        ("udp-rule-narrowed", f"iptables -w -R MARSH-EGRESS 2 -s {engine_ip}/32 -d {{gw}} -p udp ! --dport 53 -j DROP; " + count.format(marker="-s "), None),
    )
    for what, command, recover in cases:
        tag = f"loss/{what}"
        driver = Driver(ctx.normal, name)
        up = driver.wait(ready, 180)
        opened = driver.wait(lambda e: e.get("probe") == "channel-gate" and e["open"], 10)
        if not check(f"{tag}/ready-and-gate-open-first", up is not None and opened is not None):
            driver.stop()
            continue
        v4, v6, _ = reported_gateway(driver)
        if what.startswith("ipv6") and not v6:
            say("INFO", f"{tag}/skipped", "the engine reported no IPv6 gateway address in this VM")
            driver.stop()
            continue
        cursor = len(driver.events)
        mark = time.monotonic()
        out = eng(name, command.replace("{gw}", v4))
        if "-R MARSH-EGRESS" in command:
            check(f"{tag}/corruption-applied", out.strip().endswith("1"), f"the replaced rule is in the chain: {out.strip()[-3:]!r} (count of rules with the narrowing match)")
        lost = driver.wait(unavailable, 30, after=cursor)
        took = int((time.monotonic() - mark) * 1000)
        check(f"{tag}/unavailable-after-the-loss", lost is not None and "egress firewall" in (lost or {}).get("reason", ""), f"{took} ms; reason mentions the firewall={'egress firewall' in (lost or {}).get('reason', '')}")
        closed = driver.wait(lambda e: e.get("probe") == "channel-gate" and not e["open"], 10, after=cursor)
        later = [e for e in driver.events[cursor:] if e.get("probe") == "channel-gate"]
        check(f"{tag}/gate-closed-and-never-ready-again", closed is not None and later and later[-1]["open"] is False
              and "ready" not in [e["state"] for e in driver.events[cursor:] if "state" in e], f"gate events after the loss: {[e['open'] for e in later]}")
        driver.stop()
        if recover:
            # Recovery: a new generation installs the canonical rules again, and the same GitHub
            # witness from a user container is blocked again.
            again = Driver(ctx.normal, name)
            up = again.wait(ready, 180)
            check(f"{tag}/recovered-ready", up is not None)
            v4, v6, _ = reported_gateway(again)
            Witness(name, f"{tag}/recovered", True).position("default-bridge", "bridge", v4, v6)
            again.stop()


def scenario_audit(ctx, vm: Vm) -> None:
    """The daemon's own audit refuses a `ready` whose report is verified but whose rules were narrowed.

    `hold-engine-before-ready` stops `engine-up` after it verified its rules and before it reports
    `ready` (and before its supervision starts). While held, the real DROP rule is replaced by a
    source-restricted one; on release the engine reports `ready` with a verified report, and only
    the daemon's own read of `iptables -S` (through the boot engine, by container id) can refuse it.
    The engine's `lost` could never be the reason: the daemon handles `ready` before it reads further."""
    vm.create()
    vm.verify()
    name = vm.name
    s2.clear_engine(name)
    driver = Driver(ctx.breaks, name, breaks=["hold-engine-before-ready"])
    reached = s2.hold_reached(name)
    if not check("audit/hold-reached", reached):
        driver.stop()
        return
    gateway = vx(name, "getent ahostsv4 gateway.docker.internal | awk 'NR==1{print $1}'")
    applied = eng(name, f"iptables -w -R MARSH-EGRESS 1 -s 192.0.2.1/32 -d {gateway} -p tcp ! --dport 53 -j DROP; "
                        "iptables -w -S MARSH-EGRESS | grep -c -- '-s 192.0.2.1'")
    check("audit/rule-narrowed-while-held", applied.strip().endswith("1"), f"narrowed rules in the chain: {applied.strip()[-3:]!r}")
    s2.release(name)
    down = driver.wait(unavailable, 120)
    reason = (down or {}).get("reason", "")
    check("audit/daemon-refuses-a-ready-with-narrowed-rules", down is not None and "the engine firewall audit failed" in reason,
          f"reason is the daemon's audit: {'the engine firewall audit failed' in reason}; never ready: {'ready' not in driver.states()}")
    check("audit/never-ready-and-gate-never-open", "ready" not in driver.states() and not any(e["open"] for e in driver.gate_events()), f"states={driver.states()}")
    driver.stop()
    s2.clear_engine(name)


def scenario_legacy(ctx, vm: Vm) -> None:
    """An engine that reports `ready` without a firewall (the S2 engine) is never made ready."""
    vm.create()
    vm.verify()
    driver = Driver(ctx.legacy, vm.name)
    down = driver.wait(unavailable, 240)
    states = driver.states()
    check("legacy/refused-without-a-firewall-report", down is not None and "firewall" in down.get("reason", ""), (down or {}).get("reason", "")[:140])
    check("legacy/never-ready-and-gate-never-open", "ready" not in states and not any(e["open"] for e in driver.gate_events()), f"states={states}")
    driver.stop()
    vx(vm.name, "docker rm -f marsh-svc-engine >/dev/null 2>&1; true")  # the legacy engine has no firewall: do not leave it running


def scenario_break(ctx, vm: Vm) -> None:
    """Under `engine-firewall-off` the build reports ready, and the very same witnesses see the hole."""
    vm.create()
    vm.verify()
    name = vm.name
    driver = Driver(ctx.breaks, name, breaks=["engine-firewall-off"])
    up = driver.wait(ready, 240)
    check("break/engine-firewall-off/ready", up is not None, json.dumps({"state": up and up.get("state")}))
    v4, v6, addresses = reported_gateway(driver)
    present = rules_present(name, addresses)
    check("break/engine-firewall-off/no-rules-installed", not any(value for key, value in present.items() if not key.startswith("forward-reaches")),
          json.dumps(present, sort_keys=True) + " (forward-reaches is Docker's own DOCKER-USER jump)")
    full_battery(ctx, name, "firewall-off", False, v4, v6)
    driver.stop()


def main() -> None:
    global PROJECT_DIR
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--driver-normal", required=True)
    parser.add_argument("--artifacts-normal", type=Path, required=True)
    parser.add_argument("--driver-breaks", required=True)
    parser.add_argument("--artifacts-breaks", type=Path, required=True)
    parser.add_argument("--legacy-artifacts", type=Path, required=True)
    parser.add_argument("--project", type=Path, required=True)
    parser.add_argument("--template", default=None)
    parser.add_argument("--only", default="")
    parser.add_argument("--shared", action="store_true", help="hold a shared VM slot instead of the exclusive lock (this run only touches its own sandbox)")
    args = parser.parse_args()
    template = args.template or (Path.home() / ".marsh-dev/libexec/marsh/shell-image").read_text().strip()
    only = set(filter(None, args.only.split(",")))
    run_id = secrets.token_hex(3)
    project = Path(tempfile.mkdtemp(prefix="nfx-s2b-", dir="/private/tmp")).resolve()
    PROJECT_DIR = str(project)
    ctx = type("Ctx", (), {})()
    ctx.normal = Build(args.driver_normal, args.artifacts_normal, project)
    ctx.breaks = Build(args.driver_breaks, args.artifacts_breaks, project)
    ctx.legacy = Build(args.driver_normal, args.legacy_artifacts, project)
    for label, build in (("normal", ctx.normal), ("breaks", ctx.breaks)):
        marker = run(["sh", "-c", f"strings -a '{build.artifacts / 'marsh-linux-arm64'}' | grep -c 'marsh-feature:test-breaks' || true"]).stdout.strip()
        driver_marker = run(["sh", "-c", f"strings -a '{build.driver}' | grep -c 'marsh-feature:test-breaks' || true"]).stdout.strip()
        check(f"build/{label}-provenance-marker", (marker != "0") == (label == "breaks") and (driver_marker != "0") == (label == "breaks"),
              f"guest marker lines={marker}, driver marker lines={driver_marker}")
    vm = Vm(f"nfx-s2b-{run_id}-fw", template, project)
    say("INFO", "run", f"run id {run_id}; sandbox {vm.name}; project {project}")
    with vmlock.held(f"S2b-firewall-{run_id}", shared=args.shared):
        try:
            for name, scenario in (("firewall", scenario_firewall), ("loss", scenario_loss), ("audit", scenario_audit), ("legacy", scenario_legacy), ("break", scenario_break)):
                if not only or name in only:
                    scenario(ctx, vm)
        finally:
            for box in list(s2.OWNED.values()):
                box.remove()
            say("INFO", "cleanup", f"remaining owned sandboxes: {sorted(s2.OWNED)}")
    for path in project.iterdir():
        path.unlink()
    project.rmdir()
    print("FAILED: " + ", ".join(s2.FAILURES) if s2.FAILURES else "ALL PASSED")
    sys.exit(1 if s2.FAILURES else 0)


PROJECT_DIR = ""

if __name__ == "__main__":
    main()
