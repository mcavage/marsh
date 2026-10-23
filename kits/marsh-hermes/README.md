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
API, so it is never in the sandbox. marsh keeps the Kit up to date on its own
(see [Updates](#updates)).

## Skills

Hermes reads and writes `~/.agents/skills` in your marsh home: the folder Pix,
Codex, OpenCode and Claude (through `~/.claude/skills`) read too. Every skill
Hermes creates is available to all of them on their next run. Hermes's own
bundled skills stay in `~/.hermes/skills`, visible only to Hermes.

A team can add skills with a mixin in a kit set; they appear in
`/opt/hermes/kit-skills`, which Hermes also reads.

## Other agents and models

Hermes uses marsh for multi-agent work: it starts `codex exec`, `claude -p`,
`pix -p` and other registered commands as child jobs (`fanout`, `split`), each
in its own sandbox with its own key. Its in-process subagents (the `delegation`
toolset) are off by default.

## Tools

Hermes uses the work tools in your marsh home's MCP connections (for example
`~/.marsh/sbxenv.yaml` or `~/.marsh/sbxenvs/hermes.yaml`) through the sandbox's
MCP gateway. Each run keeps an `mcp-gateway` entry in Hermes's config pointing
at it; SBX holds the tools' credentials.

## Privacy

Your prompts, files and tool results go only to your model provider (and the
tools you connected). Hermes's telemetry to Nous Research is off. Hermes still
downloads its model list and checks for updates, which sends no content.

To enforce the telemetry setting rather than rely on it:

```sh
sbx policy deny network telemetry.nousresearch.com
```

## Settings

Hermes keeps its state in `~/.hermes` in your marsh home (`~/.marsh/home/.hermes`
on the Mac by default), separate from a Hermes installed on the Mac itself.
The first run writes `~/.hermes/config.yaml`; edit it to change the model or
provider. Each run also passes Hermes marsh's job context (which agents it can
start, and how) through `HERMES_EPHEMERAL_SYSTEM_PROMPT`, which takes the place
of a configured `agent.system_prompt` or personality.

## Approvals

Hermes's command-approval prompts are off, as for marsh's `claude`, `codex` and
`pi`: the job container is the sandbox boundary. Hermes still refuses its
hardline commands and honors `approvals.deny` rules in `~/.hermes/config.yaml`.
To get the prompts back, `export HERMES_YOLO_MODE=0` in your marsh shell before
running `hermes`.

## Updates

marsh checks `:stable` when its daemon starts and once a day, and switches to
a newer image when no job is using the Kit (`marsh status` shows the last
update). Inside the Kit, `hermes --version` shows the installed release and
`hermes update` is refused: it has no checkout to update, and the image is
what changes.

## Releases

CI follows Hermes's releases: every six hours it builds the latest stable
Hermes release, smoke-tests it, and publishes `:VERSION` and `:stable`
(`.github/workflows/publish.yml`). marsh follows `:stable`.

To build your own copy: `make publish TAG=docker.io/you/marsh-hermes`.

The image builds Hermes from its lockfile at a pinned commit, with its own
Python, in a Docker Hardened Image build stage, on marsh's `marsh-shell` base.

## License

MIT for this packaging. Hermes Agent is MIT licensed by Nous Research.
