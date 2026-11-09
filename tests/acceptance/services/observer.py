"""H-7: the independent observer.

Every oracle about state inside a services VM comes from here: `sbx exec` into the VM, never marsh's endpoint or
marsh's JSON. Read-only by construction: a command the caller supplies must pass an allowlist of read-only
programs (`readonly_violation`), or the caller says `write=True` and the observation is recorded as a setup write.

Where things live (SECURITY-DESIGN A1): the stock dockerd that sbx starts (the "boot" engine) only runs
`marsh-svc-engine`; user containers, the pool (`/pool`) and the snapshots live inside that container's production
engine. So `vx` has two explicit modes: `engine="boot"` runs in the VM, `engine="prod"` runs inside
`marsh-svc-engine` against the production engine's socket.

Every probe fails closed on its target. The VM must be in `sbx ls`; it must have either a stable ID the harness
recorded when it created the sandbox itself, or an ownership-map entry (name AND UUID) that still matches the live
UUID; and for the production engine the engine container's ID must equal `generation.engine` of
`marsh services status --json`. These are privileged diagnostics (tests/acceptance/CONTRACT.md, "Services E2E");
they are recorded under `observations`, apart from the public-interface `commands`.
"""

from __future__ import annotations

import hashlib
import json
import os
import pathlib
import re
import shlex
import signal
import subprocess
import threading
import time
from typing import Any, Callable

from evidence import Recorder

ENGINE_CONTAINER = "marsh-svc-engine"
# The production engine's socket inside the engine container. S2 owns this path
# (SECURITY-DESIGN A1); change it here, nowhere else.
PROD_DOCKER_SOCKET = "/var/run/marsh/engine/docker.sock"
POOL = "/pool"  # a1-probe.txt: the XFS pool is /pool inside the engine container, data-root /pool/docker
SNAPSHOT_ROOT = f"{POOL}/snapshots"

CAP_ALLOWLIST = {"NET_BIND_SERVICE", "NET_RAW", "NET_ADMIN", "SYS_NICE", "SYS_PTRACE"}
LOOPBACK = {"127.0.0.1", "::1"}
TMPFS_OPTIONS = {"size", "mode", "uid"}


class ReadOnlyViolation(ValueError):
    pass


class TargetMismatch(Exception):
    """The VM or engine a probe would hit is not the one the scope owns (or that cannot be proven)."""


class Inconclusive(Exception):
    """An observation that cannot be trusted (a dead subscription, an unknown shipped digest, a missing inspect)."""


class Pending(Exception):
    """A helper or driver a later task fills in."""


class ReadOnlyScript(str):
    """A command constant of this module that is read-only by review (loops and all), exempt from the parser."""


# ---- the read-only allowlist -------------------------------------------------------------------------
SIMPLE_READERS = {"cat", "ls", "stat", "hostname", "findmnt", "df", "du", "ps", "pgrep", "grep", "egrep", "fgrep", "head",
                  "tail", "wc", "cut", "tr", "uniq", "sha256sum", "md5sum", "readlink", "realpath", "basename",
                  "dirname", "echo", "printf", "true", "false", "test", "[", "uname", "id", "whoami", "date",
                  "pwd", "getent", "ss", "lsblk", "nproc", "free", "uptime", "seq", "pg_isready"}
DOCKER_READ = {"ps", "inspect", "info", "version", "images", "events", "logs", "port", "top", "diff", "history",
               "stats"}
DOCKER_GROUP_READ = {"volume": {"ls", "inspect"}, "network": {"ls", "inspect"}, "image": {"ls", "inspect", "history"},
                     "container": {"ls", "inspect", "top", "logs", "port", "diff"}, "system": {"df", "info", "events"},
                     "compose": {"ps", "logs", "config", "ls", "images", "top", "port", "version"}}
FIND_WRITERS = {"-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprint0", "-fprintf", "-fls"}
SED_LINES = re.compile(r"^(?:\d+(?:,\d+)?p|\$p)$")


def sed_script_is_read_only(script: str) -> bool:
    """`Np`, `N,Mp`, `$p` or a single `s/a/b/FLAGS` whose flags are only g and p (no `w`, `e`, `r`)."""
    if SED_LINES.match(script):
        return True
    if len(script) < 5 or script[0] != "s" or script[1].isalnum() or script[1] in "\\ \n":
        return False
    parts = script[2:].split(script[1])
    return len(parts) == 3 and set(parts[2]) <= set("gp") and "p" in parts[2]
SQL_READ = re.compile(r"^\s*(?:select|show|with|explain|\\d)", re.IGNORECASE)
SQL_WRITE = re.compile(r"\b(?:insert|update|delete|drop|alter|create|truncate|copy|grant|revoke|vacuum|reindex|"
                       r"call|do|set|notify|listen|comment|lock|refresh|cluster|into|nextval|setval|lastval|"
                       r"set_config|dblink\w*|lo_\w+|pg_\w+)\b|\bfor\s+(?:update|share|no\s+key)", re.I)
# The only functions a read-only query may call (aggregates and plain scalar helpers); keywords that precede `(`.
SQL_FUNCTIONS = {"count", "sum", "min", "max", "avg", "coalesce", "bool_or", "bool_and", "array_agg", "string_agg",
                 "lower", "upper", "length", "md5", "to_char", "now", "current_database", "version", "round",
                 "cast", "in", "values", "exists", "any", "all", "and", "or", "not", "from", "where", "on", "as",
                 "select", "with", "over", "filter", "distinct", "between", "limit", "offset", "by", "order",
                 "group", "having", "union", "left", "right", "inner", "join", "when", "then", "else", "case",
                 "end", "is", "null", "like", "ilike", "using", "asc", "desc"}
