# Let a judge pick the winner

When both branches of a [race](race-two-agents.md) pass your tests, your tests
cannot choose between them. Pipe `join` into a third agent and have it choose:

```console
marsh-0.5$ split {
>   claude: claude -p "make test fails. Fix slug.py without editing the tests. Reply in one line." && make test
>   codex:  codex exec "make test fails. Fix slug.py without editing the tests. Reply in one line." && make test
> } | join --keep | claude -p "Two attempts at one fix follow, each with its status, output, and patch. Pick one to apply. First line: claude or codex, nothing else. Then at most three lines of reasons. Reject a patch that edits tests."
claude

- Both patches are byte-identical (same blob hash `fe5e599c`), so the choice is arbitrary; neither edits tests.
- Both touch only `slug.py`, pass all 3 tests, and correctly collapse separator runs plus strip edge hyphens.
- Picked claude by label order only — applying codex would give the identical working tree.
split: kept …/.marsh/split/1106d7ad153e (join --keep); inspect, then: marsh splits rm 1106d7ad153e
```

The judge read both patches and said they were the same. It took 3 seconds
and one more model run.

Use a judge when both patches pass and differ in size, scope, or risk. It does
not help when neither is right: it picks among candidates and does not repair
them.

## Add the judge

<!-- doc-test: shell replace=["split {\n  claude: claude -p \"Make the retry logic idempotent.\" && make test\n  codex:  codex exec \"Make the retry logic idempotent.\" && make test\n} | join --keep | claude -p \"Two attempts at one change follow, each with its status, output, and patch. First line: claude, codex, or neither, nothing else. Then your reasons. Reject a branch that did not exit 0 or that edits tests. Prefer the smaller change.\"\n", "out=$(split {\n  claude: fixture project-write notes.txt $'alpha\\nbeta\\ngamma\\n' && grep -q gamma notes.txt\n  codex:  fixture project-write notes.txt $'alpha\\nbeta\\ndelta\\n' && grep -q alpha notes.txt\n} | join --keep | sh -c 'cat >judge.in; echo claude; echo smaller change')\ntest \"$(head -1 <<<\"$out\")\" = claude\ngrep -q '^== claude (exited 0' judge.in\ngrep -q '^== codex (exited 0' judge.in\nid=$(head -1 judge.in | cut -d' ' -f3 | tr -d :)\nmarsh splits rm \"$id\"\nrm judge.in\n"] -->
```sh
split {
  claude: claude -p "Make the retry logic idempotent." && make test
  codex:  codex exec "Make the retry logic idempotent." && make test
} | join --keep | claude -p "Two attempts at one change follow, each with its status, output, and patch. First line: claude, codex, or neither, nothing else. Then your reasons. Reject a branch that did not exit 0 or that edits tests. Prefer the smaller change."
```

The judge gets `join`'s whole output on stdin: every header, every branch's
output, and every diff, including a branch that failed. `claude -p` and
`codex exec` read piped stdin along with the prompt.

Write the judge's prompt as a rubric. Say what to prefer (the smaller change,
no new dependencies, no test edits) and fix the answer's format, so you can
read it at a glance or parse it.

`--keep` matters. Without it, `join` deletes the patches when the pipeline
succeeds, before you have read the verdict. With it, apply the chosen patch
and clean up:

```sh
git apply .marsh/split/1106d7ad153e/out/claude/diff.patch
marsh splits rm 1106d7ad153e
```

## Let the judge apply it

To apply the verdict in the same command, parse its first line. In the
`split { }` form, every command after `join` runs in your project and sees
`$SPLIT_DIR` and `$SPLIT_ID`:

<!-- doc-test: shell replace=["split {\n  claude: claude -p \"Make the retry logic idempotent.\" && make test\n  codex:  codex exec \"Make the retry logic idempotent.\" && make test\n} | join --keep | sh -c '\n  verdict=$(claude -p \"Choose the patch to apply. First line: exactly claude, codex, or neither. Then your reasons.\")\n  printf \"%s\\n\" \"$verdict\" >&2\n  pick=$(printf \"%s\\n\" \"$verdict\" | head -1)\n  case $pick in claude|codex) ;; *) echo \"no pick; split kept\" >&2; exit 1 ;; esac\n  grep -qx \"exited 0\" \"$SPLIT_DIR/$pick/status\" || exit 1\n  git apply \"$SPLIT_DIR/$pick/diff.patch\" && make test && marsh splits rm \"$SPLIT_ID\"'\n", "split {\n  claude: fixture project-write notes.txt $'alpha\\nbeta\\ngamma\\n' && grep -q gamma notes.txt\n  codex:  fixture project-write notes.txt $'alpha\\nbeta\\ndelta\\n' && grep -q alpha notes.txt\n} | join --keep | sh -c '\n  verdict=$(sh -c \"cat >/dev/null; echo claude; echo smaller change\")\n  printf \"%s\\n\" \"$verdict\" >&2\n  pick=$(printf \"%s\\n\" \"$verdict\" | head -1)\n  case $pick in claude|codex) ;; *) echo \"no pick; split kept\" >&2; exit 1 ;; esac\n  grep -qx \"exited 0\" \"$SPLIT_DIR/$pick/status\" || exit 1\n  git apply \"$SPLIT_DIR/$pick/diff.patch\" && grep -q gamma notes.txt && marsh splits rm \"$SPLIT_ID\"'\ngrep -q gamma notes.txt\n"] -->
```sh
split {
  claude: claude -p "Make the retry logic idempotent." && make test
  codex:  codex exec "Make the retry logic idempotent." && make test
} | join --keep | sh -c '
  verdict=$(claude -p "Choose the patch to apply. First line: exactly claude, codex, or neither. Then your reasons.")
  printf "%s\n" "$verdict" >&2
  pick=$(printf "%s\n" "$verdict" | head -1)
  case $pick in claude|codex) ;; *) echo "no pick; split kept" >&2; exit 1 ;; esac
  grep -qx "exited 0" "$SPLIT_DIR/$pick/status" || exit 1
  git apply "$SPLIT_DIR/$pick/diff.patch" && make test && marsh splits rm "$SPLIT_ID"'
```

Each step can stop it:

- A first line other than `claude` or `codex` (including `neither`) exits 1.
  The split stays on disk for you to read.
- A pick whose branch did not pass (`status` is not `exited 0`) exits 1.
- `git apply` fails if your tree moved.
- `make test` runs again on your real tree after the patch applies. If it
  fails, the patch stays applied and the split stays on disk; undo it with
  `git apply -R` on the same file.

The split is removed only when all of that passes. The judge's reasons go to
stderr, so you see them either way. This checks the label and the branch
status, not the judge's reasoning.

## Limits and failure modes

- **The judge can be wrong.** It can miss a defect or prefer the worse
  patch. Its reasons are claims to check, and you still read the patch you
  merge.
- **Self-preference.** A model may favor a patch in its own style. Use a
  different agent for the judge than for the branches when you can.
- **Large diffs are cut.** `join` prints at most 128 KiB of diff, and a cut
  diff ends with `(diff truncated; full diff: PATH)`. On a big change, tell
  the judge to read the full patches in `.marsh/split/ID/out/LABEL/diff.patch`.
- **Cost.** One more model run. Its input is the whole `join` output, so it
  grows with the patches.

## Next

[Review a change with a panel](review-with-a-panel.md) to put several agents
on a change that already exists.
