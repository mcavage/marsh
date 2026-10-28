# Processes: nested jobs and authority

Status: implemented. Any process in a Kit job can call registered commands,
`marsh run`, or `marsh split`. Each call or split argv branch starts a fresh
job, subject to admission. The daemon records its parent and cancels the tree
together. Kit jobs keep their image's own shells.

The model is `docs/model/Process.tla`. This builds on
[workspaces](workspaces.md); [acceptance scenarios](processes-acceptance.md)
define the external checks. Section 11 summarizes acceptance, section 12 the
design decisions, section 13 the exact contract details and formats, section
14 implementation limits, and section 15 Kit job shells and CLI fanout.

## 1. Principle: one protocol, many authorities

**One protocol.** The marsh shell runs where marsh starts a shell: the root
shell in the shell VM and split shell branches. Inside a Kit job every shell
is the image's own (section 15). Every client (the session shell, the `marsh`
CLI, and the per-name links) speaks one request protocol
(`Envelope<PublicRequest>`) over one local socket.

The daemon binds authority to the caller's socket:

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
- **Brush sugar works only in the Brush session shell.** That is `fanout { } |
  collect`, `split { } | join`, and in-process dispatch. The CLI forms
  (`marsh fanout ... | marsh collect`, `marsh split ... | marsh join`) work
  from bash and zsh too. Inside a Kit job, they accept argv branches only;
  a job cannot start a session-shell branch with wider authority.

The job capability covers both process and split requests (section 3).
`marsh-local` remains the static job artifact. Kit jobs use process admission;
split execution also retains its branch pool (`workspaces.md`, section 4).

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
  backend `CapabilityBridges`). Every Kit job receives that socket.

## 3. Requests by authority

| Request | Session (relay) | Job (`cap.sock`) |
|---|---|---|
| `ProcessRun {argv, cwd, env, spawn}` + stdin → `job`, then `Execute` frames | yes | yes; `spawn` may only narrow; a dropped stream cancels the child |
| `SplitCreate` / `SplitJoin` | yes | yes; the creator is the job and the snapshot is the job's view |
| `ProcessShow`, `Jobs`, `ShowJob` | session-authorized job inspection | the job's own subtree |
| `SplitShow` (`marsh splits`) | yes | no (a job sees its splits in `marsh jobs --tree`) |
| `SplitCancel`, `jobs cancel`, `splits rm` | yes, within its tree | no (drop the stream instead) |
| everything else (MCP, ACP, dev broker, lifecycle) | request-specific session grants; dev broker only for `--dev` | refused |

**`ProcessRun`** is admission (section 6), then an internal `Execute`
through the daemon's own endpoint (as split argv branches do in `split.rs`),
then frame relay.

- **Ordering against cancel.** Under the process-table lock, the daemon
  re-checks that the parent's capability is live and not cancelling,
  inserts the child record, and only then submits. A cancel that takes the
  lock first sees no child. A cancel that takes it later finds the record
  and cancels the child (`Spawn` is atomic in the model).
- **stdin.** The implementation uses a bounded per-connection queue. The
  proposed 256 KiB replenished stdin-credit window remains deferred
  (section 14). stdout and stderr use per-attempt credits.

**Capability socket** (`capability.rs`): blocking accept, at most
`MaxFan + 4` (12) open connections per attempt. An excess connection receives
`marsh: too many concurrent daemon requests in this job` and closes.

## 4. The links and the in-job `marsh`

**Artifact.** The static musl `marsh-local` binary is installed on each Kit
VM's local disk alongside the worker binaries and bound read-only at
`/run/marsh/marsh`. The session-shell `marsh` remains a glibc build. Runtime
tests check that the job artifact's bind is read-only.

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
  section 13), plus `xdg-open` and `open` (`marsh open`, an `OpenUrl`
  request; a registered name wins); never for `bash` or `sh`. For example
  `#!/run/marsh/marsh --link=codex`. The kernel passes the mode and the stub
  path to the binary, so the mode comes from the kernel and the link name,
  never from a caller-supplied argv0. `/proc/self/exe` confirms that the
  process is the artifact.

**Environment:**

- `PATH=/run/marsh/bin:` followed by the PATH from the image config. The
  worker reads only the image config (`Env`); there is no probe container.
