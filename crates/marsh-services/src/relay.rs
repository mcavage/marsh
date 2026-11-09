//! `marsh --internal-services relay`: the stdio channel server (S3; E1 is the
//! host side).
//!
//! The daemon starts one long-lived exec per services VM and speaks `frame`
//! frames over its stdio. **Only the host opens channels**:
//!
//! - `docker`: the stream is handed to E3's filtered endpoint
//!   (`policy::endpoint::Services::serve_stream`) with the host-attested
//!   authority from the channel's open frame. The relay never dials the engine
//!   socket for a channel.
//! - `tcp{port}`: dials `127.0.0.1:<port>` only if that port is published by a
//!   running container the policy admitted into the authority's workspace
//!   (`LedgerPorts`: E3's ledger scope, checked against the engine at open).
//!
//! Control frames: `capture`, `drop`, `list`, `reap`, `resolve`, `restore_prepare` and
//! `restore_up` (each answered by request id), and `ports` (sent by the agent whenever the engine's published
//! ports change). Flow control is credits: 16 KiB chunks, 64 per channel and
//! direction, fair round-robin on the shared stdout. Unknown, oversized or
//! malformed frames close the relay with an `error` frame; nothing panics on
//! input.
// The agent only runs on Linux (inside the engine container); on other hosts
// the Linux-only half is compiled out and what remains is not used.
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

use crate::breaks;
use crate::capture::{CaptureConfig, CaptureError};
use crate::engine_api::Engine;
use crate::frame::{
    AgentFrame, Authority, CHUNK_BYTES, ChannelKind, FrameError, HostFrame, Inbound,
    MAX_AGENT_JSON_BYTES, MAX_HOST_JSON_BYTES, Outbound, PROTOCOL, ResolvedPath, Settings,
    WINDOW_CHUNKS, read_frame, write_frame,
};
use crate::locks::Freezer;
use crate::locks::ScopeLocks;
use crate::ports;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The production engine's socket and the pool mount inside the engine
/// container: S2's values (`--engine-socket` and `--pool` override them for
/// tests).
pub const DEFAULT_ENGINE_SOCKET: &str = crate::engine::ENGINE_SOCKET;
pub const DEFAULT_POOL: &str = crate::engine::POOL_MOUNT;
/// Most frames the output queue holds before producers wait. Producers that
/// cannot make progress within the stall deadline end the relay.
const MAX_QUEUED_FRAMES: usize = 2048;
const MAX_QUEUED_DATA_BYTES: usize = 8 << 20;

const CREDIT_BATCH: u32 = 16;
const MAX_REQUESTS_IN_FLIGHT: usize = 8;
const MAX_RESOLVE_PATHS: usize = 64;
const MAX_RESOLVE_PATH_BYTES: usize = 4096;
const DIAL_TIMEOUT: Duration = Duration::from_secs(3);

/// Where `docker` channels go. E3's filtered endpoint implements this.
pub trait DockerEndpoint: Send + Sync {
    /// Checks the authority before a channel is opened for it.
    ///
    /// # Errors
    /// The reason the authority cannot be served (the channel is refused).
    fn check(&self, _authority: &Authority) -> Result<(), String> {
        Ok(())
    }

    /// Serves one connection for `authority`; returns when it is done.
    fn serve(&self, authority: Authority, stream: UnixStream);

    /// Called once when the relay listens (after recovery, before `ready`).
    fn start(&self) {}

