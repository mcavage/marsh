# Agent sessions (ACP)

An ACP session keeps one agent running, so each prompt builds on the last.
`claude -p` and `codex exec` start fresh every time. A session remembers the
first answer when you send the second prompt.

```console
marsh-0.5$ id=$(acp reserve claude-session)
marsh-0.5$ acp run --reservation "$id" claude-session &
[1] 4127
acp: session 0b6f3c1e-5d2a-4e8b-9a77-1c3e5f7a9b02 ready; Kit job 7d1e0c4a-2b93-4f60-8a15-e4c9b3d27f08
marsh-0.5$ acp list --mine --wait "$id"
ready    claude-session     0b6f3c1e-5d2a-4e8b-9a77-1c3e5f7a9b02
marsh-0.5$ acp ask "$id" 'What does src/main.rs do?'
It parses the command line in parse_args() and hands off to run()…
marsh-0.5$ acp ask "$id" 'Add a --verbose flag to the function you just described.'
Added --verbose to parse_args() in src/main.rs and passed it to run()…
marsh-0.5$ acp stop "$id"
```

`…` marks trimmed output. The session is a background job in the shell,
running in the agent's Kit VM. `acp reserve` makes the ID first, so
`acp list --wait` finds your session and not another one.

To start one session with no reservation, run `acp run AGENT &`, then
`acp list --wait`. This works only when exactly one session is live in the
project. With several, `--wait` fails and asks for an exact ID.

## Login and credentials

The agent in a session authenticates the same way as the plain command, through
Docker Sandboxes ([Signing in](kits.md#signing-in)).

## What a session can change

The agent's edits land in your project files immediately. marsh does not fork
or review them, and the agent can delete files. To try changes on private
copies, start [split](split.md) from a job.

A session is one job, so the per-job limits apply, including the default 24 hours
of wall time ([Kits](kits.md)).

## Agents

A session starts from an agent name, not a Kit name.

| Agent | Kit |
|---|---|
| `claude-session` | `claude` |
| `codex-session` | `codex` |
| `pi-session` | `pi` |

To change the list, see [Adding agents](#adding-agents).

## Commands

| Command | What it does |
|---|---|
| `acp reserve AGENT` | Print a session ID. Start it within 15 seconds, or reserve again |
| `acp run [--reservation ID] AGENT &` | Start a session; prints its session and job IDs to stderr when ready |
| `acp list [--mine] [--wait [ID]] [--json]` | List this project's sessions. `--wait` blocks until one is ready, for at most four minutes |
| `acp ask ID TEXT` | Send a turn, print the answer. `-` as TEXT reads stdin. Limit: 1 MiB |
| `acp prompt [--key UUID] ID TEXT` | Send a turn without waiting. Retrying with the same key is safe |
| `acp status ID [CURSOR] [--json]` | State, recent updates, pending permissions |
| `acp permissions ID` | List requests waiting for a one-time approval |
| `acp respond ID REQUEST OPTION` | Answer one of them |
| `acp cancel ID` | Cancel the current turn; the session stays |
| `acp stop ID` | Stop the session and its job |
| `acp attach ID`, `acp release ID` | Take or give up control from this shell |

`acp --help` and `acp COMMAND --help` give details. A usage error exits 2.
Any other failure prints `acp: MESSAGE` on stderr and exits 125.

If an `acp ask` fails in a way that leaves the turn uncertain, the error
prints a retry key. Run `acp status ID` before sending the turn again.

`acp prompt` returns at once. Read the answer with `acp status ID`, which
prints the state and the latest turn:

```console
marsh-0.5$ acp status "$id"
claude-session  0b6f3c1e-5d2a-4e8b-9a77-1c3e5f7a9b02  ready
Kit job: 7d1e0c4a-2b93-4f60-8a15-e4c9b3d27f08
Last turn: end_turn
```

With `--json`, pass the returned `next_cursor` back as `CURSOR` to read only
newer updates.

When the agent asks for approval, `acp permissions ID` lists each request with
the `acp respond` command that answers it.

`ps --marsh` and `top --marsh` show live sessions next to jobs.

| Message | Meaning |
|---|---|
| `Multiple live ACP sessions; use acp list --wait ID …` | `--wait` without an ID found several live sessions. Pass the reserved ID |
| `ACP session ID is not visible in this project` | The ID is wrong or from another project |
| `ACP session failed or ended before becoming ready; inspect the background acp run job` | The agent did not start. Run `marsh jobs` and `marsh results` for the job |
| `no ACP session became ready within four minutes; …` | `--wait` timed out |

## Control

The shell that started a session controls it. Only that shell can send turns,
cancel, answer permissions, or stop the session. Other shells in the same
project and scope can read its status. `acp release` and `acp attach` hand
control over.

This is coordination, not a security boundary. Shells for different projects
in one scope (one `MARSH_HOME`) share a shell VM and can read each other's
files.

## What is recorded

marsh does not record prompts or answers. `marsh results` shows the session's
job (exit status, cleanup, timing), not its content. The agent keeps its own
history in the guest home.

## Sharing a session over MCP

Another agent can steer your session. An MCP client, such as Codex on the Mac
or an agent in another sandbox, can send turns, cancel, and answer one-time
permissions.

```sh
acp publish "$id" --name reviewer --sandbox my-sandbox   # or --kit codex
acp unpublish reviewer
```

`acp list` marks a published session with `published as reviewer`.

To use the published session from Codex on the Mac, register it from a host
terminal in the same project:

```sh
marsh acp install-published codex reviewer
marsh acp remove-published codex reviewer
```

`install-published` and `remove-published` support only `codex`.

Whoever can call the published tool, through the sandbox or Codex
registration, can steer the agent with this project's privileges. Unpublish
before you steer the session yourself. See [MCP](mcp.md) and
[Security](security.md).

## Adding agents

The agent list comes from `agents.json`. A file of that name in the host
control directory replaces the packaged list entirely. `[]` turns ACP off.

Each entry names a registered command and the extra arguments that put its Kit
in ACP mode:

```json
[{"schema_version":1,"name":"claude-session","protocol":"acp_v1","command":"claude","arguments":["--acp"]}]
```

[Configuration](configuration.md) says where the control directory is.

Next: [Agents running agents](agents.md), [MCP](mcp.md),
[Troubleshooting](troubleshooting.md).
