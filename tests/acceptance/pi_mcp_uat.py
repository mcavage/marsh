#!/usr/bin/env python3
"""Real Pi and Pi ACP child calls to one published tool in disposable Kit VMs."""

import argparse
import json
from pathlib import Path
import re
import shlex
import subprocess
import traceback

from mcp_gateway_uat import Scope, sha256, wait_marker
from provenance import host_only_path


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-receipt", type=Path, required=True)
    parser.add_argument("--source-tree", type=Path, required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--marsh", type=Path, default=Path("target/release/marsh"))
    parser.add_argument("--sbx", type=Path, required=True)
    parser.add_argument("--guest-artifacts", type=Path, default=Path("target/libexec/marsh"))
    parser.add_argument("--evidence", type=Path, default=Path("target/acceptance/pi-mcp.json"))
    args = parser.parse_args()
    evidence = host_only_path(args.evidence.resolve(), args.source_tree)
    evidence.unlink(missing_ok=True)
    scope = Scope(args)
    source = args.source_tree.resolve()
    hashes = {}
    # One pi Kit serves plain `pi` and `pi-session` (its ACP mode), so both
    # paths load the same Gateway extension in the same Kit VM.
    for kit in ("marsh-pi",):
        source_file = source / "kits" / kit / "mcp-gateway.mjs"
        packaged_file = scope.guest_artifacts / "kits" / kit / "mcp-gateway.mjs"
        assert sha256(source_file) == sha256(packaged_file), f"packaged {kit} differs from source"
        hashes[kit] = sha256(packaged_file)
    report = {"schema": "marsh.pi-child-mcp-uat/v1", "outcome": "failed",
              "verified_build_receipt": scope.build_receipt,
              "source_identity": scope.build_receipt["source_after"],
              "stock_before": scope.stock_before,
              "marsh_sha256": sha256(scope.marsh), "kit_extension_sha256": hashes,
              "isolated_root": str(scope.root)}
    holder = None
    try:
        scope.scope_started = True
        holder = subprocess.Popen(
            [str(scope.marsh), "-c", "printf ready > .pi-mcp-ready; read _"],
            cwd=scope.project, env=scope.environment, stdin=subprocess.PIPE,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        wait_marker(scope.project / ".pi-mcp-ready", holder)
        scope.status()
        ready = scope.command("--load", "pi", "-c", "true", timeout=360)
        assert ready.returncode == 0, ready.stderr.decode(errors="replace")
        workers = scope.status()["workers"]
        vms = {}
        for kit in hashes:
            matches = [item["vm_id"] for item in workers if f"/{kit}@" in item.get("kit_profile", "")]
            assert len(matches) == 1, (kit, matches)
            vms[kit] = matches[0]
        report["child_sandboxes"] = vms
        tool = scope.tools[0]
        marker = scope.project / ".pi-mcp-called"
        scope.sandbox = vms["marsh-pi"]
        scope.publish_pipeline(tool, "cat | tr a-z A-Z | tee .pi-mcp-called")
        scope.gateway_ready(tool, "warmup")
        marker.unlink(missing_ok=True)
        prompt = (f"Use mcp_gateway_list, then mcp_gateway_call for {tool} with arguments "
                  '{"input":"foo"}. Do not run shell commands. Reply with only the uppercase result.')
        plain = scope.command("-c", f"pi -p {shlex.quote(prompt)}", timeout=360)
        report["plain_exit"] = plain.returncode
        report["plain_stdout"] = plain.stdout.decode(errors="replace")[-12000:]
        report["plain_stderr"] = plain.stderr.decode(errors="replace")[-4000:]
        assert plain.returncode == 0, report["plain_stderr"]
        assert marker.read_text() == "FOO", "plain Pi did not call the published tool"
        assert "FOO" in report["plain_stdout"], "plain Pi omitted tool output"
        marker.unlink()
        script = f'''acp run pi-session > .pi-mcp-transport.out 2> .pi-mcp-transport.err &
run_pid=$!
id=
tries=0
while [ -z "$id" ] && [ "$tries" -lt 120 ]; do
  id=$(acp list --wait | awk '$1 == "ready" && $2 == "pi-session" {{ print $3 }}')
  tries=$((tries+1))
  [ -n "$id" ] || sleep 1
done
if [ -n "$id" ]; then
  acp ask "$id" {shlex.quote(prompt)}
  answer_status=$?
  status=$(acp status "$id" --json)
  printf '@@ANSWER_STATUS@@%s\\n' "$answer_status"
  printf '@@STATUS@@%s\\n' "$status"
  acp stop "$id"
  wait "$run_pid"
else
  cat .pi-mcp-transport.err
  exit 31
fi
'''
        acp = scope.command("-c", script, timeout=360)
        report["acp_exit"] = acp.returncode
        report["acp_stdout"] = acp.stdout.decode(errors="replace")[-16000:]
        report["acp_stderr"] = acp.stderr.decode(errors="replace")[-4000:]
        assert acp.returncode == 0, report["acp_stderr"]
        answer = re.search(r"^@@ANSWER_STATUS@@(\d+)$", report["acp_stdout"], re.MULTILINE)
        status = re.search(r"^@@STATUS@@(.+)$", report["acp_stdout"], re.MULTILINE)
        assert answer and answer.group(1) == "0", report["acp_stdout"]
        assert status, report["acp_stdout"]
        details = json.loads(status.group(1))
        assert details["last_stop_reason"] == "end_turn" and details["last_error"] is None, details
        assert marker.read_text() == "FOO", "Pi ACP did not call the published tool"
        assert "FOO" in report["acp_stdout"], "Pi ACP omitted tool output"
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
        evidence.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        evidence.chmod(0o600)
        print(json.dumps({"outcome": report["outcome"], "evidence": str(evidence)}))
    return 0 if report["outcome"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
