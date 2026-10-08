# Local product contract

This is the user-visible contract for the lean local product. Read it with
`../architecture.md` (implementation) and `../model/README.md` (cross-component
state). `context.md` is historical and is not a requirement.

## Shell

On an Apple Silicon Mac with stock Docker Sandboxes, `marsh` opens a Debian
ARM64 Brush shell at the launch directory's exact absolute path. Brush owns
shell syntax, expansion, pipelines, redirection, background jobs, `jobs`,
`wait`, signals, and exit status. marsh never delegates evaluation to Bash.
`msh` is installed beside `marsh` as a symlink and behaves identically.
A user may instead choose the shell VM's own `bash` or `zsh` (`--shell`,
`MARSH_SHELL`, or `marsh config shell`; `../shells.md`). That shell then owns
syntax and evaluation with the user's own startup files; registered commands,
the `marsh` CLI, `acp`/`mcp`, PTY, Ctrl-C, exit status, and home persistence
are unchanged; only the Brush sugar and Brush-only hooks are lost.

All shells for one macOS user and selected `MARSH_HOME` share one same-user
daemon. Persistent shells share one warm shell VM. Each `--ephemeral-home`
shell uses its own disposable shell VM, so its HOME mount cannot replace a live
shell's mount. marsh coexists with other stock SBX clients.

The launch project is mounted read/write at its natural Mac path in the shell
VM and in every job container. The guest username matches the Mac user, and
guest `$HOME` is the natural Mac home path backed by `$MARSH_HOME/home`
(default `~/.marsh/home`), never the real Mac home. Home persists by
default. `--ephemeral-home` uses one of 16 private owner-only slots, does not
write back, and refuses to reuse a slot that still holds data.

The selected home is the shell VM's trust domain. Projects under one selected
home share that VM, guest user, mounted files, and relay credentials. Use a
separate selected home for work that must be private from another project.

The shell user has passwordless `sudo` and membership in the VM's `docker`
group. This is root-level authority inside the shell VM only; it grants no
host Docker or stock SBX control. Packages installed this way last only as long
as that VM.

Interactive PTYs preserve terminal bytes, color, paste, resize, Ctrl-C, and
exit status. Noninteractive commands preserve stdin, separate stdout and
stderr, pipelines, redirection, `$!`, `jobs`, and `wait`.

## Registered Kit commands

A command registration maps a shell name to a native Kit v3 source directory
or immutable Kit OCI reference. The mapping cannot restate image config, argv,
capabilities, policy, or resources; the Kit's OCI config and descriptor are the
only source of those. Overrides live in `commands.json` in an owner-only host
control directory scoped by the SHA256 of the canonical selected-home path,
outside every guest mount.

Invoking a registered command creates a fresh nonroot container from the exact
Kit image inside that Kit's warm VM, attaches it as an ordinary shell process,
and deletes it afterward. Sequential calls reuse the VM with distinct container
identities. Private command links come first on `PATH`, so child scripts,
`make`, and agents take the same route. Brush function and builtin precedence
and explicit paths keep their ordinary meaning.

Each warm Kit VM has one retained worker transport that multiplexes up to eight
concurrent jobs. Losing it during active work never replays a job: the affected
receipts become `cleanup_uncertain` and the VM is quarantined. A quarantined
VM accepts no new jobs until `marsh workers reset KIT` retires it. A dead idle
transport may be replaced after liveness validation.

Any process in a Kit job can start registered commands too
(`docs/processes.md`). Every job gets a read-only `/run/marsh` with
`cap.sock`, `job.json`, `context.md`, and one link per registered name first
on `PATH`; `bash`, `sh`, and the image's other tools stay the image's own
(agents learn about marsh from injected context, `marsh context`). A registered
name, `marsh run NAME`, or a split argv branch from inside a job becomes a
child job with the parent's mounts, recorded under its parent
(`lineage {parent, root, depth, spawn}`, `marsh jobs --tree`). One admission
covers every job: 8 concurrent per daemon, depth 4, 4 live children per job,
the same Kit at most twice in a row, 64 per tree, and the spawn set
(`MARSH_SPAWN`, `marsh run --spawn/--no-spawn`) that only narrows. Refusals
exit 125 and leave no receipt. A job's end cancels its children with
verified deletion; children never get a TTY.

`--load KIT,...` and `--load all` prepare the selected Kit VMs and job images
before the prompt returns. A startup notice appears on stderr only when marsh
actually boots a VM.

SBX supplies each Kit VM's declared network policy and credential proxy. Jobs
receive the native OCI environment plus validated `HOME`, `USER`, `LOGNAME`,
and `MARSH_SELECTED_HOME`. When stock SBX provides them, the adapter also
passes only the fixed proxy endpoint, bounded non-secret credential-mode
sentinels, a read-only CA bundle, and the sandbox-local MCP Gateway URL plus
sentinel name as a validated pair. Jobs receive no raw keys, SBX control, or
Docker/containerd/daemon socket; their only daemon channel is the scoped job
capability socket. marsh does not inspect inference requests.

