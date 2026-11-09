#!/usr/bin/env python3
"""HARNESS-03: fixtures, fixture Kit and fake agent self-test (TEST-PLAN C.0).

The check logic is `run(ctx)`. The harness core (H1a, `services_uat.py`) registers
it without importing anything else of ours:

    import harness03
    scenario = harness03.REGISTRATION    # id, tier, owner, break, run
    result = scenario["run"](ctx)        # result["checks"]["c1".."c5"], result["failed_check"]

CTX is duck-typed; the core supplies:

    ctx.workdir                    pathlib.Path under /private/tmp/nfx-<run>/ (host, writable)
    ctx.project                    pathlib.Path: the scope's project folder (the fake agent's cwd)
    ctx.break_mode                 None, or "NFX_FAKE_EXEC=off" (the self-test's break)
    ctx.marsh_run(argv, timeout)   -> object with .returncode .stdout .stderr (str); runs
                                   `marsh ARGV` in the scope (its env, MARSH_VM_PREFIX etc.)
    ctx.agent_exec(agent, script, timeout)
                                   -> list[str]: the agent_message_chunk texts of one ACP prompt
                                   `exec SCRIPT` to the fake agent registered as AGENT
    ctx.sandbox(role, workspace)   context manager -> sandbox named `nfx-<run>-<role>` with the
                                   directory WORKSPACE mounted at the same path;
                                   sandbox.name, sandbox.run(argv, timeout) -> object with
                                   .returncode .stdout .stderr, run as the sandbox user
    ctx.sbx                        the sbx executable (default "sbx"), used to load the image cache
    ctx.agent_exec / ctx.marsh_run / sandbox.run must be BOUNDED: they take the timeout
    and must raise (subprocess.TimeoutExpired or TimeoutError) when it passes.

The standalone runner at the bottom builds such a CTX from a dev install of
marsh (it takes the VM lock itself; see --help). The break: with
`NFX_FAKE_EXEC=off` the fake agent's `exec` verb is off, so c2 fails and
nothing else does. `run` returns a `verdict`: `pass`, `fail`, `verified_break` (break
mode, c2 failed with the disabled-verb observation and every other check passed) or
`invalid_negative_control` (break mode, anything else: a setup error, a timeout, a
different failing check). A cleanup problem makes the standalone runner exit nonzero
and keeps the scratch directory (the ownership record) for inspection. The fixture honours the variable in its own environment, or
the marker file `<project>/.nfx-fake-exec-off`, which `run` creates, so it works
whatever environment reaches the agent.
"""
import base64
import json
import os
import pathlib
import re
import shutil
import signal
import subprocess
import sys
import time

HERE = pathlib.Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import sentinels  # noqa: E402
import stage  # noqa: E402
import vmlock  # noqa: E402

VERSIONS = json.loads((HERE / "nfx-kit/versions.json").read_text())
NODE_IMAGE = "node:22-trixie-slim@sha256:be1e2a3dc2c4fd68cdfadad2a696a280e0d2749098bee509230174d72fb768c6"


def check(ok: bool, expected, observed) -> dict:
    return {"ok": bool(ok), "expected": expected, "observed": observed}


def poll(fn, ceiling: float = 120, interval: float = 1.0):
    """Bounded poll (no fixed sleeps): the first truthy fn() result, else None."""
    end = time.monotonic() + ceiling
    while time.monotonic() < end:
        value = fn()
        if value:
            return value
        time.sleep(interval)
    return None


def c1_tool_versions(ctx) -> dict:
    done = ctx.marsh_run(["run", "nfx", "exec", "sh", "-c", VERSIONS["command"]], 600)
    lines = [line.strip() for line in done.stdout.splitlines() if line.strip()]
    expected = [item["exact"] for item in VERSIONS["lines"]]
    matched = lines == expected
    return check(done.returncode == 0 and matched, expected,
                 {"exit": done.returncode, "lines": lines, "stderr": done.stderr[-400:]})


