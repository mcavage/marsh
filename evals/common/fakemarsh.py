#!/usr/bin/env python3
"""A stand-in for the `marsh` CLI and its agents, for dry runs of the eval
harnesses. It needs no VM, no Docker Sandboxes, and no model credentials.

It implements only the documented host-side surface the harnesses drive:

    marsh -c SCRIPT                      run SCRIPT with sh, agents on PATH
    marsh split [-n] [-b L=S]... [::: L CMD ARG... ]...   prints a JSON handle
    marsh join [--keep] [--json] [-- CMD ARG...]          reads the handle
    marsh fanout [-n] [-b L=S]... [::: L CMD...]...       prints a JSON handle
    marsh collect [--json]                                reads the handle
    marsh results [--json] | jobs | status | stop | --version

Branches run with `sh` on a copy of the project (split) or in it (fanout),
with fake `claude`, `codex`, and `pi` first on PATH, and a `shell` that, like
the shell Kit, is `sh`. The fake agents follow a
plan file (FAKEMARSH_PLAN, JSON) so a dry run can script solves, failures and
judge verdicts; see `agent_main`. Every invocation is appended as one JSON
line to FAKEMARSH_LOG when it is set, so a grader can see what was called.

Brush's `split { }` / `fanout { }` block syntax is not emulated: a script that
uses it fails here and must be graded against the real product.
"""
from __future__ import annotations

import concurrent.futures
import fcntl
import json
import os
import pathlib
import secrets
import shutil
import subprocess
import sys
import time

AGENTS = ("claude", "codex", "pi")
HERE = pathlib.Path(__file__).resolve()


def log(argv: list[str]) -> None:
    path = os.environ.get("FAKEMARSH_LOG")
    if path:
        with open(path, "a", encoding="utf-8") as handle:
            handle.write(json.dumps({"argv": argv, "cwd": os.getcwd(), "t": time.time()}) + "\n")


def agent_dir() -> pathlib.Path:
    """A directory of agent links to this file, created on demand."""
    root = pathlib.Path(os.environ.get("FAKEMARSH_STATE") or (pathlib.Path.home() / ".fakemarsh"))
    bin_dir = root / "bin"
    bin_dir.mkdir(parents=True, exist_ok=True)
    for name in AGENTS + ("marsh", "shell"):
        link = bin_dir / name
        if not link.exists():
            link.write_text(f"#!/bin/sh\nexec {sys.executable} {HERE} \"$0\" \"$@\"\n")
            link.chmod(0o755)
    return bin_dir


def branch_env() -> dict[str, str]:
    env = dict(os.environ)
    env["PATH"] = f"{agent_dir()}{os.pathsep}{env.get('PATH', '')}"
    return env


def git(cwd: pathlib.Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(["git", "-c", "user.name=fake", "-c", "user.email=fake@example.invalid",
                           "-c", "commit.gpgsign=false", *args], cwd=cwd, capture_output=True, check=check)


def parse_branches(args: list[str]) -> tuple[bool, list[tuple[str, str, object]]]:
    """Return (no_input, [(label, kind, command)]) from -n/-b/::: words."""
    no_input, branches, index = False, [], 0
    while index < len(args):
        word = args[index]
        if word == "-n":
            no_input = True
        elif word == "-b":
            index += 1
            label, _, script = args[index].partition("=")
            branches.append((label, "shell", script))
        elif word.startswith("-b") and "=" in word:
            label, _, script = word[2:].partition("=")
            branches.append((label, "shell", script))
        elif word == ":::":
            end = index + 1
            while end < len(args) and args[end] != ":::":
                end += 1
            words = args[index + 1:end]
            if not words:
                raise SystemExit("marsh: split: empty ::: branch")
            branches.append((words[0], "argv", words[1:]))
            index = end - 1
        else:
            raise SystemExit(f"marsh: unknown argument {word!r}")
        index += 1
    if not branches:
        raise SystemExit("marsh: no branches")
    return no_input, branches


def run_branch(kind: str, command: object, cwd: pathlib.Path, stdin: bytes) -> tuple[int, bytes, bytes]:
    argv = ["sh", "-c", command] if kind == "shell" else list(command)  # type: ignore[arg-type]
    done = subprocess.run(argv, cwd=cwd, input=stdin, capture_output=True, env=branch_env())
    return done.returncode, done.stdout, done.stderr


def split_main(args: list[str]) -> int:
    no_input, branches = parse_branches(args)
    stdin = b"" if no_input or sys.stdin.isatty() else sys.stdin.buffer.read()
    project = pathlib.Path.cwd()
    split_id = secrets.token_hex(6)
    root = project / ".marsh" / "split" / split_id
    out = root / "out"
    out.mkdir(parents=True)
    (project / ".marsh" / ".gitignore").write_text("*\n")

    def one(branch: tuple[str, str, object]) -> dict[str, object]:
        label, kind, command = branch
        fork = root / "forks" / label
        shutil.copytree(project, fork, symlinks=True, ignore=shutil.ignore_patterns(".marsh"))
        is_git = (fork / ".git").exists()
        if is_git:
            git(fork, "add", "-A")
            git(fork, "commit", "-q", "--allow-empty", "--no-verify", "-m", "split snapshot")
        started = int(time.time() * 1000)
        code, stdout, stderr = run_branch(kind, command, fork, stdin)
        finished = int(time.time() * 1000)
        target = out / label
        target.mkdir()
        (target / "stdout").write_bytes(stdout)
        (target / "stderr").write_bytes(stderr)
        (target / "status").write_text(f"exited {code}\n")
        files = ""
        if is_git:
            git(fork, "add", "-A")
            patch = git(fork, "diff", "--cached", "--binary", "HEAD").stdout
            files = git(fork, "diff", "--cached", "--name-status", "HEAD").stdout.decode()
            if patch:
                (target / "diff.patch").write_bytes(patch)
        (target / "files").write_text(files)
        return {"label": label, "kind": kind, "state": "exited", "status": f"exited {code}",
                "code": code, "files": len(files.splitlines()),
                "started_unix_ms": started, "finished_unix_ms": finished}

    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        records = list(pool.map(one, branches))
    status = next((r["code"] for r in records if r["code"]), 0)
    manifest = {"id": split_id, "state": "finished", "status": status, "branches": records}
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps({"id": split_id, "dir": str(out)}))
    return int(status)  # type: ignore[arg-type]


