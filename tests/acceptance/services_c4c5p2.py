#!/usr/bin/env python3
"""Live checks for C4 (`services-v1` and the plain `summary`), C5 (git-ignored state in checkpoints)
and P2 (the capture budget: skip and say so), against real sbx.

The product driver: a real `marshd` (the dev install's) and the real `marsh` CLI. It reuses the C12
script's session, daemon launch and cleanup (`services_c12_checkpoints.py`); every VM is removed through
`marsh services rm`, then by exact name after its recorded UUID is verified; daemons are signalled by
exact pid only; the run's files live in one mkdtemp directory under /private/tmp. Test daemons use
MARSH_VM_PREFIX=marsh-xnfx01-. Run under the VM lock (`vmlock.py --shared`).

  normal   the normal build: feature, summary, ignored, budget
  breaks   the test-breaks build: pass 2 and pass 3 of each break cell

Every check prints `PASS name: detail` or `FAIL name: detail`.
"""
from __future__ import annotations

import argparse
import errno
import json
import os
import subprocess
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import services_c12_checkpoints as c12  # noqa: E402
from services_c12_checkpoints import FAILURES, check, expect_break, pool_ids, prod, run, say, wait_until  # noqa: E402

# Each defect has the name of this task and the TEST-PLAN name the harness catalogue uses.
NEW_DAEMON_BREAKS = {"services-feature-off", "ignored-skip", "ignored-follow-symlink", "capture-no-skip",
                     "feature-hidden", "ignored-capture-off", "ignored-follow-outside-symlink", "capture-block-next-prompt"}
c12.KNOWN_DAEMON_BREAKS.update(NEW_DAEMON_BREAKS)
AGENT_MODES = {"slow-capture"}
c12.AGENT_BREAKS.update(AGENT_MODES)

GITIGNORE = ".env*\ndata/\npublic/uploads/\n.next/\nlinked\n"
COMPOSE = """services:
  app:
    image: busybox:1.37
    env_file: .env
    volumes:
      - ./data:/data
      - ./public/uploads:/uploads
      - ./linked:/linked
      - ${SECRET_DIR}:/x
"""


def git(path: Path, *args: str) -> None:
    run(["git", "-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", *args], cwd=path)


def gitproject(sess: c12.Session, name: str) -> Path:
    path = sess.root / name
    path.mkdir()
    (path / "README").write_text("c4c5 project\n")
    (path / ".gitignore").write_text(GITIGNORE)
    (path / "compose.yaml").write_text(COMPOSE)
    git(path, "init", "-q")
    git(path, "add", "-A")
    git(path, "commit", "-qm", "init")
    sess.projects[name] = path
    return path


class Ctx:
    def __init__(self, sess: c12.Session, args):
        self.sess = sess
        self.args = args
        self.run_id = sess.root.name
        self.pA = gitproject(sess, "pA")
        self.pI = gitproject(sess, "pI")
        self.pOff = gitproject(sess, "pOff")
        self.pCap = gitproject(sess, "pCap")
        self.vm = ""
        self.mh = sess.root / "mh"

    def config(self, **keys) -> None:
        """Merge keys into the daemon's config.json (read afresh at every use)."""
        path = self.mh / "config.json"
        doc = json.loads(path.read_text()) if path.exists() else {}
        for key, value in keys.items():
            if value is None:
                doc.pop(key, None)
            else:
                doc[key] = value
        path.write_text(json.dumps(doc))


def status_features(sess: c12.Session) -> list[str]:
    r = sess.cli("status", "--json", cwd=sess.root)
    return json.loads(r.stdout).get("features", []) if r.returncode == 0 else []


def summary_of(sess: c12.Session, name: str) -> str | None:
    doc = sess.svc(name)
    return doc.get("summary") if doc else None


# ------------------------------------------------------------------------------------------ C4


