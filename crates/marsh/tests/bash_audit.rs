//! Bounded Bash differentials through the embedded shell's executable entry point.
#![cfg(unix)]

mod common;

use std::io::Write as _;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

/// `program` is `bash` (the GNU Bash 5.3 oracle) or a marsh shell binary.
fn shell(program: &str) -> Command {
    if program == "bash" {
        common::require_bash_oracle();
    }
    common::command(program)
}

fn run(program: &str, script: &str, from_stdin: bool) -> Output {
    run_named(program, script, from_stdin, None)
}

/// As [`run`]; with `-c`, `name` is `$0`, which prefixes Bash's diagnostics.
fn run_named(program: &str, script: &str, from_stdin: bool, name: Option<&str>) -> Output {
    common::assert_no_blocking_fifo_open(script);
    let directory = tempfile::tempdir().unwrap();
    let mut command = shell(program);
    command
        .env_remove("BASH_ENV")
        .env_remove("ENV")
        .env_remove("SHELLOPTS")
        .env_remove("BASHOPTS")
        .env_remove("MARSH_TEST_EXTENSIONS")
        .env_remove("MARSH_TEST_EXTERNAL_COMMANDS")
        .env_remove("MARSH_TEST_REGISTER_PLACE")
        .env("LC_ALL", "C")
        .current_dir(directory.path())
        .stdin(Stdio::null());
    if program != "bash" {
        command.arg("--no-config");
    }
    command
        .args(["--noprofile", "--norc"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if from_stdin {
        command.arg("-s").stdin(Stdio::piped());
    } else {
        command.args(["-c", script]).args(name);
    }
    // A shell that misses a signal hangs; this fails the test by name and
    // kills the shell's group instead.
    let mut child = common::spawn_group(&mut command);
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(script.as_bytes()).unwrap();
    }
    common::wait_for_exit(
        child,
        &format!("{program}: {script}"),
        Duration::from_mins(1),
    )
}

/// The oracle and the candidate start from a cleared environment plus an
/// allowlist, never the developer's credentials (`make test` exports a
/// credential-shaped canary to prove it).
#[test]
fn oracle_and_candidate_start_without_credentials() {
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-local")] {
        let names = run(program, "env | cut -d= -f1", false);
        assert!(names.status.success(), "{program}: {names:?}");
        common::assert_no_credential_names(program, &String::from_utf8_lossy(&names.stdout));
    }
}

/// Writes an executable file with `cp`, so this process never holds a
/// descriptor open for writing on it: a child another test thread forks
/// meanwhile would inherit that descriptor until its exec, and running the
/// file then fails with ETXTBSY.
#[cfg(unix)]
fn write_executable(path: impl AsRef<std::path::Path>, contents: impl AsRef<[u8]>) {
    use std::os::unix::fs::PermissionsExt as _;
    let path = path.as_ref();
    let mut staged = path.as_os_str().to_owned();
    staged.push(".staged");
    std::fs::write(&staged, contents).unwrap();
    let copied = Command::new("cp").arg(&staged).arg(path).status().unwrap();
    assert!(copied.success(), "cp {}: {copied:?}", path.display());
    std::fs::remove_file(&staged).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn differential(script: &str, status: i32, stdout: &[u8]) {
    differential_with_input_mode(script, status, stdout, false);
}

fn differential_with_input_mode(script: &str, status: i32, stdout: &[u8], from_stdin: bool) {
    let bash = run("bash", script, from_stdin);
    assert_eq!(bash.status.code(), Some(status), "Bash: {script}: {bash:?}");
    assert_eq!(bash.stdout, stdout, "Bash: {script}: {bash:?}");
    let brush = run(env!("CARGO_BIN_EXE_marsh-local"), script, from_stdin);
    assert_eq!(
        brush.status.code(),
        bash.status.code(),
        "{script}: {brush:?}"
    );
    assert_eq!(brush.stdout, bash.stdout, "{script}: {brush:?}");
    assert_eq!(brush.stderr, bash.stderr, "{script}: {brush:?}");
}

#[test]
fn complete_read_units_execute_before_later_lexical_errors() {
    for (script, status, stdout) in [
        (
            "printf 'earlier\\n'\n'unterminated",
            2,
            b"earlier\n".as_slice(),
        ),
        ("printf earlier; 'unterminated", 2, b"".as_slice()),
        (
            "cat <<EOF\nearlier\nEOF\n'unterminated",
            2,
            b"earlier\n".as_slice(),
        ),
        (
            "printf '%s\\n' 'printf earlier' \"'unterminated\" >broken; source ./broken",
            2,
            b"earlier".as_slice(),
        ),
        (
            "printf earlier\nif true; then\nprintf hidden\n'unterminated",
            2,
            b"earlier".as_slice(),
        ),
        (
            "shopt -s expand_aliases\nalias q='printf alias'\nq\n\"unterminated",
            2,
            b"alias".as_slice(),
        ),
        (
            "printf earlier\nexit 17\n'unterminated",
            17,
            b"earlier".as_slice(),
        ),
    ] {
        for from_stdin in [false, true] {
            let oracle = run("bash", script, from_stdin);
            assert_eq!(oracle.status.code(), Some(status), "{script}: {oracle:?}");
            assert_eq!(oracle.stdout, stdout, "{script}: {oracle:?}");
            let candidate = run(env!("CARGO_BIN_EXE_marsh-local"), script, from_stdin);
            assert_eq!(
                candidate.status.code(),
                oracle.status.code(),
                "{script}: {candidate:?}"
            );
            assert_eq!(candidate.stdout, oracle.stdout, "{script}: {candidate:?}");
            // Parser diagnostics deliberately differ. Both must report an
            // actual lexical error, unless earlier control flow exits first.
            if status == 2 {
                assert!(!oracle.stderr.is_empty(), "{oracle:?}");
                assert!(
                    String::from_utf8_lossy(&candidate.stderr).contains("unterminated"),
                    "{candidate:?}"
                );
            } else {
                assert_eq!(candidate.stderr, oracle.stderr, "{candidate:?}");
            }
        }
    }
}

#[test]
fn external_standard_descriptor_sources_survive_ordered_redirection() {
    differential(
        "sh -c 'echo out; echo err >&2' 2>&1 >/dev/null",
        0,
        b"err\n",
    );
    differential("sh -c 'echo out; echo err >&2' >/dev/null 2>&1", 0, b"");
    differential("sh -c 'echo out; echo err >&2' 1>&2 2>/dev/null", 0, b"");
    differential(
        "sh -c 'echo out; echo err >&2' 3>&1 1>&2 2>&3 3>&-",
        0,
        b"err\n",
    );
    differential(
        "sh -c 'echo out; echo err >&2' 2>&1 | cat",
        0,
        b"out\nerr\n",
    );
    // stdin is a read-only descriptor: duplicating it to stdout must not inherit stdout.
    differential(
        "sh -c 'printf forbidden' 1>&0 2>/dev/null; test $? -ne 0",
        0,
        b"",
    );
}

#[test]
fn closed_standard_descriptors_are_not_inherited_or_dev_null() {
    differential(
        "sh -c 'echo forbidden; test $? -ne 0 || exit 90; exit 37' >&-",
        37,
        b"",
    );
    differential("sh -c 'echo forbidden >&2' 2>&-; test $? -ne 0", 0, b"");
    differential("sh -c 'read value' <&- 2>/dev/null; test $? -ne 0", 0, b"");
    differential("sh -c 'echo retained >&2' 2>&1 1>&-", 0, b"retained\n");
    differential(
        "sh -c 'echo forbidden' >&- 2>/dev/null; echo restored",
        0,
        b"restored\n",
    );
    differential(
        "sh -c 'echo restored' 1>&- 1>out; cat out",
        0,
        b"restored\n",
    );
    differential(
        "exec 1>&-; sh -c 'echo forbidden' 2>/dev/null; test $? -ne 0",
        0,
        b"",
    );
}

#[test]
fn background_pipeline_wait_uses_launch_time_pipefail() {
    for (options, stages, after, expected) in [
        ("set -o pipefail", "7 0", "", 7),
        ("set -o pipefail", "7 0", "set +o pipefail;", 7),
        ("set +o pipefail", "7 0", "set -o pipefail;", 0),
        ("set -o pipefail", "7 9 0", "", 9),
        ("set -o pipefail", "7 9 23", "", 23),
        ("set -o pipefail", "0 0 0", "", 0),
        ("set +o pipefail", "7 9 0", "", 0),
    ] {
        let pipeline = stages
            .split_whitespace()
            .map(|code| format!("sh -c 'exit {code}'"))
            .collect::<Vec<_>>()
            .join(" | ");
        for wait in ["wait \"$pid\"", "wait %1", "wait -f \"$pid\""] {
            differential(
                &format!("{options}; {pipeline} & pid=$!; {after} {wait}"),
                expected,
                b"",
            );
        }
    }
}

#[test]
fn background_pipefail_survives_canceled_wait_next() {
    // The losing wait reaps the rightmost stage before the competing job finishes.
    differential(
        "set -o pipefail; sh -c 'while test ! -e release; do sleep .005; done; exit 7' | sh -c 'exit 9' | sh -c ': >right-done; exit 0' & slow=$!; while test ! -e right-done; do sleep .005; done; sh -c 'exit 3' & fast=$!; wait -n; first=$?; : >release; wait \"$slow\"; second=$?; printf '%s %s\\n' \"$first\" \"$second\"",
        0,
        b"3 9\n",
    );
}

#[test]
fn background_pipefail_survives_input_loop_polling() {
    // `-s` goes through the real input loop, which polls jobs before reading each line.
    // Poll the first failure while the last stage is still running, then collect after reap.
    differential_with_input_mode(
        "mkfifo gate\nexec 3<>gate\nset -o pipefail\nsh -c ': >left-done; exit 7' | sh -c 'read x <gate; : >right-done; exit 0' & pid=$!\nuntil test -e left-done; do sleep .005; done\nset +o pipefail\necho >&3\nuntil test -e right-done; do sleep .005; done\nwait \"$pid\"\n",
        7,
        b"",
        true,
    );
    // A canceled wait consumes from the back; the input-loop poll later consumes from the
    // front. The selected failure must stay the rightmost one, not the last one observed.
    // Keep the left stage blocked until wait -n selects the competing job; it
    // marks its exit so the next line's poll sees it gone.
    differential_with_input_mode(
        "set -o pipefail\nsh -c 'while test ! -e release; do sleep .005; done; : >left-done; exit 7' | sh -c 'exit 9' | sh -c ': >right-done; exit 0' & slow=$!; while test ! -e right-done; do sleep .005; done; sh -c 'exit 3' & wait -n; first=$?; : >release\nuntil test -e left-done; do sleep .005; done\nwait \"$slow\"; second=$?; printf '%s %s\\n' \"$first\" \"$second\"\n",
        0,
        b"3 9\n",
        true,
    );
}

#[test]
fn jobs_p_names_group_leader_while_background_pid_names_rightmost() {
    // Each stage reports its actual PID; no comparison of nondeterministic raw PIDs.
    // Both stages stay alive until `jobs` has run.
    differential(
        "mkfifo gl gr; exec 3<>gl 4<>gr; sh -c 'echo $$ >left; read x <gl' | sh -c 'echo $$ >right; read x <gr; exit 23' & pid=$!; jobs -p >group; echo >&3; echo >&4; wait \"$pid\"; status=$?; test \"$(cat left)\" = \"$(cat group)\" || exit 90; test \"$(cat right)\" = \"$pid\" || exit 91; test \"$!\" = \"$pid\" || exit 92; test \"$(cat left)\" != \"$pid\" || exit 93; exit \"$status\"",
        23,
        b"",
    );
    // Polling may remove the group leader before jobs prints it; its identity must remain.
    differential_with_input_mode(
        "mkfifo gate\nexec 3<>gate\nsh -c 'echo $$ >left; exit 7' | sh -c 'echo $$ >right; read x <gate; exit 23' & pid=$!\nuntil test -s left && test -s right; do sleep 0.01; done\njobs -p >group\necho >&3\nwait \"$pid\"; status=$?; test \"$(cat left)\" = \"$(cat group)\" || exit 90; test \"$(cat right)\" = \"$pid\" || exit 91; test \"$!\" = \"$pid\" || exit 92; exit \"$status\"\n",
        23,
        b"",
        true,
    );
}

#[test]
fn lexical_alias_quotes_escapes_and_expanded_names_keep_function_lookup() {
    differential(
        "q(){ printf function; }; shopt -s expand_aliases; alias q='printf alias'; \\q; 'q'; \"q\"; n=q; $n; printf '\\n'",
        0,
        b"functionfunctionfunctionfunction\n",
    );
    differential_with_input_mode(
        "shopt -s expand_aliases\nalias q='printf \"alias:%s\\n\"'\nq 'two words'\n",
        0,
        b"alias:two words\n",
        true,
    );
}

#[test]
fn background_shell_commands_have_real_child_identity_and_isolated_state() {
    for command in ["true", "false", "{ exit 23; }", "false || exit 23", "f arg"] {
        let expected = match command {
            "true" => 0,
            "false" => 1,
            _ => 23,
        };
        differential(
            &format!(
                "f(){{ test \"$1\" = arg || exit 90; exit 23; }}; {command} & pid=$!; test -n \"$pid\" || exit 91; wait \"$pid\""
            ),
            expected,
            b"",
        );
    }
    differential(
        "printf input >input; { read value; printf '%s\\n' \"$value\"; } <input >output & pid=$!; wait \"$pid\"; cat output",
        0,
        b"input\n",
    );
}

#[test]
fn background_shell_command_bashpid_is_the_waitable_child() {
    differential(
        "hidden=retained; root=$$; f(){ test \"$hidden\" = retained || exit 91; test \"$$\" = \"$root\" || exit 92; printf '%s' \"$BASHPID\" >child; hidden=changed; sleep 0.03; return 17; }; f & pid=$!; wait \"$pid\"; r=$?; test \"$(cat child)\" = \"$pid\" || exit 93; test \"$hidden\" = retained || exit 94; exit \"$r\"",
        17,
        b"",
    );
}

#[test]
fn mixed_builtin_function_group_pipelines_preserve_streams_and_pipefail() {
    differential(
        "f(){ cat; return 17; }; printf 'bytes\\n' | f | cat; printf '%s\\n' \"${PIPESTATUS[*]}\"",
        0,
        b"bytes\n0 17 0\n",
    );
    differential(
        "set -o pipefail; printf 'bytes\\n' | { cat; exit 23; } | cat & pid=$!; wait \"$pid\"",
        23,
        b"bytes\n",
    );
    differential(
        "printf 'payload\\n' | { read value; printf '%s\\n' \"$value\"; }",
        0,
        b"payload\n",
    );
    differential(
        "exec 3>out; f(){ printf 'retained\\n' >&3; }; f 1>&- 2>&- & pid=$!; wait \"$pid\"; exec 3>&-; cat out",
        0,
        b"retained\n",
    );
}

#[test]
fn jobs_in_subshells_observe_parent_metadata_without_wait_ownership() {
    differential(
        "mkfifo gate; exec 3<>gate; sh -c 'read x <gate; :' & pid=$!; jobs -p >expected; jobs -p | cat >actual; cmp expected actual || exit 90; printf '%s\\n' \"$(jobs -p)\" >actual; cmp expected actual || exit 91; (jobs -p) >actual; test ! -s actual || exit 92; echo >&3; wait \"$pid\"",
        0,
        b"",
    );
}

#[test]
fn ordinary_expansion_arrays_functions_and_control_flow_corpus() {
    for (script, stdout) in [
        (
            "v='two words'; printf '<%s>\\n' \"$v\" $v; printf '%s\\n' \"${v/words/items}\"",
            "<two words>\n<two>\n<words>\ntwo items\n",
        ),
        (
            "a=(first 'two words' last); a[5]=tail; printf '%s:%s\\n' \"${#a[@]}\" \"${a[1]}\"; printf '<%s>\\n' \"${a[@]}\"",
            "4:two words\n<first>\n<two words>\n<last>\n<tail>\n",
        ),
        (
            "declare -A a=([x]=one [y]='two words'); printf '%s:%s\\n' \"${a[x]}\" \"${a[y]}\"",
            "one:two words\n",
        ),
        (
            "v=outer; f(){ local v=inner; printf '%s:%s\\n' \"$v\" \"$1\"; return 17; }; f arg; printf '%s:%s\\n' \"$?\" \"$v\"",
            "inner:arg\n17:outer\n",
        ),
        (
            "for n in 1 2 3; do case $n in 2) continue;; *) printf '%s' \"$n\";; esac; done; printf '\\n'",
            "13\n",
        ),
        (
            "i=0; while (( i < 3 )); do printf '%s' \"$((i++))\"; done; printf '\\n'",
            "012\n",
        ),
        (
            "set -e; false || printf recovered; if false; then exit 99; fi; ! false; printf '\\n'",
            "recovered\n",
        ),
        (
            "v=before; (v=after); printf '%s:%s\\n' \"$v\" \"$(printf sub)\"",
            "before:sub\n",
        ),
        (
            "cat <<'EOF'\n$literal\nEOF\ncat <<EOF\n$((2+3))\nEOF\n",
            "$literal\n5\n",
        ),
    ] {
        differential(script, 0, stdout.as_bytes());
    }
}

#[test]
fn script_operands_search_path_and_keep_original_zero_without_execute_permission() {
    let directory = tempfile::tempdir().unwrap();
    let path_directory = directory.path().join("bin");
    std::fs::create_dir(&path_directory).unwrap();
    std::fs::write(
        path_directory.join("operand"),
        "printf '%s:%s\\n' \"$0\" \"$1\"\nexit 17\n",
    )
    .unwrap();
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-local")] {
        let mut command = shell(program);
        if program != "bash" {
            command.arg("--no-config");
        }
        let output = command
            .args(["--noprofile", "--norc", "operand", "two words"])
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .env(
                "PATH",
                std::env::join_paths(
                    std::iter::once(path_directory.clone())
                        .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
                )
                .unwrap(),
            )
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(17), "{program}: {output:?}");
        assert_eq!(
            output.stdout, b"operand:two words\n",
            "{program}: {output:?}"
        );
        assert!(output.stderr.is_empty(), "{program}: {output:?}");
    }
}

