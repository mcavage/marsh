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
    let mut command = Command::new(program);
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

#[test]
fn descendant_command_without_relay_fails_closed() {
    let directory = tempfile::tempdir().unwrap();
    let link = directory.path().join("fixture");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_marsh"), &link).unwrap();
    let output = isolated_command(link)
        .env("MARSH_EXTERNAL_SESSION", "{}")
        .env_remove("MARSH_DAEMON_SOCKET")
        .env_remove("MARSH_DAEMON_TOKEN")
        .arg("identity")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&output.stderr).contains("attached marsh session"));
}

#[cfg(unix)]
#[test]
fn foreground_interrupt_invokes_registered_int_trap() {
    let directory = tempfile::tempdir().unwrap();
    let ready = directory.path().join("ready");
    let mut command = marsh();
    command
        .current_dir(directory.path())
        .arg("-c")
        .arg("trap 'printf trapped; exit 130' INT; sh -c 'printf %s $$ > ready; exec sleep 300'")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().unwrap();
    for _ in 0..250 {
        if ready.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(ready.exists(), "shell never reached foreground wait");
    let child_pid = std::fs::read_to_string(&ready)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let shell_pid = i32::try_from(child.id()).unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(shell_pid),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    // Give the shell's signal listener an exclusive-ready window before the
    // child exit also becomes selectable; otherwise the test races those two
    // readiness events instead of testing deferred trap delivery.
    thread::sleep(Duration::from_millis(50));
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child_pid),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    for _ in 0..100 {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(130));
    assert_eq!(output.stdout, b"trapped");
}

