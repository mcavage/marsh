---------------------------- MODULE Ownership ----------------------------
(* Stock-SBX VM ownership for one daemon (one selected home) and the       *)
(* development grants it serves to `marsh --dev` sessions.                 *)
(*                                                                         *)
(* Stock SBX is modeled as a name -> VM table that independent actors can  *)
(* change at any time (stop, remove, same-name create with a new UUID).    *)
(* The daemon keeps a persisted ownership map name -> UUID, a cached view  *)
(* refreshed by one inventory (`sbx ls`) only when the view is invalid     *)
(* (cold start, after a lifecycle change, after a failure), and a          *)
(* persisted create intent (the chosen name) written before `sbx create`.  *)
(* A later inventory adopts an intent whose name is present and drops one  *)
(* whose name is absent.                                                   *)
(*                                                                         *)
(* Each grant has its own persisted child map and intents over a random    *)
(* per-grant name prefix (PrefixOf[g]), uses the same intent-then-adopt    *)
(* create, is revoked on session end and on every daemon restart, and is   *)
(* cleaned by the host after revocation.                                   *)
(*                                                                         *)
(* Assumption A1: host `sbx` is trusted; host and grant VM names are a     *)
(* prefix plus a random id chosen by this daemon (grants reach stock only  *)
(* through the broker), so no other actor creates a VM under such a name   *)
(* (no EnvCreate on HostNames \cup ChildNames). Stock operations may       *)
(* therefore be name-addressed.                                            *)
(* Assumption A2: stock create is atomic on name uniqueness.               *)
(* Assumption A3: inventory is accurate at observation time.               *)
EXTENDS Naturals, FiniteSets

CONSTANTS HostNames,   \* names the host daemon owns (shell VM, kit VMs)
          ChildNames,  \* union of all grant prefixes
          Grants,      \* development grants (one per --dev session)
          PrefixOf,    \* [Grants -> SUBSET ChildNames], pairwise disjoint
          MaxU,        \* bound on UUIDs ever allocated (finite model)
          ChildMax,    \* max VMs (owned + intents) per grant
          Bug

ASSUME HostNames \cap ChildNames = {}
ASSUME \A g, h \in Grants : g # h => PrefixOf[g] \cap PrefixOf[h] = {}
ASSUME \A g \in Grants : PrefixOf[g] \subseteq ChildNames

Bugs == {"none", "noIntent", "childTouchHost", "childIgnoreRevoke", "childOverMax",
         "crossGrant", "childNoIntent", "restartKeepsGrant"}
ASSUME Bug \in Bugs

Names == HostNames \cup ChildNames
UU == 1..MaxU
NoVM == [u |-> 0, run |-> FALSE]
Intents == {"none", "want", "sent"}

VARIABLES
  stock,     \* [Names -> VM record]  stock SBX truth (u = 0: absent)
  nextU,     \* next fresh UUID
  maker,     \* ghost: [UU -> creator] "none" | "host" | "ext" | grant
  own,       \* host persisted ownership map name -> UUID (0 = none)
  intent,    \* host persisted create intent
  view,      \* cached inventory
  viewOk,    \* cached inventory valid?
  cown,      \* [Grants -> [Names -> UUID]] host-persisted per-grant child map
  cintent,   \* [Grants -> [Names -> Intents]] host-persisted grant intents
  crevoked,  \* [Grants -> BOOLEAN] grant revoked (persisted)
  csession,  \* [Grants -> BOOLEAN] in-memory session/relay of the grant alive
  hostBad,   \* ghost: host affected a VM it does not own
  childBad,  \* ghost: a grant affected a VM it does not own
  revokedBad \* ghost: grant effect after revoke or without its session

vars == <<stock, nextU, maker, own, intent, view, viewOk, cown, cintent,
          crevoked, csession, hostBad, childBad, revokedBad>>
childVars == <<cown, cintent, crevoked, csession>>
ghosts == <<hostBad, childBad, revokedBad>>