#[test]
fn executable_without_shebang_uses_brush_and_preserves_state_arguments_and_fds() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("plain");
    write_executable(
        &path,
        "printf '%s:%s:%s\\n' \"$0\" \"$1\" \"$hidden\"; helper >&3; hidden=child; exit 17\n",
    );
    let script = "export hidden=retained; helper(){ printf 'helper\\n'; }; export -f helper; exec 3>out; ./plain 'two words'; r=$?; exec 3>&-; cat out; printf '%s:%s\\n' \"$r\" \"$hidden\"";
    let mut outputs = Vec::new();
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-local")] {
        let mut command = shell(program);
        if program != "bash" {
            command.arg("--no-config");
        }
        let output = command
            .args(["--noprofile", "--norc", "-c", script])
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{program}: {output:?}");
        assert!(output.stderr.is_empty(), "{program}: {output:?}");
        outputs.push(output.stdout);
    }
    assert_eq!(
        outputs[0],
        b"./plain:two words:retained\nhelper\n17:retained\n"
    );
    assert_eq!(outputs[1], outputs[0]);
    std::fs::write(&path, "test -n \"$BRUSH_VERSION\" || exit 91; printf brush").unwrap();
    let output = shell(env!("CARGO_BIN_EXE_marsh-local"))
        .args(["--no-config", "--noprofile", "--norc", "-c", "./plain"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"brush");
    assert!(output.stderr.is_empty(), "{output:?}");
}

/// A trapped SIGINT that reaches the shell while it is blocked in `wait` runs
/// the trap and ends that wait with 130; the child is neither reaped nor killed.
#[test]
fn trapped_sigint_interrupts_wait_without_consuming_or_killing_the_child() {
    for wait in ["wait \"$pid\"", "wait -n -p done \"$pid\"", "wait"] {
        let script = format!(
            "trap 'printf \"trap:%s\\n\" \"$?\"' INT; mkfifo gate; sh -c 'read x < gate; exit 17' & pid=$!; printf ready >ready; {wait}; first=$?; kill -0 \"$pid\" || exit 91; test -z \"${{done+x}}\" || exit 92; echo >gate; wait \"$pid\"; second=$?; printf '%s:%s\\n' \"$first\" \"$second\""
        );
        for program in ["bash", env!("CARGO_BIN_EXE_marsh-local")] {
            let directory = tempfile::tempdir().unwrap();
            let mut command = shell(program);
            if program != "bash" {
                command.arg("--no-config");
            }
            let child = common::spawn_group(
                command
                    .args(["--noprofile", "--norc", "-c", &script])
                    .env_remove("BASH_ENV")
                    .env_remove("ENV")
                    .current_dir(directory.path())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped()),
            );
            let ready = directory.path().join("ready");
            common::wait_until(
                &format!("{program}: ready"),
                Duration::from_secs(20),
                || ready.exists(),
            );
            // Past `printf ready` the next blocking point is the wait itself.
            common::wait_until_blocked(child.id(), &format!("{program}: {wait}"));
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
                nix::sys::signal::Signal::SIGINT,
            )
            .unwrap();
            let output = common::wait_for_exit(
                child,
                &format!("{program}: {wait}"),
                Duration::from_secs(20),
            );
            assert!(output.status.success(), "{program}: {wait}: {output:?}");
            assert_eq!(
                output.stdout, b"trap:130\n130:17\n",
                "{program}: {wait}: {output:?}"
            );
            assert!(output.stderr.is_empty(), "{program}: {wait}: {output:?}");
        }
    }
}

#[test]
fn kill_defaults_to_term() {
    differential(
        "sh -c 'trap \"exit 23\" TERM; printf ready >ready; sleep .2' & pid=$!; while test ! -f ready; do sleep .01; done; kill \"$pid\"; wait \"$pid\" 2>/dev/null; printf '%s\\n' \"$?\"",
        0,
        b"23\n",
    );
}

#[test]
fn kill_job_spec_signals_every_pipeline_stage() {
    differential(
        "sh -c 'trap \"printf yes >leftterm; exit 23\" TERM; printf ready >leftready; sleep .2' 2>/dev/null | sh -c 'trap \"printf yes >rightterm; exit 17\" TERM; printf ready >rightready; sleep .2' 2>/dev/null & pid=$!; while test ! -f leftready || test ! -f rightready; do sleep .01; done; kill -TERM %1; wait \"$pid\" 2>/dev/null; r=$?; test -f leftterm && test -f rightterm || exit 90; printf '%s\\n' \"$r\"",
        0,
        b"17\n",
    );
}

#[test]
fn aliases_are_read_before_execution_and_can_add_operators_chain_or_admit_next_word() {
    for (script, stdout) in [
        (
            "shopt -s expand_aliases\nalias q='if'\nq true; then printf conditional; fi\n",
            "conditional",
        ),
        (
            "shopt -s expand_aliases\nalias q='for'\nq n in one two; do printf '%s' \"$n\"; done\n",
            "onetwo",
        ),
        (
            "shopt -s expand_aliases\nalias name='printf forbidden'\ncase name in\nname) printf matched;;\nesac\n",
            "matched",
        ),
        (
            "q(){ printf function; }; shopt -s expand_aliases; alias q='printf alias'; q\nq\n",
            "functionalias",
        ),
        (
            "shopt -s expand_aliases\nalias q='printf one | tr o O'\nq\n",
            "One",
        ),
        (
            "shopt -s expand_aliases\nalias q='printf one; printf two'\nq\n",
            "onetwo",
        ),
        (
            "shopt -s expand_aliases\nalias q='r'; alias r='printf chained'\nq\n",
            "chained",
        ),
        (
            "shopt -s expand_aliases\nalias q='printf '; alias arg='value'\nq arg\n",
            "value",
        ),
        (
            "q(){ printf recursive; }; shopt -s expand_aliases\nalias q='q'\nq\n",
            "recursive",
        ),
        (
            "q(){ printf function; }; shopt -s expand_aliases\nalias q='printf alias'\n\\q & wait\n'q' | cat\n",
            "functionfunction",
        ),
        (
            "q(){ printf function; }; shopt -s expand_aliases\nalias q='printf alias'\nf(){ q; }\nalias q='printf changed'\nf\n",
            "alias",
        ),
        (
            "shopt -s expand_aliases\nalias q='printf alias'\nvalue=retained q\nprintf ':%s' \"$value\"\n",
            "alias:",
        ),
        (
            "shopt -s expand_aliases\nalias q='printf alias'; alias literal=forbidden\ncat <<'END'\nliteral\nEND\nq\n",
            "literal\nalias",
        ),
    ] {
        differential(script, 0, stdout.as_bytes());
    }
}

#[test]
fn native_compound_children_keep_heredocs_and_close_inherited_descriptors() {
    differential(
        "{ cat <<'END'\n$literal\nEND\n} & wait\ntrue && { cat <<END\n$((2+3))\nEND\n} & wait\n",
        0,
        b"$literal\n5\n",
    );
    differential(
        "exec 3>out; { printf child >&3; exec 3>&-; sh -c 'printf leaked >&3' 2>/dev/null; test $? -ne 0; } & pid=$!; wait \"$pid\" || exit 91; printf parent >&3; exec 3>&-; cat out",
        0,
        b"childparent",
    );
    differential(
        "shopt -s expand_aliases\nalias q='printf captured'\nf(){ q; }\nalias q='printf changed'\nf | cat\nf & wait\n",
        0,
        b"capturedcaptured",
    );
}

#[test]
fn no_shebang_script_starts_fresh_and_reads_bash_env_once() {
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-local")] {
        let directory = tempfile::tempdir().unwrap();
        let plain = directory.path().join("plain");
        let startup = directory.path().join("startup");
        write_executable(
            &plain,
            "printf '%s:' \"${hidden-unset}\"; type helper >/dev/null 2>&1; printf '%s\\n' \"$?\"\n",
        );
        std::fs::write(&startup, "printf e >>events\n").unwrap();
        let output = shell(program)
            .args([
                "--noprofile",
                "--norc",
                "-c",
                "hidden=private; helper(){ :; }; ./plain; cat events",
            ])
            .env("BASH_ENV", startup)
            .env_remove("ENV")
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{program}: {output:?}");
        assert_eq!(output.stdout, b"unset:1\nee", "{program}: {output:?}");
        assert!(output.stderr.is_empty(), "{program}: {output:?}");
    }
}

