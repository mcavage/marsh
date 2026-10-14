//! Resident host broker and bounded stdio proxy for stock-SBX MCP clients.

use futures::{SinkExt, StreamExt, future};
use rmcp::{
    RoleServer, ServiceExt,
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::async_rw::JsonRpcMessageCodec,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{Notify, Semaphore},
};
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec, LinesCodecError};
use uuid::Uuid;

use crate::{HostConfig, HostMcp, SESSION_SHUTDOWN_TIMEOUT, prepare_scope_root};

const PROTOCOL_VERSION: u32 = 1;
const MAX_HANDSHAKE_BYTES: usize = 16 * 1024;
pub const MAX_MCP_FRAME_BYTES: usize = 1_048_576;
const MAX_SESSIONS: usize = 16;
const MAX_CONNECTIONS: usize = 32;
const START_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a stop waits for sessions whose clients have gone to finish their
/// cleanup: the session shutdown bound plus time to release the session.
const STOP_ADMISSION_WAIT: Duration = SESSION_SHUTDOWN_TIMEOUT.saturating_add(HANDSHAKE_TIMEOUT);

#[derive(Clone, Debug)]
pub struct BrokerOptions {
    pub workspace: PathBuf,
    pub home: PathBuf,
    pub scope_root: PathBuf,
    pub marsh: PathBuf,
    pub sbx: PathBuf,
    pub allow_full_sbx_control: bool,
}

impl BrokerOptions {
    /// Construct the pinned shared host state owned by the broker.
    ///
    /// # Errors
    /// Returns an error when any configured path or trust anchor is invalid.
    pub fn host_config(&self) -> Result<HostConfig, String> {
        HostConfig::new_with_scope_root(
            &self.workspace,
            &self.home,
            &self.scope_root,
            &self.marsh,
            &self.sbx,
            self.allow_full_sbx_control,
        )
    }

    fn fingerprint(&self) -> Result<String, String> {
        let mut digest = Sha256::new();
        digest.update(b"marsh-mcp-broker-config-v1\0");
        for (label, path, hash_file) in [
            ("workspace", &self.workspace, false),
            ("scope_root", &self.scope_root, false),
            ("marsh", &self.marsh, true),
            ("sbx", &self.sbx, true),
        ] {
            digest.update(label.as_bytes());
            digest.update(b"\0");
            let path = if path.exists() {
                path.canonicalize()
                    .map_err(|error| format!("cannot canonicalize {}: {error}", path.display()))?
            } else {
                path.clone()
            };
            digest.update(path.as_os_str().as_encoded_bytes());
            digest.update(b"\0");
            if hash_file {
                hash_path(&path, &mut digest)?;
            }
        }
        let marshd = self
            .marsh
            .parent()
            .ok_or_else(|| "marsh executable has no parent".to_owned())?
            .join("marshd");
        hash_path(&marshd, &mut digest)?;
        digest.update([u8::from(self.allow_full_sbx_control)]);
        Ok(marsh_contracts::process::hex(&digest.finalize()))
    }

    fn endpoint(&self) -> PathBuf {
        let mut digest = Sha256::new();
        digest.update(self.scope_root.as_os_str().as_encoded_bytes());
        let key = marsh_contracts::process::hex(&digest.finalize());
        PathBuf::from("/tmp")
            .join(format!(
                "marsh-mcp-{}-{}",
                rustix::process::geteuid().as_raw(),
                &key[..16]
            ))
            .join("broker.sock")
    }

    fn token_path(&self) -> PathBuf {
        self.endpoint()
            .parent()
            .expect("derived endpoint has a parent")
            .join("auth-token")
    }

    fn pid_path(&self) -> PathBuf {
        self.endpoint()
            .parent()
            .expect("derived endpoint has a parent")
            .join("broker.pid")
    }

