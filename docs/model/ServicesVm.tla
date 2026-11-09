---------------------------- MODULE ServicesVm ----------------------------
(* A project's services VM (`docs/design/services.md`; built in            *)
(* `marsh-sbx/src/services_vm.rs` and `marsh-backend/src/services.rs`).    *)
(* One VM per project, recorded in the ownership map with its UUID (VM     *)
(* naming and UUID-checked operations are Ownership.tla: a services VM is  *)
(* one more owned name). This module is the new VM kind's lifecycle, with  *)
(* the DISTINCT transitions DECISIONS 13 and 18 give it:                   *)
(*   Start / Acquire / Release  create or start the VM; lease holders      *)
(*   IdleExpire                 stop the VM, keep it                       *)
(*   Disable(p) / Enable(p)     services on/off PER PROJECT (A11): Disable *)
(*                              revokes p's holders, stops p's VM, keeps   *)
(*                              the data; Start/Acquire need enabled[p]    *)
(*   StopCmd                    `marsh stop`: stop every VM, keep the data *)
(*   ResetCmd                   default `marsh reset`: services untouched  *)
(*   ResetAll(c)                `reset --include-services`: remove the VMs *)
(*                              and data of exactly the CONFIRMED projects *)
(*   Rm(p)                      `marsh services rm`: one project's VM+data *)
(*   Restart                    daemon restart: holders gone, VMs stay     *)
(* `held` (who really uses a VM) and `data` (a project's data exists) are  *)
(* ghosts: `data` is cleared only by a confirmed removal, so a break that  *)
(* destroys a VM leaves it set. `last`, `po` and `ps` record the latest    *)
(* transition and the state before it, so each contract is a state         *)
(* invariant on what that transition did.                                  *)
EXTENDS Naturals, FiniteSets

CONSTANTS Projects, Ids, Holders, MaxRestarts, Bug

Bugs == {"none", "vmPerScope", "leaseNone", "idleRemoves", "gcServices", "idleStopOff",
         "stopDeletesData", "resetSkipConfirmation", "resetIncludeNoop",
         "rmRemovesAll", "disableIgnoresHolders", "enablePerHome"}
ASSUME Bug \in Bugs

VARIABLES
  own,      \* [Projects -> Ids \cup {0}] the recorded VM of the project (0: none)
  stock,    \* [Ids -> "none" | "running" | "stopped"] stock SBX's table
  holders,  \* [Projects -> SUBSET Holders] what marsh recorded
  held,     \* [Projects -> SUBSET Holders] ghost: who really uses the VM
  idle,     \* [Projects -> BOOLEAN] the idle timer is armed
  data,     \* [Projects -> BOOLEAN] ghost: the project's data exists
  enabled,  \* [Projects -> BOOLEAN] services are on for the project (A11: per project)
  restarts,
  last,     \* the latest transition: its tag and the projects it names
  po, ps    \* own and stock before that transition

core == <<own, stock, holders, held, idle, data, enabled, restarts>>
vars == <<own, stock, holders, held, idle, data, enabled, restarts, last, po, ps>>

Running(p) == own[p] # 0 /\ stock[own[p]] = "running"
Exists(p) == own[p] # 0 /\ stock[own[p]] # "none"

TypeOK ==
  /\ own \in [Projects -> Ids \cup {0}]
  /\ stock \in [Ids -> {"none", "running", "stopped"}]
  /\ holders \in [Projects -> SUBSET Holders]
  /\ held \in [Projects -> SUBSET Holders]
  /\ idle \in [Projects -> BOOLEAN]
  /\ data \in [Projects -> BOOLEAN]
  /\ enabled \in [Projects -> BOOLEAN]
  /\ restarts \in 0..MaxRestarts
  /\ last.arg \subseteq Projects

Init ==
  /\ own = [p \in Projects |-> 0]
  /\ stock = [i \in Ids |-> "none"]
  /\ holders = [p \in Projects |-> {}]
  /\ held = [p \in Projects |-> {}]
  /\ idle = [p \in Projects |-> FALSE]
  /\ data = [p \in Projects |-> FALSE]
  /\ enabled = [p \in Projects |-> TRUE]
  /\ restarts = 0
  /\ last = [tag |-> "init", arg |-> {}]
  /\ po = own
  /\ ps = stock

\* Every transition records itself and the state it started from.
Step(tag, arg) == /\ last' = [tag |-> tag, arg |-> arg] /\ po' = own /\ ps' = stock

\* The VM Ensure(p) uses: p's own, or (the break) any project's.
Shared(p) == IF Bug = "vmPerScope" /\ \E q \in Projects : own[q] # 0
             THEN CHOOSE q \in Projects : own[q] # 0
             ELSE p

\* Create (record intent, create, adopt UUID) or start the project's VM.
Ensure(p) ==
  \/ /\ own[Shared(p)] # 0
     /\ stock' = [stock EXCEPT ![own[Shared(p)]] = "running"]
     /\ own' = [own EXCEPT ![p] = own[Shared(p)]]
  \/ /\ own[Shared(p)] = 0
     /\ \E i \in Ids :
          /\ stock[i] = "none"
          /\ \A q \in Projects : own[q] # i
          /\ stock' = [stock EXCEPT ![i] = "running"]
          /\ own' = [own EXCEPT ![p] = i]

\* Services are on for p. The break: any enabled project enables every project.
OnFor(p) == IF Bug = "enablePerHome" THEN \E q \in Projects : enabled[q] ELSE enabled[p]

Start(p) ==
  /\ OnFor(p)
  /\ Ensure(p)
  /\ data' = [data EXCEPT ![p] = TRUE]
  /\ idle' = [idle EXCEPT ![p] = holders[p] = {}]
  /\ UNCHANGED <<holders, held, enabled, restarts>>
  /\ Step("start", {p})

Acquire(p, h) ==
  /\ OnFor(p)
  /\ Ensure(p)
  /\ h \notin held[p]
  /\ held' = [held EXCEPT ![p] = @ \cup {h}]
  \* break: the holder is not recorded, so nothing disarms the timer
  /\ holders' = IF Bug = "leaseNone" THEN holders
                ELSE [holders EXCEPT ![p] = @ \cup {h}]
  /\ idle' = [idle EXCEPT ![p] = IF Bug = "leaseNone" THEN @ ELSE FALSE]
  /\ data' = [data EXCEPT ![p] = TRUE]
  /\ UNCHANGED <<enabled, restarts>>
  /\ Step("acquire", {p})

Release(p, h) ==
  /\ h \in held[p]
  /\ held' = [held EXCEPT ![p] = @ \ {h}]
  /\ holders' = [holders EXCEPT ![p] = @ \ {h}]
  /\ idle' = [idle EXCEPT ![p] = (holders[p] \ {h}) = {}]
  /\ UNCHANGED <<own, stock, data, enabled, restarts>>
  /\ Step("release", {p})

\* The idle timer expires: stop the VM (the breaks: remove it, or do nothing).
IdleExpire(p) ==
  /\ idle[p]
  /\ holders[p] = {}
  /\ Running(p)
  /\ Bug # "idleStopOff"
  /\ idle' = [idle EXCEPT ![p] = FALSE]
  /\ IF Bug = "idleRemoves"
     THEN /\ stock' = [stock EXCEPT ![own[p]] = "none"]
          /\ own' = [own EXCEPT ![p] = 0]
     ELSE /\ stock' = [stock EXCEPT ![own[p]] = "stopped"]
          /\ UNCHANGED own
  /\ UNCHANGED <<holders, held, data, enabled, restarts>>
  /\ Step("idle", {p})

\* Turning services off for ONE project: revoke its holders, stop its VM, keep
\* the data (the break leaves a held VM running).
Disable(p) ==
  /\ enabled[p]
  /\ enabled' = [enabled EXCEPT ![p] = FALSE]
  /\ holders' = [holders EXCEPT ![p] = {}]
  /\ held' = [held EXCEPT ![p] =
        IF Bug = "disableIgnoresHolders" /\ held[p] # {} THEN held[p] ELSE {}]
  /\ idle' = [idle EXCEPT ![p] = FALSE]
  /\ stock' = [i \in Ids |->
        IF own[p] = i /\ stock[i] = "running"
           /\ ~(Bug = "disableIgnoresHolders" /\ held[p] # {})
        THEN "stopped" ELSE stock[i]]
  /\ UNCHANGED <<own, data, restarts>>
  /\ Step("off", {p})

Enable(p) ==
  /\ ~enabled[p]
  /\ enabled' = [enabled EXCEPT ![p] = TRUE]
  /\ UNCHANGED <<own, stock, holders, held, idle, data, restarts>>
  /\ Step("on", {p})

\* `marsh stop`: every VM is stopped and kept, with its data (the break
\* removes them).
StopCmd ==
  /\ holders' = [p \in Projects |-> {}]
  /\ held' = [p \in Projects |-> {}]
  /\ idle' = [p \in Projects |-> FALSE]
  /\ IF Bug = "stopDeletesData"
     THEN /\ stock' = [i \in Ids |-> "none"]
          /\ own' = [p \in Projects |-> 0]
     ELSE /\ stock' = [i \in Ids |-> IF stock[i] = "running" THEN "stopped" ELSE stock[i]]
          /\ UNCHANGED own
  /\ UNCHANGED <<data, enabled, restarts>>
  /\ Step("stop", {})

\* Default `marsh reset`: services VMs are skipped entirely.
ResetCmd ==
  /\ UNCHANGED core
  /\ Step("reset", {})

\* `marsh reset --include-services` with the projects the user confirmed:
\* exactly those VMs (and their data) go. The breaks: remove every project's
\* VM whatever was confirmed, or accept the flag and remove nothing.
ResetAll(c) ==
  LET gone == IF Bug = "resetSkipConfirmation" THEN Projects ELSE c IN
  /\ IF Bug = "resetIncludeNoop"
     THEN UNCHANGED <<own, stock, holders, held, idle, data>>
     ELSE /\ stock' = [i \in Ids |-> IF \E p \in gone : own[p] = i THEN "none" ELSE stock[i]]
          /\ own' = [p \in Projects |-> IF p \in gone THEN 0 ELSE own[p]]
          /\ holders' = [p \in Projects |-> IF p \in gone THEN {} ELSE holders[p]]
          /\ held' = [p \in Projects |-> IF p \in gone THEN {} ELSE held[p]]
          /\ idle' = [p \in Projects |-> IF p \in gone THEN FALSE ELSE idle[p]]
          \* data is destroyed only for a CONFIRMED project
          /\ data' = [p \in Projects |-> IF p \in c THEN FALSE ELSE data[p]]
  /\ UNCHANGED <<enabled, restarts>>
  /\ Step("resetAll", c)

\* `marsh services rm`: one project's VM and data (the break removes all).
Rm(p) ==
  /\ own[p] # 0
  /\ LET gone == IF Bug = "rmRemovesAll" THEN Projects ELSE {p} IN
     /\ stock' = [i \in Ids |-> IF \E q \in gone : own[q] = i THEN "none" ELSE stock[i]]
     /\ own' = [q \in Projects |-> IF q \in gone \/ own[q] = own[p] THEN 0 ELSE own[q]]
     /\ holders' = [q \in Projects |-> IF q \in gone THEN {} ELSE holders[q]]
     /\ held' = [q \in Projects |-> IF q \in gone THEN {} ELSE held[q]]
     /\ idle' = [q \in Projects |-> IF q \in gone THEN FALSE ELSE idle[q]]
     /\ data' = [q \in Projects |-> IF q = p \/ own[q] = own[p] THEN FALSE ELSE data[q]]
  /\ UNCHANGED <<enabled, restarts>>
  /\ Step("rm", {p})

\* Garbage collection removes a stopped VM only through this break.
Gc(p) ==
  /\ Bug = "gcServices"
  /\ own[p] # 0
  /\ stock[own[p]] = "stopped"
  /\ stock' = [stock EXCEPT ![own[p]] = "none"]
  /\ own' = [own EXCEPT ![p] = 0]
  /\ UNCHANGED <<holders, held, idle, data, enabled, restarts>>
  /\ Step("gc", {p})

\* The daemon restarts: holders (and their leases) are gone, every running
\* VM gets an idle timer. Nothing is removed.
Restart ==
  /\ restarts < MaxRestarts
  /\ restarts' = restarts + 1
  /\ holders' = [p \in Projects |-> {}]
  /\ held' = [p \in Projects |-> {}]
  /\ idle' = [p \in Projects |-> Running(p)]
  /\ UNCHANGED <<own, stock, data, enabled>>
  /\ Step("restart", {})

Next ==
  \/ \E p \in Projects : Start(p) \/ IdleExpire(p) \/ Rm(p) \/ Gc(p)
  \/ \E p \in Projects, h \in Holders : Acquire(p, h) \/ Release(p, h)
  \/ \E c \in SUBSET Projects : ResetAll(c)
  \/ \E p \in Projects : Disable(p) \/ Enable(p)
  \/ StopCmd \/ ResetCmd \/ Restart

Spec == Init /\ [][Next]_vars /\ \A p \in Projects : WF_vars(IdleExpire(p))

\* One VM per project: two projects never share a VM.
OneVmPerProject ==
  \A p, q \in Projects : (p # q /\ own[p] # 0 /\ own[q] # 0) => own[p] # own[q]

\* A VM someone really holds is running: idle never stops a held VM.
HeldRuns == \A p \in Projects : held[p] # {} => Running(p)

\* A project's data outlives every transition except a confirmed removal.
DataKept == \A p \in Projects : data[p] => Exists(p)

\* `marsh stop` keeps every services VM it found (DECISIONS 18).
StopKeepsServices ==
  last.tag = "stop" => \A p \in Projects : po[p] # 0 => (own[p] = po[p] /\ Exists(p))

\* The default `marsh reset` leaves services alone entirely.
ResetKeepsServices == last.tag = "reset" => (own = po /\ stock = ps)

\* `reset --include-services` removes every confirmed project's VM ...
ResetAllRemovesConfirmed ==
  last.tag = "resetAll" => \A p \in last.arg : own[p] = 0

\* ... and nobody else's, which stays exactly as it was.
ResetAllSparesUnconfirmed ==
  last.tag = "resetAll" =>
    \A p \in Projects \ last.arg : own[p] = po[p] /\ (own[p] # 0 => stock[own[p]] = ps[own[p]])

\* `marsh services rm` removes the one project's VM and only that.
RmRemovesOne ==
  last.tag = "rm" =>
    \A p \in Projects :
      IF p \in last.arg THEN own[p] = 0
      ELSE (po[p] # 0 /\ po[p] # po[CHOOSE q \in last.arg : TRUE]) => own[p] = po[p]

\* Turning services off for a project leaves it no holder and no running VM.
OffStopsProject ==
  last.tag = "off" =>
    \A p \in last.arg : holders[p] = {} /\ ~Running(p)

\* A VM is started or leased only for a project services are on for (A11).
StartsOnlyWhenEnabled ==
  last.tag \in {"start", "acquire"} => \A p \in last.arg : enabled[p]

\* An armed idle timer with no holder ends with the VM stopped, unless
\* a holder arrives.
IdleStops ==
  \A p \in Projects :
    (idle[p] /\ holders[p] = {} /\ Running(p)) ~> (~Running(p) \/ holders[p] # {})
=============================================================================