#[test]
fn waits_keep_explicit_pid_status_and_reject_nonchildren() {
    differential(
        "sh -c 'exit 17' & p=$!; wait \"$p\"; echo $?; wait \"$p\" 2>/dev/null; echo $?; wait -n 2>/dev/null; echo $?",
        0,
        b"17\n17\n127\n",
    );
    differential(
        "wait 99999999 2>/dev/null; echo $?; wait %99 2>/dev/null; echo $?; wait nope 2>/dev/null; echo $?",
        0,
        b"127\n127\n1\n",
    );
    let script = "sh -c 'sleep .1; exit 17' & p=$!; r=$(wait \"$p\" 2>/dev/null; echo $?); echo \"$r\"; wait \"$p\"; echo $?";
    let oracle = run("bash", script, false);
    assert!(oracle.status.success(), "{oracle:?}");
    assert!(oracle.stderr.is_empty(), "{oracle:?}");
    // GNU Bash versions differ here: 5.2 can expose internal status -1,
    // while 5.3 returns the ordinary non-child status 127. Classify that
    // specific oracle difference while requiring marsh's explicit 127.
    assert!(
        matches!(oracle.stdout.as_slice(), b"127\n17\n" | b"-1\n17\n"),
        "unexpected oracle behavior: {oracle:?}"
    );
    if oracle.stdout == b"-1\n17\n" {
        eprintln!(
            "classified GNU Bash oracle divergence: inherited non-child wait prints -1; marsh requires 127"
        );
    }
    let candidate = run(env!("CARGO_BIN_EXE_marsh-local"), script, false);
    assert!(candidate.status.success(), "{candidate:?}");
    assert_eq!(candidate.stdout, b"127\n17\n", "{candidate:?}");
    assert!(candidate.stderr.is_empty(), "{candidate:?}");
}

#[test]
fn native_subshell_and_coprocess_identities_are_real_processes() {
    differential(
        "p=$BASHPID; (test \"$BASHPID\" -ne \"$p\" && printf 'parens:%s\\n' \"$BASH_SUBSHELL\"); (printf 'pipe:%s\\n' \"$BASH_SUBSHELL\") | cat; r=$(test \"$BASHPID\" -ne \"$p\" && printf 'sub:%s\\n' \"$BASH_SUBSHELL\"); echo \"$r\"",
        0,
        b"parens:1\npipe:1\nsub:1\n",
    );
    differential(
        "coproc NAMED { printf '%s\\n' \"$BASHPID\"; }; p=$NAMED_PID; read -r child <&\"${NAMED[0]}\"; test \"$p\" = \"$child\" || exit 91; wait \"$p\"; echo coprocess",
        0,
        b"coprocess\n",
    );
    differential(
        "set -o pipefail; p=$BASHPID; printf '%s:%s\\n' \"$BASHPID\" \"$BASH_SUBSHELL\" | { read -r identity; case \"$identity\" in \"$p\":*) exit 91;; *:0) printf builtin;; *) exit 92;; esac; }",
        0,
        b"builtin",
    );
    differential(
        "set -o pipefail; sh -c 'test \"$1\" = \"$$\" || exit 91; printf external' ignored \"$BASHPID\" | cat",
        0,
        b"external",
    );
    differential(
        "f(){ printf before; sh -c 'printf middle'; printf after; }; f | cat",
        0,
        b"beforemiddleafter",
    );
    differential(
        "eval 'printf before; sh -c \"printf middle\"; printf after' | cat",
        0,
        b"beforemiddleafter",
    );
}

#[test]
fn kill_handles_multiple_pids_and_explicit_negative_process_groups() {
    // Each job announces itself after exec. Brush can lose a signal that lands
    // between its fork and exec; that is not what this test covers.
    differential(
        "sh -c ': >a; exec sleep 30' & a=$!; sh -c ': >b; exec sleep 30' & b=$!; until test -e a && test -e b; do sleep .01; done; kill -0 \"$a\" \"$b\" || exit 91; kill \"$a\" \"$b\"; wait; printf checked",
        0,
        b"checked",
    );
    // The job leads its own group (and only then announces itself), making
    // the negative operand a bounded probe that cannot signal the test
    // runner's group. Not `set -m`: GNU Bash's own job launch races on macOS
    // (parent and child both setpgid; the child's sometimes fails with EPERM
    // and it prints "child setpgid ...: Operation not permitted").
    differential(
        "perl -e 'setpgrp(0, 0) or die; open(F, \">p\") or die; close(F); exec(\"sleep\", \"30\")' & p=$!; until test -e p; do sleep .01; done; kill -0 -- -\"$p\" || exit 91; kill -TERM -- -\"$p\"; wait \"$p\"; printf group",
        0,
        b"group",
    );
}

#[test]
fn exec_without_shebang_keeps_pid_and_uses_brush_in_a_fresh_environment() {
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-local")] {
        let directory = tempfile::tempdir().unwrap();
        let plain = directory.path().join("plain");
        write_executable(
            &plain,
            "test \"$BASHPID\" = \"$1\" || exit 91; printf '%s:%s' \"${hidden-unset}\" \"$2\"\n",
        );
        let output = shell(program)
            .args([
                "--noprofile",
                "--norc",
                "-c",
                "hidden=private; exec ./plain \"$BASHPID\" 'two words'; printf forbidden",
            ])
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{program}: {output:?}");
        assert_eq!(output.stdout, b"unset:two words", "{program}: {output:?}");
        assert!(output.stderr.is_empty(), "{program}: {output:?}");
        std::fs::write(&plain, "test \"$BASHPID\" = \"$1\" || exit 93; sh -c 'printf forbidden' 2>/dev/null; test $? -ne 0 || exit 94; printf mapped >&13; exit 17\n").unwrap();
        let output = shell(program)
            .args(["--noprofile", "--norc", "-c", "exec 11>mapped; (exec ./plain \"$BASHPID\" 13>&11 11>&- 0<&- 1>&-); r=$?; exec 11>&-; cat mapped; printf ':%s' \"$r\""])
            .env_remove("BASH_ENV").env_remove("ENV")
            .current_dir(directory.path()).output().unwrap();
        assert!(output.status.success(), "{program}: {output:?}");
        assert_eq!(output.stdout, b"mapped:17", "{program}: {output:?}");
        assert!(output.stderr.is_empty(), "{program}: {output:?}");
        let output = shell(program)
            .args([
                "--noprofile",
                "--norc",
                "-c",
                "exec sh -c 'test ! -e /dev/fd/10 || exit 95; printf clean'",
            ])
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{program}: {output:?}");
        assert_eq!(output.stdout, b"clean", "{program}: {output:?}");
        assert!(output.stderr.is_empty(), "{program}: {output:?}");
        if program != "bash" {
            std::fs::write(
                &plain,
                "test -n \"$BRUSH_VERSION\" || exit 92; printf native",
            )
            .unwrap();
            let output = shell(program)
                .args([
                    "--no-config",
                    "--noprofile",
                    "--norc",
                    "-c",
                    "(exec ./plain); printf parent",
                ])
                .current_dir(directory.path())
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            assert_eq!(output.stdout, b"nativeparent");
            assert!(output.stderr.is_empty(), "{output:?}");
        }
    }
}

#[test]
fn failed_exec_restores_descriptors_and_respects_execfail() {
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-local")] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("denied"), "printf forbidden\n").unwrap();
        write_executable(
            directory.path().join("nointerp"),
            format!("#!{}/absent-interpreter\n", directory.path().display()),
        );
        for (script, status, stdout) in [
            (
                "exec ./denied 13>&1 1>out 2>/dev/null; printf forbidden",
                126,
                b"".as_slice(),
            ),
            (
                "shopt -s execfail; exec ./denied 13>&1 1>out 2>/dev/null; r=$?; printf 'status:%s' \"$r\"; cat out",
                0,
                b"status:126".as_slice(),
            ),
            (
                "shopt -s execfail; exec ./missing 13>&1 1>out 2>/dev/null; r=$?; printf 'status:%s' \"$r\"; cat out",
                0,
                b"status:127".as_slice(),
            ),
            (
                "PATH=.:$PATH; shopt -s execfail; exec nointerp 13>&1 1>out 2>/dev/null; r=$?; printf 'status:%s' \"$r\"; cat out",
                0,
                b"status:126".as_slice(),
            ),
            (
                "PATH=.:$PATH; nointerp 2>/dev/null; r=$?; printf 'status:%s' \"$r\"",
                0,
                b"status:126".as_slice(),
            ),
            (
                "PATH=.:$PATH; set -o pipefail; nointerp 2>/dev/null | cat; r=$?; printf 'status:%s' \"$r\"",
                0,
                b"status:126".as_slice(),
            ),
        ] {
            let output = shell(program)
                .args(["--noprofile", "--norc", "-c", script])
                .env_remove("BASH_ENV")
                .env_remove("ENV")
                .current_dir(directory.path())
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(status), "{program}: {output:?}");
            assert_eq!(output.stdout, stdout, "{program}: {output:?}");
            assert!(output.stderr.is_empty(), "{program}: {output:?}");
            assert_eq!(std::fs::read(directory.path().join("out")).unwrap(), b"");
        }
    }
    // Remove this test's private copy of the running interpreter. ENOEXEC's
    // second exec then fails; the restored runtime must still print on stdout.
    let directory = tempfile::tempdir().unwrap();
    let runner = directory.path().join("runner");
    write_executable(
        &runner,
        std::fs::read(env!("CARGO_BIN_EXE_marsh-local")).unwrap(),
    );
    write_executable(directory.path().join("plain"), "printf forbidden\n");
    let output = common::command(&runner)
        .args(["--no-config", "--noprofile", "--norc", "-c", "shopt -s execfail; rm \"$0\"; exec ./plain 13>&1 1>out 2>/dev/null; r=$?; printf 'status:%s:restored' \"$r\"; cat out"])
        .arg(&runner)
        .env_remove("BASH_ENV").env_remove("ENV")
        .current_dir(directory.path()).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"status:126:restored", "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(std::fs::read(directory.path().join("out")).unwrap(), b"");
}

#[test]
fn forked_subshells_isolate_process_state_and_die_like_bash() {
    for (script, status, stdout) in [
        (
            "umask 022; (umask 077); cd /; (cd /tmp); umask; pwd",
            0,
            "0022\n/\n",
        ),
        (
            "f(){ while :; do echo y; done; }; f | head -1; echo \"${PIPESTATUS[*]}\"",
            0,
            "y\n141 0\n",
        ),
        (
            "( ( echo \"$BASH_SUBSHELL\" ) ); echo \"$BASH_SUBSHELL\" | cat; { echo \"$BASH_SUBSHELL\"; } & wait",
            0,
            "2\n0\n1\n",
        ),
        (
            "trap 'echo parent' EXIT; (trap 'echo child' EXIT; exit 4); echo \"status $?\"",
            0,
            "child\nstatus 4\nparent\n",
        ),
        (
            "(sleep 5; echo late) & kill \"$!\"; wait \"$!\"; echo \"$?\"",
            0,
            "143\n",
        ),
        (
            "printf before; (printf mid); printf after; x=$( (echo inner) | tr a-z A-Z ); echo \" $x\"",
            0,
            "beforemidafter INNER\n",
        ),
        ("cat <(echo one) <(echo \"$BASH_SUBSHELL\")", 0, "one\n1\n"),
    ] {
        differential(script, status, stdout.as_bytes());
    }
}

#[test]
fn name_references_resolve_reads_assignments_and_builtins() {
    for (script, stdout) in [
        (
            "f(){ local -n r=$1; r=changed; }; v=orig; f v; echo \"$v\"",
            "changed\n",
        ),
        (
            "f(){ local -n out=$1; out=\"result $2\"; }; f res arg; echo \"$res\"",
            "result arg\n",
        ),
        (
            "f(){ local -n a=$1; a+=(x); echo \"${#a[@]}\"; }; arr=(1 2); f arr; echo \"${arr[*]}\"",
            "3\n1 2 x\n",
        ),
        (
            "f(){ local -n m=$1; m[key]=val; }; declare -A h; f h; echo \"${h[key]}\"",
            "val\n",
        ),
        (
            "f(){ local -n r=$1; read -r r <<< line; printf -v r '%s!' \"$r\"; }; f v; echo \"$v\"",
            "line!\n",
        ),
        (
            "declare -n r=t; t=5; declare -p r; declare r=7; echo \"$t\"; [[ -R r ]] && echo ref; unset -n r; echo \"${r-unset} $t\"",
            "declare -n r=\"t\"\n7\nref\nunset 7\n",
        ),
        (
            "for n in a b; do declare -n p=$n; p=$n$n; done; echo \"$a $b\"",
            "aa bb\n",
        ),
    ] {
        differential(script, 0, stdout.as_bytes());
    }
}

