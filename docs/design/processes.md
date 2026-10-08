# Processes: one shell, one protocol, one lineage tree (design)

Status: built (revision 5). Revision 3 adopted every fix and cut from the
adversarial review of revision 2 (section 12); revision 4 records the GM
resolutions of the acceptance author's spec gaps (section 13) and what the
implementation defers (section 14); revision 5 (section 15) removes the job
shell: Kit jobs keep the image's own `bash` and `sh` (operator decision), and
adds the `marsh fanout | marsh collect` CLI and per-job agent context. Model: `model/Process.tla`. Builds on
`workspaces.md` (split, capability socket). Acceptance:
`processes-acceptance.md`.

Goal: any process in any Kit, at any depth, can run a registered command,
`marsh run`, or `marsh split`. Each such command, and each split argv branch,
becomes a fresh marsh job. The daemon places it, records it under its
parent, charges it to one budget, and cancels it with its ancestors.
Nothing is per agent: Kit images and entrypoints are unchanged.

## 1. Principle: one protocol, many authorities

**One protocol.** The marsh shell runs where marsh starts a shell: the root
shell in the shell VM and split shell branches. Inside a Kit job every shell
is the image's own (section 15). Every client (the session shell, the `marsh`
CLI, and the per-name links) speaks one request protocol
(`Envelope<PublicRequest>`) over one local socket.

**Many authorities.** The authority is the only thing that differs, and the
daemon binds it to the socket:

| Caller | Socket | Authority | Parent of what it starts |
|---|---|---|---|
| Root shell / shell branch | relay | session (a branch is scoped to its split) | `session` / `split:<id>/<label>` |
| Any process in a Kit job | `/run/marsh/cap.sock` | job: its view, its spawn set, its tree's budget | `job:<id>` |
| Host CLI | daemon socket | master token plus an ephemeral session | `session` |

**What works in which shell:**

- **Registered commands and the `marsh` CLI work in every shell.** That
  covers `marsh run`, `split`, `join`, `fanout`, `collect`, `jobs`, and
  `context`. They are executables first on PATH: the links in
  `/run/marsh/bin`, the session command dir in the shell VM. So they work
  from real bash, dash, busybox, zsh, `env`, Node, Python, or a direct
  `execvp`.
- **Brush sugar works only in the session shell.** That is `fanout { } |
  collect`, `split { } | join`, and in-process dispatch. Every sugar form has
  a CLI form with the same semantics (`marsh fanout ... | marsh collect`,
  `marsh split ... | marsh join`), so a job, or a plain bash or zsh front end,
  keeps every capability and loses only the syntax.

**What collapses:**

- `marsh-local` becomes a mode of the one static binary.
- The split-only capability allowlist becomes one authority table
  (section 3).
- The split pool becomes process admission (section 6).

## 2. Process model

```
session ─ job ─ job ...            (registered name or marsh run: own container)
        │     └ split ─ branch job (argv branch: a child with a fork view)
        └ split ─ shell branch ─ job ...
```

- **Identities.** Session, job, attempt, worker, VM, container, split, and
  branch ids stay distinct. A job is one attempt and is never retried.
- **Records.** Each job record gains `parent`, `root`, `depth`, `spawn` (the
  names it may start). They are journaled before `mark_execution_submitted`.
- **What creates a node.** Only registered names, `marsh run`, and split
  branches do. Shell-local concurrency inside a job (pipelines, `&`,
  `marsh fanout` branches) stays in that job's container and is charged to
  it; a fanout branch that names a registered command starts a child job.
- **Capability.** Each job has one: a daemon record `{job, rights,
  generation, live|revoked}`, never a bearer secret. It is delivered
  through the worker's per-attempt socket (`marsh-worker/src/capability.rs`,
  backend `CapabilityBridges`). Today only split branches get that socket
  (`split_capability` in `marsh-backend/src/lib.rs`).

## 3. Requests by authority

| Request | Session (relay) | Job (`cap.sock`) |
|---|---|---|
| `ProcessRun {argv, cwd, env, spawn}` + stdin → `job`, then `Execute` frames | yes | yes; `spawn` may only narrow; a dropped stream cancels the child |
| `SplitCreate` / `SplitJoin` | yes | yes; the creator is the job and the snapshot is the job's view |
| `ProcessShow` | the session's forest | the job's own subtree |
| `SplitCancel`, `jobs cancel`, `splits rm` | yes, within its tree | no (drop the stream instead) |
| everything else (MCP, ACP, dev broker, lifecycle) | as today | refused |

**`ProcessRun`** is admission (section 6), then an internal `Execute`
through the daemon's own endpoint (as split argv branches do in `split.rs`),
then frame relay.

- **Ordering against cancel.** Under the process-table lock, the daemon
  re-checks that the parent's capability is live and not cancelling,
  inserts the child record, and only then submits. A cancel that takes the
  lock first sees no child. A cancel that takes it later finds the record
  and cancels the child (`Spawn` is atomic in the model).
