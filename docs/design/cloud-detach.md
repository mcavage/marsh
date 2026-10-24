# Detached cloud work: close the lid, collect later

Status: **implemented (v1).** Every step of
[the order of work](#10-order-of-work) is built, including nested agent
calls with the opt-in token (§6): off by default, a detached block that would
make one is refused. [Implementation notes](#11-implementation-notes-v1) list
where v1 differs from the text above. It extends
[cloud placement](cloud-placement.md) and changes some of its decisions, listed
in [What changes in existing decisions](#7-what-changes-in-existing-decisions).

## 1. The promise

Start long work in Docker Cloud, close the laptop, and come back to its
output and file changes:

```sh
marsh cloud -d -- make long-benchmark          # prints a job ID, returns at once
cloud -d { claude -p "refactor the parser and run the tests"; }
```

Later, on the same Mac, whether it slept, lost the network or restarted:

```sh
marsh jobs                  # cloud-detached jobs show running / finished / collected
marsh cloud attach JOB      # replays output so far, then follows live; Ctrl-C detaches
marsh cloud collect JOB     # fetches the result now (marsh also does this by itself)
```

When the work has finished, marsh collects it by itself: at the next daemon
start or wake, and on a timer while it runs. File changes return to the
workspace when that is safe, and otherwise wait as a patch (see §5). The
output is kept in the job's receipt, so `marsh results` shows it like any job's.

The first version covers session-origin runs only: `cloud -d { }` and
`marsh cloud -d` from your marsh shell or a host terminal. A detached run
cannot call back to the laptop; with an access token you opt in to, it
starts agent sandboxes itself (see §6).

## 2. Today, and why it can't survive a closed lid

A cloud call depends on one live `sbx --cloud exec -i` stream from the laptop:

- the workload is a child of that exec session, and nothing is spooled
  remotely, so output produced after the stream drops is lost;
- when the stream drops with no remote exit status, marsh deletes the sandbox
  and writes nothing back;
- after a daemon restart, sandboxes are quarantined (cleanup only), so marsh
  cannot run anything in an earlier sandbox again, by design;
- sandboxes are created with `--on-timeout delete`, so results can't outlive
  the time limit;
- the base snapshot is not kept on the laptop, so a late write-back can't tell
  whether local files changed since.

The remote helper already has useful pieces: an at-most-once `started`
marker, a hashed baseline, and repeatable `status`, `signal` and `delta` steps.

## 3. Remote side: a run that outlives the connection

`--internal-cloud run --detached` starts a small supervisor and returns as
soon as the workload has started:

- **Supervisor.** Double-forked into its own session (`setsid`), owned by
  root, outside the exec session's process tree. It starts the workload as
  `marsh-run`, exactly as today, and waits for it.
- **Spooled output.** stdout and stderr go to append-only files in the remote
  state directory (`out.1`, `out.2`), each capped (default 256 MiB; past the
  cap the tail is kept and the job records `output truncated`). There is no
  terminal and no stdin: a detached run reads from `/dev/null`.
- **Completion.** When the leader exits, the supervisor stops every
  workload process (as today), writes `status`, and then prepares the delta
  into `result.delta` with the existing delta code, so collecting it later
  needs no computation.
- **Idle stop.** After `result.delta` is complete, the supervisor writes `done`.
  The laptop stops the sandbox when it sees `done` (§4). If the laptop is
  away, the sandbox stops when its time limit runs out.

New read-only helper steps over a fresh `sbx --cloud exec` each:
`tail N OFFSET` (output from a byte offset), `state` (`running` / `done` /
`failed`, exit status, sizes), and `fetch` (stream `result.delta`). `signal`
works as today.

## 4. Sandbox lifetime: the time limit pauses, it doesn't kill

Detached runs are created with `--on-timeout stop`. Measured with sbx 0.49.2
on 2026-10-23 (a throwaway shell-image sandbox):

- A process started with `setsid nohup` keeps running after the `sbx --cloud
  exec` that started it ends.
- When `--ttl` lapses, the sandbox is **suspended in place, memory and disk
  included**. On the next `sbx --cloud exec` (which starts a stopped sandbox
  by itself, no interactive step), the process **resumes where it was**: a
  counter at 276 continued at 277, 278, 279, and the guest's uptime counted
  only running time.
- Resuming restarts the time limit with the original `--ttl`.
- The sandbox can't stop itself: the shell image has no `poweroff`, and
  `/proc/sysrq-trigger` isn't exposed. After its work is done it stays
  running (and billed) until the time limit suspends it.
- A stopped sandbox from a month earlier was still listed, so stopped
  sandboxes are kept at least that long.

So:

- `--ttl` (default `2h` for detached runs) is how long the run may use
  compute without the laptop. When it lapses mid-run, the run is **paused, not
  lost**: the next collect resumes it, and it continues for another `--ttl`.
  `state` reports `paused (time limit)` meanwhile. A longer `--ttl` lets
  more run unattended; a shorter one bounds idle billing after the work ends.
- When the laptop sees `done`, it fetches the output and delta, verifies
  them, and deletes the sandbox. If the laptop is away, a finished sandbox
  idles until the time limit suspends it, then waits stopped.
- **Retention.** A finished or paused, uncollected sandbox is kept for a
  retention period (default 14 days), measured on the laptop; then marsh
  deletes it, or on `marsh cloud discard JOB`.

## 5. Laptop side: durable records and safe write-back

At start, before the command runs, marsh writes a durable detached record
(fsynced, in `control_home/cloud/detached/JOB.json`): job ID, sandbox ID and
name, request, destination root and its identity, the time limit, and a
**base manifest** with the path, type, mode and SHA-256 of every entry it
uploaded. The job's receipt shows `placement cloud, detached`. Detached jobs
don't hold one of the 8 local job slots once the start is confirmed; a
separate cap (default 8 outstanding detached jobs) bounds sandboxes and cost.

**Collect** (automatic, or `marsh cloud collect`):

1. Start the sandbox if it is stopped; read `state`.
2. If still running, `tail` new output into the receipt and try again later.
3. If done: fetch the remaining output and `result.delta` into
   `control_home/cloud/results/JOB.delta`, verify, then stop and delete the
   sandbox. Retire-before-apply is unchanged.
4. **Write-back.** For every path the delta touches, compare the local file
   with the base manifest.
   - If every touched path is unchanged locally, apply the delta with the
     existing `apply_delta`: changes return automatically, as for an
     attached run.
   - Otherwise apply nothing. Keep the delta as a patch, and say so in the
     job and on the next interactive command:
     `marsh: cloud job 42 finished; 3 files changed here since it started. Review: marsh cloud diff 42`.
     `marsh cloud apply 42` applies it anyway; `marsh cloud discard 42`
     drops it.
   - A split-fork destination that is gone, or a workspace whose root
     identity changed, also keeps the patch instead of applying.

A detached run's exit status is the workload's, from `state`. Collection never
re-runs anything: `started` remains at-most-once, and marsh never execs the
workload a second time.

**Attach** streams `tail` from offset 0 (or a given offset) and then follows
it. Ctrl-C ends the attach, not the run; `marsh cloud stop JOB` sends
INT, then KILL after the grace period, as Ctrl-C does for an attached run.

**Wake and restart.** Daemon start reconciles every detached record against
`sbx --cloud ls`: present, so collect when done; missing, so `lost` (cleanup
verified); unknown, so leave it and retry. A daemon restart no longer turns a
detached sandbox into cleanup-only; that rule stays for attached runs.

`marsh workers reset cloud`, `marsh stop` and `marsh reset` keep deleting
attached-run sandboxes. They keep detached ones unless `--detached` is given,
and they say how many they kept.

## 6. What a detached run can't do (v1)

- **Nested agent calls need a cloud-side coordinator.** Credentials aren't the
  obstacle: a Kit call in the cloud already uses the secrets stored for Docker
  Cloud (`sbx --cloud secret`), not local logins. The obstacle is who creates
  and wires up the child sandbox. Today the laptop does it: `sbx --cloud create`
  from the Kit, with the user's Docker sign-in, then the file snapshot in, the
  stream relay and the write-back into the block. The block's sandbox has no
  Docker Cloud identity, and must not receive the user's sign-in, which would
  let the workload create, read or delete any of the user's sandboxes.

  **v1: an access token you supply (opt-in).** The supervisor (§3) takes
  over the laptop's coordinator role using a Docker personal access token with
  **Allow Cloud Sandboxes access** (the `sandbox:use` scope), stored on the
  laptop. Per Docker's [authentication docs](https://docs.docker.com/ai/sandboxes-api/authentication/)
  and [token reference](https://docs.docker.com/security/access-tokens/reference/):

  - `sandbox:use` is all-or-nothing: it can't be limited to creating
    sandboxes, or to particular Kits. Organization access tokens have no
    sandbox scope. Treat the token as full control of the account's cloud
    sandboxes and quota.
  - The token must be exchanged for a short-lived access token with a JSON
    body (`POST hub.docker.com/v2/auth/token`), and renewed. Cloud secrets are
    header-only and static, so the proxy can't hold it: the token has to be in
    the sandbox.

  So marsh uploads the token at start into a root-only file in the remote state
  directory. The supervisor, running as root, signs `sbx` in with it and serves
  nested calls; the workload runs as `marsh-run` with no `sudo` and can't read
  it. The supervisor enforces marsh's guardrails: only Kits the user allows, a
  cap on child sandboxes per run, children inherit the parent's time limit,
  all children are deleted with the parent. The workload can make the
  supervisor start allowed Kits, not use the token directly. Remaining exposure:
  the sandbox's disk (never saved as a template or moved) and the supervisor
  itself.

  This is off unless the user turns it on (`marsh config cloud-delegate on`,
  plus storing the token), and marsh says so on each run that uses it.
  Without it, a detached block refuses nested calls at start; a one-command
  block that runs directly in a registered Kit (`cloud -d { claude -p … }`)
  works either way.

  **Later: a scoped capability** from Docker Cloud (§9, question 3) replaces
  the token: injected at the proxy, limited to named Kits, a budget and a time
  limit. Then the same feature can be on by default.
- **No terminal and no stdin.** Use the interactive cloud shell for those.
- **No job-origin detach** (a job inside a Kit sandbox starting a detached run)
  until session-origin detach has shipped and been proven.
- **Same Mac only.** Records live in the scope's control directory, so another
  machine can't collect. Collecting from anywhere would need a shared record
  store; that's out of scope.

## 7. What changes in existing decisions

| Decision in cloud placement | Detached runs |
|---|---|
| "Disconnect is cancellation" | Only for attached runs. Losing the stream doesn't affect a detached run. |
| "Restart invalidates all unsettled leases; restarted resources are cleanup-only" | Detached records survive restarts. Collection uses only read-only steps (`state`, `tail`, `fetch`) plus `signal`; it never starts the workload again. |
| "Never infer remote termination from an exited carrier" | Unchanged: termination comes only from the supervisor's `status`. |
| "No execution replay" | Unchanged. |
| "Deltas return automatically; no optimistic-content check" | Detached runs add a base-manifest check before applying; on conflict the delta waits as a patch. Attached runs are unchanged. |
| Sandboxes created with `--on-timeout delete` | Detached runs use `--on-timeout stop`. |

## 8. Model and tests

Extend the TLA+ models before the Rust lifecycle changes:

- **Placement.tla:** a `detached` phase that `Lose` and `Restart` don't move
  to `uncertain`; a `Collect` action that rebinds a detached job to the
  current generation for `Capture`/`Apply`; `Apply` guarded by the base check
  (with `KeptAsPatch` as the alternative). `NoReplay` stays an invariant.
- **CloudOwnership.tla:** a restart-safe `detached` state allowing only
  read-only control and `Stop`/`Delete`, never `Use`.
- **Process.tla:** detached jobs leave the local slot pool once the start is
  confirmed, and are bounded by their own cap.

Acceptance (live, in a separate scope): start a detached run that prints for
10 minutes and edits files; kill the local daemon and the network mid-run;
restart; `attach` replays from the start and follows; the result collects and
applies by itself. Then the same with a conflicting local edit (kept as a
patch, `diff`/`apply`/`discard` work), with the time limit lapsing mid-run
(paused, then resumed by the next collect, output continuous), and with
retention expiring.

## 9. Open questions

Answered by measurement (§4): a stopped sandbox keeps memory and disk and
resumes processes; `sbx --cloud exec` starts a stopped sandbox; a sandbox can't
stop itself; stopped sandboxes are kept at least a month.

Still open, with the platform team:

1. Is a stopped sandbox's disk or memory snapshot billed, and is there a
   retention limit?
2. A way for a sandbox to suspend itself when its work is done, to end idle
   billing before the time limit.
3. **Scoped child-sandbox capability** (to replace the v1 token): a
   credential the cloud proxy injects as a header, accepted directly by the
   Sandboxes API, limited to named Kits, a budget and a time limit, children
   deleted with the parent. (Reported to the Cloud team on 2026-10-23.)

## 10. Order of work

1. Models (§8).
2. Remote supervisor, spool and `state`/`tail`/`fetch`, with VM-free tests.
3. Durable detached records, base manifest, collect and safe write-back.
4. CLI: `-d`, `attach`, `collect`, `diff`, `apply`, `discard`, `stop`;
   `marsh jobs` and `marsh status` show detached jobs; the notice on finish.
5. Nested agent calls with the opt-in token (§6).
6. Live acceptance (§8), then docs (`docs/cloud.md`).

## 11. Implementation notes (v1)

Where the built version differs from, or adds to, the sections above:

- **Supervisor start.** `run --detached` claims `started`, then re-executes
  the helper as `--internal-cloud supervise` with no terminal and stdin,
  stdout and stderr not shared with the exec session; the supervisor calls
  `setsid` itself and is reparented when `run` exits. Its first output line
  (`started` or the failure) is the start report; the host trusts only that.
- **Spool.** Each stream is kept in two segments of half the cap (default
  256 MiB per stream); offsets are absolute, so `tail` returns where it
  actually started and the job records `output truncated` when the oldest
  part was dropped.
- **Base manifest.** The record holds the SHA-256 of a sidecar file,
  `control_home/cloud/detached/JOB.base`: the helper's hashed baseline
  (path, type, mode, SHA-256 of every uploaded entry), fetched before the
  run starts. A path counts as unchanged when its local entry equals the
  uploaded one, or already equals the result (so a write-back interrupted
  by a restart is checked and finished, not kept as a conflict). A
  directory's mode alone is never a conflict.
- **Ownership.** A sandbox is marked detached (its own cap of 8) before the
  run starts and becomes `Detached` once the start is confirmed or the
  start step ended; after a restart a marked sandbox is recovered as
  `Detached`, never quarantined. Detached sandboxes accept only `state`,
  `tail`, `fetch`, `signal`, and removal.
- **Nested calls, delegation off.** The refusal at start checks the block's
  words in command positions against registered Kits with a cloud form. It is
  best effort: the command links in the sandbox refuse such a call too
  (exit 126). Both messages name `marsh config cloud-delegate on` and the
  Keychain command.
- **Nested calls, delegation on.** The token is a Keychain generic password
  (service `marsh-cloud-delegate`, account = Docker ID), read with the stock
  `security` tool at the start of every detached cloud-shell run that can
  reach a registered Kit with a cloud form, not only one whose words name a
  Kit (the word check is best effort). A missing token refuses the run with
  the `security add-generic-password` command. It goes to the sandbox on the
  stdin of one root step (`--internal-cloud delegate`) into
  `/var/lib/marsh-cloud/delegate` (root, 0600); the request carries only a
  plan (`DelegatePlan`: the allowed names and their pinned cloud Kits, the cap,
  the time limit, the CPUs, the Docker ID). A direct Kit run (`cloud -d {
  claude -p ...; }`) never gets it: a Kit sandbox has no root to keep it from
  the workload.
- **Coordinator mechanism.** The supervisor downloads a pinned Docker
  Sandboxes CLI release (linux-arm64, checked by SHA-256) into the root-only
  state directory, signs it in with `sbx login --username ID
  --password-stdin` under a root-only `HOME`, checks `sbx --cloud ls`, and
  only then starts the workload, so a bad token refuses the run. It then
  serves the command links' socket with the laptop's own crossing code,
  moved into `marsh_daemon::cloud_call` and shared by both (create from the
  Kit with the workspace mixin, copy the run's current files in with
  `archive`/`unpack`, relay, confirm, capture, delete, `apply` as
  `marsh-run`). Calling the Sandboxes API directly from Rust was not chosen:
  it would need an HTTP/TLS stack in the static helper and a second
  implementation of Kit-based creation and exec that the stock CLI already
  provides.
- **Guardrails.** Enforced by the supervisor: names outside the plan exit
  127; the cap (default 8, `MARSH_CLOUD_CHILDREN`) counts admitted calls, and
  later ones exit 125; each agent sandbox gets what remains of the run's
  `--ttl` (at least one minute) with `--on-timeout stop`. Agent sandboxes get
  no command links, so they cannot call further agents (as in attached runs).
  Each is recorded in the coordinator's own ownership record
  (`/var/lib/marsh-cloud/children/cloud-ownership.json`) before it is
  created and deleted when its call returns; when the workload's leader
  exits, write-back is sealed, the workload stopped, running calls killed,
  and leftovers deleted before `status` and `done`. `state` lists finished
  calls and any agent sandbox still owned; the laptop keeps the last list,
  records the calls in the job's receipt (`cloud_children`), and deletes
  listed sandboxes (adopted into its own ownership, only by recorded name and
  ID) before deleting the run's sandbox on collect, discard, loss, or
  retention.
- **Notices.** Shown once on the next `marsh` command that talks to the
  daemon from a terminal (`marsh` shell start, `jobs`, `status`, `results`,
  `cloud`), not by the shell prompt.
- **Collection timing.** At daemon start and every 2 minutes while a run is
  outstanding. `MARSH_CLOUD_DETACHED_RETENTION` (daemon environment)
  overrides the 14-day retention, for testing.
