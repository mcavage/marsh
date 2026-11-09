//! A job's port forwards (SECURITY-DESIGN A6): for each port its workspace
//! publishes, one listener on `127.0.0.1` and `[::1]` **inside that job
//! container's own network namespace**, never the worker's or the Kit VM's.
//!
//! The namespace is the one the Kit engine reports for that exact container
//! (`NetworkSettings.SandboxKey`), opened as a descriptor before any listener
//! exists. Each listener is made by a dedicated thread that first enters only
//! that namespace (`setns`, through `rustix`; no `unsafe`) and creates its
//! sockets only after that succeeded. Any failure is fatal for that port: no
//! listener is made, and no other namespace is used instead. A listener that
//! already exists in the job wins: nothing is replaced, and the collision is
//! reported. Each accepted connection becomes an endpoint channel to the
//! daemon, which opens the services VM's port for it.

use crate::{
    PortState, WorkerResponse, breaks,
    capability::{Conn, Hub},
};
use marsh_contracts::{ContainerId, process::ChannelKind};
use marsh_runtime::JobRuntime;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::TcpListener,
    os::fd::OwnedFd,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

/// At most this many forwards per job (the daemon validates the same bound).
const MAX_PORTS: usize = 512;
/// How long a listener thread waits in `poll` before it re-checks its stop flag.
const ACCEPT_POLL: Duration = Duration::from_millis(100);
/// How long the manager waits for the job's container to have a namespace.
const NAMESPACE_WAIT: Duration = Duration::from_secs(15);

struct Desired {
    ports: Option<BTreeSet<u16>>,
}

struct Inner {
    hub: Hub,
    runtime: Arc<dyn JobRuntime>,
    container: Arc<Mutex<Option<ContainerId>>>,
    desired: Mutex<Desired>,
    changed: Condvar,
    stop: AtomicBool,
}

pub(crate) struct Forwards {
    inner: Arc<Inner>,
    manager: Mutex<Option<thread::JoinHandle<()>>>,
}

/// One live listener thread.
struct Live {
    stop: Arc<AtomicBool>,
    handle: thread::JoinHandle<()>,
}

