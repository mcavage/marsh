#!/usr/bin/env python3
"""Live checks for services snapshots in checkpoints (C12), against real sbx.

The product driver: a real `marshd` (the dev install's) and the real `marsh
checkpoint` / `checkpoints` / `services` CLI. Stack A (H1b's `stack-a` writer, the
spike's Postgres) runs in the project's own services VM; with no job endpoint yet
(E2), the containers are made on the engine's production socket with the scope
labels the filter will put on them (`dev.marsh.scope=<workspace>`).

Two sessions, each its own scope and VMs:

  normal   the normal build: CAP-01, 02, 04, 05, 07, 08 (and 06 reuse), PERF-01/02
  breaks   the test-breaks build: each break at its check (the daemon is relaunched per cell,
           the VM stays); the in-VM agent takes its modes only from the daemon.

Run under the VM lock (this script holds none): shared for the functional scenarios, exclusive
for `perf` and the break matrix. Every sandbox it creates is removed through `marsh services rm`,
then by exact name after its recorded UUID is verified; daemons are signalled by exact pid only;
the run's files live in one mkdtemp directory under /private/tmp. Test daemons use
MARSH_VM_PREFIX=marsh-xnfx01-.

Every check prints `PASS name: detail` or `FAIL name: detail`.
"""
from __future__ import annotations

import argparse
import functools
import hashlib
import json
import os
import random
import re
import shlex
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

FAILURES: list[str] = []
OWNED: dict[str, str] = {}  # sandbox name -> uuid
SOCK = "/run/marsh-svc/docker.sock"
APP_IMAGE = "postgres:17"
SHARED_NOTE = ""


def say(status: str, name: str, detail: str = "") -> None:
    print(f"{status} {name}: {detail}", flush=True)
    if status == "FAIL":
        FAILURES.append(name)


def check(name: str, condition: bool, detail: str = "") -> bool:
    say("PASS" if condition else "FAIL", name, detail)
    return condition


def run(argv: list[str], timeout: int = 300, **kw) -> subprocess.CompletedProcess:
    return subprocess.run(argv, capture_output=True, text=True, timeout=timeout, **kw)


def inventory() -> dict[str, dict]:
    result = run(["sbx", "ls", "--json"])
    if result.returncode:
        raise RuntimeError(f"sbx ls failed: {result.stderr.strip()}")
    doc = json.loads(result.stdout or "[]")
    rows = doc.get("sandboxes", doc) if isinstance(doc, dict) else doc
    return {row["name"]: row for row in rows}


def verify_owned(vm: str, listing=None) -> None:
    """Never exec into a sandbox that is not the one the daemon recorded (AGENTS.md): called immediately before
    EVERY direct `sbx exec`, it reads the live inventory (no cache) and requires the recorded UUID and a running
    VM. `listing` is for the self-test only."""
    uuid = OWNED.get(vm)
    if uuid is None:
        raise RuntimeError(f"{vm} is not a sandbox this run recorded")
    row = (listing or inventory)().get(vm)
    if row is None or row["id"] != uuid:
        raise RuntimeError(f"{vm} is not the sandbox the daemon recorded (live: {row and row['id']}, recorded: {uuid}); not executing in it")
    if row.get("status") != "running":
        # `sbx exec` starts a stopped sandbox: this harness never wakes a VM it is observing.
        raise RuntimeError(f"{vm} is {row.get('status')}, not running; not executing in it (that would start it)")


def vx(vm: str, script: str, check_rc: bool = False, timeout: int = 300) -> str:
    """A root shell in the owned VM. The real status travels in a trailer (sbx exec reports 255 after docker)."""
    verify_owned(vm)
    wrapped = script + '\necho "@@rc=$?"'
    result = run(["sbx", "exec", "-u", "root", vm, "sh", "-c", wrapped], timeout)
    out = result.stdout
    m = re.search(r"@@rc=(\d+)\n?$", out)
    rc = int(m.group(1)) if m else result.returncode
    if m:
        out = out[: m.start()]
    if check_rc and rc:
        raise RuntimeError(f"{script[:160]}: rc={rc} {result.stderr.strip()[-300:]} {out.strip()[-200:]}")
    return out.strip()


def prod(vm: str, *args: str, check_rc: bool = True, timeout: int = 300, merge: bool = False) -> str:
    return vx(vm, shlex.join(["docker", "exec", "marsh-svc-engine", "docker", "-H", f"unix://{SOCK}", *args]) + (" 2>&1" if merge else ""), check_rc, timeout)


def eng(vm: str, script: str, check_rc: bool = False, timeout: int = 300) -> str:
    return vx(vm, shlex.join(["docker", "exec", "marsh-svc-engine", "sh", "-c", script]), check_rc, timeout)


def record_vm(name: str, uuid: str) -> None:
    """Record a VM by the name and UUID the daemon's ownership map reports (`services status`), and
    require the live inventory to agree before anything is executed in it."""
    row = inventory().get(name)
    if row is None:
        raise RuntimeError(f"{name} is not listed")
    if row["id"] != uuid:
        raise RuntimeError(f"{name}: the daemon recorded {uuid} but the inventory holds {row['id']}")
    previous = OWNED.setdefault(name, uuid)
    if previous != uuid:
        raise RuntimeError(f"{name} changed identity")
    say("INFO", "owned-sandbox", f"{name} uuid={uuid}")


def remove_owned() -> bool:
    ok = True
    for name, uuid in list(OWNED.items()):
        row = inventory().get(name)
        if row is None:
            OWNED.pop(name)
            continue
        if row["id"] != uuid:
            say("FAIL", f"cleanup/{name}", f"a different sandbox ({row['id']}) holds our name; not removing it")
            ok = False
            continue
        result = run(["sbx", "rm", "--force", name], 300)
        if result.returncode or name in inventory():
            say("FAIL", f"cleanup/{name}", (result.stderr or result.stdout).strip()[:200])
            ok = False
        else:
            say("INFO", f"cleanup/{name}", f"removed uuid {uuid}")
            OWNED.pop(name)
    return ok


def pct(values: list[float], q: float) -> float:
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(round(q * (len(ordered) - 1))))]


class Session:
    """One scope (home), its projects, and the daemon launched for it."""

    def __init__(self, args, build: str):
        self.args = args
        self.build = build
        self.prefix = Path(args.dev_breaks if build == "breaks" else args.dev_normal)
        self.root = Path(tempfile.mkdtemp(prefix="nfx-c12-", dir="/private/tmp")).resolve()
        try:
            for name in ("mh", "mc"):
                (self.root / name).mkdir()
                (self.root / name).chmod(0o700)
        except BaseException:
            shutil.rmtree(self.root, ignore_errors=True)  # our own mkdtemp directory, created a moment ago
            raise
        self.projects: dict[str, Path] = {}
        self.proc: subprocess.Popen | None = None
        self.log = None
        self.env_extra: dict[str, str] = {}
        self.reserve_bytes = 1 << 30
        self.breaks: tuple[str, ...] = ()
        say("INFO", "run-root", f"{self.root} ({build})")

    def install_kit(self) -> None:
        """Register the H1b `nfx` fixture Kit with this scope (as E2's harness does), so a real `marsh run nfx exec`
        job can reach the project's filtered Docker endpoint. Before the daemon starts."""
        stage = Path(self.args.fixtures) / "nfx-kit" / "stage.py"
        done = run([sys.executable, str(stage), "--dest", str(self.root / "kit")])
        if done.returncode:
            raise RuntimeError(f"staging the nfx Kit failed: {done.stderr.strip()[-300:]}")
        control_home = (self.root / "mc").resolve() / hashlib.sha256(os.fsencode((self.root / "mh").resolve())).hexdigest()
        control_home.mkdir(mode=0o700, exist_ok=True)
        shutil.copy(done.stdout.strip(), control_home / "commands.json")
        self.control_home = control_home

    def record_owned_map(self) -> None:
        """Record every VM the scope's ownership map holds (with its UUID, checked against the live inventory),
        so the cleanup removes them by exact name and UUID."""
        try:
            document = json.loads((self.control_home / "vm-ownership.json").read_text())
        except (OSError, ValueError, AttributeError):
            return
        live = inventory()
        for name, entry in (document.get("vms") or {}).items():
            uuid = entry.get("uuid") if isinstance(entry, dict) else None
            if uuid and name in live and live[name]["id"] == uuid:
                OWNED.setdefault(name, uuid)

    def project(self, name: str, with_fixture: bool = True) -> Path:
        path = self.root / name
        path.mkdir(exist_ok=True)
        (path / "README").write_text("c12 project\n")
        for index in range(200):
            (path / f"f{index}.txt").write_text(f"file {index}\n")
        if with_fixture:
            shutil.copytree(Path(self.args.fixtures) / "stack-a" / "app", path / "app", dirs_exist_ok=True)
        self.projects[name] = path
        return path

    # -- daemon
    def launch(self, breaks: tuple[str, ...] = (), env: dict[str, str] | None = None) -> None:
        self.breaks = breaks
        if env is not None:
            self.env_extra = dict(env)
        environment = {k: v for k, v in os.environ.items() if k != "MARSH_TEST_BREAK"}
        environment.update({
            "MARSH_HOME": str(self.root / "mh"), "MARSH_CONTROL_HOME": str(self.root / "mc"),
            "MARSH_VM_PREFIX": "marsh-xnfx01-", "MARSH_SERVICES_POOL_BYTES": self.args.pool,
            "MARSH_SERVICES_POOL_RESERVE_BYTES": str(self.reserve_bytes),
        })
        environment.update(self.env_extra)
        argv = [str(self.prefix / "bin" / "marshd"), "--home", str(self.root / "mh")]
        if breaks:
            argv.append("--test-breaks-from-env")
            environment["MARSH_TEST_BREAK"] = ",".join(breaks)
        self.env = {k: v for k, v in environment.items() if k != "MARSH_TEST_BREAK"}
        self.log = open(self.root / f"marshd-{time.monotonic_ns()}.log", "w")
        self.proc = subprocess.Popen(argv, stdout=self.log, stderr=subprocess.STDOUT, env=environment, cwd=next(iter(self.projects.values()), self.root))
        home = os.path.realpath(self.root / "mh")
        sock = Path(f"/tmp/marsh-{os.getuid()}/{hashlib.sha256(home.encode()).hexdigest()[:16]}/s")
        # A killed daemon leaves its socket file: wait until something accepts on it (never call
        # the CLI first, which would auto-start a daemon of its own), then require that this
        # process is the one that answers.
        import socket as _socket
        deadline = time.monotonic() + 90
        up = False
        while not up and time.monotonic() < deadline and self.proc.poll() is None:
            probe = _socket.socket(_socket.AF_UNIX)
            try:
                probe.connect(str(sock))
                up = True
            except OSError:
                time.sleep(0.3)
            finally:
                probe.close()
        status = json.loads(self.cli("status", "--json", cwd=self.root).stdout or "{}") if up else {}
        owner = (status.get("endpoint_owner") or {}).get("pid")
        check("marshd/started", up and owner == self.proc.pid, f"pid {self.proc.pid} answers={owner} breaks={list(breaks)}")
        if breaks:
            want = sorted(m for m in breaks if m in KNOWN_DAEMON_BREAKS)
            got = sorted(status.get("test_breaks", []))
            check("marshd/test-breaks-attested", got == want, f"daemon reports {got}, expected {want}")

    def cli(self, *args: str, cwd: Path | None = None, timeout: int = 900) -> subprocess.CompletedProcess:
        return run([str(self.prefix / "bin" / "marsh"), *args], timeout, env=self.env, cwd=cwd or next(iter(self.projects.values())))

    def kill(self) -> None:
        if self.proc and self.proc.poll() is None:
            os.kill(self.proc.pid, signal.SIGKILL)  # the exact pid this run started
            self.proc.wait(timeout=10)

    def stop(self) -> None:
        if self.proc and self.proc.poll() is None:
            self.cli("stop", cwd=self.root)
            try:
                self.proc.wait(timeout=90)
            except subprocess.TimeoutExpired:
                say("FAIL", "marshd/stop", f"pid {self.proc.pid} did not exit after `marsh stop`")

    def relaunch(self, breaks: tuple[str, ...] = (), env: dict[str, str] | None = None, kill: bool = True) -> None:
        # `marsh stop` would stop the services VM; killing the exact pid keeps it running, and the new daemon recovers its relay.
        if kill:
            self.kill()
        else:
            self.stop()
        self.launch(breaks, env if env is not None else self.env_extra)

    # -- services
    def svc(self, name: str) -> dict | None:
        result = self.cli("services", "status", "--json", cwd=self.projects[name])
        if result.returncode:
            return None
        root = os.path.realpath(self.projects[name])
        return next((p for p in json.loads(result.stdout)["projects"] if os.path.realpath(p["root"]) == root), None)

    def enable(self, name: str) -> None:
        r = self.cli("config", "services", "on", cwd=self.projects[name])
        check(f"setup/{name}-services-on", r.returncode == 0, (r.stdout + r.stderr).strip()[:160])

    def start_vm(self, name: str) -> str:
        r = self.cli("services", "start", "--json", cwd=self.projects[name], timeout=1800)
        check(f"setup/{name}-services-start", r.returncode == 0, (r.stdout + r.stderr).strip()[:200])
        doc = self.svc(name)
        vm = doc["vm"]
        record_vm(vm, doc["vm_uuid"])
        return vm

    def wait_state(self, name: str, state: str, seconds: int) -> dict | None:
        deadline = time.monotonic() + seconds
        doc = None
        while time.monotonic() < deadline:
            doc = self.svc(name)
            if doc and doc["state"] == state:
                return doc
            time.sleep(2)
        return doc

    def ckpt(self, name: str, label: str | None = None, lane: str | None = None) -> tuple[int, dict | None, float, str]:
        argv = ["checkpoint", "--json"]
        if label:
            argv += ["--label", label]
        if lane:
            argv += ["--lane", lane]
        started = time.monotonic()
        r = self.cli(*argv, cwd=self.projects[name])
        wall = time.monotonic() - started
        doc = None
        if r.returncode == 0 and r.stdout.strip():
            doc = json.loads(r.stdout)
        return r.returncode, doc, wall, (r.stdout + r.stderr).strip()

    def listed(self, name: str, label: str | None = None) -> list[dict]:
        argv = ["checkpoints", "--json"] + (["--label", label] if label else [])
        r = self.cli(*argv, cwd=self.projects[name])
        return json.loads(r.stdout)["checkpoints"] if r.returncode == 0 else []

    def rm_ckpt(self, name: str, ident: str) -> subprocess.CompletedProcess:
        return self.cli("checkpoints", "rm", ident, cwd=self.projects[name])

    def finish(self) -> bool:
        ok = True
        if self.proc and self.proc.poll() is None:
            for name in list(self.projects):
                doc = self.svc(name)
                if doc and doc.get("vm"):
                    r = self.cli("services", "rm", "--yes", cwd=self.projects[name], timeout=900)
                    say("INFO", f"cleanup/services-rm-{name}", f"rc={r.returncode}")
            self.stop()
        else:
            say("INFO", "cleanup", "daemon not running; VMs removed by exact name and UUID")
        ok = remove_owned() and ok
        if ok and self.root.parent == Path("/private/tmp") and self.root.name.startswith("nfx-c12-"):
            shutil.rmtree(self.root, ignore_errors=True)
        return ok


