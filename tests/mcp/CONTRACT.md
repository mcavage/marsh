# Host MCP acceptance contract

This contract qualifies the bounded host-stdio MCP server described in
[`docs/design/mcp.md`](../../docs/design/mcp.md). It is separate from the local shell release
gate in [`../acceptance/CONTRACT.md`](../acceptance/CONTRACT.md): MCP tests do
not weaken or replace stock-SBX product acceptance.

## Scope and harness

The harness starts the public `marsh-mcp` executable as a stdio peer with an
isolated temporary canonical workspace and an isolated owner-only development
scope root. It supplies separately installed, owner-trusted `marsh` and stock
`sbx` binaries outside that workspace and root. It speaks the public MCP
protocol only. It may start public `marsh` and stock `sbx` binaries to
independently observe a scope it owns, but it must not read daemon sockets,
token files, internal databases, process tables, Docker state, VM state, or
implementation types.

The harness owns only the isolated MCP scopes it created. It must never stop,
reset, enumerate, or modify a user or other marsh scope. It records
protocol transcripts with sensitive request fields redacted, byte-bounded tool
outputs, product status snapshots, and fixed qualification evidence references.

The candidate passes only when every successful startup emits a valid MCP
initialize handshake and the cases below pass against the assembled public
binary. Unit tests can cover parsing and internal failure injection; they do
not replace this boundary test.

## Startup and configuration cases

1. A canonical workspace, an explicit empty `--home` outside it, and separately
   installed `marsh` and `sbx` executables outside both paths start a usable
   legacy single-scope server. The server creates the home owner-only and
   rejects `scope_start`; a group/world-readable or foreign-owned selected
   home fails before any daemon, VM, or job starts.
2. `serve` with neither `--home` nor `--scope-root` is legacy single-scope
   mode on the same selected home the `marsh` CLI uses (`MARSH_HOME`, else
   `$HOME/.marsh`), so the command in `docs/mcp.md` works as written. It
   writes nothing under the protected host product root. Broker modes and
   `marsh mcp install sbx` use a generated owner-only scope root
   `<state-root>-mcp-scopes/<workspace-hash>` beside (never beneath) the
   protected host state root: by default
   `$HOME/Library/Application Support/marsh-mcp-scopes/<workspace-hash>`, or
   `$MARSH_CONTROL_HOME-mcp-scopes/<workspace-hash>` when `MARSH_CONTROL_HOME`
   is set. The hash is derived from the canonical workspace and the reserved
   default home is its `default/` child. The scope root must remain outside
   the guest-writable `$HOME/.marsh` tree. Distinct canonical workspaces
   receive distinct roots.
3. `--scope-root` selects one fixed owner-only development root outside the
   workspace. `--scope-root` and `--home` are mutually exclusive. Unsafe,
   replaced, or non-directory roots and scope homes fail before product work.
   One MCP owner holds an exclusive no-follow lease for that root. Direct stdio
   mode rejects another owner. SBX gateway mode uses one resident broker plus
   authenticated owner-only Unix-socket proxies, so multiple sandboxes share
   that one owner and registry without sharing stdio process identity. The persistent registry is owner-only,
   published atomically outside guest-writable scope homes, and never trusts a
   persisted path. A malformed record with a safe canonical ID becomes Failed;
   an invalid-key record remains preserved and is reported by `doctor` without
   exposing it as a caller-selectable scope. Legacy `--home` lifecycle state is
   process-local and has no managed-root lease or registry persistence.
   Before any managed scope home is used, the root records the canonical path
   and filesystem identity of its first workspace. Reusing that root with a
   different or replaced workspace fails closed. A populated unbound root is
   rejected rather than automatically migrated. Missing or identity-changed
   persisted homes become Failed and are not recreated or recaptured.
   Such identity-invalid Failed records remain charged against capacity and
   cannot reset or remove automatically. The error gives the fail-closed
   operator path: restore the exact original directory entry, or stop MCP,
   independently prove/remove the exact runtime with stock SBX, archive the old
   control root, and select a new empty root. A replacement path is untouched.
4. A missing, non-directory, or non-canonicalizable workspace fails before the
   server reports tools. The scope identity remains fixed through a long-lived
   server even if the client later presents a different path.
