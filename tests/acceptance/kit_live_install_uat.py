#!/usr/bin/env python3
"""Install a published Kit after shell attach and invoke it in that same shell."""

import argparse
import json
import os
from pathlib import Path
import select
import shutil
import subprocess
import sys
import time
import traceback

from provenance import (host_only_path, stock_cleanup_errors, stock_vm_names,
                        verify_candidate)
from run import (CONTAINER, IMAGE, disposable_root, scoped_control_home,
                 verify_container_deleted)


SHELL_KIT = "docker.io/docker/sbx-kit-shell@sha256:367c9a4b3fd550f777e1d57cc53550afb4a1385055d79111acf6f996272ff6de"
ALTERNATE_KIT = "docker.io/docker/sbx-kit-pi@sha256:7514ea9f2ce3613ea7e07cddf2aee3d8b598ef052ad68623603b20a332326a09"


def read_until(process: subprocess.Popen[bytes], marker: bytes, timeout: int) -> bytes:
    output = bytearray()
    deadline = time.monotonic() + timeout
    while marker not in output:
        if time.monotonic() >= deadline or process.poll() is not None:
            raise AssertionError(f"shell stopped before {marker!r}: {output[-4000:]!r}")
        ready, _, _ = select.select([process.stdout], [], [], min(1, deadline - time.monotonic()))
        if ready:
            output.extend(os.read(process.stdout.fileno(), 65536))
    return bytes(output)


