# Benchmark pilot: does a marsh workflow fix more bugs?

Status: designed and dry-run against `evals/common/fakemarsh.py`. Not yet run
against real agents.

## Question

On real bug-fix tasks, scored by tests the agents never see, how do four
workflows a marsh user can type compare in pass rate, wall time, and dollars?

This compares workflows **within marsh**. It does not claim that marsh beats
running agents without marsh. That would need a non-marsh orchestration
baseline, which is out of scope for this pilot. One derived arm (`e_fallback`,
below) separates what parallelism buys from what a second vendor buys.

| Arm | Workflow | What the user types (CLI form, as `run.py` drives it) |
|---|---|---|
| `a_claude`, `a_codex` | one attempt by one agent | one branch: `AGENT && GATE` |
| `b_retry` | one agent; if the gate fails, try again with its output, up to N=3 | one branch: a loop of `claude -p "$P$FEEDBACK"; GATE` |
| `c_race` | Claude and Codex at once on private forks, each gated; first passing branch wins | `marsh split -n -b claude='claude … && GATE' -b codex='codex exec … && GATE' \| marsh join --keep` ([race guide](../../docs/guides/race-two-agents.md)) |
| `d_judge` | the same race; when both pass, a third agent picks | `… \| marsh join --keep \| pi -p "choose …"` ([judge guide](../../docs/guides/add-a-judge.md)) |
| `e_fallback` (derived) | Claude; Codex only if Claude's gate failed | computed from the race's draws: same outcomes, sequential time, Codex billed only when needed |

GATE is a visible check that reproduces the bug (the "failing test you have"
of the race guide). The hidden tests are the upstream fix's tests, applied
only at scoring time in a separate checkout.

## Tasks

16 tasks: 12 from SWE-bench Verified and 4 self-made.

- **SWE-bench Verified, `sympy/sympy` and `pytest-dev/pytest` only.** Both are
  pure Python and need no compiled extensions or network, so they run in
  marsh's Debian arm64 shell VM after one `apt-get`/`pip` setup. The hidden
  tests are the dataset's `test_patch` (FAIL_TO_PASS and PASS_TO_PASS), so
  scoring follows established methodology and the tasks were screened by human
  annotators for solvability. Eligible: difficulty "<15 min" or "15 min–1 h",
  at most 5 FAIL_TO_PASS tests (82 instances on 2026-10-12).
  `curate_swebench.py pool` prints them in a seeded order. The curator takes
  them in that order and skips, with a recorded reason, any that fail
  `curate_swebench.py validate`. That stops anyone hand-picking easy or
  marsh-friendly tasks. Require at least 3 pytest tasks.
- **4 self-made tasks** from bug-fix commits merged after the newest model's
  training cutoff, in small pure-Python repositories, in the same format.
  SWE-bench is likely in the training data, and memorization can differ by
  model, so it can shift the differences between arms too (a race wins
  whenever either model remembers the fix). Report SWE-bench and self-made
  results separately. Four tasks are a sensitivity check, not proof of
  robustness.
- Why not the stock SWE-bench harness: it is per-instance x86 Docker images
  scored outside the workflow, and the point here is the workflow inside
  marsh. Why not Terminal-Bench: its tasks are terminal chores with their own
  container harness, not "fix this bug with these agents", so its numbers are
  not comparable.

Selection is published: the full eligible pool, the seed, every skipped
instance with its reason, and each gate, all frozen and reviewed by a second
person before any agent runs. Gate rubric: the gate reproduces the issue's
own example; it does not copy, paraphrase, or import the hidden tests; it
fails at base and passes with gold; and it runs in under a minute.

Each task directory holds `task.json`, `gate.patch` (the visible check,
committed to the base as `eval: visible gate`), `hidden.patch`, and for
SWE-bench `gold.patch`. The curator writes each gate from the issue's own
reproduction, not from the hidden tests. Validation, run in the shell VM with no
agent, requires four things: the gate fails at base, the gate passes with gold,
hidden fails at base, and hidden passes with gold. Scoring deviates from
SWE-bench in one way: it runs whole hidden test files rather than parsing
per-test logs. That is stricter, and validation shows the files pass with gold.

