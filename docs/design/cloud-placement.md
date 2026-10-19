# Run shell work locally or in Docker Cloud

Status: **session-origin slice implemented as a preview; the rest proposed.**
This document specifies the agreed product direction and the remaining release
gates. `cloud { LIST; }` and `marsh cloud` from the session (transport and
workspace slices, plus split-fork and fanout destinations) are implemented and
pass the live [cloud acceptance](../plan/cloud-dispatch.md); the user manual is
[Docker Cloud (preview)](../cloud.md). The private Git copy, sandbox options, the
interactive cloud shell (C14), `split` inside a block (C07, sandbox-local), and
registered Kits inside a block and job-origin placement in the parent's own
Kit (snapshot crossings; see the decisions of 2026-10-17 and 2026-10-18) are
implemented too. Everything else here (`marsh run --place`, cloud -> laptop
crossings, live remote workspace sharing) is proposed, not implemented;
`--place` and cloud -> laptop are recorded as v2 work. The [local contract](../plan/local-product-contract.md)
is unchanged.

## 1. Product promise

marsh is a local Unix command center. `cloud { LIST; }` runs a shell list in
Docker Cloud, preserving ordinary command streams and returning filesystem
changes to the caller's workspace. Placement changes execution location and
file visibility timing; split changes isolation; fanout changes concurrency.

The primary user is a marsh user dispatching work from their laptop. This is
not a claim of measured customer demand or performance. Interactive cloud
shells are part of the intended product, after local dispatch works.

Requirements: a **Docker Cloud subscription with sandbox access**, stock `sbx`
signed in to the intended account, cloud quota, and cloud-side provider setup
for any agent being used. A model-provider subscription/key alone does not
supply Docker Cloud compute. Compute, storage, transfers, and model calls may
have separate charges. Exact entitlement names, prices, and minimum supported
SBX version must be checked against the platform before release.

### Decisions already made

- Cloud placement is useful outside split, including in ordinary pipelines.
- Cloud uses workspace snapshots, not live host filesystem synchronization.
- Workspace deltas return automatically; there is no `--write-back` flag.
- A one-branch split is the way to keep returned edits off the source tree.
- Concurrent overlapping writes may overwrite one another. There is no merge
  engine, conflict prompt, or optimistic-content check.
- Remote shells run marsh and can compose split/fanout and registered commands.
- Nested placement belongs to one job tree, including laptop/cloud/laptop.
- No execution replay, silent fallback, or implicit cloud spillover.

## 2. Shell surface

```sh
find . -type f | cloud { process-files; } | sort

cloud { generate-assets; }
make test                         # sees returned edits before it starts

split {
  fix: cloud { codex exec 'Fix the parser'; make test; }
} | join --keep                   # returns edits to the fork, not the source

fanout {
  a: cloud { generate-a; }
  b: cloud { generate-b; }
} | collect                      # returns both deltas; overlapping paths race

cloud {
  fanout { tests: make test; lint: make lint; } | collect
}                                # both branches share one remote workspace
```

`process-files`, `generate-assets`, `generate-a`, and `generate-b` stand for
tools in the chosen cloud shell image, not bundled marsh commands. A GPU tool
is possible only if Docker Cloud exposes a supported GPU shape and an
appropriate image. **GPU support is not promised by this proposal.**

The block is function-like syntax, not an ordinary function call. Brush parses
it as one placement command; words, expansions, pipelines, lists, redirects,
and exit status inside the block are evaluated by the remote Brush shell.
There is no local `eval`, Bash implementation, or host-side re-quoting of argv.
Redirections outside the block belong to the caller:

```sh
cloud { cat input.txt; } > output.txt  # local output.txt
cloud { cat input.txt > output.txt; } # remote file returned in the delta
```

The block starts a fresh shell context, like a split shell branch: filtered
exported environment and cwd transfer, but aliases, functions, unexported variables,
traps, job tables, and shell options do not. Changes to cwd or environment do
not mutate the caller's shell. Document and differential-test this extension.
The daemon applies the existing exported-environment credential/control-name
filter on every crossing, including job-origin requests. It reconstructs HOME,
PATH, proxy/CA/MCP endpoints and capability identity for the destination from
trusted state, never from a guest's offered values. Project files, explicit
arguments, and stdin can still contain secrets; this is not content scanning.

Proposed CLI equivalents, also usable from bash/zsh:

```sh
marsh cloud -c 'generate-assets && make test'
marsh cloud -- program 'one argument' ''
marsh run --place cloud KIT ARGS...
marsh run --place local KIT ARGS...
```

The argv form preserves arguments as bytes and does not parse shell source.
Placement inherits when omitted; `local` means the originating laptop's
executor, not whichever machine happens to receive the request. Explicit
placement is constrained by the caller's allowed destinations. Keep the
current local-only `MARSH_PLACE` behavior until the new interface ships; do not
reinterpret existing configuration halfway through implementation.