def scratch_without_artifacts(ctx: Ctx) -> Path:
    """A guest artifacts directory of APFS clones of the install's, minus the services engine
    (the daemon refuses symlinked artifacts)."""
    source = ctx.sess.prefix / "libexec" / "marsh"
    target = ctx.sess.root / "libexec-no-engine"
    target.mkdir(exist_ok=True)
    for entry in source.iterdir():
        if entry.name in ("svc-engine-image.json", "svc-engine.tar") or (target / entry.name).exists():
            continue
        subprocess.run(["cp", "-cR", str(entry), str(target / entry.name)], check=True)
    return target


def feature(ctx: Ctx, absent: bool = True) -> None:
    s = ctx.sess
    features = status_features(s)
    check("feature/c1-services-v1-listed", "services-v1" in features, f"features={features}")
    check("feature/c1-other-features-intact", {"checkpoints-v1", "lane-checkpoints-v1", "events-v1"} <= set(features), "")
    if not absent:
        return
    previous = dict(s.env_extra)
    s.relaunch((), {**previous, "MARSH_GUEST_ARTIFACTS": str(scratch_without_artifacts(ctx))})
    features = status_features(s)
    check("feature/c2-absent-without-artifacts", "services-v1" not in features and "checkpoints-v1" in features, f"features={features}")
    line = summary_of(s, "pOff")
    check("feature/c2-summary-says-this-build-cannot", line == "Unavailable: this build of marsh cannot run app services", f"summary={line!r}")
    s.relaunch((), previous)
    check("feature/c3-listed-again-with-artifacts", "services-v1" in status_features(s), "")


INTERNALS = ("vm", "volume", "snapshot", "engine", "sbx", "relay", "marsh:")


def plain(text: str) -> bool:
    words = {w.strip(".,;:()—").lower() for w in text.split()}
    return not any(word in words for word in INTERNALS) and "marsh:" not in text


def summary(ctx: Ctx) -> None:
    s = ctx.sess
    seen: dict[str, str] = {}

    def note(name: str, line: str | None) -> None:
        if line:
            seen[line] = name

    line = summary_of(s, "pA")
    check("summary/off", line == "Off", f"{line!r}")
    s.enable("pA")
    line = summary_of(s, "pA")
    check("summary/on-not-started", line == "On — nothing has been started yet", f"{line!r}")
    doc = s.svc("pA")
    check("summary/reason-kept", "reason" in doc and doc["state"] == "off", f"state={doc['state']} reason={doc['reason']!r}")

    # Starting: poll while `services start` runs.
    starting = threading.Thread(target=lambda: s.cli("services", "start", "--json", cwd=ctx.pA, timeout=1800))
    starting.start()
    while starting.is_alive():
        note("start", summary_of(s, "pA"))
        time.sleep(0.4)
    starting.join()
    doc = s.wait_state("pA", "ready", 120)
    ctx.vm = doc["vm"]
    c12.record_vm(doc["vm"], doc["vm_uuid"])
    check("summary/starting-seen", "Starting" in seen, f"seen while starting: {sorted(seen)}")
    wait_until(lambda: summary_of(s, "pA") == "Ready — no containers running", 20, 1)
    line = summary_of(s, "pA")
    check("summary/ready-no-containers", line == "Ready — no containers running", f"{line!r}")

    # Containers the product's own way: the project shell's endpoint (DOCKER_HOST), so they are in the base workspace.
    r = s.cli("-c", "docker run -d --name c4-1 busybox:1.37 sleep 3600 >/dev/null && docker run -d --name c4-2 busybox:1.37 sleep 3600 >/dev/null", cwd=ctx.pA, timeout=1500)
    if r.returncode == 0:
        wait_until(lambda: summary_of(s, "pA") == "Ready — 2 containers", 30, 1)
        line = summary_of(s, "pA")
        check("summary/ready-2-containers", line == "Ready — 2 containers", f"{line!r}")
    else:
        check("summary/ready-2-containers", False, f"could not start containers from the shell: {(r.stdout + r.stderr)[-200:]}")

    r = s.cli("services", "stop", cwd=ctx.pA, timeout=600)
    doc = s.wait_state("pA", "stopped", 60)
    line = doc.get("summary")
    check("summary/stopped-app-data-kept", r.returncode == 0 and line == "Stopped — app data kept", f"{line!r}")
    check("summary/stopped-reason-kept", doc["state"] == "stopped" and "reason" in doc, "")
    s.cli("services", "start", cwd=ctx.pA, timeout=1800)
    s.wait_state("pA", "ready", 120)

    # Unavailable: a daemon that lost its relay (killed, relaunched) says so until it recovers it.
    s.kill()
    s.launch((), s.env_extra)
    deadline = time.monotonic() + 60
    unavailable = None
    while time.monotonic() < deadline and unavailable is None:
        doc = s.svc("pA") or {}
        if doc.get("state") == "unavailable":
            unavailable = doc
        elif doc.get("state") == "ready" and doc.get("relay", {}).get("state") == "ready":
            break
        time.sleep(0.15)
    if unavailable:
        line = unavailable["summary"]
        check("summary/unavailable-plain", line.startswith("Unavailable: ") and plain(line) and "marsh:" in (unavailable["reason"] or ""),
              f"summary={line!r} reason={unavailable['reason']!r}")
    else:
        say("SKIP", "summary/unavailable-plain", "the relaunched daemon recovered before it could be observed (the table unit test covers every cause)")
    wait_until(lambda: (s.svc("pA") or {}).get("relay", {}).get("state") == "ready", 120, 2)
    for line in sorted(seen):
        check(f"summary/plain/{line}", plain(line), "")