#[test]
fn globstar_matches_any_directory_depth() {
    differential(
        "mkdir -p a/b .h && touch a/b/x.txt y.txt .h/z.txt a/.d.txt; shopt -s globstar; echo **/*.txt; echo **; echo a/**; echo **/; echo a/**/x.txt; shopt -s dotglob; echo **/*.txt; shopt -u globstar; echo **/*.txt",
        0,
        b"a/b/x.txt y.txt\na a/b a/b/x.txt y.txt\na/ a/b a/b/x.txt\na/ a/b/\na/b/x.txt\n.h/z.txt a/.d.txt a/b/x.txt y.txt\n.h/z.txt a/.d.txt\n",
    );
}

#[test]
fn common_scripting_idioms_corpus() {
    for (script, stdout) in [
        (
            "a=(x 'y z' w); echo \"${#a[@]}|${a[1]}|${a[*]:1}|${!a[*]}\"",
            "3|y z|y z w|0 1 2\n",
        ),
        (
            "printf '%q\\n' 'a b' \"it's\" '$x' ''",
            "a\\ b\nit\\'s\n\\$x\n''\n",
        ),
        (
            "read -r -a arr <<< 'one two  three'; mapfile -t l < <(printf 'a\\nb\\n'); echo \"${#arr[@]} ${arr[2]} ${#l[@]} ${l[1]}\"",
            "3 three 2 b\n",
        ),
        (
            "set -- -a -b val c; while getopts 'ab:' o; do echo \"$o:${OPTARG-}\"; done; shift $((OPTIND-1)); echo \"$@\"",
            "a:\nb:val\nc\n",
        ),
        (
            "x=aXbXc; echo ${x//X/-} ${x^^} ${x,,} ${x#*X} ${x%X*} ${#x} ${x:1:3} ${x@Q}",
            "a-b-c AXBXC axbxc bXc aXb 5 XbX 'aXbXc'\n",
        ),
        ("pre_a=1; pre_b=2; echo ${!pre_@}", "pre_a pre_b\n"),
        (
            "[[ v1.22.3 =~ ^v([0-9]+)\\.([0-9]+) ]] && echo \"${BASH_REMATCH[0]} ${BASH_REMATCH[2]}\"",
            "v1.22 22\n",
        ),
        (
            "set -eo pipefail; out=$(echo a | grep b) || echo nomatch; trap 'echo err' ERR; false || true; echo ok",
            "nomatch\nok\n",
        ),
        (
            "cat <<< \"here $((1+2))\"; diff <(echo a) <(echo a) && echo same; echo {1..3} x{a,b}y {01..02}",
            "here 3\nsame\n1 2 3 xay xby 01 02\n",
        ),
        (
            "shopt -s extglob\nx=aaab; echo ${x##+(a)}; case foo.txt in !(*.log)) echo keep;; esac",
            "b\nkeep\n",
        ),
        (
            "printf 'a\\0b\\0' | while IFS= read -r -d '' x; do echo \"<$x>\"; done; IFS=, read -r p q r <<< 1,2,3; echo $q",
            "<a>\n<b>\n2\n",
        ),
    ] {
        differential(script, 0, stdout.as_bytes());
    }
}

#[test]
fn named_descriptor_redirections_allocate_from_ten() {
    differential(
        "exec {a}>one {b}>two; echo \"$a $b\"; echo x >&\"$b\"; sh -c 'echo child >>/dev/fd/'\"$a\"; exec {a}>&- {b}>&-; cat one two; exec {r}< <(printf 'l1\\nl2\\n'); read -r -u \"$r\" line; exec {r}<&-; echo \"$line\"; echo {fd} >literal; cat literal",
        0,
        b"10 11\nchild\nx\nl1\n{fd}\n",
    );
}

#[test]
fn printf_time_conversions_format_epoch_arguments() {
    differential(
        "printf '%(%Y-%m-%d)T|%s\\n' 1700000000 a 1700000000 b; printf '%12(%Y)T|%-4(%m)T|\\n' 1700000000 1700000000; printf -v now '%(%s)T'; test \"$now\" -gt 1700000000 && echo now; printf '%s)T %%\\n' x",
        0,
        b"2023-11-14|a\n2023-11-14|b\n        2023|11  |\nnow\nx)T %\n",
    );
}

#[test]
fn arithmetic_on_associative_elements_uses_the_key_text() {
    differential(
        "declare -A c; for w in a b a; do ((c[$w]++)); done; w=key; ((c[$w]+=5)); let \"c[$w]*=2\"; a=(1 2 3); i=1; ((a[i+1]++)); echo \"${c[a]} ${c[b]} ${c[key]} $((c[key]+1)) ${a[*]}\"",
        0,
        b"2 1 10 11 1 2 4\n",
    );
}

#[test]
fn read_timeouts_distinguish_end_of_file_from_silence() {
    differential(
        "mkfifo gate; exec 3<>gate; read -t 1 x < /dev/null; echo $?; read -t 0 < /dev/null; echo $?; printf '' | { read -t 1 x; echo $?; }; sh -c 'read y < gate' | { read -t 0.05 x; echo $?; echo >&3; }",
        0,
        b"1\n0\n1\n142\n",
    );
}

#[test]
fn signals_caught_outside_waits_run_traps_or_end_the_shell() {
    use std::os::unix::process::ExitStatusExt as _;
    // Once a wait has installed the runtime's handlers, a builtin-only loop
    // must still see trapped and untrapped INT/TERM.
    for (script, status, stdout) in [
        (
            "trap 'echo t; exit 3' TERM; /usr/bin/true; kill -TERM $$; while :; do :; done",
            3,
            "t\n",
        ),
        (
            "trap 'echo t; exit 4' INT; kill -INT $$; while :; do :; done",
            4,
            "t\n",
        ),
        (
            "/usr/bin/true; trap '' INT; kill -INT $$; i=0; while (( i < 200 )); do i=$((i+1)); done; echo ignored",
            0,
            "ignored\n",
        ),
    ] {
        differential(script, status, stdout.as_bytes());
    }
    for (script, signal, stdout) in [
        (
            "trap 'echo exittrap' EXIT; /usr/bin/true; kill -INT $$; while :; do :; done",
            2,
            "exittrap\n",
        ),
        ("/usr/bin/true; kill -TERM $$; while :; do :; done", 15, ""),
    ] {
        for program in ["bash", env!("CARGO_BIN_EXE_marsh-local")] {
            let output = run(program, script, false);
            assert_eq!(
                output.status.signal(),
                Some(signal),
                "{program}: {output:?}"
            );
            assert_eq!(output.stdout, stdout.as_bytes(), "{program}: {output:?}");
        }
    }
}

/// With marsh extensions on, coreutils `split` and `join` keep their meaning
/// everywhere except `split {` in command position and `join` right after it.
#[test]
fn coreutils_split_and_join_are_unaffected_by_split_join_composition() {
    for (script, status, stdout) in [
        (
            "printf 'a\\nb\\nc\\n' > f; split -l 1 f p; cat paa pab pac",
            0,
            "a\nb\nc\n",
        ),
        (
            "printf '1 a\\n2 b\\n' > l; printf '1 x\\n2 y\\n' > r; join l r; printf '1 q\\n' | join l -; X=1 join l r | wc -l | tr -d ' '",
            0,
            "1 a x\n2 b y\n1 a q\n2\n",
        ),
        (
            "printf '1 a\\n' > l; f() { join \"$@\"; }; printf '1 z\\n' | f l -; command split -l 1 l s; cat saa",
            0,
            "1 a z\n1 a\n",
        ),
    ] {
        differential(script, status, stdout.as_bytes());
    }
}

/// SIGINT to the process group of a noninteractive shell whose foreground
/// child dies of it ends the shell by SIGINT: nothing after runs. A child that
/// handles SIGINT and exits normally lets the list continue.
///
/// The `ready` marker is written by the foreground child itself (after it has
/// installed its trap, where it has one). The signal is sent once the shell is
/// also blocked: a shell that has started the child but not yet reached its
/// wait may not have installed the handler that defers the signal.
#[test]
fn noninteractive_group_sigint_ends_the_command_list() {
    use std::os::unix::process::ExitStatusExt as _;
    for (script, status, stdout) in [
        (
            "for i in 1 2 3; do sh -c 'printf r >> ready; exec sleep 300'; echo it$i; done; echo loopdone",
            None,
            b"".as_slice(),
        ),
        (
            "sh -c 'printf r > ready; exec sleep 30'; echo after",
            None,
            b"",
        ),
        (
            "sh -c 'trap \"kill \\$!; exit 3\" INT; sleep 300 & printf r > ready; wait'; echo after $?",
            Some(0),
            b"after 3\n",
        ),
    ] {
        for program in ["bash", env!("CARGO_BIN_EXE_marsh-local")] {
            let directory = tempfile::tempdir().unwrap();
            let mut command = shell(program);
            if program != "bash" {
                command.arg("--no-config");
            }
            let child = common::spawn_group(
                command
                    .args(["--noprofile", "--norc", "-c", script])
                    .current_dir(directory.path())
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped()),
            );
            let ready = directory.path().join("ready");
            common::wait_until(
                &format!("{program}: {script}: ready"),
                Duration::from_secs(20),
                || ready.exists(),
            );
            common::wait_until_blocked(child.id(), &format!("{program}: {script}"));
            let group = nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap());
            nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGINT).unwrap();
            let output = common::wait_for_exit(
                child,
                &format!("{program}: {script}: SIGINT"),
                Duration::from_secs(20),
            );
            assert_eq!(
                output.status.code(),
                status,
                "{program}: {script}: {output:?}"
            );
            if status.is_none() {
                assert_eq!(output.status.signal(), Some(2), "{program}: {output:?}");
            }
            assert_eq!(output.stdout, stdout, "{program}: {script}: {output:?}");
        }
    }
}

/// A shell that signals itself with SIGINT acts on it before its next command,
/// as Bash does: an untrapped one ends a noninteractive shell by SIGINT, a
/// trapped one runs its trap first. The preceding external command makes the
/// shell catch SIGINT (until then its default action applies), so this
/// exercises the caught-signal path. The signal is delivered to the shell
/// synchronously by `kill`; the shell must not depend on its runtime having
/// dispatched the arrival yet. Each case is repeated because the dispatch race
/// it closes (13% of runs before it was closed) is only a window.
#[test]
fn noninteractive_self_sigint_is_acted_on_before_the_next_command() {
    use std::os::unix::process::ExitStatusExt as _;
    for (script, signal, stdout) in [
        ("sh -c :; kill -INT $$; echo after", Some(2), b"".as_slice()),
        (
            "trap 'echo trapped' INT; sh -c :; kill -INT $$; echo after",
            None,
            b"trapped\nafter\n",
        ),
    ] {
        for program in ["bash", env!("CARGO_BIN_EXE_marsh-local")] {
            for attempt in 0..40 {
                let output = run(program, script, false);
                assert_eq!(
                    output.status.signal(),
                    signal,
                    "{program}: {script}: attempt {attempt}: {output:?}"
                );
                assert_eq!(
                    output.stdout, stdout,
                    "{program}: {script}: attempt {attempt}: {output:?}"
                );
                if signal.is_none() {
                    assert_eq!(
                        output.status.code(),
                        Some(0),
                        "{program}: {script}: attempt {attempt}: {output:?}"
                    );
                }
            }
        }
    }
}

