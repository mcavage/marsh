//! One retained, generation-checked supervisor transport per shell VM.
//!
//! The daemon keeps a single `sbx exec -i -u root VM marsh --internal-supervisor`
//! per warm shell VM. Every shell session and its credential relay is a child
//! of that root supervisor, addressed by a transport-local attempt number.
//! Stdin, output, resize, signals, exit status and verified cleanup all travel
//! as attempt-keyed bounded frames, so a warm shell starts no stock process.
//!
//! Rules (shared with the kit worker transport, see `docs/model/Worker.tla`):
//! - An attempt belongs to exactly one transport generation and is never
//!   replayed. Transport loss makes every unreleased attempt uncertain.
//! - Output and input are credit-windowed per attempt and stream, so one slow
//!   consumer never blocks the transport reader or a sibling session.
//! - Cleanup is reported in-band by the supervisor after it killed and reaped
//!   the session's cgroup (`populated 0`), never inferred from a local reap.

use marsh_contracts::{JobSignal, TerminalSize};
use marsh_runtime::{AttachedProcess, Attachment, AttachmentControl};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    io::{self, Read, Write},
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

/// Bytes per output/input chunk.
pub const CHUNK: usize = 32 * 1024;
/// Chunks in flight per attempt stream before the producer waits for credit.
pub const WINDOW: usize = 16;
const MAX_HEADER: usize = 256 * 1024;
pub const STDOUT: u8 = 1;
pub const STDERR: u8 = 2;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);
const START_TIMEOUT: Duration = Duration::from_secs(15);

/// How the supervisor starts one attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum StartKind {
    /// A contained shell: new session, optional PTY, `--internal-record-session`
    /// enrollment. `Started` waits for root-owned containment publication.
    Shell {
        record: Vec<u8>,
        uid: u32,
        terminal: Option<(u16, u16)>,
    },
    /// A plain child running as `uid:gid` in its own process group.
    Child { uid: u32, gid: u32 },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StartSpec {
    pub kind: StartKind,
    pub program: Vec<u8>,
    pub arguments: Vec<Vec<u8>>,
    pub environment: Vec<(Vec<u8>, Vec<u8>)>,
    pub working_directory: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlOp {
    Signal(String),
    Resize {
        rows: u16,
        columns: u16,
    },
    /// Kill the attempt's whole process tree and verify it is gone.
    Cleanup,
}

/// Host to supervisor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Down {
    Hello {
        generation: u64,
    },
    Ping {
        generation: u64,
        nonce: u64,
    },
    Start {
        attempt: u64,
        spec: StartSpec,
    },
    /// Payload carries the bytes.
    Input {
        attempt: u64,
    },
    InputEof {
        attempt: u64,
    },
    OutputCredit {
        attempt: u64,
        stream: u8,
    },
    Control {
        attempt: u64,
        seq: u64,
        op: ControlOp,
    },
    /// The host retains nothing more for this attempt.
    Release {
        attempt: u64,
    },
}

/// Supervisor to host.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Up {
    Ready {
        generation: u64,
    },
    Pong {
        generation: u64,
        nonce: u64,
    },
    Started {
        attempt: u64,
    },
    StartFailed {
        attempt: u64,
        message: String,
    },
    /// Payload carries the bytes.
    Output {
        attempt: u64,
        stream: u8,
    },
    OutputEof {
        attempt: u64,
        stream: u8,
    },
    InputCredit {
        attempt: u64,
    },
    InputClosed {
        attempt: u64,
    },
    Exited {
        attempt: u64,
        code: i32,
    },
    /// A contained shell's leader exited; the supervisor already killed and
    /// verified its whole cgroup (`error` is `None`) or could not.
    Cleaned {
        attempt: u64,
        error: Option<String>,
    },
    Done {
        attempt: u64,
        seq: u64,
        error: Option<String>,
    },
}

/// Writes one frame: big-endian header length, JSON header, payload length, payload.
///
/// # Errors
/// Returns I/O errors or an oversized frame.
pub fn write_frame<T: Serialize>(
    writer: &mut impl Write,
    header: &T,
    payload: &[u8],
) -> io::Result<()> {
    let header = serde_json::to_vec(header).map_err(io::Error::other)?;
    if header.len() > MAX_HEADER || payload.len() > CHUNK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "oversized supervisor frame",
        ));
    }
    let mut frame = Vec::with_capacity(8 + header.len() + payload.len());
    frame.extend_from_slice(
        &u32::try_from(header.len())
            .map_err(io::Error::other)?
            .to_be_bytes(),
    );
    frame.extend_from_slice(&header);
    frame.extend_from_slice(
        &u32::try_from(payload.len())
            .map_err(io::Error::other)?
            .to_be_bytes(),
    );
    frame.extend_from_slice(payload);
    writer.write_all(&frame)?;
    writer.flush()
}

