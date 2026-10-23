# Detached cloud work: close the lid, collect later

Status: **proposed.** Nothing here is implemented. It extends
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
cannot call back to the laptop (see §6).

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

## 4. Sandbox lifetime: results wait in a stopped sandbox

Detached runs are created with `--on-timeout stop`. A stopped cloud sandbox
keeps its disk, can be started again (`sbx attach` starts a stopped sandbox),
and costs no compute while stopped.

- `--ttl` (default `8h`) is the limit on running time. When it lapses, the
  sandbox stops in place. If the workload was still running, it is killed by
  the stop; `state` then reports `failed (time limit)` with whatever output
  and partial delta exist. The user can choose to collect that partial result.
- When the laptop sees `done`, it fetches the output and delta, then stops
  the sandbox (`sbx stop`). After a verified collect it deletes it.
- **Retention.** A finished, uncollected sandbox is kept for a retention
  period (default 7 days). marsh deletes it after a verified collect, after
  retention, or on `marsh cloud discard JOB`. Retention is measured on the
  laptop. If the laptop never returns, the platform's own retention of stopped
  sandboxes applies (to be confirmed, §9).

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

  The way to lift this is for the supervisor (§3) to take over the laptop's
  coordinator role, using a **sandbox-scoped capability** from Docker Cloud,
  injected at the proxy like any other credential: this sandbox may create
  child sandboxes only from named Kits, within a budget and a time limit, and
  they're deleted with it. Until the platform offers that, v1 refuses nested
  calls in a detached block at start. A one-command block that runs directly
  in a registered Kit (`cloud -d { claude -p … }`, the existing `--direct`
  form) works, because it has no nested call.
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
patch, `diff`/`apply`/`discard` work), with the time limit lapsing (partial
result reported), and with retention expiring.

## 9. Open questions to confirm with the platform

1. **Stopped-sandbox retention:** how long does Docker Cloud keep a stopped
   sandbox, and is its disk billed?
2. **Self-stop:** can the supervisor stop its own sandbox when `done` (to stop
   compute billing while the laptop is away), or does it run until its time
   limit? If not, a short default limit matters more.
3. **Billing while stopped**, for the docs.
4. **`sbx attach` vs `sbx start`** for a stopped sandbox in a script
   (non-interactive start).
5. **Network policy for long agent runs**: same as attached runs, confirm.
6. **Scoped child-sandbox capability:** can a cloud sandbox get a credential,
   injected at its proxy, that lets it create child sandboxes only from named
   Kits, within a budget and time limit, deleted with it? This decides whether
   detached blocks can call more agents (§6), and it's the same capability
   cloud-hosted agent teams need.

## 10. Order of work

1. Confirm §9 with the platform team.
2. Models (§8).
3. Remote supervisor, spool and `state`/`tail`/`fetch`, with VM-free tests.
4. Durable detached records, base manifest, collect and safe write-back.
5. CLI: `-d`, `attach`, `collect`, `diff`, `apply`, `discard`, `stop`;
   `marsh jobs` and `marsh status` show detached jobs; the notice on finish.
6. Live acceptance (§8), then docs (`docs/cloud.md`).
