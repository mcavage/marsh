//! Bounded teardown after controller loss, not a deadline on healthy shells.
//!
//! Guest cleanup, local process ownership, and client delivery are independent.
//! Only cancellable runtime streams are admitted; no opaque I/O is abandoned.

use crate::shell_delivery_exit;
use marsh_contracts::JobSignal;
use marsh_contracts::TerminalSize;
use marsh_daemon::{
    AttachmentControlError, AttachmentFrame, DaemonError, SHELL_STDIN_CHUNK, SHELL_STDIN_WINDOW,
    ServerAttachment, relay,
};
use marsh_runtime::{AttachedProcess, Attachment, AttachmentControl};
use std::{
    io::{self, Write},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const POLL: Duration = Duration::from_millis(10);
const DRAIN: Duration = Duration::from_secs(2);

pub(super) fn require_controller(attachment: &ServerAttachment) -> Result<(), DaemonError> {
    if attachment.peer_disconnected()? {
        Err(DaemonError::Io(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "shell preparation cancelled: controller disconnected",
        )))
    } else {
        Ok(())
    }
}

#[derive(Debug)]
pub(super) enum Cleanup {
    Verified,
    Uncertain(String),
}

#[derive(Debug)]
pub(super) struct Outcome {
    pub runtime_code: Option<i32>,
    pub delivered: bool,
    pub cleanup: Cleanup,
}

impl Outcome {
    pub fn status(&self) -> i32 {
        if matches!(self.cleanup, Cleanup::Verified) {
            shell_delivery_exit(self.runtime_code.unwrap_or(125), self.delivered)
        } else {
            125
        }
    }
}

enum Input {
    Bytes(Vec<u8>),
    Eof,
}

struct InputTasks {
    reader: thread::JoinHandle<()>,
    writer: thread::JoinHandle<()>,
    notices: thread::JoinHandle<()>,
}

fn shell_signal(signal: &str) -> Result<JobSignal, &'static str> {
    match signal.to_ascii_uppercase().as_str() {
        "INT" | "SIGINT" | "INTERRUPT" => Ok(JobSignal::Interrupt),
        "TERM" | "SIGTERM" | "TERMINATE" => Ok(JobSignal::Terminate),
        "KILL" | "SIGKILL" => Ok(JobSignal::Kill),
        "HUP" | "SIGHUP" | "HANGUP" => Ok(JobSignal::Hangup),
        _ => Err("unsupported signal; expected INT, TERM, KILL or HUP"),
    }
}

fn control_message(mut message: String) -> String {
    const LIMIT: usize = 1024;
    if message.len() > LIMIT {
        let mut end = LIMIT;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
        message.push_str(" [diagnostic truncated]");
    }
    message
}

fn start_input(
    attachment: ServerAttachment,
    mut writer: Box<dyn Write + Send>,
    control: Arc<dyn AttachmentControl>,
    stopping: Arc<AtomicBool>,
    lost: mpsc::Sender<()>,
) -> InputTasks {
    let (send, receive) = mpsc::sync_channel(SHELL_STDIN_WINDOW + 1);
    let output = attachment.clone();
    let writer_lost = lost.clone();
    let writer = thread::spawn(move || {
        while let Ok(input) = receive.recv() {
            match input {
                Input::Bytes(bytes) => {
                    if writer
                        .write_all(&bytes)
                        .and_then(|()| writer.flush())
                        .is_err()
                    {
                        // Early consumers close only bytes, not the control channel.
                        if output.send(&AttachmentFrame::StdinClosed).is_err() {
                            let _ = writer_lost.send(());
                        }
                        return;
                    }
                    if output.send(&AttachmentFrame::StdinCredit).is_err() {
                        let _ = writer_lost.send(());
                        return;
                    }
                }
                Input::Eof => return,
            }
        }
    });
    // Control effects never wait behind output or stdin credits. Diagnostics
    // have a finite queue; a control-flood may coalesce diagnostics, not effects.
    let (notice, notices) = mpsc::sync_channel(8);
    let notice_output = attachment.clone();
    let notice_lost = lost.clone();
    let notices = thread::spawn(move || {
        while let Ok(frame) = notices.recv() {
            if notice_output.send(&frame).is_err() {
                let _ = notice_lost.send(());
                break;
            }
        }
    });
    let reader = thread::spawn(move || {
        read_input(
            &attachment,
            send,
            control.as_ref(),
            &stopping,
            &lost,
            &notice,
        );
    });
    InputTasks {
        reader,
        writer,
        notices,
    }
}

