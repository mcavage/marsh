mod common;

use std::process::{Command, Stdio};

#[cfg(unix)]
use std::{os::unix::process::CommandExt, thread, time::Duration};

thread_local! {
    // The test thread retains its private home through every child wait.
    // No shell fixture reads or writes the developer's history or startup files.
    static SHELL_HOME: tempfile::TempDir = {
        use std::os::unix::fs::OpenOptionsExt;
        let home = tempfile::tempdir().expect("create private shell fixture home");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(home.path().join("history"))
            .expect("create private shell fixture history");
        home
    };
}

fn isolated_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    if program.as_ref() == "bash" {
        common::require_bash_oracle();
    }
    let mut command = common::command(program);
    SHELL_HOME.with(|home| {
        command
            .env("HOME", home.path())
            .env("HISTFILE", home.path().join("history"))
            .env_remove("BASH_ENV")
            .env_remove("ENV");
    });
    command
}

fn marsh() -> Command {
    let mut command = isolated_command(env!("CARGO_BIN_EXE_marsh-brush-test-driver"));
    // Most smoke cases do not exercise startup-file behavior.
    command.env_remove("BASH_ENV").env_remove("ENV");
    command
}

fn composition_shell() -> Command {
    let mut command = marsh();
    command.env("MARSH_TEST_EXTENSIONS", "1");
    command
}

/// No test shell sees the developer's credentials: each is started from a
/// cleared environment plus an allowlist (`common::command`). `make test`
/// exports a credential-shaped canary to prove it.
#[test]
fn test_shells_start_without_credentials() {
    let names = "env | cut -d= -f1";
    let options = ["--no-config", "--noprofile", "--norc", "-c", names];
    common::assert_no_credentials_reach("marsh", marsh().args(options));
    common::assert_no_credentials_reach("fanout marsh", composition_shell().args(options));
    common::assert_no_credentials_reach(
        "Bash",
        isolated_command("bash").args(["--noprofile", "--norc", "-c", names]),
    );
    #[cfg(unix)]
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-brush-test-driver")] {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("names");
        let mut shell = PtyShell::spawn(program, true);
        shell.expect_prompts(1);
        shell.send(format!("{names} >{}\r", file.display()).as_bytes());
        shell.expect_prompts(2);
        shell.send(b"exit 0\r");
        let (status, output) = shell.finish();
        assert_eq!(status, Some(0), "{program}: {output:?}");
        common::assert_no_credential_names(
            &format!("interactive {program}"),
            &std::fs::read_to_string(&file).unwrap(),
        );
    }
}

#[test]
fn child_shell_resolves_session_commands_from_brush_path() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let command = directory.path().join("fixture");
    std::fs::write(&command, "#!/bin/sh\nprintf '%s' \"$1\"\n").unwrap();
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = marsh()
        .env("MARSH_TEST_EXTERNAL_COMMANDS", directory.path())
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "sh -c 'fixture routed'",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"routed");
}

#[test]
fn brush_shim_keeps_precedence_over_descendant_path_command() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let command = directory.path().join("place_probe");
    std::fs::write(&command, "#!/bin/sh\nprintf child\n").unwrap();
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = marsh()
        .env("MARSH_TEST_REGISTER_PLACE", "1")
        .env("MARSH_TEST_EXTERNAL_COMMANDS", directory.path())
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "printf x | place_probe; sh -c place_probe",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"local:place_probe:xchild");
}

/// A registered-command link outside an attached session (no relay socket or
/// token) refuses to run, whatever bytes its arguments hold.
#[test]
fn registered_command_without_relay_fails_closed() {
    use std::os::unix::ffi::OsStringExt as _;
    let directory = tempfile::tempdir().unwrap();
    for (name, argument) in [
        ("identity", None),
        (
            "registered_probe",
            Some(std::ffi::OsString::from_vec(vec![0xff])),
        ),
    ] {
        let link = directory.path().join(name);
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_marsh"), &link).unwrap();
        let output = isolated_command(&link)
            .args(argument)
            .env("MARSH_EXTERNAL_SESSION", "untrusted-test-context")
            .env_remove("MARSH_DAEMON_SOCKET")
            .env_remove("MARSH_DAEMON_TOKEN")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(125), "{name}: {output:?}");
        assert!(output.stdout.is_empty(), "{name}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            format!("{name}: registered command requires an attached marsh session\n")
        );
    }
}

/// A terminal interrupt reaches the whole foreground process group. The shell
/// defers its INT/TERM trap until the foreground child has died of the same
/// signal, then runs it, whichever of the two events it observes first.
#[cfg(unix)]
#[test]
fn foreground_group_signal_runs_the_trap_after_the_child_dies() {
    for (signal, status) in [
        (nix::sys::signal::Signal::SIGINT, 130),
        (nix::sys::signal::Signal::SIGTERM, 143),
    ] {
        let name = &signal.as_str()[3..];
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        let mut command = marsh();
        command
            .current_dir(directory.path())
            .arg("-c")
            .arg(format!(
                "trap 'printf trapped; exit {status}' {name}; sh -c 'printf ready > ready; exec sleep 300'"
            ))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = common::spawn_group(&mut command);
        // The trap is installed before the child starts, so the shell catches
        // the signal whether or not it has reached its wait.
        common::wait_until("the foreground child", Duration::from_secs(20), || {
            ready.exists()
        });
        let group = nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap());
        nix::sys::signal::killpg(group, signal).unwrap();
        let output = common::wait_for_exit(child, name, Duration::from_secs(20));
        assert_eq!(output.status.code(), Some(status), "{name}: {output:?}");
        assert_eq!(output.stdout, b"trapped", "{name}: {output:?}");
    }
}

#[test]
fn wait_n_returns_first_child_status_and_pid() {
    // The second job cannot finish until `wait -n` has returned.
    let directory = tempfile::tempdir().unwrap();
    let output = marsh()
        .current_dir(directory.path())
        .args(["--no-config", "--noprofile", "--norc", "-c", "mkfifo gate; sh -c 'exit 23' & a=$!; sh -c 'read x < gate; exit 17' & b=$!; wait -n -p won \"$a\" \"$b\"; rc=$?; test \"$won\" = \"$a\"; printf '%s:%s' \"$rc\" \"$?\"; echo >gate; wait \"$b\""])
        .output().unwrap();
    assert_eq!(output.status.code(), Some(17), "{output:?}");
    assert_eq!(output.stdout, b"23:0", "{output:?}");
}

#[test]
fn wait_n_can_collect_each_job_once_and_reports_empty_set() {
    let dir = tempfile::tempdir().unwrap();
    let output = marsh()
        .current_dir(dir.path())
        .args(["--no-config", "--noprofile", "--norc", "-c", "sh -c 'exit 7' & sh -c 'while test ! -e second-may-exit; do sleep .01; done; exit 9' & wait -n; first=$?; : >second-may-exit; wait -n; second=$?; wait -n; third=$?; rm -f second-may-exit; printf '%s:%s:%s' \"$first\" \"$second\" \"$third\""])
        .output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"7:9:127");
}

fn matches_bash_in_a_fresh_directory(script: &str, expected: &[u8]) {
    common::require_bash_oracle();
    common::assert_no_blocking_fifo_open(script);
    let run = |shell: &str, mut command: Command| {
        let directory = tempfile::tempdir().unwrap();
        let child = common::spawn_group(
            command
                .current_dir(directory.path())
                .args(["--noprofile", "--norc", "-c", script])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        );
        // Name the shell: a case that overstays its budget in one of them
        // must say which.
        common::wait_for_exit(child, &format!("{shell}: {script}"), Duration::from_mins(1))
    };
    let bash = run("Bash", isolated_command("bash"));
    assert!(bash.status.success(), "Bash: {bash:?}");
    assert_eq!(bash.stdout, expected, "Bash: {bash:?}");
    let mut brush = marsh();
    brush.arg("--no-config");
    let brush = run("marsh", brush);
    assert_eq!(brush.status, bash.status, "{brush:?}");
    assert_eq!(brush.stdout, bash.stdout, "{brush:?}");
}

