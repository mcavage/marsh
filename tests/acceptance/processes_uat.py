#!/usr/bin/env python3
"""Nested processes (docs/design/processes.md) black-box acceptance.

Scenarios and the spec claims they check: docs/design/processes-acceptance.md.
Written before the implementation, from the spec only. Drives only public
callers: host bash, `marsh -c` sessions, the host CLI, and registered fixture
Kit commands acting as agents (the fixture's argv-only `pipeline`, `exec`,
`bench`, and `cap-flood` modes reach `bash`, `/bin/bash`, `/bin/sh`, `env`,
registered names, and `cap.sock` exactly as an agent process would). Observes
only stdout/stderr bytes, exit statuses, receipts and lineage from
`marsh jobs`/`status --json`, `marsh context`, the user's files, and stock
`sbx exec <owned vm> docker inspect`. Runs in an isolated MARSH_HOME/control
scope and removes only VMs from its own ownership map (run.py cleanup).
"""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import pathlib
import pty
import re
import select
import shlex
import signal
import statistics
import subprocess
import sys
import termios
import threading
import time
import traceback
from typing import Any, Callable

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from provenance import stock_cleanup_errors, stock_vm_inventory  # noqa: E402
from run import stable_process_identity  # noqa: E402
from workspaces_uat import Fail, Workspaces, find_records, require, text  # noqa: E402

SCENARIOS = {
    "P01-job-surface": "job_surface",
    "P02-self-spawn-bash-c": "self_spawn",
    "P03-direct-execve": "direct_execve",
    "P04-entry-once": "entry_once",
    "P05-image-tool-local": "image_tool_local",
    "P06-cross-kit-default": "cross_kit_default",
    "P07-run-three-callers": "run_three_callers",
    "P08-fanout-in-job": "fanout_in_job",
    "P09-cli-split-real-bash": "cli_split_real_bash",
    "P10-env-i-links": "env_i_links",
    "P11-env-forwarding": "env_forwarding",
    "P12-depth-cap": "depth_cap4",
    "P13-same-kit-chain": "same_kit_chain",
    "P14-fan-out-cap": "fan_out_cap",
    "P15-pool-fail-fast": "pool_fail_fast",
    "P16-spawn-narrowing": "spawn_narrowing",
    "P17-branch-confinement": "branch_confinement",
    "P18-tty-child-refused": "tty_child_refused",
    "P19-ctrl-c-tree": "ctrl_c_tree",
    "P20-parent-exit-cascade": "parent_exit_cascade",
    "P21-daemon-restart": "daemon_restart_tree",
    "P22-cap-socket-cap": "cap_socket_cap",
    "P23-awareness-context": "awareness_context",
    "P24-real-bash-startup": "real_bash_startup",
    "P25-child-after-first-split": "child_after_first_split",
    "P27-no-shell-branch-from-job": "no_shell_branch_from_job",
    "P28-env-whole-tree": "env_whole_tree",
    "P29-root-interrupt-cascade": "root_interrupt_cascade",
    "P30-inspection-text": "inspection_text",
    "P31-agents-doc-examples": "agents_doc_examples",
    "P32-live-claude-codex": "live_claude_codex",
    "P33-jobs-listing-scope": "jobs_listing_scope",
    "P34-background-cold-notice": "background_cold_notice",
    "P35-wall-inheritance": "wall_inheritance",
    "P36-tree-kit-vm-cap": "tree_kit_vm_cap",
    "P37-split-lineage-tree": "split_lineage_tree",
    "P38-fanout-cli": "fanout_cli",
}
LIVE = {"P32-live-claude-codex"}  # billed: only with --live
# The restart scenario quarantines the fixture VM, so it runs last.
ORDER = [s for s in SCENARIOS if s != "P21-daemon-restart"] + ["P21-daemon-restart"]
NEEDS_SHELL_KIT = {"P06-cross-kit-default", "P11-env-forwarding", "P12-depth-cap", "P14-fan-out-cap",
                   "P16-spawn-narrowing", "P19-ctrl-c-tree", "P20-parent-exit-cascade", "P28-env-whole-tree",
                   "P29-root-interrupt-cascade", "P30-inspection-text", "P31-agents-doc-examples",
                   "P36-tree-kit-vm-cap"}
ALT = "fixture-alt"  # a second registered name for the same fixture Kit
FAN_FRAME = b"== copy (complete) ==\ninput\n\n== upper (complete) ==\nINPUT\n\n"
STARTUP_BUDGET_US = 2000  # PATH `bash` is the image's own: p50 within 2 ms of /bin/bash
LINK_BUDGET_US = 20000  # the static artifact (`marsh`, links) starts within 20 ms
NEW_FIXTURE_MODES = ("exec", "bench", "cap-flood")


def q(argv: list[str]) -> str:
    return shlex.join(argv)


