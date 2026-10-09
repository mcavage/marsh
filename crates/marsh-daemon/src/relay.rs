//! Transparent guest-shell transport over one long-lived stock-SBX exec.
//!
//! The guest side owns an owner-only Unix socket and token under `/run`. Each
//! guest connection is multiplexed over the trusted relay process's stdio;
//! the host side opens a matching connection to the macOS daemon. Job
//! containers receive neither path nor token.

use crate::relay_cleanup::{RelayIdentity, current_identity};
use crate::{DaemonError, read_frame, write_frame};
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
    time::Duration,
};

const CHUNK_BYTES: usize = 16 * 1024;
// Each connection retains three local socket descriptors; the host daemon
// needs additional descriptors for its end. Leave room under macOS's usual
// 256-descriptor soft limit for the tunnel, listener, and active shell jobs.
const MAX_RELAY_CONNECTIONS: usize = 16;
// At most 1 MiB per connection, including an in-progress write. A stalled
// recipient must not hold the tunnel reader or the shared connection map.
const MAX_PENDING_CHUNKS: usize = 64;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum TunnelFrame {
    Initialize { token: String },
    Identity { identity: RelayIdentity },
    Ready,
    Open { connection: u64 },
    Data { connection: u64, bytes: Vec<u8> },
    Credit { connection: u64 },
    // Graceful half-close: deliver preceding data, then close only writes.
    Eof { connection: u64 },
    // Graceful full close: deliver preceding data, then close both directions.
    Finished { connection: u64 },
    // Abort: interrupt outstanding writes and discard queued data immediately.
    Close { connection: u64 },
    Shutdown,
    // Guest's final frame: listener closed, socket and token removed.
    Cleaned,
}

type SharedWriter<W> = Arc<Mutex<W>>;
type Connections = Arc<Mutex<BTreeMap<u64, Connection>>>;