/// `wait -n` selects the finished job; the unselected pipeline, whose last
/// stage already exited 7, keeps that status for a later `wait`.
#[test]
fn wait_n_preserves_unselected_pipeline_status() {
    matches_bash_in_a_fresh_directory(
        "mkfifo gate; exec 3<>gate; sh -c 'read x < gate' | sh -c 'exit 7' & slow=$!; true & fast=$!; wait -n \"$slow\" \"$fast\"; echo >&3; wait \"$slow\"; printf 'slow-status=%s\\n' \"$?\"",
        b"slow-status=7\n",
    );
}

/// `$!` of a background pipeline stays waitable after its first stage exits.
#[test]
fn wait_by_original_pipeline_pid_survives_completed_first_stage() {
    matches_bash_in_a_fresh_directory(
        "mkfifo gate; exec 3<>gate; sh -c ': > first-done' | sh -c 'read x < gate; exit 7' & pid=$!; until test -e first-done; do sleep .01; done; echo >&3; wait \"$pid\"; printf 'status=%s\\n' \"$?\"",
        b"status=7\n",
    );
}

/// `wait -f` returns only when the job terminates, not when it stops.
#[cfg(unix)]
#[test]
fn wait_for_termination_matches_bash_for_stopped_job() {
    for option in ["-f", "-n -f"] {
        matches_bash_in_a_fresh_directory(
            &format!(
                "sh -c 'kill -STOP $$; exit 23' & pid=$!; sh -c \"until ps -o stat= -p $pid | grep -q T; do sleep .01; done; kill -CONT $pid\" & wait {option} -p done \"$pid\"; rc=$?; test \"$done\" = \"$pid\"; same=$?; printf '%s:%s' \"$rc\" \"$same\"; wait"
            ),
            b"23:0",
        );
    }
}

#[test]
fn wait_p_without_n_reports_last_waited_pid() {
    let script = "sh -c 'exit 7' & first=$!; sh -c 'exit 9' & second=$!; wait -p finished \"$first\" \"$second\"; rc=$?; test \"$finished\" = \"$second\"; same=$?; wait -p empty; printf '%s:%s:%s:%s' \"$rc\" \"$same\" \"$?\" \"${empty-unset}\"";
    let bash = isolated_command("bash")
        .arg("-c")
        .arg(script)
        .output()
        .unwrap();
    let brush = marsh()
        .args(["--no-config", "--noprofile", "--norc", "-c", script])
        .output()
        .unwrap();
    assert_eq!(
        brush.status.code(),
        bash.status.code(),
        "{:?}",
        brush.stderr
    );
    assert_eq!(brush.stdout, bash.stdout, "{:?}", brush.stderr);
}

#[cfg(unix)]
#[test]
fn term_trap_runs_while_shell_waits_for_more_input() {
    use std::io::Write as _;

    let directory = tempfile::tempdir().unwrap();
    let ready = directory.path().join("ready");
    let mut child = common::spawn_group(
        marsh()
            .args(["--no-config", "--noprofile", "--norc", "-s"])
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b"trap 'printf trapped; exit 143' TERM\nprintf ready > ready\n")
        .unwrap();
    common::wait_until("idle input", Duration::from_secs(20), || ready.exists());
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    // Stdin stays open: only the trap can end the shell.
    let output = common::wait_for_exit(child, "idle TERM trap", Duration::from_secs(20));
    drop(stdin);
    assert_eq!(output.status.code(), Some(143), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"trapped");
}

#[cfg(unix)]
#[test]
fn idle_term_trap_can_resume_reading_commands() {
    use std::io::Write as _;

    let directory = tempfile::tempdir().unwrap();
    let ready = directory.path().join("ready");
    let mut child = common::spawn_group(
        marsh()
            .args(["--no-config", "--noprofile", "--norc", "-s"])
            .current_dir(directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b"trap 'printf trapped; printf handled > handled' TERM\nprintf ready > ready\n")
        .unwrap();
    common::wait_until("idle input", Duration::from_secs(20), || ready.exists());
    stdin.write_all(b"printf af").unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let handled = directory.path().join("handled");
    common::wait_until("the idle TERM trap", Duration::from_secs(20), || {
        handled.exists()
    });
    stdin.write_all(b"ter\n").unwrap();
    drop(stdin);
    let output = common::wait_for_exit(child, "idle TERM trap resume", Duration::from_secs(20));
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"trappedafter");
}

/// A job that finished while the input loop read later lines is still
/// collected by `wait -n`.
#[test]
fn wait_n_keeps_a_completed_job_after_prompt_notification() {
    use std::io::Write as _;

    let directory = tempfile::tempdir().unwrap();
    let mut child = marsh()
        .args(["--no-config", "--noprofile", "--norc", "-s"])
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"sh -c ': > finishing; exit 7' & pid=$!\nuntil test -e finishing; do sleep .01; done\n:\nwait -n -p finished \"$pid\"\nprintf '%s:%s' \"$?\" \"$finished\"\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("7:"), "{stdout}");
    assert!(stdout[2..].parse::<u32>().is_ok(), "{stdout}");
}

/// A trapped TERM that arrives while the shell is blocked in `wait` runs the
/// trap and makes `wait` return 128+15, leaving `-p`'s variable unset.
#[cfg(unix)]
#[test]
fn trapped_term_interrupts_wait() {
    for (wait, expected) in [
        ("wait -n -p done \"$pid\"", "trapped143:unset"),
        ("wait \"$pid\"", "trapped143:unset"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        let mut command = marsh();
        command
            .current_dir(directory.path())
            .arg("-c")
            .arg(format!("trap 'printf trapped' TERM; sleep 300 & pid=$!; printf ready > ready; {wait}; rc=$?; printf '%s:%s' \"$rc\" \"${{done-unset}}\"; kill \"$pid\"; wait \"$pid\" 2>/dev/null; exit 0"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = common::spawn_group(&mut command);
        common::wait_until(wait, Duration::from_secs(20), || ready.exists());
        // Past `printf ready` the next blocking point is the wait itself.
        common::wait_until_blocked(child.id(), wait);
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
            nix::sys::signal::Signal::SIGTERM,
        )
        .unwrap();
        let output = common::wait_for_exit(child, wait, Duration::from_secs(20));
        assert_eq!(output.status.code(), Some(0), "{wait}: {output:?}");
        assert_eq!(output.stdout, expected.as_bytes(), "{wait}: {output:?}");
    }
}

#[test]
fn executes_an_ordinary_command_with_exact_streams_and_status() {
    let output = marsh()
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "printf 'hello\\n'; printf 'diagnostic\\n' >&2; exit 7",
        ])
        .output()
        .expect("run marsh");

    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"hello\n");
    assert_eq!(output.stderr, b"diagnostic\n");
}

#[test]
fn startup_history_disable_preserves_command_and_script_arguments() {
    let command = marsh::cli::parse(vec![
        "marsh".into(), "+o".into(), "history".into(), "-c".into(),
        "set -o | grep -Eq '^history[[:space:]]+off$' || exit 42; printf '%s|%s' \"$0\" \"$1\"; exit 23".into(),
        "name".into(), "arg".into(),
    ]).unwrap();
    let output = marsh()
        .args(command.brush_args.iter().skip(1))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(23));
    assert_eq!(output.stdout, b"name|arg");
    assert!(output.stderr.is_empty());

    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("history.sh");
    std::fs::write(
        &script,
        "set -o | grep -Eq '^history[[:space:]]+off$' || exit 42; printf '%s' \"$1\"",
    )
    .unwrap();
    let script = script.to_str().unwrap();
    let parsed = marsh::cli::parse(vec![
        "marsh".into(),
        "+o".into(),
        "history".into(),
        script.into(),
        "argument".into(),
    ])
    .unwrap();
    let output = marsh()
        .args(parsed.brush_args.iter().skip(1))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"argument");
    assert!(output.stderr.is_empty());
}