def c2_fake_agent(ctx) -> dict:
    marker = pathlib.Path(ctx.project) / ".nfx-fake-exec-off"
    if getattr(ctx, "break_mode", None) == "NFX_FAKE_EXEC=off":
        marker.write_text("")
    try:
        updates = ctx.agent_exec("nfx-acp", "echo nfx-ok", 300)
    finally:
        marker.unlink(missing_ok=True)
    exit_update = next((u for u in updates if u.startswith("fixture:exit:")), None)
    outs = [u for u in updates if u.startswith("fixture:out:")]
    decoded = b"".join(base64.b64decode(u[len("fixture:out:"):]) for u in outs).decode() if outs else None
    result = check(exit_update == "fixture:exit:0" and decoded == "nfx-ok\n",
                   {"exit": "fixture:exit:0", "stdout": "nfx-ok\n"}, {"exit": exit_update, "stdout": decoded, "updates": updates})
    # The disabled-verb observation: the prompt was answered (only the echo), no exit, no output.
    result["disabled_verb"] = exit_update is None and not outs and updates == ["fixture:exec echo nfx-ok"]
    return result


def c3_stack_a_sentinels(ctx, sandbox, work: pathlib.Path) -> dict:
    project = work / "stack-a"
    stage.stage("stack-a", project)
    name = "nfx-fx-stack-a"

    def run(argv):
        done = sandbox.run(argv, 120)
        return done.returncode, done.stdout

    up = sandbox.run(["sh", "-c", f"cd {project} && docker compose -p {name} up -d --wait"], 600)
    try:
        # The writer seeds asynchronously: bounded poll until the items exist.
        poll(lambda: run(["docker", "exec", f"{name}-db-1", "psql", "-U", "postgres", "-d", "app", "-Atc",
                          "select count(*) from items"])[1].strip() == "1000", 90)
        observed = sentinels.read_fixture(run, name, project)
        problems = sentinels.compare(observed)
        return check(up.returncode == 0 and not problems, "sentinels equal stack-a/sentinels.json",
                     {"compose_up_exit": up.returncode, "problems": problems, "observed": observed})
    finally:
        sandbox.run(["sh", "-c", f"cd {project} && docker compose -p {name} down -v"], 300)


def c4_testcontainers(ctx, sandbox, work: pathlib.Path) -> dict:
    tc = work / "tc"
    stage.stage("tc", tc)
    base = ["docker", "run", "--rm", "--user", "root", "--network", "host",
            "-v", "/var/run/docker.sock:/var/run/docker.sock", "-e", "DOCKER_HOST=unix:///var/run/docker.sock",
            "-e", "TESTCONTAINERS_HOST_OVERRIDE=localhost", "-v", f"{tc}:/w", "-w", "/w", NODE_IMAGE, "node"]
    done = sandbox.run([*base, "tc.test.js"], 300)
    last = done.stdout.strip().splitlines()[-1] if done.stdout.strip() else ""
    try:
        verdict = json.loads(last)
    except ValueError:
        verdict = None
    # Ryuk stays on (DECISIONS 15): recorded, not part of c4.
    ryuk = sandbox.run([*base, "tc-ryuk.test.js"], 300)
    return check(done.returncode == 0 and bool(verdict and verdict.get("ok") is True), {"ok": True},
                 {"exit": done.returncode, "result": verdict, "stderr": done.stderr[-300:],
                  "ryuk_on_run": {"exit": ryuk.returncode, "out": ryuk.stdout.strip()[-400:]}})


def c5_ping(ctx, sandbox, work: pathlib.Path) -> dict:
    shutil.copy(HERE / "rawdocker.py", work / "rawdocker.py")
    shutil.copytree(HERE / "cases", work / "cases", dirs_exist_ok=True, ignore=shutil.ignore_patterns("generate.py"))
    done = sandbox.run(["python3", str(work / "rawdocker.py"), "--docker-host", "unix:///var/run/docker.sock",
                        "--check", "--case", "B01-ping", str(work / "cases/benign.json")], 60)
    try:
        result = json.loads(done.stdout.strip().splitlines()[-1])
        first = result["responses"][0]
        got = {"status": first["status"], "body": first["body"]}
    except (ValueError, IndexError, KeyError):
        got = None
    return check(done.returncode == 0 and got == {"status": 200, "body": "OK"}, {"status": 200, "body": "OK"}, got)


