//! Transparent guest-shell transport over one long-lived stock-SBX exec.
//!
//! The guest side owns an owner-only Unix socket and token under `/run`. Each
//! guest connection is multiplexed over the trusted relay process's stdio;
//! the host side opens a matching connection to the macOS daemon. Job
//! containers receive neither path nor token.

use crate::relay_cleanup::{RelayIdentity, current_identity};
use crate::{DaemonError, ErrorCode, PublicReply, read_frame, write_frame};
use marsh_contracts::process::{ChannelKind, PortState};
use serde::{Deserialize, Serialize};
use std::net::Shutdown;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::{
    collections::BTreeMap,
    fs::{self, DirBuilder, OpenOptions},
    io::{Read, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

mod forward;

const CHUNK_BYTES: usize = 16 * 1024;
// One shell's daemon connections at once; every job it runs holds one. Twice
// the default job pool (`daemon-jobs`), so a shell can fill the pool with
// room for its other requests. Each connection retains three local socket
// descriptors; the daemon raises its descriptor limit to fit them.
const MAX_RELAY_CONNECTIONS: usize = 2 * marsh_contracts::process::POOL_LIMIT;
// At most 1 MiB per connection, including an in-progress write. A stalled
// recipient must not hold the tunnel reader or the shared connection map.
const MAX_PENDING_CHUNKS: usize = 64;
// macOS can leave a thread blocked in `read` on a Unix socket after a
// `shutdown` of either end (a lost wakeup; data and a full close always wake
// it). Readers therefore wake this often to re-check, instead of relying on
// the peer's half-close or the relay's own abort to interrupt them.
const READ_WAKE: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum TunnelFrame {
    Initialize {
        token: String,
        /// The daemon's test break modes for the guest relay (an attested
        /// field, DECISIONS 22): always empty in a release daemon.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        breaks: Vec<String>,
    },
    Identity {
        identity: RelayIdentity,
    },
    Ready,
    Open {
        connection: u64,
    },
    /// A guest connection to the project's services endpoint (E2): the
    /// shell's `docker.sock` or a forwarded port. The host pairs it with a
    /// channel on the services relay instead of the daemon socket.
    OpenService {
        connection: u64,
        service: ChannelKind,
    },
    /// The ports the project's base workspace publishes now (host to guest).
    Ports {
        ports: Vec<u16>,
    },
    /// What became of one forward in the shell VM (guest to host).
    PortReport {
        port: u16,
        state: PortState,
    },
    Data {
        connection: u64,
        bytes: Vec<u8>,
    },
    Credit {
        connection: u64,
    },
    // Graceful half-close: deliver preceding data, then close only writes.
    Eof {
        connection: u64,
    },
    // Graceful full close: deliver preceding data, then close both directions.
    Finished {
        connection: u64,
    },
    // Abort: interrupt outstanding writes and discard queued data immediately.
    Close {
        connection: u64,
    },
    Shutdown,
    // Guest's final frame: listener closed, socket and token removed.
    Cleaned,
}

type SharedWriter<W> = Arc<Mutex<W>>;
type Connections = Arc<Mutex<BTreeMap<u64, Connection>>>;

struct Connection {
    /// A services endpoint connection (E2), counted apart from daemon requests.
    service: bool,
    stream: UnixStream,
    writer: mpsc::SyncSender<ConnectionWrite>,
    pending_chunks: Arc<AtomicUsize>,
    send_window: Arc<SendWindow>,
    identity: Arc<()>,
    read_closed: bool,
    /// A write to the peer failed: it reads no more.
    write_failed: bool,
    end: ConnectionEnd,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ConnectionEnd {
    Open,
    EofQueued,
    EofDelivered,
    FinishQueued,
}

enum ConnectionWrite {
    Data(Vec<u8>),
    Eof,
    Finish,
}

struct ConnectionReader {
    stream: UnixStream,
    identity: Arc<()>,
    send_window: Arc<SendWindow>,
}

/// The peer returns one credit only after delivering a complete data chunk.
/// A slow client therefore backpressures its own source reader without
/// blocking the tunnel reader or consuming unbounded queue memory.
struct SendWindow {
    state: Mutex<(usize, bool)>,
    changed: Condvar,
}

impl SendWindow {
    fn new() -> Self {
        Self {
            state: Mutex::new((MAX_PENDING_CHUNKS, false)),
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

    fn release(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.0 = state.0.saturating_add(1).min(MAX_PENDING_CHUNKS);
        self.changed.notify_one();
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.1 = true;
        self.changed.notify_all();
    }

    fn closed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1
    }
}

/// Host half of the relay. `from_guest`/`to_guest` are the stdout/stdin of one
/// retained `sbx exec -i ... marsh-relay` process.
pub fn run_host<R, W>(
    from_guest: R,
    to_guest: W,
    daemon_socket: &Path,
    session_token: String,
) -> Result<(), DaemonError>
where
    R: Read,
    W: Write + Send + 'static,
{
    run_host_with_ready(from_guest, to_guest, daemon_socket, session_token, |_| {})
}

/// Host relay with an explicit readiness notification. Backends must wait for
/// `Ok(())` before starting the guest shell that consumes the relay socket.
pub fn run_host_with_ready<R, W, F>(
    from_guest: R,
    to_guest: W,
    daemon_socket: &Path,
    session_token: String,
    ready: F,
) -> Result<(), DaemonError>
where
    R: Read,
    W: Write + Send + 'static,
    F: FnOnce(Result<(), String>),
{
    run_host_with_identity(
        from_guest,
        to_guest,
        daemon_socket,
        session_token,
        |result| ready(result.map(|_| ())),
    )
}

/// Retain independently verifiable guest process identity from trusted startup.
pub fn run_host_with_identity<R, W, F>(
    from_guest: R,
    to_guest: W,
    daemon_socket: &Path,
    session_token: String,
    ready: F,
) -> Result<(), DaemonError>
where
    R: Read,
    W: Write + Send + 'static,
    F: FnOnce(Result<Option<RelayIdentity>, String>),
{
    run_host_shared(
        from_guest,
        &Arc::new(Mutex::new(to_guest)),
        daemon_socket,
        session_token,
        ready,
        &AtomicBool::new(false),
    )
}

/// Tells the guest relay which ports the project publishes now (the whole
/// set); it forwards exactly those in the shell VM.
///
/// # Errors
/// Returns an error when the tunnel cannot be written.
pub fn send_ports<W: Write>(tunnel: &Arc<Mutex<W>>, ports: &[u16]) -> Result<(), DaemonError> {
    send(
        tunnel,
        &TunnelFrame::Ports {
            ports: ports.to_vec(),
        },
    )
}

/// Asks the guest relay to stop. It closes its listener, removes its socket
/// and token, and reports `Cleaned` as its final frame (see [`run_host_shared`]).
///
/// # Errors
/// Returns an error when the tunnel cannot be written.
pub fn request_guest_shutdown<W: Write>(tunnel: &Arc<Mutex<W>>) -> Result<(), DaemonError> {
    send(tunnel, &TunnelFrame::Shutdown)
}

/// Host relay over a caller-shared tunnel writer. `cleaned` is set when the
/// guest reports in-band that its socket and token are gone.
///
/// # Errors
/// Returns relay protocol or I/O failures.
pub fn run_host_shared<R, W, F>(
    from_guest: R,
    tunnel: &Arc<Mutex<W>>,
    daemon_socket: &Path,
    session_token: String,
    ready: F,
    cleaned: &AtomicBool,
) -> Result<(), DaemonError>
where
    R: Read,
    W: Write + Send + 'static,
    F: FnOnce(Result<Option<RelayIdentity>, String>),
{
    run_host_shared_with_services(
        from_guest,
        tunnel,
        daemon_socket,
        session_token,
        ready,
        cleaned,
        None,
    )
}

/// What the host relay needs to serve a shell's services endpoint (E2): the
/// project's channels on the services relay, and where forwards report to.
pub trait ShellServices: Send + Sync {
    /// Opens a channel to the project's filtered Docker endpoint or to one of
    /// its published ports, with the shell's authority (the project, the base
    /// workspace, attempt `shell`). The error is plain text for the user.
    ///
    /// # Errors
    /// The refusal or unavailability, as text starting with `marsh:`.
    fn open(&self, service: ChannelKind) -> Result<ServiceStream, String>;
    /// What became of a forward the shell VM tried to make.
    fn port_report(&self, port: u16, state: &PortState);
    /// The test break modes the guest relay takes (attested, DECISIONS 22).
    fn guest_breaks(&self) -> Vec<String> {
        Vec::new()
    }
}

/// One open services channel as plain byte halves.
pub struct ServiceStream {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    /// Half close: no more bytes will be written.
    pub finish: Box<dyn Fn() + Send + Sync>,
    /// Ends the channel at once.
    pub close: Box<dyn Fn() + Send + Sync>,
}

/// At most this many services connections per shell (a descriptor pair each on the Mac).
const MAX_SERVICE_CONNECTIONS: usize = 32;

/// [`run_host_shared`] with the shell's services endpoint.
///
/// # Errors
/// Returns relay protocol or I/O failures.
#[allow(clippy::too_many_lines)] // One frame loop.
pub fn run_host_shared_with_services<R, W, F>(
    mut from_guest: R,
    tunnel: &Arc<Mutex<W>>,
    daemon_socket: &Path,
    session_token: String,
    ready: F,
    cleaned: &AtomicBool,
    services: Option<&Arc<dyn ShellServices>>,
) -> Result<(), DaemonError>
where
    R: Read,
    W: Write + Send + 'static,
    F: FnOnce(Result<Option<RelayIdentity>, String>),
{
    let tunnel = Arc::clone(tunnel);
    if let Err(error) = send(
        &tunnel,
        &TunnelFrame::Initialize {
            token: session_token,
            breaks: services.map_or_default(|services| services.guest_breaks()),
        },
    ) {
        ready(Err(error.to_string()));
        return Err(error);
    }
    let first = read_frame::<TunnelFrame>(&mut from_guest);
    let (identity, frame) = match first {
        Ok(TunnelFrame::Identity { identity }) => {
            (Some(identity), read_frame::<TunnelFrame>(&mut from_guest))
        }
        frame => (None, frame),
    };
    match frame {
        Ok(TunnelFrame::Ready) => ready(Ok(identity)),
        Ok(_) => {
            let error =
                DaemonError::InvalidState("guest relay did not acknowledge initialization".into());
            ready(Err(error.to_string()));
            return Err(error);
        }
        Err(error) => {
            ready(Err(error.to_string()));
            return Err(error);
        }
    }
    let connections: Connections = Arc::default();
    let mut refusals = RefusalLog::default();
    let outcome = loop {
        match read_frame::<TunnelFrame>(&mut from_guest) {
            Ok(TunnelFrame::Open { connection }) => {
                let open = daemon_connections(&connections);
                if open >= MAX_RELAY_CONNECTIONS {
                    if let Some(line) = refusals.note(
                        Instant::now(),
                        "marshd: shell relay",
                        &format!("{open}/{MAX_RELAY_CONNECTIONS} daemon connections open"),
                    ) {
                        log_line(&line);
                    }
                    if let Err(error) = refuse_open(&tunnel, connection, &busy_message()) {
                        break Err(error);
                    }
                    continue;
                }
                let Ok(stream) = marsh_runtime::connect_unix(daemon_socket) else {
                    if let Some(line) = refusals.note(
                        Instant::now(),
                        "marshd: shell relay",
                        "the daemon socket did not accept a connection",
                    ) {
                        log_line(&line);
                    }
                    if let Err(error) = refuse_open(&tunnel, connection, UNREACHABLE_MESSAGE) {
                        break Err(error);
                    }
                    continue;
                };
                let reader = match register_connection(connection, stream, &connections, &tunnel) {
                    Ok(reader) => reader,
                    Err(error) => break Err(error),
                };
                spawn_reader(
                    connection,
                    reader,
                    Arc::clone(&connections),
                    Arc::clone(&tunnel),
                );
            }
            Ok(TunnelFrame::OpenService {
                connection,
                service,
            }) => {
                let busy = connections
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .values()
                    .filter(|open| open.service)
                    .count()
                    >= MAX_SERVICE_CONNECTIONS;
                let (Some(services), false) = (services, busy) else {
                    if let Err(error) = send(&tunnel, &TunnelFrame::Close { connection }) {
                        break Err(error);
                    }
                    continue;
                };
                let Ok((ours, theirs)) = UnixStream::pair() else {
                    if let Err(error) = send(&tunnel, &TunnelFrame::Close { connection }) {
                        break Err(error);
                    }
                    continue;
                };
                // Registered before the next frame is read, so the guest's first
                // bytes find their connection; the channel opens meanwhile.
                let reader =
                    match register_connection_as(connection, ours, &connections, &tunnel, true) {
                        Ok(reader) => reader,
                        Err(error) => break Err(error),
                    };
                spawn_reader(
                    connection,
                    reader,
                    Arc::clone(&connections),
                    Arc::clone(&tunnel),
                );
                let services = Arc::clone(services);
                thread::spawn(move || serve_service(&*services, service, theirs));
            }
            Ok(TunnelFrame::PortReport { port, state }) => {
                if let Some(services) = services {
                    services.port_report(port, &state);
                }
            }
            Ok(TunnelFrame::Data { connection, bytes }) => {
                if let Err(error) = forward_data(&connections, &tunnel, connection, bytes) {
                    break Err(error);
                }
            }
            Ok(TunnelFrame::Credit { connection }) => {
                return_credit(&connections, connection);
            }
            Ok(
                frame @ (TunnelFrame::Eof { connection } | TunnelFrame::Finished { connection }),
            ) => {
                end_after_queued_data(
                    &connections,
                    connection,
                    matches!(frame, TunnelFrame::Finished { .. }),
                );
            }
            Ok(TunnelFrame::Close { connection }) => {
                abort_connection(&connections, connection);
            }
            // The guest acknowledges Shutdown, then reports Cleaned or EOF.
            Ok(TunnelFrame::Shutdown) => {}
            Ok(TunnelFrame::Cleaned) => {
                cleaned.store(true, Ordering::Release);
                break Ok(());
            }
            Ok(_) => {
                break Err(DaemonError::InvalidState(
                    "unexpected guest relay frame".into(),
                ));
            }
            Err(DaemonError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                break Ok(());
            }
            Err(error) => break Err(error),
        }
    };
    close_all_connections(&connections);
    outcome
}

fn send_guest_ready<W: Write>(tunnel: &SharedWriter<W>) -> Result<(), DaemonError> {
    if let Some(identity) = current_identity()? {
        send(tunnel, &TunnelFrame::Identity { identity })?;
    }
    send(tunnel, &TunnelFrame::Ready)
}

fn busy_message() -> String {
    format!(
        "marsh: this shell holds {MAX_RELAY_CONNECTIONS} daemon{}",
        marsh_contracts::process::CONNECTIONS_FULL_SUFFIX
    )
}

const UNREACHABLE_MESSAGE: &str = marsh_contracts::process::RELAY_UNREACHABLE;

/// One diagnostic line on this process's stderr. Never panics: a closed
/// stderr (the host dropped the pipe) must not take a relay thread with it.
pub fn log_line(line: &str) {
    let _ = writeln!(std::io::stderr(), "{line}");
}

fn refusal(message: &str) -> PublicReply {
    PublicReply::Error {
        code: ErrorCode::InvalidRequest,
        message: message.to_owned(),
    }
}

/// Past the guest relay's cap: tell the caller why rather than close on it. A
/// bare close reaches a caller that has already written its request as a
/// reset (`ECONNRESET` on Linux). Best effort, and never blocking the accept
/// loop.
fn refuse_guest_connection(mut stream: UnixStream) {
    if stream.set_nonblocking(true).is_ok() {
        let _ = write_frame(&mut stream, &refusal(&busy_message()));
    }
    let _ = stream.shutdown(Shutdown::Write);
}

/// The host half's refusal of a connection the guest has already accepted:
/// the reason as the connection's only data, then its ordered end, so the
/// guest delivers the frame before it closes.
fn refuse_open<W: Write>(
    tunnel: &SharedWriter<W>,
    connection: u64,
    message: &str,
) -> Result<(), DaemonError> {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, &refusal(message))?;
    send(tunnel, &TunnelFrame::Data { connection, bytes })?;
    send(tunnel, &TunnelFrame::Finished { connection })
}

/// What the guest relay writes to its stderr that the host passes on to the
/// daemon's log: only its refusal lines (`RefusalLog`), whole, as plain
/// one-line text (`sanitize_vm_text`) cut to [`Self::MAX_LINE`] characters,
/// and at most [`Self::LINES_PER_WINDOW`] per [`REFUSAL_LOG_INTERVAL`]: the
/// guest rate-limits its own, but what a guest process writes is not trusted.
/// Anything else the guest says stays in the bounded head kept for a relay
/// that fails to start. A prefix is not authentication: a process that can
/// write that stream can write a false line.
#[derive(Default)]
pub struct GuestDiagnostics {
    partial: Vec<u8>,
    window: Option<Instant>,
    in_window: usize,
    left_out: u64,
}

impl GuestDiagnostics {
    const LINE_PREFIX: &'static [u8] = b"marsh-relay: refused a connection (";
    const MAX_LINE: usize = 300;
    const LINES_PER_WINDOW: usize = 3;

    /// The lines completed by `bytes` (read at `now`) that go to the daemon's
    /// log.
    pub fn feed(&mut self, now: Instant, bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for &byte in bytes {
            if byte == b'\n' {
                let line = std::mem::take(&mut self.partial);
                if line.starts_with(Self::LINE_PREFIX) {
                    lines.extend(self.admit(now, &line));
                }
            } else if self.partial.len() < Self::MAX_LINE * 2 {
                self.partial.push(byte);
            }
        }
        lines
    }

    fn admit(&mut self, now: Instant, line: &[u8]) -> Option<String> {
        if self
            .window
            .is_none_or(|start| now.saturating_duration_since(start) >= REFUSAL_LOG_INTERVAL)
        {
            self.window = Some(now);
            self.in_window = 0;
        }
        if self.in_window >= Self::LINES_PER_WINDOW {
            self.left_out += 1;
            return None;
        }
        self.in_window += 1;
        let mut text: String =
            marsh_contracts::terminal::sanitize_vm_text(&String::from_utf8_lossy(line))
                .take(Self::MAX_LINE)
                .collect();
        let left_out = std::mem::take(&mut self.left_out);
        if left_out > 0 {
            text = format!("{text} [{left_out} earlier refusal lines left out]");
        }
        Some(text)
    }
}

/// At most one diagnostic line per this long for refused connections.
const REFUSAL_LOG_INTERVAL: Duration = Duration::from_secs(10);

/// What a relay half says on its stderr when it refuses connections: one line,
/// then one more at most every [`REFUSAL_LOG_INTERVAL`], counting those it
/// left unsaid. Counts and the cap only; nothing a caller sent.
#[derive(Default)]
struct RefusalLog {
    last: Option<Instant>,
    unsaid: u64,
}

impl RefusalLog {
    fn note(&mut self, now: Instant, who: &str, detail: &str) -> Option<String> {
        if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < REFUSAL_LOG_INTERVAL)
        {
            self.unsaid += 1;
            return None;
        }
        self.last = Some(now);
        let unsaid = std::mem::take(&mut self.unsaid);
        Some(if unsaid == 0 {
            format!("{who}: refused a connection ({detail})")
        } else {
            format!(
                "{who}: refused a connection ({detail}); {unsaid} more refused since the last line"
            )
        })
    }
}

/// Guest half used by the installed `marsh-relay` helper.
pub fn run_guest<R, W>(
    from_host: R,
    to_host: W,
    socket_path: &Path,
    token_path: &Path,
) -> Result<(), DaemonError>
where
    R: Read,
    W: Write + Send + 'static,
{
    run_guest_with_endpoint(from_host, to_host, socket_path, token_path, None)
}

/// [`run_guest`], also serving the shell's services endpoint (E2): a second
/// owner-only socket at `endpoint` (the shell's `DOCKER_HOST`), and loopback
/// listeners for the ports the host reports, in the shell VM, first claimant
/// wins.
pub fn run_guest_with_endpoint<R, W>(
    from_host: R,
    to_host: W,
    socket_path: &Path,
    token_path: &Path,
    endpoint: Option<&Path>,
) -> Result<(), DaemonError>
where
    R: Read,
    W: Write + Send + 'static,
{
    run_guest_logged(
        from_host,
        to_host,
        socket_path,
        token_path,
        endpoint,
        Arc::new(log_line),
    )
}

/// [`run_guest_with_endpoint`], saying what it refuses through `log`.
#[allow(clippy::too_many_lines)] // One frame loop and its acceptors.
fn run_guest_logged<R, W>(
    mut from_host: R,
    to_host: W,
    socket_path: &Path,
    token_path: &Path,
    endpoint: Option<&Path>,
    log: Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<(), DaemonError>
where
    R: Read,
    W: Write + Send + 'static,
{
    let TunnelFrame::Initialize { token, breaks } = read_frame(&mut from_host)? else {
        return Err(DaemonError::InvalidState(
            "host relay omitted initialization".into(),
        ));
    };
    marsh_contracts::test_breaks::attest_guest(&breaks);
    prepare_guest_runtime(socket_path, token_path, &token)?;
    let runtime = GuestRuntime::new(socket_path, token_path, endpoint)?;
    let listener = UnixListener::bind(socket_path)?;
    fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let docker = match endpoint {
        Some(path) => {
            let docker = UnixListener::bind(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            docker.set_nonblocking(true)?;
            Some(docker)
        }
        None => None,
    };
    let tunnel = Arc::new(Mutex::new(to_host));
    send_guest_ready(&tunnel)?;
    let connections: Connections = Arc::default();
    let next = Arc::new(std::sync::atomic::AtomicU64::new(1));
    let stopping = Arc::new(AtomicBool::new(false));
    let acceptor = {
        let (connections, tunnel, stopping, next, log) = (
            Arc::clone(&connections),
            Arc::clone(&tunnel),
            Arc::clone(&stopping),
            Arc::clone(&next),
            log,
        );
        thread::spawn(move || {
            let mut refusals = RefusalLog::default();
            while !stopping.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let open = daemon_connections(&connections);
                        if open >= MAX_RELAY_CONNECTIONS {
                            let services = service_connections(&connections);
                            if let Some(line) = refusals.note(
                                Instant::now(),
                                "marsh-relay",
                                &format!(
                                    "{open}/{MAX_RELAY_CONNECTIONS} daemon connections open, {services}/{MAX_SERVICE_CONNECTIONS} service connections"
                                ),
                            ) {
                                log(&line);
                            }
                            refuse_guest_connection(stream);
                            continue;
                        }
                        if stream.set_nonblocking(false).is_err() {
                            break;
                        }
                        let connection = next.fetch_add(1, Ordering::Relaxed);
                        let Ok(reader) =
                            register_guest_connection(connection, stream, &connections, &tunnel)
                        else {
                            break;
                        };
                        spawn_reader(
                            connection,
                            reader,
                            Arc::clone(&connections),
                            Arc::clone(&tunnel),
                        );
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        })
    };
    let docker_acceptor = docker.map(|docker| {
        let (connections, tunnel, stopping, next) = (
            Arc::clone(&connections),
            Arc::clone(&tunnel),
            Arc::clone(&stopping),
            Arc::clone(&next),
        );
        thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                match docker.accept() {
                    Ok((mut stream, _)) => {
                        if service_connections(&connections) >= MAX_SERVICE_CONNECTIONS {
                            let _ = stream.write_all(&marsh_contracts::process::docker_error_response(
                                "429 Too Many Requests",
                                "marsh: too many open connections to the Docker endpoint in this shell",
                            ));
                            let _ = stream.shutdown(Shutdown::Both);
                            continue;
                        }
                        if stream.set_nonblocking(false).is_err() {
                            break;
                        }
                        let connection = next.fetch_add(1, Ordering::Relaxed);
                        let Ok(reader) = register_guest_service_connection(
                            connection,
                            stream,
                            ChannelKind::Docker,
                            &connections,
                            &tunnel,
                        ) else {
                            break;
                        };
                        spawn_reader(
                            connection,
                            reader,
                            Arc::clone(&connections),
                            Arc::clone(&tunnel),
                        );
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        })
    });
    let forwards = endpoint.map(|_| {
        let (open_connections, open_tunnel, open_next) = (
            Arc::clone(&connections),
            Arc::clone(&tunnel),
            Arc::clone(&next),
        );
        let report_tunnel = Arc::clone(&tunnel);
        // `/run/marsh/<uid>/claims`: who holds each forwarded port, beside the
        // session directories (used only to take a port over under a test break).
        let claims = socket_path
            .parent()
            .and_then(Path::parent)
            .map(|base| base.join("claims"));
        forward::GuestForwards::start(
            Box::new(move |stream, port| {
                if service_connections(&open_connections) >= MAX_SERVICE_CONNECTIONS {
                    return;
                }
                let Ok((ours, theirs)) = UnixStream::pair() else {
                    return;
                };
                let connection = open_next.fetch_add(1, Ordering::Relaxed);
                let Ok(reader) = register_guest_service_connection(
                    connection,
                    ours,
                    ChannelKind::Tcp { port },
                    &open_connections,
                    &open_tunnel,
                ) else {
                    return;
                };
                spawn_reader(
                    connection,
                    reader,
                    Arc::clone(&open_connections),
                    Arc::clone(&open_tunnel),
                );
                forward::pump_tcp(theirs, stream);
            }),
            Box::new(move |port, state| {
                let _ = send(&report_tunnel, &TunnelFrame::PortReport { port, state });
            }),
            claims,
        )
    });

    let outcome = loop {
        match read_frame::<TunnelFrame>(&mut from_host) {
            Ok(TunnelFrame::Data { connection, bytes }) => {
                if let Err(error) = forward_data(&connections, &tunnel, connection, bytes) {
                    break Err(error);
                }
            }
            Ok(TunnelFrame::Credit { connection }) => {
                return_credit(&connections, connection);
            }
            Ok(
                frame @ (TunnelFrame::Eof { connection } | TunnelFrame::Finished { connection }),
            ) => {
                end_after_queued_data(
                    &connections,
                    connection,
                    matches!(frame, TunnelFrame::Finished { .. }),
                );
            }
            Ok(TunnelFrame::Close { connection }) => {
                abort_connection(&connections, connection);
            }
            Ok(TunnelFrame::Ports { ports }) => {
                if let Some(forwards) = &forwards {
                    forwards.set_ports(&ports);
                }
            }
            Ok(TunnelFrame::Shutdown) => {
                let _ = send(&tunnel, &TunnelFrame::Shutdown);
                break Ok(());
            }
            Ok(_) => {
                break Err(DaemonError::InvalidState(
                    "unexpected host relay frame".into(),
                ));
            }
            Err(DaemonError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                break Ok(());
            }
            Err(error) => break Err(error),
        }
    };
    stopping.store(true, Ordering::Relaxed);
    if let Some(forwards) = forwards {
        forwards.shutdown();
    }
    let _ = acceptor.join();
    if let Some(docker_acceptor) = docker_acceptor {
        let _ = docker_acceptor.join();
    }
    close_all_connections(&connections);
    report_guest_cleaned(&tunnel, runtime, outcome.is_ok());
    outcome
}

/// Open connections to the daemon (the services' are counted apart): what
/// both halves of the relay cap at [`MAX_RELAY_CONNECTIONS`].
fn daemon_connections(connections: &Connections) -> usize {
    connections
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .filter(|open| !open.service)
        .count()
}

fn service_connections(connections: &Connections) -> usize {
    connections
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .values()
        .filter(|open| open.service)
        .count()
}

/// One services connection on the host: `peer` is the far end of the socket
/// pair whose near end is the tunnel connection. The channel opens here; its
/// refusal reads on the connection as a Docker error (or just a close), so the
/// text reaches the shell through the ordinary path.
fn serve_service(services: &dyn ShellServices, service: ChannelKind, peer: UnixStream) {
    let _ = peer.set_read_timeout(Some(READ_WAKE));
    let stream = match services.open(service) {
        Ok(stream) => stream,
        Err(message) => {
            let mut peer = peer;
            if matches!(service, ChannelKind::Docker) {
                let _ = peer.write_all(&marsh_contracts::process::docker_error_response(
                    "503 Service Unavailable",
                    &message,
                ));
            }
            let _ = peer.shutdown(Shutdown::Both);
            return;
        }
    };
    let ServiceStream {
        mut reader,
        mut writer,
        finish,
        close,
    } = stream;
    let Ok(mut to_shell) = peer.try_clone() else {
        close();
        return;
    };
    let close = Arc::new(close);
    let down = {
        let close = Arc::clone(&close);
        thread::spawn(move || {
            let mut buffer = vec![0_u8; CHUNK_BYTES];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => {
                        let _ = to_shell.shutdown(Shutdown::Write);
                        return;
                    }
                    Ok(count) => {
                        if to_shell.write_all(&buffer[..count]).is_err() {
                            close();
                            return;
                        }
                    }
                    Err(_) => {
                        close();
                        let _ = to_shell.shutdown(Shutdown::Both);
                        return;
                    }
                }
            }
        })
    };
    let mut from_shell = peer;
    let mut buffer = vec![0_u8; CHUNK_BYTES];
    loop {
        match from_shell.read(&mut buffer) {
            Ok(0) => {
                finish();
                break;
            }
            Ok(count) => {
                if writer.write_all(&buffer[..count]).is_err() {
                    close();
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(_) => {
                close();
                break;
            }
        }
    }
    let _ = down.join();
}

/// In-band cleanup report: the host treats this final frame plus a successful
/// exit of this exec as relay cleanup proof (no separate verify exec).
fn report_guest_cleaned<W: Write>(tunnel: &SharedWriter<W>, runtime: GuestRuntime, ok: bool) {
    let (socket, token, endpoint) = (
        runtime.socket.clone(),
        runtime.token.clone(),
        runtime.endpoint.clone(),
    );
    drop(runtime);
    let gone = |path: &Path| {
        fs::symlink_metadata(path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    };
    if ok && gone(&socket) && gone(&token) && endpoint.as_deref().is_none_or(gone) {
        let _ = send(tunnel, &TunnelFrame::Cleaned);
    }
}

struct GuestRuntime {
    socket: PathBuf,
    token: PathBuf,
    endpoint: Option<PathBuf>,
    directory: PathBuf,
}

impl GuestRuntime {
    fn new(socket: &Path, token: &Path, endpoint: Option<&Path>) -> Result<Self, DaemonError> {
        let directory = parent(socket)?;
        if directory != parent(token)?
            || endpoint.is_some_and(|path| path.parent() != Some(directory))
        {
            return Err(DaemonError::InvalidState(
                "relay socket and token must share a private directory".into(),
            ));
        }
        Ok(Self {
            socket: socket.to_owned(),
            token: token.to_owned(),
            endpoint: endpoint.map(Path::to_owned),
            directory: directory.to_owned(),
        })
    }
}

impl Drop for GuestRuntime {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
        let _ = fs::remove_file(&self.token);
        if let Some(endpoint) = &self.endpoint {
            let _ = fs::remove_file(endpoint);
        }
        // remove_dir is intentionally non-recursive: an unexpected artifact
        // keeps the session directory in place instead of being deleted.
        let _ = fs::remove_dir(&self.directory);
    }
}

fn spawn_reader<W>(
    connection: u64,
    mut source: ConnectionReader,
    connections: Connections,
    tunnel: SharedWriter<W>,
) -> thread::JoinHandle<()>
where
    W: Write + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0_u8; CHUNK_BYTES];
        loop {
            if !source.send_window.acquire() {
                return;
            }
            let read = loop {
                match source.stream.read(&mut buffer) {
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        if source.send_window.closed() {
                            return;
                        }
                    }
                    read => break read,
                }
            };
            // A peer that closes a socket holding input it never read ends it
            // with a reset, after the data it had sent. That is a full close,
            // not a failure: a daemon closes this way when a shell client's
            // last frames arrive after it ended. Ending the connection with
            // `Close` instead would overtake the data still queued behind it.
            let reset = read
                .as_ref()
                .is_err_and(|error| error.kind() == std::io::ErrorKind::ConnectionReset);
            let read = if reset { Ok(0) } else { read };
            match read {
                Ok(0) => {
                    // A zero-byte socket write changes no protocol bytes and
                    // distinguishes a peer that closed both directions from
                    // one that only shut down its sending direction. Retaining
                    // the former would let expired daemon handshakes consume
                    // every relay slot even after their sockets disappeared.
                    let peer_closed = reset
                        || source.stream.write(&[]).is_err_and(|error| {
                            matches!(
                                error.kind(),
                                std::io::ErrorKind::BrokenPipe
                                    | std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::NotConnected
                            )
                        });
                    if let Some(frame) =
                        reader_eof(&connections, connection, &source.identity, peer_closed)
                        && send(&tunnel, &frame).is_err()
                    {
                        finish_connection(&connections, connection, &source.identity);
                    }
                    return;
                }
                Err(_) => break,
                Ok(count) => {
                    if send(
                        &tunnel,
                        &TunnelFrame::Data {
                            connection,
                            bytes: buffer[..count].to_vec(),
                        },
                    )
                    .is_err()
                    {
                        finish_connection(&connections, connection, &source.identity);
                        return;
                    }
                }
            }
        }
        if finish_connection(&connections, connection, &source.identity) {
            let _ = send(&tunnel, &TunnelFrame::Close { connection });
        }
    })
}