- **stdin.** stdin is credit-windowed: the daemon grants 256 KiB and
  replenishes it as the child's worker consumes. The client stops reading
  its stdin at zero credit, so the agent's pipe sees backpressure and
  nothing is buffered without bound. stdout and stderr keep today's
  per-attempt credits.

**Capability socket fixes** (in `capability.rs`):

- Admit at least `MaxFan + 4` (8) connections per attempt, up from 4.
- Answer an excess connection with a refusal frame (`marsh: too many
  concurrent daemon requests in this job`). Today it is silently dropped.
- Replace the 50 ms polled accept with a blocking accept.

## 4. The links and the in-job `marsh`

**Artifact.** Build `marsh` as a static musl binary, like `marsh-worker`
already is. The guest `marsh` is glibc-dynamic today. Copy it to each Kit
VM's local disk alongside the worker binaries, and bind it read-only at
`/run/marsh/marsh`. A runtime test checks that the bind is read-only.

**Per attempt**, inside the existing capability dir `/run/marsh/`, which is
root-owned and read-only to the job:

- `cap.sock`.
- `job.json`: job id, own command name, spawn set, limits, socket path, the
  names and salted value digests of the container's starting environment,
  and the names it received from its parent (format in section 13).
- `context.md`: this job's agent context (section 9), generated by the
  worker from the shared text with the job's name and spawn set.
- `bin/`: one two-line stub per name. Stubs exist for `marsh` and every
  registered name (a name outside the spawn set is refused by its link,
  section 13); never for `bash` or `sh`. For example
  `#!/run/marsh/marsh --link=codex`. The kernel passes the mode and the stub
  path to the binary, so the mode comes from the kernel and the link name,
  never from a caller-supplied argv0. `/proc/self/exe` confirms that the
  process is the artifact.

**Environment:**

- `PATH=/run/marsh/bin:` followed by the PATH from the image config. The
  worker reads only the image config (`Env`); there is no probe container.
- `MARSH_JOB` and `MARSH_ENTRY=1`. `SHELL` is left as the image sets it.

**Nothing in the image is shadowed.** `bash`, `sh`, `/usr/bin/*`, dpkg, and
the entrypoint stay the image's own. There is no `bash` or `sh` link.

**What each caller gets:**