def render_split(out: pathlib.Path, split_id: str) -> bytes:
    manifest = json.loads((out / "manifest.json").read_text())
    parts = [f"# split {split_id}: {len(manifest['branches'])} branches; manifest {out}/manifest.json\n\n"]
    for record in manifest["branches"]:
        label = record["label"]
        target = out / label
        parts.append(f"== {label} ({record['status']}, shell-vm) ==\n")
        parts.append((target / "stdout").read_text(errors="replace"))
        if record["code"]:
            parts.append(f"== {label} stderr ==\n" + (target / "stderr").read_text(errors="replace"))
        files = [line.replace("\t", " ") for line in (target / "files").read_text().splitlines()]
        if files:
            parts.append(f"-- {label}: {len(files)} file{'s' if len(files) != 1 else ''} "
                         f"({', '.join(files)}); diff {target}/diff.patch --\n")
            parts.append((target / "diff.patch").read_text(errors="replace"))
        parts.append("\n")
    return "".join(parts).encode()


def read_handle(kind: str) -> dict[str, str]:
    for line in sys.stdin.read().splitlines():
        line = line.strip()
        if line.startswith("{"):
            return json.loads(line)
    raise SystemExit(f"marsh: {kind}: no handle on stdin")


def join_main(args: list[str]) -> int:
    keep = "--keep" in args
    as_json = "--json" in args
    command = args[args.index("--") + 1:] if "--" in args else None
    handle = read_handle("join")
    out = pathlib.Path(handle["dir"])
    manifest = json.loads((out / "manifest.json").read_text())
    rendering = (json.dumps(manifest) + "\n").encode() if as_json else render_split(out, handle["id"])
    status = int(manifest["status"])
    if command:
        env = branch_env()
        env.update(SPLIT_ID=handle["id"], SPLIT_DIR=str(out), SPLIT_MANIFEST=str(out / "manifest.json"))
        status = subprocess.run(command, input=rendering, env=env).returncode
    else:
        sys.stdout.buffer.write(rendering)
        sys.stdout.flush()
    if keep or (command and status):
        print(f"marsh: join: kept {out.parent} (join --keep); inspect, then: marsh splits rm {handle['id']}",
              file=sys.stderr)
    else:
        shutil.rmtree(out.parent)
    return status


def fanout_main(args: list[str]) -> int:
    no_input, branches = parse_branches(args)
    stdin = b"" if no_input or sys.stdin.isatty() else sys.stdin.buffer.read()
    state = pathlib.Path(os.environ.get("FAKEMARSH_STATE") or (pathlib.Path.home() / ".fakemarsh")) / "fanout"
    state.mkdir(parents=True, exist_ok=True)
    fanout_id = secrets.token_hex(6)
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        done = list(pool.map(lambda b: run_branch(b[1], b[2], pathlib.Path.cwd(), stdin), branches))
    records = [{"label": b[0], "code": d[0], "stdout": d[1].decode(errors="replace"),
                "stderr": d[2].decode(errors="replace")} for b, d in zip(branches, done)]
    (state / f"{fanout_id}.json").write_text(json.dumps(records))
    print(json.dumps({"fanout": fanout_id, "path": str(state / f"{fanout_id}.json")}))
    return next((r["code"] for r in records if r["code"]), 0)


def collect_main(args: list[str]) -> int:
    handle = read_handle("collect")
    records = json.loads(pathlib.Path(handle["path"]).read_text())
    if "--json" in args:
        print(json.dumps({"branches": records}))
    else:
        for record in records:
            state = "complete" if not record["code"] else f"failed: {record['code']}"
            sys.stdout.write(f"== {record['label']} ({state}) ==\n{record['stdout']}\n")
            if record["code"]:
                sys.stdout.write(f"== {record['label']} stderr ==\n{record['stderr']}\n")
    return next((r["code"] for r in records if r["code"]), 0)


