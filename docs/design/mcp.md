# Host MCP server

`marsh-mcp` is a local, stdio [Model Context Protocol][mcp] server for driving
one marsh development workspace from a host-side client. It gives a
coding agent a useful, inspectable way to run the project shell, manage the
marsh workers that belong to that workspace, and inspect structural receipts.
Full SBX control is a separately enabled operator capability.

It is a product-control adapter, not a general macOS, Docker, or `sbx` remote
shell. The server owns exactly one canonical workspace and a bounded set of
independent, owner-only development scopes. Each scope has its own
`MARSH_HOME`, daemon, project-shell VM, Kit VM pool, and structural results.
It never offers raw SBX/Docker
commands, daemon tokens, a host shell, caller-selected environment variables,
caller-selected filesystem paths, or VM/container identifiers.

[mcp]: https://modelcontextprotocol.io/

## Topology and boundary

```mermaid
flowchart TB
  subgraph host["macOS · one user"]
    client["Host MCP client"]
    mcp["marsh-mcp · stdio\nfixed workspace + scope registry"]
    client <-->|"MCP stdio"| mcp
  end

  subgraph default["default scope · independent MARSH_HOME"]
    daemon1["same-user marsh daemon"]
    shell1["project shell VM"]
    kits1["peer Kit VMs\nfresh job container per invocation"]
    daemon1 --> shell1
    daemon1 --> kits1
  end

  subgraph generated["generated UUID scope · independent MARSH_HOME"]
    daemon2["same-user marsh daemon"]
    shell2["project shell VM"]
    kits2["peer Kit VMs\nfresh job container per invocation"]
    daemon2 --> shell2
    daemon2 --> kits2
  end

  mcp -->|"typed requests · scope default"| daemon1
  mcp -->|"typed requests · opaque scope ID"| daemon2
```

The MCP service runs on the Mac. Direct clients start a stdio process; stock
SBX starts a stdio proxy to the resident per-workspace broker. Both use marsh's
authenticated same-user control path, but limits that authority to its fixed
workspace and server-created scopes. `shell_run` sends a command to the default
scope's project shell VM; `scope_run` targets one opaque scope ID. Neither runs
a host command. A registered command inside that project-shell command follows
the normal Kit VM and fresh-job-container path.

All scopes mount the same canonical workspace. Commands in two scopes can
observe and race on the same project files, just as two local editors can. Use
separate Git worktrees or application-level coordination when agents must make
independent writes; scope isolation does not serialize workspace I/O.

This first delivery is **host stdio only**. A Kit job container must not receive
the host daemon credential, the MCP server's inherited authority, or a raw
network endpoint to it. Direct access to this host-control server from a Kit or project-shell VM is
unsupported. A native Kit's agent may connect to the **stock SBX MCP gateway**
when it is enabled for that Kit VM; this uses a sandbox-local endpoint and
sentinel name, not this server's host daemon credential or socket. Gateway
servers are loaded per Kit VM, not per fresh job: loading `marsh-dev` into a
Kit VM would deliberately grant every job its development-control tools.
Mounting the host socket or forwarding a host bearer token is not an
acceptable substitute.

## Start it

Build and install with the normal repository flow:

```console
make build
make install
```

For development, `make dev` is the single step: it builds everything
incrementally and installs the product to `~/.marsh-dev`.

The server canonicalizes its workspace once at startup and rejects any later
attempt to change it. `serve` with neither `--home` nor `--scope-root` runs
legacy single-scope mode on the same selected home the `marsh` CLI uses
(`MARSH_HOME`, else `~/.marsh`) and disables `scope_start`. Broker modes (and
the `marsh mcp install sbx` registration) use a generated owner-only scope root
at `$HOME/Library/Application Support/marsh-mcp-scopes/<workspace-hash>` on
macOS (`$MARSH_CONTROL_HOME-mcp-scopes/<workspace-hash>` when
`MARSH_CONTROL_HOME` is set). It lives beside, never beneath, the protected
host state root, because its scope homes are mounted into shell VMs. The
reserved `default` scope uses the `default/` child; `scope_start` creates
opaque UUID siblings. A caller never chooses a scope path, environment, or VM
identity. `--scope-root` selects another fixed owner-only root. `--home`
selects an exact home in legacy single-scope mode; the two options are
mutually exclusive.