#[test]
fn supports_bash_pipeline_and_pipefail_behavior() {
    let output = marsh()
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "set -o pipefail; printf 'value\\n' | grep value | false",
        ])
        .output()
        .expect("run marsh");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn fanout_after_a_large_ordinary_program_parses() {
    let mut script = ":;\n".repeat(16_384);
    script.push_str("fanout { first: true; second: true } | collect\n");
    let output = composition_shell()
        .args(["--no-config", "--noprofile", "--norc", "-n", "-c", &script])
        .output()
        .expect("parse large ordinary program");

    assert!(output.status.success(), "{:?}", output.stderr);
}

#[test]
fn background_pid_jobs_and_wait_use_the_real_child() {
    let directory = tempfile::tempdir().unwrap();
    let output = marsh()
        .current_dir(directory.path())
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "mkfifo gate; sh -c 'read x < gate; exit 23' & pid=$!; test -n \"$pid\"; jobs -p >pids; jobs -l >long; grep -F \"$pid\" pids >/dev/null; grep -F \"$pid\" long >/dev/null; echo >gate; wait \"$pid\"; status=$?; rm pids long gate; exit \"$status\"",
        ])
        .output()
        .expect("run marsh");

    assert_eq!(output.status.code(), Some(23));
    assert!(output.stdout.is_empty());
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn background_pipeline_wait_returns_rightmost_status() {
    let output = marsh()
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "sh -c 'exit 7' | sh -c 'exit 23' & pid=$!; wait \"$pid\"; exit \"$?\"",
        ])
        .output()
        .expect("run marsh");

    assert_eq!(output.status.code(), Some(23));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn background_pipeline_pid_is_rightmost_and_survives_wait() {
    let directory = tempfile::tempdir().unwrap();
    let script = "sh -c 'exit 7' | sh -c 'printf %s \"$$\" > right-pid; exit 23' & pid=$!; while [ ! -s right-pid ]; do sleep 0.01; done; test \"$pid\" = \"$(cat right-pid)\" || exit 90; wait \"$pid\"; status=$?; test \"$!\" = \"$pid\" || exit 91; exit \"$status\"";
    for mut command in [isolated_command("bash"), marsh()] {
        let _ = std::fs::remove_file(directory.path().join("right-pid"));
        let output = command
            .current_dir(directory.path())
            .args(["--noprofile", "--norc", "-c", script])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(23), "{output:?}");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn foregrounding_background_pipeline_uses_its_original_process_group() {
    // Each stage exits 0 once its own group owns the terminal (`fg` worked),
    // or 9 after ten seconds.
    let stage = "sh -c 'i=0; until test \"$(ps -o tpgid= -p $$)\" -eq \"$(ps -o pgid= -p $$)\"; do i=$((i+1)); test $i -lt 1000 || exit 9; sleep .01; done'";
    let script = format!("{stage} | {stage} & fg; printf 'FG_STATUS=%s\\n' \"$?\"; wait");
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-brush-test-driver")] {
        let output = isolated_command("script")
            .args([
                "-q",
                "/dev/null",
                program,
                "--noprofile",
                "--norc",
                "-i",
                "-c",
                &script,
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{program}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("FG_STATUS=0"),
            "{program}: {output:?}"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn interactive_command_string_takes_terminal_for_foreground_commands() {
    // `script` gives the shell a controlling PTY as a session leader. Without
    // terminal control, the first foreground job stops on SIGTTOU and hangs.
    let script = "/bin/echo PTY_HI; /bin/echo one | /bin/cat; exit 3";
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-brush-test-driver")] {
        let child = common::spawn_group(
            isolated_command("script")
                .args([
                    "-q",
                    "/dev/null",
                    program,
                    "--noprofile",
                    "--norc",
                    "-i",
                    "-c",
                    script,
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        );
        let output = common::wait_for_exit(
            child,
            &format!("{program}: -i -c under a PTY"),
            Duration::from_secs(10),
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(3), "{program}: {output:?}");
        assert!(stdout.contains("PTY_HI"), "{program}: {output:?}");
        assert!(stdout.contains("one"), "{program}: {output:?}");
    }
}

#[test]
fn fanout_runs_pipelines_concurrently_and_collects_in_declaration_order() {
    // Each branch finishes only once the other has started (or gives up).
    let directory = tempfile::tempdir().unwrap();
    let output = composition_shell()
        .current_dir(directory.path())
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "printf input | fanout { first: sh -c 'touch first-ready; i=0; while test ! -e second-ready; do i=$((i+1)); test \"$i\" -lt 1000 || exit 41; sleep 0.01; done; printf one' | cat; second: sh -c 'touch second-ready; i=0; while test ! -e first-ready; do i=$((i+1)); test \"$i\" -lt 1000 || exit 42; sleep 0.01; done; printf input' | tr a-z A-Z } | collect --timing",
        ])
        .output()
        .expect("run fanout");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.find("== first").unwrap() < stdout.find("== second").unwrap());
    assert!(stdout.contains("one"));
    assert!(stdout.contains("INPUT"));
    assert!(stdout.contains("Timing:"));
    assert!(output.stderr.is_empty());
}

/// Runs a two-branch fanout whose branches only finish once both are running,
/// so every branch must really start. Returns false if it did not finish.
#[cfg(unix)]
fn rendezvous_fanout_finishes() -> bool {
    let directory = tempfile::tempdir().unwrap();
    let mut child = common::spawn_group(composition_shell()
        .current_dir(directory.path())
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "printf input | fanout { first: sh -c 'touch first-ready; i=0; while test ! -e second-ready; do i=$((i+1)); test \"$i\" -lt 600 || exit 41; sleep 0.01; done; printf one' | cat; second: (sh -c 'touch second-ready; i=0; while test ! -e first-ready; do i=$((i+1)); test \"$i\" -lt 600 || exit 42; sleep 0.01; done; printf input' | tr a-z A-Z) } | collect",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped()));
    // The branches give up after six seconds, so a run that outlives this is a
    // hang, not a slow machine.
    let deadline = std::time::Instant::now() + Duration::from_mins(1);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let output = common::wait_for_exit(child, "rendezvous fanout", Duration::from_secs(10));
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("one") && stdout.contains("INPUT"),
        "{output:?}"
    );
    true
}

/// A forked shell must be able to start threads before it runs anything.
/// `fork` landing while another thread was starting left the child holding
/// the standard library's thread registry lock with no owner, so its first
/// thread never started and the whole command hung (about one run in sixty on
/// a development machine, more under load). Launching many fanouts at once
/// makes that the common failure rather than a rare one.
#[cfg(unix)]
#[test]
fn forked_branch_shells_always_start_when_launched_concurrently() {
    const LAUNCHERS: usize = 6;
    const RUNS_EACH: usize = 16;
    let hung: usize = thread::scope(|scope| {
        let launchers: Vec<_> = (0..LAUNCHERS)
            .map(|_| {
                scope.spawn(|| {
                    (0..RUNS_EACH)
                        .filter(|_| !rendezvous_fanout_finishes())
                        .count()
                })
            })
            .collect();
        launchers.into_iter().map(|l| l.join().unwrap()).sum()
    });
    assert_eq!(
        hung,
        0,
        "{hung} of {} fanouts hung before every branch started",
        LAUNCHERS * RUNS_EACH
    );
}

#[test]
fn collect_renders_failed_branch_stderr_inline_and_propagates_required_failure() {
    // docs/fanout.md: a failed branch's stderr follows its header on stdout;
    // collect's own stderr stays empty.
    let directory = tempfile::tempdir().unwrap();
    let output = composition_shell()
        .current_dir(directory.path())
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "fanout { ok: printf good; bad: sh -c 'printf diagnostic >&2; exit 7' } | collect 2>diagnostics",
        ])
        .output()
        .expect("run fanout");

    assert_eq!(output.status.code(), Some(7));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("good"));
    assert!(output.stderr.is_empty());
    assert_eq!(
        std::fs::read_to_string(directory.path().join("diagnostics")).unwrap(),
        ""
    );
    let header = stdout.find("== bad (failed: 7) ==").expect("bad header");
    let section = stdout
        .find("== bad stderr ==\ndiagnostic")
        .expect("bad stderr");
    assert!(header < section, "{stdout}");
}

