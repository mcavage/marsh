#!/usr/bin/env python3
"""E2 evidence: the job's filtered Docker endpoint, its port forwards, the shell's endpoint, and
`services status`, through REAL marsh jobs (the H1b `nfx` fixture Kit) on real sbx.

    vmlock.py --run e2 --shared -- python3 tests/acceptance/services_e2_endpoint.py \
        --prefix ~/.marsh-dev-E2 [--breaks-prefix ~/.marsh-dev-E2-breaks] \
        --fixtures ~/dev/mf-H1b/tests/acceptance/services/fixtures --only endpoint,ports,...

The caller holds the VM lock (`harness/vmlock.py`, `--shared` for these runs); this script never takes it.
One scope per scenario (`MARSH_VM_PREFIX=marsh-xnfx01-`, `MARSH_HOME` under /private/tmp/nfx-e2-RUN-NAME),
a daemon this script starts with `marshd --home H [--test-breaks-from-env]`, and cleanup by exact names only:
`marsh services rm --yes`, `marsh stop`, and the VMs the scope's ownership map recorded. Nothing is removed or
killed by pattern; Mark's `~/.marsh` and any other daemon are never touched. Synthetic data only.

Each scenario prints one JSON line `{"scenario", "breaks", "checks": {name: {"ok", "expected", "observed"}}, "ok"}`.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import re
import secrets
import shutil
import subprocess
import sys
import time

SCOPE_TAG = "nfx-e2"  # the scope directory prefix; services_e4_reap.py sets its own
NGINX = "nginx:1.27-alpine@sha256:65645c7bb6a0661892a8b03b89d0743208a18dd2f3f17a54ef4b76fb8e2f2a10"
BUSYBOX = "busybox:1.37@sha256:a7dbc319b1438115a207e11930dc4bb3e643dcb3ec0bdd694bb36cd8fad289ec"
ENGINE = "marsh-svc-engine"
PROD_SOCKET = "/run/marsh-svc/docker.sock"
PREFIX = "marsh-xnfx01-"


def jline(**fields) -> None:
    print(json.dumps(fields, sort_keys=True, default=str), flush=True)


class Scope:
    """One isolated marsh home with its own daemon."""

    def __init__(self, args, label: str, breaks: list[str] | None = None):
        self.args = args
        self.label = label
        self.breaks = breaks or []
        prefix = pathlib.Path(args.breaks_prefix if self.breaks else args.prefix)
        self.marsh = prefix / "bin/marsh"
        self.marshd = prefix / "bin/marshd"
        self.guest = prefix / "libexec/marsh"
        self.root = pathlib.Path("/private/tmp") / f"{SCOPE_TAG}-{args.run}-{label}"
        self.home, self.control_root, self.project = (self.root / n for n in ("home", "control", "project"))
        for path in (self.home, self.control_root, self.project):
            path.mkdir(parents=True, mode=0o700)
        self.control_home = self.control_root.resolve() / hashlib.sha256(os.fsencode(self.home.resolve())).hexdigest()
        self.control_home.mkdir(mode=0o700)
        self.project = self.project.resolve()
        stage = subprocess.run([sys.executable, str(pathlib.Path(args.fixtures) / "nfx-kit/stage.py"),
                                "--dest", str(self.root / "kit")], capture_output=True, text=True, check=True)
        shutil.copy(stage.stdout.strip(), self.control_home / "commands.json")
        self.env = {k: os.environ[k] for k in ("PATH", "HOME", "TERM", "LANG", "TMPDIR", "USER", "LOGNAME") if k in os.environ}
        self.env.update({"MARSH_HOME": str(self.home), "MARSH_CONTROL_HOME": str(self.control_root),
                         "MARSH_VM_PREFIX": PREFIX, "MARSH_GUEST_ARTIFACTS": str(self.guest)})
        self.daemon: subprocess.Popen | None = None
        self.vms: set[str] = set()

    def start(self) -> None:
        env = dict(self.env)
        argv = [str(self.marshd), "--home", str(self.home)]
        if self.breaks:
            argv.append("--test-breaks-from-env")
            env["MARSH_TEST_BREAK"] = ",".join(self.breaks)
        log = (self.root / "marshd.log").open("wb")
        self.daemon = subprocess.Popen(argv, cwd=self.project, env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                                       process_group=0)
        # Wait for the endpoint of THIS daemon (never `marsh status`: the CLI would start a daemon of its own
        # and win the race, and a break mode is only honored by the explicitly launched one).
        canonical = self.home.resolve(strict=True)
        endpoint = pathlib.Path("/tmp") / f"marsh-{os.getuid()}" / hashlib.sha256(os.fsencode(canonical)).hexdigest()[:16] / "s"
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            if self.daemon.poll() is not None:
                raise RuntimeError(f"daemon exited: {(self.root / 'marshd.log').read_text(errors='replace')[-400:]}")
            if endpoint.exists() and self.run(["status", "--json"], timeout=30).returncode == 0:
                return
            time.sleep(0.2)
        raise RuntimeError("daemon did not answer")

    def run(self, argv, cwd=None, timeout=600, env=None, input=None):
        full = dict(self.env)
        full.update(env or {})
        return subprocess.run([str(self.marsh), *argv], cwd=cwd or self.project, env=full, capture_output=True, text=True,
                              timeout=timeout, input=input)

    def popen(self, argv, cwd=None, env=None, **kw):
        full = dict(self.env)
        full.update(env or {})
        return subprocess.Popen([str(self.marsh), *argv], cwd=cwd or self.project, env=full, process_group=0, **kw)

    def enable_services(self, project: pathlib.Path) -> None:
        """Turns services on for the project, starts its VM and loads the pinned images into its engine from the
        harness's cache (image_cache.py, a documented setup write that never goes through a job's endpoint): the
        scenarios must not depend on Docker Hub's anonymous rate limit."""
        done = self.run(["config", "services", "on"], cwd=project)
        assert done.returncode == 0, done.stderr
        started = self.run(["services", "start"], cwd=project, timeout=1800)
        assert started.returncode == 0, started.stderr
        vm = self.svc_vm(project)
        cache = pathlib.Path(self.args.fixtures)
        images = pathlib.Path(self.args.images) if getattr(self.args, "images", None) else cache.parents[3] / "target/nfx/images"
        loaded = subprocess.run([sys.executable, str(cache / "image_cache.py"), "load", "--vm", vm, "--sudo", "--engine", ENGINE,
                                 "--socket", f"unix://{PROD_SOCKET}", "--out", str(images), "--record", str(self.root / "image-loads.jsonl"),
                                 "--sbx", self.args.sbx], capture_output=True, text=True, timeout=1800,
                                env={k: v for k, v in os.environ.items() if k != "DOCKER_HOST"})
        assert loaded.returncode == 0, loaded.stderr[-400:]

    def job(self, script: str, cwd=None, env=None, timeout=900):
        """`marsh run nfx exec sh -c SCRIPT`: a real Kit job; returns (code, stdout, stderr)."""
        done = self.run(["run", "nfx", "exec", "sh", "-c", script], cwd=cwd, env=env, timeout=timeout)
        return done.returncode, done.stdout, done.stderr

    def services_status(self, cwd=None) -> dict:
        done = self.run(["services", "status", "--json"], cwd=cwd, timeout=120)
        assert done.returncode == 0, done.stderr
        return json.loads(done.stdout)

    def project_entry(self, cwd=None) -> dict:
        root = str((cwd or self.project).resolve())
        return next(p for p in self.services_status(cwd)["projects"] if p["root"] == root)

    def svc_vm(self, cwd=None) -> str:
        vm = self.project_entry(cwd).get("vm")
        assert vm, "no services VM"
        self.vms.add(vm)
        return vm

    def vx(self, command: str, cwd=None, timeout=120, vm=None) -> subprocess.CompletedProcess:
        """Read-only observation of the production engine (H-7 `vx`)."""
        vm = vm or self.svc_vm(cwd)
        inner = f"export DOCKER_HOST=unix://{PROD_SOCKET}; {command}"
        return subprocess.run([self.args.sbx, "exec", vm, "sudo", "docker", "exec", ENGINE, "sh", "-c", inner],
                              capture_output=True, text=True, timeout=timeout)

    def owned(self) -> set[str]:
        try:
            return set((json.loads((self.control_home / "vm-ownership.json").read_text()).get("vms") or {}).keys())
        except (OSError, ValueError):
            return set()

    def cleanup(self) -> list[str]:
        errors: list[str] = []
        recorded = self.owned() | self.vms
        try:
            if True:
                # every services VM this scope's ownership map records, by its project root
                document = json.loads((self.control_home / "vm-ownership.json").read_text())
                for name, entry in (document.get("vms") or {}).items():
                    if isinstance(entry, dict) and entry.get("purpose") == "services" and pathlib.Path(entry["key"]).is_dir():
                        removed = self.run(["services", "rm", "--yes"], cwd=pathlib.Path(entry["key"]), timeout=900)
                        if removed.returncode != 0:
                            errors.append(f"services rm {entry['key']}: {removed.stderr.strip()[-200:]}")
        except Exception as error:  # noqa: BLE001
            errors.append(f"services rm: {error}")
        try:
            stopped = self.run(["stop"], timeout=600)
            if stopped.returncode != 0:
                errors.append(f"marsh stop: {stopped.stderr.strip()[-200:]}")
        except Exception as error:  # noqa: BLE001
            errors.append(f"marsh stop: {error}")
        if self.daemon is not None:
            try:
                self.daemon.wait(timeout=30)
            except subprocess.TimeoutExpired:
                errors.append("daemon still running after stop")
        recorded |= self.owned()
        listed = subprocess.run([self.args.sbx, "ls", "--json"], capture_output=True, text=True, timeout=120)
        try:
            names = {entry["name"] for entry in json.loads(listed.stdout)}
        except ValueError:
            names = set()
            errors.append("sbx ls unreadable")
        left = sorted(recorded & names)
        if left:
            errors.append(f"owned VMs remain: {left}")
        return errors


