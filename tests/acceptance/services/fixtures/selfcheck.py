#!/usr/bin/env python3
"""Negative self-checks of the fixture tooling: each one feeds a known-bad input to a
checker and shows the defect is CAUGHT (and, where it matters, that the good input is
not rejected). No VM, no marsh, no network.

    selfcheck.py                 run every offline check; exit 1 unless all are as expected
    selfcheck.py --live MARSH    also run HARNESS-03 on a real dev install with injected
                                 failures (a failing `marsh stop`, a JSON-RPC error from the
                                 agent) and show the errors propagate and cleanup is verified;
                                 takes the VM lock itself (harness03.py does)
"""
import argparse
import base64
import json
import os
import pathlib
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import threading
import types

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import harness03  # noqa: E402
import image_cache  # noqa: E402
import sentinels  # noqa: E402
import stage  # noqa: E402
import synthetic_injection  # noqa: E402
import vmlock  # noqa: E402

RESULTS: list[tuple[str, bool, str]] = []
SKIPPED: list[tuple[str, str]] = []
DROP = object()  # a field to remove from a synthetic grant


def record(name: str, caught: bool, detail: str) -> None:
    RESULTS.append((name, caught, detail))
    print(f"{'CAUGHT    ' if caught else 'NOT CAUGHT'}  {name}: {detail}", flush=True)


def skip(name: str, why: str) -> None:
    SKIPPED.append((name, why))
    print(f"SKIPPED     {name}: {why}", flush=True)


def uncached_images() -> list[str]:
    """The saved image tars HARNESS-03's image load needs that are not in `target/nfx/images`."""
    wanted = [image_cache.tar_name(ref) for ref in image_cache.image_set(None)]
    wanted += [image_cache.local_tar_name(entry["tag"]) for entry in image_cache.local_images()]
    return [name for name in wanted if not (image_cache.DEFAULT_OUT / name).is_file()]


# --- 1. checkpoint completeness -------------------------------------------------------

def fake_docker(failing_column: bool):
    """A `run(argv)` answering stack-a-mixed's reads; optionally with the note-column query failing."""
    def run(argv):
        text = " ".join(argv)
        if "information_schema.columns" in text:
            return (1, "") if failing_column else (0, "1\n")
        if "count(*) from items" in text:
            return 0, "1000\n"
        if "name from items where id = 1" in text:
            return 0, "item-1\n"
        if "name from markers" in text:
            return 0, "m-seed-1\nm-seed-2\nm-seed-3\n"
        if "count(*) from tokens" in text:
            return 0, "0\n"
        if "ls /uploads" in text:
            return 0, ""
        if "sha256sum /uploads/seed.bin" in text:
            return 0, "3714b445cbcd86638566b0b43c7ed6d8d9fe0f7fa4131bf6cdc6866297863af3\n"
        if "wc -c" in text:
            return 0, "1048576\n"
        if "redis-cli" in text:
            return 0, "seed\n"
        if "/dev/tcp/mail" in text:
            return 0, 'HTTP/1.0 200 OK\r\n\r\n{"total":0}'
        if "to_regclass" in text:
            return 0, "f\n"
        if "/var/state" in text:
            return 0, "writable-seed\n"
        return 0, ""
    return run


def check_checkpoint() -> None:
    manifest = json.loads((HERE / "stack-a-mixed/sentinels.json").read_text())
    with tempfile.TemporaryDirectory() as scratch:
        project = pathlib.Path(scratch) / "p"
        stage.stage("stack-a-mixed", project)
        good = sentinels.read_fixture(fake_docker(False), "p", project, manifest)
        try:
            sentinels.checkpoint_manifest(manifest, good)
            record("checkpoint: a complete read is accepted", True, "M built, restorable kept")
        except ValueError as error:
            record("checkpoint: a complete read is accepted", False, f"wrongly refused: {error}")
        bad = sentinels.read_fixture(fake_docker(True), "p", project, manifest)
        try:
            sentinels.checkpoint_manifest(manifest, bad)
            record("checkpoint: failed note-column query", False, f"accepted, note_column={bad['db']['note_column']!r}")
        except ValueError as error:
            record("checkpoint: failed note-column query", "note_column" in str(error) and bad["db"]["note_column"] == {"error": "rc=1"},
                   f"flagged incomplete, error preserved: {str(error)[:90]}")
        absent = json.loads(json.dumps(good))
        absent["db"]["note_column"] = False  # the column genuinely is not there: an observation, not an error
        absent["app"]["token_files"] = []
        try:
            M = sentinels.checkpoint_manifest(manifest, absent)
            record("checkpoint: a legitimate False/absent observation is kept apart from errors",
                   M["sentinels"]["db"]["note_column"] is False, "note_column False accepted as an observation")
        except ValueError as error:
            record("checkpoint: a legitimate False/absent observation is kept apart from errors", False, str(error))
        variants = (
            ("empty groups", {"db": {}, "app": {}, "fresh": {}}),
            ("ignored section missing", {k: v for k, v in good.items() if k != "ignored"}),
            ("one fresh sentinel missing", {**good, "fresh": {k: v for k, v in good["fresh"].items() if k != "redis"}}),
            ("an ignored hash missing", {**good, "ignored": {**good["ignored"], "sha256": {**good["ignored"]["sha256"], ".env": None}}}),
            ("sqlite unread", {**good, "ignored": {**good["ignored"], "sqlite": None}}),
        )
        for label, observed in variants:
            try:
                sentinels.checkpoint_manifest(manifest, observed)
                record(f"checkpoint: {label}", False, "accepted")
            except ValueError as error:
                record(f"checkpoint: {label}", True, f"incomplete: {str(error)[:100]}")


# --- 2. SEC-23 body validation ----------------------------------------------------------

def check_sec23() -> None:
    cases = json.loads((HERE / "cases/canonical.json").read_text())["cases"]
    witness = next(c for c in cases if c["id"].startswith("K01"))
    authority = {"scope": "S1", "attempt": "A1", "network": "net-S1", "memory": 268435456, "nano_cpus": 1000000000, "pids_limit": 512}
    enriched = {"Image": "busybox", "Cmd": ["true"], "Labels": {"dev.marsh.scope": "S1", "dev.marsh.attempt": "A1"},
                "HostConfig": {"NetworkMode": "net-S1", "Memory": 268435456, "NanoCpus": 1000000000, "PidsLimit": 512}}

    def verdict(body: bytes) -> tuple[int, dict]:
        target = witness["expect_upstream"]["target"]
        entry = {"conn": 1, "request": {"method": "POST", "target": target, "line": f"POST {target} HTTP/1.1",
                                         "headers": [["Host", "d"], ["Content-Type", "application/json"], ["Content-Length", str(len(body))]],
                                         "body_b64": base64.b64encode(body).decode()}}
        with tempfile.TemporaryDirectory() as scratch:
            log, canonical = pathlib.Path(scratch, "log.jsonl"), pathlib.Path(scratch, "c.json")
            log.write_text(json.dumps(entry) + "\n")
            canonical.write_text(json.dumps({"cases": [witness]}))
            out = subprocess.run([sys.executable, str(HERE / "rawdocker.py"), "--compare-recording", str(log), "--canonical", str(canonical),
                                  "--authority", json.dumps(authority)], capture_output=True, text=True)
        return out.returncode, json.loads(out.stdout.splitlines()[0])

    rc, row = verdict(json.dumps(enriched, separators=(",", ":")).encode())
    record("SEC-23: the correct enriched canonical body passes", rc == 0 and row["ok"], f"ok={row['ok']}")
    for label, body in (("JSON null", b"null"), ("JSON false", b"false"), ("unenriched client fields", b'{"Image":"busybox","Cmd":["true"]}'),
                        ("empty body", b""), ("not JSON", b"{oops")):
        rc, row = verdict(body)
        record(f"SEC-23: body {label}", rc == 1 and not row["ok"], "; ".join(row["problems"])[:110])


# --- 3. event-witness health -------------------------------------------------------------

def fake_engine(path: str, events_file: str, delay: float = 0.0):
    """A fake Docker endpoint: a container create appends a `create` event to EVENTS_FILE; with DELAY,
    the events of `nfxraw-*` creates (not the probes) arrive that many seconds late."""
    if os.path.exists(path):
        os.unlink(path)
    server = socket.socket(socket.AF_UNIX)
    server.bind(path)
    server.listen(16)

    def reply(conn, status, body=""):
        conn.sendall(f"HTTP/1.1 {status}\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n{body}".encode())

    def handle(conn):
        with conn:
            first = conn.recv(65536).split(b"\r\n")[0].decode()
            if first.startswith("POST /containers/create"):
                name = first.split("name=")[1].split(" ")[0]
                line = json.dumps({"Type": "container", "Action": "create", "Actor": {"Attributes": {"name": name}}}) + "\n"

                def emit():
                    with open(events_file, "a") as handle:
                        handle.write(line)
                if delay and name.startswith("nfxraw-"):
                    threading.Timer(delay, emit).start()
                else:
                    emit()
                reply(conn, "201 Created", '{"Id":"0"}')
            elif first.startswith("DELETE"):
                reply(conn, "204 No Content")
            elif first.startswith("GET /containers/"):
                reply(conn, "404 Not Found", '{"message":"No such container"}')
            else:
                reply(conn, "200 OK", "OK")

    def serve():
        while True:
            try:
                conn, _ = server.accept()
            except OSError:
                return
            threading.Thread(target=handle, args=(conn,), daemon=True).start()

    threading.Thread(target=serve, daemon=True).start()
    return server