#[test]
fn composition_obeys_suffix_and_pipefail_statuses() {
    for (prefix, expected) in [("", 0), ("set -o pipefail; ", 1)] {
        let script = format!(
            "{prefix}fanout {{ ok: true; bad: false }} | collect | cat >/dev/null; set -- \"$?\" \"${{PIPESTATUS[*]}}\"; printf '%s' \"$2\"; exit \"$1\""
        );
        let output = composition_shell()
            .args(["--no-config", "--noprofile", "--norc", "-c", &script])
            .output()
            .expect("run composition status case");
        assert_eq!(output.status.code(), Some(expected));
        assert_eq!(output.stdout, b"1 1 0");
    }
}

/// Label-led branches (Brush `0041`): inside `fanout { ... }` a `;` starts a
/// new branch only before a `LABEL:` word; any other `;` sequences commands
/// inside the branch exactly as Bash does. Each branch's output and status
/// must equal GNU Bash running that branch's text.
#[test]
fn fanout_branches_are_label_led_and_bodies_match_bash() {
    let cases: &[(&str, &[(&str, &str)])] = &[
        (
            "a: echo a1; echo a2; b: echo b1; false",
            &[("a", "echo a1; echo a2"), ("b", "echo b1; false")],
        ),
        ("echo one; echo two", &[("echo", "echo one; echo two")]),
        (
            "a: { echo 1; echo 2; }; b: (echo 3; exit 4)",
            &[("a", "{ echo 1; echo 2; }"), ("b", "(echo 3; exit 4)")],
        ),
        (
            "a: true && echo and; false || echo or; b: false && echo never",
            &[
                ("a", "true && echo and; false || echo or"),
                ("b", "false && echo never"),
            ],
        ),
        (
            "a: x=y; case $x in y) echo matched;; *) echo no;; esac; echo after; b: echo b",
            &[
                (
                    "a",
                    "x=y; case $x in y) echo matched;; *) echo no;; esac; echo after",
                ),
                ("b", "echo b"),
            ],
        ),
        (
            "fix-lint: echo l; run_tests: echo t; echo a: b",
            &[("fix-lint", "echo l"), ("run_tests", "echo t; echo a: b")],
        ),
    ];
    for (body, expected) in cases {
        let script = format!("fanout {{ {body} }} | collect --json");
        let output = composition_shell()
            .args(["--no-config", "--noprofile", "--norc", "-c", &script])
            .output()
            .expect("run fanout");
        let report: serde_json::Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|e| panic!("{script:?}: {e}: {output:?}"));
        let branches = report["branches"].as_array().expect("branches");
        let labels = branches
            .iter()
            .map(|branch| branch["label"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            expected.iter().map(|(label, _)| *label).collect::<Vec<_>>(),
            "{script:?}"
        );
        for (branch, (label, text)) in branches.iter().zip(expected.iter()) {
            let bash = isolated_command("bash")
                .args(["--noprofile", "--norc", "-c", text])
                .output()
                .expect("run bash");
            assert_eq!(
                branch["stdout"].as_str().unwrap(),
                String::from_utf8_lossy(&bash.stdout),
                "{script:?} branch {label}"
            );
            assert_eq!(
                branch["status"].as_i64(),
                bash.status.code().map(i64::from),
                "{script:?} branch {label}"
            );
        }
    }
}

#[test]
fn failed_prefix_launches_no_fanout_branch() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("launched");
    let script = format!(
        "false | fanout {{ branch: touch {} }} | collect",
        marker.display()
    );
    let output = composition_shell()
        .args(["--no-config", "--noprofile", "--norc", "-c", &script])
        .output()
        .expect("run failed prefix");
    assert_eq!(output.status.code(), Some(1));
    assert!(!marker.exists());
}

#[test]
fn fanout_branches_share_the_current_project() {
    let directory = tempfile::tempdir().unwrap();
    let output = composition_shell()
        .current_dir(directory.path())
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "fanout { writer: printf shared > result; observer: sh -c 'i=0; until test -s result; do i=$((i+1)); test $i -lt 1000 || exit 9; sleep .01; done; cat result' } | collect",
        ])
        .output()
        .expect("run shared workspace fanout");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read(directory.path().join("result")).unwrap(),
        b"shared"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("shared"));
}

#[test]
fn fanout_enforces_branch_cap_and_json_rejects_non_utf8() {
    let branches = (0..17)
        .map(|index| format!("branch{index}: true"))
        .collect::<Vec<_>>()
        .join(", ");
    let script = format!("fanout {{ {branches} }} | collect");
    let over_cap = composition_shell()
        .args(["--no-config", "--noprofile", "--norc", "-c", &script])
        .output()
        .expect("run over-cap fanout");
    assert_eq!(over_cap.status.code(), Some(2));

    let binary = composition_shell()
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "printf '\\377' | fanout { cat } | collect --json",
        ])
        .output()
        .expect("run binary JSON fanout");
    assert!(!binary.status.success());
    assert!(binary.stdout.is_empty());
    assert!(String::from_utf8_lossy(&binary.stderr).contains("requires UTF-8"));
}

#[test]
fn fanout_enforces_prefix_and_combined_output_bounds_before_delivery() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("launched");
    let prefix_script = format!(
        "head -c 67108865 /dev/zero | fanout {{ branch: touch {} }} | collect",
        marker.display()
    );
    let prefix = composition_shell()
        .args(["--no-config", "--noprofile", "--norc", "-c", &prefix_script])
        .output()
        .expect("run over-limit fanout prefix");
    assert_eq!(prefix.status.code(), Some(2));
    assert!(!marker.exists());
    assert!(String::from_utf8_lossy(&prefix.stderr).contains("input exceeds 64 MiB"));

    // The branch would run for five minutes; collect must stop it at the bound.
    let child = common::spawn_group(
        composition_shell()
            .args([
                "--no-config",
                "--noprofile",
                "--norc",
                "-c",
                "fanout { noisy: sh -c 'head -c 17825792 /dev/zero; exec sleep 300' } | collect",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );
    let output = common::wait_for_exit(child, "over-limit fanout", Duration::from_mins(1));
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("combined output exceeds 16 MiB"));
}

#[test]
fn fanout_output_limit_stops_the_branch_process_tree() {
    let directory = tempfile::tempdir().unwrap();
    // Each branch's flood comes from a descendant of the command the branch
    // runs: one while that command is still running, one after it has exited
    // (a background grandchild keeps the branch's output open). Each records
    // the PID of a process that would otherwise live for five minutes.
    let cases = [
        (
            "running",
            "sh -c 'sleep 300 & echo $! > \"$0\"; head -c 17825792 /dev/zero; wait'",
        ),
        (
            "exited",
            "sh -c 'sh -c \"echo \\$\\$ > \\\"\\$0\\\"; head -c 17825792 /dev/zero; exec sleep 300\" \"$0\" &'",
        ),
    ];
    for (case, command) in cases {
        let pid_file = directory.path().join(case);
        let script = format!(
            "fanout {{ noisy: {command} {} }} | collect",
            pid_file.display()
        );
        let child = common::spawn_group(
            composition_shell()
                .args(["--no-config", "--noprofile", "--norc", "-c", &script])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        );
        let output = common::wait_for_exit(child, case, Duration::from_mins(1));
        assert!(!output.status.success(), "{case}");
        assert!(output.stdout.is_empty(), "{case}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("combined output exceeds 16 MiB"),
            "{case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap_or_else(|error| panic!("{case}: no descendant PID: {error}"))
            .trim()
            .parse()
            .unwrap();
        let pid = nix::unistd::Pid::from_raw(pid);
        let survived = std::iter::repeat_with(|| {
            let alive = nix::sys::signal::kill(pid, None).is_ok();
            if alive {
                thread::sleep(Duration::from_millis(20));
            }
            alive
        })
        .take(250)
        .all(|alive| alive);
        if survived {
            let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
        }
        assert!(
            !survived,
            "{case}: the stopped branch's descendant {pid} survived it"
        );
    }
}

#[test]
fn fanout_accepts_compact_commas_without_splitting_quoted_commas() {
    let output = composition_shell()
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "fanout { printf 'one,inside',printf two } | collect",
        ])
        .output()
        .expect("run compact comma fanout");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("one,inside"));
    assert!(stdout.contains("two"));
    assert!(stdout.contains("== printf-2 (complete) =="));
}

