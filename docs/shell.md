# The shell

`marsh` opens a Linux shell with your project at its Mac path. `msh` is a
symlink to the same program. After [installing marsh](install.md), run this
from your project directory:

<!-- doc-test: host stdout="Linux aarch64" -->
```sh
marsh -c 'pwd; uname -sm'
```

The first line is your current Mac directory; the second is `Linux aarch64`.

The shell runs in a Docker Sandboxes (`sbx`) microVM:

| Invocation | Runs |
|---|---|
| `marsh` | An interactive shell |
| `marsh -c 'make test'` | One command string |
| `marsh ./script.sh arg` | A script with arguments |

Registered agents work in pipelines and run in fresh containers:

<!-- doc-test: shell replace=["claude -p 'list the TODOs in this repo'", "fixture pipeline '|' printf 'TODO: review\\n'"] stdout="TODO: review" -->
```sh
claude -p 'list the TODOs in this repo' | sort | head
```

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
: Your Mac user name and UID. The shell user has passwordless `sudo` and a
  private Docker Engine (`docker run` works) inside the VM. Packages installed
  with `sudo apt-get` last as long as the VM. Kit jobs start as nonroot users
  in separate containers with no Docker or SBX sockets. See the
  [security model](security.md) for the shell VM's authority and explicit
  host grants.

System
: Debian on arm64 from the marsh shell image, with git, make, gcc, Node.js,
  Rust, Vim, and terminal Emacs.

### Shared VM

Persistent-home shells in one scope share one warm shell VM and one daemon.
A second project opened in the same scope can see the first
project's files. To keep work apart, use a separate `MARSH_HOME`.

### Ephemeral home

`marsh --ephemeral-home` starts a shell with a blank home that is not kept.
That shell gets its own VM.

## Command-line arguments

marsh passes options it does not recognize to the selected shell. For example,
`marsh -x script.sh` enables tracing. In Brush, `pipefail` makes this command
exit 1:

<!-- doc-test: host status=1 -->
```sh
marsh -o pipefail -c 'false | true'
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

The [upstream record](upstream/brush/README.md) tracks the vendored revision
and marsh's patch queue, including compatibility fixes and the composition
syntax.

To use the VM's GNU Bash or zsh instead, run `marsh --shell bash`, set
`MARSH_SHELL=zsh`, or set a default with `marsh config shell zsh`. Registered
commands and the `marsh` CLI keep working. The `split { }` and `fanout { }`
forms do not. See [Choosing your shell](shells.md).

## Startup files

Edit `~/.bashrc` at the marsh prompt (or `~/.marsh/home/.bashrc` from your
Mac in the default scope) to set aliases, environment variables, and your
prompt. The [home guide](guides/set-up-your-home.md) walks through setup.
Your dotfiles survive VM removal. Packages you install in the VM do not.

Brush reads these startup files, with `~` referring to the guest home:

| Shell type | Files read |
|---|---|
| Interactive, non-login | `/etc/bash.bashrc`, `~/.bashrc`, `~/.brushrc` |
| Login | `/etc/profile`, then the first readable file among `~/.bash_profile`, `~/.bash_login`, `~/.profile` |
| Noninteractive, non-login | The file named by `$BASH_ENV`, when set |

The prompt defaults to Bash's `\s-\v\$ `.

## Editor settings

Vim and terminal Emacs are included: use `vim FILE` or `emacs -nw FILE`.
They read `~/.vimrc` and `~/.emacs.d/init.el` from the guest home.
From a Mac editor, those paths are `~/.marsh/home/.vimrc` and
`~/.marsh/home/.emacs.d/init.el` in the default scope.

## Bash compatibility

Brush supports ordinary Bash scripts and interactive use, with known gaps.
These differences have regression tests:

- A `RETURN` trap does not fire.
- `select` and `${x~~}` are not supported.
- `(( m["a b"]=1 ))` and `m[a b]=1` do not parse.
- A trapped signal runs its trap after the current pipeline, not in the
  middle of a builtin. A blocking `read` from a terminal is not interrupted.
- `set -u` on an unset variable exits with status 1.
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
