---------------------------- MODULE Workspace ----------------------------
(* Daemon-owned workspaces and split lineage, v1 (docs/design/workspaces.md).   *)
(*                                                                       *)
(* The host daemon journals a split, snapshots the caller's tree (the    *)
(* user's tree for a session, the parent branch's fork for a nested      *)
(* split) into a frozen base and a per-split object store, clones one   *)
(* fork per branch (all splits are siblings under <root>/.marsh/split),  *)
(* and runs every branch: a shell branch (trusted, the user's own code,  *)
(* writes its cwd: C1) in the shell VM, or an argv branch as a Kit       *)
(* container that writes anything it has mounted read/write. Nested      *)
(* splits have argv branches only. Admission draws from one session     *)
(* pool. A branch's exit revokes its capability and cancels its subtree. *)
(* Capture waits until every writer is confirmed gone and rejects a fork *)
(* whose git metadata changed or that grew a nested `.git`. Only the     *)
(* creator joins and releases; on lease expiry the split is kept and     *)
(* still joinable. A daemon restart makes unfinished splits uncertain;  *)
(* nothing is replayed.                                                  *)
(*                                                                       *)
(* Split:  none -> run -> await -> done | kept -> done                   *)
(*         run -> cancel -> done ; run/await/cancel -> uncertain         *)
(* Branch: none -> ready -> running -> exited -> captured | rejected     *)
(*         ready -> captured (refused: pool full, fails visibly)         *)
(*         running/exited -> cancelling -> reaped | unreaped             *)
(*         running/exited/cancelling -> uncertain (daemon restart)       *)
(* Fork:   none -> alloc -> removed | retained                           *)
(*                                                                       *)
(* Environment: user edits, adversarial Kit writes to anything mounted   *)
(* (fork, a nested .git in it, and whatever a bug mounts), join/release/ *)
(* cancel requests from any actor naming any split, background processes *)
(* outliving their branch, a daemon crash between any two daemon steps. *)
EXTENDS Naturals, FiniteSets

CONSTANTS Splits, Labels, KitLabels, MaxDepth, MaxConc, MaxVer, MaxRestarts, Bug

Bugs == {"none", "objectsRW", "adminRW", "noCaptureCheck", "liveFork",
         "childFromUser", "nestedFork", "wholeProject", "ancestorJoin",
         "captureEarly", "noDepth", "freeOnRevoke", "orphanChildren",
         "noPropagate", "unscopedCancel", "removeLive", "replay", "noIntent",
         "noLease", "forgetUncertain"}
ASSUME Bug \in Bugs
ASSUME Splits \subseteq (Nat \ {0}) /\ KitLabels \subseteq Labels

Branches == Splits \X Labels
None == <<0, "none">>
Root == <<0, "root">>
Callers == {Root} \cup Branches
\* Locations (3-tuples so TLC can compare them) and actors.
UserTree == <<"user", 0, "u">>
Fork(b) == <<"fork", b[1], b[2]>>
ForkGit(b) == <<"forkgit", b[1], b[2]>>  \* a `.git` entry created inside the fork
Admin(b) == <<"admin", b[1], b[2]>>      \* gitfile, config, HEAD, info, packed-refs
Objs(s) == <<"objs", s, "o">>            \* the split's store.git
Locs == {UserTree} \cup {Objs(s) : s \in Splits}
        \cup {Fork(b) : b \in Branches} \cup {ForkGit(b) : b \in Branches}
        \cup {Admin(b) : b \in Branches}
UserA == <<"user", 0, "u">>
Br(b) == <<"br", b[1], b[2]>>
Join(s) == <<"join", s, "j">>

SplitActive == {"run", "await", "cancel"}

VARIABLES
  sst,      \* [Splits -> split state]
  par,      \* [Splits -> None | Root | Branches] creator (lineage parent)
  rec,      \* [Splits -> BOOLEAN] journal record exists
  srcLoc,   \* [Splits -> Locs] tree the snapshot was taken from
  snapVer,  \* [Splits -> Nat] version of that tree at the snapshot
  cancelBy, \* [Splits -> None | Root | Branches] ghost: who cancelled
  bst,      \* [Branches -> branch state]
  alive,    \* [Branches -> BOOLEAN] some process of the branch may run
  launches, \* [Branches -> Nat] journaled launches
  runs,     \* [Branches -> Nat] process starts
  wrote,    \* [Branches -> BOOLEAN] the branch has written (bounds state)
  base,     \* [Branches -> Nat] version the fork was cloned from
  res,      \* [Branches -> Nat] fork version captured
  ws,       \* [Branches -> "none" | "alloc" | "removed" | "retained"]
  cap,      \* [Branches -> "none" | "live" | "revoked"]
  ver,      \* [Locs -> Nat] versions of trees
  W,        \* [Locs -> SUBSET actors] ghost: who wrote each location
  restarts

vars == <<sst, par, rec, srcLoc, snapVer, cancelBy, bst, alive, launches, runs,
          wrote, base, res, ws, cap, ver, W, restarts>>

Init ==
  /\ sst = [s \in Splits |-> "none"]
  /\ par = [s \in Splits |-> None]
  /\ rec = [s \in Splits |-> FALSE]
  /\ srcLoc = [s \in Splits |-> UserTree]
  /\ snapVer = [s \in Splits |-> 0]
  /\ cancelBy = [s \in Splits |-> None]
  /\ bst = [b \in Branches |-> "none"]
  /\ alive = [b \in Branches |-> FALSE]
  /\ launches = [b \in Branches |-> 0]
  /\ runs = [b \in Branches |-> 0]
  /\ wrote = [b \in Branches |-> FALSE]
  /\ base = [b \in Branches |-> 0]
  /\ res = [b \in Branches |-> 0]
  /\ ws = [b \in Branches |-> "none"]
  /\ cap = [b \in Branches |-> "none"]
  /\ ver = [l \in Locs |-> 0]
  /\ W = [l \in Locs |-> {}]
  /\ restarts = 0

---------------------------------------------------------------------------
(* Helpers *)

BOf(s) == {s} \X Labels
\* Argv (Kit) branch: by label for root splits; every branch of a nested split.
Kit(b) == b[2] \in KitLabels \/ par[b[1]] \in Branches
RECURSIVE AncB(_, _)
\* Ancestor branches of split s (its creator branch, that split's creator, ...).
AncB(s, n) == IF n = 0 \/ par[s] \notin Branches THEN {}
              ELSE {par[s]} \cup AncB(par[s][1], n - 1)
Anc(s) == AncB(s, Cardinality(Splits))
Depth(s) == Cardinality(Anc(s)) + 1
UnderBranch(c) == {t \in Splits : c \in Anc(t)}
Below(s) == {s} \cup UNION {UnderBranch(b) : b \in BOf(s)}
ChildBranches(p) == {b \in Branches : par[b[1]] = p}

\* The daemon's view: a branch occupies a pool slot until its end is
\* confirmed (captured/rejected: every writer gone; reaped). Bug
\* freeOnRevoke: the slot is freed when the capability is revoked.
Holding(b) == bst[b] \in {"running", "exited", "cancelling", "unreaped", "uncertain"}
InPool(b) == Holding(b) /\ (Bug # "freeOnRevoke" \/ cap[b] = "live")
PoolUse == Cardinality({b \in Branches : InPool(b)})

\* Read/write mounts of an argv (Kit) branch's container. Git admin files
\* are bind-mounted read-only; only the worktree (and objects/, index,
\* folded into the fork here) is writable.
Mounts(b) == {Fork(b), ForkGit(b)}
             \cup (IF Bug = "adminRW" THEN {Admin(b)} ELSE {})
             \cup (IF Bug = "wholeProject" THEN {UserTree} ELSE {})
             \cup (IF Bug = "nestedFork" THEN {Fork(c) : c \in ChildBranches(b)} ELSE {})
             \cup (IF Bug = "objectsRW" THEN {Objs(b[1])} ELSE {})
\* A trusted shell branch writes below its cwd (C1).
ShellWrites(b) == {Fork(b), ForkGit(b)}

WriteLoc(l, a) ==
  /\ ver' = [ver EXCEPT ![l] = IF @ < MaxVer THEN @ + 1 ELSE @]
  /\ W' = [W EXCEPT ![l] = @ \cup {a}]

Tampered(b) == W[Admin(b)] # {} \/ W[ForkGit(b)] # {}

\* Cancel every split in T: revoke, signal, settle unstarted branches; an
\* awaiting split loses its consumer and is kept (still joinable).
CancelSet(T) ==
  LET B == UNION {BOf(t) : t \in T} IN
  /\ sst' = [t \in Splits |-> IF t \in T /\ sst[t] = "run" THEN "cancel"
                              ELSE IF t \in T /\ sst[t] = "await" THEN "kept"
                              ELSE sst[t]]
  /\ cap' = [b \in Branches |-> IF b \in B /\ cap[b] = "live" THEN "revoked" ELSE cap[b]]
  /\ bst' = [b \in Branches |->
               IF b \notin B \/ sst[b[1]] # "run" THEN bst[b]
               ELSE IF bst[b] = "running" \/ (bst[b] = "exited" /\ alive[b]) THEN "cancelling"
               ELSE IF bst[b] \in {"none", "ready", "exited"} THEN "reaped"
               ELSE bst[b]]
  /\ ws' = [b \in Branches |-> IF b[1] \in T /\ sst[b[1]] = "await" /\ ws[b] = "alloc"
                                 THEN "retained" ELSE ws[b]]

Settled(b) == ws[b] \in {"none", "removed", "retained"}

---------------------------------------------------------------------------
(* Environment *)

UserEdit ==
  /\ UserA \notin W[UserTree]
  /\ WriteLoc(UserTree, UserA)
  /\ UNCHANGED <<sst, par, rec, srcLoc, snapVer, cancelBy, bst, alive, launches,
                 runs, wrote, base, res, ws, cap, restarts>>

\* A leftover process (background job, unreaped or uncertain container) ends.
ProcDie(b) ==
  /\ alive[b] /\ bst[b] \in {"exited", "unreaped", "uncertain"}
  /\ alive' = [alive EXCEPT ![b] = FALSE]
  /\ UNCHANGED <<sst, par, rec, srcLoc, snapVer, cancelBy, bst, launches, runs,
                 wrote, base, res, ws, cap, ver, W, restarts>>

Write(b, l) ==
  /\ alive[b] /\ ~wrote[b]
  /\ l \in (IF Kit(b) THEN Mounts(b) ELSE ShellWrites(b))
  /\ wrote' = [wrote EXCEPT ![b] = TRUE]
  /\ WriteLoc(l, Br(b))
  /\ UNCHANGED <<sst, par, rec, srcLoc, snapVer, cancelBy, bst, alive, launches,
                 runs, base, res, ws, cap, restarts>>

---------------------------------------------------------------------------
(* Daemon *)

\* SplitCreate by the session or a running branch. Journal record,
\* snapshot of the caller's tree, capabilities, in one journaled step.
\* Bug noIntent: forks may exist before the record. Bug childFromUser: a
\* nested snapshot reads the user's tree.
Create(s, p) ==
  /\ sst[s] = "none"
  /\ \A t \in Splits : t < s => par[t] # None          \* ids in creation order
  /\ IF p = Root THEN TRUE
     ELSE /\ p[1] # s /\ bst[p] = "running" /\ alive[p] /\ cap[p] = "live"
          /\ (Bug = "noDepth" \/ Depth(p[1]) + 1 <= MaxDepth)
  /\ par' = [par EXCEPT ![s] = p]
  /\ sst' = [sst EXCEPT ![s] = "run"]
  /\ rec' = [rec EXCEPT ![s] = (Bug # "noIntent")]
  /\ LET src == IF p = Root \/ Bug = "childFromUser" THEN UserTree ELSE Fork(p) IN
     /\ srcLoc' = [srcLoc EXCEPT ![s] = src]
     /\ snapVer' = [snapVer EXCEPT ![s] = ver[src]]
  /\ cap' = [b \in Branches |-> IF b[1] = s THEN "live" ELSE cap[b]]
  /\ bst' = [b \in Branches |-> IF b[1] = s THEN "ready" ELSE bst[b]]
  /\ UNCHANGED <<cancelBy, alive, launches, runs, wrote, base, res, ws, ver, W,
                 restarts>>

\* clonefile one fork from the frozen base. Requires the journal record.
\* Bug liveFork: clone from the live source tree.
Alloc(b) ==
  /\ sst[b[1]] = "run" /\ bst[b] = "ready" /\ ws[b] = "none"
  /\ rec[b[1]] \/ Bug = "noIntent"
  /\ base' = [base EXCEPT ![b] = IF Bug = "liveFork" THEN ver[srcLoc[b[1]]]
                                 ELSE snapVer[b[1]]]
  /\ ws' = [ws EXCEPT ![b] = "alloc"]
  /\ UNCHANGED <<sst, par, rec, srcLoc, snapVer, cancelBy, bst, alive, launches,
                 runs, wrote, res, cap, ver, W, restarts>>