fn read_input(
    attachment: &ServerAttachment,
    send: mpsc::SyncSender<Input>,
    control: &dyn AttachmentControl,
    stopping: &AtomicBool,
    lost: &mpsc::Sender<()>,
    notice: &mpsc::SyncSender<AttachmentFrame>,
) {
    let mut send = Some(send);
    loop {
        let frame = attachment.receive();
        if stopping.load(Ordering::Acquire) {
            return;
        }
        let Ok(frame) = frame else {
            let _ = lost.send(());
            return;
        };
        #[allow(
            clippy::collapsible_match,
            reason = "input/control effects execute in arm bodies, not pattern guards"
        )]
        match frame {
            AttachmentFrame::Stdin { bytes } => {
                if bytes.len() > SHELL_STDIN_CHUNK
                    || send
                        .as_ref()
                        .is_some_and(|send| send.try_send(Input::Bytes(bytes)).is_err())
                {
                    send.take();
                    let _ = notice.try_send(AttachmentFrame::StdinClosed);
                }
            }
            AttachmentFrame::StdinEof => {
                if let Some(send) = send.take() {
                    let _ = send.try_send(Input::Eof);
                }
            }
            AttachmentFrame::Signal { signal } => {
                let result = shell_signal(&signal)
                    .map_err(|message| {
                        (
                            AttachmentControlError::UnsupportedSignal,
                            message.to_owned(),
                        )
                    })
                    .and_then(|signal| {
                        control.signal(signal).map_err(|error| {
                            (AttachmentControlError::SignalDelivery, error.to_string())
                        })
                    });
                if let Err((code, message)) = result {
                    let _ = notice.try_send(AttachmentFrame::ControlError {
                        code,
                        operation: "signal".into(),
                        message: control_message(message),
                    });
                }
            }
            AttachmentFrame::Resize { rows, columns } => {
                if let Err(error) = control.resize(TerminalSize { rows, columns }) {
                    let _ = notice.try_send(AttachmentFrame::ControlError {
                        code: AttachmentControlError::ResizeDelivery,
                        operation: "resize".into(),
                        message: control_message(error.to_string()),
                    });
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
pub(super) fn forward_raw_input(
    attachment: ServerAttachment,
    writer: Box<dyn Write + Send>,
    control: Arc<dyn AttachmentControl>,
) {
    let (lost, _) = mpsc::channel();
    let _ = start_input(
        attachment,
        writer,
        control,
        Arc::new(AtomicBool::new(false)),
        lost,
    );
}

fn poll_exit(process: &mut dyn AttachedProcess, until: Instant) -> io::Result<Option<i32>> {
    loop {
        let code = process.try_wait_unreaped()?;
        if code.is_some() || Instant::now() >= until {
            return Ok(code);
        }
        thread::sleep(POLL);
    }
}

/// Runs indefinitely while the controller/process are healthy. Teardown begins
/// only after an exit or concrete I/O/control failure. The trusted guest cleanup
/// method owns bounded TERM/grace/KILL and supplies a verified quiescence receipt.
#[allow(
    clippy::too_many_lines,
    reason = "one owner retains every process and I/O handle through teardown"
)]
pub(super) fn run(mut process: Attachment, attachment: &ServerAttachment) -> Outcome {
    if !process.process.supports_io_cancellation() {
        let _ = process.control.cleanup_session();
        let _ = process.process.terminate();
        marsh_runtime::retain_for_reaping(process.process);
        return Outcome {
            runtime_code: None,
            delivered: false,
            cleanup: Cleanup::Uncertain("opaque shell attachment I/O rejected".into()),
        };
    }
    let stopping = Arc::new(AtomicBool::new(false));
    let (lost, failures) = mpsc::channel();
    let input = start_input(
        attachment.clone(),
        process.stdin,
        Arc::clone(&process.control),
        Arc::clone(&stopping),
        lost.clone(),
    );
    let stdout = crate::stream_shell_output(
        process.stdout,
        attachment.clone(),
        false,
        Some(lost.clone()),
    );
    let stderr = crate::stream_shell_output(process.stderr, attachment.clone(), true, Some(lost));
    let mut delivered = true;
    let mut identity_known = true;
    let mut runtime_code = loop {
        match process.process.try_wait_unreaped() {
            Ok(Some(code)) => break Some(code),
            Err(_) => {
                identity_known = false;
                delivered = false;
                break None;
            }
            Ok(None) => {}
        }
        if failures.try_recv().is_ok() || attachment.peer_disconnected().unwrap_or(true) {
            delivered = false;
            break None;
        }
        thread::sleep(POLL);
    };
    if !delivered {
        let _ = attachment.shutdown();
    }
    // Keep control alive throughout cleanup AND healthy output backpressure.
    // An exited producer is not evidence that its final bytes were consumed.
    let mut cleanup = match process.control.cleanup_session() {
        Ok(()) => Cleanup::Verified,
        Err(error) => Cleanup::Uncertain(format!("guest session: {error}")),
    };
    if runtime_code.is_none() && identity_known {
        match poll_exit(&mut *process.process, Instant::now() + DRAIN) {
            Ok(code) => runtime_code = code,
            Err(_) => identity_known = false,
        }
    }
    if runtime_code.is_none() && identity_known {
        let termination = process.process.terminate();
        let observed = poll_exit(&mut *process.process, Instant::now() + DRAIN);
        identity_known = observed.is_ok();
        if termination.is_err() || !matches!(observed, Ok(Some(_))) {
            cleanup = Cleanup::Uncertain("local shell process could not be cleaned/reaped".into());
        }
    }
    if !identity_known {
        cleanup = Cleanup::Uncertain(
            "local identity observation failed; further group signaling withheld".into(),
        );
    }
    if !matches!(cleanup, Cleanup::Verified) {
        delivered = false;
        let _ = attachment.shutdown();
    }
    let mut loss_deadline = (!delivered).then(|| Instant::now() + DRAIN);
    while !(stdout.is_finished() && stderr.is_finished()) {
        if failures.try_recv().is_ok() || attachment.peer_disconnected().unwrap_or(true) {
            delivered = false;
            let _ = attachment.shutdown();
            loss_deadline.get_or_insert_with(|| Instant::now() + DRAIN);
        }
        if loss_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        thread::sleep(POLL);
    }
    if delivered && attachment.send(&AttachmentFrame::StdinClosed).is_err() {
        delivered = false;
    }
    stopping.store(true, Ordering::Release);
    attachment.close_input();
    if !delivered {
        let _ = attachment.shutdown();
    }
    // Nonblocking runtime wrappers observe this even with escaped pipe holders.
    if let Err(error) = process.process.cancel_io() {
        cleanup = Cleanup::Uncertain(format!("local I/O cancellation: {error}"));
    }
    // The unreaped leader still pins this local group against PID reuse.
    let local_group_cleaned = if identity_known {
        match process.process.terminate() {
            Ok(()) => true,
            Err(error) => {
                cleanup = Cleanup::Uncertain(format!("local group termination: {error}"));
                false
            }
        }
    } else {
        false
    };
    let receiver_ok = input.reader.join().is_ok();
    let writer_ok = input.writer.join().is_ok();
    let notices_ok = input.notices.join().is_ok();
    let input_ok = writer_ok && notices_ok;
    for stream in [stdout, stderr] {
        if let Ok(stream) = stream.join() {
            debug_assert_eq!(stream.first_output.is_some(), stream.bytes > 0);
            delivered &= stream.delivered;
        } else {
            delivered = false;
            cleanup = Cleanup::Uncertain("shell output task panicked".into());
        }
    }
    if !input_ok || !receiver_ok {
        cleanup = Cleanup::Uncertain("shell I/O task panicked".into());
    }
    if !local_group_cleaned || !matches!(process.process.try_wait(), Ok(Some(_))) {
        marsh_runtime::retain_for_reaping(process.process);
    }
    Outcome {
        runtime_code,
        delivered,
        cleanup,
    }
}

/// Retains local relay ownership through cancellable drain and finite reaping.
pub(super) struct Relay {
    process: Box<dyn AttachedProcess>,
    host: thread::JoinHandle<Result<(), DaemonError>>,
    stderr: thread::JoinHandle<io::Result<u64>>,
    ready: mpsc::Receiver<Result<Option<marsh_daemon::relay_cleanup::RelayIdentity>, String>>,
    identity: Arc<Mutex<Option<marsh_daemon::relay_cleanup::RelayIdentity>>>,
    tunnel: Arc<Mutex<Box<dyn Write + Send>>>,
    cleaned: Arc<AtomicBool>,
}

/// Local relay teardown result. `cleaned_in_band` is true only when the guest
/// relay reported socket/token removal and its exec then exited successfully.
pub(super) struct RelayStop {
    pub cleaned_in_band: bool,
}

impl Relay {
    pub fn start(
        mut attachment: Attachment,
        socket: PathBuf,
        token: String,
    ) -> Result<Self, DaemonError> {
        if !attachment.process.supports_io_cancellation() {
            let _ = attachment.process.terminate();
            marsh_runtime::retain_for_reaping(attachment.process);
            return Err(DaemonError::ShellCleanupUncertain(
                "opaque relay I/O rejected".into(),
            ));
        }
        let (send, ready) = mpsc::channel();
        let identity = Arc::new(Mutex::new(None));
        let observed_identity = Arc::clone(&identity);
        let tunnel: Arc<Mutex<Box<dyn Write + Send>>> = Arc::new(Mutex::new(attachment.stdin));
        let host_tunnel = Arc::clone(&tunnel);
        let cleaned = Arc::new(AtomicBool::new(false));
        let host_cleaned = Arc::clone(&cleaned);
        let host = thread::spawn(move || {
            relay::run_host_shared(
                attachment.stdout,
                &host_tunnel,
                &socket,
                token,
                |result| {
                    if let Ok(Some(identity)) = &result {
                        *observed_identity
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            Some(identity.clone());
                    }
                    let _ = send.send(result);
                },
                &host_cleaned,
            )
        });
        let stderr = thread::spawn(move || io::copy(&mut attachment.stderr, &mut io::sink()));
        Ok(Self {
            process: attachment.process,
            host,
            stderr,
            ready,
            identity,
            tunnel,
            cleaned,
        })
    }

    pub fn ready(&self, controller: &ServerAttachment) -> Result<(), DaemonError> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            require_controller(controller)?;
            match self.ready.recv_timeout(POLL) {
                Ok(result) => {
                    let identity = result.map_err(DaemonError::InvalidState)?.ok_or_else(|| {
                        DaemonError::ShellCleanupUncertain(
                            "guest relay did not provide its process identity".into(),
                        )
                    })?;
                    *self
                        .identity
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(identity);
                    return Ok(());
                }
                Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
                Err(_) => {
                    return Err(DaemonError::InvalidState(
                        "guest relay readiness unavailable or timed out".into(),
                    ));
                }
            }
        }
    }

    pub fn stop(mut self) -> Result<RelayStop, DaemonError> {
        // A controller may cancel just before the trusted relay's Ready is
        // observed. Give that already-owned startup a finite teardown interval
        // to publish identity; do not throw away available cleanup evidence.
        let missing_identity = self
            .identity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none();
        if missing_identity && let Ok(Ok(Some(identity))) = self.ready.recv_timeout(DRAIN) {
            *self
                .identity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(identity);
        }
        // In-band teardown: ask the guest to remove its socket/token and
        // report Cleaned, then observe this exact exec exit successfully.
        let mut cleaned_in_band = false;
        if self
            .identity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
            && relay::request_guest_shutdown(&self.tunnel).is_ok()
        {
            let deadline = Instant::now() + DRAIN;
            while !self.host.is_finished() && Instant::now() < deadline {
                thread::sleep(POLL);
            }
            if self.cleaned.load(Ordering::Acquire) {
                cleaned_in_band = matches!(poll_exit(&mut *self.process, deadline), Ok(Some(0)));
            }
        }
        let cancellation = self.process.cancel_io();
        let mut observed = poll_exit(&mut *self.process, Instant::now() + DRAIN);
        if matches!(observed, Ok(None)) {
            let _ = self.process.terminate();
            observed = poll_exit(&mut *self.process, Instant::now() + DRAIN);
        }
        let terminate = if observed.is_ok() {
            self.process.terminate()
        } else {
            Err(io::Error::other(
                "relay identity observation failed; group signaling withheld",
            ))
        };
        let host = self.host.join();
        let stderr = self.stderr.join();
        let reaped = terminate.is_ok() && matches!(self.process.try_wait(), Ok(Some(_)));
        if !reaped {
            marsh_runtime::retain_for_reaping(self.process);
        }
        if !reaped
            || cancellation.is_err()
            || terminate.is_err()
            || host.is_err()
            || stderr.is_err()
        {
            return Err(DaemonError::ShellCleanupUncertain(
                "local relay teardown incomplete".into(),
            ));
        }
        self.identity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .map(|_| RelayStop { cleaned_in_band })
            .ok_or_else(|| {
                DaemonError::ShellCleanupUncertain(
                    "guest relay identity unavailable; local reap is not guest cleanup proof"
                        .into(),
                )
            })
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use marsh_contracts::JobSignal;
    use marsh_runtime::{CommandRunner, Invocation, SystemCommandRunner};
    use std::{io::Read, os::unix::net::UnixStream};

    // Real local process control, not a peer manufacturing successful outcomes.
    // The fixture's single process is kept unreaped by the supervisor while this
    // control polls Linux /proc for actual quiescence after signal dispatch.
    struct LocalControl {
        inner: Arc<dyn AttachmentControl>,
        pid: u32,
        fail: bool,
    }
    impl AttachmentControl for LocalControl {
        fn signal(&self, signal: JobSignal) -> io::Result<()> {
            self.inner.signal(signal)
        }
        fn resize(&self, size: TerminalSize) -> io::Result<()> {
            self.inner.resize(size)
        }
        fn cleanup_session(&self) -> io::Result<()> {
            if self.fail {
                return Err(io::Error::other("control route unavailable"));
            }
            let _ = self.inner.signal(JobSignal::Terminate);
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let state = std::fs::read_to_string(format!("/proc/{}/stat", self.pid));
                if state
                    .as_ref()
                    .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
                    || state.as_ref().is_ok_and(|text| {
                        text.rsplit_once(')')
                            .is_some_and(|(_, tail)| tail.trim_start().starts_with('Z'))
                    })
                {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    self.inner.signal(JobSignal::Kill)?;
                }
                if Instant::now() >= deadline + Duration::from_secs(1) {
                    return Err(io::Error::other("fixture did not terminate"));
                }
                thread::sleep(POLL);
            }
        }
    }

    fn fixture(script: &str, fail: bool) -> Attachment {
        let runner = SystemCommandRunner::new("/tmp");
        let mut process = runner
            .spawn_attached(&Invocation {
                program: "/bin/sh".into(),
                arguments: vec!["-c".into(), format!("echo $$; {script}").into()],
                working_directory: None,
            })
            .unwrap();
        let mut pid = Vec::new();
        loop {
            let mut byte = [0];
            process.stdout.read_exact(&mut byte).unwrap();
            if byte[0] == b'\n' {
                break;
            }
            pid.push(byte[0]);
        }
        let pid = std::str::from_utf8(&pid).unwrap().parse().unwrap();
        process.control = Arc::new(LocalControl {
            inner: Arc::clone(&process.control),
            pid,
            fail,
        });
        process
    }

    #[test]
    fn eof_keeps_real_signal_control_and_trap_status_without_a_lifetime_limit() {
        let process = fixture("trap 'exit 42' TERM; while :; do :; done", false);
        let (server, mut client) = UnixStream::pair().unwrap();
        let (send, receive) = mpsc::channel();
        let task = thread::spawn(move || {
            let outcome = run(process, &ServerAttachment::new(server).unwrap());
            send.send(outcome).unwrap();
        });
        marsh_daemon::write_frame(&mut client, &AttachmentFrame::StdinEof).unwrap();
        thread::sleep(Duration::from_secs(3));
        assert!(matches!(receive.try_recv(), Err(mpsc::TryRecvError::Empty)));
        marsh_daemon::write_frame(
            &mut client,
            &AttachmentFrame::Signal {
                signal: "TERM".into(),
            },
        )
        .unwrap();
        let outcome = receive.recv_timeout(Duration::from_secs(6)).unwrap();
        task.join().unwrap();
        assert_eq!(outcome.runtime_code, Some(42));
        assert!(outcome.delivered, "{outcome:?}");
        assert!(matches!(outcome.cleanup, Cleanup::Verified), "{outcome:?}");
    }

    #[test]
    fn early_real_consumer_preserves_nonzero_status_while_closing_input() {
        let process = fixture("head -c 1 >/dev/null; exit 7", false);
        let (server, mut client) = UnixStream::pair().unwrap();
        let task = thread::spawn(move || run(process, &ServerAttachment::new(server).unwrap()));
        marsh_daemon::write_frame(
            &mut client,
            &AttachmentFrame::Stdin {
                bytes: vec![b'x'; SHELL_STDIN_CHUNK],
            },
        )
        .unwrap();
        marsh_daemon::write_frame(&mut client, &AttachmentFrame::StdinEof).unwrap();
        let outcome = task.join().unwrap();
        assert_eq!(outcome.runtime_code, Some(7));
        assert_eq!(outcome.status(), 7);
        assert!(matches!(outcome.cleanup, Cleanup::Verified));
    }

    #[test]
    fn real_shell_credit_caller_preserves_early_consumer_status_under_input_flood() {
        for expected in [0, 1, 7, 42] {
            let process = fixture(&format!("head -c 1 >/dev/null; exit {expected}"), false);
            let (server, client) = ServerAttachment::shell_pair().unwrap();
            let task = thread::spawn(move || {
                let outcome = run(process, &server);
                server
                    .send(&AttachmentFrame::Exited {
                        code: outcome.status(),
                    })
                    .unwrap();
                outcome
            });
            let input = client.clone();
            let sender = thread::spawn(move || {
                input.send(&AttachmentFrame::Stdin {
                    bytes: vec![b'x'; 8 * 1024 * 1024],
                })
            });
            let code = loop {
                if let AttachmentFrame::Exited { code } = client.receive().unwrap() {
                    break code;
                }
            };
            let sent = sender.join().unwrap();
            let outcome = task.join().unwrap();
            assert!(
                matches!(sent, Err(DaemonError::ShellStdinClosed)),
                "{sent:?}"
            );
            assert_eq!(code, expected, "{outcome:?}");
            assert!(outcome.delivered, "{outcome:?}");
            assert!(matches!(outcome.cleanup, Cleanup::Verified));
        }
    }

    #[test]
    fn blocked_real_stdin_disconnect_escalates_and_reaps() {
        let process = fixture("trap '' TERM; exec sleep 60", false);
        let (server, mut client) = UnixStream::pair().unwrap();
        let task = thread::spawn(move || run(process, &ServerAttachment::new(server).unwrap()));
        for _ in 0..SHELL_STDIN_WINDOW {
            marsh_daemon::write_frame(
                &mut client,
                &AttachmentFrame::Stdin {
                    bytes: vec![b'x'; SHELL_STDIN_CHUNK],
                },
            )
            .unwrap();
        }
        let start = Instant::now();
        drop(client);
        let outcome = task.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(5), "{outcome:?}");
        assert!(matches!(outcome.cleanup, Cleanup::Verified), "{outcome:?}");
        assert!(!outcome.delivered);
    }

    #[test]
    fn paused_real_controller_after_eof_receives_every_byte_and_actual_status() {
        for expected in [42, 0] {
            let process = fixture(
                &format!("head -c 4000000 /dev/zero; exit {expected}"),
                false,
            );
            let (server, client) = ServerAttachment::shell_pair().unwrap();
            let task = thread::spawn(move || {
                let outcome = run(process, &server);
                let _ = server.send(&AttachmentFrame::Exited {
                    code: outcome.status(),
                });
                outcome
            });
            client.send(&AttachmentFrame::StdinEof).unwrap();
            thread::sleep(Duration::from_secs(7));
            let mut output = Vec::new();
            let terminal = loop {
                match client.receive() {
                    Ok(AttachmentFrame::Stdout { bytes }) => output.extend(bytes),
                    Ok(AttachmentFrame::Exited { code }) => break Ok(code),
                    Ok(_) => {}
                    Err(error) => break Err(error),
                }
            };
            let outcome = task.join().unwrap();
            assert_eq!(
                output.len(),
                4_000_000,
                "terminal={terminal:?}, {outcome:?}"
            );
            assert!(output.iter().all(|byte| *byte == 0));
            assert_eq!(terminal.unwrap(), expected, "{outcome:?}");
            assert!(
                outcome.delivered && matches!(outcome.cleanup, Cleanup::Verified),
                "{outcome:?}"
            );
        }
    }

    #[test]
    fn already_exited_producer_keeps_healthy_final_output_backpressure() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("produced");
        let program = format!(
            "import os; os.write(1,b\"\\xff\"*65536); open(\"{}\",\"w\").close(); os._exit(42)",
            marker.display()
        );
        let process = fixture(&format!("exec python3 -c '{program}'"), false);
        let (server, client) = ServerAttachment::shell_pair().unwrap();
        let task = thread::spawn(move || {
            let outcome = run(process, &server);
            let _ = server.send(&AttachmentFrame::Exited {
                code: outcome.status(),
            });
            outcome
        });
        client.send(&AttachmentFrame::StdinEof).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !marker.exists() && Instant::now() < deadline {
            thread::sleep(POLL);
        }
        let produced_before_read = marker.exists();
        thread::sleep(Duration::from_secs(7));
        let mut output = Vec::new();
        let terminal = loop {
            match client.receive() {
                Ok(AttachmentFrame::Stdout { bytes }) => output.extend(bytes),
                Ok(AttachmentFrame::Exited { code }) => break Ok(code),
                Ok(_) => {}
                Err(error) => break Err(error),
            }
        };
        let outcome = task.join().unwrap();
        assert!(
            produced_before_read,
            "fixture did not finish writing before pause: {outcome:?}"
        );
        assert_eq!(output, vec![255; 65536], "{outcome:?}");
        // The producer finished before the 7 s pause; every byte still arrived.
        assert_eq!(terminal.unwrap(), 42);
    }

    #[test]
    fn unknown_signal_and_helper_rejection_do_not_lose_actual_terminal() {
        struct RejectSignal(Arc<dyn AttachmentControl>, bool);
        impl AttachmentControl for RejectSignal {
            fn signal(&self, _: JobSignal) -> io::Result<()> {
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    if self.1 {
                        format!("foreground leader failed: {}", "λ".repeat(600_000))
                    } else {
                        "foreground leader already exited".into()
                    },
                ))
            }
            fn resize(&self, size: TerminalSize) -> io::Result<()> {
                self.0.resize(size)
            }
            fn cleanup_session(&self) -> io::Result<()> {
                self.0.cleanup_session()
            }
        }
        for signal in ["USR1", "INT", "TERM"] {
            let mut process = fixture("trap 'exit 3' INT; sleep 1; exit 42", false);
            if signal != "USR1" {
                process.control = Arc::new(RejectSignal(process.control, signal == "TERM"));
            }
            let (server, client) = ServerAttachment::shell_pair().unwrap();
            let task = thread::spawn(move || {
                let outcome = run(process, &server);
                server
                    .send(&AttachmentFrame::Exited {
                        code: outcome.status(),
                    })
                    .unwrap();
                outcome
            });
            client.send(&AttachmentFrame::StdinEof).unwrap();
            client
                .send(&AttachmentFrame::Signal {
                    signal: signal.into(),
                })
                .unwrap();
            let mut rejected = false;
            let terminal = loop {
                match client.receive().unwrap() {
                    AttachmentFrame::ControlError {
                        code,
                        operation,
                        message,
                    } => {
                        assert_eq!(
                            code,
                            if signal == "USR1" {
                                AttachmentControlError::UnsupportedSignal
                            } else {
                                AttachmentControlError::SignalDelivery
                            }
                        );
                        assert_eq!(operation, "signal");
                        if signal == "TERM" {
                            assert!(message.ends_with(" [diagnostic truncated]"));
                            assert!(message.len() < 1100);
                        }
                        assert!(message.contains(if signal == "USR1" {
                            "unsupported signal"
                        } else {
                            "foreground leader"
                        }));
                        rejected = true;
                    }
                    AttachmentFrame::Exited { code } => break code,
                    _ => {}
                }
            };
            let outcome = task.join().unwrap();
            assert!(rejected && outcome.delivered, "{outcome:?}");
            assert_eq!(terminal, 42, "{outcome:?}");
        }
    }

    #[test]
    fn full_output_does_not_block_actual_cancel_or_disconnect_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let ready = root.path().join("ready");
        let cancelled = root.path().join("cancelled");
        let program = format!(
            "import os,signal; signal.signal(signal.SIGTERM, lambda *_: (open(\"{}\",\"w\").write(\"cancelled\"),os._exit(42))); open(\"{}\",\"w\").close(); os.write(1,b\"x\"*4000000)",
            cancelled.display(),
            ready.display()
        );
        let process = fixture(&format!("exec python3 -c '{program}'"), false);
        let (server, client) = ServerAttachment::shell_pair().unwrap();
        let task = thread::spawn(move || run(process, &server));
        client.send(&AttachmentFrame::StdinEof).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !ready.exists() && Instant::now() < deadline {
            thread::sleep(POLL);
        }
        let was_ready = ready.exists();
        thread::sleep(Duration::from_millis(100));
        let start = Instant::now();
        client
            .send(&AttachmentFrame::Signal {
                signal: "TERM".into(),
            })
            .unwrap();
        while !cancelled.exists() && start.elapsed() < Duration::from_secs(2) {
            thread::sleep(POLL);
        }
        let cancellation_observed = cancelled.exists();
        // No reads at all: a real peer close, rather than a healthy pause,
        // now wakes every writer and joins all owned I/O workers.
        client.shutdown().unwrap();
        let outcome = task.join().unwrap();
        assert!(
            was_ready && cancellation_observed,
            "cancel blocked behind full output: {outcome:?}"
        );
        assert!(start.elapsed() < Duration::from_secs(5), "{outcome:?}");
        assert_eq!(outcome.runtime_code, Some(42));
        assert!(
            !outcome.delivered && matches!(outcome.cleanup, Cleanup::Verified),
            "{outcome:?}"
        );
        assert_eq!(outcome.status(), 125);
    }

    #[test]
    fn failed_guest_control_remains_uncertain_after_local_process_is_reaped() {
        let process = fixture("exec sleep 60", true);
        let (server, client) = UnixStream::pair().unwrap();
        drop(client);
        let start = Instant::now();
        let outcome = run(process, &ServerAttachment::new(server).unwrap());
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(
            matches!(outcome.cleanup, Cleanup::Uncertain(_)),
            "{outcome:?}"
        );
        assert_eq!(outcome.runtime_code, None);
        assert_eq!(outcome.status(), 125);
    }

    #[test]
    fn nonresponsive_real_relay_is_canceled_and_reaped_without_readiness_or_eof() {
        let process = SystemCommandRunner::new("/tmp")
            .spawn_attached(&Invocation {
                program: "/bin/sleep".into(),
                arguments: vec!["60".into()],
                working_directory: None,
            })
            .unwrap();
        let relay = Relay::start(
            process,
            PathBuf::from("/nonexistent-daemon"),
            "a".repeat(64),
        )
        .unwrap();
        let start = Instant::now();
        assert!(matches!(
            relay.stop(),
            Err(DaemonError::ShellCleanupUncertain(_))
        ));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn real_controller_disconnect_terminates_running_shell_attachment() {
        let process = fixture("exec sleep 60", false);
        let (server, client) = UnixStream::pair().unwrap();
        drop(client);
        let start = Instant::now();
        let outcome = run(process, &ServerAttachment::new(server).unwrap());
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(matches!(outcome.cleanup, Cleanup::Verified), "{outcome:?}");
        assert!(!outcome.delivered);
    }
}
