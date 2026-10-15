# Keep a personal agent separate from packaged Pi

You can register an upstream-Pi-based Kit as `pix` without replacing marsh's
packaged `pi`. Keep public runtime content in the Kit and private settings/MCP
in your marsh home. This is an authoring recipe, not a released third-party Kit
or an authenticated-provider acceptance result.

The companion Pix prototype is being developed on `feat/public-pix-harness-20261015`
in the Pix repository. Its public source and local checks are separate from
marsh; this guide does not install it or publish an image.

## Separate commands, configuration and discovery

A native Kit's Dockerfile owns its entrypoint, environment and user; the Kit
descriptor declares provider credentials and network access. See
[native Kits](../../kits/README.md) and the [authoring outline](../plan/custom-agent-kit.md).

Use a separate Pi agent directory, for example `~/.pix-agent/agent`, for both
CLI and ACP. Pi supports `PI_CODING_AGENT_DIR` and expands `~` after HOME is set.
A portable image can declare this literal default:

```dockerfile
ENV PI_CODING_AGENT_DIR="~/.pix-agent/agent"
```

The entrypoint selects the invocation's HOME and leaves that value literal.
marsh filters unchanged image environment defaults when forwarding to children;
do not dynamically export a personal agent-directory value into unrelated
registered commands. Never put real keys in this variable or in image settings.

Call the image's Pi executable by **absolute path**. Bare `pi` resolves through
marsh's registered-command links and could start the separate packaged Pi as a
child instead. Point the ACP adapter at the personal entrypoint for the same
reason. Preserve `/run/marsh/context.md` injection and the normal MCP transport.

Keep public package resources inside the image and load them explicitly. Do not
put personal skills into the shared `.agents/skills` directory if packaged Pi
and Codex should not discover them. Prefer one canonical copy with guest-visible
relative links for any personal discovery directories. Preserve existing
settings/authentication/history and seed only absent first-use defaults.

## Register the personal command

Prepare and build the native source outside the project and guest-home mounts.
Merge a new entry into the scope's host-only `commands.json` (find its directory
with `marsh status`), leaving packaged names alone:

```json
{"pix": "/absolute/path/to/prepared/pix-kit"}
```

Use an existing native source directory, not a mutable image tag. Published
images use an actual immutable digest. A source path must remain outside mounts;
do not point this mapping at a checkout that the job is editing.

For ACP, add a separate `pix-session` with command `pix` and arguments
`["--acp"]`. Keep `pi-session` bound to packaged Pi. A local source can leave
`workload_digest` empty for daemon-startup pinning; a published immutable Kit
must declare its exact identity. See [ACP configuration](../acp.md#adding-agents).

Finish active work, close shells and run `marsh stop` before starting the new
configuration. Do not restart an active user session just to test registration.

## Keep business MCP private and command-scoped

Save private declarations in `~/.marsh/sbxenvs/pix.yaml`, **not** the shared
`~/.marsh/sbxenv.yaml`. The shell and packaged agents then do not automatically
load those additions. Keep credentials and organization-specific skills out of
the public source/build context. Local MCP commands run on the host; use their
normal host-side authentication, not keys baked into the agent image.

This scopes configuration/automatic loading, not filesystem confidentiality or
every explicit action an authorized caller can request. Jobs still share the
guest home, SBX's management tools remain available, and a permitted parent can
launch a Pix child with that child's own capabilities. Use separate homes or
spawn restrictions when a stronger boundary is required. See
[home setup](set-up-your-home.md) and [security](../security.md).

## Use marsh for orchestration

The personal package can provide role prompts, skills and display-only UI. It
does not need a second subagent scheduler or cancellation/watchdog service.
Teach it the actual registered commands, split/join, and fanout/collect from
[agents running agents](../agents.md). Preserve dependency order: a reviewer
that needs implementation changes runs after integration, not in a sibling
split fork. Join returns patches; it does not apply them.

Do not claim that role names enforce tools/models or that receipts prove model
identity, token usage or semantic memory. Use actual response metadata and
task-specific evidence. UI timers should clean up on shutdown and must not
silently cancel/retry jobs or require Pi-internal patches.

## Verify before switching or publishing

- In an isolated home, compare packaged Pi and the personal Kit: only the latter
  should discover the added package, defaults and UI.
- Check preservation of existing settings/logins, malformed-input failures,
  nonroot-readable resources, and clean CLI/RPC/PTY shutdown.
- Inspect the exported build context, not just the Git diff, for private skills,
  endpoints, account references, credentials and runtime files.
- Separately exercise real marsh children, split patch handling/cancellation,
  ACP and authorized provider/MCP calls. Offline discovery is not that proof.
- Before redistributing a derived image, perform its own
  [publication/legal checks](../design/kit-publication.md). Inherited base-image
  receipts do not certify a derivative.
