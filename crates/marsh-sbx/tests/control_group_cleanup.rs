//! Real public control caller: closed stdio is not descendant quiescence.
use marsh_runtime::{
    AttachedProcess, Attachment, CommandOutput, CommandRunner, Invocation, SystemCommandRunner,
};
use marsh_sbx::{SbxError, run_stock_command_capped};
use std::{
    fs, io, thread,
    time::{Duration, Instant},
};

/// Whether `pid` has exited (gone, or a zombie awaiting its reaper), waiting
/// up to a generous bound for an already-signalled process to die.
fn exits(pid: i32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if exited(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn exited(pid: i32) -> bool {
    let Some(process) = rustix::process::Pid::from_raw(pid) else {
        return true;
    };
    if rustix::process::test_kill_process(process) == Err(rustix::io::Errno::SRCH) {
        return true;
    }
    if cfg!(target_os = "linux") {
        return fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, state)| state.trim_start().starts_with('Z'))
        });
    }
    std::process::Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|output| {
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .starts_with('Z')
        })
}

#[test]
fn completed_control_leader_cannot_leave_a_same_group_late_effect() {
    closed_stdio_descendant(false);
}

#[test]
fn bounded_runtime_cannot_leave_a_same_group_late_effect() {
    closed_stdio_descendant(true);
}

fn closed_stdio_descendant(runtime: bool) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let script = r"
import json, os, pathlib, sys, time
root = pathlib.Path(sys.argv[1])
leader = os.getpid()
child = os.fork()
if child == 0:
    for fd in (0, 1, 2): os.close(fd)
    # Atomic publish: the leader exits as soon as the name exists.
    (root/'descendant.tmp').write_text(json.dumps({'leader':leader, 'child':os.getpid(), 'pgid':os.getpgrp()}))
    os.replace(root/'descendant.tmp', root/'descendant.json')
    deadline = time.monotonic() + 5
    while not (root/'release').exists() and time.monotonic() < deadline: time.sleep(.005)
    if (root/'release').exists(): (root/'late-effect').write_text('descendant retained authority after successful capture')
    (root/'child-finished').write_text('finished')
    os._exit(0)
deadline = time.monotonic() + 8
while not (root/'descendant.json').exists():
    if time.monotonic() > deadline: os._exit(3)
    time.sleep(.005)
os._exit(0)
";
    let runner = SystemCommandRunner::new(&root);
    let invocation = Invocation {
        program: "/usr/bin/python3".into(),
        arguments: [
            "-I".into(),
            "-S".into(),
            "-c".into(),
            script.into(),
            root.clone().into_os_string(),
        ]
        .into(),
        working_directory: Some(root.clone()),
        environment: Vec::new(),
    };
    let result = if runtime {
        runner
            .run_bounded(&invocation, Duration::from_secs(10))
            .map_err(|error| error.to_string())
    } else {
        run_stock_command_capped(&runner, &invocation, Duration::from_secs(10), 4096)
            .map_err(|error| error.to_string())
    };
    let identity: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("descendant.json")).unwrap()).unwrap();
    assert_eq!(identity["leader"], identity["pgid"]);
    assert_ne!(identity["child"], identity["leader"]);
    let child = i32::try_from(identity["child"].as_i64().unwrap()).unwrap();
    assert!(
        exits(child),
        "closed stdio and leader exit left a live same-group descendant"
    );
    fs::write(root.join("release"), b"caller returned").unwrap();
    let late_effect = root.join("late-effect").exists();
    eprintln!(
        "completed capture {:?}; same-group identity {identity}; effect after caller return={late_effect}",
        result.as_ref().map(|output| output.exit_code)
    );
    assert!(
        !late_effect,
        "closed stdio and leader exit left live same-group effect authority"
    );
    assert!(
        result.is_ok(),
        "a safely cleaned successful control call should still succeed: {result:?}"
    );
}

// The fixture is `/bin/sh`, which starts in milliseconds; the deadline only
// has to outlast that start for the attempt to observe cleanup.
const DEADLINE: Duration = Duration::from_secs(2);
const CLEANUP_SLACK: Duration = Duration::from_millis(2700);

#[test]
fn deadline_and_capture_overflow_remove_owned_late_effect_authority() {
    for overflow in [false, true] {
        late_effect_authority_removed(overflow);
    }
}

