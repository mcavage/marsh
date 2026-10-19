# marsh Brush pin

This source tree is pinned from `reubeno/brush` commit
`1389a8e8af6bec56fdfb9ab60a8c898bc14aa961`. It is checked in so a normal
`make build` does not depend on an unpublished fork.

The marsh source deltas are the ordered patches in
`../../docs/upstream/brush/`. The first two make registered commands ordinary
processes and complete the bounded background-job behavior. The third lets an
embedding binary pass Brush an explicit argument vector after consuming its
own product options. The fourth implements noninteractive `BASH_ENV`/`ENV`
startup-file behavior. The fifth delivers registered `INT` traps after a
foreground process group receives SIGINT. The sixth injects marsh's descendant
command directory and session context at the Brush entry point. The seventh
repairs generic trapped `TERM` and `wait -n -p` behavior, with a small
marsh-specific `fanout` TERM path. Generic portions can be proposed upstream
independently; product routing and composition remain local.
The eighth adds `wait -f`, standalone `wait -p`, and idle-input `TERM` trap
delivery. Patch `0009` retains the rightmost background pipeline PID independently
of the group ID. Patch `0010` preserves standard descriptor sources, closes
absent Unix standard descriptors in external children, retains launch-time
background `pipefail` across polling and canceled waits, and makes `jobs -p`
report the group ID without changing `$!`.
There is no `0011`: the former native-shell fork (and its vendored reedline
fork) was dropped; reedline comes from crates.io as upstream specifies.
Patch `0012` gives in-process background lists a waitable `$!` identity, keeps
their `exit` local, and retains reaped `wait PID` statuses. Patch `0013` keeps
ordinary `collect` commands out of the composition grammar. Patch `0014`
accepts `+o`/`+O` startup options. Patch `0015` makes `kill` default to `TERM`,
probe with signal 0, and accept multiple operands. Patch `0016` searches `PATH`
for slash-free script operands. Patch `0017` takes terminal control for an
interactive `-c` or script-operand shell, as the interactive loop already does.
Patch `0018` runs ENOEXEC files with Brush itself and makes a failed `exec`
restore descriptors and exit 126/127 unless `execfail` is set. Patch `0019`
lets a trapped `INT` interrupt `wait`. Patch `0020` makes `kill %JOB` reach
every pipeline stage. Patch `0021` lets `jobs` in pipeline stages and command
substitutions list the parent's jobs. Patch `0022` executes complete read
units before parsing later lines when the text does not parse whole. Patch
`0023` expands aliases lexically while reading. Patch `0024` runs subshells,
background lists, pipeline stages, coprocesses, process substitutions and
`$BASHPID`-observing command substitutions in forked processes, as Bash does.
Patch `0025` resolves `declare -n`/`local -n` name references, `0026`
implements `globstar`, `0027` parses `( (` as nested subshells, and `0028`
allocates `{name}>file` redirection descriptors. Patch `0029` implements
`printf '%(fmt)T'`, and `0030` keys associative elements in arithmetic
(`((count[$word]++))`) by subscript text. Patch `0031` lets `read -t` see
end of file on Darwin character devices such as `/dev/null`. Patch `0032`
delivers caught `INT`/`TERM` between commands. Patch `0033` adds marsh
`split { ... } | join`: the `split {` keyword beside `fanout {`, join
recognition on the stage after it, branches as forked shells in their own
process groups with graceful cancellation, and an embedding workspace hook
(`brush_core::split`). Patch `0034` makes `join` options-only: the stages
after it receive the rendering with `SPLIT_ID`/`SPLIT_DIR`/`SPLIT_MANIFEST`
exported, and the split directory is released after the last stage exits.
Patch `0035` makes split a thin client of the daemon's workspaces. Patch
`0036` makes interactive Ctrl-C abandon the rest of the command line
(`ExecutionControlFlow::Interrupted`), `0037` falls back to plain line
reading when the terminal never answers the cursor position query, `0038`
keeps one `PIPESTATUS` entry per stage when a split does not run, `0039`
reports Bash's line numbers in syntax errors, `0040` describes registered
commands in `type`/`command -v` as their session `PATH` link, and `0041`
makes `split { }`/`fanout { }` branches label-led: a `;` starts a branch only
before a `LABEL:` word and is otherwise Bash sequencing inside the branch.
Patch `0042` applies Bash's option order to `-c`: it is a flag, and the
command is the first operand after every option (`bash -c -l CMD`,
`bash -ce CMD`, `bash -c -o pipefail CMD`). Patch `0043` prints Bash's
interactive job lines: `[1] PID` at launch, `[1]+  Done<pad>CMD` or
`Exit N` on completion, `Running<pad>CMD &` in `jobs`, one `+`/`-` pair,
and job numbers reused after earlier jobs are reported. Patch `0044` keeps the
terminal's output flags (`OPOST`/`ONLCR`) on while reedline edits a line, so
background output at the prompt starts each line at column 0, as in Bash.
Patch `0045` makes the `collect` renderer public for the `marsh collect` CLI.
Patch `0046` renders a failed branch's stderr inline after its header
(`collect --stderr` shows every branch's).
Patch `0050` records caught `INT`/`TERM` arrivals in the signal handler, as
Bash does, so a shell checking between commands never misses a delivered signal.
Patch `0051` makes a forked child prove it can start a thread before it runs
anything, replacing one that cannot (a fork that lands while another thread is
starting leaves the child holding a lock nothing can release).
Patches `0052`-`0068` came from classifying the Bash differential suites
(`tests/acceptance/bash_suites.py`): `kill -- -PGID`, signals sent between
fork and exec, line feeds typed ahead at the prompt, non-UTF-8 inherited
environment entries, quoted `[[ =~ ]]` patterns, extended-glob `[[ == ]]`
patterns, `${!:-word}`, `read -u` above 255, `read` field splitting, unknown
`\D{}` prompt conversions, prompt directory names, PS4 without `xtrace`,
field splitting of expansion results only, unexpanded braces in assignments,
`"$*"` under an empty IFS, empty smallest pattern removals, and job checks
that ask the kernel.
Patch `0069` keeps the lines typed while the shell waited for a cursor
position report that never came (crossterm held them as parsed key events;
the plain line reader that takes over read the terminal itself).
Patch `0070` implements `disown`, `%name` job specs, Bash's retention of
jobs collected by `wait` until the next cleanup, and `huponexit`.
Patch `0071` implements Bash 5.3 function substitutions (`${ cmd; }`,
`${| cmd; }`); `0071a` keeps a late runtime dispatch of a delivered TERM
from running its trap again; `0071b` adds a single-threaded runtime option
for tests. `0071c` records an `INT` that arrives between
starting an external command and awaiting it. Patch `0072` treats NUL bytes in shell input as Bash does.
Patch `0073` keeps byte strings that are not UTF-8 losslessly
(`brush_core::bytes`). Patch `0074` replaces `${v/pat/rep}` matches as
Bash's `pat_subst` does. Patch `0075` gives an interactive shell without a
terminal Bash's warnings, prompts and job-control state.
Patch `0076` expands redirections after assignments, and those of a
command that runs as a process in a copy of the shell.
Patch `0077` gives each `fanout` branch one process group that a stopped
branch kills whole.
Patch `0078` makes `getopts` notice every assignment to OPTIND.
Patch `0079` finishes the byte paths outside Reedline.
Patch `0080` adds `unset`'s readonly handling, `xpg_echo` and `mapfile -C`.
Patch `0081` gives `printf` Bash's argument conversion and errors.
Patch `0082` makes `read` split and assign as Bash does.
Patch `0083` fixes negative subscripts, `{fd}` lifetime, posix special-builtin
assignments, and noninteractive `history`.
Patch `0084` matches `[[ =~ ]]` with the C library's POSIX regular expressions.
Patch `0085` runs `CHLD` traps and implements GLOBIGNORE.
Patch `0086` words common diagnostics as Bash does.
Patch `0087` implements `set -v`.
Patch `0088` runs DEBUG traps before expansion with Bash's `BASH_COMMAND` and `LINENO`.
Patch `0089` fixes small builtin details from the upstream TCK selection.
Patch `0090` supports name references to array elements.
Patch `0091` applies Bash's order to invocation `set` options.
Patch `0092` lets every catchable signal be trapped and handles SIGHUP at the prompt.
Patch `0093` decodes prompts as Bash does.
Patch `0094` makes `compgen` keep the word's spelling.
Patch `0095` keeps `{name}` redirections off inherited descriptors.
Patch `0096` keeps an `INT` recorded during a foreground wait when the child is reaped first.
Patch `0097` adds the `cloud { LIST; }` placement block (covered in `smoke.rs`).

This directory contains only the runtime crate source, manifests, licenses,
and build metadata needed by marsh. Upstream integration tests, snapshots,
examples, benches, and development tooling are intentionally omitted. Each
local delta is covered by focused tests in `crates/marsh/tests/smoke.rs` or,
for `0010`, `0012`-`0016`, `0018`-`0032`, `0036`, `0038`, `0039`, `0042`,
`0052`, `0053`, `0055`-`0067`, and `0070`-`0095`,
`crates/marsh/tests/bash_audit.rs` (`0017`, `0036`, `0037`, `0040`, `0041`, `0043`, `0044`, `0054`, `0068`, and `0069` are in `smoke.rs`, and `0070` also is; `0033` and `0034` are in `split_join.rs` and `bash_audit.rs`, with
parser cases in `brush-parser/src/parser/tests/composition.rs`). The earlier patch series was
also checked against the full upstream compatibility suite
before this source-only vendor snapshot was produced.

Patches `0005a` (composition and runtime snapshot) and `0008a` (original job
PID/status retention) are historical prerequisites applied after `0005` and
`0008` respectively. To verify or refresh the pin, check out the exact
upstream revision, apply the patch filenames in sorted order with
`git apply --exclude=MARSH-VENDORING.md --exclude=README.md`, and copy each
crate's `Cargo.toml`, `LICENSE`, `build.rs`/`about.*` (if present) and `src/`,
plus the root `Cargo.toml`, `clippy.toml`, `rustfmt.toml`, `LICENSE`,
`README.md`, `.gitignore` and `.cargo/config.toml`. Do not carry product scheduling, daemon, or sandbox code into
Brush.
