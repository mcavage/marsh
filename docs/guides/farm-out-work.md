# Let an agent farm out work

An agent running in marsh can start other agents the same way it runs
`make`: from its shell tool. Here Claude races Codex and a second Claude on a
failing test, applies a patch that passes, and reports:

```console
marsh-0.5$ claude -p "make test fails. Fix it the way AGENTS.md describes for competing fixes. Report in five lines or fewer."
`marsh split` ran codex and claude on private copies; both produced the byte-identical one-line patch to `slug.py` (only file touched, no test files).

Join applied codex's patch first and `make test` passed, so claude's was not needed.

Applied change: `re.sub(r"[^a-z0-9]", ...)` → `re.sub(r"[^a-z0-9]+", "-", title.lower()).strip("-")` — collapses runs of non-alphanumerics and trims edge hyphens.

Verified in the working tree: `make test` runs 3 tests, OK. Change is uncommitted.

Split dir kept at `.marsh/split/b8aa7b4db45f` (remove with `marsh splits rm b8aa7b4db45f`).
marsh-0.5$ marsh jobs --tree
ID        STARTED   STATE      EXIT      RUN  COMMAND
708d0681  27s ago   finished      0    27.2s  claude -p "make test fails. Fix it the way AGENTS.md describes …
85f5c50a  18s ago   finished      0    10.0s  ├─ claude -p "make test fails with this output:?FAIL: test_punctua…
59bb822e  18s ago   finished      0    11.1s  └─ codex exec "make test fails with this output:?FAIL: test_punctu…
```

This is a real run on a three-test Python project with warm VMs; the times
are not a benchmark. Claude ran the `marsh split` command from the
[AGENTS.md snippet](#the-agentsmd-snippet) below, with the failing test output
in each child's prompt (the `?` is a newline in the tree view). Each child was
a separate job in its own container, working on its own copy of the project.
Claude also chose to keep the split's copies; `marsh splits rm` from your
shell removes them.

The children are processes that marsh starts, tracks, and cleans up, not
subagents of the calling harness. Any program in a job can start them: Claude
Code's Bash tool, Codex's shell tool, Pi, or a script. They do not use the
caller's subagent slots.

## What an agent can run

Inside a job, every registered command (`claude`, `codex`, `pi`, `shell`, and
any Kit you added) and `marsh` are on `PATH`. Each agent call becomes a child
job in the same tree. Three shapes cover most uses:

| Shape | Command | Files the children see |
|---|---|---|
| One subtask | `codex exec "..." </dev/null` | the caller's files |
| Several at once | `marsh fanout -n ::: a CMD... ::: b CMD... \| marsh collect` | the caller's files |
| Competing attempts | `marsh split -n ::: a CMD... ::: b CMD... \| marsh join` | a private copy each |

### One subtask

The child runs in its own container with the caller's files, under its own
Kit's credentials and network policy. Its stdout, stderr, and exit status come
back as if it were a local command:

<!-- doc-test: shell replace=["codex exec \"List the inputs slugify() in slug.py handles wrongly. Do not edit files.\" </dev/null\n", "fixture pipeline '|' bash -c 'fixture identity </dev/null'\n"] stdout="\"cwd\":" -->
```sh
codex exec "List the inputs slugify() in slug.py handles wrongly. Do not edit files." </dev/null
```

`</dev/null` matters. marsh refuses a child whose stdin or stdout is a
terminal, and some agent tools run commands with a terminal attached.

To stop the child from starting agents of its own, run it through
`marsh run --no-spawn`:

<!-- doc-test: shell replace=["marsh run --no-spawn codex exec \"List the inputs slugify() in slug.py handles wrongly.\" </dev/null\n", "fixture pipeline '|' bash -c 'marsh run --no-spawn fixture pipeline \"|\" bash -c \"shell -c true\" </dev/null'\n"] status=126 stderr="spawn refused: shell not in this job's spawn set (MARSH_SPAWN)" -->
```sh
marsh run --no-spawn codex exec "List the inputs slugify() in slug.py handles wrongly." </dev/null
```

### Several read-only subtasks at once

`marsh fanout` runs each `::: LABEL CMD ARG...` branch at the same time.
`marsh collect` prints their output in the order written and exits with the
first nonzero status:

<!-- doc-test: shell replace=["marsh fanout -n \\\n  ::: security codex exec \"Review the uncommitted changes for security bugs. Do not edit files.\" \\\n  ::: coverage claude -p \"List behavior in the uncommitted changes that no test covers. Do not edit files.\" \\\n  | marsh collect\n", "fixture pipeline '|' bash -c 'marsh fanout -n ::: security fixture identity ::: coverage fixture identity | marsh collect'\n"] stdout="== coverage (complete) ==" -->
```sh
marsh fanout -n \
  ::: security codex exec "Review the uncommitted changes for security bugs. Do not edit files." \
  ::: coverage claude -p "List behavior in the uncommitted changes that no test covers. Do not edit files." \
  | marsh collect
```

With "Two lines at most." added to each prompt, on a change that added an
`eval` to `slug.py`:

```text
== security (complete) ==
[P1] `slug.py:12`: `eval(f.read())` allows arbitrary code execution if an attacker controls the titles file. Use `json.load()` or `ast.literal_eval()` and validate the result.
No files edited; no child jobs spawned.

== coverage (complete) ==
`load_titles()` has no test at all — nothing covers reading a well-formed file, and nothing covers its `eval()` of file contents (arbitrary code execution on untrusted input, plus unhandled `SyntaxError`/`NameError` on malformed data).

Also untested: missing/unreadable `path` (uncaught `OSError`), and the fact that no `export_titles()` exists in the repo to produce the format this claims to read.
```

`collect` hides a successful branch's stderr (Codex logs its progress
there); `--stderr` shows it.

