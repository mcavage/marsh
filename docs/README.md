# marsh manual

marsh is a Bash-compatible shell for Apple Silicon Macs in which agent CLIs
are ordinary commands. Each call runs in a fresh container in a Docker Sandbox
microVM, with your project at its Mac path.

```sh
marsh split -n ::: a claude -p 'fix the failing test' \
               ::: b codex exec 'fix the failing test' | marsh join
```

```text
# split 8d5c8fdd1563: 2 branches; manifest …/out/manifest.json

== a (exited 0, kit:claude) ==
The test raced on a fixed port; it now asks the kernel for a free one.
-- a: 1 file (M net/dial_test.go); diff …/out/a/diff.patch --
…
```

Each agent worked on a private copy of the project. `join` printed what each
one said and changed, as a patch. Your files change only when you apply one.

Start with the [Quickstart](quickstart.md).

## Getting started

- [Install](install.md): requirements, Homebrew, curl, upgrade, uninstall
- [Quickstart](quickstart.md): split two agents, apply a patch, set up
  credentials

## Guides

Each guide does one job, with real output, costs, and failure modes
([all guides](guides/README.md)):

- [Race two agents on one bug](guides/race-two-agents.md): two attempts, each
  gated on your tests, and the patch you choose
- [Let a judge pick the winner](guides/add-a-judge.md): a third agent reads
  both results and names one
- [Review a change with a panel](guides/review-with-a-panel.md): one reviewer
  per concern, plus your checks, at once
- [Let an agent farm out work](guides/farm-out-work.md): an agent starts other
  agents from its shell, with an AGENTS.md snippet

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
