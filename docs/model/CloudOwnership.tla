------------------------- MODULE CloudOwnership -------------------------
(* Current stock sign-in; no account manager. Cloud extension of local    *)
(* Ownership: persisted intent, create reply/visibility loss, cleanup-only *)
(* restart recovery, immutable-ID effects, and asynchronous deletion.      *)
(*                                                                         *)
(* A complete inventory is authoritative for resources already exposed.   *)
(* New creates may stay invisible arbitrarily long. An unseen create must *)
(* therefore never be forgotten merely because an inventory omits it.     *)
(* Names are daemon-random/reserved while pending; other actors can reuse *)
(* a name only after its original ID was known and externally removed.    *)
(* All identities are abstract integers; parsing/fsync/CLI bytes are not  *)
(* modeled. No workload restart, replay, or liveness guarantee is added.   *)
(*                                                                         *)
(* Detached runs (docs/design/cloud-detach.md): a ready sandbox may become *)
(* detached. That state survives restart and admits only read-only control *)
(* and signals (Control) and removal, never a new submission (Use). It is  *)
(* bounded by its own cap, DCAP, apart from the attached CAP.              *)
EXTENDS Naturals, FiniteSets
CONSTANTS Names, Managed, MaxId, CAP, DCAP, MaxRestarts, Bug
ASSUME Managed \subseteq Names
ASSUME Bug \in {"none", "dropMissing", "forgetUnseen", "ackIsGone", "byName",
               "restartReady", "useQuarantined", "replayCreate", "noCap",
               "staleSeen", "stalePoison", "restartDetached", "useDetached",
               "noDetachCap"}

Phases == {"none", "pending", "ready", "quarantined", "removing", "detached"}
IDs == 1..MaxId
VARIABLES phase, own, seen, flight, submitted, created, reserved, submits,
          stock, visible, maker, nextId, view, deleteTarget, ack, restarts,
          restarted, badEffect, badUse, queryState, querySeen, queryView,
          poisoned, prepared, det, badDetached

vars == <<phase, own, seen, flight, submitted, created, reserved, submits,
          stock, visible, maker, nextId, view, deleteTarget, ack, restarts,
          restarted, badEffect, badUse, queryState, querySeen, queryView,
          poisoned, prepared, det, badDetached>>
