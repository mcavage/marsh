//! The per-attempt job capability (`docs/design/processes.md` s3-s4).
//!
//! Every Kit job gets `/run/marsh`, built by the worker in
//! `/run/marsh-cap/<attempt>/` and bound read-only into that container only:
//! `cap.sock`, `context.md`, `job.json` (written by the runtime, which knows
//! the starting environment), a mountpoint for the static artifact, and
//! `bin/` with one two-line stub per link. The socket has no token: each
//! connection is forwarded as attempt-tagged frames over the worker
//! transport, and the daemon admits only this job's requests. Excess
//! connections get a refusal frame. Everything dies with the attempt.

use crate::{WorkerResponse, transport::Publisher};
use marsh_contracts::{
    kit_assets::{KitContext, MAX_CONTEXT_BYTES},
    process::{
        ChannelKind, ENDPOINT_CHANNEL_LIMIT, ENDPOINT_CHUNK, ENDPOINT_CREDIT_BATCH,
        ENDPOINT_WINDOW, JobCapability,
    },
};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::{Shutdown, TcpStream},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread,
    time::Duration,
};

pub(crate) const ROOT: &str = "/run/marsh-cap";

/// At most this many open connections per attempt (`MaxFan + 4`).
const MAX_CHANNELS: usize = marsh_contracts::process::CONNECTION_LIMIT;
/// Daemon-to-job chunks queued per connection before it is closed.
const QUEUED_CHUNKS: usize = 64;

/// One accepted connection: a Unix socket (`cap.sock`, `docker.sock`) or a
/// forwarded port's TCP stream.
pub(crate) enum Conn {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Conn {
    fn try_clone(&self) -> std::io::Result<Self> {
        Ok(match self {
            Self::Unix(stream) => Self::Unix(stream.try_clone()?),
            Self::Tcp(stream) => Self::Tcp(stream.try_clone()?),
        })
    }

    fn shutdown(&self, how: Shutdown) -> std::io::Result<()> {
        match self {
            Self::Unix(stream) => stream.shutdown(how),
            Self::Tcp(stream) => stream.shutdown(how),
        }
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        match self {
            Self::Unix(stream) => stream.set_write_timeout(timeout),
            Self::Tcp(stream) => stream.set_write_timeout(timeout),
        }
    }
}

impl Read for Conn {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.read(buffer),
            Self::Tcp(stream) => stream.read(buffer),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.write(bytes),
            Self::Tcp(stream) => stream.write(bytes),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Unix(stream) => stream.flush(),
            Self::Tcp(stream) => stream.flush(),
        }
    }
}

/// Chunks of credit for one direction of an endpoint channel.
pub(crate) struct Window {
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