def run(ctx) -> dict:
    """Run HARNESS-03; every check is attempted, `failed_check` is the first that failed."""
    checks = {}
    for name, fn in (("c1", c1_tool_versions), ("c2", c2_fake_agent)):
        try:
            checks[name] = fn(ctx)
        except Exception as error:  # a crashed check is a failed check, with the reason
            checks[name] = check(False, "no exception", f"{type(error).__name__}: {error}")
    work = pathlib.Path(ctx.workdir) / "fx"
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir(parents=True)
    try:
        with ctx.sandbox("fx", work) as sandbox:
            loaded = tracked_run([sys.executable, str(HERE / "image_cache.py"), "load", "--vm", sandbox.name,
                                     "--sbx", getattr(ctx, "sbx", "sbx")], capture_output=True, text=True, timeout=900)
            if loaded.returncode != 0:
                raise RuntimeError(f"image cache load failed: {loaded.stderr.strip()[-300:]}")
            if sandbox.run(["true"], 60).returncode != 0:
                raise RuntimeError("the fx sandbox does not answer")
            for name, fn in (("c3", c3_stack_a_sentinels), ("c4", c4_testcontainers), ("c5", c5_ping)):
                try:
                    checks[name] = fn(ctx, sandbox, work)
                except Exception as error:
                    checks[name] = check(False, "no exception", f"{type(error).__name__}: {error}")
    except Exception as error:
        for name in ("c3", "c4", "c5"):
            checks.setdefault(name, check(False, "a plain sandbox", f"{type(error).__name__}: {error}"))
    failed = next((name for name in ("c1", "c2", "c3", "c4", "c5") if not checks[name]["ok"]), None)
    if getattr(ctx, "break_mode", None):
        genuine = (failed == "c2" and checks["c2"].get("disabled_verb") is True
                   and all(checks[n]["ok"] for n in ("c1", "c3", "c4", "c5")))
        verdict = "verified_break" if genuine else "invalid_negative_control"
    else:
        verdict = "pass" if failed is None else "fail"
    return {"id": "HARNESS-03", "ok": failed is None, "verdict": verdict, "failed_check": failed, "checks": checks}


REGISTRATION = {
    "id": "HARNESS-03",
    "owner": "H1b",
    "tier": "smoke",
    "estimate_minutes": 6,
    "breaks": [{"mode": "NFX_FAKE_EXEC=off", "kind": "fixture", "failed_check": "c2"}],
    "needs": ["marsh_run", "agent_exec", "sandbox", "workdir", "project"],
    "run": run,
}


# --- standalone runner ---------------------------------------------------------

# --- tracked child processes ---------------------------------------------------------------------
# Every child this run spawns is started in a process group THIS run creates (inside the session the shared
# VM-lock helper gave this run: nothing here calls setsid, so the helper's drain of that session covers every
# descendant even if this teardown fails) and records by its exact leader pid. Before the locked section can end (success, failure or interruption) each group is
# terminated and reaped: SIGTERM to that exact group, a bounded wait, then SIGKILL to that exact group. Nothing
# outside a recorded group is ever signalled, and nothing is chosen by name or pattern. A group that cannot be
# reaped is recorded in UNREAPED and fails the run.
INTERRUPTS = {signal.SIGINT, signal.SIGTERM, signal.SIGHUP}
TRACKED: dict[int, subprocess.Popen] = {}
UNREAPED: list[str] = []
GRACE_SECONDS = 5.0


def _killpg(pgid: int, sig: int) -> None:  # a seam: the controls replace it to simulate an unkillable group
    os.killpg(pgid, sig)


def _group_alive(pgid: int) -> bool:
    try:
        os.killpg(pgid, 0)  # signal 0: existence only
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def _stop_group_once(proc: subprocess.Popen) -> bool:
    pgid = proc.pid
    if pgid not in TRACKED or TRACKED[pgid] is not proc or pgid == os.getpgrp():
        raise RuntimeError(f"refusing to signal process group {pgid}: this run did not create it")
    for sig in (signal.SIGTERM, signal.SIGKILL):
        if proc.poll() is not None and not _group_alive(pgid):
            break
        try:
            _killpg(pgid, sig)
        except ProcessLookupError:
            pass
        deadline = time.monotonic() + GRACE_SECONDS
        while time.monotonic() < deadline:
            if proc.poll() is not None and not _group_alive(pgid):
                break
            time.sleep(0.05)
    try:
        proc.wait(timeout=GRACE_SECONDS)  # reap the leader
    except subprocess.TimeoutExpired:
        pass
    return proc.poll() is not None and not _group_alive(pgid)


