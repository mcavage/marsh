# Cross-component model

Eleven TLA+ modules specify current cross-component state (ten local modules and
the cloud ownership extension). A twelfth, `Placement`,
specifies the cloud placement extension; only its single-crossing,
session-origin part is implemented (see
[coverage and gaps](#proposed-placement-model)). TLC checks their bounded state
spaces; a pass does not prove that the Rust code refines these specifications.
The code-location table below records the implementation mapping and known gaps.

- `Pins.tla`: the services agent's bind-source pins (`marsh-services`): a pin is
  mounted and recorded when a container is created with a bind, leased to that
  container and, through `--volumes-from`, to every container that takes its mounts,
  and unmounted when the last holder is deleted. The environment may delete a pin's
  source at any time; the services VM may restart, which drops every mount.
  Restoration before the engine runs recreates the pins whose source still
  resolves and poisons the others (a dangling symlink recorded as failed) so that a
  container holding one fails to start instead of running on an empty directory
  Docker would make. Invariants: `NeverEmptyDir`, `HeldPinsPersist` (a pin is not
  released while a container that needs it exists), `RunningHasMounts`, `NoLeak`.
  Negative controls: `inheritOff` (the lease of a `--volumes-from` container omits
  the inherited pins), `releaseWhileHeld` (removal releases a pin other containers
  still hold), `restoreFailOpen` (a pin that cannot be restored is left absent),
  `leak` (removal never releases). Creating the pin before the lease is recorded
  (a crash between them) and the periodic sweep of pin paths no record names are
  not modeled; the sweep is `Services::sweep_pins`, tested by the E3 evidence.
- `CloudOwnership.tla`: the owned cloud adapter's persisted intents, delayed
  create visibility/replies, cleanup-only restart recovery, immutable-ID effects,
  quarantine, resource capacity and asynchronous deletion. See
  [invariants, controls and implementation mapping](#cloud-ownership-extension).

- `ServicesVm.tla` (`../design/services.md`): a project's services VM, a new VM kind
  that marsh creates, stops and removes, with the distinct transitions DECISIONS 13 and
  18 give it: start/acquire/release, idle expiry (stop, keep), turning services off
  (per project: revoke its holders, stop its VM, keep data; start and acquire need the project's own opt-in), `marsh stop` (stop, keep), the default `marsh reset`
  (services untouched), `marsh reset --include-services` (remove exactly the confirmed
  projects' VMs and data), `marsh services rm` (one project), daemon restart and garbage
  collection. `data` is a ghost only a confirmed removal clears; `last`, `po` and `ps`
  record the latest transition and the state before it. Invariants `OneVmPerProject`,
  `HeldRuns`, `DataKept`, `StopKeepsServices`, `ResetKeepsServices`,
  `ResetAllRemovesConfirmed`, `ResetAllSparesUnconfirmed`, `RmRemovesOne`, `OffStopsProject`,
  `StartsOnlyWhenEnabled` and liveness `IdleStops`. Controls: `vmPerScope` (OneVmPerProject), `leaseNone` (HeldRuns:
  the holder is not recorded), `idleRemoves` and `gcServices` (DataKept), `idleStopOff`
  (IdleStops), `stopDeletesData` (StopKeepsServices), `resetSkipConfirmation`
  (ResetAllSparesUnconfirmed), `resetIncludeNoop` (ResetAllRemovesConfirmed),
  `rmRemovesAll` (RmRemovesOne) `disableIgnoresHolders` (OffStopsProject) and `enablePerHome` (StartsOnlyWhenEnabled: one
  enabled project enables another). The exact-one-host-mount readiness check (`sbx inspect`
  lists only the project) observes stock's mount table and is not modeled; `r4` live runs cover it. VM naming,
  UUID-checked operations and impostor names are Ownership's (`Remove`, `NoForeignEffect`,
  `OwnedAreOurs`). The Docker disk size is no model state: it is a per-sandbox variable of
  the create invocation (`DOCKER_SANDBOXES_DOCKER_SIZE`), and no global setting is changed.
- `ServicesSnapshots.tla` (`../design/services.md`, "Snapshots in checkpoints"): what
  marsh deletes of services snapshots in a project's pool, under leases, and what a
  checkpoint made while the VM is stopped may claim, for one project with two workspaces
  (the base and a lane) whose VM is running, stopped or gone. A checkpoint holds a snapshot
  of its own, references the workspace's last capture (`reused_from`, DECISIONS 9) or records
  a gap; a snapshot is in use while a live checkpoint refers to it or it is a workspace's last
  capture. Transitions: `Capture`, the fenced stop cutover as separate steps (`BeginStop`
  pauses every container and takes the write fence, the filter's freeze gate on every tracked
  workspace, held until the stop; `CaptureAtStop` captures each tracked workspace or fails;
  `CountStep` asks the engine what runs and fails closed on a running container or a failed
  query; `FinishStop` journals each outcome `proven`/`failed`, or only invalidates the journal
  (`unknown`), and stops; `AbortStop` thaws and releases the fence), `Write` (a running container
  writes) and `Revive` (a client starts or unpauses a container through the endpoint: refused
  while the workspace is fenced), `Start` (clears the old outcome durably, or leaves the marker),
  `ExtStop` (a stop marsh did not cause records nothing), `Reuse` (per workspace: proven ->
  reference, failed -> a gap, unrecorded or unknown -> reference with a warning, none -> null),
  `CkRm`, `Collect` (the project is found from the owned VM, not from records), `Rm` (a new pool
  generation; the checkpoint records keep their ids), the `Lease` a restore holds, `LoseRecord`
  (a checkpoint record lost without the untrack: a publish-to-record crash or a removed
  directory) and `Recover` (a restart reconciles the lane capture the journal still protects).
  Invariants `RefsKept`, `LeaseHeld`, `LastKept`, `ProvenIsCurrent` (what was journalled as
  proven is the workspace's last write: nothing wrote after the capture, even by a container
  started in either window of the cutover, and it was really captured), `NoSilentStale` (a
  reference to a snapshot older than the workspace's last write carries a warning),
  `LaneLastTracked` (after a restart no lane capture is protected without a live checkpoint) and
  liveness `OrphansCollected`. Controls: `reuseLeaseOff` and `rmDropsShared` (RefsKept),
  `rmIgnoresLease` (LeaseHeld), `dropLastCapture` (LastKept), `stopFenceOff` (a container
  started after the count writes before the stop) and `stopPauseOff` (containers write between
  capture and stop) and `queryFailOpen` (a failed count reads as zero, fence off) and `staleTrusted`
  (the clearing at start failed and the old file is trusted) and `stopBaseOnly` (a lane recorded
  as captured without a capture): ProvenIsCurrent; `reuseIgnoresFailure` (NoSilentStale: a failed
  final capture reuses the old snapshot silently), `reconcileOff` (LaneLastTracked),
  `rootsFromRecordsOnly` and `orphanGcOff` (OrphansCollected). The safety config is the full
  bound (69.5 M states, about fourteen minutes on two workers: `\* marsh-check: huge`), the
  liveness configs (`_live_pass`, `orphanGcOff`) a smaller one. Not modelled, deliberately: the
  capture itself (what the agent copies; S3's evidence), the reserve (a failed capture stands for
  it, and the agent checks it), the journal's file protocol (fsync order: `unknown` stands for
  "invalidated" and a stop that cannot be persisted is a step not taken; covered live), and a
  start of a container that bypasses the filtered endpoint (nothing in the product does; the
  count catches it only before the count).
- `ServicesReap.tla` (`../design/services.md`, "Reaping at attempt end"): a job's containers in
  its project's services VM at the end of its attempt. An attempt is recorded (on disk, before
  its first channel reaches the VM) before it makes containers; its end makes the record due;
  a reap that finds the relay up removes that attempt's one-off containers and keeps the
  Compose and restart-policy ones, and the record goes. A daemon death ends its attempts unseen
  and leaves the records; the next relay brings them back. Shell objects are never reaped.
  Invariants `NoEarlyReap` (no reap removes a container of a running attempt), `ShellNeverReaped`,
  `OnlyOwnAttempt`, `KeptPersist`, `RecordCoversOneOffs` (no unrecorded one-off exists) and the
  liveness `ReapsEnded`. Controls: `reapOff` (ReapsEnded), `reapAll` (OnlyOwnAttempt),
  `reapPersistent` (KeptPersist), `reapRunning` (NoEarlyReap), `noRecord` (RecordCoversOneOffs),
  `shellReap` (ShellNeverReaped). Volumes are not modeled: they are never removed (DECISIONS 11).

- `ServicesLanes.tla` (`../design/services.md`, "App data in lanes"): what marsh deletes of a lane's
  services objects (its containers, volumes and networks, and its ledger scope) when its split is
  closed, cancelled or removed, for one project's engine with the base workspace and two lanes
  whose VM is running, stopped or gone. A lane's restore reads that the lane is open and then
  the agent creates the fork's objects (`RestoreCheck`, `RestoreWrite`, two steps); `Close` fences
  the lane, waits for a restore that is under way and journals the drop (`pending`); `DropRun`
  removes the lane's own objects only, cannot run with the VM stopped (`DropLater` records the
  try; the services timer retries when the VM is back) and has nothing to do with the VM gone.
  Invariants `DropOnlyOwn` (no workspace loses objects to another's drop), `NoObjectsAfterDrop`
  (nothing creates a closed lane's objects after its drop is done: a restore still running is
  waited for) and liveness `ClosedLanesAreEmpty` (also across a VM stop). Controls:
  `dropAllScopes` (DropOnlyOwn: the drop is by project, not by workspace), `dropWithoutWait`
  (NoObjectsAfterDrop: the close does not wait for the restore), `dropOff` (journals no drop) and
  `dropNoRetry` (a drop tried with the VM stopped is never retried), both ClosedLanesAreEmpty.
  `apply` merges files only and calls nothing here (`apply-include-generated` is a files control
  and has no place in the model).
- `Sbxenv.tla`: live-reloaded native MCP/extra-mount additions, required MCP load
  before VM readiness, a reloaded (stale) VM loaded before any job launches on
  it, registration reuse without overwrite, inherited extra
  mounts without child-Kit widening, and extra-mount omission from split jobs.
  Its five negative controls are `childKitMount` (ExtraAttenuation),
  `splitExtraMount` (SplitOmitsExtras), `skipMcpLoad` (McpBeforeReady),
  `skipLiveLoad` (LiveLoadBeforeLaunch), and
  `overwriteRegistration` (NoRegistrationOverwrite). Configuration/file/path
  parsing, alias equivalence, rejecting an invalid change (the last good
  configuration stays) and unloading removed servers only from VMs with no
  running job are static code and acceptance checks, not modeled. It abstracts one declared server per Kit; source identity,
  retained mounts and cleanup remain in Worker/Ownership, and fork paths in Workspace.
  Registration inspection/addition are abstracted as one step: concurrent changes
  by other host-global SBX clients are trusted-host behavior, not an atomic
  compare-and-set promise of marsh's separate public CLI calls.
- `Ownership.tla`: one daemon per selected home, stock SBX as a name -> VM table that
  other actors change at any time, the persisted ownership map, persisted create intent
  (intent-then-adopt create), daemon restart, the cached inventory, and explicit operator
  Retire (via `Remove`).
- `Worker.tla`: one owned warm kit VM with its single worker transport (generation),
  up to `CAP` concurrent attempts, quarantine, retire (by the operator, or automatically
  once no started attempt's container can still be running: `AutoRetire`, guarded by the
  ghost `alive` that `ContainerEnds` clears), cancellation, and project/home
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
- `KitSwitch.tla`: one Kit command name and the VMs of the Kit images it has named, across
  daemon restarts. A Kit change (an automatic update, `marsh kit install`/`uninstall`, the
  move to the Kits a newer marsh ships) moves the name at once: new jobs start on the new
  Kit's VM; the VM it moved away from is retired at once only when provably idle (no live
  or uncertain job record, and either served by this daemon run or shown to run no
  container), else it drains. Garbage collection removes a VM only while stock lists it
  stopped with no live or uncertain record, re-checked under the VM's fence after its
  observation. The daemon may restart (forgetting what it served and what drains; live
  receipts become uncertain; containers keep running) and the environment may stop a VM.
  `marsh workers stop` claims a provably idle current VM as an automatic retirement does
  (no job is admitted while it stops), durably marks the VM retired, then submits
  `sbx stop`, which stock lands at once, at any later time (also after a restart), or never.
  Only a stop stock reported, with the VM listed stopped, lifts the mark; otherwise, and
  after a restart, the VM is retired for good (never prepared again, removed) and the Kit's
  next job creates a new VM (a new generation and name) that a late stop cannot reach. The next job starts the same VM; one that does not come
  back healthy, or whose new worker never reports Ready, is replaced only while nothing
  recorded on it can run.
  Invariants `ChangeSparesRunning`, `StartsOnCurrent`, `RunningHasVM`, `StopSparesRunning`;
  liveness `DrainEnds`.
  Controls: `retireBusy` (retire under a running job), `trustTable` (trust only live
  records after a restart), `gcRunning` (collect a running VM), `gcStale` (collect on the
  stale observation), `replaceRunning` (replace a resumed VM on its live records alone) —
  all ChangeSparesRunning — `admitDraining` (StartsOnCurrent), and `stopBusy` (stop without
  the idle claim), `admitStopping` (admit a job to a VM claimed for a stop), `cancelOnError`
  (reopen admission when the stop command errs) and `loseRetired` (the retired mark does not
  survive a restart, so the VM serves again), all StopSparesRunning.
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
  fan-out cap, a per-tree launch rate, and a per-tree Kit VM cap, each as in effect at that
  step (`SetLimits` changes them while jobs run; lowering one ends nothing); the child's rights (view + spawnable Kits) are its
  parent's or narrower. A job's exit, cancel (session or parent), or transport loss revokes its
  capability and cancels every descendant; a slot frees only on confirmed deletion; a daemon
  restart revokes every capability and makes unfinished jobs uncertain, never resubmitted.
  Every job's first step is its entrypoint invoking its own command (script through the PATH
  link, or ELF by absolute path); the link resolves the own name to the image binary only with
  the worker's entry marker and parent = docker-init, so every later own-name call (an agent's
  Bash tool calling itself) is a child of its own Kit under the same caps (there is no same-Kit
  chain cap: depth and the rate bound it). Any job may spawn any Kit in
  its spawn set (every registered Kit by default; the child runs under its own Kit's egress and
  credentials); launchers only narrow the set. A Kit that declares no home (`org.marsh/home@1`,
  `NoHomeKits`) never holds the home view, whether the session or a job started it.
  Environment: jobs exit at will, the user cancels any job, transport loss, leftover containers.

