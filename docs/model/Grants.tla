----------------------------- MODULE Grants -----------------------------
(* Revocable publications through the host daemon:                       *)
(*  - an ACP session published as an MCP control tool (prompt / cancel / *)
(*    choose an offered one-time permission), and                        *)
(*  - an MCP pipeline published and loaded into a named same-user stock  *)
(*    sandbox.                                                           *)
(* Both have the same shape: a grant is bound at publish time to one     *)
(* target incarnation (ACP session id / sandbox UUID). The environment   *)
(* may restart the session or recreate the sandbox under the same name.  *)
(* Requests arrive asynchronously and are delivered later; the daemon    *)
(* must check the grant at delivery, not only at arrival.                *)
(*                                                                       *)
(* Default MCP publication (`mcp publish NAME -- P`, no target): the     *)
(* daemon records it after the publish commits; a Kit VM that the daemon *)
(* creates afterwards loads it before its first job; a VM that already   *)
(* runs is never loaded automatically (the stock gateway is per VM, so   *)
(* loading it changes what running jobs see). The record is re-checked   *)
(* against the declaration at create time, because a host-terminal       *)
(* unpublish revokes without the daemon and leaves the record behind.    *)
EXTENDS Naturals

CONSTANTS Grants, MaxInc, Bug

Bugs == {"none", "checkAtArrival", "loadByName", "permReuse",
         "loadRunning", "trustRecord", "skipCreateLoad"}
ASSUME Bug \in Bugs

VARIABLES
  gst,       \* [Grants -> "none" | "published" | "revoked"]
  bound,     \* [Grants -> 0..MaxInc] target incarnation bound at publish
  target,    \* [Grants -> 1..MaxInc] current incarnation behind the name
  inflight,  \* [Grants -> BOOLEAN] request accepted at the front door
  perm,      \* [Grants -> "none" | "offered" | "chosen" | "void"]
  uses,      \* ghost: times the one offered permission was exercised
  afterRevoke, \* ghost: effect delivered through a revoked grant
  wrongTarget, \* ghost: effect delivered to an incarnation not bound
  dst,       \* default publication: "none" | "published" | "revoked"
  rec,       \* daemon's default record (mcp-defaults.json) present
  vm,        \* one Kit VM: "absent" | "running"
  vmOld,     \* the running VM existed when the record was written
  vmLoaded,  \* the default is loaded into the running VM's gateway
  intoRunning, \* ghost: a default was loaded into an already-running VM
  staleLoad    \* ghost: a revoked default was loaded into a new VM

gvars == <<gst, bound, target, inflight, perm, uses, afterRevoke, wrongTarget>>
dvars == <<dst, rec, vm, vmOld, vmLoaded, intoRunning, staleLoad>>
vars == <<gvars, dvars>>

Init ==
  /\ gst = [g \in Grants |-> "none"]
  /\ bound = [g \in Grants |-> 0]
  /\ target = [g \in Grants |-> 1]
  /\ inflight = [g \in Grants |-> FALSE]
  /\ perm = [g \in Grants |-> "none"]
  /\ uses = [g \in Grants |-> 0]
  /\ afterRevoke = FALSE /\ wrongTarget = FALSE
  /\ dst = "none" /\ rec = FALSE /\ vm = "absent" /\ vmOld = FALSE
  /\ vmLoaded = FALSE /\ intoRunning = FALSE /\ staleLoad = FALSE

Live(g) == gst[g] = "published" /\ target[g] = bound[g]
Effect(g) == /\ afterRevoke' = (afterRevoke \/ gst[g] # "published")
             /\ wrongTarget' = (wrongTarget \/ target[g] # bound[g])

Publish(g) ==
  /\ gst[g] = "none"
  /\ gst' = [gst EXCEPT ![g] = "published"]
  /\ bound' = [bound EXCEPT ![g] = target[g]]
  /\ UNCHANGED <<target, inflight, perm, uses, afterRevoke, wrongTarget>>

\* Revoke voids any outstanding one-time permission offer.
Revoke(g) ==
  /\ gst[g] = "published"
  /\ gst' = [gst EXCEPT ![g] = "revoked"]
  /\ perm' = [perm EXCEPT ![g] = IF @ = "offered" THEN "void" ELSE @]
  /\ UNCHANGED <<bound, target, inflight, uses, afterRevoke, wrongTarget>>

\* Prompt / cancel / load request arrives at the daemon.
Arrive(g) ==
  /\ ~inflight[g] /\ gst[g] = "published"
  /\ inflight' = [inflight EXCEPT ![g] = TRUE]
  /\ UNCHANGED <<gst, bound, target, perm, uses, afterRevoke, wrongTarget>>

\* Daemon forwards to the agent / sandbox.
Deliver(g) ==
  LET ok == CASE Bug = "checkAtArrival" -> TRUE
              [] Bug = "loadByName"     -> gst[g] = "published"
              [] OTHER                  -> Live(g)
  IN
  /\ inflight[g]
  /\ inflight' = [inflight EXCEPT ![g] = FALSE]
  /\ IF ok THEN Effect(g) ELSE UNCHANGED <<afterRevoke, wrongTarget>>
  /\ UNCHANGED <<gst, bound, target, perm, uses>>