def stop_group(proc: subprocess.Popen) -> bool:
    """Terminate and reap the exact group of PROC (see above). A signal that interrupts the cleanup itself
    cannot cut it short: the cleanup runs to completion and the signal's exception is re-raised after."""
    pending = None
    ok = False
    for _ in range(3):
        try:
            ok = _stop_group_once(proc)
            break
        except BaseException as error:  # e.g. Interrupted arriving inside the cleanup
            if isinstance(error, RuntimeError):
                raise
            pending = pending or error
    if not ok:
        UNREAPED.append(f"process group {proc.pid} ({os.path.basename(str(proc.args[0]))}) could not be reaped")
    TRACKED.pop(proc.pid, None)
    if pending is not None:
        raise pending
    return ok


def tracked_run(argv, *, timeout=None, check=False, capture_output=False, text=False, input=None, **kwargs):
    """subprocess.run, but the child (and everything it starts) lives in a recorded process group that is
    terminated and reaped before this returns or raises, whatever the reason (completion, timeout, a signal)."""
    if capture_output:
        kwargs.setdefault("stdout", subprocess.PIPE)
        kwargs.setdefault("stderr", subprocess.PIPE)
    if input is not None:
        kwargs["stdin"] = subprocess.PIPE
    proc = None
    held_signals = signal.pthread_sigmask(signal.SIG_BLOCK, INTERRUPTS)  # deferred across spawn and registration
    try:
        proc = subprocess.Popen(argv, process_group=0, text=text, **kwargs)  # own group, SAME session as this run
        TRACKED[proc.pid] = proc
        signal.pthread_sigmask(signal.SIG_SETMASK, held_signals)  # a pending signal is delivered here, inside this try
        out, err = proc.communicate(input=input, timeout=timeout)
    finally:
        signal.pthread_sigmask(signal.SIG_SETMASK, held_signals)
        if proc is not None:
            stop_group(proc)  # on success this only sweeps descendants that outlived the leader
    if check and proc.returncode != 0:
        raise subprocess.CalledProcessError(proc.returncode, argv, out, err)
    return subprocess.CompletedProcess(argv, proc.returncode, out, err)


class OwnershipError(Exception):
    """The scope's VM ownership record is missing or unreadable: cleanup cannot be verified."""


OWNERSHIP_TOP = {"version", "vms", "grants"}
OWNERSHIP_ENTRY = {"purpose", "key", "uuid"}
GRANT_FIELDS = {"session", "prefix", "roots", "scratch", "max_vms", "revoked", "cleanup_uncertain", "names"}
GRANT_REQUIRED = GRANT_FIELDS - {"cleanup_uncertain"}  # DevGrantRecord: cleanup_uncertain is #[serde(default)]


def _no_duplicate_keys(pairs):
    """json.loads keeps the LAST of two equal keys; marsh's serde structs reject them. So do we:
    a duplicate field could hide an owned VM behind a second, shorter `vms` map."""
    seen = {}
    for key, value in pairs:
        if key in seen:
            raise ValueError(f"duplicate JSON field {key!r}")
        seen[key] = value
    return seen


def _no_constants(name):
    raise ValueError(f"non-finite number {name}")


