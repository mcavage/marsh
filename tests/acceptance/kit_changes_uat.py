#!/usr/bin/env python3
"""Kit changes without waiting (docs/kits.md "Updates", "Removing a command"),
and garbage collection of the home's own VMs (`marsh gc`).

Black-box, through the installed CLI on stock SBX, in an isolated
MARSH_HOME/control scope that removes only VMs from its own ownership map:

- K01: a dev build runs the source Kits it installed, even when an earlier
  release's automatic update pinned the packaged command to a published image
  in `commands.json` (the `make dev DEV_PREFIX=…` over a release home case).
- K02: `marsh kit uninstall` while the Kit has a running job returns 0, the job
  runs to its end on the old VM, `marsh status --json` lists the pending change,
  and the VM is removed after the job ends.
- K03: installing a different Kit under that name while the old one still runs
  a job: the new job runs on the new Kit's VM at once, the old job finishes on
  the old VM, which is removed afterwards.
- K04: a recorded Kit VM that is stopped and superseded (its command is gone
  from `commands.json`) is removed when the next daemon starts; status lists it.
- K05: with a short idle limit, `marsh gc` removes a stopped Kit VM unused for
  longer, keeps a running one and the VM of a running job, and lists `marsh-`
  VMs it does not record as not ours; `--dry-run` removes nothing.
- K06: jobs started while `marsh gc` removes their stopped VM all succeed with
  verified cleanup (admission vs. collection).
- K07: the daemon is killed while a Kit change drains a VM with a running job;
  the next daemon's collection keeps that VM (and its job's container) until
  `marsh reset`.
- K08: an attached shell's VM is kept.
Cloud and detached runs use cloud sandboxes, which are neither in local
`sbx ls` nor in the ownership map garbage collection reads; not exercised here.

Needs a dev install (`--prefix`, `make dev`): its packaged `shell` is the source
Kit `kits/marsh-shell`; the published `docker.io/mcavage/marsh-shell` image
stands in for what an update pinned.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import subprocess
import sys
import time
from typing import Any

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from harness import (ScenarioSuite, request_authenticated_daemon_shutdown, require,  # noqa: E402
                     scenario_parser, stable_process_identity, suite_main, text)
from provenance import stock_baseline  # noqa: E402

SCENARIOS = {
    "K01-dev-build-runs-source-kits": "dev_build_runs_source_kits",
    "K02-uninstall-while-running": "uninstall_while_running",
    "K03-new-kit-while-old-runs": "new_kit_while_old_runs",
    "K04-gc-superseded-stopped-at-start": "gc_superseded_at_start",
    "K05-gc-idle-and-in-use": "gc_idle_and_in_use",
    "K06-gc-vs-admission": "gc_vs_admission",
    "K07-crash-mid-drain": "crash_mid_drain",
    "K08-gc-keeps-attached-shell": "gc_keeps_attached_shell",
}
ORDER = list(SCENARIOS)
PUBLISHED_SHELL = "docker.io/mcavage/marsh-shell"
# Drained VMs are retired by the daemon's background pass (each minute).
DRAIN_TIMEOUT = 240


def published_digest(repository: str) -> str:
    """The digest `repository:latest` names now (any published image will do)."""
    shown = subprocess.run(["docker", "buildx", "imagetools", "inspect", f"{repository}:latest"],
                           capture_output=True, text=True, timeout=120, check=True).stdout
    digest = re.search(r"^Digest:\s+(sha256:[0-9a-f]{64})$", shown, re.MULTILINE)
    if not digest:
        raise RuntimeError(f"no digest for {repository}:latest: {shown[:400]}")
    return f"{repository}@{digest.group(1)}"


class KitChanges(ScenarioSuite):
    LABEL = "kit-changes"
    EVIDENCE = "kit-changes.json"
    SCENARIOS = SCENARIOS
    ORDER = ORDER

    def __init__(self, arguments: argparse.Namespace) -> None:
        super().__init__(arguments)
        self.dev_install = (arguments.guest_artifacts / "dev-enabled").is_file()
        self.shell_image = published_digest(PUBLISHED_SHELL)
        # What an earlier release's automatic update left in this home's
        # registry, before this (dev) marsh's daemon first starts.
        registry = self.control_home / "commands.json"
        mapping = json.loads(registry.read_text())
        mapping["shell"] = self.shell_image
        registry.write_text(json.dumps(mapping, sort_keys=True) + "\n", encoding="utf-8")

    # ---- observation ----------------------------------------------------
    def status(self) -> dict[str, Any]:
        return self.document("status", "--json")

    def pending(self) -> list[dict[str, Any]]:
        return (self.status().get("kit_updates") or {}).get("pending", [])

    def recorded_vms(self) -> set[str]:
        path = self.control_home / "vm-ownership.json"
        if not path.is_file():
            return set()
        return set(json.loads(path.read_text()).get("vms", {}))

    def start_hold(self, command: str, seconds: int) -> tuple[subprocess.Popen[bytes], dict[str, Any]]:
        """A job of `command` that runs for `seconds`; returns it once running."""
        before = self.job_ids()
        process = self.spawn([self.marsh, "run", command, "-c", f"echo STARTED; sleep {seconds}; echo FINISHED"],
                             self.project)
        found: list[dict[str, Any]] = []

        def running() -> bool:
            found[:] = [r for r in self.running(before) if r.get("command") == command]
            return bool(found)
        self.wait_until(running, 600, f"a running {command} job", (process,))
        return process, found[0]

    def finish_hold(self, process: subprocess.Popen[bytes], what: str) -> None:
        status, stdout, stderr = self.reap(process, 300)
        require(status == 0 and b"FINISHED" in stdout,
                f"{what}: the running job did not finish normally ({status}): "
                f"{text(stdout)[-300:]!r} {text(stderr)[-600:]!r}")

    def wait_drained(self, command: str, vm: str) -> None:
        self.wait_until(lambda: not any(p.get("command") == command for p in self.pending()),
                        DRAIN_TIMEOUT, f"{command}'s pending change to finish")
        self.wait_until(lambda: vm not in self.recorded_vms(), DRAIN_TIMEOUT, f"the old VM {vm} to be removed")

    # ---- scenarios --------------------------------------------------------
    def dev_build_runs_source_kits(self) -> None:
        """A dev build runs its packaged source Kit, not the published image an update pinned."""
        require(self.dev_install, "needs a dev install (`make dev DEV_PREFIX=…`): libexec/marsh/dev-enabled")
        ran = self.cli(["run", "shell", "-c", "printf SOURCE_KIT_OK"], self.project, timeout=1800)
        require(ran.returncode == 0 and b"SOURCE_KIT_OK" in ran.stdout,
                f"shell failed ({ran.returncode}): {text(ran.stdout)[-300:]!r} {text(ran.stderr)[-800:]!r}")
        registered = {c["name"]: c["reference"] for c in self.status().get("commands", [])}
        require(registered.get("shell", "").startswith("local-v3:"),
                f"shell does not run the installed source Kit: {registered.get('shell')!r}")
        mapping = json.loads((self.control_home / "commands.json").read_text())
        require("shell" not in mapping, f"commands.json still pins shell: {mapping!r}")
        shells = [r for r in (self.show(job) for job in self.job_ids()) if r.get("command") == "shell"]
        require(shells and all(str(r.get("kit_profile", "")).startswith("local-v3:") for r in shells),
                f"a shell job did not run the source Kit: {[r.get('kit_profile') for r in shells]!r}"[:900])

    def uninstall_while_running(self) -> None:
        """`marsh kit uninstall` with a running job: exit 0, the job finishes, the VM goes after."""
        installed = self.cli(["kit", "install", "pinned", self.shell_image], self.project, timeout=1800)
        require(installed.returncode == 0, f"install failed: {text(installed.stderr)[-800:]}")
        process, job = self.start_hold("pinned", 45)
        vm = job["vm_id"]
        removed = self.cli(["kit", "uninstall", "pinned"], self.project, timeout=120)
        out = text(removed.stdout) + text(removed.stderr)
        require(removed.returncode == 0 and "removed pinned" in out
                and "pinned is removed when its running jobs end" in out,
                f"uninstall with a running job ({removed.returncode}): {out[-800:]!r}")
        pending = [p for p in self.pending() if p.get("command") == "pinned"]
        require(len(pending) == 1 and pending[0].get("change") == "uninstall" and pending[0].get("vm") == vm
                and pending[0].get("running_jobs") == 1 and pending[0].get("from") == self.shell_image,
                f"status lacks the pending uninstall: {pending!r}")
        gone = self.cli(["run", "pinned", "-c", "true"], self.project, timeout=120)
        require(gone.returncode != 0, "a new call still started the removed Kit")
        require(process.poll() is None, "the running job was stopped by the uninstall")
        self.finish_hold(process, "K02")
        self.wait_drained("pinned", vm)

    def new_kit_while_old_runs(self) -> None:
        """A different Kit under the name while the old one runs a job: new jobs use it at once."""
        installed = self.cli(["kit", "install", "swap", self.shell_image], self.project, timeout=1800)
        require(installed.returncode == 0, f"install failed: {text(installed.stderr)[-800:]}")
        process, job = self.start_hold("swap", 60)
        old_vm = job["vm_id"]
        removed = self.cli(["kit", "uninstall", "swap"], self.project, timeout=120)
        require(removed.returncode == 0, f"uninstall failed: {text(removed.stderr)[-800:]}")
        again = self.cli(["kit", "install", "swap", self.kit], self.project, timeout=1800)
        require(again.returncode == 0, f"reinstall with a running job failed ({again.returncode}): "
                                       f"{text(again.stdout)[-300:]!r} {text(again.stderr)[-800:]!r}")
        before = self.job_ids()
        fresh = self.cli(["run", "swap", "identity"], self.project, timeout=600)
        require(fresh.returncode == 0 and self.identity(fresh.stdout),
                f"the new Kit did not run ({fresh.returncode}): {text(fresh.stderr)[-800:]!r}")
        new = [self.show(j) for j in self.job_ids() - before]
        require(len(new) == 1 and new[0].get("vm_id") != old_vm and new[0].get("image") == self.kit,
                f"the new job did not run on the new Kit's VM: {new!r}"[:900])
        require(process.poll() is None, "the old Kit's running job was stopped")
        self.finish_hold(process, "K03")
        self.wait_drained("swap", old_vm)
        require(new[0]["vm_id"] in self.recorded_vms(), "the new Kit's VM was removed")

    def restart_daemon(self, settings: dict[str, str | None], between=None) -> None:
        """Stop the idle daemon gracefully (its VMs stay), run `between`, and start the
        next one with `settings` (MARSH_* daemon-start variables; None unsets)."""
        self.document("status", "--json")
        pid, token = self.owned_daemon_pid, self.owned_daemon_control_token
        require(pid and token and stable_process_identity(pid) == self.owned_daemon_process_identity,
                "cannot verify the owned daemon process before restarting it")

        def stopped() -> bool:
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
            setattr(self, field, None)
        for name, value in settings.items():
            for environment in (self.environment, self.host_env):
                if value is None:
                    environment.pop(name, None)
                else:
                    environment[name] = value
        if between:
            between()
        self.document("status", "--json")

    def gc_recent(self) -> list[dict[str, Any]]:
        return (self.status().get("vm_gc") or {}).get("recent", [])

    def vm_of(self, command: str) -> str:
        before = self.job_ids()
        argv = ["identity"] if command == "fixture" else ["-c", "true"]
        ran = self.cli(["run", command, *argv], self.project, timeout=1800)
        require(ran.returncode == 0, f"{command} failed ({ran.returncode}): {text(ran.stderr)[-800:]!r}")
        fresh = [self.show(job) for job in self.job_ids() - before]
        require(len(fresh) == 1 and fresh[0].get("vm_id"), f"no job receipt with a VM: {fresh!r}"[:600])
        return fresh[0]["vm_id"]

    def gc_superseded_at_start(self) -> None:
        """A stopped, superseded Kit VM this home records is removed when the daemon starts."""
        installed = self.cli(["kit", "install", "retired", self.shell_image], self.project, timeout=1800)
        require(installed.returncode == 0, f"install failed: {text(installed.stderr)[-800:]}")
        old = self.vm_of("retired")
        keep = self.vm_of("fixture")

        def as_an_older_marsh_left_it() -> None:  # the command gone, its VM stopped
            registry = self.control_home / "commands.json"
            mapping = json.loads(registry.read_text())
            mapping.pop("retired")
            registry.write_text(json.dumps(mapping, sort_keys=True) + "\n", encoding="utf-8")
            stopped = subprocess.run([self.sbx, "stop", old], capture_output=True, text=True, timeout=120)
            require(stopped.returncode == 0, f"sbx stop {old}: {stopped.stderr[-400:]}")
            # A superseded VM used (stock counts a stop) in the last ten
            # minutes, or within the idle limit if shorter, is kept: it may be
            # a Kit being installed.
            time.sleep(25)
        self.restart_daemon({"MARSH_VM_IDLE_SECONDS": "20"}, as_an_older_marsh_left_it)
        try:
            self.superseded_gone(old, keep)
        finally:
            self.restart_daemon({"MARSH_VM_IDLE_SECONDS": None})

    def superseded_gone(self, old: str, keep: str) -> None:
        self.wait_until(lambda: old not in self.recorded_vms(), 180, f"the superseded VM {old} to be removed")
        listed = subprocess.run([self.sbx, "ls", "--json"], capture_output=True, text=True, timeout=60)
        require(old not in listed.stdout, f"{old} is still listed by sbx")
        recent = [entry for entry in self.gc_recent() if entry.get("vm") == old]
        require(recent and recent[-1].get("action") == "removed"
                and recent[-1].get("reason", "").startswith("superseded"),
                f"status does not report the removal: {self.gc_recent()!r}")
        require(keep in self.recorded_vms(), f"the current fixture VM {keep} was removed")

    def gc_idle_and_in_use(self) -> None:
        """`marsh gc` removes an idle VM, keeps one a job uses, and never touches others'."""
        self.restart_daemon({"MARSH_VM_IDLE_SECONDS": "20"})
        try:
            idle = self.vm_of("fixture")
            installed = self.cli(["kit", "install", "busy", self.shell_image], self.project, timeout=1800)
            require(installed.returncode == 0, f"install failed: {text(installed.stderr)[-800:]}")
            process, job = self.start_hold("busy", 120)
            busy = job["vm_id"]
            self.sbx_stop(idle)
            time.sleep(25)  # past the idle limit, counting the stop as a use
            dry = self.cli(["gc", "--dry-run", "--json"], self.project, timeout=300)
            require(dry.returncode == 0, f"gc --dry-run failed: {text(dry.stderr)[-800:]}")
            planned = {entry["vm"]: entry for entry in json.loads(dry.stdout)["entries"]}
            require(planned.get(idle, {}).get("action") == "would_remove", f"dry run: {planned!r}"[:900])
            require(idle in self.recorded_vms(), "a dry run removed a VM")
            done = self.cli(["gc", "--json"], self.project, timeout=600)
            require(done.returncode == 0, f"gc failed ({done.returncode}): {text(done.stdout)[-600:]!r} "
                                          f"{text(done.stderr)[-800:]!r}")
            report = {entry["vm"]: entry for entry in json.loads(done.stdout)["entries"]}
            require(report.get(idle, {}).get("action") == "removed"
                    and report[idle]["reason"].startswith("unused for"), f"idle VM: {report.get(idle)!r}")
            require(report.get(busy, {}).get("action") == "kept"
                    and "job" in report[busy]["reason"], f"busy VM: {report.get(busy)!r}")
            require(idle not in self.recorded_vms() and busy in self.recorded_vms(),
                    "gc removed the wrong VM")
            others = [entry for entry in report.values() if entry["action"] == "not_ours"]
            listed = json.loads(subprocess.run([self.sbx, "ls", "--json"], capture_output=True, text=True,
                                               timeout=60).stdout or "[]")
            listed = listed.get("sandboxes", []) if isinstance(listed, dict) else listed
            foreign = {row["name"] for row in listed if row["name"].startswith("marsh-")} - self.recorded_vms()
            require({entry["vm"] for entry in others} == foreign,
                    f"not-ours list {sorted(e['vm'] for e in others)} != {sorted(foreign)}")
            require(process.poll() is None, "gc stopped the running job")
            self.finish_hold(process, "K05")
            again = self.vm_of("fixture")
            require(again != idle, "the removed VM was not recreated under a new name")
        finally:
            self.restart_daemon({"MARSH_VM_IDLE_SECONDS": None})

    def sbx_stop(self, vm: str) -> None:
        stopped = subprocess.run([self.sbx, "stop", vm], capture_output=True, text=True, timeout=180)
        require(stopped.returncode == 0, f"sbx stop {vm}: {stopped.stderr[-400:]}")

    def sbx_status(self, vm: str) -> str | None:
        listed = json.loads(subprocess.run([self.sbx, "ls", "--json"], capture_output=True, text=True,
                                           timeout=60).stdout or "[]")
        listed = listed.get("sandboxes", []) if isinstance(listed, dict) else listed
        return next((row.get("status") for row in listed if row.get("name") == vm), None)

    def gc_report(self) -> dict[str, dict[str, Any]]:
        done = self.cli(["gc", "--json"], self.project, timeout=600)
        require(done.returncode == 0, f"gc failed ({done.returncode}): {text(done.stdout)[-600:]!r} "
                                      f"{text(done.stderr)[-800:]!r}")
        return {entry["vm"]: entry for entry in json.loads(done.stdout)["entries"]}

    def gc_vs_admission(self) -> None:
        """Jobs started while `marsh gc` removes their stopped VM succeed with verified cleanup."""
        self.restart_daemon({"MARSH_VM_IDLE_SECONDS": "5"})
        try:
            for round_ in range(3):
                vm = self.vm_of("fixture")
                self.sbx_stop(vm)
                time.sleep(7)
                before = self.job_ids()
                runner = self.spawn([self.marsh, "run", "fixture", "identity"], self.project)
                collector = self.spawn([self.marsh, "gc", "--json"], self.project)
                status, out, err = self.reap(runner, 900)
                require(status == 0 and self.identity(out),
                        f"round {round_}: the job failed ({status}): {text(err)[-800:]!r}")
                gc_status, gc_out, gc_err = self.reap(collector, 900)
                require(gc_status == 0, f"round {round_}: gc failed: {text(gc_err)[-800:]!r}")
                entry = {e["vm"]: e for e in json.loads(gc_out)["entries"]}.get(vm, {})
                require(entry.get("action") in ("removed", "kept", None),
                        f"round {round_}: {entry!r}")
                receipts = [self.show(job) for job in self.job_ids() - before]
                require(len(receipts) == 1 and receipts[0].get("cleanup") == "verified",
                        f"round {round_}: {receipts!r}"[:900])
        finally:
            self.restart_daemon({"MARSH_VM_IDLE_SECONDS": None})

    def crash_mid_drain(self) -> None:
        """A restart mid-drain never lets collection remove a VM whose job may still run."""
        installed = self.cli(["kit", "install", "draining", self.shell_image], self.project, timeout=1800)
        require(installed.returncode == 0, f"install failed: {text(installed.stderr)[-800:]}")
        process, job = self.start_hold("draining", 150)
        vm = job["vm_id"]
        removed = self.cli(["kit", "uninstall", "draining"], self.project, timeout=120)
        require(removed.returncode == 0, f"uninstall failed: {text(removed.stderr)[-800:]}")
        pid = self.owned_daemon_pid
        subprocess.run(["kill", "-KILL", str(pid)], check=True)
        self.wait_until(lambda: stable_process_identity(pid) is None, 60, "the killed daemon to exit")
        for field in ("owned_daemon_id", "owned_daemon_pid", "owned_daemon_process_identity",
                      "owned_daemon_control_token"):
            setattr(self, field, None)
        self.document("status", "--json")  # its successor, which this scope now owns
        report = self.gc_report()
        require(report.get(vm, {}).get("action") == "kept", f"the draining VM: {report.get(vm)!r}")
        require(vm in self.recorded_vms() and self.sbx_status(vm) == "running",
                f"{vm} was removed or stopped under its job")
        ps = subprocess.run([self.sbx, "exec", "-u", "root", vm, "docker", "ps", "-q"],
                            capture_output=True, text=True, timeout=120)
        require(ps.returncode == 0 and ps.stdout.strip(), f"the job's container is gone: {ps.stdout!r} {ps.stderr!r}")
        self.kill(process)
        # Its receipt is unknown now; only an operator reset removes the VM.
        reset = self.cli(["reset", "--json"], self.project, timeout=900)
        require(reset.returncode == 0, f"reset failed: {text(reset.stderr)[-800:]}")
        require(vm not in self.recorded_vms(), "reset left the VM")

    def gc_keeps_attached_shell(self) -> None:
        """An attached shell's VM is kept, whatever the idle limit."""
        self.restart_daemon({"MARSH_VM_IDLE_SECONDS": "1"})
        shell = subprocess.Popen([self.marsh, "--no-config", "--noprofile", "--norc", "-s"], cwd=self.project,
                                 env=self.host_env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 stderr=subprocess.STDOUT, start_new_session=True)
        self.live.append(shell)
        try:
            assert shell.stdin is not None
            shell.stdin.write(b"printf 'ATTACHED\\n'\n")
            shell.stdin.flush()
            self.wait_until(lambda: any(s.get("state") == "attached" for s in self.status().get("shells", [])),
                            600, "an attached shell", (shell,))
            time.sleep(3)
            shells = {e["vm"]: e for e in self.gc_report().values() if e.get("purpose") == "shell"}
            require(shells and all(e["action"] == "kept" for e in shells.values()),
                    f"shell VMs: {shells!r}")
            shell.stdin.write(b"exit\n")
            shell.stdin.flush()
            shell.wait(timeout=120)
        finally:
            self.kill(shell)
            self.restart_daemon({"MARSH_VM_IDLE_SECONDS": None})

    def run_all(self) -> None:
        self.stock_before = stock_baseline(self.sbx)
        self.source["stock_before"] = self.stock_before
        self.source["published_shell"] = self.shell_image
        self.run_scenarios()


def main() -> int:
    parser = scenario_parser(
        __doc__, SCENARIOS, preflight=False,
        kit_help="fixture Kit: immutable OCI ref, native v3 source dir, or a commands.json whose \"fixture\" key "
                 "holds the ref (K03 installs it as the replacement Kit)")
    return suite_main(KitChanges, parser)


if __name__ == "__main__":
    raise SystemExit(main())