SQL_CALL = re.compile(r"\b([a-z_][a-z0-9_]*)\s*\(", re.I)
DATE_SAFE = {"-u", "--utc", "-R", "--rfc-email"}
HOSTNAME_FLAGS = {"-s", "-f", "-d", "-i", "-I", "-A", "-a", "-y", "--short", "--fqdn", "--long", "--domain",
                  "--ip-address", "--all-ip-addresses", "--all-fqdns", "--alias", "--yp", "--nis"}
EXEC_FLAGS = {"-i", "-t", "-it", "-ti", "--interactive", "--tty"}
SORT_SHORT = re.compile(r"-[nrufVhbdgMsz]+")
SORT_LONG = {"--numeric-sort", "--reverse", "--unique", "--ignore-case", "--version-sort", "--human-numeric-sort",
             "--ignore-leading-blanks", "--dictionary-order", "--general-numeric-sort", "--month-sort", "--stable"}
SED_SHORT = re.compile(r"-[nEsrz]+")
SED_LONG = {"--quiet", "--silent", "--regexp-extended", "--separate", "--null-data"}
PSQL_FLAGS = set("AtqXxHz")      # no-argument short options that cannot write or run anything
PSQL_VALUES = set("Udhpc")       # short options whose value is the rest of the word or the next word
PSQL_LONG = {"--no-align", "--tuples-only", "--quiet", "--no-psqlrc", "--csv", "--html", "--expanded"}
PSQL_LONG_VALUE = ("--command", "--dbname", "--username", "--host", "--port")
ENDPOINT_ASSIGNMENT = re.compile(r"^(?:DOCKER_|PATH=|LD_|BUILDKIT_HOST|SSH_AUTH_SOCK|HOME=)", re.I)
REDIS_READ = {"get", "mget", "exists", "keys", "dbsize", "ping", "info", "ttl", "type", "scan", "llen", "scard",
              "hget", "hgetall", "strlen"}
ASSIGNMENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")
OPERATORS = {";", "&&", "||", "|", "&"}


def _segments(command: str) -> list[list[str]]:
    lexer = shlex.shlex(command, posix=True, punctuation_chars=";|&()<>")
    lexer.whitespace_split = True
    segments: list[list[str]] = [[]]
    for token in lexer:
        if token and set(token) <= set(";|&") and token in OPERATORS:
            segments.append([])
        else:
            segments[-1].append(token)
    return [s for s in segments if s]


def _redirect_ok(tokens: list[str]) -> str | None:
    """Redirections are allowed only to /dev/null or between descriptors (`2>&1`)."""
    for index, token in enumerate(tokens):
        if token in (">", ">>", ">|", "<>", "&>"):
            target = tokens[index + 1] if index + 1 < len(tokens) else ""
            if target not in ("/dev/null", "&1", "&2"):
                return f"output redirection to {target or '?'}"
        elif token == "<" and (index + 1 >= len(tokens) or tokens[index + 1] in (">", "<")):
            return "here-document or malformed input redirection"
        elif token in ("<<", "<<<"):
            return "here-document"
        elif token.endswith(">") and token[:-1].isdigit():
            target = tokens[index + 1] if index + 1 < len(tokens) else ""
            if target not in ("/dev/null", "&1", "&2"):
                return f"output redirection to {target or '?'}"
    return None


def sql_is_read_only(query: str) -> bool:
    """A single read query: starts with select/show/with/explain (or one psql `\\d` command), has no second
    statement or backslash command, no write keyword or side-effecting function (nextval, setval, `into`, pg_*,
    `for update`, ...), and calls only allowlisted functions."""
    body = query.strip().rstrip(";")
    if not SQL_READ.match(query) or ";" in body or SQL_WRITE.search(body):
        return False
    if body.lstrip().startswith("\\"):
        return "\\" not in body.lstrip()[1:] and "!" not in body
    if "\\" in body:
        return False
    return all(name.lower() in SQL_FUNCTIONS for name in SQL_CALL.findall(body))