def _is_int(value) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def read_ownership(path: pathlib.Path) -> set[str]:
    """Every VM name the scope's daemon recorded (pending intents and development-grant names
    included: each names a VM that must be gone). The persisted schema is validated exactly as
    marsh does (vm_ownership.rs, all structs deny_unknown_fields): version 2 `{version: 2,
    vms: {name: {purpose, key, uuid?}}, grants?: {session: DevGrantRecord}}` or the supported
    legacy version 1 `{version: 1, vms: {name: uuid}}`; duplicate JSON fields, a non-integer
    version, an incomplete or unknown-field grant and anything else raise OwnershipError, valid
    JSON or not: a malformed map is never read as "no VMs"."""
    path = pathlib.Path(path)
    try:
        record = json.loads(path.read_text(), object_pairs_hook=_no_duplicate_keys, parse_constant=_no_constants)
    except (OSError, ValueError) as error:
        raise OwnershipError(f"ownership record {path} unreadable ({type(error).__name__}: {error})") from None

    def bad(why: str):
        return OwnershipError(f"ownership record {path} is malformed: {why}")

    if not isinstance(record, dict):
        raise bad(f"top level is {type(record).__name__}, not an object")
    unknown = set(record) - OWNERSHIP_TOP
    if unknown:
        raise bad(f"unknown top-level fields {sorted(unknown)}")
    version = record.get("version")
    if not _is_int(version) or version not in (1, 2):
        raise bad(f"unsupported version {version!r} (an integer 1 or 2 is required)")
    vms = record.get("vms")
    if not isinstance(vms, dict):
        raise bad(f"`vms` is {type(vms).__name__}, not an object")
    names: set[str] = set()
    for name, entry in vms.items():
        if not name:
            raise bad("an empty VM name")
        if version == 1:
            if not isinstance(entry, str) or not entry:
                raise bad(f"legacy entry {name!r} is not a UUID string")
        else:
            if not isinstance(entry, dict) or set(entry) - OWNERSHIP_ENTRY or not {"purpose", "key"} <= set(entry):
                raise bad(f"entry {name!r} is not {{purpose, key, uuid?}}")
            if not isinstance(entry["purpose"], str) or not isinstance(entry["key"], str):
                raise bad(f"entry {name!r} has a non-string purpose or key")
            if entry.get("uuid") is not None and (not isinstance(entry["uuid"], str) or not entry["uuid"]):
                raise bad(f"entry {name!r} has an invalid uuid")
        names.add(name)
    grants = record.get("grants", {})
    if not isinstance(grants, dict) or (version == 1 and "grants" in record):
        raise bad("`grants` is not an object (or present in a legacy record)")
    for session, grant in grants.items():
        if not isinstance(grant, dict):
            raise bad(f"grant {session!r} is not an object")
        if set(grant) - GRANT_FIELDS:
            raise bad(f"grant {session!r} has unknown fields {sorted(set(grant) - GRANT_FIELDS)}")
        if GRANT_REQUIRED - set(grant):
            raise bad(f"grant {session!r} lacks fields {sorted(GRANT_REQUIRED - set(grant))}")
        if not all(isinstance(grant[k], str) for k in ("session", "prefix", "scratch")):
            raise bad(f"grant {session!r}: session, prefix and scratch must be strings")
        if not (isinstance(grant["roots"], list) and all(isinstance(r, str) for r in grant["roots"])):
            raise bad(f"grant {session!r}: roots must be a list of paths")
        if not _is_int(grant["max_vms"]) or grant["max_vms"] < 0:
            raise bad(f"grant {session!r}: max_vms must be a non-negative integer")
        if not isinstance(grant["revoked"], bool) or not isinstance(grant.get("cleanup_uncertain", False), bool):
            raise bad(f"grant {session!r}: revoked and cleanup_uncertain must be booleans")
        if not isinstance(grant["names"], dict):
            raise bad(f"grant {session!r}: names is not an object")
        for name, uuid in grant["names"].items():
            if not name or (uuid is not None and not isinstance(uuid, str)):
                raise bad(f"grant {session!r} lists an invalid name entry")
            names.add(name)
    return names


def verify_cleanup(*, run_id: str, owned: set[str] | None, ownership_error: str | None, scope_ran: bool,
                   stop_returncode: int | None, removal_errors: list[str], remaining: list[str] | None) -> list[str]:
    """Every cleanup problem, as strings (empty = verified clean). Missing or corrupt ownership
    evidence is a problem whenever the scope ran jobs: without it nothing says which VMs to check."""
    errors = list(removal_errors)
    if owned is not None and not (isinstance(owned, (set, frozenset)) and all(isinstance(n, str) and n for n in owned)):
        errors.append(f"ownership evidence is malformed ({type(owned).__name__}): cleanup cannot be verified")
        owned = None
        ownership_error = ownership_error or "malformed ownership evidence"
    if stop_returncode != 0:
        errors.append(f"marsh stop exited {stop_returncode}")
    if ownership_error:
        errors.append(ownership_error)
    elif scope_ran and not owned:
        errors.append("the ownership record lists no VMs although the scope ran jobs")
    if remaining is None:
        errors.append("cannot verify cleanup: sbx ls failed")
    else:
        leftovers = sorted(n for n in remaining if n in (owned or set()) or n.startswith(f"nfx-{run_id}-"))
        if leftovers:
            errors.append(f"leftover sandboxes: {leftovers}")
    return errors


