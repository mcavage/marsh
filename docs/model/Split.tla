----------------------------- MODULE Split -----------------------------
(* One `split { ... } | join | CMD...` evaluation in one Git repository  *)
(* or plain directory (docs/split-join.md). The shell takes one snapshot *)
(* of the user's working state, allocates one private workspace (Git     *)
(* worktree or copy) per branch, runs the branches with cwd inside their *)
(* workspaces, captures each finished branch's result (status, output,  *)
(* diff vs the snapshot), runs the downstream stages ("join" below: the  *)
(* pipeline after `join`) in the original project, and after the last    *)
(* stage exits removes or retains every workspace. A registered Kit job  *)
(* a branch starts mounts only that branch's workspace (and Git dir).    *)
(*                                                                       *)
(* Split phase: idle -> alloc -> run -> join -> done                     *)
(*              alloc -> abort -> done       (worktree setup failed)     *)
(*              alloc/run -> cancel -> done  (Ctrl-C or output limit)    *)
(*              any active phase -> crashed  (shell process died)        *)
(* Branch (the shell's view):                                            *)
(*              none -> alloc -> running -> done | failed                *)
(*              running -> cancelling -> cancelled | unreaped            *)
(*              running/cancelling -> unreaped  (shell crashed)          *)
(* Worktree:    none -> allocated -> removed | retained                  *)
(*                                                                       *)
(* `alive` is the truth about a branch process, which the shell sees     *)
(* only through exit/reap confirmations. The user may edit the project   *)
(* at any time. A branch process writes only below its cwd (assumption   *)
(* C1 in docs/split-join.md: cooperative confinement, not isolation), so *)
(* a branch whose cwd is the user's tree writes the user's tree. A Kit   *)
(* job is isolated: it can write an absolute project path only if the    *)
(* daemon mounted the whole project for it. One                         *)
(* split is modeled; concurrent splits use disjoint random ids.          *)
EXTENDS Naturals, FiniteSets

CONSTANTS Branches, MaxVer, Bug

Bugs == {"none", "fallbackInPlace", "perBranchSnapshot", "joinEarly",
         "removeUnreaped", "cancelOrphan", "retainSilently", "noScan",
         "kitWholeProject"}
ASSUME Bug \in Bugs

Finished == {"done", "failed"}                                \* exited on its own
Reaped   == {"none", "alloc", "done", "failed", "cancelled"}  \* shell saw no process left

VARIABLES
  phase,     \* split phase
  bst,       \* [Branches -> branch state as the shell sees it]
  alive,     \* [Branches -> BOOLEAN] ghost: the branch process may still run
  cwd,       \* [Branches -> "none" | "wt" | "user"] where the branch runs
  wt,        \* [Branches -> "none" | "allocated" | "removed" | "retained"]
  reported,  \* [Branches -> BOOLEAN] retained path printed or listed
  base,      \* [Branches -> 0..MaxVer] snapshot the worktree was built from
  wrote,     \* [Branches -> BOOLEAN] branch has written (bounds the state)
  res,       \* [Branches -> BOOLEAN] result captured after the branch finished
  userVer,   \* environment: version of the user's working tree
  snap,      \* the split's one snapshot version
  keep,      \* `join --keep`
  joinSt,    \* "none" | "running" | "ok" | "failed"
  joinSaw,   \* branches whose results the join input contained
  userW,     \* ghost: actors that wrote the user's working tree
  kit        \* "none" | "workspace" | "project": mounts of the Kit job that
             \* branch KitBranch starts, as admitted by the daemon (one
             \* representative branch keeps the state space small)

vars == <<phase, bst, alive, cwd, wt, reported, base, wrote, res, userVer,
          snap, keep, joinSt, joinSaw, userW, kit>>

Init ==
  /\ phase = "idle"
  /\ bst = [b \in Branches |-> "none"]
  /\ alive = [b \in Branches |-> FALSE]
  /\ cwd = [b \in Branches |-> "none"]
  /\ wt = [b \in Branches |-> "none"]
  /\ reported = [b \in Branches |-> FALSE]
  /\ base = [b \in Branches |-> 0]
  /\ wrote = [b \in Branches |-> FALSE]
  /\ res = [b \in Branches |-> FALSE]
  /\ userVer = 0 /\ snap = 0 /\ keep = FALSE
  /\ joinSt = "none" /\ joinSaw = {}
  /\ userW = {}
  /\ kit = "none"

KitBranch == CHOOSE b \in Branches : TRUE

Settled(b) == wt[b] \in {"none", "removed"} \/ (wt[b] = "retained" /\ reported[b])

\* Environment: the user keeps editing the project.
UserEdit ==
  /\ userVer < MaxVer
  /\ userVer' = userVer + 1
  /\ userW' = userW \cup {"user"}
  /\ UNCHANGED <<phase, bst, alive, cwd, wt, reported, base, wrote, res, snap,
                 keep, joinSt, joinSaw, kit>>

\* Upstream input is spooled; one private-index snapshot is taken.
Start ==
  /\ phase = "idle"
  /\ phase' = "alloc"
  /\ snap' = userVer
  /\ keep' \in BOOLEAN
  /\ UNCHANGED <<bst, alive, cwd, wt, reported, base, wrote, res, userVer,
                 joinSt, joinSaw, userW, kit>>

\* `git worktree add --no-checkout` + read-tree/checkout-index of the snapshot.
\* Bug perBranchSnapshot: each worktree copies the live working state.
Alloc(b) ==
  /\ phase = "alloc" /\ bst[b] = "none"
  /\ bst' = [bst EXCEPT ![b] = "alloc"]
  /\ cwd' = [cwd EXCEPT ![b] = "wt"]
  /\ wt' = [wt EXCEPT ![b] = "allocated"]
  /\ base' = [base EXCEPT ![b] = IF Bug = "perBranchSnapshot" THEN userVer ELSE snap]
  /\ UNCHANGED <<phase, alive, reported, wrote, res, userVer, snap, keep,
                 joinSt, joinSaw, userW, kit>>

\* Worktree creation failed. Correct: abort before any branch starts.
\* Bug fallbackInPlace: run that branch in the original project instead.
AllocFail(b) ==
  /\ phase = "alloc" /\ bst[b] = "none"
  /\ IF Bug = "fallbackInPlace"
       THEN /\ bst' = [bst EXCEPT ![b] = "alloc"]
            /\ cwd' = [cwd EXCEPT ![b] = "user"]
            /\ base' = [base EXCEPT ![b] = snap]
            /\ UNCHANGED phase
       ELSE /\ phase' = "abort"
            /\ UNCHANGED <<bst, cwd, base, kit>>
  /\ UNCHANGED <<alive, wt, reported, wrote, res, userVer, snap, keep, joinSt,
                 joinSaw, userW, kit>>

\* Every worktree exists: start all branches.
Launch ==
  /\ phase = "alloc" /\ \A b \in Branches : bst[b] = "alloc"
  /\ phase' = "run"
  /\ bst' = [b \in Branches |-> "running"]
  /\ alive' = [b \in Branches |-> TRUE]
  /\ UNCHANGED <<cwd, wt, reported, base, wrote, res, userVer, snap, keep,
                 joinSt, joinSaw, userW, kit>>

BranchWrite(b) ==
  /\ alive[b] /\ ~wrote[b]
  /\ wrote' = [wrote EXCEPT ![b] = TRUE]
  /\ userW' = IF cwd[b] = "user" THEN userW \cup {"branch"} ELSE userW
  /\ UNCHANGED <<phase, bst, alive, cwd, wt, reported, base, res, userVer, snap,
                 keep, joinSt, joinSaw, kit>>

BranchExit(b) ==
  /\ bst[b] = "running"
  /\ \E s \in Finished : bst' = [bst EXCEPT ![b] = s]
  /\ alive' = [alive EXCEPT ![b] = FALSE]
  /\ UNCHANGED <<phase, cwd, wt, reported, base, wrote, res, userVer, snap,
                 keep, joinSt, joinSaw, userW, kit>>

\* Private-index `add -A` + write-tree of the worktree, then diff vs snapshot.
Capture(b) ==
  /\ phase = "run" /\ bst[b] \in Finished /\ ~res[b]
  /\ res' = [res EXCEPT ![b] = TRUE]
  /\ UNCHANGED <<phase, bst, alive, cwd, wt, reported, base, wrote, userVer,
                 snap, keep, joinSt, joinSaw, userW, kit>>

\* Ctrl-C or the shared output limit. Correct: signal every running branch
\* and wait for confirmations. Bug cancelOrphan: drop the branch futures
\* and treat them as gone (kill-on-drop without waiting).
Interrupt ==
  /\ phase \in {"alloc", "run"}
  /\ phase' = "cancel"
  /\ bst' = [b \in Branches |->
               IF bst[b] = "running"
                 THEN IF Bug = "cancelOrphan" THEN "cancelled" ELSE "cancelling"
                 ELSE bst[b]]
  /\ UNCHANGED <<alive, cwd, wt, reported, base, wrote, res, userVer, snap,
                 keep, joinSt, joinSaw, userW, kit>>

\* The branch confirmed exit (Brush reaped it; a Kit shim exited after the
\* worker reported container deletion).
Reap(b) ==
  /\ bst[b] = "cancelling"
  /\ bst' = [bst EXCEPT ![b] = "cancelled"]
  /\ alive' = [alive EXCEPT ![b] = FALSE]
  /\ UNCHANGED <<phase, cwd, wt, reported, base, wrote, res, userVer, snap,
                 keep, joinSt, joinSaw, userW, kit>>

\* Grace expired: SIGKILL sent, exit not confirmed.
GraceExpire(b) ==
  /\ bst[b] = "cancelling"
  /\ bst' = [bst EXCEPT ![b] = "unreaped"]
  /\ UNCHANGED <<phase, alive, cwd, wt, reported, base, wrote, res, userVer,
                 snap, keep, joinSt, joinSaw, userW, kit>>

\* Correct: join starts only when every branch finished and was captured.
\* Bug joinEarly: start when any result is available.
JoinStart ==
  /\ phase = "run"
  /\ IF Bug = "joinEarly" THEN \E b \in Branches : res[b]
                          ELSE \A b \in Branches : res[b]
  /\ phase' = "join"
  /\ joinSt' = "running"
  /\ joinSaw' = {b \in Branches : res[b]}
  /\ UNCHANGED <<bst, alive, cwd, wt, reported, base, wrote, res, userVer, snap,
                 keep, userW, kit>>

\* The downstream stages run in the user's project and may change it.
JoinWrite ==
  /\ joinSt = "running" /\ "join" \notin userW
  /\ userW' = userW \cup {"join"}
  /\ UNCHANGED <<phase, bst, alive, cwd, wt, reported, base, wrote, res,
                 userVer, snap, keep, joinSt, joinSaw, kit>>

JoinExit ==
  /\ joinSt = "running"
  /\ \E s \in {"ok", "failed"} : joinSt' = s
  /\ UNCHANGED <<phase, bst, alive, cwd, wt, reported, base, wrote, res,
                 userVer, snap, keep, joinSaw, userW, kit>>

\* Removal is allowed after the last stage exits 0 without --keep (joinSt =
\* "ok": the downstream segment has finished, so SPLIT_DIR stays valid for
\* every stage that reads it), and after
\* abort or cancel, but only once the shell saw the branch end.
\* Bug removeUnreaped: remove as soon as cancellation is requested.
MayRemove(b) ==
  /\ wt[b] = "allocated"
  /\ \/ phase = "join" /\ joinSt = "ok" /\ ~keep
     \/ phase \in {"abort", "cancel"}
  /\ bst[b] \in Reaped \/ (Bug = "removeUnreaped" /\ phase = "cancel")

Remove(b) ==
  /\ MayRemove(b)
  /\ wt' = [wt EXCEPT ![b] = "removed"]
  /\ UNCHANGED <<phase, bst, alive, cwd, reported, base, wrote, res, userVer,
                 snap, keep, joinSt, joinSaw, userW, kit>>

\* Retain after a finished join (failed, --keep, or a failed removal), an
\* unreaped branch, or a failed removal during abort/cancel. Retaining
\* always prints the path. Bug retainSilently: keep it without reporting.
Retain(b) ==
  /\ wt[b] = "allocated"
  /\ \/ phase = "join" /\ joinSt \in {"ok", "failed"}
     \/ phase = "cancel" /\ bst[b] = "unreaped"
     \/ MayRemove(b)
  /\ wt' = [wt EXCEPT ![b] = "retained"]
  /\ reported' = [reported EXCEPT ![b] = (Bug # "retainSilently")]
  /\ UNCHANGED <<phase, bst, alive, cwd, base, wrote, res, userVer, snap, keep,
                 joinSt, joinSaw, userW, kit>>

Finish ==
  /\ phase \in {"join", "abort", "cancel"}
  /\ joinSt # "running"
  /\ \A b \in Branches : wt[b] # "allocated" /\ bst[b] # "cancelling"
  /\ phase' = "done"
  /\ UNCHANGED <<bst, alive, cwd, wt, reported, base, wrote, res, userVer, snap,
                 keep, joinSt, joinSaw, userW, kit>>

\* The shell process dies (SIGKILL, VM loss). Nothing it started is
\* observed any more; worktrees stay on disk unreported.
Crash ==
  /\ phase \in {"alloc", "run", "join", "abort", "cancel"}
  /\ phase' = "crashed"
  /\ bst' = [b \in Branches |-> IF bst[b] \in {"running", "cancelling"}
                                  THEN "unreaped" ELSE bst[b]]
  /\ joinSt' = IF joinSt = "running" THEN "failed" ELSE joinSt
  /\ UNCHANGED <<alive, cwd, wt, reported, base, wrote, res, userVer, snap,
                 keep, joinSaw, userW, kit>>

\* The next split in this repository scans .marsh/split/ and reports every
\* directory without a live owner as retained. It never removes.
Scan(b) ==
  /\ Bug # "noScan"
  /\ phase = "crashed" /\ wt[b] = "allocated"
  /\ wt' = [wt EXCEPT ![b] = "retained"]
  /\ reported' = [reported EXCEPT ![b] = TRUE]
  /\ UNCHANGED <<phase, bst, alive, cwd, base, wrote, res, userVer, snap, keep,
                 joinSt, joinSaw, userW, kit>>

\* A running branch starts a registered Kit job. The daemon validates the
\* branch's workspace claim (registered worktree or copy beside its base,
\* cwd inside it) and mounts only that workspace and the Git directory.
\* A claim it cannot validate is refused (no job; not modeled further).
\* Bug kitWholeProject: the job gets the whole project, as before isolation.
KitLaunch ==
  /\ LET b == KitBranch IN
       phase = "run" /\ bst[b] = "running" /\ cwd[b] = "wt" /\ kit = "none"
  /\ kit' = IF Bug = "kitWholeProject" THEN "project" ELSE "workspace"
  /\ UNCHANGED <<phase, bst, alive, cwd, wt, reported, base, wrote, res,
                 userVer, snap, keep, joinSt, joinSaw, userW>>

