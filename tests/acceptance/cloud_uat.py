#!/usr/bin/env python3
"""Opt-in live Docker Cloud acceptance for `cloud { ... }` / `marsh cloud`.

Real installed product, real stock `sbx`, real Docker Cloud sandboxes (billed;
needs a signed-in account with cloud sandbox access). Not part of `make regress`.
Covers docs/design/cloud-placement.md s10 C02-C06, C09, C11, C12 for the
session-origin slice: exact streams and status in a pipeline, automatic delta
return before the next command, local edits to unchanged paths surviving, a
one-branch split, two fanout placements, Ctrl-C with confirmed partial return,
protected/link entries, and verified cleanup of every owned sandbox.

    python3 tests/acceptance/cloud_uat.py --marsh PREFIX/bin/marsh --kit KIT \
        --evidence DIR --source-revision REV --source-tree . --i-accept-cloud-charges
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import signal
import subprocess
import time

from harness import ProductScope, stable_process_identity
from provenance import candidate_arguments, stock_baseline

CLOUD_TIMEOUT = 600


class CloudAcceptance(ProductScope):
    EVIDENCE = "cloud.json"
    ROOT_PREFIX = "marsh-cloud-"

    def cloud_shell(self, script: str, *, stdin: bytes = b"", check: bool = True):
        result = self.run([self.marsh, "-c", script], stdin=stdin, timeout=CLOUD_TIMEOUT, check=False)
        if check and result.returncode != 0:
            raise AssertionError(
                f"{script!r} exited {result.returncode}: {result.stderr.decode(errors='replace')}"
            )
        return result

    def check(self, name: str, condition: bool, detail: object) -> None:
        self.records.append({"check": name, "passed": bool(condition)})
        if not condition:
            raise AssertionError(f"{name}: {detail!r}")

    def git(self, *arguments: str) -> None:
        subprocess.run(["git", "-C", str(self.project), *arguments], check=True,
                       capture_output=True, env={**os.environ, "GIT_CONFIG_GLOBAL": "/dev/null",
                                                 "GIT_CONFIG_NOSYSTEM": "1"})

    def run_all(self) -> None:
        self.stock_before = stock_baseline(self.sbx)
        self.source["stock_before"] = self.stock_before
        self.document("status", "--json")
        p = self.project
        self.git("init", "-q")
        (p / ".gitignore").write_text("secret.env\n*.log\n")
        (p / "secret.env").write_text("TOKEN=local-only\n")
        (p / "keep.txt").write_text("keep\n")
        (p / "gone.txt").write_text("gone\n")
        (p / "tool.sh").write_text("#!/bin/sh\necho tool\n")
        (p / "busy").mkdir()
        (p / "busy" / "local.txt").write_text("local\n")
        self.git("-c", "user.email=a@b", "-c", "user.name=t", "add", "keep.txt", ".gitignore")
        self.git("-c", "user.email=a@b", "-c", "user.name=t", "commit", "-qm", "init")
        self.git("remote", "add", "origin", "https://user:SECRET@example.com/x.git")
        git_before = sorted(str(path.relative_to(p)) for path in (p / ".git").rglob("*"))

        # C02: streams, stderr, status, pipefail, ordering inside a pipeline.
        piped = self.cloud_shell(
            "set -o pipefail; printf 'b\\na\\n' | cloud { sort; printf 'to-stderr' >&2; exit 7; } "
            "| tr a-z A-Z; echo \"status=$? pipe=${PIPESTATUS[*]}\"",
        )
        self.check("C02 pipeline stdout", piped.stdout == b"A\nB\nstatus=7 pipe=0 7 0\n", piped.stdout)
        self.check("C02 separate stderr", b"to-stderr" in piped.stderr, piped.stderr)

        raw = self.run([self.marsh, "cloud", "--", "sh", "-c", 'cat; printf "%s" "$1"; exit 23', "sh",
                        os.fsdecode(b"\xff")],
                       stdin=b"in\x00put\xff", timeout=CLOUD_TIMEOUT, check=False)
        self.check("C02 byte streams, non-UTF-8 argv, exit", (raw.returncode, raw.stdout) ==
                   (23, b"in\x00put\xff\xff"), (raw.returncode, raw.stdout, raw.stderr[-400:]))

        # C02: a closed downstream pipe cancels the node; it settles (and
        # writes back) before the shell moves on, reported like SIGPIPE.
        closed = self.cloud_shell(
            "cloud { trap 'echo stopped > stopped.txt; exit 1' INT; while :; do echo y; done; } | head -2; "
            "echo \"pipe=${PIPESTATUS[*]}\"; cat stopped.txt",
        )
        self.check("C02 downstream close", closed.stdout == b"y\ny\npipe=141 0\nstopped\n",
                   (closed.stdout, closed.stderr[-400:]))

        # C02: input far larger than the credit window streams through to EOF.
        big = os.urandom(3 * 1024 * 1024 + 7)
        counted = self.run([self.marsh, "cloud", "--", "sh", "-c", "cat > /tmp/in; wc -c < /tmp/in; sha256sum < /tmp/in"], stdin=big,
                           timeout=CLOUD_TIMEOUT, check=False)
        import hashlib
        self.check("C02 large stdin to EOF", (counted.returncode, counted.stdout.split()[:2]) ==
                   (0, [str(len(big)).encode(), hashlib.sha256(big).hexdigest().encode()]),
                   (counted.stdout, counted.stderr[-400:]))

        # C02: output far beyond the stock channel's retention reaches a slow
        # reader intact (the stock channel alone drops it).
        slow = self.cloud_shell("cloud { head -c 16M /dev/zero; } | (sleep 20; wc -c)")
        self.check("C02 large output to a slow reader", slow.stdout.strip() == b"16777216",
                   (slow.stdout, slow.stderr[-400:]))

        # C03: create/modify/delete/mode/dirs return before the next command;
        # a nonzero exit still returns; ignored inputs stay local; generated
        # ignored outputs and symlinks return as entries.
        returned = self.cloud_shell(
            "cloud { test ! -e secret.env || exit 99; echo new > made.txt; echo remote > keep.txt; "
            "rm gone.txt; chmod +x tool.sh; mkdir -p deep/er; echo out > build.log; "
            "ln -s /etc etc-link; mkdir -p .git/hooks; echo evil > .git/hooks/pre-commit; false; }; "
            "echo \"after=$?\"; cat made.txt",
        )
        self.check("C03 status and visibility", returned.stdout == b"after=1\nnew\n", returned.stdout)
        self.check("C03 modified", (p / "keep.txt").read_text() == "remote\n", None)
        self.check("C03 deleted", not (p / "gone.txt").exists(), None)
        self.check("C03 mode", os.access(p / "tool.sh", os.X_OK), None)
        self.check("C03 directories", (p / "deep" / "er").is_dir(), None)
        self.check("C03 ignored input stayed local", (p / "secret.env").read_text() == "TOKEN=local-only\n", None)
        self.check("C03 generated ignored output", (p / "build.log").read_text() == "out\n", None)
        self.check("C11 symlink entry not followed", os.readlink(p / "etc-link") == "/etc", None)
        git_after = sorted(str(path.relative_to(p)) for path in (p / ".git").rglob("*"))
        self.check("C11 caller .git untouched", git_after == git_before, set(git_after) ^ set(git_before))
        self.check("C03 summary on stderr", b"wrote back" in returned.stderr, returned.stderr[-400:])

        # Stop before capture: a writer that left the process group
        # (setsid) is killed; the workload has no sudo or Docker; a program
        # that cannot start is an ordinary 127 with verified cleanup; an
        # ignored cwd is created remotely.
        (p / "build").mkdir()
        (p / ".gitignore").write_text("secret.env\n*.log\nbuild/\n")
        bounded = self.cloud_shell(
            "cd build && cloud { pwd; id -un; sudo -n true 2>/dev/null && echo SUDO; "
            "docker ps >/dev/null 2>&1 && echo DOCKER; "
            "curl -sS -o /dev/null -w '%{http_code}\\n' https://github.com; "
            "setsid sh -c 'sleep 4; echo late > ../escaped.txt' </dev/null >/dev/null 2>&1 & echo started; }; "
            "sleep 6; test -e ../escaped.txt && echo ESCAPED; true",
        )
        self.check("stop/identity", bounded.stdout == f"{p}/build\nmarsh-run\n200\nstarted\n".encode(),
                   (bounded.stdout, bounded.stderr[-400:]))
        missing = self.run([self.marsh, "cloud", "--", "no-such-program-marsh"], timeout=CLOUD_TIMEOUT,
                           check=False)
        self.check("failed start is 127", missing.returncode == 127, missing.stderr[-400:])

        # Git: a private, hook-free copy without remotes; remote commits stay
        # remote. Options select the sandbox shape.
        gitted = self.cloud_shell(
            "echo edited >> keep.txt; cloud --cpus 1 --ttl 10m { nproc; git status --porcelain keep.txt; "
            "git log --oneline | wc -l; git remote | wc -l; test -e .git/hooks && echo HOOKS; "
            "git -c user.email=r@r -c user.name=r commit -qam remote && echo committed; }; "
            "git log --oneline | wc -l",
        )
        self.check("git copy", gitted.stdout.split() == [b"1", b"M", b"keep.txt", b"1", b"0", b"committed", b"1"],
                   (gitted.stdout, gitted.stderr[-400:]))

        # C04: an overlapping local edit loses; an unrelated local edit survives.
        process = subprocess.Popen(
            [self.marsh, "cloud", "-c", "echo ready; read line; echo overlap-remote > keep.txt"],
            cwd=p, env=self.environment, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        assert process.stdout and process.stdin
        ready = process.stdout.readline()
        (p / "keep.txt").write_text("local overlap\n")
        (p / "busy" / "local.txt").write_text("local only edit\n")
        process.stdin.write(b"go\n")
        process.stdin.close()
        process.wait(timeout=CLOUD_TIMEOUT)
        self.check("C04 ran", (ready, process.returncode) == (b"ready\n", 0), process.stderr.read()[-400:])
        self.check("C04 overlap last writer", (p / "keep.txt").read_text() == "overlap-remote\n", None)
        self.check("C04 unrelated edit survives", (p / "busy" / "local.txt").read_text() == "local only edit\n", None)

        # C05: a one-branch split keeps cloud edits in its fork.
        split = self.cloud_shell("split { fix: cloud { echo fixed > keep.txt; } } | join")
        self.check("C05 source unchanged", (p / "keep.txt").read_text() == "overlap-remote\n", split.stdout)
        self.check("C05 patch", b"+fixed" in split.stdout and b"keep.txt" in split.stdout, split.stdout)

        # C07: a split inside a cloud block forks the block's current files
        # in the sandbox, keeps siblings private, joins there, and returns
        # only the block's own changes.
        inner = self.cloud_shell(
            "cloud { echo base > inner.txt; split { a: sed -i s/base/BASE/ inner.txt; b: cat inner.txt; "
            "echo B >> inner.txt } | join; echo \"split=$?\"; }"
        )
        # `a` is a same-size edit made right after the fork (coarse Linux
        # file clocks must not hide it).
        self.check("C07 split in cloud", b"+BASE" in inner.stdout and b"+B" in inner.stdout
                   and b"split=0" in inner.stdout and b"== b (exited 0, cloud) ==\nbase\n" in inner.stdout,
                   (inner.stdout[-1500:], inner.stderr[-400:]))
        self.check("C07 only the block's changes return", (p / "inner.txt").read_text() == "base\n", None)

        self.kits()

        # C06: two placements in a fanout return to the shared destination.
        fanout = self.cloud_shell(
            "fanout { a: cloud { echo A > fan-a.txt; }; b: cloud { echo B > fan-b.txt; } } | collect"
        )
        self.check("C06 both returned", (p / "fan-a.txt").read_text() == "A\n"
                   and (p / "fan-b.txt").read_text() == "B\n", fanout.stdout)

        # C09: Ctrl-C reaches the remote command; confirmed partial edits return.
        interrupted = subprocess.Popen(
            [self.marsh, "cloud", "-c",
             "trap 'echo partial > partial.txt; exit 130' INT; echo ready; while :; do sleep 1; done"],
            cwd=p, env=self.environment, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, start_new_session=True,
        )
        assert interrupted.stdout
        line = interrupted.stdout.readline()
        os.killpg(interrupted.pid, signal.SIGINT)
        _, stderr = interrupted.communicate(timeout=CLOUD_TIMEOUT)
        self.check("C09 cancelled 130", (line, interrupted.returncode) == (b"ready\n", 130), stderr[-400:])
        self.check("C09 partial returned", (p / "partial.txt").read_text() == "partial\n", None)

        # C09: Ctrl-C still lands while unread stdin is backpressured.
        flood = subprocess.Popen(
            ["/bin/sh", "-c", f"yes | {self.marsh} cloud -c 'echo ready; sleep 600'"],
            cwd=p, env=self.environment, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, start_new_session=True,
        )
        assert flood.stdout
        ready = flood.stdout.readline()
        time.sleep(3)
        started = time.monotonic()
        os.killpg(flood.pid, signal.SIGINT)
        _, stderr = flood.communicate(timeout=CLOUD_TIMEOUT)
        self.check("C09 Ctrl-C under stdin backpressure",
                   ready == b"ready\n" and time.monotonic() - started < 60, stderr[-400:])

        # C09: Ctrl-C while the sandbox is still being prepared: nothing runs.
        early = subprocess.Popen(
            [self.marsh, "cloud", "-c", "echo ran > early.txt"], cwd=p, env=self.environment,
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            start_new_session=True,
        )
        time.sleep(4)
        os.killpg(early.pid, signal.SIGINT)
        _, stderr = early.communicate(timeout=CLOUD_TIMEOUT)
        self.check("C09 setup cancel 130, nothing ran",
                   (early.returncode, (p / "early.txt").exists()) == (130, False), stderr[-400:])

        # C12/receipts: every node recorded with its sandbox and verified cleanup.
        jobs = self.document("jobs", "--json")["jobs"]
        clouds = [self.document("jobs", "show", job["job_id"], "--json") for job in jobs
                  if job["command"] in ("cloud", "fixture") and job.get("placement") == "cloud"]
        self.records.append({"receipts": clouds})
        self.check("receipts placement/vm/cleanup", len(clouds) >= 20 and all(
            r.get("placement") == "cloud" and str(r.get("vm_id", "")).startswith("sbx_")
            and r.get("cleanup") == "verified" for r in clouds), clouds)
        self.check("C09 receipt cancelled", any(r.get("state") == "cancelled" for r in clouds), clouds)
        ownership = self.control_home / "cloud" / "cloud-ownership.json"
        owned = json.loads(ownership.read_text())["entries"]
        self.check("C12 ownership map empty", owned == {}, owned)
        inventory = json.loads(subprocess.run([self.sbx, "--cloud", "ls", "--json"], check=True,
                                              capture_output=True, timeout=120).stdout)
        ids = {r.get("vm_id") for r in clouds}
        self.check("C12 sandboxes absent", not any(row.get("id") in ids for row in inventory), inventory)
        self.stdin_terminal()
        self.listing()
        self.interactive()
        self.daemon_crash()

    def kits(self) -> None:
        """Registered Kits in Docker Cloud: a command called in a cloud block
        runs in its own digest-pinned Kit (its sandbox, the block's current
        files, streams, status, changes back before it returns, one job tree),
        and a Kit job's `marsh cloud` runs that same Kit with the job's view."""
        p = self.project
        called = self.cloud_shell(
            "cloud { echo base > agent.txt; printf 'in\\n' | fixture exec sh -c 'cat; cat agent.txt; "
            "echo agent >> agent.txt; id -un; echo err >&2; exit 4'; "
            "echo \"agent=$?\"; cat agent.txt; }"
        )
        self.check("agent call streams/status/return",
                   called.stdout == b"in\nbase\nagent\nagent=4\nbase\nagent\n" and b"err" in called.stderr,
                   (called.stdout, called.stderr[-600:]))
        self.check("agent change reaches the laptop", (p / "agent.txt").read_text() == "base\nagent\n", None)
        narrowed = subprocess.run(
            [self.marsh, "cloud", "-c", "fixture exec true"], cwd=p,
            env={**self.environment, "MARSH_SPAWN": "none"}, stdin=subprocess.DEVNULL,
            capture_output=True, timeout=CLOUD_TIMEOUT,
        )
        self.check("MARSH_SPAWN narrows agent calls in a block", narrowed.returncode == 127,
                   (narrowed.returncode, narrowed.stderr[-400:]))
        interrupted_call = subprocess.Popen(
            [self.marsh, "cloud", "-c",
             "fixture exec sh -c 'trap \"echo partial > agent-part.txt; exit 130\" INT; echo ready; "
             "while :; do sleep 1; done'; echo \"after=$?\""],
            cwd=p, env=self.environment, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, start_new_session=True,
        )
        assert interrupted_call.stdout
        line = interrupted_call.stdout.readline()
        os.killpg(interrupted_call.pid, signal.SIGINT)
        _, stderr = interrupted_call.communicate(timeout=CLOUD_TIMEOUT)
        cancelled = [self.document("jobs", "show", job["job_id"], "--json")
                     for job in self.document("jobs", "--json")["jobs"]
                     if job["state"] != "finished"]
        self.check("agent call cancelled across both levels",
                   (line, interrupted_call.returncode) == (b"ready\n", 130)
                   and (p / "agent-part.txt").exists()
                   and (p / "agent-part.txt").read_text() == "partial\n",
                   (line, interrupted_call.returncode, stderr[-600:], cancelled))

        # A block that is exactly one call runs straight in that Kit: one
        # sandbox, one job (the command itself), no cloud shell.
        direct = self.cloud_shell(
            "cloud { fixture exec sh -c 'echo direct > direct.txt; id -un' </dev/null; }")
        self.check("direct Kit call", direct.stdout == b"agent\n"
                   and b"Kit fixture" in direct.stderr and b"marsh cloud shell" not in direct.stderr
                   and (p / "direct.txt").read_text() == "direct\n", (direct.stdout, direct.stderr[-600:]))

        # From a Kit job: `marsh cloud` runs the job's own Kit in the cloud
        # with the job's view; its changes return to the laptop; stdin,
        # streams, and status relay through the job.
        from_job = self.run(
            [self.marsh, "-c",
             "printf 'job-in\\n' | fixture exec /run/marsh/marsh cloud -c "
             "'cat; echo fromjob > job.txt; id -un; test -e /run/marsh/cap.sock || echo NOCAP; exit 5'; "
             "echo \"job=$?\""],
            timeout=CLOUD_TIMEOUT, check=False,
        )
        self.check("job-origin cloud streams/status",
                   from_job.stdout == b"job-in\nagent\nNOCAP\njob=5\n",
                   (from_job.stdout, from_job.stderr[-600:]))
        self.check("job-origin cloud write-back", (p / "job.txt").read_text() == "fromjob\n", None)

        # `cloud` is its own spawn name: MARSH_SPAWN can withhold it.
        withheld = subprocess.run(
            [self.marsh, "-c",
             "export MARSH_SPAWN=fixture; fixture exec /run/marsh/marsh cloud -c 'echo ran > withheld.txt'"],
            cwd=p, env=self.environment, stdin=subprocess.DEVNULL,
            capture_output=True, timeout=CLOUD_TIMEOUT,
        )
        self.check("MARSH_SPAWN withholds job-origin cloud",
                   withheld.returncode == 125 and b"spawn refused: cloud" in withheld.stderr
                   and not (p / "withheld.txt").exists(), (withheld.returncode, withheld.stderr[-400:]))

        # The calling job ends first: the cloud node is cancelled and its
        # changes are kept on the host, never written into the job's view.
        orphan = self.run(
            [self.marsh, "-c",
             "fixture exec sh -c '( /run/marsh/marsh cloud -c \"trap \\\"echo late > orphan-late.txt; exit 0\\\" INT; "
             "echo ready; while :; do sleep 1; done\" > orphan.out 2>&1 & ); "
             "while ! grep -q ready orphan.out 2>/dev/null; do sleep 1; done; exit 0'"],
            timeout=CLOUD_TIMEOUT, check=False,
        )
        def orphan_settled():
            for job in self.document("jobs", "--json")["jobs"]:
                if job["command"] == "cloud" and job["state"] not in ("queued", "running"):
                    shown = self.document("jobs", "show", job["job_id"], "--json")
                    if (shown.get("exit") or {}).get("cause") == "write_back_skipped: caller gone":
                        return shown
            return None
        deadline = time.monotonic() + CLOUD_TIMEOUT
        settled = None
        while settled is None and time.monotonic() < deadline:
            settled = orphan_settled()
            time.sleep(2)
        self.check("job-origin write-back skipped after the caller ended",
                   orphan.returncode == 0 and settled is not None and settled.get("cleanup") == "verified"
                   and not (p / "orphan-late.txt").exists(),
                   (orphan.returncode, orphan.stderr[-400:], settled))

        tree = self.document("jobs", "--json")["jobs"]
        shown = [self.document("jobs", "show", job["job_id"], "--json") for job in tree
                 if job.get("placement") == "cloud" and job["command"] in ("fixture", "cloud")]
        agents = [r for r in shown if r["command"] == "fixture"
                  and str((r.get("lineage") or {}).get("parent", "")).startswith("job:")]
        directs = [r for r in shown if r["command"] == "fixture"
                   and str((r.get("lineage") or {}).get("parent", "")).startswith("session:")]
        from_jobs = [r for r in shown if r["command"] == "cloud"
                     and str((r.get("lineage") or {}).get("parent", "")).startswith("job:")
                     and r.get("kit_profile") == self.kit]
        self.records.append({"agent_receipts": agents, "job_origin_receipts": from_jobs})
        self.check("agent receipts under their block", len(agents) == 2 and all(
            str((r.get("lineage") or {}).get("parent", "")).startswith("job:")
            and str(r.get("vm_id", "")).startswith("sbx_") and r.get("cleanup") == "verified"
            and r.get("image") == self.kit for r in agents), agents)
        self.check("direct receipt: the command itself, in its Kit", len(directs) == 1 and all(
            r.get("image") == self.kit and r.get("cleanup") == "verified"
            and r.get("args") == ["exec", "sh", "-c", "echo direct > direct.txt; id -un"]
            for r in directs), directs)
        self.check("job-origin receipt", len(from_jobs) == 2 and all(
            str(r.get("vm_id", "")).startswith("sbx_") and r.get("cleanup") == "verified"
            and r.get("image") == self.kit for r in from_jobs), from_jobs)

    def daemon_crash(self) -> None:
        """C10: the daemon dies mid-execution. Nothing replays or writes back;
        the receipt is uncertain and `workers reset cloud` removes the sandbox."""
        p = self.project
        process = subprocess.Popen(
            [self.marsh, "cloud", "-c", "echo ready; sleep 5; echo late > late.txt; sleep 600"],
            cwd=p, env=self.environment, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, start_new_session=True,
        )
        assert process.stdout
        line = process.stdout.readline()
        old_pid = self.owned_daemon_pid
        self.check("C10 owned daemon", bool(old_pid) and stable_process_identity(old_pid)
                   == self.owned_daemon_process_identity, old_pid)
        os.kill(old_pid, signal.SIGKILL)
        deadline = time.monotonic() + 30
        while stable_process_identity(old_pid) is not None and time.monotonic() < deadline:
            time.sleep(0.1)
        try:
            process.wait(timeout=60)
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
        self.check("C10 client failed", (line, process.returncode not in (0, None)) == (b"ready\n", True),
                   process.returncode)
        for field in ("owned_daemon_id", "owned_daemon_pid", "owned_daemon_process_identity",
                      "owned_daemon_control_token"):
            setattr(self, field, None)
        self.document("status", "--json")
        jobs = self.document("jobs", "--json")["jobs"]
        crashed = [self.document("jobs", "show", job["job_id"], "--json") for job in jobs
                   if job["command"] == "cloud" and job["state"] not in ("finished", "cancelled")]
        self.records.append({"crashed": crashed})
        self.check("C10 uncertain receipt", len(crashed) == 1 and crashed[0]["state"] == "unknown"
                   and crashed[0]["cleanup"] == "uncertain"
                   and str(crashed[0].get("vm_id", "")).startswith("sbx_"), crashed)
        time.sleep(8)
        self.check("C10 no write-back", not (p / "late.txt").exists(), None)
        reset = self.run([self.marsh, "workers", "reset", "cloud"], timeout=CLOUD_TIMEOUT, check=False)
        self.check("C10 reset cloud", reset.returncode == 0, reset.stderr[-400:])
        owned = json.loads((self.control_home / "cloud" / "cloud-ownership.json").read_text())["entries"]
        self.check("C10 ownership empty after reset", owned == {}, owned)
        inventory = json.loads(subprocess.run([self.sbx, "--cloud", "ls", "--json"], check=True,
                                              capture_output=True, timeout=120).stdout)
        self.check("C10 sandbox absent", not any(row.get("id") == crashed[0]["vm_id"] for row in inventory),
                   inventory)

    def listing(self) -> None:
        """Files a block creates are listed by the very next command in the
        session shell (its VM file share caches directory listings)."""
        (self.project / "deep").mkdir(exist_ok=True)
        listed = self.cloud_shell(
            "ls >/dev/null; ls deep >/dev/null; "
            "cloud { echo x > listed.txt; mkdir -p deep/er2; echo y > deep/er2/f.txt; }; "
            "ls; echo ---; ls deep; echo ---; ls deep/er2"
        )
        top, deep, inner = listed.stdout.split(b"---\n")
        self.check("write-back listed by the next command",
                   b"listed.txt" in top.split() and b"er2" in deep.split() and b"f.txt" in inner.split(),
                   (listed.stdout, listed.stderr[-400:]))

    def stdin_terminal(self) -> None:
        """A block run from a terminal: its stdin relays the terminal as a
        pipe. `fanout` reads nothing from it (as at a local prompt) instead of
        waiting for EOF; piped input inside the block is still data; `read`
        still gets what is typed."""
        import pty
        import select

        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(self.project)
            os.execve(self.marsh, [self.marsh, "-c",
                                   "cloud { fanout { a: echo A; b: echo B; } | collect; "
                                   "printf piped | fanout { p: cat; } | collect; "
                                   "echo asking; read line; echo \"got=$line\"; }"],
                      self.environment)
        output = bytearray()
        typed = False
        deadline = time.monotonic() + 300
        status = None
        try:
            while time.monotonic() < deadline:
                ready, _, _ = select.select([fd], [], [], 0.5)
                if ready:
                    try:
                        chunk = os.read(fd, 65536)
                    except OSError:
                        chunk = b""
                    if not chunk:
                        break
                    output.extend(chunk)
                if not typed and b"asking" in output:
                    os.write(fd, b"typed\r")
                    typed = True
        finally:
            done = os.waitpid(pid, os.WNOHANG)
            if done[0] == 0:
                os.kill(pid, signal.SIGTERM)
                done = os.waitpid(pid, 0)
            status = os.waitstatus_to_exitcode(done[1])
            os.close(fd)
        text = bytes(output).replace(b"\r\n", b"\n")
        self.check("stdin-terminal: fanout does not wait; piped data; read typed",
                   status == 0 and b"\nA\n" in text and b"\nB\n" in text and b"piped" in text
                   and b"got=typed" in text, (status, text[-1500:]))

    def interactive(self) -> None:
        """C14: `marsh cloud` on a terminal is an interactive cloud shell:
        a real tty of the caller's size, resizes, exit status, write-back."""
        import fcntl
        import pty
        import select
        import struct
        import termios

        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(self.project)
            os.execve(self.marsh, [self.marsh, "cloud"], self.environment)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
        output = bytearray()

        def read_until(marker: bytes, timeout: float) -> bool:
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                ready, _, _ = select.select([fd], [], [], 0.2)
                if not ready:
                    continue
                try:
                    chunk = os.read(fd, 65536)
                except OSError:
                    return False
                if not chunk:
                    return False
                output.extend(chunk)
                # Answer the shell's cursor-position query like a terminal.
                if b"\x1b[6n" in chunk:
                    os.write(fd, b"\x1b[1;1R")
                if marker and marker in output:
                    return True
            return False

        try:
            self.check("C14 prompt", read_until(b"$ ", 240), output[-400:])
            os.write(fd, b"tty | sed s/^/TTY=/; stty size | sed s/^/SIZE=/; echo made > interactive.txt\r")
            self.check("C14 initial size", read_until(b"SIZE=30 100", 60), output[-600:])
            fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
            time.sleep(2)
            os.write(fd, b"stty size | sed s/^/SIZE=/\r")
            self.check("C14 resize", read_until(b"SIZE=40 120", 60), output[-600:])
            os.write(fd, b"exit 7\r")
            read_until(b"\x00never", 60)
        finally:
            # A failed check must not wait forever on a live shell.
            deadline = time.monotonic() + 30
            while (done := os.waitpid(pid, os.WNOHANG))[0] == 0 and time.monotonic() < deadline:
                time.sleep(0.5)
            if done[0] == 0:
                os.kill(pid, signal.SIGTERM)
                done = os.waitpid(pid, 0)
            status = done[1]
            os.close(fd)
        self.records.append({"interactive_tail": output[-1500:].decode(errors="replace")})
        self.check("C14 tty", b"TTY=/dev/pts/" in output, output[-600:])
        self.check("C14 exit status", os.waitstatus_to_exitcode(status) == 7, status)
        self.check("C14 write-back", (self.project / "interactive.txt").read_text() == "made\n", None)

    def finish(self, error: Exception | None) -> int:
        cleanup_errors = self.scope_cleanup_errors()
        if cleanup_errors:
            cleanup = RuntimeError("; ".join(cleanup_errors))
            error = cleanup if error is None else RuntimeError(f"{error}; cleanup: {cleanup}")
        destination = self.evidence / self.EVIDENCE
        destination.write_text(json.dumps({
            "outcome": "failed" if error else "passed",
            "failure": str(error) if error else None,
            "environment": self.source,
            "records": self.records,
        }, indent=2) + "\n", encoding="utf-8")
        destination.chmod(0o600)
        print(f"cloud acceptance: {'failed' if error else 'passed'}; evidence: {destination}")
        if error:
            print(f"cloud acceptance: {error}")
        return 1 if error else 0


