#!/usr/bin/env python3
"""Preflight the controlled egress endpoints from the Mac (H-12).

    egress_preflight.py [--json]

Checks, with the Mac's own network, that everything the egress scenarios rely
on is reachable and unchanged: EGRESS_URL answers 2xx/3xx, EGRESS_HOST resolves
(IPv4, and IPv6 when the Mac can reach it), EGRESS_ECHO_URL echoes request
headers, and both images' manifests exist at
their pinned digests (EGRESS_ABSENT_IMAGE on Docker Hub; EGRESS_DISALLOWED_IMAGE
on quay.io). Exit 0 when every required check passes; 1 otherwise. A check that
cannot apply (no IPv6 route) is reported as "unavailable", not as a pass.
"""
import argparse
import json
import pathlib
import socket
import subprocess
import sys
import urllib.request

HERE = pathlib.Path(__file__).resolve().parent


def load_config() -> dict[str, str]:
    config = {}
    for line in (HERE / "egress.env").read_text().splitlines():
        if line.strip() and not line.startswith("#"):
            key, _, value = line.partition("=")
            config[key.strip()] = value.strip()
    return config


def http(url: str, timeout: float = 15) -> tuple[int, bytes]:
    request = urllib.request.Request(url, headers={"User-Agent": "nfx-preflight"})
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.status, response.read(1 << 20)
    except urllib.error.HTTPError as error:
        return error.code, error.read(1 << 16)


def addresses(host: str, family: int) -> list[str]:
    try:
        return sorted({info[4][0] for info in socket.getaddrinfo(host, 443, family, socket.SOCK_STREAM)})
    except socket.gaierror:
        return []


def can_connect(address: str, family: int) -> bool:
    try:
        with socket.socket(family, socket.SOCK_STREAM) as sock:
            sock.settimeout(8)
            sock.connect((address, 443))
        return True
    except OSError:
        return False


def manifest_exists(reference: str) -> bool:
    return subprocess.run(["docker", "buildx", "imagetools", "inspect", reference],
                          capture_output=True, timeout=60).returncode == 0


def run() -> dict:
    config = load_config()
    results: dict[str, dict] = {}

    status, _ = http(config["EGRESS_URL"])
    results["url"] = {"ok": 200 <= status < 400, "status": status, "required": True}

    v4 = addresses(config["EGRESS_HOST"], socket.AF_INET)
    results["host_ipv4"] = {"ok": bool(v4) and can_connect(v4[0], socket.AF_INET), "addresses": v4[:2], "required": True}
    v6 = addresses(config["EGRESS_HOST"], socket.AF_INET6)
    if v6 and can_connect(v6[0], socket.AF_INET6):
        results["host_ipv6"] = {"ok": True, "addresses": v6[:2], "required": False}
    else:
        results["host_ipv6"] = {"ok": False, "unavailable": True, "addresses": v6[:2], "required": False}

    status, body = http(config["EGRESS_ECHO_URL"])
    try:
        echoed = json.loads(body)["headers"]
    except (ValueError, KeyError, TypeError):
        echoed = None
    results["echo"] = {"ok": status == 200 and isinstance(echoed, dict), "status": status, "required": True}

    results["dns"] = {"ok": bool(v4), "host": config["EGRESS_HOST"], "required": True}

    for key, name in (("absent_image", "EGRESS_ABSENT_IMAGE"), ("disallowed_image", "EGRESS_DISALLOWED_IMAGE")):
        results[key] = {"ok": manifest_exists(config[name]), "ref": config[name], "required": True}
    cached = (HERE / "images.lock").read_text()
    results["absent_image_not_cached"] = {
        "ok": config["EGRESS_ABSENT_IMAGE"].split("@")[1] not in cached, "required": True}
    return {"ok": all(r["ok"] for r in results.values() if r["required"]), "checks": results, "config": config}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    report = run()
    if args.json:
        print(json.dumps(report, indent=1, sort_keys=True))
    else:
        for name, check in report["checks"].items():
            mark = "ok " if check["ok"] else ("n/a" if check.get("unavailable") else "BAD")
            print(f"{mark} {name}: {json.dumps({k: v for k, v in check.items() if k not in ('ok', 'required')})}")
    sys.exit(0 if report["ok"] else 1)
