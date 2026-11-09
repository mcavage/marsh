#!/usr/bin/env python3
"""LD evidence: local data (git-ignored folder files + Docker) comes along on `split --open --from`, with the one line.

    vmlock.py --run ld --shared -- python3 tests/acceptance/services_ld_localdata.py \
        --prefix ~/.marsh-dev-LD --fixtures tests/acceptance/services/fixtures --images ~/dev/marsh-fork/target/nfx/images

Reuses the C3 scope machinery (`services_c3_lanes.Lab`): a real daemon in its own MARSH_HOME, the `nfx` fixture Kit,
`MARSH_VM_PREFIX=marsh-xnfx01-`, cleanup by exact names only. One scenario:

1. A git project with a compose Postgres and an ignored `.wrangler/state` file; checkpoint.
2. Change the file and the database row; `split --open --from`.
3. The lane has the OLD file and the OLD row, the line says so, a job in the lane sees the Docker section in
   context.md, and `docker compose version` works there.
4. A fresh project with no config is `enabled: true` and has no VM until a job runs `docker ps`.
5. A project with Docker turned off and an ignored file: the lane still gets the file, a restore entry and a line.
"""
from __future__ import annotations

import argparse
import json
import pathlib
import secrets
import shutil
import subprocess
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import services_c3_lanes as c3  # noqa: E402

check, jline, pg = c3.check, c3.jline, c3.pg
GIT = ["git", "-c", "user.name=nfx", "-c", "user.email=nfx@example.invalid", "-c", "commit.gpgsign=false"]


