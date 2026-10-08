# Local acceptance contract

This opt-in gate checks the local product in
`../../docs/plan/local-product-contract.md`. It is a black-box test: the
harness starts public `marsh` binaries, invokes the stock `sbx` CLI, and
inspects only the versioned public JSON described below. It does not read
daemon databases, sockets, process tables, Docker state, VM files, or Rust
implementation types.

## Runs

There are three ways to run the same harness (`run.py`; `smoke.py` is an entry
point to it):

- `make dev-smoke DEV_KIT=<fixture-ref>` runs the smoke subset against the
  incremental `make dev` build in `target/`. It needs no build receipt and
  is the everyday development check. Its evidence is useful but is not release
  evidence.
- `make acceptance-smoke` builds a fresh candidate in a private directory
  outside the checkout, records a build receipt, and runs the smoke subset.
- `make acceptance` builds a fresh candidate the same way and runs every
  observation below. This is the release run.

When a build receipt is supplied the harness verifies it against the source
tree and artifacts before creating any scope; without one (dev runs) that check
is skipped and the evidence records no receipt. The smoke subset covers
prewarm, fresh registered-command containers, VM reuse, receipts, verified
deletion, daemon sharing, natural paths, persistent home, streams, pipelines,
background jobs, signals, fanout/collect, and results. It stops at the first
failure. `make dev-split` runs the split/join observation (19) against the dev
install.

The release and smoke targets use the fixture OCI reference from
`target/kit-release/fixture-commands.json` (written by
`make kit-publish-fixture KIT_REPOSITORY_PREFIX=REGISTRY/NAMESPACE`), else
`target/fixture-ref` (written by `make fixture-ref` from the latest release's
`fixture-ref.txt`; release CI publishes `docker.io/mcavage/marsh-fixture`), or
`ACCEPTANCE_KIT=repository@sha256:...`. The registry must be allowed by stock
SBX. Local source and mutable tags do not satisfy the release run. Normal
`make test` is independent of these stock-SBX runs.

## Environment

The gate keeps durable evidence under the caller-selected workspace evidence
directory. Its isolated `MARSH_HOME` and project directory live in a separate
system temporary root (`/private/tmp` on macOS), outside the user's natural
home and outside the evidence tree. This prevents the project grant from
accidentally inheriting visibility through the selected-home path. Disposable
state is removed after the run; evidence remains available to
reviewers. The gate never uses provider credentials. The default source under
`fixture/` is validated and assembled by the stock Kit v3 frontend; its final
ARM64 image uses a DHI sandbox template and contains the compiled Rust fixture
executable. The harness may instead receive another source directory or
immutable `repository@sha256:<64 lowercase hex>` reference through `--kit`.
The configured `--sbx` executable is resolved before the run, exported to the
product as `MARSH_SBX`, used for every direct harness probe, and recorded by
absolute path. A missing or non-executable override fails before UAT starts.
The three packaged ARM64 guest binaries are resolved before UAT and recorded
by SHA256. A standalone host-binary directory must set
`MARSH_GUEST_ARTIFACTS` to its matching guest-artifact directory.

The harness creates an owner-only host control root outside both guest mounts,
sets `MARSH_CONTROL_HOME` to that root, and writes its minimal `commands.json`
under the SHA256 scope of the canonical selected-home path. The resulting
registry, Kit lifecycle, and structural journal remain isolated from another
selected home using the same override root. The mapping binds the arbitrary
command `fixture` to that native Kit source or immutable reference. It carries
no image config, argv, policy, capability, or resource copy.
Product code must not recognize the fixture name specially.