5. A selected home or scope root inside the workspace, or an `marsh` or `sbx`
   executable inside the workspace, scope root, or a scope home, fails before
   the server reports tools. Replacing, relinking, changing the
   safe mode/owner of, or otherwise changing the pinned identity of either
   executable after startup fails closed before the next host invocation. A
   hostile inherited `MARSH_SBX` does not alter the pinned `--sbx` path or the
   fixed child environment. The server resolves its effective UID through the
   operating-system passwd database once at startup, validates the account
   name, and explicitly sets both `USER` and `LOGNAME` for every cleared child
   environment rather than inheriting either variable. `marsh` and `marshd`
   require trusted parent
   directories; the external Homebrew-managed SBX file keeps strict metadata
   and content pinning without rejecting Homebrew's group-writable Caskroom
   ancestor.
6. Tool discovery exposes exactly `doctor`, `status`, `results_list`,
   `result_get`, `shell_run`, `scope_start`, `scope_list`, `scope_run`,
   `scope_results_list`, `scope_result_get`, `scope_prewarm`,
   `scope_workers_reset`, `scope_status`, `scope_reset`, `scope_stop`,
   `scope_remove`, `prewarm`, `workers_reset`, `qualify`,
   `operation_get`, `operation_output`, and `operation_cancel`. Every
   schema rejects unknown properties, duplicate fields, wrong JSON types,
   overlong strings, invalid UTF-8/control data where text is required, and
   out-of-range timeout/page/output values. Without
   `--allow-full-sbx-control`, `qualify` returns a clear disabled failure;
   with that flag it accepts its fixed gates. No other tool changes.
7. Repeated initialize requests, malformed frames, unsupported protocol
   versions, and requests before initialization fail cleanly without starting
   product work or corrupting the session.

## Product-control cases

8. `status`, `results_list`, `result_get`, `shell_run`, `prewarm`, and
   `workers_reset` target only the reserved `default` scope. They return the
   corresponding versioned public marsh data or operation for that scope. An
   unknown, malformed, or
   cross-scope job/result identifier fails without revealing whether it exists
   elsewhere.
9. `shell_run` executes a normal command in the fixed workspace's
   project shell VM. It proves its natural working directory, byte-faithful
   bounded stdout/stderr, nonzero exit, and a registered Kit command
   through the ordinary fresh-container path. It cannot change working
   directory or add a host environment through tool arguments.
10. `prewarm` accepts only `all` or registered names from this
   scope. It reports warm readiness; a following compatible registered command
   reuses the Kit VM and still receives a distinct fresh job container.
11. `workers_reset` affects only selected workers from this scope, refuses
   while one of those workers has active work, and neither resets the project
    shell nor affects an independently created marsh scope.
    `scope_prewarm` and `scope_workers_reset` provide the same bounded behavior
    for one exact Ready generated scope or `default`; neither can select a path,
    runtime identity, peer scope, or unregistered Kit.
12. `scope_start` accepts no properties and returns an opaque lowercase UUID
    plus a boot operation. Two successful starts create distinct owner-only
    homes beneath the fixed root, distinct daemons and VM pools, and the same
    canonical workspace mount. Neither call accepts a path, label,
    environment, executable, or runtime identity. Scope operations and their
    outputs carry the selected scope ID.
13. `scope_status` inspects `default` or a generated UUID. Unknown and malformed
    IDs fail closed. `scope_run` reaches only a Ready scope. A Stopped scope is
    inspectable with `runtime_state: absent` and `runtime: null` and is not
    silently restarted by status, run, an MCP restart, or any default-scope
    alias. A Failed scope reports `runtime_state: unknown`, its last lifecycle
    operation, and a bounded diagnostic without probing or starting a daemon.
    `scope_results_list` and `scope_result_get` inspect durable structural
    receipts from one exact Ready or Stopped scope. Reading a Stopped scope may
    start its local daemon to read the owner-only journal but never boots its
    project-shell or Kit VMs. Before responding it sends bounded exact typed
    stop control with the full 600-second whole-scope lifecycle bound and proves
    the daemon stopped. A read failure before daemon start leaves the scope
    Stopped when exact stop proves absence; cleanup uncertainty marks the scope
    Failed rather than claiming it remains Stopped.