TypeOK ==
  /\ stock \in [Names -> [u : 0..MaxU, run : BOOLEAN]]
  /\ nextU \in 1..(MaxU + 1)
  /\ own \in [Names -> 0..MaxU]
  /\ cown \in [Grants -> [Names -> 0..MaxU]]
  /\ intent \in [Names -> Intents]
  /\ cintent \in [Grants -> [Names -> Intents]]
  /\ viewOk \in BOOLEAN
  /\ crevoked \in [Grants -> BOOLEAN] /\ csession \in [Grants -> BOOLEAN]

Init ==
  /\ stock = [n \in Names |-> NoVM]
  /\ nextU = 1
  /\ maker = [u \in UU |-> "none"]
  /\ own = [n \in Names |-> 0]
  /\ intent = [n \in Names |-> "none"]
  /\ view = [n \in Names |-> NoVM]
  /\ viewOk = FALSE
  /\ cown = [g \in Grants |-> [n \in Names |-> 0]]
  /\ cintent = [g \in Grants |-> [n \in Names |-> "none"]]
  /\ crevoked = [g \in Grants |-> FALSE]
  /\ csession = [g \in Grants |-> TRUE]
  /\ hostBad = FALSE /\ childBad = FALSE /\ revokedBad = FALSE

\* Stock operations are name-addressed for every owned name (A1) ...
CondOk(n, u) == u # 0 /\ stock[n].u # 0
\* ... and land on whatever VM holds the name.
Landed(n) == stock[n].u

HostEffect(n)     == hostBad' = (hostBad \/ maker[Landed(n)] # "host")
ChildEffect(g, n) == /\ childBad' = (childBad \/ maker[Landed(n)] # g)
                     /\ revokedBad' = (revokedBad \/ crevoked[g] \/ ~csession[g])
