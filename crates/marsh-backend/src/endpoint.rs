//! The daemon's side of a job's filtered Docker endpoint and port forwards
//! (E2; SECURITY-DESIGN A2, A6, A11).
//!
//! A job of a project with services on gets `docker.sock` and port listeners
//! from its worker. Each connection arrives here as an endpoint channel over
//! the worker transport (`ChanOpen`, `CapData`, credit both ways) and is paired
//! with a channel on the project's services relay, whose authority comes from
//! the job record alone: `{project root, workspace base, attempt}`. The daemon
//! never parses HTTP. The ports the relay reports for the base workspace are
//! sent to the worker, which forwards exactly those in the job's own network
//! namespace.

use super::{StockDaemonBackend, services_reap};
use marsh_contracts::process::{
    ChannelKind, ENDPOINT_CHUNK, ENDPOINT_CREDIT_BATCH, ENDPOINT_WINDOW,
};
use marsh_daemon::relay::{ServiceStream, ShellServices};
use marsh_sbx::{
    WorkerControlHandle,
    services_relay::{AttemptScope, BASE_WORKSPACE, Stream},
};
use marsh_worker::{PortState, WorkerRequest, WorkerResponse};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, Scope},
    time::Duration,
};

/// Test breaks of the daemon's half (`docs/design/test-breaks.md`).
pub(crate) mod breaks {
    /// No forward is ever requested of the worker.
    pub const PORTS_OFF: &str = "ports-off";
    /// Each ports update reaches the worker three seconds late.
    pub const SLOW_PORTS: &str = "slow-ports";
    /// The shell gets no Docker endpoint and no `DOCKER_HOST`.
    pub const SHELL_NO_ENDPOINT: &str = "shell-no-endpoint";
    /// A later claimant takes over a shell VM port (D19's non-behavior).
    pub const SHELL_FORWARD_LAST_CLAIMANT: &str = "shell-forward-last-claimant";
}

/// Longest a job holds its project's services VM awake, in addition to its wall clock.
const HOLD_SLACK_SECONDS: u64 = 120;

/// Whose objects a job's endpoint speaks for (A4, C3): `{project root, lane
/// workspace, attempt}`, from the daemon's own state. The project's own jobs
/// are the base workspace with the project root; a job in a lane is that
/// lane's workspace with the lane's folder as the root its bind sources
/// resolve beneath, so it can name nothing of the base or of another lane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EndpointScope {
    /// The project's root: its relay, its VM hold, its forward records.
    pub root: PathBuf,
    /// The folder the authority's bind sources resolve beneath.
    pub authority_root: PathBuf,
    pub workspace: String,
}

impl EndpointScope {
    /// The project's own workspace.
    pub(crate) fn base(root: PathBuf) -> Self {
        Self {
            authority_root: root.clone(),
            root,
            workspace: BASE_WORKSPACE.to_owned(),
        }
    }
}

/// Credit for the daemon's chunks towards the worker.
struct Window {
    state: Mutex<(u32, bool)>,
    changed: Condvar,
}

impl Window {
    fn new(chunks: u32) -> Self {
        Self {
            state: Mutex::new((chunks, false)),
            changed: Condvar::new(),
        }
    }

    fn acquire(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.0 == 0 && !state.1 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if state.1 {
            return false;
        }
        state.0 -= 1;
        true
    }

    fn add(&self, chunks: u32) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.0 = state.0.saturating_add(chunks).min(ENDPOINT_WINDOW);
        self.changed.notify_all();
    }

    fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1 = true;
        self.changed.notify_all();
    }
}

enum Message {
    Data(Vec<u8>),
    /// The job half-closed its side.
    Eof,
    Abort,
}

struct Channel {
    send: mpsc::SyncSender<Message>,
    window: Arc<Window>,
    /// The relay channel, once open, so an abort can close it.
    stream: Arc<Mutex<Option<Stream>>>,
}

/// What a job's endpoint reports, to the job's own stderr as `marsh:` lines.
pub(crate) type Notice = Arc<dyn Fn(String) + Send + Sync>;

