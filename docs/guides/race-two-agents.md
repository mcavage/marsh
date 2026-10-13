# Race two agents on one bug

Two agents fix the same failing test at the same time, each on a private copy
of your project, and each copy runs your tests. marsh waits for both, then
shows both patches and both test results. The agents never touch your working
tree; you apply the patch you choose.

```console
marsh-0.5$ split {
>   claude: claude -p "make test fails. Fix slug.py without editing the tests. Reply in one line." && make test
>   codex:  codex exec "make test fails. Fix slug.py without editing the tests. Reply in one line." && make test
> } | join --keep
# split 3c8145fd9046: 2 branches; manifest …/.marsh/split/3c8145fd9046/out/manifest.json

== claude (exited 0, shell-vm) ==
Fixed `slug.py:6` to collapse runs of non-alphanumerics into one hyphen and strip leading/trailing hyphens — all 3 tests pass (no children spawned; the edit was a one-liner).
python3 -m unittest -q
-- claude: 1 file (M slug.py); diff …/out/claude/diff.patch --
diff --git a/slug.py b/slug.py
…
-    return re.sub(r"[^a-z0-9]", "-", title.lower())
+    return re.sub(r"[^a-z0-9]+", "-", title.lower()).strip("-")

== codex (exited 0, shell-vm) ==
Fixed `slug.py` to collapse separators and trim hyphens; all 3 tests pass, tests unchanged, no child agents used.
python3 -m unittest -q
-- codex: 1 file (M slug.py); diff …/out/codex/diff.patch --
diff --git a/slug.py b/slug.py
…
split: kept …/.marsh/split/3c8145fd9046 (join --keep); inspect, then: marsh splits rm 3c8145fd9046
```

That run took 12 seconds on a three-test Python project with warm VMs (not a
benchmark). Both agents found the same one-line fix. When the patches differ,
compare their scope and their test output before you apply one. `…` marks
output trimmed here.

Use a race when a fix is worth a second attempt and you have a failure you
can reproduce: a regression, or a failing CI check that also fails locally.
For a flaky test, gate on a repeated run (`go test -count=20 -run TestX`);
one passing run proves nothing. Skip races for changes your tests cannot
judge.

You need a test command that exits nonzero on failure, and a credential for
each agent ([Agent credentials](../quickstart.md#agent-credentials)).

## Make sure the tests run in marsh

The `&& make test` part of each branch runs in marsh's shell VM, not on your
Mac. The VM is Debian on arm64 with git, make, gcc, Python 3, Node.js, and
Rust. Run your test command at the marsh prompt once. If it needs something
else, install it there; packages stay until the VM is removed:

```sh
sudo apt-get install -y python3-pytest
```

If the tests fail in the VM for reasons unrelated to the bug, every branch
fails and the race tells you nothing.

## Run the race

<!-- doc-test: shell replace=["split {\n  claude: claude -p \"make test fails. Fix the code, not the tests.\" && make test\n  codex:  codex exec \"make test fails. Fix the code, not the tests.\" && make test\n} | join --keep\n", "out=$(split {\n  claude: fixture project-write notes.txt $'alpha\\nbeta\\ngamma\\n' && grep -q gamma notes.txt\n  codex:  fixture project-write notes.txt $'alpha\\nbeta\\n' && grep -q gamma notes.txt\n} | join --keep)\nprintf '%s\\n' \"$out\" | grep -q '^== claude (exited 0'\nprintf '%s\\n' \"$out\" | grep -q '^== codex (exited 1'\nid=$(printf '%s\\n' \"$out\" | head -1 | cut -d' ' -f3 | tr -d :)\ngit apply \".marsh/split/$id/out/claude/diff.patch\"\ngrep -q gamma notes.txt\nmarsh splits rm \"$id\"\n"] -->
```sh
split {
  claude: claude -p "make test fails. Fix the code, not the tests." && make test
  codex:  codex exec "make test fails. Fix the code, not the tests." && make test
} | join --keep
```

Each line inside the braces is a branch: a label, a colon, and ordinary Bash.
marsh gives each branch its own copy of your working tree, uncommitted changes
included, and runs the branches at once. `agent && make test` runs the tests
on that copy only if the agent exits 0.

The agent runs in a fresh container that mounts only its branch's copy.
`make test` runs in the shell VM as you, starting in the branch's copy. It is
your own command, so marsh does not confine it: a test that writes to a fixed
path outside the project writes the real one.

## Read the result

For each branch, `join` prints:

- A header with the exit status. `exited 0` means the agent finished and the
  tests passed on that copy. Anything else means one of them failed; the
  branch's stderr follows.
- What the agent and the tests printed.
- A `--` line listing the files changed, then the diff.

Look at the files list before the diff. A patch that lists a test file
(`M test_slug.py`) may have passed by changing the test. The prompt says not
to; check anyway.

`join` exits with the first nonzero branch status. With `--keep`, the copies
and patches stay on disk either way, so you can apply one.

## Apply one

The split id is on the first line. Apply the patch you chose, then remove
the kept split:

```sh
git apply .marsh/split/3c8145fd9046/out/claude/diff.patch
marsh splits rm 3c8145fd9046
```

That `git apply` is the step that brings an agent's change into your working
tree. If you edited the same lines while the agents worked, use
`git apply --3way`
([Merging when your tree has moved](../split.md#merging-when-your-tree-has-moved)).

## When it goes wrong

| What you see | What it means | What to do |
|---|---|---|
| Both branches `exited 1` or `exited 2` | Neither fix passed | Read both stderr sections. Sharpen the prompt (name the file, paste the error) and race again |
| A branch exits nonzero with an authentication error | No credential for that agent | `sbx secret set anthropic` or `sbx secret set openai`, then rerun |
| `[starting codex worker VM…]` and a long wait | First use of that agent boots its VM and may download its image (about 3.5 GB) | Wait once; later runs reuse the warm VM. `marsh --load claude,codex` starts them ahead of time |
| One branch never finishes | An agent is stuck | Ctrl-C stops every branch and anything they started |
| `capacity: 8 jobs` in a branch's stderr | Eight jobs were already running on this daemon, so that agent never started | Wait for some to finish and rerun |
| `git apply` fails | Your tree moved since the split | `git apply --3way`, as above |

## Cost

Each branch is a full agent run, billed to that agent's account. A two-way
race costs two runs for one fix. You wait for the slower branch, not for both
in turn. A split takes up to 16 branches; at most 8 jobs run at once per
daemon.

## Next

- [Let a judge pick the winner](add-a-judge.md) when both branches pass.
- [Let an agent farm out work](farm-out-work.md) to have an agent run this
  race itself.
- [Split and join](../split.md) for every option.
