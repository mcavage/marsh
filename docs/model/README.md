# Lean cross-component model

Six small TLA+ modules specify the cross-component state of the lean product.
They are bounded executable specifications, not proofs that the Rust code refines them.

- `Ownership.tla`: one daemon per selected home, stock SBX as a name -> VM table that
  other actors change at any time, the persisted ownership map, persisted create intent
  (intent-then-adopt create), daemon restart, the cached inventory, explicit operator
  Retire (via `Remove`), and the development grants of `marsh --dev` sessions: per-grant
  child maps and intents over a random per-grant name prefix, intent-then-adopt child
  create, revocation on session end and on every daemon restart, and host cleanup.
- `Worker.tla`: one owned warm kit VM with its single worker transport (generation),
  up to `CAP` concurrent attempts, quarantine, retire, cancellation, and project/home
  mounts whose sources the environment may replace at any time. A mount is made on
  first reference and retained while the VM is warm (mounted whenever referenced); an
  idle mount is reused, unmounted only to replace a stale source (`Worker_evictInUse`
  shows evicting a referenced one is caught), and released at Retire.
  The same module also specifies each warm shell VM: its one retained root supervisor
  transport (`sbx exec -i -u root VM marsh --internal-supervisor`) is the worker
  transport, and each shell session (and its relay child) is an attempt keyed to that
  transport generation. Start, exit, and verified cgroup cleanup are in-band frames;
  transport loss makes started sessions uncertain and quarantines the VM; a dead idle
  supervisor is replaced only after a failed liveness ping with no live attempt.
- `Grants.tla`: an ACP session published as an MCP tool and an MCP pipeline loaded into a
  named sandbox. Both use the same model: publish binds one target incarnation, revoke is
  terminal, and requests and one-time permissions are checked when delivered. The same
  module specifies a default (untargeted) MCP publication: the daemon's record, one Kit VM
  that is created and removed, shell and host-terminal revocation, and loading only at VM
  create after re-checking the declaration.
- `Split.tla` (the retired `split-join.md` design; implemented by patches `0033`/`0034`, `split_workspace.rs`,
  and `marsh-daemon/src/split_confinement.rs`): one `split { } | join | CMD...` over
  private workspaces (Git worktrees or copies): one snapshot, workspace allocate/abort,
  branch run/exit/capture, Ctrl-C with confirmed reap or unconfirmed (`unreaped`) grace
  expiry, the downstream stages ("join"), remove or retain-and-report after the last stage
  exits, shell crash, and the next split's non-destructive scan. The user edits the project
  at any time; branch shell processes write only below their cwd (assumption C1), while a
  branch's Kit job is confined by its mounts (one representative branch starts one).
  Superseded by `Workspace.tla`; the shell-side split is deleted.
- `Workspace.tla` (`../design/workspaces.md`; built in `marsh-daemon/src/split.rs`): daemon-owned workspaces and split
  lineage. The daemon journals each split, snapshots the caller's tree (the user's tree for a
  session, the creator branch's fork for a nested split), clones sibling forks, and runs every
  branch: trusted shell branches write their cwd (C1); Kit (argv) branches write anything they
  have mounted, including a nested `.git` in the fork. Nested splits are argv-only. Launch draws
  from one session pool; a slot frees only on a confirmed end. Branch exit cancels its subtree;
  capture waits for every writer to be gone and rejects a fork whose git admin files changed or
  that holds a nested `.git`; only the creator joins/releases (awaiting or kept); lease expiry
  keeps the split joinable; one daemon crash between any two steps makes recorded unfinished
  splits uncertain, never replayed. Environment: user edits, adversarial Kit writes, join/cancel
  requests from any actor, leftover processes.
