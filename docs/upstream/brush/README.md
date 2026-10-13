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
  split returns the `0036` interrupt flow. Covered in `smoke.rs`
  (`failed_split_reports_a_status_for_every_stage`: setup failure;
  cancellation shares the code path).
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

- `0050` records a caught `INT`/`TERM` in the signal handler itself (a flag set
  through `signal-hook`, installed before the runtime's own handler) instead of
  reading it from a persistent Tokio signal stream. A stream only learns of a
  signal when the runtime's driver next dispatches it, so a check made between
  delivery and dispatch (`deliver_pending_signals` after the foreground child
  that the same group `INT` killed was reaped; an `INT` the shell sends itself)
  missed the arrival and ran the next command before the shell died by the
  signal. Bash records the arrival in its handler, so the window is closed
  there. Generic; the only new dependency edge is `signal-hook`, already in the
  lock file. Covered by `noninteractive_self_sigint_is_acted_on_before_the_next_command`
  (about one run in eight failed before) and by
  `noninteractive_group_sigint_ends_the_command_list`, which flaked in CI.

- `0051` makes a forked child prove it can start a thread before it runs
  anything. A starting thread holds the standard library's process-wide
  thread registry lock (its stack overflow handler's, on Darwin); a `fork`
  landing in that window hands the child the lock held, with no thread left to
  release it, and the child's first thread spawn (the host of its fresh
  runtime) never returns, hanging the whole command. Brush made that likely:
  `fanout` starts three capture threads per branch and forks straight after,
  a pipeline starts a pool thread for a leading builtin before forking the next
  stage, and a child's own runtime is still starting workers when its body
  forks. Two parts. The Brush-owned thread starts (`spawn_running`) wait for
  the thread to be past its start, removing the systematic collisions without
  a delay. `fork` then waits for the child to claim its start in a word of
  shared memory (compare-and-swap, so exactly one side decides); a child that
  has not claimed it within a bounded time has run nothing, so it is killed
  and replaced (a bounded number of times, then an error, never a hang). Not
  Bash-visible. Covered by `forked_branch_shells_always_start_when_launched_concurrently`
  (96 concurrent two-branch fanouts; 5 of 5 runs hung before) and by the
  `fanout` smoke tests, which hung about 1.7% of runs.

- `0052` keeps `--` among `kill`'s operands, so `kill -- -PGID` with no signal
  sends `TERM` to the process group, as in Bash (clap consumed `--` and the
  operand was read as a signal: "invalid signal specification"). Covered by
  `kill_double_dash_negative_operand_terminates_the_group`.
- `0053` makes a signal sent to a command the moment it is launched reach it.
  A forked shell runs its in-place `exec` (the sole external command of
  `cmd &`, `( cmd )`, a pipeline stage) on its main thread: Darwin drops a
  signal, even `SIGKILL`, sent while a secondary thread of a multithreaded
  process execs, so `sleep 3 & kill $!; wait $!` exited 0 about one run in
  seven on macOS (Bash: 143 always). The request reaches the main thread over
  a pipe (a forked child must not park threads on synchronization state
  copied from its parent; Darwin traps). And every signal stays blocked from
  `fork` until the child has given its caught signals their default actions
  (an external command's child just before `exec`, a forked shell once it has
  set its dispositions), so one landing in that window no longer runs the
  shell's handler in the child. Covered by
  `a_signal_sent_right_after_launch_reaches_the_command` (200 launches each
  of `TERM` and `KILL`).
- `0054` delivers a typed line feed to the line editor as a carriage return
  (`INLCR` while reedline reads). reedline stops reading a batch of queued
  input at Enter (CR) and keeps the rest for the next prompt, but a raw LF is
  Ctrl-J, which also submits the line, so every later line typed ahead in the
  same batch (a pty driver, `expect`, a paste without bracketed paste) was
  discarded. Bracketed pastes keep their newlines (reedline turns CR back into
  LF there). Covered by `lines_typed_ahead_with_line_feeds_all_run` (smoke).
- `0055` imports inherited environment entries that are not UTF-8 instead of
  panicking at startup: the shell sees the value as text (U+FFFD for invalid
  bytes, see `docs/design/bash-compatibility.md`, BYTES), while external
  commands receive the original bytes until the shell assigns or unsets the
  variable; entries whose names are not UTF-8 pass through unchanged. Covered
  by `inherited_non_utf8_environment_reaches_commands_unchanged`.
- `0056` evaluates `[[ s =~ "quoted" ]]` (a pattern that starts with a quote)
  as the regex it is, so a match sets `BASH_REMATCH` and unquoted parts after
  the quote are regex syntax; Brush tested for a substring and left
  `BASH_REMATCH` empty. Covered by `quoted_regex_patterns_set_bash_rematch`.
- `0057` matches `[[ == ]]` and `[[ != ]]` patterns as extended globs whether
  or not `extglob` is set, as Bash does. Covered by
  `conditional_patterns_are_extended_globs_without_extglob`.
- `0058` parses `${!:-word}` and `${!-word}` as operators on `$!`; `!` is
  indirection only before a parameter. Brush printed the text literally.
  Covered by `last_background_pid_takes_parameter_operators`.
- `0059` lets `read -u` name any descriptor (it took only 0-255). Covered by
  `read_takes_descriptors_above_255`.
- `0060` splits a line for `read NAME...` as Bash's `read` does: each name
  but the last takes a word and the delimiter after it (IFS whitespace around
  at most one other IFS character), and the last takes the rest less trailing
  IFS whitespace, or a lone remaining word without its delimiter
  (`IFS=, read a b <<< 'a,b,'` sets `b` to `b`; Brush gave `b,`). Covered by
  `read_splits_fields_for_names_like_bash`.
- `0061` formats a prompt's `\D{...}` one conversion at a time and shows one
  the formatter does not know as written; an unknown conversion panicked and
  ended the shell. Covered by
  `unknown_prompt_date_conversion_does_not_end_the_shell`.
- `0062` escapes the working directory (`\w`, `\W`) and shell name (`\s`)
  before a prompt's parameter expansion, as Bash does, so a `$(...)` or
  backquote in a directory name is not run (and, in PS4, did not recurse
  forever). Covered by `prompt_directory_names_are_not_expanded`.
- `0063` expands PS4 with `xtrace` off, as Bash does: tracing the commands of
  a `$(...)` in PS4 expanded PS4 again without end. Covered by
  `trace_prompt_command_substitutions_are_not_traced`.
- `0064` field-splits only the results of expansions, as Bash does. Unquoted
  text written in the word (`x:y` with `IFS=:`) is a new `Literal` piece: a
  pattern, never split; the text in an operator's word (`${u:-a:b}`) is part
  of the expansion's result and still splits. Splitting itself now follows
  Bash: a non-whitespace IFS character with the IFS whitespace around it ends
  a field, so `:a::b:` is `""`, `a`, `""`, `b` (Brush dropped empty fields).
  Brace expansion yields separate words that expand on their own, so
  `{a:b,c:d}` is two fields whatever IFS is and an empty alternative
  vanishes; Brush joined the words with spaces and relied on splitting.
  Covered by `field_splitting_splits_only_expansion_results`.
- `0065` does not brace-expand an assignment's value (`v={a,b}` is
  `{a,b}`). Covered by `assignment_values_are_not_brace_expanded`.