/// Syntax errors name the line Bash names: the line of an unexpected token,
/// or the line after the last one read for an unexpected end of file.
#[test]
fn syntax_errors_report_the_line_bash_reports() {
    fn reported_line(stderr: &[u8]) -> Option<String> {
        let text = String::from_utf8_lossy(stderr);
        let start = text.find("line ")? + "line ".len();
        let digits = text[start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>();
        text[start + digits.len()..]
            .starts_with(": syntax error")
            .then_some(digits)
    }
    for script in [
        "if true; then echo",
        "if true; then\n",
        "f() {\n:\n",
        "for x in a; do",
        "echo a; )",
        "echo a\n\necho b; )",
    ] {
        let bash = run("bash", script, false);
        let brush = run(env!("CARGO_BIN_EXE_marsh-local"), script, false);
        assert_eq!(bash.status.code(), Some(2), "{script:?}: {bash:?}");
        assert_eq!(brush.status.code(), Some(2), "{script:?}: {brush:?}");
        let expected = reported_line(&bash.stderr);
        assert!(expected.is_some(), "{script:?}: {bash:?}");
        assert_eq!(
            reported_line(&brush.stderr),
            expected,
            "{script:?}: {brush:?}"
        );
    }
}

/// Bash's `-c` is a flag: the command is the first operand after every
/// option, in any order (docs/upstream/brush/0042). Agents call
/// `bash -c -l CMD`, `bash -lc CMD`, and `bash -o pipefail -c CMD`.
#[test]
fn command_flag_takes_the_first_operand_in_bash_option_order() {
    let script = "echo \"x:$0:$*:${-//[^ef]/}:$(shopt -q login_shell && echo login)\"; \
                  set -o | grep -q 'pipefail *on' && echo pipefail; false; echo after";
    for options in [
        &["-c", "-l"][..],
        &["-cl"],
        &["-lc"],
        &["--login", "-c"],
        &["-c", "-e"],
        &["-ce"],
        &["-ec"],
        &["-c", "-o", "pipefail"],
        &["-o", "pipefail", "-c"],
        &["-co", "pipefail"],
        &["-c", "-l", "--"],
        &["-c"],
    ] {
        let outputs = ["bash", env!("CARGO_BIN_EXE_marsh-local")].map(|program| {
            let mut command = shell(program);
            if program != "bash" {
                command.arg("--no-config");
            }
            command
                .env_remove("BASH_ENV")
                .env_remove("ENV")
                .env_remove("SHELLOPTS")
                .env("HOME", "/nonexistent")
                .env("LC_ALL", "C")
                .stdin(Stdio::null())
                .args(["--noprofile", "--norc"])
                .args(options)
                .args([script, "argv0", "a1", "-l"])
                .output()
                .unwrap()
        });
        let [bash, brush] = &outputs;
        assert!(
            String::from_utf8_lossy(&bash.stdout).starts_with("x:argv0:a1 -l:"),
            "Bash {options:?}: {bash:?}"
        );
        assert_eq!(
            brush.status.code(),
            bash.status.code(),
            "{options:?}: {brush:?}"
        );
        assert_eq!(brush.stdout, bash.stdout, "{options:?}: {brush:?}");
    }
    // `-c` with no operand is a usage error (status 2) in both.
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-local")] {
        let output = shell(program)
            .args(["-c", "-l"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{program}: {output:?}");
    }
}

/// `kill -- -PGID` with no signal sends `TERM` to the process group: `--` ends
/// the options, so `-PGID` is an operand, not a signal (patch `0052`).
#[test]
fn kill_double_dash_negative_operand_terminates_the_group() {
    differential(
        "perl -e 'setpgrp(0, 0) or die; open(F, \">p\") or die; close(F); exec(\"sleep\", \"30\")' & p=$!; until test -e p; do sleep .01; done; kill -- -\"$p\"; echo \"kill:$?\"; wait \"$p\"; echo \"wait:$?\"",
        0,
        b"kill:0\nwait:143\n",
    );
}

/// A signal sent to a command the instant it is launched reaches it. A forked
/// shell used to exec the command from a secondary thread, and Darwin drops a
/// signal (even `SIGKILL`) sent during such an exec, leaving `sleep` running
/// (about one run in seven on macOS); a signal landing between fork and exec
/// also ran the shell's handler in the child. Repeated because it is a window
/// (patch `0053`).
#[test]
fn a_signal_sent_right_after_launch_reaches_the_command() {
    differential(
        // Bash reports each killed job on stderr ("Killed: 9"); Brush does not.
        "exec 2>/dev/null; n=0; for i in $(seq 200); do sleep 2 & kill $!; wait $!; test $? = 143 && n=$((n+1)); \
         /bin/sleep 2 & kill -KILL $!; wait $!; test $? = 137 && n=$((n+1)); done; echo $n",
        0,
        b"400\n",
    );
}

/// Environment entries that are not UTF-8 neither end the shell nor change:
/// external commands receive them byte for byte, unless the shell assigns or
/// unsets the variable (patch `0055`). Inside the shell such a value is text
/// (see docs/design/bash-compatibility.md, BYTES).
#[test]
fn inherited_non_utf8_environment_reaches_commands_unchanged() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;
    let script = "/usr/bin/env | grep -a -e '^RAW_' | sort; KEPT=1; RAW_SET=new; unset RAW_UNSET; \
                  /usr/bin/env | grep -a -e '^RAW_SET' -e '^RAW_UNSET' | sort";
    let outputs = ["bash", env!("CARGO_BIN_EXE_marsh-local")].map(|program| {
        let mut command = shell(program);
        if program != "bash" {
            command.arg("--no-config");
        }
        command
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .env("LC_ALL", "C")
            .env("RAW_KEPT", OsStr::from_bytes(b"a\xffZ"))
            .env("RAW_SET", OsStr::from_bytes(b"b\xfe"))
            .env("RAW_UNSET", OsStr::from_bytes(b"c\xff"))
            .stdin(Stdio::null())
            .args(["--noprofile", "--norc", "-c", script])
            .output()
            .unwrap()
    });
    let [bash, brush] = &outputs;
    assert_eq!(
        bash.stdout, b"RAW_KEPT=a\xffZ\nRAW_SET=b\xfe\nRAW_UNSET=c\xff\nRAW_SET=new\n",
        "Bash: {bash:?}"
    );
    assert_eq!(brush.status.code(), bash.status.code(), "{brush:?}");
    assert_eq!(brush.stdout, bash.stdout, "{brush:?}");
    assert_eq!(brush.stderr, bash.stderr, "{brush:?}");
}

/// A pattern that starts with a quote is still a regex (its quoted parts
/// match literally), and a match sets `BASH_REMATCH` (patch `0056`).
#[test]
fn quoted_regex_patterns_set_bash_rematch() {
    differential(
        "[[ xay =~ \"a\" ]]; echo \"$?:${BASH_REMATCH[0]}:${#BASH_REMATCH[@]}\"; \
         [[ xa.yz =~ \"a\".y ]]; echo \"$?:${BASH_REMATCH[0]}\"; \
         [[ xay =~ \"b\" ]]; echo \"$?:${#BASH_REMATCH[@]}\"",
        0,
        b"0:a:1\n0:a.y\n1:0\n",
    );
}

/// `[[ == ]]` and `[[ != ]]` match extended globs whether or not `extglob`
/// is set, literal or expanded (patch `0057`).
#[test]
fn conditional_patterns_are_extended_globs_without_extglob() {
    differential(
        "shopt -u extglob; p='@(a|ab)'; [[ ab == $p ]]; echo $?; [[ ab != $p ]]; echo $?; \
         [[ abc == +([a-c]) ]]; echo $?",
        0,
        b"0\n1\n0\n",
    );
}

/// `!` directly before an operator is the parameter `$!`, not indirection
/// (patch `0058`).
#[test]
fn last_background_pid_takes_parameter_operators() {
    differential(
        "echo \"${!:-none}\"; sh -c : & p=$!; wait; test \"${!:-none}\" = \"$p\" && echo same; \
         test \"${!:+set}\" = set && echo set; x=y; y=z; echo \"${!x}\"",
        0,
        b"none\nsame\nset\nz\n",
    );
}

/// `read -u` takes any descriptor, not only 0-255 (patch `0059`).
#[test]
fn read_takes_descriptors_above_255() {
    differential(
        "ulimit -n 512; exec 260<<<hello; read -u 260 x; echo \"$?:$x\"",
        0,
        b"0:hello\n",
    );
}

/// `read` with several names splits as Bash does: IFS whitespace around a
/// delimiter is part of it, and the last name drops a trailing delimiter when
/// one word is left (patch `0060`).
#[test]
fn read_splits_fields_for_names_like_bash() {
    differential(
        r#"for l in "a,b," "a,b,c," "a,," " a , b , " "a,b,,"; do IFS=", " read a b <<< "$l"; printf '[%s][%s]' "$a" "$b"; done; echo
           IFS=: read a b <<< "x::y:"; echo "[$a][$b]"; IFS=, read a <<< "x,"; echo "[$a]""#,
        0,
        b"[a][b][a][b,c,][a][][a][b][a][b,,]\n[x][:y:]\n[x]\n",
    );
}

/// An unknown `\D{...}` conversion in a prompt is shown as written instead of
/// ending the shell (patch `0061`). Not a differential: which conversions
/// exist is the platform `strftime`'s, and the panic is what is tested.
#[test]
fn unknown_prompt_date_conversion_does_not_end_the_shell() {
    let output = run(
        env!("CARGO_BIN_EXE_marsh-local"),
        "PS4='\\D{%Q}x '; set -x; :; set +x; echo after",
        false,
    );
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stdout, b"after\n", "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("%Qx :\n"),
        "{output:?}"
    );
}

/// `\w` and `\W` show the directory name as data: a `$(...)` in it is not run
/// (patch `0062`). Before, tracing such a command expanded PS4 again forever.
#[test]
fn prompt_directory_names_are_not_expanded() {
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-local")] {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().join("a$(touch hit)`touch hit2`\"q\"\\b");
        std::fs::create_dir(&cwd).unwrap();
        let mut command = shell(program);
        if program != "bash" {
            command.arg("--no-config");
        }
        let child = command
            .env_remove("BASH_ENV")
            .env("LC_ALL", "C")
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args([
                "--noprofile",
                "--norc",
                "-c",
                "PS4='\\W| '; set -x; :; set +x",
            ])
            .spawn()
            .unwrap();
        let output = common::wait_for_exit(child, program, Duration::from_secs(20));
        assert_eq!(output.status.code(), Some(0), "{program}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "a$(touch hit)`touch hit2`\"q\"\\b| :\na$(touch hit)`touch hit2`\"q\"\\b| set +x\n",
            "{program}"
        );
        assert!(
            std::fs::read_dir(&cwd).unwrap().next().is_none(),
            "{program}: ran a command"
        );
    }
}

/// PS4 is expanded with `xtrace` off, as in Bash: a `$(...)` in it is not
/// traced, which used to expand PS4 again without end (patch `0063`).
#[test]
fn trace_prompt_command_substitutions_are_not_traced() {
    let script = "n=0; PS4='$(printf X)$((n+=1)) '; set -x; false; :; set +x; echo \"n=$n\"";
    let outputs = ["bash", env!("CARGO_BIN_EXE_marsh-local")].map(|program| {
        let mut command = shell(program);
        if program != "bash" {
            command.arg("--no-config");
        }
        let child = command
            .env_remove("BASH_ENV")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args(["--noprofile", "--norc", "-c", script])
            .spawn()
            .unwrap();
        common::wait_for_exit(child, program, Duration::from_secs(20))
    });
    let [bash, brush] = &outputs;
    assert_eq!(bash.stdout, b"n=3\n", "Bash: {bash:?}");
    assert_eq!(
        bash.stderr, b"X1 false\nX2 :\nX3 set +x\n",
        "Bash: {bash:?}"
    );
    assert_eq!(brush.stdout, bash.stdout, "{brush:?}");
    assert_eq!(brush.stderr, bash.stderr, "{brush:?}");
}

/// Only the results of expansions are field-split, as in Bash: with `IFS=:` a
/// literal `x:y` and each word of a brace expansion stay whole, while `$v`
/// splits, keeping the empty fields a non-whitespace IFS character delimits
/// (patch `0064`).
#[test]
fn field_splitting_splits_only_expansion_results() {
    differential(
        r#"IFS=:; v=":a::b:"; printf '<%s>' x:y $v {c:d,e:f} ${u:-g:h}; echo
           v=':'; printf '<%s>' a${v}${v}b a${v} ${v}b; echo
           IFS=' :'; v='  : a  :  : b  :  '; printf '<%s>' $v; echo
           unset IFS; printf '<%s>' {,} ""{,} a{,}b {"p q",r}; echo"#,
        0,
        b"<x:y><><a><><b><c:d><e:f><g><h>\n<a><><b><a><><b>\n<><a><><b>\n<><><ab><ab><p q><r>\n",
    );
}

/// An assignment's value is not brace-expanded (patch `0065`).
#[test]
fn assignment_values_are_not_brace_expanded() {
    differential(
        "v={a,b}; y=(x{1,2} [5]={p,q}); echo \"$v ${y[*]}\"",
        0,
        b"{a,b} x1 x2 {p,q}\n",
    );
}

/// With `IFS` set but empty, `"$*"` joins the parameters with nothing (patch `0066`).
#[test]
fn star_joins_with_nothing_under_an_empty_ifs() {
    differential(
        "set -- a b; IFS=; x=\"$*\"; a=(p q); echo \"$x ${a[*]}\"; unset IFS; echo \"$*\"",
        0,
        b"ab pq\na b\n",
    );
}

/// The smallest prefix or suffix a pattern matches may be empty: `${v#*}` and
/// `${v%?(a)}` remove nothing (patch `0067`).
#[test]
fn smallest_pattern_removal_can_remove_nothing() {
    differential(
        "shopt -s extglob; v=aaaa; p=/x/y; echo \"${v#*} ${v%*} ${v%?(a)} ${v#@(|a)} ${v##*}. ${p#*/} ${p%/*}\"",
        0,
        b"aaaa aaaa aaaa aaaa . x/y /x\n",
    );
}

