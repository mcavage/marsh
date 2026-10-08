# Stock Gateway MCP UAT

`mcp_gateway_uat.py` exercises published Brush pipelines through the stock SBX
MCP Gateway. It is an opt-in macOS test and requires an authenticated stock
`sbx`, a host release directory containing `marsh`, `marshd`, and `marsh-mcp`,
and assembled guest artifacts for that same candidate.

```sh
python3 tests/acceptance/mcp_gateway_uat.py \
  --source-tree "$PWD" \
  --source-revision "$(git rev-parse HEAD)" \
  --marsh /absolute/path/to/release/marsh \
  --guest-artifacts /absolute/path/to/guest-artifacts \
  --real-home \
  --evidence /private/tmp/marsh-mcp-gateway-evidence
```

The harness creates a unique project, selected home, and agent sandbox. It
publishes the first tool from an already attached shell, calls through a second
sandbox's Gateway, and unpublishes from that same shell. It then verifies
that two disposable-home shells cannot publish or unpublish, that both finish
without a teardown failure, and that the
original publishing shell and its persistent HOME remain usable. It also verifies
binary output and text-state honesty, `pipefail` exit status, the 64 KiB stdin
limit, the 256 KiB stdout capture limit, cancellation, and that first discovery
and invocation occur on the original sandbox boot. The final boot ID is
recorded separately because stock SBX may restart the sandbox during the longer
edge suite.

`published_codex_uat.py` additionally keeps an old MCP exporter and sandbox
load across unpublish and an identical republish. Old calls remain denied,
including a fresh exporter launched with the old registration arguments;
a new exporter and a second sandbox load can call the fresh publication.
Each publish has a new generation even when the pipeline text is unchanged.

`--exercise-restart` checks that a publication keeps working after restarting
only this isolated daemon. `--core-only` is accepted for compatibility and has
no effect.
The harness requires a clean source tree at the exact 40-character revision.
The report records that source identity, host binary and three Linux guest
artifact hashes, the harness hash, results, selected scope, and cleanup status.
The shell and sandbox are removed only when owned by this
run. If cleanup fails, the isolated root is retained for manual inspection.

The login `HOME` is used for stock SBX authentication, while `MARSH_HOME` and
the project stay disposable. A successful Brush pipeline may still
report `cleanup_certainty=uncertain`, since a host process exit does not verify
the nested shell VM's cleanup. The test records that outcome without upgrading
it to verified. It separately requires `host_cleanup_certainty=verified` for
the completed host process group. Synthetic input contains no credentials.
After a normally completed host child is reaped, its last process-group
check has a narrow PGID reuse race before any follow-up signal. The host
reports nested pipeline cleanup as uncertain; this UAT does not prove the
absence of that kernel identity race.

Gateway cancellation depends on the stock Gateway forwarding the MCP
`notifications/cancelled` request. The stock Gateway may replace the tool's
structured cancellation result with a JSON-RPC `context canceled` error. The
full harness checks that a delayed project marker does not appear after
`sleep 15; printf late`, which checks that the whole script stops. A
`--cancel-only` diagnostic uses `sleep 15 && printf late` to isolate
interruption of the foreground command; `--cancel-semicolon` selects the
whole-script case and `--cancel-after-start` waits for an observable start
marker before sending cancellation. When the Gateway masks the result,
`cleanup_certainty`
is **not observable from the Gateway**; the report must never upgrade it to
verified. Direct MCP protocol tests separately assert the server's structured
`outcome=cancelled` and `output_complete=false`. Neither boundary proves nested
VM cleanup. The marker is delayed beyond the server's bounded SIGINT/SIGTERM/
SIGKILL process-group escalation window to avoid a timing race at its edge.
The harness watches for 30 seconds after the call starts. The dedicated
`--cancel-only` diagnostic also confirms the same sandbox boot ID throughout
that short cancellation case.
