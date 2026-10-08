# FAQ

marsh is Bash with `split`, `fanout`, `join`, and `collect` for running agents
in parallel, each command in its own sandbox. This is what it does:

```console
marsh-0.5$ marsh split -n ::: a claude -p 'fix the flaky test' \
                          ::: b codex exec 'fix the flaky test' | marsh join --keep

== a (exited 0, kit:claude) ==
The test raced on a shared port; it now asks the kernel for a free one.
-- a: 1 file (M test/net_test.go); diff …/out/a/diff.patch --
...
== b (exited 0, kit:codex) ==
Added a retry around the dial and a longer timeout.
-- b: 2 files (M net/dial.go, M test/net_test.go); diff …/out/b/diff.patch --
...
marsh: join: kept /Users/you/src/my-project/.marsh/split/8d5c8fdd1563 (join --keep); inspect, then: marsh splits rm 8d5c8fdd1563
marsh-0.5$ git apply .marsh/split/8d5c8fdd1563/out/a/diff.patch
```

Each agent worked on its own fork, a private copy of the project. (`...`
marks omitted output.) Your files changed only
when you ran `git apply`. Ctrl-C during the split would have stopped both
agents and anything they started.

## Why not something else

### Why not run agents in Docker myself?

You can. A container with the project mounted read/write can also delete the
project. marsh provides:

- A fresh nonroot container per call, inside a VM per Kit. The VM has its own
  Docker Engine and no Mac Docker socket.
- Placeholder credentials in the container. The real keys stay with the
  Docker Sandboxes proxy.
- A private copy of the project per `split` branch, and a patch back.
- Limits per job (CPU, memory, processes, output, wall time), one job tree
  across nested agents, and a check that each container was removed.