| Caller | Runs |
|---|---|
| any shell (`bash -c`, Codex's `bash -lc`, `$SHELL`, `/bin/sh`, `system(3)`, Node `spawn('bash')`, Python `shell=True`) | the image's own shell |
| a registered name from any of them, or a direct `execvp` | its link (below) |
| `marsh VERB` (`run`, `split`, `join`, `splits`, `fanout`, `collect`, `jobs`, `context`) | the in-job CLI (the artifact's `marsh` link) |
| an absolute image binary (`/usr/local/bin/codex`) | that binary, in this container, with this job's authority |

**The in-job `marsh`** starts from `job.json` alone (no `USER`, `HOME`, or
daemon), so `env -i /run/marsh/bin/codex …` and `env -i /run/marsh/bin/marsh
jobs` work. It connects to `cap.sock` per request. `type codex` in the
image's bash prints `codex is /run/marsh/bin/codex`. A login profile that
resets PATH hides the links; `marsh run NAME` and the absolute link path
still work.

## 5. The links: name resolution and the job's own command

A link `L` invoked for name `N` works as follows.

1. **Entry case.** If `N` is the job's own name, `MARSH_ENTRY=1` is set,
   and `getppid()` is docker-init (pid 1; the worker always passes
   `--init`), the link:
   - unsets `MARSH_ENTRY`;
   - resolves `N` by searching PATH with `/run/marsh/bin` excluded;
   - `exec`s that image binary by absolute path.

   PATH stays intact, so registered names the agent's later shells run
   still route through their links.

   The worker sets the marker for every entrypoint. This works for any
   entrypoint: a `#!/bin/sh` script's `exec claude`, real bash, busybox,
   `env`, tini, or an exec-form `ENTRYPOINT ["claude"]` that Docker resolves
   through PATH.
2. **Image tool case.** Otherwise, if `N` is not the job's own name and the
   image provides `N` on its PATH without `/run/marsh/bin`, the link execs
   the image's `N`. A registered `python` or `node` therefore stays local
   inside an image that ships it.
3. **Child case.** Otherwise the link sends `ProcessRun` and relays stdio
   and exit status.

`marsh run N` always takes the child case.

So the unchanged claude entrypoint (`exec claude …`) starts the real
`claude` once. Claude's Bash tool running `claude -p` is a new process whose
parent is not docker-init, and its marker is gone anyway. It gets a child
claude job.

**ELF entrypoints.** Docker execs an absolute ELF entrypoint directly, so
the marker stays in its environment. That is harmless: its descendants'
parent is not docker-init.

**Assumption A6** (stated, not modeled). An orphan reparented to docker-init
that still carries the marker, or a job that forges the marker, can only
run its own image binary in its own container. That is a lineage miss, not
an authority change. Authority is enforced at the daemon.

**Model.** `EntryOnce` (the own name resolves locally at most once per job)
and `NoEntryRecursion` (an entrypoint never spawns its own Kit), with
controls `linkIgnoresMarker` and `linkIgnoresPpid`. Every job's first step
is the entry step: an ELF exec, or a script exec through the link.

**Self-update.** `claude update` inside a job is an own-name call, so it
runs as a child job. It can write only what that job's mounts allow, which
includes the selected home. Kit images pin versions; a container-local
update is discarded when the job's container is deleted.

## 6. Authority and budgets

**Rights** are `(view, spawn set)`. A child gets its parent's recorded
rights or fewer, never rights recomputed from the session's grants. The
view is checked by `Attenuation` (control `widen`) and the spawn set by
`SpawnSetAttenuates` (control `spawnWiden`).

**Cross-Kit spawning is allowed by default (operator decision).** Any job
may spawn any registered command. The user's command registry is the trust
boundary: registering a Kit means trusting it to be started by any job in
any session.

**A spawned child runs under its own Kit's policy, not its parent's.** It
gets its own Kit's SBX network egress policy and SBX-proxied credentials,
in its own VM. A claude job that runs `codex` thereby causes a container
with OpenAI egress and credentials to run, with the same files as the
claude job. A prompt-injected agent can use any registered Kit's egress in
this way. To prevent that, narrow the spawn set.

**Narrowing only.** These set a smaller spawn set:

- `MARSH_SPAWN=a,b`, exported in the root shell and read at invocation like
  `MARSH_PLACE`;
- `marsh run --spawn a,b`;
- `--no-spawn` or `MARSH_SPAWN=none`: the empty spawn set. The socket and
  the links stay (splits and `jobs` still work); admission refuses every
  spawn.

A child's set is its parent's set or a subset of it. Nothing can widen it,
from the cap socket or from the session (`SpawnSetAttenuates`, control
`spawnWiden`). Raw secrets never cross the socket: frames carry argv, env,
and bytes.

**Environment for children.** A child receives the variables its parent
received from *its* parent (or, for a root job, the user's exported shell
variables), plus those the parent set or changed. So a variable the user
exports reaches the whole tree, and a child re-exporting a received name
with the same value still passes it on. Image `ENV` is never forwarded
unless the job changed it, and the split filter (`workspaces.md` s17) drops
credential-shaped names daemon-side.

- The worker records the received names in `job.json` (`forwarded`, from
  the request's environment) and, for every starting variable, an
  HMAC-SHA256 of `NAME=VALUE` under a per-daemon random key that the worker
  receives in the capability and never writes into the container. The job
  therefore cannot test guesses of a value against `job.json`.
- The link offers every variable except Docker-set ones (`""` digests), with
  the starting digests of offered names it did not receive. The daemon
  (`ProcessRun`, and `SplitCreate` from a job) drops an offered variable whose
  value still matches its digest. A forged offer can only forward the job's
  own variables, which it could do by changing them.
- Limitation: a job that sets an image `ENV` variable to its image value is
  indistinguishable from not setting it; that variable is not forwarded.

The child gets its own image's environment.

**Budgets.** One admission covers top-level jobs, Run children, and split
branches. It fails fast and never queues, because a parent waiting on a
queued child would deadlock the pool.

| Limit | Value | Refusal (exit 125 / `failed: …`) |
|---|---|---|
| Pool | 8 concurrent jobs per daemon (selected home): one pool across all sessions and host CLI calls, all depths; a slot frees only on verified deletion (`BudgetBound`, control `freeOnRevoke`) | `capacity: 8 jobs (2 held by uncertain jobs on Kit codex: run marsh workers reset codex)` |
| Kit VMs | 4 distinct Kit VMs live at once per job tree (one warm VM per Kit identity: aliases of a live Kit, or a second call to it, cost nothing; `MARSH_TREE_KIT_VMS` at daemon start sets 1–64; `KitVMBound`, `noKitCap`) | `shell refused: tree already uses 4 Kit VMs (claude, codex, fixture, pi) (MARSH_TREE_KIT_VMS)` |
| Depth | 4; a root job is depth 1 (`LineageWF`, `noDepth`) | `depth limit 4` |
| Same-Kit chain | 2 consecutive parent→child steps with the same Kit identity (Kit source/image, not the registered name): claude → claude is allowed, a third claude is refused (`SameKitBound`, `noSameKit`) | `claude → claude → claude refused: same-Kit chain limit 2 (see /run/marsh/context.md)` |
| Fan-out | 4 live children per job, including its splits' branches (`FanOutBound`, `noFan`) | `fan-out limit 4` |
| Total | 64 launches per top-level tree (`TotalBound`, `noTotal`) | `total limit 64` |
| Wall time | min(the Kit's own limit, `MARSH_JOB_WALL_SECONDS`, and the parent's remaining time); a child needs 1 s left at admission. The receipt's `lineage.deadline_unix_ms` is the effective deadline and `parent_deadline` says it is the parent's. The worker stops the child at it (`exit.cause` `limit:wall: parent deadline`); if the parent's own wall kill lands first, its end cancels the child with the same cause | `fixture refused: deadline: parent job 1a2b3c4d has 420 ms of wall time left (MARSH_JOB_WALL_SECONDS)` |

**No TTY children in v1.** A job's child gets no PTY. A `ProcessRun` whose
link has a terminal on stdin *or* stdout is refused with `marsh: interactive
child jobs are not supported yet: NAME has a terminal on stdin or stdout;
run it non-interactively with neither attached, e.g. \`NAME ... </dev/null |
cat\``. PTY frames on `cap.sock` come later.

**Refusals** happen at admission, before any record: a refused spawn leaves
no receipt, is printed on the caller's stderr, exits 125 (126 from a link
refused by the spawn set), and is counted in `marsh status`
(`processes.refused`).

## 7. Workspace inheritance

- **View.** A child's mounts are its parent's recorded `PublicMount` list,
  copied verbatim and never re-derived from the cwd or the session grants;
  each is bound from the prepared grant that covers it (as a subpath when
  narrower). The one addition is narrowing: a project `.marsh` created after
  the parent started is bound read-only.
  A top-level job's child sees the project and the selected home. A branch
  job's child sees, of the project, only the fork and its Git metadata
  (`split_confinement.rs`), plus the selected home read-write as every Kit
  job does (agents keep credentials and session state there).
- **cwd.** The child's cwd must lie inside those mounts.
- **Narrowing.** Only splits narrow a view, and nothing widens one.
- **Splits.** A split created by a job snapshots the job's view root.
  Capture waits for the creating branch's whole subtree (`workspaces.md` s17).
- **No new writable paths.** The artifact and `/run/marsh` are read-only
  binds.

## 8. Cancellation, revocation, restart

- **Job end.** A job ends by exit, cancel, or VM transport loss. Its end
  revokes the capability of the job and all its descendants in one step,
  then sends INT, waits 10 s, and sends KILL to each descendant container,
  and verifies deletion in every VM (`LiveUnderLive`, `NoOrphanRunning`,
  `CancelPropagates`; controls `orphanChildren`, `revokeOnly`,
  `noPropagate`). This generalizes `cancel_children_of` in `split.rs`.
- **Lost parent.** It becomes `cleanup_uncertain`, and its children are
  cancelled.
- **Ctrl-C.** Daemon-driven. In the root shell, the client's `INT` (or
  `TERM`/`HUP`) signal frame, or its disconnect, makes the daemon cancel the
  job's whole tree at once (`DaemonStore::cancel_tree`): every live
  descendant is marked cancelling (admission refuses its spawns) and gets
  `INT` in its own container now, everything still running gets `KILL`
  after 10 s, and each executor verifies deletion. Every job in the tree is
  recorded `cancelled`, exit 130, and the root client returns 130. Inside a
  job, Ctrl-C to a link (or the link's death, or its parent job's end) does
  the same for that child's subtree. One grace covers every depth.
- **Unconfirmed deletion.** The job becomes `unreaped`, its VM is
  quarantined, and its slot stays held. Held slots are derived from the
  receipts (`cleanup: uncertain` after the last successful `workers reset`
  of that Kit, persisted in `process-resets.json`), so they survive a
  daemon restart and are released only after a reset succeeds.
- **Daemon restart.** All capabilities are revoked. Unfinished jobs become
  uncertain and their VMs are quarantined. Nothing is resubmitted
  (`RevokedAtRestart`, `RestartUncertain`, `NoReplay`). The root client
  attached to an in-flight tree exits 125 with `job uncertain`; its
  children are listed uncertain.
- **Restart under `--dev`.** `make dev` reinstalls and restarts `marshd`.
  Every Kit VM with a running job, nested ones included, is then
  quarantined until `marsh workers reset KIT`. Stop agent trees before
  reinstalling. The dev broker's grants are revoked at restart as today
  (`self-development.md`).

## 9. Lineage and awareness

**Lineage.**

- Receipts' `lineage` becomes `{parent, root, depth, spawn, split?,
  label?}` (section 13). `marsh jobs show` prints it (text: `parent`,
  `root`, `depth`, `spawn`, `split`) and adds `children` (child ids);
  `marsh results` adds a `PARENT` column (the parent job's short id, `-` for
  a root).
- `marsh jobs --tree [--all] [--json]` prints the forest, or inside a job its
  own subtree. `marsh jobs [--all]` lists the same jobs flat. The text forms
  are scoped (section 13): in a session, trees rooted in that session; from
  a host terminal, running trees, trees started in the last hour, and the
  five newest trees; `--all`, every tree. `--json` is never scoped.
- Splits are nodes of the forest. A split created by a session (or the
  host CLI) is a root whose children are its branches; a branch's children
  are the jobs it started: an argv branch's job, or every Kit command its
  shell ran (the daemon records the branch session at `OpenShell` and gives
  such a job `lineage.parent = split:<id>/<label>`), with their own
  descendants, and any split the branch created. A session's scoped
  listing includes the splits it created and everything under them, also
  after `join` removed the split (the daemon keeps the 64 newest splits'
  lineage in `split-lineage.json`). A root job started by a stage after
  `join` (it sees `SPLIT_ID`) records `lineage.consumes` and is drawn with
  `  (consumes split ID)`. A job's own split (argv branches only) stays
  under the job: its branches are the job's children.
- An interactive shell's background job (stderr a terminal whose foreground
  process group is not the job's) does not print the cold-start notice
  `[starting NAME worker VM…]`. A foreground job prints it as before.
- `marsh status` adds `processes: {running, refused, held}` (text:
  `processes: N running, N refused, N held`). Ended shells (detached, their
  authority released) are no longer listed in `shells`.

**Awareness.**

- Inside a job: `type codex`, `marsh --help`, `/run/marsh/context.md`, and
  `MARSH_JOB`. The context file is one source text
  (`crates/marsh-contracts/src/context.md`, at most about 60 lines) that the
  worker fills in per job with the job's name and spawn set
  (`process::job_context`). It covers starting one child (`codex exec "…"`),
  parallel private attempts (`marsh split -n ::: a … ::: b … | marsh join`
  and applying a branch's patch), shared-workspace parallel work (`marsh
  fanout … | marsh collect`), `marsh jobs --tree`, narrowing, the limits,
  confinement, and when not to spawn.
- `marsh context` prints the same text (outside a job: the generic text). It
  only prints; it never writes.
- **Delivery to agents.** The packaged agent Kits pass the file to their
  agent for that run, the way the agent reads instructions: Claude Code
  `--append-system-prompt` (CLI; in ACP mode `claude-cli.sh` appends it to
  the Agent SDK's value), Codex `-c developer_instructions=` (CLI) and
  codex-acp's `CODEX_CONFIG` (ACP), Pi `--append-system-prompt FILE` (both
  modes). Nothing is written into the project, the selected home, `CLAUDE.md`,
  `AGENTS.md`, `~/.codex`, or `~/.pi` (`agents.md`). Stock SBX's
  `agent-context@1` capability is not used by marsh jobs (it writes into the
  agent's home); third-party Kits read `/run/marsh/context.md` or run
  `marsh context`.

## 10. Model

`model/Process.tla` models:

- Launches by the session, or by any running job that holds a live
  capability and has finished its entry step.
- Rights narrowing: the view and the spawn set.
- The pool, depth, same-Kit chain, fan-out, total, and per-tree Kit VM caps.
- The exit, cancel, and loss cascades, with grace and `unreaped`.
- A daemon restart between any two steps.
- Per-Kit entrypoint kind (script or ELF), a mandatory entry step, and the
  link's own-name decision.

Invariants:

- `LineageWF`, `Attenuation`, `BudgetBound`, `FanOutBound`, `TotalBound`;
- `LiveUnderLive`, `NoOrphanRunning`, `CancelPropagates`;
- `RevokedAtRestart`, `RestartUncertain`, `NoReplay`;
- `EntryOnce`, `NoEntryRecursion`, `SameKitBound`, `SpawnSetAttenuates`,
  `KitVMBound`;
- the liveness property `EventuallySettled`.

Each invariant has one negative control that removes a guard in a real
action.

Not modeled:

- assumption A6 and Bash semantics;
- the read-only artifact (a runtime test checks it);
- mount paths, credentials, streams and credit;
- wall time: there is no clock. A deadline is one of the environment's
  `Cancel(j, Session)` steps at any time, which already reaches every
  descendant (`CancelPropagates`). That a child's deadline is never later
  than its parent's is a value fixed at admission, checked by
  `process_tests.rs` and P35, not by TLC;
- `Workspace.tla`'s fork and capture, which compose through the shared
  cascade and the pool.

## 11. Size and migration

**Size** (non-test Rust): about 2.3k added and 0.3k removed.

| Piece | LOC |
|---|---|
| Daemon process table: lineage, admission (pool, depth, same-Kit, fan, total, spawn set), cascade | 800 |
| `ProcessRun`/`ProcessShow`, authority table, stdin credit, lock-ordered submission | 400 |
| Split as a client | +200 / -250 |
| Worker: artifact copy, stubs, `job.json`, env, connection cap fix | 250 |
| Links and in-job CLI: stub modes, entry rule, image-tool rule, env diff | 250 |
| CLI: `run --spawn/--no-spawn`, `jobs --tree`, `context`, status, receipts | 350 |
| Delete `marsh-local` | -50 |

**Migration.**

1. Lineage fields and receipts. One admission. Top-level jobs now count
   against the pool, which is a visible change.
2. `marsh run` over the relay and from the host.
3. Static artifact, stubs, `job.json`, the capability for every Kit job,
   and the cascade. Initially only the registered-name links and `marsh`
   are present.
4. (Revision 5: the `bash` stub and `SHELL` were added and later removed;
   section 15.)
5. Split as a client. Delete the split pool and `marsh-local`.
6. Amend the `AGENTS.md` rule to "the scoped job capability socket". Update
   `architecture.md`, the contract, and `CONTRACT.md`.

**Acceptance** (real callers, any Kit):

- `claude`, then the Bash tool runs `claude -p`, which
  runs `codex exec`: three containers with lineage. Ctrl-C in the root
  shell gives verified deletion in each VM.
- With `MARSH_SPAWN=claude`, `codex` from claude is refused, with the
  message, and a child cannot widen its set.
- An unchanged third-party Kit (Alpine, with a `#!/bin/sh` ELF wrapper)
  runs registered names, and `split` from `bash -c`.
