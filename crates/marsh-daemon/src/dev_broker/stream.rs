//! Duplex stream for one `DevSbx` call: the host serves a stock `sbx`
//! process; the guest shim relays its own stdio. Salvaged from the former
//! broker attach framing.

use crate::{DaemonError, read_frame, write_frame};
use marsh_contracts::{JobSignal, TerminalSize};
use marsh_runtime::{AttachedProcess, Attachment, CommandRunner, Invocation};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, IsTerminal as _, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const CHUNK: usize = 4096;
const INPUT_QUEUE: usize = crate::SHELL_STDIN_WINDOW;
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const CANCEL_GRACE: Duration = Duration::from_millis(500);
const REAP_TIMEOUT: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(10);

fn kill_and_reap(process: &mut dyn AttachedProcess) -> io::Result<i32> {
    // Trusted HOST carrier only: invoke native owned-PGID cleanup before reap.
    // Linux killpg can partially deliver across changed UIDs; setsid can leave
    // that group. This is NOT arbitrary-descendant or remote-effect closure.
    // Releasing authority still requires independent stock UUID/export absence
    // or the guest session's cgroup proof, owned by those lifecycle layers.
    process.terminate()?;
    let deadline = Instant::now() + REAP_TIMEOUT;
    loop {
        if let Some(code) = process.try_wait()? {
            return Ok(code);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "broker process reap incomplete",
            ));
        }
        thread::sleep(POLL);
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Frame {
    // Do not let an older peer silently stall after the initial stdin window.
    // `pty`: the stock process has a terminal; the caller goes raw.
    #[serde(rename = "started_credited")]
    Started {
        #[serde(default)]
        pty: bool,
    },
    Error {
        message: String,
    },
    Input {
        bytes: Vec<u8>,
    },
    InputEof,
    InputCredit,
    InputClosed,
    Output {
        bytes: Vec<u8>,
    },
    Diagnostic {
        bytes: Vec<u8>,
    },
    Signal {
        signal: JobSignal,
    },
    Resize {
        size: TerminalSize,
    },
    Exit {
        code: i32,
    },
}

/// Refuse a call before any stock process starts.
pub fn reject(stream: &UnixStream, message: &str) -> Result<(), DaemonError> {
    send(
        &Arc::new(BrokerWriter::new(stream.try_clone()?)),
        &Frame::Error {
            message: message.to_owned(),
        },
    )?;
    finish(stream);
    Ok(())
}

/// Close gracefully: half-close, then consume the peer's unread frames until
/// it closes. Closing a Linux Unix socket with unread input resets the peer,
/// which a relay hop would turn into loss of the final frames.
fn finish(stream: &UnixStream) {
    let _ = stream.shutdown(Shutdown::Write);
    let _ = stream.set_read_timeout(Some(DRAIN_TIMEOUT));
    let mut sink = [0_u8; CHUNK];
    let mut reader = stream;
    while matches!(reader.read(&mut sink), Ok(count) if count > 0) {}
}

/// Deliver an already captured (and possibly filtered) stock result.
pub fn reply_captured(
    stream: &UnixStream,
    stdout: &[u8],
    stderr: &[u8],
    code: i32,
) -> Result<(), DaemonError> {
    let writer = Arc::new(BrokerWriter::new(stream.try_clone()?));
    send(&writer, &Frame::Started { pty: false })?;
    for chunk in stdout.chunks(CHUNK) {
        send(
            &writer,
            &Frame::Output {
                bytes: chunk.to_vec(),
            },
        )?;
    }
    for chunk in stderr.chunks(CHUNK) {
        send(
            &writer,
            &Frame::Diagnostic {
                bytes: chunk.to_vec(),
            },
        )?;
    }
    send(&writer, &Frame::Exit { code })?;
    finish(stream);
    Ok(())
}