One MCP owner holds an exclusive owner-only lease for the entire development
root. Direct Codex and Claude registrations each use their own client root.
The SBX registration instead uses one resident per-workspace Unix-socket broker
that owns the lease and shared scope state; every sandbox gets a thin
authenticated stdio proxy to that broker. Multiple sandboxes or Kit VMs can therefore
load the same SBX registration concurrently without starting competing scope
owners. Legacy `--home` mode has no managed root registry or broker; its
lifecycle state lasts only for that direct MCP process.

Before creating or using a scope home, the server binds every managed root to
the canonical path and filesystem identity of the configured workspace. A
later process can reuse that root only for that exact workspace identity. This
also covers an explicit `--scope-root`, whose name may not encode a workspace.
For fail-safe migration, a populated managed root that predates the binding is
rejected rather than claimed automatically. Point the server at a new empty
root, or inspect and explicitly repair the owner-only old state. Missing or
replaced persisted scope homes are isolated as failed records on restart; the
server does not recreate them or bless a replacement directory with a new
identity.

The previous experimental `$HOME/.marsh/mcp/<workspace-hash>`,
`$HOME/.marsh/.dev-mcp/<workspace-hash>`, and
`$HOME/Library/Application Support/marsh/.dev-mcp/` locations are not part of
this namespace. The server does not read, migrate, or delete that state; an
operator may inspect and remove it separately after confirming it is unused.
The scope root is host-side control state only. It is not a location or
protocol for future MCP capabilities exposed to project-shell or Kit
containers.

The `--marsh` and `--sbx` executables are trust anchors, not convenience paths.
Both must be installed outside the mutable workspace and managed scope root. The
server pins their identities at startup and revalidates them before each host
invocation; replacement, permission changes, or an identity mismatch fail
closed. `marsh` and its sibling `marshd` also require a trusted, non-writable
ancestor chain. The external Homebrew-managed SBX executable retains strict
regular-file, owner, mode, device, inode, and content-hash pinning, while its
Caskroom ancestor permissions are not treated as marsh-managed trust state.
Use `make install` first and point at that installed `marsh`.
Homebrew owns the selected stock `sbx` installation, stable or nightly.
`make install` never
installs, copies, signs, modifies, or removes SBX. marsh resolves `MARSH_SBX`
when explicitly set, otherwise `sbx` on `PATH`, and canonicalizes that exact
executable. Existing client registrations intentionally retain the canonical
path they stored; after Homebrew upgrades the Cask, restart marsh and rerun the
applicable `marsh mcp install CLIENT` command.
Do not point the MCP server at anything under `target/`, another file in the
checkout, or a binary under the selected home.

For host clients, run the applicable command from the workspace after
`make install`:

```console
marsh mcp install codex
marsh mcp install claude
marsh mcp install sbx
```

Each command registers `marsh-dev-<workspace-hash>` and pins the canonical
workspace and resolved official SBX executable. Codex uses a durable private
root beneath `${CODEX_HOME:-$HOME/.codex}/marsh/dev-mcp`; Claude uses
`${CLAUDE_CONFIG_DIR:-$HOME/.claude}/marsh/dev-mcp`; SBX uses the generated
scope root beside the host state root
(`~/Library/Application Support/marsh-mcp-scopes/clients/sbx/<workspace-hash>`). Client names and the full workspace
hash keep roots disjoint. Codex and Claude launch the stdio subprocess when
needed; Claude uses its local workspace scope rather than
writing shared project MCP configuration. For SBX, installation starts the
shared broker and registers the bounded `connect` proxy with its gateway for
later `sbx mcp load`. Registrations for different workspaces and clients
coexist. `marsh mcp start` restores the broker after a host restart without
changing the registration. `marsh mcp stop` stops an idle broker and refuses
while any sandbox proxy is attached. `marsh mcp install sbx` is explicitly
replacing: it runs
`sbx mcp rm --force NAME` before adding the current definition. It accepts
success or the exact `MCP server "NAME" not found` diagnostic for that
deterministic name; transport, authentication, lookalike-name, and all other
remove failures abort. Active sandboxes retain their already-loaded proxy and
broker session; replacement changes future loads only. A changed installed
broker build replaces an idle old broker automatically and refuses to replace
a busy broker, so stop loaded sandboxes and rerun install for a clean upgrade.
If SBX later provides explicit MCP unload,
unload first and then load.
An interruption between remove and add leaves the registration absent;
rerunning the same install command is safe.

