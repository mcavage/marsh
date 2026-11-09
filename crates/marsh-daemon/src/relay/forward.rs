//! The shell VM's port forwards (E2; SECURITY-DESIGN A6, DECISIONS 19).
//!
//! The guest relay listens on `127.0.0.1` and `[::1]` **in the shell VM** for
//! each port the project's base workspace publishes, and every accepted
//! connection becomes a services channel through the shell relay. The shell VM
//! is shared by every project of the scope, so the first claimant of a port
//! wins: a later one finds the port taken, reports it, and replaces nothing.
//! Forwards do not check who connects (a documented residual, as for files).

use marsh_contracts::process::PortState;
use marsh_runtime::loopback::{BindError, bind_loopback};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

const MAX_PORTS: usize = 512;
const ACCEPT_POLL: Duration = Duration::from_millis(100);
/// Test break: a later claimant takes the port over (`shell-forward-last-claimant`).
const LAST_CLAIMANT: &str = "shell-forward-last-claimant";

pub(super) type Accept = Box<dyn Fn(TcpStream, u16) + Send + Sync>;
pub(super) type Report = Box<dyn Fn(u16, PortState) + Send + Sync>;

struct Inner {
    accept: Accept,
    report: Report,
    claims: Option<PathBuf>,
    desired: Mutex<Option<BTreeSet<u16>>>,
    changed: Condvar,
    stop: AtomicBool,
}

pub(super) struct GuestForwards {
    inner: Arc<Inner>,
    manager: Mutex<Option<thread::JoinHandle<()>>>,
}

struct Live {
    stop: Arc<AtomicBool>,
    handle: thread::JoinHandle<()>,
}