\* Journal "running" and hand the start to the supervisor or worker, if
\* the session pool has a slot.
Launch(b) ==
  /\ sst[b[1]] = "run" /\ bst[b] = "ready" /\ cap[b] = "live"
  /\ \A c \in BOf(b[1]) : ws[c] = "alloc"
  /\ PoolUse < MaxConc
  /\ bst' = [bst EXCEPT ![b] = "running"]
  /\ launches' = [launches EXCEPT ![b] = @ + 1]
  /\ UNCHANGED <<sst, par, rec, srcLoc, snapVer, cancelBy, alive, runs, wrote,
                 base, res, ws, cap, ver, W, restarts>>

\* Pool full: the branch fails visibly without running (capacity).
Refuse(b) ==
  /\ sst[b[1]] = "run" /\ bst[b] = "ready" /\ cap[b] = "live"
  /\ \A c \in BOf(b[1]) : ws[c] = "alloc"
  /\ PoolUse >= MaxConc
  /\ bst' = [bst EXCEPT ![b] = "captured"]
  /\ res' = [res EXCEPT ![b] = ver[Fork(b)]]
  /\ cap' = [cap EXCEPT ![b] = "revoked"]
  /\ UNCHANGED <<sst, par, rec, srcLoc, snapVer, cancelBy, alive, launches, runs,
                 wrote, base, ws, ver, W, restarts>>

