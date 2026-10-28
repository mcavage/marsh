# Pix on marsh

This directory builds Pix as a marsh Kit: upstream Pi plus Pix's public
skills, roles, themes and UI extensions. The published image is
`docker.io/mcavage/marsh-pix`. Inside marsh you run it as `pix`; the packaged
`pi` is unchanged.

There is no Pi fork, subagent runner, scheduler, memory service or model
gateway here. marsh runs child jobs (fanout/collect, split/join), and SBX holds
the API keys.

## Install

```sh
sbx secret set openai
marsh kit install pix docker.io/mcavage/marsh-pix:stable
marsh
pix
```

This needs marsh 0.1.8 or later (`brew upgrade marsh`). On marsh 0.1.7, write
`--from` before the image name.

marsh 0.1.11 and later add `pix` on their own and keep it on `stable`: they
check for a new image at daemon start and once a day, and print
`pix updated (A → B)` after a switch. `marsh kit uninstall pix` removes it.
The version is the Pi version plus a hash of Pix's own files
(`1.3.0-pix.686a67ed`), so a rebuild that changes neither switches silently.

The default model is `openai/gpt-6.1-sol`. To use another provider, pass it
explicitly, e.g. `pix --provider anthropic --model claude-sonnet-5-5`. The Kit
can use OpenAI, Anthropic and Google keys; SBX inserts them at those providers'
API hosts, and no key is in the image.

The Kit declares its entrypoint's `--acp` mode as its ACP adapter
(`com.docker.sandbox/acp@1`), so `acp run pix` starts a session.

## Build it yourself

```sh
kit=~/.local/share/pix-marsh-kit
# The Pix content (skills, roles, themes) comes from a checkout of the Pix repository.
python3 kits/marsh-pix/prepare.py --pix-source ~/src/pix "$kit"   # empty destination
docker buildx build --platform linux/arm64 --load -t pix-marsh:local \
  $(python3 kits/marsh-pix/kit_inputs.py build-args "$kit") -f "$kit/pix.yaml" "$kit"
```

`kit_inputs.py build-args` resolves `docker.io/mcavage/marsh-pi:latest` to its
current digest, stamps the Kit version into the prepared copy and prints the
matching `--build-arg` flags. Without them the build uses the tag as it is.

`prepare.py` copies only Git-tracked public files (the selected skills, roles,
themes and package files). It never reads `~/.pix` or `~/.marsh`. To use a
local build, add `"pix": "/absolute/path/to/the/prepared/kit"` to `commands.json`
in your marsh control directory instead of installing the published image.

## Your settings

Pix keeps its state in `~/.marsh/home/.pix-agent/agent/` (inside marsh:
`~/.pix-agent/agent/`). Use `/settings` for themes and `/pix-status` for
activity. Output styles live in `context/output-styles`, managed with
`/output-style`. `PIX_UI=off` turns off the activity display.

## Orchestration

Load `/skill:marsh-orchestration`. Roles are prompts given to a registered
command with an explicit model. Use fanout/collect for independent work,
split/join for parallel edits (join returns patches; it does not apply them),
and plain sequential calls when one step depends on another. `marsh jobs --tree`
and `marsh results` show what ran.

## Adding your own skills and tools

Your own skills go in `~/.marsh/home/.pix-agent/agent/skills/`. MCP servers for
Pix go in `~/.marsh/sbxenvs/pix.yaml`.

To give a team the same skills and instructions, publish them as a mixin and
combine it with this image in a
[kit set](https://docs.docker.com/ai/sandboxes/customize/author/kit-sets/).
Then everyone installs the set instead of this image:

```yaml
# syntax=docker/sandbox-kit:3
schemaVersion: "3"
kind: set
displayName: Team Pix
kits:
  - ref: docker.io/mcavage/marsh-pix
    digest: sha256:…
  - ref: registry.example.com/team/pix-skills   # kind: mixin
    digest: sha256:…
```

In the mixin, declare each skill folder with `agent-skill@1` and your
instructions with `agent-context@1`. Pix loads those skills from
`/opt/pix/kit-skills`, and the instructions are added to its system prompt.
That needs marsh 0.1.8 or later. Keep organization-specific material out of
this repository.

## Tests

```sh
export PIX_SOURCE=~/src/pix                      # a checkout of the Pix repository
python3 -m unittest discover -s kits/marsh-pix/tests -v   # export boundary and preparation
python3 kits/marsh-pix/tests/runtime.py          # builds the Kit and runs Pi with fake keys
```

`runtime.py` builds on the current `marsh-pi:latest`; set `PIX_MARSH_PI_BASE`
to a `docker.io/mcavage/marsh-pi@sha256:…` reference to pin it.

## Publishing

The [`Kit marsh-pix` workflow](../../.github/workflows/kit-pix.yml) publishes
`docker.io/mcavage/marsh-pix:stable`. The Kit's own files are here, in the marsh
repository. The Pix content it ships (skills, roles, themes, `LICENSE`) stays in
the [Pix repository](https://github.com/mcavage/pix), which the workflow checks
out at every run. It runs on pushes to `main` that change this directory, every
hour, on a `pix-content-changed` repository dispatch, and by hand. marsh's Pi
image moves without a marsh release and the Pix content changes in its own
repository, so the schedule is what picks up either.

Each run resolves `marsh-pi:latest` to a digest and builds from exactly that
digest, which the image records in its `org.opencontainers.image.base.name`
label. A push or scheduled run stops there when `stable` already has the same
Kit version and base digest; a manual run always publishes. Otherwise it runs
the tests above, publishes an immutable `<version>-r<run>` tag with the
publisher in this repository, checks that image's version, base and
`org.marsh.kit.contract` (which must be one the newest stable marsh
supports), and only then moves `stable`. A failure leaves `stable` where it
was. A manual run with `stable` off publishes only the immutable tag. Pull
requests run the tests and a cache-only build.

Locally, `python3 kits/marsh-pix/publish.py --pix-source ~/src/pix --publisher scripts/publish-kits.py --output DIR --tag TAG [--push]`
does the same build and writes the `pix` → digest mapping; it never moves
`stable`.
