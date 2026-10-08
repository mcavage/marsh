# Split and join

`split` runs commands in parallel, each on its own private copy of your
project (a *fork*). `join` prints what each one said and a patch of what it
changed. Your files and your `.git` stay as they were until you apply a patch.

Two agents try to fix the same failing test:

```console
marsh-0.5$ split {
>   a: claude -p "fix TestDialTimeout"
>   b: pi -p "fix TestDialTimeout"
> } | join
# split 8d5c8fdd1563: 2 branches; manifest …/out/manifest.json

== a (exited 0, shell-vm) ==
The test raced on a fixed port; it now asks the kernel for a free one.
-- a: 1 file (M net/dial_test.go); diff …/out/a/diff.patch --
diff --git a/net/dial_test.go b/net/dial_test.go
…
== b (exited 0, shell-vm) ==
Added a retry around Dial and a longer timeout.
-- b: 2 files (M net/dial.go, M net/dial_test.go); diff …/out/b/diff.patch --
diff --git a/net/dial.go b/net/dial.go
…
marsh-0.5$ git apply .marsh/split/8d5c8fdd1563/out/a/diff.patch
```

Both agents ran at once, each on its own fork. You read both answers and
applied the patch you wanted. Ctrl-C during the split stops every branch and
everything it started. `…` marks trimmed output.

