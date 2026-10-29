----------------------------- MODULE Worker -----------------------------
(* One warm, owned kit VM: its single worker transport, concurrent job    *)
(* attempts (one fresh nonroot container each), and source mounts that   *)
(* are mounted on first reference and retained while the VM is warm: an   *)
(* idle mount is reused, unmounted only to replace a stale source, and    *)
(* released at Retire. VM ownership itself is Ownership.tla; "Retire" is  *)
(* a UUID-conditional remove + recreate of this owned VM.                 *)
(* (Overlap eviction of idle mounts is a static path check, not modeled.) *)
(*                                                                         *)
(* Attempt: idle -> admitted -> running -> exited -> verified             *)
(*          admitted -> rejected (mount check failed, cancel, loss before *)
(*                                start frame was written)                *)
(*          running/exited -> uncertain (transport lost)                  *)
(* A quarantined VM is retired by the operator (Retire) or automatically   *)
(* (AutoRetire) once no started attempt's container can still be running:  *)
(* the VM is stopped or its Docker lists no running container. `alive` is  *)
(* that physical fact; uncertain receipts stay uncertain either way.       *)
(* Sources can be replaced by the environment at any time (identity       *)
(* counter). The pathname race inside the stock mount call is outside the *)
(* model: if the post-mount identity equals the pre-mount identity we     *)
(* assume the mount bound that identity (upstream gap).                   *)
EXTENDS Naturals, FiniteSets

CONSTANTS Attempts, Sources, CAP, MaxGen, MaxId, Bug

Bugs == {"none", "noCap", "replayOnLoss", "verifyOnLoss", "replaceQuarantined",
         "skipPostCheck", "reuseStaleMount", "earlyUnmount", "cancelAll",
         "hangOnLoss", "evictInUse", "retireUnverified"}
ASSUME Bug \in Bugs

Active   == {"admitted", "running", "exited"}
Terminal == {"verified", "uncertain", "rejected"}
Started  == {"running", "exited"}

VARIABLES
  ast,       \* [Attempts -> state]
  asrc,      \* [Attempts -> Sources] source the attempt mounts
  aid,       \* [Attempts -> 0..MaxId] source identity verified at admission
  agen,      \* [Attempts -> 0..MaxGen] transport generation of the attempt
  held,      \* [Attempts -> BOOLEAN] attempt holds a mount reference
  runs,      \* ghost: number of container starts per attempt
  lost,      \* ghost: attempt had started when its transport was lost
  why,       \* ghost: exit reason "none" | "self" | "cancel"
  cancelReq, \* client of that attempt was killed
  gen,       \* current transport generation
  tstate,    \* "live" | "dead"
  quar,      \* VM quarantined
  ref,       \* [Sources -> Nat] mount refcount
  mst,       \* [Sources -> "unmounted" | "mounting" | "mounted"]; "mounted"
             \* with ref = 0 is an idle retained mount
  mpre,      \* identity observed before mount
  mid,       \* identity actually mounted
  srcId,     \* environment: current identity at the source path
  alive,     \* ghost: [Attempts -> BOOLEAN] the attempt's container is running
  autoKill   \* ghost: an automatic retire destroyed a running container

vars == <<ast, asrc, aid, agen, held, runs, lost, why, cancelReq,
          gen, tstate, quar, ref, mst, mpre, mid, srcId, alive, autoKill>>
avars == <<ast, asrc, aid, agen, held, runs, lost, why, cancelReq>>
mvars == <<ref, mst, mpre, mid>>

S0 == CHOOSE s \in Sources : TRUE

Init ==
  /\ ast = [a \in Attempts |-> "idle"]
  /\ asrc = [a \in Attempts |-> S0]
  /\ aid = [a \in Attempts |-> 0]
  /\ agen = [a \in Attempts |-> 0]
  /\ held = [a \in Attempts |-> FALSE]
  /\ runs = [a \in Attempts |-> 0]
  /\ lost = [a \in Attempts |-> FALSE]
  /\ why = [a \in Attempts |-> "none"]
  /\ cancelReq = [a \in Attempts |-> FALSE]
  /\ gen = 1 /\ tstate = "live" /\ quar = FALSE
  /\ ref = [s \in Sources |-> 0]
  /\ mst = [s \in Sources |-> "unmounted"]
  /\ mpre = [s \in Sources |-> 0]
  /\ mid = [s \in Sources |-> 0]
  /\ srcId = [s \in Sources |-> 1]
  /\ alive = [a \in Attempts |-> FALSE]
  /\ autoKill = FALSE

NActive == Cardinality({a \in Attempts : ast[a] \in Active})
Holders(h, s) == {a \in Attempts : h[a] /\ asrc[a] = s}
\* Refcounts after a set of references is dropped. Dropping the final
\* reference keeps the mount (retained while warm), so mst is unchanged.
RefAfter(h) == [s \in Sources |-> Cardinality(Holders(h, s))]
MstAfter(r) == mst

---------------------------------------------------------------------------
Admit(a, s) ==
  LET stale == mst[s] = "mounted" /\ srcId[s] # mid[s] /\ Bug # "reuseStaleMount"
      \* Idle retained mount of a replaced source: unmount it, mount afresh.
      evict == stale /\ (ref[s] = 0 \/ Bug = "evictInUse")
      ok == CASE mst[s] = "unmounted" -> TRUE
              [] mst[s] = "mounting"  -> srcId[s] = mpre[s]
              [] mst[s] = "mounted"   -> ~stale \/ evict
  IN
  /\ ast[a] = "idle" /\ tstate = "live"
  /\ ~quar \/ Bug = "replaceQuarantined"
  /\ NActive < CAP \/ Bug = "noCap"
  /\ asrc' = [asrc EXCEPT ![a] = s]
  /\ IF ok
       THEN /\ ast' = [ast EXCEPT ![a] = "admitted"]
            /\ aid' = [aid EXCEPT ![a] = srcId[s]]
            /\ agen' = [agen EXCEPT ![a] = gen]
            /\ held' = [held EXCEPT ![a] = TRUE]
            /\ ref' = [ref EXCEPT ![s] = @ + 1]
            /\ IF mst[s] = "unmounted" \/ evict
                 THEN /\ mst' = [mst EXCEPT ![s] = "mounting"]
                      /\ mpre' = [mpre EXCEPT ![s] = srcId[s]]
                 ELSE UNCHANGED <<mst, mpre>>
       ELSE /\ ast' = [ast EXCEPT ![a] = "rejected"]     \* fail closed
            /\ UNCHANGED <<aid, agen, held, ref, mst, mpre>>
  /\ UNCHANGED <<runs, lost, why, cancelReq, gen, tstate, quar, mid, srcId, alive, autoKill>>

\* Stock mount call, then post-mount identity check.
MountFinish(s) ==
  /\ mst[s] = "mounting"
  /\ IF srcId[s] = mpre[s] \/ Bug = "skipPostCheck"
       THEN /\ mst' = [mst EXCEPT ![s] = "mounted"]
            /\ mid' = [mid EXCEPT ![s] = srcId[s]]
            /\ UNCHANGED <<ast, held, ref>>
       ELSE \* mismatch: unmount, reject every waiter, never use the mount
            LET W == Holders(held, s)
                h2 == [a \in Attempts |-> IF a \in W THEN FALSE ELSE held[a]]
            IN /\ ast' = [a \in Attempts |-> IF a \in W THEN "rejected" ELSE ast[a]]
               /\ held' = h2
               /\ ref' = RefAfter(h2)
               /\ mst' = [mst EXCEPT ![s] = "unmounted"]
               /\ UNCHANGED mid
  /\ UNCHANGED <<asrc, aid, agen, runs, lost, why, cancelReq, gen, tstate, quar,
                 mpre, srcId, alive, autoKill>>

\* Start frame written; fresh container created.
Run(a) ==
  /\ ast[a] = "admitted" /\ tstate = "live" /\ agen[a] = gen
  /\ ~quar \/ Bug = "replaceQuarantined"
  /\ mst[asrc[a]] = "mounted"
  /\ ast' = [ast EXCEPT ![a] = "running"]
  /\ runs' = [runs EXCEPT ![a] = @ + 1]
  /\ alive' = [alive EXCEPT ![a] = TRUE]
  /\ UNCHANGED <<asrc, aid, agen, held, lost, why, cancelReq, gen, tstate, quar,
                 ref, mst, mpre, mid, srcId, autoKill>>

Exit(a) ==
  /\ ast[a] = "running"
  /\ ast' = [ast EXCEPT ![a] = "exited"]
  /\ why' = [why EXCEPT ![a] = "self"]
  /\ alive' = [alive EXCEPT ![a] = FALSE]
  /\ UNCHANGED <<asrc, aid, agen, held, runs, lost, cancelReq, gen, tstate, quar,
                 ref, mst, mpre, mid, srcId, autoKill>>

\* Client of attempt a was killed: cancel only a.
Cancel(a) ==
  LET victims == IF Bug = "cancelAll"
                   THEN {b \in Attempts : ast[b] = "running"} ELSE {a} IN
  /\ ast[a] \in {"admitted", "running"} /\ ~cancelReq[a]
  /\ cancelReq' = [cancelReq EXCEPT ![a] = TRUE]
  /\ IF ast[a] = "admitted"
       THEN LET h2 == [held EXCEPT ![a] = FALSE] IN
            /\ ast' = [ast EXCEPT ![a] = "rejected"]
            /\ held' = h2 /\ ref' = RefAfter(h2) /\ mst' = MstAfter(RefAfter(h2))
            /\ UNCHANGED why
       ELSE /\ ast' = [b \in Attempts |-> IF b \in victims THEN "exited" ELSE ast[b]]
            /\ why' = [b \in Attempts |-> IF b \in victims THEN "cancel" ELSE why[b]]
            /\ UNCHANGED <<held, ref, mst>>
  /\ alive' = [b \in Attempts |-> IF ast'[b] = "exited" THEN FALSE ELSE alive[b]]
  /\ UNCHANGED <<asrc, aid, agen, runs, lost, gen, tstate, quar, mpre, mid, srcId, autoKill>>