/// `disown` with job specs, PIDs and options, its diagnostics, and Bash's
/// table of jobs collected by `wait`: still listed (so `disown %1` succeeds)
/// until the next `jobs`, `wait` or process.
#[test]
fn disown_removes_jobs_as_bash_does() {
    for (script, stdout) in [
        ("disown; echo $?", b"1\n".as_slice()),
        ("disown -z; echo $?", b"2\n"),
        ("disown -a; disown -r; disown -ah; echo $?", b"0\n"),
        ("disown %3 0 '' abc -- -1; echo $?", b"1\n"),
        (
            "sleep 1 & disown; echo $?; jobs; wait $!; echo $?",
            b"0\n0\n",
        ),
        (
            "sleep 1 & disown $!; jobs; sleep 1 & disown -h; set -- $(jobs -p); echo $#",
            b"1\n",
        ),
        (
            "sleep 1 & sleep 1 & disown %1 %1; echo $?; jobs",
            b"1\n[2]+  Running                    sleep 1 &\n",
        ),
        (
            "sleep 1 | cat & disown %cat; echo $?; disown %?LEE; echo $?",
            b"1\n0\n",
        ),
        (
            "sleep 1 | cat & sleep 1 & disown %?LEE; echo $?; disown %sleep; echo $?",
            b"1\n1\n",
        ),
        (
            "sleep 1 & sleep 1 & disown %sl; echo $?; disown -a; jobs",
            b"1\n",
        ),
        (
            "sleep 1 & echo \"$(disown; echo $?)\"; (disown; echo $?)",
            b"0\n1\n",
        ),
        (
            "true & wait $!; disown %1; echo $?; true & wait $!; jobs; disown %1; echo $?",
            b"0\n1\n",
        ),
        (
            "true & wait $!; disown -r; disown %1; echo $?; true & wait $!; /usr/bin/true; disown %1; echo $?",
            b"0\n1\n",
        ),
        (
            "sleep 1 & true & wait $!; disown -r; disown; echo $?; jobs",
            b"1\n",
        ),
        (
            "(exit 7) & wait $!; echo $?; wait $!; echo $?; disown %1; echo $?",
            b"7\n7\n1\n",
        ),
        // `wait` keeps the `$!` job only if it had already finished, until `jobs` reports it.
        (
            "/usr/bin/true & /bin/sleep .3; wait; disown %1; echo $?; /bin/sleep .3 & wait; disown %1; echo $?; /bin/sleep .1 & /bin/sleep .4; wait; jobs",
            b"0\n1\n[1]+  Done                       /bin/sleep .1\n",
        ),
    ] {
        let bash = run_named("bash", script, false, Some("audit"));
        assert_eq!(bash.stdout, stdout, "Bash: {script}: {bash:?}");
        let brush = run_named(
            env!("CARGO_BIN_EXE_marsh-local"),
            script,
            false,
            Some("audit"),
        );
        assert_eq!(
            brush.status.code(),
            bash.status.code(),
            "{script}: {brush:?}"
        );
        assert_eq!(brush.stdout, bash.stdout, "{script}: {brush:?}");
        assert_eq!(brush.stderr, bash.stderr, "{script}: {brush:?}");
    }
}

/// Bash 5.3 function substitutions: `${ cmd; }` runs `cmd` in the current
/// shell (side effects persist, `local` and `return` work, positional
/// parameters are shared) and yields its output; `${| cmd; }` yields a local
/// `REPLY` and leaves the output alone. The closing brace counts only where a
/// reserved word could be.
#[test]
fn function_substitutions_run_in_the_current_shell() {
    differential(
        r#"v=before; echo "${ v=after; printf hi; }" "$v"
echo "[${ local x=1; echo $x; return 3; echo no; }] $? x=${x-unset}"
set -- a b; echo "${ echo $1; set -- c; }" $1
echo "${| REPLY=hi; }" ${REPLY-unset}; REPLY=o; echo "${| REPLY=x; }" $REPLY; echo "${| echo out; }" "<${| :; }>"
x=${ printf "a

"; }; echo "[$x]"; set -- ${ echo a b; }; echo $#; echo "${ (exit 4); }" $?; v=${ false; }; echo $?
a=${ echo {a,b}; }; echo $a; b=${ { echo g; }; }; echo $b; c=${ echo x}y; }; echo $c; echo ${ echo hi;}x
echo "${	echo tab; }" "${
echo nl
}" "${ printf '}'; }" "${ echo 'a }' ; }"
y=${ echo ${ echo inner; }; }; z=${ if true; then echo yes; fi; }; w=${ case a in a) echo ca;; esac; }; echo $y $z $w
g(){ echo "${ return 7; }"; echo "g:$?"; }; g
cat <<E
${ echo here; } ${| REPLY=reply; }
E
t=${ sleep 3 & echo bg; }; echo "$t"; kill %1
set -x; W=${ v=traced; echo t; } :; set +x
echo "${ exit 5; }"; echo after"#,
        5,
        b"hi after\n[1] 3 x=unset\na c\nhi unset\nx o\nout\n <>\n[a]\n2\n 4\n1\na b\ng\nx}y\nhix\ntab nl } a }\ninner yes ca\n\ng:0\nhere reply\nbg\n",
    );
}

/// NUL bytes in shell input, as Bash treats them: dropped from scripts and
/// standard input; refused as a binary file when one is on a script's first
/// line (first 80 bytes; 128 for a file run by `exec` or as a command); for
/// `.`, Bash's removal that keeps every second of consecutive NULs (ending
/// the text) and refuses more than 256; `printf -v` stops at a NUL.
#[test]
fn nul_bytes_in_shell_input_follow_bash() {
    let directory = tempfile::tempdir().unwrap();
    let files: &[(&str, &[u8])] = &[
        ("dropped", b"printf FIRST\n\0printf SECOND\npri\0ntf ' %s' a\\\0\nb\ncat <<E\nx\nE\0\nprintf END\0"),
        ("binary", b"#\0comment\nprintf AFTER\n"),
        ("late", b"#xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\0\nprintf LATE\n"),
        ("pairs", b"printf A\n\0\0\nprintf B\n"),
        ("syntax", b"printf one\nif\n"),
    ];
    for (name, bytes) in files {
        let path = directory.path().join(name);
        write_executable(&path, bytes);
    }
    let many: Vec<u8> = b"printf X\n"
        .iter()
        .copied()
        .chain(std::iter::repeat_n(b"#\0\n".iter().copied(), 257).flatten())
        .collect();
    std::fs::write(directory.path().join("many"), many).unwrap();
    let runs: &[&[&str]] = &[
        &["dropped"],
        &["binary"],
        &["late"],
        &[
            "-c",
            ". ./dropped; echo \" $?\"; . ./pairs; echo \" $?\"; . ./many; echo \" $?\"; . ./syntax; echo \" $?\"",
            "audit",
        ],
        &[
            "-c",
            "./binary; echo \" $?\"; ./late; echo \" $?\"; printf -v v 'a\\0b'; echo \"$v\"; exec ./late",
            "audit",
        ],
        &["-s"],
    ];
    for args in runs {
        let outputs = ["bash", env!("CARGO_BIN_EXE_marsh-local")].map(|program| {
            let mut command = shell(program);
            if program != "bash" {
                command.arg("--no-config");
            }
            command
                .args(["--noprofile", "--norc"])
                .args(*args)
                .current_dir(directory.path())
                .env_remove("BASH_ENV")
                .env("LC_ALL", "C")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = common::spawn_group(&mut command);
            let mut stdin = child.stdin.take().unwrap();
            // A shell running a script may exit before it reads its input.
            if let Err(error) = stdin.write_all(b"printf IN\0PUT\n") {
                assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe, "{error}");
            }
            drop(stdin);
            common::wait_for_exit(
                child,
                &format!("{program} {args:?}"),
                Duration::from_mins(1),
            )
        });
        let [bash, brush] = &outputs;
        let stderr = |output: &Output| {
            String::from_utf8_lossy(&output.stderr)
                .replace(directory.path().to_str().unwrap(), "DIR")
        };
        assert_eq!(
            brush.status.code(),
            bash.status.code(),
            "{args:?}: {brush:?} vs {bash:?}"
        );
        assert_eq!(brush.stdout, bash.stdout, "{args:?}: {brush:?} vs {bash:?}");
        if !args.iter().any(|arg| arg.contains("syntax")) {
            assert_eq!(stderr(brush), stderr(bash), "{args:?}");
        }
    }
}

/// Bytes that are not UTF-8 survive every boundary as in Bash: arguments,
/// `-c` and script text, command substitution, `$'\NNN'`, `echo -e`,
/// `printf` (format, `%s`, `-v`), the environment, external arguments,
/// `read` and `mapfile` (which read byte by byte and leave the rest),
/// here-documents, file names in globs, redirections, `cd` and command
/// names, and `${#v}` counts bytes in the C locale.
#[test]
fn non_utf8_bytes_survive_every_boundary() {
    use std::os::unix::ffi::OsStrExt as _;
    let directory = tempfile::tempdir().unwrap();
    let script =
        br#"v=$(printf '\377\376'); w=$'\377'; printf '%s|%s|' "$v" "$w"; echo -ne '\0375|'
printf "$v:%s|" ok; printf -v p '%s\374' "$v"; printf '%s|%s|%s|' "$p" "${#v}" "${#p}"
export RAW="$v"; /bin/sh -c 'printf "%s|" "$RAW" "$1"' sh "$v$1"
cat <<E
here $v
E
printf 'a\377 b\376\nrest\n' | { read -r x y; printf '[%s][%s]' "$x" "$y"; cat; }
printf '\303X\376' | { IFS= read -r -n 1 c; printf '(%s)' "$c"; cat; }
printf '\377\n\376\n' | { mapfile -t m; printf '{%s}' "${m[@]}"; }
case $v in ??) echo two;; esac; echo "${v#?}" | od -An -tx1
"#;
    // macOS refuses file names that are not UTF-8.
    let files: &[u8] = if cfg!(target_os = "linux") {
        br#"printf '%s\n' "$v" >"f$v"; cat "f$v"; for f in f*; do printf '<%s>' "$f"; done
mkdir "d$v"; cd d*; pwd | od -An -c | tr -s ' ' | tr -d '\n'; cd ..
printf '#!/bin/sh\necho ran\n' >"x$v"; chmod +x "x$v"; ./x*
"#
    } else {
        b""
    };
    let path = directory.path().join("script");
    std::fs::write(&path, [script.as_slice(), files].concat()).unwrap();
    let outputs = ["bash", env!("CARGO_BIN_EXE_marsh-local")].map(|program| {
        let mut command = shell(program);
        if program != "bash" {
            command.arg("--no-config");
        }
        command
            .args(["--noprofile", "--norc"])
            .arg(&path)
            .arg(std::ffi::OsStr::from_bytes(b"\xfb"))
            .current_dir(directory.path())
            .env_remove("BASH_ENV")
            .env("LC_ALL", "C");
        for entry in std::fs::read_dir(directory.path()).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name() != "script" {
                let _ = std::fs::remove_dir_all(entry.path())
                    .or_else(|_| std::fs::remove_file(entry.path()));
            }
        }
        command.output().unwrap()
    });
    let [bash, brush] = &outputs;
    assert!(bash.status.success(), "Bash: {bash:?}");
    assert!(
        bash.stdout.starts_with(b"\xff\xfe|\xff|\xfd|\xff\xfe:ok|"),
        "Bash: {bash:?}"
    );
    assert_eq!(brush.stdout, bash.stdout, "{brush:?}");
    // Without a controlling terminal (CI) both warn; Bash's warning names the
    // process group it could not hand the terminal, which depends on the caller.
    let after_warning = |stderr: &[u8]| {
        let text = String::from_utf8_lossy(stderr).into_owned();
        let warned = text.contains("shell: cannot set terminal process group (")
            && text.contains("shell: no job control in this shell\n");
        let rest = text
            .rsplit_once("no job control in this shell\n")
            .map_or(text.clone(), |(_, rest)| rest.to_owned());
        (warned, rest)
    };
    assert_eq!(
        after_warning(&brush.stderr),
        after_warning(&bash.stderr),
        "{brush:?}"
    );
    assert_eq!(brush.status.code(), bash.status.code());
}

/// `${v/pattern/string}` replaces as Bash's `pat_subst`: the leftmost,
/// longest match (extended globs such as `!(a)` included); after an empty
/// match one character is kept; only a `*` pattern meets empty text (except
/// at the end for `/%`); an empty pattern replaces nothing; with
/// `patsub_replacement` an unquoted `&` is the match and `$0` is text.
#[test]
fn pattern_substitution_matches_like_bash() {
    differential(
        r#"shopt -s extglob; v=abba; r='&'
echo "${v/!(a)/r} ${v//!(a)/r} ${v//?(a)/r} ${v//@(|a)/r} ${v///r} ${v/#/r} ${v/%/r} ${v//?/<&>}"
echo "${v//b/\&} ${v//b/"&"} ${v//b/$r} ${v//b/"$r"} ${v//b/\$0} ${v/%?(x)/r}"
e=; echo "[${e/*/r}][${e/@(|a)/r}][${e/%?(a)/r}]"; shopt -u patsub_replacement; echo "${v//b/&}""#,
        0,
        b"r r rrbrbr rrbrbr abba rabba abbar <a><b><b><a>\na&&a a&&a abba a&&a a$0$0a abbar\n[r][][r]\na&&a\n",
    );
}