Calling placement from a Kit job does not grant session-shell authority.
`-c` runs Brush within a job-grade, nonroot container with that job's rights;
it does not attach to the privileged session shell. Job-created splits retain
their argv-only branch restriction. A session-authorized cloud shell may use
shell branches. No extra `local {}` grammar is needed for the first release.

Execution profiles are explicit and pinned:

- A session-origin shell/argv placement uses the configured cloud shell image,
  its declared native SBX policy, and session limits. This is the tool-equipped
  marsh shell profile, not an arbitrary built-in agent selected by name.
- A job-origin shell/argv placement uses its parent's exact Kit workload image
  and native policy/credential profile, with the trusted marsh artifact as the
  requested shell entry. It is a shell placement node, not a registered Kit
  invocation overriding that Kit's entrypoint. Its limits, spawn set, and
  allowed places are the parent's or narrower. It cannot select the broader
  session-shell profile or add credentials/egress.
- Registered Kit calls retain their Kit-authoritative entrypoint, argv,
  credentials and policy, as today. A shell node does not reset consecutive
  same-Kit accounting: it carries the originating Kit identity through shell
  intermediaries. Shell crossings count toward depth/fan/total/concurrency.

Reproducing an inherited policy at another location is a preparation gate; if
stock cannot enforce it, refuse the crossing. Do not approximate deny rules.

## 3. Streams, status, and terminals

- stdin, stdout, and stderr are separate live byte streams with bounded
  queues/credit. No whole-input spool is introduced by placement. Boot and
  snapshot upload can backpressure the upstream producer.
- stdout contains only command output. Progress and actionable diagnostics go
  to stderr; receipts and workspace results use their existing separate paths.
- Preserve empty/non-UTF-8 argv, explicit EOF, exit status, and `PIPESTATUS`.
  `pipefail` remains the caller shell's rule. Existing split/fanout capture
  limits remain in force where those constructs already buffer streams.
- A closed downstream pipe cancels that stage's subtree and settles it without
  blocking the rest of the pipeline forever. Do not confuse local carrier exit
  with remote cancellation. Specify and test SIGPIPE/closed-output behavior
  against ordinary Brush pipelines before the syntax is enabled.
- The command is not complete until its writers are settled, the delta return
  has settled, and required cleanup has been verified or reported uncertain.
  Earlier stdout is not a write-back acknowledgement.
- A nonzero command exit still returns its confirmed changes. An ordinary
  nonzero exit is not a rollback. Preserve its status when transfer and cleanup
  succeed. Setup/transfer/apply failures use 125; unconfirmed runtime cleanup
  uses the existing `cleanup_uncertain` exit contract. Cancellation returns 130
  when settled; receipts retain execution, cancellation, transfer, and cleanup
  outcomes separately. Uncertain cleanup takes precedence over an ordinary exit.
- Dispatch must support noninteractive streams first. PTY allocation, exact
  terminal bytes, resize, and signal delivery are a separate acceptance gate
  required before interactive cloud shells ship.

## 4. Workspace transfer and automatic return

### Caller-bound views

A placement boundary records the **immediate caller's admitted workspace view**,
its identity, relative cwd, access mode, and return destination before effects.
It materializes that view remotely and uses the same guest-visible paths where
the platform permits them. Failure to preserve a required path is a preparation
error, not a silent cwd rewrite. An absolute path outside the view is not made
available because a program prints it on stdin.

| Caller | Snapshot source | Automatic return destination |
|---|---|---|
| Ordinary session | Admitted project | That project |
| Split branch | Branch fork | That same fork |
| Fanout branch | Caller's view | That shared view |
| Job at another placement | Parent's current materialized view | That parent view |

Source roots are pinned by identity, not reopened from guest-supplied paths.
A replaced root is an error, never authority to write into the replacement.

### Baseline and delta

For the first session-to-cloud admission, use split's filesystem snapshot rules:
uncommitted project bytes are inputs;
Git ignore rules filter initial contents (including tracked ignored files under
the current contract); sockets/devices/FIFOs and marsh control state are not
transferred. Ignored build inputs are consequently absent remotely. Ignore
rules are not a security scanner: admitted project files can contain secrets.
Further placement crossings within an already admitted materialized view copy
its current ordinary files, including ignored outputs generated there; they do
not reapply input ignore rules and lose an intermediate build artifact.
Protected/special entries remain excluded. An explicit nested `split` still
uses split's own snapshot filtering; placement does not change that contract.

