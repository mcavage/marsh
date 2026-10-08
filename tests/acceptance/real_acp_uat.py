#!/usr/bin/env python3
"""Real-stock-SBX E2E for one packaged ACP agent, with optional live inference."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import select
import signal
import shutil
import subprocess
import sys
import tempfile
import time

from run import source_identity
from provenance import host_only_path, stock_cleanup_errors, stock_vm_names, verify_candidate


AGENTS = ("claude-session", "codex-session", "pi-session")
# One Kit per agent: `NAME-session` runs the `NAME` Kit in its ACP mode.
KIT_DIRS = {name: f"marsh-{name.split('-')[0]}" for name in AGENTS}


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(chunk)
    return f"sha256:{value.hexdigest()}"


def tree_digest(root: Path) -> str:
    if not root.is_dir():
        raise FileNotFoundError(root)
    value = hashlib.sha256()
    for path in sorted(path for path in root.rglob("*") if path.is_file()):
        value.update(str(path.relative_to(root)).encode() + b"\0")
        value.update(bytes.fromhex(digest(path)[7:]))
    return f"sha256:{value.hexdigest()}"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-receipt", type=Path, required=True)
    parser.add_argument("--source-tree", type=Path, required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--agent", choices=AGENTS, required=True)
    parser.add_argument("--live", action="store_true", help="require a real PONG model response")
    parser.add_argument("--expect-auth-required", action="store_true")
    parser.add_argument("--evidence", type=Path)
    parser.add_argument("--marsh", type=Path, default=Path("target/release/marsh"))
    parser.add_argument("--sbx", type=Path, default=Path(shutil.which("sbx") or "sbx"))
    parser.add_argument("--guest-artifacts", type=Path, default=Path("target/libexec/marsh"))
    args = parser.parse_args()
    if args.live and args.expect_auth_required:
        parser.error("--live and --expect-auth-required are mutually exclusive")
    if args.expect_auth_required and args.agent != "codex-session":
        parser.error("--expect-auth-required currently qualifies only Codex")
    source = args.source_tree.resolve(strict=True)
    identity = source_identity(source, args.source_revision)
    marsh = args.marsh.resolve(strict=True)
    guest, build_receipt = verify_candidate(args, str(marsh))
    sbx = args.sbx.resolve(strict=True)
    kit_source = tree_digest(source / "kits" / KIT_DIRS[args.agent])
    kit_package = tree_digest(guest / "kits" / KIT_DIRS[args.agent])
    if kit_source != kit_package:
        parser.error("packaged ACP Kit differs from the source tree; run make build")
    mode = "live" if args.live else "auth-required" if args.expect_auth_required else "start"
    if args.evidence is None:
        parser.error("--evidence must name a host-only path outside the source checkout")
    evidence_path = host_only_path(args.evidence.resolve(), source)
    evidence_path.unlink(missing_ok=True)
    baseline = stock_vm_names(sbx)
    evidence = {
        "schema": "marsh.real-acp-uat/v1", "agent": args.agent, "mode": mode,
        **identity, "verified_build_receipt": build_receipt, "stock_before": baseline,
        "sbx_version": subprocess.check_output([str(sbx), "version"], text=True).strip(),
        "marsh_binary_sha256": digest(marsh),
        "kit_source_sha256": kit_source,
        "kit_package_sha256": kit_package,
        "guest_artifact_sha256": {
            name: digest(guest / name) for name in
            ("marsh-linux-arm64", "marsh-worker-linux-arm64", "marsh-relay-linux-arm64",
             "commands.json", "agents.json")
        },
    }

    def finish(code: int, detail: str) -> int:
        outcome = "failed" if code else "passed" if args.live else "passed_partial"
        evidence.update(outcome=outcome, detail=detail,
                        qualification="live provider" if args.live else "startup/auth diagnostic only")
        evidence_path.parent.mkdir(parents=True, exist_ok=True)
        evidence_path.write_text(json.dumps(evidence, indent=2) + "\n")
        evidence_path.chmod(0o600)
        print(detail, file=sys.stdout if code == 0 else sys.stderr)
        return 2 if outcome == "passed_partial" else code
    script = f'''acp run {args.agent} > .acp-transport.out 2> .acp-transport.err &
run_pid=$!
id=
tries=0
while [ -z "$id" ] && [ "$tries" -lt 120 ]; do
  id=$(acp list --wait | awk '$1 == "ready" && $2 == "{args.agent}" {{ print $3 }}')
  tries=$((tries+1))
  if [ -z "$id" ] && grep -q 'Authentication required' .acp-transport.err 2>/dev/null; then break; fi
  [ -n "$id" ] || sleep 1
done
if [ -z "$id" ]; then
  if grep -q 'Authentication required' .acp-transport.err; then printf '@@AUTH_REQUIRED@@\n'; fi
  tail -20 .acp-transport.err
  exit 31
fi
'''
    if args.live:
        script += '''acp prompt "$id" 'Reply with exactly PONG and no other text.' || exit 32
tries=0
while [ "$tries" -lt 90 ]; do
  status=$(acp status "$id" --json)
  if printf '%s' "$status" | grep -q '"last_stop_reason":"end_turn"'; then break; fi
  if printf '%s' "$status" | grep -q '"last_error":"[^" ]'; then break; fi
  tries=$((tries+1))
  sleep 1
done
if printf '%s' "$status" | grep -q '"last_stop_reason":"end_turn"'; then
  printf '@@FIRST@@%s\\n' "$status"
  after=$(printf '%s' "$status" | sed -n 's/.*"next_cursor":\\([0-9][0-9]*\\).*/\\1/p')
  [ -n "$after" ] || exit 38
  acp prompt "$id" 'Reply with exactly PONG and no other text.' || exit 34
  tries=0
  while [ "$tries" -lt 90 ]; do
    status=$(acp status "$id" "$after" --json)
    if printf '%s' "$status" | grep -q '"last_stop_reason":"end_turn"'; then break; fi
    if printf '%s' "$status" | grep -q '"last_error":"[^" ]'; then break; fi
    tries=$((tries+1))
    sleep 1
  done
fi
'''
    else:
        script += 'status=$(acp status "$id" --json)\n'
    if args.agent == "codex-session":
        script += '''[ -f "$HOME/.codex-acp/config.toml" ] || exit 40
[ -f "$HOME/.codex-acp/auth.json" ] || exit 41
grep -qx '{"OPENAI_API_KEY":"proxy-managed"}' "$HOME/.codex-acp/auth.json" || exit 42
[ ! -e "$HOME/.codex-acp/state_5.sqlite" ] || exit 43
if find "$HOME/.codex-acp" -maxdepth 1 \\( -name '.config.*' -o -name '.auth.*' \\) | grep -q .; then exit 44; fi
printf '@@HOME_OK@@\\n'
'''
    script += '''printf '@@STATUS@@%s\\n' "$status"
acp stop "$id" || exit 33
wait "$run_pid"
wait_status=$?
[ "$wait_status" -eq 0 ] || [ "$wait_status" -eq 143 ] || exit 39
'''
    if args.live:
        script += f'''acp run {args.agent} > .acp-second.out 2> .acp-second.err &
run_pid=$!
next=
tries=0
while [ -z "$next" ] && [ "$tries" -lt 120 ]; do
  next=$(acp list --wait | awk -v previous="$id" '$1 == "ready" && $2 == "{args.agent}" && $3 != previous {{ print $3; exit }}')
  tries=$((tries+1))
  [ -n "$next" ] || sleep 1
done
[ -n "$next" ] && [ "$next" != "$id" ] || exit 35
acp prompt "$next" 'Reply with exactly PONG and no other text.' || exit 36
tries=0
while [ "$tries" -lt 90 ]; do
  status=$(acp status "$next" --json)
  if printf '%s' "$status" | grep -q '"last_stop_reason":"end_turn"'; then break; fi
  if printf '%s' "$status" | grep -q '"last_error":"[^" ]'; then break; fi
  tries=$((tries+1))
  sleep 1
done
printf '@@REOPEN@@%s\\n' "$status"
acp stop "$next" || exit 37
wait "$run_pid"
wait_status=$?
[ "$wait_status" -eq 0 ] || [ "$wait_status" -eq 143 ] || exit 39
'''
    script += 'exit 0\n'
    state_dir = Path(tempfile.mkdtemp(prefix="real-acp-drive-state-", dir=evidence_path.parent))
    command = [sys.executable, str(Path(__file__).with_name("drive.py")),
               "--marsh", str(marsh),
               "--sbx", str(sbx),
               "--guest-artifacts", str(guest),
               "--state-dir", str(state_dir), "--build-receipt", str(args.build_receipt),
               "--source-tree", str(source), "--source-revision", args.source_revision]
    drive = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                             stderr=subprocess.STDOUT, bufsize=0, start_new_session=True)
    initial = bytearray()
    ready = False
    drive_error = None
    output = ""
    try:
        deadline = time.monotonic() + 600
        while drive.poll() is None and time.monotonic() < deadline:
            readable, _, _ = select.select([drive.stdout], [], [], 1)
            if not readable:
                continue
            chunk = os.read(drive.stdout.fileno(), 65536)
            if not chunk:
                break
            initial.extend(chunk)
            if len(initial) > 8 * 1024 * 1024:
                raise ValueError("drive startup output exceeded bound")
            if b"Kit VMs ready. Opening the test shell." in initial:
                ready = True
                break
        if not ready:
            raise ValueError("drive never reached its shell within the startup deadline")
        rest, _ = drive.communicate(input=script.encode(), timeout=900)
        output = (initial + rest).decode(errors="replace")
    except Exception as error:
        drive_error = str(error)
        if drive.poll() is None:
            os.killpg(drive.pid, signal.SIGTERM)
        try:
            rest, _ = drive.communicate(timeout=90)
        except subprocess.TimeoutExpired:
            os.killpg(drive.pid, signal.SIGKILL)
            rest, _ = drive.communicate(timeout=10)
        output = (initial + rest).decode(errors="replace")
    try:
        remaining = stock_vm_names(sbx)
        cleanup_errors = stock_cleanup_errors(baseline, remaining)
        evidence["stock_after"] = remaining
    except Exception as error:
        cleanup_errors = [f"independent stock cleanup could not be verified: {error}"]
    if list(state_dir.iterdir()):
        cleanup_errors.append(f"drive state retained: {state_dir}")
    else:
        state_dir.rmdir()
    cleaned = not cleanup_errors
    evidence.update(drive_exit=drive.returncode, cleanup_complete=cleaned,
                    cleanup_errors=cleanup_errors, drive_output_tail=output[-4000:])
    if drive_error:
        return finish(1, drive_error)
    if args.expect_auth_required:
        if "@@AUTH_REQUIRED@@" in output and cleaned:
            return finish(0, f"{args.agent}: authentication required; owned scope removed")
        print(output[-4000:], file=sys.stderr)
        return finish(1, "expected authentication failure with complete cleanup")
    match = re.search(r"^@@STATUS@@(.+)$", output, re.MULTILINE)
    first = re.search(r"^@@FIRST@@(.+)$", output, re.MULTILINE)
    reopened = re.search(r"^@@REOPEN@@(.+)$", output, re.MULTILINE)
    if drive.returncode or not match or not cleaned:
        print(output[-4000:], file=sys.stderr)
        return finish(1, "ACP session or scope cleanup failed")
    status = json.loads(match.group(1))
    stderr = bytes(status.get("attachment", {}).get("stderr") or []).decode("utf-8", "replace")
    credential = re.search(r"marsh-auth-mode=(apikey|oauth|none)", stderr)
    evidence.update(
        credential_mode=credential.group(1) if credential else "unobserved",
        job_id=status.get("attachment", {}).get("job_id"),
        stop_reason=status["last_stop_reason"],
        last_error=status["last_error"],
    )
    if status["adapter"] != args.agent or status["last_error"] is not None:
        return finish(1, f"{args.agent}: {status['last_error'] or 'wrong adapter'}")
    if args.live and evidence["credential_mode"] not in ("apikey", "oauth"):
        return finish(1, f"{args.agent}: stock SBX credential mode was not observed")
    if args.agent == "codex-session" and "@@HOME_OK@@" not in output:
        return finish(1, "Codex selected-home auth or SQLite isolation check failed")
    if args.live:
        turns = [json.loads(first.group(1)), status, json.loads(reopened.group(1))] if first and reopened else [status]
        answers = ["".join(item["update"].get("content", {}).get("text", "")
                           for item in turn["updates"]
                           if item["update"].get("sessionUpdate") == "agent_message_chunk").strip()
                   for turn in turns]
        evidence["responses"] = answers
        if len(turns) != 3 or any(turn["last_stop_reason"] != "end_turn" for turn in turns) or answers != ["PONG"] * 3:
            return finish(1, f"{args.agent}: live ACP turns or persisted-home reopen failed: {answers!r}")
    return finish(0, f"{args.agent}: {'three live turns across two sessions' if args.live else 'ACP session started'}; owned scope removed")


if __name__ == "__main__":
    raise SystemExit(main())