A mounted project is still writable, in marsh or in Docker. See
[What can go wrong](#what-can-go-wrong).

### Why not git worktrees?

A worktree is a second checkout. It does not limit what a process running in
it can do on your Mac.

A `split` branch that runs a registered command (`::: LABEL CMD ...`) mounts
only its fork, so it cannot write your real files or `.git`. The directory does
not have to be a Git repository. A fork is a copy-on-write clone: it shares
storage with the original until a branch writes.

`split` returns each branch's result as a `git apply` patch. It does not
merge, and it does not pick between branches.

### Why not tmux?

tmux gives you several terminals. You start each agent, watch each one, and
collect results yourself.

`split { ... } | join` starts the branches, waits for all of them, and prints
each one's output, exit status, and diff. `marsh jobs --tree` shows which job
started which. Ctrl-C stops the whole tree. tmux does not start branches or
capture what a pane changed.

### How is it different from plain `sbx`?

marsh calls the public `sbx` command line and ships no modified copy. `sbx`
creates the VMs, enforces each VM's network policy, and holds your credentials
behind a proxy. marsh adds:

- a Bash-compatible shell that opens your project in a VM at the same path
- registered commands: a name such as `codex` that runs a Docker Sandboxes
  Kit in a fresh container ([Commands and Kits](kits.md))
- `split`, `fanout`, `join`, and `collect`
- nested jobs under one tree, with limits, and `marsh results`
- MCP and ACP publication of pipelines and sessions

marsh needs Docker Sandboxes 0.45.0 or newer.

## What it costs

Money
: marsh is Apache-2.0. Agents bill through your own provider accounts, and a
  split with N agent branches runs N agent sessions. The job limits do not
  cap API spend.

Time
: The first command boots the shell VM. The first call to each Kit boots that
  Kit's VM, and marsh prints `[starting NAME worker VM…]`. Later calls reuse
  warm VMs. `marsh --load all` boots every Kit VM before your first prompt.
  `marsh join --timing` prints marsh's own overhead for a split.
  A Kit image that is not yet cached is downloaded on first use (about
  3.5 GB, a few minutes); marsh prints a notice before it starts.

CPU and memory
: Each job gets 4 CPUs and 8 GiB by default. At most 8 jobs run at once. These
  docs give no figure for the VMs themselves.

Disk
: A split puts its forks in `PROJECT/.marsh/split/ID/`, which Git ignores.
  `join` removes them. It keeps them if the `join` command failed, if you pass
  `--keep`, if a branch could not be confirmed stopped, or if nothing joined
  for 60 seconds. Remove a kept split with `marsh splits rm ID`. Packages you install in a VM are lost when the VM
  is removed.

Limits: [Commands and Kits](kits.md#limits-per-job) and
[Configuration](configuration.md#job-limits).

## What can go wrong

An agent can delete your project files.
: Every job can read and write the project. Agents started inside a `split`
  branch see only their fork. Outside `split`, nothing protects the project.
  Keep your work in Git.

Shell code in a split branch is not confined.
: A branch from `-b` or `split { }` runs your own code in the shell VM and can
  write anything you can write there. Only its fork's changes are captured.
  Agents it starts, like `claude`, run in containers that mount only the fork.

Every job can read the guest home.
: Agents keep logins and history there, so an agent in one Kit can read
  another agent's settings. For work that must stay apart, use a separate
  `MARSH_HOME`.

The agents' own sandboxes are off.
: The packaged Kits run Claude Code with `--dangerously-skip-permissions` and
  Codex with `--dangerously-bypass-approvals-and-sandbox`. Pi has none. The
  container and VM are the only sandbox.

Containers in one Kit VM share a kernel.
: The VM separates Kits from each other. The container does not.

marsh can fail to confirm that a container was removed.
: It does not re-run the job. It marks it `cleanup_uncertain` and quarantines
  that Kit VM until you run `marsh workers reset KIT`.

A job can be refused.
: At a limit (for example 8 jobs at once), the job starts nothing, prints a
  reason, and exits 125. [Troubleshooting](troubleshooting.md#a-job-was-refused)
  lists every limit.

The project is young.
: There has been no independent security review, and the binaries are not
  signed or notarized.

[Security model](security.md) has the rest.
[Troubleshooting](troubleshooting.md) lists error messages and fixes.

## Agents and logins

### Which agents work?

`claude` (Claude Code), `codex` (OpenAI Codex CLI), and `pi` (the Pi coding
agent) are registered. `shell` runs a plain `/bin/sh` with no credentials.
Any other tool works if you package it as a Docker Sandboxes Kit and register
it with `marsh kit install` or `commands.json`. See
[Commands and Kits](kits.md).

### How do agents authenticate?

marsh has no login. Run `sbx login` once, then give Docker Sandboxes a
credential with `sbx secret set anthropic` or `sbx secret set openai`. Its proxy
adds the credential to the agent's requests, and containers see placeholders.
See [Signing in](kits.md#signing-in).

### Can Mac clients use what I build in marsh?

Mac Codex can load published pipelines and ACP sessions with
`marsh mcp install-published` and `marsh acp install-published`. Those
commands support only `codex`. `marsh mcp install` also registers Claude Code
and Docker Sandboxes' gateway. See [MCP](mcp.md) and [ACP](acp.md).

### Does marsh record my sessions?

It records what ran, the exit status, cleanup, and timing (`marsh results`).
It does not record prompts, terminal content, or output. Arguments and paths
do appear in `marsh results` and `marsh jobs`, so keep secrets out of them.

## The shell

### Is it Bash?

No. It is [Brush](https://github.com/reubeno/brush), a Bash-compatible shell
written in Rust, plus 49 patches in
[`docs/upstream/brush/`](upstream/brush/README.md), most of them Bash
compatibility fixes. marsh never calls Bash to do its work. Known differences
are listed in [The shell](shell.md#bash-compatibility).

### How do `split` and `fanout` differ?

`split` gives every branch a private copy of the project and returns patches.
`fanout` runs branches at once on your real files, so branches can conflict.

Use `split` for competing attempts and `fanout` for independent tasks such as
tests and lint. See [Split and join](split.md) and
[Fanout and collect](fanout.md).

### Does it run on Linux or Intel Macs?

No. It needs an Apple Silicon Mac and Docker Sandboxes. Contributors can build
the shell and run its tests on Linux arm64.

### Do I need Docker Desktop?

Not to run a release. Building marsh from source, or running a Kit from a
source directory, needs Docker Desktop with Buildx.

### How do I remove everything?

Run `marsh stop`, then follow [Install](install.md#uninstall). `marsh stop`
refuses while shells or jobs are open.

### Why "marsh"?

From *marshal*. `msh` is the short name.

### How do I develop marsh?

See [CONTRIBUTING.md](../CONTRIBUTING.md). You can also develop marsh from
inside marsh with `marsh --dev`
([design/self-development.md](design/self-development.md)).

Next: [Quickstart](quickstart.md).
