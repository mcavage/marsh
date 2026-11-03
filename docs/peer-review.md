# Peer review

Peer review is off by default. When it is on, an agent gets a second opinion
before it reports substantial work as done. The reviewer is a model from a
**different vendor in the same class** (small, mid or large). The agent asks
for it with the [`task` tool](agents.md#the-task-tool) and either addresses
the findings or tells you which ones it did not address, and why. Different
vendor means the model's vendor, not the agent's: Pi running a Claude model
does not give a second opinion on Claude.

```sh
marsh config peer-review on     # detect reviewers, write peer-review.d/marsh.yaml
marsh peer-review ls            # the effective table
marsh config peer-review off
```

## What the agent is told

While peer review is on, each job's `/run/marsh/context.md` (`marsh context`
in a job) ends with a short section. The section names the kinds of work that
need a review, the example families in each class, and the ready reviewers
this job may start:

```text
## Second opinion
Before you report substantial work as done (a code change, a plan or design, an analysis someone will act on, a document the user will send; not a quick answer or a trivial edit), get a second opinion: call `task` with the reviewer for your own model's class from the table below, from a different vendor than your model's, sharing the folder, asking for findings only. Then address them, or tell the user which you didn't and why. No row for your class with another vendor: skip it.
Classes: small (claude-haiku, gpt-luna, gemini-flash), mid (claude-sonnet, gpt-terra, gemini-pro), large (claude-opus, claude-fable, gpt-sol, gpt-astra).

| class | vendor | task arguments |
|---|---|---|
| mid | anthropic | agent: "claude", model: "claude-sonnet-5-5" |
| mid | openai | agent: "codex", model: "gpt-6.1-terra" |
```

The section goes only to root jobs, the agents that report to you; a task's
child (a reviewer, or any other subagent) does not get it, so reviews do not
start reviews of their own. It is also left out when peer review is off, when
no reviewer is ready, or when no ready reviewer's agent is in the job's spawn
set (`MARSH_SPAWN`). The table has at most one row per class and vendor (the
first ready reviewer in table order) and at most 8 rows.
Agents know their own model, so the table does not name it.

## `task(model)`

`task` takes an optional `model`. marsh passes it to the agent's own model
option, which the agent's Kit declares with marsh's capability
`org.marsh/agent-model@1`. Its `config.args` holds the option's words with a
`{{.Model}}` placeholder, and marsh appends them after the prompt verbs
(stock SBX rejects unknown keys in `agent-sessions@1`, so the option lives in
its own capability). `marsh status --json` shows it as `model` under
`commands[].sessions`.

```yaml
  - type: org.marsh/agent-model@1
    optional: true      # stock SBX refuses a required capability it doesn't know
    config:
      args: ["--model", "{{.Model}}"]
```

| Kit | `args` | The child runs |
|---|---|---|
| `claude` | `["--model", "{{.Model}}"]` | `claude -p PROMPT --model M` |
| `codex` | `["-m", "{{.Model}}"]` | `codex exec PROMPT -m M` |
| `pi`, `pix` | `["--model", "{{.Model}}"]` | `pi -p PROMPT --model M` (`provider/id` works) |

A call that sets `model` for an agent whose Kit has no model option fails with
a tool error, and so does a malformed id (1 to 128 characters from
`A-Za-z0-9-_.:/@[]`, not starting with `-`).

### The job's `model`

