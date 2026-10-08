# Quickstart

Two agents attempt the same fix on private copies of your project. You read
both patches and apply the one you want. Your files do not change until you
do.

```sh
brew install docker/tap/sbx mcavage/tap/marsh
sbx login
```

Each agent in the examples needs a credential first
([Agent credentials](#agent-credentials)).

The first use of each agent downloads its Kit image (about 3.5 GB) into a new
VM, which takes a few minutes. marsh says so when it happens:
`[starting claude worker VM… first use downloads the Kit image (~3.5 GB), this takes a few minutes]`.
The first shell VM can download its template the same way. Marsh keeps a copy
of each Kit image, so later VMs for the same Kit start much faster.

## Split a task

Open marsh in a project. It works in a Git repository or a plain directory.

```console
$ cd ~/src/api
$ marsh
marsh-0.5$ split {
>   a: claude -p "fix TestDialTimeout"
>   b: codex exec "fix TestDialTimeout"
> } | join
```

Each branch runs on its own copy of the project (a *fork*). `claude` and
`codex` start as jobs in fresh containers that see only that fork. The first
call to each agent boots its VM and prints `[starting NAME worker VM…]`. When
both finish, `join` prints:

```text
# split 8d5c8fdd1563: 2 branches; manifest …/out/manifest.json

== a (exited 0, shell-vm) ==
The test raced on a fixed port; it now asks the kernel for a free one.
-- a: 1 file (M net/dial_test.go); diff …/out/a/diff.patch --
…
== b (exited 0, shell-vm) ==
Added a retry around Dial and a longer timeout.
-- b: 2 files (M net/dial.go, M net/dial_test.go); diff …/out/b/diff.patch --
…
```

Each branch's output comes first, then its patch (shown as `…` here). Ctrl-C
during the split stops every branch and anything they started.

## Apply one result

The `marsh split` form works from any shell. It prints a handle, and
`join -- CMD` runs CMD with the patches in `$SPLIT_DIR/LABEL/diff.patch`. Here
`:::` marks a branch that is one registered command:

```sh
h=$(marsh split -n ::: a claude -p 'fix TestDialTimeout' \
                   ::: b codex exec 'fix TestDialTimeout')
marsh join -- sh -c 'cat; git apply "$SPLIT_DIR/b/diff.patch"' <<<"$h"
```

`git apply` needs no repository, so this also works in a plain directory. The
split is removed when CMD exits 0 and kept otherwise. To merge when your tree
has changed, see [Split and join](split.md#merging-when-your-tree-has-moved).

Agents started by a split can write only their fork of the project. A shell branch
(`-b LABEL=STRING`, or the branch's own shell code in `split { }`) runs as you
and can write your real directory. marsh does not review patches.

## Agent credentials

marsh has no login. Docker Sandboxes keeps each agent's credential on your Mac
and its proxy adds it to the agent's requests. The container sees placeholder
values. After `sbx login`, store a key once per provider:

```sh
sbx secret set anthropic     # claude, and pi with Anthropic models
sbx secret set openai        # codex
```

`sbx secret import` takes keys from your environment variables instead. The
agent Kits also accept an OAuth login that `sbx` holds, and use whichever is
bound. `sbx secret ls` shows what is stored.

An agent with no credential fails on authentication. Store one and run it
again.

The guest home (`~/.marsh/home` on the Mac) keeps each agent's settings and
history. Every job can read it.

## Run an agent directly

`claude`, `codex`, `pi`, and `shell` are registered commands. Each call runs in
a fresh container in that command's VM, with your project mounted read-write:

```console
marsh-0.5$ claude -p 'summarize README.md in one line'
```

A direct call is not forked. The agent can edit or delete files in your
project. Registered commands work in pipes, scripts, and Makefiles:

```sh
git diff | codex exec 'review this diff' > review.txt
```

To boot VMs before you need them: `marsh --load claude,codex`.

## Agents running agents

Inside a job, the registered names are on `PATH`, so one agent can call
another. Every call joins one job tree:

```sh
claude -p "use your Bash tool to run: codex exec 'is this function correct?'"
marsh jobs --tree
```

```text
ID        STARTED   STATE      EXIT      RUN  COMMAND
301c56d6  2m ago    finished      0   2m12s  claude -p "use your Bash tool to run: codex exec…
8e41f0a2  1m ago    finished      0    14.2s  └─ codex exec "is this function correct?"
```

Ctrl-C stops the whole tree. At most 8 jobs run at once and a tree is at most
4 deep. See [Agents running agents](agents.md).

## Run commands side by side

`fanout` runs branches at once on the same files, with no forks, so branches
that write the same file can conflict. `collect` prints their output in the
order written:

```sh
marsh fanout -n -b test='make test' -b lint='make lint' | marsh collect
```

```text
== test (complete) ==
ok

== lint (failed: 2) ==

== lint stderr ==
src/a.rs:3: unused import
```

The exit status is the first nonzero branch status. See [Fanout](fanout.md).

## Inspect and clean up

```sh
marsh status      # daemon, VMs, limits
marsh jobs        # recent jobs
marsh results     # finished jobs: exit status, cleanup, timing
marsh stop        # remove every VM marsh created; stop the daemon
```

`marsh stop` leaves your project, the guest home, and job results. The next
`marsh` starts fresh VMs. If a command or VM seems stuck, see
[Troubleshooting](troubleshooting.md).

Next: [Split and join](split.md), [Agents running agents](agents.md),
[Agent sessions (ACP)](acp.md), [The shell](shell.md).
