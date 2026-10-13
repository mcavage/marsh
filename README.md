# marsh

**Code mode for agents.** marsh is a Bash-compatible shell for Apple Silicon
Macs in which `claude`, `codex`, and `pi` are commands. Each call runs in a
fresh container inside a Docker Sandbox microVM, with your project at the same
path as on your Mac. You combine agents with pipes, `&&`, scripts, and two
additions, `split` and `fanout`. An agent running in marsh can do the same from
its own shell tool: it writes a few lines of shell to hand work to other
agents, with no orchestration framework.

Two agents fix the same bug, each on a private copy of the project, and each
copy runs the tests:

```console
$ cd ~/src/slug
$ marsh
marsh-0.5$ split {
>   claude: claude -p "make test fails. Fix slug.py without editing the tests. Reply in one line." && make test
>   codex:  codex exec "make test fails. Fix slug.py without editing the tests. Reply in one line." && make test
> } | join --keep
# split 3c8145fd9046: 2 branches; manifest …/out/manifest.json

== claude (exited 0, shell-vm) ==
Fixed `slug.py:6` to collapse runs of non-alphanumerics into one hyphen and strip leading/trailing hyphens — all 3 tests pass (no children spawned; the edit was a one-liner).
python3 -m unittest -q
-- claude: 1 file (M slug.py); diff …/out/claude/diff.patch --
…
== codex (exited 0, shell-vm) ==
Fixed `slug.py` to collapse separators and trim hyphens; all 3 tests pass, tests unchanged, no child agents used.
python3 -m unittest -q
-- codex: 1 file (M slug.py); diff …/out/codex/diff.patch --
…
split: kept …/.marsh/split/3c8145fd9046 (join --keep); inspect, then: marsh splits rm 3c8145fd9046
marsh-0.5$ git apply .marsh/split/3c8145fd9046/out/codex/diff.patch
```

Each agent edited its own copy. `git apply` copied the chosen patch into your
working tree. The `make test` in each branch is your own command and runs in the
shell VM, unconfined. `…` marks trimmed output; the run took 12 seconds with
warm VMs.

## What it does

- **Agents are commands.** `git diff | codex exec 'review this diff' > review.txt`
  works, and so does a Makefile target that calls `claude -p`. Each call is a
  new nonroot container in that agent's warm VM, deleted when the call exits.
- **`split { ... } | join`** runs branches at once, each on a private copy of
  your working tree. `join` prints each branch's exit status, output, and a
  patch for `git apply`.
- **`fanout { ... } | collect`** runs branches at once on your real files,
  with no copies. Use it for commands that can share a working directory. A
  "do not edit files" prompt is a request, not a guard.
- **Agents can run agents.** Inside a job, the same commands are on `PATH`. A
  call becomes a child job in one tree, under fixed limits, and one Ctrl-C
  stops the tree. See [Let an agent farm out work](docs/guides/farm-out-work.md).
- **A record of what ran.** `marsh jobs --tree` shows who started what.
  `marsh results` shows each job's exit status, wall time, and whether its
  container was confirmed deleted.
- **MCP and ACP.** `mcp publish` makes a shell pipeline an MCP tool for agents.
  `acp publish` does the same for a running agent session.

The shell is [Brush](https://github.com/reubeno/brush), which runs Bash
syntax with [known gaps](docs/shell.md#bash-compatibility). `marsh --shell zsh`
or `--shell bash` runs those shells instead and keeps everything above except
the `split { }` and `fanout { }` block syntax. `msh` is the same program.

## Why not worktrees and a script?

You can get part of this with `git worktree` and `&`. marsh adds:

- Each agent call runs in its own container in a microVM that mounts the
  project (in a split, only that branch's copy), not your Mac home.
- Credentials are added by the Docker Sandboxes proxy. Containers see
  placeholders, not your keys.
- Split copies include uncommitted changes. You get a patch per branch, and
  no branches or worktrees are left in your repo.
- Agents an agent starts are tracked in one tree, held to fixed limits, and
  stopped together by one Ctrl-C.
- Each job leaves a record that says whether its container was confirmed
  deleted.

## What changes for your agents

- They are the Linux CLI versions pinned in marsh's Kits, not the ones on
  your Mac.
- They do not see your Mac's `~/.claude` or `~/.codex`. Their settings,
  logins, and history live in marsh's guest home, `~/.marsh/home`. Project
  files such as `CLAUDE.md` and `AGENTS.md` are visible as usual.
- They authenticate with keys you store with `sbx secret set`, or an OAuth
  login that `sbx` holds.
- They run with their permission prompts and their own sandboxes turned off.
  The container is the sandbox. An agent you call directly can edit or delete
  project files; run it in a `split` to get a patch instead.

The [security model](docs/security.md) lists what each job can reach.

## Install

You need an Apple Silicon Mac and [Docker Sandboxes](https://docs.docker.com/ai/sandboxes/)
(`sbx`) 0.45.0 or newer, signed in.

```sh
brew install docker/tap/sbx mcavage/tap/marsh
sbx login
sbx secret set anthropic     # for claude and pi
sbx secret set openai        # for codex
```

Without Homebrew for marsh itself, install `sbx` as above, then run
`curl -fsSL https://runmar.sh/install | sh`. It installs into `~/.local`.
[Install](docs/install.md) covers upgrades, removal, the nightly channel, and
building from source.

## First run

```sh
cd ~/src/your-project
marsh                                   # a Linux shell at the same path
claude -p 'what does this repo do?'     # an agent, in a fresh container
marsh jobs --tree                       # what ran
marsh stop                              # remove marsh's VMs
```

The first call to each agent boots its VM and downloads its image, about
600 MB, which takes a few minutes. Later calls start in seconds.

Then follow a guide:

- [Race two agents on one bug](docs/guides/race-two-agents.md)
- [Let a judge pick the winner](docs/guides/add-a-judge.md)
- [Review a change with a panel](docs/guides/review-with-a-panel.md)
- [Let an agent farm out work](docs/guides/farm-out-work.md)

## Limits

- Apple Silicon Macs only. marsh needs `sbx` and does not run on Intel Macs
  or Linux.
- Commands run on Linux arm64. A build or test that needs macOS (Xcode, macOS
  frameworks) does not run in marsh.
- At most 8 jobs run at once per daemon. A job tree is at most 4 deep, with 4
  live children per job and 64 jobs in total.
- Every agent call is a full model run on your account. marsh has no
  benchmarks. It does not make an agent better; it runs several and shows you
  what each did.
- marsh is early. Binaries are not signed. Expect changes.

## Documentation

- [Manual](docs/README.md) and `man marsh`
- [Security model](docs/security.md): what each job can reach
- [Design documents](docs/design/README.md)
- Site: [runmar.sh](https://runmar.sh). For agents:
  [runmar.sh/llms.txt](https://runmar.sh/llms.txt)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Security reports:
[SECURITY.md](SECURITY.md). Conduct: [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).

## License

Apache License 2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE). Brush is MIT
licensed; see [THIRD_PARTY.md](THIRD_PARTY.md).
