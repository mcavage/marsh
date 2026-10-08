# Choosing your shell

To use your own `.bashrc` or `.zshrc`, run the shell VM's GNU `bash` or
`zsh` instead of Brush:

```sh
marsh --shell zsh -c 'echo $ZSH_VERSION'
```

Registered commands (`claude`, `codex`, ...), `acp`, `mcp`, and the `marsh`
CLI keep working. The `split { }` and `fanout { }` forms do not
([details](#what-is-brush-only)). Choose bash or zsh when your startup files
matter more than those forms. Brush is the default.

## Selecting

| Where | Form | Scope |
|---|---|---|
| flag | `marsh --shell NAME [ARGS...]` | this launch |
| environment | `MARSH_SHELL=NAME marsh` | this launch |
| home default | `marsh config shell NAME` | every launch with this `MARSH_HOME` |

`NAME` is `marsh` (Brush), `bash`, or `zsh`.

Precedence, highest first: `--shell`, then `MARSH_SHELL`, then the home
default. With none set, you get Brush (`marsh`).

`marsh config shell` with no name prints the default. The default is stored
in `$MARSH_HOME/config.json`, outside the guest home.

An unknown name exits 2 before any VM starts and prints
`unknown shell "NAME" (choose marsh, bash, or zsh)`, preceded by which of the
three sources supplied the name. To run a script named `config`, use `marsh ./config`.

Arguments after the marsh options go to the chosen shell unchanged:

```sh
marsh --shell bash script.sh
marsh --shell bash -l
```

## What keeps working

- Registered names (`claude`, `codex`, ...), from the shell or from any
  program it runs. Each starts a Kit job.
- `acp`, `mcp`, `ps`, and `top`. `ps` and `top` without `--marsh` run the
  system's.
- Every `marsh` subcommand: `run`, `split`, `join`, `splits`, `fanout`,
  `collect`, `jobs`, `status`, `context`, `results`, and `kit install`. A Kit
  you install later is available at once.
- The terminal and window resizing.
- Ctrl-C reaches a local command or a Kit job.
- Exit status. A shell killed by a signal exits 128+N.
- Your persistent home.
- Cleanup. As with Brush, everything the shell starts is cleaned up when the
  session ends.

Registered names, `acp`, `mcp`, `ps`, and `top` are links in a per-session
directory at the front of `PATH`. `$MARSH_SESSION_BIN` names it.

## What is Brush-only

- `split { ... } | join` and `fanout { ... } | collect`. Use the CLI forms:

  ```sh
  marsh split -b a='...' ::: b CMD ARG | marsh join
  marsh fanout -b a='...' ::: b CMD ARG | marsh collect
  ```

- `-b LABEL=STRING` strings always run in a fresh Brush shell. Your bash or
  zsh aliases and functions are not visible there.
- After `acp run AGENT &`, the session can take a moment to appear in
  `acp list`. `acp list --wait` waits for it.
- A file without a shebang runs by your chosen shell's rule, not with Brush.

## Startup files

Your guest home holds your `.bashrc`, `.zshrc`, and the rest. It is
`$MARSH_HOME/home`, and it is `$HOME` in the shell. marsh never writes these
files.

marsh starts the shell with a generated layer in a private per-session
directory. The layer sources your files.

### bash

marsh runs `bash --rcfile LAYER`. The layer sources `~/.bashrc`, then moves
the session command directory back to the front of `PATH`.

Noninteractive bash reads nothing and inherits the session `PATH`.

### zsh

marsh sets `ZDOTDIR=LAYER`.

- `.zshenv` in the layer sources yours and pins `PATH`. It reads yours from
  `$ZDOTDIR` if your environment sets it, else from `$HOME`.
- For interactive shells, the layer's `.zprofile` and `.zshrc` source yours.
- Then `ZDOTDIR` is handed back, so `.zlogin` and `.zlogout` are read from
  your directory directly.

### Login shells

A login bash (`-l`) reads your profile instead of the layer. If the profile
rewrites `PATH`, the session directory may no longer be first. To keep it
first, prepend `$MARSH_SESSION_BIN` to `PATH` at the end of your profile.

## Variables

| Variable | Value |
|---|---|
| `MARSH_SHELL` | the running shell |
| `MARSH_SESSION_BIN` | the session command directory |

## How it works

The guest marsh sets up the session and the command links as it does for
Brush. It then runs bash or zsh as a child and exits with the child's
status. `zsh` is installed in the shell image.

Code: `crates/marsh/src/shell_choice.rs`.

Next: [The shell](shell.md), [Commands and Kits](kits.md).