def scenario(args, lab: c3.Lab) -> dict:
    c: dict = {}
    lab.use([])
    project = lab.setup_stack(load=0)
    state = project / ".wrangler/state/v3/d1"
    (project / ".gitignore").write_text(".wrangler/\n")
    state.mkdir(parents=True)
    (state / "db.sqlite").write_text("OLD-STATE")
    lab.base_job(pg("insert into markers values ('ld-old')") + " >/dev/null; echo ok")
    c1 = lab.ckpt()
    ignored = c1.get("ignored") or {}
    c["checkpoint_saved_the_ignored_file"] = check(
        ".wrangler" in (ignored.get("roots") or []), "ignored.roots names .wrangler", ignored)
    (state / "db.sqlite").write_text("NEW-STATE")
    lab.base_job(pg("insert into markers values ('ld-new')") + " >/dev/null; echo ok")

    rc, doc, err, secs = lab.open_split(c1["checkpoint"], ["a"])
    lane = (doc.get("restore") or {}).get("a", {})
    print("LINE:", lane.get("line"), flush=True)
    print("STDERR:", err[-400:], flush=True)
    lane_dir = lab.lane_paths["a"]
    old_file = (lane_dir / ".wrangler/state/v3/d1/db.sqlite").read_text() if (lane_dir / ".wrangler/state/v3/d1/db.sqlite").exists() else None
    c["lane_has_old_file"] = check(old_file == "OLD-STATE", "the lane's .wrangler/state file is the checkpoint's", old_file)
    lab.wait_ports = True
    rows = lab.lane_job(lane_dir, pg("select string_agg(name, ',' order by name) from markers where name like 'ld-%'"))[1].strip()
    c["lane_has_old_row"] = check(rows == "ld-old", "the lane's database has the old row and not the new one", rows)
    line = lane.get("line", "")
    c["line_says_so"] = check(rc == 0 and lane.get("ok") is True and "Local data restored:" in line and ".wrangler" in line
                              and "(Docker)" in line and "VM" not in line, "ok:true and the line names both places", line)
    c["folder_in_json"] = check(bool((lane.get("folder") or {}).get("copied")), "restore.folder.copied", lane.get("folder"))
    out = lab.lane_job(lane_dir, "grep -c '^## Docker' /run/marsh/context.md; grep -c 'DOCKER_HOST' /run/marsh/context.md; "
                                 "docker compose version")
    c["context_and_compose"] = check(out[0] == 0 and out[1].split()[0] == "1" and "Docker Compose version" in out[1],
                                     "the lane job sees the Docker section and docker compose works", out[1][-300:] + out[2][-200:])
    lab.close_split(doc.get("id"))

    # Docker is on by default (DECISIONS 31): a fresh project with no config is enabled, and has no VM until a job
    # actually uses Docker.
    fresh = lab.root / "fresh-project"
    fresh.mkdir()
    for argv in (["git", "init", "-q", "-b", "main"], GIT + ["commit", "-q", "--allow-empty", "-m", "stage"]):
        subprocess.run(argv, cwd=fresh, check=True, capture_output=True)
    status = lab.services_status(fresh)
    entry = lab.project_entry(fresh)
    shown = lab.run(["config", "docker"], cwd=fresh).stdout.strip()
    c["fresh_is_enabled_with_no_vm"] = check(
        entry.get("enabled") is True and not entry.get("vm") and shown == "on" and str(fresh.resolve()) in status["enabled_projects"]
        and status.get("default_enabled") is True,
        "a project with no config: enabled true, `config docker` on, no VM", {"entry": entry, "shown": shown, "default": status.get("default_enabled")})
    code, out, err = lab.job("ls /run/marsh/docker.sock; echo DH=$DOCKER_HOST", cwd=fresh)
    entry = lab.project_entry(fresh)
    c["a_job_that_does_not_use_docker_creates_no_vm"] = check(
        code == 0 and "docker.sock" in out and not entry.get("vm"),
        "the job has the endpoint, and still no VM exists", {"out": out[-200:], "err": err[-200:], "vm": entry.get("vm"), "state": entry.get("state")})
    code, out, err = lab.job("docker ps >/dev/null 2>&1; echo RC=$?", cwd=fresh, timeout=1800)
    entry = lab.project_entry(fresh)
    c["docker_ps_creates_the_vm"] = check("RC=0" in out and bool(entry.get("vm")) and entry.get("state") == "ready",
                                          "`docker ps` in a job creates and readies the VM", {"out": out[-200:], "err": err[-300:], "entry": {k: entry.get(k) for k in ("vm", "state", "enabled")}})
    if entry.get("vm"):
        lab.vms.add(entry["vm"])
    off_cfg = lab.run(["config", "docker", "off"], cwd=fresh).stdout.strip()
    status = lab.services_status(fresh)
    c["off_is_effective"] = check(off_cfg == "off" and lab.project_entry(fresh).get("enabled") is False
                                  and str(fresh.resolve()) not in status["enabled_projects"] and str(fresh.resolve()) in status.get("disabled_projects", []),
                                  "config docker off: enabled false and listed as disabled", {"cfg": off_cfg, "disabled": status.get("disabled_projects")})

    # Docker off: a project that turned it off.
    off = lab.root / "off-project"
    (off / "web").mkdir(parents=True)
    (off / ".gitignore").write_text(".wrangler/\n.env\nnode_modules/\n")
    (off / "web/index.txt").write_text("hi")
    for argv in (["git", "init", "-q", "-b", "main"], ["git", "add", "-A"], GIT + ["commit", "-q", "-m", "stage"]):
        subprocess.run(argv, cwd=off, check=True, capture_output=True)
    (off / ".env").write_text("K=1")
    (off / ".wrangler/state").mkdir(parents=True)
    (off / ".wrangler/state/x").write_text("X")
    (off / "node_modules/p").mkdir(parents=True)
    (off / "node_modules/p/i.js").write_text("js")
    saved = lab.project_dir
    lab.project_dir = off
    try:
        lab.run(["config", "docker", "off"], cwd=off)
        done = lab.run(["checkpoint", "--json", "--label", "off"], cwd=off, timeout=300)
        ck = json.loads(done.stdout)
        rc, doc, err, _ = lab.open_split(ck["checkpoint"], ["b"], label="off")
        lane = (doc.get("restore") or {}).get("b", {})
        lane_dir = lab.lane_paths["b"]
        skipped = [s["path"] for s in (lane.get("folder") or {}).get("skipped", [])]
        c["docker_off_folder_data"] = check(
            rc == 0 and (lane_dir / ".env").read_text() == "K=1" and (lane_dir / ".wrangler/state/x").exists()
            and not (lane_dir / "node_modules").exists() and "node_modules" in skipped
            and lane.get("line", "").startswith("Local data restored: ") and "Docker" not in lane.get("line", ""),
            "Docker off: the lane has .env and .wrangler/state, not node_modules, with a restore entry and line",
            {"line": lane.get("line"), "skipped": skipped, "stderr": err[-200:]})
        lab.close_split(doc.get("id"))
    finally:
        lab.project_dir = saved
    return c


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--prefix", required=True)
    parser.add_argument("--breaks-prefix")
    parser.add_argument("--fixtures", required=True)
    parser.add_argument("--sbx", default="sbx")
    parser.add_argument("--images")
    parser.add_argument("--keep", action="store_true")
    args = parser.parse_args()
    args.run = secrets.token_hex(3)
    args.prefix = str(pathlib.Path(args.prefix).expanduser())
    args.fixtures = str(pathlib.Path(args.fixtures).expanduser().resolve())
    lab = c3.Lab(args, "ldlocal", [])
    lab.project_dir = lab.project
    result: dict = {}
    crashed = None
    try:
        result = scenario(args, lab)
    except BaseException as error:  # noqa: BLE001
        import traceback
        crashed = f"{type(error).__name__}: {error}"
        traceback.print_exc()
    finally:
        problems = lab.cleanup()
    ok = crashed is None and not problems and all(x["ok"] for x in result.values())
    jline(scenario="ld-localdata", run=args.run, ok=ok, crashed=crashed, cleanup_errors=problems, checks=result, scratch=str(lab.root))
    if ok and not args.keep:
        shutil.rmtree(lab.root, ignore_errors=True)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