\* The Kit job writes the absolute project path. It reaches the user's tree
\* only when the project is mounted; otherwise the path does not exist in
\* its container (or is read-only Git metadata) and the write fails.
KitWriteAbsolute ==
  /\ alive[KitBranch] /\ kit = "project" /\ "kit" \notin userW
  /\ userW' = userW \cup {"kit"}
  /\ UNCHANGED <<phase, bst, alive, cwd, wt, reported, base, wrote, res,
                 userVer, snap, keep, joinSt, joinSaw, kit>>

System ==
  \/ Start \/ Launch \/ JoinStart \/ JoinWrite \/ JoinExit \/ Finish
  \/ KitLaunch \/ KitWriteAbsolute
  \/ \E b \in Branches :
       Alloc(b) \/ AllocFail(b) \/ BranchWrite(b) \/ BranchExit(b) \/ Capture(b)
       \/ Reap(b) \/ GraceExpire(b) \/ Remove(b) \/ Retain(b) \/ Scan(b)

\* The user, Ctrl-C, and crashes are environment steps without fairness.
Next == UserEdit \/ Interrupt \/ Crash \/ System

Spec == Init /\ [][Next]_vars /\ WF_vars(System)
          /\ \A b \in Branches : WF_vars(Scan(b))

----------------------------------------------------------------------------
\* marsh and branch processes never write the user's working tree; only the
\* user and, once it has started, the join command do.
NoWriteToUserTreeBeforeJoin == userW \subseteq {"user", "join"}

