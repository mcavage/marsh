# marsh manual

marsh is a shell for composing agents on Apple Silicon Macs. It opens your
project in Linux at its Mac path, runs registered agent commands in fresh
containers, and lets you combine their work with shell syntax.

## Getting started

- [Install](install.md): requirements, installation methods, upgrades, removal.
- [Quickstart](quickstart.md): one agent job on a project copy to find the
  repository's test command and verify its source.

## Guides

- [Make independent changes in parallel](guides/race-two-agents.md): give
  each task a project copy and bring the patches back yourself.
- [Review a change with a panel](guides/review-with-a-panel.md): ask two
  reviewers different questions about the same diff.
- [Let an agent farm out work](guides/farm-out-work.md): have an agent start
  one bounded child job from its shell tool.
- [Review split results in a pipeline](guides/add-a-judge.md): send results
  to another agent for an optional second opinion.
- [Set up your marsh home](guides/set-up-your-home.md): skills, dotfiles,
  shared and per-Kit MCP servers and extra mounts.

## Using marsh

- [The shell](shell.md): guest paths, startup files, Bash compatibility.
- [Using bash or zsh instead](shells.md): shell selection and startup files.
- [Commands and Kits](kits.md): packaged commands and your own registrations.
- [Agents running agents](agents.md): child jobs, launch limits, cancellation.
- [Split and join](split.md): project copies and patch handling.
- [Fanout and collect](fanout.md): parallel commands on shared files.
- [Agent sessions (ACP)](acp.md): long-lived sessions for ACP clients.
- [MCP](mcp.md): server setup and published pipelines.

## Reference

- [Configuration](configuration.md): settings and paths.
- [Troubleshooting](troubleshooting.md): refused jobs, stuck VMs, cleanup.
- [Security model](security.md): process authority and isolation boundaries.
- [FAQ](faq.md): costs, authentication, comparisons with other tools.
- `man marsh`: command options ([source](man/marsh.1.md)).

## Working on marsh

- [CONTRIBUTING.md](../CONTRIBUTING.md): build and test loop.
- [Design documents](design/README.md): architecture, contracts, TLA+ model.
- [GitHub Discussions](https://github.com/mcavage/marsh/discussions): Kits
  and recipes from other users.