struct Shared {
    scope: EndpointScope,
    attempt: String,
    control: WorkerControlHandle,
    channels: Mutex<BTreeMap<u32, Channel>>,
    closed: AtomicBool,
    notice: Notice,
    /// The services VM holder this job holds while it has the endpoint.
    holder: Mutex<Option<String>>,
    /// How long a hold may last: the job's wall clock plus slack.
    hold_seconds: u64,
}

pub(crate) struct JobEndpoint<'scope, 'env> {
    scope: &'scope Scope<'scope, 'env>,
    backend: &'env StockDaemonBackend,
    shared: Arc<Shared>,
}

impl<'scope, 'env> JobEndpoint<'scope, 'env> {
    /// Starts the job's port watcher. `root` is the project's canonical root,
    /// from the daemon's own state.
    pub(crate) fn start(
        scope: &'scope Scope<'scope, 'env>,
        backend: &'env StockDaemonBackend,
        endpoint: EndpointScope,
        attempt: String,
        control: WorkerControlHandle,
        notice: Notice,
        wall_seconds: u64,
    ) -> Self {
        let shared = Arc::new(Shared {
            scope: endpoint,
            attempt,
            control,
            channels: Mutex::new(BTreeMap::new()),
            closed: AtomicBool::new(false),
            notice,
            holder: Mutex::new(None),
            hold_seconds: wall_seconds.saturating_add(HOLD_SLACK_SECONDS),
        });
        {
            let shared = Arc::clone(&shared);
            scope.spawn(move || watch_ports(backend, &shared));
        }
        Self {
            scope,
            backend,
            shared,
        }
    }

    /// Whether this endpoint serves `channel`.
    pub(crate) fn has(&self, channel: u32) -> bool {
        self.shared
            .channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&channel)
    }

    /// A job connected to its `docker.sock` or to a forwarded port.
    pub(crate) fn open(&self, channel: u32, kind: ChannelKind) {
        let (send, receive) = mpsc::sync_channel(ENDPOINT_WINDOW as usize + 16);
        let window = Arc::new(Window::new(ENDPOINT_WINDOW));
        let stream = Arc::new(Mutex::new(None));
        self.shared
            .channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                channel,
                Channel {
                    send,
                    window: Arc::clone(&window),
                    stream: Arc::clone(&stream),
                },
            );
        let (shared, backend, scope) = (Arc::clone(&self.shared), self.backend, self.scope);
        self.scope.spawn(move || {
            serve(
                scope, backend, &shared, channel, kind, &receive, &window, &stream,
            );
        });
    }

    /// The job's bytes for a channel. A job past its credit is cut off.
    pub(crate) fn data(&self, channel: u32, bytes: Vec<u8>) {
        let full = {
            let channels = self
                .shared
                .channels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            channels
                .get(&channel)
                .is_some_and(|open| open.send.try_send(Message::Data(bytes)).is_err())
        };
        if full {
            self.abort(channel);
        }
    }

    pub(crate) fn eof(&self, channel: u32) {
        if let Some(open) = self
            .shared
            .channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&channel)
        {
            let _ = open.send.try_send(Message::Eof);
        }
    }

    pub(crate) fn credit(&self, channel: u32, chunks: u32) {
        if let Some(open) = self
            .shared
            .channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&channel)
        {
            open.window.add(chunks);
        }
    }

    /// The worker closed the channel (or the daemon cuts it off).
    pub(crate) fn abort(&self, channel: u32) {
        abort(&self.shared, channel);
        let _ = self.shared.control.send(&WorkerRequest::CapClose {
            attempt: self.shared.attempt.clone(),
            channel,
        });
    }

    /// What became of one forward of this job.
    pub(crate) fn port_report(&self, port: u16, state: &PortState) {
        match state {
            PortState::Forwarding { .. } | PortState::Revoked => {}
            PortState::Collision { message } | PortState::Failed { message } => {
                (self.shared.notice)(format!("{message}\n"));
            }
        }
        self.backend.record_forward(
            &self.shared.scope.root,
            &self.shared.attempt,
            "job",
            port,
            state,
        );
    }
}