- `0066` joins `"$*"` and `${a[*]}` with nothing when IFS is set and empty
  (Brush used a space). Covered by `star_joins_with_nothing_under_an_empty_ifs`.
- `0067` lets the smallest prefix or suffix a pattern matches be empty:
  `${v#*}` and `${v%?(a)}` remove nothing (Brush removed one character).
  Covered by `smallest_pattern_removal_can_remove_nothing`.
- `0068` makes a forked child's wait ask the kernel (`waitpid` with
  `WNOHANG`) on every poll. The interactive prompt checks each job by
  polling its wait once; that wait re-checked only after the runtime had
  relayed a `SIGCHLD`, so a job that had already exited was still listed as
  running whenever the relay was not done, and its `[1]+  Done` line missed
  the prompt (`interactive_job_launch_and_completion_lines_match_bash` failed
  that way at 1.5 CPUs). Covered by
  `an_exited_job_is_reported_at_the_next_prompt_without_sigchld` (smoke,
  Linux): the shell starts with `SIGCHLD` blocked, so the relay never comes.
- `0069` keeps what was typed while the shell waited for a cursor position
  report that never came. crossterm parses every byte it reads during the
  wait and holds the keys that are not the report; the plain line reader that
  takes over for the session read the terminal itself, so those lines were
  lost. The keys (characters, Tab, Enter) are now taken from crossterm's
  queue, echoed, and read first. Covered by
  `interactive_shell_survives_a_terminal_that_never_reports_the_cursor`,
  which types its first line during the wait.
