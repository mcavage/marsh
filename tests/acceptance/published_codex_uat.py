#!/usr/bin/env python3
"""Stock-SBX publication and real Codex CLI registration, using a private Codex home."""

import argparse
import base64
import json
import pathlib
import subprocess
import sys
import time
import traceback

from mcp_gateway_uat import Scope, gateway_call, gateway_data, sha256, wait_until
from run import source_identity
from provenance import host_only_path, stock_vm_inventory, remove_owned_stock_vm

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "mcp"))
from jsonrpc_peer import JsonRpcPeer  # noqa: E402


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-receipt", type=pathlib.Path, required=True)
    parser.add_argument("--marsh", required=True)
    parser.add_argument("--sbx", required=True)
    parser.add_argument("--guest-artifacts", required=True)
    parser.add_argument("--codex", required=True)
    parser.add_argument("--evidence", type=pathlib.Path, required=True)
    parser.add_argument("--source-tree", type=pathlib.Path, required=True)
    parser.add_argument("--source-revision", required=True)
    args = parser.parse_args()
    source_tree = args.source_tree.resolve(strict=True)
    identity = source_identity(source_tree, args.source_revision)
    args.evidence = args.evidence.resolve()
    host_only_path(args.evidence / "published-codex-uat.json", source_tree).unlink(missing_ok=True)
    scope = Scope(args)
    codex = pathlib.Path(args.codex).resolve(strict=True)
    codex_home = scope.root / "codex-home"
    codex_home.mkdir(mode=0o700)
    scope.environment["CODEX_HOME"] = str(codex_home)
    tool = scope.tools[0]
    peer = None
    fresh_peer = None
    fresh_sandbox = scope.sandbox + "-fresh"
    fresh_sandbox_created = False
    fresh_sandbox_id = None
    guest_root = pathlib.Path(args.guest_artifacts).resolve(strict=True)
    guest_hashes = {name: sha256((guest_root / name).resolve(strict=True))
                    for name in ("marsh-linux-arm64", "marsh-worker-linux-arm64", "marsh-relay-linux-arm64")}
    report = {"schema": "marsh.published-codex-uat/v1", "outcome": "failed",
              "project": str(scope.project), "selected_home": str(scope.home),
              "codex_home": str(codex_home), "sandbox": scope.sandbox,
              "source_identity": identity, "verified_build_receipt": scope.build_receipt,
              "stock_before": scope.stock_before,
              "binary_sha256": {name: sha256(path) for name, path in
                                (("marsh", scope.marsh), ("marshd", scope.marshd),
                                 ("marsh-mcp", scope.mcp), ("codex", codex))},
              "guest_artifact_sha256": guest_hashes,
              "harness_sha256": sha256(pathlib.Path(__file__))}

    def cli(*words: str, expected: int = 0) -> subprocess.CompletedProcess[bytes]:
        result = subprocess.run([str(codex), *words], cwd=scope.project,
                                env=scope.environment, capture_output=True, timeout=30)
        assert (result.returncode == 0) == (expected == 0), (words, result.stderr)
        return result

    try:
        version = subprocess.run([scope.sbx, "version"], cwd=scope.project,
                                 env=scope.environment, capture_output=True, timeout=20)
        assert version.returncode == 0, version.stderr
        report["stock_sbx_version"] = version.stdout.decode(errors="replace").strip()
        scope.scope_started = True
        scope.status()
        scope.create_sandbox()
        scope.publish_pipeline(tool, "cat | tr a-z A-Z")
        install = scope.command("mcp", "install-published", "codex", tool)
        assert install.returncode == 0, install.stderr.decode(errors="replace")
        assert b"Start a new Codex task and verify tool discovery" in install.stdout
        repeated = scope.command("mcp", "install-published", "codex", tool)
        assert repeated.returncode == 0, repeated.stderr.decode(errors="replace")
        server = scope.server_name(tool)
        config = json.loads(cli("mcp", "get", server, "--json").stdout)
        transport = config["transport"]
        assert transport["command"] == str(scope.mcp) and transport["args"][0] == "export-serve"
        assert "--expected-generation" in transport["args"], transport
        assert transport["env"] is None and transport["cwd"] is None
        peer = JsonRpcPeer([transport["command"], *transport["args"]], scope.environment)
        peer.initialize()
        listed = peer.request("tools/list", timeout=30)["result"]["tools"]
        assert [entry["name"] for entry in listed] == [tool], listed
        called = peer.request("tools/call", {"name": tool, "arguments": {"input": "foo"}}, timeout=180)
        data = called["result"]["structuredContent"]["data"]
        assert data["outcome"] == "success" and base64.b64decode(data["stdout_base64"]) == b"FOO"
        gateway_data(scope.gateway_ready(tool, "bar"), tool, b"BAR")
        old_sandbox_boot = scope.sandbox_boot_id()
        revoked = scope.command("mcp", "unpublish", tool)
        assert revoked.returncode == 0, revoked.stderr.decode(errors="replace")
        scope.published.remove(tool)
        stale = peer.request("tools/call", {"name": tool, "arguments": {"input": "again"}}, timeout=30)
        assert "error" in stale or stale.get("result", {}).get("isError") is True, stale
        removed = scope.command("mcp", "remove-published", "codex", tool)
        assert removed.returncode == 0, removed.stderr.decode(errors="replace")
        cli("mcp", "get", server, "--json", expected=1)
        created = subprocess.run([scope.sbx, "create", "--name", fresh_sandbox, "shell"], cwd=scope.root,
                                 env=scope.environment, capture_output=True, timeout=180)
        assert created.returncode == 0, created.stderr.decode(errors="replace")
        fresh_sandbox_created = True
        fresh_sandbox_id = stock_vm_inventory(scope.sbx).get(fresh_sandbox)
        assert fresh_sandbox_id and fresh_sandbox_id not in scope.stock_before.values(), "new sandbox lacks stable identity"
        scope.published.add(tool)
        republished = scope.command("-c", f"mcp publish {tool} --sandbox {fresh_sandbox} -- 'cat | tr a-z A-Z'", timeout=180)
        assert republished.returncode == 0, republished.stderr.decode(errors="replace")
        assert peer.request("tools/list", timeout=30)["result"]["tools"] == [], "revoked exporter rediscovered an identical publication"
        stale_after_republish = peer.request("tools/call", {"name": tool, "arguments": {"input": "stale"}}, timeout=30)
        assert "error" in stale_after_republish or stale_after_republish.get("result", {}).get("isError") is True, stale_after_republish
        respawned_old = subprocess.run([transport["command"], *transport["args"]], cwd=scope.project,
                                      env=scope.environment, stdin=subprocess.DEVNULL, capture_output=True, timeout=30)
        assert respawned_old.returncode != 0 and b"publication generation changed" in respawned_old.stderr, respawned_old.stderr
        stopped = subprocess.run([scope.sbx, "stop", scope.sandbox], cwd=scope.root, env=scope.environment,
                                 capture_output=True, timeout=90)
        assert stopped.returncode == 0, stopped.stderr.decode(errors="replace")
        try:
            old_gateway = gateway_call(scope.sbx, scope.sandbox, tool, "stale", cwd=scope.root, env=scope.environment)
        except AssertionError as error:
            old_gateway = {"denied": str(error)}
        restarted_boot = scope.sandbox_boot_id()
        assert restarted_boot != old_sandbox_boot, "old sandbox did not restart before its stale Gateway probe"
        report["old_sandbox_boot_after_restart"] = restarted_boot
        old_call = old_gateway.get("call")
        assert old_call is None or "error" in old_call or old_call.get("result", {}).get("isError") is True, old_gateway
        report["old_gateway_after_republish"] = old_gateway
        # Do not scan host processes or signal an exporter owned by stock SBX.
        # The explicitly spawned old argv above already proves fresh-launch
        # rejection; repeat the old Gateway calls after the owned VM restart.
        # This is NOT a forced stock-exporter crash/respawn qualification.
        old_after_exporter_stop = []
        for _ in range(3):
            time.sleep(1)
            try:
                probe = gateway_call(scope.sbx, scope.sandbox, tool, "stale", cwd=scope.root,
                                     env=scope.environment)
            except AssertionError as error:
                probe = {"denied": str(error)}
            restarted_call = probe.get("call")
            assert (restarted_call is None or "error" in restarted_call
                    or restarted_call.get("result", {}).get("isError") is True), probe
            old_after_exporter_stop.append(probe)
        report["old_gateway_after_vm_restart"] = old_after_exporter_stop
        report["stock_exporter_crash_respawn"] = "not probed: stock-owned processes are outside signal authority"
        def fresh_gateway_ready():
            probe = gateway_call(scope.sbx, fresh_sandbox, tool, "gateway", cwd=scope.root, env=scope.environment)
            return probe if tool in probe["tools"] else None
        gateway_data(wait_until(fresh_gateway_ready, "fresh Gateway discovery", 20), tool, b"GATEWAY")
        installed_fresh = scope.command("mcp", "install-published", "codex", tool)
        assert installed_fresh.returncode == 0, installed_fresh.stderr.decode(errors="replace")
        fresh_transport = json.loads(cli("mcp", "get", server, "--json").stdout)["transport"]
        assert fresh_transport["args"] != transport["args"], "Codex registration reused revoked generation"
        fresh_peer = JsonRpcPeer([fresh_transport["command"], *fresh_transport["args"]], scope.environment)
        fresh_peer.initialize()
        assert [entry["name"] for entry in fresh_peer.request("tools/list", timeout=30)["result"]["tools"]] == [tool]
        fresh_call = fresh_peer.request("tools/call", {"name": tool, "arguments": {"input": "fresh"}}, timeout=180)
        fresh_data = fresh_call["result"]["structuredContent"]["data"]
        assert fresh_data["outcome"] == "success" and base64.b64decode(fresh_data["stdout_base64"]) == b"FRESH", fresh_call
        removed_fresh = scope.command("mcp", "remove-published", "codex", tool)
        assert removed_fresh.returncode == 0, removed_fresh.stderr.decode(errors="replace")
        cli("mcp", "add", server, "--", "/bin/cat")
        for operation in ("install-published", "remove-published"):
            conflict = scope.command("mcp", operation, "codex", tool)
            assert conflict.returncode != 0 and b"occupied" in conflict.stderr, conflict.stderr
        assert json.loads(cli("mcp", "get", server, "--json").stdout)["transport"]["command"] == "/bin/cat"
        report.update({"outcome": "passed", "tool": tool, "server": server,
                       "direct_stdout_base64": data["stdout_base64"], "revocation": "live old exporter, manually restarted old argv, and three old Gateway calls after VM restart denied; fresh exporter and Gateway succeeded",
                       "collision": "unchanged"})
    except Exception:
        report["failure"] = traceback.format_exc()
    finally:
        if peer is not None:
            peer.close()
        if fresh_peer is not None:
            fresh_peer.close()
        if fresh_sandbox_created:
            try:
                remove_owned_stock_vm(scope.sbx, fresh_sandbox, fresh_sandbox_id, scope.stock_before)
            except (OSError, ValueError, subprocess.SubprocessError) as error:
                report.setdefault("cleanup_errors", []).append(str(error))
        report.setdefault("cleanup_errors", []).extend(scope.cleanup())
        report["stock_after"] = scope.stock_after
        if report["cleanup_errors"]:
            report["outcome"] = "failed"
        args.evidence.mkdir(parents=True, exist_ok=True)
        (args.evidence / "published-codex-uat.json").write_text(json.dumps(report, indent=2) + "\n")
        (args.evidence / "published-codex-uat.json").chmod(0o600)
    print(json.dumps({"outcome": report["outcome"], "evidence": str(args.evidence / "published-codex-uat.json")}))
    return 0 if report["outcome"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