#[test]
fn composition_parser_preserves_ordinary_words_and_rejects_ambiguous_pipe_stderr() {
    let ordinary = composition_shell()
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "printf '<%s>\\n' before fanout { a,b } after",
        ])
        .output()
        .expect("run ordinary command containing fanout words");
    assert!(ordinary.status.success());
    assert_eq!(
        ordinary.stdout,
        b"<before>\n<fanout>\n<{>\n<a,b>\n<}>\n<after>\n"
    );

    let pipe_stderr = composition_shell()
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "fanout { printf value } |& collect",
        ])
        .output()
        .expect("run invalid composition pipe-stderr form");
    assert_eq!(pipe_stderr.status.code(), Some(2));

    let assignment = composition_shell()
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "fanout() { printf '%s:%s' \"$MARKER\" \"$*\"; }; MARKER=ordinary fanout { a,b }",
        ])
        .output()
        .expect("run assignment-prefixed ordinary fanout");
    assert!(assignment.status.success());
    assert_eq!(assignment.stdout, b"ordinary:{ a,b }");
}

#[test]
fn collect_help_redirects_and_invalid_options_launch_no_branch() {
    let directory = tempfile::tempdir().unwrap();
    let help = directory.path().join("help");
    let marker = directory.path().join("launched");
    let script = format!(
        "fanout {{ true }} | collect --help > {}; test -s {}",
        help.display(),
        help.display()
    );
    let redirected = composition_shell()
        .args(["--no-config", "--noprofile", "--norc", "-c", &script])
        .output()
        .expect("redirect collect help");
    assert!(redirected.status.success());
    assert!(redirected.stdout.is_empty());
    let help_text = std::fs::read_to_string(&help).unwrap();
    assert!(help_text.contains("64 MiB completed stdin"));
    assert!(help_text.contains("16 MiB combined branch stdout and stderr"));

    let invalid_script = format!(
        "fanout {{ branch: touch {} }} | collect --unknown",
        marker.display()
    );
    let invalid = composition_shell()
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            &invalid_script,
        ])
        .output()
        .expect("run invalid collect option");
    assert_eq!(invalid.status.code(), Some(2));
    assert!(!marker.exists());
}

#[cfg(unix)]
#[test]
fn ordinary_collect_resolves_functions_and_external_commands() {
    use std::os::unix::fs::PermissionsExt as _;

    let function = composition_shell()
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "collect() { printf 'function:%s' \"$1\"; }; collect value",
        ])
        .output()
        .expect("run collect function");
    assert!(function.status.success());
    assert_eq!(function.stdout, b"function:value");

    let interleaved = composition_shell()
        .args([
            "--no-config", "--noprofile", "--norc", "-c",
            "collect(){ printf '%s' \"$@\"; }; collect > /dev/null hidden; collect 2>/dev/null visible",
        ])
        .output().expect("run interleaved collect redirections");
    assert!(interleaved.status.success(), "{interleaved:?}");
    assert_eq!(interleaved.stdout, b"visible");
    assert!(interleaved.stderr.is_empty(), "{interleaved:?}");

    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("collect");
    std::fs::write(&executable, "#!/bin/sh\nprintf 'external:%s' \"$1\"\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let external = composition_shell()
        .env("PATH", directory.path())
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "collect value",
        ])
        .output()
        .expect("run external collect");
    assert!(external.status.success());
    assert_eq!(external.stdout, b"external:value");
}

#[test]
fn time_fanout_reports_shell_timing() {
    let output = composition_shell()
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "time fanout { printf value } | collect",
        ])
        .output()
        .expect("time fanout");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("value"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("real\t"));
    assert!(stderr.contains("user\t"));
    assert!(stderr.contains("sys\t"));
}

#[test]
fn collect_stdout_redirection_overrides_a_downstream_pipe() {
    let directory = tempfile::tempdir().unwrap();
    let output = composition_shell()
        .current_dir(directory.path())
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "fanout { printf value } | collect >captured | cat",
        ])
        .output()
        .expect("run redirected collect pipeline");
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(
        std::fs::read_to_string(directory.path().join("captured"))
            .unwrap()
            .contains("value")
    );
}

#[test]
fn background_fanout_is_one_waitable_shell_job() {
    let directory = tempfile::tempdir().unwrap();
    let result = directory.path().join("result");
    let script = format!(
        "fanout {{ a: sh -c 'sleep .1; printf a'; b: sh -c 'sleep .1; printf b' }} | collect > {} & wait; cat {}",
        result.display(),
        result.display()
    );
    let output = composition_shell()
        .args(["--no-config", "--noprofile", "--norc", "-c", &script])
        .output()
        .expect("run background fanout");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("== a (complete) =="));
    assert!(stdout.contains("== b (complete) =="));
}

/// Branch commands are grandchildren of the shell: once their branch shell is
/// gone, an exited one is reaped by init, not by marsh. A leaked branch is one
/// that keeps running.
#[cfg(unix)]
fn assert_branches_exit(members: &[i32]) {
    for &member in members {
        common::wait_until(
            &format!("fanout branch {member} to exit"),
            Duration::from_secs(10),
            || common::has_exited(member),
        );
    }
}

