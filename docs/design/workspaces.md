# Workspaces: daemon-owned snapshots, forks, and split lineage (design)

Status: implemented (v1 scope; revision 3 of the design). Revisions 2 and 3
answer two adversarial reviews; section 15 has the disposition. This
replaces the earlier shell-side workspace hook (the old `split-join.md`).
The user guide is `split.md`. Model: `model/Workspace.tla`.
Acceptance: `workspaces-acceptance.md` (W01-W24). Section 16 records the
resolved open questions and JSON shapes; section 17 lists where the code
differs from this text.

## 1. Concepts and trust

The host daemon is the kernel. It snapshots, forks, runs every branch,
captures results, and keeps lineage. Clients (CLI, Brush sugar, an agent
in a Kit job) only make requests and render.

- **Snapshot**: a frozen base tree plus `S` (with `H` and index tree `I` in
  a repository) hashed into a per-split object store
  `<root>/.marsh/split/<id>/store.git` and pinned by `refs/marsh/*`. It is
  taken from the caller's tree: the user's tree for a session, or the
  creator branch's fork for a nested split.
- **Fork**: a private copy-on-write clone of the base per branch,
  `<root>/.marsh/split/<id>/<label>`. All splits, nested ones included,
  are siblings under `<root>/.marsh/split/`, inside the existing project
  mount. No new stock mount is needed. The journal records the parent.
- **Result**: after every writer is gone and the fork has been verified,
  the daemon writes the `out/` artifacts (section 5). Consumers read
  `out/`, never live git admin dirs. marsh never writes the caller's tree;
  the consumer applies what it wants.
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
    read-write like every Kit job (agents keep credentials and session state
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

- `-b LABEL=STRING`: `SHELL -c STRING`, where `SHELL` is the session's
  front-end shell (`marsh` today, bash or zsh when those front ends
  exist). It runs as its own supervisor attempt (its own cgroup) with cwd
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
  setup), and prints one handle line `{"marsh_split":1,"id":"…"}` (it
  renders instead when stdout is a terminal).
- `marsh join [--json] [--keep] [-- CMD ARGS...]` reads a handle and is
  accepted only from the split's **creator** (the same session, or the same
  job). It works on awaiting and kept splits.
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
  for any).
- **Host CLI** (a plain Mac terminal): `marsh split` opens an ephemeral
  session (`AttachEphemeralShell` path, no TTY) so shell branches have a
  shell VM. `SplitCreate` carries the exported environment, minus
  `MARSH_*`, `SBX_*`, `DOCKER_*`, and relay or bearer tokens.
- **Kit containers inside a split branch** get `marsh` on `PATH` (the
  `marsh-local` binary), so the same command works nested. Nested splits
  take **argv branches only** in v1.

