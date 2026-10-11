//! One framed writer. Attempt cancellation can revoke queued frames, never an
//! already-started frame. No IO or callback runs with the queue mutex held.
use crate::output::InterruptibleWriter;
use crate::{MAX_CONTROL_FRAME, WorkerError, WorkerResponse};
use marsh_runtime::{CancellableFile, Cancellation};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::File,
    io::{self, Write},
    os::{fd::OwnedFd, unix::net::UnixStream},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

const ATTEMPT_FRAMES: usize = 8;
const ATTEMPT_BYTES: usize = 512 * 1024;
const CONTROL_FRAMES: usize = 128;
const CONTROL_BYTES: usize = 512 * 1024;
const CONTROL_BURST: usize = 4;
const POLL: Duration = Duration::from_millis(10);

/// A single cancellation domain constructed with genuinely interruptible IO.
/// The server, not its caller, wires reader failure and writer failure together.
pub struct WorkerTransport {
    pub(crate) input: CancellableFile,
    output: InterruptibleWriter,
    cancel: Cancellation,
    stall: Duration,
}
impl WorkerTransport {
    /// # Errors
    /// Rejects descriptors without an interruptible pipe/socket implementation.
    pub fn from_files(input: File, output: File) -> io::Result<Self> {
        let cancel = Cancellation::default();
        Ok(Self {
            input: CancellableFile::from_file(input, &cancel)?,
            output: InterruptibleWriter::from_file(output, cancel.clone())?,
            cancel,
            stall: Duration::from_secs(30),
        })
    }
    /// # Errors
    /// Returns fd duplication or transport construction errors.
    pub fn from_socket(socket: UnixStream) -> io::Result<Self> {
        let output = File::from(OwnedFd::from(socket.try_clone()?));
        Self::from_files(File::from(OwnedFd::from(socket)), output)
    }
    /// An explicit connection-level no-progress bound, not an attempt deadline.
    /// Each frame also has a finite four-times-stall total lifetime.
    #[must_use]
    pub fn with_stall_timeout(mut self, timeout: Duration) -> Self {
        self.stall = timeout.max(POLL);
        self
    }
    #[must_use]
    pub fn cancellation(&self) -> Cancellation {
        self.cancel.clone()
    }
    pub(crate) fn start(self) -> (CancellableFile, TransportWriter) {
        let Self {
            input,
            output,
            cancel,
            stall,
        } = self;
        (input, TransportWriter::new(output, cancel, stall))
    }
}

#[derive(Clone, Copy, Debug)]
enum PublicationFailure {
    Cancelled,
    Closed,
}
impl PublicationFailure {
    fn error(self) -> WorkerError {
        WorkerError::Io(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            match self {
                Self::Cancelled => "attempt publication cancelled",
                Self::Closed => "worker transport closed",
            },
        ))
    }
}
type Ack = mpsc::SyncSender<Result<(), PublicationFailure>>;
struct Frame {
    bytes: Vec<u8>,
    attempt: Option<u64>,
    cancel: Option<Cancellation>,
    ack: Option<Ack>,
    pong: bool,
    terminal: bool,
    on_start: Option<Box<dyn FnOnce() + Send>>,
}
struct AttemptQueue {
    frames: VecDeque<Frame>,
    bytes: usize,
}
#[derive(Default)]
struct State {
    attempts: BTreeMap<u64, AttemptQueue>,
    ready: VecDeque<u64>,
    priority: VecDeque<Frame>,
    priority_bytes: usize,
    next_attempt: u64,
    closed: bool,
    active_deadline: Option<Instant>,
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    cancel: Cancellation,
}
impl Shared {
    fn close(&self) {
        self.cancel.cancel();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        for frame in state.priority.drain(..) {
            acknowledge(frame, Err(PublicationFailure::Closed));
        }
        for (_, queue) in std::mem::take(&mut state.attempts) {
            for frame in queue.frames {
                acknowledge(frame, Err(PublicationFailure::Closed));
            }
        }
        state.priority_bytes = 0;
        state.ready.clear();
        self.changed.notify_all();
    }
}
fn acknowledge(mut frame: Frame, result: Result<(), PublicationFailure>) {
    if let Some(ack) = frame.ack.take() {
        let _ = ack.try_send(result);
    }
}
fn encode(response: &WorkerResponse) -> Result<Vec<u8>, WorkerError> {
    let body = serde_json::to_vec(response)?;
    if body.is_empty() || body.len() > MAX_CONTROL_FRAME {
        return Err(WorkerError::FrameTooLarge);
    }
    let mut bytes = Vec::with_capacity(body.len() + 4);
    bytes.extend_from_slice(
        &u32::try_from(body.len())
            .map_err(|_| WorkerError::FrameTooLarge)?
            .to_be_bytes(),
    );
    bytes.extend(body);
    Ok(bytes)
}