fn close_connection(connections: &mut BTreeMap<u64, Connection>, connection: u64) {
    if let Some(client) = connections.remove(&connection) {
        client.send_window.close();
        let _ = client.stream.shutdown(Shutdown::Both);
    }
}

fn abort_connection(connections: &Connections, connection: u64) {
    close_connection(
        &mut connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        connection,
    );
}

fn finish_connection(connections: &Connections, connection: u64, identity: &Arc<()>) -> bool {
    let mut guard = connections
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard
        .get(&connection)
        .is_some_and(|client| Arc::ptr_eq(&client.identity, identity))
    {
        close_connection(&mut guard, connection);
        true
    } else {
        false
    }
}

fn reader_eof(
    connections: &Connections,
    connection: u64,
    identity: &Arc<()>,
    peer_closed: bool,
) -> Option<TunnelFrame> {
    let mut guard = connections
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let client = guard.get_mut(&connection)?;
    if !Arc::ptr_eq(&client.identity, identity) {
        return None;
    }
    client.read_closed = true;
    // A peer that cannot be written to has closed, as much as one whose
    // read reached a reset.
    let peer_closed = peer_closed || client.write_failed;
    if peer_closed || client.end == ConnectionEnd::EofDelivered {
        close_connection(&mut guard, connection);
    }
    // The peer must drain this reader's final queued data before closing.
    // A fully closed socket also ends the remote reader: otherwise a guest
    // retaining its write direction would indefinitely consume a relay slot.
    Some(if peer_closed {
        TunnelFrame::Finished { connection }
    } else {
        TunnelFrame::Eof { connection }
    })
}