The **primary endpoint is SWE-bench's**: every FAIL_TO_PASS test passes and
no PASS_TO_PASS test fails. These are read per test from pytest's junit XML
against the dataset's frozen lists, which do not depend on what the candidate
touched. The whole-file exit status is a secondary, stricter endpoint. The
candidate's raw patch is stored in the record. Before scoring, its edits to
files the hidden patch touches are discarded (as SWE-bench does), and
`touched_tests` records that. A patch that does not apply counts as failed.
That case and a hidden run with no results are recorded with their reasons
(`reason`, `infra`) and handled by the fixed rule under "Metrics".

## Agents

- Claude Code (`claude`, marsh-claude Kit) on `claude-sonnet-5-5`
- Codex (`codex`, marsh-codex Kit) on `gpt-6.1-sol`
- Judge: Pi (`pi`, marsh-pi Kit) on `google/gemini-3.8-flash`, a third vendor,
  so the judge is not grading its own model's style (judge guide advice)

The commands are templates in `config.example.json`. Each Kit's own defaults
stay untouched (the Kits already skip permission prompts). The runner records
`marsh --version`. Before the run, also record the Kit refs (`marsh status`)
and the model ids.

## Procedure (per task × repetition)

1. Two fresh clones of the task template: one for the race and one for the
   retry. The template is the base tree exported into a new repository with
   two commits (`base`, `eval: visible gate`). The upstream history, and with
   it the fix commit, is not in the project. Each clone has its own Mac path,
   so per-project agent state does not carry over. Real runs use a dedicated
   selected home (`--marsh-home`), so the guest home's agent settings start
   empty and nobody's own configuration leaks in.
2. Concurrently: the race split (`claude`, `codex`) and the retry split
   (`retry`). That is three agent jobs, within marsh's limit of 8. Each branch
   prints `@@eval` marker lines (attempt start and end, agent exit, gate exit,
   epoch seconds) around the agent's JSON output, so token usage comes from
   `claude --output-format json` / `codex exec --json`.
3. The gate command is embedded in the branch script, not read from the fork,
   so an agent cannot rewrite which command gates it. An agent can still
   weaken the test file. That is realistic. The hidden checkout restores test
   files, and `touched_tests` records it.
4. If both race branches passed the gate, the judge reads `join`'s rendering on
   stdin through `marsh -c`. Branch order in the split, and so in the
   rendering, is shuffled per run from the seed to cancel position bias.
5. Each distinct patch is scored once. Apply it to a clean clone, reset any
   test files the hidden patch touches, apply the hidden patch, and run the
   hidden command with `marsh -c` in the shell VM.
6. `marsh splits rm`, delete the clones, and write `TASK.repN.json`.

Selection rules, fixed in advance: c = the passing branch whose gate finished
first, else Claude's patch. d = the judge's pick when both passed (falling
back to c's rule if the judge's answer is not a label), else c. Every arm always
submits something, so a missing patch counts as a fail.

`a_claude`, `a_codex`, and `e_fallback` are **counterfactual accounting
estimates** computed from the race's own branches (common random numbers),
not standalone executions. `c − a_claude` estimates what adding Codex and
first-pass selection buys on the same draws. Contention from the other
branches can change wall time, and through the timeout even a patch, so a
standalone single-agent run could differ. `e_fallback`'s rule is fixed: take
Claude's patch if its gate passed, otherwise Codex's, even if Codex failed
too. When neither passes, c submits Claude's patch and e submits Codex's, so
the two differ there for reasons that have nothing to do with parallelism.
A real parallelism claim needs actual sequential and parallel runs with the
same selection rule. `b_retry` is an
independent draw. Every agent call is capped at `agent_timeout_s` (30 min)
with `timeout` in the shell VM, and whatever a capped agent left is scored.

## Repetitions and order