- `env -i /run/marsh/bin/NAME` and `env -i /run/marsh/bin/marsh jobs` work.
- A self-replicating `marsh run shell -c` stops at the caps.
- A branch job's child cannot see the project.
- A restart mid-tree leaves uncertain jobs and no replay.
- Exported variables reach the whole tree; image `ENV` and secrets never do.
- The artifact bind is read-only.

## 12. Review disposition (revision 2 → 3)

| Finding | Resolution |
|---|---|
| C1 entry rule in the shell | The link holds the rule (marker + docker-init parent, then exec the image binary); the shell-side rule is deleted (section 5) |
| H1 shadowing `/bin/sh`, `/bin/bash` | Nothing is shadowed: PATH `bash` and `SHELL` only, no `sh` link; probe, cache, `real/`, and sh mode deleted (section 4) |
| H2 egress widening | Operator decision: cross-Kit spawning is allowed by default and the registry is the trust boundary; a child runs under its own Kit's egress and credentials, which the doc states plainly. Only narrowing exists (`MARSH_SPAWN`, `--spawn`, `--no-spawn`); the spawn set never widens (`SpawnSetAttenuates`). Rationale: the core use (claude → codex) must work with no flags, and a registered Kit is one the user already trusts |
| H3 four connections, silent drop, polled accept | ≥ `MaxFan + 4`, refusal frame, blocking accept (section 3) |
| H4 vacuous entry model | Per-Kit entrypoint kind, mandatory entry step, link decision with marker and parent guards; `ShellIntegrity` dropped for a runtime test (section 10) |
| M1 artifact location, absolute gate | VM-local copy; gate relative to real bash; pre-forked server rejected |
| M2 environment dependencies | Job mode starts from `job.json` only |
| M3 argv0 mode | Mode from the shebang stub and `/proc/self/exe` |
| M4 `sh` link | None |
| M5 env forwarding | Only variables the job exported on top of its starting environment |
| M6 stdin, cancel race | Credit window; re-check under the process-table lock |
| M7 same-Kit loops, opaque refusals | Chain cap 2; messages name the chain, `context.md`, and `workers reset KIT` |
| Lows | Restart under `--dev` (section 8); `claude update` (section 5); size trimmed |
| Q1–Q5 | Q1 superseded by the operator decision (H2); Q2 refuse TTY children; Q3 `marsh context`, print only; Q4 image-provided names stay local; Q5 relative gate |