ChildMayAct(g)    == ~crevoked[g] \/ Bug = "childIgnoreRevoke"
GrantCount(g)     == Cardinality({n \in Names : cown[g][n] # 0 \/ cintent[g][n] # "none"})

---------------------------------------------------------------------------
(* Environment: stock SBX and other clients.                               *)

EnvStop(n) ==
  /\ stock[n].u # 0 /\ stock[n].run
  /\ stock' = [stock EXCEPT ![n].run = FALSE]
  /\ UNCHANGED <<nextU, maker, own, intent, view, viewOk, childVars, ghosts>>

EnvRemove(n) ==
  /\ stock[n].u # 0
  /\ stock' = [stock EXCEPT ![n] = NoVM]
  /\ UNCHANGED <<nextU, maker, own, intent, view, viewOk, childVars, ghosts>>

EnvCreate(n) ==
  /\ n \notin HostNames \cup ChildNames      \* A1: random daemon-chosen names
  /\ stock[n].u = 0 /\ nextU <= MaxU
  /\ stock' = [stock EXCEPT ![n] = [u |-> nextU, run |-> TRUE]]
  /\ maker' = [maker EXCEPT ![nextU] = "ext"]
  /\ nextU' = nextU + 1
  /\ UNCHANGED <<own, intent, view, viewOk, childVars, ghosts>>

---------------------------------------------------------------------------
(* Host daemon.                                                            *)

Inventory ==
  /\ ~viewOk
  /\ view' = stock /\ viewOk' = TRUE
  /\ UNCHANGED <<stock, nextU, maker, own, intent, childVars, ghosts>>

\* Use a running target VM (open worker transport / exec / mount).
Use(n) ==
  LET t == own[n] IN
  /\ n \in HostNames /\ viewOk
  /\ t # 0 /\ view[n].u = t /\ view[n].run
  /\ IF stock[n].run /\ CondOk(n, t)
       THEN /\ HostEffect(n) /\ UNCHANGED viewOk
       ELSE /\ viewOk' = FALSE /\ UNCHANGED hostBad   \* failure -> re-inventory
  /\ UNCHANGED <<stock, nextU, maker, own, intent, view, childVars,
                 childBad, revokedBad>>

StartVM(n) ==
  /\ n \in HostNames /\ viewOk
  /\ own[n] # 0 /\ view[n].u = own[n] /\ ~view[n].run
  /\ IF CondOk(n, own[n])
       THEN /\ stock' = [stock EXCEPT ![n].run = TRUE] /\ HostEffect(n)
       ELSE UNCHANGED <<stock, hostBad>>
  /\ viewOk' = FALSE
  /\ UNCHANGED <<nextU, maker, own, intent, view, childVars, childBad, revokedBad>>

\* Absent name: persist intent (chosen random name) before creating.
CreateBegin(n) ==
  /\ n \in HostNames /\ viewOk /\ view[n].u = 0 /\ intent[n] = "none"
  /\ intent' = [intent EXCEPT ![n] = "want"]
  /\ own' = [own EXCEPT ![n] = 0]
  /\ UNCHANGED <<stock, nextU, maker, view, viewOk, childVars, ghosts>>

\* `sbx create`. The next inventory adopts the intent by its random name.
CreateDo(n) ==
  /\ n \in HostNames /\ intent[n] = "want" /\ nextU <= MaxU
  /\ IF stock[n].u = 0
       THEN /\ stock' = [stock EXCEPT ![n] = [u |-> nextU, run |-> TRUE]]
            /\ maker' = [maker EXCEPT ![nextU] = "host"]
            /\ nextU' = nextU + 1
       ELSE UNCHANGED <<stock, maker, nextU>>
  /\ intent' = [intent EXCEPT ![n] = "sent"]
  /\ viewOk' = FALSE
  /\ UNCHANGED <<own, view, childVars, ghosts>>

Record(n) ==
  /\ n \in HostNames /\ intent[n] = "sent" /\ viewOk
  /\ IF view[n].u # 0
       THEN own' = [own EXCEPT ![n] = view[n].u]   \* adopt by our random name
       ELSE UNCHANGED own                          \* absent: drop the intent
  /\ intent' = [intent EXCEPT ![n] = "none"]
  /\ UNCHANGED <<stock, nextU, maker, view, viewOk, childVars, ghosts>>

\* Daemon restart: the cached view and every in-memory session are lost;
\* persisted maps and intents are kept and every persisted grant is revoked.
\* Bug "noIntent"/"childNoIntent" keep intents only in memory; bug
\* "restartKeepsGrant" leaves a grant usable without its session.
Restart ==
  /\ viewOk' = FALSE
  /\ intent' = IF Bug = "noIntent" THEN [n \in Names |-> "none"] ELSE intent
  /\ cintent' = IF Bug = "childNoIntent"
                  THEN [g \in Grants |-> [n \in Names |-> "none"]] ELSE cintent
  /\ crevoked' = IF Bug = "restartKeepsGrant" THEN crevoked ELSE [g \in Grants |-> TRUE]
  /\ csession' = [g \in Grants |-> FALSE]
  /\ UNCHANGED <<stock, nextU, maker, own, view, cown, ghosts>>

\* Stop/remove an owned VM by its recorded UUID (incl. operator Retire).
Remove(n) ==
  LET t == own[n] IN
  /\ n \in HostNames /\ viewOk /\ t # 0 /\ view[n].u = t
  /\ IF CondOk(n, t)
       THEN /\ stock' = [stock EXCEPT ![n] = NoVM] /\ HostEffect(n)
       ELSE UNCHANGED <<stock, hostBad>>
  /\ own' = [own EXCEPT ![n] = 0]
  /\ viewOk' = FALSE
  /\ UNCHANGED <<nextU, maker, intent, view, childVars, childBad, revokedBad>>

---------------------------------------------------------------------------
(* Dev broker: DevSbx calls of grant g, checked against g's map only.      *)

ChildCreateBegin(g, n) ==
  /\ ChildMayAct(g) /\ n \in PrefixOf[g] /\ viewOk
  /\ view[n].u = 0 /\ cown[g][n] = 0 /\ cintent[g][n] = "none"
  /\ GrantCount(g) < ChildMax \/ Bug = "childOverMax"
  /\ cintent' = [cintent EXCEPT ![g][n] = "want"]
  /\ UNCHANGED <<stock, nextU, maker, own, intent, view, viewOk, cown,
                 crevoked, csession, ghosts>>

\* The forwarded `sbx create --name n` (an in-flight call is atomic here).
ChildCreateDo(g, n) ==
  /\ ChildMayAct(g) /\ cintent[g][n] = "want" /\ nextU <= MaxU
  /\ IF stock[n].u = 0
       THEN /\ stock' = [stock EXCEPT ![n] = [u |-> nextU, run |-> TRUE]]
            /\ maker' = [maker EXCEPT ![nextU] = g]
            /\ nextU' = nextU + 1
            /\ revokedBad' = (revokedBad \/ crevoked[g] \/ ~csession[g])
       ELSE UNCHANGED <<stock, maker, nextU, revokedBad>>
  /\ cintent' = [cintent EXCEPT ![g][n] = "sent"]
  /\ viewOk' = FALSE
  /\ UNCHANGED <<own, intent, view, cown, crevoked, csession, hostBad, childBad>>

\* Host bookkeeping: the next inventory adopts or drops a sent intent.
ChildRecord(g, n) ==
  /\ cintent[g][n] = "sent" /\ viewOk
  /\ IF view[n].u # 0
       THEN cown' = [cown EXCEPT ![g][n] = view[n].u]
       ELSE UNCHANGED cown
  /\ cintent' = [cintent EXCEPT ![g][n] = "none"]
  /\ UNCHANGED <<stock, nextU, maker, own, intent, view, viewOk, crevoked,
                 csession, ghosts>>

\* Any forwarded effect (exec, mount, cp, stop) on a VM of the grant whose
\* map h the broker consulted; correct code consults only the caller's map.
ChildOp(g, h, n) ==
  LET t == IF Bug = "childTouchHost" /\ own[n] # 0 THEN own[n] ELSE cown[h][n] IN
  /\ h = g \/ Bug = "crossGrant"
  /\ ChildMayAct(g) /\ viewOk /\ t # 0 /\ view[n].u = t
  /\ IF CondOk(n, t)
       THEN /\ stock' = [stock EXCEPT ![n].run = ~@] /\ ChildEffect(g, n)
            /\ UNCHANGED viewOk
       ELSE /\ viewOk' = FALSE /\ UNCHANGED <<stock, childBad, revokedBad>>
  /\ UNCHANGED <<nextU, maker, own, intent, view, childVars, hostBad>>

ChildRemove(g, n) ==
  /\ ChildMayAct(g) /\ n \in PrefixOf[g] /\ viewOk
  /\ cown[g][n] # 0 /\ view[n].u = cown[g][n]
  /\ IF CondOk(n, cown[g][n])
       THEN /\ stock' = [stock EXCEPT ![n] = NoVM] /\ ChildEffect(g, n)
       ELSE UNCHANGED <<stock, childBad, revokedBad>>
  /\ cown' = [cown EXCEPT ![g][n] = 0]
  /\ viewOk' = FALSE
  /\ UNCHANGED <<nextU, maker, own, intent, view, cintent, crevoked, csession, hostBad>>

\* Session exit, relay loss, shell retire, or `marsh stop`/`marsh reset`.
RevokeChild(g) ==
  /\ ~crevoked[g]
  /\ crevoked' = [crevoked EXCEPT ![g] = TRUE]
  /\ csession' = [csession EXCEPT ![g] = FALSE]
  /\ UNCHANGED <<stock, nextU, maker, own, intent, view, viewOk, cown, cintent, ghosts>>

\* After revoke: adopt-or-drop intents, then remove the grant's VMs.
HostAdoptChild(g, n) ==
  /\ crevoked[g] /\ viewOk /\ cintent[g][n] # "none"
  /\ IF view[n].u # 0 /\ cintent[g][n] = "sent"
       THEN cown' = [cown EXCEPT ![g][n] = view[n].u]
       ELSE UNCHANGED cown
  /\ cintent' = [cintent EXCEPT ![g][n] = "none"]
  /\ UNCHANGED <<stock, nextU, maker, own, intent, view, viewOk, crevoked,
                 csession, ghosts>>

HostCleanChild(g, n) ==
  /\ crevoked[g] /\ viewOk /\ cown[g][n] # 0 /\ cintent[g][n] = "none"
  /\ IF view[n].u = cown[g][n] /\ CondOk(n, cown[g][n])
       THEN /\ stock' = [stock EXCEPT ![n] = NoVM]
            /\ hostBad' = (hostBad \/ maker[Landed(n)] # g)
       ELSE UNCHANGED <<stock, hostBad>>
  /\ cown' = [cown EXCEPT ![g][n] = 0]
  /\ viewOk' = FALSE
  /\ UNCHANGED <<nextU, maker, own, intent, view, cintent, crevoked, csession,
                 childBad, revokedBad>>

---------------------------------------------------------------------------
Next ==
  \/ \E n \in Names : EnvStop(n) \/ EnvRemove(n) \/ EnvCreate(n)
  \/ Inventory \/ Restart
  \/ \E n \in Names : Use(n) \/ StartVM(n) \/ CreateBegin(n) \/ CreateDo(n)
                      \/ Record(n) \/ Remove(n)
  \/ \E g \in Grants : RevokeChild(g)
  \/ \E g \in Grants, n \in Names :
        \/ ChildCreateBegin(g, n) \/ ChildCreateDo(g, n) \/ ChildRecord(g, n)
        \/ ChildRemove(g, n) \/ HostAdoptChild(g, n) \/ HostCleanChild(g, n)
        \/ \E h \in Grants : ChildOp(g, h, n)

Spec == /\ Init /\ [][Next]_vars
        /\ WF_vars(Inventory)
        \* Cleanup is retried after every restart/inventory: strong fairness.
        /\ SF_vars(\E g \in Grants, n \in Names : HostAdoptChild(g, n) \/ HostCleanChild(g, n))

---------------------------------------------------------------------------
(* Safety *)
NoForeignEffect   == ~hostBad
OwnedAreOurs      == \A n \in Names : own[n] # 0 => maker[own[n]] = "host"
\* No created-but-unowned leak: every host-made VM is recorded or intended.
NoLeak            == \A n \in HostNames :
                       (stock[n].u # 0 /\ maker[stock[n].u] = "host")
                         => (own[n] = stock[n].u \/ intent[n] # "none")
\* The same for every grant, so revocation can find every child VM.
ChildNoLeak       == \A g \in Grants : \A n \in PrefixOf[g] :
                       (stock[n].u # 0 /\ maker[stock[n].u] = g)
                         => (cown[g][n] = stock[n].u \/ cintent[g][n] # "none")
ChildConfinement  == /\ ~childBad
                     /\ \A g \in Grants, n \in Names : cown[g][n] # 0 => maker[cown[g][n]] = g
RevokedChildInert == ~revokedBad
ChildCapacity     == \A g \in Grants : GrantCount(g) <= ChildMax
MapsDisjoint      == \A n \in Names, g, h \in Grants :
                       /\ ~(own[n] # 0 /\ cown[g][n] # 0)
                       /\ (g # h => ~(cown[g][n] # 0 /\ cown[h][n] # 0))

(* Liveness: after revoke, the host eventually forgets/cleans every child VM. *)
ChildCleanedAfterRevoke ==
  \A g \in Grants : crevoked[g] ~> (\A n \in Names : cown[g][n] = 0 /\ cintent[g][n] = "none")

\* Model value for configs: two grants, g1 with two names (capacity tests).
MCPrefixOf == [g \in Grants |-> IF g = "g1" THEN {"c1", "c3"} ELSE {"c2"}]
=============================================================================
