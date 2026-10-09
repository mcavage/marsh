# Brush deltas for marsh

These patches target pinned Brush revision
`1389a8e8af6bec56fdfb9ab60a8c898bc14aa961`. The source under
`vendor/brush/` is that revision with the runtime patches applied in filename order,
and the marsh build compiles it directly. `vendor/brush/UPSTREAM_REVISION` and
`vendor/brush/MARSH-VENDORING.md` record how to reproduce the vendored tree.

- `0001` turns bundled registrations into process-backed builtin shims. Command
  lookup keeps special builtin, function, and ordinary builtin precedence, but
  returns the normal external `StartedProcess`, including the pipeline process
  group, before PATH lookup.
- `0002` retains real process handles for simple background pipelines and adds
  `$!`, `wait PID`, `jobs -p`, and `jobs -l` compatibility coverage. Compound
  `&&` and `||` background lists retain Brush's existing internal-task path.
- `0003` exposes the existing shell entry point with an explicit argument
  vector and gives process shims fixed embedding arguments. marsh uses those
  hooks to remove its own options before Brush and to carry the opaque shell
  session identity into each registered-command child without environment
  mutation.
- `0004` implements Bash-compatible noninteractive startup-file handling:
  unset or empty `BASH_ENV`/`ENV` is skipped, while a nonempty value receives
  basic expansion and is sourced when the resulting file exists.
- `0005` retains SIGINT observed while awaiting a foreground process and
  delivers the registered `INT` trap after reaping that process. This preserves
  ordinary process status while allowing trap control flow such as `exit 130`.
- `0005a` records preexisting composition/parser/runtime-manifest changes that
  were present in the preserved pre-audit vendor snapshot but missing from the
  older queue. It also records rightmost status selection needed by `0007`.
  This prerequisite reconstructs historical source; it introduces no new
  runtime edits in this audit.
- `0006` exports the marsh session command directory and authority context
  through Brush's entry path, including after startup files. This is product
  wiring, not a Bash compatibility change.
- `0007` extends trapped `TERM` delivery through foreground child waits and
  marsh `fanout`, and implements `wait -n -p` over managed jobs. It retains
  completed status through prompt notification and interrupted waits. Generic
  signal and wait portions are candidates for separate upstream proposals;
  `fanout` handling remains marsh-specific.
- `0008` adds `wait -f`, standalone `wait -p`, and idle-input `TERM` trap
  delivery through the minimal input backend. It retains partial input across
  a trapped signal and lets the shell exit without waiting for an orphaned
  stdin worker.
- `0008a` records the preexisting original-job-PID cache and status preservation
  prerequisite expected by `0009`, recovered from that same preserved snapshot.
- `0009` uses the rightmost external pipeline stage for `$!` and retains that
  PID after the job is reaped or a stopped foreground job becomes current.
  It retains the original process group for `fg` separately from `$!`.
  Bash and the embedded Brush driver are compared through a real two-stage
  background pipeline and a terminal-backed `fg` journey in
  `crates/marsh/tests/smoke.rs`.
- `0010` preserves standard descriptor sources when creating external commands,
  closes explicitly absent standard descriptors in the Unix child, and retains
  launch-time `pipefail` for background pipelines. Job status selection uses
  pipeline stage indices so polling and canceled waits cannot change which
  failure wins. `jobs -p` uses the retained group ID; `$!` still uses the
  rightmost PID from `0009`. On non-Unix platforms, unsupported descriptor
  closure fails explicitly rather than substituting a successful null writer.

The former `0011` native-shell/lexical-alias fork (including a vendored
reedline fork) was dropped; `0011` is intentionally unused. `vendor/brush/` is
now the pristine pin plus `0001` through `0010` (with `0005a` and `0008a`) and
`0012` through `0046` applied with
`git apply --exclude=MARSH-VENDORING.md --exclude=README.md` in filename
order. Of the behavior previously attributed to `0011`, small readable slices
were re-ported as `0018` (ENOEXEC fallback), `0022` (read units), and `0023`
(lexical aliases). Native subprocess snapshots, lossless byte values, and the
regex helper are not present; any remaining Bash gaps are `#[ignore]`d with
reasons in `crates/marsh/tests/bash_audit.rs`.

