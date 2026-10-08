#!/usr/bin/env python3
"""Shell choice (docs/shells.md) black-box acceptance.

For each of the shell VM's own `bash` and `zsh` (`marsh --shell NAME`), drives
only public callers (host `marsh` sessions with `-c`, an interactive `marsh` on
a fresh controlling terminal, the host CLI) and observes only bytes, exit
statuses, receipts and lineage from `marsh jobs`, and stock `sbx exec <owned
vm> docker inspect`. Checks: the chosen shell really runs; a registered name
(the fixture) is a Kit job with a verified-cleanup receipt; `marsh split` /
`join` and `marsh fanout` / `collect` work; the user's own ~/.bashrc or
~/.zshrc in the selected home is honored while the session command directory
stays first on PATH; Ctrl-C returns to the prompt; exit statuses propagate;
selection precedence (--shell, MARSH_SHELL, `marsh config shell`). Runs in an
isolated MARSH_HOME/control scope and removes only VMs from its own ownership
map (run.py cleanup).
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import subprocess
import sys
import time
import traceback

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from provenance import stock_cleanup_errors, stock_vm_inventory  # noqa: E402
from processes_uat import Processes  # noqa: E402
from workspaces_uat import Fail, require, text  # noqa: E402

SHELLS = ("bash", "zsh")
SCENARIOS = {
    "S01-identity-and-status": "identity_and_status",
    "S02-registered-command-job": "registered_command_job",
    "S03-split-join": "split_join",
    "S04-fanout-collect": "fanout_collect",
    "S05-interactive-rc-ctrl-c-exit": "interactive",
    "S06-selection-precedence": "selection",
}
ORDER = list(SCENARIOS)
PER_SHELL = {"S01-identity-and-status", "S02-registered-command-job", "S03-split-join",
             "S04-fanout-collect", "S05-interactive-rc-ctrl-c-exit"}
BIN = r"/tmp/marsh-commands-[A-Za-z0-9]+"
RC = {
    "bash": (".bashrc", "alias marshalias='echo alias-ok'\nexport PATH=/opt/user-first:$PATH\n"
                        "PS1='bash-ready$ '\n"),
    "zsh": (".zshrc", "alias marshalias='echo alias-ok'\npath=(/opt/user-first $path)\n"
                      "PROMPT='zsh-ready$ '\n"),
}


class Shells(Processes):
    def __init__(self, arguments: argparse.Namespace) -> None:
        super().__init__(arguments)
        self.selected = [s for s in ORDER if not arguments.only or s in arguments.only]
        self.shells = [s for s in SHELLS if not arguments.shell or s in arguments.shell]
        self.host_env.pop("MARSH_SHELL", None)

    def session(self, shell: str, script: str, **kw: object) -> subprocess.CompletedProcess[bytes]:
        return self.call(f"marsh-{shell}", [self.marsh, "--shell", shell, "-c", script], self.project, **kw)

    # ---- scenarios --------------------------------------------------------
    def identity_and_status(self, shell: str) -> None:
        """`-c` runs in the chosen shell; its exit status and signal status propagate; PATH starts with the
        session command directory; `acp`/`mcp`/`ps` resolve there too."""
        version = "$ZSH_VERSION" if shell == "zsh" else "$BASH_VERSION"
        completed = self.session(shell, f'echo "v={version}"; echo "shell=$SHELL"; echo "path=$PATH"; '
                                        "for n in fixture acp mcp ps; do echo \"$n=$(command -v $n)\"; done")
        v = self.kv(completed.stdout)
        require(completed.returncode == 0 and re.fullmatch(r"5\.\d.*", v.get("v", "")),
                f"{shell} -c did not run {shell}: {completed.stdout!r} {text(completed.stderr)[-400:]!r}")
        require(v.get("shell", "").endswith(f"/{shell}"), f"SHELL is not {shell}: {v.get('shell')!r}")
        require(re.match(BIN + ":", v.get("path", "")), f"PATH does not start with the session bin: {v}")
        for name in ("fixture", "acp", "mcp", "ps"):
            require(re.fullmatch(BIN + "/" + name, v.get(name, "")), f"{name} is not a session link: {v}")
        status = self.session(shell, "echo out; exit 7")
        require(status.returncode == 7 and status.stdout == b"out\n", f"exit 7 -> {status.returncode} {status.stdout!r}")
        killed = self.session(shell, "kill -TERM $$")
        require(killed.returncode == 143, f"{shell} killed by TERM -> marsh exited {killed.returncode}")
        ps = self.session(shell, "ps --marsh --json")
        try:
            json.loads(ps.stdout)
        except ValueError:
            raise Fail(f"ps --marsh --json from {shell}: {ps.returncode} {ps.stdout[:300]!r} "
                       f"{text(ps.stderr)[-300:]!r}") from None
        plain = self.session(shell, "ps -o comm= -p $$; true")
        require(plain.returncode == 0 and plain.stdout.strip() == shell.encode(),
                f"plain ps from {shell} is not the system's: {plain.stdout!r} {text(plain.stderr)[-300:]!r}")
        acp = self.session(shell, "acp list --json")
        require(acp.returncode == 0, f"acp list from {shell}: {acp.returncode} {text(acp.stderr)[-300:]!r}")

    def registered_command_job(self, shell: str) -> None:
        """A registered name from bash/zsh is a session-root Kit job with a verified-cleanup receipt; `marsh run`
        works too; stdin/stdout/status are relayed."""
        before = self.job_ids()
        completed = self.session(shell, "fixture identity; echo rc=$?; marsh run fixture identity >/dev/null; "
                                        "echo run=$?")
        out = text(completed.stdout)
        require(completed.returncode == 0 and "rc=0" in out and "run=0" in out and self.identity(completed.stdout),
                f"fixture from {shell}: {out[-600:]!r} {text(completed.stderr)[-600:]!r}")
        receipts = self.settle(before, 2)
        for receipt in receipts:
            require(receipt.get("command") == "fixture" and receipt.get("state") == "finished",
                    f"receipt: {receipt!r}"[:600])
            require(str(self.lin(receipt).get("parent", "")).startswith("session"),
                    f"not a session root job: {self.lin(receipt)}")
        self.verify_deleted(receipts)

    def split_join(self, shell: str) -> None:
        """`marsh split -n -b a='echo x' ::: b fixture identity | marsh join` from bash/zsh."""
        before = self.job_ids()
        state = self.user_state(self.project)
        completed = self.session(shell, "marsh split -n -b a='echo x' ::: b fixture identity | marsh join; "
                                        "echo rc=$?")
        out = text(completed.stdout)
        require("rc=0" in out and re.search(r"(?m)^x$", out) and '"cwd"' in out,
                f"split/join from {shell}: {out[-800:]!r} {text(completed.stderr)[-600:]!r}")
        receipts = self.settle(before, 1)
        require(receipts[0].get("command") == "fixture", f"split branch receipt: {receipts[0]!r}"[:600])
        self.assert_user_unchanged(self.project, state, f"S03-{shell}")
        self.verify_deleted(receipts)

    def fanout_collect(self, shell: str) -> None:
        """`marsh fanout ... | marsh collect` from bash/zsh: ordered frames, first failing status."""
        before = self.job_ids()
        completed = self.session(shell, "marsh fanout -n -b a='echo x' ::: id fixture identity "
                                        "::: bad sh -c 'exit 4' | marsh collect; echo rc=$?")
        out = text(completed.stdout)
        require("== a (complete) ==\nx\n" in out and "== id (complete) ==" in out
                and "== bad (failed: 4) ==" in out and "rc=4" in out,
                f"fanout from {shell}: {out[-800:]!r} {text(completed.stderr)[-600:]!r}")
        require(out.index("== a ") < out.index("== id ") < out.index("== bad "), "branches out of order")
        self.verify_deleted(self.settle(before, 1))

    def interactive(self, shell: str) -> None:
        """Interactive bash/zsh on a PTY: the user's rc alias works, the session bin stays first on PATH
        after the rc prepends, a registered name works, Ctrl-C (a local command, a Kit job) returns to the
        prompt, `exit 5` is marsh's status."""
        name, contents = RC[shell]
        rc = self.guest_home / name
        rc.write_text(contents)
        before = self.job_ids()
        process, master = self.pty_session_with(["--shell", shell])
        output = bytearray()
        prompt = f"{shell}-ready\\$ "
        try:
            self.pty_expect(master, output, prompt, 600)
            mark = len(output)
            os.write(master, b'marshalias; echo "first=${PATH%%:*}"; echo "user=$PATH"\r')
            self.pty_expect(master, output, r"alias-ok\r?\n", 60, mark)
            found = self.pty_expect(master, output, r"first=(\S+)\r?\n", 60, mark)
            require(re.fullmatch(BIN, found.group(1)), f"session bin not first after rc: {found.group(1)!r}")
            self.pty_expect(master, output, r"user=\S*/opt/user-first", 60, mark)
            mark = len(output)
            os.write(master, b"fixture identity >/dev/null; echo fx=$?\r")
            done = self.pty_expect(master, output, r"fx=0", 600, mark)
            self.pty_expect(master, output, prompt, 60, mark + done.end())
            mark = len(output)
            os.write(master, b"sleep 300\r")
            echoed = self.pty_expect(master, output, r"sleep 300", 30, mark)
            time.sleep(2)
            os.write(master, b"\x03")
            self.pty_expect(master, output, prompt, 30, mark + echoed.end())
            mark = len(output)
            os.write(master, b"echo after=$?\r")
            self.pty_expect(master, output, r"after=130", 30, mark)
            mark = len(output)
            os.write(master, b"fixture hold 300\r")
            ready = self.pty_expect(master, output, r"READY", 120, mark)
            os.write(master, b"\x03")
            self.pty_expect(master, output, prompt, 60, mark + ready.end())
            mark = len(output)
            os.write(master, b"echo job=$?\r")
            found = self.pty_expect(master, output, r"job=(\d+)", 30, mark)
            require(found.group(1) != "0", "an interrupted Kit job reported success")
            mark = len(output)
            os.write(master, b"exit 5\r")
            deadline = time.monotonic() + 60
            while process.poll() is None and time.monotonic() < deadline:
                self.pty_drain_once(master, output)
            require(process.poll() == 5, f"interactive {shell} `exit 5` -> {process.poll()}: "
                                         f"{text(bytes(output[mark:]))[-600:]!r}")
        finally:
            if process.poll() is None:
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
            rc.unlink(missing_ok=True)
            self.records.append({"scenario": f"S05-{shell}", "output": text(bytes(output))[-6000:]})
        receipts = self.settle(before, 2)
        # agents.md: Ctrl-C at the root records the interrupted job `cancelled`, never `finished 130`.
        held = [r for r in receipts if "hold" in (r.get("args") or [])]
        require(len(held) == 1 and held[0].get("state") == "cancelled",
                f"Ctrl-C'd Kit job not recorded cancelled: "
                f"{[(r.get('args'), r.get('state'), (r.get('exit') or {}).get('code')) for r in receipts]}")
        self.verify_deleted(receipts)

    def selection(self, _shell: str | None = None) -> None:
        """`marsh config shell` sets the home default; MARSH_SHELL overrides it; --shell overrides both;
        an unknown name is a usage error before any VM work; Brush stays the default."""
        probe = 'echo "${ZSH_VERSION:+zsh}${MARSH_SHELL:-brush}"'
        default = self.cli(["-c", probe], self.project)
        require(default.stdout == b"brush\n", f"default shell is not Brush: {default.stdout!r}")
        config = self.home / "config.json"
        try:
            shown = self.cli(["config", "shell"], self.project, timeout=30)
            require(shown.stdout == b"marsh\n", f"config shell (unset): {shown.stdout!r}")
            setting = self.cli(["config", "shell", "zsh"], self.project, timeout=30)
            require(setting.returncode == 0 and json.loads(config.read_text()) == {"shell": "zsh"},
                    f"config shell zsh: {setting.returncode} {text(setting.stderr)!r}")
            require(config.stat().st_mode & 0o777 == 0o600, "config.json is not owner-only")
            self.check_probe([], {}, "zshzsh")
            self.check_probe([], {"MARSH_SHELL": "bash"}, "bash")
            self.check_probe(["--shell", "marsh"], {"MARSH_SHELL": "bash"}, "brush")
            bad = self.cli(["--shell", "fish", "-c", "true"], self.project, timeout=30)
            require(bad.returncode == 2 and b"unknown shell" in bad.stderr, f"--shell fish: {bad!r}")
            bad = self.cli(["config", "shell", "fish"], self.project, timeout=30)
            require(bad.returncode == 2, f"config shell fish -> {bad.returncode}")
        finally:
            config.unlink(missing_ok=True)

    def check_probe(self, flags: list[str], env: dict[str, str], want: str) -> None:
        probe = 'echo "${ZSH_VERSION:+zsh}${MARSH_SHELL:-brush}"'
        saved = dict(self.host_env)
        self.host_env.update(env)
        try:
            completed = self.cli([*flags, "-c", probe], self.project)
        finally:
            self.host_env = saved
        require(completed.stdout == f"{want}\n".encode(),
                f"{flags} {env}: want {want!r}, got {completed.stdout!r} {text(completed.stderr)[-300:]!r}")

    # ---- PTY helpers ------------------------------------------------------
    def pty_session_with(self, flags: list[str]) -> tuple[subprocess.Popen[bytes], int]:
        import fcntl
        import pty
        import termios
        master, slave = pty.openpty()

        def controlling() -> None:
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        process = subprocess.Popen([self.marsh, *flags], cwd=self.project, env={**self.host_env, "TERM": "xterm"},
                                   stdin=slave, stdout=slave, stderr=slave, preexec_fn=controlling)
        os.close(slave)
        self.live.append(process)
        return process, master

    @staticmethod
    def pty_drain_once(master: int, output: bytearray) -> None:
        import select
        ready, _, _ = select.select([master], [], [], 0.25)
        if ready:
            try:
                chunk = os.read(master, 65536)
            except OSError:
                time.sleep(0.1)
                return
            if b"\x1b[6n" in chunk:
                os.write(master, b"\x1b[1;1R")
            output.extend(chunk)

    # ---- driver -----------------------------------------------------------
    def run_all(self) -> None:
        self.stock_before = stock_vm_inventory(self.sbx)
        self.source["stock_before"] = self.stock_before
        (self.project / "README.md").write_text("shells acceptance\n")
        self.git(self.project, "init", "-q", "-b", "main")
        self.git(self.project, "add", "-A")
        self.git(self.project, "commit", "-q", "-m", "seed")
        self.run([self.marsh, "--load", "fixture", "-c", "true"], timeout=900)
        failures = []
        cases = []
        for scenario in self.selected:
            if scenario in PER_SHELL:
                cases.extend((scenario, shell) for shell in self.shells)
            else:
                cases.append((scenario, None))
        for scenario, shell in cases:
            label = scenario + (f"[{shell}]" if shell else "")
            began = time.monotonic()
            try:
                getattr(self, SCENARIOS[scenario])(shell)
                outcome, failure = "passed", None
            except Fail as error:
                outcome, failure = "failed", str(error)
            except Exception as error:  # a product or harness break is still a failure
                outcome, failure = "failed", f"{type(error).__name__}: {error}"
                self.records.append({"scenario": label, "traceback": traceback.format_exc()})
            finally:
                for process in self.live:
                    self.kill(process)
                self.live.clear()
            self.results.append({"scenario": label, "outcome": outcome, "failure": failure,
                                 "seconds": round(time.monotonic() - began, 1)})
            print(f"shells: {label}: {outcome}" + (f": {failure[:500]}" if failure else ""), flush=True)
            if failure:
                failures.append(label)
                if self.fail_fast:
                    break
        self.document("status", "--json")
        self.selected = [label for label, _ in cases]
        if failures:
            raise Fail(f"{len(failures)}/{len(cases)} cases failed: {', '.join(failures)}")

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
        destination = self.evidence / "shells.json"
        destination.write_text(json.dumps({
            "outcome": "failed" if error else "passed", "failure": str(error) if error else None,
            "scenarios": self.results, "root": str(self.root), "environment": self.source,
            "records": self.records}, indent=2, default=str) + "\n", encoding="utf-8")
        destination.chmod(0o600)
        passed = sum(r["outcome"] == "passed" for r in self.results)
        print(f"shells: {passed}/{len(self.results)} passed; {'failed' if error else 'passed'}; "
              f"evidence: {destination}")
        if error:
            print(f"shells: {error}")
        return 1 if error else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--prefix", default=str(pathlib.Path.home() / ".marsh-dev"),
                        help="installed dev product (bin/marsh, libexec/marsh)")
    parser.add_argument("--sbx", default="sbx")
    parser.add_argument("--kit", default=None, help="fixture Kit: immutable OCI ref or native v3 source dir")
    parser.add_argument("--evidence", default=None)
    parser.add_argument("--only", action="append", choices=sorted(SCENARIOS), help="repeatable")
    parser.add_argument("--shell", action="append", choices=SHELLS, help="repeatable; default both")
    parser.add_argument("--fail-fast", action="store_true")
    parser.add_argument("--list", action="store_true", help="print scenario ids and exit")
    parser.add_argument("--source-tree", default=str(pathlib.Path(__file__).resolve().parents[2]))
    parser.add_argument("--source-revision", default=None)
    parser.add_argument("--build-receipt", type=pathlib.Path, default=None)
    arguments = parser.parse_args()
    if arguments.list:
        for scenario in ORDER:
            print(f"{scenario}\t{getattr(Shells, SCENARIOS[scenario]).__doc__.strip().splitlines()[0]}")
        return 0
    if not arguments.kit:
        parser.error("--kit is required")
    arguments.no_preflight = True
    arguments.live = False
    prefix = pathlib.Path(arguments.prefix).expanduser().resolve()
    arguments.marsh = str(prefix / "bin" / "marsh")
    arguments.guest_artifacts = prefix / "libexec" / "marsh"
    if not os.access(arguments.marsh, os.X_OK):
        print(f"shells: no installed product at {arguments.marsh} (run `make dev`)")
        return 1
    if arguments.source_revision is None:
        arguments.source_revision = subprocess.run(
            ["git", "-C", arguments.source_tree, "rev-parse", "HEAD"], capture_output=True, text=True,
            timeout=30, check=True).stdout.strip()
    if arguments.evidence is None:
        arguments.evidence = f"/private/tmp/marsh-dev-shells-{os.getuid()}"
    pathlib.Path(arguments.evidence).mkdir(mode=0o700, parents=True, exist_ok=True)
    harness = Shells(arguments)
    try:
        harness.run_all()
    except KeyboardInterrupt:
        return harness.finish(InterruptedError("shells acceptance interrupted"))
    except Exception as error:
        return harness.finish(error)
    return harness.finish(None)


if __name__ == "__main__":
    raise SystemExit(main())