- `Process.tla` (`../design/processes.md`; built): one lineage forest per daemon. The
  session or any running job with a live capability launches a child (Run, or a split branch
  with a fork-only view) after admission against one session pool, a depth cap, a per-job
  fan-out cap, a per-tree total cap, and a per-tree Kit VM cap; the child's rights (view + spawnable Kits) are its
  parent's or narrower. A job's exit, cancel (session or parent), or transport loss revokes its
  capability and cancels every descendant; a slot frees only on confirmed deletion; a daemon
  restart revokes every capability and makes unfinished jobs uncertain, never resubmitted.
  Every job's first step is its entrypoint invoking its own command (script through the PATH
  link, or ELF by absolute path); the link resolves the own name to the image binary only with
  the worker's entry marker and parent = docker-init, so every later own-name call (an agent's
  Bash tool calling itself) is a same-Kit child under a chain cap. Any job may spawn any Kit in
  its spawn set (every registered Kit by default; the child runs under its own Kit's egress and
  credentials); launchers only narrow the set.
  Environment: jobs exit at will, the user cancels any job, transport loss, leftover containers.

Invariants: `NoForeignEffect`, `OwnedAreOurs`, `NoLeak`, `ChildNoLeak`, `ChildConfinement`
(a grant only affects VMs it made), `RevokedChildInert` (no grant effect after revoke or
without its session), `ChildCapacity`, `MapsDisjoint` (Ownership); `CapacityBound`, `NoReplay`,
`NoStartOnQuarantined`, `LossIsUncertain`, `MountRefcount`, `NoStaleSourceUse`,
`CancelScoped` (Worker); `RevokedGrantInert`, `GrantTargetBound`, `OneTimePermission`,
`DefaultSparesRunning`, `DefaultRevokedInert`, `DefaultReachesNewVms` (Grants); `NoWriteToUserTreeBeforeJoin`, `OneBase`, `JoinSeesAllBranches`, `NoRemoveWhileLive`,
`CancelReapsAll`, `CleanupOrRetain`, `BranchJobConfined` (Split); `SnapshotImmutable`, `AdminReadOnly`,
`ExposedClean`, `OneBase`, `ChildFromParent`, `ForkIsolation`, `NoWriteToUserTree`,
`CaptureStable`, `LineageWF`, `BudgetBound`, `LiveUnderLive`, `CancelPropagates`,
`CancelScoped`, `NoRemoveWhileLive`, `NoReplay`, `WorkspaceRecorded` (Workspace); `LineageWF`,
`Attenuation`, `BudgetBound`, `FanOutBound`, `TotalBound`, `LiveUnderLive`, `NoOrphanRunning`,
`CancelPropagates`, `RevokedAtRestart`, `RestartUncertain`, `NoReplay`, `EntryOnce`,
`NoEntryRecursion`, `SameKitBound`, `SpawnSetAttenuates`, `KitVMBound` (Process). Liveness: `ChildCleanedAfterRevoke` (strong
fairness on host cleanup, which is retried after every restart and inventory),
`AttemptsTerminate` (weak fairness), `EventuallySettled` (Split; weak fairness on process
progress and on the next split's scan), and `EventuallySettled` (Workspace; weak fairness on
daemon steps and process progress, checked in the smaller `Workspace_live_pass.cfg`).

