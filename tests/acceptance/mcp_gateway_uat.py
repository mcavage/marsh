#!/usr/bin/env python3
"""Disposable stock Gateway UAT for published MCP pipelines."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import pathlib
import subprocess
import sys
import time
import traceback

from harness import sha256, wait_until
from mcp_harness import McpScope, gateway_cancel, gateway_data
from provenance import host_only_path, source_identity, verify_candidate


class CoreJourneyComplete(Exception):
    """Internal control flow after a successful core-only journey."""


def exercise_gateway_cancel(scope: McpScope, results: dict, *, sequence_after_interrupt: bool = False, cancel_after_start: bool = False) -> None:
    cancel_tool = scope.tools[7]
    late_marker = scope.project / ".uat-late-marker"
    pipeline = "sleep 15; printf late > .uat-late-marker" if sequence_after_interrupt else "sleep 15 && printf late > .uat-late-marker"
    start_marker = scope.project / ".uat-started" if cancel_after_start else None
    if start_marker is not None:
        pipeline = "printf started > .uat-started; " + pipeline
    scope.publish_pipeline(cancel_tool, pipeline)
    wait_until(lambda: cancel_tool in scope.gateway(cancel_tool, "", list_only=True)["tools"], f"Gateway discovery of {cancel_tool}", 20)
    probe_started = time.monotonic()
    cancellation = gateway_cancel(scope.sbx, scope.sandbox, cancel_tool, cwd=scope.root, env=scope.environment, start_marker=start_marker)
    if start_marker is not None:
        assert cancellation["started_marker_seen"], "published pipeline did not start before cancellation"
    assert not cancellation.get("cancel_error") and not cancellation["call_pending"], cancellation
    call = cancellation.get("call")
    assert call is not None, cancellation
    if "result" in call:
        cancel_result = call["result"]
        assert cancel_result.get("isError") is True, cancel_result
        cancel_data = cancel_result["structuredContent"]["data"]
        assert cancel_data["outcome"] == "cancelled" and cancel_data["output_complete"] is False, cancel_data
        assert cancel_data["cleanup_certainty"] == "uncertain", cancel_data
        cancellation_result = {"gateway_response":"structured_cancelled", "cleanup_certainty":cancel_data["cleanup_certainty"]}
    else:
        error = call.get("error", {})
        assert "cancel" in error.get("message", "").lower(), call
        cancellation_result = {"gateway_response":"jsonrpc_context_canceled", "error_code":error.get("code"), "cleanup_certainty":"not_exposed_by_gateway"}
    cancellation_result["request_id"] = call.get("id")
    cancellation_result["started_marker_seen"] = cancellation["started_marker_seen"]
    cancellation_result["pipeline"] = pipeline
    cancellation_result["probe_elapsed_seconds"] = cancellation["elapsed_seconds"]
    results["cancellation"] = cancellation_result
    marker_seen_at = None
    while time.monotonic() - probe_started < 30:
        if late_marker.exists() and marker_seen_at is None:
            marker_seen_at = round(time.monotonic() - probe_started, 3)
        time.sleep(0.1)
    cancellation_result["marker_observation_seconds"] = round(time.monotonic() - probe_started, 3)
    assert cancellation_result["marker_observation_seconds"] >= 15, "cancel probe did not outwait the late marker"
    cancellation_result["late_marker_elapsed_seconds"] = marker_seen_at
    cancellation_result["late_marker_absent"] = not late_marker.exists()
    assert cancellation_result["late_marker_absent"], "cancelled pipeline continued to its late marker"


def exercise_gateway_edges(scope: McpScope, results: dict) -> None:
    """Qualify fixed Brush pipelines through the stock Gateway in one live sandbox."""

    binary_tool = scope.tools[3]
    scope.publish_pipeline(binary_tool, r"printf '\000\377\200A'")
    binary = scope.gateway_ready(binary_tool, "")
    binary_data = gateway_data(binary, binary_tool, b"\x00\xff\x80A")
    assert binary_data["stdout_text_state"] == "lossy", binary_data
    assert binary_data["output_complete"] is True and not binary_data["stdout_truncated"], binary_data
    results["binary_output"] = {"stdout_base64":binary_data["stdout_base64"], "text_state":binary_data["stdout_text_state"], "output_complete":binary_data["output_complete"]}

    pipefail_tool = scope.tools[4]
    scope.publish_pipeline(pipefail_tool, "set -o pipefail; false | cat")
    pipefail = scope.gateway_ready(pipefail_tool, "")
    assert pipefail_tool in pipefail["tools"], pipefail
    pipefail_result = pipefail["call"]["result"]
    assert pipefail_result.get("isError") is True, pipefail_result
    pipefail_data = pipefail_result["structuredContent"]["data"]
    assert pipefail_data["outcome"] == "exit_nonzero" and pipefail_data["exit_code"] == 1, pipefail_data
    assert base64.b64decode(pipefail_data["stdout_base64"]) == b"", pipefail_data
    assert pipefail_data["output_complete"] is True, pipefail_data
    results["pipefail"] = {"outcome":pipefail_data["outcome"], "exit_code":pipefail_data["exit_code"], "output_complete":pipefail_data["output_complete"]}

    input_tool = scope.tools[5]
    scope.publish_pipeline(input_tool, "cat")
    accepted = scope.gateway_ready(input_tool, "x" * 65_536)
    accepted_data = gateway_data(accepted, input_tool, b"x" * 65_536)
    assert accepted_data["output_complete"] is True, accepted_data
    rejected = scope.gateway(input_tool, "x" * 65_537)
    rejected_result = rejected["call"]["result"]
    assert rejected_result.get("isError") is True, rejected_result
    rejected_body = rejected_result["structuredContent"]
    assert rejected_body["ok"] is False and "maximum limit" in rejected_body["error"], rejected_body
    results["input_bound"] = {"accepted_bytes":65_536, "accepted_stdout_sha256":hashlib.sha256(b"x" * 65_536).hexdigest(), "rejected_bytes":65_537, "rejection":rejected_body["error"]}

    output_tool = scope.tools[6]
    scope.publish_pipeline(output_tool, "head -c 262200 /dev/zero")
    oversized = scope.gateway_ready(output_tool, "")
    oversized_data = gateway_data(oversized, output_tool, b"\0" * 262_144)
    assert oversized_data["stdout_truncated"] is True and oversized_data["output_complete"] is False, oversized_data
    results["output_bound"] = {"captured_bytes":len(base64.b64decode(oversized_data["stdout_base64"])), "stdout_truncated":oversized_data["stdout_truncated"], "output_complete":oversized_data["output_complete"]}

    exercise_gateway_cancel(scope, results, sequence_after_interrupt=True, cancel_after_start=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-receipt", type=pathlib.Path,
                        help="optional observed-build receipt; absent records source identity only")
    parser.add_argument("--source-tree", required=True)
    parser.add_argument("--source-revision", required=True, help="exact 40-character clean Git HEAD used for this candidate")
    parser.add_argument("--marsh", required=True)
    parser.add_argument("--sbx", default="sbx")
    parser.add_argument("--guest-artifacts", required=True)
    parser.add_argument("--evidence", required=True)
    parser.add_argument("--commands", type=pathlib.Path,
                        help="optional isolated commands.json overlay (e.g. staged Kit sources or OCI digests)")
    parser.add_argument("--exercise-restart", action="store_true")
    parser.add_argument("--cancel-only", action="store_true", help="diagnose one stock Gateway cancellation")
    parser.add_argument("--cancel-semicolon", action="store_true", help="with --cancel-only, diagnose whether Brush executes a following command after interrupted sleep")
    parser.add_argument("--cancel-after-start", action="store_true", help="with --cancel-only, wait for a pipeline start marker before sending cancellation")
    args = parser.parse_args()
    if (args.cancel_semicolon or args.cancel_after_start) and not args.cancel_only:
        parser.error("cancel diagnostic options require --cancel-only")
    verify_candidate(args, str(pathlib.Path(args.marsh).resolve(strict=True)))
    if sys.platform != "darwin":
        parser.error("stock Gateway UAT requires macOS")
    source_tree = pathlib.Path(args.source_tree).resolve(strict=True)

    try:
        identity = source_identity(source_tree, args.source_revision)
    except (ValueError, subprocess.CalledProcessError) as error:
        parser.error(str(error))
    guest_artifacts = pathlib.Path(args.guest_artifacts).resolve(strict=True)
    guest_sha256 = {
        str(path): sha256(path)
        for path in (
            (guest_artifacts / name).resolve(strict=True)
            for name in ("marsh-linux-arm64", "marsh-worker-linux-arm64", "marsh-relay-linux-arm64")
        )
    }
    evidence = pathlib.Path(args.evidence).resolve()
    report_path = evidence / "mcp-gateway-uat.json"
    host_only_path(report_path, source_tree).unlink(missing_ok=True)
    scope = McpScope(args)
    report: dict = {"schema":"marsh.mcp-gateway-uat/v1", "mode":"cancel-only" if args.cancel_only else "core", "outcome":"failed", "isolated_root":str(scope.root), "selected_home":str(scope.home), "host_home_mode":"real", "sandbox":scope.sandbox, "tool_names":scope.tools, "source_identity":identity, "binary_sha256":{str(path):sha256(path) for path in (scope.marsh,scope.marshd,scope.mcp)}, "guest_artifact_sha256":guest_sha256, "harness_sha256":sha256(pathlib.Path(__file__))}
    report["verified_build_receipt"] = scope.build_receipt
    report["stock_before"] = scope.stock_before
    try:
        scope.scope_started = True
        report["scope_status"] = scope.status()
        scope.create_sandbox()
        report["sandbox_boot_id_before"] = scope.sandbox_boot_id()
        if args.cancel_only:
            report["gateway_edges"] = {}
            exercise_gateway_cancel(scope, report["gateway_edges"], sequence_after_interrupt=args.cancel_semicolon, cancel_after_start=args.cancel_after_start)
            report["sandbox_boot_id_after"] = scope.sandbox_boot_id()
            assert report["sandbox_boot_id_after"] == report["sandbox_boot_id_before"], "Gateway sandbox restarted during cancellation"
            report["outcome"] = "passed_partial"
            report["qualification"] = "cancellation diagnostic only"
            raise CoreJourneyComplete
        scope.start_publishing_shell(evidence)
        publisher_shells = [shell for shell in scope.status()["shells"] if shell["pid"] == scope.child.pid]
        assert len(publisher_shells) == 1, f"cannot identify attached publishing shell: {publisher_shells}"
        report["publisher_session_id"] = publisher_shells[0]["session_id"]
        first_input = f"mcp_capture_{scope.tag}\n"
        first = scope.gateway_ready(scope.tools[0], first_input)
        first_data = gateway_data(first, scope.tools[0], first_input.upper().encode())
        assert first_data["cleanup_certainty"] == "uncertain", first_data
        assert first_data["host_cleanup_certainty"] == "verified", first_data
        assert any("sandbox_cleanup=uncertain" in block.get("text", "")
                   and "host_process_group_cleanup=verified" in block.get("text", "")
                   for block in first["call"]["result"]["content"]), first["call"]
        report["sandbox_boot_id_at_first_call"] = scope.sandbox_boot_id()
        assert report["sandbox_boot_id_at_first_call"] == report["sandbox_boot_id_before"], "Gateway discovery required a sandbox restart"
        report["core_gateway"] = {"tools":first["tools"],"stdout_base64":first_data["stdout_base64"],"outcome":first_data["outcome"],"exit_code":first_data["exit_code"],"output_complete":first_data["output_complete"],"cleanup_certainty":first_data["cleanup_certainty"],"host_cleanup_certainty":first_data["host_cleanup_certainty"]}
        invalid_name = scope.command("-c", f"mcp publish '-bad' --sandbox {scope.sandbox} -- 'cat'")
        assert invalid_name.returncode != 0 and b"published tool name" in invalid_name.stderr, invalid_name.stderr
        invalid_host_name = scope.command("mcp", "publish", "-bad", "--", "cat")
        assert invalid_host_name.returncode != 0 and b"published tool name" in invalid_host_name.stderr, invalid_host_name.stderr
        assert scope.registration_absent("-bad"), "invalid tool name reached host registration"
        assert scope.child.poll() is None, "publishing shell exited after invalid tool name"
        report["leading_dash_name_rejected"] = True
        ephemeral = scope.command(
            "--ephemeral-home", "-c",
            f"mcp publish {scope.tools[0]} --sandbox {scope.sandbox} -- 'cat | tr a-z A-Z'",
        )
        assert ephemeral.returncode != 0 and b"requires a persistent home" in ephemeral.stderr, ephemeral.stderr
        assert b"teardown was not confirmed" not in ephemeral.stderr, ephemeral.stderr
        assert scope.child.poll() is None, "publishing shell exited during ephemeral publish rejection"
        home_check = scope.command("-c", 'test -d "$HOME" && printf persistent-home-ok')
        assert home_check.returncode == 0 and home_check.stdout == b"persistent-home-ok", home_check.stderr
        report["ephemeral_publish_rejected"] = True
        ephemeral_unpublish = scope.command("--ephemeral-home", "-c", f"mcp unpublish {scope.tools[0]}")
        assert ephemeral_unpublish.returncode != 0 and b"requires a persistent home" in ephemeral_unpublish.stderr, ephemeral_unpublish.stderr
        assert b"teardown was not confirmed" not in ephemeral_unpublish.stderr, ephemeral_unpublish.stderr
        assert scope.child.poll() is None, "publishing shell exited during ephemeral unpublish rejection"
        home_check = scope.command("-c", 'test -d "$HOME" && printf persistent-home-ok')
        assert home_check.returncode == 0 and home_check.stdout == b"persistent-home-ok", home_check.stderr
        report["ephemeral_unpublish_rejected"] = True
        report["publisher_home_survived_ephemeral_shells"] = True
        scope.advance_shell(".uat-private")
        scope.advance_shell("")
        scope.published.remove(scope.tools[0])
        assert scope.registration_absent(scope.tools[0]), "attached-shell unpublish left stock registration"
        report["attached_unpublish_registration_absent"] = True
        if args.exercise_restart:
            scope.publish_pipeline(scope.tools[2], "cat | tr a-z A-Z")
            before_input = f"mcp_before_restart_{scope.tag}\n"
            gateway_data(scope.gateway_ready(scope.tools[2], before_input), scope.tools[2], before_input.upper().encode())
            scope.restart_daemon()
            after_input = f"mcp_after_restart_{scope.tag}\n"
            gateway_data(scope.gateway_ready(scope.tools[2], after_input), scope.tools[2], after_input.upper().encode())
            report["publication_survives_daemon_restart"] = True
        report["gateway_edges"] = {}
        exercise_gateway_edges(scope, report["gateway_edges"])
        report["sandbox_boot_id_after"] = scope.sandbox_boot_id()
        report["outcome"] = "passed"
    except CoreJourneyComplete:
        pass
    except Exception:
        report["failure"] = traceback.format_exc()
    finally:
        report["cleanup_errors"] = scope.cleanup()
        report["stock_after"] = scope.stock_after
        if report["cleanup_errors"]:
            report["outcome"] = "failed"
        report_path.write_text(json.dumps(report,indent=2,sort_keys=True)+"\n",encoding="utf-8")
        report_path.chmod(0o600)
    print(json.dumps({"outcome":report["outcome"],"evidence":str(report_path)}))
    return 0 if report["outcome"] == "passed" else 1

if __name__ == "__main__":
    raise SystemExit(main())