**Brush sugar.** `P | split { a: X; b: Y } | join | C1 | C2` is `SplitCreate`
with shell branches `X` and `Y` (the spool is P's output), a wait, then
`C1 | C2` with `SPLIT_*` pushed, then `SplitRelease(consumed(last
status))`. Every sugar branch is a shell branch (a `::: CMD ARGS` line is
not recognized; use the CLI for argv branches).
Branches are label-led (Brush `0041`; `split.md`): `;` starts a branch only
before a `LABEL:` word, so `a: cd x; make` is one branch `cd x; make`.
Labels, `PIPESTATUS`, and Ctrl-C stay as they are; Brush forks nothing.

**Terminal and signals.** Branches have no controlling terminal (`/dev/tty`
gives ENXIO) and their stdin is the spool. Ctrl-C at the client sends
`SplitCancel`. Ctrl-Z stops only the client while branches keep running,
and `fg` resumes the wait. Client death or SIGHUP before capture drops the
stream, and a dropped stream cancels the split.

## 3. Daemon requests

| Request | From | Payload -> reply |
|---|---|---|
| `SplitCreate` | relay, socket | `cwd`, `branches: [{label, shell(string) \| argv([..])}]`, `env`, `keep`, streamed stdin -> `split`, events, manifest. Dropping the stream before capture cancels |
| `SplitJoin` | relay, socket | `split`, then `consumed(status) \| keep` -> manifest and `out/` contents (streamed to callers that cannot see the dir). Creator only; this is also the release |
| `SplitCancel` | relay | `split` -> ack once the subtree is revoked and signalled. The session may cancel any of its splits; a shell branch may cancel splits below it |
| `SplitShow` | relay | `split?` -> manifest + subtree, or every split in scope |
| `SplitRemove` | relay | `split` -> removes a kept split (creator or operator) |

The capability socket admits only `SplitCreate` and `SplitJoin`. Frames
are credited per attempt. `processes.md` (design) generalizes it into one
job capability for every Kit job, with split as one client.

## 4. Budgets and capability

- **One session pool**: 8 concurrent processes and 4 Kit VMs per session,
  counting shell branches, argv branches, Kit jobs started from shell
  branches, and every nested level. Admission happens at launch. When the
  pool is full, the branch fails visibly (`failed: capacity`) and join
  still runs (`BudgetBound`). A slot frees only when its process's end is
  confirmed (captured, rejected, or reaped). Uncertain and unreaped slots
  stay held until `marsh workers reset`.
- **Caps**: depth 3, 16 branches per split, 1 h per split. Spool (64 MiB)
  and captured output (16 MiB) are hard limits; crossing either cancels
  the split. There are no per-branch reservations or byte quotas in v1.
- **Capability**: a daemon record per branch `{split, creator, live|revoked}`,
  never a bearer secret. It is revoked at the branch's exit or crash, and
  that cancels everything below it (`LiveUnderLive`, `CancelPropagates`).
  Every daemon restart revokes all of them.
- **Delivery to a Kit job** (only for argv branches and for Kit jobs
  started inside a split branch): the worker binds a per-attempt Unix
  socket at `/run/marsh/cap.sock` and forwards its frames, tagged with the
  attempt id and credited per attempt, over the existing worker transport.
  The socket has no token and dies with the container. It accepts only
  `SplitCreate` (creator = this job) and `SplitJoin` (only for splits this
  job created). `AGENTS.md` now reads: "Jobs get no Docker, containerd, or
  SBX socket; their only daemon channel is the scoped split capability
  socket." This is the dev broker shape: narrow verbs, an attempt-bound
  channel, a persisted record, and revocation at end and at restart.

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
(shell-vm | kit:<vm> | later cloud:<lease>), state, status, R}`. Receipts
gain `lineage`. `marsh splits` prints the tree (split, branch state,
placement, job id, changed-file count, and the reason a split was kept or
cancelled). `marsh status` (text and `--json`) counts active, awaiting, and
kept splits; its text counts only attached shells.

## 6. Lifecycle and cleanup

Split states: `run -> await -> done | kept`, `run -> cancel -> done`; any
active state becomes `uncertain` on daemon restart.

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
  uncertain. Launched branches become uncertain (their VM is quarantined).
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
- **Cancel** (Ctrl-C, stream drop, deadline, output limit, or creator
  branch exit) revokes and signals the whole subtree in one step: SIGINT,
  10 s, SIGKILL. When every branch's end is confirmed the whole split
  directory is removed (the record stays as `cancelled` until `splits rm`);
  otherwise everything is kept and the message names the unconfirmed
  branches. Status 130; join does not run.
- Kit transport loss or (later) Cloud lease loss makes the branch
  uncertain and quarantines the VM. It is never re-run.

## 8. Snapshot, fork, confinement (host, in-process gix)

- **Git dirs come from the journal** (resolved once at create, through
  no-follow fds, and required to lie inside the project). The daemon never
  follows `alternates` or gitfiles found in forks. It parses the user's
  index with caps (64 MiB, 1M entries).
- **Isolated gix**: it reads no repo, system, or global config except
  `core.excludesFile` (read as data from the user's `~/.gitconfig`, no
  includes), plus XDG `git/ignore`, `.git/info/exclude`, and `.gitignore`
  files. No attributes, filters, hooks, or `GIT_*` environment are honored.
  The object directories are passed explicitly. Filters such as LFS and
  eol are not applied, so `S` holds raw bytes.
- **Snapshot** (M2): walk with `openat(O_NOFOLLOW)` and
  `fstatat(AT_SYMLINK_NOFOLLOW)` (symlinks recorded, special files
  skipped). `clonefile` each file into `<id>/base`, lstat the source
  before and after, and re-clone up to 3 times if it changed (then fail
  with `file kept changing: <path>`). Then hash the frozen base. A file's
  oid is reused from the user's index when its pre-clone stat matches the
  index entry **and** the object's size equals the file size (which guards
  against filtered blobs); otherwise the file is hashed. New objects go into
  `store.git`. The snapshot comes from the caller's tree: the creator's
  fork for a nested split (`ChildFromParent`).
- **Forks**: one `clonefile(base, <id>/<label>)` per branch (`OneBase`).
  The base is never mounted into a job.
- **Git metadata per fork**: a gitfile `<fork>/.git` pointing at
  `<id>/.admin/<label>/`, which holds:
  - `HEAD` = H;
  - a daemon-written config with no hooks;
  - `info/`;
  - a copy of `packed-refs`;
  - `index` (from I, stat data from the clone);
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

  Nothing else is mounted (`NoWriteToUserTree`, `ForkIsolation`). `HEAD`
  names `refs/heads/marsh-split`, so `git commit` updates a writable ref
  and never `HEAD`. Capture also rejects any admin entry outside Git's own
  state files (`commondir`, `gitdir`, `config.worktree`, `hooks`, ...). An
  ordinary (unconfined) Kit job gets `<root>/.marsh` as a read-only bind.
- **Capture verification**: the gitfile and admin files must be
  byte-identical to what the daemon wrote, and the fork may contain no
  nested `.git` entry at any depth. If either check fails, the branch is
  rejected, so a host `git -C <fork> status` or IDE never meets a
  branch-written `core.fsmonitor` or `hooksPath`.
- **Patch writer** (decision): text hunks use gix's blob diff
  (imara-diff) in unified format. Binary changes use git's `GIT binary
  patch` / `literal` hunks (zlib + base85, no delta), written by marsh in
  about 150 LOC. The daemon never executes `git`. Hardened host `git diff
  --binary` was rejected for three reasons:
  - stock macOS may have no `git`;
  - every exec of git keeps a config, attribute, and environment surface
    to audit;
  - literal hunks are enough for `git apply`.

  Renames show as delete + add.

## 9. Cloud mapping (designed in, not built)

Placement is recorded per branch. A Cloud-placed argv branch ships the
objects of `S` the VM's cache lacks (once per snapshot and VM), runs on an
overlay or reflink of `S`, and returns new objects plus `out/<label>/`.
There is no live sync. The patch-only `out/` contract is exactly what local
nested splits already use. This stays out of the model until it is built.

## 10. Migration

1. Daemon split module with journal, requests, lease, and pool. Port the
   rendering and manifest code from `split_workspace.rs`.
2. Snapshot, fork, capture, and verification with gix. The patch writer.
3. Shell branches as supervisor attempts; argv branches as `Execute`.
   Adapt `split_confinement.rs` mounts.
4. CLI (host, guest, `marsh-local` as `marsh` in Kit containers), and the
   ephemeral session for the host CLI.
5. Worker capability socket.
6. The Brush hook becomes a thin client (delete branch forking from
   `split_interp.rs`). Delete the guest `split_workspace.rs`. Update
   the user guide (`split.md`), `architecture.md`, the contract, and `CONTRACT.md`.
   Fold `Split.tla` in.

## 11. Acceptance additions

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

## 12. Model

`model/Workspace.tla` covers nested splits created by any branch (nested
ones argv-only), snapshots from the caller's tree, trusted shell branches
(C1) and adversarial Kit branches (they write anything mounted, including
a nested `.git`), the session pool, creator-only join, ancestor and
session cancel, exit cascade, capture after confirmed exit with
verification, the lease with kept-and-joinable, and one daemon crash
between any two steps. It checks 16 invariants plus `EventuallySettled`,
split across four positive configs to stay tractable (`model/README.md`).
Each negative control removes one guard in a real action. Not modeled: C1
itself, Git, bytes, dir identity, and Cloud.

## 13. Implementation size budget (≤ ~5k Rust LOC, excluding tests)

| Component | LOC (budget) | Built (non-test Rust, `+added/-removed`) | Reuses |
|---|---|---|---|
| Daemon split module: journal, requests, lease, pool, cancel cascade, lineage in receipts | 900 | `split.rs` 1,554 (with branch execution), `lib.rs` +205 | ownership-map atomic store (`vm_ownership.rs`), receipt journal, request authority plumbing |
| Snapshot, fork, capture: walk, ignores, clonefile, `store.git`, admin writer, verification | 1,100 | `split_fs.rs` 1,166 (descriptor layer, snapshot walker, patch writer); `marsh-host-identity` `clone_at` +57 | `ignore` crate as data, `std::fs::copy`/`clonefile` |
| Patch writer (text + literal binary), manifest, `out/`, rendering | 550 | in `split_fs.rs` / `split.rs` | `similar`, `flate2`, `sha1` |
| Branch execution: shell attempts, argv `Execute`, signals, capture trigger | 550 | in `split.rs` | the daemon's own `OpenShell`/`Execute` endpoint |
| Kit confinement: read-only admin binds, mount set | 200 | `split_confinement.rs` 123 (net -138), backend +95, runtime +28, contracts +20 | `JobMount::subpath`, `resolve_grant_subpath` |
| Capability socket: worker bind, forwarding, allowlist, credits | 400 | worker 209 + 55, backend +115, daemon +105, sbx +19, contracts +6 | worker transport frames |
| CLI `split`/`join`/`splits`, host ephemeral session, `marsh` shim in Kit | 500 | `split_cli.rs` 642 (with the sugar client), `main.rs` +73, `marsh_local.rs` +37 | `AttachShell`, `marsh-local` |
| Brush thin client (net of deletions in `split_interp.rs`) | 200 | +105/-571 (patch `0035`) | existing parser, `PIPESTATUS`, and env push |
| **Total** | **4,400** | **about +4.5k non-test (review fixes added ~0.85k net); -2.4k non-test removed (`split_workspace.rs` -1,673, Brush -571); tests -0.9k superseded, +0.4k boundary tests** | |

**v1 cuts**: no nested shell branches, no `--with-ignored`, no `--shell`;
socket verbs are only Create and Join; one pool instead of shares; no
`snapshot_torn`, no Cloud, no per-branch uid. Kept: the journal, placement
strings, `store.git`, and patch results.

## 14. Non-goals and open questions

Non-goals: confining shell branches, Cloud execution, `join --apply`,
replay or resume, Brush functions in branches, cross-daemon lineage.

Open:

1. Lease timeout (60 s proposed).
2. Whether `.marsh/split` should move outside `git clean`'s reach. That
   needs a new mount and is rejected for v1; identity checks report the
   hazard instead.

## 15. Review disposition

| Round | Finding | Resolution |
|---|---|---|
| 1 | C1 host git runs repo config/filters/hooks | isolated gix, no git exec (8) |
| 1 | C2 child forks inside parent mount | sibling forks, parent in journal; `ForkIsolation`/`nestedFork` |
| 1 | C3 capture races writers; rw user objects | capture after confirmed exit; `store.git` read-only; user `.git` never written (6, 8); `CaptureStable`, `SnapshotImmutable` |
| 1 | H1 parent exit, H2 nested source, H4 lease | cascade cancel (`orphanChildren`); snapshot from parent fork (`ChildFromParent`); lease (`noLease`) |
| 1 | H3 vacuous controls | environment actions; every control removes a guard |
| 1 | M1–M5 socket, quota, bytes, pins, drift | superseded in round 2 by the pool, the allowlist, and stat-checked cloning |
| 2 | CRITICAL Kit-writable git metadata in the user tree | admin files bound read-only, only `objects/` and `index` writable; verify gitfile and admin, reject nested `.git`; consumers read `out/` (5, 8); acceptance test (11); `AdminReadOnly`/`adminRW`, `ExposedClean`/`noCaptureCheck` |
| 2 | H1 shell branches unconfined | the branch's own code is documented as trusted; Kit commands it starts are confined by cwd (8); example fixed (1, 2) |
| 2 | H2 ancestor join/release | creator only; ancestors get show/cancel; `ancestorJoin` -> `NoWriteToUserTree` |
| 2 | H3 following fork git dirs | git dirs from the journal, alternates never followed, index caps (8) |
| 2 | H4 host CLI has no shell VM | ephemeral session; env without tokens (2) |
| 2 | H5 expiry loses results | kept and joinable; creator removes; two-step form (2, 6) |
| 2 | M1 shares too complex | one session pool + depth + fan-out caps; Kit jobs from shell branches counted (4); `BudgetBound`/`freeOnRevoke` |
| 2 | M2 snapshot method | stat-checked clonefile, then stat-cache hashing of the frozen base; `snapshot_torn` dropped (8) |
| 2 | M3 ignore sources | `core.excludesFile` and XDG ignore read as data (8) |
| 2 | M4 `git clean -xfd` | hazard stated; dir identity re-check; fd-based removal (6) |
| 2 | M5 two shell semantics | one: session shell for `-b`; nested argv-only; sugar losses listed (2) |
| 2 | M6 AGENTS.md vs socket | rule amended; per-attempt credits (4) |
| 2 | Lows | declaration order; join runs after failures; pipefail; `SPLIT_OBJECTS` root-only; exact `out/` layout; regular depth-3 config; `marsh` shim in Kit |
| 2 | Size (13k estimate) | v1 cuts; budget 4.4k (13) |
| 2 | Binary patch | own literal-hunk writer; no git exec (8) |

Rejected in round 2: none. Partial: the `git clean` hazard is reported, not
prevented (open question 2).

## 16. Decisions and JSON shapes (implementation)

Resolved for v1 (GM decisions on the acceptance open questions):

- **Host creator identity.** A host `marsh split` attaches an ephemeral
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

Shapes (all keys always present unless marked optional):

```
handle (split stdout, one line)  {"marsh_split":1,"id":"<id>"}