#[cfg(unix)]
#[test]
fn interrupt_cancels_every_fanout_branch() {
    let directory = tempfile::tempdir().unwrap();
    let pids = directory.path().join("pids");
    let script = format!(
        "fanout {{ a: sh -c 'echo $$ >> {0}; exec sleep 300'; b: sh -c 'echo $$ >> {0}; exec sleep 300' }} | collect",
        pids.display()
    );
    let mut command = composition_shell();
    command
        .args(["--no-config", "--noprofile", "--norc", "-c", &script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = common::spawn_group(&mut command);
    common::wait_until("both branches", Duration::from_secs(20), || {
        std::fs::read_to_string(&pids).is_ok_and(|contents| contents.lines().count() == 2)
    });
    let members = std::fs::read_to_string(&pids)
        .unwrap()
        .lines()
        .map(|value| value.parse::<i32>().unwrap())
        .collect::<Vec<_>>();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    let output = common::wait_for_exit(child, "SIGINT to fanout", Duration::from_secs(20));
    assert_eq!(output.status.code(), Some(130));
    assert_branches_exit(&members);
}

#[cfg(unix)]
#[test]
fn term_trap_cancels_every_fanout_branch() {
    let directory = tempfile::tempdir().unwrap();
    let pids = directory.path().join("pids");
    let script = format!(
        "trap 'printf trapped; exit 143' TERM; fanout {{ a: sh -c 'echo $$ >> {0}; exec sleep 300'; b: sh -c 'echo $$ >> {0}; exec sleep 300' }} | collect",
        pids.display()
    );
    let mut command = composition_shell();
    command
        .args(["--no-config", "--noprofile", "--norc", "-c", &script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = common::spawn_group(&mut command);
    common::wait_until("both branches", Duration::from_secs(20), || {
        std::fs::read_to_string(&pids).is_ok_and(|contents| contents.lines().count() == 2)
    });
    let members = std::fs::read_to_string(&pids)
        .unwrap()
        .lines()
        .map(|value| value.parse::<i32>().unwrap())
        .collect::<Vec<_>>();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let output = common::wait_for_exit(child, "TERM to fanout", Duration::from_secs(20));
    assert_eq!(output.status.code(), Some(143), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"trapped");
    assert_branches_exit(&members);
}

#[test]
fn flagless_command_works_with_an_isolated_home() {
    let root = std::env::temp_dir().join(format!("marsh-smoke-{}", std::process::id()));
    let config = root.join("config");
    std::fs::create_dir_all(&config).expect("create isolated config directory");

    let output = isolated_command(env!("CARGO_BIN_EXE_marsh-brush-test-driver"))
        .env_remove("BASH_ENV")
        .env_remove("ENV")
        .env("HOME", &root)
        .env("XDG_CONFIG_HOME", &config)
        .args(["-c", "printf 'flagless\\n'"])
        .output()
        .expect("run marsh");

    let _ = std::fs::remove_dir_all(&root);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"flagless\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn empty_bash_env_skips_noninteractive_startup_file() {
    let output = isolated_command(env!("CARGO_BIN_EXE_marsh-brush-test-driver"))
        .env("BASH_ENV", "")
        .env_remove("ENV")
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "printf executed",
        ])
        .output()
        .expect("run marsh");

    assert!(output.status.success());
    assert_eq!(output.stdout, b"executed");
    assert!(output.stderr.is_empty());
}

#[test]
fn bash_env_is_expanded_and_sourced_for_noninteractive_shell() {
    let root = std::env::temp_dir().join(format!("marsh-bash-env-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("create BASH_ENV test directory");
    std::fs::write(root.join("startup.sh"), b"STARTUP_VALUE=loaded\n")
        .expect("write BASH_ENV startup file");
    let output = isolated_command(env!("CARGO_BIN_EXE_marsh-brush-test-driver"))
        .env("HOME", &root)
        .env("BASH_ENV", "$HOME/startup.sh")
        .env_remove("ENV")
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "printf '%s' \"$STARTUP_VALUE\"",
        ])
        .output()
        .expect("run marsh");

    let _ = std::fs::remove_dir_all(&root);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"loaded");
    assert!(output.stderr.is_empty());
}

#[test]
fn reports_the_script_supplied_zero_argument() {
    let output = marsh()
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "printf '%s\\n' \"$0\"",
            "marsh",
        ])
        .output()
        .expect("run marsh");

    assert!(output.status.success());
    assert_eq!(output.stdout, b"marsh\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn compound_background_list_has_a_waitable_pid() {
    let directory = tempfile::tempdir().unwrap();
    let output = marsh()
        .current_dir(directory.path())
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "mkfifo gate; true && sh -c 'read x < gate; :' & pid=$!; test -n \"$pid\"; jobs -p | grep -Fx \"$pid\" >/dev/null; echo >gate; wait \"$pid\"",
        ])
        .output()
        .expect("run marsh");

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

/// Bracketed paste on: emitted once each time the shell starts reading a line.
#[cfg(unix)]
const READING_LINE: &str = "\x1b[?2004h";