/// Reads one frame written by [`write_frame`].
///
/// # Errors
/// Returns I/O errors, EOF, or an invalid/oversized frame.
pub fn read_frame<T: for<'de> Deserialize<'de>>(
    reader: &mut impl Read,
) -> io::Result<(T, Vec<u8>)> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let header_length = u32::from_be_bytes(length) as usize;
    if header_length == 0 || header_length > MAX_HEADER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid supervisor frame header",
        ));
    }
    let mut header = vec![0_u8; header_length];
    reader.read_exact(&mut header)?;
    reader.read_exact(&mut length)?;
    let payload_length = u32::from_be_bytes(length) as usize;
    if payload_length > CHUNK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized supervisor payload",
        ));
    }
    let mut payload = vec![0_u8; payload_length];
    reader.read_exact(&mut payload)?;
    let header = serde_json::from_slice(&header).map_err(io::Error::other)?;
    Ok((header, payload))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lost() -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "shell supervisor transport lost; session cleanup uncertain",
    )
}

#[derive(Default)]
struct AttemptState {
    output: [VecDeque<Vec<u8>>; 2],
    output_eof: [bool; 2],
    input_credit: usize,
    input_closed: bool,
    started: Option<Result<(), String>>,
    exit: Option<i32>,
    cleaned: Option<Result<(), String>>,
    done: BTreeMap<u64, Option<String>>,
    lost: bool,
    cancelled: bool,
}

struct AttemptShared {
    state: Mutex<AttemptState>,
    changed: Condvar,
}

impl AttemptShared {
    fn update(&self, change: impl FnOnce(&mut AttemptState)) {
        change(&mut lock(&self.state));
        self.changed.notify_all();
    }
}

/// Host side of one supervisor transport generation.
pub struct Supervisor {
    generation: u64,
    writer: Mutex<Box<dyn Write + Send>>,
    attempts: Mutex<BTreeMap<u64, Arc<AttemptShared>>>,
    next_attempt: AtomicU64,
    next_seq: AtomicU64,
    next_nonce: AtomicU64,
    pongs: Mutex<BTreeMap<u64, mpsc::SyncSender<u64>>>,
    alive: AtomicBool,
    last_frame: Mutex<Instant>,
    process: Mutex<Option<Box<dyn AttachedProcess>>>,
}