    fn log_path(&self) -> PathBuf {
        self.endpoint()
            .parent()
            .expect("derived endpoint has a parent")
            .join("broker.log")
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Handshake {
    protocol: u32,
    build: String,
    fingerprint: String,
    token: String,
    purpose: HandshakePurpose,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum HandshakePurpose {
    Probe,
    Session,
    Stop,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HandshakeReply {
    ok: bool,
    error: Option<String>,
    pid: u32,
}

/// What a client asserts in its handshake: the build of this marsh-mcp and
/// the configuration fingerprint. Both hash whole executables, so one command
/// computes them once and reuses them for every connection it makes.
struct Identity {
    build: String,
    fingerprint: String,
}

impl Identity {
    fn of(options: &BrokerOptions) -> Result<Self, String> {
        Ok(Self {
            build: build_id()?,
            fingerprint: options.fingerprint()?,
        })
    }
}

#[derive(Clone)]
struct BrokerContext {
    expected_build: String,
    expected_fingerprint: String,
    expected_token: String,
    session_permits: Arc<Semaphore>,
    sessions: Arc<AdmittedSessions>,
    stopping: Arc<AtomicBool>,
    shutdown: Arc<Notify>,
}

/// The connection of every session holding a permit. Admission and stop
/// admission take the lock, so a stop sees every permit holder.
#[derive(Default)]
struct AdmittedSessions {
    connections: Mutex<BTreeMap<u64, OwnedFd>>,
    next: AtomicU64,
}

/// A session's permit and its registered connection, released together.
struct AdmittedSession {
    sessions: Arc<AdmittedSessions>,
    id: u64,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for AdmittedSession {
    fn drop(&mut self) {
        self.sessions
            .connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
}

impl AdmittedSessions {
    /// `Ok(Err(reason))` refuses the session.
    fn admit(
        self: &Arc<Self>,
        context: &BrokerContext,
        stream: &UnixStream,
    ) -> Result<Result<AdmittedSession, String>, String> {
        let mut connections = self
            .connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Under the lock: a stop that has looked at the admitted sessions is
        // never followed by another admission.
        if context.stopping.load(Ordering::SeqCst) {
            return Ok(Err("resident MCP broker is stopping".into()));
        }
        let Ok(permit) = Arc::clone(&context.session_permits).try_acquire_owned() else {
            return Ok(Err(format!(
                "broker session capacity ({MAX_SESSIONS}) is busy; retry later"
            )));
        };
        // Close-on-exec atomically: sessions spawn host commands.
        let connection = rustix::io::fcntl_dupfd_cloexec(stream.as_fd(), 0)
            .map_err(|error| format!("cannot retain MCP session connection: {error}"))?;
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        connections.insert(id, connection);
        Ok(Ok(AdmittedSession {
            sessions: Arc::clone(self),
            id,
            _permit: permit,
        }))
    }

    /// Whether any session's client is still connected. A client that has
    /// closed its end has left: its session is only finishing its cleanup.
    fn any_client_connected(connections: &BTreeMap<u64, OwnedFd>) -> bool {
        connections.values().any(|connection| {
            let mut byte = [0_u8; 1];
            match rustix::net::recv(
                connection,
                &mut byte,
                rustix::net::RecvFlags::PEEK | rustix::net::RecvFlags::DONTWAIT,
            ) {
                // End of stream: the client closed its end.
                Ok((0, _)) => false,
                // Pending bytes, or nothing yet on an open connection.
                Ok(_) => true,
                Err(error) => error == rustix::io::Errno::AGAIN,
            }
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
enum StopAdmissionError {
    AlreadyStopping,
    SessionsAttached,
}

/// Ensure the exact resident broker is running and authenticated.
///
/// # Errors
/// Returns an error when startup fails or a resident broker has incompatible
/// configuration, protocol, or build identity.
pub async fn broker_start(options: BrokerOptions) -> Result<(), String> {
    prepare_scope_root(&options.scope_root)?;
    let endpoint = options.endpoint();
    let endpoint_parent = endpoint
        .parent()
        .ok_or_else(|| "broker endpoint has no parent".to_owned())?;
    prepare_private_root(endpoint_parent)?;
    let token = load_or_create_token(&options.token_path())?;
    let identity = Identity::of(&options);
    match connect_handshake(&options, &identity, &token, HandshakePurpose::Probe).await {
        Ok(stream) => {
            drop(stream);
            return Ok(());
        }
        Err(error) if error.starts_with("broker rejected connection:") => {
            stop(&options, &identity).await.map_err(|stop_error| {
                format!("{error}; cannot replace resident broker: {stop_error}")
            })?;
        }
        Err(_) => {}
    }

    let current = std::env::current_exe()
        .map_err(|error| format!("cannot locate marsh-mcp executable: {error}"))?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(options.log_path())
        .map_err(|error| format!("cannot open resident MCP broker log: {error}"))?;
    let mut command = Command::new(current);
    command
        .arg("_broker-run")
        .arg("--workspace")
        .arg(&options.workspace)
        .arg("--scope-root")
        .arg(&options.scope_root)
        .arg("--marsh")
        .arg(&options.marsh)
        .arg("--sbx")
        .arg(&options.sbx)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log));
    if options.allow_full_sbx_control {
        command.arg("--allow-full-sbx-control");
    }
    command
        .spawn()
        .map_err(|error| format!("cannot start resident MCP broker: {error}"))?;

    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    let mut last_error = "broker did not create its endpoint".to_owned();
    while tokio::time::Instant::now() < deadline {
        match connect_handshake(&options, &identity, &token, HandshakePurpose::Probe).await {
            Ok(stream) => {
                drop(stream);
                return Ok(());
            }
            Err(error) => last_error = error,
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(format!(
        "resident MCP broker failed to become ready: {last_error}; inspect {}",
        options.log_path().display()
    ))
}

/// Own the scope lease and serve authenticated MCP sessions until terminated.
///
/// # Errors
/// Returns an error when the endpoint, scope, or listener cannot be secured.
pub async fn broker_run(options: BrokerOptions) -> Result<(), String> {
    prepare_scope_root(&options.scope_root)?;
    let server = HostMcp::new(options.host_config()?);
    let endpoint = options.endpoint();
    let endpoint_parent = endpoint
        .parent()
        .ok_or_else(|| "broker socket has no parent".to_owned())?;
    prepare_private_root(endpoint_parent)?;
    let token = load_or_create_token(&options.token_path())?;
    // Pin both identities before publishing the endpoint. Replacing an
    // installed executable must not change the identity of this live process.
    let expected_fingerprint = options.fingerprint()?;
    let expected_build = build_id()?;
    match fs::remove_file(&endpoint) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("cannot remove stale broker socket: {error}")),
    }
    // Created, like each accepted connection, inside the host descriptor
    // fence: sessions spawn host commands concurrently (see accept_fenced).
    let listener = marsh_runtime::with_host_descriptor_creation_excluded(|| {
        std::os::unix::net::UnixListener::bind(&endpoint)
    })
    .and_then(|listener| {
        listener.set_nonblocking(true)?;
        UnixListener::from_std(listener)
    })
    .map_err(|error| format!("cannot bind resident MCP broker socket: {error}"))?;
    fs::set_permissions(&endpoint, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("cannot protect resident MCP broker socket: {error}"))?;
    write_owner_file(
        &options.pid_path(),
        std::process::id().to_string().as_bytes(),
    )?;
    let connection_permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let context = BrokerContext {
        expected_build,
        expected_fingerprint,
        expected_token: token,
        session_permits: Arc::new(Semaphore::new(MAX_SESSIONS)),
        sessions: Arc::default(),
        stopping: Arc::new(AtomicBool::new(false)),
        shutdown: Arc::new(Notify::new()),
    };
    loop {
        let accepted = tokio::select! {
            accepted = accept_fenced(&listener) => Some(accepted),
            () = context.shutdown.notified() => None,
        };
        let Some(accepted) = accepted else { break };
        let stream =
            accepted.map_err(|error| format!("resident MCP broker accept failed: {error}"))?;
        let permit = Arc::clone(&connection_permits).try_acquire_owned();
        let session = server.new_session();
        let context = context.clone();
        tokio::spawn(async move {
            let Ok(_permit) = permit else {
                let mut stream = stream;
                let exchange = async {
                    let _ = read_handshake(&mut stream).await?;
                    write_reply(
                        &mut stream,
                        HandshakeReply {
                            ok: false,
                            error: Some(format!(
                                "broker connection capacity ({MAX_CONNECTIONS}) is busy; retry later"
                            )),
                            pid: std::process::id(),
                        },
                    )
                    .await
                };
                let _ = tokio::time::timeout(HANDSHAKE_TIMEOUT, exchange).await;
                return;
            };
            if let Err(error) = serve_connection(stream, session, &context).await {
                eprintln!("marsh-mcp broker session: {error}");
            }
        });
    }
    drop(listener);
    remove_owner_file(&endpoint)?;
    remove_owner_file(&options.pid_path())?;
    Ok(())
}

/// Accept one connection inside the host descriptor fence. On macOS an
/// accepted socket becomes close-on-exec only after `accept`; a host command
/// a session spawned in between would hold the connection open for its life.
async fn accept_fenced(listener: &UnixListener) -> std::io::Result<UnixStream> {
    // Fence only each nonblocking poll/accept, never the asynchronous wait.
    // Tokio owns the descriptor registration, so no unsafe AsyncFd constructor
    // is needed and accepted sockets still cannot leak into a concurrent spawn.
    std::future::poll_fn(|cx| {
        marsh_runtime::with_host_descriptor_creation_excluded(|| listener.poll_accept(cx))
    })
    .await
    .map(|(stream, _)| stream)
}

/// Proxy one bounded stdio MCP transport to the authenticated broker.
///
/// # Errors
/// Returns an error on handshake mismatch, oversized frames, or transport I/O.
pub async fn connect(options: BrokerOptions) -> Result<(), String> {
    let token = load_token(&options.token_path())?;
    let stream = connect_handshake(
        &options,
        &Identity::of(&options),
        &token,
        HandshakePurpose::Session,
    )
    .await?;
    let (read, write) = stream.into_split();
    // Any failure of the broker connection, or its end while the client
    // still has the session open, is a lost broker, never a clean end.
    let log = options.log_path();
    let lost = move |detail: &dyn std::fmt::Display| {
        format!(
            "lost its connection to the resident MCP broker ({detail}); inspect {}",
            log.display()
        )
    };
    let upload_lost = lost.clone();
    let download_lost = lost.clone();
    let mut from_client = FramedRead::new(
        tokio::io::stdin(),
        LinesCodec::new_with_max_length(MAX_MCP_FRAME_BYTES),
    );
    let mut to_broker = FramedWrite::new(write, LinesCodec::new());
    // Set before the broker can see the end of client input, so the end of
    // the broker's stream after it is never mistaken for a lost broker.
    let client_ended = Arc::new(AtomicBool::new(false));
    let upload_ended = Arc::clone(&client_ended);
    let upload = tokio::spawn(async move {
        while let Some(frame) = from_client.next().await {
            let frame = frame.map_err(|error| format!("invalid MCP input frame: {error}"))?;
            to_broker
                .send(frame)
                .await
                .map_err(|error| upload_lost(&error))?;
        }
        upload_ended.store(true, Ordering::Release);
        drop(to_broker);
        Ok::<(), String>(())
    });
    let mut from_broker =
        FramedRead::new(read, LinesCodec::new_with_max_length(MAX_MCP_FRAME_BYTES));
    let mut to_client = FramedWrite::new(tokio::io::stdout(), LinesCodec::new());
    let download = tokio::spawn(async move {
        while let Some(frame) = from_broker.next().await {
            let frame = frame.map_err(|error| match error {
                LinesCodecError::Io(error) => download_lost(&error),
                LinesCodecError::MaxLineLengthExceeded => {
                    format!("invalid MCP broker frame: {error}")
                }
            })?;
            to_client
                .send(frame)
                .await
                .map_err(|error| format!("cannot write MCP output frame: {error}"))?;
        }
        Ok::<(), String>(())
    });
    tokio::pin!(upload);
    tokio::pin!(download);
    tokio::select! {
        uploaded = &mut upload => {
            uploaded.map_err(|error| format!("MCP proxy upload task failed: {error}"))??;
            download.await.map_err(|error| format!("MCP proxy download task failed: {error}"))?
        }
        downloaded = &mut download => {
            downloaded.map_err(|error| format!("MCP proxy download task failed: {error}"))??;
            if client_ended.load(Ordering::Acquire) {
                return upload
                    .await
                    .map_err(|error| format!("MCP proxy upload task failed: {error}"))?;
            }
            upload.as_mut().abort();
            Err(lost(&"the broker closed the session"))
        }
    }
}

/// Stop the exact resident broker when it has no attached MCP sessions.
///
/// # Errors
/// Returns an error when authentication fails, the broker is busy, or it does
/// not terminate within the bounded timeout.
pub async fn broker_stop(options: BrokerOptions) -> Result<(), String> {
    stop(&options, &Identity::of(&options)).await
}

async fn stop(options: &BrokerOptions, identity: &Result<Identity, String>) -> Result<(), String> {
    if !options.endpoint().exists() {
        return Ok(());
    }
    let token = load_token(&options.token_path())?;
    // The broker removes its endpoint before it exits, but holds the scope
    // root's lease until then: a replacement started in between cannot take
    // the lease and never becomes ready. Wait for the process too.
    // (A broker run in this process, as in tests, ends with its task.)
    let pid = fs::read_to_string(options.pid_path())
        .ok()
        .and_then(|pid| pid.trim().parse::<i32>().ok())
        .filter(|pid| u32::try_from(*pid).ok() != Some(std::process::id()))
        .and_then(rustix::process::Pid::from_raw);
    let stream = connect_handshake(options, identity, &token, HandshakePurpose::Stop).await?;
    drop(stream);
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        let exited = pid.is_none_or(|pid| {
            rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH)
        });
        if !options.endpoint().exists() && exited {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err(format!(
        "resident MCP broker did not stop; inspect {}",
        options.log_path().display()
    ))
}

async fn serve_connection(
    mut stream: UnixStream,
    server: HostMcp,
    context: &BrokerContext,
) -> Result<(), String> {
    let request = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_handshake(&mut stream))
        .await
        .map_err(|_| "resident MCP broker handshake timed out".to_owned())??;
    let mismatch = handshake_error(&request, context, std::process::id());
    if let Some(error) = mismatch {
        write_reply(
            &mut stream,
            HandshakeReply {
                ok: false,
                error: Some(error),
                pid: std::process::id(),
            },
        )
        .await?;
        return Ok(());
    }
    if request.purpose == HandshakePurpose::Stop {
        return handle_stop(&mut stream, context).await;
    }
    if context.stopping.load(Ordering::SeqCst) {
        write_reply(
            &mut stream,
            HandshakeReply {
                ok: false,
                error: Some("resident MCP broker is stopping".into()),
                pid: std::process::id(),
            },
        )
        .await?;
        return Ok(());
    }
    if request.purpose == HandshakePurpose::Probe {
        write_reply(
            &mut stream,
            HandshakeReply {
                ok: true,
                error: None,
                pid: std::process::id(),
            },
        )
        .await?;
        return Ok(());
    }
    let _session = match context.sessions.admit(context, &stream)? {
        Ok(session) => session,
        Err(refusal) => {
            write_reply(
                &mut stream,
                HandshakeReply {
                    ok: false,
                    error: Some(refusal),
                    pid: std::process::id(),
                },
            )
            .await?;
            return Ok(());
        }
    };
    write_reply(
        &mut stream,
        HandshakeReply {
            ok: true,
            error: None,
            pid: std::process::id(),
        },
    )
    .await?;
    serve_mcp_stream(stream, server).await
}

async fn handle_stop(stream: &mut UnixStream, context: &BrokerContext) -> Result<(), String> {
    let shutdown_permits = match begin_stop(context, STOP_ADMISSION_WAIT).await {
        Ok(permits) => permits,
        Err(error) => {
            let message = match error {
                StopAdmissionError::AlreadyStopping => "resident MCP broker is already stopping",
                StopAdmissionError::SessionsAttached => {
                    "resident MCP broker has attached sessions; unload or stop them before replacement"
                }
            };
            write_reply(
                stream,
                HandshakeReply {
                    ok: false,
                    error: Some(message.into()),
                    pid: std::process::id(),
                },
            )
            .await?;
            return Ok(());
        }
    };
    let reply = write_reply(
        stream,
        HandshakeReply {
            ok: true,
            error: None,
            pid: std::process::id(),
        },
    )
    .await;
    context.shutdown.notify_one();
    // The process is committed to shutdown. Keeping every session permit
    // consumed prevents an already-accepted connection from being admitted in
    // the interval between this reply and listener teardown.
    std::mem::forget(shutdown_permits);
    reply
}

/// Admit a stop only when no session's client is connected. Sessions whose
/// clients have gone are waited for (up to `wait`) while they clean up.
async fn begin_stop(
    context: &BrokerContext,
    wait: Duration,
) -> Result<tokio::sync::OwnedSemaphorePermit, StopAdmissionError> {
    if context
        .stopping
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err(StopAdmissionError::AlreadyStopping);
    }
    let all = u32::try_from(MAX_SESSIONS).expect("session limit fits u32");
    let releasing = {
        let connections = context
            .sessions
            .connections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Ok(permits) = Arc::clone(&context.session_permits).try_acquire_many_owned(all) {
            return Ok(permits);
        }
        if AdmittedSessions::any_client_connected(&connections) {
            context.stopping.store(false, Ordering::SeqCst);
            return Err(StopAdmissionError::SessionsAttached);
        }
        // Every permit holder is releasing, and `stopping` (set before the
        // lock) refuses any later admission.
        Arc::clone(&context.session_permits).acquire_many_owned(all)
    };
    if let Ok(Ok(permits)) = tokio::time::timeout(wait, releasing).await {
        Ok(permits)
    } else {
        context.stopping.store(false, Ordering::SeqCst);
        Err(StopAdmissionError::SessionsAttached)
    }
}

async fn serve_mcp_stream(stream: UnixStream, server: HostMcp) -> Result<(), String> {
    let (read, write) = stream.into_split();
    let reader = FramedRead::new(
        read,
        JsonRpcMessageCodec::<RxJsonRpcMessage<RoleServer>>::new_with_max_length(
            MAX_MCP_FRAME_BYTES,
        ),
    )
    .take_while(|result| future::ready(result.is_ok()))
    .filter_map(|result| future::ready(result.ok()));
    let writer = FramedWrite::new(
        write,
        JsonRpcMessageCodec::<TxJsonRpcMessage<RoleServer>>::new_with_max_length(
            MAX_MCP_FRAME_BYTES,
        ),
    );
    let running = server
        .clone()
        .serve((writer, reader))
        .await
        .map_err(|error| format!("cannot start brokered MCP transport: {error}"))?;
    let result = running
        .waiting()
        .await
        .map_err(|error| format!("brokered MCP transport failed: {error}"));
    let shutdown = server.shutdown_session().await;
    result?;
    shutdown
}

fn handshake_error(request: &Handshake, context: &BrokerContext, pid: u32) -> Option<String> {
    if request.token != context.expected_token {
        Some("authentication failed for resident MCP broker".to_owned())
    } else if request.purpose != HandshakePurpose::Stop && request.protocol != PROTOCOL_VERSION {
        Some(format!(
            "protocol mismatch: broker={}, client={}; reinstall and restart marsh-mcp",
            PROTOCOL_VERSION, request.protocol
        ))
    } else if request.purpose != HandshakePurpose::Stop && request.build != context.expected_build {
        Some(format!(
            "build mismatch: resident broker PID {pid} uses a different marsh-mcp; run `marsh mcp stop` and then `marsh mcp install sbx`"
        ))
    } else if request.purpose != HandshakePurpose::Stop
        && request.fingerprint != context.expected_fingerprint
    {
        Some(format!(
            "configuration mismatch: resident broker PID {pid} owns this scope root with different paths or privileges; run `marsh mcp stop` or use a distinct --scope-root"
        ))
    } else {
        None
    }
}

async fn connect_handshake(
    options: &BrokerOptions,
    identity: &Result<Identity, String>,
    token: &str,
    purpose: HandshakePurpose,
) -> Result<UnixStream, String> {
    // File identities are computed before the socket is opened. Hashing a
    // large SBX executable can be slower than the broker's wire-handshake
    // deadline and must not consume a server connection while no bytes can
    // yet be sent.
    let Identity { build, fingerprint } = identity.as_ref().map_err(Clone::clone)?;
    let endpoint = options.endpoint();
    validate_private_endpoint(&endpoint)?;
    let mut stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, UnixStream::connect(&endpoint))
        .await
        .map_err(|_| "resident MCP broker connection timed out".to_owned())?
        .map_err(|error| format!("cannot connect to resident MCP broker: {error}"))?;
    let exchange = async {
        write_handshake(
            &mut stream,
            Handshake {
                protocol: PROTOCOL_VERSION,
                build: build.clone(),
                fingerprint: fingerprint.clone(),
                token: token.to_owned(),
                purpose,
            },
        )
        .await?;
        read_packet(&mut stream).await
    };
    // A stop's reply waits for sessions whose clients have gone to clean up.
    let reply_timeout = if purpose == HandshakePurpose::Stop {
        HANDSHAKE_TIMEOUT.saturating_add(STOP_ADMISSION_WAIT)
    } else {
        HANDSHAKE_TIMEOUT
    };
    let reply: HandshakeReply = tokio::time::timeout(reply_timeout, exchange)
        .await
        .map_err(|_| "resident MCP broker handshake timed out".to_owned())??;
    if !reply.ok {
        return Err(format!(
            "broker rejected connection: {}",
            reply.error.unwrap_or_else(|| "unspecified error".into())
        ));
    }
    Ok(stream)
}

async fn write_handshake(stream: &mut UnixStream, value: Handshake) -> Result<(), String> {
    write_packet(stream, &value).await
}

async fn read_handshake(stream: &mut UnixStream) -> Result<Handshake, String> {
    read_packet(stream).await
}

async fn write_reply(stream: &mut UnixStream, value: HandshakeReply) -> Result<(), String> {
    write_packet(stream, &value).await
}

async fn write_packet(stream: &mut UnixStream, value: &impl Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| format!("cannot encode broker handshake: {error}"))?;
    if bytes.len() > MAX_HANDSHAKE_BYTES {
        return Err("broker handshake exceeds its fixed limit".into());
    }
    let length = u32::try_from(bytes.len())
        .map_err(|_| "broker handshake length cannot be represented".to_owned())?;
    stream
        .write_u32(length)
        .await
        .map_err(|error| format!("cannot write broker handshake length: {error}"))?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|error| format!("cannot write broker handshake: {error}"))
}

async fn read_packet<T: for<'de> Deserialize<'de>>(stream: &mut UnixStream) -> Result<T, String> {
    let length = stream
        .read_u32()
        .await
        .map_err(|error| format!("cannot read broker handshake length: {error}"))?
        as usize;
    if length > MAX_HANDSHAKE_BYTES {
        return Err("broker handshake exceeds its fixed limit".into());
    }
    let mut bytes = vec![0; length];
    stream
        .read_exact(&mut bytes)
        .await
        .map_err(|error| format!("cannot read broker handshake: {error}"))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("invalid broker handshake: {error}"))
}

/// This process's build: computed once, so replacing the installed
/// executable later cannot change the identity of a live process.
fn build_id() -> Result<String, String> {
    static BUILD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(build) = BUILD.get() {
        return Ok(build.clone());
    }
    let current = std::env::current_exe()
        .map_err(|error| format!("cannot locate marsh-mcp executable: {error}"))?;
    let mut digest = Sha256::new();
    hash_path(&current, &mut digest)?;
    let build = format!(
        "{}:{}:{}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        marsh_contracts::process::hex(&digest.finalize())
    );
    Ok(BUILD.get_or_init(|| build).clone())
}

fn hash_path(path: &Path, digest: &mut Sha256) -> Result<(), String> {
    let mut file = fs::File::open(path).map_err(|error| {
        format!(
            "cannot open {} for broker identity: {error}",
            path.display()
        )
    })?;
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("cannot hash {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(())
}

fn prepare_private_root(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("cannot create owner-only broker directory: {error}"))?;
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)
        .map_err(|error| format!("cannot securely open broker directory: {error}"))?;
    let metadata = directory
        .metadata()
        .map_err(|error| format!("cannot inspect broker directory: {error}"))?;
    if !metadata.is_dir() || metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(format!(
            "broker directory must be owned by this user: {}",
            path.display()
        ));
    }
    directory
        .set_permissions(fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("cannot protect broker directory: {error}"))?;
    if directory
        .metadata()
        .map_err(|error| format!("cannot recheck broker directory: {error}"))?
        .mode()
        & 0o077
        != 0
    {
        return Err(format!(
            "broker directory must be owner-only: {}",
            path.display()
        ));
    }
    Ok(())
}

