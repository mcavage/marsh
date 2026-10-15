# Review a change with a panel

Ask two reviewers different questions about the same uncommitted change.
Each gets a project copy, so an unexpected agent edit stays out of your
working tree.

Use a Git repository with a change visible in `git diff HEAD`, and finish
the [Quickstart](../quickstart.md) first. Both reviewers below use Claude
and the same Anthropic credential. The review covers staged and unstaged
changes to tracked files; it does not include untracked files.

## Run the panel

At the **marsh prompt**:

<!-- doc-test: shell replace=["split {\n  security: claude -p 'Review git diff HEAD for security defects. Do not edit files or start other agents. For each finding, give the file, line, and a concrete failure scenario. Say when you found none.'\n  coverage: claude -p 'Review git diff HEAD for changed behavior that tests do not cover. Do not edit files or start other agents. Point to the behavior and the missing test; distinguish guesses from findings.'\n} | join --keep", "printf 'gamma\\n' >> notes.txt\nsplit {\n  security: fixture identity\n  coverage: fixture identity\n} | join --keep"] stdout="== coverage (exited 0, shell-vm) ==" -->
```sh
split {
  security: claude -p 'Review git diff HEAD for security defects. Do not edit files or start other agents. For each finding, give the file, line, and a concrete failure scenario. Say when you found none.'
  coverage: claude -p 'Review git diff HEAD for changed behavior that tests do not cover. Do not edit files or start other agents. Point to the behavior and the missing test; distinguish guesses from findings.'
} | join --keep
```

Both reviewers start from the same project snapshot, including uncommitted
work. The prompts name the same diff and assign different concerns. If you
want a committed branch reviewed instead, change **both** prompts to the
same base comparison, such as `git diff main...HEAD` when your base is
`main`.

Check each finding against the source and existing tests before making a fix.

If a reviewer edited files, `join` also shows that diff. Inspect it
separately from the findings and leave it unapplied unless you want the
change.

## Run checks without copies

Run the repository's real checks alongside the review or afterward. The
following recipe assumes your project already has working `make lint` and
`make typecheck` targets; substitute its documented commands if not. Verify
they work on Linux arm64 before using them here.

<!-- doc-test: shell replace=["fanout {\n  lint: make lint\n  types: make typecheck\n} | collect", "fanout {\n  lint: grep -q alpha notes.txt\n  types: grep -q beta notes.txt\n} | collect"] stdout="== types (complete) ==" -->
```sh
fanout {
  lint: make lint
  types: make typecheck
} | collect
```

`fanout` uses your real files. Only use this form when the commands can
safely share a directory: checks that rewrite the same cache, fixture, or
generated file may interfere with each other. `collect` prints results in
declaration order and reports a nonzero status if a branch fails. These
checks run as you in the shell VM, without confinement to the project.

## Finish the review

Make or request the intended fix, run the relevant checks,
and review the resulting diff. Remove the saved panel with the split ID
printed by `join`:

<!-- doc-test: shell replace=["marsh splits rm YOUR_SPLIT_ID", "set -e\nsplit_id=$(marsh split -n ::: review fixture identity)\nsplit_id=$(marsh join --keep -- sh -c 'cat >/dev/null; printf \"%s\\n\" \"$SPLIT_ID\"' <<<\"$split_id\")\nmarsh splits rm \"$split_id\""] -->
```sh
marsh splits rm YOUR_SPLIT_ID
```

## Limits and failure modes

This panel starts two billed agent runs. If a reviewer fails, `join`
reports the failure and keeps the copies. Ctrl-C stops the foreground panel
and its child jobs. For copy restrictions and capture limits, see
[Split and join](../split.md).

## Next

[Make independent changes in parallel](race-two-agents.md) uses split for
edits. [Let an agent farm out work](farm-out-work.md) starts a review from
inside another job. See [Split and join](../split.md) for capture limits and
full patch paths.