def check_event_witness() -> None:
    with tempfile.TemporaryDirectory() as scratch:
        scratch = pathlib.Path(scratch)
        sock, events = str(scratch / "e.sock"), scratch / "events"
        events.write_text("")
        engine = fake_engine(sock, str(events))
        create = {"id": "T-create", "send": ["POST /containers/create?name=nfxraw-1 HTTP/1.1\r\nHost: d\r\nContent-Length: 2\r\n\r\n{}"],
                  "responses": 1, "expect": {"statuses": [403], "forbidden_objects": ["nfxraw-1"]}, "cleanup": []}
        slow = {"id": "T-slow", "send": [{"delay_ms": 3500}, "GET /_ping HTTP/1.1\r\nHost: d\r\n\r\n"], "responses": 1,
                "expect": {"statuses": [200]}, "cleanup": []}
        (scratch / "cases.json").write_text(json.dumps({"cases": [create, slow]}))
        (scratch / "dying.py").write_text(
            "import subprocess, sys, time\n"
            f"p = subprocess.Popen(['tail', '-n', '0', '-f', {str(events)!r}], stdout=sys.stdout)\n"
            "time.sleep(2.5)\np.kill()\n")

        def run(events_cmd: str, case_id: str) -> tuple[int, list[dict]]:
            out = subprocess.run([sys.executable, str(HERE / "rawdocker.py"), "--docker-host", f"unix://{sock}", "--calibrate",
                                  "--no-cleanup", "--observer-cmd", "false", "--events-cmd", events_cmd, "--case", case_id,
                                  str(scratch / "cases.json")], capture_output=True, text=True, timeout=120)
            return out.returncode, [json.loads(line) for line in out.stdout.splitlines() if line.strip()]

        rc, rows = run(f"tail -n 0 -f {events}", "T-create")
        case = next((r for r in rows if r.get("id") == "T-create"), {})
        record("event witness: a healthy stream reports the create event (positive control)",
               rc == 0 and case.get("forbidden_created_events") == ["nfxraw-1"], f"exit {rc}, events {case.get('forbidden_created_events')}")
        rc, rows = run("false", "T-create")
        record("event witness: a command that exits at once is invalid, not 'no creates'",
               rc == 2 and bool(rows) and rows[0].get("events_witness") == "invalid" and not any(r.get("id") for r in rows),
               f"exit {rc}, {rows[0] if rows else None}")
        rc, rows = run("sleep 30", "T-create")  # alive but silent: never reports the probe create
        record("event witness: a silent stream that never reports the probe is invalid",
               rc == 2 and bool(rows) and rows[0].get("events_witness") == "invalid", f"exit {rc}, {rows[0] if rows else None}")
        rc, rows = run(f"{sys.executable} {scratch / 'dying.py'}", "T-slow")
        slow_row = next((r for r in rows if r.get("id") == "T-slow"), {})
        record("event witness: a stream that dies mid-run marks the case and the run invalid",
               rc == 2 and slow_row.get("events_unhealthy") is True and slow_row.get("expect_ok") is False,
               f"exit {rc}, events_unhealthy={slow_row.get('events_unhealthy')}, expect_ok={slow_row.get('expect_ok')}")
        engine.close()

        # delayed events: the forbidden create's event arrives 2 s after the (refused-looking) response
        delayed_sock, delayed_events = str(scratch / "d.sock"), scratch / "d-events"
        delayed_events.write_text("")
        delayed_engine = fake_engine(delayed_sock, str(delayed_events), delay=2.0)

        def run_delayed(settle: str) -> dict:
            out = subprocess.run([sys.executable, str(HERE / "rawdocker.py"), "--docker-host", f"unix://{delayed_sock}", "--calibrate",
                                  "--no-cleanup", "--observer-cmd", "false", "--events-cmd", f"tail -n 0 -f {delayed_events}",
                                  "--event-settle", settle, "--case", "T-create", str(scratch / "cases.json")], capture_output=True, text=True, timeout=120)
            return next(json.loads(l) for l in out.stdout.splitlines() if '"T-create"' in l)
        row = run_delayed("3")
        record("event witness: an event delayed 2 s is caught inside the 3 s settle window",
               row.get("forbidden_created_events") == ["nfxraw-1"] and row.get("expect_ok") is False,
               f"events {row.get('forbidden_created_events')}, expect_ok {row.get('expect_ok')}")
        row = run_delayed("0.2")
        print(f"INFO        documented limit: with --event-settle 0.2 a 2 s delayed event is missed (events {row.get('forbidden_created_events')}); "
              "the default window is 3 s", flush=True)
        delayed_engine.close()

        # a stream that proves itself once and then goes silent cannot deliver the post-case barrier
        (scratch / "oneshot.py").write_text(
            "import signal, subprocess, sys, time\n"
            f"p = subprocess.Popen(['tail', '-n', '0', '-f', {str(events)!r}], stdout=subprocess.PIPE, text=True)\n"
            "signal.signal(signal.SIGTERM, lambda *a: (p.kill(), sys.exit(0)))\n"
            "print(p.stdout.readline(), end='', flush=True)\n"
            "time.sleep(60)\n")
        engine2 = fake_engine(sock, str(events))
        rc, rows = run(f"{sys.executable} {scratch / 'oneshot.py'} ", "T-create")
        case = next((r for r in rows if r.get("id") == "T-create"), {})
        record("event witness: a stream that cannot deliver the post-case barrier makes the case and run invalid",
               rc == 2 and case.get("events_unhealthy") is True and case.get("expect_ok") is False,
               f"exit {rc}, events_unhealthy={case.get('events_unhealthy')}, why={str(case.get('events_why'))[:60]}")
        engine2.close()


# --- 4. egress leak ---------------------------------------------------------------------------

def check_egress() -> None:
    def echo(**headers):
        return json.dumps({"headers": headers})
    kit = echo(**{"X-Nfx-Synthetic": "nfx-review"})
    clean = echo(**{"X-Nfx-Synthetic": "nfx-ph-kit"})
    cases = [
        ("clean services echo (positive control)", kit, clean, "passed"),
        ("value leaked into Authorization next to the placeholder", kit,
         echo(**{"X-Nfx-Synthetic": "nfx-ph-kit", "Authorization": "Bearer nfx-review"}), "failed"),
        ("value leaked as the trigger header itself", kit, echo(**{"X-Nfx-Synthetic": "nfx-review"}), "failed"),
        ("value anywhere in a non-JSON body", kit, "oops nfx-review oops", "failed"),
        ("positive control missing", clean, clean, "invalid"),
        ("services trigger never arrived", kit, echo(**{"Accept": "*/*"}), "invalid"),
    ]
    for label, kit_echo, services_echo, want in cases:
        got = synthetic_injection.verdict("review", kit_echo, services_echo)
        record(f"egress: {label}", got["result"] == want, f"verdict {got['result']} ({got.get('why', 'ok')}; {got.get('where', '')})")


# --- 6. the VM lock helper never writes the lock file ---------------------------------------------------

def lock_env(lock_path) -> dict:
    return {**os.environ, "VMLOCK_TEST_PATH": str(lock_path)}  # the shared helper's own test override: a synthetic lock only


def spawn_holder(lock_path, seconds: float, name: str):
    """A process holding the synthetic lock (through the shim and the shared helper) for SECONDS."""
    code = (f"import sys, time; sys.path.insert(0, {str(HERE)!r}); import vmlock\n"
            f"with vmlock.held({name!r}):\n    print('HELD', flush=True); time.sleep({seconds})\n")
    proc = subprocess.Popen([sys.executable, "-c", code], env=lock_env(lock_path), stdout=subprocess.PIPE, text=True)
    assert proc.stdout.readline().strip() == "HELD"
    return proc


def try_lock(lock_path) -> str:
    out = subprocess.run([sys.executable, "-c", "import fcntl,os,sys; fd=os.open(sys.argv[1], os.O_RDONLY)\n"
                          "try: fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB); print('GOT')\n"
                          "except BlockingIOError: print('BLOCKED')", str(lock_path)], capture_output=True, text=True)
    return out.stdout.strip()