fn late_effect_authority_removed(overflow: bool) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let script = r#"
echo $$ > "$1/ready"
if [ "$2" = overflow ]; then head -c 8192 /dev/zero; fi
i=0
while [ ! -e "$1/release" ] && [ $i -lt 2000 ]; do sleep 0.005; i=$((i + 1)); done
if [ -e "$1/release" ]; then echo 'still alive after control failure' > "$1/late-effect"; fi
"#;
    let started = Instant::now();
    let result = run_stock_command_capped(
        &SystemCommandRunner::new(&root),
        &Invocation {
            program: "/bin/sh".into(),
            arguments: vec![
                "-c".into(),
                script.into(),
                "fixture".into(),
                root.clone().into_os_string(),
                if overflow { "overflow" } else { "deadline" }.into(),
            ],
            working_directory: Some(root.clone()),
            environment: Vec::new(),
        },
        DEADLINE,
        1024,
    );
    let elapsed = started.elapsed();
    let leader = fs::read_to_string(root.join("ready"))
        .unwrap_or_else(|error| panic!("fixture never started within {DEADLINE:?}: {error}"));
    let leader = leader.trim().parse::<i32>().unwrap();
    assert!(exits(leader), "failed capture left its leader running");
    fs::write(root.join("release"), b"caller failed").unwrap();
    assert!(
        !root.join("late-effect").exists(),
        "failed capture retained live effect authority"
    );
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains(if overflow {
            "capture limit"
        } else {
            "deadline"
        }),
        "{error}"
    );
    // Overflow ends the call early; a deadline ends it at the deadline, and
    // cleanup after either finishes within the same slack.
    let bound = if overflow {
        DEADLINE
    } else {
        DEADLINE + CLEANUP_SLACK
    };
    assert!(elapsed < bound, "cleanup exceeded its bound: {elapsed:?}");
}

struct RejectCleanup(SystemCommandRunner);
struct RejectTermination(Box<dyn AttachedProcess>);

impl CommandRunner for RejectCleanup {
    fn run(&self, invocation: &Invocation) -> io::Result<CommandOutput> {
        self.0.run(invocation)
    }
    fn run_bounded(&self, invocation: &Invocation, timeout: Duration) -> io::Result<CommandOutput> {
        self.0.run_bounded(invocation, timeout)
    }
    fn spawn_attached(&self, invocation: &Invocation) -> io::Result<Attachment> {
        let mut attached = self.0.spawn_attached(invocation)?;
        attached.process = Box::new(RejectTermination(attached.process));
        Ok(attached)
    }
}

impl AttachedProcess for RejectTermination {
    fn wait(&mut self) -> io::Result<i32> {
        self.0.wait()
    }
    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        self.0.try_wait()
    }
    fn try_wait_unreaped(&mut self) -> io::Result<Option<i32>> {
        self.0.try_wait_unreaped()
    }
    fn supports_io_cancellation(&self) -> bool {
        self.0.supports_io_cancellation()
    }
    fn cancel_io(&self) -> io::Result<()> {
        self.0.cancel_io()
    }
    fn terminate(&mut self) -> io::Result<()> {
        // Clean the actual owned process first. Injection exercises propagation,
        // and is deliberately not described as an actual changed-UID denial.
        self.0.terminate()?;
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "injected control cleanup denial",
        ))
    }
}

#[test]
fn successful_native_output_preserves_typed_termination_uncertainty() {
    let root = tempfile::tempdir().unwrap();
    let error = run_stock_command_capped(
        &RejectCleanup(SystemCommandRunner::new(root.path())),
        &Invocation {
            program: "/usr/bin/python3".into(),
            arguments: ["-I", "-S", "-c", "print('successful native output')"]
                .map(Into::into)
                .into(),
            working_directory: Some(root.path().to_owned()),
            environment: Vec::new(),
        },
        Duration::from_secs(3),
        4096,
    )
    .unwrap_err();
    match error {
        SbxError::StockControlCleanupUncertain {
            control_error,
            terminate_error,
            reaped,
            readers_joined,
            ..
        } => {
            assert!(control_error.is_none());
            assert_eq!(
                terminate_error.as_deref(),
                Some("injected control cleanup denial")
            );
            assert!(reaped && readers_joined);
        }
        other => panic!("native success masked or misclassified cleanup uncertainty: {other}"),
    }
}
