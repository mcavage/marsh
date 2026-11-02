# marsh: your subagents are marsh agents

You are running as an marsh job: a fresh container in a Docker Sandbox VM,
with the project's files at their usual path. Commands you may start: every registered command (`ls /run/marsh/bin` in a job).

## Hand work off with `task`
Your subagents are marsh agents. Whenever you would hand work off (parallel
parts, a second opinion, a review, a tool you don't hold), call the `task`
tool of the `marsh` MCP server. It is fast and cheap. By default it starts the
same agent as you, working in your folder. Set `agent` for another one (a
different agent for reviews), `files: "copy"` for a private copy whose diff
`task_apply` applies, and `background: true` to keep working while it runs
(`task_output` reads the result). Children get the tool too.

## Agents without the tool
Call a registered command by name from any shell; it runs as a child job
with your files, and its output and exit status come back:
    codex exec "review src/parser.rs and list bugs" </dev/null
    claude -p "write a test for parse()" </dev/null
`marsh run NAME [ARG...]` does the same. A child gets no terminal: give it
`</dev/null` (and `| cat`).
- Private copies, compared:
    h=$(marsh split -n ::: a codex exec "fix the test" ::: b claude -p "fix the test")
    marsh join --keep <<<"$h"        # each branch's status, output and diff
    marsh split apply ID b           # apply branch b's diff (ID is in the handle)
  For work longer than your tool call's timeout, `marsh split -d ...` prints
  the handle at once and `marsh join --keep <<<"$h"` waits later.
- Same files, at once: `marsh fanout -n ::: t make test ::: l make lint | marsh collect`.
A child is a fresh container from its own Kit's image: paste what it needs
from your image (skills under /opt/...) into its prompt.

## Limits
- `marsh jobs --tree` shows your subtree; `marsh jobs show ID` one job.
- The user narrows what you may start with `MARSH_SPAWN=a,b` (or `none`);
  `marsh run --spawn a,b NAME` narrows one child. Nothing in a job widens it.
- 16 jobs at once, depth 4, the same Kit at most twice in a row, 8 live
  children per job, 128 jobs per tree, 4 distinct Kit VMs per tree; a
  child's wall time ends with its parent's. A refusal starts nothing: `task`
  returns an error that says why; the CLI exits 125 (126 for a name outside
  the spawn set).
- Exported variables reach children; credential-shaped names never do.
