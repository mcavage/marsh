---------------------------- MODULE ServicesReap ----------------------------
(* A job's containers in its project's services VM at the end of its      *)
(* attempt (E4; DECISIONS 11, SECURITY-DESIGN A4; `docs/design/services.md`,*)
(* "Reaping at attempt end"; built in `marsh-backend/src/services_reap.rs`, *)
(* `marsh-backend/src/endpoint.rs` `hold` and `JobEndpoint` drop, and      *)
(* `marsh-services/src/reap.rs`).                                           *)
(*   Begin(a)     the attempt starts and is RECORDED first (`hold` writes   *)
(*                the record before any channel reaches the VM)             *)
(*   Create(c)    the attempt makes a container; a shell's containers       *)
(*                (owner "shell") are made any time and never recorded      *)
(*   End(a)       the attempt ends (normal exit, cancel, crash)            *)
(*   Kill         the daemon dies: its attempts' ends go unseen, the relay  *)
(*                is gone, the records stay on disk                         *)
(*   Restart      the next daemon's relay is up again                       *)
(*   Reap(a)      the record of an ended attempt is due and the relay is up: *)
(*                stop and remove that attempt's ONE-OFF containers, keep   *)
(*                Compose and restart-policy ones, and drop the record      *)
(*   ShellExit    (only in the break) a shell's objects are reaped at exit  *)
(* Ghosts: `removedBy` says which reap (or the shell exit) removed a         *)
(* container, `early` records a reap that removed a container of a running   *)
(* attempt. Attempts start once; a dead attempt never runs again.           *)
EXTENDS Naturals, TLC

Attempts == {"a1", "a2"}
Ctrs == {"c1", "c2", "c3", "c4"}
\* Owner: the attempt that made a container ("shell" for a shell's object).
Owner == ("c1" :> "a1") @@ ("c2" :> "a1") @@ ("c3" :> "a2") @@ ("c4" :> "shell")
\* Kind: "oneoff" (docker run, Testcontainers) or "persist" (a Compose project's or restart-policy container).
Kind == ("c1" :> "oneoff") @@ ("c2" :> "persist") @@ ("c3" :> "oneoff") @@ ("c4" :> "oneoff")

CONSTANTS Bug, MaxKills

Bugs == {"none", "reapOff", "reapAll", "reapPersistent", "reapRunning", "noRecord", "shellReap"}
ASSUME Bug \in Bugs /\ MaxKills \in Nat

VARIABLES
  started,   \* attempts that began
  running,   \* attempts that have not ended
  rec,       \* attempts whose reap record is on disk
  made,      \* containers created
  live,      \* containers not removed
  relayUp,   \* the project's relay answers
  kills,     \* daemon deaths so far
  removedBy, \* ghost: the reap (attempt) or "shell" that removed the container; "none" if not removed
  early      \* ghost: a reap removed a container of an attempt that was still running

vars == <<started, running, rec, made, live, relayUp, kills, removedBy, early>>

TypeOK ==
  /\ started \subseteq Attempts
  /\ running \subseteq Attempts
  /\ rec \subseteq Attempts
  /\ made \subseteq Ctrs
  /\ live \subseteq Ctrs
  /\ relayUp \in BOOLEAN
  /\ kills \in 0..MaxKills
  /\ removedBy \in [Ctrs -> Attempts \cup {"shell", "none"}]
  /\ early \in BOOLEAN

Init ==
  /\ started = {}
  /\ running = {}
  /\ rec = {}
  /\ made = {}
  /\ live = {}
  /\ relayUp = TRUE
  /\ kills = 0
  /\ removedBy = [c \in Ctrs |-> "none"]
  /\ early = FALSE

Begin(a) ==
  /\ a \notin started
  /\ started' = started \cup {a}
  /\ running' = running \cup {a}
  /\ rec' = IF Bug = "noRecord" THEN rec ELSE rec \cup {a}
  /\ UNCHANGED <<made, live, relayUp, kills, removedBy, early>>

Create(c) ==
  /\ c \notin made
  /\ IF Owner[c] = "shell"
       THEN TRUE
       ELSE /\ Owner[c] \in running
            /\ (Owner[c] \in rec \/ Bug = "noRecord")
  /\ made' = made \cup {c}
  /\ live' = live \cup {c}
  /\ UNCHANGED <<started, running, rec, relayUp, kills, removedBy, early>>

End(a) ==
  /\ a \in running
  /\ running' = running \ {a}
  /\ UNCHANGED <<started, rec, made, live, relayUp, kills, removedBy, early>>

Kill ==
  /\ kills < MaxKills
  /\ kills' = kills + 1
  /\ running' = {}
  /\ relayUp' = FALSE
  /\ UNCHANGED <<started, rec, made, live, removedBy, early>>

Restart ==
  /\ ~relayUp
  /\ relayUp' = TRUE
  /\ UNCHANGED <<started, running, rec, made, live, kills, removedBy, early>>

\* The containers one reap removes: the attempt's one-offs (DECISIONS 11).
Victims(a) ==
  IF Bug = "reapAll" THEN live
  ELSE IF Bug = "reapPersistent" THEN {c \in live : Owner[c] = a}
  ELSE {c \in live : Owner[c] = a /\ Kind[c] = "oneoff"}

Reap(a) ==
  /\ Bug # "reapOff"
  /\ a \in rec
  /\ (a \notin running \/ Bug = "reapRunning")
  /\ relayUp
  /\ live' = live \ Victims(a)
  /\ removedBy' = [c \in Ctrs |-> IF c \in Victims(a) THEN a ELSE removedBy[c]]
  /\ early' = (early \/ (\E c \in Victims(a) : Owner[c] \in running))
  /\ rec' = rec \ {a}
  /\ UNCHANGED <<started, running, made, relayUp, kills>>

\* Only in the break: the shell's objects go when the shell exits.
ShellExit ==
  /\ Bug = "shellReap"
  /\ live' = live \ {c \in live : Owner[c] = "shell"}
  /\ removedBy' = [c \in Ctrs |-> IF c \in live /\ Owner[c] = "shell" THEN "shell" ELSE removedBy[c]]
  /\ UNCHANGED <<started, running, rec, made, relayUp, kills, early>>

Next ==
  \/ \E a \in Attempts : Begin(a) \/ End(a) \/ Reap(a)
  \/ \E c \in Ctrs : Create(c)
  \/ Kill \/ Restart \/ ShellExit

Fairness == \A a \in Attempts : WF_vars(Reap(a)) /\ WF_vars(Restart)

Spec == Init /\ [][Next]_vars /\ Fairness

\* Invariants. Each is a rule of E4, with its negative control in the config.

\* A reap never removes a container of an attempt that is still running.
NoEarlyReap == ~early

\* A shell's object is never removed by anything but the user.
ShellNeverReaped == \A c \in Ctrs : Owner[c] = "shell" => removedBy[c] = "none"

\* A reap removes only what its own attempt made.
OnlyOwnAttempt == \A c \in Ctrs : removedBy[c] \in Attempts => removedBy[c] = Owner[c]

\* A Compose or restart-policy container is never removed by a reap.
KeptPersist == \A c \in Ctrs : Kind[c] = "persist" => removedBy[c] = "none"

\* A one-off container exists only while its attempt has a record: no unrecorded container can leak.
RecordCoversOneOffs ==
  \A c \in live : (Kind[c] = "oneoff" /\ Owner[c] \in Attempts) => Owner[c] \in rec

\* Liveness: once an attempt has ended, its record is reaped (the relay comes back after a daemon death).
ReapsEnded == \A a \in Attempts : (a \in rec /\ a \notin running) ~> (a \notin rec)

=============================================================================
