# ACP stock-SBX acceptance

`acp_uat.py` runs a credential-free native Kit v3 agent in a disposable selected
home and project. Use host binaries and Linux guest artifacts assembled from the
same source candidate:

```sh
python3 tests/acceptance/acp_uat.py \
  --source-tree "$PWD" \
  --source-revision "$(git rev-parse HEAD)" \
  --marsh /absolute/path/to/marsh \
  --sbx /absolute/path/to/sbx \
  --guest-artifacts /absolute/path/to/libexec/marsh \
  --build-receipt /private/host-only/candidate/build-receipt.json \
  --evidence /private/host-only/acp-evidence
```

`--build-receipt` is optional for `make dev` runs (as in `smoke.py`); without it
only the source identity is recorded and the run is not release evidence. The
packaged source Kits under `kits/` need staged `dhi-notices`/`image-repair`
inputs before stock SBX can build them, so `mcp_load_uat.py`,
`mcp_gateway_uat.py`, and `acp_mcp_uat.py` accept `--commands FILE` to overlay
the isolated scope's `commands.json` with staged sources or published digests.

With a receipt, evidence requires the exact source tree at the specified revision, including
any dirty bytes bound by an observed-build receipt from `scripts/build-candidate.py`, verified before any
product or stock-SBX effects. Evidence and receipts must be outside the mounted
checkout. It writes source identity, artifact hashes, shell output, final public
status, and independent before/after stock-SBX inventories to `acp-uat.json`. It authenticates the daemon that belongs
to its selected home and removes only that scope's VMs. A cleanup error fails
the run and is retained in the evidence.

The guest parent shell starts `acp run fixture-session &`, waits for its
independent ACP session ID with `acp list --wait --json` in an initially empty scope, checks that default
`acp list` and `acp status ID` are readable, checks that `ps --marsh` and
`top --marsh --once` show the linked live session and Kit job, and checks
`ps --marsh --json` has the versioned snapshot. Ordinary `ps` is compared
with the Linux executable on the same shell PID. The harness submits two
lower-level turns, then sends one synchronous `acp ask` turn that prints the
answer, sees updates, cancels a
live turn, approves one offered one-time permission, and stops the Kit. `jobs`
must list the live background job; `wait` must match the terminal Kit receipt,
whose cleanup must be verified. The fixture has no credential or network
capability. The real provider Kits require a separate credentialed UAT.
A separate terminal checks two `top --marsh` refreshes before interrupting
that viewer without leaving a watcher.
The packaged fixture reuses one explicit `acp prompt --key UUID` and checks
that the retry emits no second update; the same key with changed text must
fail. The daemon caller test drops the first reply after admission and verifies
that retrying the printed key returns one stable turn ID and only one ACP
`session/prompt` reaches the Kit. The first 1024 keys remain in the live
session's in-memory ledger; new keys then fail closed, while an old key still
retrieves its original turn ID. Start a new session for more turns. Restart
recovery is outside this gate. A held turn remains cancellable after a short
client timeout would have elapsed, and a second turn is admitted after cancel.
After a handoff, losing the original run process lease leaves the new
controller active; detaching that final controller stops and reaps the Kit.
Detaching a controller while the run lease is still live leaves the Kit
available for another controller.

While the parent holds its controller lease, separate stock shell callers must
fail to attach or prompt the session. The fixture's remote ACP ID must not work
as a marsh session ID, and a shell in another project must not inspect the
opaque session ID. After the normal turns, the fixture sends malformed ACP JSON
followed by a valid update and response. The parent must report a turn error,
must not accept the later update as a successful turn, and must still stop and
reap the Kit with verified cleanup. This checks the public shell boundary and
one invalid frame; protocol fuzzing remains in the ACP transport tests.

Status and streamed turn updates are visible to another shell from the same
macOS user, exact project, and selected home. The project check governs the
public client API; it is not an isolation boundary against code in a shared
shell VM. Shells for different projects under one selected home run as the same
user in that VM and can read each other's mounted files and relay credentials.
Only the controller shell can prompt, cancel, stop, or see pending permission
requests through the public API. Use a separate selected home for private ACP
work.