Each module has a `Bug` constant. `Module.cfg` uses `Bug = "none"` and checks every
property. Each `Module_<bug>.cfg` turns on one guarded wrong action and checks only the
property that must catch it. Grant controls: `childTouchHost`, `crossGrant` (broker
consults another grant's map), `childIgnoreRevoke`, `childOverMax`, `childNoIntent`
(grant intent kept only in memory), and `restartKeepsGrant` (a persisted grant survives
the restart that lost its session). Default-publication controls: `loadRunning` (load
into the running Kit VM at the next admission: DefaultSparesRunning), `trustRecord` (load
from the record without re-reading the declaration, so a host-terminal unpublish is
resurrected: DefaultRevokedInert), and `skipCreateLoad` (DefaultReachesNewVms). Split controls: `fallbackInPlace` (a branch runs in the
project when its worktree fails), `perBranchSnapshot`, `joinEarly`, `removeUnreaped`,
`cancelOrphan` (fanout-style drop treated as reaped), `retainSilently`, `noScan`, and
`kitWholeProject` (a branch's Kit job mounts the whole project and writes the user's tree
through an absolute path). Workspace controls each remove one guard of a real action: `objectsRW` (store.git mounted
rw: SnapshotImmutable), `adminRW` (git admin files mounted rw: AdminReadOnly), `noCaptureCheck`
(capture skips admin/nested-`.git` verification: ExposedClean), `liveFork` (OneBase),
`childFromUser` (ChildFromParent), `nestedFork` (ForkIsolation), `wholeProject` and
`ancestorJoin` (an ancestor or the session joins a nested split and writes its own tree:
NoWriteToUserTree), `captureEarly` (CaptureStable), `noDepth` (LineageWF), `freeOnRevoke`
(pool slot freed at revoke: BudgetBound), `orphanChildren` (LiveUnderLive), `noPropagate`
(CancelPropagates), `unscopedCancel` (CancelScoped), `removeLive` (NoRemoveWhileLive),
`replay` (NoReplay), `noIntent` (WorkspaceRecorded), `noLease` and `forgetUncertain`
(EventuallySettled). Positive Workspace configs split the state space: `Workspace.cfg` (2 splits x 2 labels,
mixed shell/Kit, nesting at depth 2, no restart), `Workspace_depth3_pass.cfg` (3 splits, one
shell label at the root, argv nested, depth 3, one restart), `Workspace_restart_pass.cfg` (one
split, two mixed siblings, one restart), and `Workspace_live_pass.cfg` (liveness).
Process controls, one per invariant, each in a real action: `noDepth` (LineageWF), `widen`
(child rights recomputed from the session's grants: Attenuation), `freeOnRevoke` (BudgetBound),
`noFan` (FanOutBound), `noTotal` (TotalBound), `orphanChildren` (LiveUnderLive), `revokeOnly`
(descendant capabilities revoked but containers not signalled: NoOrphanRunning), `noPropagate`
(CancelPropagates), `restartKeepsCap` (RevokedAtRestart), `keepRunning` (RestartUncertain),
`replay` (NoReplay), `linkIgnoresMarker` (the link dispatches a script entrypoint's own name as a
child: NoEntryRecursion), `linkIgnoresPpid` (the link resolves the own name locally whenever the
marker is set, which an ELF entrypoint leaves in place: EntryOnce), `noSameKit` (SameKitBound),
`spawnWiden` (child spawn set reset to every registered Kit: SpawnSetAttenuates), `noKitCap` (a
child brings one more distinct Kit VM into a full tree: KitVMBound). Wall time is not modeled:
a deadline is the environment's `Cancel(j, Session)`, and a child's inherited deadline is an
admission-time value checked by `process_tests.rs` and P35, not by TLC. Positive Process configs:
`Process.cfg` (one script Kit, 3 jobs, depth 3, one restart, every invariant; ~40 s),
`Process_kits_pass.cfg` (a script Kit and an ELF Kit, spawn-set narrowing, same-Kit cap 2,
no restart; ~1.5 min with 2 workers),
`Process_caps_pass.cfg` (4 jobs, tight pool/fan/total caps, no restart; ~1.5 min),
`Process_kitvms_pass.cfg` (two ELF Kits, one Kit VM per tree, no restart; ~1.5 min),
`Process_live_pass.cfg` (`EventuallySettled`).
A config named
`Module_<name>_pass.cfg` is positive.

Assumptions (outside the model, stated not checked):
- A1: host `sbx` is trusted, and every host VM name is a fixed prefix plus a random
  8-character base36 id (`marsh-k-<id>`, `marsh-s-<id>`) chosen by this daemon and kept in
  its ownership map, the only source of VM names. No other actor creates a VM under such a
  name, so host stock operations may be name-addressed and an intent may be adopted by name.
  A1 also covers grant names: each grant's prefix (`marsh-x<5 base36>-`) is random and a
  child reaches stock only through the broker, which still requires the name in the grant
  with its recorded UUID in the cached inventory before any op.
- A2: stock create is atomic on name uniqueness.
- Create persists the intent (random name, purpose, kit identity) before `sbx create`;
  the next inventory adopts a present intent and drops an absent one loaded at startup.
  `Ownership_noIntent` shows that an in-memory intent leaks a created VM across a restart.
- A3: `sbx ls` is accurate when it is observed. It can be stale immediately afterward.
- A4: if source identity matches before and after `sbx mount`, the mount used that identity.
  The pathname swap inside the stock mount call remains an upstream gap. The dev broker
  applies the same pre/post check to forwarded mounts and on mismatch unmounts and revokes
  the grant (operator decision: accepted window, host `sbx` trusted, same user); this
  mount-source step is not modeled.
- Source overlap with other sources or the control root is a static path check before
  admission. It is not modeled.
- An in-flight forwarded call is atomic at its effect. Revocation fences and kills
  in-flight calls; a create that lands in that window is still found through its persisted
  intent.
- A5 (nesting): an inner daemon's grants use prefixes `${prefix}x<5>-` inside the outer
  grant's prefix and roots under the outer scratch, so the host spec bounds them as part of
  the outer grant (name pattern caps depth at 3). The inner daemon is not checked.
- Host names behave the same and are independent, so configs use one host name. Bounds
  are small: 3 UUIDs, 3 attempts with CAP=2, 2 transport generations, and 2 source identities.

Run: `docs/model/check.sh [--quick | --huge | --no-huge] [--cores N] [--workers N] [CONFIG...]`. Configs run
concurrently inside a core budget (default: the runner's CPUs, at most 8), one TLC process each with its own
log; the verdict of a config does not depend on scheduling. `--quick` skips the configs marked `slow`
(`\* marsh-check: slow` on the first line of the `.cfg`; about 30 s to 7 min with two workers, and
`Workspace_depth3_pass` about 30 min) and is the static gate (`make verify-static`); the default and
`make regress` run everything. `--huge` / `--no-huge` split the one config that does not fit a CI job with
its siblings from the rest. `TLC_CACHE=DIR` skips a config whose spec, config, jar and `check.sh` are
byte-identical to one that already gave its expected verdict; the release run does not set it. This needs
Docker (or `TLC_RUNNER=java` with a local Java 17+) and the
TLA+ tools jar at `/private/tmp/marsh-tla2tools-1.8.0.jar` (override with
`TLA2TOOLS_JAR`). The script exits nonzero if a positive config fails or a
negative control passes or fails for the wrong reason. The jar is not committed.

| Spec action | Intended code location |
|---|---|
| Inventory, Use, StartVM, Remove | marsh-sbx stock adapter (inventory cache, UUID-conditional ops) - intended |
| CreateBegin / CreateDo / Record / Restart | marsh-sbx `vm_ownership.rs` (`assign`, `store_view` adopt/drop) |
| Remove (incl. operator Retire) | marsh-sbx `retire_kit_vm_before` via `marsh workers reset KIT` (a quarantined VM's references, and pins of shells no longer attached, end with the VM) and via `marsh stop`/`reset` `teardown_scope` (lifecycle admission proves no attached shell or running job, so every retained reference ends with the VM) |
| ChildCreateBegin/Do/Record, ChildOp, ChildRemove | marsh-daemon `dev_broker.rs` (`serve`, `serve_admitted` incl. `Action::Run`: a fresh `run` is a ChildCreate, a reattach a ChildOp; `check_owned`) + `dev_broker/policy.rs`; grant section in marsh-sbx `vm_ownership.rs` (`update_dev_grants`, adoption in `store_view`) |
| RevokeChild, Restart (revoke all), HostAdoptChild, HostCleanChild | marsh-daemon `dev_broker.rs` (`revoke_session` at session end in marsh-backend `open_shell`, `revoke_all` at `marsh stop`/`marsh reset` and in `marshd` startup, `remove_children`) |
| Admit / Run / Exit / Cleanup / Cancel | marsh-daemon admission + marsh-worker control - intended |
| TransportLoss / ReplaceTransport / Retire | marsh-worker transport + marsh-daemon worker registry - intended |
| Shell: Admit / Run / Exit / Cleanup | marsh-sbx `shell_supervisor.rs` (`Supervisor::spawn`, `Control::cleanup_session`); guest `marsh/src/shell_supervisor.rs` |
| Shell: TransportLoss / ReplaceTransport | marsh-sbx `shell_supervisor.rs` (`lose`, `ping`); `ensure_shell_lease` (replace idle, quarantine with live attempts) |
| Admit (mount ref) / MountFinish | marsh-sbx host_grants / shell_mount_observation - intended |
| Publish / Revoke / Deliver / Offer / Choose | marsh-daemon acp_bridge, mcp_publication; marsh-acp, marsh-mcp - intended |
| DPublish / DRecord / DRevokeDaemon / DRevokeHost (default MCP publication) | marsh-daemon `lib.rs` `McpPublish` (record after commit: `McpHostControl::record_default`; targeted republish: `drop_default`) and `McpUnpublish` (`drop_default` before revocation); host-terminal unpublish `marsh/src/main.rs` `unpublish_mcp`; record file `marsh-daemon/src/mcp_defaults.rs` |
| VmCreate (default load; no LazyLoad step exists) | marsh-backend `prepare_one_authorized` (only when `ReadyKitVm::cold_started` and not ephemeral, before `ready_kits` insert) -> `load_default_mcp_publications` -> `mcp_defaults::loadable` (re-reads declaration generation, revoke marker; prunes) -> marsh-sbx `StockSbx::load_mcp_server` |
| Workspace: Create / Alloc / Refuse | `marsh-daemon/src/split.rs` `SplitEngine::create` -> `plan` (labels, 16-branch cap, depth 3, registered argv, creator-only nesting, argv branches only for a nested creator *and for any job creator* (a top-level job on its capability socket gets no shell branch: the model's `Kit(b)` for a branch-created split, which the code applies to every `Creator::Job`); Refuse = status 2 before effects) -> `setup` (journal record before branches; all I/O by descriptor through `split_fs::Dir` (`O_NOFOLLOW`, `O_EXCL`, `clonefileat` with `CLONE_NOFOLLOW`); `split_fs::snapshot` clones the caller's tree and prunes ignored entries and nested `.git`, one `clonefileat` of `base` per fork, `write_admin`) |
| Workspace: Launch / Start | `split.rs` `branch` -> `admit`: pool admission (8 per session, one pool for all host CLI splits, uncertain slots held until `workers reset` via `release_uncertain_slots`; `failed: capacity`), shell branch = fresh session + `OpenShell` through the daemon endpoint, argv branch = `Execute` with cwd in the fork; `drive` records the job id |
| Workspace: Capture / Await / Consume / LeaseExpire | `split.rs` `branch` (after confirmed exit and `settle_jobs` for a shell branch's Kit jobs: `split_fs::verify` admin digests, admin entry allowlist, nested `.git`, then `capture` -> `out/`; transport loss -> `uncertain`, never removed), `create` (status, manifest, `await`), `join` (creator check, `SplitRelease` -> `remove_files` or `kept`), `expire_leases` (60 s unheld -> `kept`) |
| Workspace: Write (AdminReadOnly, NoWriteToUserTree, SnapshotImmutable) | `marsh-daemon/src/split_confinement.rs` `branch_confinement`; `marsh-backend` `confine_job_mounts` / `public_mounts`; `marsh-runtime` `resolve_grant_subpath` (file leaves) |
| Workspace: Exit (cascade) | `split.rs` `branch` -> `cancel_children_of`; capability socket: `marsh-worker/src/capability.rs`, `marsh-backend` `CapabilityBridges` (token revoked at job end), `marsh-daemon` `AuthenticationScope::Capability` (Create/Join only) |
| Workspace: Cancel / Reap / GraceExpire / CancelEnd | `split.rs` `cancel` (stream drop, client signal frame (CLI and Brush sugar Ctrl-C), `SplitCancel`, the 1 h deadline; recursive over children; `INT`, 10 s, `KILL`), in `create` the whole split directory is removed once every branch end is confirmed, else all of it is kept with the unconfirmed labels as the reason (stricter than `CancelEnd`, which removes reaped siblings) |
| Workspace: Restart / Recover | `split.rs` `SplitEngine::open` (journal load: `run` -> `uncertain`, launched branches `untrusted`, forks retained; capabilities are in-memory and lost) |
| Process: Spawn (admission, journal parent/root/depth/spawn before submit) | `marsh-daemon/src/process.rs` `admit` (pool 8 per daemon incl. held uncertain slots, depth 4, fan-out 4, total 64, spawn set, same-Kit chain 2), called by `DaemonStore::admit_process` before VM work and again under the store lock in `begin_job_process` (`lib.rs`), which journals `lineage {parent, root, depth, spawn}` before `mark_execution_submitted`; refusals count in `processes.refused` and leave no receipt. `ProcessRun` enters in the `AuthenticationScope::Capability` branch of `lib.rs` `Server::handle` -> `process::serve_run` -> this daemon's own `Execute` (`marsh-backend` `execute`) |
| Process: Spawn rights (Attenuation, SpawnSetAttenuates) | `marsh-backend` `execute`: a child's view is its parent's recorded mounts (`process::child_view`; cwd outside it is refused), bound from the covering grants by `view_job_mounts` (split argv branches, `ProcessLink.branch`, are the only narrowing besides a late `.marsh` read-only); spawn set = parent's ∩ requested (`marsh-contracts/src/process.rs` `narrow_spawn`); relays cannot name a parent (`authorize_request_session` clears `parent_job`) |
| Process: Exit / Cancel / Lose cascade, Reap / GraceExpire | a job's end drops its `CapabilityBridges` (token revoked, connections closed) and the worker's `capability.rs` `Capability` (socket and streams); `process::serve_run` sees the caller stream drop and calls `DaemonStore::cancel_tree` on the child (`process.rs`), which in one step marks the child and every live descendant `cancelling` (admission then refuses their spawns, `child_lineage`), sends each container INT through the `JobControl` the backend registered at launch, and KILLs whatever still runs after 10 s (the one-step `CancelPropagates` cascade; previously one grace per level). A root caller's Ctrl-C (signal frame) or disconnect does the same from `marsh-backend` `forward_multiplexed_input`. A cancelling job ends `cancelled` (`finish_job_as`), exit 130; the backend verifies container deletion; an unconfirmed end holds the slot (`process.rs` `holds_slot`, derived from `cleanup: uncertain` receipts, so it survives restart) until a successful `workers reset` (`release_process_slots`, persisted) |
| Process: Restart | daemon start: capability tokens are in memory (lost); journal load marks unfinished jobs uncertain; no resubmission; the root client reports `job uncertain` (`marsh/src/client.rs`) |
| Process: EntryLocal / EntryDispatch / AgentSelfLocal (link decision) | worker/runtime set `MARSH_ENTRY=1`, `MARSH_JOB`, and `PATH=/run/marsh/bin:<image PATH>` for every job (no `bash`/`sh` link and no `SHELL` override: shells are the image's own, `processes.md` s15) (`marsh-runtime` `job_environment`); stubs `#!/run/marsh/marsh --link=NAME` (`marsh-worker/src/capability.rs` `publish`); `marsh/src/job.rs` `link` -> `marsh-contracts` `process::link_decision` (own name + marker + `getppid()` = 1 -> clear marker, exec the image binary by absolute path with PATH intact; another image-provided name -> exec locally; else `ProcessRun`) |
| Process: Spawn Kit VM cap (KitVMBound) | `process::kit_vm_cap` (from `child_lineage`, split branches included): distinct `kit_ref` among the tree's live receipts, at most `JobDefaults::tree_kit_vms` (4; `MARSH_TREE_KIT_VMS`), checked in the pre-check and in `begin_job_process` |
| Process: Spawn spawn-set narrowing, same-Kit cap | `process::admit` (`child_lineage`): same-Kit = consecutive parent→child steps with equal `kit_ref`; `MARSH_SPAWN` / `marsh run --spawn` / `--no-spawn` (no `cap.sock`, links refuse) |