Other stdio MCP clients can launch the equivalent server directly:

```console
SBX_SOURCE=${MARSH_SBX:-$(command -v sbx)}
SBX_BIN=$(realpath "$SBX_SOURCE")
marsh-mcp serve \
  --workspace "$(pwd)" \
  --marsh "$HOME/.local/bin/marsh" \
  --sbx "$SBX_BIN"
```

For an SBX agent sandbox, `marsh mcp install sbx` uses the canonical official SBX executable.
`sbx mcp load` then gives the selected running sandbox access to its stdio
protocol. It does not mount a host socket, expose a TCP port, or require
Tailscale. The sandbox's agent receives exactly the server's tool surface, so treat
loading this registration as granting that sandbox full marsh product authority
for the configured workspace and its managed development scopes. The manual
form below illustrates the gateway shape with an explicit private scope root:

```console
SBX_SOURCE=${MARSH_SBX:-$(command -v sbx)}
SBX_BIN=$(realpath "$SBX_SOURCE")
sbx mcp add marsh-dev \
  --command "$HOME/.local/bin/marsh-mcp" \
  --args "connect,--workspace,$(pwd),--scope-root,$HOME/Library/Application Support/marsh-mcp-scopes/manual-sbx,--marsh,$HOME/.local/bin/marsh,--sbx,$SBX_BIN" \
  --dir "$(pwd)"
sbx mcp load marsh-dev --sandbox SANDBOX_NAME
```

Before using that manual registration, run the same argument list once with
`broker-start` in place of `connect`. The normal `marsh mcp install sbx`
command performs both steps and is preferred.

Stock SBX warns accurately that a `--command` MCP registration is an
unsandboxed host subprocess with the user's permissions. Use it only for this
reviewed local binary, and never substitute an untrusted executable. The host
subprocess uses a fixed minimal `PATH` and the pinned `--sbx` executable; it
does not inherit `MARSH_SBX`. At startup, `marsh-mcp` resolves the effective
UID through the operating-system passwd database, validates the account name,
and sets both `USER` and `LOGNAME` explicitly in every cleared child
environment. It never trusts optional inherited identity variables. Tool calls
cannot add, replace, or inspect environment variables. A direct MCP client can
equivalently launch the same command and argument list over stdio.

### Deliberate full-SBX-control opt-in

Full SBX control is **disabled by default**. `qualify` remains visible in the
static MCP tool list, but every call returns a clear disabled failure until the
server is started with `--allow-full-sbx-control`. This is intentional. A source,
smoke, full, or performance gate runs
repository recipes on the Mac; a mutable `Makefile`, build script, test helper,
or dependency build step can execute code as the host user. An MCP sandbox does
not contain that host execution.

Enable it only when the checkout and the agent directing it are trusted for
host-user code execution. The flag grants that authority; it does not make the
checkout sandbox-scoped or safe for an untrusted prompt. A second server cannot
share the everyday server's leased root. Register qualification with a distinct,
explicit host-only root; its scopes and results are intentionally separate:

```console
SBX_SOURCE=${MARSH_SBX:-$(command -v sbx)}
SBX_BIN=$(realpath "$SBX_SOURCE")
sbx mcp add marsh-dev-qualify \
  --command "$HOME/.local/bin/marsh-mcp" \
  --args "serve,--workspace,$(pwd),--scope-root,$HOME/Library/Application Support/marsh-mcp-scopes/qualify,--marsh,$HOME/.local/bin/marsh,--sbx,$SBX_BIN,--allow-full-sbx-control" \
  --dir "$(pwd)"
sbx mcp load marsh-dev-qualify --sandbox TRUSTED_SANDBOX_NAME
```

## First-delivery tool surface

All tools return structured JSON with bounded byte-faithful output encoded as
base64 where command streams are present. Server errors are typed and fail
closed. Stdio frames are capped at 1 MiB, output is
bounded, and one global subprocess cap applies across all operations; capacity
exhaustion is a typed response, never an unbounded host process queue. Tool
names and fields below are the public contract; omitted fields are rejected
rather than interpreted as defaults.