def acp_exec(argv: list[str], cwd, env, script: str, timeout: float) -> list[str]:
    """One ACP prompt `exec SCRIPT` to the agent started by ARGV; returns the agent_message_chunk
    texts. Every read is bounded by TIMEOUT (selectors, not blocking readline). A JSON-RPC error
    on any request, a stop reason other than end_turn, or a server that closes early raises:
    only a successfully completed prompt can support a verdict about the verb."""
    import selectors
    proc = None
    selector = None
    buffer, texts = b"", []

    def send(obj):
        proc.stdin.write((json.dumps({"jsonrpc": "2.0", **obj}) + "\n").encode())
        proc.stdin.flush()

    held_signals = signal.pthread_sigmask(signal.SIG_BLOCK, INTERRUPTS)  # deferred across spawn and registration
    try:
        proc = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                process_group=0)  # own group, same session as this run
        TRACKED[proc.pid] = proc
        signal.pthread_sigmask(signal.SIG_SETMASK, held_signals)  # a pending signal is delivered here, inside this try
        deadline = time.monotonic() + timeout
        selector = selectors.DefaultSelector()
        selector.register(proc.stdout, selectors.EVENT_READ)
        send({"id": 1, "method": "initialize", "params": {"protocolVersion": 1, "clientCapabilities": {}}})
        send({"id": 2, "method": "session/new", "params": {"cwd": str(cwd), "mcpServers": []}})
        finished = False
        while not finished:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"ACP prompt exceeded {timeout}s")
            if not selector.select(min(remaining, 1.0)):
                continue
            chunk = os.read(proc.stdout.fileno(), 65536)
            if not chunk:
                raise RuntimeError("the ACP server closed before the prompt finished")
            buffer += chunk
            while b"\n" in buffer:
                line, buffer = buffer.split(b"\n", 1)
                if not line.strip():
                    continue
                message = json.loads(line)
                if "error" in message and message.get("id") in (1, 2, 3):
                    raise RuntimeError(f"JSON-RPC error on request {message['id']}: {json.dumps(message['error'])[:200]}")
                if message.get("id") == 2 and "result" in message:
                    send({"id": 3, "method": "session/prompt", "params": {
                        "sessionId": message["result"]["sessionId"], "prompt": [{"type": "text", "text": f"exec {script}"}]}})
                elif message.get("method") == "session/update":
                    update = message["params"]["update"]
                    if update.get("sessionUpdate") == "agent_message_chunk":
                        texts.append(update["content"]["text"])
                elif message.get("id") == 3:
                    stop = message.get("result", {}).get("stopReason")
                    if stop != "end_turn":
                        raise RuntimeError(f"the prompt ended with stopReason {stop!r}, not end_turn")
                    finished = True
                    break
    finally:
        signal.pthread_sigmask(signal.SIG_SETMASK, held_signals)
        if selector is not None:
            selector.close()
        if proc is not None:
            try:
                proc.stdin.close()
            except OSError:
                pass
            try:
                proc.wait(timeout=5)  # a well-behaved agent exits when its stdin closes
            except subprocess.TimeoutExpired:
                pass
            try:
                stop_group(proc)  # the agent's exact group, terminated and reaped
            finally:
                proc.stdout.close()
    return texts


