# Guides

Each guide does one job from start to finish: the commands, what comes back,
what it costs, and what goes wrong. The output shown comes from real runs.
Every marked example also runs in marsh's test suite, with a stand-in command
where the guide calls a model.

| Guide | Use it when |
|---|---|
| [Race two agents on one bug](race-two-agents.md) | A test fails and a second attempt is worth a second model run |
| [Let a judge pick the winner](add-a-judge.md) | Both attempts pass and you want a reasoned choice |
| [Review a change with a panel](review-with-a-panel.md) | A risky change needs several reviewers and your checks, at once |
| [Let an agent farm out work](farm-out-work.md) | You want an agent to run races, reviews, and subtasks itself |

They use four commands:

- `split { ... } | join` runs each branch on a private copy of your project and
  prints each branch's output and patch. See [Split and join](../split.md).
- `fanout { ... } | collect` runs branches side by side on your real files.
  See [Fanout and collect](../fanout.md).

From inside an agent, or from bash or zsh, the same commands are
`marsh split ::: LABEL CMD ARG... | marsh join` and
`marsh fanout ::: LABEL CMD ARG... | marsh collect`.