def check(ok: bool, expected, observed) -> dict:
    return {"ok": bool(ok), "expected": expected, "observed": observed}


def poll(fn, ceiling=120.0, interval=1.0):
    deadline = time.monotonic() + ceiling
    last = None
    while time.monotonic() < deadline:
        last = fn()
        if last:
            return last
        time.sleep(interval)
    return last


def stage_fixture(args, name: str, dest: pathlib.Path, *overlays: str) -> pathlib.Path:
    sys.path.insert(0, str(pathlib.Path(args.fixtures)))
    import stage  # noqa: PLC0415  (H1b's stager)
    return stage.stage(name, dest, tuple(overlays)).resolve()


SCENARIOS: dict = {}


def scenario(name):
    def register(fn):
        SCENARIOS[name] = fn
        return fn
    return register


ENVGREP = "env | grep -E '^(DOCKER_|TESTCONTAINERS_)' | sort"


@scenario("endpoint")
def endpoint(args, scope: Scope) -> dict:
    """EP-01 c1-c3 and c5, SEC-08 c1, VM-01 c6: compose from a real job, the socket, the env, services off."""
    checks: dict = {}
    scope.start()
    off = (scope.root / "project-off")
    off.mkdir()
    off = off.resolve()
    code, out, err = scope.job("ls -l /run/marsh; echo ---; " + ENVGREP + "; echo ---; "
                               "test -e /run/marsh/docker.sock && echo SOCKET-PRESENT || echo no-socket", cwd=off)
    parts = out.split("---")
    checks["off_no_socket_no_env"] = check(
        code == 0 and "docker.sock" not in parts[0] and "SOCKET-PRESENT" not in out and len(parts) > 1 and not parts[1].strip(),
        "no docker.sock in /run/marsh, no DOCKER_*/TESTCONTAINERS_*", {"exit": code, "out": out[-500:], "err": err[-200:]})
    project = stage_fixture(args, "stack-a", scope.project / "app")
    scope.enable_services(project)
    compose_hash = hashlib.sha256((project / "compose.yaml").read_bytes()).hexdigest()
    script = ("id -u; stat -c 'socket %F %u %a' /run/marsh/docker.sock; "
              "test ! -e /var/run/docker.sock && test ! -e /run/docker.sock && echo no-standard-sockets; "
              "echo ID=$(docker info --format '{{.ID}}'); " + ENVGREP + "; "
              "docker compose up -d --wait 2>&1 | tail -3; echo UP-EXIT=$?; "
              "echo PSQL=$(docker compose exec -T db psql -U postgres -d app -Atc 'select 1'); "
              "(env -u DOCKER_HOST docker info >/dev/null 2>&1 && echo ALT-DEFAULT-OK || echo alt-default-fails); "
              "(DOCKER_HOST=unix:///var/run/docker.sock docker info >/dev/null 2>&1 && echo ALT-EXPLICIT-OK || echo alt-explicit-fails)")
    t0 = time.monotonic()
    code, out, err = scope.job(script, cwd=project, timeout=1500)
    elapsed = round(time.monotonic() - t0, 1)
    lines = out.strip().splitlines()
    user = lines[0] if lines else ""
    probe = scope.vx("docker info --format '{{.ID}}'", cwd=project)
    prod_id = probe.stdout.strip()
    checks["job_ran"] = check(code == 0, "exit 0", {"exit": code, "stderr": err[-400:], "tail": lines[-10:], "seconds": elapsed})
    sockets = [line for line in lines if line.startswith("socket ")]
    checks["sec08_c1_socket_mode_owner"] = check(sockets == [f"socket socket {user} 600"], f"socket socket <job uid {user}> 600", sockets)
    checks["no_standard_sockets"] = check("no-standard-sockets" in lines, True, lines[:6])
    checks["ep01_c1_engine_is_the_production_engine"] = check(bool(prod_id) and f"ID={prod_id}" in lines, f"ID={prod_id}",
                                                              {"job": [l for l in lines if l.startswith("ID=")],
                                                               "vx_stderr": probe.stderr[-300:]})
    env_lines = sorted(l for l in lines if l.startswith(("DOCKER_", "TESTCONTAINERS_")))
    checks["ep01_c2_env_exact"] = check(env_lines == ["DOCKER_HOST=unix:///run/marsh/docker.sock",
                                                     "TESTCONTAINERS_HOST_OVERRIDE=localhost"], "exactly the two", env_lines)
    checks["ep01_c3_up_then_exec"] = check("PSQL=1" in lines, "PSQL=1", [l for l in lines if l.startswith(("PSQL", "UP-EXIT"))])
    checks["sec08_c1_alt_engines_fail"] = check("alt-default-fails" in lines and "alt-explicit-fails" in lines, True,
                                                [l for l in lines if "alt-" in l])
    checks["ep01_c5_compose_unchanged"] = check(
        hashlib.sha256((project / "compose.yaml").read_bytes()).hexdigest() == compose_hash, compose_hash, "same")
    listing = scope.vx("docker ps --format '{{.Names}}' | sort", cwd=project)
    names = listing.stdout.split()
    checks["vx_sees_the_stack"] = check(any(n.endswith("-db-1") for n in names), "a db container in the services engine",
                                        {"names": names, "stderr": listing.stderr[-300:]})
    return checks



def pg(sql: str) -> str:
    return f"PGPASSWORD=devpass psql -h localhost -U postgres -d app -Atc \"{sql}\""


COMPOSE_UP = "docker compose up -d --wait 2>&1 | tail -2"
# A nonce a shell can read without psql (the shell image has curl, not psql): a message in the project's own mailpit.
SET_NONCE = ("curl -s -X POST localhost:8025/api/v1/send -H 'Content-Type: application/json' "
             "-d '{{\"From\":{{\"Email\":\"a@b.c\"}},\"To\":[{{\"Email\":\"d@e.f\"}}],\"Subject\":\"nonce-{tag}\"}}' >/dev/null")
GET_NONCE = "curl -s localhost:8025/api/v1/messages | grep -o 'nonce-[a-z0-9]*' | head -1"