14. `scope_stop` refuses active MCP operations and active daemon shells/jobs,
    first performs a pinned boot or daemon validation and then sends the exact
    typed stop control,
    removes only the selected scope's project-shell and Kit VM runtime, stops
    its daemon, and preserves its home, results, and registry record. A peer
    scope remains usable. Endpoint absence alone cannot prove VM absence;
    uncertain or partial cleanup never claims success.
15. `scope_reset` has the same busy and ownership checks, removes the exact
    selected runtime, preserves home and results, then boots and validates a
    fresh project shell before marking the scope Ready. It may revive a Stopped
    scope. Ready and Failed recovery performs pinned boot, typed exact reset,
    then pinned boot in that order, and still rejects foreign or unverifiable exact VMs. It
    is not an alias for `workers_reset`, and a peer scope remains usable
    throughout.
16. Lifecycle requests serialize within one scope while independent scopes can
    progress concurrently under the global subprocess limit. At most 16
    generated scopes may be live and at most 128 generated records retained;
    Failed generated scopes count as live. A runtime failure with its exact home
    intact can reset, stop, and remove through the typed lifecycle. A missing,
    replaced, or malformed home stays fail-closed until the documented operator
    recovery; it is never silently recaptured or removed merely to free capacity;
    exhaustion returns a typed failure without creating an untracked home or
    runtime. `scope_list` rediscovers persisted IDs without disclosing paths or
    runtime identities. `scope_remove` queues bounded deletion of only a
    generated Stopped scope's exact private home and record; it rejects default,
    live, and Failed scopes. A crash-persisted Removing record is discoverable
    and retryable without recreating its home or runtime.
    Removal uses a bounded no-follow preflight and synchronous deletion; links
    are unlinked without following them, while excess entries/depth, special
    entries, ownership changes, and replacements fail without an untracked
    detached deletion task.
    Independent read-only requests in one Ready scope may overlap and remain
    observable while prewarm runs. Whole-scope reset, stop, and removal take
    exclusive scope access; `scope_status` reports their persisted transitional
    state without probing the runtime.
17. `qualify` is rejected when full SBX control is disabled. With explicit
    `--allow-full-sbx-control`, it accepts exactly `source`, `smoke`,
    `full`, and `perf`. Each uses the fixed repository recipe and its bounded
    output names the recipe's fixed evidence location beneath that canonical
    workspace. A passed profile has independently readable evidence; a failing
    profile reports the terminal failure without pretending success. A
    sentinel host side effect in a mutable qualification recipe proves that it
    cannot run without the opt-in; the equivalent opt-in test is isolated and
    records that host-user execution was deliberately authorized.

## Authority and containment cases

18. No tool accepts or reaches an arbitrary host executable, command line,
    Make target, environment variable, working directory, evidence directory,
    source tree, mount, SBX selector, Docker selector, daemon endpoint,
    VM ID, or container ID. Attempts to smuggle these via strings, arrays,
    object keys, Unicode confusables, symlinks, or JSON extensions fail before
    execution.
19. The default server may launch only its pinned public `marsh` executable
    under a fixed environment/PATH. `shell_run` routes through that fixed
    marsh product path and does not invoke `/bin/sh`, `bash`, `zsh`, `env`, a
    caller-selected Make target, or a caller-selected executable on the Mac.
    Only explicit `--allow-full-sbx-control` permits the fixed `/usr/bin/make`
    qualification recipe; that opt-in is documented as host-user code
    execution from a mutable checkout, not sandbox-scoped execution.
20. A job launched through MCP has no Docker/containerd/SBX/daemon socket,
    daemon master token, host-control MCP credential, raw provider credential,
    or direct host process authority. Its observed environment and network
    policy remain the native Kit/SBX contract, including the sandbox-local
    MCP Gateway URL and sentinel name only when stock SBX enables that gateway.
    Loaded Gateway servers are shared at the Kit VM boundary and can themselves
    grant host-side tools; this gate does not load `marsh-dev` into a Kit VM.
21. Operation state and bounded output are held only in process memory and
    disappear when that MCP server exits. The server adds no credential, daemon
    token, or host-private grant path to them; test commands must not print
    secret material. Durable product results remain separate structural
    receipts; this first delivery makes no durable MCP audit or evaluation-
    record claim.