KNOWN_DAEMON_BREAKS = {
    "retention-keep-snapshots", "orphan-gc-off", "reuse-lease-off", "capture-admission-blocks", "orphan-roots-from-records",
    "lane-capture-root", "backend-services-disconnected", "capture-ensure-vm", "reuse-last-snapshot-off", "idle-final-capture-off",
    "admit-ignore-pool", "stop-fence-off", "stop-pause-off", "stop-base-only", "reuse-ignores-failure", "idle-ignores-inflight",
    "capture-gate-ignored", "journal-write-fail", "journal-marker-fail", "journal-crash-after-intent", "journal-intent-off",
    "journal-neither-ignored", "journal-vm-unbound", "journal-loss-silent", "lane-reconcile-off", "manifest-refs-unchecked",
    "hold-checkpoint-before-space-check", "hold-cutover-after-capture", "hold-cutover-after-count",
    "fault-engine-query", "engine-query-fail-open",
}


class Events:
    """Pause/unpause events of the production engine, with the engine's own timestamps."""

    def __init__(self, vm: str):
        verify_owned(vm)
        self.lines: list[tuple[str, str, int]] = []
        self.p = subprocess.Popen(
            ["sbx", "exec", "-u", "root", vm, "docker", "exec", "marsh-svc-engine", "timeout", "3000", "docker", "-H", f"unix://{SOCK}", "events",
             "--filter", "type=container", "--filter", "event=pause", "--filter", "event=unpause", "--format", "{{.Action}} {{.Actor.ID}} {{.TimeNano}}"],
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        threading.Thread(target=self._read, daemon=True).start()
        time.sleep(2)

    def _read(self) -> None:
        for line in self.p.stdout:
            a = line.split()
            if len(a) == 3:
                self.lines.append((a[0], a[1], int(a[2])))

    def mark(self) -> int:
        return len(self.lines)

    def since(self, mark: int, settle: float = 2.5) -> list[tuple[str, str, int]]:
        time.sleep(settle)
        return self.lines[mark:]

    def close(self) -> None:
        if self.p.poll() is None:
            self.p.terminate()  # our own child
            try:
                self.p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.p.kill()


def window_ms(events: list[tuple[str, str, int]], ids: set[str] | None = None) -> float | None:
    picked = [e for e in events if ids is None or e[1] in ids]
    pauses = [e[2] for e in picked if e[0] == "pause"]
    unpauses = [e[2] for e in picked if e[0] == "unpause"]
    if not pauses or not unpauses:
        return None
    return (max(unpauses) - min(pauses)) / 1e6


# ---------------------------------------------------------------- fixtures in the VM


def labels(ws: str, project: str | None = None) -> list[str]:
    out = ["--label", f"dev.marsh.scope={ws}", "--label", "dev.marsh.attempt=c12"]
    if project:
        out += ["--label", f"com.docker.compose.project={project}"]
    return out


def pg_count(vm: str, container: str, sql: str) -> str:
    return prod(vm, "exec", container, "psql", "-U", "postgres", "-d", "app", "-At", "-c", sql, check_rc=False).strip()


class Stack:
    """Stack A in workspace `ws`: db (published port, pgdata), app writer (uploads, tokens), and the
    standalone `keeper` holding the `notes` volume (`m` = n1)."""

    def __init__(self, vm: str, appdir: Path, ws: str = "base", project: str = "stacka", port: int = 15432):
        self.vm, self.appdir, self.ws, self.project, self.port = vm, appdir, ws, project, port
        self.load = True

    @property
    def db(self) -> str:
        return f"{self.project}-db"

    @property
    def app(self) -> str:
        return f"{self.project}-app"

    def up(self, load: bool = True) -> None:
        vm, ws, project = self.vm, self.ws, self.project
        self.load = load
        self.down()
        if APP_IMAGE.split(":")[0] not in prod(vm, "images", "--format", "{{.Repository}}"):
            prod(vm, "pull", "-q", APP_IMAGE, timeout=900)
        net = f"net-{project}"
        prod(vm, "network", "create", *labels(ws), net, check_rc=False)
        for v in ("pgdata", "uploads", "notes"):
            prod(vm, "volume", "create", *labels(ws, project), "--label", f"com.docker.compose.volume={v}", f"{project}_{v}")
        prod(vm, "run", "-d", "--name", self.db, "--network", net, "--network-alias", "db", *labels(ws, project),
             "--label", "com.docker.compose.service=db", "-e", "POSTGRES_PASSWORD=devpass", "-e", "POSTGRES_DB=app",
             "-p", f"127.0.0.1:{self.port}:5432", "-v", f"{project}_pgdata:/var/lib/postgresql/data", APP_IMAGE)
        for _ in range(120):
            if pg_count(vm, self.db, "select 1") == "1":
                break
            time.sleep(0.5)
        prod(vm, "run", "-d", "--name", self.app, "--network", net, *labels(ws, project), "--label", "com.docker.compose.service=app",
             "-e", "PGHOST=db", "-e", "PGUSER=postgres", "-e", "PGPASSWORD=devpass", "-e", "PGDATABASE=app",
             "-e", f"LOAD={'1' if load else '0'}", "-v", f"{project}_uploads:/uploads", "-v", f"{self.appdir}:/app:ro", APP_IMAGE, "bash", "/app/run.sh")
        prod(vm, "run", "-d", "--name", f"{project}-keeper", *labels(ws), "-v", f"{project}_notes:/notes", APP_IMAGE,
             "sh", "-c", "echo n1 > /notes/m; exec sleep infinity")
        deadline = time.time() + 180
        while time.time() < deadline:
            if (pg_count(vm, self.db, "select count(*) from items") or "0").isdigit() and int(pg_count(vm, self.db, "select count(*) from items") or 0) >= 1000:
                break
            time.sleep(1)
        else:
            raise RuntimeError("stack A was not seeded")

    def down(self) -> None:
        names = [f"{self.project}-db", f"{self.project}-app", f"{self.project}-keeper"]
        prod(self.vm, "rm", "-f", *names, check_rc=False)
        for v in ("pgdata", "uploads", "notes"):
            prod(self.vm, "volume", "rm", "-f", f"{self.project}_{v}", check_rc=False)
        prod(self.vm, "network", "rm", f"net-{self.project}", check_rc=False)

    def ids(self) -> dict[str, str]:
        return {n: prod(self.vm, "inspect", "-f", "{{.Id}}", f"{self.project}-{n}").strip() for n in ("db", "app", "keeper")}

    def tokens(self) -> int:
        return int(pg_count(self.vm, self.db, "select count(*) from tokens") or 0)

    def errors(self) -> int:
        out = prod(self.vm, "logs", self.app, check_rc=False)
        return len(re.findall(r"^ERROR", out, re.M))


def lane_writer(vm: str, ws: str, name: str, secret: str) -> str:
    prod(vm, "volume", "create", *labels(ws), f"{name}_data")
    prod(vm, "run", "-d", "--name", f"{name}-w", *labels(ws), "-v", f"{name}_data:/d", APP_IMAGE, "bash", "-c",
         f"echo {secret} > /d/secret; i=0; while :; do i=$((i+1)); echo $i > /d/n; sleep 0.1; done")
    return prod(vm, "inspect", "-f", "{{.Id}}", f"{name}-w").strip()


def lane_counter(vm: str, name: str) -> int:
    out = prod(vm, "exec", f"{name}-w", "cat", "/d/n", check_rc=False).strip()
    return int(out) if out.isdigit() else -1


def drop_lane(vm: str, name: str) -> None:
    prod(vm, "rm", "-f", f"{name}-w", check_rc=False)
    prod(vm, "volume", "rm", "-f", f"{name}_data", check_rc=False)


def pool_ids(vm: str) -> list[str]:
    """The published snapshots. An engine that cannot be reached (it restarts after a daemon relaunch) is an
    error, never an empty pool: "absent" must mean the listing worked and the snapshot was not in it."""
    out = eng(vm, "ls -1 /pool/snapshots 2>/dev/null; echo @@ok")
    if not out.rstrip().endswith("@@ok"):
        raise RuntimeError("the services engine could not be reached")
    return sorted(n for n in out.replace("@@ok", "").split() if not n.startswith(".") and not n.startswith("lost"))


def snapshot_present(vm: str, snap: str, wait: float = 90) -> bool:
    """Whether the snapshot is published, once the engine can be listed at all (it restarts after a relaunch)."""
    deadline = time.monotonic() + wait
    while True:
        try:
            return snap in pool_ids(vm)
        except RuntimeError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(2)


def pool_manifest(vm: str, snap: str) -> dict:
    return json.loads(eng(vm, f"cat /pool/snapshots/{snap}/manifest.json"))


def pool_used(vm: str) -> int:
    out = eng(vm, "df -B1 /pool | tail -n 1")
    return int(out.split()[2])


def pool_free(vm: str) -> int:
    out = eng(vm, "df -B1 /pool | tail -n 1")
    return int(out.split()[3])


def paused(vm: str) -> list[str]:
    return prod(vm, "ps", "-q", "--filter", "status=paused").split()


def live_set(sess: Session, name: str) -> set[str]:
    refs: set[str] = set()
    for c in sess.listed(name):
        s = c.get("services") or {}
        for key in ("snapshot", "reused_from"):
            if s.get(key):
                refs.add(s[key])
    last = (sess.svc(name) or {}).get("last_capture")
    if last:
        refs.add(last["id"])
    return refs


def wait_until(predicate, seconds: float, step: float = 1.0):
    """Poll until the predicate holds; a predicate that raises (the engine is restarting) is "not yet"."""
    deadline = time.monotonic() + seconds

    def attempt():
        try:
            return predicate()
        except Exception:  # noqa: BLE001
            return False

    value = attempt()
    while not value and time.monotonic() < deadline:
        time.sleep(step)
        value = attempt()
    return value


def _pg_on_copy(vm: str, source_dir: str, sql: str) -> int | None:
    """Run `sql` (one integer) on a throwaway Postgres over a reflink copy of `source_dir` inside the
    engine (own path under /pool, removed after)."""
    work = f"/pool/verify-{abs(hash(source_dir)) % 10**8}"
    eng(vm, f"rm -rf {work}; mkdir -p {work} && cp -a --reflink=auto {source_dir}/. {work}/")
    try:
        out = prod(vm, "run", "--rm", "-v", f"{work}:/var/lib/postgresql/data", APP_IMAGE, "bash", "-c",
                   "gosu postgres pg_ctl -D /var/lib/postgresql/data -w -o '-c listen_addresses=' start >/dev/null 2>&1; "
                   f"gosu postgres psql -d app -At -c \"{sql}\"; "
                   "gosu postgres pg_ctl -D /var/lib/postgresql/data -m immediate stop >/dev/null 2>&1", check_rc=False, timeout=300, merge=True)
        digits = [line.strip() for line in out.splitlines() if line.strip().isdigit()]
        if not digits:
            say("INFO", "pg_on_copy/raw", out[-400:].replace("\n", " | "))
        return int(digits[-1]) if digits else None
    finally:
        eng(vm, f"rm -rf {work}")


def snap_sql(vm: str, snap: str, sql: str) -> int | None:
    """An integer from the pgdata a snapshot holds (a direct read of the snapshot, not a restore)."""
    manifest = pool_manifest(vm, snap)
    vol = next((v for v in manifest["volumes"] if v["name"].endswith("pgdata") and v["captured"]), None)
    return None if vol is None else _pg_on_copy(vm, f"/pool/snapshots/{snap}/{vol['id']}", sql)


def live_sql(vm: str, volume: str, sql: str) -> int | None:
    """The same on the live volume's directory (the VM runs, the database container is not)."""
    mount = prod(vm, "volume", "inspect", "-f", "{{.Mountpoint}}", volume, check_rc=False).strip()
    return _pg_on_copy(vm, mount, sql) if mount.startswith("/") else None


def snap_rows(vm: str, snap: str, like: str) -> int | None:
    return snap_sql(vm, snap, f"select count(*) from markers where name like '{like}%'")


def snap_file(vm: str, snap: str, volume_suffix: str, name: str) -> str:
    man = pool_manifest(vm, snap)
    vol = next((v for v in man["volumes"] if v["name"].endswith(volume_suffix)), None)
    return eng(vm, f"cat /pool/snapshots/{snap}/{vol['id']}/{name} 2>/dev/null") if vol else ""


# ---------------------------------------------------------------- oracles (pure; `--self-test` runs them on synthetic inputs)

SECOND_CHECKPOINT_LIMIT_S = 1.0  # CAP-03 c3: the recorded result of the overlapped checkpoint
ORPHAN_COLLECTION_LIMIT_S = 60.0  # CAP-04 c4


def answered_in_time(wall_s: float) -> bool:
    return wall_s <= SECOND_CHECKPOINT_LIMIT_S


def pairs_cover(counts: dict[str, tuple[int, int]], expected: set[str]) -> bool:
    """CAP-03 c4: every expected container id has exactly one pause and one unpause, and nothing else does."""
    return set(counts) == set(expected) and all(pair == (1, 1) for pair in counts.values())


def collected_in_time(found_after_s: float | None, limit_s: float = ORPHAN_COLLECTION_LIMIT_S) -> bool:
    """CAP-04 c4: the orphan was seen gone, and no later than the limit after the relaunch."""
    return found_after_s is not None and found_after_s <= limit_s


def valid_on_off_pair(on: tuple[int, dict | None], off_rc: int) -> bool:
    """PERF-02: a pair counts only when the services-on checkpoint really captured (a snapshot, no gap; a skipped
    capture exits 0 with a gap) and the services-off twin succeeded."""
    rc, doc = on
    svc = (doc or {}).get("services") or {}
    return rc == 0 and off_rc == 0 and bool(svc.get("snapshot")) and not svc.get("gaps")


def wait_found_at(predicate, started: float, limit_s: float, step: float = 2.0) -> float | None:
    """Seconds after `started` at which `predicate` first held, polling until `limit_s` after `started`; None if it never did."""
    found: list[float] = []

    def seen():
        if predicate():
            found.append(time.monotonic() - started)
            return True
        return False

    wait_until(seen, max(0.0, limit_s - (time.monotonic() - started)), step)
    return found[0] if found else None


# ---------------------------------------------------------------- scenarios


class Ctx:
    def __init__(self, sess: Session):
        self.sess = sess
        self.pa = sess.project("pA")
        self.pb: Path | None = None
        self.vm = ""
        self.stack: Stack | None = None
        self.events: Events | None = None


def setup(ctx: Ctx) -> None:
    s = ctx.sess
    if (Path(s.args.fixtures) / "nfx-kit" / "stage.py").is_file():
        s.install_kit()
    s.launch()
    s.enable("pA")
    ctx.vm = s.start_vm("pA")
    doc = s.svc("pA")
    check("setup/ready", doc["state"] == "ready" and doc["relay"]["state"] == "ready", f"{doc['state']} relay {doc['relay']['state']}")
    ctx.stack = Stack(ctx.vm, ctx.pa / "app")
    ctx.events = Events(ctx.vm)
    # The pool's current content is the baseline: the VM is new, so it holds no snapshot.
    say("INFO", "setup/pool", f"snapshots at start: {pool_ids(ctx.vm)} used={pool_used(ctx.vm)}")


def cap01(ctx: Ctx) -> None:
    """The checkpoint records exactly what was saved; only the checkpoint's workspace is paused (CAP-01, CAP-07 base half)."""
    s, vm = ctx.sess, ctx.vm
    ctx.stack.up(load=True)
    ids = ctx.stack.ids()
    drop_lane(vm, "lanex")
    lane_id = lane_writer(vm, "lanex", "lanex", "secret-lanex")
    n_before = lane_counter(vm, "lanex")
    mark = ctx.events.mark()
    rc, doc, wall, text = s.ckpt("pA", "cap01")
    ev = ctx.events.since(mark)
    check("cap01/exit-0", rc == 0 and doc is not None, text[:160])
    if doc is None:
        return
    svc = doc.get("services")
    check("cap01/c1-services-non-null", bool(svc) and bool(svc.get("snapshot")), json.dumps(svc)[:200])
    if not svc or not svc.get("snapshot"):
        return
    names = [v["name"] for v in svc["volumes"]]
    want = {"pgdata", "uploads", "notes"}
    check("cap01/c1-volumes", all(any(n.endswith(w) for n in names) for w in want), str(names))
    stack_services = {c["service"] for st in svc["stacks"] if st["project"] == ctx.stack.project for c in st["containers"]}
    check("cap01/c1-stacks", {"db", "app"} <= stack_services, str(svc["stacks"])[:200])
    check("cap01/c1-standalone", any("keeper" in n for n in svc.get("standalone", [])), str(svc.get("standalone")))
    check("cap01/c1-gaps-empty", svc["gaps"] == [], str(svc["gaps"]))
    listed = [c for c in s.listed("pA") if c["checkpoint"] == doc["checkpoint"]]
    check("cap01/checkpoints-json-lists-it", bool(listed) and listed[0].get("services") == svc, "same record in `checkpoints --json`")
    base_ids = {ids["db"], ids["app"], ids["keeper"]}
    w = window_ms(ev, base_ids)
    check("cap01/c2-frozen-ms", w is not None and abs(svc["frozen_ms"] - w) <= 50 and svc["frozen_ms"] <= wall * 1000,
          f"frozen_ms={svc['frozen_ms']} engine events window={w and round(w, 1)} ms wall={wall * 1000:.0f} ms")
    snap = svc["snapshot"]
    man = pool_manifest(vm, snap)
    check("cap01/c3-manifest-matches-record", man["id"] == snap and [v["id"] for v in man["volumes"]] == [v["id"] for v in svc["volumes"]], f"{len(man['volumes'])} volumes")
    check("cap01/c3-every-volume-captured", all(v["captured"] for v in man["volumes"]) and len(man["volumes"]) == 3, str([(v["name"], v["captured"]) for v in man["volumes"]]))
    by = {v["name"].rsplit("_", 1)[-1]: v["id"] for v in man["volumes"]}
    if {"pgdata", "uploads", "notes"} <= set(by):
        pgv = eng(vm, f"cat /pool/snapshots/{snap}/{by['pgdata']}/PG_VERSION")
        seed = eng(vm, f"stat -c %s /pool/snapshots/{snap}/{by['uploads']}/seed.bin")
        note = eng(vm, f"cat /pool/snapshots/{snap}/{by['notes']}/m")
        check("cap01/c3-sentinels", pgv == "17" and seed == "1048576" and note == "n1", f"PG_VERSION={pgv} seed.bin={seed} notes/m={note}")
    check("cap01/c3-scope-is-base-only", man["workspace"] == "base" and not any("lanex" in v["name"] for v in man["volumes"]), f"workspace={man['workspace']}")
    check("cap01/c3-lane-x-not-paused-and-kept-writing", not any(e[1] == lane_id for e in ev) and lane_counter(vm, "lanex") > n_before + 5,
          f"counter {n_before} -> {lane_counter(vm, 'lanex')}; pause events for lane x: {sum(1 for e in ev if e[1] == lane_id)}")
    check("cap01/c4-nothing-paused", paused(vm) == [], str(paused(vm)))
    drop_lane(vm, "lanex")


def cap01_never_created(ctx: Ctx) -> None:
    """CAP-01 c5: an enabled project whose VM was never created gets `services: null`, and no VM."""
    s = ctx.sess
    if ctx.pb is None:
        ctx.pb = s.project("pB", with_fixture=False)
        s.enable("pB")
    before = {n for n in inventory() if n.startswith("marsh-xnfx01-")}
    rc, doc, wall, text = s.ckpt("pB", "cap01c5")
    after = {n for n in inventory() if n.startswith("marsh-xnfx01-")}
    check("cap01/c5-null", rc == 0 and doc is not None and doc.get("services") is None, f"rc={rc} services={doc and doc.get('services')}")
    check("cap01/c5-no-vm-created", after == before and (s.svc("pB") or {}).get("state") == "off", f"new VMs: {sorted(after - before)}; state={(s.svc('pB') or {}).get('state')}")


def pair_counts(events: list[tuple[str, str, int]]) -> dict[str, tuple[int, int]]:
    """(pauses, unpauses) per container id."""
    counts: dict[str, list[int]] = {}
    for action, cid, _ in events:
        entry = counts.setdefault(cid, [0, 0])
        entry[0 if action == "pause" else 1] += 1
    return {cid: (a, b) for cid, (a, b) in counts.items()}


def cap07_lane(ctx: Ctx) -> None:
    """CAP-07 product: `checkpoint --lane S/a` captures that lane's workspace only (A4)."""
    s, vm = ctx.sess, ctx.vm
    ctx.stack.up(load=True)
    base_ids = set(ctx.stack.ids().values())
    split = open_lanes(ctx)
    if split is None:
        return
    ws_a, ws_b = f"fork-{split}-a", f"fork-{split}-b"
    try:
        for n in ("lanea", "laneb"):
            drop_lane(vm, n)
        id_a = lane_writer(vm, ws_a, "lanea", "secret-lane-a")
        id_b = lane_writer(vm, ws_b, "laneb", "secret-lane-b")
        n_base = ctx.stack.tokens()
        n_b = lane_counter(vm, "laneb")
        mark = ctx.events.mark()
        rc, doc, wall, text = s.ckpt("pA", "cap07", lane=f"{split}/a")
        ev = ctx.events.since(mark)
        check("cap07/exit-0", rc == 0 and doc is not None, text[:200])
        svc = (doc or {}).get("services") or {}
        snap = svc.get("snapshot")
        check("cap07/c1-services-recorded", bool(snap), json.dumps(svc)[:200])
        if not snap:
            return
        man = pool_manifest(vm, snap)
        check("cap07/c1-workspace-is-the-lane", man["workspace"] == ws_a, man["workspace"])
        counts = pair_counts(ev)
        check("cap07/c1-exactly-one-pause-pair-for-lane-a-and-none-else",
              counts == {id_a: (1, 1)} and not (set(counts) & (base_ids | {id_b})),
              f"pairs per container: { {cid[:12]: pair for cid, pair in counts.items()} }")
        check("cap07/c1-others-kept-writing", ctx.stack.tokens() > n_base and lane_counter(vm, "laneb") > n_b + 5,
              f"base tokens {n_base}->{ctx.stack.tokens()}, lane b {n_b}->{lane_counter(vm, 'laneb')}")
        by = {v["name"]: v["id"] for v in man["volumes"]}
        secrets = {eng(vm, f"cat /pool/snapshots/{snap}/{vid}/secret 2>/dev/null") for vid in by.values()}
        check("cap07/c2-holds-a-secret-only", "secret-lane-a" in secrets and "secret-lane-b" not in secrets and not any("stacka" in n for n in by),
              f"volumes {list(by)}; secrets {sorted(x for x in secrets if x)}")
    finally:
        for n in ("lanea", "laneb"):
            drop_lane(vm, n)
        s.cli("split", "cancel", split, cwd=ctx.pa)


def open_lanes(ctx: Ctx) -> str | None:
    """Open a lane split of project pA (a git project) and return its id."""
    s = ctx.sess
    if not (ctx.pa / ".git").exists():
        for argv in (["init", "-q"], ["add", "README"], ["-c", "user.name=c12", "-c", "user.email=c12@example.invalid", "commit", "-q", "-m", "init"]):
            run(["git", *argv], cwd=ctx.pa)
    r = s.cli("split", "--open", "--label", "c12", "a", "b", cwd=ctx.pa)
    if not check("lanes/open", r.returncode == 0, (r.stdout + r.stderr).strip()[:200]):
        return None
    return json.loads(r.stdout)["id"]


def cap02_fault(ctx: Ctx, expect_unpaused: bool = True) -> None:
    """CAP-02 variant b: a capture that fails after the pause (`fault-capture-fail-after-pause`)."""
    s, vm = ctx.sess, ctx.vm
    ctx.stack.up(load=True)
    started = time.monotonic()
    rc, doc, wall, text = s.ckpt("pA", "cap02b")
    check("cap02/file-checkpoint-succeeds", rc == 0 and doc is not None, text[:160])
    gaps = ((doc or {}).get("services") or {}).get("gaps", [])
    check("cap02/c2-gap-recorded", bool(gaps) and not ((doc or {}).get("services") or {}).get("snapshot"), str(gaps)[:200])
    quick = wait_until(lambda: paused(vm) == [], 1.0, 0.2)
    check("cap02/c2-all-unpaused-within-1s", bool(quick), f"paused {len(paused(vm))} one second after the checkpoint returned")
    time.sleep(3)
    check("cap02/c3-still-nothing-paused", paused(vm) == [], str(paused(vm)))
    if not quick:
        prod(vm, "unpause", *paused(vm), check_rc=False)  # our own containers


def cap05(ctx: Ctx) -> None:
    """CAP-05: a pool below the reserve gives a gap, the file checkpoint proceeds, and the snapshot set is unchanged."""
    s, vm = ctx.sess, ctx.vm
    ctx.stack.up(load=False)
    rc0, c0, _, text = s.ckpt("pA", "cap05-c0")
    check("cap05/c0-captured", rc0 == 0 and bool((c0 or {}).get("services", {}).get("snapshot")), text[:120])
    reserve = ctx.sess.reserve_bytes
    free = pool_free(vm)
    target = reserve - (200 << 20)  # leave 200 MiB below the reserve: room for a small clone
    take = free - target
    eng(vm, f"fallocate -l {take} /pool/c12-fill || dd if=/dev/zero of=/pool/c12-fill bs=1M count={take >> 20}")
    say("INFO", "cap05/pool", f"free was {free >> 20} MiB, now {pool_free(vm) >> 20} MiB (reserve {reserve >> 20} MiB)")
    before = pool_ids(vm)
    results = [s.ckpt("pA", f"cap05-low{i}") for i in range(3)]
    after = pool_ids(vm)
    ok_exit = all(r[0] == 0 and r[1] is not None for r in results)
    gaps = [((r[1] or {}).get("services") or {}).get("gaps", []) for r in results]
    check("cap05/c1-exit-0-with-files", ok_exit and all(os.path.isdir(r[1]["dir"] + "/tree") for r in results if r[1]), "; ".join(r[3][:60] for r in results if r[0]))
    check("cap05/c1-space-gap-and-no-snapshot", all("not enough space for app data" in g for g in gaps) and before == after,
          f"gaps={gaps[0]}; snapshots {len(before)} -> {len(after)}")
    cont = prod(vm, "ps", "-q", "--filter", f"name={ctx.stack.project}-keeper").strip()
    prod(vm, "pause", cont)
    p1 = paused(vm)
    prod(vm, "unpause", cont)
    check("cap05/c2-pause-unpause-work-below-reserve", len(p1) == 1 and paused(vm) == [], f"paused while held: {len(p1)}")
    eng(vm, "rm -f /pool/c12-fill")
    time.sleep(1)
    rc, doc, _, text = s.ckpt("pA", "cap05-freed")
    svc = (doc or {}).get("services") or {}
    check("cap05/c4-no-gap-after-free", rc == 0 and bool(svc.get("snapshot")) and svc.get("gaps") == [], json.dumps(svc.get("gaps")))


def cap04(ctx: Ctx) -> None:
    """CAP-04: retention drops snapshots, rm drops, orphans are collected, and the pool returns to a clean baseline."""
    s, vm = ctx.sess, ctx.vm
    ctx.stack.up(load=False)
    # A clean baseline: no checkpoint of the project, the pool holding only its last capture.
    for old in s.listed("pA"):
        s.rm_ckpt("pA", old["checkpoint"])
    rc, c0, _, text = s.ckpt("pA", "base")
    if not check("cap04/baseline-capture", rc == 0 and bool((c0 or {}).get("services", {}).get("snapshot")), text[:120]):
        return
    s.rm_ckpt("pA", c0["checkpoint"])
    wait_until(lambda: set(pool_ids(vm)) <= live_set(s, "pA"), 20)
    time.sleep(2)
    base_used = pool_used(vm)
    say("INFO", "cap04/baseline", f"pool used {base_used >> 20} MiB with snapshots {pool_ids(vm)} (the last capture only)")
    for _ in range(21):
        rc, doc, _, text = s.ckpt("pA", "ret")
        if rc or not doc:
            check("cap04/make", False, text[:160])
            return
    ret = s.listed("pA", "ret")
    check("cap04/c1-twenty-remain", len(ret) == 20, f"{len(ret)} checkpoints labelled ret")
    live = live_set(s, "pA")
    held = set(pool_ids(vm))
    check("cap04/c1-snapshot-set-equals-live-set", held == live, f"pool {len(held)}, live {len(live)}; extra={sorted(held - live)[:3]} missing={sorted(live - held)[:3]}")
    last = (s.svc("pA") or {}).get("last_capture", {}).get("id")
    victim = next(c for c in reversed(ret) if (c["services"] or {}).get("snapshot") not in (None, last))
    vsnap = victim["services"]["snapshot"]
    r = s.rm_ckpt("pA", victim["checkpoint"])
    gone = wait_until(lambda: vsnap not in pool_ids(vm), 10)
    check("cap04/c2-rm-drops-within-10s", r.returncode == 0 and bool(gone), f"rm rc={r.returncode}, snapshot still present: {not gone}")
    # c4: an orphan the daemon never saw removed: the daemon is killed (exact pid), one checkpoint directory
    # deleted by its own path, the daemon relaunched; the clock starts when the new daemon is launched.
    ret = s.listed("pA", "ret")
    lost = next(c for c in reversed(ret) if (c["services"] or {}).get("snapshot") not in (None, last))
    lsnap = lost["services"]["snapshot"]
    s.kill()
    shutil.rmtree(lost["dir"])
    relaunched = time.monotonic()
    s.launch(s.breaks, s.env_extra)
    found = wait_found_at(lambda: lsnap not in pool_ids(vm), relaunched, ORPHAN_COLLECTION_LIMIT_S)
    check("cap04/c4-orphan-collected-within-60s-of-relaunch", collected_in_time(found),
          f"snapshot {lsnap[:8]} " + (f"collected {found:.0f}s after the relaunch" if found is not None else f"still present {time.monotonic() - relaunched:.0f}s after the relaunch"))
    expected = live_set(s, "pA") - {lsnap}
    check("cap04/orphan-collection-keeps-the-referenced", bool(wait_until(lambda: set(pool_ids(vm)) >= expected, 60, 2)), "every referenced snapshot still in the pool")
    # c6: remove everything, and the pool is within 64 MiB of the clean baseline, in either direction.
    for c in s.listed("pA"):
        s.rm_ckpt("pA", c["checkpoint"])
    wait_until(lambda: set(pool_ids(vm)) <= live_set(s, "pA"), 20)
    time.sleep(3)
    used = pool_used(vm)
    check("cap04/c6-pool-within-64MiB-of-baseline", abs(used - base_used) <= 64 << 20,
          f"baseline {base_used >> 20} MiB, now {used >> 20} MiB (|delta| {abs(used - base_used) >> 20} MiB); snapshots left {pool_ids(vm)} (last capture {live_set(s, 'pA')})")


def cap08(ctx: Ctx, breaks_run: bool = False) -> None:
    """CAP-08 / CAP-06 reuse: idle stop takes a final capture; checkpoints while stopped reference it (D9, D12)."""
    s, vm = ctx.sess, ctx.vm
    ctx.stack.up(load=False)
    s.cli("config", "services-idle-seconds", "30", cwd=ctx.pa)
    r1 = s.ckpt("pA", "cap08")
    c1 = r1[1]
    check("cap08/c1-first-checkpoint", r1[0] == 0 and bool(((c1 or {}).get("services") or {}).get("snapshot")), r1[3][:120])
    c1snap = c1["services"]["snapshot"]
    # a shell-started writer inserts rows after C1, then exits
    prod(vm, "exec", ctx.stack.db, "psql", "-U", "postgres", "-d", "app", "-q", "-c",
         "insert into markers(name) select 'post-c1-' || g from generate_series(1,25) g")
    rows = pg_count(vm, ctx.stack.db, "select count(*) from markers where name like 'post-c1-%'")
    check("cap08/post-c1-rows-written", rows == "25", rows)
    s.cli("services", "start", cwd=ctx.pa)  # re-arms the idle timer at the 30 s setting
    stopped = s.wait_state("pA", "stopped", 150)
    check("cap08/idle-stopped", stopped is not None and stopped["state"] == "stopped", f"state {stopped and stopped['state']}")
    if not stopped or stopped["state"] != "stopped":
        return
    last = stopped.get("last_capture") or {}
    F = last.get("id")
    check("cap08/c2-final-capture-is-last-capture", last.get("reason") == "idle-stop" and F not in (None, c1snap), f"last_capture={last}")
    before_vms = inventory()
    r2, r3 = s.ckpt("pA", "cap08"), s.ckpt("pA", "cap08")
    reused = [((r[1] or {}).get("services") or {}).get("reused_from") for r in (r2, r3)]
    check("cap08/c1-reused-from-set-no-wake", all(reused) and (s.svc("pA") or {}).get("state") == "stopped" and inventory()[vm]["status"] == before_vms[vm]["status"], f"reused_from={reused}; state={(s.svc('pA') or {}).get('state')}")
    check("cap08/c2-reused-from-is-F-not-C1", reused == [F, F] and F != c1snap, f"F={F} C1={c1snap} reused={reused}")
    check("cap08/c1-proven-reuse-carries-no-gap", all(services_of(r).get("gaps") == [] for r in (r2, r3)), str([services_of(r).get("gaps") for r in (r2, r3)]))
    if not all(reused):
        s.cli("config", "services-idle-seconds", "900", cwd=ctx.pa)
        restart_vm(ctx)
        return
    c2, c3 = r2[1]["checkpoint"], r3[1]["checkpoint"]
    check("cap08/reuse-record-carries-the-snapshots-volumes", bool(services_of(r2).get("volumes")) and services_of(r2).get("snapshot") is None, str([v["name"] for v in services_of(r2).get("volumes", [])]))
    s.rm_ckpt("pA", c1["checkpoint"])
    # supersede F while C2 and C3 are live: restart the VM, take C4
    s.cli("services", "start", cwd=ctx.pa, timeout=1800)
    r4 = s.ckpt("pA", "cap08b")
    c4snap = ((r4[1] or {}).get("services") or {}).get("snapshot")
    now_last = (s.svc("pA") or {}).get("last_capture", {}).get("id")
    refs_to_F = [c["checkpoint"] for c in s.listed("pA") if F in ((c.get("services") or {}).get("snapshot"), (c.get("services") or {}).get("reused_from"))]
    check("cap08/superseded-F-held-only-by-C2-C3", c4snap and now_last == c4snap and sorted(refs_to_F) == sorted([c2, c3]), f"last={now_last}, refs to F={refs_to_F}")
    s.relaunch(s.breaks, s.env_extra, kill=True)  # collection after a daemon relaunch
    time.sleep(20)
    kept = snapshot_present(vm, F)
    check("cap08/c3-F-survives-collection-while-C2-C3-name-it", kept, f"F present: {kept}")
    if kept:
        got = snap_rows(vm, F, "post-c1-")
        check("cap08/c3-F-holds-the-post-c1-rows", got == 25, f"post-c1 rows in the snapshot: {got}")
    for c in (c2, c3):
        s.rm_ckpt("pA", c)
    gone = wait_until(lambda: F not in pool_ids(vm), 30)
    check("cap08/c3-F-collected-after-C2-C3-removed", bool(gone), f"F present: {not gone}")
    s.cli("config", "services-idle-seconds", "900", cwd=ctx.pa)
    s.cli("services", "start", cwd=ctx.pa, timeout=1800)


def cap08_null(ctx: Ctx) -> None:
    """CAP-08 c4: never captured, stopped out of band (`sbx stop`): `services: null`."""
    s = ctx.sess
    if ctx.pb is None:
        ctx.pb = s.project("pB", with_fixture=False)
        s.enable("pB")
    vm_b = s.start_vm("pB")
    verify_owned(vm_b)
    check("cap08/c4-uuid-verified-before-stop", inventory()[vm_b]["id"] == OWNED[vm_b], OWNED[vm_b])
    r = run(["sbx", "stop", vm_b])
    say("INFO", "cap08/c4-sbx-stop", f"rc={r.returncode}")
    wait_until(lambda: inventory()[vm_b]["status"] != "running", 60)
    rc, doc, _, text = s.ckpt("pB", "cap08null")
    check("cap08/c4-null", rc == 0 and doc is not None and doc.get("services") is None, f"rc={rc} services={(doc or {}).get('services')}")


def perf(ctx: Ctx, n: int) -> None:
    """PERF-01 / PERF-02 through the product. Exclusive lock. Nothing is substituted: a sample without its
    engine-event window, a gap, a failure or a missing pair is a failure of the run, not a smaller N."""
    s, vm = ctx.sess, ctx.vm
    ctx.stack.up(load=True)
    for old in s.listed("pA"):
        s.rm_ckpt("pA", old["checkpoint"])
    time.sleep(3)
    ids = set(ctx.stack.ids().values())
    frozen, reported, wall, bad, no_window, mismatched = [], [], [], 0, 0, 0
    first_problem = ""
    for i in range(n + 3):
        mark = ctx.events.mark()
        began = time.monotonic()
        rc, doc, w, text = s.ckpt("pA", "perf")
        svc = (doc or {}).get("services") or {}
        ev = ctx.events.since(mark, settle=0.6)
        if i >= 3:
            ew = window_ms(ev, ids)
            if rc or not svc.get("snapshot") or svc.get("gaps"):
                bad += 1
                first_problem = first_problem or f"rc={rc} gaps={svc.get('gaps')} snapshot={svc.get('snapshot')} events={[(e[0], e[1][:8]) for e in ev][:8]} mine={sorted(i[:8] for i in ids)} stream_alive={ctx.events.p.poll() is None}"
            if ew is None or svc.get("frozen_ms") is None:
                no_window += 1
            else:
                frozen.append(ew)
                reported.append(svc["frozen_ms"])
                if abs(svc["frozen_ms"] - ew) > 50:
                    mismatched += 1
            wall.append(w * 1000)
        time.sleep(max(0, 1.0 - (time.monotonic() - began)))
    leftovers = len(paused(vm))
    errs = ctx.stack.errors()
    say("INFO", "perf01/freeze-ms", f"n={len(frozen)} p50={pct(frozen, .5):.0f} p95={pct(frozen, .95):.0f} max={max(frozen):.0f}" if frozen else "no samples")
    say("INFO", "perf01/checkpoint-wall-ms", f"p50={pct(wall, .5):.0f} p95={pct(wall, .95):.0f} max={max(wall):.0f} (the whole `marsh checkpoint`, files + services + retention drops)")
    check("perf01/every-sample-has-an-engine-event-window", no_window == 0 and len(frozen) == n, f"{len(frozen)} of {n} samples with a window, {no_window} without")
    check("perf01/freeze-p95-under-500ms", bool(frozen) and pct(frozen, .95) < 500, f"p95={pct(frozen, .95):.0f} ms (engine events)" if frozen else "no samples")
    check("perf01/frozen_ms-within-50ms-of-the-event-window", mismatched <= 2, f"{mismatched} of {len(frozen)} differ by more than 50 ms")
    check("perf01/capture-wall-p95-under-1s", bool(wall) and pct(wall, .95) < 1000, f"p95 of the whole checkpoint={pct(wall, .95):.0f} ms")
    check("perf01/gaps-and-failures-at-most-2", bad <= 2, f"{bad} of {n} with a gap, failure or no snapshot; first: {first_problem}")
    check("perf01/nothing-left-paused-no-writer-errors", leftovers == 0 and errs == 0, f"paused={leftovers} writer ERROR lines={errs}")
    # PERF-02: matched twins, randomized alternating pairs; every pair counts
    if "pOff" not in s.projects:
        s.project("pOff")
        s.cli("config", "services", "off", cwd=s.projects["pOff"])
    deltas, failed_pairs = [], 0
    for i in range(n + 3):
        order = random.choice([("pA", "pOff"), ("pOff", "pA")])
        got = {}
        for name in order:
            rc, doc, w, text = s.ckpt(name, "perf2")
            got[name] = (rc, doc, w * 1000)
        if i >= 3:
            if valid_on_off_pair((got["pA"][0], got["pA"][1]), got["pOff"][0]):
                deltas.append(got["pA"][2] - got["pOff"][2])
            else:
                failed_pairs += 1
        time.sleep(0.5)
    say("INFO", "perf02/delta-ms", f"pairs={len(deltas)} p50={pct(deltas, .5):.0f} p95={pct(deltas, .95):.0f} max={max(deltas):.0f}" if deltas else "no pairs")
    check("perf02/all-n-pairs-completed", failed_pairs == 0 and len(deltas) == n, f"{len(deltas)} of {n} valid captured pairs, {failed_pairs} skipped, gapped or failed")
    check("perf02/p95-delta-under-1s", bool(deltas) and pct(deltas, .95) < 1000, f"p95(services on - off)={pct(deltas, .95):.0f} ms over {len(deltas)} pairs" if deltas else "no pairs")


# ---------------------------------------------------------------- freshness, stop, orphans, admission (round 2)


def set_idle(ctx: Ctx, seconds: int) -> None:
    ctx.sess.cli("config", "services-idle-seconds", str(seconds), cwd=ctx.pa)


def restart_vm(ctx: Ctx) -> None:
    s = ctx.sess
    s.cli("services", "start", cwd=ctx.pa, timeout=1800)
    wait_until(lambda: (s.svc("pA") or {}).get("relay", {}).get("state") == "ready", 120, 2)
    if ctx.events:
        ctx.events.close()
    ctx.events = Events(ctx.vm)


def idle_stop(ctx: Ctx, seconds: int = 30) -> dict | None:
    """Arm the idle timer at `seconds` and wait for the VM to stop."""
    set_idle(ctx, seconds)
    ctx.sess.cli("services", "start", cwd=ctx.pa)
    stopped = ctx.sess.wait_state("pA", "stopped", seconds + 150)
    set_idle(ctx, 900)  # the next start arms the default again: no scenario is stopped under its own feet
    return stopped


def fill_below_reserve(ctx: Ctx) -> None:
    """Fill the pool until 200 MiB below the reserve: room for a small clone, not for a capture."""
    vm = ctx.vm
    take = pool_free(vm) - (ctx.sess.reserve_bytes - (200 << 20))
    eng(vm, f"rm -f /pool/c12-fill; fallocate -l {take} /pool/c12-fill || dd if=/dev/zero of=/pool/c12-fill bs=1M count={take >> 20}")
    say("INFO", "pool", f"{pool_free(vm) >> 20} MiB free, reserve {ctx.sess.reserve_bytes >> 20} MiB")


def unfill(ctx: Ctx) -> None:
    eng(ctx.vm, "rm -f /pool/c12-fill")


def services_of(result: tuple[int, dict | None, float, str]) -> dict:
    return ((result[1] or {}).get("services")) or {}


def fresh_stop(ctx: Ctx) -> None:
    """Items 1-3: a stop marsh causes is fenced and captured per workspace. A persistent writer keeps
    writing right up to the stop; a lane is changed after its first checkpoint; checkpoints made while
    the VM is stopped reference snapshots that hold the last write, and say nothing is missing."""
    s, vm = ctx.sess, ctx.vm
    ctx.stack.up(load=True)
    split = open_lanes(ctx)
    if split is None:
        return
    ws_a = f"fork-{split}-a"
    try:
        drop_lane(vm, "lanea")
        lane_writer(vm, ws_a, "lanea", "before")
        c1 = s.ckpt("pA", "fresh")
        cl1 = s.ckpt("pA", "fresh-lane", lane=f"{split}/a")
        c1snap, cl1snap = services_of(c1).get("snapshot"), services_of(cl1).get("snapshot")
        if not check("fresh/first-checkpoints", bool(c1snap and cl1snap), f"{c1[3][:80]} {cl1[3][:80]}"):
            return
        # The lane changes after its first checkpoint; the base writer never stops.
        prod(vm, "exec", "lanea-w", "sh", "-c", "echo after > /d/secret")
        stopped = idle_stop(ctx)
        if not check("fresh/idle-stopped", bool(stopped) and stopped["state"] == "stopped", f"state {stopped and stopped['state']}"):
            return
        last = stopped.get("last_capture") or {}
        F = last.get("id")
        check("fresh/c2-final-capture-is-the-base-last-capture", last.get("reason") == "idle-stop" and F not in (None, c1snap), f"last_capture={last}")
        rb = s.ckpt("pA", "fresh-stopped")
        svc_b = services_of(rb)
        check("fresh/c1-base-stopped-checkpoint-references-the-final-capture-with-no-gap",
              svc_b.get("reused_from") == F and svc_b.get("gaps") == [], f"reused_from={svc_b.get('reused_from')} gaps={svc_b.get('gaps')}")
        rl = s.ckpt("pA", "fresh-lane-stopped", lane=f"{split}/a")
        svc_l = services_of(rl)
        lane_ref = svc_l.get("reused_from")
        check("fresh/c3-lane-stopped-checkpoint-is-not-the-stale-lane-snapshot",
              bool(lane_ref) and lane_ref not in (cl1snap, F) and svc_l.get("gaps") == [],
              f"lane reused_from={lane_ref} (first lane snapshot {cl1snap}, base final capture {F}) gaps={svc_l.get('gaps')}")
        restart_vm(ctx)
        if lane_ref:
            held = snap_file(vm, lane_ref, "lanea_data", "secret")
            live = prod(vm, "run", "--rm", "-v", "lanea_data:/d", APP_IMAGE, "cat", "/d/secret", check_rc=False).strip()
            check("fresh/c3-lane-snapshot-holds-the-lane-data-as-it-was-at-the-stop", held == "after" and live == "after", f"snapshot={held!r} live={live!r}")
        in_snapshot = snap_sql(vm, F, "select count(*) from tokens") if F else None
        live_rows = live_sql(vm, "stacka_pgdata", "select count(*) from tokens")
        check("fresh/c2-no-write-between-the-final-capture-and-the-stop",
              in_snapshot is not None and in_snapshot == live_rows,
              f"tokens in the final capture {in_snapshot}, in the live volume after the stop {live_rows}")
    finally:
        drop_lane(vm, "lanea")
        s.cli("split", "cancel", split, cwd=ctx.pa)


def stop_failure(ctx: Ctx) -> None:
    """Items 1 and 7: a final capture that cannot happen (the pool is below the reserve at the stop) is a
    gap for every checkpoint made while stopped, across a daemon restart; an explicit `services stop` and
    `marsh stop` capture and prove."""
    s, vm = ctx.sess, ctx.vm
    ctx.stack.up(load=False)
    c1 = s.ckpt("pA", "sf")
    c1snap = services_of(c1).get("snapshot")
    if not check("stopfail/first-checkpoint", bool(c1snap), c1[3][:100]):
        return
    prod(vm, "exec", ctx.stack.db, "psql", "-U", "postgres", "-d", "app", "-q", "-c",
         "insert into markers(name) select 'after-c1-' || g from generate_series(1,25) g")
    fill_below_reserve(ctx)
    stopped = idle_stop(ctx)
    if not check("stopfail/idle-stopped", bool(stopped) and stopped["state"] == "stopped", f"state {stopped and stopped['state']}"):
        return
    r = s.ckpt("pA", "sf-stopped")
    svc = services_of(r)
    gaps = " | ".join(svc.get("gaps", []))
    check("stopfail/c1-final-capture-below-reserve-is-a-gap-not-the-old-snapshot",
          r[0] == 0 and svc.get("snapshot") is None and svc.get("reused_from") is None and "final capture" in gaps and "not enough space" in gaps,
          f"snapshot={svc.get('snapshot')} reused_from={svc.get('reused_from')} gaps={gaps[:200]}")
    check("stopfail/c1-the-last-capture-is-still-the-checkpoint-one", ((s.svc("pA") or {}).get("last_capture") or {}).get("id") == c1snap, str((s.svc("pA") or {}).get("last_capture")))
    s.relaunch(s.breaks, s.env_extra)  # a restart (the exact pid is killed); the VM stays stopped
    r2 = s.ckpt("pA", "sf-stopped-after-restart")
    svc2 = services_of(r2)
    check("stopfail/c1-the-gap-survives-a-daemon-restart", svc2.get("snapshot") is None and svc2.get("reused_from") is None and "final capture" in " ".join(svc2.get("gaps", [])),
          f"reused_from={svc2.get('reused_from')} gaps={svc2.get('gaps')}")
    restart_vm(ctx)
    unfill(ctx)
    ctx.stack.up(load=False)
    s.cli("services", "stop", cwd=ctx.pa)
    r3 = s.ckpt("pA", "sf-explicit-stop")
    svc3 = services_of(r3)
    check("stopfail/c1-explicit-services-stop-captures-and-proves",
          bool(svc3.get("reused_from")) and svc3["reused_from"] != c1snap and svc3.get("gaps") == [], f"reused_from={svc3.get('reused_from')} gaps={svc3.get('gaps')}")
    restart_vm(ctx)
    ctx.stack.up(load=False)
    s.stop()  # `marsh stop`: the daemon exits, the cutover runs for the running services VM first
    s.launch(s.breaks, s.env_extra)
    r4 = s.ckpt("pA", "sf-marsh-stop")
    svc4 = services_of(r4)
    check("stopfail/c1-marsh-stop-captures-and-proves-across-the-restart",
          bool(svc4.get("reused_from")) and svc4["reused_from"] not in (c1snap, svc3.get("reused_from")) and svc4.get("gaps") == [],
          f"reused_from={svc4.get('reused_from')} gaps={svc4.get('gaps')}")
    restart_vm(ctx)


def journal_loss(ctx: Ctx, silent: bool = False) -> None:
    """Item 1: a journal that cannot be written (`journal-write-fail`) is not a silent fallback after a
    restart: the checkpoint references the workspace's latest record WITH a warning gap."""
    s, vm = ctx.sess, ctx.vm
    s.relaunch(("journal-write-fail",), s.env_extra)
    ready_with(ctx, ())
    ctx.stack.up(load=False)
    c1 = s.ckpt("pA", "jl")
    stopped = idle_stop(ctx)
    if not check("journal/idle-stopped", bool(stopped) and stopped["state"] == "stopped", f"state {stopped and stopped['state']}"):
        return
    F = (stopped.get("last_capture") or {}).get("id")
    r = s.ckpt("pA", "jl-stopped")
    check("journal/same-run-uses-the-in-memory-record", services_of(r).get("reused_from") == F and F is not None, f"reused_from={services_of(r).get('reused_from')} F={F}")
    s.relaunch(("journal-loss-silent",) if silent else (), s.env_extra)
    r2 = s.ckpt("pA", "jl-after-restart")
    svc2 = services_of(r2)
    check("journal/c1-lost-journal-is-a-warning-reuse-not-a-silent-null",
          bool(svc2.get("reused_from")) and any("lost its record" in g for g in svc2.get("gaps", [])),
          f"reused_from={svc2.get('reused_from')} gaps={svc2.get('gaps')} services={'null' if not svc2 else 'record'}")
    restart_vm(ctx)


def rm_recreate(ctx: Ctx) -> None:
    """Item 1: what marsh recorded is bound to the owned VM. After `services rm` and a new VM, a journal entry that
    survived (a crash between the two) is not a reuse source: the new VM's checkpoint is null."""
    s = ctx.sess
    ctx.stack.up(load=False)
    s.ckpt("pA", "rr")
    stopped = idle_stop(ctx)
    if not check("rmrecreate/idle-stopped", bool(stopped) and stopped["state"] == "stopped", f"state {stopped and stopped['state']}"):
        return
    journal = s.root / "mh" / "services-last-capture.json"
    saved = journal.read_bytes()
    old_vm = ctx.vm
    r = s.cli("services", "rm", "--yes", cwd=ctx.pa, timeout=900)
    check("rmrecreate/rm", r.returncode == 0, (r.stdout + r.stderr).strip()[:120])
    OWNED.pop(old_vm, None)
    if ctx.events:
        ctx.events.close()
        ctx.events = None
    journal.write_bytes(saved)  # as if the crash came between the removal and forgetting the entry
    s.relaunch(s.breaks, s.env_extra)
    ctx.vm = s.start_vm("pA")
    ctx.stack = Stack(ctx.vm, ctx.pa / "app")
    check("rmrecreate/a-different-vm", ctx.vm != old_vm, f"{old_vm} -> {ctx.vm}")
    verify_owned(ctx.vm)
    run(["sbx", "stop", ctx.vm])  # outside marsh: no outcome is recorded for this VM
    wait_until(lambda: inventory()[ctx.vm]["status"] != "running", 60)
    r = s.ckpt("pA", "rr-new")
    check("rmrecreate/c1-the-new-vm-has-no-snapshot-to-reuse", r[0] == 0 and r[1] is not None and r[1].get("services") is None,
          f"services={(r[1] or {}).get('services')}")
    restart_vm(ctx)


def orphan_only(ctx: Ctx) -> None:
    """Item 5: a project whose last checkpoint record is gone and that has no base capture is still scanned
    (found from the owned VM), so a lane-only snapshot removed while the VM was stopped is collected after
    a restart."""
    s = ctx.sess
    old_vm = ctx.vm
    s.cli("services", "rm", "--yes", cwd=ctx.pa, timeout=900)  # a clean pool and journal for the project
    OWNED.pop(old_vm, None)
    if ctx.events:
        ctx.events.close()
        ctx.events = None
    for c in s.listed("pA"):
        s.rm_ckpt("pA", c["checkpoint"])
    ctx.vm = s.start_vm("pA")
    vm = ctx.vm
    ctx.stack = Stack(vm, ctx.pa / "app")
    ctx.stack.up(load=False)
    split = open_lanes(ctx)
    if split is None:
        return
    try:
        drop_lane(vm, "lanea")
        lane_writer(vm, f"fork-{split}-a", "lanea", "orphan")
        cl = s.ckpt("pA", "orph", lane=f"{split}/a")
        snap = services_of(cl).get("snapshot")
        if not check("orphan/lane-checkpoint", bool(snap), cl[3][:100]):
            return
        verify_owned(vm)
        run(["sbx", "stop", vm])  # outside marsh: no cutover, no base capture
        wait_until(lambda: inventory()[vm]["status"] != "running", 60)
        r = s.rm_ckpt("pA", cl[1]["checkpoint"])
        check("orphan/rm-while-stopped", r.returncode == 0, (r.stdout + r.stderr).strip()[:120])
        journal = s.root / "mh" / "services-last-capture.json"
        known = json.loads(journal.read_text())["projects"] if journal.exists() else {}
        check("orphan/no-journal-root-and-no-record", os.path.realpath(ctx.pa) not in {os.path.realpath(k) for k in known} and not s.listed("pA"),
              f"journal projects={list(known)}")
        s.relaunch(s.breaks, s.env_extra)
        began = time.monotonic()
        s.cli("services", "start", cwd=ctx.pa, timeout=1800)
        found = wait_found_at(lambda: snap not in pool_ids(vm), began, ORPHAN_COLLECTION_LIMIT_S)
        check("orphan/c1-lane-only-orphan-collected-after-restart", collected_in_time(found),
              f"snapshot {snap[:8]} " + (f"collected {found:.0f}s after the restart" if found is not None else f"still present {time.monotonic() - began:.0f}s after the restart"))
    finally:
        drop_lane(vm, "lanea")
        s.cli("split", "cancel", split, cwd=ctx.pa)
        wait_until(lambda: (s.svc("pA") or {}).get("relay", {}).get("state") == "ready", 120, 2)
        if ctx.events:
            ctx.events.close()
        ctx.events = Events(vm)


def background_checkpoint(ctx: Ctx, label: str) -> subprocess.Popen:
    s = ctx.sess
    return subprocess.Popen([str(s.prefix / "bin" / "marsh"), "checkpoint", "--json", "--label", label], cwd=ctx.pa, env=s.env,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)


def agent_hold(ctx: Ctx, point: str) -> bool:
    return wait_until(lambda: f"{point}.reached" in eng(ctx.vm, "ls /run/marsh-test-holds 2>/dev/null"), 60, 0.5)


class AutoRelease:
    """Releases an agent hold every time it is reached, until stopped: later captures (a stop cutover's)
    hit the same hold, which this scenario is not about."""

    def __init__(self, ctx: Ctx, point: str):
        self.ctx, self.point, self.done = ctx, point, threading.Event()
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    def _run(self) -> None:
        while not self.done.wait(1.0):
            try:
                if f"{self.point}.reached" in eng(self.ctx.vm, "ls /run/marsh-test-holds 2>/dev/null"):
                    agent_release(self.ctx, self.point)
            except Exception:  # noqa: BLE001
                pass

    def stop(self) -> None:
        self.done.set()
        self.thread.join(10)


def agent_release(ctx: Ctx, point: str) -> None:
    eng(ctx.vm, f"touch /run/marsh-test-holds/{point}.release")


def concurrent(ctx: Ctx) -> None:
    """Item 4 (CAP-03 c3/c4 through the product): a checkpoint made while another is frozen at
    `hold-capture-frozen` is answered by the agent's own busy gap within about a second, not held behind
    the first; the freeze windows never overlap."""
    s, vm = ctx.sess, ctx.vm
    ctx.stack.up(load=True)
    ids = set(ctx.stack.ids().values())
    eng(vm, "rm -f /run/marsh-test-holds/capture-frozen.reached /run/marsh-test-holds/capture-frozen.release")
    mark = ctx.events.mark()
    first = background_checkpoint(ctx, "conc1")
    try:
        if not check("conc/first-capture-frozen-at-the-hold", agent_hold(ctx, "capture-frozen"), "capture-frozen.reached"):
            return
        result: dict = {}
        thread = threading.Thread(target=lambda: result.update(r=s.ckpt("pA", "conc2")))
        thread.start()
        thread.join(6.0)  # the limit is judged on the checkpoint's own wall time below; a blocked one is released after this
    finally:
        agent_release(ctx, "capture-frozen")
    thread.join(60)
    out, err = first.communicate(timeout=120)
    doc2 = result.get("r") or (1, None, 99.0, "")
    svc2 = services_of(doc2)
    check("conc/c3-second-checkpoint-is-not-held-behind-the-first", doc2[0] == 0 and answered_in_time(doc2[2]),
          f"the second checkpoint's result came after {doc2[2]:.2f}s while the first capture is frozen (limit {SECOND_CHECKPOINT_LIMIT_S:.1f}s)")
    check("conc/c3-second-result-is-the-agents-skipped-gap",
          doc2[0] == 0 and svc2.get("snapshot") is None and any("another capture in progress" in g for g in svc2.get("gaps", [])),
          f"gaps={svc2.get('gaps')}")
    first_doc = json.loads(out) if out.strip() else {}
    # The first capture was held past the agent's freeze budget on purpose, so it may legitimately end as a
    # gap; what matters is that the checkpoint itself succeeded and recorded what happened.
    check("conc/first-checkpoint-succeeds-and-records-its-services-result", first.returncode == 0 and first_doc.get("services") is not None,
          json.dumps(first_doc.get("services"))[:400] if first_doc else (err or out)[:200])
    ev = ctx.events.since(mark)
    counts = pair_counts(ev)
    check("conc/c4-freeze-windows-never-overlap-one-pair-per-container",
          pairs_cover(counts, ids), f"expected {sorted(i[:12] for i in ids)}; pairs per container: { {cid[:12]: pair for cid, pair in counts.items()} }")


def idle_with_capture_in_flight(ctx: Ctx) -> None:
    """Item 1 (overlap): a checkpoint capture in flight keeps the idle stop from starting under it; the stop
    then proceeds with its cutover."""
    s, vm = ctx.sess, ctx.vm
    ctx.stack.up(load=False)
    eng(vm, "rm -f /run/marsh-test-holds/capture-frozen.reached /run/marsh-test-holds/capture-frozen.release")
    set_idle(ctx, 20)
    first = background_checkpoint(ctx, "idle-inflight")
    try:
        if not check("inflight/capture-frozen-at-the-hold", agent_hold(ctx, "capture-frozen"), "capture-frozen.reached"):
            return
        s.cli("services", "start", cwd=ctx.pa)  # arms the idle timer at 20 s
        time.sleep(40)
        state = (s.svc("pA") or {}).get("state")
        check("inflight/idle-stop-waits-for-the-capture", state == "ready", f"state {state} 40 s after the idle deadline, capture still held")
    finally:
        agent_release(ctx, "capture-frozen")
    first.communicate(timeout=120)
    # The cutover's own capture reaches the same hold, and the agent's freeze budget is half a second: a release
    # already waiting lets it through at once (and the helper releases any later one).
    agent_release(ctx, "capture-frozen")
    auto = AutoRelease(ctx, "capture-frozen")
    try:
        stopped = s.wait_state("pA", "stopped", 150)
    finally:
        auto.stop()
    last = ((stopped or {}).get("last_capture") or {})
    check("inflight/idle-stop-then-proceeds-with-its-final-capture", bool(stopped) and stopped["state"] == "stopped" and last.get("reason") == "idle-stop",
          f"state {stopped and stopped['state']} last_capture={last}")
    set_idle(ctx, 900)
    restart_vm(ctx)


def space_boundary(ctx: Ctx) -> None:
    """Item 7: the reserve is checked at the capture boundary. The pool is filled while the checkpoint is held
    just before it; the capture is skipped with the space gap. An unreadable pool is skipped too."""
    s, vm = ctx.sess, ctx.vm
    ctx.stack.up(load=False)
    holds = s.root / "mh" / "test-holds"
    holds.mkdir(parents=True, exist_ok=True)
    (holds / "checkpoint-before-space-check.release").write_text("")  # this warm-up checkpoint passes the hold at once
    s.ckpt("pA", "sb0")  # a normal reading first (under `space-check-stale` it is the one that is reused)
    for name in ("checkpoint-before-space-check.reached", "checkpoint-before-space-check.release"):
        (holds / name).unlink(missing_ok=True)
    first = background_checkpoint(ctx, "sb")
    try:
        reached = wait_until(lambda: (holds / "checkpoint-before-space-check.reached").exists(), 60, 0.5)
        if not check("space/held-before-the-space-check", bool(reached), "checkpoint-before-space-check.reached"):
            return
        fill_below_reserve(ctx)
    finally:
        holds.mkdir(parents=True, exist_ok=True)
        (holds / "checkpoint-before-space-check.release").write_text("")
    out, err = first.communicate(timeout=180)
    doc = json.loads(out) if out.strip() else {}
    svc = doc.get("services") or {}
    check("space/c1-pool-filled-during-the-hold-is-a-space-gap-at-capture-time",
          first.returncode == 0 and svc.get("snapshot") is None and "not enough space" in " ".join(svc.get("gaps", [])),
          f"rc={first.returncode} snapshot={svc.get('snapshot')} gaps={svc.get('gaps')}")
    unfill(ctx)


def pool_unreadable(ctx: Ctx) -> None:
    """Item 7: a ready VM whose pool cannot be read is not captured (`fault-pool-unreadable`)."""
    s = ctx.sess
    ctx.stack.up(load=False)
    r = s.ckpt("pA", "unreadable")
    svc = services_of(r)
    check("space/c1-unreadable-pool-is-a-gap-not-a-capture",
          r[0] == 0 and svc.get("snapshot") is None and "could not be read" in " ".join(svc.get("gaps", [])),
          f"rc={r[0]} snapshot={svc.get('snapshot')} gaps={svc.get('gaps')}")


def publish_atomic(ctx: Ctx) -> None:
    """CAP-11 c2 through the product: a snapshot is not published (nor listed) at `hold-capture-before-publish`."""
    s, vm = ctx.sess, ctx.vm
    ctx.stack.up(load=False)
    s.ckpt("pA", "pa0")
    eng(vm, "rm -f /run/marsh-test-holds/capture-before-publish.reached /run/marsh-test-holds/capture-before-publish.release")
    before = set(pool_ids(vm))
    first = background_checkpoint(ctx, "pa")
    try:
        if not check("publish/held-before-publish", agent_hold(ctx, "capture-before-publish"), "capture-before-publish.reached"):
            return
        during = set(pool_ids(vm))
        staging = eng(vm, "ls -A /pool/snapshots | grep -c '^\\.tmp-'")
        check("publish/c2-not-published-while-held", during == before and staging.strip() == "1", f"published {len(before)} -> {len(during)}, staging dirs {staging.strip()}")
    finally:
        agent_release(ctx, "capture-before-publish")
    out, err = first.communicate(timeout=120)
    doc = json.loads(out) if out.strip() else {}
    new = (doc.get("services") or {}).get("snapshot")
    check("publish/published-after-release-and-recorded", bool(new) and new in pool_ids(vm),
          f"snapshot {new}; services={json.dumps(doc.get('services'))[:300]}")


# ---------------------------------------------------------------- HF2: journal, fence, exclusion, recovery, manifest


def journal_paths(s: Session) -> tuple[Path, Path]:
    return s.root / "mh" / "services-last-capture.json", s.root / "mh" / "services-last-capture.lost"


def journal_entry(s: Session, root: Path) -> dict | None:
    file, _ = journal_paths(s)
    if not file.exists():
        return None
    projects = json.loads(file.read_text()).get("projects", {})
    return next((v for k, v in projects.items() if os.path.realpath(k) == os.path.realpath(root)), None)


def stop_outside(ctx: Ctx) -> None:
    """`sbx stop` of the project's VM (outside marsh), after its UUID is verified."""
    verify_owned(ctx.vm)
    run(["sbx", "stop", ctx.vm])
    wait_until(lambda: inventory()[ctx.vm]["status"] != "running", 60)


def journal_stale(ctx: Ctx, silent: bool = False) -> None:
    """Item 1: a stale journal file that survives a failed write is not trusted. A `proven` stop outcome is in the
    file; the VM then starts with the journal unwritable (its clearing cannot be persisted, only the marker); the
    VM runs, writes and is stopped outside marsh; after a restart the checkpoint must say marsh lost its record."""
    s, vm = ctx.sess, ctx.vm
    relaunch_ready(ctx, ())
    ctx.stack.up(load=False)
    c1 = s.ckpt("pA", "jst")
    stopped = idle_stop(ctx)
    if not check("journal/stale-idle-stopped", bool(stopped) and stopped["state"] == "stopped" and services_of(c1).get("snapshot") is not None, f"state {stopped and stopped['state']}"):
        return
    f1 = (stopped.get("last_capture") or {}).get("id")
    before = journal_entry(s, ctx.pa) or {}
    check("journal/stale-setup-the-file-holds-a-proven-stop", ((before.get("stop") or {}).get("workspaces", {}).get("base") or {}).get("proven", {}).get("id") == f1,
          json.dumps(before.get("stop"))[:200])
    relaunch_ready(ctx, ("journal-write-fail",))  # the start clears the outcome: only the marker can be written
    file, marker = journal_paths(s)
    after = journal_entry(s, ctx.pa) or {}
    check("journal/stale-old-file-remains-and-the-marker-is-written", marker.exists() and after.get("stop") == before.get("stop") and after.get("stop") is not None,
          f"marker={marker.exists()} stop still in the file={after.get('stop') is not None}")
    prod(vm, "start", ctx.stack.db, check_rc=False)
    wait_until(lambda: pg_count(vm, ctx.stack.db, "select 1") == "1", 60, 1)
    prod(vm, "exec", ctx.stack.db, "psql", "-U", "postgres", "-d", "app", "-q", "-c", "insert into markers(name) select 'post-stale-' || g from generate_series(1,25) g", check_rc=False)
    stop_outside(ctx)
    s.relaunch(("journal-loss-silent",) if silent else (), s.env_extra)
    r = s.ckpt("pA", "jst-after")
    svc = services_of(r)
    check("journal/c1-stale-file-with-marker-is-unknown-not-proven",
          r[0] == 0 and bool(svc.get("reused_from")) and any("lost its record" in g for g in svc.get("gaps", [])),
          f"reused_from={svc.get('reused_from')} gaps={svc.get('gaps')}")
    restart_vm(ctx)


def journal_neither(ctx: Ctx, ignored: bool = False) -> None:
    """Item 1: when neither the outcome nor an invalidation can be persisted, a capture is a gap (and its snapshot is
    dropped) and the stop is refused; nothing is left frozen."""
    s, vm = ctx.sess, ctx.vm
    relaunch_ready(ctx, ("journal-write-fail", "journal-marker-fail") + (("journal-neither-ignored",) if ignored else ()))
    ctx.stack.up(load=True)
    before = pool_ids(vm)
    r = s.ckpt("pA", "jn")
    svc = services_of(r)
    time.sleep(1)
    check("journal/neither-a-capture-is-a-gap-and-no-unrecorded-snapshot",
          r[0] == 0 and svc.get("snapshot") is None and any("journal" in g for g in svc.get("gaps", [])) and pool_ids(vm) == before,
          f"snapshot={svc.get('snapshot')} gaps={svc.get('gaps')} pool {len(before)} -> {len(pool_ids(vm))}")
    tokens = ctx.stack.tokens()
    res = s.cli("services", "stop", cwd=ctx.pa, timeout=300)
    time.sleep(2)
    state = (s.svc("pA") or {}).get("state")
    check("journal/c1-neither-persistable-refuses-the-stop", res.returncode != 0 and state == "ready" and "refused" in (res.stdout + res.stderr),
          f"rc={res.returncode} state={state} {(res.stdout + res.stderr).strip()[:160]}")
    if state != "ready":
        relaunch_ready(ctx, ())  # the daemon still cannot write its journal: a clean one starts the VM again
        return
    time.sleep(2)
    check("journal/neither-refused-stop-leaves-nothing-frozen-and-the-writer-running", paused(vm) == [] and ctx.stack.tokens() > tokens,
          f"paused={paused(vm)} tokens {tokens} -> {ctx.stack.tokens()}")


def journal_crash(ctx: Ctx, intent_off: bool = False) -> None:
    """Item 1: the daemon dies at the journal boundary (after the marker, before the journal is rewritten). A restart
    treats the journal as unknown (the marker), never as what the old file says."""
    s, vm = ctx.sess, ctx.vm
    relaunch_ready(ctx, ())
    ctx.stack.up(load=False)
    s.ckpt("pA", "jc")
    s.cli("services", "stop", cwd=ctx.pa, timeout=300)  # a complete cutover: nothing of an earlier scenario is suspect any more
    restart_vm(ctx)
    entry = journal_entry(s, ctx.pa) or {}
    check("journal/crash-setup-the-entry-is-trusted", entry.get("suspect") is False and entry.get("stop") is None, f"suspect={entry.get('suspect')} stop={entry.get('stop')}")
    relaunch_ready(ctx, ("journal-crash-after-intent",) + (("journal-intent-off",) if intent_off else ()))
    s.cli("services", "stop", cwd=ctx.pa, timeout=300)  # the daemon dies in the cutover
    died = wait_until(lambda: s.proc.poll() is not None, 60, 1)
    _, marker = journal_paths(s)
    check("journal/crash-the-daemon-died-at-the-boundary", bool(died), f"exit {s.proc.poll()}")
    check("journal/c1-crash-at-the-boundary-leaves-the-marker", marker.exists(), f"marker={marker.exists()}")
    s.launch((), s.env_extra)
    wait_until(lambda: (s.svc("pA") or {}).get("relay", {}).get("state") == "ready", 180, 2)
    stop_outside(ctx)
    s.relaunch((), s.env_extra)
    r = s.ckpt("pA", "jc-after")
    svc = services_of(r)
    check("journal/c1-restart-after-a-crash-at-the-boundary-is-unknown",
          r[0] == 0 and bool(svc.get("reused_from")) and any("lost its record" in g for g in svc.get("gaps", [])),
          f"reused_from={svc.get('reused_from')} gaps={svc.get('gaps')}")
    restart_vm(ctx)
    if ctx.events:
        ctx.events.close()
    ctx.events = Events(ctx.vm)


def daemon_hold_reached(s: Session, point: str, seconds: float = 120) -> bool:
    return bool(wait_until(lambda: (s.root / "mh" / "test-holds" / f"{point}.reached").exists(), seconds, 0.3))


def daemon_release(s: Session, point: str) -> None:
    holds = s.root / "mh" / "test-holds"
    holds.mkdir(parents=True, exist_ok=True)
    (holds / f"{point}.release").write_text("")


def clear_daemon_holds(s: Session) -> None:
    holds = s.root / "mh" / "test-holds"
    if holds.is_dir():
        for name in holds.iterdir():
            if name.suffix in (".reached", ".release"):
                name.unlink(missing_ok=True)


FENCE_JOB = r"""
docker rm -f fence-victim >/dev/null 2>&1
docker run -d --name fence-victim postgres:17 sleep 3000 >/dev/null 2>&1 || { echo "NOVICTIM"; exit 1; }
echo READY
while [ ! -e ./.fence-go ]; do sleep 0.2; done
i=0
while [ $i -lt 700 ]; do
  i=$((i+1))
  out=$(docker unpause fence-victim 2>&1); rc=$?
  echo "U $i rc=$rc $(echo $out | tr '\n' ' ')"
  sleep 0.2
done
"""


class Job:
    """A real `marsh run nfx exec sh -c SCRIPT` job (its Docker endpoint is the project's filtered endpoint); its stdout
    lines, each with the time it arrived."""

    def __init__(self, s: Session, cwd: Path, script: str):
        self.p = subprocess.Popen([str(s.prefix / "bin" / "marsh"), "run", "nfx", "exec", "sh", "-c", script], cwd=cwd, env=s.env,
                                  stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, bufsize=1)
        self.lines: list[tuple[float, str]] = []
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self) -> None:
        for line in self.p.stdout:
            self.lines.append((time.monotonic(), line.rstrip("\n")))

    def has(self, word: str) -> bool:
        return any(word in text for _, text in self.lines)

    def close(self) -> None:
        if self.p.poll() is None:
            self.p.terminate()  # our own child
            try:
                self.p.wait(timeout=20)
            except subprocess.TimeoutExpired:
                self.p.kill()