impl GuestForwards {
    pub(super) fn start(accept: Accept, report: Report, claims: Option<PathBuf>) -> Self {
        let inner = Arc::new(Inner {
            accept,
            report,
            claims,
            desired: Mutex::new(None),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let manager = {
            let inner = Arc::clone(&inner);
            thread::spawn(move || manage(&inner))
        };
        Self {
            inner,
            manager: Mutex::new(Some(manager)),
        }
    }

    /// The whole set the project publishes now; the latest wins.
    pub(super) fn set_ports(&self, ports: &[u16]) {
        let wanted = ports
            .iter()
            .copied()
            .filter(|port| *port != 0)
            .take(MAX_PORTS)
            .collect();
        *self
            .inner
            .desired
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(wanted);
        self.inner.changed.notify_all();
    }

    pub(super) fn shutdown(&self) {
        self.inner.stop.store(true, Ordering::Release);
        self.inner.changed.notify_all();
        let manager = self
            .manager
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(manager) = manager {
            let _ = manager.join();
        }
    }
}

/// How long a port that collided waits before it is tried again: its first
/// claimant may close, and then a waiting shell claims it.
const RETRY: Duration = Duration::from_secs(2);

enum Wake {
    Ports(BTreeSet<u16>),
    Retry,
    Stop,
}

fn next_wake(inner: &Inner, retrying: bool) -> Wake {
    let mut desired = inner
        .desired
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if inner.stop.load(Ordering::Acquire) {
            return Wake::Stop;
        }
        if let Some(ports) = desired.take() {
            return Wake::Ports(ports);
        }
        if retrying {
            let (guard, timeout) = inner
                .changed
                .wait_timeout(desired, RETRY)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            desired = guard;
            if timeout.timed_out() && desired.is_none() {
                return Wake::Retry;
            }
        } else {
            desired = inner
                .changed
                .wait(desired)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

fn manage(inner: &Arc<Inner>) {
    let mut live: BTreeMap<u16, Live> = BTreeMap::new();
    // A port that failed: `true` while it only collided (and is retried).
    let mut failed: BTreeMap<u16, bool> = BTreeMap::new();
    let mut wanted: BTreeSet<u16> = BTreeSet::new();
    loop {
        let retrying = match next_wake(inner, failed.values().any(|collided| *collided)) {
            Wake::Stop => break,
            Wake::Ports(ports) => {
                wanted = ports;
                false
            }
            Wake::Retry => true,
        };
        // A listener that ended by itself (taken over under a test break) is gone.
        let ended: Vec<u16> = live
            .iter()
            .filter(|(_, forward)| forward.handle.is_finished())
            .map(|(port, _)| *port)
            .collect();
        for port in ended {
            if let Some(forward) = live.remove(&port) {
                let _ = forward.handle.join();
            }
        }
        let gone: Vec<u16> = live
            .keys()
            .copied()
            .filter(|port| !wanted.contains(port))
            .collect();
        for port in gone {
            if let Some(forward) = live.remove(&port) {
                forward.stop.store(true, Ordering::Release);
                let _ = forward.handle.join();
                (inner.report)(port, PortState::Revoked);
            }
        }
        failed.retain(|port, _| wanted.contains(port));
        let add: Vec<u16> = wanted
            .iter()
            .copied()
            .filter(|port| !live.contains_key(port))
            .filter(|port| match failed.get(port) {
                None => true,
                Some(collided) => retrying && *collided,
            })
            .collect();
        for port in add {
            let stop = Arc::new(AtomicBool::new(false));
            let (ready_send, ready) = mpsc::sync_channel::<Result<bool, PortState>>(1);
            let handle = {
                let (inner, stop) = (Arc::clone(inner), Arc::clone(&stop));
                thread::spawn(move || listen(&inner, port, &stop, &ready_send))
            };
            match ready.recv() {
                Ok(Ok(ipv6)) => {
                    failed.remove(&port);
                    (inner.report)(port, PortState::Forwarding { ipv6 });
                    live.insert(port, Live { stop, handle });
                }
                Ok(Err(state)) => {
                    let collided = matches!(state, PortState::Collision { .. });
                    // Reported once; a retry that collides again stays quiet.
                    if !(retrying && failed.get(&port).copied() == Some(true)) {
                        (inner.report)(port, state);
                    }
                    failed.insert(port, collided);
                    let _ = handle.join();
                }
                Err(_) => {
                    failed.insert(port, false);
                }
            }
        }
    }
    for (_, forward) in std::mem::take(&mut live) {
        forward.stop.store(true, Ordering::Release);
        let _ = forward.handle.join();
    }
}

fn port_state(port: u16, error: BindError) -> PortState {
    match error {
        BindError::Collision { address } => PortState::Collision {
            message: format!(
                "marsh: port {port} is already in use in the shell VM ({address}); its owner keeps it and nothing was forwarded"
            ),
        },
        BindError::Failed { address, reason } => PortState::Failed {
            message: format!(
                "marsh: port {port} could not be forwarded in the shell VM ({address}): {reason}"
            ),
        },
    }
}

/// Who holds a port, only to let a test break take it over.
fn claim_path(inner: &Inner, port: u16) -> Option<PathBuf> {
    inner.claims.as_ref().map(|dir| dir.join(port.to_string()))
}

fn write_claim(inner: &Inner, port: u16) {
    if !marsh_contracts::break_mode(LAST_CLAIMANT) {
        return;
    }
    if let Some(dir) = &inner.claims {
        let _ = fs::create_dir_all(dir);
    }
    if let Some(path) = claim_path(inner, port) {
        let _ = fs::write(path, std::process::id().to_string());
    }
}

fn claimed_by_another(inner: &Inner, port: u16) -> bool {
    marsh_contracts::break_mode(LAST_CLAIMANT)
        && claim_path(inner, port)
            .and_then(|path| fs::read_to_string(path).ok())
            .is_some_and(|owner| owner.trim() != std::process::id().to_string())
}

fn listen(
    inner: &Arc<Inner>,
    port: u16,
    stop: &AtomicBool,
    ready: &mpsc::SyncSender<Result<bool, PortState>>,
) {
    let mut bound = bind_loopback(port);
    if marsh_contracts::break_mode(LAST_CLAIMANT)
        && matches!(bound, Err(BindError::Collision { .. }))
    {
        // The defect under test: the later claimant takes the port over. It
        // writes its claim; the earlier holder notices and lets go.
        write_claim(inner, port);
        let deadline = Instant::now() + Duration::from_secs(3);
        while matches!(bound, Err(BindError::Collision { .. })) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
            bound = bind_loopback(port);
        }
    }
    let (v4, v6) = match bound {
        Ok(listeners) => listeners,
        Err(error) => {
            let _ = ready.send(Err(port_state(port, error)));
            return;
        }
    };
    write_claim(inner, port);
    let _ = ready.send(Ok(v6.is_some()));
    let listeners: Vec<&TcpListener> = std::iter::once(&v4).chain(v6.as_ref()).collect();
    while !stop.load(Ordering::Acquire) && !inner.stop.load(Ordering::Acquire) {
        if claimed_by_another(inner, port) {
            return;
        }
        let mut fds: Vec<rustix::event::PollFd<'_>> = listeners
            .iter()
            .map(|listener| rustix::event::PollFd::new(*listener, rustix::event::PollFlags::IN))
            .collect();
        let timeout = rustix::event::Timespec {
            tv_sec: 0,
            tv_nsec: i64::try_from(ACCEPT_POLL.as_nanos()).unwrap_or(100_000_000),
        };
        if rustix::event::poll(&mut fds, Some(&timeout)).is_err() {
            continue;
        }
        for listener in &listeners {
            while let Ok((stream, _)) = listener.accept() {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_nodelay(true);
                (inner.accept)(stream, port);
            }
        }
    }
}

/// Joins a forwarded TCP connection to the socket pair end that the shell
/// relay's connection machinery reads and writes.
pub(super) fn pump_tcp(unix: UnixStream, tcp: TcpStream) {
    let (Ok(mut unix_reader), Ok(mut tcp_writer)) = (unix.try_clone(), tcp.try_clone()) else {
        return;
    };
    let (mut tcp_reader, mut unix_writer) = (tcp, unix);
    thread::spawn(move || {
        let mut buffer = vec![0_u8; 16 * 1024];
        loop {
            match unix_reader.read(&mut buffer) {
                Ok(0) => {
                    let _ = tcp_writer.shutdown(Shutdown::Write);
                    return;
                }
                Ok(count) => {
                    if tcp_writer.write_all(&buffer[..count]).is_err() {
                        let _ = unix_reader.shutdown(Shutdown::Both);
                        return;
                    }
                }
                Err(_) => {
                    let _ = tcp_writer.shutdown(Shutdown::Both);
                    return;
                }
            }
        }
    });
    thread::spawn(move || {
        let mut buffer = vec![0_u8; 16 * 1024];
        loop {
            match tcp_reader.read(&mut buffer) {
                Ok(0) => {
                    let _ = unix_writer.shutdown(Shutdown::Write);
                    return;
                }
                Ok(count) => {
                    if unix_writer.write_all(&buffer[..count]).is_err() {
                        let _ = tcp_reader.shutdown(Shutdown::Both);
                        return;
                    }
                }
                Err(_) => {
                    let _ = unix_writer.shutdown(Shutdown::Both);
                    return;
                }
            }
        }
    });
}