- `MARSH_JOB` and `MARSH_ENTRY=1`. `SHELL` is left as the image sets it.
- `BROWSER=xdg-open` when the job has no `BROWSER`.

The image's shells, system tools, and entrypoint are unchanged. Only registered
names, `marsh`, `xdg-open`, and `open` get links; there is no `bash` or `sh`
link.

**What each caller gets:**

| Caller | Runs |
|---|---|
| any shell (`bash -c`, Codex's `bash -lc`, `$SHELL`, `/bin/sh`, `system(3)`, Node `spawn('bash')`, Python `shell=True`) | the image's own shell |
| a registered name from any of them, or a direct `execvp` | its link (below) |
| `marsh VERB` (`run`, `split`, `join`, `fanout`, `collect`, `jobs`, `context`) | the in-job CLI (the artifact's `marsh` link) |
| an absolute image binary (`/usr/local/bin/codex`) | that binary, in this container, with this job's authority |

**The in-job `marsh`** starts from `job.json` alone (no `USER`, `HOME`, or
daemon), so `env -i /run/marsh/bin/codex …` and `env -i /run/marsh/bin/marsh
jobs` work. It connects to `cap.sock` per request. `type codex` in the
image's bash prints `codex is /run/marsh/bin/codex`. A login profile that
resets PATH hides the links; `marsh run NAME` and the absolute link path
still work. The in-job CLI parses `marsh splits`, but the capability refuses
its `SplitShow` request (section 3).

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

**Own-name subcommands.** An own-name call such as `codex exec …` from a codex
job's shell is a child case, so it runs as a child job that can write only what
its mounts allow, which includes the selected home. Kit images pin agent
versions, and anything a job installs into its container is discarded when
the container is deleted. The packaged Claude Kit refuses `claude update` and
`claude upgrade` in its entrypoint (exit 125, `self-updates are disabled`) and
sets `DISABLE_AUTOUPDATER=1`; the child job starts and exits without updating.

## 6. Authority and budgets

**Rights** are `(view, spawn set)`. A child gets its parent's recorded
rights or fewer, never rights recomputed from the session's grants. The
view is checked by `Attenuation` (control `widen`) and the spawn set by
`SpawnSetAttenuates` (control `spawnWiden`).

**Cross-Kit spawning is allowed by default.** Any job may spawn any
registered command, so the core use (claude → codex) works with no flags. The
user's command registry is the trust boundary: registering a Kit means
trusting it to be started by any job in any session.

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
`spawnWiden`). marsh does not send proxy-held credentials over this socket.
Frames carry caller-supplied argv, filtered environment, and stream bytes;
callers can still put sensitive data in those fields.

**Environment for children.** A child receives the variables its parent
received from *its* parent (or, for a root job, the user's exported shell
  variables), plus those the parent set or changed. A variable the user
exports reaches the whole tree, and a child re-exporting a received name
with the same value still passes it on. Image `ENV` is never forwarded
unless the job changed it, and the split filter (`workspaces.md` s13) drops
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

**Budgets.** Process admission covers top-level Kit jobs, Run children, and
split argv jobs. Split execution also has a separate branch pool
(`workspaces.md`, section 4). Admission fails fast and never queues, because a parent waiting on a
queued child would deadlock the pool.

| Limit | Value | Refusal (exit 125 / `failed: …`) |
|---|---|---|
| Pool | 16 concurrent jobs per daemon (selected home): one pool across all sessions and host CLI calls, all depths; a slot frees only on verified deletion (`BudgetBound`, control `freeOnRevoke`) | `capacity: 16 jobs (2 held by uncertain jobs on Kit codex: run marsh workers reset codex)` |
| Kit VMs | 4 distinct Kit VMs live at once per job tree (one warm VM per Kit identity: aliases of a live Kit, or a second call to it, cost nothing; `MARSH_TREE_KIT_VMS` at daemon start sets 1–64; `KitVMBound`, `noKitCap`) | `shell refused: tree already uses 4 Kit VMs (claude, codex, fixture, pi) (MARSH_TREE_KIT_VMS)` |
| Depth | 4; a root job is depth 1 (`LineageWF`, `noDepth`) | `depth limit 4` |
| Same-Kit chain | 2 consecutive parent→child steps with the same Kit identity (Kit source/image, not the registered name): claude → claude is allowed, a third claude is refused (`SameKitBound`, `noSameKit`) | `claude → claude → claude refused: same-Kit chain limit 2 (see /run/marsh/context.md)` |
| Fan-out | 8 live children per job, including its splits' branches (`FanOutBound`, `noFan`) | `fan-out limit 8` |
| Total | 128 launches per top-level tree (`TotalBound`, `noTotal`) | `total limit 128` |
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
  job does (agents keep settings and session state there).
- **cwd.** The child's cwd must lie inside those mounts.
- **Narrowing.** Splits narrow the project view; the late `.marsh` read-only
  bind also reduces writes. Nothing widens a child's view.
- **Splits.** A split created by a job snapshots the job's view root.
  Capture waits for the creating branch's whole subtree (`workspaces.md` s13).
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
  uncertain and retain held admission slots until verified reset. Nothing is
  resubmitted (`RevokedAtRestart`, `RestartUncertain`, `NoReplay`). The adapter's
  in-memory VM quarantine is not restored from receipts (`results.md`). The root client
  attached to an in-flight tree exits 125 with `job uncertain`; its
  children are listed uncertain.
- **Restart under `--dev`.** `make dev` reinstalls and restarts `marshd`.
  Jobs that were running, nested ones included, become uncertain and hold
  their slots until `marsh workers reset KIT`. Stop agent trees before
  reinstalling. The dev broker's grants are revoked at restart
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
  five newest trees; `--all`, every tree. `--json` is not scoped by session
  or age. Inside a job, every form, `--all` and `--json` included, covers only
  the job's own subtree (the capability filters `Jobs` and `ProcessShow`).
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
- Fanouts are display nodes. `marsh fanout` gives each branch
  `FANOUT_BRANCH=<fanout id>/<label>`; a root job or split-branch job started
  with it records `lineage.fanout`, and the tree draws those jobs under one
  fanout node at the first job's place. A registered command in a fanout
  branch inside a job is an ordinary child of that job and gets no fanout
  node. The grouping is for display only; it adds no authority or lifecycle.
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
  `AGENTS.md`, `~/.codex`, or `~/.pi` (`agents.md`). Third-party Kits read
  `/run/marsh/context.md` or run `marsh context`.
- **Kit skills and instructions.** Stock SBX applies `agent-context`,
  `agent-skill@1` and `agent-skills@1` in the outer sandbox container, which
  jobs never see. The daemon reads them instead from the image's
  `vnd.docker.sandbox.kit.descriptor` annotation (linux/arm64 manifest; a kit
  set's build has already composed its components there), once per digest,
  cached in `kit-assets/` under the control home, and passes them in the job
  capability (`marsh_contracts::kit_assets`). The worker appends each
  instruction body to `context.md` (inline text, or a regular world-readable
  file of the image, at most 64 KiB) and writes `skills/<index>/<name>` links;
  the runtime binds each skills destination read-only when the Kit bundles
  skills and the destination neither contains nor lies inside a job mount.
  Nothing is written to the agent's home. A failed descriptor read starts the
  job without them and says so on stderr; a local source Kit has none.

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

## 11. Acceptance

The acceptance goals, with real callers and any Kit, are below.
[processes-acceptance.md](processes-acceptance.md) lists the maintained
black-box scenarios and the gaps they do not cover.

- `claude`, then the Bash tool runs `claude -p`, which runs `codex exec`:
  three containers with lineage. Ctrl-C in the root shell gives verified
  deletion in each VM.
- With `MARSH_SPAWN=claude`, `codex` from claude is refused, with the
  message, and a child cannot widen its set.
- An unchanged third-party Kit (for example Alpine, with a `#!/bin/sh`
  entrypoint wrapper) runs registered names, and `split` from `bash -c`.
- `env -i /run/marsh/bin/NAME` and `env -i /run/marsh/bin/marsh jobs` work.
- A self-replicating `marsh run shell -c` stops at the caps.
- A branch job's child cannot see the project.
- A restart mid-tree leaves uncertain jobs and no replay.
- Exported variables reach the whole tree; image `ENV` and secrets never do.
- The artifact bind is read-only.

## 12. Design decisions

- **The link holds the entry rule.** The marker and docker-init parent check
  live in the artifact, not in a shell, so the rule works for any entrypoint
  (section 5).
- **Nothing is shadowed.** Only registered names, `marsh`, `xdg-open`, and
  `open` get links;
  `/bin/sh`, `/bin/bash`, PATH `bash`, and `SHELL` are the image's own
  (sections 4 and 15).
- **Cross-Kit spawning is on by default; only narrowing exists.** A child runs
  under its own Kit's egress and credentials. `MARSH_SPAWN`, `--spawn`, and
  `--no-spawn` narrow; nothing widens (`SpawnSetAttenuates`, section 6).
- **The link mode comes from the kernel.** The shebang stub and
  `/proc/self/exe` select the mode, never a caller-supplied argv0, and job mode
  starts from `job.json` alone (section 4).
- **Children receive only what the job received or changed.** Unchanged image
  `ENV` is never forwarded; digests are keyed so a job cannot test guesses
  (section 6).
- **The capability socket refuses, never drops.** It uses a blocking accept,
  at least `MaxFan + 4` connections, and a refusal frame (section 3).
- **Spawn is ordered against cancel** by re-checking the parent's capability
  under the process-table lock (section 3).
- **Loops are bounded and refusals explain themselves.** The same-Kit chain cap
  is 2; messages name the chain, `context.md`, and `marsh workers reset KIT`.
- **No TTY children; image-provided names stay local; `marsh context` only
  prints** (sections 5, 6, and 9).
- **The model checks the link decision.** Per-Kit entrypoint kind and a
  mandatory entry step make `EntryOnce` and `NoEntryRecursion` non-vacuous;
  shell integrity is a runtime test, not an invariant (section 10).

## 13. Contract details

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
     ["name", ...], "split"?: "<id>", "label"?: "<label>",
     "deadline_unix_ms"?: N, "parent_deadline"?: true, "consumes"?: "<split
     id>", "fanout"?: "<fanout id>/<label>"}`; `jobs show` adds
     `"children": ["<job id>", ...]`. `split:` is the parent of a job a split
     branch started without a parent job (an argv branch of a session's
     split, or a Kit command run by a shell branch); `consumes` marks a root
     job started by a stage after `join`; `fanout` marks a job without a job
     parent that a `marsh fanout` branch started (display grouping only).
   - Receipts carry `"args": ["...", ...]` for display: the invocation's
     first 16 arguments, each cut to 200 characters (lossy UTF-8; omitted
     when empty).
   - `marsh jobs --tree --json`: `{"schema": "marsh.jobs.tree/v1", "jobs":
     [NODE]}`, roots newest first, every tree (not scoped by session or age;
     inside a job, the job's subtree). A `NODE` is one of
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
       "created_unix_ms", "finished_unix_ms", "children": [NODE]}`;
     - a fanout: `{"node": "fanout", "fanout_id", "session_id", "state"
       (`running` | `finished`), "status" (the first nonzero branch exit,
       else 0 once finished), "created_unix_ms", "finished_unix_ms",
       "children": [FANOUT BRANCH]}`, where a fanout branch is `{"node":
       "branch", "fanout_id", "label", "kind": "argv", "job_id", "state",
       "children": [the job's NODE]}`.
     A daemon restart may forget a split whose jobs remain: it is drawn
     from their lineage with `state: null`. `marsh jobs --json` stays the
     `marsh.jobs/v1` document.
   - Text (`jobs`, `jobs --tree`): a header `ID STARTED STATE EXIT RUN
     [PARENT] COMMAND`, then one line per job: the first 8 characters of the
     job id, start as `Ns|Nm|Nh|Nd ago`, state, exit code or `-`, `RUN` (from
     the job record's creation, when its Kit VM is ready, to its end:
     `12.3s`, `2m13s`, `1h05m`), and `command arg...` with each argument
     shell-quoted, cut to 64 characters with `…`, then `  cleanup uncertain
     (run marsh workers reset KIT)` when it applies. Trees containing a
     queued or running job come first, then newest first. `--tree` keeps
     children under their parent in launch order, drawn with `├─ `/`└─ `
     (and `│  `) in the command column; the flat form sorts every job the
     same way and adds `PARENT` (the parent's short id, `-` for a root).
     A split row shows the split's first 8 id characters, `joined` (or
     `running`, `awaiting`, `kept`, `cancelled`, `uncertain`), its status,
     its run time, and `split (LABEL, ...)`. A fanout row shows the fanout's
     first 8 id characters, `running` or `finished`, its status, its run
     time, and `fanout (LABEL, ...)`. An argv branch row is its job's row
     (id and fields) with `LABEL: COMMAND`; a shell branch row has the ID
     `<split8>/LABEL`, the branch state in job words (`queued` for pending,
     `finished` for captured, `unknown` for uncertain, otherwise as
     recorded), and `LABEL: COMMAND`. The flat form lists jobs only; a
     branch's job shows the split's short id as `PARENT`. Scoped-out trees
     end the listing with `(N older job trees not shown; marsh jobs [--tree]
     --all lists every job)`; an empty listing prints `no jobs in this
     session` or `no jobs recorded`.
   - `marsh status --json`: `"processes": {"running": N, "refused": N,
     "held": N}`.
   - `/run/marsh/job.json`: `{"version": 1, "job", "name", "spawn":
      [...], "registered": [...], "socket": "/run/marsh/cap.sock",
     "path": "<starting PATH>", "limits": {"pool", "depth", "same_kit",
     "fan_out", "total"}, "env": {"NAME": "<HMAC-SHA256 hex of NAME=VALUE
     under the daemon's key>" | "" (set by Docker, never forwarded)},
      "forwarded": ["NAME", ...] (received from the parent)}`.
      Every Kit job retains the socket and links, including with `--no-spawn`;
      an empty spawn set narrows admission rather than removing the channel.
9. **Restart.** The root client attached to an in-flight tree exits 125
   with `job uncertain`; children are listed uncertain.
10. **Connection cap** counts open connections including idle ones; an
    excess connection reads one error frame (`too many concurrent daemon
    requests in this job`) and is closed.
11. **TTY.** A child is refused if its stdin or stdout is a terminal.

## 14. Implementation notes and deferrals

- The worker installs `marsh-local-linux-arm64` at
  `/usr/local/libexec/marsh-local` on the Kit VM's disk.
- Process admission is checked before VM preparation and again under the
  store lock in `begin_job_process`, before submission.
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

## 15. Kit job shells, CLI fanout, agent context

**A Kit job's shells are the image's own.** marsh adds no `bash` or `sh` link
and no `SHELL` override to a job, and the artifact has no job-shell mode
(Brush is not a job shell). Rationale:

- **Compatibility.** Agents and build tools run whatever Bash the image
  ships; no classified Brush divergence (`bash-compatibility.md`) can reach
  an agent's tool calls, scripts, or `bash -lc` profiles.
- **Performance.** No wrapper sits between an agent and its shell (P24
  checks PATH `bash` is within 2 ms of `/bin/bash`).
- **Debuggability.** What runs in a job is what the image author tested; a
  failing tool call reproduces with `docker run` of the same image.
- **CLI forms cover composition.** Registered names (links), `marsh run`,
  `marsh split | marsh join`, `marsh fanout | marsh collect`, `marsh jobs`, and
  `marsh context` work from any shell. Only the Brush sugar syntax is
  unavailable in jobs.

The static artifact (`marsh-local`, musl) provides the links and the in-job
CLI, and it is the Brush binary that the Bash-compatibility tests use. The
Brush patches serve the session shell.

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
- On the host with the cwd in a split fork or lane's copy: no session shell
  (it would mount the copy as a project, `workspaces.md` s14). Each argv
  branch is `marsh run NAME ARG...` from the copy, so a registered name is a
  confined lane job; other argv names and `-b` branches fail or are refused.
- On the host: the same fanout runs in a session shell in the shell VM
  (`marsh -c 'exec marsh fanout …'`); the host picks the output form and
  passes `-n` when its stdin is a terminal.

No daemon request is involved: fanout is local concurrency, so the Brush
sugar and the CLI share the renderer rather than a protocol.

**Agent context** is generated per job and delivered by the agent Kits
(section 9, `agents.md`).
