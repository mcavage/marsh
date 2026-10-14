# Fanout and collect

`fanout` runs commands concurrently on the same project files. `collect`
waits for them and prints labelled output in declaration order. From an
[installed marsh shell](shell.md), you can run a review alongside tests:

<!-- doc-test: shell replace=["fanout {\n  races: codex exec \"find data races in net/\"\n  leaks: claude -p \"find goroutine leaks in net/\"\n  tests: make test\n} | collect", "fanout {\n  races: fixture identity\n  leaks: fixture identity\n  tests: printf 'tests finished\\n'\n} | collect"] stdout="== tests (complete) ==\ntests finished" -->
```sh
fanout {
  races: codex exec "find data races in net/"
  leaks: claude -p "find goroutine leaks in net/"
  tests: make test
} | collect
```

Ctrl-C stops every branch, along with every process the branch started. The
exit status of `fanout | collect` is the first nonzero branch status in the
order written.

## When to use split instead

Use `fanout` when commands can safely share their files. Branches write to
your live project, so they can overwrite each other's changes. Tests and
linters may share caches or build output, and a review prompt does not
prevent an agent from editing or deleting files. Use [`split`](split.md)
when you need private project copies and patches to inspect.

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

The pipeline carries a structured result between `fanout` and `collect`;
ordinary commands belong after `collect`. Normal `pipefail` rules apply to
later pipeline stages.

## In the marsh shell

`label: commands` names a branch, as in `split { }`. Inside the braces, a new
branch starts after any of these:

- the opening `{`
- a newline
- a comma
- a `;` followed by `LABEL:`

Anything else inside a branch is ordinary shell syntax, including other `;`, `&&`,
and groups.

## From any shell

Use these forms in scripts, in a non-marsh shell, or from inside a job.

<!-- doc-test: host stdout="== count (complete) ==\n2" -->
<!-- doc-test: shell stdout="== count (complete) ==\n2" -->
```sh
marsh fanout -n -b count='wc -l < notes.txt' \
                -b contents='cat notes.txt' | marsh collect
```

This example assumes `notes.txt` contains two lines. An argv branch avoids
shell parsing:

<!-- doc-test: shell replace=["codex exec 'review notes.txt'", "fixture identity"] stdout="== review (complete) ==" -->
```sh
marsh fanout -n ::: review codex exec 'review notes.txt' ::: count wc -l notes.txt | marsh collect
```

Branch forms:

- `-b LABEL=STRING` runs STRING in a fresh Brush shell in the shell VM. A job
  cannot use `-b`. These shell commands have your shell VM user's access to
  mounted files; the branch does not confine them.
- `::: LABEL CMD ARG...` runs one command. The next `:::` ends the branch.
  A registered name starts a job; other commands run in the caller's
  environment. An argument cannot be the literal `:::`.

CLI labels are unique ignoring case, up to 64 characters, and begin with a
letter or `_`. Following characters may be letters, digits, `_`, `-`, or `.`.

Where it runs:

- From a host terminal, marsh runs the whole fanout in the shell VM for the
  current project.
- From inside a job, it runs in that job's container, and only `:::`
  branches are allowed.

Input:

- stdin is read once (up to 64 MiB), and every branch gets the same bytes.
- `-n`, or a terminal on stdin, gives every branch empty input.

## Jobs in `marsh jobs --tree`

Each branch runs with `FANOUT_BRANCH=ID/LABEL` exported. The jobs it starts
appear under a `fanout (LABEL, ...)` node in `marsh jobs --tree`.

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

Oversized input is refused before branches start. Exceeding the output budget
cancels and reaps every branch and its process group, with no collected
output. Errors still appear on stderr. More than 16 branches fails with
`fanout: at most 16 branches`.

The daemon runs only 8 jobs at once, across all callers. If more than 8
jobs would be live at once, extra calls are refused with
`capacity: 8 jobs` ([Troubleshooting](troubleshooting.md#a-job-was-refused)).

Next: [Split and join](split.md), [Agents running agents](agents.md).