Start(b) ==
  /\ bst[b] = "running" /\ runs[b] < launches[b]
  /\ alive' = [alive EXCEPT ![b] = TRUE]
  /\ runs' = [runs EXCEPT ![b] = @ + 1]
  /\ UNCHANGED <<sst, par, rec, srcLoc, snapVer, cancelBy, bst, launches, wrote,
                 base, res, ws, cap, ver, W, restarts>>

\* The branch's main process exits or crashes: its capability is revoked
\* and everything below it is cancelled. A shell branch may leave
\* background processes (its cgroup is not empty yet). Bug orphanChildren:
\* child splits keep running.
Exit(b) ==
  /\ bst[b] = "running" /\ alive[b] /\ sst[b[1]] = "run"
  /\ \E left \in (IF Kit(b) THEN {FALSE} ELSE BOOLEAN) :
       alive' = [alive EXCEPT ![b] = left]
  /\ LET T == IF Bug = "orphanChildren" THEN {} ELSE UnderBranch(b)
         B == UNION {BOf(t) : t \in T} IN
     /\ sst' = [t \in Splits |-> IF t \in T /\ sst[t] = "run" THEN "cancel"
                                 ELSE IF t \in T /\ sst[t] = "await" THEN "kept"
                                 ELSE sst[t]]
     /\ cap' = [c \in Branches |-> IF (c = b \/ c \in B) /\ cap[c] = "live"
                                     THEN "revoked" ELSE cap[c]]
     /\ bst' = [c \in Branches |->
                  IF c = b THEN "exited"
                  ELSE IF c \notin B \/ sst[c[1]] # "run" THEN bst[c]
                  ELSE IF bst[c] = "running" \/ (bst[c] = "exited" /\ alive[c]) THEN "cancelling"
                  ELSE IF bst[c] \in {"none", "ready", "exited"} THEN "reaped"
                  ELSE bst[c]]
     /\ ws' = [c \in Branches |-> IF c[1] \in T /\ sst[c[1]] = "await" /\ ws[c] = "alloc"
                                    THEN "retained" ELSE ws[c]]
  /\ UNCHANGED <<par, rec, srcLoc, snapVer, cancelBy, launches, runs, wrote, base,
                 res, ver, W, restarts>>

