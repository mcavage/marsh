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
installed marsh from the checkout; [the dev install](../docs/design/dev-install.md)
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
stdio adapter, anything else is the CLI as before. Each Kit declares that
adapter as `com.docker.sandbox/acp@1` (`/usr/local/bin/marsh-NAME --acp`), so
`marsh run --acp NAME` and `acp run NAME` start it; the declaration selects
the program and nothing else.

`patch-pi-acp.mjs` removes one behavior of `pi-acp` at image build: its
terminal startup banner (`pi vX`, `## Context`, `## Skills`) is not sent as an
agent message. The text stays in the `session/new` response
(`_meta.piAcp.startupInfo`). A `pi-acp` release that changes that code fails
the build.
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

Docker Sandboxes reports `SBX_CRED_ANTHROPIC_MODE` as `apikey` or `none`. A
Mac whose only Anthropic credential is a claude.ai login shows as `none`, so
the Claude entrypoint asks the proxy: it requests
`api.anthropic.com/api/oauth/profile` through the proxy with the access-token
sentinel, and only a direct 200 (not a redirect) means the proxy holds a login.
The job then runs with that sentinel in `CLAUDE_CODE_OAUTH_TOKEN`. A busy
answer (429 or 5xx) is asked once more. If there is still no clear answer, the
job starts signed out and prints one line on stderr saying so. Nothing is
written to the selected home, and an `apiKeyHelper` that an earlier API key
binding wrote there (`echo proxy-managed`) is removed. A helper of any other
value is left alone. The request adds about 0.3 seconds to a launch in that case
only, and at most 3 seconds if the proxy does not answer.

The job's own credential always wins over a login that `sbx` holds. If the job
has any of these, the proxy is not asked and the token is not set, also when
the mode is `oauth`:

- `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`,
  `ANTHROPIC_PROFILE` or `ANTHROPIC_FEDERATION_RULE_ID`;
- a cloud provider (`CLAUDE_CODE_USE_BEDROCK`, `_VERTEX`, `_FOUNDRY`);
- a custom endpoint, `ANTHROPIC_BASE_URL`, since the sentinel works only
  through the proxy, toward `api.anthropic.com`;
- a saved login, `.credentials.json` in `${CLAUDE_CONFIG_DIR:-$HOME/.claude}`;
- a Console API key that `/login` saved as `primaryApiKey` in Claude's global
  config, `${CLAUDE_CONFIG_DIR:-$HOME}/.claude.json`;
- an active Anthropic profile: `active_config` or `configs/default.json` in
  `${ANTHROPIC_CONFIG_DIR:-$HOME/.config/anthropic}`;
- an `apiKeyHelper` of its own, or one of these variables in the `env` of
  the user settings (`${CLAUDE_CONFIG_DIR:-$HOME/.claude}/settings.json`), of
  the project's `.claude/settings.json` or `.claude/settings.local.json`, or
  of settings given on the command line (`--settings`, as JSON or a file).

Any non-empty value of these counts, even a provider flag set to `0`. Only
regular files of up to 16 MiB are read: a settings path that is not a regular
file, such as a FIFO, is ignored rather than allowed to block the launch. The
check errs toward the job's own configuration and does not cover:

- managed settings (`/etc/claude-code/managed-settings.json` and its `.d`
  directory), which the Kit image does not ship;
- `--setting-sources`. A helper or variable in a scope that this option leaves
  out still counts, so the job starts with its own configuration and not with
  the login that `sbx` holds;
- an ACP-only home. ACP mode starts before the settings merge, so a stale
  `echo proxy-managed` helper is removed only by a CLI launch.

Claude's old outer-only block volumes were removed. Selected-home persistence
is the single state mechanism for nested jobs, avoiding hidden state that the
user cannot see or carry between worker VMs.

## ACP smoke