Transfer Git metadata as a private, hook-free view sufficient for ordinary
status/diff and agents. Never return writes to the caller's original `.git`,
split control/admin files, snapshot base, or journal. Preserve the existing
split Git authority boundaries; do not copy its gitfile as an arbitrary link to
host paths. Private commit metadata is not automatically imported into the
source repository. A nested crossing takes Git state from its immediate
parent's current private view, including new objects, not from the laptop's
original Git directory or stale alternates. Mutable private index/objects/refs
may return only where that parent view already grants them writable; protected
split admin/config and original Git state remain immutable to return.
Generated regular files are included in the result even
when their names match ignore rules: ignore rules filter inputs, not outputs.

The baseline is immutable. After every writer in the placement subtree is
confirmed stopped, compare the final view to that baseline. Return only
creations, modifications (including executable-bit changes), type changes, and
deletions. Renames may be represented as delete/create. Include empty
directories where representable. Preserve symlinks as entries, never follow
them during transfer; do not preserve hard-link authority across roots.

Apply changed entries without comparing destination content to the baseline.
An overlapping caller edit can be overwritten or deleted. Unchanged remote
files do not participate in return, so unrelated caller edits survive. There
is no atomic multi-file commit and no automatic rollback after partial apply.
Use per-file staged replacement where possible. Import journal/progress and
retained payload identify partial results. Never reapply a delta automatically
after daemon restart or an ambiguous apply acknowledgement.

Directory structure still needs safe mechanics: remove directories only when
empty, and fail a type replacement that would recursively erase concurrent
children not present in the delta. That is a structural/authority failure, not
a content conflict policy. Serialize marsh's own imports per destination root;
arbitrary local processes may still race. No atomicity with external editors
is promised.

All entries are validated before apply: root-relative paths, no `..`, duplicate
or contradictory operations, bounds, source access, and protected-path checks.
A canonical manifest has one final operation per path. Directory declarations
may contain child operations; a file/symlink creation or recursive deletion
cannot also name descendants. Create directories parent-first, replace leaves,
and delete leaves/directories child-first; remove directories only when empty.

Validate names against the destination filesystem, not only Linux byte strings.
Case-folding/Unicode-equivalent names must map injectively, including baseline,
result, protected names, and existing destination entries. A new `readme` may
not alias an unchanged `README`; `.GIT` must not bypass `.git` protection on a
case-insensitive volume. Unsupported or ambiguous mappings fail before import
and retain the payload, without silent renaming. Recheck structural aliases
at each descriptor-bound operation; external namespace races can cause partial
failure, never permission to follow a different path. This is name/authority
validation, not content conflict prevention for the same admitted path.

The receiver uses no-follow descriptor-relative operations and refuses
symlink ancestors. A symlink leaf may be replaced as an entry but never followed
as a destination. A read-only grant cannot become writable by round-tripping.
Archive extraction into the live project is forbidden. Names, sizes, metadata,
and hashes must match the staged manifest. Existing host identity assumptions
remain explicit; TLC does not validate filesystem syscalls or archive parsers.

### Concurrency and nesting

Separate `cloud` calls in fanout take independent snapshots at their individual
entry times, not one common fanout snapshot. Returned deltas race. Put fanout
inside one cloud block for a shared live remote workspace; use split for common
baseline isolation. Shell sequencing waits for return before the next command.

An inherited-location Kit child uses the parent's live admitted view, as local
Kit jobs do today; it is not another hidden snapshot. An explicit location
crossing snapshots the parent's view and returns there. Cloud -> laptop cannot
reopen the user's checkout or guest home. A later laptop -> cloud crossing is
another child, not a new root or budget. A split created remotely snapshots its
creator's remote view, not a stale Mac mirror. The authoritative coordinator
records logical view identities independently of their materialized paths.

### Home and extra mounts

Do not upload the existing Mac-backed guest home, explicit login files, SBX
credentials, host control state, or local MCP executables implicitly. Cloud
work uses a cloud-side home scoped to its workspace lineage; siblings within
that view share it, isolated split views must not gain access to one another's
project. Remote home changes do not import into the laptop home. First release
may use disposable cloud homes; durable cloud homes require a separate design
and must not use concurrently mounted last-writer-wins volumes as shared state.

Cloud placement must refuse unsupported required extra mounts/MCP declarations
before execution and explain them, not silently drop them or upload their
sources. Native cloud declarations can be admitted later with explicit source
and credential policy. Returning to local keeps the cloud caller's narrowed
home/view, not the laptop session's larger configured environment.

| Resource | First laptop -> cloud | Nested crossing in either direction | Return/disposal |
|---|---|---|---|
| Project | Filtered root snapshot | Current parent materialization, including generated ignored files | Delta into immediate admitted caller view |
| Git | Private hook-free state from admitted source | Current parent's private state/objects; no original-host alternates | Writable private state only; never original `.git` or protected split admin |
| Home | Fresh cloud-side home, no laptop-home upload | Snapshot of current lineage home into a private materialization | Child home delta returns to parent lineage home, never laptop session home; disposable after lineage |
| Extra grants/MCP | Refuse unsupported required declarations before effects | Only parent-recorded explicitly cloud-admitted grants; RO stays RO | No child-Kit additions; no implicit host executables/credentials |