\* Capture once every writer is confirmed gone (container deleted, cgroup
\* empty, jobs cleaned). It verifies the gitfile and admin files are
\* unchanged and the fork has no nested `.git`; otherwise the branch is
\* rejected and its fork removed (no writer is left). Bug captureEarly:
\* capture at main-process exit. Bug noCaptureCheck: no verification.
Capture(b) ==
  /\ bst[b] = "exited" /\ sst[b[1]] = "run"
  /\ ~alive[b] \/ Bug = "captureEarly"
  /\ IF Tampered(b) /\ Bug # "noCaptureCheck"
       THEN /\ bst' = [bst EXCEPT ![b] = "rejected"]
            /\ ws' = [ws EXCEPT ![b] = "removed"]
            /\ UNCHANGED res
       ELSE /\ bst' = [bst EXCEPT ![b] = "captured"]
            /\ res' = [res EXCEPT ![b] = ver[Fork(b)]]
            /\ UNCHANGED ws
  /\ UNCHANGED <<sst, par, rec, srcLoc, snapVer, cancelBy, alive, launches, runs,
                 wrote, base, cap, ver, W, restarts>>

Await(s) ==
  /\ sst[s] = "run" /\ \A b \in BOf(s) : bst[b] \in {"captured", "rejected"}
  /\ sst' = [sst EXCEPT ![s] = "await"]
  /\ UNCHANGED <<par, rec, srcLoc, snapVer, cancelBy, bst, alive, launches, runs,
                 wrote, base, res, ws, cap, ver, W, restarts>>

