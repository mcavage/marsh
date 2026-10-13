marsh runs only on Apple Silicon Macs (`uname -m` prints `arm64`). It needs
Docker Sandboxes (`sbx`) 0.45.0 or newer, signed in; `sbx version` shows the
version. `sbx` ships only as a Homebrew cask, so Homebrew is required. marsh
does not run on Intel Macs, Linux, or Windows. Docker Desktop is not needed.
`msh` is the same program as `marsh`.

## Install

```sh
brew install docker/tap/sbx mcavage/tap/marsh
sbx login                    # interactive sign-in
sbx secret set anthropic     # interactive: prompts for a key; used by claude and pi
sbx secret set openai        # interactive: prompts for a key; used by codex
marsh --version
```

The three `sbx` steps need the user. If you are an agent installing marsh,
run the `brew` line, then ask the user to run the `sbx` lines; do not invent
credentials or export API keys as environment variables. `sbx secret ls`
shows which credentials are stored. marsh has no login of its own. To check
that a credential works: `marsh -c 'claude -p "reply with OK" </dev/null'`
(the first call to each agent takes minutes; see below).

To install marsh without its Homebrew formula: `brew install docker/tap/sbx`,
the `sbx` lines above, then `curl -fsSL https://runmar.sh/install | sh`. It
installs into `~/.local/bin`; if that is not on `PATH`, run
`export PATH="$HOME/.local/bin:$PATH"`.

## Run

Run marsh from a project directory (a Git repository or a plain directory).
`marsh` opens an interactive Linux shell in a VM, at the same path as on the
Mac, with the project mounted read/write. `marsh -c 'CMD'` runs one command
and exits, like `bash -c`; use it when you cannot drive an interactive
terminal. The shell is Brush, which runs Bash syntax; the differences are
listed at https://runmar.sh/docs/shell.md#bash-compatibility.

```sh
cd ~/src/project
marsh -c 'pwd; uname -sm'                          # /Users/you/src/project, Linux aarch64
marsh -c 'claude -p "summarize README.md" </dev/null'
```

There are two kinds of place where commands run:

- The shell VM: Debian arm64 with git, make, gcc, Python 3, Node.js, Rust,
  and passwordless `sudo apt-get`. Plain commands (`make test`, `git`) run
  here. Packages you install here stay until the VM is removed.
- Kit containers: `claude`, `codex`, `pi`, and `shell` (plain `/bin/sh`, no
  credentials) are registered commands: names that marsh runs in containers. Each call runs in a new nonroot
  container in that Kit's microVM, with the project at its usual path, and
  the container is deleted when the call exits. Packages installed in the
  shell VM are not in these containers. The claude, codex, and pi Kits
  include git, make, gcc, Python 3, Node.js 24, and ripgrep (no Go, no Java,
  no package manager).

The first call to each registered command boots its VM and may download about
600 MB; expect a few minutes and the notice `[starting NAME worker VM…]`.
`marsh --load claude,codex` boots VMs ahead of time. Agents run with their own
permission prompts and sandboxes turned off; the container is the sandbox. An
agent called directly can edit or delete project files. Agents in marsh do not
see the Mac's `~/.claude` or `~/.codex`; their settings live in marsh's guest
home (`~/.marsh/home`). Project files such as `AGENTS.md` are visible.

## Parallel work

A complete race, run from the Mac, that keeps the results so you can apply
one. First run the test command once (`marsh -c 'make test'`): if it cannot
run in the shell VM, every branch fails after paying for its agent run.

```sh
cd ~/src/project
marsh -c 'split {
  a: claude -p "make test fails. Fix the code, not the tests." && make test
  b: codex exec "make test fails. Fix the code, not the tests." && make test
} | join --keep'
```

Read the output. The first line is `# split ID: 2 branches; ...`. Each branch
has a header such as `== a (exited 0, shell-vm) ==`; `exited 0` means the
agent finished and `make test` passed on that copy. A branch that passed is a
candidate. If both passed, prefer the smaller patch that does not touch test
files, or ask the user. If neither passed, apply nothing. Then, with ID and
the chosen label filled in:

```sh
git apply --check .marsh/split/ID/out/a/diff.patch   # does it apply to the tree now?
git apply .marsh/split/ID/out/a/diff.patch
marsh -c 'make test'                                  # rerun on the real tree, in the VM
marsh splits rm ID
```

`git apply` works from the Mac or inside marsh. Run the tests with
`marsh -c`, since the branches ran them in the Linux VM.

