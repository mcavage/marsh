# Configuration

Raise a job's memory limit, or give a project its own state:

```sh
marsh stop
MARSH_JOB_MEMORY_BYTES=17179869184 marsh     # 16 GiB per job
```

```sh
MARSH_HOME=~/.marsh-work marsh               # a separate scope
```

`marsh status` shows the limits in effect. It prints a block that starts
`Per-job defaults (daemon first-launch snapshot):`, then one line per limit,
each ending in `[built-in default]` or
`[daemon launch environment: VARIABLE]`.

The rest of this page is lookup: variables, files, and where marsh keeps state.

## Scopes

A *scope* is one `MARSH_HOME`. It has its own daemon, shell VM, Kit VMs, guest
home, and record of finished jobs. The default scope is `~/.marsh`. Everything
run with the same `MARSH_HOME` shares that state.

Use separate scopes when work must not share a VM or guest home, for example
a client project and your personal agent logins. Scopes do not separate your
project files: a job in any scope can write the project it runs in.

## Environment

| Variable | Meaning |
|---|---|
| `MARSH_HOME` | Absolute scope root. Default `~/.marsh`. The guest home is `$MARSH_HOME/home`. |
| `MARSH_SHELL` | Shell to run: `marsh` (Brush, the default), `bash`, or `zsh`. See [Choosing a shell](shells.md). |
| `MARSH_SBX` | Path to `sbx`. Default: `sbx` on `PATH`. |
| `MARSH_CONTROL_HOME` | Host-only state root. See [Control home](#control-home). |
| `MARSH_SPAWN` | Names that jobs started from this shell may start. A comma-separated list, or `none`. It can only narrow. A refused name exits 126 ([Agents running agents](agents.md#narrowing-what-an-agent-may-start)). |
| `MARSH_PLACE` | Where registered commands run. Only `local` is accepted; any other value is an error (`MARSH_PLACE must be local`). |
| `MARSH_BANNER` | Set to `1` inside the shell VM to show the stock shell's banner. See [Banner](#banner). |
| `MARSH_KIT_IMAGE_CACHE` | Cache of Kit images built from source. Default `~/Library/Caches/marsh/kit-images`. |

### Job limits

The daemon reads these once, when it starts for a scope. They apply to every
job in the scope. Values are positive integers.

| Variable | Default |
|---|---|
| `MARSH_JOB_CPU_MILLIS` | 4000 (4 CPUs) |
| `MARSH_JOB_MEMORY_BYTES` | 8589934592 (8 GiB) |
| `MARSH_JOB_PIDS` | 4096 |
| `MARSH_JOB_WRITABLE_BYTES` | 10737418240 (10 GiB). Counts the container's own files, not mounted directories. |
| `MARSH_JOB_OUTPUT_BYTES` | 268435456 (256 MiB). stdout and stderr together. |
| `MARSH_JOB_WALL_SECONDS` | 86400 (24 hours) |
| `MARSH_TREE_KIT_VMS` | 4. Distinct Kit VMs one job tree may use, 1 to 64. |

To see the values in effect, run `marsh status` (or `marsh status --json` and
read `job_defaults`).

To change them:

1. Let running work finish.
2. Run `marsh stop`.
3. Start marsh with the new values, as in the example at the top.

A shell started with a value that conflicts with the running daemon is
refused. `marsh status` shows the daemon's values.

#### Writable limit

The writable limit is not preallocated. marsh measures a running job's
writable layer about every 20 ms. It kills the job (cause `limit:writable`)
once the layer is over the limit. A job that writes very fast can exceed the
limit by whatever it writes in that interval.

#### Fixed limits

These cannot be changed. Each refusal prints a message and exits 125
([messages](agents.md#limits-and-messages)).

- 8 jobs at once per daemon
- depth 4
- 4 live children per job
- the same Kit at most twice in a row
- 64 jobs per tree

### Control home

`MARSH_CONTROL_HOME` must be an existing directory with mode 0700. Under it:

- scope control directories (`HASH/`);
- MCP and ACP publications: `published-mcp/`, `published-acp/`, and
  `publication-locks/`.

Generated MCP server scopes go beside it, in
`$MARSH_CONTROL_HOME-mcp-scopes/`.

When unset, control directories are in
`~/Library/Application Support/marsh/control`. Publications are in
`~/Library/Application Support/marsh`.

### Banner

The stock shell template's ASCII banner is suppressed. To show it, set
`MARSH_BANNER=1` in the VM's environment, for example:

```sh
MARSH_BANNER=1 bash -l
```

## Files

| Path | What |
|---|---|
| `$MARSH_HOME/config.json` | The default shell, set by `marsh config shell NAME`. Default path `~/.marsh/config.json`. |
| `$MARSH_HOME/home/` | The guest home. Agent logins and settings (`.claude`, `.codex`) live here. Every job in the scope can read it. Default path `~/.marsh/home/`. |
| `CONTROL/HASH/` | Host-only control directory for a scope. `marsh status` prints it. Never mounted into a VM. |
| `CONTROL/HASH/commands.json` | Your command registry. Overrides packaged entries by name. See [Commands and Kits](kits.md#adding-a-command). |
| `CONTROL/HASH/agents.json` | Your ACP agent list. Replaces the packaged list; `[]` disables ACP. See [ACP](acp.md#adding-agents). |
| `CONTROL/HASH/state/results.journal` | Finished jobs, for `marsh results`. |
| `PREFIX/libexec/marsh/` | Packaged `commands.json`, `agents.json`, `shell-image`, and the Linux binaries. |
| `~/Library/Application Support/marsh/published-mcp/`, `published-acp/` | Host-only MCP and ACP publication declarations. Under `MARSH_CONTROL_HOME` when it is set. |
| `~/Library/Application Support/marsh-mcp-scopes/` | Generated scope roots for `marsh mcp install sbx` and `marsh-mcp` broker modes. |
| `PROJECT/.marsh/split/ID/` | A split's forks and results. Ignored by Git. |

`CONTROL` is `$MARSH_CONTROL_HOME`, or `~/Library/Application Support/marsh/control`
when unset. `HASH` is the SHA-256 of the canonical `MARSH_HOME` path.

A malformed `commands.json` or `agents.json` stops the daemon from starting.
The error names the file. Fix that file; deleting other state does not help.

## Shell prompt and startup files

The shell reads the usual startup files from the guest home. Set `PS1` there
to change the prompt. See [The shell](shell.md#startup-files).

## Results

`marsh results` lists finished jobs: the command and where it ran, its exit
status, whether its container's deletion was verified, and timing. It does not
store prompts or output. The newest 1,000 entries are kept.

Next: [Commands and Kits](kits.md), [Troubleshooting](troubleshooting.md).