class SignedOut(CloudAcceptance):
    """C01: the stock CLI is not signed in for cloud (a real stock refusal,
    obtained by giving only its `--cloud` calls an empty HOME). The call is
    refused with the sign-in instruction; nothing runs locally or remotely."""

    EVIDENCE = "cloud-signed-out.json"

    def run_all(self) -> None:
        self.stock_before = stock_baseline(self.sbx)
        self.source["stock_before"] = self.stock_before
        self.document("status", "--json")
        refused = self.run([self.marsh, "cloud", "-c", "echo ran > ran.txt"], timeout=CLOUD_TIMEOUT,
                           check=False)
        self.check("C01 refused 125", refused.returncode == 125, refused.stderr[-400:])
        self.check("C01 actionable", b"sbx login" in refused.stderr, refused.stderr[-400:])
        self.check("C01 nothing ran", not (self.project / "ran.txt").exists(), None)
        jobs = [job for job in self.document("jobs", "--json")["jobs"] if job["command"] == "cloud"]
        receipt = self.document("jobs", "show", jobs[0]["job_id"], "--json") if jobs else {}
        self.check("C01 receipt", receipt.get("execution", {}).get("status") == "not_started"
                   and receipt.get("cleanup") == "not_required" and receipt.get("vm_id") is None, receipt)


