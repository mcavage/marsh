------------------------- MODULE ServicesLanes -------------------------
(* App data in lanes (C3; `marsh-daemon/src/split.rs` `restore_lanes`,        *)
(* `drop_lane_services`, `wait_restore`, `marsh-daemon/src/lane_services.rs`   *)
(* `LaneDrops`, `marsh-services/src/restore.rs` `drop_workspace`), for ONE      *)
(* project's services engine with the base workspace "b" and two lanes. What    *)
(* marsh deletes now includes a lane's services objects (its containers,        *)
(* volumes and networks, and its ledger scope): when its split is closed,       *)
(* cancelled or removed, BY WORKSPACE ONLY, through the agent. The VM is        *)
(* running, stopped or gone (ServicesVm.tla says who stops it).                 *)
(*                                                                              *)
(*   Create(l)    the lane opens; its restore may start (RestoreBegin)              *)
(*   RestoreCheck / RestoreWrite  the restore reads that the lane is open, then *)
(*                the agent creates the fork's objects: two steps, so a close   *)
(*                can come between them unless it waits for the restore         *)
(*   Close(l)     close, cancel or rm: the lane is fenced, the drop is journaled*)
(*                (`pending`); it waits for a running restore                   *)
(*   DropRun(l)   the drop: only the lane's own objects; with the VM stopped it *)
(*                cannot run (it is retried when the VM is back); with the VM   *)
(*                gone there is nothing to drop                                 *)
(* `foreign` is a ghost: some workspace lost objects to a drop that was not its *)
(* own. `tried` records that a drop was attempted and could not run.            *)
EXTENDS Naturals, FiniteSets

CONSTANTS Bug

Lanes == {"l1", "l2"}
Ws == Lanes \cup {"b"}

Bugs == {"none", "dropAllScopes", "dropWithoutWait", "dropOff", "dropNoRetry"}
ASSUME Bug \in Bugs

VARIABLES
  vm,        \* "running" | "stopped" | "gone"
  stops,     \* how many times the VM stopped (bounded)
  lane,      \* [Lanes -> {"none", "open", "closed"}]
  obj,       \* [Ws -> BOOLEAN]: the workspace has objects in the engine
  restoring, \* [Lanes -> {"no", "check", "write"}]: a restore is under way
  pending,   \* [Lanes -> BOOLEAN]: a drop is journaled and not done
  tried,     \* [Lanes -> BOOLEAN]: a drop was attempted and could not run
  foreign    \* BOOLEAN ghost

vars == <<vm, stops, lane, obj, restoring, pending, tried, foreign>>

TypeOK ==
  /\ vm \in {"running", "stopped", "gone"} /\ stops \in 0..2
  /\ lane \in [Lanes -> {"none", "open", "closed"}]
  /\ obj \in [Ws -> BOOLEAN]
  /\ restoring \in [Lanes -> {"no", "check", "write"}]
  /\ pending \in [Lanes -> BOOLEAN] /\ tried \in [Lanes -> BOOLEAN]
  /\ foreign \in BOOLEAN

Init ==
  /\ vm = "running" /\ stops = 0
  /\ lane = [l \in Lanes |-> "none"] /\ obj = [w \in Ws |-> FALSE]
  /\ restoring = [l \in Lanes |-> "no"]
  /\ pending = [l \in Lanes |-> FALSE] /\ tried = [l \in Lanes |-> FALSE]
  /\ foreign = FALSE

\* The base workspace's own objects (a job of the project).
BaseWrite ==
  /\ vm = "running" /\ ~obj["b"]
  /\ obj' = [obj EXCEPT !["b"] = TRUE]
  /\ UNCHANGED <<vm, stops, lane, restoring, pending, tried, foreign>>

Create(l) ==
  /\ lane[l] = "none"
  /\ lane' = [lane EXCEPT ![l] = "open"]
  /\ UNCHANGED <<vm, stops, obj, restoring, pending, tried, foreign>>

\* `split --open --from`: the restore starts for the open lane.
RestoreBegin(l) ==
  /\ lane[l] = "open" /\ restoring[l] = "no" /\ ~obj[l]
  /\ restoring' = [restoring EXCEPT ![l] = "check"]
  /\ UNCHANGED <<vm, stops, lane, obj, pending, tried, foreign>>

\* The restore reads that the lane is still open (`aborted()`).
RestoreCheck(l) ==
  /\ restoring[l] = "check"
  /\ IF lane[l] = "open"
       THEN restoring' = [restoring EXCEPT ![l] = "write"]
       ELSE restoring' = [restoring EXCEPT ![l] = "no"]
  /\ UNCHANGED <<vm, stops, lane, obj, pending, tried, foreign>>

\* The agent creates the fork's objects, with the VM running.
RestoreWrite(l) ==
  /\ restoring[l] = "write" /\ vm = "running"
  /\ obj' = [obj EXCEPT ![l] = TRUE]
  /\ restoring' = [restoring EXCEPT ![l] = "no"]
  /\ UNCHANGED <<vm, stops, lane, pending, tried, foreign>>

\* A lane job of an open lane creates its own objects.
LaneWrite(l) ==
  /\ lane[l] = "open" /\ vm = "running" /\ ~obj[l]
  /\ obj' = [obj EXCEPT ![l] = TRUE]
  /\ UNCHANGED <<vm, stops, lane, restoring, pending, tried, foreign>>

\* Close, cancel or remove: the lane is fenced and the drop journaled before it
\* is tried. It waits for the restore (`wait_restore`); the break does not.
\* The other break journals nothing.
Close(l) ==
  /\ lane[l] = "open"
  /\ restoring[l] = "no" \/ Bug = "dropWithoutWait"
  /\ lane' = [lane EXCEPT ![l] = "closed"]
  /\ pending' = [pending EXCEPT ![l] = (Bug # "dropOff")]
  /\ UNCHANGED <<vm, stops, obj, restoring, tried, foreign>>

\* The drop runs (the first try, or a retry from the services timer). By
\* workspace only; the break drops every workspace's objects. With the VM gone
\* there is nothing to drop; stopped, it cannot run and is tried again later.
DropRun(l) ==
  /\ lane[l] = "closed" /\ pending[l]
  /\ vm # "stopped"
  /\ Bug # "dropNoRetry" \/ ~tried[l]
  /\ obj' = IF Bug = "dropAllScopes"
              THEN [w \in Ws |-> FALSE]
              ELSE [obj EXCEPT ![l] = FALSE]
  /\ foreign' = (foreign \/ (\E w \in Ws \ {l} : obj[w] /\ ~obj'[w]))
  /\ pending' = [pending EXCEPT ![l] = FALSE]
  /\ UNCHANGED <<vm, stops, lane, restoring, tried>>

\* A try with the VM stopped: nothing could be dropped yet.
DropLater(l) ==
  /\ lane[l] = "closed" /\ pending[l] /\ vm = "stopped" /\ ~tried[l]
  /\ tried' = [tried EXCEPT ![l] = TRUE]
  /\ UNCHANGED <<vm, stops, lane, obj, restoring, pending, foreign>>

VmStop ==
  /\ vm = "running" /\ stops < 2
  /\ vm' = "stopped" /\ stops' = stops + 1
  /\ UNCHANGED <<lane, obj, restoring, pending, tried, foreign>>

VmStart ==
  /\ vm = "stopped"
  /\ vm' = "running"
  /\ UNCHANGED <<stops, lane, obj, restoring, pending, tried, foreign>>

\* `marsh services rm`: the engine's objects go with the VM.
VmRm ==
  /\ vm # "gone"
  /\ vm' = "gone" /\ obj' = [w \in Ws |-> FALSE]
  /\ UNCHANGED <<stops, lane, restoring, pending, tried, foreign>>

Next ==
  \/ BaseWrite \/ VmStop \/ VmStart \/ VmRm
  \/ \E l \in Lanes : Create(l) \/ RestoreBegin(l) \/ RestoreCheck(l) \/ RestoreWrite(l)
                      \/ LaneWrite(l) \/ Close(l) \/ DropRun(l) \/ DropLater(l)

Spec == Init /\ [][Next]_vars
        /\ WF_vars(VmStart)
        /\ \A l \in Lanes : WF_vars(DropRun(l)) /\ WF_vars(RestoreCheck(l))
                            /\ WF_vars(RestoreWrite(l))

\* A drop is by workspace only: no workspace loses objects to another's drop
\* (RST-11 for the engine side, RST-09 c1).
DropOnlyOwn == ~foreign

\* A closed lane whose drop is done has no objects: nothing creates objects for
\* a lane after its drop (a restore that was still running is waited for).
NoObjectsAfterDrop == \A l \in Lanes : (lane[l] = "closed" /\ ~pending[l]) => ~obj[l]

\* A closed lane's objects are eventually gone (zero fork objects after close,
\* cancel or rm), also when the VM was stopped at the first try.
ClosedLanesAreEmpty ==
  \A l \in Lanes : (lane[l] = "closed" /\ obj[l]) ~> ~obj[l]
=============================================================================
