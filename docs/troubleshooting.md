# Troubleshooting

Use marsh's status and job records to find the failing part of a command:

<!-- doc-test: host -->
```sh
marsh status
marsh jobs --tree
marsh results
```

- `marsh status` shows the daemon, the host control directory, job limits, open
  shells, and Kit VMs.
- `marsh jobs --tree` shows jobs under their parents.
- `marsh results` shows each recent job, its exit status, and whether its
  cleanup was verified.

| Symptom | Section |
|---|---|
| The first call to an agent takes a while | [The first command is slow](#the-first-command-is-slow) |
| Error before a shell opens | [marsh will not start](#marsh-will-not-start) |
| An environment file or MCP connection fails | [Environment files and MCP connections](#environment-files-and-mcp-connections) |
| A command exits 125 or 126 | [A job was refused](#a-job-was-refused) |
| `cleanup uncertain` in `jobs --tree` | [Cleanup uncertain and quarantine](#cleanup-uncertain-and-quarantine) |
| A VM will not go away | [Removing VMs](#removing-vms) |
| `join` left a split behind | [A split was kept](#a-split-was-kept) |
| `ephemeral home capacity exhausted` | [Ephemeral home slots](#ephemeral-home-slots) |
| A local Kit's image digest does not match | [Local Kit builds](#local-kit-build-does-not-match) |

## The first command is slow

Starting with no VMs requires the shell VM to boot and each Kit VM to boot
and load its image on first use. A foreground call prints
`[starting NAME worker VM…]` when it boots a Kit VM; background calls suppress
that notice. Later calls reuse warm VMs.

To boot them up front, use `marsh --load all` or `marsh --load claude,codex`.

## marsh will not start

### Missing or old `sbx`

| Message | Cause and fix |
|---|---|
| `stock sbx not found on PATH`, followed by `marsh needs Docker Sandboxes: ...` | The marsh formula does not install `sbx`. Run `brew install docker/tap/sbx`, then `sbx login`. To use an `sbx` elsewhere, set `MARSH_SBX` to its path. |
| `stock sbx not found at PATH (MARSH_SBX)` | `MARSH_SBX` names a file that does not exist or is not a regular executable. Fix the path, or unset `MARSH_SBX` to use `sbx` on `PATH`. |
| `stock SBX v0.47.0 or newer ... required` | Check `sbx version`, then run `brew upgrade docker/tap/sbx`. |

### Login and policy errors from `sbx`

If `sbx` reports that you are not signed in, run `sbx login`.

If `sbx` asks for a network policy, `sbx policy init balanced` sets the
default one.

If `sbx` refuses a Kit's source, allow `docker.io/mcavage` (or your own
registry) in its Kit sources policy.

### Install and configuration errors

| Message | Cause and fix |
|---|---|
| `install must provide libexec/marsh/shell-image` | `bin/` and `libexec/marsh/` are not under the same prefix. Reinstall with the method you used: `brew reinstall mcavage/tap/marsh`, rerun the install script, or copy the tarball's `bin/` and `libexec/` under one prefix. |
| A message naming `commands.json` or `agents.json` | That file in the control directory is malformed. Fix it. `marsh status` shows the control directory. |

### Cannot mount the directory

The message is `cannot mount DIR into the VM: it contains marsh's private host
state at PATH`. It can also end `it is inside ...`.

The directory you started in contains, or is inside, one of marsh's own paths:

- the install prefix
- the control directory
- a Kit source
- the daemon's runtime directory

For example, `/tmp` contains the daemon's private runtime directory and
cannot be mounted as a project. Start marsh from your project or a scratch
directory outside the protected paths.

## Environment files and MCP connections

Configuration errors name the YAML file. Edit it on the Mac, then finish jobs,
exit marsh shells, run `marsh stop`, and start marsh again.

| Error | Fix |
|---|---|
| Unsupported field or schema version | Use `schemaVersion: "1"` and the [supported fields](configuration.md#servers-and-extra-directories). |
| File ownership, permissions, or symlink | Use a regular YAML file owned by your Mac account; remove group/other write permission. `sbxenvs` must be a real directory too. |
| Duplicate MCP name or overlapping directory | Remove the duplicate from the shared or per-command file. Lists add together; they do not override each other. |
| Aliases have different environments | Give names for the same Kit identical combined configuration. |
| Server already has a different host-global definition | Choose another server name, or update the existing registration with `sbx mcp` on the Mac. Other sandboxes may use it. |
| Missing MCP authorization | Run `sbx mcp auth NAME` on the Mac and retry the job. |
| MCP registration or loading failed | Check the reported `sbx` error and retry after fixing the connection. Declared servers must load before the VM can be used. |

Restart after changing YAML. Authentication changes alone can be retried
without editing the file. A worker reset uses the daemon's existing YAML
snapshot. Published pipeline defaults have a different failure policy:
see [MCP defaults](mcp.md#changing-or-removing-a-default).

## A job was refused

A refused job starts nothing and prints a reason. A registered-name link
excluded by `MARSH_SPAWN` exits 126; other refusals, including those through
`marsh run`, exit 125.

| Message | Cause |
|---|---|
| `capacity: 16 jobs` | 16 jobs are already running under this daemon |
| `depth limit 4` | the tree is already 4 jobs deep |
| `fan-out limit 8` | this job already has 8 running children |
| `... refused: same-Kit chain limit 2` | the call would make the same Kit three times in a row |
| `total limit 128` | the tree has used its 128 launches, including refused child calls |
| `tree already uses ... Kit VMs (...) (MARSH_TREE_KIT_VMS)` | live jobs in the tree already use the configured number of distinct Kits; let one finish before adding another |
| `deadline: parent job ... has ... ms of wall time left` | less than one second remains before the parent's deadline; a child cannot outlive it |
| `spawn refused: NAME not in this job's spawn set` | `MARSH_SPAWN` or `--spawn` excludes it (126 from a registered-name link, 125 from `marsh run`) |
| `interactive child jobs are not supported yet` | a child job was given a terminal; redirect its stdin and stdout |

Refused child calls count toward the tree's 64-launch budget. Avoid retry
loops. A depth or same-Kit-chain refusal needs a different call structure;
waiting will not clear it. See [job-tree limits](agents.md#limits-and-messages).

## Cleanup uncertain and quarantine

marsh checks that every container it starts is deleted. Sometimes it cannot
confirm that, for example because the connection to a Kit VM was lost
mid-job. Then marsh:

- does not re-run the job
- marks the job `cleanup_uncertain` (`cleanup uncertain` in `jobs --tree`)
- quarantines that Kit VM, so no new jobs go to it

Run `marsh workers reset KIT`, using the Kit name in the message, or
`marsh workers reset all` to retire all idle or quarantined Kit VMs.

This removes the VM, and the next call creates a new one. Files the job wrote
to your project stay as they are. Review them with `git status` and
`git diff`, or your own tools.

## Removing VMs

| Command | Effect |
|---|---|
| `marsh workers reset KIT` | Remove one idle or quarantined Kit VM |
| `marsh workers reset all` | Remove all idle or quarantined Kit VMs |
| `marsh workers reset cloud` | Delete every [Docker Cloud](cloud.md) sandbox this daemon owns |
| `marsh reset` | Remove the scope's VMs; keep the daemon running |
| `marsh stop` | Remove the scope's VMs and stop its daemon |

`reset` and `stop` refuse while shells or jobs are open. Each prints the VMs it
removed.

If a removal could not be verified, the command names what is left and exits
nonzero.

Your project, guest home, and job results are kept. Packages installed in a VM
are not.

`reset` and `stop` do not delete Docker Cloud sandboxes. marsh deletes each
one when its `cloud` call ends; if a call ended `cleanup uncertain`, run
`marsh workers reset cloud`. Cloud sandboxes also expire after 4 hours.

### Which VMs marsh touches

marsh only touches VMs it created. They are named `marsh-k-…` (Kits) and
`marsh-s-…` (shells), and the daemon keeps a list of them. Any other VM,
including ones you made with `sbx`, is left alone.

Remove marsh's VMs with `marsh workers reset`, `reset`, or `stop`, not with
`sbx`. If you remove one with `sbx`, the daemon treats its state as unknown,
not as cleaned up.

### A removal keeps failing

Fix `sbx` first. `sbx ls` should work. Then run the same command again.

Do not delete the control directory or kill the daemon to get past it. The
daemon's list of its VMs is in that directory, and without it marsh cannot
prove which VMs are its own.

## A split was kept

`join --keep` retains results deliberately. A nonzero join status, interrupted
join, expired lease, or unconfirmed branch cleanup also keeps them. Bare join
returns the branch status; with a downstream command, retention follows that
command's status. `pipefail` does not change retention.

List results with `marsh splits`, inspect a split's state with
`marsh splits ID`, and remove it with `marsh splits rm ID`. Removal deletes
the forks and patches, so apply anything you want first. Join never applies
a patch itself.

If a branch's VM was lost, run `marsh workers reset all` before `rm`.

See [split and join](split.md) for details.

## Ephemeral home slots

`marsh --ephemeral-home` uses one of 16 private slots. It skips a slot that still
holds data, which happens when a shell's cleanup was not verified. When no slot
is free, it fails with `ephemeral home capacity exhausted`.

When nothing is using the slot, run `marsh recover-home SLOT --discard`,
substituting its slot number. This deletes the slot's data.

## Stale daemon after an upgrade

- Upgrading `sbx`: marsh replaces its daemon on its own once no shell or job
  is running, including when the upgrade removed the old `sbx`. Rerun
  `marsh mcp install ...` for any MCP registrations.
- Upgrading marsh: a running daemon keeps the old binaries. Run `marsh stop`
  when nothing is running; the next launch uses the new marsh and checks for
  newer Kits ([Upgrade](install.md#upgrade)).

## Reporting a bug

Include:

- `marsh --version` and `sbx version`
- the exact command
- what you expected
- what happened (stdout, stderr, exit status)

For shell behavior, run the same script with `bash -c` and include both
outputs. marsh aims to match Bash.

Do not paste tokens or private files. Report security problems as described
in [SECURITY.md](../SECURITY.md).

## Local Kit build does not match

marsh and Docker Sandboxes must build the same image for a local-source Kit.
marsh retries once if their digests differ. If it reports "still mismatched
after stock SBX rebuilt the Kit", run `docker buildx ls` and select a local
builder (the `*` entry, for example `desktop-linux`), then retry. Both builds
need to use the same BuildKit cache. See the [registry design](design/command-registry.md)
for the build and image checks.

Next: [Configuration](configuration.md), [Commands and Kits](kits.md).