/// An interactive shell whose input and standard error are not a terminal
/// (and that has no controlling terminal) says it cannot use job control,
/// leaves `m` out of `$-`, prompts with PS1 and PS2 on standard error for
/// every line it reads, and announces `exit` at the end of input.
#[test]
fn interactive_shell_without_a_terminal_prompts_on_standard_error() {
    use std::os::unix::process::CommandExt as _;
    let outputs = ["bash", env!("CARGO_BIN_EXE_marsh-local")].map(|program| {
        let mut command = shell(program);
        command.arg0("shell");
        if program != "bash" {
            command.args(["--no-config", "--input-backend", "basic"]);
        }
        command
            .args(["--noprofile", "--norc", "--noediting", "-i", "-s"])
            .env("PS1", "P1> ")
            .env("PS2", "P2> ")
            .env("LC_ALL", "C")
            .env_remove("BASH_ENV")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"printf A\nif true; then\nprintf B\nfi\necho \"$-\"\n")
            .unwrap();
        child.wait_with_output().unwrap()
    });
    let [bash, brush] = &outputs;
    assert_eq!(bash.stdout, b"ABhiBHs\n", "Bash: {bash:?}");
    assert!(
        bash.stderr.ends_with(b"P1> P1> P2> P2> P1> P1> exit\n"),
        "Bash: {bash:?}"
    );
    assert_eq!(brush.stdout, bash.stdout, "{brush:?}");
    // Without a controlling terminal (CI) both warn; Bash's warning names the
    // process group it could not hand the terminal, which depends on the caller.
    let after_warning = |stderr: &[u8]| {
        let text = String::from_utf8_lossy(stderr).into_owned();
        let warned = text.contains("shell: cannot set terminal process group (")
            && text.contains("shell: no job control in this shell\n");
        let rest = text
            .rsplit_once("no job control in this shell\n")
            .map_or(text.clone(), |(_, rest)| rest.to_owned());
        (warned, rest)
    };
    assert_eq!(
        after_warning(&brush.stderr),
        after_warning(&bash.stderr),
        "{brush:?}"
    );
    assert_eq!(brush.status.code(), bash.status.code());
}

/// A command's words expand first, then its assignments (each seeing the one
/// before), then its redirections, which do not see the command's variables.
/// For a command that runs as its own process, the redirections expand as if
/// in the child: their side effects (`$((++i))`, `${v:=x}`, `{fd}`, a function
/// substitution's assignments or `exit`) do not reach the shell. A failed
/// redirection is reported as Bash does.
#[test]
fn redirections_expand_after_assignments_and_in_the_child_of_a_process() {
    let script = r#"i=0; /bin/echo a >"$((++i))"; unset v; /bin/echo b >"${v:=out}"; echo "i=$i v=${v-unset}"
unset fd; /bin/echo c {fd}>f1; echo "fd=${fd-unset}"; printf d {fd}>f2; echo " fd=${fd-unset}"
v=before; W=${ v=after; printf val; } /bin/echo e >"${ printf '%s' "$v"; }"; echo "v=$v"; cat after
v=before; command /bin/echo f >"${ v=after; printf g; }"; echo "v=$v"; cat g
a=old; a=new b=$a /usr/bin/printenv b >h; cat h; unset W; W=val /bin/echo i >"${W-none}"; cat none
trap 'echo EXIT' EXIT; /bin/echo j >"${ exit 12; }"; echo "status=$?"
cat <<E
${ v=heredoc; printf k; }
E
echo "v=$v"; /bin/echo l >missing/m; echo "status=$?""#;
    let expected = b"i=0 v=unset\nc\nfd=unset\nd fd=10\nv=after\ne\nv=before\nf\nnew\ni\nstatus=12\nk\nv=before\nstatus=1\nEXIT\n";
    let bash = run_named("bash", script, false, Some("audit"));
    assert_eq!(bash.stdout, expected, "Bash: {bash:?}");
    let brush = run_named(
        env!("CARGO_BIN_EXE_marsh-local"),
        script,
        false,
        Some("audit"),
    );
    assert_eq!(brush.status.code(), bash.status.code(), "{brush:?}");
    assert_eq!(brush.stdout, bash.stdout, "{brush:?}");
    assert_eq!(brush.stderr, bash.stderr, "{brush:?}");
}

/// `getopts` notices every assignment to OPTIND, even of the value it has:
/// `OPTIND=1` restarts parsing, and another value takes effect once the
/// cluster being parsed (`-xyz`) is done.
#[test]
fn getopts_notices_every_assignment_to_optind() {
    for (script, stdout) in [
        (
            "set -- -abc; getopts abc v; printf %s \"$v\"; OPTIND=1; getopts abc v; echo \"$v|$OPTIND\"",
            b"aa|1\n".as_slice(),
        ),
        (
            "set -- -xyz -abc; getopts xyzabc v; printf %s: \"$v\"; OPTIND=2; getopts xyzabc v; printf %s:%s \"$v\" \"$OPTIND\"; getopts xyzabc v; printf :%s:%s \"$v\" \"$OPTIND\"; getopts xyzabc v; echo \":$v:$OPTIND\"",
            b"x:y:2:z:3:?:3\n",
        ),
        (
            "f() { local OPTIND; getopts ab v; echo \"$v$OPTIND\"; }; set -- -ab; f; f",
            b"?1\n?1\n",
        ),
    ] {
        differential(script, 0, stdout);
    }
}

/// Small byte and builtin details: `echo` options in order (`-eE`), `read -n 0`,
/// `export` of an expanded `NAME=value` word (and its diagnostics), `printf %q`
/// of bytes that are not UTF-8, `\NNN` in a prompt, `\w` showing control
/// characters, and `bash -- script`.
#[test]
fn byte_and_builtin_details_match_bash() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("s.sh"), "echo \"zero=$0 n=$# $*\"\n").unwrap();
    let script = r#"echo -eE '\0101'; echo -Ee 'a\tb'; echo -- -n; echo '' | { read -r -n 0 v; echo "[$v]"; }
a="X=1"; export "$a"; sh -c 'echo "X=$X"'; export "b-c=2"; echo $?; readonly R=1; export R=2 S=3; echo "$? $R $S"
printf '%q %#q %q\n' "$(printf 'a\377b')" "a b" "a b"
PS4='\101\w> '; set -x; : traced; set +x
"#;
    let outputs = ["bash", env!("CARGO_BIN_EXE_marsh-local")].map(|program| {
        let mut command = shell(program);
        if program != "bash" {
            command.arg("--no-config");
        }
        let in_dir = command
            .args(["--noprofile", "--norc", "-c", script, "audit"])
            .current_dir(directory.path())
            .env("HOME", directory.path())
            .env("LC_ALL", "C")
            .env_remove("BASH_ENV")
            .output()
            .unwrap();
        let mut command = shell(program);
        if program != "bash" {
            command.arg("--no-config");
        }
        let script_operand = command
            .args(["--noprofile", "--norc", "--", "s.sh", "a", "--"])
            .current_dir(directory.path())
            .output()
            .unwrap();
        (in_dir, script_operand)
    });
    let [(bash, bash_script), (brush, brush_script)] = &outputs;
    assert!(
        bash.stdout
            .starts_with(b"\\0101\na\tb\n-- -n\n[]\nX=1\n1\n1 1 3\n$'a\\377b' 'a b' a\\ b\n"),
        "Bash: {bash:?}"
    );
    assert_eq!(brush.stdout, bash.stdout, "{brush:?}");
    assert_eq!(brush.stderr, bash.stderr, "{brush:?}");
    assert_eq!(
        bash_script.stdout, b"zero=s.sh n=2 a --\n",
        "Bash: {bash_script:?}"
    );
    assert_eq!(brush_script.stdout, bash_script.stdout, "{brush_script:?}");
}

/// `unset` reports a readonly name and unsets the rest; `echo` follows
/// `xpg_echo` (and takes no options with it in posix mode); `mapfile -C`
/// runs its callback every `-c` lines with the index and the line about to be
/// stored, and the callback can read the same input.
#[test]
fn unset_echo_and_mapfile_callback_details_match_bash() {
    differential(
        r#"readonly first=one; second=two; unset first second 2>/dev/null; echo "$? $first ${second-unset}"
shopt -s xpg_echo; echo 'a\tb'; echo -E 'a\tb'; set -o posix; echo -nE 'c\td'; set +o posix; shopt -u xpg_echo
cb() { printf 'cb<%s><%s>[%s]' "$1" "$2" "${#a[@]}"; return 7; }
printf 'x\ny\nz\n' | { mapfile -t -C cb -c 2 a; echo "|$?|${a[*]}"; }
take() { IFS= read -r d; printf '<%s>' "$d"; }
printf "a\nconsumed\nb\nlast\no'k\n" | { mapfile -t -C take -c 1 a; echo "|${a[*]}"; }
printf "o'k\n" | mapfile -t -C cb -c 1 a; echo"#,
        0,
        b"1 one unset\na\tb\na\\tb\n-nE c\td\ncb<1><y>[1]|0|x y z\n<consumed><last><>|a b o'k\ncb<0><o'k>[0]\n",
    );
}

/// `printf` converts numeric arguments as Bash does (`strtoimax`/`strtod`,
/// `'c` character codes): a bad one is `invalid number`, uses what was
/// converted, and makes the status 1; an invalid conversion prints what came
/// before it and fails. `%b` is Bash's `bexpand`: `\"` keeps its backslash,
/// `\c` ends all output, and `\x` without digits is reported. Diagnostics come
/// before each pass's output, as Bash flushes it.
#[test]
fn printf_conversions_and_errors_match_bash() {
    let script = r#"printf '%d %u %d %x|' " 12" -1 "'A" 0x1f; echo "s=$?"
printf '%d|%d|%f|' "" 12x 1.5x 2>&1; echo "s=$?"
printf '%.*f|%*d|\n' 2 3.14159 5 7 x 1.5 2>&1
printf 'head%ZTAIL' x 2>&1; echo " s=$?"
printf '<%b>X' 'a\"b\q\101\0101\x41' '\x' 'c\cd' never 2>&1; echo " s=$?"
printf 'a\cb\n'"#;
    let expected = b"12 18446744073709551615 65 1f|s=0\nbash: line 2: printf: : invalid number\nbash: line 2: printf: 12x: invalid number\nbash: line 2: printf: 1.5x: invalid number\n0|12|1.500000|s=1\n3.14|    7|\nbash: line 3: printf: x: invalid number\n2|0|\nbash: line 4: printf: `Z': invalid format character\nhead s=1\nbash: line 5: printf: missing hex digit for \\x\n<a\\\"b\\qAAA>X<\\x>X<c s=0\na\\cb\n";
    let bash = run_named("bash", script, false, Some("bash"));
    assert_eq!(bash.stdout, expected, "Bash: {bash:?}");
    let brush = run_named(
        env!("CARGO_BIN_EXE_marsh-local"),
        script,
        false,
        Some("bash"),
    );
    assert_eq!(brush.status.code(), bash.status.code(), "{brush:?}");
    assert_eq!(brush.stdout, bash.stdout, "{brush:?}");
    assert_eq!(brush.stderr, bash.stderr, "{brush:?}");
}

/// `read` splits as Bash does: a backslash-escaped IFS character is data, IFS
/// whitespace around one other IFS character is one delimiter (arrays too),
/// and the last name's trailing IFS whitespace goes even when escaped. `read
/// -t 0` assigns nothing; a readonly name stops `read` with status 2.
#[test]
fn read_splitting_and_assignment_match_bash() {
    differential(
        r#"IFS=', '; read a b <<< 'a\ b c'; printf '<%s><%s>' "$a" "$b"; read a b <<< 'a\,b,c'; printf '<%s><%s>\n' "$a" "$b"
IFS=': '; read -a arr <<< 'a : b\ c::d \ '; printf '[%s]' "${arr[@]}"; echo
IFS=' '; read x y <<< 'p\  q  \ '; printf '<%s><%s>' "$x" "$y"; read x y <<< 'p q\ '; printf '<%s><%s>\n' "$x" "$y"
readonly first=one; read -r first second <<< 'a b' 2>/dev/null; printf '<%s><%s>|%s\n' "$first" "$second" "$?"
v=old; exec <<<data; read -t 0 v; echo "$v $?""#,
        0,
        b"<a b><c><a,b><c>\n[a][b c][][d][ ]\n<p ><q><p><q >\n<one><>|2\nold 0\n",
    );
}

/// Negative subscripts count back from one past the highest index; `{fd}>file`
/// on a builtin or function leaves the descriptor open in the shell (not with
/// `varredir_close`, nor for an external command); in posix mode, assignments
/// before a special builtin persist, still exported; a noninteractive shell's
/// `history` keeps a list (`-r` takes `#comment` lines as entries, `-w` keeps
/// bytes).
#[test]
fn subscripts_named_descriptors_special_assignments_and_history_match_bash() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("hist"),
        "#comment\nprintf a\n#1234567890\n",
    )
    .unwrap();
    let script = r#"a=([4]=old [9]=last); a[-1]+=x; a[-2]=y; echo "${a[-1]} ${!a[*]}"
