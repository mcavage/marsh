#!/usr/bin/env python3
"""Credential-free stock-SBX UAT of the parent-shell ACP journey."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import re
import select
import signal
import subprocess
import sys
import time
import traceback

from run import IsolatedScopeCleanup, disposable_root, resolve_executable, scoped_control_home, source_identity
from provenance import verify_candidate, host_only_path, stock_vm_names, stock_baseline


SCRIPT = r'''
acp run fixture-session &
run_pid=$!
printf '@@IMMEDIATE_LIST@@\n'
acp list --json
tries=0
until ps --marsh --json > .acp-start-view && grep -q '"starting_kits":\[{' .acp-start-view; do
  tries=$((tries+1))
  [ "$tries" -lt 100 ] || exit 70
  sleep 0.1
done
printf '@@START_VIEW@@\n'
cat .acp-start-view
id=
id=$(acp list --wait --json | sed -n 's/.*"agent_session_id":"\([^"]*\)".*/\1/p')
[ -n "$id" ] || exit 31
printf '@@ID@@%s\n' "$id"
normal=$(ps -o comm= -p $$) || exit 58
absolute=$(/bin/ps -o comm= -p $$) || exit 59
[ "$normal" = "$absolute" ] || exit 60
top -b -n 1 -p $$ > .top-normal || exit 64
/usr/bin/top -b -n 1 -p $$ > .top-absolute || exit 65
head -n 1 .top-normal | grep -q '^top -' || exit 66
head -n 1 .top-absolute | grep -q '^top -' || exit 67
printf '@@PROCESS_PASS_THROUGH@@%s\n' "$normal"
printf '@@PROCESS_VIEW@@\n'
ps --marsh --json || exit 61
printf '@@PROCESS_HUMAN@@\n'
ps --marsh || exit 62
printf '@@TOP_ONCE@@\n'
top --marsh --once || exit 63
top --marsh --verbose --once > .top-verbose-first || exit 77
top --marsh --once --verbose > .top-verbose-last || exit 78
grep -q "$id" .top-verbose-first || exit 79
grep -q "$id" .top-verbose-last || exit 80
printf '@@TOP_VERBOSE_FLAGS@@\n'
n=0
while [ "$n" -lt 8 ]; do
  acp status "$id" --json >/dev/null &
  status_pid=$!
  ps --marsh --json >/dev/null &
  view_pid=$!
  wait "$status_pid" || exit 68
  wait "$view_pid" || exit 69
  n=$((n+1))
done
printf '@@PROCESS_CONCURRENT@@\n'
printf '@@HUMAN_LIST@@\n'
acp list
printf '@@LIST_HELP@@\n'
acp list --help
printf '@@HUMAN_STATUS@@\n'
acp status "$id"
printf '%s\n' "$id" > .acp-uat-id
tries=0
while [ ! -e .acp-uat-peer-done ]; do
  tries=$((tries+1))
  [ "$tries" -lt 600 ] || exit 45
  sleep 0.1
done

key=11111111-1111-4111-8111-111111111111
acp prompt --key "$key" "$id" first || exit 32
printf '@@FIRST_INITIAL@@\n'
acp status "$id" --json
tries=0
until acp status "$id" --json | grep -q '"last_stop_reason":"end_turn"'; do
  tries=$((tries+1))
  if [ "$tries" -ge 100 ]; then
    printf '@@FIRST_TIMEOUT@@\n'
    acp status "$id" --json
    exit 33
  fi
  sleep 0.1
done
printf '@@FIRST@@\n'
acp status "$id" --json
acp prompt --key "$key" "$id" first || exit 48
if acp prompt --key "$key" "$id" changed; then exit 49; fi
printf '@@RETRY@@\n'
acp status "$id" --json

acp prompt "$id" second || exit 34
tries=0
until acp status "$id" --json | grep -q '"turn_active":false.*fixture:second'; do
  tries=$((tries+1))
  [ "$tries" -lt 100 ] || exit 35
  sleep 0.1
done
printf '@@SECOND@@\n'
acp status "$id" --json
acp run stale-ready-probe > .acp-new-failed.out 2>&1 &
failed_pid=$!
wait "$failed_pid"
[ "$?" -ne 0 ] || exit 90
failed_id=$(acp list | awk '$1 == "failed" && $2 == "stale-ready-probe" { print $3 }')
[ -n "$failed_id" ] || exit 91
if acp list --wait "$failed_id" > .acp-newest-wait.out 2>&1; then exit 91; fi
grep -q '^failed *stale-ready-probe' .acp-newest-wait.out || exit 92
acp list --wait "$id" > .acp-id-wait.out || exit 93
grep -q "^ready *fixture-session *$id" .acp-id-wait.out || exit 94
printf '@@WAIT_TARGETED@@\n'

printf '@@ASK@@\n'
acp ask "$id" third || exit 57
printf '@@ASK_DONE@@\n'

acp prompt "$id" hold || exit 36
tries=0
until acp status "$id" --json | grep -q 'fixture:hold'; do
  tries=$((tries+1))
  [ "$tries" -lt 100 ] || exit 37
  sleep 0.1
done
acp cancel "$id" || exit 38
tries=0
until acp status "$id" --json | grep -q '"last_stop_reason":"cancelled"'; do
  tries=$((tries+1))
  [ "$tries" -lt 100 ] || exit 39
  sleep 0.1
done
printf '@@CANCEL@@\n'
acp status "$id" --json

acp prompt "$id" permission || exit 40
request=
tries=0
while [ -z "$request" ] && [ "$tries" -lt 100 ]; do
  request=$(acp permissions "$id" --json | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
  tries=$((tries+1))
  [ -n "$request" ] || sleep 0.1
done
[ -n "$request" ] || exit 41
printf '@@OFFER@@\n'
acp permissions "$id" --json
acp respond "$id" "$request" once || exit 42
tries=0
until acp status "$id" --json | grep -q '"turn_active":false.*fixture:permission:allowed'; do
  tries=$((tries+1))
  [ "$tries" -lt 100 ] || exit 43
  sleep 0.1
done
printf '@@PERMISSION@@\n'
acp status "$id" --json

idle_id=$(acp reserve fixture-session) || exit 96
acp run --reservation "$idle_id" fixture-session > .acp-idle-run &
idle_pid=$!
acp list --wait "$idle_id" || exit 96
[ -n "$idle_id" ] && [ "$idle_id" != "$id" ] || exit 96
acp ask "$idle_id" idle-malformed || exit 97
tries=0
until acp status "$idle_id" --json | grep -q '"terminal":{'; do
  tries=$((tries+1))
  [ "$tries" -lt 300 ] || exit 98
  sleep 0.1
done
printf '@@IDLE_LOST@@\n'
acp status "$idle_id" --json
printf '@@IDLE_LIST@@\n'
acp list
wait "$idle_pid"

acp prompt "$id" malformed || exit 46
tries=0
until acp status "$id" --json | grep -q '"last_error":"[^"]'; do
  tries=$((tries+1))
  [ "$tries" -lt 100 ] || exit 47
  sleep 0.1
done
printf '@@MALFORMED@@\n'
acp status "$id" --json
printf '@@MALFORMED_LIST@@\n'
acp list
tries=0
until acp status "$id" --json | grep -q '"terminal":{'; do
  tries=$((tries+1))
  [ "$tries" -lt 300 ] || exit 95
  sleep 0.1
done
printf '@@AUTO_STOP@@\n'
acp status "$id" --json

printf '@@JOBS@@\n'
jobs -l
acp stop "$id" || exit 44
wait "$run_pid"
wait_status=$?
printf '@@WAIT@@%s\n' "$wait_status"
printf '@@FINAL@@\n'
acp status "$id" --json
printf '@@JOBS_AFTER@@\n'
jobs -l
before_shadow=$(acp list --json)
acp() { printf 'shadowed acp\n'; }
acp run fixture-session > .acp-shadow.out &
wait || exit 71
grep -qx 'shadowed acp' .acp-shadow.out || exit 71
unset -f acp
after_shadow=$(acp list --json)
[ "$before_shadow" = "$after_shadow" ] || exit 72
printf '@@FUNCTION_SHADOW@@\n'
before_alias=$(acp list --json)
shopt -s expand_aliases
alias acp='printf alias-shadow'
acp run fixture-session > .acp-alias.out &
wait || exit 73
unalias acp
grep -q '^alias-shadow' .acp-alias.out || exit 73
after_alias=$(acp list --json)
[ "$before_alias" = "$after_alias" ] || exit 74
printf '@@ALIAS_SHADOW@@\n'
acp run nonexistent-agent > .acp-invalid.out 2>&1 &
invalid_pid=$!
[ -n "$invalid_pid" ] && [ "$invalid_pid" != "$run_pid" ] || exit 75
printf '@@BACKGROUND_CONTINUED@@\n'
wait "$invalid_pid"
[ "$?" -ne 0 ] || exit 76
after_invalid=$(acp list --json)
printf '@@FAILED_LIST@@\n%s\n' "$after_invalid"
printf '@@FAILED_START@@\n'
export MARSH_PLACE=invalid
acp run fixture-session > .acp-invalid-place.out 2>&1 &
orphan_pid=$!
export MARSH_PLACE=local
orphan_id=$(acp list | awk '$1 == "starting" && $2 == "fixture-session" { print $3; exit }')
[ -n "$orphan_id" ] || exit 81
acp stop "$orphan_id" || exit 82
wait "$orphan_pid"
if acp list --wait "$orphan_id" > .acp-failed-wait.out 2>&1; then exit 83; fi
grep -q "failed *fixture-session *$orphan_id" .acp-failed-wait.out || exit 84
printf '@@PENDING_CANCELLED@@%s\n' "$orphan_id"
export MARSH_PLACE=invalid
acp run fixture-session > .acp-expired-place.out 2>&1 &
expired_pid=$!
export MARSH_PLACE=local
expired_id=$(acp list | awk '$1 == "starting" && $2 == "fixture-session" { print $3; exit }')
[ -n "$expired_id" ] && [ "$expired_id" != "$orphan_id" ] || exit 87
wait "$expired_pid"
if acp list --wait "$expired_id" > .acp-expired-wait.out 2>&1; then exit 88; fi
grep -q "failed *fixture-session *$expired_id" .acp-expired-wait.out || exit 89
printf '@@PENDING_EXPIRED@@%s\n' "$expired_id"
readonly MARSH_ACP_RESERVATION_ID=blocked
acp run nonexistent-agent > .acp-readonly.out 2>&1 &
readonly_pid=$!
wait "$readonly_pid"
[ "$?" -ne 0 ] || exit 85
after_readonly=$(acp list --json)
if printf '%s' "$after_readonly" | grep -q '"state":"starting"'; then exit 90; fi
printf '@@READONLY_CONTINUED@@\n'
'''

PROTOCOL_SCRIPT = r'''
agent=fixture-session
id=$(acp reserve "$agent") || exit 101
acp run --reservation "$id" "$agent" > .protocol-run.out 2> .protocol-run.err &
pid=$!
acp list --mine --wait "$id" || exit 102
printf '@@WAIT_AFTER_HISTORY@@\n'
acp list --wait --json || exit 123
acp ask "$id" lossy > .lossy.out
[ "$?" -eq 125 ] || exit 103
printf '@@LOSSY@@\n'
acp status "$id" --json
printf '@@CLEAN@@\n'
acp ask "$id" clean || exit 104
printf '@@STDIN@@\n'
printf '  exact\nbytes\t\n\n' | acp ask "$id" - || exit 105
printf '@@BURST@@\n'
acp ask "$id" burst-400 || exit 106
printf '@@BURST_DONE@@\n'
cursor=$(acp status "$id" --json | sed -n 's/.*"latest_cursor":\([0-9]*\).*/\1/p')
acp prompt "$id" cancel-burst || exit 113
acp cancel "$id" || exit 114
tries=0
until acp status "$id" --json | grep -q '"last_stop_reason":"cancelled"'; do
  tries=$((tries+1)); [ "$tries" -lt 100 ] || exit 115; sleep 0.1
done
tries=0
while true; do
  acp status "$id" "$cursor" --json > .cancel-page || exit 116
  printf '@@CANCEL_PAGE@@\n'; cat .cancel-page
  grep -q '"more_updates":false' .cancel-page && break
  cursor=$(sed -n 's/.*"next_cursor":\([0-9]*\).*/\1/p' .cancel-page)
  tries=$((tries+1)); [ "$tries" -lt 20 ] || exit 117
done
cursor=$(acp status "$id" --json | sed -n 's/.*"latest_cursor":\([0-9]*\).*/\1/p')
acp ask "$id" late || exit 118
tries=0
until acp status "$id" --json | grep -q '"out_of_turn_updates":1'; do
  tries=$((tries+1)); [ "$tries" -lt 100 ] || exit 119; sleep 0.1
done
printf '@@LATE@@\n'
acp status "$id" "$cursor" --json
acp ask "$id" old-retained || exit 120
printf '@@OLD_BEFORE@@\n'
acp status "$id" --json
cursor=$(acp status "$id" --json | sed -n 's/.*"turn_start_cursor":\([0-9]*\).*/\1/p')
acp ask "$id" oversize-update > .oversize-update.out 2> .oversize-update.err
[ "$?" -eq 125 ] || exit 121
grep -q 'updates were lost' .oversize-update.err || exit 122
printf '@@OLD_AFTER_OVERSIZE@@\n'
acp status "$id" "$cursor" --json
cursor=$(acp status "$id" --json | sed -n 's/.*"latest_cursor":\([0-9]*\).*/\1/p')
acp ask "$id" diff || exit 107
printf '@@DIFF@@\n'
acp status "$id" "$cursor" --json
acp ask "$id" always-only > .always.out 2> .always.err
[ "$?" -eq 125 ] || exit 108
grep -q 'allow_once' .always.err || exit 109
head -c 1048577 /dev/zero | tr '\000' x | acp prompt "$id" - > .large.out 2> .large.err
[ "$?" -eq 125 ] || exit 110
grep -q 'exceeds 1 MiB' .large.err || exit 111
acp stop "$id" || exit 112
wait "$pid"
printf '@@PROTOCOL_FINAL@@\n'
acp status "$id" --json
'''

PRIVATE_SCRIPT = r'''
set +o history || exit 51
set -o | grep -Eq '^history[[:space:]]+off$' || exit 52
printf '@@PRIVACY_HISTORY_OFF@@\n'
id=$(acp reserve fixture-session) || exit 54
acp run --reservation "$id" fixture-session > .acp-private-run &
run_pid=$!
acp list --mine --wait "$id" || exit 54
[ -n "$id" ] || exit 54
printf '@@PRIVACY_ID@@%s\n' "$id"
acp prompt "$id" private || exit 55
tries=0
until acp status "$id" --json | grep -q '"last_stop_reason":"end_turn"'; do
  tries=$((tries+1))
  [ "$tries" -lt 100 ] || exit 56
  sleep 0.1
done
acp stop "$id" || exit 57
wait "$run_pid"
wait_status=$?
printf '@@PRIVACY_WAIT@@%s\n' "$wait_status"
printf '@@PRIVACY_FINAL@@\n'
acp status "$id" --json
'''


def section(output: str, marker: str) -> str:
    match = re.search(rf"(?m)^@@{marker}@@\n([^\n]*)", output)
    if match is None:
        raise AssertionError(f"missing {marker} evidence")
    return match.group(1)


def status(output: str, marker: str) -> dict:
    return json.loads(section(output, marker))


def update_texts(document: dict) -> list[str]:
    return [
        item["update"]["content"]["text"]
        for item in document["updates"]
        if item["update"].get("sessionUpdate") == "agent_message_chunk"
    ]


def verify(output: str) -> dict:
    identity = re.search(r"(?m)^@@ID@@([^\n]+)$", output)
    assert identity is not None, "agent session ID was not printed"
    immediate = json.loads(section(output, "IMMEDIATE_LIST"))
    assert any(item["agent_session_id"] == identity.group(1) for item in immediate), (
        "ACP session was absent immediately after the background prompt returned")
    starting = status(output, "START_VIEW")
    assert any(item["command"] == "fixture-acp" for item in starting["starting_kits"]), (
        "cold ACP Kit startup was absent from the scoped process view")
    view = status(output, "PROCESS_VIEW")
    assert view["schema"] == "marsh.process_view/v1"
    assert view["observed_unix_ms"] > 0 and view["daemon_id"]
    assert isinstance(view["starting_kits"], list)
    assert not view["starting_kits"], "Kit startup row remained after job admission"
    assert any(item["agent_session_id"] == identity.group(1) for item in view["acp_sessions"])
    assert any(item["job_id"] in {job["job_id"] for job in view["jobs"]}
               for item in view["acp_sessions"] if item["agent_session_id"] == identity.group(1))
    assert "@@PROCESS_PASS_THROUGH@@" in output
    human = output.split("@@PROCESS_HUMAN@@\n", 1)[1].split("@@TOP_ONCE@@\n", 1)[0]
    assert identity.group(1)[:8] in human and "CPU and memory unavailable" in human
    assert "container=" not in human and "vm=" not in human
    top_once = output.split("@@TOP_ONCE@@\n", 1)[1].split("@@HUMAN_LIST@@\n", 1)[0]
    assert identity.group(1)[:8] in top_once and "CPU and memory unavailable" in top_once
    assert "Refreshing every 2s" not in top_once, "one-shot top claimed it would refresh"
    assert "@@TOP_VERBOSE_FLAGS@@\n" in output
    assert "@@PROCESS_CONCURRENT@@\n" in output
    first = status(output, "FIRST")
    retry = status(output, "RETRY")
    second = status(output, "SECOND")
    cancelled = status(output, "CANCEL")
    permission = status(output, "PERMISSION")
    idle_lost = status(output, "IDLE_LOST")
    malformed = status(output, "MALFORMED")
    auto_stop = status(output, "AUTO_STOP")
    final = status(output, "FINAL")
    offer = json.loads(section(output, "OFFER"))
    assert first["agent_session_id"] == identity.group(1)
    assert first["last_stop_reason"] == "end_turn"
    assert "fixture:first" in update_texts(first)
    assert retry["updates"] == first["updates"], "same-key retry sent another ACP turn"
    assert retry["last_stop_reason"] == first["last_stop_reason"]
    assert "fixture:second" in update_texts(second)
    assert re.search(r"(?m)^ready +fixture-session +" + re.escape(identity.group(1)), output)
    assert "Usage: acp list [--mine] [--wait [ID]] [--json]" in output.split("@@LIST_HELP@@\n", 1)[1]
    assert re.search(r"@@HUMAN_STATUS@@\nfixture-session  " + re.escape(identity.group(1)) + r"  ready", output)
    ask_output = output.split("@@ASK@@\n", 1)[1].split("@@ASK_DONE@@\n", 1)[0]
    assert "fixture:third" in ask_output and "acp prompt key:" not in ask_output
    assert cancelled["last_stop_reason"] == "cancelled"
    assert "fixture:hold" in update_texts(cancelled)
    assert len(offer) == 1
    assert {item["optionId"] for item in offer[0]["options"]} == {"once", "deny"}
    assert permission["last_stop_reason"] == "end_turn"
    assert "fixture:permission:allowed" in update_texts(permission)
    assert permission["permissions"] == []
    assert idle_lost["agent_session_id"] != identity.group(1)
    assert idle_lost["last_error"] == "ACP transport lost"
    assert idle_lost["attachment"]["terminal"] is not None
    idle_list = output.split("@@IDLE_LIST@@\n", 1)[1].split("@@MALFORMED@@\n", 1)[0]
    assert not re.search(r"(?m)^ready +fixture-session +" + re.escape(idle_lost["agent_session_id"]), idle_list)
    assert malformed["turn_active"] is False
    assert malformed["last_stop_reason"] is None
    assert malformed["last_error"], "malformed ACP frame was accepted"
    assert malformed["stopping"] is True, "lost ACP transport was still shown as ready"
    malformed_list = output.split("@@MALFORMED_LIST@@\n", 1)[1].split("@@AUTO_STOP@@\n", 1)[0]
    assert not re.search(r"(?m)^ready +fixture-session +" + re.escape(identity.group(1)), malformed_list)
    assert auto_stop["attachment"]["terminal"] is not None, "broken ACP Kit did not stop without a user command"
    assert "fixture:malformed" in update_texts(malformed)
    assert "fixture:after-malformed" not in update_texts(malformed)
    jobs = output.split("@@JOBS@@\n", 1)[1].split("@@JOBS_AFTER@@\n", 1)[0]
    assert "acp" in jobs, "Brush did not list the ACP background job"
    wait = re.search(r"(?m)^@@WAIT@@([0-9]+)$", output)
    assert wait is not None, "Brush wait result was not printed"
    receipt = final["receipt"]
    assert receipt is not None and receipt["cleanup"] == "verified"
    assert final["attachment"]["job_id"] == receipt["job_id"]
    code = (receipt["exit"] or {}).get("code")
    expected_wait = code if code is not None else 125
    assert int(wait.group(1)) == expected_wait, "Brush wait differs from the Kit receipt"
    assert "@@FUNCTION_SHADOW@@\n" in output
    assert "@@ALIAS_SHADOW@@\n" in output
    assert "@@BACKGROUND_CONTINUED@@\n" in output
    assert "@@WAIT_TARGETED@@\n" in output
    assert "@@FAILED_START@@\n" in output
    failed_rows = [row for row in json.loads(section(output, "FAILED_LIST"))
                   if row["adapter"] == "nonexistent-agent"]
    assert len(failed_rows) == 1 and failed_rows[0]["terminal"] is True
    assert failed_rows[0].get("state") == "failed"
    assert re.search(r"(?m)^@@PENDING_CANCELLED@@[0-9a-f-]{36}$", output)
    assert re.search(r"(?m)^@@PENDING_EXPIRED@@[0-9a-f-]{36}$", output)
    assert "@@READONLY_CONTINUED@@\n" in output
    return {"agent_session_id": identity.group(1), "wait_status": int(wait.group(1))}


def verify_private(output: str) -> dict:
    assert "@@PRIVACY_HISTORY_OFF@@\n" in output
    final = status(output, "PRIVACY_FINAL")
    assert "fixture:private" in update_texts(final)
    receipt = final["receipt"]
    assert receipt is not None and receipt["cleanup"] == "verified"
    wait = re.search(r"(?m)^@@PRIVACY_WAIT@@([0-9]+)$", output)
    assert wait is not None
    code = (receipt["exit"] or {}).get("code")
    assert int(wait.group(1)) == (code if code is not None else 125)
    return {
        "agent_session_id": final["agent_session_id"],
        "shell_session_id": receipt["session_id"],
        "job_id": receipt["job_id"],
        "wait_status": int(wait.group(1)),
    }


class Scope(IsolatedScopeCleanup):
    def __init__(self, args: argparse.Namespace) -> None:
        self.marsh = str(pathlib.Path(args.marsh).resolve(strict=True))
        self.sbx = resolve_executable(args.sbx)
        self.guest_artifacts, self.build_receipt = verify_candidate(args, self.marsh)
        evidence = pathlib.Path(args.evidence).absolute()
        evidence.mkdir(parents=True, mode=0o700, exist_ok=True)
        host_only_path(evidence / "source-binding.json", pathlib.Path(args.source_tree).resolve())
        self.stock_before = stock_baseline(self.sbx)
        self.root = disposable_root("marsh-acp-uat-")
        self.home = self.root / "home"
        self.control_root = self.root / "control"
        self.project = self.root / "project"
        self.home.mkdir()
        self.control_root.mkdir(mode=0o700)
        self.control_home = scoped_control_home(self.control_root, self.home)
        self.project.mkdir()
        kit = pathlib.Path(__file__).with_name("acp-fixture").resolve(strict=True)
        (self.control_home / "commands.json").write_text(
            json.dumps({"fixture-acp": str(kit)}) + "\n", encoding="utf-8"
        )
        (self.control_home / "agents.json").write_text(
            json.dumps([{
                "schema_version": 1,
                "name": "fixture-session",
                "protocol": "acp_v1",
                "command": "fixture-acp",
                "required_capabilities": [],
            }]) + "\n", encoding="utf-8"
        )
        self.environment = os.environ.copy()
        shell_history = self.project / ".marsh-uat-history"
        shell_history.touch(mode=0o600, exist_ok=False)
        self.environment["HISTFILE"] = str(shell_history)
        self.environment["MARSH_HOME"] = str(self.home)
        self.environment["MARSH_CONTROL_HOME"] = str(self.control_root)
        self.environment["MARSH_SBX"] = self.sbx
        self.environment["MARSH_GUEST_ARTIFACTS"] = str(self.guest_artifacts)
        self.initialize_scope_cleanup()

    def cleanup_isolated_scope(self) -> list[str]:
        errors = []
        if self.scope_started:
            try:
                self.public_status()  # include Kits created since the initial snapshot
            except Exception as error:
                errors.append(f"final ownership snapshot unavailable: {error}")
        errors.extend(super().cleanup_isolated_scope())
        try:
            self.stock_after = stock_vm_names(self.sbx)
            errors.extend(self.stock_leftover_errors(self.stock_after))
        except Exception as error:
            errors.append(f"independent stock cleanup inventory unavailable: {error}")
        return errors

    def public_status(self) -> dict:
        self.scope_started = True
        result = subprocess.run(
            [self.marsh, "status", "--json"], cwd=self.project,
            env=self.environment, capture_output=True, timeout=20, check=True, start_new_session=True,
        )
        document = json.loads(result.stdout)
        self.remember_owned_status(document)
        return document

    def top_refresh_interrupt(self) -> dict:
        master, slave = os.openpty()
        watcher = subprocess.Popen(
            [self.marsh, "-c", "top --marsh"], cwd=self.project,
            env=self.environment, stdin=slave, stdout=slave, stderr=slave,
            start_new_session=True,
        )
        os.close(slave)
        output = bytearray()
        try:
            deadline = time.monotonic() + 45
            while output.count(b"marsh activity") < 2 and time.monotonic() < deadline:
                ready, _, _ = select.select([master], [], [], 0.2)
                if ready:
                    try:
                        output.extend(os.read(master, 65536))
                    except OSError:
                        break
                assert watcher.poll() is None, "top watcher exited before a second refresh"
            assert output.count(b"marsh activity") >= 2, "top did not refresh"
            os.killpg(watcher.pid, signal.SIGINT)
            watcher.wait(timeout=20)
            assert watcher.returncode is not None, "top watcher survived interruption"
            return {"refreshes": output.count(b"marsh activity"), "interrupt_exit": watcher.returncode}
        finally:
            if watcher.poll() is None:
                os.killpg(watcher.pid, signal.SIGKILL)
                watcher.wait(timeout=10)
            os.close(master)

    def adversarial_call(self, project: pathlib.Path, command: str) -> dict:
        completed = subprocess.run(
            [self.marsh, "-c", command], cwd=project,
            env=self.environment, capture_output=True, timeout=45, check=False, start_new_session=True,
        )
        return {
            "status": completed.returncode,
            "stdout": completed.stdout.decode(errors="replace"),
            "stderr": completed.stderr.decode(errors="replace"),
        }

    def adversarial_checks(self, process: subprocess.Popen[bytes], evidence: dict) -> None:
        identity_file = self.project / ".acp-uat-id"
        release_file = self.project / ".acp-uat-peer-done"
        deadline = time.monotonic() + 90
        try:
            while not identity_file.is_file():
                assert process.poll() is None, "parent shell exited before publishing ACP ID"
                assert time.monotonic() < deadline, "parent shell did not publish ACP ID"
                time.sleep(0.1)
            identity = identity_file.read_text(encoding="utf-8").strip()
            assert re.fullmatch(r"[0-9a-f-]{36}", identity), "invalid opaque ACP ID"
            same_project_attach = self.adversarial_call(self.project, f"acp attach {identity}")
            evidence["same_project_attach"] = same_project_attach
            assert same_project_attach["status"] != 0
            assert "already has a controller" in same_project_attach["stderr"]
            same_project_prompt = self.adversarial_call(self.project, f"acp prompt {identity} intrude")
            evidence["same_project_prompt"] = same_project_prompt
            assert same_project_prompt["status"] != 0
            assert "controlled by another shell" in same_project_prompt["stderr"]
            remote_id = self.adversarial_call(self.project, "acp status synthetic-acp-session")
            evidence["remote_id"] = remote_id
            assert remote_id["status"] != 0
            assert "not found" in remote_id["stderr"].lower()
            foreign = self.root / "foreign-project"
            foreign.mkdir()
            foreign_id = self.adversarial_call(foreign, f"acp status {identity}")
            evidence["foreign_project_id"] = foreign_id
            assert foreign_id["status"] != 0
            assert "different user, project, or home" in foreign_id["stderr"].lower()
            foreign_view = self.adversarial_call(foreign, "ps --marsh --json")
            evidence["foreign_process_view"] = foreign_view
            assert foreign_view["status"] == 0
            foreign_rows = json.loads(foreign_view["stdout"])
            assert not foreign_rows["jobs"] and not foreign_rows["acp_sessions"]
        finally:
            release_file.touch()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-tree", required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--marsh", required=True)
    parser.add_argument("--sbx", default="sbx")
    parser.add_argument("--guest-artifacts", required=True)
    parser.add_argument("--evidence", required=True)
    parser.add_argument("--build-receipt", type=pathlib.Path,
                        help="optional observed-build receipt; absent records source identity only")
    args = parser.parse_args()
    source = source_identity(pathlib.Path(args.source_tree), args.source_revision)
    evidence = pathlib.Path(args.evidence).resolve()
    evidence.mkdir(parents=True, mode=0o700, exist_ok=True)
    scope = Scope(args)
    result: dict = {"outcome": "failed", "isolated_root": str(scope.root), "source_identity": source,
                    "build_receipt": scope.build_receipt, "stock_before": scope.stock_before}
    try:
        fixture = pathlib.Path(__file__).with_name("acp-fixture")
        files = [
            pathlib.Path(scope.marsh), pathlib.Path(scope.marsh).with_name("marshd"),
            *(scope.guest_artifacts / name for name in (
                "marsh-linux-arm64", "marsh-worker-linux-arm64", "marsh-relay-linux-arm64",
            )),
            *(fixture / name for name in (
                "acp-fixture.yaml", "acp-fixture.dockerfile", "agent.mjs",
            )),
        ]
        result["sha256"] = {
            str(path.resolve(strict=True)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in files
        }
        scope.scope_started = True
        scope.public_status()
        process = subprocess.Popen(
            [scope.marsh, "-c", SCRIPT], cwd=scope.project,
            env=scope.environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
        )
        result["adversarial"] = {}
        try:
            scope.adversarial_checks(process, result["adversarial"])
        finally:
            try:
                stdout, stderr = process.communicate(timeout=420)
            except subprocess.TimeoutExpired:
                process.kill()
                stdout, stderr = process.communicate()
                raise
            result.update({
                "shell_status": process.returncode,
                "stdout": stdout.decode(errors="replace"),
                "stderr": stderr.decode(errors="replace"),
            })
        assert process.returncode == 0, f"parent shell exited {process.returncode}"
        result.update(verify(result["stdout"]))
        protocol = subprocess.run(
            [scope.marsh, "-c", PROTOCOL_SCRIPT], cwd=scope.project, env=scope.environment,
            capture_output=True, timeout=240, check=False, start_new_session=True,
        )
        result["protocol"] = {"status": protocol.returncode, "stdout": protocol.stdout.decode(),
                              "stderr": protocol.stderr.decode(errors="replace")}
        assert protocol.returncode == 0, result["protocol"]
        pout = result["protocol"]["stdout"]
        waited = json.loads(section(pout, "WAIT_AFTER_HISTORY"))
        assert len(waited) == 1 and waited[0]["agent_session_id"] == status(pout, "PROTOCOL_FINAL")["agent_session_id"], waited
        assert status(pout, "LOSSY")["dropped_updates"] == 1
        assert "@@CLEAN@@\nfixture:clean\n" in pout
        assert "@@STDIN@@\nfixture:  exact\nbytes\t\n\n\n" in pout
        burst = pout.split("@@BURST@@\n", 1)[1].split("@@BURST_DONE@@\n", 1)[0]
        assert burst == "fixture:burst-400" + "".join(f"b{n};" for n in range(400)) + "\n", burst
        cancel_pages = [json.loads(line) for line in re.findall(r"(?m)^@@CANCEL_PAGE@@\n([^\n]*)", pout)]
        assert cancel_pages and all(not page["updates_lost"] for page in cancel_pages), cancel_pages
        assert [text for page in cancel_pages for text in update_texts(page)] == ["fixture:cancel-burst", *(f"c{n};" for n in range(200))]
        late = status(pout, "LATE")
        assert late["out_of_turn_updates"] == 1 and update_texts(late) == ["fixture:late"], late
        old_before = status(pout, "OLD_BEFORE")
        old_after = status(pout, "OLD_AFTER_OVERSIZE")
        old_id = old_before["last_turn_id"]
        assert old_after["updates_lost"], "oversized status update did not exercise omission"
        assert old_after["turns"][old_id] == old_before["turns"][old_id], "later omission corrupted old receipt"
        assert old_after["turns"][old_id]["dropped_updates"] == 0
        assert old_after["turns"][old_id]["retained_after"] == 0
        assert [u["update"]["content"]["text"] for u in old_after["updates"] if u["turn_id"] == old_id] == ["fixture:old-retained"]
        diff = status(pout, "DIFF")
        assert not diff["updates_lost"], diff
        assert diff["updates"][1]["update"]["content"][0]["newText"] == "new"
        assert diff["updates"][1]["update"]["locations"][0]["line"] == 3
        assert diff["updates"][1]["update"]["_meta"] == {"fixture": "metadata"}
        assert diff["updates"][2]["update"]["availableCommands"][0]["name"] == "review"
        scope.public_status()
        private = subprocess.run(
            [scope.marsh, "-c", PRIVATE_SCRIPT], cwd=scope.project,
            env=scope.environment, capture_output=True, timeout=240, check=False, start_new_session=True,
        )
        result["private_shell"] = {
            "status": private.returncode,
            "stdout": private.stdout.decode(errors="replace"),
            "stderr": private.stderr.decode(errors="replace"),
        }
        assert private.returncode == 0, f"private parent shell exited {private.returncode}"
        private_ids = verify_private(result["private_shell"]["stdout"])
        result["private_shell"].update(private_ids)
        result["final_status"] = scope.public_status()
        result["top_refresh_interrupt"] = scope.top_refresh_interrupt()
        result["outcome"] = "passed"
    except Exception:
        result["error"] = traceback.format_exc()
        try:
            scope.public_status()
        except Exception:
            pass
    finally:
        cleanup = scope.cleanup_isolated_scope()
        result["stock_after"] = getattr(scope, "stock_after", {})
        if cleanup:
            result["outcome"] = "failed"
            result["cleanup_errors"] = cleanup
        (evidence / "acp-uat.json").write_text(
            json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    print(f"ACP UAT: {result['outcome']}; evidence: {evidence / 'acp-uat.json'}")
    return 0 if result["outcome"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
