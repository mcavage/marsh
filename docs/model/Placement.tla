---------------------------- MODULE Placement ----------------------------
(* PROPOSED cross-placement workspace lifecycle; no Rust refinement claim. *)
(* Locations are logical views, not paths. A child crossing reads/returns  *)
(* its parent's materialization, never the root checkout. Each crossing    *)
(* holds one shared admission slot through result settlement and verified  *)
(* deletion. The sandbox truth can outlive a successful delete request.    *)
(*                                                                       *)
(* A one-path Apply is intentionally nontransactional and last-writer-wins.*)
(* CallerEdit may race; unchanged remote paths must not be written back.   *)
(* File bytes, manifests, source inode admission and stream credit are     *)
(* abstracted, not proven. Ownership of each sandbox is assumed here;      *)
(* account/ID/create-intent reconciliation needs the Ownership extension.  *)
EXTENDS Integers, FiniteSets

CONSTANTS Jobs, Files, Values, CAP, MaxRestarts, Bug
ASSUME Jobs \subseteq (Nat \ {0}) /\ 0 \in Values
ASSUME \A j \in Jobs : j <= Cardinality(Jobs)
ASSUME Bug \in {"none", "ancestorReturn", "hostSnapshot", "widen",
               "captureLive", "copySnapshot", "lateReturn", "replay",
               "ackIsAbsent", "earlyComplete", "noCap", "pendingReturn"}

User == 0
Fork == Cardinality(Jobs) + 1
Roots == {User, Fork}
Locs == Roots \cup Jobs
Data == Values \cup {-1}  \* -1 abstracts an absent file
Phases == {"none", "staged", "running", "stopped", "applying", "done", "uncertain"}
Pending == {"staged", "running", "stopped", "applying"}
Places == {"local", "cloud"}
EmptyTree == [f \in Files |-> 0]

VARIABLES phase, parent, place, scope, rights, dest, source, baseline,
          final, fs, alive, captured, applied, held, runs, cap, epoch,
          generation, present, deleteAck, absentObserved, wrote, edited,
          lost, badCapture, badEpoch, badComplete

vars == <<phase, parent, place, scope, rights, dest, source, baseline,
          final, fs, alive, captured, applied, held, runs, cap, epoch,
          generation, present, deleteAck, absentObserved, wrote, edited,
          lost, badCapture, badEpoch, badComplete>>

Init ==
  /\ phase = [j \in Jobs |-> "none"]
  /\ parent = [j \in Jobs |-> 0]
  /\ place = [j \in Jobs |-> "local"]
  /\ scope = [j \in Jobs |-> User]
  /\ rights = [j \in Jobs |-> {}]
  /\ dest = [j \in Jobs |-> User]
  /\ source = [j \in Jobs |-> User]
  /\ baseline = [j \in Jobs |-> EmptyTree]
  /\ final = [j \in Jobs |-> EmptyTree]
  /\ fs = [l \in Locs |-> EmptyTree]
  /\ alive = [j \in Jobs |-> FALSE]
  /\ captured = [j \in Jobs |-> FALSE]
  /\ applied = [j \in Jobs |-> {}]
  /\ held = [j \in Jobs |-> FALSE]
  /\ runs = [j \in Jobs |-> 0]
  /\ cap = [j \in Jobs |-> FALSE]
  /\ epoch = [j \in Jobs |-> 0]
  /\ generation = 0
  /\ present = [j \in Jobs |-> FALSE]
  /\ deleteAck = [j \in Jobs |-> FALSE]
  /\ absentObserved = [j \in Jobs |-> FALSE]
  /\ wrote = {}
  /\ edited = {}
  /\ lost = {}
  /\ badCapture = FALSE /\ badEpoch = FALSE /\ badComplete = FALSE

RECURSIVE Anc(_, _)
Anc(j, n) == IF n = 0 \/ parent[j] = 0 THEN {}
             ELSE {parent[j]} \cup Anc(parent[j], n - 1)
Desc(j) == {k \in Jobs : phase[k] # "none" /\ j \in Anc(k, Cardinality(Jobs))}
Children(j) == {k \in Jobs : phase[k] # "none" /\ parent[k] = j}
Delta(j) == {f \in Files : final[j][f] # baseline[j][f]}
Quiescent(j) == ~alive[j] /\ \A k \in Desc(j) : ~alive[k]
ReturnsSettled(j) == \A k \in Desc(j) : phase[k] = "done"
Slots == Cardinality({j \in Jobs : held[j]})

(* Persist transfer binding + admission before provisioning or execution. *)
Stage(j, p, root, loc) ==
  /\ phase[j] = "none"
  /\ \A k \in Jobs : k < j => phase[k] # "none"
  /\ p = 0 \/ (p \in Jobs /\ p < j /\ phase[p] = "running" /\ cap[p])
  /\ loc = (IF p = 0 THEN "cloud"
             ELSE IF place[p] = "cloud" THEN "local" ELSE "cloud")
  /\ Slots < CAP \/ Bug = "noCap"
  /\ LET target == IF p = 0 THEN root ELSE p
         src == IF p # 0 /\ Bug = "hostSnapshot" THEN User ELSE target
     IN /\ source' = [source EXCEPT ![j] = src]
        /\ dest' = [dest EXCEPT ![j] =
             IF Bug = "ancestorReturn" /\ (p # 0 \/ root = Fork)
             THEN User ELSE target]
        /\ baseline' = [baseline EXCEPT ![j] = fs[src]]
        /\ fs' = [fs EXCEPT ![j] = fs[src]]
  /\ phase' = [phase EXCEPT ![j] = "staged"]
  /\ parent' = [parent EXCEPT ![j] = p]
  /\ place' = [place EXCEPT ![j] = loc]
  /\ scope' = [scope EXCEPT ![j] = IF p = 0 THEN root ELSE scope[p]]
  /\ rights' = [rights EXCEPT ![j] =
       IF p = 0 THEN {root}
       ELSE IF Bug = "widen" THEN Roots ELSE rights[p]]
  /\ held' = [held EXCEPT ![j] = TRUE]
  /\ cap' = [cap EXCEPT ![j] = TRUE]
  /\ epoch' = [epoch EXCEPT ![j] = generation]
  /\ present' = [present EXCEPT ![j] = TRUE]
  /\ UNCHANGED <<final, alive, captured, applied, runs, generation, deleteAck,
                  absentObserved, wrote, edited, lost, badCapture, badEpoch, badComplete>>

Start(j) ==
  /\ phase[j] = "staged" \/ (Bug = "replay" /\ phase[j] = "uncertain" /\ runs[j] = 1)
  /\ cap[j] \/ Bug = "replay"
  /\ epoch[j] = generation \/ Bug = "replay"
  /\ phase' = [phase EXCEPT ![j] = "running"]
  /\ alive' = [alive EXCEPT ![j] = TRUE]
  /\ runs' = [runs EXCEPT ![j] = @ + 1]
  /\ UNCHANGED <<parent, place, scope, rights, dest, source, baseline, final, fs,
                  captured, applied, held, cap, epoch, generation, present, deleteAck,
                  absentObserved, wrote, edited, lost, badCapture, badEpoch, badComplete>>

(* One bounded program edit per path/crossing. Child returns add more edits. *)
Write(j, f, v) ==
  /\ phase[j] = "running" /\ alive[j] /\ <<j, f>> \notin wrote
  /\ v # fs[j][f]
  /\ fs' = [fs EXCEPT ![j][f] = v]
  /\ wrote' = wrote \cup {<<j, f>>}
  /\ UNCHANGED <<phase, parent, place, scope, rights, dest, source, baseline, final,
                  alive, captured, applied, held, runs, cap, epoch, generation,
                  present, deleteAck, absentObserved, edited, lost,
                  badCapture, badEpoch, badComplete>>

CallerEdit(root, f, v) ==
  /\ <<root, f>> \notin edited
  /\ v # fs[root][f]
  /\ fs' = [fs EXCEPT ![root][f] = v]
  /\ edited' = edited \cup {<<root, f>>}
  /\ UNCHANGED <<phase, parent, place, scope, rights, dest, source, baseline, final,
                  alive, captured, applied, held, runs, cap, epoch, generation,
                  present, deleteAck, absentObserved, wrote, lost,
                  badCapture, badEpoch, badComplete>>

(* Abstract verified stop of this scope and settled child returns. *)
Stop(j) ==
  /\ phase[j] = "running"
  /\ ReturnsSettled(j) \/ Bug = "pendingReturn"
  /\ phase' = [phase EXCEPT ![j] = "stopped"]
  /\ alive' = [alive EXCEPT ![j] = FALSE]
  /\ cap' = [cap EXCEPT ![j] = FALSE]
  /\ UNCHANGED <<parent, place, scope, rights, dest, source, baseline, final, fs,
                  captured, applied, held, runs, epoch, generation, present, deleteAck,
                  absentObserved, wrote, edited, lost, badCapture, badEpoch, badComplete>>

Capture(j) ==
  /\ phase[j] = "stopped" \/ (Bug = "captureLive" /\ phase[j] = "running")
  /\ epoch[j] = generation /\ (Quiescent(j) \/ Bug = "captureLive")
  /\ ReturnsSettled(j) \/ Bug = "pendingReturn" \/ Bug = "captureLive"
  /\ phase' = [phase EXCEPT ![j] = "applying"]
  /\ captured' = [captured EXCEPT ![j] = TRUE]
  /\ final' = [final EXCEPT ![j] = fs[j]]
  /\ badCapture' = (badCapture \/ ~Quiescent(j))
  /\ UNCHANGED <<parent, place, scope, rights, dest, source, baseline, fs,
                  alive, applied, held, runs, cap, epoch, generation, present, deleteAck,
                  absentObserved, wrote, edited, lost, badEpoch, badComplete>>

Apply(j, f) ==
  /\ captured[j]
  /\ phase[j] = "applying" \/ (Bug = "lateReturn" /\ phase[j] = "uncertain")
  /\ epoch[j] = generation \/ Bug = "lateReturn"
  /\ f \in (IF Bug = "copySnapshot" THEN Files ELSE Delta(j)) \ applied[j]
  /\ fs' = [fs EXCEPT ![dest[j]][f] = final[j][f]]
  /\ applied' = [applied EXCEPT ![j] = @ \cup {f}]
  /\ badEpoch' = (badEpoch \/ epoch[j] # generation \/ phase[j] = "uncertain")
  /\ UNCHANGED <<phase, parent, place, scope, rights, dest, source, baseline, final,
                  alive, captured, held, runs, cap, epoch, generation, present, deleteAck,
                  absentObserved, wrote, edited, lost, badCapture, badComplete>>

DeleteRequest(j) ==
  /\ phase[j] \in {"applying", "uncertain"} /\ ~deleteAck[j]
  /\ deleteAck' = [deleteAck EXCEPT ![j] = TRUE]
  /\ absentObserved' = [absentObserved EXCEPT ![j] = (Bug = "ackIsAbsent")]
  /\ UNCHANGED <<phase, parent, place, scope, rights, dest, source, baseline, final, fs,
                  alive, captured, applied, held, runs, cap, epoch, generation, present,
                  wrote, edited, lost, badCapture, badEpoch, badComplete>>

(* Environment: deletion finishes later, and can destroy all writers. *)
DeleteLands(j) ==
  /\ deleteAck[j] /\ present[j]
  /\ present' = [present EXCEPT ![j] = FALSE]
  /\ alive' = [alive EXCEPT ![j] = FALSE]
  /\ UNCHANGED <<phase, parent, place, scope, rights, dest, source, baseline, final, fs,
                  captured, applied, held, runs, cap, epoch, generation, deleteAck,
                  absentObserved, wrote, edited, lost, badCapture, badEpoch, badComplete>>

ObserveAbsent(j) ==
  /\ phase[j] # "none" /\ ~present[j] /\ ~absentObserved[j]
  /\ absentObserved' = [absentObserved EXCEPT ![j] = TRUE]
  /\ UNCHANGED <<phase, parent, place, scope, rights, dest, source, baseline, final, fs,
                  alive, captured, applied, held, runs, cap, epoch, generation, present,
                  deleteAck, wrote, edited, lost, badCapture, badEpoch, badComplete>>

Complete(j) ==
  /\ phase[j] = "applying" /\ absentObserved[j]
  /\ Delta(j) \subseteq applied[j] \/ Bug = "earlyComplete"
  /\ phase' = [phase EXCEPT ![j] = "done"]
  /\ held' = [held EXCEPT ![j] = FALSE]
  /\ cap' = [cap EXCEPT ![j] = FALSE]
  /\ badComplete' = (badComplete \/ ~(Delta(j) \subseteq applied[j]))
  /\ UNCHANGED <<parent, place, scope, rights, dest, source, baseline, final, fs,
                  alive, captured, applied, runs, epoch, generation, present, deleteAck,
                  absentObserved, wrote, edited, lost, badCapture, badEpoch>>

(* Loss revokes the whole subtree, retains slots/payload, never imports. *)
Lose(j) ==
  /\ phase[j] \in Pending
  /\ LET victims == {j} \cup Desc(j)
         active == {k \in victims : phase[k] \in Pending}
     IN /\ phase' = [k \in Jobs |-> IF k \in active THEN "uncertain" ELSE phase[k]]
        /\ cap' = [k \in Jobs |-> IF k \in victims THEN FALSE ELSE cap[k]]
        /\ lost' = lost \cup active
  /\ UNCHANGED <<parent, place, scope, rights, dest, source, baseline, final, fs,
                  alive, captured, applied, held, runs, epoch, generation, present,
                  deleteAck, absentObserved, wrote, edited, badCapture, badEpoch, badComplete>>

Restart ==
  /\ generation < MaxRestarts
  /\ generation' = generation + 1
  /\ phase' = [j \in Jobs |-> IF phase[j] \in Pending THEN "uncertain" ELSE phase[j]]
  /\ cap' = [j \in Jobs |-> FALSE]
  /\ lost' = lost \cup {j \in Jobs : phase[j] \in Pending}
  /\ UNCHANGED <<parent, place, scope, rights, dest, source, baseline, final, fs,
                  alive, captured, applied, held, runs, epoch, present, deleteAck,
                  absentObserved, wrote, edited, badCapture, badEpoch, badComplete>>

RetireUncertain(j) ==
  /\ phase[j] = "uncertain" /\ held[j] /\ absentObserved[j]
  /\ \A k \in Desc(j) : ~held[k]
  /\ held' = [held EXCEPT ![j] = FALSE]
  /\ UNCHANGED <<phase, parent, place, scope, rights, dest, source, baseline, final, fs,
                  alive, captured, applied, runs, cap, epoch, generation, present,
                  deleteAck, absentObserved, wrote, edited, lost,
                  badCapture, badEpoch, badComplete>>

Next ==
  \/ \E j \in Jobs, p \in Jobs \cup {0}, r \in Roots, l \in Places : Stage(j, p, r, l)
  \/ \E j \in Jobs : Start(j) \/ Stop(j) \/ Capture(j) \/ DeleteRequest(j)
                      \/ DeleteLands(j) \/ ObserveAbsent(j) \/ Complete(j)
                      \/ Lose(j) \/ RetireUncertain(j)
  \/ \E j \in Jobs, f \in Files, v \in Data : Write(j, f, v)
  \/ \E r \in Roots, f \in Files, v \in Data : CallerEdit(r, f, v)
  \/ \E j \in Jobs, f \in Files : Apply(j, f)
  \/ Restart

Spec == Init /\ [][Next]_vars

TypeOK ==
  /\ phase \in [Jobs -> Phases] /\ parent \in [Jobs -> Jobs \cup {0}]
  /\ place \in [Jobs -> Places] /\ scope \in [Jobs -> Roots]
  /\ rights \in [Jobs -> SUBSET Roots]
  /\ dest \in [Jobs -> Locs] /\ source \in [Jobs -> Locs]
  /\ fs \in [Locs -> [Files -> Data]]
  /\ baseline \in [Jobs -> [Files -> Data]] /\ final \in [Jobs -> [Files -> Data]]
  /\ applied \in [Jobs -> SUBSET Files]
  /\ alive \in [Jobs -> BOOLEAN] /\ held \in [Jobs -> BOOLEAN]
  /\ captured \in [Jobs -> BOOLEAN] /\ cap \in [Jobs -> BOOLEAN]
  /\ present \in [Jobs -> BOOLEAN] /\ deleteAck \in [Jobs -> BOOLEAN]
  /\ absentObserved \in [Jobs -> BOOLEAN] /\ runs \in [Jobs -> 0..2]
  /\ epoch \in [Jobs -> 0..MaxRestarts] /\ generation \in 0..MaxRestarts
  /\ wrote \subseteq (Jobs \X Files) /\ edited \subseteq (Roots \X Files) /\ lost \subseteq Jobs
  /\ badCapture \in BOOLEAN /\ badEpoch \in BOOLEAN /\ badComplete \in BOOLEAN

ParentBound == \A j \in Jobs : phase[j] # "none" =>
  /\ parent[j] = 0 \/ parent[j] < j
  /\ dest[j] = (IF parent[j] = 0 THEN scope[j] ELSE parent[j])
SourceBound == \A j \in Jobs : phase[j] # "none" =>
  source[j] = (IF parent[j] = 0 THEN scope[j] ELSE parent[j])
Attenuation == \A j \in Jobs : phase[j] # "none" =>
  rights[j] \subseteq (IF parent[j] = 0 THEN {scope[j]} ELSE rights[parent[j]])
StableCapture == ~badCapture
DeltaOnly == \A j \in Jobs : applied[j] \subseteq Delta(j)
NoLateReturn == ~badEpoch
NoReplay == \A j \in Jobs : runs[j] <= 1
VerifiedAbsence == \A j \in Jobs : absentObserved[j] => ~present[j]
ReturnBeforeComplete == ~badComplete
SharedBudget == Slots <= CAP
NoEarlyRelease == \A j \in Jobs :
  phase[j] # "none" /\ ~held[j] => absentObserved[j] /\ phase[j] \in {"done", "uncertain"}
LossIsUncertain == \A j \in lost : phase[j] = "uncertain" /\ ~cap[j]
ParentCaptureSettled == \A j \in Jobs : captured[j] => ReturnsSettled(j)
ReturnLease == \A j \in Jobs : phase[j] \in Pending /\ parent[j] # 0 =>
  ~captured[parent[j]] /\ present[parent[j]]

(* Positive nested config isolates the alternating chain; siblings are in Placement.cfg. *)
NestedOnly == \A j \in Jobs : phase[j] # "none" => parent[j] = j - 1
=============================================================================
