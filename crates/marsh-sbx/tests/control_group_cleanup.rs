//! Real public control caller: closed stdio is not descendant quiescence.
use marsh_runtime::{
    AttachedProcess, Attachment, CommandOutput, CommandRunner, Invocation, SystemCommandRunner,
};
use marsh_sbx::{SbxError, run_stock_command_capped};
use std::{
    fs, io, thread,
    time::{Duration, Instant},
};

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
    (root/'descendant.json').write_text(json.dumps({'leader':leader, 'child':os.getpid(), 'pgid':os.getpgrp()}))
    deadline = time.monotonic() + 5
    while not (root/'release').exists() and time.monotonic() < deadline: time.sleep(.005)
    if (root/'release').exists(): (root/'late-effect').write_text('descendant retained authority after successful capture')
    (root/'child-finished').write_text('finished')
    os._exit(0)
deadline = time.monotonic() + 3
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
            .run_bounded(&invocation, Duration::from_secs(3))
            .map_err(|error| error.to_string())
    } else {
        run_stock_command_capped(&runner, &invocation, Duration::from_secs(3), 4096)
            .map_err(|error| error.to_string())
    };
    let identity: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("descendant.json")).unwrap()).unwrap();
    assert_eq!(identity["leader"], identity["pgid"]);
    assert_ne!(identity["child"], identity["leader"]);
    fs::write(root.join("release"), b"caller returned").unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    while !root.join("child-finished").exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
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

// The deadline must outlast a cold /usr/bin/python3 start (the Xcode shim takes
// hundreds of ms on a loaded runner), or the fixture is killed before it writes
// `ready` and that attempt observes nothing about cleanup. An attempt whose
// fixture never started is inconclusive, not a failure: it is repeated with a
// doubled deadline until the fixture has really started.
const DEADLINE: Duration = Duration::from_secs(2);
const OVERFLOW_DEADLINE: Duration = Duration::from_secs(3);
const CLEANUP_SLACK: Duration = Duration::from_millis(2700);
const ATTEMPTS: u32 = 4;

#[test]
fn deadline_and_capture_overflow_remove_owned_late_effect_authority() {
    for overflow in [false, true] {
        let base = if overflow {
            OVERFLOW_DEADLINE
        } else {
            DEADLINE
        };
        let observed = (0..ATTEMPTS)
            .any(|attempt| late_effect_authority_removed(overflow, base * 2_u32.pow(attempt)));
        assert!(
            observed,
            "fixture never started in {ATTEMPTS} attempts (overflow={overflow})"
        );
    }
}

/// Returns false if the fixture was killed before it started; asserts
/// everything else.
fn late_effect_authority_removed(overflow: bool, deadline: Duration) -> bool {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let script = r"
import os, pathlib, sys, time
root = pathlib.Path(sys.argv[1])
(root/'ready').write_text(str(os.getpid()))
if sys.argv[2] == 'overflow': os.write(1, b'x'*8192)
deadline = time.monotonic()+5
while not (root/'release').exists() and time.monotonic()<deadline: time.sleep(.005)
if (root/'release').exists(): (root/'late-effect').write_text('still alive after control failure')
";
    let started = Instant::now();
    let result = run_stock_command_capped(
        &SystemCommandRunner::new(&root),
        &Invocation {
            program: "/usr/bin/python3".into(),
            arguments: vec![
                "-I".into(),
                "-S".into(),
                "-c".into(),
                script.into(),
                root.clone().into_os_string(),
                if overflow { "overflow" } else { "deadline" }.into(),
            ],
            working_directory: Some(root.clone()),
            environment: Vec::new(),
        },
        deadline,
        1024,
    );
    let elapsed = started.elapsed();
    if !root.join("ready").exists() {
        eprintln!("fixture never started within {deadline:?}: {result:?}; retrying");
        return false;
    }
    fs::write(root.join("release"), b"caller failed").unwrap();
    thread::sleep(Duration::from_millis(100));
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
    // Cleanup after the failure must finish within the same slack as ever:
    // overflow ends the call early, a deadline ends it at the deadline.
    let bound = if overflow {
        deadline
    } else {
        deadline + CLEANUP_SLACK
    };
    assert!(elapsed < bound, "cleanup exceeded its bound: {elapsed:?}");
    eprintln!("overflow={overflow}; elapsed={elapsed:?}; error={error}; late_effect=false");
    true
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