- `0070` implements `disown` (`-a`, `-h`, `-r`, job specs and PIDs, Bash's
  diagnostics with the `NAME: line N:` prefix) and `%name`/`%?name` job
  specs. A job collected by `wait PID` or `wait %N` stays in the table, as
  Bash's notified dead job does, until the next `jobs`, `wait`, or process;
  `wait` without operands keeps the `$!` job if it had already finished
  (until `jobs` reports it `Done`), as Bash's noninteractive shell does;
  a disowned job's status is kept for `wait PID` and the shell still reaps
  it. An interactive login shell with `huponexit` sends SIGHUP to its jobs
  on exit, skipping those marked by `disown -h`. Covered by
  `disown_removes_jobs_as_bash_does` (bash_audit) and
  `huponexit_hangs_up_jobs_except_those_disowned_with_h` (smoke, PTY).
- `0071` implements Bash 5.3 function substitutions, `${ cmd; }` and
  `${| cmd; }`. The tokenizer ends one at a `}` in command position (not
  inside a word or among arguments, counting nested `{ }` groups); the word
  parser finds the same brace through the tokenizer. The command runs in the
  current shell with a local scope (`local`, `return`), its output captured
  in an unlinked temporary file as Bash does; `${| }` yields a local
  `REPLY`. `exit`, `break` and `continue` end the enclosing command
  (`ErrorKind::SubstitutionControlFlow`). Covered by
  `function_substitutions_run_in_the_current_shell`.
- `0071a` runs a TERM trap once per signal. The runtime can dispatch an
  arrival after the shell already ran its trap at a command boundary (from
  the arrival its handler recorded); the idle input read's next wait for TERM
  then ran the trap a second time, about once in a hundred runs, which made
  `idle_term_trap_can_resume_reading_commands` flake. That wait now counts a
  dispatch only if the handler's record of it is still unconsumed (waits that
  run alongside a command keep counting every dispatch, as several may wait
  at once).
- `0071b` lets an embedder run the shell on a single-threaded Tokio runtime
  (`brush_shell::entry::use_current_thread_runtime`), whose signal driver
  then runs only when the shell waits. The test driver turns it on with
  `MARSH_TEST_CURRENT_THREAD_RUNTIME`, which makes `0071a`'s late dispatch
  happen every time: `a_term_trap_delivered_at_a_command_boundary_runs_once`
  (smoke) printed `TAT` on every run without `0071a`.
- `0071c` installs the shell's `INT` recording before it starts an external
  command, and a wait counts an `INT` recorded since then as arriving during
  the wait. An `INT` sent to the group after the fork but before the wait
  began (the first command a shell runs had no handler yet) killed the shell
  by the default action instead of letting it continue when the child did not
  die of it. `noninteractive_group_sigint_ends_the_command_list` (bash_audit)
  covered it intermittently; with a 300 ms pause injected after the spawn it
  fails every time without this patch and passes with it.