def _check_words(words: list[str]) -> str | None:
    words = [w for w in words if w not in ("(", ")")]
    if any(ASSIGNMENT.match(w) and ENDPOINT_ASSIGNMENT.match(w) for w in words[:1]):
        return f"{words[0].split('=')[0]} may not be overridden: the oracle must not leave its own engine"
    if words and ASSIGNMENT.match(words[0]):
        return "environment assignments are not read-only context (use write=True)"
    if not words:
        return None
    program, args = words[0].rsplit("/", 1)[-1], words[1:]
    if program == "date":
        bad = [a for a in args if not (a.startswith(("+", "-I", "--iso-8601")) or a in DATE_SAFE)]
        return f"date {bad[0]} (sets the clock or is not a read form)" if bad else None
    if program == "hostname":  # only the read flags; NAME, -F/--file and -b (attached or not) set the hostname
        bad = [a for a in args if a not in HOSTNAME_FLAGS]
        return f"hostname {bad[0]} is not a read-only form" if bad else None
    if program == "ss" and any(a == "--kill" or re.fullmatch(r"-[a-zA-Z]*K[a-zA-Z]*", a) for a in args):
        return "ss --kill"
    if program == "uniq" and len([a for a in args if not a.startswith("-")]) > 1:
        return "uniq INPUT OUTPUT writes a file"
    if program in SIMPLE_READERS:
        return None
    if program == "find":
        bad = [a for a in args if a in FIND_WRITERS]
        return f"find {bad[0]}" if bad else None
    if program == "sed":
        scripts = [a for a in args if not a.startswith("-")]
        options = [a for a in args if a.startswith("-")]
        quiet = any(re.fullmatch(r"-[nEsrz]*n[nEsrz]*", a) or a in ("--quiet", "--silent") for a in options)
        if not quiet or any(not (SED_SHORT.fullmatch(a) or a in SED_LONG) for a in options) or not scripts or \
                not sed_script_is_read_only(scripts[0]):
            return "sed other than `sed -n 'Np'` or `sed -n 's/a/b/p'`"
        return None
    if program == "sort":  # only known read flags: -o/--output (and abbreviations, attached values) and
        # --compress-program (it runs a program) are not among them
        bad = [a for a in args if a.startswith("-") and a != "-" and not (SORT_SHORT.fullmatch(a) or a in SORT_LONG
               or re.fullmatch(r"-[kt].+", a) or re.fullmatch(r"--(?:key|field-separator)=.+", a))]
        if any(a in ("-k", "-t") for a in args):
            bad += [a for i, a in enumerate(args[:-1]) if a in ("-k", "-t") and args[i + 1].startswith("-")]
        return f"sort {bad[0]} is not a read-only form" if bad else None
    if program == "xargs":
        rest = [a for a in args if a not in ("-0", "-r", "--no-run-if-empty")]
        while rest and rest[0] in ("-n", "-I", "-P", "-L"):
            rest = rest[2:]
        return _check_words(rest) if rest else "xargs without a command"
    if program in ("sh", "bash"):
        if "-c" in args and args.index("-c") + 1 < len(args):
            return readonly_violation(args[args.index("-c") + 1])
        return f"{program} without -c"
    if program == "docker":
        return _check_docker(args)
    if program == "psql":
        return _check_psql(args)
    if program == "redis-cli":
        words = [a for a in args if not a.startswith("-")]
        flag_values = {args[i + 1] for i, a in enumerate(args[:-1]) if a in ("-h", "-p", "-n", "-a", "--user", "-u")}
        words = [w for w in words if w not in flag_values]
        return None if words and words[0].lower() in REDIS_READ else "redis-cli other than a read command"
    return f"{program} is not on the read-only allowlist"


def _check_psql(args: list[str]) -> str | None:
    """psql with a single read-only -c query and only options that cannot write a file, run a script or a program:
    short options are parsed getopt-style (a value option takes the rest of its word or the next word), anything not
    on the allowlist (-o, -L, -f, -s, -1, -v, ... attached or not, and every unknown long option) is refused."""
    queries: list[str] = []
    index = 0
    while index < len(args):
        arg = args[index]
        index += 1
        if arg.startswith("--"):
            name, equals, value = arg.partition("=")
            if arg in PSQL_LONG:
                continue
            if name in PSQL_LONG_VALUE:
                if not equals:
                    if index >= len(args):
                        return "psql option without a value"
                    value, index = args[index], index + 1
                if name == "--command":
                    queries.append(value)
                continue
            return f"psql {arg} is not a read-only form"
        if arg.startswith("-") and arg != "-":
            position = 1
            while position < len(arg):
                letter = arg[position]
                position += 1
                if letter in PSQL_FLAGS:
                    continue
                if letter in PSQL_VALUES:
                    value = arg[position:]
                    if not value:
                        if index >= len(args):
                            return "psql option without a value"
                        value, index = args[index], index + 1
                    if letter == "c":
                        queries.append(value)
                    break
                return f"psql -{letter} is not a read-only form"
            continue
        return f"psql positional argument {arg!r} (a database goes with -d)"
    if len(queries) != 1 or not sql_is_read_only(queries[0]):
        return "psql other than a single read-only -c query"
    return None


def _check_docker(args: list[str]) -> str | None:
    index = 0
    while index < len(args) and args[index].startswith("-"):
        if args[index] not in ("-D", "--debug"):  # every other global option (-H, -c, --context, --config, --tls*,
            # attached forms like -Hunix://... or -cjob included) can move the oracle off its own engine
            return f"docker {args[index]} would move the oracle off its own engine (global options are not allowed)"
        index += 1
    if index >= len(args):
        return "docker without a subcommand"
    sub, rest = args[index], args[index + 1:]
    if sub in DOCKER_READ:
        return "docker stats without --no-stream" if sub == "stats" and "--no-stream" not in rest else None
    if sub in DOCKER_GROUP_READ:
        return None if rest and rest[0] in DOCKER_GROUP_READ[sub] else f"docker {sub} {rest[0] if rest else ''}".strip()
    if sub == "exec":
        words = list(rest)
        while words and words[0].startswith("-"):
            option = words[0]
            if option in EXEC_FLAGS:
                words = words[1:]
            elif option in ("-u", "--user", "-w", "--workdir"):
                words = words[2:]
            elif option.startswith(("--user=", "--workdir=")) or re.fullmatch(r"-[uw].+", option):
                words = words[1:]
            else:  # -e/--env/--env-file (attached or not), -d, --privileged, ... could redirect or detach
                return f"docker exec {option} is not a read-only form"
        return _check_words(words[1:]) if len(words) > 1 else "docker exec without a command"
    return f"docker {sub} is not read-only"


