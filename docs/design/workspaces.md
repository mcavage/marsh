# Workspaces: snapshots, forks, and split lineage

Status: implemented. The [user guide](../split.md) covers usage; the
[acceptance scenarios](workspaces-acceptance.md) cover observable behavior.
The model is `docs/model/Workspace.tla`.

Sections 1–8 describe the implementation. Section 9 records the model scope,
section 10 non-goals and v1 limits, section 11 the acceptance requirements,
section 12 creator identity and JSON shapes, section 13 implementation
bounds and process integration, and section 14 open lanes, checkpoints, and
storage.

## 1. Concepts and trust

The host daemon snapshots the caller's tree, creates forks, runs branches,
captures results, and records lineage. The CLI, Brush syntax, and in-job client
send requests and render replies.

- **Snapshot** (`S`): a frozen filesystem copy in `<id>/base`, plus the
  caller's HEAD (`H`) and index bytes when it is a Git repository. The daemon
  does not hash the whole snapshot into Git objects. `store.git` receives
  pre- and post-image blobs for changed paths during capture. The source is
  the user's tree for a session, or the creator branch's fork for a nested split.
- **Fork**: a private filesystem copy of the base per branch,
  `<root>/.marsh/split/<id>/<label>`. All splits, nested ones included,
  are siblings under `<root>/.marsh/split/`, inside the existing project
  mount. No new stock mount is needed. The journal records the parent.
- **Result**: after every writer is gone and the fork has been verified,
  the daemon writes the `out/` artifacts (section 5). Consumers read
  `out/`, never live Git admin directories. Split machinery writes only its
  `.marsh` state, leaving source files and the original `.git` unchanged;
  the consumer chooses which patches to apply.
- **Two kinds of branch, two trust levels.**
  - **Shell branch** (`-b LABEL='string'`) is **trusted code**, with the
    same authority as the user's shell: it runs in the shell VM, as the
    user's uid, and sees the whole project at its natural path. Its cwd is
    its fork, but nothing confines its own code (assumption C1). Use it for
    your own commands (`make test`, `rg`). A registered Kit command that a
    shell branch starts is an ordinary Kit job whose cwd is in the fork, so it
    is confined like an argv branch (`split_confinement.rs`); that is how
    `split { fix: claude -p "..." }` keeps the agent in its fork.
  - **Argv branch** (`::: LABEL CMD ARGS...`) is one registered Kit command.
    It runs in a fresh container that mounts, of the project, only its fork
    and the fork's Git metadata (section 8), plus the selected home
    read-write like every Kit job (agents keep settings and session state
    there). Use it when the branch is exactly one command, or from inside a
    job, where shell branches are not available.
- **Lineage**: session -> split -> branch -> job -> child split, journaled
  before each effect. Split, branch, capability, job, attempt, VM, and
  container identities stay distinct.

## 2. CLI (primary)

```sh
git diff | marsh split -b tests='make test' \
             ::: review codex exec "review this diff" \
             ::: sec    claude -p "review security" \
         | marsh join -- claude -p "apply valid feedback"

h=$(marsh split ::: a codex exec "x" ::: b pi -p "y" </dev/null)  # two-step
marsh join <<<"$h" -- ./apply.sh
```

- `-b LABEL=STRING`: a fresh guest `marsh -c STRING` session using Brush,
  including when the caller uses the bash or zsh frontend.
  It runs as its own supervisor attempt (its own cgroup) with cwd
  = the fork's matching subdirectory and the caller's exported environment.
  It does **not** inherit unexported variables, aliases, functions, `set`
  options, traps, or the job table; Brush sugar loses these too.
- `::: LABEL CMD [ARG...]`: argv taken verbatim (the outer shell has already
  quoted it). `CMD` must be a registered command. A job argument cannot be
  the literal `:::`.
- stdin is read to EOF into the daemon spool (64 MiB) before the snapshot,
  and every branch reads the spool. A terminal stdin, or `-n`, means empty.
- `marsh split` waits until every branch is captured. It exits with the
  first nonzero branch status in **declaration order** (130 on cancel, 2 on
  setup). On completion it prints one handle line `{"marsh_split":1,"id":"…"}`,
  including when stdout is a terminal.