**Open.** None.

## 13. GM resolutions (revision 4)

1. **Entry rule keeps PATH.** The link takes the entry path, clears
   `MARSH_ENTRY`, and execs the image binary by absolute path (PATH searched
   with `/run/marsh/bin` excluded). PATH is not stripped (section 5).
2. **Same-Kit chain** counts consecutive parent→child steps whose Kit
   identity (the Kit reference the job ran from) is equal; cap 2. A split
   argv branch is not such a step: nested splits are already bounded by the
   split depth limit 3 (`workspaces.md`), and counting them would refuse
   the third level of an all-fixture nested split. The chain walk stops at
   a branch job.
3. **Depth base.** A root job is depth 1.
4. **Refusals** leave no receipt; they go to the caller's stderr and
   `processes.refused`.
5. **Spawn set.** Every registered name has a link. A name outside the
   spawn set prints `marsh: spawn refused: NAME not in this job's spawn set
   (MARSH_SPAWN)` and exits 126 (125 from `marsh run`). `--spawn` beyond the
   parent's set narrows to the intersection and warns on stderr.
6. **Splits from a job.** Any job may split; the snapshot is its view.
   From a job a split is argv-only: the daemon refuses shell branches (`-b`,
   and the sugar) for every job creator, top-level or nested, because a
   shell branch is a session shell with the session's full spawn set
   (`split: LABEL: shell branches (-b, split { }) run only from the session
   shell; from a job or a split branch use argv branches: ::: LABEL CMD
   [ARG...]`). Argv branches are child jobs of the creating job
   (`lineage.parent = job:<creator>`). Inside a job, `marsh fanout ::: …` is
   the in-container, shared-workspace form (argv branches only, refused
   locally for `-b`).
