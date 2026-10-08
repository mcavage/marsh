//! Bounded Bash differentials through the embedded shell's executable entry point.
#![cfg(unix)]

use std::io::Write as _;
use std::process::{Command, Output, Stdio};

fn run(program: &str, script: &str, from_stdin: bool) -> Output {
    let directory = tempfile::tempdir().unwrap();
    let mut command = Command::new(program);
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
    command.args(["--noprofile", "--norc"]);
    if from_stdin {
        let mut child = command
            .arg("-s")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    } else {
        command.args(["-c", script]).output().unwrap()
    }
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
        "set -o pipefail\nsh -c 'exit 7' | sh -c 'sleep 0.15; exit 0' & pid=$!\nsleep 0.05\nset +o pipefail\nsleep 0.2\nwait \"$pid\"\n",
        7,
        b"",
        true,
    );
    // A canceled wait consumes from the back; the input-loop poll later consumes from the
    // front. The selected failure must stay the rightmost one, not the last one observed.
    // Keep the left stage blocked until wait -n selects the competing job, even under load.
    differential_with_input_mode(
        "set -o pipefail\nsh -c 'while test ! -e release; do sleep .005; done; exit 7' | sh -c 'exit 9' | sh -c ': >right-done; exit 0' & slow=$!; while test ! -e right-done; do sleep .005; done; sh -c 'exit 3' & wait -n; first=$?; : >release\nsleep 0.1\nwait \"$slow\"; second=$?; printf '%s %s\\n' \"$first\" \"$second\"\n",
        0,
        b"3 9\n",
        true,
    );
}