    /// A workspace is being dropped (C3): forget what exists for the scope
    /// itself (substituted sockets, the default network entry). The default
    /// is for endpoints with nothing to forget.
    ///
    /// # Errors
    /// Why the scope could not be forgotten.
    fn forget_scope(&self, _workspace: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Removes an attempt's containers when the attempt ends (E4; `reap.rs`).
pub trait Reaper: Send + Sync {
    fn reap(&self, workspace: &str, attempt: &str) -> crate::reap::Report;
}

/// Which published ports a channel may dial.
pub trait PortPolicy: Send + Sync {
    fn allows(&self, authority: &Authority, port: u16) -> bool;

    /// Whether the policy admitted this container into the workspace (what the
    /// `ports` frames may report).
    fn admitted(&self, workspace: &str, container_id: &str) -> bool;
}

/// E3's filtered endpoint behind the relay's channels. The authority of a
/// channel is the host's (the open frame): `project` is the project root (an
/// absolute path), `workspace` and `attempt` are E3's scope and attempt ids.
pub struct Filtered {
    svc: Arc<crate::policy::endpoint::Services>,
}

impl Filtered {
    #[must_use]
    pub fn new(svc: Arc<crate::policy::endpoint::Services>) -> Self {
        Self { svc }
    }

    fn policy_authority(authority: &Authority) -> crate::policy::Authority {
        crate::policy::Authority {
            project_root: PathBuf::from(&authority.project),
            workspace: authority.workspace.clone(),
            attempt: authority.attempt.clone(),
        }
    }
}

impl DockerEndpoint for Filtered {
    fn check(&self, authority: &Authority) -> Result<(), String> {
        Self::policy_authority(authority).validate()
    }

    fn serve(&self, authority: Authority, stream: UnixStream) {
        let _ = self
            .svc
            .serve_stream(&Self::policy_authority(&authority), stream);
    }

    fn forget_scope(&self, workspace: &str) -> Result<(), String> {
        self.svc
            .forget_scope(workspace)
            .map_err(|error| error.to_string())
    }

    fn start(&self) {
        // The sockets reserved for substituted binds are served again, now that
        // the relay listens (E3 leaves this to the agent).
        let restored = self.svc.restore_sockets();
        eprintln!("marsh services relay: restored {restored} filtered sockets");
    }
}

impl Reaper for Filtered {
    fn reap(&self, workspace: &str, attempt: &str) -> crate::reap::Report {
        crate::reap::reap(&self.svc, workspace, attempt)
    }
}

impl Freezer for Filtered {
    fn freeze(&self, workspace: &str, wait: Duration) -> Result<Box<dyn Send>, String> {
        self.svc
            .freeze_scope_within(workspace, Some(wait))
            .map(|guard| Box::new(guard) as Box<dyn Send>)
            .ok_or_else(|| "a request in this workspace is still running".to_owned())
    }
}

/// The ports a container's workspace may be dialled on: published (on loopback
/// inside the engine) by a running container that E3's ledger holds for that
/// workspace and whose scope label is that workspace, read from the engine at
/// the time of the open. Only the ledger's own containers are inspected, by
/// id, never the engine's whole container list.
pub struct LedgerPorts {
    svc: Arc<crate::policy::endpoint::Services>,
    engine: Engine,
    /// The container that last answered for a workspace and port: asked first
    /// next time. A hint only: it is inspected again like any other.
    hints: Mutex<HashMap<(String, u16), String>>,
}

const MAX_HINTS: usize = 1024;

impl LedgerPorts {
    #[must_use]
    pub fn new(svc: Arc<crate::policy::endpoint::Services>, engine: Engine) -> Self {
        Self {
            svc,
            engine,
            hints: Mutex::default(),
        }
    }

    fn hints(&self) -> std::sync::MutexGuard<'_, HashMap<(String, u16), String>> {
        self.hints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn publishes(&self, workspace: &str, id: &str, port: u16) -> bool {
        self.engine
            .inspect_container(id)
            .is_ok_and(|detail| may_dial(&detail, workspace, port))
    }
}

/// The rule: running, labelled for this workspace, publishing the port.
fn may_dial(detail: &crate::engine_api::ContainerDetail, workspace: &str, port: u16) -> bool {
    detail.is_running()
        && detail.label(crate::engine_api::SCOPE_LABEL) == Some(workspace)
        && detail.publishes_tcp(port)
}

impl PortPolicy for LedgerPorts {
    fn allows(&self, authority: &Authority, port: u16) -> bool {
        if crate::breaks::break_mode("ports-list-all") {
            return self.allows_by_listing(authority, port);
        }
        let workspace = authority.workspace.as_str();
        let mut owned = self.svc.ledger.container_ids(workspace);
        if owned.is_empty() {
            return false;
        }
        let key = (workspace.to_owned(), port);
        let hint = self.hints().get(&key).cloned();
        if let Some(hint) = hint {
            // Asked first; it is still checked against the engine's answer now.
            if let Some(at) = owned.iter().position(|id| *id == hint) {
                owned.swap(0, at);
            }
        }
        for id in &owned {
            if self.publishes(workspace, id, port) {
                let mut hints = self.hints();
                if hints.len() >= MAX_HINTS {
                    hints.clear();
                }
                hints.insert(key, id.clone());
                return true;
            }
        }
        self.hints().remove(&key);
        false
    }

    fn admitted(&self, workspace: &str, container_id: &str) -> bool {
        self.svc
            .ledger
            .container_ids(workspace)
            .iter()
            .any(|id| id == container_id)
    }
}

impl LedgerPorts {
    /// Test break (`ports-list-all`): the check as it was, one listing of every
    /// container the engine has per open.
    fn allows_by_listing(&self, authority: &Authority, port: u16) -> bool {
        let owned = self.svc.ledger.container_ids(&authority.workspace);
        if owned.is_empty() {
            return false;
        }
        self.engine.list_containers(false).is_ok_and(|containers| {
            containers.iter().any(|c| {
                c.state == "running"
                    && owned.contains(&c.id)
                    && c.label(crate::engine_api::SCOPE_LABEL) == Some(authority.workspace.as_str())
                    && c.ports
                        .iter()
                        .flatten()
                        .any(|p| p.kind == "tcp" && p.public_port == port)
            })
        })
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub engine_socket: PathBuf,
    pub pool: PathBuf,
    /// Break modes from the dedicated `--break-modes` argument (test builds).
    pub break_modes: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            engine_socket: PathBuf::from(DEFAULT_ENGINE_SOCKET),
            pool: PathBuf::from(DEFAULT_POOL),
            break_modes: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
struct Limits {
    stall: Duration,
    idle: Duration,
    max_channels: usize,
    max_per_attempt: usize,
}

impl Limits {
    fn from(settings: &Settings) -> Self {
        let millis = |value: Option<u64>, default: u64, low: u64, high: u64| {
            Duration::from_millis(value.unwrap_or(default).clamp(low, high))
        };
        Self {
            stall: millis(settings.stall_ms, 60_000, 1_000, 600_000),
            idle: millis(settings.idle_ms, 1_800_000, 5_000, 86_400_000),
            max_channels: settings
                .max_channels
                .map_or(256, |n| (n as usize).clamp(1, 256)),
            max_per_attempt: settings
                .max_channels_per_attempt
                .map_or(64, |n| (n as usize).clamp(1, 64)),
        }
    }
}

fn capture_config(settings: &Settings) -> CaptureConfig {
    let millis = |value: Option<u64>, default: u64, low: u64, high: u64| {
        Duration::from_millis(value.unwrap_or(default).clamp(low, high))
    };
    CaptureConfig {
        freeze_budget: millis(settings.freeze_budget_ms, 500, 20, 10_000),
        watchdog: millis(settings.watchdog_ms, 3_000, 500, 60_000),
        lock_wait: millis(settings.lock_wait_ms, 500, 0, 10_000),
        ..CaptureConfig::default()
    }
}

// ---------------------------------------------------------------- output

/// An item that must stay ordered within its channel.
enum ChannelItem {
    Opened,
    Refused(String),
    Data(Vec<u8>),
    Eof,
    Close(Option<String>),
}

#[derive(Default)]
struct OutInner {
    control: VecDeque<AgentFrame>,
    /// The latest `ports` state: a new one replaces the one still waiting.
    ports: Option<AgentFrame>,
    channels: HashMap<u32, VecDeque<ChannelItem>>,
    ring: VecDeque<u32>,
    queued: usize,
    data_bytes: usize,
    closed: bool,
}

/// The shared stdout queue. Control frames go first; channel items are served
/// round-robin so one busy channel cannot starve the others, and keep their
/// order within a channel. The queue is bounded (frames and data bytes):
/// producers wait for space, and if one waits past the stall deadline the host
/// is not reading, so the relay ends rather than buffer without limit.
struct OutQueue {
    inner: Mutex<OutInner>,
    wake: Condvar,
    space: Condvar,
    stall: Duration,
    /// When the writer started its current blocking write.
    writing_since: Mutex<Option<Instant>>,
}

impl OutQueue {
    fn new(stall: Duration) -> Self {
        Self {
            inner: Mutex::new(OutInner::default()),
            wake: Condvar::new(),
            space: Condvar::new(),
            stall,
            writing_since: Mutex::new(None),
        }
    }

    fn lock(&self) -> MutexGuard<'_, OutInner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn transport_stalled(what: &str) -> ! {
        eprintln!("marsh services relay: the host is not reading ({what}); ending the relay");
        std::process::exit(1);
    }

    /// Waits for room for one more frame carrying `bytes` of data. Returns
    /// `None` once the queue is closed.
    fn reserve(&self, bytes: usize) -> Option<MutexGuard<'_, OutInner>> {
        let deadline = Instant::now() + self.stall;
        let mut inner = self.lock();
        loop {
            if inner.closed {
                return None;
            }
            if breaks::break_mode("output-unbounded")
                || inner.queued < MAX_QUEUED_FRAMES
                    && (bytes == 0 || inner.data_bytes < MAX_QUEUED_DATA_BYTES)
            {
                return Some(inner);
            }
            let now = Instant::now();
            if now >= deadline {
                drop(inner);
                Self::transport_stalled("output queue full");
            }
            inner = self
                .space
                .wait_timeout(inner, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    fn control(&self, frame: AgentFrame) {
        if matches!(frame, AgentFrame::Ports { .. }) {
            self.lock().ports = Some(frame);
            self.wake.notify_one();
            return;
        }
        let Some(mut inner) = self.reserve(0) else {
            return;
        };
        inner.control.push_back(frame);
        inner.queued += 1;
        drop(inner);
        self.wake.notify_one();
    }

    fn item(&self, channel: u32, item: ChannelItem) {
        let bytes = if let ChannelItem::Data(data) = &item {
            data.len()
        } else {
            0
        };
        let Some(mut inner) = self.reserve(bytes) else {
            return;
        };
        let queue = inner.channels.entry(channel).or_default();
        let was_empty = queue.is_empty();
        queue.push_back(item);
        if was_empty {
            inner.ring.push_back(channel);
        }
        inner.queued += 1;
        inner.data_bytes += bytes;
        drop(inner);
        self.wake.notify_one();
    }

    fn close(&self) {
        self.lock().closed = true;
        self.wake.notify_all();
        self.space.notify_all();
    }

    /// Blocks for the next frame; `None` once closed and drained.
    fn next(&self) -> Option<Outbound> {
        let mut inner = self.lock();
        loop {
            if let Some(frame) = inner.control.pop_front() {
                inner.queued -= 1;
                drop(inner);
                self.space.notify_all();
                return Some(Outbound::Control(frame));
            }
            if let Some(frame) = inner.ports.take() {
                return Some(Outbound::Control(frame));
            }
            if let Some(channel) = inner.ring.pop_front() {
                let queue = inner.channels.get_mut(&channel)?;
                let item = queue.pop_front();
                if queue.is_empty() {
                    inner.channels.remove(&channel);
                } else {
                    inner.ring.push_back(channel);
                }
                if let Some(item) = item {
                    inner.queued -= 1;
                    if let ChannelItem::Data(data) = &item {
                        inner.data_bytes -= data.len();
                    }
                    drop(inner);
                    self.space.notify_all();
                    return Some(match item {
                        ChannelItem::Opened => Outbound::Control(AgentFrame::Opened { channel }),
                        ChannelItem::Refused(reason) => {
                            Outbound::Control(AgentFrame::Refused { channel, reason })
                        }
                        ChannelItem::Data(bytes) => Outbound::Data { channel, bytes },
                        ChannelItem::Eof => Outbound::Control(AgentFrame::Eof { channel }),
                        ChannelItem::Close(reason) => {
                            Outbound::Control(AgentFrame::Close { channel, reason })
                        }
                    });
                }
                continue;
            }
            if inner.closed {
                return None;
            }
            inner = self
                .wake
                .wait(inner)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn set_writing(&self, since: Option<Instant>) {
        *self
            .writing_since
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = since;
    }

    /// Ends the process if one write has been blocked past the stall deadline
    /// (a blocked `write` cannot be interrupted).
    fn watch_transport(self: &Arc<Self>) {
        let queue = Arc::clone(self);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let since = *queue
                    .writing_since
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if since.is_some_and(|t| t.elapsed() > queue.stall) {
                    Self::transport_stalled("a write is blocked");
                }
            }
        });
    }
}

// ---------------------------------------------------------------- targets

enum Target {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Target {
    fn try_clone(&self) -> std::io::Result<Self> {
        Ok(match self {
            Self::Unix(s) => Self::Unix(s.try_clone()?),
            Self::Tcp(s) => Self::Tcp(s.try_clone()?),
        })
    }

    fn shutdown(&self, how: Shutdown) {
        let _ = match self {
            Self::Unix(s) => s.shutdown(how),
            Self::Tcp(s) => s.shutdown(how),
        };
    }

    fn set_write_timeout(&self, timeout: Duration) {
        let _ = match self {
            Self::Unix(s) => s.set_write_timeout(Some(timeout)),
            Self::Tcp(s) => s.set_write_timeout(Some(timeout)),
        };
    }
}

impl Read for Target {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Unix(s) => s.read(buffer),
            Self::Tcp(s) => s.read(buffer),
        }
    }
}

impl Write for Target {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Unix(s) => s.write(buffer),
            Self::Tcp(s) => s.write(buffer),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Unix(s) => s.flush(),
            Self::Tcp(s) => s.flush(),
        }
    }
}

// ---------------------------------------------------------------- channels

enum ToTarget {
    Data(Vec<u8>),
    Eof,
    Close,
}

#[derive(Default)]
struct Credit {
    chunks: u32,
    dead: bool,
}

struct Channel {
    authority: Authority,
    to_target: SyncSender<ToTarget>,
    credit: Mutex<Credit>,
    credit_changed: Condvar,
    target: Mutex<Option<Target>>,
    closed: AtomicBool,
    last_activity: Mutex<Instant>,
    agent_eof: AtomicBool,
    host_eof: AtomicBool,
}

impl Channel {
    fn touch(&self) {
        *self
            .last_activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Instant::now();
    }
}

struct Shared {
    out: Arc<OutQueue>,
    channels: Mutex<HashMap<u32, Arc<Channel>>>,
    limits: Limits,
    endpoint: Arc<dyn DockerEndpoint>,
    port_policy: Arc<dyn PortPolicy>,
    reaper: Arc<dyn Reaper>,
    #[cfg(target_os = "linux")]
    capturer: Arc<crate::capture::Capturer>,
    #[cfg(target_os = "linux")]
    restorer: Arc<crate::restore::Restorer>,
    requests: AtomicUsize,
}

impl Shared {
    fn channels(&self) -> MutexGuard<'_, HashMap<u32, Arc<Channel>>> {
        self.channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Ends a channel: idempotent. `tell_host` sends a `close` frame.
    fn end_channel(&self, id: u32, channel: &Arc<Channel>, reason: Option<&str>, tell_host: bool) {
        if channel.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut map = self.channels();
            if map.get(&id).is_some_and(|c| Arc::ptr_eq(c, channel)) {
                map.remove(&id);
            }
        }
        if tell_host {
            self.out
                .item(id, ChannelItem::Close(reason.map(str::to_owned)));
        }
        if let Some(target) = channel
            .target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            target.shutdown(Shutdown::Both);
        }
        channel
            .credit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .dead = true;
        channel.credit_changed.notify_all();
        let _ = channel.to_target.try_send(ToTarget::Close);
    }

    fn maybe_finish(&self, id: u32, channel: &Arc<Channel>) {
        if channel.agent_eof.load(Ordering::SeqCst) && channel.host_eof.load(Ordering::SeqCst) {
            // Both directions are done and all data has been queued ahead of
            // the agent's eof; no close frame is needed.
            self.end_channel(id, channel, None, false);
        }
    }
}