## VMs, ownership, and mounts

Host `sbx` is trusted. marsh uses only its public CLI and never ships a patched
SBX. Every VM marsh creates has a random name (`marsh-k-xxxxxxxx` for Kit VMs,
`marsh-s-xxxxxxxx` for shell VMs). The daemon persists the name as an intent in
its ownership map before `sbx create`, then records the observed UUID. marsh
stops, removes, mounts into, or execs into only VMs present in that map with
their recorded UUID; anything else is foreign and left alone.

Project and home sources are admitted through no-follow directory descriptors
and checked before and after `sbx mount`. A mount is made on first use and kept
while the VM is warm; an idle mount is replaced only when its source changed,
and all mounts are released when the VM is retired. The pathname reopen inside
`sbx mount` is an upstream limitation accepted under the trusted-host
assumption.

## Composition and results

`fanout { ... } | collect` runs up to 16 branches concurrently in cloned shell
contexts in the current project, so file writes are shared and may race.
Branches are label-led (the same rule for `split { }`): `label: list` names
one, and a branch starts after `{`, a newline, or a comma, or after a semicolon
only when the next word is a bare `LABEL:` (`[A-Za-z_][A-Za-z0-9_-]*:`). Any
other semicolon, `&&`, `||`, group, or subshell is ordinary Bash sequencing
inside the branch, so `a: cd x; make; b: cd y; make` is two branches and an
unlabeled `x; y` is one.
Stdin is read once, up to 64 MiB, before any branch starts. Branches share one
16 MiB stdout/stderr capture budget; crossing either limit cancels and reaps
all branches and renders nothing. Per-Kit capacity (8) is independent, so more
than 8 branches on one Kit produce visible capacity failures. `collect` emits
declaration-ordered bytes, supports `--timing` and `--json`, and is nonzero if
a branch fails; normal `pipefail` rules apply downstream.

`marsh split [-n] [-b LABEL=STRING]... [::: LABEL CMD ARGS...]...`, `marsh
join [--json] [--keep] [-- CMD ARGS...]`, and `marsh splits` run branches in
daemon-owned private forks of the caller's tree under
`<root>/.marsh/split/<id>/` (`../workspaces.md`). Shell branches (`-b`) are
trusted and run in the shell VM; argv branches (`:::`) run one registered Kit
command that mounts only its fork and read-only Git metadata. Results are
`out/<label>/{stdout,stderr,status,diff.patch,files}`; marsh never writes the
user's tree or `.git`. Only the split's creator can join it; an unjoined split
is kept after 60 s. The Brush sugar `split { ... } | join | CMD...` keeps its
grammar, `PIPESTATUS`, and `SPLIT_*` scoping as a thin client of the same
requests. Ignored files are not copied into forks.

`marsh results` (alias `marsh jobs`) shows newest-first structural receipts and
does not replace Brush's `history`. The journal is owner-only, bounded, and
checksummed under the host control directory. It never stores prompt, stdout,
or stderr bytes. Work active across a daemon exit becomes `unknown`; receipts
do not resume workloads.

## ACP and MCP

An attached shell can start ACP agent sessions (`acp run`, `acp ask`,
`acp cancel`, `acp stop`) backed by registered ACP Kits. It can publish a
running session as an MCP control tool; clients that load it can prompt and
cancel that agent and answer its one-time permission requests. It can also
publish a fixed project pipeline with `mcp publish NAME -- 'PIPELINE'` and load
it into a named same-user stock SBX sandbox. Publications bind one target, are
explicit and revocable, and revocation is terminal. An untargeted pipeline
publication is a default: every agent Kit VM the daemon creates afterwards
loads it before its first job, and no VM that is already running is changed. These grants also apply to
code running in that shell.

## Development from inside marsh

`marsh --dev` (on a `make dev` install, or a daemon started with
`MARSH_ENABLE_DEV_SCOPES=1`) opens a dev
shell whose `sbx` is a host broker confined to one revocable grant: random VM
name prefix, project and per-project scratch roots, at most 8 VMs, removed when
the session ends. `make dev-inner` / `make dev-inner-run` build and run a Linux
candidate there. See `../self-development.md`.

## Release gate

A release passes every observation in `../../tests/acceptance/CONTRACT.md`
with `make acceptance` on stock SBX, using a freshly built candidate and an
immutable fixture Kit. Unit and mock tests support but do not replace that run.
No private SBX fork, patched binary, or direct-in-VM command fallback is
allowed.

## Out of scope

Cloud execution, general M:N scheduling, isolating shell-VM commands in a
`split` branch, copying ignored files into split worktrees, workload restart recovery, multi-device history, additional host architectures, and
public signing or notarization. There is no flight recorder: marsh keeps no transcripts of shell, job,
ACP or MCP bytes; `marsh results` holds structural receipts only.
