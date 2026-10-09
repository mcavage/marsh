#!/usr/bin/env python3
"""Public-CLI shell disconnect UAT; real stock guest inventory is the oracle.

No Kit/provider/Cloud is needed. Run on the supported Mac with matching host
and guest artifacts. A guest-written PID is only a selector: successful stock
kernel `/proc` inventories must independently establish birth identity and descendants.
An inherited unique fixture token discovers even adopted/late-TERM forks; SID is
observed as escape evidence, never used as the cleanup absence oracle.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import pty
import select
import shlex
import shutil
import signal
import subprocess
import sys
import time
import uuid

# An independent kernel oracle, not production cgroup/record parsing. Keep PID
# birth time so PID reuse never becomes either a survivor or a killed canary.
PROC_INVENTORY = r'''
import json, os
rows=[]
for entry in os.listdir('/proc'):
    if not entry.isdecimal(): continue
    try:
        stat=open('/proc/'+entry+'/stat').read().rsplit(')',1)[1].split()
        status=open('/proc/'+entry+'/status').read().splitlines()
        uid=int(next(v for v in status if v.startswith('Uid:')).split()[1])
        command=os.fsdecode(open('/proc/'+entry+'/cmdline','rb').read().replace(b'\0',b' '))
        after=open('/proc/'+entry+'/stat').read().rsplit(')',1)[1].split()
        if after[19] != stat[19]: continue
        rows.append(dict(pid=int(entry), parent=int(stat[1]), group=int(stat[2]),
                         session=int(stat[3]), uid=uid, state=stat[0],
                         start_time=int(stat[19]), command=command))
    except (FileNotFoundError, ProcessLookupError): pass
print(json.dumps(rows))
'''


def process_identity(row):
    return row["pid"], row["start_time"]


def descendants(rows, leader):
    """Transitive PPID ancestry at this observation, independent of SID/UID."""
    found = {process_identity(leader)}
    parents = {row["pid"] for row in rows if process_identity(row) == process_identity(leader)}
    while True:
        children = [row for row in rows if row["parent"] in parents
                    and row["start_time"] >= leader["start_time"]
                    and process_identity(row) not in found]
        if not children:
            return found
        found.update(map(process_identity, children))
        parents.update(row["pid"] for row in children)


def owned_survivors(rows, identities, token):
    # A unique inherited argv token catches reparented and late-created forks.
    # It is only a discovery selector. No pattern-based signal is ever sent.
    return [row for row in rows if not row["state"].startswith(("Z", "X"))
            and (process_identity(row) in identities or (token and token in row["command"]))]

from run import disposable_root, resolve_executable, scoped_control_home
from provenance import host_only_path, verify_build_receipt


class Journey:
    def __init__(self, args: argparse.Namespace):
        self.marsh = resolve_executable(args.marsh)
        self.sbx = resolve_executable(args.sbx)
        artifacts = Path(args.guest_artifacts).resolve(strict=True)
        # A `make dev` build has no build receipt; like the dev smoke, record
        # binary digests only and make no release provenance claim.
        receipt = verify_build_receipt(Path(args.build_receipt), source_tree=Path(args.source_tree),
                                       revision=args.source_revision, marsh=Path(self.marsh),
                                       guest_artifacts=artifacts) if args.build_receipt else {
            "source_before": {"revision": args.source_revision, "dev_build": True}}
        self.evidence = host_only_path(Path(args.evidence) / "shell-disconnect.json",
                                       Path(args.source_tree)).parent
        self.result = {"schema": "marsh.shell-disconnect/v1", "outcome": "running",
                       "source": receipt["source_before"], "build_receipt": receipt,
                       "commands": [], "checks": []}
        self.root = disposable_root("marsh-shell-disconnect-")
        self.home = self.root / "selected-home"
        self.project = self.root / "project"
        self.control = self.root / "control"
        for path in (self.home, self.project, self.control):
            path.mkdir(mode=0o700)
        (scoped_control_home(self.control, self.home) / "commands.json").write_text("{}\n")
        self.result["binaries"] = {
            str(path): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in [Path(self.marsh), *(artifacts / name for name in
                         ("marsh-linux-arm64", "marsh-worker-linux-arm64", "marsh-relay-linux-arm64"))]
        }
        self.env = dict(os.environ, MARSH_HOME=str(self.home), MARSH_CONTROL_HOME=str(self.control),
                        MARSH_GUEST_ARTIFACTS=str(artifacts), MARSH_SBX=self.sbx)
        for key in ("MARSH_DAEMON_SOCKET", "MARSH_DAEMON_TOKEN", "MARSH_SESSION_ID"):
            self.env.pop(key, None)
        # Shell VM names are random (`marsh-s-<id>`); discovered after first use.
        self.vm = None
        # Diagnostic only: a skipped fixture is recorded and never a pass.
        self.skip = set(args.skip or [])
        self.children = []
        self.started = False
        self.baseline = None
        self.save()

    def save(self):
        destination = self.evidence / "shell-disconnect.json"
        descriptor = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(descriptor, "w") as stream:
            stream.write(json.dumps(self.result, indent=2) + "\n")

    def command(self, argv, *, timeout=30, input=None, check=True):
        start = time.monotonic()
        completed = subprocess.run(argv, cwd=self.project, env=self.env, input=input,
                                   stdin=subprocess.DEVNULL if input is None else None,
                                   capture_output=True, timeout=timeout, check=False, start_new_session=True)
        self.result["commands"].append({"argv": argv, "status": completed.returncode,
                                        "seconds": time.monotonic() - start,
                                        "stdout": completed.stdout.decode(errors="replace"),
                                        "stderr": completed.stderr.decode(errors="replace")})
        self.save()
        if check and completed.returncode != 0:
            raise AssertionError(f"command failed ({completed.returncode}): {argv}: {completed.stderr!r}")
        return completed

    def stock_names(self):
        document = json.loads(self.command([self.sbx, "ls", "--json"]).stdout)
        rows = document if isinstance(document, list) else document.get("sandboxes")
        if not isinstance(rows, list) or any(not isinstance(row, dict) or not isinstance(row.get("name"), str) or not row["name"] for row in rows):
            raise AssertionError("malformed stock sandbox inventory")
        names = {row["name"] for row in rows}
        if len(names) != len(rows):
            raise AssertionError("duplicate stock sandbox inventory entries")
        return names

    def inventory(self):
        result = self.command([self.sbx, "exec", "-u", "root", self.vm,
                               "python3", "-I", "-S", "-c", PROC_INVENTORY])
        rows = json.loads(result.stdout)
        required = {"pid", "parent", "group", "session", "uid", "state", "start_time", "command"}
        if not isinstance(rows, list) or not rows or any(
                not isinstance(row, dict) or set(row) != required
                or any(type(row[key]) is not int or row[key] < 0
                       for key in ("pid", "parent", "group", "session", "uid", "start_time"))
                or row["pid"] <= 0
                or not isinstance(row["command"], str) or not isinstance(row["state"], str)
                for row in rows) or len({row["pid"] for row in rows}) != len(rows):
            raise AssertionError("malformed independent kernel process inventory")
        return rows

    def status(self):
        result = json.loads(self.command([self.marsh, "status", "--json"]).stdout)
        if result.get("schema") != "marsh.status/v1":
            raise AssertionError("missing public status schema")
        return result

    def start_shell(self, name: str, terminal: bool, privileged=False, fixture=None):
        marker = self.project / f"{name}.pid"
        token = f"marsh-containment-{uuid.uuid4().hex}"
        if fixture:
            fixture_path = self.project / "shell_containment_fixture.py"
            shutil.copyfile(Path(__file__).with_name("shell_containment_fixture.py"), fixture_path)
            child_command = ("sudo -n " if privileged else "") + "python3 -I -S " + " ".join(
                shlex.quote(str(value)) for value in (fixture_path, fixture, self.project, token))
            # Foreground sudo under a real interactive tty activates sudo use_pty.
            # exec tests the cleanup path when the recorded leader exits first.
            tail = (f"exec {child_command}" if fixture == "leader-first" else child_command)
            command = f"trap '' TERM; printf '%s\\n' $$ > {shlex.quote(str(marker))}; {tail}"
        else:
            child_command = "sudo -n /bin/sleep 600" if privileged else "/bin/sleep 600"
            command = f"trap '' TERM; {child_command} & printf '%s\\n' $$ > {shlex.quote(str(marker))}; wait"
        argv = [self.marsh, *(["-i"] if terminal else []), "-c", command]
        master = None
        log = (self.evidence / f"{name}.log").open("wb")
        if terminal:
            master, slave = pty.openpty()
            child = subprocess.Popen(argv, cwd=self.project, env=self.env, stdin=slave,
                                     stdout=slave, stderr=slave, start_new_session=True)
            os.close(slave)
        else:
            child = subprocess.Popen(argv, cwd=self.project, env=self.env, stdin=subprocess.DEVNULL,
                                     stdout=log, stderr=log, start_new_session=True)
        self.children.append((child, master, log))
        self.result["commands"].append({"argv": argv, "pty": terminal, "host_pid": child.pid})
        deadline = time.monotonic() + 180
        while not marker.exists():
            if child.poll() is not None or time.monotonic() >= deadline:
                raise AssertionError(f"{name} did not start; see {log.name}")
            time.sleep(0.1)
        pid = int(marker.read_text().strip())
        rows = self.inventory()
        leader = next((row for row in rows if row["pid"] == pid), None)
        if leader is None or leader["session"] != pid or leader["group"] != pid:
            raise AssertionError(f"not a live session/group leader: {leader}")
        deadline = time.monotonic() + 15
        while True:
            identities = descendants(rows, leader)
            selected = [row for row in rows if token in row["command"]] if fixture else []
            identities.update(map(process_identity, selected))
            live = owned_survivors(rows, identities, token if fixture else None)
            ready = len(live) > 1
            if fixture in ("setsid", "doublefork", "leader-first"):
                ready = ready and any(row["session"] != pid for row in selected)
            if privileged:
                ready = ready and any(row["uid"] == 0 for row in selected or live)
            if terminal and privileged:
                ready = ready and any(row["uid"] == 0 and row["session"] != pid for row in selected)
            if ready:
                break
            if child.poll() is not None or time.monotonic() >= deadline:
                raise AssertionError(f"fixture lacks independently observed required descendants: {name}: {rows}")
            time.sleep(0.1)
            rows = self.inventory()
        self.result["checks"].append({"name": f"{name}-kernel-before", "leader": leader,
                                      "members": live, "token": token if fixture else None})
        self.save()
        return child, leader, identities, token if fixture else None

    def concurrent_open_and_exit(self):
        # Different launch roots force new project grants rather than merely
        # incrementing the sibling's existing project reference. The controlled
        # runner regression pins the exact stock-call interleaving; this stock
        # journey observes overlapping public lifetimes and recovery/reuse.
        for attempt in range(8):
            project = self.root / f"overlap-project-{attempt}"
            project.mkdir(mode=0o700)
            ready = self.project / f"overlap-{attempt}.ready"
            release = self.project / f"overlap-{attempt}.release"
            script = f"printf ready > {shlex.quote(str(ready))}; while [ ! -e {shlex.quote(str(release))} ]; do sleep 0.02; done; exit 0"
            exiting = subprocess.Popen([self.marsh, "-c", script], cwd=self.project, env=self.env,
                stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
            opening = None
            try:
                deadline = time.monotonic() + 60
                while not ready.exists():
                    assert exiting.poll() is None and time.monotonic() < deadline, "overlap fixture did not start"
                    time.sleep(0.02)
                # A warm shell can open and exit before one status poll; hold
                # it open until its attached overlap is observed.
                go = project / "go"
                opening = subprocess.Popen([self.marsh, "-c", f"printf 'opened\\n'; while [ ! -e {shlex.quote(str(go))} ]; do sleep 0.02; done"], cwd=project, env=self.env,
                    stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
                while True:
                    status = self.status()
                    if any(row["pid"] == opening.pid and row["state"] == "attached" for row in status["shells"]):
                        break
                    assert opening.poll() is None and time.monotonic() < deadline, "new shell never overlapped live exit fixture"
                    time.sleep(0.01)
                assert exiting.poll() is None
                go.touch()
                release.touch()
                output, error = opening.communicate(timeout=60)
                _, exit_error = exiting.communicate(timeout=60)
                assert opening.returncode == 0 and output == b"opened\n", (output, error)
                assert exiting.returncode == 0, exit_error
                status = self.status()
                assert not any(row["state"] == "cleanup_uncertain" for row in status["shells"]), status
                self.result["checks"].append({"name": "concurrent-open-and-exit", "attempt": attempt,
                    "opening_pid": opening.pid, "exiting_pid": exiting.pid, "project": str(project),
                    "statuses": [opening.returncode, exiting.returncode]})
                self.save()
            finally:
                release.touch()
                (project / "go").touch()
                for owned in (opening, exiting):
                    if owned is not None:
                        if owned.poll() is None: owned.kill()
                        owned.wait(timeout=5)
        self.command([self.marsh, "-c", "printf 'overlap-reusable\\n'"], timeout=60)

    def paused_output(self, expected: int, pager: bool):
        """Healthy slow readers are backpressure, not disconnected controllers."""
        name = f"paused-{'pager' if pager else 'reader'}-{expected}"
        marker = self.project / f"{name}.ready"
        payload = self.evidence / f"{name}.bytes"
        script = f"printf ready > {shlex.quote(str(marker))}; head -c 4000000 /dev/zero; exit {expected}"
        child = subprocess.Popen([self.marsh, "-c", script], cwd=self.project, env=self.env,
                                 stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                 start_new_session=True)
        sink = None
        try:
            if pager:
                sink = subprocess.Popen([sys.executable, "-c",
                    "import pathlib,sys; pathlib.Path(sys.argv[1]).write_bytes(sys.stdin.buffer.read())", str(payload)],
                    stdin=child.stdout, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, start_new_session=True)
                child.stdout.close(); child.stdout = None
                sink.send_signal(signal.SIGSTOP)
            deadline = time.monotonic() + 60
            while not marker.exists():
                if child.poll() is not None or time.monotonic() > deadline:
                    raise AssertionError(f"{name} did not start")
                time.sleep(0.02)
            started = time.monotonic()
            time.sleep(7)
            if child.poll() is not None:
                raise AssertionError(f"{name} killed healthy producer during pause: {child.returncode}")
            if sink is not None:
                sink.send_signal(signal.SIGCONT)
            stdout, stderr = child.communicate(timeout=60)
            if sink is not None:
                _, sink_error = sink.communicate(timeout=10)
                assert sink.returncode == 0 and not sink_error, sink_error
                stdout = payload.read_bytes()
            assert child.returncode == expected and stdout == bytes(4_000_000), (name, child.returncode, len(stdout), stderr)
            self.result["checks"].append({"name": name, "bytes": len(stdout), "status": child.returncode,
                "sha256": hashlib.sha256(stdout).hexdigest(), "pause_seconds": 7,
                "drain_and_cleanup_seconds": time.monotonic() - started - 7,
                "controller_pid": child.pid, "pager_pid": sink.pid if sink else None})
            self.save()
        finally:
            for owned in (sink, child):
                if owned is not None:
                    if owned.poll() is None:
                        owned.send_signal(signal.SIGCONT); owned.kill()
                    owned.wait(timeout=5)

    def terminal_stderr_redirect(self):
        master, slave = pty.openpty()
        child = subprocess.Popen([self.marsh, "-c", "printf 'stdout-only\\n'; printf 'stderr-only\\n' >&2"],
            cwd=self.project, env=self.env, stdin=slave, stdout=slave, stderr=subprocess.PIPE,
            start_new_session=True)
        try:
            _, stderr = child.communicate(timeout=60)
            # Darwin discards unread PTY output once the last slave closes;
            # keep ours open until the delivered bytes are read.
            stdout = bytearray()
            while select.select([master], [], [], 0.2)[0]:
                try:
                    data = os.read(master, 65536)
                except OSError:
                    break
                if not data:
                    break
                stdout.extend(data)
            os.close(slave)
            slave = None
            assert child.returncode == 0 and bytes(stdout) == b"stdout-only\r\n" and stderr == b"stderr-only\n", (stdout, stderr)
            self.result["checks"].append({"name": "terminal-stderr-redirection", "status": child.returncode,
                "stdout_hex": bytes(stdout).hex(), "stderr_hex": stderr.hex()})
            self.save()
        finally:
            if child.poll() is None: child.kill()
            child.wait(timeout=5); os.close(master)
            if slave is not None: os.close(slave)

    def run(self):
        if self.command([self.sbx, "version"], check=False).returncode:
            self.command([self.sbx, "--version"])
        self.baseline = self.stock_names()
        self.started = True
        latency = []
        for index in range(6):
            result = self.command([self.marsh, "-c", "printf 'shell-ready\\n'"], timeout=240)
            elapsed = self.result["commands"][-1]["seconds"]
            assert result.stdout == b"shell-ready\n", result.stdout
            if self.vm is None:
                created = sorted(name for name in self.stock_names() - self.baseline
                                 if name.startswith("marsh-s-"))
                if len(created) != 1:
                    raise AssertionError(f"cannot identify this run's shell VM: {created}")
                self.vm = created[0]
            listing = json.loads(self.command([self.sbx, "ls", "--json"]).stdout)
            rows = listing if isinstance(listing, list) else listing["sandboxes"]
            row = next(row for row in rows if row["name"] == self.vm)
            identity = row.get("id")
            if not isinstance(identity, str) or not identity:
                raise AssertionError("latency sample lacks independently observed stock UUID")
            if latency and identity != latency[0]["vm_id"]:
                raise AssertionError("warm latency sample changed VM identity")
            latency.append({"kind": "cold-vm-create" if index == 0 else "warm-vm", "seconds": elapsed,
                "vm_id": identity, "status": result.returncode,
                "boundary": "host CLI spawn through complete delivery and teardown; image-cache coldness not inferred"})
        self.result["checks"].append({"name": "observed-shell-latency", "samples": latency})
        self.save()
        for code in (42, 0):
            self.paused_output(code, False)
            self.paused_output(code, True)
        self.terminal_stderr_redirect()
        sibling, sibling_leader, sibling_members, _ = self.start_shell("sibling", False)
        self.concurrent_open_and_exit()
        assert sibling.poll() is None, "concurrent open/exit killed the unrelated sibling"
        for name, terminal, privileged, fixture in (
                ("pipe", False, False, None), ("pty", True, False, None),
                ("sudo", False, True, "sudo"), ("sudo-tty", True, True, "sudo-tty"),
                ("setsid", False, False, "setsid"), ("doublefork", False, False, "doublefork"),
                ("late-term", False, False, "late-term"),
                ("uid-change", False, True, "uid-change"),
                ("uid-change-term", False, True, "uid-change-term"),
                ("leader-first", False, False, "leader-first")):
            if name in self.skip:
                self.result["checks"].append({"name": name, "skipped": True})
                continue
            child, leader, identities, token = self.start_shell(name, terminal, privileged, fixture)
            pid = leader["pid"]
            if fixture == "uid-change":
                root_marker = self.project / f"{token}.root-ready"
                deadline = time.monotonic() + 15
                while not root_marker.exists():
                    if time.monotonic() >= deadline:
                        raise AssertionError("sudo fixture did not reach root phase")
                    time.sleep(0.05)
                root_pid = int(root_marker.read_text())
                root_row = next(row for row in self.inventory() if row["pid"] == root_pid)
                if root_row["uid"] != 0 or token not in root_row["command"]:
                    raise AssertionError("sudo root phase not independently confirmed")
                (self.project / f"{token}.change-uid").touch()
                deadline = time.monotonic() + 15
                while True:
                    changed = [row for row in self.inventory()
                               if process_identity(row) == process_identity(root_row)
                               and row["uid"] == leader["uid"]]
                    if changed:
                        identities.update(map(process_identity, changed))
                        break
                    if time.monotonic() >= deadline:
                        raise AssertionError("real sudo setuid transition was not independently observed")
                    time.sleep(0.1)
            # The live controller and EOF-only sibling must outlive the cleanup
            # grace. There is no total deadline on a healthy interactive shell.
            time.sleep(6)
            assert child.poll() is None and sibling.poll() is None
            before = self.status()
            session_ids = {item["session_id"] for item in before["shells"]
                           if item["pid"] == child.pid and item["state"] == "attached"}
            if len(session_ids) != 1:
                raise AssertionError(f"public shell session not identifiable: {before}")
            def matching_relays(rows):
                return [row for row in rows if "marsh-relay" in row["command"]
                        and any(f"/{session}/s" in row["command"] for session in session_ids)
                        and not row["state"].startswith("Z")]
            if not matching_relays(self.inventory()):
                raise AssertionError("shell has no independently observed guest relay")
            start = time.monotonic()
            if fixture == "leader-first":
                (self.project / f"{token}.release").touch()
            else:
                child.kill()  # abrupt controller loss, NOT a cooperative signal frame
                child.wait(timeout=5)
            deadline = time.monotonic() + 45
            while True:
                rows = self.inventory()
                # Discover both still-parented and adopted late forks. Do not
                # mirror production SID/cgroup cleanup membership.
                identities.update(descendants(rows, leader))
                alive = owned_survivors(rows, identities, token)
                identities.update(map(process_identity, alive))
                live_identities = {process_identity(row) for row in rows
                                   if not row["state"].startswith(("Z", "X"))}
                if not sibling_members.issubset(live_identities):
                    raise AssertionError("disconnect killed unrelated sibling shell/descendant canary")
                status = self.status()
                attached = [item for item in status["shells"]
                            if item["session_id"] in session_ids and item["state"] == "attached"]
                relays = matching_relays(rows)
                if not alive and not attached and not relays:
                    break
                if time.monotonic() >= deadline:
                    raise AssertionError(f"{name} disconnect cleanup incomplete: {alive}; {attached}; relays={relays}")
                time.sleep(0.2)
            if fixture == "leader-first":
                if child.wait(timeout=15) != 23:
                    raise AssertionError("leader-first lost the actual process status 23")
            if token:
                pulses = {str(path): path.read_bytes() for path in self.project.glob(f"{token}-*.pulse")}
                time.sleep(0.3)
                after = {str(path): path.read_bytes() for path in self.project.glob(f"{token}-*.pulse")}
                if not pulses or after != pulses or owned_survivors(self.inventory(), identities, token):
                    raise AssertionError(f"{name}: post-cleanup process/byte side effect survived")
            self.result["checks"].append({"name": name, "guest_session": pid,
                                          "observed_identities": sorted(identities),
                                          "cleanup_seconds": time.monotonic() - start,
                                          "public_session": sorted(session_ids),
                                          "sibling_survived": True})
            # Reuse checks cleanup did not quarantine a healthy successful path.
            self.command([self.marsh, "-c", "printf 'reusable\\n'"], timeout=60)
        for script, data, expected in (("cat >/dev/null; exit 42", b"input", 42),
                                       ("head -c 1 >/dev/null; exit 7", b"x" * 1_000_000, 7),
                                       ("head -c 1 >/dev/null; exit 0", b"x" * 1_000_000, 0),
                                       ("exit 1", b"", 1)):
            result = self.command([self.marsh, "-c", script], input=data, timeout=60, check=False)
            assert result.returncode == expected, (script, result.returncode, result.stderr)
            self.result["checks"].append({"name": "stdin-eof-or-early-close", "status": expected})
        sibling.kill()
        sibling.wait(timeout=5)
        deadline = time.monotonic() + 45
        while owned_survivors(self.inventory(), sibling_members, None):
            if time.monotonic() >= deadline:
                raise AssertionError("sibling disconnect cleanup incomplete")
            time.sleep(0.2)

    def finish(self, failure):
        for child, master, log in self.children:
            if child.poll() is None:
                child.kill()
            child.wait(timeout=5)
            if master is not None:
                os.close(master)
            log.close()
        if self.started:
            try:
                # A just-killed controller's session may still be finishing
                # host teardown; marsh stop correctly refuses until it ends.
                deadline = time.monotonic() + 30
                while True:
                    result = self.command([self.marsh, "stop", "--json"], timeout=180,
                                          check=False)
                    if result.returncode == 0 or time.monotonic() >= deadline \
                            or b"active shell sessions" not in result.stderr:
                        break
                    time.sleep(0.5)
                if result.returncode != 0:
                    raise AssertionError(f"marsh stop failed: {result.stderr!r}")
                if not json.loads(result.stdout).get("cleanup_complete"):
                    raise AssertionError("scope cleanup uncertain")
                remaining = self.stock_names()
                if self.vm in remaining or not (self.baseline or set()).issubset(remaining):
                    raise AssertionError("owned shell VM remains or unrelated sandbox disappeared")
            except Exception as error:
                failure = f"{failure or ''}; cleanup failed: {error}"
        skipped = sorted(self.skip)
        self.result["outcome"] = "failed" if failure else ("passed-with-skips" if skipped else "passed")
        self.result["failure"] = failure
        if failure:
            self.result["retained_root"] = str(self.root)
        else:
            shutil.rmtree(self.root)
        self.save()
        print(f"{self.result['outcome']}: {self.evidence / 'shell-disconnect.json'}")
        return 1 if failure else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("marsh", "guest-artifacts", "source-tree", "source-revision", "evidence"):
        parser.add_argument(f"--{name}", required=True)
    parser.add_argument("--build-receipt", help="release build receipt; omit for a make dev build")
    parser.add_argument("--skip", action="append", metavar="FIXTURE",
                        help="diagnostic: skip one named fixture (recorded as skipped)")
    parser.add_argument("--sbx", default="sbx")
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("requires the supported macOS stock-SBX host; Linux component tests are not product E2E")
    journey = Journey(args)
    failure = None
    try:
        journey.run()
    except Exception as error:
        failure = f"{type(error).__name__}: {error}"
    return journey.finish(failure)


if __name__ == "__main__":
    raise SystemExit(main())