# ------------------------------------------------------------------------------------------ C5


def tree_files(root: Path) -> dict[str, bytes]:
    found = {}
    for path in sorted(root.rglob("*")):
        if path.is_file() and not path.is_symlink():
            found[str(path.relative_to(root))] = path.read_bytes()
    return found


def grep_tree(root: Path, needle: str) -> list[str]:
    result = run(["grep", "-rl", "--", needle, str(root)])
    return result.stdout.split()


def fixture_ignored(ctx: Ctx, project: Path) -> tuple[Path, Path]:
    """Reset the project's ignored state to the C1 values; (canary file, canary dir)."""
    canary = ctx.sess.root / "canary"
    (canary / "dir").mkdir(parents=True, exist_ok=True)
    (canary / "outside").write_text(f"U-{ctx.run_id}\n")
    (canary / "dir" / "f").write_text(f"U2-{ctx.run_id}\n")
    for name, text in ((".env", "FOO=c1\n"), (".env.local", "BAR=c1\n")):
        (project / name).write_text(text)
    for path, data in (("data/app.sqlite", b"row c1"), ("public/uploads/a.bin", b"A" * 4096), (".next/cache/x", b"cache")):
        (project / path).parent.mkdir(parents=True, exist_ok=True)
        (project / path).write_bytes(data)
    for link, target in ((".env.evil", canary / "outside"), ("linked", canary / "dir")):
        if os.path.lexists(project / link):
            (project / link).unlink()
        (project / link).symlink_to(target)
    fifo = project / ".env.fifo"
    if not fifo.exists():
        os.mkfifo(fifo)
    return canary / "outside", canary / "dir"