Fanout makes no copies. Two branches that write the same file overwrite each
other, and nothing stops an agent branch from editing. Use it for questions
and reviews.

### Competing fixes, gated on your tests

`marsh split` gives each branch a private copy of the caller's files and waits
for every branch to finish. The command after `marsh join --` then gets each
branch's patch in `$SPLIT_DIR/LABEL/diff.patch` and its changed paths in
`$SPLIT_DIR/LABEL/files`. This one puts the failing output in the prompt, then
tries Codex's patch and Claude's in that order, and keeps the first that passes
`make test`:

<!-- doc-test: shell replace=["task=\"make test fails with this output:\n$(make test 2>&1 | tail -30)\nFix the code, not the tests. Run make test before you finish. Do not start other agents.\"\nmarsh split -n ::: codex codex exec \"$task\" ::: claude claude -p \"$task\" \\\n  | marsh join -- sh -c '\n      cat\n      for b in codex claude; do\n        p=$SPLIT_DIR/$b/diff.patch\n        test -f \"$p\" || continue\n        grep -qx \"exited 0\" \"$SPLIT_DIR/$b/status\" || continue\n        if grep -q test \"$SPLIT_DIR/$b/files\"; then echo \"$b changed a test file; skipped\"; continue; fi\n        git apply \"$p\" || continue\n        if make test; then echo \"applied $b\"; exit 0; fi\n        git apply -R \"$p\" || { echo \"could not undo $b; stopping\"; exit 2; }\n      done\n      echo \"no patch passed\"; exit 1'\n", "cat > gate.sh <<'EOF'\nmarsh split -n \\\n  ::: x fixture project-write test_x.txt hi \\\n  ::: y fixture project-write notes.txt $'alpha\\nbeta\\ndelta\\n' \\\n  ::: z fixture project-write notes.txt $'alpha\\nbeta\\ngamma\\n' \\\n  | marsh join -- sh -c '\n      cat\n      for b in x y z; do\n        p=$SPLIT_DIR/$b/diff.patch\n        test -f \"$p\" || continue\n        grep -qx \"exited 0\" \"$SPLIT_DIR/$b/status\" || continue\n        if grep -q test \"$SPLIT_DIR/$b/files\"; then echo \"$b changed a test file; skipped\"; continue; fi\n        git apply \"$p\" || continue\n        if grep -q gamma notes.txt; then echo \"applied $b\"; exit 0; fi\n        git apply -R \"$p\" || { echo \"could not undo $b; stopping\"; exit 2; }\n      done\n      echo \"no patch passed\"; exit 1'\nEOF\nfixture pipeline '|' bash gate.sh | tee gate.out\ngrep -q '^x changed a test file; skipped' gate.out && grep -q '^applied z' gate.out && grep -q gamma notes.txt && ! grep -q delta notes.txt && test ! -e test_x.txt && rm gate.sh gate.out\n"] -->
```sh
task="make test fails with this output:
$(make test 2>&1 | tail -30)
Fix the code, not the tests. Run make test before you finish. Do not start other agents."
marsh split -n ::: codex codex exec "$task" ::: claude claude -p "$task" \
  | marsh join -- sh -c '
      cat
      for b in codex claude; do
        p=$SPLIT_DIR/$b/diff.patch
        test -f "$p" || continue
        grep -qx "exited 0" "$SPLIT_DIR/$b/status" || continue
        if grep -q test "$SPLIT_DIR/$b/files"; then echo "$b changed a test file; skipped"; continue; fi
        git apply "$p" || continue
        if make test; then echo "applied $b"; exit 0; fi
        git apply -R "$p" || { echo "could not undo $b; stopping"; exit 2; }
      done
      echo "no patch passed"; exit 1'
```