/// A write to the connection's peer failed: it has closed. What the remote
/// relay still sends for it is discarded, but the connection ends only after
/// everything this side's reader forwarded, so never with a `Close`, which the
/// remote applies at once and which drops what it has queued.
///
/// If the reader is still running it ends the connection when the peer's
/// socket reaches its end (`reader_eof`). If it has already ended (the peer
/// half-closed first), no reader remains to do that: the connection ends here,
/// with the ordered `Finished` that tells the remote relay the peer is gone.
fn writer_failed(
    connections: &Connections,
    connection: u64,
    identity: &Arc<()>,
) -> Option<TunnelFrame> {
    let mut guard = connections
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let client = guard.get_mut(&connection)?;
    if !Arc::ptr_eq(&client.identity, identity) {
        return None;
    }
    client.write_failed = true;
    if !client.read_closed {
        return None;
    }
    close_connection(&mut guard, connection);
    Some(TunnelFrame::Finished { connection })
}

/// Whether this connection (this identity of it) is still in the map.
fn registered(connections: &Connections, connection: u64, identity: &Arc<()>) -> bool {
    connections
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&connection)
        .is_some_and(|client| Arc::ptr_eq(&client.identity, identity))
}

fn writer_eof(connections: &Connections, connection: u64, identity: &Arc<()>) -> bool {
    let mut guard = connections
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(client) = guard.get_mut(&connection) else {
        return false;
    };
    if !Arc::ptr_eq(&client.identity, identity) {
        return false;
    }
    if client.read_closed {
        close_connection(&mut guard, connection);
        true
    } else {
        if client.end != ConnectionEnd::FinishQueued {
            client.end = ConnectionEnd::EofDelivered;
        }
        false
    }
}

fn return_credit(connections: &Connections, connection: u64) {
    if let Some(client) = connections
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&connection)
    {
        client.send_window.release();
    }
}

fn forward_data<W: Write>(
    connections: &Connections,
    tunnel: &SharedWriter<W>,
    connection: u64,
    bytes: Vec<u8>,
) -> Result<(), DaemonError> {
    let mut guard = connections
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(client) = guard.get(&connection) else {
        return Ok(());
    };
    if client.end != ConnectionEnd::Open {
        return Ok(());
    }
    if bytes.len() <= CHUNK_BYTES
        && client.pending_chunks.load(Ordering::Acquire) < MAX_PENDING_CHUNKS
    {
        client.pending_chunks.fetch_add(1, Ordering::AcqRel);
        if client.writer.try_send(ConnectionWrite::Data(bytes)).is_ok() {
            return Ok(());
        }
        client.pending_chunks.fetch_sub(1, Ordering::AcqRel);
    }
    close_connection(&mut guard, connection);
    drop(guard);
    send(tunnel, &TunnelFrame::Close { connection })
}