\* Every worktree of one split is built from the same snapshot.
OneBase == \A b \in Branches : wt[b] # "none" => base[b] = snap

\* The join starts only after every branch finished on its own, and its
\* input contains every branch's captured result.
JoinSeesAllBranches ==
  joinSt # "none" => /\ joinSaw = Branches
                     /\ \A b \in Branches : bst[b] \in Finished /\ res[b] /\ ~alive[b]

\* A worktree is removed only when no branch process can still write into
\* it (or recreate it under .marsh/split/).
NoRemoveWhileLive == \A b \in Branches : wt[b] = "removed" => ~alive[b]

\* A finished split leaves no process behind that it does not report: every
\* branch process still possibly alive is known unreaped, never joined, and
\* its worktree is retained and reported.
CancelReapsAll ==
  phase = "done" =>
    \A b \in Branches : alive[b] => /\ bst[b] = "unreaped"
                                    /\ joinSt = "none"
                                    /\ wt[b] = "retained" /\ reported[b]

\* When a split finishes, every worktree is removed or retained and reported.
CleanupOrRetain == phase = "done" => \A b \in Branches : Settled(b)

\* A branch's Kit job is admitted only with its own workspace mounted, so it
\* never writes the user's working tree, even through an absolute path.
BranchJobConfined ==
  /\ "kit" \notin userW
  /\ kit \in {"none", "workspace"}

\* Liveness: every allocated worktree is eventually removed or reported,
\* including after a crash (by the next split's scan).
EventuallySettled == \A b \in Branches : (wt[b] = "allocated") ~> Settled(b)
=============================================================================
