# Configuration

marsh keeps configuration and runtime state per `MARSH_HOME`. Use this page
to change job limits, choose a shell, or find the files used by your
[installation](install.md).

For a separate guest home and set of VMs, launch with
`MARSH_HOME=~/.marsh-work marsh`. To raise the per-job memory limit to 16 GiB
for jobs started from now on, run
`marsh config limit job-memory-bytes 17179869184`.

`marsh config limits` shows the limits in effect and whether each came from a
default, `config.json`, or the daemon's launch environment.

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
| `MARSH_SBX` | Path to `sbx`. Default: `sbx` on `PATH`. A bare name (no `/`), like the default, is looked up only on the absolute entries of `PATH`: empty and relative entries such as `.` are skipped, so a file in the current directory is never used. A relative path is taken against the directory marsh starts in. |
| `MARSH_DOCKER` | Path to the Docker CLI the daemon reads Kit images from registries and builds local Kits with. Default: `docker`. Docker runs with the `PATH` `/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin`, so a bare name is looked up only there, not on your shell's `PATH`, and the daemon does not start if it is not found; give the absolute path of a Docker CLI installed elsewhere (such as `~/.docker/bin/docker`). |
| `MARSH_CONTROL_HOME` | Host-only state root. See [Control home](#control-home). |
| `MARSH_SPAWN` | Names that jobs started from this shell may start. A comma-separated list, or `none`. See [spawn restrictions and refusal statuses](agents.md#limits-and-messages). |
| `MARSH_PLACE` | Where registered commands run. Only `local` is accepted; any other value is an error (`MARSH_PLACE must be local`). |
| `MARSH_KIT_IMAGE_CACHE` | Cache of Kit images built from source. Default `~/Library/Caches/marsh/kit-images`. |
| `MARSH_CLOUD_CPUS` | Default `--cpus` for [`cloud`](cloud.md#sandbox-size-lifetime-and-image) calls from this shell: 1, 2, 4, 8, or 16 (2 GiB of memory per CPU). Default 2. |
| `MARSH_CLOUD_TTL` | Default `--ttl` for `cloud` calls: 1m to 24h, such as `30m` or `2h`. Default `4h`. |
| `MARSH_CLOUD_IMAGE` | Default `--image` for `cloud` calls: an image pinned by digest. Read by the daemon too, as its own default. |
| `MARSH_SKILLS_HOME` | For tests: the HOME whose skills folders the [skills mirror](guides/set-up-your-home.md#agent-settings-and-skills) reads, instead of yours. Read when the daemon starts. |
| `MARSH_VM_IDLE_SECONDS` | For tests: the [idle VM limit](troubleshooting.md#garbage-collection) in seconds instead of `marsh config vm-idle-days`. Read when the daemon starts. |
| `MARSH_AUTO_UPDATE` | `0` (or `off`) turns off [automatic Kit updates](kits.md#updates), and the automatic adding of `pix` and `hermes`, for the daemon it starts, as `marsh config auto-update off` does. Read when the daemon starts. |
| `MARSH_CLOUD_CHILDREN` | The most agent sandboxes one [detached cloud run](cloud.md#agents-calling-agents-with-the-lid-closed) may start with `marsh config cloud-delegate on`: 1 to 64. Default 8. Read when the daemon starts. |
| `MARSH_OPEN_URLS` | `0` (or `off`) stops [`xdg-open` in a shell or job](agents.md#opening-links) from opening links on your Mac, as `marsh config open-urls off` does; links are printed instead. Read when the daemon starts. |
| `MARSH_SERVICES_IDLE_SECONDS` | How long a [services VM](design/services.md#lease-holders-and-idle-stop) with no lease holder keeps running before it is stopped, in seconds, instead of `marsh config services-idle-seconds` (default 900). Read when the daemon starts; tests use a short value. |
| `MARSH_OPEN_COMMAND` | The program the daemon runs as `PROGRAM URL` to open a link. Default `/usr/bin/open`. A bare name is looked up on the absolute entries of your `PATH` (never `.` or other relative entries); if it is not found there, links are not opened. Read when the daemon starts; tests point it at a recorder. |

### Job limits

Every limit has a name, a default, and a `MARSH_*` variable. Set one for this
scope with `marsh config limit NAME VALUE`, or put it back with
`marsh config limit NAME default`. The value is kept in
`$MARSH_HOME/config.json` and needs no restart:

- admission limits apply to the next launch;
- per-job resource limits apply to jobs started after the change. Running
  jobs keep theirs.

`marsh config limits` lists each limit with its value, default, source
(`default`, `config`, or `env`), range, and when a change applies.
`marsh config limits --json` gives the same list to programs. `marsh status`
shows the values in effect, and `marsh status --json` has them under `limits`.

```console
$ marsh config limit job-children 32
job-children 32
$ marsh config limit job-children default
job-children 16
```

A variable set when marsh starts wins over `config.json` until marsh
restarts without it. A shell started with a value that conflicts with the
running daemon's is refused; `marsh config limit` changes it without a
restart.

| Name | Variable | Default | Range | Applies |
|---|---|---|---|---|
| `daemon-jobs` | `MARSH_DAEMON_JOBS` | 64. Jobs at once in the scope, all depths. Each Kit VM also runs at most 64 jobs at once. | 1 to 512 | now |
| `job-children` | `MARSH_JOB_CHILDREN` | 16. Live children of one job. | 1 to 128 | now |
| `tree-depth` | `MARSH_TREE_DEPTH` | 4. Your command is depth 1. | 1 to 16 | now |
| `tree-kit-vms` | `MARSH_TREE_KIT_VMS` | 8. Distinct Kit VMs used by live jobs in one tree. Aliases of the same Kit share a VM. | 1 to 64 | now |
| `tasks-live` | `MARSH_TASKS_LIVE` | 16. Tasks one agent's [task tool](agents.md#the-task-tool) has running at once. Finished tasks do not count. | 1 to 128 | new jobs |
| `tree-launch-rate` | `MARSH_TREE_LAUNCH_RATE` | `60/60s`. Launches one job tree may make in any rolling window, as `LAUNCHES/SECONDS` (`60/1m` also works). It catches tight loops: a long session calling tools steadily never reaches it. Refused child calls count only up to 8 per window. | 1 to 10000 in 1 to 3600 s | now |
| `job-cpu-millis` | `MARSH_JOB_CPU_MILLIS` | Half the Mac's cores, at least 2 (2000 = 2 CPUs). | 1 or more | new jobs |
| `job-memory-bytes` | `MARSH_JOB_MEMORY_BYTES` | A quarter of the Mac's memory, from 4 GiB to 8 GiB. A Kit VM created while it is above stock SBX's VM size (half the Mac's memory) is made big enough for one such job. | 1 or more | new jobs |
| `job-pids` | `MARSH_JOB_PIDS` | 4096 | 1 or more | new jobs |
| `job-writable-bytes` | `MARSH_JOB_WRITABLE_BYTES` | 10737418240 (10 GiB). Counts the container's own files, not mounted directories. | 1 or more | new jobs |
| `job-output-bytes` | `MARSH_JOB_OUTPUT_BYTES` | 268435456 (256 MiB). stdout and stderr together. | 1 or more | new jobs |
| `job-wall-seconds` | `MARSH_JOB_WALL_SECONDS` | 86400 (24 hours) | 1 or more | new jobs |

A call over an admission limit is refused with exit 125. The refusal names
the limit, how to raise it, and when to retry
([messages](agents.md#limits-and-messages)). Lowering a limit ends nothing
that runs; it refuses the next launch over it.

There is no limit on the same Kit calling itself: `claude` may hand work to
`claude`, which hands work to `claude`. Depth and the launch rate stop a
runaway.

#### Writable limit

marsh periodically measures the container's writable layer and kills a job
that exceeds the limit (`limit:writable`). A fast writer can overshoot between
checks. Files in mounted directories do not count toward this limit.

#### Other limits

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
| `$MARSH_HOME/config.json` | The default shell, set by `marsh config shell NAME`, whether Kits update themselves, set by `marsh config auto-update on\|off`, whether `xdg-open` opens links on your Mac, set by `marsh config open-urls on\|off`, and whether detached cloud runs may start agent sandboxes, set by `marsh config cloud-delegate on\|off` (default off), whether marsh mirrors your skills folders into the Docker Sandboxes skills store, set by `marsh config skills-mirror on\|off` (default on), and after how many days unused VMs are removed, set by `marsh config vm-idle-days DAYS\|off` (default 3), job limits (`limits`), set by `marsh config limit NAME VALUE\|default`, and the [services VM](design/services.md) settings: `services_projects`, the projects services are on for, set by `marsh config services on\|off` in a project's folder (default none; per project, never per home), `services_disk_size`, set by `marsh config services-disk-size SIZE` (default `40g`), and `services_idle_seconds`, set by `marsh config services-idle-seconds SECONDS` (default 900). Default path `~/.marsh/config.json`. |
| `CONTROL/skills-mirror/` | Shared by every scope's skills mirror, since the Docker Sandboxes store belongs to your Mac account: `record-*.json`, the skills a mirror put in the store, with a digest of each (only these are ever replaced or removed), and `lock`, held while one scope's mirror changes the store. An older marsh's `CONTROL/HASH/skills-mirror.json` is merged in and renamed `skills-mirror.json.migrated`. |
| `$MARSH_HOME/home/` | The guest home. Agent logins and settings (`.claude`, `.codex`) live here. Every job in the scope can read it. Default path `~/.marsh/home/`. |
| `$MARSH_HOME/sbxenv.yaml` | Shared MCP servers and extra directories for every Kit and the shell. |
| `$MARSH_HOME/sbxenv.d/*.yaml` | More shared servers and directories, same schema as `sbxenv.yaml`, read in name order after it. |
| `$MARSH_HOME/peer-review.d/*.yaml` | Peer review on/off and the ready reviewers; `marsh config peer-review on\|off` writes `marsh.yaml`, Narwhal `narwhal.yaml`. Read at every job start. See [Peer review](peer-review.md). |
| `$MARSH_HOME/model-classes.yaml` | Your model-class rules, retired globs and price bands, tried before marsh's. See [Peer review](peer-review.md#model-classes). |
| `$MARSH_HOME/sbxenvs/NAME.yaml` | Additional servers and directories for registered command `NAME`; `shell.yaml` also configures the shell VM. |
| `CONTROL/HASH/` | Host-only control directory for a scope. `marsh status` prints it. Never mounted into a VM. |
| `CONTROL/HASH/commands.json` | Your command registry. Overrides packaged entries by name. See [Commands and Kits](kits.md#adding-a-command). |
| `CONTROL/HASH/vm-gc.json` | When [garbage collection](troubleshooting.md#garbage-collection) last ran, what it removed, and when this marsh last used each of its VMs. |
| `CONTROL/HASH/kit-updates.json` | When [automatic Kit updates](kits.md#updates) last checked, and what they changed. Kit changes still finishing are in `marsh status`, not here. |
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

An `additionalWorkspaces` folder inside the project is already visible at the
same path, so marsh skips it with a one-line note
(`marsh: ~/dev/pack is inside the project; already visible`). A folder that
contains the project, or overlaps the managed HOME, is refused with a message
naming the file and entry to change.

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

## Skills mirror

Every job reads skills from the Docker Sandboxes skills store
([Set up your marsh home](guides/set-up-your-home.md#agent-settings-and-skills)).
The daemon keeps that store in step with the folders `sbx skills import`
scans on your Mac (`~/.agents/skills`, `~/.claude/skills`, and the others its
`--help` lists): once at start, then about two seconds after a change there it
runs `sbx skills import --force --no-input` for the skills whose store copy
differs, and `sbx skills rm --force` for the skills it imported whose folder
is gone. It never replaces or removes a skill it did not import itself: when
a name is already taken by a skill from `sbx skills add`, that skill wins and
the name is listed as a clash. An `sbx skills add` skill with the same content
as the folder's copy is left alone and stays when the folder's copy goes. What
any scope's mirror imported, every scope's mirror may update or remove: the
record is kept per Mac account, beside the scopes' control folders, and one
mirror at a time changes the store (another scope's waits and tries again a
minute later).

The import gets a copy of each skill's folder, never a link to it. A skill
folder may itself be a link (`~/.claude/skills/my-skill -> ~/dev/my-skill`),
but inside it marsh copies only directories and regular files. A link inside
counts as the regular file it names in the same folder, by a relative path
that stays in the folder. A link that leads anywhere else, a file with other
hard links, or a socket or device, keeps the whole skill out of the store: the store keeps its last good
copy, and `marsh status` names the skill and the link under errors.
Hard links are refused because the other link could be any file of yours.
The whole skill is kept out, not just that file, so a skill never reaches
jobs incomplete. This has two costs. A job that can write a skill's folder
can keep that skill from updating by hard-linking one of its files, until
you remove the extra link. And a skill kept in a hard-linked package store
(as pnpm makes) is refused.

A skill with the name of one a registered Kit bundles (`agent-skill@1`) is
never imported: the Kit's copy wins, and the name is listed as a clash
`NAME (bundled by KIT)`. One an earlier marsh imported is removed from the
store. Docker Sandboxes refuses to create a VM whose Kit bundles a skill the
store also holds; if that still happens (an `sbx skills add` skill of that
name), marsh creates the Kit's VM without the store (`--skills off`), and
`marsh status` says `KIT runs without shared skills: name clash with NAMES`.
Remove the store's copy (`sbx skills rm NAME`), then `marsh workers reset KIT`
to get the store back in that Kit.

```sh
marsh config skills-mirror        # on (the default) or off
marsh config skills-mirror off
```

The setting is `skills_mirror` in `$MARSH_HOME/config.json`, read on every
pass. `marsh status` shows the last import, clashes, and errors. `marsh status
--json` has the feature `skills-mirror-v1` and a `skills_mirror` object:
`enabled` (bool), `last_import` (unix ms of the last finished import or
removal, or `null`), `errors` (strings from the last pass; empty when it
succeeded, plus a line for each skill kept out of the store and each Kit VM
running without the store), and
`clashes` (skill names where the `sbx skills add` skill won, and
`NAME (bundled by KIT)` for those a Kit bundles).
`sbx settings set skills.defaultMode off` keeps the store out of new Kit VMs;
jobs then see only the Kit's own skills.

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
