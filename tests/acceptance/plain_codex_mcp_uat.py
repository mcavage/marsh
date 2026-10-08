#!/usr/bin/env python3
"""Real Codex Kit journey: publish into an already warm VM, then call the tool."""

import argparse
import json
from pathlib import Path
import shlex
import traceback

from mcp_gateway_uat import Scope, sha256
from run import source_identity
from provenance import host_only_path


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-receipt", type=Path, required=True)
    parser.add_argument("--marsh", required=True)
    parser.add_argument("--sbx", required=True)
    parser.add_argument("--guest-artifacts", required=True)
    parser.add_argument("--source-tree", required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--evidence", required=True)
    args = parser.parse_args()
    identity = source_identity(Path(args.source_tree), args.source_revision)
    evidence = Path(args.evidence).resolve()
    host_only_path(evidence / "plain-codex-mcp-uat.json", Path(args.source_tree)).unlink(missing_ok=True)
    scope = Scope(args)
    report = {
        "schema": "marsh.plain-codex-mcp-uat/v1",
        "source_identity": identity, "verified_build_receipt": scope.build_receipt,
        "stock_before": scope.stock_before,
        "marsh_sha256": sha256(scope.marsh),
        "outcome": "failed",
    }
    try:
        scope.scope_started = True
        scope.status()
        ready = scope.command("--load", "codex", "-c", "true", timeout=300)
        assert ready.returncode == 0, ready.stderr.decode(errors="replace")
        workers = [worker for worker in scope.status()["workers"]
                   if "/marsh-codex@" in worker.get("kit_profile", "")]
        assert len(workers) == 1 and workers[0]["warm"], workers
        scope.sandbox = workers[0]["vm_id"]
        report["warm_sandbox"] = scope.sandbox
        tool = f"codexhot_{scope.tag}"
        marker = scope.project / ".codex-mcp-called"
        pipeline = "cat | tr a-z A-Z | tee .codex-mcp-called"
        publish = scope.command(
            "-c", f"mcp publish {tool} --kit codex -- {shlex.quote(pipeline)}",
            timeout=240,
        )
        assert publish.returncode == 0, publish.stderr.decode(errors="replace")
        assert f"Loaded into sandbox: {scope.sandbox}".encode() in publish.stdout
        scope.published.add(tool)
        discovered = scope.gateway_ready(tool, "foo")
        assert tool in discovered["tools"] and marker.read_text() == "FOO"
        marker.unlink()
        prompt = (f"Call the mcp-gateway tool {tool} with input exactly foo. "
                  "Use the MCP tool, not a shell command. Reply with only its uppercase output.")
        model = scope.command(
            "-c", f"codex exec --skip-git-repo-check {shlex.quote(prompt)}",
            timeout=420,
        )
        report["codex_exit"] = model.returncode
        report["codex_stdout"] = model.stdout.decode(errors="replace")[-12000:]
        report["codex_stderr"] = model.stderr.decode(errors="replace")[-4000:]
        assert model.returncode == 0, report["codex_stderr"]
        assert marker.read_text() == "FOO", "plain Codex did not call the loaded MCP tool"
        assert "FOO" in report["codex_stdout"], "plain Codex omitted the tool result"
        report["outcome"] = "passed"
    except Exception:
        report["failure"] = traceback.format_exc()
    finally:
        report["cleanup_errors"] = scope.cleanup()
        report["stock_after"] = scope.stock_after
        if report["cleanup_errors"]:
            report["outcome"] = "failed"
        (evidence / "plain-codex-mcp-uat.json").write_text(json.dumps(report, indent=2) + "\n")
        (evidence / "plain-codex-mcp-uat.json").chmod(0o600)
    print(json.dumps({"outcome": report["outcome"], "evidence": str(evidence / "plain-codex-mcp-uat.json")}))
    return 0 if report["outcome"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