@scenario("ports")
def ports(args, scope: Scope) -> dict:
    """EP-01 c4, EP-02 (c1-c7), SEC-08 c3: forwards follow the containers; collisions are reported."""
    checks: dict = {}
    scope.start()
    project = stage_fixture(args, "stack-a", scope.project / "app")
    scope.enable_services(project)
    code, out, err = scope.job(COMPOSE_UP, cwd=project, timeout=1500)
    checks["compose_up_job"] = check(code == 0, "exit 0", {"exit": code, "out": out[-300:], "err": err[-300:]})
    # EP-01 c4: a NEW job, no docker call, reaches the stack on localhost
    script = ("for i in $(seq 1 90); do n=$(" + pg("select count(*) from items") + " 2>/dev/null); "
              "[ \"$n\" = 1000 ] && break; sleep 1; done; echo ITEMS=$n; "
              "echo REDIS=$(redis-cli -h localhost ping); echo MAIL=$(curl -sf localhost:8025/api/v1/info | jq -r .Version); "
              "ss -ltnH | tr -s ' ' | cut -d' ' -f4 | sort | tr '\\n' ' '; echo")
    code, out, err = scope.job(script, cwd=project)
    lines = out.strip().splitlines()
    checks["ep01_c4_localhost_reaches_the_stack"] = check(
        "ITEMS=1000" in lines and "REDIS=PONG" in lines and any(l.startswith("MAIL=v") for l in lines),
        "1000, PONG, a version", {"exit": code, "lines": lines, "err": err[-300:]})
    # EP-02
    ep02 = r"""
set -u
echo C1=$(echo hi | docker run -i --rm busybox cat)
docker run -d --name nw -p 18080:80 nginx >/dev/null
code=000; for i in $(seq 1 50); do code=$(curl -s -o /dev/null -w '%{http_code}' localhost:18080 || true); [ "$code" = 200 ] && break; sleep 0.1; done
echo C2=$code
echo C3=$(script -qc "docker exec -it nw sh -c 'echo \$((6*7))'" /dev/null | tr -d '\r\000' | head -1 | cut -d' ' -f1)
head -c 1048576 /dev/urandom > /tmp/in.bin; echo IN=$(sha256sum < /tmp/in.bin | cut -c1-16)
docker cp /tmp/in.bin nw:/tmp/in.bin; echo CP-IN=$(docker exec nw sha256sum /tmp/in.bin | cut -c1-16)
docker cp nw:/tmp/in.bin /tmp/out.bin; echo CP-OUT=$(sha256sum < /tmp/out.bin | cut -c1-16)
docker exec -i nw sh -c 'cat > /tmp/x' < /tmp/in.bin; echo EXEC-IN=$(docker exec nw sha256sum /tmp/x | cut -c1-16)
echo EXEC-OUT=$(docker exec nw cat /tmp/x | sha256sum | cut -c1-16)
echo C5=$(docker logs nw 2>&1 | grep -c 'GET /')
docker run -d --name nr -p 127.0.0.1::80 nginx >/dev/null
P=$(docker port nr 80/tcp | head -1 | sed 's/.*://'); sleep 0.5
echo C6=$(curl -s -o /dev/null -w '%{http_code}' localhost:$P)
t0=$(date +%s.%N); docker stop nw >/dev/null
for i in $(seq 1 60); do ss -ltnH 'sport = :18080' | grep -q . || break; sleep 0.1; done
echo C7-LISTENERS=$(ss -ltnH 'sport = :18080' | wc -l)
echo C7-SECONDS=$(python3 -c "import time;print(round(time.time()-$t0,2))")
timeout 1 nc -l 127.0.0.1 18080 </dev/null; echo C7-NC-RC=$?
docker rm -f nr nw >/dev/null
"""
    code, out, err = scope.job(ep02.replace(" nginx ", f" {NGINX} ").replace(" busybox ", f" {BUSYBOX} "), cwd=project, timeout=1500)
    kv = dict(l.split("=", 1) for l in out.splitlines() if re.match(r"^[A-Z][A-Z0-9-]*=", l))
    checks["ep02_ran"] = check(code == 0, "exit 0", {"exit": code, "err": err[-1200:], "kv": kv})
    checks["ep02_c1_stdin_through_run"] = check(kv.get("C1") == "hi", "hi", kv.get("C1"))
    checks["ep02_c2_published_port_answers"] = check(kv.get("C2") == "200", "200", kv.get("C2"))
    checks["ep02_c3_tty_exec"] = check(kv.get("C3") == "42", "42", kv.get("C3"))
    checks["ep02_c4_byte_exact"] = check(len({kv.get("IN"), kv.get("CP-IN"), kv.get("CP-OUT"), kv.get("EXEC-IN"), kv.get("EXEC-OUT")}) == 1
                                         and kv.get("IN"), "all hashes equal", {k: kv.get(k) for k in ("IN", "CP-IN", "CP-OUT", "EXEC-IN", "EXEC-OUT")})
    checks["ep02_c5_logs_show_the_get"] = check(int(kv.get("C5", "0") or 0) >= 1, ">= 1 GET", kv.get("C5"))
    checks["ep02_c6_random_loopback_port"] = check(kv.get("C6") == "200", "200", kv.get("C6"))
    checks["ep02_c7_revoked_within_3s_and_bindable"] = check(
        kv.get("C7-LISTENERS") == "0" and float(kv.get("C7-SECONDS", "99")) <= 3.0 and kv.get("C7-NC-RC") == "124",
        "no listener in <= 3 s; nc binds (rc 124 = bound and waited)", {k: kv.get(k) for k in ("C7-LISTENERS", "C7-SECONDS", "C7-NC-RC")})
    # SEC-08 c3: the job's own listener wins; the collision is reported; nothing is replaced
    scope.run(["services", "stop"], cwd=project, timeout=300)  # a clean slate for the collision job
    sec = r"""
nc -lk 5432 >/dev/null 2>&1 &
sleep 1
docker compose up -d --wait 2>&1 | tail -1
sleep 4
ss -ltnp 2>/dev/null | grep ':5432' || true
echo ---
PGCONNECT_TIMEOUT=3 pg_isready -h localhost -p 5432 || true
"""
    code, out, err = scope.job(sec, cwd=project, timeout=1500)
    checks["sec08_c3_job_listener_wins_and_collision_reported"] = check(
        code == 0 and "nc" in out.split("---")[0] and "port 5432 is already in use in this job" in err,
        "ss shows nc on 5432; stderr has the marsh: collision line",
        {"exit": code, "ss": out.split("---")[0][-400:], "stderr": err[-500:], "ready": out.split("---")[-1].strip()})
    return checks


@scenario("testcontainers")
def testcontainers(args, scope: Scope) -> dict:
    """EP-03 c1 (Ryuk off) and c2 (Ryuk on: the job's own endpoint path is Ryuk's socket, D20) from a real job."""
    checks: dict = {}
    scope.start()
    project = stage_fixture(args, "tc", scope.project / "tc")
    scope.enable_services(project)
    off = scope.job("TESTCONTAINERS_RYUK_DISABLED=true node tc.test.js", cwd=project, timeout=1500)
    last = (off[1].strip().splitlines() or [""])[-1]
    try:
        verdict = json.loads(last)
    except ValueError:
        verdict = None
    checks["ep03_c1_ryuk_off"] = check(off[0] == 0 and bool(verdict and verdict.get("ok") and verdict.get("port", 0) >= 32768),
                                       "ok, port >= 32768", {"exit": off[0], "out": last, "err": off[2][-300:]})
    on = scope.job("env | grep -E '^(TESTCONTAINERS_|DOCKER_)' | sort; node tc-ryuk.test.js 2>&1 | tail -3", cwd=project, timeout=1500)
    lines = on[1].strip().splitlines()
    env = [l for l in lines if l.startswith(("TESTCONTAINERS_", "DOCKER_"))]
    checks["ep03_c2_ryuk_on_env"] = check(env == ["DOCKER_HOST=unix:///run/marsh/docker.sock", "TESTCONTAINERS_HOST_OVERRIDE=localhost"],
                                          "only HOST_OVERRIDE (no RYUK_DISABLED, no SOCKET_OVERRIDE)", env)
    checks["ep03_c2_ryuk_on_passes"] = check(on[0] == 0 and any('"ok":true' in l or "ok" in l.lower() for l in lines[-2:]),
                                             "the Ryuk-on test passes", {"exit": on[0], "tail": lines[-3:], "err": on[2][-300:]})
    # privileged create is refused as `marsh:` in the job (EP/SEC-01 row)
    refused = scope.job(f"docker run --rm --privileged {BUSYBOX} true 2>&1; echo RC=$?", cwd=project, timeout=600)
    checks["privileged_create_refused_marsh"] = check("marsh:" in refused[1] and "RC=0" not in refused[1],
                                                      "a marsh: refusal", refused[1][-300:])
    return checks



def kit_vms(scope: Scope) -> list[str]:
    return sorted(name for name in scope.owned() if name.startswith(PREFIX + "k-"))


def wait_file(path: pathlib.Path, ceiling=300.0) -> str:
    return poll(lambda: path.read_text() if path.exists() else "", ceiling, 0.5) or ""


HELD = r"""
IP=$(python3 -c "import socket;print(socket.gethostbyname(socket.gethostname()))")
echo $IP > .{tag}-ip
export PGAPPNAME=nfx-held-{tag}
export PGPASSWORD=devpass
((echo "begin;"; sleep 600) | psql -h localhost -U postgres -d app -Atq >/dev/null 2>&1 &)
while [ ! -e .release-{tag} ]; do
  echo "$(date +%s) NONCE=$(psql -h localhost -U postgres -d app -Atc 'select name from items where id=1' 2>&1 | head -1)" >> .{tag}-log
  sleep 1
done
echo released
"""