- `0012` gives in-process background lists (`(exit 7) &`, `false &`,
  `a && b &`) a waitable identity above every platform PID limit, so `$!`,
  `jobs -p`, and `wait PID` report the list status; that identity is never
  signaled or used as a process group. A background list's `exit`/`return`
  ends only that list. `wait PID` retains reaped statuses as Bash's `bgpids`
  does, and unknown PIDs or job specs return 127.
- `0013` stops the `collect` stage rule from claiming a prefix of an ordinary
  command, so `collect >/dev/null word` parses as a simple command and resolves
  to a function, builtin, or PATH executable like any other name outside
  `fanout { ... } |`.
- `0014` accepts `+o OPTION`/`+O OPTION` startup options, rewriting only the
  option prefix so command strings and script arguments are untouched.
- `0015` makes `kill` default to `TERM`, treats signal 0 as an existence probe,
  and signals every pid, negative process group, or job operand (after `--`).
- `0016` reads a slash-free script operand absent from the working directory
  from the first regular file on `PATH`, keeping `$0` as given.
- `0017` makes an interactive shell that runs `-c` or a script operand with a
  terminal on stdin acquire the terminal the same way the stdin-reading
  interactive loop already does: ignore `SIGTTOU`, lead its own process group
  when possible, `tcsetpgrp` itself, and restore the previous foreground group
  on exit. Without it, `-i -c '/bin/echo hi'` under a PTY stops the first
  foreground job on `SIGTTOU`. Covered against Bash under `script(1)` in
  `crates/marsh/tests/smoke.rs`.
- `0018` runs files the kernel rejects with `ENOEXEC` with the shell itself, as
  Bash does, rather than the platform launcher's `/bin/sh` retry. The final
  pre-exec step is a raw `execve`; on `ENOEXEC` it execs the interpreter
  registered by `brush_shell::entry::run` (the running executable; embedders
  calling `run_from` may call `install_script_interpreter`) on the file, with
  `--inherit-fd` for every mapped descriptor above 2. `exec` resolves `PATH`
  itself, uses the same fallback, restores the shell's descriptors when the
  exec fails, and (unless `execfail` is set or the shell is interactive) exits
  126/127. An existing file whose interpreter is missing reports 126, not 127.
- `0019` lets a trapped `INT`, like the existing trapped `TERM`, interrupt every
  form of `wait`: the wait returns 128+N, the trap runs, and the awaited child
  is neither killed nor reaped.
- `0020` makes `kill %JOB` signal the job's process group when the job leads
  one, and otherwise (no job control) each of the job's live processes, so the
  rightmost stage of a background pipeline receives the signal as in Bash.
- `0021` gives pipeline stages and command substitutions a non-waitable view
  of the parent's jobs, so `jobs -p | cat` and `$(jobs -p)` list them as Bash
  does; an explicit `( ... )` subshell starts with none. `wait` and `kill`
  never act on the view.
- `0022` executes `-c` strings, scripts, and sourced files as Bash reads them
  when they do not parse as a whole: each read unit (a line, extended until
  its commands are complete) runs before the next is parsed, so commands that
  precede a lexical error execute. Text that parses whole runs as before.
- `0023` expands aliases lexically while reading. `brush-parser` rewrites the
  token stream (`parser/aliases.rs`): an unquoted word in command position
  that names an alias is replaced by the alias value's tokens, so aliases can
  supply reserved words and operators, chain, admit the next word with a
  trailing blank, and never recurse into themselves. Case patterns, `[[ ]]`,
  `(( ))`, redirection targets, and here-documents are not command positions.
  The shell parses alias-aware (uncached) only when `expand_aliases` is set
  and an alias exists; when a command changes that state, later lines are
  parsed again. The former execution-time alias substitution is removed, so
  quoted or expanded names reach functions and function bodies keep the
  aliases of their definition.
