# Set up your marsh home

marsh mounts `~/.marsh/home` as the Linux user's home. Put your agent settings,
skills and dotfiles there, not in your real Mac home. Nothing needs to be
packaged as a profile or installed from a registry.

| Agent | Skills | Global instructions |
|---|---|---|
| Pi | `~/.marsh/home/.agents/skills/NAME/SKILL.md` | `~/.marsh/home/.pi/agent/AGENTS.md` |
| Codex | the same `.agents/skills` directory | `~/.marsh/home/.codex/AGENTS.md` |
| Claude | `~/.marsh/home/.claude/skills/NAME/SKILL.md` | `~/.marsh/home/.claude/CLAUDE.md` |

Normal agent login, settings and history files live alongside these files. Do
not put the entire guest home in Git. Track only selected personal content;
exclude authentication, sessions, caches, plugins and runtime state. Keep any
repository containing internal workflows private. Shell startup files are
described in [Choosing a shell](../shells.md).

## MCP servers and additional directories

Two optional locations configure native SBX environment **additions**:

```text
~/.marsh/
  home/                  # guest home: agent settings, skills, dotfiles
  sbxenv.yaml            # shared additions for every Kit and the shell
  sbxenvs/
    pi.yaml              # additions for the registered pi command
    claude.yaml
    codex.yaml
    shell.yaml           # shell VM and registered shell command
```

With another `MARSH_HOME`, use that scope's directories instead. No file is
required. Shared lists are followed by the matching command's lists, as in
SBX's additive environment layering. Duplicate MCP names and overlapping extra
directories are errors, not replacement rules. There is no subtraction syntax;
put a connection in the per-Kit file if it should not reach every Kit. Aliases
of the same workload must have identical additions, because they share one VM.

For example, `~/.marsh/sbxenvs/pi.yaml`:

```yaml
schemaVersion: "1"
additionalWorkspaces:
  - path: /Users/me/reference
    readOnly: true
mcp:
  servers:
    - name: granola
      url: https://mcp.granola.ai/mcp
```

The supported native fields are `schemaVersion`, `additionalWorkspaces` and
`mcp.servers` (`name`, `url` or `command` with `args`). These are additions,
not a complete standalone `sbx env create` document: marsh supplies the Kit,
random owned VM name, primary workspace, guest home and worker. Other fields,
including `agent`, `name`, `kits`, `workspace`, `env`, `secrets`, `bindings`,
`sandboxOptions`, `ports`, `args` and `lifecycle`, are refused explicitly.

Files are read once when the scope's daemon starts. After editing, let jobs and
shells finish, run `marsh stop`, then start marsh again. Configuration generations
have distinct owned VM names; existing VMs are not silently reconfigured. A
worker reset recreates its VM with the daemon's same snapshot and reloads MCP.
Kit files for commands registered after daemon startup take effect after a
daemon restart. Ephemeral-home sessions do not receive personal additions.

### MCP authentication and authority

marsh reuses a matching SBX registration, or registers a missing one without
opening an OAuth browser. It refuses to overwrite a different registration
under the same name. Registrations and authentication remain managed by SBX on
the Mac, not by files in the guest home:

```sh
sbx mcp auth granola
```

If first use reports missing authorization, authorize on the Mac, then retry.
Required registration/load failures stop Kit preparation; marsh does not silently
continue without a declared connection. Loaded servers are available to every
job in that Kit VM, across projects. Verify a small read-only request; a tool
listing is not evidence that the account's data can be read. The shell can have
a gateway too, but loading a server does not add a shell MCP client command.

Per-Kit files select automatic loading; they are not an MCP permission sandbox
or an allowlist for SBX's host-global registry. SBX and the server's own grants
determine connection authority, including any gateway discovery/loading it offers.

A local server uses native SBX `command`/`args`, for example:

```yaml
schemaVersion: "1"
mcp:
  servers:
    - name: my-reader
      command: /absolute/host/path/to/trusted-mcp-reader
      args: ["--readonly"]
```

**Local MCP commands run on your Mac with your account's permissions.** Writing
the declaration opts in to that host execution when the gateway connects. Trust
the executable and every script/configuration it loads; do not keep those inputs
in writable guest mounts. The environment files and resolved executable itself
are protected against guest mounting. Files must be yours, not symlinks, and not
writable by group or others. Use SBX-managed authentication or trusted host-side
credential handling; never store keys in YAML, arguments or the guest home.

SBX registrations are host-global. Removing a declaration or stopping marsh
does not delete the registration or its OAuth grant. Use `sbx mcp` to manage
them explicitly, without disrupting another sandbox that uses the same server.

### Mount behavior

Directories must exist and pass marsh's normal descriptor-bound source checks.
They appear at their canonical Mac paths in Linux. Relative paths are resolved
against the file declaring them; `~`, `${HOME}` and other variables are not
expanded. `readOnly` defaults to false, as in native SBX; set it to true for
reference material.

Extra sources must not overlap the project, the managed home backing directory,
another extra source, or marsh's protected host control/configuration/artifacts.
They may be below the guest user's home path (for example `~/reference`) but may
not replace the whole guest home. The shell sees shared plus `shell.yaml`
directories; each root Kit job sees shared plus its own file. Receipts include
every added mount and its access mode.

Nested jobs inherit the parent's recorded mounts, not new mounts from the
child's Kit file. Split Kit jobs omit all extra directories: they get the fork,
its Git metadata, and the guest home only. This prevents an extra mount from
bypassing the split's project isolation. MCP access still follows the child's
Kit VM, as network policy and credentials do.
