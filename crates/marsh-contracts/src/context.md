# marsh: starting other agents from this job

You are running as an marsh job: a fresh container in a Docker Sandbox VM,
with the project's files at their usual path. You can start other agents and
tools as tracked child jobs. Commands you may start: every registered command (`ls /run/marsh/bin` in a job).

## One child job
- Call a registered command by name from any shell or program:
    codex exec "review src/parser.rs and list bugs" </dev/null
    claude -p "write a test for parse()" </dev/null
  It runs in a new container with this job's files, under its own network
  policy and credentials; stdin, stdout, stderr and the exit status are
  relayed. `marsh run NAME [ARG...]` does the same.
- A child gets no terminal: if its stdin or stdout is a terminal it is
  refused. Give it `</dev/null` (and `| cat`).

## Parallel attempts on private copies (compare, then pick one)
    h=$(marsh split -n ::: a codex exec "fix the failing test" \
                       ::: b claude -p "fix the failing test")
    marsh join --keep <<<"$h"     # each branch's status, output and diff
    marsh join -- sh -c 'cat >/dev/null; git apply "$SPLIT_DIR/b/diff.patch"' <<<"$h"
Each `::: LABEL CMD [ARG...]` branch is a child job in its own private fork
of the files; your files change only when you apply a patch. In one line:
`marsh split -n ::: a CMD... ::: b CMD... | marsh join`.

## Parallel work on the same files (no fork)
    marsh fanout -n ::: tests make test ::: lint make lint | marsh collect
runs the branches at once in this container and prints each one's output in
order (`collect --json`, `--timing`); it exits with the first nonzero branch
status. A branch naming a registered command starts it as a child job.

## Inspect, limits, confinement
- `marsh jobs --tree` shows this job's subtree; `marsh jobs show ID` one job.
- The user narrows what you may start with `MARSH_SPAWN=a,b` (or `none`);
  `marsh run --spawn a,b NAME` narrows one child. Nothing in a job widens it.
- Limits: 8 jobs at once, depth 4, the same Kit at most twice in a row,
  4 live children per job, 64 jobs per tree, 4 distinct Kit VMs per tree;
  a child's wall time ends with its parent's. A refusal starts nothing and
  exits 125 (126 for a name outside the spawn set).
- Exported variables reach children; credential-shaped names never do.
- Shell branches (`split -b`, `fanout -b`) run only in the user's own shell;
  from a job use `::: LABEL CMD [ARG...]`.
- `bash`, `sh` and every other tool here are the image's own.

## When not to spawn
Each child costs a container, maybe a VM boot, and model tokens. Do reads,
edits and ordinary commands yourself. Spawn when the user asks for it, for
an independent second opinion, or to compare alternative changes in
parallel. Never start a copy of yourself just to continue your own task.