def readonly_violation(command: str | ReadOnlyScript) -> str | None:
    """Why COMMAND is not provably read-only (None: it is). An allowlist: every program of every pipeline segment
    must be a known reader (docker only through read subcommands, `docker exec` and `sh -c` recursively, psql and
    redis-cli only with read queries); output redirection only to /dev/null; no here-documents or command
    substitution that is not itself read-only. Anything else needs write=True."""
    if isinstance(command, ReadOnlyScript):
        return None
    if "\n" in command or "`" in command:
        return "newlines and backticks are not parsed"
    substitutions = re.findall(r"\$\(([^()]*)\)", command)
    flattened = re.sub(r"\$\(([^()]*)\)", "SUBSTITUTED", command)
    if "$(" in flattened or "${" in flattened and "}" not in flattened:
        return "nested command substitution"
    for inner in substitutions:
        reason = readonly_violation(inner)
        if reason:
            return f"command substitution: {reason}"
    try:
        segments = _segments(flattened)
    except ValueError as error:
        return f"unparseable ({error})"
    for tokens in segments:
        reason = _redirect_ok(tokens)
        if reason:
            return reason
        words, skip = [], False
        for position, token in enumerate(tokens):
            if skip:
                skip = False
            elif token in (">", ">>", "<", ">|", "<>", "&>") or (token.endswith(">") and token[:-1].isdigit()):
                skip = True
            elif token == "&" or token in ("(", ")"):
                continue
            else:
                words.append(token)
        reason = _check_words(words)
        if reason:
            return reason
    return None


def is_mutating(command: str) -> bool:
    """True when COMMAND is not provably read-only."""
    return readonly_violation(command) is not None


def vx_argv(sbx: str, svc: str, command: str, engine: str) -> list[str]:
    """The `sbx exec` argv for one observation (H-7: `sbx exec $SVC sudo sh -c 'CMD'`)."""
    if engine == "boot":
        return [sbx, "exec", svc, "sudo", "sh", "-c", command]
    if engine == "prod":
        inner = f"export DOCKER_HOST=unix://{PROD_DOCKER_SOCKET}; {command}"
        return [sbx, "exec", svc, "sudo", "docker", "exec", ENGINE_CONTAINER, "sh", "-c", inner]
    raise ValueError(f"engine must be 'boot' or 'prod', not {engine!r}")


# ---- audits ---------------------------------------------------------------------------------------------
def audit_container(container: dict[str, Any], admitted_roots: list[str]) -> list[str]:
    """Violations in one `docker inspect` container document (H-7 vaudit)."""
    name = (container.get("Name") or container.get("Id", "?")[:12]).lstrip("/")
    host = container.get("HostConfig") or {}
    found: list[str] = []

    def bad(what: str) -> None:
        found.append(f"container {name}: {what}")

    if host.get("Privileged"):
        bad("Privileged")
    for cap in host.get("CapAdd") or []:
        if cap.removeprefix("CAP_").upper() not in CAP_ALLOWLIST:
            bad(f"CapAdd {cap}")
    for key in ("Devices", "DeviceRequests", "DeviceCgroupRules"):
        if host.get(key):
            bad(key)
    for key in ("NetworkMode", "PidMode", "IpcMode", "UTSMode", "UsernsMode", "CgroupnsMode"):
        if host.get(key) == "host":
            bad(f"{key}: host")
    for option in host.get("SecurityOpt") or []:
        lowered = option.lower()
        if "unconfined" in lowered or lowered.startswith("seccomp=") or lowered.startswith("seccomp:"):
            bad(f"SecurityOpt {option}")
    for key in ("MaskedPaths", "ReadonlyPaths"):
        if not host.get(key):
            bad(f"{key} emptied")
    sources = [m.get("Source", "") for m in container.get("Mounts") or [] if m.get("Type") == "bind"]
    sources += [b.split(":", 1)[0] for b in host.get("Binds") or [] if b.startswith("/")]
    for source in sources:
        if not any(source == root or source.startswith(root.rstrip("/") + "/") for root in admitted_roots):
            bad(f"bind source {source} outside the admitted roots")
    for port, bindings in (host.get("PortBindings") or {}).items():
        for binding in bindings or []:
            if (binding.get("HostIp") or "") not in LOOPBACK:
                bad(f"published port {port} bound to {binding.get('HostIp') or '(all interfaces)'}")
    log_type = (host.get("LogConfig") or {}).get("Type") or "json-file"
    if log_type != "json-file":
        bad(f"log driver {log_type}")
    return found


def audit_volume(volume: dict[str, Any]) -> list[str]:
    name = volume.get("Name", "?")
    found = []
    if volume.get("Driver", "local") != "local":
        found.append(f"volume {name}: driver {volume.get('Driver')}")
    options = volume.get("Options") or {}
    if options:
        extra = set(options) - {"type", "device", "o"}
        mount_options = {part.split("=", 1)[0] for part in (options.get("o") or "").split(",") if part}
        if (options.get("type") != "tmpfs" or options.get("device") not in (None, "tmpfs") or extra
                or not mount_options <= TMPFS_OPTIONS):
            found.append(f"volume {name}: driver options {options}")
    return found


def event_id(event: dict[str, Any]) -> str:
    """A container event's ID: `Actor.ID` (Docker 29), or the older top-level `id`."""
    return str((event.get("Actor") or {}).get("ID") or event.get("id") or "")


def event_action(event: dict[str, Any]) -> str:
    return str(event.get("Action") or event.get("status") or "").split(":")[0]


