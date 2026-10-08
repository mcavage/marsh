#!/usr/bin/env python3
"""`make check`: a fast real smoke test of the installed dev product.

Drives ~/.marsh-dev/bin/marsh (or --prefix) against real stock `sbx` in one
persistent isolated scope at target/check/{home,control,project}. The scope's
daemon and VMs stay warm between runs; when the installed daemon or guest
binaries change, the scope is stopped (`marsh stop`) first. `--reset` stops
the scope and deletes target/check. No fakes: every check observes public
outputs of the real product and stock SBX.
"""
from __future__ import annotations

import argparse
import concurrent.futures
import fcntl
import hashlib
import json
import os
import pathlib
import pty
import re
import select
import shlex
import shutil
import struct
import subprocess
import sys
import termios
import threading
import time
import traceback
import uuid
from typing import Any, Callable

ACCEPTANCE = pathlib.Path(__file__).resolve().parent / "acceptance"
sys.path.insert(0, str(ACCEPTANCE))

from run import product_owned_vm_names, resolve_executable, verify_container_deleted  # noqa: E402
from provenance import stock_vm_inventory  # noqa: E402
from mcp_gateway_uat import gateway_call, gateway_data, wait_until  # noqa: E402

CHECKOUT = pathlib.Path(__file__).resolve().parents[1]
ROOT = CHECKOUT / "target" / "check"
WRITABLE_LIMIT = 32 * 1024 * 1024
# Binaries whose change makes a warm scope stale (daemon and what it pushes
# into VMs). A change stops the scope before checks run.
STAMPED = ["bin/marshd", "libexec/marsh/marsh-linux-arm64", "libexec/marsh/marsh-worker-linux-arm64",
           "libexec/marsh/marsh-relay-linux-arm64", "libexec/marsh/marshd-linux-arm64",
           "libexec/marsh/marsh-local-linux-arm64", "libexec/marsh/shell-image"]