/// An interactive shell on a fresh pseudo-terminal, under `script(1)` so the
/// terminal is its controlling terminal. A reader thread records output and,
/// when `answer_cursor` is set, answers cursor position queries (`ESC [ 6 n`)
/// as a terminal emulator would. Keys are written through `script`.
#[cfg(unix)]
struct PtyShell {
    program: String,
    input: std::sync::Arc<std::sync::Mutex<std::process::ChildStdin>>,
    /// `script`, leading its own process group. The shell under it leads
    /// the terminal's session, outside that group. `None` once reaped.
    child: Option<std::process::Child>,
    output: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    reader: Option<thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl PtyShell {
    fn spawn(program: &str, answer_cursor: bool) -> Self {
        Self::spawn_wrapped(program, answer_cursor, &[])
    }

    /// As [`Self::spawn`], with the shell run by `wrapper` (words before it).
    fn spawn_wrapped(program: &str, answer_cursor: bool, wrapper: &[&str]) -> Self {
        Self::spawn_with(program, answer_cursor, wrapper, &[])
    }

    /// As [`Self::spawn_wrapped`], with `options` added to the shell's own.
    fn spawn_with(program: &str, answer_cursor: bool, wrapper: &[&str], options: &[&str]) -> Self {
        use std::io::{Read as _, Write as _};
        if program == "bash" {
            common::require_bash_oracle();
        }

        let mut shell = wrapper.to_vec();
        shell.push(program);
        if program != "bash" {
            shell.push("--no-config");
        }
        shell.extend(["--noprofile", "--norc", "-i"]);
        shell.extend(options);
        let mut command = isolated_command("script");
        if cfg!(target_os = "linux") {
            // util-linux: one command string, run by $SHELL -c.
            command.args(["-q", "-e", "-c", &shell.join(" "), "/dev/null"]);
            command.env("SHELL", "/bin/sh");
        } else {
            command.args(["-q", "/dev/null"]).args(&shell);
        }
        let mut child = command
            .env("PS1", "PROMPT$ ")
            .env("TERM", "xterm")
            .env("LC_ALL", "C")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let input = std::sync::Arc::new(std::sync::Mutex::new(child.stdin.take().unwrap()));
        let output = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut reader = child.stdout.take().unwrap();
        let answerer = std::sync::Arc::clone(&input);
        let recorded = std::sync::Arc::clone(&output);
        let reader = thread::spawn(move || {
            let mut buffer = [0_u8; 4096];
            while let Ok(count @ 1..) = reader.read(&mut buffer) {
                let chunk = &buffer[..count];
                if answer_cursor && chunk.windows(4).any(|w| w == b"\x1b[6n") {
                    let _ = answerer.lock().unwrap().write_all(b"\x1b[1;1R");
                }
                recorded.lock().unwrap().extend_from_slice(chunk);
            }
        });
        Self {
            program: program.into(),
            input,
            child: Some(child),
            output,
            reader: Some(reader),
        }
    }

    fn output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    /// Lines the shell has started reading. Both shells switch bracketed paste
    /// on once per line read; Brush draws the prompt itself twice (again after
    /// its cursor position query), so counting prompts would run ahead.
    fn prompts(&self) -> usize {
        self.output().matches(READING_LINE).count()
    }

    /// Waits until the shell has started reading its `count`th line.
    fn expect_prompts(&mut self, count: usize) {
        self.expect(READING_LINE, count);
    }

    /// Waits until the output contains `needle` `count` times.
    fn expect(&mut self, needle: &str, count: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while self.output().matches(needle).count() < count {
            if std::time::Instant::now() > deadline {
                let output = self.output();
                self.kill();
                panic!("{}: no {needle:?} x{count} in {output:?}", self.program);
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        use std::io::Write as _;
        self.input.lock().unwrap().write_all(bytes).unwrap();
    }

    /// Asks the shell for its own PID; `prompts` is the prompt count that
    /// follows the answer.
    fn shell_pid(&mut self, prompts: usize) -> u32 {
        self.send(b"echo shell-pid=$$\r");
        self.expect_prompts(prompts);
        let output = self.output();
        let (_, tail) = output.rsplit_once("shell-pid=").unwrap();
        let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
        digits
            .parse()
            .unwrap_or_else(|_| panic!("no shell pid in {output:?}"))
    }

    /// Waits until the shell has no live child (an exited child may linger as a
    /// zombie until the shell reaps it, which is the shell's business).
    fn expect_children_exited(&mut self, shell: u32) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let table = Command::new("ps")
                .args(["-A", "-o", "pid=,ppid=,stat="])
                .output()
                .unwrap();
            let live = String::from_utf8_lossy(&table.stdout)
                .lines()
                .filter_map(|row| {
                    let mut fields = row.split_whitespace();
                    let (_, ppid, stat) = (fields.next()?, fields.next()?, fields.next()?);
                    (ppid.parse() == Ok(shell) && !stat.starts_with('Z')).then_some(())
                })
                .count();
            if live == 0 {
                return;
            }
            if std::time::Instant::now() > deadline {
                let output = self.output();
                self.kill();
                panic!(
                    "{}: children of {shell} still running: {output:?}",
                    self.program
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits until a child of `shell` owns the terminal's foreground group.
    fn expect_foreground_child(&mut self, shell: u32) {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            let table = Command::new("ps")
                .args(["-A", "-o", "ppid=,stat="])
                .output()
                .unwrap();
            if String::from_utf8_lossy(&table.stdout).lines().any(|row| {
                let mut fields = row.split_whitespace();
                fields.next().and_then(|ppid| ppid.parse().ok()) == Some(shell)
                    && fields.next().is_some_and(|stat| stat.contains('+'))
            }) {
                return;
            }
            if std::time::Instant::now() > deadline {
                let output = self.output();
                self.kill();
                panic!(
                    "{}: no foreground child of {shell}: {output:?}",
                    self.program
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Kills the shell and everything under `script` (whatever session or
    /// group it is in), then `script`'s group, and reaps `script`.
    fn kill(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        common::kill_descendants(child.id());
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = child.wait();
    }

    /// Waits for the shell to exit and returns its status and output.
    fn finish(mut self) -> (Option<i32>, String) {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                self.child = None;
                // `script` has exited, so the reader reaches end of file.
                self.reader.take().unwrap().join().unwrap();
                return (status.code(), self.output());
            }
            if std::time::Instant::now() > deadline {
                let output = self.output();
                self.kill();
                panic!("{}: shell did not exit: {output:?}", self.program);
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(unix)]
impl Drop for PtyShell {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Ctrl-C typed while an interactive shell waits for a foreground job that
/// dies of SIGINT abandons the rest of the command line (later list items and
/// loop iterations), as in Bash, and `$?` is 130.
#[cfg(unix)]
#[test]
fn interactive_ctrl_c_abandons_the_rest_of_the_command_line() {
    // `external` lines are waiting on a foreground child when ^C is typed; the
    // other is the shell's own loop.
    for (line, external) in [
        (
            "printf '%s\\n' st''arted; for i in 1 2 3; do sleep 30; echo it$i; done; echo loop''done",
            true,
        ),
        ("printf '%s\\n' st''arted; sleep 30; echo aft''er", true),
        (
            "printf '%s\\n' st''arted; while :; do :; done; echo aft''er",
            false,
        ),
        (
            "printf '%s\\n' st''arted; f() { sleep 30; echo in''f; }; f; echo aft''er",
            true,
        ),
    ] {
        for program in ["bash", env!("CARGO_BIN_EXE_marsh-brush-test-driver")] {
            let mut shell = PtyShell::spawn(program, true);
            shell.expect_prompts(1);
            let pid = shell.shell_pid(2);
            shell.send(format!("{line}\r").as_bytes());
            shell.expect("started\r\n", 1);
            if external {
                shell.expect_foreground_child(pid);
            }
            // Typed-ahead input is safe once the next prompt is drawn.
            let prompts = shell.prompts();
            shell.send(b"\x03");
            shell.expect_prompts(prompts + 1);
            shell.send(b"echo rc=$?\r");
            shell.expect("rc=130", 1);
            shell.send(b"exit 3\r");
            let (status, output) = shell.finish();
            assert_eq!(status, Some(3), "{program}: {line}: {output:?}");
            for absent in ["it1", "loopdone", "after", "inf"] {
                assert!(!output.contains(absent), "{program}: {line}: {output:?}");
            }
            // The prompt after ^C starts on a fresh line.
            assert!(output.contains("^C\r\n"), "{program}: {line}: {output:?}");
        }
    }
}

/// A terminal that never answers the cursor position query does not end the
/// interactive session: lines are still read and run (Bash never asks), and
/// the first line, typed while the shell waits for the answer, is not lost.
#[cfg(unix)]
#[test]
fn interactive_shell_survives_a_terminal_that_never_reports_the_cursor() {
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-brush-test-driver")] {
        let mut shell = PtyShell::spawn(program, false);
        shell.expect_prompts(1);
        shell.send(b"echo o''ne\r");
        shell.expect("one\r\n", 1);
        shell.send(b"for i in a b; do\r");
        shell.send(b"echo x$i; done\r");
        shell.expect("xb\r\n", 1);
        shell.send(b"exit 4\r");
        let (status, output) = shell.finish();
        assert_eq!(status, Some(4), "{program}: {output:?}");
        assert!(output.contains("xa\r\n"), "{program}: {output:?}");
    }
}

/// A registered command runs as a separate process reached through the
/// session command directory on `PATH`, so `type`/`command` describe it as
/// that file (as Bash would describe the link) rather than as a builtin.
#[test]
fn type_describes_registered_commands_as_their_path_link() {
    let directory = tempfile::tempdir().unwrap();
    let link = directory.path().join("place_probe");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_marsh-brush-test-driver"), &link).unwrap();
    let link = link.to_str().unwrap();
    let script = "type place_probe; type -t place_probe; command -v place_probe; type -a place_probe; type -P place_probe; printf x | place_probe";
    let output = marsh()
        .env("MARSH_TEST_REGISTER_PLACE", "1")
        .env("MARSH_TEST_EXTERNAL_COMMANDS", directory.path())
        .args(["--no-config", "--noprofile", "--norc", "-c", script])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!(
            "place_probe is {link}\nfile\n{link}\nplace_probe is {link}\n{link}\nlocal:place_probe:x"
        )
    );
    // Without a link on PATH there is no file to name; it stays a builtin.
    let output = marsh()
        .env("MARSH_TEST_REGISTER_PLACE", "1")
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "type -t place_probe",
        ])
        .output()
        .unwrap();
    assert_eq!(output.stdout, b"builtin\n", "{output:?}");
}

/// Interactive job-control lines match Bash byte for byte (PIDs aside): the
/// launch line is `[1] PID` (no `+`, no tab), completion notifications are
/// `[1]+  Done<pad>CMD` or `Exit N`, `jobs` lists `Running<pad>CMD &`, and job
/// numbers are reused once earlier jobs are reported (patch `0043`).
#[cfg(unix)]
#[test]
fn interactive_job_launch_and_completion_lines_match_bash() {
    fn job_lines(output: &str) -> Vec<String> {
        // Drop terminal escapes (`ESC [ ... final`, `ESC 7`) between lines.
        let mut plain = String::new();
        let mut chars = output.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                plain.push(c);
            } else if chars.next() == Some('[') {
                while chars.next().is_some_and(|c| !('@'..='~').contains(&c)) {}
            }
        }
        plain
            .split(['\r', '\n'])
            .filter(|line| {
                line.starts_with('[') && line.as_bytes().get(1).is_some_and(u8::is_ascii_digit)
            })
            .map(|line| {
                // `[N] PID` -> `[N] PID` with the PID normalised.
                match line.split_once("] ") {
                    Some((head, pid)) if pid.bytes().all(|b| b.is_ascii_digit()) => {
                        format!("{head}] PID")
                    }
                    _ => line.to_owned(),
                }
            })
            .collect()
    }
    let mut seen = Vec::new();
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-brush-test-driver")] {
        let mut shell = PtyShell::spawn(program, true);
        shell.expect_prompts(1);
        let pid = shell.shell_pid(2);
        // Each notification is read only after the real condition: every job
        // the shell started has exited. (Fixed sleeps raced a loaded machine:
        // `sh -c 'exit 3'` had not run yet when `true` asked for the report.)
        shell.send(b"sleep 1 &\r");
        shell.expect_prompts(3);
        shell.expect_children_exited(pid);
        shell.send(b"true\r");
        shell.expect("Done", 1);
        shell.expect_prompts(4);
        shell.send(b"sh -c 'exit 3' &\r");
        shell.expect_prompts(5);
        shell.expect_children_exited(pid);
        shell.send(b"true\r");
        shell.expect("Exit 3", 1);
        shell.expect_prompts(6);
        shell.send(b"sleep 2 & sleep 2 &\r");
        shell.expect_prompts(7);
        shell.send(b"jobs\r");
        shell.expect("Running", 2);
        shell.expect_prompts(8);
        shell.expect_children_exited(pid);
        shell.send(b"true\r");
        shell.expect("Done", 3);
        shell.expect_prompts(9);
        shell.send(b"exit 0\r");
        let (status, output) = shell.finish();
        assert_eq!(status, Some(0), "{program}: {output:?}");
        seen.push((program, job_lines(&output), output));
    }
    let (_, bash, bash_output) = &seen[0];
    assert_eq!(
        bash,
        &[
            "[1] PID",
            "[1]+  Done                       sleep 1",
            "[1] PID",
            "[1]+  Exit 3                     sh -c 'exit 3'",
            "[1] PID",
            "[2] PID",
            "[1]-  Running                    sleep 2 &",
            "[2]+  Running                    sleep 2 &",
            "[1]-  Done                       sleep 2",
            "[2]+  Done                       sleep 2",
        ],
        "Bash oracle: {bash_output:?}"
    );
    let (program, brush, brush_output) = &seen[1];
    assert_eq!(brush, bash, "{program}: {brush_output:?}");
}

/// A background job that writes lines while the shell sits at the prompt gets
/// the terminal's output processing (`ONLCR`): every line starts at column 0,
/// as under Bash's readline, instead of stair-stepping (patch `0044`).
#[cfg(unix)]
#[test]
fn background_output_at_the_prompt_keeps_terminal_newline_translation() {
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-brush-test-driver")] {
        let mut shell = PtyShell::spawn(program, true);
        shell.expect_prompts(1);
        // The job writes once the line editor has put the terminal in
        // noncanonical mode to wait for input.
        shell.send(b"(until stty -a </dev/tty | grep -q -- -icanon; do sleep .01; done; printf 'al''pha\\nbe''ta\\nga''mma\\n') &\r");
        shell.expect_prompts(2);
        shell.expect("gamma", 1);
        shell.send(b"exit 0\r");
        let (status, output) = shell.finish();
        assert_eq!(status, Some(0), "{program}: {output:?}");
        assert!(
            output.contains("alpha\r\nbeta\r\ngamma\r\n"),
            "{program}: lines do not start at column 0: {output:?}"
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
        let output = isolated_command(env!("CARGO_BIN_EXE_marsh-local"))
            .args(["--no-config", "--noprofile", "--norc", "-c", script])
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(0), "{script}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            expected,
            "{script}: {output:?}"
        );
    }
}

/// Lines typed ahead with line feeds (a pty driver, `expect`, or a paste
/// without bracketed paste) all run: the line editor submits the first and
/// keeps the rest for the next prompts, as Bash's readline does (patch `0054`).
#[cfg(unix)]
#[test]
fn lines_typed_ahead_with_line_feeds_all_run() {
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-brush-test-driver")] {
        let mut shell = PtyShell::spawn(program, true);
        shell.expect_prompts(1);
        shell.send(b"echo one-$((0+1))\necho two-$((1+1))\necho three-$((1+2))\nexit 0\n");
        let (status, output) = shell.finish();
        assert_eq!(status, Some(0), "{program}: {output:?}");
        for line in ["one-1", "two-2", "three-3"] {
            assert!(
                output.contains(line),
                "{program}: {line} did not run: {output:?}"
            );
        }
    }
}

/// A background job that has exited is reported at the next prompt even
/// before the shell's runtime relays its `SIGCHLD`: the prompt's job check
/// asks the kernel (patch `0068`). Before, it waited for the relay, and the
/// `[1]+  Done` line could miss its prompt. Here the shell starts with
/// `SIGCHLD` blocked, so the relay never comes; the command line waits with
/// builtins only (`/proc`) until the job is a zombie (Bash reaps it first).
#[cfg(target_os = "linux")]
#[test]
fn an_exited_job_is_reported_at_the_next_prompt_without_sigchld() {
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-brush-test-driver")] {
        // `script` resets its child's signal mask, so a wrapper blocks SIGCHLD
        // and execs the shell.
        let wrapper = [
            "perl",
            "-MPOSIX",
            "-e",
            "'sigprocmask(SIG_BLOCK, POSIX::SigSet->new(SIGCHLD)) or die; exec @ARGV or die'",
        ];
        let mut shell = PtyShell::spawn_wrapped(program, true, &wrapper);
        shell.expect_prompts(1);
        shell.send(b"sh -c 'exit 3' & p=$!\r");
        shell.expect_prompts(2);
        shell.send(
            b"until [[ ! -e /proc/$p ]] || { read -r l < /proc/$p/stat && [[ $l == *') Z '* ]]; } 2>/dev/null; do :; done\r",
        );
        shell.expect_prompts(3);
        shell.send(b"true\r");
        shell.expect("Exit 3", 1);
        shell.expect_prompts(4);
        shell.send(b"exit 0\r");
        let (status, output) = shell.finish();
        assert_eq!(status, Some(0), "{program}: {output:?}");
    }
}

/// An interactive login shell with `huponexit` sends SIGHUP to its jobs as it
/// exits, except a job marked by `disown -h`, as Bash does.
#[cfg(unix)]
#[test]
fn huponexit_hangs_up_jobs_except_those_disowned_with_h() {
    fn alive(pid: i32) -> bool {
        let table = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&table.stdout);
        !stat.trim().is_empty() && !stat.trim_start().starts_with('Z')
    }
    fn number_after(output: &str, label: &str) -> i32 {
        let (_, tail) = output.rsplit_once(label).unwrap();
        let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
        digits
            .parse()
            .unwrap_or_else(|_| panic!("no {label} in {output:?}"))
    }
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-brush-test-driver")] {
        let mut shell = PtyShell::spawn_with(program, true, &[], &["-l"]);
        shell.expect_prompts(1);
        shell.send(
            b"shopt -s huponexit; sleep 60 & echo hup=$!; sleep 60 & echo kept=$!; disown -h\r",
        );
        shell.expect("kept=", 2);
        shell.expect_prompts(2);
        let output = shell.output();
        let (hup, kept) = (
            number_after(&output, "hup="),
            number_after(&output, "kept="),
        );
        shell.send(b"exit\r");
        let (status, output) = shell.finish();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while alive(hup) && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let (hup_alive, kept_alive) = (alive(hup), alive(kept));
        for pid in [hup, kept] {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        assert_eq!(status, Some(0), "{program}: {output:?}");
        assert!(!hup_alive, "{program}: the job got no SIGHUP: {output:?}");
        assert!(
            kept_alive,
            "{program}: the `disown -h` job got SIGHUP: {output:?}"
        );
    }
}

/// One TERM runs its trap once. On a single-threaded runtime the runtime only
/// dispatches a signal when the shell next waits, so `kill -TERM $$` has its
/// trap run at the command boundary first, from the handler's record, and the
/// late dispatch reaches the idle input read afterwards; that dispatch must
/// not run the trap again (patch `0071a`; it printed `TAT`).
#[cfg(unix)]
#[test]
fn a_term_trap_delivered_at_a_command_boundary_runs_once() {
    use std::io::Write as _;
    let mut child = common::spawn_group(
        marsh()
            .env("MARSH_TEST_CURRENT_THREAD_RUNTIME", "1")
            .args(["--no-config", "--noprofile", "--norc", "-s"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b"trap 'printf T' TERM\nkill -TERM $$\nprintf A\n")
        .unwrap();
    drop(stdin);
    let output = common::wait_for_exit(child, "TERM trap once", Duration::from_secs(20));
    assert_eq!(output.stdout, b"TA", "{:?}", output.stderr);
    assert_eq!(output.status.code(), Some(0));
}
