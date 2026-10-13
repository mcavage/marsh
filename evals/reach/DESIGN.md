# Agent reach: can a model use marsh from llms.txt alone?

Status: harness built and dry-run on canned replies against `fakemarsh`
(`test_reach.py`). No model has been asked yet.

## Question

A model gets only `https://runmar.sh/llms.txt`. Can it write commands that
install marsh and use it correctly? We cannot rule out that a model has seen
marsh before, so the finding is "performance with the supplied
documentation", not "performance from zero". Where a model fails, the
conditions below show whether the needed fact was missing, hard to find, or
present and still misused. That is evidence of documentation sensitivity, not
proof of the cause.

## Conditions

| Condition | What the model gets | Why |
|---|---|---|
| `closed` | the llms.txt text (6.8 KB, an index with one line per page) in the prompt; no tools | lower bound: does the index alone carry the core commands? |
| `open` | the same, plus a web-fetch tool allowed only for `runmar.sh/*` and the marsh GitHub repository (everything else denied) | the intended llms.txt use: follow the index to the pages, or to `llms-full.txt` |
| `reference` | the whole manual (`llms-full.txt`, about 130 KB) in the prompt; no tools | every fact is present. These are **delivery conditions**. Reference differs from open in context length and tool use as well as in which facts are reachable, so a gap between them suggests discoverability but does not prove it. A later round can add a short expert-written reference per task |

`ask` writes an opencode agent into a scratch directory
(`.opencode/agents/reach-<condition>.md`). It denies every tool except, in the
open condition, those web fetches. Its system prompt replaces opencode's
coding prompt and says the model cannot run anything. It also caps the run at
12 steps. Models are shuffled within each (task, repetition, condition)
block from a fixed seed. The llms.txt and llms-full.txt texts are pinned by SHA-256 in each
trial's `meta.json`. The opencode JSON event stream, including fetched page
bodies, is kept as `transcript.jsonl`. Freeze the site for the run, so `open`
sees the same pages as the pinned files; if that is not possible, finish within
a day.

## Models

`anthropic/claude-sonnet-5-5`, `openai/gpt-6.1-sol`, `google/gemini-3.8-flash`,
and one free opencode model (default `opencode/mimo-v2.6-flash-free`; check
`opencode models` on the day, since free models rotate). Three repetitions per
cell at the provider's default temperature. Do not tune prompts per model.
Record the opencode version and each model's reported id.

## Tasks (`tasks.json`)

Every task asks for one sh script that the developer runs from the project in
macOS Terminal (not inside marsh), unattended.

| Task | Request | Critical checks (every requested requirement is critical) |
|---|---|---|
| R1 install | install marsh and its prerequisites, sign in, store an Anthropic key (the developer may sign in or paste the key interactively) | `brew install docker/tap/sbx`; `sbx login`; marsh from `mcavage/tap/marsh`, or the runmar.sh installer actually piped to `sh`; `sbx secret set anthropic` (or `import`); no invented installers (`pip/npm/cargo …`, untapped `brew install marsh`, Docker Desktop) |
| R2 agent job | have Claude summarize README.md into SUMMARY.md through marsh | exits 0; a new `claude` job is in `marsh jobs`; SUMMARY.md mentions the project (outcome) |
| R3 fanout | run `make lint` and `make test` side by side, labelled | a lint and a test header from collect; lint's output and the failing test's output both present (both ran); uses fanout |
| R4 race | Claude and Codex fix the failing test at once on private copies, each gated on `make test`; leave my files alone | both agent jobs ran; uses split; **two** `agent && make test` gates; join reported both branches; one branch `exited 0` (outcome); the work tree is unchanged |
| R5 race + apply | as R4, then apply a passing branch's patch and clean up | both agents; split; two gates; `make test` passes afterwards (outcome); only `calc.py` changed (outcome); nothing left in `.marsh/split` |
| R6 results | print exit status and wall time of recent jobs (two `shell` jobs are seeded first) | exits 0; calls `marsh results` or `marsh jobs`; the output shows wall time and exit status |
| R7 stop | shut marsh's VMs down without losing project or settings | `marsh stop`; no `sbx rm/stop`, `marsh reset`, or `rm -rf`; project untouched. `marsh stop` is a proxy: the grader does not inspect VM state |

Known gaps in what the checks prove: R3 relies on fanout's documented
concurrency rather than measuring overlap. In R4, `exited 0` together with an
`agent && make test` branch shows that the gate ran in that branch, but a
branch whose agent failed never reaches its gate. R7 checks the command, not
VM state. `test_reach.py` holds false-pass attempts (wrong installers, fanout
for writes, invented subcommands, destructive cleanup); add one whenever a
review finds a false pass.

Checks marked *outcome* depend on the agents doing the work. A trial
**succeeds** when every critical check passes. It is
**orchestration-correct** when every critical non-outcome check passes. That
separates the script's correctness from Claude or Codex failing to fix a
one-line bug. It is only partial, because an agent that exits nonzero also
skips its gate. A cleaner orchestration track would run real marsh with
deterministic stand-in agents: a stub Kit registered as `claude` and `codex`
that applies a fixed patch or fails on cue. That is planned, not built. Regex checks on the script (`split`, `&& make test`) are
diagnostic evidence beside the observed behaviour, not proof by themselves.

Every task also has a non-critical `known_cli` check: each `marsh WORD` in the
script must be a subcommand in `docs/man/marsh.1.md`. That makes invented
commands (`marsh race`, `marsh login`) a counted failure mode, not just a
crash.