def digest(path: pathlib.Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            hasher.update(block)
    return hasher.hexdigest()


class Failure(AssertionError):
    pass


def require(condition: object, message: str) -> None:
    if not condition:
        raise Failure(message)


def text(value: bytes) -> str:
    return value.decode(errors="replace")


class Scope:
    def __init__(self, args: argparse.Namespace) -> None:
        self.prefix = pathlib.Path(args.prefix).expanduser().resolve(strict=True)
        self.marsh = str(self.prefix / "bin" / "marsh")
        self.guest = self.prefix / "libexec" / "marsh"
        self.sbx = resolve_executable(args.sbx)
        self.kit = args.kit
        self.root = ROOT
        self.home = ROOT / "home"
        self.control_root = ROOT / "control"
        self.project = ROOT / "project"
        self.stamp_path = ROOT / "stamp.json"
        self.environment = os.environ.copy()
        for key in [key for key in self.environment if key.startswith("MARSH_")]:
            del self.environment[key]
        self.environment.update({
            "MARSH_HOME": str(self.home),
            "MARSH_CONTROL_HOME": str(self.control_root),
            "MARSH_SBX": self.sbx,
            "MARSH_GUEST_ARTIFACTS": str(self.guest),
            "MARSH_JOB_WRITABLE_BYTES": str(WRITABLE_LIMIT),
        })
        self.records: list[dict[str, Any]] = []
        self.lock = threading.Lock()

    @property
    def control_home(self) -> pathlib.Path:
        scope = hashlib.sha256(os.fsencode(self.home.resolve(strict=True))).hexdigest()
        return self.control_root / scope

    def stamp(self) -> dict[str, str]:
        return {name: digest(self.prefix / name) for name in STAMPED if (self.prefix / name).exists()}

    def run(self, argv: list[str], *, stdin: bytes = b"", timeout: float = 60,
            cwd: pathlib.Path | None = None, check: bool = False) -> subprocess.CompletedProcess[bytes]:
        started = time.monotonic()
        completed = subprocess.run(argv, cwd=cwd or self.project, env=self.environment, input=stdin,
                                   capture_output=True, timeout=timeout, check=False)
        with self.lock:
            self.records.append({"argv": argv, "status": completed.returncode,
                                 "ms": round((time.monotonic() - started) * 1000),
                                 "stdout": text(completed.stdout)[-2000:],
                                 "stderr": text(completed.stderr)[-2000:]})
        if check and completed.returncode != 0:
            raise Failure(f"{shlex.join(argv)} exited {completed.returncode}: {text(completed.stderr)[-800:]}")
        return completed

    def marsh_run(self, *args: str, **kwargs: Any) -> subprocess.CompletedProcess[bytes]:
        return self.run([self.marsh, *args], **kwargs)

    def document(self, *args: str) -> Any:
        return json.loads(self.marsh_run(*args, check=True).stdout)

    def status(self) -> dict[str, Any]:
        return self.document("status", "--json")

    # Scope lifecycle -------------------------------------------------------

    def daemon_running(self) -> bool:
        # `marsh status` starts a daemon; only the endpoint shows a live one.
        if not self.home.exists():
            return False
        canonical = self.home.resolve(strict=True)
        scope = hashlib.sha256(os.fsencode(canonical)).hexdigest()[:16]
        return (pathlib.Path("/tmp") / f"marsh-{os.getuid()}" / scope / "s").exists()

    def stop(self) -> list[str]:
        """`marsh stop` for this scope, then prove its VMs are gone from stock."""
        errors: list[str] = []
        if not self.home.exists():
            return errors
        owned = product_owned_vm_names(self.control_home) if self.control_home.exists() else set()
        if self.daemon_running() or owned:
            stopped = self.marsh_run("stop", timeout=300)
            if stopped.returncode != 0:
                errors.append(f"marsh stop exited {stopped.returncode}: {text(stopped.stderr)[-800:]}")
        remaining = sorted(owned & set(stock_vm_inventory(self.sbx)))
        if remaining:
            errors.append(f"scope VMs remain after marsh stop: {remaining}")
        return errors

    def reset(self) -> list[str]:
        errors = self.stop()
        if not errors and self.root.exists():
            shutil.rmtree(self.root)
        return errors

    def prepare(self) -> dict[str, Any]:
        """Create or reuse the persistent scope; stop it when binaries changed."""
        info: dict[str, Any] = {"reused": self.root.exists()}
        current = self.stamp()
        if self.root.exists():
            previous = json.loads(self.stamp_path.read_text()) if self.stamp_path.exists() else {}
            if previous != current:
                info["restarted_for_changed_binaries"] = sorted(
                    name for name in set(previous) | set(current) if previous.get(name) != current.get(name))
                errors = self.stop()
                require(not errors, f"could not stop stale scope: {errors}")
        for path in (self.root, self.home, self.home / "home", self.control_root, self.project):
            path.mkdir(mode=0o700, parents=True, exist_ok=True)
            path.chmod(0o700)
        self.control_home.mkdir(mode=0o700, exist_ok=True)
        commands = {"fixture": self.kit, "fixture-acp": str(ACCEPTANCE / "acp-fixture")}
        agents = [{"schema_version": 1, "name": "fixture-session", "protocol": "acp_v1",
                   "command": "fixture-acp", "required_capabilities": []}]
        for name, value in (("commands.json", commands), ("agents.json", agents)):
            path = self.control_home / name
            content = json.dumps(value, sort_keys=True) + "\n"
            if not path.exists() or path.read_text() != content:
                if self.daemon_running():
                    # The daemon freezes its registry at startup.
                    errors = self.stop()
                    require(not errors, f"could not stop scope for a registry change: {errors}")
                path.write_text(content)
        self.stamp_path.write_text(json.dumps(current, sort_keys=True) + "\n")
        self.prepare_project()
        return info

    def git(self, *args: str) -> str:
        return text(subprocess.run(["git", "-C", str(self.project), *args], check=True,
                                   capture_output=True, timeout=30).stdout)

    NOTES = b"alpha\nbeta\n"

    def prepare_project(self) -> None:
        if not (self.project / ".git").exists():
            subprocess.run(["git", "init", "-q", str(self.project)], check=True, timeout=30)
            self.git("config", "user.email", "check@marsh.invalid")
            self.git("config", "user.name", "marsh check")
            (self.project / ".git" / "info" / "exclude").write_text(".marsh/\n")
            (self.project / "notes.txt").write_bytes(self.NOTES)
            self.git("add", "notes.txt")
            self.git("commit", "-q", "-m", "notes")
        # Each run starts from the committed tree.
        self.git("checkout", "-q", "--", ".")
        self.git("clean", "-q", "-fdx", "-e", ".marsh/")
        shutil.rmtree(self.project / ".marsh", ignore_errors=True)
        require(self.git("status", "--porcelain") == "", "check project is not clean")

    def tree_state(self) -> tuple[str, str, bytes]:
        return (self.git("rev-parse", "HEAD"), self.git("status", "--porcelain"),
                (self.project / "notes.txt").read_bytes())

    # Receipts ----------------------------------------------------------------

    def show(self, job_id: str) -> dict[str, Any]:
        return self.document("jobs", "show", job_id, "--json")

    def tree_jobs(self) -> list[dict[str, Any]]:
        flat: list[dict[str, Any]] = []

        def walk(nodes: list[dict[str, Any]], parent: str | None) -> None:
            for node in nodes:
                node = {**node, "_tree_parent": parent}
                flat.append(node)
                walk(node.get("children", []), node.get("job_id"))
        walk(self.document("jobs", "--tree", "--json")["jobs"], None)
        return flat

    def marked(self, marker: str, *, count: int, settle: float = 15) -> list[dict[str, Any]]:
        """Terminal tree nodes whose argv carries this check's unique marker."""
        deadline = time.monotonic() + settle
        while True:
            nodes = [node for node in self.tree_jobs() if node.get("job_id") and marker in node.get("args", [])]
            if len(nodes) >= count and all(node.get("state") != "running" for node in nodes):
                return nodes
            if time.monotonic() > deadline:
                raise Failure(f"expected {count} terminal jobs marked {marker}, saw "
                              f"{[(n.get('args'), n.get('state')) for n in nodes]}")
            time.sleep(0.2)


# Checks ----------------------------------------------------------------------
# Each returns evidence; a raised exception is the failure.

def check_bytes(scope: Scope, run_id: str) -> dict[str, Any]:
    stdin = b"stdin\x00bytes\xff\n"
    completed = scope.marsh_run("-c", "cat; printf 'err\\0\\377' >&2; exit 23", stdin=stdin)
    require(completed.returncode == 23, f"exit {completed.returncode}, expected 23")
    require(completed.stdout == stdin, f"stdout not byte-identical: {completed.stdout!r}")
    require(completed.stderr == b"err\x00\xff", f"stderr not byte-identical: {completed.stderr!r}")
    return {"exit": 23}


def check_job_receipt(scope: Scope, run_id: str) -> dict[str, Any]:
    marker = f"chk-b-{run_id}"
    completed = scope.marsh_run("-c", f"fixture identity {marker}")
    require(completed.returncode == 0, f"fixture identity exited {completed.returncode}: {text(completed.stderr)}")
    identity = json.loads(completed.stdout)
    require(identity["cwd"] == str(scope.project), f"job cwd is not the natural project path: {identity}")
    (node,) = scope.marked(marker, count=1)
    receipt = scope.show(node["job_id"])
    require(receipt["state"] == "finished" and receipt["exit"].get("code") == 0, f"receipt state: {receipt['exit']}")
    require(receipt["cleanup"] == "verified", f"receipt cleanup is {receipt['cleanup']}")
    require(re.fullmatch(r"[0-9a-f]{64}", receipt.get("container_id") or ""), "receipt lacks a container id")
    verify_container_deleted(lambda argv: scope.run(argv, timeout=30, cwd=scope.root),
                             [scope.sbx, "exec", receipt["vm_id"]], receipt["container_id"])
    return {"job_id": receipt["job_id"], "vm_id": receipt["vm_id"], "container_id": receipt["container_id"],
            "cleanup": receipt["cleanup"]}


def split_render_ok(rendering: str, kit_branch: str) -> None:
    require(re.search(r"(?m)^== a \(exited 0, shell-vm\) ==\nx\n-- a: no changes --$", rendering),
            f"branch a rendering wrong: {rendering[-1500:]!r}")
    require(re.search(rf"(?m)^== b \(exited 0, {re.escape(kit_branch)}\) ==$", rendering),
            f"branch b header wrong: {rendering[-1500:]!r}")
    b = rendering.split("== b (", 1)[1]
    require("-- b: 1 file (M notes.txt)" in b and "\n-alpha\n-beta\n+fixed\n" in b,
            f"branch b diff lacks the change: {b[-1500:]!r}")


def check_split_cli(scope: Scope, run_id: str) -> dict[str, Any]:
    before = scope.tree_state()
    split = subprocess.Popen([scope.marsh, "split", "-n", "-b", "a=echo x", ":::", "b", "fixture", "project-write",
                              "notes.txt", "fixed"], cwd=scope.project, env=scope.environment,
                             stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        joined = subprocess.run([scope.marsh, "join"], cwd=scope.project, env=scope.environment, stdin=split.stdout,
                                capture_output=True, timeout=60)
    finally:
        assert split.stdout is not None
        split.stdout.close()
        split_err = split.stderr.read() if split.stderr else b""
        split.wait(timeout=60)
    require(split.returncode == 0, f"marsh split exited {split.returncode}: {text(split_err)}")
    require(joined.returncode == 0, f"marsh join exited {joined.returncode}: {text(joined.stderr)}")
    split_render_ok(text(joined.stdout), "kit:fixture")
    require(scope.tree_state() == before, "split changed the user's tree")
    return {"tree": before[0].strip()}


def check_split_brush(scope: Scope, run_id: str) -> dict[str, Any]:
    before = scope.tree_state()
    completed = scope.marsh_run("-c", "split { a: echo x; b: fixture project-write notes.txt fixed; } | join")
    require(completed.returncode == 0, f"split {{}} | join exited {completed.returncode}: {text(completed.stderr)}")
    split_render_ok(text(completed.stdout), "shell-vm")
    require(scope.tree_state() == before, "split changed the user's tree")
    return {"tree": before[0].strip()}


def check_fanout(scope: Scope, run_id: str) -> dict[str, Any]:
    marker = f"chk-e-{run_id}"
    fanout = subprocess.Popen([scope.marsh, "fanout", "-n", ":::", "a", "fixture", "identity", marker,
                               ":::", "b", "fixture", "identity", marker], cwd=scope.project, env=scope.environment,
                              stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        collected = subprocess.run([scope.marsh, "collect"], cwd=scope.project, env=scope.environment,
                                   stdin=fanout.stdout, capture_output=True, timeout=60)
    finally:
        assert fanout.stdout is not None
        fanout.stdout.close()
        fanout_err = fanout.stderr.read() if fanout.stderr else b""
        fanout.wait(timeout=60)
    require(fanout.returncode == 0, f"fanout exited {fanout.returncode}: {text(fanout_err)}")
    require(collected.returncode == 0, f"collect exited {collected.returncode}: {text(collected.stderr)}")
    output = text(collected.stdout)
    sections = re.findall(r"(?m)^== (\w+) \(complete\) ==\n(\{[^\n]*\})$", output)
    require([label for label, _ in sections] == ["a", "b"], f"collect output not ordered a, b: {output!r}")
    for _, body in sections:
        require(json.loads(body)["cwd"] == str(scope.project), f"branch ran elsewhere: {body}")
    nodes = scope.marked(marker, count=2)
    require(len(nodes) == 2 and all(node.get("exit_code") == 0 for node in nodes), f"fanout jobs: {nodes}")
    return {"jobs": [node["job_id"] for node in nodes]}


def check_nested(scope: Scope, run_id: str) -> dict[str, Any]:
    marker = f"chk-f-{run_id}"
    # The parent job runs the registered name `fixture` itself (argv, no shell).
    completed = scope.marsh_run("-c", shlex.join(["fixture", "pipeline", "|", "fixture", "identity", marker]))
    require(completed.returncode == 0, f"nested run exited {completed.returncode}: {text(completed.stderr)}")
    require(json.loads(completed.stdout.splitlines()[0])["cwd"] == str(scope.project), "child cwd is not natural")
    nodes = {node["args"][0]: node for node in scope.marked(marker, count=2)}
    require(set(nodes) == {"pipeline", "identity"}, f"unexpected marked jobs: {list(nodes)}")
    parent, child = nodes["pipeline"], nodes["identity"]
    require(child["_tree_parent"] == parent["job_id"], "child is not drawn under its parent in jobs --tree --json")
    require(child["lineage"]["parent"] == f"job:{parent['job_id']}" and child["lineage"]["root"] == parent["job_id"]
            and child["lineage"]["depth"] == parent["lineage"]["depth"] + 1, f"child lineage: {child['lineage']}")
    require(parent["cleanup"] == child["cleanup"] == "verified", "nested cleanup not verified")
    return {"parent": parent["job_id"], "child": child["job_id"]}


def check_ctrl_c(scope: Scope, run_id: str) -> dict[str, Any]:
    marker = f"chk-g-{run_id}"
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))

    def controlling() -> None:
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    process = subprocess.Popen([scope.marsh, "-c", f"fixture hold 60 {marker}"], cwd=scope.project,
                               env=scope.environment, stdin=slave, stdout=slave, stderr=slave,
                               preexec_fn=controlling)
    os.close(slave)
    output = bytearray()
    try:
        deadline = time.monotonic() + 20
        while b"READY" not in output and time.monotonic() < deadline and process.poll() is None:
            ready, _, _ = select.select([master], [], [], 0.1)
            if ready:
                try:
                    output.extend(os.read(master, 4096))
                except OSError:
                    break
        require(b"READY" in output, f"held job did not start: {bytes(output)!r}")
        os.write(master, b"\x03")  # the user's Ctrl-C through the terminal line discipline
        pressed = time.monotonic()
        status = process.wait(timeout=15)
        interrupt_s = time.monotonic() - pressed
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        os.close(master)
    require(status == 130, f"Ctrl-C returned {status}, expected 130")
    (node,) = scope.marked(marker, count=1)
    receipt = scope.show(node["job_id"])
    require(receipt["state"] == "cancelled", f"job recorded {receipt['state']}, expected cancelled")
    require(receipt["cleanup"] == "verified", f"cancelled job cleanup {receipt['cleanup']}")
    return {"job_id": receipt["job_id"], "cause": receipt["exit"].get("cause"), "interrupt_s": round(interrupt_s, 2)}


def check_writable(scope: Scope, run_id: str) -> dict[str, Any]:
    marker = f"chk-h-{run_id}"
    started = time.monotonic()
    completed = scope.marsh_run("-c", f"fixture writable-pressure {marker}", timeout=30)
    elapsed = time.monotonic() - started
    require(completed.returncode != 0, "writable pressure was not stopped")
    require(elapsed < 5, f"writable limit took {elapsed:.1f}s")
    (node,) = scope.marked(marker, count=1)
    receipt = scope.show(node["job_id"])
    require(receipt["exit"].get("cause") == "limit:writable", f"cause {receipt['exit']}")
    require(receipt["cleanup"] == "verified", f"cleanup {receipt['cleanup']}")
    return {"job_id": receipt["job_id"], "seconds": round(elapsed, 2), "limit_bytes": WRITABLE_LIMIT,
            "exit": completed.returncode}


def check_mcp(scope: Scope, run_id: str) -> dict[str, Any]:
    tool = f"chk_{run_id}"
    published = False
    try:
        completed = scope.marsh_run("-c", f"mcp publish {tool} --kit fixture -- 'cat | tr a-z A-Z'", timeout=120)
        require(completed.returncode == 0, f"mcp publish exited {completed.returncode}: {text(completed.stderr)}")
        published = True
        lines = dict(line.split(": ", 1) for line in text(completed.stdout).splitlines() if ": " in line)
        vm = lines.get("Loaded into sandbox")
        require(vm and vm in product_owned_vm_names(scope.control_home), f"publish target is not a scope Kit VM: {vm}")

        def call(value: str) -> dict:
            return gateway_call(scope.sbx, vm, tool, value, cwd=scope.root, env=scope.environment, timeout=60)

        probe = wait_until(lambda: (lambda p: p if tool in p["tools"] else None)(call("check\n")),
                           "Kit VM gateway discovery", 20)
        data = gateway_data(probe, tool, b"CHECK\n")
        unpublished = scope.marsh_run("-c", f"mcp unpublish {tool}")
        require(unpublished.returncode == 0, f"mcp unpublish exited {unpublished.returncode}")
        published = False
        denied = call("again\n")["call"]
        result = (denied or {}).get("result") or {}
        require("error" in (denied or {}) or result.get("isError") is True
                or result.get("structuredContent", {}).get("data", {}).get("outcome") != "success",
                f"unpublished tool still runs: {denied}")
        return {"tool": tool, "sandbox": vm, "cleanup_certainty": data.get("cleanup_certainty")}
    finally:
        if published:
            scope.marsh_run("-c", f"mcp unpublish {tool}")


ACP_SCRIPT = r"""
id=$(acp reserve fixture-session) || exit 30
acp run --reservation "$id" fixture-session >/dev/null 2>&1 &
run_pid=$!
acp list --mine --wait "$id" >/dev/null || exit 31
acp stop "$id" >/dev/null || exit 44
wait "$run_pid"
printf 'wait=%s
' "$?"
acp status "$id" --json
"""


def check_acp(scope: Scope, run_id: str) -> dict[str, Any]:
    completed = scope.marsh_run("-c", ACP_SCRIPT, timeout=900)
    require(completed.returncode == 0, f"ACP session script exited {completed.returncode}: {text(completed.stderr)[-1500:]}")
    first, rest = text(completed.stdout).split("\n", 1)
    wait_status = int(first.removeprefix("wait="))
    final = json.loads(rest)
    receipt = final.get("receipt") or {}
    require(receipt.get("cleanup") == "verified", f"ACP job cleanup: {receipt.get('cleanup')}")
    code = (receipt.get("exit") or {}).get("code")
    require(wait_status == (code if code is not None else 125), f"wait {wait_status} vs receipt exit {code}")
    return {"agent_session_id": final.get("agent_session_id"), "job_id": receipt.get("job_id"),
            "wait": wait_status, "cause": (receipt.get("exit") or {}).get("cause")}


CHECKS: list[tuple[str, Callable[[Scope, str], dict[str, Any]]]] = [
    ("bytes", check_bytes),
    ("job-receipt-and-deletion", check_job_receipt),
    ("split-cli-join", check_split_cli),
    ("split-brush-join", check_split_brush),
    ("fanout-collect", check_fanout),
    ("nested-job-tree", check_nested),
    ("ctrl-c-cancel", check_ctrl_c),
    ("writable-limit", check_writable),
    ("mcp-publish-gateway", check_mcp),
    ("acp-session", check_acp),
]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prefix", default="~/.marsh-dev")
    parser.add_argument("--sbx", default=os.environ.get("MARSH_SBX", "sbx"))
    parser.add_argument("--kit", default="", help="immutable fixture Kit OCI reference")
    parser.add_argument("--reset", action="store_true", help="stop the scope and delete target/check")
    parser.add_argument("--jobs", type=int, default=5, help="checks run at once")
    parser.add_argument("--only", action="append", default=[])
    args = parser.parse_args()
    scope = Scope(args)
    if args.reset:
        errors = scope.reset()
        for error in errors:
            print(f"check-reset: {error}", file=sys.stderr)
        if not errors:
            print(f"check-reset: stopped the scope and removed {ROOT}")
        return 1 if errors else 0
    if not re.fullmatch(r"[a-z0-9][a-z0-9._:/-]*@sha256:[0-9a-f]{64}", args.kit):
        parser.error("--kit must be an immutable fixture Kit OCI reference (make fixture-ref, or DEV_KIT=...)")
    wall = time.monotonic()
    run_id = uuid.uuid4().hex[:8]
    report: dict[str, Any] = {"schema": "marsh.check/v1", "run_id": run_id, "prefix": str(scope.prefix),
                              "kit": args.kit, "started": time.time(), "checks": {}}
    failed = False
    try:
        report["scope"] = scope.prepare()
        if report["scope"].get("restarted_for_changed_binaries"):
            print("check: installed binaries changed; stopped the warm scope")
        started = time.monotonic()
        report["status_before"] = {key: value for key, value in scope.status().items()
                                   if key in ("daemon_id", "scope_id", "workers")}
        # Boot the shell and fixture VMs first (a no-op when warm), so the
        # stock boot notices never interleave with a check's exact stderr.
        warmed = scope.marsh_run("--load", "fixture", "-c", "true", timeout=900)
        require(warmed.returncode == 0, f"warm-up exited {warmed.returncode}: {text(warmed.stderr)[-800:]}")
        report["prepare_s"] = round(time.monotonic() - started, 2)
        print(f"prepare {report['prepare_s']:.2f}s", flush=True)
        selected = [(name, fn) for name, fn in CHECKS if not args.only or name in args.only]

        def timed(name: str, fn: Callable[[Scope, str], dict[str, Any]]) -> dict[str, Any]:
            begun = time.monotonic()
            try:
                evidence = fn(scope, run_id)
                return {"outcome": "PASS", "seconds": round(time.monotonic() - begun, 2), "evidence": evidence}
            except Exception as error:  # noqa: BLE001 - every failure is reported, not raised
                return {"outcome": "FAIL", "seconds": round(time.monotonic() - begun, 2),
                        "error": f"{type(error).__name__}: {error}", "traceback": traceback.format_exc()}

        with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
            futures = {name: pool.submit(timed, name, fn) for name, fn in selected}
            for name, _ in selected:
                result = futures[name].result()
                report["checks"][name] = result
                line = f"{result['outcome']} {name:<26} {result['seconds']:6.2f}s"
                if result["outcome"] != "PASS":
                    failed = True
                    line += f"  {result['error'][:400]}"
                print(line, flush=True)
        scope.prepare_project()
    except Exception as error:  # noqa: BLE001
        failed = True
        report["error"] = traceback.format_exc()
        print(f"FAIL setup: {error}", flush=True)
    report["wall_s"] = round(time.monotonic() - wall, 2)
    report["outcome"] = "failed" if failed else "passed"
    report["commands"] = scope.records
    ROOT.mkdir(mode=0o700, parents=True, exist_ok=True)
    evidence = ROOT / "last.json"
    evidence.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    evidence.chmod(0o600)
    print(f"{'FAILED' if failed else 'PASSED'} {sum(1 for c in report['checks'].values() if c['outcome'] == 'PASS')}"
          f"/{len(report['checks'])} checks in {report['wall_s']:.1f}s; evidence {evidence}")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
