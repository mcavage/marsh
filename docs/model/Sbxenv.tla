----------------------------- MODULE Sbxenv -----------------------------
(* Live additions: each Kit is a workload; the environment files are   *)
(* reloaded while its VM runs. A valid change (Reload) leaves the VM    *)
(* stale until the changed server is registered and loaded; a job never *)
(* launches on a stale VM. An invalid change never reaches the VM and   *)
(* is not modeled. One server per Kit abstracts a bounded list. Project *)
(* includes its split subpath; actual fork/Git paths are checked by     *)
(* Workspace.tla.                                                       *)
EXTENDS Naturals, FiniteSets
CONSTANTS Jobs, Kits, Bug
ASSUME Bug \in {"none", "childKitMount", "splitExtraMount", "skipMcpLoad", "overwriteRegistration", "skipLiveLoad"}
Base == {"project", "home"}
Extra(k) == {k}
VARIABLES vm, registration, loaded, overwritten, active, parent, view, split, stale, staleLaunch
vars == <<vm, registration, loaded, overwritten, active, parent, view, split, stale, staleLaunch>>
Init ==
  /\ vm = [k \in Kits |-> "absent"]
  /\ registration \in [Kits -> {"absent", "matching", "conflicting"}]
  /\ loaded = [k \in Kits |-> FALSE] /\ overwritten = FALSE
  /\ active = {} /\ parent = [j \in Jobs |-> 0]
  /\ view = [j \in Jobs |-> {}] /\ split = [j \in Jobs |-> FALSE]
  /\ stale = [k \in Kits |-> FALSE] /\ staleLaunch = FALSE
Create(k) ==
  /\ vm[k] = "absent" /\ vm' = [vm EXCEPT ![k] = "preparing"]
  /\ UNCHANGED <<registration, loaded, overwritten, active, parent, view, split, stale, staleLaunch>>
Register(k) ==
  /\ vm[k] \in {"preparing", "ready"} /\ ~loaded[k]
  /\ (registration[k] = "absent" \/ (registration[k] = "conflicting" /\ Bug = "overwriteRegistration"))
  /\ overwritten' = (overwritten \/ (registration[k] = "conflicting"))
  /\ registration' = [registration EXCEPT ![k] = "matching"]
  /\ UNCHANGED <<vm, loaded, active, parent, view, split, stale, staleLaunch>>
Load(k) ==
  /\ vm[k] \in {"preparing", "ready"} /\ registration[k] = "matching" /\ ~loaded[k]
  /\ loaded' = [loaded EXCEPT ![k] = TRUE] /\ stale' = [stale EXCEPT ![k] = FALSE]
  /\ UNCHANGED <<vm, registration, overwritten, active, parent, view, split, staleLaunch>>
Ready(k) ==
  /\ vm[k] = "preparing" /\ (loaded[k] \/ Bug = "skipMcpLoad")
  /\ vm' = [vm EXCEPT ![k] = "ready"]
  /\ UNCHANGED <<registration, loaded, overwritten, active, parent, view, split, stale, staleLaunch>>
Remove(k) ==
  /\ vm[k] = "ready" /\ vm' = [vm EXCEPT ![k] = "absent"]
  /\ loaded' = [loaded EXCEPT ![k] = FALSE] /\ stale' = [stale EXCEPT ![k] = FALSE]
  /\ UNCHANGED <<registration, overwritten, active, parent, view, split, staleLaunch>>
Reload(k) ==
  /\ vm[k] = "ready" /\ loaded[k] /\ ~stale[k]
  /\ \E r \in {"absent", "matching"} : registration' = [registration EXCEPT ![k] = r]
  /\ loaded' = [loaded EXCEPT ![k] = FALSE] /\ stale' = [stale EXCEPT ![k] = TRUE]
  /\ UNCHANGED <<vm, overwritten, active, parent, view, split, staleLaunch>>
Launch(j, p, k, branch) ==
  /\ j \notin active /\ vm[k] = "ready" /\ (loaded[k] \/ Bug = "skipLiveLoad")
  /\ staleLaunch' = (staleLaunch \/ stale[k])
  /\ (p = 0 \/ (p \in active /\ p < j))
  /\ active' = active \cup {j} /\ parent' = [parent EXCEPT ![j] = p]
  /\ split' = [split EXCEPT ![j] = branch]
  /\ view' = [view EXCEPT ![j] =
       IF branch THEN Base \cup (IF Bug = "splitExtraMount" THEN Extra(k) ELSE {})
       ELSE IF p = 0 THEN Base \cup Extra(k)
       ELSE IF Bug = "childKitMount" THEN view[p] \cup Extra(k) ELSE view[p]]
  /\ UNCHANGED <<vm, registration, loaded, overwritten, stale>>
Next == (\E k \in Kits : Create(k) \/ Register(k) \/ Load(k) \/ Ready(k) \/ Remove(k) \/ Reload(k))
        \/ (\E j \in Jobs, p \in Jobs \cup {0}, k \in Kits, b \in BOOLEAN : Launch(j, p, k, b))
Spec == Init /\ [][Next]_vars
TypeOK == /\ active \subseteq Jobs /\ view \in [Jobs -> SUBSET (Base \cup Kits)]
          /\ vm \in [Kits -> {"absent", "preparing", "ready"}]
          /\ stale \in [Kits -> BOOLEAN] /\ staleLaunch \in BOOLEAN
NoRegistrationOverwrite == ~overwritten
McpBeforeReady == \A k \in Kits : vm[k] = "ready" => (loaded[k] \/ stale[k])
LiveLoadBeforeLaunch == ~staleLaunch
ExtraAttenuation == \A j \in active : parent[j] # 0 => view[j] \subseteq view[parent[j]]
SplitOmitsExtras == \A j \in active : split[j] => view[j] \cap Kits = {}
=============================================================================
