#!/usr/bin/env python3
"""C3 evidence: `split --open --from` restores app data into lanes, through the real marsh CLI and real jobs.

    vmlock.py --run c3 --shared -- python3 tests/acceptance/services_c3_lanes.py \
        --prefix ~/.marsh-dev-C3 --breaks-prefix ~/.marsh-dev-C3-breaks \
        --fixtures tests/acceptance/services/fixtures --images ~/dev/marsh-fork/target/nfx/images \
        [--only stacka,perf3,...] [--cells MODE,...]

The caller holds the VM lock (`harness/vmlock.py`; `--shared` for the functional scenarios, exclusive for PERF and the
break matrix); this script never takes it. Each scenario has its own scope (`MARSH_VM_PREFIX=marsh-xnfx01-`, a
`MARSH_HOME` under /private/tmp/nfx-c3-RUN-NAME, a daemon this script starts with `marshd --home H`), and cleans up by
exact names only: `marsh services rm --yes`, `marsh stop`, the VMs its ownership map recorded, and its own directory.
Nothing is removed or killed by pattern; Mark's `~/.marsh` and every other daemon are untouched. Synthetic data only.

The product driver: the real `marsh split --open --from`, `marsh checkpoint`, `marsh split close`, `marsh splits cancel`
and `marsh run nfx exec ...` jobs in the lanes (the `nfx` Kit of H1b). The engine is observed with `vx` (read-only, a
root exec into the services VM), never used to make the subject's state. Each scenario prints one JSON line.
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
import signal
import statistics
import subprocess
import sys
import threading
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import services_e2_endpoint as e2  # noqa: E402  (its Scope, check, poll, stage_fixture)

check, poll, jline = e2.check, e2.poll, e2.jline
ENGINE, PROD_SOCKET = e2.ENGINE, e2.PROD_SOCKET
R10 = re.compile(r"\b(volumes?|snapshots?|btrfs|xfs|reflink|pool)\b|\bVM\b", re.I)
SEED_SHA = "3714b445cbcd86638566b0b43c7ed6d8d9fe0f7fa4131bf6cdc6866297863af3"
AGENT_MODES = {"restore-files-from-agent", "fault-restore-skip-volumes", "fault-restore-bad-file-name", "fault-restore-huge-file",
               "fault-restore-nul", "fault-restore-bad-port", "fault-restore-bad-name", "fault-restore-markup", "fault-capture-fail",
               "hold-restore-before-install", "hold-restore-before-up", "restore-no-env", "restore-skip-volumes"}
HOST_MODES = {"restore-line-trust-vm", "lane-scope-root", "lane-drop-off", "drop-noop", "restore-silent-fail", "line-always-ok",
              "line-omit-fresh", "line-internal-words", "lane-ports-unmapped", "lane-files-follow-symlinks",
              "apply-include-generated", "restore-files-from-agent", "hold-lane-files-write", "hold-restore-after-lease"}


def pg(sql: str) -> str:
    return f"PGPASSWORD=devpass psql -h localhost -U postgres -d app -Atc \"{sql}\""


# A new job's forwards appear a moment after it starts (the daemon sends the ports once the job runs): a job that
# uses localhost waits for the ports it needs, bounded, like any client of a service that is still coming up.
WAIT = ("for i in $(seq 1 60); do pg_isready -h localhost -q && break; sleep 0.2; done; "
        "for i in $(seq 1 25); do redis-cli -h localhost ping >/dev/null 2>&1 && break; sleep 0.2; done; "
        "for i in $(seq 1 25); do curl -sf localhost:8025/api/v1/info >/dev/null 2>&1 && break; sleep 0.2; done; ")


class Lab(e2.Scope):
    """A scope that can be relaunched with other break modes while its services VM stays."""

    def __init__(self, args, label, breaks=None, idle=None):
        super().__init__(args, label, breaks)
        if idle:
            self.env["MARSH_SERVICES_IDLE_SECONDS"] = str(idle)
        self.pool_reserve = None
        self.lane_paths: dict[str, pathlib.Path] = {}
        self.wait_ports = False

    def job(self, script: str, cwd=None, env=None, timeout=900):
        return super().job((WAIT if self.wait_ports else "") + script, cwd=cwd, env=env, timeout=timeout)

    def use(self, breaks: list[str]) -> None:
        prefix = pathlib.Path(self.args.breaks_prefix if breaks else self.args.prefix)
        self.breaks = breaks
        self.marsh, self.marshd, self.guest = prefix / "bin/marsh", prefix / "bin/marshd", prefix / "libexec/marsh"
        self.env["MARSH_GUEST_ARTIFACTS"] = str(self.guest)

    def endpoint_path(self) -> pathlib.Path:
        canonical = self.home.resolve(strict=True)
        return pathlib.Path("/tmp") / f"marsh-{os.getuid()}" / hashlib.sha256(os.fsencode(canonical)).hexdigest()[:16] / "s"

    def kill_daemon(self) -> None:
        """The exact pid this run started, then its own stale endpoint (so no CLI-started daemon can answer)."""
        if self.daemon is not None and self.daemon.poll() is None:
            os.kill(self.daemon.pid, signal.SIGKILL)
            self.daemon.wait(timeout=20)
        self.endpoint_path().unlink(missing_ok=True)

    def relaunch(self, breaks: list[str]) -> None:
        self.kill_daemon()
        self.use(breaks)
        self.start()
        owner = (json.loads(self.run(["status", "--json"]).stdout).get("endpoint_owner") or {}).get("pid")
        assert owner == self.daemon.pid, f"the answering daemon is {owner}, not the launched {self.daemon.pid}"
        if breaks:
            got = sorted(json.loads(self.run(["status", "--json"]).stdout).get("test_breaks", []))
            want = sorted(m for m in breaks if m in HOST_MODES)
            assert got == want, f"daemon reports {got}, expected {want}"
        self.wait_ready(breaks)

    def wait_ready(self, breaks: list[str]) -> None:
        """A new daemon starts a new engine generation and a new relay: nothing is asked of it before it is ready, and the
        agent reports exactly the agent-side modes asked for."""
        def ready():
            entry = self.project_entry(self.project_dir)
            return entry.get("state") == "ready" and (entry.get("relay") or {}).get("state") == "ready" and entry
        entry = poll(ready, 240, 3)
        assert entry, f"the services relay did not come back: {self.project_entry(self.project_dir)}"
        want = sorted(m for m in breaks if m in AGENT_MODES)
        got = sorted(entry.get("test_breaks_agent", []))
        assert got == want, f"the agent reports {got}, expected {want}"

    def start(self) -> None:
        if self.daemon is not None:
            self.endpoint_path().unlink(missing_ok=True) if self.daemon.poll() is not None else None
        super().start()

    # --- observation -------------------------------------------------------------------------------------------

    def scope_objects(self, workspace: str) -> dict:
        label = f"label=dev.marsh.scope={workspace}"
        out = self.vx(f"docker ps -aq --filter {label}; echo '--'; docker volume ls -q --filter {label}; echo '--'; "
                      f"docker network ls -q --filter {label}", vm=self.vm()).stdout
        parts = out.split("--")
        names = [p.split() for p in parts] + [[], [], []]
        return {"containers": names[0], "volumes": names[1], "networks": names[2]}

    def vm(self) -> str:
        if not getattr(self, "_vm", None):
            self._vm = self.svc_vm(self.project_dir)
        return self._vm

    def pool_used(self) -> int:
        out = self.vx("df -k /pool | tail -1 | tr -s ' ' | cut -d' ' -f3", vm=self.vm()).stdout.strip()
        return int(out or 0) * 1024

    def pool_snapshot_hash(self, snapshot: str) -> str:
        script = (f"cd /pool/snapshots/{snapshot} && find . -type f | sort | xargs sha256sum | sha256sum | cut -c1-16")
        return self.vx(script, vm=self.vm()).stdout.strip()

    def vm_status(self) -> str:
        entry = self.project_entry(self.project_dir)
        return entry.get("state", "")

    # --- the product ---------------------------------------------------------------------------------------------

    def ckpt(self, label="nfx", lane=None) -> dict:
        argv = ["checkpoint", "--json", "--label", label]
        if lane:
            argv = ["checkpoint", "--json", "--label", label, "--lane", lane]
        done = self.run(argv, cwd=self.project_dir, timeout=300)
        assert done.returncode == 0, f"checkpoint failed: {done.stderr[-300:]}"
        return json.loads(done.stdout)

    def open_split(self, checkpoint: str, labels: list[str], label="nfx", timeout=900) -> tuple[int, dict, str, float]:
        t0 = time.monotonic()
        done = self.run(["split", "--open", "--from", checkpoint, "--label", label, *labels], cwd=self.project_dir,
                        timeout=timeout)
        elapsed = time.monotonic() - t0
        doc = {}
        for line in done.stdout.splitlines():
            if line.startswith("{"):
                doc = json.loads(line)
        for name, path in (doc.get("lanes") or {}).items():
            self.lane_paths[name] = pathlib.Path(path)
        return done.returncode, doc, done.stderr, elapsed

    def lane_job(self, lane: pathlib.Path, script: str, timeout=600) -> tuple[int, str, str]:
        return self.job(script, cwd=lane, timeout=timeout)

    def base_job(self, script: str, timeout=600) -> tuple[int, str, str]:
        return self.job(script, cwd=self.project_dir, timeout=timeout)

    def close_split(self, split_id: str) -> subprocess.CompletedProcess:
        return self.run(["split", "close", split_id], cwd=self.project_dir, timeout=600)

    def splits(self) -> list[dict]:
        done = self.run(["splits", "--json"], cwd=self.project_dir, timeout=60)
        try:
            doc = json.loads(done.stdout)
        except ValueError:
            return []
        return doc if isinstance(doc, list) else doc.get("splits", [])

    def setup_stack(self, fixture="stack-a", load=1, git=True, extra_env=None) -> pathlib.Path:
        """Stage the fixture into a git project, turn services on, start the stack from a real job, wait for the seed."""
        t0 = time.monotonic()
        self.start()
        print(f"[{time.monotonic() - t0:5.1f}s] daemon up", flush=True)
        project = e2.stage_fixture(self.args, fixture, self.project / "app")
        print(f"[{time.monotonic() - t0:5.1f}s] fixture staged", flush=True)
        if load:
            with (project / ".env").open("a") as env:
                env.write(f"LOAD={load}\n")
        for key, value in (extra_env or {}).items():
            with (project / ".env").open("a") as env:
                env.write(f"{key}={value}\n")
        if git:
            for argv in (["git", "init", "-q", "-b", "main"], ["git", "add", "-A"],
                         ["git", "-c", "user.name=nfx", "-c", "user.email=nfx@example.invalid", "-c", "commit.gpgsign=false", "commit", "-q", "-m", "stage"]):
                subprocess.run(argv, cwd=project, check=True, capture_output=True)
        self.project_dir = project
        self.enable_services(project)
        print(f"[{time.monotonic() - t0:5.1f}s] services enabled and started", flush=True)
        self.wait_ports = False
        code, out, err = self.base_job("docker compose up -d --wait 2>&1 | tail -8; echo UP-RC=${PIPESTATUS[0]}", timeout=1500)
        print(f"[{time.monotonic() - t0:5.1f}s] compose up: {out[-600:]} {err[-300:]}", flush=True)
        assert code == 0, f"compose up failed: {out[-300:]} {err[-300:]}"
        last: list = []

        def seeded() -> bool:
            result = self.base_job(pg("select count(*) from items") + " 2>&1; echo rc=$?")
            last[:] = [result]
            return result[1].strip().startswith("1000")
        self.wait_ports = True
        n = poll(seeded, 120, 3)
        if not n:
            ps = self.vx("docker ps -a --format '{{.Names}} {{.Status}} {{.Ports}}'", vm=self.vm()).stdout
            status = json.dumps({k: self.project_entry(self.project_dir).get(k) for k in ("state", "ports", "containers")})
            diag = self.base_job("ss -ltn 2>&1 | head; ls -l /run/marsh; env | grep -E '^(DOCKER|TESTCONT)'; docker ps --format '{{.Names}}' | head -3")
            log = (self.root / "marshd.log").read_text(errors="replace")[-1500:]
            raise AssertionError(f"the stack did not seed: {last}; engine: {ps}; status: {status}; job: {diag}; marshd.log: {log}")
        return project


# ----------------------------------------------------------------------------------------------------- truth
TRUTH = r"""
set +e
cd "$PWD"
echo ITEMS=$(__PG__ "select count(*) from items")
echo NOTE=$(__PG__ "select count(*) from information_schema.columns where table_name='items' and column_name='note'")
echo ROW1=$(__PG__ "select name from items where id=1")
echo ROW=$(__PG__ "select name||'/'||coalesce(note,'-') from items where id=__ID__" 2>&1 | tail -1)
echo MARKERS=$(__PG__ "select string_agg(name, ',' order by name) from markers")
echo REDIS=$(redis-cli -h localhost get k)
echo MAILS=$(curl -s localhost:8025/api/v1/messages | jq -r .total)
APPC=$(docker compose ps -q app)
docker compose pause app >/dev/null 2>&1; sleep 1
docker run --rm --volumes-from $APPC busybox:1.37 sh -c 'echo SEEDSUM=$(sha256sum /uploads/seed.bin 2>/dev/null | cut -d" " -f1); ls /uploads' > /tmp/up.txt 2>/dev/null
echo SEED=$(grep '^SEEDSUM=' /tmp/up.txt | cut -d= -f2)
grep '^tok-' /tmp/up.txt | sed 's/^tok-//' | sort > /tmp/files.txt
__PG__ "select token from tokens order by token" 2>/dev/null | sort > /tmp/tokens.txt
echo TOKENS=$(wc -l < /tmp/tokens.txt | tr -d ' ')
echo TOKFILES=$(wc -l < /tmp/files.txt | tr -d ' ')
echo TOKNOFILE=$(comm -23 /tmp/tokens.txt /tmp/files.txt | wc -l | tr -d ' ')
echo FILENOTOK=$(comm -13 /tmp/tokens.txt /tmp/files.txt | wc -l | tr -d ' ')
docker compose unpause app >/dev/null 2>&1
""".replace("__PG__", "PGPASSWORD=devpass psql -h localhost -U postgres -d app -Atc")


def read_truth(lab: Lab, cwd: pathlib.Path, row=3) -> dict:
    code, out, err = lab.job(TRUTH.replace("__ID__", str(row)), cwd=cwd, timeout=300)
    values = dict(line.split("=", 1) for line in out.splitlines() if re.match(r"^[A-Z0-9]+=", line))
    values["_exit"] = code
    values["_err"] = err[-300:]
    return values


def as_int(value, default=-1) -> int:
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def restored_services(lane: dict) -> list[str]:
    return sorted(i.get("service") for i in lane.get("restored", []) if i.get("kind") == "service")


def fresh_services(lane: dict) -> list[str]:
    return sorted(i.get("service") for i in lane.get("fresh", []) if i.get("kind") == "service")


def truth_ok(t: dict, m: dict) -> tuple[bool, dict]:
    """truth(fork, M): every restorable service present with M's sentinels, every fresh service running with its
    pre-capture sentinel absent (items, note column, row 1, markers, seed.bin, token files, redis, mail)."""
    parts = {
        "items": as_int(t.get("ITEMS")) == m["items"],
        "note_column": (as_int(t.get("NOTE")) == 1) == m["note"],
        "row1": t.get("ROW1") == m["row1"],
        "markers": t.get("MARKERS") == "m-seed-1,m-seed-2,m-seed-3",
        "seed.bin": t.get("SEED") == m.get("seed", SEED_SHA),
        "tokens_have_files": as_int(t.get("TOKNOFILE")) == 0,
        "files_beyond_tokens": as_int(t.get("FILENOTOK")) in (0, 1),
        "redis_fresh": t.get("REDIS") == "seed",
        "mail_fresh": as_int(t.get("MAILS")) == 0,
    }
    return all(parts.values()), parts


def split_objects(lab: Lab, split_id: str, labels: list[str]) -> dict:
    return {lbl: lab.scope_objects(f"fork-{split_id}-{lbl}") for lbl in labels}


def empty(objects: dict) -> bool:
    return all(not v for per in objects.values() for v in per.values())


M1 = {"items": 1000, "note": True, "row1": "item-1"}

SCENARIOS: dict = {}
ORDER: list[str] = []


def scenario(name):
    def register(fn):
        SCENARIOS[name] = fn
        ORDER.append(name)
        return fn
    return register


# ------------------------------------------------------------------------------------------------- stack A
@scenario("stacka")
def stacka(args, lab: Lab) -> dict:
    """RST-01, RST-03, RST-05, RST-09 (c1-c3), RST-11, EP-10, CAP-06, CAP-07 (c3), the token-file invariant."""
    c: dict = {}
    lab.use([])
    project = lab.setup_stack(load=1)
    time.sleep(6)  # the writer produces tokens
    vm = lab.vm()
    base_pool0 = lab.pool_used()
    t_before = lab.base_job("date +%s%N")[1].strip()
    c1 = lab.ckpt()
    t_after = lab.base_job("date +%s%N")[1].strip()
    s1 = c1.get("services") or {}
    c["ckpt1_has_services"] = check(bool(s1.get("snapshot")) and s1.get("gaps") == [], "snapshot set, no gaps",
                                    {k: s1.get(k) for k in ("snapshot", "reused_from", "gaps")})
    snap1 = s1.get("snapshot")
    hash1 = lab.pool_snapshot_hash(snap1) if snap1 else None
    # ---- RST-03 / EP-10 / RST-11 on C1, base intact ------------------------------------------------------------
    rc, doc, err, secs = lab.open_split(c1["checkpoint"], ["a", "b"])
    split1 = doc.get("id")
    restore = doc.get("restore") or {}
    a, b = restore.get("a", {}), restore.get("b", {})
    c["open_ok"] = check(rc == 0 and a.get("ok") is True and b.get("ok") is True, "both lanes ok:true",
                         {"rc": rc, "err": err[-400:], "a": {k: a.get(k) for k in ("ok", "line", "gaps")},
                          "b": {k: b.get(k) for k in ("ok", "line", "gaps")}, "seconds": round(secs, 1)})
    c["rst01_c2_restored_and_fresh"] = check(
        restored_services(a) == ["app", "db"] and fresh_services(a) == ["mail", "redis"] and "restored for app, db" in a.get("line", ""),
        "restored app+db, fresh mail+redis, the line names app and db", {"restored": restored_services(a),
                                                                      "fresh": fresh_services(a), "line": a.get("line")})
    c["rst07_c5_line_shape"] = check(
        all("\n" not in x.get("line", "") and len(x.get("line", "")) <= 400 and not R10.search(x.get("line", ""))
            for x in (a, b)) and bool(a.get("line")), "one line, <= 400, no internal words", [a.get("line"), b.get("line")])
    c["ports_remapped"] = check(sorted(a.get("ports", {})) == ["1025", "5432", "6379", "8025"] and all(
        int(v) > 1024 and int(v) != int(k) for k, v in a.get("ports", {}).items()), "every published port remapped", a.get("ports"))
    la, lb = lab.lane_paths["a"], lab.lane_paths["b"]
    c["lane_files_written_by_marshd"] = check(
        (la / "compose.fork.yaml").is_file() and not (la / "compose.fork.yaml").is_symlink() and (la / ".env").is_file(),
        "compose.fork.yaml and .env are regular files in the lane", sorted(p.name for p in la.iterdir() if p.name.startswith(("compose", ".env"))))
    env_text = (la / ".env").read_text()
    c["rst04_env_has_compose_lines"] = check(
        f"COMPOSE_PROJECT_NAME=fork-{split1}-a" in env_text and "COMPOSE_FILE=compose.yaml:compose.fork.yaml" in env_text and
        env_text.count("COMPOSE_") == 2, "exactly the two COMPOSE_ lines", env_text[-300:])
    ta, tb = read_truth(lab, la), read_truth(lab, lb)
    ok_a, parts_a = truth_ok(ta, M1)
    ok_b, parts_b = truth_ok(tb, M1)
    tok_a = as_int(ta.get("TOKENS"))
    c["rst01_c3_lane_sees_the_checkpoint_on_the_original_ports"] = check(
        ok_a and ok_b, "truth(lane, M(C1)) in both lanes, read from the lane's job on localhost:5432/6379/8025",
        {"a": parts_a, "b": parts_b, "a_raw": {k: ta.get(k) for k in ("ITEMS", "TOKENS", "TOKFILES", "TOKNOFILE", "FILENOTOK", "_exit", "_err")}})
    # the token invariant against the base at the checkpoint time
    base_tokens = lab.base_job(pg(f"select count(*) from tokens where token::bigint < {t_after}"))[1].strip()
    base_before = lab.base_job(pg(f"select count(*) from tokens where token::bigint < {t_before}"))[1].strip()
    lane_at = lab.lane_job(la, pg(f"select count(*) from tokens where token::bigint < {t_after}"))[1].strip()
    c["token_invariant_vs_base"] = check(
        as_int(base_before) - 2 <= as_int(lane_at) <= as_int(base_tokens) and as_int(lane_at) > 0 and as_int(ta.get("TOKNOFILE")) == 0,
        "the lane's tokens up to the checkpoint = the base's (within the freeze window), none without its file",
        {"base_before": base_before, "base_after": base_tokens, "lane": lane_at, "tokenfiles": ta.get("TOKFILES"), "tokens": ta.get("TOKENS")})
    # RST-03: nonces and destruction
    nonce = lambda who: f"UPDATE items SET name='nonce-{who}' WHERE id=1"  # noqa: E731
    lab.lane_job(la, pg(nonce("a")))
    lab.lane_job(lb, pg(nonce("b")) + "; " + pg("UPDATE items SET note='b-only' WHERE id=2") + "; redis-cli -h localhost set k b >/dev/null")
    lab.base_job(pg(nonce("orig")) + "; " + pg("UPDATE items SET note='orig-only' WHERE id=3") + "; redis-cli -h localhost set k orig >/dev/null")
    c["rst03_c1_lane_a_reads_its_nonce"] = check(lab.lane_job(la, pg("select name from items where id=1"))[1].strip() == "nonce-a",
                                                "nonce-a", "read from lane a's localhost:5432")
    lab.lane_job(la, pg("DELETE FROM items") + "; " + pg("DROP TABLE tokens") + "; redis-cli -h localhost flushall >/dev/null; "
                 "docker compose exec -T app sh -c 'rm -f /uploads/*'; "
                 "curl -s -X POST localhost:8025/api/v1/send -H 'Content-Type: application/json' "
                 "-d '{\"From\":{\"Email\":\"a@b.c\"},\"To\":[{\"Email\":\"d@e.f\"}],\"Subject\":\"from-a\"}' >/dev/null")
    orig = lab.base_job(pg("select count(*) from items") + "; " + pg("select name from items where id=1") + "; " + pg("select note from items where id=3") +
                        "; " + pg("select count(*) from items where note='b-only'") + "; " + pg("select count(*) from information_schema.tables where table_name='tokens'") +
                        "; redis-cli -h localhost get k; curl -s localhost:8025/api/v1/messages | jq -r .total; "
                        "docker compose exec -T app sha256sum /uploads/seed.bin | cut -d' ' -f1")[1].split()
    c["rst03_c2_base_untouched"] = check(orig[:8] == ["1000", "nonce-orig", "orig-only", "0", "1", "orig", "0", SEED_SHA],
                                        "1000 items, orig nonce, orig-only, no b-only, tokens, k=orig, 0 mails, seed.bin", orig)
    lane_b = lab.lane_job(lb, pg("select count(*) from items") + "; " + pg("select note from items where id=2") + "; redis-cli -h localhost get k; "
                          "curl -s localhost:8025/api/v1/messages | jq -r .total")[1].split()
    c["rst03_c3_lane_b_untouched"] = check(lane_b == ["1000", "b-only", "b", "0"], "1000 items, b-only, k=b, 0 mails", lane_b)
    lane_a = lab.lane_job(la, pg("select count(*) from items") + "; curl -s localhost:8025/api/v1/messages | jq -r .total")[1].split()
    c["rst03_c4_lane_a_destroyed_only_itself"] = check(lane_a == ["0", "1"], "0 items, 1 mail", lane_a)
    objs = {w: lab.scope_objects(w) for w in ("base", f"fork-{split1}-a", f"fork-{split1}-b")}
    nets = [objs[w]["networks"] for w in objs]
    c["rst03_c5_three_scopes_distinct"] = check(
        all(len(objs[w]["containers"]) >= 4 for w in objs) and len({tuple(x) for x in nets}) == 3 and
        not set(objs["base"]["containers"]) & set(objs[f"fork-{split1}-a"]["containers"]),
        "three scopes, each its own containers and network", {w: {k: len(v) for k, v in o.items()} for w, o in objs.items()})
    # EP-10: a standalone container without --network lands on its scope's own default network
    nets_seen: dict = {}
    refused: dict = {}
    for who, where in (("base", project), ("a", la), ("b", lb)):
        standalone = (f"docker run -d --name nfx-sa-{who} busybox:1.37 sh -c 'echo nonce-sa-{who} > /www.txt; exec httpd -f -p 8123 -h /' >/dev/null; "
                      f"docker inspect nfx-sa-{who} --format '{{{{range $k,$v := .NetworkSettings.Networks}}}}{{{{$k}}}} {{{{end}}}}'; "
                      "docker run --rm --network bridge busybox:1.37 true 2>&1 | head -2; echo END")
        code, out, err = lab.job(standalone, cwd=where, timeout=300)
        lines = [x for x in out.strip().splitlines() if x != "END"]
        nets_seen[who] = lines[0].strip() if lines else ""
        refused[who] = " ".join(lines[1:])
    c["ep10_c1_own_default_network_and_bridge_refused"] = check(
        all(n and n != "bridge" for n in nets_seen.values()) and len(set(nets_seen.values())) == 3 and
        all("marsh:" in r for r in refused.values()),
        "each scope's standalone container is on its own default network (3 distinct, not bridge); --network bridge refused with marsh:",
        {"networks": nets_seen, "refused": refused})
    # no foreign nonce is reachable from a container in scope a: every container IP of base and b, and the gateways
    def addresses(ws):
        return lab.vx(f"docker ps -q --filter label=dev.marsh.scope={ws} | xargs -r docker inspect --format "
                      "'{{range .NetworkSettings.Networks}}{{.IPAddress}} {{.Gateway}} {{end}}'", vm=vm).stdout.split()
    foreign = sorted(set(addresses("base") + addresses(f"fork-{split1}-b")) - {""})
    own = lab.vx("docker inspect nfx-sa-a --format '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}'", vm=vm).stdout.strip()
    probe = ("for ip in " + " ".join(foreign) + "; do for p in 8123 5432 6379 8025 1025; do "
             "nc -w 1 $ip $p </dev/null >/dev/null 2>&1 && echo REACH=$ip:$p; done; done; "
             f"echo OWN=$(wget -q -T 2 -O- http://{own}:8123/www.txt); echo DONE")
    reach = lab.lane_job(la, "docker run --rm busybox:1.37 sh -c '" + probe + "'", timeout=300)[1]
    remapped_b = " ".join(str(v) for v in b.get("ports", {}).values())
    local = lab.lane_job(la, "for p in " + remapped_b + "; do nc -w 1 localhost $p </dev/null >/dev/null 2>&1 && echo LOCAL-REACH=$p; done; echo DONE", timeout=120)[1]
    c["ep10_c2_no_foreign_route_from_scope_a"] = check(
        "REACH=" not in reach and "OWN=nonce-sa-a" in reach and "LOCAL-REACH" not in local,
        "from a container in scope a no address of base or b answers (container IPs, gateways), a's own does; b's remapped ports are not on a's localhost",
        {"probed": len(foreign), "reach": reach[-300:], "local": local[-200:]})
    # RST-11: destructive commands in lane a touch only lane a
    before = {w: sorted(lab.scope_objects(w)["containers"]) for w in ("base", f"fork-{split1}-b")}
    destroy = ("docker rm -f $(docker ps -aq) >/dev/null 2>&1; docker volume prune -af >/dev/null 2>&1; docker network prune -f >/dev/null 2>&1; "
               "docker compose down -v >/dev/null 2>&1; docker system prune -af --volumes 2>&1 | head -3; echo PS=$(docker ps -aq | wc -l | tr -d ' '); "
               "docker -H unix:///run/marsh/docker.sock plugin ls 2>&1 | head -1")
    code, out, err = lab.lane_job(la, destroy, timeout=600)
    after = {w: sorted(lab.scope_objects(w)["containers"]) for w in ("base", f"fork-{split1}-b")}
    a_left = lab.scope_objects(f"fork-{split1}-a")
    c["rst11_c1_base_and_b_keep_their_containers"] = check(before == after and all(before.values()), "same container ids in base and b",
                                                          {"before": before, "after": after})
    c["rst11_c2_a_objects_gone"] = check(not a_left["containers"] and "PS=0" in out, "a's own containers gone", {"left": a_left, "out": out[-300:]})
    c["rst11_c3_refused_route_says_marsh"] = check("marsh:" in out, "403 with marsh:", out[-300:])
    c["rst11_base_and_b_still_serve"] = check(
        lab.base_job(pg("select count(*) from items"))[1].strip() == "1000" and
        lab.lane_job(lb, pg("select note from items where id=2"))[1].strip() == "b-only", "base 1000 items, b b-only", "ok")
    # ---- RST-09: close drops the forks; apply merges files only --------------------------------------------------
    lab.lane_job(la, "echo from-a > a.txt; echo X=1 >> .env")  # (its stack is gone: a.txt and .env are what matter here)
    root_env = hashlib.sha256((project / ".env").read_bytes()).hexdigest()
    closed = lab.close_split(split1)
    c["rst09_close_ok"] = check(closed.returncode == 0, "close exits 0", closed.stderr[-300:])
    left = split_objects(lab, split1, ["a", "b"])
    c["rst09_c1_zero_fork_objects_after_close"] = check(empty(left), "no container, volume or network of either lane", left)
    applied = lab.run(["split", "apply", split1, "a", "--3way"], cwd=project, timeout=300)
    porcelain = subprocess.run(["git", "status", "--porcelain"], cwd=project, capture_output=True, text=True).stdout.split("\n")
    porcelain = [p for p in porcelain if p.strip()]
    c["rst09_c2_apply_clean_a_txt_present"] = check(applied.returncode == 0 and (project / "a.txt").exists(),
                                                   "apply exit 0, a.txt in the project", {"rc": applied.returncode, "err": applied.stderr[-300:]})
    c["rst09_c3_apply_merges_files_only"] = check(
        not (project / "compose.fork.yaml").exists() and hashlib.sha256((project / ".env").read_bytes()).hexdigest() == root_env and
        [p.strip() for p in porcelain] in (["?? a.txt"], ["A  a.txt".strip()]), "no compose.fork.yaml in the root, .env identical, porcelain only a.txt",
        {"porcelain": porcelain, "compose.fork.yaml": (project / "compose.fork.yaml").exists()})
    c["rst09_apply_left_services_alone"] = check(
        lab.base_job(pg("select count(*) from items"))[1].strip() == "1000", "the base still serves", "ok")
    removed = lab.run(["splits", "rm", split1], cwd=project, timeout=120)
    c["rst09_rm_ok"] = check(removed.returncode == 0, "splits rm exits 0", removed.stderr[-200:])
    pool_after = lab.pool_used()
    c["rst09_c1_pool_near_baseline"] = check(abs(pool_after - base_pool0) <= 64 << 20 or pool_after <= base_pool0 + (64 << 20),
                                            "pool within 64 MiB of the baseline",
                                            {"baseline_mib": base_pool0 >> 20, "after_mib": pool_after >> 20})
    # ---- RST-01 (break + C2), CAP-06 (the user's own cleanup) -------------------------------------------------
    lab.base_job(pg("ALTER TABLE items DROP COLUMN note; DELETE FROM items WHERE id > 500") + "; docker compose exec -T app rm -f /uploads/seed.bin")
    c2 = lab.ckpt()
    rc, doc, err, _ = lab.open_split(c1["checkpoint"], ["fix"])
    fix = (doc.get("restore") or {}).get("fix", {})
    split2 = doc.get("id")
    tf = read_truth(lab, lab.lane_paths["fix"])
    ok_f, parts_f = truth_ok(tf, M1)
    c["rst01_c1_c4_c5"] = check(rc == 0 and fix.get("ok") is True and ok_f, "ok:true and truth(fix, M(C1)): 1000 items, note, seed.bin back",
                                {"ok": fix.get("ok"), "parts": parts_f, "raw": {k: tf.get(k) for k in ("ITEMS", "NOTE", "SEED")}})
    orig_after = lab.base_job(pg("select count(*) from items") + "; docker compose exec -T app test -e /uploads/seed.bin && echo SEED-PRESENT || echo seed-absent")[1].split()
    c["rst01_c4_original_unchanged_by_the_restore"] = check(orig_after == ["500", "seed-absent"], "the original is still broken: 500 items, no seed.bin", orig_after)
    rc2, doc2, err2, _ = lab.open_split(c2["checkpoint"], ["late"])
    late = (doc2.get("restore") or {}).get("late", {})
    tl = read_truth(lab, lab.lane_paths["late"])
    m2 = {"items": 500, "note": False, "row1": "nonce-orig", "seed": ""}
    ok_l, parts_l = truth_ok(tl, {**M1, **m2})
    parts_l.pop("markers", None)
    c["rst01_c5_late_lane_has_the_later_state"] = check(
        rc2 == 0 and late.get("ok") is True and as_int(tl.get("ITEMS")) == 500 and as_int(tl.get("NOTE")) == 0 and tl.get("ROW1") == "nonce-orig",
        "truth(late, M(C2)): 500 items, no note column, row 1 = nonce-orig (the app re-seeds seed.bin when it starts, so that file is not a sentinel of C2)",
        {"ok": late.get("ok"), "line": late.get("line"), "c2": {k: (c2.get("services") or {}).get(k) for k in ("snapshot", "reused_from", "gaps")},
         "raw": {k: tl.get(k) for k in ("ITEMS", "NOTE", "ROW1", "SEED")}})
    # ---- RST-05 fork of a fork, CAP-07 c3 ----------------------------------------------------------------------
    fix_dir = lab.lane_paths["fix"]
    lab.lane_job(fix_dir, pg("UPDATE items SET name='gen1' WHERE id=4") + "; " + pg("INSERT INTO markers VALUES ('secret-fix')"))
    lab.base_job(pg("INSERT INTO markers VALUES ('secret-base')"))
    # a lane's volumes have just been written: flush them, as a user's own checkpoint-worthy moment would (a capture that is
    # not done inside the freeze budget is a gap by design, S3)
    lab.lane_job(fix_dir, pg("CHECKPOINT"))
    lab.vx("sync", vm=vm)
    c3 = lab.ckpt(lane=f"{split2}/fix")
    s3 = c3.get("services") or {}
    c["rst05_c1_lane_checkpoint_has_services"] = check(bool(s3.get("snapshot")), "services non-null", {k: s3.get(k) for k in ("snapshot", "gaps")})
    lab.lane_job(fix_dir, pg("UPDATE items SET name='after-c3' WHERE id=4"))
    rc3, doc3, err3, _ = lab.open_split(c3["checkpoint"], ["g2"])
    g2 = (doc3.get("restore") or {}).get("g2", {})
    split3 = doc3.get("id")
    tg = read_truth(lab, lab.lane_paths["g2"], row=4)
    mk = lab.lane_job(lab.lane_paths["g2"], pg("select string_agg(name, ',' order by name) from markers"))[1].strip()
    lab.lane_job(lab.lane_paths["g2"], pg("UPDATE items SET name='gen2' WHERE id=5"))
    c["rst05_c2_g2_has_the_lane_state"] = check(
        rc3 == 0 and g2.get("ok") is True and as_int(tg.get("NOTE")) == 1 and tg.get("ROW", "").startswith("gen1/"),
        "g2: ok:true, the first lane's state (note present, row 4 = gen1)", {"ok": g2.get("ok"), "row4": tg.get("ROW"), "note": tg.get("NOTE"), "line": g2.get("line"),
                                                                    "gaps": g2.get("gaps"), "c3": {k: s3.get(k) for k in ("snapshot", "gaps")}})
    c["cap07_c3_restore_holds_only_that_lanes_data"] = check(
        "secret-fix" in mk and "secret-base" not in mk, "g2 holds the lane's secret and not the base's", mk)
    base_g = lab.base_job(pg("select name from items where id=4") + "; " + pg("select count(*) from items where name='gen1'"))[1].split()
    fix_g = lab.lane_job(fix_dir, pg("select count(*) from items where name='gen2'") + "; " + pg("select name from items where id=4"))[1].split()
    c["rst05_c3_c4_original_and_g1_untouched"] = check(
        base_g[1] == "0" and fix_g == ["0", "after-c3"], "the original lacks gen1; g1 lacks gen2", {"base": base_g, "fix": fix_g})
    for sid in (split3, split2):
        lab.close_split(sid)
        lab.run(["splits", "rm", sid], cwd=project, timeout=120)
    # ---- CAP-06: the user's own cleanup cannot touch the checkpoint ---------------------------------------------
    lab.base_job("docker compose down -v >/dev/null 2>&1; docker volume prune -af >/dev/null 2>&1; docker builder prune -af >/dev/null 2>&1; echo done")
    live = lab.vx("docker volume ls -q --filter label=dev.marsh.scope=base | wc -l", vm=vm).stdout.strip()
    rcx, docx, errx, _ = lab.open_split(c1["checkpoint"], ["x"])
    x = (docx.get("restore") or {}).get("x", {})
    tx = read_truth(lab, lab.lane_paths["x"])
    ok_x, parts_x = truth_ok(tx, M1)
    c["cap06_c1_live_volumes_gone"] = check(live == "0", "no live base volume", live)
    c["cap06_c2_fork_has_the_data"] = check(rcx == 0 and x.get("ok") is True and ok_x, "truth(fork, M(C1)) and ok:true", {"ok": x.get("ok"), "parts": parts_x, "line": x.get("line")})
    c["cap06_c3_snapshot_unchanged"] = check(hash1 is not None and lab.pool_snapshot_hash(snap1) == hash1, "C1's snapshot hash is unchanged", hash1)
    lab.close_split(docx.get("id"))
    return c


@scenario("smoke")
def smoke(args, lab: Lab) -> dict:
    """One lane from one checkpoint: the first thing to run."""
    c: dict = {}
    lab.use([])
    lab.setup_stack(load=1)
    time.sleep(4)
    c1 = lab.ckpt()
    print("CHECKPOINT", json.dumps(c1.get("services"))[:600], flush=True)
    rc, doc, err, secs = lab.open_split(c1["checkpoint"], ["a"])
    print("OPEN", rc, round(secs, 1), json.dumps(doc)[:1500], "\nSTDERR", err[-600:], flush=True)
    la = lab.lane_paths.get("a")
    t = read_truth(lab, la) if la else {}
    print("TRUTH", t, flush=True)
    ok, parts = truth_ok(t, M1)
    c["smoke_truth"] = check(ok, "truth(lane, M1)", parts)
    sid = doc.get("id")
    lab.close_split(sid)
    c["smoke_close_drops"] = check(empty(split_objects(lab, sid, ["a"])), "no fork objects", split_objects(lab, sid, ["a"]))
    return c


# ----------------------------------------------------------------------------------------------- reuse
@scenario("reuse")
def reuse(args, lab: Lab) -> dict:
    """A checkpoint taken while the VM was idle-stopped (`reused_from`) restores the right data (CAP-08 c3 shape)."""
    c: dict = {}
    lab.use([])
    lab.setup_stack(load=0)
    c1 = lab.ckpt()
    lab.base_job("for i in $(seq 1 25); do " + pg("INSERT INTO markers VALUES ('post-c1-'||" + "$i)") + " >/dev/null; done; echo ok")
    state = poll(lambda: lab.vm_status() == "stopped", 240, 5)
    c["idle_stop"] = check(bool(state), "the services VM idle-stopped", lab.vm_status())
    c2 = lab.ckpt()
    s2 = c2.get("services") or {}
    c["reused_from"] = check(bool(s2.get("reused_from")) and not s2.get("snapshot") and lab.vm_status() == "stopped",
                             "reused_from set, no own snapshot, the VM stayed stopped", {k: s2.get(k) for k in ("snapshot", "reused_from", "gaps")})
    rc, doc, err, secs = lab.open_split(c2["checkpoint"], ["x"])
    x = (doc.get("restore") or {}).get("x", {})
    n = lab.lane_job(lab.lane_paths["x"], pg("select count(*) from markers where name like 'post-c1-%'"))[1].strip()
    c["restores_the_stopped_turns_data"] = check(rc == 0 and x.get("ok") is True and n == "25",
                                                 "ok:true and the 25 post-c1 rows are in the lane", {"ok": x.get("ok"), "rows": n, "seconds": round(secs, 1), "err": err[-300:]})
    lab.close_split(doc.get("id"))
    return c


# --------------------------------------------------------------------------------------- hostile results
HOSTILE = [("fault-restore-bad-file-name", "a lane file named ../../escape/.env"),
           ("fault-restore-huge-file", "a 200 KiB .env"),
           ("fault-restore-nul", "a NUL in .env"),
           ("fault-restore-bad-port", "port 70000"),
           ("fault-restore-bad-name", "a service name with an escape and a newline")]


@scenario("hostile")
def hostile(args, lab: Lab) -> dict:
    """The restore result is hostile text: bad or oversized fields give ok:false with a gap and write nothing (E1-R2)."""
    c: dict = {}
    lab.use([])
    project = lab.setup_stack(load=0)
    c1 = lab.ckpt()
    vm = lab.vm()
    base_before = sorted(lab.scope_objects("base")["volumes"])
    escape = pathlib.Path(lab.root) / "escape"
    for mode, what in HOSTILE:
        lab.relaunch([mode])
        rc, doc, err, _ = lab.open_split(c1["checkpoint"], ["h"])
        lane = (doc.get("restore") or {}).get("h", {})
        lane_dir = lab.lane_paths.get("h")
        files = sorted(p.name for p in lane_dir.iterdir() if p.name in ("compose.fork.yaml",)) if lane_dir else ["?"]
        env_has = (lane_dir / ".env").exists() if lane_dir else None
        objects = lab.scope_objects(f"fork-{doc.get('id')}-h")
        c[f"{mode}"] = check(
            rc == 0 and lane.get("ok") is False and any("refused" in g for g in lane.get("gaps", [])) and not files and empty({"h": objects}) and
            not escape.exists() and not R10.search(lane.get("line", "")),
            f"{what}: ok:false, a gap saying it was refused, no compose.fork.yaml in the lane, no fork object left, nothing outside the lane",
            {"ok": lane.get("ok"), "gaps": lane.get("gaps"), "line": lane.get("line"), "files": files, "objects": objects, "env_exists": env_has})
        lab.close_split(doc.get("id"))
    # the markup fault: display text only, the restore still works and its line is plain
    lab.relaunch(["fault-restore-markup"])
    rc, doc, err, _ = lab.open_split(c1["checkpoint"], ["m"])
    lane = (doc.get("restore") or {}).get("m", {})
    reasons = " ".join(i.get("reason", "") for i in lane.get("fresh", []))
    c["fault-restore-markup"] = check(
        rc == 0 and "\x1b" not in json.dumps(lane) and "\\u001b" not in json.dumps(lane) and "\n" not in lane.get("line", "") and "\r" not in reasons,
        "escape characters and newlines from the VM are gone from every field and the line", {"line": lane.get("line"), "reasons": reasons[:200]})
    lab.close_split(doc.get("id"))
    base_after = sorted(lab.scope_objects("base")["volumes"])
    c["original_untouched"] = check(bool(base_before) and base_after == base_before, "the base's own volumes are unchanged by every restore", {"before": base_before, "after": base_after})
    return c


# ---------------------------------------------------------------------------------------- failures (RST-08)
@scenario("failures")
def failures(args, lab: Lab) -> dict:
    """RST-08: a failed or partial restore is ok:false with gaps and a line that says so, never half started."""
    c: dict = {}
    lab.use([])
    lab.setup_stack(load=0)
    c1 = lab.ckpt()
    # (b) the agent's fault: the data is not installed
    lab.relaunch(["fault-restore-skip-volumes"])
    rc, doc, err, _ = lab.open_split(c1["checkpoint"], ["f"])
    lane = (doc.get("restore") or {}).get("f", {})
    c["rst08_b_ok_false_and_the_line_says_so"] = check(
        rc == 0 and lane.get("ok") is False and "Not everything came back" in lane.get("line", "") and not R10.search(lane.get("line", "")),
        "ok:false, a failure clause naming the services, no internal words", {"ok": lane.get("ok"), "line": lane.get("line"), "gaps": lane.get("gaps")})
    left = lab.scope_objects(f"fork-{doc.get('id')}-f")
    c["rst08_c3_no_half_started_fork"] = check(empty({"f": left}), "nothing of the fork is left", left)
    lab.close_split(doc.get("id"))
    # (c) the checkpoint itself has a gap: no snapshot
    lab.relaunch(["fault-capture-fail"])
    c_gap = lab.ckpt()
    sg = c_gap.get("services") or {}
    lab.relaunch([])
    rc, doc, err, _ = lab.open_split(c_gap["checkpoint"], ["g"])
    lane = (doc.get("restore") or {}).get("g", {})
    c["rst08_c_capture_gap"] = check(
        rc == 0 and not (sg.get("snapshot") or sg.get("reused_from")) and lane.get("ok") is False and "wasn't saved" in lane.get("line", ""),
        "ok:false and the line says this turn's app data wasn't saved", {"services": sg, "ok": lane.get("ok"), "line": lane.get("line")})
    lab.close_split(doc.get("id"))
    return c


# ------------------------------------------------------------------------------------------------- blockers
@scenario("blockers")
def blockers(args, lab: Lab) -> dict:
    """RST-10: fork-unsafe Compose features are reported, never attempted."""
    c: dict = {}
    lab.use([])
    lab.start()
    project = e2.stage_fixture(args, "stack-a-blockers", lab.project / "app")
    subprocess.run(["sh", str(pathlib.Path(args.fixtures) / "stack-a-blockers/setup.sh")], cwd=project, check=False, capture_output=True)
    for argv in (["git", "init", "-q", "-b", "main"], ["git", "add", "-A"],
                 ["git", "-c", "user.name=nfx", "-c", "user.email=nfx@example.invalid", "-c", "commit.gpgsign=false", "commit", "-q", "-m", "stage"]):
        subprocess.run(argv, cwd=project, check=True, capture_output=True)
    lab.project_dir = project
    lab.enable_services(project)
    # the external volume the fixture's compose names: created through the job's endpoint, as a user would
    lab.base_job("docker volume create nfx-ext >/dev/null; docker compose up -d --wait 2>&1 | tail -2", timeout=900)
    ids = lab.vx("docker ps -aq --filter label=dev.marsh.scope=base | xargs -r docker inspect --format '{{.Name}} {{.Id}} {{.State.StartedAt}}'", vm=lab.vm()).stdout
    c1 = lab.ckpt()
    rc, doc, err, _ = lab.open_split(c1["checkpoint"], ["x"])
    lane = (doc.get("restore") or {}).get("x", {})
    gaps = " ".join(lane.get("gaps", []))
    fork_ws = f"fork-{doc.get('id')}-x"
    mounts = lab.vx(f"docker ps -aq --filter label=dev.marsh.scope={fork_ws} | xargs -r docker inspect --format '{{{{.Name}}}} {{{{range .Mounts}}}}{{{{.Name}}}} {{{{end}}}}'", vm=lab.vm()).stdout
    after = lab.vx("docker ps -aq --filter label=dev.marsh.scope=base | xargs -r docker inspect --format '{{.Name}} {{.Id}} {{.State.StartedAt}}'", vm=lab.vm()).stdout
    c["rst10_c1_original_unchanged"] = check(ids == after and bool(ids), "the base's containers keep ids and StartedAt", {"before": ids[-200:], "after": after[-200:]})
    c["rst10_c2_gaps_name_each_blocker"] = check(
        all(n in gaps for n in ("nfx-mail", "nfx-ext")) and lane.get("ok") is False, "the gaps name the fixed name and the external volume; ok:false",
        {"gaps": lane.get("gaps"), "line": lane.get("line"), "restored": restored_services(lane)})
    c["rst10_c3_no_fork_container_mounts_a_blocker"] = check("nfx-ext" not in mounts, "no fork container mounts nfx-ext", mounts[-300:])
    lab.close_split(doc.get("id"))
    return c


# --------------------------------------------------------------------------------------------- perf (PERF-03)
@scenario("perf3")
def perf3(args, lab: Lab) -> dict:
    """PERF-03: from `split --open --from` until the lane job's localhost returns the fork's data, N samples."""
    c: dict = {}
    lab.use([])
    lab.setup_stack(load=0)
    c1 = lab.ckpt()
    n = int(args.n)
    totals, opens, firsts = [], [], []
    for i in range(n + 1):
        t0 = time.monotonic()
        rc, doc, err, secs = lab.open_split(c1["checkpoint"], ["p"])
        lane = (doc.get("restore") or {}).get("p", {})
        code, out, _ = lab.lane_job(lab.lane_paths["p"], pg("select count(*) from items"))
        t2 = time.monotonic() - t0
        good = rc == 0 and lane.get("ok") is True and out.strip() == "1000"
        if i:  # the first run is a warm-up
            totals.append(t2) if good else None
            opens.append(secs)
            firsts.append(t2 - secs)
        lab.close_split(doc.get("id"))
        lab.run(["splits", "rm", doc.get("id")], cwd=lab.project_dir, timeout=120)
        if not good:
            c[f"sample_{i}"] = check(False, "ok:true and 1000 items", {"rc": rc, "ok": lane.get("ok"), "out": out.strip(), "err": err[-200:]})
    totals.sort()
    p50 = statistics.median(totals) if totals else None
    p95 = totals[min(len(totals) - 1, int(round(0.95 * (len(totals) - 1))))] if totals else None
    c["perf03"] = check(len(totals) == n and p95 is not None and p95 < 10, f"p95 < 10 s over {n} samples (the plan's N is 20)",
                       {"n": len(totals), "p50": round(p50 or 0, 2), "p95": round(p95 or 0, 2), "max": round(max(totals or [0]), 2),
                        "open_p50": round(statistics.median(opens), 2) if opens else None,
                        "job_after_open_p50": round(statistics.median(firsts), 2) if firsts else None})
    return c


@scenario("rst2")
def rst2(args, lab: Lab) -> dict:
    """RST-02: ten checkpoints under load, each opened, read, closed and removed."""
    c: dict = {}
    lab.use([])
    lab.setup_stack(load=1)
    counts, bad = [], []
    for i in range(int(args.n)):
        time.sleep(3)
        ck = lab.ckpt()
        rc, doc, err, _ = lab.open_split(ck["checkpoint"], ["t"])
        lane = (doc.get("restore") or {}).get("t", {})
        t = read_truth(lab, lab.lane_paths["t"])
        counts.append(as_int(t.get("TOKENS")))
        if not (rc == 0 and lane.get("ok") is True and as_int(t.get("TOKNOFILE")) == 0 and as_int(t.get("FILENOTOK")) in (0, 1) and as_int(t.get("ITEMS")) == 1000):
            bad.append({"i": i, "ok": lane.get("ok"), "t": {k: t.get(k) for k in ("ITEMS", "TOKENS", "TOKNOFILE", "FILENOTOK")}, "gaps": lane.get("gaps")})
        lab.close_split(doc.get("id"))
        lab.run(["splits", "rm", doc.get("id")], cwd=lab.project_dir, timeout=120)
        lab.run(["checkpoints", "rm", ck["checkpoint"]], cwd=lab.project_dir, timeout=120)
    c["rst02_c2_every_fork_is_consistent"] = check(not bad, "0 tokens without a file, files beyond tokens in {0,1}, every restore ok:true", bad or counts)
    c["rst02_c3_counts_increase"] = check(all(b > a for a, b in zip(counts, counts[1:])), "token counts increase across checkpoints", counts)
    return c


# ----------------------------------------------------------------------------------------- close, cancel
@scenario("cancel")
def cancel(args, lab: Lab) -> dict:
    """RST-09: close, cancel and rm drop the forks (the daemon runs with breaks for the interrupted restore)."""
    c: dict = {}
    lab.use([])
    lab.setup_stack(load=0)
    c1 = lab.ckpt()
    base_pool = lab.pool_used()
    # cancel of an open split with running lanes
    rc, doc, err, _ = lab.open_split(c1["checkpoint"], ["a", "b"])
    sid = doc.get("id")
    before = split_objects(lab, sid, ["a", "b"])
    cancelled = lab.run(["splits", "cancel", sid], cwd=lab.project_dir, timeout=300)
    gone = poll(lambda: empty(split_objects(lab, sid, ["a", "b"])), 90, 2)
    c["cancel_drops_the_forks"] = check(cancelled.returncode == 0 and bool(gone) and not all(empty({k: v}) for k, v in before.items()),
                                       "the lanes had objects and, after cancel, none is left", {"before": {k: {n: len(x) for n, x in v.items()} for k, v in before.items()},
                                                                                           "cancel": cancelled.stderr[-200:]})
    # close then rm of a kept split
    rc, doc, err, _ = lab.open_split(c1["checkpoint"], ["a"])
    sid2 = doc.get("id")
    lab.close_split(sid2)
    left_after_close = split_objects(lab, sid2, ["a"])
    lab.run(["splits", "rm", sid2], cwd=lab.project_dir, timeout=120)
    left_after_rm = split_objects(lab, sid2, ["a"])
    c["close_then_rm"] = check(empty(left_after_close) and empty(left_after_rm), "nothing left after close, nor after rm", {"close": left_after_close, "rm": left_after_rm})
    pool_after = lab.pool_used()
    c["pool_within_64mib_of_baseline"] = check(pool_after <= base_pool + (64 << 20), "pool within 64 MiB of the baseline", {"baseline_mib": base_pool >> 20, "after_mib": pool_after >> 20})
    # a lane whose VM was stopped when it closed is dropped when the VM is back
    rc, doc, err, _ = lab.open_split(c1["checkpoint"], ["s"])
    sid3 = doc.get("id")
    stopped = lab.run(["services", "stop"], cwd=lab.project_dir, timeout=300)
    closed = lab.close_split(sid3)
    lab.run(["services", "start"], cwd=lab.project_dir, timeout=900)
    done = poll(lambda: empty(split_objects(lab, sid3, ["s"])), 120, 3)
    c["drop_retried_when_the_vm_is_back"] = check(stopped.returncode == 0 and closed.returncode == 0 and bool(done),
                                                  "a lane closed while the VM was stopped has nothing left once it runs again",
                                                  split_objects(lab, sid3, ["s"]))
    return c


# ------------------------------------------------------------------------------------------- break cells
def cell_stack(lab: Lab) -> tuple[pathlib.Path, dict]:
    lab.use([])
    project = lab.setup_stack(load=0)
    return project, lab.ckpt()


def cells(args, lab: Lab, project, c1, wanted: set[str]) -> dict:
    """Each break at its named check, and the same check on the normal build (pass 1)."""
    c: dict = {}
    vm = lab.vm()
    chk = c1["checkpoint"]

    def run_cell(mode, breaks, body):
        if wanted and mode not in wanted:
            return
        if args.normal:
            # pass 1: the same check on the normal build (only the faults, which are inputs, stay)
            breaks = [b for b in breaks if b.startswith("fault-")]
        lab.relaunch(breaks)
        lab.base_job("docker compose up -d --wait 2>&1 | tail -1", timeout=900)  # the base stack (a relaunch starts a new engine)
        try:
            body(("normal/" if args.normal else "") + mode)
        except Exception as error:  # noqa: BLE001
            c[f"{mode}/crashed"] = check(False, "ran", f"{type(error).__name__}: {error}")

    def open_and_line(label):
        rc, doc, err, _ = lab.open_split(chk, [label])
        lane = (doc.get("restore") or {}).get("x", {}) if False else (doc.get("restore") or {}).get(label, {})
        return rc, doc, lane

    def line_checks(mode):
        rc, doc, lane = open_and_line("x")
        line = lane.get("line", "")
        names_fresh = all(n in line for n in ("redis", "mail"))
        c[f"{mode}/c2_fresh_in_the_line"] = check(names_fresh, "the line names the fresh services redis and mail", line)
        c[f"{mode}/c5_no_internal_words"] = check(bool(line) and not R10.search(line) and len(line) <= 400 and "\n" not in line,
                                                 "one line, <= 400, no internal words", line)
        lab.close_split(doc.get("id"))

    for mode in ("line-omit-fresh", "line-internal-words", "restore-line-trust-vm"):
        run_cell(mode, [mode], line_checks)

    def ports_cell(mode):
        rc, doc, lane = open_and_line("x")
        n = lab.lane_job(lab.lane_paths["x"], pg("select count(*) from items"))[1].strip()
        c[f"{mode}/rst03_c1_original_ports_reach_the_fork"] = check(n == "1000", "the lane's localhost:5432 reaches the fork (1000 items)", n)
        lab.close_split(doc.get("id"))
    run_cell("lane-ports-unmapped", ["lane-ports-unmapped"], ports_cell)

    def scope_cell(mode):
        rc, doc, lane = open_and_line("x")
        base_ids = sorted(lab.scope_objects("base")["containers"])
        lab.lane_job(lab.lane_paths["x"], "docker rm -f $(docker ps -aq) >/dev/null 2>&1; echo done")
        after = sorted(lab.scope_objects("base")["containers"])
        c[f"{mode}/rst11_c1_base_keeps_its_containers"] = check(bool(base_ids) and base_ids == after, "the base's containers survive a lane's `rm -f $(docker ps -aq)`",
                                                               {"before": len(base_ids), "after": len(after)})
        lab.close_split(doc.get("id"))
    run_cell("lane-scope-root", ["lane-scope-root"], scope_cell)

    def drop_cell(mode):
        rc, doc, lane = open_and_line("x")
        sid = doc.get("id")
        lab.close_split(sid)
        left = lab.scope_objects(f"fork-{sid}-x")
        c[f"{mode}/rst09_c1_zero_fork_objects_after_close"] = check(empty({"x": left}), "no object of the lane after close", left)
        lab.run(["splits", "rm", sid], cwd=project, timeout=120)
    for mode in ("lane-drop-off", "drop-noop"):
        run_cell(mode, [mode], drop_cell)

    def fault_cell(mode):
        rc, doc, lane = open_and_line("f")
        c[f"{mode}/rst08_c1_ok_equals_truth"] = check(lane.get("ok") is False, "ok:false (the data was not installed)", {"ok": lane.get("ok"), "line": lane.get("line")})
        c[f"{mode}/rst08_c2_line_has_a_failure_clause"] = check("Not everything came back" in lane.get("line", ""), "the line says it did not all come back", lane.get("line"))
        lab.close_split(doc.get("id"))
    for mode in ("restore-silent-fail", "line-always-ok"):
        run_cell(mode, ["fault-restore-skip-volumes", mode], fault_cell)

    def generated_cell(mode):
        rc, doc, lane = open_and_line("x")
        sid = doc.get("id")
        lab.lane_job(lab.lane_paths["x"], "echo from-x > x.txt")
        lab.close_split(sid)
        applied = lab.run(["split", "apply", sid, "x", "--3way"], cwd=project, timeout=300)
        present = (project / "compose.fork.yaml").exists()
        c[f"{mode}/rst09_c3_no_generated_file_in_the_root"] = check(applied.returncode == 0 and not present, "apply carries x.txt and no compose.fork.yaml", {"rc": applied.returncode, "compose.fork.yaml": present})
        if present:
            subprocess.run(["git", "rm", "-q", "-f", "compose.fork.yaml"], cwd=project, capture_output=True)
        for path in ("x.txt", ".env.applied"):
            (project / path).unlink(missing_ok=True)
        subprocess.run(["git", "reset", "-q", "--hard"], cwd=project, capture_output=True)
        lab.run(["splits", "rm", sid], cwd=project, timeout=120)
    run_cell("apply-include-generated", ["apply-include-generated"], generated_cell)
    return c


@scenario("cells")
def cells_scenario(args, lab: Lab) -> dict:
    project, c1 = cell_stack(lab)
    wanted = {m for m in args.cells.split(",") if m}
    return cells(args, lab, project, c1, wanted)


# ---- holds: SEC-22 (A9), CAP-04 c3 ------------------------------------------------------------------------
def hold(lab: Lab, point: str) -> pathlib.Path:
    return lab.home / "test-holds" / point


def wait_reached(lab: Lab, point: str, ceiling=120) -> bool:
    return bool(poll(lambda: (lab.home / "test-holds" / f"{point}.reached").exists(), ceiling, 0.3))


def release(lab: Lab, point: str) -> None:
    (lab.home / "test-holds").mkdir(exist_ok=True)
    (lab.home / "test-holds" / f"{point}.release").write_text("")


def background_open(lab: Lab, checkpoint: str, labels: list[str]):
    result: dict = {}

    def target():
        t = lab.open_split(checkpoint, labels)
        result["value"] = t
    thread = threading.Thread(target=target)
    thread.start()
    return thread, result


@scenario("holds")
def holds(args, lab: Lab) -> dict:
    """SEC-22 (A9) and CAP-04 c3. Pass 2 with the holds; the normal pass of SEC-22 is the same script with no break."""
    c: dict = {}
    wanted = {m for m in args.cells.split(",") if m}
    lab.use([])
    project = lab.setup_stack(load=0)
    c1 = lab.ckpt()
    chk = c1["checkpoint"]
    snap = (c1.get("services") or {}).get("snapshot")
    canary = lab.root / "canary"
    canary.mkdir()

    def sec22(mode, breaks):
        if wanted and mode not in wanted:
            return
        lab.relaunch(["hold-lane-files-write", *breaks])
        (lab.home / "test-holds").mkdir(exist_ok=True)
        for leftover in canary.iterdir():  # this run's own canary directory, emptied between cells
            leftover.unlink()
        for name in ("env", "override"):
            (canary / name).write_text(f"CANARY-{name}\n")
        before = {n: hashlib.sha256((canary / n).read_bytes()).hexdigest() for n in ("env", "override")}
        thread, result = background_open(lab, chk, ["x", "y"])
        reached = wait_reached(lab, "hold-lane-files-write")
        # pass 2: at the hold the lanes have no generated file yet (only marshd writes them, after the hold)
        splits = [s for s in lab.splits() if s.get("state") == "run"]
        sid = splits[0]["id"] if splits else None
        lane_x = project / ".marsh/split" / str(sid) / "x"
        lane_y = project / ".marsh/split" / str(sid) / "y"
        early = sorted(p for p in ("compose.fork.yaml",) if (lane_x / p).exists())
        c[f"{mode}/a9_no_generated_file_before_marshd_writes"] = check(reached and not early,
                                                                      "at hold-lane-files-write the lane has no compose.fork.yaml", {"reached": reached, "early": early})
        # x: symlinks to the canaries; y: the lane directory swapped for a link to the canary directory
        (lane_x / ".env").unlink(missing_ok=True)
        (lane_x / ".env").symlink_to(canary / "env")
        (lane_x / "compose.fork.yaml").unlink(missing_ok=True)
        (lane_x / "compose.fork.yaml").symlink_to(canary / "override")
        moved = lane_y.with_name("y-moved")
        lane_y.rename(moved)
        lane_y.symlink_to(canary)
        release(lab, "hold-lane-files-write")
        thread.join(timeout=900)
        rc, doc, err, _ = result.get("value", (1, {}, "", 0))
        after = {n: hashlib.sha256((canary / n).read_bytes()).hexdigest() for n in ("env", "override")}
        listing = sorted(p.name for p in canary.iterdir())
        regular = all((lane_x / n).is_file() and not (lane_x / n).is_symlink() for n in (".env", "compose.fork.yaml"))
        refused = (doc.get("restore") or {}).get("y", {})
        c[f"{mode}/sec22_c1_canaries_unchanged_and_no_new_file"] = check(
            before == after and listing == ["env", "override"] and (regular or (doc.get("restore") or {}).get("x", {}).get("ok") is False),
            "the canaries are unchanged, nothing new appears beside them, the lane files are regular files or the lane was refused",
            {"before": before, "after": after, "listing": listing, "x_regular": regular, "y": refused.get("gaps"), "x_ok": (doc.get("restore") or {}).get("x", {}).get("ok")})
        c[f"{mode}/sec22_y_swapped_lane_refused"] = check(refused.get("ok") is False, "the lane whose directory was swapped for a link is ok:false", {"ok": refused.get("ok"), "gaps": refused.get("gaps")})
        lane_y.unlink(missing_ok=True)
        moved.rename(lane_y)
        if sid:
            lab.close_split(sid)

    sec22("sec22-normal", [])
    sec22("lane-files-follow-symlinks", ["lane-files-follow-symlinks"])
    sec22("restore-files-from-agent", ["restore-files-from-agent"])

    # CAP-04 c3: rm of a checkpoint a restore holds
    if not wanted or "cap04" in wanted:
        lab.relaunch(["hold-restore-after-lease"])
        thread, result = background_open(lab, chk, ["l"])
        reached = wait_reached(lab, "hold-restore-after-lease")
        removed = lab.run(["checkpoints", "rm", chk], cwd=project, timeout=60)
        survives = subprocess.run([lab.args.sbx, "exec", "-u", "root", lab.vm(), "docker", "exec", ENGINE, "ls", f"/pool/snapshots/{snap}"],
                                  capture_output=True, text=True).returncode == 0
        release(lab, "hold-restore-after-lease")
        thread.join(timeout=900)
        rc, doc, err, _ = result.get("value", (1, {}, "", 0))
        lane = (doc.get("restore") or {}).get("l", {})
        c["cap04_c3_held_rm_fails_and_the_snapshot_survives"] = check(
            reached and removed.returncode != 0 and "in use" in removed.stderr and survives and lane.get("ok") is True,
            "rm exits non-zero with the in-use message, the snapshot survives until the clone, the restore ok:true",
            {"reached": reached, "rm_rc": removed.returncode, "rm_err": removed.stderr[-160:], "snapshot_present": survives, "ok": lane.get("ok")})
        if doc.get("id"):
            lab.close_split(doc.get("id"))
    # RST-09 c4: cancel during an interrupted restore
    if not wanted or "cancel-mid-restore" in wanted:
        lab.relaunch(["hold-restore-before-install"])
        thread, result = background_open(lab, chk, ["k"])
        reached = poll(lambda: bool(lab.vx("ls /run/marsh-test-holds/hold-restore-before-install.reached 2>/dev/null; true", vm=lab.vm()).stdout.strip()) if False else
                       subprocess.run([lab.args.sbx, "exec", "-u", "root", lab.vm(), "docker", "exec", ENGINE, "sh", "-c",
                                       "test -e /run/marsh-test-holds/restore-before-install.reached && echo y"], capture_output=True, text=True).stdout.strip() == "y", 120, 0.5)
        sid = next((s["id"] for s in lab.splits() if s.get("state") == "run"), None)
        cancelled = lab.run(["splits", "cancel", str(sid)], cwd=project, timeout=300) if sid else None
        subprocess.run([lab.args.sbx, "exec", "-u", "root", lab.vm(), "docker", "exec", ENGINE, "sh", "-c",
                        "touch /run/marsh-test-holds/restore-before-install.release"], capture_output=True, text=True)
        thread.join(timeout=900)
        gone = poll(lambda: empty(split_objects(lab, str(sid), ["k"])), 120, 2)
        c["rst09_c4_cancel_mid_restore_leaves_nothing"] = check(bool(reached and sid and gone), "the lane's objects are gone after the cancel",
                                                               {"reached": reached, "sid": sid, "left": split_objects(lab, str(sid), ["k"])})
    return c


# ------------------------------------------------------------------------------------------------- mixed
@scenario("mixed")
def mixed(args, lab: Lab) -> dict:
    """RST-07: an honest restore line across service types (restored, fresh, kept without a container, not restored)."""
    c: dict = {}
    lab.use([])
    project = lab.setup_stack("stack-a-mixed", load=0)
    # distinct pre-capture values: redis k=v1, one mail, an anon row, a writable file; keeper with its notes volume; a Testcontainers container
    lab.base_job("redis-cli -h localhost set k v1 >/dev/null; curl -s -X POST localhost:8025/api/v1/send -H 'Content-Type: application/json' "
                 "-d '{\"From\":{\"Email\":\"a@b.c\"},\"To\":[{\"Email\":\"d@e.f\"}],\"Subject\":\"one\"}' >/dev/null; "
                 "docker run -d --name keeper -v notes:/d busybox:1.37 sh -c 'echo n1 > /d/m; exec sleep 1d' >/dev/null; "
                 "docker run -d --name tc-held --label org.testcontainers=true --label org.testcontainers.sessionId=nfx busybox:1.37 sleep 1d >/dev/null; echo ok")
    c1 = lab.ckpt()
    rc, doc, err, _ = lab.open_split(c1["checkpoint"], ["x"])
    lane = (doc.get("restore") or {}).get("x", {})
    line = lane.get("line", "")
    restored = restored_services(lane)
    volumes = [i for i in lane.get("restored", []) if i.get("kind") == "volume"]
    name = volumes[0]["name"] if volumes else "none"
    cat = lab.lane_job(lab.lane_paths["x"], f"docker run --rm -v {name}:/d busybox:1.37 cat /d/m")[1].strip()
    fork_containers = lab.vx(f"docker ps -a --filter label=dev.marsh.scope=fork-{doc.get('id')}-x --format '{{{{.Names}}}}'", vm=lab.vm()).stdout
    c["rst07_c1_restored_and_fresh"] = check(restored == ["app", "db"] and {"anon", "mail", "redis", "writable"} <= set(fresh_services(lane)) | {"anon", "writable"},
                                            "restored = db, app; the other services started fresh", {"restored": restored, "fresh": fresh_services(lane)})
    c["rst07_c2_line_names_everything"] = check(
        "app, db" in line and all(n in line for n in ("redis", "mail")) and "keeper" in line and "tc-held" in line,
        "the restored clause, each fresh service, keeper (not started, by name) and the Testcontainers container (not restored)", line)
    c["rst07_c3_kept_data_back_and_nothing_started"] = check(
        cat == "n1" and "keeper" not in fork_containers and "tc-held" not in fork_containers, "cat prints n1; no keeper or Testcontainers container in the fork", {"cat": cat, "containers": fork_containers})
    c["rst07_c4_ok_true"] = check(lane.get("ok") is True, "ok:true", {"ok": lane.get("ok"), "gaps": lane.get("gaps")})
    c["rst07_c5_one_short_line_no_internal_words"] = check(bool(line) and "\n" not in line and len(line) <= 400 and not R10.search(line) and not re.search(r"\bVM\b", line),
                                                          "one line, <= 400 chars, none of the internal words", line)
    lab.close_split(doc.get("id"))
    return c


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--prefix", required=True)
    parser.add_argument("--breaks-prefix")
    parser.add_argument("--fixtures", required=True)
    parser.add_argument("--sbx", default="sbx")
    parser.add_argument("--images")
    parser.add_argument("--only", default="")
    parser.add_argument("--cells", default="", help="restrict `cells` and `holds` to these modes")
    parser.add_argument("--n", default="8", help="samples for perf3 and rst2")
    parser.add_argument("--normal", action="store_true", help="run the break cells' checks with no break mode (pass 1)")
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()
    args.run = secrets.token_hex(3)
    args.prefix = str(pathlib.Path(args.prefix).expanduser())
    if args.breaks_prefix:
        args.breaks_prefix = str(pathlib.Path(args.breaks_prefix).expanduser())
    args.fixtures = str(pathlib.Path(args.fixtures).expanduser().resolve())
    names = [n for n in (args.only.split(",") if args.only else [x for x in ORDER if x != "smoke"]) if n]
    rc = 0
    for name in names:
        idle = 30 if name == "reuse" else None
        lab = Lab(args, f"c3{name}", [], idle=idle)
        lab.project_dir = lab.project
        result: dict = {}
        crashed = None
        try:
            result = SCENARIOS[name](args, lab)
        except BaseException as error:  # noqa: BLE001
            import traceback
            crashed = f"{type(error).__name__}: {error}"
            traceback.print_exc()
        finally:
            lab.kill_daemon() if False else None
            problems = lab.cleanup()
        ok = crashed is None and not problems and all(x["ok"] for x in result.values())
        jline(scenario=name, run=args.run, ok=ok, crashed=crashed, cleanup_errors=problems, checks=result, scratch=str(lab.root))
        if ok and not args.keep:
            shutil.rmtree(lab.root, ignore_errors=True)  # this scope's own exact directory
        rc |= 0 if ok else 1
    return rc


if __name__ == "__main__":
    sys.exit(main())