f() { :; }; f {g}>o1; echo g >&$g; printf p {p}>o2; echo q >&$p; cat o1 o2
shopt -s varredir_close; /bin/echo e {k}>o4; echo "k=${k-unset}"
set -o posix; W=val : >"${W-none}"; A=1 eval 'echo in=$A'; sh -c 'echo "W=$W A=$A"'; C=3 command :; echo "C=${C-unset}"; set +o posix
history -c; history -r hist; history -s "$(printf 'b\377')"; history; history -w saved; cat saved"#;
    let outputs = ["bash", env!("CARGO_BIN_EXE_marsh-local")].map(|program| {
        let mut command = shell(program);
        if program != "bash" {
            command.arg("--no-config");
        }
        command
            .args(["--noprofile", "--norc", "-c", script, "audit"])
            .env("HISTFILE", "")
            .current_dir(directory.path())
            .output()
            .unwrap()
    });
    let [bash, brush] = &outputs;
    assert_eq!(
        bash.stdout,
        b"lastx 4 8 9\npg\nq\ne\nk=unset\nin=1\nW=val A=1\nC=unset\n    1  #comment\n    2  printf a\n    3  #1234567890\n    4  b\xff\n#comment\nprintf a\n#1234567890\nb\xff\n",
        "Bash: {bash:?}"
    );
    assert_eq!(brush.stdout, bash.stdout, "{brush:?}");
    assert_eq!(brush.stderr, bash.stderr, "{brush:?}");
    assert_eq!(brush.status.code(), bash.status.code());
}

/// `[[ =~ ]]` is the C library's POSIX ERE, as in Bash: leftmost-longest
/// alternation, every group in `BASH_REMATCH`, `nocasematch`; xtrace shows the
/// quoted parts as Bash escapes them; an invalid regex is status 2 (passed on
/// by `&&`) and leaves `BASH_REMATCH`.
#[test]
fn regex_matching_is_posix_extended() {
    differential(
        r#"[[ ab =~ (a|ab) ]]; printf '%s|' "$?" "${BASH_REMATCH[@]}"
[[ xyz =~ (x)(q)?(y) ]]; printf '%s|' "$?" "${#BASH_REMATCH[@]}" "${BASH_REMATCH[@]}"
shopt -s nocasematch; [[ ABC =~ b ]]; printf '%s|' "$?" "${BASH_REMATCH[0]}"; shopt -u nocasematch
r='('; [[ ab =~ $r ]] 2>/dev/null; printf '%s|' "$?" "${BASH_REMATCH[0]}"; [[ a =~ $r && b == b ]] 2>/dev/null; echo "and=$?"
set -x; [[ 'x[.]y' =~ x"[.]"y ]] 2>&1; set +x"#,
        0,
        b"0|ab|ab|0|4|xy|x||y|0|B|2|B|and=2\n+ [[ x[.]y =~ x\\[\\.]y ]]\n",
    );
}

/// A `CHLD` trap runs once for each reaped child, not for its own; GLOBIGNORE
/// filters glob results (whole paths, `.` and `..`) and implies dotglob.
#[test]
fn chld_traps_and_globignore_match_bash() {
    let directory = tempfile::tempdir().unwrap();
    for name in ["a.txt", "b.txt", ".h.txt", "d/x.txt"] {
        let path = directory.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "").unwrap();
    }
    let script = r#"trap 'printf "CHLD:%s\n" "$?"' CHLD; /usr/bin/true; f() { :; }; f & wait; /usr/bin/true | /usr/bin/true
trap '/usr/bin/true; echo inner' CHLD; /usr/bin/true; trap - CHLD; /usr/bin/true; echo done
GLOBIGNORE='a*'; echo *; GLOBIGNORE='*.txt'; echo * d/*; GLOBIGNORE='*'; echo *; GLOBIGNORE=x; echo .*; echo a.txt
unset GLOBIGNORE; echo *"#;
    let outputs = ["bash", env!("CARGO_BIN_EXE_marsh-local")].map(|program| {
        let mut command = shell(program);
        if program != "bash" {
            command.arg("--no-config");
        }
        command
            .args(["--noprofile", "--norc", "-c", script, "audit"])
            .current_dir(directory.path())
            .output()
            .unwrap()
    });
    let [bash, brush] = &outputs;
    assert_eq!(
        bash.stdout,
        b"CHLD:0\nCHLD:0\nCHLD:0\nCHLD:0\ninner\ndone\n.h.txt b.txt d\nd d/x.txt\n*\n.h.txt\na.txt\na.txt b.txt d\n",
        "Bash: {bash:?}"
    );
    assert_eq!(brush.stdout, bash.stdout, "{brush:?}");
    assert_eq!(brush.stderr, bash.stderr, "{brush:?}");
}

/// Diagnostics Bash words in a fixed form: command not found, `${v:?}`,
/// `local` outside a function, invalid identifiers, `declare +a`, `export -f`,
/// `read -u` of a closed descriptor, and `help` of unknown topics.
#[test]
fn common_diagnostics_are_worded_as_in_bash() {
    let script = r#"nosuchcmd_x; echo $?; declare -n ref=missing; ( echo "${ref:?variable is required}" ); ( : "${u?}" ); ( : "${u:?}" ); ( : "${1:?}" )
local x; declare -a da=(1); declare +a da; echo $?; declare "bad-name=1"; readonly "b-x=1"; export -f nosuchfn; export "e[0]=1"; echo $?
help echo nosuch >/dev/null; echo $?; help nosuch; echo $?; read -t 0 -u 1000 x; echo "R:$?""#;
    let bash = run_named("bash", script, false, Some("audit"));
    let brush = run_named(
        env!("CARGO_BIN_EXE_marsh-local"),
        script,
        false,
        Some("audit"),
    );
    assert!(
        bash.stderr.starts_with(b"audit: line 1: nosuchcmd_x: command not found\naudit: line 1: ref: variable is required\n"),
        "Bash: {bash:?}"
    );
    assert_eq!(brush.stdout, bash.stdout, "{brush:?}");
    assert_eq!(brush.stderr, bash.stderr, "{brush:?}");
    assert_eq!(brush.status.code(), bash.status.code());
}

/// `set -v` writes each input line as it is read: the rest of the line that
/// turned it on is not written, multi-line commands are written whole before
/// they run, and `eval`, `.` and trap strings are input too.
#[test]
fn verbose_writes_input_lines_as_they_are_read() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("src.sh"),
        "echo src1\n# c\necho src2\n",
    )
    .unwrap();
    let script = "set -v; echo first\nif true\nthen echo c\nfi\neval 'echo e1\necho e2'\n. ./src.sh\ntrap 'echo tr' EXIT\necho end # trailing\n# last comment";
    let outputs = ["bash", env!("CARGO_BIN_EXE_marsh-local")].map(|program| {
        let mut command = shell(program);
        if program != "bash" {
            command.arg("--no-config");
        }
        command
            .args([
                "--noprofile",
                "--norc",
                "-c",
                &std::format!("exec 2>&1\n{script}"),
                "audit",
            ])
            .current_dir(directory.path())
            .output()
            .unwrap()
    });
    let [bash, brush] = &outputs;
    assert_eq!(
        bash.stdout,
        b"first\nif true\nthen echo c\nfi\nc\neval 'echo e1\necho e2'\necho e1\ne1\necho e2\ne2\n. ./src.sh\necho src1\nsrc1\n# c\necho src2\nsrc2\ntrap 'echo tr' EXIT\necho end # trailing\nend\n# last comment\necho tr\ntr\n",
        "Bash: {bash:?}"
    );
    assert_eq!(brush.stdout, bash.stdout, "{brush:?}");
}

/// A DEBUG trap runs before the command expands or redirects, sees the command
/// as written in `BASH_COMMAND` and its line in `LINENO`; ERR traps see the
/// failing command's line.
#[test]
fn debug_and_err_traps_see_the_command_as_written() {
    differential(
        "trap 'printf \"D:%s:%s\\n\" \"$LINENO\" \"$BASH_COMMAND\"' DEBUG\nprintf '[%s]\\n' \"a b\" >/dev/null\nx=1 y=$(echo 2) true\ntrap - DEBUG\ntrap 'echo ERR:$LINENO' ERR\n\nfalse",
        1,
        b"D:2:printf '[%s]\\n' \"a b\" > /dev/null\nD:3:x=1 y=$(echo 2) true\nD:4:trap - DEBUG\nERR:7\n",
    );
}

/// Small builtin details from the upstream TCK: `set` with an empty value,
/// `$!` unset under `set -u`, an empty PATH entry as `./`, `declare 'b[]'`,
/// `shopt -o` columns, `local -`, `${a[@]@K}`/`@k`, and `exec` without `_`.
#[test]
fn small_builtin_details_match_bash() {
    let directory = tempfile::tempdir().unwrap();
    write_executable(directory.path().join("prog"), "#!/bin/sh\necho ran\n");
    let script = r#"q1=; set | grep '^q1='; ( set -u; echo "${!}" ) 2>&1 | sed 's/^[^:]*: line [0-9]*: //'
PATH=":$PATH"; type prog; command -v prog; hash prog; hash -t prog
declare 'b[]' 2>/dev/null; echo "s=$?"; shopt -o errexit | od -c | head -1; shopt -o | grep errexit | od -c | head -1
f() { local -; set -u; }; f; echo "$-"; a=(x y); printf '<%s>' "${a[@]@K}" "${a[@]@k}"; echo
(exec /usr/bin/env) | grep -c '^_='"#;
    let outputs = ["bash", env!("CARGO_BIN_EXE_marsh-local")].map(|program| {
        let mut command = shell(program);
        if program != "bash" {
            command.arg("--no-config");
        }
        command
            .args(["--noprofile", "--norc", "-c", script, "audit"])
            .current_dir(directory.path())
            .output()
            .unwrap()
    });
    let [bash, brush] = &outputs;
    assert!(
        bash.stdout
            .starts_with(b"q1=\n!: unbound variable\nprog is ./prog\n./prog\n./prog\ns=1\n"),
        "Bash: {bash:?}"
    );
    assert_eq!(brush.stdout, bash.stdout, "{brush:?}");
    assert_eq!(brush.stderr, bash.stderr, "{brush:?}");
}

/// A name reference to an array element reads, assigns, `read`s, `getopts`,
/// `printf -v`s and `unset`s the element, its subscript expanded at use.
#[test]
fn name_references_to_array_elements_match_bash() {
    differential(
        r#"a=(x y z); declare -n r="a[1]"; echo "$r"; r=Q; r+=W; echo "${a[*]}"; i=2; declare -n s="a[i]"; s=I; echo "${a[2]}"
IFS= read -r r <<< rd; getopts b: s -b val; printf -v r '%s!' "${a[1]}"; echo "${a[*]}"
declare -A h; k='the key'; h[$k]=v; declare -n hr='h["$k"]'; echo "[$hr]"; unset hr; echo "${#h[@]}""#,
        0,
        b"y\nx QW z\nI\nx rd! b\n[v]\n0\n",
    );
}

/// Invocation `set` options take effect left to right, `+` forms included,
/// with `-o` names inside clusters; unknown names are Bash's errors.
#[test]
fn invocation_options_follow_bash_order() {
    use std::os::unix::process::CommandExt as _;
    let flags = "printf '%s\\n' \"$-\"";
    for args in [
        vec!["-e", "+e", "-c", "false; echo survived"],
        vec!["-o", "errexit", "+e", "-c", "false; echo survived"],
        vec!["+u", "-o", "nounset", "-c", flags],
        vec![
            "-B",
            "+o",
            "braceexpand",
            "-a",
            "+o",
            "allexport",
            "-c",
            flags,
        ],
        vec!["-oe", "errexit", "-c", "false; echo survived"],
        vec!["-c", "-c", flags],
        vec!["-i", "+i", "-c", flags],
        vec!["-c", "-", "echo \"$0 $#\"", "--", "+e"],
        vec!["-o", "not_an_option", "-c", "echo ran"],
        vec!["-O", "not_an_option", "-c", "echo ran"],
    ] {
        let outputs = ["bash", env!("CARGO_BIN_EXE_marsh-local")].map(|program| {
            let mut command = shell(program);
            command.arg0("shell");
            if program != "bash" {
                command.arg("--no-config");
            }
            command
                .args(["--noprofile", "--norc"])
                .args(&args)
                .output()
                .unwrap()
        });
        let [bash, brush] = &outputs;
        assert_eq!(
            brush.status.code(),
            bash.status.code(),
            "{args:?}: {brush:?} {bash:?}"
        );
        assert_eq!(brush.stdout, bash.stdout, "{args:?}: {brush:?}");
        assert_eq!(brush.stderr, bash.stderr, "{args:?}: {brush:?}");
    }
}

/// Any catchable signal can be trapped (the trap runs at the next command
/// boundary) or ignored with an empty trap; the shell ignores SIGQUIT itself.
#[test]
fn every_catchable_signal_can_be_trapped_or_ignored() {
    differential(
        r#"for sig in USR1 USR2 HUP ALRM QUIT; do trap "echo got $sig" $sig; kill -$sig $$; trap '' $sig; kill -$sig $$; done
kill -QUIT $$; trap 'echo T' USR1; ( sleep 0.2; kill -USR1 $$ ) & sleep 1; echo slept; wait; trap - USR1; trap - QUIT; kill -QUIT $$
trap 'echo W' WINCH; trap - WINCH; kill -WINCH $$; trap - HUP; exec 2>/dev/null; ( kill -HUP $BASHPID; echo unreachable ); echo "status=$?""#,
        0,
        b"got USR1\ngot USR2\ngot HUP\ngot ALRM\ngot QUIT\nT\nslept\nstatus=129\n",
    );
}
