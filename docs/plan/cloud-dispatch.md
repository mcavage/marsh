# Cloud placement: session-origin dispatch

Date: 2026-10-16. Status: **implemented as a preview**; user manual in
[Docker Cloud (preview)](../cloud.md); contract in
[cloud placement](../design/cloud-placement.md). This slice covers design
section 8 units 1 (transport) and 2 (workspace), plus the split-fork and
fanout destinations of unit 3.

## What runs where

| Step | Code | Notes |
|---|---|---|
| `cloud { LIST; }` | Brush patch `0097` (`ast::Command::Placement`, `interp::placement_as_simple`) | One external stage: `marsh cloud -c SOURCE`, source quoted and unexpanded; caller redirections stay local |
| `marsh cloud -c` / `--` | `crates/marsh/src/main.rs` `cloud_command` | `Execute` with `Placement::Cloud`, argv bytes; a closed stdout cancels and settles, then exits 141 |
| Admission, receipt | `marsh-daemon` `begin_cloud_job`, `record_cloud_sandbox`, `mark_cloud_running`; journal validation for cloud receipts | Session-origin only; counts toward the shared job pool; `vm_id` is the cloud sandbox ID, no worker/container |
| Snapshot | `marsh-daemon/src/cloud_fs.rs` `write_snapshot` | Caller's view: the split fork when cwd is in one, else the project. Descriptor walk, Git ignore rules for a project (as split), never `.git`/`.marsh`/special files, stable reads; streamed gzip, never spooled |
| Provision / cleanup | `marsh-sbx/src/cloud.rs` `CloudSbx` | Owned ID-bound lifecycle; retire observes absence; 4 h TTL, delete on timeout |
| Remote helper | `crates/marsh/src/cloud_helper.rs` (static `marsh-local`) | Control steps run as root via `sudo -n` from a root-owned installed copy, with state in root-only `/var/lib/marsh-cloud`: `prepare` (workload user, natural path), `unpack` (baseline with hashes), `run` (workload as `marsh-run`: no sudo/Docker, environment rebuilt from the sandbox's proxy/CA settings plus the filtered request; after exit every process of that uid is killed, then the status is recorded), `status`, `signal`, `delta` (refuses without a status) |
| Orchestration | `marsh-backend/src/cloud.rs` `execute_cloud` | Phases admit → snapshot → provision → upload → execute → confirm end → capture to owner-only `cloud/results/JOB.delta` → retire → apply |
| Apply | `cloud_fs::apply_delta` | Full validation first (paths, protected names, ASCII case aliases, duplicates, descendants of replaced entries); archive end record; root identity check; descriptor-relative `O_NOFOLLOW` walk; destination alias check (`statat` vs exact directory entry); order: retype deletions, directories (owner-writable), files/symlinks via staged `renameat`, other deletions (a non-empty directory is kept and reported), directory modes last; serialized per destination root |
| Cancellation | `Relay` + `JobControl` | Caller frames are read from admission, with credit-windowed stdin so signals are never queued behind input: Ctrl-C or loss during setup stops before execution (130); during execution `cancel_tree` sends INT, then KILL after the grace; confirmed partial edits return. A closed client stdout cancels the same way and the client exits 141 after settlement. A lost caller whose destination is a split fork gets no write-back (payload retained) |
| Stream flow control | `marsh_contracts::cloud::run_frame`, `cloud_helper::relay_streams`, backend `Window` | The stock exec channel has no backpressure of its own (see the [feasibility findings](cloud-feasibility.md#streams-and-file-transfer)): unread stdin broke the sandbox's exec relay, and output more than about 1 MiB ahead of the CLI's reads was dropped. Both directions are framed and windowed at 256 KiB: the helper acknowledges stdin the workload accepted, the host acknowledges output it delivered (reading the carrier promptly and queueing within the window), and an end-of-output frame makes any loss a reported failure (exit 125), never silent. The client's stdin is credit-windowed |
| Failure retention | `execute_cloud` | A failed capture keeps the sandbox (quarantined, owned) instead of deleting the only copy; a failed apply keeps `cloud/results/JOB.delta` |
| Recovery | `marsh workers reset cloud` | Retires every owned cloud sandbox and releases slots of uncertain cloud jobs. A daemon restart marks running cloud jobs unknown/uncertain; nothing is replayed or written back |

The block runs the session shell profile directly in the sandbox (as user
`agent`, with the image's tools), not the cloud worker profile: no registered
Kits run in cloud yet.

## Evidence

Opt-in live acceptance, real installed dev product, stock `sbx`
`v0.48.0-683-g7167374c8`, real Docker Cloud:

```sh
python3 tests/acceptance/cloud_uat.py --marsh PREFIX/bin/marsh \
  --guest-artifacts PREFIX/libexec/marsh --kit tests/acceptance/fixture \
  --evidence DIR --source-revision REV --source-tree . --i-accept-cloud-charges
```

It covers C02 (pipeline bytes, stderr, non-UTF-8 argv, EOF, exit status,
`PIPESTATUS`/`pipefail`, downstream close settling before the next command,
3 MiB of stdin through to EOF, 16 MiB of output to a reader that waits 20 s),
the stop boundary (a `setsid` writer killed before capture; workload user has
no sudo or Docker; a program that cannot start is 127; an ignored cwd works), C03 (create/modify/delete/mode/
directories returned before the next command, nonzero exit still returns,
ignored inputs stay local, generated ignored outputs return), C04 (overlap is
last writer; unrelated local edits survive), C05 (one-branch split: patch in
`join`, source untouched), C06 (two fanout placements), C09 (Ctrl-C during
execution returns confirmed partial edits with 130; Ctrl-C during setup runs
nothing; Ctrl-C under saturated stdin backpressure settles), C10 (daemon killed mid-execution: unknown/uncertain receipt, no
write-back, `workers reset cloud` observes absence), C11 (symlink entries not
followed, caller `.git` untouched), and C12 (every receipt names its sandbox
with verified cleanup; ownership map empty; sandboxes absent from the stock
inventory). The final 2026-10-16 run passed all 39 checks in 366 seconds
(15 cloud calls); each call took about 20 to 25 seconds of wall time, dominated by provisioning (~6 s), upload of the
helper and snapshot (~8 s), capture (~3.5 s), and deletion (~2 s).
VM-free tests: archive path validation (`marsh-contracts`), apply authority
cases (`marsh-daemon` `cloud_fs`), and the Brush block shape (`smoke.rs`
`cloud_block_runs_as_one_stage_with_unexpanded_source`).

These timings are observations from one run, not a benchmark.

The 2026-10-17 run of the extended suite (agent calls, split in a block, Git
copy, options, interactive shell, signed-out refusal) passed 53 checks in 543
seconds plus 4 signed-out checks, with no cloud sandbox left and the local
smoke gate passing on the same build.

## Follow-up slice (2026-10-17)

- **Options:** `--cpus`, `--ttl`, `--image` on `marsh cloud` and before a
  block (`cloud --cpus 8 { ... }`, Brush patch `0097` option words), with
  `MARSH_CLOUD_*` defaults; validated by the daemon (shape table, 1m-24h,
  digest-pinned image).
- **Git:** `cloud_fs::write_git` adds a private, hook-free `.git` to the
  snapshot: the project's own metadata, or for a split fork its admin
  directory plus the object directories its alternates name (rewritten to
  the copy). Config is reduced to the repository format and extensions (no
  remotes, credential helpers, or commands). The helper unpacks it outside
  the baseline, so it is never captured; host apply rejects `.git` anyway.
  Over 512 MiB (or a file over 256 MiB) it is skipped with a notice.
- **Interactive cloud shell (C14):** `marsh cloud` with no command on a
  terminal. The client uses its existing raw-terminal relay; the request
  carries the terminal size; the helper runs the workload on a pty
  (util-linux `setsid --ctty`) and relays resizes (`run_frame::RESIZE`).
- **C01:** a real stock "not signed in" refusal (only the stock CLI's
  `--cloud` calls see an empty HOME) yields 125 with the `sbx login`
  instruction, nothing run, a `not_started` receipt, and no sandbox.
- Pinned Rust 1.95 `cargo fmt --check` and `clippy --workspace --all-targets
  -D warnings` pass in `rust:1.95-bookworm` (Linux target).

- **Split inside a cloud block (C07):** `marsh_daemon::split::LocalSplit`
  (the daemon's snapshot, fork, capture, rendering, and manifest code, with no
  journal) behind a sandbox-local split hook (`crates/marsh/src/cloud_split.rs`),
  installed when the helper sets `MARSH_CLOUD_ROOT`. Brush patch `0097` now
  normalizes `split {` as a cloud block's first command and displays
  composition branches in braces, so multi-item branches survive the source
  round trip.
- **Agents inside a cloud block** (superseded 2026-10-18 by Kit sandboxes,
  below): cloud bindings (`cloud-kits.json`, default
  `claude`/`codex` to the stock cloud agents) produce command links in the
  workload (`/run/marsh-cloud/bin`, a socket served by the helper). Each call
  travels over the run carrier (`KIT_*` frames, inside the same acknowledged
  windows) to `AgentCalls` in the backend: a child job admitted under the
  cloud job (spawn set and limits apply), an agent sandbox
  (`sbx --cloud create AGENT`, ownership-bound like the rest), the parent's
  current files streamed `archive` -> `unpack` sandbox to sandbox, the agent
  run as the sandbox's `agent` user under `setpriv --no-new-privs
  --clear-groups` with the stock credential environment (stop = kill that
  user's no-new-privileges processes), streams relayed to the calling
  process, then capture, retire, and `apply` into the parent as `marsh-run`.
  A parent that ends cancels its running calls and waits for them to settle
  before its own capture. Live: a real `claude -p` call wrote a file that
  reached the laptop through both levels.
- Review fixes (2026-10-17): a fork's Git copy opens the admin and object
  stores from the pinned project without following links and never reads the
  admin's `alternates` (a branch job can write it); the Git copy is an
  allowlist; `MARSH_SPAWN` narrows agent calls; calls are counted before
  their threads start, refused once the block settles, and never apply after
  the block's capture begins; control steps use `/usr/bin/sudo` and fixed
  paths (the agent user can write its `PATH` directories); each call has its
  own reply queue and window.
- Transient stock sign-in failures before a step starts ("init cloud
  dispatcher", token refresh contention) are retried for repeatable steps.
- **Registered Kits in cloud and job-origin placement (2026-10-18):** Docker
  Cloud accepts our v3 Kits by digest (`sbx --cloud create KIT@sha256:...`:
  the Kit's image, entrypoint, network policy, and credential injection; a
  real `marsh-claude -p` answered there). The stock-agent bindings are
  replaced: a registered command's cloud form is its own digest-pinned Kit
  (or a `cloud-kits.json` `{"kit": REF}` override for source-registered
  Kits), which also settles tag pinning. Kit sandboxes have no root, so a
  marsh mixin (`--kit`, a `lifecycle` startup step as uid 0) creates the
  natural workspace path owned by the Kit user; the uploaded helper then runs
  every control step as that user (`Ctl { kit: true }`, state in
  `/tmp/marsh-cloud`). The runner sets no-new-privileges before starting the
  workload; the stop rule kills the user's no-new-privileges processes except
  the runner and the caller's own lineage. With a mixin the inventory shows a
  stock-built template instead of the image, so ownership readiness matches
  the Kit's agent name (`sbx kit inspect`) for Kit records. In a Kit sandbox
  the control helper shares the workload's user: the workload can disturb its
  own result, but write-back is still checked and applied on the host into
  the caller's view only. Job-origin `marsh cloud` (in-job CLI ->
  `ProcessRun` with cloud placement) runs in the parent Kit's cloud form with
  the parent's project view, admitted as a child (the same-Kit chain counts it); refused without a cloud form, without write
  access to the view, and with `--image`. Review fixes (2026-10-18): `cloud`
  is its own spawn name (in every root's set; `MARSH_SPAWN` withholds it);
  the parent's Kit identity must match the binding; TTL is bounded by the
  parent's remaining wall time; write-back is skipped (payload kept) when
  the parent job has ended; the workspace mixin refuses an existing path;
  `marsh stop`/`reset` retire owned cloud sandboxes, and absent-daemon stop
  reports recorded ones; the Kit-mode stop rule is documented as best effort.
  The helper's `run` is at most once per sandbox (an exclusive `started`
  marker) and signals its start with an empty ACK; the backend (`spawn_run`)
  starts it again only when the stock CLI failed before reaching the sandbox
  (a live run hit a `login.docker.com` timeout there), never after a start.
- **Direct Kit calls (2026-10-19):** the client offers a block that is
  exactly one literal call (`single_call`: no expansions, globs, lists,
  pipes, assignments, or redirections other than `</dev/null`) as
  `--direct N WORD...` beside the normal `-c SOURCE`. The daemon runs it in
  that command's Kit (Argv mode, the Kit's entrypoint) when the command has a
  cloud form and the spawn set allows it; otherwise the block runs in the
  cloud shell unchanged. The receipt is the command's own (its name and
  arguments). Also: a block's stdin that relays the caller's terminal is
  named (`MARSH_TERMINAL_STDIN`) so `fanout` reads nothing from it, as at a
  local prompt (Brush `0097`); split capture in a sandbox takes the fork time
  from the filesystem's clock (coarse Linux timestamps hid same-size edits).
- **Listings after write-back (2026-10-19):** the shell VM's virtiofs share
  can keep serving a cached directory listing after the host writes into it
  (lookups by name are fresh), so `ls` right after a block could omit files
  it created; this is intermittent and also affects host-side writes. The
  apply reports the directories whose listing changed and the daemon sends
  them (`AttachmentFrame::Refresh`) before the exit status; the client, in a
  Linux VM, creates and removes a probe entry in each, which refreshes the
  share's listing (observed: a stale `ls` became current after one probe). Live: `cloud_uat.py
  --scenario kits` (fixture Kit: agent call streams/status/return,
  `MARSH_SPAWN`, cancellation across both levels, job-origin streams, status,
  write-back, receipts).

## Open gates

- **v2 TODO:** cloud -> laptop placement (C08: a cloud workload calling back
  to a local Kit or the laptop), and `marsh run --place cloud|local KIT`
  (section 4 of the design). Both are deferred by decision (2026-10-18).
- Live sharing of one remote workspace between sandboxes (agent calls use
  snapshot crossings by decision); cloud -> cloud from a cloud block's own
  shell (`cloud` inside a block) stays refused.
- APFS Unicode-alias real-caller case (C11).
- The session shell profile has no `sudo` or Docker inside cloud blocks
  (required for a verified stop of every writer); a privileged profile needs
  its own stop guarantee.
- A transient stock inventory failure during the post-run steps quarantines
  the sandbox and is reported as a lost transport.
- A public inspect/discard command for retained `cloud/results/*.delta`.
