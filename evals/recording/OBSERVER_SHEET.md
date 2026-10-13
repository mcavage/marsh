# Observer sheet

Participant: P__  Date: ____  Moderator: ____  Observer: ____
`marsh --version`: ____  `sbx version`: ____  macOS / chip: ____
Docs snapshot (date, commit): ____  Practice repo commit: ____
Face video opted in: [ ] yes [ ] no

Timestamps are recording time (mm:ss). Help levels and outcome classes are
defined in [SCRIPT.md](SCRIPT.md#help-ladder).

## Inventory

| | Before | After |
|---|---|---|
| `brew list --versions \| grep -Ei 'sbx\|marsh'` | | |
| `ls -d ~/.marsh ~/.sbx` | | |
| `command -v sbx marsh` | | |
| `marsh status` / `sbx ls` (after only) | — | |

## Per task

Outcome: U unassisted / P prompted / A assisted / NF not finished / OB onboarding bypassed / RE researcher environment (spare Mac).

| Task | Start | First right command | End | Elapsed | Waiting | Active | Outcome | Max help | Docs pages |
|---|---|---|---|---|---|---|---|---|---|
| 1 install | | | | | | | | | |
| 2 first agent | | | | | | | | | |
| 3 race + apply | | | | | | | | | |
| 4 what happened | | | | | | | | | |
| 5 clean up | | | | | | | | | |

Interventions (minute, level, what was said):

## Predictions (verbatim)

- Task 2, "What do you expect to happen, and where will it run?":
- Task 3, "What do you expect this to change, and where?":

## Task 3

Content snapshot (`shasum` of every file except `.git`/`.marsh`) before the task: saved as ____
After the agents finished, before apply: [ ] no observed change [ ] changed: ____
`fswatch` log: [ ] not run [ ] no observed writes [ ] writes: ____

- Chose: [ ] split [ ] fanout [ ] other: ____
- [ ] Gated branches on `make test` unprompted
- [ ] Noticed test-file edits (if any)
- Applied with: [ ] `git apply` [ ] `join -- …` [ ] by hand [ ] not applied
- [ ] Removed the kept split (`marsh splits rm`) or understood `--keep`

| Score | Pass? | Evidence |
|---|---|---|
| Isolation (safety, not success): no observed change before apply | | |
| Execution: both agents ran | | |
| Interpretation: said correctly which branch passed and what changed | | |
| Application: applied, and named its branch correctly (provenance) | | |
| Correctness: `validate.sh` passes | | |

## Part 6: what changed where

Mark each statement S (spontaneous, during tasks) or P (probed, here).

1. Files, shell, and agents (attach drawing):
2. Which files changed, and when:
3. Where the other attempt is now:
4. What is still running or on disk:

## Events

| Time | Task | Labels (docs / command / concept / product / infrastructure / expected) | What happened | User impact and recovery | Quote |
|---|---|---|---|---|---|
| | | | | | |

## Debrief notes

1. One-sentence description:
2. Most confusing / most reassuring:
3. How they chose between fixes:
4. Would not trust it with:
5. Would use it for:
6. Expected but missing:

SUS score: ____  Key spend: Anthropic $____ OpenAI $____  Keys revoked: [ ]