    /// Takes one chunk of credit; `false` once the window is closed.
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

/// One connection's writer: a bounded queue drained by its own thread, so a
/// job that stops reading never blocks the worker transport.
pub(crate) struct Channel {
    /// `None` once the daemon half-closed an endpoint channel.
    queue: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    stream: Conn,
    /// `None` for a `cap.sock` connection; the endpoint kinds are credited.
    kind: Option<ChannelKind>,
    /// Credit for the job's bytes towards the daemon.
    window: Arc<Window>,
    job_eof: bool,
    daemon_eof: bool,
}

impl Channel {
    /// Ends an endpoint or capability connection at once.
    fn discard(self) {
        let _ = self.stream.shutdown(Shutdown::Both);
        self.window.close();
    }
}

pub(crate) type Streams = Arc<Mutex<BTreeMap<(String, u32), Channel>>>;

/// The attempts whose port forwards the worker serves, by attempt.
pub(crate) type Forwarders = Arc<Mutex<BTreeMap<String, Arc<crate::forward::Forwards>>>>;

pub(crate) struct Capability {
    dir: PathBuf,
    attempt: String,
    stop: Arc<AtomicBool>,
    streams: Streams,
    accepts: Vec<(thread::JoinHandle<()>, PathBuf)>,
    forwarders: Forwarders,
    forwards: Option<Arc<crate::forward::Forwards>>,
}

/// The refusal an excess connection reads before its EOF: the daemon's
/// `PublicReply::Error` wire frame (the worker does not link the daemon).
fn refusal_frame() -> Vec<u8> {
    let body = serde_json::json!({
        "type": "error",
        "code": "invalid_request",
        "message": marsh_contracts::process::CONNECTION_REFUSAL,
    })
    .to_string();
    let mut frame = u32::try_from(body.len())
        .unwrap_or(0)
        .to_be_bytes()
        .to_vec();
    frame.extend_from_slice(body.as_bytes());
    frame
}

fn docker_error(status: &str, message: &str) -> Vec<u8> {
    marsh_contracts::process::docker_error_response(status, message)
}

/// Admits connections of one attempt: `cap.sock`, `docker.sock` and the
/// forwarded ports share one channel numbering and one table.
#[derive(Clone)]
pub(crate) struct Hub {
    pub(crate) attempt: String,
    streams: Streams,
    pub(crate) output: Publisher,
    next: Arc<AtomicU32>,
}

impl Hub {
    fn new(attempt: &str, streams: Streams, output: Publisher) -> Self {
        Self {
            attempt: attempt.to_owned(),
            streams,
            output,
            next: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Registers one accepted connection and starts its pump. `kind` is
    /// `None` for `cap.sock`.
    #[allow(clippy::too_many_lines)] // One admission: limit, writer thread, table, open frame, pump.
    pub(crate) fn admit(&self, conn: Conn, kind: Option<ChannelKind>) {
        let endpoint = kind.is_some();
        let (limit, open) = {
            let streams = self
                .streams
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                if endpoint {
                    ENDPOINT_CHANNEL_LIMIT
                } else {
                    MAX_CHANNELS
                },
                streams
                    .iter()
                    .filter(|((owner, _), channel)| {
                        *owner == self.attempt && channel.kind.is_some() == endpoint
                    })
                    .count(),
            )
        };
        let mut conn = conn;
        if open >= limit {
            // Never a silent drop: the excess connection reads one refusal.
            let _ = conn.set_write_timeout(Some(Duration::from_secs(1)));
            match kind {
                None => {
                    let _ = conn.write_all(&refusal_frame());
                }
                Some(ChannelKind::Docker) => {
                    let _ = conn.write_all(&docker_error(
                        "429 Too Many Requests",
                        &format!(
                            "marsh: too many open connections to the Docker endpoint in this job (limit {ENDPOINT_CHANNEL_LIMIT})"
                        ),
                    ));
                }
                Some(ChannelKind::Tcp { .. }) => {}
            }
            let _ = conn.shutdown(Shutdown::Both);
            return;
        }
        let channel = self.next.fetch_add(1, Ordering::AcqRel) + 1;
        let (Ok(mut writer), Ok(handle)) = (conn.try_clone(), conn.try_clone()) else {
            return;
        };
        let (queue, chunks) = std::sync::mpsc::sync_channel::<Vec<u8>>(QUEUED_CHUNKS);
        let window = Arc::new(Window::new(ENDPOINT_WINDOW));
        let (credit_output, credit_attempt) = (self.output.clone(), self.attempt.clone());
        thread::spawn(move || {
            let mut written = 0_u32;
            for chunk in chunks {
                if writer.write_all(&chunk).is_err() {
                    break;
                }
                if endpoint {
                    written += 1;
                    if written.is_multiple_of(ENDPOINT_CREDIT_BATCH) {
                        // Never the non-blocking `control` path: a credit lost to a
                        // momentarily full spool would stall the channel for good.
                        let _ = credit_output.send(&WorkerResponse::ChanCredit {
                            attempt: credit_attempt.clone(),
                            channel,
                            chunks: ENDPOINT_CREDIT_BATCH,
                        });
                    }
                }
            }
            // The daemon closed: deliver EOF after the data.
            let _ = writer.shutdown(Shutdown::Write);
        });
        self.streams
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                (self.attempt.clone(), channel),
                Channel {
                    queue: Some(queue),
                    stream: handle,
                    kind,
                    window: Arc::clone(&window),
                    job_eof: false,
                    daemon_eof: false,
                },
            );
        let opened = match kind {
            None => self.output.control(&WorkerResponse::CapOpen {
                attempt: self.attempt.clone(),
                channel,
            }),
            // Blocking publication: an endpoint channel is never opened half way.
            Some(kind) => self.output.send(&WorkerResponse::ChanOpen {
                attempt: self.attempt.clone(),
                channel,
                kind,
            }),
        };
        if opened.is_err() && endpoint {
            let removed = self
                .streams
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&(self.attempt.clone(), channel));
            if let Some(open) = removed {
                open.discard();
            }
            return;
        }
        pump(conn, self.clone(), channel, kind, window);
    }
}