fn spawn_channel(
    shared: &Arc<Shared>,
    id: u32,
    kind: ChannelKind,
    port: Option<u16>,
    authority: Authority,
) {
    let (to_target, from_host) = sync_channel::<ToTarget>(WINDOW_CHUNKS as usize + 4);
    let channel = Arc::new(Channel {
        authority: authority.clone(),
        to_target,
        credit: Mutex::new(Credit {
            chunks: WINDOW_CHUNKS,
            dead: false,
        }),
        credit_changed: Condvar::new(),
        target: Mutex::new(None),
        closed: AtomicBool::new(false),
        last_activity: Mutex::new(Instant::now()),
        agent_eof: AtomicBool::new(false),
        host_eof: AtomicBool::new(false),
    });
    shared.channels().insert(id, Arc::clone(&channel));
    let shared = Arc::clone(shared);
    std::thread::spawn(move || {
        let target = match establish(&shared, kind, port, &authority) {
            Ok(target) => target,
            Err(reason) => {
                shared.end_channel(id, &channel, None, false);
                shared.out.item(id, ChannelItem::Refused(reason));
                return;
            }
        };
        target.set_write_timeout(shared.limits.stall);
        let (Ok(read_half), Ok(write_half)) = (target.try_clone(), target.try_clone()) else {
            shared.end_channel(id, &channel, None, false);
            shared
                .out
                .item(id, ChannelItem::Refused("could not use the stream".into()));
            return;
        };
        *channel
            .target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(target);
        if channel.closed.load(Ordering::SeqCst) {
            read_half.shutdown(Shutdown::Both);
            return;
        }
        shared.out.item(id, ChannelItem::Opened);
        {
            let shared = Arc::clone(&shared);
            let channel = Arc::clone(&channel);
            std::thread::spawn(move || {
                to_target_loop(&shared, id, &channel, &from_host, write_half);
            });
        }
        from_target_loop(&shared, id, &channel, read_half);
    });
}

