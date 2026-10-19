# Build your own agent on marsh

Package your own agent (your skills, prompts, defaults and tools) as a Kit and
run it next to marsh's packaged `pi`. [Pix](https://github.com/mcavage/pix/tree/main/marsh)
is a working example: upstream Pi plus a set of public skills, published as
`docker.io/mcavage/marsh-pix`.

## The Kit

Start from a Pi-based image and add your content. Three things matter:

- **Its own state directory.** Set `ENV PI_CODING_AGENT_DIR="~/.pix-agent/agent"`
  (literally, with the `~`), so its settings and sessions don't mix with packaged
  Pi's `~/.pi/agent`. Seed default settings only when none exist.
- **Call Pi by absolute path** in the entrypoint and the ACP adapter. A bare
  `pi` resolves to marsh's packaged Pi.
- **Load skills explicitly** from inside the image. Don't put them in the shared
  `.agents/skills` directory, or packaged Pi and Codex will pick them up too.

No keys go in the image. Declare which providers it uses in the Kit file, and
SBX inserts the keys at those providers' API hosts. See [native Kits](../../kits/README.md).

## Register it

Find your control directory with `marsh status --json` (`control_home`) and add
the command to its `commands.json`, keeping existing entries:

```json
{"pix": "docker.io/mcavage/marsh-pix@sha256:<digest>"}
```

For a local build, use the absolute path of the prepared Kit directory instead,
outside any project you open in marsh. For ACP, add a separate `pix-session`
entry with `"command": "pix", "arguments": ["--acp"]` and leave `pi-session`
alone ([ACP configuration](../acp.md#adding-agents)). Run `marsh stop` before
the new mapping takes effect.

## Add private tools and skills

Keep anything organization-specific out of the public image:

- **A private layer.** Build a second Kit `FROM` the public image that adds your
  private skills, and register it as `pix` instead.
- **Tools.** MCP servers only this agent should load go in
  `~/.marsh/sbxenvs/pix.yaml`, not the shared `sbxenv.yaml`. Small CLIs can be
  published as typed MCP tools (`marsh-mcp export-serve`), each running as a Kit
  job whose API key SBX injects at the network proxy.

This keeps other agents from loading those tools automatically. It is not a
security boundary: jobs share the same guest home. Use a separate marsh home if
you need one ([home setup](set-up-your-home.md), [security](../security.md)).

## Orchestration

Your agent doesn't need its own subagent runner. Teach it marsh's commands:
fanout/collect for independent work, split/join for parallel edits, and
sequential calls when one step needs another's result ([agents running agents](../agents.md)).
Join returns patches; it does not apply them.

## Before you share it

- Compare it with packaged Pi in a fresh home: only your agent should see its
  skills and defaults.
- Check the exported build context, not just the Git diff, for private skills,
  account names, endpoints or keys.
- Run a real model turn and a real child job; listing tools offline proves
  little.
- If you redistribute a derived image, run its own
  [publication checks](../design/kit-publication.md).