/// Write the read-only parts of `/run/marsh`: links, context, mountpoint.
fn publish(dir: &Path, capability: &JobCapability) -> std::io::Result<()> {
    let bin = dir.join("bin");
    fs::create_dir(&bin)?;
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755))?;
    // `bash` and `sh` are never links: a job's shells are the image's own.
    let mut names = vec!["marsh".to_owned()];
    names.extend(
        capability
            .registered
            .iter()
            .filter(|name| valid_link(name))
            .cloned(),
    );
    // Web links open on the Mac (`marsh open`) unless a Kit has the name.
    for helper in marsh_contracts::process::OPEN_LINKS {
        if !names.iter().any(|name| name == helper) {
            names.push(helper.to_owned());
        }
    }
    for name in names {
        let stub = bin.join(&name);
        fs::write(
            &stub,
            format!("#!{} --link={name}\n", marsh_contracts::process::ARTIFACT),
        )?;
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755))?;
    }
    let context = dir.join("context.md");
    let mut text = marsh_contracts::process::job_context(&capability.name, &capability.spawn);
    // Peer review (`docs/peer-review.md`): present only while it is on, and
    // first after the opening paragraph; at the end, agents skipped it.
    if !capability.peer_review.trim().is_empty() {
        let section = capability.peer_review.trim_end();
        if let Some(at) = text.find("\n## ") {
            text.insert_str(at + 1, &format!("{section}\n\n"));
        } else {
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push('\n');
            text.push_str(section);
            text.push('\n');
        }
    }
    if capability.services.is_some() {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push('\n');
        text.push_str(marsh_contracts::process::DOCKER_CONTEXT);
    }
    for body in &capability.kit.contexts {
        if let Some(body) = kit_context(body) {
            if !text.ends_with('\n') {
                text.push('\n');
            }
            text.push('\n');
            text.push_str(body.trim_end());
            text.push('\n');
        }
    }
    fs::write(&context, text)?;
    fs::set_permissions(&context, fs::Permissions::from_mode(0o644))?;
    // The mountpoint Docker binds the artifact over (the runtime refuses a
    // job when the artifact is missing); unbound, it is not executable.
    let artifact = dir.join("marsh");
    fs::write(&artifact, b"")?;
    fs::set_permissions(&artifact, fs::Permissions::from_mode(0o000))?;
    Ok(())
}

/// One Kit instruction body. Files are read from this container, which runs
/// the job's own image; the worker is root, so only regular, world-readable
/// files qualify (a Kit cannot use this to show a job a root-only file).
fn kit_context(body: &KitContext) -> Option<String> {
    use rustix::fs::{Mode, OFlags};
    let path = match body {
        KitContext::Text(text) => return Some(text.clone()),
        KitContext::File(path) => path,
    };
    let file = fs::File::from(
        rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )
        .ok()?,
    );
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o004 == 0 {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONTEXT_BYTES as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    String::from_utf8(bytes).ok()
}