/// Only this owner has the fd. Drop wakes and joins both writer and watchdog.
pub(crate) struct TransportWriter {
    publisher: Publisher,
    writer: Option<thread::JoinHandle<()>>,
    watchdog: Option<thread::JoinHandle<()>>,
}
impl TransportWriter {
    fn new(output: InterruptibleWriter, cancel: Cancellation, stall: Duration) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            cancel,
        });
        let writing = shared.clone();
        let writer = thread::spawn(move || writer_loop(output, &writing, stall));
        let watching = shared.clone();
        let watchdog = thread::spawn(move || watch_stall(&watching));
        Self {
            publisher: Publisher {
                shared,
                attempt: None,
                cancel: None,
            },
            writer: Some(writer),
            watchdog: Some(watchdog),
        }
    }
    pub(crate) fn publisher(&self) -> Publisher {
        self.publisher.clone()
    }
}
impl Drop for TransportWriter {
    fn drop(&mut self) {
        self.publisher.close();
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
    }
}
#[derive(Clone)]
pub(crate) struct Publisher {
    shared: Arc<Shared>,
    attempt: Option<u64>,
    cancel: Option<Cancellation>,
}
impl Publisher {
    pub(crate) fn close(&self) {
        self.shared.close();
    }
    pub(crate) fn cancelled(&self) -> bool {
        self.shared.cancel.is_cancelled()
    }
    pub(crate) fn attempt(&self, cancel: Cancellation) -> Self {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.next_attempt = state
            .next_attempt
            .checked_add(1)
            .expect("attempt sequence exhausted");
        let id = state.next_attempt;
        state.attempts.insert(
            id,
            AttemptQueue {
                frames: VecDeque::new(),
                bytes: 0,
            },
        );
        Self {
            shared: self.shared.clone(),
            attempt: Some(id),
            cancel: Some(cancel),
        }
    }
    pub(crate) fn send(&self, response: &WorkerResponse) -> Result<(), WorkerError> {
        let priority = !matches!(
            response,
            WorkerResponse::Stdout { .. } | WorkerResponse::Stderr { .. }
        );
        self.publish(response, priority, None, true)
    }
    pub(crate) fn control(&self, response: &WorkerResponse) -> Result<(), WorkerError> {
        self.publish(response, true, None, false)
    }
    pub(crate) fn terminal(
        &self,
        response: &WorkerResponse,
        on_start: Box<dyn FnOnce() + Send>,
    ) -> Result<(), WorkerError> {
        // Terminal is not revocable by the attempt token. Earlier started data
        // must finish first, and queued cancelled data is discarded by writer.
        self.publish(response, true, Some(on_start), true)
    }
    fn publish(
        &self,
        response: &WorkerResponse,
        priority: bool,
        on_start: Option<Box<dyn FnOnce() + Send>>,
        wait: bool,
    ) -> Result<(), WorkerError> {
        // Serialize/validate BEFORE queueing and before any transport effect.
        let bytes = encode(response)?;
        let terminal = matches!(response, WorkerResponse::Terminal { .. });
        let cancel = if terminal { None } else { self.cancel.clone() };
        let pong = matches!(response, WorkerResponse::Pong { .. });
        let (ack, receive) = mpsc::sync_channel(1);
        let frame = Frame {
            bytes,
            attempt: self.attempt,
            cancel: cancel.clone(),
            ack: wait.then_some(ack),
            pong,
            terminal,
            on_start,
        };
        self.enqueue(frame, priority)?;
        if !wait {
            return Ok(());
        }
        loop {
            match receive.recv_timeout(POLL) {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(error)) => return Err(error.error()),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(PublicationFailure::Closed.error());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if cancel.as_ref().is_some_and(Cancellation::is_cancelled) {
                self.shared.changed.notify_all();
                return Err(PublicationFailure::Cancelled.error());
            }
            if self.shared.cancel.is_cancelled() {
                return Err(PublicationFailure::Closed.error());
            }
        }
    }
    fn enqueue(&self, frame: Frame, priority: bool) -> Result<(), WorkerError> {
        let budget = if priority || frame.attempt.is_none() {
            CONTROL_BYTES
        } else {
            ATTEMPT_BYTES
        };
        if frame.bytes.len() > budget {
            return Err(WorkerError::FrameTooLarge);
        }
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if state.closed || self.shared.cancel.is_cancelled() {
                return Err(PublicationFailure::Closed.error());
            }
            if frame
                .cancel
                .as_ref()
                .is_some_and(Cancellation::is_cancelled)
            {
                return Err(PublicationFailure::Cancelled.error());
            }
            let attempt = if priority { None } else { frame.attempt };
            if let Some(id) = attempt {
                let Some(queue) = state.attempts.get_mut(&id) else {
                    return Err(PublicationFailure::Closed.error());
                };
                if queue.frames.len() < ATTEMPT_FRAMES
                    && queue.bytes + frame.bytes.len() <= ATTEMPT_BYTES
                {
                    let was_empty = queue.frames.is_empty();
                    queue.bytes += frame.bytes.len();
                    queue.frames.push_back(frame);
                    if was_empty {
                        state.ready.push_back(id);
                    }
                    self.shared.changed.notify_all();
                    return Ok(());
                }
            } else {
                if frame.pong
                    && let Some(old) = state.priority.iter_mut().find(|old| old.pong)
                {
                    // At most one queued Pong. The started Pong is immutable;
                    // nonces let the host ignore it if it arrives late.
                    let old_bytes = old.bytes.len();
                    *old = frame;
                    let new_bytes = old.bytes.len();
                    state.priority_bytes = state.priority_bytes - old_bytes + new_bytes;
                    self.shared.changed.notify_all();
                    return Ok(());
                }
                if state.priority.len() < CONTROL_FRAMES
                    && state.priority_bytes + frame.bytes.len() <= CONTROL_BYTES
                {
                    state.priority_bytes += frame.bytes.len();
                    state.priority.push_back(frame);
                    self.shared.changed.notify_all();
                    return Ok(());
                }
                if frame.ack.is_none() {
                    return Err(WorkerError::ControlSpoolFull);
                }
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, POLL)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }
}
fn next_frame(state: &mut State, priority_run: &mut usize) -> Option<Frame> {
    loop {
        let priority =
            !state.priority.is_empty() && (*priority_run < CONTROL_BURST || state.ready.is_empty());
        let frame = if priority {
            *priority_run += 1;
            let frame = state.priority.pop_front()?;
            state.priority_bytes -= frame.bytes.len();
            frame
        } else {
            let id = state.ready.pop_front()?;
            *priority_run = 0;
            let Some(queue) = state.attempts.get_mut(&id) else {
                continue;
            };
            let Some(frame) = queue.frames.pop_front() else {
                continue;
            };
            queue.bytes -= frame.bytes.len();
            if !queue.frames.is_empty() {
                state.ready.push_back(id);
            }
            frame
        };
        if frame
            .cancel
            .as_ref()
            .is_some_and(Cancellation::is_cancelled)
        {
            acknowledge(frame, Err(PublicationFailure::Cancelled));
            continue;
        }
        if frame.terminal
            && let Some(id) = frame.attempt
        {
            // All producers were joined before Terminal. Only cancelled queued
            // frames may remain; no old-generation queue survives slot reuse.
            if let Some(queue) = state.attempts.remove(&id) {
                for queued in queue.frames {
                    acknowledge(queued, Err(PublicationFailure::Cancelled));
                }
            }
            state.ready.retain(|ready| *ready != id);
        }
        return Some(frame);
    }
}
fn writer_loop(mut output: InterruptibleWriter, shared: &Shared, stall: Duration) {
    let mut priority_run = 0;
    loop {
        let mut frame = {
            let mut state = shared
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            loop {
                if state.closed || shared.cancel.is_cancelled() {
                    drop(state);
                    shared.close();
                    return;
                }
                if let Some(frame) = next_frame(&mut state, &mut priority_run) {
                    state.active_deadline = Instant::now().checked_add(stall);
                    shared.changed.notify_all();
                    break frame;
                }
                state = shared
                    .changed
                    .wait_timeout(state, POLL)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
        };
        if let Some(on_start) = frame.on_start.take() {
            on_start();
        }
        let started = Instant::now();
        let absolute = started.checked_add(stall.saturating_mul(4));
        let result = (|| -> io::Result<()> {
            let mut sent = 0;
            while sent < frame.bytes.len() {
                // ONLY the transport token is checked once a frame starts.
                let count = output.scoped_write(&frame.bytes[sent..], &shared.cancel)?;
                if count == 0 {
                    return Err(io::ErrorKind::WriteZero.into());
                }
                sent += count;
                let mut state = shared
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.active_deadline = Instant::now()
                    .checked_add(stall)
                    .zip(absolute)
                    .map(|(idle, total)| idle.min(total));
                shared.changed.notify_all();
            }
            output.flush()
        })();
        shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_deadline = None;
        shared.changed.notify_all();
        if result.is_err() {
            acknowledge(frame, Err(PublicationFailure::Closed));
            shared.close();
            return;
        }
        acknowledge(frame, Ok(()));
    }
}
fn watch_stall(shared: &Shared) {
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if state.closed {
            return;
        }
        if state
            .active_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            drop(state);
            shared.close();
            return;
        }
        state = shared
            .changed
            .wait_timeout(state, POLL)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
}