def freeze_window(lines: list[str]) -> dict[str, Any] | None:
    """Freeze window from `docker events --format '{{json .}}'` lines: the last `unpause` timeNano minus the first
    `pause` timeNano (None if nothing paused)."""
    pauses, unpauses = [], []
    for line in lines:
        if not line.strip():
            continue
        event = json.loads(line)
        when = event.get("timeNano")
        if event_action(event) == "pause" and isinstance(when, int):
            pauses.append(when)
        elif event_action(event) == "unpause" and isinstance(when, int):
            unpauses.append(when)
    if not pauses:
        return None
    last = max(unpauses) if unpauses else None
    return {"first_pause_ns": min(pauses), "last_unpause_ns": last, "pauses": len(pauses), "unpauses": len(unpauses),
            "window_ms": None if last is None else (last - min(pauses)) / 1e6}


def freeze_pairs(lines: list[str], ids: set[str] | None) -> dict[str, Any] | None:
    """Pause/unpause events of the given container IDs (all when IDS is None), paired IN ORDER per container: a
    pause opens a freeze, an unpause closes the open one. An unpause with nothing open, or a pause still open at the
    end, is unpaired (so pause, unpause, unpause, pause reports two). The window is last unpause minus first pause."""
    wanted = None if ids is None else {i[:12] for i in ids}
    events = []
    for line in lines:
        if line.strip():
            event = json.loads(line)
            if event_action(event) in ("pause", "unpause") and (wanted is None or event_id(event)[:12] in wanted) \
                    and isinstance(event.get("timeNano"), int):
                events.append(event)
    if not events:
        return None
    events.sort(key=lambda e: e["timeNano"])
    open_at: dict[str, int] = {}
    unpaired: list[str] = []
    for event in events:
        key = event_id(event)[:12]
        if event_action(event) == "pause":
            if key in open_at:
                unpaired.append(key)  # a second pause while frozen
            open_at[key] = event["timeNano"]
        elif key in open_at:
            del open_at[key]
        else:
            unpaired.append(key)
    unpaired += list(open_at)
    window = freeze_window([json.dumps(e) for e in events]) or {
        "first_pause_ns": None, "last_unpause_ns": None, "pauses": 0,
        "unpauses": sum(event_action(e) == "unpause" for e in events), "window_ms": None}
    window["unpaired"] = sorted(set(unpaired))
    return window


def parse_snapshot_listing(output: str) -> dict[str, str]:
    """`NAME HASH` lines from vsnap's listing into {name: hash}."""
    listing = {}
    for line in output.splitlines():
        if line.strip():
            name, _, digest = line.partition(" ")
            listing[name] = digest.strip()
    return listing


# Type-aware: `find -P` never follows symlinks; only regular files are read (FIFOs, sockets and device nodes
# are listed by type, never opened); a sparse file is sized with its apparent and allocated sizes, not read.
SNAPSHOT_LISTING = ReadOnlyScript(
    f"cd {SNAPSHOT_ROOT} 2>/dev/null || exit 0; for d in *; do [ -d \"$d\" ] || continue; "
    "h=$( ( find -P \"$d\" -type f -printf '%s %b %p\\n' | sort -k3 | while read -r sz bl p; do "
    "if [ $((bl * 512)) -ge \"$sz\" ]; then echo \"f $(sha256sum -- \"$p\" | cut -d' ' -f1) $sz $p\"; "
    "else echo \"sparse $sz $bl $p\"; fi; done; "
    "find -P \"$d\" -type l -printf 'l %p -> %l\\n' | sort; "
    "find -P \"$d\" \\( -type p -o -type b -o -type c -o -type s \\) -printf '%y %p\\n' | sort ) | sha256sum | cut -d' ' -f1 ); "
    "echo \"$d $h\"; done")


def boot_audit(containers: list[dict[str, Any]], expected_image: str) -> list[str]:
    """The boot engine runs exactly one container, `marsh-svc-engine`, from the shipped digest (H-7 vboot_audit).
    EXPECTED_IMAGE is the digest the marsh artifacts ship; without it the audit proves nothing and is refused."""
    if not expected_image or "@sha256:" not in expected_image and not expected_image.startswith("sha256:"):
        raise Inconclusive(f"the shipped engine image digest is unknown or not a digest: {expected_image!r}")
    found = []
    names = sorted((c.get("Names") or c.get("Name") or "?").lstrip("/") for c in containers)
    if names != [ENGINE_CONTAINER]:
        found.append(f"the boot engine runs {names}, expected exactly [{ENGINE_CONTAINER!r}]")
    for container in containers:
        image = container.get("Image") or ""
        if expected_image not in (image, container.get("ImageID")) and not (
                expected_image.startswith("sha256:") and expected_image == container.get("ImageID")):
            found.append(f"engine container image {image!r} is not the shipped {expected_image!r}")
    return found