fn end_after_queued_data(connections: &Connections, connection: u64, finish: bool) {
    let mut guard = connections
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(client) = guard.get_mut(&connection) {
        let message = if finish && client.end != ConnectionEnd::FinishQueued {
            client.end = ConnectionEnd::FinishQueued;
            ConnectionWrite::Finish
        } else if !finish && client.end == ConnectionEnd::Open {
            client.end = ConnectionEnd::EofQueued;
            ConnectionWrite::Eof
        } else {
            return;
        };
        // Reserve one slot each for graceful EOF and subsequent full close.
        if client.writer.try_send(message).is_err() {
            close_connection(&mut guard, connection);
        }
    }
}

fn close_all_connections(connections: &Connections) {
    let mut guard = connections
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (_, client) in std::mem::take(&mut *guard) {
        client.send_window.close();
        let _ = client.stream.shutdown(Shutdown::Both);
    }
}

fn register_guest_connection<W: Write + Send + 'static>(
    connection: u64,
    stream: UnixStream,
    connections: &Connections,
    tunnel: &SharedWriter<W>,
) -> Result<ConnectionReader, DaemonError> {
    let reader = register_connection(connection, stream, connections, tunnel)?;
    if let Err(error) = send(tunnel, &TunnelFrame::Open { connection }) {
        close_connection(
            &mut connections
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            connection,
        );
        return Err(error);
    }
    Ok(reader)
}

fn register_guest_service_connection<W: Write + Send + 'static>(
    connection: u64,
    stream: UnixStream,
    service: ChannelKind,
    connections: &Connections,
    tunnel: &SharedWriter<W>,
) -> Result<ConnectionReader, DaemonError> {
    let reader = register_connection_as(connection, stream, connections, tunnel, true)?;
    if let Err(error) = send(
        tunnel,
        &TunnelFrame::OpenService {
            connection,
            service,
        },
    ) {
        close_connection(
            &mut connections
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            connection,
        );
        return Err(error);
    }
    Ok(reader)
}

fn register_connection<W: Write + Send + 'static>(
    connection: u64,
    stream: UnixStream,
    connections: &Connections,
    tunnel: &SharedWriter<W>,
) -> Result<ConnectionReader, DaemonError> {
    register_connection_as(connection, stream, connections, tunnel, false)
}

fn register_connection_as<W: Write + Send + 'static>(
    connection: u64,
    mut stream: UnixStream,
    connections: &Connections,
    tunnel: &SharedWriter<W>,
    service: bool,
) -> Result<ConnectionReader, DaemonError> {
    let identity = Arc::new(());
    let send_window = Arc::new(SendWindow::new());
    // EINVAL (macOS): the peer has already closed, so reads cannot block.
    if let Err(error) = stream.set_read_timeout(Some(READ_WAKE))
        && error.raw_os_error() != Some(rustix::io::Errno::INVAL.raw_os_error())
    {
        return Err(error.into());
    }
    let reader = ConnectionReader {
        stream: stream.try_clone()?,
        identity: Arc::clone(&identity),
        send_window: Arc::clone(&send_window),
    };
    let shutdown = stream.try_clone()?;
    let (writer, pending) = mpsc::sync_channel(MAX_PENDING_CHUNKS + 2);
    let pending_chunks = Arc::new(AtomicUsize::new(0));
    let mut guard = connections
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.contains_key(&connection) {
        let _ = stream.shutdown(Shutdown::Both);
        return Err(DaemonError::InvalidState(
            "relay reused an active connection identity".into(),
        ));
    }
    guard.insert(
        connection,
        Connection {
            service,
            stream: shutdown,
            writer,
            pending_chunks: Arc::clone(&pending_chunks),
            send_window,
            identity: Arc::clone(&identity),
            read_closed: false,
            write_failed: false,
            end: ConnectionEnd::Open,
        },
    );
    drop(guard);
    let connections = Arc::clone(connections);
    let tunnel = Arc::clone(tunnel);
    thread::spawn(move || {
        // Once a write fails the peer reads no more (`writer_failed`): what
        // arrives for it is discarded, and credited so the far side's sender
        // is not left waiting.
        let mut unwritable = false;
        while let Ok(message) = pending.recv() {
            let failed = match message {
                ConnectionWrite::Data(bytes) => {
                    let failed = !unwritable && stream.write_all(&bytes).is_err();
                    pending_chunks.fetch_sub(1, Ordering::AcqRel);
                    // A connection closed here meanwhile (a teardown, an abort)
                    // has nothing left to credit: crediting would let the far
                    // side's source run on, its data thrown away, until that
                    // side is torn down too.
                    if (failed || unwritable) && !registered(&connections, connection, &identity) {
                        return;
                    }
                    if send(&tunnel, &TunnelFrame::Credit { connection }).is_err() {
                        break;
                    }
                    failed
                }
                ConnectionWrite::Eof => {
                    let failed = !unwritable && stream.shutdown(Shutdown::Write).is_err();
                    if !failed && writer_eof(&connections, connection, &identity) {
                        return;
                    }
                    failed
                }
                ConnectionWrite::Finish => {
                    finish_connection(&connections, connection, &identity);
                    return;
                }
            };
            if failed {
                unwritable = true;
                if let Some(frame) = writer_failed(&connections, connection, &identity) {
                    let _ = send(&tunnel, &frame);
                    return;
                }
            }
        }
        if finish_connection(&connections, connection, &identity) {
            let _ = send(&tunnel, &TunnelFrame::Close { connection });
        }
    });
    Ok(reader)
}

fn send<W: Write>(writer: &SharedWriter<W>, frame: &TunnelFrame) -> Result<(), DaemonError> {
    let mut writer = writer
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    write_frame(&mut *writer, frame)
}