def main() -> int:
    import argparse
    import contextlib
    import hashlib
    import os
    import secrets
    import signal

    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--marsh", required=True, help="a dev install's bin/marsh")
    parser.add_argument("--guest-artifacts", help="the install's libexec/marsh (default: next to --marsh)")
    parser.add_argument("--break", dest="break_mode", choices=["NFX_FAKE_EXEC=off"])
    parser.add_argument("--sbx", default="sbx")
    parser.add_argument("--lock", help="TEST ONLY: a synthetic lock file (sets VMLOCK_TEST_PATH for the shared helper); never the real lock")
    parser.add_argument("--no-lock", action="store_true",
                        help="the caller already holds the VM lock (vmlock.run); without it this process re-runs itself "
                             "under vmlock.run, so the lock is held until the working child has exited")
    args = parser.parse_args()
    if args.lock:
        os.environ["VMLOCK_TEST_PATH"] = args.lock  # read by the shared helper when it is first imported
    if not args.no_lock:
        # The work happens in a CHILD of vmlock.run: the lock is held until that child is reaped, and
        # INT/TERM/HUP sent here are forwarded to it (it then cleans up and keeps its evidence).
        inner = [sys.executable, str(pathlib.Path(__file__).resolve()), *sys.argv[1:], "--no-lock"]
        try:
            return vmlock.run(inner, run=f"harness03-{os.getpid()}").returncode
        except vmlock.lock_error() as error:
            print(f"harness03: cannot use the VM lock: {error}", file=sys.stderr)
            return 2

    run_id = "h3" + secrets.token_hex(3)
    root = pathlib.Path("/private/tmp") / f"nfx-{run_id}"
    marsh = pathlib.Path(args.marsh).resolve()
    guest = pathlib.Path(args.guest_artifacts or marsh.parents[1] / "libexec/marsh")
    home, control_root, project = root / "home", root / "control", root / "project"
    for path in (home, control_root, project):
        path.mkdir(parents=True, mode=0o700)
    control_home = control_root.resolve() / hashlib.sha256(os.fsencode(home.resolve())).hexdigest()
    control_home.mkdir(mode=0o700)
    try:
        guard = vmlock.LockGuard(vmlock.lock_path())  # inside the lock: the caller (vmlock.run) holds it
    except (vmlock.lock_error(), OSError, FileNotFoundError) as error:
        print(f"harness03: cannot use the VM lock: {error}", file=sys.stderr)
        shutil.rmtree(root, ignore_errors=True)  # this run's own scratch, created above
        return 2

    class Interrupted(BaseException):
        """INT/TERM/HUP while the run works: unwind through the cleanup below, which keeps the evidence."""

    interrupted = {"signalled": False}

    def on_signal(signum, _frame):
        if interrupted["signalled"] is False:
            interrupted["signalled"] = True
            raise Interrupted(signal.Signals(signum).name)
    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):  # before the FIRST child, so no spawn runs unguarded
        signal.signal(signum, on_signal)
    try:
        kit_stage = tracked_run([sys.executable, str(HERE / "nfx-kit/stage.py")], capture_output=True, text=True, check=True)
        shutil.copy(kit_stage.stdout.strip(), control_home / "commands.json")
    except BaseException as error:  # an interruption (or failure) while staging: the group is already reaped
        for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            signal.signal(signum, signal.SIG_IGN)
        reason = f"interrupted by {error}" if isinstance(error, Interrupted) else f"{type(error).__name__}: {error}"
        for leftover in list(TRACKED.values()):
            try:
                stop_group(leftover)
            except Exception:
                pass
        print(json.dumps({"id": "HARNESS-03", "ok": False, "verdict": "invalid_negative_control" if args.break_mode else "fail",
                          "failed_check": None, "checks": {}, "scratch": str(root), "run_id": run_id,
                          "cleanup_errors": [f"kit staging {reason}: no VM existed yet; scratch kept"] + list(UNREAPED),
                          "owned_vms": [], "inventory": {"ok": True, "owned_remaining": []},
                          "lock_check": {"unchanged": not guard.verify(), "problems": guard.verify()}}, indent=1, sort_keys=True))
        return 1
    env = {k: os.environ[k] for k in ("PATH", "HOME", "TERM", "LANG", "TMPDIR", "USER", "LOGNAME") if k in os.environ}
    env.update({"MARSH_HOME": str(home), "MARSH_CONTROL_HOME": str(control_root), "MARSH_VM_PREFIX": "marsh-xnfx01-",
                "MARSH_GUEST_ARTIFACTS": str(guest)})

    cleanup_errors: list[str] = []

    class Proc:
        def __init__(self, done):
            self.returncode, self.stdout, self.stderr = done.returncode, done.stdout, done.stderr

    class Sandbox:
        def __init__(self, name):
            self.name = name

        def run(self, argv, timeout):
            return Proc(tracked_run([args.sbx, "exec", self.name, *argv], capture_output=True, text=True, timeout=timeout))

    def sbx_names() -> list[str]:
        listed = tracked_run([args.sbx, "ls", "--json"], capture_output=True, text=True, timeout=60, check=True)
        return [entry["name"] for entry in json.loads(listed.stdout)]


    class Ctx:
        sbx = args.sbx
        workdir = root
        break_mode = args.break_mode

        def __init__(self):
            self.project = project

        def marsh_run(self, argv, timeout):
            return Proc(tracked_run([str(marsh), *argv], cwd=project, env=env, capture_output=True, text=True, timeout=timeout))

        def agent_exec(self, agent, script, timeout):
            return acp_exec([str(marsh), "run", "--acp", agent], project, env, script, timeout)

        @contextlib.contextmanager
        def sandbox(self, role, workspace):
            name = f"nfx-{run_id}-{role}"
            tracked_run([args.sbx, "create", "--name", name, "shell", str(workspace)], check=True, capture_output=True, timeout=600)
            try:
                yield Sandbox(name)
            finally:
                removed = tracked_run([args.sbx, "rm", "-f", name], capture_output=True, text=True, timeout=300)
                if removed.returncode != 0:
                    cleanup_errors.append(f"sbx rm {name} failed: {removed.stderr.strip()[-200:]}")

    ctx = Ctx()

    result = {"id": "HARNESS-03", "ok": False, "verdict": "fail", "failed_check": None, "checks": {}}
    owned: set[str] = set()
    ownership_error = None
    scope_ran = False
    try:
        scope_ran = True
        result = run(ctx)
    except Interrupted as signal_name:
        result["interrupted"] = str(signal_name)
        result["verdict"] = "invalid_negative_control" if args.break_mode else "fail"
        cleanup_errors.append(f"interrupted by {signal_name}: cleanup ran, evidence is kept")
    except Exception as error:
        result["crashed"] = f"{type(error).__name__}: {error}"
        result["verdict"] = "invalid_negative_control" if args.break_mode else "fail"
    finally:
        for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            signal.signal(signum, signal.SIG_IGN)  # a second signal must not cut the cleanup short
        for leftover in list(TRACKED.values()):  # nothing this run started may outlive the locked section
            try:
                stop_group(leftover)
            except Exception as error:
                cleanup_errors.append(f"stopping process group {leftover.pid}: {type(error).__name__}: {error}")
        try:  # read BEFORE stop: the record is the evidence of what must be gone
            owned = read_ownership(control_home / "vm-ownership.json")
        except OwnershipError as error:
            ownership_error = str(error)
        stop_rc = None
        try:
            stopped = tracked_run([str(marsh), "stop"], cwd=project, env=env, capture_output=True, text=True, timeout=300)
            stop_rc = stopped.returncode
            sys.stderr.write(f"marsh stop: rc={stopped.returncode} {stopped.stdout.strip()} {stopped.stderr.strip()[-200:]}\n")
        except subprocess.TimeoutExpired:
            cleanup_errors.append("marsh stop timed out")
        try:
            remaining = sbx_names()
        except Exception as error:
            remaining = None
            cleanup_errors.append(f"sbx ls failed: {type(error).__name__}: {error}")
        cleanup_errors.extend(u for u in UNREAPED if u not in cleanup_errors)  # any group that could not be reaped fails the run
        cleanup_errors = verify_cleanup(run_id=run_id, owned=owned, ownership_error=ownership_error, scope_ran=scope_ran,
                                        stop_returncode=stop_rc, removal_errors=cleanup_errors, remaining=remaining)
        result["foreign_nfx_resources"] = sorted(n for n in (remaining or []) if (n.startswith("nfx-") or n.startswith("marsh-xnfx01-"))
                                                 and n not in owned and not n.startswith(f"nfx-{run_id}-"))
        result["owned_vms"] = sorted(owned)
        result["run_id"] = run_id
        result["scratch"] = str(root)
        result["inventory"] = {"ok": remaining is not None,
                               "owned_remaining": sorted(n for n in (remaining or []) if n in owned)}
        result["cleanup_errors"] = cleanup_errors
        # The lock is verified BEFORE anything is decided about the evidence: a lock that changed under
        # the run makes cleanup untrustworthy, so the scratch (ownership record) is kept.
        lock_problems = guard.verify()
        result["lock_check"] = {"unchanged": not lock_problems, "problems": lock_problems}
        if lock_problems:
            sys.stderr.write(f"VM lock file problem: {lock_problems}\n")
        if cleanup_errors or lock_problems:
            result["ok"] = False
            result["verdict"] = "invalid_negative_control" if args.break_mode else "fail"
            sys.stderr.write(f"{'lock anomaly' if lock_problems else 'cleanup problems'}; scratch kept at {root}\n")
        elif guard.verify():  # re-checked at the moment of deletion
            result["ok"] = False
            result["verdict"] = "invalid_negative_control" if args.break_mode else "fail"
            result["lock_check"] = {"unchanged": False, "problems": guard.verify()}
            sys.stderr.write(f"lock anomaly at deletion time; scratch kept at {root}\n")
        else:
            shutil.rmtree(root, ignore_errors=True)
    print(json.dumps(result, indent=1, sort_keys=True))
    return 0 if result["ok"] and not cleanup_errors and result["lock_check"]["unchanged"] else 1


if __name__ == "__main__":
    sys.exit(main())
