# Let an agent farm out work

Ask an agent to delegate one code question, then verify the child's answer.
The child runs from the parent's shell tool and appears beneath it in the
job tree.

Use an existing repository with a CLI and complete the
[Quickstart](../quickstart.md) first. This example uses Claude for both jobs
with the same Anthropic credential.

## Start with one subtask

At the **marsh prompt**:

<!-- doc-test: shell replace=["claude -p \"$prompt\"", "set -e\nfixture pipeline '|' bash -o pipefail -c 'marsh run --no-spawn fixture identity </dev/null | cat'"] stdout="└─ fixture identity" -->
<!-- doc-test: shell replace=["claude -p \"$prompt\"", "set -e\nfixture pipeline '|' bash -o pipefail -c 'marsh run --no-spawn fixture pipeline \"|\" bash -c \"shell -c true\" </dev/null | cat'"] status=126 stderr="spawn refused: shell not in this job's spawn set (MARSH_SPAWN)" -->
```sh
prompt='Use your shell tool to run this one child command:

marsh run --no-spawn claude -p \
  "Find the code that writes CLI output and a test that exercises it.
Cite files and lines. If no test exists, say where you looked.
Do not edit files." </dev/null | cat

Give the shell call enough time for a cold VM start.
Read the files the child cites and check its answer yourself.
Report what you verified, including any disagreement with the child.
Do not edit files or start any other jobs.'
claude -p "$prompt"
marsh jobs --tree
marsh results
```

Look for a child beneath the parent in the tree. The parent's claim that it
delegated is not evidence of a launch. Job records show who started whom and
their exit statuses; check the cited code to judge the answer itself. If no
child appears, read the parent's report for an error before trying again.

`</dev/null | cat` keeps a terminal off the child's stdin and stdout, as
required for child jobs. `--no-spawn` prevents that child from starting
further jobs.

## What the child can reach

A direct child shares the parent's project files. Either agent can edit or
delete them despite the read-only request. The guest home, including saved
logins and settings, is also shared. The child uses its own Kit's credentials
and network policy; proxy-managed provider keys stay with Docker Sandboxes.

For work that should edit files, ask the parent to use a split and return
the patch for inspection. Inside a job, it must use the
`marsh split ::: LABEL CMD ARG... | marsh join` form. See
[Split from inside an agent](../agents.md#split-from-inside-an-agent).

## Keep delegation bounded

Each child starts another billed agent run. Avoid having the parent and
child write the same files at once. Set a shell-tool timeout long enough
for startup and the task; first use of a Kit can take minutes.

Ctrl-C stops the foreground tree. A parent that ends also cancels its
remaining children. If a tool times out, inspect the job records rather
than assuming its child finished.

For launch restrictions, see
[Narrowing what an agent may start](../agents.md#narrowing-what-an-agent-may-start).
[Agents running agents](../agents.md#limits-and-messages) lists the job
limits and refusal statuses.

## Next

[Make independent changes in parallel](race-two-agents.md) runs editing
jobs on separate copies. [Review a change with a panel](review-with-a-panel.md)
gives reviewers different questions about the same diff.