fn establish(
    shared: &Arc<Shared>,
    kind: ChannelKind,
    port: Option<u16>,
    authority: &Authority,
) -> Result<Target, String> {
    match kind {
        ChannelKind::Docker => {
            shared.endpoint.check(authority)?;
            let (ours, theirs) = UnixStream::pair()
                .map_err(|error| format!("could not open the stream: {error}"))?;
            let endpoint = Arc::clone(&shared.endpoint);
            let authority = authority.clone();
            std::thread::spawn(move || endpoint.serve(authority, theirs));
            Ok(Target::Unix(ours))
        }
        ChannelKind::Tcp => {
            let port = port.ok_or("tcp channel without a port")?;
            if !shared.port_policy.allows(authority, port) {
                return Err("marsh: that port is not published for this workspace".into());
            }
            let address = SocketAddr::from(([127, 0, 0, 1], port));
            TcpStream::connect_timeout(&address, DIAL_TIMEOUT)
                .map(Target::Tcp)
                .map_err(|error| format!("could not connect: {error}"))
        }
    }
}

fn to_target_loop(
    shared: &Arc<Shared>,
    id: u32,
    channel: &Arc<Channel>,
    from_host: &Receiver<ToTarget>,
    mut target: Target,
) {
    let mut consumed = 0_u32;
    loop {
        match from_host.recv() {
            Ok(ToTarget::Data(bytes)) => {
                if let Err(error) = target.write_all(&bytes) {
                    let reason = if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) {
                        "stalled"
                    } else {
                        "write failed"
                    };
                    shared.end_channel(id, channel, Some(reason), true);
                    return;
                }
                channel.touch();
                consumed += 1;
                if consumed >= CREDIT_BATCH {
                    shared.out.control(AgentFrame::Credit {
                        channel: id,
                        chunks: consumed,
                    });
                    consumed = 0;
                }
            }
            Ok(ToTarget::Eof) => {
                if consumed > 0 {
                    shared.out.control(AgentFrame::Credit {
                        channel: id,
                        chunks: consumed,
                    });
                    consumed = 0;
                }
                target.shutdown(Shutdown::Write);
                channel.host_eof.store(true, Ordering::SeqCst);
                shared.maybe_finish(id, channel);
            }
            Ok(ToTarget::Close) | Err(_) => return,
        }
    }
}

fn from_target_loop(shared: &Arc<Shared>, id: u32, channel: &Arc<Channel>, mut target: Target) {
    let mut buffer = vec![0_u8; CHUNK_BYTES];
    loop {
        let read = match target.read(&mut buffer) {
            Ok(n) => n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => {
                shared.end_channel(id, channel, Some("read failed"), true);
                return;
            }
        };
        if read == 0 {
            // Queued after all data already pushed for this channel.
            shared.out.item(id, ChannelItem::Eof);
            channel.agent_eof.store(true, Ordering::SeqCst);
            shared.maybe_finish(id, channel);
            return;
        }
        // Wait for credit from the host (progress deadline).
        let deadline = Instant::now() + shared.limits.stall;
        let mut credit = channel
            .credit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while credit.chunks == 0 && !credit.dead {
            let now = Instant::now();
            if now >= deadline {
                drop(credit);
                shared.end_channel(id, channel, Some("stalled"), true);
                return;
            }
            credit = channel
                .credit_changed
                .wait_timeout(credit, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        if credit.dead {
            return;
        }
        credit.chunks -= 1;
        drop(credit);
        channel.touch();
        shared
            .out
            .item(id, ChannelItem::Data(buffer[..read].to_vec()));
    }
}

fn janitor(shared: &Arc<Shared>) {
    let shared = Arc::clone(shared);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(5));
            let idle: Vec<(u32, Arc<Channel>)> = shared
                .channels()
                .iter()
                .filter(|(_, channel)| {
                    channel
                        .last_activity
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .elapsed()
                        > shared.limits.idle
                })
                .map(|(id, channel)| (*id, Arc::clone(channel)))
                .collect();
            for (id, channel) in idle {
                shared.end_channel(id, &channel, Some("idle"), true);
            }
        }
    });
}

// ---------------------------------------------------------------- requests

fn begin_request(shared: &Arc<Shared>, request: u64) -> bool {
    if shared.requests.fetch_add(1, Ordering::SeqCst) >= MAX_REQUESTS_IN_FLIGHT {
        shared.requests.fetch_sub(1, Ordering::SeqCst);
        shared.out.control(AgentFrame::Error {
            request: Some(request),
            message: "too many requests in progress".into(),
        });
        return false;
    }
    true
}

fn spawn_request(shared: &Arc<Shared>, request: u64, work: impl FnOnce(&Shared) + Send + 'static) {
    if !begin_request(shared, request) {
        return;
    }
    let shared = Arc::clone(shared);
    std::thread::spawn(move || {
        work(&shared);
        shared.requests.fetch_sub(1, Ordering::SeqCst);
    });
}

