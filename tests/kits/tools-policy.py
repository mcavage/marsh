#!/usr/bin/env python3
"""Check the marsh-tools gog policy against the gog in the built image.

Usage: tests/kits/tools-policy.py IMAGE

0. The image's gog is the version the Dockerfile pins (a gog upgrade is a
   deliberate change to that pin, made together with the lists below).
1. Every gog command (hidden ones included) is in exactly one of
   kits/marsh-tools/gog/allowed-commands and refused-commands, so a gog release
   that adds a command forces a decision.
2. Every rule in flag-rules names a real flag, and the built wrapper enforces it.
3. gog has no flag of an allowed command that runs a program, picks a
   destination, converts a file, takes HTML, evaluates formulas, or reads image
   markup from a file, unless flag-rules covers it.
"""
import fnmatch
import json
import os
import re
import subprocess
import sys

root = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "kits", "marsh-tools", "gog")
# Words that mean a command sends, shares or deletes; never allowed, whatever the list says.
forbidden = re.compile(r"(^|-)(send|share|unshare|delete|trash|remove|clear|publish|respond)(-|$)")
refused_services = {"admin", "auth", "config", "backup", "batch", "api", "zoom", "mcp", "login", "logout", "status", "update", "send"}
never_allowed = {"drive.move", "calendar.move", "calendar.create", "calendar.update", "slides.create-from-markdown"}
image_files = {"--file", "--content-file"}


def lines(name):
    return [l.strip() for l in open(os.path.join(root, name)) if l.strip() and not l.strip().startswith("#")]


def pinned_version():
    dockerfile = os.path.join(root, "..", "Dockerfile")
    return re.search(r"^ARG GOG_VERSION=(\S+)", open(dockerfile).read(), re.M).group(1)


def gog_commands(image):
    out = subprocess.run(
        ["docker", "run", "--rm", "--network", "none", "--entrypoint", "/usr/local/libexec/gog",
         image, "schema", "--json", "--include-hidden"],
        check=True, capture_output=True, text=True).stdout
    found = {}

    def walk(node, path):
        subs = node.get("subcommands") or []
        if not subs and path:
            found[".".join(path)] = node
        for s in subs:
            walk(s, path + [s["name"]])

    schema = json.loads(out)["command"]
    walk(schema, [])
    return found, {f["name"] for f in schema["flags"]}


def flags_of(node, globals_):
    return [f for f in node.get("flags", []) if f["name"] not in globals_]


def parse_rules():
    rules = []
    for l in lines("flag-rules"):
        kind, pattern, spec = l.split(None, 2)
        rules.append((kind, pattern, spec.split()))
    return rules


def covered(rules, path, flag, kinds):
    for kind, pattern, spec in rules:
        if kind in kinds and fnmatch.fnmatchcase(path, pattern):
            for s in spec:
                if s.split("=")[0] == "--" + flag:
                    return True
    return False


def spellings(f):
    return ["--" + f["name"]] + ["--" + a for a in f.get("aliases", [])] + (["-" + f["short"]] if f.get("short") else [])


def covered_token(rules, path, token):
    return any(fnmatch.fnmatchcase(path, pattern) and token in [x.split("=")[0] for x in spec]
               for _, pattern, spec in rules)


def sample_args(path, node, globals_):
    """Arguments that parse for this command: required positionals and flags."""
    usage = node.get("usage", "")
    args = [path.split(".")]
    words = path.split(".")
    for tok in re.findall(r"\[[^\]]*\]|<[^>]+>|--[a-z0-9-]+=[A-Za-z]+", usage):
        if tok.startswith("["):
            continue
        if tok.startswith("<"):
            words.append("x")
        else:
            name, typ = tok[2:].split("=")
            flag = [f for f in node.get("flags", []) if f["name"] == name]
            numeric = flag and flag[0].get("type") in ("int", "int64", "float")
            words.append(f"--{name}={'1' if numeric else 'x'}")
    return words


def wrapper(image, words):
    r = subprocess.run(["docker", "run", "--rm", "--network", "none", image, "gog"] + words,
                       capture_output=True, text=True, stdin=subprocess.DEVNULL)
    return r.stdout + r.stderr


