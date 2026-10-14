# marsh

**A shell for composing agents.** marsh opens a Bash-compatible Linux shell
on your Apple Silicon Mac. `claude`, `codex`, and `pi` work as commands, so
you can put agents in a pipeline, call them from a script, or give each one
a separate copy of your project with `split`.

For a project with a CLI and a paginated API, you might divide the work
like this. Adapt the prompts to your code, after configuring Claude as
shown under [Install](#install):

<!-- doc-test: shell replace=["split {\n  cli: claude -p 'Add JSON output to the existing list command. Preserve text output by default. Change only CLI code and tests.'\n  api: claude -p 'Test the existing API pagination contract for the final page and invalid cursors. Change only API code and tests.'\n} | join --keep", "split {\n  cli: fixture project-write notes.txt gamma\n  api: fixture project-write pagination.txt cursor\n} | join --keep"] stdout="== api (exited 0, shell-vm) ==" -->
```sh
split {
  cli: claude -p 'Add JSON output to the existing list command. Preserve text output by default. Change only CLI code and tests.'
  api: claude -p 'Test the existing API pagination contract for the final page and invalid cursors. Change only API code and tests.'
} | join --keep
```

Both branches start from the same project snapshot and run in parallel.
`join` shows their output and patches; you decide what to apply. The
[parallel guide](docs/guides/race-two-agents.md) adds per-branch tests and
checks the combined change. For a first session, the
[Quickstart](docs/quickstart.md) runs one inspection job on your repository.

## Work in the shell

Your project appears at the same absolute path as on your Mac. Ordinary shell
tools run in a Linux VM. Each registered agent call gets a fresh nonroot
container in its Kit's VM, which stays warm between calls. Split uses
copy-on-write cloning where the filesystem supports it, so unchanged files
can share disk space.

`split { ... } | join` is for work that needs separate project copies.
`fanout { ... } | collect` runs commands together on the same files, such as
checks that do not interfere with one another. Pipes, redirection, `&&`, and
scripts work as they do in a shell.

An agent can use the same commands from its own shell tool. Its calls become
child jobs, each in a separate container, under one job tree. Ctrl-C stops
the whole foreground tree. `marsh jobs --tree` shows the relationships;
`marsh results` records exit status, timing, and whether marsh confirmed
container deletion.

You can also publish a pipeline as an MCP tool or expose a running ACP agent
session to a client. See [MCP](docs/mcp.md) and [ACP](docs/acp.md).

The default shell is [Brush](https://github.com/reubeno/brush), with
[Bash compatibility gaps](docs/shell.md#bash-compatibility). It is not Bash.
If you prefer, `marsh --shell zsh` or `marsh --shell bash` selects the VM's
own shell. Registered commands still work; use the `marsh split` and
`marsh fanout` CLI forms instead of Brush's block syntax. `msh` is the short
command name for `marsh`.

## Install

You need an Apple Silicon Mac and Docker Sandboxes (`sbx`) 0.47.0 or newer,
signed in. Check the installed version with `sbx version`.
From your **Mac terminal**, install both programs, sign in to Docker
Sandboxes, and configure one provider:

```sh
brew install docker/tap/sbx mcavage/tap/marsh
sbx login
sbx secret set anthropic
```

That provider is enough for the Claude examples above and in the Quickstart.
Codex uses `sbx secret set openai`. Calls bill through your provider account.

To install marsh without Homebrew, install `sbx` first, then run
`curl -fsSL https://runmar.sh/install | sh`. The installer uses `~/.local`.
[Install](docs/install.md) covers other methods, upgrades, and removal.

## First run

From your Mac terminal, enter an existing project and run `marsh`. At the
marsh prompt, you are in Linux at the same project path. A first shell or
agent launch can take a few minutes while Docker Sandboxes downloads images
and starts VMs. Later calls reuse the warm VMs.

Follow the [Quickstart](docs/quickstart.md) to run an agent on a project copy,
find your repository's test command, and check where it is defined.

When finished, run `exit` to leave marsh, then `marsh stop` from your Mac
terminal. Stop refuses while any marsh shells or jobs remain active. It
removes marsh's VMs without deleting your project or saved guest home.

## Know what can change

An agent called directly can edit or delete your project. In a split, its
container mounts only that branch's project copy, along with the shared
guest home and read-only Git metadata. Your own shell code, including tests
in a split branch, runs unconfined in the shell VM.

The guest home is backed by `~/.marsh/home` on your Mac, not your whole Mac
home. Jobs share its settings, history, and saved logins. Docker Sandboxes
keeps proxy-managed provider keys out of containers; that does not protect
login files saved in the guest home from other jobs. The packaged agents'
permission prompts and internal sandboxes are disabled. Read the
[security model](docs/security.md) before using sensitive projects.

Split copies include uncommitted work but omit Git-ignored files. A linked
worktree or submodule root with a `.git` file is refused; use a checkout
with a `.git` directory. See [Split and join](docs/split.md) for details.

## Keep going

- [Make independent changes in parallel](docs/guides/race-two-agents.md)
- [Review a change with a panel](docs/guides/review-with-a-panel.md)
- [Let an agent farm out work](docs/guides/farm-out-work.md)
- [Manual](docs/README.md) and `man marsh`

## Limits

marsh runs on Apple Silicon Macs, with Linux arm64 guest tools. Builds that
require Xcode or macOS frameworks cannot run in the guest. At most eight
jobs run at once per daemon; a job tree has at most four levels, four live
children per job, and 64 jobs in total.

marsh is early, and its release binaries are unsigned.

## Documentation

The site is [runmar.sh](https://runmar.sh); agent-readable documentation starts
at [runmar.sh/llms.txt](https://runmar.sh/llms.txt). For implementation details,
see the [design documents](docs/design/README.md).

## Contributing

Contributions: [CONTRIBUTING.md](CONTRIBUTING.md). Security reports:
[SECURITY.md](SECURITY.md). Conduct: [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## License

Apache License 2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE). Brush is MIT
licensed; see [THIRD_PARTY.md](THIRD_PARTY.md).