def ignored(ctx: Ctx) -> None:
    s = ctx.sess
    project = ctx.pI
    s.enable("pI")
    fixture_ignored(ctx, project)

    # A writer that notices a reader on the FIFO: the daemon must never open it.
    reader_seen = []
    stop = threading.Event()

    def watch() -> None:
        while not stop.is_set():
            try:
                fd = os.open(project / ".env.fifo", os.O_WRONLY | os.O_NONBLOCK)
                reader_seen.append(time.monotonic())
                os.close(fd)
            except OSError as error:
                if error.errno != errno.ENXIO:
                    reader_seen.append(error.errno)
            time.sleep(0.02)

    watcher = threading.Thread(target=watch, daemon=True)
    watcher.start()
    rc, doc, wall, text = s.ckpt("pI", "c1")
    stop.set()
    watcher.join()
    check("ignored/checkpoint-succeeds", rc == 0 and doc is not None, text[:160])
    if doc is None:
        return
    record = doc.get("ignored") or {}
    saved = tree_files(Path(doc["dir"]) / "ignored")
    expected = {".env": b"FOO=c1\n", ".env.local": b"BAR=c1\n", "data/app.sqlite": b"row c1", "public/uploads/a.bin": b"A" * 4096}
    check("ignored/c1-captured", saved == expected and record.get("files") == 4 and record.get("bytes") == sum(map(len, expected.values())),
          f"saved={sorted(saved)} record files={record.get('files')} bytes={record.get('bytes')}")
    check("ignored/c1-unreferenced-absent", ".next/cache/x" not in saved, "")
    said = {item["path"]: item["reason"] for item in record.get("skipped", [])}
    check("ignored/c1-skipped-said", all(k in said for k in (".env.evil", "linked", ".env.fifo")) and "${SECRET_DIR}" in said,
          json.dumps(said)[:300])
    leaks = grep_tree(Path(doc["dir"]) / "ignored", f"U-{ctx.run_id}") + grep_tree(Path(doc["dir"]) / "ignored", f"U2-{ctx.run_id}")
    check("ignored/no-escape", not leaks and not reader_seen, f"canary bytes under ignored/: {leaks}; fifo readers seen: {len(reader_seen)}")
    check("ignored/c4-services-null-without-a-vm", doc.get("services") is None, f"services={doc.get('services')}")

    # A lane of the checkpoint gets the checkpoint's values, not today's.
    (project / ".env").write_text("FOO=c2\n")
    (project / "public/uploads/a.bin").unlink()
    r = s.cli("split", "--open", "--from", doc["checkpoint"], "a", "b", cwd=project)
    opened = json.loads(r.stdout.strip().splitlines()[-1]) if r.returncode == 0 else {}
    lanes = opened.get("lanes", {})
    check("ignored/split-opens", set(lanes) == {"a", "b"}, (r.stdout + r.stderr)[:200])
    if lanes:
        for label in ("a", "b"):
            lane = Path(lanes[label])
            got = {
                ".env": (lane / ".env").read_text() if (lane / ".env").is_file() else None,
                "a.bin": (lane / "public/uploads/a.bin").is_file(),
                "sqlite": (lane / "data/app.sqlite").read_bytes() if (lane / "data/app.sqlite").is_file() else None,
                "next": (lane / ".next").exists(),
            }
            check(f"ignored/lane-has-checkpoint-values/{label}", got == {".env": "FOO=c1\n", "a.bin": True, "sqlite": b"row c1", "next": False}, str(got))
            escapes = grep_tree(lane, f"U-{ctx.run_id}") + grep_tree(lane, f"U2-{ctx.run_id}")
            check(f"ignored/lane-no-escape/{label}", not escapes and not (lane / ".env.evil").exists() and not (lane / "linked").exists(),
                  f"{escapes}")
        check("ignored/root-keeps-today", (project / ".env").read_text() == "FOO=c2\n", "")
        # The lane's own tracked edit is all `apply` would carry: the copied ignored files are not changes.
        (Path(lanes["a"]) / "README").write_text("edited in lane a\n")
        split_id = opened["id"]
        s.cli("split", "close", split_id, "--json", cwd=project)
        out = project / ".marsh" / "split" / split_id / "out" / "a" / "files"
        files = out.read_text() if out.exists() else ""
        check("ignored/apply-carries-only-the-tracked-edit", files.strip().endswith("README") and ".env" not in files.replace("README", ""),
              f"changed files: {files.strip()!r}")

    # Services off: nothing is saved (VM-01 c4, CAP-10 c3).
    s.cli("config", "services", "off", cwd=ctx.pOff)
    rc, off, _, text = s.ckpt("pOff", "off")
    check("ignored/c3-null-when-services-are-off", rc == 0 and off and off.get("ignored") is None and not (Path(off["dir"]) / "ignored").exists(),
          f"ignored={off and off.get('ignored')}")
    cap(ctx)