Invariants: `NoForeignEffect`, `OwnedAreOurs`, `NoLeak` (Ownership); `CapacityBound`, `NoReplay`,
`NoStartOnQuarantined`, `LossIsUncertain`, `MountRefcount`, `NoStaleSourceUse`,
`CancelScoped`, `AutoRetireVerified` (Worker); `OneVmPerProject`, `HeldRuns`, `DataKept`, `StopKeepsServices`, `ResetKeepsServices`, `ResetAllRemovesConfirmed`, `ResetAllSparesUnconfirmed`, `RmRemovesOne`, `OffStopsProject` (ServicesVm); `NoEarlyReap`, `ShellNeverReaped`, `OnlyOwnAttempt`, `KeptPersist`, `RecordCoversOneOffs`, `ReapsEnded` (ServicesReap); `DropOnlyOwn`, `NoObjectsAfterDrop` (ServicesLanes); `ChangeSparesRunning`, `StartsOnCurrent`, `RunningHasVM`, `StopSparesRunning` (KitSwitch); `RevokedGrantInert`, `GrantTargetBound`, `OneTimePermission`,
`DefaultSparesRunning`, `DefaultRevokedInert`, `DefaultReachesNewVms` (Grants); `SnapshotImmutable`, `CheckpointImmutable`, `AdminReadOnly`,
`ExposedClean`, `OneBase`, `ChildFromParent`, `ForkIsolation`, `NoWriteToUserTree`,
`CaptureStable`, `LineageWF`, `BudgetBound`, `LiveUnderLive`, `CancelPropagates`,
`CancelScoped`, `NoRemoveWhileLive`, `NoReplay`, `WorkspaceRecorded` (Workspace); `LineageWF`,
`Attenuation`, `BudgetBound`, `FanOutBound`, `RateBound`, `LiveUnderLive`, `NoOrphanRunning`,
`CancelPropagates`, `RevokedAtRestart`, `RestartUncertain`, `NoReplay`, `EntryOnce`,
`NoEntryRecursion`, `SpawnSetAttenuates`, `KitVMBound`, `NoHomeKitHome`, `AdmittedUnderLimits` (Process). Liveness:
`AttemptsTerminate` and `QuarantineHeals` (Worker; weak fairness, no operator action), `DrainEnds` (KitSwitch; weak fairness on job ends and the drain pass), and `EventuallySettled` (Workspace; weak fairness on
daemon steps and process progress, checked in the smaller `Workspace_live_pass.cfg`).

