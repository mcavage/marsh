#!/usr/bin/env python3
"""Host-only M3 LOCAL worker/receipt UAT. No Cloud, mocks or receipt injection.

Requires exact candidate provenance and immutable fixture + shell Kit digests.
Normal exit7/direct stderr, output/PID/wall limits and Docker create failure are
observed through public CLI receipts, independent files and stock inspection.
This is a later root stock gate, not a credential-free VM test.
"""
import argparse
import hashlib
import json
import os
import pathlib
import platform
import re
import shlex
import signal
import subprocess
import tempfile
import time

from provenance import candidate_arguments, verify_candidate, source_identity, stock_vm_inventory, stock_cleanup_errors
from run import verify_container_deleted


class Scope:
    def __init__(self, args, guest, records, memory):
        self.args, self.records = args, records
        self.root = pathlib.Path(tempfile.mkdtemp(prefix="marsh-typed-receipts-", dir="/private/tmp")).resolve()
        self.project = self.root / "project"
        self.home = self.root / "selected"
        self.control = self.root / "control"
        for path in (self.project, self.home, self.home / "home", self.control):
            path.mkdir(mode=0o700)
        scope = self.control / hashlib.sha256(os.fsencode(self.home)).hexdigest()
        scope.mkdir(mode=0o700)
        (scope / "commands.json").write_text(json.dumps({"fixture": args.kit, "typed-shell": args.shell_kit}))
        # Explicit replacement must disable packaged adapters, not merge them back.
        (scope / "agents.json").write_text("[]")
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("MARSH_")}
        self.env.update(MARSH_HOME=str(self.home), MARSH_CONTROL_HOME=str(self.control),
                        MARSH_GUEST_ARTIFACTS=str(guest), MARSH_SBX=args.sbx,
                        MARSH_JOB_CPU_MILLIS="1000", MARSH_JOB_MEMORY_BYTES=str(memory),
                        MARSH_JOB_PIDS="32", MARSH_JOB_WRITABLE_BYTES="16777216",
                        MARSH_JOB_OUTPUT_BYTES="1048576", MARSH_JOB_WALL_SECONDS="30")
        self.before = stock_vm_inventory(args.sbx)
        self.started = False

    def spawn(self, argv):
        if argv[0] == self.args.marsh:
            self.started = True
        child = subprocess.Popen(argv, cwd=self.project, env=self.env, stdin=subprocess.DEVNULL,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        entry = {"argv": argv, "pid": child.pid, "pgid": os.getpgid(child.pid)}
        self.records.append(entry)
        return child, entry

    def finish(self, child, entry, timeout):
        try:
            stdout, stderr = child.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGTERM)
            try:
                stdout, stderr = child.communicate(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                stdout, stderr = child.communicate()
            entry["timeout"] = True
            raise AssertionError(f"owned command timed out: {entry['argv']}")
        entry.update(status=child.returncode, stdout_bytes=len(stdout), stderr_bytes=len(stderr),
                     stdout_sha256=hashlib.sha256(stdout).hexdigest(), stderr=stderr.decode(errors="replace"))
        return subprocess.CompletedProcess(entry["argv"], child.returncode, stdout, stderr)

    def run(self, argv, timeout=30):
        return self.finish(*self.spawn(argv), timeout)

    def product(self, *argv, timeout=30):
        return self.run([self.args.marsh, *argv], timeout)

    def receipt(self, command):
        listed = self.product("results", "--json")
        assert listed.returncode == 0, listed.stderr
        candidates = [job for job in json.loads(listed.stdout)["jobs"] if job["command"] == command]
        assert candidates, f"no public receipt for {command}"
        shown = self.product("results", "show", candidates[0]["job_id"], "--json")
        assert shown.returncode == 0, shown.stderr
        receipt = json.loads(shown.stdout)
        self.records.append({"receipt": receipt})
        return receipt

    def check(self, receipt, execution, code, cleanup="verified"):
        assert receipt["placement"] == "local" and receipt["execution"] == execution, receipt
        assert receipt["exit"]["code"] == code, receipt
        assert receipt["output_complete"] is True and receipt["cleanup"] == cleanup, receipt
        if cleanup == "verified":
            container = receipt["container_id"]
            assert re.fullmatch(r"[0-9a-f]{64}", container), receipt
            verify_container_deleted(self.run, [self.args.sbx, "exec", receipt["vm_id"]], container)

    def close(self):
        if self.started:
            stopped = self.product("stop", "--json", timeout=600)
            assert stopped.returncode == 0, (stopped.stdout, stopped.stderr, str(self.root))
        after = stock_vm_inventory(self.args.sbx)
        errors = stock_cleanup_errors(self.before, after)
        assert not errors, errors
        self.records.append({"scope_root": str(self.root), "stock_cleanup": "verified", "stock_after": after})
        # Preserve project/effect files alongside the receipts for independent review.

    def ordinary(self):
        warmed = self.product("--load", "fixture,typed-shell", "-c", "true", timeout=600)
        assert warmed.returncode == 0, warmed.stderr
        for label, script, expected_code, stdout, stderr in [
            ("normal7", "printf seven > normal7.effect; exit 7", 7, b"", b""),
            ("directstderr", "printf effect > directstderr.effect; printf 'data\\n'; printf 'diagnostic\\n' >&2", 0, b"data\n", b"diagnostic\n"),
        ]:
            completed = self.product("-c", shlex.join(["typed-shell", "-c", script]))
            assert (completed.returncode, completed.stdout, completed.stderr) == (expected_code, stdout, stderr)
            assert (self.project / f"{label}.effect").read_bytes() == (b"seven" if label == "normal7" else b"effect")
            self.check(self.receipt("typed-shell"), {"status": "exited", "code": expected_code}, expected_code)
        for label, mode in [("output", "output-pressure"), ("pids", "pid-pressure"), ("wall", "wall")]:
            ready = self.project / f".marsh-ready-{label}"
            release = self.project / f".marsh-go-{label}"
            child, entry = self.spawn([self.args.marsh, "-c", shlex.join(["fixture", "gate", label, mode])])
            try:
                deadline = time.monotonic() + 20
                while not ready.exists() and child.poll() is None and time.monotonic() < deadline:
                    time.sleep(0.01)
                assert ready.read_bytes() == b"ready", entry
                active = self.receipt("fixture")
                assert active["state"] == "running" and active["container_id"], active
                inspected = self.run([self.args.sbx, "exec", active["vm_id"], "docker", "inspect", active["container_id"]])
                assert inspected.returncode == 0 and json.loads(inspected.stdout)[0]["State"]["Running"], inspected.stderr
                release.write_bytes(b"go")
                completed = self.finish(child, entry, 45)
            finally:
                if child.poll() is None:
                    os.killpg(child.pid, signal.SIGTERM)
                    self.finish(child, entry, 5)
            assert completed.returncode != 0
            receipt = self.receipt("fixture")
            assert receipt["job_id"] == active["job_id"]
            # Limit outcomes have no fabricated native exit code; public fallback is125.
            self.check(receipt, {"status": "limit_exceeded", "resource": label}, None)
            assert not ready.exists() and not release.exists(), "worker gate files were not consumed"
            assert (self.project / "normal7.effect").read_bytes() == b"seven"
            if label == "output":
                assert 0 < len(completed.stdout) <= 1048576
                assert set(completed.stdout) == {ord("x")}
            else:
                assert completed.stdout == b""

    def setup_failure(self):
        # Docker's real create rejects memory=1 below its minimum. This is not a
        # fabricated worker response or an injected exit with a limit-like code.
        completed = self.product("-c", "fixture project-write must-not-exist forbidden", timeout=600)
        assert completed.returncode != 0 and not (self.project / "must-not-exist").exists()
        receipt = self.receipt("fixture")
        self.check(receipt, {"status": "setup_failed", "stage": "create"}, 125, "uncertain")
        assert receipt["container_id"] is None
        state = json.loads(self.product("status", "--json").stdout)
        assert any(worker["health"] == "quarantined" and worker["worker_id"] == receipt["worker_id"] for worker in state["workers"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    candidate_arguments(parser)
    for option in ("marsh", "sbx", "kit", "shell-kit", "source-tree", "source-revision", "evidence"):
        parser.add_argument("--" + option, required=True)
    args = parser.parse_args()
    if platform.system() != "Darwin" or platform.machine() != "arm64":
        parser.error("root stock UAT requires macOS arm64; never run stock/Cloud from a VM")
    for ref in (args.kit, args.shell_kit):
        if not re.fullmatch(r"[^\s]+@sha256:[0-9a-f]{64}", ref):
            parser.error("fixture and shell Kits must be immutable repository-qualified digests")
    args.marsh = str(pathlib.Path(args.marsh).resolve(strict=True))
    args.sbx = str(pathlib.Path(args.sbx).resolve(strict=True))
    guest, candidate = verify_candidate(args, args.marsh)
    report = {"kind": "typed-local-worker-stock-uat", "outcome": "failed", "candidate": candidate,
              "source": source_identity(pathlib.Path(args.source_tree), args.source_revision), "records": []}
    destination = pathlib.Path(args.evidence).resolve()
    destination.mkdir(mode=0o700, parents=True, exist_ok=True)
    try:
        for memory, case in [(134217728, "ordinary"), (1, "setup_failure")]:
            scope = Scope(args, guest, report["records"], memory)
            try:
                getattr(scope, case)()
            finally:
                scope.close()
        report["outcome"] = "passed"
    except Exception as error:
        report["failure"] = repr(error)
    (destination / "result.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({key: report[key] for key in ("outcome", "kind")}))
    return 0 if report["outcome"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
