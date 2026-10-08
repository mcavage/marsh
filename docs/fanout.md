# Fanout and collect

`fanout` runs commands side by side on your project files. `collect` prints
their output in the order you wrote them, labelled. Total time is about that
of the slowest branch, not the sum.

```sh
fanout {
  races: codex exec "find data races in net/"
  leaks: claude -p "find goroutine leaks in net/"
  tests: make test
} | collect
```

```text
== races (complete) ==
net/dial.go:88 reads d.timeout without holding d.mu.

== leaks (complete) ==
None found: every goroutine in net/ exits on ctx.Done().

== tests (failed: 2) ==

== tests stderr ==
--- FAIL: TestDialTimeout (0.31s)
```

Ctrl-C stops every branch. The exit status of `fanout | collect` is the first
nonzero branch status in the order written, here 2.

Each agent branch is a job in its own container and authenticates as it does
when you run it directly, through Docker Sandboxes ([Kits](kits.md#signing-in)).

## When to use split instead

`fanout` makes no private copies. Branches share your project, so their writes
are real and can race: two branches that write the same file overwrite each
other. An agent branch can also delete project files.

Use `fanout` for reads and checks: tests, lint, reviews. Use
[`split`](split.md) when branches write files, or when you want a patch to
review before anything changes.

## Output and status

For each branch, in the order written, `collect` prints `== LABEL (state) ==`,
the branch's stdout, and a blank line. A failed branch also gets its stderr
under `== LABEL stderr ==`, as [`join`](split.md) does. Everything goes to
stdout, so a header always precedes its stderr.

| `collect` option | Effect |
|---|---|
| `--stderr` | Show stderr for successful branches too. It is hidden by default. |
| `--timing` | Append a `Timing:` block with each branch's wall time and a `total` line, in ms. |
| `--json` | Print one JSON document. Branch output must be UTF-8. |

On a terminal, `fanout` prints the same output itself, so `| collect` is
optional there.

## In the marsh shell

`label: commands` names a branch, as in `split { }`. Inside the braces, a new
branch starts after any of these:

- the opening `{`
- a newline
- a comma
- a `;` followed by `LABEL:`

Anything else inside a branch is ordinary Bash, including other `;`, `&&`,
and groups.

## From any shell

Use these forms in scripts, in a non-marsh shell, or from inside a job.

```sh
marsh fanout -n -b test='make test' -b lint='make lint' | marsh collect
marsh fanout -n ::: a codex exec 'review src/a.rs' ::: b pi -p 'review src/b.rs' | marsh collect
```

Branch forms:

- `-b LABEL=STRING` runs STRING with the marsh shell in the shell VM. A job
  cannot use `-b`.
- `::: LABEL CMD ARG...` runs one command. The next `:::` ends the branch.
  A registered name starts a job.

Where it runs:

- From a host terminal, marsh runs the whole fanout in the shell VM for the
  current project.
- From inside a job, it runs in that job's container, and only `:::`
  branches are allowed.

Input:

- stdin is read once (up to 64 MiB), and every branch gets the same bytes.
- `-n`, or a terminal on stdin, gives every branch empty input.

## Jobs in `marsh jobs --tree`

Each branch runs with `FANOUT_BRANCH=ID/LABEL` exported. The jobs a branch
starts appear under one `fanout (LABEL, ...)` node, as a split's jobs do. The
node for the example above:

```text
ID        STARTED   STATE      EXIT      RUN  COMMAND
3e0c1a7b  1m ago    finished      0    1m02s  fanout (races, leaks)
```

Two nesting cases:

- A fanout inside a split branch runs on that branch's fork. It is drawn
  under the branch ([split](split.md#a-fanout-inside-a-branch)).
- A split branch does not inherit an enclosing fanout's `FANOUT_BRANCH`, so
  its jobs are not drawn under that fanout.

## Limits

| Limit | Value |
|---|---|
| Branches | 16 |
| Input | 64 MiB |
| Output, all branches together | 16 MiB |

Crossing a limit cancels every branch and prints nothing. More than 16
branches fails with `fanout: at most 16 branches`.

The daemon runs only 8 jobs at once, across all callers. If more than 8
branches call registered commands, the extra branches are refused with
`capacity: 8 jobs` ([Troubleshooting](troubleshooting.md#a-job-was-refused)).

Next: [Split and join](split.md), [Agents running agents](agents.md).