A cloud-created Git object and generated `dist/tool` therefore reach an
explicitly local child from its cloud parent. That child still receives neither
the laptop's real `.git` paths nor its guest-home login files. Home contents
can include explicit cloud login state; it is sensitive lineage data, not
proxy-held credentials copied out of SBX. Recovery payloads containing home data
must have the same owner-only protection and must not appear in public receipts.

## 5. One coordinator, one lineage and admission

The originating daemon remains authoritative for the entire tree. Remote
workers and marsh shells relay scoped requests; they do not start independent
daemons with fresh admission budgets or unrestricted host credentials.

Placement blocks are visible execution nodes with their own job/attempt IDs;
registered commands and split branches keep theirs. Session, job, attempt,
workspace transfer, worker, sandbox ID, and nested container IDs stay distinct.
Receipts/tree views expose placement and parentage, including return direction.
Transfer/control helpers are phases, not fake user jobs.

Preserve daemon-wide concurrency, depth/fan/total/deadline limits and spawn-set
narrowing across locations. Shell placement nodes must count toward bounded
admission; there is no uncounted recursive-shell path. Define shell-node versus
Kit-node same-Kit accounting explicitly in the Process model extension. Count
actual worker identities by `(placement, execution profile, Kit identity)`, not
Kit identity alone. Cloud has an additional explicit sandbox/shape ceiling and
TTL, never unlimited provisioning. Suspended parents do not create unbounded
reservations; a full pool produces a visible refusal rather than deadlock.

Placement permission is separate from Kit spawn permission: a cloud-authorized
child is not automatically authorized to use the laptop executor. Initially
the trusted session may allow both; every child can only narrow that set.
Permission to execute locally grants a nonroot Kit with the inherited view,
never Mac process execution, a privileged session shell, or the dev broker.

Cancel, parent exit, disconnect, and deadline revoke the subtree and signal all
placements. INT then the existing grace period then KILL applies to the whole
tree, not one grace period per hop. Slots remain held until execution cleanup
is verified. Result transfer has its own bounded settlement and held workspace
lease; do not report the placement complete while return is pending.

### Bottom-up settlement after exit or cancellation

Execution/spawn authority and coordinator-owned settlement authority are
separate. A revoked job capability can never spawn or request an import. The
coordinator may finish an already-journaled return under a destination-bound,
generation-bound lease; this is not a surviving guest capability.

1. Fence new launches, revoke execution capabilities, and signal the subtree.
2. Confirm writers stopped, from leaves upward. A child's pending import is a
   writer/lease on its immediate destination even after its process has exited.
3. Capture/stage the child's bounded result, then finish or abandon its return.
   Parent capture and destination teardown wait for this outcome. On timeout or
   unavailable destination, fence further apply, record any partial writes, and
   retain the confirmed payload outside the disappearing workspace.
4. Settle each parent after its child returns. Incomplete child settlement makes
   the ancestor result incomplete too; it may retain a stable recovery capture,
   but cannot report a successful full return or silently redirect child edits
   to an ancestor checkout. Unconfirmed live writers still prohibit capture.
5. Release destination leases only when every import has completed or been
   explicitly fenced/abandoned with its retained result recorded. Then remove
   owned sandboxes and verify absence. Restart invalidates all unsettled leases;
   it never resumes an ambiguous import automatically.

Keep current cancelled-split cleanup: confirmed cancelled forks may be removed.
Before removing one, retain any confirmed cloud payload and partial-apply
manifest outside the fork in owner-only recovery storage. Such a payload is
recoverable even though the fork is gone; it is never applied to the original
project as a substitute. Uncertain writers keep the existing split retention
rule. The public recovery API/storage limits must be specified before the
workspace slice ships; cancellation is not permission to lose the only result.

## 6. Remote lifecycle and failure boundaries

Use only public stock `sbx --cloud` operations under the current Docker sign-in;
account selection/switching is ordinary stock CLI behavior, not a separate
marsh feature. Do not parse credential stores or add account-marker resources.
Intent is persisted before create; ownership binds a random name and immutable
sandbox ID. Local and cloud inventories cannot collide. Cleanup and later exec
target recorded IDs; names alone are not cloud authority. Delayed create
visibility is not proof of absence: retain an unresolved intent until stock
provides a conclusive outcome or operator reconciliation. Do not immediately
drop cloud intents using the local inventory assumption.