/// Guest side: relay this process's stdio over an accepted `DevSbx` stream
/// and return the stock exit status (125 for a refusal or lost stream).
#[allow(clippy::too_many_lines)] // Input pump and frame loop share credits.
pub fn run_client(stream: UnixStream) -> i32 {
    let Ok(write_half) = stream.try_clone() else {
        return 125;
    };
    let writer = Arc::new(BrokerWriter::new(write_half));
    let credits = Arc::new((
        Mutex::new(crate::SHELL_STDIN_WINDOW),
        std::sync::Condvar::new(),
    ));
    let input_done = Arc::new(AtomicBool::new(false));
    {
        let writer = Arc::clone(&writer);
        let credits = Arc::clone(&credits);
        let input_done = Arc::clone(&input_done);
        thread::spawn(move || {
            let mut stdin = io::stdin().lock();
            let mut chunk = [0_u8; CHUNK];
            loop {
                let count = match stdin.read(&mut chunk) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) | Ok(0) => 0,
                    Ok(count) => count,
                };
                if input_done.load(Ordering::Acquire) {
                    return;
                }
                if count == 0 {
                    let _ = send(&writer, &Frame::InputEof);
                    return;
                }
                {
                    let (lock, changed) = &*credits;
                    let mut available = lock
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    while *available == 0 && !input_done.load(Ordering::Acquire) {
                        available = changed
                            .wait(available)
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                    if input_done.load(Ordering::Acquire) {
                        return;
                    }
                    *available -= 1;
                }
                if send(
                    &writer,
                    &Frame::Input {
                        bytes: chunk[..count].to_vec(),
                    },
                )
                .is_err()
                {
                    return;
                }
            }
        });
    }
    // Restored on every return (dropped after the frame loop).
    let mut raw: Option<crate::HostTerminalMode> = None;
    let mut reader = stream;
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let stop_input = || {
        input_done.store(true, Ordering::Release);
        credits.1.notify_all();
    };
    loop {
        match read_frame::<Frame>(&mut reader) {
            Ok(Frame::Started { pty }) => {
                if pty && raw.is_none() && io::stdin().is_terminal() {
                    raw = crate::HostTerminalMode::enter(io::stdin()).ok();
                    if raw.is_some() {
                        spawn_resize_relay(&writer, &input_done);
                    }
                }
            }
            Ok(Frame::Output { bytes }) => {
                if stdout
                    .write_all(&bytes)
                    .and_then(|()| stdout.flush())
                    .is_err()
                {
                    stop_input();
                    return 125;
                }
            }
            Ok(Frame::Diagnostic { bytes }) => {
                let _ = stderr.write_all(&bytes);
                let _ = stderr.flush();
            }
            Ok(Frame::InputCredit) => {
                let (lock, changed) = &*credits;
                *lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
                changed.notify_all();
            }
            Ok(Frame::InputClosed) => stop_input(),
            Ok(Frame::Exit { code }) => {
                stop_input();
                return code;
            }
            Ok(Frame::Error { message }) => {
                stop_input();
                let _ = writeln!(stderr, "sbx: dev broker refused: {message}");
                return 125;
            }
            Ok(_) | Err(_) => {
                stop_input();
                let _ = writeln!(stderr, "sbx: dev broker stream lost");
                return 125;
            }
        }
    }
}

/// Forward the caller's terminal size changes while a PTY call runs.
fn spawn_resize_relay(writer: &SharedWriter, done: &Arc<AtomicBool>) {
    let writer = Arc::clone(writer);
    let done = Arc::clone(done);
    thread::spawn(move || {
        let size = || {
            rustix::termios::tcgetwinsize(io::stdin())
                .ok()
                .filter(|size| size.ws_row != 0 && size.ws_col != 0)
                .map(|size| (size.ws_row, size.ws_col))
        };
        let mut last = size();
        while !done.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(200));
            let now = size();
            if now != last
                && let Some((rows, columns)) = now
            {
                last = now;
                if send(
                    &writer,
                    &Frame::Resize {
                        size: TerminalSize { rows, columns },
                    },
                )
                .is_err()
                {
                    return;
                }
            }
        }
    });
}

// Connected peers backpressure whole frames without a write/admission deadline.
// Explicit cancellation or independently observed loss wakes blocked writers.
struct BrokerWriter {
    stream: UnixStream,
    lock: Mutex<()>,
}

type SharedWriter = Arc<BrokerWriter>;

impl BrokerWriter {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            lock: Mutex::new(()),
        }
    }
}

struct PollingWriter<'a> {
    stream: &'a UnixStream,
    cancellation: Option<&'a Mutex<Option<Instant>>>,
}

