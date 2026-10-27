# Local acceptance contract

This opt-in gate checks the local product in
`../../docs/plan/local-product-contract.md`. It is a black-box test: the
harness starts public `marsh` binaries, invokes the stock `sbx` CLI, and
inspects only the versioned public JSON described below. It does not read
daemon databases, sockets, process tables, Docker state, VM files, or Rust
implementation types.

## Runs

`tests/acceptance/sbxenvs_uat.py` exercises home-level native environment
additions: shared/per-Kit mounts, RO/RW bytes and receipts, child inheritance
without Kit widening, split omission, credential-free MCP echo through a real
job, restoration after worker reset, startup snapshots, and unsafe-config
refusals without VM changes. It is included in `make verify`/`make regress`.

`tests/acceptance/cloud_uat.py` is the opt-in live Docker Cloud family for
`cloud { }` / `marsh cloud` (C02-C06 and C09-C12 of the
[cloud placement design](../../docs/design/cloud-placement.md#10-observable-acceptance)).
It creates billed sandboxes, needs `--i-accept-cloud-charges` and a signed-in
account with cloud sandbox access, and is not part of `make verify`,
`make regress`, or the release run. Evidence and coverage:
[cloud dispatch](../../docs/plan/cloud-dispatch.md).

There are three ways to run the gate (`run.py` is the full gate; `smoke.py`
is its fast subset, sharing the same scope harness, `harness.py`):

- `make regress ONLY=smoke` runs the smoke subset against the incremental
  `make dev` build (`DEV_KIT` defaults to `make fixture-ref`'s reference). It needs no build receipt and
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
failure. `make regress ONLY=workspaces` runs the split/join scenarios against the
dev install.

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

The gate passes only when it observes all of the following. Each of
observations 1-18 names, in parentheses, the `run.py` checks (`CHECKS`) that
make it; observations 19-21 are the scenario suites indexed at the end.

1. (`cold-parallel-prewarm`, `prewarm-ready-and-subsecond`) `--load fixture` and `--load all` return with a warm worker and
   cached image; from one already-open project shell launched with `--load fixture`,
   three cached jobs launched concurrently reach their container gate within one
   second of dispatch. The result records the measured readiness time.
2. (`shared-daemon-two-shells`) Two overlapping shell processes report the same daemon identity.
3. (`reused-vm-distinct-containers`, `descendant-registered-command`) Sequential fixture invocations use the same warm kit VM and distinct real
   64-hex container IDs. A registered command invoked by a descendant `/bin/sh`
   through exported `PATH` follows the same fresh-container route.
4. (`concurrent-containers-one-vm`, `capacity-overflow-visible`, `client-disconnect-preserves-worker`) Two overlapping fixture invocations are active concurrently in that VM; an
   invocation beyond advertised capacity terminates with an actionable capacity
   rejection, and the worker never exceeds its advertised capacity.
   Killing one attached client cancels and cleans only that attempt; the same
   healthy worker remains reusable and is not durably quarantined.
5. (`natural-project-write`) A direct job atomically writes and updates the launch project's natural
   absolute path, while a write outside project/home grants fails.
6. (`persistent-and-ephemeral-home`) Default home changes persist across fresh containers; an
   `--ephemeral-home` change does not enter the persistent home.
7. (`exact-streams-argv-status`) Empty arguments, spaces, metacharacters, stdin, stdout, stderr, and a
   nonzero exit status survive byte-for-byte.
8. (`background-pipeline-job-control`) A registered-command pipeline runs in the background; `$!`, `jobs -p`,
   `jobs -l`, and `wait` agree and preserve the pipeline's nonzero status.
   `wait -n -p` identifies the first completed child and returns its status.
   `wait -n -f -p` keeps waiting through a stopped state and reports the
   terminating child's status and PID.
9. (`pty-color-resize-interrupt`, `blocked-stdin-interrupt`) A real PTY preserves color and paste bytes, receives resize, handles Ctrl-D,
   and lets Ctrl-C reach the job and return status 130 without hanging. A
   separate shell that never reads stdin receives sustained input backpressure;
   Ctrl-C still reaches it and returns status 130 within a bounded time.
   A noninteractive shell waiting on a foreground child runs its registered
   SIGTERM cleanup trap and returns the trap's exit status. A shell idle while
   reading command input also runs its registered SIGTERM cleanup trap.
10. (`resource-and-wall-enforcement`) The fixture observes the admitted CPU, memory, and PID ceilings;
    memory/PID/output/writable/wall pressure has a typed bounded outcome and
    CPU consumption is throttled. The writable-layer limit is detect-and-kill
    (the Kit VM's containerd overlayfs snapshotter ignores Docker's
    `--storage-opt size`, so there is no in-container ceiling to observe): a
    job that keeps writing 1 MiB + `fsync` rounds under a 16 MiB limit is
    killed with exit cause `limit:writable` within a couple of seconds, having
    overshot the limit by at most what it wrote in one sampling window.
11. (`verified-container-deletion`) Successful runs have verified deletion and disappear from every worker's
   active-container set.
12. (`uncertain-cleanup-quarantine`) Stopping the exact acceptance kit VM during a held job produces uncertain
    cleanup, a nonzero `cleanup_uncertain` receipt, quarantines that worker,
    prevents its reuse, and makes a following invocation fail with an
    actionable error until `marsh workers reset` retires it. Automatic
    replacement is out of scope.
13. (`no-runtime-authority`) The job reports no Docker/containerd/SBX/daemon socket and receives no
    runtime authority environment variable. Beyond the validated session
    identity variables, only the exact stock-SBX proxy endpoint, bounded
    non-secret credential-mode sentinels, validated read-only CA bundle, and
    the sandbox-local MCP Gateway URL plus sentinel name (only as a validated
    pair when enabled by stock SBX) may be added for a native workload using
    SBX credential and Gateway services. The loaded MCP server set belongs to
    the Kit VM, not to individual job containers.
14. (`useful-phase-timing`) Timing includes the eight observable phases and semantic milestones above.
    First output is present only when observed. Durations and milestones are
    not conflated, unavailable queue/placement evidence is not fabricated,
    orchestration and execution remain separate, and their sum is consistent
    with harness wall time.
15. (`stock-sbx-client-coexistence`) Stock-SBX listings retain every pre-existing resource of other SBX clients.
16. (`fanout-collect-composition`, `fanout-collect-bounds`) `fanout` accepts named registered-command pipelines, executes the branches
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
17. (`durable-results-view`) `marsh results` lists durable structural summaries in strictly newest-first
    cursor order. `results show` resolves the first displayed cursor to the
    exact versioned receipt, and both public views expose only their declared
    structural fields without prompt, stdin, stdout, or stderr content.
18. (`shell-sudo-apt-install`, `shell-private-docker-without-sudo`) The project shell user has noninteractive passwordless root only inside the
    shell VM, Debian package metadata is fetched over HTTPS, and a real package
    can be installed. The user belongs to the VM's `docker` group and can run
    Docker without `sudo` against that VM's private Engine. Its Engine identity
    differs from the host Engine; neither command grants stock-SBX or host
    Docker control.

19. Daemon-owned workspaces (`workspaces_uat.py`, the W scenarios below, specified in
    `docs/design/workspaces-acceptance.md`; run by `make regress`;
    one family: `make regress ONLY=workspaces`): `marsh split` / `join` /
    `splits` from host bash, sessions, and Kit jobs; each fork mirrors the
    user's `git status`; branch results appear only in `out/` as
    `git apply`-compatible patches; the user's tree and `.git` are never
    written; argv branches mount only their fork and read-only Git metadata,
    and admin tampering or a nested `.git` is rejected; creator-only join,
    the lease, the session pool, depth 3, cancel cascades, and a daemon crash
    mid-split (uncertain, untrusted, no replay).

20. Nested processes (`processes_uat.py`, the P scenarios below, specified in
    `docs/design/processes-acceptance.md`; run by `make regress`; `ONLY=processes`): every Kit
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

21. Shell choice (`shells_uat.py`, the S scenarios below, specified in
    `docs/shells.md`; run by `make regress`; `ONLY=shells`): for each of the shell VM's own
    `bash` and `zsh`, the chosen shell runs, registered names are Kit jobs with
    verified cleanup, split/join and fanout/collect work, the user's rc file is
    honored with the session command directory first on PATH, Ctrl-C returns
    to the prompt, statuses propagate, and `--shell` / `MARSH_SHELL` /
    `marsh config shell` precedence holds.

## Scenario index

Generated from the suites' `SCENARIOS` tables and scenario docstrings by
`python3 tests/acceptance/contract_index.py --write`;
`test_contract_index.py` fails when this section and the code disagree.

<!-- BEGIN GENERATED SCENARIO INDEX -->

### Acceptance gate checks (`run.py`, 25)

- `cold-parallel-prewarm`
- `prewarm-ready-and-subsecond`
- `shared-daemon-two-shells`
- `shell-sudo-apt-install`
- `shell-private-docker-without-sudo`
- `reused-vm-distinct-containers`
- `concurrent-containers-one-vm`
- `client-disconnect-preserves-worker`
- `capacity-overflow-visible`
- `natural-project-write`
- `descendant-registered-command`
- `persistent-and-ephemeral-home`
- `exact-streams-argv-status`
- `fanout-collect-composition`
- `fanout-collect-bounds`
- `durable-results-view`
- `background-pipeline-job-control`
- `pty-color-resize-interrupt`
- `blocked-stdin-interrupt`
- `resource-and-wall-enforcement`
- `verified-container-deletion`
- `uncertain-cleanup-quarantine`
- `stock-sbx-client-coexistence`
- `no-runtime-authority`
- `useful-phase-timing`

### Workspaces (`workspaces_uat.py`, 32)

- `W01-plain-bash`: s2 primary CLI from host bash: spool, -b and :::, join -- CMD, SPLIT_*, release.
- `W02-lease-kept-joinable`: s6 lease: unheld 60 s -> kept, still joinable by its creator (two-step).
- `W03-snapshot-fidelity`: s1/s8: fork mirrors H, I, worktree incl. untracked; ignored and .marsh absent; gitfile.
- `W04-snapshot-consistency`: s8 M2: user edits made while the split runs never reach the branch or its patch.
- `W05-patch-roundtrip`: s5/s8 patch writer: binary, mode, symlink, typechange, delete, add, quoting via git apply.
- `W06-non-git`: A plain directory splits; patches apply with `git apply`; Kit stays confined.
- `W07-status-pipefail`: s2: exit = first nonzero in declaration order; pipefail returns CMD's when nonzero.
- `W08-join-after-failure`: s2: join -- CMD runs after a failed branch with statuses; sugar keeps PIPESTATUS/SPLIT_*.
- `W09-retention`: s2/s6 release rules, `marsh status` counts, `marsh splits rm`.
- `W10-environment`: s2: exported env reaches -b; unexported, functions, set options, MARSH_/SBX_/DOCKER_ do not.
- `W11-kit-confinement`: s8 mounts/NoWriteToUserTree, s4 no daemon sockets, s5 receipts gain lineage.
- `W12-admin-tamper`: s8/s11: Kit writes admin config/HEAD/info/gitfile; host git on the fork runs nothing.
- `W13-nested-dotgit`: s8 capture verification: nested `.git` (any case) -> rejected and removed.
- `W14-hostile-user-repo`: s8 isolated gix: the user's fsmonitor/filters/hooks/diff.external/include never run.
- `W15-label-and-argv-refusal`: s2/s7 setup all-or-nothing: bad labels and unregistered argv -> 2, nothing runs.
- `W16-caps-and-pool`: s4: 16-branch cap (setup 2) and the 16-process session pool (`failed: capacity`).
- `W17-nested-from-kit`: s2/s3/s4: nested split via in-Kit shim (argv-only); ancestor join refused; sibling layout.
- `W18-depth-cap`: s4: depth 3 allowed (root=1), a depth-4 split is refused (2) and nothing runs.
- `W19-interrupt-subtree`: s2/s7: Ctrl-C cancels the subtree: shell branch, Kit branch, nested Kit job; join never runs.
- `W20-creator-exit-cascade`: s4/s7: a Kit branch that exits while its child split runs cancels that subtree.
- `W21-daemon-restart`: s6: restart mid-split -> uncertain, forks retained untrusted, nothing replayed.
- `W22-git-clean-replaced`: s6 hazard: `git clean -xfd` mid-split -> `workspace replaced`; nothing removed by path.
- `W23-non-creator-join`: s2/s3: a different session cannot join (or release) a split.
- `W24-performance`: Proposed budget: split overhead on a ~10k-file repo <= 1.5 s over a warm `marsh -c true`.
- `W25-planted-symlinks`: C1: symlinks planted in .marsh (by a Kit job or the guest) never redirect host writes.
- `W26-nested-repository`: H2: a nested repository (vendored or submodule checkout) splits; its .git is not copied.
- `W27-sugar-interrupt`: Brush sugar: Ctrl-C cancels the daemon-side split; join and later stages never run.
- `W28-quarantine-retire`: Owned quarantined or orphan-pinned Kit VMs are always retirable, with the cause printed.
- `W29-doc-examples`: Run marked public-documentation examples with declared fixture substitutions.
- `W30-capture-ignored`: s8 capture: what a branch creates that the project's ignore rules exclude (`__pycache__/` from
- `W31-shell-git-config`: Git uses the selected home, not SBX's agent-user config; session overrides still work.
- `W32-detached-split`: s4: `split -d` returns at once and outlives its client; join waits for it. A plain

### Processes (`processes_uat.py`, 37)

- `P01-job-surface`: s4: PATH/MARSH_JOB; bash, sh and SHELL are the image's own; registered names route; read-only binds.
- `P02-self-spawn-bash-c`: s1/s5/s9: a job's `bash -c 'fixture'` and `/bin/bash -c` become child jobs with lineage; `jobs --tree`.
- `P03-direct-execve`: s4 table/s5 child case: execvp of a registered name, its link path, and from /bin/sh and env.
- `P04-entry-once`: s5 entry case (EntryOnce, NoEntryRecursion): `exec fixture` from the entrypoint runs the image binary once.
- `P05-image-tool-local`: s5 image tool case: a registered name the image ships stays local in a job of another name.
- `P06-cross-kit-default`: s6: cross-Kit spawning is allowed by default; the child runs in its own Kit's VM.
- `P07-run-three-callers`: s3/s5: `marsh run` from the host, the shell VM, and a job: same bytes, status, stdin; lineage differs.
- `P08-fanout-in-job`: s1: `marsh fanout | marsh collect` from a job's real bash: shell-local branches, ordered frame;
- `P09-cli-split-real-bash`: s1/s7: `marsh split | marsh join` works from the image's real /bin/bash in a job; its view is the job's.
- `P10-env-i-links`: s4: a link and the in-job `marsh` start from job.json alone (`env -i`) and still spawn children.
- `P11-env-forwarding`: s6: a child gets only job-exported (set or changed) vars, never image ENV, unexported vars, or secrets.
- `P12-depth-cap`: s6 depth 4 (LineageWF): an alternating fixture/shell chain stops at depth 4 with exit 125.
- `P13-same-kit-chain`: s6 same-Kit chain 2 (SameKitBound): fixture -> fixture is allowed, a third fixture is refused.
- `P14-fan-out-cap`: s6 fan-out 8 (FanOutBound): a ninth live child of one job is refused fast; the eight run.
- `P15-pool-fail-fast`: s6 session pool 16 (BudgetBound): a nested 17th job is refused at once with `capacity: 16 jobs`; no deadlock.
- `P16-spawn-narrowing`: s6 narrowing only (SpawnSetAttenuates): MARSH_SPAWN/--spawn/--no-spawn narrow; nothing widens.
- `P17-branch-confinement`: s7: a split branch job's child gets the branch's mounts verbatim: the fork, never the project.
- `P18-tty-child-refused`: s6 no TTY children: a link whose stdin/stdout are terminals is refused with the stated message.
- `P19-ctrl-c-tree`: s8 Ctrl-C: SIGINT to the root caller cancels the whole tree across Kit VMs with verified deletion.
- `P20-parent-exit-cascade`: s8 job end: a parent that exits while its child runs cancels the child (no orphan).
- `P21-daemon-restart`: s8 restart (RestartUncertain, NoReplay): a live tree becomes uncertain; nothing is resubmitted.
- `P22-cap-socket-cap`: s3 socket fixes: >= 12 concurrent connections admitted; excess ones get a refusal frame, not a drop.
- `P23-awareness-context`: s9: `marsh context` prints (never writes) the job context; job.json/context.md; status `nested`.
- `P24-real-bash-startup`: s4: PATH `bash` is the image's own (no wrapper: p50 within 2 ms of /bin/bash); the static
- `P25-child-after-first-split`: s7: a job started before the project's first split still spawns children after it splits.
- `P27-no-shell-branch-from-job`: s13.6 (B2): a job's split takes argv branches only; the daemon refuses shell branches (-b).
- `P28-env-whole-tree`: s6 (B3): a root export reaches every depth; a child's re-export of the same value still flows.
- `P29-root-interrupt-cascade`: s8 (B4): Ctrl-C at the root cancels the whole tree at once (< 15 s), all `cancelled`, root 130.
- `P30-inspection-text`: s9 text forms: `jobs --tree` (exit, duration), `jobs show` (lineage, children), results, status.
- `P31-agents-doc-examples`: docs/agents.md: every marked example runs as written (agents replaced by fixture callers).
- `P32-live-claude-codex`: Billed (--live): a real Claude Code Bash-tool turn runs bash and a codex child job, with lineage. (billed; only with `--live`)
- `P33-jobs-listing-scope`: s9 listings: in a session `jobs`/`jobs --tree` show only that session's trees, `--all` every one.
- `P34-background-cold-notice`: Interactive: `NAME &` prints Bash's `[1] PID`, and a background job's cold start is silent;
- `P35-wall-inheritance`: s6 wall time: a child's budget is min(its own limit, the parent's remaining time); it is
- `P36-tree-kit-vm-cap`: s6 Kit VMs per tree: a child needing one more distinct Kit VM than the cap is refused
- `P37-split-lineage-tree`: s9: in the session, `jobs --tree` draws its split, each branch, and every job a branch's shell
- `P38-fanout-cli`: s1: `marsh fanout | marsh collect` from host bash and in a session: concurrent branches on the shared

### Shells (`shells_uat.py`, 6)

- `S01-identity-and-status`: `-c` runs in the chosen shell; its exit status and signal status propagate; PATH starts with the
- `S02-registered-command-job`: A registered name from bash/zsh is a session-root Kit job with a verified-cleanup receipt; `marsh run`
- `S03-split-join`: `marsh split -n -b a='echo x' ::: b fixture identity | marsh join` from bash/zsh.
- `S04-fanout-collect`: `marsh fanout ... | marsh collect` from bash/zsh: ordered frames, first failing status.
- `S05-interactive-rc-ctrl-c-exit`: Interactive bash/zsh on a PTY: the user's rc alias works, the session bin stays first on PATH
- `S06-selection-precedence`: `marsh config shell` sets the home default; MARSH_SHELL overrides it; --shell overrides both;

<!-- END GENERATED SCENARIO INDEX -->