impl Drop for JobEndpoint<'_, '_> {
    fn drop(&mut self) {
        // Nothing of the job may outlive it: every channel closes, the
        // watcher ends, the services VM hold is released.
        self.shared.closed.store(true, Ordering::Release);
        let channels: Vec<u32> = self
            .shared
            .channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .copied()
            .collect();
        for channel in channels {
            abort(&self.shared, channel);
        }
        if let Some(holder) = self
            .shared
            .holder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            self.backend
                .services_release_job(&self.shared.scope.root, &holder);
        }
        self.backend
            .services_reap_attempt_ended(&self.shared.attempt);
        self.backend
            .forget_forwards(&self.shared.scope.root, &self.shared.attempt);
    }
}

fn abort(shared: &Shared, channel: u32) {
    let open = shared
        .channels
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&channel);
    if let Some(open) = open {
        open.window.close();
        let _ = open.send.try_send(Message::Abort);
        if let Some(stream) = open
            .stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            stream.close();
        }
    }
}

/// Holds the project's VM for the job and records the attempt, so its containers
/// are reaped at its end (E4). The record comes first: no channel may reach the
/// VM for an attempt that has none.
fn hold(backend: &StockDaemonBackend, shared: &Shared) -> Result<(), String> {
    let mut holder = shared
        .holder
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if holder.is_none() && !shared.closed.load(Ordering::Acquire) {
        backend.services_reap_track(&shared.scope.root, &shared.attempt)?;
        *holder = backend.services_hold_job(&shared.scope.root, shared.hold_seconds);
    }
    Ok(())
}

fn refuse(shared: &Shared, channel: u32, message: &str) {
    let _ = shared.control.send(&WorkerRequest::ChanRefused {
        attempt: shared.attempt.clone(),
        channel,
        message: message.to_owned(),
    });
    shared
        .channels
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&channel);
}

