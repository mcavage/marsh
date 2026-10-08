# marsh — marshal your agents

marsh is a Bash-compatible shell for Apple Silicon Macs. It opens your project
in a Linux VM at the same path. Each agent or tool you call runs in a fresh
container inside its own Docker Sandbox microVM. `split` runs agents on private
copies of your repo, and `join` brings back their patches.

```console
$ cd ~/src/my-project
$ marsh
marsh-0.5$ git diff | codex exec 'review this diff' > review.txt
marsh-0.5$ marsh split -n ::: a claude -p 'fix the flaky test' \
                          ::: b codex exec 'fix the flaky test' | marsh join --keep

== a (exited 0, kit:claude) ==
The test raced on a shared port; it now asks the kernel for a free one.
-- a: 1 file (M test/net_test.go); diff …/out/a/diff.patch --
...
== b (exited 0, kit:codex) ==
Added a retry around the dial and a longer timeout.
-- b: 2 files (M net/dial.go, M test/net_test.go); diff …/out/b/diff.patch --
...
marsh: join: kept /Users/you/src/my-project/.marsh/split/8d5c8fdd1563 (join --keep); inspect, then: marsh splits rm 8d5c8fdd1563
marsh-0.5$ git apply .marsh/split/8d5c8fdd1563/out/a/diff.patch
```

VMs come from stock [Docker Sandboxes](https://docs.docker.com/ai/sandboxes/)
(`sbx`). The packaged Kits and the shell image are built on Docker Hardened
Images. `msh` is the same program.

## Install

You need an Apple Silicon Mac and Docker Sandboxes 0.45.0 or newer, signed in.

```sh
brew install docker/tap/sbx && sbx login
brew install mcavage/tap/marsh
```

Or, without Homebrew for marsh itself:

```sh
curl -fsSL https://runmar.sh/install | sh
```

The script verifies a release from GitHub and installs into `~/.local`. The
Homebrew tap, the installer, and GitHub Releases are live. To build from
source instead, use `make install` (or `make dist-local` for a tarball).
See [Install](docs/install.md) for options, upgrading, and removal.

## Quickstart

```sh
cd ~/src/my-project
marsh                                   # a Linux shell, in your project
claude -p 'what does this repo do?'     # an agent, in a fresh container
marsh jobs --tree                       # what ran, and what it started
marsh stop                              # remove marsh's VMs
```

- Pipelines, job control, scripts, and `make` work as in Bash. Your
  project is mounted read/write at its Mac path, and your guest home persists.
- `claude`, `codex`, `pi`, and `shell` are registered commands. Each call runs
  in a new nonroot container in that Kit's warm VM, and the container is
  deleted when it exits. Add your own as Docker Sandboxes Kits.
- An agent can call another agent. The call becomes a child job in the same
  tree, under the same limits, and one Ctrl-C stops them all.
- `split` runs branches on private forks of your working tree. `join` prints
  each branch's output and a patch for `git apply`. Your files change only when
  you apply one.
- `mcp publish` turns a pipeline into an MCP tool. Agent Kit VMs created
  after the publish load it, `mcp load` adds it to a running one, and
  `marsh mcp install-published` gives it to Codex on your Mac. `acp publish`
  makes a running agent session a tool for one target. `marsh mcp install` lets an MCP
  client drive marsh itself.
- `marsh --shell zsh` (or `bash`) keeps all of this except the `split { }` and
  `fanout { }` syntax ([shells](docs/shells.md)).

The [Quickstart](docs/quickstart.md) walks through each.

## Documentation

- [Manual](docs/README.md): install, the shell (and [choosing bash or
  zsh](docs/shells.md)), Kits, agents, split, fanout, ACP, MCP,
  configuration, troubleshooting, security, FAQ
- `man marsh`
- [Design documents](docs/design/README.md)
- Site: [runmar.sh](https://runmar.sh)

## Status

marsh is early. Its author uses it daily with `sbx` 0.46. Expect rough edges
and changes. Bash compatibility has
[known gaps](docs/shell.md#bash-compatibility). Binaries are not yet signed.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Security reports:
[SECURITY.md](SECURITY.md). Conduct: [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## License

Apache License 2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE). Brush is MIT
licensed; see [THIRD_PARTY.md](THIRD_PARTY.md).

## Thanks

marsh's shell is built on a patched [Brush](https://github.com/reubeno/brush):
upstream revision `1389a8e` plus the 49 patches in
[docs/upstream/brush](docs/upstream/brush/README.md). 34 of them fix Bash
compatibility: job control, signals and traps, `wait`, aliases, namerefs,
subshells, globstar, redirections, and startup files. The other 15 add marsh's
registered commands, `split`/`join`, and `fanout`/`collect`. Thanks to Reuben
Olinsky and the Brush contributors, and to the
[Docker Sandboxes](https://docs.docker.com/ai/sandboxes/) team.