fn prepare_guest_runtime(
    socket_path: &Path,
    token_path: &Path,
    token: &str,
) -> Result<(), DaemonError> {
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DaemonError::InvalidState("invalid relay token".into()));
    }
    let socket_parent = parent(socket_path)?;
    if socket_parent != parent(token_path)? {
        return Err(DaemonError::InvalidState(
            "relay socket and token must share a private directory".into(),
        ));
    }
    let runtime_base = parent(socket_parent)?;
    let base_metadata = fs::symlink_metadata(runtime_base)?;
    if base_metadata.file_type().is_symlink()
        || !base_metadata.is_dir()
        || base_metadata.uid() != rustix::process::getuid().as_raw()
        || base_metadata.mode() & 0o022 != 0
    {
        return Err(DaemonError::UnsafeToken(runtime_base.to_owned()));
    }
    match DirBuilder::new().mode(0o700).create(socket_parent) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(DaemonError::EndpointExists(socket_parent.to_owned()));
        }
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(socket_parent)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(DaemonError::UnsafeToken(socket_parent.to_owned()));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(token_path)?;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn parent(path: &Path) -> Result<&Path, DaemonError> {
    path.parent()
        .filter(|parent| parent.is_absolute() && *parent != Path::new("/"))
        .ok_or_else(|| DaemonError::UnsafeToken(PathBuf::from(path)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{PATIENCE, read_to_end, recv, socket_pair, wait_until};
    use crate::{
        AttachmentFrame, Client, DaemonBackend, DaemonStore, ExecuteSpec, LoadSelection,
        PreparationProgress, PreparationResult, PublicReply, PublicRequest, Server,
        ServerAttachment, SessionSpec, ShellSpec,
    };
    use std::{
        io::Cursor,
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::Instant,
    };

    struct RejectWrites;

    impl Write for RejectWrites {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "test tunnel closed",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct RejectAfterInitialize(bool);

    impl Write for RejectAfterInitialize {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            if self.0 {
                Err(std::io::ErrorKind::BrokenPipe.into())
            } else {
                Ok(buffer.len())
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.0 = true;
            Ok(())
        }
    }

    /// Bound a relay join so a protocol regression fails instead of hanging.
    fn join_within(
        what: &str,
        handle: thread::JoinHandle<Result<(), DaemonError>>,
    ) -> Result<(), DaemonError> {
        wait_until(what, || handle.is_finished());
        handle.join().unwrap()
    }

    /// The relay's next connection to the daemon socket; `None` if it never
    /// arrives (the relay refused the guest instead).
    fn accept_next(listener: &UnixListener) -> Option<UnixStream> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            match listener.accept() {
                Ok((peer, _)) => return Some(peer),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(_) => return None,
            }
        }
    }

    #[test]
    fn failed_capacity_rejection_closes_already_open_daemon_connections() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (accepted_send, accepted_receive) = mpsc::channel();
        let acceptor = thread::spawn(move || {
            let peers = (0..MAX_RELAY_CONNECTIONS)
                .map(|_| listener.accept().unwrap().0)
                .collect::<Vec<_>>();
            accepted_send.send(peers).unwrap();
        });
        let mut input = Vec::new();
        write_frame(&mut input, &TunnelFrame::Ready).unwrap();
        for connection in 1..=MAX_RELAY_CONNECTIONS as u64 + 1 {
            write_frame(&mut input, &TunnelFrame::Open { connection }).unwrap();
        }
        let result = run_host(
            Cursor::new(input),
            RejectAfterInitialize::default(),
            &socket,
            "a".repeat(64),
        );
        let mut peers = recv(&accepted_receive, "daemon connections accepted");
        let all_closed = peers.iter_mut().all(|peer| {
            peer.set_nonblocking(true).unwrap();
            matches!(peer.read(&mut [0_u8; 1]), Ok(0))
        });
        drop(peers);
        acceptor.join().unwrap();
        assert!(result.is_err());
        assert!(
            all_closed,
            "capacity reply failure left daemon connections open"
        );
    }

    #[test]
    fn host_relay_rejects_the_connection_after_its_bounded_capacity() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (accepted_send, accepted_receive) = mpsc::channel();
        let (release_send, release_receive) = mpsc::channel();
        let acceptor = thread::spawn(move || {
            let mut accepted = Vec::new();
            for _ in 0..MAX_RELAY_CONNECTIONS {
                accepted.push(listener.accept().unwrap().0);
            }
            accepted_send.send(()).unwrap();
            release_receive.recv().unwrap();
        });
        let (host, mut guest) = socket_pair();
        guest.set_read_timeout(Some(PATIENCE)).unwrap();
        let host_reader = host.try_clone().unwrap();
        let relay = thread::spawn(move || run_host(host_reader, host, &socket, "a".repeat(64)));
        assert!(matches!(
            read_frame::<TunnelFrame>(&mut guest).unwrap(),
            TunnelFrame::Initialize { .. }
        ));
        write_frame(&mut guest, &TunnelFrame::Ready).unwrap();
        for connection in 1..=MAX_RELAY_CONNECTIONS as u64 + 1 {
            write_frame(&mut guest, &TunnelFrame::Open { connection }).unwrap();
        }
        recv(&accepted_receive, "daemon connections accepted");
        let refused = MAX_RELAY_CONNECTIONS as u64 + 1;
        // The reason, then the connection's ordered end; never a bare Close.
        let TunnelFrame::Data { connection, bytes } =
            read_frame::<TunnelFrame>(&mut guest).unwrap()
        else {
            panic!("the refused connection was not given a reason");
        };
        assert_eq!(connection, refused);
        assert!(matches!(
            read_frame::<PublicReply>(&mut Cursor::new(bytes)),
            Ok(PublicReply::Error { message, .. }) if message.contains("try again")
        ));
        assert_eq!(
            read_frame::<TunnelFrame>(&mut guest).unwrap(),
            TunnelFrame::Finished {
                connection: refused
            }
        );
        // The host waits for the guest's in-band Cleaned (or EOF) after Shutdown.
        write_frame(&mut guest, &TunnelFrame::Shutdown).unwrap();
        write_frame(&mut guest, &TunnelFrame::Cleaned).unwrap();
        release_send.send(()).unwrap();
        join_within("the host relay stops", relay).unwrap();
        acceptor.join().unwrap();
    }

    #[test]
    fn stalled_connection_does_not_block_a_sibling_or_relay_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (host, mut guest) = socket_pair();
        guest.set_read_timeout(Some(PATIENCE)).unwrap();
        guest.set_write_timeout(Some(PATIENCE)).unwrap();
        let host_reader = host.try_clone().unwrap();
        let relay = thread::spawn(move || run_host(host_reader, host, &socket, "a".repeat(64)));
        assert!(matches!(
            read_frame::<TunnelFrame>(&mut guest).unwrap(),
            TunnelFrame::Initialize { .. }
        ));
        write_frame(&mut guest, &TunnelFrame::Ready).unwrap();
        write_frame(&mut guest, &TunnelFrame::Open { connection: 1 }).unwrap();
        let stalled = listener.accept().unwrap().0;
        write_frame(&mut guest, &TunnelFrame::Open { connection: 2 }).unwrap();
        // A permanently blocked sibling never gets its bytes, so these bounds only
        // need to outlast a starved machine, not to be tight.
        let mut sibling = listener.accept().unwrap().0;
        sibling.set_read_timeout(Some(PATIENCE)).unwrap();
        let sender = thread::spawn(move || {
            for _ in 0..256 {
                write_frame(
                    &mut guest,
                    &TunnelFrame::Data {
                        connection: 1,
                        bytes: vec![b'x'; CHUNK_BYTES],
                    },
                )?;
            }
            write_frame(
                &mut guest,
                &TunnelFrame::Data {
                    connection: 2,
                    bytes: b"sibling".to_vec(),
                },
            )?;
            write_frame(&mut guest, &TunnelFrame::Eof { connection: 2 })?;
            Ok::<_, DaemonError>(guest)
        });
        let mut actual = [0; 7];
        let sibling_result = sibling.read_exact(&mut actual);
        // Release every socket before assertions so a failing regression never
        // leaves a relay thread blocked on the deliberately stalled recipient.
        let _ = stalled.shutdown(Shutdown::Both);
        let mut guest = sender.join().unwrap().unwrap();
        write_frame(&mut guest, &TunnelFrame::Shutdown).unwrap();
        write_frame(&mut guest, &TunnelFrame::Cleaned).unwrap();
        join_within("the host relay stops", relay).unwrap();
        sibling_result.unwrap();
        assert_eq!(&actual, b"sibling");
    }

    #[test]
    fn explicit_close_aborts_an_unread_sink_and_allows_connection_reuse() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (host, mut guest) = socket_pair();
        guest.set_read_timeout(Some(PATIENCE)).unwrap();
        guest.set_write_timeout(Some(PATIENCE)).unwrap();
        let host_reader = host.try_clone().unwrap();
        let relay = thread::spawn(move || run_host(host_reader, host, &socket, "a".repeat(64)));
        assert!(matches!(
            read_frame::<TunnelFrame>(&mut guest).unwrap(),
            TunnelFrame::Initialize { .. }
        ));
        // Drain the relay's frames (credits, closes) as a real guest does; an
        // undrained tunnel would eventually block the relay's writers.
        let mut frames = guest.try_clone().unwrap();
        let drain = thread::spawn(move || while read_frame::<TunnelFrame>(&mut frames).is_ok() {});
        write_frame(&mut guest, &TunnelFrame::Ready).unwrap();
        write_frame(&mut guest, &TunnelFrame::Open { connection: 1 }).unwrap();
        let mut stalled = listener.accept().unwrap().0;
        // This peer obeys the initial credit window but never drains the sink.
        // Close must interrupt the socket writer instead of waiting behind it.
        for _ in 0..MAX_PENDING_CHUNKS {
            write_frame(
                &mut guest,
                &TunnelFrame::Data {
                    connection: 1,
                    bytes: vec![b'x'; CHUNK_BYTES],
                },
            )
            .unwrap();
        }
        thread::sleep(Duration::from_millis(100));
        write_frame(&mut guest, &TunnelFrame::Close { connection: 1 }).unwrap();
        write_frame(&mut guest, &TunnelFrame::Open { connection: 1 }).unwrap();
        let mut replacement = listener.accept().unwrap().0;
        replacement.set_read_timeout(Some(PATIENCE)).unwrap();
        // Portable disconnect proof: the abandoned peer reaches EOF before the
        // queued megabyte arrives. (macOS accepts writes after a peer's
        // SHUT_RD, so a failed write is not a portable observation.)
        let abandoned = read_to_end(&mut stalled);
        write_frame(
            &mut guest,
            &TunnelFrame::Data {
                connection: 1,
                bytes: b"replacement".to_vec(),
            },
        )
        .unwrap();
        write_frame(&mut guest, &TunnelFrame::Eof { connection: 1 }).unwrap();
        let delivered = read_to_end(&mut replacement);
        // Always release blocked writers before assertions, including with
        // the previous implementation that queued Close behind the data. The
        // relay stops in band: macOS can miss the wakeup of a tunnel read
        // after a socket shutdown. Closing both daemon peers fully then ends
        // every connection thread, so the tunnel closes and the drain ends.
        write_frame(&mut guest, &TunnelFrame::Shutdown).unwrap();
        write_frame(&mut guest, &TunnelFrame::Cleaned).unwrap();
        join_within("the host relay stops", relay).unwrap();
        drop((stalled, replacement));
        wait_until("the tunnel drain ends", || drain.is_finished());
        assert!(
            abandoned.is_ok_and(|bytes| bytes.len() < MAX_PENDING_CHUNKS * CHUNK_BYTES),
            "Close left the abandoned socket connected or delivered all queued data"
        );
        assert_eq!(delivered.unwrap(), b"replacement");
    }

    #[test]
    fn daemon_full_close_releases_slots_without_waiting_for_guest_eof() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (host, mut guest) = socket_pair();
        guest.set_read_timeout(Some(PATIENCE)).unwrap();
        let host_reader = host.try_clone().unwrap();
        let relay = thread::spawn(move || run_host(host_reader, host, &socket, "a".repeat(64)));
        assert!(matches!(
            read_frame::<TunnelFrame>(&mut guest).unwrap(),
            TunnelFrame::Initialize { .. }
        ));
        write_frame(&mut guest, &TunnelFrame::Ready).unwrap();
        let mut completed = 0;
        for connection in 1..=MAX_RELAY_CONNECTIONS as u64 * 2 {
            write_frame(&mut guest, &TunnelFrame::Open { connection }).unwrap();
            let Some(peer) = accept_next(&listener) else {
                break;
            };
            // Emulates daemon handshake expiry: the guest keeps its sending
            // side open, but the daemon drops the whole socket without a reply.
            drop(peer);
            if read_frame::<TunnelFrame>(&mut guest).unwrap()
                != (TunnelFrame::Finished { connection })
            {
                break;
            }
            completed += 1;
        }
        write_frame(&mut guest, &TunnelFrame::Shutdown).unwrap();
        write_frame(&mut guest, &TunnelFrame::Cleaned).unwrap();
        join_within("the host relay stops", relay).unwrap();
        assert_eq!(
            completed,
            MAX_RELAY_CONNECTIONS * 2,
            "fully closed daemon sockets retained relay slots"
        );
    }

    struct PairedRelays {
        _root: tempfile::TempDir,
        socket: PathBuf,
        listener: UnixListener,
        tunnel: SharedWriter<UnixStream>,
        cleaned: Arc<AtomicBool>,
        host: thread::JoinHandle<Result<(), DaemonError>>,
        guest: thread::JoinHandle<Result<(), DaemonError>>,
    }

    impl PairedRelays {
        fn new() -> Self {
            Self::start(None, Arc::new(|_| {}))
        }

        /// With the shell's services endpoint (`docker.sock` beside the
        /// socket) served by `services`.
        fn with_services(services: Arc<dyn ShellServices>) -> Self {
            Self::start(Some(services), Arc::new(|_| {}))
        }

        /// Saying what the guest relay refuses through `log`.
        fn logging(log: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
            Self::start(None, log)
        }

        fn start(
            services: Option<Arc<dyn ShellServices>>,
            log: Arc<dyn Fn(&str) + Send + Sync>,
        ) -> Self {
            let has_services = services.is_some();
            let root = tempfile::tempdir().unwrap();
            fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let daemon_socket = root.path().join("daemon.sock");
            let listener = UnixListener::bind(&daemon_socket).unwrap();
            let socket = root.path().join("session/socket");
            let token = root.path().join("session/token");
            let (host_stream, guest_stream) = socket_pair();
            let host_reader = host_stream.try_clone().unwrap();
            let guest_reader = guest_stream.try_clone().unwrap();
            let tunnel = Arc::new(Mutex::new(host_stream));
            let cleaned = Arc::new(AtomicBool::new(false));
            let (ready, received) = mpsc::channel();
            let host = {
                let tunnel = Arc::clone(&tunnel);
                let cleaned = Arc::clone(&cleaned);
                thread::spawn(move || {
                    run_host_shared_with_services(
                        host_reader,
                        &tunnel,
                        &daemon_socket,
                        "a".repeat(64),
                        |result| {
                            ready.send(result.map(|_| ())).unwrap();
                        },
                        &cleaned,
                        services.as_ref(),
                    )
                })
            };
            let guest_socket = socket.clone();
            let endpoint = has_services.then(|| socket.with_file_name("docker.sock"));
            let guest = thread::spawn(move || {
                run_guest_logged(
                    guest_reader,
                    guest_stream,
                    &guest_socket,
                    &token,
                    endpoint.as_deref(),
                    log,
                )
            });
            recv(&received, "relay readiness").unwrap();
            Self {
                _root: root,
                socket,
                listener,
                tunnel,
                cleaned,
                host,
                guest,
            }
        }

        fn connect(&self) -> (UnixStream, UnixStream) {
            let guest = UnixStream::connect(&self.socket).unwrap();
            let host = self.listener.accept().unwrap().0;
            for stream in [&guest, &host] {
                stream.set_read_timeout(Some(PATIENCE)).unwrap();
                stream.set_write_timeout(Some(PATIENCE)).unwrap();
            }
            (guest, host)
        }

        /// Stop both relays in band, as the backend does. A `shutdown` of the
        /// tunnel socket would rely on macOS waking the relays' blocked
        /// tunnel reads, which it sometimes never does (see `READ_WAKE`).
        fn stop(self) {
            request_guest_shutdown(&self.tunnel).unwrap();
            join_within("the guest relay stops", self.guest).unwrap();
            join_within("the host relay stops", self.host).unwrap();
            assert!(self.cleaned.load(Ordering::Acquire));
            assert!(!self.socket.exists());
        }
    }

    /// What the caller does after the daemon has closed its socket.
    #[derive(Clone, Copy)]
    enum Afterwards {
        Nothing,
        /// Writes more (a shell's late stdin frames and credits do).
        Writes,
        /// Ends its sending direction, as a caller that has sent its request does.
        HalfCloses,
    }

    /// The daemon answers and closes its socket while the response is still
    /// on its way: nobody reads it yet, so the daemon writes until every
    /// buffer on the way is full and part of it is still with the host relay
    /// when the socket closes. `unread_input` is what the caller sent before
    /// that, which the daemon never reads (its close then ends in a reset).
    ///
    /// The caller then reads half the response and stops for a while, so the
    /// relays reach the end of the connection while the rest is still queued
    /// at the guest relay, and an end that was a `Close` would drop it.
    fn response_survives_the_daemon_closing(unread_input: bool, afterwards: Afterwards) {
        let relays = PairedRelays::new();
        let mut guest = UnixStream::connect(&relays.socket).unwrap();
        guest.set_read_timeout(Some(PATIENCE)).unwrap();
        let mut peer = relays.listener.accept().unwrap().0;
        if unread_input {
            guest.write_all(b"early input").unwrap();
            thread::sleep(Duration::from_millis(100));
        }
        // The size depends on the platform's socket buffers, so it is
        // whatever the daemon got written.
        peer.set_write_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let pattern: Vec<u8> = (0..CHUNK_BYTES)
            .map(|index| u8::try_from(index % 251).unwrap())
            .collect();
        let mut written = 0_usize;
        while written < 16 << 20 {
            match peer.write(&pattern[written % CHUNK_BYTES..]) {
                Ok(count) => written += count,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(error) => panic!("the daemon's write failed: {error}"),
            }
        }
        assert!(written > 0 && written < 16 << 20, "{written}");
        drop(peer);
        thread::sleep(Duration::from_millis(100));
        match afterwards {
            Afterwards::Nothing => {}
            Afterwards::Writes => guest.write_all(b"late input").unwrap(),
            Afterwards::HalfCloses => guest.shutdown(Shutdown::Write).unwrap(),
        }
        let mut response = vec![0_u8; written / 2];
        guest.read_exact(&mut response).unwrap();
        thread::sleep(Duration::from_millis(300));
        let rest = read_to_end(&mut guest);
        relays.stop();
        let expected: Vec<u8> = (0..written)
            .map(|index| pattern[index % CHUNK_BYTES])
            .collect();
        response.extend(rest.as_ref().map_or_default(Vec::as_slice));
        assert!(
            rest.is_ok() && response == expected,
            "the response was cut short: {} of {written} bytes ({:?})",
            response.len(),
            rest.as_ref().err()
        );
    }

    #[test]
    fn a_caller_writing_after_the_daemon_closed_still_gets_the_whole_response() {
        response_survives_the_daemon_closing(false, Afterwards::Writes);
    }

    #[test]
    fn a_caller_half_closing_after_the_daemon_closed_still_gets_the_whole_response() {
        response_survives_the_daemon_closing(false, Afterwards::HalfCloses);
    }

    #[test]
    fn a_daemon_that_closes_with_unread_input_still_delivers_the_whole_response() {
        response_survives_the_daemon_closing(true, Afterwards::Nothing);
    }

    /// The caller half-closes (its reader has ended and sent `Eof`), then goes
    /// away, and a response then fails to write: no reader remains to end the
    /// connection, so the failed write must, and with the ordered `Finished`
    /// that tells the remote relay, never a `Close`.
    #[test]
    fn a_failed_write_after_the_reader_ended_releases_the_connection() {
        let connections: Connections = Arc::default();
        let (stream, peer) = socket_pair();
        let tunnel = Arc::new(Mutex::new(Vec::new()));
        let reader = register_connection(1, stream, &connections, &tunnel).unwrap();
        let task = spawn_reader(1, reader, Arc::clone(&connections), Arc::clone(&tunnel));
        peer.shutdown(Shutdown::Write).unwrap();
        wait_until("the reader sees the half-close", || task.is_finished());
        task.join().unwrap();
        assert_eq!(connections.lock().unwrap().len(), 1);
        drop(peer);
        forward_data(&connections, &tunnel, 1, b"late response".to_vec()).unwrap();
        // The connection is removed before its `Finished` is sent, so wait for
        // the frame itself, not for the removal.
        let tunnel_frames = || {
            let sent = tunnel.lock().unwrap().clone();
            let mut sent = Cursor::new(sent);
            let mut frames = Vec::new();
            while let Ok(frame) = read_frame::<TunnelFrame>(&mut sent) {
                frames.push(frame);
            }
            frames
        };
        wait_until("the failed write ends the connection with Finished", || {
            tunnel_frames().contains(&TunnelFrame::Finished { connection: 1 })
        });
        assert!(connections.lock().unwrap().is_empty());
        let frames = tunnel_frames();
        assert!(
            frames.contains(&TunnelFrame::Eof { connection: 1 }),
            "{frames:?}"
        );
        assert!(
            frames.contains(&TunnelFrame::Finished { connection: 1 }),
            "{frames:?}"
        );
        assert!(
            !frames.contains(&TunnelFrame::Close { connection: 1 }),
            "{frames:?}"
        );
    }

    /// The caller's end of stream reaches a connection whose peer has closed
    /// (on macOS the half-close then fails with ENOTCONN). That is not a
    /// reason to send `Close`, which the remote applies at once, dropping what
    /// it has queued for its own peer: the connection stays until its reader
    /// reports the peer's end.
    #[test]
    fn a_half_close_toward_a_closed_peer_does_not_send_close() {
        let connections: Connections = Arc::default();
        let (stream, peer) = socket_pair();
        let tunnel = Arc::new(Mutex::new(Vec::new()));
        let _reader = register_connection(1, stream, &connections, &tunnel).unwrap();
        drop(peer);
        end_after_queued_data(&connections, 1, false);
        wait_until("the writer has handled the end of stream", || {
            connections.lock().unwrap().get(&1).is_none_or(|client| {
                client.write_failed || client.end == ConnectionEnd::EofDelivered
            })
        });
        thread::sleep(Duration::from_millis(100));
        assert_eq!(connections.lock().unwrap().len(), 1);
        let sent = tunnel.lock().unwrap().clone();
        let mut sent = Cursor::new(sent);
        while let Ok(frame) = read_frame::<TunnelFrame>(&mut sent) {
            assert!(!matches!(frame, TunnelFrame::Close { .. }), "{frame:?}");
        }
        close_all_connections(&connections);
    }

    #[test]
    fn daemon_full_close_drains_final_data_and_releases_both_relay_slots() {
        let relays = PairedRelays::new();
        relays.listener.set_nonblocking(true).unwrap();
        let expected = vec![b'x'; 128 * 1024];
        let mut retained_clients = Vec::new();
        let mut complete_responses = true;
        for _ in 0..MAX_RELAY_CONNECTIONS * 2 {
            let mut guest = UnixStream::connect(&relays.socket).unwrap();
            guest.set_read_timeout(Some(PATIENCE)).unwrap();
            let Some(mut peer) = accept_next(&relays.listener) else {
                break;
            };
            peer.set_nonblocking(false).unwrap();
            peer.set_write_timeout(Some(PATIENCE)).unwrap();
            peer.write_all(&expected).unwrap();
            drop(peer);
            // Let full-close reach the guest while its final bytes remain
            // queued behind an intentionally unread socket buffer.
            thread::sleep(Duration::from_millis(25));
            if read_to_end(&mut guest).ok().as_ref() != Some(&expected) {
                complete_responses = false;
                break;
            }
            // Keep this socket's write direction open for the whole test.
            // Neither relay may depend on the caller dropping it to reclaim
            // a daemon connection that has already fully closed.
            retained_clients.push(guest);
        }
        relays.stop();
        assert!(
            complete_responses,
            "full-close truncated the final response"
        );
        assert_eq!(
            retained_clients.len(),
            MAX_RELAY_CONNECTIONS * 2,
            "retained guest sockets consumed closed daemon relay slots"
        );
    }

    /// The reason a refused caller reads, from whichever of its write or its
    /// read carried it.
    fn refusal_read(result: Result<PublicReply, DaemonError>) -> String {
        match result {
            Ok(PublicReply::Error { message, .. }) | Err(DaemonError::Remote(message)) => message,
            other => panic!("the refused connection read {other:?}, not a refusal"),
        }
    }

    fn fill_to_capacity(relays: &PairedRelays) -> Vec<(UnixStream, UnixStream)> {
        (0..MAX_RELAY_CONNECTIONS)
            .map(|_| relays.connect())
            .collect()
    }

    fn client(relays: &PairedRelays) -> Client {
        Client::connect_relay(&relays.socket, &relays.socket.with_file_name("token")).unwrap()
    }

    #[test]
    fn guest_relay_past_its_capacity_refuses_with_a_frame_not_a_reset() {
        let relays = PairedRelays::new();
        let held = fill_to_capacity(&relays);
        let mut excess = UnixStream::connect(&relays.socket).unwrap();
        excess.set_read_timeout(Some(PATIENCE)).unwrap();
        excess.set_write_timeout(Some(PATIENCE)).unwrap();
        // A client writes its request before it reads the reply; left unread
        // by the relay, that request turns the close into a reset (ECONNRESET
        // on Linux, where the shell's relay runs). Whether the write lands
        // before the relay's close is timing, so only the read is checked.
        let _ = write_frame(&mut excess, &PublicRequest::Ping);
        let reply = read_frame::<PublicReply>(&mut excess);
        drop(held);
        relays.stop();
        let message = refusal_read(reply);
        assert!(
            message.contains(&MAX_RELAY_CONNECTIONS.to_string()) && message.contains("try again"),
            "{message}"
        );
    }

    #[test]
    fn a_client_that_writes_after_the_refusal_reads_the_reason_not_a_broken_pipe() {
        let relays = PairedRelays::new();
        let held = fill_to_capacity(&relays);
        let mut excess = UnixStream::connect(&relays.socket).unwrap();
        excess.set_write_timeout(Some(PATIENCE)).unwrap();
        // The relay refuses within its poll; this client is scheduled later.
        thread::sleep(Duration::from_millis(100));
        let result = crate::write_request(&mut excess, &PublicRequest::Ping)
            .and_then(|()| read_frame::<PublicReply>(&mut excess));
        drop(held);
        relays.stop();
        assert!(refusal_read(result).contains("try again"));
    }

    #[test]
    fn a_real_client_with_a_large_request_reads_the_capacity_refusal() {
        let relays = PairedRelays::new();
        let held = fill_to_capacity(&relays);
        // More than the socket buffers hold: the write is still going when
        // the relay has refused and closed.
        let reply = client(&relays).request(PublicRequest::OpenUrl {
            url: "x".repeat(512 * 1024),
        });
        drop(held);
        relays.stop();
        assert!(refusal_read(reply).contains("try again"));
    }

    #[test]
    fn a_real_client_reads_the_refusal_when_the_host_cannot_reach_the_daemon() {
        let relays = PairedRelays::new();
        fs::remove_file(
            relays
                .socket
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("daemon.sock"),
        )
        .unwrap();
        let client = client(&relays);
        let small = client.request(PublicRequest::Ping);
        let large = client.request(PublicRequest::OpenUrl {
            url: "x".repeat(512 * 1024),
        });
        relays.stop();
        for reply in [small, large] {
            assert!(refusal_read(reply).contains("could not open a connection to the daemon"));
        }
    }

    /// A services channel held open on the host, so the shell has one
    /// services connection.
    struct HoldingService(Arc<AtomicBool>);

    struct PendingRead(mpsc::Receiver<()>);

    impl Read for PendingRead {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            Ok(0)
        }
    }

    impl ShellServices for HoldingService {
        fn open(&self, _service: ChannelKind) -> Result<ServiceStream, String> {
            let (finish, finished) = mpsc::channel();
            let close = finish.clone();
            self.0.store(true, Ordering::Release);
            Ok(ServiceStream {
                reader: Box::new(PendingRead(finished)),
                writer: Box::new(std::io::sink()),
                finish: Box::new(move || {
                    let _ = finish.send(());
                }),
                close: Box::new(move || {
                    let _ = close.send(());
                }),
            })
        }

        fn port_report(&self, _port: u16, _state: &PortState) {}
    }

    #[test]
    fn a_services_connection_does_not_take_a_daemon_slot_on_the_host() {
        let opened = Arc::new(AtomicBool::new(false));
        let relays = PairedRelays::with_services(Arc::new(HoldingService(Arc::clone(&opened))));
        let service = UnixStream::connect(relays.socket.with_file_name("docker.sock")).unwrap();
        wait_until("the service reaches the host", || {
            opened.load(Ordering::Acquire)
        });
        relays.listener.set_nonblocking(true).unwrap();
        // The guest counts only daemon connections, so with one service open
        // it admits all of them; the host must too (it once counted the
        // service and refused the last one with a bare close).
        let mut held = Vec::new();
        for _ in 0..MAX_RELAY_CONNECTIONS {
            let guest = UnixStream::connect(&relays.socket).unwrap();
            let Some(host) = accept_next(&relays.listener) else {
                break;
            };
            held.push((guest, host));
        }
        let admitted = held.len();
        drop(held);
        drop(service);
        relays.stop();
        assert_eq!(
            admitted, MAX_RELAY_CONNECTIONS,
            "the host refused a daemon connection below the cap"
        );
    }

    #[test]
    fn the_guest_says_once_how_full_it_was_when_it_refuses() {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&lines);
        let relays = PairedRelays::logging(Arc::new(move |line| {
            sink.lock().unwrap().push(line.to_owned());
        }));
        let held = fill_to_capacity(&relays);
        for _ in 0..3 {
            let mut excess = UnixStream::connect(&relays.socket).unwrap();
            excess.set_read_timeout(Some(PATIENCE)).unwrap();
            refusal_read(read_frame::<PublicReply>(&mut excess));
        }
        drop(held);
        relays.stop();
        let lines = lines.lock().unwrap().clone();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with("marsh-relay: refused a connection (")
                && lines[0].contains(&format!(
                    "{MAX_RELAY_CONNECTIONS}/{MAX_RELAY_CONNECTIONS} daemon connections open"
                ))
                && !lines[0].contains('\n'),
            "{}",
            lines[0]
        );
    }

    const GUEST_LINE: &str = "marsh-relay: refused a connection (128/128 daemon connections open)";

    #[test]
    fn only_the_guests_refusal_lines_reach_the_daemon_log() {
        let now = Instant::now();
        let mut diagnostics = GuestDiagnostics::default();
        // Split across reads, between other output, with a control character.
        assert_eq!(
            diagnostics.feed(now, b"some other noise\nmarsh-relay: refu"),
            Vec::<String>::new()
        );
        let lines = diagnostics.feed(
            now,
            format!(
                "sed a connection (128/128 daemon connections open)\u{1b}[2J\nmore noise\n{GUEST_LINE}\n"
            )
            .as_bytes(),
        );
        // The escape byte goes; the rest is plain text.
        assert_eq!(lines, [format!("{GUEST_LINE}[2J"), GUEST_LINE.to_owned()]);
        assert!(lines.iter().all(|line| !line.contains('\u{1b}')));
    }

    #[test]
    fn forwarded_guest_text_is_one_line_of_plain_text() {
        let mut diagnostics = GuestDiagnostics::default();
        // Bidi overrides and isolates, line and paragraph separators, a
        // zero-width space and a tag character: none passes.
        let hostile =
            format!("{GUEST_LINE}\u{202e}\u{2066}\u{2028}\u{2029}\u{200b}\u{e0041}\u{85}\r!\n");
        let lines = diagnostics.feed(Instant::now(), hostile.as_bytes());
        assert_eq!(lines, [format!("{GUEST_LINE}!")]);
    }

    #[test]
    fn a_long_guest_line_is_cut_and_an_endless_one_is_not_kept() {
        let mut diagnostics = GuestDiagnostics::default();
        let long = format!("{GUEST_LINE}{}\n", "y".repeat(5_000));
        let lines = diagnostics.feed(Instant::now(), long.as_bytes());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].chars().count(), GuestDiagnostics::MAX_LINE);
        // A refusal line that never ends keeps only a bounded part.
        let mut diagnostics = GuestDiagnostics::default();
        diagnostics.feed(Instant::now(), GUEST_LINE.as_bytes());
        diagnostics.feed(Instant::now(), &vec![b'z'; 100_000]);
        assert!(diagnostics.partial.len() <= GuestDiagnostics::MAX_LINE * 2);
        let lines = diagnostics.feed(Instant::now(), b"\n");
        assert!(lines.len() == 1 && lines[0].chars().count() <= GuestDiagnostics::MAX_LINE);
    }

    #[test]
    fn a_flood_of_guest_refusal_lines_is_limited_per_window_and_counted() {
        let start = Instant::now();
        let mut diagnostics = GuestDiagnostics::default();
        let flood = format!("{GUEST_LINE}\n").repeat(1_000);
        let lines = diagnostics.feed(start, flood.as_bytes());
        assert_eq!(lines.len(), GuestDiagnostics::LINES_PER_WINDOW);
        // Still the same window.
        assert_eq!(
            diagnostics.feed(start + Duration::from_secs(9), flood.as_bytes()),
            Vec::<String>::new()
        );
        // The next window: a few more, the first saying what was left out.
        let lines = diagnostics.feed(start + REFUSAL_LOG_INTERVAL, flood.as_bytes());
        assert_eq!(lines.len(), GuestDiagnostics::LINES_PER_WINDOW);
        assert!(
            lines[0].ends_with(&format!(
                "[{} earlier refusal lines left out]",
                2 * 1_000 - GuestDiagnostics::LINES_PER_WINDOW
            )),
            "{}",
            lines[0]
        );
        assert!(!lines[1].contains("left out"));
    }

    #[test]
    fn a_large_acp_prompt_at_capacity_reads_the_refusal() {
        let relays = PairedRelays::new();
        let held = fill_to_capacity(&relays);
        let reply = client(&relays).acp_prompt_classified(
            "agent".into(),
            session("shell-1", Path::new("/home/u")),
            "op".into(),
            "x".repeat(512 * 1024),
        );
        drop(held);
        relays.stop();
        let message = match reply {
            Err(crate::acp_session::PromptAdmissionError::Rejected(DaemonError::Remote(
                message,
            ))) => message,
            other => panic!("the prompt was answered {other:?}, not a refusal"),
        };
        assert!(message.contains("try again"), "{message}");
    }

    #[test]
    fn what_a_link_retries_covers_every_refusal_that_says_try_again() {
        use marsh_contracts::process::{CONNECTION_REFUSAL, RELAY_UNREACHABLE};
        let remote = |message: String| DaemonError::Remote(message);
        for refusal in [
            remote(busy_message()),
            remote(crate::daemon_busy_message(1024)),
            remote(RELAY_UNREACHABLE.into()),
            remote(CONNECTION_REFUSAL.into()),
            remote(format!(
                "{CONNECTION_REFUSAL} (at most 4 at once: twice job-children, plus 4)"
            )),
            DaemonError::Io(std::io::ErrorKind::BrokenPipe.into()),
        ] {
            assert!(crate::is_retryable_refusal(&refusal), "{refusal:?}");
        }
        for other in [
            remote("marsh: unauthorized".into()),
            remote("spawn refused: shell is not allowed".into()),
            DaemonError::InvalidState("busy".into()),
            DaemonError::JobUncertain("lost".into()),
            DaemonError::Io(std::io::ErrorKind::NotFound.into()),
        ] {
            assert!(!crate::is_retryable_refusal(&other), "{other:?}");
        }
    }

    #[test]
    fn a_process_run_refused_by_the_relay_is_not_admitted_and_retryable() {
        let run = |client: &Client, bytes: usize| {
            client.run_process_with_io_attempt(
                ExecuteSpec {
                    model: None,
                    usage: None,
                    label: None,
                    kit: None,
                    command: "fixture".into(),
                    arguments: vec![vec![b'x'; bytes]],
                    placement: crate::Placement::Local,
                    environment: marsh_contracts::ExportedEnvironment::default(),
                    working_directory: None,
                    session: session("shell-1", Path::new("/home/u")),
                    process: None,
                    acp: false,
                },
                std::io::empty(),
                std::io::sink(),
                std::io::sink(),
                &mut |_| panic!("a refused run started"),
            )
        };
        let relays = PairedRelays::new();
        let held = fill_to_capacity(&relays);
        let client = client(&relays);
        // Small, and larger than the socket buffers (written while the relay
        // refuses and closes): both are refused before admission.
        let capped = [run(&client, 10), run(&client, 150_000)];
        drop(held);
        fs::remove_file(
            relays
                .socket
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("daemon.sock"),
        )
        .unwrap();
        let unreachable = run(&client, 10);
        relays.stop();
        for result in capped.into_iter().chain([unreachable]) {
            match result {
                Err(error @ crate::ProcessRunError::NotAdmitted(_)) => {
                    assert!(error.is_retryable(), "{error:?}");
                }
                other => panic!("expected a refusal before admission, got {other:?}"),
            }
        }
    }

    #[test]
    fn every_relay_refusal_a_real_client_reads_is_retryable() {
        let relays = PairedRelays::new();
        let held = fill_to_capacity(&relays);
        let client = client(&relays);
        let capped = [
            client.request(PublicRequest::Ping),
            client.request(PublicRequest::OpenUrl {
                url: "x".repeat(512 * 1024),
            }),
        ];
        drop(held);
        fs::remove_file(
            relays
                .socket
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("daemon.sock"),
        )
        .unwrap();
        let unreachable = client.request(PublicRequest::Ping);
        relays.stop();
        for reply in capped.into_iter().chain([unreachable]) {
            // A refusal read as the reply, or carried by the failed write.
            let error = match reply {
                Ok(reply) => crate::unexpected_reply(reply),
                Err(error) => error,
            };
            assert!(crate::is_retryable_refusal(&error), "{error:?}");
        }
    }

    #[test]
    fn refusal_lines_are_rate_limited_and_count_what_they_skip() {
        let start = Instant::now();
        let mut log = RefusalLog::default();
        let first = log.note(start, "who", "128/128 open").unwrap();
        assert_eq!(first, "who: refused a connection (128/128 open)");
        assert!(
            log.note(start + Duration::from_secs(1), "who", "x")
                .is_none()
        );
        assert!(
            log.note(start + Duration::from_secs(9), "who", "x")
                .is_none()
        );
        let next = log
            .note(start + REFUSAL_LOG_INTERVAL, "who", "128/128 open")
            .unwrap();
        assert_eq!(
            next,
            "who: refused a connection (128/128 open); 2 more refused since the last line"
        );
        assert!(
            !log.note(start + REFUSAL_LOG_INTERVAL * 2, "who", "x")
                .unwrap()
                .contains("more refused")
        );
    }

    #[test]
    fn slow_client_preserves_large_output_while_a_sibling_completes() {
        let relays = PairedRelays::new();
        let (mut slow_client, mut producer) = relays.connect();
        slow_client.shutdown(Shutdown::Write).unwrap();
        let payload: Vec<u8> = (0_u8..=250).cycle().take(4 * 1024 * 1024).collect();
        let expected = payload.clone();
        let (completed, received) = mpsc::channel();
        let writer = thread::spawn(move || {
            let result = producer
                .write_all(&payload)
                .and_then(|()| producer.shutdown(Shutdown::Write));
            completed.send(result).unwrap();
        });
        // Leave the recipient paused long enough to fill the bounded window.
        // A healthy pause must not turn into a truncated stream or disconnect.
        thread::sleep(Duration::from_millis(300));
        let early = received.try_recv();
        let was_waiting = matches!(early, Err(mpsc::TryRecvError::Empty));
        let (mut sibling, mut server) = relays.connect();
        sibling.write_all(b"request").unwrap();
        sibling.shutdown(Shutdown::Write).unwrap();
        let sibling_request = read_to_end(&mut server);
        let sibling_reply = server
            .write_all(b"response")
            .and_then(|()| server.shutdown(Shutdown::Write));
        let sibling_read = read_to_end(&mut sibling);
        let slow_read = read_to_end(&mut slow_client);
        let written = early.unwrap_or_else(|_| recv(&received, "producer finishes"));
        // Release transport and every blocked credit/socket waiter before
        // asserting so a regression cannot strand either relay in the test.
        relays.stop();
        writer.join().unwrap();
        assert!(
            was_waiting,
            "slow recipient did not backpressure its producer"
        );
        assert_eq!(sibling_request.unwrap(), b"request");
        sibling_reply.unwrap();
        assert_eq!(sibling_read.unwrap(), b"response");
        written.unwrap();
        assert_eq!(slow_read.unwrap(), expected);
    }

    #[test]
    fn shutdown_releases_a_source_waiting_for_remote_credit() {
        let relays = PairedRelays::new();
        let (_paused_client, mut producer) = relays.connect();
        let (completed, received) = mpsc::channel();
        let writer = thread::spawn(move || {
            completed
                .send(producer.write_all(&vec![b'x'; 4 * 1024 * 1024]))
                .unwrap();
        });
        thread::sleep(Duration::from_millis(300));
        let early = received.try_recv();
        let was_waiting = matches!(early, Err(mpsc::TryRecvError::Empty));
        relays.stop();
        let result = early.unwrap_or_else(|_| recv(&received, "producer wakes"));
        writer.join().unwrap();
        assert!(was_waiting, "producer did not wait for remote credit");
        assert!(result.is_err(), "teardown did not wake the blocked source");
    }

    /// The connection is closed here while its writer is blocked on a peer
    /// that does not read, with chunks queued behind it. The writer then
    /// fails, and must not return credit for the chunks it throws away: that
    /// would let the far side's source (the daemon's producer) run on, its
    /// data discarded and its writes succeeding, until the far relay is torn
    /// down too.
    #[test]
    fn a_connection_closed_here_gives_no_credit_for_what_it_throws_away() {
        let connections: Connections = Arc::default();
        let (stream, _unread) = socket_pair();
        let tunnel = Arc::new(Mutex::new(Vec::new()));
        let _reader = register_connection(1, stream, &connections, &tunnel).unwrap();
        let chunk = vec![b'x'; CHUNK_BYTES];
        for _ in 0..MAX_PENDING_CHUNKS {
            forward_data(&connections, &tunnel, 1, chunk.clone()).unwrap();
        }
        let credits = || {
            let sent = tunnel.lock().unwrap().clone();
            let mut sent = Cursor::new(sent);
            let mut credits = 0;
            while let Ok(frame) = read_frame::<TunnelFrame>(&mut sent) {
                credits += usize::from(matches!(frame, TunnelFrame::Credit { .. }));
            }
            credits
        };
        let writer = connections.lock().unwrap().get(&1).unwrap().writer.clone();
        close_all_connections(&connections);
        // Barrier: the writer has returned. A `Finish` after the queued chunks
        // is harmless to it (the connection is gone), and once the writer has
        // dropped its receiver a send fails: so by then it has either returned
        // at its first failed write or gone through every queued chunk, and the
        // credits below are all it will ever send.
        wait_until("the connection's writer has returned", || {
            matches!(
                writer.try_send(ConnectionWrite::Finish),
                Err(mpsc::TrySendError::Disconnected(_))
            )
        });
        // What the peer's buffers took before it was cut, at most: far fewer
        // than the 64 chunks queued, whose credit is what must not be sent.
        assert!(
            credits() < MAX_PENDING_CHUNKS / 2,
            "{} credits for {MAX_PENDING_CHUNKS} queued chunks",
            credits()
        );
    }

    #[test]
    fn half_closed_guest_client_receives_its_response() {
        let base = tempfile::tempdir().unwrap();
        fs::set_permissions(base.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = base.path().join("session");
        let socket = runtime.join("socket");
        let token = runtime.join("token");
        let (guest, mut host) = socket_pair();
        host.set_read_timeout(Some(PATIENCE)).unwrap();
        let guest_reader = guest.try_clone().unwrap();
        let relay_socket = socket.clone();
        let relay = thread::spawn(move || run_guest(guest_reader, guest, &relay_socket, &token));
        write_frame(
            &mut host,
            &TunnelFrame::Initialize {
                token: "a".repeat(64),
                breaks: Vec::new(),
            },
        )
        .unwrap();
        let first = read_frame::<TunnelFrame>(&mut host).unwrap();
        let ready = if matches!(first, TunnelFrame::Identity { .. }) {
            read_frame::<TunnelFrame>(&mut host).unwrap()
        } else {
            first
        };
        assert_eq!(ready, TunnelFrame::Ready);
        let mut client = UnixStream::connect(&socket).unwrap();
        client.set_read_timeout(Some(PATIENCE)).unwrap();
        let TunnelFrame::Open { connection } = read_frame(&mut host).unwrap() else {
            panic!("missing client open")
        };
        client.write_all(b"request").unwrap();
        client.shutdown(Shutdown::Write).unwrap();
        assert_eq!(
            read_frame::<TunnelFrame>(&mut host).unwrap(),
            TunnelFrame::Data {
                connection,
                bytes: b"request".to_vec()
            }
        );
        assert_eq!(
            read_frame::<TunnelFrame>(&mut host).unwrap(),
            TunnelFrame::Eof { connection }
        );
        write_frame(
            &mut host,
            &TunnelFrame::Data {
                connection,
                bytes: b"response".to_vec(),
            },
        )
        .unwrap();
        write_frame(&mut host, &TunnelFrame::Eof { connection }).unwrap();
        let received = read_to_end(&mut client);
        write_frame(&mut host, &TunnelFrame::Shutdown).unwrap();
        relay.join().unwrap().unwrap();
        assert_eq!(received.unwrap(), b"response");
    }

    #[test]
    fn guest_runtime_is_an_atomic_owner_only_session_child() {
        let base = tempfile::tempdir().unwrap();
        fs::set_permissions(base.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = base.path().join("session");
        let socket = runtime.join("s");
        let token = runtime.join("t");

        prepare_guest_runtime(&socket, &token, &"a".repeat(64)).unwrap();
        let runtime_metadata = fs::symlink_metadata(&runtime).unwrap();
        assert!(runtime_metadata.is_dir());
        assert_eq!(runtime_metadata.mode() & 0o777, 0o700);
        let token_metadata = fs::symlink_metadata(&token).unwrap();
        assert!(token_metadata.is_file());
        assert_eq!(token_metadata.mode() & 0o777, 0o600);

        assert!(matches!(
            prepare_guest_runtime(&socket, &token, &"b".repeat(64)),
            Err(DaemonError::EndpointExists(path)) if path == runtime
        ));
    }

    #[test]
    fn guest_runtime_guard_removes_the_empty_session_directory() {
        let base = tempfile::tempdir().unwrap();
        fs::set_permissions(base.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = base.path().join("session");
        let socket = runtime.join("s");
        let token = runtime.join("t");

        prepare_guest_runtime(&socket, &token, &"a".repeat(64)).unwrap();
        fs::write(&socket, b"placeholder").unwrap();
        drop(GuestRuntime::new(&socket, &token, None).unwrap());

        assert!(!runtime.exists());
    }

    #[test]
    fn completed_relay_readers_preserve_the_response_direction_until_remote_eof() {
        let connections: Connections = Arc::default();
        for connection in 1..=64 {
            let (stream, peer) = socket_pair();
            let tunnel = Arc::new(Mutex::new(Vec::new()));
            let reader = register_connection(connection, stream, &connections, &tunnel).unwrap();
            let task = spawn_reader(
                connection,
                reader,
                Arc::clone(&connections),
                Arc::clone(&tunnel),
            );
            peer.shutdown(Shutdown::Write).unwrap();
            wait_until("the reader sees the peer's EOF", || task.is_finished());
            task.join().unwrap();
            assert_eq!(connections.lock().unwrap().len(), 1);

            let mut frame = Cursor::new(tunnel.lock().unwrap().clone());
            assert_eq!(
                read_frame::<TunnelFrame>(&mut frame).unwrap(),
                TunnelFrame::Eof { connection }
            );
            close_all_connections(&connections);
        }
    }

    #[test]
    fn failed_open_frame_releases_the_unannounced_guest_connection() {
        let connections: Connections = Arc::default();
        let (stream, mut peer) = socket_pair();
        let tunnel = Arc::new(Mutex::new(RejectWrites));

        assert!(register_guest_connection(1, stream, &connections, &tunnel).is_err());
        assert!(connections.lock().unwrap().is_empty());
        let mut byte = [0_u8; 1];
        assert_eq!(peer.read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn failed_data_frame_releases_the_tracked_connection() {
        let connections: Connections = Arc::default();
        let (stream, mut peer) = socket_pair();
        let tunnel = Arc::new(Mutex::new(RejectWrites));
        let reader = register_connection(1, stream, &connections, &tunnel).unwrap();
        let task = spawn_reader(1, reader, Arc::clone(&connections), tunnel);

        peer.write_all(b"data").unwrap();
        task.join().unwrap();
        assert!(connections.lock().unwrap().is_empty());
        // Portable close observation (macOS may accept writes after SHUT_RD).
        let _ = peer.set_read_timeout(Some(PATIENCE));
        assert_eq!(peer.read(&mut [0_u8; 1]).unwrap(), 0);
    }

    #[test]
    fn readiness_reports_handshake_failure_instead_of_leaving_waiter_blocked() {
        let (send, receive) = mpsc::channel();
        let result = run_host_with_ready(
            Cursor::new(Vec::<u8>::new()),
            Vec::<u8>::new(),
            Path::new("/unused"),
            "a".repeat(64),
            move |ready| send.send(ready).unwrap(),
        );

        assert!(result.is_err());
        assert!(receive.try_recv().unwrap().is_err());
    }

    #[derive(Debug)]
    struct RelayBackend;

    impl DaemonBackend for RelayBackend {
        fn registered_commands(&self) -> Result<Vec<String>, DaemonError> {
            Ok(vec!["fixture".into()])
        }

        fn prepare(
            &self,
            _selection: &LoadSelection,
            _session: &SessionSpec,
            _progress: PreparationProgress,
            _store: DaemonStore,
        ) -> Result<PreparationResult, DaemonError> {
            Ok(PreparationResult::default())
        }

        fn execute(
            &self,
            _request: ExecuteSpec,
            attachment: ServerAttachment,
            _store: DaemonStore,
        ) -> Result<(), DaemonError> {
            loop {
                match attachment.receive()? {
                    AttachmentFrame::Stdin { bytes } => {
                        attachment.send(&AttachmentFrame::Stdout { bytes })?;
                    }
                    AttachmentFrame::StdinEof => {
                        attachment.send(&AttachmentFrame::Exited { code: 23 })?;
                        return Ok(());
                    }
                    AttachmentFrame::Resize { .. } => {}
                    AttachmentFrame::Signal { signal } if signal == "interrupt" => {
                        attachment.send(&AttachmentFrame::Exited { code: 130 })?;
                        return Ok(());
                    }
                    frame => {
                        return Err(DaemonError::InvalidState(format!(
                            "unexpected attachment frame: {frame:?}"
                        )));
                    }
                }
            }
        }

        fn open_shell(
            &self,
            _request: ShellSpec,
            attachment: ServerAttachment,
            _store: DaemonStore,
        ) -> Result<(), DaemonError> {
            attachment.send(&AttachmentFrame::ShellReady)?;
            attachment.send(&AttachmentFrame::Stdout {
                bytes: b"\x1b[32mguest shell\x1b[0m".to_vec(),
            })?;
            attachment.send(&AttachmentFrame::Exited { code: 0 })
        }
    }

    fn session(session_id: &str, home: &Path) -> SessionSpec {
        SessionSpec {
            session_id: session_id.into(),
            username: "example".into(),
            uid: 1000,
            gid: 1000,
            launch_directory: "/Users/example/project".into(),
            guest_home: "/Users/example".into(),
            home_backing: home.into(),
            ephemeral_home: false,
            terminal: false,
            terminal_size: None,
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn concurrent_guest_connections_reach_one_host_daemon() {
        let home = tempfile::tempdir().unwrap();
        let server = Arc::new(
            Server::bind(home.path())
                .unwrap()
                .with_backend(Arc::new(RelayBackend)),
        );
        let session_id = server.store().attach_shell(
            std::process::id(),
            crate::SessionAuthority {
                username: "example".into(),
                uid: 1000,
                gid: 1000,
                launch_directory: PathBuf::from("/Users/example/project"),
                guest_home: "/Users/example".into(),
                home_backing: home.path().into(),
                ephemeral_home: false,
            },
        );
        let token = server.store().issue_relay_token(&session_id).unwrap();
        let daemon_socket = crate::EndpointPaths::for_home(home.path()).unwrap().socket;
        let stop = Arc::new(AtomicBool::new(false));
        let server_task = {
            let server = Arc::clone(&server);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                server.serve_until(|| stop.load(Ordering::Relaxed)).unwrap();
            })
        };

        let runtime_base = home.path().join("guest-runtime");
        fs::create_dir(&runtime_base).unwrap();
        fs::set_permissions(&runtime_base, fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = runtime_base.join("session");
        let socket = runtime.join("daemon.sock");
        let token_path = runtime.join("daemon.token");
        let (host_tunnel, guest_tunnel) = socket_pair();
        let host_reader = host_tunnel.try_clone().unwrap();
        let host_tunnel = Arc::new(Mutex::new(host_tunnel));
        let cleaned = Arc::new(AtomicBool::new(false));
        let (ready_send, ready_receive) = mpsc::channel();
        let host_task = {
            let tunnel = Arc::clone(&host_tunnel);
            let cleaned = Arc::clone(&cleaned);
            thread::spawn(move || {
                run_host_shared(
                    host_reader,
                    &tunnel,
                    &daemon_socket,
                    token,
                    move |ready| {
                        ready_send.send(ready.map(|_| ())).unwrap();
                    },
                    &cleaned,
                )
                .unwrap();
            })
        };
        let guest_task = {
            let reader = guest_tunnel.try_clone().unwrap();
            let socket = socket.clone();
            let token_path = token_path.clone();
            thread::spawn(move || {
                run_guest(reader, guest_tunnel, &socket, &token_path).unwrap();
            })
        };

        recv(&ready_receive, "relay readiness").unwrap();
        wait_until("the guest relay socket appears", || socket.exists());
        // Exercise the same sweep that runs every five seconds in serve_until.
        // A fabricated client PID lets that sweep revoke our relay mid-test.
        crate::release_departed(&server.store(), server.backend.as_ref(), &server.acp);
        let client = Client::connect_relay(&socket, &token_path).unwrap();
        let daemon_id = server.store().daemon_id();
        let build_identity = server.build_identity.clone();
        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let client = client.clone();
                let daemon_id = daemon_id.clone();
                let build_identity = build_identity.clone();
                thread::spawn(move || {
                    assert_eq!(
                        client.request(PublicRequest::Ping).unwrap(),
                        PublicReply::Pong {
                            daemon_id,
                            build_identity: Some(build_identity),
                            pid: Some(std::process::id()),
                            update_identity: None,
                            version: Some(env!("CARGO_PKG_VERSION").into()),
                        }
                    );
                })
            })
            .collect();
        for task in tasks {
            task.join().unwrap();
        }

        // More than one terminal's usual 256-FD budget worth of cloned relay
        // endpoints would accumulate here if Close only dropped the map entry
        // while its reader clone remained blocked.
        for _ in 0..64 {
            let mut stdout = Vec::new();
            assert_eq!(
                client
                    .execute_with_io(
                        ExecuteSpec {
                            model: None,
                            usage: None,
                            label: None,
                            kit: None,
                            acp: false,
                            command: "fixture".into(),
                            placement: crate::Placement::Local,
                            arguments: Vec::new(),
                            environment: std::collections::BTreeMap::new(),
                            working_directory: None,
                            session: session(&session_id, home.path()),
                            process: None,
                        },
                        std::io::Cursor::new(b"raw\x00\xff".to_vec()),
                        &mut stdout,
                        Vec::new(),
                    )
                    .unwrap(),
                23
            );
            assert_eq!(stdout, b"raw\x00\xff");
        }
        let mut shell_output = Vec::new();
        assert_eq!(
            client
                .open_shell_with_io(
                    ShellSpec {
                        dev: false,
                        arguments: Vec::new(),
                        session: session(&session_id, home.path()),
                        terminal_environment: std::collections::BTreeMap::new(),
                        working_directory: None,
                    },
                    std::fs::File::open("/dev/null").unwrap(),
                    &mut shell_output,
                    Vec::new(),
                )
                .unwrap(),
            0
        );
        assert_eq!(shell_output, b"\x1b[32mguest shell\x1b[0m");

        // Stop in band through the relay's own writer lock, as the backend
        // does, so no frame interleaves with in-flight credits. A socket
        // shutdown would rely on macOS waking both blocked tunnel reads,
        // which it sometimes never does (see `READ_WAKE`).
        request_guest_shutdown(&host_tunnel).unwrap();
        wait_until("the guest relay stops", || guest_task.is_finished());
        guest_task.join().unwrap();
        wait_until("the host relay stops", || host_task.is_finished());
        host_task.join().unwrap();
        assert!(cleaned.load(Ordering::Acquire));
        stop.store(true, Ordering::Relaxed);
        server_task.join().unwrap();
        assert!(!socket.exists());
        assert!(!token_path.exists());
        assert!(!runtime.exists());
    }
}