fn valid_link(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !matches!(name, "marsh" | "bash" | "sh" | "." | "..")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// One blocking accept loop for one of the attempt's sockets.
fn accept_loop(listener: &UnixListener, hub: &Hub, stop: &AtomicBool, kind: Option<ChannelKind>) {
    // Blocking accept; `Drop` wakes it with one local connection.
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let _ = stream.set_nonblocking(false);
                hub.admit(Conn::Unix(stream), kind);
            }
            Err(_) => thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// A socket in the capability directory, owned by the job's uid and gid, 0600.
fn bind_private(path: &Path, uid: u32, gid: u32) -> std::io::Result<UnixListener> {
    let listener = UnixListener::bind(path)?;
    std::os::unix::fs::chown(path, Some(uid), Some(gid))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

impl Capability {
    #[allow(clippy::too_many_arguments)] // One attempt's whole endpoint; each is a distinct input.
    pub(crate) fn start(
        attempt: &str,
        uid: u32,
        gid: u32,
        output: Publisher,
        streams: Streams,
        capability: &JobCapability,
        forwarders: Forwarders,
        runtime: Arc<dyn marsh_runtime::JobRuntime>,
        container: Arc<Mutex<Option<marsh_contracts::ContainerId>>>,
    ) -> std::io::Result<Self> {
        let dir = Path::new(ROOT).join(attempt);
        fs::create_dir_all(ROOT)?;
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755))?;
        publish(&dir, capability)?;
        let stop = Arc::new(AtomicBool::new(false));
        let hub = Hub::new(attempt, Arc::clone(&streams), output);
        let socket = dir.join("cap.sock");
        let mut sockets = vec![(bind_private(&socket, uid, gid)?, socket, None)];
        // A job of a services-enabled project also gets the filtered Docker
        // endpoint and its port forwards (E2); every other job gets neither.
        let forwards = if let Some(endpoint) = &capability.services {
            marsh_contracts::test_breaks::attest_guest(&endpoint.breaks);
            let socket = dir.join("docker.sock");
            sockets.push((
                bind_private(&socket, uid, gid)?,
                socket,
                Some(ChannelKind::Docker),
            ));
            let forwards = crate::forward::Forwards::start(hub.clone(), runtime, container);
            forwarders
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(attempt.to_owned(), Arc::clone(&forwards));
            Some(forwards)
        } else {
            None
        };
        let accepts = sockets
            .into_iter()
            .map(|(listener, socket, kind)| {
                let (hub, stop) = (hub.clone(), Arc::clone(&stop));
                (
                    thread::spawn(move || accept_loop(&listener, &hub, &stop, kind)),
                    socket,
                )
            })
            .collect();
        Ok(Self {
            dir,
            attempt: attempt.to_owned(),
            stop,
            streams,
            accepts,
            forwarders,
            forwards,
        })
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }
}