The proposed executor owns disposable cloud resources per crossing/workspace.
Distinct native policy/credential profiles use distinct sandboxes, each with
fresh nonroot Kit containers and trusted worker control outside jobs. One
authoritative remote workspace may be mounted by trusted filesystem clients in
those sandboxes, with narrow filesystem RPCs relayed over retained stock exec
streams. Basic sharing under opposite allowlists was proven by the live probe;
the production client/coherence/locking design remains a gate. Do not infer
that the tested SSHFS prototype is the final implementation.

Reuse within that workspace is allowed; cross-call warm pooling is not required
for correctness. Never run incompatible Kits inside one broader-policy sandbox
as a substitute. If shared workspace and per-Kit boundaries cannot both be
provided, stop at that blocker rather than weaken policy or fake containers.

Each call follows these logical phases (bounded failures are possible in each):

`admit -> snapshot -> provision -> upload -> execute -> settle writers ->
capture delta -> stage return -> apply -> verify cleanup -> complete`.

Independent preparation may overlap, but execution cannot precede complete
validated input and journaled authority. Captured payload is retained outside
the disposable sandbox before that sandbox is deleted. Cleanup can overlap
apply once capture is stable. A `rm` acknowledgement is not verified absence.

| Failure | Required result |
|---|---|
| Subscription/login/quota/unsupported shape or image | Actionable preparation failure; no local fallback |
| Upload interrupted | No command start on incomplete input; clean only owned resources |
| Normal nonzero exit | Capture and return confirmed edits, preserve command status |
| Cancellation with confirmed stop | Capture/return confirmed edits within settlement bounds; report cancellation |
| Transport loss with possible writers | Quarantine; uncertain receipt; no capture/apply from a live or unverified view |
| Delta download/integrity failure | No live-tree extraction; retained result or named unresolved resource |
| Apply failure or root replacement | Report partial/not-applied state and retained payload; no execution retry |
| Daemon restart/ambiguous apply ack | Revoke capabilities, retain uncertain state; no execution or write-back replay |
| Cloud TTL expiry | Handle as runtime loss unless termination/cleanup independently verified |
| Delete accepted but still present | Keep cleanup pending/uncertain and resource identity visible |

Never infer remote termination from an exited `sbx` carrier. Cleanup recovery
may inspect/delete an owned sandbox and release held slots after verification;
it may not rerun workloads or automatically publish an uncertain result.
Specify finite bootstrap, transfer, apply, and deletion timeouts before shipping;
timeouts are errors with recovery evidence, not evidence of successful cleanup.

## 7. Platform feasibility gates

### Kit-version boundary

The 2026-10-16 [live probes](../plan/cloud-feasibility.md) verified acceptance of
**Kit v2** cloud sandbox descriptors, following the user's compatibility note.
They did not test whether cloud accepts v3. Local marsh uses native Kit v3.
Public CLI help accepting a Kit reference is not
evidence that the cloud service accepts its descriptor version or preserves
the same policy, credential, image, and bootstrap semantics.

Treat local-v3 and cloud-v2 preparation as distinct adapter paths. First verify
the cloud-supported Kit version and artifact format, then prove a pinned
cloud-compatible shell/fixture boots. Record the supported descriptor version,
workload image identity, architecture, entrypoint/argv, user, resources, network
policy, and credential behavior for each destination. Do not send local v3
artifacts to cloud assuming compatibility, automatically downgrade descriptors,
or silently discard v3 fields that v2 cannot represent. Any separately published
cloud binding must be explicit and immutable, with equivalent required behavior
verified; unrepresentable requirements cause a preparation refusal.

The shell/workspace/job contract remains version-independent. The placement
model does not establish v2/v3 compatibility. If cloud v2 cannot enforce that
contract, return to the architecture decision rather than weakening it.

Initial discovery used installed stock SBX CLI help, version
`v0.48.0-683-g7167374c8`. Subsequent approved live probes verified v2 boot,
streaming, diagnostic worker containers, opposite native allowlists, basic
stdio-tunneled live sharing, and owned resource cleanup. See the
[evidence and limitations](../plan/cloud-feasibility.md), including the worker
proxy mismatch and the prototype's unsupported shared mmap. The full platform
and product acceptance gates are not closed.

The user approved billable probes on 2026-10-16. Continue to record execution
context, resource ceilings, TTL, resource IDs, and verified cleanup. Before
implementation expands, prove the remaining parts of these gates:

1. The cloud-supported Kit version (v2 acceptance now verified) and artifact format
   are established; a pinned compatible shell/fixture boots at a supported
   architecture with marsh artifacts, independently of the local v3 path.
2. Bootstrap can obtain the trusted worker authority needed for containment and
   fresh nonroot containers. Cloud exec rejects `--user`; using an image's
   supported sudo/bootstrap was observed for the tested pinned image, not
   established for arbitrary images. Qualify each supported profile without
   patching SBX.