struct Connection {
    stream: UnixStream,
    writer: mpsc::SyncSender<ConnectionWrite>,
    pending_chunks: Arc<AtomicUsize>,
    send_window: Arc<SendWindow>,
    identity: Arc<()>,
    read_closed: bool,
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
    mut from_guest: R,
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
    let tunnel = Arc::clone(tunnel);
    if let Err(error) = send(
        &tunnel,
        &TunnelFrame::Initialize {
            token: session_token,
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
    let outcome = loop {
        match read_frame::<TunnelFrame>(&mut from_guest) {
            Ok(TunnelFrame::Open { connection }) => {
                if connections
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .len()
                    >= MAX_RELAY_CONNECTIONS
                {
                    if let Err(error) = send(&tunnel, &TunnelFrame::Close { connection }) {
                        break Err(error);
                    }
                    continue;
                }
                let Ok(stream) = UnixStream::connect(daemon_socket) else {
                    if let Err(error) = send(&tunnel, &TunnelFrame::Close { connection }) {
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

/// Guest half used by the installed `marsh-relay` helper.
pub fn run_guest<R, W>(
    mut from_host: R,
    to_host: W,
    socket_path: &Path,
    token_path: &Path,
) -> Result<(), DaemonError>
where
    R: Read,
    W: Write + Send + 'static,
{
    let TunnelFrame::Initialize { token } = read_frame(&mut from_host)? else {
        return Err(DaemonError::InvalidState(
            "host relay omitted initialization".into(),
        ));
    };
    prepare_guest_runtime(socket_path, token_path, &token)?;
    let runtime = GuestRuntime::new(socket_path, token_path)?;
    let listener = UnixListener::bind(socket_path)?;
    fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let tunnel = Arc::new(Mutex::new(to_host));
    send_guest_ready(&tunnel)?;
    let connections: Connections = Arc::default();
    let accept_connections = Arc::clone(&connections);
    let accept_tunnel = Arc::clone(&tunnel);
    let stopping = Arc::new(AtomicBool::new(false));
    let accept_stopping = Arc::clone(&stopping);
    let acceptor = thread::spawn(move || {
        let mut next = 1_u64;
        while !accept_stopping.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    if accept_connections
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .len()
                        >= MAX_RELAY_CONNECTIONS
                    {
                        let _ = stream.shutdown(Shutdown::Both);
                        continue;
                    }
                    if stream.set_nonblocking(false).is_err() {
                        break;
                    }
                    let connection = next;
                    next = next.saturating_add(1);
                    let Ok(reader) = register_guest_connection(
                        connection,
                        stream,
                        &accept_connections,
                        &accept_tunnel,
                    ) else {
                        break;
                    };
                    spawn_reader(
                        connection,
                        reader,
                        Arc::clone(&accept_connections),
                        Arc::clone(&accept_tunnel),
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
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
    let _ = acceptor.join();
    close_all_connections(&connections);
    report_guest_cleaned(&tunnel, runtime, outcome.is_ok());
    outcome
}

/// In-band cleanup report: the host treats this final frame plus a successful
/// exit of this exec as relay cleanup proof (no separate verify exec).
fn report_guest_cleaned<W: Write>(tunnel: &SharedWriter<W>, runtime: GuestRuntime, ok: bool) {
    let (socket, token) = (runtime.socket.clone(), runtime.token.clone());
    drop(runtime);
    let gone = |path: &Path| {
        fs::symlink_metadata(path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    };
    if ok && gone(&socket) && gone(&token) {
        let _ = send(tunnel, &TunnelFrame::Cleaned);
    }
}

struct GuestRuntime {
    socket: PathBuf,
    token: PathBuf,
    directory: PathBuf,
}

impl GuestRuntime {
    fn new(socket: &Path, token: &Path) -> Result<Self, DaemonError> {
        let directory = parent(socket)?;
        if directory != parent(token)? {
            return Err(DaemonError::InvalidState(
                "relay socket and token must share a private directory".into(),
            ));
        }
        Ok(Self {
            socket: socket.to_owned(),
            token: token.to_owned(),
            directory: directory.to_owned(),
        })
    }
}

impl Drop for GuestRuntime {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
        let _ = fs::remove_file(&self.token);
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
            match source.stream.read(&mut buffer) {
                Ok(0) => {
                    // A zero-byte socket write changes no protocol bytes and
                    // distinguishes a peer that closed both directions from
                    // one that only shut down its sending direction. Retaining
                    // the former would let expired daemon handshakes consume
                    // every relay slot even after their sockets disappeared.
                    let peer_closed = source.stream.write(&[]).is_err_and(|error| {
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

fn register_connection<W: Write + Send + 'static>(
    connection: u64,
    mut stream: UnixStream,
    connections: &Connections,
    tunnel: &SharedWriter<W>,
) -> Result<ConnectionReader, DaemonError> {
    let identity = Arc::new(());
    let send_window = Arc::new(SendWindow::new());
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
            stream: shutdown,
            writer,
            pending_chunks: Arc::clone(&pending_chunks),
            send_window,
            identity: Arc::clone(&identity),
            read_closed: false,
            end: ConnectionEnd::Open,
        },
    );
    drop(guard);
    let connections = Arc::clone(connections);
    let tunnel = Arc::clone(tunnel);
    thread::spawn(move || {
        while let Ok(message) = pending.recv() {
            match message {
                ConnectionWrite::Data(bytes) => {
                    let result = stream.write_all(&bytes);
                    pending_chunks.fetch_sub(1, Ordering::AcqRel);
                    if result.is_err()
                        || send(&tunnel, &TunnelFrame::Credit { connection }).is_err()
                    {
                        break;
                    }
                }
                ConnectionWrite::Eof => {
                    if stream.shutdown(Shutdown::Write).is_err() {
                        break;
                    }
                    if writer_eof(&connections, connection, &identity) {
                        return;
                    }
                }
                ConnectionWrite::Finish => {
                    finish_connection(&connections, connection, &identity);
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
        handle: thread::JoinHandle<Result<(), DaemonError>>,
        limit: Duration,
    ) -> Result<(), DaemonError> {
        let deadline = Instant::now() + limit;
        while !handle.is_finished() {
            assert!(
                Instant::now() < deadline,
                "host relay did not stop within {limit:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
        handle.join().unwrap()
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
        let mut peers = accepted_receive
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
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
        let (host, mut guest) = UnixStream::pair().unwrap();
        guest
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
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
        accepted_receive
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            read_frame::<TunnelFrame>(&mut guest).unwrap(),
            TunnelFrame::Close {
                connection: MAX_RELAY_CONNECTIONS as u64 + 1
            }
        );
        // The host waits for the guest's in-band Cleaned (or EOF) after Shutdown.
        write_frame(&mut guest, &TunnelFrame::Shutdown).unwrap();
        write_frame(&mut guest, &TunnelFrame::Cleaned).unwrap();
        release_send.send(()).unwrap();
        join_within(relay, Duration::from_secs(10)).unwrap();
        acceptor.join().unwrap();
    }

    #[test]
    fn stalled_connection_does_not_block_a_sibling_or_relay_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (host, mut guest) = UnixStream::pair().unwrap();
        guest
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        guest
            .set_write_timeout(Some(Duration::from_secs(30)))
            .unwrap();
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
        sibling
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
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
        join_within(relay, Duration::from_secs(30)).unwrap();
        sibling_result.unwrap();
        assert_eq!(&actual, b"sibling");
    }

    #[test]
    fn explicit_close_aborts_an_unread_sink_and_allows_connection_reuse() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (host, mut guest) = UnixStream::pair().unwrap();
        guest
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        guest
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let control = guest.try_clone().unwrap();
        let host_reader = host.try_clone().unwrap();
        let relay = thread::spawn(move || run_host(host_reader, host, &socket, "a".repeat(64)));
        assert!(matches!(
            read_frame::<TunnelFrame>(&mut guest).unwrap(),
            TunnelFrame::Initialize { .. }
        ));
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
        replacement
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        // Portable disconnect proof: the abandoned peer reaches EOF before the
        // queued megabyte arrives. (macOS accepts writes after a peer's
        // SHUT_RD, so a failed write is not a portable observation.)
        // macOS rejects setsockopt (EINVAL) once the peer has fully closed.
        let _ = stalled.set_read_timeout(Some(Duration::from_secs(3)));
        let mut abandoned_bytes = Vec::new();
        let abandoned = stalled.read_to_end(&mut abandoned_bytes);
        write_frame(
            &mut guest,
            &TunnelFrame::Data {
                connection: 1,
                bytes: b"replacement".to_vec(),
            },
        )
        .unwrap();
        write_frame(&mut guest, &TunnelFrame::Eof { connection: 1 }).unwrap();
        let mut received = Vec::new();
        let delivered = replacement.read_to_end(&mut received);
        // Always release blocked writers before assertions, including with
        // the previous implementation that queued Close behind the data.
        control.shutdown(Shutdown::Both).unwrap();
        relay.join().unwrap().unwrap();
        assert!(
            abandoned.is_ok() && abandoned_bytes.len() < MAX_PENDING_CHUNKS * CHUNK_BYTES,
            "Close left the abandoned socket connected or delivered all queued data"
        );
        delivered.unwrap();
        assert_eq!(received, b"replacement");
    }

    #[test]
    fn daemon_full_close_releases_slots_without_waiting_for_guest_eof() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (host, mut guest) = UnixStream::pair().unwrap();
        guest
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let control = guest.try_clone().unwrap();
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
            let deadline = Instant::now() + Duration::from_millis(300);
            let peer = loop {
                match listener.accept() {
                    Ok((peer, _)) => break Some(peer),
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(1));
                    }
                    _ => break None,
                }
            };
            let Some(peer) = peer else { break };
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
        control.shutdown(Shutdown::Both).unwrap();
        relay.join().unwrap().unwrap();
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
        control: UnixStream,
        host: thread::JoinHandle<Result<(), DaemonError>>,
        guest: thread::JoinHandle<Result<(), DaemonError>>,
    }

    impl PairedRelays {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let daemon_socket = root.path().join("daemon.sock");
            let listener = UnixListener::bind(&daemon_socket).unwrap();
            let socket = root.path().join("session/socket");
            let token = root.path().join("session/token");
            let (host_stream, guest_stream) = UnixStream::pair().unwrap();
            let control = host_stream.try_clone().unwrap();
            let host_reader = host_stream.try_clone().unwrap();
            let guest_reader = guest_stream.try_clone().unwrap();
            let (ready, received) = mpsc::channel();
            let host = thread::spawn(move || {
                run_host_with_ready(
                    host_reader,
                    host_stream,
                    &daemon_socket,
                    "a".repeat(64),
                    |result| {
                        ready.send(result).unwrap();
                    },
                )
            });
            let guest_socket = socket.clone();
            let guest =
                thread::spawn(move || run_guest(guest_reader, guest_stream, &guest_socket, &token));
            received
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            Self {
                _root: root,
                socket,
                listener,
                control,
                host,
                guest,
            }
        }

        fn connect(&self) -> (UnixStream, UnixStream) {
            let guest = UnixStream::connect(&self.socket).unwrap();
            let host = self.listener.accept().unwrap().0;
            for stream in [&guest, &host] {
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
            }
            (guest, host)
        }

        fn stop(self) {
            self.control.shutdown(Shutdown::Both).unwrap();
            self.host.join().unwrap().unwrap();
            self.guest.join().unwrap().unwrap();
            assert!(!self.socket.exists());
        }
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
            guest
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let deadline = Instant::now() + Duration::from_millis(300);
            let peer = loop {
                match relays.listener.accept() {
                    Ok((peer, _)) => break Some(peer),
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(1));
                    }
                    _ => break None,
                }
            };
            let Some(mut peer) = peer else { break };
            peer.set_nonblocking(false).unwrap();
            peer.set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            peer.write_all(&expected).unwrap();
            drop(peer);
            // Let full-close reach the guest while its final bytes remain
            // queued behind an intentionally unread socket buffer.
            thread::sleep(Duration::from_millis(25));
            let mut response = Vec::new();
            if guest.read_to_end(&mut response).is_err() || response != expected {
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
        let mut request = Vec::new();
        let sibling_request = server.read_to_end(&mut request);
        let sibling_reply = server
            .write_all(b"response")
            .and_then(|()| server.shutdown(Shutdown::Write));
        let mut reply = Vec::new();
        let sibling_read = sibling.read_to_end(&mut reply);
        let mut actual = Vec::new();
        let slow_read = slow_client.read_to_end(&mut actual);
        let written = match early {
            Ok(result) => Ok(result),
            Err(_) => received.recv_timeout(Duration::from_secs(5)),
        };
        // Release transport and every blocked credit/socket waiter before
        // asserting so a regression cannot strand either relay in the test.
        relays.stop();
        writer.join().unwrap();
        assert!(
            was_waiting,
            "slow recipient did not backpressure its producer"
        );
        sibling_request.unwrap();
        sibling_reply.unwrap();
        sibling_read.unwrap();
        assert_eq!(request, b"request");
        assert_eq!(reply, b"response");
        written.unwrap().unwrap();
        slow_read.unwrap();
        assert_eq!(actual, expected);
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
        let result = match early {
            Ok(result) => Ok(result),
            Err(_) => received.recv_timeout(Duration::from_secs(2)),
        };
        writer.join().unwrap();
        assert!(was_waiting, "producer did not wait for remote credit");
        assert!(
            result.unwrap().is_err(),
            "teardown did not wake the blocked source"
        );
    }

    #[test]
    fn half_closed_guest_client_receives_its_response() {
        let base = tempfile::tempdir().unwrap();
        fs::set_permissions(base.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = base.path().join("session");
        let socket = runtime.join("socket");
        let token = runtime.join("token");
        let (guest, mut host) = UnixStream::pair().unwrap();
        host.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let guest_reader = guest.try_clone().unwrap();
        let relay_socket = socket.clone();
        let relay = thread::spawn(move || run_guest(guest_reader, guest, &relay_socket, &token));
        write_frame(
            &mut host,
            &TunnelFrame::Initialize {
                token: "a".repeat(64),
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
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
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
        let mut response = Vec::new();
        let received = client.read_to_end(&mut response);
        write_frame(&mut host, &TunnelFrame::Shutdown).unwrap();
        relay.join().unwrap().unwrap();
        received.unwrap();
        assert_eq!(response, b"response");
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
        drop(GuestRuntime::new(&socket, &token).unwrap());

        assert!(!runtime.exists());
    }

    #[test]
    fn completed_relay_readers_preserve_the_response_direction_until_remote_eof() {
        let connections: Connections = Arc::default();
        for connection in 1..=64 {
            let (stream, peer) = UnixStream::pair().unwrap();
            let tunnel = Arc::new(Mutex::new(Vec::new()));
            let reader = register_connection(connection, stream, &connections, &tunnel).unwrap();
            let task = spawn_reader(
                connection,
                reader,
                Arc::clone(&connections),
                Arc::clone(&tunnel),
            );
            peer.shutdown(Shutdown::Write).unwrap();
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
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let tunnel = Arc::new(Mutex::new(RejectWrites));

        assert!(register_guest_connection(1, stream, &connections, &tunnel).is_err());
        assert!(connections.lock().unwrap().is_empty());
        let mut byte = [0_u8; 1];
        assert_eq!(peer.read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn failed_data_frame_releases_the_tracked_connection() {
        let connections: Connections = Arc::default();
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let tunnel = Arc::new(Mutex::new(RejectWrites));
        let reader = register_connection(1, stream, &connections, &tunnel).unwrap();
        let task = spawn_reader(1, reader, Arc::clone(&connections), tunnel);

        peer.write_all(b"data").unwrap();
        task.join().unwrap();
        assert!(connections.lock().unwrap().is_empty());
        // Portable close observation (macOS may accept writes after SHUT_RD).
        let _ = peer.set_read_timeout(Some(Duration::from_secs(2)));
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
        assert!(
            receive
                .recv_timeout(Duration::from_millis(100))
                .unwrap()
                .is_err()
        );
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
            42,
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
        let (host_tunnel, guest_tunnel) = UnixStream::pair().unwrap();
        let host_control = host_tunnel.try_clone().unwrap();
        let (ready_send, ready_receive) = mpsc::channel();
        let host_task = {
            let reader = host_tunnel.try_clone().unwrap();
            thread::spawn(move || {
                run_host_with_ready(reader, host_tunnel, &daemon_socket, token, move |ready| {
                    ready_send.send(ready).unwrap();
                })
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

        ready_receive
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !socket.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(socket.exists());
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
                    },
                    std::fs::File::open("/dev/null").unwrap(),
                    &mut shell_output,
                    Vec::new(),
                )
                .unwrap(),
            0
        );
        assert_eq!(shell_output, b"\x1b[32mguest shell\x1b[0m");

        // Interrupt the transport through the socket rather than injecting a
        // frame outside the relay writer lock alongside in-flight credits.
        host_control.shutdown(Shutdown::Both).unwrap();
        guest_task.join().unwrap();
        host_task.join().unwrap();
        stop.store(true, Ordering::Relaxed);
        server_task.join().unwrap();
        assert!(!socket.exists());
        assert!(!token_path.exists());
        assert!(!runtime.exists());
    }
}