The harness also checks a malformed update followed by a clean turn, 400 burst
chunks, unknown content/diff/location/metadata, exact UTF-8 stdin including
trailing newlines, oversized stdin rejected before dispatch, and persistent-only
permissions with an actionable note. The cancellation fixture emits 200 updates
after cancel, all of which must survive paging without loss. A late update after
the wire response must be counted as unscoped, not attributed to that turn. An
oversized later update must not corrupt a retained older turn's loss receipt.
The no-ID `list --wait` journey also runs after a prior session ended.
It then starts a second shell with `set +o history` that reserves a session,
runs it in the background, prompts, stops it, and checks the structural Kit
receipt (`cleanup: verified`, the wait status equals the receipt's exit). The
flight recorder was removed; structural receipts (`marsh results`) are the job
record, so there are no recorder rows to assert.

The daemon's real-caller tests cover concurrent controller rejection, shell
and run-process death, handshake disconnect, permission timeout, and update
loss. Agent-private activity remains outside the ACP and Kit stream boundary.

## Composition, retention, and control bounds

For unambiguous startup (including two simultaneous jobs or quoted agent names):

```sh
agent=fixture-session
id=$(acp reserve "$agent")
acp run --reservation "$id" "$agent" &
pid=$!
acp list --mine --wait "$id"
printf 'Review exactly these bytes\n\n' | acp ask "$id" -
acp stop "$id"
wait "$pid"
```

An unclaimed reservation expires after 15 seconds. `acp run` prints the ready
session and Kit job identities on stderr; these are distinct from `$!`.
`--mine` filters the initiating shell identity, not the current controller or
project's newest job. `list --wait` selects the sole live session; several live sessions require an
explicit ID. Terminal history does not make that selection ambiguous;
an older ready session is never proof that new work started. A sole `-` prompt
reads exact UTF-8 stdin, without trimming. Both the prompt text and its encoded
ACP **and daemon control** frames (JSON escaping, project/session fields and
envelopes) must each fit 1 MiB. Thus the usable text ceiling can be slightly lower
and depends on escaping and session-path lengths. Invalid UTF-8 or oversize input
is rejected before dispatch with a shorten/split remedy. `ask` prints a retry
key for a lost or invalid reply only after its complete request frame was sent.
Connection failures, incomplete writes and explicit daemon admission rejections
do not get an uncertain-outcome hint. Argv words otherwise retain space-joined
semantics. Human `permissions` shows one-time choices and response commands;
`--json` retains machine output. Human status defaults to the latest turn.

The raw update queue holds four values, each bounded by the 1 MiB incoming wire
frame limit (at most 4 MiB serialized JSON, plus parsed-object overhead). Its
consumer runs independently of the response future and off the Tokio worker
threads. Normal backpressure is lossless. No controller or
active-turn lock is held while waiting on queue/transport IO. Cancellation dispatch uses an independent writer; healthy post-cancel updates
retain ordinary lossless backpressure. A consumer stalled for the full 10-second
delivery deadline fences normal transport. During cancellation, that deadline
instead abandons remaining turn delivery (counting every omitted update) so the
reader can reach the terminal response. The deadline is paid only once, not once
per queued update. Discards are counted honestly, not hidden. Outbound queue/write deadlines also bound
control behind a hostile request flood; a dispatched cancel does not imply
provider compute or billing stopped. Terminal capture records `updates_lost`
and `dropped_updates`, plus a content-free gap for actual transport capture loss.

The wire prompt response closes that turn's update sink immediately, not when a
scheduled future happens to run. Updates/invalid frames outside an active turn
are not reattributed to the completed turn: the session exposes
`out_of_turn_updates` (MCP `session_out_of_turn_updates`) and records an unscoped
`acp_capture_gap`. Their content is outside this turn-capture interface; it is
not silently presented as part of a complete turn transcript.

Status retains 512 KiB of update JSON per session, pages at most 32 updates /
256 KiB, and omits individual updates exceeding the page bound. Such retention
loss is exposed by `retained_after` and `updates_lost`. Each receipt tracks its
own actually omitted/evicted cursors; a later oversized update does not mark a
fully retained earlier turn as lossy. Status-window eviction is not
reported as transport capture loss. Turn-specific capture loss resets on
admission without erasing old receipt loss. Status is responsive during pending
cancel dispatch, and a reserved cancel cannot target a newer turn. A cancel that
races natural completion returns `AlreadyFinished`; the same session remains
usable. Publication name checks and grant assignment are serialized; rejected
daemon preflight does not prepare a Kit. The admitted
generation reserves the ACP name and fences parent steering throughout preparation;
rollback revokes only that generation. After admission, a host registration,
preparation or load failure revokes the grant. Host declaration,
stock-registration ownership and cross-protocol name checks also precede Kit
preparation. The host holds its file lock and requests exactly one preparation
through the same private channel used by MCP publication; it receives no general
daemon credential. The daemon rechecks the admitted generation and project before
and after preparation. A host preflight failure does not boot the target Kit.
A Kit warmed after successful preflight may remain warm if registration or load
subsequently fails; publication rollback is not VM deletion.

## Runnable Linux boundary evidence (not stock-SBX qualification)

The actual Node acceptance fixture, daemon Unix sockets, and MCP JSON-RPC over
a Unix socket pair can be exercised without stock SBX or credentials. An additional
external Python MCP process drives the real exporter library through a Unix
socket, including idle paging, retry, cancellation tail, late updates and retained
old receipts. The exact CLI process test also distinguishes a connection failure
before dispatch from a discarded real daemon acceptance reply:


```sh
export PATH=/home/agent/.cargo/bin:$PATH
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
# Use a new empty target for each independent source snapshot.
export CARGO_TARGET_DIR=/tmp/marsh-acp-$(date +%s)-target
cargo test -p marsh-acp --test fixture_caller --test capture_backpressure -- --test-threads=1
cargo build -p marsh --bin marsh
MARSH_ACP_CALLER_BIN="$CARGO_TARGET_DIR/debug/marsh" \
  cargo test -p marsh-acp --test fixture_caller cli_stdin -- --ignored --nocapture
docs/model/check.sh 'Grants*.cfg'
```

Record source hashes and the exact executable paths/SHA256 before execution;
never reuse a target across copied source trees based on copied mtimes. These
tests substitute only Kit launch and host registration with a bounded
local fixture process and declaration writer. They do not qualify VMs, stock
Gateway loading or packaged cleanup. Publication,
revocation, and one-time permissions are specified in `docs/model/Grants.tla`
(see `docs/model/README.md`); that bounded model is not a Rust refinement proof
or stock-SBX qualification.

## External MCP clients

`acp publish ID --name NAME [--kit KIT | --sandbox SANDBOX]` transfers control of one
idle ACP session to a revocable MCP tool named `NAME`. It requires the current
controller shell. A running turn must finish first. The parent shell can still
inspect status but cannot steer the session until `acp unpublish NAME`.
The grant keeps the Kit alive when its original `acp run` process lease ends.
If the publishing shell exits, the grant and Kit stay live until unpublish.
Any new shell in the same project and selected home can unpublish
after the publisher detaches; while published, `acp stop` cannot stop the Kit.
Unpublish transfers control to the shell that ran it. Run `acp stop ID`, or
let that shell exit, to stop the Kit; then `marsh stop` can stop the idle scope.
A daemon restart can leave a host MCP registration
advertised with no live grant. From a new shell in the same project and
selected home, run `acp unpublish NAME` to remove it; repeating the command
is safe. Remove an optional host Codex entry separately with
`marsh acp remove-published codex NAME`.
After that lease exits, Brush `jobs` and `wait` describe only the finished shell
job. `acp list` and `acp status ID` still show the live agent session; the
scope activity view can also track its Kit job independently.

The tool exposes `ask`, `status`, `cancel`, and `respond`. An `ask` needs a
new UUID key, such as `7e4a4a9d-5bf6-4e88-92ac-4a47c5fb1670`; retry the same
key and text to recover the same turn. It returns `{turn_id, start_cursor,
next_cursor}` immediately, without old history. Poll `status` with that `turn_id`
and `cursor: start_cursor`. Pass every returned `next_cursor` back **unchanged**:
the cursor is exclusive (last delivered update), and idle polls do not advance
it. Continue until both `turn_active` and `more_updates` are false. Status without
a `turn_id` selects the current/latest turn; concurrent clients should always
supply their returned ID. Updates carry their own turn ID. Start/end cursors,
stop reason, bounded content-free error/remedy and transport dropped counts are
retained for all 1024 admitted turns, including retries after subsequent turns.
Unknown updates and content retain the complete JSON object, including diffs,
locations and `_meta`, rather than a reduced typed projection. `respond` accepts only the pending request's offered one-time allow
or reject choice. The tool does not expose a persistent permission grant.
Status omits raw Kit receipts, attachment stderr, and host diagnostics. MCP
clients that receive the tool can read the session's prompts, updates, and
pending permission requests, and can steer it with the controller's project
privileges. Publish only to clients you trust with that authority. Allowed
one-time actions can write the project through the agent's Kit. A published
prompt is attributed to the publishing shell session in its receipts.
Pi's packaged Gateway extension calls MCP tools directly rather than through
an ACP one-time permission request; loading a tool into a Pi Kit grants that
Kit direct access to it.

This is an ACP client session exported through MCP, not a native ACP server
endpoint. Stock Docker Sandbox clients receive it with `--sandbox SANDBOX`.
`--kit` prepares and loads the exact registered Kit VM, not an arbitrary older
ready worker. Options may appear in any order. Publication always prints the
stock MCP server name and a command for loading another sandbox. For host Codex, run `marsh acp install-published codex NAME` from the same
project and selected home after publication, then start a new Codex chat to
discover the MCP tool. Remove the host registration with
`marsh acp remove-published codex NAME`; `acp unpublish NAME` revokes its
authority even if a host client retains a cached registration. A later
publication with the same name has a fresh generation and cannot be reached
through an old exporter or stock sandbox load. Reload the new publication
to grant a client access again.

`published_acp_uat.py` checks this flow against a real disposable marsh daemon,
fixture ACP Kit, stock Docker Sandbox MCP gateway, and a disposable host Codex
MCP configuration. It exercises a host turn, child turn, retry key, one-time
permission, parent process lease exit, cached tool revocation, and removal of
the registration from a new shell after publisher exit. Use exact
candidate artifacts and a fresh evidence directory:

```sh
python3 tests/acceptance/published_acp_uat.py \
  --source-tree "$PWD" \
  --source-revision "$(git rev-parse HEAD)" \
  --marsh "$PWD/target/release/marsh" \
  --sbx "$(command -v sbx)" \
  --guest-artifacts "$PWD/target/libexec/marsh" \
  --codex "$(command -v codex)" \
  --build-receipt /private/host-only/candidate/build-receipt.json \
  --evidence /private/host-only/published-acp-evidence
```

Both harnesses verify the exact observed build receipt before product effects.
A dirty tree is not a waiver or a separate pass class: its bytes must match the
receipt, and the complete journey and cleanup must pass. Unbuilt edits are rejected
before product, stock or provider calls. There is no `--allow-dirty` bypass.
Real provider Kit model turns remain a separate credentialed acceptance gate.

## Real provider Kits

There is one Kit per agent. `claude`, `codex`, and `pi` run the agent's CLI;
`acp run claude-session` (or `codex-session`, `pi-session`) runs the same Kit
in its ACP mode for a steerable background session. `packaging/agents.json`
selects the mode with a fixed `"arguments": ["--acp"]` tail, which the daemon
appends to the Kit's own entrypoint exactly like typed arguments; the
declaration cannot change the image, entrypoint, mounts, network, or
credentials. Each Kit image ships the CLI and the pinned ACP adapter (the
official `claude-agent-acp` and `codex-acp`, the community `pi-acp`), so ACP
sessions share the CLI's warm VM and its network allowlist. ACP mode disables
the agent's own sandbox like the CLI does: Claude's Agent SDK runs Claude Code
through `claude-cli.sh`, which merges `sandbox.enabled=false` into the
command-line settings (so a project `.claude/settings.local.json` that enables
the sandbox does not fail Bash with a bubblewrap error), and Codex sessions
start in `agent-full-access` unless `INITIAL_AGENT_MODE` is exported. Pi has no
OS-level command sandbox. `ps` and `top` are reserved shell command names for
the explicit process view; rename any older Kit mapping that used either name
before upgrading.

For published MCP tools inside child ACP sessions, the Claude and Codex Kits'
ACP modes add only the validated sandbox-local stock Gateway to `session/new`.
Preload the tool into the exact Kit VM; publication in the project alone does
not load it. The focused `acp_mcp_uat.py --agent claude-session|codex-session`
journey checks the packaged adapter's advertised HTTP MCP capability, a real
model tool call, the pipeline's `FOO` side effect and answer, one-time approval
of only the named tool when required, and full isolated cleanup. The pinned Pi
ACP adapter does not connect ACP-provided MCP servers. Its packaged Pi process
loads the same image-owned Gateway extension as plain Pi, exposing the generic
`mcp_gateway_list` and `mcp_gateway_call` tools to the model. The separate
`pi_mcp_uat.py` journey loads one publication into the pi Kit VM, then
checks a real plain Pi and Pi ACP model call, the pipeline's `FOO` side effect
and answer, and full isolated cleanup.

After `make build` (or the single-step `make dev`), run `real_acp_uat.py --agent NAME --live` for each of
`claude-session`, `codex-session`, and `pi-session`. It opens a disposable
stock-SBX scope, starts the packaged agent Kit in ACP mode, sends two prompts in one session
and a third after reopening in the same selected home, checks all streamed
`PONG` replies and `end_turn` states, stops both jobs, and
requires complete scope cleanup. It writes candidate and artifact hashes plus
the observed stock-SBX credential mode to `target/acceptance/real-acp/`. The
source and artifacts must match the observed build receipt. Configure
the corresponding service with `sbx secret set anthropic`
or `sbx secret set openai` first. With no OpenAI secret, use
`--agent codex-session --expect-auth-required` to check the visible failure and
cleanup path; that does not qualify Codex's live-model flow.