@scenario("netns")
def netns(args, scope: Scope) -> dict:
    """EP-07 (no hold or fault) and the two-concurrent-jobs collision case: the same port in two jobs, each its own."""
    checks: dict = {}
    scope.start()
    projects = {}
    for tag in ("p1", "p2"):
        project = stage_fixture(args, "stack-a", scope.project / f"app-{tag}")
        scope.enable_services(project)
        projects[tag] = project
        up = scope.job(COMPOSE_UP + "; docker compose exec -T db psql -U postgres -d app -Atc \"update items set name='nonce-" + tag
                       + "' where id=1\"", cwd=project, timeout=1500)
        checks[f"{tag}_stack_up"] = check(up[0] == 0, "exit 0", {"exit": up[0], "out": up[1][-200:], "err": up[2][-200:]})
    held = {tag: scope.popen(["run", "nfx", "exec", "sh", "-c", HELD.format(tag=tag)], cwd=projects[tag],
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) for tag in projects}
    try:
        logs = {tag: wait_file(projects[tag] / f".{tag}-log", 300) for tag in projects}
        poll(lambda: all(f"NONCE=nonce-{tag}" in (projects[tag] / f".{tag}-log").read_text() for tag in projects
                         if (projects[tag] / f".{tag}-log").exists()) and all((projects[tag] / f".{tag}-log").exists() for tag in projects),
             120, 1)
        seen = {tag: (projects[tag] / f".{tag}-log").read_text() if (projects[tag] / f".{tag}-log").exists() else "" for tag in projects}
        checks["two_concurrent_jobs_each_reach_their_own"] = check(
            all(f"NONCE=nonce-{tag}" in seen[tag] and all(f"nonce-{other}" not in seen[tag] for other in projects if other != tag)
                for tag in projects), "p1 reads nonce-p1 only, p2 reads nonce-p2 only",
            {tag: seen[tag].strip().splitlines()[-2:] for tag in projects})
        ip1 = (projects["p1"] / ".p1-ip").read_text().strip()
        # a sibling job (same Kit VM, own namespace)
        j3 = scope.job("ss -ltnH | tr -s ' ' | cut -d' ' -f4 | sort | tr '\n' ' '; echo; "
                       f"(nc -z -w2 {ip1} 5432 && echo REACHED || echo not-reached); (nc -z -w2 {ip1} 8025 && echo REACHED2 || echo not-reached2)",
                       cwd=projects["p1"], timeout=300)
        lines = j3[1].strip().splitlines()
        checks["ep07_c1_sibling_netns_has_no_listener_and_cannot_reach_j1"] = check(
            j3[0] == 0 and ":5432" not in (lines[0] if lines else "") and "not-reached" in lines and "not-reached2" in lines,
            "no listener in the sibling's namespace; J1's IP does not answer", {"exit": j3[0], "lines": lines, "j1_ip": ip1})
        # the Kit worker's namespace: no listener of ours, none on a wildcard
        kits = kit_vms(scope)
        worker_ns = {}
        for vm in kits:
            done = subprocess.run([args.sbx, "exec", vm, "ss", "-ltnH"], capture_output=True, text=True, timeout=60)
            worker_ns[vm] = [l.split()[3] for l in done.stdout.splitlines() if len(l.split()) > 3]
        bad = {vm: [a for a in addrs if a.endswith((":5432", ":6379", ":8025", ":18080"))] for vm, addrs in worker_ns.items()}
        checks["ep07_c1_kit_vm_netns_has_no_forward_listener"] = check(
            bool(kits) and all(not v for v in bad.values()), "no :5432/:6379/:8025 listener in the Kit VM's namespace", worker_ns)
        # c2: J2 ends; its accepted session closes (the engine's own view) and nothing stays
        sessions = lambda: scope.vx("docker exec " + next(n for n in scope.vx("docker ps --format '{{.Names}}'", cwd=projects["p2"]).stdout.split() if n.endswith("-db-1"))
                                    + " psql -U postgres -d app -Atc \"select count(*) from pg_stat_activity where application_name='nfx-held-p2'\"",
                                    cwd=projects["p2"]).stdout.strip()
        before = sessions()
        (projects["p2"] / ".release-p2").write_text("")
        held["p2"].wait(timeout=120)
        after = poll(lambda: sessions() == "0" and "0", 30, 1) or sessions()
        checks["ep07_c2_ended_job_closes_its_accepted_session"] = check(before not in ("", "0") and after == "0",
                                                                        "the held session exists, then 0 after the job ends",
                                                                        {"before": before, "after": after})
    finally:
        for tag in projects:
            (projects[tag] / f".release-{tag}").write_text("")
        for tag, proc in held.items():
            try:
                proc.wait(timeout=120)
            except subprocess.TimeoutExpired:
                proc.terminate()
    return checks



def shell(scope: Scope, project: pathlib.Path, script: str, timeout=900):
    """`marsh -c SCRIPT` in the project: the marsh shell in the shell VM (not a Kit job)."""
    done = scope.run(["-c", script], cwd=project, timeout=timeout)
    return done.returncode, done.stdout, done.stderr


@scenario("shell")
def shell_path(args, scope: Scope) -> dict:
    """EP-05, EP-09 c1-c2, VM-01 c6 (shell side): the shell gets the project endpoint and its forwards."""
    checks: dict = {}
    scope.start()
    p1 = stage_fixture(args, "stack-a", scope.project / "app-p1")
    p2 = stage_fixture(args, "stack-a", scope.project / "app-p2")
    off = (scope.root / "project-off")
    off.mkdir()
    off = off.resolve()
    scope.enable_services(p1)
    scope.enable_services(p2)
    for tag, project in (("p1", p1), ("p2", p2)):
        up = scope.job(COMPOSE_UP + "; for i in $(seq 1 240); do curl -sf localhost:8025/api/v1/info >/dev/null && break; sleep 0.5; done; "
                       + SET_NONCE.format(tag=tag), cwd=project, timeout=1500)
        checks[f"{tag}_stack_up"] = check(up[0] == 0, "exit 0", {"exit": up[0], "err": up[2][-200:]})
    job_view = {}
    for tag, project in (("p1", p1), ("p2", p2)):
        job_view[tag] = scope.job("docker info --format '{{.ID}}'; docker ps --format '{{.Names}}' | sort", cwd=project)[1].split()
    prod_ids = {tag: scope.vx("docker info --format '{{.ID}}'", cwd=project).stdout.strip() for tag, project in (("p1", p1), ("p2", p2))}
    script = ("echo [$DOCKER_HOST]; echo ID=$(docker info --format '{{.ID}}'); docker ps --format '{{.Names}}' | sort; "
              "for i in $(seq 1 40); do curl -sf localhost:8025/api/v1/info >/dev/null && break; sleep 0.5; done; "
              "curl -sf localhost:8025/api/v1/info >/dev/null && echo MAIL-OK || echo MAIL-NO; "
              "echo NONCE=$(" + GET_NONCE + ")")
    code, out, err = shell(scope, p1, script)
    lines = out.strip().splitlines()
    host_line = lines[0] if lines else ""
    checks["ep05_c0_docker_host_is_the_relayed_endpoint"] = check(
        host_line.startswith("[unix:///run/marsh/") and host_line.endswith("/d]"), "[unix:///run/marsh/<uid>/<session>/d]", host_line)
    checks["ep05_c1_engine_is_p1s_production_engine"] = check(f"ID={prod_ids['p1']}" in lines, f"ID={prod_ids['p1']}",
                                                              {"lines": lines[:3], "err": err[-300:]})
    checks["ep05_c2_names_match_the_jobs_view"] = check(
        [l for l in lines[2:] if not l.startswith(("MAIL", "NONCE"))] == job_view["p1"][1:], job_view["p1"][1:], lines)
    checks["ep05_c3_forward_in_the_shell_vm"] = check("MAIL-OK" in lines and "NONCE=nonce-p1" in lines, "MAIL-OK and nonce-p1",
                                                      [l for l in lines if l.startswith(("MAIL", "NONCE"))])
    code2, out2, err2 = shell(scope, p2, script)
    lines2 = out2.strip().splitlines()
    checks["ep05_c4_p2_sees_only_its_own"] = check(f"ID={prod_ids['p2']}" in lines2 and f"ID={prod_ids['p1']}" not in lines2
                                                   and "NONCE=nonce-p2" in lines2, "p2's engine and nonce only",
                                                   {"lines": lines2, "err": err2[-200:]})
    # a project with services off: no DOCKER_HOST, the shell's private engine (VM-01 c6)
    code3, out3, err3 = shell(scope, off, "echo [$DOCKER_HOST]; docker info --format '{{.ID}}'")
    off_lines = out3.strip().splitlines()
    checks["vm01_c6_services_off_shell_has_no_endpoint"] = check(
        code3 == 0 and off_lines[:1] == ["[]"] and off_lines[1:2] and off_lines[1] not in prod_ids.values(),
        "DOCKER_HOST empty and the shell's own private engine", {"lines": off_lines, "err": err3[-200:]})
    # EP-09 c1: unset DOCKER_HOST reaches the private engine; objects there are not the project's
    # (objects are created, not run; the endpoint refuses image import, so it uses the preloaded busybox)
    both = (
        f"docker create --name priv {BUSYBOX} /none >/dev/null || echo CREATE-FAILED; echo PRIV-ID=$(docker info --format '{{{{.ID}}}}'); "
        "echo PRIV-LIST=$(docker ps -a --format '{{.Names}}' | tr '\\n' ' '); docker rm -f priv >/dev/null; unset DOCKER_HOST; "
        "tar cf - --files-from /dev/null | docker import - nfx-empty >/dev/null; "
        "docker create --name priv nfx-empty /none >/dev/null; echo PRIV-ID2=$(docker info --format '{{.ID}}'); "
        "echo PRIV-LIST2=$(docker ps -a --format '{{.Names}}' | tr '\\n' ' '); docker rm -f priv >/dev/null")
    code4, out4, err4 = shell(scope, p1, both)
    kv = dict(l.split("=", 1) for l in out4.splitlines() if re.match(r"^[A-Z][A-Z0-9-]*=", l))
    checks["ep09_c1_unset_reaches_the_private_engine"] = check(
        kv.get("PRIV-ID") == prod_ids["p1"] and kv.get("PRIV-ID2") not in (None, "", prod_ids["p1"]) and "priv" in kv.get("PRIV-LIST", "")
        and "priv" in kv.get("PRIV-LIST2", ""), "endpoint = production engine; unset = a different, private engine", {**kv, "stderr": err4[-600:]})
    # EP-09 c2: an object made from the shell survives the shell's exit (and an unrelated job's end)
    shell(scope, p1, f"docker run -d --name svcobj {BUSYBOX} sleep 600 >/dev/null")
    scope.job("true", cwd=p1)
    names = scope.vx("docker ps --format '{{.Names}}'", cwd=p1).stdout.split()
    checks["ep09_c2_shell_objects_persist"] = check("svcobj" in names, "svcobj still running in the services engine", names)
    scope.vx("docker rm -f svcobj", cwd=p1)
    return checks


