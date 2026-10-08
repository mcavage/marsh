//! Stdio shell controller: preparation is cancellable and owns no raw tty or
//! stdin pump until the backend reports readiness. All I/O workers are joined.
use super::{
    AttachmentFrame, Client, ClientExecution, DaemonError, HostTerminalMode, InputPumpFailure,
    SHELL_STDIN_CHUNK, ShellSpec, ShellState, SignalRelay, attachment_input_closed,
    capture_initial_terminal_size, pending_input_fatal, write_attachment_bytes,
};
use signal_hook::{
    consts::signal::{SIGHUP, SIGINT, SIGTERM},
    iterator::{Handle as SignalHandle, Signals},
};
use std::{
    fs::File,
    io::{self, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

struct PreparationSignals {
    handle: SignalHandle,
    task: Option<thread::JoinHandle<()>>,
    cancelled: Arc<AtomicUsize>,
}

impl PreparationSignals {
    /// SIGINT before readiness is a user abort and cancels preparation.
    /// SIGTERM/SIGHUP are queued as signal frames: the daemon reads them only
    /// once the shell runs and delivers them after containment publication,
    /// so a trap installed by user code observes them.
    fn start(execution: ClientExecution) -> io::Result<Self> {
        let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP])?;
        let handle = signals.handle();
        let cancelled = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&cancelled);
        let task = thread::spawn(move || {
            for signal in signals.forever() {
                let queued = match signal {
                    SIGTERM => Some("terminate"),
                    SIGHUP => Some("hangup"),
                    _ => None,
                };
                if let Some(name) = queued
                    && execution
                        .send(&AttachmentFrame::Signal {
                            signal: name.into(),
                        })
                        .is_ok()
                {
                    continue;
                }
                observed.store(
                    usize::try_from(128 + signal).unwrap_or(125),
                    Ordering::Release,
                );
                // Not a signal frame queued for a future guest process. The
                // preparation owner observes disconnect and rolls back.
                let _ = execution.shutdown();
                break;
            }
        });
        Ok(Self {
            handle,
            task: Some(task),
            cancelled,
        })
    }

    fn finish(mut self) -> Option<i32> {
        self.stop();
        match self.cancelled.load(Ordering::Acquire) {
            0 => None,
            code => Some(i32::try_from(code).unwrap_or(125)),
        }
    }

    fn stop(&mut self) {
        self.handle.close();
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

impl Drop for PreparationSignals {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(super) fn run(client: &Client, mut spec: ShellSpec) -> Result<i32, DaemonError> {
    let terminal = spec.session.terminal;
    capture_initial_terminal_size(&mut spec.session)?;
    let session_id = spec.session.session_id.clone();
    let execution = client.start_shell(spec)?;
    let preparation = PreparationSignals::start(execution.clone())?;
    let ready = wait_ready(&execution, &mut io::stderr());
    if let Some(code) = preparation.finish() {
        writeln!(
            io::stderr(),
            "marsh: cancelling shell preparation; waiting for cleanup confirmation"
        )?;
        wait_cancelled_cleanup(client, &session_id)?;
        return Ok(code);
    }
    ready?;
    let _terminal = HostTerminalMode::enter_if(terminal)?;
    // The duplicate is polled, never a detached blocking std::io::Stdin read.
    let input = File::from(rustix::io::dup(io::stdin()).map_err(io::Error::from)?);
    let result = relay_io(&execution, terminal, input, io::stdout(), io::stderr());
    let _ = execution.shutdown();
    result.map_err(crate::lost_after_accept)
}

fn wait_cancelled_cleanup(client: &Client, session_id: &str) -> Result<(), DaemonError> {
    // Cancellation closes the stream, so use an independent observation rather
    // than claiming that local socket shutdown verified rollback. Never issue
    // DetachShell while the preparation owner may still hold guest authority.
    let deadline = Instant::now() + Duration::from_mins(2);
    loop {
        let status = client.status(Some(session_id.into()))?;
        match status.shells.iter().find(|shell| shell.session_id == session_id).map(|shell| shell.state) {
            Some(ShellState::Detached) => return Ok(()),
            Some(ShellState::Attached) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => return Err(DaemonError::ShellCleanupUncertain("cancelled shell preparation rollback is not confirmed; inspect status and use host scope recovery".into())),
        }
    }
}

pub(super) fn wait_ready(
    execution: &ClientExecution,
    error: &mut impl Write,
) -> Result<(), DaemonError> {
    loop {
        match execution.receive()? {
            AttachmentFrame::ShellReady => return Ok(()),
            AttachmentFrame::ColdBoot { kit, download } => {
                writeln!(error, "{}", crate::cold_boot_notice(&kit, download))?;
                error.flush()?;
            }
            AttachmentFrame::Failed { message } => return Err(DaemonError::Remote(message)),
            frame => {
                return Err(DaemonError::InvalidState(format!(
                    "unexpected shell preparation frame: {frame:?}"
                )));
            }
        }
    }
}

fn cancellable_terminal_input(input: File) -> io::Result<File> {
    if !rustix::termios::isatty(&input) {
        return Ok(input);
    }
    // poll readiness is advisory (not a cancellation guarantee for a canonical
    // TTY read on Darwin). Open a NEW terminal description as nonblocking;
    // changing flags on dup(stdin) would change the parent shell's description.
    let original = rustix::fs::fstat(&input)?;
    let name = rustix::termios::ttyname(&input, Vec::new())?;
    let reopened = File::from(rustix::fs::open(
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::NOCTTY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )?);
    let current = rustix::fs::fstat(&reopened)?;
    if (original.st_dev, original.st_ino, original.st_rdev)
        != (current.st_dev, current.st_ino, current.st_rdev)
    {
        return Err(io::Error::other(
            "terminal input identity changed while opening cancellable reader",
        ));
    }
    Ok(reopened)
}

fn pump_input(
    execution: &ClientExecution,
    mut input: File,
    stopping: &AtomicBool,
) -> Result<(), DaemonError> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let mut bytes = [0; SHELL_STDIN_CHUNK];
    while !stopping.load(Ordering::Acquire) && !execution.stdin_closed() {
        let mut fds = [PollFd::new(&input, PollFlags::IN)];
        match poll(
            &mut fds,
            Some(&Timespec {
                tv_sec: 0,
                tv_nsec: 20_000_000,
            }),
        ) {
            Ok(0) | Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(DaemonError::Io(error.into())),
            Ok(_) => {}
        }
        match input.read(&mut bytes) {
            Ok(0) => {
                execution.send(&AttachmentFrame::StdinEof)?;
                return Ok(());
            }
            Ok(count) => match execution.send(&AttachmentFrame::Stdin {
                bytes: bytes[..count].to_vec(),
            }) {
                Ok(()) => {}
                Err(DaemonError::ShellStdinClosed) => return Ok(()),
                Err(error) => return Err(error),
            },
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(super) fn relay_io(
    execution: &ClientExecution,
    terminal: bool,
    input: File,
    mut output: impl Write,
    mut error: impl Write,
) -> Result<i32, DaemonError> {
    let input = cancellable_terminal_input(input)?;
    let stopping = Arc::new(AtomicBool::new(false));
    let input_stopping = Arc::clone(&stopping);
    let input_execution = execution.clone();
    let (failure, failures) = mpsc::channel();
    let signals = SignalRelay::start(execution.clone(), terminal, failure.clone())?;
    let pump = thread::spawn(move || {
        if let Err(error) = pump_input(&input_execution, input, &input_stopping)
            && !input_stopping.load(Ordering::Acquire)
            && !input_execution.terminal_received()
        {
            if attachment_input_closed(&error) {
                // The peer may have already queued Exited before closing reads.
                // Do not discard that actual terminal based on a write race.
                let _ = failure.send(InputPumpFailure::Closed);
            } else {
                let _ = failure.send(InputPumpFailure::Fatal(error.to_string()));
                let _ = input_execution.shutdown();
            }
        }
    });
    let result = receive_output(execution, terminal, &mut output, &mut error);
    stopping.store(true, Ordering::Release);
    // Wake credit waiters and any in-flight frame, before joining either worker.
    let _ = execution.shutdown();
    drop(signals);
    let joined = pump.join();
    if let Some(reason) = pending_input_fatal(&failures) {
        return Err(DaemonError::ShellInputTransportClosed(reason));
    }
    if joined.is_err() {
        return Err(DaemonError::InvalidState(
            "shell stdin worker panicked".into(),
        ));
    }
    result
}

fn receive_output(
    execution: &ClientExecution,
    terminal: bool,
    output: &mut impl Write,
    error: &mut impl Write,
) -> Result<i32, DaemonError> {
    loop {
        match execution.receive()? {
            AttachmentFrame::Stdout { bytes } => write_attachment_bytes(output, &bytes)?,
            AttachmentFrame::Stderr { bytes } => write_attachment_bytes(error, &bytes)?,
            AttachmentFrame::ControlError {
                operation, message, ..
            } => {
                write!(
                    error,
                    "marsh: {operation}: {message}{}",
                    if terminal { "\r\n" } else { "\n" }
                )?;
                error.flush()?;
            }
            AttachmentFrame::Exited { code } => return Ok(code),
            AttachmentFrame::Failed { message } => return Err(DaemonError::Remote(message)),
            frame => {
                return Err(DaemonError::InvalidState(format!(
                    "unexpected shell output frame: {frame:?}"
                )));
            }
        }
    }
}