def check_lock() -> None:
    with tempfile.TemporaryDirectory() as scratch:
        scratch = pathlib.Path(scratch)
        lock_path = scratch / "lock"
        lock_path.write_bytes(b"")
        before = os.stat(lock_path)
        holder = spawn_holder(lock_path, 1.5, "selfcheck")
        info = scratch / "lock.info"
        after = os.stat(lock_path)
        record("lock (shared helper): holding leaves the lock file's inode and contents untouched; holder metadata goes to lock.info",
               (before.st_ino, before.st_size) == (after.st_ino, after.st_size) and lock_path.read_bytes() == b""
               and json.loads(info.read_text())["run"] == "selfcheck", f"inode {after.st_ino}, size {after.st_size}, info {info.read_text().strip()[:70]}")
        record("lock (shared helper): while held, a second process is excluded", try_lock(lock_path) == "BLOCKED", "BLOCKED")
        holder.wait()
        record("lock (shared helper): released when the holder exits", try_lock(lock_path) == "GOT", "GOT")

        guard = vmlock.LockGuard(lock_path)
        record("lock guard: an untouched lock file verifies (positive control)", guard.verify() == [], "no problems")
        lock_path.write_bytes(b"x")
        record("lock guard: a write to the lock file is detected", any("contents changed" in p for p in guard.verify()), "; ".join(guard.verify()))
        lock_path.write_bytes(b"")
        guard = vmlock.LockGuard(lock_path)
        replacement = scratch / "lock.new"
        replacement.write_bytes(b"")
        os.replace(replacement, lock_path)
        record("lock guard: a replaced lock file (new inode, same contents) is detected", any("replaced" in p for p in guard.verify()), "; ".join(guard.verify()))
        guard = vmlock.LockGuard(lock_path)
        lock_path.unlink()
        record("lock guard: a deleted lock file is detected", any("gone" in p for p in guard.verify()), "; ".join(guard.verify()))
        missing = subprocess.run([sys.executable, "-c", f"import sys; sys.path.insert(0, {str(HERE)!r}); import vmlock\n"
                                  "try:\n    with vmlock.held('x'): print('ENTERED')\n"
                                  "except vmlock.lock_error() as e: print('LockError')"], env=lock_env(lock_path), capture_output=True, text=True)
        record("lock (shared helper): a missing lock file fails closed and is not recreated",
               missing.stdout.strip() == "LockError" and not lock_path.exists(), f"{missing.stdout.strip()}; path still absent")

        # round-5 item 1: a waiter that locked the OLD inode while the path was replaced must not enter
        lock_path.write_bytes(b"")
        old_holder = spawn_holder(lock_path, 60, "A")
        waiter_log = scratch / "waiter.log"
        waiter = subprocess.Popen([sys.executable, "-c", f"import sys, time; sys.path.insert(0, {str(HERE)!r}); import vmlock\n"
                                   f"with vmlock.held('W'):\n    open({str(waiter_log)!r}, 'w').write('ENTERED %f' % time.time())\n"],
                                  env=lock_env(lock_path))
        time.sleep(1.0)  # the waiter is blocked on A's inode
        new_file = scratch / "lock.replacement"
        new_file.write_bytes(b"")
        os.replace(new_file, lock_path)  # the path now names a NEW inode
        new_holder = spawn_holder(lock_path, 60, "B")
        old_holder.terminate()
        old_holder.wait()
        time.sleep(2.5)  # the waiter now owns the old inode's lock: it must notice, drop it and wait on the new file
        entered_early = waiter_log.exists()
        new_holder.terminate()
        new_holder.wait()
        waiter.wait(timeout=30)
        record("lock (shared helper): a waiter on a replaced path does not enter while the new inode is held, and enters once it is free",
               not entered_early and waiter_log.exists(), f"entered while the replacement was held: {entered_early}; entered after release: {waiter_log.exists()}")

        # round-5 item 2: interrupting the wrapper must not release the lock while the child still works
        lock_path.unlink()
        lock_path.write_bytes(b"")
        child = scratch / "child.py"
        child.write_text("import signal, sys, time\nsignal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
                         f"print('CHILD-UP', flush=True)\ntime.sleep(4)\nopen({str(scratch / 'child.done')!r}, 'w').write('done')\nsys.exit(7)\n")
        wrapper = subprocess.Popen([sys.executable, str(HERE / "vmlock.py"), "--run", "selfcheck", "--", sys.executable, str(child)],
                                   env=lock_env(lock_path), stdout=subprocess.PIPE, text=True)
        assert wrapper.stdout.readline().strip() == "CHILD-UP"
        wrapper.send_signal(15)  # SIGTERM the wrapper (the child ignores it and keeps working for ~4 s)
        time.sleep(1.5)
        while_child_runs = try_lock(lock_path)
        status = wrapper.wait(timeout=30)
        after_child = try_lock(lock_path)
        record("lock (shared helper): terminating the wrapper does not release the lock while the child still works; released after the child is reaped",
               while_child_runs == "BLOCKED" and (scratch / "child.done").exists() and status == 7 and after_child == "GOT",
               f"during: {while_child_runs}; child finished: {(scratch / 'child.done').exists()}; wrapper exit {status}; after: {after_child}")


FAKE_MARSH = r'''#!/bin/sh
# a stand-in marsh for the end-to-end ordering check: records an ownership map, tampers with the
# SYNTHETIC lock file when asked, and answers just enough of `run` and `stop`
TAMPER="@TAMPER@"
for d in "$MARSH_CONTROL_HOME"/*/; do
  printf '{"version":2,"vms":{"marsh-xnfx01-k-fake":{"purpose":"kit","key":"k"}}}' > "${d}vm-ownership.json"
done
if [ "$1" = run ] && [ -n "$TAMPER" ]; then echo tampered >> "$TAMPER"; fi
case "$1" in
  stop) exit 0 ;;
  run) if [ "$2" = "--acp" ]; then exec python3 @STUB@; fi
       cat @VERSIONS@ ; exit 0 ;;
esac
exit 0
'''
FAKE_SBX = '''#!/bin/sh
case "$1" in
  ls) echo '[]' ;;
  create) echo "no sandbox in this check" >&2; exit 1 ;;
esac
exit 0
'''


def check_lock_ordering() -> None:
    """End to end (no VM): HARNESS-03's own runner with a stand-in marsh. The lock check must come BEFORE
    the evidence is deleted: a lock that changed during the run keeps the scratch and makes a break invalid."""
    for label, tamper, mode in (("normal run, lock tampered", True, None), ("break run, lock tampered", True, "NFX_FAKE_EXEC=off"),
                                ("normal run, lock untouched (positive control)", False, None)):
        with tempfile.TemporaryDirectory() as scratch:
            scratch = pathlib.Path(scratch)
            lock_path = scratch / "lock"
            lock_path.write_bytes(b"")
            (scratch / "versions.txt").write_text("\n".join(i["exact"] for i in harness03.VERSIONS["lines"]) + "\n")
            stub = scratch / "acp.py"
            stub.write_text(FAKE_ACP.replace("sys.argv[1]", "'ok'"))
            bin_dir = scratch / "bin"
            bin_dir.mkdir()
            (bin_dir / "marsh").write_text(FAKE_MARSH.replace("@TAMPER@", str(lock_path) if tamper else "").replace("@STUB@", str(stub))
                                           .replace("@VERSIONS@", str(scratch / "versions.txt")))
            (bin_dir / "sbx").write_text(FAKE_SBX)
            for name in ("marsh", "sbx"):
                (bin_dir / name).chmod(0o755)
            command = [sys.executable, str(HERE / "harness03.py"), "--marsh", str(bin_dir / "marsh"), "--guest-artifacts", str(scratch),
                       "--sbx", str(bin_dir / "sbx"), "--lock", str(lock_path)] + (["--break", mode] if mode else [])
            out = subprocess.run(command, capture_output=True, text=True, timeout=300)
            try:
                result = json.loads(out.stdout)
            except ValueError:
                record(f"lock ordering: {label}", False, f"no result: {out.stderr[-200:]}")
                continue
            kept = pathlib.Path(result["scratch"]).is_dir()
            try:
                if tamper:
                    wanted = "invalid_negative_control" if mode else "fail"
                    record(f"lock ordering: {label}: evidence kept, lock flagged, verdict {wanted}",
                           kept and result["lock_check"]["unchanged"] is False and out.returncode != 0 and result["verdict"] == wanted
                           and result["ok"] is False, f"scratch kept={kept}; lock_check {result['lock_check']['problems']}; verdict {result['verdict']}; exit {out.returncode}")
                else:
                    record(f"lock ordering: {label}: evidence removed only when cleanup and lock are both verified",
                           not kept and result["lock_check"]["unchanged"] is True and not result["cleanup_errors"],
                           f"scratch kept={kept}; cleanup_errors {result['cleanup_errors']}; lock_check unchanged")
            finally:
                path = pathlib.Path(result["scratch"])
                if path.parent == pathlib.Path("/private/tmp") and path.name.startswith("nfx-h3") and path.is_dir():
                    shutil.rmtree(path)  # the exact scratch directory this check's own run created


# --- 5. HARNESS-03 error propagation and cleanup verification ----------------------------------------

