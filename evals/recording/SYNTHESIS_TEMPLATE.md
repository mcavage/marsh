# Synthesis: five developers meet marsh

Sessions: P1 ____ … P5 ____  marsh versions: ____  Written by: ____

## 1. Summary (five bullets, at most)

The top findings, most severe first, each with how many of the five it
affected (for example "4/5 used fanout for the race; 2 of them had agents
overwrite the real file"). Counts, not rates.

## 2. Triage rules (from SCRIPT.md; internal, not statistical)

| Rule | Result | Triggered? |
|---|---|---|
| Fewer than 4/5 install unassisted | | |
| Fewer than 3/5 succeed at task 3 (execution, interpretation, application, correctness; help ≤ 2) | | |
| Anyone wrong on isolation or provenance in part 6, or applies a patch from a different branch than they believe | | |
| Any loss of work, or a real file changed during a race the participant thought was isolated | | |

## 3. Task outcomes

U unassisted / P prompted / A assisted / NF not finished / OB onboarding bypassed.

| Task | P1 | P2 | P3 | P4 | P5 | Active time, median of finishers | Waiting time, median |
|---|---|---|---|---|---|---|---|
| 1 install | | | | | | | |
| 2 first agent | | | | | | | |
| 3 race + apply | | | | | | | |
| 4 what happened | | | | | | | |
| 5 clean up | | | | | | | |

Task 3 scores (✓/✗):

| | P1 | P2 | P3 | P4 | P5 |
|---|---|---|---|---|---|
| Isolation | | | | | |
| Execution | | | | | |
| Interpretation | | | | | |
| Application (provenance) | | | | | |
| Correctness (`validate.sh`) | | | | | |

One line per participant on the path they took (for example "P2: README →
quickstart → fanout → noticed real-file edits → split → applied by hand").

## 4. Findings

One row per distinct problem. Merge duplicates across participants first.
Severity: **4** blocks the task or risks work or trust; **3** major delay or
wrong mental model; **2** minor delay; **1** cosmetic.

| # | Finding | Category | Evidence (P#, timestamp, quote) | Count | Severity | Where (docs page, CLI message, behavior) | Proposed change | Owner |
|---|---|---|---|---|---|---|---|---|
| 1 | | | | /5 | | | | |

Infrastructure problems appear here too when they affected the participant;
put their technical causes in section 10.

## 5. Mental models

Each participant's predictions compared with what happened, and their
part 6 answers. Keep spontaneous and probed statements apart. Group the
drawings as correct, partly correct, or wrong, and quote the clearest wrong
model.

## 6. Trust and safety

What participants would not trust marsh with, and why. Every case where
someone believed their files had or had not changed and was wrong.

## 7. Documentation path

Which pages participants actually used, in what order, and where they left
the docs (search, AI assistant, `--help`). Compare with the agent-reach eval
(`evals/reach/`): did the humans stall on the same steps as the models?

## 8. What worked

Moments of delight or quick success worth keeping as they are.

## 9. Recommendations

Ranked by severity × count. Link each to an issue.

## 10. Method notes

Deviations from the script, infrastructure failures, rescues (onboarding
bypassed), help given beyond the ladder, and anything that limits these
conclusions. This was one diagnostic round with five participants. It may
have missed problems that are rare or specific to one kind of user, it
estimates no rates, and it is not evidence of safety. Note the frozen
versions and docs snapshot.