The project for R2–R7 (`fixtures/calc`) is a one-function Python library with
a failing test, a Makefile (`test`, `lint`), and a README.

## Grading (automated)

`run.py grade` runs each script with `sh` in a fresh scratch project and
scratch `$HOME`. The scratch `PATH` puts recording shims first for `brew`,
`curl`, `wget`, `sbx`, `sudo`, `docker`, and the language package managers.
They log their argv and do nothing, so nothing is ever installed, and the
logged calls are what the install checks read. `marsh` is a recording shim in
front of either:

- `--backend fake`: `evals/common/fakemarsh.py` with scripted agents. This only
  tests the harness. It does not emulate Brush `split { }` blocks, so a correct
  script in that form fails here. **Do not report fake-backend scores.**
- `--backend real --marsh-home DIR --yes`: the installed marsh, with an
  isolated selected home and the real `sbx` (via `MARSH_SBX`) for marsh itself.
  Agent checks compare `marsh jobs --all --json` before and after the trial.
  R1 always uses the shims, so R1 measures **compliance with the documented
  install commands**, not a completed install. One selected home is shared
  across a grading run (a home per trial would boot a new shell VM every
  time); trials are isolated by fresh project directories and job-id diffs.
  An infrastructure failure (sbx error, quota, provider outage, seen in
  stderr or `marsh results`) is marked by hand and the trial is re-run once;
  the rule is fixed in advance.

A trial **succeeds** when every critical check passes. Its **score** is the
fraction of all checks passed. Scripts are screened before running: no reply
or no code block counts as a failure, and a denylist (`rm -rf /`, `~`,
`diskutil`, `launchctl`, …) refuses to run a script and records why.

**Human review**, about 2 hours. Review every failed trial and a random 20% of
passes, to catch false passes. For each, record separately the observed error,
the documentation evidence (the page and sentence the model needed, or that
it misread), and the hypothesized cause. Several labels are allowed: invented
command or flag; wrong context (inside-shell syntax run from Terminal, or the
reverse); fanout where split was needed; missing gate; interactive command;
installer error; fact absent from the docs; fact present but hard to find;
fact ambiguous or inconsistent; or other. Two coders independently code a
sample stratified by model and condition (at least 30 trials), report Cohen's
kappa, and settle disagreements. Documentation findings become issues. A
follow-up re-runs a fresh, randomized set of trials on both the original and
the repaired page. Re-asking only the failed trials would show regression to
the mean, not the repair.

## Analysis (`run.py report`)

- The estimand is average success on **these seven fixed tasks**, not on marsh
  use in general. The per-task table (successes out of 3 for each model ×
  condition) comes first.
- Success and orchestration-correct rates per model × condition, with Wilson
  95% intervals. These treat the 21 trials as independent and only describe
  this task set.
- Condition effects (open − closed, reference − open) per model, compared task
  by task: count the tasks that improved, stayed the same, or got worse.
- Invented-CLI rate and the failure-cause table from the human review.
- Read it as a capability profile on seven tasks, not a ranking of models. A
  general claim would need many more tasks.

## Cost

| Phase | Trials | Estimate |
|---|---|---|
| ask, closed (≈2.5k tokens in, 0.6k out) | 84 (63 paid) | ≈ $0.6 |
| ask, open (fetches; ≈60k cumulative in, 1.5k out) | 84 (63 paid) | ≈ $7 |
| ask, reference (≈33k in, 0.6k out) | 84 (63 paid) | ≈ $4 |
| real grading: R2 (a Claude summary, ≈$0.05) | 36 | ≈ $2 |
| real grading: R4 + R5 (a Claude + Codex race on a one-line bug, ≈$0.25) | 72 | ≈ $18 |
| R1, R3, R6, R7 | 144 | $0 (no agent) |
| **total** | | **≈ $32**; stop only between complete model × condition blocks, if it passes $40 |

Real grading only runs scripts that call an agent, and a script that fails
before reaching an agent costs nothing, so the grading line is an upper
bound.

## Running it (later)

```sh
python3 evals/reach/run.py ask --out results/reach            # 4 models, 3 conditions, 3 reps
python3 evals/reach/run.py grade --out results/reach --backend real \
  --marsh "$(command -v marsh)" --marsh-home ~/.marsh-reach --yes
python3 evals/reach/run.py report --out results/reach > results/reach/REPORT.md
MARSH_HOME=~/.marsh-reach marsh stop
```

**Isolation is required, not optional.** Model-written scripts can use
absolute paths, other interpreters, or the network. The shims, scratch HOME,
and denylist are not a security boundary. Run `grade --backend real` only:

- in a dedicated, **non-administrator** macOS user account with no personal
  files, SSH keys, keychain items, or browser sessions (check that other users'
  homes are not readable from it);
- with outbound network limited by a host firewall to what grading needs
  (Docker Sandboxes and the model providers);
- signed in to sbx with a test account, using provider keys with a hard spend
  cap that you revoke afterwards;
- after a person has read every `script.sh`.

The harness's `--yes` stands for that confirmation. A macOS VM would be
better, but Docker Sandboxes needs hardware virtualization that a nested macOS
VM does not reliably provide.

## Phase 2 (designed, not built)

An agentic variant: an opencode agent with a shell tool, in a disposable macOS
user account with marsh preinstalled (for R2–R7), works each task to
completion. The same end-state checks apply, and it also measures turns to
success and whether it recovers from errors using `marsh --help`. Phase 1
already isolates the documentation question more cleanly. Phase 2 adds how
well marsh's error messages steer an agent.