\* Environment: session restarts / sandbox recreated under the same name.
Restart(g) ==
  /\ target[g] < MaxInc
  /\ target' = [target EXCEPT ![g] = @ + 1]
  /\ UNCHANGED <<gst, bound, inflight, perm, uses, afterRevoke, wrongTarget>>

\* Agent offers a one-time permission.
Offer(g) ==
  /\ perm[g] = "none" /\ Live(g)
  /\ perm' = [perm EXCEPT ![g] = "offered"]
  /\ UNCHANGED <<gst, bound, target, inflight, uses, afterRevoke, wrongTarget>>

\* Client chooses the offered permission through the grant.
Choose(g) ==
  /\ perm[g] = "offered" /\ Live(g) /\ uses[g] < 2
  /\ perm' = [perm EXCEPT ![g] = IF Bug = "permReuse" THEN "offered" ELSE "chosen"]
  /\ uses' = [uses EXCEPT ![g] = @ + 1]
  /\ Effect(g)
  /\ UNCHANGED <<gst, bound, target, inflight>>

\* ---- Default publication ----------------------------------------------
\* Publish commits (declaration + stock registration); no VM is loaded.
DPublish ==
  /\ dst = "none"
  /\ dst' = "published"
  /\ UNCHANGED <<rec, vm, vmOld, vmLoaded, intoRunning, staleLoad>>

\* The daemon writes its record before replying to `mcp publish`.
DRecord ==
  /\ dst = "published" /\ ~rec
  /\ rec' = TRUE
  /\ vmOld' = (vm = "running")
  /\ UNCHANGED <<dst, vm, vmLoaded, intoRunning, staleLoad>>

\* Shell `mcp unpublish`: drop the record, then revoke; stock rm unloads.
DRevokeDaemon ==
  /\ dst = "published"
  /\ dst' = "revoked" /\ rec' = FALSE /\ vmLoaded' = FALSE
  /\ UNCHANGED <<vm, vmOld, intoRunning, staleLoad>>

\* Host-terminal `marsh mcp unpublish`: bypasses the daemon's record.
DRevokeHost ==
  /\ dst = "published"
  /\ dst' = "revoked" /\ vmLoaded' = FALSE
  /\ UNCHANGED <<rec, vm, vmOld, intoRunning, staleLoad>>

\* The daemon creates the Kit VM (first use / after workers reset) and loads
\* every record whose declaration is still current, before the first job.
VmCreate ==
  LET ok == CASE Bug = "trustRecord"    -> rec
              [] Bug = "skipCreateLoad" -> FALSE
              [] OTHER                  -> rec /\ dst = "published"
  IN
  /\ vm = "absent"
  /\ vm' = "running" /\ vmOld' = FALSE
  /\ vmLoaded' = ok
  /\ staleLoad' = (staleLoad \/ (ok /\ dst # "published"))
  /\ UNCHANGED <<dst, rec, intoRunning>>

\* `marsh workers reset KIT` (or the VM disappears).
VmRemove ==
  /\ vm = "running"
  /\ vm' = "absent" /\ vmLoaded' = FALSE /\ vmOld' = FALSE
  /\ UNCHANGED <<dst, rec, intoRunning, staleLoad>>

\* Negative control only: load lazily into the running VM (e.g. at the next
\* job admission). The implementation has no such step.
LazyLoad ==
  /\ Bug = "loadRunning"
  /\ vm = "running" /\ rec /\ dst = "published" /\ ~vmLoaded
  /\ vmLoaded' = TRUE /\ intoRunning' = TRUE
  /\ UNCHANGED <<dst, rec, vm, vmOld, staleLoad>>

DNext == DPublish \/ DRecord \/ DRevokeDaemon \/ DRevokeHost \/ VmCreate
         \/ VmRemove \/ LazyLoad

Next == \/ /\ \E g \in Grants :
                Publish(g) \/ Revoke(g) \/ Arrive(g) \/ Deliver(g) \/ Restart(g)
                \/ Offer(g) \/ Choose(g)
           /\ UNCHANGED dvars
        \/ DNext /\ UNCHANGED gvars

Spec == Init /\ [][Next]_vars

RevokedGrantInert == ~afterRevoke /\ \A g \in Grants : gst[g] = "revoked" => perm[g] # "offered"
GrantTargetBound  == ~wrongTarget
OneTimePermission == \A g \in Grants : uses[g] <= 1
\* A default never changes a VM that was already running when recorded.
DefaultSparesRunning == ~intoRunning
\* A revoked default is never loaded into a new VM, even from a stale record.
DefaultRevokedInert == ~staleLoad
\* Every VM created after the record (while still published) has the tool.
DefaultReachesNewVms ==
  (vm = "running" /\ ~vmOld /\ rec /\ dst = "published") => vmLoaded
=============================================================================
