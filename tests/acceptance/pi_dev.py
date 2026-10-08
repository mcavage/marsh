#!/usr/bin/env python3
"""E2E 2 for docs/design/self-development.md: Pi (`pi-dev`) develops marsh from marsh --dev.

From the Mac: open `marsh --dev` in this checkout and run `pi-dev -p` (one real,
billed model turn) asking it to build the inner candidate and run `fixture
identity` through it. Pi runs in the dev shell VM, so its bash tool must run
there: the tool result has to carry a per-run nonce together with the dev
shell VM's own name, the inner build must land in the scratch artifacts, and
the inner run must leave receipts in the scratch control home. Model access is
stock SBX's proxy-managed credential; no host agent profile is read. After exit: no child VM, grant or disposable scratch remains, Pi's state
stays in S/cache, and pre-existing VMs are unchanged.
"""
from __future__ import annotations

import argparse
import json
import pathlib
import secrets
import shlex
import subprocess
import sys
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from self_dev import REPO, stage, stock, warm_dev_shell  # noqa: E402


def session_script(kit: str, nonce: str) -> str:
    tool = (f'echo "PIMARK-{nonce} vm=$SANDBOX_NAME prefix=$MARSH_VM_PREFIX"; '
            f'DEV_KIT={shlex.quote(kit)} make dev-inner >"$MARSH_DEV_SCRATCH/tmp/build.log" 2>&1 '
            '|| { tail -20 "$MARSH_DEV_SCRATCH/tmp/build.log"; exit 1; }; '
            "make --no-print-directory dev-inner-run CMD='fixture identity'")
    prompt = ("Use your bash tool to run exactly this command once, unchanged (it takes several minutes):\n\n"
              f"{tool}\n\n"
              "Then reply with only the first output line that starts with {\"cwd\": (or the error, if it failed).")
    return "\n".join([
        "set -u",
        'echo "DEV PREFIX=$MARSH_VM_PREFIX SCRATCH=$MARSH_DEV_SCRATCH"',
        f"pi-dev -p --mode json {shlex.quote(prompt)} > \"$MARSH_DEV_SCRATCH/tmp/pi-dev.jsonl\"; echo \"PI_EXIT=$?\"",
        'while IFS= read -r line; do echo "PIEVENT $line"; done < "$MARSH_DEV_SCRATCH/tmp/pi-dev.jsonl"',
        'echo "RECEIPTS=$(find "$MARSH_DEV_SCRATCH/control" -type f | wc -l)"',
        'sbx ls --json > "$MARSH_DEV_SCRATCH/tmp/ls.json"; echo "LS=$(tr -d "\\n" < "$MARSH_DEV_SCRATCH/tmp/ls.json")"',
        "exit 0",
    ])


