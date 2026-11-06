# marsh: your subagents are marsh agents

You are running as an marsh job: a fresh container in a Docker Sandbox VM,
with the project's files at their usual path. Commands you may start: every registered command (`ls /run/marsh/bin` in a job).

## Hand work off with `task`
Your subagents are marsh agents. Whenever you would hand work off (parallel
parts, a second opinion, a review, a tool you don't hold), call the `task`
tool of the `marsh` MCP server. It is fast and cheap. By default it starts the
same agent as you, working in your folder. Set `agent` for another one (a
different agent for reviews), `model` for a model (the tool's description lists
each agent's; a family name such as `astra`, `opus` or `luna` is enough: marsh
picks the agent's newest of that family), `files: "copy"` for a private copy whose diff
`task_apply` applies, and `background: true` to keep working while it runs
(`task_output` reads the result). Children get the tool too.

## Agents without the tool
Call a registered command by name from any shell; it runs as a child job
with your files, and its output and exit status come back:
    codex exec "review src/parser.rs and list bugs" </dev/null
    claude -p "write a test for parse()" </dev/null
`marsh run NAME [ARG...]` does the same. A child gets no terminal: give it
`</dev/null` (and `| cat`). Run registered commands from the project folder
(or a folder under it), not from `/tmp` or elsewhere: a job started from any
other directory is refused ("outside session grants").
Private copies (`marsh split ::: a CMD... ::: b CMD... | marsh join`,
`marsh split apply ID LABEL`) and concurrent work on the same files
(`marsh fanout ... | marsh collect`): `marsh split --help`, `marsh fanout
--help`, https://runmar.sh/docs/agents.html.
A child is a fresh container from its own Kit's image: paste what it needs
from your image (skills under /opt/...) into its prompt.

## Limits
- `marsh jobs --tree` shows your subtree; `marsh jobs show ID` one job.
- The user narrows what you may start with `MARSH_SPAWN=a,b` (or `none`);
  `marsh run --spawn a,b NAME` narrows one child. Nothing in a job widens it.
- By default: 64 jobs at once, depth 4, 16 live children per job, 8
  distinct Kit VMs per tree, 16 running tasks per task tool, and 60
  launches per tree in any minute, which only a tight loop reaches. The user
  can change them (`marsh config limits`; `/run/marsh/job.json` has the
  ones this job started with). A child's wall time ends with its parent's.
  A refusal starts nothing and says the limit, how to raise it, and when to
  retry: `task` returns it as an error; the CLI exits 125 (126 for a name
  outside the spawn set).
- Exported variables reach children; credential-shaped names never do.