FAKE_ACP = r'''
import base64, json, sys, time
mode = sys.argv[1]
def out(obj):
    print(json.dumps({"jsonrpc": "2.0", **obj}), flush=True)
for line in sys.stdin:
    m = json.loads(line)
    if m.get("method") == "initialize":
        out({"id": m["id"], "result": {"protocolVersion": 1}})
    elif m.get("method") == "session/new":
        out({"id": m["id"], "result": {"sessionId": "s"}})
    elif m.get("method") == "session/prompt":
        text = m["params"]["prompt"][0]["text"]
        def upd(t):
            out({"method": "session/update", "params": {"sessionId": "s", "update": {
                "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": t}}}})
        upd("fixture:" + text)
        if mode == "error":
            out({"id": m["id"], "error": {"code": -32001, "message": "boom"}})
        elif mode == "refusal":
            out({"id": m["id"], "result": {"stopReason": "refusal"}})
        elif mode == "stall":
            time.sleep(60)
        else:
            upd("fixture:exit:0"); upd("fixture:out:" + base64.b64encode(b"nfx-ok\n").decode())
            out({"id": m["id"], "result": {"stopReason": "end_turn"}})
'''


def check_harness03() -> None:
    with tempfile.TemporaryDirectory() as scratch:
        fake = pathlib.Path(scratch, "acp.py")
        fake.write_text(FAKE_ACP)

        def acp(mode: str, timeout: float = 20):
            return harness03.acp_exec([sys.executable, str(fake), mode], scratch, dict(os.environ), "echo nfx-ok", timeout)
        record("HARNESS-03: a good ACP prompt returns the updates (positive control)", "fixture:exit:0" in acp("ok"), "fixture:exit:0 seen")
        for mode, kind in (("error", RuntimeError), ("refusal", RuntimeError), ("stall", TimeoutError)):
            try:
                acp(mode, 3)
                record(f"HARNESS-03: ACP prompt with {mode}", False, "returned updates as if it had succeeded")
            except kind as error:
                record(f"HARNESS-03: ACP prompt with {mode}", True, f"raised {type(error).__name__}: {str(error)[:70]}")

        class Ctx:  # an agent JSON-RPC error in break mode: invalid, never a verified break
            break_mode = "NFX_FAKE_EXEC=off"
            workdir = pathlib.Path(scratch, "w")
            project = scratch

            def marsh_run(self, argv, timeout):
                return types.SimpleNamespace(returncode=0, stdout="\n".join(i["exact"] for i in harness03.VERSIONS["lines"]), stderr="")

            def agent_exec(self, agent, script, timeout):
                return acp("error")

            def sandbox(self, *args):
                raise RuntimeError("no sandbox offline")
        outcome = harness03.run(Ctx())
        record("HARNESS-03: break mode + JSON-RPC error is invalid_negative_control, not verified_break",
               outcome["verdict"] == "invalid_negative_control" and not outcome["checks"]["c2"].get("disabled_verb"),
               f"verdict {outcome['verdict']}; c2 {str(outcome['checks']['c2'].get('observed'))[:70]}")

        ownership = pathlib.Path(scratch, "vm-ownership.json")
        messages = {}
        for label, setup in (("missing", lambda: None), ("corrupt", lambda: ownership.write_text("{not json")),
                             ("without a vms key", lambda: ownership.write_text("{}"))):
            ownership.unlink(missing_ok=True)
            setup()
            try:
                harness03.read_ownership(ownership)
                record(f"HARNESS-03: ownership record {label}", False, "read as valid")
            except harness03.OwnershipError as error:
                messages[label] = str(error)
                record(f"HARNESS-03: ownership record {label}", True, "OwnershipError")
        schema_bad = {
            "vms is a string": {"version": 2, "vms": "corrupt"},
            "vms is a nonempty list": {"version": 2, "vms": ["marsh-xnfx01-k-owned"]},
            "vms is null": {"version": 2, "vms": None},
            "an entry is null": {"version": 2, "vms": {"marsh-xnfx01-k-owned": None}},
            "an entry is a string": {"version": 2, "vms": {"marsh-xnfx01-k-owned": "uuid"}},
            "an entry lacks purpose and key": {"version": 2, "vms": {"marsh-xnfx01-k-owned": {"uuid": "u"}}},
            "an entry has an unknown field": {"version": 2, "vms": {"marsh-xnfx01-k-owned": {"purpose": "kit", "key": "k", "extra": 1}}},
            "an entry has a non-string uuid": {"version": 2, "vms": {"marsh-xnfx01-k-owned": {"purpose": "kit", "key": "k", "uuid": 7}}},
            "an empty VM name": {"version": 2, "vms": {"": {"purpose": "kit", "key": "k"}}},
            "version 3": {"version": 3, "vms": {}},
            "no version": {"vms": {}},
            "top level is a list": ["marsh-xnfx01-k-owned"],
            "unknown top-level field": {"version": 2, "vms": {}, "extra": {}},
            "a legacy entry that is not a uuid string": {"version": 1, "vms": {"marsh-xnfx01-k-owned": {"x": 1}}},
            "grants is a list": {"version": 2, "vms": {}, "grants": ["x"]},
            "a grant with only a prefix": {"version": 2, "vms": {}, "grants": {"s": {"prefix": "p"}}},
        }
        grant = {"session": "s1", "prefix": "marsh-xnfx01-", "roots": ["/private/tmp/nfx-x/p"], "scratch": "/private/tmp/nfx-x/s",
                 "max_vms": 4, "revoked": False, "cleanup_uncertain": False, "names": {"marsh-xnfx01-k-d": None, "marsh-xnfx01-k-e": "u"}}

        def with_grant(**changes):
            document = {**grant, **changes}
            for key in [k for k, v in changes.items() if v is DROP]:
                del document[key]
            return {"version": 2, "vms": {}, "grants": {"s1": document}}
        schema_bad.update({
            "a grant without session": with_grant(session=DROP),
            "a grant without names": with_grant(names=DROP),
            "a grant without roots, scratch, max_vms or revoked": {"version": 2, "vms": {}, "grants": {"s1": {"session": "s", "prefix": "p", "names": {"x": None}}}},
            "a grant with an unknown field": with_grant(extra=1),
            "a grant whose roots is a string": with_grant(roots="/private/tmp"),
            "a grant whose max_vms is a float": with_grant(max_vms=4.0),
            "a grant whose max_vms is negative": with_grant(max_vms=-1),
            "a grant whose revoked is a string": with_grant(revoked="no"),
            "a grant whose cleanup_uncertain is 0": with_grant(cleanup_uncertain=0),
            "a grant whose names value is a number": with_grant(names={"marsh-xnfx01-k-d": 7}),
            "a grant that is a list": {"version": 2, "vms": {}, "grants": {"s1": []}},
            "version 2.0": {"version": 2.0, "vms": {}},
            "version true": {"version": True, "vms": {}},
            "version \"2\"": {"version": "2", "vms": {}},
            "version 1 with a grants field": {"version": 1, "vms": {}, "grants": {}},
        })
        raw_bad = {
            "duplicate top-level `vms` (the round-4 reviewer's case)":
                '{"version":2,"vms":{"marsh-xnfx01-k-aaaaaaaa":{"purpose":"kit","key":"k","uuid":"u"}},"vms":{"marsh-xnfx01-k-bbbbbbbb":{"purpose":"kit","key":"k"}}}',
            "duplicate `version`": '{"version":1,"version":2,"vms":{}}',
            "duplicate nested entry field": '{"version":2,"vms":{"marsh-xnfx01-k-a":{"purpose":"kit","purpose":"shell","key":"k"}}}',
            "duplicate VM name": '{"version":2,"vms":{"marsh-xnfx01-k-a":{"purpose":"kit","key":"k"},"marsh-xnfx01-k-a":{"purpose":"kit","key":"k2"}}}',
            "duplicate grant field": '{"version":2,"vms":{},"grants":{"s":{"session":"s","session":"t","prefix":"p","roots":[],"scratch":"x","max_vms":1,"revoked":false,"names":{}}}}',
            "version 2.0 written as text": '{"version":2.0,"vms":{}}',
            "NaN anywhere": '{"version":2,"vms":{},"grants":{"s":{"max_vms":NaN}}}',
        }
        for label, text in raw_bad.items():
            ownership.write_text(text)
            try:
                names = harness03.read_ownership(ownership)
                record(f"HARNESS-03 ownership schema: {label}", False, f"read as valid: {sorted(names)[:3]}")
            except harness03.OwnershipError as error:
                record(f"HARNESS-03 ownership schema: {label}", True, str(error).split(" (")[-1][:80] if "unreadable" in str(error) else str(error).split("malformed: ")[-1][:80])
        # the round-4 reviewer's chain: a duplicate-vms record must keep a surviving VM from being reported as foreign
        ownership.write_text(raw_bad["duplicate top-level `vms` (the round-4 reviewer's case)"])
        try:
            harness03.read_ownership(ownership)
            record("HARNESS-03 chain: duplicate `vms` + surviving VM", False, "read as valid")
        except harness03.OwnershipError as error:
            errors = harness03.verify_cleanup(run_id="r1", owned=set(), ownership_error=str(error), scope_ran=True, stop_returncode=1,
                                              removal_errors=[], remaining=["marsh-xnfx01-k-aaaaaaaa"])
            scratch_dir = pathlib.Path(tempfile.mkdtemp(prefix="nfx-h3sc-", dir="/private/tmp"))
            try:
                verified, deleted = finish_live({"verdict": "invalid_negative_control", "checks": {"c2": {"ok": False, "observed": "x"}},
                                                 "scratch": str(scratch_dir), "owned_vms": [], "inventory": {"ok": True, "owned_remaining": []},
                                                 "cleanup_errors": ["marsh stop exited 1"] + errors, "lock_check": {"unchanged": True}}, 1, emit=False)
                record("HARNESS-03 chain: duplicate `vms` + surviving VM is a cleanup error and the evidence is kept",
                       any("malformed" in e or "duplicate" in e for e in errors) and not verified and not deleted and scratch_dir.is_dir(),
                       f"errors {len(errors)}, verified={verified}, scratch kept={scratch_dir.is_dir()}")
            finally:
                shutil.rmtree(scratch_dir, ignore_errors=True)
        for label, document in schema_bad.items():
            ownership.write_text(json.dumps(document))
            try:
                names = harness03.read_ownership(ownership)
                record(f"HARNESS-03 ownership schema: {label}", False, f"read as valid: {sorted(names)[:3]}")
            except harness03.OwnershipError as error:
                record(f"HARNESS-03 ownership schema: {label}", "malformed" in str(error), str(error).split("malformed: ")[-1][:70])
        legit = {
            "empty map": ({"version": 2, "vms": {}}, set()),
            "a recorded VM": ({"version": 2, "vms": {"marsh-xnfx01-k-a": {"purpose": "kit", "key": "k", "uuid": "u"}}}, {"marsh-xnfx01-k-a"}),
            "a pending intent (no uuid)": ({"version": 2, "vms": {"marsh-xnfx01-k-b": {"purpose": "kit", "key": "k"}}}, {"marsh-xnfx01-k-b"}),
            "a supported legacy record": ({"version": 1, "vms": {"marsh-xnfx01-k-c": "uuid"}}, {"marsh-xnfx01-k-c"}),
            "a valid development grant (the product's DevGrantRecord)": ({"version": 2, "vms": {}, "grants": {"s1": grant}},
                                                                         {"marsh-xnfx01-k-d", "marsh-xnfx01-k-e"}),
            "a grant without cleanup_uncertain (serde default)": ({"version": 2, "vms": {}, "grants": {"s1": {k: v for k, v in grant.items() if k != "cleanup_uncertain"}}},
                                                                  {"marsh-xnfx01-k-d", "marsh-xnfx01-k-e"}),
        }
        for label, (document, expected) in legit.items():
            ownership.write_text(json.dumps(document))
            try:
                got = harness03.read_ownership(ownership)
                record(f"HARNESS-03 ownership schema: {label} is accepted (positive control)", got == expected, f"names {sorted(got)}")
            except harness03.OwnershipError as error:
                record(f"HARNESS-03 ownership schema: {label} is accepted (positive control)", False, str(error))
        ownership.write_text(json.dumps({"version": 2, "vms": {"marsh-xnfx01-k-owned": {"purpose": "kit", "key": "k", "uuid": "u"}}}))
        owned_ok = harness03.read_ownership(ownership)
        record("HARNESS-03 cleanup: a valid map naming a surviving VM reports it",
               any("leftover" in e for e in harness03.verify_cleanup(run_id="r1", owned=owned_ok, ownership_error=None, scope_ran=True,
                                                                     stop_returncode=0, removal_errors=[], remaining=["marsh-xnfx01-k-owned"])), "leftover")
        errors = harness03.verify_cleanup(run_id="r1", owned="corrupt", ownership_error=None, scope_ran=True, stop_returncode=0,
                                          removal_errors=[], remaining=["marsh-xnfx01-k-owned"])
        record("HARNESS-03 cleanup: a non-set `owned` (the round-3 string case) is an error, not a pass",
               any("malformed" in e for e in errors), "; ".join(errors)[:90])
        base = dict(run_id="r1", owned={"marsh-xnfx01-k-aaaa"}, ownership_error=None, scope_ran=True, stop_returncode=0,
                    removal_errors=[], remaining=["foreign-sandbox"])
        record("HARNESS-03 cleanup: a clean run verifies clean (positive control)", harness03.verify_cleanup(**base) == [], "no errors")
        for label, change, needle in (
                ("ownership record unreadable", dict(owned=None, ownership_error=messages["corrupt"]), "unreadable"),
                ("ownership record empty although jobs ran", dict(owned=set()), "no VMs"),
                ("marsh stop failed", dict(stop_returncode=1), "marsh stop exited 1"),
                ("an owned VM is still there", dict(remaining=["marsh-xnfx01-k-aaaa"]), "leftover"),
                ("this run's sandbox is still there", dict(remaining=["nfx-r1-fx"]), "leftover"),
                ("sandbox removal failed", dict(removal_errors=["sbx rm nfx-r1-fx failed"]), "sbx rm"),
                ("sbx ls failed", dict(remaining=None), "sbx ls")):
            errors = harness03.verify_cleanup(**{**base, **change})
            record(f"HARNESS-03 cleanup: {label}", any(needle in e for e in errors), "; ".join(errors)[:100])