3. **Two incompatible native Kit policies** can use the intended shared live
   workspace while both retain fresh nonroot containers, distinct SBX-enforced
   network/credential policy, and no Docker/SBX sockets/raw proxy-held keys in
   jobs. A one-Kit probe cannot establish this. Record whether this needs
   multiple sandboxes plus a supported shared workspace transport. No broader
   common policy or snapshot-only substitute for promised same-place sharing.
4. Byte streams, EOF, signals, deadlines, and backpressure work over retained
   transports; a live writer cannot be mistaken for verified cleanup.
5. Safe upload/download supports the manifest, path, metadata, and byte limits.
6. Immutable ID inventory supports intent reconciliation and observed deletion.

V2 preparation also bakes temporary sandboxes and saved templates. Ownership
must cover those side effects and crash/ambiguous-output reconciliation; never
delete a reused template as if marsh created it. Basic shared-file probes do
not establish mmap, distributed locking, metadata/flush, tool compatibility, or
disconnect behavior. A dead filesystem transport must not expose an empty
underlying mount directory as a valid workspace or a delta that deletes the
caller's files. Retain mount/source identity and generation checks through
capture and apply.

If these fail, revise the architecture with the user. Do not substitute direct
agent execution for fresh Kit containers or quietly remove nested placement.

## 8. Architecture and implementation units

Implementation progress: the first [trusted cloud worker profile](../plan/cloud-worker-profile.md)
is implemented and qualified with fresh binaries in real cloud, preserving the
native proxy and CA. The [owned cloud lifecycle adapter](../plan/cloud-lifecycle.md)
is also implemented and E2E-qualified, with a mapped cloud ownership model.
[Session-origin dispatch](../plan/cloud-dispatch.md) (units 1 and 2, and the
split-fork/fanout destinations of unit 3) is wired through the daemon and the
`cloud { }` Brush block, using the lifecycle adapter with the pinned shell
image and the static `marsh-local` helper. It runs the shell profile directly
in the sandbox as an unprivileged workload user (no sudo or Docker), so every
writer can be killed by uid before capture, with root-owned control state; the cloud worker profile (fresh nonroot Kit containers in
cloud) is not yet used, so registered Kits, remote workspace services, nested
placement, and job-origin callers remain open, as do native Kit preparation
and the interactive slice.

Keep shell syntax in a small Brush patch with a CLI equivalent. Keep stock
platform operations in `marsh-sbx`; add a cloud adapter/module rather than
threading a boolean through local name/mount operations. Extend daemon contracts
with placement nodes, logical workspace references, and return outcomes. The
daemon coordinates transfer; neither the guest nor manifest names host paths.

`marsh-backend` selects the executor and binds authority; shared worker/runtime
contracts preserve fresh-container supervision and capability relay. Move the
existing split filesystem operations behind a location-aware workspace service:
Mac descriptor/clone operations locally; a trusted remote workspace agent for
remote snapshots/forks/capture. Do not copy remote workspace state back to the
Mac on every nested split to make the existing host-only code appear to work.
The remote service gets one bounded view, never the host daemon's master token.

Retain results under daemon-owned, identity-pinned storage with a versioned
manifest and structural receipt. The exact host storage path and public inspect/
discard/export commands are implementation decisions to settle before unit 2.
Result bytes are not receipt-journal content. Report bounded phase durations
for actual observations, including transfer/apply; do not invent remote timing
or mix them into existing local phase fields without a versioned schema change.

1. **Transport slice:** credential-free fixture pipeline, no file mutation;
   ID-bound provision/cleanup, bounded streams and truthful receipt.
2. **Workspace slice:** pinned source, immutable baseline, delta return into the
   caller, failure/partial-result inspection, no-change and deletion paths.
3. **Composition slice:** split fork return, both fanout placements, remote
   workspace service, real registered Kit under native policy, one job tree.
4. **Nesting slice:** cloud split/fanout, cloud -> laptop Kit -> cloud, inherited
   views, placement/spawn limits, cancellation and loss across hops.
5. **Interactive slice:** PTY and cloud-entry shell using the same contracts.
   Proposed entry spelling is `marsh --cloud`; settle shell exit/return and
   disconnect UX against the working dispatch path before implementing it.

No unit is a product release until its promised path passes real-caller tests.
Local behavior remains the default and its existing gate must keep passing.

## 9. TLA+ obligations and current coverage

[`Placement.tla`](../model/Placement.tla) is an executable bounded **proposal**
model for crossing identity, immediate-parent snapshot/return, inherited view
rights, one shared admission pool, last-writer-wins per-path delta import,
stable capture, no replay, and delete acknowledgement versus observed absence.
It allows caller edits and sibling imports; it deliberately does not assert
content conflict prevention or atomic multi-file write-back.