out/manifest.json, join --json, and each element of `splits --json`:
{"version":2,"id":"<id>","state":"await","status":3,
 "creator":{"kind":"host"} | {"kind":"session","id":"<sid>"} | {"kind":"job","id":"<job>"},
 "session":"<pool session id>","parent":null | {"split":"<id>","label":"<label>"},
 "depth":1,"root":"<project>","dir":"<root>/.marsh/split/<id>","out":"<dir>/out",
 "objects":"<dir>/store.git/objects","cwd":"<relative cwd>","created_unix_ms":0,
 "reason":"workspace replaced" (optional),
 "git":{"head":"<H>","index":"<path>"|null,"packed_refs":"<path>"|null,"objects":[...]} | null,
 "identity":[dev,ino],"timing_ms":{"snapshot":0,"forks":0,"run":0,"capture:<label>":0,"consumer":0},
 "finished_unix_ms":0 (optional, all branches captured),
 "branches":[{"label":"a","kind":"shell"|"argv","placement":"shell-vm"|"kit:<command>",
   "state":"pending"|"running"|"captured"|"rejected"|"failed"|"cancelled"|"uncertain",
   "status":"exited 0"|"failed: capacity"|"failed: <why>"|"rejected: <why>"|"cancelled" (optional),
   "code":0 (optional),"job_id":"<job>" (optional),"files":2,
   "trust":"untrusted" (optional),"fork":"<dir>/<label>",
   "started_unix_ms":0,"finished_unix_ms":0 (optional)}]}

