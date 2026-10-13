# Benchmark pilot: does a marsh workflow fix more bugs?

Status: 16 tasks gated and validated in the shell Kit
([validation/](validation/)), and a calibration run with real agents done
([validation/calibration.json](validation/calibration.json)). The full pilot
has not been run; see "Running it".

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
| `c_race` | Claude and Codex at once on private forks, each gated; first passing branch wins | `marsh split -n -b a='claude … && GATE' -b b='codex exec … && GATE' \| marsh join --keep` ([race guide](../../docs/guides/race-two-agents.md)) |
| `d_judge` | the same race; when both pass, a judge agent picks | `… \| marsh join --keep \| pi -p "choose a or b …"` ([judge guide](../../docs/guides/add-a-judge.md)) |
| `e_fallback` (derived) | Claude; Codex only if Claude's gate failed | computed from the race's draws: same outcomes, sequential time, Codex billed only when needed |

GATE is a visible check that reproduces the bug (the "failing test you have"
of the race guide). The hidden tests are the upstream fix's tests, applied
only at scoring time in a separate checkout.

## Tasks

16 tasks: 12 from SWE-bench Verified (9 `sympy/sympy`, 3 `django/django`)
and 4 self-made (more-itertools, tomlkit, packaging, tenacity). The list, and
every candidate that was skipped with its reason, is in
[validation/selection.json](validation/selection.json).

**Test environment.** The agent Kits and the shell Kit are minimal DHI images
with Debian's Python **3.13.5**, but no pip, venv, or package manager. The shell
VM itself has Python 3.14. So the gate and the hidden tests run where the
agents do: the config's `test_runner` is `shell -c {cmd}`, the shell Kit,
whose `python3` is the same Debian build as the agent Kits'. Each task's
pure-Python test dependencies are pinned with hashes in `task.json` (`env.deps`,
`py3-none-any` wheels only). `run.py` downloads them once (`pip download
--require-hashes`) and unpacks them into `.eval-deps/` in the task repository,
in an `eval: test environment` commit. Every command then uses
`PYTHONPATH=.eval-deps` (plus `src` or `.` where the project needs it). The
same files are therefore importable by the agents in their containers, by the
gate, and by the hidden run, with no network access and no install step. Old
sympy imports `distutils`, which was removed in 3.12. setuptools' shim provides
it: `.eval-deps/sitecustomize.py` runs `site.addsitedir` so that setuptools'
`.pth` file is honoured. That file is generated with the deps.

