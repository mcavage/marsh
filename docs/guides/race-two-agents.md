# Race two agents on one bug

You have a failing test. Instead of trying one agent and then another, run
both at once, each on its own private copy of your project, let each prove its
fix with your own test suite, and read what comes back. Nothing touches your
files until you apply a patch.

You need a project with a test command that exits nonzero when a test fails,
and a credential for each agent
([Agent credentials](../quickstart.md#agent-credentials)).

## Check that your tests run in the VM

The commands in a branch run in marsh's Linux VM, not on your Mac. That VM is
Debian on arm64 with git, make, gcc, Node.js, and Rust. If your tests need
something else, install it once before you race. Every branch shares the VM, so
all of them see it, and it lasts as long as the VM does:

```sh
sudo apt-get install -y python3-pytest
```

Run your test command in the marsh shell first. If it passes or fails there the
way it does on your Mac, the gate in the next step means something.

## Race

<!-- doc-test: shell replace=["split {\n  claude: claude -p \"TestDialTimeout fails. Fix it without editing the test.\" && make test\n  codex:  codex exec \"TestDialTimeout fails. Fix it without editing the test.\" && make test\n} | join --keep\n", "out=$(split {\n  claude: fixture project-write notes.txt $'alpha\\nbeta\\ngamma\\n' && grep -q gamma notes.txt\n  codex:  fixture project-write notes.txt $'alpha\\nbeta\\n' && grep -q gamma notes.txt\n} | join --keep)\nprintf '%s\\n' \"$out\" | grep -q '^== claude (exited 0'\nprintf '%s\\n' \"$out\" | grep -q '^== codex (exited 1'\nid=$(printf '%s\\n' \"$out\" | head -1 | cut -d' ' -f3 | tr -d :)\ngit apply \".marsh/split/$id/out/claude/diff.patch\"\ngrep -q gamma notes.txt\nmarsh splits rm \"$id\"\n"] -->
```sh
split {
  claude: claude -p "TestDialTimeout fails. Fix it without editing the test." && make test
  codex:  codex exec "TestDialTimeout fails. Fix it without editing the test." && make test
} | join --keep
```

Each line is a branch with its own fork of your project. A branch is ordinary
Bash, so `agent && make test` means: run the agent on the fork, and if it
exits 0, run your tests on the same fork. A branch counts as passed only when
the agent finished and the tests passed. Both branches run at once.

`join` prints each branch's status, its output, and a patch:

```text
# split 8d5c8fdd1563: 2 branches; manifest …/out/manifest.json

== claude (exited 0, shell-vm) ==
…
ok  	example.com/api/net	0.412s
-- claude: 1 file (M net/dial.go); diff …/out/claude/diff.patch --
diff --git a/net/dial.go b/net/dial.go
…
== codex (exited 2, shell-vm) ==
…
--- FAIL: TestDialTimeout (0.31s)
-- codex: 2 files (M net/dial.go, M net/dial_test.go); diff …/out/codex/diff.patch --
diff --git a/net/dial.go b/net/dial.go
…
```

Here `claude` passed and `codex` did not. `--keep` leaves the results on disk
so you can apply one.

## Read before you apply

Passing tests prove that the tests pass, not that the fix is right. Two lines
in the output tell you more:

- The status in each `==` header: `exited 0` means the agent finished and your
  tests passed on that fork.
- The files in each `--` line. An agent that made a test pass by editing the
  test shows `M net/dial_test.go` there. Say "without editing the test" in the
  prompt, and treat a patch that touches tests as suspect anyway.

## Apply one

The split's id is in the first line of the output. Apply the patch you want
with `git apply`, then remove the kept results:

```sh
git apply .marsh/split/8d5c8fdd1563/out/claude/diff.patch
marsh splits rm 8d5c8fdd1563
```

Your tree changes only with that `git apply`. If it has moved since the split,
see [Merging when your tree has moved](../split.md#merging-when-your-tree-has-moved).

## What this costs

Two agents are two model runs for one fix. The race buys wall-clock time (you
wait for the slower one, not the sum) and a second attempt, not a cheaper one.
A split has at most 16 branches, and marsh runs at most 8 jobs at a time per
daemon ([Agents running agents](../agents.md#limits-and-messages)). If one
branch hangs, Ctrl-C stops all of them and anything they started.

## Next

When both pass, have a third agent choose: [Let a judge pick the
winner](add-a-judge.md). To use several agents on a change that is already
written, see [Review a change with a panel](review-with-a-panel.md).