def marsh_main(args: list[str]) -> int:
    log(["marsh", *args])
    if not args:
        raise SystemExit("fakemarsh: interactive shells are not emulated")
    head, rest = args[0], args[1:]
    if head == "-c":
        return subprocess.run(["sh", "-c", rest[0], *rest[1:]], env=branch_env()).returncode
    if head in ("-V", "--version"):
        print("marsh 0.0.0-fake")
        return 0
    if head == "split":
        return split_main(rest)
    if head == "join":
        return join_main(rest)
    if head == "fanout":
        return fanout_main(rest)
    if head == "collect":
        return collect_main(rest)
    if head == "results" and "--json" not in rest:
        print("CURSOR  JOB       COMMAND  PLACEMENT  STATUS     CLEANUP   WALL  RESULT  PARENT\n"
              "2       0a1b2c3d  shell    kit-vm     exited 1   verified  1.2s  failed  -\n"
              "1       4e5f6a7b  shell    kit-vm     exited 0   verified  1.1s  ok      -")
        return 0
    if head in ("results", "jobs", "status", "splits"):
        print(json.dumps({head: []}) if "--json" in rest else f"(fake) no {head}")
        return 0
    if head == "stop":
        return 0
    raise SystemExit(f"fakemarsh: unsupported command {head!r}")


def next_action(agent: str) -> dict[str, object]:
    """The plan's next action for this agent: {"agent": [action, ...]}, or
    {"agent@LABEL": [...]} for calls from split branch LABEL's fork.

    Actions: {"do": "apply", "patch": PATH} applies a patch in the cwd;
    {"do": "write", "path": P, "text": T}; {"do": "say", "text": T} prints;
    {"do": "fail"} exits 1. The last action repeats once the list runs out.
    """
    plan_path = os.environ.get("FAKEMARSH_PLAN")
    plan = json.loads(pathlib.Path(plan_path).read_text()) if plan_path else {}
    cwd = pathlib.Path.cwd()
    key = f"{agent}@{cwd.name}" if cwd.parent.name == "forks" and f"{agent}@{cwd.name}" in plan else agent
    actions = plan.get(key) or [{"do": "say", "text": f"{agent}: nothing to do"}]
    count = 0
    if plan_path:
        # Concurrent branches may call the same agent: count under a lock (an
        # unlocked read-modify-write let one call read the file while another
        # had just truncated it, crash, and leave its fork untouched).
        with open(plan_path + f".{key}.count", "a+") as counter:
            fcntl.flock(counter, fcntl.LOCK_EX)
            counter.seek(0)
            count = int(counter.read() or 0)
            counter.seek(0)
            counter.truncate()
            counter.write(str(count + 1))
    return actions[min(count, len(actions) - 1)]


def agent_main(agent: str, args: list[str]) -> int:
    log([agent, *args])
    action = next_action(agent)
    kind = action.get("do")
    if kind == "apply":
        subprocess.run(["git", "apply", str(action["patch"])], check=True)
    elif kind == "write":
        path = pathlib.Path(str(action["path"]))
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(str(action["text"]))
    elif kind == "fail":
        print(f"{agent}: failed", file=sys.stderr)
        return 1
    text = str(action.get("text", f"{agent}: done"))
    if agent == "claude" and "--output-format" in args:
        print(json.dumps({"type": "result", "subtype": "success", "result": text, "total_cost_usd": 0.01,
                          "usage": {"input_tokens": 1000, "cache_read_input_tokens": 5000,
                                    "cache_creation_input_tokens": 200, "output_tokens": 300}}))
    elif agent == "codex" and "--json" in args:
        print(json.dumps({"type": "item.completed", "item": {"type": "agent_message", "text": text}}))
        print(json.dumps({"type": "turn.completed", "usage": {"input_tokens": 6000, "cached_input_tokens": 4000,
                                                               "output_tokens": 250}}))
    else:
        if not sys.stdin.isatty():
            data = sys.stdin.read()
            # FAKEMARSH_STDIN_LOG keeps what an agent was given (a judge's input).
            if os.environ.get("FAKEMARSH_STDIN_LOG"):
                with open(os.environ["FAKEMARSH_STDIN_LOG"], "a", encoding="utf-8") as handle:
                    handle.write(data)
        print(text)
    return 0


def main(argv: list[str]) -> int:
    name = pathlib.Path(argv[1]).name if len(argv) > 1 and argv[1].startswith("/") else None
    if name in AGENTS:
        return agent_main(name, argv[2:])
    if name == "marsh":
        return marsh_main(argv[2:])
    if name == "shell":
        # The shell Kit's entrypoint is sh: `shell -c CMD` runs CMD.
        log(["shell", *argv[2:]])
        return subprocess.run(["sh", *argv[2:]]).returncode
    return marsh_main(argv[1:])


if __name__ == "__main__":
    sys.exit(main(sys.argv))