2 repetitions. Repetition-major order (every task's rep 0 before any rep 1),
with tasks shuffled per rep from a fixed seed. If the budget cap stops the run,
it still leaves one complete paired replicate, and slow API hours do not line
up with one task.

## Metrics

- Primary: hidden pass, as defined in "Tasks". Every FAIL_TO_PASS test
  passes and no PASS_TO_PASS test fails, read per test against the frozen
  lists. A listed test that is missing, fails to collect, or times out counts
  as failed. A hidden run that produces no results at all (`infra`) is
  re-scored once, then counted as failed, and the count is reported.
  Self-made tasks freeze their own lists the same way. Secondary: the
  whole-file exit status.
- Wall time per arm. For a and b this is the branch's own markers; for c it is
  the split's end-to-end time, since marsh waits for both branches; for d, that
  plus the judge. The time to the first passing branch is shown as a
  labelled hypothetical (what cancel-on-first-pass would give). marsh does not
  do that today, and a cancelled loser could change cost.
- Dollars and tokens per arm. Claude's dollar figure is its own report;
  Codex's is tokens × the price table; the judge's is estimated from
  characters. Check the total against the provider dashboards before and
  after.
- Gate overfit: P(hidden pass | gate pass). Also judge accuracy on decisive runs
  (both gates pass, hidden tests split), test-touching rate, and retry attempts.

## Analysis (`report.py`)

- Per arm: pass rate with a 95% task-clustered bootstrap CI (resample tasks
  with their repetitions; 10,000 seeded resamples).
- Three comparisons, chosen in advance: `c_race − a_claude`,
  `b_retry − a_claude`, `d_judge − c_race`. For each: the task-level paired
  difference with a cluster-bootstrap CI, task counts better/worse, and an
  exact sign-flip permutation p-value over tasks, Holm-adjusted across the
  three. These p-values are **exploratory**. The arms were not randomly
  assigned within a task (they are different policies on shared draws), so
  the test assumes that under no effect each task's difference is as likely
  to be positive as negative. Run-level win/loss counts are shown, but no
  run-level test, since repetitions within a task are not independent. With
  16 clusters the bootstrap's coverage is only approximate. Lead with the
  per-task outcomes and counts.
- Judge accuracy is reported as decisive-and-correct out of decisive runs
  (both gates pass, hidden results split). Expect only a handful.
- Cost-effectiveness: measured $ per hidden pass for each arm. Spend is not
  matched between arms (retry spend depends on gate failures, race spend on two
  agents), so this is a report, not a controlled comparison.
- This is a **pilot**. As a scenario, not a promise: if task-level
  differences have a standard deviation of 0.4, the 95% half-width is about
  1.96 × 0.4 / √16 ≈ 0.20, so only large effects will be clear. Its outputs are effect sizes, between-task and within-task variance,
  and per-attempt cost, used to size a confirmatory run. Tasks needed ≈
  (1.96+0.84)² · Var(task-level diff) / δ². Size it on a conservative upper
  bound of that variance, or by simulation, not on the pilot's point estimate.

## Threats

- **Contamination and lookup.** The fix is not in the project's history, but
  agents reach model APIs through the sbx proxy and may reach the web. If a
  Kit's sbx network policy can be narrowed to the model endpoints (with
  dependencies installed beforehand), run that way and make it primary.
  Otherwise, label every result **lookup-enabled**: observable, not
  controlled. Check
  each Kit's network allowlist before the run, and record whether GitHub is
  reachable from a Kit job (`curl -sI https://github.com`). Report SWE-bench
  and self-made tasks separately. Scan transcripts for fetches of the task
  repository, its PRs, or mirrors. Analyse flagged runs as a sensitivity check;
  do not drop them silently.
- **Environment.** The gate and hidden tests run in the shell VM; agents run
  in Kit containers that may not have the project's test dependencies. That
  can interact with vendor and workflow; "it applies to every arm" does not
  rule out a biased difference. Before the run, probe each Kit
  (`claude -p 'run: python3 -c "import pytest"'`, and the same for Codex) and
  publish which agents can run the tests. Record whether agents tried to.
- **Shared state.** marsh shares one guest home across jobs by design, so agent
  settings and caches persist across tasks within the run. Fresh project paths
  stop per-project state from carrying over, but not global state. This is a
  stated limitation. Kits are pinned by immutable digest, recorded in
  `run-meta.json`.