detVars == <<det, badDetached>>
recordVars == <<phase, own, seen, flight, submitted, created, reserved, submits>>
serviceVars == <<stock, visible, maker, nextId, deleteTarget, ack>>
queryVars == <<queryState, querySeen, queryView>>
failureVars == <<poisoned, prepared>>
otherVars == <<view, restarts, restarted, badEffect, badUse, queryVars, failureVars>>
Holding == {n \in Managed : phase[n] # "none" /\ ~det[n]}
DHolding == {n \in Managed : phase[n] # "none" /\ det[n]}
AliveIDs == {stock[n] : n \in Names} \ {0}

Init ==
  /\ phase = [n \in Managed |-> "none"] /\ own = [n \in Managed |-> 0]
  /\ seen = [n \in Managed |-> FALSE] /\ flight = [n \in Managed |-> FALSE]
  /\ submitted = [n \in Managed |-> FALSE] /\ created = [n \in Managed |-> 0]
  /\ reserved = {} /\ submits = [n \in Managed |-> 0]
  /\ stock = [n \in Names |-> 0] /\ visible = [n \in Names |-> FALSE]
  /\ maker = [u \in IDs |-> "none"] /\ nextId = 1
  /\ view = [n \in Names |-> 0]
  /\ deleteTarget = [n \in Managed |-> 0] /\ ack = [n \in Managed |-> FALSE]
  /\ restarts = 0 /\ restarted = {} /\ badEffect = FALSE /\ badUse = FALSE
  /\ queryState = [n \in Managed |-> "idle"]
  /\ querySeen = [n \in Managed |-> FALSE] /\ queryView = [n \in Managed |-> 0]
  /\ poisoned = FALSE /\ prepared = [n \in Managed |-> FALSE]
  /\ det = [n \in Managed |-> FALSE] /\ badDetached = FALSE

Reserve(n) ==
  /\ ~poisoned
  /\ n \notin reserved /\ phase[n] = "none"
  /\ Cardinality(Holding) < CAP \/ Bug = "noCap"
  /\ phase' = [phase EXCEPT ![n] = "pending"]
  /\ flight' = [flight EXCEPT ![n] = TRUE] /\ reserved' = reserved \cup {n}
  /\ UNCHANGED <<own, seen, submitted, created, submits, serviceVars, otherVars>>
  /\ UNCHANGED detVars

Submit(n) ==
  /\ ~poisoned
  /\ phase[n] = "pending"
  /\ (flight[n] /\ ~submitted[n]) \/ (Bug = "replayCreate" /\ submitted[n] /\ submits[n] = 1)
  /\ submitted' = [submitted EXCEPT ![n] = TRUE]
  /\ submits' = [submits EXCEPT ![n] = @ + 1]
  /\ UNCHANGED <<phase, own, seen, flight, created, reserved, serviceVars, otherVars>>
  /\ UNCHANGED detVars

(* Service create may land after the caller timed out or restarted. *)
CreateLands(n) ==
  /\ submitted[n] /\ created[n] = 0 /\ stock[n] = 0 /\ nextId <= MaxId
  /\ stock' = [stock EXCEPT ![n] = nextId]
  /\ created' = [created EXCEPT ![n] = nextId]
  /\ maker' = [maker EXCEPT ![nextId] = "host"] /\ nextId' = nextId + 1
  /\ visible' = [visible EXCEPT ![n] = FALSE]
  /\ UNCHANGED <<phase, own, seen, flight, submitted, reserved, submits,
                  deleteTarget, ack, otherVars>>
  /\ UNCHANGED detVars

Reply(n) ==
  /\ ~poisoned
  /\ phase[n] = "pending" /\ flight[n] /\ created[n] # 0 /\ own[n] = 0
  /\ own' = [own EXCEPT ![n] = created[n]]
  /\ UNCHANGED <<phase, seen, flight, submitted, created, reserved, submits, serviceVars, otherVars>>
  /\ UNCHANGED detVars

Expose(n) ==
  /\ stock[n] # 0 /\ ~visible[n]
  /\ visible' = [visible EXCEPT ![n] = TRUE]
  /\ UNCHANGED <<recordVars, stock, maker, nextId, deleteTarget, ack, otherVars>>
  /\ UNCHANGED detVars

Observe ==
  /\ ~poisoned
  /\ view' = [n \in Names |-> IF visible[n] THEN stock[n] ELSE 0]
  /\ UNCHANGED <<recordVars, serviceVars, restarts, restarted, badEffect, badUse, queryVars, failureVars>>
  /\ UNCHANGED detVars

SeeOwned(n) ==
  /\ ~poisoned
  /\ own[n] # 0 /\ view[n] = own[n] /\ ~seen[n]
  /\ seen' = [seen EXCEPT ![n] = TRUE]
  /\ UNCHANGED <<phase, own, flight, submitted, created, reserved, submits, serviceVars, otherVars>>
  /\ UNCHANGED detVars

Ready(n) ==
  /\ ~poisoned
  /\ phase[n] = "pending" /\ flight[n] /\ own[n] # 0 /\ view[n] = own[n] /\ seen[n]
  /\ phase' = [phase EXCEPT ![n] = "ready"] /\ flight' = [flight EXCEPT ![n] = FALSE]
  /\ UNCHANGED <<own, seen, submitted, created, reserved, submits, serviceVars, otherVars>>
  /\ UNCHANGED detVars

LoseReply(n) ==
  /\ phase[n] = "pending" /\ flight[n]
  /\ flight' = [flight EXCEPT ![n] = FALSE]
  /\ UNCHANGED <<phase, own, seen, submitted, created, reserved, submits, serviceVars, otherVars>>
  /\ UNCHANGED detVars

Adopt(n) ==
  /\ ~poisoned
  /\ phase[n] = "pending" /\ ~flight[n] /\ own[n] = 0 /\ view[n] # 0
  /\ own' = [own EXCEPT ![n] = view[n]]
  /\ seen' = [seen EXCEPT ![n] = TRUE]
  /\ phase' = [phase EXCEPT ![n] = "quarantined"]
  /\ UNCHANGED <<flight, submitted, created, reserved, submits, serviceVars, otherVars>>
  /\ UNCHANGED detVars

Quarantine(n) ==
  /\ ~poisoned
  /\ phase[n] = "ready"
  /\ phase' = [phase EXCEPT ![n] = "quarantined"]
  /\ UNCHANGED <<own, seen, flight, submitted, created, reserved, submits, serviceVars, otherVars>>
  /\ UNCHANGED detVars

(* An earlier check can pass before waiting on the admission lock. *)
PrepareUse(n) ==
  /\ ~poisoned /\ own[n] # 0 /\ view[n] = own[n]
  /\ phase[n] = "ready" \/ (Bug = "useDetached" /\ phase[n] = "detached")
  /\ ~prepared[n] /\ prepared' = [prepared EXCEPT ![n] = TRUE]
  /\ UNCHANGED <<recordVars, serviceVars, view, restarts, restarted,
                  badEffect, badUse, queryVars, poisoned>>
  /\ UNCHANGED detVars

FailPersistence ==
  /\ ~poisoned /\ \E n \in Managed : phase[n] = "ready"
  /\ poisoned' = TRUE
  /\ UNCHANGED <<recordVars, serviceVars, view, restarts, restarted,
                  badEffect, badUse, queryVars, prepared>>
  /\ UNCHANGED detVars

Use(n) ==
  /\ prepared[n]
  /\ ~poisoned \/ Bug = "stalePoison"
  /\ own[n] # 0 /\ view[n] = own[n]
  /\ \/ phase[n] = "ready"
     \/ (Bug = "useQuarantined" /\ phase[n] = "quarantined")
     \/ (Bug = "useDetached" /\ phase[n] = "detached")
  /\ LET target == IF Bug = "byName" THEN stock[n] ELSE own[n] IN
     badEffect' = (badEffect \/ (target \in AliveIDs /\ maker[target] # "host"))
  /\ badUse' = (badUse \/ phase[n] # "ready" \/ poisoned)
  /\ badDetached' = (badDetached \/ phase[n] = "detached")
  /\ prepared' = [prepared EXCEPT ![n] = FALSE]
  /\ UNCHANGED <<recordVars, serviceVars, view, restarts, restarted, queryVars, poisoned, det>>

(* A ready sandbox whose run detached: durable, outside the attached cap. *)
Detach(n) ==
  /\ ~poisoned /\ phase[n] = "ready"
  /\ Cardinality(DHolding) < DCAP \/ Bug = "noDetachCap"
  /\ phase' = [phase EXCEPT ![n] = "detached"]
  /\ det' = [det EXCEPT ![n] = TRUE]
  /\ UNCHANGED <<own, seen, flight, submitted, created, reserved, submits, serviceVars,
                  otherVars, badDetached>>

(* Read-only control (state, tail, fetch) or a signal, by recorded ID, in *)
(* any daemon generation. It starts no workload.                          *)
Control(n) ==
  /\ ~poisoned /\ phase[n] = "detached" /\ own[n] # 0 /\ view[n] = own[n]
  /\ LET target == IF Bug = "byName" THEN stock[n] ELSE own[n] IN
     badEffect' = (badEffect \/ (target \in AliveIDs /\ maker[target] # "host"))
  /\ UNCHANGED <<recordVars, serviceVars, view, restarts, restarted, badUse, queryVars,
                  failureVars, detVars>>

BeginRemove(n) ==
  /\ ~poisoned
  /\ own[n] # 0 /\ phase[n] \in {"pending", "ready", "quarantined", "detached"}
  /\ view[n] = 0 \/ view[n] = own[n]
  /\ phase' = [phase EXCEPT ![n] = "removing"]
  /\ UNCHANGED <<own, seen, flight, submitted, created, reserved, submits, serviceVars, otherVars>>
  /\ UNCHANGED detVars

DeleteRequest(n) ==
  /\ ~poisoned
  /\ phase[n] = "removing" /\ ~ack[n]
  /\ LET target == IF Bug = "byName" THEN stock[n] ELSE own[n] IN
     /\ deleteTarget' = [deleteTarget EXCEPT ![n] = target]
     /\ badEffect' = (badEffect \/ (target \in AliveIDs /\ maker[target] # "host"))
  /\ ack' = [ack EXCEPT ![n] = TRUE]
  /\ UNCHANGED <<recordVars, stock, visible, maker, nextId, view, restarts, restarted, badUse, queryVars, failureVars>>
  /\ UNCHANGED detVars

DeleteLands(n) ==
  /\ ack[n] /\ deleteTarget[n] # 0
  /\ stock' = [s \in Names |-> IF stock[s] = deleteTarget[n] THEN 0 ELSE stock[s]]
  /\ visible' = [s \in Names |-> IF stock[s] = deleteTarget[n] THEN FALSE ELSE visible[s]]
  /\ deleteTarget' = [deleteTarget EXCEPT ![n] = 0]
  /\ UNCHANGED <<recordVars, maker, nextId, ack, otherVars>>
  /\ UNCHANGED detVars

(* Each retiring caller retains its own query and prior-seen snapshot. *)
BeginQuery(n) ==
  /\ ~poisoned /\ phase[n] = "removing" /\ ack[n] /\ queryState[n] = "idle"
  /\ querySeen' = [querySeen EXCEPT ![n] = seen[n]]
  /\ queryState' = [queryState EXCEPT ![n] = "issued"]
  /\ UNCHANGED <<recordVars, serviceVars, view, restarts, restarted, badEffect, badUse, queryView, failureVars>>
  /\ UNCHANGED detVars

FinishQuery(n) ==
  /\ ~poisoned /\ phase[n] = "removing" /\ queryState[n] = "issued"
  /\ LET actual == IF visible[n] THEN stock[n] ELSE 0 IN
     /\ queryView' = [queryView EXCEPT ![n] = actual]
     /\ seen' = [seen EXCEPT ![n] = (@ \/ (actual # 0 /\ actual = own[n]))]
  /\ queryState' = [queryState EXCEPT ![n] = "captured"]
  /\ UNCHANGED <<phase, own, flight, submitted, created, reserved, submits,
                  serviceVars, view, restarts, restarted, badEffect, badUse, querySeen, failureVars>>
  /\ UNCHANGED detVars

NextQuery(n) ==
  /\ phase[n] = "removing" /\ queryState[n] = "captured"
  /\ queryState' = [queryState EXCEPT ![n] = "idle"]
  /\ UNCHANGED <<recordVars, serviceVars, view, restarts, restarted, badEffect, badUse,
                  querySeen, queryView, failureVars>>
  /\ UNCHANGED detVars

Forget(n) ==
  /\ ~poisoned
  /\ phase[n] = "removing"
  /\ (queryState[n] = "captured" /\ queryView[n] = 0
      /\ (querySeen[n] \/ Bug = "forgetUnseen" \/ (Bug = "staleSeen" /\ seen[n])))
     \/ (Bug = "ackIsGone" /\ ack[n])
  /\ phase' = [phase EXCEPT ![n] = "none"] /\ own' = [own EXCEPT ![n] = 0]
  /\ seen' = [seen EXCEPT ![n] = FALSE] /\ det' = [det EXCEPT ![n] = FALSE]
  /\ UNCHANGED <<flight, submitted, created, reserved, submits, serviceVars, otherVars,
                  badDetached>>

DropMissing(n) ==
  /\ ~poisoned
  /\ Bug = "dropMissing" /\ phase[n] = "pending" /\ ~flight[n] /\ view[n] = 0
  /\ phase' = [phase EXCEPT ![n] = "none"] /\ own' = [own EXCEPT ![n] = 0]
  /\ UNCHANGED <<seen, flight, submitted, created, reserved, submits, serviceVars, otherVars>>
  /\ UNCHANGED detVars

Restart ==
  /\ restarts < MaxRestarts /\ restarts' = restarts + 1
  /\ flight' = [n \in Managed |-> FALSE]
  /\ restarted' = restarted \cup Holding
  /\ phase' = [n \in Managed |->
       IF phase[n] = "ready" /\ Bug # "restartReady" THEN "quarantined"
       ELSE IF phase[n] = "detached" /\ Bug = "restartDetached" THEN "quarantined"
       ELSE phase[n]]
  /\ poisoned' = FALSE /\ prepared' = [n \in Managed |-> FALSE]
  /\ queryState' = [n \in Managed |-> "idle"]
  /\ UNCHANGED <<own, seen, submitted, created, reserved, submits, serviceVars, view, badEffect, badUse, querySeen, queryView, detVars>>

EnvRemove(n) ==
  /\ stock[n] # 0
  /\ stock' = [stock EXCEPT ![n] = 0] /\ visible' = [visible EXCEPT ![n] = FALSE]
  /\ UNCHANGED <<recordVars, maker, nextId, deleteTarget, ack, otherVars>>
  /\ UNCHANGED detVars

EnvForeign(n) ==
  /\ stock[n] = 0 /\ nextId <= MaxId
  /\ IF n \in Managed THEN own[n] # 0 ELSE TRUE
  /\ stock' = [stock EXCEPT ![n] = nextId] /\ visible' = [visible EXCEPT ![n] = TRUE]
  /\ maker' = [maker EXCEPT ![nextId] = "foreign"] /\ nextId' = nextId + 1
  /\ UNCHANGED <<recordVars, deleteTarget, ack, otherVars>>
  /\ UNCHANGED detVars

Next ==
  \/ \E n \in Managed : Reserve(n) \/ Submit(n) \/ CreateLands(n) \/ Reply(n)
       \/ SeeOwned(n) \/ Ready(n) \/ LoseReply(n) \/ Adopt(n) \/ Quarantine(n)
       \/ PrepareUse(n) \/ Use(n) \/ BeginRemove(n) \/ DeleteRequest(n) \/ DeleteLands(n)
       \/ BeginQuery(n) \/ FinishQuery(n) \/ NextQuery(n) \/ Forget(n) \/ DropMissing(n)
       \/ Detach(n) \/ Control(n)
  \/ \E n \in Names : Expose(n) \/ EnvRemove(n) \/ EnvForeign(n)
  \/ Observe \/ Restart \/ FailPersistence
Spec == Init /\ [][Next]_vars

TypeOK ==
  /\ phase \in [Managed -> Phases] /\ own \in [Managed -> 0..MaxId]
  /\ seen \in [Managed -> BOOLEAN] /\ flight \in [Managed -> BOOLEAN]
  /\ submitted \in [Managed -> BOOLEAN] /\ created \in [Managed -> 0..MaxId]
  /\ reserved \subseteq Managed /\ submits \in [Managed -> 0..2]
  /\ stock \in [Names -> 0..MaxId] /\ visible \in [Names -> BOOLEAN]
  /\ maker \in [IDs -> {"none", "host", "foreign"}] /\ nextId \in 1..(MaxId+1)
  /\ view \in [Names -> 0..MaxId] /\ deleteTarget \in [Managed -> 0..MaxId]
  /\ ack \in [Managed -> BOOLEAN] /\ restarts \in 0..MaxRestarts
  /\ restarted \subseteq Managed /\ badEffect \in BOOLEAN /\ badUse \in BOOLEAN
  /\ queryState \in [Managed -> {"idle", "issued", "captured"}]
  /\ querySeen \in [Managed -> BOOLEAN] /\ queryView \in [Managed -> 0..MaxId]
  /\ poisoned \in BOOLEAN /\ prepared \in [Managed -> BOOLEAN]
  /\ det \in [Managed -> BOOLEAN] /\ badDetached \in BOOLEAN
NoForeignEffect == ~badEffect
OwnedAreOurs == \A n \in Managed : own[n] # 0 => maker[own[n]] = "host"
NoLeak == \A n \in Managed : created[n] \in AliveIDs =>
  phase[n] # "none" /\ own[n] \in {0, created[n]}
NoCreateReplay == \A n \in Managed : submits[n] <= 1
CapacityBound == Cardinality(Holding) <= CAP
RestartInert == \A n \in restarted : phase[n] # "ready"
NoQuarantinedUse == ~badUse
NoUnsafeSubmission == ~badUse
DetachedBound == Cardinality(DHolding) <= DCAP
\* A detached sandbox leaves its state only for removal, across restarts.
DetachedSurvives == \A n \in Managed : det[n] => phase[n] \in {"detached", "removing"}
NoDetachedUse == ~badDetached
=============================================================================
