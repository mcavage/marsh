#!/usr/bin/env python3
"""Fast fail real-product acceptance smoke gate (`make dev-smoke`, regress `smoke`)."""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import pty
import re
import select
import signal
import struct
import subprocess
import time

from harness import (APT_UPDATE_READY, CONTAINER, ProductScope, kit_profile_matches,
                     verify_container_deleted)
from provenance import candidate_arguments, stock_baseline


class Smoke(ProductScope):
    def run_all(self) -> None:
        self.stock_before = stock_baseline(self.sbx)
        self.source["stock_before"] = self.stock_before
        status = self.document("status", "--json")
        if status.get("schema") != "marsh.status/v1":
            raise AssertionError("status handshake has the wrong schema")

        cold = [
            subprocess.Popen(
                [self.marsh, "--load", "fixture", "-c", "true"],
                cwd=self.project,
                env=self.environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            for _ in range(2)
        ]
        cold_stderr = bytearray()
        for process in cold:
            _, stderr = process.communicate(timeout=300)
            cold_stderr.extend(stderr)
            if process.returncode != 0:
                raise RuntimeError(
                    f"parallel cold --load failed ({process.returncode}): "
                    f"{stderr.decode(errors='replace')}"
                )

        warm_started = time.monotonic()
        second = self.run([self.marsh, "--load", "fixture", "-c", "true"], timeout=120)
        warm_ms = round((time.monotonic() - warm_started) * 1000)
        if b"[starting fixture worker VM" in second.stderr:
            raise AssertionError("warm preflight printed a false fixture cold-boot notice")
        if warm_ms > 10_000:
            raise AssertionError(f"ready worker reuse took {warm_ms} ms")

        warm = self.document("status", "--json")
        workers = [
            worker
            for worker in warm["workers"]
            if kit_profile_matches(worker.get("kit_profile"), self.kit)
        ]
        if len(workers) != 1 or not workers[0]["warm"] or workers[0]["health"] != "ready":
            raise AssertionError(f"fixture prewarm did not expose one ready worker: {workers!r}")
        vm = workers[0]["vm_id"]

        identity = self.shell(
            'printf "cwd=%s\\nhome=%s\\nuser=%s\\n" "$PWD" "$HOME" "$USER"'
        ).stdout.decode()
        expected_home = f"/Users/{os.environ['USER']}"
        if f"cwd={self.project}\n" not in identity or f"home={expected_home}\n" not in identity:
            raise AssertionError(f"job paths are not natural: {identity!r}")

        package = self.shell(
            f"sudo -n true && {APT_UPDATE_READY} && "
            "sudo -n env DEBIAN_FRONTEND=noninteractive "
            "apt-get install -y -qq --no-install-recommends ed >/dev/null && "
            "test -x /usr/bin/ed && "
            "test \"$(stat -c %U:%G:%a /etc/sudoers.d/marsh)\" = root:root:440 && "
            "! grep -Rqs 'http://deb.debian.org/' "
            "/etc/apt/sources.list /etc/apt/sources.list.d"
        )
        if package.returncode != 0:
            raise AssertionError(
                "project shell could not install a Debian package through confined sudo: "
                f"{package.stderr.decode(errors='replace')}"
            )

        self.shell("printf natural > natural-write.txt")
        if (self.project / "natural-write.txt").read_text(encoding="utf-8") != "natural":
            raise AssertionError("job write did not reach the exact host project path")
        # ENOEXEC (no shebang) runs under marsh like Bash, not the platform
        # /bin/sh: `[[ ]]` and arrays are not POSIX sh.
        plain = self.shell(
            "printf '%s\\n' 'a=(x y); [[ ${a[1]} == y ]] && printf \"plain:%s\" \"$1\"' > no-shebang && "
            "chmod +x no-shebang && ./no-shebang ok; r=$?; rm -f no-shebang; exit $r",
            check=False,
        )
        if plain.returncode != 0 or plain.stdout != b"plain:ok":
            raise AssertionError(f"no-shebang script did not run under marsh: {plain!r}")
        self.shell('printf selected-home > "$HOME/.marsh-uat-home"')
        persisted = self.shell('cat "$HOME/.marsh-uat-home"')
        if persisted.stdout != b"selected-home":
            raise AssertionError("selected HOME did not persist across fresh job containers")
        if (self.guest_home / ".marsh-uat-home").read_bytes() != b"selected-home":
            raise AssertionError("selected HOME did not write to MARSH_HOME/home")
        if (self.home / ".marsh-uat-home").exists():
            raise AssertionError("selected HOME wrote into the host control scope")
        ephemeral = self.run(
            [
                self.marsh,
                "--ephemeral-home",
                "-c",
                'test ! -e "$HOME/.marsh-uat-home" && printf ephemeral > "$HOME/.ephemeral-only"',
            ]
        )
        if ephemeral.returncode != 0 or (self.guest_home / ".ephemeral-only").exists():
            raise AssertionError("--ephemeral-home read from or wrote to the persistent selected home")

        stream = self.shell("cat; printf err >&2; exit 23", stdin=b"stdin\x00bytes\xff", check=False)
        if stream.returncode != 23 or stream.stdout != b"stdin\x00bytes\xff" or stream.stderr != b"err":
            raise AssertionError("stdin/stdout/stderr/exit status were not byte faithful")
        pipeline = self.shell("printf pipeline | tr a-z A-Z")
        if pipeline.stdout != b"PIPELINE":
            raise AssertionError("registered command failed inside a shell pipeline")
        composition = self.shell(
            "printf input | fanout { "
            "first: cat | tr a-z A-Z; "
            "second: sed s/input/branch/ "
            "} | collect --timing"
        )
        if b"== first (complete) ==\nINPUT" not in composition.stdout or b"== second (complete) ==\nbranch" not in composition.stdout:
            raise AssertionError("fanout pipelines were not collected in declaration order")
        total = re.search(rb"(?m)^  total\s+([0-9]+) ms$", composition.stdout)
        if total is None or int(total.group(1)) >= 1_000:
            raise AssertionError(
                f"warm local fanout orchestration exceeded one second: {composition.stdout!r}"
            )
        background = self.shell(
            "sleep 1 & pid=$!; jobs -p; wait \"$pid\"; "
            "printf background > bg.out; cat bg.out"
        )
        if not re.search(rb"(?m)^[1-9][0-9]*$", background.stdout) or not background.stdout.endswith(b"background"):
            raise AssertionError("background PID/jobs/wait/output behavior failed")

        wait_next = self.shell(
            "sh -c 'sleep .05; exit 23' & first=$!; "
            "sh -c 'sleep .2; exit 17' & second=$!; "
            "wait -n -p finished \"$first\" \"$second\"; status=$?; "
            "test \"$status\" = 23 && test \"$finished\" = \"$first\" && "
            "wait \"$second\"; test \"$?\" = 17"
        )
        if wait_next.returncode != 0:
            raise AssertionError("wait -n -p lost the first child status or identity")

        wait_stopped = self.shell(
            "sh -c 'kill -STOP $$; exit 23' & pid=$!; "
            "sh -c \"sleep .1; kill -CONT $pid\" & "
            "wait -n -f -p finished \"$pid\"; status=$?; "
            "test \"$status\" = 23 && test \"$finished\" = \"$pid\""
        )
        if wait_stopped.returncode != 0:
            raise AssertionError("wait -n -f did not wait for the stopped child to terminate")

        signal_ready = self.project / ".signal-ready"
        signaled = subprocess.Popen(
            [
                self.marsh,
                "-c",
                "trap 'exit 130' INT; sh -c 'touch .signal-ready; exec sleep 300'",
            ],
            cwd=self.project,
            env=self.environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        try:
            self.wait_for([signal_ready], [signaled])
            time.sleep(0.2)
            os.killpg(signaled.pid, signal.SIGINT)
            try:
                _, signal_stderr = signaled.communicate(timeout=15)
            except subprocess.TimeoutExpired as error:
                os.killpg(signaled.pid, signal.SIGKILL)
                _, final_stderr = signaled.communicate(timeout=5)
                partial = error.stderr or b""
                raise AssertionError(
                    "Ctrl-C timed out: "
                    + (partial + final_stderr).decode(errors="replace")
                ) from error
            if signaled.returncode != 130:
                raise AssertionError(
                    f"Ctrl-C returned {signaled.returncode}, expected 130: "
                    f"{signal_stderr.decode(errors='replace')}"
                )
        finally:
            if signaled.poll() is None:
                os.killpg(signaled.pid, signal.SIGKILL)
                signaled.wait(timeout=5)

        term_ready = self.project / ".term-ready"
        terminated = subprocess.Popen(
            [
                self.marsh,
                "-c",
                "trap 'printf term-trap; exit 143' TERM; "
                "sh -c 'touch .term-ready; exec sleep 300'",
            ],
            cwd=self.project,
            env=self.environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        try:
            self.wait_for([term_ready], [terminated])
            time.sleep(0.2)
            os.killpg(terminated.pid, signal.SIGTERM)
            try:
                term_stdout, term_stderr = terminated.communicate(timeout=15)
            except subprocess.TimeoutExpired as error:
                os.killpg(terminated.pid, signal.SIGKILL)
                terminated.communicate(timeout=5)
                raise AssertionError("SIGTERM cleanup trap timed out") from error
            if terminated.returncode != 143 or term_stdout != b"term-trap":
                raise AssertionError(
                    f"SIGTERM trap returned {terminated.returncode}, "
                    f"stdout={term_stdout!r}, stderr={term_stderr!r}"
                )
        finally:
            if terminated.poll() is None:
                os.killpg(terminated.pid, signal.SIGKILL)
                terminated.wait(timeout=5)

        idle_ready = self.project / ".idle-term-ready"
        idle = subprocess.Popen(
            [self.marsh, "-s"],
            cwd=self.project,
            env=self.environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        try:
            assert idle.stdin is not None
            idle.stdin.write(
                b"trap 'printf idle-term; exit 143' TERM\n"
                b"touch .idle-term-ready\n"
            )
            idle.stdin.flush()
            self.wait_for([idle_ready], [idle])
            time.sleep(0.2)
            os.killpg(idle.pid, signal.SIGTERM)
            deadline = time.monotonic() + 15
            while idle.poll() is None and time.monotonic() < deadline:
                time.sleep(0.05)
            if idle.poll() is None:
                os.killpg(idle.pid, signal.SIGKILL)
                idle.communicate(timeout=5)
                raise AssertionError("idle SIGTERM cleanup trap timed out")
            try:
                idle_stdout, idle_stderr = idle.communicate(timeout=5)
            except subprocess.TimeoutExpired as error:
                os.killpg(idle.pid, signal.SIGKILL)
                idle.communicate(timeout=5)
                raise AssertionError("idle SIGTERM output read timed out") from error
            if idle.returncode != 143 or idle_stdout != b"idle-term":
                raise AssertionError(
                    f"idle SIGTERM trap returned {idle.returncode}, "
                    f"stdout={idle_stdout!r}, stderr={idle_stderr!r}"
                )
        finally:
            if idle.poll() is None:
                os.killpg(idle.pid, signal.SIGKILL)
                idle.wait(timeout=5)

        self.pty_resize_color()

        gates = [self.project / ".ready-a", self.project / ".ready-b"]
        release = self.project / ".release"
        scripts = [
            f"touch {ready}; while test ! -e {release}; do sleep 0.05; done; printf shell"
            for ready in gates
        ]
        processes = [
            subprocess.Popen(
                [self.marsh, "-c", script],
                cwd=self.project,
                env=self.environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                start_new_session=True,
            )
            for script in scripts
        ]
        try:
            self.wait_for(gates, processes)
            active = self.document("status", "--json")
            attached = [shell for shell in active["shells"] if shell["state"] == "attached"]
            if len(attached) < 2 or len({shell["daemon_id"] for shell in attached}) != 1:
                raise AssertionError("two parallel shells did not share one daemon")
            release.write_text("go", encoding="utf-8")
            for process in processes:
                stdout, stderr = process.communicate(timeout=30)
                if process.returncode != 0:
                    raise RuntimeError(f"parallel shell failed: {stderr.decode(errors='replace')}")
        finally:
            release.touch()
            for process in processes:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=5)

        for _ in range(2):
            identity = self.shell("fixture identity")
            if not identity.stdout.strip():
                raise AssertionError("registered fixture identity command produced no output")

        receipts = [receipt for receipt in self.receipts() if receipt["state"] == "finished"]
        if len(receipts) < 2:
            raise AssertionError("parallel runs are absent from durable jobs")
        recent = receipts[:2]
        containers = [receipt["container_id"] for receipt in recent]
        if (
            len({receipt["vm_id"] for receipt in recent}) != 1
            or len(set(containers)) != 2
            or not all(CONTAINER.fullmatch(container or "") for container in containers)
            or any(receipt["cleanup"] != "verified" for receipt in recent)
        ):
            raise AssertionError(f"fresh-container/reused-VM/cleanup evidence failed: {recent!r}")
        for container in containers:
            verify_container_deleted(
                lambda argv: self.run(argv, timeout=15, check=False),
                [self.sbx, "exec", "-u", "root", vm],
                container,
            )

        results = self.document("results", "--json")
        if results.get("schema") != "marsh.jobs/v1" or len(results.get("jobs", [])) < 2:
            raise AssertionError(f"durable result summary is incomplete: {results!r}")
        cursor = str(results["jobs"][0]["cursor"])
        shown = self.document("results", "show", cursor, "--json")
        if shown.get("cursor") != int(cursor) or shown.get("cleanup") != "verified":
            raise AssertionError(f"durable result lookup is incomplete: {shown!r}")

        # The first run may boot both worker and shell VMs; retain this useful
        # diagnostic without prescribing which one was cold on a reused host.
        self.records.append(
            {
                "cold_boot_stderr": cold_stderr.decode(errors="replace"),
                "warm_reuse_ms": warm_ms,
            }
        )

    def pty_resize_color(self) -> None:
        master, slave = pty.openpty()
        fcntl.ioctl(slave, 0x80087467, struct.pack("HHHH", 24, 80, 0, 0))
        script = (
            "printf '\\033[32mCOLOR\\033[0m\\n'; "
            "IFS= read -r barrier; "
            "while :; do "
            "size=$(stty size); "
            "if test \"$size\" = '40 120'; then printf 'SIZE=%s\\n' \"$size\"; exit 47; fi; "
            "sleep 0.05; "
            "done"
        )
        process = subprocess.Popen(
            [self.marsh, "-c", script],
            cwd=self.project,
            env=self.environment,
            stdin=slave,
            stdout=slave,
            stderr=slave,
            start_new_session=True,
        )
        os.close(slave)
        output = bytearray()
        try:
            deadline = time.monotonic() + 20
            while b"\x1b[32mCOLOR\x1b[0m" not in output and time.monotonic() < deadline:
                readable, _, _ = select.select([master], [], [], 0.1)
                if readable:
                    chunk = os.read(master, 4096)
                    if not chunk:
                        break
                    output.extend(chunk)
            if b"\x1b[32mCOLOR\x1b[0m" not in output:
                raise AssertionError(f"PTY lost color bytes: {bytes(output)!r}")
            fcntl.ioctl(master, 0x80087467, struct.pack("HHHH", 40, 120, 0, 0))
            os.killpg(process.pid, signal.SIGWINCH)
            os.write(master, b"continue\n")
            output_deadline = time.monotonic() + 15
            while b"40 120" not in output and time.monotonic() < output_deadline:
                readable, _, _ = select.select([master], [], [], 0.1)
                if not readable:
                    continue
                try:
                    chunk = os.read(master, 4096)
                    if not chunk:
                        break
                    output.extend(chunk)
                except OSError:
                    break
            status = process.wait(timeout=5)
            if status != 47 or b"40 120" not in output:
                raise AssertionError(
                    f"PTY resize/exit status failed: status={status}, output={bytes(output)!r}"
                )
        finally:
            os.close(master)
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)

    def finish(self, error: Exception | None) -> int:
        cleanup_errors = self.scope_cleanup_errors()
        if cleanup_errors:
            cleanup = RuntimeError("; ".join(cleanup_errors))
            error = cleanup if error is None else RuntimeError(f"{error}; cleanup: {cleanup}")
        destination = self.evidence / "smoke.json"
        destination.write_text(
            json.dumps(
                {
                    "outcome": "failed" if error else "passed",
                    "failure": str(error) if error else None,
                    "root": str(self.root),
                    "environment": self.source,
                    "records": self.records,
                },
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
        destination.chmod(0o600)
        print(f"acceptance smoke: {'failed' if error else 'passed'}; evidence: {destination}")
        if error:
            print(f"acceptance smoke: {error}")
        return 1 if error else 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Fast fail real-product acceptance smoke gate.")
    candidate_arguments(parser)
    parser.add_argument("--marsh", required=True)
    parser.add_argument("--sbx", default="sbx")
    parser.add_argument(
        "--kit", required=True, help="native v3 source directory or immutable OCI digest"
    )
    parser.add_argument("--evidence", required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--source-tree", required=True)
    smoke = Smoke(parser.parse_args(argv))
    try:
        smoke.run_all()
    except KeyboardInterrupt:
        return smoke.finish(InterruptedError("acceptance smoke interrupted"))
    except Exception as error:  # preserve evidence from the first real failure
        return smoke.finish(error)
    return smoke.finish(None)


if __name__ == "__main__":
    raise SystemExit(main())
