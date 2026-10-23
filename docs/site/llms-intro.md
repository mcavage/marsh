marsh is a shell for composing agents on Apple Silicon Macs. It opens a Linux
shell at the project's Mac path. Registered commands such as `claude`, `codex`,
and `pi` each run in a fresh nonroot container inside a warm VM for that Kit.
`msh` is the same program.

## Install

You need an Apple Silicon Mac (`uname -m` prints `arm64`), Homebrew, and Docker
Sandboxes (`sbx`) signed in. See https://runmar.sh/docs/install.md for the
supported `sbx` version and other installation methods. Docker Desktop is not
required to run marsh. Intel Macs, Linux, and Windows are not supported hosts.

```sh
brew install docker/tap/sbx mcavage/tap/marsh
sbx login                    # interactive sign-in
sbx secret set anthropic     # interactive key entry; claude and pi
sbx secret set openai        # interactive key entry; codex
marsh --version
```

The `sbx` sign-in and key-entry steps need an interactive terminal.
`sbx secret ls` lists stored credentials; marsh has no login of its own.
Provider keys managed by the `sbx` proxy stay outside jobs. An agent's own
login flow can save credentials in the shared guest home, where other jobs
can read them.

## Run

Use `marsh` for an interactive shell or `marsh -c 'COMMAND'` when your tool
cannot drive a terminal. The default shell is Brush, with Bash-compatible
syntax and the gaps listed at
https://runmar.sh/docs/shell.md#bash-compatibility. `--shell bash` and
`--shell zsh` select those shells inside the VM instead.

```sh
cd ~/src/project
marsh -c 'pwd; uname -sm'
marsh -c 'claude -p "summarize README.md" </dev/null'
```

`pwd` prints the same project path as on the Mac; `uname -sm` reports Linux
aarch64. Plain commands such as `git` and `make test` run in the shell VM.
Registered commands run in Kit containers, so packages installed in the shell
VM are not available to them. The first call to a Kit may need to download
its image and boot its VM. `marsh --load claude,codex` prepares those VMs
before opening the shell.

Agents called directly can change or delete project files. The packaged Kits
turn off the agents' own sandboxes and permission prompts. They do not see the
Mac's `~/.claude` or `~/.codex`; settings, history, and saved logins use marsh's
guest home (`$MARSH_HOME/home`, normally `~/.marsh/home`). Project instructions
such as `AGENTS.md` remain visible.

## Parallel work

`split` gives each branch a private project copy, including uncommitted
changes but excluding ignored files. Use `join --keep` to retain the patches
for review. For a project with a working `make test` target:

```sh
marsh -c 'split {
  a: claude -p "Fix the failing test without changing the tests." && make test
  b: codex exec "Fix the failing test without changing the tests." && make test
} | join --keep'
```

Run `marsh -c 'make test'` once before starting agents to check that the test
command works in Linux. A branch's `exited 0` means its command list succeeded.
Inspect the diff and test output before choosing a branch. With the reported
split ID and chosen label:

```sh
git apply --check .marsh/split/ID/out/a/diff.patch
git apply .marsh/split/ID/out/a/diff.patch
marsh -c 'make test'
marsh splits rm ID
```

Registered commands in a split mount only their fork. Your own shell code,
including each `make test` above, starts in the fork but is not confined there.
It can write elsewhere in the shell VM. All branches still share the guest
home. Without `--keep`, a successful `join` removes the split and patches; a
failed branch keeps them. See https://runmar.sh/docs/split.md.

`fanout` shares files instead of copying them. Use it only when the commands
can safely run together. This example prints both results in
declaration order:

<!-- doc-test: host stdout="== second (complete) ==\ntwo" -->
```sh
marsh -c 'fanout { first: printf "one\n"; second: printf "two\n" } | collect'
```

The brace forms require Brush. From another shell, use the CLI forms:

```sh
marsh split -n ::: a claude -p 'fix the bug' ::: b codex exec 'fix the bug' | marsh join --keep
marsh fanout -n ::: tests make test ::: lint make lint | marsh collect
```

For `split`, `::: LABEL CMD ARG...` runs one registered command in its fork.
A `fanout` argv branch can run an ordinary command too. `-b LABEL='commands'`
runs a shell branch, but is allowed only from the user's shell, never from a
job. `-n` supplies empty stdin. Both forms allow at most 16 branches.

## Inside a marsh job

`$MARSH_JOB` identifies a Kit job. Registered commands and the in-job `marsh`
are on `PATH` under `/run/marsh/bin`. Calling a registered command starts a
managed child job with the parent's project files, under the child Kit's
network policy and credentials.

- Children get no terminal. Supply `</dev/null`; if your tool gives stdout a
  terminal, redirect it to a file or pipe. With `| cat`, enable `pipefail` to
  preserve a child's failure status.
- Inside a job, `marsh` supports `run`, `split`, `join`, `fanout`, `collect`,
  `jobs`, and `context`, not host controls such as `status` or `stop`. Split
  and fanout accept only `:::` branches here. `marsh context` prints the
  current job's instructions and limits.
- `marsh run --no-spawn NAME ARG...` starts a child that cannot launch more
  jobs. `MARSH_SPAWN=codex,pi` or `none` narrows permitted child commands;
  nothing in a job can widen its inherited permission.
- Allow time for image downloads and model calls. Killing a parent cancels
  its children. Ctrl-C cancels the foreground tree with INT, then KILL after
  10 seconds if needed.

Fixed limits are 16 concurrent jobs per daemon, depth 4, 8 live children per job,
and 128 launches per tree, including refused child calls. The same Kit may
appear at most twice consecutively in a lineage. The per-tree Kit VM cap
defaults to 4 and can be set with `MARSH_TREE_KIT_VMS`. Refusals exit 125;
registered-name links return 126 for a spawn-set refusal, while `marsh run`
returns 125. See https://runmar.sh/docs/agents.md for exact messages.

## Inspect and clean up

```sh
marsh jobs --tree         # running and recent jobs, with children
marsh results             # exit status, wall time, cleanup
marsh status              # daemon, VMs, effective limits
marsh workers reset KIT   # retire an idle or quarantined Kit VM
marsh stop                # remove owned VMs and stop the daemon
```

`stop` refuses while shells or jobs are active and keeps the project, guest
home, and results. Results contain command arguments and paths, so do not put
secrets in them; they do not capture command output. For authentication errors,
ask the user to check the relevant `sbx` credential. For `capacity` or
`fan-out limit`, wait for jobs to finish before retrying. Other admission
limits require a smaller or less deeply nested plan.