- `marsh join [--json] [--keep] [-- CMD ARGS...]` reads a handle and is
  accepted only from the split's **creator**: the same session, the same job,
  or a host caller holding a host-created split's handle (section 12).
  It works on awaiting and kept splits.
  - With `-- CMD`, CMD runs even if branches failed. It gets the rendering
    on stdin (statuses included) and `SPLIT_ID`, `SPLIT_DIR` (the `out/`
    dir), `SPLIT_MANIFEST`, and `SPLIT_OBJECTS` (root splits only, for
    `git apply --3way` preimages). join then releases (remove on CMD
    status 0, otherwise keep) and exits with CMD's status.
  - Bare `| marsh join | cmd` releases once the rendering is written:
    remove if every branch succeeded and `--keep` is not given, otherwise
    keep.
  - With `pipefail`, `marsh split … | marsh join -- CMD` returns CMD's
    status when nonzero, otherwise split's.
- `marsh splits [--json] [ID]`, `marsh splits cancel ID`, and
  `marsh splits rm ID` (the creator's own kept splits; operator authority
  for any). These are not available from a Kit job, whose capability admits
  only `SplitCreate` and `SplitJoin` (section 3).
- **Host CLI** (a plain Mac terminal): `marsh split` opens a temporary
  ordinary session without a TTY so shell branches have a
  shell VM. `SplitCreate` carries the exported environment, minus
  `MARSH_*`, `SBX_*`, `DOCKER_*`, and relay or bearer tokens.
- **Splits created by any Kit job or split branch** take **argv branches
  only**, including when the creator is a top-level job. Every Kit container
  gets the `marsh-local` client as `marsh` on `PATH`, so it can create and join
  its own splits.