#[cfg(unix)]
#[test]
fn foreground_term_invokes_registered_term_trap_after_child_finishes() {
    let directory = tempfile::tempdir().unwrap();
    let ready = directory.path().join("ready");
    let mut command = marsh();
    command
        .current_dir(directory.path())
        .arg("-c")
        .arg("trap 'printf trapped; exit 143' TERM; sh -c 'printf %s $$ > ready; exec sleep 300'")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().unwrap();
    for _ in 0..250 {
        if ready.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(ready.exists(), "shell never reached foreground wait");
    let child_pid = std::fs::read_to_string(&ready)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let shell_pid = i32::try_from(child.id()).unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(shell_pid),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    thread::sleep(Duration::from_millis(50));
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child_pid),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    for _ in 0..100 {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if child.try_wait().unwrap().is_none() {
        let _ = child.kill();
        panic!("shell did not finish after TERM");
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(143), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"trapped");
}

#[test]
fn wait_n_returns_first_child_status_and_pid() {
    let output = marsh()
        .args(["--no-config", "--noprofile", "--norc", "-c", "sh -c 'sleep .05; exit 23' & a=$!; sh -c 'sleep .3; exit 17' & b=$!; wait -n -p won \"$a\" \"$b\"; rc=$?; printf '%s:%s' \"$rc\" \"$won\"; wait \"$b\""])
        .output().unwrap();
    assert_eq!(output.status.code(), Some(17), "{:?}", output.stderr);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("23:"), "{stdout}");
    assert!(stdout[3..].parse::<u32>().is_ok(), "{stdout}");
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

#[test]
fn wait_n_preserves_unselected_pipeline_status_after_prompt_poll() {
    let script = "sleep .4 | sh -c 'exit 7' & slow=$!; sleep .1 & fast=$!; wait -n \"$slow\" \"$fast\"; sleep .6; wait \"$slow\"; printf 'slow-status=%s\\n' \"$?\"";
    let bash = isolated_command("bash")
        .arg("-c")
        .arg(script)
        .output()
        .unwrap();
    let brush = marsh()
        .args(["--no-config", "--noprofile", "--norc", "-c", script])
        .output()
        .unwrap();
    assert!(bash.status.success(), "{:?}", bash.stderr);
    assert_eq!(bash.stdout, b"slow-status=7\n");
    assert_eq!(brush.status, bash.status, "{:?}", brush.stderr);
    assert_eq!(brush.stdout, bash.stdout, "{:?}", brush.stderr);
}

#[test]
fn wait_by_original_pipeline_pid_survives_completed_first_stage() {
    let script = "sh -c 'exit 0' | sh -c 'sleep .3; exit 7' & pid=$!; sleep .1; wait \"$pid\"; printf 'status=%s\\n' \"$?\"";
    let bash = isolated_command("bash")
        .arg("-c")
        .arg(script)
        .output()
        .unwrap();
    let brush = marsh()
        .args(["--no-config", "--noprofile", "--norc", "-c", script])
        .output()
        .unwrap();
    assert!(bash.status.success(), "{:?}", bash.stderr);
    assert_eq!(bash.stdout, b"status=7\n");
    assert_eq!(brush.status, bash.status, "{:?}", brush.stderr);
    assert_eq!(brush.stdout, bash.stdout, "{:?}", brush.stderr);
}

#[cfg(unix)]
#[test]
fn wait_for_termination_matches_bash_for_stopped_job() {
    for option in ["-f", "-n -f"] {
        let script = format!(
            "sh -c 'kill -STOP $$; exit 23' & pid=$!; sh -c \"sleep .1; kill -CONT $pid\" & wait {option} -p done \"$pid\"; rc=$?; test \"$done\" = \"$pid\"; same=$?; printf '%s:%s' \"$rc\" \"$same\"; wait"
        );
        let bash = isolated_command("bash")
            .arg("-c")
            .arg(&script)
            .output()
            .unwrap();
        assert!(bash.status.success(), "{:?}", bash.stderr);
        let brush = marsh()
            .args(["--no-config", "--noprofile", "--norc", "-c", &script])
            .output()
            .unwrap();
        assert_eq!(
            brush.status.code(),
            bash.status.code(),
            "{option}: {:?}",
            brush.stderr
        );
        assert_eq!(brush.stdout, bash.stdout, "{option}: {:?}", brush.stderr);
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
    let mut child = marsh()
        .args(["--no-config", "--noprofile", "--norc", "-s"])
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b"trap 'printf trapped; exit 143' TERM\nprintf ready > ready\n")
        .unwrap();
    for _ in 0..250 {
        if ready.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(ready.exists(), "shell never reached idle input");
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    for _ in 0..100 {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    if child.try_wait().unwrap().is_none() {
        let _ = child.kill();
        panic!("shell did not run idle TERM trap");
    }
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(143), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"trapped");
}

#[cfg(unix)]
#[test]
fn idle_term_trap_can_resume_reading_commands() {
    use std::io::Write as _;

    let directory = tempfile::tempdir().unwrap();
    let ready = directory.path().join("ready");
    let mut child = marsh()
        .args(["--no-config", "--noprofile", "--norc", "-s"])
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(b"trap 'printf trapped; printf handled > handled' TERM\nprintf ready > ready\n")
        .unwrap();
    for _ in 0..250 {
        if ready.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(ready.exists(), "shell never reached idle input");
    stdin.write_all(b"printf af").unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    let handled = directory.path().join("handled");
    for _ in 0..250 {
        if handled.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(handled.exists(), "idle TERM trap did not run");
    stdin.write_all(b"ter\n").unwrap();
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"trappedafter");
}

#[test]
fn wait_n_keeps_a_completed_job_after_prompt_notification() {
    use std::io::Write as _;

    let mut child = marsh()
        .args(["--no-config", "--noprofile", "--norc", "-s"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"sh -c 'exit 7' & pid=$!\nsleep .1\nwait -n -p finished \"$pid\"\nprintf '%s:%s' \"$?\" \"$finished\"\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("7:"), "{stdout}");
    assert!(stdout[2..].parse::<u32>().is_ok(), "{stdout}");
}

#[cfg(unix)]
#[test]
fn trapped_term_interrupts_wait_n_and_leaves_pid_variable_unset() {
    let directory = tempfile::tempdir().unwrap();
    let ready = directory.path().join("ready");
    let mut command = marsh();
    command
        .current_dir(directory.path())
        .arg("-c")
        .arg("trap 'printf trapped' TERM; sleep 2 & pid=$!; printf ready > ready; wait -n -p done \"$pid\"; rc=$?; printf '%s:%s' \"$rc\" \"${done-unset}\"; kill \"$pid\"; wait \"$pid\" 2>/dev/null; exit 0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().unwrap();
    for _ in 0..250 {
        if ready.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(ready.exists(), "shell never reached wait -n");
    thread::sleep(Duration::from_millis(50));
    let shell_pid = i32::try_from(child.id()).unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(shell_pid),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    for _ in 0..200 {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if child.try_wait().unwrap().is_none() {
        let _ = child.kill();
        panic!("shell did not leave interrupted wait -n");
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"trapped143:unset");
}

#[cfg(unix)]
#[test]
fn trapped_term_interrupts_wait_for_pid() {
    let directory = tempfile::tempdir().unwrap();
    let ready = directory.path().join("ready");
    let mut command = marsh();
    command
        .current_dir(directory.path())
        .arg("-c")
        .arg("trap 'printf trapped' TERM; sleep 2 & pid=$!; printf ready > ready; wait \"$pid\"; rc=$?; printf '%s' \"$rc\"; kill \"$pid\"; wait \"$pid\" 2>/dev/null; exit 0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().unwrap();
    for _ in 0..250 {
        if ready.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(ready.exists(), "shell never reached wait PID");
    thread::sleep(Duration::from_millis(50));
    let shell_pid = i32::try_from(child.id()).unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(shell_pid),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    for _ in 0..200 {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if child.try_wait().unwrap().is_none() {
        let _ = child.kill();
        panic!("shell did not leave interrupted wait PID");
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"trapped143");
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
fn extension_scan_is_linear_for_a_large_ordinary_program() {
    let mut script = ":;\n".repeat(16_384);
    script.push_str("fanout { first: true; second: true } | collect\n");
    let started = std::time::Instant::now();
    let output = composition_shell()
        .args(["--no-config", "--noprofile", "--norc", "-n", "-c", &script])
        .output()
        .expect("parse large ordinary program");

    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "fanout extension scan regressed to prefix-rescanning behavior: {:?}",
        started.elapsed()
    );
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
            "sh -c 'sleep 0.05; exit 23' & pid=$!; test -n \"$pid\"; jobs -p >pids; jobs -l >long; grep -F \"$pid\" pids >/dev/null; grep -F \"$pid\" long >/dev/null; wait \"$pid\"; status=$?; rm pids long; exit \"$status\"",
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
    let script = "sh -c 'exit 7' | sh -c 'printf %s \"$$\" > right-pid; sleep 0.05; exit 23' & pid=$!; while [ ! -s right-pid ]; do sleep 0.01; done; test \"$pid\" = \"$(cat right-pid)\" || exit 90; wait \"$pid\"; status=$?; test \"$!\" = \"$pid\" || exit 91; exit \"$status\"";
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
    let script = "sleep 0.2 | sleep 0.2 & fg; printf 'FG_STATUS=%s\\n' \"$?\"; wait";
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
                script,
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
        let mut child = isolated_command("script")
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
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() > deadline {
                // Kill the whole group so a stopped shell or job cannot linger.
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
                    nix::sys::signal::Signal::SIGKILL,
                );
                let output = child.wait_with_output().unwrap();
                panic!("{program}: -i -c hung under a PTY: {output:?}");
            }
            thread::sleep(Duration::from_millis(20));
        }
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(3), "{program}: {output:?}");
        assert!(stdout.contains("PTY_HI"), "{program}: {output:?}");
        assert!(stdout.contains("one"), "{program}: {output:?}");
    }
}

#[test]
fn fanout_runs_pipelines_concurrently_and_collects_in_declaration_order() {
    let directory = tempfile::tempdir().unwrap();
    let started = std::time::Instant::now();
    let output = composition_shell()
        .current_dir(directory.path())
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "printf input | fanout { first: sh -c 'touch first-ready; i=0; while test ! -e second-ready; do i=$((i+1)); test \"$i\" -lt 300 || exit 41; sleep 0.01; done; printf one' | cat; second: sh -c 'touch second-ready; i=0; while test ! -e first-ready; do i=$((i+1)); test \"$i\" -lt 300 || exit 42; sleep 0.01; done; printf input' | tr a-z A-Z } | collect --timing",
        ])
        .output()
        .expect("run fanout");

    assert!(output.status.success());
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.find("== first").unwrap() < stdout.find("== second").unwrap());
    assert!(stdout.contains("one"));
    assert!(stdout.contains("INPUT"));
    assert!(stdout.contains("Timing:"));
    assert!(output.stderr.is_empty());
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
            let bash = Command::new("bash")
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
            "fanout { writer: printf shared > result; observer: sleep 0.1 | cat result } | collect",
        ])
        .output()
        .expect("run shared workspace fanout");
    assert!(output.status.success());
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

    let started = std::time::Instant::now();
    let output = composition_shell()
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "fanout { noisy: sh -c 'head -c 17825792 /dev/zero; sleep 300' } | collect",
        ])
        .output()
        .expect("run over-limit combined fanout output");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("combined output exceeds 16 MiB"));
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