The harness owns every resource under its isolated `MARSH_HOME`. It may stop
only a VM identity reported for that home and kit profile (a random
`marsh-k-`/`marsh-s-` name from that daemon's ownership map). It never changes
another SBX client's resources.

The isolated daemon receives explicit positive decimal job ceilings through
the generic `MARSH_JOB_{CPU_MILLIS,MEMORY_BYTES,PIDS,WRITABLE_BYTES,
OUTPUT_BYTES,WALL_SECONDS}` settings. The evidence records those values, and
resource assertions derive their expected cgroup values from the same map.

## Public interfaces under test

The product exposes these public commands:

```text
marsh status --json
marsh jobs [--json]
marsh jobs show JOB_ID [--json]
marsh results [--json]
marsh results show CURSOR|JOB [--json]
marsh stop [--json]
marsh reset [--json]
marsh [-c COMMAND]
marsh --ephemeral-home [-c COMMAND]
sbx ls
sbx stop VM_ID
```

`status` returns `schema: "marsh.status/v1"` and includes the feature
`direct-command-acceptance-v1`. This handshake prevents a missing product from
producing a confusing cascade of test errors.

`stop` removes the selected home's owned VMs and stops its daemon; `reset`
removes them and keeps the daemon. Both refuse while shells or jobs are active.
Each prints one stderr line naming the VMs the daemon reported removed, one line
per VM it left with the next step, and exits 1 unless every cleanup was
verified. With no daemon and no VM recorded for the home, each prints
`marsh: nothing running for HOME` and exits 0 without starting anything. Like
`status`, these words are subcommands only as the first operand; a script named
`stop` runs as `marsh ./stop`.

`status` returns the acceptance scope, daemon, attached shells, and workers.
`jobs` without `--json` gives a readable summary from the host or attached
project shell; `jobs show` gives a readable receipt. With `--json`, `jobs`
returns `schema: "marsh.jobs/v1"` and stable job IDs. `jobs show`
returns each `schema: "marsh.job/v1"` receipt. Human and JSON views read the
same scope-wide structural journal, including active jobs. `results` is an
alias for this journal. Together they expose:

- one acceptance scope, daemon identity, endpoint-owner identity, and attached
  shell sessions;
- workers with distinct worker and VM identities, kit profile, warm state,
  owning scope, health, bounded container capacity, and active real container
  IDs; and
- runs with distinct job, attempt, session, worker, VM, and real container
  identities; command name; terminal state and exit/cause; cleanup state; and
  orchestration phase durations.

For every state snapshot, the harness independently runs read-only
`sbx exec VM_ID docker inspect CONTAINER_ID`. Inspection must succeed for every
reported active container and fail for every container whose cleanup is
reported verified. Product metadata alone is never accepted as proof of a real
container identity or deletion.

Run timing has two distinct maps. `durations_ms` contains eight contiguous,
observable phases: `vm_prepare`, `admission`, `mount_prepare`, `worker_start`,
`execution`, `output_drain`, `result_capture`, and `cleanup`.
`milestones_unix_ms` contains the semantic observations `request_received`,
`worker_progress`, `process_exit`, `output_drained`, and `completed`, plus
`first_output` only when output was observed. Implementations do not invent
unobservable queue or placement durations and do not clamp reordered clocks.
Milestones must be monotonic where causally ordered, durations must be
nonnegative, and their sum equals `wall_ms`. The separately reported
`orchestration_ms` equals all durations except `execution`.

These views are public diagnostic metadata. They contain no raw credentials,
environment values, prompt content, or daemon/runtime socket path.

## Required observations

The harness records each command, byte-exact standard streams, status, elapsed
time, selected state snapshots, and stock-SBX output in `result.json`.
`result.schema.json` defines the durable evidence format. The harness reads
that schema and validates the completed file before reporting success.
The environment records the source commit, an explicit dirty-worktree flag,
and a SHA-256 identity over every tracked or unignored source file. This keeps
evidence from a useful in-progress candidate honest without requiring a commit
before each UAT run. `acceptance-smoke` records the same source identity.

The gate passes only when it observes all of the following:

1. `--load fixture` and `--load all` return with a warm worker and
   cached image; from one already-open project shell launched with `--load fixture`,
   three cached jobs launched concurrently reach their container gate within one
   second of dispatch. The result records the measured readiness time.
2. Two overlapping shell processes report the same daemon identity.
3. Sequential fixture invocations use the same warm kit VM and distinct real
   64-hex container IDs. A registered command invoked by a descendant `/bin/sh`
   through exported `PATH` follows the same fresh-container route.
4. Two overlapping fixture invocations are active concurrently in that VM; an
   invocation beyond advertised capacity terminates with an actionable capacity
   rejection, and the worker never exceeds its advertised capacity.
   Killing one attached client cancels and cleans only that attempt; the same
   healthy worker remains reusable and is not durably quarantined.
5. A direct job atomically writes and updates the launch project's natural
   absolute path, while a write outside project/home grants fails.
6. Default home changes persist across fresh containers; an
   `--ephemeral-home` change does not enter the persistent home.
7. Empty arguments, spaces, metacharacters, stdin, stdout, stderr, and a
   nonzero exit status survive byte-for-byte.
8. A registered-command pipeline runs in the background; `$!`, `jobs -p`,
   `jobs -l`, and `wait` agree and preserve the pipeline's nonzero status.
   `wait -n -p` identifies the first completed child and returns its status.
   `wait -n -f -p` keeps waiting through a stopped state and reports the
   terminating child's status and PID.
9. A real PTY preserves color and paste bytes, receives resize, handles Ctrl-D,
   and lets Ctrl-C reach the job and return status 130 without hanging. A
   separate shell that never reads stdin receives sustained input backpressure;
   Ctrl-C still reaches it and returns status 130 within a bounded time.
   A noninteractive shell waiting on a foreground child runs its registered
   SIGTERM cleanup trap and returns the trap's exit status. A shell idle while
   reading command input also runs its registered SIGTERM cleanup trap.
10. The fixture observes the admitted CPU, memory, and PID ceilings;
    memory/PID/output/writable/wall pressure has a typed bounded outcome and
    CPU consumption is throttled. The writable-layer limit is detect-and-kill
    (the Kit VM's containerd overlayfs snapshotter ignores Docker's
    `--storage-opt size`, so there is no in-container ceiling to observe): a
    job that keeps writing 1 MiB + `fsync` rounds under a 16 MiB limit is
    killed with exit cause `limit:writable` within a couple of seconds, having
    overshot the limit by at most what it wrote in one sampling window.
11. Successful runs have verified deletion and disappear from every worker's
   active-container set.
12. Stopping the exact acceptance kit VM during a held job produces uncertain
    cleanup, a nonzero `cleanup_uncertain` receipt, quarantines that worker,
    prevents its reuse, and makes a following invocation fail with an
    actionable error until `marsh workers reset` retires it. Automatic
    replacement is out of scope.
13. The job reports no Docker/containerd/SBX/daemon socket and receives no
    runtime authority environment variable. Beyond the validated session
    identity variables, only the exact stock-SBX proxy endpoint, bounded
    non-secret credential-mode sentinels, validated read-only CA bundle, and
    the sandbox-local MCP Gateway URL plus sentinel name (only as a validated
    pair when enabled by stock SBX) may be added for a native workload using
    SBX credential and Gateway services. The loaded MCP server set belongs to
    the Kit VM, not to individual job containers.
14. Timing includes the eight observable phases and semantic milestones above.
    First output is present only when observed. Durations and milestones are
    not conflated, unavailable queue/placement evidence is not fabricated,
    orchestration and execution remain separate, and their sum is consistent
    with harness wall time.
15. Stock-SBX listings retain every pre-existing resource of other SBX clients.
16. `fanout` accepts named registered-command pipelines, executes the branches
    in parallel, and `collect --timing` renders their output in declaration
    order with truthful branch and total timings. Branches are label-led: a
    `;` starts a branch only before a `LABEL:` word, so `ok: X; bad: Y` is two
    branches and an unlabeled `;` sequences commands inside one. A failed
    registered branch keeps its diagnostics on stderr and remains nonzero
    through `collect`, a later pipeline stage, and `pipefail`. Warm
    local-command orchestration, with `--load fixture` before dispatch,
    completes in under one second. A
    one-second two-branch probe reports a total at least as large as either
    branch but materially smaller than their sum. Completed input is capped at
    64 MiB before a registered branch starts, and branches share a 16 MiB
    combined stdout/stderr capture budget; either
    breach cancels and reaps the branches and renders no partial collection.
17. `marsh results` lists durable structural summaries in strictly newest-first
    cursor order. `results show` resolves the first displayed cursor to the
    exact versioned receipt, and both public views expose only their declared
    structural fields without prompt, stdin, stdout, or stderr content.
18. The project shell user has noninteractive passwordless root only inside the
    shell VM, Debian package metadata is fetched over HTTPS, and a real package
    can be installed. The user belongs to the VM's `docker` group and can run
    Docker without `sudo` against that VM's private Engine. Its Engine identity
    differs from the host Engine; neither command grants stock-SBX or host
    Docker control.

19. Daemon-owned workspaces (`workspaces_uat.py`, scenarios W01-W24 in
    `docs/design/workspaces-acceptance.md`; run by `make dev-workspaces`,
    `make dev-split`, and `make dev-acceptance`): `marsh split` / `join` /
    `splits` from host bash, sessions, and Kit jobs; each fork mirrors the
    user's `git status`; branch results appear only in `out/` as
    `git apply`-compatible patches; the user's tree and `.git` are never
    written; argv branches mount only their fork and read-only Git metadata,
    and admin tampering or a nested `.git` is rejected; creator-only join,
    the lease, the session pool, depth 3, cancel cascades, and a daemon crash
    mid-split (uncertain, untrusted, no replay).

20. Nested processes (`processes_uat.py`, scenarios P01-P38 in
    `docs/design/processes-acceptance.md`; run by `make dev-processes`): every Kit
    job's read-only `/run/marsh` (links, `job.json`, the job's own
    `context.md`, `cap.sock`), the image's own `bash`, `sh`, and `$SHELL`
    (no bash link), the entry rule (the own name runs once), image-provided names
    staying local, registered names and `marsh run` from any caller as child
    jobs with lineage, CLI splits and `marsh fanout | marsh collect` inside
    jobs and from the host and session, environment
    forwarding of job-exported variables only, the depth, same-Kit, fan-out,
    and pool caps (fail fast), spawn-set narrowing, branch confinement,
    refused TTY children, Ctrl-C and parent-exit cascades with verified
    deletion, a daemon restart mid-tree (uncertain, no replay), the
    capability connection cap with a refusal frame, `marsh context`, and
    PATH `bash` timing equal to `/bin/bash`.