`split` does not review patches. In the example, `claude` and `pi` are
registered commands, so each runs in a container that mounts only its fork.
The branch's own shell code is different: plain commands run as you in the shell
VM and can write anywhere you can, including your real project directory
([below](#shell-branches-and--branches)).

Agents authenticate through the Docker Sandboxes proxy. A container sees
placeholder values, never your keys ([Kits](kits.md)).

The examples below run in a Git repository that holds a file `notes.txt`. The
test suite runs each of them.

## What split touches

- It writes no files in your project and nothing in your `.git`, except the
  `.marsh/split/ID` directory that holds the results.
- Branches run in the shell VM or in a Kit's VM, not on the Mac.
- `marsh split` works from the marsh shell and from plain bash on the Mac.
  `split { }` works only in the marsh shell. Inside a job, only `:::`
  branches work.
- `join` shows at most 128 KiB of diff and the last 4 KiB of a failed branch's
  stderr. `diff.patch` and `stderr` in `out/` hold the full text.

## Applying a result

`git apply` needs no index. It works from the marsh shell and from a Mac
terminal:

<!-- doc-test: host -->
<!-- doc-test: shell -->
```sh
split_id=$(marsh split -n -b fix='printf "alpha\nbeta\ngamma\n" > notes.txt')
marsh join -- sh -c 'cat >/dev/null; git apply "$SPLIT_DIR/fix/diff.patch"' <<<"$split_id"
grep -q gamma notes.txt
```

### Merging when your tree has moved

`git apply --3way` merges when your tree has changed since the split, and it
stages the result. Commit or stage your own edits first.

Two details matter:

- `SPLIT_OBJECTS` holds file versions Git may not have. Pass it as
  `GIT_ALTERNATE_OBJECT_DIRECTORIES`.
- Git compares the index with your files by stat data, and the shell VM and
  macOS report different stat data for the same files. If the other side wrote
  the files last, run `git update-index -q --refresh` first.

<!-- doc-test: host prep=other control="git update-index -q --refresh" -->
<!-- doc-test: shell prep=other control="git update-index -q --refresh" -->
```sh
split_id=$(marsh split -n -b fix='printf "alpha\nbeta\ngamma\n" > notes.txt')
marsh join -- sh -c 'cat >/dev/null; git update-index -q --refresh
  GIT_ALTERNATE_OBJECT_DIRECTORIES="$SPLIT_OBJECTS" git apply --3way "$SPLIT_DIR/fix/diff.patch"' <<<"$split_id"
git diff --cached --name-only | grep -qx notes.txt
```

## Inside marsh (Brush form)

<!-- doc-test: shell stdout="== count (exited 0, shell-vm) ==" -->
```sh
split {
  upper: tr a-z A-Z < notes.txt > n && mv n notes.txt
  count: wc -l < notes.txt
} | join | less -FX
```

Each `label: command` line is one branch. Without a label, a branch is named
after its command.

- Labels must be unique, ignoring case.
- A split has at most 16 branches.
- Commands after `join` read its output on stdin and run in your real
  project directory (in the marsh shell, the shell VM's view of it). They see
  `SPLIT_ID`, `SPLIT_DIR`, `SPLIT_MANIFEST`, and `SPLIT_OBJECTS`.
- Every branch in `split { }` is a shell branch
  ([see below](#shell-branches-and--branches)). For `:::` branches, use the CLI
  form.

### Where one branch ends and the next begins

A new branch starts after `{`, after a newline, or after `,`.

After `;`, a new branch starts only if the next word is a label. A label
matches `[A-Za-z_][A-Za-z0-9_-]*:`.

Everything else is ordinary Bash inside the current branch. That includes
other `;`, `&&`, `||`, `{ ...; }`, `( ... )`, and `case ... ;;`.

| Input | Result |
|---|---|
| `a: cd x; make; b: cd y; make` | two branches, two commands each |
| `x; y` | one branch |
| `a: echo b: c` | one branch (only the first word after `;` counts) |
| `x; echo: hi` | two branches, the second named `echo` |

A command whose name ends in `:` is read as a label. Quote it to run it as a
command: `'echo:' hi`.

<!-- doc-test: shell stdout="-- upper: 1 file (M notes.txt)" -->
```sh
split { upper: tr a-z A-Z < notes.txt > n; mv n notes.txt; count: { wc -l; echo done; } < notes.txt } | join
```

### What join prints

For each branch, `join` prints a header with the status and where it ran
(`shell-vm`, or `kit:NAME` for a `:::` branch), then its stdout, its stderr if
it failed, and its diff. A branch that changed nothing prints
`-- LABEL: no changes --`. A diff cut at the limit ends with
`(diff truncated; full diff: PATH)`.

A binary change appears as
`binary file changed: PATH (N bytes; full patch in $SPLIT_DIR/LABEL/diff.patch)`.
`diff.patch` has the full bytes.

### A fanout inside a branch

A branch can run a [`fanout`](fanout.md) on its own fork. `join` shows the
branch's `collect` output, and `marsh jobs --tree` draws a `fanout (...)`
node under the branch:

<!-- doc-test: shell replace=["split {\n  fix: claude -p \"fix TestDialTimeout\"\n  review: fanout { races: codex exec \"review net/ for races\", leaks: claude -p \"look for goroutine leaks\" } | collect\n} | join\n", "split {\n  fix: fixture project-write notes.txt fixed\n  review: fanout { races: fixture identity, leaks: fixture identity } | collect\n} | join\nmarsh jobs --tree\n"] stdout="└─ fanout (" -->
```sh
split {
  fix: claude -p "fix TestDialTimeout"
  review: fanout { races: codex exec "review net/ for races", leaks: claude -p "look for goroutine leaks" } | collect
} | join
```

## From plain bash (CLI form)

<!-- doc-test: host stdout="== count (exited 0, shell-vm) ==" -->
```sh
marsh split -b upper='tr a-z A-Z < notes.txt > n && mv n notes.txt' \
            -b count='wc -l < notes.txt' </dev/null | marsh join
```

`marsh split` prints one line of JSON, the handle (it contains `"id"`). It
exits with the first nonzero branch status, in the order the branches were
written. `marsh join` reads the handle.

Input to the branches:

- `marsh split` reads stdin once and gives every branch the same bytes.
- With `-n`, or when stdin is a terminal, branches get empty input.

With `-- CMD`, `marsh join` runs CMD with its output on stdin and the
`SPLIT_*` variables set. The split is removed when CMD exits 0 and kept
otherwise.

## Shell branches and `:::` branches

There are two kinds of branch.

A *shell branch* is your own code:

- It is `-b LABEL=STRING`, or any branch in `split { }`.
- It runs in the shell VM as you and starts in its fork.
- Its own commands (`make`, `sed`, a script) are not confined. They can write
  anywhere you can, including your real project directory. Only the fork's
  changes are captured.
- A registered command it starts (`claude`, `codex`, `pi`) runs in a Kit
  container whose working directory is the fork, so that container mounts only
  the fork.
- Use it for `make test`, linters, scripts you trust, and agent calls.

An *argv branch* is one registered command in a fresh container:

- It is `::: LABEL CMD ARG...`.
- Of the project, it mounts only its fork and the fork's Git metadata.
- Like every Kit job, it also mounts your marsh home read/write. Agents keep
  logins and session state there (`~/.claude`, `~/.codex`). All jobs can read
  that home.
- Use it for agents. Within the project, they can write only their fork.

<!-- doc-test: host replace=["codex exec 'fix the typo in notes.txt'", "fixture project-write notes.txt fixed"] stdout="-- review: 1 file (M notes.txt)" -->
```sh
marsh split -n -b tests='grep -c . notes.txt' \
            ::: review codex exec 'fix the typo in notes.txt' | marsh join
```

`-b` takes one word, `LABEL=STRING`. `:::` starts an argv branch, and the next
`:::` ends it. Words pass through unchanged, so an argument cannot be `:::`.

## Results

```
$SPLIT_DIR = <project>/.marsh/split/<id>/out
  manifest.json            ($SPLIT_MANIFEST) each branch's state, status, and where it ran
  <label>/stdout, stderr   the branch's output bytes
  <label>/status           "exited 0", "failed: ...", "rejected: ...", "cancelled"
  <label>/files            one "A|M|D|T<TAB>path" line per changed path
  <label>/diff.patch       a git-apply patch (absent when nothing changed)
```

In a Git repository, `.marsh/` has its own `.gitignore`, so `git status` does
not show it.

### What is captured

In a Git repository, new files that the project's ignore rules exclude are not
captured, as with `git status`. The rules are `.gitignore`,
`.git/info/exclude`, and `core.excludesFile`. `__pycache__/` from running
Python is an example.

A file that existed when the split started is always captured, even if a rule
matches it.

## Directories that are not Git repositories

A plain directory splits the same way. The patches still apply with
`git apply`, which needs no repository. marsh writes no Git files there.

<!-- doc-test: host dir=plain -->
```sh
marsh split -n -b fix='printf "alpha\nbeta\ngamma\n" > notes.txt' |
  marsh join -- sh -c 'cat >/dev/null; git apply "$SPLIT_DIR/fix/diff.patch"'
grep -q gamma notes.txt && test ! -e .git && test ! -e .marsh/.gitignore
```

## Failures, pipefail, Ctrl-C

`join -- CMD` runs CMD even when branches failed. Their statuses are in join's
output.

The exit status of the pipeline depends on the form:

- In the `split { }` form, the last stage's status decides.
- With `pipefail`, the pipeline returns CMD's status if it is nonzero,
  otherwise the split's.

If CMD fails, marsh keeps the split and says why:

<!-- doc-test: host status=1 stderr="(join command exited 1); inspect, then: marsh splits rm " -->
```sh
marsh split -n -b bad='echo oops >&2; exit 4' | marsh join -- sh -c 'cat; exit 1'
```

### Ctrl-C

Ctrl-C during `split` (or a `split { }` pipeline) cancels every branch and
anything they started. Each gets SIGINT, then SIGKILL after 10 seconds.
Nothing after `join` runs.

When every branch is confirmed stopped, marsh removes the split directory.

If a branch cannot be confirmed, for example because its VM was lost, marsh
keeps the split and the message names the branches. Run
`marsh workers reset all` before `marsh splits rm`.

## Inspecting and removing kept splits

<!-- doc-test: host stdout="  bad  shell-vm  exited 4" stderr="(join --keep); inspect, then: marsh splits rm " -->
```sh
h=$(marsh split -n -b bad='exit 4')
marsh join --keep -- true <<<"$h"   # prints why it was kept and how to remove it
marsh splits                        # id, state, dir; one line per branch
id=$(sed 's/.*"id":"\([^"]*\)".*/\1/' <<<"$h")
marsh splits rm "$id"
```

Other commands:

| Command | Effect |
|---|---|
| `marsh splits ID` | describes a split, including one `join` already removed (states and times, no files) |
| `marsh splits cancel ID` | cancels a running split |
| `marsh status` | counts active, awaiting, and kept splits |
| `marsh join --timing` | prints a timing breakdown on stderr |

A split waiting for `join` that no process has held open for 60 seconds is kept. Its
creator can still join it.

Example of `--timing` output:

```text
split 8d5c8fdd1563 timing: snapshot 0ms; forks 2ms; run 727ms; capture fix 0ms, review 0ms; consumer 278ms
```

- `run`: the slowest branch, including Kit VM boots.
- `consumer`: the commands after `join`.
- `snapshot`, `forks`, `capture`: marsh's own overhead.

## Seeing a split in `marsh jobs --tree`

In your session, `marsh jobs --tree` draws each split you ran, with its
branches and every job they started. It does this even after `join` removed
the split. A job started after `join` says which split it read:

```text
ID        STARTED   STATE      EXIT      RUN  COMMAND
524499e5  0s ago    finished      0     0.2s  claude -p "apply the good ones…"  (consumes split 8d5c8fdd)
8d5c8fdd  1m ago    joined        0    1m01s  split (fix, review)
-         1m ago    finished      0    1m01s  ├─ fix: claude -p "find and fix one small bug…"
9f47f6af  1m ago    finished      0    58.2s  │  └─ claude -p "find and fix one small bug…"
b982a277  1m ago    finished      0    20.1s  │     └─ codex exec "…"
-         1m ago    finished      0    48.4s  └─ review: codex exec "suggest one small improvement…"
149f157c  1m ago    finished      0    47.9s     └─ codex exec "suggest one small improvement…"
```

A shell branch's row (`-`) times the whole branch. The job rows under it time
each Kit job from its start. The gap between them is shell startup plus any
Kit VM boot.

Next: [Fanout and collect](fanout.md), [Agents running agents](agents.md),
[design: workspaces](design/workspaces.md).