| Tool | Input | Result | Boundary |
| --- | --- | --- | --- |
| `doctor` | none | fixed workspace, home, executable, and static limits | No credentials or daemon relay authority. |
| `status` | none | versioned marsh status for this scope | Read-only product state only. |
| `results_list` | none | newest-first durable receipts | No stdin, stdout, stderr, or prompt retention. |
| `result_get` | exact receipt cursor or job selector | one durable structural receipt | Selector resolves only through this scope. |
| `shell_run` | `command`, optional bounded timeout | opaque operation ID | Runs only through the fixed project's marsh shell VM. It never invokes a host shell. |
| `scope_start` | empty object | opaque scope ID and boot operation ID | Creates an owner-only home beneath the fixed development root; callers provide no path, label, environment, or runtime identity. |
| `scope_list` | empty object | scope ID, lifecycle state, and creation time | Rediscovers persisted scopes without exposing homes, endpoints, or runtime IDs. |
| `scope_run` | opaque scope ID, `command`, optional bounded timeout | opaque operation ID | Runs through that scope's independent project shell and VM pool. |
| `scope_results_list` | opaque scope ID or `default` | newest-first durable receipts | Works for Ready and Stopped scopes. A Stopped read may start only the local daemon, then sends exact typed stop control before returning; it does not boot project-shell or Kit VMs. |
| `scope_result_get` | opaque scope ID or `default`, exact receipt cursor or job selector | one durable structural receipt | Resolves only through the selected scope, including while Stopped. |
| `scope_prewarm` | opaque scope ID or `default`, `selection: "all"` or validated Kit names | opaque operation ID | Prewarms only the selected Ready scope's registered Kits. |
| `scope_workers_reset` | opaque scope ID or `default`, `selection: "all"` or validated Kit names | opaque operation ID | Refuses active jobs and resets only selected workers owned by the selected Ready scope. |
| `scope_status` | opaque scope ID or `default` | lifecycle state, runtime certainty, and typed runtime status | Stopped is proven absent without starting a daemon; Ready may validate or recover its daemon through normal status handling. Failed is explicitly unknown with a bounded diagnostic. |
| `scope_reset` | opaque scope ID or `default` | opaque operation ID | Refuses active work, removes only that scope's exact runtime, preserves home/results, then boots a fresh project shell. |
| `scope_stop` | opaque scope ID or `default` | opaque operation ID | Refuses active work, performs a pinned boot/daemon validation, sends exact typed stop control, and preserves home/results. |
| `scope_remove` | stopped or interrupted-removal generated scope ID | opaque operation ID | Queues bounded deletion of the exact private home and record; default, live, and failed scopes are rejected. An interrupted persisted removal is retryable after restart. |
| `prewarm` | `selection: "all"` or validated comma-separated Kit names | opaque operation ID | Can name only the fixed scope's registered Kits. |
| `workers_reset` | `selection: "all"` or validated comma-separated Kit names | opaque operation ID | Refuses active jobs; affects only workers owned by this scope. |
| `qualify` | fixed gate: `source`, `smoke`, `full`, or `perf`; optional bounded timeout | clear disabled failure by default; opaque operation ID when explicitly enabled | The only tool that can run a non-marsh host program. |
| `operation_get`, `operation_output`, `operation_cancel` | opaque operation ID | state/metadata, terminal bounded output, or cancellation request | Limited to operations started by this client session. |

The original `status`, `results_*`, `shell_run`, `prewarm`, and `workers_reset`
tools are aliases for the reserved `default` scope. Stopping that scope disables
those aliases across MCP restarts rather than silently recreating its runtime. `scope_status` can
still inspect it, and `scope_reset` recreates and validates its project shell.
Lifecycle operations serialize within one scope; independent scopes may make
progress concurrently under the global four-operation ceiling. The server
allows independent read-only status and result calls to overlap. Read-only
calls also remain available during prewarm; whole-scope reset/stop/removal
take exclusive scope access, while `scope_status` continues to report the
persisted transitional state without probing the runtime.
The server
admits at most 16 live generated scopes and retains at most 128 generated scope
records. A failed or cancelled runtime start, reset, or stop with its exact home
identity intact leaves the scope in the inspectable `failed` state; callers use
`scope_reset` to recover it before new work is admitted. `scope_list` makes retained opaque IDs discoverable after
an MCP restart, and `scope_remove` frees retained capacity only after a generated
scope has reached the proven `stopped` state.