Each module has a `Bug` constant. `Module.cfg` uses `Bug = "none"` and checks every
property. Each `Module_<bug>.cfg` turns on one guarded wrong action and checks only the
property that must catch it. Worker's `retireUnverified` lets automatic recovery retire
a quarantined VM while a started attempt's container still runs (AutoRetireVerified). Default-publication controls: `loadRunning` (load
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
(EventuallySettled); for open lanes, `laneAfterClose` (a closed or captured lane admits a
job), `captureLiveLane` (close captures while a lane job runs), and `laneRestartLive` (a
restart keeps an open split open and forgets its live lane jobs), all CaptureStable,
`checkpointMounted` (a job mounts a checkpoint: CheckpointImmutable), `laneSplit` (a lane
job's split snapshots the project its cwd names: ChildFromParent), `restartReopens` (a restart
forgets a close under way and reopens the lanes: ClosedStaysClosed), and `removeLiveLane` (a
kept lane split is removed while a lane job a close timed out on still runs:
NoRemoveWhileLive). Positive Workspace configs split the state space: `Workspace.cfg` (2 splits x 2 labels,
mixed shell/Kit, nesting at depth 2, no restart), `Workspace_depth3_pass.cfg` (3 splits, one
shell label at the root, argv nested, depth 3, one restart), `Workspace_restart_pass.cfg` (one
split, two mixed siblings, one restart), `Workspace_lanes_pass.cfg` (one open lane split,
two lanes, two jobs per lane, one restart), and `Workspace_live_pass.cfg` (liveness).