- `0072` treats NUL bytes in shell input as Bash does. The script, standard
  input and interactive line readers drop them. A script run by the shell is
  refused as binary (Bash's `check_binary_file`: a NUL on the first line, or
  the first two after `#!`, within 80 bytes) with status 126 and no `EXIT`
  trap; an ENOEXEC file run as a command or by `exec` is checked on its first
  128 bytes in the child before the shell is exec'd on it. `.` and startup
  files follow `_evalfile`, which keeps every second of consecutive NULs and
  stops at the first one left; `.` refuses a file with more than 256. A
  syntax error in a sourced file fails only `.` (it was fatal), and
  `printf -v` stops at a NUL. Covered by `nul_bytes_in_shell_input_follow_bash`.
- `0073` makes shell text byte-preserving with lossless conversions at the
  edges (`brush_core::bytes`). Decoding keeps valid UTF-8 and turns each
  other byte into one character from the end of plane 16 (`U+10FF80` stands
  for `0x80` ... `U+10FFFF` for `0xFF`); valid input already holding one of
  those is decoded byte by byte, so decoding then encoding is the identity on
  every byte string. Decoded: arguments, script, sourced and standard input,
  command and function substitution output, the inherited environment,
  `$'...'`, `echo -e`, `printf -v`, `read` (now byte by byte, so the rest of
  the input stays for the next command, and control characters other than
  NUL are data; `^C`/`^D` are keys only on a terminal), `mapfile`, file
  names. Encoded: external command names, arguments and environment, every
  write through the shell's open files, here-documents, and paths built from
  text (`absolute_path`, PATH search, globs, command names). `${#v}` counts
  bytes. The `0055` raw-environment side table is gone. Covered by
  `non_utf8_bytes_survive_every_boundary`.
- `0074` replaces `${v/pat/rep}` as Bash's `pat_subst`: matches are found
  leftmost-longest (exactly, by trying each end, for extended globs; through
  the regex for plain globs), one character is kept after an empty match,
  only `*` patterns meet empty text (except at the end for `/%`), an empty
  pattern changes nothing, and `patsub_replacement` makes an unquoted `&`
  the match. The replacement is no longer a regex replacement string, so
  `$0` is text. Covered by `pattern_substitution_matches_like_bash`.
- `0075` makes an interactive shell without a terminal behave as Bash's: when
  standard error is not a terminal and `/dev/tty` cannot be opened, it prints
  Bash's two job-control warnings and runs without job control; the minimal
  and basic backends prompt with PS1 and PS2 on standard error for every line
  they read even when input is not a terminal; and `--input-backend reedline`
  without a terminal reads as the minimal backend (Reedline needs one). Covered
  by `interactive_shell_without_a_terminal_prompts_on_standard_error`.
- `0076` expands a simple command's redirections after its words and
  assignments, as Bash does: assignment values expand first, each applied in
  a scope of its own while the next expands, then dropped so redirections do
  not see them (the values are kept quoted, so the command does not expand
  them again). For a command that runs as its own process (not a function or
  builtin; `command NAME` is `NAME`), redirections whose words expand, and
  `{fd}` ones, are set up in a copy of the shell, so their side effects stay
  out of it, as in Bash's forked child; an `exit` there ends only the command.
  A failed redirection reads `NAME: line N: WORD: No such file or directory`.
  Covered by `redirections_expand_after_assignments_and_in_the_child_of_a_process`.
- `0077` gives each `fanout` branch one process group, held by a small anchor
  process (`subshell::GroupAnchor`) for the branch's lifetime. Every command
  and forked shell the branch starts joins it (`RuntimeOptions::process_group`).
  A branch stopped before it finishes (the output limit, an interrupt, an
  error) kills that whole group, so descendants of its commands no longer
  outlive it; a branch that finishes leaves the group alone. The branch awaits
  its capture threads instead of joining them, so a stop also reaches a branch
  whose command has exited while a background descendant still writes to its
  output. Covered by `fanout_output_limit_stops_the_branch_process_tree`.
