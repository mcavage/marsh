# Configuration

marsh keeps configuration and runtime state per `MARSH_HOME`. Use this page
to change job limits, choose a shell, or find the files used by your
[installation](install.md).

For a separate guest home and set of VMs, launch with
`MARSH_HOME=~/.marsh-work marsh`. To raise the per-job memory limit to 16 GiB,
finish active work, exit open shells, run `marsh stop`, then launch with
`MARSH_JOB_MEMORY_BYTES=17179869184 marsh`.

`marsh status` shows the limits in effect and whether each came from a built-in
default or the daemon's launch environment.

## Scopes

A *scope* is one canonical `MARSH_HOME` path. It has its own daemon, VMs,
guest home, and job records. The default scope is `~/.marsh`.
Persistent-home shells in that scope share the shell VM; an
[ephemeral-home shell](shell.md#ephemeral-home) gets a separate VM and home.

Use separate scopes when work must not share a VM or guest home, for example
a client project and your personal agent logins. Scopes do not separate your
project files: a job in any scope can write the project it runs in.

For skills, dotfiles, MCP connections, and extra directories, see
[Set up your marsh home](guides/set-up-your-home.md).

## Environment

| Variable | Meaning |
|---|---|
| `MARSH_HOME` | Absolute scope root. Default `~/.marsh`. The guest home is `$MARSH_HOME/home`. |
| `MARSH_SHELL` | Shell to run: `marsh` (Brush, the default), `bash`, or `zsh`. See [Choosing your shell](shells.md). |
| `MARSH_SBX` | Path to `sbx`. Default: `sbx` on `PATH`. |
| `MARSH_CONTROL_HOME` | Host-only state root. See [Control home](#control-home). |
| `MARSH_SPAWN` | Names that jobs started from this shell may start. A comma-separated list, or `none`. See [spawn restrictions and refusal statuses](agents.md#limits-and-messages). |
| `MARSH_PLACE` | Where registered commands run. Only `local` is accepted; any other value is an error (`MARSH_PLACE must be local`). |
| `MARSH_KIT_IMAGE_CACHE` | Cache of Kit images built from source. Default `~/Library/Caches/marsh/kit-images`. |
| `MARSH_CLOUD_CPUS` | Default `--cpus` for [`cloud`](cloud.md#sandbox-size-lifetime-and-image) calls from this shell: 1, 2, 4, 8, or 16 (2 GiB of memory per CPU). Default 2. |
| `MARSH_CLOUD_TTL` | Default `--ttl` for `cloud` calls: 1m to 24h, such as `30m` or `2h`. Default `4h`. |
| `MARSH_CLOUD_IMAGE` | Default `--image` for `cloud` calls: an image pinned by digest. Read by the daemon too, as its own default. |
| `MARSH_AUTO_UPDATE` | `0` (or `off`) turns off [automatic Kit updates](kits.md#updates), and the automatic adding of `pix` and `hermes`, for the daemon it starts, as `marsh config auto-update off` does. Read when the daemon starts. |
| `MARSH_CLOUD_CHILDREN` | The most agent sandboxes one [detached cloud run](cloud.md#agents-calling-agents-with-the-lid-closed) may start with `marsh config cloud-delegate on`: 1 to 64. Default 8. Read when the daemon starts. |
| `MARSH_OPEN_URLS` | `0` (or `off`) stops [`xdg-open` in a shell or job](agents.md#opening-links) from opening links on your Mac, as `marsh config open-urls off` does; links are printed instead. Read when the daemon starts. |
| `MARSH_OPEN_COMMAND` | The program the daemon runs as `PROGRAM URL` to open a link. Default `/usr/bin/open`. Read when the daemon starts; tests point it at a recorder. |

### Job limits

The daemon reads these once, when it starts for a scope. They apply to every
job in the scope. Values are positive integers.

| Variable | Default |
|---|---|
| `MARSH_JOB_CPU_MILLIS` | 2000 (2 CPUs) |
| `MARSH_JOB_MEMORY_BYTES` | 4294967296 (4 GiB) |
| `MARSH_JOB_PIDS` | 4096 |
| `MARSH_JOB_WRITABLE_BYTES` | 10737418240 (10 GiB). Counts the container's own files, not mounted directories. |
| `MARSH_JOB_OUTPUT_BYTES` | 268435456 (256 MiB). stdout and stderr together. |
| `MARSH_JOB_WALL_SECONDS` | 86400 (24 hours) |
| `MARSH_TREE_KIT_VMS` | 4. Distinct Kit VMs used by live jobs in one tree, 1 to 64. Aliases of the same Kit share a VM. |

To see the values in effect, run `marsh status` (or `marsh status --json` and
read `job_defaults`).

To change them:

1. Let running work finish and exit open shells.
2. Run `marsh stop`.
3. Start marsh with the new values, as in the example at the top.

A shell started with a value that conflicts with the running daemon is
refused. `marsh status` shows the daemon's values.

#### Writable limit

marsh periodically measures the container's writable layer and kills a job
that exceeds the limit (`limit:writable`). A fast writer can overshoot between
checks. Files in mounted directories do not count toward this limit.

#### Fixed limits

These cannot be changed. A call that exceeds one is refused with exit 125
([messages](agents.md#limits-and-messages)).

- 16 jobs at once per daemon
- depth 4
- 8 live children per job
- the same Kit at most twice in a row
- 128 launches per tree, including refused child calls

Children also inherit their parent's wall-time deadline. With less than one
second remaining, a new child is refused. Parallel composition has separate
[fanout](fanout.md#limits) and [split capture](split.md#what-is-captured)
limits.

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

## Files

| Path | Contents |
|---|---|
| `$MARSH_HOME/config.json` | The default shell, set by `marsh config shell NAME`, whether Kits update themselves, set by `marsh config auto-update on\|off`, whether `xdg-open` opens links on your Mac, set by `marsh config open-urls on\|off`, and whether detached cloud runs may start agent sandboxes, set by `marsh config cloud-delegate on\|off` (default off). Default path `~/.marsh/config.json`. |
| `$MARSH_HOME/home/` | The guest home. Agent logins and settings (`.claude`, `.codex`) live here. Every job in the scope can read it. Default path `~/.marsh/home/`. |
| `$MARSH_HOME/sbxenv.yaml` | Shared MCP servers and extra directories for every Kit and the shell. |
| `$MARSH_HOME/sbxenv.d/*.yaml` | More shared servers and directories, same schema as `sbxenv.yaml`, read in name order after it. |
| `$MARSH_HOME/sbxenvs/NAME.yaml` | Additional servers and directories for registered command `NAME`; `shell.yaml` also configures the shell VM. |
| `CONTROL/HASH/` | Host-only control directory for a scope. `marsh status` prints it. Never mounted into a VM. |
| `CONTROL/HASH/commands.json` | Your command registry. Overrides packaged entries by name. See [Commands and Kits](kits.md#adding-a-command). |
| `CONTROL/HASH/kit-updates.json` | When [automatic Kit updates](kits.md#updates) last checked, and what they changed. |
| `CONTROL/HASH/state/results.journal` | Structural job receipts, for `marsh results`. |
| `PREFIX/libexec/marsh/` | Packaged `commands.json`, `shell-image`, and the Linux binaries. |
| `~/Library/Application Support/marsh/published-mcp/`, `published-acp/` | Host-only MCP and ACP publication declarations. Under `MARSH_CONTROL_HOME` when it is set. |
| `~/Library/Application Support/marsh-mcp-scopes/` | Generated scope roots for `marsh mcp install sbx` and `marsh-mcp` broker modes. |
| `PROJECT/.marsh/split/ID/` | A split's forks and results. Ignored by Git. |

`CONTROL` is `$MARSH_CONTROL_HOME`, or `~/Library/Application Support/marsh/control`
when unset. `HASH` is the SHA-256 of the canonical `MARSH_HOME` path.

A malformed `commands.json` stops the daemon from starting.
The error names the file. Fix that file; deleting other state does not help.

## Servers and extra directories

The optional `sbxenv.yaml`, `sbxenv.d/*.yaml`, and `sbxenvs/NAME.yaml` files use these fields:

| Field | Meaning |
|---|---|
| `schemaVersion` | Required; the string `"1"` |
| `mcp.servers` | List of servers, each with a `name` and either `url` or `command` |
| `mcp.servers[].args` | Literal argument list for a local `command` server |
| `additionalWorkspaces` | List of existing Mac directories, each with `path` and optional `readOnly` |
| `additionalWorkspaces[].readOnly` | Defaults to `false`; use `true` for reference material |

Other fields are rejected. For examples, see the [home guide](guides/set-up-your-home.md).

### Files and merging

- Each YAML file must be a regular file owned by your Mac account, not a
  symlink, and not writable by group or others. The `sbxenvs` and `sbxenv.d`
  directories must also be real directories, not symlinks.
- `sbxenv.d` holds at most 64 files. Only names ending in `.yaml` are read;
  names starting with `.` are ignored. They are concatenated in name order
  after `sbxenv.yaml` and count as shared entries.
- Shared lists are followed by the command's lists. Entries cannot remove or
  override shared entries. Duplicate server names or overlapping directories
  in the combined lists are errors.
- The same MCP name must have the same definition across Kits. Aliases of one
  Kit must have identical combined configuration because they share a VM.
- Each file is limited to 64 KiB. A combined environment permits at most 32
  MCP servers and 16 extra directories.

### Live reload

The daemon watches these files while it runs; there is nothing to restart.
It checks file stamps every couple of seconds and before it starts a job or
prepares a VM, then reads and validates all files together.

- **Valid change:** it replaces the active configuration. New MCP servers are
  registered with `sbx mcp add` (an existing, matching registration is reused)
  and loaded into the warm Kit VMs of the affected commands and into every VM
  created afterwards. A server whose name already exists with a different
  definition is refused, as before; use a new name. Changed
  `additionalWorkspaces` apply to jobs started afterwards; running jobs keep
  their mounts.
- **Removed server:** not loaded into new VMs. A Kit VM with no running job has
  it unloaded (`sbx mcp unload`); a busy VM keeps it until idle. The shell VM
  unloads it when no other shell is attached. The registration is left alone.
- **Invalid change:** any error the checks above or in
  [Troubleshooting](troubleshooting.md#environment-files-and-mcp-connections)
  describe, including a rejected registration, keeps the last good
  configuration. The error shows in `marsh status`, and each open shell gets a
  one-line notice at its next command. The file is retried when it changes, and
  every few seconds otherwise.
- **At start:** a bad file stops the daemon from starting, because there is no
  earlier configuration to keep.

`marsh status --json` has a `sbxenv` object: `files` (the files behind the
active configuration, in load order), `hash` (SHA-256 of their names and bytes),
`last_reload_unix_ms`, and `last_error` (`message`, `unix_ms`) while the files on
disk are being rejected. A server unloaded or loaded after a daemon restart is
not tracked: a VM that outlived the daemon can keep a removed server until it is
replaced (`marsh workers reset KIT`).

### Paths and access

Extra directories appear at their canonical Mac paths in Linux. Relative paths
are resolved from the YAML file's directory; `~` and variables are not expanded.
Directories cannot overlap the project, the guest home's backing directory,
another extra directory, or marsh's install and host-control files. They cannot
replace the guest home itself.

Root jobs receive the shared directories plus their command's directories.
Child jobs inherit the parent's mounts, without adding directories from the
child Kit's file. Split Kit jobs omit extra directories.
`marsh jobs show JOB --json` includes the mount targets and access modes.

A local MCP `command` is an absolute path or an executable name on the Mac's
`PATH`; its arguments are literal strings. Its resolved executable and the
environment files cannot be guest-mounted. The YAML ownership and symlink rules
above apply to the configuration files, not the executable.

MCP servers are loaded per VM. The shell VM can load servers too, but you need
an MCP client to call them. See [MCP access](security.md#mcp-connections).

### Applying changes

Files are read when the daemon starts. After editing them or adding a file for
a newly registered command, finish jobs, exit shells, run `marsh stop` on the
Mac, and launch marsh again. A worker reset reloads the daemon's existing
configuration; it does not reread the YAML. Ephemeral-home sessions skip these
files' servers and directories.

A declared server must register and load successfully before its VM can be
used. [Setup errors](troubleshooting.md#environment-files-and-mcp-connections)
explain authentication and name conflicts.

## Shell prompt and startup files

The shell reads the usual startup files from the guest home. Set `PS1` there
to change the prompt. See [The shell](shell.md#startup-files).

## Results

`marsh results` lists job receipts with the registered command name, where it
ran, exit status, cleanup verification, and timing. The journal retains up to
1,000 terminal receipts, including at most 16 arguments of 200 characters
each. Prompts passed as arguments can be stored; stdin, stdout, and stderr
are not recorded. marsh does not resume work after a daemon restart.

Next: [Commands and Kits](kits.md), [Troubleshooting](troubleshooting.md).