/// One endpoint channel: opens the relay channel the job's authority allows,
/// then moves bytes both ways under credit.
#[allow(clippy::too_many_arguments)] // The channel's whole state; each is a distinct input.
#[allow(clippy::too_many_lines)] // One channel: open, reader thread, writer loop, end.
fn serve<'scope, 'env>(
    scope: &'scope Scope<'scope, 'env>,
    backend: &'env StockDaemonBackend,
    shared: &Arc<Shared>,
    channel: u32,
    kind: ChannelKind,
    receive: &mpsc::Receiver<Message>,
    window: &Arc<Window>,
    slot: &Arc<Mutex<Option<Stream>>>,
) {
    let relay = match backend.services_relay(&shared.scope.root) {
        Ok(relay) => relay,
        Err(error) => return refuse(shared, channel, &format!("marsh: {error}")),
    };
    if let Err(error) = hold(backend, shared) {
        return refuse(shared, channel, &format!("marsh: {error}"));
    }
    let opened = AttemptScope::new(
        &shared.scope.authority_root,
        &shared.scope.workspace,
        &shared.attempt,
    )
    .and_then(|authority| match kind {
        ChannelKind::Docker => relay.open_docker(&authority),
        // Inside a lane the original port numbers work: the job's listener on
        // `port` reaches the fork's published port (A6).
        ChannelKind::Tcp { port } => {
            relay.open_tcp(&authority, backend.lane_port(&shared.scope.workspace, port))
        }
    });
    let stream = match opened {
        Ok(stream) => stream,
        Err(error) => return refuse(shared, channel, &format!("marsh: {error}")),
    };
    // Aborted while the relay channel was opening.
    if !shared
        .channels
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_key(&channel)
    {
        stream.close();
        return;
    }
    *slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(stream.clone());
    let reader = {
        let (mut from_relay, shared, window) =
            (stream.clone(), Arc::clone(shared), Arc::clone(window));
        scope.spawn(move || {
            let mut buffer = vec![0_u8; ENDPOINT_CHUNK];
            loop {
                if !window.acquire() {
                    return;
                }
                match from_relay.read(&mut buffer) {
                    Ok(0) => {
                        let _ = shared.control.send(&WorkerRequest::ChanEof {
                            attempt: shared.attempt.clone(),
                            channel,
                        });
                        return;
                    }
                    Ok(count) => {
                        if shared
                            .control
                            .send(&WorkerRequest::CapData {
                                attempt: shared.attempt.clone(),
                                channel,
                                bytes: buffer[..count].to_vec(),
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(_) => {
                        // The relay channel failed: the job's connection ends.
                        abort(&shared, channel);
                        let _ = shared.control.send(&WorkerRequest::CapClose {
                            attempt: shared.attempt.clone(),
                            channel,
                        });
                        return;
                    }
                }
            }
        })
    };
    let mut to_relay = stream.clone();
    let mut passed = 0_u32;
    let finished_cleanly = loop {
        match receive.recv() {
            Ok(Message::Data(bytes)) => {
                if to_relay.write_all(&bytes).is_err() {
                    break false;
                }
                passed += 1;
                if passed.is_multiple_of(ENDPOINT_CREDIT_BATCH) {
                    let _ = shared.control.send(&WorkerRequest::ChanCredit {
                        attempt: shared.attempt.clone(),
                        channel,
                        chunks: ENDPOINT_CREDIT_BATCH,
                    });
                }
            }
            Ok(Message::Eof) => {
                let _ = to_relay.shutdown_write();
                // The channel lives until the relay side is done too.
                let _ = reader.join();
                break true;
            }
            Ok(Message::Abort) | Err(_) => break false,
        }
    };
    if !finished_cleanly {
        window.close();
        stream.close();
        let _ = shared.control.send(&WorkerRequest::CapClose {
            attempt: shared.attempt.clone(),
            channel,
        });
    }
    shared
        .channels
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&channel);
}

/// Follows the project's relay and hands every change of `workspace`'s
/// published ports to `send` (which returns `false` when its peer is gone),
/// as the numbers the job listens on: in a lane, the original port numbers
/// the restore remapped (A6). Needs no request of its own: it follows the
/// project's relay whenever one exists, and never starts a services VM.
fn watch(
    backend: &StockDaemonBackend,
    root: &std::path::Path,
    workspace: &str,
    closed: &AtomicBool,
    acquired: &dyn Fn(),
    send: &dyn Fn(Vec<u16>) -> bool,
) {
    if marsh_contracts::break_mode(breaks::PORTS_OFF) {
        return;
    }
    let relays = backend.services_relays();
    while !closed.load(Ordering::Acquire) {
        let Some(relay) = relays
            .as_ref()
            .and_then(|relays| relays.get(root).ok())
            .map(|entry| entry.relay().clone())
        else {
            thread::sleep(Duration::from_millis(500));
            continue;
        };
        let Ok(mut subscription) = relay.ports(workspace) else {
            thread::sleep(Duration::from_millis(500));
            continue;
        };
        acquired();
        // The mapping the last update used: a lane's is known after its
        // restore, which may be after the first report.
        let mut mapped_at = backend.lane_generation(workspace);
        while !closed.load(Ordering::Acquire) {
            let changed = subscription.wait_change(Duration::from_millis(500));
            let generation = backend.lane_generation(workspace);
            if changed.is_none() {
                if relay.closed().is_some() {
                    break;
                }
                if generation == mapped_at {
                    continue;
                }
            }
            mapped_at = generation;
            let ports = changed.unwrap_or_else(|| subscription.current());
            if marsh_contracts::break_mode(breaks::SLOW_PORTS) {
                thread::sleep(Duration::from_secs(3));
            }
            if !send(backend.lane_listen_ports(workspace, &ports)) {
                return;
            }
        }
    }
}

fn watch_ports(backend: &StockDaemonBackend, shared: &Arc<Shared>) {
    watch(
        backend,
        &shared.scope.root,
        &shared.scope.workspace,
        &shared.closed,
        // A refused record only keeps the watcher from holding the VM: every
        // channel that could create a container holds it again, and is refused.
        &|| {
            let _ = hold(backend, shared);
        },
        &|ports| {
            shared
                .control
                .send(&WorkerRequest::Ports {
                    attempt: shared.attempt.clone(),
                    ports,
                })
                .is_ok()
        },
    );
}

/// The attachment-side reaction to every endpoint frame, shared by the job loop.
pub(crate) fn handle_frame(endpoint: &JobEndpoint<'_, '_>, response: &WorkerResponse) -> bool {
    match response {
        WorkerResponse::ChanOpen { channel, kind, .. } => endpoint.open(*channel, *kind),
        WorkerResponse::ChanEof { channel, .. } => endpoint.eof(*channel),
        WorkerResponse::ChanCredit {
            channel, chunks, ..
        } => endpoint.credit(*channel, *chunks),
        WorkerResponse::PortReport { port, state, .. } => endpoint.port_report(*port, state),
        WorkerResponse::CapData { channel, bytes, .. } if endpoint.has(*channel) => {
            endpoint.data(*channel, bytes.clone());
        }
        WorkerResponse::CapClose { channel, .. } if endpoint.has(*channel) => {
            abort(&endpoint.shared, *channel);
        }
        _ => return false,
    }
    true
}

/// The authority attempt every shell of a project uses (DECISIONS 10).
const SHELL_ATTEMPT: &str = "shell";

enum Request {
    Open {
        kind: ChannelKind,
        reply: mpsc::SyncSender<Result<ServiceStream, String>>,
    },
    Report {
        port: u16,
        state: PortState,
    },
}

/// What the shell relay's host thread holds (it is `'static`): it only sends
/// requests to the shell run's own scoped threads, which have the daemon.
pub(crate) struct ShellProvider {
    requests: Mutex<mpsc::Sender<Request>>,
    notice: Notice,
    breaks: Vec<String>,
}

impl ShellServices for ShellProvider {
    fn open(&self, service: ChannelKind) -> Result<ServiceStream, String> {
        let (reply, receive) = mpsc::sync_channel(1);
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .send(Request::Open {
                kind: service,
                reply,
            })
            .map_err(|_| "marsh: this shell's services endpoint has ended".to_owned())?;
        receive
            .recv_timeout(Duration::from_secs(180))
            .map_err(|_| "marsh: the project's services did not open in time".to_owned())?
    }

    fn port_report(&self, port: u16, state: &PortState) {
        match state {
            PortState::Forwarding { .. } | PortState::Revoked => {}
            // The shell's notice adds its own line ending (and tells a message
            // before the shell is ready from one after).
            PortState::Collision { message } | PortState::Failed { message } => {
                (self.notice)(message.clone());
            }
        }
        let _ = self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .send(Request::Report {
                port,
                state: state.clone(),
            });
    }

    fn guest_breaks(&self) -> Vec<String> {
        self.breaks.clone()
    }
}

/// A shell's services endpoint on the daemon (E2, DECISIONS 10 and 19): the
/// shell relay asks for channels with `{project root, base, "shell"}`. The
/// opens run on scoped threads of the shell's own run, so the shell relay
/// never waits on a services VM start.
pub(crate) struct ShellEndpoint<'scope, 'env> {
    scope: &'scope Scope<'scope, 'env>,
    backend: &'env StockDaemonBackend,
    root: PathBuf,
    done: Arc<AtomicBool>,
    holder: Arc<Mutex<Option<String>>>,
}

/// Ends the endpoint with the shell: the opener stops, the services VM hold
/// is released, the forward records go.
pub(crate) struct ShellEndpointGuard<'a> {
    backend: &'a StockDaemonBackend,
    root: PathBuf,
    session: String,
    done: Arc<AtomicBool>,
    holder: Arc<Mutex<Option<String>>>,
}

impl Drop for ShellEndpointGuard<'_> {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        if let Some(holder) = self
            .holder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            self.backend.services_release_job(&self.root, &holder);
        }
        // A shell's objects persist (DECISIONS 10 and 11): only the test break
        // reaps them when the shell exits.
        if marsh_contracts::break_mode(services_reap::breaks::SHELL_REAP_ON_EXIT)
            && let Err(error) = self.backend.services_reap_now(&self.root, SHELL_ATTEMPT)
        {
            eprintln!("marshd: reaping the shell's services containers: {error}");
        }
        self.backend.forget_forwards(&self.root, &self.session);
    }
}

