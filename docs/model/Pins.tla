------------------------------ MODULE Pins ------------------------------
(* Bind-source pins of the services agent (A3): who may release a pin, and *)
(* what a container finds at its pin path after an engine restart.         *)
(*                                                                         *)
(* A pin is a mount of a project directory (or file) at a root-only path   *)
(* that Docker is pointed at. The ledger records each pin and the lease of *)
(* each container: the pins it made and those it inherited through         *)
(* `--volumes-from`. The pin is unmounted and its path removed when the    *)
(* last container holding it is deleted. After an engine restart the mounts *)
(* are gone; `restore` recreates every pin whose source still resolves     *)
(* (before the engine runs) and leaves each of the others as a dangling    *)
(* symlink, recorded as failed, so that `start` fails instead of Docker    *)
(* creating an empty directory at a missing bind source.                   *)
(*                                                                         *)
(* The environment (the project folder's owner) may delete a pin's source  *)
(* at any time; that matters only to the next restore.                     *)
EXTENDS FiniteSets

CONSTANTS Cs, Ps, Bug

Bugs == {"none", "inheritOff", "releaseWhileHeld", "restoreFailOpen", "leak"}
ASSUME Bug \in Bugs

VARIABLES
  ledger,   \* pins the ledger records
  mounted,  \* pins with a live mount at their path
  poisoned, \* pins whose path is a dangling symlink (restore failed; flag in the ledger)
  srcGone,  \* pins whose source the environment has deleted
  holds,    \* [Cs -> SUBSET Ps]: the lease the ledger records for each container
  needs,    \* ghost [Cs -> SUBSET Ps]: pins the container's mounts actually reference
  cst,      \* [Cs -> "none" | "created" | "running" | "removed"]
  engine,   \* "up" | "down"
  emptyDir  \* ghost: a container ran on an empty directory Docker made at a pin path

vars == <<ledger, mounted, poisoned, srcGone, holds, needs, cst, engine, emptyDir>>

Live(c) == cst[c] \in {"created", "running"}

Init ==
  /\ ledger = {} /\ mounted = {} /\ poisoned = {} /\ srcGone = {}
  /\ holds = [c \in Cs |-> {}] /\ needs = [c \in Cs |-> {}]
  /\ cst = [c \in Cs |-> "none"] /\ engine = "up" /\ emptyDir = FALSE

\* `create` with a bind: the pin is mounted, recorded, and leased to the new container.
CreateOwn(c, p) ==
  /\ engine = "up" /\ cst[c] = "none" /\ p \notin ledger /\ p \notin mounted
  /\ ledger' = ledger \cup {p} /\ mounted' = mounted \cup {p}
  /\ holds' = [holds EXCEPT ![c] = {p}] /\ needs' = [needs EXCEPT ![c] = {p}]
  /\ cst' = [cst EXCEPT ![c] = "created"]
  /\ UNCHANGED <<poisoned, srcGone, engine, emptyDir>>

\* `create --volumes-from d`: Docker copies d's mounts, so the new container
\* needs d's pins; the lease must say so (the defect leaves it empty).
CreateInherit(c, d) ==
  /\ engine = "up" /\ cst[c] = "none" /\ c # d /\ Live(d) /\ needs[d] # {}
  /\ needs' = [needs EXCEPT ![c] = needs[d]]
  /\ holds' = [holds EXCEPT ![c] = IF Bug = "inheritOff" THEN {} ELSE holds[d]]
  /\ cst' = [cst EXCEPT ![c] = "created"]
  /\ UNCHANGED <<ledger, mounted, poisoned, srcGone, engine, emptyDir>>

\* Admission refuses `start` of a container whose lease holds a failed pin.
Blocked(c) == Bug # "restoreFailOpen" /\ holds[c] \cap poisoned # {}

\* A pin neither mounted nor poisoned is absent: Docker makes an empty directory
\* and the container runs on it. A poisoned one (dangling symlink) fails to start.
Start(c) ==
  /\ engine = "up" /\ cst[c] = "created" /\ ~Blocked(c) /\ needs[c] \cap poisoned = {}
  /\ cst' = [cst EXCEPT ![c] = "running"]
  /\ emptyDir' = (emptyDir \/ ~(needs[c] \subseteq mounted))
  /\ UNCHANGED <<ledger, mounted, poisoned, srcGone, holds, needs, engine>>

Stop(c) ==
  /\ cst[c] = "running"
  /\ cst' = [cst EXCEPT ![c] = "created"]
  /\ UNCHANGED <<ledger, mounted, poisoned, srcGone, holds, needs, engine, emptyDir>>

\* `rm`: the pins no other live container holds are unmounted and forgotten.
Remove(c) ==
  LET others == UNION {holds[d] : d \in {x \in Cs : x # c /\ Live(x)}}
      released == IF Bug = "leak" THEN {}
                  ELSE IF Bug = "releaseWhileHeld" THEN holds[c]
                  ELSE holds[c] \ others
  IN
  /\ engine = "up" /\ Live(c)
  /\ cst' = [cst EXCEPT ![c] = "removed"]
  /\ holds' = [holds EXCEPT ![c] = {}] /\ needs' = [needs EXCEPT ![c] = {}]
  /\ ledger' = ledger \ released /\ mounted' = mounted \ released
  /\ poisoned' = poisoned \ released /\ srcGone' = srcGone \ released
  /\ UNCHANGED <<engine, emptyDir>>

\* Environment: the project folder's owner deletes a pin's source.
SourceGone(p) ==
  /\ p \in ledger /\ p \notin srcGone
  /\ srcGone' = srcGone \cup {p}
  /\ UNCHANGED <<ledger, mounted, poisoned, holds, needs, cst, engine, emptyDir>>

\* The services VM or engine restarts: mounts vanish, containers stop.
EngineRestart ==
  /\ engine = "up"
  /\ engine' = "down" /\ mounted' = {}
  /\ cst' = [c \in Cs |-> IF cst[c] = "running" THEN "created" ELSE cst[c]]
  /\ UNCHANGED <<ledger, poisoned, srcGone, holds, needs, emptyDir>>

\* engine-up, before the production engine runs: recreate every pin that still
\* resolves; poison and flag the others (the defect leaves them absent).
Restore ==
  /\ engine = "down"
  /\ mounted' = ledger \ srcGone
  /\ poisoned' = IF Bug = "restoreFailOpen" THEN {} ELSE ledger \cap srcGone
  /\ engine' = "up"
  /\ UNCHANGED <<ledger, srcGone, holds, needs, cst, emptyDir>>

Next ==
  \/ \E c \in Cs, p \in Ps : CreateOwn(c, p)
  \/ \E c, d \in Cs : CreateInherit(c, d)
  \/ \E c \in Cs : Start(c) \/ Stop(c) \/ Remove(c)
  \/ \E p \in Ps : SourceGone(p)
  \/ EngineRestart \/ Restore

Spec == Init /\ [][Next]_vars

\* No container ever runs on an empty directory made at a missing pin.
NeverEmptyDir == ~emptyDir
\* What a live container's mounts reference is still recorded: a pin is never
\* released while a container that needs it exists (`--volumes-from` included).
HeldPinsPersist == \A c \in Cs : Live(c) => needs[c] \subseteq ledger
\* A running container's pins are mounted.
RunningHasMounts == \A c \in Cs : (engine = "up" /\ cst[c] = "running") => needs[c] \subseteq mounted
\* A recorded pin is held: nothing leaks once its last holder is gone.
NoLeak == \A p \in ledger : \E c \in Cs : p \in holds[c]
=============================================================================