\* Worker reports container removed; drop the mount reference.
Cleanup(a) ==
  LET h2 == [held EXCEPT ![a] = FALSE]
      r2 == RefAfter(h2) IN
  /\ ast[a] = "exited" /\ tstate = "live" /\ agen[a] = gen
  /\ ast' = [ast EXCEPT ![a] = "verified"]
  /\ held' = h2 /\ ref' = r2
  /\ mst' = IF Bug = "earlyUnmount" THEN [mst EXCEPT ![asrc[a]] = "unmounted"]
            ELSE MstAfter(r2)
  /\ UNCHANGED <<asrc, aid, agen, runs, lost, why, cancelReq, gen, tstate, quar,
                 mpre, mid, srcId, alive, autoKill>>

\* Environment: the single `sbx exec -i` worker transport dies.
TransportLoss ==
  LET S  == {a \in Attempts : ast[a] \in Started /\ agen[a] = gen}
      A  == {a \in Attempts : ast[a] = "admitted"}
  IN
  /\ tstate = "live"
  /\ lost' = [a \in Attempts |-> lost[a] \/ a \in S]
  /\ CASE Bug = "replayOnLoss" /\ gen < MaxGen ->
            \* WRONG: re-admit started attempts on a fresh transport
            /\ ast' = [a \in Attempts |-> IF a \in S THEN "admitted" ELSE ast[a]]
            /\ agen' = [a \in Attempts |-> IF a \in S \cup A THEN gen + 1 ELSE agen[a]]
            /\ gen' = gen + 1
            /\ UNCHANGED <<held, ref, mst, tstate, quar>>
       [] Bug = "verifyOnLoss" ->
            LET h2 == [a \in Attempts |-> IF a \in S \cup A THEN FALSE ELSE held[a]] IN
            /\ ast' = [a \in Attempts |-> IF a \in S THEN "verified"
                                          ELSE IF a \in A THEN "rejected" ELSE ast[a]]
            /\ held' = h2 /\ ref' = RefAfter(h2) /\ mst' = MstAfter(RefAfter(h2))
            /\ tstate' = "dead" /\ UNCHANGED <<agen, gen, quar>>
       [] Bug = "hangOnLoss" ->
            \* WRONG: quarantine but leave started attempts non-terminal
            /\ tstate' = "dead" /\ quar' = (quar \/ S # {})
            /\ UNCHANGED <<ast, held, ref, mst, agen, gen>>
       [] OTHER ->
            \* Never replay. Started -> uncertain (keep refs: container may
            \* still write). Admitted-but-unsent -> rejected.
            LET h2 == [a \in Attempts |-> IF a \in A THEN FALSE ELSE held[a]] IN
            /\ ast' = [a \in Attempts |-> IF a \in S THEN "uncertain"
                                          ELSE IF a \in A THEN "rejected" ELSE ast[a]]
            /\ held' = h2 /\ ref' = RefAfter(h2) /\ mst' = MstAfter(RefAfter(h2))
            /\ tstate' = "dead"
            /\ quar' = (quar \/ S # {})
            /\ UNCHANGED <<agen, gen>>
  /\ UNCHANGED <<asrc, aid, runs, why, cancelReq, mpre, mid, srcId, alive, autoKill>>

