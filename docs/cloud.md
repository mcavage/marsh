# Run commands in Docker Cloud (preview)

**Preview.** `cloud { ... }` and `marsh cloud` run shell work from your marsh
session in a Docker Cloud sandbox and bring file changes back. This page
describes what works now. Nested placement, registered Kits inside cloud work,
and an interactive cloud shell are not available yet; see
[Current limits](#current-limits). The design and remaining gates are in
[cloud placement](design/cloud-placement.md).

## Requirements

- A Docker account with **Docker Cloud sandbox access** (subscription and
  quota), signed in with stock `sbx login`. marsh uses whatever account the
  stock CLI is signed in to. A model-provider subscription does not supply
  cloud compute.
- Cloud sandboxes, transfers, and run time may be billed. Check Docker's terms.
- Your project files are uploaded to the sandbox (Git-ignored files excluded).
  Ignore rules are not a secret scanner.

## Run a block in the cloud

From your marsh shell:

```sh
printf 'pear\napple\n' | cloud { sort; }
```

This prints `apple` then `pear`. The producer runs locally, `sort` runs in the
cloud, and output streams back while the remote command runs. A cloud block
works where a command does: in a pipeline, after `&&` or `;`, a
`fanout` branch, or a `split` branch.

The block is evaluated by a fresh shell in the sandbox, like a split shell
branch. Exported variables and the current directory go with it (marsh drops
credential-like names such as `*TOKEN*` or `*KEY*`, as it does for jobs).
Aliases, functions, unexported variables, and shell options do not. Changes
to the directory or variables inside the block do not affect your shell.

Redirections outside the block are local; inside, they are remote:

```sh
cloud { cat input.txt; } > output.txt   # local output.txt
cloud { cat input.txt > output.txt; }   # remote output.txt, written back
```

The same from bash, zsh, or a host terminal:

```sh
marsh cloud -c 'make test && ./bench > results.txt'
marsh cloud -- python3 train.py --epochs 3      # argv, no shell parsing
```

stdin, stdout, stderr, and the exit status pass through; `PIPESTATUS` and
`pipefail` behave as for a local command. A progress notice
naming what the sandbox runs (`[starting a Docker Cloud sandbox: marsh cloud
shell…]`, or `Kit claude` for an agent or a job's own Kit) and the write-back
summary go to stderr, never into stdout. Each call starts a new sandbox; expect roughly
20 seconds of setup before the command starts.

## Sandbox size, lifetime, and image

Options go before the block or the command:

```sh
cloud --cpus 8 --ttl 2h { make -j8 test; }
marsh cloud --cpus 4 -- ./train.sh
```

- `--cpus N`: 1, 2, 4, 8, or 16, with 2 GiB of memory per CPU. Default 2.
- `--ttl DURATION`: the sandbox's lifetime limit, from `1m` to `24h`. Default
  `4h`. A command still running when it expires is lost, not written back.
- `--image REF`: another image, pinned by digest
  (`REPOSITORY@sha256:...`), Linux ARM64. It must provide `sudo` for the
  `agent` user, `useradd`, `setsid`, and `gzip`, as the default image does.

`MARSH_CLOUD_CPUS`, `MARSH_CLOUD_TTL`, and `MARSH_CLOUD_IMAGE` set defaults
for the options you leave out ([configuration](configuration.md#environment)).

## Interactive cloud shell

Run `marsh cloud` with no command, from a terminal (your marsh shell or a host
terminal), for an interactive shell in a new sandbox:

```sh
marsh cloud --cpus 4
```

It has a real terminal of your window's size, follows resizes, and supports
line editing. When you exit, its file changes are written back, as for a
block, and `marsh cloud` exits with the shell's status; `marsh cloud sync`
from another terminal brings them back sooner
([sync while it runs](#sync-while-it-runs)). Ctrl-C is sent to the
remote program, as in any terminal; closing your terminal ends the session.

## Sync while it runs

A long-lived block, such as an interactive cloud shell or an agent that
never exits by itself (`marsh cloud -- claude --acp`), writes its changes
back only when it ends. To bring them back sooner, sync it:

```sh
marsh cloud sync 42          # one block
marsh cloud sync --all       # every running attached block
marsh cloud sync --json 42   # {"synced": [{"cursor": 42, "created": 1, ...}]} on stdout
```

Each attached block names its job on stderr when its command starts:
`` marsh: cloud: job 42 running (JOB_ID); `marsh cloud sync 42` writes its changes back now ``.
`marsh jobs --json` lists it too (`placement` `cloud`, `state` `running`).

- The block keeps running in the same sandbox; sync only reads its files.
  It is one-way: local edits are not uploaded.
- Each changed path is checked as a detached run's are: a path also changed
  here since it was uploaded (or last synced) is skipped, your local file
  kept, and reported as a conflict. The rest is written as at the end of a
  block, with the same rules for directories and names.
- What a sync wrote becomes the new base: a later sync, and the final
  write-back, carry only newer changes. A path skipped as a conflict is
  checked again each time.
- Once a block has synced, its final write-back is checked the same way:
  conflicting paths are skipped and reported, and the full change set is kept
  under `cloud/results/JOB.delta` in your control home.
- stderr reports each sync, for example
  `marsh: cloud: job 42: synced 3 changes (1 created, 2 modified, 0 deleted); 1 conflict (changed here too, kept local): notes.txt`.
  `marsh cloud sync` exits 1 when a sync failed (its message says why and
  what was already written); a later sync or the final write-back retries.
- Sync is for attached blocks started from your session or a host terminal.
  A detached run's changes come back when it is collected. `marsh status
  --json` lists the `cloud-sync-v1` feature.

## File changes come back automatically

When the command ends, marsh compares the sandbox's project with the snapshot
it uploaded and writes the differences back to your project before the
command returns:

```sh
cloud { ./generate-assets; } && make test   # make sees the generated files
```

- Created, modified (including the executable bit), and deleted files and
  directories return; symlinks return as links and are never followed.
- Unchanged files do not, so your local edits to other files survive.
- **A returned change overwrites your local edit to the same path.** There is
  no merge or conflict check (unless the block [synced](#sync-while-it-runs)).
  Concurrent cloud calls that write the same file: the last one applied wins.
- A command that exits nonzero still returns its changes, as a local command
  would have made them.
- New files whose names match ignore rules (build outputs, logs) return too:
  ignore rules filter what is uploaded, not what comes back.
- `.marsh` is never uploaded. `.git` is never written back.

## Git in the cloud

The sandbox gets a private copy of the project's Git metadata, so `git status`,
`git diff`, `git log`, and commits work in the block. In a split branch it is
the branch's own repository view. The copy holds only what Git needs for that (`HEAD`,
refs, objects, the index, `info/exclude`): no hooks, no remotes, no settings
from your Git config except the repository format, and no tool state such as
git-crypt keys; commits and
other Git changes made in the cloud stay in the sandbox and are deleted with
it. Only working-tree file changes come back. Metadata larger than 512 MiB (or
with a file over 256 MiB) is not uploaded; marsh says so on stderr and the
block runs without it.

stderr reports what was written, for example
`marsh: cloud: wrote back 3 changes (1 created, 2 modified, 0 deleted)`.

A directory deleted in the cloud that still holds local files here (ones that
were never uploaded, such as ignored caches) is kept, and the summary names
it.

The write-back is per file, not one transaction. If it stops (a local
directory in the way of a returned file, a name that would alias another on
your case-insensitive Mac volume, the project directory replaced meanwhile),
marsh says which entry failed and how many changes were already written,
exits 125, and keeps the full change set in your control home under
`cloud/results/JOB.delta`. It never retries a write-back by itself. If the
changes cannot be downloaded at all (for example a file over the size limit),
nothing is written back and marsh keeps the sandbox instead of deleting it,
naming its ID so you can recover files with `sbx --cloud exec ID sh`;
`marsh workers reset cloud` deletes it.

## Keep cloud changes off your source tree

Run the block in a one-branch [split](split.md). Changes return to the
branch's private copy, and `join` shows them as a patch:

```sh
split { fix: cloud { ./codemod.sh; } } | join --keep
```

## Fanout

Each `cloud` block in a [fanout](fanout.md) is its own sandbox with its own
snapshot, taken when that branch starts; their changes return to the shared
project and can race:

```sh
fanout { a: cloud { ./gen a; }, b: cloud { ./gen b; } } | collect
```

A fanout inside one cloud block shares that sandbox's files:

```sh
cloud { fanout { tests: make test; lint: make lint; } | collect; }
```

## Registered commands inside a cloud block

Registered commands whose Kit is pinned by digest (the packaged `claude`,
`codex`, `pi`, and `shell`, and Kits you register by `REF@sha256:...`) can be
called inside a block. Each runs in Docker Cloud from the same Kit it runs
locally:

```sh
cloud { make build && claude -p 'fix the failing test, then run make test'; }
```

Each call runs in its own new cloud sandbox made from that Kit: its image,
entrypoint, network policy, and the credentials you stored for Docker Cloud
(for example with `sbx --cloud secret`; not your local Kit logins). It starts
with the block's current files, including files the block generated, and a
private Git copy. Its stdin, stdout, stderr, and exit status pass through to
the block. When it ends, its file changes are written into the block's files
before the call returns (`marsh: claude: wrote back ...` on stderr), and they
come back to your project with the block's changes.

- A block that is only one such call, with literal words (and at most
  `</dev/null`), such as `cloud { claude -p 'fix the test' </dev/null; }`,
  runs straight in that Kit's sandbox: one sandbox and one job (`claude`,
  placement `cloud`), with no cloud shell in between. Anything more (a
  second command, a pipe, `$VAR`, a glob, another redirection) uses the
  cloud shell as above.
- The call sees a copy, not the live block: something else in the block
  writing at the same time does not see the agent's edits until it returns,
  and an overlapping write is overwritten by the agent's change.
- The call is a child job of the cloud job (`marsh jobs --tree`), under the
  usual limits and `MARSH_SPAWN`. Ctrl-C reaches it; its confirmed changes
  still return.
- It runs as the Kit's own user, as it does locally.
- A Kit registered from a source directory has no cloud form and is not
  available in a block. To give it one, publish it and name the published
  reference in `cloud-kits.json` in your control home:
  `{"mytool": {"kit": "docker.io/me/mytool@sha256:..."}}`.
- The first use of a Kit in a sandbox shape builds a small Docker Cloud
  template from it (`sbx --cloud template ls` lists them); later calls reuse
  it.

## Cloud from a job

A Kit job can place work in Docker Cloud with the in-job `marsh cloud`:

```sh
claude -p "use your Bash tool to run: marsh cloud -c 'make test'"
```

The work runs in a new cloud sandbox made from the job's own Kit, with the
job's project view (its project or split fork) at the same path. Streams,
status, Ctrl-C, write-back, and cleanup behave as for a block from your
session, with these differences:

- It is a child job of the calling job, under the usual limits. `cloud` is a
  name in the spawn set: `MARSH_SPAWN` without `cloud` refuses it
  (`spawn refused: cloud not in this job's spawn set (MARSH_SPAWN)`).
- The sandbox lives no longer than the calling job's remaining wall time.
- If the calling job ends first, the cloud work is cancelled and its changes
  are kept on your Mac (the path is printed in the job's notes) instead of
  being written into the job's files.
- It is refused when the job's Kit has no cloud form (see above) or was
  registered again since the job started, when the job cannot write its
  project view, and with `--image`.
- In a Kit sandbox, marsh's control steps run as the Kit's own user, so they
  cannot stop processes the workload hides from them. The returned changes
  are still checked on your Mac and written only into the job's project view. `marsh cloud` from a job takes `[--cpus N] [--ttl DURATION]
-c SCRIPT` or `-- PROGRAM [ARG...]`; it has no interactive form.

## Split inside a cloud block

`split { ... } | join` works inside a block. The forks are made from the
block's current files inside the sandbox, the branches run there, and `join`
renders their patches there; the cloud block's own changes still come back
as usual, but the split's forks do not:

```sh
cloud { ./prepare; split { fast: ./tune --fast, safe: ./tune --safe } | join; }
```

Branch placement shows as `cloud`. A split kept with `join --keep` stays in
the sandbox and is deleted with it, so copy anything you want out of it
within the block. Host `marsh splits` and `marsh jobs --tree` show only the
cloud job, not the split inside it.

## Close the lid: detached runs

Add `-d` to start work that keeps running after you close the laptop:

```sh
cloud -d { make long-benchmark > bench.txt; }
marsh cloud -d -- make long-benchmark
```

marsh uploads the project as usual, starts the command, prints its job
number, and returns at once. The run has no terminal and reads no input.

```sh
marsh jobs                  # running, paused, finished, collected, kept-as-patch, lost
marsh cloud attach 42       # output so far, then live; Ctrl-C stops following, not the run
marsh cloud collect 42      # check on it now
```

- **Collection happens by itself.** marsh checks detached runs when its
  daemon starts and every two minutes while one is outstanding. When a run
  has finished, marsh downloads its output and file changes, deletes the
  sandbox, and tells you once, on your next `marsh` command:
  `marsh: cloud job 42 finished (exit 0); changes applied`.
- **Conflicts wait as a patch.** marsh remembers every file it uploaded. If
  any file the run changed was also changed here since it started, nothing is
  written back: `marsh cloud diff 42` shows the patch, `marsh cloud apply 42`
  applies it anyway, and `marsh cloud discard 42` drops it. A split fork that
  is gone, or a project directory that was replaced, also keeps the patch.
- **The time limit pauses, it doesn't kill.** A detached run's `--ttl`
  defaults to 2 hours of running without your laptop. When it runs out, the
  sandbox is suspended with the run inside it; the next check resumes it
  where it was, for another `--ttl`. While your laptop is on, marsh's checks
  keep it going.
- **Costs.** A sandbox is billed while it runs. If your laptop is away when
  the work finishes, the sandbox idles until its time limit suspends it, so a
  shorter `--ttl` bounds that idle time. A run nobody collects is deleted
  after 14 days.
- `marsh cloud stop 42` interrupts a run (`INT`, then `KILL` after 10
  seconds); its changes so far come back as usual.
- `marsh stop`, `marsh reset`, and `marsh workers reset cloud` keep
  detached runs and say how many they kept; add `--detached` to discard them.
- Up to 8 detached runs at a time. They don't count toward the 8 jobs that
  run at once.

A block that is exactly one call to a registered command runs in that
command's Kit, so `cloud -d { claude -p "refactor the parser" </dev/null; }`
works, and so does the argv form, `marsh cloud -d -- claude -p "refactor the
parser"` (a detached run reads no input). The command needs a cloud form: its
Kit pinned by digest, as in a release install. A dev install builds the
packaged Kits from source; marsh then refuses the run and says so, and
`cloud-kits.json` (above) names a published Kit for it. The control home is
the `control_home` that `marsh status` prints. Detached runs start from your marsh session or a host
terminal, not from inside a job. Design:
[detached cloud work](design/cloud-detach.md).

### Agents calling agents with the lid closed

In an attached block, your laptop starts a sandbox for each registered Kit
the block calls (`codex exec ...` inside `cloud { }`). A detached run has no
laptop to do that, so by default marsh refuses a detached block that calls
one, and a call it misses at start fails in the sandbox (exit 126).

To let detached runs start those sandboxes themselves, give marsh a Docker
access token and turn on delegation:

1. In Docker Hub, create a personal access token with **Public Repo
   Read-only** access (**Read-only** if your Kits are private) and **Allow
   Cloud Sandboxes access**. The stock CLI in the sandbox fetches each Kit
   from Docker Hub with this token, so a token with only sandbox access is
   refused when the run starts.
2. Store it in your login Keychain, with your Docker ID as the account:

   ```sh
   security add-generic-password -s marsh-cloud-delegate -a YOUR_DOCKER_ID -w
   ```

3. `marsh config cloud-delegate on` (`off` turns it off again).

Then a detached block can call any registered Kit that has a cloud form, as
an attached block can:

```sh
marsh cloud -d -- sh -c 'codex exec "add a test for the parser" </dev/null; make test'
```

Each run that can do this says so when it starts:
`marsh: this run can start agent sandboxes in Docker Cloud as YOUR_DOCKER_ID (cloud-delegate on)`.
Each call starts its own sandbox from the Kit, with the run's current files;
its output appears in the run's output (`marsh cloud attach`), and its file
changes return to the run before the call returns. Model credentials come
from your Docker Cloud secrets (`sbx --cloud secret`), as for attached
calls. `marsh jobs show JOB` lists each call (`agent call: codex exec ...
(exit 0); wrote back 1 change`).

**What the token can do.** Docker's sandbox access is all or nothing: the
token can create, use, and delete every cloud sandbox on your account and
spend its quota. marsh uploads it into each such run's sandbox, into a file
only root can read; the run's commands run as an unprivileged user without
`sudo` and can't read it. They can only ask the run's supervisor to start a
call. If you don't want a token in your sandboxes, leave delegation off.

**marsh's limits on a run's calls**, enforced by the supervisor:

- only the registered Kits this run could call (the same Kits and pinned
  versions an attached block gets, narrowed by `MARSH_SPAWN`);
- at most 8 agent sandboxes per run in all (`MARSH_CLOUD_CHILDREN` in the
  daemon's environment, 1 to 64); the next call is refused;
- each gets what remains of the run's `--ttl`, and pauses rather than ends
  when it runs out;
- each is deleted when its call returns, any left are deleted before the
  run's end is recorded, and collecting, `marsh cloud discard`, or retention
  deletes any the run could not;
- the called agent can't call further agents (its sandbox has no command
  links), as in an attached block.

Delegation needs the marsh cloud shell (the default image, or an `--image`
with `curl`, `tar` and `sudo`): the supervisor installs a pinned Docker
Sandboxes CLI in the sandbox and signs in with the token.

## Jobs, Ctrl-C, and cleanup

Each cloud call is a job: `marsh jobs` shows `cloud -c ...` with placement
`cloud`, and `marsh jobs show JOB --json` records the sandbox ID (`vm_id`),
the exit status, and the cleanup result. Cloud calls count toward the
8-jobs-at-once limit.

- **Ctrl-C** sends `INT` to the remote command (then `KILL` after 10 seconds).
  When it has stopped, its changes so far are written back and the call exits
  130. Ctrl-C during sandbox setup stops before anything runs.
- Closing the pipe downstream (for example `| head -1`) cancels the call the
  same way; the call still writes back before it exits, with status 141 (as
  a process killed by `SIGPIPE`).
- When the command ends, marsh stops every process it started, including
  background ones, before reading its changes. (In a Kit sandbox this is
  best effort; see [Cloud from a job](#cloud-from-a-job).)
- After every call marsh deletes the sandbox and checks that it is gone.
- `marsh workers reset` (all) and `marsh workers reset cloud` delete every
  cloud sandbox this daemon owns, including those of calls still running;
  such calls end as lost and write nothing back. `marsh stop` and
  `marsh reset` delete owned cloud sandboxes too. Detached runs are kept
  unless you add `--detached`.
- If marsh loses the connection or the daemon stops before it can confirm the
  remote command ended, nothing is written back and nothing is re-run. The job
  shows `cleanup uncertain` and holds its slot. Run
  `marsh workers reset cloud` to delete every cloud sandbox this daemon owns
  and release it. Sandboxes also expire on their own after 4 hours.

## Current limits

- Laptop to cloud only: cloud work starts from your marsh session, a host
  terminal, or a local Kit job. From inside a cloud block or sandbox, `cloud`
  is refused, and cloud work cannot call back to your laptop. A
  `marsh run --place` form is not available yet.
- Inside a block, only registered commands with a cloud form (above),
  `split {}`, and `fanout {}` are available; `marsh` subcommands are not.
- The block runs as an unprivileged user (`marsh-run`) in a fixed shell image
  (`docker/sandbox-templates:shell-docker`, pinned by digest, Ubuntu) with
  2 CPUs, 4 GiB of memory, Linux ARM64, the sandbox's network policy, and a
  fresh home directory. It has no `sudo` and no Docker access. Your marsh home
  is not uploaded. GPUs are not offered.
- Blocks have no terminal (programs see pipes); use the
  [interactive cloud shell](#interactive-cloud-shell) for one.
- Limits: 256 MiB per file and 1 GiB per upload or change set.
