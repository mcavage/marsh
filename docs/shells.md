# Choosing your shell

Brush is the default shell. To use GNU Bash or zsh with startup files from
your [guest home](shell.md#where-you-are), select the shell VM's copy:

<!-- doc-test: host stdout="zsh" -->
```sh
marsh --shell zsh -c 'echo $MARSH_SHELL'
```

Registered commands (`claude`, `codex`, ...), `acp`, `mcp`, and the `marsh`
CLI keep working. The `split { }` and `fanout { }` forms are
[Brush-only](#what-is-brush-only).

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
`marsh --shell bash script.sh` runs a script; `marsh --shell bash -l` opens a
login shell.

## What keeps working

- Registered names (`claude`, `codex`, ...), from the shell or from any
  program it runs. Each starts a Kit job.
- `acp`, `mcp`, `ps`, and `top` remain available. Without `--marsh`, `ps` and
  `top` run the system binaries.
- The `marsh` CLI keeps working. A Kit you install later with `marsh kit install`
  is available at once.
- Terminal input and window resizing work as before.
- Ctrl-C reaches a local command or a Kit job.
- marsh returns the chosen shell's exit status, or 128+N if a signal kills it.
- Your guest home persists between sessions.
- Session cleanup still covers everything the shell starts.

Registered names, `acp`, `mcp`, `ps`, and `top` are links in a per-session
directory at the front of `PATH`. `$MARSH_SESSION_BIN` names it.

## What is Brush-only

- `split { ... } | join` and `fanout { ... } | collect`. Use
  [`marsh split`](split.md#from-plain-bash-cli-form) and
  [`marsh fanout`](fanout.md#from-any-shell) instead.
- `-b LABEL=STRING` strings always run in a fresh Brush shell. Your bash or
  zsh aliases and functions are not visible there.
- A file without a shebang runs by your chosen shell's rule, not with Brush.

## Startup files

Put your `.bashrc`, `.zshrc`, and other startup files in the guest home,
backed by `$MARSH_HOME/home` and mounted as `$HOME` in the shell. marsh does
not copy your Mac's dotfiles or modify these files.

marsh starts the shell with a generated layer in a private per-session
directory. The layer sources your files.

### bash

marsh runs `bash --rcfile LAYER`. For an interactive non-login shell, the layer
sources `~/.bashrc`, then moves the session command directory back to the
front of `PATH`.

Noninteractive Bash inherits the session `PATH`. marsh removes `BASH_ENV`
and `ENV` before starting the selected shell.

### zsh

marsh sets `ZDOTDIR=LAYER`.

- `.zshenv` in the layer sources yours and pins `PATH`. It reads yours from
  `$ZDOTDIR` if your environment sets it, else from `$HOME`.
- Interactive login shells read the layer's `.zprofile`, and interactive
  shells read its `.zshrc`. Each sources your corresponding file.
- Interactive shells return `ZDOTDIR` to your directory in `.zshrc`;
  noninteractive shells do so in `.zshenv`. Later startup files, including
  `.zlogin` and `.zlogout` for login shells, are read from your directory
  directly.

### Login shells

A login bash (`-l`) reads your profile instead of the layer. If the profile
rewrites `PATH`, the session directory may no longer be first. To keep it
first, prepend `$MARSH_SESSION_BIN` to `PATH` at the end of your profile.

## Variables

| Variable | Value |
|---|---|
| `MARSH_SHELL` | the running shell |
| `MARSH_SESSION_BIN` | the session command directory |

Next: [The shell](shell.md), [Commands and Kits](kits.md).