@scenario("collision")
def shell_collision(args, scope: Scope) -> dict:
    """EP-08 (D19): the first claimant of a shell VM port keeps it; a later claim is reported; no isolation claimed.
    (Port 8025, mailpit's: the shell image has curl, not psql; both projects publish the same ports.)"""
    checks: dict = {}
    scope.start()
    projects = {}
    for tag in ("a", "b"):
        project = stage_fixture(args, "stack-a", scope.project / f"app-{tag}")
        scope.enable_services(project)
        projects[tag] = project
        up = scope.job(COMPOSE_UP + "; for i in $(seq 1 240); do curl -sf localhost:8025/api/v1/info >/dev/null && break; sleep 0.5; done; "
                       + SET_NONCE.format(tag=tag), cwd=project, timeout=1500)
        checks[f"{tag}_stack_up"] = check(up[0] == 0, "exit 0", {"exit": up[0], "err": up[2][-200:]})
    held_script = ("for i in $(seq 1 60); do n=$(" + GET_NONCE + "); case \"$n\" in nonce-*) break;; esac; sleep 0.5; done; "
                   "while [ ! -e .release-a ]; do echo \"$(date +%s) $(" + GET_NONCE + ")\" >> .a-log; sleep 1; done; echo A-DONE")
    sa = scope.popen(["-c", held_script], cwd=projects["a"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        first = poll(lambda: (projects["a"] / ".a-log").exists() and "nonce-a" in (projects["a"] / ".a-log").read_text()
                     and (projects["a"] / ".a-log").read_text(), 180, 1)
        a_output = ""
        if not first and sa.poll() is not None:
            a_output = (sa.stdout.read() + sa.stderr.read())[-600:]
        checks["s_a_first_claim_reads_nonce_a"] = check(bool(first), "nonce-a", {"log": (first or "")[-120:], "shell_output": a_output})
        if not first:
            return checks
        code, out, err = shell(scope, projects["b"], "sleep 8; echo B-QUERY=$(" + GET_NONCE + ")", timeout=300)
        a_after = (projects["a"] / ".a-log").read_text().strip().splitlines()[-1]
        checks["ep08_c1_collision_reported_and_a_keeps_the_port"] = check(
            "port 8025 is already in use in the shell VM" in err and "marsh:" in err and "nonce-a" in a_after,
            "S_B's claim reports a marsh: collision; S_A still reads nonce-a (no isolation claimed)",
            {"b_stderr": err[-500:], "a_after": a_after, "b_query": out.strip()[-80:]})
        (projects["a"] / ".release-a").write_text("")
        sa.wait(timeout=120)
        code, out, err = shell(scope, projects["b"],
                               "for i in $(seq 1 60); do n=$(" + GET_NONCE + "); case \"$n\" in nonce-b) break;; esac; sleep 1; done; echo B2=$n", timeout=300)
        checks["ep08_c2_after_a_closes_b_reads_nonce_b"] = check("B2=nonce-b" in out, "nonce-b", {"out": out[-120:], "err": err[-200:]})
    finally:
        (projects["a"] / ".release-a").write_text("")
        try:
            sa.wait(timeout=120)
        except subprocess.TimeoutExpired:
            sa.terminate()
    return checks


@scenario("status")
def status(args, scope: Scope) -> dict:
    """`services status --json` containers and ports match what `vx` observes (N3's agreed shape)."""
    checks: dict = {}
    scope.start()
    project = stage_fixture(args, "stack-a", scope.project / "app")
    scope.enable_services(project)
    before = None
    code, out, err = scope.job(COMPOSE_UP + f"; docker run -d --name solo -p 127.0.0.1:18081:80 {NGINX} >/dev/null", cwd=project, timeout=1500)
    checks["stack_up"] = check(code == 0, "exit 0", {"exit": code, "err": err[-200:]})
    entry = poll(lambda: (lambda e: e if e.get("containers") else None)(scope.project_entry(project)), 60, 2) or scope.project_entry(project)
    observed = scope.vx("docker ps --format '{{json .}}'", cwd=project)
    rows = [json.loads(l) for l in observed.stdout.splitlines() if l.strip()]
    expected_names = sorted(r["Names"] for r in rows)
    got = entry.get("containers", [])
    checks["status_container_names_match_vx"] = check(sorted(c["name"] for c in got) == expected_names, expected_names,
                                                      [c["name"] for c in got])
    shape_ok = all(set(c) == {"name", "service", "state", "health"} for c in got) and all(
        set(p) == {"service", "container_port", "host_port", "protocol"} for p in entry.get("ports", []))
    checks["status_shape_is_the_agreed_one"] = check(shape_ok, "containers {name,service,state,health}; ports {service,container_port,host_port,protocol}",
                                                     {"containers": got[:2], "ports": entry.get("ports", [])[:2]})
    by_name = {c["name"]: c for c in got}
    db = next((c for n, c in by_name.items() if n.endswith("-db-1")), {})
    checks["status_service_state_health"] = check(db.get("service") == "db" and db.get("state") == "running" and db.get("health") == "healthy",
                                                  "db: service db, running, healthy", db)
    published = {(p["service"], p["container_port"], p["host_port"]) for p in entry.get("ports", [])}
    want = {("db", 5432, 5432), ("redis", 6379, 6379), ("mail", 8025, 8025), ("mail", 1025, 1025), ("solo", 80, 18081)}
    checks["status_ports_match_the_published_ones"] = check(want <= published, sorted(want), sorted(published))
    checks["status_internet_kept"] = check(set(entry.get("internet", {})) >= {"on", "governed", "effective"}, "internet {on, governed, effective}",
                                           entry.get("internet"))
    scope.vx("docker rm -f solo", cwd=project)
    gone = poll(lambda: (lambda e: not any(p["host_port"] == 18081 for p in e.get("ports", [])) or None)(scope.project_entry(project)), 30, 1)
    checks["status_follows_a_removed_container"] = check(bool(gone), "port 18081 no longer listed", scope.project_entry(project).get("ports"))
    return checks


@scenario("xfer")
def xfer(args, scope: Scope) -> dict:
    """EP-02 c4 / EP-06 shape, repeated: `docker cp` and `docker exec -i/cat` of 1 MiB, 20 rounds, every hash equal."""
    checks: dict = {}
    scope.start()
    project = (scope.project / "app")
    project.mkdir()
    project = project.resolve()
    scope.enable_services(project)
    script = r"""
docker run -d --name nw BUSYBOX sleep 600 >/dev/null
bad=0; rounds=0
for i in $(seq 1 20); do
  head -c 1048576 /dev/urandom > /tmp/in.bin
  a=$(sha256sum < /tmp/in.bin | cut -c1-12)
  docker cp /tmp/in.bin nw:/tmp/in.bin; b=$(docker exec nw sha256sum /tmp/in.bin | cut -c1-12)
  docker exec -i nw sh -c 'cat > /tmp/x' < /tmp/in.bin 2>/tmp/err.txt; rc=$?
  c=$(docker exec nw sha256sum /tmp/x | cut -c1-12)
  d=$(docker exec nw cat /tmp/x | sha256sum | cut -c1-12)
  docker cp nw:/tmp/x /tmp/out.bin; e=$(sha256sum < /tmp/out.bin | cut -c1-12)
  rounds=$((rounds+1))
  if [ "$a$a$a$a" != "$b$c$d$e" ] || [ $rc != 0 ]; then bad=$((bad+1)); echo "BAD i=$i rc=$rc orig=$a cp-in=$b exec-in=$c exec-out=$d cp-out=$e err=$(head -c 200 /tmp/err.txt)"; fi
done
echo ROUNDS=$rounds
echo BAD-COUNT=$bad
docker rm -f nw >/dev/null
"""
    code, out, err = scope.job(script.replace("BUSYBOX", BUSYBOX), cwd=project, timeout=900)
    checks["byte_exact_cp_and_exec_stream_20_rounds_of_1_MiB"] = check(
        code == 0 and "BAD-COUNT=0" in out and "ROUNDS=20" in out, "20 rounds, no bad hash", {"exit": code, "out": out[-800:], "err": err[-300:]})
    return checks


def acp_exec(scope: Scope, agent: str, script: str, cwd: pathlib.Path, timeout: float = 600.0):
    """One ACP prompt `exec SCRIPT` to the agent `marsh run --acp AGENT` starts (the fixture's fake agent);
    returns (texts, stderr_text, returncode_or_none). A JSON-RPC error is returned, not raised."""
    import selectors
    proc = subprocess.Popen([str(scope.marsh), "run", "--acp", agent], cwd=cwd, env=scope.env, stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, process_group=0)
    selector = selectors.DefaultSelector()
    selector.register(proc.stdout, selectors.EVENT_READ)
    buffer, texts, error = b"", [], None

    def send(obj):
        proc.stdin.write((json.dumps({"jsonrpc": "2.0", **obj}) + "\n").encode())
        proc.stdin.flush()
    try:
        send({"id": 1, "method": "initialize", "params": {"protocolVersion": 1, "clientCapabilities": {}}})
        send({"id": 2, "method": "session/new", "params": {"cwd": str(cwd), "mcpServers": []}})
        deadline = time.monotonic() + timeout
        finished = False
        while not finished and time.monotonic() < deadline:
            if not selector.select(1.0):
                if proc.poll() is not None:
                    break
                continue
            chunk = os.read(proc.stdout.fileno(), 65536)
            if not chunk:
                break
            buffer += chunk
            while b"\n" in buffer:
                line, buffer = buffer.split(b"\n", 1)
                if not line.strip():
                    continue
                message = json.loads(line)
                if "error" in message:
                    error = json.dumps(message["error"])[:300]
                    finished = True
                    break
                if message.get("id") == 2 and "result" in message:
                    send({"id": 3, "method": "session/prompt", "params": {
                        "sessionId": message["result"]["sessionId"], "prompt": [{"type": "text", "text": f"exec {script}"}]}})
                elif message.get("method") == "session/update":
                    update = message["params"]["update"]
                    if update.get("sessionUpdate") == "agent_message_chunk":
                        texts.append(update["content"]["text"])
                elif message.get("id") == 3:
                    finished = True
                    break
    finally:
        try:
            proc.stdin.close()
        except OSError:
            pass
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, 15)  # this scope's own agent group, by the pid this run started
            proc.wait(timeout=10)
    err = proc.stderr.read().decode(errors="replace")
    selector.close()
    out = b"".join(__import__("base64").b64decode(t[len("fixture:out:"):]) for t in texts if t.startswith("fixture:out:")).decode(errors="replace")
    return out, err, error


@scenario("env")
def env_sources(args, scope: Scope) -> dict:
    """SEC-08 c1-c2: DOCKER_* comes only from marsh. Sources: the host env of `marsh run`, a Kit's env, a descendant job."""
    checks: dict = {}
    # the Kit source: an ACP adapter whose descriptor declares DOCKER_* (written before the daemon reads commands.json)
    kits = sorted((scope.root / "kit").glob("*/commands.json"))[0]
    commands = json.loads(kits.read_text())
    src = pathlib.Path(commands["nfx-acp"])
    dst = src.parent / "nfx-env"
    shutil.copytree(src, dst, dirs_exist_ok=True)
    descriptor = (dst / "nfx-acp.yaml").read_text().replace("      agent: nfx-acp\n", "      agent: nfx-env\n      env:\n        DOCKER_HOST: tcp://10.0.0.1:2375\n        DOCKER_CONTEXT: evil\n")
    descriptor = descriptor.replace('provides: ["nfx-acp@1.0.0"]', 'provides: ["nfx-env@1.0.0"]')
    (dst / "nfx-acp.yaml").write_text(descriptor)
    commands["nfx-env"] = str(dst)
    (scope.control_home / "commands.json").write_text(json.dumps(commands, indent=1))
    scope.start()
    project = (scope.project / "app")
    project.mkdir()
    project = project.resolve()
    scope.enable_services(project)
    hostile = {"DOCKER_HOST": "tcp://10.0.0.1:2375", "DOCKER_CONTEXT": "evil", "DOCKER_CONFIG": "/tmp/evil",
               "TESTCONTAINERS_HOST_OVERRIDE": "evil.example"}
    show = "env | grep -E '^(DOCKER_|TESTCONTAINERS_)' | sort"
    expected = ["DOCKER_HOST=unix:///run/marsh/docker.sock", "TESTCONTAINERS_HOST_OVERRIDE=localhost"]
    # (1) the host environment of `marsh run`
    code, out, err = scope.job(show, cwd=project, env=hostile)
    got = out.strip().splitlines()
    checks["sec08_c2_host_env_of_marsh_run"] = check(code == 0 and got == expected, f"marsh's value only: {expected}", {"exit": code, "got": got, "err": err[-200:]})
    # (2) a descendant that spawns `marsh run` with those variables set
    code, out, err = scope.job("DOCKER_HOST=tcp://10.0.0.1:2375 DOCKER_CONTEXT=evil TESTCONTAINERS_HOST_OVERRIDE=evil.example "
                               "/run/marsh/bin/marsh run nfx exec sh -c \"" + show + "\"", cwd=project)
    got = out.strip().splitlines()
    checks["sec08_c2_descendant_marsh_run"] = check(code == 0 and got == expected, f"marsh's value only: {expected}", {"exit": code, "got": got, "err": err[-200:]})
    # (3) the Kit whose ACP adapter declares DOCKER_*: refused naming the variable (normal), or the job runs with it (break)
    out, err, error = acp_exec(scope, "nfx-env", show, project)
    got = out.strip().splitlines()
    scope_breaks = bool(scope.breaks)
    if not scope_breaks:
        refused = ("DOCKER_" in err or (error or "").find("DOCKER_") >= 0) and not got
        checks["sec08_c2_kit_env_refused_naming_the_variable"] = check(refused, "refused, naming DOCKER_HOST", {"stderr": err[-300:], "error": error, "got": got})
    else:
        checks["sec08_c2_kit_env_refused_naming_the_variable"] = check(
            "DOCKER_HOST=unix:///run/marsh/docker.sock" in got and not any(l.startswith("DOCKER_HOST=tcp") for l in got) and not any(
                l.startswith(("DOCKER_CONTEXT", "DOCKER_CONFIG")) for l in got),
            "refused or marsh's value (the break lets the Kit's value through)", {"stderr": err[-300:], "error": error, "got": got})
    return checks


@scenario("tcdebug")
def tcdebug(args, scope: Scope) -> dict:
    checks: dict = {}
    scope.start()
    project = stage_fixture(args, "tc", scope.project / "tc")
    scope.enable_services(project)
    script = r"""
cat > /tmp/proxy.py <<'EOF'
import socket, threading, os, sys
P = "/tmp/p.sock"; U = "/run/marsh/docker.sock"
try: os.unlink(P)
except OSError: pass
srv = socket.socket(socket.AF_UNIX); srv.bind(P); srv.listen(20)
log = open("/tmp/proxy.log", "ab")
def pipe(a, b, tag):
    try:
        while True:
            d = a.recv(65536)
            if not d: break
            if tag == "req" and b"/containers/create" in d[:200]:
                log.write(d[:3000] + b"\n=====\n"); log.flush()
            b.sendall(d)
    except OSError: pass
    try: b.shutdown(socket.SHUT_WR)
    except OSError: pass
def handle(c):
    u = socket.socket(socket.AF_UNIX); u.connect(U)
    threading.Thread(target=pipe, args=(c, u, "req"), daemon=True).start()
    threading.Thread(target=pipe, args=(u, c, "resp"), daemon=True).start()
while True:
    c, _ = srv.accept(); handle(c)
EOF
python3 /tmp/proxy.py &
sleep 1
DOCKER_HOST=unix:///tmp/p.sock TESTCONTAINERS_RYUK_DISABLED=true node tc.test.js 2>&1 | tail -3
echo ---; cat /tmp/proxy.log | head -c 3500
"""
    code, out, err = scope.job(script, cwd=project, timeout=600)
    checks["tcdebug"] = check(False, "see output", {"exit": code, "out": out[-6000:], "err": err[-500:]})
    return checks


@scenario("perf06")
def perf06(args, scope: Scope) -> dict:
    """PERF-06: `docker run -p` returning -> first connect, and `docker stop` -> refused; 20 runs, p95 <= 2 s each."""
    checks: dict = {}
    scope.start()
    project = (scope.project / "app")
    project.mkdir()
    project = project.resolve()
    scope.enable_services(project)
    script = r"""
for i in $(seq 1 20); do
  port=$((19000 + i))
  docker run -d --name "p$i" -p $port:80 NGINX >/dev/null
  t0=$(date +%s.%N)
  for k in $(seq 1 100); do [ "$(curl -s -o /dev/null -w '%{http_code}' localhost:$port)" = 200 ] && break; sleep 0.05; done
  t1=$(date +%s.%N)
  docker stop "p$i" >/dev/null
  t2=$(date +%s.%N)
  for k in $(seq 1 100); do ss -ltnH "sport = :$port" | grep -q . || break; sleep 0.05; done
  t3=$(date +%s.%N)
  python3 -c "print('SAMPLE up=%.2f down=%.2f' % ($t1-$t0, $t3-$t2))"
  docker rm -f "p$i" >/dev/null
done
"""
    code, out, err = scope.job(script.replace("NGINX", NGINX), cwd=project, timeout=1800)
    ups, downs = [], []
    for line in out.splitlines():
        m = re.match(r"SAMPLE up=([0-9.]+) down=([0-9.]+)", line)
        if m:
            ups.append(float(m.group(1)))
            downs.append(float(m.group(2)))
    p95 = lambda xs: sorted(xs)[max(0, int(len(xs) * 0.95 + 0.999) - 1)] if xs else None
    checks["perf06_20_samples"] = check(code == 0 and len(ups) == 20, 20, {"exit": code, "samples": len(ups), "err": err[-200:]})
    checks["perf06_up_p95_le_2s"] = check(bool(ups) and p95(ups) <= 2.0, "<= 2.0 s (target < 1 s)",
                                          {"p50": sorted(ups)[len(ups) // 2] if ups else None, "p95": p95(ups), "max": max(ups) if ups else None})
    checks["perf06_down_p95_le_2s"] = check(bool(downs) and p95(downs) <= 2.0, "<= 2.0 s (target < 1 s)",
                                            {"p50": sorted(downs)[len(downs) // 2] if downs else None, "p95": p95(downs), "max": max(downs) if downs else None})
    return checks


@scenario("setns")
def setns_fault(args, scope: Scope) -> dict:
    """EP-07 step 4: with `fault-setns-fail` nothing listens anywhere and the failure is reported; under
    `listener-before-setns` a listener appears in the worker's own namespace."""
    checks: dict = {}
    scope.start()
    project = (scope.project / "app")
    project.mkdir()
    project = project.resolve()
    scope.enable_services(project)
    script = r"""
docker run -d --name nw -p 18090:80 NGINX >/dev/null
sleep 14
echo JOB-LISTENERS=$(ss -ltnH | tr -s ' ' | cut -d' ' -f4 | grep -c :18090)
docker rm -f nw >/dev/null
"""
    scope.job("true", cwd=project)  # the Kit worker is up (and its VM known)
    kits = kit_vms(scope)
    proc = scope.popen(["run", "nfx", "exec", "sh", "-c", script.replace("NGINX", NGINX)], cwd=project, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    seen: dict = {}
    deadline = time.monotonic() + 600
    while proc.poll() is None and time.monotonic() < deadline:
        # while the job runs: the Kit VM's own namespace (the worker's) must hold no listener of ours
        for vm in kits:
            done = subprocess.run([args.sbx, "exec", vm, "ss", "-ltnH"], capture_output=True, text=True, timeout=60)
            for line in done.stdout.splitlines():
                if len(line.split()) > 3 and line.split()[3].endswith(":18090"):
                    seen.setdefault(vm, set()).add(line.split()[3])
        time.sleep(1)
    out, err = proc.communicate(timeout=60)
    listeners = {vm: sorted(addrs) for vm, addrs in seen.items()}
    checks["ep07_c1_failure_reported"] = check("marsh: port 18090 was not forwarded in this job" in err and "setns" in err,
                                               "a marsh: line naming port 18090 and setns", err[-400:])
    checks["ep07_c1_no_listener_in_the_job_namespace"] = check("JOB-LISTENERS=0" in out, "0", out.strip()[-60:])
    checks["ep07_c1_no_listener_in_the_kit_vm_namespace"] = check(bool(kits) and not listeners, "no :18090 listener in the Kit VM's namespace at any sample",
                                                                   {"seen": listeners, "samples_while_the_job_ran": True})
    return checks


@scenario("perf05")
def perf05(args, scope: Scope) -> dict:
    """PERF-05 (c) TPC-B -c4 through the job (0 failed) and (d) tc.test.js through the endpoint vs the shell's private
    engine (ratio <= 1.5, median of 5 interleaved pairs)."""
    checks: dict = {}
    scope.start()
    app = stage_fixture(args, "stack-a", scope.project / "app")
    tc = stage_fixture(args, "tc", scope.project / "tc")
    scope.enable_services(app)
    scope.enable_services(tc)
    script = r"""
docker compose up -d --wait 2>&1 | tail -1
export PGPASSWORD=devpass
for i in $(seq 1 60); do [ "$(psql -h localhost -U postgres -d app -Atc 'select 1' 2>/dev/null)" = 1 ] && break; sleep 1; done
pgbench -h localhost -U postgres -d app -i -s 1 -q 2>&1 | tail -1
pgbench -h localhost -U postgres -d app -c 4 -j 2 -T 15 2>&1 | grep -E "failed|tps|clients"
"""
    code, out, err = scope.job(script, cwd=app, timeout=1800)
    failed = re.search(r"number of failed transactions: (\d+)", out)
    tps = re.search(r"tps = ([0-9.]+)", out)
    checks["perf05_c_tpcb_c4_zero_failed"] = check(code == 0 and bool(failed) and failed.group(1) == "0" and bool(tps),
                                                   "0 failed transactions", {"exit": code, "out": out[-500:], "err": err[-200:]})
    # (d) The time is taken inside the job (and inside the shell) around `node tc.test.js` only: a job's own start
    # (a container) is not what the endpoint adds, and the shell has none.
    stamp = "t0=$(date +%s%N); node tc.test.js >/dev/null 2>&1; rc=$?; t1=$(date +%s%N); echo NS=$((t1-t0)); exit $rc"
    failures: list = []

    def seconds(done) -> float:
        match = re.search(r"NS=([0-9]+)", done[1])
        if done[0] != 0 or not match:
            failures.append({"exit": done[0], "out": done[1][-200:], "err": done[2][-200:]})
            return -1.0
        return int(match.group(1)) / 1e9

    def timed_job() -> float:
        return seconds(scope.job(stamp, cwd=tc, timeout=900))

    def timed_shell() -> float:
        return seconds(shell(scope, tc, "unset DOCKER_HOST; " + stamp, timeout=900))
    timed_job()
    timed_shell()  # warm both (images, VMs)
    pairs = [(timed_shell(), timed_job()) for _ in range(5)]
    ratios = sorted(job / direct for direct, job in pairs if direct > 0 and job > 0)
    median = ratios[len(ratios) // 2] if ratios else None
    checks["perf05_d_tc_ratio_le_1_5"] = check(len(ratios) == 5 and median is not None and median <= 1.5,
                                               "median job/direct <= 1.5 over 5 pairs", {"pairs_direct_job_s": pairs, "median_ratio": median, "baseline_failures": failures[:2]})
    return checks


def worker_fds(scope: Scope) -> dict:
    """Open descriptors of each marsh-worker process in the scope's Kit VMs (read only)."""
    counts = {}
    for vm in kit_vms(scope):
        done = subprocess.run([scope.args.sbx, "exec", "-u", "root", vm, "sh", "-c",
                               "for d in /proc/[0-9]*; do exe=$(readlink $d/exe 2>/dev/null); case $exe in *marsh-worker*) "
                               "echo ${d#/proc/} $(ls $d/fd 2>/dev/null | wc -l);; esac; done"],
                              capture_output=True, text=True, timeout=60)
        counts[vm] = sorted(line for line in done.stdout.splitlines() if re.fullmatch(r"\d+ \d+", line.strip()))
    return counts


@scenario("retain")
def retain(args, scope: Scope) -> dict:
    """EP-07 c2: after a job ends nothing of its forwards remains (the worker holds no descriptor of them)."""
    checks: dict = {}
    scope.start()
    project = stage_fixture(args, "stack-a", scope.project / "app")
    scope.enable_services(project)
    scope.job("true", cwd=project)  # the Kit worker is up
    before = worker_fds(scope)
    code, out, err = scope.job(COMPOSE_UP + "; for i in $(seq 1 60); do curl -sf localhost:8025/api/v1/info >/dev/null && break; sleep 0.5; done; "
                               "curl -sf localhost:8025/api/v1/info >/dev/null && echo FORWARDED", cwd=project, timeout=1500)
    time.sleep(5)
    after = worker_fds(scope)
    nums = lambda counts: {vm: [int(line.split()[1]) for line in lines] for vm, lines in counts.items()}
    checks["the_forward_worked"] = check(code == 0 and "FORWARDED" in out, "FORWARDED", {"exit": code, "out": out[-200:], "err": err[-200:]})
    checks["ep07_c2_no_descriptor_of_the_job_remains"] = check(
        bool(before) and all(nums(before).values()) and all(sum(a) <= sum(b) for vm, a in nums(after).items() for b in [nums(before)[vm]]),
        "the worker's descriptor count is back to its baseline", {"before": nums(before), "after": nums(after)})
    return checks


@scenario("hold")
def hold(args, scope: Scope) -> dict:
    """EP-07 step 3: `hold-netns-attach` stops a new publish between the namespace key and its open, until released."""
    checks: dict = {}
    scope.start()
    project = stage_fixture(args, "stack-a", scope.project / "app")
    scope.enable_services(project)
    scope.job("true", cwd=project)
    kits = kit_vms(scope)
    marker = "/run/marsh-test-holds/netns-attach"
    script = (COMPOSE_UP + "; for i in $(seq 1 300); do curl -sf localhost:8025/api/v1/info >/dev/null && echo FORWARDED && break; sleep 0.5; done")
    proc = scope.popen(["run", "nfx", "exec", "sh", "-c", script], cwd=project, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        def reached():
            return [vm for vm in kits if subprocess.run([args.sbx, "exec", vm, "test", "-e", marker + ".reached"], capture_output=True).returncode == 0]
        got = poll(reached, 240, 1)
        checks["hold_point_reached_before_any_listener"] = check(bool(got), "netns-attach.reached exists in the Kit VM", bool(got))
        time.sleep(3)
        # Every publish of the job opens the namespace and so reaches the hold: release each one as it is reached. The
        # hold directory belongs to the worker (root) and `sbx exec` runs as an unprivileged user by default, so the
        # release is made as root; without it a hold only ends at its own 120 s timeout.
        released: list = []
        deadline = time.monotonic() + 300
        while proc.poll() is None and time.monotonic() < deadline:
            for vm in reached():
                released.append(subprocess.run([args.sbx, "exec", "-u", "root", vm, "touch", marker + ".release"], capture_output=True).returncode)
                poll(lambda: not reached(), 30, 0.5)
            time.sleep(0.5)
        checks["every_hold_was_released"] = check(bool(released) and all(code == 0 for code in released), "each release file was created", released)
        out, err = proc.communicate(timeout=300)
        checks["after_release_the_forward_works"] = check("FORWARDED" in out, "FORWARDED", {"out": out[-200:], "err": err[-200:]})
    finally:
        if proc.poll() is None:
            proc.terminate()
    return checks


LATENCY_JS = r"""
const http = require('http');
const socketPath = process.argv[2];
const n = 200;
function once(agent, path) {
  return new Promise((resolve, reject) => {
    const t0 = process.hrtime.bigint();
    http.get({socketPath, path, agent}, (res) => { res.resume(); res.on('end', () => resolve(Number(process.hrtime.bigint() - t0) / 1e6)); }).on('error', reject);
  });
}
(async () => {
  const med = (xs) => xs.sort((a, b) => a - b)[Math.floor(xs.length / 2)];
  const keep = new http.Agent({keepAlive: true, maxSockets: 1});
  await once(keep, '/_ping');
  const a = []; for (let i = 0; i < n; i++) a.push(await once(keep, '/_ping'));
  const b = []; for (let i = 0; i < n; i++) b.push(await once(false, '/_ping'));
  const c = []; for (let i = 0; i < 50; i++) c.push(await once(false, '/containers/json'));
  console.log(JSON.stringify({keepalive_ping_ms: med(a), new_conn_ping_ms: med(b), new_conn_list_ms: med(c)}));
})();
"""


@scenario("latency")
def latency(args, scope: Scope) -> dict:
    """Per-request cost of the endpoint (median over 200 pings and 50 lists) against the shell's private engine."""
    checks: dict = {}
    scope.start()
    project = stage_fixture(args, "tc", scope.project / "tc")
    (project / "latency.js").write_text(LATENCY_JS)
    scope.enable_services(project)
    job = scope.job("node latency.js /run/marsh/docker.sock", cwd=project)
    direct = shell(scope, project, "unset DOCKER_HOST; node latency.js /var/run/docker.sock")
    checks["measured"] = check(job[0] == 0 and direct[0] == 0, "both ran",
                               {"through_the_endpoint": job[1].strip()[-200:], "shell_private_engine": direct[1].strip()[-200:],
                                "err": (job[2] + direct[2])[-300:]})
    return checks


@scenario("shellstart")
def shellstart(args, scope: Scope) -> dict:
    """Starts a marsh shell in a services-enabled project, cold, three times in a row."""
    checks: dict = {}
    scope.start()
    project = (scope.project / "app")
    project.mkdir()
    project = project.resolve()
    scope.enable_services(project)
    for i in range(3):
        code, out, err = shell(scope, project, "echo hi-$DOCKER_HOST")
        checks[f"shell_{i}"] = check(code == 0 and out.startswith("hi-unix:///run/marsh/"), "hi-unix:///run/marsh/...",
                                     {"exit": code, "out": out[-120:], "err": err[-700:]})
    return checks


SCENARIO_ORDER = ["endpoint", "ports", "testcontainers", "env", "netns", "shell", "collision", "status", "perf06", "setns", "perf05", "retain", "hold", "latency"]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--prefix", required=True, help="a dev install (make dev DEV_PREFIX=...)")
    parser.add_argument("--breaks-prefix", help="the test-breaks install for --break runs")
    parser.add_argument("--fixtures", required=True, help="H1b's tests/acceptance/services/fixtures")
    parser.add_argument("--sbx", default="sbx")
    parser.add_argument("--images", help="the saved image tars (default: <fixtures' repo>/target/nfx/images)")
    parser.add_argument("--only", default="")
    parser.add_argument("--break", dest="breaks", default="", help="comma-separated break modes (the daemon's env)")
    parser.add_argument("--keep", action="store_true", help="keep the scope's directory even when it passes")
    args = parser.parse_args()
    args.run = secrets.token_hex(3)
    args.prefix = str(pathlib.Path(args.prefix).expanduser())
    if args.breaks_prefix:
        args.breaks_prefix = str(pathlib.Path(args.breaks_prefix).expanduser())
    names = [n for n in (args.only.split(",") if args.only else SCENARIO_ORDER) if n]
    rc = 0
    for name in names:
        breaks = [b for b in args.breaks.split(",") if b]
        scope = Scope(args, name, breaks)
        result: dict = {}
        crashed = None
        try:
            result = SCENARIOS[name](args, scope)
        except BaseException as error:  # noqa: BLE001
            crashed = f"{type(error).__name__}: {error}"
        finally:
            problems = scope.cleanup()
        ok = crashed is None and not problems and all(c["ok"] for c in result.values())
        jline(scenario=name, run=args.run, breaks=breaks, ok=ok, crashed=crashed, cleanup_errors=problems, checks=result,
              scratch=str(scope.root))
        if ok and not args.keep:
            shutil.rmtree(scope.root, ignore_errors=True)  # this scope's own exact directory
        rc |= 0 if ok else 1
    return rc


if __name__ == "__main__":
    sys.exit(main())