/// The job's bytes to the daemon. A `cap.sock` slot stays counted until the
/// daemon side closes (`close`), never just because the job hung up; an
/// endpoint channel sends credit-limited chunks and a half close.
fn pump(mut stream: Conn, hub: Hub, channel: u32, kind: Option<ChannelKind>, window: Arc<Window>) {
    thread::spawn(move || {
        let endpoint = kind.is_some();
        let mut buffer = vec![0_u8; if endpoint { ENDPOINT_CHUNK } else { 64 * 1024 }];
        let clean = loop {
            if endpoint && !window.acquire() {
                break false;
            }
            match stream.read(&mut buffer) {
                Ok(0) => break true,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                    if endpoint {
                        window.add(1);
                    }
                }
                Err(_) => break false,
                Ok(count) => {
                    let data = WorkerResponse::CapData {
                        attempt: hub.attempt.clone(),
                        channel,
                        bytes: buffer[..count].to_vec(),
                    };
                    // An endpoint channel's bytes wait for room in the transport
                    // (backpressure on the job); `control` would fail the moment the
                    // spool is full and cut the connection mid-transfer.
                    let published = if endpoint {
                        hub.output.send(&data)
                    } else {
                        hub.output.control(&data)
                    };
                    if published.is_err() {
                        break false;
                    }
                }
            }
        };
        if endpoint && clean {
            // The job closed its writing side: tell the daemon, and keep the
            // channel until the daemon's side is done too.
            {
                let mut streams = hub
                    .streams
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let key = (hub.attempt.clone(), channel);
                let done = streams.get_mut(&key).is_some_and(|open| {
                    open.job_eof = true;
                    open.daemon_eof
                });
                // Both sides are done: the entry goes, but the writer thread still
                // delivers what is queued and then closes (never a `shutdown(Both)`
                // here: the job may not have read it all yet).
                if done {
                    streams.remove(&key);
                }
            }
            let _ = hub.output.send(&WorkerResponse::ChanEof {
                attempt: hub.attempt.clone(),
                channel,
            });
            return;
        }
        if endpoint {
            let key = (hub.attempt.clone(), channel);
            let removed = hub
                .streams
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&key);
            if let Some(open) = removed {
                open.discard();
            }
        }
        let _ = hub.output.control(&WorkerResponse::CapClose {
            attempt: hub.attempt.clone(),
            channel,
        });
    });
}

/// Daemon-to-job bytes for one connection; a connection whose job stopped
/// reading (queue full) is closed rather than stalling the transport.
pub(crate) fn deliver(streams: &Streams, attempt: &str, channel: u32, bytes: &[u8]) {
    let mut streams = streams
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (attempt.to_owned(), channel);
    if let Some(open) = streams.get(&key)
        && open
            .queue
            .as_ref()
            .is_none_or(|queue| queue.try_send(bytes.to_vec()).is_err())
        && let Some(open) = streams.remove(&key)
    {
        open.discard();
    }
}

/// The daemon side closed: its queued bytes drain, then the job sees EOF. An
/// endpoint channel's close is an abort: nothing more is delivered either way.
pub(crate) fn close(streams: &Streams, attempt: &str, channel: u32) {
    let removed = streams
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&(attempt.to_owned(), channel));
    if let Some(open) = removed
        && open.kind.is_some()
    {
        open.discard();
    }
}

/// The daemon will send no more on an endpoint channel: the job's connection
/// gets EOF after the queued data, and the channel ends once the job's side is done.
pub(crate) fn daemon_eof(streams: &Streams, attempt: &str, channel: u32) {
    let mut streams = streams
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (attempt.to_owned(), channel);
    let done = streams.get_mut(&key).is_some_and(|open| {
        open.queue = None;
        open.daemon_eof = true;
        open.job_eof
    });
    // The writer thread drains the queue and half-closes; the job's side is
    // already over, so the entry just goes.
    if done {
        streams.remove(&key);
    }
}

/// Credit for the job's bytes: the daemon passed that many chunks on.
pub(crate) fn credit(streams: &Streams, attempt: &str, channel: u32, chunks: u32) {
    if let Some(open) = streams
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&(attempt.to_owned(), channel))
    {
        open.window.add(chunks);
    }
}

/// The daemon could not open an endpoint channel: a Docker connection reads an
/// HTTP error with the reason, a port forward just closes. Either way the
/// connection ends.
pub(crate) fn refuse(streams: &Streams, attempt: &str, channel: u32, message: &str) {
    let mut streams = streams
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (attempt.to_owned(), channel);
    let Some(open) = streams.get_mut(&key) else {
        return;
    };
    if matches!(open.kind, Some(ChannelKind::Docker))
        && let Some(queue) = &open.queue
    {
        let _ = queue.try_send(docker_error("503 Service Unavailable", message));
    }
    open.queue = None;
    open.daemon_eof = true;
    // The job's request is not read any further.
    let _ = open.stream.shutdown(Shutdown::Read);
}