Every packaged Kit image with an ACP adapter (`com.docker.sandbox/acp@1`) must
pass `scripts/kit-acp-smoke.py` before any tag names it. release.yml pushes the
Kits by digest only (`make kit-publish KIT_UNTAGGED=1`), runs the smoke, and
only then applies the moving tag (`:release`, `:nightly`, or an older stable
release's version), the version tags and `:latest`. ci.yml runs it on every
pull request that changes a Kit (`make kits KIT_LOAD=marsh-validate && make
kit-acp-smoke`).

The script starts the adapter in the image the way a marsh ACP job does on
stock SBX: a job's `/run/marsh` (context, `job.json` and a `marsh task-mcp`),
the SBX proxy and MCP Gateway variables, and credential modes as if a key were
bound. It checks `initialize` (ACP v1), `session/new`, and a prompt that must
end with `stopReason: end_turn` after the agent called the model with its
apikey-mode sentinel. One local fake server stands in for the proxy, the model
endpoint and the MCP Gateway, so no credentials are needed. Not covered: SBX's
real proxy and credential injection, the Kit's network policy, real
authentication and OAuth, running a task, and Pi's built-in providers. To check
a published image:

```sh
python3 scripts/kit-acp-smoke.py codex=docker.io/mcavage/marsh-codex:latest
```

## Kits that publish themselves

Not every directory here ships inside marsh. `make kits` and `make kit-publish`
cover only the packaged Kits listed in `packaging/commands.json`. The others
build and publish on their own, from a workflow of their own
(`.github/workflows/kit-NAME.yml`): it builds the Kit, smoke-tests it, and moves
its `:stable` tag on Docker Hub, which marsh follows for automatic updates.
A pull request that touches one builds and tests it and pushes nothing.

| Directory | Image | Workflow |
|---|---|---|
| `marsh-hermes` | `docker.io/mcavage/marsh-hermes:stable` | [kit-hermes.yml](../.github/workflows/kit-hermes.yml): follows Hermes Agent's releases every six hours |
| `marsh-tools` | `docker.io/mcavage/marsh-tools:stable` | [kit-tools.yml](../.github/workflows/kit-tools.yml): `gog` and `bamboohr` in one Kit; follows gog's releases every six hours |
| `marsh-pix` | `docker.io/mcavage/marsh-pix:stable` | [kit-pix.yml](../.github/workflows/kit-pix.yml): follows `marsh-pi:latest` and the Pix content, which stays in [mcavage/pix](https://github.com/mcavage/pix) and is checked out at every run |

## `marsh-docker-cli`: the docker CLI for any agent image

`marsh-docker-cli` is a v3 **mixin**, not an agent. Its image holds only
`/usr/local/bin/docker` (Docker's official static CLI, 29.9.3) and the Compose
plugin (`/usr/local/lib/docker/cli-plugins/docker-compose`, v5.8.0), each
downloaded from the vendor at build time and checked against a pinned SHA-256
(a mismatch fails the build; move a pin by changing the version and both
digests together). The Compose version is the services engine's
(`docker-compose-plugin` in `packaging/svc-engine/Dockerfile`);
`tests/release/test_compose_pins.py` fails until every copy matches it and
both digests are upstream's for that version, as recorded in
`tests/release/compose-release-digests.txt`. A job
of a project with Docker on already has `DOCKER_HOST` set to the project's
private endpoint, so an agent image that ships these two files can use it.

Build it (arm64 and amd64) and publish it like any Kit:

```sh
docker buildx build -f kits/marsh-docker-cli/marsh-docker-cli.yaml \
  -t docker.io/YOU/marsh-docker-cli:1 --platform linux/arm64,linux/amd64 --push kits/marsh-docker-cli
```

It is a mixin for `sbx` (`sbx run KIT --kit docker.io/YOU/marsh-docker-cli:1`,
or `sbx kit add SANDBOX REFERENCE`, which adds it to a sandbox that already
exists), and a Kit author can list it in a kit set beside an agent.

The packaged agent Kits (`marsh-claude`, `marsh-codex`, `marsh-pi`) and `marsh-shell` do not need it: they already ship the docker CLI (`docker-ce-cli`) and now also the Compose plugin, fetched by `packaging/image-base/docker-compose.sh` (pinned version and SHA-256 per architecture, checked at build; their smoke stage runs `docker --version && docker compose version`). `marsh-pix` and `marsh-hermes` are built on `marsh-pi` and `marsh-shell`, so they pick it up when rebuilt on a newer base.

**marsh does not apply it to a job's Kit VM by itself.** A marsh job runs in a
fresh container of the Kit's own image inside the Kit VM, and `sbx kit add`
changes the sandbox's container, not those job containers, so there is no
Kit-mechanism step marsh could take when a project turns Docker on. An agent
image that lacks `docker` gets the files in its own Dockerfile instead:

```dockerfile
COPY --from=docker.io/YOU/marsh-docker-cli:1 /usr/local/ /usr/local/
```