EXPECTED_INJECTED_ERRORS = ["marsh stop exited 1"]


def finish_live(result: dict, returncode: int, emit: bool = True) -> tuple[bool, bool]:
    """Judge the injected-failure run and dispose of its scratch state. Cleanup counts as VERIFIED only
    when the sole cleanup error is the injected `marsh stop` failure, the VM inventory was read, the
    ownership record named VMs, and none of them is left. The scratch directory (the ownership evidence)
    is deleted ONLY then, and only the exact path the run reported it created (under /private/tmp, named
    nfx-h3*); otherwise it is kept and the self-check fails. Returns (verified, deleted)."""
    scratch = pathlib.Path(result.get("scratch", ""))
    errors = result.get("cleanup_errors", [])
    inventory = result.get("inventory", {})
    unexpected = [e for e in errors if e not in EXPECTED_INJECTED_ERRORS]
    verified = (not unexpected and EXPECTED_INJECTED_ERRORS[0] in errors and inventory.get("ok") is True
                and inventory.get("owned_remaining") == [] and bool(result.get("owned_vms"))
                and result.get("lock_check", {}).get("unchanged") is True)
    deleted = False
    if verified and scratch.parent == pathlib.Path("/private/tmp") and scratch.name.startswith("nfx-h3") and scratch.is_dir():
        shutil.rmtree(scratch)
        deleted = True
    if emit:
        record("live: an injected agent JSON-RPC error is not a verified break",
               result.get("verdict") == "invalid_negative_control" and result.get("checks", {}).get("c2", {}).get("ok") is False,
               f"verdict {result.get('verdict')}; c2 observed {json.dumps(result.get('checks', {}).get('c2', {}).get('observed'))[:90]}")
        record("live: a failing `marsh stop` is a cleanup error and the runner exits nonzero",
               returncode != 0 and EXPECTED_INJECTED_ERRORS[0] in errors, f"exit {returncode}; cleanup_errors {errors}")
        record("live: cleanup verification knew the owned VMs, read the inventory and found them gone",
               verified, f"owned {result.get('owned_vms')}; inventory {inventory}; unexpected errors {unexpected}")
        record("live: the scratch evidence was retained until verified, then removed (exact reported path only)"
               if verified else "live: UNVERIFIED cleanup: the scratch evidence is retained",
               deleted if verified else scratch.is_dir(), f"scratch {scratch} {'removed' if deleted else 'kept'}")
    return verified, deleted