marsh records the model each job was asked to run as `model` on its receipt:
`marsh jobs --json` (each job), `marsh jobs show ID` (`model:`, and `model`
with `--json`), and the job lines of `marsh events --json`. The daemon reads it
from the job's own arguments through its Kit's model option, so it is present
for a `task(model: M)` child and for a caller's own `claude -p … --model M`.
marsh reads the option the way the agent's parser does: `FLAG M` and, for a
long flag, `FLAG=M` (`--model sonnet --model=opus` is `opus`); the last one
wins; nothing after `--` is an option. When marsh cannot be sure, it records
nothing rather than a guess: a malformed value, another spelling of a model
flag than the Kit declares (`--model` for Codex's `-m`), or a Kit option that
is not one `FLAG {{.Model}}` pair. When no model was passed, `model` is
absent: the agent then uses its own default, which marsh does not read from
the agent's settings and does not guess. A job
event's `model` appears from the job's first transition after it is queued
(the same as `args`). The task label stays `task: DESCRIPTION`, so a task card
can show "Review by codex · gpt-6.1-terra" from `command`, `label` and `model`.

## Model classes

marsh decides a model's class and vendor:

1. **Curated table.** Built-in glob rules (`crates/marsh-daemon/data/model-classes.yaml`)
   are matched against the model id, then against the model's models.dev
   family. Before matching, marsh lowercases the id and strips any
   `provider/` prefix, cloud region or vendor prefix (`us.`, `global.anthropic.`)
   and host suffix (`@eu`, `:free`). The first match wins.
2. **Price band.** A model with no matching rule is classed by its output price
   in a models.dev snapshot: under $4 per million output tokens is small, $18
   or more is large, and anything between is mid. The vendor is the model's
   maker, never the host that lists it (see below).
3. **Unknown.** marsh cannot class a model that is in neither the table nor the
   snapshot. A reviewer with such a model needs explicit `class` and `vendor`.

| Class | Anthropic | OpenAI | Google | Open families |
|---|---|---|---|---|
| small | `claude-haiku-*` | `gpt-*luna*`, `gpt-*-mini*`, `gpt-*-nano*`, `gpt-oss-20b` | `gemini-*flash*`, `gemma-*` | `deepseek-*flash*`, `qwen*-flash*`, `glm-*flash*`, `glm-*air*`, `minimax-*flash*`, `ministral*`, `mistral-small*` |
| mid | `claude-sonnet-*` | `gpt-*terra*`, `gpt-oss-120b` | `gemini-*pro*` | `deepseek-*`, `qwen*`, `kimi-*`, `glm-*`, `minimax-*`, `mistral-medium*`, `devstral*` |
| large | `claude-opus-*`, `claude-fable*` | `gpt-*sol*`, `gpt-*astra*` | | `qwen*-max*`, `kimi-k3*`, `mistral-large*` |

Open-family vendors are `deepseek`, `alibaba` (Qwen), `moonshotai` (Kimi),
`zai` (GLM), `minimax` and `mistral`. The bare aliases `haiku`, `sonnet`,
`opus` and `fable` are Anthropic's.

`$MARSH_HOME/model-classes.yaml` overrides the built-in table. Its rules are
tried first, and its `bands` replace marsh's. If the file is malformed, marsh
ignores it and reports the error in `marsh peer-review ls`.

```yaml
bands: {small_below: 4, large_from: 18}
rules:
  - {match: "gpt-*sol*", class: mid, vendor: openai}
  - {match: "my-house-*", class: large, vendor: acme}
```

The snapshot, `crates/marsh-daemon/data/models-dev.json`, is generated from
<https://models.dev/api.json> by `scripts/update-models-dev.py`. For each
vendor and model name it keeps one row: provider, model id, family, vendor,
output price, open weights and release date.

A row's vendor is the model's maker, found in this order: the prefix of
models.dev's `canonical_model_id`; the model id's own prefix when it names a
known maker (`x-ai/grok-4`, not `azure/grok-4-3`); the provider when it is a
known maker (its own listing); the one known maker of the same model name on
other hosts (`grok-4-6` and `grok-4.6` are one name); the one known maker of
its family (Muse on Venice is Meta's). A known maker is one some listing's
`canonical_model_id` names. Maker spellings are canonicalized by
`crates/marsh-daemon/data/vendor-aliases.json` (`x-ai` and `spacexai` are
`xai`, `zhipuai` and `z-ai` are `zai`, `qwen` is `alibaba`, a leading `~` is
dropped), which marsh also applies to rule and drop-in vendors. When the
maker stays unknown the vendor is empty: marsh classes the model by price but
gives it no vendor, so it pairs only with an explicit `vendor` in a drop-in.

The snapshot keeps only chat models that
call tools and list a price. The `models-dev` workflow regenerates the
snapshot weekly and opens a pull request when it changes. marsh never fetches
models.dev at run time.

## Who is ready: drop-ins

Ready reviewers come from `$MARSH_HOME/peer-review.d/*.yaml` (`.yml` also
works). marsh reads these files in name order whenever it needs them, at every
job start and on every `marsh status`, so an edit applies to the next job
without a restart. Narwhal writes `narwhal.yaml` and
`marsh config peer-review on|off` writes `marsh.yaml`.

```yaml
version: 1            # optional; only 1
enabled: true         # optional
reviewers:
  - agent: codex      # a registered command
    model: gpt-6.1-terra
    class: mid        # optional: from the model classes
    vendor: openai    # optional: from the model classes
  - {agent: claude, model: claude-sonnet-5-5}
```

- Peer review is on when at least one file says `enabled: true` and no file
  says `enabled: false`. An `enabled: false` in any file turns it off.
- A reviewer pairs an agent with a model that agent can run with the vendor's
  credentials already in place. The file's author vouches for the
  credentials. marsh checks that the agent is a registered command and that
  its Kit declares a model option, and it fills in `class` and `vendor` from
  the model classes when the file leaves them out.
- Unknown keys, an unsupported `version`, a malformed agent name, model id or
  vendor (lowercase `a-z0-9-_.`), more than 64 reviewers in one file, or a file
  over 64 KiB each make that whole file invalid. An invalid file contributes
  nothing, and marsh reports it under `errors`. marsh reads at most 64 files;
  dotfiles and other extensions are ignored.
- If two files name the same agent and model, the first file in name order
  wins.

`marsh config peer-review on` writes `enabled: true` and replaces the
`reviewers` list in `marsh.yaml` with the reviewers marsh can detect. A
reviewer is detected for each registered agent whose Kit declares a model
option: for each credential service the Kit declares (`credential@1`) that
`sbx secret ls --global` shows as stored, marsh picks that vendor's newest
model in each class from the snapshot. When several agents reach the same
vendor, it prefers the vendor's own agent, meaning the one that reaches only
that vendor. Credentials that sbx does not list, such as some OAuth logins,
are not detected; add those reviewers to the file yourself. `off` sets
`enabled: false` and keeps the list.

## `marsh peer-review ls` and `marsh status`

`marsh peer-review ls` prints whether peer review is on, and which file turned
it off if one did. It then prints one row per ready reviewer (class, vendor,
agent, model, the drop-in that named it, and how its class was found),
followed by the reviewers that are not ready with the reason for each, and any
errors. `--json` prints the same document as `marsh status --json`
`peer_review`:

```json
{"enabled": true,
 "files": ["/Users/me/.marsh/peer-review.d/narwhal.yaml"],
 "reviewers": [{"class": "mid", "vendor": "openai", "agent": "codex", "model": "gpt-6.1-terra",
                "source": "/Users/me/.marsh/peer-review.d/narwhal.yaml",
                "classified": "model-classes: gpt-*terra*"}],
 "skipped": [{"class": "large", "vendor": "anthropic", "agent": "pi", "model": "claude-opus-5-5",
              "source": "…", "classified": "model-classes: claude-opus-*",
              "reason": "pi's Kit declares no model option (org.marsh/agent-model@1)"}],
 "errors": []}
```

`disabled_by` lists the files that say `enabled: false`. `marsh status --json`
includes the `peer-review-v1` feature, and `commands[].services` gives each
Kit's credential services.
