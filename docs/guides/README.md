# Guides

After the [Quickstart](../quickstart.md), use these recipes for work in
your project:

- [Make independent changes in parallel](race-two-agents.md): separate CLI
  and API changes, with tests in each branch and on the combined tree.
- [Review a change with a panel](review-with-a-panel.md): two review
  questions about one diff.
- [Let an agent farm out work](farm-out-work.md): one child job and evidence
  you can check in the source.
- [Review split results in a pipeline](add-a-judge.md): optional comments
  from a downstream agent before you apply patches.
- [Set up your marsh home](set-up-your-home.md): personal skills, dotfiles,
  shared and per-Kit MCP connections and mounted directories.
- [Build your own agent on marsh](build-a-personal-agent.md): package your
  agent as a Kit next to the packaged `pi`, with private skills and tools kept separate.

`split { ... } | join` creates project copies. `fanout { ... } | collect`
runs on shared files. In another shell, or inside an agent job, use the CLI
forms: `marsh split ::: LABEL CMD ARG... | marsh join` and
`marsh fanout ::: LABEL CMD ARG... | marsh collect`.

For the full behavior and options, see [Split and join](../split.md),
[Fanout and collect](../fanout.md), and [Agents running agents](../agents.md).

Preview: [Docker Cloud](../cloud.md) runs a `cloud { ... }` block in a cloud
sandbox and writes its file changes back. Requires a Docker Cloud subscription
with sandbox access.