What the gate does and does not do:

- It skips a branch whose agent did not exit 0, and a patch whose changed
  paths contain `test`. Change that pattern to match where your tests live;
  as written it also skips `contest.py`.
- It undoes a patch that fails and stops if the undo fails. It is not a
  transaction: anything `make test` writes stays, and a test that rewrites a
  tracked file can make the undo fail.
- It checks that tests pass, not that the patch is right. `cat` prints every
  patch first, so the calling agent can read the one it applied.
- It runs in the calling agent's container, so `make test` needs the tools
  your tests use there. The packaged claude, codex, and pi Kits include git,
  make, gcc, Python 3, and Node.js.
- When it exits 0, marsh deletes the split's copies. Otherwise it keeps them
  and prints `inspect, then: marsh splits rm ID`.

To have a third agent choose instead, pipe the join output to it
([Let a judge pick](add-a-judge.md)).

## The AGENTS.md snippet

Every marsh job already gets a short note on these commands
(`/run/marsh/context.md`, see [what agents are told](../agents.md#what-agents-are-told)).
It explains how, not when. Put the snippet below in your project's
`AGENTS.md` (read by Codex and Pi) and `CLAUDE.md` (Claude Code; a line
`@AGENTS.md` imports it). Replace `make test` with your test command.

Children read the same file, since they see the same project. The rules tell
them not to delegate again when their prompt says so.

````markdown
## Handing work to other agents (marsh)

If `$MARSH_JOB` is set, you are running as a marsh job. Then `claude`,
`codex`, `pi`, and `marsh` are on your PATH, and each agent call starts a
child job: a new container that sees these project files, with its own
credentials and no terminal.

Do reads and small edits yourself. Hand off a subtask only when it is
independent of what you are doing, or when a second attempt or a second
opinion is worth another model run. If your own prompt says "Do not start
other agents", do not.

Every prompt you give a child states the problem (paste the failing output),
the files it may change, and how to check its work, and ends with "Do not
start other agents." (`marsh run --no-spawn` enforces that for a direct
child.)

One read-only subtask, on the same files as you:

```sh
marsh run --no-spawn codex exec "Find every caller of parse() and say which pass untrusted input. Do not edit files." </dev/null
```

Read-only subtasks at once, on the same files:

```sh
marsh fanout -n \
  ::: security codex exec "Review the uncommitted changes for security bugs. Do not edit files. Do not start other agents." \
  ::: coverage claude -p "List behavior in the uncommitted changes that no test covers. Do not edit files. Do not start other agents." \
  | marsh collect
```

Anything that edits files goes on private copies. This tries each patch in
turn and keeps the first that passes `make test`; join prints every patch
first:

```sh
task="make test fails with this output:
$(make test 2>&1 | tail -30)
Fix the code, not the tests. Run make test before you finish. Do not start other agents."
marsh split -n ::: codex codex exec "$task" ::: claude claude -p "$task" \
  | marsh join -- sh -c '
      cat
      for b in codex claude; do
        p=$SPLIT_DIR/$b/diff.patch
        test -f "$p" || continue
        grep -qx "exited 0" "$SPLIT_DIR/$b/status" || continue
        if grep -q test "$SPLIT_DIR/$b/files"; then echo "$b changed a test file; skipped"; continue; fi
        git apply "$p" || continue
        if make test; then echo "applied $b"; exit 0; fi
        git apply -R "$p" || { echo "could not undo $b; stopping"; exit 2; }
      done
      echo "no patch passed"; exit 1'
```

Rules:

- Give a child you call directly `</dev/null`, and `| cat` if your stdout
  is a terminal. marsh refuses a child that has a terminal.
- A split or fanout branch is one command: `::: LABEL CMD ARG...`.
- Limits: 4 live children per job, 4 levels deep, 64 jobs per tree, 8 jobs
  at once per marsh daemon, and no claude → claude → claude chains. A refused
  call starts nothing, exits 125, and says why on stderr. Do not retry it in
  a loop. Use fewer children or do the work yourself.
- These commands take minutes. Give your shell call a timeout of at least
  10 minutes. If your tool kills the command, marsh cancels its children.
- Passing tests do not make a patch right. Read the applied patch before you
  report success. Treat a child's findings as claims and check them in the
  code.
- Do not edit files while children work on the same files.
- When you finish, say which child changed what, and the test result.
````

Then ask for the outcome, not the mechanics: "make test fails; fix it the way
AGENTS.md describes for competing fixes."

## Limits

marsh refuses a launch that would cross a limit. The refused command starts
nothing, prints the reason, and exits 125. Waiting clears the first two; the
rest need a different plan.

| Limit | Message on stderr |
|---|---|
| 8 jobs at once per daemon, across every tree and session | `capacity: 8 jobs` |
| 4 live children per job, split and fanout branches included | `fan-out limit 4: job … already has 4 live children` |
| Depth 4 (your command is depth 1) | `depth limit 4: … would run at depth 5` |
| Two jobs of one Kit in a row at most | `claude → claude → claude refused: same-Kit chain limit 2` |
| 64 launches per tree | `total limit 64 launches in this tree` |
| 4 distinct Kit VMs per tree | `tree already uses 4 Kit VMs (…)` |
| A command outside `MARSH_SPAWN` (exit 126) | `spawn refused: NAME not in this job's spawn set (MARSH_SPAWN)` |

A child's wall time ends when its parent's does. Inside a job, `marsh` has
`run`, `split`, `join`, `fanout`, `collect`, `jobs`, and `context`, and not
`results`, `status`, or `stop`. All limits are in
[Agents running agents](../agents.md#limits-and-messages).

## Failure modes

- **The agent's tool times out.** Claude Code's Bash tool stops a command
  after 2 minutes unless the agent asks for longer. When the calling process
  dies, marsh cancels the child and everything under it.
- **A direct child edits your files.** One-subtask children and fanout
  branches work on the caller's real files. If the parent and a child edit the
  same file, the last write wins. Put anything that writes in a split.
- **A gate that proves little.** Passing tests prove the tests pass. The
  test-file check above is a filename match, not a guarantee. Read the patch
  before you commit it.
- **Too many children.** A fanout with five agent branches gets the fifth
  refused. Keep it to four per job.
- **Leftover splits.** A job cannot remove a split; if an agent keeps one
  (`--keep`, or a gate that exits nonzero), `marsh splits` in your shell
  lists it and `marsh splits rm ID` removes it.
- **Wasted runs.** Limits stop runaway trees, not pointless ones. An agent
  that delegates a one-line edit spends a model run and a minute on it. The
  snippet's first rule is there for that.
- **Cancelling.** Ctrl-C in your shell stops the whole tree: every job gets
  `INT`, anything still running 10 seconds later gets `KILL`, and marsh checks
  that each container is gone.

## Cost and control

Every child is a full model run, billed to that agent's account. A two-way
race costs two runs plus the caller's own. The first use of each Kit boots
its VM, which takes minutes once and seconds after that.

To cap what an agent may start, set `MARSH_SPAWN` in your shell before you
run it. Nothing inside a job can widen it:

<!-- doc-test: shell replace=["export MARSH_SPAWN=codex\nclaude -p \"fix the failing test; ask codex for a second opinion\"\n", "export MARSH_SPAWN=fixture\nfixture pipeline '|' bash -c 'shell -c true'\n"] status=126 stderr="spawn refused: shell not in this job's spawn set (MARSH_SPAWN)" -->
```sh
export MARSH_SPAWN=codex
claude -p "fix the failing test; ask codex for a second opinion"
```

Here claude may start codex and nothing else. `MARSH_SPAWN=none` stops jobs
from starting anything.

To see what ran, use `marsh jobs --tree` for the shape and `marsh results`
for each job's exit status, wall time, and whether its container was
confirmed deleted.

## Next

- [Race two agents on one bug](race-two-agents.md): the same race from your
  own shell, with the tests in each branch.
- [Agents running agents](../agents.md): every in-job command, variable, and
  limit.