\* SplitJoin + SplitRelease from actor a, accepted only from the creator's
\* connection (the session for a root split, the running creator branch
\* for a nested one), on an awaiting or kept split. The consumer may write
\* the actor's own tree, then removes or keeps. Bug ancestorJoin: any
\* ancestor (or the session) may join a nested split.
Consume(s, a) ==
  /\ sst[s] \in {"await", "kept"}
  /\ IF a = Root THEN par[s] = Root \/ (Bug = "ancestorJoin" /\ par[s] # None)
     ELSE /\ bst[a] = "running" /\ alive[a]
          /\ (par[s] = a \/ (Bug = "ancestorJoin" /\ a \in Anc(s)))
  /\ WriteLoc(IF a = Root THEN UserTree ELSE Fork(a), Join(s))
  /\ sst' = [sst EXCEPT ![s] = "done"]
  /\ \E keep \in BOOLEAN :
       ws' = [b \in Branches |-> IF b[1] = s /\ ws[b] \in {"alloc", "retained"}
                                   THEN (IF keep THEN "retained" ELSE "removed")
                                   ELSE ws[b]]
  /\ UNCHANGED <<par, rec, srcLoc, snapVer, cancelBy, bst, alive, launches, runs,
                 wrote, base, res, cap, restarts>>

\* Lease expires without release: kept (forks retained), still joinable by
\* the creator. Bug noLease: never.
LeaseExpire(s) ==
  /\ sst[s] = "await" /\ Bug # "noLease"
  /\ sst' = [sst EXCEPT ![s] = "kept"]
  /\ ws' = [b \in Branches |-> IF b[1] = s /\ ws[b] = "alloc" THEN "retained" ELSE ws[b]]
  /\ UNCHANGED <<par, rec, srcLoc, snapVer, cancelBy, bst, alive, launches, runs,
                 wrote, base, res, cap, ver, W, restarts>>

