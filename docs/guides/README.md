# Guides

The manual pages describe each command. These guides show how to do a job with
them, start to finish, and say what to do and what to watch for. Every example
runs in the test suite, with a stand-in command where a guide calls a model.

- [Race two agents on one bug](race-two-agents.md): run two agents at once on
  private copies of your project, let each prove its fix with your tests, and
  apply the patch you choose. Start here.
- [Let a judge pick the winner](add-a-judge.md): when both attempts pass, a
  third agent reads the results and names the patch to apply, or applies it.
- [Review a change with a panel](review-with-a-panel.md): one reviewer per
  concern, plus your own checks, at once and without touching your files.

All three use the same shape: `split` for the work that needs a private copy,
`join` to read it and choose, and ordinary Bash in between. For what each
command does on its own, see [Split and join](../split.md) and
[Fanout and collect](../fanout.md).