impl Forwards {
    pub(crate) fn start(
        hub: Hub,
        runtime: Arc<dyn JobRuntime>,
        container: Arc<Mutex<Option<ContainerId>>>,
    ) -> Arc<Self> {
        let inner = Arc::new(Inner {
            hub,
            runtime,
            container,
            desired: Mutex::new(Desired { ports: None }),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let manager = {
            let inner = Arc::clone(&inner);
            thread::spawn(move || manage(&inner))
        };
        Arc::new(Self {
            inner,
            manager: Mutex::new(Some(manager)),
        })
    }

    /// The whole set the job's workspace publishes now; the latest wins.
    pub(crate) fn set_ports(&self, ports: &[u16]) {
        let wanted = ports
            .iter()
            .copied()
            .filter(|port| *port != 0)
            .take(MAX_PORTS)
            .collect();
        self.inner
            .desired
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .ports = Some(wanted);
        self.inner.changed.notify_all();
    }

    /// Ends every forward and joins every thread, so no listener outlives the
    /// attempt (`listener-retain-on-end` leaves them to the process).
    pub(crate) fn shutdown(&self) {
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

fn report(inner: &Inner, port: u16, state: PortState) {
    let _ = inner.hub.output.send(&WorkerResponse::PortReport {
        attempt: inner.hub.attempt.clone(),
        port,
        state,
    });
}

/// How long a port that collided waits before it is tried again: a listener
/// the job had may go away, and then the forward takes its place.
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
        if let Some(ports) = desired.ports.take() {
            return Wake::Ports(ports);
        }
        if retrying {
            let (guard, timeout) = inner
                .changed
                .wait_timeout(desired, RETRY)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            desired = guard;
            if timeout.timed_out() && desired.ports.is_none() {
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

#[allow(clippy::too_many_lines)] // One manager loop: wake, revoke, open the namespace, listen, report.
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
        // A forward is revoked when its port stops being published.
        if !breaks::on(breaks::PORTS_KEEP_STALE) {
            let gone: Vec<u16> = live
                .keys()
                .copied()
                .filter(|port| !wanted.contains(port))
                .collect();
            for port in gone {
                if let Some(forward) = live.remove(&port) {
                    forward.stop.store(true, Ordering::Release);
                    let _ = forward.handle.join();
                    report(inner, port, PortState::Revoked);
                }
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
        if add.is_empty() {
            continue;
        }
        // Each batch opens the namespace of the job's container as it is now: a
        // publish after the container was recreated must not forward into the
        // old one (E2 review, suggestion 1).
        let fd = match open_namespace(inner) {
            Ok(fd) => Arc::new(fd),
            Err(message) => {
                for port in add {
                    report(
                        inner,
                        port,
                        PortState::Failed {
                            message: message.clone(),
                        },
                    );
                    failed.insert(port, false);
                }
                continue;
            }
        };
        for port in add {
            let stop = Arc::new(AtomicBool::new(false));
            let (ready_send, ready) = mpsc::sync_channel::<Result<bool, PortState>>(1);
            let handle = {
                let (inner, fd, stop) = (Arc::clone(inner), Arc::clone(&fd), Arc::clone(&stop));
                thread::spawn(move || listen(&inner, port, &fd, &stop, &ready_send))
            };
            match ready.recv() {
                Ok(Ok(ipv6)) => {
                    failed.remove(&port);
                    report(inner, port, PortState::Forwarding { ipv6 });
                    live.insert(port, Live { stop, handle });
                }
                Ok(Err(state)) => {
                    let collided = matches!(state, PortState::Collision { .. });
                    // Reported once; a retry that collides again stays quiet.
                    if !(retrying && failed.get(&port).copied() == Some(true)) {
                        report(inner, port, state);
                    }
                    failed.insert(port, collided);
                    let _ = handle.join();
                }
                Err(_) => {
                    report(
                        inner,
                        port,
                        PortState::Failed {
                            message: "the forward's thread ended before it listened".into(),
                        },
                    );
                    failed.insert(port, false);
                }
            }
        }
    }
    // The attempt is ending: every listener closes now.
    if breaks::on(breaks::LISTENER_RETAIN_ON_END) {
        // Test break: the listeners are left behind with their threads.
        std::mem::forget(live);
    } else {
        for (_, forward) in std::mem::take(&mut live) {
            forward.stop.store(true, Ordering::Release);
            let _ = forward.handle.join();
        }
    }
}

/// The job container's network namespace as an open descriptor, from the Kit
/// engine's inspect of that exact container.
fn open_namespace(inner: &Inner) -> Result<OwnedFd, String> {
    let deadline = Instant::now() + NAMESPACE_WAIT;
    let container = loop {
        if let Some(container) = inner
            .container
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            break container;
        }
        if inner.stop.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(
                "marsh: the job's container is not known yet; no port was forwarded".into(),
            );
        }
        thread::sleep(Duration::from_millis(20));
    };
    let key = loop {
        match inner.runtime.network_namespace(&container) {
            Ok(Some(key)) => break key,
            Ok(None) if Instant::now() < deadline && !inner.stop.load(Ordering::Acquire) => {
                thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                return Err(
                    "marsh: the job's container has no network namespace; no port was forwarded"
                        .into(),
                );
            }
            Err(error) => {
                return Err(format!(
                    "marsh: cannot read the job container's network namespace ({error}); no port was forwarded"
                ));
            }
        }
    };
    breaks::hold("netns-attach", &|| inner.stop.load(Ordering::Acquire));
    rustix::fs::open(
        &key,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map_err(|error| {
        format!(
            "marsh: cannot open the job container's network namespace ({error}); no port was forwarded"
        )
    })
}

/// Enters exactly the job's namespace on this thread. The kernel checks that
/// the descriptor is a network namespace.
#[cfg(target_os = "linux")]
fn enter_namespace(fd: &OwnedFd) -> Result<(), String> {
    use std::os::fd::AsFd as _;
    if breaks::on(breaks::FAULT_SETNS_FAIL) {
        return Err("setns failed: Operation not permitted (os error 1)".into());
    }
    rustix::thread::move_into_link_name_space(
        fd.as_fd(),
        Some(rustix::thread::LinkNameSpaceType::Network),
    )
    .map_err(|error| format!("setns failed: {error}"))
}

#[cfg(not(target_os = "linux"))]
fn enter_namespace(_fd: &OwnedFd) -> Result<(), String> {
    Err("network namespaces need Linux".into())
}

fn port_state(port: u16, error: marsh_runtime::loopback::BindError) -> PortState {
    use marsh_runtime::loopback::BindError;
    match error {
        BindError::Collision { address } => PortState::Collision {
            message: format!(
                "marsh: port {port} is already in use in this job ({address}); its own listener keeps it and nothing was forwarded"
            ),
        },
        BindError::Failed { address, reason } => PortState::Failed {
            message: format!(
                "marsh: port {port} could not be forwarded in this job ({address}): {reason}"
            ),
        },
    }
}

/// One forward: enter the job's namespace, make the listeners, accept.
fn listen(
    inner: &Arc<Inner>,
    port: u16,
    namespace: &OwnedFd,
    stop: &AtomicBool,
    ready: &mpsc::SyncSender<Result<bool, PortState>>,
) {
    // Test break: the sockets are made before the namespace is entered, so
    // they live in the worker's own namespace (and stay there on a fault).
    let early = if breaks::on(breaks::LISTENER_BEFORE_SETNS) {
        Some(marsh_runtime::loopback::bind_loopback(port).map_err(|error| port_state(port, error)))
    } else {
        None
    };
    if let Err(message) = enter_namespace(namespace) {
        let _ = ready.send(Err(PortState::Failed {
            message: format!("marsh: port {port} was not forwarded in this job: {message}"),
        }));
        if let Some(Ok(leaked)) = early {
            // The defect under test: the listener outlives the failure.
            while !stop.load(Ordering::Acquire) && !inner.stop.load(Ordering::Acquire) {
                thread::sleep(ACCEPT_POLL);
            }
            drop(leaked);
        }
        return;
    }
    let bound = early.unwrap_or_else(|| {
        marsh_runtime::loopback::bind_loopback(port).map_err(|error| port_state(port, error))
    });
    let (v4, v6) = match bound {
        Ok(listeners) => listeners,
        Err(state) => {
            let _ = ready.send(Err(state));
            return;
        }
    };
    let _ = ready.send(Ok(v6.is_some()));
    let listeners: Vec<&TcpListener> = std::iter::once(&v4).chain(v6.as_ref()).collect();
    // Test break `listener-retain-on-end`: the attempt's end does not close it.
    let retain = breaks::on(breaks::LISTENER_RETAIN_ON_END);
    while !stop.load(Ordering::Acquire) && (retain || !inner.stop.load(Ordering::Acquire)) {
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
                inner
                    .hub
                    .admit(Conn::Tcp(stream), Some(ChannelKind::Tcp { port }));
            }
        }
    }
}
