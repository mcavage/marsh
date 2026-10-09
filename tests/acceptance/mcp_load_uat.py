#!/usr/bin/env python3
"""Stock-SBX existing-publication load journey. No model or provider credentials.

An optional host-owned observed-build receipt is verified before effects. Only this harness's disposable
scope/sandbox is cleaned up. Controlled Linux tests do not qualify this route.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import shutil
import json
import pathlib
import shlex
import subprocess
import sys
import traceback

from mcp_gateway_uat import Scope, gateway_call, gateway_data, wait_until
from provenance import SharedBaseline, host_only_path, stock_vm_names, verify_candidate


def checked(result: subprocess.CompletedProcess) -> str:
    if result.returncode:
        raise AssertionError(result.stderr.decode(errors="replace"))
    return result.stdout.decode()


def inspect(scope: Scope, server: str) -> dict:
    result = subprocess.run([scope.sbx, "mcp", "inspect", server, "--json"],
                            cwd=scope.root, env=scope.environment,
                            capture_output=True, timeout=30, check=True, start_new_session=True)
    return json.loads(result.stdout)


def named_line(output: str, field: str) -> str:
    matches = [line.removeprefix(field + ": ") for line in output.splitlines()
               if line.startswith(field + ": ")]
    if len(matches) != 1 or not matches[0]:
        raise AssertionError(f"missing exact {field} in CLI output: {output}")
    return matches[0]


# Runs inside a real nonroot Kit job. Inspect metadata/mounts only, never token
# contents. The public CLI caller is copied from the receipt-verified guest build.
WORKER_BOUNDARY = r'''
import json, os, pathlib, stat, subprocess, sys, uuid
candidate, tool, target, control, relay_socket, relay_token = sys.argv[1:]
CAP = '/run/marsh/cap.sock'
assert relay_socket and relay_token
assert not os.path.lexists(relay_socket), 'live shell relay socket is visible in Kit'
assert not os.path.lexists(relay_token), 'live shell relay token file is visible in Kit'
assert not os.environ.get('MARSH_DAEMON_SOCKET')
assert not os.environ.get('MARSH_DAEMON_TOKEN')
roots = {'/run/marsh', '/run/user', '/tmp/marsh', '/home', os.environ['HOME']}
if os.environ.get('MARSH_SELECTED_HOME'): roots.add(os.environ['MARSH_SELECTED_HOME'])
violations = []
seen = set()
for root in roots:
 if not pathlib.Path(root).exists(): continue
 for directory, dirs, files in os.walk(root, followlinks=False, onerror=lambda e: (_ for _ in ()).throw(e)):
  for name in dirs + files:
   p = pathlib.Path(directory) / name
   info = p.lstat()
   if (info.st_dev, info.st_ino) in seen: continue
   seen.add((info.st_dev, info.st_ino))
   # The job's own scoped capability socket is its one daemon channel
   # (docs/design/processes.md); anything else is a leak.
   if str(p) == CAP:
    continue
   if stat.S_ISSOCK(info.st_mode) or name.endswith('.token') or name in ('token', 's', 't'):
    violations.append(str(p))
assert not violations, violations
assert not pathlib.Path(control).exists(), 'host control directory is visible in Kit'
# /run/marsh is the job's own capability directory: its tmpfs, cap.sock, and
# the in-job CLI (`/run/marsh/marsh`). Nothing else may be mounted there.
mounts = pathlib.Path('/proc/self/mountinfo').read_text().splitlines()
allowed = {'/run/marsh', '/run/marsh/marsh', CAP}
leaks = [line for line in mounts if control in line or (
    line.split()[4].startswith('/run/marsh/') and line.split()[4] not in allowed)]
assert not leaks, leaks
unix = [line for line in pathlib.Path('/proc/net/unix').read_text().splitlines() if not line.endswith(CAP)]
assert not any('marsh' in line or 'docker.sock' in line or 'containerd.sock' in line for line in unix), unix
# Guessing the *live* parent's route is not authority: neither its socket nor
# credential file is mounted. No token contents are read or copied by the test.
env = {**os.environ, 'MARSH_DAEMON_SOCKET': relay_socket, 'MARSH_DAEMON_TOKEN': relay_token}
result = subprocess.run([candidate, '--marsh-guest', '--marsh-session', str(uuid.uuid4()), '--noprofile', '--norc', '-c',
                         'mcp load ' + tool + ' --sandbox ' + target], env=env, capture_output=True, timeout=30, start_new_session=True)
assert result.returncode != 0, 'Kit acquired publication authority'
error = result.stderr.decode(errors='replace').lower()
assert 'daemon' in error or 'relay' in error or 'no such file' in error, error
print(json.dumps({'roots_checked': sorted(roots), 'mount_and_unix_tables_checked': True,
                  'live_parent_paths_absent': True, 'worker_cli_exit': result.returncode, 'worker_cli_error': error}))
'''


def kit_vms(scope: Scope, kit: str) -> list[str]:
    return [worker["vm_id"] for worker in scope.status()["workers"]
            if kit in worker.get("kits", []) and worker["warm"]]


def gateway_tools(scope: Scope, vm: str) -> list[str]:
    return gateway_call(scope.sbx, vm, "-", "", cwd=scope.root, env=scope.environment, list_only=True)["tools"]


def exercise_default_publication(scope: Scope, kit: str, report: dict) -> list[str]:
    """Untargeted `mcp publish`: never loaded into a running Kit VM (held job or
    idle), loaded into the next VM the daemon creates, removed by unpublish.
    A host-terminal unpublish (which bypasses the daemon's default record) must
    not let a later VM load the revoked tool. Returns the tools still loaded
    nowhere-but-revoked so the caller can check a later cold VM lacks them."""
    import os
    import signal
    import time
    checks = report["checks"]
    tool = "default_" + scope.tag
    stale = "hostrevoked_" + scope.tag
    # 1. A running Kit VM with a held job, created before any default exists.
    held = subprocess.Popen([str(scope.marsh), "-c", f"{kit} hold 900"], cwd=scope.project,
                            env=scope.environment, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, start_new_session=True)
    try:
        assert held.stdout is not None
        ready = held.stdout.readline()
        assert ready.strip() == b"READY", f"held job did not start: {ready!r} {held.poll()}"
        (old_vm,) = kit_vms(scope, kit)
        before = gateway_tools(scope, old_vm)
        scope.published.add(tool)
        published = checked(scope.command("-c", f"mcp publish {tool} -- 'cat | tr a-z A-Z'"))
        assert "every Kit VM created from now on" in published, published
        assert "Loaded into sandbox" not in published, published
        server = named_line(published, "Server")
        report["default_publication"] = {"tool": tool, "server": server, "running_vm": old_vm,
                                         "publish_output": published}
        # Give a wrong implementation (async load) time to show up.
        time.sleep(5)
        during = gateway_tools(scope, old_vm)
        assert tool not in during, f"default publication loaded into a running Kit VM: {during}"
        assert sorted(during) == sorted(before), (before, during)
        checks["default_not_loaded_into_running_vm_with_held_job"] = True
    finally:
        if held.poll() is None:
            os.killpg(held.pid, signal.SIGINT)
        try:
            held.communicate(timeout=60)
        except subprocess.TimeoutExpired:
            os.killpg(held.pid, signal.SIGKILL)
            held.communicate()
    report["default_publication"]["held_job_exit"] = held.returncode
    # 2. The same VM, now idle, runs a new job: still not loaded (create-only rule).
    checked(scope.command("-c", f"{kit} identity", timeout=300))
    assert kit_vms(scope, kit) == [old_vm], "warm Kit VM was replaced unexpectedly"
    idle = gateway_tools(scope, old_vm)
    assert tool not in idle, f"default publication loaded into an existing idle Kit VM: {idle}"
    checks["default_not_loaded_into_existing_vm_at_next_job"] = True
    # 3. Recreate the Kit VM: the next job's VM has the tool before that job.
    checked(scope.command("workers", "reset", kit))
    assert old_vm not in stock_vm_names(scope.sbx), "reset did not remove the running Kit VM"
    created = scope.command("-c", f"{kit} identity", timeout=1200)
    checked(created)
    assert f"[starting {kit} worker VM…]" in created.stderr.decode(errors="replace")
    (new_vm,) = kit_vms(scope, kit)
    assert new_vm != old_vm
    # Loaded before the VM was Ready: no waiting needed for discovery.
    probe = gateway_call(scope.sbx, new_vm, tool, "default\n", cwd=scope.root, env=scope.environment)
    gateway_data(probe, tool, b"DEFAULT\n")
    report["default_publication"]["recreated_vm"] = new_vm
    checks["default_loaded_into_recreated_vm_before_first_job"] = True
    # 4. Unpublish revokes it in that VM (stock registration removed).
    checked(scope.command("-c", f"mcp unpublish {tool}"))
    scope.published.discard(tool)
    assert scope.registration_absent(tool)
    # As for any revoked publication, a gateway may still list the tool, but
    # it can no longer run the pipeline.
    revoked = gateway_call(scope.sbx, new_vm, tool, "revoked\n", cwd=scope.root, env=scope.environment)
    response = revoked["call"]
    if tool in revoked["tools"]:
        assert response is None or "error" in response or response["result"].get("isError") is True, response
        data = (response or {}).get("result", {}).get("structuredContent", {}).get("data", {})
        assert data.get("stdout_base64") is None or base64.b64decode(data["stdout_base64"]) != b"REVOKED\n", response
    report["default_publication"]["after_unpublish"] = revoked
    checks["unpublish_revokes_default_in_loaded_vm"] = True
    # 5. A default revoked from a host terminal (no daemon, so the record stays)
    #    must not be loaded into a later VM.
    scope.published.add(stale)
    checked(scope.command("-c", f"mcp publish {stale} -- 'cat'"))
    host = subprocess.run([str(scope.marsh), "mcp", "unpublish", stale], cwd=scope.project, env=scope.environment,
                          stdin=subprocess.DEVNULL, capture_output=True, timeout=90, start_new_session=True)
    checked(host)
    scope.published.discard(stale)
    return [tool, stale]


def owned_view(scope) -> tuple[frozenset[str], dict[str, str]]:
    names = frozenset(scope.owned_stock_names())
    return names, {n: i for n, i in stock_vm_names(scope.sbx).items() if n in names}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    for flag in ("marsh", "guest-artifacts", "source-tree", "source-revision", "evidence"):
        parser.add_argument("--" + flag, required=True)
    parser.add_argument("--build-receipt", type=pathlib.Path,
                        help="optional observed-build receipt; absent records source identity only")
    parser.add_argument("--commands", type=pathlib.Path,
                        help="optional isolated commands.json overlay (e.g. staged Kit sources or OCI digests)")
    parser.add_argument("--sbx", default="sbx")
    parser.add_argument("--kit", default="shell", help="registered Kit command, prepared without invoking a model")
    args = parser.parse_args()
    # Must fail before creating a scope or invoking stock SBX on a mismatched build.
    verify_candidate(args, str(pathlib.Path(args.marsh).resolve(strict=True)))
    if sys.platform != "darwin":
        parser.error("stock MCP load UAT requires the macOS host")
    evidence = pathlib.Path(args.evidence).absolute()
    report_path = host_only_path(evidence / "mcp-load-uat.json", pathlib.Path(args.source_tree))
    scope = Scope(args)
    report = {"schema": "marsh.mcp-load-uat/v1", "outcome": "failed",
              "verified_build_receipt": scope.build_receipt,
              "stock_before": sorted(scope.stock_before), "checks": {}}
    peer = None
    try:
        scope.scope_started = True
        report["status_before"] = scope.status()
        revoked_defaults: list[str] = []
        if args.kit == "fixture":
            revoked_defaults = exercise_default_publication(scope, args.kit, report)
        else:
            report["checks"]["default_publication"] = "skipped: needs --kit fixture (held job)"
        scope.create_sandbox()
        tool = "load_" + scope.tag
        scope.published.add(tool)
        published = checked(scope.command("-c",
            f"mcp publish {tool} --sandbox {shlex.quote(scope.sandbox)} -- 'cat | tr a-z A-Z'"))
        server = named_line(published, "Server")
        original = inspect(scope, server)
        command = original["command"]
        generation = command[command.index("--expected-generation") + 1]
        report["server"] = server
        report["generation"] = generation
        report["publishing_sandbox"] = scope.sandbox
        first = scope.gateway_ready(tool, "first\n")
        gateway_data(first, tool, b"FIRST\n")
        # Keep an actual original exporter process/client connected across load.
        sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "mcp"))
        from jsonrpc_peer import JsonRpcPeer
        peer = JsonRpcPeer(command, scope.environment)
        peer.initialize()
        assert peer.request("tools/list")["result"]["tools"][0]["name"] == tool
        checked(scope.command("workers", "reset", args.kit))
        cold_start = scope.status()
        assert not any(worker["warm"] for worker in cold_start["workers"]), cold_start
        report["status_before_cold_load"] = cold_start
        load_result = scope.command("-c",
            f"mcp load {tool} --kit {shlex.quote(args.kit)}", timeout=1200)
        loaded = checked(load_result)
        boot = f"[starting {args.kit} worker VM…]"
        assert boot in load_result.stderr.decode(errors="replace"), "cold load lost real boot progress"
        report["checks"]["cold_load_reports_boot_progress"] = True
        report["status_after_load"] = scope.status()
        target = named_line(loaded, "Sandbox")
        assert named_line(loaded, "Server") == server
        assert target != scope.sandbox, "load did not target a different Kit VM"
        current = inspect(scope, server)
        assert current["command"] == original["command"], "load changed generation or declaration binding"
        def ready():
            probe = gateway_call(scope.sbx, target, tool, "second\n", cwd=scope.root, env=scope.environment)
            return probe if tool in probe["tools"] else None
        cold_probe = wait_until(ready, "new Kit Gateway discovery", 30)
        gateway_data(cold_probe, tool, b"SECOND\n")
        if revoked_defaults:
            # This Kit VM was created after both defaults were revoked (one by
            # the shell, one from a host terminal that left the record behind).
            leaked = [name for name in revoked_defaults if name in cold_probe["tools"]]
            assert not leaked, f"revoked default publication loaded into a new Kit VM: {leaked}"
            report["checks"]["revoked_defaults_not_loaded_into_new_vm"] = True
        result = peer.request("tools/call", {"name": tool, "arguments": {"input": "parent\n"}}, timeout=120)["result"]
        assert result.get("isError") is not True, result
        import base64
        assert base64.b64decode(result["structuredContent"]["data"]["stdout_base64"]) == b"PARENT\n"
        gateway_data(scope.gateway(tool, "original\n"), tool, b"ORIGINAL\n")
        report["checks"]["same_generation_both_gateways_original_client"] = True
        report["target"] = target
        # Inspect the actual fresh Kit namespace and drive the real guest CLI,
        # not just two empty environment variables or a host/master-token peer.
        caller = scope.project / "publication-worker-caller"
        source = scope.guest_artifacts / "marsh-linux-arm64"
        shutil.copyfile(source, caller)
        caller.chmod(0o700)
        report["worker_caller_sha256"] = hashlib.sha256(caller.read_bytes()).hexdigest()
        assert report["worker_caller_sha256"] == hashlib.sha256(source.read_bytes()).hexdigest()
        inner = "python3 -c " + shlex.quote(WORKER_BOUNDARY) + " " + " ".join(map(shlex.quote,
            [str(caller), tool, target, str(scope.control_root)])) + ' "$1" "$2"'
        worker = checked(scope.command("-c",
            'test -S "$MARSH_DAEMON_SOCKET" && test -f "$MARSH_DAEMON_TOKEN" && shell -c ' +
            shlex.quote(inner) + ' boundary "$MARSH_DAEMON_SOCKET" "$MARSH_DAEMON_TOKEN"'))
        report["worker_boundary"] = json.loads(worker)
        assert inspect(scope, server)["command"] == original["command"]
        report["checks"]["actual_kit_namespace_and_public_cli_deny_publication"] = True
        wrong_project = scope.root / "other-project"
        wrong_project.mkdir(mode=0o700)
        wrong = subprocess.run([str(scope.marsh), "mcp", "load", tool, "--sandbox", scope.sandbox],
                               cwd=wrong_project, env=scope.environment, capture_output=True, timeout=60, start_new_session=True)
        assert wrong.returncode != 0
        assert inspect(scope, server)["command"] == original["command"]
        report["checks"]["wrong_project_denied"] = True
        checked(scope.command("-c", f"mcp unpublish {tool}"))
        scope.published.remove(tool)
        # This Kit must be genuinely cold: warm preparation would leave the
        # VM inventory unchanged and make a no-preparation assertion vacuous.
        checked(scope.command("workers", "reset", args.kit))
        cold = scope.status()
        assert not any(worker["vm_id"] == target and worker["warm"] for worker in cold["workers"]), cold
        before = stock_vm_names(scope.sbx)
        assert target not in before and target not in before.values(), "reset did not remove exact target VM"
        owned_before = owned_view(scope)
        report["cold_denial_workers_before"] = cold["workers"]
        denied = scope.command("-c", f"mcp load {tool} --kit {shlex.quote(args.kit)}")
        assert denied.returncode != 0, "revoked publication was loaded"
        assert scope.registration_absent(tool)
        # Alone, the whole stock inventory is unchanged. Beside concurrent
        # suites (their VMs come and go) the witness is this scope's own view:
        # its ownership map (a daemon records a VM before `sbx create`) and the
        # stock VMs those names resolve to are both unchanged.
        assert owned_view(scope) == owned_before, "revoked load changed this scope's stock VMs"
        if not isinstance(scope.stock_before, SharedBaseline):
            assert stock_vm_names(scope.sbx) == before, "revoked load changed stock VM inventory"
        after_denial = scope.status()
        assert after_denial["workers"] == cold["workers"], "revoked load prepared a cold worker"
        report["cold_denial_workers_after"] = after_denial["workers"]
        assert peer.request("tools/call", {"name": tool, "arguments": {"input": "revoked"}})["result"]["isError"]
        report["checks"]["unpublished_load_denied_cold_kit_no_worker_or_vm_creation"] = True
        report["status_after"] = scope.status()
        report["outcome"] = "passed"
    except Exception:
        report["failure"] = traceback.format_exc()
    finally:
        if peer is not None:
            peer.close()
        ownership_errors = []
        if scope.scope_started:
            try:
                scope.status()  # retain exact newly prepared VM ownership even on failure
            except Exception as error:
                ownership_errors.append(f"could not refresh owned resources: {error}")
        report["cleanup_errors"] = ownership_errors + scope.cleanup()
        report["stock_after"] = sorted(scope.stock_after) if scope.stock_after is not None else None
        if report["cleanup_errors"]:
            report["outcome"] = "failed"
        report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        report_path.chmod(0o600)
    print(json.dumps({"outcome": report["outcome"], "evidence": str(report_path)}))
    return 0 if report["outcome"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
