# MCP

`mcp publish` gives other agents a tool that runs a pipeline you choose.
For a project with a `make lint` target, run this in the marsh shell:

<!-- doc-test: shell stdout="Published MCP tool: lint" -->
```sh
mcp publish lint --description 'Run the linter' -- 'make lint 2>&1'
```

```text
Published MCP tool: lint
Server: SERVER-NAME
Available to agent Kits: every Kit VM created from now on loads it before its first job.
Kit VMs already running are not changed. Load one now with: mcp load lint --kit KIT, or recreate it with: marsh workers reset KIT
```

`Server:` gives this project's registration name. When an agent calls `lint`,
marsh runs `make lint 2>&1` in the project's shell VM and returns the output
and exit status. Newly created agent Kit VMs load the tool before their first
job; start a new agent session there to use it.

To load it into a running Kit VM now:

```sh
mcp load lint --kit pi
```

You can also publish a running [ACP session](acp.md) as a tool, or register
marsh itself as a server so an agent on the Mac can drive the project.

## Publish a pipeline

`mcp publish` runs inside the marsh shell. The pipeline text is fixed by the
publisher. A caller may pass an `input` string, which becomes the pipeline's
stdin, and receives the output and exit status. The pipeline runs through
Brush in this project's shell VM, with the shell's privileges. Published
pipelines accept up to 64 KiB of input, time out after two minutes, and
capture up to 256 KiB each of stdout and stderr.

Tool names are 1 to 64 ASCII letters, digits, `_`, `-`, or `.`. A name cannot
start with `-` or consist only of dots.

| Command | What it does |
|---|---|
| `mcp publish NAME [--description TEXT] -- 'PIPELINE'` | Publish for every agent Kit VM created from now on |
| `mcp publish NAME [--description TEXT] --kit KIT \| --sandbox SANDBOX -- 'PIPELINE'` | Publish and load into one target now |
| `mcp load NAME --kit KIT \| --sandbox SANDBOX` | Load an existing publication somewhere else, now |
| `mcp unpublish NAME` | Revoke it; later calls are refused |

Choose a target to load the tool immediately:

- `--kit KIT` loads into that Kit's VM. Every later job of that Kit can use
  the tool.
- `--sandbox SANDBOX` loads into a running Docker sandbox owned by your Mac
  account.

After loading, start a new agent session. An existing session may keep its
previous tool list.

From a Mac terminal in the same project and with the same `MARSH_HOME`, you
can also load or unpublish:

```sh
marsh mcp load NAME --sandbox SANDBOX
marsh mcp unpublish NAME
```

### Publish without a target

With neither `--kit` nor `--sandbox`, every agent Kit VM created from now on
loads the tool before its first job. That covers every registered Kit
(`claude`, `codex`, `pi`, and any you add) on first use, after
`marsh workers reset KIT`, and whenever a Kit VM is recreated.

A Kit VM that is already running is never loaded automatically, even when it
is idle. Docker Sandboxes' MCP gateway is per VM, so loading would change the
tool list seen by every job already in that VM. For a running VM, do one of
these:

```sh
mcp load lint --kit codex        # load into the running codex VM now
marsh workers reset codex        # or: the next codex job gets a new VM with it
```

Not loaded automatically:

- Other Docker sandboxes and the shell VM. Use `--sandbox` for those.
- Kit VMs of sessions started with `--ephemeral-home`. Those sessions get
  their own Kit VMs, and those VMs do not load defaults.

#### Changing or removing a default

- Publishing the same name again with `--kit` or `--sandbox` makes it a
  targeted publication. New Kit VMs stop loading it.
- `mcp unpublish NAME` stops new Kit VMs loading it. It also revokes the tool
  everywhere it was loaded.
- If loading a default into a new Kit VM fails, the VM still starts without
  that tool. The reason is in the daemon log, and `mcp load NAME --kit KIT`
  retries.

Revocation refuses later calls; a client may still display the stale tool.
Publishing the same name again replaces its grant. Existing clients must
reload it before they can call the replacement.

## Use a pipeline from Codex on the Mac

Codex on the Mac can call a published pipeline. First publish it in the marsh
shell:

```sh
mcp publish lint --description 'Run the linter' -- 'make lint 2>&1'
```

Then, from a Mac terminal in the same project and with the same `MARSH_HOME`:

```sh
marsh mcp install-published codex lint
codex exec 'run lint and fix what it finds'
```