`split { }` and `fanout { }` are marsh shell syntax: they work at the marsh
prompt and in `marsh -c`. Each line in the braces is `LABEL: commands`, and
the commands are ordinary Bash run on that branch's private copy of the
working tree (uncommitted changes included). Without `--keep`, `join` deletes
the copies and patches after a run in which every branch succeeded. When any
branch fails, `join` exits nonzero and keeps them.

```sh
fanout { lint: make lint; test: make test } | collect   # same files, no copies
```

The CLI form works from any shell, from bash or zsh on the Mac, and from
inside a marsh job:

```sh
marsh split -n ::: a claude -p "fix it" ::: b codex exec "fix it" | marsh join --keep
# Apply branch a only if it exited 0 and its patch applies, then rerun the tests:
marsh split -n ::: a claude -p "fix it" ::: b codex exec "fix it" |
  marsh join -- sh -c 'cat; grep -qx "exited 0" "$SPLIT_DIR/a/status" &&
    git apply "$SPLIT_DIR/a/diff.patch" && make test'
marsh fanout -n ::: lint make lint ::: test make test | marsh collect
```

- `::: LABEL CMD ARG...` is one branch that runs one command; the next `:::`
  ends it. `-b LABEL='shell string'` is a shell branch, allowed only from the
  user's shell, never from inside a job.
- `-n` gives branches empty stdin. Use it unless you mean to pipe input.
- `marsh join -- CMD` runs CMD with join's output on stdin and `$SPLIT_ID`
  and `$SPLIT_DIR` set. The patch for branch LABEL is
  `$SPLIT_DIR/LABEL/diff.patch` (absent if the branch changed nothing), its
  changed paths are in `$SPLIT_DIR/LABEL/files`, and its status (`exited 0`,
  ...) is in `$SPLIT_DIR/LABEL/status`. CMD runs even if branches failed. The
  split is deleted when CMD exits 0 and kept otherwise. A `:::` branch that
  runs only an agent exits 0 when the agent does, whatever it changed; check
  the patch and run the tests yourself.
- `marsh join -- CMD` exits with CMD's status. Without `-- CMD`, `join`
  exits with the first nonzero branch status, and a nonzero status keeps the
  split. `collect` exits with the first nonzero branch status. `marsh splits`
  lists kept splits; `marsh splits rm ID` removes one.
- At most 16 branches per split or fanout.
- A split's branches change only their copies; your files change when you
  apply a patch. A fanout branch, or an agent called directly, writes the real
  files.

## Inside a marsh job

When an agent runs in marsh, `$MARSH_JOB` is set and `claude`, `codex`, `pi`,
`shell`, and `marsh` are on PATH (`/run/marsh/bin`). Calling one starts a
child job with the same project files. In a job:

- Give a direct child `</dev/null`. marsh refuses a child whose stdin or
  stdout is a terminal. Agent shell tools usually capture stdout; if yours
  gives commands a terminal, send stdout to a file or a pipe (with `| cat`,
  set `-o pipefail` to keep the child's exit status).
- Use only `:::` branches with `marsh split` and `marsh fanout`.
- The in-job `marsh` has `run`, `split`, `join`, `fanout`, `collect`, `jobs`,
  and `context`; not `results`, `status`, or `stop`. `marsh context` prints
  this job's instructions. `marsh run --no-spawn NAME ARG...` starts a child
  that cannot start anything itself.
- These calls take minutes. Give your shell tool a timeout of 10 minutes or
  more; if the calling command is killed, marsh cancels its children.

Limits: 8 jobs at once per daemon; depth 4; 4 live children per job (split
and fanout branches count); 64 jobs per tree; 4 distinct Kit VMs per tree;
the same Kit at most twice in a row (claude → claude → claude is refused).
A refused launch starts nothing, prints the reason on stderr, and exits 125
(126 for a name outside `MARSH_SPAWN`). `capacity` and `fan-out limit` clear
when running jobs finish. For the others, start fewer jobs, nest fewer
levels, or use fewer distinct agents.
`export MARSH_SPAWN=codex` (or `none`) in the user's shell limits what jobs
may start.

## Inspect and clean up

```sh
marsh jobs --tree         # jobs and their children
marsh results             # exit status, wall time, cleanup per job
marsh status              # daemon, VMs, limits
marsh stop                # remove marsh's VMs and stop its daemon; keeps project and home
marsh workers reset KIT   # retire one Kit's VM, e.g. after "cleanup uncertain"
```

Ctrl-C cancels the whole job tree (INT, then KILL after 10 s) and exits 130.

Common errors: `marsh needs Docker Sandboxes` (install `sbx`); an agent
exits nonzero with an authentication error (the user runs `sbx secret set
...`); `capacity: 8 jobs` (wait and retry); `interactive child jobs are not
supported yet` (add `</dev/null`, and make stdout a file or pipe).
