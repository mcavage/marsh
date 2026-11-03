---------------------------- MODULE KitSwitch ----------------------------
(* One Kit command name and the warm Kit VMs of the Kit images it has     *)
(* named. A Kit change (an automatic update, `marsh kit install` /        *)
(* `uninstall`, the move to the Kits a newer marsh ships) moves the name  *)
(* at once, whatever runs: new jobs start on the VM of the Kit the name   *)
(* now names; a VM it moved away from is retired at once only if it is    *)
(* provably idle, else it drains and is retired once it is. Garbage       *)
(* collection removes a VM only while stock lists it stopped and no job   *)
(* recorded on it is live or uncertain, checked again under the VM's      *)
(* fence (GcObserve, then GcRetire). The daemon may restart at any time:  *)
(* it forgets which VMs it served and what drains, its live job records   *)
(* become uncertain, and the containers keep running (orphans). The       *)
(* environment may stop a VM, which ends its containers. Container and    *)
(* transport detail is Worker.tla; VM ownership is Ownership.tla.         *)
(* `marsh workers stop` stops a provably idle VM and keeps it: its claim  *)
(* (UserStopClaim, the same as an automatic retirement's) holds off new   *)
(* jobs while `sbx stop` runs (UserStopSubmit). Stock lands it at once or *)
(* later, or fails it (StopLands, StopFails). The stop is settled only    *)
(* when stock reported it and lists the VM stopped (UserStopReturn);      *)
(* otherwise the VM stays fenced (quarantined) and is retired only once   *)
(* the stop can no longer land (FenceRetire, after STOP_SETTLE: the       *)
(* timing assumption is that a stop not reported by then never lands). A  *)
(* daemon restart forgets a pending stop; its late landing is then        *)
(* indistinguishable from StopVM. The next job starts the same VM         *)
(* (Start); one that does not come back healthy, or whose new worker      *)
(* never reports Ready, is replaced (Replace) only while nothing recorded *)
(* on it runs.                                                           *)
EXTENDS Naturals, FiniteSets

CONSTANTS Jobs, Kits, MaxChanges, MaxRestarts, MaxStops, Bug

None == "none"
Bugs == {"none", "retireBusy", "admitDraining", "trustTable", "gcRunning", "gcStale",
         "stopBusy", "admitStopping", "replaceRunning", "cancelOnError", "noSettleWait"}
ASSUME Bug \in Bugs

VARIABLES
  cur,      \* the Kit the name runs, or None (uninstalled)
  vm,       \* [Kits -> "absent" | "running" | "stopped"]
  served,   \* [Kits -> BOOLEAN] this daemon run registered that VM's worker
  draining, \* Kits whose VM the name moved away from while it was in use
  job,      \* [Jobs -> "idle" | "running" | "done"] the container, physically
  on,       \* [Jobs -> Kits] the VM a job runs on
  rec,      \* [Jobs -> "none" | "live" | "uncertain" | "ended"] its journaled receipt
  cand,     \* Kits a garbage collection pass observed as removable
  stopping, \* `marsh workers stop`: Kits claimed, submitted (`sbx stop` running),
            \* reported (stock reported it done), inflight (stock may still land it),
            \* fenced (quarantined: a stop that did not settle)
  changes, restarts, stops,
  killed,   \* ghost: a VM was removed while a container ran in it
  stale,    \* ghost: a job started on a VM the name had moved away from
  ended     \* ghost: `marsh workers stop` ended a running container

vars == <<cur, vm, served, draining, job, on, rec, cand, stopping, changes, restarts, stops,
          killed, stale, ended>>

K0 == CHOOSE k \in Kits : TRUE
NoStops == [claimed |-> {}, submitted |-> {}, reported |-> {}, inflight |-> {}, fenced |-> {}]
Stopping == stopping.claimed \cup stopping.fenced  \* admits no job

Init ==
  /\ cur = K0
  /\ vm = [k \in Kits |-> "absent"]
  /\ served = [k \in Kits |-> FALSE]
  /\ draining = {}
  /\ job = [j \in Jobs |-> "idle"]
  /\ on = [j \in Jobs |-> K0]
  /\ rec = [j \in Jobs |-> "none"]
  /\ cand = {}
  /\ stopping = NoStops
  /\ changes = 0 /\ restarts = 0 /\ stops = 0
  /\ killed = FALSE
  /\ stale = FALSE
  /\ ended = FALSE

PhysBusy(k) == \E j \in Jobs : job[j] = "running" /\ on[j] = k
\* What the daemon's records say: a live or cleanup-uncertain job on k.
RecBusy(k) == \E j \in Jobs : rec[j] \in {"live", "uncertain"} /\ on[j] = k
RecLive(k) == \E j \in Jobs : rec[j] = "live" /\ on[j] = k

\* `automatic_claim`: no recorded live/uncertain job, and either this run
\* served the VM (its records are complete) or a check of the VM itself
\* (stopped, or `docker ps` empty) shows no container.
CanRetire(k) ==
  IF Bug = "trustTable" THEN ~RecLive(k)
  ELSE ~RecBusy(k) /\ (served[k] \/ ~PhysBusy(k)) /\ k \notin stopping.fenced

Remove(k) ==
  /\ vm' = [vm EXCEPT ![k] = "absent"]
  /\ served' = [served EXCEPT ![k] = FALSE]
  /\ killed' = (killed \/ PhysBusy(k))

\* A job of the name starts on its current Kit's VM, starting (resuming) a
\* stopped one or creating it. A VM claimed for a stop admits no job.
Start(j, k) ==
  /\ job[j] = "idle"
  /\ \/ cur # None /\ k = cur
     \/ Bug = "admitDraining" /\ k \in draining /\ k # cur
  /\ k \notin Stopping \/ Bug = "admitStopping"
  /\ job' = [job EXCEPT ![j] = "running"]
  /\ on' = [on EXCEPT ![j] = k]
  /\ rec' = [rec EXCEPT ![j] = "live"]
  /\ vm' = [vm EXCEPT ![k] = "running"]
  /\ served' = [served EXCEPT ![k] = TRUE]
  /\ stale' = (stale \/ k # cur)
  /\ UNCHANGED <<cur, draining, cand, stopping, changes, restarts, stops, killed, ended>>

\* The container ends; an uncertain receipt stays uncertain.
End(j) ==
  /\ job[j] = "running"
  /\ job' = [job EXCEPT ![j] = "done"]
  /\ rec' = [rec EXCEPT ![j] = IF @ = "live" THEN "ended" ELSE @]
  /\ UNCHANGED <<cur, vm, served, draining, on, cand, stopping, changes, restarts, stops,
                 killed, stale, ended>>

\* Environment: stock stops a VM; its containers end, live receipts become
\* uncertain (the transport was lost).
StopVM(k) ==
  /\ stops < MaxStops /\ vm[k] = "running"
  /\ vm' = [vm EXCEPT ![k] = "stopped"]
  /\ job' = [j \in Jobs |-> IF on[j] = k /\ job[j] = "running" THEN "done" ELSE job[j]]
  /\ rec' = [j \in Jobs |-> IF on[j] = k /\ rec[j] = "live" THEN "uncertain" ELSE rec[j]]
  /\ stops' = stops + 1
  /\ UNCHANGED <<cur, served, draining, on, cand, stopping, changes, restarts, killed, stale, ended>>

\* The daemon restarts: memory is lost, containers keep running. A stop it
\* had claimed is forgotten; a `sbx stop` still landing is StopVM.
Restart ==
  /\ restarts < MaxRestarts
  /\ served' = [k \in Kits |-> FALSE]
  /\ draining' = {}
  /\ cand' = {}
  /\ stopping' = NoStops
  /\ rec' = [j \in Jobs |-> IF rec[j] = "live" THEN "uncertain" ELSE rec[j]]
  /\ restarts' = restarts + 1
  /\ UNCHANGED <<cur, vm, job, on, changes, stops, killed, stale, ended>>

\* The VM of a Kit no longer named is retired now when provably idle, else
\* it drains.
Change(k) ==
  LET old == cur IN
  /\ changes < MaxChanges
  /\ k # cur
  /\ cur \notin stopping.claimed   \* both hold the Kit lock
  /\ cur' = k
  /\ changes' = changes + 1
  /\ IF old # None /\ vm[old] # "absent"
       THEN IF CanRetire(old) \/ Bug = "retireBusy"
              THEN /\ Remove(old) /\ UNCHANGED draining
              ELSE /\ draining' = draining \cup {old}
                   /\ UNCHANGED <<vm, served, killed>>
       ELSE UNCHANGED <<vm, served, draining, killed>>
  /\ UNCHANGED <<job, on, rec, cand, stopping, restarts, stops, stale, ended>>

\* `retire_drained`: a draining VM the name names again leaves the set;
\* otherwise it is retired once provably idle.
Reuse(k) ==
  /\ k \in draining /\ k = cur
  /\ draining' = draining \ {k}
  /\ UNCHANGED <<cur, vm, served, job, on, rec, cand, stopping, changes, restarts, stops,
                 killed, stale, ended>>

RetireDrained(k) ==
  /\ k \in draining /\ k # cur
  /\ CanRetire(k) \/ Bug = "retireBusy"
  /\ Remove(k)
  /\ draining' = draining \ {k}
  /\ UNCHANGED <<cur, job, on, rec, cand, stopping, changes, restarts, stops, stale, ended>>

\* Garbage collection: superseded or idle; only a stopped VM with no live
\* or uncertain record, not draining.
GcOk(k) ==
  /\ k \notin draining /\ k \notin Stopping
  /\ IF Bug = "gcRunning" THEN vm[k] # "absent" /\ ~RecLive(k)
     ELSE vm[k] = "stopped" /\ ~RecBusy(k)

GcObserve(k) ==
  /\ GcOk(k) /\ k \notin cand
  /\ cand' = cand \cup {k}
  /\ UNCHANGED <<cur, vm, served, draining, job, on, rec, stopping, changes, restarts, stops,
                 killed, stale, ended>>

\* Under the VM's fence the conditions are checked again (`gcStale` trusts
\* the observation instead).
GcRetire(k) ==
  /\ k \in cand
  /\ cand' = cand \ {k}
  /\ IF GcOk(k) \/ (Bug = "gcStale" /\ vm[k] # "absent")
       THEN Remove(k)
       ELSE UNCHANGED <<vm, served, killed>>
  /\ UNCHANGED <<cur, draining, job, on, rec, stopping, changes, restarts, stops, stale, ended>>

\* `marsh workers stop`: `automatic_claim` under the Kit lock (CanRetire;
\* `stopBusy` skips it), which makes the worker Repairing so no job is
\* admitted, then `sbx stop` under the VM's transition fence.
UserStopClaim(k) ==
  /\ k = cur /\ vm[k] = "running" /\ k \notin Stopping /\ k \notin draining
  /\ CanRetire(k) \/ Bug = "stopBusy"
  /\ stopping' = [stopping EXCEPT !.claimed = @ \cup {k}]
  /\ UNCHANGED <<cur, vm, served, draining, job, on, rec, cand, changes, restarts, stops,
                 killed, stale, ended>>

UserStopSubmit(k) ==
  /\ k \in stopping.claimed /\ k \notin stopping.submitted /\ k \notin stopping.inflight
  /\ stopping' = [stopping EXCEPT !.submitted = @ \cup {k}, !.inflight = @ \cup {k}]
  /\ UNCHANGED <<cur, vm, served, draining, job, on, rec, cand, changes, restarts, stops,
                 killed, stale, ended>>

\* Stock lands a submitted stop, now or late; it is reported only while
\* the daemon still waits for it.
StopLands(k) ==
  /\ k \in stopping.inflight
  /\ stopping' = [stopping EXCEPT !.inflight = @ \ {k},
                     !.reported = IF k \in stopping.submitted THEN @ \cup {k} ELSE @]
  /\ vm' = [vm EXCEPT ![k] = IF @ = "running" THEN "stopped" ELSE @]
  /\ ended' = (ended \/ PhysBusy(k))
  /\ job' = [j \in Jobs |-> IF on[j] = k /\ job[j] = "running" THEN "done" ELSE job[j]]
  /\ rec' = [j \in Jobs |-> IF on[j] = k /\ rec[j] = "live" THEN "uncertain" ELSE rec[j]]
  /\ UNCHANGED <<cur, served, draining, on, cand, changes, restarts, stops, killed, stale>>

StopFails(k) ==
  /\ k \in stopping.inflight
  /\ stopping' = [stopping EXCEPT !.inflight = @ \ {k}]
  /\ UNCHANGED <<cur, vm, served, draining, job, on, rec, cand, changes, restarts, stops,
                 killed, stale, ended>>

\* `stop_idle_kit_vm` returns (at once after the report, or on its timeout
\* or failure). Settled: the stopped VM is no registered worker. Otherwise
\* the VM is fenced; `cancelOnError` reopens admission instead.
UserStopReturn(k) ==
  /\ k \in stopping.submitted
  /\ LET settled == k \in stopping.reported /\ vm[k] = "stopped" IN
     /\ stopping' = [stopping EXCEPT !.claimed = @ \ {k}, !.submitted = @ \ {k},
                        !.reported = @ \ {k},
                        !.fenced = IF settled \/ Bug = "cancelOnError" THEN @ ELSE @ \cup {k}]
     /\ served' = IF settled THEN [served EXCEPT ![k] = FALSE] ELSE served
  /\ UNCHANGED <<cur, vm, draining, job, on, rec, cand, changes, restarts, stops,
                 killed, stale, ended>>

\* Quarantine recovery retires a fenced VM once the stop can no longer land
\* (`verify_no_running_containers` waits out STOP_SETTLE; `noSettleWait`
\* does not); the next job creates a new one.
FenceRetire(k) ==
  /\ k \in stopping.fenced
  /\ k \notin stopping.inflight \/ Bug = "noSettleWait"
  /\ ~RecBusy(k)
  /\ stopping' = [stopping EXCEPT !.fenced = @ \ {k}]
  /\ IF vm[k] # "absent" THEN Remove(k) ELSE UNCHANGED <<vm, served, killed>>
  /\ UNCHANGED <<cur, draining, job, on, rec, cand, changes, restarts, stops, stale, ended>>

\* A VM whose start failed is removed (the next Start creates it again):
\* one observed stopped before the start with no live or uncertain record,
\* or (its new worker never reported Ready) a running one CanRetire allows
\* (`replace_unhealthy`). Never a fenced one. `replaceRunning` trusts only
\* the live records, whatever stock lists.
Replace(k) ==
  /\ k = cur /\ k \notin draining /\ k \notin Stopping
  /\ IF Bug = "replaceRunning" THEN vm[k] # "absent" /\ ~RecLive(k)
     ELSE \/ vm[k] = "stopped" /\ ~RecBusy(k)
          \/ vm[k] = "running" /\ CanRetire(k)
  /\ Remove(k)
  /\ UNCHANGED <<cur, draining, job, on, rec, cand, stopping, changes, restarts, stops,
                 stale, ended>>

Next ==
  \/ \E j \in Jobs, k \in Kits : Start(j, k)
  \/ \E j \in Jobs : End(j)
  \/ \E k \in Kits \cup {None} : Change(k)
  \/ \E k \in Kits : Reuse(k) \/ RetireDrained(k) \/ StopVM(k) \/ GcObserve(k) \/ GcRetire(k)
  \/ \E k \in Kits : UserStopClaim(k) \/ UserStopSubmit(k) \/ StopLands(k) \/ StopFails(k)
                     \/ UserStopReturn(k) \/ FenceRetire(k) \/ Replace(k)
  \/ Restart

Spec == /\ Init /\ [][Next]_vars
        /\ \A j \in Jobs : WF_vars(End(j))
        /\ \A k \in Kits : WF_vars(Reuse(k)) /\ WF_vars(RetireDrained(k))
        \* Stock resolves a submitted stop; quarantine recovery runs each minute.
        /\ \A k \in Kits : WF_vars(StopLands(k) \/ StopFails(k)) /\ WF_vars(FenceRetire(k))

TypeOK ==
  /\ cur \in Kits \cup {None}
  /\ vm \in [Kits -> {"absent", "running", "stopped"}]
  /\ served \in [Kits -> BOOLEAN]
  /\ draining \subseteq Kits /\ cand \subseteq Kits
  /\ stopping \in [{"claimed", "submitted", "reported", "inflight", "fenced"} -> SUBSET Kits]
  /\ job \in [Jobs -> {"idle", "running", "done"}]
  /\ on \in [Jobs -> Kits]
  /\ rec \in [Jobs -> {"none", "live", "uncertain", "ended"}]

(* No Kit change, drain, garbage collection, or replacement of a resumed  *)
(* VM removes a VM a container still runs in, across restarts.             *)
ChangeSparesRunning == ~killed
(* `marsh workers stop` never ends a running container: it stops only a    *)
(* provably idle VM, and no job is admitted to it until its stop is settled *)
(* or can no longer land.                                                   *)
StopSparesRunning == ~ended
(* New jobs start on the Kit the name names now, never a draining one. *)
StartsOnCurrent == ~stale
(* A running container's VM exists and runs. *)
RunningHasVM == \A j \in Jobs : job[j] = "running" => vm[on[j]] = "running"

Uncertain(k) == \E j \in Jobs : rec[j] = "uncertain" /\ on[j] = k
(* A draining VM is retired, serves the name again, or is forgotten by a    *)
(* restart (then garbage collection owns it) once its jobs end; one with an *)
(* uncertain job is left to quarantine recovery (Worker.tla).              *)
DrainEnds == \A k \in Kits :
  (k \in draining /\ ~Uncertain(k)) ~> (k \notin draining \/ Uncertain(k))
=============================================================================