def fence_window(job: Job, label: str, victim: str, vm: str) -> None:
    """For three seconds the job keeps trying to unpause its container through the endpoint: every attempt in the window must
    be refused (the freeze gate), none may succeed, and the container is still paused."""
    began = time.monotonic()
    time.sleep(3.0)
    attempts = [text for at, text in job.lines if at >= began and text.startswith("U ")]
    refused = [a for a in attempts if "rc=0" not in a and "checkpoint is being taken" in a]
    succeeded = [a for a in attempts if " rc=0 " in a]
    state = prod(vm, "inspect", "-f", "{{.State.Status}}", victim, check_rc=False).strip()
    check(f"fence/{label}-unpause-through-the-endpoint-is-refused",
          len(attempts) >= 3 and len(refused) == len(attempts) and not succeeded and state == "paused",
          f"{len(attempts)} attempts, {len(refused)} refused, {len(succeeded)} succeeded; container {state}; e.g. {attempts[:1]}")


def fence_windows(ctx: Ctx) -> None:
    """Item 2: the write fence through the stop. A job (through the real filtered endpoint) tries to unpause its container
    in both windows of the cutover: after a workspace capture, and after the engine count. Both are refused."""
    s, vm = ctx.sess, ctx.vm
    clear_daemon_holds(s)
    ctx.stack.up(load=True)
    job = Job(s, ctx.pa, FENCE_JOB)
    stop = None
    try:
        ready = wait_until(lambda: job.has("READY") or job.has("NOVICTIM") or job.p.poll() is not None, 900, 2)
        if not check("fence/job-runs-with-its-container", bool(ready) and job.has("READY"), str([t for _, t in job.lines][-3:])):
            return
        s.record_owned_map()
        victim = prod(vm, "inspect", "-f", "{{.Id}}", "fence-victim").strip()
        stop = subprocess.Popen([str(s.prefix / "bin" / "marsh"), "services", "stop"], cwd=ctx.pa, env=s.env,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        for point, label in (("cutover-after-capture", "c1-after-a-workspace-capture"), ("cutover-after-count", "c2-after-the-count")):
            if not check(f"fence/{label}-cutover-held", daemon_hold_reached(s, point), f"{point}.reached"):
                break
            if point == "cutover-after-capture":
                (ctx.pa / ".fence-go").write_text("")  # the job starts trying only now (not while the engine is being paused)
            fence_window(job, label, victim, vm)
            if point == "cutover-after-count":
                # The job's next request after the stop would start the VM again (a project's endpoint creates its VM lazily):
                # it is ended before the cutover is let go.
                job.close()
                time.sleep(3)
            daemon_release(s, point)
        out, _ = stop.communicate(timeout=300)
        check("fence/services-stop-completes", stop.returncode == 0, (out or "").strip()[-160:])
        r = s.ckpt("pA", "fence-stopped")
        svc = services_of(r)
        check("fence/c3-stopped-checkpoint-is-proven-and-the-container-never-ran", bool(svc.get("reused_from")) and svc.get("gaps") == [],
              f"reused_from={svc.get('reused_from')} gaps={svc.get('gaps')}")
    finally:
        for point in ("cutover-after-capture", "cutover-after-count"):
            daemon_release(s, point)
        if stop is not None and stop.poll() is None:
            try:
                stop.communicate(timeout=120)
            except subprocess.TimeoutExpired:
                stop.kill()
        job.close()
        (ctx.pa / ".fence-go").unlink(missing_ok=True)
        s.record_owned_map()
        clear_daemon_holds(s)
        restart_vm(ctx)


def engine_query_fault(ctx: Ctx) -> None:
    """Item 2: an engine query that fails (`fault-engine-query`: the count cannot reach the engine) is a gap, never `proven`."""
    s = ctx.sess
    ctx.stack.up(load=False)
    res = s.cli("services", "stop", cwd=ctx.pa, timeout=300)
    check("enginequery/stop-completes", res.returncode == 0, (res.stdout + res.stderr).strip()[:120])
    r = s.ckpt("pA", "eq-stopped")
    svc = services_of(r)
    check("enginequery/c1-a-failed-engine-query-is-a-gap-not-proven",
          r[0] == 0 and svc.get("reused_from") is None and svc.get("snapshot") is None and any("engine could not be asked" in g for g in svc.get("gaps", [])),
          f"reused_from={svc.get('reused_from')} gaps={svc.get('gaps')}")
    restart_vm(ctx)


def stop_exclusion(ctx: Ctx) -> None:
    """Item 3: a checkpoint capture in flight (held in the agent) keeps both explicit stops from happening. `services stop`
    waits its (shortened) time for the capture exclusion and then refuses; the VM keeps running. `marsh stop` never reaches
    the services teardown: the daemon's lifecycle admission refuses it while the checkpoint request is in flight (the
    teardown's own exclusion is a second line behind that, which no held capture can reach)."""
    s, vm = ctx.sess, ctx.vm
    s.env_extra["MARSH_SERVICES_STOP_WAIT_SECONDS"] = "5"
    try:
        s.relaunch(s.breaks, s.env_extra)
        ready_with(ctx, s.breaks)
        for path in ("services-stop", "marsh-stop"):
            ctx.stack.up(load=False)
            eng(vm, "rm -f /run/marsh-test-holds/capture-frozen.reached /run/marsh-test-holds/capture-frozen.release")
            first = background_checkpoint(ctx, f"excl-{path}")
            try:
                if not check(f"exclusion/{path}-capture-held", agent_hold(ctx, "capture-frozen"), "capture-frozen.reached"):
                    return
                if path == "services-stop":
                    res = s.cli("services", "stop", cwd=ctx.pa, timeout=300)
                    out = (res.stdout + res.stderr).strip()
                    state = (s.svc("pA") or {}).get("state")
                    check("exclusion/c1-services-stop-is-refused-under-a-held-capture",
                          res.returncode != 0 and state == "ready" and "capture" in out, f"rc={res.returncode} state={state} {out[:140]}")
                else:
                    res = s.cli("stop", cwd=s.root, timeout=300)
                    out = (res.stdout + res.stderr).strip()
                    row = inventory().get(vm) or {}
                    check("exclusion/c2-marsh-stop-is-refused-under-a-held-capture",
                          res.returncode != 0 and s.proc.poll() is None and row.get("id") == OWNED.get(vm) and row.get("status") == "running",
                          f"rc={res.returncode} daemon alive={s.proc.poll() is None} vm={row.get('status')} {out[:160]}")
            finally:
                try:
                    agent_release(ctx, "capture-frozen")
                except Exception:  # noqa: BLE001  (a defect that stopped the VM: nothing left to release)
                    pass
            try:
                first.communicate(timeout=180)
            except subprocess.TimeoutExpired:
                first.kill()
            if (inventory().get(vm) or {}).get("status") != "running" or (s.svc("pA") or {}).get("state") != "ready":
                restart_vm(ctx)
            wait_until(lambda: (s.svc("pA") or {}).get("relay", {}).get("state") == "ready", 180, 2)
            if ctx.events:
                ctx.events.close()
            ctx.events = Events(vm)
    finally:
        s.env_extra.pop("MARSH_SERVICES_STOP_WAIT_SECONDS", None)


def lane_orphan_recovery(ctx: Ctx) -> None:
    """Item 4: the reviewer's reproduction. A lane's snapshot is journalled before its checkpoint record exists; the
    daemon is killed (exact pid), the test's own lane checkpoint directory is removed (the record is lost), the daemon
    restarts: the snapshot is collected within the window, and the live base snapshot is kept."""
    s, vm = ctx.sess, ctx.vm
    ctx.stack.up(load=False)
    base = s.ckpt("pA", "lo-base")
    bsnap = services_of(base).get("snapshot")
    split = open_lanes(ctx)
    if split is None:
        return
    try:
        drop_lane(vm, "lanea")
        lane_writer(vm, f"fork-{split}-a", "lanea", "recovery")
        cl = s.ckpt("pA", "lo-lane", lane=f"{split}/a")
        lsnap = services_of(cl).get("snapshot")
        if not check("laneorphan/lane-checkpoint", bool(lsnap and bsnap), cl[3][:100]):
            return
        s.kill()
        shutil.rmtree(cl[1]["dir"])  # the test's own lane checkpoint directory: its record will not survive the restart
        began = time.monotonic()
        s.launch(s.breaks, s.env_extra)
        check("laneorphan/the-record-is-gone", not [c for c in s.listed("pA") if c["checkpoint"] == cl[1]["checkpoint"]], "no record of the lane checkpoint")
        found = wait_found_at(lambda: lsnap not in pool_ids(vm), began, ORPHAN_COLLECTION_LIMIT_S)
        check("laneorphan/c1-snapshot-of-the-lost-lane-record-is-collected", collected_in_time(found),
              f"lane snapshot {lsnap[:8]} " + (f"collected {found:.0f}s after the restart" if found is not None else f"still present {time.monotonic() - began:.0f}s after the restart"))
        check("laneorphan/the-live-base-snapshot-is-kept", snapshot_present(vm, bsnap), f"base snapshot {bsnap[:8]}")
    finally:
        drop_lane(vm, "lanea")
        s.cli("split", "cancel", split, cwd=ctx.pa)
        wait_until(lambda: (s.svc("pA") or {}).get("relay", {}).get("state") == "ready", 180, 2)
        if ctx.events:
            ctx.events.close()
        ctx.events = Events(vm)


def malformed_manifest(ctx: Ctx) -> None:
    """Item 5: the real agent publishes a manifest naming a volume it does not list (`manifest-dangling-ref`): the
    record is a capture gap, never an apparently complete record, and the published snapshot is dropped again."""
    s, vm = ctx.sess, ctx.vm
    ctx.stack.up(load=False)
    before = pool_ids(vm)
    r = s.ckpt("pA", "mm")
    svc = services_of(r)
    time.sleep(2)
    check("manifest/c1-dangling-reference-is-a-gap-not-a-complete-record",
          r[0] == 0 and svc.get("snapshot") is None and any("manifest was refused" in g for g in svc.get("gaps", [])),
          f"rc={r[0]} snapshot={svc.get('snapshot')} gaps={svc.get('gaps')}")
    listed = [c for c in s.listed("pA") if c["checkpoint"] == (r[1] or {}).get("checkpoint")]
    check("manifest/c1-checkpoints-json-agrees", bool(listed) and (listed[0].get("services") or {}).get("snapshot") is None, "the stored record has no snapshot")
    check("manifest/the-refused-snapshot-is-dropped", set(pool_ids(vm)) <= set(before) | live_set(s, "pA"), f"pool {len(before)} -> {len(pool_ids(vm))}")


# ---------------------------------------------------------------- break cells


def expect_break(mode: str, failing: str, before: list[str], allowed: tuple[str, ...]) -> None:
    """A cell verifies only when the named check failed, and every OTHER new failure was declared
    in advance as a consequence of the defect. Anything else (a failed attestation, a failed setup
    check, an exception) stays in FAILURES and fails the matrix."""
    new = list(FAILURES)
    for name in before:
        if name in new:
            new.remove(name)
    others = [f for f in new if f != failing and f not in allowed]
    if failing in new and not others:
        for name in new:
            FAILURES.remove(name)
        say("PASS", f"break/{mode}", f"fails at {failing} as it must" + (f" (declared consequences: {[f for f in new if f != failing]})" if len(new) > 1 else ""))
    else:
        say("FAIL", f"break/{mode}", f"{failing} {'failed' if failing in new else 'did NOT fail'}; undeclared failures: {others}")


AGENT_BREAKS = {
    "fault-capture-fail-after-pause", "capture-skip-volumes", "capture-all-workspaces", "capture-skip-unpause-on-error",
    "slow-freeze", "hold-capture-frozen", "hold-capture-before-publish", "snapshot-publish-early",
    "space-check-stale", "fault-pool-unreadable", "unreadable-pool-ok", "manifest-dangling-ref",
}


def ready_with(ctx: Ctx, breaks: tuple[str, ...]) -> None:
    """Wait for the relay, and require the agent to report exactly the agent-side modes asked for."""
    s = ctx.sess
    wait_until(lambda: (s.svc("pA") or {}).get("relay", {}).get("state") == "ready", 120, 2)
    # A relaunch starts a new engine generation: the old event stream ended with the old engine.
    if ctx.events:
        ctx.events.close()
    ctx.events = Events(ctx.vm)
    doc = s.svc("pA") or {}
    want = sorted(m for m in breaks if m in AGENT_BREAKS)
    got = sorted(doc.get("test_breaks_agent", []))
    check("marshd/agent-modes-attested", got == want, f"agent reports {got}, expected {want}")


def relaunch_ready(ctx: Ctx, breaks: tuple[str, ...]) -> None:
    s = ctx.sess
    set_idle(ctx, 900)  # before the new daemon arms its idle timers: no cell inherits an earlier cell's timeout
    s.relaunch(breaks, s.env_extra)
    if (s.svc("pA") or {}).get("state") == "stopped":
        s.cli("services", "start", cwd=ctx.pa, timeout=1800)  # an earlier cell left the VM idle-stopped
    ready_with(ctx, breaks)


def daemon_log_tail(s: Session, label: str, lines: int = 25) -> None:
    """For a cell that did not verify: the daemon's own last lines (its log is removed with the run)."""
    try:
        s.log.flush()
        text = Path(s.log.name).read_text(errors="replace").splitlines()[-lines:]
    except (OSError, ValueError, AttributeError):
        return
    for line in text:
        say("INFO", f"daemon-log/{label}", line[:300])


def run_cell(ctx: Ctx, label: str, failing: str, scenario, daemon_breaks: tuple[str, ...], allowed: tuple[str, ...] = ()) -> None:
    before = list(FAILURES)
    say("INFO", f"break/{label}", f"cell: daemon modes {list(daemon_breaks)}; expect {failing} to fail")
    relaunch_ready(ctx, daemon_breaks)
    try:
        scenario(ctx)
    except Exception as error:  # noqa: BLE001
        import traceback
        traceback.print_exc()
        say("FAIL", f"break/{label}/exception", repr(error)[:200])
    expect_break(label, failing, before, allowed)
    if f"break/{label}" in FAILURES:
        daemon_log_tail(ctx.sess, label)


def run_pass2(ctx: Ctx, label: str, scenario, daemon_breaks: tuple[str, ...]) -> None:
    """Pass 2 of a cell: the fault or hold alone, no defect: the scenario must pass in full."""
    before = len(FAILURES)
    say("INFO", f"pass2/{label}", f"daemon modes {list(daemon_breaks)}")
    relaunch_ready(ctx, daemon_breaks)
    try:
        scenario(ctx)
    except Exception as error:  # noqa: BLE001
        import traceback
        traceback.print_exc()
        say("FAIL", f"pass2/{label}/exception", repr(error)[:200])
    check(f"pass2/{label}-passes-without-the-defect", len(FAILURES) == before, f"{len(FAILURES) - before} failed checks")


def cells():
    """(label, the one check it must fail at, scenario, daemon modes, declared consequences)."""
    partial = functools.partial
    return [
        ("backend-services-disconnected", "cap01/c1-services-non-null", cap01, ("backend-services-disconnected",), ()),
        ("capture-skip-volumes", "cap01/c3-sentinels", cap01, ("capture-skip-volumes",), ("cap01/c2-frozen-ms",)),
        ("capture-ensure-vm", "cap01/c5-no-vm-created", cap01_never_created, ("capture-ensure-vm",), ("cap01/c5-null",)),
        ("lane-capture-root", "cap07/c1-workspace-is-the-lane", cap07_lane, ("lane-capture-root",),
         ("cap07/c1-exactly-one-pause-pair-for-lane-a-and-none-else", "cap07/c2-holds-a-secret-only")),
        ("capture-all-workspaces", "cap07/c1-exactly-one-pause-pair-for-lane-a-and-none-else", cap07_lane, ("capture-all-workspaces",), ("cap07/c2-holds-a-secret-only",)),
        ("capture-skip-unpause-on-error", "cap02/c2-all-unpaused-within-1s", cap02_fault, ("fault-capture-fail-after-pause", "capture-skip-unpause-on-error"), ()),
        ("retention-keep-snapshots", "cap04/c1-snapshot-set-equals-live-set", cap04, ("retention-keep-snapshots",), ()),
        ("orphan-gc-off", "cap04/c4-orphan-collected-within-60s-of-relaunch", cap04, ("orphan-gc-off",),
         ("cap04/c1-snapshot-set-equals-live-set", "cap04/c6-pool-within-64MiB-of-baseline")),
        ("admit-ignore-pool/cap05", "cap05/c1-space-gap-and-no-snapshot", cap05, ("admit-ignore-pool",), ()),
        ("reuse-last-snapshot-off", "cap08/c1-reused-from-set-no-wake", cap08, ("reuse-last-snapshot-off",), ("cap08/c2-reused-from-is-F-not-C1", "cap08/c1-proven-reuse-carries-no-gap")),
        ("idle-final-capture-off", "cap08/c2-final-capture-is-last-capture", cap08, ("idle-final-capture-off",),
         ("cap08/c2-reused-from-is-F-not-C1", "cap08/c1-proven-reuse-carries-no-gap", "cap08/c3-F-holds-the-post-c1-rows")),
        ("reuse-lease-off", "cap08/c3-F-survives-collection-while-C2-C3-name-it", cap08, ("reuse-lease-off",), ()),
        ("stop-pause-off", "fresh/c2-no-write-between-the-final-capture-and-the-stop", fresh_stop, ("stop-pause-off",), ()),
        ("stop-fence-off", "fence/c1-after-a-workspace-capture-unpause-through-the-endpoint-is-refused", fence_windows,
         ("hold-cutover-after-capture", "hold-cutover-after-count", "stop-fence-off"),
         ("fence/c2-after-the-count-unpause-through-the-endpoint-is-refused", "fence/c3-stopped-checkpoint-is-proven-and-the-container-never-ran")),
        ("engine-query-fail-open", "enginequery/c1-a-failed-engine-query-is-a-gap-not-proven", engine_query_fault,
         ("fault-engine-query", "engine-query-fail-open"), ()),
        ("capture-gate-ignored", "exclusion/c1-services-stop-is-refused-under-a-held-capture", stop_exclusion,
         ("hold-capture-frozen", "capture-gate-ignored"), ()),
        ("journal-loss-silent/stale", "journal/c1-stale-file-with-marker-is-unknown-not-proven", partial(journal_stale, silent=True), (), ()),
        ("journal-neither-ignored", "journal/c1-neither-persistable-refuses-the-stop", partial(journal_neither, ignored=True), (),
         ("journal/neither-a-capture-is-a-gap-and-no-unrecorded-snapshot",)),
        ("journal-intent-off", "journal/c1-restart-after-a-crash-at-the-boundary-is-unknown", partial(journal_crash, intent_off=True), (),
         ("journal/c1-crash-at-the-boundary-leaves-the-marker",)),
        ("lane-reconcile-off", "laneorphan/c1-snapshot-of-the-lost-lane-record-is-collected", lane_orphan_recovery, ("lane-reconcile-off",), ()),
        ("manifest-refs-unchecked", "manifest/c1-dangling-reference-is-a-gap-not-a-complete-record", malformed_manifest,
         ("manifest-dangling-ref", "manifest-refs-unchecked"), ("manifest/c1-checkpoints-json-agrees",)),
        ("stop-base-only", "fresh/c3-lane-stopped-checkpoint-is-not-the-stale-lane-snapshot", fresh_stop, ("stop-base-only",),
         ("fresh/c3-lane-snapshot-holds-the-lane-data-as-it-was-at-the-stop",)),
        ("reuse-ignores-failure", "stopfail/c1-final-capture-below-reserve-is-a-gap-not-the-old-snapshot", stop_failure, ("reuse-ignores-failure",),
         ("stopfail/c1-the-gap-survives-a-daemon-restart",)),
        ("admit-ignore-pool/stop", "stopfail/c1-final-capture-below-reserve-is-a-gap-not-the-old-snapshot", stop_failure, ("admit-ignore-pool",),
         ("stopfail/c1-the-last-capture-is-still-the-checkpoint-one", "stopfail/c1-the-gap-survives-a-daemon-restart")),
        ("space-check-stale", "space/c1-pool-filled-during-the-hold-is-a-space-gap-at-capture-time", space_boundary,
         ("hold-checkpoint-before-space-check", "space-check-stale"), ()),
        ("admit-ignore-pool/space", "space/c1-pool-filled-during-the-hold-is-a-space-gap-at-capture-time", space_boundary,
         ("hold-checkpoint-before-space-check", "admit-ignore-pool"), ()),
        ("unreadable-pool-ok", "space/c1-unreadable-pool-is-a-gap-not-a-capture", pool_unreadable, ("fault-pool-unreadable", "unreadable-pool-ok"), ()),
        ("journal-loss-silent", "journal/c1-lost-journal-is-a-warning-reuse-not-a-silent-null", partial(journal_loss, silent=True), (), ()),
        ("journal-vm-unbound", "rmrecreate/c1-the-new-vm-has-no-snapshot-to-reuse", rm_recreate, ("journal-vm-unbound",), ()),
        ("orphan-roots-from-records", "orphan/c1-lane-only-orphan-collected-after-restart", orphan_only, ("orphan-roots-from-records",), ()),
        ("capture-admission-blocks", "conc/c3-second-checkpoint-is-not-held-behind-the-first", concurrent, ("hold-capture-frozen", "capture-admission-blocks"),
         ("conc/c3-second-result-is-the-agents-skipped-gap", "conc/c4-freeze-windows-never-overlap-one-pair-per-container")),
        ("idle-ignores-inflight", "inflight/idle-stop-waits-for-the-capture", idle_with_capture_in_flight, ("hold-capture-frozen", "idle-ignores-inflight"),
         ("inflight/idle-stop-then-proceeds-with-its-final-capture",)),
        ("snapshot-publish-early", "publish/c2-not-published-while-held", publish_atomic, ("hold-capture-before-publish", "snapshot-publish-early"), ()),
    ]


def pass2s():
    return [
        ("cap02-fault", cap02_fault, ("fault-capture-fail-after-pause",)),
        ("concurrent", concurrent, ("hold-capture-frozen",)),
        ("idle-with-capture-in-flight", idle_with_capture_in_flight, ("hold-capture-frozen",)),
        ("space-boundary", space_boundary, ("hold-checkpoint-before-space-check",)),
        ("pool-unreadable", pool_unreadable, ("fault-pool-unreadable",)),
        ("publish-atomic", publish_atomic, ("hold-capture-before-publish",)),
        ("journal-loss", journal_loss, ()),
        ("journal-stale", journal_stale, ()),
        ("journal-neither", journal_neither, ()),
        ("journal-crash", journal_crash, ()),
        ("fence-windows", fence_windows, ("hold-cutover-after-capture", "hold-cutover-after-count")),
        ("engine-query", engine_query_fault, ("fault-engine-query",)),
        ("stop-exclusion", stop_exclusion, ("hold-capture-frozen",)),
        ("lane-orphan", lane_orphan_recovery, ()),
        ("malformed-manifest", malformed_manifest, ("manifest-dangling-ref",)),
    ]


def breaks_session(ctx: Ctx, args) -> None:
    only = set(args.cells.split(",")) if args.cells else None
    for label, scenario, modes in pass2s():
        if only and f"pass2:{label}" not in only:
            continue
        run_pass2(ctx, label, scenario, modes)
    for label, failing, scenario, modes, allowed in cells():
        if only and label not in only:
            continue
        run_cell(ctx, label, failing, scenario, modes, allowed)
    # perf control: slow-freeze (an agent-side measurement control) must fail the gate
    if not only or "slow-freeze" in only:
        before = list(FAILURES)
        relaunch_ready(ctx, ("slow-freeze",))
        perf(ctx, 8)
        # With the control the agent's own budget refuses every capture ("the copy stalled past the freeze budget"):
        # the gap, wall-time and PERF-02 gates fail with it, and a failed capture leaves no complete window.
        gates = ("perf01/freeze-p95-under-500ms", "perf01/capture-wall-p95-under-1s", "perf01/gaps-and-failures-at-most-2", "perf02/p95-delta-under-1s",
                 "perf01/frozen_ms-within-50ms-of-the-event-window", "perf01/every-sample-has-an-engine-event-window", "perf02/all-n-pairs-completed")
        new = list(FAILURES)
        for name in before:
            if name in new:
                new.remove(name)
        undeclared = [f for f in new if f not in gates]
        if "perf01/freeze-p95-under-500ms" in new and not undeclared:
            for name in new:
                FAILURES.remove(name)
            say("PASS", "break/slow-freeze", f"PERF-01's freeze gate fails with the control on (also: {[f for f in new if f != 'perf01/freeze-p95-under-500ms']})")
        else:
            say("FAIL", "break/slow-freeze", f"freeze gate failed={'perf01/freeze-p95-under-500ms' in new}; undeclared failures {undeclared}")


# ---------------------------------------------------------------- main


PENDING = [
    ("CAP-02 variants c-e (hold + agent kill, daemon kill, sbx stop during a hold)", "their recovery paths are the agent's (S3) and E1's, driven by holds this script does not own"),
    ("CAP-04 c3 (held rm of a leased checkpoint) and the `open` halves of CAP-05 c3, CAP-06, CAP-07 c3, RST-*", "need C3's restore and lease holder (`split --from`); the direct snapshot reads here are NOT restore evidence"),
    ("CAP-11 c1 (mutations through the endpoint against a frozen capture)", "needs the E2 endpoint; CAP-11 c2 is run through the product (publish)"),
    ("CAP-03 c1/c2 (the 300 000-file freeze budget)", "the agent's (S3) budget; c3/c4 are run here (concurrent)"),
    ("PERF-01 stack B (Supabase)", "needs the job endpoint (E2)"),
]


def self_test(args) -> int:
    """The harness's own oracles, against synthetic inputs: a verified break never hides a failed
    attestation or setup check; a missing fixture creates nothing."""
    ok = True

    def expect(name: str, condition: bool) -> None:
        nonlocal ok
        ok &= condition
        print(f"{'PASS' if condition else 'FAIL'} self-test/{name}", flush=True)

    keep = list(FAILURES)
    FAILURES.clear()
    FAILURES.extend(["x/named", "x/declared"])
    expect_break("m", "x/named", [], ("x/declared",))
    expect("a-declared-consequence-verifies-the-cell", FAILURES == [])
    FAILURES.clear()
    FAILURES.extend(["marshd/test-breaks-attested", "x/named"])
    expect_break("m", "x/named", [], ())
    expect("a-failed-attestation-is-not-cleared-and-fails-the-cell",
           "marshd/test-breaks-attested" in FAILURES and "break/m" in FAILURES)
    FAILURES.clear()
    FAILURES.extend(["x/other"])
    expect_break("m", "x/named", [], ())
    expect("a-cell-whose-named-check-did-not-fail-is-not-verified", "break/m" in FAILURES)
    FAILURES.clear()
    FAILURES.extend(keep)
    # the absolute baseline distance (CAP-04 c6) is symmetric
    base = 900 << 20
    expect("c6-oracle-fails-145MiB-below-the-baseline", not (abs((777 << 20) - base) <= 64 << 20))
    expect("c6-oracle-passes-10MiB-away", abs((base + (10 << 20)) - base) <= 64 << 20)
    # item 6: the harness re-checks the recorded UUID and running state before EVERY direct exec (no cache)
    calls = []
    live = {"vm-a": {"id": "u1", "status": "running"}}

    def listing():
        calls.append(1)
        return live

    OWNED["vm-a"] = "u1"
    verify_owned("vm-a", listing)
    verify_owned("vm-a", listing)
    expect("every-direct-exec-reads-the-inventory-again", len(calls) == 2)

    def refused(change) -> bool:
        live["vm-a"] = {"id": "u1", "status": "running", **change}
        try:
            verify_owned("vm-a", listing)
            return False
        except RuntimeError:
            return True

    expect("a-replaced-uuid-is-refused-at-once", refused({"id": "other"}))
    expect("a-stopped-vm-is-refused-at-once", refused({"status": "stopped"}))
    live.clear()
    try:
        verify_owned("vm-a", listing)
        absent = False
    except RuntimeError:
        absent = True
    expect("an-absent-name-is-refused", absent)
    OWNED.pop("vm-a", None)
    # item 7: the oracles
    expect("c3-oracle-fails-a-1.5s-response", not answered_in_time(1.5) and answered_in_time(0.9))
    expect("c4-oracle-fails-a-missing-container-id", not pairs_cover({"a": (1, 1)}, {"a", "b"}) and pairs_cover({"a": (1, 1), "b": (1, 1)}, {"a", "b"}))
    expect("c4-oracle-fails-two-pairs-for-one-container", not pairs_cover({"a": (2, 2), "b": (1, 1)}, {"a", "b"}))
    expect("cap04-c4-oracle-fails-a-late-success-and-a-never", not collected_in_time(61.0) and not collected_in_time(None) and collected_in_time(59.0))
    captured = (0, {"services": {"snapshot": "s1", "gaps": []}})
    skipped = (0, {"services": {"snapshot": None, "gaps": ["skipped: another capture in progress"]}})
    expect("perf02-oracle-counts-only-valid-captured-pairs",
           valid_on_off_pair(captured, 0) and not valid_on_off_pair(skipped, 0) and not valid_on_off_pair(captured, 1) and not valid_on_off_pair((1, None), 0))
    # a missing fixture exits before anything is created
    before = {p.name for p in Path("/private/tmp").glob("nfx-c12-*")}
    result = subprocess.run([sys.executable, __file__, "--dev-normal", "x", "--dev-breaks", "y", "--fixtures", "/nonexistent-c12-fixtures"], capture_output=True, text=True)
    after = {p.name for p in Path("/private/tmp").glob("nfx-c12-*")}
    expect("a-missing-fixture-exits-2-and-creates-no-directory", result.returncode == 2 and before == after)
    print(f"{'OK' if ok else 'FAIL'}: self-test", flush=True)
    return 0 if ok else 1


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--session", choices=["normal", "breaks"], default="normal")
    ap.add_argument("--dev-normal")
    ap.add_argument("--dev-breaks")
    ap.add_argument("--self-test", action="store_true", help="check the harness's own oracles (no VM)")
    ap.add_argument("--fixtures", default=str(Path(__file__).resolve().parent / "services" / "fixtures"),
                    help="the services E2E fixtures (default: this checkout's)")
    ap.add_argument("--only", default="cap01,cap07,cap02,cap05,cap04,cap08,fresh,stopfail,rmrecreate,orphan,laneorphan")
    ap.add_argument("--cells", default="")
    ap.add_argument("--pool", default="3g")
    ap.add_argument("--reserve", default="1g")
    ap.add_argument("--n", type=int, default=50)
    args = ap.parse_args()
    if args.self_test:
        return self_test(args)
    if not (args.dev_normal and args.dev_breaks):
        print("FAIL --dev-normal and --dev-breaks are required", flush=True)
        return 2
    if not (Path(args.fixtures) / "stack-a" / "app" / "run.sh").is_file():
        print(f"FAIL fixtures: {args.fixtures}/stack-a/app/run.sh is missing", flush=True)
        return 2
    sess = None
    ctx = None
    try:
        sess = Session(args, args.session)
        sess.reserve_bytes = {"1g": 1 << 30, "512m": 512 << 20}.get(args.reserve, 1 << 30)
        ctx = Ctx(sess)
        setup(ctx)
        if args.session == "normal":
            steps = {"cap01": [cap01, cap01_never_created], "cap07": [cap07_lane], "cap02": [cap02_fault_normal], "cap05": [cap05],
                     "cap04": [cap04], "cap08": [cap08, cap08_null], "fresh": [fresh_stop], "stopfail": [stop_failure],
                     "rmrecreate": [rm_recreate], "orphan": [orphan_only], "laneorphan": [lane_orphan_recovery], "perf": [lambda c: perf(c, args.n)]}
            for key in args.only.split(","):
                for step in steps[key]:
                    say("INFO", f"scenario/{getattr(step, '__name__', key)}", "begin")
                    step(ctx)
        else:
            breaks_session(ctx, args)
    except Exception as error:  # noqa: BLE001
        import traceback
        traceback.print_exc()
        say("FAIL", "run/exception", repr(error)[:300])
    finally:
        if ctx and ctx.events:
            ctx.events.close()
        if ctx and ctx.stack:
            try:
                ctx.stack.down()
            except Exception:  # noqa: BLE001
                pass
        cleaned = sess.finish() if sess else True
        say("INFO" if cleaned else "FAIL", "cleanup/complete", str(cleaned))
    for what, why in PENDING:
        print(f"PENDING {what}: {why}", flush=True)
    print(f"{'FAIL' if FAILURES else 'OK'}: {len(FAILURES)} failed checks {FAILURES}")
    return 1 if FAILURES else 0


def cap02_fault_normal(ctx: Ctx) -> None:
    """CAP-02 variant a (normal completion): nothing paused afterwards."""
    s, vm = ctx.sess, ctx.vm
    ctx.stack.up(load=True)
    rc, doc, wall, text = s.ckpt("pA", "cap02a")
    check("cap02/c1-normal-completion-nothing-paused", rc == 0 and paused(vm) == [], f"paused={paused(vm)}")


if __name__ == "__main__":
    sys.exit(main())