22. Starting the host-control server from a Kit/project-shell container is
    unsupported: no host daemon socket is mounted, and no direct connection
    route or host bearer credential is documented. With `marsh-dev` **not**
    loaded into a Kit VM's stock Gateway, the test verifies that an unprivileged
    job cannot discover or use host MCP control authority. Loading that server
    into the stock Gateway intentionally changes this authority boundary and
    must not be claimed safe by this test.

## Concurrency, limits, and cancellation cases

23. Frames over 1 MiB fail before work starts. Independent read operations can
    complete concurrently, while one global subprocess cap applies to all
    operation types. Same-scope lifecycle conflicts receive an immediate typed
    busy/conflict outcome rather than waiting or racing worker state.
    Development scopes share the same live workspace mount, so concurrent file
    writes can race; callers use separate worktrees or coordinate mutations.
24. A client cancellation during `shell_run` or `scope_run` reaches the
    submitted job, produces an explicit cancelled or
    `cancellation_uncertain` typed result, reaps product work where possible,
    and leaves the server usable for a later command. The same holds for an
    enabled running qualification profile, without touching another scope.
25. Output, request-body, result page, duration, and concurrent-operation
    limits are enforced. A limit breach has a typed result, preserves the
    durable product receipt where one exists, and does not strand a worker or
    leave a partial successful qualification claim.
26. Client disconnect closes or cancels its outstanding requests according to
    the documented policy, never grants a later client access to that request's
    output, and leaves the server capable of a fresh initialize/session.
27. One SBX registration can be loaded into at least two sandboxes concurrently.
    Both proxies authenticate to one exact broker and share development scopes
    and the global operation bound. Neither client can read or cancel the
    other's operation UUID, and disconnecting one cancels only its operations;
    the peer remains usable. Broker loss closes clients without replay. A
    mismatched installed build or configuration never replaces a busy broker.

## Export-only published-command MCP mode (Cut A)

28. The server starts in export-only mode via `export-serve --home ABS --declaration PATH`
    (or `serve --home ABS --export PATH`). `--home` is required and identifies
    the exact user command-registry/daemon home; the generated per-workspace
    development-control MCP scope root and `--scope-root` are rejected.
    Explicitly configured other development roots must not be selected as
    export homes; this release does not identify them automatically. Export may not enable full SBX
    control. It loads and validates one explicitly declared tool declaration
    (`marsh.published_tool/v1`); the job guest HOME remains the natural account
    home path, backed by the explicit selected home.
29. Tool discovery (`tools/list`) exposes ONLY the single declared tool. All
    development-control tools (`doctor`, `status`, `results_list`, `result_get`,
    `shell_run`, `scope_*`, `prewarm`, `workers_reset`, `qualify`, `operation_*`)
    are absent from the tool list and rejected on invocation (MCP-05).
30. The declaration binds input schema properties to literal argv positions,
    named options, boolean flags, or bounded standard input. Fields cannot
    select an executable, Kit source, mount, credential, network policy, or
    resource ceiling.
31. Caller-supplied unknown properties are rejected before job creation (MCP-01).
32. Valid inputs become literal argv and stdin without shell interpolation or
    option injection (MCP-02). Any string value bound to a positional or named
    option that starts with `-` is rejected before job creation.
33. The command must be registered in the local marsh daemon scope; missing or
    unregistered commands fail closed before job creation (MCP-03).
34. The tool runs directly through the existing daemon Kit path (`ExecuteSpec`)
    into a fresh nonroot container in the designated Kit VM.
35. Nonzero exit, full output, truncation, timeout, cancellation, and uncertain
    cleanup are distinct caller-visible outcomes (MCP-04). Successful and
    nonzero completions return separate stdout/stderr, exit code, cleanup
    certainty, completeness, and a structural receipt selector. Base64 fields
    carry the authoritative output bytes; `stdout` and `stderr` are text views.
    Their `*_text_state` is `utf8`, `lossy`, `omitted`, or `truncated` so an
    invalid UTF-8 sequence or response-frame omission cannot silently appear
    as empty output. `output_complete` describes the authoritative byte
    streams, not whether the optional text views are present. A cancel or
    timeout signal alone never proves container cleanup; the exact Kit receipt
    determines `cleanup_certainty`.

