---------------------------- MODULE KitSwitch ----------------------------
(* One Kit command name and the warm Kit VMs of the Kit images it has     *)
(* named. A Kit change (an automatic update, `marsh kit install` /        *)
(* `uninstall`, the move to the Kits a newer marsh ships) moves the name  *)
(* at once, whatever runs: new jobs start on the VM of the Kit the name   *)
(* now names, and a VM the name moved away from keeps its running jobs    *)
(* and is retired only once none runs there (draining). The name may move *)
(* back to a draining Kit, which then serves again. Container/transport   *)
(* detail is Worker.tla; VM ownership is Ownership.tla.                   *)
EXTENDS Naturals, FiniteSets

CONSTANTS Jobs, Kits, MaxChanges, Bug

None == "none"
Bugs == {"none", "retireBusy", "admitDraining"}
ASSUME Bug \in Bugs

VARIABLES
  cur,      \* the Kit the name runs, or None (uninstalled)
  vm,       \* [Kits -> BOOLEAN] that Kit's VM exists
  draining, \* Kits whose VM the name moved away from while it was in use
  job,      \* [Jobs -> "idle" | "running" | "done"]
  on,       \* [Jobs -> Kits] the VM a job runs on
  changes,  \* Kit changes so far (bound)
  killed,   \* ghost: a retire removed a VM a job was running on
  stale     \* ghost: a job started on a VM the name had moved away from

vars == <<cur, vm, draining, job, on, changes, killed, stale>>

K0 == CHOOSE k \in Kits : TRUE

Init ==
  /\ cur = K0
  /\ vm = [k \in Kits |-> FALSE]
  /\ draining = {}
  /\ job = [j \in Jobs |-> "idle"]
  /\ on = [j \in Jobs |-> K0]
  /\ changes = 0
  /\ killed = FALSE
  /\ stale = FALSE

Busy(k) == \E j \in Jobs : job[j] = "running" /\ on[j] = k

\* A job of the name starts on its current Kit's VM, preparing it if needed.
Start(j, k) ==
  /\ job[j] = "idle"
  /\ \/ cur # None /\ k = cur
     \/ Bug = "admitDraining" /\ k \in draining /\ k # cur
  /\ job' = [job EXCEPT ![j] = "running"]
  /\ on' = [on EXCEPT ![j] = k]
  /\ vm' = [vm EXCEPT ![k] = TRUE]
  /\ stale' = (stale \/ k # cur)
  /\ UNCHANGED <<cur, draining, changes, killed>>

End(j) ==
  /\ job[j] = "running"
  /\ job' = [job EXCEPT ![j] = "done"]
  /\ UNCHANGED <<cur, vm, draining, on, changes, killed, stale>>

\* The VM of a Kit no longer named goes now when idle, else it drains.
Change(k) ==
  LET old == cur IN
  /\ changes < MaxChanges
  /\ k # cur
  /\ cur' = k
  /\ changes' = changes + 1
  /\ IF old # None /\ vm[old]
       THEN IF Busy(old)
              THEN /\ draining' = draining \cup {old}
                   /\ UNCHANGED vm
              ELSE /\ vm' = [vm EXCEPT ![old] = FALSE]
                   /\ UNCHANGED draining
       ELSE UNCHANGED <<vm, draining>>
  /\ UNCHANGED <<job, on, killed, stale>>

\* `retire_drained`: a draining VM the name names again leaves the set;
\* otherwise it is retired once no job runs there.
Reuse(k) ==
  /\ k \in draining /\ k = cur
  /\ draining' = draining \ {k}
  /\ UNCHANGED <<cur, vm, job, on, changes, killed, stale>>

RetireDrained(k) ==
  /\ k \in draining /\ k # cur
  /\ ~Busy(k) \/ Bug = "retireBusy"
  /\ vm' = [vm EXCEPT ![k] = FALSE]
  /\ draining' = draining \ {k}
  /\ killed' = (killed \/ Busy(k))
  /\ UNCHANGED <<cur, job, on, changes, stale>>

Next ==
  \/ \E j \in Jobs, k \in Kits : Start(j, k)
  \/ \E j \in Jobs : End(j)
  \/ \E k \in Kits \cup {None} : Change(k)
  \/ \E k \in Kits : Reuse(k) \/ RetireDrained(k)

Spec == /\ Init /\ [][Next]_vars
        /\ \A j \in Jobs : WF_vars(End(j))
        /\ \A k \in Kits : WF_vars(Reuse(k)) /\ WF_vars(RetireDrained(k))

TypeOK ==
  /\ cur \in Kits \cup {None}
  /\ vm \in [Kits -> BOOLEAN]
  /\ draining \subseteq Kits
  /\ job \in [Jobs -> {"idle", "running", "done"}]
  /\ on \in [Jobs -> Kits]

(* A Kit change never stops running work. *)
ChangeSparesRunning == ~killed
(* New jobs start on the Kit the name names now, never a draining one. *)
StartsOnCurrent == ~stale
(* A running job's VM exists. *)
RunningHasVM == \A j \in Jobs : job[j] = "running" => vm[on[j]]

(* A draining VM is retired, or serves the name again, once its jobs end. *)
DrainEnds == \A k \in Kits : (k \in draining) ~> (k \notin draining)
=============================================================================
