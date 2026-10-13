# Bash compatibility qualification

marsh's shell is a patched Brush: upstream revision `1389a8e` (full hash in
`vendor/brush/UPSTREAM_REVISION`) plus the 97 patches in
[`docs/upstream/brush/`](../upstream/brush/README.md). 81 fix Bash
compatibility: `compgen` words, prompt decoding, signal traps and SIGHUP, invocation option order, name references to array elements, small builtin details (`set`, `$!`, empty `PATH` entries, `local -`, `@K`), DEBUG and ERR traps, `set -v`, common diagnostics, `CHLD` traps, GLOBIGNORE, `[[ =~ ]]` as POSIX extended regular expressions, negative subscripts, `{fd}` lifetime, posix special-builtin assignments, noninteractive `history`, `read` splitting and assignment, `printf` conversions and errors, `unset` of readonly names, `xpg_echo`, `mapfile -C`, `getopts` and `OPTIND`, redirection expansion order, job control, job lines and `disown`, function substitutions, NUL bytes in input, byte strings that are not UTF-8, pattern substitution, interactive shells without a terminal, signals and traps, `wait`, aliases,
namerefs, subshells, globstar, descriptor redirections, `printf` time
formats, startup files, option order, field splitting, `read`, `[[ ]]`
patterns, prompts, and input typed ahead at the prompt. The other 16 add marsh's registered
commands, `split`/`join`, `fanout`/`collect`, and a test hook.

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
| Name references | `local -n`/`declare -n` reads (and references to array elements, `declare -n r='a[i]'`, through `$r`, assignment, `read`, `getopts`, `printf -v`, `export` and `unset`), scalar/array/associative assignments, `+=`, `read`, `printf -v`, `mapfile`, `unset`/`unset -n`, `declare -p` and `[[ -R ]]` |
| Globbing | `globstar` (`**`, `**/`, `dir/**`, with `dotglob`), extglob `+( )`/`!( )` |
| Redirection expansion | A command's words expand first, then its assignments (each seeing the previous one), then its redirections, which do not see the command's variables; an external command's redirections and here-documents expand as in its child (`>"$((++i))"`, `>"${v:=x}"`, `{fd}>file`, `>"${ exit 3; }"` leave the shell alone); failed redirections use Bash's message |
| Interactive shell without a terminal | `-i` with standard error not a terminal and no controlling terminal: `NAME: cannot set terminal process group (-1): Inappropriate ioctl for device` and `NAME: no job control in this shell`, no `m` in `$-`, PS1 and PS2 on standard error for every line read (minimal, basic and Reedline backends; Reedline reads as the minimal backend without a terminal), `exit` at end of input |
| Pattern substitution | `${v/pat/rep}`, `//`, `/#`, `/%` take Bash's leftmost, longest match (extended globs such as `!(a)` and `?(a)` included), keep one character after an empty match, try only a `*` pattern against empty text (and every pattern at the end for `/%`), change nothing for an empty pattern, and with `patsub_replacement` replace an unquoted `&` with the match (`\&` and quoted `&` are literal); `$0` in the replacement is text |
| Bytes that are not UTF-8 | Arguments (and `bash -- script`), `BASH_ENV` and script paths, `-c` and script text, standard input, command substitution, the environment, `$'\NNN'`, `echo -e`, `printf` (format, arguments, `-v`), external arguments and environment, `read` and `mapfile` (byte by byte, leaving the rest of the input unread), here-documents, prompts (`\NNN`, and `\w`/`\W` showing control characters as `^X`, and in a non-UTF-8 locale other bytes as `M-X`), and file names in globs, redirections, `cd` and command names keep every byte; `${#v}` counts bytes; `printf %q` writes them as `$'\NNN'`; `echo` takes `-n`/`-e`/`-E` in order; `read -n 0` reads nothing; `export "$w"` assigns a `NAME=value` word and reports invalid names and readonly variables as Bash does |
| NUL bytes in input | Dropped from scripts, standard input and interactive input; a script whose first line (two after `#!`) has a NUL within 80 bytes is refused (`NAME: PATH: cannot execute binary file`, 126, no `EXIT` trap), within 128 bytes for a file run as a command or by `exec` (`cannot execute binary file: Exec format error`); `.` and startup files remove NULs as Bash's `_evalfile` does (every second of consecutive NULs stays and ends the text) and `.` refuses more than 256; a syntax error in a sourced file fails only `.`; `printf -v` stops at a NUL |
| Function substitutions | Bash 5.3 `${ cmd; }` runs in the current shell (side effects persist, `local` and `return` work, positional parameters are shared, `exit` exits) and yields its output without trailing newlines; `${\| cmd; }` yields a local `REPLY`; the closing brace counts only in command position (`${ echo x}y; }`, `${ echo hi;}x`); nested substitutions, groups, here-documents, double quotes and here-document bodies; xtrace depth |
| `[[ =~ ]]` | Matched with the C library's POSIX extended regular expressions, as Bash does (leftmost-longest alternation, the libc's classes, `nocasematch` as `REG_ICASE`), in the locale `LC_ALL`/`LC_CTYPE`/`LC_COLLATE`/`LANG` name; quoted parts keep only ERE special characters escaped, which xtrace shows (`x\[\.]y`); an invalid regex is status 2 with `[[: invalid regular expression ...` and the libc's reason, leaving `BASH_REMATCH`; `&&`/`\|\|` pass the 2 on |
| Signal traps | Every catchable signal can be trapped (`trap 'cmd' USR1`, `HUP`, `QUIT`, `ALRM`, ...; the trap runs at the next command boundary, after a foreground command) or ignored (`trap '' SIG`); the shell itself ignores SIGQUIT; at an interactive prompt a SIGHUP without a trap saves the history, runs the EXIT trap and ends the shell by the signal, and with a trap runs it (then exits if a line editor was reading); the line the hangup cut short is dropped |
| `CHLD` traps and GLOBIGNORE | A `CHLD` trap runs once for each child the shell reaps (at the next command boundary, never for the trap's own children, not in subshells); a non-empty GLOBIGNORE removes glob results matching its `:`-separated patterns (whole paths, `*` not crossing `/`), and `.`/`..`, and implies dotglob |
| Diagnostics | `NAME: command not found`; `${v:?msg}` as `v: msg` (`parameter null or not set`, `parameter not set`); `local` outside a function, invalid identifiers (`` `bad-name=1' ``, `` `e[0]' ``), `declare +a` on an array, `export -f` of a missing function, `read -u` of a closed descriptor, and `help` of unknown topics (only when none matches) as Bash words them |
| Prompt decoding | As Bash's `decode_prompt_string`: `\NNN` takes exactly three octal digits (a byte; `\000` shows nothing) and feeds the expansion pass (`\044x` expands `$x`); every other escape's text is protected from it (a `$(...)` in `\w`, a `$x` from `\D{}`); `\\` and unknown escapes pass on as written; `\[`/`\]` are markers only with line editing, and only PS1/PS2 drop them; the expansion pass keeps quotes, braces, `~` and globs literal; `\D{fmt}` is the C library's `strftime` |
| `compgen` | `compgen -f`/`-d` keep the word's spelling: names in its directory part (a literal directory of that name first, else dequoted with `~` and parameters expanded, never a command substitution) that start with the rest as written, shown after the directory part as written (`~/item1`, `$HOME/x`, `sub\ dir/child`); `compgen -A command` lists aliases and leaves out directories and disabled builtins |
| `set -v` | Each input line is written to standard error as it is read, before the commands it completes run (scripts, `-c`, standard input, `.`, `eval` and trap strings; comments and trailing lines too); lines read before `set -v` took effect are not |
| DEBUG and ERR traps | A DEBUG trap runs before a simple command's words are expanded and its redirections set up (so it writes where the shell does), with `BASH_COMMAND` the command as written (`printf '[%s]\n' "a b" > /dev/null`), never re-entered for its own commands; `LINENO` in DEBUG and ERR traps is the line of the command they run for |
| Builtin details | `set` leaves empty values unquoted; `$!` is unset (so `set -u` reports it) until a background job starts; an empty `PATH` entry is `.` (`./prog` in `type`, `command -v`, `hash`); `declare 'b[]'` is an invalid identifier; `shopt -o` pads names to 15 columns; `local -` restores the `set` options when the function returns; `exec` does not set `_`; `${a[@]@K}`/`@k` list subscripts and values; `unbound variable` messages |
| Invocation options | `set` options take effect left to right as in Bash (`-e +e`, `-o errexit +e`, `+u -o nounset`), with every `set` letter and its `+` form (`-a`, `-B`, `-E`, `-T`, `-h`, `+x`, ...), `-o`/`+o`/`-O`/`+O` names inside clusters (`-oe errexit`), `+c`/`+s` as `-c`/`-s`, a repeated `-c`, `-i`/`+i`, a lone `-` ending the options and a lone `+` ignored; unknown `-o`/`-O` names and `-c` without a command are Bash's errors (status 2) |
| Common idioms | Arrays and slices (a negative subscript counts back from the highest index), `{fd}>file` on a builtin or function leaving the descriptor open (unless `varredir_close`), posix-mode assignments before a special builtin persisting, `history` in a noninteractive shell (`-r`, `-s`, `-w`, bytes kept), `printf %q`, `printf %b` (Bash's escapes; `\c` ends all output), `printf` numeric arguments converted as `strtoimax`/`strtod` with Bash's `invalid number` and `invalid format character` errors (status 1), and diagnostics after the output's complete lines, `read -r -a`, `read -d ''`, `read -t` end of file versus timeout, `read` field splitting (a backslash-escaped IFS character is data; `IFS=': '` around one `:` is one delimiter), `read -t 0` assigning nothing, `read -n 1` taking `^D` from a terminal as data, `read` stopping at a readonly name (status 2), `printf '%(fmt)T'`, `((count[$key]++))`, `mapfile`, `getopts` (and `OPTIND=1` restarting it), `${x//a/b}`, `${x^^}`, `${x@Q}`, `${!prefix@}`, `BASH_REMATCH`, `pipefail` with `||`, here-strings, `<( )`, brace sequences, nested `( ( ) )` |
| Job ownership | Real-PID `$!` for background lists, `jobs -p`/`-l`, inherited `jobs` metadata in pipelines and substitutions, cleared explicit/background subshell jobs, saved PID statuses and missing-child status 127; `disown` (`-a`, `-h`, `-r`, `%N`, `%name`, `%?name`, PIDs) with Bash's diagnostics, and jobs collected by `wait` kept listed until the next `jobs`, `wait` or process; `huponexit` hangs up jobs not marked by `disown -h` |
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

The current corpus has 79 differential test functions (all enabled), a split
`PIPESTATUS` check,
and 55 smoke journeys (53 on Linux, where two macOS `script(1)` job-control
journeys are compiled out; 54 on macOS, without one that reads `/proc`), run on macOS ARM64 with Rust
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
- `break` or `continue` inside a function substitution ends the enclosing
  command at once; Bash finishes that command first.
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

## Differential suites

`tests/acceptance/bash_suites.py` runs the VM-free differential suites
(`tests/acceptance/bash_*.py`) against GNU Bash 5.3.0, the release CI builds
(`make test-harness` runs it on Linux; macOS cannot create the suites'
non-UTF-8 file names). Each suite drives the built `marsh-local` and GNU Bash
through their command lines, stdin, scripts and real PTYs, compares stdout,
stderr, status, file effects, terminal-visible state (`READLINE_LINE`/`POINT`,
history files, callback counts) and cleanup case by case, and writes a verdict
per case. The run fails on any mismatch not listed in
`tests/acceptance/bash_divergences.json`, on a listed case that now matches
(stale), on a case where GNU Bash fails the case's own expectation, and on any
harness error. The IDs in that file are the ones below; each lists its cases
by exact id.

PTY cases wait for observable conditions, never for time: a prompt or file,
or the shell being idle (no unread typed input, no output arriving, and every
thread of every process in its session blocked in a system call without a
timeout, read from `/proc`). A shell that goes idle without reaching the
awaited state fails at once; one generous 30-second budget per session bounds
the rest. The whole run takes about a minute at 2 CPUs. Cases whose bytes are
all ASCII run in the C locale only; GNU Bash 5.3.0 crashes on a few
invalid-UTF-8 PS4 traces in a UTF-8 locale, and those run in the C locale
only too.

## Classified differential divergences

Each entry is an observed difference from GNU Bash 5.3.0, reproduced by the
listed suite cases. None is waived silently: BYTES and NUL are known bugs
(data is rejected or changed), the rest are unimplemented or different
behavior that needs more than a small Brush patch.

| ID | Difference from Bash | Scope | Reason |
| --- | --- | --- | --- |
| BYTES | Text that is not UTF-8 is held losslessly (patches `0073`, `0079`), except in Reedline: keys typed as bytes that are not UTF-8, editing over them, `READLINE_POINT` and completion of such names | `readline` | Reedline (and crossterm) read and edit text as UTF-8 |
| LOCALE | No locale model: in the C locale `?`, `${v:i:n}`, and `[[:alpha:]]` in globs take a valid multibyte character as one (`${#v}` counts bytes in every locale), IFS characters are whole characters, case conversion changes non-ASCII letters, and `\u00e9` prints UTF-8; in UTF-8 Rust's full Unicode case mapping is used (`ß` upper-cases to `SS`, `ı`/`İ` differ) and `nocasematch` classes differ | Byte matcher and builtin cases with non-ASCII text | Brush text is Unicode regardless of `LC_ALL` |
| EXTGLOB-GNU-OPTIMIZATION | GNU's matcher gives non-POSIX results for `*` followed by a nullable extglob (`[[ b == *@(\|a) ]]` fails, `${v#*@(\|a)}` removes the whole value); Brush matches by the pattern's language | `byte-matchers` `terminal-nullable-*` | Won't fix: a GNU matcher bug (its fast path for a leading `*` skips the empty alternative); the pattern's language is the POSIX meaning |
| STARTUP-OPTIONS | Invalid option letters (`-z`) are clap's error, not Bash's `-z: invalid option` and usage; `-o`, `+o`, `-O`, `+O` without a name do not list the options; `-s` with `-c` is missing from `$-`; `+O extglob` leaves extglob on (Brush parses with it); `--+o=NAME` is accepted | `startup` | Clap parses the rest of the command line |
| INTERACTIVE-WITHOUT-TERMINAL | `-i` without a terminal and with line editing on: Bash's Readline echoes each line it reads from the pipe after the prompt (with `--noediting` the prompts match); input of nothing but NULs | readline `exit-announcement-*`, history-input `only-nul-eof` | Brush's minimal reader does not emulate Readline on a pipe |
| REDIRECT-EXPANSION | `$BASHPID` in an external command's redirection or here-document is the shell's PID | `external-redirects` `here-pid`, `herestring-pid` | Won't fix: Brush sets up such redirections in a copy of the shell, not a forked child, and the child's PID does not exist yet |
| TCK-GAPS | Upstream TCK selection: `builtin declare c=$v` splits; `declare -aA` is accepted; `readonly -f` does not make a function readonly; `${!a[@]:-x}` does not parse; `$BASH` is the invocation name, not the executable's full path; associative arrays list in insertion order, not Bash's hash order (also `${h[@]@K}`); globstar `**/` leaves out directory symlinks; a function's display re-indents lines of a multi-line quoted string | `tck` | Individual Brush gaps |
| DIAGNOSTICS | Bad array subscripts (`${a[-9]}` is silent instead of `a: bad array subscript`; an assignment says `array index out of range`) and syntax errors in sourced files (`unterminated single quote at L,C`) are worded as Brush words them | `tck`, `nul-input` | Messages from Brush's array and parser errors |
| JOBS | Job-control builtins: `jobs -n` and `jobs SPEC` are unimplemented, `jobs -l` lists a pipeline on one line, a stopped job's status is 148 whatever the signal, finished jobs are listed `Running` until waited, signal names (`Terminated`) are not shown, `(wd: ...)` is missing, `bg` is unimplemented, `fg`/`bg` option errors | `jobs` | Brush's job table is partial |
| PROMPT | In a prompt's expansion pass, quotes inside a `${v:-'w'}` word are removed; Bash keeps them, as in a here-document | `prompt` | Brush's here-document expansion of operator words |
| READ | `read -t` timing out right after a backslash stores Bash's internal quoting marker (`\x01`, CTLESC) in the variable; Brush stores the backslash | `byte-builtins` | A Bash artifact (its quoted input leaks on that path); not reproduced |
| READLINE-VARIABLES | `bind -x`: `READLINE_POINT` counts characters, not bytes; out-of-range and invalid points are handled differently; unsetting `READLINE_LINE`/`POINT` in the command, their values before and after a binding, and the line seen during a continuation (PS2) read differ; PS2 is expanded more often | `readline` | Reedline's editor model, not GNU Readline |
| READLINE-BINDINGS | `bind` macros (`"\C-g":"text"`), `\C-x`-prefixed and hex/octal key spellings, and some raw keys are not supported | `readline` | Reedline keybinding model |
| READLINE-COMPLETION | Programmable completion with `-o filenames`, `fullquote`, `noquote`: quoting of inserted names and when the function runs differ | `readline` | Reedline completion model |
| HISTORY-MULTILINE | `cmdhist`/`lithist` storage of multi-line commands, comments and here documents differs | `readline` | History recording |
| HISTORY-EXPANSION | History expansion (`!!`, `!x`, `:p`) inside double quotes, command substitutions, aliases and pastes, and its error/print-only status, differ | history-input `context-*`, `paste`, `policy` | Partial history expansion |
| HISTVERIFY | `histverify` and `histreedit` are not implemented | history-input `edit` | Not implemented |
| STDIN-READAHEAD | A script read from stdin is read ahead, so `exec 0<file` or `exec 0<&-` inside it does not change what the shell reads next | readline `reader-*` | Buffered input reader |
| HUP | A noninteractive shell with an EXIT trap dies by SIGHUP without running it (Bash catches the fatal signals once an EXIT trap is set); a trapped HUP that arrives while a script, or an `-i` shell without line editing, is reading a line runs after that line instead of at once | `history-input` | Signals are delivered at command boundaries |
| EOF-INCOMPLETE | End of input in the middle of a line of an interactive shell on a pipe: PROMPT_COMMAND and PS2 run a different number of times, and unterminated quotes are reported differently | history-input `eof` | Reader and diagnostics |

## Run focused comparisons

Prerequisites: Rust 1.95.0, GNU Bash 5.3 on `PATH` (5.3.0 for the suites), a C
linker and ordinary Unix utilities. No Docker, SBX, credentials or Cloud
resources are needed.

```sh
cargo test --locked -p marsh --test bash_audit --test smoke
cargo clippy --locked -p marsh --all-targets -- -D warnings
cargo build --locked -p marsh --bins   # then, on Linux:
python3 tests/acceptance/bash_suites.py --candidate target/debug/marsh-local \
    --oracle "$(command -v bash)" --evidence "$(mktemp -d)"
```

The [upstream patch record](../upstream/brush/README.md) identifies inherited
architecture versus product routing and parser changes. An inherited gap is
not a waiver of the product promise. Record the oracle version, source state
and built artifact hash with reproductions.