## Published Brush pipeline MCP mode

36. A pipeline declaration runs its fixed source through the pinned `marsh`
    executable and Brush inside the project-shell VM. It does not invoke host
    Bash. The MCP caller may supply bounded stdin but cannot supply shell
    source, argv, environment, a working directory, or host executable.
    An attached shell can explicitly load a published tool into any named
    running same-user stock SBX sandbox, granting narrow host MCP control.
    Registered commands within the pipeline follow their ordinary Kit path;
    native stages do not acquire a Kit container merely by joining a pipeline.
37. A Brush pipeline is not a single Kit `ExecuteSpec` job. It reports its
    pipeline exit status and byte-stream completeness, with no invented Kit
    `receipt_selector` or `job_id`. Host process-group reaping can be verified
    separately, but it does not prove cleanup of constituent sandbox jobs;
    pipeline `cleanup_certainty` stays `uncertain` without such proof. A client
    cancellation or timeout never claims that signal dispatch alone stopped
    the sandbox work. Stock Gateway UAT checks that cancellation prevents a
    later command in a `sleep; printf` script, including a request cancelled
    before the guest session record is ready and one cancelled after a start
    marker. Black-box MCP protocol tests cover a pipeline's literal
    source, stdin, nonzero status, invalid UTF-8 bytes, response-frame text
    omission, timeout, and absence of a fabricated Kit receipt.
38. Replacing or removing a published declaration keeps the stock registration,
    but a previously loaded export server returns an empty `tools/list` and
    rejects `tools/call` until reloaded. It does not send
    `notifications/tools/list_changed`; a gateway cache may still display old
    discovery until the client reloads. The publish command tells the operator
    this explicitly on same-name replacement.
39. (Removed: MCP call reporting to the flight recorder.)
40. Guest publish/unpublish requests for the same project and name serialize
    through host registration. Direct host CLI invocations share the owner
    publication file lock with guest-requested host operations.
41. The export server launches its internal Brush pipeline shell as
    `marsh -c PIPELINE`; a real protocol pipeline test verifies the exact argv.

