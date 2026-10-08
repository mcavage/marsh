# Agents running agents

Inside a marsh job, every registered command (`claude`, `codex`, `pi`, ...) is
on `PATH`. An agent that calls one starts a child job in its own container.
`marsh jobs --tree` shows both, and one Ctrl-C stops both.

From your marsh shell:

<!-- doc-test: shell replace=["claude -p \"use your Bash tool to run: codex exec 'reply with the number 7'\"", "fixture pipeline '|' bash -c \"shell -c 'echo 7'\""] stdout="└─ shell -c \"echo 7\"" -->
```sh
claude -p "use your Bash tool to run: codex exec 'reply with the number 7'"
marsh jobs --tree
```

```text
ID        STARTED   STATE      EXIT      RUN  COMMAND
301c56d6  2m ago    finished      0   2m12s  claude -p "use your Bash tool to run: codex exec 'reply with th…
8e41f0a2  1m ago    finished      0    14.2s  └─ codex exec "reply with the number 7"
```

The child job sees the same project files as its parent, not a copy. It runs
under its own Kit's network policy and credentials. Any agent can start any other, or itself, up
to the [limits](#limits-and-messages). To restrict which commands a job may
start, see [Narrowing what an agent may start](#narrowing-what-an-agent-may-start).

## Signing in

marsh has no login. Docker Sandboxes holds each agent's credential on your Mac
([Signing in](kits.md#signing-in)). A child job uses its own Kit's credential,
not its parent's.

## Split from inside an agent

An agent can split work across private forks of its files and read the results
back as patches. From a job, each branch is `::: LABEL CMD ARG...`. It runs as
a child job that sees only its fork of the project.

<!-- doc-test: shell replace=["marsh split ::: fix codex exec 'fix the typo in notes.txt' ::: review claude -p 'review notes.txt' | marsh join", "fixture pipeline '|' bash -c 'marsh split ::: fix fixture project-write notes.txt fixed ::: review fixture identity | marsh join'"] stdout="-- fix: 1 file (M notes.txt)" -->
```sh
marsh split ::: fix codex exec 'fix the typo in notes.txt' ::: review claude -p 'review notes.txt' | marsh join
```

`marsh join` prints one section per branch. `kit:NAME` is the Kit that ran the
branch:

```text
# split 8d5c8fdd1563: 2 branches; manifest …/out/manifest.json

== fix (exited 0, kit:codex) ==
Fixed the typo "recieve" in notes.txt.
-- fix: 1 file (M notes.txt); diff …/out/fix/diff.patch --
diff --git a/notes.txt b/notes.txt
…
== review (exited 0, kit:claude) ==
notes.txt reads correctly after the fix. No other issues.
-- review: no changes --
```

A split does not write to your project. You apply the patches you want. See
[Split and join](split.md).

Shell branches (`marsh split -b`, `split { a: ...; b: ... }`) run only from
your own shell. A job that asks for one is refused:

```text
split: a: shell branches (-b, split { }) run only from the session shell
```

## Parallel work on the same files

`marsh fanout` runs its branches at once on the same files, with no fork.
Writes are real and can race. It runs in the caller's environment: the job's
container, or the shell VM. A branch that names a registered command starts it
as a child job.

<!-- doc-test: shell replace=["claude -p \"run tests and lint at once with marsh fanout\"", "fixture pipeline '|' bash -c \"marsh fanout -n ::: tests echo ok ::: lint echo clean | marsh collect\""] stdout="== lint (complete) ==\nclean" -->
```sh
claude -p "run tests and lint at once with marsh fanout"
```

The agent would run:

```sh
marsh fanout ::: tests make test ::: lint make lint | marsh collect
```

`collect` prints each branch's output in the order written, and exits with the
first failing branch's status:

```text
== tests (complete) ==
ok

== lint (complete) ==
clean
```

From a job, branches must be `::: LABEL CMD ARG...`, as for `split`. In your
own shell, `marsh fanout` also takes shell branches (`-b LABEL=STRING`), and
you can write `fanout { a: CMD; b: CMD } | collect`. See
[Fanout and collect](fanout.md).

## Ctrl-C and cleanup

Ctrl-C in your shell cancels the foreground job's whole tree:

1. Every job gets `INT`.
2. Anything still running 10 seconds later gets `KILL`.
3. marsh checks that each container is deleted and records every job as
   `cancelled`.
4. Your command exits 130.

A job that ends cancels its own children the same way: `INT`, then `KILL`
after 10 seconds.

If marsh cannot confirm a job ended, `jobs --tree` shows it as
`cleanup uncertain`. `marsh workers reset KIT` removes that Kit's VM and frees
the slot ([Troubleshooting](troubleshooting.md#cleanup-uncertain-and-quarantine)).

## Narrowing what an agent may start

By default any job may start any registered command. To narrow that, set
`MARSH_SPAWN` in your shell to a comma-separated list of names, or `none`.

`marsh run --spawn a,b NAME` and `--no-spawn` do the same for one command.
Nothing inside a job can widen the list.

<!-- doc-test: shell replace=["claude -p \"use your Bash tool to run: shell -c true\"", "fixture pipeline '|' bash -c 'shell -c true'"] status=126 stderr="spawn refused: shell not in this job's spawn set (MARSH_SPAWN)" -->
```sh
export MARSH_SPAWN=codex,fixture
claude -p "use your Bash tool to run: shell -c true"
```

This limits which commands start. It does not limit what a permitted agent
does to your project files.

## Limits and messages

A refused call starts nothing, prints the reason on stderr, and exits 125.
A name outside `MARSH_SPAWN` exits 126. `marsh status` counts refusals
(`processes: N running, N refused, N held`).

| Limit | Message |
|---|---|
| 8 jobs at once per daemon, all depths | `capacity: 8 jobs` |
| Depth 4 (your command is depth 1) | `depth limit 4` |
| The same Kit at most twice in a row | `claude → claude → claude refused: same-Kit chain limit 2` |
| 4 live children per job | `fan-out limit 4` |
| 64 jobs per tree | `total limit 64` |
| A name outside `MARSH_SPAWN` | `spawn refused: NAME not in this job's spawn set (MARSH_SPAWN)` |
| No terminal for a child | `interactive child jobs are not supported yet` |

A child job gets no terminal. If its stdin or stdout is a terminal, marsh
refuses it. Run the command as `codex exec '...' </dev/null | cat`. Agents'
tool calls already run without a terminal.

After `capacity` or `fan-out limit`, wait for running jobs to finish and
retry. The other refusals do not clear by waiting. Change the plan: use a
different Kit, fewer levels, or fewer jobs in the tree.

## Reading the job tree

The `marsh jobs --tree` columns:

- `ID`: the short job id. Any prefix works with `marsh jobs show`.
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

`--all` lists every recorded job.

A split or fanout you ran is a node too. Its branches hang under it, and the
jobs each branch started hang under those
([split](split.md#seeing-a-split-in-marsh-jobs---tree)). A job started after
`join` ends with `(consumes split ID)`.

Related commands:

- `marsh jobs` shows the same jobs as a flat list with a `PARENT` column.
- `marsh jobs show JOB` adds the depth, the commands the job may start, and
  its children.
- `marsh results` has a `WALL` column. It covers the whole request, from the
  call to verified cleanup, including any Kit VM boot.

A command you start with `&` prints `[1] PID` as in Bash. If its Kit VM has to
boot first, the `[starting NAME worker VM…]` notice is not printed for it. A
foreground command prints it.

## How a child job starts

Every registered name (`claude`, `codex`, `shell`, ...) is in
`/run/marsh/bin`, on the job's `PATH`. Calling one, from any shell or program,
starts a child job: a fresh container in that Kit's VM, with this job's files,
under the child Kit's own network policy and credentials.

stdin, stdout, stderr, and the exit status pass through as for a local
command.

`bash`, `sh`, and every other tool in the job come from the image. The in-job
`marsh` command is also in `/run/marsh/bin`. It has `run`, `split`, `join`,
`fanout`, `collect`, `jobs`, and `context`.

## Variables

A variable you export in your shell reaches every job in the tree. So does a
variable a job exports or changes, for its children. Image `ENV` and
credential-shaped names (`*_TOKEN`, `*_SECRET`, `*_KEY`, ...) are never
forwarded. A job that needs a credential gets it through the Docker Sandboxes
proxy, not an exported variable.

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
- advice on when not to start more jobs.

`marsh context` prints it. Outside a job, it prints the shared text with no job
name or spawn set filled in.

The packaged agent Kits pass the file to their agent as extra instructions,
for that run only:

| Kit | CLI | ACP |
|---|---|---|
| `claude` | `--append-system-prompt "$(cat /run/marsh/context.md)"` | added to the Agent SDK's `--append-system-prompt` (or passed as one) by `claude-cli.sh` |
| `codex` | `-c developer_instructions="…"` | `developer_instructions` in codex-acp's `CODEX_CONFIG` (a caller's `CODEX_CONFIG` object is kept) |
| `pi` | `--append-system-prompt /run/marsh/context.md` | the same: pi-acp starts Pi through the Kit entrypoint |

Nothing is written into your project, your home, `CLAUDE.md`, `AGENTS.md`,
`~/.codex`, or `~/.pi`.

Exceptions:

- For a codex run, `-c developer_instructions` replaces any
  `developer_instructions` in your own `config.toml`.
- Management subcommands (`claude mcp`, `codex login`, ...) get nothing.
- Other Kits can read `/run/marsh/context.md` or run `marsh context`.

## Agent sandboxes and permission prompts

Inside a job, an agent can read and change everything the job can reach. That
includes your project files, which it can delete, and the guest home, which
every job in the scope can read. The container is the only sandbox.

The agents' own sandboxes cannot run in the container, because bubblewrap
cannot create namespaces there. The agent Kits turn those sandboxes off, along
with permission prompts:

- `claude` runs with `--dangerously-skip-permissions` and
  `--settings '{"sandbox":{"enabled":false}}'`. This stops a project's
  `sandbox.enabled`, meant for your Mac, from failing Bash calls in the job.
- `codex` runs with `--dangerously-bypass-approvals-and-sandbox` for `codex`
  and `codex exec`. That means approval `never` and sandbox
  `danger-full-access`. Its new `config.toml` says the same.
- Pi has no OS-level sandbox of its own. It runs with `--approve` so project
  resources load without a trust prompt.

ACP sessions (`acp run claude-session`, `codex-session`, `pi-session`) run the
same Kit in its ACP mode and keep these defaults:

- Claude's ACP adapter runs Claude Code with `sandbox.enabled` forced off. The
  setting is merged into any settings the adapter passes.
- Codex sessions start in codex-acp's full-access mode
  (`INITIAL_AGENT_MODE=agent-full-access`) unless you export another
  `INITIAL_AGENT_MODE`. The ACP client can still switch modes.

To keep an agent's edits off your files, run it in a
[split](split.md) and apply the patch yourself.

Next: [Split and join](split.md), [Fanout and collect](fanout.md),
[design: processes](design/processes.md).