\* SplitCancel: from the session (any split), or from a running branch for
\* splits below it (the creator dropping its stream, or `marsh splits
\* cancel` from a trusted shell branch). Bug unscopedCancel: a Kit job may
\* cancel any split. Bug noPropagate: only the named split.
Cancel(s, a) ==
  /\ sst[s] = "run"
  /\ \/ a = Root
     \/ /\ a \in Branches /\ bst[a] = "running" /\ alive[a] /\ cap[a] = "live"
        /\ (a \in Anc(s) \/ (Bug = "unscopedCancel" /\ Kit(a)))
  /\ cancelBy' = [cancelBy EXCEPT ![s] = a]
  /\ CancelSet(IF Bug = "noPropagate" THEN {s} ELSE Below(s))
  /\ UNCHANGED <<par, rec, srcLoc, snapVer, alive, launches, runs, wrote, base,
                 res, ver, W, restarts>>

Reap(b) ==
  /\ bst[b] = "cancelling"
  /\ bst' = [bst EXCEPT ![b] = "reaped"]
  /\ alive' = [alive EXCEPT ![b] = FALSE]
  /\ UNCHANGED <<sst, par, rec, srcLoc, snapVer, cancelBy, launches, runs, wrote,
                 base, res, ws, cap, ver, W, restarts>>

GraceExpire(b) ==
  /\ bst[b] = "cancelling"
  /\ bst' = [bst EXCEPT ![b] = "unreaped"]
  /\ UNCHANGED <<sst, par, rec, srcLoc, snapVer, cancelBy, alive, launches, runs,
                 wrote, base, res, ws, cap, ver, W, restarts>>

\* A cancelled split settles: reaped forks removed, unreaped retained
\* (flagged untrusted). Bug removeLive: unreaped forks removed too.
CancelEnd(s) ==
  /\ sst[s] = "cancel"
  /\ \A b \in BOf(s) : bst[b] \in {"reaped", "unreaped", "captured", "rejected", "uncertain"}
  /\ sst' = [sst EXCEPT ![s] = "done"]
  /\ ws' = [b \in Branches |->
              IF b[1] = s /\ ws[b] = "alloc"
                THEN IF bst[b] = "unreaped" /\ Bug # "removeLive" THEN "retained" ELSE "removed"
                ELSE ws[b]]
  /\ UNCHANGED <<par, rec, srcLoc, snapVer, cancelBy, bst, alive, launches, runs,
                 wrote, base, res, cap, ver, W, restarts>>

\* Daemon crash + restart between any two daemon steps. The journal
\* survives: recorded unfinished splits become uncertain, launched
\* branches uncertain, unlaunched never start, capabilities revoked;
\* unrecorded splits are lost. Bug replay: resume, treating journaled but
\* unconfirmed launches as not started.
Restart ==
  /\ restarts < MaxRestarts
  /\ restarts' = restarts + 1
  /\ IF Bug = "replay"
       THEN /\ bst' = [b \in Branches |-> IF bst[b] = "running" THEN "ready" ELSE bst[b]]
            /\ UNCHANGED <<sst, cap>>
       ELSE /\ sst' = [s \in Splits |-> IF sst[s] \in SplitActive
                                          THEN IF rec[s] THEN "uncertain" ELSE "lost"
                                          ELSE sst[s]]
            /\ bst' = [b \in Branches |->
                         IF sst[b[1]] \notin SplitActive THEN bst[b]
                         ELSE IF bst[b] \in {"running", "exited", "cancelling"} THEN "uncertain"
                         ELSE IF bst[b] \in {"none", "ready"} THEN "reaped"
                         ELSE bst[b]]
            /\ cap' = [b \in Branches |-> IF cap[b] = "live" THEN "revoked" ELSE cap[b]]
  /\ UNCHANGED <<par, rec, srcLoc, snapVer, cancelBy, alive, launches, runs, wrote,
                 base, res, ws, ver, W>>

\* After restart, recorded forks of uncertain splits are retained and
\* reported (untrusted). Bug forgetUncertain: never.
Recover(b) ==
  /\ sst[b[1]] = "uncertain" /\ ws[b] = "alloc" /\ Bug # "forgetUncertain"
  /\ ws' = [ws EXCEPT ![b] = "retained"]
  /\ UNCHANGED <<sst, par, rec, srcLoc, snapVer, cancelBy, bst, alive, launches,
                 runs, wrote, base, res, cap, ver, W, restarts>>