def signed_out_sbx(real: str, directory: pathlib.Path) -> str:
    empty = directory / "signed-out-home"
    empty.mkdir(mode=0o700, exist_ok=True)
    wrapper = directory / "sbx-signed-out"
    wrapper.write_text(
        "#!/bin/sh\n"
        f'for a in "$@"; do [ "$a" = --cloud ] && HOME="{empty}" exec "{real}" "$@"; done\n'
        f'exec "{real}" "$@"\n'
    )
    wrapper.chmod(0o700)
    return str(wrapper)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    candidate_arguments(parser)
    parser.add_argument("--marsh", required=True)
    parser.add_argument("--sbx", default="sbx")
    parser.add_argument("--kit", required=True)
    parser.add_argument("--evidence", required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--source-tree", required=True)
    parser.add_argument("--i-accept-cloud-charges", action="store_true", required=True,
                        help="this creates billed Docker Cloud sandboxes")
    parser.add_argument("--scenario", choices=["all", "main", "signed-out", "kits", "stdin"], default="all")
    arguments = parser.parse_args(argv)
    if arguments.scenario in ("all", "signed-out"):
        import shutil as _shutil
        real = _shutil.which(arguments.sbx) or arguments.sbx
        scratch = pathlib.Path(arguments.evidence).resolve()
        signed = argparse.Namespace(**{**vars(arguments), "sbx": signed_out_sbx(real, scratch)})
        suite = SignedOut(signed)
        try:
            suite.run_all()
            status = suite.finish(None)
        except Exception as error:  # keep evidence from the first real failure
            status = suite.finish(error)
        if status or arguments.scenario == "signed-out":
            return status
    suite = CloudAcceptance(arguments)
    started = time.monotonic()
    try:
        if arguments.scenario in ("kits", "stdin"):
            suite.stock_before = stock_baseline(suite.sbx)
            suite.source["stock_before"] = suite.stock_before
            suite.document("status", "--json")
            if arguments.scenario == "kits":
                suite.kits()
            else:
                suite.stdin_terminal()
                suite.listing()
        else:
            suite.run_all()
    except KeyboardInterrupt:
        return suite.finish(InterruptedError("cloud acceptance interrupted"))
    except Exception as error:  # keep evidence from the first real failure
        return suite.finish(error)
    suite.records.append({"seconds": round(time.monotonic() - started)})
    return suite.finish(None)


if __name__ == "__main__":
    raise SystemExit(main())