- `0024` runs `( ... )` subshells, background lists, pipeline stages (except
  the `lastpipe` stage and composition stages), coprocess bodies, process
  substitutions, and `$( ... )` text that mentions `BASHPID` in forked
  processes, as Bash does (`brush-core/src/subshell.rs`). Other command
  substitutions stay in-process: they cannot observe the difference and are
  much cheaper. The child never touches the parent's multi-threaded Tokio
  runtime: it replaces the runtime's process-global signal socket pair (found
  by `brush_shell::entry` as the unnamed Unix sockets the runtime build
  opened), closes inherited close-on-exec pipes its shell does not reference,
  resets `INT`/`QUIT`/`TERM`/`PIPE` (keeping `trap ''` ignores; `INT`/`QUIT`
  ignored in asynchronous lists without job control, and the runtime handler
  reinstalled if the child traps the signal), joins the process group an
  external command would, runs the parsed AST on a fresh runtime on a new
  thread, runs only an `EXIT` trap it set itself, and leaves with `_exit`. A
  stage or subshell that is a single external simple command replaces the
  child (`exec`), so its PID is `$BASHPID` and `$!`. `$$` is the top-level
  shell's PID; `$BASHPID` is dynamic; `$BASH_SUBSHELL` increments for
  `( )`, `&`, coprocesses and substitutions but not for pipeline stages.
  `exec` replaces a forked subshell. `0012`'s synthetic background identity
  and the in-process paths remain only as fallbacks where forking is
  unavailable (non-Unix, or an embedder that did not record the runtime's
  descriptors). The ENOEXEC fallback passes `--closed-fd=N` for closed
  standard descriptors, which the Rust runtime would otherwise reopen on
  `/dev/null`.
- `0025` resolves name references (`declare -n`, `local -n`): reads,
  assignments (scalar, array element, `+=`, compound), `read`, `printf -v`,
  `mapfile`, arithmetic and `unset` act on the named variable, following
  chains up to eight deep. `declare -n`/`+n`, `declare -p`, `[[ -R ]]` and
  `unset -n` act on the reference itself. `${!ref}` on a reference and
  references to array elements (`declare -n r='a[1]'`) are not implemented.
- `0026` implements `shopt -s globstar`: a `**` path component matches zero or
  more directories (a final `**` also matches files; a trailing `**/` only
  directories), honors `dotglob`, does not follow directory symlinks, and
  sorts the combined result.
- `0027` opens an arithmetic command only for adjacent `((`, so `( (cmd) )`
  and `( ( cmd ) )` parse as nested subshells as in Bash.
- `0028` parses `{name}` immediately before a redirection operator
  (`exec {fd}>file`, `{fd}<&-`) as `ast::IoRedirect::NamedFd`. The shell
  allocates the lowest free shell descriptor from 10 and stores it in the
  variable; a close (`{name}>&-`) uses the descriptor the variable holds.
- `0029` implements `printf '%[flags][width](strftime)T'`: each time
  conversion becomes `%s` over the argument rendered with chrono's
  strftime in local time (empty, missing, `-1` and `-2` are now), preserving
  argument reuse across format cycles. The process `TZ` applies; a `TZ`
  assigned only inside the shell does not.
- `0030` uses an associative array element's subscript text, not its
  arithmetic value, as the key in arithmetic reads and assignments, so
  `((count[$word]++))` and `let 'm[k]+=1'` update `count[word]`/`m[k]` rather
  than element `0`. Quoted subscripts with blanks inside `(( ))` still do not
  parse.
