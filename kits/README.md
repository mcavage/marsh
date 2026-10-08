# Native Docker Sandbox Kits

Each child directory is a Docker Sandbox Kit v3 workload source. The YAML
descriptor uses the stock `docker/sandbox-kit:3` BuildKit frontend, and the
companion Dockerfile owns the workload image configuration, including its
entrypoint, environment, and user.

marsh does not define or translate a Kit schema. `make kits` asks the stock v3
frontend to validate and build every source for Linux ARM64. `make kit-publish
KIT_REPOSITORY_PREFIX=docker.io/YOU` pushes the native images and writes
`target/kit-release/commands.json` with immutable digests. Install those
mappings using `make install KIT_COMMANDS=target/kit-release/commands.json`.

At runtime, a shell command maps to either an immutable native Kit OCI
reference or a native v3 source directory relative to the registry file:

```json
{"claude":"kits/marsh-claude","shell":"kits/marsh-shell"}
```

Packaged defaults install their source under `libexec/marsh/kits`. User
overrides live in the owner-only host control scope's `commands.json`. Absolute
Kit source paths and immutable OCI digests are supported. Relative paths are
resolved from that registry file. Local paths must name an existing directory.
The mapping does not repeat an image, argv, environment, capability, policy, or
resource field: those remain part of the one native Kit artifact. Mutable tags
are rejected by runtime discovery.

Registered source directories must be outside the mounted project and guest
home. marsh refuses overlapping mounts to keep trusted Kit declarations out of
guest write access. Edit source in your checkout, then install or copy a
candidate snapshot into a separate host-owned directory before registering
it. For these packaged Kits, run `make install` on the Mac and launch the
installed marsh from the checkout; [the development guide](../docs/design/self-development.md)
explains the complete loop.

The directory basename is the Kit identity for local v3 source. Packaged
sources use names such as `marsh-claude` so stock SBX does not confuse them
with or reject them against its built-in agents. The shell command remains the
independent key in `commands.json`, so `claude` maps to `kits/marsh-claude`.
The local-source runtime path has an unresolved image-identity mismatch
between stock SBX assembly and a second Buildx build. Published immutable
digests avoid that mismatch: stock SBX and the worker's Docker Engine pull the
same artifact. Use the published mapping for acceptance and release.

There is one Kit per agent. `marsh-claude`, `marsh-codex`, and `marsh-pi`
each ship the agent's CLI and its pinned ACP adapter (`package.json` /
`package-lock.json`): the official `@agentclientprotocol/claude-agent-acp`
and `@agentclientprotocol/codex-acp`, and the community `pi-acp`. The
entrypoint chooses the mode from its first argument: `--acp` runs the ACP v1
stdio adapter, anything else is the CLI as before. `packaging/agents.json`
maps `claude-session`, `codex-session`, and `pi-session` to those commands
with `"arguments": ["--acp"]`; the daemon appends the tail to the Kit's own
entrypoint like typed arguments, and the declaration can change nothing else.
ACP sessions therefore share the CLI's warm Kit VM, network allowlist, and
credential binding. Claude's adapter drives its Agent SDK's own native Claude
Code build (the SDK pins it) through `claude-cli.sh`. Codex ACP keeps its
selected-home configuration in `.codex-acp`, separate from the CLI's `.codex`.
Pi ACP starts Pi in RPC mode through the same entrypoint, so both modes load
the same trusted project resources and MCP Gateway extension.

`marsh-shell` is the credential-free generic command workload. Its stock DHI
shell image runs `/bin/sh`: without arguments it reads nonterminal stdin as a
script or becomes interactive on a PTY, while arguments such as `-c` retain
their ordinary POSIX shell meaning. Repeated `shell` invocations share this
exact Kit VM but each invocation still runs in a fresh nested container.

## Agent sandboxes

The job container is the sandbox boundary. Agent Kits disable the agent's own
sandbox and approval prompts from the start, since bubblewrap cannot create
namespaces in a job container: Claude runs with
`--dangerously-skip-permissions --settings '{"sandbox":{"enabled":false}}'`
(command-line settings outrank a project's `sandbox.enabled`, and nothing is
written), Codex with `--dangerously-bypass-approvals-and-sandbox` for both
`codex` and `codex exec` (a new `config.toml` also says `approval_policy =
"never"`, `sandbox_mode = "danger-full-access"`). In ACP mode, Claude's Agent
SDK runs Claude Code through `claude-cli.sh`, which merges
`sandbox.enabled=false` into the SDK's `--settings` (or adds it); Codex ACP
exports `INITIAL_AGENT_MODE=agent-full-access` unless the caller set it,
because codex-acp ignores `config.toml` and otherwise starts sessions in
workspace-write. Pi has no OS-level command sandbox.

## Selected home

Each workload image owns a small entrypoint that consumes the adapter-provided
`MARSH_SELECTED_HOME`. During stock Kit lifecycle this names a daemon-owned
neutral workspace, never a session's selected home. For a nested job it names
the invocation's natural mounted home path. The entrypoint exports it as
`HOME`, creates only that tool's missing documented configuration, and
preserves existing settings, authentication, skills, and history. The adapter
never copies lifecycle workspace or `/home/agent` content into a selected
home. Persistent shells therefore write agent state into `MARSH_HOME`;
ephemeral shells use their throwaway backing while sharing the same warm Kit
VM.

Claude's native v3 OAuth `credentialFile.path` cannot interpolate an arbitrary
runtime home. It remains useful when the workload runs directly under stock
SBX at its image-declared home, but marsh never copies that rendered file into
the selected home because doing so could move credential material across the
trust boundary. Selected-home jobs use the stock-SBX proxy and non-secret mode
sentinels admitted by the adapter. Codex and Pi likewise receive proxy-managed
state, never raw keys.

Claude's old outer-only block volumes were removed. Selected-home persistence
is the single state mechanism for nested jobs, avoiding hidden state that the
user cannot see or carry between worker VMs.