def cap(ctx: Ctx) -> None:
    """The bound at cap-1, cap and cap+1 bytes (CAP-10 c1): over the cap is said and the checkpoint still succeeds."""
    s = ctx.sess
    s.enable("pCap")
    limit = 100_000
    ctx.config(services_ignored_max_bytes=limit)
    for size, want_files, label in ((limit - 1, 1, "cap-1"), (limit, 1, "cap"), (limit + 1, 0, "cap+1")):
        (ctx.pCap / ".env.pad").write_bytes(b"x" * size)
        rc, doc, _, text = s.ckpt("pCap", label)
        record = (doc or {}).get("ignored") or {}
        skipped = [item for item in record.get("skipped", []) if item["path"] == ".env.pad"]
        ok = rc == 0 and record.get("files") == want_files and bool(skipped) == (want_files == 0)
        if want_files == 0:
            ok = ok and "size limit" in skipped[0]["reason"]
        check(f"ignored/c1-{label}", ok, f"rc={rc} files={record.get('files')} bytes={record.get('bytes')} skipped={skipped}")
    ctx.config(services_ignored_max_bytes=None)


# ------------------------------------------------------------------------------------------ P2


GAP_WORDS = "held up your next step"


def budget(ctx: Ctx) -> None:
    s = ctx.sess
    vm = ctx.vm
    before = pool_ids(vm)
    rc, doc, wall, text = s.ckpt("pA", "p2-normal")
    services = (doc or {}).get("services") or {}
    check("budget/c0-default-budget-captures", rc == 0 and services.get("snapshot") and not any(GAP_WORDS in g for g in services.get("gaps", [])),
          f"rc={rc} snapshot={services.get('snapshot')} gaps={services.get('gaps')} wall={wall:.2f}s")
    after_normal = pool_ids(vm)
    # A budget (100 ms) the steps before the request already use: nothing is sent.
    ctx.config(services_capture_budget_ms=100)
    rc, doc, wall, text = s.ckpt("pA", "p2-skip")
    services = (doc or {}).get("services") or {}
    gaps = services.get("gaps", [])
    check("budget/c1-skipped", rc == 0 and not services.get("snapshot") and any(GAP_WORDS in g for g in gaps),
          f"rc={rc} snapshot={services.get('snapshot')} gaps={gaps} wall={wall:.2f}s")
    check("budget/c2-gap-in-plain-words", bool(gaps) and all(plain(g) for g in gaps), f"{gaps}")
    check("budget/c2-files-and-ignored-still-saved", bool(doc) and doc["bytes"] > 0 and (Path(doc["dir"]) / "tree" / "README").is_file()
          and doc.get("ignored") is not None, "")
    check("budget/c3-no-request-sent", pool_ids(vm) == after_normal, f"pool before {len(after_normal)} after {len(pool_ids(vm))}")
    check("budget/c3-nothing-left-paused", c12.paused(vm) == [], "")
    ctx.config(services_capture_budget_ms=None)
    rc, doc, wall, text = s.ckpt("pA", "p2-after")
    services = (doc or {}).get("services") or {}
    check("budget/c4-next-checkpoint-captures-again", rc == 0 and services.get("snapshot"), f"snapshot={services.get('snapshot')} gaps={services.get('gaps')}")
    del before


