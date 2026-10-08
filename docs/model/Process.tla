----------------------------- MODULE Process -----------------------------
(* One lineage tree for every job (docs/design/processes.md).                    *)
(*                                                                       *)
(* The daemon is the kernel. The user's session (id 0) launches top-level *)
(* jobs; a running job with a live capability launches children through  *)
(* its job capability (`marsh run`, a registered-name shim, or a split    *)
(* branch, which is a child with a narrowed view). Every launch is        *)
(* journaled with its parent before it is submitted, draws from one       *)
(* session pool, and is checked against depth, fan-out and per-tree total *)
(* caps. A child's rights (mounts and spawnable Kits) are a subset of its  *)
(* parent's; it may narrow further (a split fork, or no spawn at all).    *)
(* A job's end, cancel, or transport loss revokes its capability and      *)
(* cancels every descendant. A pool slot frees only on a confirmed end    *)
(* (container verified deleted). A daemon restart revokes every           *)
(* capability and makes every unfinished job uncertain; nothing replays. *)
(*                                                                       *)
(* Job: none -> launched -> running -> ending -> done                     *)
(*      launched/running -> cancelling -> done | unreaped                 *)
(*      launched/running/ending/cancelling -> uncertain (loss, restart)   *)
(*                                                                       *)
(* Entry: every job's first step is its entrypoint invoking the job's   *)
(* own command. A script entrypoint execs it by name through PATH, which *)
(* reaches the marsh link; an ELF entrypoint is started by absolute path *)
(* from the image config. The worker sets MARSH_ENTRY for every          *)
(* entrypoint; the link, invoked as the own name with the marker set and *)
(* its parent = docker-init, clears the marker, and execs the image     *)
(* binary by absolute path (PATH stays intact). Any other own-name call *)
(* (an agent's tool call) is a child job of the same Kit, under a        *)
(* same-Kit chain cap. Any job may spawn any Kit in its spawn set     *)
(* (every registered Kit by default; the child runs under its own Kit's *)
(* egress and credentials); a launcher may only narrow the set, never   *)
(* widen it.                                                             *)
(* Assumption (A6, not modeled): no orphan reparented to docker-init     *)
(* carries the marker; if one does, it runs its own image binary in its  *)
(* own container (lineage miss, no authority change).                   *)
(*                                                                       *)
(* A tree uses at most MaxKitVMs distinct Kits (one warm VM each) among  *)
(* its live jobs at once; a child of a Kit already live in it is free.  *)
(* Wall time is not modeled: a deadline is the environment's Cancel.     *)
(*                                                                       *)
(* Environment: the user (or a deadline) cancels any job; transport loss *)
(* on any job's VM; leftover processes die; a daemon crash between any   *)
(* two daemon steps; jobs exit whenever they like.                       *)
EXTENDS Naturals, FiniteSets

CONSTANTS Jobs, Kits, ScriptKits, MaxDepth, MaxConc, MaxFan, MaxTotal,
          MaxSame, MaxKitVMs, MaxRestarts, Bug

Bugs == {"none", "noDepth", "widen", "freeOnRevoke", "noFan", "noTotal",
         "orphanChildren", "revokeOnly", "noPropagate", "restartKeepsCap",
         "keepRunning", "replay", "linkIgnoresMarker", "linkIgnoresPpid",
         "noSameKit", "spawnWiden", "noKitCap"}
ASSUME Bug \in Bugs
ASSUME Jobs \subseteq (Nat \ {0}) /\ ScriptKits \subseteq Kits

Session == 0
\* Rights: "proj" = the project/home view, "fork" = a split fork view,
\* and one right per Kit the job may spawn. The session holds all of them.
AllRights == {"proj", "fork"} \cup Kits
\* What a launcher may hand a child: the same rights, the fork view only
\* (a split branch), no spawn at all (`--no-spawn`), or a smaller spawn
\* set (`--spawn` / MARSH_SPAWN without one Kit).
Narrowings(R) == {R, R \ {"proj"}, R \ Kits} \cup {R \ {c} : c \in Kits}
View == {"proj", "fork"}

Active == {"launched", "running", "ending", "cancelling"}
Holding == Active \cup {"unreaped", "uncertain"}

VARIABLES
  st,        \* [Jobs -> job state]
  par,       \* [Jobs -> Session | Jobs] lineage parent (journaled at launch)
  kit,       \* [Jobs -> Kits \cup {"none"}]
  rights,    \* [Jobs -> SUBSET AllRights]
  cap,       \* [Jobs -> "none" | "live" | "revoked"]
  cgen,      \* [Jobs -> Nat] daemon generation that issued the job/cap
  alive,     \* [Jobs -> BOOLEAN] a container of the job may run
  launches,  \* [Jobs -> Nat] submissions to a worker
  runs,      \* [Jobs -> Nat] container starts
  cancelled, \* ghost: jobs named by a cancel
  restarts,  \* daemon generation
  entry,     \* [Jobs -> "pending" | "done"] the entrypoint's own-name step
  marker,    \* [Jobs -> BOOLEAN] MARSH_ENTRY in the main process's environment
  selfLocal, \* [Jobs -> Nat] own-name calls resolved to the image binary
  viaEntry   \* [Jobs -> BOOLEAN] ghost: created by its parent's entry step

vars == <<st, par, kit, rights, cap, cgen, alive, launches, runs, cancelled, restarts,
          entry, marker, selfLocal, viaEntry>>
sv == <<entry, marker, selfLocal, viaEntry>>

Init ==
  /\ st = [j \in Jobs |-> "none"]
  /\ par = [j \in Jobs |-> Session]
  /\ kit = [j \in Jobs |-> "none"]
  /\ rights = [j \in Jobs |-> {}]
  /\ cap = [j \in Jobs |-> "none"]
  /\ cgen = [j \in Jobs |-> 0]
  /\ alive = [j \in Jobs |-> FALSE]
  /\ launches = [j \in Jobs |-> 0]
  /\ runs = [j \in Jobs |-> 0]
  /\ cancelled = {}
  /\ restarts = 0
  /\ entry = [j \in Jobs |-> "pending"]
  /\ marker = [j \in Jobs |-> FALSE]
  /\ selfLocal = [j \in Jobs |-> 0]
  /\ viaEntry = [j \in Jobs |-> FALSE]

---------------------------------------------------------------------------
(* Helpers *)

RECURSIVE AncN(_, _)
AncN(j, n) == IF n = 0 \/ par[j] = Session THEN {} ELSE {par[j]} \cup AncN(par[j], n - 1)
Anc(j) == AncN(j, Cardinality(Jobs))
Depth(j) == Cardinality(Anc(j)) + 1
Desc(j) == {d \in Jobs : st[d] # "none" /\ j \in Anc(d)}
RootOf(j) == IF par[j] = Session THEN j ELSE CHOOSE r \in Anc(j) : par[r] = Session
Tree(r) == {r} \cup Desc(r)
Children(p) == {c \in Jobs : st[c] # "none" /\ par[c] = p}
\* Length of the same-Kit ancestor chain ending at j (j included).
RECURSIVE SameN(_, _)
SameN(j, n) == IF n = 0 \/ par[j] = Session \/ kit[par[j]] # kit[j] THEN 1
               ELSE 1 + SameN(par[j], n - 1)
SameChain(j) == SameN(j, Cardinality(Jobs))
\* Distinct Kits (warm VMs) of a tree's live jobs.
TreeKits(r) == {kit[d] : d \in {x \in Tree(r) : st[x] \in Active}}

\* A pool slot is held until the job's end is confirmed. Bug freeOnRevoke:
\* the daemon stops counting a job once its capability is revoked.
InPool(j) == st[j] \in Holding /\ (Bug # "freeOnRevoke" \/ cap[j] = "live")
PoolUse == Cardinality({j \in Jobs : InPool(j)})

\* Revoke and signal a set of jobs: launched or running jobs move to
\* cancelling (INT, 10 s, KILL). Jobs already ending or cancelling stay.
CancelState(T) ==
  [d \in Jobs |-> IF d \in T /\ st[d] \in {"launched", "running"} THEN "cancelling" ELSE st[d]]
RevokeCaps(T) ==
  [d \in Jobs |-> IF d \in T /\ cap[d] = "live" THEN "revoked" ELSE cap[d]]

\* The cascade a job's end (exit, loss) applies to its descendants. Bug
\* orphanChildren: none. Bug revokeOnly: their capabilities are revoked
\* but their containers are not signalled.
Cascade(j, self) ==
  LET T == IF Bug = "orphanChildren" THEN {} ELSE Desc(j) IN
  /\ cap' = RevokeCaps(T \cup {j})
  /\ st' = [d \in Jobs |-> IF d = j THEN self
                           ELSE IF Bug = "revokeOnly" THEN st[d]
                           ELSE CancelState(T)[d]]

---------------------------------------------------------------------------
(* Daemon *)

\* Run (or a split branch, or a top-level job from the session): admission
\* and the journal record (parent, kit, rights) before submission, under
\* the lock that also re-checks the parent's capability. A link (from any
\* of the image's own shells or programs) or the in-job `marsh run` reaches
\* this for any registered name, the job's own included.
\* `fromEntry`: the parent's entry step dispatched its own name.
SpawnFrom(j, p, fromEntry) ==
  /\ st[j] = "none"
  /\ \A i \in Jobs : i < j => st[i] # "none"                 \* ids in creation order
  /\ IF p = Session THEN TRUE
     ELSE /\ p \in Jobs /\ p < j
          /\ st[p] = "running" /\ alive[p] /\ cap[p] = "live"
          /\ (entry[p] = "done") # fromEntry                  \* entry step first
          /\ (Bug = "noDepth" \/ Depth(p) + 1 <= MaxDepth)
          /\ (Bug = "noFan" \/ Cardinality({c \in Children(p) : st[c] \in Holding}) < MaxFan)
          /\ (Bug = "noTotal" \/ Cardinality(Tree(RootOf(p))) < MaxTotal)
  /\ PoolUse < MaxConc
  /\ LET R == IF p = Session THEN AllRights ELSE rights[p] IN
     \E k \in Kits, N \in Narrowings(R) :
       /\ k \in R \/ Bug = "widen"
       /\ fromEntry => k = kit[p]
       /\ p # Session /\ k = kit[p] =>
            (Bug = "noSameKit" \/ SameChain(p) + 1 <= MaxSame)
       \* Bug noKitCap: a child may bring one more Kit VM into a full tree.
       /\ p # Session =>
            (Bug = "noKitCap" \/ k \in TreeKits(RootOf(p))
             \/ Cardinality(TreeKits(RootOf(p))) < MaxKitVMs)
       /\ kit' = [kit EXCEPT ![j] = k]
       \* The child's rights are the launcher's recorded rights, narrowed.
       \* Bug widen: the view is recomputed from the session's grants. Bug
       \* spawnWiden: the spawn set is reset to every registered Kit.
       /\ rights' = [rights EXCEPT ![j] =
            IF p = Session THEN N
            ELSE IF Bug = "widen" THEN View \cup (N \cap Kits)
            ELSE IF Bug = "spawnWiden" THEN (N \cap View) \cup Kits
            ELSE N]
  /\ par' = [par EXCEPT ![j] = p]
  /\ st' = [st EXCEPT ![j] = "launched"]
  /\ cap' = [cap EXCEPT ![j] = "live"]
  /\ cgen' = [cgen EXCEPT ![j] = restarts]
  /\ launches' = [launches EXCEPT ![j] = @ + 1]
  /\ marker' = [marker EXCEPT ![j] = TRUE]                    \* every entrypoint
  /\ viaEntry' = [viaEntry EXCEPT ![j] = fromEntry]
  /\ entry' = IF fromEntry THEN [entry EXCEPT ![p] = "done"] ELSE entry
  /\ UNCHANGED <<alive, runs, cancelled, restarts, selfLocal>>

Spawn(j, p) == SpawnFrom(j, p, FALSE)

\* The link's own-name decision: image binary iff the marker is set and
\* the caller's parent is docker-init (true only for the entrypoint's own
\* exec, by A6). Bug linkIgnoresMarker: the marker is not consulted.
\* Bug linkIgnoresPpid: the parent is not checked.
LinkLocal(j, ppidInit) ==
  /\ marker[j] /\ Bug # "linkIgnoresMarker"
  /\ ppidInit \/ Bug = "linkIgnoresPpid"

\* The entry step: an ELF entrypoint is the image binary, started by
\* absolute path (the marker stays in its environment). A script
\* entrypoint execs the own name through the link, which clears the marker
\* and execs the image binary.
EntryLocal(j) ==
  /\ st[j] = "running" /\ alive[j] /\ entry[j] = "pending"
  /\ kit[j] \notin ScriptKits \/ LinkLocal(j, TRUE)
  /\ entry' = [entry EXCEPT ![j] = "done"]
  /\ selfLocal' = [selfLocal EXCEPT ![j] = @ + 1]
  /\ marker' = [marker EXCEPT ![j] = IF kit[j] \in ScriptKits THEN FALSE ELSE @]
  /\ UNCHANGED <<st, par, kit, rights, cap, cgen, alive, launches, runs, cancelled,
                 restarts, viaEntry>>

\* Otherwise the link dispatches the script entrypoint's own name as a
\* child job of the same Kit (whose entrypoint does the same).
EntryDispatch(c, j) ==
  /\ st[j] = "running" /\ alive[j] /\ entry[j] = "pending"
  /\ kit[j] \in ScriptKits /\ ~LinkLocal(j, TRUE)
  /\ SpawnFrom(c, j, TRUE)

\* A later own-name call from inside the job (an agent's tool call; its
\* parent is never docker-init by A6) resolves locally only if the link
\* says so; otherwise it is Spawn(c, j) with kit[j].
AgentSelfLocal(j) ==
  /\ st[j] = "running" /\ alive[j] /\ entry[j] = "done"
  /\ LinkLocal(j, FALSE)
  /\ selfLocal' = [selfLocal EXCEPT ![j] = @ + 1]
  /\ UNCHANGED <<st, par, kit, rights, cap, cgen, alive, launches, runs, cancelled,
                 restarts, entry, marker, viaEntry>>

\* The worker starts the container (possibly after a cancel was sent).
Start(j) ==
  /\ st[j] \in {"launched", "cancelling"} /\ runs[j] < launches[j]
  /\ alive' = [alive EXCEPT ![j] = TRUE]
  /\ runs' = [runs EXCEPT ![j] = @ + 1]
  /\ st' = [st EXCEPT ![j] = IF @ = "launched" THEN "running" ELSE @]
  /\ UNCHANGED <<sv, par, kit, rights, cap, cgen, launches, cancelled, restarts>>

\* The job's main process exits: capability revoked, descendants cancelled.
Exit(j) ==
  /\ st[j] = "running" /\ alive[j] /\ entry[j] = "done"
  /\ Cascade(j, "ending")
  /\ UNCHANGED <<sv, par, kit, rights, cgen, alive, launches, runs, cancelled, restarts>>

\* The worker deletes the container and verifies its absence.
Cleanup(j) ==
  /\ st[j] = "ending"
  /\ st' = [st EXCEPT ![j] = "done"]
  /\ alive' = [alive EXCEPT ![j] = FALSE]
  /\ UNCHANGED <<sv, par, kit, rights, cap, cgen, launches, runs, cancelled, restarts>>

\* Cancel from the session (Ctrl-C in the root shell, the 1 h deadline,
\* session end: any job of the tree) or from the parent (it dropped the
\* child's Run stream, or its own `marsh run` got Ctrl-C). Bug noPropagate:
\* only the named job.
Cancel(j, a) ==
  /\ st[j] \in {"launched", "running"}
  /\ IF a = Session THEN TRUE
     ELSE a = par[j] /\ st[a] = "running" /\ cap[a] = "live"
  /\ LET T == IF Bug = "noPropagate" THEN {j} ELSE {j} \cup Desc(j) IN
     /\ cap' = RevokeCaps(T)
     /\ st' = CancelState(T)
  /\ cancelled' = cancelled \cup {j}
  /\ UNCHANGED <<sv, par, kit, rights, cgen, alive, launches, runs, restarts>>

Reap(j) ==
  /\ st[j] = "cancelling"
  /\ st' = [st EXCEPT ![j] = "done"]
  /\ alive' = [alive EXCEPT ![j] = FALSE]
  /\ UNCHANGED <<sv, par, kit, rights, cap, cgen, launches, runs, cancelled, restarts>>

GraceExpire(j) ==
  /\ st[j] = "cancelling"
  /\ st' = [st EXCEPT ![j] = "unreaped"]
  /\ UNCHANGED <<sv, par, kit, rights, cap, cgen, alive, launches, runs, cancelled, restarts>>

\* Worker transport loss on the job's VM: uncertain, VM quarantined, never
\* re-run; its descendants (in other containers) are cancelled.
Lose(j) ==
  /\ st[j] \in Active
  /\ Cascade(j, "uncertain")
  /\ UNCHANGED <<sv, par, kit, rights, cgen, alive, launches, runs, cancelled, restarts>>

\* Daemon crash + restart between any two steps. The journal survives:
\* every unfinished job becomes uncertain and every capability is revoked.
\* Bug restartKeepsCap: capabilities survive. Bug keepRunning: jobs keep
\* their state. Bug replay: submitted jobs are submitted again.
Restart ==
  /\ restarts < MaxRestarts
  /\ restarts' = restarts + 1
  /\ cap' = [j \in Jobs |-> IF cap[j] = "live" /\ Bug # "restartKeepsCap" THEN "revoked" ELSE cap[j]]
  /\ st' = [j \in Jobs |->
              IF st[j] \notin Active THEN st[j]
              ELSE IF Bug = "keepRunning" THEN st[j]
              ELSE IF Bug = "replay" /\ st[j] \in {"launched", "running"} THEN "launched"
              ELSE "uncertain"]
  /\ launches' = [j \in Jobs |-> IF Bug = "replay" /\ st[j] \in {"launched", "running"}
                                   THEN launches[j] + 1 ELSE launches[j]]
  /\ UNCHANGED <<sv, par, kit, rights, cgen, alive, runs, cancelled>>

---------------------------------------------------------------------------
(* Environment *)

\* A leftover container (unreaped, or on a quarantined VM) ends unobserved.
ProcDie(j) ==
  /\ alive[j] /\ st[j] \in {"unreaped", "uncertain"}
  /\ alive' = [alive EXCEPT ![j] = FALSE]
  /\ UNCHANGED <<sv, st, par, kit, rights, cap, cgen, launches, runs, cancelled, restarts>>

Next ==
  \/ Restart
  \/ \E j \in Jobs :
       \/ \E p \in {Session} \cup Jobs : Spawn(j, p)
       \/ Start(j) \/ Exit(j) \/ Cleanup(j) \/ Reap(j) \/ GraceExpire(j)
       \/ Lose(j) \/ ProcDie(j) \/ EntryLocal(j) \/ AgentSelfLocal(j)
       \/ \E c \in Jobs : EntryDispatch(c, j)
       \/ \E a \in {Session} \cup Jobs : Cancel(j, a)

Fairness ==
  \A j \in Jobs : /\ WF_vars(Start(j)) /\ WF_vars(Exit(j)) /\ WF_vars(Cleanup(j))
                  /\ WF_vars(EntryLocal(j) \/ \E c \in Jobs : EntryDispatch(c, j))
                  /\ WF_vars(Reap(j) \/ GraceExpire(j))

Spec == Init /\ [][Next]_vars /\ Fairness

---------------------------------------------------------------------------
(* Invariants. Each negative control removes one guard in a real action. *)

TypeOK ==
  /\ st \in [Jobs -> {"none", "done", "unreaped", "uncertain"} \cup Active]
  /\ par \in [Jobs -> {Session} \cup Jobs]
  /\ kit \in [Jobs -> Kits \cup {"none"}]
  /\ entry \in [Jobs -> {"pending", "done"}]
  /\ rights \in [Jobs -> SUBSET AllRights]
  /\ cap \in [Jobs -> {"none", "live", "revoked"}]

\* Lineage is a forest rooted at the session, created parent-first, depth-capped.
LineageWF == \A j \in Jobs : st[j] # "none" =>
  /\ (par[j] # Session => par[j] < j /\ st[par[j]] # "none")
  /\ Depth(j) <= MaxDepth

\* Confinement never widens: a child's view is within its parent's, and
\* its Kit was in the parent's spawn set.
Attenuation == \A j \in Jobs : st[j] # "none" /\ par[j] # Session =>
  /\ rights[j] \cap View \subseteq rights[par[j]]
  /\ kit[j] \in rights[par[j]]

\* The spawn set never widens down the tree.
SpawnSetAttenuates == \A j \in Jobs : st[j] # "none" /\ par[j] # Session =>
  rights[j] \cap Kits \subseteq rights[par[j]] \cap Kits

\* Containers that may exist stay within the session pool.
BudgetBound == Cardinality({j \in Jobs : alive[j]}) <= MaxConc

FanOutBound == \A p \in Jobs : Cardinality({c \in Children(p) : st[c] \in Holding}) <= MaxFan

TotalBound == \A r \in Jobs : st[r] # "none" /\ par[r] = Session => Cardinality(Tree(r)) <= MaxTotal

\* Revocation on parent end: no live capability under a revoked one.
LiveUnderLive == \A j \in Jobs :
  (cap[j] = "live" /\ par[j] # Session) => cap[par[j]] = "live"

\* No orphan: a launched or running job's parent job is running.
NoOrphanRunning == \A j \in Jobs :
  (st[j] \in {"launched", "running"} /\ par[j] # Session) => st[par[j]] = "running"

\* A cancel reaches every descendant.
CancelPropagates == \A j \in cancelled : \A d \in {j} \cup Desc(j) :
  cap[d] # "live" /\ st[d] \notin {"launched", "running"}

\* Every live capability was issued by the current daemon generation.
RevokedAtRestart == \A j \in Jobs : cap[j] = "live" => cgen[j] = restarts

\* After a restart nothing from an earlier generation is still active.
RestartUncertain == \A j \in Jobs : st[j] \in Active => cgen[j] = restarts

NoReplay == \A j \in Jobs : runs[j] <= 1

\* The job's own command resolves to the image binary at most once per
\* job (its main process); every later own-name call is a child job.
EntryOnce == \A j \in Jobs : selfLocal[j] <= 1

\* An entrypoint never recurses into a child of its own Kit.
NoEntryRecursion == \A j \in Jobs :
  (st[j] # "none" /\ par[j] # Session /\ kit[j] = kit[par[j]]) => ~viaEntry[j]

\* Same-Kit chains (claude -> claude -> ...) are capped.
SameKitBound == \A j \in Jobs : st[j] # "none" => SameChain(j) <= MaxSame

\* A tree's live jobs use at most MaxKitVMs distinct Kit VMs.
KitVMBound == \A r \in Jobs : st[r] # "none" /\ par[r] = Session =>
  Cardinality(TreeKits(r)) <= MaxKitVMs

(* Liveness: every job eventually ends (done) or is reported (unreaped, uncertain). *)
EventuallySettled == <>[](\A j \in Jobs : st[j] \in {"none", "done", "unreaped", "uncertain"})
=============================================================================