7. **Pool scope** is per daemon (selected home).
8. **Formats.**
   - Receipt `lineage`: `{"parent": "session:<id>" | "job:<id>" |
     "split:<id>/<label>", "root": "<root job id>", "depth": 1.., "spawn":
     ["name", ...], "split"?: "<id>", "label"?: "<label>", "consumes"?:
     "<split id>"}`; `jobs show` adds `"children": ["<job id>", ...]`.
     `split:` is the parent of a job a split branch started without a
     parent job (an argv branch of a session's split, or a Kit command run
     by a shell branch); `consumes` marks a root job started by a stage
     after `join`.
   - Receipts carry `"args": ["...", ...]` for display: the invocation's
     first 16 arguments, each cut to 200 characters (lossy UTF-8; omitted
     when empty).
   - `marsh jobs --tree --json`: `{"schema": "marsh.jobs.tree/v1", "jobs":
     [NODE]}`, roots newest first, every tree (unscoped). A `NODE` is one of
     - a job: `{"node": "job", "job_id", "session_id", "command", "args",
       "state", "cleanup", "exit_code", "created_unix_ms",
       "finished_unix_ms", "lineage", "children": [NODE]}`;
     - a split: `{"node": "split", "split_id", "session_id" (the creating
       session), "state" (`run`, `await`, `joined`, `kept`, `cancelled`,
       `uncertain`), "status", "created_unix_ms", "finished_unix_ms" (all
       branches captured), "parent": {"split", "label"} | null,
       "timing_ms", "children": [BRANCH]}`;
     - a branch: `{"node": "branch", "split_id", "label", "kind" (`shell` |
       `argv`), "command", "state" (`pending`, `running`, `captured`,
       `rejected`, `failed`, `cancelled`, `uncertain`), "status",
       "exit_code", "job_id" (an argv branch's job), "files",
       "created_unix_ms", "finished_unix_ms", "children": [NODE]}`.
     A daemon restart may forget a split whose jobs remain: it is drawn
     from their lineage with `state: null`. `marsh jobs --json` stays the
     `marsh.jobs/v1` document.
   - Text (`jobs`, `jobs --tree`): a header `ID STARTED STATE EXIT TIME
     [PARENT] COMMAND`, then one line per job: the first 8 characters of the
     job id, start as `Ns|Nm|Nh|Nd ago`, state, exit code or `-`, duration
     (`12.3s`, `2m13s`, `1h05m`), and `command arg...` with each argument
     shell-quoted, cut to 64 characters with `…`, then `  cleanup uncertain
     (run marsh workers reset KIT)` when it applies. Trees containing a
     queued or running job come first, then newest first. `--tree` keeps
     children under their parent in launch order, drawn with `├─ `/`└─ `
     (and `│  `) in the command column; the flat form sorts every job the
     same way and adds `PARENT` (the parent's short id, `-` for a root).
     A split row shows the split's first 8 id characters, `joined` (or
     `running`, `awaiting`, `kept`, `cancelled`, `uncertain`), its status,
     its run time, and `split (LABEL, ...)`; a branch row shows `-` (an argv
     branch: its job's id and fields), the branch state in job words
     (`finished` for captured, `queued`, `rejected`, ...), and `LABEL:
     COMMAND`. The flat form lists jobs only; a branch's job shows the
     split's short id as `PARENT`. Scoped out trees end the listing with `(N older job trees not shown;
     marsh jobs [--tree] --all lists every job)`; an empty listing prints
     `no jobs in this session` or `no jobs recorded`.
   - `marsh status --json`: `"processes": {"running": N, "refused": N,
     "held": N}`.
   - `/run/marsh/job.json`: `{"version": 1, "job", "name", "spawn":
     [...], "registered": [...], "socket": "/run/marsh/cap.sock" | null,
     "path": "<starting PATH>", "limits": {"pool", "depth", "same_kit",
     "fan_out", "total"}, "env": {"NAME": "<HMAC-SHA256 hex of NAME=VALUE
     under the daemon's key>" | "" (set by Docker, never forwarded)},
     "forwarded": ["NAME", ...] (received from the parent)}`.
9. **Restart.** The root client attached to an in-flight tree exits 125
   with `job uncertain`; children are listed uncertain.
10. **Connection cap** counts open connections including idle ones; an
    excess connection reads one error frame (`too many concurrent daemon
    requests in this job`) and is closed.
11. **TTY.** A child is refused if its stdin or stdout is a terminal.

## 14. Implementation notes and deferrals

- The static artifact is the musl build of `marsh-local`
  (`marsh-local-linux-arm64`), installed with the worker to
  `/usr/local/libexec/marsh-local` on the Kit VM's disk and bound at
  `/run/marsh/marsh`. The host and shell-VM `marsh` stay glibc builds.
- `ProcessRun` runs the child through this daemon's own `Execute`
  (`process::serve_run`); admission is `process::admit` under the store
  lock, once before VM work and again in `begin_job_process`.
- `--no-spawn` / an empty spawn set keeps `cap.sock` and the links; the
  daemon refuses every spawn (resolution 5).
- The artifact is mandatory: the worker install and identity and the job's
  container setup fail without it; a failed `/run/marsh` setup fails the
  attempt (`SetupFailed`), never runs it without one.
- The same-Kit pre-check uses the Kit identity resolved before VM work;
  refused launches count toward their tree's total.
- The daemon applies the split environment filter to `ProcessRun` itself.
- A connection slot on `cap.sock` is freed only when the daemon side closes;
  a link retries a refused connection briefly (nothing was started).
- `job uncertain` is reported only for transport loss after the daemon
  accepted the job or shell.
- Deferred: the stdin credit window (stdin uses the existing per-connection
  queue).
- Wall-time inheritance: `DaemonStore::set_job_deadline` (`process.rs`)
  fixes each job's deadline just before launch and the backend passes the
  remaining seconds as the worker's `wall_seconds`; `child_lineage` copies
  the parent's deadline and refuses a child with under a second left. The
  Kit VM cap is `kit_vm_cap`, counted over the tree's live receipts by Kit
  identity (`kit_ref`), in both the pre-check and `begin_job_process`.

## 15. Revision 5: real bash in Kit jobs, CLI fanout, agent context

**Operator decision: a Kit job's shells are the image's own.** The `bash`
link and `SHELL=/run/marsh/bin/bash` are removed, with the job-shell mode of
the artifact (Brush in a job), its `$SHELL`/PATH re-add special cases, and
the job-shell startup gate. Rationale:

- **Compatibility.** Agents and build tools run whatever Bash the image
  ships; no classified Brush divergence (`bash-compatibility.md`) can reach
  an agent's tool calls, scripts, or `bash -lc` profiles.
- **Performance.** No wrapper sits between an agent and its shell (P24
  checks PATH `bash` is within 2 ms of `/bin/bash`).
- **Debuggability.** What runs in a job is what the image author tested; a
  failing tool call reproduces with `docker run` of the same image.
- **Nothing is lost.** Every capability the job shell offered has a CLI form
  that works from any shell: registered names (links), `marsh run`,
  `marsh split | marsh join`, `marsh fanout | marsh collect`, `marsh jobs`,
  `marsh context`. Only the Brush sugar syntax is gone from jobs.

Kept: the links for every registered name, the entry rule, the in-job
`marsh` CLI, `cap.sock`, lineage, and budgets. The static artifact stays
(`marsh-local`, musl): it is the links and the in-job CLI, and it remains the
Brush binary that the Bash-compatibility tests use.
The Brush patches stay: they serve the session shell.

**`marsh fanout [-n] [-b LABEL=STRING]... [::: LABEL CMD ARG...]... | marsh
collect [--json] [--timing] [--stderr]`** has the sugar's semantics: branches run at
once in the caller's place on the same files (no fork), each reads the same
spooled stdin (`-n` or a terminal: empty), and `collect` renders them in
declaration order with Brush's own renderer (`brush_core::render_collected`,
patches 0045, 0046): per branch the header, its stdout, then a failed
branch's stderr under `== LABEL stderr ==`, all on stdout (as `join`
does; `--stderr` shows successful branches' stderr too); the exit status is
the first nonzero branch status. Limits: 16 branches, 64 MiB input, 16 MiB
combined output; INT, TERM, and HUP are forwarded to every branch's process
group (10 s, then KILL) and `fanout` exits 128+signal. `fanout` writes one
JSON line (`marsh.fanout/v1`, branch bytes base64) for `collect`, or the
rendering itself when its stdout is a terminal.

- In a job: argv branches only (as for `split`); `-b` is refused locally
  with the session-shell message. A registered name in a branch is a child
  job through its link.
- In the shell VM: `-b` runs STRING with the session shell (a child guest
  marsh attached to the same session, as Brush runs a shebang-less script).
- On the host: the same fanout runs in a session shell in the shell VM
  (`marsh -c 'exec marsh fanout …'`); the host picks the output form and
  passes `-n` when its stdin is a terminal.

No daemon request is involved: fanout is local concurrency, so the Brush
sugar and the CLI share the renderer rather than a protocol.

**Agent context** is generated per job and delivered by the agent Kits
(section 9, `agents.md`).
