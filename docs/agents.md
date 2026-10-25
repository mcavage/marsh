# Agents running agents

An agent running in marsh can call registered commands to start child jobs.
Each child gets a fresh container, appears under its parent in
`marsh jobs --tree`, and stops when its parent ends. Ctrl-C cancels the whole
foreground job tree.

With [marsh installed](install.md) and both
[providers configured](kits.md#signing-in), try this from your marsh shell:

<!-- doc-test: shell replace=["claude -p \"use your Bash tool to run: codex exec 'reply with the number 7'\"", "fixture pipeline '|' bash -c \"shell -c 'echo 7'\""] stdout="└─ shell -c \"echo 7\"" -->
```sh
claude -p "use your Bash tool to run: codex exec 'reply with the number 7'"
marsh jobs --tree
```

If Claude makes the requested call, Codex appears as its child in the tree.
The child inherits its parent's project mounts and shared guest home, with
its own Kit's network policy and credentials. A [split](split.md) can narrow
its project mount to a fork. Any registered command is allowed by default,
including another invocation of the same agent, subject to the
[limits](#limits-and-messages) and [spawn set](#narrowing-what-an-agent-may-start).

## Signing in

Configure each child's provider through
[Docker Sandboxes](kits.md#signing-in), as you would for a direct call.

## Split from inside an agent

An agent can split work across private forks and read the results back as
patches. From a job, each branch must be `::: LABEL CMD ARG...`, naming a
registered command. The child mounts its fork at the fork's own host path;
the guest home is still shared.

<!-- doc-test: shell replace=["marsh split ::: fix codex exec 'fix the typo in notes.txt' ::: review claude -p 'review notes.txt' | marsh join", "fixture pipeline '|' bash -c 'marsh split ::: fix fixture project-write notes.txt fixed ::: review fixture identity | marsh join'"] stdout="-- fix: 1 file (M notes.txt)" -->
```sh
marsh split ::: fix codex exec 'fix the typo in notes.txt' ::: review claude -p 'review notes.txt' | marsh join
```

This assumes `notes.txt` exists. Both branches begin with the same snapshot;
the reviewer does not see the fixer's edits. `marsh join` prints one section
per branch, with `kit:NAME` identifying the Kit and a patch for any captured
changes. It never applies the patches. Use `marsh join --keep` if you want
to inspect or apply them after join exits. See [Split and join](split.md).

Shell branches (`marsh split -b`, `split { a: ...; b: ... }`) run only from
your own shell. A job that asks for one is refused:

```text
split: a: shell branches (-b, split { }) run only from the session shell
```

## Parallel work on the same files

Inside a job, `marsh fanout` runs commands concurrently in that job's
container, sharing its files. A registered command starts a child job;
other commands use the container's own tools. Writes can race.

<!-- doc-test: shell replace=["claude -p \"run tests and lint at once with marsh fanout\"", "fixture pipeline '|' bash -c \"marsh fanout -n ::: tests echo ok ::: lint echo clean | marsh collect\""] stdout="== lint (complete) ==\nclean" -->
```sh
claude -p "run tests and lint at once with marsh fanout"
```

The agent can run argv branches directly:

<!-- doc-test: shell replace=["marsh fanout ::: tests make test ::: lint make lint | marsh collect", "fixture pipeline '|' bash -c 'marsh fanout -n ::: tests printf tests-finished ::: lint printf lint-finished | marsh collect'"] stdout="== lint (complete) ==\nlint-finished" -->
```sh
marsh fanout ::: tests make test ::: lint make lint | marsh collect
```

`collect` prints each branch's output in the order written, and exits with the
first failing branch's status. This example needs the project's `test` and
`lint` Makefile targets.

From a job, branches must be `::: LABEL CMD ARG...`, as for `split`. The
[fanout reference](fanout.md) also covers shell branches and Brush blocks
for use from your own shell.

## Ctrl-C and cleanup

Ctrl-C in your shell cancels the foreground job's whole tree:

1. Every job gets `INT`.
2. Anything still running 10 seconds later gets `KILL`.
3. marsh checks each container's deletion and records the cancellation and
   cleanup result. A job that had already ended keeps its completed state.
4. Your command exits 130.

A job that ends cancels its own children the same way: `INT`, then `KILL`
after 10 seconds.

If marsh cannot confirm a job ended, `jobs --tree` shows it as
`cleanup uncertain`. `marsh workers reset KIT` removes that Kit's VM and frees
the slot ([Troubleshooting](troubleshooting.md#cleanup-uncertain-and-quarantine)).

## Narrowing what an agent may start

To restrict child commands, set
`MARSH_SPAWN` in your shell to a comma-separated list of names, or `none`.

`marsh run --spawn a,b NAME` and `--no-spawn` do the same for one command.
Nothing inside a job can widen the list.

<!-- doc-test: shell replace=["claude -p \"use your Bash tool to run: shell -c true\"", "fixture pipeline '|' bash -c 'shell -c true'"] status=126 stderr="spawn refused: shell not in this job's spawn set (MARSH_SPAWN)" -->
```sh
export MARSH_SPAWN=codex
claude -p "use your Bash tool to run: shell -c true"
```

This limits which commands start. It does not limit what a permitted agent
does to your project files.

## Limits and messages

A refused call starts nothing and prints the reason on stderr. A registered-name
link outside `MARSH_SPAWN` exits 126; other refusals, including those through
`marsh run`, exit 125. `marsh status` counts refusals
(`processes: N running, N refused, N held`).

| Limit | Message |
|---|---|
| 16 jobs at once per daemon, all depths | `capacity: 16 jobs` |
| Depth 4 (your command is depth 1) | `depth limit 4` |
| The same Kit at most twice in a row | `claude → claude → claude refused: same-Kit chain limit 2` |
| 8 live children per job | `fan-out limit 8` |
| 128 launches per tree, counting refused child calls | `total limit 128` |
| 4 distinct Kit VMs used by live jobs in one tree, by default | `tree already uses 4 Kit VMs (...) (MARSH_TREE_KIT_VMS)` |
| At least one second must remain before the parent's deadline | `deadline: parent job ... has ... ms of wall time left` |
| A name outside `MARSH_SPAWN` | `spawn refused: NAME not in this job's spawn set (MARSH_SPAWN)` |
| No terminal for a child | `interactive child jobs are not supported yet` |

A child job gets no terminal. If either stdin or stdout is a terminal, marsh
refuses it. Run the command as `codex exec '...' </dev/null | cat`.
With a pipe, enable `pipefail` to preserve the child's failure status.

After `capacity`, `fan-out limit`, or the Kit VM limit, let running jobs finish
before retrying. Repeated refusals consume the tree's launch budget. Depth,
same-Kit, and total limits need a different plan, such as fewer levels or
fewer child calls. The Kit VM limit is configurable through
[`MARSH_TREE_KIT_VMS`](configuration.md#job-limits); children share their
parent's deadline.

## Reading the job tree

The `marsh jobs --tree` columns:

- `ID`: the short job id. An unambiguous prefix works with `marsh jobs show`.
- `STATE`: `queued`, `running`, `finished`, `failed`, `cancelled`, or
  `unknown`.
- `RUN`: from the job's start in its warm Kit VM to its end. VM boot time is
  not included.
- `COMMAND`: the command and its arguments, cut at 64 characters.

Trees with a running job come first, then the newest. The jobs listed depend
on where you run the command:

| Where | Jobs listed |
|---|---|
| Your marsh shell | This session's jobs |
| A host terminal | Running trees, trees from the last hour, and the five newest |
| Inside a job | That job's own subtree |

From a shell or host terminal, `--all` lists every recorded job in the scope.
Inside a job, it still shows only that job's subtree.

A split or fanout also appears as a node, with its branch jobs below it
([split](split.md#seeing-a-split-in-marsh-jobs---tree)). A job started in a
stage downstream of `join` is marked `(consumes split ID)`.

Related commands:

- `marsh jobs` shows the same jobs as a flat list with a `PARENT` column.
- `marsh jobs show JOB` adds the depth, the commands the job may start, and
  its children.
- `marsh results` has a `WALL` column. It covers the whole request, from the
  call to verified cleanup, including any Kit VM boot.

## How a child job starts

Registered commands are links in `/run/marsh/bin`, first on the job's `PATH`.
Any process in the container can call one to request a child job. stdin,
stdout, stderr, and exit status pass through.

`bash`, `sh`, and every other tool in the job come from the image. The in-job
`marsh` command is also in `/run/marsh/bin`. It has `run`, `split`, `join`,
`fanout`, `collect`, `jobs`, `context`, `open`, and `cloud`
([Cloud from a job](cloud.md#cloud-from-a-job)). `xdg-open` and `open` are
there too (see [Opening links](#opening-links)).

## Variables

Ordinary exported variables pass to jobs and their children. A job can change
one for its own children. marsh filters unchanged image `ENV` defaults and
control variables. It also drops names containing `KEY`, `SECRET`, `PASS`,
`TOKEN`, `CREDENTIAL`, or `COOKIE`, plus `AWS_*` and `*_PAT`, ignoring case.
This includes ordinary names such as `KEYMAP`. Supply provider credentials through
[Docker Sandboxes](kits.md#signing-in).

### Your terminal

When you start marsh in a terminal, the shell and its jobs see which terminal
it is, so programs such as Pi, Claude Code, and Codex turn on the colors and
clickable links it supports. marsh copies these variables from your Mac, when
they are set:

`TERM`, `COLORTERM`, `TERM_PROGRAM`, `TERM_PROGRAM_VERSION`, `LC_TERMINAL`,
`LC_TERMINAL_VERSION`, `ITERM_SESSION_ID`, `KITTY_WINDOW_ID`, `WEZTERM_PANE`

Each value must be at most 128 letters, digits, or `. _ - : + @`; anything
else is left out. A `TERM` that Linux may not know, such as `xterm-ghostty`
or `xterm-kitty`, becomes `xterm-256color`; `xterm`, `xterm-256color`,
`screen*`, `tmux*`, `vt100`, `vt220`, `linux`, `ansi`, and `dumb` pass as they
are. With no `TERM`, the shell uses `xterm`. Nothing is copied when marsh runs
without a terminal (a pipe, ACP, or MCP).

### Opening links

`xdg-open URL`, `open URL`, and `marsh open URL` in the shell VM or a job open
a web link in your Mac's browser. `BROWSER` is set to `xdg-open`, so programs
that open a sign-in page or a report find it.

- Only `http://` and `https://` links open. A file, a path, or any other kind
  of link is refused: `marsh: can't open file:///tmp/x from a job; open it on
  your Mac`.
- The link opens only while your terminal is attached to the session, from
  the session's own shell or a job started from it (and that job's children).
  From a job started without a terminal (ACP, MCP, or a split or fanout
  branch), or once you detach, the link is printed instead. Cloud sandboxes
  have no marsh `xdg-open`.
- At most 3 links per session open in 10 seconds; more are printed.
- The link is always printed on stderr, as a clickable link when stderr is a
  terminal, so you can open it yourself.
- `xdg-open` exits 0 when the Mac opened the link, 4 when it only printed
  it, and 1 for a refused link.
- `marsh jobs show JOB --json` lists the sites (`scheme://host`) a job opened
  under `opened`.

To only print links, run `marsh config open-urls off` on your Mac, or start
the daemon with `MARSH_OPEN_URLS=0`.

<!-- doc-test: shell replace=["claude -p \"use your Bash tool to run: codex exec 'print the value of TICKET'\"", "fixture pipeline '|' bash -c \"shell -c 'echo TICKET=\\$TICKET'\""] stdout="TICKET=ABC-123" -->
```sh
export TICKET=ABC-123
claude -p "use your Bash tool to run: codex exec 'print the value of TICKET'"
```

## What agents are told

Every Kit job gets `/run/marsh/context.md`, written for that job. It contains:

- the job's name,
- the commands it may start,
- the limits,
- short examples of `codex exec`, `marsh split`, `marsh fanout`, and
  `marsh jobs --tree`,
- how to run a long split from a tool with a timeout, and what a child cannot
  read from the parent's image,
- advice on when not to start more jobs.

`marsh context` prints it. Outside a job, it prints the shared text with no job
name or spawn set filled in.

The packaged Kits pass this context as extra instructions in both CLI and ACP
sessions, for that run only. They do not write it into your project or home.

Exceptions:

- For a Codex run, marsh's context replaces any
  `developer_instructions` in your own `config.toml`.
- Management subcommands (`claude mcp`, `codex login`, ...) get nothing.
- Other Kits can read `/run/marsh/context.md` or run `marsh context`.

## Agent sandboxes and permission prompts

An agent can read, edit, or delete writable files mounted in its job,
including the project (or split fork) and shared guest home. Jobs start as
nonroot users without Docker or SBX sockets. Your shell VM user has
passwordless sudo and the VM's Docker Engine.

The packaged Kits disable the agents' own sandboxes and permission prompts.
Their namespace-based sandbox tools cannot run with the container's
permissions:

- `claude` runs with `--dangerously-skip-permissions` and
  `--settings '{"sandbox":{"enabled":false}}'`. This stops a project's
  `sandbox.enabled`, meant for your Mac, from failing Bash calls in the job.
- `codex` runs with `--dangerously-bypass-approvals-and-sandbox` for `codex`
  and `codex exec`. That means approval `never` and sandbox
  `danger-full-access`. The default `config.toml` the Kit writes sets the
  same options.
- Pi has no OS-level sandbox of its own. It runs with `--approve` so project
  resources load without a trust prompt.

ACP sessions (`acp run claude-session`, `codex-session`, `pi-session`) run the
same Kit in its ACP mode and keep these defaults:

- Claude's ACP adapter runs Claude Code with `sandbox.enabled` forced off. The
  setting is merged into any settings the adapter passes.
- Codex sessions start in codex-acp's full-access mode
  (`INITIAL_AGENT_MODE=agent-full-access`) unless you export another
  `INITIAL_AGENT_MODE`. The ACP client can still switch modes.

Run an agent in a [split](split.md) to review its project edits as a patch.
The [security model](security.md) covers shared files and additional grants.

Next: [Split and join](split.md), [Fanout and collect](fanout.md),
[design: processes](design/processes.md).