#[test]
fn jobs_p_names_group_leader_while_background_pid_names_rightmost() {
    // Each stage reports its actual PID; no comparison of nondeterministic raw PIDs.
    differential(
        "sh -c 'echo $$ >left; sleep 0.1' | sh -c 'echo $$ >right; sleep 0.1; exit 23' & pid=$!; jobs -p >group; wait \"$pid\"; status=$?; test \"$(cat left)\" = \"$(cat group)\" || exit 90; test \"$(cat right)\" = \"$pid\" || exit 91; test \"$!\" = \"$pid\" || exit 92; test \"$(cat left)\" != \"$pid\" || exit 93; exit \"$status\"",
        23,
        b"",
    );
    // Polling may remove the group leader before jobs prints it; its identity must remain.
    differential_with_input_mode(
        "sh -c 'echo $$ >left; exit 7' | sh -c 'echo $$ >right; sleep 0.6; exit 23' & pid=$!\nuntil test -s left && test -s right; do sleep 0.01; done\nsleep 0.05\njobs -p >group\nwait \"$pid\"; status=$?; test \"$(cat left)\" = \"$(cat group)\" || exit 90; test \"$(cat right)\" = \"$pid\" || exit 91; test \"$!\" = \"$pid\" || exit 92; exit \"$status\"\n",
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
        "sleep 0.5 & pid=$!; jobs -p >expected; jobs -p | cat >actual; cmp expected actual || exit 90; printf '%s\\n' \"$(jobs -p)\" >actual; cmp expected actual || exit 91; (jobs -p) >actual; test ! -s actual || exit 92; wait \"$pid\"",
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
        let mut command = Command::new(program);
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
    use std::os::unix::fs::PermissionsExt as _;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("plain");
    std::fs::write(
        &path,
        "printf '%s:%s:%s\\n' \"$0\" \"$1\" \"$hidden\"; helper >&3; hidden=child; exit 17\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let script = "export hidden=retained; helper(){ printf 'helper\\n'; }; export -f helper; exec 3>out; ./plain 'two words'; r=$?; exec 3>&-; cat out; printf '%s:%s\\n' \"$r\" \"$hidden\"";
    let mut outputs = Vec::new();
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-local")] {
        let mut command = Command::new(program);
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
    let output = Command::new(env!("CARGO_BIN_EXE_marsh-local"))
        .args(["--no-config", "--noprofile", "--norc", "-c", "./plain"])
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"brush");
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn trapped_sigint_interrupts_wait_without_consuming_or_killing_the_child() {
    use std::os::unix::process::CommandExt as _;
    for wait in ["wait \"$pid\"", "wait -n -p done \"$pid\"", "wait"] {
        let script = format!(
            "trap 'printf \"trap:%s\\n\" \"$?\"' INT; sh -c 'sleep .4; exit 17' & pid=$!; printf ready >ready; {wait}; first=$?; kill -0 \"$pid\" || exit 91; test -z \"${{done+x}}\" || exit 92; wait \"$pid\"; second=$?; printf '%s:%s\\n' \"$first\" \"$second\""
        );
        for program in ["bash", env!("CARGO_BIN_EXE_marsh-local")] {
            let directory = tempfile::tempdir().unwrap();
            let mut command = Command::new(program);
            if program != "bash" {
                command.arg("--no-config");
            }
            let mut child = command
                .args(["--noprofile", "--norc", "-c", &script])
                .env_remove("BASH_ENV")
                .env_remove("ENV")
                .current_dir(directory.path())
                .process_group(0)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while !directory.path().join("ready").exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{program} failed to become ready"
                );
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "{program} exited before wait"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
                nix::sys::signal::Signal::SIGINT,
            )
            .unwrap();
            while child.try_wait().unwrap().is_none() {
                if std::time::Instant::now() > deadline {
                    let _ = nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(-i32::try_from(child.id()).unwrap()),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                    panic!("{program}: trapped INT wait did not finish");
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let output = child.wait_with_output().unwrap();
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
fn product_registered_subprocess_dispatch_accepts_non_utf8_arguments() {
    use std::os::unix::{ffi::OsStringExt as _, fs::symlink};
    let directory = tempfile::tempdir().unwrap();
    let registered = directory.path().join("registered_probe");
    symlink(env!("CARGO_BIN_EXE_marsh"), &registered).unwrap();
    let output = Command::new(registered)
        .arg(std::ffi::OsString::from_vec(vec![0xff]))
        .env("MARSH_EXTERNAL_SESSION", "untrusted-test-context")
        .env_remove("MARSH_DAEMON_SOCKET")
        .env_remove("MARSH_DAEMON_TOKEN")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(125), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert_eq!(
        output.stderr,
        b"registered_probe: registered command requires an attached marsh session\n"
    );
}

#[test]
fn no_shebang_script_starts_fresh_and_reads_bash_env_once() {
    use std::os::unix::fs::PermissionsExt as _;
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-local")] {
        let directory = tempfile::tempdir().unwrap();
        let plain = directory.path().join("plain");
        let startup = directory.path().join("startup");
        std::fs::write(&plain, "printf '%s:' \"${hidden-unset}\"; type helper >/dev/null 2>&1; printf '%s\\n' \"$?\"\n").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(&startup, "printf e >>events\n").unwrap();
        let output = Command::new(program)
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
    differential(
        "sleep .1 & a=$!; sleep .1 & b=$!; kill -0 \"$a\" \"$b\" || exit 91; wait; printf checked",
        0,
        b"checked",
    );
    // Job control gives this job its own group, making the negative operand
    // a bounded existence probe that cannot signal the test runner's group.
    differential(
        "set -m; sleep .1 & p=$!; kill -0 -- -\"$p\" || exit 91; wait \"$p\"; printf group",
        0,
        b"group",
    );
}

#[test]
fn exec_without_shebang_keeps_pid_and_uses_brush_in_a_fresh_environment() {
    use std::os::unix::fs::PermissionsExt as _;
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-local")] {
        let directory = tempfile::tempdir().unwrap();
        let plain = directory.path().join("plain");
        std::fs::write(
            &plain,
            "test \"$BASHPID\" = \"$1\" || exit 91; printf '%s:%s' \"${hidden-unset}\" \"$2\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = Command::new(program)
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
        let output = Command::new(program)
            .args(["--noprofile", "--norc", "-c", "exec 11>mapped; (exec ./plain \"$BASHPID\" 13>&11 11>&- 0<&- 1>&-); r=$?; exec 11>&-; cat mapped; printf ':%s' \"$r\""])
            .env_remove("BASH_ENV").env_remove("ENV")
            .current_dir(directory.path()).output().unwrap();
        assert!(output.status.success(), "{program}: {output:?}");
        assert_eq!(output.stdout, b"mapped:17", "{program}: {output:?}");
        assert!(output.stderr.is_empty(), "{program}: {output:?}");
        let output = Command::new(program)
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
            let output = Command::new(program)
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
    use std::os::unix::fs::PermissionsExt as _;
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-local")] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("denied"), "printf forbidden\n").unwrap();
        std::fs::write(
            directory.path().join("nointerp"),
            format!("#!{}/absent-interpreter\n", directory.path().display()),
        )
        .unwrap();
        std::fs::set_permissions(
            directory.path().join("nointerp"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
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
            let output = Command::new(program)
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
    std::fs::copy(env!("CARGO_BIN_EXE_marsh-local"), &runner).unwrap();
    std::fs::write(directory.path().join("plain"), "printf forbidden\n").unwrap();
    std::fs::set_permissions(
        directory.path().join("plain"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let output = Command::new(&runner)
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
        "read -t 1 x < /dev/null; echo $?; read -t 0 < /dev/null; echo $?; printf '' | { read -t 1 x; echo $?; }; sleep 0.3 | { read -t 0.05 x; echo $?; }",
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
#[test]
fn noninteractive_group_sigint_ends_the_command_list() {
    use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
    for (script, status, stdout) in [
        (
            "printf r > ready; for i in 1 2 3; do sleep 5; echo it$i; done; echo loopdone",
            None,
            b"".as_slice(),
        ),
        ("printf r > ready; sleep 30; echo after", None, b""),
        (
            "printf r > ready; sh -c 'trap \"exit 3\" INT; sleep 5 & wait'; echo after $?",
            Some(0),
            b"after 3\n",
        ),
    ] {
        for program in ["bash", env!("CARGO_BIN_EXE_marsh-local")] {
            let directory = tempfile::tempdir().unwrap();
            let mut command = Command::new(program);
            if program != "bash" {
                command.arg("--no-config");
            }
            let mut child = command
                .args(["--noprofile", "--norc", "-c", script])
                .current_dir(directory.path())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0)
                .spawn()
                .unwrap();
            let ready = directory.path().join("ready");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !ready.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
            let group = nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap());
            nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGINT).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while child.try_wait().unwrap().is_none() {
                if std::time::Instant::now() > deadline {
                    let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
                    panic!("{program}: {script}: did not end after SIGINT");
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let output = child.wait_with_output().unwrap();
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

/// `PIPESTATUS` keeps one entry per pipeline stage when a split cannot run
/// (here: no workspace provider), including the stages after `join`.
#[test]
fn failed_split_reports_a_status_for_every_stage() {
    for (script, expected) in [
        (
            "split { a: true } | join | cat; echo \"${PIPESTATUS[*]}\"",
            "2 2 2\n",
        ),
        (
            "printf x | split { a: true } | join | cat | cat; echo \"${PIPESTATUS[*]}\"",
            "0 2 2 2 2\n",
        ),
        ("split { a: true }; echo \"${PIPESTATUS[*]}\"", "2\n"),
    ] {
        let output = run(env!("CARGO_BIN_EXE_marsh-local"), script, false);
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            expected,
            "{script}: {output:?}"
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
            let mut command = Command::new(program);
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
        let output = Command::new(program)
            .args(["-c", "-l"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{program}: {output:?}");
    }
}