fn request_error(shared: &Shared, request: u64, message: impl Into<String>) {
    shared.out.control(AgentFrame::Error {
        request: Some(request),
        message: message.into(),
    });
}

#[cfg(target_os = "linux")]
fn handle_capture(
    shared: &Arc<Shared>,
    request: u64,
    id: String,
    workspace: String,
    reserve_bytes: u64,
) {
    spawn_request(shared, request, move |shared| {
        let capturer = &shared.capturer;
        match capturer.capture(&id, &workspace, reserve_bytes) {
            Ok(report) => {
                let manifest = report
                    .manifest
                    .as_ref()
                    .and_then(|manifest| serde_json::to_value(manifest).ok());
                let mut frame = AgentFrame::Captured {
                    request,
                    id: report.id.clone(),
                    outcome: report.outcome.as_str().to_owned(),
                    manifest,
                    gaps: report.gaps.clone(),
                };
                let size = serde_json::to_vec(&frame).map_or(usize::MAX, |bytes| bytes.len());
                if size > MAX_AGENT_JSON_BYTES - 1024 {
                    frame = AgentFrame::Captured {
                        request,
                        id: report.id,
                        outcome: report.outcome.as_str().to_owned(),
                        manifest: None,
                        gaps: vec!["the capture record was too large to report".into()],
                    };
                }
                shared.out.control(frame);
            }
            Err(
                CaptureError::Invalid(why) | CaptureError::Failed(why) | CaptureError::Stuck(why),
            ) => {
                request_error(shared, request, why);
            }
        }
    });
}

#[cfg(not(target_os = "linux"))]
fn handle_capture(
    shared: &Arc<Shared>,
    request: u64,
    _id: String,
    _workspace: String,
    _reserve_bytes: u64,
) {
    request_error(shared, request, "snapshots require Linux");
}

#[cfg(target_os = "linux")]
fn handle_fence(shared: &Arc<Shared>, request: u64, workspaces: Vec<String>) {
    spawn_request(shared, request, move |shared| {
        let (held, reason) = match shared.capturer.fence(&workspaces) {
            Ok(()) => (true, None),
            Err(why) => (false, Some(why)),
        };
        shared.out.control(AgentFrame::Fenced {
            request,
            held,
            reason,
        });
    });
}

#[cfg(not(target_os = "linux"))]
fn handle_fence(shared: &Arc<Shared>, request: u64, _workspaces: Vec<String>) {
    request_error(shared, request, "snapshots require Linux");
}

#[cfg(target_os = "linux")]
fn handle_unfence(shared: &Arc<Shared>, request: u64) {
    spawn_request(shared, request, move |shared| {
        shared.capturer.unfence();
        shared.out.control(AgentFrame::Fenced {
            request,
            held: false,
            reason: None,
        });
    });
}

#[cfg(not(target_os = "linux"))]
fn handle_unfence(shared: &Arc<Shared>, request: u64) {
    request_error(shared, request, "snapshots require Linux");
}

#[cfg(target_os = "linux")]
fn handle_drop(shared: &Arc<Shared>, request: u64, id: String) {
    spawn_request(shared, request, move |shared| {
        let capturer = &shared.capturer;
        match capturer.drop_snapshot(&id) {
            Ok(existed) => shared.out.control(AgentFrame::Dropped {
                request,
                id,
                existed,
            }),
            Err(error) => request_error(shared, request, error.to_string()),
        }
    });
}

#[cfg(not(target_os = "linux"))]
fn handle_drop(shared: &Arc<Shared>, request: u64, _id: String) {
    request_error(shared, request, "snapshots require Linux");
}

fn handle_reap(shared: &Arc<Shared>, request: u64, workspace: String, attempt: String) {
    if !crate::reap::valid_id(&workspace) || !crate::reap::valid_id(&attempt) {
        return request_error(shared, request, "invalid reap request");
    }
    spawn_request(shared, request, move |shared| {
        let report = shared.reaper.reap(&workspace, &attempt);
        shared.out.control(AgentFrame::Reaped {
            request,
            removed: report.removed,
            kept: report.kept,
            complete: report.complete,
            gaps: report.gaps,
        });
    });
}

#[cfg(target_os = "linux")]
fn handle_drop_workspace(shared: &Arc<Shared>, request: u64, workspace: String) {
    spawn_request(shared, request, move |shared| {
        match shared.restorer.drop_workspace(&workspace) {
            Ok(report) => shared.out.control(AgentFrame::WorkspaceDropped {
                request,
                workspace,
                containers: report.containers,
                volumes: report.volumes,
                networks: report.networks,
                left: report.left,
            }),
            Err(error) => restore_error(shared, request, &error),
        }
    });
}

#[cfg(not(target_os = "linux"))]
fn handle_drop_workspace(shared: &Arc<Shared>, request: u64, _workspace: String) {
    request_error(shared, request, "workspaces require Linux");
}

#[cfg(target_os = "linux")]
fn handle_list(shared: &Arc<Shared>, request: u64) {
    spawn_request(shared, request, move |shared| {
        let capturer = &shared.capturer;
        match capturer.list() {
            Ok(snapshots) => shared
                .out
                .control(AgentFrame::Listed { request, snapshots }),
            Err(error) => request_error(shared, request, error.to_string()),
        }
    });
}

#[cfg(not(target_os = "linux"))]
fn handle_list(shared: &Arc<Shared>, request: u64) {
    request_error(shared, request, "snapshots require Linux");
}

#[cfg(target_os = "linux")]
fn restore_error(shared: &Shared, request: u64, error: &crate::restore::RestoreError) {
    request_error(shared, request, error.to_string());
}

#[cfg(target_os = "linux")]
fn handle_restore_prepare(shared: &Arc<Shared>, request: u64, req: crate::restore::PrepareRequest) {
    spawn_request(shared, request, move |shared| {
        match shared.restorer.prepare(&req) {
            Ok(result) => shared
                .out
                .control(AgentFrame::RestorePrepared { request, result }),
            Err(error) => restore_error(shared, request, &error),
        }
    });
}

#[cfg(target_os = "linux")]
fn handle_restore_up(shared: &Arc<Shared>, request: u64, req: crate::restore::UpRequest) {
    spawn_request(shared, request, move |shared| {
        match shared.restorer.up(&req) {
            Ok(result) => shared.out.control(AgentFrame::Restored { request, result }),
            Err(error) => restore_error(shared, request, &error),
        }
    });
}

#[cfg(not(target_os = "linux"))]
fn handle_restore_unavailable(shared: &Arc<Shared>, request: u64) {
    request_error(shared, request, "restore requires Linux");
}

