# Plan: bring your own agent Kit

Status: authoring plan, not a qualified third-party Kit recipe. marsh ships
Kits for Claude Code, Codex and Pi. The existing registry accepts custom native
Kits; the profile and packaging guidance below describes how to build one
outside this repository.

## Shape

An agent is a [native Kit v3 source](../../kits/README.md): a YAML descriptor, a
Dockerfile that owns the image, entrypoint, environment and user, and the
agent's locked dependencies. marsh adds nothing to the Kit schema. Register it
in the host control scope's `commands.json`
([configuration](../configuration.md)):

```json
{"myagent": "/Users/me/kits/myagent"}
```

or, for an immutable published image, `"myagent": "registry/repo@sha256:…"`.
Kit source must live outside the mounted project and guest home.

## Starting from the pi Kit

1. Copy `kits/marsh-pi` to a directory outside the project. Keep one lock
   (`package.json` / `package-lock.json`) for Pi, the agent's own packages,
   `pi-acp` and the MCP Gateway extension.
2. Keep the entrypoint's two modes: `--acp` runs the ACP adapter, anything
   else runs the CLI. Add the job context (`/run/marsh/context.md`) the way
   the pi Kit does (`--append-system-prompt`).
3. Keep the agent's state under the selected home, in its own directory, so it
   does not share state with plain `pi`.
4. Declare each model provider as a Kit credential capability backed by
   `sbx secret` (proxy-managed sentinels) with a matching network allowlist.
   No raw key goes into the image, the Kit source or marsh.
5. For an ACP session, add an entry to the host control scope's `agents.json`
   ([ACP](../acp.md#adding-agents)) with `"arguments": ["--acp"]`, as
   `pi-session` does in `packaging/agents.json`.

## Profiles

Choose the declarative profile files to copy into the Kit source or selected
home, review each one, and reject secret-bearing files. A read-only daemon-provided profile mount
would need a new mount kind and a `docs/model/` change; it is out of scope.

## Publishing

A local-only Kit does not go through marsh's publication gate. Before
redistributing one built on the DHI base, follow [Kit publication](../design/kit-publication.md) and
[supply chain](../design/supply-chain.md): pinned inputs, the image repair
step, collected license notices for every added package, and a final
`verify.py` gate.

## Risks

- Patches to Pi internals break on Pi upgrades; pin the Pi version and
  upgrade deliberately.
- Extensions that need integrations widen the Kit's egress for every job.
- Licensing for private packages: a published image needs real license texts.