impl<'scope, 'env> ShellEndpoint<'scope, 'env> {
    /// Starts the opener thread; returns the endpoint, the provider for the
    /// shell relay and the guard that ends it.
    pub(crate) fn start(
        scope: &'scope Scope<'scope, 'env>,
        backend: &'env StockDaemonBackend,
        root: PathBuf,
        session: String,
        notice: Notice,
    ) -> (Arc<Self>, Arc<ShellProvider>, ShellEndpointGuard<'env>) {
        let done = Arc::new(AtomicBool::new(false));
        let holder = Arc::new(Mutex::new(None));
        let (sender, receiver) = mpsc::channel::<Request>();
        let endpoint = Arc::new(Self {
            scope,
            backend,
            root: root.clone(),
            done: Arc::clone(&done),
            holder: Arc::clone(&holder),
        });
        let provider = Arc::new(ShellProvider {
            requests: Mutex::new(sender),
            notice,
            breaks: marsh_contracts::test_breaks::selected(&[breaks::SHELL_FORWARD_LAST_CLAIMANT]),
        });
        {
            let (endpoint, session) = (Arc::clone(&endpoint), session.clone());
            scope.spawn(move || {
                while !endpoint.done.load(Ordering::Acquire) {
                    match receiver.recv_timeout(Duration::from_millis(500)) {
                        Ok(Request::Open { kind, reply }) => {
                            let endpoint = Arc::clone(&endpoint);
                            endpoint.scope.spawn(move || {
                                let _ = reply.send(endpoint.open_now(kind));
                            });
                        }
                        Ok(Request::Report { port, state }) => {
                            endpoint.backend.record_forward(
                                &endpoint.root,
                                &session,
                                "shell",
                                port,
                                &state,
                            );
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
            });
        }
        let guard = ShellEndpointGuard {
            backend,
            root,
            session,
            done,
            holder,
        };
        (endpoint, provider, guard)
    }

    /// Sends the guest relay the ports the project publishes, as they change;
    /// it listens for them in the shell VM.
    pub(crate) fn follow_ports<W: std::io::Write + Send + 'static>(
        self: &Arc<Self>,
        tunnel: Arc<Mutex<W>>,
    ) {
        let endpoint = Arc::clone(self);
        self.scope.spawn(move || {
            watch(
                endpoint.backend,
                &endpoint.root,
                BASE_WORKSPACE,
                &endpoint.done,
                &|| endpoint.hold(),
                &|ports| marsh_daemon::relay::send_ports(&tunnel, &ports).is_ok(),
            );
        });
    }

    fn hold(&self) {
        let mut holder = self
            .holder
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if holder.is_none() && !self.done.load(Ordering::Acquire) {
            *holder = self.backend.services_hold_job(&self.root, 24 * 60 * 60);
        }
    }

    fn open_now(&self, kind: ChannelKind) -> Result<ServiceStream, String> {
        let relay = self
            .backend
            .services_relay(&self.root)
            .map_err(|error| format!("marsh: {error}"))?;
        self.hold();
        let authority = AttemptScope::new(&self.root, BASE_WORKSPACE, SHELL_ATTEMPT)
            .map_err(|error| format!("marsh: {error}"))?;
        let stream = match kind {
            ChannelKind::Docker => relay.open_docker(&authority),
            ChannelKind::Tcp { port } => relay.open_tcp(&authority, port),
        }
        .map_err(|error| format!("marsh: {error}"))?;
        let (finish, close) = (stream.clone(), stream.clone());
        Ok(ServiceStream {
            reader: Box::new(stream.clone()),
            writer: Box::new(stream),
            finish: Box::new(move || {
                let _ = finish.shutdown_write();
            }),
            close: Box::new(move || close.close()),
        })
    }
}
