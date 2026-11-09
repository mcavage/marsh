------------------------- MODULE ServicesSnapshots -------------------------
(* Services snapshots in checkpoints (C12; `marsh-daemon/src/checkpoint.rs`,  *)
(* `marsh-backend/src/services_snapshots.rs`), for ONE project with two      *)
(* workspaces: "b" (the base) and "l" (a lane). What marsh deletes now        *)
(* includes snapshots in the project's services pool, under leases; and what  *)
(* a checkpoint made while the VM is stopped may CLAIM is decided from what   *)
(* marsh recorded at the stop, per workspace. VM naming and who stops the VM  *)
(* are ServicesVm.tla; here the VM is running, stopped or gone.               *)
(*                                                                            *)
(* A checkpoint holds a snapshot of its own ("own"), references the workspace *)
(* last capture ("reuse", `reused_from`), or records a gap ("gap").           *)
(* A snapshot is IN USE while a live checkpoint refers to it, or while it is  *)
(* a workspace's last capture. marsh deletes one in three places, all by that *)
(* rule:                                                                      *)
(*   CkRm(c)     `checkpoints rm` and retention (never a leased record);      *)
(*               with the VM stopped nothing is dropped                       *)
(*   Collect(s)  the orphan collector, VM running only; the project is found  *)
(*               from the OWNED VM, not from checkpoint records               *)
(*   Rm          `marsh services rm`: the pool goes with the VM; checkpoint   *)
(*               records keep their ids (they refer to a pool that is gone)   *)
(*                                                                            *)
(* The stop CUTOVER (DECISIONS 12, fenced): BeginStop freezes the engine, each *)
(* tracked workspace (the base, and the lane while it has a checkpoint) is    *)
(* captured or fails (CaptureAtStop), FinishStop journals each outcome        *)
(* (`proven` / `failed`) and stops; AbortStop thaws if the stop fails. A stop *)
(* marsh did not cause (ExtStop) records nothing (`unrec`). Reuse then        *)
(* decides per workspace: proven -> reference; failed -> a gap; unrec ->      *)
(* reference WITH a warning (or nothing if never captured).                   *)
(* `dirty` is a ghost: writes since the workspace's last capture.             *)
(*                                                                            *)
(* HF2. The cutover's write fence: BeginStop pauses every container           *)
(* (`paused`) and takes the filter's freeze gate on every tracked workspace   *)
(* (`Fenced`, held until FinishStop/AbortStop/Crash: the agent exits with the *)
(* VM). A container that runs writes (`Write`); a client can start or unpause *)
(* one (`Revive`) unless the workspace is fenced. CountStep asks the engine   *)
(* what runs after the captures; a running container, and a failed query,     *)
(* make every outcome a gap (fail closed). The journal is durable: a stop     *)
(* outcome is persisted, or the old journal is invalidated by a marker        *)
(* (`unknown`: every checkpoint warns), or the stop is refused (FinishStop is *)
(* not taken); the same for the clearing at Start. `LoseRecord` loses a       *)
(* checkpoint record without the untrack (a publish-to-record crash or a      *)
(* removed directory), and `Recover` (a restart) reconciles the lane capture  *)
(* the journal still protects.                                                *)
EXTENDS Naturals, FiniteSets

CONSTANTS Snaps, Cks, Bug

Ws == {"b", "l"}

Bugs == {"none", "reuseLeaseOff", "rmDropsShared", "rmIgnoresLease", "dropLastCapture",
         "stopFenceOff", "stopPauseOff", "stopBaseOnly", "reuseIgnoresFailure",
         "rootsFromRecordsOnly", "orphanGcOff", "queryFailOpen", "staleTrusted",
         "reconcileOff"}
ASSUME Bug \in Bugs

VARIABLES
  vm,       \* "gone" | "running" | "stopped"
  epoch,    \* 0..1: the pool generation (`services rm` starts a new one)
  pool,     \* SUBSET Snaps
  last,     \* [Ws -> Snaps \cup {"none"}]: each workspace's last capture
  dirty,    \* [Ws -> BOOLEAN] ghost: writes since the last capture
  stop,     \* [Ws -> {"unrec", "proven", "failed", "unknown"}]: what marsh journalled at the stop
  cutting,  \* BOOLEAN: a cutover is in progress
  paused,   \* [Ws -> BOOLEAN]: the workspace's containers are paused (nothing runs)
  counted,  \* BOOLEAN: the cutover has asked the engine what runs
  recov,    \* BOOLEAN: a checkpoint record was lost and no restart has reconciled yet
  cut,      \* [Ws -> {"todo", "ok", "bad", "skip"}]: the cutover's progress
  ckLive, ckWs, ckRef, ckKind, ckWarn, ckStale, ckEpoch, ckLease

vars == <<vm, epoch, pool, last, dirty, stop, cutting, paused, counted, recov, cut,
          ckLive, ckWs, ckRef, ckKind, ckWarn, ckStale, ckEpoch, ckLease>>

Nones == [w \in Ws |-> "none"]

TypeOK ==
  /\ vm \in {"gone", "running", "stopped"} /\ epoch \in 0..1
  /\ pool \subseteq Snaps
  /\ last \in [Ws -> Snaps \cup {"none"}]
  /\ dirty \in [Ws -> BOOLEAN]
  /\ stop \in [Ws -> {"unrec", "proven", "failed", "unknown"}]
  /\ cutting \in BOOLEAN /\ paused \in [Ws -> BOOLEAN] /\ counted \in BOOLEAN /\ recov \in BOOLEAN
  /\ cut \in [Ws -> {"todo", "ok", "bad", "skip"}]
  /\ ckLive \subseteq Cks /\ ckLease \subseteq Cks
  /\ ckWs \in [Cks -> Ws]
  /\ ckRef \in [Cks -> Snaps \cup {"none"}]
  /\ ckKind \in [Cks -> {"own", "reuse", "gap", "null"}]
  /\ ckWarn \in [Cks -> BOOLEAN] /\ ckStale \in [Cks -> BOOLEAN]
  /\ ckEpoch \in [Cks -> 0..1]

Init ==
  /\ vm = "gone" /\ epoch = 0 /\ pool = {} /\ last = Nones
  /\ dirty = [w \in Ws |-> FALSE] /\ stop = [w \in Ws |-> "unrec"]
  /\ cutting = FALSE /\ paused = [w \in Ws |-> FALSE] /\ counted = FALSE /\ recov = FALSE
  /\ cut = [w \in Ws |-> "skip"]
  /\ ckLive = {} /\ ckLease = {}
  /\ ckWs = [c \in Cks |-> "b"] /\ ckRef = [c \in Cks |-> "none"]
  /\ ckKind = [c \in Cks |-> "null"] /\ ckWarn = [c \in Cks |-> FALSE]
  /\ ckStale = [c \in Cks |-> FALSE] /\ ckEpoch = [c \in Cks |-> 0]

Lasts == {last[w] : w \in Ws} \ {"none"}

\* Ids no pool holds, no checkpoint refers to, and no workspace calls its last.
FreeIds == {s \in Snaps : s \notin pool /\ s \notin Lasts /\ \A d \in ckLive : ckRef[d] # s}

\* The checkpoints that count as referring to s when marsh decides to delete.
\* The break (`reuse-lease-off`) leaves out the ones that reuse it.
Refs(s) == {d \in ckLive : ckRef[d] = s /\ (Bug # "reuseLeaseOff" \/ ckKind[d] # "reuse")}

\* A lane is tracked (the cutover captures it) while it has a checkpoint.
Tracked(w) == w = "b" \/ \E d \in ckLive : ckWs[d] = w

KeepCk == UNCHANGED <<ckLive, ckWs, ckRef, ckKind, ckWarn, ckStale, ckEpoch, ckLease>>
KeepCut == UNCHANGED <<cutting, paused, counted, cut>>
KeepRecov == UNCHANGED recov

\* The VM starts: the old stop outcome is cleared durably (`unrec`), or, when only
\* the marker could be written, every entry is `unknown`; under the break a failed
\* clearing leaves the old file trusted (`stale`: the old outcome stays).
Start ==
  /\ vm # "running"
  /\ vm' = "running"
  /\ \/ stop' = [w \in Ws |-> "unrec"]
     \/ stop' = IF Bug = "staleTrusted" THEN stop ELSE [w \in Ws |-> "unknown"]
  /\ paused' = [w \in Ws |-> FALSE]
  /\ UNCHANGED <<epoch, pool, last, dirty, cutting, counted, cut>> /\ KeepCk /\ KeepRecov

\* A container that runs writes.
Write(w) ==
  /\ vm = "running" /\ ~paused[w] /\ ~dirty[w]
  /\ dirty' = [dirty EXCEPT ![w] = TRUE]
  /\ UNCHANGED <<vm, epoch, pool, last, stop>> /\ KeepCut /\ KeepCk /\ KeepRecov

\* The write fence: a tracked workspace's mutating requests are refused from the
\* cutover's first step until the VM stops (the agent holds the filter's freeze gate).
\* The control `queryFailOpen` also runs without it, so the count is what stands between
\* a revived container and a `proven` outcome.
Fenced(w) == cutting /\ Bug \notin {"stopFenceOff", "queryFailOpen"} /\ cut[w] # "skip"

\* A client starts or unpauses a container through the filtered endpoint.
Revive(w) ==
  /\ vm = "running" /\ paused[w] /\ ~Fenced(w)
  /\ paused' = [paused EXCEPT ![w] = FALSE]
  /\ UNCHANGED <<vm, epoch, pool, last, dirty, stop, cutting, counted, cut>> /\ KeepCk /\ KeepRecov

\* A checkpoint capture with the VM running and no cutover (the capture gate).
Capture(c, s, w) ==
  /\ vm = "running" /\ ~cutting
  /\ c \in Cks \ ckLive /\ s \in FreeIds
  /\ pool' = pool \cup {s} /\ last' = [last EXCEPT ![w] = s]
  /\ dirty' = [dirty EXCEPT ![w] = FALSE]
  /\ ckLive' = ckLive \cup {c} /\ ckWs' = [ckWs EXCEPT ![c] = w]
  /\ ckRef' = [ckRef EXCEPT ![c] = s] /\ ckKind' = [ckKind EXCEPT ![c] = "own"]
  /\ ckWarn' = [ckWarn EXCEPT ![c] = FALSE] /\ ckStale' = [ckStale EXCEPT ![c] = FALSE]
  /\ ckEpoch' = [ckEpoch EXCEPT ![c] = epoch]
  /\ UNCHANGED <<vm, epoch, stop, ckLease>> /\ KeepCut /\ KeepRecov

\* The cutover begins: every container is paused (the break `stopPauseOff` leaves them
\* running and takes no count) and every tracked workspace is fenced (`Fenced`).
BeginStop ==
  /\ vm = "running" /\ ~cutting
  /\ cutting' = TRUE /\ counted' = FALSE
  /\ paused' = IF Bug = "stopPauseOff" THEN paused ELSE [w \in Ws |-> TRUE]
  /\ cut' = [w \in Ws |-> IF Tracked(w) THEN "todo" ELSE "skip"]
  /\ UNCHANGED <<vm, epoch, pool, last, dirty, stop>> /\ KeepCk /\ KeepRecov

\* One tracked workspace is captured at the stop, or its capture fails (a fault,
\* the reserve, an overlap that stayed busy). The break records every lane as
\* captured without capturing it.
CaptureAtStop(w) ==
  /\ cutting /\ cut[w] = "todo"
  /\ \/ /\ Bug = "stopBaseOnly" /\ w = "l"
        /\ cut' = [cut EXCEPT ![w] = "ok"]
        /\ UNCHANGED <<pool, last, dirty>>
     \/ /\ \E s \in FreeIds :
              /\ pool' = pool \cup {s} /\ last' = [last EXCEPT ![w] = s]
              /\ dirty' = [dirty EXCEPT ![w] = FALSE]
        /\ cut' = [cut EXCEPT ![w] = "ok"]
     \/ /\ cut' = [cut EXCEPT ![w] = "bad"]
        /\ UNCHANGED <<pool, last, dirty>>
  /\ UNCHANGED <<vm, epoch, stop, cutting, paused, counted>> /\ KeepCk /\ KeepRecov

\* After the captures the engine is asked what runs. A running container, and a
\* query that fails, make every tracked outcome a gap (fail closed); under
\* `queryFailOpen` a failed query reads as "nothing runs". Under `stopPauseOff`
\* nothing is counted.
\* A lane whose last checkpoint was removed while the cutover ran is untracked
\* again before the outcomes are journalled (its capture is no longer protected).
Untracked == IF \E d \in ckLive : ckWs[d] = "l" THEN last ELSE [last EXCEPT !["l"] = "none"]

AllBad == [w \in Ws |-> IF cut[w] = "skip" THEN "skip" ELSE "bad"]

CountStep ==
  /\ cutting /\ ~counted /\ \A w \in Ws : cut[w] # "todo"
  /\ counted' = TRUE
  /\ \/ cut' = IF Bug # "stopPauseOff" /\ (\E w \in Ws : ~paused[w]) THEN AllBad ELSE cut
     \/ cut' = IF Bug \in {"queryFailOpen", "stopPauseOff"} THEN cut ELSE AllBad
  /\ UNCHANGED <<vm, epoch, pool, last, dirty, stop, cutting, paused>> /\ KeepCk /\ KeepRecov

\* Every workspace is settled and counted: journal the outcomes, then stop. The
\* journal is persisted, or only invalidated by the marker (every entry `unknown`);
\* when neither is possible the stop is refused (this step is not taken).
FinishStop ==
  /\ cutting /\ counted
  /\ vm' = "stopped" /\ cutting' = FALSE /\ counted' = FALSE
  /\ paused' = [w \in Ws |-> FALSE]
  /\ \/ stop' = [w \in Ws |-> IF cut[w] = "ok" THEN "proven" ELSE "failed"]
     \/ stop' = [w \in Ws |-> "unknown"]
  /\ cut' = [w \in Ws |-> "skip"]
  /\ last' = Untracked
  /\ UNCHANGED <<epoch, pool, dirty>> /\ KeepCk /\ KeepRecov

\* The stop fails or is refused: thaw and release the fence, and the VM runs on.
AbortStop ==
  /\ cutting
  /\ cutting' = FALSE /\ counted' = FALSE /\ paused' = [w \in Ws |-> FALSE]
  /\ cut' = [w \in Ws |-> "skip"]
  /\ last' = Untracked
  /\ UNCHANGED <<vm, epoch, pool, dirty, stop>> /\ KeepCk /\ KeepRecov

\* A stop marsh did not cause (an external `sbx stop`, G13): nothing is recorded,
\* so the journal is what the last start left (`unrec`, `unknown`, or under
\* `staleTrusted` an old outcome).
ExtStop ==
  /\ vm = "running" /\ ~cutting
  /\ vm' = "stopped" /\ paused' = [w \in Ws |-> FALSE]
  /\ UNCHANGED <<epoch, pool, last, dirty, stop, cutting, counted, cut>> /\ KeepCk /\ KeepRecov

\* A checkpoint of workspace w with the VM stopped. proven: reference the last
\* capture; failed: a gap (the break reuses the stale snapshot as if current);
\* unrec: reference it with a warning, or nothing if never captured; unknown (the
\* journal was lost): the workspace's latest record, with a warning. `ckStale`
\* records whether the referenced snapshot lacks writes the workspace had.
Reuse(c, w) ==
  /\ vm = "stopped"
  /\ c \in Cks \ ckLive
  /\ LET src == last[w]
         kind == IF stop[w] = "proven" THEN (IF src = "none" THEN "gap" ELSE "reuse")
                 ELSE IF stop[w] = "failed"
                      THEN (IF Bug = "reuseIgnoresFailure" /\ src # "none" THEN "reuse" ELSE "gap")
                      ELSE (IF src = "none" THEN "null" ELSE "reuse")
         warn == (stop[w] \in {"unrec", "unknown"} /\ kind = "reuse")
     IN /\ ckLive' = ckLive \cup {c} /\ ckWs' = [ckWs EXCEPT ![c] = w]
        /\ ckRef' = [ckRef EXCEPT ![c] = IF kind = "reuse" THEN src ELSE "none"]
        /\ ckKind' = [ckKind EXCEPT ![c] = kind]
        /\ ckWarn' = [ckWarn EXCEPT ![c] = warn]
        /\ ckStale' = [ckStale EXCEPT ![c] = kind = "reuse" /\ dirty[w]]
        /\ ckEpoch' = [ckEpoch EXCEPT ![c] = epoch]
  /\ UNCHANGED <<vm, epoch, pool, last, dirty, stop, ckLease>> /\ KeepCut /\ KeepRecov

\* A restore holds a checkpoint while it reads it.
Lease(c) ==
  /\ c \in ckLive \ ckLease
  /\ ckLease' = ckLease \cup {c}
  /\ UNCHANGED <<vm, epoch, pool, last, dirty, stop>> /\ KeepCut /\ KeepRecov
  /\ UNCHANGED <<ckLive, ckWs, ckRef, ckKind, ckWarn, ckStale, ckEpoch>>

Unlease(c) ==
  /\ c \in ckLease
  /\ ckLease' = ckLease \ {c}
  /\ UNCHANGED <<vm, epoch, pool, last, dirty, stop>> /\ KeepCut /\ KeepRecov
  /\ UNCHANGED <<ckLive, ckWs, ckRef, ckKind, ckWarn, ckStale, ckEpoch>>

\* `checkpoints rm` or retention. A leased checkpoint stays (the break removes
\* it). When it was the lane's last checkpoint the lane is untracked and its
\* last capture no longer protected. The snapshot goes with it unless something
\* else uses it (the break drops it regardless); VM not running: nothing drops.
CkRm(c) ==
  /\ c \in ckLive
  /\ c \notin ckLease \/ Bug = "rmIgnoresLease"
  /\ LET w == ckWs[c]
         s == ckRef[c]
         laneGone == w = "l" /\ \A d \in ckLive \ {c} : ckWs[d] # "l"
         last1 == IF laneGone THEN [last EXCEPT ![w] = "none"] ELSE last
         used == \/ s \in {last1[x] : x \in Ws}
                 \/ Refs(s) \ {c} # {}
         drop == /\ s # "none" /\ vm = "running" /\ ckEpoch[c] = epoch
                 /\ (~used \/ Bug = "rmDropsShared")
     IN /\ pool' = IF drop THEN pool \ {s} ELSE pool
        /\ last' = last1
  /\ ckLive' = ckLive \ {c}
  /\ ckLease' = IF Bug = "rmIgnoresLease" THEN ckLease ELSE ckLease \ {c}
  /\ UNCHANGED <<vm, epoch, dirty, stop>> /\ KeepCut /\ KeepRecov
  /\ UNCHANGED <<ckWs, ckRef, ckKind, ckWarn, ckStale, ckEpoch>>

\* The project is discoverable by the collector: from the owned VM (always),
\* or, under the break, only from a checkpoint record.
Discoverable == Bug # "rootsFromRecordsOnly" \/ ckLive # {}

\* The orphan collector, with the VM running: a snapshot nothing uses.
Collect(s) ==
  /\ Bug # "orphanGcOff"
  /\ Discoverable
  /\ vm = "running" /\ s \in pool
  /\ Refs(s) = {}
  /\ s \notin Lasts \/ Bug = "dropLastCapture"
  /\ pool' = pool \ {s}
  /\ UNCHANGED <<vm, epoch, last, dirty, stop>> /\ KeepCut /\ KeepCk /\ KeepRecov

\* `marsh services rm`: the pool goes with the VM, a new pool generation starts;
\* the checkpoint records keep their ids.
Rm ==
  /\ vm # "gone" /\ epoch = 0 /\ ~cutting
  /\ vm' = "gone" /\ epoch' = 1 /\ pool' = {} /\ last' = Nones
  /\ dirty' = [w \in Ws |-> FALSE] /\ stop' = [w \in Ws |-> "unrec"]
  /\ KeepCut /\ KeepCk /\ KeepRecov

\* A checkpoint record is lost without the untrack (the daemon died between
\* publishing a snapshot and recording its checkpoint, or its directory was
\* removed): the journal still names the lane's capture as protected.
LoseRecord(c) ==
  /\ c \in ckLive /\ c \notin ckLease
  /\ ckLive' = ckLive \ {c}
  /\ recov' = TRUE
  /\ UNCHANGED <<vm, epoch, pool, last, dirty, stop>> /\ KeepCut
  /\ UNCHANGED <<ckLease, ckWs, ckRef, ckKind, ckWarn, ckStale, ckEpoch>>

\* A daemon restart reconciles the journal against the records that survived: a
\* lane that has no live checkpoint is untracked, so its capture is no longer
\* protected (the collector reclaims it unless something still names it).
Recover ==
  /\ recov
  /\ recov' = FALSE
  /\ last' = IF Bug = "reconcileOff" \/ \E d \in ckLive : ckWs[d] = "l" THEN last
              ELSE [last EXCEPT !["l"] = "none"]
  /\ UNCHANGED <<vm, epoch, pool, dirty, stop>> /\ KeepCut /\ KeepCk

Next ==
  \/ Start \/ BeginStop \/ CountStep \/ FinishStop \/ AbortStop \/ ExtStop \/ Rm \/ Recover
  \/ \E w \in Ws : Write(w) \/ Revive(w) \/ CaptureAtStop(w)
  \/ \E c \in Cks : Lease(c) \/ Unlease(c) \/ CkRm(c) \/ LoseRecord(c)
  \/ \E c \in Cks, w \in Ws : Reuse(c, w)
  \/ \E c \in Cks, s \in Snaps, w \in Ws : Capture(c, s, w)
  \/ \E s \in Snaps : Collect(s)

Spec == Init /\ [][Next]_vars /\ (\A s \in Snaps : WF_vars(Collect(s)))

\* marsh never deletes a snapshot a live checkpoint holds or reuses, leased or
\* not, while its pool exists (`in_use` decides in `release_snapshots` and
\* `collect_orphans`).
RefsKept ==
  \A c \in ckLive : (ckRef[c] # "none" /\ ckEpoch[c] = epoch) => ckRef[c] \in pool

\* A restore's lease outlives retention and `rm`.
LeaseHeld == ckLease \subseteq ckLive

\* A workspace's last capture, the reuse source, is never dropped. (The code
\* also forgets one that is dropped, a net the model leaves out, so a drop of
\* it shows as a reference to nothing.)
LastKept == \A w \in Ws : last[w] # "none" => last[w] \in pool

\* What marsh journalled as proven at the stop really is the workspace's last
\* write: nothing wrote between the capture and the stop (the fence), and the
\* workspace really was captured (not recorded without a capture).
ProvenIsCurrent == \A w \in Ws : stop[w] = "proven" => ~dirty[w]

\* After a restart has reconciled, a lane's last capture is protected only while
\* the lane has a live checkpoint (no capture is protected forever by a journal
\* entry whose record was lost). A cutover in progress may hold a capture of a lane
\* whose last checkpoint was just removed; it is untracked again when the cutover ends.
LaneLastTracked == (~recov /\ ~cutting /\ last["l"] # "none") => \E d \in ckLive : ckWs[d] = "l"

\* A checkpoint never silently claims data that lacks writes: a reference to a
\* snapshot older than the workspace's last write carries a warning gap.
NoSilentStale == \A c \in ckLive : (ckKind[c] = "reuse" /\ ckStale[c]) => ckWarn[c]

\* An unused snapshot in a running pool is collected, unless a use appears or
\* the VM stops first.
OrphansCollected ==
  \A s \in Snaps :
    (s \in pool /\ vm = "running" /\ Refs(s) = {} /\ s \notin Lasts)
      ~> (s \notin pool \/ vm # "running" \/ Refs(s) # {} \/ s \in Lasts)
=============================================================================