- `0031` treats `POLLNVAL` from the readiness poll as "let the read decide":
  Darwin's `poll` reports it for character devices, so `read -t 1 < /dev/null`
  returned the timeout status 142 instead of end of file (1).
- `0032` delivers `INT` and `TERM` that arrive while the shell runs builtins
  (`brush-core/src/signals.rs`). Once the runtime's handler is installed (any
  child wait listens for `INT`; waiting with a `TERM` trap, or registering an
  `INT`/`TERM` trap, listens for that signal), the signal no longer has its
  default effect and waits only observed it while they ran: a builtin loop
  ignored Ctrl-C after the first external command, and a trap set before any
  wait let `TERM` kill the shell. The shell now checks after every pipeline:
  a trapped signal runs its trap (`exit` in it ends the shell); `trap ''`
  ignores it; otherwise a noninteractive shell runs its `EXIT` trap and dies by
  the signal. Each trap invocation consumes the pending arrival, so a wait
  that already delivered it does not deliver it twice; an untrapped `INT`
  during a wait whose job did not die of it (status 130) is discarded, as in
  Bash. Forked children forget the parent's listeners. Remaining
  differences: a blocking builtin read (`read` from a terminal) is not
  interrupted; `kill -TERM $$` is delivered at the next pipeline rather than
  before `kill` returns; and an `EXIT` trap alone does not make `TERM` run it.
- `0033` adds marsh `split { ... } | join` (now `docs/split.md`). The parser
  recognizes `split {` in command position exactly like `fanout {` and parses
  it as `Command::Split(FanoutCommand)`; labels must also be unique without
  regard to case. `join` stays an ordinary simple command: the interpreter
  treats the stage right after a split as the join only when its name is the
  literal unquoted `join` with no prefix (`Command::is_split_join_stage`). The
  parser rejects `|&` on a split or its join, `collect` after `split`, and a
  second fanout/split in the same pipeline. Each branch is a forked shell
  (`fork_child`) in its own process group, with cwd and exported
  `SPLIT_ID`/`SPLIT_LABEL` from the embedding's `SplitPlan`; on Ctrl-C, a
  trapped `TERM`, or the 16 MiB output budget the split sends `SIGINT` to every
  branch group, waits up to 10 s for each branch shell to be reaped and its
  group to empty, then `SIGKILL`s what remains, and reports unconfirmed groups
  to the embedding instead of removing their workspaces. The join command ran
  as an ordinary simple command with `SPLIT_ID`, `SPLIT_DIR`, and
  `SPLIT_MANIFEST` as prefix assignments (replaced by `0034`). The fanout prefix spool is shared
  (`composition_input`); fanout behavior is unchanged. All repository writes
  are the embedding's (`brush_core::split::SplitWorkspace`, installed with
  `brush_shell::bundled::install_split_workspace_hook`); without a hook (for
  example `marsh-local` in Kit jobs) `split` fails with status 2. The parser
  cases in `brush-parser/src/parser/tests/composition.rs` need Brush's unit-test
  dependencies, which the vendored tree omits; they were run as an integration
  test in a scratch copy of `vendor/brush` (`crate::` -> `brush_parser::`).
  Runtime behavior is covered by `crates/marsh/tests/split_join.rs` and the
  coreutils differential in `bash_audit.rs`.
- `0034` makes the split pipeline pure Unix: `join` takes only `--json`,
  `--timing`, `--keep`, and redirections, and a word after them is a status 2
  error that shows `split { ... } | join | CMD`. The stages after `join` (or
  after `split` when `join` is omitted) run as an ordinary pipeline segment
  reading the rendering, with `SPLIT_ID`, `SPLIT_DIR`, and `SPLIT_MANIFEST`
  exported through a command-scope environment that is popped when the
  segment finishes, so functions, builtins, Kit commands, and PATH programs
  all see them. The split directory is released only after that segment
  exits: removed when the last stage exits 0 without `--keep`, otherwise
  kept and reported. Covered by `crates/marsh/tests/split_join.rs`.
