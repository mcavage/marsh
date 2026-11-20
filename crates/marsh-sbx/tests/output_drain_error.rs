//! A stock call whose command exited while a helper held its output pipes
//! reaches the caller as its own error, with the captured output, and is not
//! relabelled as the call running out of time.
use marsh_runtime::{
    Attachment, CommandOutput, CommandRunner, Invocation, OutputDrainTimeout, SystemCommandRunner,
};
use marsh_sbx::{SbxError, StockSbx};
use std::{io, sync::Arc, time::Duration};

struct Drain;

impl CommandRunner for Drain {
    fn run(&self, invocation: &Invocation) -> io::Result<CommandOutput> {
        self.run_bounded(invocation, Duration::from_secs(1))
    }
    fn run_bounded(&self, _: &Invocation, _: Duration) -> io::Result<CommandOutput> {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            OutputDrainTimeout {
                status: Some(0),
                stdout_bytes: 12,
                stderr_bytes: 9,
                stderr_tail: "pulling..".into(),
            },
        ))
    }
    fn spawn_attached(&self, _: &Invocation) -> io::Result<Attachment> {
        Err(io::Error::other("unexpected attachment"))
    }
}

#[test]
fn a_drain_failure_keeps_its_own_message_and_captured_output() {
    let error = StockSbx::new("sbx", Arc::new(Drain))
        .stock_inventory(true)
        .unwrap_err();
    let message = error.to_string();
    assert!(matches!(error, SbxError::OutputDrain { .. }), "{message}");
    assert!(
        message.contains("the command exited (status Some(0)) but its output pipes stayed open for 2s (a helper process holds them)")
            && message.contains("captured 12 bytes of stdout and 9 of stderr")
            && message.contains("pulling.."),
        "{message}"
    );
    assert!(!message.contains("timed out after"), "{message}");
}

/// Through the real runner and a real helper that holds the pipes.
#[test]
fn a_real_helper_holding_the_pipes_is_reported_as_a_drain_failure() {
    let root = tempfile::tempdir().unwrap();
    let sbx = root.path().join("sbx");
    std::fs::write(
        &sbx,
        "#!/bin/sh\nsleep 30 &\necho partial-listing; exit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&sbx, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let started = std::time::Instant::now();
    let error = StockSbx::new(&sbx, Arc::new(SystemCommandRunner::new(root.path())))
        .stock_inventory(true)
        .unwrap_err();
    let message = error.to_string();
    assert!(started.elapsed() < Duration::from_secs(15), "{message}");
    assert!(
        message.contains("output pipes stayed open for 2s")
            && message.contains("captured 16 bytes of stdout"),
        "{message}"
    );
    assert!(!message.contains("timed out after"), "{message}");
}