**Outside the model: a lane shell.** A shell the user starts from the host in a lane's copy (`marsh` or `marsh -c`, project = the split's root, start = the copy) is trusted user code in the shell VM, which mounts the whole project root. It is not a lane job: it is not counted live, a close neither waits for it nor fences it, and so `CaptureStable` holds for lane jobs only: what such a shell writes after a lane is captured is not in the capture. `marsh` says so when an interactive one starts ("changes you make here are captured only if made before Finish"); `workspaces.md` s14 documents it. `split apply` is outside the model too (git runs in the shell VM against the project after the capture).
Process controls, one per invariant, each in a real action: `noDepth` (LineageWF), `widen`
(child rights recomputed from the session's grants: Attenuation), `freeOnRevoke` (BudgetBound),
`noFan` (FanOutBound), `noRate` (RateBound), `orphanChildren` (LiveUnderLive), `revokeOnly`
(descendant capabilities revoked but containers not signalled: NoOrphanRunning), `noPropagate`
(CancelPropagates), `restartKeepsCap` (RevokedAtRestart), `keepRunning` (RestartUncertain),
`replay` (NoReplay), `linkIgnoresMarker` (the link dispatches a script entrypoint's own name as a
child: NoEntryRecursion), `linkIgnoresPpid` (the link resolves the own name locally whenever the
marker is set, which an ELF entrypoint leaves in place: EntryOnce), `staleLimits` (admission
uses the limits read at daemon start, not the ones in effect after a change: AdmittedUnderLimits),
`spawnWiden` (child spawn set reset to every registered Kit: SpawnSetAttenuates), `noKitCap` (a
child brings one more distinct Kit VM into a full tree: KitVMBound), `noDetachCap` (DetachedBound),
`detachHolds` (a detached cloud run keeps its pool slot: DetachedOutOfPool), `restartDetached` (a
restart makes a detached run uncertain: DetachedSurvives), `childKeepsHome` (only a top-level
launch applies a Kit's no-home declaration, so an agent's tool call inherits the agent's home:
NoHomeKitHome). `Detach` takes a started root job with
no children out of the pool (`marsh-daemon` `detach_cloud_job`, `JobState::Detached`, not `live`);
`Collect` ends it without a second start. Wall time is not modeled:
a deadline is the environment's `Cancel(j, Session)`, and a child's inherited deadline is an
admission-time value checked by `process_tests.rs` and P35, not by TLC. Positive Process configs:
`Process.cfg` (one script Kit, 3 jobs, depth 3, one restart, every invariant; ~40 s),
`Process_kits_pass.cfg` (a script Kit and an ELF Kit that declares no home, spawn-set
narrowing, no restart; ~5 min with 2 workers),
`Process_limits_pass.cfg` (two Kits, limits lowered and restored while jobs run, every launch
within the limits then in effect, the caps still hold),
`Process_caps_pass.cfg` (4 jobs, tight pool/fan/rate caps, no restart; ~1.5 min),
`Process_kitvms_pass.cfg` (two ELF Kits, one Kit VM per tree, no restart; ~1.5 min),
`Process_rate_pass.cfg` (the launch rate over time: the clock ticks, launches resume as the
window moves, no window holds more than `MaxRate`), other configs keep the clock still
(`MaxTime = 0`), where the rate is a bound on each tree's launches,
`Process_live_pass.cfg` (`EventuallySettled`).
A config named
`Module_<name>_pass.cfg` is positive.

Assumptions (outside the model, stated not checked):
- A1: host `sbx` is trusted, and every host VM name is a fixed prefix plus a random
  8-character base36 id (`marsh-k-<id>`, `marsh-s-<id>`) chosen by this daemon and kept in
  its ownership map, the only source of VM names. No other actor creates a VM under such a
  name, so host stock operations may be name-addressed and an intent may be adopted by name.
- Legacy `marsh --dev` state: a map an older daemon wrote may hold development grants
  (child VM names under a `marsh-x<5 base36>-` prefix) and a dev shell VM. At load
  (marsh-sbx `vm_ownership.rs` `retire_dev_vms`) each becomes an `own` entry marked
  `retired`, removed by `Remove` / `RemoveRetired` like any other. A1 holds for them: the
  child daemon chose each name under a random grant prefix that an earlier daemon of this
  same map chose and recorded. This is a load step, not a model
  action.
- A2: stock create is atomic on name uniqueness.
- Create persists the intent (random name, purpose, kit identity) before `sbx create`;
  the next inventory adopts a present intent and drops an absent one loaded at startup.
  `Ownership_noIntent` shows that an in-memory intent leaks a created VM across a restart.
  `Ownership_nameReuse` (OwnedAreOurs) and `Ownership_nameReuseEffect` (NoForeignEffect)
  drop A1, letting another actor create a VM under a host name: name-addressed operations
  then reach it. With A1 these two invariants hold because no other actor ever creates a
  VM in the model; the controls show they still catch the defect A1 rules out.
- A3: `sbx ls` is accurate when it is observed. It can be stale immediately afterward.
- A4: if source identity matches before and after `sbx mount`, the mount used that identity.
  The pathname swap inside the stock mount call remains an upstream gap.
- Source overlap with other sources or the control root is a static path check before
  admission. It is not modeled.
- Host names behave the same and are independent, so configs use one host name. Bounds
  are small: 3 UUIDs, 3 attempts with CAP=2, 2 transport generations, and 2 source identities.

Run: `docs/model/check.sh [--quick | --huge | --no-huge] [--cores N] [--workers N] [CONFIG...]`. Configs run
concurrently inside a core budget (default: the runner's CPUs, at most 8), one TLC process each with its own
log; the verdict of a config does not depend on scheduling. `--quick` (run by `make test`, about 90 s on 8
cores) checks every negative control and, for each module, a positive config with every invariant
(`Ownership.cfg`, `Worker.cfg`, `Grants.cfg`, `Pins.cfg`, `Process.cfg`, `Workspace_restart_pass.cfg`) plus the liveness
configs (`Process_live_pass.cfg`, `Workspace_live_pass.cfg`). It skips only the configs marked
`\* marsh-check: slow`, the larger positive bounds: `Workspace.cfg` (about 7 min), `Process_caps_pass`,
`Process_kits_pass`, `Process_kitvms_pass` (about 2 min each), and `Workspace_depth3_pass` (about 30 min).
The default and `make regress` run every config. `--huge` / `--no-huge` split the one config that does
not fit a CI job from the rest: CI's model job runs `--no-huge`, and the weekly
[model.yml](../../.github/workflows/model.yml) runs `--huge`. `TLC_CACHE=DIR` skips a config whose spec, config,
jar and `check.sh` are byte-identical to one that already gave its expected verdict (`make test` uses
`target/tlc-cache`); the full sweep (`make regress`, `MODEL_FLAGS=`) does not set it. This needs Docker (or `TLC_RUNNER=java` with a local
Java 17+) and the pinned TLA+ tools jar, which `make test-model` downloads and checks (the build is
mirrored as a release of this repository because upstream replaces its v1.8.0 asset in place; see the
Makefile). Run directly, `check.sh` takes the jar from `TLA2TOOLS_JAR`. The script exits nonzero if a
positive config fails or a negative control passes or fails for the wrong reason. The jar is not committed.

| Spec action | Code location |
|---|---|
| Sbxenv: Create / Register / Load / Ready / Remove / Reload | `marsh-backend/src/sbxenv.rs` `with_sbxenvs` (first snapshot), `refresh_sbxenv` (live reload; `register_new_servers` before a change is accepted; Reload), `watch_sbxenv_pass` (warm VMs), `lib.rs` `kit_spec` (one VM per workload, independent of the files), `prepare_one_authorized` (required setup before ready cache, and again for a cached VM: `configure_prepared_mcp` loads a stale VM before the job), `configure_kit_mcp`/`sync_mcp`; `marsh-sbx/src/mcp_environment.rs` `ensure_mcp_server` (inspect/reuse or add, no overwrite), `load_mcp_server` / `load_shell_mcp_server` (owned UUID); worker reset and scope teardown |
| Sbxenv: Launch (ExtraAttenuation, SplitOmitsExtras) | `marsh-backend` `execute` selects a root's environment, an ordinary child's inherited receipt view, or no extras for a split; `sbxenv.rs` `additional_grants` applies existing descriptor/overlap admission; `inherited_environment` maps only parent-recorded additions. Rust callers: `sbxenv::tests`, `marsh-sbx/tests/mcp_environment.rs`; real caller: `tests/acceptance/sbxenvs_uat.py` |
| ServicesVm: Start / Acquire / Release / IdleExpire | marsh-sbx `services_vm.rs`: `ensure_services_vm` (Start/Acquire: create under an ownership intent keyed by the canonical project root with `DOCKER_SANDBOXES_DOCKER_SIZE` on that one `sbx create`, mount only the project, verify mount and Docker size, retained lease), `stop_services_vm` (IdleExpire: drops the lease and `sbx stop`s by recorded UUID, never removes); marsh-backend `services.rs` `services_start` (holders; `lease-none`), `services_timer_pass` / `services_idle_stop` (expiry and idle timer; `idle-stop-off`), daemon `spawn_services_timer` |
| ServicesVm: Disable(p) / Enable(p) (per project) | marsh `shell_choice.rs` `services_config` (`marsh config services on\|off` adds or removes the current project's canonical root in `services_projects`); marsh-backend `services.rs` `Settings::enabled_for` (break `services-enable-per-home`), `require_services_enabled(root)` (`start`/`hold` refuse for a project that is not listed, even when another is), `services_timer_pass`: a tracked project that is not listed has its holders cleared and its VM stopped through `services_idle_stop`, data kept; `stop`/`rm`/`release` never need it on |
| ServicesVm: StopCmd / ResetCmd | marsh-backend `lib.rs` `teardown_scope` -> `services.rs` `services_scope_teardown`: for `marsh stop` `stop_all_services_vms_before` (stopped, kept, holders dropped; break `services-stop-deletes-data` removes instead), for the default `marsh reset` nothing but a `Kept` report per VM; `StockSbx::recorded_vms` skips services VMs and `vm_gc.rs` `decide` keeps them (Gc) |
| ServicesVm: ResetAll(c) | `marsh reset --include-services`: marsh `product.rs` `confirm_services_in_reset` (lists the projects from `marsh services status`, asks or takes `--yes`; no terminal without `--yes` refuses) -> daemon `PublicRequest::ResetScopeIncludingServices { confirmed }` (host lifecycle authority) -> `DaemonBackend::remove_services` -> `services.rs` `services_remove_confirmed` (break `reset-include-services-noop`) -> marsh-sbx `remove_confirmed_services_vms_before` (removes by recorded UUID only the VMs whose project key is in `confirmed`; break `reset-skip-confirmation`) |
| ServicesVm: Rm(p) / Restart | `marsh services rm` -> `services_remove` -> marsh-sbx `remove_services_vm` (that project's VM only); Restart: `services_timer_pass` re-arms an idle timer for every running services VM after a daemon restart |
| ServicesVm: Acquire / Release by a job or shell (E2; no invariant or negative control changes) | marsh-backend `services.rs` `services_hold_job` / `services_release_job` and `endpoint.rs`: a job of a services-enabled project holds the project's VM awake (a `job-N` holder expiring at its wall clock plus 120 s, never past 24 h) once it uses the endpoint or the project's relay exists, and releases it when the job ends (`JobEndpoint` drop, on every path out of the response loop); a shell holds it for the shell's life (`ShellEndpointGuard` drop). These are the model's Acquire and Release with the job or shell as the holder, so `HeldRuns` (a VM with a running user is not idle-stopped) is what they uphold and break `lease-none` (`services_hold_job` records nothing) is their negative control. Port listeners and endpoint channels are not VMs, containers, jobs, mounts, grants or ownership records, and nothing marsh kills or reassigns changes: a job's listeners close with its attempt (`Forwards::shutdown`), a shell's with its guest relay (`GuestForwards::shutdown`), and the objects a job or shell creates in the services engine are reaped (or kept, DECISIONS 11) by E4, not here |
| ServicesSnapshots: Capture | `marsh-daemon/src/checkpoint.rs` `Checkpoints::make` -> `services_for` (after the tree snapshot; the snapshot is held `pending` from before the capture until the record exists, and nothing is held across the capture, so a second checkpoint reaches the agent's busy gap: break `capture-admission-blocks`); marsh-backend `services_snapshots.rs` `services_capture` (a ready relay only, never starts or creates a VM: `capture-ensure-vm`, `backend-services-disconnected`; the capture gate shared with other checkpoint captures, exclusive with a cutover), `capture_through` (the reserve is sent with the request and checked by the agent on its pool descriptor, `marsh-services` `capture.rs` `check_reserve`: `admit-ignore-pool`, agent `space-check-stale`, `fault-pool-unreadable`, `unreadable-pool-ok`; strict manifest validation `ServicesRecord::from_manifest`, `manifest-refs-unchecked`; the journal) |
| ServicesSnapshots: BeginStop / CaptureAtStop / CountStep / FinishStop / AbortStop / Revive (ProvenIsCurrent) | marsh-backend `services_snapshots.rs` `services_stop_prepare` (the capture exclusion, held in the returned `StopPlan` until the VM has stopped, by `services_stop`, `services_idle_stop` and `services_scope_teardown`; a stop whose exclusion or journal cannot be had is refused or deferred: `capture-gate-ignored`, `journal-neither-ignored`) and `services_cutover`: the write fence (`Relay::fence` -> the agent's `Capturer::fence`, the filter's freeze gate on every tracked workspace, held until `unfence` or the agent exits with the VM: `stop-fence-off`), every container paused by marsh-sbx `services_quiesce` (`stop-pause-off`), every tracked workspace captured, `services_running_count` (fails closed on the docker exit status: `fault-engine-query`, `engine-query-fail-open`), a lane no longer tracked untracked again, then the outcomes journalled bound to the VM UUID before the stop (`stop-base-only`, `idle-final-capture-off`, holds `hold-cutover-after-capture`/`-after-count`); `services_cutover_abort` thaws exactly the frozen ids, releases the fence and forgets the outcome; `services_start` clears the outcome durably before the VM starts. marsh-sbx `services_net.rs` `owned_engine` classifies the VM against the ownership map and a fresh inventory before every one of these execs (table test `no_exec_reaches_a_services_vm_that_is_not_the_recorded_running_one`); `marsh-services` `capture.rs` records a frozen container as running |
| ServicesSnapshots: ExtStop / Reuse (NoSilentStale) / Start | `checkpoint.rs` `reuse_source` over `DaemonBackend::services_reuse` -> `services_reuse_decision` (per workspace: `proven` -> reference; `failed` -> a gap, `reuse-ignores-failure`; no outcome -> reference with the stopped-outside-marsh warning; an entry of another VM UUID is never used, `journal-vm-unbound`; an entry marked `suspect` by a restart that found the marker, or no entry in a lost journal, is `Unknown` -> the workspace's latest record with a warning, `journal-loss-silent`; `reuse-last-snapshot-off`); the journal `persist_journal` (file `fsync`, rename, directory `fsync`; the marker first for a change that matters for a stop, removed after: `journal-write-fail`, `journal-marker-fail`, `journal-crash-after-intent`, `journal-intent-off`) |
| ServicesSnapshots: CkRm / Lease (RefsKept, LeaseHeld, LastKept) | `checkpoint.rs` `Checkpoints::remove` and `retain` -> `release_snapshots` (a lane left with no checkpoint is untracked first; then one decision, `in_use`: a live record of the root naming it through `snapshot` or `reused_from`, `pending`, or the backend's `services_protected` (every workspace's last capture, captures being recorded); `begin_drop` decides and marks `dropping` in one step under the state lock, so a reuse that starts later is refused; `retention-keep-snapshots`, `reuse-lease-off`); a leased checkpoint is a live record, `Checkpoints::lease` makes `remove` refuse and `retain` skip it (unit test `a_leased_checkpoint_outlives_retention_until_released`); the drop is `DaemonBackend::drop_services` -> `services_drop` (the relay's `drop_snapshot`, and it forgets a last capture that is gone) |
| ServicesSnapshots: Collect (OrphansCollected) | `checkpoint.rs` `collect_orphans` / `collect_orphans_of`, run from daemon `spawn_services_timer` every 5 s (the first pass after start collects what a lost checkpoint directory left): the projects are `services_snapshot_roots` (journal entries and every owned services VM, found with no record and without starting a VM; `orphan-roots-from-records`), `list_services` of a ready VM only, `begin_drop` read after the list, a snapshot younger than 15 s left to the capture that may still be recording it; `orphan-gc-off` |
| ServicesSnapshots: Rm / LoseRecord / Recover (LaneLastTracked) | `services.rs` `services_remove` and `services_remove_confirmed` call `forget_last_capture` (the pool's snapshots went with the VM and the tracked lanes are forgotten); checkpoint records keep their ids and a restore reports them missing. `Checkpoints::open` drops a record whose directory is gone; `attach_backend` then calls `DaemonBackend::services_reconcile` -> `services_reconcile_lanes`, which untracks every journalled lane capture without a surviving record (`lane-reconcile-off`) |
| ServicesVm: Acquire / Release by a job or shell (E2; no invariant or negative control changes) | marsh-backend `services.rs` `services_hold_job` / `services_release_job` and `endpoint.rs`: a job of a services-enabled project holds the project's VM awake (a `job-N` holder expiring at its wall clock plus 120 s, never past 24 h) once it uses the endpoint or the project's relay exists, and releases it when the job ends (`JobEndpoint` drop, on every path out of the response loop); a shell holds it for the shell's life (`ShellEndpointGuard` drop). These are the model's Acquire and Release with the job or shell as the holder, so `HeldRuns` (a VM with a running user is not idle-stopped) is what they uphold and break `lease-none` (`services_hold_job` records nothing) is their negative control. Port listeners and endpoint channels are not VMs, containers, jobs, mounts, grants or ownership records, and nothing marsh kills or reassigns changes: a job's listeners close with its attempt (`Forwards::shutdown`), a shell's with its guest relay (`GuestForwards::shutdown`), and the containers a job creates in the services engine are reaped by `ServicesReap` (below), not here |
| ServicesReap: Begin / Create / End / Kill / Restart / Reap / ShellExit | marsh-backend `services_reap.rs` `services_reap_track` (Begin: the record is written by `endpoint.rs` `hold` before the attempt's first channel reaches the VM; break `reap-off` writes none), `services_reap_attempt_ended` (End: `JobEndpoint` drop, on every path out of the response loop), `services_reap_pass` (Reap, once the project's relay is up: at a job's end, and at the recovery after a daemon start when the `services_reap_startup`; Restart brings the relay back and Kill leaves the records on disk); there is no periodic retry, so a failed reap waits for the next such trigger); marsh-services `reap.rs` `reap` (Reap: the ledger and the labels select the attempt's containers, `keeps` keeps Compose and restart-policy ones, the removal never takes volumes; breaks `reap-all-scope` (Victims = every live container) and `reap-persistent`); the policy's container create records the owner (`policy/endpoint.rs` `record_container`, Create); `endpoint.rs` `ShellEndpointGuard` drop (ShellExit, break `shell-reap-on-exit`; the normal shell reaps nothing). Negative controls: `reapOff` (ReapsEnded), `reapAll` (OnlyOwnAttempt), `reapPersistent` (KeptPersist), `reapRunning` (NoEarlyReap), `noRecord` (RecordCoversOneOffs), `shellReap` (ShellNeverReaped) |
| ServicesLanes: Create / RestoreBegin / RestoreCheck / RestoreWrite | marsh-daemon `split.rs` `SplitEngine::create` (an open split `--from` a checkpoint with app data: the record is inserted with the split marked `restoring`, each lane's workspace `fork-SPLIT-LABEL` is registered with the backend (`services_lane`), then `restore_lanes` runs the lanes in parallel), `lane_services.rs` `restore_lane` (`restore_prepare`, validation of the whole result, `abort` read before the lane files are written (RestoreCheck) and before `restore_up`, the agent's create (RestoreWrite): `marsh-services` `restore.rs` `prepare` / `up`). The checkpoint (and so the snapshot its record names) is held by a lease from the plan until the restore ends (`hold-restore-after-lease`) |
| ServicesLanes: Close (cancel, rm) | marsh-daemon `split.rs`: `close` after fencing the split (`closing`, the store's fence), `cancel_request` and `remove` call `wait_restore` (up to 150 s for `Inner::restoring` to clear: `dropWithoutWait`) and then `drop_lane_services`, which forgets each lane with the backend and gives `LaneDrops::drop_lane` its workspace; the drop is journaled in `lane-drops.json` before it is tried (`lane-drop-off`, `drop-noop`: `dropOff`) |
| ServicesLanes: DropRun / DropLater / retry | marsh-daemon `lane_services.rs` `LaneDrops::try_drop` -> `DaemonBackend::services_drop_workspace` -> marsh-backend `services_lanes.rs` `lane_drop_workspace` (a ready relay only, never starts a VM; a VM that exists but is not ready is `Later`, a project with no VM has nothing) -> `Relay::drop_workspace` (`marsh-services` `host.rs`, frame `drop_workspace` carries the workspace and nothing else) -> agent `restore.rs` `Restorer::drop_workspace`: containers (force, anonymous volumes), then volumes, then networks, each removed **through the filter as that workspace** (a `GET` lists only its own objects, so a drop cannot name another workspace's: `dropAllScopes`; the test mode `lane-authority-off` is the defect that would), then `Services::forget_scope` (its substituted sockets, which stop their listener threads, and its default network entry in the ledger). What remains is reported in `left` and retried by the services timer (`LaneDrops::retry`, every 5 s: `dropNoRetry`); `apply` (`split apply` carries `out/diff.patch`) calls nothing here |
| Services relay (E1; no model change) | marsh-sbx `services_relay.rs` `ServicesRelays::{establish, close, close_all}`, `marsh-services` `host.rs`: the relay is one `sbx exec` client process the daemon starts and ends itself (by the handle it holds) and a channel table in the daemon; channels are not VMs, containers, jobs, mounts, grants or ownership records. `services_stop`, `services_idle_stop`, `services_remove`, `marsh stop` and `reset --include-services` now close the project's relay before they stop or remove the VM, which changes nothing about what is stopped or removed. A new engine generation (a replaced `marsh-svc-engine` container, S2) is the only thing that kills user containers, and it is unchanged: E1 starts a new generation only when the engine is not ready or the relay ended for a violation. `ServicesVm.tla` states no invariant about relays, so there is no new invariant, negative control or transition |
| Inventory, Use, StartVM, Remove | marsh-sbx `lib.rs` `StockSbx::stock_view` (cached `sbx ls --json`, refreshed on demand; `stock_inventory.rs` decodes it) and `classify_vm` -> `vm_ownership.rs` `classify` (owned only with its recorded UUID present, else foreign or absent); `ensure_kit_vm_authorized` / `ensure_shell_vm_inner` (StartVM); `remove_owned_vm` (fresh classification, `sbx rm`, then proves the recorded UUID is gone) |
| CreateBegin / CreateDo / Record / Restart | marsh-sbx `vm_ownership.rs` (`assign`, `restore_intent` for a replacement under a just-removed name, `store_view` adopt/drop; `persisted` retires legacy `marsh --dev` grant names and the dev shell VM at load, `retire_dev_vms`) |
| Remove (incl. operator Retire) | marsh-backend `vm_gc.rs` (`marsh gc`, daemon start, hourly): only names from this daemon's map (`StockSbx::recorded_vms`, fresh inventory, recorded UUID present); a current Kit's VM is claimed as by a reset under its Kit lock, any other through marsh-sbx `remove_unused_kit_vm` (refuses quarantined, in-use, or — unless this daemon served it — running VMs) or `reset_shell_vm_before`; then marsh-sbx `retire_kit_vm_before` via `marsh workers reset KIT` (a quarantined VM's references, and pins of shells no longer attached, end with the VM; a host caller's session stops being attached when it detaches, or when marsh-daemon `client_liveness.rs` finds its process gone with no job or shell attachment in flight) and via `marsh stop`/`reset` `teardown_scope` (lifecycle admission proves no attached shell or running job, so every retained reference ends with the VM); `shell_recovery_specs` includes persisted `owned_shell_names` across personal-environment generations, used only for UUID-checked removal; `retire_stale_shell` when an owned shell VM's marker names another shell image (an upgrade), refused while a session or live mount holds it, then CreateBegin again under the same name |
| Admit / Run / Exit / Cleanup / Cancel | marsh-sbx `StockSbx::launch_worker` (refuses a quarantined VM or a stale worker generation; one `AttemptRoute` per attempt on the VM's retained worker transport); marsh-worker `control.rs` (per-attempt start, exit and cleanup frames) and `transport.rs` (`cancellation`); a `docker create` that Docker refused is Run+Exit+Cleanup only after marsh-runtime `remove_labelled` proves no container with its per-create label remains (`RuntimeError::CreateRefused`); any other create error is uncertain and quarantines. Not implemented as modeled: a per-VM `CAP`; concurrency is bounded by the daemon's session pool (`marsh-daemon/src/process.rs` `admit`) |
| TransportLoss / ReplaceTransport / Retire | marsh-sbx `ensure_worker_lease` (a dead transport with live attempts quarantines the VM in `quarantined_workers`, with its reason; a live one that is slow to answer a ping fails only the new start; an idle one is replaced under a new worker generation; a reinstalled worker artifact is not swapped in under running attempts), `WorkerLoss` (the loss diagnostic), `stop_quarantined_because`, `retire_kit_vm_before` (`marsh workers reset KIT`). marsh-backend `execute`: a start or wall timeout with the transport up sends Cancel and waits `WORKER_CANCEL_SETTLE` for the worker's verified report (Cleanup), not TransportLoss |
| KitSwitch: Change / Start / RetireDrained / Reuse / GcObserve / GcRetire / Restart / StopVM / UserStopClaim / UserStopSubmit / StopLands / StopFails / UserStopReturn / RemoveRetired / Replace | marsh-backend `lib.rs`: `claim_old_vm` (Change) and `retire_drained` both go through `automatic_claim` (CanRetire: not busy or pinned, not quarantined, marsh-daemon `DaemonStore::begin_automatic_retire` refuses reservations and unfinished or cleanup-uncertain receipts atomically, and a VM this run never served needs marsh-sbx `verify_no_running_containers`), never the operator-reset claim; Change callers are `apply_upgrade`, `unregister` (`uninstall_kit`, `move_to_shipped`) and `bring_shipped_source_kits`. Start is `kit_spec` of the registered name; Reuse is `retire_drained` dropping a VM the name names again. `vm_gc.rs` `collect_vms` (GcObserve) and `remove_recorded` (GcRetire: the store claim, then marsh-sbx `remove_unused_kit_vm` / `remove_stopped_shell_vm`, which list the VM afresh under its transition fence and refuse it unless stopped). Restart is daemon start (`draining_kits`, `registered_workers` in memory; receipts journaled; `DaemonStore::with_master_token` makes running jobs unknown). StopVM is stock. UserStopClaim is marsh-backend `worker_stop.rs` `stop_one_worker` under the Kit lock: `DaemonStore::worker_job_count`, quarantine and `kit_vm_in_use` refuse with the reason, then `automatic_claim`; UserStopSubmit is marsh-sbx `stop_idle_kit_vm` (under the VM's transition fence, refusing retained job mounts, live pins or a busy worker lease): `VmOwnership::mark_retired` persists purpose `retired` in `vm-ownership.json` before `sbx stop`; StopLands / StopFails are stock. UserStopReturn: settled only when `sbx stop` succeeded and a fresh listing shows the VM stopped, then `unmark_retired` and `finish_worker_reset`; otherwise the mark stays (`assign` / `lookup` never return a `retired` entry, so the next job gets a new name; `ensure_kit_vm` refuses a retired name) and `remove_retired_locked` removes it; `stop_one_worker` never `cancel_workers_reset`s after a submitted stop. RemoveRetired is that removal or `remove_retired_kit_vms` (each background pass, from daemon start), which also removes the legacy `marsh --dev` VMs that `vm_ownership.rs` `retire_dev_vms` retires at load. A retired name that a fresh listing shows held by another UUID is never named in a stock call; it is forgotten only when that listing has no VM with the recorded UUID, and otherwise kept and retried (`remove_retired_locked`). Restart keeps the mark: it is in the ownership map. Start resumes it in `prepare_one_unnoted` (`kit_vm_exists` / `ensure_kit_vm` start a stopped VM). Replace is `replace_unhealthy` (`replace_after`: `WorkerNotReady` from `read_worker_ready` on any start, other start failures only for a VM `kit_vm_stopped` observed stopped; a VM not observed stopped needs this run to have served it or `verify_no_running_containers`; never quarantined or with unsettled jobs) and marsh-sbx `replace_unhealthy_kit_vm`. |
| ContainerEnds / AutoRetire | the quarantine stop (`stop_quarantined_name`, `sbx stop`) or the container's own end; marsh-backend `worker_recovery.rs` `recover_quarantined_vm` (background pass each minute, and before a job prepares the VM): no unfinished job record on the VM (`DaemonStore::worker_has_unfinished_jobs`), then marsh-sbx `verify_no_running_containers` (recorded VM absent, stopped, or running with an empty `docker ps`; otherwise it stays quarantined with the reason in `WorkerQuarantine::recovery`), then the same retirement as Retire; held slots are released as after a reset |
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
| Workspace: Restart / Recover | `split.rs` `SplitEngine::open` (journal load: `run` -> `uncertain`, launched branches `untrusted`, forks retained; capabilities are in-memory and lost; an open lane split whose lanes journal no `live` job stays `run`; one whose journaled `closing` is set is `kept` with its open lanes `uncertain` (ClosedStaysClosed); `live` is then zeroed, a pre-restart lane job being an unconfirmed receipt) |
| Workspace: OpenLane / LaneJob / LaneJobEnd | `split.rs` `create` (`open`: lanes in state `running`, reply `SplitOpened`), `admit_lane` (split `run`, not `closing`, lane `running`, caller is the creator; `live` += 1, journaled) called from `marsh-daemon/src/lib.rs` `ConnectionHandler::handle` for an `Execute` whose cwd is in a fork; `LaneGuard` drop (`live` -= 1) when `Execute` returns. Admission precedes Kit preparation, so the atomic step is completed by `lib.rs` `DaemonStore::fenced` (via `fence_split`, called under the split engine lock in the same step that sets `closing`), checked in `begin_job_inner` and `mark_execution_submitted`: a fenced split's fork records or submits no job |
| Workspace: Create by a lane job (ChildFromParent; control `laneSplit`) | `split.rs` `plan`: a `Creator::Job`'s fork comes from its recorded mounts (`JobLineage::split_of`, which descendants inherit), never its cwd; a lane split's job is refused before any source is chosen; a recorded fork whose split is gone is refused, never `Source::Project` |
| Workspace: Close / LaneCapture / LaneTimeout / LaneKeep | `split.rs` `close` (`closing` set; `settle_lanes` waits 30 s on `live` and on `DaemonStore::fork_jobs` (a queued, running, or detached cloud job whose mounts name the fork is live: a detached run is a lane job until collection settles it, and `ConnectionHandler::handle` collects it every 3 s while the close waits); `capture` per open lane; uncertain on timeout or uncertain cleanup; `kept`); `closing` is a journaled `SplitRecord` field, journaled by `save_checked` (snapshot and write serialized by `saving`, a unique temporary file each). Close is never undone (the model's Close is one step): a failed write leaves the split fenced and `closing` in memory, and a repeated close or cancel resumes it. A restart keeps it only once `closing` reached `splits.json` (Restart's closing case; covered when the later lineage write fails: `workspace_cli.rs` `a_restart_after_a_close_whose_lineage_write_failed_leaves_the_split_uncertain`); if the first write failed before its rename, disk still says open and the split is preserved as open (`a_restart_after_a_close_that_never_reached_the_journal_keeps_the_split_open`); `Inner::closers` (in memory) tells a running close from one stopped at the journal. `cancel_request` refuses a split a close is running on, fences it under the lock, and cancels live fork jobs (a detached cloud run is discarded instead: `SplitRecord::cancelled` is journaled with `closing` and `DaemonStore::cancel_split` set in the same step, re-set from the journal at start, so every collector pass and `lib.rs` `SplitCancel` discard it and `detach.rs` `collect_job` never writes it back; `mark_cloud_running` checks the fence and the job's cancellation flag under the store lock, so a cloud lane job still uploading never starts; the setup checks the same flag before and after the sandbox is created and around the upload; `cloud/detach.rs` `import_unless_cancelled` is the last check and the import under the root's import lock; `DaemonStore::cloud_setup_orphans` hands the collector the sandboxes of jobs that were Queued at restart); a job cancelled before its control registers gets INT (and KILL after the grace) at `process.rs` `register_job_control` |
| Workspace: CancelKept / RemoveKept (NoRemoveWhileLive; control `removeLiveLane`) | `split.rs` `cancel_request` on a kept or uncertain lane split (`cancel_lane_jobs`; lanes stay `uncertain`, so `join` keeps it, `kept_reason`); `remove` refuses while a lane counts `live`, `DaemonStore::fork_jobs` has a live job, or (lane splits) one whose end is unconfirmed since the Kit's last reset (`held`, `process.rs` `holds_slot`); `remove` checks and claims (takes the record) in one step under the engine lock; `expire_leases` claims expired kept records under that lock and skips uncertain or live lanes; `join` moves to `joined` in the same lock and clears `kept_unix_ms`; `split apply` (`split_cli.rs` `apply`) is a join with `keep`, so its patch is never removed under it |
| Workspace: Create from Ckpt (CheckpointImmutable) | `marsh-daemon/src/checkpoint.rs` (snapshot into `.marsh/checkpoints/<id>`); `split.rs` `plan`/`setup` (`Source::Checkpoint`: `clone_to` of `tree`, `split_fs::saved_git`); `split_confinement.rs` refuses a job cwd under `.marsh` outside a fork; `checkpoint.rs` `Checkpoints::lease` (`CheckpointLease` held in `Source::Checkpoint` until the base is cloned): retention skips and `rm` refuses a leased checkpoint (unit test `a_leased_checkpoint_outlives_retention_until_released`). `checkpoint.rs` `Checkpoints::create_lane` and `split.rs` `checkpoint_lane` (a checkpoint of a lane's copy, `checkpoint --lane`) are the same `Ckpt` with another source tree: the model has no checkpoint-creating action, the checkpoint is never written or mounted, and the request is creator/host-only on the relay or host socket and absent from the job capability socket, so no invariant or negative control changes (rust caller: `workspace_cli::a_lane_checkpoint_starts_the_next_split_from_the_lanes_state_for_its_creator_only`; real caller: `lanes_uat.py`). Not modelled, deliberately: the model's Create snapshots its source in one step, so no retention can interleave with it; the lease is what makes the code's setup (cloning `tree` into `base`) behave as that one step, and retention otherwise only removes a checkpoint no split is using, which no invariant observes (a split's base is its own clone) |
| Process: Spawn (admission, journal parent/root/depth/spawn before submit) | `marsh-daemon/src/process.rs` `admit` (pool `daemon-jobs`, 64 by default, incl. held uncertain slots; depth `tree-depth`, 4; fan-out `job-children`, 16; launch rate (`launch_rate`: the tree's launches in the rolling window plus up to `REFUSAL_BURST` refusals, `tree-launch-rate`; launches are the admission times `ProcessTable::launched_roots` keeps for the window, or the tree's receipts when a restart left more of those, so trimming receipts never hides a launch, as the model's untrimmed `Recent` assumes: `process_tests.rs` `a_raised_launch_rate_is_enforced_past_receipt_trimming`); spawn set), called by `DaemonStore::admit_process` before VM work and again under the store lock in `begin_job_process` (`lib.rs`), which journals `lineage {parent, root, depth, spawn}` before `mark_execution_submitted`; refusals count in `processes.refused` and leave no receipt. `ProcessRun` enters in the `AuthenticationScope::Capability` branch of `lib.rs` `Server::handle` -> `process::serve_run` -> this daemon's own `Execute` (`marsh-backend` `execute`) |
| Process: Spawn rights (Attenuation, SpawnSetAttenuates) | `marsh-backend` `execute`: a child's view is its parent's recorded mounts (`process::child_view`; cwd outside it is refused), bound from the covering grants by `view_job_mounts` (split argv branches, `ProcessLink.branch`, are the only narrowing besides a late `.marsh` read-only); spawn set = parent's ∩ requested (`marsh-contracts/src/process.rs` `narrow_spawn`); relays cannot name a parent (`authorize_request_session` clears `parent_job`) |
| Process: Exit / Cancel / Lose cascade, Reap / GraceExpire | a job's end drops its `CapabilityBridges` (token revoked, connections closed) and the worker's `capability.rs` `Capability` (socket and streams); `process::serve_run` sees the caller stream drop and calls `DaemonStore::cancel_tree` on the child (`process.rs`), which in one step marks the child and every live descendant `cancelling` (admission then refuses their spawns, `child_lineage`), sends each container INT through the `JobControl` the backend registered at launch, and KILLs whatever still runs after 10 s (the one-step `CancelPropagates` cascade; previously one grace per level). A root caller's Ctrl-C (signal frame) or disconnect does the same from `marsh-backend` `forward_multiplexed_input`. A cancelling job ends `cancelled` (`finish_job_as`), exit 130; the backend verifies container deletion; an unconfirmed end holds the slot (`process.rs` `holds_slot`, derived from `cleanup: uncertain` receipts, so it survives restart) until a successful `workers reset` (`release_process_slots`, persisted) |
| Process: Restart | daemon start: capability tokens are in memory (lost); journal load marks unfinished jobs uncertain; no resubmission; the root client reports `job uncertain` (`marsh/src/client.rs`). A restart for an upgrade (`marsh-daemon/src/upgrade.rs`, `Server::hand_over`) is one the model already allows, taken only when idle: under the lifecycle fence (`begin_lifecycle_request`: no request, job, shell, Kit start or background pass, whose admissions `begin_background_request` refuses), with the background workers joined before the successor is even started (`serve_until`), the successor kept (its child handle) only when ready and still alive, checked again right before the release (`marshd`; a dead one: serve on, workers restarted, failure reported), and the fence held until after the endpoint (`marshd` drops the server, then the `HandoverFence`, then the `Successor`); the successor binds only after that (`handover_ready`) |
| Process: EntryLocal / EntryDispatch / AgentSelfLocal (link decision) | worker/runtime set `MARSH_ENTRY=1`, `MARSH_JOB`, and `PATH=/run/marsh/bin:<image PATH>` for every job (no `bash`/`sh` link and no `SHELL` override: shells are the image's own, `processes.md` s15) (`marsh-runtime` `job_environment`); stubs `#!/run/marsh/marsh --link=NAME` (`marsh-worker/src/capability.rs` `publish`); `marsh/src/job.rs` `link` -> `marsh-contracts` `process::link_decision` (own name + marker + `getppid()` = 1 -> clear marker, exec the image binary by absolute path with PATH intact; another image-provided name -> exec locally; else `ProcessRun`) |
| Pins: CreateOwn | `marsh-services` `policy/create.rs` (`rewrite_bind`, `rewrite_mount`: a bind source becomes a pin path) -> `policy/endpoint.rs` `Scope::pin_bind` (`PinManager::pin`, then `Ledger::add_pin`; a ledger failure unmounts the pin and refuses) -> `record_container` (`Ledger::add_container` with the pin ids; a ledger failure deletes the engine container and frees the pin) |
| Pins: CreateInherit (inheritOff) | `policy/create.rs` `host_config`, the `VolumesFrom` arm (`PinGuard::inherited` from `Env::container_pins`) -> `Post::ContainerCreate { inherited }` -> `record_container` stores own and inherited ids as the lease. Break mode `pin-inherit-off`; unit witness `volumes_from_pins_are_inherited_by_the_new_container`; `e3/pins.sh` (`volumes-from` rows) |
| Pins: Start / Blocked (restoreFailOpen) | `policy/admit.rs` (`ContainerStart` / `ContainerRestart` refused through `Env::container_blocked` -> `Ledger::container_has_failed_pin`); `pins.rs` `PinManager::poison` (dangling symlink `/marsh-pin-not-restored`). Break mode `restore-fail-open`; `e3/pins.sh` (`failed restore` rows) |
| Pins: Remove (releaseWhileHeld, leak) | `policy/endpoint.rs` `post_actions` (`ContainerDelete`) -> `Ledger::remove_container` returns the pins no other container holds, with their records -> `Services::unpin_freed` -> `PinManager::unpin` (`remove_any`: directory, file or poison symlink). The unit witness `pins_are_released_with_the_last_container` and `e3/pins.sh` (file pin, `volumes-from`) cover the release rule; break modes `bind-pin-leak` and `pin-file-leak` cover `leak` |
| Pins: SourceGone | environment (the project folder's owner); `pins.rs` `restore` resolves beneath the reopened project root and fails closed |
| Pins: EngineRestart / Restore | `marsh-services` `engine.rs` `restore_pins` (engine-up, before `dockerd`) -> `pins::restore_all` (`restore_all_in`: `PinManager::restore` per pin, `poison` and `Ledger::set_pin_failed` for the others, `reserve_socket_paths`); an `Err` fails the boot. The relay agent calls `Services::restore_sockets` once it listens (S2c) |
| Process: Spawn no-home Kits (NoHomeKitHome) | `marsh-contracts/src/kit_assets.rs` `KitAgent::from_descriptor` reads `org.marsh/home@1` (`no_home`; any set component declaring `none` wins); `marsh-backend` `kit_wants_no_home` (fails closed) is applied in `execute` to root and child views alike (drops the home `PublicMount` and its `AdmittedHostGrant`, refuses a cwd that is the home or inside a dot-directory of it, or a project that is the home or a dot-directory (`no_home_view_admitted`), keeps a project under the home mounted alone, `HOME`=`MARSH_SELECTED_HOME`=`/tmp`) and in `prepare_for_shell` (`--load` session pins). Real caller: the evidence in the change that added it (`marsh-tools` from a Pi job) |
| Process: Spawn Kit VM cap (KitVMBound) | `process::kit_vm_cap` (from `child_lineage`, split branches included): distinct `kit_ref` among the tree's live receipts, at most `tree_kit_vms` (8 by default; `tree-kit-vms`), checked in the pre-check and in `begin_job_process` |
| Process: SetLimits / Lim (AdmittedUnderLimits; control `staleLimits`) | every admission check reads `State::limits` (`marsh-daemon/src/lib.rs`): the daemon's launch-environment snapshot (`JobDefaults::from_environment`) with the scope's `config.json` `limits` applied (`limits.rs` `ConfigCache`, re-read when the file changes; written by `marsh config limit`, `marsh/src/limits_cli.rs`). A change neither ends nor reassigns a running job; it only refuses the next launch over it |
| Process: Spawn spawn-set narrowing | `process::admit` (`child_lineage`): `MARSH_SPAWN` / `marsh run --spawn` / `--no-spawn` narrow admission. The capability socket remains available for subtree inspection; disallowed spawn requests are refused. |

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
| `NoStaleApply` | `noBaseCheck` | Apply a detached return over a path changed here since the run started |
| `DetachedSurvives` | `restartDetached` | Make a detached run uncertain at restart while its sandbox still exists |
| `DetachedBudget` | `noDetachCap` | Detach beyond the separate detached cap |

Detached runs ([design](../design/cloud-detach.md)): `Detach` moves a started
root crossing with no children out of the shared slot pool into the detached
cap (`DCAP`) and revokes its crossing capability. `Lose` and `Restart` leave it
in place; `Collect` rebinds it to the current generation (read-only, never a
second `Start`); `BaseCheck` lets `Apply` proceed only if every touched path
still has its base contents at the destination, else the return is `kept`
(a patch) and `SettleKept` frees the slot once the sandbox is gone.
`Vanish`/`DetachedLost` model a sandbox removed elsewhere: lost only after
its absence is observed. `DetachedApart` (positive configs) checks a detached
job holds no shared slot, is a root, and has no children.
`Placement_detach_pass.cfg` covers two crossings, one restart, and detach.

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
| `Capture` | `cloud_helper::delta` refuses without the status (`StableCapture`); delta from the hashed unpack baseline, changed entries only (`DeltaOnly`). Outside the model: `marsh cloud sync` (`cloud/sync.rs`, helper `sync`/`rebase`) reads a running block's changes so far, a partial return that is never the final capture; it writes only paths whose local entry is still the base (or already the result), then moves the sandbox baseline to what it wrote, and the final capture still waits for the status |
| `Apply` (parent-bound delta, generation, per-path progress) | `cloud_fs::apply_delta` into the root pinned by identity at snapshot; per-entry staged rename; partial progress reported, payload retained; never re-applied (`NoLateReturn`). The generation is the job: a restarted daemon has no in-flight apply. A nested call's delta is applied into its immediate parent's live view (`AgentCalls::apply`, helper `apply`), never the laptop (`ParentBound`); `AgentCalls::settle` cancels and waits for running calls before the parent is captured (`ParentCaptureSettled`) |
| `Lose` / `Restart` / `RetireUncertain` | Unconfirmed end: quarantine, retire, `ExecutionOutcome::Unknown`, no capture (`LossIsUncertain`, `NoReplay`). Restart: `DaemonStore::with_master_token` marks running cloud jobs unknown/uncertain; `marsh workers reset cloud` retires owned sandboxes |
| `DeleteRequest` / `DeleteLands` / `ObserveAbsent` | `CloudSbx::retire` (observed absence, `VerifiedAbsence`), mapped in the cloud ownership extension below |
| `Complete` (return + verified cleanup) | `Settlement::finish` after capture, retire, and apply (`ReturnBeforeComplete`); the client's exit follows that frame |
| `Detach` | `cloud/detach.rs` `start_detached`: durable record and base manifest, `CloudSbx::mark_detached` (detached cap), the at-most-once `run --detached`, `CloudSbx::detach`, `DaemonStore::detach_cloud_job` (`Detached` receipt, out of the pool) |
| `Collect` / `Capture` | `cloud/detach.rs` `collect_job` (`state`, `tail`, `fetch`; the supervisor in `cloud_helper/detached.rs` writes the status and the delta before `done`), retire before write-back (`retire_settled`) |
| `BaseCheck` / `Apply` / `SettleKept` | `cloud/detach.rs` `write_back` (`conflicts` against `JOB.base`; `apply_delta` only when none; otherwise `kept-as-patch`) |
| `Vanish` / `DetachedLost` | `collect_job`: absent from a complete inventory, then `lost` |

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
| `DetachedSurvives` | `restartDetached` | A detached sandbox keeps its state across restart (only removal leaves it) |
| `NoDetachedUse` | `useDetached` | A detached sandbox takes no new submission, only `Control` |
| `DetachedBound` | `noDetachCap` | Detached sandboxes are bounded by their own cap `DCAP` |
| `ChildBound` | `noChildCap` | A detached run's coordinator starts at most `KCAP` agent sandboxes |
| `NoOrphanChild` | `orphanChild` | The laptop deletes a run's listed agent sandboxes before removing the run's sandbox |
| `EndedWithoutChildren` | `doneWithChild` | The supervisor deletes every agent sandbox before writing `done` |

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
| ChildCreate / ChildDelete / RunEnds | Coordinator (`marsh/src/cloud_helper/coordinator.rs`): `Guard::admit` (allowed Kits, cap `MARSH_CLOUD_CHILDREN`), `cloud_call::run_child` (its own `CloudSbx` record in the root-only remote state; retire when the call returns), `Coordinator::settle` before `done`; laptop `delete_children` (`CloudSbx::adopt` then retire) in collect and discard. An agent sandbox's own create is the same `CloudSbx::create`, so `NoCreateReplay` covers it |
| Detach / Control | `mark_detached` (persisted `detached` flag, cap `MAX_DETACHED`), `detach` (`Detached` phase), `open` (Ready + detached -> Detached on restart), `control_detached` (identity re-observed, admitted under the map lock only in `Detached`) |

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