impl Write for PollingWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(POLL))?;
        loop {
            if self.cancellation.is_some_and(|deadline| {
                deadline
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_some_and(|deadline| Instant::now() >= deadline)
            }) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "broker delivery cancelled",
                ));
            }
            match self.stream.write(bytes) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(()) // UnixStream is unbuffered.
    }
}

fn send(writer: &SharedWriter, frame: &Frame) -> io::Result<()> {
    let result = {
        let _guard = writer
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        write_frame(
            &mut PollingWriter {
                stream: &writer.stream,
                cancellation: None,
            },
            frame,
        )
        .map_err(io::Error::other)
    };
    if result.is_err() {
        let _ = writer.stream.shutdown(Shutdown::Both);
    }
    result
}

fn forward_output(
    mut source: Box<dyn Read + Send>,
    writer: &SharedWriter,
    diagnostic: bool,
    cancelled: &AtomicBool,
) -> io::Result<()> {
    let mut chunk = [0_u8; CHUNK];
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(io::Error::other("broker output drain cancelled"));
        }
        let count = match source.read(&mut chunk) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            return Ok(());
        }
        let frame = if diagnostic {
            Frame::Diagnostic {
                bytes: chunk[..count].to_vec(),
            }
        } else {
            Frame::Output {
                bytes: chunk[..count].to_vec(),
            }
        };
        send(writer, &frame)?;
    }
}

fn forward_input(
    mut stdin: Box<dyn Write + Send>,
    input_rx: mpsc::Receiver<Vec<u8>>,
    writer: &SharedWriter,
    stopped: &AtomicBool,
    input_closed: &AtomicBool,
) -> io::Result<()> {
    while !stopped.load(Ordering::Acquire) && !input_closed.load(Ordering::Acquire) {
        let bytes = match input_rx.recv_timeout(POLL) {
            Ok(bytes) => bytes,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match stdin.write_all(&bytes) {
            Err(error)
                if error.kind() == io::ErrorKind::BrokenPipe
                    || input_closed.load(Ordering::Acquire) =>
            {
                break;
            }
            result => result?,
        }
        send(writer, &Frame::InputCredit)?;
    }
    // A consumer may stop reading long before it exits. Revoke stdin
    // and discard the bounded queue without touching output or status.
    input_closed.store(true, Ordering::Release);
    drop(input_rx);
    drop(stdin);
    send(writer, &Frame::InputClosed)
}

// Wake an idle or partial-frame reader after reaping without shutting down the
// socket's read half: in-flight stdin must not fail before Exit is delivered.
struct StoppableReader<'a> {
    stream: &'a mut UnixStream,
    stopped: &'a AtomicBool,
}

impl Read for StoppableReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.stopped.load(Ordering::Relaxed) {
                return Ok(0);
            }
            match self.stream.read(bytes) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                result => return result,
            }
        }
    }
}