**Brush sugar.** `P | split { a: X; b: Y } | join | C1 | C2` is `SplitCreate`
with shell branches `X` and `Y` (the spool is P's output), a wait, then
`C1 | C2` with `SPLIT_*` pushed, then `SplitRelease(consumed(last
status))`. Every sugar branch is a shell branch (a `::: CMD ARGS` line is
not recognized; use the CLI for argv branches).
Branches are label-led (Brush `0041`; `split.md`): `;` starts a branch only
before a `LABEL:` word, so `a: cd x; make` is one branch `cd x; make`.
Labels, `PIPESTATUS`, and Ctrl-C are preserved; Brush does not fork split branches.

**Terminal and signals.** Branches have no controlling terminal (`/dev/tty`
gives ENXIO) and their stdin is the spool. Ctrl-C at the client sends
`SplitCancel`. Ctrl-Z stops only the client while branches keep running,
and `fg` resumes the wait. Client death or SIGHUP before capture drops the
stream, and a dropped stream cancels the split, unless it was created with
`marsh split -d` (`SplitCreateSpec.detach`): then the create stream ends after
`SplitStarted` and only an explicit signal or `SplitCancel` cancels it.
`SplitJoin` on a running split waits for it to finish.

## 3. Daemon requests

| Request | From | Payload -> reply |
|---|---|---|
| `SplitCreate` | relay, socket | `cwd`, `branches: [{label, shell(string) \| argv([..])}]`, `env`, `keep`, streamed stdin -> `split`, events, manifest. Dropping the stream before capture cancels |
| `SplitJoin` | relay, socket | `split`, then `consumed(status) \| keep` -> manifest and `out/` contents (streamed to callers that cannot see the dir). Creator only; this is also the release |
| `SplitCancel` | relay | `split` -> ack once the subtree is revoked and signalled. The session may cancel any of its splits; a shell branch may cancel splits below it |
| `SplitShow` | relay | `split?` -> manifest + subtree, or every split in scope |
| `SplitRemove` | relay | `split` -> removes a kept split (creator or operator) |
| `SplitClose` | relay, socket | `split` -> closes an open lane split: manifest (s14) |
| `Checkpoint` | relay, socket | `root?` (a relay's is its session project), `create {label}` / `list {label}` / `remove {id}` (s14) |
| `Storage` | relay, socket | `root?` -> per-project checkpoints, kept splits, free space (s14) |

For splits, the job capability socket admits `SplitCreate` and `SplitJoin`,
not show, cancel, or remove. Every Kit job now has the socket; the complete
request allowlist is in [processes](processes.md#3-requests-by-authority).
Frames are credited per attempt.

## 4. Budgets and capability

- **Split branch pool**: 16 concurrent branches per creating session, with
  nested splits using the same pool and all host CLI splits sharing `host`.
  Kit jobs also pass the daemon-wide process admission in `processes.md`;
  the split pool does not count Kit jobs started by a shell branch.
  Admission happens at launch. When the
  pool is full, the branch fails visibly (`failed: capacity`) and join
  still runs (`BudgetBound`). A slot frees only when its process's end is
  confirmed (captured, rejected, or reaped). Uncertain and unreaped slots
  stay held until `marsh workers reset`.
- **Caps**: depth 3, 16 branches per split, and the job wall limit per split
  (`MARSH_JOB_WALL_SECONDS`, 24 h by default; it was 1 h before 0.2.1). Spool (64 MiB)
  and captured output (16 MiB) are hard limits; crossing either cancels
  the split. There are no per-branch reservations or byte quotas in v1.
- **Capability**: a daemon record per branch `{split, creator, live|revoked}`,
  never a bearer secret. It is revoked at the branch's exit or crash, and
  that cancels everything below it (`LiveUnderLive`, `CancelPropagates`).
  Every daemon restart revokes all of them.
- **Delivery to a Kit job**: the worker binds a per-attempt Unix
  socket at `/run/marsh/cap.sock` and forwards its frames, tagged with the
  attempt id and credited per attempt, over the existing worker transport.
  The socket has no token and dies with the container. Its split verbs are
  `SplitCreate` (creator = this job) and `SplitJoin` (only
  for splits this job created), alongside the process requests. Jobs get no
  Docker, containerd, or SBX socket and cannot use the dev shell's relay.
  Ending the job or restarting the daemon revokes its capability.

## 5. Results: `out/` layout and lineage

```
<root>/.marsh/split/<id>/out/manifest.json        version 2; lineage, placement per branch
<root>/.marsh/split/<id>/out/<label>/stdout
<root>/.marsh/split/<id>/out/<label>/stderr
<root>/.marsh/split/<id>/out/<label>/status       "exited 0" | "failed: capacity" | "rejected: <why>" | "cancelled"
<root>/.marsh/split/<id>/out/<label>/diff.patch   git-apply compatible, from S to R (absent when no change)
<root>/.marsh/split/<id>/out/<label>/files        "<A|M|D|T>\t<path>\n" per changed path
```

The journal (owner-only, host control dir) holds `split{id, creator, cap,
snapshot, dir identity, state}` and `branch{label, run, placement
(shell-vm | kit:<command>), state, status, R}`. Receipts
gain `lineage`. `marsh splits` prints the tree (split, branch state,
placement, job id, changed-file count, and the reason a split was kept or
cancelled). `marsh status` (text and `--json`) counts active, awaiting, and
kept splits; its text counts only attached shells.

## 6. Lifecycle and cleanup

Split states: `run -> await -> joined | kept`, or `run -> cancelled`.
Unfinished work becomes `uncertain` on daemon restart. Section 12 lists the
public state strings.

- **Capture** happens once every writer is confirmed gone: the shell
  branch's cgroup is empty, every Kit job under it is cleaned up, and an
  argv branch's container is verified deleted (`CaptureStable`).
  Verification follows (section 8). A fork that fails it is **rejected**
  and removed, and its status says why (`ExposedClean`).
- **Lease**: an awaiting split is held by the create stream, then by a
  join. If it stays unheld for 60 s it becomes **kept**: forks retained,
  reported, and still joinable by the creator. Its creator can remove it.
- **Release**: `consumed(0)` without keep removes the forks, admin dirs,
  store, spool, and `out/`. Anything else keeps them, and the message says
  why and gives `marsh splits rm ID`. Nothing is removed
  while a process of its branch may live (`NoRemoveWhileLive`). Removal
  goes through directory fds opened at create, never by re-resolving a
  path.
- **Daemon restart**: the journal survives. Unfinished splits become
  uncertain. Launched branches become uncertain; their recorded unfinished
  jobs retain held process slots. Restart does not restore the adapter's
  in-memory quarantine set (`results.md`).
  Unlaunched branches never start. Capabilities are revoked. Forks are
  retained, reported, and flagged **untrusted** (no verification ran), and
  the same goes for unreaped forks after a cancel. Nothing is resumed or
  replayed (`NoReplay`, `WorkspaceRecorded`).
- **`git clean -xfd` hazard**: `.marsh/` is git-ignored, so the user's
  `git clean -xfd` (or `rm -rf`) deletes live forks. Before capture and
  release, the daemon re-checks the `(dev, ino)` of the split dir and each
  fork against the journal. On a mismatch the branch fails with
  `workspace replaced` and nothing is removed by path.

## 7. Failure semantics

- Setup failure (snapshot, fork, or a caller not entitled to create) is
  all-or-nothing: status 2 and nothing runs.
- **Cancel** (Ctrl-C, a client signal, stream drop, deadline, output limit,
  or creator branch exit; the message names which) revokes and signals the whole subtree in one step: SIGINT,
  10 s, SIGKILL. When every branch's end is confirmed the whole split
  directory is removed (the record stays as `cancelled` until `splits rm`);
  otherwise everything is kept and the message names the unconfirmed
  branches. Status 130; join does not run.
- Kit transport loss makes the branch
  uncertain and quarantines the VM. It is never re-run.

## 8. Snapshot, fork, and confinement

- **Git dirs come from the journal** (resolved once at create, through
  no-follow fds, and required to lie inside the project). The daemon never
  follows branch-written `alternates` or gitfiles. It copies the user's index
  bytes under a 64 MiB bound. A project root with a gitfile instead of a `.git`
  directory is refused; nested splits use the creator's journaled admin directory.
- **No gix or host Git execution.** `split_fs.rs` reads Git metadata and
  ignore rules as data. The `ignore` crate supplies global excludes; local
  `.git/info/exclude` and `.gitignore` rules also apply. Repository hooks,
  attributes, clean/smudge filters, and external diff commands never run.
  Snapshot files retain their raw bytes, including LFS pointer files.
- **Snapshot** (M2): clone admitted top-level entries into `<id>/base`,
  then walk the clone through no-follow directory descriptors. Prune ignored
  entries, special files, and nested `.git` entries. Ignore rules also exclude
  tracked files. If a copied file's size, mtime, or mode differs from its source,
  re-clone with a before/after stat check, up to three times; continued changes
  fail with `file kept changing: <path>`. The source of a nested split is its
  creator's fork (`ChildFromParent`).
- **Forks**: copy the frozen base once per branch (`OneBase`), using macOS
  `clonefileat(..., CLONE_NOFOLLOW)`. Only `Unsupported` falls back to
  descriptor-based copying. The base is never mounted into a job.
- **Git metadata per fork**: a gitfile `<fork>/.git` pointing at
  `<id>/.admin/<label>/`, which holds:
  - `HEAD` names `refs/heads/marsh-split`, initially holding H;
  - a daemon-written config with no hooks;
  - `info/`;
  - a copy of `packed-refs`;
  - `index` (the caller's index bytes);
  - `objects/`, whose alternates are `store.git` and the user's
    `.git/objects`.

  It is not a worktree of the user's repo, and the user's `.git` is never
  written.
- **Kit mounts for an argv branch**:

  | Path | Access |
  |---|---|
  | fork worktree | read-write |
  | `.admin/<label>/` (so Git can create `index.lock`, objects, refs) | read-write |
  | gitfile, `config`, `HEAD`, `info/`, `packed-refs` | read-only binds over the read-write fork and admin dir (`AdminReadOnly`) |
  | `store.git` and the user's `.git/objects` | read-only (`SnapshotImmutable`) |

  The selected guest home is also mounted read-write. No other project path
  is mounted (`NoWriteToUserTree`, `ForkIsolation`). `HEAD`
  names `refs/heads/marsh-split`, so `git commit` updates a writable ref
  and never `HEAD`. Capture also rejects any admin entry outside Git's own
  state files (`commondir`, `gitdir`, `config.worktree`, `hooks`, ...). An
  ordinary (unconfined) Kit job gets `<root>/.marsh` as a read-only bind.
  A Kit job whose project root itself lies inside a `.marsh` directory (a
  shell opened in a fork) is refused: it would mount the fork as a project
  without its Git metadata and with a writable gitfile. A host command run
  in a fork (`marsh run`, `split`) instead takes the split's root as its
  project and the fork as its working directory, so it is confined.
- **Capture verification**: the gitfile and admin files must be
  byte-identical to what the daemon wrote, and the fork may contain no
  nested `.git` entry at any depth. If either check fails, the branch is
  rejected, so a host `git -C <fork> status` or IDE never meets a
  branch-written `core.fsmonitor` or `hooksPath`.
- **Patch writer**: text hunks use `similar` in unified format with full blob
  IDs on `index` lines. Binary changes use Git's `GIT binary patch` / `literal`
  forward and reverse hunks (zlib + base85, no delta). Capture writes the changed
  blobs to `store.git`. The daemon never executes `git`, including
  `git diff --binary`, because:
  - stock macOS may have no `git`;
  - every exec of git keeps a config, attribute, and environment surface
    to audit;
  - literal hunks are enough for `git apply`.

  Renames show as delete + add.

## 9. Model scope

`model/Workspace.tla` covers nested splits created by any branch (nested
ones argv-only), snapshots from the caller's tree, trusted shell branches
(C1) and adversarial Kit branches (they write anything mounted, including
a nested `.git`), the session pool, creator-only join, ancestor and
session cancel, exit cascade, capture after confirmed exit with
verification, the lease with kept-and-joinable, open lanes (any number of
Kit jobs per lane, admitted only while open, close before capture), a
checkpoint as an immutable snapshot source, and one daemon crash between
any two steps. It checks 17 invariants plus `EventuallySettled`, split
across five positive configs to stay tractable (`model/README.md`).
Each negative control removes one guard in a real action. Not modeled: C1
itself, Git, bytes, and dir identity.

## 10. Non-goals and v1 limits

Non-goals: confining shell branches, `join --apply`, replay or resume,
Brush functions in branches, cross-daemon lineage, and Cloud execution.
Placement is recorded per branch (`shell-vm` or `kit:<command>`); only local
placements exist.

v1 limits: no nested shell branches, no `--with-ignored`, no `--shell`; the
job capability's split operations are `SplitCreate` and `SplitJoin`; one split pool per
creating session, with no finer-grained shares; no per-branch uid or per-branch byte quotas.

The `git clean -xfd` hazard (section 6) is reported, not prevented. Moving
`.marsh/split` outside `git clean`'s reach would need a new stock mount; the
daemon instead re-checks directory identity before capture and release.

## 11. Acceptance requirements

These requirements are maintained as scenarios in `workspaces-acceptance.md`;
that page and its harness are authoritative.

- A plain-bash pipeline with `-b` and `:::`, and the two-step handle.
- `join -- CMD` after a failed branch.
- A kept split joined after 60 s.
- **A Kit branch writes `core.fsmonitor` and `core.hooksPath` into its
  admin config and creates `sub/.git/config`.** The writes fail (read-only
  binds) or the branch is rejected. Then host `git -C <fork> status` runs
  nothing (verified with a marker file).
- A repo whose own config sets `core.fsmonitor` and filters: nothing runs
  on the host.
- A nested split from a Kit job, cancelled when the parent exits.
- A join attempt from a non-creator, refused.
- A background process delaying capture.
- `git clean -xfd` mid-split gives `workspace replaced`.
- A restart mid-split gives uncertain and untrusted, with no replay.

## 12. Decisions and JSON shapes

- **Host creator identity.** A host `marsh split` attaches a temporary
  session only so shell branches have a shell VM. Its creator is `host`: the
  handle is the join capability for host-level callers, so two host processes
  that hold the same handle are the same creator. A session (`session:<id>`,
  from its relay token) or a Kit job (`job:<id>`) can join only splits it
  created. The host is the operator for `splits rm` and `splits cancel`.
- **Depth.** A root split is depth 1; creating depth 4 is refused (status 2,
  `depth limit 3 reached`).
- **State strings.** Split `state` is one of `run`, `await`, `joined`,
  `kept`, `cancelled`, `uncertain`. A cancelled branch's status is
  `cancelled`.

Selected JSON fields follow. The complete serialized fields are defined by
`SplitRecord`, `BranchRecord`, and `SplitRecord::manifest` in
`crates/marsh-daemon/src/split.rs`; Git source fields come from `GitSource` in
`split_fs.rs`. `(optional)` fields may be absent, rather than `null`.

```
handle (split stdout, one line)  {"marsh_split":1,"id":"<id>"}

out/manifest.json, join --json, and each element of `splits --json`:
{"version":2,"id":"<id>","state":"await","status":3 (optional until settled),
 "creator":{"kind":"host"} | {"kind":"session","id":"<sid>"} | {"kind":"job","id":"<job>"},
 "session":"<pool session id>","parent":null | {"split":"<id>","label":"<label>"},
 "depth":1,"root":"<project>","dir":"<root>/.marsh/split/<id>","out":"<dir>/out",
 "objects":"<dir>/store.git/objects","cwd":"<relative cwd>","created_unix_ms":0,
 "reason":"workspace replaced" (optional),
 "git":{"head":"<H>","objects":["<object directory>",...]} (optional),
 "identity":[dev,ino],"timing_ms":{"snapshot":0,"forks":0,"run":0,"capture:<label>":0,"consumer":0},
 "finished_unix_ms":0 (optional, all branches captured),
 "branches":[{"label":"a","kind":"shell"|"argv","placement":"shell-vm"|"kit:<command>",
   "state":"pending"|"running"|"captured"|"rejected"|"failed"|"cancelled"|"uncertain",
   "status":"exited 0"|"failed: capacity"|"failed: <why>"|"rejected: <why>"|"cancelled" (optional),
   "code":0 (optional),"job_id":"<job>" (optional),"files":2,
   "trust":"untrusted" (optional),"fork":"<dir>/<label>",
    "started_unix_ms":0 (optional),"finished_unix_ms":0 (optional)}]}

marsh splits --json [ID]        {"splits":[<manifest>...]}   (host: all; session: its own tree)
marsh status --json             ... "splits":{"active":0,"awaiting":0,"kept":0}
marsh jobs show JOB --json      ... "lineage":{"split":"<id>","label":"<label>"} (split jobs only)
```

Branch codes: `exited N` is N, `failed:`/`rejected:` are 125, `cancelled`
is 130. `split` exits with the first nonzero code in declaration order.

## 13. Implementation bounds and process integration

- **Descriptor-based I/O.** `split_fs::Dir` enforces the no-follow operations
  used in section 8:
  - every directory is opened one `O_NOFOLLOW` component at a time from an
    open directory (`split_fs::Dir`);
  - files are created `O_CREAT | O_EXCL | O_NOFOLLOW`, and read only after
    an `O_NOFOLLOW | O_NONBLOCK` open and an `fstat` that says the file is
    regular (so a FIFO cannot block the daemon);
  - a planted symlink at `.marsh`, `.marsh/split`, `<id>`, `out/<label>`,
    or any `out/` file makes that step fail; it is never followed.
    `out/` that cannot be written is reported as
    `rejected: out/<label> was tampered with`.
- **Capture.** Same-size files whose mtime matches are still compared byte
  for byte when their ctime is at or after the fork's creation; a branch
  cannot set ctime, so an edit that restores the mtime is still seen. The
  limits are 32 MiB per file and 256 MiB of changed bytes in total
  (`rejected: capture limit`), with a 5 s deadline per text diff. Paths in
  `files` are C-quoted when they contain control characters, `"`, or `\`.
- **Writers before capture.** After a shell branch exits, capture waits
  up to 30 s until no Kit job of the branch's session is queued or running.
  Otherwise, or when the branch's transport is lost, the branch is
  `uncertain` and `untrusted`. Its fork is never verified or removed, and
  its pool slot stays held until `marsh workers reset`. A split with an
  uncertain branch is never removed by `join`; it is kept.
- **Rendering.** The text rendering replaces each binary hunk with
  `binary file changed: PATH (N bytes; full patch in
  $SPLIT_DIR/<label>/diff.patch)`; `diff.patch` keeps the literal hunks.
- **Ignored output.** In a Git project, capture skips each path the fork
  holds but the snapshot does not when the ignore rules exclude it
  (`core.excludesFile`, the fork's `info/exclude`, and the fork's
  `.gitignore` files, deepest first; an excluded new directory is skipped
  whole). A path present in the snapshot is always compared.
  `.marsh/.gitignore` is written only inside a Git worktree.
- **Environment filtering.** Shell branches receive only forwardable exported
  variables. The filter excludes:
   - `MARSH_*`, `SBX_*`, or `DOCKER_*`;
   - placement-bound or host-only variables;
   - credential-shaped names (`*KEY*`, `*SECRET*`, `*PASS*`, `*TOKEN*`,
    `*CREDENTIAL*`, `*COOKIE*`, `AWS_*`, `*_PAT`).
   The match is deliberately by substring, ignoring case: it also drops
   ordinary names such as `KEYMAP` or `BYPASS_PROXY`. A dropped variable is
   one the branch must be given another way; a credential that slipped
   through would sit in an agent's VM. Host-only values (`TMPDIR`, `PWD`,
   `SHELL`, ...) name Mac paths and are wrong in a VM.
- **Capability socket.**
  - It admits at most 12 open connections per attempt, including idle ones;
    an excess connection receives a refusal frame.
  - Each connection's queue is bounded in both directions (64 chunks), so
    a job that stops reading is disconnected instead of stalling the
    worker transport.
  - The daemon raises its descriptor soft limit (up to 4096) for its 128
    connections.
- **Nested splits from a shell branch.** `marsh split` run inside a shell
  branch arrives on that branch's session relay. The daemon maps the
  session to the branch, so the split is a child: argv-only, depth + 1,
  and cancelled when the branch exits.
- **Join and release.** If the join reply cannot be written or no release
  arrives, the split returns to `kept`. A released split is renamed into
  `.marsh/.trash` through descriptors, after the identity check, and
  deleted in the background. Bare `join` escapes control bytes when its
  stdout is a terminal.
- **Retention.** A kept split is removed 7 days after it was kept
  (`kept_unix_ms`), unless a branch is uncertain. `marsh splits` lists kept
  splits with their state and times, and `marsh splits rm ID` removes one
  now (the host can remove any). Removals go through `.marsh/.trash`, which
  the daemon sweeps at startup for every project it has journaled.
- **Kit VM limit.** Splits add no Kit VM cap of their own. Process admission
  caps distinct live Kit VMs per job tree (`processes.md`, section 6).

## 14. Open lanes, checkpoints, and storage

**Checkpoints.** `marsh checkpoint [--label TEXT] [--json]` snapshots the
project exactly as a split's `base/` is made (section 8) into
`<root>/.marsh/checkpoints/<id>/tree`, and saves the Git metadata a fork needs
(resolved `HEAD`, `index`, `packed-refs`, `info/exclude`) in `<id>/git`. The
daemon journals each checkpoint (`checkpoints.json` in the control directory,
with the directory's identity) and reaches its files only by descriptor.

- Retention runs on every create and at daemon start: the newest 20 per
  label (unlabeled checkpoints are one label), none older than 7 days, at
  most 200 per project. A retired checkpoint goes to `.marsh/.trash`.
- A create is refused with exit 75 when the volume has less than 5 GiB or
  less than 5% free.
- Creating anything under `.marsh` marks it excluded from Time Machine (the
  sticky `com.apple.metadata:com_apple_backup_excludeItem` attribute) and
  adds `.metadata_never_index`.
- A checkpoint is immutable: no job mounts it (`CheckpointImmutable`), and a
  Kit job whose cwd is in one is refused like any non-fork path under
  `.marsh`.
- `marsh checkpoints [--json] [--label TEXT]` lists them newest first
  (`{"checkpoints":[{"checkpoint","label","created_unix_ms","root","dir","bytes"}]}`);
  `marsh checkpoints rm ID` removes one.
- A split being made `--from` a checkpoint leases it until its `base/` is
  cloned: retention passes over a leased checkpoint (the next pass retires
  it), and `rm` of one fails with "in use by a split being made from it;
  retry".
- Every project that gets a `.marsh` area is journaled
  (`project-roots.json`) before the area is made; at daemon start each
  one's `.marsh/.trash` is emptied, and a project with no splits, no
  checkpoints, and an empty trash is forgotten.

**`--from CHECKPOINT`.** A root split may start from a checkpoint of its
project: the daemon clones the checkpoint's `tree` into `base/` and builds
every fork's `.admin` from the checkpoint's saved Git metadata, with the
project's `.git/objects` as alternates. The manifest records `from`.

**Open lanes.** `marsh split --open [--from C] LABEL...` (host or session
shell only; not from a job or a branch) makes a split whose branches have no
command: each is a lane (`kind` and `placement` `lane`, state `running`). It
replies at once with `{"marsh_split":1,"id":ID,"lanes":{LABEL:FORK}}`. The
split stays `run`, with no lease and no deadline, until it is closed or
cancelled.

- **Lane jobs.** A Kit job whose cwd is in a lane's fork is confined to it
  (section 8). The daemon admits it only while the split is `run`, not
  closing, the lane is still open, and the caller is the split's creator
  (`host` for a host split, the session for a session split); otherwise it
  is refused (exit 125) before any effect. The admitted job is counted live
  in the journal (`live` on the lane) until its `Execute` ends, and recorded
  with lineage `split:<id>/<label>`. A lane runs any number of jobs, long and
  interactive ones included (`marsh run --acp NAME`). Their children are
  admitted with them. Admission comes before the Kit is prepared, so a close
  or cancel, in the same locked step that sets `closing`, also fences the
  split: from then on no job whose mounts name one of its forks is recorded
  or submitted. The step is then journaled. It is never undone: if that
  write fails, the split stays fenced and `closing` in memory, the error
  says so, and repeating `marsh split close ID` or `marsh splits cancel ID`
  resumes it and writes the journal again. A restart before that retry
  depends on what reached disk: once `closing` is in `splits.json` (the
  second journal file, the lineage, failed) the split is `kept` and its
  lanes `uncertain`; if the first write failed before its rename, disk still
  says open and the split is preserved as open. A close or cancel already running refuses a second one.
- **No split from a lane job.** A job's split is planned from the job's
  recorded mounts, never its cwd: a job (or any descendant, which inherits
  its parent's mounts) whose mounts name a branch fork nests in that fork,
  and one whose mounts name a lane is refused ("a job in a lane cannot start
  a split; split from the host or a session shell"), whatever path it
  passes. A job whose recorded fork's split is gone is refused, never given
  the project.
- **Several agents in one lane.** Jobs of any Kit run in a lane at once (a
  checkpoint's copy shared by N agents): every Kit VM mounts the same project
  root (identical sources are shared, `ForkIsolation` is the container's view)
  and each job's container sees only the fork. The per-tree Kit VM cap and
  the same-Kit chain bound a job's descendants, not sibling lane jobs, which
  are separate trees. A shell session opened in the fork (a terminal whose cwd
  is the fork) is not a lane job: it mounts the fork as a project of its own,
  which overlaps the Kit jobs' mount of the project root and is refused while
  one holds it ("overlaps active grant"); that overlap rule is a safety rule
  and stays. Host `marsh fanout` in a lane's copy therefore does not open a
  shell session there: each branch is `marsh run NAME ARG...` from the copy (a
  confined lane job, counted live like any other), registered names only; a
  `-b` shell branch is refused, since a shell would see the whole project.
- **`marsh split close ID [--json]`** (creator, or the host): stops admitting
  lane jobs, waits up to 30 s for the lane's live jobs (and any job whose
  mounts name its fork) to end, then captures each open lane exactly as a
  branch is captured. A lane whose jobs did not end, or ended with cleanup
  uncertain, is `uncertain` and `untrusted`, never verified or removed. A
  captured lane's status is `closed` (code 0). The split becomes `kept` and
  the reply is the manifest; `out/<label>/{diff.patch,files,status}` hold the
  results.
- **`marsh split apply ID LABEL [--3way]`** (the split's creator) applies a
  captured lane's `diff.patch` to the project with `git apply` inside the
  shell VM (from the host, through `marsh -c`), with the split's
  `store.git/objects` as `GIT_ALTERNATE_OBJECT_DIRECTORIES`. It is a join
  that keeps the split: while it runs the split is `joined`, so neither
  `splits rm` nor the collector removes the patch under it, and the release
  keeps it with a fresh 7 days. A lane with no changes applies nothing.
- **Cancel.** `marsh splits cancel ID` on an open split stops admission
  (journaled `closing`, and the fence), cancels every live lane job tree
  (INT, then KILL after 10 s). A job marked cancelled before it could be
  signalled (its Kit still being prepared) keeps the cancel: it is
  delivered (INT, then KILL after 10 s) the moment the job's container
  registers, and the split waits for that job too. Once every lane job's end
  is confirmed, removes the split's files; otherwise the split is kept with
  the unconfirmed lanes `uncertain`. A closing split cannot be cancelled. On
  a kept or uncertain lane split, cancel ends the lane jobs a close timed
  out on; the lanes stay `uncertain`.
- **Removal.** `marsh splits rm ID` (and the 7-day kept-split collector)
  never removes a lane split while a lane job may still write: it refuses
  while a lane job runs ("still has a running lane job; end it with `marsh
  splits cancel ID`") or while one's end is unconfirmed and its Kit was not
  reset since ("run `marsh workers reset all`"). `splits rm` and the
  collector check and claim the split (taking its record) in one step under
  the lock `join` takes it with, so a join or `split apply` either wins (the
  split is `joined` and the removal is refused; if kept again its 7 days
  restart) or finds it gone.
- **Restart.** `closing` is journaled. An open lane split whose lanes had
  no live job and no close under way stays open; one with a live lane job
  becomes `uncertain` (section 6). A close or cancel the restart
  interrupted leaves the split `kept`, its still-open lanes `uncertain`
  ("daemon restarted while closing"), forks in place; it never reopens.

**Storage.** `marsh storage [--json] [--all]` reports, for the current
project (every journaled project with `--all`):
`{"projects":[{"root","checkpoints":{"count","bytes"},"kept_splits":{"count","bytes","splits":[{"id","state","bytes","kept_unix_ms"}]},"volume":{"free_bytes","total_bytes"}}]}`.
Bytes are logical (APFS clones share blocks until they diverge).

**Events.** Split changes are published to `marsh events` (`results.md`) as
`{"seq","type":"split","split":{"id","state","root","branches":[{"label","state","job_id"}]}}`
(state `removed` when the record is gone), checkpoint changes as
`{"seq","type":"checkpoint","checkpoint":{"id","root","label","removed"}}`.
At most 8 subscribers are connected at once (each holds one of the
daemon's 128 connections); a ninth is refused with "marsh events: at most 8
subscribers at once", and ordinary requests are never held off.

The model covers lanes (many jobs per lane, admission only while open, close
before capture, restart, restart during close, cancel and removal of a kept
lane split), lane jobs never splitting, and the checkpoint as an immutable
source: `Workspace_lanes_pass.cfg`, and the controls `laneAfterClose`,
`captureLiveLane`, `laneRestartLive`, `checkpointMounted`, `laneSplit`,
`restartReopens`, and `removeLiveLane`. The checkpoint lease, the event
subscriber cap, and the collector/join race are checked in code tests
(`checkpoint.rs`, `crates/marsh/tests/workspace_cli.rs`).