impl Supervisor {
    /// Takes ownership of the spawned `sbx exec ... --internal-supervisor` and
    /// completes the generation handshake.
    ///
    /// # Errors
    /// Returns an error when the supervisor does not acknowledge this generation.
    pub fn start(
        attachment: Attachment,
        generation: u64,
        timeout: Duration,
    ) -> io::Result<Arc<Self>> {
        let Attachment {
            stdin,
            mut stdout,
            mut stderr,
            process,
            control: _,
        } = attachment;
        let supervisor = Arc::new(Self {
            generation,
            writer: Mutex::new(stdin),
            attempts: Mutex::new(BTreeMap::new()),
            next_attempt: AtomicU64::new(1),
            next_seq: AtomicU64::new(1),
            next_nonce: AtomicU64::new(1),
            pongs: Mutex::new(BTreeMap::new()),
            alive: AtomicBool::new(true),
            last_frame: Mutex::new(Instant::now()),
            process: Mutex::new(Some(process)),
        });
        thread::spawn(move || io::copy(&mut stderr, &mut io::sink()));
        let (ready_send, ready) = mpsc::sync_channel(1);
        let reader = Arc::clone(&supervisor);
        thread::spawn(move || {
            let first = read_frame::<Up>(&mut stdout);
            let ok = matches!(&first, Ok((Up::Ready { generation }, _)) if *generation == reader.generation);
            let _ = ready_send.send(ok);
            if ok {
                reader.read_loop(&mut stdout);
            }
            reader.lose();
        });
        if let Err(error) = supervisor.send(&Down::Hello { generation }, &[]) {
            supervisor.shutdown();
            return Err(error);
        }
        if ready.recv_timeout(timeout) != Ok(true) {
            supervisor.shutdown();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "shell supervisor did not acknowledge its generation",
            ));
        }
        Ok(supervisor)
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// True while the transport reader and the local stock process are live.
    pub fn alive(&self) -> bool {
        if !self.alive.load(Ordering::Acquire) {
            return false;
        }
        let exited = lock(&self.process)
            .as_mut()
            .is_none_or(|process| !matches!(process.try_wait_unreaped(), Ok(None)));
        if exited {
            self.lose();
        }
        !exited
    }

    /// Attempts started on this transport and not yet released by the host.
    pub fn active_attempts(&self) -> usize {
        lock(&self.attempts).len()
    }

    /// Liveness check before reuse: a frame from this generation within
    /// `recent` is proof enough; otherwise a ping round trip.
    ///
    /// # Errors
    /// Returns an error if the transport is lost or answers another generation.
    pub fn check_live(&self, recent: Duration, timeout: Duration) -> io::Result<()> {
        if !self.alive() {
            return Err(lost());
        }
        if lock(&self.last_frame).elapsed() < recent {
            return Ok(());
        }
        self.ping(timeout)
    }

    /// Round-trip liveness and generation check.
    ///
    /// # Errors
    /// Returns an error if the transport is lost or answers another generation.
    pub fn ping(&self, timeout: Duration) -> io::Result<()> {
        let nonce = self.next_nonce.fetch_add(1, Ordering::Relaxed);
        let (send, receive) = mpsc::sync_channel(1);
        lock(&self.pongs).insert(nonce, send);
        let result = self
            .send(
                &Down::Ping {
                    generation: self.generation,
                    nonce,
                },
                &[],
            )
            .and_then(|()| receive.recv_timeout(timeout).map_err(|_| lost()));
        lock(&self.pongs).remove(&nonce);
        match result {
            Ok(generation) if generation == self.generation => Ok(()),
            Ok(_) => {
                self.lose();
                Err(io::Error::other("shell supervisor generation mismatch"))
            }
            Err(error) => {
                self.lose();
                Err(error)
            }
        }
    }

    /// Stops the transport. Unreleased attempts become uncertain.
    pub fn shutdown(&self) -> Option<Box<dyn AttachedProcess>> {
        self.lose();
        lock(&self.process).take()
    }

    fn lose(&self) {
        self.alive.store(false, Ordering::Release);
        for attempt in lock(&self.attempts).values() {
            attempt.update(|state| state.lost = true);
        }
        lock(&self.pongs).clear();
    }

    fn send(&self, header: &Down, payload: &[u8]) -> io::Result<()> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(lost());
        }
        let result = write_frame(&mut *lock(&self.writer), header, payload);
        if result.is_err() {
            self.lose();
        }
        result
    }

    fn read_loop(&self, stdout: &mut dyn Read) {
        let mut reader = io::BufReader::with_capacity(2 * CHUNK, stdout);
        while let Ok((frame, payload)) = read_frame::<Up>(&mut reader) {
            *lock(&self.last_frame) = Instant::now();
            if let Up::Pong { generation, nonce } = frame {
                if let Some(send) = lock(&self.pongs).remove(&nonce) {
                    let _ = send.try_send(generation);
                }
                continue;
            }
            let attempt = match &frame {
                Up::Started { attempt }
                | Up::StartFailed { attempt, .. }
                | Up::Output { attempt, .. }
                | Up::OutputEof { attempt, .. }
                | Up::InputCredit { attempt }
                | Up::InputClosed { attempt }
                | Up::Exited { attempt, .. }
                | Up::Cleaned { attempt, .. }
                | Up::Done { attempt, .. } => *attempt,
                Up::Ready { .. } | Up::Pong { .. } => return,
            };
            let Some(shared) = lock(&self.attempts).get(&attempt).cloned() else {
                continue;
            };
            shared.update(|state| match frame {
                Up::Started { .. } => state.started = Some(Ok(())),
                Up::StartFailed { message, .. } => state.started = Some(Err(message)),
                Up::Output { stream, .. } => {
                    if let Some(queue) = state.output.get_mut(usize::from(stream) - 1) {
                        queue.push_back(payload);
                    }
                }
                Up::OutputEof { stream, .. } => {
                    if let Some(eof) = state.output_eof.get_mut(usize::from(stream) - 1) {
                        *eof = true;
                    }
                }
                Up::InputCredit { .. } => state.input_credit = (state.input_credit + 1).min(WINDOW),
                Up::InputClosed { .. } => state.input_closed = true,
                Up::Exited { code, .. } => state.exit = Some(code),
                Up::Cleaned { error, .. } => state.cleaned = Some(error.map_or(Ok(()), Err)),
                Up::Done { seq, error, .. } => {
                    state.done.insert(seq, error);
                }
                Up::Ready { .. } | Up::Pong { .. } => {}
            });
        }
    }

    /// Starts one attempt and waits for the supervisor's `Started`.
    ///
    /// # Errors
    /// Returns the supervisor's start failure, a timeout, or transport loss.
    /// On error the attempt has already been released (and cleaned) in-band.
    pub fn spawn(self: &Arc<Self>, spec: StartSpec) -> io::Result<Attachment> {
        let terminal = matches!(
            spec.kind,
            StartKind::Shell {
                terminal: Some(_),
                ..
            }
        );
        let attempt = self.next_attempt.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::new(AttemptShared {
            state: Mutex::new(AttemptState {
                input_credit: WINDOW,
                ..AttemptState::default()
            }),
            changed: Condvar::new(),
        });
        if !self.alive.load(Ordering::Acquire) {
            return Err(lost());
        }
        lock(&self.attempts).insert(attempt, Arc::clone(&shared));
        let contained = matches!(spec.kind, StartKind::Shell { .. });
        let handle = Arc::new(Handle {
            supervisor: Arc::clone(self),
            attempt,
            contained,
            shared,
        });
        handle
            .supervisor
            .send(&Down::Start { attempt, spec }, &[])?;
        let deadline = Instant::now() + START_TIMEOUT;
        // A plain child is usable at once: frames are ordered, so input sent
        // now follows its start, and a start failure surfaces on its streams.
        if contained {
            handle.await_started(deadline)?;
        }
        Ok(Attachment {
            stdin: Box::new(Input {
                handle: Arc::clone(&handle),
                terminal,
            }),
            stdout: Box::new(Output {
                handle: Arc::clone(&handle),
                stream: STDOUT,
                current: Vec::new(),
                offset: 0,
            }),
            stderr: Box::new(Output {
                handle: Arc::clone(&handle),
                stream: STDERR,
                current: Vec::new(),
                offset: 0,
            }),
            process: Box::new(Process {
                handle: Arc::clone(&handle),
            }),
            control: Arc::new(Control { handle }),
        })
    }
}

