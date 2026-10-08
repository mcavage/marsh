# marsh manual

marsh is Bash with `split`, `fanout`, `join`, and `collect` for running
agents in parallel, each command in its own sandbox.

```sh
marsh split -n ::: a claude -p 'fix the failing test' \
               ::: b codex exec 'fix the failing test' | marsh join
```

```text
# split 8d5c8fdd1563: 2 branches; manifest …/out/manifest.json

== a (exited 0, shell-vm) ==
The test raced on a fixed port; it now asks the kernel for a free one.
-- a: 1 file (M net/dial_test.go); diff …/out/a/diff.patch --
…
```

Each agent works on a private copy of your project. `join` prints what each
one said and changed, as a patch. Your files do not change until you apply a
patch. Ctrl-C stops every job.

Each agent or tool runs in a fresh container in a Linux VM that Docker
Sandboxes (`sbx`) manages, with your project at the same path as on the Mac.
marsh runs on Apple Silicon Macs.

Start with the [Quickstart](quickstart.md).

## Getting started

- [Install](install.md): requirements, Homebrew, curl, upgrade, uninstall
- [Quickstart](quickstart.md): split two agents, apply a patch, set up
  credentials

## Using marsh

- [The shell](shell.md): where you are, startup files, Bash compatibility
- [Using bash or zsh instead](shells.md): GNU bash or zsh with your own startup files
- [Commands and Kits](kits.md): registered commands, adding your own
- [Agents running agents](agents.md): child jobs, limits, Ctrl-C
- [Split and join](split.md): parallel attempts on private copies
- [Fanout and collect](fanout.md): parallel work on the same files
- [Agent sessions (ACP)](acp.md): long-lived agent sessions for ACP clients
- [MCP](mcp.md): marsh as a server; publishing tools

## Reference

- [Configuration](configuration.md): environment, files, limits
- [Troubleshooting](troubleshooting.md): error messages, stuck jobs, reset,
  stop, quarantine
- [Security model](security.md): what each job can reach, and what marsh does
  not protect
- [FAQ](faq.md)
- `man marsh`: every command and option ([source](man/marsh.1.md))

## Working on marsh

- [CONTRIBUTING.md](../CONTRIBUTING.md): building, tests, the dev loop
- [Design documents](design/README.md): architecture, contracts, the TLA+
  model
- [GitHub Discussions](https://github.com/mcavage/marsh/discussions): Kits,
  registries, and split recipes in Show and tell
