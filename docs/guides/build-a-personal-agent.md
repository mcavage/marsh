# Build your own agent

marsh runs any agent CLI as a command in its own sandbox: Claude Code, Codex,
OpenCode, a claw, or something you wrote. Your own agent is one of those plus
your choices: default model, skills, instructions and tools. You package it
once as a **Kit**, and then `myagent` works in every marsh shell, can start
other agents as child jobs, and never sees an API key.

This guide builds `myagent` from [OpenCode](https://opencode.ai). Every step
says what to change for a different agent.

You need marsh 0.1.8 or later, Docker Desktop with Buildx, and a model key
stored in SBX (`sbx secret set anthropic`).

## What you'll make

A folder outside any project you open in marsh:

```text
myagent/
├── myagent.yaml        # the Kit: network, credentials, skills folder
├── Dockerfile          # the agent on marsh's base image
├── entrypoint.sh       # starts the agent with your settings
├── config/
│   └── opencode.json   # your defaults
└── skills/
    └── release-notes/
        └── SKILL.md    # a skill of your own
```

## 1. The image

```dockerfile
FROM dhi.io/debian-base:trixie-dev AS download
ARG OPENCODE_VERSION=1.19.3
ADD https://github.com/anomalyco/opencode/releases/download/v${OPENCODE_VERSION}/opencode-linux-arm64.tar.gz /tmp/opencode.tar.gz
RUN tar -xzf /tmp/opencode.tar.gz -C /tmp opencode

FROM docker.io/mcavage/marsh-shell:0.1.8
COPY --from=download /tmp/opencode /usr/local/bin/opencode
COPY config /opt/myagent/config
COPY skills /opt/myagent/config/skills
COPY --chmod=0755 entrypoint.sh /usr/local/bin/myagent
ENTRYPOINT ["/usr/local/bin/myagent"]
```

Start from `marsh-shell`, at the version of your marsh. The same image boots
the sandbox your jobs run in, which needs the Docker Engine and plumbing that
`marsh-shell` provides. It's built on a [Docker Hardened Image](https://docs.docker.com/dhi/)
and has no package manager, so fetch or build your agent in a separate stage
and copy the result in. Use a hardened image there too: the `-dev` variants,
like `dhi.io/debian-base:trixie-dev`, include the usual build tools and are
free with the Docker sign-in you already use for `sbx`.

**Another agent:** change the download stage. Anything that ends up as a
binary, or a runtime plus your agent's files, works.

## 2. The entrypoint

```sh
#!/bin/sh
set -eu
# marsh names the home to use; settings and sessions persist there.
export HOME="${MARSH_SELECTED_HOME:-$HOME}"
# Your defaults and skills, shipped in the image.
export OPENCODE_CONFIG_DIR=/opt/myagent/config
# SBX replaces this placeholder with your real key at Anthropic's API.
[ "${SBX_CRED_ANTHROPIC_MODE:-none}" = none ] || export ANTHROPIC_API_KEY=proxy-managed
exec /usr/local/bin/opencode "$@"
```

Three things every agent's entrypoint does:

- **Use `MARSH_SELECTED_HOME` as `HOME`**, so the agent's sign-ins, settings and
  history live in your marsh home and persist between runs.
- **Point the agent at your defaults**, without overwriting settings you
  later change by hand. Here that's OpenCode's `OPENCODE_CONFIG_DIR`. Other
  agents have a flag, an environment variable, or a config file to seed on
  first run.
- **Set a placeholder key** when SBX reports it manages one. The agent sends
  the placeholder, and SBX's proxy swaps in the real key only on the way to
  the provider's API.

## 3. Your defaults, skills and instructions

`config/opencode.json`:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "model": "anthropic/claude-sonnet-5-5",
  "autoupdate": false,
  "instructions": ["/run/marsh/context.md"],
  "skills": { "paths": ["/opt/myagent/kit-skills"] }
}
```

- **Skills** in `skills/` ship in the image. OpenCode finds them inside its
  config folder.
- **`/run/marsh/context.md`** is written by marsh for every job. It tells your
  agent which commands it can start, and how (`marsh fanout`, `marsh split`,
  `marsh jobs --tree`), and it includes any instructions the Kit declares.
  Pass it to your agent as instructions or as an extra system prompt.
- **`/opt/myagent/kit-skills`** is where skills added later by a mixin appear
  (step 7). Load it as a second skills folder.

**Another agent:** find how it takes an extra instructions file and extra
skill folders. For example, marsh's Pi Kit uses `--append-system-prompt FILE`
and `--skill DIR`.

## 4. The Kit file

`myagent.yaml`:

```yaml
# syntax=docker/sandbox-kit:3
schemaVersion: "3"
kind: workload
displayName: My agent
description: OpenCode with my defaults and skills
licenses: [MIT]
dockerfile: ./Dockerfile
capabilities:
  - type: com.docker.sandbox/sbx@1
  - type: com.docker.sandbox/network-policy@1
    config:
      runtime:
        allow:
          - api.anthropic.com:443
          - models.dev:443
  - type: com.docker.sandbox/credential@1
    description: Anthropic API key
    config:
      service: anthropic
      phase: runtime
      apiKey:
        name: ANTHROPIC_API_KEY
        proxyManaged: true
        inject:
          - {domain: api.anthropic.com, header: x-api-key, format: "%s"}
  - type: com.docker.sandbox/agent-skills@1
    config:
      path: /opt/myagent/kit-skills