`failed` counts against the live-scope ceiling because its runtime may still
exist. When the exact home identity remains valid, recovery must succeed through
`scope_reset`, followed by `scope_stop`, before removal is allowed. A missing,
replaced, or malformed persisted home cannot be repaired automatically: MCP
does not recreate, recapture, delete, or free that record. The error directs the
operator to restore the exact original directory entry, or to stop MCP,
independently prove/remove the exact runtime with stock SBX, archive the old
development-control root, and select a new empty `--scope-root`. This deliberately
leaves the affected capacity charged until an operator resolves the ambiguous
runtime; a replacement path is never touched.

A receipt read from a Stopped scope is serialized against lifecycle changes.
The server always follows the read with bounded exact scope-stop control before
returning, even when the read fails. That cleanup uses the same 600-second bound
as other exact whole-scope lifecycle control; a shorter nested timeout must not
preempt valid stock-SBX cleanup. If the read fails before starting a daemon and
exact stop still proves absence, the scope remains Stopped. If daemon shutdown cannot be proven, the
scope becomes Failed rather than continuing to claim a stopped runtime.
Scope-home deletion first performs a bounded, no-follow traversal and rejects
excess depth, excess entries, special files, ownership changes, or replacement
before deleting the validated tree. Symbolic links are unlinked and never
followed. No detached deletion task
continues after the removal operation reports failure.

Runtime recovery from `failed`, when the exact home is still verified, first
performs a pinned project-shell boot so the exact control daemon exists, then
runs the typed whole-scope reset, then performs a final pinned boot validation.
Each step must succeed in order; identity-invalid homes and foreign or
unverifiable exact VMs still fail closed.

`shell_run` deliberately accepts a shell command string because development
requires shell syntax such as pipelines, redirections, `make`, and `cargo`.
The string is carried as data over the daemon's typed request and interpreted
inside the project shell by marsh/Brush. It is never concatenated into or
evaluated by a host shell. Its working directory is always the canonical
workspace. The tool cannot set a working directory, executable path, host
environment, mount, Kit source, resource ceiling, or VM identity.

When the server was started with `--allow-full-sbx-control`, `qualify`
accepts a fixed profile — `source`, `smoke`, `full`, or `perf` — and returns an
opaque operation ID. Without that flag it returns the clear disabled failure.
This is the only tool that runs a
non-marsh host program, through a repository-owned allowlisted recipe.

The profiles are:

- `source` runs the repository's source checks;
- `smoke` runs the assembled black-box smoke gate;
- `full` runs the full local Kit-v3 acceptance gate; and
- `perf` runs the repository's bounded performance probe.

The fixed repository recipe determines its own evidence path (for example,
under the workspace `target/` directory); `operation_output` returns the
bounded recipe output that names it. It does not accept a command, Make target,
arbitrary arguments, arbitrary evidence directory, or arbitrary source tree.
That narrow input surface limits accidental invocation; it does not make a
mutable repository's host recipes safe. A caller that needs a new host action
must add it to the reviewed allowlist and its contract first.

## Security, operations, and cancellation

The MCP client is trusted with all marsh product actions in the selected scopes;
it is not trusted with host or stock-SBX administration. In particular, the
server:

- canonicalizes the startup workspace and creates/checks an owner-only
  host-only development root, default home, atomic scope registry, and
  exclusive direct-process or broker lease before serving;
- uses no caller-provided path, environment, executable, SBX selector,
  Docker selector, socket, VM ID, or container ID;
- limits protocol frames to 1 MiB, bounds output and global child-process
  concurrency, and returns explicit truncation/timeout/cancellation status;
- requests cancellation of the submitted project command or qualification
  operation and reports the resulting terminal/uncertain state; and
- bounds concurrent operations, relies on marsh to refuse destructive worker
  changes with active work, and gives each operation an opaque ID.

Operation state and bounded captured output are in broker/process memory and
are accessible only to the client session that created them. Durable product
receipts remain available through `results_*`.
Persistent MCP audit/evaluation records are deliberately future work; this
delivery does not imply a durable host transcript. Cancellation is
conservative: this delivery always reports `cancellation_uncertain` after a
cancellation request, even when its local process-group cleanup completed,
because that alone does not prove the submitted sandbox work stopped.