def main():
    image = sys.argv[1]
    allowed = lines("allowed-commands")
    refused = lines("refused-commands")
    actual, globals_ = gog_commands(image)
    rules = parse_rules()
    problems = []

    # 0. The gog under test is the one the Dockerfile pins, so the lists below
    # were reviewed against it.
    version = subprocess.run(["docker", "run", "--rm", "--entrypoint", "/usr/local/libexec/gog", image, "--version"],
                             capture_output=True, text=True).stdout.strip()
    if version.lstrip("v").split()[0] != pinned_version():
        problems.append(f"the image's gog is {version!r}, not the pinned {pinned_version()}")

    # 1. Every command is classified once.
    dup = sorted({c for c in allowed + refused if (allowed + refused).count(c) > 1})
    if dup:
        problems.append(f"listed twice (or in both lists): {dup}")
    new = sorted(set(actual) - set(allowed) - set(refused))
    if new:
        problems.append("gog has commands that are in neither list (decide each, then add it to "
                        f"allowed-commands or refused-commands): {new}")
    stale = sorted((set(allowed) | set(refused)) - set(actual))
    if stale:
        problems.append(f"listed but gog has no such command: {stale}")
    for c in allowed:
        if not re.fullmatch(r"[a-z0-9.-]+", c):
            problems.append(f"allowed command the wrapper would reject: {c}")
        if (c.split(".")[0] in refused_services or c in never_allowed
                or any(forbidden.search(seg) for seg in c.split("."))):
            problems.append(f"allowed command breaks the policy: {c}")

    # 2. Rules name real flags, and the wrapper enforces each one.
    for kind, pattern, spec in rules:
        if kind not in ("reject", "force", "scan"):
            problems.append(f"unknown rule kind: {kind}")
            continue
        targets = [p for p in actual if fnmatch.fnmatchcase(p, pattern) and (p in allowed or pattern == "*")]
        if not targets:
            problems.append(f"rule matches no allowed gog command: {kind} {pattern}")
        for s in spec:
            flag, _, value = s.partition("=")
            having = [p for p in targets if any(flag in spellings(f) for f in flags_of(actual[p], globals_))]
            if not having:
                problems.append(f"rule names a flag no matching command has: {kind} {pattern} {flag}")
                continue
            for p in having:
                f = [f for f in flags_of(actual[p], globals_) if flag in spellings(f)]
                for form in [x for x in spellings(f[0]) if x != flag]:
                    if not covered_token(rules, p, form):
                        problems.append(f"{flag} of {p} has another spelling, {form}, that flag-rules doesn't cover")
            if kind == "scan":
                continue
            p = [x for x in having if x in allowed or pattern == "*"][0]
            words = sample_args(p, actual[p], globals_)
            if kind == "reject":
                if not flag.startswith("--"):
                    given = [flag, "x"]
                else:
                    given = [flag + ("=x" if _takes_value(actual[p], flag) else "")]
                out = wrapper(image, words + given)
                expect = "is not available"
            else:
                out = wrapper(image, words + [flag + "=USER_ENTERED"])
                expect = "is fixed to"
            if expect not in out:
                problems.append(f"the wrapper doesn't enforce '{kind} {pattern} {s}' ({' '.join(words)}): {out.strip()[:120]}")

    # 3. Dangerous flags are covered by a rule.
    exec_name = re.compile(r"^(mmdc|on-change|on-new|open|browser|exec|editor|pager)$")
    exec_help = re.compile(r"trusted local shell|path to .*(cli|binary|program)|executable|mermaid|in (a|the) (default )?browser", re.I)
    for p, node in sorted(actual.items()):
        for f in flags_of(node, globals_):
            name, help_ = f["name"], f.get("help", "")
            if (exec_name.search(name) or exec_help.search(help_)) \
                    and not any(k == "reject" and pt == "*" and "--" + name in sp for k, pt, sp in rules):
                problems.append(f"flag runs or opens a program and isn't rejected for every command: {p} --{name}")
    for p in allowed:
        node = actual.get(p)
        if not node:
            continue
        for f in flags_of(node, globals_):
            name, help_ = f["name"], f.get("help", "")
            checks = []
            if re.search(r"destination folder|parent folder id", help_, re.I):
                checks.append(("a destination", ("reject", "force")))
            if name.startswith("convert"):
                checks.append(("a conversion", ("reject",)))
            if "html" in name or name == "raw-file":
                checks.append(("HTML or raw message content", ("reject",)))
            if name == "input" and "RAW" in help_:
                checks.append(("formula evaluation", ("force",)))
            if p.startswith("docs.") and "--" + name in image_files:
                checks.append(("file content that could hold image markup", ("scan",)))
            for what, kinds in checks:
                if not covered(rules, p, name, kinds):
                    problems.append(f"allowed command has a flag for {what} that flag-rules doesn't cover: {p} --{name}")

    if problems:
        print("FAIL: tools policy", *problems, sep="\n  ", file=sys.stderr)
        sys.exit(1)
    print(f"policy: ok ({len(allowed)} allowed, {len(refused)} refused, {len(actual)} in gog, {len(rules)} flag rules)")


def _takes_value(node, flag):
    f = [f for f in node.get("flags", []) if "--" + f["name"] == flag]
    return bool(f) and f[0].get("type") != "bool"


main()
