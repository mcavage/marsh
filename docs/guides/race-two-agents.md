# Make independent changes in parallel

Give one agent the CLI's JSON output and another the API's pagination.
Each works on a project copy and runs the tests there. Inspect both patches,
then test the combined change in your working tree.

Use this recipe in an existing project with a CLI and a paginated API.
Complete the
[Quickstart](../quickstart.md) to configure Anthropic. Both jobs use Claude.
Adjust the prompts to name your CLI command, API handler, and test files.
Choose responsibilities that can change independently; settle any shared
response format before delegating.

## Make sure the tests run in marsh

The examples below use `make test` as a placeholder; replace every occurrence
with your repository's test command. Run it at the marsh prompt first and
install any Linux arm64
dependencies it requires. Split copies omit Git-ignored files, so tests
that depend on ignored artifacts need to recreate them in each copy.

## Run the tasks

At the **marsh prompt**, adapt the scopes and test command, then run:

<!-- doc-test: shell replace=["split {\n  cli: claude -p 'Add a JSON output option to the existing list command. Keep the current text output as the default. Change only CLI code and its tests; do not change the API or start other agents.' && make test\n  api: claude -p 'Add tests for the existing pagination contract: the final page and an invalid cursor. Fix the API handler if those cases fail. Change only API code and its tests; do not change the CLI or start other agents.' && make test\n} | join --keep", "split {\n  cli: fixture project-write notes.txt $'alpha\\nbeta\\ngamma\\n' && grep -q gamma notes.txt\n  api: fixture project-write pagination.txt cursor && grep -qx cursor pagination.txt\n} | join --keep"] stdout="== api (exited 0, shell-vm) ==" -->
<!-- doc-test: shell replace=["split {\n  cli: claude -p 'Add a JSON output option to the existing list command. Keep the current text output as the default. Change only CLI code and its tests; do not change the API or start other agents.' && make test\n  api: claude -p 'Add tests for the existing pagination contract: the final page and an invalid cursor. Fix the API handler if those cases fail. Change only API code and its tests; do not change the CLI or start other agents.' && make test\n} | join --keep", "split {\n  cli: fixture project-write notes.txt $'alpha\\nbeta\\ngamma\\n' && grep -q gamma notes.txt\n  api: fixture project-write pagination.txt cursor && grep -qx missing pagination.txt\n} | join --keep"] status=1 stdout="== api (exited 1, shell-vm) ==" -->
```sh
split {
  cli: claude -p 'Add a JSON output option to the existing list command. Keep the current text output as the default. Change only CLI code and its tests; do not change the API or start other agents.' && make test
  api: claude -p 'Add tests for the existing pagination contract: the final page and an invalid cursor. Fix the API handler if those cases fail. Change only API code and its tests; do not change the CLI or start other agents.' && make test
} | join --keep
```

Both branches start from the same snapshot. `&&` runs a branch's tests only
after its agent exits successfully. The agent container mounts only its
fork of the project; the test command runs as you in the shell VM, starting
in that fork but unconfined.

## Read the result

`join` prints each branch's status, output, and patch. A zero status here
means the agent and that branch's test command both exited successfully.
A failed branch makes `join` exit nonzero without discarding its sibling's
result. `--keep` saves both copies and their patches.

Read the changed-file lists and diffs. Check that the CLI preserves existing
text output, and that the API tests exercise the intended cursor behavior.
The prompts do not enforce file boundaries. If both agents changed shared
code, resolve the overlap before applying their patches together.

## Apply one

Use the split ID from the first join output line. If you want both patches,
apply them one at a time, stopping if either check fails. Replace
`YOUR_SPLIT_ID` and the test command below:

<!-- doc-test: shell replace=["split_id=YOUR_SPLIT_ID\ngit apply --check \".marsh/split/$split_id/out/cli/diff.patch\"\ngit apply \".marsh/split/$split_id/out/cli/diff.patch\"\ngit apply --check \".marsh/split/$split_id/out/api/diff.patch\"\ngit apply \".marsh/split/$split_id/out/api/diff.patch\"\nmake test\ngit diff --check\ngit diff", "set -e\nsplit_id=$(marsh split -n -b cli=\"fixture project-write notes.txt gamma && grep -qx gamma notes.txt\" -b api=\"fixture project-write pagination.txt cursor && grep -qx cursor pagination.txt\")\nsplit_id=$(marsh join --keep -- sh -c 'cat >/dev/null; printf \"%s\\n\" \"$SPLIT_ID\"' <<<\"$split_id\")\ntest \"$(wc -l < notes.txt)\" -eq 2\ntest ! -e pagination.txt\ngit apply --check \".marsh/split/$split_id/out/cli/diff.patch\"\ngit apply \".marsh/split/$split_id/out/cli/diff.patch\"\ngit apply --check \".marsh/split/$split_id/out/api/diff.patch\"\ngit apply \".marsh/split/$split_id/out/api/diff.patch\"\ngrep -qx gamma notes.txt\ngrep -qx cursor pagination.txt\ngit diff --check\nmarsh splits rm \"$split_id\""] -->
```sh
split_id=YOUR_SPLIT_ID
git apply --check ".marsh/split/$split_id/out/cli/diff.patch"
git apply ".marsh/split/$split_id/out/cli/diff.patch"
git apply --check ".marsh/split/$split_id/out/api/diff.patch"
git apply ".marsh/split/$split_id/out/api/diff.patch"
make test
git diff --check
git diff
```

The combined test run matters: each branch passed against its own starting
copy, without the other branch's changes. Read the final diff before
committing. You can also take only one patch and run the tests on that tree.
If your files moved while the agents worked, see
[Merging when your tree has moved](../split.md#merging-when-your-tree-has-moved).

Remove the saved split when you have finished inspecting its results:

<!-- doc-test: shell replace=["marsh splits rm \"$split_id\"", "set -e\nsplit_id=$(marsh split -n ::: cli fixture identity)\nsplit_id=$(marsh join --keep -- sh -c 'cat >/dev/null; printf \"%s\\n\" \"$SPLIT_ID\"' <<<\"$split_id\")\nmarsh splits rm \"$split_id\""] -->
```sh
marsh splits rm "$split_id"
```

## When it goes wrong

Read a failed branch's stderr before rerunning it. A missing dependency can
make a valid edit fail its check. A capacity refusal means the agent did not
start. Ctrl-C stops the foreground split and its children.

## Cost

This starts two billed agent runs and waits for both. Kept copies use disk
space; copy-on-write cloning shares unchanged data where the filesystem
supports it. See [Split and join](../split.md) for copy restrictions and
capture limits.

## Next

[Review split results in a pipeline](add-a-judge.md) creates a fresh split
and sends its report to another agent before you apply anything.
[Review a change with a panel](review-with-a-panel.md) examines one existing
diff from different angles.