- **SWE-bench Verified.** Eligible: sympy and pytest instances at difficulty
  "<15 min" or "15 min–1 h" with at most 5 FAIL_TO_PASS tests (82 instances),
  and the same filter on Django 5.0 (21). `curate_swebench.py pool` prints
  them in a seeded order (seed 20261012). The curator took them in that order
  and skipped any that failed validation, recording the reason:
  - **sympy:** the first 9 that validate. The 6 skipped on the way are
    sympy 1.0–1.1, which do not import on Python 3.13 (`collections.Mapping`).
  - **pytest-dev/pytest: dropped.** Every eligible base predates Python 3.10
    support. pytest's assertion rewriting fails to compile on 3.13 (`required
    field "lineno" missing from alias`), and fixing that would mean patching
    the code under test. **Django 5.0 replaces it** as the second repository:
    it is pure Python (asgiref and sqlparse as deps), and its runner's verbose
    log is parsed SWE-bench's way (`format: unittest-verbose`). The first 3
    validated, and none were skipped. Later Django candidates that need Pillow
    or Jinja2 (not pure Python) fail validation because their FAIL_TO_PASS
    tests are skipped.
- **4 self-made tasks** from bug-fix commits merged **after 2026-09-29**, the
  release date of the newest model in the pilot (so after every model's
  training cutoff). Each comes from a different small pure-Python repository,
  and `curate_swebench.py self-made` builds it. The base is the fix's parent;
  `hidden.patch` is the commit's test changes and `gold.patch` the rest. The
  FAIL_TO_PASS and PASS_TO_PASS lists are computed the way SWE-bench defines
  them, by running the hidden test files at the base and with gold on Python
  3.13, then frozen. The problem statement (`issue.md`) is written from the
  observed behaviour at the base, not from the fix. SWE-bench is likely in the
  training data, and memorization can differ by model, so it can shift the
  differences between arms too (a race wins whenever either model remembers
  the fix). Report SWE-bench and self-made results separately. Four tasks are
  a sensitivity check, not proof of robustness.
- Why not the stock SWE-bench harness: it is per-instance x86 Docker images
  scored outside the workflow, and the point here is the workflow inside
  marsh. Why not Terminal-Bench: its tasks are terminal chores with their own
  container harness, not "fix this bug with these agents", so its numbers are
  not comparable.

**Gates.** Each task has a visible gate: `gate.patch` adds one test file
(`test_eval_gate.py`; for Django, `tests/<app>/test_eval_gate.py`), and
`gate.cmd` runs only that file. Both are committed to the base as `eval:
visible gate`. Gate rubric:

- It encodes the issue's own example, with the outcome the maintainers chose
  (where the issue only guesses, as in sympy-20801).
- Often upstream turned the same example into a hidden test, so some overlap
  is unavoidable. The gate never covers the hidden tests' other cases or the
  PASS_TO_PASS regressions.
- It fails at the base, passes with gold, and runs in seconds.

The gate is the "failing test you have"; the hidden tests check that the fix
is general and breaks nothing.

**Validation** runs in the shell Kit with no agent
(`curate_swebench.py validate`). It checks SWE-bench's own conditions (at the
base every FAIL_TO_PASS test fails and every PASS_TO_PASS test passes; with
gold all pass) and the gate's (fails at the base, passes with gold). All 16 pass
([validation/kit-validation.json](validation/kit-validation.json), marsh
0.1.4-nightly.20261013155429.g871a6bf).

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
- Judge: Pi (`pi`, marsh-pi Kit) on `claude-opus-5-5`. The marsh-pi Kit's
  network and credential grants cover Anthropic only, so a third vendor is not
  available without a custom Kit. Instead the judge is **blind**: the race's
  branches are labelled `a` and `b`, assigned to Claude and Codex at random per
  run from the seed. Each branch writes its markers and the agent's own output
  to stderr, which `join` hides for a passing branch, and the judge is only
  asked when both passed. So the judge sees each branch's gate output and
  patch, but not which agent wrote it. A model's preference for its own
  style remains possible; report judge picks by agent.

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
2. Concurrently: the race split (`a`, `b`) and the retry split (`retry`).
   That is three agent jobs, within marsh's limit of 8, plus a shell-Kit job
   per gate. Each branch writes `@@eval` marker lines (attempt start and end,
   agent exit, gate exit, epoch seconds) around the agent's JSON output to its
   stderr, so token usage comes from `claude --output-format json` /
   `codex exec --json`.
3. The gate command is embedded in the branch script, not read from the fork,
   so an agent cannot rewrite which command gates it. An agent can still
   weaken the test file. That is realistic. The hidden checkout restores test
   files, and `touched_tests` records it.
4. If both race branches passed the gate, the judge reads `join`'s rendering on
   stdin through `marsh -c` (blind, as above). The random a/b assignment also
   randomizes which agent is rendered first.
5. Each distinct patch is scored once. Apply it to a clean clone, reset any
   test files the hidden patch touches, apply the hidden patch, and run the
   hidden command with `marsh -c` through the shell Kit.
6. `marsh splits rm`, delete the clones, and write `TASK.repN.json`.

Every project, scoring, and gate directory gets a name that has never been
used (`fresh_name`). marsh keeps the shell VM's mount of each project path,
and it refuses a path that was deleted and recreated ("overlaps active
grant"), so reusing a path, even on a rerun, would fail.

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
  and `marsh --version`; finish the run within a few days.
- **Prompt.** One prompt for every arm (issue text plus a fixed instruction
  that names the gate). Retry feedback appends the gate's output tail only.

## Cost model (calibrated; fits under $100)

**Calibration** (2026-10-13, 7 runs on 6 tasks, $1.30 in total; not analysed;
[validation/calibration.json](validation/calibration.json)):

| | min | mean | max |
|---|---|---|---|
| Claude attempt (Sonnet 5.5, own report) | $0.029 | $0.057 | $0.108 |
| Codex attempt (GPT-6.1-Sol, tokens × price) | $0.050 | $0.062 | $0.086 |
| judge (Pi, Opus 5.5, from characters) | | ≈ $0.01 | |
| race split, wall | 20 s | | 69 s |
| retry split, wall | 10 s | | 61 s |
| one run end to end, with hidden scoring | | ≈ 75 s | |

No first attempt failed its gate, and the judge ran in all 7 runs because
both branches always passed. The hidden tests failed for some arm in 3 runs.
On sympy-15017, Codex's fix was overfit twice, and on the old nightly Claude's
branches were also rejected. On sympy-20438, both fixes passed the gate and
failed the hidden tests. The
SWE-bench tasks were annotated "<1 h for a human", and they are easy for these
agents. Expect retry to behave like a single attempt on most tasks, and expect
arm differences to come mainly from runs where the gate passes but the hidden
tests fail. That limits the pilot. A confirmatory study needs harder tasks.

**Model** (`config.example.json`, `run.py --estimate`): per-attempt cost twice
the observed means ($0.12 each), the gate-failure rate kept at a pessimistic
0.45 (so 1.65 expected retry attempts), and the judge in 90% of runs:

| Item | Per run | × 32 runs (16 tasks × 2) |
|---|---|---|
| race: Claude + Codex | $0.24 | $7.7 |
| retry: 1.65 expected Claude attempts | $0.20 | $6.3 |
| judge | $0.02 | $0.6 |
| **expected** | **$0.46** | **$14.6** |
| at the observed means | $0.19 | $6 |

**Cap.** `budget_usd` is $90. Before each run, the runner checks that spend plus
`reserve_per_run_usd` stays under the cap. The reserve is $17: the time-bounded
worst case of one run, in which all five agent calls (2 race + 3 retry) run to
`agent_timeout_s`, 900 s (13× the longest observed attempt), at the highest
calibrated rate of $0.0037 per agent-second. So, as long as no agent spends
faster than calibration saw, no run can take the total past $90. The agent CLIs
have no enforceable dollar limit, and the timeout bounds time, not tokens.
`test_benchmark.py` checks that the runner refuses a run it cannot afford
(and starts one it can) and that the reserve covers the modelled worst case.
On the real calibration output, a $14.50 cap with $0.74 spent refused the next
run (exit 3) before any agent started. Claude's figure is its own report;
Codex's is tokens × price, with cached reads and cache writes priced
separately; the judge's is estimated from characters. Reconcile with the
provider dashboards.

## Running it (bills real models)

The tasks, gates, and calibrated config are in this directory. The only
prerequisites are credentials in `sbx secret` (`anthropic`, `openai`) and a
marsh that includes #76 (0.1.4-nightly.20261013173603.g854e312 or later). From
a checkout of `main`:

```sh
MARSH=/path/to/marsh sh evals/benchmark/start-pilot.sh
```

It prints the estimate, runs every (task, rep) into `target/bench-pilot/`
(rerunning resumes), writes `REPORT.md`, and stops its dedicated selected home
(`~/.marsh-bench`). Run it when no other real-VM suites share the Mac, since
wall time is a metric.

Curating more tasks:

```sh
python3 evals/benchmark/curate_swebench.py pool > pool.txt
python3 evals/benchmark/curate_swebench.py fetch --tasks evals/benchmark/tasks ID...
python3 evals/benchmark/curate_swebench.py self-made --tasks evals/benchmark/tasks --id ID \
  --repo URL --fix SHA --tests tests/ --prompt issue.md
# write gate.patch and gate.cmd, then (shell Kit, no agents):
python3 evals/benchmark/curate_swebench.py validate --tasks evals/benchmark/tasks ID...
```

Dry run, no VM or credentials (also what `test_benchmark.py` does):

```sh
FAKEMARSH_PLAN=plan.json python3 evals/benchmark/run.py --config evals/benchmark/config.example.json \
  --tasks evals/benchmark/fixtures --only toy-slugify --out /tmp/dry --marsh evals/common/fakemarsh.py --reps 1
```