def check_live_judgement() -> None:
    """The decision of `finish_live`, fed mocked results (no VM): unverified cleanup must keep its evidence."""
    base_checks = {"c2": {"ok": False, "observed": "RuntimeError: injected"}}

    def result(scratch, **overrides):
        document = {"verdict": "invalid_negative_control", "checks": base_checks, "scratch": str(scratch),
                    "owned_vms": ["marsh-xnfx01-k-aaaa"], "inventory": {"ok": True, "owned_remaining": []},
                    "cleanup_errors": ["marsh stop exited 1"], "lock_check": {"unchanged": True}}
        document.update(overrides)
        return document

    scenarios = (
        ("only the injected stop error, inventory read, nothing left", {}, True),
        ("injected stop error AND a failed inventory", {"cleanup_errors": ["marsh stop exited 1", "cannot verify cleanup: sbx ls failed"],
                                                         "inventory": {"ok": False, "owned_remaining": []}}, False),
        ("injected stop error and an owned VM still present", {"cleanup_errors": ["marsh stop exited 1", "leftover sandboxes: ['marsh-xnfx01-k-aaaa']"],
                                                                "inventory": {"ok": True, "owned_remaining": ["marsh-xnfx01-k-aaaa"]}}, False),
        ("an unexpected extra error", {"cleanup_errors": ["marsh stop exited 1", "sbx rm nfx-x-fx failed"]}, False),
        ("inventory flag missing", {"inventory": {}}, False),
        ("no owned VMs recorded", {"owned_vms": []}, False),
        ("the VM lock file changed during the run", {"lock_check": {"unchanged": False, "problems": ["the lock file's contents changed"]}}, False),
        ("no lock check reported", {"lock_check": {}}, False),
        ("the expected stop error absent (the injection did not happen)", {"cleanup_errors": []}, False),
    )
    for label, overrides, want_verified in scenarios:
        scratch = pathlib.Path(tempfile.mkdtemp(prefix="nfx-h3sc-", dir="/private/tmp"))
        (scratch / "vm-ownership.json").write_text("{}")
        try:
            verified, deleted = finish_live(result(scratch, **overrides), 1, emit=False)
            kept = scratch.is_dir()
            ok = verified == want_verified and (deleted == want_verified) and (kept != want_verified)
            record(f"live judgement: {label}", ok,
                   f"verified={verified}, scratch {'removed' if deleted else 'KEPT'}" + ("" if want_verified else " (evidence retained)"))
        finally:
            shutil.rmtree(scratch, ignore_errors=True)
    outside = pathlib.Path(tempfile.mkdtemp(prefix="not-nfx-"))
    try:
        verified, deleted = finish_live(result(outside), 1, emit=False)
        record("live judgement: a reported scratch path outside /private/tmp/nfx-h3* is never deleted",
               verified and not deleted and outside.is_dir(), f"verified={verified}, deleted={deleted}")
    finally:
        shutil.rmtree(outside, ignore_errors=True)


