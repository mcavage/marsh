# Quickstart

Ask an agent to find your repository's test command and cite where it is
defined. The example uses one job on a private project copy and asks only
for a report.

Use an existing Git repository with a tracked `README.md`, opened at its
root. Split needs a checkout with a `.git` directory; linked worktrees and
submodule roots with a `.git` file are refused. To run the tests afterward,
you will need the project's Linux arm64 dependencies.

## Install and sign in

You need an **Apple Silicon Mac** and **Docker Sandboxes** (`sbx`) **0.47.0 or
newer**. In your **Mac terminal**, install both tools, sign in to Docker
Sandboxes, and configure the provider used below:

```sh
brew install docker/tap/sbx mcavage/tap/marsh
sbx login
sbx secret set anthropic
```

The last command configures Anthropic for the Claude job below.
[Install](install.md) has other installation methods.

In the same Mac terminal, change to your repository's root and run:

```sh
marsh
```

You are now in Linux at the same project path. The first shell launch may
download its VM template. First use of Claude also starts its Kit VM and
may download an image of about 600 MB. Allow a few minutes for these cold
starts; later launches reuse warm VMs.

## Make it your shell

marsh keeps its own home directory, backed by `~/.marsh/home` on your Mac
in the default scope. Your Mac's dotfiles stay untouched.

Vim and terminal Emacs are included. At the **marsh prompt**, open
`~/.bashrc` with `vim ~/.bashrc` or `emacs -nw ~/.bashrc`. You can also
open `~/.marsh/home/.bashrc` in your Mac editor.

For example, add an alias and set your prompt:

<!-- doc-test: shell stdout="ll=" -->
```sh
cat >> ~/.bashrc <<'EOF'
alias ll='ls -lah'
PS1='\w \$ '
EOF
source ~/.bashrc
alias ll
```

Brush reads `~/.bashrc` when you open an interactive shell. Your editor
settings live in the same guest home: `~/.vimrc` for Vim, or
`~/.emacs.d/init.el` for Emacs. See [Editor settings](shell.md#editor-settings)
for examples.

Your dotfiles survive VM removal. Jobs using this guest home can read them
too, along with any saved logins. Keep credentials out of
the files you copy from your Mac.

For other startup files or zsh configuration, see
[The shell](shell.md#startup-files) and [Choosing your shell](shells.md).

## Split a task

At the **marsh prompt**:

<!-- doc-test: shell replace=["claude -p 'Find the command this repository uses to run its tests. Start with README.md, then check the build files and CI configuration. Cite the defining file and line, and explain any required setup. If there is no test command, say what you checked. Do not edit files, run tests, or start other agents.'", "fixture identity"] stdout="-- tests: no changes --" -->
```sh
split {
  tests: claude -p 'Find the command this repository uses to run its tests. Start with README.md, then check the build files and CI configuration. Cite the defining file and line, and explain any required setup. If there is no test command, say what you checked. Do not edit files, run tests, or start other agents.'
} | join --keep
```

`split` gives the job a project copy, called a *fork*. Its container mounts
that fork rather than your original project. The copy includes uncommitted
work but omits Git-ignored files. Copy-on-write cloning shares unchanged
file data where the filesystem supports it.

`join` prints the job's exit status, report, and any captured changes.
Read the report, then open the cited source file yourself. Check whether
the command actually runs tests and whether it requires services, generated
files, or dependencies. If you have the Linux dependencies installed, try
that command at the marsh prompt. If not, the cited definition still gives
you something concrete to verify before setting up the environment.

Ctrl-C stops the foreground job and its children.

This task requests no edits, so there is normally no patch to apply. If
`join` shows a diff, inspect it; do not apply an unsolicited change. For a
task that changes code, continue with
[Make independent changes in parallel](guides/race-two-agents.md).

## Inspect and clean up

Still at the **marsh prompt**, inspect what ran:

```sh
marsh jobs --tree
marsh results
```

The tree shows parent-child relationships. Results record exit status,
timing, and whether marsh confirmed container deletion. They do not verify
the agent's conclusions.

`--keep` preserved this split's output. Once you have read it, replace
`YOUR_SPLIT_ID` below with the ID from the first join output line, remove
the saved copy, and leave the shell:

```sh
marsh splits rm YOUR_SPLIT_ID
exit
```

Back in your **Mac terminal**, stop the VMs:

```sh
marsh stop
```

Stop refuses while any marsh shells or jobs remain active. It preserves
your project, guest home, and job records.

## Run an agent directly

A direct command such as `claude -p 'Explain this repository'` mounts your
original project read-write. The agent can change or delete files despite
a request only to read. Packaged agents run without their own permission
prompts or internal sandboxes.

Your own shell commands, including tests, run unconfined in the shell VM.
A test started inside a split branch begins in its fork but can write
elsewhere in that VM. See the [security model](security.md).

## Agent credentials

The example uses a proxy-managed Anthropic credential. For Codex, configure
`sbx secret set openai` on your Mac before calling `codex exec`.
`sbx secret ls` lists stored credentials. OAuth options are in
[Signing in](kits.md#signing-in).

Docker Sandboxes keeps proxy-managed provider keys on your Mac and adds
them to outgoing requests; containers receive placeholders. Saved guest
logins are files in the shared guest home, backed by `~/.marsh/home` on
your Mac. Every job in the scope can read that home, including settings,
history, and login files. It is not your actual Mac home.

## Agents running agents

An agent can call registered commands from its shell tool. Each call starts
a child job with its own container and Kit credentials. Try
[Let an agent farm out work](guides/farm-out-work.md) for one bounded subtask.

## Run commands side by side

Use `fanout { ... } | collect` for commands that can safely share files.
It creates no project copies, so concurrent writers can interfere with each
other. See [Fanout and collect](fanout.md).

Continue with [Split and join](split.md) or [The shell](shell.md).
