# Review split results in a pipeline

Pipe a split's report to another agent for comments on the proposed changes.
You read those comments and decide what to apply.

Use an existing project with a CLI, a paginated API, and a working Linux
test command, as in [Make independent changes in parallel](race-two-agents.md).
Configure Anthropic before starting. Adapt the prompts to your code and
replace `make test` with the repository's actual check. This example creates
its own split; it does not need a saved result from another guide.

## Send the join report to a reviewer

At the **marsh prompt**:

<!-- doc-test: shell replace=["review='Read both branch reports and patches. Look for conflicting assumptions between CLI JSON output and API pagination, and for missing tests. Give comments I can check in the code. Do not edit files, apply patches, or start other agents.'\nsplit {\n  cli: claude -p 'Add JSON output to the existing list command, preserving text output by default. Change only CLI code and tests.' && make test\n  api: claude -p 'Test the existing pagination contract for the final page and invalid cursors. Fix the API handler if needed. Change only API code and tests.' && make test\n} | join --keep | claude -p \"$review\"\nmarsh splits", "set -e\nset -o pipefail\nsplit {\n  cli: fixture project-write notes.txt gamma && grep -qx gamma notes.txt\n  api: fixture project-write pagination.txt cursor && grep -qx cursor pagination.txt\n} | join --keep | fixture pipeline '|' sh -ec 'input=$(cat); printf \"%s\\n\" \"$input\" | grep -q \"== cli (exited 0\"; printf \"%s\\n\" \"$input\" | grep -q \"== api (exited 0\"; printf \"%s\\n\" \"$input\" | grep -q pagination.txt; printf \"received both results\\n\"'\nmarsh splits"] stdout="received both results" -->
```sh
review='Read both branch reports and patches. Look for conflicting assumptions between CLI JSON output and API pagination, and for missing tests. Give comments I can check in the code. Do not edit files, apply patches, or start other agents.'
split {
  cli: claude -p 'Add JSON output to the existing list command, preserving text output by default. Change only CLI code and tests.' && make test
  api: claude -p 'Test the existing pagination contract for the final page and invalid cursors. Fix the API handler if needed. Change only API code and tests.' && make test
} | join --keep | claude -p "$review"
marsh splits
```

The last agent receives the join report on stdin, including branch statuses
and printed diffs. It runs directly against your original project, with
read-write access despite the request not to edit. The editing agents work
on their private project copies. Use a separate split for the reviewer too
if its possible writes must stay off your original files.

`marsh splits` lists the kept run so you can find its ID without relying on
the reviewer's reply. Inspect the saved patches and test output yourself.
For a large change, `join` may truncate a printed diff; use the full
`out/LABEL/diff.patch` file when checking a comment about omitted code.

## Apply or discard the changes

The reviewer supplies comments, not an approval signal or a machine-readable
winner. Apply the patches you want using the
[parallel guide's application steps](race-two-agents.md#apply-one), then run
your checks on the combined tree. Substitute this run's ID, not one from a
previous session.

When finished, remove this run's saved copies:

<!-- doc-test: shell replace=["marsh splits rm YOUR_SPLIT_ID", "set -e\nsplit_id=$(marsh split -n ::: cli fixture identity)\nsplit_id=$(marsh join --keep -- sh -c 'cat >/dev/null; printf \"%s\\n\" \"$SPLIT_ID\"' <<<\"$split_id\")\nmarsh splits rm \"$split_id\""] -->
```sh
marsh splits rm YOUR_SPLIT_ID
```

## Cost and limits

This starts three billed agent runs. The reviewer's input grows with the
patches and can exceed its context limit. A failed reviewer leaves the kept
split available; the pipeline does not apply patches or remove the copies.

For reviewers examining one existing diff, use
[Review a change with a panel](review-with-a-panel.md).
