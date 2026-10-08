#!/usr/bin/env python3
"""Daemon-owned workspaces (`marsh split` / `marsh join`) black-box acceptance.

Scenarios and the spec claims they check: docs/design/workspaces-acceptance.md.
Drives only public callers (host bash, `marsh -c` sessions, the host CLI,
registered fixture Kit commands, the in-Kit `marsh` shim through the fixture's
`pipeline` mode) and observes only public outputs: bytes, statuses,
PIPESTATUS, the user's tree and `.git`, `out/` artifacts, public JSON, and
stock `sbx exec ... docker inspect`. Runs in an isolated MARSH_HOME/control
scope and removes only VMs from its own ownership map (run.Smoke cleanup).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import re
import shlex
import shutil
import signal
import statistics
import subprocess
import sys
import time
import traceback
from typing import Any, Callable

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import doc_examples  # noqa: E402
from provenance import stock_cleanup_errors, stock_vm_inventory  # noqa: E402
from run import (  # noqa: E402
    CONTAINER,
    Smoke,
    product_owned_vm_names,
    stable_process_identity,
    verify_container_deleted,
)

SCENARIOS = {
    "W01-plain-bash": "plain_bash",
    "W02-lease-kept-joinable": "lease_kept_joinable",
    "W03-snapshot-fidelity": "snapshot_fidelity",
    "W04-snapshot-consistency": "snapshot_consistency",
    "W05-patch-roundtrip": "patch_roundtrip",
    "W06-non-git": "non_git",
    "W07-status-pipefail": "status_pipefail",
    "W08-join-after-failure": "join_after_failure",
    "W09-retention": "retention",
    "W10-environment": "environment_forwarding",
    "W11-kit-confinement": "kit_confinement",
    "W12-admin-tamper": "admin_tamper",
    "W13-nested-dotgit": "nested_dotgit",
    "W14-hostile-user-repo": "hostile_user_repo",
    "W15-label-and-argv-refusal": "label_refusal",
    "W16-caps-and-pool": "caps_and_pool",
    "W17-nested-from-kit": "nested_from_kit",
    "W18-depth-cap": "depth_cap",
    "W19-interrupt-subtree": "interrupt_subtree",
    "W20-creator-exit-cascade": "creator_exit_cascade",
    "W21-daemon-restart": "daemon_restart",
    "W22-git-clean-replaced": "git_clean_replaced",
    "W23-non-creator-join": "non_creator_join",
    "W24-performance": "performance",
    "W25-planted-symlinks": "planted_symlinks",
    "W26-nested-repository": "nested_repository",
    "W27-sugar-interrupt": "sugar_interrupt",
    "W28-quarantine-retire": "quarantine_retire",
    "W29-doc-examples": "doc_examples",
    "W30-capture-ignored": "capture_ignored",
}
# Ordered so the scenario that quarantines the fixture VM runs last.
ORDER = [
    "W03-snapshot-fidelity", "W01-plain-bash", "W07-status-pipefail",
    "W08-join-after-failure", "W10-environment", "W09-retention",
    "W05-patch-roundtrip", "W04-snapshot-consistency", "W06-non-git", "W30-capture-ignored",
    "W29-doc-examples",
    "W11-kit-confinement", "W12-admin-tamper", "W13-nested-dotgit",
    "W14-hostile-user-repo", "W15-label-and-argv-refusal", "W16-caps-and-pool",
    "W17-nested-from-kit", "W18-depth-cap", "W23-non-creator-join",
    "W19-interrupt-subtree", "W20-creator-exit-cascade", "W22-git-clean-replaced",
    "W25-planted-symlinks", "W26-nested-repository", "W27-sugar-interrupt",
    "W02-lease-kept-joinable", "W24-performance", "W21-daemon-restart",
    "W28-quarantine-retire",
]
assert sorted(ORDER) == sorted(SCENARIOS)
KIT_SCENARIOS = {
    "W01-plain-bash", "W05-patch-roundtrip", "W06-non-git", "W11-kit-confinement",
    "W12-admin-tamper", "W13-nested-dotgit", "W15-label-and-argv-refusal",
    "W17-nested-from-kit", "W18-depth-cap", "W19-interrupt-subtree",
    "W20-creator-exit-cascade", "W21-daemon-restart", "W25-planted-symlinks",
    "W28-quarantine-retire", "W29-doc-examples",
}

GIT_ENV = {"GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1", "GIT_OPTIONAL_LOCKS": "0"}
SPLIT_OVERHEAD_BUDGET_S = 1.5  # proposed (docs/design/workspaces-acceptance.md W24)
LEASE_S = 60  # docs/design/workspaces.md section 6
RO_ERRORS = ("read-only file system", "device or resource busy", "permission denied",
             "operation not permitted")


class Fail(AssertionError):
    pass


def require(condition: object, message: str) -> None:
    if not condition:
        raise Fail(message)


def text(value: bytes | None) -> str:
    return (value or b"").decode(errors="replace")


def find_records(document: Any, predicate: Callable[[dict[str, Any]], bool]) -> list[dict[str, Any]]:
    found: list[dict[str, Any]] = []
    stack = [document]
    while stack:
        value = stack.pop()
        if isinstance(value, dict):
            if predicate(value):
                found.append(value)
            stack.extend(value.values())
        elif isinstance(value, list):
            stack.extend(value)
    return found


class Workspaces(Smoke):
    def __init__(self, arguments: argparse.Namespace) -> None:
        super().__init__(arguments)
        self.prefix_bin = str(pathlib.Path(self.marsh).parent)
        self.selected = [s for s in ORDER if not arguments.only or s in arguments.only]
        self.fail_fast = arguments.fail_fast
        self.preflight_enabled = not arguments.no_preflight
        self.results: list[dict[str, Any]] = []
        self.live: list[subprocess.Popen[bytes]] = []
        self.host_env = {**self.environment, **GIT_ENV,
                         "PATH": f"{self.prefix_bin}:{self.environment.get('PATH', '/usr/bin:/bin')}",
                         "LC_ALL": "C"}

    # ---- callers -------------------------------------------------------
    def _record(self, kind: str, argv: Any, cwd: pathlib.Path, started: float,
                status: int | None, stdout: bytes, stderr: bytes) -> None:
        self.records.append({
            "caller": kind, "argv": argv, "cwd": str(cwd), "status": status,
            "elapsed_ms": round((time.monotonic() - started) * 1000),
            "stdout": text(stdout)[-8000:], "stderr": text(stderr)[-8000:],
        })

    def call(self, kind: str, argv: list[str], cwd: pathlib.Path, *, stdin: bytes = b"",
             timeout: float = 240) -> subprocess.CompletedProcess[bytes]:
        self.scope_started = True
        started = time.monotonic()
        process = subprocess.Popen(argv, cwd=cwd, env=self.host_env, stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   start_new_session=True)
        try:
            stdout, stderr = process.communicate(input=stdin, timeout=timeout)
        except subprocess.TimeoutExpired:
            self.kill(process)  # the whole process group: no orphaned callers
            self._record(kind, argv, cwd, started, None, b"", b"")
            raise Fail(f"{kind} timed out after {timeout}s: {shlex.join(argv)[:300]}") from None
        finally:
            self.kill(process)
        self._record(kind, argv, cwd, started, process.returncode, stdout, stderr)
        return subprocess.CompletedProcess(argv, process.returncode, stdout, stderr)

    def bash(self, script: str, cwd: pathlib.Path, **kw: Any) -> subprocess.CompletedProcess[bytes]:
        """Plain host bash invoking the installed CLI (`marsh` on PATH)."""
        return self.call("host-bash", ["/bin/bash", "-c", script], cwd, **kw)

    def brush(self, script: str, cwd: pathlib.Path, **kw: Any) -> subprocess.CompletedProcess[bytes]:
        """One marsh session (Brush in the shell VM)."""
        return self.call("marsh-session", [self.marsh, "-c", script], cwd, **kw)

    def cli(self, args: list[str], cwd: pathlib.Path, **kw: Any) -> subprocess.CompletedProcess[bytes]:
        return self.call("host-cli", [self.marsh, *args], cwd, **kw)

    def spawn(self, argv: list[str], cwd: pathlib.Path) -> subprocess.Popen[bytes]:
        self.scope_started = True
        process = subprocess.Popen(argv, cwd=cwd, env=self.host_env, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   start_new_session=True)
        self.live.append(process)
        self.records.append({"caller": "spawn", "argv": argv, "cwd": str(cwd), "pid": process.pid})
        return process

    def reap(self, process: subprocess.Popen[bytes], timeout: float) -> tuple[int, bytes, bytes]:
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.kill(process)
            raise Fail(f"process {process.args!r} did not exit within {timeout}s") from None
        self.records.append({"caller": "reap", "pid": process.pid, "status": process.returncode,
                             "stdout": text(stdout)[-8000:], "stderr": text(stderr)[-8000:]})
        return process.returncode, stdout, stderr

    @staticmethod
    def kill(process: subprocess.Popen[bytes]) -> None:
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                process.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                pass

    def wait_until(self, predicate: Callable[[], bool], timeout: float, what: str,
                   processes: tuple[subprocess.Popen[bytes], ...] = ()) -> None:
        deadline = time.monotonic() + timeout
        while not predicate():
            for process in processes:
                if process.poll() is not None:
                    _, out, err = self.reap(process, 5)
                    raise Fail(f"{what}: caller exited {process.returncode} first: "
                               f"stdout={text(out)[-400:]!r} stderr={text(err)[-800:]!r}")
            if time.monotonic() > deadline:
                raise Fail(f"timed out after {timeout}s waiting for {what}")
            time.sleep(0.25)

    # ---- projects and observation --------------------------------------
    def git(self, cwd: pathlib.Path, *args: str, check: bool = True) -> subprocess.CompletedProcess[bytes]:
        completed = subprocess.run(
            ["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid",
             "-c", "commit.gpgsign=false", *args],
            cwd=cwd, env={**os.environ, **GIT_ENV, "HOME": str(self.root)},
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            timeout=60, check=False)
        if check and completed.returncode != 0:
            raise RuntimeError(f"git {args}: {text(completed.stderr)}")
        return completed

    def new_repo(self, name: str, files: dict[str, bytes | str], *, commit: bool = True) -> pathlib.Path:
        project = self.root / name
        project.mkdir()
        for relative, content in files.items():
            path = project / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content.encode() if isinstance(content, str) else content)
        if commit:
            self.git(project, "init", "-q", "-b", "main")
            self.git(project, "add", "-A")
            self.git(project, "commit", "-q", "-m", "seed")
        return project

    def seed_main(self) -> None:
        """Committed base plus staged, unstaged, deleted, untracked, ignored edits."""
        self.new_repo("project-main", {
            "staged.txt": "one\n", "unstaged.txt": "one\n", "removed.txt": "gone soon\n",
            ".gitignore": "*.log\n", "src/lib.txt": "lib\n",
        })
        main = self.root / "project-main"
        (main / "staged.txt").write_text("two\n")
        self.git(main, "add", "staged.txt")
        (main / "unstaged.txt").write_text("two\n")
        (main / "removed.txt").unlink()
        (main / "untracked.txt").write_text("new\n")
        (main / "secret.log").write_text("ignored\n")
        # The isolated product's launch project is self.project; make it the main repo.
        shutil.rmtree(self.project)
        main.rename(self.project)

    @staticmethod
    def tree_state(root: pathlib.Path) -> dict[str, tuple[Any, ...]]:
        """Every user path (not `.git`/`.marsh`): kind, exec bit, bytes digest or link target."""
        state: dict[str, tuple[Any, ...]] = {}
        for path in sorted(root.rglob("*")):
            relative = path.relative_to(root)
            if relative.parts[0] in {".git", ".marsh"}:
                continue
            if path.is_symlink():
                state[str(relative)] = ("l", os.readlink(path))
            elif path.is_file():
                state[str(relative)] = ("f", bool(path.stat().st_mode & 0o100),
                                        hashlib.sha256(path.read_bytes()).hexdigest())
        return state

    def user_state(self, project: pathlib.Path) -> dict[str, Any]:
        """The user's tree plus `.git` index bytes, refs, objects, config, hooks."""
        state: dict[str, Any] = {"tree": self.tree_state(project)}
        git_dir = project / ".git"
        if git_dir.is_dir():
            index = git_dir / "index"
            state["index"] = (hashlib.sha256(index.read_bytes()).hexdigest(), index.stat().st_mtime_ns) \
                if index.exists() else None
            state["head"] = (git_dir / "HEAD").read_bytes()
            state["config"] = (git_dir / "config").read_bytes()
            state["refs"] = sorted(str(p.relative_to(git_dir)) + ":" + p.read_text()
                                   for p in (git_dir / "refs").rglob("*") if p.is_file())
            state["packed"] = (git_dir / "packed-refs").read_bytes() if (git_dir / "packed-refs").exists() else b""
            state["objects"] = sorted(str(p.relative_to(git_dir)) for p in (git_dir / "objects").rglob("*")
                                      if p.is_file())
            state["hooks"] = sorted(p.name for p in (git_dir / "hooks").iterdir()) \
                if (git_dir / "hooks").is_dir() else []
            state["worktrees"] = (git_dir / "worktrees").exists()
        return state

    def assert_user_unchanged(self, project: pathlib.Path, before: dict[str, Any], scenario: str) -> None:
        after = self.user_state(project)
        changed = sorted(key for key in before if before[key] != after.get(key))
        require(not changed, f"{scenario}: the user's {changed} changed (marsh never writes the "
                             "caller's tree or .git; workspaces.md s1, s8)")

    @staticmethod
    def split_root(project: pathlib.Path) -> pathlib.Path:
        return project / ".marsh" / "split"

    def split_ids(self, project: pathlib.Path) -> set[str]:
        root = self.split_root(project)
        return {p.name for p in root.iterdir() if p.is_dir()} if root.is_dir() else set()

    def out(self, project: pathlib.Path, split_id: str) -> pathlib.Path:
        return self.split_root(project) / split_id / "out"

    def handle(self, completed: subprocess.CompletedProcess[bytes] | bytes, what: str) -> str:
        stdout = completed if isinstance(completed, bytes) else completed.stdout
        lines = [line for line in stdout.splitlines() if line.strip()]
        try:
            document = json.loads(lines[-1])
        except (IndexError, json.JSONDecodeError):
            stderr = "" if isinstance(completed, bytes) else text(completed.stderr)
            raise Fail(f"{what}: no handle line {{\"marsh_split\":1,\"id\":...}} on stdout "
                       f"(s2): stdout={text(stdout)[-300:]!r} stderr={stderr[-600:]!r}") from None
        require(isinstance(document, dict) and document.get("marsh_split") == 1
                and isinstance(document.get("id"), str) and set(document) == {"marsh_split", "id"},
                f"{what}: malformed handle {lines[-1]!r}")
        require(re.fullmatch(r"[A-Za-z0-9_-]{1,64}", document["id"]),
                f"{what}: split id is not a safe path component: {document['id']!r}")
        return document["id"]

    @staticmethod
    def handle_line(split_id: str) -> bytes:
        return json.dumps({"marsh_split": 1, "id": split_id}, separators=(",", ":")).encode() + b"\n"

    def splits(self, cwd: pathlib.Path, split_id: str | None = None) -> Any:
        completed = self.cli(["splits", "--json", *([split_id] if split_id else [])], cwd, timeout=30)
        require(completed.returncode == 0, f"`marsh splits --json` failed ({completed.returncode}): "
                                           f"{text(completed.stderr)[-600:]}")
        return json.loads(completed.stdout)

    def split_record(self, cwd: pathlib.Path, split_id: str) -> dict[str, Any]:
        document = self.splits(cwd, split_id)
        records = find_records(document, lambda d: d.get("id") == split_id and "state" in d)
        require(records, f"`marsh splits --json {split_id}` lacks the split: {document!r}"[:800])
        return records[0]

    @staticmethod
    def branch_status(project: pathlib.Path, split_id: str, label: str) -> str:
        path = project / ".marsh" / "split" / split_id / "out" / label / "status"
        require(path.is_file(), f"out/{label}/status missing for split {split_id} (s5)")
        return path.read_text().strip()

    def fixture_job_ids(self) -> set[str]:
        return {summary["job_id"] for summary in self.document("jobs", "--json")["jobs"]
                if summary.get("command") == "fixture"}

    def new_receipts(self, before: set[str], count: int, timeout: float = 120,
                     finished: bool = True, states: tuple[str, ...] = ("finished",)) -> list[dict[str, Any]]:
        deadline = time.monotonic() + timeout
        while True:
            fresh = [self.document("jobs", "show", job, "--json")
                     for job in sorted(self.fixture_job_ids() - before)]
            done = [r for r in fresh if r.get("state") in states] if finished else fresh
            if len(done) >= count and (not finished or len(done) == len(fresh)):
                return fresh
            if time.monotonic() > deadline:
                raise Fail(f"expected {count} {'finished ' if finished else ''}fixture jobs, got "
                           f"{[(r.get('job_id'), r.get('state')) for r in fresh]}")
            time.sleep(0.5)

    def verify_deleted(self, receipts: list[dict[str, Any]]) -> None:
        for receipt in receipts:
            container = receipt.get("container_id")
            require(receipt.get("cleanup") == "verified" and CONTAINER.fullmatch(container or ""),
                    f"Kit container is not verified deleted: {receipt!r}"[:800])
            verify_container_deleted(lambda argv: self.run(argv, timeout=30, check=False),
                                     [self.sbx, "exec", "-u", "root", receipt["vm_id"]], container)

    def fixture_capable(self, *blobs: bytes | str) -> None:
        joined = "".join(b if isinstance(b, str) else text(b) for b in blobs)
        if "unknown fixture mode: pipeline" in joined:
            raise Fail("fixture Kit lacks the `pipeline` mode (nested-split driver); pass "
                       "--kit tests/acceptance/fixture or republish the fixture from this checkout")

    # ---- scenarios ------------------------------------------------------
    def plain_bash(self) -> None:
        """s2 primary CLI from host bash: spool, -b and :::, join -- CMD, SPLIT_*, release."""
        project, dest = self.project, self.root / "w01"
        dest.mkdir()
        before, jobs = self.user_state(project), self.fixture_job_ids()
        script = (
            "printf 'in\\000put\\n' | marsh split -b sh='cat' ::: k fixture streams 'a b' '$x' '' '*' "
            "| marsh join -- sh -c 'cat > \"$1/rendering\"; env | grep \"^SPLIT_\" | sort > \"$1/env\"; "
            "cp -R \"$SPLIT_DIR\" \"$1/out\"; cp \"$SPLIT_MANIFEST\" \"$1/manifest.json\"' _ "
            f"{shlex.quote(str(dest))}; echo \"rc=$? ${{PIPESTATUS[*]}}\"")
        completed = self.bash(script, project)
        require(text(completed.stdout).strip().endswith("rc=0 0 23 0"),
                f"want PIPESTATUS 0 23 0 (split exits first nonzero branch status; join exits CMD's): "
                f"{text(completed.stdout)!r} {text(completed.stderr)[-800:]!r}")
        spool = b"in\0put\n"
        out = dest / "out"
        require((out / "sh" / "stdout").read_bytes() == spool, "shell branch did not read the spool")
        require((out / "k" / "stdout").read_bytes() == b'OUT\0["a b","$x","","*"]\n' + spool,
                f"argv branch argv/spool bytes differ: {(out / 'k' / 'stdout').read_bytes()!r}")
        require((out / "k" / "stderr").read_bytes() == b"ERR\0fixture\n", "argv branch stderr bytes differ")
        require((out / "k" / "status").read_text().strip() == "exited 23", "k status != exited 23")
        require((out / "sh" / "status").read_text().strip() == "exited 0", "sh status != exited 0")
        env = dict(line.split("=", 1) for line in (dest / "env").read_text().splitlines())
        split_id = env.get("SPLIT_ID", "")
        require({"SPLIT_ID", "SPLIT_DIR", "SPLIT_MANIFEST", "SPLIT_OBJECTS"} <= set(env),
                f"join CMD lacks SPLIT_* (root split): {env}")
        require(env["SPLIT_DIR"] == str(self.out(project, split_id)),
                f"SPLIT_DIR is not <root>/.marsh/split/<id>/out: {env['SPLIT_DIR']}")
        require("exited 23" in (dest / "rendering").read_text(errors="replace"),
                "rendering on CMD stdin lacks branch statuses")
        manifest = json.loads((dest / "manifest.json").read_text())
        require(manifest.get("version") == 2, f"manifest version != 2: {manifest!r}"[:400])
        placements = {b.get("label"): b.get("placement") for b in find_records(manifest, lambda d: "label" in d)}
        require(placements.get("sh") == "shell-vm" and str(placements.get("k", "")).startswith("kit:"),
                f"manifest placement per branch wrong: {placements}")
        require(split_id not in self.split_ids(project), "CMD exited 0 but the split was not removed (s2)")
        self.assert_user_unchanged(project, before, "W01")
        self.verify_deleted(self.new_receipts(jobs, 1))

    def lease_kept_joinable(self) -> None:
        """s6 lease: unheld 60 s -> kept, still joinable by its creator (two-step)."""
        # The session's commands see the project, not the harness root.
        dest = self.project / ".git" / "marsh-w02"
        dest.mkdir()
        script = (
            "h=$(marsh split -b a='printf A > a.txt' </dev/null); echo \"split=$?\"; "
            f"printf '%s\\n' \"$h\" > {shlex.quote(str(dest / 'handle'))}; sleep {LEASE_S + 8}; "
            f"marsh splits --json > {shlex.quote(str(dest / 'kept.json'))}; "
            "marsh join -- sh -c 'cat >/dev/null; cat \"$SPLIT_DIR/a/files\"' <<<\"$h\"; echo \"join=$?\"")
        completed = self.brush(script, self.project, timeout=LEASE_S + 240)
        out = text(completed.stdout)
        require("split=0" in out, f"two-step split failed: {out!r} {text(completed.stderr)[-600:]!r}")
        split_id = self.handle((dest / "handle").read_bytes(), "two-step handle")
        kept = find_records(json.loads((dest / "kept.json").read_text()), lambda d: d.get("id") == split_id)
        require(kept and kept[0].get("state") == "kept",
                f"unheld awaiting split was not kept after {LEASE_S}s: {kept!r}")
        require("A\ta.txt" in out and "join=0" in out, f"kept split was not joinable: {out!r}")
        require(split_id not in self.split_ids(self.project), "consumed(0) did not remove the kept split")

    def snapshot_fidelity(self) -> None:
        """s1/s8: fork mirrors H, I, worktree incl. untracked; ignored and .marsh absent; gitfile."""
        project, dest = self.project, self.root / "w03"
        before = self.user_state(project)
        expected_status = [line for line in text(self.git(project, "status", "--porcelain=v1", "-uall").stdout)
                           .splitlines() if ".marsh" not in line]
        expected_cached = text(self.git(project, "diff", "--cached", "--name-status").stdout).splitlines()
        head = text(self.git(project, "rev-parse", "HEAD").stdout).strip()
        script = ("git status --porcelain=v1 -uall; echo ---; git diff --cached --name-status; echo ---; "
                  "git rev-parse HEAD; echo ---; ls -A; echo ---; test -f .git && echo gitfile")
        completed = self.bash(
            f"marsh split -n -b g={shlex.quote(script)} | marsh join -- sh -c "
            f"'cat >/dev/null; cp \"$SPLIT_DIR/g/stdout\" \"$1\"' _ {shlex.quote(str(dest))}", project)
        require(completed.returncode == 0 and dest.exists(),
                f"split | join failed: {completed.returncode} {text(completed.stderr)[-800:]!r}")
        parts = dest.read_text().split("---\n")
        require(len(parts) == 5, f"fork probe output malformed: {dest.read_text()!r}")
        require(parts[0].splitlines() == expected_status,
                f"fork status {parts[0]!r} != user's {expected_status!r} (snapshot S/H/I)")
        require(parts[1].splitlines() == expected_cached, f"fork index {parts[1]!r} != user's I")
        require(parts[2].strip() == head, "fork HEAD != user's H")
        entries = set(parts[3].split())
        require(entries == {".git", ".gitignore", "src", "staged.txt", "unstaged.txt", "untracked.txt"},
                f"fork top level {sorted(entries)} (ignored secret.log and .marsh must be absent)")
        require(parts[4].strip() == "gitfile", "fork .git is not a gitfile (s8)")
        self.assert_user_unchanged(project, before, "W03")

    def snapshot_consistency(self) -> None:
        """s8 M2: user edits made while the split runs never reach the branch or its patch."""
        project = self.new_repo("w04", {"u.txt": "one\n"})
        # Shell branches see the project (not the harness root): gate in .git.
        gate = project / ".git" / "marsh-w04-gate"
        gate.mkdir()
        branch = (f"touch {gate}/ready; while [ ! -e {gate}/go ]; do sleep 0.1; done; "
                  "cat u.txt; cat late.txt 2>/dev/null; echo end")
        process = self.spawn([self.marsh, "split", "-n", "-b", f"r={branch}"], project)
        try:
            self.wait_until(lambda: (gate / "ready").exists(), 120, "branch start", (process,))
            (project / "u.txt").write_text("two\n")
            (project / "late.txt").write_text("late\n")
            self.git(project, "add", "u.txt")
            user_after = self.user_state(project)
            (gate / "go").touch()
            status, stdout, stderr = self.reap(process, 120)
        finally:
            self.kill(process)
        require(status == 0, f"split exited {status}: {text(stderr)[-600:]!r}")
        split_id = self.handle(stdout, "W04 split")
        out = self.out(project, split_id) / "r"
        require((out / "stdout").read_text() == "one\nend\n",
                f"branch saw post-snapshot user edits: {(out / 'stdout').read_text()!r}")
        files = (out / "files").read_text() if (out / "files").exists() else ""
        require(not (out / "diff.patch").exists() and not files.strip(),
                f"a non-writing branch produced a patch (user's later edits leaked into R): {files!r}")
        joined = self.cli(["join"], project, stdin=self.handle_line(split_id))
        require(joined.returncode == 0, f"join failed: {text(joined.stderr)[-400:]!r}")
        self.assert_user_unchanged(project, user_after, "W04")

    def patch_roundtrip(self) -> None:
        """s5/s8 patch writer: binary, mode, symlink, typechange, delete, add, quoting via git apply."""
        binary = bytes(range(256)) * 12
        project = self.new_repo("w05", {
            "bin.dat": binary, "script.sh": "#!/bin/sh\n", "kscript.sh": "#!/bin/sh\n",
            "target.txt": "t\n", "other.txt": "o\n", "typechg.txt": "regular\n",
            "gone.txt": "bye\n", "noeol.txt": "a\nb", "dir with space/\u00fc.txt": "u\n",
        })
        os.symlink("target.txt", project / "link")
        self.git(project, "add", "-A")
        self.git(project, "commit", "-q", "-m", "links")
        expected = self.tree_state(project)
        edit = ("printf '\\000\\377\\376\\375tail' >> bin.dat; chmod 755 script.sh; "
                "rm link && ln -s other.txt link; rm typechg.txt && ln -s target.txt typechg.txt; "
                "rm gone.txt; printf '\\000\\001\\002\\003' > new.bin; mkdir -p d && printf 'x\\n' > d/new.txt; "
                "printf z > noeol.txt; printf 'v\\n' > 'dir with space/\u00fc.txt'")
        dest = self.root / "w05-out"
        dest.mkdir()
        before_index = self.user_state(project)["index"]
        completed = self.bash(
            f"marsh split -n -b edit={shlex.quote(edit)} ::: kmode fixture project-chmod kscript.sh 755 "
            "::: klink fixture project-symlink klink target.txt | marsh join -- sh -c "
            "'cat > \"$1/rendering\"; for l in edit kmode klink; do cp \"$SPLIT_DIR/$l/files\" \"$1/$l.files\" && "
            "git apply \"$SPLIT_DIR/$l/diff.patch\" || exit 9; done' _ " + shlex.quote(str(dest)), project)
        require(completed.returncode == 0, f"split | join -- git apply failed ({completed.returncode}): "
                                           f"{text(completed.stderr)[-1200:]!r}")

        def file(content: bytes, executable: bool = False) -> tuple[Any, ...]:
            return ("f", executable, hashlib.sha256(content).hexdigest())

        expected.update({
            "bin.dat": file(binary + b"\0\xff\xfe\xfdtail"), "script.sh": file(b"#!/bin/sh\n", True),
            "kscript.sh": file(b"#!/bin/sh\n", True), "link": ("l", "other.txt"),
            "typechg.txt": ("l", "target.txt"), "new.bin": file(b"\0\1\2\3"), "d/new.txt": file(b"x\n"),
            "noeol.txt": file(b"z"), "dir with space/\u00fc.txt": file(b"v\n"), "klink": ("l", "target.txt"),
        })
        del expected["gone.txt"]
        actual = self.tree_state(project)
        diff = {k: (expected.get(k), actual.get(k)) for k in expected.keys() | actual.keys()
                if expected.get(k) != actual.get(k)}
        require(not diff, f"git apply of diff.patch did not reproduce the branch results: {diff}")

        def listing(label: str) -> set[tuple[str, ...]]:
            return {tuple(line.split("\t", 1)) for line in (dest / f"{label}.files").read_text().splitlines()}

        require(listing("edit") == {("M", "bin.dat"), ("M", "script.sh"), ("M", "link"), ("T", "typechg.txt"),
                                    ("D", "gone.txt"), ("A", "new.bin"), ("A", "d/new.txt"),
                                    ("M", "noeol.txt"), ("M", "dir with space/\u00fc.txt")},
                f"out/edit/files wrong: {listing('edit')}")
        require(listing("kmode") == {("M", "kscript.sh")} and listing("klink") == {("A", "klink")},
                f"Kit branch files wrong: {listing('kmode')} {listing('klink')}")
        require(self.user_state(project)["index"] == before_index, "marsh wrote the user's .git/index")
        rendering = (dest / "rendering").read_text(errors="replace")
        require("binary file changed: bin.dat (" in rendering and "binary file changed: new.bin "
                "(4 bytes; full patch in $SPLIT_DIR/edit/diff.patch)" in rendering and "GIT binary patch" not in rendering,
                f"the text rendering must summarize binary hunks (diff.patch keeps them): {rendering[-1500:]!r}")

    def capture_ignored(self) -> None:
        """s8 capture: what a branch creates that the project's ignore rules exclude (`__pycache__/` from
        an import) is not captured; a snapshotted file still is, even once a rule matches it; binary
        changes point at diff.patch; `join --timing` reports the split's phases."""
        project = self.new_repo("w30", {".gitignore": "__pycache__/\n*.log\n", "x.py": "X = 1\n",
                                        "notes.txt": "alpha\n", "blob.bin": b"\0\1\2"})
        branch = ("python3 -c 'import x' && ls __pycache__ | grep -c '[.]pyc$'; "
                  "mkdir -p build/__pycache__ && printf '\\000' > build/__pycache__/y.pyc; echo r > run.log; "
                  "printf 'notes.txt\\n' >> .gitignore; echo beta >> notes.txt; printf '\\000\\003' >> blob.bin")
        consumer = 'cat; echo "==files"; cat "$SPLIT_DIR/py/files"; echo "==patch"; cat "$SPLIT_DIR/py/diff.patch"'
        completed = self.bash(f"marsh split -n -b py={shlex.quote(branch)} | "
                              f"marsh join --timing -- sh -c {shlex.quote(consumer)}", project)
        out, err = text(completed.stdout), text(completed.stderr)
        rendering, _, rest = out.partition("==files\n")
        files, _, patch = rest.partition("==patch\n")
        require(completed.returncode == 0 and "== py (exited 0, shell-vm) ==\n1\n" in rendering,
                f"the branch did not run or made no __pycache__: {completed.returncode} {out[-1200:]!r} {err[-600:]!r}")
        require(set(files.splitlines()) == {"M\t.gitignore", "M\tnotes.txt", "M\tblob.bin"},
                f"out/py/files must hold only the non-ignored changes: {files!r}")
        headers = [line for line in (patch + rendering).splitlines() if line.startswith("diff --git ")]
        require(not any("pyc" in line or "run.log" in line for line in headers),
                f"ignored files the branch created were captured: {headers!r}")
        require("binary file changed: blob.bin (5 bytes; full patch in $SPLIT_DIR/py/diff.patch)" in rendering,
                f"binary change text must point at diff.patch: {rendering[-1200:]!r}")
        require(re.search(r"split [0-9a-f]+ timing: snapshot \d+ms; forks \d+ms; run [\d.]+m?s; "
                          r"capture py [\d.]+m?s; consumer [\d.]+m?s", err),
                f"`join --timing` lacks the phase times: {err[-800:]!r}")

    def non_git(self) -> None:
        """A plain directory splits; patches apply with `git apply`; Kit stays confined."""
        plain = self.new_repo("w06", {"calc.py": "def add(a, b):\n    return a - b\n",
                                      "keep.txt": "keep\n", "img.bin": bytes(range(256))}, commit=False)
        escape = plain / "escaped.txt"
        before = self.tree_state(plain)
        completed = self.bash(
            "marsh split -n -b a='printf new > n.txt; rm keep.txt; printf \"\\000\" >> img.bin' "
            "::: k fixture project-write calc.py fixed "
            f"::: esc fixture project-write {shlex.quote(str(escape))} X "
            "| marsh join -- sh -c 'cat >/dev/null; git apply \"$SPLIT_DIR/a/diff.patch\" && "
            "git apply \"$SPLIT_DIR/k/diff.patch\"'; echo \"rc=$? ${PIPESTATUS[*]}\"", plain)
        out = text(completed.stdout)
        require(re.search(r"rc=0 [1-9]\d* 0\s*$", out),
                f"want split nonzero (esc refused) and join CMD 0: {out!r} {text(completed.stderr)[-800:]!r}")
        require(not escape.exists(), "a Kit branch wrote the user's plain directory")
        require((plain / "calc.py").read_text() == "fixed" and (plain / "n.txt").read_text() == "new"
                and not (plain / "keep.txt").exists()
                and (plain / "img.bin").read_bytes() == bytes(range(256)) + b"\0",
                f"non-git patches did not apply: {self.tree_state(plain)} vs {before}")
        require(not (plain / ".git").exists(), "split created a .git in a plain directory")
        require(not (plain / ".marsh" / ".gitignore").exists(), "split wrote .marsh/.gitignore in a non-Git directory")

    def status_pipefail(self) -> None:
        """s2: exit = first nonzero in declaration order; pipefail returns CMD's when nonzero."""
        script = (
            "set -o pipefail; "
            "marsh split -n -b a='sleep 2; exit 3' -b b='exit 5' -b c=true | marsh join -- true; "
            "echo \"p1=$? ${PIPESTATUS[*]}\"; "
            "marsh split -n -b a='sleep 2; exit 3' -b b='exit 5' | marsh join -- sh -c 'cat >/dev/null; exit 4'; "
            "echo \"p2=$? ${PIPESTATUS[*]}\"; set +o pipefail; "
            "marsh split -b a='exit 3' </dev/null | marsh join -- true; echo \"p3=$? ${PIPESTATUS[*]}\"")
        out = text(self.bash(script, self.project).stdout)
        for want in ("p1=3 3 0", "p2=4 3 4", "p3=0 3 0"):
            require(want in out, f"want {want!r}: {out!r}")

    def join_after_failure(self) -> None:
        """s2: join -- CMD runs after a failed branch with statuses; sugar keeps PIPESTATUS/SPLIT_*."""
        dest = self.root / "w08"
        dest.mkdir()
        completed = self.bash(
            "marsh split -n -b ok='echo fine' -b bad='echo oops >&2; exit 4' | marsh join -- sh -c "
            "'cat > \"$1/r\"; echo \"$SPLIT_ID\" > \"$1/id\"; cat \"$SPLIT_DIR/bad/status\" > \"$1/bs\"; "
            "cat \"$SPLIT_DIR/bad/stderr\" > \"$1/be\"' _ " + shlex.quote(str(dest))
            + "; echo \"rc=$? ${PIPESTATUS[*]}\"", self.project)
        require(text(completed.stdout).strip().endswith("rc=0 4 0"), f"{text(completed.stdout)!r}")
        rendering = (dest / "r").read_text(errors="replace")
        require("exited 4" in rendering and "exited 0" in rendering, f"rendering lacks statuses: {rendering!r}")
        require((dest / "bs").read_text().strip() == "exited 4" and (dest / "be").read_text() == "oops\n",
                "failed branch status/stderr not in out/")
        require((dest / "id").read_text().strip() not in self.split_ids(self.project),
                "CMD status 0 must remove the split even when a branch failed")
        sugar = self.brush(
            "printf in | split { a: cat; b: exit 3 } | join | { cat >/dev/null; cat \"$SPLIT_DIR/a/stdout\"; "
            "echo; cat \"$SPLIT_DIR/b/status\"; echo; }; echo \"rc=$? ${PIPESTATUS[*]}\"; "
            "echo \"after=${SPLIT_ID-unset}\"", self.project)
        lines = text(sugar.stdout).splitlines()
        require(lines[:2] == ["in", "exited 3"] and lines[-1] == "after=unset",
                f"Brush sugar lost out/ or SPLIT_* scoping: {text(sugar.stdout)!r} {text(sugar.stderr)[-600:]!r}")
        pipestatus = re.search(r"^rc=0 0 3 (\d+) 0$", text(sugar.stdout), re.M)
        require(pipestatus, f"sugar PIPESTATUS changed: {text(sugar.stdout)!r}")

    def environment_forwarding(self) -> None:
        """s2: exported env reaches -b; unexported, functions, set options, MARSH_/SBX_/DOCKER_ do not."""
        dest = self.root / "w10"
        dest.mkdir()
        probe = ("printf '%s|%s|%s|%s|%s\\n' \"${WS_EXPORTED-unset}\" \"${WS_LOCAL-unset}\" "
                 "\"${MARSH_WS_PROBE-unset}\" \"${DOCKER_HOST-unset}\" \"${SBX_WS_PROBE-unset}\"; "
                 "( : </dev/tty ) 2>/dev/null; echo \"tty=$?\"")
        secrets = ("AWS_ACCESS_KEY_ID", "OPENAI_API_KEY", "DB_PASSWORD", "GH_TOKEN", "GITHUB_PAT",
                   "CLIENT_SECRET", "GOOGLE_APPLICATION_CREDENTIALS")
        leak = self.root / "w10-leak"
        leaked = self.bash(
            "export " + " ".join(f"{name}=s3cret" for name in secrets) + "; "
            "marsh split -n -b e='env' | marsh join -- sh -c 'cat >/dev/null; cat \"$SPLIT_DIR/e/stdout\" > \"$1\"' _ "
            + shlex.quote(str(leak)), self.project)
        require(leaked.returncode == 0 and leak.exists(), f"env probe failed: {text(leaked.stderr)[-400:]!r}")
        # Match our value: SBX itself may set proxy-managed variables such as GH_TOKEN.
        forwarded = [name for name in secrets if f"{name}=s3cret" in leak.read_text()]
        require(not forwarded, f"credential-shaped variables reached a branch: {forwarded}")
        self.bash(
            "export WS_EXPORTED='a b$c' MARSH_WS_PROBE=1 DOCKER_HOST=tcp://127.0.0.1:9 SBX_WS_PROBE=1; "
            f"WS_LOCAL=1; marsh split -n -b e={shlex.quote(probe)} | marsh join -- sh -c "
            f"'cat >/dev/null; cat \"$SPLIT_DIR/e/stdout\" > \"$1\"' _ {shlex.quote(str(dest / 'host'))}",
            self.project)
        lines = (dest / "host").read_text().splitlines() if (dest / "host").exists() else []
        require(lines[:1] == ["a b$c|unset|unset|unset|unset"],
                f"host CLI env forwarding wrong (want exported only, minus MARSH_/SBX_/DOCKER_): {lines}")
        require(len(lines) == 2 and lines[1] != "tty=0", f"branch has a controlling terminal: {lines}")
        session = self.brush(
            "export E2=x; L2=y; fn() { :; }; set -f; marsh split -n -b e='echo \"${E2-unset}|${L2-unset}\"; "
            "type fn >/dev/null 2>&1 && echo fn || echo nofn; case $- in *f*) echo noglob;; *) echo glob;; esac' "
            "| marsh join -- sh -c 'cat >/dev/null; cat \"$SPLIT_DIR/e/stdout\"'", self.project)
        require(text(session.stdout) == "x|unset\nnofn\nglob\n",
                f"session-created branch inherited non-exported state: {text(session.stdout)!r} "
                f"{text(session.stderr)[-400:]!r}")

    def retention(self) -> None:
        """s2/s6 release rules, `marsh status` counts, `marsh splits rm`."""
        project = self.project

        def manifest_id(completed: subprocess.CompletedProcess[bytes]) -> str:
            require(completed.stdout.strip(), f"no manifest: {text(completed.stderr)[-400:]!r}")
            return json.loads(completed.stdout)["id"]

        ok = manifest_id(self.bash("marsh split -n -b a='printf x > a.txt' | marsh join --json", project))
        require(ok not in self.split_ids(project), "bare join, all succeeded: split not removed")
        failed = manifest_id(self.bash("marsh split -n -b a=true -b b='exit 1' | marsh join --json", project))
        kept = manifest_id(self.bash("marsh split -n -b a=true | marsh join --json --keep", project))
        cmd = self.root / "w09-id"
        self.bash(f"marsh split -n -b a=true | marsh join -- sh -c 'echo $SPLIT_ID > {cmd}; exit 1'", project)
        cmd_failed = cmd.read_text().strip()
        for split_id, why in ((failed, "failed branch"), (kept, "--keep"), (cmd_failed, "CMD nonzero")):
            require(split_id in self.split_ids(project), f"{why}: split was not kept")
            require(self.split_record(project, split_id).get("state") == "kept", f"{why}: state != kept")
        require(self.branch_status(project, failed, "b") == "exited 1", "kept out/b/status wrong")
        status = self.document("status", "--json")
        counts = find_records(status, lambda d: isinstance(d.get("kept"), int))
        require(counts and counts[0]["kept"] >= 3, f"`marsh status` does not count kept splits: {counts}")
        for split_id in (failed, kept, cmd_failed):
            removed = self.cli(["splits", "rm", split_id], project, timeout=60)
            require(removed.returncode == 0 and split_id not in self.split_ids(project),
                    f"`marsh splits rm {split_id}` failed: {text(removed.stderr)[-400:]!r}")
        # `jobs --tree` keeps a removed split's lineage, now `removed`, never still `kept`.
        tree = text(self.cli(["jobs", "--tree", "--all"], project, timeout=60).stdout)
        for split_id in (failed, kept, cmd_failed):
            line = next((row for row in tree.splitlines() if row.startswith(split_id[:8])), "")
            require(re.match(rf"{split_id[:8]}  \S+ ago +removed ", line),
                    f"`jobs --tree` shows removed split {split_id} as: {line!r}")

    def kit_confinement(self) -> None:
        """s8 mounts/NoWriteToUserTree, s4 no daemon sockets, s5 receipts gain lineage."""
        project, before, jobs = self.project, self.user_state(self.project), self.fixture_job_ids()
        targets = {"esc": f"{project}/escaped.txt", "rel": "../../../../escaped2.txt",
                   "gitw": f"{project}/.git/escaped", "objs": f"{project}/.git/objects/escaped",
                   "store": "../store.git/escaped"}
        argv = ["split", "-n"]
        for label, path in targets.items():
            argv += [":::", label, "fixture", "project-write", path, "X"]
        argv += [":::", "ok", "fixture", "project-write", "inside.txt", "Y",
                 ":::", "auth", "fixture", "authority", ":::", "id", "fixture", "identity"]
        completed = self.cli(argv, project)
        split_id = self.handle(completed, "W11 split")
        require(completed.returncode not in (0, 2), f"escaping branches did not fail: {completed.returncode}")
        for label in targets:
            require(self.branch_status(project, split_id, label) != "exited 0", f"{label} write escaped")
        require(not (project / "escaped.txt").exists() and not (project / "escaped2.txt").exists()
                and not (project / ".git" / "escaped").exists()
                and not (project / ".git" / "objects" / "escaped").exists(), "Kit wrote the user's tree/.git")
        out = self.out(project, split_id)
        require((out / "ok" / "files").read_text() == "A\tinside.txt\n", "worktree write missing from files")
        authority = json.loads((out / "auth" / "stdout").read_text())
        require(authority == {"sockets": [], "authority_env": []}, f"job has daemon/docker authority: {authority}")
        fork = self.split_root(project) / split_id
        require(json.loads((out / "id" / "stdout").read_text())["cwd"] == str(fork / "id"),
                "argv branch cwd is not its fork at the natural path")
        receipts = self.new_receipts(jobs, 8)
        for receipt in receipts:
            mounts = {(m.get("target"), m.get("access")) for m in receipt.get("mounts", [])}
            label = next((t.rsplit("/", 1)[1] for t, a in mounts
                          if a == "read_write" and t and t.startswith(f"{fork}/")
                          and t.count("/") == str(fork).count("/") + 1), None)
            require(label, f"receipt lacks a read-write fork mount: {sorted(mounts)}")
            # s8 as amended (review M6): the admin dir is writable so Git can
            # take index.lock; config/HEAD/info/packed-refs and the gitfile
            # stay read-only binds.
            want = {(f"{fork}/{label}", "read_write"), (f"{fork}/.admin/{label}", "read_write"),
                    (f"{fork}/.admin/{label}/config", "read_only"), (f"{fork}/.admin/{label}/info", "read_only"),
                    (f"{fork}/.admin/{label}/HEAD", "read_only"), (f"{fork}/.admin/{label}/packed-refs", "read_only"),
                    (f"{fork}/{label}/.git", "read_only"),
                    (f"{fork}/store.git", "read_only"), (f"{project}/.git/objects", "read_only")}
            require(want <= mounts, f"{label}: mounts differ from s8 table; missing {sorted(want - mounts)}")
            allowed = (f"{fork}/{label}", f"{fork}/.admin/{label}/", f"{fork}/store.git", f"{project}/.git/objects")
            extra = sorted((t, a) for t, a in mounts if t and (t == str(project) or t.startswith(f"{project}/"))
                           and not any(t == p.rstrip("/") or t.startswith(p.rstrip("/") + "/") for p in allowed))
            require(not extra, f"{label}: mounts beyond the s8 table (NoWriteToUserTree): {extra}")
            require(not any(a == "read_write" and t and (t.startswith(f"{fork}/store.git") or
                            t.startswith(f"{project}/.git")) for t, a in mounts),
                    f"{label}: snapshot store or user objects writable (SnapshotImmutable): {sorted(mounts)}")
            require(split_id in json.dumps(receipt.get("lineage")), f"receipt lacks lineage: {receipt!r}"[:600])
        self.verify_deleted(receipts)
        self.cli(["join"], project, stdin=self.handle_line(split_id))
        self.assert_user_unchanged(project, before, "W11")

    def fsmonitor_oracle(self, marker: pathlib.Path) -> None:
        control = self.new_repo(f"oracle-{marker.name}", {"f": "f\n"})
        self.git(control, "config", "core.fsmonitor", f"touch {marker}")
        self.git(control, "status", check=False)
        require(marker.exists(), "oracle: host git does not run core.fsmonitor; marker test would be vacuous")
        marker.unlink()

    def admin_tamper(self) -> None:
        """s8/s11: Kit writes admin config/HEAD/info/gitfile; host git on the fork runs nothing."""
        project = self.project
        marker, hook_marker = self.root / "w12-fsmonitor-ran", self.root / "w12-hook-ran"
        self.fsmonitor_oracle(marker)
        hooks = self.root / "w12-hooks"
        hooks.mkdir()
        (hooks / "pre-commit").write_text(f"#!/bin/sh\ntouch {hook_marker}\n")
        (hooks / "pre-commit").chmod(0o755)
        evil = f"[core]\n\tfsmonitor = touch {marker}\n\thooksPath = {hooks}\n"
        argv = ["split", "-n",
                ":::", "cfg", "fixture", "project-write", "../.admin/cfg/config", evil,
                ":::", "head", "fixture", "project-write", "../.admin/head/HEAD", "ref: refs/heads/evil\n",
                ":::", "info", "fixture", "project-write", "../.admin/info/info/exclude", "*\n",
                ":::", "gitfile", "fixture", "project-write", ".git", f"gitdir: {self.root}/evil\n",
                # Git's own state (index.lock, refs, COMMIT_EDITMSG) stays writable (review M6).
                ":::", "lk", "fixture", "project-write", "../.admin/lk/COMMIT_EDITMSG", "msg\n"]
        completed = self.cli(argv, project)
        split_id = self.handle(completed, "W12 split")
        self.cli(["join", "--keep"], project, stdin=self.handle_line(split_id))
        out = self.out(project, split_id)
        require(self.branch_status(project, split_id, "lk") == "exited 0",
                f"admin dir not writable for git state: {self.branch_status(project, split_id, 'lk')}")
        for label in ("cfg", "head", "info", "gitfile"):
            status = self.branch_status(project, split_id, label)
            stderr = (out / label / "stderr").read_text(errors="replace").lower() \
                if (out / label / "stderr").exists() else ""
            require(status.startswith("rejected:") or (status != "exited 0" and any(e in stderr for e in RO_ERRORS)),
                    f"{label}: admin write neither blocked read-only nor rejected: {status!r} {stderr!r}")
            fork = self.split_root(project) / split_id / label
            if fork.exists():
                self.git(fork, "status", check=False)
                self.git(fork, "commit", "-q", "--allow-empty", "-m", "t", check=False)
                configured = text(self.git(fork, "config", "--get-regexp", r"core\.(fsmonitor|hookspath)",
                                           check=False).stdout)
                require(not configured, f"{label}: fork config carries branch-written keys: {configured!r}")
        require(not marker.exists() and not hook_marker.exists(),
                "host git on a fork executed branch-written fsmonitor/hooksPath")
        self.cli(["splits", "rm", split_id], project, timeout=60)

    def nested_dotgit(self) -> None:
        """s8 capture verification: nested `.git` (any case) -> rejected and removed."""
        project, marker = self.project, self.root / "w13-fsmonitor-ran"
        evil = self.new_repo("w13-evil", {"f": "f\n"})
        self.git(evil, "config", "core.fsmonitor", f"touch {marker}")
        completed = self.cli(["split", "-n",
                              ":::", "nest", "fixture", "project-write", "src/.git", f"gitdir: {evil}/.git\n",
                              ":::", "nestcase", "fixture", "project-write", "src/.GiT", f"gitdir: {evil}/.git\n",
                              ":::", "fine", "fixture", "project-write", "src/ok.txt", "ok"], project)
        split_id = self.handle(completed, "W13 split")
        for label in ("nest", "nestcase"):
            status = self.branch_status(project, split_id, label)
            require(status.startswith("rejected:"), f"{label}: nested .git not rejected: {status!r}")
            require(not (self.split_root(project) / split_id / label).exists(), f"{label}: rejected fork kept")
        require(self.branch_status(project, split_id, "fine") == "exited 0", "clean sibling branch affected")
        self.git(project, "status", check=False)
        require(not marker.exists(), "a planted nested .git config executed on the host")
        self.cli(["join"], project, stdin=self.handle_line(split_id))

    def hostile_user_repo(self) -> None:
        """s8 isolated gix: the user's fsmonitor/filters/hooks/diff.external/include never run."""
        markers = {k: self.root / f"w14-{k}" for k in ("fsmonitor", "clean", "smudge", "external", "include")}
        project = self.new_repo("w14", {".gitattributes": "* filter=evil diff=evil\n", "f.txt": "a\n"})
        (self.root / "w14-inc").write_text(f"[core]\n\tfsmonitor = touch {markers['include']}\n")
        for key, value in (("core.fsmonitor", f"touch {markers['fsmonitor']}"),
                           ("filter.evil.clean", f"sh -c 'touch {markers['clean']}; cat'"),
                           ("filter.evil.smudge", f"sh -c 'touch {markers['smudge']}; cat'"),
                           ("filter.evil.required", "true"), ("diff.external", f"touch {markers['external']}"),
                           ("core.hooksPath", str(self.root / "w14-hooks")),
                           ("include.path", str(self.root / "w14-inc"))):
            self.git(project, "config", key, value)
        (project / "f.txt").write_text("b\n")  # stat mismatch forces hashing
        (project / "g.txt").write_text("new\n")
        completed = self.bash("marsh split -n -b a='printf x >> f.txt' | marsh join -- sh -c "
                              "'cat >/dev/null; test -s \"$SPLIT_DIR/a/diff.patch\"'", project)
        require(completed.returncode == 0, f"split in a hostile-config repo failed: {text(completed.stderr)[-600:]!r}")
        ran = [k for k, m in markers.items() if m.exists()]
        require(not ran, f"repo config executed during split/join: {ran}")

    def label_refusal(self) -> None:
        """s2/s7 setup all-or-nothing: bad labels and unregistered argv -> 2, nothing runs."""
        project, jobs = self.project, self.fixture_job_ids()
        marker = self.root / "w15-ran"
        touch = f"touch {marker}"
        cases = {
            "traversal": ["-b", f"../x={touch}"], "slash": ["-b", f"a/b={touch}"],
            "case": ["-b", f"A={touch}", "-b", f"a={touch}"], "dup": ["-b", f"a={touch}", "-b", f"a={touch}"],
            "reserved-out": ["-b", f"out={touch}"], "reserved-base": ["-b", f"Base={touch}"],
            "dot-admin": ["-b", f".admin={touch}"], "argv-traversal": [":::", "../y", "fixture", "identity"],
            "unregistered-argv": ["-b", f"ok={touch}", ":::", "k", "/bin/sh", "-c", touch],
            "unregistered-bash": [":::", "k", "bash", "-c", touch],
        }
        before_ids = self.split_ids(project)
        for name, args in cases.items():
            completed = self.cli(["split", "-n", *args], project, timeout=60)
            require(completed.returncode == 2 and completed.stderr.strip(),
                    f"{name}: want status 2 with a diagnostic, got {completed.returncode} "
                    f"{text(completed.stderr)[-300:]!r}")
            require(not marker.exists(), f"{name}: a branch ran despite setup refusal")
        require(self.split_ids(project) == before_ids and not (project / ".marsh" / "x").exists()
                and not (project / ".marsh" / "y").exists(), "refused splits left directories")
        require(self.fixture_job_ids() == jobs, "refused splits started Kit jobs")

    def caps_and_pool(self) -> None:
        """s4: 16-branch cap (setup 2) and the 8-process session pool (`failed: capacity`)."""
        project, marker = self.project, self.root / "w16-ran"
        too_many = [a for i in range(17) for a in ("-b", f"b{i}=touch {marker}")]
        completed = self.cli(["split", "-n", *too_many], project, timeout=60)
        require(completed.returncode == 2 and not marker.exists(), f"17 branches: {completed.returncode}")
        dest = self.root / "w16"
        dest.mkdir()
        nine = " ".join(f"-b p{i}='sleep 6'" for i in range(9))
        result = self.bash(f"marsh split -n {nine} | marsh join -- sh -c 'cat > \"$1/r\"; "
                           f"for d in \"$SPLIT_DIR\"/p*; do cat \"$d/status\"; echo; done > \"$1/s\"' _ {dest}; "
                           "echo \"rc=$? ${PIPESTATUS[*]}\"", project, timeout=180)
        statuses = sorted(line for line in (dest / "s").read_text().splitlines() if line) \
            if (dest / "s").exists() else []
        require(statuses.count("failed: capacity") == 1 and statuses.count("exited 0") == 8,
                f"want 8 admitted + 1 `failed: capacity`: {statuses} {text(result.stderr)[-400:]!r}")
        require("failed: capacity" in (dest / "r").read_text(), "capacity failure not visible in rendering")
        require(re.search(r"rc=0 [1-9]\d* 0", text(result.stdout)), f"join did not run: {text(result.stdout)!r}")

    def nested_script(self, child_tail: list[str]) -> str:
        return " ".join(shlex.quote(a) for a in child_tail)

    def nested_from_kit(self) -> None:
        """s2/s3/s4: nested split via in-Kit shim (argv-only); ancestor join refused; sibling layout."""
        project, jobs = self.project, self.fixture_job_ids()
        # Session commands see the project only: keep their files in .git.
        dest = project / ".git" / "marsh-w17"
        dest.mkdir()
        marker = dest / "ancestor-joined"
        inner = ["fixture", "pipeline", "|", "marsh", "split", "%%%", "c", "fixture", "project-write", "c.txt", "C",
                 "|", "tee", "/dev/stderr", "|", "marsh", "join", "--keep"]
        script = (
            f"h=$(marsh split ::: p {self.nested_script(inner)} </dev/null); echo \"root=$?\"; "
            "rid=$(printf '%s' \"$h\" | sed -n 's/.*\"id\":\"\\([^\"]*\\)\".*/\\1/p'); echo \"rid=$rid\"; "
            "ch=$(grep -o '{\"marsh_split\":1,\"id\":\"[^\"]*\"}' \".marsh/split/$rid/out/p/stderr\"); "
            "echo \"child=$ch\"; "
            f"marsh join -- touch {marker} <<<\"$ch\"; echo \"ancestor=$?\"; "
            f"marsh splits --json > {dest}/tree.json; "
            "marsh join -- true <<<\"$h\"; echo \"rootjoin=$?\"")
        completed = self.brush(script, project, timeout=300)
        out = text(completed.stdout)
        root_err = ""
        rid = re.search(r"^rid=(\S+)$", out, re.M)
        if rid and (self.out(project, rid.group(1)) / "p" / "stderr").exists():
            root_err = (self.out(project, rid.group(1)) / "p" / "stderr").read_text(errors="replace")
        self.fixture_capable(out, completed.stderr, root_err)
        require("root=0" in out, f"nested split from a Kit job failed: {out!r} {text(completed.stderr)[-600:]!r}")
        child = re.search(r'^child=\{"marsh_split":1,"id":"([^"]+)"\}$', out, re.M)
        require(child, f"nested handle not observed: {out!r}")
        cid = child.group(1)
        ancestor = re.search(r"^ancestor=(\d+)$", out, re.M)
        require(ancestor and ancestor.group(1) != "0" and not marker.exists(),
                "an ancestor session joined a split created by a Kit job (creator-only join)")
        tree = json.loads((dest / "tree.json").read_text())
        record = find_records(tree, lambda d: d.get("id") == cid)
        require(record and record[0].get("state") == "kept", f"child split not kept after refused join: {record}")
        require(rid.group(1) in json.dumps(record[0]), f"child lineage lacks parent split: {record[0]}")
        require((self.out(project, cid) / "c" / "files").read_text() == "A\tc.txt\n"
                and not (project / "c.txt").exists(), "nested result wrong or leaked into user tree")
        require(not any(self.split_root(project).glob(f"{rid.group(1)}/*/.marsh")),
                "nested split dir lives inside the parent fork (want siblings, s1)")
        require("rootjoin=0" in out, "creator could not join its own root split")
        receipts = self.new_receipts(jobs, 2)
        require(any(cid in json.dumps(r.get("lineage")) for r in receipts), "child job receipt lacks lineage")
        self.verify_deleted(receipts)
        self.cli(["splits", "rm", cid], project, timeout=60)

    def depth_cap(self) -> None:
        """s4: depth 3 allowed (root=1), a depth-4 split is refused (2) and nothing runs."""
        project, jobs = self.project, self.fixture_job_ids()
        level3 = ["fixture", "pipeline", "|3", "marsh", "split", "%%%", "d4", "fixture", "identity",
                  "|3", "marsh", "join"]
        level2 = ["fixture", "pipeline", "|2", "marsh", "split", "%%%", "d3",
                  *[("%" + a if re.fullmatch(r"%{3,}", a) else a) for a in level3], "|2", "marsh", "join"]
        argv = ["split", "-n", ":::", "d1", "fixture", "pipeline", "|1", "marsh", "split", "%%%", "d2",
                *[("%" + a if re.fullmatch(r"%{3,}", a) else a) for a in level2], "|1", "marsh", "join"]
        completed = self.cli(argv, project, timeout=400)
        split_id = self.handle(completed, "W18 root split")
        out = self.out(project, split_id) / "d1"
        blob = "".join((out / n).read_text(errors="replace") for n in ("stdout", "stderr") if (out / n).exists())
        self.fixture_capable(blob)
        require(self.branch_status(project, split_id, "d1") == "exited 2" and completed.returncode == 2,
                f"depth-4 refusal did not surface as status 2 through levels: {completed.returncode}")
        require("depth" in blob.lower(), f"depth refusal not visible: {blob[-600:]!r}")
        require(len(self.new_receipts(jobs, 3)) == 3, "want exactly d1, d2, d3 jobs (d4 never runs)")
        self.cli(["join"], project, stdin=self.handle_line(split_id))

    def interrupt_subtree(self) -> None:
        """s2/s7: Ctrl-C cancels the subtree: shell branch, Kit branch, nested Kit job; join never runs."""
        project, jobs, gate = self.project, self.fixture_job_ids(), self.project / ".git" / "marsh-w19"
        gate.mkdir()
        before_ids = self.split_ids(project)
        process = self.spawn([self.marsh, "split", "-n", "-b", f"s=touch {gate}/ready; sleep 15; touch {gate}/late",
                              ":::", "k", "fixture", "hold", "600",
                              ":::", "n", "fixture", "pipeline", "|", "marsh", "split", "%%%", "nk",
                              "fixture", "hold", "600"], project)
        try:
            def started() -> bool:
                running = [self.document("jobs", "show", j, "--json") for j in self.fixture_job_ids() - jobs]
                return (gate / "ready").exists() and sum(r.get("state") == "running" for r in running) >= 3
            self.wait_until(started, 180, "shell branch, Kit branch, and nested Kit job running", (process,))
            os.killpg(process.pid, signal.SIGINT)
            began = time.monotonic()
            status, stdout, stderr = self.reap(process, 60)
            elapsed = time.monotonic() - began
        finally:
            self.kill(process)
        require(status == 130 and not stdout.strip(), f"cancelled split exited {status}, stdout={stdout!r}")
        require(b"every branch ended; removed" in stderr, f"cancel did not report removal: {stderr[-600:]!r}")
        joined = self.cli(["join", "--", "touch", str(gate / "joined")], project, stdin=stdout)
        require(joined.returncode != 0 and not (gate / "joined").exists(), "join ran after cancellation")
        time.sleep(18)
        require(not (gate / "late").exists(), "shell branch survived Ctrl-C")
        # A job cancelled with its tree is recorded `cancelled` (processes.md s8).
        receipts = self.new_receipts(jobs, 3, timeout=60, states=("cancelled",))
        self.verify_deleted(receipts)
        left = self.split_ids(project) - before_ids
        require(not left, f"confirmed-cancelled split directories were left on disk: {left}")
        self.records.append({"scenario": "W19", "cancel_seconds": round(elapsed, 2)})
        require(elapsed <= 25, f"cancel took {elapsed:.1f}s (> SIGINT, 10 s, SIGKILL bound)")

    def creator_exit_cascade(self) -> None:
        """s4/s7: a Kit branch that exits while its child split runs cancels that subtree."""
        project, jobs = self.project, self.fixture_job_ids()
        began = time.monotonic()
        completed = self.cli(["split", "-n", ":::", "p", "fixture", "pipeline", "|", "--exit-after", "20",
                              "marsh", "split", "%%%", "c", "fixture", "hold", "600"], project, timeout=200)
        split_id = self.handle(completed, "W20 split")
        self.fixture_capable((self.out(project, split_id) / "p" / "stderr").read_bytes()
                             if (self.out(project, split_id) / "p" / "stderr").exists() else b"")
        require(time.monotonic() - began < 120, "creator exit did not cancel the child hold within bound")
        require(self.branch_status(project, split_id, "p") == "exited 7", "creator branch status != exited 7")
        receipts = self.new_receipts(jobs, 2, timeout=60, states=("finished", "cancelled"))
        require(sorted(r.get("state") for r in receipts) == ["cancelled", "finished"],
                f"want the creator finished and its child cancelled: {[r.get('state') for r in receipts]}")
        self.verify_deleted(receipts)
        children = find_records(self.splits(project), lambda d: d.get("id") not in (None, split_id)
                                and split_id in json.dumps(d.get("parent", d.get("lineage", ""))))
        require(children and all(c.get("state") in ("cancel", "cancelled", "done") for c in children),
                f"child split not cancelled after its creator exited: {children}")
        self.cli(["join"], project, stdin=self.handle_line(split_id))

    def daemon_restart(self) -> None:
        """s6: restart mid-split -> uncertain, forks retained untrusted, nothing replayed."""
        project, jobs, gate = self.project, self.fixture_job_ids(), self.project / ".git" / "marsh-w21"
        gate.mkdir()
        before_ids = self.split_ids(project)
        process = self.spawn([self.marsh, "split", "-n", "-b", f"s=touch {gate}/ready; sleep 30",
                              ":::", "k", "fixture", "hold", "600"], project)
        try:
            self.wait_until(lambda: (gate / "ready").exists() and any(
                self.document("jobs", "show", j, "--json").get("state") == "running"
                for j in self.fixture_job_ids() - jobs), 180, "branches running", (process,))
            new = self.split_ids(project) - before_ids
            require(len(new) == 1, f"cannot identify the running split: {new}")
            split_id = new.pop()
            old_pid = self.owned_daemon_pid
            # The authenticated shutdown refuses while shells and jobs are
            # active (scope contract), so restart by crashing the verified
            # owned daemon process.
            require(old_pid and stable_process_identity(old_pid) == self.owned_daemon_process_identity,
                    "cannot verify the owned daemon process before restarting it")
            os.kill(old_pid, signal.SIGKILL)
            self.wait_until(lambda: stable_process_identity(old_pid) is None, 30, "daemon exit")
            restarted = time.time() * 1000
            status, _, _ = self.reap(process, 60)
        finally:
            self.kill(process)
        require(status not in (0, None), f"client reported success across a daemon restart: {status}")
        for field in ("owned_daemon_id", "owned_daemon_pid", "owned_daemon_process_identity",
                      "owned_daemon_control_token"):
            setattr(self, field, None)  # deliberate restart: re-learn the new daemon's ownership
        self.document("status", "--json")
        record = self.split_record(project, split_id)
        require(record.get("state") == "uncertain", f"split state after restart: {record.get('state')}")
        require("untrusted" in json.dumps(record), f"retained forks not flagged untrusted: {record}")
        require((self.split_root(project) / split_id / "s").exists(), "fork was not retained")
        time.sleep(10)
        replayed = [r for r in self.new_receipts(jobs, 1, finished=False)
                    if (r.get("created_unix_ms") or 0) > restarted]
        require(not replayed, f"a branch was replayed after restart: {replayed}")
        reset = self.cli(["workers", "reset", "fixture"], project, timeout=180)
        require(reset.returncode == 0, f"workers reset failed: {text(reset.stderr)[-400:]!r}")

    def git_clean_replaced(self) -> None:
        """s6 hazard: `git clean -xfd` mid-split -> `workspace replaced`; nothing removed by path."""
        project = self.new_repo("w22", {"a.txt": "a\n"})
        gate = project / ".git" / "marsh-w22-gate"
        gate.mkdir()
        process = self.spawn([self.marsh, "split", "-n", "-b",
                              f"w=touch {gate}/ready; while [ ! -e {gate}/go ]; do sleep 0.1; done; printf x > w.txt"],
                             project)
        try:
            self.wait_until(lambda: (gate / "ready").exists(), 120, "branch start", (process,))
            ids = self.split_ids(project)
            require(len(ids) == 1, f"split dir not found: {ids}")
            split_id = ids.pop()
            self.git(project, "clean", "-xfdq")
            planted = self.split_root(project) / split_id / "w"
            planted.mkdir(parents=True)
            (planted / "sentinel").write_text("user data\n")
            (gate / "go").touch()
            status, stdout, stderr = self.reap(process, 120)
        finally:
            self.kill(process)
        require(status not in (0, None), f"split succeeded over a replaced workspace: {status}")
        require("workspace replaced" in json.dumps(self.split_record(project, split_id)) + text(stderr),
                "`workspace replaced` not reported")
        self.cli(["join"], project, stdin=self.handle_line(split_id))
        self.cli(["splits", "rm", split_id], project, timeout=60)
        require((planted / "sentinel").exists(), "daemon removed a replaced directory by path")

    def non_creator_join(self) -> None:
        """s2/s3: a different session cannot join (or release) a split."""
        project, marker = self.project, self.project / ".git" / "marsh-w23-joined"
        created = self.brush("marsh split -n -b a='printf A > a.txt'", project)
        split_id = self.handle(created, "W23 session A split")
        other = self.brush(f"marsh join -- touch {marker}", project, stdin=self.handle_line(split_id))
        require(other.returncode != 0 and not marker.exists(), "session B joined session A's split")
        require(split_id in self.split_ids(project), "refused join released the split")
        self.cli(["splits", "rm", split_id], project, timeout=60)

    def planted_symlinks(self) -> None:
        """C1: symlinks planted in .marsh (by a Kit job or the guest) never redirect host writes."""
        project = self.new_repo("w25", {"a.txt": "a\n"})
        gate = project / ".git" / "marsh-w25-gate"
        gate.mkdir()
        target = self.root / "w25-target"
        target.write_text("original\n")
        process = self.spawn([self.marsh, "split", "-n", "-b",
                              f"a=touch {gate}/ready; while [ ! -e {gate}/go ]; do sleep 0.1; done; echo branch"],
                             project)
        try:
            self.wait_until(lambda: (gate / "ready").exists(), 120, "branch start", (process,))
            split_id = self.split_ids(project).pop()
            out = self.split_root(project) / split_id / "out" / "a"
            # An ordinary (unconfined) Kit job: `.marsh` is read-only to it.
            planted = self.brush(f"cd {shlex.quote(str(project))} && fixture project-symlink "
                                 f"{shlex.quote(str(out / 'stdout'))} {shlex.quote(str(target))}", project)
            require(planted.returncode != 0 and not (out / "stdout").is_symlink(),
                    f"an ordinary Kit job wrote into .marsh: {planted.returncode} {text(planted.stderr)[-300:]!r}")
            # A guest that can write .marsh (the shell VM) plants the same links.
            (out / "stdout").symlink_to(target)
            (out / "status").symlink_to(target)
            (gate / "go").touch()
            status, _, stderr = self.reap(process, 120)
        finally:
            self.kill(process)
        require(target.read_text() == "original\n", "the daemon wrote through a planted symlink")
        require(status not in (0, None), f"tampered out/ was not reported: {status} {text(stderr)[-300:]!r}")
        record = json.dumps(self.split_record(project, split_id))
        require("tampered" in record, f"tampering not recorded: {record[:600]}")
        self.cli(["splits", "rm", split_id], project, timeout=60)
        require(target.read_text() == "original\n", "removal followed a planted symlink")

    def nested_repository(self) -> None:
        """H2: a nested repository (vendored or submodule checkout) splits; its .git is not copied."""
        project = self.new_repo("w26", {"top.txt": "t\n", "vendor/lib/lib.c": "int x;\n"}, commit=False)
        self.git(project / "vendor" / "lib", "init", "-q", "-b", "main")
        self.git(project / "vendor" / "lib", "add", "-A")
        self.git(project / "vendor" / "lib", "commit", "-q", "-m", "lib")
        self.git(project, "init", "-q", "-b", "main")
        self.git(project, "add", "top.txt")
        self.git(project, "commit", "-q", "-m", "seed")
        dest = self.root / "w26-out"
        completed = self.bash(
            "marsh split -n -b a='ls -A vendor/lib; echo x > a.txt' | marsh join -- sh -c "
            "'cat >/dev/null; cp -R \"$SPLIT_DIR/a\" \"$1\"' _ " + shlex.quote(str(dest)), project)
        require(completed.returncode == 0 and dest.exists(),
                f"split over a nested repository failed: {text(completed.stderr)[-600:]!r}")
        require((dest / "status").read_text().strip() == "exited 0", (dest / "status").read_text())
        require(".git" not in (dest / "stdout").read_text().split(), "the nested .git reached the fork")
        require((dest / "files").read_text() == "A\ta.txt\n", (dest / "files").read_text())
        require((project / "vendor" / "lib" / ".git").is_dir(), "the user's nested repository was touched")

    def sugar_interrupt(self) -> None:
        """Brush sugar: Ctrl-C cancels the daemon-side split; join and later stages never run."""
        project = self.project
        gate = project / ".git" / "marsh-w27"
        gate.mkdir()
        before_ids = self.split_ids(project)
        script = (f"split {{ a: touch {gate}/ready && sleep 30 && touch {gate}/late }} | join | touch {gate}/joined; "
                  "echo \"rc=$?\"")
        process = self.spawn([self.marsh, "-c", script], project)
        try:
            self.wait_until(lambda: (gate / "ready").exists(), 120, "sugar branch start", (process,))
            began = time.monotonic()
            os.killpg(process.pid, signal.SIGINT)
            status, stdout, stderr = self.reap(process, 60)
            elapsed = time.monotonic() - began
        finally:
            self.kill(process)
        require(elapsed <= 25, f"sugar cancel took {elapsed:.1f}s")
        require(status != 0 or b"rc=130" in stdout, f"sugar split not cancelled: {status} {stdout!r} {stderr[-300:]!r}")
        time.sleep(max(0, 32 - elapsed))
        require(not (gate / "joined").exists(), "join stages ran after Ctrl-C")
        require(not (gate / "late").exists(), "the branch survived Ctrl-C")
        left = self.split_ids(project) - before_ids
        require(not left, f"a cancelled sugar split left its directory: {left}")

    def quarantine_once(self, what: str) -> str:
        """Quarantine the fixture Kit VM by stopping it under a running job (as run.py's gate)."""
        jobs = self.fixture_job_ids()
        process = self.spawn([self.marsh, "-c", "fixture hold 600"], self.project)
        try:
            receipts: list[dict[str, Any]] = []

            def running() -> bool:
                receipts[:] = [r for r in self.new_receipts(jobs, 1, finished=False)
                               if r.get("state") == "running"]
                return bool(receipts)
            self.wait_until(running, 300, f"{what}: held fixture job running", (process,))
            vm = str(receipts[0].get("vm_id"))
            stopped = self.run([self.sbx, "stop", vm], timeout=60, check=False)
            require(stopped.returncode == 0, f"{what}: stock sbx could not stop {vm}")
            self.reap(process, 120)
        finally:
            self.kill(process)
        workers = self.document("status", "--json").get("workers", [])
        require(any(w.get("worker_id") == vm and w.get("health") == "quarantined" for w in workers),
                f"{what}: stopping the VM under a job did not quarantine it: {workers}"[:600])
        return vm

    def doc_examples(self) -> None:
        """docs/split.md: every marked example runs as written (host bash and/or the shell)."""
        failures = []
        for number, example in enumerate(doc_examples.examples()):
            runs = [(example.script, True)]
            if example.control:
                require(example.control in example.script, f"{example.name()}: control text missing")
                runs.append((example.script.replace(example.control, "true"), False))
            for variant, (script, positive) in enumerate(runs):
                project = self.new_repo(f"w29-{number}-{variant}", {"notes.txt": "alpha\nbeta\n"},
                                        commit=not example.plain)
                if example.prep and example.kind == "host":
                    self.brush("git status --short >/dev/null", project)
                elif example.prep:
                    self.git(project, "status", "--short")
                run = self.bash if example.kind == "host" else self.brush
                completed = run(script, project)
                out, err = text(completed.stdout), text(completed.stderr)
                if not positive:
                    if completed.returncode == 0:
                        failures.append(f"{example.name()}: still passes without "
                                        f"{example.control!r} (the step is not needed)")
                    continue
                if completed.returncode != example.status or example.stdout not in out \
                        or example.stderr not in err:
                    failures.append(f"{example.name()}: status {completed.returncode} (want "
                                    f"{example.status}); stdout={out[-500:]!r} stderr={err[-700:]!r}")
                for split in self.splits(project).get("splits", []):
                    if split.get("root") == str(project) and split.get("state") != "run":
                        self.cli(["splits", "rm", split["id"]], project, timeout=60)
        require(not failures, "doc examples failed:\n" + "\n".join(failures))
        status = text(self.cli(["status"], self.project, timeout=30).stdout)
        require(re.search(r"shells:\s+\d+ attached", status) and re.search(
            r"splits:\s+\d+ active, \d+ awaiting, \d+ kept", status) and re.search(
            r"shell VM:\s+\S", status) and status.count("shells:") == 1,
            f"`marsh status` text lacks attached shells or split counts: {status!r}")
        # Session stdio pipes belong to the session user: /dev/stderr reopens.
        tee = self.brush("echo via-tee | tee /dev/stderr >/dev/null", self.project)
        require(tee.returncode == 0 and "via-tee" in text(tee.stderr),
                f"`tee /dev/stderr` failed in a session: {tee.returncode} {text(tee.stderr)[-300:]!r}")
        usage = text(self.cli(["--help"], self.project, timeout=30).stdout)
        require("flight" not in usage, "`marsh --help` lists the removed flight recorder")

    def lose_shell_with_kit_pins(self, what: str) -> None:
        """A shell that ran a Kit job loses its VM: its Kit pins outlive the dead session."""
        gate = self.project / ".git" / f"marsh-w28-{what.replace(' ', '-')}"
        gate.mkdir()
        before = {n for n in product_owned_vm_names(self.control_home) if n.startswith("marsh-s-")}
        process = self.spawn([self.marsh, "-c", f"fixture identity >/dev/null; touch {gate}/ready; sleep 600"],
                             self.project)
        try:
            self.wait_until(lambda: (gate / "ready").exists(), 300, f"{what}: shell ran a Kit job", (process,))
            shells = sorted(n for n in product_owned_vm_names(self.control_home)
                            if n.startswith("marsh-s-") and n in stock_vm_inventory(self.sbx)) or sorted(before)
            require(shells, f"{what}: no owned shell VM found")
            for shell in shells:
                self.run([self.sbx, "stop", shell], timeout=60, check=False)
            self.reap(process, 120)
        finally:
            self.kill(process)

    def vm_gone(self, vm: str | None, what: str) -> None:
        inventory = stock_vm_inventory(self.sbx)
        if vm is not None:
            require(vm not in inventory, f"{what}: quarantined VM {vm} still exists")
        # Deliberately removed VMs may come back under the same random name.
        identities = getattr(self, "owned_vm_identities", {})
        for name in [name for name in identities if inventory.get(name) != identities[name]]:
            del identities[name]

    def quarantine_retire(self) -> None:
        """Owned quarantined or orphan-pinned Kit VMs are always retirable, with the cause printed."""
        def check(completed: subprocess.CompletedProcess[bytes], what: str) -> None:
            require(completed.returncode == 0, f"`marsh {what}` could not retire a VM it owns: "
                                               f"{text(completed.stdout)!r} {text(completed.stderr)[-800:]!r}")

        vm = self.quarantine_once("workers reset")
        check(self.cli(["workers", "reset", "fixture"], self.project, timeout=240), "workers reset fixture")
        self.vm_gone(vm, "workers reset")
        # The UAT path: a lost shell leaves Kit pins; Kit resets must not refuse.
        self.lose_shell_with_kit_pins("shell loss then workers reset")
        check(self.cli(["workers", "reset", "all"], self.project, timeout=240), "workers reset all")
        check(self.cli(["reset"], self.project, timeout=240), "reset")  # recovers the shell VM
        self.vm_gone(None, "reset after shell loss")
        vm = self.quarantine_once("reset")
        check(self.cli(["reset"], self.project, timeout=240), "reset")
        self.vm_gone(vm, "reset")
        self.lose_shell_with_kit_pins("shell loss then stop")
        stopped = self.cli(["stop"], self.project, timeout=240)
        check(stopped, "stop")
        require("removed" in text(stopped.stderr), f"stop did not say what it removed: {text(stopped.stderr)!r}")
        self.vm_gone(None, "stop")
        for field in ("owned_daemon_id", "owned_daemon_pid", "owned_daemon_process_identity",
                      "owned_daemon_control_token"):
            setattr(self, field, None)  # stopped on purpose; the final status starts a fresh daemon
        self.document("status", "--json")

    def performance(self) -> None:
        """Proposed budget: split overhead on a ~10k-file repo <= 1.5 s over a warm `marsh -c true`."""
        project = self.root / "w24"
        project.mkdir()
        for i in range(10_000):
            path = project / f"d{i % 100:02d}" / f"f{i:05d}.txt"
            path.parent.mkdir(exist_ok=True)
            path.write_bytes(f"{i}\n".encode() * 64)
        self.git(project, "init", "-q", "-b", "main")
        self.git(project, "add", "-A")
        self.git(project, "commit", "-q", "-m", "seed")
        split = "marsh split -n -b a=true | marsh join >/dev/null"

        def timed(script: str) -> float:
            began = time.monotonic()
            completed = self.bash(script, project, timeout=120)
            require(completed.returncode == 0, f"{script}: {text(completed.stderr)[-400:]!r}")
            return time.monotonic() - began

        timed("marsh -c true")
        timed(split)
        base = statistics.median(timed("marsh -c true") for _ in range(3))
        cost = statistics.median(timed(split) for _ in range(3))
        self.records.append({"scenario": "W24", "baseline_s": round(base, 3), "split_s": round(cost, 3)})
        require(cost - base <= SPLIT_OVERHEAD_BUDGET_S,
                f"split overhead {cost - base:.2f}s > {SPLIT_OVERHEAD_BUDGET_S}s (split {cost:.2f}s, base {base:.2f}s)")

    # ---- driver ----------------------------------------------------------
    def preflight(self) -> str | None:
        """Fail fast, before any VM work, if the product lacks the workspace CLI.

        `marsh --help` is daemon- and VM-free. An unknown word such as
        `marsh splits` is otherwise taken as a Brush script and boots a VM.
        """
        # Session stdio pipes belong to the session user: /dev/stderr reopens.
        tee = self.brush("echo via-tee | tee /dev/stderr >/dev/null", self.project)
        require(tee.returncode == 0 and "via-tee" in text(tee.stderr),
                f"`tee /dev/stderr` failed in a session: {tee.returncode} {text(tee.stderr)[-300:]!r}")
        usage = text(self.cli(["--help"], self.project, timeout=30).stdout)
        missing = [verb for verb in ("split", "join", "splits")
                   if not re.search(rf"^\s*marsh {verb}\b", usage, re.M)]
        if missing:
            return (f"`marsh --help` lists no {', '.join(f'`marsh {v}`' for v in missing)} subcommand "
                    "(workspace CLI not implemented; `marsh split ...` would run as a Brush script)")
        for args in (["splits", "--json"], ["split", "--help"], ["join", "--help"]):
            completed = self.cli(args, self.project, timeout=60)
            if completed.returncode != 0:
                lines = [re.sub(r"\x1b\[[0-9;]*m", "", line) for line in
                         (text(completed.stderr) or text(completed.stdout)).splitlines()]
                lines = [line for line in lines if line.strip() and not line.startswith("[")]
                return f"`marsh {' '.join(args)}` exited {completed.returncode}: " + \
                    (lines[0] if lines else "(no output)")
        return None

    def run_all(self) -> None:
        self.stock_before = stock_vm_inventory(self.sbx)
        self.source["stock_before"] = self.stock_before
        self.seed_main()
        self.document("status", "--json")
        if self.preflight_enabled:
            blocked = self.preflight()
            if blocked:
                for scenario in self.selected:
                    self.results.append({"scenario": scenario, "outcome": "blocked", "failure": blocked})
                    print(f"workspaces: {scenario}: blocked: {blocked}", flush=True)
                raise Fail(f"preflight: workspace CLI unavailable: {blocked}")
        if KIT_SCENARIOS & set(self.selected):
            self.run([self.marsh, "--load", "fixture", "-c", "true"], timeout=900)
            self.document("status", "--json")
        failures = []
        for scenario in self.selected:
            began = time.monotonic()
            try:
                getattr(self, SCENARIOS[scenario])()
                outcome, failure = "passed", None
            except Fail as error:
                outcome, failure = "failed", str(error)
            except Exception as error:  # an unexpected harness/product break is still a failure
                outcome, failure = "failed", f"{type(error).__name__}: {error}"
                self.records.append({"scenario": scenario, "traceback": traceback.format_exc()})
            finally:
                for process in self.live:
                    self.kill(process)
                self.live.clear()
            self.results.append({"scenario": scenario, "outcome": outcome, "failure": failure,
                                 "seconds": round(time.monotonic() - began, 1)})
            print(f"workspaces: {scenario}: {outcome}" + (f": {failure[:400]}" if failure else ""), flush=True)
            if failure:
                failures.append(scenario)
                if self.fail_fast:
                    break
        self.document("status", "--json")
        if failures:
            raise Fail(f"{len(failures)}/{len(self.selected)} scenarios failed: {', '.join(failures)}")

    def finish(self, error: Exception | None) -> int:
        for process in self.live:
            self.kill(process)
        cleanup_errors = self.cleanup_isolated_scope()
        if self.stock_before is not None:
            try:
                after = stock_vm_inventory(self.sbx)
                self.source["stock_after"] = after
                cleanup_errors.extend(stock_cleanup_errors(self.stock_before, after))
            except Exception as caught:
                cleanup_errors.append(f"independent stock cleanup unavailable: {caught}")
        if cleanup_errors:
            cleanup = RuntimeError("; ".join(dict.fromkeys(cleanup_errors)))
            error = cleanup if error is None else RuntimeError(f"{error}; cleanup: {cleanup}")
        destination = self.evidence / "workspaces.json"
        destination.write_text(json.dumps({
            "outcome": "failed" if error else "passed", "failure": str(error) if error else None,
            "scenarios": self.results, "root": str(self.root), "environment": self.source,
            "records": self.records}, indent=2, default=str) + "\n", encoding="utf-8")
        destination.chmod(0o600)
        passed = sum(r["outcome"] == "passed" for r in self.results)
        print(f"workspaces: {passed}/{len(self.selected)} passed; "
              f"{'failed' if error else 'passed'}; evidence: {destination}")
        if error:
            print(f"workspaces: {error}")
        return 1 if error else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--prefix", default=str(pathlib.Path.home() / ".marsh-dev"),
                        help="installed dev product (bin/marsh, libexec/marsh)")
    parser.add_argument("--sbx", default="sbx")
    parser.add_argument("--kit", default=None,
                        help="fixture Kit: immutable OCI ref, native v3 source dir, or a commands.json "
                             "whose \"fixture\" key holds the ref (nested scenarios need the `pipeline` mode)")
    parser.add_argument("--evidence", default=None)
    parser.add_argument("--only", action="append", choices=sorted(SCENARIOS), help="repeatable")
    parser.add_argument("--fail-fast", action="store_true", help="stop at the first failed scenario")
    parser.add_argument("--no-preflight", action="store_true")
    parser.add_argument("--list", action="store_true", help="print scenario ids and exit")
    parser.add_argument("--source-tree", default=str(pathlib.Path(__file__).resolve().parents[2]))
    parser.add_argument("--source-revision", default=None)
    parser.add_argument("--build-receipt", type=pathlib.Path, default=None)
    arguments = parser.parse_args()
    if arguments.list:
        for scenario in ORDER:
            print(f"{scenario}\t{getattr(Workspaces, SCENARIOS[scenario]).__doc__.strip().splitlines()[0]}")
        return 0
    if not arguments.kit:
        parser.error("--kit is required")
    prefix = pathlib.Path(arguments.prefix).expanduser().resolve()
    arguments.marsh = str(prefix / "bin" / "marsh")
    arguments.guest_artifacts = prefix / "libexec" / "marsh"
    if not os.access(arguments.marsh, os.X_OK):
        print(f"workspaces: no installed product at {arguments.marsh} (run `make dev`)")
        return 1
    kit = pathlib.Path(arguments.kit).expanduser()
    if kit.is_file():
        arguments.kit = json.loads(kit.read_text())["fixture"]
    if arguments.source_revision is None:
        arguments.source_revision = subprocess.run(
            ["git", "-C", arguments.source_tree, "rev-parse", "HEAD"], capture_output=True, text=True,
            timeout=30, check=True).stdout.strip()
    if arguments.evidence is None:
        arguments.evidence = f"/private/tmp/marsh-dev-workspaces-{os.getuid()}"
    harness = Workspaces(arguments)
    try:
        harness.run_all()
    except KeyboardInterrupt:
        return harness.finish(InterruptedError("workspaces acceptance interrupted"))
    except Exception as error:
        return harness.finish(error)
    return harness.finish(None)


if __name__ == "__main__":
    raise SystemExit(main())