impl Drop for Capability {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(forwards) = self.forwards.take() {
            self.forwarders
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.attempt);
            forwards.shutdown();
        }
        for (accept, socket) in std::mem::take(&mut self.accepts) {
            let _ = UnixStream::connect(&socket);
            let _ = accept.join();
        }
        let closing = {
            let mut streams = self
                .streams
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let keys = streams
                .keys()
                .filter(|(attempt, _)| *attempt == self.attempt)
                .cloned()
                .collect::<Vec<_>>();
            keys.iter()
                .filter_map(|key| streams.remove(key))
                .collect::<Vec<_>>()
        };
        for open in closing {
            open.discard();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use marsh_contracts::kit_assets::KitAssets;

    #[test]
    fn publishes_kit_instructions() {
        let root = std::env::temp_dir().join(format!("marsh-kit-assets-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dir = root.join("attempt");
        fs::create_dir_all(&dir).unwrap();
        let readable = root.join("context.md");
        fs::write(&readable, "From the mixin.\n").unwrap();
        fs::set_permissions(&readable, fs::Permissions::from_mode(0o644)).unwrap();
        let private = root.join("private.md");
        fs::write(&private, "root only").unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o600)).unwrap();
        let capability = JobCapability {
            name: "pix".into(),
            kit: KitAssets {
                skill_destinations: vec!["/opt/a".into()],
                contexts: vec![
                    KitContext::Text("Inline.".into()),
                    KitContext::File(readable),
                    KitContext::File(private),
                    KitContext::File(root.join("missing.md")),
                ],
            },
            peer_review: "## Second opinion\nAsk codex.\n".into(),
            ..JobCapability::default()
        };
        publish(&dir, &capability).unwrap();
        let context = fs::read_to_string(dir.join("context.md")).unwrap();
        let base = marsh_contracts::process::job_context("pix", &[]);
        let (intro, rest) = base.split_at(base.find("\n## ").unwrap() + 1);
        assert!(
            context.starts_with(&format!("{intro}## Second opinion\nAsk codex.\n\n{rest}")),
            "{context}"
        );
        assert!(
            context.ends_with("\n\nInline.\n\nFrom the mixin.\n"),
            "{context}"
        );
        assert!(!context.contains("root only"));
        assert!(!dir.join("skills").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_job_with_docker_gets_the_docker_section_and_one_without_does_not() {
        let root = std::env::temp_dir().join(format!("marsh-docker-ctx-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for (docker, name) in [(true, "with"), (false, "without")] {
            let dir = root.join(name);
            fs::create_dir_all(&dir).unwrap();
            let capability = JobCapability {
                name: "pix".into(),
                services: docker.then(marsh_contracts::process::ServicesEndpoint::default),
                ..JobCapability::default()
            };
            publish(&dir, &capability).unwrap();
            let context = fs::read_to_string(dir.join("context.md")).unwrap();
            assert_eq!(context.contains("\n## Docker\n"), docker, "{context}");
            assert_eq!(context.contains("`docker compose`"), docker);
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_kit_without_assets_publishes_only_the_capability() {
        let root = std::env::temp_dir().join(format!("marsh-kit-none-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let capability = JobCapability {
            name: "shell".into(),
            ..JobCapability::default()
        };
        publish(&root, &capability).unwrap();
        for name in ["marsh", "xdg-open", "open"] {
            assert_eq!(
                fs::read_to_string(root.join("bin").join(name)).unwrap(),
                format!("#!{} --link={name}\n", marsh_contracts::process::ARTIFACT)
            );
        }
        assert_eq!(
            fs::read_to_string(root.join("context.md")).unwrap(),
            marsh_contracts::process::job_context("shell", &[])
        );
        fs::remove_dir_all(&root).unwrap();
    }
}