def budget_slow(ctx: Ctx) -> None:
    """The agent's `slow-capture` (1.5 s before it freezes) against a 1 s budget: the wait ends with the budget."""
    s = ctx.sess
    vm = ctx.vm
    ctx.config(services_capture_budget_ms=1000)
    baseline = set(pool_ids(vm))
    rc, doc, wall, text = s.ckpt("pA", "p2-slow")
    services = (doc or {}).get("services") or {}
    gaps = services.get("gaps", [])
    check("budget/slow-skipped", rc == 0 and not services.get("snapshot") and any(GAP_WORDS in g for g in gaps) and wall < 1.4,
          f"rc={rc} snapshot={services.get('snapshot')} gaps={gaps} wall={wall:.2f}s (must be < 1.4 s)")
    check("budget/slow-files-saved", bool(doc) and (Path(doc["dir"]) / "tree" / "README").is_file(), "")
    # The agent finishes the capture the daemon stopped waiting for; nothing names it, so it is collected.
    gone = wait_until(lambda: set(pool_ids(vm)) <= baseline | referenced(ctx), 90, 3)
    check("budget/slow-late-snapshot-collected", bool(gone), f"pool {sorted(pool_ids(vm))}")
    check("budget/slow-nothing-left-paused", c12.paused(vm) == [], "")
    ctx.config(services_capture_budget_ms=None)


def referenced(ctx: Ctx) -> set[str]:
    refs: set[str] = set()
    for name in ctx.sess.projects:
        for item in ctx.sess.listed(name):
            services = item.get("services") or {}
            refs.update(x for x in (services.get("snapshot"), services.get("reused_from")) if x)
    return refs


# ------------------------------------------------------------------------------------------ driver


