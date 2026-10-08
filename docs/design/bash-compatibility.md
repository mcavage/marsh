# Bash compatibility qualification

marsh's shell is a patched Brush: upstream revision `1389a8e` (full hash in
`vendor/brush/UPSTREAM_REVISION`) plus the 49 patches in
[`docs/upstream/brush/`](../upstream/brush/README.md). 34 fix Bash
compatibility: job control and job lines, signals and traps, `wait`, aliases,
namerefs, subshells, globstar, descriptor redirections, `printf` time
formats, startup files, and option order. The other 15 add marsh's registered
commands, `split`/`join`, and `fanout`/`collect`.

Bash is the test oracle. It is never a fallback and never the implementation.
The goal is ordinary Bash behavior, not complete GNU Bash conformance.

## Repaired and exercised behavior

The focused executable corpus covers these regressions:

| Boundary | Observable comparison |
| --- | --- |
| Read units and lexical errors | Earlier complete commands, alias changes and here documents execute before a later lexical error; same-line/incomplete groups stay unexecuted and earlier exit controls flow |
| Aliases | Escaped/quoted/dynamically expanded command names, operator bodies, chains, trailing blanks, recursion, function-definition timing, same-line read timing, reserved syntax and here documents |
| Shell state isolation | Subshells, background lists, command substitutions and pipeline stages keep their variable/function/option/umask/directory changes out of the parent |
| Process identity | `( )`, `&` lists, pipeline stages, coprocesses and process substitutions are forked processes: `$BASHPID` is the child (and `$!`/`$NAME_PID`), `$$` is unchanged, `$BASH_SUBSHELL` increments as in Bash, a single external command replaces the child, `exec` in a subshell keeps its PID, function pipeline stages die of `SIGPIPE`, and only an `EXIT` trap set inside the subshell runs there. `$( )` forks only when its text mentions `BASHPID` |
| Name references | `local -n`/`declare -n` reads, scalar/array/associative assignments, `+=`, `read`, `printf -v`, `mapfile`, `unset`/`unset -n`, `declare -p` and `[[ -R ]]` |
| Globbing | `globstar` (`**`, `**/`, `dir/**`, with `dotglob`), extglob `+( )`/`!( )` |
| Common idioms | Arrays and slices, `printf %q`, `read -r -a`, `read -d ''`, `read -t` end of file versus timeout, `printf '%(fmt)T'`, `((count[$key]++))`, `mapfile`, `getopts`, `${x//a/b}`, `${x^^}`, `${x@Q}`, `${!prefix@}`, `BASH_REMATCH`, `pipefail` with `||`, here-strings, `<( )`, brace sequences, nested `( ( ) )` |
| Job ownership | Real-PID `$!` for background lists, `jobs -p`/`-l`, inherited `jobs` metadata in pipelines and substitutions, cleared explicit/background subshell jobs, saved PID statuses and missing-child status 127 |
| Interactive job lines (PTY) | The launch line is `[1] 467` (no `+`, no tab); completion notifications are `[1]+  Done<pad>CMD` and `[1]+  Exit 3<pad>CMD` with Bash's 27-column state field; `jobs` lists `[1]-  Running<pad>CMD &` with one `+` and one `-`; job numbers restart at one past the highest job still listed. Divergence: a background job killed by a signal is reported `Exit 143`, where Bash names the signal (`Terminated`) |
| Descriptors and pipelines | `exec {fd}>file` allocation from 10 and `{fd}>&-` closure, ordered redirections, closed stdio, inherited high descriptors and later closure, mixed builtin/function/external stages, launch-time `pipefail`, canceled waits and polling |
| Scripts | Non-executable script operands found through `PATH`, original script `$0`, ENOEXEC (no shebang) through Brush rather than `/bin/sh`, fresh exported environment/functions, positional arguments and `BASH_ENV`. In the product, the guest shell runs such files with a child guest `marsh` attached to the same session |
| `exec` | ENOEXEC through Brush, no private-state descriptor leak into successful external executables, high descriptor remaps, closed stdio, restored runtime descriptors after first/second exec failure, missing-interpreter status 126, `execfail` continuation |
| Signals | Trapped/ignored/untrapped INT and TERM in builtin-only loops after a child wait (trap runs, `trap ''` ignores, otherwise `EXIT` trap then death by the signal); default `kill` is TERM; multiple PID operands, signal-zero probes, negative process groups and whole-job signaling; trapped INT interrupts PID/next/all waits without consuming or killing the child. Interactive Ctrl-C (PTY): a foreground job that dies of an untrapped SIGINT, or Ctrl-C during a builtin-only loop, abandons the rest of the command line (later list items, loop iterations, function bodies), prints a newline after `^C`, and leaves `$?` at 130; noninteractive `-c` given SIGINT on its process group dies by SIGINT unless the child handled it |
| Invocation option order | `-c` is a flag and the command is the first operand after every option, in any order: `bash -c -l CMD`, `-cl`, `-lc`, `--login -c`, `-c -e`, `-ce`, `-ec`, `-c -o pipefail`, `-o pipefail -c`, `-co pipefail`, `-c -l -- CMD`, `-c CMD argv0 args` (`$0`, `$@`), and `-c` with no operand is status 2. Divergence (lenient): Bash refuses a long option after a short one (`bash -c --login CMD`, "invalid option"); Brush runs `CMD` |
| Diagnostics and `type` | Syntax errors name Bash's line (`-c: line 2: syntax error: unexpected end of file`, `line N: syntax error near unexpected token `)'`). `type`/`command -v` describe a registered command as its session `PATH` link (`type -t` is `file`), see below |
| Composition collisions | Ordinary `collect` function/executable arguments survive interleaved redirections with product extensions enabled. Coreutils `split -l` and `join` (direct, from stdin, through a function, `command split`, assignment-prefixed `join`) match Bash with extensions on. Divergences by design: `split {` in command position is the marsh `split` form (coreutils would split a file named `{`), and a bare `join` directly after a `split { }` stage is the marsh join stage, which takes only options (`split { } | join a b` is a status 2 error; `command join` reaches coreutils) |

`crates/marsh/tests/bash_audit.rs` compares separately captured stdout, stderr
and exit status through the shipping `marsh-local` executable. It also exercises
actual host `marsh` registered-command dispatch with a non-UTF-8 argument and
checks failure without daemon authority. The smoke suite retains separate
embedding, composition, terminal and signal journeys. The background compound
list regression is enabled. Tests do not use real credentials or Cloud effects.

One explicit oracle-version divergence is classified: command-substitution
`wait` on a parent PID can print internal status `-1` on GNU Bash 5.2, while
5.3 and marsh print `127`. The test records that oracle variant and requires
marsh's exact `127`; the parent's subsequent wait must still return its child
status. Candidate failures are not normalized away.

The current corpus has 38 differential test functions (all enabled), a split
`PIPESTATUS` check, and 49 smoke journeys (47 on Linux, where two
macOS `script(1)` job-control journeys are compiled out), run on macOS ARM64 with Rust
1.95.0 and GNU Bash 5.3 and in a Linux arm64 `rust:1.95-bookworm` container
with GNU Bash 5.3.0 built from source. A test function may contain several distinct
scripts. These counts describe the selected
corpus, not a GNU Bash test-suite pass. The canceled-wait fixture uses explicit
completion barriers so its expected ordering does not depend on process
startup speed.

A separate 17-case audit compared the exact pinned upstream runtime, GNU
Bash and shipping `marsh-local`: fourteen targeted differences were inherited
Brush gaps, one was an marsh `collect` collision, and ENOEXEC requires Brush
interpreter identity. A plain control agreed across all three. The pristine
build preserved upstream runtime source/default features but used isolated
manifests without development dependencies and an available compatible lock.
It does not qualify the full upstream TCK.

## Known gaps

No differential test is currently `#[ignore]`d. Real-process subshells (patch
`0024`) carry these boundaries:

- A command substitution runs in-process unless its text mentions `BASHPID`,
  so a function called from `$( )` that reads `$BASHPID` sees the parent's PID.
- Forked children start a fresh Tokio runtime; `( : )` costs about 0.9 ms on
  an M-series Mac (GNU Bash about 0.55 ms). In-process `$( )` stays about
  30 µs.
- The Linux guest path (procfs descriptor discovery, inherited epoll) is
  exercised by the packaged acceptance run and by running these tests in a
  Linux arm64 container, not by the macOS host run alone.

Other differences found by the common-idiom differential sample and not yet
repaired (classified, not waived):

- `printf '%(fmt)T'` uses the process `TZ`, not a `TZ` set only inside the
  shell.
- A `RETURN` trap does not fire.
- A caught signal is delivered after the current pipeline, not mid-builtin:
  `kill -TERM $$` runs its trap after the next pipeline rather than before
  `kill` returns, and a blocking `read` from a terminal is not interrupted.
  An `EXIT` trap without a `TERM` trap does not run when `TERM` kills the
  shell (GNU Bash runs it).
- `(( m["a b"]=1 ))` (quoted associative subscript in arithmetic) does not
  parse.
- `set -u` on an unbound variable exits 1 (GNU Bash 5.3: 127); diagnostics
  use Brush's wording throughout.
- `${x~~}` case toggling, `select`, and an unquoted space inside an
  associative subscript assignment (`m[a b]=1`) are not supported.
- `${!ref}` on a name reference does not yield the target name.
- Syntax error wording: the line number matches Bash, but an unexpected end
  of file omits Bash's ``from `if' command on line 1`` and Bash's echo of the
  offending line; some errors Bash reports at a token (`echo (`) are reported
  as an unexpected end of file. Messages keep a `(col N)` suffix.
- Interactive Ctrl-C at a plain-fallback prompt (below) is noticed when the
  line ends, not immediately.

Classified divergences by design:

- A registered command (a Kit command such as `claude`) is resolved before
  `PATH`, but runs as a separate process. `type NAME`, `type -a`, `command -v`
  and `command -V` describe it as the first `PATH` entry that links to the
  marsh executable (the session command directory, which is first on `PATH`):
  `claude is /tmp/marsh-commands-XXXX/claude`, and `type -t claude` prints
  `file`. Scripts that test `type -t NAME = file` or use `command -v NAME` as a
  path therefore work, and that path runs the same command from any
  descendant. Without such a link (embeddings that do not install the
  directory) it is still described as a `shell builtin`.
- A terminal that never answers the cursor position query (`ESC [ 6 n`) that
  the Reedline editor sends before each prompt: the first such timeout (2 s)
  at the session's first prompt, or two in a row later, switch the session to
  plain cooked-mode line reading with a one-line warning, instead of ending
  the shell. Bash's Readline never sends the query. Keys typed before the
  timeout are consumed by the query and lost.

## Qualification boundaries

- `[[ value =~ regex ]]` uses upstream Brush's matcher. The former native
  helper with explicit resource limits was removed with the Brush fork (see
  `upstream/brush/README.md`); expensive expressions are not bounded by marsh.
- Product composition syntax (`fanout`/`collect`, `split`/`join`) is an marsh extension. Its
  parser coexistence is checked by the focused journeys above; arbitrary Bash
  grammar with extensions enabled is not exhaustively qualified.
- This is a focused real-caller corpus, not the complete upstream Brush TCK or
  a complete three-way GNU Bash/pristine Brush/marsh conformance corpus. The
  TCK refresh remains a separate track. Unexercised shell options, process
  substitution lifecycle, interactive backends and coprocess cleanup must not
  be inferred from the covered PID and stream cases.
- Parsing retains original tokens and executes complete read units before
  capturing the next unit's aliases. Successfully tokenized prefixes survive
  later lexical errors. Parser diagnostic wording differs from GNU Bash;
  error journeys compare output/status and require a lexical diagnostic, and
  a separate differential compares the reported line number.
- The product models assume faithful descriptor/signal operations for
  external processes. They do not model Bash grammar, expansion or
  the Brush interpreter internals. Wait cancellation, descriptor ownership and
  script fallback claims above depend on executable evidence;
  a TLC pass does not establish those claims or qualify a shipped candidate.
- Host shell tests do not qualify the packaged Mac/stock-SBX process and PTY
  path. Use the [packaged acceptance contract](../../tests/acceptance/CONTRACT.md)
  for that exact candidate.

## Explicit enumeration and platform differences

External environment arrays have no ordered-export contract. In the concrete
`BASHPID=7 exec /usr/bin/env` caller, GNU5.3.20 emits its internal hash-table
order while Rust's command builder sorts the normal environment. Qualification
retains that raw stdout difference and separately requires exact unique names,
raw byte values, prefix precedence, status, stderr and filesystem effects.
No output normalization turns this caller into a byte-exact pass.

GNU's `read -t 0` readiness probe uses a fixed `fd_set`. At descriptor1024 and
above its observed result varies by platform; on Linux an out-of-range FD_SET
can access beyond that stack object. Brush uses a dynamically sized Darwin
select fallback and poll elsewhere, and does not copy undefined stack access or
add an artificial1024 descriptor limit. Real reads and low/high OS-descriptor
readiness are measured separately. This does not excuse ordinary closed-pipe,
EOF, timeout or invalid-descriptor differences.

GNU Bash 5.3.20 has an uninitialized-prefix divergence when a HUP trap's syntax
error resets the input buffer during a partial, unedited interactive pipe read.
The reproducible `singlequote-backslash`, `continued-singlequote`, and
`one-error` cases (a local probe script, not checked in) produce changing command/history bytes, diagnostics and sometimes exit status
on Linux. The official 5.3 sources plus patches 1–20 explain the retained outer
read index followed by allocation of an uninitialized prefix; the strict
comparison failures were retained as local evidence. Brush uses deterministic cleared-input
resumption, matching the stable Mac GNU controls, and does not reproduce
allocator contents. This classification does not waive the remaining ordinary
input/history differences or turn these Linux rows into byte-exact passes.

## Interactive input qualification

`tests/acceptance/bash_readline_cli.py` exercises the public executable in owned
PTY sessions and ordinary stdin/file/command modes. It records raw command
streams, editable bytes and cursor positions, terminal traffic, exit status,
completion callback counts, kernel-FD observations, and verified cleanup.
GNU Bash 5.3.20 on Linux is the primary reference for this corpus. Mac runs
record their actual GNU version (including 5.3.9); they are not relabeled as a
newer reference or inferred from Linux results.

Reedline is a UTF8 UI, not the shell's value or source representation. Brush
retains byte strings and uses its byte editor when the UI cannot represent a
prompt, buffer, completion, binding or history item. Display-only caret/octal
notation is never substituted into the command buffer. The normal UTF8 editor
and the byte editor have different paint protocols; terminal transcripts are
retained rather than normalized into purported byte-identical screens.
In forced interactive mode on a pipe, the minimal reader echoes complete
physical input lines. GNU Readline can instead horizontally scroll a long line
and emit a carriage return plus `<` and its visible suffix. This concrete
presentation difference is retained in the history-load and interactive
call-stack fixtures; their command stdout/status/files are checked separately.
It is not a reason to ignore missing prompts, missing input echo, extra exit
announcements, or any command-data difference.

Raw keyboard transport probes explicitly configure GNU's `input-meta on` and
`convert-meta off` where needed. C-locale Meta-key interpretation is not the
same thing as preserving an eight-bit input stream. Those fixture settings are
recorded, and do not establish complete inputrc/Readline-option support for the
alternate editor.

The maintained Reedline dependency has an opt-in event handoff that retains the
unprocessed input suffix when a completion or binding yields to the host.
Completion functions must not be rerun merely to cross that boundary. Primary
stdin readers follow the current shell FD0, including persistent `exec`
redirections and closure; named script and `-c` sources remain separate from
stdin. PS2 is expanded on an actual continuation read, not speculatively at
PS1 or on every redisplay. These behaviors require the exact current candidate's
caller evidence, strict checks and independent review; source presence or a GNU
self-run is not a qualification receipt.

Basic-editor feature coverage, screen-cell behavior, advanced keyboard
protocols and history-navigation behavior must be assessed explicitly. They
are not blanket inherited waivers and do not establish complete GNU Readline
or Bash conformance. marsh/Brush identity and help remain truthful; the product
does not claim to be GNU Bash.

## Run focused comparisons

Prerequisites: Rust 1.95.0, GNU Bash 5.x on `PATH`, a C linker and ordinary Unix
utilities. No Docker, SBX, credentials or Cloud resources are needed.

```sh
cargo test --locked -p marsh --test bash_audit --test smoke
cargo clippy --locked -p marsh --all-targets -- -D warnings
```

The [upstream patch record](../upstream/brush/README.md) identifies inherited
architecture versus product routing and parser changes. An inherited gap is
not a waiver of the product promise. Record the oracle version, source state
and built artifact hash with reproductions.