# ---- live events (H-13) ---------------------------------------------------------------------------------
class EventWatcher:
    """`vevents`: a `docker events` subscription on one engine, live before the operation under test.

    Liveness is proven end to end, not assumed: after the script prints `R` (the subscription process started),
    `start()` has the harness create and remove a marker network `nfx-ready-<nonce>` (a documented setup write) and
    returns only after the marker's create event came back through the stream. The script also subscribes with
    `--since` the VM clock it read first, so nothing between the clock read and the subscription is lost. Every
    object type is recorded. For each container, volume and network created, the create-time `inspect` is taken inside
    the VM; an inspect that returns nothing (the object was already gone) is recorded in `uninspected`, and `audit()`
    treats it as inconclusive, never clean. A subscription that dies before `stop()` makes every query raise
    `Inconclusive`, even if the reader has not yet seen the end of the stream. With SCOPE, `scoped_*` views and
    `audit(scoped=True)` consider only objects whose labels carry `dev.marsh.scope=SCOPE`.
    """

    INSPECTED = ("container", "volume", "network")
    SCRIPT = ReadOnlyScript(
        "T=$(date +%s); echo \"R $T\"; docker events --since \"$T\" --format '{{json .}}' | while IFS= read -r line; do "
        "echo \"E $line\"; case \"$line\" in *'\"Action\":\"create\"'*) "
        "kind=$(printf '%s' \"$line\" | sed -n 's/.*\"Type\":\"\\([a-z]*\\)\".*/\\1/p'); "
        "id=$(printf '%s' \"$line\" | sed -n 's/.*\"Actor\":{\"ID\":\"\\([^\"]*\\)\".*/\\1/p'); "
        "case \"$kind\" in container|volume|network) "
        "echo \"I $kind $id $(docker $kind inspect \"$id\" 2>/dev/null | tr -d '\\n')\";; esac;; esac; done")

    def __init__(self, observer: "Observer", engine: str = "prod", scope: str | None = None) -> None:
        self.observer = observer
        self.engine = engine
        self.scope = scope
        self.events: list[dict[str, Any]] = []
        self.created: dict[str, dict[str, Any]] = {}  # containers by ID
        self.inspected: dict[str, dict[str, dict[str, Any]]] = {"volume": {}, "network": {}}
        self.uninspected: list[str] = []
        self.started = threading.Event()
        self.error: str | None = None
        self.process: subprocess.Popen[bytes] | None = None
        self.thread: threading.Thread | None = None
        self.lock = threading.Lock()
        self.stopped = False
        self.stopping = False
        self.marker = f"nfx-ready-{os.urandom(4).hex()}"

    def start(self, popen: Callable[[list[str]], "subprocess.Popen[bytes]"], timeout: float = 30) -> "EventWatcher":
        target = self.observer.target()
        self.observer.check_target(self.engine, target)
        self.process = popen(vx_argv(self.observer.sbx, target, self.SCRIPT, self.engine))
        self.thread = threading.Thread(target=self._read, daemon=True)
        self.thread.start()
        deadline = time.monotonic() + timeout

        def failed(why: str) -> Inconclusive:
            self.stop()
            return Inconclusive(why)

        while not self.started.wait(0.1):
            if self.process.poll() is not None or time.monotonic() > deadline:
                raise failed("the event subscription process did not start")
        self.observer.vx(f"docker network create {self.marker}", engine=self.engine, write=True, check=True)
        try:
            while not any(e.get("Type") == "network" and event_action(e) == "create" and
                          ((e.get("Actor") or {}).get("Attributes") or {}).get("name") == self.marker
                          for e in list(self.events)):
                if self.error or self.process.poll() is not None or time.monotonic() > deadline:
                    raise failed("the marker event never came back: the subscription is not live")
                time.sleep(0.05)
        finally:
            self.observer.vx(f"docker network rm {self.marker}", engine=self.engine, write=True)
        return self

    def _read(self) -> None:
        assert self.process is not None and self.process.stdout is not None
        for raw in self.process.stdout:
            stamp = time.monotonic_ns()
            line = raw.decode("utf-8", "replace").rstrip("\n")
            kind, _, payload = line.partition(" ")
            if kind == "R":
                self.started.set()
                continue
            object_type = identifier = ""
            if kind == "I":
                object_type, _, rest = payload.partition(" ")
                identifier, _, payload = rest.partition(" ")
            try:
                document = json.loads(payload)
            except ValueError:
                if kind == "I":
                    with self.lock:
                        self.uninspected.append(identifier)
                continue
            with self.lock:
                if kind == "E" and isinstance(document, dict):
                    self.events.append({**document, "host_ns": stamp})
                elif kind == "I" and isinstance(document, list) and document:
                    if object_type == "container":
                        self.created[document[0]["Id"]] = document[0]
                    else:
                        self.inspected[object_type][identifier] = document[0]
                elif kind == "I":
                    self.uninspected.append(identifier)  # gone before the inspect ran
        if not self.stopping and self.error is None:
            self.error = "the event subscription ended before it was stopped"

    def stop(self) -> None:
        """End the subscription. Only a SIGKILL this method delivered, that ended the subscriber with exactly that
        signal, is a clean stop. A subscriber that was already gone, or that exited on its own in the instant between
        any check and the kill (any other exit status), is recorded as a lost witness: it is never blessed."""
        self.stopping = True  # the reader treats the end of the stream as expected from here on; stop() decides below
        delivered, status = False, None
        if self.process is not None:
            if self.process.poll() is None:
                try:
                    os.killpg(self.process.pid, signal.SIGKILL)
                    delivered = True
                except ProcessLookupError:
                    pass
            status = self.process.wait(timeout=10)
            if not (delivered and status == -signal.SIGKILL) and self.error is None:
                self.error = f"the event subscription ended on its own before it was stopped (exit {status})"
        self.stopped = True
        if self.thread is not None:
            self.thread.join(timeout=10)

    def healthy(self) -> bool:
        return self.error is None and self.process is not None and (self.stopped or self.process.poll() is None)

    def require_healthy(self) -> None:
        if not self.healthy():
            raise Inconclusive(self.error or "the event subscription is not running")

    def in_scope(self, event: dict[str, Any]) -> bool:
        if self.scope is None:
            return True
        return ((event.get("Actor") or {}).get("Attributes") or {}).get("dev.marsh.scope") == self.scope

    def actions(self, action: str, kind: str | None = None, *, scoped: bool = False) -> list[dict[str, Any]]:
        self.require_healthy()
        with self.lock:
            return [e for e in self.events if event_action(e) == action and (kind is None or e.get("Type") == kind)
                    and (not scoped or self.in_scope(e))
                    and ((e.get("Actor") or {}).get("Attributes") or {}).get("name") != self.marker]

    def objects(self, kind: str, *, scoped: bool = False) -> set[str]:
        """Names of every object of KIND (container, volume, network, image) a create event announced."""
        return {((e.get("Actor") or {}).get("Attributes") or {}).get("name") or event_id(e)
                for e in self.actions("create", kind, scoped=scoped)}

    def created_names(self) -> set[str]:
        with self.lock:
            return {(c.get("Name") or "").lstrip("/") for c in self.created.values()}

    def audit(self, admitted_roots: list[str], *, scoped: bool = False) -> list[str]:
        """vaudit's rules over the create-time inspects of containers and volumes. A created object with no inspect,
        and a dead subscription, are reported as inconclusive violations: a missing record is never a clean one."""
        self.require_healthy()
        with self.lock:
            found: list[str] = []
            for container in self.created.values():
                labels = (container.get("Config") or {}).get("Labels") or {}
                if scoped and self.scope is not None and labels.get("dev.marsh.scope") != self.scope:
                    continue
                found += audit_container(container, admitted_roots)
            for volume in self.inspected["volume"].values():
                if volume.get("Name", "").startswith("nfx-ready-"):
                    continue
                if scoped and self.scope is not None and (volume.get("Labels") or {}).get("dev.marsh.scope") != self.scope:
                    continue
                found += audit_volume(volume)
            for identifier in self.uninspected:
                announced = next((e for e in self.events if event_id(e) == identifier), None)
                if announced is not None and ((announced.get("Actor") or {}).get("Attributes") or {}).get("name") == self.marker:
                    continue  # the readiness marker is ours
                if scoped and self.scope is not None and announced is not None and not self.in_scope(announced):
                    continue  # its create event says it belongs to another scope
                found.append(f"inconclusive: {identifier[:12]} was created but could not be inspected")
            return found


