# Let a judge pick the winner

When two attempts both pass your tests, you still have to choose. A judge is
one more command after `join`: it reads every branch's result on its stdin, and
you read its verdict before you apply anything. Start from
[Race two agents on one bug](race-two-agents.md).

## Add the judge

<!-- doc-test: shell replace=["split {\n  claude: claude -p \"Make the retry logic idempotent.\" && make test\n  codex:  codex exec \"Make the retry logic idempotent.\" && make test\n} | join --keep | claude -p \"Two agents made the same change; each one's output and patch follow. Choose the one to apply. First line: its label, claude or codex. Then your reasons. Prefer the smaller change, and reject a patch that edits tests.\"\n", "out=$(split {\n  claude: fixture project-write notes.txt $'alpha\\nbeta\\ngamma\\n' && grep -q gamma notes.txt\n  codex:  fixture project-write notes.txt $'alpha\\nbeta\\ndelta\\n' && grep -q alpha notes.txt\n} | join --keep | sh -c 'cat >judge.in; echo claude; echo smaller change')\ntest \"$(head -1 <<<\"$out\")\" = claude\ngrep -q '^== claude (exited 0' judge.in\ngrep -q '^== codex (exited 0' judge.in\nid=$(head -1 judge.in | cut -d' ' -f3 | tr -d :)\nmarsh splits rm \"$id\"\nrm judge.in\n"] -->
```sh
split {
  claude: claude -p "Make the retry logic idempotent." && make test
  codex:  codex exec "Make the retry logic idempotent." && make test
} | join --keep | claude -p "Two agents made the same change; each one's output and patch follow. Choose the one to apply. First line: its label, claude or codex. Then your reasons. Prefer the smaller change, and reject a patch that edits tests."
```

Commands after `join` read its output on stdin: every status, every output,
every patch. The judge sees the whole result, including a branch that failed,
so it can say that only one of them passed. `claude -p` reads that stdin as
part of its prompt. `--keep` leaves the results on disk, so you can still apply
a patch after reading what the judge says.

The judge's reply is the last thing printed. You apply the patch it names, the
same way as before:

```sh
git apply .marsh/split/8d5c8fdd1563/out/claude/diff.patch
marsh splits rm 8d5c8fdd1563
```

A judge is another agent and can be wrong. Its value is that it reads both
patches closely and gives you reasons to check, not that you can skip reading.
Pick a different agent for the judge than for the branches if you can; a model
may favor its own style.

## Let the judge apply it

Once you trust the verdict, you can apply it in the same command. The judge
must answer with a branch label on the first line, so the script can use it:

<!-- doc-test: shell replace=["split {\n  claude: claude -p \"Make the retry logic idempotent.\" && make test\n  codex:  codex exec \"Make the retry logic idempotent.\" && make test\n} | join --keep | sh -c 'pick=$(claude -p \"Choose the patch to apply. Reply with exactly one word on the first line, claude or codex, then your reasons.\" | tee /dev/stderr | head -1)\n  git apply \"$SPLIT_DIR/$pick/diff.patch\" && marsh splits rm \"$SPLIT_ID\"'\n", "split {\n  claude: fixture project-write notes.txt $'alpha\\nbeta\\ngamma\\n' && grep -q gamma notes.txt\n  codex:  fixture project-write notes.txt $'alpha\\nbeta\\ndelta\\n' && grep -q alpha notes.txt\n} | join --keep | sh -c 'pick=$(sh -c \"cat >/dev/null; echo claude; echo smaller change\" | tee /dev/stderr | head -1)\n  git apply \"$SPLIT_DIR/$pick/diff.patch\" && marsh splits rm \"$SPLIT_ID\"'\ngrep -q gamma notes.txt\n"] -->
```sh
split {
  claude: claude -p "Make the retry logic idempotent." && make test
  codex:  codex exec "Make the retry logic idempotent." && make test
} | join --keep | sh -c 'pick=$(claude -p "Choose the patch to apply. Reply with exactly one word on the first line, claude or codex, then your reasons." | tee /dev/stderr | head -1)
  git apply "$SPLIT_DIR/$pick/diff.patch" && marsh splits rm "$SPLIT_ID"'
```

Commands after `join` run in your project and see `$SPLIT_DIR` and `$SPLIT_ID`.
This is safe to try because it fails closed. If the judge answers with anything
that is not a label, there is no `$SPLIT_DIR/$pick/diff.patch`, `git apply`
fails, and `--keep` has left the results on disk for you to read. Only when the
patch applies does the command remove them, so your tree has exactly one patch
applied. `tee /dev/stderr` shows you the judge's reasons as it works.

## Where a judge helps, and where it doesn't

- It helps when both patches pass and differ in scope, style, or risk, which is
  the case your tests cannot decide.
- It does not help when neither patch is right. A judge picks among candidates;
  it does not repair them. If both are wrong, change the prompt or the tests and
  race again.

## Next

To use agents on a change that already exists, see
[Review a change with a panel](review-with-a-panel.md).