fn validate_private_endpoint(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "broker endpoint has no parent".to_owned())?;
    prepare_private_root(parent)?;
    if path.exists() {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("cannot inspect broker endpoint: {error}"))?;
        if !metadata.file_type().is_socket()
            || metadata.uid() != rustix::process::geteuid().as_raw()
        {
            return Err("resident MCP broker endpoint is not an owner-controlled socket".into());
        }
    }
    Ok(())
}

fn load_or_create_token(path: &Path) -> Result<String, String> {
    let temporary = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|error| format!("cannot create broker token: {error}"))?;
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let prepared = file
        .write_all(token.as_bytes())
        .and_then(|()| file.sync_all());
    if let Err(error) = prepared {
        let _ = fs::remove_file(&temporary);
        return Err(format!("cannot prepare broker token: {error}"));
    }
    // The final name appears only after its contents are durable. Hard-link
    // publication is no-replace, so concurrent creators agree on one token.
    let published = fs::hard_link(&temporary, path);
    let _ = fs::remove_file(&temporary);
    match published {
        Ok(()) => Ok(token),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => load_token(path),
        Err(error) => Err(format!("cannot publish broker token: {error}")),
    }
}

fn load_token(path: &Path) -> Result<String, String> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)
        .map_err(|error| format!("cannot securely open broker token: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect broker token: {error}"))?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err("broker token must be an owner-only regular file".into());
    }
    if metadata.len() != 64 {
        return Err("broker token is malformed".into());
    }
    let mut bytes = Vec::with_capacity(65);
    file.take(65)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read broker token: {error}"))?;
    let token = String::from_utf8(bytes).map_err(|_| "broker token is malformed".to_owned())?;
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("broker token is malformed".into());
    }
    Ok(token)
}

