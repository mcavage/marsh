#!/usr/bin/env python3
"""Call a published MCP tool from a real Claude or Codex ACP child Kit."""

import argparse
import json
from pathlib import Path
import re
import select
import shlex
import subprocess
import sys
import traceback

from mcp_gateway_uat import Scope, sha256, wait_marker
from provenance import host_only_path
from run import source_identity


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-receipt", type=Path,
                        help="optional observed-build receipt; absent records source identity only")
    parser.add_argument("--source-tree", type=Path, required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--commands", type=Path,
                        help="optional isolated commands.json overlay (e.g. staged Kit sources or OCI digests)")
    parser.add_argument("--agent", choices=("claude-session", "codex-session"), required=True)
    parser.add_argument("--marsh", type=Path, default=Path("target/release/marsh"))
    parser.add_argument("--sbx", type=Path, required=True)
    parser.add_argument("--guest-artifacts", type=Path, default=Path("target/libexec/marsh"))
    parser.add_argument("--evidence", type=Path, default=Path("target/acceptance/acp-mcp"))
    args = parser.parse_args()
    evidence = args.evidence.resolve()
    host_only_path(evidence / f"{args.agent}.json", args.source_tree).unlink(missing_ok=True)
    scope = Scope(args)
    command = args.agent.replace("-session", "-acp")
    kit = f"marsh-{command}"
    source_kit = args.source_tree.resolve() / "kits" / kit / "mcp-gateway-proxy.mjs"
    packaged_kit = scope.guest_artifacts / "kits" / kit / "mcp-gateway-proxy.mjs"
    assert sha256(source_kit) == sha256(packaged_kit), "packaged ACP Kit differs from source"
    tool = scope.tools[0]
    report = {"schema": "marsh.acp-child-mcp-uat/v1", "agent": args.agent,
              "outcome": "failed", "verified_build_receipt": scope.build_receipt,
              "source_identity": (scope.build_receipt["source_after"] if scope.build_receipt
                                  else source_identity(args.source_tree, args.source_revision)),
              "stock_before": scope.stock_before,
              "marsh_sha256": sha256(scope.marsh),
              "kit_sha256": sha256(packaged_kit),
              "isolated_root": str(scope.root)}
    holder = None
    try:
        scope.scope_started = True
        holder = subprocess.Popen(
            [str(scope.marsh), "-c", "printf ready > .acp-mcp-ready; read _"],
            cwd=scope.project, env=scope.environment, stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        wait_marker(scope.project / ".acp-mcp-ready", holder)
        scope.status()
        ready = scope.command("--load", command, "-c", "true", timeout=240)
        assert ready.returncode == 0, ready.stderr.decode(errors="replace")
        workers = [item for item in scope.status()["workers"]
                   if f"/{kit}@" in item.get("kit_profile", "")]
        assert len(workers) == 1, workers
        scope.sandbox = workers[0]["vm_id"]
        report["child_sandbox"] = scope.sandbox
        initialize = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                                 "params": {"protocolVersion": 1,
                                            "clientCapabilities": {"terminal": False},
                                            "clientInfo": {"name": "marsh-uat", "version": "1"}}})
        probe = subprocess.Popen(
            [str(scope.marsh), "-c", command], cwd=scope.project, env=scope.environment,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        try:
            probe.stdin.write((initialize + "\n").encode())
            probe.stdin.flush()
            ready, _, _ = select.select([probe.stdout], [], [], 90)
            assert ready, "packaged ACP adapter did not answer initialize"
            initialized = json.loads(probe.stdout.readline())
            assert initialized.get("id") == 1 and initialized.get("result"), initialized
        finally:
            probe.stdin.close()
            try:
                probe.wait(timeout=15)
            except subprocess.TimeoutExpired:
                probe.terminate()
                probe.wait(timeout=15)
        capabilities = initialized.get("result", {}).get("agentCapabilities", {})
        report["mcp_capabilities"] = capabilities.get("mcpCapabilities")
        assert report["mcp_capabilities"] and report["mcp_capabilities"].get("http") is True, capabilities
        pipeline = "cat | tr a-z A-Z | tee .acp-mcp-called"
        published = scope.command("-c", f"mcp publish {shlex.quote(tool)} --kit {shlex.quote(command)} -- {shlex.quote(pipeline)}", timeout=180)
        assert published.returncode == 0, published.stderr.decode(errors="replace")
        assert f"Loaded into sandbox: {scope.sandbox}".encode() in published.stdout, published.stdout
        scope.published.add(tool)
        scope.gateway_ready(tool, "warmup")
        marker = scope.project / ".acp-mcp-called"
        marker.unlink(missing_ok=True)
        prompt = (f"Call the mcp-gateway tool named {tool} with input exactly foo. "
                  "Reply with only the tool's returned uppercase text. Do not run shell commands.")
        script = f'''acp run {args.agent} > .acp-mcp-transport.out 2> .acp-mcp-transport.err &
run_pid=$!
id=
tries=0
while [ -z "$id" ] && [ "$tries" -lt 120 ]; do
  id=$(acp list --wait | awk '$1 == "ready" && $2 == "{args.agent}" {{ print $3 }}')
  tries=$((tries+1))
  [ -n "$id" ] || sleep 1
done
if [ -n "$id" ]; then
  acp prompt "$id" {shlex.quote(prompt)}
  prompt_status=$?
  expected_tool={shlex.quote('mcp__mcp-gateway__' + tool)}
  tries=0
  while [ "$tries" -lt 180 ]; do
    status=$(acp status "$id" --json)
    case "$status" in
      *'"turn_active":false'*) break ;;
    esac
    permission=$(acp permissions "$id" --json)
    request=$(printf '%s' "$permission" | sed -n 's/.*"request_id":"\\([^"]*\\)".*/\\1/p')
    if [ -n "$request" ]; then
      title=$(printf '%s' "$permission" | sed -n 's/.*"title":"\\([^"]*\\)".*/\\1/p')
      [ "$title" = "$expected_tool" ] || break
      case "$permission" in
        *'"input_preview":"{{\\"input\\":\\"foo\\"}}"'*) ;;
        *) break ;;
      esac
      option=$(printf '%s' "$permission" | sed -n 's/.*"optionId":"\\([^"]*\\)","name":"[^"]*","kind":"allow_once".*/\\1/p')
      [ -n "$option" ] || break
      acp respond "$id" "$request" "$option" || break
    fi
    tries=$((tries+1))
    sleep 1
  done
  printf '@@PROMPT_STATUS@@%s\\n' "$prompt_status"
  printf '@@STATUS@@%s\\n' "$status"
  acp stop "$id"
  wait "$run_pid"
  wait_status=$?
  printf '@@WAIT@@%s\\n' "$wait_status"
  printf '@@FINAL@@%s\\n' "$(acp status "$id" --json)"
else
  cat .acp-mcp-transport.err
  exit 31
fi
'''
        run = scope.command("-c", script, timeout=360)
        report["shell_exit"] = run.returncode
        full_stdout = run.stdout.decode(errors="replace")
        (evidence / f"{args.agent}.shell-stdout").write_text(full_stdout)
        report["shell_stdout"] = full_stdout[-16000:]
        report["shell_stderr"] = run.stderr.decode(errors="replace")[-4000:]
        assert run.returncode == 0, report["shell_stderr"]
        answer = re.search(r"^@@PROMPT_STATUS@@(\d+)$", full_stdout, re.MULTILINE)
        status = re.search(r"^@@STATUS@@(.+)$", full_stdout, re.MULTILINE)
        assert answer and answer.group(1) == "0", report["shell_stdout"]
        # Brush `wait` reports the Kit receipt's exit (125 when the stopped Kit
        # has no exit code), as in acp_uat; the stopped Kit's cleanup must verify.
        waited = re.search(r"^@@WAIT@@(\d+)$", full_stdout, re.MULTILINE)
        final = re.search(r"^@@FINAL@@(.+)$", full_stdout, re.MULTILINE)
        assert waited and final, report["shell_stdout"]
        receipt = json.loads(final.group(1))["receipt"]
        report["final_receipt"] = {key: receipt.get(key) for key in ("state", "exit", "cleanup", "job_id")}
        assert receipt["cleanup"] == "verified", report["final_receipt"]
        code = (receipt["exit"] or {}).get("code")
        assert int(waited.group(1)) == (code if code is not None else 125), (waited.group(1), report["final_receipt"])
        assert status, report["shell_stdout"]
        details = json.loads(status.group(1))
        assert details["last_stop_reason"] == "end_turn" and details["last_error"] is None, details
        assert marker.read_text() == "FOO", "published MCP pipeline was not called"
        answer_text = "".join(update["update"].get("content", {}).get("text", "")
                              for update in details["updates"]
                              if update["update"].get("sessionUpdate") == "agent_message_chunk")
        assert "FOO" in answer_text, "ACP agent answer omitted the tool result"
        report["outcome"] = "passed"
    except Exception:
        report["failure"] = traceback.format_exc()
    finally:
        if holder is not None and holder.poll() is None:
            try:
                holder.stdin.write(b"done\n")
                holder.stdin.flush()
                holder.wait(timeout=15)
            except Exception:
                holder.terminate()
                holder.wait(timeout=15)
        report["cleanup_errors"] = scope.cleanup()
        report["stock_after"] = scope.stock_after
        if report["cleanup_errors"]:
            report["outcome"] = "failed"
        path = evidence / f"{args.agent}.json"
        path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        path.chmod(0o600)
        print(json.dumps({"outcome": report["outcome"], "evidence": str(path)}))
    return 0 if report["outcome"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