```

- **Network:** the hosts your agent may reach. OpenCode also reads its model
  list from `models.dev`.
- **Credential:** which stored key SBX sends, and to which host and header.
  Add one entry per provider your agent uses.
- **Skills folder:** where mixins may add skills (step 7).

## 5. Try it

Register the folder as a command. `marsh status --json` prints your control
directory (`control_home`). Add this to its `commands.json`, keeping any
existing entries:

```json
{ "myagent": "/Users/you/kits/myagent" }
```

Then restart marsh's daemon so it reads the change, and run your agent:

```sh
marsh stop
marsh
myagent run "Summarize this repository in three bullets"
```

The first run builds the image and boots its sandbox, which takes a few
minutes. After that it starts in seconds. Edit any file and run again; marsh
rebuilds when the folder changes.

Inside marsh, other agents can call yours, and yours can call them:

```sh
fanout {
  mine: myagent run "review the diff"
  claude: claude -p "review the diff"
} | collect
```

## 6. Publish and install

Push the Kit to a registry:

```sh
docker buildx build --platform linux/arm64 --push -f myagent.yaml \
  -t registry.example.com/you/myagent:1.0 -t registry.example.com/you/myagent:stable .
```

Remove the local `myagent` entry from `commands.json`, then install it the way
anyone else would:

```sh
marsh kit install myagent registry.example.com/you/myagent:stable
marsh kit upgrade myagent    # later: move to whatever stable names now
```

## 7. Let others add their own skills

A team can add skills and instructions to your agent without changing your
image: they publish a **mixin** and combine it with your Kit in a **kit set**.
marsh links the mixin's skills into `/opt/myagent/kit-skills` and adds its
instructions to `/run/marsh/context.md`. See
[Adding skills and instructions with mixins](../kits.md#adding-skills-and-instructions-with-mixins).

## 8. Give it tools

MCP servers only your agent should load go in `~/.marsh/sbxenvs/myagent.yaml`
on your Mac; the shared ones go in `~/.marsh/sbxenv.yaml`. SBX's MCP gateway
runs them and holds their credentials. See
[Set up your marsh home](set-up-your-home.md).

## Examples

- marsh's own [`claude`, `codex` and `pi` Kits](https://github.com/mcavage/marsh/tree/main/kits):
  small entrypoints, each passing `context.md` to its agent its own way.
- [Pix](https://github.com/mcavage/pix/tree/main/marsh): Pi with a skills
  library, themes and a status display. A larger example of the same steps.

## Before you share it

- Run it next to the packaged agents in a fresh marsh home. Only `myagent`
  should see your skills and defaults.
- Look at what the image contains, not just your Git diff, for private skills,
  account names, endpoints or keys.
- Make one real model call and one child job. A version or tool listing proves
  little.
