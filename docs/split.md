# Split and join

`split` runs parallel commands on private copies of your project, called
*forks*. `join` shows their output and patches. It never applies a patch.

From a marsh shell, ask two agents to work independently:

<!-- doc-test: shell replace=["split {\n  a: claude -p 'append gamma to notes.txt'\n  b: pi -p 'append gamma to notes.txt'\n} | join --keep", "split {\n  a: fixture project-write notes.txt fixed\n  b: fixture project-write notes.txt corrected\n} | join --keep"] stdout="-- a: 1 file (M notes.txt)" -->
```sh
split {
  a: claude -p 'append gamma to notes.txt'
  b: pi -p 'append gamma to notes.txt'
} | join --keep
```

`--keep` leaves the results under `.marsh/split/ID/out/`. Review a branch's
`diff.patch`, apply it with `git apply`, then remove the split with
`marsh splits rm ID`. Keep results whenever you want to read or apply them
after the pipeline ends.

Registered agents mount their own fork of the project. A branch's shell
commands, including tests, run with your shell VM privileges and can reach
your real project ([branch types](#shell-branches-and--branches)).

The shell examples below assume `notes.txt` contains `alpha` and `beta` on
separate lines. Use a scratch project to try them. Agent examples also need
[provider credentials](kits.md#signing-in).

## Long splits from an agent's tool

`split` blocks until every branch finishes. An agent's tool call usually has a
timeout, and when it expires the harness kills `split`, which cancels every
branch and deletes its work (exit 130, `cancelled: its client got SIGTERM`).
For long work, start the split detached and join it in a later call:

```sh
handle=$(marsh split -d ::: a claude -p 'refactor the parser' ::: b codex exec 'refactor the parser')
marsh join --keep <<<"$handle"    # waits for every branch
```

`-d` prints the handle line and exits at once; the branches run on without
it. `marsh join` waits for a running split, and Ctrl-C there stops only the
waiting. Check progress with `marsh splits ID` or `marsh jobs --tree`, and stop
a detached split with `marsh splits cancel ID`. After `join --keep`, each
branch's full output is in `.marsh/split/ID/out/LABEL/stdout`.

## What split touches

- marsh creates `.marsh/split/ID/` for the snapshot, forks, and results. In a
  Git project, it also creates `.marsh/.gitignore` if absent. It leaves the
  original repository's index, objects, and refs untouched.
- Branches run in the shell VM or in a Kit's VM, not on the Mac.
- In Git repositories, ignored files are not copied into forks. A split's root
  cannot be a linked Git worktree or a submodule checkout with a `.git` file;
  use the main checkout.
- `marsh split` works from the marsh shell and from plain bash on the Mac.
  `split { }` works only in the marsh shell. Inside a job, only `:::`
  branches work.
- `join` limits previews to 128 KiB of stdout per branch, 128 KiB of diff
  across branches, and the last 4 KiB of a failed branch's stderr. The whole
  rendering is capped at 192 KiB. Files in `out/` hold the full captured bytes.

The snapshot comes from your current files, including uncommitted edits and
nonignored untracked files. It is not a checkout of `HEAD`. marsh uses
copy-on-write filesystem clones, falling back to ordinary copies on filesystems
without cloning support. Other copy errors fail the split. Forks live and mount
at their own host paths, such as `PROJECT/.marsh/split/ID/a`. In a shell
branch, an absolute path to the original project still reaches your live
files. A registered Kit command mounts the fork, not the original project
root.

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

If your tree has changed since the split, `git apply --3way` can attempt a
three-way merge. It stages the result and can leave conflicts. Commit or stage
your own edits first.

Two details matter:

- `SPLIT_OBJECTS` holds file versions Git may not have. Pass it as
  `GIT_ALTERNATE_OBJECT_DIRECTORIES`.
- Your index must match your working files. If Git reports a stale index after
  changes from the other side of the mount, run `git update-index -q --refresh`.

The example keeps an uncommitted line from the starting snapshot and merges
two later edits to different lines. The objects from the split are needed
because that starting version was never committed to the original repository.

<!-- doc-test: host control="export GIT_ALTERNATE_OBJECT_DIRECTORIES=\"$SPLIT_OBJECTS\"" -->
<!-- doc-test: shell control="export GIT_ALTERNATE_OBJECT_DIRECTORIES=\"$SPLIT_OBJECTS\"" -->
```sh
set -e
printf 'alpha\nbeta\nmiddle\nuncommitted\n' > notes.txt
handle=$(marsh split -n -b fix='sed s/alpha/ALPHA/ notes.txt > next; mv next notes.txt')
printf 'alpha\nbeta\nmiddle\nlocal\n' > notes.txt
git add notes.txt
marsh join -- sh -c 'cat >/dev/null
  export GIT_ALTERNATE_OBJECT_DIRECTORIES="$SPLIT_OBJECTS"
  git apply --3way "$SPLIT_DIR/fix/diff.patch"' <<<"$handle"
grep -qx ALPHA notes.txt
grep -qx local notes.txt
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

- Labels start with a letter or `_`, followed by letters, digits, `_`, or `-`,
  up to 64 characters. They must be unique, ignoring case; `out`, `base`, and
  `owner` are reserved.
- A split has at most 16 branches.
- Commands after `join` read its output on stdin and run in your real
  project directory (in the marsh shell, the shell VM's view of it). They see
  `SPLIT_ID`, `SPLIT_DIR`, and `SPLIT_MANIFEST`, plus `SPLIT_OBJECTS` for Git
  projects. These variables are scoped to the downstream stages.
- Every branch in `split { }` is a shell branch
  ([see below](#shell-branches-and--branches)). For `:::` branches, use the CLI
  form.

### Where one branch ends and the next begins

A new branch starts after `{`, after a newline, or after `,`.

After `;`, a new branch starts only if the next word is a label. A label
matches `[A-Za-z_][A-Za-z0-9_-]*:`.

Everything else is ordinary shell syntax inside the current branch. That includes
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
it failed, and its diff. `-- LABEL: no changes --` means capture found no net
changes relative to the snapshot. It does not describe new files excluded by
ignore rules, shared-home writes, or edits the branch later reverted. A diff
cut at the limit ends with
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
`SPLIT_*` variables set. It removes the split when CMD exits 0, unless
`--keep` was set or cleanup is uncertain. A nonzero CMD status keeps it.

CMD runs where you invoke `marsh join`: on the Mac from a Mac terminal, in
the shell VM from a marsh shell, or in the container from a job.

| Option | Effect |
|---|---|
| `marsh split -n` | Give branches empty stdin |
| `marsh join --json` | Emit the manifest instead of the text rendering |
| `marsh join --keep` | Retain forks and results after joining |
| `marsh join --timing` | Print phase timings on stderr |
| `marsh join -- CMD ARG...` | Run a command with the rendering on stdin and `SPLIT_*` set |

## Shell branches and `:::` branches

A *shell branch*, written as `-b LABEL=STRING` or inside `split { }`, runs
your code in a fresh Brush shell in the shell VM. It starts in its fork,
even when your interactive shell is Bash or zsh.

Its ordinary commands (`make`, `sed`, a script) are not confined: they can
write anywhere your shell user can, including your live project. Capture
records only changes in the fork. Use shell branches for tests and scripts
you trust. Registered commands they start still run in Kit containers that
mount the fork.

An *argv branch*, written as `::: LABEL CMD ARG...`, runs one registered
command in a fresh container. Its project mount is the fork, along with the
Git metadata the fork needs:

- The index, objects, and refs are writable, with `HEAD` naming a private
  `marsh-split` branch. The command can stage and commit without changing
  your original repository.
- The original object store and the fork's fixed configuration files are
  read-only mounts.

Every Kit job also mounts the shared guest home read/write. Login files and
session state saved there, such as `~/.claude` and `~/.codex`, remain visible
to other jobs in that home.

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

In a Git repository, marsh creates `.marsh/.gitignore` containing `*` when
that file is absent, so Git ignores new split state. An existing ignore file
is left unchanged.

### What is captured

In a Git repository, new files that the project's ignore rules exclude are not
captured, as with `git status`. The rules are `.gitignore`,
`.git/info/exclude`, and `core.excludesFile`. `__pycache__/` from running
Python is an example.

A file included in the snapshot is always compared, even if a branch adds an
ignore rule that matches it. Ignore filtering applies only to Git projects;
a plain directory's `.gitignore` does not exclude files.

Capture allows 32 MiB per changed file and 256 MiB of changed bytes per branch,
with a five-second deadline per text diff. Exceeding a capture limit rejects
that branch's result. `.marsh`, nested `.git` entries, and special files are
excluded from the initial snapshot; creating a nested `.git` in a fork causes
capture to reject it.

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

`marsh split` returns the first nonzero branch status in declaration order.
Bare `marsh join` returns that status too, keeping failed results. With
`marsh join -- CMD`, CMD runs even if branches failed, and its status becomes
join's status. A successful CMD removes the results unless `--keep` was set
or cleanup is uncertain.

In Brush's `split { } | join | CMD` form, `PIPESTATUS` records the split stage,
join stage, and downstream stages. The last stage determines the pipeline's
status unless `pipefail` is set. Retention still follows the last stage's
status, even with `pipefail`: a successful CMD can remove a failed branch's
results. Use `join --keep` if you need them afterward.

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
creator can still join it. A kept split is removed 7 days after it was kept;
`marsh storage` shows what kept splits and checkpoints take up.

`--timing` reports these phases:

- `run`: the slowest branch, including Kit VM boots.
- `consumer`: the commands after `join`.
- `snapshot`, `forks`, `capture`: marsh's own overhead.

Splits allow 16 branches, 64 MiB of input, and 16 MiB of combined branch
output. A split's time limit is the job wall limit (`MARSH_JOB_WALL_SECONDS`,
24 hours by default), and splits can nest three deep. Nested
splits snapshot the calling branch's fork and accept only argv branches.
Branches share a 16-slot session pool, while registered commands also
count against the daemon's [job limits](agents.md#limits-and-messages).
Only the creator may join a split, including one retained after lease expiry.

## Checkpoints and open lanes

A checkpoint freezes the project as a split would snapshot it, so a split can
start from it later:

```sh
c=$(marsh checkpoint --label before-refactor)   # prints its ID
marsh checkpoints                               # newest first
```

An open split makes forks ("lanes") with no command and returns at once. You
then run agents in them yourself, for as long as you like, and close the split
to collect each lane's patch:

```sh
marsh split --open --from "$c" a b     # {"marsh_split":1,"id":ID,"lanes":{"a":FORK,"b":FORK}}
(cd FORK_A && marsh run claude -p 'fix the flaky test')
(cd FORK_B && marsh run --acp codex)   # an ACP client drives the agent over stdio
marsh split close ID                   # captures every lane; the split is kept
marsh split apply ID a --3way          # applies lane a's patch to the project
```

A command run in a lane is confined to its fork, like a `:::` branch. Only the
split's creator can start one, and only until the split is closed. `close`
waits up to 30 seconds for running lane jobs; a lane whose jobs are still
running is kept as uncertain. `marsh splits cancel ID` stops the lane jobs and
removes the split. See [design: workspaces](design/workspaces.md#14-open-lanes-checkpoints-and-storage).

## Seeing a split in `marsh jobs --tree`

In your session, `marsh jobs --tree` draws each split with its branches and
the jobs they started, even after `join` removed the files. A job started in
a downstream stage says `(consumes split ID)`.

A shell branch's row (`SPLIT/label`) times the whole branch. The job rows under it time
each Kit job from its start. The gap between them is shell startup plus any
Kit VM boot.

Next: [Fanout and collect](fanout.md), [Agents running agents](agents.md),
[design: workspaces](design/workspaces.md).