- `0035` makes Brush a thin client of the daemon-owned workspaces
  (`docs/design/workspaces.md`). The split stage spools its prefix and hands the
  branch sources (`FanoutBranch::body` rendered as shell text), the shell's
  exported variables, and the spool to the embedding's
  `SplitWorkspace::run`; the marsh daemon snapshots, forks, runs every branch
  as a fresh session shell, and captures. Brush no longer forks branches,
  signals groups, or owns cancellation (`fork_branch`, `run_branches`,
  `cancel_branches` and the capture budget are deleted). It writes the
  returned rendering (or manifest with `--json`) as the join stage's output,
  runs the stages after `join` with `SPLIT_ID`, `SPLIT_DIR` (the `out/`
  directory), `SPLIT_MANIFEST` and `SPLIT_OBJECTS` pushed, and releases the
  split with the last stage's status and `--keep`. `--timing` is gone. The
  provider is `SplitWorkspace::start` returning a `SplitHandle`; the wait runs
  in `spawn_blocking` and Ctrl-C (`await_ctrl_c`) calls its canceller, so an
  interrupted sugar split is cancelled daemon-side and returns 130. Parsing,
  labels, `PIPESTATUS`, and the composition shape checks are unchanged.

- `0036` makes Ctrl-C in an interactive shell abandon the rest of the
  command line, as Bash does. A foreground job that dies of SIGINT (the
  shell, in its own process group under job control, sees no SIGINT itself)
  sets the new `ExecutionControlFlow::Interrupted` on the pipeline result
  unless `INT` is trapped; lists, `&&`/`||`, loops, functions, sourced files
  and the program loop stop on it like `exit`, and the interactive loop
  returns to the prompt with `$?` 130. An interactive session now listens
  for SIGINT from its start (`start_interactive_session`), so Ctrl-C during a
  builtin-only loop is delivered between pipelines as the same interrupt
  instead of killing the shell (no child wait yet) or being ignored. A
  newline ends the echoed `^C` line. An interrupted interactive `fanout`
  returns the same flow. `brush_core::traps::take_pending_interrupt` exposes
  the recorded arrival to input backends. Noninteractive behavior is
  unchanged (`0032` already dies by SIGINT). Covered under `script(1)` PTYs
  against Bash in `smoke.rs` and by a process-group differential in
  `bash_audit.rs`.
- `0037` keeps an interactive shell alive when the terminal never answers
  Reedline's cursor position query (`ESC [ 6 n`; crossterm gives up after
  2 s and `read_line` fails). Reedline (crates.io) is unchanged: the Brush
  backend falls back to plain cooked-mode line reading (prompt,
  continuation prompt, completeness check) for the rest of the session on
  the first timeout before any successful read, or on a second consecutive
  timeout later, and prints one warning. Ctrl-C typed during a plain read is
  reported when the line ends, discarding the line. Covered in `smoke.rs`
  with a PTY that never answers.
- `0038` gives `PIPESTATUS` one entry per stage when a split does not run
  its stages (setup failure 2, cancellation 130): the stages after `join`
  get the same status instead of being omitted. An interactive Ctrl-C'd
  split returns the `0036` interrupt flow. Covered in `bash_audit.rs`
  (setup failure; cancellation shares the code path).