def send(process: subprocess.Popen[bytes], command: str, marker: str, timeout: int) -> bytes:
    assert process.stdin is not None
    process.stdin.write((command + "\n").encode())
    process.stdin.flush()
    return read_until(process, marker.encode(), timeout)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--marsh", type=Path, required=True)
    parser.add_argument("--guest-artifacts", type=Path, required=True)
    parser.add_argument("--build-receipt", type=Path, required=True)
    parser.add_argument("--source-tree", type=Path, required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--sbx", type=Path, required=True)
    parser.add_argument("--initial-kit", default=SHELL_KIT)
    parser.add_argument("--alternate-kit", default=ALTERNATE_KIT)
    parser.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    marsh = args.marsh.resolve(strict=True)
    guest, receipt = verify_candidate(args, str(marsh))
    args.evidence = host_only_path(args.evidence, args.source_tree)
    if args.evidence.exists():
        parser.error("choose a fresh live Kit evidence file")
    if not all(IMAGE.fullmatch(value) for value in (args.initial_kit, args.alternate_kit)):
        parser.error("both Kits must use immutable OCI references")
    sbx = args.sbx.resolve(strict=True)
    stock_before = stock_vm_names(sbx)

    root = disposable_root("marsh-kit-install-")
    project = root / "project"
    home = root / "scope"
    control = root / "control"
    for directory in (project, home, control):
        directory.mkdir(mode=0o700)
    (home / "home").mkdir(mode=0o700)
    registry = scoped_control_home(control, home) / "commands.json"
    registry.write_text(json.dumps({"shell": args.initial_kit}) + "\n")
    registry.chmod(0o600)
    environment = {**os.environ,
                   "MARSH_HOME": str(home), "MARSH_CONTROL_HOME": str(control),
                   "MARSH_GUEST_ARTIFACTS": str(guest), "MARSH_SBX": str(sbx),
                   "MARSH_PLACE": "local"}
    process = subprocess.Popen(
        [str(marsh), "--no-config", "--noprofile", "--norc", "-s"],
        cwd=project, env=environment, stdin=subprocess.PIPE,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
    )
    passed = False
    output = bytearray()
    report = {"schema": "marsh.live-kit-install-uat/v1", "outcome": "failed",
              "verified_build_receipt": receipt, "stock_before": stock_before,
              "initial_kit": args.initial_kit, "alternate_kit": args.alternate_kit,
              "scope": str(home), "stock_commands": []}
    def stock(arguments):
        result = subprocess.run([str(sbx), *arguments], capture_output=True, timeout=45)
        report["stock_commands"].append({"argv": [str(sbx), *arguments], "status": result.returncode,
                                          "stdout": result.stdout.decode(errors="replace"),
                                          "stderr": result.stderr.decode(errors="replace")})
        return result
    try:
        output.extend(send(process, "printf 'ATTACHED\\n'", "ATTACHED", 300))
        output.extend(send(process,
                           f"marsh kit install alternate --from {args.alternate_kit}; printf 'INSTALL_EXIT:%s\\n' \"$?\"",
                           "INSTALL_EXIT:", 900))
        assert b"INSTALL_EXIT:0" in output, output[-4000:]
        assert f"installed alternate from {args.alternate_kit}".encode() in output, output[-4000:]
        output.extend(send(process,
                           "command -v alternate; alternate --version; printf 'INVOKE_EXIT:%s\\n' \"$?\"",
                           "INVOKE_EXIT:", 600))
        assert b"INVOKE_EXIT:0" in output, output[-4000:]
        assert b"/marsh-commands-" in output, output[-4000:]
        status = subprocess.run([str(marsh), "status", "--json"], cwd=project,
                                env=environment, capture_output=True, timeout=30)
        assert status.returncode == 0, (
            f"status failed: {status.stderr.decode(errors='replace')} "
            f"stdout={status.stdout.decode(errors='replace')}"
        )
        workers = json.loads(status.stdout)["workers"]
        assert any(worker.get("kit_profile") == args.alternate_kit for worker in workers), workers
        persisted = json.loads(registry.read_text())
        assert persisted["alternate"] == args.alternate_kit, persisted
        listed = subprocess.run([str(marsh), "jobs", "--json"], cwd=project,
                                env=environment, capture_output=True, check=True, timeout=30)
        jobs = [item for item in json.loads(listed.stdout)["jobs"] if item["command"] == "alternate"]
        assert len(jobs) == 1, jobs
        shown = subprocess.run([str(marsh), "jobs", "show", jobs[0]["job_id"], "--json"],
                               cwd=project, env=environment, capture_output=True, check=True, timeout=30)
        job = json.loads(shown.stdout)
        assert job["cleanup"] == "verified" and CONTAINER.fullmatch(job["container_id"]), job
        verify_container_deleted(stock, ["exec", job["vm_id"]], job["container_id"])
        report["job_receipt"] = job
        passed = True
    except Exception as error:
        report["failure"] = "".join(traceback.format_exception(type(error), error, error.__traceback__))
    finally:
        cleanup_errors = []
        if process.poll() is None:
            try:
                assert process.stdin is not None
                process.stdin.write(b"exit\n")
                process.stdin.flush()
                process.communicate(timeout=90)
            except (BrokenPipeError, subprocess.TimeoutExpired, ValueError):
                process.kill()
                process.wait()
                cleanup_errors.append("attached shell failed to exit normally")
        report["shell_exit_status"] = process.returncode
        if process.returncode != 0:
            cleanup_errors.append(f"attached shell exited {process.returncode}")
        cleanup_complete = False
        try:
            stopped = subprocess.run([str(marsh), "stop", "--json"], cwd=project,
                                     env=environment, capture_output=True, timeout=600)
            report["scope_stop"] = {"status": stopped.returncode,
                                    "stdout": stopped.stdout.decode(errors="replace"),
                                    "stderr": stopped.stderr.decode(errors="replace")}
            cleanup_complete = stopped.returncode == 0 and json.loads(stopped.stdout).get("cleanup_complete") is True
            if not cleanup_complete:
                cleanup_errors.append("public scope cleanup was not complete")
        except Exception as error:
            cleanup_errors.append(f"scope cleanup failed: {error}")
        try:
            stock_after = stock_vm_names(sbx)
            report["stock_after"] = stock_after
            cleanup_errors.extend(stock_cleanup_errors(stock_before, stock_after))
            verify_candidate(args, str(marsh))
        except Exception as error:
            cleanup_errors.append(f"final independent verification failed: {error}")
        cleanup_complete = cleanup_complete and not cleanup_errors
        report.update(passed=passed and cleanup_complete, cleanup_complete=cleanup_complete,
                      cleanup_errors=cleanup_errors, output_tail=output[-4000:].decode(errors="replace"),
                      outcome="passed" if passed and cleanup_complete else "failed")
        if cleanup_complete:
            shutil.rmtree(root)
        else:
            print(f"Kit UAT cleanup incomplete; isolated scope retained at {root}", file=sys.stderr)
        args.evidence.write_text(json.dumps(report, indent=2) + "\n")
        args.evidence.chmod(0o600)
    assert passed and cleanup_complete, args.evidence.read_text()
    print("PASS: alternate Kit installed after attach, invoked in same shell, persisted, and cleaned")


if __name__ == "__main__":
    main()