fn handle_resolve(shared: &Arc<Shared>, request: u64, paths: Vec<String>) {
    if paths.len() > MAX_RESOLVE_PATHS
        || paths
            .iter()
            .any(|p| p.len() > MAX_RESOLVE_PATH_BYTES || !p.starts_with('/') || p.contains('\0'))
    {
        request_error(shared, request, "invalid resolve request");
        return;
    }
    spawn_request(shared, request, move |shared| {
        let resolved = paths
            .into_iter()
            .map(|path| match std::fs::canonicalize(&path) {
                Ok(real) => ResolvedPath {
                    path,
                    resolved: Some(real.to_string_lossy().into_owned()),
                    error: None,
                },
                Err(error) => ResolvedPath {
                    path,
                    resolved: None,
                    error: Some(format!("{:?}", error.kind())),
                },
            })
            .collect();
        shared.out.control(AgentFrame::Resolved {
            request,
            paths: resolved,
        });
    });
}

// ---------------------------------------------------------------- main loop

/// Waits for the writer to finish what is queued, but never longer than the
/// stall deadline: a blocked transport must not keep the relay alive.
fn finish(shared: &Arc<Shared>, done: &Arc<(Mutex<bool>, Condvar)>) {
    shared.out.close();
    let (flag, signal) = &**done;
    let deadline = Instant::now()
        + if breaks::break_mode("output-unbounded") {
            Duration::from_hours(24)
        } else {
            shared.limits.stall
        };
    let mut finished = flag
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while !*finished {
        let now = Instant::now();
        if now >= deadline {
            eprintln!("marsh services relay: the output is blocked; exiting");
            return;
        }
        finished = signal
            .wait_timeout(finished, deadline - now)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
}

fn fatal(shared: &Arc<Shared>, done: &Arc<(Mutex<bool>, Condvar)>, message: &str) -> i32 {
    eprintln!("marsh services relay: {message}");
    shared.out.control(AgentFrame::Error {
        request: None,
        message: message.to_owned(),
    });
    finish(shared, done);
    1
}

fn open_channel(
    shared: &Arc<Shared>,
    channel: u32,
    kind: ChannelKind,
    port: Option<u16>,
    authority: Authority,
) -> Result<(), String> {
    let refuse = |reason: &str| {
        shared
            .out
            .item(channel, ChannelItem::Refused(reason.to_owned()));
    };
    if let Err(why) = authority.validate() {
        refuse(&why);
        return Ok(());
    }
    match (kind, port) {
        (ChannelKind::Tcp, Some(port)) if port != 0 => {}
        (ChannelKind::Docker, None) => {}
        _ => {
            refuse("invalid channel kind or port");
            return Ok(());
        }
    }
    {
        let map = shared.channels();
        if map.contains_key(&channel) {
            return Err(format!("channel {channel} is already open"));
        }
        let same_attempt = map
            .values()
            .filter(|c| {
                c.authority.project == authority.project
                    && c.authority.workspace == authority.workspace
                    && c.authority.attempt == authority.attempt
            })
            .count();
        let same_project = map
            .values()
            .filter(|c| c.authority.project == authority.project)
            .count();
        if same_attempt >= shared.limits.max_per_attempt
            || same_project >= shared.limits.max_channels
            || map.len() >= shared.limits.max_channels
        {
            drop(map);
            refuse("too many open channels");
            return Ok(());
        }
    }
    spawn_channel(shared, channel, kind, port, authority);
    Ok(())
}

#[allow(clippy::too_many_lines)] // One arm per host frame.
fn handle_frame(shared: &Arc<Shared>, frame: HostFrame) -> Result<bool, String> {
    match frame {
        HostFrame::Init { .. } => return Err("init sent twice".into()),
        HostFrame::Open {
            channel,
            kind,
            port,
            authority,
        } => open_channel(shared, channel, kind, port, authority)?,
        HostFrame::Credit { channel, chunks } => {
            let target = shared.channels().get(&channel).cloned();
            if let Some(target) = target {
                let mut credit = target
                    .credit
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let total = credit.chunks.saturating_add(chunks);
                if chunks == 0 || total > WINDOW_CHUNKS {
                    drop(credit);
                    shared.end_channel(channel, &target, Some("flow control violation"), true);
                } else {
                    credit.chunks = total;
                    drop(credit);
                    target.credit_changed.notify_all();
                }
            }
        }
        HostFrame::Eof { channel } => {
            let target = shared.channels().get(&channel).cloned();
            if let Some(target) = target {
                target.touch();
                if let Err(TrySendError::Full(_)) = target.to_target.try_send(ToTarget::Eof) {
                    shared.end_channel(channel, &target, Some("flow control violation"), true);
                }
            }
        }
        HostFrame::Close { channel } => {
            let target = shared.channels().get(&channel).cloned();
            if let Some(target) = target {
                shared.end_channel(channel, &target, None, false);
            }
        }
        HostFrame::Capture {
            request,
            id,
            workspace,
            reserve_bytes,
        } => handle_capture(shared, request, id, workspace, reserve_bytes),
        HostFrame::Fence {
            request,
            workspaces,
        } => handle_fence(shared, request, workspaces),
        HostFrame::Unfence { request } => handle_unfence(shared, request),
        HostFrame::DropSnapshot { request, id } => handle_drop(shared, request, id),
        HostFrame::DropWorkspace { request, workspace } => {
            handle_drop_workspace(shared, request, workspace);
        }
        HostFrame::List { request } => handle_list(shared, request),
        HostFrame::Reap {
            request,
            workspace,
            attempt,
        } => handle_reap(shared, request, workspace, attempt),
        #[cfg(target_os = "linux")]
        HostFrame::RestorePrepare {
            request,
            id,
            workspace,
            project,
            source,
            lane_dir,
            fork,
        } => handle_restore_prepare(
            shared,
            request,
            crate::restore::PrepareRequest {
                id,
                workspace,
                project,
                source,
                lane_dir,
                fork,
            },
        ),
        #[cfg(target_os = "linux")]
        HostFrame::RestoreUp {
            request,
            workspace,
            project,
            source,
            lane_dir,
            fork,
        } => handle_restore_up(
            shared,
            request,
            crate::restore::UpRequest {
                workspace,
                project,
                source,
                lane_dir,
                fork,
            },
        ),
        #[cfg(not(target_os = "linux"))]
        HostFrame::RestorePrepare { request, .. } | HostFrame::RestoreUp { request, .. } => {
            handle_restore_unavailable(shared, request);
        }
        HostFrame::Resolve { request, paths } => handle_resolve(shared, request, paths),
        HostFrame::Shutdown => return Ok(false),
    }
    Ok(true)
}

fn handle_data(shared: &Arc<Shared>, channel: u32, bytes: Vec<u8>) {
    let target = shared.channels().get(&channel).cloned();
    if let Some(target) = target {
        target.touch();
        match target.to_target.try_send(ToTarget::Data(bytes)) {
            Err(TrySendError::Full(_)) => {
                shared.end_channel(channel, &target, Some("flow control violation"), true);
            }
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

/// What the relay serves besides captures: E3's filtered endpoint, the freeze
/// the capture takes through it, and the published-port check. `locks` is the
/// operation lock shared by capture and drop across agent processes.
pub struct Services {
    pub locks: Arc<ScopeLocks>,
    /// What a capture holds while the workspace is frozen.
    pub freezer: Arc<dyn Freezer>,
    pub endpoint: Arc<dyn DockerEndpoint>,
    pub port_policy: Arc<dyn PortPolicy>,
    pub reaper: Arc<dyn Reaper>,
}

/// The agent's own view of its engine and pool, checked before `ready`: the
/// production engine answers, the pool is mounted as XFS, and the generation is
/// derived here (boot id and engine container id), never taken from the caller.
#[cfg(target_os = "linux")]
fn check_engine_and_pool(
    config: &Config,
    engine: &Engine,
) -> Result<crate::engine::Generation, String> {
    engine
        .ping()
        .map_err(|error| format!("the production engine is not ready: {error}"))?;
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|error| format!("cannot read mountinfo: {error}"))?;
    let pool = config.pool.to_string_lossy();
    match mount_fstype(&mountinfo, &pool) {
        Some(fstype) if fstype == "xfs" => {}
        Some(other) => return Err(format!("the pool at {pool} is {other}, not xfs")),
        None => return Err(format!("the pool is not mounted at {pool}")),
    }
    crate::engine::generation()
}

/// The filesystem type of the last mount at `path` in a mountinfo text.
#[cfg(target_os = "linux")]
fn mount_fstype(mountinfo: &str, path: &str) -> Option<String> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            (before.split_whitespace().nth(4)? == path)
                .then(|| after.split_whitespace().next().map(str::to_owned))?
        })
        .next_back()
}

