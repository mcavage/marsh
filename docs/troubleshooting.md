# Troubleshooting

Start with these three commands:

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
| A command exits 125 or 126 | [A job was refused](#a-job-was-refused) |
| `cleanup uncertain` in `jobs --tree` | [Cleanup uncertain, and quarantine](#cleanup-uncertain-and-quarantine) |
| A VM will not go away | [Removing VMs](#removing-vms) |
| `join` left a split behind | [A split was kept](#a-split-was-kept) |
| `ephemeral home capacity exhausted` | [Ephemeral home slots](#ephemeral-home-slots) |

## The first command is slow

The first shell boots the shell VM. The first call to each Kit boots that
Kit's VM, loads its image, and prints `[starting NAME worker VM…]`. Later
calls reuse warm VMs.

To boot them up front, use `marsh --load all` or `marsh --load claude,codex`.

## marsh will not start

### Missing or old `sbx`

| Message | Cause and fix |
|---|---|
| `stock sbx not found on PATH`, followed by `marsh needs Docker Sandboxes: ...` | The marsh formula does not install `sbx`. Run `brew install docker/tap/sbx`, then `sbx login`. To use an `sbx` elsewhere, set `MARSH_SBX` to its path. |
| `stock sbx not found at PATH (MARSH_SBX)` | `MARSH_SBX` names a file that does not exist or is not a regular executable. Fix the path, or unset `MARSH_SBX` to use `sbx` on `PATH`. |
| `stock SBX v0.45.0 or newer ... required` | Run `brew upgrade docker/tap/sbx`. |

### Login and policy errors from `sbx`

If `sbx` reports that you are not signed in, run `sbx login`.

If `sbx` asks for a network policy, `sbx policy init balanced` sets the
default one.

If `sbx` refuses a Kit's source, allow `docker.io/mcavage` (or your own
registry) in its Kit sources policy.

### Install and configuration errors

| Message | Cause and fix |
|---|---|
| `install must provide libexec/marsh/shell-image` | `bin/` and `libexec/marsh/` are not under the same prefix. Reinstall with `brew reinstall mcavage/tap/marsh`. |
| A message naming `commands.json` or `agents.json` | That file in the control directory is malformed. Fix it. `marsh status` shows the control directory. |

### Cannot mount the directory

The message is `cannot mount DIR into the VM: it contains marsh's private host
state at PATH`. It can also end `it is inside ...`.

Start marsh from your project, or from a scratch directory:

```sh
mkdir -p ~/scratch && cd ~/scratch
```

The directory you started in contains, or is inside, one of marsh's own paths:

- the install prefix
- the control directory
- a Kit source
- the daemon's runtime directory

`/tmp` itself fails for this reason.

## A job was refused

A refused job starts nothing, prints a reason, and exits 125. It exits 126 for
a name that `MARSH_SPAWN` excludes.

| Message | Cause |
|---|---|
| `capacity: 8 jobs` | 8 jobs are already running under this daemon |
| `depth limit 4` | the tree is already 4 jobs deep |
| `fan-out limit 4` | this job already has 4 running children |
| `... refused: same-Kit chain limit 2` | the call would make the same Kit three times in a row |
| `total limit 64` | the tree has started 64 jobs |
| `spawn refused: NAME not in this job's spawn set` | `MARSH_SPAWN` or `--spawn` excludes it (exit 126) |
| `interactive child jobs are not supported yet` | a child job was given a terminal; redirect its stdin and stdout |

## Cleanup uncertain, and quarantine

marsh checks that every container it starts is deleted. Sometimes it cannot
confirm that, for example because the connection to a Kit VM was lost
mid-job. Then marsh:

- does not re-run the job
- marks the job `cleanup_uncertain` (`cleanup uncertain` in `jobs --tree`)
- quarantines that Kit VM, so no new jobs go to it

To clear it:

```sh
marsh workers reset KIT        # the Kit named in the message, or: all
```

This removes the VM, and the next call creates a new one. Files the job wrote
to your project stay as they are. Review them with `git status` and
`git diff`, or your own tools.

## Removing VMs

```sh
marsh workers reset KIT|all    # idle Kit VMs
marsh reset                    # every VM in this scope; daemon keeps running
marsh stop                     # every VM in this scope, then stop the daemon
```

`reset` and `stop` refuse while shells or jobs are open. Each prints the VMs it
removed.

If a removal could not be verified, the command names what is left and exits
nonzero.

Your project, guest home, and job results are kept. Packages installed in a VM
are not.

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

`join` keeps a split if its command failed or if a branch could not be
confirmed stopped:

```sh
marsh splits               # list them
marsh splits ID            # one split's branches and states
marsh splits rm ID         # delete it
```

If a branch's VM was lost, run `marsh workers reset all` before `rm`.

See [split and join](split.md) for details.

## Ephemeral home slots

`marsh --ephemeral-home` uses one of 16 private slots. It skips a slot that still
holds data, which happens when a shell's cleanup was not verified. When no slot
is free, it fails with `ephemeral home capacity exhausted`.

When nothing is using the slot, discard it. This deletes the slot's data:

```sh
marsh recover-home SLOT --discard
```

## Stale daemon after an upgrade

- Upgrading `sbx`: marsh restarts an idle daemon on its own. Rerun
  `marsh mcp install ...` for any MCP registrations.
- Upgrading marsh: a running daemon keeps the old binaries. Run `marsh stop`
  when idle.

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

## Kit builder holds a stale build

`marsh` reports that stock SBX's Kit builder holds a stale build of a Kit,
typically after Docker's build cache was pruned. Remove the builder; stock
`sbx` recreates it on the next Kit build. Then retry:

```sh
sbx kit builder rm --force
```