- **Concurrency.** Three agent jobs share Kit VMs. Wall time includes that
  contention; compare arms within a run, not against other benchmarks.
- **Nondeterminism and drift.** Two repetitions estimate within-task variance.
  Record the model ids, Kit refs (`run-meta.json` keeps `marsh status --json`),
  `marsh --version`, and each task's setup; finish the run within a few days.
- **Prompt.** One prompt for every arm (issue text plus a fixed instruction
  that names the gate). Retry feedback appends the gate's output tail only.

## Cost model (fits under $100)

Prices per million tokens (input / cache read / output): Sonnet 5.5
$2 / $0.10 / $10, GPT-6.1-Sol $2 / $0.10 / $10, Gemini 3.8 Flash $0.75 / $0.075 / $3.75.
A Claude Code attempt on a SWE-bench-sized fix is about 1M input tokens
(mostly cache reads), 100k cache writes, and 15k output, roughly **$0.60**.
A Codex attempt is roughly **$0.50**. A judge call is about **$0.03**.

| Item | Per run | × 32 runs (16 tasks × 2) |
|---|---|---|
| race: Claude + Codex | $1.10 | $35 |
| retry: 1.65 expected Claude attempts (assumes each attempt's gate fails with p = 0.45: 1 + 0.45 + 0.45²) | $0.99 | $32 |
| judge, when both pass (≈35%) | $0.01 | $0.3 |
| **expected** | **$2.10** | **$67** |
| calibration: 2 tasks × 1 rep first, then update `cost_model` | | $4–6 |
| worst case (every retry runs 3 attempts; every attempt costs 1.5× the estimate) | $4.40 | $141 |

`run.py --estimate --task-count 16` prints this from `config.example.json`.
The runner checks spend before each run and stops at **`budget_usd` = $90**.
Before each run, the runner checks that spend plus `reserve_per_run_usd`
($4.40, the modelled worst case per run) stays under the cap. That is still not
a hard ceiling. The agent CLIs have no enforceable dollar limit; the per-agent
timeout bounds time, not tokens. Calibration replaces the 1.5× worst-case
assumption with the observed maximum per attempt, and the reserve is raised
to match. Rep 0 alone costs $34 expected and $70 at the modelled worst case,
inside the cap. A rep 1 cut short by the cap is reported
separately from the complete rep 0. Calibration runs are not analysed. Fixed
rule: if calibration shows attempts costing more than $0.80, drop to 12 tasks
before starting. Claude's figure is its own report; Codex's is tokens × price
(cache writes priced as in the table); the judge's is estimated from
characters. Reconcile with the provider dashboards.

## Running it (later; bills real models)

```sh
brew install mcavage/tap/marsh && sbx login
sbx secret set anthropic && sbx secret set openai && sbx secret set google
cp evals/benchmark/config.example.json evals/benchmark/config.json
python3 evals/benchmark/curate_swebench.py pool > pool.txt
python3 evals/benchmark/curate_swebench.py fetch --tasks evals/benchmark/tasks $(head -20 pool.txt)
# write each gate.patch + gate.cmd + setup, then:
python3 evals/benchmark/curate_swebench.py validate --tasks evals/benchmark/tasks ID...
MARSH_HOME=~/.marsh-evals marsh --load claude,codex,pi -c true
python3 evals/benchmark/run.py --config evals/benchmark/config.json --tasks evals/benchmark/tasks \
  --out results/pilot --marsh-home ~/.marsh-evals --only ID1 --only ID2 --reps 1   # calibrate
python3 evals/benchmark/run.py --config evals/benchmark/config.json --tasks evals/benchmark/tasks \
  --out results/pilot --marsh-home ~/.marsh-evals
python3 evals/benchmark/report.py results/pilot > results/pilot/REPORT.md
MARSH_HOME=~/.marsh-evals marsh stop
```

Dry run, no VM or credentials (also what `test_benchmark.py` does):

```sh
FAKEMARSH_PLAN=plan.json python3 evals/benchmark/run.py --config evals/benchmark/config.example.json \
  --tasks evals/benchmark/fixtures --only toy-slugify --out /tmp/dry --marsh evals/common/fakemarsh.py --reps 1
```