/// Serve one admitted stock-SBX attachment with a host-owned revocation
/// fence independent of peer EOF. Never replays: loss kills and reaps.
#[allow(clippy::too_many_lines)] // Admission, pipe ownership and supervisor handoff stay together.
pub fn serve(
    mut stream: UnixStream,
    invocation: &Invocation,
    pty: Option<TerminalSize>,
    runner: &dyn CommandRunner,
    revoked: &AtomicBool,
) -> Result<(), DaemonError> {
    let writer = Arc::new(BrokerWriter::new(stream.try_clone()?));
    stream.set_read_timeout(Some(POLL))?;
    let attachment = match pty {
        Some(size) => runner.spawn_pty_sized(invocation, size),
        None => runner.spawn_attached(invocation),
    };
    let Attachment {
        stdin,
        stdout,
        stderr,
        mut process,
        control,
    } = match attachment {
        Ok(value) => value,
        Err(error) => {
            send(
                &writer,
                &Frame::Error {
                    message: error.to_string(),
                },
            )?;
            return Err(error.into());
        }
    };
    if !process.supports_io_cancellation() {
        let cleanup = kill_and_reap(&mut *process);
        if cleanup.is_err() {
            marsh_runtime::retain_for_reaping(process);
        }
        let message = format!("opaque broker attachment rejected; cleanup: {cleanup:?}");
        let _ = send(
            &writer,
            &Frame::Error {
                message: message.clone(),
            },
        );
        return Err(DaemonError::InvalidState(message));
    }
    if let Err(error) = send(&writer, &Frame::Started { pty: pty.is_some() }) {
        let cancellation = process.cancel_io();
        let cleanup = kill_and_reap(&mut *process);
        if cleanup.is_err() {
            marsh_runtime::retain_for_reaping(process);
        }
        return Err(DaemonError::InvalidState(format!(
            "broker start delivery: {error}; I/O: {cancellation:?}; cleanup: {cleanup:?}"
        )));
    }
    let stopped = Arc::new(AtomicBool::new(false));
    let input_closed = Arc::new(AtomicBool::new(false));
    let output_cancelled = Arc::new(AtomicBool::new(false));
    let cancel_deadline = Arc::new(Mutex::new(None));
    let [output, diagnostic] = [(stdout, false), (stderr, true)].map(|(source, diagnostic)| {
        let writer = Arc::clone(&writer);
        let cancelled = Arc::clone(&output_cancelled);
        thread::spawn(move || forward_output(source, &writer, diagnostic, &cancelled))
    });
    let (input_tx, input_rx) = mpsc::sync_channel::<Vec<u8>>(INPUT_QUEUE);
    let input = {
        let writer = Arc::clone(&writer);
        let stopped = Arc::clone(&stopped);
        let input_closed = Arc::clone(&input_closed);
        thread::spawn(move || forward_input(stdin, input_rx, &writer, &stopped, &input_closed))
    };
    let reader = {
        let stopped = Arc::clone(&stopped);
        let input_closed = Arc::clone(&input_closed);
        let cancel_deadline = Arc::clone(&cancel_deadline);
        let control = Arc::clone(&control);
        thread::spawn(move || {
            let mut input_tx = Some(input_tx);
            loop {
                let frame = read_frame::<Frame>(&mut StoppableReader {
                    stream: &mut stream,
                    stopped: &stopped,
                });
                if stopped.load(Ordering::Relaxed) {
                    return Ok(());
                }
                match frame {
                    Ok(Frame::Input { bytes }) if bytes.len() <= CHUNK => {
                        if input_closed.load(Ordering::Acquire) {
                            input_tx.take();
                            continue;
                        }
                        // No socket/control reader may wait for the stdin sink.
                        // An over-window peer still fails while input is open;
                        // closure racing try_send only discards in-flight input.
                        let result = input_tx
                            .as_ref()
                            .ok_or_else(|| io::Error::other("broker stdin already closed"))?
                            .try_send(bytes);
                        if result.is_err() && !input_closed.load(Ordering::Acquire) {
                            return Err(io::Error::other("broker stdin window exceeded or closed"));
                        }
                    }
                    Ok(Frame::InputEof)
                        if input_closed.load(Ordering::Acquire) || input_tx.is_some() =>
                    {
                        // EOF does not consume a queue slot. Drain before close.
                        input_tx.take();
                    }
                    Ok(Frame::Signal { signal }) => {
                        // This broker's signals are cancellation-only, with
                        // bounded escalation; they do not implement job pause.
                        cancel_deadline
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .get_or_insert(Instant::now() + CANCEL_GRACE);
                        // A failed courtesy signal is not controller loss.
                        // The supervisor still enforces cancellation/cleanup;
                        // a signal racing actual exit must not erase its status.
                        let _ = control.signal(signal);
                    }
                    Ok(Frame::Resize { size }) => {
                        let _ = control.resize(size);
                    }
                    Ok(_) => return Err(io::Error::other("unexpected broker input frame")),
                    Err(error) => return Err(io::Error::other(error)),
                }
            }
        })
    };
    supervise_attach(
        &writer,
        process,
        &stopped,
        &input_closed,
        &output_cancelled,
        &cancel_deadline,
        [output, diagnostic, input, reader],
        revoked,
    )
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "one owner retains process identity and every joined worker through cleanup"
)]
fn supervise_attach(
    writer: &SharedWriter,
    mut process: Box<dyn AttachedProcess>,
    stopped: &AtomicBool,
    input_closed: &AtomicBool,
    output_cancelled: &AtomicBool,
    cancel_deadline: &Mutex<Option<Instant>>,
    workers: [thread::JoinHandle<io::Result<()>>; 4],
    revoked: &AtomicBool,
) -> Result<(), DaemonError> {
    let mut workers = workers.map(Some);
    let mut status = None;
    let mut failure = None;
    let mut cleanup_attempted = false;
    let mut identity_uncertain = false;
    let mut reaped = false;
    let mut loss_deadline = None;
    loop {
        if revoked.load(Ordering::Acquire) {
            failure.get_or_insert_with(|| "broker scope revoked".into());
        }
        for worker in &mut workers {
            if worker.as_ref().is_some_and(thread::JoinHandle::is_finished)
                && let Some(worker) = worker.take()
                && let Err(error) = worker
                    .join()
                    .unwrap_or_else(|_| Err(io::Error::other("broker I/O worker panicked")))
            {
                failure.get_or_insert_with(|| error.to_string());
            }
        }
        if status.is_none() && !cleanup_attempted {
            // Never release the local PID/PGID identity before native cleanup.
            match process.try_wait_unreaped() {
                Ok(code) => status = code,
                Err(error) => {
                    identity_uncertain = true;
                    failure.get_or_insert_with(|| error.to_string());
                }
            }
        }
        let cancel_expired = cancel_deadline
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|deadline| Instant::now() >= deadline);
        if cancel_expired && workers[..3].iter().any(Option::is_some) {
            failure.get_or_insert_with(|| "broker cancellation drain deadline reached".into());
        }
        if failure.is_some() {
            // Explicit loss/revocation/cancel, never a healthy slow reader.
            stopped.store(true, Ordering::Release);
            input_closed.store(true, Ordering::Release);
            output_cancelled.store(true, Ordering::Release);
            let _ = writer.stream.shutdown(Shutdown::Both);
            if loss_deadline.is_none() {
                loss_deadline = Some(Instant::now() + DRAIN_TIMEOUT);
                if let Err(error) = process.cancel_io() {
                    failure = Some(format!("broker I/O cancellation unverified: {error}"));
                }
            }
        }
        if !cleanup_attempted && (status.is_some() || failure.is_some()) {
            input_closed.store(true, Ordering::Release);
            // Mandatory on NORMAL completion too: EOF + leader exit is not
            // group absence. Native terminate owns the exact identity proof.
            let cleanup = if identity_uncertain {
                Err(io::Error::other(
                    "termination withheld after uncertain child identity observation",
                ))
            } else {
                kill_and_reap(&mut *process)
            };
            match cleanup {
                Ok(code) => {
                    status.get_or_insert(code);
                    reaped = true;
                }
                Err(error) => {
                    failure = Some(format!("broker process cleanup unverified: {error}"));
                }
            }
            cleanup_attempted = true;
        }
        if cleanup_attempted && workers[..3].iter().all(Option::is_none) {
            stopped.store(true, Ordering::Release);
        }
        if cleanup_attempted && workers.iter().all(Option::is_none) {
            break;
        }
        if loss_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            // Only cancellation-capable attachments were admitted. Wake every
            // endpoint before joining; never detach an opaque blocking copy.
            let _ = process.cancel_io();
            let _ = writer.stream.shutdown(Shutdown::Both);
            for worker in workers.iter_mut().filter_map(Option::take) {
                if let Err(error) = worker
                    .join()
                    .unwrap_or_else(|_| Err(io::Error::other("broker I/O worker panicked")))
                {
                    failure.get_or_insert_with(|| error.to_string());
                }
            }
            break;
        }
        thread::sleep(POLL);
    }
    if !reaped {
        marsh_runtime::retain_for_reaping(process);
    }
    let result = match failure {
        Some(message) => Err(DaemonError::InvalidState(format!(
            "{message}; observed process exit: {status:?}; public status 125"
        ))),
        None => send(
            writer,
            &Frame::Exit {
                code: status.ok_or_else(|| {
                    DaemonError::InvalidState("broker terminal unavailable".into())
                })?,
            },
        )
        .map_err(DaemonError::from),
    };
    if result.is_ok() {
        finish(&writer.stream);
    } else {
        let _ = writer.stream.shutdown(Shutdown::Both);
    }
    result
}