`operation_output` deliberately returns the command's bounded stdout and
stderr to the MCP client. Although marsh does not forward provider credentials,
a command can print sensitive project content; treat that output as data the
connected client is authorized to read and do not use it as a secret store.

The existing SBX policy and credential boundary remains authoritative. The MCP
server does not read or relay secret values, Docker sockets, Kit-worker control
sockets, or the daemon's master credential to a job container. Provider
authentication still belongs to stock SBX and the Kit declaration.

## Published-command export mode

From an attached project shell, run `mcp publish NAME [--description TEXT]
[--kit KIT | --sandbox SANDBOX] -- 'PIPELINE'` to publish a fixed Brush pipeline, or
`mcp unpublish NAME` to revoke it. To load an existing publication elsewhere,
use `mcp load NAME --kit KIT` or `mcp load NAME --sandbox SANDBOX`; do not
republish just to add a client. These commands also work in noninteractive
shell scripts. Publication requires that attached session: a direct host
`marsh mcp publish` is rejected before touching publication files or stock SBX.
Host terminals can still load or revoke an **existing** publication in the same
project and selected home. The authenticated shell session sends a typed request
to the host daemon; Kit jobs have no publication token. The host owns the registration
and declaration, and pins the attached project's path and file identity.
With neither `--kit` nor `--sandbox` the publication is a *default*: after the
publish commits, the daemon records the stock server name, declaration path,
and generation in its control directory (`mcp-defaults.json`,
`marsh-daemon/src/mcp_defaults.rs`). Whenever the backend creates a
non-ephemeral Kit VM (`ReadyKitVm::cold_started`), it runs stock
`sbx mcp load SERVER --sandbox VM` for each record before the VM is cached as
Ready and before any job container starts there. It never loads a VM that
already exists, even an idle or adopted one, because the stock gateway is per
VM and every running container would see the new tool. Each record is re-read
against its declaration before loading; a missing declaration, a pending
revocation marker, or a different generation (host-terminal unpublish,
republish elsewhere) is skipped and pruned. A targeted republish and
`mcp unpublish` drop the record before their stock effects. A failed default
load leaves the VM usable without that tool and is logged; it is not a
preparation failure. `mcp load NAME --kit KIT` is the explicit way to load a
running Kit VM.
`--kit codex` (or another registered Kit command) prepares its worker and loads
the tool into the exact Kit VM selected by preparation, including when the worker is already warm and older Kit generations are still running. Later
jobs sharing that Kit VM in the selected-home scope can use the loaded tool,
including jobs from another project in that scope. Start a
new agent session after loading so it discovers the tool. `--sandbox`
explicitly grants the attached shell authority to load the tool
into any named running same-user stock SBX sandbox, including an unrelated agent
sandbox. Code running in the attached shell can exercise that grant; choose the
shell and target sandbox accordingly. Without `--sandbox`, load the reported
registration explicitly. Publishing grants every
client loaded into that sandbox the ability to run the fixed pipeline with the
project shell's privileges, including sudo and control of its private Docker
Engine, and read its output.

Public tool names use 1–64 ASCII letters, numbers, `_`, `-` or `.`, without a
leading `-` or a dot-only name. For example, `team.review` is a publication name.
A `--kit` target is different: it must be a registered Kit command name under
the command registry's rules, not an image reference or a dotted tool name.

Loading preserves the declaration bytes, generation, and original publisher's
recording policy. It creates no new publication grant and does not invalidate
already-loaded clients. A missing, revoked, stale, cross-protocol, or modified
registration is rejected rather than repaired. For `--kit`, validation happens
before Kit preparation; `publish --kit` also validates its declaration and
registration before preparing. ACP and MCP transactions share one project/home admission lock. The daemon
acquires the real cross-process lock before grant admission and retains it through
preparation, stock load, rollback and result checks. A competing publication is
rejected as busy before changing its grant or recording policy. Kit
preparation runs directly in the authenticated daemon and reports real cold-boot
progress; it does not reconnect with a master token or use the five-minute host-idle
budget. The daemon owns each bounded stock command, even if the host CLI exits.
Preflight/commit and rollback have separate command allowances; a slow rollback
cannot be cut short merely because preparation or prior stock calls used time.
Unconfirmed mutating delivery or carrier cleanup fences the scope for host
inspection rather than allowing a following unpublish to race unfinished work. Loading does not change network policy or move the
pipeline into the consuming sandbox's workspace.