#[cfg(unix)]
#[test]
fn interrupt_cancels_and_reaps_all_fanout_branches() {
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
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().unwrap();
    for _ in 0..100 {
        if std::fs::read_to_string(&pids).is_ok_and(|contents| contents.lines().count() == 2) {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
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
    for _ in 0..100 {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
        panic!("shell did not propagate a direct SIGINT to fanout branches");
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(130));
    for member in members {
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(member), None).is_err(),
            "fanout leaked child {member}"
        );
    }
}

#[cfg(unix)]
#[test]
fn term_trap_cancels_and_reaps_all_fanout_branches() {
    let directory = tempfile::tempdir().unwrap();
    let pids = directory.path().join("pids");
    let script = format!(
        "trap 'printf trapped; exit 143' TERM; fanout {{ a: sh -c 'echo $$ >> {0}; exec sleep 2'; b: sh -c 'echo $$ >> {0}; exec sleep 2' }} | collect",
        pids.display()
    );
    let mut command = composition_shell();
    command
        .args(["--no-config", "--noprofile", "--norc", "-c", &script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().unwrap();
    for _ in 0..100 {
        if std::fs::read_to_string(&pids).is_ok_and(|contents| contents.lines().count() == 2) {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
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
    for _ in 0..100 {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
        panic!("shell did not propagate TERM to fanout branches");
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(143), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"trapped");
    for member in members {
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(member), None).is_err(),
            "fanout leaked child {member}"
        );
    }
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
    let output = marsh()
        .args([
            "--no-config",
            "--noprofile",
            "--norc",
            "-c",
            "true && sleep 0.05 & pid=$!; test -n \"$pid\"; jobs -p | grep -Fx \"$pid\" >/dev/null; wait \"$pid\"",
        ])
        .output()
        .expect("run marsh");

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

/// An interactive shell on a fresh pseudo-terminal, under `script(1)` so the
/// terminal is its controlling terminal. A reader thread records output and,
/// when `answer_cursor` is set, answers cursor position queries (`ESC [ 6 n`)
/// as a terminal emulator would. Keys are written through `script`.
#[cfg(unix)]
struct PtyShell {
    program: String,
    input: std::sync::Arc<std::sync::Mutex<std::process::ChildStdin>>,
    child: std::process::Child,
    output: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

#[cfg(unix)]
impl PtyShell {
    fn spawn(program: &str, answer_cursor: bool) -> Self {
        use std::io::{Read as _, Write as _};

        let mut shell = vec![program];
        if program != "bash" {
            shell.push("--no-config");
        }
        shell.extend(["--noprofile", "--norc", "-i"]);
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
        thread::spawn(move || {
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
            child,
            output,
        }
    }

    fn output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
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
        self.expect("PROMPT$ ", prompts);
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

    fn kill(&mut self) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(i32::try_from(self.child.id()).unwrap()),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = self.child.wait();
    }

    /// Waits for the shell to exit and returns its status and output.
    fn finish(mut self) -> (Option<i32>, String) {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                thread::sleep(Duration::from_millis(50));
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

/// Ctrl-C typed while an interactive shell waits for a foreground job that
/// dies of SIGINT abandons the rest of the command line (later list items and
/// loop iterations), as in Bash, and `$?` is 130.
#[cfg(unix)]
#[test]
fn interactive_ctrl_c_abandons_the_rest_of_the_command_line() {
    for line in [
        "printf '%s\\n' st''arted; for i in 1 2 3; do sleep 5; echo it$i; done; echo loop''done",
        "printf '%s\\n' st''arted; sleep 30; echo aft''er",
        "printf '%s\\n' st''arted; while :; do :; done; echo aft''er",
        "printf '%s\\n' st''arted; f() { sleep 5; echo in''f; }; f; echo aft''er",
    ] {
        for program in ["bash", env!("CARGO_BIN_EXE_marsh-brush-test-driver")] {
            let mut shell = PtyShell::spawn(program, true);
            shell.expect("PROMPT$ ", 1);
            shell.send(format!("{line}\r").as_bytes());
            shell.expect("started\r\n", 1);
            thread::sleep(Duration::from_millis(500));
            shell.send(b"\x03");
            thread::sleep(Duration::from_millis(300));
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
/// interactive session: lines are still read and run (Bash never asks).
#[cfg(unix)]
#[test]
fn interactive_shell_survives_a_terminal_that_never_reports_the_cursor() {
    for program in ["bash", env!("CARGO_BIN_EXE_marsh-brush-test-driver")] {
        let mut shell = PtyShell::spawn(program, false);
        shell.expect("PROMPT$ ", 1);
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
        shell.expect("PROMPT$ ", 1);
        let pid = shell.shell_pid(2);
        // Each notification is read only after the real condition: every job
        // the shell started has exited. (Fixed sleeps raced a loaded machine:
        // `sh -c 'exit 3'` had not run yet when `true` asked for the report.)
        shell.send(b"sleep 1 &\r");
        shell.expect("PROMPT$ ", 3);
        shell.expect_children_exited(pid);
        shell.send(b"true\r");
        shell.expect("Done", 1);
        shell.expect("PROMPT$ ", 4);
        shell.send(b"sh -c 'exit 3' &\r");
        shell.expect("PROMPT$ ", 5);
        shell.expect_children_exited(pid);
        shell.send(b"true\r");
        shell.expect("Exit 3", 1);
        shell.expect("PROMPT$ ", 6);
        shell.send(b"sleep 2 & sleep 2 &\r");
        shell.expect("PROMPT$ ", 7);
        shell.send(b"jobs\r");
        shell.expect("Running", 2);
        shell.expect("PROMPT$ ", 8);
        shell.expect_children_exited(pid);
        shell.send(b"true\r");
        shell.expect("Done", 3);
        shell.expect("PROMPT$ ", 9);
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
        shell.expect("PROMPT$ ", 1);
        shell.send(b"(sleep 0.3; printf 'al''pha\\nbe''ta\\nga''mma\\n') &\r");
        shell.expect("PROMPT$ ", 2);
        // The job writes while the line editor is waiting for input.
        shell.expect("gamma", 1);
        thread::sleep(Duration::from_millis(100));
        shell.send(b"exit 0\r");
        let (status, output) = shell.finish();
        assert_eq!(status, Some(0), "{program}: {output:?}");
        assert!(
            output.contains("alpha\r\nbeta\r\ngamma\r\n"),
            "{program}: lines do not start at column 0: {output:?}"
        );
    }
}