fn write_owner_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|error| format!("cannot create broker state: {error}"))?;
    file.write_all(bytes)
        .map_err(|error| format!("cannot write broker state: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("cannot sync broker state: {error}"))?;
    fs::rename(&temporary, path).map_err(|error| format!("cannot publish broker state: {error}"))
}

fn remove_owner_file(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "cannot remove broker state {}: {error}",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    struct RemoveOnDrop(PathBuf);

    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn first_start_options(scope_root: PathBuf, root: &Path) -> BrokerOptions {
        BrokerOptions {
            workspace: root.join("missing-workspace"),
            home: scope_root.join("default"),
            scope_root,
            marsh: root.join("missing-marsh"),
            sbx: root.join("missing-sbx"),
            allow_full_sbx_control: false,
        }
    }

    #[tokio::test]
    async fn broker_first_start_creates_daemon_compatible_product_directory() {
        let root = tempfile::tempdir().unwrap();
        let product = root
            .path()
            .join("Library/Application Support/marsh-mcp-scopes");
        let scope = product.join("workspace");
        let error = broker_run(first_start_options(scope.clone(), root.path()))
            .await
            .unwrap_err();
        assert!(error.contains("workspace"), "{error}");
        for path in [product, scope] {
            let metadata = fs::symlink_metadata(path).unwrap();
            assert!(metadata.file_type().is_dir());
            assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        }
    }

    #[tokio::test]
    async fn broker_first_start_rejects_symlinked_product_directory() {
        let root = tempfile::tempdir().unwrap();
        let app_support = root.path().join("Library/Application Support");
        fs::create_dir_all(&app_support).unwrap();
        let replacement = root.path().join("replacement");
        fs::create_dir(&replacement).unwrap();
        std::os::unix::fs::symlink(&replacement, app_support.join("marsh-mcp-scopes")).unwrap();
        let scope = app_support.join("marsh-mcp-scopes/workspace");
        let options = first_start_options(scope, root.path());
        let error = broker_start(options.clone()).await.unwrap_err();
        assert!(error.contains("must not be a symlink"), "{error}");
        let error = broker_run(options).await.unwrap_err();
        assert!(error.contains("must not be a symlink"), "{error}");
        assert!(!replacement.join("workspace").exists());
    }

    #[test]
    fn token_is_owner_only_and_stable() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("token");
        let first = load_or_create_token(&path).unwrap();
        let second = load_or_create_token(&path).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 64);
        assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn concurrent_token_creators_publish_one_complete_value() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("token");
        let gate = Arc::new(std::sync::Barrier::new(8));
        let threads = (0..8)
            .map(|_| {
                let path = path.clone();
                let gate = Arc::clone(&gate);
                std::thread::spawn(move || {
                    gate.wait();
                    load_or_create_token(&path).unwrap()
                })
            })
            .collect::<Vec<_>>();
        let values = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>();
        assert!(values.iter().all(|value| value == &values[0]));
        assert_eq!(load_token(&path).unwrap(), values[0]);
    }

    #[test]
    fn incomplete_temporary_token_does_not_publish_or_poison_name() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("token");
        fs::write(path.with_extension("tmp-interrupted"), b"partial").unwrap();
        assert!(!path.exists());
        assert_eq!(
            load_or_create_token(&path).unwrap(),
            load_token(&path).unwrap()
        );
        fs::write(&path, vec![b'x'; 1024 * 1024]).unwrap();
        assert_eq!(load_token(&path).unwrap_err(), "broker token is malformed");
    }

    #[tokio::test]
    async fn handshake_frames_are_bounded() {
        let (mut left, mut right) = UnixStream::pair().unwrap();
        let writer = tokio::spawn(async move {
            left.write_u32(u32::try_from(MAX_HANDSHAKE_BYTES + 1).unwrap())
                .await
                .unwrap();
        });
        let result = read_handshake(&mut right).await;
        writer.await.unwrap();
        assert!(result.unwrap_err().contains("fixed limit"));
    }

    #[tokio::test]
    async fn client_computes_identity_before_opening_the_broker_socket() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let home = root.path().join("home");
        let scope_root = root.path().join("scope");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&scope_root).unwrap();
        let marsh = root.path().join("marsh");
        fs::write(&marsh, b"marsh").unwrap();
        fs::write(root.path().join("marshd"), b"marshd").unwrap();
        let options = BrokerOptions {
            workspace,
            home,
            scope_root,
            marsh,
            sbx: root.path().join("missing-sbx"),
            allow_full_sbx_control: false,
        };
        let endpoint = options.endpoint();
        // The endpoint lives under the fixed /tmp root, outside `root`; remove
        // it even when an assertion fails.
        let _endpoint_root = RemoveOnDrop(endpoint.parent().unwrap().to_path_buf());
        prepare_private_root(endpoint.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(endpoint).unwrap();

        let error = connect_handshake(
            &options,
            &Identity::of(&options),
            "unused",
            HandshakePurpose::Probe,
        )
        .await
        .unwrap_err();
        assert!(error.contains("missing-sbx"));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "identity hashing failure opened a socket before the handshake was ready"
        );
    }

    /// The broker's sessions spawn host commands. On macOS a socket becomes
    /// close-on-exec only after it is created, inside the shared host fence;
    /// the broker must not accept a connection while that fence is held, or
    /// a concurrently spawned command could hold the connection for its life.
    #[tokio::test(flavor = "multi_thread")]
    async fn broker_accepts_no_connection_while_a_descriptor_is_being_created() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let scope_root = root.path().join("scope");
        fs::create_dir(&workspace).unwrap();
        for name in ["marsh", "marshd", "sbx"] {
            let path = root.path().join(name);
            fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let options = BrokerOptions {
            workspace,
            home: scope_root.join("default"),
            scope_root,
            marsh: root.path().join("marsh"),
            sbx: root.path().join("sbx"),
            allow_full_sbx_control: false,
        };
        let _endpoint_root = RemoveOnDrop(options.endpoint().parent().unwrap().to_path_buf());
        let broker = tokio::spawn(broker_run(options.clone()));
        let ready = tokio::time::Instant::now() + START_TIMEOUT;
        while !options.endpoint().exists() {
            assert!(tokio::time::Instant::now() < ready, "broker never listened");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let token = load_token(&options.token_path()).unwrap();
        let released = Arc::new(AtomicBool::new(false));
        let (held, holding) = std::sync::mpsc::channel();
        let creator = {
            let released = Arc::clone(&released);
            std::thread::spawn(move || {
                marsh_runtime::with_host_descriptor_creation_excluded(|| {
                    held.send(()).unwrap();
                    std::thread::sleep(Duration::from_secs(1));
                    released.store(true, Ordering::Release);
                });
            })
        };
        holding.recv().unwrap();
        let probe = connect_handshake(
            &options,
            &Identity::of(&options),
            &token,
            HandshakePurpose::Probe,
        )
        .await;
        let accepted_after_release = released.load(Ordering::Acquire);
        creator.join().unwrap();
        drop(probe.unwrap());
        broker_stop(options).await.unwrap();
        broker.await.unwrap().unwrap();
        assert!(
            accepted_after_release,
            "the broker accepted a connection while a descriptor was being created"
        );
    }

    /// The fingerprint hashes the pinned executables' bytes: replacing their
    /// contents changes it even when size and modification time are kept.
    #[test]
    fn fingerprint_binds_executable_bytes_not_their_metadata() {
        let root = tempfile::tempdir().unwrap();
        for (name, bytes) in [("marsh", "marsh"), ("marshd", "marshd"), ("sbx", "sbx-1")] {
            fs::write(root.path().join(name), bytes).unwrap();
        }
        let options = BrokerOptions {
            workspace: root.path().to_path_buf(),
            home: root.path().join("home"),
            scope_root: root.path().join("scope"),
            marsh: root.path().join("marsh"),
            sbx: root.path().join("sbx"),
            allow_full_sbx_control: false,
        };
        let before = options.fingerprint().unwrap();
        for name in ["marsh", "marshd", "sbx"] {
            let path = root.path().join(name);
            let original = fs::read(&path).unwrap();
            let modified = fs::metadata(&path).unwrap().modified().unwrap();
            let mut replaced = original.clone();
            *replaced.last_mut().unwrap() ^= 1;
            fs::write(&path, &replaced).unwrap();
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(modified)
                .unwrap();
            assert_ne!(options.fingerprint().unwrap(), before, "{name}");
            fs::write(&path, &original).unwrap();
        }
        assert_eq!(options.fingerprint().unwrap(), before);
    }

    #[test]
    fn handshake_rejects_auth_config_protocol_and_build_mismatches() {
        let build = build_id().unwrap();
        let context = BrokerContext {
            expected_build: build.clone(),
            expected_fingerprint: "config".into(),
            expected_token: "secret".into(),
            session_permits: Arc::new(Semaphore::new(MAX_SESSIONS)),
            sessions: Arc::default(),
            stopping: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(Notify::new()),
        };
        let valid = Handshake {
            protocol: PROTOCOL_VERSION,
            build: build.clone(),
            fingerprint: "config".into(),
            token: "secret".into(),
            purpose: HandshakePurpose::Session,
        };
        assert_eq!(handshake_error(&valid, &context, 42), None);
        let wrong_token = Handshake {
            token: "other".into(),
            ..valid
        };
        assert!(
            handshake_error(&wrong_token, &context, 42)
                .unwrap()
                .contains("authentication failed")
        );
        let wrong_config = Handshake {
            token: "secret".into(),
            fingerprint: "other".into(),
            ..wrong_token
        };
        assert!(
            handshake_error(&wrong_config, &context, 42)
                .unwrap()
                .contains("configuration mismatch")
        );
        let wrong_protocol = Handshake {
            protocol: PROTOCOL_VERSION + 1,
            fingerprint: "config".into(),
            ..wrong_config
        };
        assert!(
            handshake_error(&wrong_protocol, &context, 42)
                .unwrap()
                .contains("protocol mismatch")
        );
        let wrong_build = Handshake {
            protocol: PROTOCOL_VERSION,
            build: format!("{build}-old"),
            ..wrong_protocol
        };
        assert!(
            handshake_error(&wrong_build, &context, 42)
                .unwrap()
                .contains("build mismatch")
        );
        let stop = Handshake {
            purpose: HandshakePurpose::Stop,
            ..wrong_build
        };
        assert_eq!(handshake_error(&stop, &context, 42), None);
    }

    /// A stop is refused while a session's client is connected, waits for a
    /// session whose client has gone to release, and admits no session in
    /// between.
    #[tokio::test]
    async fn stop_admission_refuses_connected_clients_and_waits_for_departed_ones() {
        let context = Arc::new(BrokerContext {
            expected_build: "build".into(),
            expected_fingerprint: "config".into(),
            expected_token: "secret".into(),
            session_permits: Arc::new(Semaphore::new(MAX_SESSIONS)),
            sessions: Arc::default(),
            stopping: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(Notify::new()),
        });
        let (client, connection) = UnixStream::pair().unwrap();
        let session = context
            .sessions
            .admit(&context, &connection)
            .unwrap()
            .unwrap();
        // The wait is out of reach: a connected client is refused at once.
        assert_eq!(
            begin_stop(&context, Duration::from_hours(1)).await.err(),
            Some(StopAdmissionError::SessionsAttached)
        );
        assert!(!context.stopping.load(Ordering::SeqCst));

        drop(client);
        let stopping = Arc::clone(&context);
        let stop =
            tokio::spawn(async move { begin_stop(&stopping, Duration::from_hours(1)).await });
        while !context.stopping.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        let (_late_client, late) = UnixStream::pair().unwrap();
        assert_eq!(
            context.sessions.admit(&context, &late).unwrap().err(),
            Some("resident MCP broker is stopping".to_owned())
        );
        drop(session);
        let shutdown_permits = stop.await.unwrap().unwrap();
        assert_eq!(context.session_permits.available_permits(), 0);
        drop(shutdown_permits);
    }
}
