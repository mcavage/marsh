# Review a change with a panel

Before you merge, get several reviewers on the change at once: one agent per
concern, plus your own checks, all running while you read. This guide is for a
change that already exists on your branch.

The rule for the whole page: **agents go in `split`, deterministic checks can
go in `fanout`.** A `split` gives each branch a private fork, so a reviewer that
edits a file by mistake changes only its own copy. A `fanout` runs on your real
files, which is right for a linter and wrong for an agent.

## The panel

<!-- doc-test: shell replace=["split {\n  security: claude -p \"Review the changes on this branch for security problems. Do not edit files.\"\n  races:    codex exec \"Review the changes on this branch for data races and misuse of concurrency. Do not edit files.\"\n  tests:    make test\n} | join\n", "echo gamma >> notes.txt\nsplit {\n  security: fixture identity\n  races:    fixture identity\n  tests:    grep -q gamma notes.txt\n} | join | tee panel.out\ngrep -q '^== security (exited 0' panel.out\ngrep -q '^== races (exited 0' panel.out\ngrep -q '^== tests (exited 0' panel.out\nrm panel.out\n"] -->
```sh
split {
  security: claude -p "Review the changes on this branch for security problems. Do not edit files."
  races:    codex exec "Review the changes on this branch for data races and misuse of concurrency. Do not edit files."
  tests:    make test
} | join
```

Each reviewer starts from the same snapshot of your working tree, with your
uncommitted changes, so they all see exactly what you see. The three branches
run together. `join` prints each reviewer's findings under its label, and
`tests` shows your test results beside them.

What to say in the prompts:

- **Name the concern.** "Review for security problems" finds different things
  than "review this change". One concern per reviewer keeps each answer short.
- **Say what to look at.** "The changes on this branch" points the agent at the
  diff. Add the base if it is not obvious: "compared with `main`".
- **Say "do not edit files".** A fork contains any slip, but an agent that
  edits files is not reviewing.

Different agents are different opinions, not a guarantee. Use them to widen
what gets looked at, and read the findings yourself before you act on them.

## Run your checks at once

Linters, type checks, and tests only read your files, so they do not need
forks. `fanout` runs them side by side on your project and `collect` gathers
the output:

<!-- doc-test: shell -->
```sh
fanout {
  lint:  grep -q alpha notes.txt
  types: grep -q beta notes.txt
} | collect
```

Replace the two commands with your own, for example `make lint` and
`make typecheck`. The wall time is the slowest check, not the sum. See
[Fanout and collect](../fanout.md) for how failures and output are reported.

## Combine them

A branch of a split can itself be a fanout. Put your checks in one branch and
the agents in the others, and `join` reports all of it together:

<!-- doc-test: shell replace=["split {\n  security: claude -p \"Review the changes on this branch for security problems. Do not edit files.\"\n  checks:   fanout { lint: make lint, types: make typecheck } | collect\n} | join\n", "split {\n  security: fixture identity\n  checks:   fanout { lint: grep -q alpha notes.txt, types: grep -q beta notes.txt } | collect\n} | join | tee panel.out\ngrep -q '^== checks (exited 0' panel.out\nrm panel.out\n"] -->
```sh
split {
  security: claude -p "Review the changes on this branch for security problems. Do not edit files."
  checks:   fanout { lint: make lint, types: make typecheck } | collect
} | join
```

A fanout inside a branch runs on that branch's fork. `marsh jobs --tree` draws
it under the branch ([A fanout inside a branch](../split.md#a-fanout-inside-a-branch)).

## When this is worth it

A panel costs one model run per reviewer. It pays off on changes where a miss is
expensive (auth, concurrency, migrations) and is not worth it for a typo fix.
Start with two reviewers on the riskiest change you have and see what they
find that you did not.

## Next

To have an agent decide which approach to take, see
[Race two agents on one bug](race-two-agents.md).
