# The shell

`marsh` (or `msh`, the same program) opens a Bash-compatible shell in a Linux
VM, with your project mounted at the path it has on your Mac:

```sh
cd ~/src/app
marsh -c 'pwd; uname -sm'
```

```text
/Users/you/src/app
Linux aarch64
```

The VM is a Docker Sandboxes (`sbx`) microVM. Run a command, a script, or an
interactive session:

```sh
marsh                          # interactive shell
marsh -c 'make test'           # one command, like bash -c
marsh ./script.sh arg          # run a script
```

Agents are ordinary commands in that shell. Each runs in its own container
and works in a pipe:

```sh
claude -p 'list the TODOs in this repo' | sort | head
```

For parallel agents on private copies of your files, see
[Split and join](split.md) and [Fanout](fanout.md). How agents sign in is in
[Commands and Kits](kits.md).

## Where you are

Project
: The directory you started in, mounted read/write at the same path. `pwd`
  prints the Mac path. Changes appear on both sides immediately. marsh does
  not review or protect these files: any command you run, including an
  agent, can edit or delete them.

Home
: `$HOME` has the same path as your Mac home. It is backed by
  `~/.marsh/home` (the guest home) and persists between shells, so put
  dotfiles there. Set `MARSH_HOME` to use another scope (see
  [Configuration](configuration.md)).

User
: Your Mac user name and UID. You have passwordless `sudo` and a private
  Docker Engine (`docker run` works) inside the VM. Neither sudo nor Docker
  reaches the Mac. Packages installed with `sudo apt-get` last as long as the
  VM.

System
: Debian on arm64 from the marsh shell image, with git, make, gcc, Node.js,
  and Rust.

### Shared VM

All shells in one scope share one warm shell VM and one daemon. A second
project opened in the same scope uses the same VM and can see the first
project's files. To keep work apart, use a separate `MARSH_HOME`.

### Ephemeral home

`marsh --ephemeral-home` starts a shell with a blank home that is not kept.
That shell gets its own VM.

## Command-line arguments

marsh passes options it does not recognize to the shell, so the usual Bash
options work:

```sh
marsh -o pipefail -c 'false | true'
marsh -x script
```

After `-c` or `--`, marsh passes every argument to the shell unchanged.

A script named like a marsh subcommand (`stop`, `reset`) must be run as
`marsh ./stop`.

## marsh commands in the shell

| Command | What it does |
|---|---|
| `split { a: ...; b: ... } \| join [\| CMD]` | Branches on private copies; see [Split and join](split.md) |
| `fanout { a: ...; b: ... } \| collect` | Branches on the same files; see [Fanout](fanout.md) |
| `ps --marsh`, `top --marsh` | This project's shell, jobs, and agent sessions |
| `acp run`, `acp ask`, ... | Agent sessions over ACP; see [ACP](acp.md) |
| `mcp publish`, `mcp load`, ... | Publish a pipeline as an MCP tool; see [MCP](mcp.md) |
| `marsh jobs`, `marsh split`, ... | The `marsh` CLI works inside the shell too |

`split {` and `join` directly after a split stage are marsh syntax. To reach
the coreutils programs, use `command split` or `command join`.

## Registered commands

Registered commands such as `claude`, `codex`, `pi`, and `shell` each run in
a fresh container in a VM for that Kit. stdin, stdout, stderr, and exit
status pass through. They work in pipelines, `$( )`, background jobs,
scripts, and `make`.

Child processes get the same behavior from a private directory of links on
`PATH`. `type claude` prints the link's path.

As in Bash, a function or builtin with the same name takes precedence. An
explicit path bypasses both. See [Commands and Kits](kits.md).

## What the shell is

The shell is [Brush](https://github.com/reubeno/brush), a Bash-compatible
shell written in Rust. marsh never runs Bash to implement itself.

marsh uses upstream revision `1389a8e` plus 49 patches in
[`docs/upstream/brush/`](upstream/brush/README.md):

- 34 patches fix Bash compatibility: job control, signals and traps, `wait`,
  aliases, namerefs, subshells, globstar, redirections, and startup files.
- 15 patches add registered commands, `split`/`join`, and `fanout`/`collect`.

To use the VM's GNU Bash or zsh instead, run `marsh --shell bash`, set
`MARSH_SHELL=zsh`, or set a default with `marsh config shell zsh`. Registered
commands and the `marsh` CLI keep working. The `split { }` and `fanout { }`
forms do not. See [Choosing your shell](shells.md).

## Startup files

marsh reads these files from the guest home:

| Shell type | Files read |
|---|---|
| Interactive | `/etc/bash.bashrc`, `~/.bashrc`, `~/.brushrc` |
| Login | `~/.bash_profile` or `~/.profile` |
| Noninteractive | `$BASH_ENV` |

The prompt defaults to Bash's `\s-\v\$ `.

## Bash compatibility

Ordinary scripts and interactive use behave as in Bash except for the
differences below. Each is recorded and tested:

- A `RETURN` trap does not fire.
- `select` and `${x~~}` are not supported.
- `(( m["a b"]=1 ))` and `m[a b]=1` do not parse.
- A trapped signal runs its trap after the current pipeline, not in the
  middle of a builtin. A blocking `read` from a terminal is not interrupted.
- `set -u` on an unset variable exits with status 1 (Bash 5.3: 127).
- Error messages use Brush's wording and add a `(col N)` suffix.
- `printf '%(fmt)T'` uses the process `TZ`, not one set only in the shell.
- A background job killed by a signal is reported as `Exit 143`. Bash prints
  the signal name.
- If your terminal never answers the cursor-position query, the line editor
  waits two seconds, warns, and falls back to plain line input.

The full list, with the test for each entry, is in
[Bash compatibility](design/bash-compatibility.md).

### Report a difference

[Open an issue](https://github.com/mcavage/marsh/issues) with the smallest
script that shows the difference and Bash's output. Fixes go into the Brush
patch queue.

Next: [Split and join](split.md), [Choosing your shell](shells.md).