/// Runs the relay over arbitrary streams. Returns the process exit code.
///
/// `ready` is sent only after the engine answers, the pool is mounted, the
/// generation is derived, and startup recovery has confirmed that no container
/// of a dead capture is still paused.
#[cfg(target_os = "linux")]
#[allow(clippy::too_many_lines)]
pub fn serve<R: Read, W: Write + Send + 'static>(
    reader: &mut R,
    writer: W,
    config: &Config,
    make_services: impl FnOnce() -> Result<Services, String>,
) -> i32 {
    let mut writer = writer;
    // The first frame must be `init`.
    let (expected_generation, settings) =
        match read_frame::<_, HostFrame>(reader, MAX_HOST_JSON_BYTES) {
            Ok(Some(Inbound::Control(HostFrame::Init {
                protocol,
                generation,
                settings,
            }))) if protocol == PROTOCOL => (generation, settings),
            Ok(None) => return 0,
            Ok(_) => {
                report_fatal(
                    &mut writer,
                    "the first frame must be init with a supported protocol",
                );
                return 1;
            }
            Err(error) => {
                report_fatal(&mut writer, &error.to_string());
                return 1;
            }
        };
    breaks::install(&config.break_modes);
    let limits = Limits::from(&settings);
    let engine = Engine::new(&config.engine_socket);

    // Fail closed: no `ready` until the engine, the pool and recovery are.
    let unchecked = breaks::break_mode("ready-unchecked");
    let generation = match if unchecked {
        // Deliberately wrong (a break mode): trust the caller.
        Ok(expected_generation
            .clone()
            .unwrap_or(crate::engine::Generation {
                boot_id: "unchecked".into(),
                engine_id: "unchecked".into(),
            }))
    } else {
        check_engine_and_pool(config, &engine)
    } {
        Ok(generation) => generation,
        Err(message) => {
            report_fatal(&mut writer, &message);
            return 1;
        }
    };
    if let Some(expected) = &expected_generation
        && *expected != generation
    {
        report_fatal(
            &mut writer,
            "the expected generation is not this engine's generation",
        );
        return 1;
    }
    let services = match make_services() {
        Ok(services) => services,
        Err(message) => {
            report_fatal(&mut writer, &message);
            return 1;
        }
    };
    let capturer = match crate::capture::Capturer::new(
        &config.pool,
        engine.clone(),
        Arc::clone(&services.locks),
        Arc::clone(&services.freezer),
        capture_config(&settings),
    ) {
        Ok(capturer) => capturer,
        Err(error) => {
            report_fatal(&mut writer, &format!("the pool is not usable: {error}"));
            return 1;
        }
    };
    let restorer = match crate::restore::Restorer::new(
        &config.pool,
        engine.clone(),
        Arc::clone(&services.locks),
        Arc::clone(&services.endpoint),
        crate::restore::RestoreConfig::default(),
    ) {
        Ok(restorer) => restorer,
        Err(error) => {
            report_fatal(&mut writer, &format!("the pool is not usable: {error}"));
            return 1;
        }
    };
    let recovery = capturer.recover();
    for line in &recovery.log {
        eprintln!("marsh services relay: recovery: {line}");
    }
    if !recovery.settled {
        report_fatal(
            &mut writer,
            "startup recovery could not confirm that every container is thawed",
        );
        return 1;
    }

    // The recovery above settled: the endpoint may serve (and restore its sockets).
    services.endpoint.start();

    let shared = Arc::new(Shared {
        out: Arc::new(OutQueue::new(limits.stall)),
        channels: Mutex::new(HashMap::new()),
        limits,
        endpoint: services.endpoint,
        port_policy: services.port_policy,
        reaper: services.reaper,
        capturer: Arc::new(capturer),
        restorer: Arc::new(restorer),
        requests: AtomicUsize::new(0),
    });

    let done = Arc::new((Mutex::new(false), Condvar::new()));
    {
        let shared = Arc::clone(&shared);
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            while let Some(frame) = shared.out.next() {
                shared.out.set_writing(Some(Instant::now()));
                let failed = write_frame(&mut writer, &frame).is_err();
                shared.out.set_writing(None);
                if failed {
                    // The host is gone; the read side will see EOF and exit.
                    shared.out.close();
                    break;
                }
            }
            let (flag, signal) = &*done;
            *flag
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
            signal.notify_all();
        });
    }
    if !breaks::break_mode("output-unbounded") {
        shared.out.watch_transport();
    }
    shared.out.control(AgentFrame::Ready {
        protocol: PROTOCOL,
        generation,
        pid: std::process::id(),
        test_breaks_agent: breaks::active(),
        window_chunks: WINDOW_CHUNKS,
        chunk_bytes: CHUNK_BYTES,
    });
    {
        let out = Arc::clone(&shared);
        let policy = Arc::clone(&shared.port_policy);
        ports::spawn_watcher(
            engine,
            move |workspace, container| policy.admitted(workspace, container),
            move |frame| out.out.control(frame),
        );
    }
    janitor(&shared);

    loop {
        match read_frame::<_, HostFrame>(reader, MAX_HOST_JSON_BYTES) {
            Ok(None) => {
                finish(&shared, &done);
                return 0;
            }
            Ok(Some(Inbound::Data { channel, bytes })) => handle_data(&shared, channel, bytes),
            Ok(Some(Inbound::Control(frame))) => match handle_frame(&shared, frame) {
                Ok(true) => {}
                Ok(false) => {
                    finish(&shared, &done);
                    return 0;
                }
                Err(message) => return fatal(&shared, &done, &message),
            },
            Err(error @ (FrameError::Io(_) | FrameError::Truncated)) => {
                // The transport is gone; there is nobody to tell.
                eprintln!("marsh services relay: {error}");
                finish(&shared, &done);
                return 1;
            }
            Err(error) => return fatal(&shared, &done, &error.to_string()),
        }
    }
}