- `0039` reports Bash's line in syntax errors:
  `line N: syntax error near unexpected token `X' (col C)` and
  `line N: syntax error: unexpected end of file`, where an end of file is on
  the line after the last token (input without a final newline still ends
  with one, as in Bash's `-c`). `ParseError::ParsingNear` now carries the
  token text and `ParsingAtEndOfInput` the end position (both are matched in
  `brush-core` read units and `brush-interactive` completeness). Covered by a
  line-number differential in `bash_audit.rs`.
- `0040` describes a process-backed registration (an marsh registered
  command) in `type`, `type -t`, `type -a`, `command -v` and `command -V` as
  the first `PATH` executable that resolves to the registration's
  executable, i.e. its session command-directory link: `NAME is PATH`, `file`.
  Without such a link it remains a builtin. Covered in `smoke.rs`.
- `0041` makes `split { }` and `fanout { }` branches label-led
  (`normalize_fanout_body`): a branch starts after `{`, a top-level newline,
  or `,`, and after a top-level `;` only when the next token is a bare word
  matching `^[A-Za-z_][A-Za-z0-9_-]*:$`. Every other `;` stays Bash's sequence
  operator inside the current branch, so `split { a: cd x; make; b: cd y;
  make }` has two branches and an unlabeled `cmd1; cmd2` is one branch
  (previously two). `a: echo b: c` is unaffected (only the first word after
  `;` counts); a command named like `echo:` right after `;` reads as a label
  and must be quoted. Parser cases (labeled and unlabeled `;`, groups,
  subshells, `&&`/`||`, `case ... ;;`, hyphen/underscore labels, quoted and
  mid-command `x:` words) are in `brush-parser/src/parser/tests/composition.rs`,
  run as an integration test in a scratch copy as for `0033`; a Bash
  differential of each branch's output and status is in `smoke.rs`
  (`fanout_branches_are_label_led_and_bodies_match_bash`).
- `0042` applies Bash's option order to `-c` (`CommandLineArgs::
  gnu_command_order`). In Bash `-c` is a flag and the command string is the
  first operand after *all* options, so `bash -c -l CMD`, `bash -cl CMD`,
  `bash -ce CMD`, `bash -c -o pipefail CMD`, and `bash -c -l -- CMD` run
  `CMD`; clap read the next argument as `-c`'s value (`bash -c -l 'echo x'`
  failed with "a value is required for '-c <COMMAND>'", and `-ce` took `e`
  as the command). The scan stops at the first operand, `-`, or `--`, and a
  value-taking short option (`-o`, `-O`) ends its cluster; the `-c` flag
  then moves to just before the operand. `+o`/`+O` after `-c` are options
  too. One known leniency: Bash rejects a long option after a short one
  (`bash -c --login CMD`); Brush accepts it. Covered by a Bash differential
  of thirteen orders in `bash_audit.rs`
  (`command_flag_takes_the_first_operand_in_bash_option_order`) and in a
  job by `processes_uat.py` P26.
- `0043` prints Bash's interactive job-control lines. The launch line is
  `[N] PID` (Brush printed `[N]+<TAB>PID`); a completion notification is
  `[N]A  STATE<pad>CMD` with Bash's 27-column state field, `Done` for status
  0 and `Exit N` otherwise; `jobs` lists running jobs with a trailing ` &`.
  Only one job is `+` and one `-` (Brush left every older job `-`), and
  after jobs leave the table the previous job becomes current. A new job is
  numbered one past the highest job still listed, so `sleep 1 &` after a
  reported job is `[1]` again; a reported status that shared the reused
  number stays reachable by `wait PID`. Divergence: a job killed by a signal
  reports `Exit 128+N` because the poll result carries no signal; Bash names
  the signal. Covered by a PTY differential against Bash in `smoke.rs`
  (`interactive_job_launch_and_completion_lines_match_bash`).
- `0044` keeps the terminal's output processing on while reedline edits a
  line. reedline enters crossterm's raw mode (`cfmakeraw`), which clears
  `OPOST`/`ONLCR`, so a background job writing `a\nb\n` while the prompt is
  up stair-stepped (LF without CR). GNU readline leaves `c_oflag` alone. The
  backend now enters crossterm's raw mode itself just before each
  `read_line`, puts the original `c_oflag` back, and lets reedline's own
  `enable_raw_mode` be the no-op it is when raw mode is already set;
  reedline's `disable_raw_mode` restores the full original termios as
  before. reedline paints with explicit `\r\n`, so its output is
  unchanged. Covered by a PTY differential against Bash in `smoke.rs`
  (`background_output_at_the_prompt_keeps_terminal_newline_translation`).

- `0045` makes the `collect` renderer public (`BranchResult`,
  `CollectOptions`, `render_collected`, `render_collected_stderr`) so the
  `marsh fanout | marsh collect` CLI prints exactly what the sugar prints.
  The sugar's stderr rendering moves into `render_collected_stderr`; output
  is unchanged. marsh-specific; covered by the P08/P38 frame checks in
  `tests/acceptance/processes_uat.py` and `fanout_cli.rs` tests.

- `0046` renders a branch's stderr inline in `collect`'s stdout, right after
  that branch's header and stdout, and only for a failed branch (the `join`
  rule); `collect --stderr` shows successful branches' stderr too. Before,
  all stderr went to `collect`'s stderr ahead of the rendering, so headers
  and their stderr could appear out of order. marsh-specific; covered by
  P38 in `tests/acceptance/processes_uat.py`.

- `0048` normalizes a `fanout { }` or `split { }` nested in a branch, right
  after the branch label (`review: fanout { a: x, b: y } | collect`), and
  treats `{` as command position (`{ fanout { ... } | collect; }`). Before,
  the label word ended command position, so the nested block's commas were
  left unnormalized and the line failed with a syntax error (only `;` or
  newline separators worked). Normalizing a normalized body is a no-op.
  marsh-specific; parser cases in `composition.rs`
  (`a_fanout_nested_in_a_branch_is_normalized_after_its_label`), run in a
  scratch copy as for `0033`.

- `0049` makes the TERM wait (`await_term`: idle-input trap delivery and the
  `wait` builtins) also take a TERM recorded before its listener existed. The
  listener it creates sees signals only from its creation on, so a TERM that
  landed while the previous command finished, just before the shell began
  waiting for input, was recorded but not delivered until the next command
  line arrived (Bash runs the trap at once). Generic; covered by
  `idle_term_trap_can_resume_reading_commands`, which flaked on a loaded
  machine.

The `0004` delta is also covered through the embedded marsh caller: an empty
`BASH_ENV` executes the requested command, and a nonempty `$HOME`-expanded
path is sourced before it.

The `0005` delta is covered through the embedded marsh caller with a real
foreground process group: SIGINT interrupts the child, invokes the registered
trap, preserves its output, and exits with status 130.

The vendored directory is a runtime-source snapshot, so it omits upstream's
test harness and development corpus. The maintained regression gate for these
first nine deltas is `crates/marsh/tests/smoke.rs`; the full upstream suite is run
in an isolated checkout when refreshing the pin. Patch `0010` has a separate
real-caller differential gate:

```sh
cargo test -p marsh --test bash_audit -- --test-threads=1
```

The original `0010` gate compared six GNU Bash differentials through the
embedded Brush driver. That historical Linux ARM64 result did not cover the
broader shell behaviors added below.

Patches `0024`-`0032` were also checked on Linux arm64 (the guest platform:
procfs descriptor discovery, epoll, pidfd child reaping): `bash_audit` and
`smoke` pass in a `rust:1.95-bookworm` container against GNU Bash 5.3 (the
differentials assume 5.3 semantics; Debian bookworm's 5.2 restores no
descriptors after a failed `exec` under `execfail`) with an init that reaps
orphans (`docker run --init`). A fork stress of 1000 `(true)`, 300 `f | true`
and 200 `sleep 0 &` plus `wait` completes with no zombies or descriptor growth
in about 3x Bash's time (0.64 s vs 0.21 s, release build). With `0036`-`0040`,
`bash_audit` (40) and `smoke` (47; two macOS-only journeys compiled out) pass
there against GNU Bash 5.3.0 built from source, as do all 40 and 49 on macOS
ARM64 with Homebrew GNU Bash 5.3.