Next ==
  \/ UserEdit \/ Restart
  \/ \E s \in Splits : \/ \E p \in Callers : Create(s, p)
                       \/ Await(s) \/ LeaseExpire(s) \/ CancelEnd(s)
                       \/ \E a \in Callers : Cancel(s, a) \/ Consume(s, a)
  \/ \E b \in Branches : \/ ProcDie(b) \/ Alloc(b) \/ Launch(b) \/ Refuse(b) \/ Start(b)
                         \/ Exit(b) \/ Capture(b) \/ Reap(b) \/ GraceExpire(b)
                         \/ Recover(b) \/ \E l \in Locs : Write(b, l)

Fairness ==
  /\ \A b \in Branches : /\ WF_vars(Alloc(b)) /\ WF_vars(Launch(b) \/ Refuse(b))
                         /\ WF_vars(Start(b)) /\ WF_vars(Exit(b)) /\ WF_vars(Capture(b))
                         /\ WF_vars(ProcDie(b)) /\ WF_vars(Reap(b) \/ GraceExpire(b))
                         /\ WF_vars(Recover(b))
  /\ \A s \in Splits : WF_vars(Await(s)) /\ WF_vars(LeaseExpire(s)) /\ WF_vars(CancelEnd(s))

Spec == Init /\ [][Next]_vars /\ Fairness

---------------------------------------------------------------------------
(* Invariants. Each negative control removes one guard above. *)

TypeOK ==
  /\ sst \in [Splits -> {"none", "run", "await", "kept", "done", "cancel", "uncertain", "lost"}]
  /\ bst \in [Branches -> {"none", "ready", "running", "exited", "captured", "rejected",
                           "cancelling", "reaped", "unreaped", "uncertain"}]
  /\ ws \in [Branches -> {"none", "alloc", "removed", "retained"}]
  /\ cap \in [Branches -> {"none", "live", "revoked"}]

\* Snapshot objects (store.git) are never written by a branch.
SnapshotImmutable == \A s \in Splits : W[Objs(s)] = {}

\* Git admin files (gitfile, config, HEAD, info, packed-refs) are never
\* written by a branch.
AdminReadOnly == \A b \in Branches : W[Admin(b)] = {}

\* A result handed to consumers comes from a fork with untouched git
\* metadata and no nested `.git` (nothing host git would execute).
ExposedClean == \A b \in Branches : bst[b] = "captured" => ~Tampered(b)

OneBase == \A b \in Branches : ws[b] # "none" => base[b] = snapVer[b[1]]

ChildFromParent == \A s \in Splits : par[s] \in Branches => srcLoc[s] = Fork(par[s])

ForkIsolation == \A b \in Branches :
  W[Fork(b)] \subseteq {Br(b)} \cup {Join(t) : t \in {u \in Splits : par[u] = b}}

NoWriteToUserTree ==
  W[UserTree] \subseteq {UserA} \cup {Join(s) : s \in {t \in Splits : par[t] = Root}}

CaptureStable == \A b \in Branches : bst[b] = "captured" => res[b] = ver[Fork(b)]

LineageWF == \A s \in Splits : par[s] # None =>
  /\ Depth(s) <= MaxDepth
  /\ par[s] \in Branches => par[s][1] # s /\ par[par[s][1]] # None

\* Processes that actually exist stay within the session pool.
BudgetBound == Cardinality({b \in Branches : alive[b]}) <= MaxConc

LiveUnderLive == \A b \in Branches :
  (cap[b] = "live" /\ par[b[1]] \in Branches) => cap[par[b[1]]] = "live"

CancelPropagates == \A s \in Splits : sst[s] = "cancel" =>
  \A t \in Below(s) : \A b \in BOf(t) : cap[b] # "live" /\ bst[b] # "running"

CancelScoped == \A s \in Splits : cancelBy[s] \in Branches => cancelBy[s] \in Anc(s)

NoRemoveWhileLive == \A b \in Branches : ws[b] = "removed" => ~alive[b]

NoReplay == \A b \in Branches : runs[b] <= 1

WorkspaceRecorded == \A b \in Branches : ws[b] # "none" => rec[b[1]]

(* Liveness: every fork is eventually removed or retained (and reported). *)
EventuallySettled == <>[](\A b \in Branches : Settled(b))
=============================================================================
