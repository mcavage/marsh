# Hermes on marsh

[Hermes Agent](https://github.com/NousResearch/hermes-agent) from Nous Research,
packaged as a [marsh](https://runmar.sh) command.

```sh
sbx secret set anthropic          # or: sbx secret set google
marsh kit install hermes docker.io/mcavage/marsh-hermes:stable
marsh
hermes
```

That's all the setup. On first run Hermes uses the provider of the key you
stored (Anthropic, else Google Gemini). SBX inserts the key at the provider's
API, so it is never in the sandbox. `marsh kit upgrade hermes` moves to new
releases.

## Skills

Hermes reads and writes `~/.agents/skills` in your marsh home: the folder Pix,
Codex, OpenCode and Claude (through `~/.claude/skills`) read too. Every skill
Hermes creates is available to all of them on their next run. Hermes's own
bundled skills stay in `~/.hermes/skills`, visible only to Hermes.

A team can add skills with a mixin in a kit set; they appear in
`/opt/hermes/kit-skills`, which Hermes also reads.

## Settings

Hermes keeps its state in `~/.hermes` in your marsh home (`~/.marsh/home/.hermes`
on the Mac by default), separate from a Hermes installed on the Mac itself.
The first run writes `~/.hermes/config.yaml`; edit it to change the model or
provider. Each run also passes Hermes marsh's job context (which agents it can
start, and how) through `HERMES_EPHEMERAL_SYSTEM_PROMPT`, which takes the place
of a configured `agent.system_prompt` or personality.

## Releases

CI follows Hermes's releases: every six hours it builds the latest stable
Hermes release, smoke-tests it, and publishes `:VERSION` and `:stable`
(`.github/workflows/publish.yml`). `marsh kit upgrade hermes` picks it up.

To build your own copy: `make publish TAG=docker.io/you/marsh-hermes`.

The image builds Hermes from its lockfile at a pinned commit, with its own
Python, in a Docker Hardened Image build stage, on marsh's `marsh-shell` base.

## License

MIT for this packaging. Hermes Agent is MIT licensed by Nous Research.
