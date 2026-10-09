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

This directory contains only the runtime crate source, manifests, licenses,
and build metadata needed by marsh. Upstream integration tests, snapshots,
examples, benches, and development tooling are intentionally omitted. Each
local delta is covered by focused tests in `crates/marsh/tests/smoke.rs` or,
for `0010`, `0012`-`0016`, `0018`-`0032`, `0036`, `0038`, `0039`, and `0042`,
`crates/marsh/tests/bash_audit.rs` (`0017`, `0036`, `0037`, `0040`, `0041`, `0043`, and `0044` are in `smoke.rs`; `0033` and `0034` are in `split_join.rs` and `bash_audit.rs`, with
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
