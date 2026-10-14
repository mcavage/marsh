----------------------------- MODULE Sbxenv -----------------------------
(* Snapshot additions: each Kit is a workload + environment selector.   *)
(* One server per Kit abstracts a bounded list. Project includes its    *)
(* split subpath; actual fork/Git paths are checked by Workspace.tla.   *)
EXTENDS Naturals, FiniteSets
CONSTANTS Jobs, Kits, Bug
ASSUME Bug \in {"none", "childKitMount", "splitExtraMount", "skipMcpLoad", "overwriteRegistration"}
Base == {"project", "home"}
Extra(k) == {k}
VARIABLES vm, registration, loaded, overwritten, active, parent, view, split
vars == <<vm, registration, loaded, overwritten, active, parent, view, split>>
Init ==
  /\ vm = [k \in Kits |-> "absent"]
  /\ registration \in [Kits -> {"absent", "matching", "conflicting"}]
  /\ loaded = [k \in Kits |-> FALSE] /\ overwritten = FALSE
  /\ active = {} /\ parent = [j \in Jobs |-> 0]
  /\ view = [j \in Jobs |-> {}] /\ split = [j \in Jobs |-> FALSE]
Create(k) ==
  /\ vm[k] = "absent" /\ vm' = [vm EXCEPT ![k] = "preparing"]
  /\ UNCHANGED <<registration, loaded, overwritten, active, parent, view, split>>
Register(k) ==
  /\ vm[k] = "preparing"
  /\ (registration[k] = "absent" \/ (registration[k] = "conflicting" /\ Bug = "overwriteRegistration"))
  /\ overwritten' = (overwritten \/ (registration[k] = "conflicting"))
  /\ registration' = [registration EXCEPT ![k] = "matching"]
  /\ UNCHANGED <<vm, loaded, active, parent, view, split>>
Load(k) ==
  /\ vm[k] = "preparing" /\ registration[k] = "matching" /\ ~loaded[k]
  /\ loaded' = [loaded EXCEPT ![k] = TRUE]
  /\ UNCHANGED <<vm, registration, overwritten, active, parent, view, split>>
Ready(k) ==
  /\ vm[k] = "preparing" /\ (loaded[k] \/ Bug = "skipMcpLoad")
  /\ vm' = [vm EXCEPT ![k] = "ready"]
  /\ UNCHANGED <<registration, loaded, overwritten, active, parent, view, split>>
Remove(k) ==
  /\ vm[k] = "ready" /\ vm' = [vm EXCEPT ![k] = "absent"]
  /\ loaded' = [loaded EXCEPT ![k] = FALSE]
  /\ UNCHANGED <<registration, overwritten, active, parent, view, split>>
Launch(j, p, k, branch) ==
  /\ j \notin active /\ vm[k] = "ready" /\ (p = 0 \/ (p \in active /\ p < j))
  /\ active' = active \cup {j} /\ parent' = [parent EXCEPT ![j] = p]
  /\ split' = [split EXCEPT ![j] = branch]
  /\ view' = [view EXCEPT ![j] =
       IF branch THEN Base \cup (IF Bug = "splitExtraMount" THEN Extra(k) ELSE {})
       ELSE IF p = 0 THEN Base \cup Extra(k)
       ELSE IF Bug = "childKitMount" THEN view[p] \cup Extra(k) ELSE view[p]]
  /\ UNCHANGED <<vm, registration, loaded, overwritten>>
Next == (\E k \in Kits : Create(k) \/ Register(k) \/ Load(k) \/ Ready(k) \/ Remove(k))
        \/ (\E j \in Jobs, p \in Jobs \cup {0}, k \in Kits, b \in BOOLEAN : Launch(j, p, k, b))
Spec == Init /\ [][Next]_vars
TypeOK == /\ active \subseteq Jobs /\ view \in [Jobs -> SUBSET (Base \cup Kits)]
          /\ vm \in [Kits -> {"absent", "preparing", "ready"}]
NoRegistrationOverwrite == ~overwritten
McpBeforeReady == \A k \in Kits : vm[k] = "ready" => loaded[k]
ExtraAttenuation == \A j \in active : parent[j] # 0 => view[j] \subseteq view[parent[j]]
SplitOmitsExtras == \A j \in active : split[j] => view[j] \cap Kits = {}
=============================================================================