def canary_command(case: str, mount: str = "/h") -> str:
    """The command a forbidden case runs inside its container: it writes `<mount>/.nfx-canary/<case>`, which only
    lands on the Mac if the forbidden bind was really admitted and started."""
    safe = "".join(c if c.isalnum() or c in "-_." else "_" for c in case)
    return f"mkdir -p {mount}/.nfx-canary && echo escaped > {mount}/.nfx-canary/{safe}"


def canary_present(project: pathlib.Path, case: str) -> bool:
    safe = "".join(c if c.isalnum() or c in "-_." else "_" for c in case)
    return (pathlib.Path(project) / ".nfx-canary" / safe).exists()


# ---- the observer ---------------------------------------------------------------------------------------
class Observer:
    """`vx` and its helpers for one run. RUN is `(argv, timeout) -> (exit, stdout, stderr, started_ns, elapsed_ns)`.

    Required callbacks make every probe fail closed: `owned()` the names this scope may observe, `listed()` the names
    in `sbx ls`, `uuid_of(name)` the live stable ID, `owned_uuid(name)` the ID the ownership map records (None when
    the map has no ID for it), and `engine_generation(svc)` the engine-container ID that `marsh services status
    --json` reports for the VM (None when it cannot say, which refuses every production-engine probe).
    """

    def __init__(self, sbx: str, run: Callable[..., tuple[int, bytes, bytes, int, int]], recorder: Recorder,
                 owned: Callable[[], set[str]], listed: Callable[[], set[str]], *,
                 uuid_of: Callable[[str], str | None], owned_uuid: Callable[[str], str | None],
                 engine_generation: Callable[[str], str | None]) -> None:
        self.sbx = sbx
        self.run = run
        self.recorder = recorder
        self.owned = owned
        self.listed = listed
        self.uuid_of = uuid_of
        self.owned_uuid = owned_uuid
        self.engine_generation = engine_generation
        self.svc: str | None = None
        self.direct_uuid: str | None = None
        self.shipped_image: str | None = None

    def target(self, svc: str | None = None) -> str:
        target = svc or self.svc
        if target is None:
            raise ValueError("vx: no services VM bound (call bind() first)")
        return target

    def bind(self, svc: str, *, direct_uuid: str | None = None) -> None:
        """Bind SVC. A sandbox the harness made itself passes the stable ID it recorded at creation (DIRECT_UUID);
        any other VM must have an ownership-map entry with a UUID. Either must equal the live UUID now."""
        if svc not in self.listed():
            raise TargetMismatch(f"{svc} is not in `sbx ls --json`")
        self.svc, self.direct_uuid = svc, direct_uuid
        try:
            self.check_target("boot", svc)
        except TargetMismatch:
            self.svc = self.direct_uuid = None
            raise

    def expected_uuid(self, name: str) -> str:
        expected = self.direct_uuid if name == self.svc and self.direct_uuid else self.owned_uuid(name)
        if not expected:
            raise TargetMismatch(f"{name} has neither a recorded creation ID nor an ownership-map UUID: "
                                 "ownership is unproven, refusing to observe it")
        return expected

    def check_target(self, engine: str, svc: str | None = None) -> None:
        """The VM is listed, still owned by this scope, and its live UUID equals the recorded one; for `prod`, the
        engine container is the current generation reported by `marsh services status --json`."""
        target = self.target(svc)
        if target not in self.listed():
            raise TargetMismatch(f"{target} is no longer in `sbx ls --json`")
        if self.direct_uuid is None and target not in self.owned():
            raise TargetMismatch(f"{target} is no longer in the scope's ownership map")
        live, expected = self.uuid_of(target), self.expected_uuid(target)
        if live != expected:
            raise TargetMismatch(f"{target} has UUID {live}, recorded {expected}: it was replaced")
        if engine == "prod":
            reported = self.engine_generation(target)
            if not reported:
                raise TargetMismatch(f"no generation.engine is known for {target}: the production engine "
                                     "cannot be observed")
            running = self._probe("docker inspect --format '{{.Id}}' " + ENGINE_CONTAINER, "boot", target, False,
                                  60).stdout.decode().strip()
            if not running or not (running.startswith(reported) or reported.startswith(running)):
                raise TargetMismatch(f"engine container {running!r} is not generation.engine {reported!r}")

    def _probe(self, command: str, engine: str, target: str, write: bool, timeout: float
               ) -> subprocess.CompletedProcess[bytes]:
        argv = vx_argv(self.sbx, target, command, engine)
        exit_code, out, err, started, elapsed = self.run(argv, timeout)
        self.recorder.observations.append({
            **self.recorder.command(argv, None, exit_code, started, elapsed, out, err),
            "kind": "vx", "engine": engine, "svc": target, "write": write})
        return subprocess.CompletedProcess(argv, exit_code, out, err)

    def vx(self, command: str, *, engine: str = "boot", write: bool = False, timeout: float = 60,
           svc: str | None = None, check: bool = False) -> subprocess.CompletedProcess[bytes]:
        target = self.target(svc)
        if not write:
            reason = readonly_violation(command)
            if reason:
                raise ReadOnlyViolation(f"vx is read-only ({reason}); pass write=True for a setup write: "
                                        f"{str(command)[:120]}")
        self.check_target(engine, target)
        done = self._probe(command, engine, target, write, timeout)
        if check and done.returncode != 0:
            raise RuntimeError(f"vx {str(command)[:80]!r} failed ({done.returncode}): "
                               f"{done.stderr.decode(errors='replace')[-300:]}")
        return done

    def vaudit(self, admitted_roots: list[str]) -> list[str]:
        """Every container and volume in the production engine; the list of violations."""
        found: list[str] = []
        ids = self.vx("docker ps -aq", engine="prod", check=True).stdout.decode().split()
        if ids:
            inspected = self.vx("docker inspect " + " ".join(shlex.quote(i) for i in ids), engine="prod", check=True)
            for container in json.loads(inspected.stdout):
                found += audit_container(container, admitted_roots)
        names = self.vx("docker volume ls -q", engine="prod", check=True).stdout.decode().split()
        if names:
            inspected = self.vx("docker volume inspect " + " ".join(shlex.quote(n) for n in names),
                                engine="prod", check=True)
            for volume in json.loads(inspected.stdout):
                found += audit_volume(volume)
        return found

    def vboot_audit(self) -> list[str]:
        """The boot engine holds exactly `marsh-svc-engine`, from the shipped digest (`self.shipped_image`, from the
        marsh artifacts; an unknown digest makes the audit inconclusive)."""
        if not self.shipped_image:
            raise Inconclusive("vboot_audit needs the shipped engine image digest (Observer.shipped_image)")
        listing = self.vx("docker ps -a --no-trunc --format '{{json .}}'", engine="boot", check=True)
        containers = [json.loads(line) for line in listing.stdout.decode().splitlines() if line.strip()]
        return boot_audit(containers, self.shipped_image)

    def vfreeze(self, t0: float, t1: float, ids: set[str] | None = None) -> dict[str, Any] | None:
        """Freeze window between unix times T0 and T1, from the production engine's own events, for the exact
        container IDs of one scope, paired in order. `unpaired` lists IDs with an unmatched pause or unpause."""
        done = self.vx(f"docker events --since {t0:.3f} --until {t1:.3f} --filter type=container "
                       "--format '{{json .}}'", engine="prod", timeout=max(60, t1 - time.time() + 60), check=True)
        return freeze_pairs(done.stdout.decode().splitlines(), ids)

    def vsnap(self) -> dict[str, Any]:
        """Names and a type-aware content hash of every snapshot under the pool."""
        done = self.vx(SNAPSHOT_LISTING, engine="prod", timeout=600, check=True)
        listing = parse_snapshot_listing(done.stdout.decode())
        return {"snapshots": listing,
                "digest": hashlib.sha256(json.dumps(listing, sort_keys=True).encode()).hexdigest()}

    def vevents(self, popen: Callable[[list[str]], "subprocess.Popen[bytes]"], engine: str = "prod",
                scope: str | None = None) -> EventWatcher:
        """Start the live event observer (live when this returns); call `.stop()` after the operation."""
        return EventWatcher(self, engine, scope).start(popen)

    def vA(self, project: str) -> dict[str, Any]:
        """Port of `nf.verify` via `vx docker exec <project>-db-1 psql` / `<project>-app-1`:
        {items, has_note, tokens, token_files, missing, seed_file, redis_keys, markers}. Filled in with the stack-a
        fixture (H1b's `sentinels.py` reads it)."""
        raise Pending("vA: filled in with the stack-a fixture (H1b)")

    def vB(self, project_id: str) -> dict[str, Any]:
        """Port of `nf.verify_supabase` via `supabase_db_<id>` / `supabase_storage_<id>`. Filled in by P1."""
        raise Pending("vB: filled in with the stack-b fixture (P1)")