class Processes(Workspaces):
    def __init__(self, arguments: argparse.Namespace) -> None:
        super().__init__(arguments)
        self.selected = [s for s in ORDER if (not arguments.only or s in arguments.only)
                         and (s not in LIVE or arguments.live)]
        registry = self.control_home / "commands.json"
        mapping = json.loads(registry.read_text())
        mapping[ALT] = mapping["fixture"]
        if arguments.live:  # the packaged agent Kits, for the billed check
            for name in ("claude", "codex"):
                mapping.setdefault(name, str(arguments.guest_artifacts / "kits" / f"marsh-{name}"))
        registry.write_text(json.dumps(mapping, sort_keys=True) + "\n", encoding="utf-8")

    # ---- callers ---------------------------------------------------------
    def injob(self, script: str, *, shell: str = "bash", name: str = "fixture", prefix: str = "",
              timeout: float = 300) -> subprocess.CompletedProcess[bytes]:
        """A top-level Kit job whose process runs `SHELL -c SCRIPT` with no shell in between."""
        completed = self.brush(prefix + q([name, "pipeline", "|", shell, "-c", script]), self.project,
                               timeout=timeout)
        self.fixture_capable(completed.stdout, completed.stderr)
        return completed

    def fixture_capable(self, *blobs: bytes | str) -> None:
        joined = "".join(b if isinstance(b, str) else text(b) for b in blobs)
        mode = re.search(r"unknown fixture mode: (\S+)", joined)
        if mode:
            raise Fail(f"fixture Kit lacks the `{mode.group(1)}` mode; pass --kit tests/acceptance/fixture "
                       "or republish the fixture from this checkout")

    # ---- receipts and lineage ---------------------------------------------
    def job_ids(self) -> set[str]:
        return {s["job_id"] for s in self.document("jobs", "--json")["jobs"]}

    def show(self, job: str) -> dict[str, Any]:
        return self.document("jobs", "show", job, "--json")

    def settle(self, before: set[str], count: int, *, timeout: float = 120, exact: bool = True,
               verified: bool = True) -> list[dict[str, Any]]:
        """Wait until `count` new jobs are terminal (and cleanup verified); with `exact`, no more appear."""
        deadline = time.monotonic() + timeout
        while True:
            fresh = [self.show(j) for j in sorted(self.job_ids() - before)]
            done = [r for r in fresh if r.get("state") not in ("running", "queued", "submitted")
                    and (not verified or r.get("cleanup") == "verified")]
            if len(fresh) >= count and len(done) == len(fresh):
                break
            if time.monotonic() > deadline:
                raise Fail(f"want {count} settled new jobs, got "
                           f"{[(r.get('command'), r.get('state'), r.get('cleanup')) for r in fresh]}")
            time.sleep(0.5)
        if exact:
            time.sleep(3)
            fresh = [self.show(j) for j in sorted(self.job_ids() - before)]
            require(len(fresh) == count, f"want exactly {count} new jobs, got {len(fresh)}: "
                                         f"{[(r.get('command'), self.lin(r, strict=False)) for r in fresh]}"[:900])
        return fresh

    def running(self, before: set[str]) -> list[dict[str, Any]]:
        return [r for r in (self.show(j) for j in self.job_ids() - before) if r.get("state") == "running"]

    @staticmethod
    def lin(receipt: dict[str, Any], strict: bool = True) -> dict[str, Any]:
        lineage = receipt.get("lineage")
        if strict:
            require(isinstance(lineage, dict) and {"parent", "root", "depth"} <= set(lineage),
                    f"receipt lacks lineage {{parent, root, depth, spawn}} (s9): {receipt!r}"[:800])
        return lineage if isinstance(lineage, dict) else {}

    def parent_is(self, child: dict[str, Any], parent: dict[str, Any]) -> bool:
        return parent["job_id"] in json.dumps(self.lin(child).get("parent"))

    def tops(self, receipts: list[dict[str, Any]]) -> list[dict[str, Any]]:
        ids = {r["job_id"] for r in receipts}
        return [r for r in receipts if not any(i in json.dumps(self.lin(r).get("parent")) for i in ids)]

    def one_top(self, receipts: list[dict[str, Any]], what: str) -> dict[str, Any]:
        tops = self.tops(receipts)
        require(len(tops) == 1, f"{what}: want one root job, got {[(r.get('command'), self.lin(r)) for r in tops]}")
        return tops[0]

    def assert_children(self, receipts: list[dict[str, Any]], parent: dict[str, Any], commands: list[str],
                        what: str) -> list[dict[str, Any]]:
        children = [r for r in receipts if r is not parent]
        require(sorted(r.get("command") for r in children) == sorted(commands),
                f"{what}: child commands {[r.get('command') for r in children]} != {commands}")
        depth = self.lin(parent).get("depth")
        for child in children:
            lineage = self.lin(child)
            require(self.parent_is(child, parent), f"{what}: child parent is not job {parent['job_id']}: {lineage}")
            require(parent["job_id"] in json.dumps(lineage.get("root")) or
                    json.dumps(self.lin(parent).get("root")) == json.dumps(lineage.get("root")),
                    f"{what}: child root differs from its parent's tree: {lineage}")
            require(isinstance(depth, int) and lineage.get("depth") == depth + 1,
                    f"{what}: child depth {lineage.get('depth')} != parent depth {depth} + 1")
        return children

    def kv(self, stdout: bytes) -> dict[str, str]:
        return dict(line.split("=", 1) for line in text(stdout).splitlines() if "=" in line)

    def identity(self, stdout: bytes) -> list[dict[str, Any]]:
        found = []
        for line in text(stdout).splitlines():
            if line.startswith("{\"cwd\""):
                found.append(json.loads(line))
        return found

    # ---- scenarios --------------------------------------------------------
    def job_surface(self) -> None:
        """s4: PATH/MARSH_JOB; bash, sh and SHELL are the image's own; registered names route; read-only binds."""
        before = self.job_ids()
        probe = "; ".join([
            'echo "PATH=$PATH"', 'echo "SHELL=${SHELL-unset}"', 'echo "JOB=$MARSH_JOB"',
            'echo "pathbash=$(command -v bash)"',
            'echo "bashv=$(bash --version | head -n 1)"', 'echo "realv=$(/bin/bash --version | head -n 1)"',
            "echo \"typebash=$(bash -c 'type -P bash')\"",
            "echo \"type=$(bash -c 'type fixture')\"", "echo \"bv=$(bash -c 'echo ${BASH_VERSION:+set}')\"",
            'echo "bashlink=$(test -e /run/marsh/bin/bash && echo yes || echo no)"',
            'echo "shlink=$(test -e /run/marsh/bin/sh && echo yes || echo no)"',
            'echo "links=$(ls /run/marsh/bin | tr "\\n" " ")"',
            'echo "binsh=$(readlink -f /bin/sh)"', 'echo "binbash=$(readlink -f /bin/bash)"',
            'echo "w_art=$( (: >> /run/marsh/marsh) 2>/dev/null && echo yes || echo no)"',
            'echo "w_bin=$( (: > /run/marsh/bin/x) 2>/dev/null && echo yes || echo no)"',
            'echo "w_json=$( (: >> /run/marsh/job.json) 2>/dev/null && echo yes || echo no)"',
            "echo \"ctx=$(head -n 8 /run/marsh/context.md | tr '\\n' ' ')\"",
            "echo \"jobjson=$(tr -d '\\n' < /run/marsh/job.json)\""])
        completed = self.injob(probe, shell="/bin/sh")
        require(completed.returncode == 0, f"probe job failed: {text(completed.stderr)[-600:]!r}")
        v = self.kv(completed.stdout)
        receipts = self.settle(before, 1)
        require(v.get("PATH", "").startswith("/run/marsh/bin:"), f"PATH must start /run/marsh/bin: {v.get('PATH')!r}")
        require(not v.get("SHELL", "").startswith("/run/marsh"), f"SHELL points into marsh: {v.get('SHELL')!r}")
        require(v.get("JOB") == receipts[0]["job_id"], f"MARSH_JOB={v.get('JOB')!r} is not the job id")
        # Operator decision: a job's bash is the image's real bash.
        require(v.get("pathbash") and not v["pathbash"].startswith("/run/marsh"), f"PATH bash is {v.get('pathbash')!r}")
        require("GNU bash" in v.get("bashv", "") and "marsh" not in v.get("bashv", ""),
                f"`bash --version` is not the image's GNU bash: {v.get('bashv')!r}")
        require(v.get("bashv") == v.get("realv"), f"PATH bash {v.get('bashv')!r} != /bin/bash {v.get('realv')!r}")
        require(not v.get("typebash", "/run").startswith("/run/marsh"), f"type -P bash: {v.get('typebash')!r}")
        require(v.get("type") == "fixture is /run/marsh/bin/fixture", f"type fixture: {v.get('type')!r}")
        require(v.get("bv") == "set", "the image's bash does not set BASH_VERSION")
        require((v.get("bashlink"), v.get("shlink")) == ("no", "no"), f"a bash/sh link exists: {v}")
        links = set(v.get("links", "").split())
        require({"marsh", "fixture", ALT, "shell"} <= links, f"links lack registered names: {links}")
        require(not v.get("binsh", "/run").startswith("/run/marsh") and
                not v.get("binbash", "/run").startswith("/run/marsh"), f"image shells shadowed: {v}")
        require((v.get("w_art"), v.get("w_bin"), v.get("w_json")) == ("no", "no", "no"),
                f"artifact, links, or job.json writable by the job: {v}")
        ctx = v.get("ctx", "")
        require("`fixture`" in ctx and "`shell`" in ctx and "marsh job" in ctx,
                f"context.md is not this job's (name and spawn set): {ctx!r}"[:600])
        document = json.loads(v.get("jobjson") or "null")
        require(isinstance(document, dict) and receipts[0]["job_id"] in json.dumps(document)
                and "fixture" in json.dumps(document), f"job.json lacks job id / own name: {document!r}"[:600])
        self.verify_deleted(receipts)

    def self_spawn(self) -> None:
        """s1/s5/s9: a job's `bash -c 'fixture'` and `/bin/bash -c` become child jobs with lineage; `jobs --tree`."""
        before = self.job_ids()
        first = self.brush("fixture identity", self.project)
        require(first.returncode == 0, f"top-level fixture failed: {text(first.stderr)[-400:]!r}")
        unrelated = self.settle(before, 1)[0]["job_id"]
        before = self.job_ids()
        completed = self.injob("fixture identity; echo rc1=$?; /bin/bash -c 'fixture identity'; echo rc2=$?; "
                               "marsh jobs --tree; echo tree=$?")
        v = self.kv(completed.stdout)
        require((v.get("rc1"), v.get("rc2"), v.get("tree")) == ("0", "0", "0"),
                f"self-spawn statuses {v}: {text(completed.stderr)[-600:]!r}")
        require(len(self.identity(completed.stdout)) == 2, "children's stdout was not relayed")
        receipts = self.settle(before, 3)
        top = self.one_top(receipts, "self-spawn")
        children = self.assert_children(receipts, top, ["fixture", "fixture"], "self-spawn")
        shown = json.dumps(self.show(top["job_id"]))
        require(all(c["job_id"] in shown for c in children), "`jobs show PARENT` does not list child ids (s9)")
        tree = text(self.cli(["jobs", "--tree"], self.project, timeout=60).stdout)
        require(all(r["job_id"][:8] in tree for r in receipts), f"host `jobs --tree` lacks the tree: {tree[-800:]!r}")
        inner = text(completed.stdout)
        require(top["job_id"][:8] in inner and unrelated[:8] not in inner,
                "in-job `jobs --tree` must show only the job's own subtree (s3 ProcessShow)")
        self.verify_deleted(receipts)

    def direct_execve(self) -> None:
        """s4 table/s5 child case: execvp of a registered name, its link path, and from /bin/sh and env."""
        before = self.job_ids()
        completed = self.brush(q(["fixture", "pipeline", "|", "fixture", "streams", "a b"]), self.project)
        self.fixture_capable(completed.stderr)
        require(completed.returncode == 23 and b'OUT\0["a b"]\n' in completed.stdout
                and b"ERR\0fixture\n" in completed.stderr,
                f"execvp child bytes/status not relayed: {completed.returncode} {completed.stdout!r}")
        receipts = self.settle(before, 2)
        self.assert_children(receipts, self.one_top(receipts, "execvp"), ["fixture"], "execvp")
        before = self.job_ids()
        completed = self.injob("/run/marsh/bin/fixture identity; echo a=$?; fixture identity; echo b=$?; "
                               "env fixture identity; echo c=$?", shell="/bin/sh")
        v = self.kv(completed.stdout)
        require((v.get("a"), v.get("b"), v.get("c")) == ("0", "0", "0"), f"real-shell callers: {v}")
        receipts = self.settle(before, 4)
        self.assert_children(receipts, self.one_top(receipts, "/bin/sh"), ["fixture"] * 3, "/bin/sh callers")
        self.verify_deleted(receipts)

    def entry_once(self) -> None:
        """s5 entry case (EntryOnce, NoEntryRecursion): `exec fixture` from the entrypoint runs the image binary once."""
        before = self.job_ids()
        completed = self.brush(q(["fixture", "exec", "fixture", "identity"]), self.project)
        self.fixture_capable(completed.stderr)
        require(completed.returncode == 0 and len(self.identity(completed.stdout)) == 1,
                f"entry exec failed: {completed.returncode} {text(completed.stderr)[-400:]!r}")
        self.settle(before, 1)  # exactly one job: the own name did not recurse into a child
        before = self.job_ids()
        completed = self.brush(q(["fixture", "exec", "fixture", "pipeline", "|", "/bin/sh", "-c",
                                  'echo "PATH=$PATH"; echo "ENTRY=${MARSH_ENTRY-unset}"']),
                               self.project)
        v = self.kv(completed.stdout)
        require(v.get("ENTRY") == "unset", f"link did not unset MARSH_ENTRY: {v}")
        # GM resolution 1: the link execs the image binary by absolute path and
        # leaves PATH intact, so later registered names still route.
        require(v.get("PATH", "").startswith("/run/marsh/bin:"),
                f"entry case removed /run/marsh/bin from PATH: {v.get('PATH')!r}")
        self.settle(before, 1)
        before = self.job_ids()
        completed = self.brush(q(["fixture", "exec", "fixture", "pipeline", "|", "bash", "-c",
                                  "fixture identity; echo rc=$?"]), self.project)
        require(self.kv(completed.stdout).get("rc") == "0",
                f"the image's bash after entry did not reach the links: {text(completed.stderr)[-400:]!r}")
        receipts = self.settle(before, 2)
        self.assert_children(receipts, self.one_top(receipts, "after entry"), ["fixture"], "after entry")
        before = self.job_ids()  # control: without the marker the own name is a child (linkIgnoresMarker)
        completed = self.brush(q(["fixture", "exec", "env", "-u", "MARSH_ENTRY", "fixture", "identity"]), self.project)
        require(completed.returncode == 0, f"marker-less own name failed: {text(completed.stderr)[-400:]!r}")
        receipts = self.settle(before, 2)
        self.assert_children(receipts, self.one_top(receipts, "no marker"), ["fixture"], "no marker")
        self.verify_deleted(receipts)

    def image_tool_local(self) -> None:
        """s5 image tool case: a registered name the image ships stays local in a job of another name."""
        before = self.job_ids()
        completed = self.brush(f"{ALT} identity", self.project)
        require(completed.returncode == 0 and len(self.identity(completed.stdout)) == 1,
                f"{ALT} entry failed: {text(completed.stderr)[-400:]!r}")
        self.settle(before, 1)
        before = self.job_ids()
        completed = self.injob("fixture identity; echo a=$?; /usr/local/bin/marsh-fixture identity; echo b=$?",
                               name=ALT)
        v = self.kv(completed.stdout)
        require((v.get("a"), v.get("b")) == ("0", "0") and len(self.identity(completed.stdout)) == 2,
                f"image-provided name / absolute binary did not run locally: {v} {text(completed.stderr)[-400:]!r}")
        receipts = self.settle(before, 1)
        require(receipts[0].get("command") == ALT, f"unexpected job {receipts[0].get('command')}")
        self.verify_deleted(receipts)

    def cross_kit_default(self) -> None:
        """s6: cross-Kit spawning is allowed by default; the child runs in its own Kit's VM."""
        before = self.job_ids()
        completed = self.injob(f"shell -c {shlex.quote('echo kit-shell; echo child_job=$MARSH_JOB')}; echo rc1=$?; "
                               f"{ALT} identity; echo rc2=$?")
        v = self.kv(completed.stdout)
        require((v.get("rc1"), v.get("rc2")) == ("0", "0") and b"kit-shell" in completed.stdout,
                f"cross-Kit spawn refused without flags: {v} {text(completed.stderr)[-600:]!r}")
        receipts = self.settle(before, 3)
        top = self.one_top(receipts, "cross-Kit")
        children = self.assert_children(receipts, top, ["shell", ALT], "cross-Kit")
        shell = next(c for c in children if c.get("command") == "shell")
        require(v.get("child_job") == shell["job_id"], f"shell child MARSH_JOB {v.get('child_job')!r} is not its id")
        require(shell.get("vm_id") and shell.get("vm_id") != top.get("vm_id"), "shell child did not run in its own Kit VM")
        self.verify_deleted(receipts)

    def run_three_callers(self) -> None:
        """s3/s5: `marsh run` from the host, the shell VM, and a job: same bytes, status, stdin; lineage differs."""
        want = b'OUT\0["x"]\nin\n'
        before = self.job_ids()
        host = self.cli(["run", "fixture", "streams", "x"], self.project, stdin=b"in\n")
        require(host.returncode == 23 and host.stdout == want and b"ERR\0fixture\n" in host.stderr,
                f"host `marsh run`: {host.returncode} {host.stdout!r} {text(host.stderr)[-300:]!r}")
        session = self.brush("printf 'in\\n' | marsh run fixture streams x", self.project)
        require(session.returncode == 23 and session.stdout == want, f"session `marsh run`: {session.returncode} "
                                                                     f"{session.stdout!r}")
        outer = self.settle(before, 2)
        require(len(self.tops(outer)) == 2 and not any("job" in json.dumps(self.lin(r).get("parent")) for r in outer),
                "host and session `marsh run` jobs must both be session roots")
        before = self.job_ids()
        job = self.injob("printf 'in\\n' | marsh run fixture streams x; echo rc=$?", shell="/bin/sh")
        require(want + b"rc=23\n" in job.stdout, f"in-job `marsh run`: {job.stdout!r} {text(job.stderr)[-400:]!r}")
        receipts = self.settle(before, 2)
        self.assert_children(receipts, self.one_top(receipts, "in-job run"), ["fixture"], "in-job run")
        self.verify_deleted(outer + receipts)

    def fanout_in_job(self) -> None:
        """s1: `marsh fanout | marsh collect` from a job's real bash: shell-local branches, ordered frame;
        a registered-name branch is a child job; `-b` and the Brush sugar are not available in a job."""
        before = self.job_ids()
        fan = ("printf 'input\\n' | marsh fanout ::: copy cat ::: upper tr a-z A-Z | marsh collect; "
               "echo \"ps=${PIPESTATUS[*]}\"")
        completed = self.injob(fan)
        require(completed.stdout == FAN_FRAME + b"ps=0 0 0\n",
                f"CLI fanout in a job's bash: {completed.stdout!r} {text(completed.stderr)[-400:]!r}")
        completed = self.injob(fan, shell="/bin/bash")
        require(completed.stdout == FAN_FRAME + b"ps=0 0 0\n", f"CLI fanout via /bin/bash: {completed.stdout!r}")
        self.settle(before, 2)  # fanout branches are processes in the job: no extra jobs
        before = self.job_ids()
        completed = self.injob("marsh fanout -n ::: one fixture identity ::: two sh -c 'echo two; exit 3' "
                               "| marsh collect --json; echo \"ps=${PIPESTATUS[*]}\"")
        out = text(completed.stdout)
        document = json.loads(out.splitlines()[0]) if out.startswith("{") else {}
        labels = [b.get("label") for b in document.get("branches", [])]
        require(labels == ["one", "two"] and "ps=3 3" in out and "cwd" in document["branches"][0].get("stdout", ""),
                f"fanout with a registered-name branch: {out[-600:]!r} {text(completed.stderr)[-400:]!r}")
        receipts = self.settle(before, 2)
        self.assert_children(receipts, self.one_top(receipts, "fanout child"), ["fixture"], "fanout child")
        # A job's bash is the image's: the Brush sugar is a syntax error there,
        # and `-b` shell branches are refused like split's (s13.6).
        state, before = self.user_state(self.project), self.job_ids()
        completed = self.injob("printf 'input\\n' | fanout { copy: cat } | collect; echo sugar=$?; "
                               "marsh fanout -b a='touch p08-escape' </dev/null; echo b=$?")
        v, err = self.kv(completed.stdout), text(completed.stderr)
        require(v.get("sugar") not in (None, "0") and b"== copy" not in completed.stdout,
                f"the image's bash ran fanout sugar: {completed.stdout!r}")
        require(v.get("b") == "2" and "run only from the session shell" in err and "::: LABEL CMD" in err,
                f"fanout -b in a job was not refused clearly: {v} {err[-600:]!r}")
        require(not (self.project / "p08-escape").exists(), "a refused shell branch ran")
        self.settle(before, 1)
        self.assert_user_unchanged(self.project, state, "P08")

    def cli_split_real_bash(self) -> None:
        """s1/s7: `marsh split | marsh join` works from the image's real /bin/bash in a job; its view is the job's."""
        state, before = self.user_state(self.project), self.job_ids()
        completed = self.injob("marsh split ::: a fixture project-write a.txt A ::: k fixture project-write k.txt K "
                               "| marsh join -- cat; echo \"ps=${PIPESTATUS[*]}\"", shell="/bin/bash")
        out = text(completed.stdout)
        # A two-stage pipeline has two PIPESTATUS entries in the image's bash.
        require("ps=0 0\n" in out and "a.txt" in out and "k.txt" in out,
                f"CLI split from /bin/bash in a job: {out[-600:]!r} {text(completed.stderr)[-600:]!r}")
        receipts = self.settle(before, 3)
        top = self.one_top(receipts, "in-job split")
        for branch in (r for r in receipts if r is not top):
            require(top["job_id"] in json.dumps(self.lin(branch)), f"branch lineage lacks the creating job: "
                                                                   f"{self.lin(branch)}")
        self.assert_user_unchanged(self.project, state, "P09")
        self.verify_deleted(receipts)

    def env_i_links(self) -> None:
        """s4: a link and the in-job `marsh` start from job.json alone (`env -i`) and still spawn children."""
        before = self.job_ids()
        completed = self.injob("env -i /run/marsh/bin/fixture identity; echo rc=$?; "
                               "env -i /run/marsh/bin/marsh jobs --json >/dev/null; echo jobs=$?",
                               shell="/bin/sh")
        v = self.kv(completed.stdout)
        require((v.get("rc"), v.get("jobs")) == ("0", "0"), f"env -i links: {v} {text(completed.stderr)[-500:]!r}")
        receipts = self.settle(before, 2)
        self.assert_children(receipts, self.one_top(receipts, "env -i"), ["fixture"], "env -i")
        self.verify_deleted(receipts)

    def env_forwarding(self) -> None:
        """s6: a child gets only job-exported (set or changed) vars, never image ENV, unexported vars, or secrets."""
        before = self.job_ids()
        secrets = {"API_TOKEN": "tok-p11", "MY_SECRET": "sec-p11", "AWS_SECRET_ACCESS_KEY": "aws-p11",
                   "GITHUB_PAT": "pat-p11"}
        script = ("echo parent_image=$FIXTURE_IMAGE_ENV; echo parent_job=$MARSH_JOB; "
                  "echo \"jobjson=$(tr -d '\\n' < /run/marsh/job.json)\"; export JOB_EXPORTED=fromjob "
                  + " ".join(f"{k}={v}" for k, v in secrets.items())
                  + "; NOT_EXPORTED=local-p11; echo @@1; shell -c env; echo @@2; "
                  "export FIXTURE_IMAGE_ENV=changed-by-job; shell -c env; echo @@3")
        completed = self.injob(script)
        out = text(completed.stdout)
        v = self.kv(completed.stdout)
        require(v.get("parent_image") == "image-config",
                "fixture image lacks ENV FIXTURE_IMAGE_ENV (republish the fixture from this checkout)")
        # job.json names the starting variables but holds no plain value
        # hashes the job could test guesses against (salted by the daemon).
        document = json.loads(v.get("jobjson") or "{}")
        digest = document.get("env", {}).get("FIXTURE_IMAGE_ENV")
        plain = {hashlib.sha256(x.encode()).hexdigest() for x in
                 ("image-config", "FIXTURE_IMAGE_ENV=image-config")}
        require(digest and digest not in plain and "env_key" not in json.dumps(document),
                f"job.json exposes an unsalted digest or the key: {digest!r}")
        require("@@1" in out and "@@3" in out, f"env children failed: {out[-500:]!r} {text(completed.stderr)[-400:]!r}")
        first, second = out.split("@@1", 1)[1].split("@@2", 1)
        require("JOB_EXPORTED=fromjob\n" in first, "a job-exported variable did not reach the child")
        require("FIXTURE_IMAGE_ENV" not in first, "the parent image's ENV was forwarded to a child")
        require("local-p11" not in out, "an unexported shell variable reached a child")
        leaked = [k for k, s in secrets.items() if s in first + second]
        require(not leaked, f"credential-shaped variables crossed cap.sock: {leaked}")
        require(f"MARSH_JOB={v.get('parent_job')}\n" not in first, "child inherited the parent's MARSH_JOB")
        require("FIXTURE_IMAGE_ENV=changed-by-job\n" in second, "a variable the job changed was not forwarded")
        receipts = self.settle(before, 3)
        self.verify_deleted(receipts)

    def chain(self, kits: list[str]) -> tuple[list[str], int]:
        """Nest one registered call per level; each level reports `L<n>=<status>` on stderr."""
        leaf = {"fixture": ["fixture", "identity"], "shell": ["shell", "-c", "true"]}
        argv = leaf[kits[-1]]
        for level in range(len(kits) - 1, 0, -1):
            script = f"{q(argv)}; rc=$?; echo L{level + 1}=$rc >&2; exit $rc"
            argv = ["fixture", "pipeline", "|", "bash", "-c", script] if kits[level - 1] == "fixture" \
                else ["shell", "-c", script]
        return argv, len(kits)

    def depth_cap4(self) -> None:
        """s6 depth 4 (LineageWF): an alternating fixture/shell chain stops at depth 4 with exit 125."""
        before = self.job_ids()
        argv, _ = self.chain(["fixture", "shell"] * 3)
        completed = self.brush(q(argv), self.project, timeout=600)
        err = text(completed.stderr)
        require("depth limit 4" in err and re.search(r"^L\d=125$", err, re.M),
                f"depth refusal (125, `depth limit 4`) not surfaced: {completed.returncode} {err[-800:]!r}")
        receipts = self.settle(before, 0, exact=False, timeout=180)
        depths = sorted(self.lin(r).get("depth") for r in receipts)
        require(depths and depths[-1] == 4 and depths == list(range(depths[0], 5)),
                f"want one job per depth up to 4 and none deeper: {depths}")
        self.verify_deleted(receipts)

    def same_kit_chain(self) -> None:
        """s6 same-Kit chain 2 (SameKitBound): fixture -> fixture is allowed, a third fixture is refused."""
        before = self.job_ids()
        argv, _ = self.chain(["fixture", "fixture", "fixture"])
        completed = self.brush(q(argv), self.project, timeout=300)
        err = text(completed.stderr)
        require(re.search(r"^L3=125$", err, re.M) and "same-Kit chain limit 2" in err
                and "/run/marsh/context.md" in err, f"third same-Kit link not refused clearly: {err[-800:]!r}")
        require(re.search(r"fixture → fixture → fixture refused", err), "refusal does not name the chain")
        receipts = self.settle(before, 2)
        self.verify_deleted(receipts)

    def fan_out_cap(self) -> None:
        """s6 fan-out 4 (FanOutBound): a fifth live child of one job is refused fast; the four run."""
        gate = self.project / "p14-gate"
        before = self.job_ids()
        script = ("d=p14-gate; mkdir -p $d; for i in 1 2 3 4; do shell -c \"touch $d/r$i; sleep 20\" & done; "
                  "n=0; while [ $(ls $d | wc -l) -lt 4 ] && [ $n -lt 900 ]; do sleep 0.2; n=$((n+1)); done; "
                  "echo ready=$(ls $d | wc -l); s=$(date +%s); shell -c true; echo fifth=$?; "
                  "echo took=$(( $(date +%s) - s )); wait; echo waited")
        try:
            completed = self.injob(script, timeout=300)
        finally:
            if gate.exists():
                for path in gate.iterdir():
                    path.unlink()
                gate.rmdir()
        v = self.kv(completed.stdout)
        require(v.get("ready") == "4", f"four children did not start: {v} {text(completed.stderr)[-500:]!r}")
        require(v.get("fifth") == "125" and "fan-out limit 4" in text(completed.stderr),
                f"fifth child not refused with `fan-out limit 4`: {v} {text(completed.stderr)[-500:]!r}")
        require(int(v.get("took", "99")) <= 10, "fan-out refusal was not fail-fast")
        receipts = self.settle(before, 5)
        self.assert_children(receipts, self.one_top(receipts, "fan-out"), ["shell"] * 4, "fan-out")
        self.verify_deleted(receipts)

    def pool_fail_fast(self) -> None:
        """s6 session pool 8 (BudgetBound): a nested 9th job is refused at once with `capacity: 8 jobs`; no deadlock."""
        before = self.job_ids()
        script = ("g=.git/marsh-p15; mkdir -p $g; for i in 1 2 3 4 5 6 7; do fixture hold 45 > $g/h$i & done; "
                  "n=0; while [ $(cat $g/h* 2>/dev/null | grep -c READY) -lt 7 ] && [ $n -lt 1200 ]; do sleep 0.2; "
                  "n=$((n+1)); done; echo held=$(cat $g/h* | grep -c READY); s=$(date +%s); "
                  + q(["fixture", "pipeline", "|", "bash", "-c", "fixture identity; echo inner=$?"])
                  + "; echo outer=$?; echo took=$(( $(date +%s) - s )); wait; rm -rf $g")
        completed = self.brush(script, self.project, timeout=400)
        v = self.kv(completed.stdout)
        require(v.get("held") == "7", f"seven holds did not start: {v} {text(completed.stderr)[-500:]!r}")
        require(v.get("inner") == "125" and "capacity: 8 jobs" in text(completed.stderr),
                f"9th job not refused with `capacity: 8 jobs`: {v} {text(completed.stderr)[-600:]!r}")
        require(v.get("outer") == "0" and int(v.get("took", "999")) <= 30,
                f"parent of the refused child did not finish promptly (queueing/deadlock): {v}")
        receipts = self.settle(before, 8, timeout=180)
        self.verify_deleted(receipts)

    def spawn_narrowing(self) -> None:
        """s6 narrowing only (SpawnSetAttenuates): MARSH_SPAWN/--spawn/--no-spawn narrow; nothing widens."""
        before = self.job_ids()
        grandchild = q(["fixture", "pipeline", "|", "/bin/sh", "-c",
                        "shell -c true 2>/dev/null && echo gc_has_shell_link; marsh run shell -c true; "
                        "echo gc_run=$?"])
        script = ("shell -c true; echo byname=$?; marsh run shell -c true; echo run=$?; "
                  "MARSH_SPAWN=fixture,shell marsh run shell -c true; echo envwiden=$?; "
                  f"marsh run --spawn fixture,shell {grandchild}; echo widen=$?; fixture identity; echo self=$?")
        completed = self.injob(script, prefix="export MARSH_SPAWN=fixture; ")
        v, err = self.kv(completed.stdout), text(completed.stderr)
        require(v.get("byname") not in (None, "0"), f"a name outside the spawn set ran: {v}")
        require(v.get("run") == "125" and re.search(r"spawn", err, re.I) and "shell" in err,
                f"daemon did not refuse `marsh run shell` under MARSH_SPAWN=fixture: {v} {err[-600:]!r}")
        require(v.get("envwiden") == "125", f"MARSH_SPAWN inside a job widened the set: {v}")
        require(b"gc_has_shell_link" not in completed.stdout and v.get("gc_run") in (None, "125")
                and (v.get("gc_run") == "125" or v.get("widen") not in (None, "0")),
                f"`--spawn` widened a child's set: {v}")
        require(v.get("self") == "0", f"the job's own (allowed) name was refused: {v}")
        receipts = self.settle(before, 0, exact=False)
        require(not [r for r in receipts if r.get("command") == "shell"], "a shell job ran despite narrowing")
        for receipt in receipts:
            spawn = self.lin(receipt).get("spawn")
            require(spawn is not None and "shell" not in json.dumps(spawn), f"lineage spawn widened: {spawn}")
        before = self.job_ids()
        none = self.cli(["run", "--no-spawn", "fixture", "pipeline", "|", "/bin/sh", "-c",
                         "test -e /run/marsh/cap.sock; echo sock=$?; fixture identity; echo name=$?; "
                         "marsh run fixture identity; echo run=$?"], self.project, timeout=180)
        v = self.kv(none.stdout)
        # Review L11: --no-spawn keeps cap.sock (splits and `jobs` still work);
        # admission refuses every spawn, so the name and `marsh run` fail.
        require(v.get("sock") == "0" and v.get("name") not in (None, "0") and v.get("run") not in (None, "0"),
                f"--no-spawn left a spawn usable or removed the socket: {v} {text(none.stderr)[-400:]!r}")
        self.verify_deleted(receipts + self.settle(before, 1))

    def branch_confinement(self) -> None:
        """s7: a split branch job's child gets the branch's mounts verbatim: the fork, never the project."""
        project, state, before = self.project, self.user_state(self.project), self.job_ids()
        script = (f"fixture project-kind {project}/README.md; fixture project-write {project}/escape.txt E; "
                  "echo esc=$?; fixture project-write child.txt C; echo child=$?; fixture identity")
        completed = self.bash(q(["marsh", "split", ":::", "b", "fixture", "pipeline", "|", "bash", "-c", script])
                              + " | marsh join -- sh -c 'cat; echo; cat \"$SPLIT_DIR/b/stdout\" "
                                "\"$SPLIT_DIR/b/files\"'", project, timeout=400)
        out = text(completed.stdout)
        self.fixture_capable(out)
        require("missing" in out and "child=0" in out and re.search(r"esc=[1-9]", out),
                f"branch child saw or wrote the project: {out[-800:]!r} {text(completed.stderr)[-400:]!r}")
        require("A\tchild.txt" in out, "capture did not wait for the branch's child (files lacks child.txt)")
        ids = self.identity(completed.stdout)
        require(ids and ids[-1]["cwd"].startswith(f"{project}/.marsh/split/"), f"child cwd not in the fork: {ids}")
        self.assert_user_unchanged(project, state, "P17")
        receipts = self.settle(before, 5)
        branch = self.one_top(receipts, "branch")
        children = self.assert_children(receipts, branch, ["fixture"] * 4, "branch children")
        for child in children:
            require(child.get("mounts") == branch.get("mounts") and branch.get("mounts"),
                    f"child mounts are not the branch's verbatim: {child.get('mounts')} vs {branch.get('mounts')}")
            targets = [m.get("target") for m in child.get("mounts", [])]
            require(str(project) not in targets, f"branch child mounts the project: {targets}")
        self.verify_deleted(receipts)

    def tty_child_refused(self) -> None:
        """s6 no TTY children: a link whose stdin/stdout are terminals is refused with the stated message."""
        before = self.job_ids()
        master, slave = pty.openpty()

        def controlling() -> None:
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        process = subprocess.Popen([self.marsh, "-c", q(["fixture", "pipeline", "|", "fixture", "identity"])
                                    + "; echo rc=$?"], cwd=self.project, env=self.host_env, stdin=slave,
                                   stdout=slave, stderr=slave, preexec_fn=controlling)
        os.close(slave)
        output, deadline = bytearray(), time.monotonic() + 240
        try:
            while time.monotonic() < deadline:
                ready, _, _ = select.select([master], [], [], 0.5)
                if ready:
                    try:
                        chunk = os.read(master, 65536)
                    except OSError:
                        break
                    if not chunk:
                        break
                    output.extend(chunk)
                elif process.poll() is not None:
                    break
        finally:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=30)
            os.close(master)
        self.records.append({"caller": "pty", "status": process.returncode, "output": text(bytes(output))[-4000:]})
        blob = text(bytes(output))
        require("interactive child jobs are not supported yet" in blob and "</dev/null" in blob
                and re.search(r"rc=[1-9]", blob),
                f"TTY child not refused with the stated message: {blob[-600:]!r}")
        self.verify_deleted(self.settle(before, 1))

    def ctrl_c_tree(self) -> None:
        """s8 Ctrl-C: SIGINT to the root caller cancels the whole tree across Kit VMs with verified deletion."""
        before = self.job_ids()
        process = self.spawn([self.marsh, "-c", q(["fixture", "pipeline", "|", "bash", "-c",
                                                   "shell -c 'sleep 600' & fixture hold 600 & wait"])], self.project)
        try:
            self.wait_until(lambda: len(self.running(before)) >= 3, 240, "parent and two children running", (process,))
            os.killpg(process.pid, signal.SIGINT)
            began = time.monotonic()
            status, _, _ = self.reap(process, 60)
            elapsed = time.monotonic() - began
        finally:
            self.kill(process)
        require(status != 0, f"interrupted root exited {status}")
        require(elapsed <= 25, f"Ctrl-C took {elapsed:.1f}s (> INT, 10 s, KILL)")
        receipts = self.settle(before, 3, timeout=90)
        require(len({r.get("vm_id") for r in receipts}) >= 2, "tree did not span two Kit VMs")
        self.verify_deleted(receipts)

    def parent_exit_cascade(self) -> None:
        """s8 job end: a parent that exits while its child runs cancels the child (no orphan)."""
        before, began = self.job_ids(), time.monotonic()
        completed = self.brush(q(["fixture", "pipeline", "|", "--exit-after", "15", "shell", "-c", "sleep 600"]),
                               self.project, timeout=240)
        require(completed.returncode == 7, f"parent status {completed.returncode} != 7")
        receipts = self.settle(before, 2, timeout=90)
        require(time.monotonic() - began < 120, "child outlived its parent beyond the cancel bound")
        self.verify_deleted(receipts)

    def daemon_restart_tree(self) -> None:
        """s8 restart (RestartUncertain, NoReplay): a live tree becomes uncertain; nothing is resubmitted."""
        before = self.job_ids()
        process = self.spawn([self.marsh, "-c", q(["fixture", "pipeline", "|", "fixture", "hold", "600"])], self.project)
        try:
            self.wait_until(lambda: len(self.running(before)) >= 2, 240, "parent and child running", (process,))
            old_pid = self.owned_daemon_pid
            require(old_pid and stable_process_identity(old_pid) == self.owned_daemon_process_identity,
                    "cannot verify the owned daemon process before restarting it")
            os.kill(old_pid, signal.SIGKILL)
            self.wait_until(lambda: stable_process_identity(old_pid) is None, 30, "daemon exit")
            restarted = time.time() * 1000
            status, _, stderr = self.reap(process, 90)
        finally:
            self.kill(process)
        require(status not in (0, None), f"root client reported success across a restart: {status}")
        for field in ("owned_daemon_id", "owned_daemon_pid", "owned_daemon_process_identity",
                      "owned_daemon_control_token"):
            setattr(self, field, None)
        self.document("status", "--json")
        time.sleep(10)
        fresh = [self.show(j) for j in self.job_ids() - before]
        require(len(fresh) == 2, f"want the 2 original jobs and no replay: {[r.get('state') for r in fresh]}")
        for receipt in fresh:
            require(re.search(r"uncertain|daemon_restarted", json.dumps(receipt)),
                    f"job not uncertain after restart: {receipt.get('state')} {receipt.get('cleanup')}")
            require((receipt.get("created_unix_ms") or 0) <= restarted, "a job was created after the restart")
        reset = self.cli(["workers", "reset", "fixture"], self.project, timeout=180)
        require(reset.returncode == 0, f"workers reset failed: {text(reset.stderr)[-400:]!r}")
        self.records.append({"scenario": "P21", "client_stderr": text(stderr)[-2000:]})

    def cap_socket_cap(self) -> None:
        """s3 socket fixes: >= 8 concurrent connections admitted; excess ones get a refusal frame, not a drop."""
        before = self.job_ids()
        completed = self.injob("/usr/local/bin/marsh-fixture cap-flood 24; fixture identity; echo after=$?",
                               shell="/bin/sh")
        lines = [line for line in text(completed.stdout).splitlines() if line.startswith("[")]
        require(lines, f"cap-flood produced no report: {text(completed.stderr)[-400:]!r}")
        rows = json.loads(lines[0])
        message = "too many concurrent daemon requests in this job"
        refused = [r for r in rows if message in r["read"]]
        admitted = [r for r in rows if r["connect"] == "ok" and not r["eof"] and message not in r["read"]]
        silent = [r["i"] for r in rows if r not in refused and r not in admitted]
        self.records.append({"scenario": "P22", "admitted": len(admitted), "refused": len(refused), "silent": silent})
        require(len(admitted) >= 8, f"fewer than 8 concurrent connections admitted: {len(admitted)}")
        require(refused, "no excess connection received the refusal frame")
        require(not silent, f"connections dropped or refused without the frame: {silent}")
        require(self.kv(completed.stdout).get("after") == "0", "the job could not spawn after the flood")
        self.verify_deleted(self.settle(before, 2))

    def awareness_context(self) -> None:
        """s9: `marsh context` prints (never writes) the job context; job.json/context.md; status `nested`."""
        state, home = self.user_state(self.project), self.tree_state(self.guest_home)
        host = self.cli(["context"], self.project, timeout=60)
        session = self.brush("marsh context", self.project)
        body = text(host.stdout)
        require(host.returncode == 0 and "marsh run" in body and "MARSH_SPAWN" in body,
                f"`marsh context` lacks run/spawn guidance: {host.returncode} {body[:400]!r}")
        require(session.stdout == host.stdout, "session and host `marsh context` differ")
        before = self.job_ids()
        job = self.injob("echo @@1; marsh context; echo @@2; cat /run/marsh/context.md; echo @@3", shell="/bin/sh")
        out = text(job.stdout)
        require("@@3" in out, f"in-job context failed: {text(job.stderr)[-400:]!r}")
        printed, filed = out.split("@@1\n", 1)[1].split("@@2\n", 1)
        filed = filed.split("@@3", 1)[0]
        require(printed == filed and "marsh run" in filed, "in-job `marsh context` != /run/marsh/context.md")
        self.assert_user_unchanged(self.project, state, "P23")
        require(self.tree_state(self.guest_home) == home, "`marsh context` wrote into the selected home")
        require(not any(p.name in ("CLAUDE.md", "AGENTS.md") for root in (self.project, self.guest_home)
                        for p in root.rglob("*")), "marsh wrote CLAUDE.md/AGENTS.md")
        require(find_records(self.document("status", "--json"), lambda d: "processes" in d),
                "status lacks `processes`")
        self.verify_deleted(self.settle(before, 1))

    def real_bash_startup(self) -> None:
        """s4: PATH `bash` is the image's own (no wrapper: p50 within 2 ms of /bin/bash); the static
        artifact (`marsh`, links) starts within 20 ms."""
        argv = ["fixture", "bench", "40", "|", "bash", "-c", "true", "|", "/bin/bash", "-c", "true",
                "|", "/run/marsh/bin/marsh", "--help", "|", "/bin/true"]
        self.brush(q(["fixture", "bench", "3", *argv[3:]]) + " >/dev/null", self.project)  # warm the page cache
        completed = self.brush(q(argv), self.project, timeout=300)
        self.fixture_capable(completed.stderr)
        rows = json.loads(text(completed.stdout).strip().splitlines()[-1])
        p50 = [statistics.median(row["us"]) for row in rows]
        self.records.append({"scenario": "P24", "p50_us": p50, "failed": [row["failed"] for row in rows]})
        require(all(row["failed"] == 0 for row in rows), f"a benchmarked command failed: {rows}"[:600])
        require(abs(p50[0] - p50[1]) <= STARTUP_BUDGET_US,
                f"bash -c true p50 {p50[0]:.0f}us vs /bin/bash {p50[1]:.0f}us: PATH bash is not the image's")
        require(p50[2] - p50[3] <= LINK_BUDGET_US, f"marsh --help p50 {p50[2]:.0f}us > /bin/true {p50[3]:.0f}us + 20 ms")

    def child_after_first_split(self) -> None:
        """s7: a job started before the project's first split still spawns children after it splits."""
        project = self.new_repo("p25-project", {"README.md": "p25\n"})  # no .marsh yet
        state, before = self.user_state(project), self.job_ids()
        script = ("test -e .marsh && echo pre=yes || echo pre=no; "
                  "marsh split ::: k fixture identity | marsh join -- cat >/dev/null; echo split=$?; "
                  "test -d .marsh && echo post=yes || echo post=no; fixture identity; echo child=$?")
        completed = self.brush(q(["fixture", "pipeline", "|", "/bin/sh", "-c", script]), project)
        self.fixture_capable(completed.stderr)
        v = self.kv(completed.stdout)
        require((v.get("pre"), v.get("split"), v.get("post"), v.get("child")) == ("no", "0", "yes", "0"),
                f"child refused after the project's first split: {v} {text(completed.stderr)[-600:]!r}")
        receipts = self.settle(before, 3)
        top = self.one_top(receipts, "after first split")
        self.assert_children(receipts, top, ["fixture", "fixture"], "after first split")
        child = next(r for r in receipts if r is not top and not self.lin(r).get("split"))
        require(all(m in child.get("mounts", []) for m in top.get("mounts", [])),
                f"child view is not its parent's: {child.get('mounts')} vs {top.get('mounts')}")
        self.assert_user_unchanged(project, state, "P25")
        self.verify_deleted(receipts)

    def no_shell_branch_from_job(self) -> None:
        """s13.6 (B2): a job's split takes argv branches only; the daemon refuses shell branches (-b)."""
        state, before = self.user_state(self.project), self.job_ids()
        splits = {d.get("id") for d in find_records(self.splits(self.project), lambda d: "id" in d)}
        script = ("marsh split -b a='touch p27-escape' </dev/null >/dev/null; echo b=$?; "
                  "marsh split -b a='touch p27-escape' ::: k fixture identity </dev/null >/dev/null; echo mixed=$?")
        completed = self.injob(script)
        v, err = self.kv(completed.stdout), text(completed.stderr)
        require(all(v.get(k) not in (None, "0") for k in ("b", "mixed")),
                f"a shell branch from a job was admitted: {v} {err[-600:]!r}")
        require(err.count("run only from the session shell") >= 2 and "::: a CMD" in err,
                f"refusal does not explain argv branches: {err[-800:]!r}")
        require(not (self.project / "p27-escape").exists(), "a refused shell branch ran")
        self.settle(before, 1)  # no branch job, no session shell job
        after = {d.get("id") for d in find_records(self.splits(self.project), lambda d: "id" in d)}
        require(after == splits, f"a refused split left a record: {after - splits}")
        self.assert_user_unchanged(self.project, state, "P27")

    def env_whole_tree(self) -> None:
        """s6 (B3): a root export reaches every depth; a child's re-export of the same value still flows."""
        before = self.job_ids()
        leaf = "echo d3=${P28_ROOT-unset}:${P28_MID-unset}:${API_TOKEN-unset}"
        middle = (f"echo d2=${{P28_ROOT-unset}}; export P28_ROOT=from-root P28_MID=mid; shell -c {shlex.quote(leaf)}")
        script = f"echo d1=${{P28_ROOT-unset}}; export P28_ROOT=from-root; shell -c {shlex.quote(middle)}"
        completed = self.brush("export P28_ROOT=from-root API_TOKEN=tok-p28; "
                               + q(["fixture", "pipeline", "|", "bash", "-c", script]), self.project, timeout=400)
        v = self.kv(completed.stdout)
        require((v.get("d1"), v.get("d2"), v.get("d3")) == ("from-root", "from-root", "from-root:mid:unset"),
                f"exported variables did not reach the whole tree: {v} {text(completed.stderr)[-600:]!r}")
        self.verify_deleted(self.settle(before, 3, timeout=180))

    def interrupt_and_time(self, script: str, count: int, what: str) -> list[dict[str, Any]]:
        before = self.job_ids()
        process = self.spawn([self.marsh, "-c", script], self.project)
        try:
            self.wait_until(lambda: len(self.running(before)) >= count, 300, f"{what}: tree running", (process,))
            time.sleep(2)
            os.killpg(process.pid, signal.SIGINT)
            began = time.monotonic()
            status, _, stderr = self.reap(process, 90)
            elapsed = time.monotonic() - began
        finally:
            self.kill(process)
        self.records.append({"scenario": "P29", "what": what, "status": status, "seconds": round(elapsed, 2),
                             "stderr": text(stderr)[-2000:]})
        require(elapsed < 15, f"{what}: root Ctrl-C returned after {elapsed:.1f}s (want < 15 s)")
        require(status in (130, -signal.SIGINT), f"{what}: interrupted root exited {status}, want 130")
        receipts = self.settle(before, count, timeout=120)
        states = [(r.get("command"), r.get("state"), (r.get("exit") or {}).get("code")) for r in receipts]
        require(all(r.get("state") == "cancelled" for r in receipts), f"{what}: tree not recorded cancelled: {states}")
        self.verify_deleted(receipts)
        return receipts

    def root_interrupt_cascade(self) -> None:
        """s8 (B4): Ctrl-C at the root cancels the whole tree at once (< 15 s), all `cancelled`, root 130."""
        inner = "shell -c 'sleep 300' </dev/null | cat"
        self.interrupt_and_time(q(["shell", "-c", inner]), 2, "shell -> shell")
        self.interrupt_and_time(q(["fixture", "pipeline", "|", "bash", "-c",
                                   "shell -c 'shell -c \"sleep 300\" </dev/null | cat'"]), 3, "fixture -> shell -> shell")

    def inspection_text(self) -> None:
        """s9 text forms: `jobs --tree` (exit, duration), `jobs show` (lineage, children), results, status."""
        before = self.job_ids()
        completed = self.injob("fixture identity >/dev/null; echo rc=$?")
        require(self.kv(completed.stdout).get("rc") == "0", f"child failed: {text(completed.stderr)[-400:]!r}")
        receipts = self.settle(before, 2)
        top = self.one_top(receipts, "inspection")
        kid = next(r for r in receipts if r is not top)
        tree = text(self.cli(["jobs", "--tree"], self.project, timeout=60).stdout)
        require(re.search(rf"^{top['job_id'][:8]} +\d+[smhd] ago +finished +0 +[\d.]+s  "
                          rf"fixture pipeline \"\|\" bash -c ['\"]fixture identity", tree, re.M)
                and re.search(rf"^{kid['job_id'][:8]} +\d+[smhd] ago +finished +0 +[\d.]+s  "
                              rf"└─ fixture identity$", tree, re.M),
                f"`jobs --tree` text lacks start, state, exit, duration, command, or nesting: {tree[:800]!r}")
        shown = text(self.cli(["jobs", "show", kid["job_id"][:8]], self.project, timeout=60).stdout)
        require(kid["job_id"] in shown, f"`jobs show SHORT_ID` does not find the job: {shown[:400]!r}")
        shown = text(self.cli(["jobs", "show", kid["job_id"]], self.project, timeout=60).stdout)
        require(f"parent:     job:{top['job_id']}" in shown and "depth:      2" in shown,
                f"`jobs show CHILD` text lacks lineage: {shown!r}")
        shown = text(self.cli(["jobs", "show", top["job_id"]], self.project, timeout=60).stdout)
        require(f"children:   {kid['job_id']}" in shown, f"`jobs show PARENT` text lacks children: {shown!r}")
        results = text(self.cli(["results"], self.project, timeout=60).stdout)
        row = next((line for line in results.splitlines() if kid["job_id"] in line), "")
        require(row.rstrip().endswith(top["job_id"][:8]), f"`results` row lacks the parent: {row!r}")
        status = text(self.cli(["status"], self.project, timeout=60).stdout)
        require(re.search(r"processes:\s+\d+ running, \d+ refused, \d+ held", status), f"status text: {status!r}")
        shells = len(self.document("status", "--json").get("shells", []))
        for _ in range(3):
            self.brush("true", self.project)
        after = len(self.document("status", "--json").get("shells", []))
        require(after <= shells, f"ended `marsh -c` shells accumulate in status: {shells} -> {after}")
        # A refused `marsh run` reports on stderr only, before the caller's next output.
        refused = self.brush("marsh run --no-spawn nope 2>&1; echo after", self.project)
        lines = text(refused.stdout).splitlines()
        require(lines and lines[-1] == "after" and len(lines) >= 2 and "nope: not a registered command" in lines[0],
                f"refused `marsh run` output order: {lines!r}")
        before = self.job_ids()
        refused = self.injob("echo first; marsh run nope x 2>&1; echo after=$?", shell="/bin/sh")
        lines = text(refused.stdout).splitlines()
        require(len(lines) >= 3 and lines[0] == "first" and lines[-1] == "after=125"
                and "nope: not a registered command" in lines[1] and "nope" not in text(refused.stderr),
                f"in-job refused `marsh run` output order: {lines!r} {text(refused.stderr)!r}")
        receipts += self.settle(before, 1)
        typed = text(self.brush("type bash; bash -c true; type bash", self.project).stdout).splitlines()
        require(len(typed) == 2 and typed[0].startswith("bash is /") and typed[1].startswith("bash is hashed (/"),
                f"session `type bash` is not GNU-worded: {typed!r}")
        self.verify_deleted(receipts)

    def fanout_cli(self) -> None:
        """s1: `marsh fanout | marsh collect` from host bash and in a session: concurrent branches on the shared
        workspace, ordered output, first failing status; the same frame as the Brush sugar."""
        fan = "printf 'input\\n' | marsh fanout ::: copy cat ::: upper tr a-z A-Z | marsh collect; echo \"ps=${PIPESTATUS[*]}\""
        before = self.job_ids()
        host = self.bash(fan, self.project)
        require(host.stdout == FAN_FRAME + b"ps=0 0 0\n",
                f"host CLI fanout: {host.stdout!r} {text(host.stderr)[-600:]!r}")
        usage = self.cli(["fanout", ":::", "a"], self.project, timeout=60)
        require(usage.returncode == 2 and b"has no command" in usage.stderr, f"fanout usage: {usage.stderr!r}")
        shared = self.project / "p38-shared.txt"
        mixed = self.bash("marsh fanout -n -b w='printf shared > p38-shared.txt; echo wrote; echo quiet >&2' "
                          "::: id fixture identity "
                          "::: bad sh -c 'echo oops >&2; exit 4' | marsh collect --timing; "
                          "echo \"ps=${PIPESTATUS[*]}\"", self.project)
        out, err = text(mixed.stdout), text(mixed.stderr)
        try:
            require("ps=4 4" in out and "== w (complete) ==\nwrote" in out and "== id (complete) ==" in out
                    and "== bad (failed: 4) ==" in out and "Timing:" in out and '"cwd"' in out,
                    f"host fanout with -b and a registered name: {out[-800:]!r} {err[-400:]!r}")
            require(out.index("== w ") < out.index("== id ") < out.index("== bad "), "branches out of order")
            # fanout.md: per branch, header, stdout, then a failed branch's stderr (join-consistent);
            # a successful branch's stderr shows only with --stderr.
            require("== bad (failed: 4) ==\n\n== bad stderr ==\noops\n" in out,
                    f"failed branch stderr not after its header: {out[-800:]!r} {err[-400:]!r}")
            require("quiet" not in out and "quiet" not in err and "oops" not in err,
                    f"successful branch stderr shown by default: {out[-800:]!r} {err[-400:]!r}")
            loud = self.bash("marsh fanout -n -b w='echo wrote; echo quiet >&2' | marsh collect --stderr",
                             self.project)
            require(text(loud.stdout) == "== w (complete) ==\nwrote\n== w stderr ==\nquiet\n\n",
                    f"collect --stderr: {loud.stdout!r} {text(loud.stderr)[-400:]!r}")
            require(shared.is_file() and shared.read_text() == "shared",
                    "a -b branch did not write the shared workspace (fanout never forks)")
        finally:
            shared.unlink(missing_ok=True)
        receipts = self.settle(before, 1)
        require(receipts[0].get("command") == "fixture" and str(self.lin(receipts[0]).get("parent", "")).startswith(
            "session"), f"the fixture branch is not a session root job: {self.lin(receipts[0])}")
        # `jobs --tree` groups a fanout's jobs under one fanout node (like split).
        require(re.fullmatch(r"[0-9a-f]{16}/id", str(self.lin(receipts[0]).get("fanout", ""))),
                f"the fanout branch job lacks fanout lineage: {self.lin(receipts[0])}")
        tree = text(self.cli(["jobs", "--tree", "--all"], self.project, timeout=60).stdout)
        require(re.search(r"^[0-9a-f]{8}  .*fanout \(id\)\n" + receipts[0]["job_id"][:8]
                          + r"  .*└─ id: fixture identity$", tree, re.M),
                f"`jobs --tree` does not group the fanout job under a fanout node: {tree[-1200:]!r}")
        session = self.brush("printf 'input\\n' | fanout { copy: cat; upper: tr a-z A-Z } | collect; echo @@; "
                             + fan + "; marsh fanout -b s='echo $((6*7))' </dev/null | marsh collect --json",
                             self.project)
        parts = session.stdout.split(b"@@\n", 1)
        require(len(parts) == 2 and parts[0] == FAN_FRAME and parts[1].startswith(FAN_FRAME + b"ps=0 0 0\n"),
                f"session fanout CLI != sugar: {session.stdout!r} {text(session.stderr)[-600:]!r}")
        document = json.loads(parts[1].split(b"ps=0 0 0\n", 1)[1])
        require([(b["label"], b["status"], b["stdout"]) for b in document["branches"]] == [("s", 0, "42\n")],
                f"session -b branch: {document!r}")
        self.verify_deleted(receipts)

    def split_lineage_tree(self) -> None:
        """s9: in the session, `jobs --tree` draws its split, each branch, and every job a branch's shell
        started (and their children); the join consumer's job names the split it consumed."""
        before = self.job_ids()
        script = ("split {\n  fix: fixture pipeline '|' bash -c 'fixture identity >/dev/null'\n"
                  "  review: fixture identity >/dev/null\n} | join | fixture identity >/dev/null\n"
                  "echo rc=$?\nmarsh jobs --tree\n")
        completed = self.brush(script, self.project, timeout=400)
        out = text(completed.stdout)
        self.fixture_capable(out, completed.stderr)
        require("rc=0" in out, f"split pipeline failed: {out[-800:]!r} {text(completed.stderr)[-800:]!r}")
        receipts = self.settle(before, 4)
        fix = next(r for r in receipts if r.get("args", [""])[0] == "pipeline")
        nested = [r for r in receipts if self.parent_is(r, fix)]
        require(len(nested) == 1, f"the fix branch job's bash child is not its child: {receipts!r}"[:900])
        split = (self.lin(fix).get("split") or "")
        require(self.lin(fix).get("parent") == f"split:{split}/fix" and self.lin(fix).get("label") == "fix",
                f"a shell branch's Kit job must have the branch as parent: {self.lin(fix)}")
        rest = [r for r in receipts if r is not fix and r is not nested[0]]
        review = [r for r in rest if self.lin(r).get("parent") == f"split:{split}/review"]
        consumer = [r for r in rest if self.lin(r).get("consumes") == split]
        require(len(review) == 1 and len(consumer) == 1,
                f"want one review branch job and one consumer naming split {split}: "
                f"{[self.lin(r) for r in rest]}")
        tree = out.split("rc=0", 1)[1]
        ago = r"\d+[smhd] ago +"
        want = [
            rf"^{split[:8]} +{ago}joined +0 +\S+  split \(fix, review\)$",
            rf"^{split[:8]}/fix +{ago}finished +0 +\S+  ├─ fix: fixture pipeline",
            rf"^{fix['job_id'][:8]} +{ago}finished +0 +\S+  │  └─ fixture pipeline",
            rf"^{nested[0]['job_id'][:8]} +{ago}finished +0 +\S+  │     └─ fixture identity$",
            rf"^{split[:8]}/review +{ago}finished +0 +\S+  └─ review: fixture identity",
            rf"^{review[0]['job_id'][:8]} +{ago}finished +0 +\S+     └─ fixture identity$",
            rf"^{consumer[0]['job_id'][:8]} +{ago}finished +0 +\S+  fixture identity  \(consumes split {split[:8]}\)$",
        ]
        for pattern in want:
            require(re.search(pattern, tree, re.M), f"`jobs --tree` lacks {pattern!r}:\n{tree}")
        rows = [line for line in tree.splitlines() if line[:1] not in ("", "I")]
        start = next(i for i, line in enumerate(rows) if " split (fix, review)" in line)
        require([row.split()[0] for row in rows[start + 1:start + 6]]
                == [f"{split[:8]}/fix", fix["job_id"][:8], nested[0]["job_id"][:8],
                    f"{split[:8]}/review", review[0]["job_id"][:8]],
                f"split subtree rows out of order:\n{tree}")
        self.verify_deleted(receipts)

    def agents_doc_examples(self) -> None:
        """docs/agents.md: every marked example runs as written (agents replaced by fixture callers)."""
        import doc_examples
        guide = pathlib.Path(__file__).resolve().parents[2] / "docs" / "agents.md"
        failures = []
        found = doc_examples.examples(guide)
        require(found, "docs/agents.md has no doc-test examples")
        for number, example in enumerate(found):
            project = self.new_repo(f"p31-{number}", {"notes.txt": "alpha\nbeta\n"})
            run = self.bash if example.kind == "host" else self.brush
            completed = run(example.script, project, timeout=400)
            out, err = text(completed.stdout), text(completed.stderr)
            self.fixture_capable(out, err)
            if completed.returncode != example.status or example.stdout not in out or example.stderr not in err:
                failures.append(f"{example.name()}: status {completed.returncode} (want {example.status}); "
                                f"stdout={out[-500:]!r} stderr={err[-700:]!r}")
            for split in self.splits(project).get("splits", []):
                if split.get("root") == str(project) and split.get("state") != "run":
                    self.cli(["splits", "rm", split["id"]], project, timeout=60)
        require(not failures, "agents.md examples failed:\n" + "\n".join(failures))

    def live_claude_codex(self) -> None:
        """Billed (--live): a real Claude Code Bash-tool turn runs bash and a codex child job, with lineage."""
        before = self.job_ids()
        prompt = ("use your Bash tool to run: echo hi-from-bash; then run: codex exec 'reply with the number 7'. "
                  "Report both outputs.")
        completed = self.brush(q(["claude", "-p", prompt]) + " </dev/null", self.project, timeout=900)
        out, err = text(completed.stdout), text(completed.stderr)
        self.records.append({"scenario": "P32", "status": completed.returncode, "stdout": out[-4000:],
                             "stderr": err[-4000:]})
        require(completed.returncode == 0 and "hi-from-bash" in out and "7" in out,
                f"claude turn failed: {completed.returncode} {out[-800:]!r} {err[-800:]!r}")
        require("CLAUDE_CODE_SHELL" not in json.dumps(self.environment), "the CLAUDE_CODE_SHELL workaround is set")
        receipts = self.settle(before, 0, exact=False, timeout=300)
        top = self.one_top(receipts, "claude")
        require(top.get("command") == "claude", f"root is not claude: {top.get('command')}")
        codex = [r for r in receipts if r.get("command") == "codex" and self.parent_is(r, top)]
        require(codex, f"codex did not run as claude's child: {[(r.get('command'), self.lin(r)) for r in receipts]}")
        tree = text(self.cli(["jobs", "--tree"], self.project, timeout=60).stdout)
        require(re.search(rf"^{top['job_id'][:8]} .* claude -p .*\n(?:.*\n)*?{codex[0]['job_id'][:8]} .*[├└]─ codex ",
                          tree, re.M),
                f"`jobs --tree` does not show codex under claude: {tree[:800]!r}")
        self.records.append({"scenario": "P32", "tree": tree})
        self.verify_deleted(receipts)

    def jobs_listing_scope(self) -> None:
        """s9 listings: in a session `jobs`/`jobs --tree` show only that session's trees, `--all` every one."""
        before = self.job_ids()
        self.brush("fixture identity >/dev/null", self.project)
        other = self.settle(before, 1)[0]
        before = self.job_ids()
        listed = text(self.brush("fixture identity >/dev/null; echo ==tree; marsh jobs --tree; echo ==flat; "
                                 "marsh jobs; echo ==all; marsh jobs --tree --all; echo ==json; "
                                 "marsh jobs --tree --json", self.project, timeout=300).stdout)
        mine = self.settle(before, 1)[0]
        parts = dict(re.findall(r"^==(\w+)\n(.*?)(?=^==|\Z)", listed, re.M | re.S))
        require(set(parts) == {"tree", "flat", "all", "json"}, f"listing sections missing: {listed[-1200:]!r}")
        for name in ("tree", "flat"):
            section = parts[name]
            require(re.match(r"ID +STARTED   STATE", section), f"`jobs` {name} has no header: {section!r}")
            require(re.search(rf"^{mine['job_id'][:8]} +\d+s ago +finished +0 .*fixture identity$", section, re.M),
                    f"session `jobs` {name} lacks this session's job: {section!r}")
            require(other["job_id"][:8] not in section, f"session `jobs` {name} lists another session's job: {section!r}")
        require(mine["job_id"][:8] in parts["all"] and other["job_id"][:8] in parts["all"],
                f"`jobs --tree --all` is not the full forest: {parts['all']!r}")
        document = json.loads(parts["json"])
        # Roots are job, split, and fanout nodes (`"node": "split"`, s13 formats).
        jobs = [n for n in document["jobs"] if n.get("node", "job") == "job"]
        require(all(n.get("node") in ("job", "split", "fanout") for n in document["jobs"]),
                f"`jobs --tree --json` root of unknown kind: {document['jobs']!r}"[:600])
        ids = {node["job_id"] for node in jobs}
        require({mine["job_id"], other["job_id"]} <= ids, "`jobs --tree --json` is not the full, unscoped forest")
        node = next(n for n in jobs if n["job_id"] == mine["job_id"])
        require(node.get("args") == ["identity"] and node.get("session_id") == mine["session_id"],
                f"tree node lacks args/session_id: {node!r}")
        host = text(self.cli(["jobs", "--tree"], self.project, timeout=60).stdout)
        require(mine["job_id"][:8] in host and other["job_id"][:8] in host,
                f"host `jobs --tree` lacks the last hour's trees: {host!r}")
        self.verify_deleted([other, mine])

    def pty_session(self) -> tuple[subprocess.Popen[bytes], int]:
        """An interactive `marsh` on a fresh controlling terminal."""
        master, slave = pty.openpty()

        def controlling() -> None:
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        process = subprocess.Popen([self.marsh], cwd=self.project, env={**self.host_env, "TERM": "xterm"},
                                   stdin=slave, stdout=slave, stderr=slave, preexec_fn=controlling)
        os.close(slave)
        self.live.append(process)
        return process, master

    @staticmethod
    def pty_expect(master: int, output: bytearray, pattern: str, timeout: float, start: int = 0) -> re.Match[str]:
        """Read (answering cursor queries) until `pattern` appears in output[start:]."""
        deadline = time.monotonic() + timeout
        while True:
            found = re.search(pattern, text(bytes(output[start:])))
            if found:
                return found
            if time.monotonic() > deadline:
                raise Fail(f"terminal never showed {pattern!r}: {text(bytes(output))[-1500:]!r}")
            ready, _, _ = select.select([master], [], [], 0.25)
            if ready:
                try:
                    chunk = os.read(master, 65536)
                except OSError:
                    chunk = b""
                if not chunk:
                    raise Fail(f"terminal closed before {pattern!r}: {text(bytes(output))[-1500:]!r}")
                if b"\x1b[6n" in chunk:
                    os.write(master, b"\x1b[1;1R")
                output.extend(chunk)

    @staticmethod
    def pty_drain(master: int, output: bytearray, stop: threading.Event) -> None:
        """Read (answering cursor queries) until `stop` is set."""
        while not stop.is_set():
            ready, _, _ = select.select([master], [], [], 0.25)
            if ready:
                try:
                    chunk = os.read(master, 65536)
                except OSError:
                    return
                if not chunk:
                    return
                if b"\x1b[6n" in chunk:
                    os.write(master, b"\x1b[1;1R")
                output.extend(chunk)

    def background_cold_notice(self) -> None:
        """Interactive: `NAME &` prints Bash's `[1] PID`, and a background job's cold start is silent;
        a foreground job still prints `[starting NAME worker VM…]`."""
        # Both Kits cold: retire their workers once no earlier session holds them.
        self.wait_until(lambda: not [s for s in self.document("status", "--json").get("shells", [])
                                     if s.get("state") == "attached"], 120, "earlier sessions detached")
        for kit in ("fixture", "shell"):
            reset = self.cli(["workers", "reset", kit], self.project, timeout=600)
            require(reset.returncode == 0, f"workers reset {kit} failed: {text(reset.stderr)[-400:]!r}")
        before = self.job_ids()
        process, master = self.pty_session()
        output = bytearray()
        try:
            self.pty_expect(master, output, r"marsh-[\d.]+\$ ", 600)
            mark = len(output)
            os.write(master, b"shell -c true &\r")
            self.pty_expect(master, output, r"\[1\] \d+\r\n", 60)
            # Keep reading (and answering the prompt's cursor query) while the
            # job settles: an unanswered query turns line editing off.
            stop = threading.Event()
            reader = threading.Thread(target=self.pty_drain, args=(master, output, stop), daemon=True)
            reader.start()
            try:
                self.settle(before, 1, timeout=600)
            finally:
                stop.set()
                reader.join()
            os.write(master, b"\r")
            self.pty_expect(master, output, r"\[1\]\+  Done {20,}shell -c true", 60)
            background = text(bytes(output[mark:]))
            require("worker VM" not in background,
                    f"a background job printed the cold-start notice over the prompt: {background[-1200:]!r}")
            require(not re.search(r"\[1\]\+\s+\d+", background), f"Brush-style `[1]+ PID` line: {background!r}")
            mark = len(output)
            os.write(master, b"fixture identity >/dev/null; echo fg=$?\r")
            found = self.pty_expect(master, output, r"fg=0", 600, mark)
            foreground = text(bytes(output[mark:]))
            self.pty_expect(master, output, r"marsh-[\d.]+\$ ", 60, mark + found.end())
            require("[starting fixture worker VM…]" in foreground,
                    f"a foreground cold start lost its notice: {foreground[-1200:]!r}")
            os.write(master, b"exit\r")
            self.reap_pty(process, master, output)
        finally:
            if process.poll() is None:  # leave no attached session behind
                try:
                    os.write(master, b"\x03exit\r")
                    process.wait(timeout=60)
                except (OSError, subprocess.TimeoutExpired):
                    pass
            self.kill(process)
            try:
                os.close(master)
            except OSError:
                pass
            self.records.append({"scenario": "P34", "output": text(bytes(output))[-6000:]})
        self.verify_deleted(self.settle(before, 2, timeout=120))

    def restart_daemon(self, settings: dict[str, str | None]) -> None:
        """Stop the idle owned daemon gracefully (its VMs stay) and let the next call start
        one with `settings` (MARSH_* daemon-start variables; None unsets)."""
        from run import request_authenticated_daemon_shutdown
        self.document("status", "--json")
        pid, token = self.owned_daemon_pid, self.owned_daemon_control_token
        require(pid and token and stable_process_identity(pid) == self.owned_daemon_process_identity,
                "cannot verify the owned daemon process before restarting it")
        def stopped() -> bool:  # refused while shells or jobs are still active
            try:
                request_authenticated_daemon_shutdown(self.home, token)
                return True
            except RuntimeError:
                time.sleep(1)
                return False
        self.wait_until(stopped, 180, "an idle scope to stop the daemon")
        self.wait_until(lambda: stable_process_identity(pid) is None, 60, "daemon exit")
        for field in ("owned_daemon_id", "owned_daemon_pid", "owned_daemon_process_identity",
                      "owned_daemon_control_token"):
            setattr(self, field, None)  # deliberate restart: re-learn the new daemon's ownership
        for name, value in settings.items():
            for environment in (self.environment, self.host_env):
                if value is None:
                    environment.pop(name, None)
                else:
                    environment[name] = value
        self.document("status", "--json")

    def with_daemon_settings(self, settings: dict[str, str], body: Callable[[], None]) -> None:
        self.restart_daemon(dict(settings))
        try:
            body()
        finally:
            self.restart_daemon({name: None for name in settings})

    def wall_inheritance(self) -> None:
        """s6 wall time: a child's budget is min(its own limit, the parent's remaining time); it is
        stopped near the parent's deadline and its receipt says so."""
        wall = 45

        def body() -> None:
            before = self.job_ids()
            began = time.time()
            completed = self.injob("sleep 12; fixture hold 600; echo child=$?", timeout=300)
            elapsed = time.time() - began
            receipts = self.settle(before, 2, timeout=120)
            root = self.one_top(receipts, "wall")
            child = next((r for r in receipts if r is not root), None)
            require(child is not None and self.lin(child).get("parent") == f"job:{root['job_id']}",
                    f"no child of the parent job: {[(r.get('command'), self.lin(r)) for r in receipts]}")
            parent_deadline = self.lin(root).get("deadline_unix_ms")
            lineage = self.lin(child)
            require(isinstance(parent_deadline, int) and lineage.get("deadline_unix_ms") == parent_deadline
                    and lineage.get("parent_deadline") is True,
                    f"child did not inherit the parent's deadline: parent={self.lin(root)} child={lineage}")
            cause = json.dumps(child.get("exit"))
            require("parent deadline" in cause, f"child receipt does not name the parent deadline: {cause}")
            finished = child.get("finished_unix_ms") or 0
            require(abs(finished - parent_deadline) <= 15_000,
                    f"child ended {(finished - parent_deadline) / 1000:.1f}s from the parent's deadline")
            require(finished - (child.get("created_unix_ms") or 0) < (wall - 5) * 1000,
                    "child ran for its own full wall limit, not the parent's remaining time")
            require(completed.returncode != 0 and elapsed < wall + 60,
                    f"root: status {completed.returncode} after {elapsed:.0f}s (wall {wall}s)")
            self.verify_deleted(receipts)
            self.records.append({"scenario": "P35", "root": root, "child": child,
                                 "stderr": text(completed.stderr)[-2000:]})

        self.with_daemon_settings({"MARSH_JOB_WALL_SECONDS": str(wall)}, body)

    def tree_kit_vm_cap(self) -> None:
        """s6 Kit VMs per tree: a child needing one more distinct Kit VM than the cap is refused
        fast (exit 125) with the Kit names; an alias of a live Kit is not a new VM."""

        def body() -> None:
            before = self.job_ids()
            script = ("fixture-alt identity >/dev/null; echo alias=$?; s=$(date +%s); "
                      "shell -c true; echo other=$?; echo took=$(( $(date +%s) - s ))")
            completed = self.injob(script, timeout=300)
            v, err = self.kv(completed.stdout), text(completed.stderr)
            require(v.get("alias") == "0", f"an alias of the live Kit was refused: {v} {err[-600:]!r}")
            require(v.get("other") == "125"
                    and "shell refused: tree already uses 1 Kit VM (fixture) (MARSH_TREE_KIT_VMS)" in err,
                    f"second distinct Kit VM not refused clearly: {v} {err[-600:]!r}")
            require(int(v.get("took", "99")) <= 10, "the Kit VM refusal was not fail-fast")
            receipts = self.settle(before, 2)
            require(sorted(r.get("command") for r in receipts) == ["fixture", ALT],
                    f"want the root and its alias child only: {[r.get('command') for r in receipts]}")
            self.verify_deleted(receipts)
            # Another tree has its own budget.
            other = self.brush("shell -c true; echo top=$?", self.project, timeout=300)
            require(self.kv(other.stdout).get("top") == "0", f"a new tree was refused: {text(other.stderr)[-400:]!r}")

        self.with_daemon_settings({"MARSH_TREE_KIT_VMS": "1"}, body)

    @staticmethod
    def reap_pty(process: subprocess.Popen[bytes], master: int, output: bytearray) -> None:
        deadline = time.monotonic() + 120
        while process.poll() is None and time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.25)
            if ready:
                try:
                    chunk = os.read(master, 65536)
                except OSError:
                    break
                if b"\x1b[6n" in chunk:
                    os.write(master, b"\x1b[1;1R")
                output.extend(chunk)
        require(process.poll() == 0, f"interactive session exited {process.poll()}: {text(bytes(output))[-600:]!r}")

    # ---- driver -----------------------------------------------------------
    def preflight(self) -> str | None:
        """Fail before any VM work if the product lacks the process CLI (`marsh --help` is VM-free)."""
        usage = text(self.cli(["--help"], self.project, timeout=30).stdout)
        missing = [verb for verb in ("run", "context") if not re.search(rf"^\s*marsh {verb}\b", usage, re.M)]
        if missing:
            return (f"`marsh --help` lists no {', '.join(f'`marsh {v}`' for v in missing)} subcommand "
                    "(process CLI not implemented; `marsh run ...` would run as a Brush script)")
        for args in (["run", "--help"], ["context"]):
            completed = self.cli(args, self.project, timeout=60)
            if completed.returncode != 0:
                lines = [line for line in (text(completed.stderr) or text(completed.stdout)).splitlines() if line.strip()]
                return f"`marsh {' '.join(args)}` exited {completed.returncode}: " + (lines[0] if lines else "(no output)")
        return None

    def run_all(self) -> None:
        self.stock_before = stock_vm_inventory(self.sbx)
        self.source["stock_before"] = self.stock_before
        (self.project / "README.md").write_text("processes acceptance\n")
        self.git(self.project, "init", "-q", "-b", "main")
        self.git(self.project, "add", "-A")
        self.git(self.project, "commit", "-q", "-m", "seed")
        self.document("status", "--json")
        if self.preflight_enabled:
            blocked = self.preflight()
            if blocked:
                for scenario in self.selected:
                    self.results.append({"scenario": scenario, "outcome": "blocked", "failure": blocked})
                    print(f"processes: {scenario}: blocked: {blocked}", flush=True)
                raise Fail(f"preflight: process CLI unavailable: {blocked}")
        self.run([self.marsh, "--load", "fixture", "-c", "true"], timeout=900)
        if LIVE & set(self.selected):
            self.run([self.marsh, "--load", "claude,codex", "-c", "true"], timeout=1800)
        if NEEDS_SHELL_KIT & set(self.selected):
            self.run([self.marsh, "--load", "shell", "-c", "true"], timeout=900)
        self.document("status", "--json")
        failures = []
        for scenario in self.selected:
            began = time.monotonic()
            try:
                getattr(self, SCENARIOS[scenario])()
                outcome, failure = "passed", None
            except Fail as error:
                outcome, failure = "failed", str(error)
            except Exception as error:  # a product or harness break is still a failure
                outcome, failure = "failed", f"{type(error).__name__}: {error}"
                self.records.append({"scenario": scenario, "traceback": traceback.format_exc()})
            finally:
                for process in self.live:
                    self.kill(process)
                self.live.clear()
            self.results.append({"scenario": scenario, "outcome": outcome, "failure": failure,
                                 "seconds": round(time.monotonic() - began, 1)})
            print(f"processes: {scenario}: {outcome}" + (f": {failure[:400]}" if failure else ""), flush=True)
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
        destination = self.evidence / "processes.json"
        destination.write_text(json.dumps({
            "outcome": "failed" if error else "passed", "failure": str(error) if error else None,
            "scenarios": self.results, "root": str(self.root), "environment": self.source,
            "records": self.records}, indent=2, default=str) + "\n", encoding="utf-8")
        destination.chmod(0o600)
        passed = sum(r["outcome"] == "passed" for r in self.results)
        print(f"processes: {passed}/{len(self.selected)} passed; {'failed' if error else 'passed'}; "
              f"evidence: {destination}")
        if error:
            print(f"processes: {error}")
        return 1 if error else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--prefix", default=str(pathlib.Path.home() / ".marsh-dev"),
                        help="installed dev product (bin/marsh, libexec/marsh)")
    parser.add_argument("--sbx", default="sbx")
    parser.add_argument("--kit", default=None,
                        help="fixture Kit: immutable OCI ref, native v3 source dir, or a commands.json whose "
                             "\"fixture\" key holds the ref (needs the exec/bench/cap-flood modes)")
    parser.add_argument("--evidence", default=None)
    parser.add_argument("--only", action="append", choices=sorted(SCENARIOS), help="repeatable")
    parser.add_argument("--fail-fast", action="store_true", help="stop at the first failed scenario")
    parser.add_argument("--no-preflight", action="store_true")
    parser.add_argument("--list", action="store_true", help="print scenario ids and exit")
    parser.add_argument("--live", action="store_true",
                        help="also run billed checks (P32: a real claude turn that runs codex; 1-2 model calls)")
    parser.add_argument("--source-tree", default=str(pathlib.Path(__file__).resolve().parents[2]))
    parser.add_argument("--source-revision", default=None)
    parser.add_argument("--build-receipt", type=pathlib.Path, default=None)
    arguments = parser.parse_args()
    if arguments.list:
        for scenario in ORDER:
            print(f"{scenario}\t{getattr(Processes, SCENARIOS[scenario]).__doc__.strip().splitlines()[0]}")
        return 0
    if not arguments.kit:
        parser.error("--kit is required")
    prefix = pathlib.Path(arguments.prefix).expanduser().resolve()
    arguments.marsh = str(prefix / "bin" / "marsh")
    arguments.guest_artifacts = prefix / "libexec" / "marsh"
    if not os.access(arguments.marsh, os.X_OK):
        print(f"processes: no installed product at {arguments.marsh} (run `make dev`)")
        return 1
    kit = pathlib.Path(arguments.kit).expanduser()
    if kit.is_file():
        arguments.kit = json.loads(kit.read_text())["fixture"]
    if arguments.source_revision is None:
        arguments.source_revision = subprocess.run(
            ["git", "-C", arguments.source_tree, "rev-parse", "HEAD"], capture_output=True, text=True,
            timeout=30, check=True).stdout.strip()
    if arguments.evidence is None:
        arguments.evidence = f"/private/tmp/marsh-dev-processes-{os.getuid()}"
    harness = Processes(arguments)
    try:
        harness.run_all()
    except KeyboardInterrupt:
        return harness.finish(InterruptedError("processes acceptance interrupted"))
    except Exception as error:
        return harness.finish(error)
    return harness.finish(None)


if __name__ == "__main__":
    raise SystemExit(main())