- `0078` makes `getopts` notice every assignment to OPTIND, even of the
  value it holds: each variable assignment now gets a distinct stamp
  (`ShellVariable::assignment_stamp`), which `getopts` records with OPTIND.
  As in Bash's `sv_optind`, `OPTIND=1` (or less) restarts parsing, and
  another value takes effect once the cluster being parsed is done. Covered
  by `getopts_notices_every_assignment_to_optind`.
- `0079` finishes the byte paths outside Reedline and fixes the builtins those
  cases exposed: script and `BASH_ENV` paths and `bash -- script` (a leading
  `--` ended options only after `-c`), `printf %q`/`%#q` of bytes that are not
  UTF-8 (`$'\\NNN'`), prompt `\\NNN` bytes, prompt `\\w`/`\\W` visible control
  characters (Bash's `sh_strvis`), prompts written by the line readers, `exec`
  of an ENOEXEC file by its full path, `echo` options in order, `read -n 0`, and
  `export` of an expanded `NAME=value` word with Bash's invalid-name and
  readonly diagnostics. Covered by `byte_and_builtin_details_match_bash`.
- `0080` makes `unset` report a readonly name (`unset: NAME: cannot unset:
  readonly variable`) and go on with the other names; makes `echo` follow
  `xpg_echo` (escapes by default, `-E` turns them off, and in posix mode no
  options at all); and implements `mapfile -C`/`-c`: as in Bash's
  `mapfile.def`, every `-c` lines the callback runs as `CALLBACK INDEX 'LINE'`
  before the line is stored, its status ignored, and elements are stored as
  they are read. Covered by `unset_echo_and_mapfile_callback_details_match_bash`.
- `0081` gives `printf` Bash's argument conversion and errors. Each
  conversion's arguments (`*` width and precision, then the value, each at its
  `N$` position or the next) are taken by `printf` itself and handed to uucore
  converted: integers as `strtoimax`/`strtoumax` with base 0, floats checked
  as `strtod`, `'c` as a character code. A bad one reports `ARG: invalid
  number`, uses what was converted, and makes the status 1; out-of-range is
  the C library's ERANGE message. An invalid conversion prints what came
  before it, reports `\`C': invalid format character`, and fails. `%b` is
  Bash's `bexpand` (`\\"` keeps its backslash, `\\c` ends all output,
  `\\x`/`\\u` without digits are reported); a `\\c` in the format is
  literal. Diagnostics go to the builtin's standard error after the output's
  complete lines so far, as Bash's line-buffered output has it. Covered by
  `printf_conversions_and_errors_match_bash`.
- `0082` makes `read` split and assign as Bash does: it notes which characters a
  backslash escaped, and those are never delimiters; one splitter serves names
  and `-a` (IFS whitespace around one other IFS character is one delimiter,
  which `-a` got wrong), and the last name's trailing IFS whitespace goes even
  if escaped. `read -t 0` assigns nothing; `-n`/`-N` take `^D` from a terminal
  as data; a readonly name stops `read` with `NAME: readonly variable`,
  status 2. Covered by `read_splitting_and_assignment_match_bash`.
- `0083` fixes four small differences: a negative subscript of an indexed array
  counts back from one past its highest index, not from its element count;
  `{fd}>file` on a builtin, function or compound command leaves the descriptor
  open in the shell afterwards (not with `varredir_close`; a close stays
  scoped); in posix mode, assignments before a special builtin persist and stay
  exported; and a shell that records no commands still has a history list for
  the `history` builtin (`Shell::history_list_mut`), with `history -r`
  (readline's rules: `#`-digit lines are timestamps only in an interactive
  shell or a file that starts with one) and `-w` keeping every byte. Covered by
  `subscripts_named_descriptors_special_assignments_and_history_match_bash`.

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