\* Environment: an uncertain attempt's container ends on its own, or the
\* quarantine stop (`sbx stop`) took effect and stopped the whole VM.
ContainerEnds(a) ==
  /\ alive[a] /\ ast[a] = "uncertain"
  /\ alive' = [alive EXCEPT ![a] = FALSE]
  /\ UNCHANGED <<avars, gen, tstate, quar, mvars, srcId, autoKill>>

\* A dead idle transport may be replaced.
ReplaceTransport ==
  /\ tstate = "dead" /\ gen < MaxGen
  /\ ~quar \/ Bug = "replaceQuarantined"
  /\ \A a \in Attempts : ast[a] \notin Active
  /\ gen' = gen + 1 /\ tstate' = "live"
  /\ UNCHANGED <<avars, quar, mvars, srcId, alive, autoKill>>

\* Quarantine release: UUID-conditional remove of the owned VM (destroys
\* every container and mount), then recreate. Uncertain receipts stay
\* uncertain; they are never replayed.
RetireVM ==
  /\ quar /\ tstate = "dead" /\ gen < MaxGen
  /\ held' = [a \in Attempts |-> FALSE]
  /\ ref' = [s \in Sources |-> 0]
  /\ mst' = [s \in Sources |-> "unmounted"]
  /\ alive' = [a \in Attempts |-> FALSE]
  /\ quar' = FALSE /\ gen' = gen + 1 /\ tstate' = "live"
  /\ UNCHANGED <<ast, asrc, aid, agen, runs, lost, why, cancelReq, mpre, mid, srcId>>