marsh splits --json [ID]        {"splits":[<manifest>...]}   (host: all; session: its own tree)
marsh status --json             ... "splits":{"active":0,"awaiting":0,"kept":0}
marsh jobs show JOB --json      ... "lineage":{"split":"<id>","label":"<label>"} (split jobs only)
```

Branch codes: `exited N` is N, `failed:`/`rejected:` are 125, `cancelled`
is 130. `split` exits with the first nonzero code in declaration order.

## 17. Implementation notes (differences from the design text)

- **No gix; descriptors only.** The daemon reads no Git configuration,
  never executes `git`, and never resolves a path inside the guest-writable
  `<root>/.marsh` or the user's `.git`:
  - every directory is opened one `O_NOFOLLOW` component at a time from an
    open directory (`split_fs::Dir`);
  - files are created `O_CREAT | O_EXCL | O_NOFOLLOW`, and read only after
    an `O_NOFOLLOW | O_NONBLOCK` open and an `fstat` that says the file is
    regular (so a FIFO cannot block the daemon);
  - files and trees are cloned with `clonefileat(..., CLONE_NOFOLLOW)`
    (`marsh-host-identity::clone_at`, the one `unsafe` call);
  - a planted symlink at `.marsh`, `.marsh/split`, `<id>`, `out/<label>`,
    or any `out/` file makes that step fail; it is never followed.
    `out/` that cannot be written is reported as
    `rejected: out/<label> was tampered with`.
- **Snapshot without object hashing.** The fork's `index` is the byte
  content of the caller's index, `HEAD` names `refs/heads/marsh-split`
  holding `H`, and `store.git` holds only the pre- and post-image blobs of
  changed paths, for `git apply --3way` with `SPLIT_OBJECTS`. Ignore rules
  are parsed as data (`.gitignore` at every level, `.git/info/exclude`,
  the global excludes file). A tracked file that matches an ignore rule is
  not snapshotted.
- **Snapshot (M2).** Top-level entries that the root rules do not ignore
  are cloned whole. The clone is then walked by descriptor, which prunes:
  - ignored entries;
  - special files;
  - every nested `.git` (a vendored repository or submodule checkout).

  A cloned file whose size, mtime, or mode differs from the source is
  re-cloned with a stat check before and after, up to three times.
- **Project must be a main checkout.** A project whose `.git` is a file (a
  linked worktree or submodule) is refused at create with that reason.
- **Capture.** Same-size files whose mtime matches are still compared byte
  for byte when their ctime is at or after the fork's creation; a branch
  cannot set ctime, so an edit that restores the mtime is still seen. The
  limits are 32 MiB per file and 256 MiB of changed bytes in total
  (`rejected: capture limit`), with a 5 s deadline per text diff. Paths in
  `files` are C-quoted when they contain control characters, `"`, or `\`.