/// Shared by every part of one attempt's [`Attachment`]. The last drop
/// releases the attempt in-band.
struct Handle {
    supervisor: Arc<Supervisor>,
    attempt: u64,
    contained: bool,
    shared: Arc<AttemptShared>,
}

impl Handle {
    fn await_started(&self, deadline: Instant) -> io::Result<()> {
        let mut state = lock(&self.shared.state);
        loop {
            match &state.started {
                Some(Ok(())) => return Ok(()),
                Some(Err(message)) => {
                    return Err(io::Error::other(format!(
                        "shell supervisor start failed: {message}"
                    )));
                }
                None if state.lost => return Err(lost()),
                None => {}
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "shell supervisor start was not acknowledged",
                ));
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    fn control(&self, op: ControlOp, timeout: Duration) -> io::Result<()> {
        let seq = self.supervisor.next_seq.fetch_add(1, Ordering::Relaxed);
        self.supervisor.send(
            &Down::Control {
                attempt: self.attempt,
                seq,
                op,
            },
            &[],
        )?;
        let deadline = Instant::now() + timeout;
        let mut state = lock(&self.shared.state);
        loop {
            if let Some(result) = state.done.remove(&seq) {
                return result.map_or(Ok(()), |message| Err(io::Error::other(message)));
            }
            if state.lost {
                return Err(lost());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "shell supervisor control was not acknowledged",
                ));
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.supervisor.send(
            &Down::Release {
                attempt: self.attempt,
            },
            &[],
        );
        lock(&self.supervisor.attempts).remove(&self.attempt);
    }
}

struct Input {
    handle: Arc<Handle>,
    terminal: bool,
}

impl Write for Input {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let chunk = &bytes[..bytes.len().min(CHUNK)];
        {
            let mut state = lock(&self.handle.shared.state);
            loop {
                if state.lost {
                    return Err(lost());
                }
                if state.input_closed || state.cancelled || matches!(state.started, Some(Err(_))) {
                    return Err(io::Error::from(io::ErrorKind::BrokenPipe));
                }
                if state.input_credit > 0 {
                    state.input_credit -= 1;
                    break;
                }
                state = self
                    .handle
                    .shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
        self.handle.supervisor.send(
            &Down::Input {
                attempt: self.handle.attempt,
            },
            chunk,
        )?;
        Ok(chunk.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        // A PTY has no stdin EOF; ^D is a byte the client sends.
        if !self.terminal {
            let _ = self.handle.supervisor.send(
                &Down::InputEof {
                    attempt: self.handle.attempt,
                },
                &[],
            );
        }
    }
}

struct Output {
    handle: Arc<Handle>,
    stream: u8,
    current: Vec<u8>,
    offset: usize,
}

impl Read for Output {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.offset >= self.current.len() {
            let index = usize::from(self.stream) - 1;
            let mut state = lock(&self.handle.shared.state);
            loop {
                if state.cancelled {
                    return Ok(0);
                }
                if let Some(chunk) = state.output[index].pop_front() {
                    drop(state);
                    self.current = chunk;
                    self.offset = 0;
                    // Credit returns once the consumer took the chunk.
                    let _ = self.handle.supervisor.send(
                        &Down::OutputCredit {
                            attempt: self.handle.attempt,
                            stream: self.stream,
                        },
                        &[],
                    );
                    break;
                }
                if state.output_eof[index] {
                    return Ok(0);
                }
                if let Some(Err(message)) = &state.started {
                    return Err(io::Error::other(format!(
                        "supervisor start failed: {message}"
                    )));
                }
                if state.lost {
                    return Err(lost());
                }
                state = self
                    .handle
                    .shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
        let count = buffer.len().min(self.current.len() - self.offset);
        buffer[..count].copy_from_slice(&self.current[self.offset..self.offset + count]);
        self.offset += count;
        Ok(count)
    }
}

struct Process {
    handle: Arc<Handle>,
}

impl AttachedProcess for Process {
    fn wait(&mut self) -> io::Result<i32> {
        let mut state = lock(&self.handle.shared.state);
        loop {
            if let Some(code) = state.exit {
                return Ok(code);
            }
            if state.lost || matches!(state.started, Some(Err(_))) {
                return Err(lost());
            }
            state = self
                .handle
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        let state = lock(&self.handle.shared.state);
        match state.exit {
            Some(code) => Ok(Some(code)),
            None if state.lost => Err(lost()),
            None if matches!(state.started, Some(Err(_))) => {
                Err(io::Error::other("supervisor start failed"))
            }
            None => Ok(None),
        }
    }

    fn terminate(&mut self) -> io::Result<()> {
        if self.try_wait()?.is_some() {
            return Ok(());
        }
        self.handle
            .control(ControlOp::Signal("KILL".into()), CONTROL_TIMEOUT)
    }

    fn supports_io_cancellation(&self) -> bool {
        true
    }

    fn cancel_io(&self) -> io::Result<()> {
        self.handle.shared.update(|state| state.cancelled = true);
        Ok(())
    }
}

struct Control {
    handle: Arc<Handle>,
}

impl AttachmentControl for Control {
    fn signal(&self, signal: JobSignal) -> io::Result<()> {
        let name = match signal {
            JobSignal::Interrupt => "INT",
            JobSignal::Terminate => "TERM",
            JobSignal::Kill => "KILL",
            JobSignal::Hangup => "HUP",
        };
        self.handle
            .control(ControlOp::Signal(name.into()), CONTROL_TIMEOUT)
    }

    fn resize(&self, size: TerminalSize) -> io::Result<()> {
        self.handle.control(
            ControlOp::Resize {
                rows: size.rows,
                columns: size.columns,
            },
            CONTROL_TIMEOUT,
        )
    }

    fn cleanup_session(&self) -> io::Result<()> {
        // After the leader exits the supervisor cleans the cgroup on its own
        // and reports `Cleaned` (no extra round trip). Before that, ask.
        let deadline = Instant::now() + CLEANUP_TIMEOUT;
        let mut state = lock(&self.handle.shared.state);
        if !self.handle.contained || (state.exit.is_none() && state.cleaned.is_none()) {
            drop(state);
            return self.handle.control(ControlOp::Cleanup, CLEANUP_TIMEOUT);
        }
        loop {
            if let Some(result) = &state.cleaned {
                return result.clone().map_err(io::Error::other);
            }
            if state.lost {
                return Err(lost());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "shell supervisor cleanup was not reported",
                ));
            }
            state = self
                .handle
                .shared
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}