\* Operator: `marsh workers reset KIT` retires it whatever runs there.
Retire == RetireVM /\ UNCHANGED autoKill

\* Automatic recovery: only once verification shows no container of a
\* started attempt is still running (worker_recovery.rs).
AutoRetire ==
  /\ (\A a \in Attempts : ~alive[a]) \/ Bug = "retireUnverified"
  /\ RetireVM
  /\ autoKill' = (autoKill \/ \E a \in Attempts : alive[a])

\* Environment: something replaces the directory at a source path.
ReplaceSource(s) ==
  /\ srcId[s] < MaxId
  /\ srcId' = [srcId EXCEPT ![s] = @ + 1]
  /\ UNCHANGED <<avars, gen, tstate, quar, mvars, alive, autoKill>>

---------------------------------------------------------------------------
Next ==
  \/ \E a \in Attempts, s \in Sources : Admit(a, s)
  \/ \E s \in Sources : MountFinish(s) \/ ReplaceSource(s)
  \/ \E a \in Attempts : Run(a) \/ Exit(a) \/ Cancel(a) \/ Cleanup(a) \/ ContainerEnds(a)
  \/ TransportLoss \/ ReplaceTransport \/ Retire \/ AutoRetire

Spec == /\ Init /\ [][Next]_vars
        /\ \A a \in Attempts : WF_vars(Run(a)) /\ WF_vars(Exit(a)) /\ WF_vars(Cleanup(a))
                                /\ WF_vars(ContainerEnds(a))
        /\ \A s \in Sources : WF_vars(MountFinish(s))
        /\ WF_vars(ReplaceTransport) /\ WF_vars(AutoRetire)

---------------------------------------------------------------------------
(* Safety *)
CapacityBound == NActive <= CAP
NoReplay == \A a \in Attempts : runs[a] <= 1
NoStartOnQuarantined ==
  \A a \in Attempts : ast[a] = "running" => (~quar /\ agen[a] = gen)
LossIsUncertain == \A a \in Attempts : lost[a] => ast[a] = "uncertain"
\* Referenced => mounted (or mounting). Unreferenced mounts may be retained.
MountRefcount ==
  \A s \in Sources : /\ ref[s] = Cardinality(Holders(held, s))
                     /\ (ref[s] > 0) => (mst[s] # "unmounted")
NoStaleSourceUse ==
  \A a \in Attempts : ast[a] \in Started =>
      mst[asrc[a]] = "mounted" /\ mid[asrc[a]] = aid[a]
CancelScoped == \A a \in Attempts : why[a] = "cancel" => cancelReq[a]
\* Automatic recovery never destroys a container that may still be running.
AutoRetireVerified == ~autoKill

(* Liveness: every admitted attempt reaches a terminal state, assuming jobs *)
(* exit and the daemon keeps acting (weak fairness above).                 *)
AttemptsTerminate ==
  \A a \in Attempts : (ast[a] \in Active) ~> (ast[a] \in Terminal)
(* A quarantine lifts without an operator once the containers are gone. *)
QuarantineHeals == (quar /\ gen < MaxGen) ~> ~quar
=============================================================================
