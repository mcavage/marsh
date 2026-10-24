# Cross-component model

Seven TLA+ modules specify current cross-component state (six local modules and
the cloud ownership extension). An eighth, `Placement`,
specifies the cloud placement extension; only its single-crossing,
session-origin part is implemented (see
[coverage and gaps](#proposed-placement-model)). TLC checks their bounded state
spaces; a pass does not prove that the Rust code refines these specifications.
The code-location table below records the implementation mapping and known gaps.

- `CloudOwnership.tla`: the owned cloud adapter's persisted intents, delayed
  create visibility/replies, cleanup-only restart recovery, immutable-ID effects,
  quarantine, resource capacity and asynchronous deletion. See
  [invariants, controls and implementation mapping](#cloud-ownership-extension).

- `Sbxenv.tla`: snapshotted native MCP/extra-mount additions, required MCP load
  before VM readiness, registration reuse without overwrite, inherited extra
  mounts without child-Kit widening, and extra-mount omission from split jobs.
  Its four negative controls are `childKitMount` (ExtraAttenuation),
  `splitExtraMount` (SplitOmitsExtras), `skipMcpLoad` (McpBeforeReady), and
  `overwriteRegistration` (NoRegistrationOverwrite). Configuration/file/path
  parsing, alias equivalence and content-sensitive VM selectors are static code
  checks, not modeled. It abstracts one declared server per Kit; source identity,
  retained mounts and cleanup remain in Worker/Ownership, and fork paths in Workspace.
  Registration inspection/addition are abstracted as one step: concurrent changes
  by other host-global SBX clients are trusted-host behavior, not an atomic
  compare-and-set promise of marsh's separate public CLI calls.
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
`DefaultSparesRunning`, `DefaultRevokedInert`, `DefaultReachesNewVms` (Grants); `SnapshotImmutable`, `AdminReadOnly`,
`ExposedClean`, `OneBase`, `ChildFromParent`, `ForkIsolation`, `NoWriteToUserTree`,
`CaptureStable`, `LineageWF`, `BudgetBound`, `LiveUnderLive`, `CancelPropagates`,
`CancelScoped`, `NoRemoveWhileLive`, `NoReplay`, `WorkspaceRecorded` (Workspace); `LineageWF`,
`Attenuation`, `BudgetBound`, `FanOutBound`, `TotalBound`, `LiveUnderLive`, `NoOrphanRunning`,
`CancelPropagates`, `RevokedAtRestart`, `RestartUncertain`, `NoReplay`, `EntryOnce`,
`NoEntryRecursion`, `SameKitBound`, `SpawnSetAttenuates`, `KitVMBound` (Process). Liveness: `ChildCleanedAfterRevoke` (strong
fairness on host cleanup, which is retried after every restart and inventory),
`AttemptsTerminate` (weak fairness), and `EventuallySettled` (Workspace; weak fairness on
daemon steps and process progress, checked in the smaller `Workspace_live_pass.cfg`).

Each module has a `Bug` constant. `Module.cfg` uses `Bug = "none"` and checks every
property. Each `Module_<bug>.cfg` turns on one guarded wrong action and checks only the
property that must catch it. Grant controls: `childTouchHost`, `crossGrant` (broker
consults another grant's map), `childIgnoreRevoke`, `childOverMax`, `childNoIntent`
(grant intent kept only in memory), and `restartKeepsGrant` (a persisted grant survives
the restart that lost its session). Default-publication controls: `loadRunning` (load
into the running Kit VM at the next admission: DefaultSparesRunning), `trustRecord` (load
from the record without re-reading the declaration, so a host-terminal unpublish is
resurrected: DefaultRevokedInert), and `skipCreateLoad` (DefaultReachesNewVms). Workspace controls each remove one guard of a real action: `objectsRW` (store.git mounted
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
log; the verdict of a config does not depend on scheduling. `--quick` (run by `make test`, about 90 s on 8
cores) checks every negative control and, for each module, a positive config with every invariant
(`Ownership.cfg`, `Worker.cfg`, `Grants.cfg`, `Process.cfg`, `Workspace_restart_pass.cfg`) plus the liveness
configs (`Process_live_pass.cfg`, `Workspace_live_pass.cfg`). It skips only the configs marked
`\* marsh-check: slow`, the larger positive bounds: `Workspace.cfg` (about 7 min), `Process_caps_pass`,
`Process_kits_pass`, `Process_kitvms_pass` (about 2 min each), and `Workspace_depth3_pass` (about 30 min).
The default, CI's model job, and `make regress` run every config. `--huge` / `--no-huge` split the one config
that does not fit a CI job with its siblings from the rest. `TLC_CACHE=DIR` skips a config whose spec, config,
jar and `check.sh` are byte-identical to one that already gave its expected verdict (`make test` uses
`target/tlc-cache`); the release run does not set it. This needs Docker (or `TLC_RUNNER=java` with a local
Java 17+) and the pinned TLA+ tools jar, which `make test-model` downloads and checks (the build is
mirrored as a release of this repository because upstream replaces its v1.8.0 asset in place; see the
Makefile). Run directly, `check.sh` takes the jar from `TLA2TOOLS_JAR`. The script exits nonzero if a
positive config fails or a negative control passes or fails for the wrong reason. The jar is not committed.

| Spec action | Code location |
|---|---|
| Sbxenv: Create / Register / Load / Ready / Remove | `marsh-backend/src/sbxenv.rs` `with_sbxenvs` (snapshot), `lib.rs` `kit_spec` (environment selector), `prepare_one_authorized` (required setup before ready cache), `configure_kit_mcp`; `marsh-sbx/src/mcp_environment.rs` `ensure_mcp_server` (inspect/reuse or add, no overwrite), `load_mcp_server` / `load_shell_mcp_server` (owned UUID); worker reset and scope teardown |
| Sbxenv: Launch (ExtraAttenuation, SplitOmitsExtras) | `marsh-backend` `execute` selects a root's environment, an ordinary child's inherited receipt view, or no extras for a split; `sbxenv.rs` `additional_grants` applies existing descriptor/overlap admission; `inherited_environment` maps only parent-recorded additions. Rust callers: `sbxenv::tests`, `marsh-sbx/tests/mcp_environment.rs`; real caller: `tests/acceptance/sbxenvs_uat.py` |
| Inventory, Use, StartVM, Remove | marsh-sbx `lib.rs` `StockSbx::stock_view` (cached `sbx ls --json`, refreshed on demand; `stock_inventory.rs` decodes it) and `classify_vm` -> `vm_ownership.rs` `classify` (owned only with its recorded UUID present, else foreign or absent); `ensure_kit_vm_authorized` / `ensure_shell_vm_inner` (StartVM); `remove_owned_vm` (fresh classification, `sbx rm`, then proves the recorded UUID is gone) |
| CreateBegin / CreateDo / Record / Restart | marsh-sbx `vm_ownership.rs` (`assign`, `store_view` adopt/drop) |
| Remove (incl. operator Retire) | marsh-sbx `retire_kit_vm_before` via `marsh workers reset KIT` (a quarantined VM's references, and pins of shells no longer attached, end with the VM) and via `marsh stop`/`reset` `teardown_scope` (lifecycle admission proves no attached shell or running job, so every retained reference ends with the VM); `shell_recovery_specs` includes persisted `owned_shell_names` across personal-environment generations, used only for UUID-checked removal |
| ChildCreateBegin/Do/Record, ChildOp, ChildRemove | marsh-daemon `dev_broker.rs` (`serve`, `serve_admitted` incl. `Action::Run`: a fresh `run` is a ChildCreate, a reattach a ChildOp; `check_owned`) + `dev_broker/policy.rs`; grant section in marsh-sbx `vm_ownership.rs` (`update_dev_grants`, adoption in `store_view`) |
| RevokeChild, Restart (revoke all), HostAdoptChild, HostCleanChild | marsh-daemon `dev_broker.rs` (`revoke_session` at session end in marsh-backend `open_shell`, `revoke_all` at `marsh stop`/`marsh reset` and in `marshd` startup, `remove_children`) |
| Admit / Run / Exit / Cleanup / Cancel | marsh-sbx `StockSbx::launch_worker` (refuses a quarantined VM or a stale worker generation; one `AttemptRoute` per attempt on the VM's retained worker transport); marsh-worker `control.rs` (per-attempt start, exit and cleanup frames) and `transport.rs` (`cancellation`). Not implemented as modeled: a per-VM `CAP`; concurrency is bounded by the daemon's session pool (`marsh-daemon/src/process.rs` `admit`) |
| TransportLoss / ReplaceTransport / Retire | marsh-sbx `ensure_worker_lease` (a dead or unresponsive transport, `ping_worker`, with live attempts quarantines the VM in `quarantined_workers`; an idle one is replaced under a new worker generation), `WorkerLoss` (the loss diagnostic), `stop_quarantined`, `retire_kit_vm_before` (`marsh workers reset KIT`) |
| Shell: Admit / Run / Exit / Cleanup | marsh-sbx `shell_supervisor.rs` (`Supervisor::spawn`, `Control::cleanup_session`); guest `marsh/src/shell_supervisor.rs` |
| Shell: TransportLoss / ReplaceTransport | marsh-sbx `shell_supervisor.rs` (`lose`, `ping`); `ensure_shell_lease` (replace idle, quarantine with live attempts) |
| Admit (mount ref) / MountFinish | marsh-sbx `prepare_job_grants` -> `acquire_shared_grant` (mount on first reference, refcounted in `grant_mount_references`; a source whose identity changed is refused, `SourceChanged`) -> `mount_shared_grant`; `revoke_grants` / `release_session_grant` drop references; `evict_idle_grant_locked` unmounts only an idle mount of a replaced source; shell VMs: `acquire_shell_mount` / `evict_idle_shell_mount` |
| Publish / Revoke / Deliver / Offer / Choose | marsh-daemon `mcp_publication.rs` `McpHostControl` (`publish`, `publish_acp`, `load`, `unpublish`, `unpublish_acp`, `mark_revocation_pending`); marsh-mcp `acp_export.rs` (every published prompt, status, cancel and permission response carries the declaration `generation`); marsh-daemon `acp_session.rs` `AcpPermissionInbox` (only an offered `allow_once` / `reject_once` option is accepted, and the request leaves `pending` when chosen) |
| DPublish / DRecord / DRevokeDaemon / DRevokeHost (default MCP publication) | marsh-daemon `lib.rs` `McpPublish` (record after commit: `McpHostControl::record_default`; targeted republish: `drop_default`) and `McpUnpublish` (`drop_default` before revocation); host-terminal unpublish `marsh/src/main.rs` `unpublish_mcp`; record file `marsh-daemon/src/mcp_defaults.rs` |
| VmCreate (default load; no LazyLoad step exists) | marsh-backend `prepare_one_authorized` (only when `ReadyKitVm::cold_started` and not ephemeral, before `ready_kits` insert) -> `load_default_mcp_publications` -> `mcp_defaults::loadable` (re-reads declaration generation, revoke marker; prunes) -> marsh-sbx `StockSbx::load_mcp_server` |
| Workspace: Create / Alloc / Refuse | `marsh-daemon/src/split.rs` `SplitEngine::create` -> `plan` (labels, 16-branch cap, depth 3, registered argv, creator-only nesting, argv branches only for a nested creator *and for any job creator* (a top-level job on its capability socket gets no shell branch: the model's `Kit(b)` for a branch-created split, which the code applies to every `Creator::Job`); Refuse = status 2 before effects) -> `setup` (journal record before branches; all I/O by descriptor through `split_fs::Dir` (`O_NOFOLLOW`, `O_EXCL`, `clonefileat` with `CLONE_NOFOLLOW`); `split_fs::snapshot` clones the caller's tree and prunes ignored entries and nested `.git`, one `clonefileat` of `base` per fork, `write_admin`) |
| Workspace: Launch / Start | `split.rs` `branch` -> `admit`: pool admission (16 per session, one pool for all host CLI splits, uncertain slots held until `workers reset` via `release_uncertain_slots`; `failed: capacity`), shell branch = fresh session + `OpenShell` through the daemon endpoint, argv branch = `Execute` with cwd in the fork; `drive` records the job id |
| Workspace: Capture / Await / Consume / LeaseExpire | `split.rs` `branch` (after confirmed exit and `settle_jobs` for a shell branch's Kit jobs: `split_fs::verify` admin digests, admin entry allowlist, nested `.git`, then `capture` -> `out/`; transport loss -> `uncertain`, never removed), `create` (status, manifest, `await`), `join` (creator check, `SplitRelease` -> `remove_files` or `kept`), `expire_leases` (60 s unheld -> `kept`) |
| Workspace: Write (AdminReadOnly, NoWriteToUserTree, SnapshotImmutable) | `marsh-daemon/src/split_confinement.rs` `branch_confinement`; `marsh-backend` `confine_job_mounts` / `public_mounts`; `marsh-runtime` `resolve_grant_subpath` (file leaves) |
| Workspace: Exit (cascade) | `split.rs` `branch` -> `cancel_children_of`; capability socket: `marsh-worker/src/capability.rs`, `marsh-backend` `CapabilityBridges` (token revoked at job end), `marsh-daemon` `AuthenticationScope::Capability` (job-owned Create/Join, child `ProcessRun`, and subtree job inspection) |
| Workspace: Cancel / Reap / GraceExpire / CancelEnd | `split.rs` `cancel` (stream drop, client signal frame (CLI and Brush sugar Ctrl-C), `SplitCancel`, the 1 h deadline; recursive over children; `INT`, 10 s, `KILL`), in `create` the whole split directory is removed once every branch end is confirmed, else all of it is kept with the unconfirmed labels as the reason (stricter than `CancelEnd`, which removes reaped siblings) |
| Workspace: Restart / Recover | `split.rs` `SplitEngine::open` (journal load: `run` -> `uncertain`, launched branches `untrusted`, forks retained; capabilities are in-memory and lost) |
| Process: Spawn (admission, journal parent/root/depth/spawn before submit) | `marsh-daemon/src/process.rs` `admit` (pool 16 per daemon incl. held uncertain slots, depth 4, fan-out 8, total 128, spawn set, same-Kit chain 2), called by `DaemonStore::admit_process` before VM work and again under the store lock in `begin_job_process` (`lib.rs`), which journals `lineage {parent, root, depth, spawn}` before `mark_execution_submitted`; refusals count in `processes.refused` and leave no receipt. `ProcessRun` enters in the `AuthenticationScope::Capability` branch of `lib.rs` `Server::handle` -> `process::serve_run` -> this daemon's own `Execute` (`marsh-backend` `execute`) |
| Process: Spawn rights (Attenuation, SpawnSetAttenuates) | `marsh-backend` `execute`: a child's view is its parent's recorded mounts (`process::child_view`; cwd outside it is refused), bound from the covering grants by `view_job_mounts` (split argv branches, `ProcessLink.branch`, are the only narrowing besides a late `.marsh` read-only); spawn set = parent's ∩ requested (`marsh-contracts/src/process.rs` `narrow_spawn`); relays cannot name a parent (`authorize_request_session` clears `parent_job`) |
| Process: Exit / Cancel / Lose cascade, Reap / GraceExpire | a job's end drops its `CapabilityBridges` (token revoked, connections closed) and the worker's `capability.rs` `Capability` (socket and streams); `process::serve_run` sees the caller stream drop and calls `DaemonStore::cancel_tree` on the child (`process.rs`), which in one step marks the child and every live descendant `cancelling` (admission then refuses their spawns, `child_lineage`), sends each container INT through the `JobControl` the backend registered at launch, and KILLs whatever still runs after 10 s (the one-step `CancelPropagates` cascade; previously one grace per level). A root caller's Ctrl-C (signal frame) or disconnect does the same from `marsh-backend` `forward_multiplexed_input`. A cancelling job ends `cancelled` (`finish_job_as`), exit 130; the backend verifies container deletion; an unconfirmed end holds the slot (`process.rs` `holds_slot`, derived from `cleanup: uncertain` receipts, so it survives restart) until a successful `workers reset` (`release_process_slots`, persisted) |
| Process: Restart | daemon start: capability tokens are in memory (lost); journal load marks unfinished jobs uncertain; no resubmission; the root client reports `job uncertain` (`marsh/src/client.rs`) |
| Process: EntryLocal / EntryDispatch / AgentSelfLocal (link decision) | worker/runtime set `MARSH_ENTRY=1`, `MARSH_JOB`, and `PATH=/run/marsh/bin:<image PATH>` for every job (no `bash`/`sh` link and no `SHELL` override: shells are the image's own, `processes.md` s15) (`marsh-runtime` `job_environment`); stubs `#!/run/marsh/marsh --link=NAME` (`marsh-worker/src/capability.rs` `publish`); `marsh/src/job.rs` `link` -> `marsh-contracts` `process::link_decision` (own name + marker + `getppid()` = 1 -> clear marker, exec the image binary by absolute path with PATH intact; another image-provided name -> exec locally; else `ProcessRun`) |
| Process: Spawn Kit VM cap (KitVMBound) | `process::kit_vm_cap` (from `child_lineage`, split branches included): distinct `kit_ref` among the tree's live receipts, at most `JobDefaults::tree_kit_vms` (4; `MARSH_TREE_KIT_VMS`), checked in the pre-check and in `begin_job_process` |
| Process: Spawn spawn-set narrowing, same-Kit cap | `process::admit` (`child_lineage`): same-Kit = consecutive parent→child steps with equal `kit_ref`; `MARSH_SPAWN` / `marsh run --spawn` / `--no-spawn` narrow admission. The capability socket remains available for subtree inspection; disallowed spawn requests are refused. |

## Proposed placement model

`Placement.tla` accompanies the [cloud placement design](../design/cloud-placement.md).
The [dispatch](../plan/cloud-dispatch.md) implements laptop -> cloud crossings
from the session and one nested shape, cloud -> cloud agent calls (a child
crossing from the block's current view, returned into that view). The table
maps those actions. Job-origin placement (a local Kit job's `marsh cloud`,
in its own Kit) is a laptop -> cloud crossing from the job's view, admitted
as that job's child (`process::child_lineage`, `cloud.rs` `execute_cloud`).
Cloud -> laptop crossings and remote workspace services remain
**unimplemented**. A TLC pass is not
evidence about the Rust code; the live acceptance run is.

A crossing has an immediate caller, a local/cloud execution location, a logical
workspace source/destination, immutable baseline, captured final contents, and
per-path import progress. Nested crossings use the parent's materialization;
a split-root crossing uses the fork, not the original checkout. All placements
draw from one bounded pool. Loss/restart revoke authority and retain uncertain
work; retirement can verify deletion but cannot replay execution or return.
Delete acknowledgement and actual disappearance are separate transitions.
Capturing a parent waits for its child returns, which hold workspace leases.

`CallerEdit` races with per-path `Apply`. Deltas intentionally overwrite changed
paths without a content comparison. Only changed paths participate, and import
is neither a merge nor an atomic multi-file transaction. The model permits
partial apply before restart and independent sibling returns. It does not
assert that overlapping caller edits survive. Absence is the abstract value -1.

| Invariant | Negative control | What must be caught |
|---|---|---|
| `ParentBound` | `ancestorReturn` | Return into the user tree instead of the owning fork/parent view |
| `SourceBound` | `hostSnapshot` | Snapshot the original checkout instead of the parent's materialization |
| `Attenuation` | `widen` | A location crossing restores broader view rights |
| `StableCapture` | `captureLive` | Capture with possible writers |
| `DeltaOnly` | `copySnapshot` | Return unchanged snapshot entries |
| `NoLateReturn` | `lateReturn` | Import after loss/restart revoked its generation |
| `NoReplay` | `replay` | Execute an uncertain job a second time |
| `VerifiedAbsence` | `ackIsAbsent` | Treat accepted deletion as observed absence |
| `ReturnBeforeComplete` | `earlyComplete` | Report completion with entries unapplied |
| `SharedBudget` | `noCap` | Provision beyond the shared local/cloud pool |
| `ParentCaptureSettled` | `pendingReturn` | Capture a parent view while any descendant return is pending, including before child capture |

Positive configs also check `TypeOK`, `NoEarlyRelease`, `LossIsUncertain`, and
`ReturnLease` (destination remains present/uncaptured for every pending child
phase, not only active import).
`Placement.cfg` covers two crossings, one file and racing edits, no restart;
`Placement_partial_pass.cfg` covers two files and restart during partial import;
`Placement_nested_pass.cfg` covers three crossings (including alternating
locations in a constrained chain), one file, no restart. The two-crossing config
covers sibling returns. Each bug removes a real action
guard or changes its effect; its config checks the named invariant. Normal
`check.sh` discovery includes these configs, including in `--quick`.

Assumptions/gaps, not checked platform guarantees:

- Each abstract crossing owns one sandbox; provisioning is atomic here.
  Account-bound IDs, delayed creates, name reuse, visibility, inventory failures,
  and orphan reconciliation require the `Ownership` extension. `DeleteLands`
  models a deletion that the environment may delay indefinitely.
- `Stop` abstracts verified termination and successful child settlement;
  `Lose` abstracts unconfirmed cancellation/loss. The INT/KILL protocol,
  bottom-up cancelled-result retention, and timeouts need integration with
  `Process`/`Workspace`, not a claim that `Stop` implements cancellation.
- Files/roots are abstract identities. Source inode pinning, safe manifests,
  destination filename aliases, protected Git paths, symlinks, read-only rights,
  and syscall races need code/real-caller checks. Rights here are root-view
  rights, not the full placement/spawn/network/credential policy.
- No stream credits, transfer-byte bounds, PTYs, deadline clock, remote split
  operations, or liveness/fairness claims are modeled. A valid safety run must
  not be presented as eventual settlement under network failure.
- Kit descriptor versions are outside this lifecycle model. Local marsh uses
  v3; live probes accepted cloud v2 on 2026-10-16 (v3 was not tested). A model pass
  is not evidence of artifact, bootstrap, policy, or credential compatibility.
- Per-path `Apply` is atomic in this model. Real replace plus journal ack is
  not atomic: restart must conservatively mark ambiguous apply uncertain and
  never repeat it automatically. Results already applied stay applied.

| Placement action | Code (session-origin slice) / remaining gap |
|---|---|
| `Stage` (binding + one shared slot) | `marsh-backend/src/cloud.rs` `execute_cloud` selects the immediate caller's view (split fork via `split_confinement`, else the project; `ParentBound`, `SourceBound`); `DaemonStore::begin_cloud_job` admits under the shared `POOL_LIMIT` (`SharedBudget`). A job-origin node (`execute_cloud` with a parent job) uses the parent's own Kit (`cloud_bindings`, identity equal to the parent's `kit_ref`) and the parent's read-write view, admitted as its child under the `cloud` spawn name, with the parent's remaining wall time as its TTL; its delta is applied only while the parent is live. A nested agent call (`AgentCalls::run_call`) is admitted as a child of the cloud job (depth, fan-out, total, spawn set) and snapshots the parent's current materialization (`archive`), not the laptop checkout (`SourceBound`). Cloud -> laptop: **not implemented** |
| `Start` / `Write` / `Stop` | `cloud_helper::run` (own process group); `Stop` = status file written only after `killpg` of the group and `kill_workload` (every process of the workload user), read back by the backend before capture. Cancellation: `Relay` + `JobControl` (INT, KILL after grace). Enforced for the session shell profile (root-owned control, separate `marsh-run` user). In a Kit sandbox (`Ctl { kit: true }`: no root, control steps as the Kit's user, the workload under no-new-privileges) the workload shares the control user, so `Stop` is only as trustworthy as that workload; `Apply`'s host checks still bound where its delta lands |
| `Capture` | `cloud_helper::delta` refuses without the status (`StableCapture`); delta from the hashed unpack baseline, changed entries only (`DeltaOnly`) |
| `Apply` (parent-bound delta, generation, per-path progress) | `cloud_fs::apply_delta` into the root pinned by identity at snapshot; per-entry staged rename; partial progress reported, payload retained; never re-applied (`NoLateReturn`). The generation is the job: a restarted daemon has no in-flight apply. A nested call's delta is applied into its immediate parent's live view (`AgentCalls::apply`, helper `apply`), never the laptop (`ParentBound`); `AgentCalls::settle` cancels and waits for running calls before the parent is captured (`ParentCaptureSettled`) |
| `Lose` / `Restart` / `RetireUncertain` | Unconfirmed end: quarantine, retire, `ExecutionOutcome::Unknown`, no capture (`LossIsUncertain`, `NoReplay`). Restart: `DaemonStore::with_master_token` marks running cloud jobs unknown/uncertain; `marsh workers reset cloud` retires owned sandboxes |
| `DeleteRequest` / `DeleteLands` / `ObserveAbsent` | `CloudSbx::retire` (observed absence, `VerifiedAbsence`), mapped in the cloud ownership extension below |
| `Complete` (return + verified cleanup) | `Settlement::finish` after capture, retire, and apply (`ReturnBeforeComplete`); the client's exit follows that frame |

Before changing the Rust lifecycle, extend the existing six modules and their
code-location/negative-control tables as specified in design section 9.

### Static runtime-profile boundary

The first cloud implementation slice adds no scheduling/ownership/transport
state transition. `marsh-runtime` `SbxRuntimeProfile`,
`validate_sbx_environment`, and `system_with_profile` bind proxy authority to a
trusted bootstrap profile. `marsh-worker/src/main.rs` selects cloud-v2 only via
its root-only startup mode, before Ready. Job frames cannot choose the profile.
Local selection is unchanged; unqualified cloud MCP is refused.

This is static URL/environment/CA validation, **not established by TLC**.
Supporting rejection checks are in `marsh-runtime/src/tests.rs`; the primary
evidence is [fresh-worker cloud qualification](../plan/cloud-worker-profile.md)
under native policy, including verified cleanup. A working internal worker
mode is not a proof of the remaining lifecycle extensions. Session-origin
dispatch (table above) does not use the worker profile.

## Cloud ownership extension

`CloudOwnership.tla` is the cloud counterpart to local `Ownership.tla`, mapped
to `marsh-sbx/src/cloud.rs`. It uses the current stock sign-in, not account
selection or a context-marker protocol. SDK calls are trusted; ID parsing and
filesystem persistence are separate code obligations.

The service can create after a lost reply or daemon restart and expose a new
resource later. Unknown intents stay recorded. An ID returned before visibility
is not evidence that a missing listing proves deletion: `seen` must first
record a matching positive observation. Deletion acknowledgement is separate
from the service deleting the actual resource. Observations can go stale after
they were accurate; ID-addressed effects spare a foreign VM that reuses a name.

| Invariant | Negative control | Required protection |
|---|---|---|
| `NoLeak` | `dropMissing` | Keep missing create intents; a delayed create may still land |
| `NoLeak` | `forgetUnseen` | Keep a known ID that has not been positively observed |
| `NoLeak` | `ackIsGone` | Delete acknowledgement is not actual disappearance |
| `NoLeak` | `staleSeen` | A later sighting cannot validate an earlier caller's absence observation |
| `NoForeignEffect` | `byName` | Effects address the recorded immutable ID, not a reused name |
| `RestartInert` | `restartReady` | Recovered resources are cleanup-only |
| `NoQuarantinedUse` | `useQuarantined` | No new submissions after quarantine |
| `NoCreateReplay` | `replayCreate` | Reconciliation never repeats an uncertain create |
| `CapacityBound` | `noCap` | Unresolved resources count toward capacity |
| `NoUnsafeSubmission` | `stalePoison` | Recheck persistence poisoning under the admission lock, not before waiting |

Positive configs also check `TypeOK` and `OwnedAreOurs`. Retirement has separate
per-resource query state and a prior-sighting snapshot: another observer can
update the shared flag after an absence query has completed. The poison model
also separates a preliminary check from admission, allowing persistence failure
while a submission waits. Those two negative controls reproduce the races
found by adversarial code review; checking one shared `view` would miss them.
`CloudOwnership.cfg`
models one owned name, a foreign name, two IDs and one restart;
`CloudOwnership_concurrent_pass.cfg` models two owned names, three IDs and a
restart. Names abstract fresh requests and are not reused in these bounds.

| Model action | Rust / platform mapping |
|---|---|
| Reserve / Submit | `CloudSbx::create`: validate, private durable intent, `CreationFlight`, one stock inline-image create |
| CreateLands / Reply / Expose | Trusted stock service and create response; response ID persisted before control effects |
| Observe / SeeOwned / Ready | `inventory`, `observe_owned` (durable `observed`), `wait_ready` |
| BeginQuery / FinishQuery / NextQuery | `retire`: snapshot `observed` before each fresh query; retain its ordering through the forget decision |
| LoseReply / Adopt | Create errors retain intent; `reconcile` adopts only for quarantine/cleanup, skipping live creation flights |
| Quarantine / PrepareUse / FailPersistence / Use | `quarantine`, `ready`, `spawn_admitted`; all SDK carrier starts recheck storage/poisoning after acquiring the map lock, then submit under that lock |
| Restart | `open` validates/locks the map and makes loaded Ready entries Quarantined |
| BeginRemove / DeleteRequest / DeleteLands | `retire` persists Removing and requests removal by ID; actual deletion is provider state |
| Forget | `retire`: successful complete inventory absence plus prior positive observation, then durable record removal |

Assumptions/gaps: managed pending names are random/exclusive; other actors may
reuse a known name after removing its old ID. Stock inventory is complete and
accurate when observed once a resource has been exposed; newly created resources
may initially be invisible. IDs are immutable and never recycled. SDK effects
are atomic at their provider commit, not at local carrier exit. Current sign-in
is ordinary trusted host configuration, not a modeled account-switch feature.
No liveness is claimed for an indefinitely hidden/failed create. The model's
poison transition abstracts a failed persistence operation; real filesystem
failure mechanics are exercised separately by the opt-in caller. Disk atomicity,
path/JSON validation, image qualification, streams, clocks/TTL, and job lineage
are outside this bounded ownership model. It is not a composed proof of the
whole cloud product.

Primary implementation evidence is the [real cloud lifecycle caller](../plan/cloud-lifecycle.md),
including foreign-ID refusal, streams, quarantine and cleanup after restart.