New Codex tasks see the tool, and calls run the pipeline in the marsh shell.
Because the publish has no target, agent Kit VMs created from now on load it
too.

Only `codex` is supported by `install-published` and `remove-published`. For
Claude Code on the Mac, `marsh mcp install claude` (below) registers marsh
itself as a server, which is a different feature. Other Mac clients can launch
the stdio server.

To undo:

```sh
marsh mcp remove-published codex lint   # remove the Codex registration
marsh mcp unpublish lint                # revoke the tool everywhere
```

## Publish an agent session

`acp publish ID --name NAME` publishes a running ACP session as a tool other
agents can prompt:

```sh
acp publish "$id" --name reviewer --kit codex
```

See [ACP](acp.md#sharing-a-session-over-mcp) for the full flow, including
reserving and running the session.

## marsh as an MCP server

An MCP client can drive a marsh project: run shell commands, read job results,
and prepare or reset Kit VMs. Run one of these from the project directory:

```sh
marsh mcp install codex      # Codex on the Mac
marsh mcp install claude     # Claude Code on the Mac
marsh mcp install sbx        # Docker Sandboxes' MCP gateway
```

Each registers a server named `marsh-dev-HASH` for this directory, using a
separate MCP scope root. Codex and Claude Code start it when they need it.

For `sbx`, marsh starts a small broker on the Mac and registers it with the
Docker Sandboxes MCP gateway. Load it into a sandbox:

```sh
sbx mcp load marsh-dev-HASH --sandbox NAME
```

| Command | When to use it |
|---|---|
| `marsh mcp start` | Restart the broker after a reboot |
| `marsh mcp stop` | Stop the broker when no sandbox is attached |
| `marsh mcp install ...` | Rerun after upgrading `sbx` |

### Tools

The server has tools to:

- run a command in this project's marsh shell (`shell_run`) and read its
  output (`operation_get`, `operation_output`, `operation_cancel`);
- read status and finished jobs (`status`, `results_list`, `result_get`);
- prepare or reset Kit VMs (`prewarm`, `workers_reset`);
- create and manage separate scopes for parallel work (`scope_start`,
  `scope_run`, `scope_stop`, and others). Each scope has its own home, shell
  VM, and Kit VMs.

[design/mcp.md](design/mcp.md) lists every tool and its limits.

Shell commands run in the shell VM. VM lifecycle operations run through marsh
on the Mac; clients get no generic host-command tool or control socket. An
optional `--allow-full-sbx-control` flag enables the `qualify` tool to run
fixed repository test gates on the Mac. Enable that only for a trusted
checkout: those gates execute its Makefile.

### Other MCP clients

Other clients can launch the server directly over stdio:

```sh
marsh mcp serve --workspace "$PWD" --marsh "$(command -v marsh)" --sbx "$(realpath "$(command -v sbx)")"
```

This direct `serve` command uses the CLI's scope (`MARSH_HOME`, else
`~/.marsh`) and shares its shell and Kit VMs. It does not allow `scope_start`.
Pass `--scope-root ABSOLUTE_PATH` to enable separate managed scopes, as the
`install` commands do. `marsh` and `sbx` must be installed outside the project.

## What a caller can do

- A client of the marsh server can do anything the marsh shell can do in this
  project, including `sudo` in the shell VM and starting agents.
- Scopes share the same project files, so two scopes can race on one file.
- A caller of a published pipeline cannot change the pipeline text. It can
  choose the `input`, so a pipeline that executes its stdin gives the caller
  that power. Project scripts the pipeline calls can also change after
  publication.
- Anyone who can reach a registered tool can call it with this project's
  privileges. Give a tool only to agents you would let run that pipeline.

Publishing registers a tool with the host broker; calling it runs the pipeline
with the publishing shell's privileges. Loading a tool into a Kit VM makes
that operation available to jobs there, without giving them a general
host-command channel. Other MCP servers you load may offer different access;
check their tools before sharing them with a job.

See [Security](security.md).

## MCP servers inside Kit jobs

Docker Sandboxes can run an MCP gateway inside each Kit VM, and the packaged
agent Kits connect to it. Every job in that Kit's VM then sees servers loaded
by any of these:

- `sbx mcp load`
- `mcp publish --kit` or `mcp load --kit`
- an untargeted `mcp publish`, when the VM is created

Next: [ACP](acp.md), [Security](security.md), [design/mcp.md](design/mcp.md).