Its invariants, negative controls, assumptions, and code-location/gap table are
in the [model README](../model/README.md#proposed-placement-model). A pass is
evidence about the bounded specification, **not about unimplemented Rust**.

Before implementing state changes, extend the existing modules and their
negative controls/code-location table together:

| Module | Required extension |
|---|---|
| `Process` | Shell and Kit node kinds, inherited/explicit placement, allowed-place narrowing, shared caps and `(place, profile, Kit)` worker counting; cancel across hops |
| `Workspace` | Logical views backed by local/remote services, remote split source/capture, placement return to owning fork; pending returns are writers for capture/release |
| `Worker` | Transfer/return phases distinct from execution, generation-bound delayed frames, TTL/loss, no capture until writers settle |
| `Ownership` | Current stock sign-in + immutable cloud IDs, eventual create visibility, unresolved intents, v2 bake/template side effects and reused-artifact authority, delete requested versus absence observed |
| `Grants` / `Sbxenv` | No implicit local MCP/home/extra-source export; remote capability scope; reject unsupported required declarations |

These are extension gates, not claims that the modules have already been
composed into a whole-system proof. Model stream credits, finite transfer
budgets, and fairness assumptions before making liveness claims. Filename
validation, symlink safety, exact bytes, and cloud provider behavior require
code/real-caller evidence beyond TLC.

## 10. Observable acceptance

Keep one focused cloud family in the existing acceptance runner. No parallel
test framework, billed model calls, or implementation-mirroring mock suite.
VM-free model checks stay in normal CI; live cloud checks are opt-in, require
explicit subscription/consent, and have isolated ownership, quotas, TTL, and
durable evidence. Fixture jobs need no model-provider credentials.

| ID | Real-caller observation |
|---|---|
| C01 | Subscription/login/quota refusal is actionable; no local execution or foreign-resource change |
| C02 | `producer | cloud { fixture; } | consumer`: exact streams/argv/EOF/status, bounded backpressure, output before input EOF, `pipefail`, downstream close |
| C03 | Remote create/modify/delete/executable-bit changes return before the next local command; ordinary nonzero exit returns edits |
| C04 | Local edit to an unchanged remote path survives; overlap is last applied change wins, not an invented conflict failure |
| C05 | One-branch split receives cloud edits only in its fork; original files/Git unchanged; join emits an applicable patch |
| C06 | Fanout outside cloud uses independent snapshots/shared destination; fanout inside cloud sees one live view; disjoint and overlapping returns observed |
| C07 | Remote shell split snapshots current remote edits, keeps siblings private, joins there, and returns only the owning view |
| C08 | Laptop -> cloud -> laptop Kit -> cloud is one tree; inherited cwd/view, no host-home/original-checkout widening, shared caps/deadlines |
| C09 | Ctrl-C/parent exit cancels all hops; confirmed partial edits return; uncertain writers never publish; slots held until verified cleanup |
| C10 | Drop transport during execute/download/apply; restart between phases: no execution/write-back replay, truthful partial/uncertain result and retained IDs |
| C11 | Malicious paths/archives, symlink ancestors, changed root, read-only view, protected Git/control paths and resource-exhaustion manifests cannot escape authority |
| C12 | Delete acknowledgement while sandbox remains visible cannot report verified cleanup or release its held resource slot |
| C13 | No implicit guest-home keys/login/MCP/extra-mount export; native Kit policy and no runtime sockets/raw proxy-held keys remain true |
| C14 | Interactive entry: PTY bytes/resize/interrupt, nested composition, final write-back, loss and cleanup; no release claim before this gate |

C09 must cover a stopped child with a pending return, a cancelled/removed
destination, return timeout, and recovery after cancelled split deletion. C11
must run case-insensitive APFS and Unicode-alias cases as well as Linux cases.
C01 must reject unsupported Kit versions/fields before execution. C13 must
verify cloud-v2 policy/credential behavior rather than assume local-v3 parity,
and send forged environment offers from both root and job callers. C08
must include a generated ignored artifact and new private Git object crossing
back to the laptop, plus an interposed-shell attempt to reset policy/chain caps.

Update the guides listed in [the draft cloud guide](cloud-placement-guide.md)
as each slice ships. Current manuals must continue to say local-only until the
corresponding behavior is real and tested.

## 11. Adversarial decision record

Date: 2026-10-15. Status: product direction agreed in conversation; proposed
contract/model reviewed; architecture **pending platform evidence**, not
implementation authorization.
The decision is to add explicit shell-native cloud placement with automatic
delta return, rather than a cloud-only split feature or a live-sync filesystem.

### Alternatives and position

- **Do nothing:** no new cost or regression risk, but no remote command center.
  Reject for the requested product; retain as the fallback if safety gates fail.
- **Smaller: cloud argv jobs returning patches only:** fewer filesystem/state
  changes and an earlier proof slice; reject as the final experience because
  ordinary pipelines and sequential local commands would need special handling.
- **Different: keep a whole interactive environment remotely and SSH into it:**
  coherent remote files and less return machinery, but not local command
  composition. Keep as a later entry mode, not a replacement for dispatch.
- **Live two-way sync/shared volume:** lower apparent boundary friction but
  distributed write visibility, mount authority, and concurrent-volume loss.
  Reject. Stock cloud volumes explicitly document snapshot/last-exit semantics.
- **Wrap stock `sbx` in a script:** use stock for platform effects, but a script
  alone does not preserve lineage/capability/cancellation and workspace return.

These alternatives are relative effort/risk judgments, not delivery estimates.

### Assumptions, pre-mortem, and kill criteria

1. **Verified in current code:** local split/fanout/process authority exists;
   **unverified remotely:** same enforcement can be provided by public SBX.
   Change the architecture if a fixture cannot prove containment and policy.
2. **Agreed product risk:** automatic overlapping return can destroy edits.
   Show this warning before the first tutorial mutation; teach one-branch split.
   Do not silently introduce content conflict checking to undo this decision.
3. **Unverified:** transfer cost/latency is acceptable for intended projects.
   Measure a credential-free representative project after the transport slice;
   bound transfers and reject over-budget inputs before provisioning where
   possible. No speedup claim until measured.
4. **Failure scenario:** delayed create/delete leaves billed orphan resources.
   Persist ID-bound ownership/intents, finite TTL, explicit ceilings and inspectable
   uncertain IDs. Stop shipping if observed absence cannot be established.
5. **Failure scenario:** nested placement reopens laptop paths and exports
   secrets. Separate logical views from paths; test against a narrowed split
   and a cloud-origin job, not only a trusted root shell.
6. **Failure scenario:** a successful command destroys data during unsafe import
   or is reapplied after a crash. Require bounded staged manifests, descriptor
   confinement, partial-result evidence, and no automatic import replay.

If the feature succeeds, transfers and nested fanout can amplify spending and
held-capacity refusals. Expose placement, phase, transferred bytes, shape, TTL,
and owned IDs; keep explicit quotas. Remote shells have finite trust domains,
not an excuse to expose the whole Docker account. Subscription and model costs
must be distinguished in installation, guides, and troubleshooting.

### Decisions of 2026-10-17

With the live-sharing gate (section 7, item 3) still open, the user chose:

- **Agent Kits inside cloud blocks** use explicit cloud bindings (stock cloud
  agents or published cloud-v2 Kits) in their own sandboxes, with the block's
  current files crossing by snapshot and the Kit's changes returning as a
  delta when it finishes. Siblings do not see each other's writes live. This
  replaces live sharing for this release; it is not a v3-to-v2 translation.
- **`split` inside a cloud block** runs in the sandbox itself (forks,
  branches, capture, join), as composition within the admitted cloud node.
  Host split records and lineage show only the cloud job.
- **Job-origin cloud placement stays refused** until cloud bindings of the
  parent Kit exist; it is never widened to the session shell profile.

### Decisions of 2026-10-18

- Docker Cloud runs our v3 Kits by digest, so a registered command's cloud
  form is its own pinned Kit (image, entrypoint, policy, credentials), not a
  stock agent. This replaces the stock-agent bindings and their tags.
- Job-origin placement is implemented on that basis: the parent's Kit, the
  parent's view, the parent's limits (section 4's job-origin bullet).
- Cloud -> laptop crossings and `marsh run --place` are v2 work.

### Review disposition

The independent adversarial pass returned REVISE: destination name aliases and
nested cancellation/return ordering were critical. This revision adds
destination-equivalence validation, coordinator-owned bottom-up return leases,
cancelled-split payload retention, pinned shell policy/identity, per-crossing
environment filtering, a home/Git/grant matrix, and an explicit two-policy
platform probe. These are required gates, not claims of implementation. Review
uses the available default model; no cross-vendor review is claimed.

The final review passed after also strengthening parent capture to require all
descendant returns settled (including stopped, not-yet-captured children) and
making the replay negative control weaken the real execution-start guards.
Validation: all 14 placement configs gave their expected verdicts; the existing
quick model gate gave 84/84 expected verdicts, followed by a fresh 14/14 run
after the final invariant tightening. `make site-check` and `git diff --check`
passed. That review preceded the subsequently approved
[live platform probes](../plan/cloud-feasibility.md). No production Rust or
Brush changes were made during those probes. A later
[worker-profile slice](../plan/cloud-worker-profile.md) adds runtime plumbing,
not full cloud dispatch. Neither review nor model results close the
remaining platform/product gates.
