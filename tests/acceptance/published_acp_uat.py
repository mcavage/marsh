#!/usr/bin/env python3
"""Stock-SBX ACP publication journey through host Codex and a child MCP Gateway."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import traceback
import uuid

from acp_uat import Scope
from mcp_gateway_uat import sha256, wait_marker, wait_until
from run import source_identity

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "mcp"))
from jsonrpc_peer import JsonRpcPeer  # noqa: E402


GATEWAY_PROBE = r'''
import base64, json, os, sys, urllib.request
url = os.environ["MCP_GATEWAY_URL"]
name = sys.argv[1]
args = json.loads(base64.b64decode(sys.argv[2]))
headers = {"Content-Type":"application/json", "Accept":"application/json, text/event-stream", "MCP-Protocol-Version":"2025-06-18"}
def post(message):
    req = urllib.request.Request(url, data=json.dumps(message).encode(), method="POST", headers=headers)
    with urllib.request.urlopen(req, timeout=25) as response:
        session = response.headers.get("Mcp-Session-Id")
        if session: headers["Mcp-Session-Id"] = session
        body = response.read(1024 * 1024).decode()
    if body.startswith("event:"):
        for line in body.splitlines():
            if line.startswith("data: "): return json.loads(line[6:])
    return json.loads(body) if body else None
post({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"marsh-acp-publication-uat","version":"1"}}})
post({"jsonrpc":"2.0","method":"notifications/initialized"})
listed = post({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})
tools = [item["name"] for item in listed.get("result",{}).get("tools",[])]
called = post({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":name,"arguments":args}}) if name in tools else None
print(json.dumps({"tools":tools,"call":called},sort_keys=True))
'''


def data(reply: dict) -> dict:
    assert "result" in reply, reply
    result = reply["result"]
    assert result.get("isError") is not True, reply
    payload = result["structuredContent"]
    assert payload["ok"] is True, payload
    return payload["result"]


def peer_call(peer: JsonRpcPeer, name: str, **arguments) -> dict:
    return peer.request("tools/call", {"name": name, "arguments": arguments}, timeout=30)


def gateway_call(scope: Scope, sandbox: str, name: str, **arguments) -> dict:
    encoded = base64.b64encode(json.dumps(arguments).encode()).decode()
    result = subprocess.run(
        [scope.sbx, "exec", sandbox, "python3", "-c", GATEWAY_PROBE, name, encoded],
        cwd=scope.project, env=scope.environment, stdin=subprocess.DEVNULL,
        capture_output=True, timeout=60, start_new_session=True,
    )
    assert result.returncode == 0, result.stderr.decode(errors="replace")
    return json.loads(result.stdout)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--marsh", required=True)
    parser.add_argument("--sbx", required=True)
    parser.add_argument("--guest-artifacts", required=True)
    parser.add_argument("--codex", required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--source-tree", type=Path, required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--build-receipt", required=True)
    args = parser.parse_args()
    args.evidence.mkdir(parents=True, mode=0o700, exist_ok=True)
    identity = source_identity(args.source_tree.resolve(strict=True), args.source_revision)
    scope = Scope(args)
    scope.home.chmod(0o700)
    scope.environment["CODEX_HOME"] = str(scope.root / "codex-home")
    (scope.root / "codex-home").mkdir(mode=0o700)
    codex = Path(args.codex).resolve(strict=True)
    sandbox = f"marsh-acp-pub-{uuid.uuid4().hex[:8]}"
    tool = f"agent_{uuid.uuid4().hex[:8]}"
    key = hashlib.sha256(os.fsencode(scope.project.resolve()) + b"\0" + os.fsencode((scope.home / "home").resolve())).hexdigest()
    server = f"marsh-acp-{key[:12]}-{tool}"
    report = {
        "schema": "marsh.published-acp-uat/v1", "outcome": "failed", "source_identity": identity,
        "sandbox": sandbox, "tool": tool, "server": server,
        "binary_sha256": {"marsh": sha256(Path(scope.marsh)), "marsh-mcp": sha256(Path(scope.marsh).with_name("marsh-mcp")), "codex": sha256(codex)},
        "harness_sha256": sha256(Path(__file__)),
        "build_receipt": scope.build_receipt, "stock_before": scope.stock_before,
    }
    peer = None
    shell = None
    sandbox_created = False
    codex_installed = False
    log_out = log_err = None

    def host(*words: str, timeout: float = 60) -> subprocess.CompletedProcess[bytes]:
        return subprocess.run([scope.marsh, *words], cwd=scope.project, env=scope.environment,
                              stdin=subprocess.DEVNULL, capture_output=True, timeout=timeout, start_new_session=True)

    try:
        scope.public_status()
        created = subprocess.run([scope.sbx, "create", "--name", sandbox, "shell"], cwd=scope.project,
                                 env=scope.environment, stdin=subprocess.DEVNULL, capture_output=True, timeout=180, start_new_session=True)
        assert created.returncode == 0, created.stderr.decode(errors="replace")
        sandbox_created = True
        script = f'''id=$(acp reserve fixture-session) || exit 31
acp run --reservation "$id" fixture-session > .acp-run.out 2> .acp-run.err &
run_pid=$!
acp list --wait "$id" --mine || exit 31
acp ask "$id" before-publication || exit 31
acp publish "$id" --sandbox {sandbox} --name {tool} || exit 32
for action in prompt cancel stop; do
  if [ "$action" = prompt ]; then
    acp prompt "$id" forbidden > .denied-$action 2>&1
  else
    acp "$action" "$id" > .denied-$action 2>&1
  fi
  [ "$?" -ne 0 ] || exit 38
  grep -q 'acp unpublish' .denied-$action || exit 39
done
acp list --json > .acp-published-list
acp status "$id" --json > .acp-published-status
printf '%s\\n' "$id" > .acp-published-id
kill "$run_pid" 2>/dev/null || true
wait "$run_pid" 2>/dev/null || true
printf ready > .acp-published-ready
while [ ! -f .acp-published-finish ]; do sleep 0.2; done
acp unpublish {tool} || exit 33
acp publish "$id" --name {tool} || exit 35
printf ready > .acp-published-again
while [ ! -f .acp-published-finish-again ]; do sleep 0.2; done
acp unpublish {tool} || exit 36
acp publish "$id" --name {tool} || exit 37
printf ready > .acp-published-orphaned
'''
        log_out = (args.evidence / "publishing-shell.stdout").open("wb")
        log_err = (args.evidence / "publishing-shell.stderr").open("wb")
        shell = subprocess.Popen([scope.marsh, "-c", script], cwd=scope.project, env=scope.environment,
                                 stdin=subprocess.DEVNULL, stdout=log_out, stderr=log_err, start_new_session=True)
        wait_marker(scope.project / ".acp-published-ready", shell, 240)
        scope.public_status()  # remember the actual fixture Kit for failure cleanup
        agent_id = (scope.project / ".acp-published-id").read_text().strip()
        report["agent_session_id"] = agent_id
        listed_shell = json.loads((scope.project / ".acp-published-list").read_text())
        assert next(item for item in listed_shell if item["agent_session_id"] == agent_id)["published_name"] == tool
        status_shell = json.loads((scope.project / ".acp-published-status").read_text())
        assert status_shell["published_name"] == tool
        collision = host("-c", f"mcp publish {tool} -- 'cat'", timeout=90)
        assert collision.returncode != 0, "pipeline MCP tool reused published ACP name"
        assert b"other protocol" in collision.stderr, collision.stderr.decode(errors="replace")
        installed = host("acp", "install-published", "codex", tool)
        assert installed.returncode == 0, installed.stderr.decode(errors="replace")
        codex_installed = True
        inspected = subprocess.run([str(codex), "mcp", "get", server, "--json"], cwd=scope.project,
                                   env=scope.environment, capture_output=True, timeout=30, start_new_session=True)
        assert inspected.returncode == 0, inspected.stderr.decode(errors="replace")
        transport = json.loads(inspected.stdout)["transport"]
        assert transport["args"][0] == "acp-export-serve", transport
        assert "--expected-generation" in transport["args"], transport
        peer = JsonRpcPeer([transport["command"], *transport["args"]], scope.environment)
        peer.initialize()
        listed = peer.request("tools/list", timeout=30)["result"]["tools"]
        assert [item["name"] for item in listed] == [tool], listed
        first_key = str(uuid.uuid4())
        first = data(peer_call(peer, tool, action="ask", text="first", key=first_key))
        assert first["turn_id"] and "status" not in first
        assert isinstance(first["start_cursor"], int)
        done = wait_until(lambda: (value if (value := data(peer_call(peer, tool, action="status")))["last_stop_reason"] == "end_turn" else None), "host ACP first turn", 30)
        assert "fixture:first" in json.dumps(done["updates"]), done
        retry = data(peer_call(peer, tool, action="ask", text="first", key=first_key))
        assert retry["turn_id"] == first["turn_id"]
        changed = peer_call(peer, tool, action="ask", text="changed", key=first_key)
        assert changed["result"].get("isError") is True, changed
        # Follow the documented exclusive cursor literally, including idle polls.
        slow = data(peer_call(peer, tool, action="ask", text="slow-6", key=str(uuid.uuid4())))
        cursor = slow["start_cursor"]
        chunks = []
        idle_polls = 0
        deadline = time.monotonic() + 30
        while True:
            page = data(peer_call(peer, tool, action="status", turn_id=slow["turn_id"], cursor=cursor))
            assert not page["updates_lost"], page
            if not page["updates"]:
                assert page["next_cursor"] == cursor, page
                idle_polls += 1
            for update in page["updates"]:
                assert update["turn_id"] == slow["turn_id"], update
                chunks.append(update["update"]["content"]["text"])
            cursor = page["next_cursor"]
            if not page["turn_active"] and not page["more_updates"]:
                break
            assert time.monotonic() < deadline, "slow paging timed out"
            time.sleep(0.04)
        assert chunks == ["fixture:slow-6", *(f"u{n};" for n in range(6))], chunks
        assert idle_polls > 0, "idle-cursor path was not exercised"
        # A healthy consumer must retain the complete post-cancel tail.
        tail = data(peer_call(peer, tool, action="ask", text="cancel-burst", key=str(uuid.uuid4())))
        wait_until(lambda: data(peer_call(peer, tool, action="status", turn_id=tail["turn_id"]))["updates"], "cancel burst started", 30)
        data(peer_call(peer, tool, action="cancel"))
        cursor = tail["start_cursor"]
        tail_pages, tail_chunks = [], []
        deadline = time.monotonic() + 30
        while True:
            page = data(peer_call(peer, tool, action="status", turn_id=tail["turn_id"], cursor=cursor))
            tail_pages.append(page)
            assert not page["updates_lost"], page
            assert all(u["turn_id"] == tail["turn_id"] for u in page["updates"]), page
            tail_chunks.extend(u["update"]["content"]["text"] for u in page["updates"])
            cursor = page["next_cursor"]
            if not page["turn_active"] and not page["more_updates"]:
                assert page["last_stop_reason"] == "cancelled", page
                break
            assert time.monotonic() < deadline, "post-cancel paging timed out"
            time.sleep(0.02)
        assert tail_chunks == ["fixture:cancel-burst", *(f"c{n};" for n in range(200))], tail_chunks
        before_late = page["session_out_of_turn_updates"]
        late_turn = data(peer_call(peer, tool, action="ask", text="late", key=str(uuid.uuid4())))
        late = wait_until(lambda: (value if not (value := data(peer_call(peer, tool, action="status", turn_id=late_turn["turn_id"]))) ["turn_active"] and value["session_out_of_turn_updates"] > before_late else None), "wire terminal late update", 30)
        assert late["session_out_of_turn_updates"] == before_late + 1 and not late["updates_lost"], late
        assert [u["update"]["content"]["text"] for u in late["updates"]] == ["fixture:late"], late
        report["cancel_tail_pages"] = tail_pages
        report["late_update"] = late
        held = data(peer_call(peer, tool, action="ask", text="hold", key=str(uuid.uuid4())))
        data(peer_call(peer, tool, action="cancel"))
        cancelled = wait_until(lambda: (value if not (value := data(peer_call(peer, tool, action="status", turn_id=held["turn_id"]))) ["turn_active"] else None), "published cancel", 30)
        assert cancelled["last_stop_reason"] == "cancelled", cancelled
        gateway_key = str(uuid.uuid4())
        gateway = gateway_call(scope, sandbox, tool, action="ask", text="from-child", key=gateway_key)
        assert tool in gateway["tools"], gateway
        data(gateway["call"])
        child_done = wait_until(lambda: (value if "fixture:from-child" in json.dumps((value := data(peer_call(peer, tool, action="status")))["updates"]) and not value["turn_active"] else None), "child ACP turn", 30)
        assert child_done["terminal"] is False
        permission_key = str(uuid.uuid4())
        data(peer_call(peer, tool, action="ask", text="permission", key=permission_key))
        offered = wait_until(lambda: (value if (value := data(peer_call(peer, tool, action="status")))["permissions"] else None), "ACP permission request", 30)
        permission = offered["permissions"][0]
        assert {item["optionId"] for item in permission["options"]} == {"once", "deny"}
        data(peer_call(peer, tool, action="respond", request_id=permission["request_id"], option_id="once"))
        permitted = wait_until(lambda: (value if "fixture:permission:allowed" in json.dumps((value := data(peer_call(peer, tool, action="status")))["updates"]) and not value["turn_active"] else None), "approved ACP turn", 30)
        assert permitted["turn_active"] is False
        data(peer_call(peer, tool, action="ask", text="oversize-update", key=str(uuid.uuid4())))
        wait_until(lambda: (value if not (value := data(peer_call(peer, tool, action="status")))["turn_active"] else None), "oversize turn terminal", 30)
        older = data(peer_call(peer, tool, action="status", turn_id=first["turn_id"], cursor=first["start_cursor"]))
        assert not older["updates_lost"] and [u["update"]["content"]["text"] for u in older["updates"]] == ["fixture:first"], older
        data(peer_call(peer, tool, action="ask", text="always-only", key=str(uuid.uuid4())))
        denied = wait_until(lambda: (value if not (value := data(peer_call(peer, tool, action="status")))["turn_active"] else None), "persistent permission denied", 30)
        assert "allow_once" in denied["permission_note"], denied
        older = data(peer_call(peer, tool, action="status", turn_id=first["turn_id"], cursor=first["start_cursor"]))
        assert older["permission_note"] is None and older["last_stop_reason"] == "end_turn", older
        (scope.project / ".acp-published-finish").write_text("finish\n")
        wait_marker(scope.project / ".acp-published-again", shell, 90)
        stale = peer_call(peer, tool, action="status")
        assert stale["result"].get("isError") is True, stale
        respawned_old = subprocess.run([transport["command"], *transport["args"]], cwd=scope.project,
                                      env=scope.environment, stdin=subprocess.DEVNULL, capture_output=True, timeout=30, start_new_session=True)
        assert respawned_old.returncode != 0 and b"ACP publication generation changed" in respawned_old.stderr, respawned_old.stderr
        removed_old = host("acp", "remove-published", "codex", tool)
        assert removed_old.returncode == 0, removed_old.stderr.decode(errors="replace")
        installed_fresh = host("acp", "install-published", "codex", tool)
        assert installed_fresh.returncode == 0, installed_fresh.stderr.decode(errors="replace")
        refreshed = subprocess.run([str(codex), "mcp", "get", server, "--json"], cwd=scope.project,
                                   env=scope.environment, capture_output=True, timeout=30, start_new_session=True)
        assert refreshed.returncode == 0, refreshed.stderr.decode(errors="replace")
        fresh_transport = json.loads(refreshed.stdout)["transport"]
        assert fresh_transport["args"] != transport["args"], "new ACP registration reused revoked generation"
        fresh_peer = JsonRpcPeer([fresh_transport["command"], *fresh_transport["args"]], scope.environment)
        try:
            fresh_peer.initialize()
            fresh = data(peer_call(fresh_peer, tool, action="status"))
            assert fresh["terminal"] is False, fresh
        finally:
            fresh_peer.close()
        (scope.project / ".acp-published-finish-again").write_text("finish\n")
        wait_marker(scope.project / ".acp-published-orphaned", shell, 90)
        shell.wait(timeout=90)
        assert shell.returncode == 0, f"publishing shell exited {shell.returncode}"
        stale = peer_call(peer, tool, action="status")
        assert stale["result"].get("isError") is True, stale
        cleaned = host("-c", f"acp unpublish {tool}", timeout=120)
        assert cleaned.returncode == 0, cleaned.stderr.decode(errors="replace")
        assert b"Revoked" in cleaned.stdout, cleaned.stdout
        repeated = host("-c", f"acp unpublish {tool}", timeout=120)
        assert repeated.returncode == 0, repeated.stderr.decode(errors="replace")
        assert b"No host ACP registration remains" in repeated.stdout, repeated.stdout
        absent = subprocess.run([scope.sbx, "mcp", "inspect", server, "--json"],
                                cwd=scope.project, env=scope.environment,
                                capture_output=True, timeout=20, start_new_session=True)
        assert absent.returncode != 0, "new-shell unpublish left a host registration"
        for _ in range(120):
            stopped = host("stop", "--json", timeout=180)
            if stopped.returncode == 0:
                break
            assert b"attached shells or active jobs" in stopped.stderr, stopped.stderr
            time.sleep(0.5)
        assert stopped.returncode == 0, stopped.stderr.decode(errors="replace")
        assert json.loads(stopped.stdout)["cleanup_complete"] is True, stopped.stdout
        removed = host("acp", "remove-published", "codex", tool)
        assert removed.returncode == 0, removed.stderr.decode(errors="replace")
        report.update(outcome="passed", host_turn="fixture:first", child_turn="fixture:from-child",
                      permission="one-time allow", retry="one turn", revocation="cached tool denied",
                      name_collision="rejected", republish="fresh grant; stale peer and old exporter argv denied",
                      detached_publisher_cleanup="new shell removed host registration; repeat was safe")
    except Exception:
        report["failure"] = traceback.format_exc()
    finally:
        if peer is not None:
            peer.close()
        if shell is not None and shell.poll() is None:
            (scope.project / ".acp-published-finish").write_text("finish\n")
            (scope.project / ".acp-published-finish-again").write_text("finish\n")
            try:
                shell.wait(timeout=30)
            except subprocess.TimeoutExpired:
                shell.kill()
                shell.wait()
        for stream in (log_out, log_err):
            if stream is not None:
                stream.close()
        errors = []
        if report["outcome"] != "passed":
            revoked = host("-c", f"acp unpublish {tool}", timeout=120)
            if revoked.returncode:
                errors.append("ACP grant revocation failed")
        if codex_installed:
            removed = host("acp", "remove-published", "codex", tool)
            if removed.returncode:
                errors.append("Codex registration removal failed")
        inspected = subprocess.run([scope.sbx, "mcp", "inspect", server, "--json"], cwd=scope.project,
                                   env=scope.environment, stdin=subprocess.DEVNULL, capture_output=True, timeout=20, start_new_session=True)
        if inspected.returncode == 0:
            removed = subprocess.run([scope.sbx, "mcp", "rm", "--force", server], cwd=scope.project,
                                     env=scope.environment, stdin=subprocess.DEVNULL, capture_output=True, timeout=60, start_new_session=True)
            if removed.returncode:
                errors.append("stock SBX ACP registration removal failed")
        if sandbox_created:
            removed = subprocess.run([scope.sbx, "rm", "--force", sandbox], cwd=scope.project,
                                     env=scope.environment, stdin=subprocess.DEVNULL, capture_output=True, timeout=90, start_new_session=True)
            if removed.returncode:
                errors.append("child sandbox removal failed")
        errors.extend(scope.cleanup_isolated_scope())
        report["stock_after"] = getattr(scope, "stock_after", {})
        report["cleanup_errors"] = errors
        if errors:
            report["outcome"] = "failed"
        args.evidence.mkdir(parents=True, mode=0o700, exist_ok=True)
        (args.evidence / "published-acp-uat.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"outcome": report["outcome"], "evidence": str(args.evidence / "published-acp-uat.json")}))
    return 0 if report["outcome"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