Publication control replies distinguish `committed`, `rejected_before_effect`,
and `uncertain`. Rejection means no publication or worker preparation was admitted; an uncertain result requires inspecting the state
before retrying. Lost replies and failures after effects are not argument errors
or evidence of cancellation. The private host transaction returns framed JSON,
not an exit-code guess or a line parsed from stdout. Invalid targets are checked
before host-scope effects.

Unpublishing an existing MCP publication marks revocation pending before waiting
for a slow load's lock. The load rechecks that marker after preparation and will
not load the tool. Preparation itself may still finish; a closed CLI is not a
worker-cancellation receipt. If unpublish reports that the transaction is busy,
retry it to finish revocation. Until unpublish succeeds, previously loaded
clients may still call the tool; the pending marker fences new loads, not
existing calls. Loads and republishes remain blocked by the marker until
unpublish succeeds. A load already dispatched to stock SBX
is not retroactively cancelled; unpublish waits for its lock, then revokes it.

From a host terminal in the publishing project and selected-home scope:

```sh
marsh mcp load my-tool --sandbox EXISTING_SANDBOX
```

Host `--kit` requires the attached-shell context: run `marsh`, then
`mcp load my-tool --kit codex`. The result names the actual stock server and
target sandbox. Start a new agent session in the target to discover the tool;
existing sessions can keep cached tool lists.

For a plain Codex Kit demo from an attached marsh shell, publish and load in
one command, then start a new Codex process so it discovers the tool:

```sh
mcp publish shout-in-espanol --kit codex -- "tr a-z A-Z | claude -p 'Translate the piped text to Spanish. Output only the translation.'"
printf '%s\n' 'Call the MCP tool shout-in-espanol with input exactly: hi don, how are you? Reply only with its output.' | codex exec --skip-git-repo-check -
```

The published pipeline runs when Codex calls the MCP tool; it calls Claude in
print mode. `codex exec -` reads a prompt from stdin without a terminal. The
Codex Kit adapter also accepts `printf 'hi' | codex summarize` or `printf 'hi'
| codex 'Call the MCP tool with this input'` and sends the instruction and piped
text as one prompt. Use `--prompt` if the instruction is also a Codex command,
for example `printf 'hi' | codex --prompt review`.
The optional description and tool output are untrusted content shown to the
receiving agent; publish from a trusted attached shell only.

The default description says the pipeline runs in its **publishing marsh
project**. That is the project where `mcp publish` ran, using the selected
marsh home; it is not the consuming sandbox's workspace or the already-open
interactive shell process. Each call starts a fresh shell in that project.
Publishing registers a stock SBX MCP server. For a separate running sandbox,
use `mcp load NAME --sandbox SANDBOX` in the attached shell, or
`marsh mcp load NAME --sandbox SANDBOX` from a host terminal in the same project
and selected home. These forms check the declaration and exact registration
before loading; a raw `sbx mcp load` does not perform those checks.
An agent in a Kit VM may instead find the registered server in the stock MCP
Gateway catalog and add it to that VM; the user's live Claude Kit exercise
followed this path. It does not require a `claude --mcp` flag. The loaded server
set belongs to the Kit VM, so subsequent jobs there may see it. Publication
alone does not force every agent client to load or discover the tool.

