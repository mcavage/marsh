# Agent sessions (ACP)

An ACP session keeps an agent running across prompts. Use it when you want to
ask a follow-up without starting over. In your marsh shell, from a project
with a README that describes its tests:

```sh
id=$(acp reserve claude-session)
acp run --reservation "$id" claude-session &
acp list --mine --wait "$id"
acp ask "$id" 'Read the README and tell me how to run the tests. Do not change files.'
acp ask "$id" 'Check that command against the project configuration. What needs to be installed first? Do not change files.'
acp stop "$id"
```

`acp run` prints the session and Kit job IDs to stderr when the agent is
ready. Each `ask` prints the agent's answer. The session runs as a background
job in the agent's Kit VM. Reserving its ID before starting lets `list --wait`
wait for this session, even if another one is already ready.

To start one session with no reservation, run `acp run AGENT &`, then
`acp list --wait`. This works only when exactly one session is live in the
project. With more than one, `--wait` fails and asks for an exact ID. A
reservation is safer in scripts: an older ready session does not prove the
new one started.

## What a session can change

The agent edits your project files directly and can delete them. marsh does
not review those changes. The packaged agent Kits also turn off their own
sandboxes and permission prompts. To have an agent work on a private project
copy, start a [split](split.md) from a job.

The session stays in one Kit job for its lifetime, including the time between
prompts. Its [per-job limits](kits.md#limits-per-job) include a default 24-hour
wall-time limit.

### Login and credentials

Use the same Docker Sandboxes login as for the agent's plain command. See
[Signing in](kits.md#signing-in) to bind a provider credential or use an
agent's own login flow.

## Agents

A session starts from an agent name, not a Kit name.

| Agent | Registered command |
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
| `acp ask ID TEXT` | Send a turn and print the answer. `-` as TEXT reads stdin; size limits are below |
| `acp prompt [--key UUID] ID TEXT` | Send a turn without waiting. Reuse the same key and identical text after an uncertain submission |
| `acp status ID [CURSOR] [--json]` | State, recent updates, pending permissions |
| `acp permissions ID [--json]` | List requests waiting for a one-time approval |
| `acp respond ID REQUEST OPTION` | Answer one of them |
| `acp cancel ID` | Cancel the current turn; the session stays |
| `acp stop ID` | Stop the session and its job |
| `acp attach ID`, `acp release ID` | Take or give up control from this shell |

`acp --help` and `acp COMMAND --help` give details. A usage error exits 2.
Any other failure prints `acp: MESSAGE` on stderr and exits 125.

Prompt text must be UTF-8 and no larger than 1 MiB. Each message carrying it
must also fit 1 MiB after JSON encoding, including escaping and protocol fields,
so a prompt near the text limit may still be too large to send.

If an `acp ask` fails in a way that leaves the turn uncertain, the error
prints a retry key. Run `acp status ID` before sending the turn again.

`acp prompt` returns without waiting for the answer. While the session is
running, read progress with `acp status ID`; without a cursor, its text output
shows the latest turn:

```sh
acp status "$id"
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

The results journal does not store ACP prompts or answers. `marsh results`
shows the session's job, including its exit status, cleanup, and timing. Agent
software can save its own history in the shared guest home.

## Sharing a session over MCP

Start a session as above, but leave it running rather than calling `acp stop`.
Publish the idle session to let an MCP client send turns, cancel a turn, and
answer one-time permission requests. The client can be Codex on the Mac or an
agent in another sandbox.

```sh
acp publish "$id" --name reviewer --kit codex
```

`acp list` marks the session with `published as reviewer`. Start a new client
agent session after loading the tool. To load into an existing Docker sandbox
instead, use `--sandbox NAME` in place of `--kit codex`.

To use the published session from Codex on the Mac, register it from a host
terminal in the same project and with the same `MARSH_HOME`:

```sh
marsh acp install-published codex reviewer
marsh acp remove-published codex reviewer
```

`install-published` and `remove-published` support only `codex`.

Whoever can call the tool can steer the agent with its access to this project.
Run `acp unpublish reviewer` before steering the session yourself; removing a
Codex registration alone does not revoke other clients. See [MCP](mcp.md) and
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