- **Writers before capture (H1).** After a shell branch exits, capture waits
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
  whole). A path the snapshot has is always compared, like a tracked file. `.marsh/.gitignore` is written only inside a Git worktree.
- **Patch writer.** Text hunks come from the `similar` crate in unified
  format with full blob ids on `index` lines. Binary hunks are `literal`
  forward and reverse hunks (zlib + base85).
- **Shell branches** run as `marsh --marsh-guest -c SCRIPT` in a fresh
  session attached with the creator's authority, through the daemon's own
  `OpenShell`. Only forwardable exported variables are passed:
  - not `MARSH_*`, `SBX_*`, or `DOCKER_*`;
  - not placement-bound or host-only variables;
  - not credential-shaped names (`*KEY*`, `*SECRET*`, `*PASS*`, `*TOKEN*`,
    `*CREDENTIAL*`, `*COOKIE*`, `AWS_*`, `*_PAT`).
- **Argv branches** are `Execute` requests through the daemon's own
  endpoint, with cwd in the fork. Kit confinement follows from that cwd
  (`split_confinement.rs`).
- **Host CLI session.** The host CLI attaches an ordinary session only for
  the split's duration. All host CLI splits share one pool (`host`).
- **Capability socket.**
  - It is given to every Kit job in a fork, but the daemon admits
    `SplitCreate` only from a job that is an argv branch.
  - It admits at most 4 open connections per attempt.
  - Each connection's queue is bounded in both directions (64 chunks), so
    a job that stops reading is disconnected instead of stalling the
    worker transport.
  - The daemon raises its descriptor soft limit (up to 4096) for its 128
    connections.
- **Nested splits from a shell branch.** `marsh split` run inside a shell
  branch arrives on that branch's session relay. The daemon maps the
  session to the branch, so the split is a child: argv-only, depth + 1,
  and cancelled when the branch exits.
- **Deadline and cancel.** The 1 h split deadline cancels the subtree.
  Ctrl-C in the Brush sugar (`split { } | join`) cancels the split the same
  way the CLI does.
- **Join and release.** If the join reply cannot be written or no release
  arrives, the split returns to `kept`. A released split is renamed into
  `.marsh/.trash` through descriptors, after the identity check, and
  deleted in the background. Bare `join` escapes control bytes when its
  stdout is a terminal.
- **Retention.** Kept splits are not collected automatically. `marsh
  splits` lists them with their state and creation time, and `marsh splits
  rm ID` removes one (the host can remove any).
- **Not built in v1:**
  - the 4-Kit-VM-per-session cap;
  - counting Kit jobs started from shell branches in the pool (they are
    waited for at capture, but not admitted against it);
  - terminal rendering by `marsh split` (it always prints the handle).