def text_of(message: dict) -> str:
    content = message.get("content")
    if isinstance(content, str):
        return content
    return "".join(part.get("text", "") for part in content or [] if isinstance(part, dict))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prefix", default=str(pathlib.Path.home() / ".marsh-dev"),
                        help="dev product installed by make dev (DEV_PREFIX)")
    parser.add_argument("--sbx", required=True)
    parser.add_argument("--kit", required=True)
    parser.add_argument("--evidence", type=pathlib.Path)
    args = parser.parse_args()

    before = stock(args.sbx)
    marsh, env, control = stage(args.prefix, args.sbx, "marsh-pidev-")
    shell_vm, _ = warm_dev_shell(marsh, args.sbx, env, control, None)
    nonce = secrets.token_hex(6)
    started = time.time()
    session = subprocess.run([marsh, "--dev", "-c", session_script(args.kit, nonce)], cwd=REPO, env=env,
                             capture_output=True, text=True, timeout=3600, stdin=subprocess.DEVNULL)
    elapsed = time.time() - started
    out = session.stdout + session.stderr
    marks: dict[str, str] = {}
    events: list[dict] = []
    for line in out.splitlines():
        if line.startswith("PIEVENT "):
            try:
                events.append(json.loads(line[len("PIEVENT "):]))
            except json.JSONDecodeError:
                pass
            continue
        for key in ("PI_EXIT", "RECEIPTS", "LS"):
            if line.startswith(key + "="):
                marks[key] = line.split("=", 1)[1]
        if line.startswith("DEV PREFIX="):
            marks["PREFIX"] = line.split()[1].split("=", 1)[1]
            marks["SCRATCH"] = line.split()[2].split("=", 1)[1]
    print("\n".join(line for line in out.splitlines() if not line.startswith("PIEVENT ")))
    prefix, scratch = marks.get("PREFIX", ""), pathlib.Path(marks.get("SCRATCH", "/nonexistent"))

    bash_results = [json.dumps(event.get("result")) for event in events
                    if event.get("type") == "tool_execution_end" and event.get("toolName") == "bash"]
    marked = [result for result in bash_results if f"PIMARK-{nonce} vm={shell_vm} prefix={prefix}" in result]
    finals = [text_of(event["message"]) for event in events
              if event.get("type") == "message_end" and isinstance(event.get("message"), dict)
              and event["message"].get("role") == "assistant"]
    final = finals[-1] if finals else ""
    models = sorted({f"{event['message'].get('provider')}/{event['message'].get('model')}" for event in events
                     if event.get("type") == "message_end" and isinstance(event.get("message"), dict)
                     and event["message"].get("role") == "assistant"})
    checks: dict[str, bool] = {}
    checks["session_exit_0"] = session.returncode == 0
    checks["pi_exit_0"] = marks.get("PI_EXIT") == "0"
    checks["pi_bash_tool_ran_in_dev_shell_vm"] = bool(marked) and shell_vm != "unknown"
    checks["tool_ran_inner_fixture_identity"] = any('{\\"cwd\\":' in result and str(REPO) in result
                                                    for result in marked)
    checks["model_reported_identity"] = '{"cwd":' in final and str(REPO) in final
    checks["inner_build_in_scratch_artifacts"] = (scratch / "artifacts/bin/marsh").is_file() and (
        scratch / "artifacts/bin/marsh").stat().st_mtime >= started
    checks["receipt_in_scratch_control"] = int(marks.get("RECEIPTS", "0") or 0) > 0
    try:
        rows = json.loads(marks.get("LS", "{}")).get("sandboxes", [])
    except json.JSONDecodeError:
        rows = None
    checks["child_vms_have_grant_prefix"] = bool(rows) and bool(prefix) and all(
        row["name"].startswith(prefix) for row in rows)
    agent_dir = scratch / "cache/pi-dev/agent"
    checks["pi_state_in_scratch_cache"] = (agent_dir / "settings.json").is_file() and any(
        (agent_dir / "sessions").rglob("*.jsonl"))
    after = stock(args.sbx)
    checks["no_prefixed_vm_after_exit"] = bool(prefix) and not [n for n in after if n.startswith(prefix)]
    ownership = list(control.glob("*/vm-ownership.json"))
    checks["no_grant_record"] = not (json.loads(ownership[0].read_text()).get("grants", {}) if ownership else {})
    checks["disposable_scratch_removed"] = scratch.is_dir() and not any(
        (scratch / leaf).exists() for leaf in ("home", "control", "tmp"))
    checks["baseline_unchanged"] = all(after.get(name, {}).get("id") == row["id"] for name, row in before.items())
    stop = subprocess.run([marsh, "stop"], cwd=REPO, env=env, capture_output=True, text=True, timeout=600)
    leftovers = sorted(set(stock(args.sbx)) - set(before))
    checks["scope_stop_leaves_no_new_vm"] = stop.returncode == 0 and not leftovers
    report = {"schema": "marsh.pi-dev-e2e/v1", "passed": all(checks.values()), "checks": checks,
              "shell_vm": shell_vm, "prefix": prefix, "final": final[-2000:], "models": models, "events": len(events),
              "bash_tool_calls": len(bash_results), "elapsed_s": round(elapsed, 1), "leftovers": leftovers,
              "scope_stop": stop.stdout[-2000:] + stop.stderr[-2000:], "root": str(control.parent)}
    if args.evidence:
        args.evidence.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