def live(marsh: str, sbx: str = "sbx") -> None:
    """HARNESS-03 on a real install with injected failures: they must surface as errors and
    cleanup must still be verified (and honest) although the run is broken."""
    wrapper_dir = pathlib.Path(tempfile.mkdtemp(prefix="nfx-selfcheck-"))
    real = pathlib.Path(marsh).resolve()
    wrapper = wrapper_dir / "bin" / "marsh"
    wrapper.parent.mkdir()
    (wrapper_dir / "libexec").symlink_to(real.parents[1] / "libexec")
    for sibling in real.parent.iterdir():
        if sibling.name != "marsh":
            (wrapper.parent / sibling.name).symlink_to(sibling)
    stub = wrapper_dir / "acp_error.py"
    stub.write_text(
        "import json, sys\n"
        "for line in sys.stdin:\n"
        "    m = json.loads(line)\n"
        "    def out(o): print(json.dumps({'jsonrpc': '2.0', **o}), flush=True)\n"
        "    if m.get('method') == 'initialize': out({'id': m['id'], 'result': {'protocolVersion': 1}})\n"
        "    elif m.get('method') == 'session/new': out({'id': m['id'], 'result': {'sessionId': 's'}})\n"
        "    elif m.get('method') == 'session/prompt':\n"
        "        out({'id': m['id'], 'error': {'code': -32001, 'message': 'injected'}}); break\n")
    wrapper.write_text(f'''#!/bin/sh
# injected failures: a JSON-RPC error from the agent session, and a failing `marsh stop`
case "$1" in
  stop) "{real}" "$@"; exit 1 ;;
  run) if [ "$2" = "--acp" ]; then exec python3 {stub}; fi ;;
esac
exec "{real}" "$@"
''')
    wrapper.chmod(0o755)
    try:
        # The shared helper's `run`: the lock is held until HARNESS-03 (the working child, started with --no-lock)
        # has been reaped, and INT/TERM/HUP sent to this process are forwarded to it. The child snapshots the lock
        # file once it is inside the lock and re-checks it before it decides about its evidence.
        out = vmlock.run([sys.executable, str(HERE / "harness03.py"), "--marsh", str(wrapper), "--guest-artifacts",
                          str(real.parents[1] / "libexec/marsh"), "--sbx", sbx, "--break", "NFX_FAKE_EXEC=off", "--no-lock"],
                         run="h1b-selfcheck-live", stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            result = json.loads(out.stdout)
        except ValueError:
            record("live: injected failures", False, f"no result: {(out.stderr or '')[-300:]}")
            return
        finish_live(result, out.returncode)
        record("live: the VM lock file's inode and contents are unchanged across the run (checked by the runner inside the lock)",
               result.get("lock_check", {}).get("unchanged") is True, f"runner {result.get('lock_check')}")
    finally:
        shutil.rmtree(wrapper_dir, ignore_errors=True)  # this self-check's own wrapper directory


FAKE_MARSH_BLOCKING = r'''#!/bin/sh
# a stand-in marsh for the interruption controls: `run` announces itself and then blocks IGNORING SIGTERM;
# `stop` (the cleanup) announces itself and takes a few seconds; the ownership map is written on every call
DIR="@DIR@"
for d in "$MARSH_CONTROL_HOME"/*/; do
  printf '{"version":2,"vms":{"marsh-xnfx01-k-fake":{"purpose":"kit","key":"k"}}}' > "${d}vm-ownership.json"
done
case "$1" in
  run) touch "$DIR/RUNNING"; trap '' TERM INT HUP; exec sleep 60 ;;
  stop) touch "$DIR/STOPPING"; sleep 4; exit 0 ;;
esac
exit 0
'''


def wait_for(path: pathlib.Path, seconds: float = 90) -> bool:
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        if path.exists():
            return True
        time.sleep(0.1)
    return False


def pid_alive(pid: int) -> bool:
    """Alive means running: a zombie awaiting its reaper does not count."""
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    state = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
    return bool(state) and not state.startswith("Z")


FAKE_SBX_NESTED = r'''#!/bin/sh
# a stand-in sbx: `create` works, `rm` FAILS, and the image load is a long-running nested `sbx exec` that ignores SIGTERM
DIR="@DIR@"
case "$1" in
  ls) echo '[]' ;;
  create) exit 0 ;;
  rm) echo "injected removal failure" >&2; exit 1 ;;
  exec)
    case "$*" in
      *" load "*) echo $$ > "$DIR/NESTED.pid"; trap '' TERM INT HUP; exec sleep 60 ;;
      *"image inspect"*) exit 1 ;;
    esac ;;
esac
exit 0
'''


def check_nested_interruption() -> None:
    """Interruption + a failing sandbox removal + a long-running NESTED child (HARNESS-03 -> image_cache.py ->
    `sbx exec ... docker load`, which ignores SIGTERM): the nested child must be gone before a competitor can take
    the (synthetic) lock, and the evidence must be kept.

    HARNESS-03 loads the saved images before that `sbx exec`, and stops at the first one that is missing, so
    without `image_cache.py save` (no Docker, no network here) the nested child never exists to be checked."""
    missing = uncached_images()
    if missing:
        skip("nested interruption", f"needs the saved images in {image_cache.DEFAULT_OUT} (run image_cache.py save); "
                                    f"missing {len(missing)}, first {missing[0]}")
        return
    with tempfile.TemporaryDirectory() as scratch:
        scratch = pathlib.Path(scratch)
        lock_path = scratch / "lock"
        lock_path.write_bytes(b"")
        markers = scratch / "markers"
        markers.mkdir()
        (scratch / "versions.txt").write_text("\n".join(i["exact"] for i in harness03.VERSIONS["lines"]) + "\n")
        stub = scratch / "acp.py"
        stub.write_text(FAKE_ACP.replace("sys.argv[1]", "'ok'"))
        bin_dir = scratch / "bin"
        bin_dir.mkdir()
        (bin_dir / "marsh").write_text(FAKE_MARSH.replace("@TAMPER@", "").replace("@STUB@", str(stub)).replace("@VERSIONS@", str(scratch / "versions.txt")))
        (bin_dir / "sbx").write_text(FAKE_SBX_NESTED.replace("@DIR@", str(markers)))
        for name in ("marsh", "sbx"):
            (bin_dir / name).chmod(0o755)
        proc = subprocess.Popen([sys.executable, str(HERE / "harness03.py"), "--marsh", str(bin_dir / "marsh"), "--guest-artifacts", str(scratch),
                                 "--sbx", str(bin_dir / "sbx"), "--lock", str(lock_path)], env=lock_env(lock_path),
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        nested = None
        kept = None
        try:
            if not wait_for(markers / "NESTED.pid"):
                record("nested interruption", False, "the nested sbx exec never started")
                proc.kill()
                return
            nested = int((markers / "NESTED.pid").read_text().strip())
            alive_before = pid_alive(nested)
            blocked_while_alive = 0
            proc.send_signal(15)  # SIGTERM to the exact HARNESS-03 parent
            nested_alive_at_acquire = None
            end = time.monotonic() + 90
            while time.monotonic() < end:
                alive = pid_alive(nested)
                got = try_lock(lock_path)
                if got == "BLOCKED" and alive:
                    blocked_while_alive += 1
                if got == "GOT":
                    nested_alive_at_acquire = pid_alive(nested)
                    break
                time.sleep(0.1)
            out, err = proc.communicate(timeout=120)
            try:
                result = json.loads(out)
            except ValueError:
                result = {}
            kept = pathlib.Path(result.get("scratch", ""))
            errors = result.get("cleanup_errors", [])
            record("nested interruption: with a failing sandbox removal the nested `sbx exec` (ignoring SIGTERM) is gone before a competitor can take the lock",
                   alive_before and blocked_while_alive > 0 and nested_alive_at_acquire is False
                   and any("sbx rm" in e for e in errors) and any("interrupted" in e for e in errors)
                   and not any("reaped" in e for e in errors) and kept.is_dir() and bool(list(kept.glob("control/*/vm-ownership.json"))),
                   f"nested alive before {alive_before}; lock refused {blocked_while_alive}x while it lived; nested alive when the lock was taken: "
                   f"{nested_alive_at_acquire}; cleanup_errors {errors}; ownership evidence kept {kept.is_dir()}")
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()
            if nested is not None and pid_alive(nested):
                os.kill(nested, 9)  # the exact pid this control's own stand-in started
            if kept and kept.parent == pathlib.Path("/private/tmp") and kept.name.startswith("nfx-h3") and kept.is_dir():
                shutil.rmtree(kept)


def check_nested_control() -> None:
    """Non-vacuity: killing only the immediate child (what `subprocess.run` does on interruption) leaves a nested
    process running, which is what the nested-interruption check would report without group tracking."""
    with tempfile.TemporaryDirectory() as scratch:
        pid_file = pathlib.Path(scratch) / "nested.pid"
        leader = subprocess.Popen(["sh", "-c", f"sh -c 'echo $$ > {pid_file}; exec sleep 20' & wait"])
        wait_for(pid_file, 30)
        nested = int(pid_file.read_text().strip())
        leader.kill()
        leader.wait()
        survived = pid_alive(nested)
        record("nested control: killing only the immediate child leaves its nested child running (so the nested check can fail)",
               survived, f"nested {nested} alive after the leader was killed: {survived}")
        if survived:
            os.kill(nested, 9)  # the exact pid this control started


LIFETIME_DRIVER = r'''
import os, signal, sys, time
sys.path.insert(0, sys.argv[1])
import harness03
scenario, rest = sys.argv[2], sys.argv[3:]
if scenario == "unreaped":
    harness03._killpg = lambda pgid, sig: None  # the group-level teardown cannot kill anything
    harness03.GRACE_SECONDS = 0.4
elif scenario in ("spawn-race", "spawn-race-unprotected"):
    class RacingDict(dict):  # a SIGTERM delivered between the child's spawn and its registration
        fired = False
        def __setitem__(self, key, proc):
            if not RacingDict.fired and os.path.basename(str(proc.args[0])) == "marsh":
                RacingDict.fired = True
                os.kill(os.getpid(), signal.SIGTERM)
            super().__setitem__(key, proc)
    harness03.TRACKED = RacingDict()
    if scenario == "spawn-race-unprotected":
        signal.pthread_sigmask = lambda how, mask: set()  # the OLD behaviour: no deferral across spawn and registration
elif scenario == "staging":
    harness03.HERE = __import__("pathlib").Path(os.environ["NFX_FAKE_HERE"])  # a stand-in nfx-kit/stage.py that blocks
sys.argv = ["harness03.py", *rest, "--no-lock"]
sys.exit(harness03.main())
'''

FAKE_MARSH_STRAGGLER = r'''#!/bin/sh
# stand-in marsh for the lock-lifetime controls
DIR="@DIR@"; MODE="@MODE@"
for d in "$MARSH_CONTROL_HOME"/*/; do
  printf '{"version":2,"vms":{"marsh-xnfx01-k-fake":{"purpose":"kit","key":"k"}}}' > "${d}vm-ownership.json"
done
case "$1" in
  stop) exit 0 ;;
  run)
    if [ "$2" = "--acp" ]; then exec python3 @STUB@; fi
    if [ "$MODE" = unreaped ]; then
      sh -c 'trap "" TERM INT HUP; echo $$ > "'"$DIR"'/NESTED.pid"; exec sleep 40' >/dev/null 2>&1 </dev/null &
      exit 0
    fi
    if [ "$MODE" = block ]; then echo $$ > "$DIR/NESTED.pid"; trap '' TERM INT HUP; exec sleep 60; fi ;;
esac
exit 0
'''


def lifetime_case(label: str, scenario: str, signal_parent: bool, extra_checks) -> None:
    """One lock-lifetime control against the shared helper (synthetic lock): HARNESS-03 runs as the child of
    vmlock.run in a parent process; a stand-in leaves a SIGTERM-ignoring descendant; the competitor must be
    refused until that descendant (and everything else of the session) is gone."""
    with tempfile.TemporaryDirectory() as scratch:
        scratch = pathlib.Path(scratch)
        lock_path = scratch / "lock"
        lock_path.write_bytes(b"")
        markers = scratch / "markers"
        markers.mkdir()
        pid_file = markers / "NESTED.pid"
        (scratch / "versions.txt").write_text("\n".join(i["exact"] for i in harness03.VERSIONS["lines"]) + "\n")
        stub = scratch / "acp.py"
        stub.write_text(FAKE_ACP.replace("sys.argv[1]", "'ok'"))
        bin_dir = scratch / "bin"
        bin_dir.mkdir()
        mode = {"unreaped": "unreaped", "spawn-race": "block", "spawn-race-unprotected": "block", "staging": "none"}[scenario]
        (bin_dir / "marsh").write_text(FAKE_MARSH_STRAGGLER.replace("@DIR@", str(markers)).replace("@MODE@", mode).replace("@STUB@", str(stub)))
        (bin_dir / "sbx").write_text("#!/bin/sh\ncase \"$1\" in ls) echo '[]' ;; create) exit 1 ;; esac\nexit 0\n")
        for name in ("marsh", "sbx"):
            (bin_dir / name).chmod(0o755)
        fake_here = scratch / "here"
        (fake_here / "nfx-kit").mkdir(parents=True)
        (fake_here / "nfx-kit" / "stage.py").write_text(
            f"import signal, time\nsignal.signal(signal.SIGTERM, signal.SIG_IGN)\nopen({str(pid_file)!r}, 'w').write(str(__import__('os').getpid()))\ntime.sleep(60)\n")
        driver = scratch / "driver.py"
        driver.write_text(LIFETIME_DRIVER)
        child_cmd = [sys.executable, str(driver), str(HERE), scenario, "--marsh", str(bin_dir / "marsh"), "--guest-artifacts", str(scratch),
                     "--sbx", str(bin_dir / "sbx"), "--lock", str(lock_path)]
        parent_code = (f"import subprocess, sys; sys.path.insert(0, {str(HERE)!r}); import vmlock\n"
                       f"sys.exit(vmlock.run({child_cmd!r}, run='lifetime-{scenario}').returncode)\n")
        parent = subprocess.Popen([sys.executable, "-c", parent_code], env={**lock_env(lock_path), "NFX_FAKE_HERE": str(fake_here)},
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        nested = None
        kept = None
        try:
            if not wait_for(pid_file):
                record(f"lock lifetime ({label})", False, "the descendant never started")
                parent.kill()
                return
            time.sleep(0.3)
            nested = int(pid_file.read_text().strip())
            alive_before = pid_alive(nested)
            if signal_parent:
                parent.send_signal(15)  # SIGTERM to the exact parent that holds the lock
            blocked_while_alive = 0
            alive_at_acquire = None
            end = time.monotonic() + 120
            while time.monotonic() < end:
                alive = pid_alive(nested)
                got = try_lock(lock_path)
                if got == "BLOCKED" and alive:
                    blocked_while_alive += 1
                if got == "GOT":
                    alive_at_acquire = pid_alive(nested)
                    break
                time.sleep(0.1)
            out, err = parent.communicate(timeout=180)
            try:
                result = json.loads(out)
            except ValueError:
                result = {}
            if result.get("scratch"):
                kept = pathlib.Path(result["scratch"])
            ok, detail = extra_checks(result, err, kept)
            record(f"lock lifetime ({label}): the competitor is refused until the SIGTERM-ignoring descendant is gone; {detail[0]}",
                   alive_before and blocked_while_alive > 0 and alive_at_acquire is False and ok,
                   f"descendant alive before {alive_before}; lock refused {blocked_while_alive}x while it lived; alive when the lock was taken: "
                   f"{alive_at_acquire}; {detail[1]}")
        finally:
            if parent.poll() is None:
                parent.kill()
                parent.wait()
            if nested is not None and pid_alive(nested):
                os.kill(nested, 9)  # the exact pid this control's own stand-in started
            if kept and kept.parent == pathlib.Path("/private/tmp") and kept.name.startswith("nfx-h3") and kept.is_dir():
                shutil.rmtree(kept)


def check_lock_lifetime() -> None:
    def unreaped(result, err, kept):
        errors = result.get("cleanup_errors", [])
        return (any("could not be reaped" in e for e in errors) and bool(kept and kept.is_dir()),
                ("the unreaped group is reported, the run fails, evidence kept", f"cleanup_errors {errors}; evidence kept {bool(kept and kept.is_dir())}"))

    def race(result, err, kept):
        return (bool(result.get("interrupted")) and bool(kept and kept.is_dir()),
                ("a signal at the spawn/registration boundary is deferred into the guarded section", f"interrupted {result.get('interrupted')}; evidence kept {bool(kept and kept.is_dir())}"))

    def race_unprotected(result, err, kept):
        return ("stopped leftover processes" in err,
                ("with NO deferral the helper itself drains the untracked child", f"helper stderr: {err.strip()[-90:]}"))

    def staging(result, err, kept):
        errors = result.get("cleanup_errors", [])
        return (any("kit staging interrupted" in e for e in errors) and bool(kept and kept.is_dir()),
                ("an interrupted kit staging is handled before any other child", f"cleanup_errors {errors}"))
    lifetime_case("unreaped group", "unreaped", False, unreaped)
    lifetime_case("spawn/registration race", "spawn-race", False, race)
    lifetime_case("spawn/registration race, deferral disabled", "spawn-race-unprotected", False, race_unprotected)
    lifetime_case("interrupted kit staging", "staging", True, staging)


def check_group_tracking() -> None:
    """The tracked-group mechanism itself: an unreapable group fails the run; only recorded groups are ever signalled."""
    saved = (harness03._killpg, harness03.GRACE_SECONDS)
    harness03.GRACE_SECONDS = 0.4
    harness03._killpg = lambda pgid, sig: None  # a group that cannot be killed
    harness03.UNREAPED.clear()
    stuck = None
    try:
        try:
            harness03.tracked_run(["sleep", "30"], timeout=0.3)
        except subprocess.TimeoutExpired:
            pass
        recorded = list(harness03.UNREAPED)
        if recorded:
            stuck = int(recorded[0].split("process group ")[1].split()[0])
        record("group tracking: a group that cannot be reaped is recorded (and so fails the run)",
               bool(recorded) and "could not be reaped" in recorded[0], recorded[0] if recorded else "nothing recorded")
    finally:
        harness03._killpg, harness03.GRACE_SECONDS = saved
        harness03.UNREAPED.clear()
        if stuck:  # clean up this control's own exact group
            try:
                os.killpg(stuck, 9)
                os.waitpid(stuck, 0)
            except (ProcessLookupError, ChildProcessError):
                pass
    stranger = subprocess.Popen(["sleep", "30"], start_new_session=True)
    try:
        try:
            harness03.stop_group(stranger)
            record("group tracking: a group this run did not record is never signalled", False, "stop_group accepted it")
        except RuntimeError as error:
            record("group tracking: a group this run did not record is never signalled", stranger.poll() is None, f"refused: {str(error)[:70]}; process untouched")
    finally:
        stranger.kill()
        stranger.wait()
    done = harness03.tracked_run(["sh", "-c", "sleep 30 & echo $!"], capture_output=True, text=True)
    orphan = int(done.stdout.strip())
    gone = False
    end = time.monotonic() + 10
    while time.monotonic() < end and not gone:
        gone = not pid_alive(orphan)
        time.sleep(0.1)
    record("group tracking: a descendant that outlives its leader is swept when the call returns", gone, f"background child {orphan} alive: {not gone}")
    if not gone and pid_alive(orphan):
        os.kill(orphan, 9)


def check_interruption_control() -> None:
    """The control is not vacuous: the OLD pattern (`held()` around `subprocess.run`) does release the lock when the
    parent is terminated while the child keeps working, which is what the two checks below would then report."""
    with tempfile.TemporaryDirectory() as scratch:
        scratch = pathlib.Path(scratch)
        lock_path = scratch / "lock"
        lock_path.write_bytes(b"")
        child = scratch / "child.py"
        child.write_text("import signal, time\nsignal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
                         f"open({str(scratch / 'UP')!r}, 'w').close()\ntime.sleep(5)\n")
        parent = subprocess.Popen([sys.executable, "-c", f"import subprocess, sys; sys.path.insert(0, {str(HERE)!r}); import vmlock\n"
                                   f"with vmlock.held('old-pattern'):\n    subprocess.run([sys.executable, {str(child)!r}])\n"],
                                  env=lock_env(lock_path))
        wait_for(scratch / "UP", 30)
        parent.send_signal(15)
        parent.wait(timeout=30)
        leaked = try_lock(lock_path)
        record("interruption control: the old pattern (held() around subprocess.run) releases the lock while the child still works (so the checks can fail)",
               leaked == "GOT", f"competitor while the child is alive: {leaked}")
        time.sleep(5.5)  # the child is this check's own and exits by itself


def check_interruption() -> None:
    """SIGTERM to the exact parent while its working child runs (a stand-in marsh that ignores SIGTERM): the lock
    (synthetic, VMLOCK_TEST_PATH) must stay held until that child has exited, the child must still clean up, and the
    ownership evidence must be preserved. Both H1b lock paths: HARNESS-03's own runner and the live self-check."""
    for label, live_path in (("HARNESS-03 runner", False), ("live self-check", True)):
        with tempfile.TemporaryDirectory() as scratch:
            scratch = pathlib.Path(scratch)
            lock_path = scratch / "lock"
            lock_path.write_bytes(b"")
            markers = scratch / "markers"
            markers.mkdir()
            bin_dir = scratch / "fake" / "bin"
            bin_dir.mkdir(parents=True)
            (scratch / "fake" / "libexec" / "marsh").mkdir(parents=True)
            (bin_dir / "marsh").write_text(FAKE_MARSH_BLOCKING.replace("@DIR@", str(markers)))
            (bin_dir / "sbx").write_text("#!/bin/sh\ncase \"$1\" in ls) echo '[]' ;; create) exit 1 ;; esac\nexit 0\n")
            for name in ("marsh", "sbx"):
                (bin_dir / name).chmod(0o755)
            if live_path:
                command = [sys.executable, str(HERE / "selfcheck.py"), "--only-live", "--live", str(bin_dir / "marsh"), "--sbx", str(bin_dir / "sbx")]
            else:
                command = [sys.executable, str(HERE / "harness03.py"), "--marsh", str(bin_dir / "marsh"), "--guest-artifacts", str(scratch),
                           "--sbx", str(bin_dir / "sbx"), "--lock", str(lock_path)]
            proc = subprocess.Popen(command, env=lock_env(lock_path), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            kept_paths: list[pathlib.Path] = []
            try:
                if not wait_for(markers / "RUNNING"):
                    record(f"interruption ({label})", False, "the working child never started")
                    proc.kill()
                    continue
                held_before = try_lock(lock_path)
                proc.send_signal(15)  # SIGTERM to the exact parent
                stopping = wait_for(markers / "STOPPING", 30)  # the child is now in its cleanup (`marsh stop`)
                held_during = try_lock(lock_path)  # the child is still working: the lock must still be held
                out, err = proc.communicate(timeout=120)
                after = try_lock(lock_path)
                for line in (out + err).splitlines():
                    for token in line.replace("kept at ", " ").replace("scratch ", " ").split():
                        if token.startswith("/private/tmp/nfx-h3"):
                            kept_paths.append(pathlib.Path(token))
                evidence = [p for p in kept_paths if p.is_dir() and list(p.glob("control/*/vm-ownership.json"))]
                record(f"interruption ({label}): SIGTERM to the parent does not release the lock while the child works; evidence preserved",
                       held_before == "BLOCKED" and stopping and held_during == "BLOCKED" and after == "GOT" and bool(evidence) and proc.returncode != 0,
                       f"before {held_before}; child in cleanup {stopping}; during cleanup {held_during}; after exit {after}; parent exit {proc.returncode}; "
                       f"ownership evidence kept: {bool(evidence)}")
            finally:
                if proc.poll() is None:
                    proc.kill()
                    proc.wait()
                for path in set(kept_paths):  # only the exact scratch directories these synthetic runs reported
                    if path.parent == pathlib.Path("/private/tmp") and path.name.startswith("nfx-h3") and path.is_dir():
                        shutil.rmtree(path)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--live", metavar="MARSH")
    parser.add_argument("--sbx", default="sbx", help="the sbx executable for --live (a stand-in in the interruption controls)")
    parser.add_argument("--only-live", action="store_true", help="skip the offline checks (used by the interruption control)")
    args = parser.parse_args()
    if not args.only_live:
        for check in (check_checkpoint, check_sec23, check_event_witness, check_egress, check_harness03, check_live_judgement,
                      check_lock, check_lock_ordering, check_interruption_control, check_interruption, check_group_tracking, check_nested_control, check_nested_interruption, check_lock_lifetime):
            check()
    if args.live:
        live(args.live, args.sbx)
    missed = [name for name, caught, _ in RESULTS if not caught]
    print(f"\n{len(RESULTS) - len(missed)}/{len(RESULTS)} self-checks as expected" + (f"; NOT AS EXPECTED: {missed}" if missed else "")
          + (f"; SKIPPED: {[name for name, _ in SKIPPED]}" if SKIPPED else ""))
    return 1 if missed else 0


if __name__ == "__main__":
    sys.exit(main())
