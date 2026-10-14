# FAQ

For a first session, follow the [Quickstart](quickstart.md).

## Why not something else

### Why not run agents in Docker myself?

You can build that setup yourself. marsh handles a fresh nonroot container
for each registered command, reuses its Kit's VM, and checks container
deletion when the call ends. It also provides project copies and patches
with `split`, tracks nested agent calls in one tree, and cancels that tree
together.

Docker Sandboxes manages the VMs, network policies, and provider credential
proxy. No job receives a Docker socket or an SBX control socket. The mounted
project remains writable: outside a split, an agent can delete it. See
[What can go wrong](#what-can-go-wrong).

### Why not git worktrees?

Worktrees give you separate checkouts. A process in one still has your Mac
user's authority unless you arrange confinement yourself.

A registered agent in a marsh split mounts only its fork of the project,
plus the shared guest home and read-only Git metadata. The fork includes
uncommitted work, and copy-on-write cloning shares unchanged file data where
the filesystem supports it. `join` returns patches without merging them.
You inspect and apply the changes you want.

Split also works in a plain directory. For Git projects, it omits ignored
files and refuses roots with a `.git` file, including linked worktrees and
submodule checkouts. Use a checkout with a `.git` directory.

### Why not tmux?

tmux is useful when you want separate interactive terminals. It does not
capture a pane's project changes or track agent calls across containers.

In marsh, `split { ... } | join` starts branches on project copies and
prints each branch's output, exit status, and diff. `marsh jobs --tree`
shows nested jobs, and Ctrl-C stops the foreground tree. You can still use
tmux to organize your terminals.

### How is it different from plain `sbx`?

`sbx` creates VMs, applies their network policies, and manages proxy-held
credentials. marsh uses its public CLI without shipping a modified copy.
It adds the shell, registered agent commands, project branching, parallel
pipelines, and nested-job tracking. It can also publish pipelines as MCP
tools and expose ACP sessions to clients.

marsh requires Docker Sandboxes 0.45.0 or newer on an Apple Silicon Mac.

## What it costs

### Who pays for the agents?

You use your own provider accounts. Each branch or child starts a separate
agent run, which may make many model requests. marsh is Apache-2.0 software;
its job limits do not impose a spending cap.

### Why does the first call take longer?

The first shell launch starts its VM. First use of a Kit starts another VM
and may download an image of about 600 MB, which can take a few minutes.
Later calls reuse warm VMs. `marsh --load all` prepares Kit VMs before the
prompt, and `marsh join --timing` reports split overhead.

### How much CPU and memory does a job get?

Each job defaults to four CPUs and 8 GiB of memory, with at most eight
concurrent jobs per daemon. The VMs also need resources beyond those job
limits. See [Job limits](configuration.md#job-limits).

### Where do split copies go, and when are they removed?

Copies and results live under `PROJECT/.marsh/split/ID/`. A successful join
normally removes them; `--keep`, a failed join, uncertain cleanup, or a split
left unjoined for 60 seconds preserves them. Inspect them with `marsh splits` and remove a
finished one with `marsh splits rm ID`. VM removal discards packages you
installed inside that VM, but not the saved guest home.

## What can go wrong

### Can an agent delete my files?

Yes. A direct call has read-write access to the original project. A split
agent has that access to its fork instead. Keep recoverable copies of your
work and inspect patches before applying them.

### Are my tests confined to a split copy?

No. Your own shell code runs unconfined in the shell VM. A branch created
with `-b` or `split { }` starts in a fork but can write elsewhere in that VM,
including other mounted project paths. Registered agents it starts get the
fork-only project mount. That does not confine a test command you run after
the agent.

### Can one job read another agent's saved login?

Yes. Jobs share a guest home containing settings, history, and saved logins.
One job can read files another agent saved there. Use a separate
`MARSH_HOME` for work that must stay apart; a project fork is not a private
home.

### Will agents ask before running tools?

The packaged agents do not. Claude Code runs with
`--dangerously-skip-permissions`; Codex uses
`--dangerously-bypass-approvals-and-sandbox`. Their own internal sandboxes
are disabled, and Pi has no OS-level sandbox. The job container and VM
provide the isolation. Containers within one Kit VM share a kernel.

### What if marsh loses contact with a running job?

If marsh cannot confirm that a container ended and was removed, it records
`cleanup_uncertain` and quarantines the Kit VM. It does not replay the job.
Use `marsh workers reset KIT` to retire the VM before using that Kit again.

### Why was a job refused?

A launch would have exceeded a limit or used a command outside its allowed
spawn set. At a limit, marsh starts nothing, prints the reason, and exits
125. A command outside the spawn set exits 126.
See [A job was refused](troubleshooting.md#a-job-was-refused).

### Has marsh had a security review?

Not yet. Release binaries are also unsigned and not notarized. The
[security model](security.md) describes the boundaries in detail;
[Troubleshooting](troubleshooting.md) covers
failure recovery.

## Agents and logins

### Which agents work?

The packaged commands are `claude` (Claude Code), `codex` (OpenAI Codex
CLI), `pi` (the Pi coding agent), and `shell` (plain `/bin/sh`, without
provider credentials). They are Linux CLI versions supplied by marsh's
Kits, not the installations on your Mac.

You can package another tool as a Docker Sandboxes Kit and register it with
`marsh kit install` or `commands.json`. See [Commands and Kits](kits.md).

### How do agents authenticate?

Run `sbx login` on your Mac, then configure the provider before calling an
agent: `sbx secret set anthropic` for Claude, or `sbx secret set openai` for
Codex. Pi can use Anthropic models with the Anthropic credential. Docker
Sandboxes also supports OAuth bindings for the agent Kits; see
[Signing in](kits.md#signing-in).

For proxy-managed credentials, the real provider key stays on the Mac and
containers receive placeholders. Saved guest login files are ordinary files
in the shared guest home and can be read by other jobs. A child uses its own
Kit's credential and network policy, not its parent's.

### Can Mac clients use what I build in marsh?

Mac Codex can load published pipelines with `marsh mcp install-published`
and ACP sessions with `marsh acp install-published`. Those two commands
support only `codex`. `marsh mcp install` also supports Claude Code and the
Docker Sandboxes gateway. See [MCP](mcp.md) and [ACP](acp.md).

### Does marsh record my sessions?

`marsh results` records job status, cleanup, and timing, not a terminal
transcript. It does not retain stdin, stdout, or stderr. Command arguments
and paths do appear in job records, so a prompt passed as an argument can
appear there too. Keep secrets out of arguments. Split results separately
retain branch output and patches while the split exists.

## The shell

### Is it Bash?

No. The default shell is [Brush](https://github.com/reubeno/brush), a
Bash-compatible shell written in Rust, with marsh's
[patch queue](upstream/brush/README.md). marsh does not use Bash to implement
its default shell. Known gaps are in
[Bash compatibility](shell.md#bash-compatibility).

You can select the guest's own Bash or zsh with `marsh --shell bash` or
`marsh --shell zsh`. Registered agent commands still work. For split and
fanout, use their CLI forms rather than Brush's block syntax. See
[Using bash or zsh instead](shells.md).

### How do `split` and `fanout` differ?

`split` gives each branch a project copy and returns a patch. Use it for
independent edits, alternative implementations, or reviewers whose
unexpected writes should stay off the original project.

`fanout` runs branches on shared files. It suits checks that can safely
share a working directory, but parallel writers can collide. A request not
to edit does not make an agent read-only. See [Split and join](split.md)
and [Fanout and collect](fanout.md).

### Does it run on Linux or Intel Macs?

No. The host product requires Apple Silicon and Docker Sandboxes. Its
guest commands run on Linux arm64. Contributors can build the shell and run
its tests on Linux arm64, which is different from running the Mac product
there.

### Do I need Docker Desktop?

Not for a release install. Building marsh from source or running a Kit from
a source directory requires Docker Desktop with Buildx.

### How do I remove everything?

Finish jobs, run `exit` in every marsh shell, then run `marsh stop` in your
Mac terminal. Stop refuses while shells or jobs remain active. Follow
[Uninstall](install.md#uninstall) to remove the program and, if wanted,
its saved data.

### Why "marsh"?

From *marshal*. `msh` is the short command name.

### How do I develop marsh?

[CONTRIBUTING.md](../CONTRIBUTING.md) describes the build and test loop.
You can also work from inside marsh with `marsh --dev`; see
[Self-development](design/self-development.md).

Next: [Quickstart](quickstart.md) or [Guides](guides/README.md).