def breaks_session(ctx: Ctx, args) -> None:
    s = ctx.sess

    def ready(breaks: tuple[str, ...]) -> None:
        s.relaunch(breaks, s.env_extra)
        if (s.svc("pA") or {}).get("state") == "stopped":
            s.cli("services", "start", cwd=ctx.pA, timeout=1800)
        wait_until(lambda: (s.svc("pA") or {}).get("relay", {}).get("state") == "ready", 120, 2)
        got = sorted((s.svc("pA") or {}).get("test_breaks_agent", []))
        want = sorted(m for m in breaks if m in AGENT_MODES)
        check("marshd/agent-modes-attested", got == want, f"agent reports {got}, expected {want}")

    cells = [
        ("services-feature-off", "feature/c1-services-v1-listed", lambda c: feature(c, absent=False), ("services-feature-off",), ()),
        ("ignored-skip", "ignored/c1-captured", ignored, ("ignored-skip",),
         ("ignored/c1-skipped-said", "ignored/lane-has-checkpoint-values/a", "ignored/lane-has-checkpoint-values/b",
          "ignored/c1-cap-1", "ignored/c1-cap", "ignored/c1-cap+1")),
        ("ignored-follow-symlink", "ignored/no-escape", ignored, ("ignored-follow-symlink",),
         ("ignored/c1-captured", "ignored/c1-skipped-said", "ignored/lane-no-escape/a", "ignored/lane-no-escape/b",
          "ignored/lane-has-checkpoint-values/a", "ignored/lane-has-checkpoint-values/b", "ignored/c1-unreferenced-absent")),
        ("capture-no-skip", "budget/c1-skipped", budget, ("capture-no-skip",), ("budget/c3-no-request-sent", "budget/c2-gap-in-plain-words")),
        ("capture-no-skip/slow-capture", "budget/slow-skipped", budget_slow, ("slow-capture", "capture-no-skip"),
         ("budget/slow-files-saved",)),
        ("feature-hidden", "feature/c1-services-v1-listed", lambda c: feature(c, absent=False), ("feature-hidden",), ()),
        ("ignored-capture-off", "ignored/c1-captured", ignored, ("ignored-capture-off",),
         ("ignored/c1-skipped-said", "ignored/lane-has-checkpoint-values/a", "ignored/lane-has-checkpoint-values/b",
          "ignored/c1-cap-1", "ignored/c1-cap", "ignored/c1-cap+1")),
        ("ignored-follow-outside-symlink", "ignored/no-escape", ignored, ("ignored-follow-outside-symlink",),
         ("ignored/c1-captured", "ignored/c1-skipped-said", "ignored/lane-no-escape/a", "ignored/lane-no-escape/b",
          "ignored/lane-has-checkpoint-values/a", "ignored/lane-has-checkpoint-values/b", "ignored/c1-unreferenced-absent")),
        ("capture-block-next-prompt", "budget/c1-skipped", budget, ("capture-block-next-prompt",),
         ("budget/c3-no-request-sent", "budget/c2-gap-in-plain-words")),
        ("capture-block-next-prompt/slow-capture", "budget/slow-skipped", budget_slow, ("slow-capture", "capture-block-next-prompt"),
         ("budget/slow-files-saved",)),
    ]
    only = set(filter(None, args.cells.split(",")))
    for label, failing, scenario, modes, allowed in cells:
        if only and label not in only:
            continue
        # Pass 2: the build with the faults and holds the cell needs, but no defect.
        faults = tuple(m for m in modes if m in AGENT_MODES)
        before = len(FAILURES)
        say("INFO", f"pass2/{label}", f"daemon modes {list(faults)}")
        ready(faults)
        try:
            scenario(ctx)
        except Exception as error:  # noqa: BLE001
            import traceback
            traceback.print_exc()
            say("FAIL", f"pass2/{label}/exception", repr(error)[:200])
        check(f"pass2/{label}-passes-without-the-defect", len(FAILURES) == before, f"{len(FAILURES) - before} failed checks")
        before = list(FAILURES)
        say("INFO", f"break/{label}", f"cell: daemon modes {list(modes)}; expect {failing} to fail")
        ready(modes)
        try:
            scenario(ctx)
        except Exception as error:  # noqa: BLE001
            import traceback
            traceback.print_exc()
            say("FAIL", f"break/{label}/exception", repr(error)[:200])
        expect_break(label, failing, before, allowed)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--session", choices=["normal", "breaks"], default="normal")
    ap.add_argument("--dev-normal", required=True)
    ap.add_argument("--dev-breaks", required=True)
    ap.add_argument("--only", default="feature,summary,ignored,budget")
    ap.add_argument("--cells", default="")
    ap.add_argument("--pool", default="3g")
    ap.add_argument("--fixtures", default=str(Path(__file__).resolve().parent / "services" / "fixtures"))
    args = ap.parse_args()
    sess = None
    ctx = None
    try:
        sess = c12.Session(args, args.session)
        sess.reserve_bytes = 1 << 30
        ctx = Ctx(sess, args)
        sess.launch()
        if args.session == "normal":
            only = args.only.split(",")
            if "feature" in only:
                feature(ctx)
            if "ignored" in only:
                ignored(ctx)
            if "summary" in only or "budget" in only:
                summary(ctx)
            if "budget" in only:
                budget(ctx)
        else:
            sess.enable("pA")
            summary_vm_up(ctx)
            breaks_session(ctx, args)
    except Exception as error:  # noqa: BLE001
        import traceback
        traceback.print_exc()
        say("FAIL", "run/exception", repr(error)[:300])
    finally:
        cleaned = sess.finish() if sess else True
        say("INFO" if cleaned else "FAIL", "cleanup/complete", str(cleaned))
    print(f"{'FAIL' if FAILURES else 'OK'}: {len(FAILURES)} failed checks {FAILURES}")
    return 1 if FAILURES else 0


def summary_vm_up(ctx: Ctx) -> None:
    """The breaks session needs a ready services VM."""
    s = ctx.sess
    ctx.vm = s.start_vm("pA")


if __name__ == "__main__":
    sys.exit(main())