42. `mcp load NAME --kit KIT | --sandbox SANDBOX` loads an existing pipeline
    publication, not a replacement generation. The host form is
    `marsh mcp load NAME --sandbox SANDBOX`. The declaration bytes and exact stock registration stay unchanged;
    an original connected MCP caller still succeeds after a second load.
    Unknown/revoked, cross-protocol, changed project identity, unsafe file,
    stale generation, and registration collision cases fail before stock
    mutation. Publication itself requires the attached daemon route: direct host
    `marsh mcp publish` fails before creating publication directories or calling
    stock SBX. Host load/revoke of existing publications remain supported.
    Published names share one checked ASCII/dot-capable type and scope hash;
    Kit selectors use the separate command-registry name type.
    Validation precedes Kit preparation for both load and publish.
    One per-scope owner lock shared by ACP and MCP is acquired in the daemon
    before grant admission and covers validation, preparation, stock load,
    rollback and result checks. The host context binds its exact lock path.
    Stock operations execute under daemon ownership using the common bounded
    stock runner, not as independent children of the killable host CLI. Eight
    preflight/commit calls and four reserved rollback calls each have a one-minute
    control deadline plus bounded cleanup; their duration and backend preparation
    are excluded from the host's idle watchdog. Unconfirmed mutating delivery or
    cleanup fences the scope rather than permitting later operations to race it. Missing names create no per-name lock
    inodes. A pending unpublish fences a slow existing load before its final
    stock dispatch, without claiming that worker preparation was cancelled.
    Relay authority is session-bound; a host master token cannot impersonate a
    guest load request. Success prints the actual server and sandbox, not a
    guessed prefix. Invalid raw publish requests (including simultaneous Kit
    and sandbox selectors) preserve the existing private lineage and cause no
    preparation or stock effects. Framed host/daemon replies carry typed
    committed/rejected-before-effect/uncertain results; stdout, stderr prose,
    exit-zero or a legacy digest sentinel cannot stand in for a commit receipt.
    Lost replies after dispatch and post-effect failures remain uncertain.
    The controlled guest CLI checks full actionable shell/job
    IDs in both `ps --marsh` and `top --marsh --once` output.
    `make mcp-load-callers` (also run by `make test` and `make mcp-test`) invokes
    `run_publication_callers.py`: it builds all three host binaries into an owned
    cache, retains their hashes and copies in `$(TARGET_DIR)/mcp-callers/bin`,
    then builds and runs the real Rust callers from a second owned cache. The
    two caches persist under `$(TARGET_DIR)/mcp-callers/cargo-host` and
    `cargo-tests`, so a rerun recompiles only what Cargo's own fingerprints
    mark changed, and Cargo (never a copied binary) produces every executable
    that runs. A cache is discarded and rebuilt from empty when the toolchain
    or the Cargo/rustc environment differs, when it
    exceeds its size bound, or when any build input (crates, vendored Brush,
    Cargo and toolchain files, embedded packaging and fixture files) has
    different content from the last passing run yet no newer mtime, the one
    change Cargo's mtime fingerprints cannot see. `MCP_FRESH=1`
    (`--fresh`) builds both in empty temporary caches as the original gate did,
    and an immutable source capsule (no Git root) always does. It runs the public
    host CLI suite once. No stale default or prebuilt-binary shortcut
    substitutes for that build. Source and all host and test executable hashes
    are checked again after the gate. Callers require an explicit binary path,
    fail if any sibling is missing, and record their binary SHA256s in
    `$(TARGET_DIR)/mcp-test-evidence`. These are controlled process/Unixsocket
    tests, not VM qualification or observed-build provenance receipts. The host
    transaction suite uses a controlled framed daemon peer to seed publications;
    the separate real daemon/guest CLI journey proves session admission and
    lineage. Neither route bypasses publication validation.
    [`mcp_load_uat.py`](../acceptance/mcp_load_uat.py) is the separate
    observed-build-receipt-gated stock journey: existing sandbox plus a different
    Kit, unchanged generation, actual Gateway and original MCP client calls,
    wrong-scope denial, revocation against a reset/cold Kit (no worker or VM
    creation), and independently checked cleanup. With `--kit fixture` it also
    runs the default (untargeted) publication journey: not loaded into a Kit VM
    running a held job nor into that VM's next job, loaded into the VM created
    after `workers reset`, denied there after unpublish, and a host-terminal
    unpublished default not loaded into a later VM. **Only that stock journey**
    checks a real Kit's publication boundary: environment, socket/token file
    metadata in runtime paths and mounted homes, mount/Unix-socket tables,
    absence of the host control root, and a denied receipt-bound guest CLI
    `mcp load` caller inside the job. Controlled tests do not establish those
    worker isolation facts.

The packaged [`mcp_gateway_uat.py`](../acceptance/mcp_gateway_uat.py) runs
against the stock SBX Gateway and a disposable already-running agent sandbox.
It verifies attached-shell publication, discovery, byte-exact binary output,
`pipefail` nonzero status, the input/output bounds, cancellation without a
false cleanup claim, and an unchanged sandbox boot ID. Stock Gateway can mask
the structured cancellation result as a generic `context canceled` error, so
its cleanup certainty is then unobservable. Its invocation and
preserved-artifact policy are in
[`MCP_GATEWAY.md`](../acceptance/MCP_GATEWAY.md).

Registered-command MCP results expose the bound receipt's shared typed
`execution` field directly, including observed exits, limits, setup stage and
unknown execution. It is independent of the MCP invocation's `outcome`, public
exit code and cleanup facts. Pipeline exports or missing receipts omit this
per-worker field rather than inventing one worker's result. The real MCP and
daemon Unix-socket caller `observed_worker_outcome_survives_real_mcp_and_daemon_sockets`
checks that public status 125 and legacy cause prose do not overwrite the worker
fact. This is wire-fidelity evidence, not proof of real VM limit enforcement.

## Required evidence

The UAT result identifies the exact `marsh-mcp`, `marsh`, and `sbx` binaries,
their pinned identities, source revision and dirty state, platform, canonical
workspace identity, development root and selected scope identities and safe
modes, MCP protocol version, whether full SBX control was enabled, selected
qualification profile/evidence IDs, operation timings, and cleanup outcome.
It does not capture prompts, command text, stdout/stderr beyond the redacted
bounded test transcript, token values, or provider credential values.