/// The agent runs only in the engine container, on Linux.
#[cfg(not(target_os = "linux"))]
pub fn serve<R: Read, W: Write + Send + 'static>(
    _reader: &mut R,
    mut writer: W,
    _config: &Config,
    _make_services: impl FnOnce() -> Result<Services, String>,
) -> i32 {
    report_fatal(
        &mut writer,
        "the services agent requires Linux (the engine container)",
    );
    1
}

fn report_fatal<W: Write>(writer: &mut W, message: &str) {
    eprintln!("marsh services relay: {message}");
    let _ = write_frame(
        writer,
        &Outbound::Control(AgentFrame::Error {
            request: None,
            message: message.to_owned(),
        }),
    );
}

/// The agent's wiring: E3's filtered endpoint (its ledger lives on the pool),
/// its freeze as the capture's gate, and the ledger's published ports.
#[cfg(target_os = "linux")]
fn production_services(config: &Config, locks: Arc<ScopeLocks>) -> Result<Services, String> {
    let mut cfg = crate::policy::endpoint::Config::default();
    // The registry allowlist is the engine's configuration (`engine-up` wrote it
    // from the Internet gate). It must be readable to serve at all (fail
    // closed), and the filter re-reads it so a toggle reaches this agent.
    let engine_config = std::path::Path::new(crate::engine::ENGINE_CONFIG);
    crate::engine::EngineConfig::load(engine_config)
        .map_err(|error| format!("cannot read the engine configuration: {error}"))?
        .apply(&mut cfg);
    cfg.registry_config = Some(engine_config.to_path_buf());
    let state = config.pool.join("marsh");
    let _ = std::fs::create_dir_all(&state);
    cfg.paths.ledger = state.join("ledger.json");
    // A ledger that cannot be opened is fatal: nothing may be admitted that
    // the agent could not remember after a restart.
    let svc = crate::policy::endpoint::Services::new(cfg, &config.engine_socket)
        .map_err(|error| format!("cannot open the policy ledger: {error}"))?;
    let filtered = Arc::new(Filtered::new(Arc::clone(&svc)));
    Ok(Services {
        locks,
        freezer: Arc::clone(&filtered) as Arc<dyn Freezer>,
        reaper: Arc::clone(&filtered) as Arc<dyn Reaper>,
        endpoint: filtered,
        port_policy: Arc::new(LedgerPorts::new(svc, Engine::new(&config.engine_socket))),
    })
}

#[cfg(not(target_os = "linux"))]
fn production_services(_config: &Config, _locks: Arc<ScopeLocks>) -> Result<Services, String> {
    Err("the services agent requires Linux".into())
}

/// `marsh --internal-services relay [--pool DIR] [--engine-socket PATH]`
/// (and, in test builds only, `--break-modes a,b`).
#[must_use]
pub fn run(args: &[String]) -> i32 {
    let mut config = Config::default();
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        match (flag.as_str(), iter.next()) {
            ("--pool", Some(value)) => config.pool = PathBuf::from(value),
            ("--engine-socket", Some(value)) => config.engine_socket = PathBuf::from(value),
            #[cfg(feature = "test-breaks")]
            ("--break-modes", Some(value)) => {
                config.break_modes = value
                    .split(',')
                    .filter(|mode| !mode.is_empty())
                    .map(str::to_owned)
                    .collect();
            }
            _ => {
                eprintln!("marsh services relay: unknown argument {flag}");
                return 2;
            }
        }
    }
    // Modes are needed before `serve` (the lock table is built first).
    breaks::install(&config.break_modes);
    let locks = if breaks::break_mode("locks-process-local") {
        // Deliberately wrong (a break mode): a private, process-local table.
        ScopeLocks::in_memory()
    } else {
        match ScopeLocks::open(&config.pool.join("locks")) {
            Ok(locks) => locks,
            Err(error) => {
                eprintln!("marsh services relay: cannot create the workspace locks: {error}");
                // `serve` reports the same pool problem to the host.
                ScopeLocks::in_memory()
            }
        }
    };
    let stdin = std::io::stdin();
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, stdin.lock());
    serve(&mut reader, std::io::stdout(), &config, || {
        production_services(&config, locks)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inspect(
        status: &str,
        scope: Option<&str>,
        ports: &str,
    ) -> crate::engine_api::ContainerDetail {
        let labels = scope.map_or_else(
            || "{}".to_owned(),
            |scope| format!(r#"{{"dev.marsh.scope":"{scope}"}}"#),
        );
        serde_json::from_str(&format!(
            r#"{{"State":{{"Status":"{status}"}},"Config":{{"Labels":{labels}}},"NetworkSettings":{{"Ports":{ports}}}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn a_port_may_be_dialled_only_when_all_of_the_rule_holds() {
        let published = r#"{"5432/tcp":[{"HostIp":"127.0.0.1","HostPort":"15432"}],"9000/udp":[{"HostIp":"127.0.0.1","HostPort":"9000"}],"80/tcp":null}"#;
        for (name, detail, port, want) in [
            (
                "running, own scope, published",
                inspect("running", Some("ws"), published),
                15432,
                true,
            ),
            (
                "another port",
                inspect("running", Some("ws"), published),
                15433,
                false,
            ),
            (
                "an exposed port nobody published",
                inspect("running", Some("ws"), published),
                80,
                false,
            ),
            (
                "a UDP binding",
                inspect("running", Some("ws"), published),
                9000,
                false,
            ),
            (
                "paused",
                inspect("paused", Some("ws"), published),
                15432,
                false,
            ),
            (
                "exited",
                inspect("exited", Some("ws"), published),
                15432,
                false,
            ),
            (
                "another workspace's label",
                inspect("running", Some("other"), published),
                15432,
                false,
            ),
            (
                "no label",
                inspect("running", None, published),
                15432,
                false,
            ),
            (
                "no ports at all",
                inspect("running", Some("ws"), "null"),
                15432,
                false,
            ),
        ] {
            assert_eq!(may_dial(&detail, "ws", port), want, "{name}");
        }
    }
}