Claude and Codex ACP sessions (the agent Kits' ACP mode) pass the validated stock Gateway as an HTTP MCP
server when creating their ACP session. Load the published tool into that Kit
VM before `acp run`, then use `acp prompt` and approve the exact tool's
one-time permission if asked. The ACP child path is covered by
`tests/acceptance/acp_mcp_uat.py` with real model calls and exact scope cleanup.
Plain Pi and Pi ACP use an image-owned Pi extension instead: Pi exposes
`mcp_gateway_list` and `mcp_gateway_call` to the model. Ask Pi to list the
Gateway tools, then call the published name with `{"input":"foo"}` (or the
listed tool's argument schema). The pinned Pi ACP adapter does not wire ACP
`mcpServers` into Pi; this extension connects only to the validated stock
Gateway, through the stock SBX proxy. Plain Pi and Pi ACP run in the same pi
Kit VM, so `mcp load NAME --kit pi` serves both for an existing publication
(use the registered command name in your scope). Every fresh job in a
loaded Kit VM can use its Gateway tools. `tests/acceptance/pi_mcp_uat.py`
checks real plain Pi and Pi ACP model calls, exact pipeline side effects, and
disposable-scope cleanup.

Host Codex MCP configuration is separate from stock SBX's registration. From a
macOS terminal in the publishing project, with the same `MARSH_HOME` and
`MARSH_CONTROL_HOME`, run `marsh mcp install-published codex NAME`. This adds
only the fixed export server to the host Codex account. Start a new Codex task
to discover it; already-running tasks may retain their tool list. When finished,
run `marsh mcp remove-published codex NAME` from that same project and scope.
After republishing the same name, run `marsh mcp remove-published codex NAME`
followed by `marsh mcp install-published codex NAME` to bind Codex to the new
generation.
Both commands verify the exact marsh-owned registration and refuse a name
collision or modified Codex entry. A published tool grants Codex tasks under
that account the ability to run its pipeline with the publishing project
shell's privileges, including guest sudo and private VM Docker access.
`mcp unpublish NAME` still revokes
calls if a Codex task has cached the tool, but it leaves the account's Codex
registration until the host removal command runs.

The separate `marsh mcp install codex|claude` commands above install the
broader **development-control server**, not this fixed published pipeline.
Treat a one-off direct stdio call as protocol verification, not proof of
native Codex tool discovery.
The focused `tests/acceptance/published_codex_uat.py` journey uses stock SBX,
a disposable publishing project and sandbox, and a private `CODEX_HOME`. It
checks the real Codex CLI registration, calls the registered export server over
MCP, verifies Gateway access and cached-call revocation, and rejects a
same-name Codex collision. Native Codex desktop task discovery remains a
separate manual acceptance check.
Run it on a clean, packaged candidate with `--marsh`, `--sbx`,
`--guest-artifacts`, `--codex`, `--source-tree`, `--source-revision`, and
`--evidence` pointing to that exact build and an outside evidence directory.
The report records source and binary hashes, stock SBX version, and cleanup.

`marsh-mcp export-serve` is separate from this development-control server. It
loads one explicitly declared typed tool and exposes no `shell_run`, scope
control, worker reset, or `qualify`. It requires `--home ABS` naming the exact
normal marsh command-registry home (commonly `~/.marsh`); it rejects the
generated development scope root for this workspace and `--scope-root`.
Other manually chosen development roots are not detected: do not pass one as
`--home`. The native Kit declaration
still controls its workload and network/credential authority. Its guest HOME
is the natural account home backed by that selected home, just as with an
ordinary marsh command. The client must opt in to registering/loading the
export-only server. A tool is not automatically published because its shell
command is registered. See `tests/mcp/CONTRACT.md` items 28-35 for the
candidate-specific proof still required before claiming stock-SBX support.

Published Brush pipelines run in the project-shell VM through marsh, so their
result is a shell pipeline result rather than one Kit job receipt. A registered
command inside the pipeline still uses its ordinary Kit path. Output base64 is
the authoritative byte stream; the optional `stdout` and `stderr` text views
report `utf8`, `lossy`, `omitted`, or `truncated` states. Text may be omitted to
keep the MCP response bounded while the base64 bytes remain complete. Pipeline
`cleanup_certainty` remains `uncertain` for sandbox work even when the host
process group has been reaped. `host_cleanup_certainty` separately reports
the host process-group check; it does not verify nested VM processes. See
contract items 36-37.

Republishing replaces the stock SBX registration with a new generation. A server
already loaded into a sandbox keeps its startup declaration: after the declaration
changes, it returns an empty `tools/list` and rejects `tools/call` with a reload
error. Starting an exporter from the old registration also fails. Reload the new
registration to use the new publication. The export server does not send
`notifications/tools/list_changed`; clients or gateways that cache discovery
may continue showing the old tool until reloaded. Unpublishing likewise makes
already loaded servers return an empty list and reject calls.

## Qualification status

This document defines the intended first delivery; it is not evidence that the
MCP server has passed UAT. The required black-box protocol, security, and
end-to-end cases are in [`tests/mcp/CONTRACT.md`](../../tests/mcp/CONTRACT.md).
The normal local product release gate remains
[`tests/acceptance/CONTRACT.md`](../../tests/acceptance/CONTRACT.md).
