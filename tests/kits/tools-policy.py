#!/usr/bin/env python3
"""Fail unless every gog command is classified exactly once.

Usage: tests/kits/tools-policy.py IMAGE

Lists every command path in the image's gog (hidden commands included) and
checks kits/marsh-tools/gog/allowed-commands plus refused-commands against it,
so a gog release that adds a command forces a policy decision.
"""
import json
import os
import re
import subprocess
import sys

root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "kits", "marsh-tools", "gog")
# Words that mean a command sends, shares or deletes; never allowed, whatever the list says.
forbidden = re.compile(r"(^|-)(send|share|unshare|delete|trash|remove|clear|publish|respond)(-|$)")
refused_services = {"admin", "auth", "config", "backup", "batch", "api", "zoom", "mcp", "login", "logout", "status", "update", "send"}


def read(name):
    lines = [l.strip() for l in open(os.path.join(root, name))]
    cmds = [l for l in lines if l and not l.startswith("#")]
    dup = sorted({c for c in cmds if cmds.count(c) > 1})
    return cmds, dup


def gog_commands(image):
    out = subprocess.run(
        ["docker", "run", "--rm", "--network", "none", "--entrypoint", "/usr/local/libexec/gog",
         image, "schema", "--json", "--include-hidden"],
        check=True, capture_output=True, text=True).stdout
    found = set()

    def walk(node, path):
        subs = node.get("subcommands") or []
        if not subs and path:
            found.add(".".join(path))
        for s in subs:
            walk(s, path + [s["name"]])

    walk(json.loads(out)["command"], [])
    return found


def main():
    allowed, dup_a = read("allowed-commands")
    refused, dup_r = read("refused-commands")
    actual = gog_commands(sys.argv[1])
    problems = []
    if dup_a or dup_r:
        problems.append(f"listed twice: {dup_a + dup_r}")
    both = sorted(set(allowed) & set(refused))
    if both:
        problems.append(f"both allowed and refused: {both}")
    new = sorted(actual - set(allowed) - set(refused))
    if new:
        problems.append("gog has commands that are in neither list (decide each, then add it to "
                        f"allowed-commands or refused-commands): {new}")
    stale = sorted((set(allowed) | set(refused)) - actual)
    if stale:
        problems.append(f"listed but gog has no such command: {stale}")
    for c in allowed:
        if not re.fullmatch(r"[a-z0-9.-]+", c):
            problems.append(f"allowed command the wrapper would reject: {c}")
        if c.split(".")[0] in refused_services or c in ("drive.move", "calendar.move") or any(forbidden.search(seg) for seg in c.split(".")):
            problems.append(f"allowed command breaks the policy: {c}")
    if problems:
        print("FAIL: tools policy", *problems, sep="\n  ", file=sys.stderr)
        sys.exit(1)
    print(f"policy: ok ({len(allowed)} allowed, {len(refused)} refused, {len(actual)} in gog)")


main()
