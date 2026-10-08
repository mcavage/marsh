//! Same-user resident daemon boundary and public inspection API.
//!
//! The daemon endpoint is scoped to one canonical `MARSH_HOME`. The Unix
//! socket and bearer token are both owner-only; every length-framed request
//! must present that token. Job data streams use separate attachments and are
//! deliberately absent from this inspection protocol.

#![allow(clippy::missing_errors_doc)]

pub mod host_state_directory;

use marsh_acp::AgentAdapterDeclaration;
pub use marsh_contracts::command_registry::CommandName as KitCommandName;
pub use marsh_contracts::{ExecutionOutcome, TerminalSize};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use signal_hook::{
    consts::signal::{SIGHUP, SIGINT, SIGTERM, SIGWINCH},
    iterator::{Handle as SignalHandle, Signals},
};
#[cfg(test)]
use std::process::Stdio;
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        io::{AsFd, OwnedFd},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use thiserror::Error;
use uuid::Uuid;

mod acp_bridge;
mod daemon_startup;
mod receipt_journal;
pub use daemon_startup::{
    DEFAULT_JOB_RESOURCES, JobDefaults, StartupFailure, adopt_startup_lock,
    finish_startup_diagnostics,
};
pub mod process;
mod shell_admission;
mod shell_client;
pub mod split;
pub mod split_confinement;
pub mod split_fs;
pub use acp_bridge::{AcpAttachmentBridge, AcpAttachmentStatus};
mod acp_session;
pub use acp_session::{
    AcpPendingPermission, AcpSessionStatus, AcpSessionSummary, AcpUpdate, PromptAdmissionError,
};
use acp_session::{AcpSessionManager, AcpStartupAbort};
pub mod mcp_defaults;
mod mcp_publication;
use mcp_publication::McpHostControl;
pub use mcp_publication::{
    HostPublicationContext, PublicationCommit, PublicationHostEvent, PublicationKind,
    PublicationOperation, PublicationOutcome, PublicationScope, PublicationStockOutput,
    PublishedName, mark_revocation_pending as mark_mcp_revocation_pending,
    publication_load_arguments, validate_publication_options, validate_publication_pipeline,
    validate_publication_sandbox_output,
};
pub mod dev_broker;
pub mod relay;
pub mod relay_cleanup;
pub use dev_broker::DevBroker;

pub const PROTOCOL: &str = "marsh.daemon/v1";
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// Maximum shell stdin frames in flight while the guest is not reading.
pub const SHELL_STDIN_WINDOW: usize = 64;
pub const SHELL_STDIN_CHUNK: usize = 16 * 1024;
pub const FEATURES: &[&str] = &["direct-command-acceptance-v1", "durable-results-v1"];
// Scope cleanup may contain several independently bounded stock-SBX
// operations. Keep the batch above those ordinary per-command limits while
// still giving its caller one finite aggregate deadline.
const SCOPE_LIFECYCLE_EXECUTION_TIMEOUT: Duration = Duration::from_mins(8);
const SCOPE_LIFECYCLE_REPLY_TIMEOUT: Duration = Duration::from_mins(9);
const RESIDENT_SHUTDOWN_RETRY_TIMEOUT: Duration = Duration::from_millis(250);
const RESIDENT_SHUTDOWN_RETRY_INTERVAL: Duration = Duration::from_millis(10);
const BUSY_LIFECYCLE_MESSAGE: &str = "daemon has active shell sessions or jobs, or unrecovered shell cleanup uncertainty; exit active shells and use host `marsh reset` or `marsh stop` to recover";
// Leave descriptor headroom for relay socket clones, active attachments,
// worker transports, and the listener on macOS's default soft FD limit.
/// Each connection holds one descriptor plus at most two transient ones. A
/// split's 16 branches each hold one daemon-internal connection, and every
/// argv branch at most four capability connections, so 128 leaves room for
/// one full split beside ordinary shells. The server raises its descriptor
/// soft limit at bind (`raise_descriptor_limit`) so 128 connections fit.
const MAX_DAEMON_CONNECTIONS: usize = 128;
const DAEMON_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
// Publication replies may legitimately wait behind one cold Kit preparation
// (stock sbx 5 + build 20 + load 10 + docker 2 minutes) and the five-minute
// host publication budget. Bound the reply idle time above that sum so a
// lost reply surfaces as uncertain instead of blocking the caller forever.
const PUBLICATION_REPLY_IDLE_TIMEOUT: Duration = Duration::from_mins(45);

struct DeadlineRead<'a> {
    stream: &'a mut UnixStream,
    deadline: Instant,
}

impl Read for DeadlineRead<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon handshake deadline elapsed",
            ));
        }
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(bytes)
    }
}

struct DaemonConnectionPermit(Arc<AtomicUsize>);

impl Drop for DaemonConnectionPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Raise the soft descriptor limit toward the hard limit (macOS defaults to
/// 256), capped at 4096: connections, relays, and splits all hold descriptors.
fn raise_descriptor_limit() {
    let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile);
    let wanted = limit.maximum.map_or(4096, |maximum| maximum.min(4096));
    if limit.current.is_some_and(|current| current < wanted) {
        let _ = rustix::process::setrlimit(
            rustix::process::Resource::Nofile,
            rustix::process::Rlimit {
                current: Some(wanted),
                maximum: limit.maximum,
            },
        );
    }
}

fn acquire_daemon_connection(active: &Arc<AtomicUsize>) -> Option<DaemonConnectionPermit> {
    active
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < MAX_DAEMON_CONNECTIONS).then_some(count + 1)
        })
        .ok()?;
    Some(DaemonConnectionPermit(Arc::clone(active)))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointPaths {
    pub runtime_directory: PathBuf,
    pub socket: PathBuf,
    pub lock: PathBuf,
    pub token: PathBuf,
}

impl EndpointPaths {
    /// Resolve one daemon endpoint from a trusted, existing `MARSH_HOME`.
    ///
    /// # Errors
    /// Rejects symlinks, non-directories, and homes owned by another user.
    pub fn for_home(home: &Path) -> Result<Self, DaemonError> {
        let metadata = fs::symlink_metadata(home)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(DaemonError::UnsafeHome(home.to_path_buf()));
        }
        let uid = rustix::process::getuid().as_raw();
        if metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
            return Err(DaemonError::UnsafeHome(home.to_path_buf()));
        }
        let canonical = fs::canonicalize(home)?;
        let scope = hex(&Sha256::digest(canonical.as_os_str().as_encoded_bytes()));
        // Selected-home contents are mounted into shell and job containers.
        // Control credentials therefore live in the user's private host
        // runtime tree, keyed by the selected home's canonical identity.
        // A fixed root lets shells with different TMPDIR values share it.
        let runtime_directory = Path::new("/tmp")
            .join(format!("marsh-{uid}"))
            .join(&scope[..16]);
        Ok(Self {
            socket: runtime_directory.join("s"),
            lock: runtime_directory.join("l"),
            token: runtime_directory.join("t"),
            runtime_directory,
        })
    }
}

/// Exclusive lifecycle ownership for one scoped daemon.
pub struct Lifecycle {
    paths: EndpointPaths,
    _lock: File,
    listener: UnixListener,
}

impl Lifecycle {
    /// Claim the endpoint, reclaiming only an owner-verified endpoint whose
    /// recorded process is conclusively gone.
    ///
    /// # Errors
    /// A live owner, malformed lock, foreign artifact, or unknown runtime
    /// content fails closed.
    pub fn bind(home: &Path) -> Result<Self, DaemonError> {
        let paths = EndpointPaths::for_home(home)?;
        let base = paths
            .runtime_directory
            .parent()
            .ok_or_else(|| DaemonError::UnsafeHome(paths.runtime_directory.clone()))?;
        if !base.exists() {
            match fs::create_dir(base) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        verify_owned_directory(base)?;
        fs::set_permissions(base, fs::Permissions::from_mode(0o700))?;
        ensure_runtime_directory(&paths.runtime_directory)?;

        if paths.lock.exists() {
            match reclaim_stale_endpoint(&paths)? {
                ReclaimOutcome::Reclaimed => ensure_runtime_directory(&paths.runtime_directory)?,
                ReclaimOutcome::Live | ReclaimOutcome::Absent => {
                    return Err(DaemonError::EndpointExists(paths.lock));
                }
            }
        }

        let mut lock = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&paths.lock)
            .map_err(|error| map_endpoint_collision(error, &paths.lock))?;
        writeln!(&mut lock, "{}", std::process::id())?;

        if let Err(error) = remove_owned_orphan(&paths.socket, EndpointKind::Socket)
            .and_then(|()| remove_owned_orphan(&paths.token, EndpointKind::Regular))
        {
            let _ = fs::remove_file(&paths.lock);
            return Err(error);
        }
        if paths.socket.exists() {
            let _ = fs::remove_file(&paths.lock);
            return Err(DaemonError::EndpointExists(paths.socket));
        }
        let listener = match UnixListener::bind(&paths.socket) {
            Ok(listener) => listener,
            Err(error) => {
                let _ = fs::remove_file(&paths.lock);
                return Err(error.into());
            }
        };
        fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
        if let Err(error) = create_token(&paths.token) {
            let _ = fs::remove_file(&paths.socket);
            let _ = fs::remove_file(&paths.lock);
            return Err(error);
        }
        Ok(Self {
            paths,
            _lock: lock,
            listener,
        })
    }

    #[must_use]
    pub fn listener(&self) -> &UnixListener {
        &self.listener
    }

    #[must_use]
    pub fn paths(&self) -> &EndpointPaths {
        &self.paths
    }
}

fn ensure_runtime_directory(path: &Path) -> Result<(), DaemonError> {
    if !path.exists() {
        fs::create_dir(path)?;
    }
    verify_owned_directory(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReclaimOutcome {
    Absent,
    Live,
    Reclaimed,
}

#[derive(Clone, Copy)]
enum EndpointKind {
    Regular,
    Socket,
}

fn reclaim_stale_endpoint(paths: &EndpointPaths) -> Result<ReclaimOutcome, DaemonError> {
    if !paths.lock.exists() {
        return Ok(ReclaimOutcome::Absent);
    }
    verify_endpoint_entry(&paths.lock, EndpointKind::Regular)?;
    let owner_text = fs::read_to_string(&paths.lock)?;
    let owner_raw = owner_text
        .trim()
        .parse::<i32>()
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| DaemonError::EndpointInconsistent(paths.runtime_directory.clone()))?;
    match rustix::process::test_kill_process(owner_raw) {
        Ok(()) | Err(rustix::io::Errno::PERM) => return Ok(ReclaimOutcome::Live),
        Err(rustix::io::Errno::SRCH) => {}
        Err(error) => return Err(DaemonError::Io(error.into())),
    }
    verify_reclaimable_directory(paths)?;
    let quarantine = paths
        .runtime_directory
        .with_extension(format!("stale-{}", Uuid::new_v4().as_simple()));
    match fs::rename(&paths.runtime_directory, &quarantine) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ReclaimOutcome::Reclaimed);
        }
        Err(error) => return Err(error.into()),
    }
    for name in ["s", "t", "l"] {
        let path = quarantine.join(name);
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    fs::remove_dir(quarantine)?;
    Ok(ReclaimOutcome::Reclaimed)
}

fn verify_reclaimable_directory(paths: &EndpointPaths) -> Result<(), DaemonError> {
    verify_owned_directory(&paths.runtime_directory)?;
    for entry in fs::read_dir(&paths.runtime_directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let path = entry.path();
        match name.to_str() {
            Some("l" | "t") => verify_endpoint_entry(&path, EndpointKind::Regular)?,
            Some("s") => verify_endpoint_entry(&path, EndpointKind::Socket)?,
            _ => {
                return Err(DaemonError::EndpointInconsistent(
                    paths.runtime_directory.clone(),
                ));
            }
        }
    }
    Ok(())
}

fn remove_owned_orphan(path: &Path, kind: EndpointKind) -> Result<(), DaemonError> {
    if path.exists() {
        verify_endpoint_entry(path, kind)?;
        fs::remove_file(path)?;
    }
    Ok(())
}

fn verify_endpoint_entry(path: &Path, kind: EndpointKind) -> Result<(), DaemonError> {
    let metadata = fs::symlink_metadata(path)?;
    let expected_type = match kind {
        EndpointKind::Regular => metadata.is_file(),
        EndpointKind::Socket => metadata.file_type().is_socket(),
    };
    if metadata.file_type().is_symlink()
        || !expected_type
        || metadata.uid() != rustix::process::getuid().as_raw()
    {
        return Err(DaemonError::EndpointInconsistent(path.to_owned()));
    }
    Ok(())
}

impl Drop for Lifecycle {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.paths.socket);
        let _ = fs::remove_file(&self.paths.token);
        let _ = fs::remove_file(&self.paths.lock);
    }
}

fn map_endpoint_collision(error: io::Error, path: &Path) -> DaemonError {
    if error.kind() == io::ErrorKind::AlreadyExists {
        DaemonError::EndpointExists(path.to_path_buf())
    } else {
        DaemonError::Io(error)
    }
}

fn verify_owned_directory(path: &Path) -> Result<(), DaemonError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != rustix::process::getuid().as_raw()
    {
        return Err(DaemonError::UnsafeHome(path.to_path_buf()));
    }
    Ok(())
}

fn create_token(path: &Path) -> Result<(), DaemonError> {
    let token = random_token()?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| map_endpoint_collision(error, path))?;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn random_token() -> Result<String, DaemonError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| DaemonError::Random(error.to_string()))?;
    Ok(hex(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(char::from(DIGITS[usize::from(byte >> 4)]));
        result.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    result
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Identity of one exact executable file version: device, inode, size, and
/// modification/change times. Every client invocation computes this, so it
/// must not read the file: hashing the stock CLI and daemon (~190 MB) cost
/// ~300 ms per warm command. Any replacement or in-place rewrite changes the
/// inode or ctime (which unprivileged callers cannot set), so a changed
/// artifact still yields a different daemon identity.
fn artifact_identity(path: &Path) -> Result<String, DaemonError> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(DaemonError::EndpointInconsistent(path.to_owned()));
    }
    Ok(format!(
        "file:{}:{}:{}:{}.{}:{}.{}",
        metadata.dev(),
        metadata.ino(),
        metadata.size(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec()
    ))
}

/// Resolve an executable to the regular file that will be used for daemon
/// configuration. Callers pass the returned path to the child process so the
/// identity checked before spawn is also the path resolved by the daemon.
pub fn canonical_executable(path: &Path) -> Result<PathBuf, DaemonError> {
    let canonical = fs::canonicalize(path)?;
    let metadata = fs::symlink_metadata(&canonical)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(DaemonError::EndpointInconsistent(canonical));
    }
    Ok(canonical)
}

fn runtime_identity(daemon_program: &Path, stock_sbx: &Path) -> Result<String, DaemonError> {
    let daemon_program = canonical_executable(daemon_program)?;
    let stock_sbx = canonical_executable(stock_sbx)?;
    let daemon_digest = artifact_identity(&daemon_program)?;
    let sbx_digest = artifact_identity(&stock_sbx)?;
    let mut digest = Sha256::new();
    digest.update(b"marsh-daemon-runtime-v2\0");
    for (path, artifact) in [
        (daemon_program.as_path(), daemon_digest.as_str()),
        (stock_sbx.as_path(), sbx_digest.as_str()),
    ] {
        let path = path.as_os_str().as_encoded_bytes();
        digest.update(path.len().to_be_bytes());
        digest.update(path);
        digest.update(artifact.len().to_be_bytes());
        digest.update(artifact.as_bytes());
    }
    Ok(format!("sha256:{}", hex(&digest.finalize())))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope<T> {
    protocol: String,
    token: String,
    body: T,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PublicRequest {
    Ping,
    Shutdown,
    RegisteredCommands,
    RegisteredKits,
    ResetWorkers {
        selection: LoadSelection,
    },
    ResetScope,
    StopScope,
    Prepare {
        selection: LoadSelection,
        session: SessionSpec,
        /// `marsh --dev --load`: the session attaches to the dev shell VM.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        dev: bool,
    },
    Execute(ExecuteSpec),
    OpenShell(ShellSpec),
    AttachShell {
        pid: u32,
        session: SessionAuthority,
    },
    /// Host-only attachment that preserves a published project identity.
    AttachPinnedShell {
        pid: u32,
        session: SessionAuthority,
        expected_project_identity: (u64, u64),
    },
    /// Host-authenticated private home allocation; the token is never part of
    /// guest `SessionSpec` or a relay-owned attachment request.
    AttachEphemeralShell {
        pid: u32,
        session: SessionAuthority,
        expected_project_identity: Option<(u64, u64)>,
        token: marsh_sbx::EphemeralHomeToken,
    },
    DetachShell {
        session_id: String,
    },
    Status {
        session_id: Option<String>,
    },
    ProcessView {
        session: SessionSpec,
    },
    Jobs,
    ShowJob {
        job_id: String,
    },
    McpPublish {
        session: SessionSpec,
        name: String,
        description: Option<String>,
        sandbox: Option<String>,
        #[serde(default)]
        kit: Option<String>,
        pipeline: String,
    },
    McpLoad {
        session: SessionSpec,
        name: String,
        kit: Option<String>,
        sandbox: Option<String>,
    },
    McpUnpublish {
        session: SessionSpec,
        name: String,
    },
    AcpStart {
        adapter: String,
        session: SessionSpec,
        #[serde(default)]
        reservation_id: Option<String>,
    },
    AcpReserve {
        adapter: String,
        session: SessionSpec,
    },
    AcpPrompt {
        agent_session_id: String,
        session: SessionSpec,
        operation_id: String,
        text: String,
    },
    AcpCancel {
        agent_session_id: String,
        session: SessionSpec,
    },
    AcpRespond {
        agent_session_id: String,
        session: SessionSpec,
        request_id: String,
        option_id: String,
    },
    AcpStatus {
        agent_session_id: String,
        session: SessionSpec,
        after: u64,
    },
    AcpList {
        session: SessionSpec,
    },
    AcpAttach {
        agent_session_id: String,
        session: SessionSpec,
    },
    AcpRelease {
        agent_session_id: String,
        session: SessionSpec,
    },
    AcpStop {
        agent_session_id: String,
        session: SessionSpec,
    },
    AcpPublish {
        agent_session_id: String,
        session: SessionSpec,
        name: String,
        sandbox: Option<String>,
        #[serde(default)]
        kit: Option<String>,
    },
    AcpUnpublish {
        session: SessionSpec,
        name: String,
    },
    AcpPublishedPrompt {
        agent_session_id: String,
        generation: String,
        operation_id: String,
        text: String,
    },
    AcpPublishedStatus {
        agent_session_id: String,
        generation: String,
        after: u64,
    },
    AcpPublishedCancel {
        agent_session_id: String,
        generation: String,
    },
    AcpPublishedRespond {
        agent_session_id: String,
        generation: String,
        request_id: String,
        option_id: String,
    },
    /// Run one stock `sbx` command for the caller's development grant.
    /// Authorized only by an attached relay; the session comes from the relay.
    DevSbx {
        argv: Vec<String>,
        pty: Option<TerminalSize>,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    InstallKit {
        command: String,
        reference: String,
    },
    /// `marsh split`: streamed stdin frames follow (`docs/design/workspaces.md` s3).
    SplitCreate(split::SplitCreateSpec),
    /// Creator only; a `SplitRelease` frame follows the reply.
    SplitJoin {
        split: String,
    },
    SplitShow {
        split: Option<String>,
    },
    SplitCancel {
        split: String,
    },
    SplitRemove {
        split: String,
    },
    /// A job's `cap.sock` child: attachment frames follow, as for `Execute`
    /// (`docs/design/processes.md` s3).
    ProcessRun(ExecuteSpec),
    /// The forest (host), or the caller job's own subtree (`cap.sock`).
    ProcessShow,
}

/// Host-approved identity and paths captured before entering the guest VM.
/// Relay clients never get to replace these values.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionAuthority {
    pub username: String,
    pub uid: u32,
    pub gid: u32,
    pub launch_directory: PathBuf,
    pub guest_home: PathBuf,
    pub home_backing: PathBuf,
    pub ephemeral_home: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PublicReply {
    Pong {
        daemon_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        build_identity: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
    },
    ShuttingDown,
    RegisteredCommands {
        commands: Vec<String>,
    },
    RegisteredKits {
        kits: BTreeMap<String, String>,
    },
    WorkersReset {
        kits: Vec<String>,
    },
    ScopeLifecycle(ScopeLifecycleReport),
    PreparationAccepted,
    ExecutionAccepted,
    ShellAccepted,
    ShellAttached {
        session_id: String,
    },
    Detached,
    Status(StatusDocument),
    ProcessTree {
        document: serde_json::Value,
    },
    ProcessView(ProcessViewDocument),
    Jobs(JobsDocument),
    Job(Box<JobReceipt>),
    McpPublication {
        outcome: PublicationOutcome,
    },
    AcpStarted {
        agent_session_id: String,
        job_id: String,
    },
    AcpReserved {
        agent_session_id: String,
    },
    AcpAccepted,
    AcpPromptAccepted {
        turn_id: String,
    },
    AcpStatus(Box<AcpSessionStatus>),
    AcpSessions {
        sessions: Vec<AcpSessionSummary>,
    },
    AcpCancelled {
        phase: String,
    },
    AcpPublication {
        outcome: PublicationOutcome,
    },
    /// Same bounded wire frame as `PreparationFrame::ColdBoot`, forwarded only
    /// during an admitted publication with a Kit target.
    #[serde(rename = "cold_boot")]
    AcpColdBoot {
        kit: String,
    },
    InstalledKit {
        command: String,
        reference: String,
    },
    SplitStarted {
        id: String,
    },
    SplitFinished {
        id: String,
        status: i32,
        cancelled: bool,
        message: Option<String>,
    },
    SplitJoined {
        id: String,
        manifest: String,
        rendering: Vec<u8>,
        dir: PathBuf,
        objects: Option<PathBuf>,
    },
    SplitDone {
        message: String,
    },
    Splits {
        document: serde_json::Value,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeLifecycleAction {
    Reset,
    Stop,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeCleanupState {
    Removed,
    Absent,
    CleanupUncertain,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeCleanupComponent {
    pub kind: String,
    pub label: String,
    pub state: ScopeCleanupState,
    /// Exact stock VM name this component acted on, when it names one VM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeLifecycleReport {
    pub action: ScopeLifecycleAction,
    pub cleanup_complete: bool,
    pub components: Vec<ScopeCleanupComponent>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadSelection {
    All,
    Kits(Vec<String>),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSpec {
    pub session_id: String,
    pub username: String,
    pub uid: u32,
    pub gid: u32,
    pub launch_directory: PathBuf,
    pub guest_home: PathBuf,
    pub home_backing: PathBuf,
    pub ephemeral_home: bool,
    pub terminal: bool,
    pub terminal_size: Option<TerminalSize>,
}

/// Explicit execution pool selected by the shell for one registered command.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    #[default]
    Local,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecuteSpec {
    pub command: String,
    pub arguments: Vec<Vec<u8>>,
    #[serde(default)]
    pub placement: Placement,
    /// Bounded exported shell values for this invocation, never journaled.
    #[serde(default)]
    pub environment: marsh_contracts::ExportedEnvironment,
    /// Optional invocation cwd; the authenticated session retains mount authority.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "marsh_contracts::byte_path::optional"
    )]
    pub working_directory: Option<PathBuf>,
    pub session: SessionSpec,
    /// Parent job and requested spawn set (`docs/design/processes.md` s6). Only
    /// the daemon's own endpoint may name a parent; relays may only narrow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<process::ProcessLink>,
}

impl ExecuteSpec {
    /// The split branch confinement for this job, if it is a branch job.
    /// Only meaningful after `authorize_request_session` has replaced the
    /// session roots with retained authority.
    ///
    /// # Errors
    /// Returns why a claimed or implied branch workspace is not admitted.
    pub fn split_confinement(
        &self,
    ) -> Result<Option<split_confinement::BranchConfinement>, String> {
        split_confinement::branch_confinement(
            &self.session.launch_directory,
            self.effective_working_directory(),
        )
    }

    /// Resolve the legacy missing-cwd case without changing session authority.
    /// Callers must retain normal request authentication and cwd admission.
    #[must_use]
    pub fn effective_working_directory(&self) -> &Path {
        self.working_directory
            .as_deref()
            .unwrap_or(&self.session.launch_directory)
    }

    fn working_directory_is_admitted(&self) -> bool {
        let cwd = self.effective_working_directory();
        marsh_contracts::byte_path::validate(cwd).is_ok()
            && (cwd.starts_with(&self.session.launch_directory)
                || cwd.starts_with(&self.session.guest_home))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShellSpec {
    pub arguments: Vec<Vec<u8>>,
    pub session: SessionSpec,
    /// `marsh --dev`: attach with a development grant (host checks policy).
    #[serde(default)]
    pub dev: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AttachmentFrame {
    ColdBoot {
        kit: String,
    },
    /// Guest preparation completed; terminal input may now be admitted.
    ShellReady,
    /// A rejected control operation is not a lost controller or terminal result.
    ControlError {
        code: AttachmentControlError,
        operation: String,
        message: String,
    },
    JobStarted {
        job_id: String,
    },
    Stdin {
        bytes: Vec<u8>,
    },
    StdinEof,
    StdinCredit,
    StdinClosed,
    Signal {
        signal: String,
    },
    Resize {
        rows: u16,
        columns: u16,
    },
    Stdout {
        bytes: Vec<u8>,
    },
    Stderr {
        bytes: Vec<u8>,
    },
    Exited {
        code: i32,
    },
    Failed {
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentControlError {
    UnsupportedSignal,
    SignalDelivery,
    ResizeDelivery,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PreparationFrame {
    ColdBoot { kit: String },
    Complete { result: PreparationResult },
    Failed { message: String },
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PreparationResult {
    pub cold_kits: Vec<String>,
    /// Exact sandbox selected for each prepared command, including warm Kits.
    pub sandboxes: BTreeMap<String, String>,
}

pub trait DaemonBackend: Send + Sync + 'static {
    fn validate_ephemeral_home(
        &self,
        _session: &SessionAuthority,
        _token: &marsh_sbx::EphemeralHomeToken,
    ) -> Result<(), DaemonError> {
        Err(DaemonError::BackendUnavailable)
    }

    fn close_ephemeral_session(
        &self,
        _session: &str,
        _store: &DaemonStore,
    ) -> Result<(), DaemonError> {
        Err(DaemonError::BackendUnavailable)
    }

    fn registered_commands(&self) -> Result<Vec<String>, DaemonError>;
    fn install_kit(
        &self,
        _command: String,
        _reference: String,
        _store: DaemonStore,
    ) -> Result<String, DaemonError> {
        Err(DaemonError::BackendUnavailable)
    }
    fn resolve_acp_agent(&self, _name: &str) -> Result<AgentAdapterDeclaration, DaemonError> {
        Err(DaemonError::BackendUnavailable)
    }
    fn registered_kits(&self) -> Result<BTreeMap<String, String>, DaemonError> {
        Ok(BTreeMap::new())
    }
    fn reset_workers(
        &self,
        _selection: &LoadSelection,
        _store: DaemonStore,
    ) -> Result<Vec<String>, DaemonError> {
        Err(DaemonError::BackendUnavailable)
    }
    fn teardown_scope(
        &self,
        action: ScopeLifecycleAction,
        _store: DaemonStore,
        _deadline: Instant,
    ) -> ScopeLifecycleReport {
        ScopeLifecycleReport {
            action,
            cleanup_complete: false,
            components: vec![ScopeCleanupComponent {
                kind: "scope".into(),
                label: "runtime".into(),
                state: ScopeCleanupState::CleanupUncertain,
                vm: None,
                detail: Some(DaemonError::BackendUnavailable.to_string()),
            }],
        }
    }
    fn prepare(
        &self,
        selection: &LoadSelection,
        session: &SessionSpec,
        progress: PreparationProgress,
        store: DaemonStore,
    ) -> Result<PreparationResult, DaemonError>;
    /// `--load` for a `--dev` session, which attaches to the dev shell VM:
    /// a backend that warms a shell VM while it prepares Kits warms that one.
    fn prepare_dev(
        &self,
        selection: &LoadSelection,
        session: &SessionSpec,
        progress: PreparationProgress,
        store: DaemonStore,
    ) -> Result<PreparationResult, DaemonError> {
        self.prepare(selection, session, progress, store)
    }
    fn execute(
        &self,
        request: ExecuteSpec,
        attachment: ServerAttachment,
        store: DaemonStore,
    ) -> Result<(), DaemonError>;
    fn open_shell(
        &self,
        request: ShellSpec,
        attachment: ServerAttachment,
        store: DaemonStore,
    ) -> Result<(), DaemonError>;
}

#[derive(Debug)]
struct UnavailableBackend;

impl DaemonBackend for UnavailableBackend {
    fn registered_commands(&self) -> Result<Vec<String>, DaemonError> {
        Err(DaemonError::BackendUnavailable)
    }

    fn prepare(
        &self,
        _selection: &LoadSelection,
        _session: &SessionSpec,
        _progress: PreparationProgress,
        _store: DaemonStore,
    ) -> Result<PreparationResult, DaemonError> {
        Err(DaemonError::BackendUnavailable)
    }

    fn execute(
        &self,
        _request: ExecuteSpec,
        _attachment: ServerAttachment,
        _store: DaemonStore,
    ) -> Result<(), DaemonError> {
        Err(DaemonError::BackendUnavailable)
    }

    fn open_shell(
        &self,
        _request: ShellSpec,
        _attachment: ServerAttachment,
        _store: DaemonStore,
    ) -> Result<(), DaemonError> {
        Err(DaemonError::BackendUnavailable)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unauthorized,
    InvalidRequest,
    NotFound,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EndpointOwner {
    pub uid: u32,
    pub pid: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ShellStatus {
    pub session_id: String,
    pub daemon_id: String,
    pub pid: u32,
    pub project: PathBuf,
    pub state: ShellState,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellState {
    Attached,
    Detached,
    /// Controller is gone, but guest cleanup and authority release are unproved.
    CleanupUncertain,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkerStatus {
    pub worker_id: String,
    pub vm_id: String,
    pub scope_id: String,
    #[serde(rename = "kit_profile")]
    pub kit_ref: String,
    /// Registered command (Kit) names served by this worker's workload.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kits: Vec<String>,
    pub warm: bool,
    pub health: WorkerHealth,
    pub container_capacity: u16,
    pub active_container_ids: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerHealth {
    Ready,
    Repairing,
    Quarantined,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StatusDocument {
    pub schema: String,
    pub features: Vec<String>,
    pub scope_id: String,
    pub daemon_id: String,
    pub endpoint_owner: EndpointOwner,
    pub current_session_id: Option<String>,
    /// Actual canonical host-only configuration and receipt directory.
    pub control_home: PathBuf,
    /// None for an inspection-only server without an execution backend.
    pub job_defaults: Option<JobDefaults>,
    pub shells: Vec<ShellStatus>,
    pub workers: Vec<WorkerStatus>,
    /// Active, awaiting, and kept splits (`docs/design/workspaces.md` s5).
    #[serde(default)]
    pub splits: split::SplitCounts,
    /// Running jobs, refused spawns, and slots held by uncertain jobs
    /// (`docs/design/processes.md` s9).
    #[serde(default)]
    pub processes: process::ProcessCounts,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProcessViewDocument {
    pub schema: String,
    pub observed_unix_ms: u64,
    pub daemon_id: String,
    pub truncated: bool,
    pub shells: Vec<ProcessShell>,
    pub starting_kits: Vec<ProcessStartingKit>,
    pub jobs: Vec<ProcessJob>,
    pub acp_sessions: Vec<AcpSessionSummary>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProcessShell {
    pub session_id: String,
    pub host_attachment_pid: u32,
    pub state: ShellState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProcessStartingKit {
    pub invocation_id: String,
    pub shell_session_id: String,
    pub command: String,
    pub placement: Placement,
    pub started_unix_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProcessJob {
    pub job_id: String,
    pub shell_session_id: String,
    pub command: String,
    pub placement: Placement,
    pub state: JobState,
    pub cleanup: CleanupState,
    pub created_unix_ms: u64,
    pub worker_id: Option<String>,
    pub worker_health: Option<WorkerHealth>,
    pub vm_id: Option<String>,
    pub container_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JobsDocument {
    pub schema: String,
    pub jobs: Vec<JobSummary>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JobSummary {
    pub cursor: u64,
    pub job_id: String,
    pub command: String,
    pub placement: Placement,
    pub cleanup: CleanupState,
    pub state: JobState,
    pub exit_code: Option<i32>,
    pub wall_ms: u64,
    /// `lineage.parent` (`session:<id>`, `job:<id>`, or `split:<id>/<label>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Finished,
    Failed,
    Cancelled,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupState {
    Pending,
    Verified,
    Uncertain,
    NotRequired,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PublicMount {
    pub target: PathBuf,
    pub access: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub cause: String,
}

/// Public exit status used when exact cleanup cannot be verified.
pub const CLEANUP_UNCERTAIN_EXIT_CODE: i32 = 125;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TimingReport {
    pub durations_ms: BTreeMap<String, u64>,
    pub milestones_unix_ms: BTreeMap<String, u64>,
    pub wall_ms: u64,
    pub orchestration_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JobReceipt {
    pub schema: String,
    pub cursor: u64,
    pub job_id: String,
    pub attempt_id: String,
    pub session_id: String,
    pub command: String,
    /// The invocation's arguments for display (`jobs`, `jobs --tree`):
    /// at most `DISPLAY_ARGS` of them, each cut to `DISPLAY_ARG_CHARS`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default)]
    pub placement: Placement,
    pub state: JobState,
    #[serde(rename = "kit_profile")]
    pub kit_ref: String,
    #[serde(rename = "image")]
    pub workload_image: String,
    pub mounts: Vec<PublicMount>,
    pub worker_id: Option<String>,
    pub vm_id: Option<String>,
    pub container_id: Option<String>,
    /// Actual observed execution, independent of the public settlement code.
    #[serde(default)]
    pub execution: ExecutionOutcome,
    pub exit: Option<ExitStatus>,
    pub output_complete: bool,
    pub cleanup: CleanupState,
    pub timing: TimingReport,
    pub created_unix_ms: u64,
    pub finished_unix_ms: Option<u64>,
    /// Split lineage for a job that runs in a split fork (`docs/design/workspaces.md` s5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage: Option<JobLineage>,
    /// Child job ids (`jobs show`), filled when shown; never journaled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<String>,
}

/// Receipt lineage (`docs/design/processes.md` s9): `parent` is `session:<id>`,
/// `job:<id>`, or `split:<id>/<label>` (a split branch's job), `root` the
/// root job id, `depth` 1 for a root, `spawn` the
/// names it may start, and `split`/`label` for a job in a split fork.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct JobLineage {
    #[serde(default)]
    pub parent: String,
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub depth: u32,
    #[serde(default)]
    pub spawn: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Wall-time deadline (unix ms): the job's own limit, or its parent's
    /// deadline when that is earlier (`docs/design/processes.md` s6). A child
    /// inherits it at admission; the backend sets the effective value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_unix_ms: Option<u64>,
    /// The deadline is the parent's, not the job's own limit.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parent_deadline: bool,
    /// The split whose results this root job consumed: it was started by a
    /// stage after `join` (`SPLIT_ID` set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumes: Option<String>,
    /// `<fanout-id>/<label>`: the `fanout` branch that started this root job
    /// (`FANOUT_BRANCH` set). Display grouping for `jobs --tree` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fanout: Option<String>,
}

impl JobLineage {
    /// The fork a confined job mounts read-write: `.../.marsh/split/<id>/<label>`.
    fn split_of(mounts: &[PublicMount]) -> Option<(String, String)> {
        mounts.iter().find_map(|mount| {
            let parts = mount
                .target
                .components()
                .rev()
                .take(4)
                .map(|part| part.as_os_str().to_str())
                .collect::<Option<Vec<_>>>()?;
            (mount.access == "read_write"
                && parts.len() == 4
                && parts[3] == ".marsh"
                && parts[2] == "split")
                .then(|| (parts[1].to_owned(), parts[0].to_owned()))
        })
    }
}

/// Arguments a receipt keeps for display (`JobReceipt::args`).
pub const DISPLAY_ARGS: usize = 16;
/// Characters kept of each displayed argument.
pub const DISPLAY_ARG_CHARS: usize = 200;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewJob {
    pub session_id: String,
    pub command: String,
    pub kit_ref: String,
    pub workload_image: String,
    pub mounts: Vec<PublicMount>,
}

#[derive(Debug)]
struct State {
    scope_id: String,
    daemon_id: String,
    control_home: PathBuf,
    job_defaults: Option<JobDefaults>,
    owner: EndpointOwner,
    shells: BTreeMap<String, ShellStatus>,
    session_authorities: BTreeMap<String, SessionAuthority>,
    session_project_identities: BTreeMap<String, Option<(u64, u64)>>,
    session_home_backings: BTreeMap<String, PathBuf>,
    ephemeral_home_tokens: BTreeMap<String, marsh_sbx::EphemeralHomeToken>,
    // Host-only cleanup ownership, retained when development authority is revoked.
    shell_cleanup_vms: BTreeMap<String, String>,
    shell_attachments: BTreeMap<String, String>,
    workers: BTreeMap<String, WorkerStatus>,
    starting_kits: BTreeMap<String, ProcessStartingKit>,
    jobs: BTreeMap<String, JobReceipt>,
    next_cursor: u64,
    journal: receipt_journal::ReceiptJournal,
    authentication_tokens: BTreeMap<String, Option<String>>,
    capability_tokens: BTreeMap<String, String>,
    reservations: BTreeMap<String, String>,
    ordinary_requests: usize,
    lifecycle_active: bool,
    process: process::ProcessTable,
    /// Session of each running split shell branch -> (split, label): a Kit
    /// job it starts is that branch's child (`docs/design/processes.md` s9).
    branch_sessions: BTreeMap<String, (String, String)>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AuthenticationScope {
    Master,
    Relay(String),
    /// A split branch's job through its capability socket: `SplitCreate`
    /// and `SplitJoin` only (`docs/design/workspaces.md` s4).
    Capability(String),
}

/// Every new request must explicitly choose its transport authority. Keeping
/// this exhaustive prevents a new host operation from inheriting relay access
/// through an unrelated deny-list's wildcard.
enum RequestAuthority {
    Shared,
    HostLifecycle,
    HostPublishedAcp,
    RelayDevelopment,
    RelayMcp,
    RelayAcp,
}

impl PublicRequest {
    fn authority(&self) -> RequestAuthority {
        match self {
            Self::AttachShell { .. }
            | Self::AttachPinnedShell { .. }
            | Self::AttachEphemeralShell { .. }
            | Self::Shutdown
            | Self::ResetWorkers { .. }
            | Self::ResetScope
            | Self::StopScope => RequestAuthority::HostLifecycle,
            Self::AcpPublishedPrompt { .. }
            | Self::AcpPublishedStatus { .. }
            | Self::AcpPublishedCancel { .. }
            | Self::AcpPublishedRespond { .. } => RequestAuthority::HostPublishedAcp,
            Self::DevSbx { .. } => RequestAuthority::RelayDevelopment,
            Self::McpPublish { .. } | Self::McpLoad { .. } | Self::McpUnpublish { .. } => {
                RequestAuthority::RelayMcp
            }
            Self::AcpPublish { .. } | Self::AcpUnpublish { .. } => RequestAuthority::RelayAcp,
            Self::Ping
            | Self::RegisteredCommands
            | Self::RegisteredKits
            | Self::Prepare { .. }
            | Self::Execute(_)
            | Self::OpenShell(_)
            | Self::DetachShell { .. }
            | Self::Status { .. }
            | Self::ProcessView { .. }
            | Self::Jobs
            | Self::ShowJob { .. }
            | Self::AcpStart { .. }
            | Self::AcpReserve { .. }
            | Self::AcpPrompt { .. }
            | Self::AcpCancel { .. }
            | Self::AcpRespond { .. }
            | Self::AcpStatus { .. }
            | Self::AcpList { .. }
            | Self::AcpAttach { .. }
            | Self::AcpRelease { .. }
            | Self::AcpStop { .. }
            | Self::InstallKit { .. }
            | Self::SplitCreate(_)
            | Self::SplitJoin { .. }
            | Self::SplitShow { .. }
            | Self::SplitCancel { .. }
            | Self::SplitRemove { .. }
            | Self::ProcessRun(_)
            | Self::ProcessShow => RequestAuthority::Shared,
        }
    }
}

impl RequestAuthority {
    fn rejection(&self, caller: &AuthenticationScope) -> Option<&'static str> {
        match (self, caller) {
            (Self::HostLifecycle, AuthenticationScope::Relay(_)) => {
                Some("relay token cannot perform host lifecycle operations")
            }
            (Self::HostPublishedAcp, AuthenticationScope::Relay(_)) => {
                Some("published ACP control requires the host owner")
            }
            (Self::RelayDevelopment, AuthenticationScope::Master) => {
                Some("development scope requires an attached relay")
            }
            (Self::RelayMcp, AuthenticationScope::Master) => {
                Some("MCP publication requires an attached project-shell session token")
            }
            (Self::RelayAcp, AuthenticationScope::Master) => {
                Some("ACP publication requires an attached project-shell session token")
            }
            (_, AuthenticationScope::Capability(_))
            | (Self::Shared, AuthenticationScope::Master | AuthenticationScope::Relay(_))
            | (Self::HostLifecycle | Self::HostPublishedAcp, AuthenticationScope::Master)
            | (
                Self::RelayDevelopment | Self::RelayMcp | Self::RelayAcp,
                AuthenticationScope::Relay(_),
            ) => None,
        }
    }
}

/// Thread-safe state and typed integration seam for the SBX executor adapter.
#[derive(Clone, Debug)]
pub struct DaemonStore(Arc<Mutex<State>>);

fn scope_is_idle(state: &State) -> bool {
    scope_can_recover(state)
        && state
            .shells
            .values()
            .all(|shell| shell.state == ShellState::Detached)
}

fn scope_can_recover(state: &State) -> bool {
    state
        .shells
        .values()
        .all(|shell| shell.state != ShellState::Attached)
        && state.starting_kits.is_empty()
        && state
            .jobs
            .values()
            .all(|job| !matches!(job.state, JobState::Queued | JobState::Running))
}

struct RequestAdmission {
    store: DaemonStore,
    lifecycle: bool,
}

impl Drop for RequestAdmission {
    fn drop(&mut self) {
        let mut state = self.store.lock();
        if self.lifecycle {
            state.lifecycle_active = false;
        } else {
            state.ordinary_requests = state
                .ordinary_requests
                .checked_sub(1)
                .expect("ordinary request admission count underflow");
        }
    }
}

impl DaemonStore {
    #[must_use]
    /// Opens a daemon store and panics when its owner-only receipt journal is
    /// unavailable. Production daemon startup uses the fallible [`Self::open`].
    ///
    /// # Panics
    /// Panics when the journal cannot be opened or safely replayed.
    pub fn new(home: &Path) -> Self {
        Self::open(home).expect("daemon store must open")
    }

    pub fn open(home: &Path) -> Result<Self, DaemonError> {
        Self::with_master_token(home, home, None)
    }

    fn with_master_token(
        home: &Path,
        journal_home: &Path,
        master_token: Option<String>,
    ) -> Result<Self, DaemonError> {
        let canonical = fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
        let scope_id = hex(&Sha256::digest(canonical.as_os_str().as_encoded_bytes()));
        let (mut journal, mut jobs) = receipt_journal::ReceiptJournal::open(journal_home)?;
        let mut repaired = false;
        for receipt in jobs.values_mut() {
            if matches!(receipt.state, JobState::Queued | JobState::Running) {
                receipt.state = JobState::Unknown;
                if matches!(
                    receipt.execution,
                    ExecutionOutcome::NotStarted | ExecutionOutcome::Unknown
                ) {
                    receipt.execution = ExecutionOutcome::Unknown;
                }
                receipt.exit = Some(ExitStatus {
                    code: Some(125),
                    cause: "daemon_restarted".into(),
                });
                receipt.output_complete = false;
                receipt.cleanup = CleanupState::Uncertain;
                receipt.finished_unix_ms = Some(unix_time_ms());
                journal.append(receipt, true)?;
                repaired = true;
            }
        }
        let trimmed = receipt_journal::trim_receipts(&mut jobs);
        if repaired || trimmed {
            journal.compact(&jobs)?;
        }
        let next_cursor = jobs
            .values()
            .map(|receipt| receipt.cursor)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| DaemonError::InvalidState("receipt cursor exhausted".into()))?;
        let mut authentication_tokens = BTreeMap::new();
        if let Some(token) = master_token {
            authentication_tokens.insert(token, None);
        }
        Ok(Self(Arc::new(Mutex::new(State {
            scope_id,
            daemon_id: Uuid::new_v4().to_string(),
            control_home: fs::canonicalize(journal_home)?,
            job_defaults: None,
            owner: EndpointOwner {
                uid: rustix::process::getuid().as_raw(),
                pid: std::process::id(),
            },
            shells: BTreeMap::new(),
            session_authorities: BTreeMap::new(),
            session_project_identities: BTreeMap::new(),
            session_home_backings: BTreeMap::new(),
            ephemeral_home_tokens: BTreeMap::new(),
            shell_cleanup_vms: BTreeMap::new(),
            shell_attachments: BTreeMap::new(),
            workers: BTreeMap::new(),
            starting_kits: BTreeMap::new(),
            jobs,
            next_cursor,
            journal,
            authentication_tokens,
            capability_tokens: BTreeMap::new(),
            process: process::ProcessTable::load(journal_home),
            reservations: BTreeMap::new(),
            ordinary_requests: 0,
            lifecycle_active: false,
            branch_sessions: BTreeMap::new(),
        }))))
    }

    /// Issue a shell-session-scoped bearer for the trusted guest relay. The
    /// token belongs under the shell VM's private `/run`, never selected home.
    pub fn issue_relay_token(&self, session_id: &str) -> Result<String, DaemonError> {
        let mut state = self.lock();
        if !state.shells.contains_key(session_id) {
            return Err(DaemonError::NotFound(session_id.into()));
        }
        let token = random_token()?;
        state
            .authentication_tokens
            .insert(token.clone(), Some(session_id.into()));
        Ok(token)
    }

    pub fn revoke_relay_token(&self, token: &str) {
        self.lock().authentication_tokens.remove(token);
    }

    /// A per-attempt split capability for one job; revoked at its end and
    /// never persisted (a restart revokes every capability).
    pub fn issue_capability_token(&self, job_id: &str) -> Result<String, DaemonError> {
        let token = random_token()?;
        self.lock()
            .capability_tokens
            .insert(token.clone(), job_id.into());
        Ok(token)
    }

    pub fn revoke_capability_token(&self, token: &str) {
        self.lock().capability_tokens.remove(token);
    }

    /// The session spec a job's split requests run under.
    fn job_session(&self, job_id: &str) -> Option<SessionSpec> {
        let state = self.lock();
        let session_id = state.jobs.get(job_id)?.session_id.clone();
        let authority = state.session_authorities.get(&session_id)?;
        Some(SessionSpec {
            session_id,
            username: authority.username.clone(),
            uid: authority.uid,
            gid: authority.gid,
            launch_directory: authority.launch_directory.clone(),
            guest_home: authority.guest_home.clone(),
            home_backing: authority.home_backing.clone(),
            ephemeral_home: authority.ephemeral_home,
            terminal: false,
            terminal_size: None,
        })
    }

    fn authentication_scope(&self, token: &str) -> Option<AuthenticationScope> {
        let state = self.lock();
        if let Some(job) = state
            .capability_tokens
            .iter()
            .find_map(|(candidate, job)| constant_time_eq(candidate, token).then(|| job.clone()))
        {
            return Some(AuthenticationScope::Capability(job));
        }
        drop(state);
        self.lock()
            .authentication_tokens
            .iter()
            .find_map(|(candidate, owner)| {
                constant_time_eq(candidate, token).then(|| {
                    owner
                        .clone()
                        .map_or(AuthenticationScope::Master, AuthenticationScope::Relay)
                })
            })
    }

    #[must_use]
    pub fn daemon_id(&self) -> String {
        self.lock().daemon_id.clone()
    }

    fn safe_to_shutdown(&self) -> bool {
        let state = self.lock();
        scope_is_idle(&state)
    }

    fn begin_ordinary_request(&self) -> Result<RequestAdmission, DaemonError> {
        let mut state = self.lock();
        if state.lifecycle_active {
            return Err(DaemonError::InvalidState(
                "daemon lifecycle transition is active".into(),
            ));
        }
        state.ordinary_requests = state
            .ordinary_requests
            .checked_add(1)
            .ok_or_else(|| DaemonError::InvalidState("request admission exhausted".into()))?;
        Ok(RequestAdmission {
            store: self.clone(),
            lifecycle: false,
        })
    }

    fn begin_lifecycle_request(&self) -> Result<RequestAdmission, DaemonError> {
        let mut state = self.lock();
        if state.lifecycle_active {
            return Err(DaemonError::InvalidState(
                "daemon lifecycle transition is already active".into(),
            ));
        }
        if state.ordinary_requests != 0 || !scope_can_recover(&state) {
            return Err(DaemonError::InvalidState(BUSY_LIFECYCLE_MESSAGE.into()));
        }
        state.lifecycle_active = true;
        Ok(RequestAdmission {
            store: self.clone(),
            lifecycle: true,
        })
    }

    #[must_use]
    pub fn attach_shell(&self, pid: u32, authority: SessionAuthority) -> String {
        let identity = project_identity(&authority.launch_directory);
        self.attach_session(pid, authority, ShellState::Attached, identity)
    }

    /// Attach a host caller without allowing pathname replacement to rebind its project.
    ///
    /// # Errors
    /// Returns an error when the current canonical project differs from the pin.
    pub fn attach_pinned_shell(
        &self,
        pid: u32,
        authority: SessionAuthority,
        expected_project_identity: (u64, u64),
    ) -> Result<String, DaemonError> {
        if project_identity(&authority.launch_directory) != Some(expected_project_identity) {
            return Err(DaemonError::InvalidState(
                "published project identity changed before attachment".into(),
            ));
        }
        // Preserve the expected identity even if the pathname changes immediately
        // after the check. The backend compares its retained directory descriptor.
        Ok(self.attach_session(
            pid,
            authority,
            ShellState::Attached,
            Some(expected_project_identity),
        ))
    }

    /// The immutable project identity captured or pinned at host attachment.
    #[must_use]
    pub fn session_project_identity(&self, session_id: &str) -> Option<(u64, u64)> {
        self.lock()
            .session_project_identities
            .get(session_id)
            .copied()
            .flatten()
    }

    /// Host-only opaque home capability; never obtained from guest `SessionSpec`.
    #[must_use]
    pub fn ephemeral_home_token(&self, session: &str) -> Option<marsh_sbx::EphemeralHomeToken> {
        self.lock().ephemeral_home_tokens.get(session).cloned()
    }

    /// Host lifecycle recovery can fence this daemon's retained allocations;
    /// this is not an enumeration of unrelated per-UID slots.
    #[must_use]
    pub fn retained_ephemeral_home_tokens(&self) -> Vec<(String, marsh_sbx::EphemeralHomeToken)> {
        self.lock()
            .ephemeral_home_tokens
            .iter()
            .map(|(session, token)| (session.clone(), token.clone()))
            .collect()
    }

    /// Forget the host selector only after actual session/VM cleanup. Durable
    /// private authority is separately released by the slot lease transaction.
    pub fn finish_ephemeral_session(&self, session: &str) {
        self.lock().ephemeral_home_tokens.remove(session);
    }

    fn attach_session(
        &self,
        pid: u32,
        authority: SessionAuthority,
        shell_state: ShellState,
        project_identity: Option<(u64, u64)>,
    ) -> String {
        let mut state = self.lock();
        let session_id = Uuid::new_v4().to_string();
        let daemon_id = state.daemon_id.clone();
        state.shells.insert(
            session_id.clone(),
            ShellStatus {
                session_id: session_id.clone(),
                daemon_id,
                pid,
                project: authority.launch_directory.clone(),
                state: shell_state,
            },
        );
        state
            .session_authorities
            .insert(session_id.clone(), authority.clone());
        state
            .session_project_identities
            .insert(session_id.clone(), project_identity);
        state
            .session_home_backings
            .insert(session_id.clone(), authority.home_backing);
        session_id
    }

    #[must_use]
    pub(crate) fn shell_is_attached(&self, session_id: &str) -> bool {
        self.lock()
            .shells
            .get(session_id)
            .is_some_and(|shell| shell.state == ShellState::Attached)
    }

    fn verify_publication_project(&self, session: &SessionSpec) -> Result<(u64, u64), DaemonError> {
        let expected = self
            .lock()
            .session_project_identities
            .get(&session.session_id)
            .copied()
            .flatten()
            .ok_or_else(|| {
                DaemonError::InvalidState("attached project identity is unavailable".into())
            })?;
        if project_identity(&session.launch_directory) != Some(expected) {
            return Err(DaemonError::InvalidState(
                "attached project directory was replaced or is no longer canonical".into(),
            ));
        }
        Ok(expected)
    }

    fn authorize_session(
        &self,
        token: &str,
        requested: &SessionSpec,
    ) -> Result<SessionSpec, DaemonError> {
        let state = self.lock();
        let scope = state
            .authentication_tokens
            .iter()
            .find_map(|(candidate, owner)| {
                constant_time_eq(candidate, token).then(|| {
                    owner
                        .clone()
                        .map_or(AuthenticationScope::Master, AuthenticationScope::Relay)
                })
            })
            .ok_or_else(|| DaemonError::InvalidState("daemon authentication failed".into()))?;
        if matches!(scope, AuthenticationScope::Relay(owner) if owner != requested.session_id) {
            return Err(DaemonError::InvalidState(
                "relay token does not own the requested session".into(),
            ));
        }
        let authority = state
            .session_authorities
            .get(&requested.session_id)
            .ok_or_else(|| DaemonError::NotFound(requested.session_id.clone()))?;
        Ok(SessionSpec {
            session_id: requested.session_id.clone(),
            username: authority.username.clone(),
            uid: authority.uid,
            gid: authority.gid,
            launch_directory: authority.launch_directory.clone(),
            guest_home: authority.guest_home.clone(),
            home_backing: authority.home_backing.clone(),
            ephemeral_home: authority.ephemeral_home,
            terminal: requested.terminal,
            terminal_size: requested.terminal_size,
        })
    }

    /// Kit jobs of `session` that are queued, starting, or running.
    #[must_use]
    pub fn session_jobs_active(&self, session: &str) -> usize {
        let state = self.lock();
        state
            .jobs
            .values()
            .filter(|job| {
                job.session_id == session
                    && matches!(job.state, JobState::Queued | JobState::Running)
            })
            .count()
            + state
                .starting_kits
                .values()
                .filter(|kit| kit.shell_session_id == session)
                .count()
    }

    /// Whether a shell VM is already bound to this session (relay callers).
    #[must_use]
    pub fn shell_vm_bound(&self, session_id: &str) -> bool {
        self.lock().shell_cleanup_vms.contains_key(session_id)
    }

    /// Host backend binding only: public/relay payloads cannot select a VM to remove.
    pub fn bind_shell_cleanup_vm(&self, session_id: &str, vm: &str) -> Result<(), DaemonError> {
        let mut state = self.lock();
        if !state
            .shells
            .get(session_id)
            .is_some_and(|shell| shell.state == ShellState::Attached)
            || state
                .shell_cleanup_vms
                .get(session_id)
                .is_some_and(|existing| existing != vm)
        {
            return Err(DaemonError::InvalidState(
                "shell cleanup binding changed or session is not attached".into(),
            ));
        }
        state
            .shell_cleanup_vms
            .insert(session_id.to_owned(), vm.to_owned());
        Ok(())
    }

    /// Finalize only sessions bound to the VM whose full removal/healthy UUID
    /// absence the host adapter just verified. Never called after mere umount.
    pub fn complete_shell_vm_recovery(&self, vm: &str) -> Result<(), DaemonError> {
        let mut state = self.lock();
        if state.ordinary_requests != 0 || !scope_can_recover(&state) {
            return Err(DaemonError::InvalidState(BUSY_LIFECYCLE_MESSAGE.into()));
        }
        let sessions = state
            .shell_cleanup_vms
            .iter()
            .filter(|(_, name)| name.as_str() == vm)
            .map(|(session, _)| session.clone())
            .collect::<Vec<_>>();
        for session in sessions {
            if let Some(shell) = state.shells.get_mut(&session) {
                shell.state = ShellState::Detached;
            }
            state.session_authorities.remove(&session);
            state.shell_cleanup_vms.remove(&session);
            state
                .authentication_tokens
                .retain(|_, owner| owner.as_ref() != Some(&session));
        }
        Ok(())
    }

    /// Unrecovered shell records must keep an operator scope report incomplete.
    #[must_use]
    pub fn shell_cleanup_pending(&self) -> bool {
        self.lock()
            .shells
            .values()
            .any(|shell| shell.state == ShellState::CleanupUncertain)
    }

    pub fn mark_shell_cleanup_uncertain(&self, session_id: &str) {
        let mut state = self.lock();
        if let Some(shell) = state.shells.get_mut(session_id) {
            shell.state = ShellState::CleanupUncertain;
        }
        state
            .authentication_tokens
            .retain(|_, owner| owner.as_deref() != Some(session_id));
    }

    /// Sessions whose shell is still attached (not detached or cleanup-uncertain).
    #[must_use]
    pub fn attached_session_ids(&self) -> std::collections::BTreeSet<String> {
        self.lock()
            .shells
            .iter()
            .filter(|(_, shell)| shell.state == ShellState::Attached)
            .map(|(session, _)| session.clone())
            .collect()
    }

    pub fn detach_shell(&self, session_id: &str) -> Result<(), DaemonError> {
        let mut state = self.lock();
        state.branch_sessions.remove(session_id);
        shell_admission::detach_locked(&mut state, session_id)
    }

    /// Record that `session_id` is split `split`'s shell branch `label`
    /// until it detaches: Kit jobs it starts get that branch as parent.
    pub(crate) fn mark_branch_session(&self, session_id: &str, split: &str, label: &str) {
        self.lock()
            .branch_sessions
            .insert(session_id.into(), (split.into(), label.into()));
    }

    pub fn register_worker(&self, mut worker: WorkerStatus) -> Result<(), DaemonError> {
        let mut state = self.lock();
        if worker.scope_id != state.scope_id
            || worker.container_capacity == 0
            || !worker.active_container_ids.is_empty()
            || state.workers.contains_key(&worker.worker_id)
            || state
                .workers
                .values()
                .any(|existing| existing.vm_id == worker.vm_id)
        {
            return Err(DaemonError::InvalidState(
                "invalid or duplicate worker registration".into(),
            ));
        }
        worker.active_container_ids.sort();
        worker.active_container_ids.dedup();
        state.workers.insert(worker.worker_id.clone(), worker);
        Ok(())
    }

    /// Atomically block admission to every selected worker before destructive
    /// lifecycle work. No worker is claimed if any selected worker is active.
    pub fn begin_workers_reset(&self, worker_ids: &[String]) -> Result<(), DaemonError> {
        let mut state = self.lock();
        for worker_id in worker_ids {
            let Some(worker) = state.workers.get(worker_id) else {
                continue;
            };
            let reserved = state
                .reservations
                .values()
                .any(|reserved_worker| reserved_worker == worker_id);
            // A quarantined worker's containers are cleanup-uncertain, not
            // live: retiring the VM is the operator's release action. Their
            // receipts are left exactly as uncertain as they already are.
            let quarantined = worker.health == WorkerHealth::Quarantined;
            if reserved || (!quarantined && !worker.active_container_ids.is_empty()) {
                return Err(DaemonError::InvalidState(format!(
                    "worker {worker_id} has active jobs; wait for them before resetting"
                )));
            }
        }
        for worker_id in worker_ids {
            if let Some(worker) = state.workers.get_mut(worker_id) {
                if worker.health == WorkerHealth::Quarantined {
                    worker.active_container_ids.clear();
                }
                worker.health = WorkerHealth::Repairing;
                worker.warm = false;
            }
        }
        Ok(())
    }

    /// Complete one claimed reset. Successful removal forgets only the live
    /// worker registration; receipts and all other daemon state remain.
    pub fn finish_worker_reset(&self, worker_id: &str, succeeded: bool) -> Result<(), DaemonError> {
        let mut state = self.lock();
        let Some(worker) = state.workers.get(worker_id) else {
            return Ok(());
        };
        if worker.health != WorkerHealth::Repairing
            || state
                .reservations
                .values()
                .any(|reserved_worker| reserved_worker == worker_id)
            || !worker.active_container_ids.is_empty()
        {
            return Err(DaemonError::InvalidState(format!(
                "worker {worker_id} changed while resetting"
            )));
        }
        if succeeded {
            state.workers.remove(worker_id);
        } else if let Some(worker) = state.workers.get_mut(worker_id) {
            worker.health = WorkerHealth::Quarantined;
            worker.warm = false;
        }
        Ok(())
    }

    /// Release reset claims that were not acted on after another selected VM
    /// failed removal.
    pub fn cancel_workers_reset(&self, worker_ids: &[String]) {
        let mut state = self.lock();
        for worker_id in worker_ids {
            if let Some(worker) = state.workers.get_mut(worker_id)
                && worker.health == WorkerHealth::Repairing
                && worker.active_container_ids.is_empty()
            {
                worker.health = WorkerHealth::Ready;
                worker.warm = true;
            }
        }
    }

    /// Atomically exclude new reservations while an idle worker VM is repaired.
    ///
    /// An unregistered worker needs no claim: its generation-specific backend
    /// lock owns initial creation. A registered worker may be repaired only
    /// while it has neither admitted reservations nor live containers.
    pub fn begin_worker_repair(&self, worker_id: &str, vm_id: &str) -> Result<bool, DaemonError> {
        let mut state = self.lock();
        let Some(worker) = state.workers.get(worker_id) else {
            return Ok(false);
        };
        let reserved = state
            .reservations
            .values()
            .any(|reserved_worker| reserved_worker == worker_id);
        if worker.vm_id != vm_id
            || worker.health != WorkerHealth::Ready
            || !worker.warm
            || reserved
            || !worker.active_container_ids.is_empty()
        {
            return Err(DaemonError::InvalidState(format!(
                "worker {worker_id} cannot be repaired while unhealthy, admitted, or running"
            )));
        }
        let worker = state.workers.get_mut(worker_id).ok_or_else(|| {
            DaemonError::InvalidState("worker disappeared during repair claim".into())
        })?;
        worker.health = WorkerHealth::Repairing;
        worker.warm = false;
        Ok(true)
    }

    /// Publish the result of a previously claimed worker repair.
    pub fn finish_worker_repair(
        &self,
        worker_id: &str,
        succeeded: bool,
    ) -> Result<(), DaemonError> {
        let mut state = self.lock();
        if state
            .reservations
            .values()
            .any(|reserved_worker| reserved_worker == worker_id)
        {
            return Err(DaemonError::InvalidState(
                "repairing worker acquired a reservation".into(),
            ));
        }
        let worker = state
            .workers
            .get_mut(worker_id)
            .ok_or_else(|| DaemonError::NotFound(worker_id.into()))?;
        if worker.health != WorkerHealth::Repairing || !worker.active_container_ids.is_empty() {
            return Err(DaemonError::InvalidState(
                "worker repair completion has invalid lifecycle state".into(),
            ));
        }
        worker.health = if succeeded {
            WorkerHealth::Ready
        } else {
            WorkerHealth::Quarantined
        };
        worker.warm = succeeded;
        Ok(())
    }

    /// Excludes a worker from admission after cleanup becomes uncertain.
    /// Existing attempts retain their records so callers can finish or report
    /// them, but the worker can no longer be advertised as warm-ready.
    ///
    /// # Errors
    /// Returns an error when the named worker is not registered.
    pub fn quarantine_worker(&self, worker_id: &str) -> Result<(), DaemonError> {
        let mut state = self.lock();
        let worker = state
            .workers
            .get_mut(worker_id)
            .ok_or_else(|| DaemonError::NotFound(worker_id.into()))?;
        worker.health = WorkerHealth::Quarantined;
        worker.warm = false;
        Ok(())
    }

    pub fn begin_job(&self, job: NewJob) -> Result<(String, String), DaemonError> {
        self.begin_job_at(job, Placement::Local)
    }

    pub fn begin_job_at(
        &self,
        job: NewJob,
        placement: Placement,
    ) -> Result<(String, String), DaemonError> {
        self.begin_job_inner(job, placement, None)
            .map(|(job, attempt, _)| (job, attempt))
    }

    /// `begin_job` with the authoritative process admission under the same
    /// lock that inserts the record (`docs/design/processes.md` s3).
    ///
    /// # Errors
    /// Returns `Refused` when admission fails; no receipt is written.
    pub fn begin_job_process(
        &self,
        job: NewJob,
        link: &process::ProcessLink,
        registered: &[String],
    ) -> Result<(String, String, JobLineage), DaemonError> {
        self.begin_job_inner(job, Placement::Local, Some((link, registered)))
    }

    fn begin_job_inner(
        &self,
        job: NewJob,
        placement: Placement,
        admission: Option<(&process::ProcessLink, &[String])>,
    ) -> Result<(String, String, JobLineage), DaemonError> {
        let mut state = self.lock();
        if !state.shells.contains_key(&job.session_id) {
            return Err(DaemonError::NotFound(job.session_id));
        }
        let job_id = Uuid::new_v4().to_string();
        let mut lineage = match admission {
            Some((link, registered)) => {
                match process::admit(
                    &state,
                    &job.session_id,
                    &job.command,
                    Some(&job.kit_ref),
                    link,
                    registered,
                ) {
                    Ok((lineage, _)) => lineage,
                    Err(message) => {
                        process::refused(&mut state, link.parent_job.as_deref());
                        return Err(DaemonError::Refused(message));
                    }
                }
            }
            None => JobLineage {
                parent: format!("session:{}", job.session_id),
                depth: 1,
                ..JobLineage::default()
            },
        };
        if lineage.root.is_empty() {
            lineage.root.clone_from(&job_id);
        }
        // Only a branch job (or a job confined to a fork without a parent)
        // is marked with its split; a child copies its parent's mounts.
        let parented = admission.is_some_and(|(link, _)| link.parent_job.is_some() && !link.branch);
        if let Some((split, label)) = JobLineage::split_of(&job.mounts)
            .or_else(|| state.branch_sessions.get(&job.session_id).cloned())
            .filter(|_| !parented)
        {
            // A job a split branch started (an argv branch, or a Kit command
            // from a shell branch's session) is that branch's child.
            if lineage.parent.starts_with("session:") {
                lineage.parent = format!("split:{split}/{label}");
            }
            lineage.split = Some(split);
            lineage.label = Some(label);
        }
        let attempt_id = Uuid::new_v4().to_string();
        let cursor = state.next_cursor;
        let receipt = JobReceipt {
            schema: "marsh.job/v1".into(),
            cursor,
            job_id: job_id.clone(),
            attempt_id: attempt_id.clone(),
            session_id: job.session_id,
            command: job.command,
            args: Vec::new(),
            placement,
            state: JobState::Queued,
            kit_ref: job.kit_ref,
            workload_image: job.workload_image,
            lineage: Some(lineage.clone()),
            children: Vec::new(),
            mounts: job.mounts,
            worker_id: None,
            vm_id: None,
            container_id: None,
            execution: ExecutionOutcome::NotStarted,
            exit: None,
            output_complete: false,
            cleanup: CleanupState::Pending,
            timing: TimingReport::default(),
            created_unix_ms: unix_time_ms(),
            finished_unix_ms: None,
        };
        state.journal.append(&receipt, false)?;
        state.next_cursor = cursor
            .checked_add(1)
            .ok_or_else(|| DaemonError::InvalidState("receipt cursor exhausted".into()))?;
        state.jobs.insert(job_id.clone(), receipt);
        if state.journal.exceeds_size_limit() {
            let receipts = state.jobs.clone();
            state.journal.compact(&receipts)?;
        }
        drop(state);
        Ok((job_id, attempt_id, lineage))
    }

    /// Record a queued job's arguments for display, bounded (lossy UTF-8).
    /// Journaled with the job's next transition.
    pub fn set_job_args(&self, job_id: &str, arguments: &[Vec<u8>]) {
        let args = arguments
            .iter()
            .take(DISPLAY_ARGS)
            .map(|argument| {
                String::from_utf8_lossy(argument)
                    .chars()
                    .take(DISPLAY_ARG_CHARS)
                    .collect()
            })
            .collect();
        if let Some(receipt) = self.lock().jobs.get_mut(job_id) {
            receipt.args = args;
        }
    }

    /// Record that a root job was started by a split's consumer (a stage
    /// after `join`, which sees `SPLIT_ID`). Journaled with the job's next
    /// transition.
    pub fn set_job_consumes(&self, job_id: &str, split: &str) {
        if !split::valid_id(split) {
            return;
        }
        if let Some(lineage) = self
            .lock()
            .jobs
            .get_mut(job_id)
            .and_then(|receipt| receipt.lineage.as_mut())
            .filter(|lineage| !lineage.parent.starts_with("job:"))
        {
            lineage.consumes = Some(split.into());
        }
    }

    /// Record the `fanout` branch (`<id>/<label>`) that started a root job
    /// or a split branch's job (a fanout run inside a split branch). Split
    /// branches never inherit `FANOUT_BRANCH` (`split::forwardable`).
    pub fn set_job_fanout(&self, job_id: &str, fanout: &str) {
        let valid = fanout.split_once('/').is_some_and(|(id, label)| {
            (8..=32).contains(&id.len())
                && id.bytes().all(|byte| byte.is_ascii_hexdigit())
                && !label.is_empty()
                && label.len() <= 64
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        });
        if !valid {
            return;
        }
        if let Some(lineage) = self
            .lock()
            .jobs
            .get_mut(job_id)
            .and_then(|receipt| receipt.lineage.as_mut())
            .filter(|lineage| !lineage.parent.starts_with("job:"))
        {
            lineage.fanout = Some(fanout.into());
        }
    }

    /// Bind one queued receipt to the exact ready worker and runtime-issued
    /// container. Duplicate/fabricated identities and capacity overflow fail.
    /// Mark the submission boundary before a worker can accept Start. Queued
    /// journal replay is already Unknown; this also makes live inspection honest
    /// during the interval before Started or a conclusive rejection arrives.
    pub fn mark_execution_submitted(&self, job_id: &str) -> Result<(), DaemonError> {
        let mut state = self.lock();
        let receipt = state
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| DaemonError::NotFound(job_id.into()))?;
        if receipt.state != JobState::Queued {
            return Err(DaemonError::InvalidState(
                "job is not awaiting submission".into(),
            ));
        }
        receipt.execution = ExecutionOutcome::Unknown;
        Ok(())
    }

    pub fn mark_running(
        &self,
        job_id: &str,
        worker_id: &str,
        vm_id: &str,
        container_id: &str,
    ) -> Result<(), DaemonError> {
        if container_id.len() != 64
            || !container_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(DaemonError::InvalidState(
                "runtime returned an invalid container identity".into(),
            ));
        }
        let mut state = self.lock();
        if state
            .jobs
            .values()
            .any(|job| job.container_id.as_deref() == Some(container_id))
        {
            return Err(DaemonError::InvalidState(
                "runtime container identity is already bound".into(),
            ));
        }
        let reserved_worker = state.reservations.get(job_id).ok_or_else(|| {
            DaemonError::InvalidState("job did not reserve worker capacity".into())
        })?;
        if reserved_worker != worker_id {
            return Err(DaemonError::InvalidState(
                "job reserved a different worker".into(),
            ));
        }
        let worker = state
            .workers
            .get(worker_id)
            .ok_or_else(|| DaemonError::NotFound(worker_id.into()))?;
        if worker.vm_id != vm_id || worker.health != WorkerHealth::Ready || !worker.warm {
            return Err(DaemonError::InvalidState(
                "worker is incompatible, unhealthy, or at capacity".into(),
            ));
        }
        let receipt = state
            .jobs
            .get(job_id)
            .ok_or_else(|| DaemonError::NotFound(job_id.into()))?;
        if receipt.state != JobState::Queued {
            return Err(DaemonError::InvalidState(
                "only a queued job may start".into(),
            ));
        }
        let mut updated = receipt.clone();
        updated.worker_id = Some(worker_id.into());
        updated.vm_id = Some(vm_id.into());
        updated.container_id = Some(container_id.into());
        updated.state = JobState::Running;
        updated.execution = ExecutionOutcome::Unknown;
        state.journal.append(&updated, false)?;
        state.jobs.insert(job_id.into(), updated);
        if state.journal.exceeds_size_limit() {
            let receipts = state.jobs.clone();
            state.journal.compact(&receipts)?;
        }
        state.reservations.remove(job_id);
        let worker = state.workers.get_mut(worker_id).ok_or_else(|| {
            DaemonError::InvalidState("worker disappeared during transition".into())
        })?;
        worker.active_container_ids.push(container_id.into());
        worker.active_container_ids.sort();
        Ok(())
    }

    /// Atomically claim one worker slot before grants or containers exist.
    pub fn reserve_worker(
        &self,
        job_id: &str,
        worker_id: &str,
        vm_id: &str,
    ) -> Result<(), DaemonError> {
        let mut state = self.lock();
        let receipt = state
            .jobs
            .get(job_id)
            .ok_or_else(|| DaemonError::NotFound(job_id.into()))?;
        if receipt.state != JobState::Queued || state.reservations.contains_key(job_id) {
            return Err(DaemonError::InvalidState(
                "job cannot reserve capacity".into(),
            ));
        }
        let worker = state
            .workers
            .get(worker_id)
            .ok_or_else(|| DaemonError::NotFound(worker_id.into()))?;
        let reserved = state
            .reservations
            .values()
            .filter(|id| id.as_str() == worker_id)
            .count();
        if worker.vm_id != vm_id
            || worker.health != WorkerHealth::Ready
            || !worker.warm
            || worker.active_container_ids.len() + reserved
                >= usize::from(worker.container_capacity)
        {
            return Err(DaemonError::InvalidState(
                "worker is incompatible, unhealthy, or at capacity".into(),
            ));
        }
        state.reservations.insert(job_id.into(), worker_id.into());
        let receipt = state.jobs.get_mut(job_id).ok_or_else(|| {
            DaemonError::InvalidState("job disappeared during reservation".into())
        })?;
        receipt.worker_id = Some(worker_id.into());
        receipt.vm_id = Some(vm_id.into());
        Ok(())
    }

    /// Publish a terminal failed receipt and release any pre-create reservation.
    pub fn abort_job(&self, job_id: &str, cause: impl Into<String>) -> Result<(), DaemonError> {
        self.finish_job_as(
            job_id,
            ExitStatus {
                code: None,
                cause: cause.into(),
            },
            false,
            CleanupState::Uncertain,
            TimingReport::default(),
            Some(JobState::Failed),
            None,
        )
    }

    /// Cancel a reservation before any grants or runtime resources exist.
    pub fn cancel_job(&self, job_id: &str, cause: impl Into<String>) -> Result<(), DaemonError> {
        self.finish_job_as(
            job_id,
            ExitStatus {
                code: None,
                cause: cause.into(),
            },
            false,
            CleanupState::NotRequired,
            TimingReport::default(),
            Some(JobState::Cancelled),
            Some(ExecutionOutcome::NotStarted),
        )
    }

    /// Publish terminal state only after the executor supplies capture and
    /// cleanup outcomes. Uncertain cleanup atomically quarantines the worker.
    pub fn finish_job(
        &self,
        job_id: &str,
        exit: ExitStatus,
        output_complete: bool,
        cleanup: CleanupState,
        timing: TimingReport,
    ) -> Result<(), DaemonError> {
        self.finish_job_as(job_id, exit, output_complete, cleanup, timing, None, None)
    }

    /// Persist an authoritative observation without decoding a public status or cause.
    #[allow(clippy::too_many_arguments)] // Same atomic terminal transition plus its typed observation.
    pub fn finish_job_with_execution(
        &self,
        job_id: &str,
        execution: ExecutionOutcome,
        exit: ExitStatus,
        output_complete: bool,
        cleanup: CleanupState,
        timing: TimingReport,
    ) -> Result<(), DaemonError> {
        self.finish_job_as(
            job_id,
            exit,
            output_complete,
            cleanup,
            timing,
            None,
            Some(execution),
        )
    }

    #[allow(clippy::too_many_arguments)] // One atomic journal transition with its typed observation.
    #[allow(clippy::too_many_lines)] // One locked journal transition; splitting it obscures atomicity.
    fn finish_job_as(
        &self,
        job_id: &str,
        mut exit: ExitStatus,
        output_complete: bool,
        cleanup: CleanupState,
        timing: TimingReport,
        final_state: Option<JobState>,
        execution: Option<ExecutionOutcome>,
    ) -> Result<(), DaemonError> {
        if cleanup == CleanupState::Pending {
            return Err(DaemonError::InvalidState(
                "terminal job cannot have pending cleanup".into(),
            ));
        }
        if !output_complete
            || execution.as_ref().is_some_and(|observed| {
                matches!(
                    observed,
                    ExecutionOutcome::Unknown
                        | ExecutionOutcome::NotStarted
                        | ExecutionOutcome::SupervisionFailed
                )
            })
        {
            exit.code = Some(CLEANUP_UNCERTAIN_EXIT_CODE);
        }
        if cleanup == CleanupState::Uncertain {
            exit.code = Some(CLEANUP_UNCERTAIN_EXIT_CODE);
            if exit.cause != "cleanup_uncertain" {
                exit.cause.push_str("; cleanup_uncertain");
            }
        }
        let mut state = self.lock();
        let receipt = state
            .jobs
            .get(job_id)
            .ok_or_else(|| DaemonError::NotFound(job_id.into()))?;
        if !matches!(receipt.state, JobState::Queued | JobState::Running) {
            return Err(DaemonError::InvalidState("job is already terminal".into()));
        }
        let worker_id = receipt.worker_id.clone();
        let container_id = receipt.container_id.clone();
        if worker_id
            .as_ref()
            .is_some_and(|worker_id| !state.workers.contains_key(worker_id))
        {
            return Err(DaemonError::InvalidState(
                "assigned worker is missing".into(),
            ));
        }
        let mut updated = receipt.clone();
        let cancelled = state.process.cancelling.remove(job_id)
            // Ctrl-C typed into its terminal, then ended by SIGINT.
            | (state.process.interrupted.remove(job_id) && exit.code == Some(130));
        updated.state = final_state.unwrap_or_else(|| {
            if cleanup == CleanupState::Uncertain {
                JobState::Unknown
            } else if cancelled {
                // Its tree was cancelled (s8): never `finished` or `failed`.
                JobState::Cancelled
            } else if !output_complete {
                JobState::Failed
            } else {
                JobState::Finished
            }
        });
        match execution {
            Some(ExecutionOutcome::Unknown) | None => {
                if matches!(
                    updated.execution,
                    ExecutionOutcome::NotStarted | ExecutionOutcome::Unknown
                ) {
                    updated.execution = ExecutionOutcome::Unknown;
                }
            }
            Some(observed) => updated.execution = observed,
        }
        updated.exit = Some(exit);
        updated.output_complete = output_complete;
        updated.cleanup = cleanup;
        updated.timing = timing;
        updated.finished_unix_ms = Some(unix_time_ms());
        state.journal.append(&updated, true)?;

        state.reservations.remove(job_id);
        if let Some(worker_id) = worker_id {
            let worker = state
                .workers
                .get_mut(&worker_id)
                .expect("worker existence was validated while holding the state lock");
            if let Some(container_id) = &container_id {
                worker
                    .active_container_ids
                    .retain(|active| active != container_id);
            }
            if cleanup == CleanupState::Uncertain {
                worker.health = WorkerHealth::Quarantined;
                worker.warm = false;
            }
        }
        state.jobs.insert(job_id.into(), updated.clone());
        let trimmed = receipt_journal::trim_receipts(&mut state.jobs);
        if trimmed || state.journal.exceeds_size_limit() {
            let receipts = state.jobs.clone();
            state.journal.compact(&receipts)?;
        }
        drop(state);
        Ok(())
    }

    #[must_use]
    pub fn status(&self, current_session_id: Option<String>) -> StatusDocument {
        let state = self.lock();
        StatusDocument {
            schema: "marsh.status/v1".into(),
            features: FEATURES.iter().map(ToString::to_string).collect(),
            scope_id: state.scope_id.clone(),
            daemon_id: state.daemon_id.clone(),
            endpoint_owner: state.owner.clone(),
            control_home: state.control_home.clone(),
            job_defaults: state.job_defaults.clone(),
            current_session_id,
            // An ended shell (detached, its authority released) is history, not
            // state: past `marsh -c` sessions do not accumulate here.
            shells: state
                .shells
                .values()
                .filter(|shell| {
                    shell.state != ShellState::Detached
                        || state.session_authorities.contains_key(&shell.session_id)
                })
                .cloned()
                .collect(),
            workers: state.workers.values().cloned().collect(),
            splits: split::SplitCounts::default(),
            processes: process::ProcessCounts::default(),
        }
    }

    /// Make VM preparation visible before a Kit job receives its runtime identity.
    pub fn begin_kit_start(
        &self,
        session_id: &str,
        command: &str,
        placement: Placement,
    ) -> Result<String, DaemonError> {
        let mut state = self.lock();
        if !state
            .shells
            .get(session_id)
            .is_some_and(|shell| shell.state == ShellState::Attached)
        {
            return Err(DaemonError::NotFound(session_id.into()));
        }
        let id = Uuid::new_v4().to_string();
        state.starting_kits.insert(
            id.clone(),
            ProcessStartingKit {
                invocation_id: id.clone(),
                shell_session_id: session_id.into(),
                command: command.into(),
                placement,
                started_unix_ms: unix_time_ms(),
            },
        );
        Ok(id)
    }

    pub fn finish_kit_start(&self, invocation_id: &str) {
        self.lock().starting_kits.remove(invocation_id);
    }

    #[must_use]
    pub fn process_view(
        &self,
        caller: &SessionSpec,
        mut acp_sessions: Vec<AcpSessionSummary>,
    ) -> ProcessViewDocument {
        const LIMIT: usize = 256;
        let state = self.lock();
        let scope = session_project_scope(&state, &caller.session_id);
        let mut shells = Vec::new();
        let mut starting_kits = Vec::new();
        let mut jobs = Vec::new();
        let mut truncated = false;
        for shell in state.shells.values() {
            if shell.state == ShellState::Attached
                && scope.is_some()
                && session_project_scope(&state, &shell.session_id) == scope
                && state.session_home_backings.get(&shell.session_id) == Some(&caller.home_backing)
            {
                if shells.len() == LIMIT {
                    truncated = true;
                    break;
                }
                shells.push(ProcessShell {
                    session_id: shell.session_id.clone(),
                    host_attachment_pid: shell.pid,
                    state: shell.state,
                });
            }
        }
        for job in state.jobs.values() {
            if (matches!(
                job.state,
                JobState::Queued | JobState::Running | JobState::Unknown
            ) || job.cleanup == CleanupState::Pending)
                && scope.is_some()
                && session_project_scope(&state, &job.session_id) == scope
                && state.session_home_backings.get(&job.session_id) == Some(&caller.home_backing)
            {
                if jobs.len() == LIMIT {
                    truncated = true;
                    break;
                }
                jobs.push(ProcessJob {
                    job_id: job.job_id.clone(),
                    shell_session_id: job.session_id.clone(),
                    command: job.command.clone(),
                    placement: job.placement,
                    state: job.state,
                    cleanup: job.cleanup,
                    created_unix_ms: job.created_unix_ms,
                    worker_id: job.worker_id.clone(),
                    worker_health: job
                        .worker_id
                        .as_ref()
                        .and_then(|id| state.workers.get(id).map(|worker| worker.health)),
                    vm_id: job.vm_id.clone(),
                    container_id: job.container_id.clone(),
                });
            }
        }
        for start in state.starting_kits.values() {
            if scope.is_some()
                && session_project_scope(&state, &start.shell_session_id) == scope
                && state.session_home_backings.get(&start.shell_session_id)
                    == Some(&caller.home_backing)
            {
                if starting_kits.len() == LIMIT {
                    truncated = true;
                    break;
                }
                starting_kits.push(start.clone());
            }
        }
        acp_sessions.retain(|session| !session.terminal);
        if acp_sessions.len() > LIMIT {
            acp_sessions.truncate(LIMIT);
            truncated = true;
        }
        ProcessViewDocument {
            schema: "marsh.process_view/v1".into(),
            observed_unix_ms: unix_time_ms(),
            daemon_id: state.daemon_id.clone(),
            truncated,
            shells,
            starting_kits,
            jobs,
            acp_sessions,
        }
    }

    /// Reports whether the currently registered worker generation forbids
    /// further effects. Historical receipts remain auditable but cannot block
    /// a physically recreated VM after the daemon's deferred restart reset.
    #[must_use]
    pub fn worker_reuse_blocked(&self, vm_id: &str) -> bool {
        let state = self.lock();
        state
            .workers
            .values()
            .any(|worker| worker.vm_id == vm_id && worker.health == WorkerHealth::Quarantined)
    }

    #[must_use]
    pub fn jobs(&self) -> JobsDocument {
        let state = self.lock();
        let mut jobs: Vec<_> = state
            .jobs
            .values()
            .map(|job| JobSummary {
                cursor: job.cursor,
                job_id: job.job_id.clone(),
                command: job.command.clone(),
                placement: job.placement,
                cleanup: job.cleanup,
                state: job.state,
                exit_code: job.exit.as_ref().and_then(|exit| exit.code),
                wall_ms: job.timing.wall_ms,
                parent: job.lineage.as_ref().map(|lineage| lineage.parent.clone()),
            })
            .collect();
        jobs.sort_by_key(|job| std::cmp::Reverse(job.cursor));
        JobsDocument {
            schema: "marsh.jobs/v1".into(),
            jobs,
        }
    }

    pub fn job(&self, selector: &str) -> Result<JobReceipt, DaemonError> {
        if selector.is_empty() {
            return Err(DaemonError::InvalidState(
                "job selector cannot be empty".into(),
            ));
        }
        let state = self.lock();
        if let Ok(cursor) = selector.parse::<u64>()
            && let Some(receipt) = state
                .jobs
                .values()
                .find(|receipt| receipt.cursor == cursor)
                .cloned()
        {
            return Ok(receipt);
        }
        if let Some(receipt) = state.jobs.get(selector) {
            return Ok(receipt.clone());
        }
        let mut matches = state
            .jobs
            .values()
            .filter(|receipt| receipt.job_id.starts_with(selector));
        let result = matches
            .next()
            .cloned()
            .ok_or_else(|| DaemonError::NotFound(selector.into()))?;
        if matches.next().is_some() {
            return Err(DaemonError::InvalidState(format!(
                "job selector is ambiguous: {selector}"
            )));
        }
        Ok(result)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn project_identity(path: &Path) -> Option<(u64, u64)> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_dir() || fs::canonicalize(path).ok()?.as_path() != path {
        return None;
    }
    Some((metadata.dev(), metadata.ino()))
}

fn project_scope(path: &Path, (device, inode): (u64, u64)) -> String {
    let mut hash = Sha256::new();
    let path = path.as_os_str().as_encoded_bytes();
    hash.update((path.len() as u64).to_le_bytes());
    hash.update(path);
    hash.update(device.to_le_bytes());
    hash.update(inode.to_le_bytes());
    format!("{:x}", hash.finalize())
}

fn session_project_scope(state: &State, session_id: &str) -> Option<String> {
    let path = &state.shells.get(session_id)?.project;
    let identity = state
        .session_project_identities
        .get(session_id)
        .copied()
        .flatten()?;
    Some(project_scope(path, identity))
}

/// Blocking, one-request-per-connection public daemon server.
pub struct Server {
    lifecycle: Lifecycle,
    store: DaemonStore,
    backend: Arc<dyn DaemonBackend>,
    build_identity: String,
    shutdown_requested: Arc<AtomicBool>,
    scope_lifecycle_timeout: Duration,
    acp: Arc<AcpSessionManager>,
    mcp_host: Option<McpHostControl>,
    protected_guest_roots: Vec<PathBuf>,
    dev_broker: Option<Arc<dev_broker::DevBroker>>,
    splits: Arc<split::SplitEngine>,
}

impl Server {
    pub fn bind(home: &Path) -> Result<Self, DaemonError> {
        let executable = std::env::current_exe()?;
        Self::bind_with_identity(home, artifact_identity(&executable)?)
    }

    /// Bind a production daemon whose advertised identity covers both the
    /// daemon artifact and the exact configured stock-SBX executable.
    #[cfg(test)]
    pub fn bind_with_stock_sbx(home: &Path, stock_sbx: &Path) -> Result<Self, DaemonError> {
        Self::bind_stock_sbx(home, stock_sbx, home)
    }

    /// Production daemon: keep durable receipts outside the guest-writable
    /// selected home. The selected home still determines the scope identity.
    pub fn bind_with_stock_sbx_and_control_home(
        home: &Path,
        stock_sbx: &Path,
        control_home: &Path,
    ) -> Result<Self, DaemonError> {
        receipt_journal::verify_control_home(home, control_home)?;
        Self::bind_stock_sbx(home, stock_sbx, control_home)
    }

    fn bind_stock_sbx(
        home: &Path,
        stock_sbx: &Path,
        journal_home: &Path,
    ) -> Result<Self, DaemonError> {
        let executable = std::env::current_exe()?;
        let mut server = Self::bind_with_identity_and_journal_home(
            home,
            journal_home,
            runtime_identity(&executable, stock_sbx)?,
        )?;
        server.mcp_host = Some(
            McpHostControl::new(
                executable.with_file_name("marsh"),
                stock_sbx.to_path_buf(),
                std::env::var_os("HOME")
                    .ok_or_else(|| DaemonError::InvalidState("host HOME is unavailable".into()))
                    .and_then(|home| fs::canonicalize(home).map_err(DaemonError::from))?,
                Some(home.to_path_buf()),
            )
            .with_defaults_home(journal_home.to_path_buf()),
        );
        Ok(server)
    }

    fn bind_with_identity(home: &Path, build_identity: String) -> Result<Self, DaemonError> {
        Self::bind_with_identity_and_journal_home(home, home, build_identity)
    }

    fn bind_with_identity_and_journal_home(
        home: &Path,
        journal_home: &Path,
        build_identity: String,
    ) -> Result<Self, DaemonError> {
        let lifecycle = Lifecycle::bind(home)?;
        raise_descriptor_limit();
        let token = read_secret(&lifecycle.paths.token)?;
        let splits = split::SplitEngine::open(
            journal_home,
            Client {
                paths: lifecycle.paths.clone(),
                token: token.clone(),
            },
        );
        let store = DaemonStore::with_master_token(home, journal_home, Some(token))?;
        Ok(Self {
            lifecycle,
            store,
            backend: Arc::new(UnavailableBackend),
            build_identity,
            shutdown_requested: Arc::new(AtomicBool::new(false)),
            scope_lifecycle_timeout: SCOPE_LIFECYCLE_EXECUTION_TIMEOUT,
            acp: Arc::new(AcpSessionManager::new()?),
            mcp_host: None,
            protected_guest_roots: Vec::new(),
            dev_broker: None,
            splits,
        })
    }

    #[must_use]
    pub fn with_backend(mut self, backend: Arc<dyn DaemonBackend>) -> Self {
        self.backend = backend;
        self
    }

    /// Advertise the same daemon-launch resource snapshot used by its backend.
    #[must_use]
    pub fn with_job_defaults(self, defaults: JobDefaults) -> Self {
        self.store.lock().job_defaults = Some(defaults);
        self
    }

    /// Serve `DevSbx` relay requests for development grants.
    #[must_use]
    pub fn with_dev_broker(mut self, broker: Arc<dev_broker::DevBroker>) -> Self {
        self.dev_broker = Some(broker);
        self
    }

    /// Host-owned paths that must never become writable guest mounts.
    #[must_use]
    pub fn with_protected_guest_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.protected_guest_roots.extend(roots);
        self
    }

    #[cfg(test)]
    fn with_scope_lifecycle_timeout(mut self, timeout: Duration) -> Self {
        self.scope_lifecycle_timeout = timeout;
        self
    }

    #[must_use]
    pub fn store(&self) -> DaemonStore {
        self.store.clone()
    }

    pub fn serve_one(&self) -> Result<(), DaemonError> {
        let (stream, _) = self.lifecycle.listener.accept()?;
        self.handler().handle(stream)
    }

    pub fn serve_until<F>(&self, stop: F) -> Result<(), DaemonError>
    where
        F: Fn() -> bool,
    {
        self.lifecycle.listener.set_nonblocking(true)?;
        let active_connections = Arc::new(AtomicUsize::new(0));
        while !stop() && !self.shutdown_requested.load(Ordering::Acquire) {
            match self.lifecycle.listener.accept() {
                Ok((stream, _)) => {
                    let Some(permit) = acquire_daemon_connection(&active_connections) else {
                        drop(stream);
                        continue;
                    };
                    let handler = self.handler();
                    if let Err(error) = thread::Builder::new().spawn(move || {
                        let _permit = permit;
                        let _ = handler.handle(stream);
                    }) {
                        eprintln!("marshd: cannot spawn connection handler: {error}");
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    fn handler(&self) -> ConnectionHandler {
        ConnectionHandler {
            store: self.store.clone(),
            runtime_directory: self.lifecycle.paths.runtime_directory.clone(),
            backend: Arc::clone(&self.backend),
            build_identity: self.build_identity.clone(),
            shutdown_requested: Arc::clone(&self.shutdown_requested),
            scope_lifecycle_timeout: self.scope_lifecycle_timeout,
            acp: Arc::clone(&self.acp),
            mcp_host: self.mcp_host.clone(),
            protected_guest_roots: self.protected_guest_roots.clone(),
            dev_broker: self.dev_broker.clone(),
            splits: Arc::clone(&self.splits),
        }
    }
}

struct ConnectionHandler {
    store: DaemonStore,
    runtime_directory: PathBuf,
    backend: Arc<dyn DaemonBackend>,
    build_identity: String,
    shutdown_requested: Arc<AtomicBool>,
    scope_lifecycle_timeout: Duration,
    acp: Arc<AcpSessionManager>,
    mcp_host: Option<McpHostControl>,
    protected_guest_roots: Vec<PathBuf>,
    dev_broker: Option<Arc<dev_broker::DevBroker>>,
    splits: Arc<split::SplitEngine>,
}

/// Reject a guest mount containing, or contained by, a host-owned path.
/// Canonicalization fails closed so symlink changes cannot bypass the check.
/// A protected root that does not exist yet is resolved through its nearest
/// existing ancestor plus the missing tail, so a mount that would contain it
/// once it is created is still refused.
pub fn reject_guest_mount_overlap(mounts: &[&Path], roots: &[PathBuf]) -> Result<(), String> {
    let roots = roots
        .iter()
        .map(|root| resolve_protected_root(root))
        .collect::<Result<Vec<_>, _>>()?;
    for mount in mounts {
        let mount = mount.canonicalize().map_err(|_| {
            format!(
                "cannot mount {} into the VM: the directory is unavailable",
                mount.display()
            )
        })?;
        if let Some(root) = roots
            .iter()
            .find(|root| root.starts_with(&mount) || mount.starts_with(root))
        {
            return Err(if root.starts_with(&mount) {
                format!(
                    "cannot mount {} into the VM: it contains marsh's private host state at {}; start marsh from a project directory instead (for example `mkdir -p ~/scratch && cd ~/scratch`)",
                    mount.display(),
                    root.display()
                )
            } else {
                format!(
                    "cannot mount {} into the VM: it is inside marsh's private host state at {}; start marsh from a project directory outside it",
                    mount.display(),
                    root.display()
                )
            });
        }
    }
    Ok(())
}

/// Canonical location of a protected root. When the root (or a trailing part
/// of it) is absent, canonicalize the nearest existing ancestor and append the
/// missing plain components. Any other failure, or a missing tail containing
/// `..`/`.`, fails closed.
fn resolve_protected_root(root: &Path) -> Result<PathBuf, String> {
    const UNAVAILABLE: &str = "protected host path is unavailable";
    if !root.is_absolute() {
        return Err(UNAVAILABLE.to_owned());
    }
    let mut missing = Vec::new();
    let mut cursor = root;
    loop {
        match cursor.canonicalize() {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A dangling symlink is present but unresolvable: fail closed.
                if cursor.symlink_metadata().is_ok() {
                    return Err(UNAVAILABLE.to_owned());
                }
                match cursor.components().next_back() {
                    Some(std::path::Component::Normal(name)) => missing.push(name.to_owned()),
                    _ => return Err(UNAVAILABLE.to_owned()),
                }
                cursor = cursor.parent().ok_or_else(|| UNAVAILABLE.to_owned())?;
            }
            Err(_) => return Err(UNAVAILABLE.to_owned()),
        }
    }
}

#[cfg(test)]
mod mount_guard_tests {
    use super::reject_guest_mount_overlap;
    use std::{fs, os::unix::fs::symlink};

    #[test]
    fn rejects_ancestor_sibling_and_symlink_mounts() {
        let temp = tempfile::tempdir().unwrap();
        let selected = temp.path().join("selected");
        let project = temp.path().join("project");
        let runtime = temp.path().join("runtime");
        let control = temp.path().join("control");
        for path in [&selected, &project, &runtime, &control] {
            fs::create_dir(path).unwrap();
        }
        let sibling_token = runtime.join("another-home");
        fs::create_dir(&sibling_token).unwrap();
        assert!(
            reject_guest_mount_overlap(&[&project, &selected], &[runtime.clone(), control.clone()])
                .is_ok()
        );
        assert!(reject_guest_mount_overlap(&[&runtime], std::slice::from_ref(&runtime)).is_err());
        let error = reject_guest_mount_overlap(&[temp.path()], &[sibling_token]).unwrap_err();
        let launch = temp.path().canonicalize().unwrap();
        assert!(
            error.starts_with(&format!("cannot mount {} into the VM", launch.display()))
                && error.contains("start marsh from a project directory"),
            "{error}"
        );
        let alias = temp.path().join("alias");
        symlink(&control, &alias).unwrap();
        assert!(reject_guest_mount_overlap(&[&alias], &[control]).is_err());
        assert!(reject_guest_mount_overlap(&[&project], &[temp.path().join("missing")]).is_ok());
    }

    #[test]
    fn absent_protected_root_guards_its_future_location() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let home = temp.path().join("home");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&home).unwrap();
        // Like ~/.local/state/marsh in a dev VM whose daemon uses
        // MARSH_CONTROL_HOME: the product root was never created.
        let absent = home.join(".local/state/marsh");
        assert!(reject_guest_mount_overlap(&[&project], std::slice::from_ref(&absent)).is_ok());
        // A mount that would contain the root once created is still refused.
        let error =
            reject_guest_mount_overlap(&[&home], std::slice::from_ref(&absent)).unwrap_err();
        assert!(error.contains("private host state"), "{error}");
        // The ancestor is resolved through symlinks.
        let alias = temp.path().join("home-alias");
        symlink(&home, &alias).unwrap();
        assert!(reject_guest_mount_overlap(&[&home], &[alias.join(".local/state/marsh")]).is_err());
        // Once it exists, overlap is checked as before.
        fs::create_dir_all(&absent).unwrap();
        assert!(reject_guest_mount_overlap(&[&absent], std::slice::from_ref(&absent)).is_err());
        assert!(reject_guest_mount_overlap(&[&project], &[absent]).is_ok());
        // A dangling symlink root and a `..` tail fail closed.
        let dangling = temp.path().join("dangling");
        symlink(temp.path().join("nowhere"), &dangling).unwrap();
        assert!(reject_guest_mount_overlap(&[&project], &[dangling]).is_err());
        assert!(reject_guest_mount_overlap(&[&project], &[home.join("gone/..")]).is_err());
    }
}

impl ConnectionHandler {
    fn check_attachment_mounts(&self, session: &SessionAuthority) -> Result<(), String> {
        let mut roots = self.protected_guest_roots.clone();
        roots.push(
            self.runtime_directory
                .parent()
                .ok_or("daemon runtime root is unavailable")?
                .to_path_buf(),
        );
        reject_guest_mount_overlap(&[&session.launch_directory, &session.home_backing], &roots)
    }

    fn reject_unauthorized(stream: &mut UnixStream, message: &str) -> Result<(), DaemonError> {
        write_frame(
            stream,
            &PublicReply::Error {
                code: ErrorCode::Unauthorized,
                message: message.into(),
            },
        )
    }

    fn handle_shutdown(&self, stream: &mut UnixStream) -> Result<(), DaemonError> {
        if !self.store.safe_to_shutdown() {
            return write_frame(
                stream,
                &PublicReply::Error {
                    code: ErrorCode::InvalidRequest,
                    message: BUSY_LIFECYCLE_MESSAGE.into(),
                },
            );
        }
        write_frame(stream, &PublicReply::ShuttingDown)?;
        self.shutdown_requested.store(true, Ordering::Release);
        Ok(())
    }

    fn handle_scope_lifecycle(
        &self,
        stream: &mut UnixStream,
        action: ScopeLifecycleAction,
    ) -> Result<(), DaemonError> {
        if action == ScopeLifecycleAction::Stop {
            self.acp.revoke_publications_for_scope_stop();
        }
        let deadline = Instant::now() + self.scope_lifecycle_timeout;
        let report = self
            .backend
            .teardown_scope(action, self.store.clone(), deadline);
        write_frame(stream, &PublicReply::ScopeLifecycle(report.clone()))?;
        if action == ScopeLifecycleAction::Stop && report.cleanup_complete {
            self.shutdown_requested.store(true, Ordering::Release);
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)] // One linear dispatch over the public request variants.
    fn handle_authorized(
        &self,
        mut stream: UnixStream,
        request: PublicRequest,
    ) -> Result<(), DaemonError> {
        match request {
            PublicRequest::Shutdown => self.handle_shutdown(&mut stream),
            PublicRequest::AttachEphemeralShell {
                pid,
                session,
                expected_project_identity,
                token,
            } => {
                let result = (|| {
                    if !session.ephemeral_home {
                        return Err(DaemonError::InvalidState(
                            "private ephemeral attachment requires an ephemeral session".into(),
                        ));
                    }
                    self.check_attachment_mounts(&session)
                        .map_err(DaemonError::InvalidState)?;
                    self.backend.validate_ephemeral_home(&session, &token)?;
                    let session_id = if let Some(identity) = expected_project_identity {
                        self.store.attach_pinned_shell(pid, session, identity)?
                    } else {
                        self.store.attach_shell(pid, session)
                    };
                    self.store
                        .lock()
                        .ephemeral_home_tokens
                        .insert(session_id.clone(), token);
                    Ok(PublicReply::ShellAttached { session_id })
                })();
                write_frame(
                    &mut stream,
                    &result.unwrap_or_else(|error| public_error(&error)),
                )
            }
            PublicRequest::AttachShell { pid, session } => {
                let reply = match self.check_attachment_mounts(&session) {
                    Ok(()) => PublicReply::ShellAttached {
                        session_id: self.store.attach_shell(pid, session),
                    },
                    Err(message) => PublicReply::Error {
                        code: ErrorCode::InvalidRequest,
                        message,
                    },
                };
                write_frame(&mut stream, &reply)
            }
            PublicRequest::AttachPinnedShell {
                pid,
                session,
                expected_project_identity,
            } => {
                let reply = self
                    .check_attachment_mounts(&session)
                    .map_err(DaemonError::InvalidState)
                    .and_then(|()| {
                        self.store
                            .attach_pinned_shell(pid, session, expected_project_identity)
                    })
                    .map_or_else(
                        |error| public_error(&error),
                        |session_id| PublicReply::ShellAttached { session_id },
                    );
                write_frame(&mut stream, &reply)
            }
            PublicRequest::McpPublish {
                session,
                name,
                description,
                sandbox,
                kit,
                pipeline,
            } => {
                let reply = (|| {
                    validate_mcp_publication_name(&name).map_err(PublicationOutcome::rejected)?;
                    validate_publication_options(
                        description.as_deref(),
                        kit.as_deref(),
                        sandbox.as_deref(),
                        false,
                    )
                    .map_err(PublicationOutcome::rejected)?;
                    validate_publication_pipeline(&pipeline)
                        .map_err(PublicationOutcome::rejected)?;
                    if session.ephemeral_home {
                        return Err(PublicationOutcome::rejected(
                            "MCP publication requires a persistent home",
                        ));
                    }
                    if let Some(kit) = &kit
                        && !self
                            .backend
                            .registered_kits()
                            .map_err(|error| PublicationOutcome::rejected(error.to_string()))?
                            .contains_key(kit)
                    {
                        return Err(PublicationOutcome::rejected(
                            "unknown Kit command; use a registered command or --sandbox SANDBOX",
                        ));
                    }
                    let identity = self
                        .store
                        .verify_publication_project(&session)
                        .map_err(|error| PublicationOutcome::rejected(error.to_string()))?;
                    let scope = project_scope(&session.launch_directory, identity);
                    let host = self.mcp_host.as_ref().ok_or_else(|| {
                        PublicationOutcome::rejected("host MCP publication is unavailable")
                    })?;
                    let publication_lock = host.publication_lock(&scope, &name);
                    let _publication = wait_mcp_publication_lock(&publication_lock)
                        .map_err(PublicationOutcome::rejected)?;
                    let admission = host
                        .admit_scope(&session)
                        .map_err(PublicationOutcome::rejected)?;
                    host.preflight_sandbox(&session, sandbox.as_deref(), &admission)?;
                    // The private BeginCommit handshake reaches this callback
                    // only after host validation under lock.
                    let mut before_commit = || {
                        self.store
                            .verify_publication_project(&session)
                            .map_err(|error| error.to_string())?;
                        Ok(())
                    };
                    let mut prepare = || {
                        mcp_publication::prepare_publication_kit(
                            self,
                            &session,
                            kit.as_deref().ok_or("no Kit target")?,
                            stream.try_clone().map_err(|error| error.to_string())?,
                        )
                    };
                    let committed = host.publish(
                        &session,
                        identity,
                        &name,
                        description.as_deref(),
                        sandbox.as_deref(),
                        &pipeline,
                        kit.as_deref(),
                        &mut prepare,
                        &mut before_commit,
                        &admission,
                    )?;
                    // Untargeted publish = default for Kit VMs created from
                    // now on; a targeted (re)publish is not a default.
                    let recorded = if kit.is_none() && sandbox.is_none() {
                        host.record_default(&session, &name)
                    } else {
                        host.drop_default(&session, &name)
                    };
                    if let Err(error) = recorded {
                        return Err(PublicationOutcome::uncertain(format!(
                            "{}\npublished, but the Kit default record was not updated: {error}; republish, or use `mcp load {name} --kit KIT`",
                            committed.message
                        )));
                    }
                    Ok(committed)
                })();
                write_frame(&mut stream, &mcp_publication_reply(reply))
            }
            PublicRequest::McpLoad {
                session,
                name,
                kit,
                sandbox,
            } => {
                let reply = (|| {
                    validate_mcp_publication_name(&name).map_err(PublicationOutcome::rejected)?;
                    validate_publication_options(None, kit.as_deref(), sandbox.as_deref(), true)
                        .map_err(PublicationOutcome::rejected)?;
                    if session.ephemeral_home {
                        return Err(PublicationOutcome::rejected(
                            "MCP load requires a persistent home",
                        ));
                    }
                    let identity = self
                        .store
                        .verify_publication_project(&session)
                        .map_err(|error| PublicationOutcome::rejected(error.to_string()))?;
                    let scope = project_scope(&session.launch_directory, identity);
                    let host = self.mcp_host.as_ref().ok_or_else(|| {
                        PublicationOutcome::rejected("host MCP publication is unavailable")
                    })?;
                    let publication_lock = host.publication_lock(&scope, &name);
                    let _publication = wait_mcp_publication_lock(&publication_lock)
                        .map_err(PublicationOutcome::rejected)?;
                    let admission = host
                        .admit_scope(&session)
                        .map_err(PublicationOutcome::rejected)?;
                    // Loading is not a publication transition.
                    // The host holds the cross-process lock through validation,
                    // optional Kit preparation, exact registration check and load.
                    let mut prepare = || {
                        mcp_publication::prepare_publication_kit(
                            self,
                            &session,
                            kit.as_deref().ok_or("no Kit target")?,
                            stream.try_clone().map_err(|error| error.to_string())?,
                        )
                    };
                    host.load(
                        &session,
                        identity,
                        &name,
                        (kit.as_deref(), sandbox.as_deref()),
                        &mut prepare,
                        &admission,
                    )
                })();
                write_frame(&mut stream, &mcp_publication_reply(reply))
            }
            PublicRequest::McpUnpublish { session, name } => {
                let reply = (|| {
                    validate_mcp_publication_name(&name).map_err(PublicationOutcome::rejected)?;
                    if session.ephemeral_home {
                        return Err(PublicationOutcome::rejected(
                            "MCP publication requires a persistent home",
                        ));
                    }
                    let identity = self
                        .store
                        .verify_publication_project(&session)
                        .map_err(|error| PublicationOutcome::rejected(error.to_string()))?;
                    let scope = project_scope(&session.launch_directory, identity);
                    let host = self.mcp_host.as_ref().ok_or_else(|| {
                        PublicationOutcome::rejected("host MCP publication is unavailable")
                    })?;
                    let publication_lock = host.publication_lock(&scope, &name);
                    // Fence a slow admitted load before waiting for its lock.
                    // This does not claim cancellation of backend preparation.
                    host.mark_revocation_pending(&session, &name)
                        .map_err(PublicationOutcome::uncertain)?;
                    // New Kit VMs stop loading it before revocation starts.
                    host.drop_default(&session, &name)
                        .map_err(PublicationOutcome::uncertain)?;
                    let _publication = wait_mcp_publication_lock(&publication_lock)
                        .map_err(PublicationOutcome::uncertain)?;
                    let admission = host
                        .admit_scope(&session)
                        .map_err(PublicationOutcome::uncertain)?;
                    host.unpublish(&session, identity, &name, &admission)
                        .map_err(|outcome| PublicationOutcome::uncertain(format!("{}; revocation was already requested; inspect state and retry unpublish", outcome.message())))
                })();
                write_frame(&mut stream, &mcp_publication_reply(reply))
            }
            PublicRequest::ResetScope => {
                self.handle_scope_lifecycle(&mut stream, ScopeLifecycleAction::Reset)
            }
            PublicRequest::StopScope => {
                self.handle_scope_lifecycle(&mut stream, ScopeLifecycleAction::Stop)
            }
            PublicRequest::Execute(spec) => {
                // authorize_request_session has replaced caller-supplied roots
                // with retained authority. Inspect only guest path syntax and
                // component containment here, never host filesystem existence.
                if !spec.working_directory_is_admitted() {
                    return write_frame(
                        &mut stream,
                        &PublicReply::Error {
                            code: ErrorCode::InvalidRequest,
                            message: "execution working directory is outside session grants".into(),
                        },
                    );
                }
                // A split branch's job is the one exception: its claimed
                // workspace is checked on the host filesystem (registered
                // worktree or copy, no symlinks) so it can be mounted alone.
                if let Err(reason) = spec.split_confinement() {
                    return write_frame(
                        &mut stream,
                        &PublicReply::Error {
                            code: ErrorCode::InvalidRequest,
                            message: format!("split branch job refused: {reason}"),
                        },
                    );
                }
                if marsh_contracts::validate_exported_environment(&spec.environment).is_err() {
                    return write_frame(
                        &mut stream,
                        &PublicReply::Error {
                            code: ErrorCode::InvalidRequest,
                            message: "invalid exported shell environment".into(),
                        },
                    );
                }
                write_frame(&mut stream, &PublicReply::ExecutionAccepted)?;
                stream.set_read_timeout(None)?;
                stream.set_write_timeout(None)?;
                let attachment = ServerAttachment::new(stream)?;
                if let Err(error) =
                    self.backend
                        .execute(spec, attachment.clone(), self.store.clone())
                {
                    attachment.send(&AttachmentFrame::Failed {
                        message: error.to_string(),
                    })?;
                }
                Ok(())
            }
            PublicRequest::OpenShell(spec) => {
                let session_id = spec.session.session_id.clone();
                let mut owner = match self.store.begin_shell_attachment(&session_id) {
                    Ok(owner) => owner,
                    Err(error) => return write_frame(&mut stream, &public_error(&error)),
                };
                // The owner exists before ACK: failure here cannot orphan a
                // registration or retire a duplicate request's live generation.
                write_frame(&mut stream, &PublicReply::ShellAccepted)?;
                stream.set_read_timeout(None)?;
                stream.set_write_timeout(None)?;
                let attachment = ServerAttachment::new(stream)?;
                owner.started();
                let outcome = self
                    .backend
                    .open_shell(spec, attachment.clone(), self.store.clone());
                if matches!(outcome, Err(DaemonError::ShellCleanupUncertain(_))) {
                    self.store.mark_shell_cleanup_uncertain(&session_id);
                }
                let detached = owner.finish();
                self.acp.detach_shell(&session_id);
                if let Err(error) = outcome {
                    // Release the shell's controller before publishing failure.
                    attachment.send(&AttachmentFrame::Failed {
                        message: error.to_string(),
                    })?;
                }
                detached
            }
            PublicRequest::AcpReserve { adapter, session } => {
                let reply = self
                    .backend
                    .resolve_acp_agent(&adapter)
                    .and_then(|_| self.acp.reserve(&adapter, session));
                match reply {
                    Ok(agent_session_id) => {
                        write_frame(&mut stream, &PublicReply::AcpReserved { agent_session_id })
                    }
                    Err(error) => write_frame(&mut stream, &public_error(&error)),
                }
            }
            PublicRequest::AcpStart {
                adapter,
                session,
                reservation_id,
            } => {
                let startup = Arc::new(AcpStartupAbort::default());
                let startup_done = Arc::new(AtomicBool::new(false));
                let mut watcher = stream.try_clone()?;
                watcher.set_read_timeout(Some(Duration::from_millis(100)))?;
                let watched_startup = Arc::clone(&startup);
                let watched_done = Arc::clone(&startup_done);
                thread::spawn(move || {
                    let mut byte = [0u8; 1];
                    while !watched_done.load(Ordering::Acquire) {
                        match watcher.read(&mut byte) {
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                                ) => {}
                            _ => {
                                watched_startup.abandon();
                                break;
                            }
                        }
                    }
                });
                let started = self.acp.start_reserved(
                    Arc::clone(&self.backend),
                    &self.store,
                    &adapter,
                    session.clone(),
                    &startup,
                    reservation_id.as_deref(),
                );
                startup_done.store(true, Ordering::Release);
                match started {
                    Ok((agent_session_id, job_id)) => {
                        startup.disarm();
                        let sent = write_frame(
                            &mut stream,
                            &PublicReply::AcpStarted {
                                agent_session_id: agent_session_id.clone(),
                                job_id,
                            },
                        );
                        if sent.is_ok() {
                            let mut byte = [0u8; 1];
                            if stream
                                .set_read_timeout(Some(Duration::from_millis(100)))
                                .is_ok()
                            {
                                loop {
                                    if self.acp.run_finished(&agent_session_id, &self.store) {
                                        let _ = stream.write_all(b"T");
                                        break;
                                    }
                                    match stream.read(&mut byte) {
                                        Err(error)
                                            if matches!(
                                                error.kind(),
                                                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                                            ) => {}
                                        _ => break,
                                    }
                                }
                            }
                        }
                        self.acp.run_disconnected(
                            &agent_session_id,
                            &session.session_id,
                            &self.store,
                        );
                        sent
                    }
                    Err(error) => {
                        startup.abandon();
                        write_frame(&mut stream, &public_error(&error))
                    }
                }
            }
            PublicRequest::AcpPrompt {
                agent_session_id,
                session,
                operation_id,
                text,
            } => {
                let reply = self
                    .acp
                    .prompt_with_key(&agent_session_id, &session, &operation_id, text)
                    .map_or_else(
                        |error| public_error(&error),
                        |turn_id| PublicReply::AcpPromptAccepted { turn_id },
                    );
                write_frame(&mut stream, &reply)
            }
            PublicRequest::AcpCancel {
                agent_session_id,
                session,
            } => {
                let reply = self.acp.cancel(&agent_session_id, &session).map_or_else(
                    |error| public_error(&error),
                    |phase| PublicReply::AcpCancelled { phase },
                );
                write_frame(&mut stream, &reply)
            }
            PublicRequest::AcpRespond {
                agent_session_id,
                session,
                request_id,
                option_id,
            } => {
                let reply = self
                    .acp
                    .respond(&agent_session_id, &session, &request_id, &option_id)
                    .map_or_else(|error| public_error(&error), |()| PublicReply::AcpAccepted);
                write_frame(&mut stream, &reply)
            }
            PublicRequest::AcpStatus {
                agent_session_id,
                session,
                after,
            } => {
                let reply = self
                    .acp
                    .status(&agent_session_id, &session, after, &self.store)
                    .map_or_else(
                        |error| public_error(&error),
                        |status| PublicReply::AcpStatus(Box::new(status)),
                    );
                write_frame(&mut stream, &reply)
            }
            PublicRequest::AcpList { session } => write_frame(
                &mut stream,
                &PublicReply::AcpSessions {
                    sessions: self.acp.list(&session),
                },
            ),
            PublicRequest::ProcessView { session } => write_frame(
                &mut stream,
                &PublicReply::ProcessView(
                    self.store.process_view(&session, self.acp.list(&session)),
                ),
            ),
            PublicRequest::AcpAttach {
                agent_session_id,
                session,
            } => {
                let reply = self
                    .acp
                    .attach(&agent_session_id, &session)
                    .map_or_else(|error| public_error(&error), |()| PublicReply::AcpAccepted);
                write_frame(&mut stream, &reply)
            }
            PublicRequest::AcpRelease {
                agent_session_id,
                session,
            } => {
                let reply = self
                    .acp
                    .release(&agent_session_id, &session)
                    .map_or_else(|error| public_error(&error), |()| PublicReply::AcpAccepted);
                write_frame(&mut stream, &reply)
            }
            PublicRequest::AcpStop {
                agent_session_id,
                session,
            } => {
                let reply = self
                    .acp
                    .stop(&agent_session_id, &session)
                    .map_or_else(|error| public_error(&error), |()| PublicReply::AcpAccepted);
                write_frame(&mut stream, &reply)
            }
            PublicRequest::AcpPublishedPrompt {
                agent_session_id,
                generation,
                operation_id,
                text,
            } => {
                let reply = self
                    .acp
                    .published_prompt(&agent_session_id, &generation, &operation_id, text)
                    .map_or_else(
                        |error| public_error(&error),
                        |turn_id| PublicReply::AcpPromptAccepted { turn_id },
                    );
                write_frame(&mut stream, &reply)
            }
            PublicRequest::AcpPublishedStatus {
                agent_session_id,
                generation,
                after,
            } => {
                let reply = self
                    .acp
                    .published_status(&agent_session_id, &generation, after, &self.store)
                    .map_or_else(
                        |error| public_error(&error),
                        |status| PublicReply::AcpStatus(Box::new(status)),
                    );
                write_frame(&mut stream, &reply)
            }
            PublicRequest::AcpPublishedCancel {
                agent_session_id,
                generation,
            } => {
                let reply = self
                    .acp
                    .published_cancel(&agent_session_id, &generation)
                    .map_or_else(
                        |error| public_error(&error),
                        |phase| PublicReply::AcpCancelled { phase },
                    );
                write_frame(&mut stream, &reply)
            }
            PublicRequest::AcpPublishedRespond {
                agent_session_id,
                generation,
                request_id,
                option_id,
            } => {
                let reply = self
                    .acp
                    .published_respond(&agent_session_id, &generation, &request_id, &option_id)
                    .map_or_else(|error| public_error(&error), |()| PublicReply::AcpAccepted);
                write_frame(&mut stream, &reply)
            }
            PublicRequest::AcpPublish {
                agent_session_id,
                session,
                name,
                sandbox,
                kit,
            } => {
                let reply = (|| -> Result<PublicationCommit, PublicationOutcome> {
                    validate_mcp_publication_name(&name).map_err(PublicationOutcome::rejected)?;
                    validate_publication_options(None, kit.as_deref(), sandbox.as_deref(), false)
                        .map_err(PublicationOutcome::rejected)?;
                    if session.ephemeral_home {
                        return Err(PublicationOutcome::rejected(
                            "ACP publication requires a persistent home",
                        ));
                    }
                    let identity = self
                        .store
                        .verify_publication_project(&session)
                        .map_err(|error| PublicationOutcome::rejected(error.to_string()))?;
                    let scope = project_scope(&session.launch_directory, identity);
                    let host = self.mcp_host.as_ref().ok_or_else(|| {
                        PublicationOutcome::rejected("host ACP publication is unavailable")
                    })?;
                    let publication_lock = host.publication_lock(&scope, &name);
                    let _publication = wait_mcp_publication_lock(&publication_lock)
                        .map_err(PublicationOutcome::rejected)?;
                    if let Some(kit) = &kit
                        && !self
                            .backend
                            .registered_kits()
                            .map_err(|error| PublicationOutcome::rejected(error.to_string()))?
                            .contains_key(kit)
                    {
                        return Err(PublicationOutcome::rejected(
                            "unknown Kit command; use a registered command or --sandbox SANDBOX",
                        ));
                    }
                    let admission = host
                        .admit_scope(&session)
                        .map_err(PublicationOutcome::rejected)?;
                    host.preflight_sandbox(&session, sandbox.as_deref(), &admission)?;
                    // This is authoritative admission, not an advisory check:
                    // the grant reserves the name and fences parent steering
                    // before any potentially cold Kit preparation.
                    let generation = self
                        .acp
                        .publish(&agent_session_id, &session, &name)
                        .map_err(|error| PublicationOutcome::rejected(error.to_string()))?;
                    let check = || {
                        self.acp
                            .validate_publication(&agent_session_id, &generation)
                            .map_err(|error| error.to_string())?;
                        self.store
                            .verify_publication_project(&session)
                            .map_err(|error| error.to_string())?;
                        Ok::<_, String>(())
                    };
                    let mut prepare = || {
                        check()?;
                        let sandbox = mcp_publication::prepare_publication_kit(
                            self,
                            &session,
                            kit.as_deref()
                                .ok_or("unexpected ACP Kit preparation request")?,
                            stream.try_clone().map_err(|error| error.to_string())?,
                        )?;
                        check()?;
                        Ok(sandbox)
                    };
                    // The same host-file lock and private preparation handshake
                    // as MCP: reject cross-protocol/stock conflicts before boot.
                    let publish = host
                        .publish_acp(
                            &session,
                            identity,
                            &name,
                            &agent_session_id,
                            &generation,
                            sandbox.as_deref(),
                            kit.as_deref(),
                            &mut prepare,
                            &mut || check(),
                            &admission,
                        )
                        .and_then(|commit| {
                            check().map_err(PublicationOutcome::uncertain)?;
                            Ok(commit)
                        });
                    match publish {
                        Ok(commit) => Ok(commit),
                        Err(error) => {
                            let rollback = self.acp.rollback_publication(
                                &agent_session_id,
                                &generation,
                                &session.session_id,
                            );
                            // Grant admission
                            // already happened. A host-only preflight rejection cannot
                            // turn this transaction into a no-effect rejection.
                            let host_effects =
                                !matches!(&error, PublicationOutcome::RejectedBeforeEffect { .. });
                            let host = acp_host_failure(error);
                            let remedy = if host_effects {
                                format!(
                                    "Prepared Kits or host registration may remain. Run `acp status {agent_session_id}` and `acp unpublish {name}` before publishing again."
                                )
                            } else {
                                format!(
                                    "The host did not prepare a Kit or mutate registration. Run `acp status {agent_session_id}`, correct the reported host conflict, then retry `acp publish {agent_session_id} --name {name}` with the intended target. Do not remove a tool owned by the other protocol."
                                )
                            };
                            Err(PublicationOutcome::uncertain(match rollback {
                                Ok(()) => format!(
                                    "{host}; ACP grant was revoked after admission. {remedy}"
                                ),
                                Err(rollback) => format!(
                                    "{host}; ACP grant rollback failed: {rollback}. Run `acp status {agent_session_id}` and `acp unpublish {name}`; no cancellation or cleanup is confirmed"
                                ),
                            }))
                        }
                    }
                })();
                write_frame(&mut stream, &acp_publication_reply(reply))
            }
            PublicRequest::AcpUnpublish { session, name } => {
                let reply = (|| -> Result<PublicationCommit, PublicationOutcome> {
                    validate_mcp_publication_name(&name).map_err(PublicationOutcome::rejected)?;
                    if session.ephemeral_home {
                        return Err(PublicationOutcome::rejected(
                            "ACP publication requires a persistent home; use the publishing shell and selected home to unpublish",
                        ));
                    }
                    let identity = self
                        .store
                        .verify_publication_project(&session)
                        .map_err(|error| PublicationOutcome::rejected(error.to_string()))?;
                    let scope = project_scope(&session.launch_directory, identity);
                    let host = self.mcp_host.as_ref().ok_or_else(|| {
                        PublicationOutcome::rejected("host ACP publication is unavailable")
                    })?;
                    let publication_lock = host.publication_lock(&scope, &name);
                    let _publication = wait_mcp_publication_lock(&publication_lock)
                        .map_err(PublicationOutcome::rejected)?;
                    // A stopped/restarted daemon has no in-memory grant, but
                    // its fail-closed host registration may still need removal.
                    let revoked = match self.acp.unpublish(&name, &session, None) {
                        Ok(_) => true,
                        Err(DaemonError::NotFound(_)) => false,
                        Err(error) => return Err(PublicationOutcome::rejected(error.to_string())),
                    };
                    let admission = host.admit_scope(&session).map_err(|error| {
                        if revoked {
                            PublicationOutcome::uncertain(format!(
                                "{error}; ACP grant was revoked; host cleanup is pending"
                            ))
                        } else {
                            PublicationOutcome::rejected(error)
                        }
                    })?;
                    host.unpublish_acp(&session, identity, &name, &mut || {
                        self.store.verify_publication_project(&session).map(|_| ()).map_err(|error| error.to_string())
                    }, &admission).map_err(|outcome| {
                        if revoked {
                            PublicationOutcome::uncertain(format!("{}; ACP grant was revoked, but host removal did not complete; retry `acp unpublish {name}` and inspect the host registration if it still fails.", acp_host_failure(outcome)))
                        } else {
                            outcome
                        }
                    })
                })();
                write_frame(&mut stream, &acp_publication_reply(reply))
            }
            PublicRequest::DetachShell { session_id } => {
                let reply = match self.store.detach_shell(&session_id) {
                    Ok(()) => {
                        if self.store.ephemeral_home_token(&session_id).is_some()
                            && let Err(error) = self
                                .backend
                                .close_ephemeral_session(&session_id, &self.store)
                        {
                            self.store.mark_shell_cleanup_uncertain(&session_id);
                            return write_frame(&mut stream, &public_error(&error));
                        }
                        self.acp.detach_shell(&session_id);
                        PublicReply::Detached
                    }
                    Err(error) => public_error(&error),
                };
                write_frame(&mut stream, &reply)
            }
            PublicRequest::Prepare {
                selection,
                session,
                dev,
            } => {
                let session_id = session.session_id.clone();
                write_frame(&mut stream, &PublicReply::PreparationAccepted)?;
                stream.set_read_timeout(None)?;
                stream.set_write_timeout(None)?;
                let progress = PreparationProgress::new(stream);
                let prepared = if dev {
                    self.backend.prepare_dev(
                        &selection,
                        &session,
                        progress.clone(),
                        self.store.clone(),
                    )
                } else {
                    self.backend
                        .prepare(&selection, &session, progress.clone(), self.store.clone())
                };
                match prepared {
                    Ok(result) => progress.send(&PreparationFrame::Complete { result }),
                    Err(error) => {
                        let detached = self.store.detach_shell(&session_id);
                        // As with shell setup, publish failure only after the
                        // session is no longer attached.
                        let notification = progress.send(&PreparationFrame::Failed {
                            message: error.to_string(),
                        });
                        notification?;
                        detached
                    }
                }
            }
            request => write_frame(
                &mut stream,
                &dispatch_request(
                    &self.store,
                    self.backend.as_ref(),
                    &self.build_identity,
                    request,
                ),
            ),
        }
    }

    #[allow(clippy::too_many_lines)] // Transport grants and request admission share this boundary.
    fn handle(&self, mut stream: UnixStream) -> Result<(), DaemonError> {
        stream.set_nonblocking(false)?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        let mut request: Envelope<PublicRequest> = read_frame(&mut DeadlineRead {
            stream: &mut stream,
            deadline: Instant::now() + DAEMON_HANDSHAKE_TIMEOUT,
        })?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let (true, Some(authentication_scope)) = (
            request.protocol == PROTOCOL,
            self.store.authentication_scope(&request.token),
        ) else {
            return Self::reject_unauthorized(&mut stream, "daemon authentication failed");
        };
        if let Some(message) = request.body.authority().rejection(&authentication_scope) {
            return Self::reject_unauthorized(&mut stream, message);
        }
        if let AuthenticationScope::Capability(job) = &authentication_scope {
            // The capability admits a job's own splits and nothing else; its
            // session is the job's, never a payload field.
            let job = job.clone();
            return match request.body {
                PublicRequest::SplitCreate(mut spec) => match self.store.job_session(&job) {
                    Some(session) => {
                        spec.session = session;
                        self.splits.create(
                            stream,
                            split::Creator::Job(job),
                            spec,
                            self.backend.as_ref(),
                            &self.store,
                        )
                    }
                    None => Self::reject_unauthorized(&mut stream, "job session is gone"),
                },
                PublicRequest::SplitJoin { split } => {
                    self.splits.join(stream, &split::Creator::Job(job), &split)
                }
                PublicRequest::ProcessRun(spec) => {
                    process::serve_run(self.splits.client(), &self.store, stream, &job, spec)
                }
                PublicRequest::ProcessShow => write_frame(
                    &mut stream,
                    &PublicReply::ProcessTree {
                        document: self.store.process_tree(Some(&job)),
                    },
                ),
                PublicRequest::Jobs => {
                    let mut jobs = self.store.jobs();
                    jobs.jobs
                        .retain(|summary| self.store.in_subtree(&job, &summary.job_id));
                    write_frame(&mut stream, &PublicReply::Jobs(jobs))
                }
                PublicRequest::ShowJob { job_id } => {
                    let reply = match self.store.job(&job_id) {
                        Ok(mut found) if self.store.in_subtree(&job, &found.job_id) => {
                            found.children = self.store.process_children(&found.job_id);
                            PublicReply::Job(Box::new(found))
                        }
                        Ok(_) | Err(_) => public_error(&DaemonError::NotFound(job_id)),
                    };
                    write_frame(&mut stream, &reply)
                }
                _ => Self::reject_unauthorized(
                    &mut stream,
                    "the job capability admits only ProcessRun, ProcessShow, jobs, SplitCreate, and SplitJoin",
                ),
            };
        }
        let lifecycle_request = matches!(
            request.body,
            PublicRequest::ResetScope | PublicRequest::StopScope | PublicRequest::Shutdown
        );
        let admission = if lifecycle_request {
            self.store.begin_lifecycle_request()
        } else {
            self.store.begin_ordinary_request()
        };
        let _admission = match admission {
            Ok(admission) => admission,
            Err(error) => {
                return write_frame(
                    &mut stream,
                    &PublicReply::Error {
                        code: ErrorCode::InvalidRequest,
                        message: match error {
                            DaemonError::InvalidState(message) => message,
                            other => other.to_string(),
                        },
                    },
                );
            }
        };
        authorize_request_session(
            &self.store,
            &authentication_scope,
            &request.token,
            &mut request.body,
        )?;
        if let PublicRequest::DevSbx { argv, pty, cwd } = &request.body {
            // The relay's authentication is the capability: the session is
            // the relay owner, never a payload field.
            let AuthenticationScope::Relay(session) = &authentication_scope else {
                return Self::reject_unauthorized(
                    &mut stream,
                    "development grant requires a relay",
                );
            };
            let Some(broker) = &self.dev_broker else {
                return dev_broker::stream::reject(&stream, "development broker is not enabled");
            };
            stream.set_read_timeout(None)?;
            stream.set_write_timeout(None)?;
            let result = broker.serve(&stream, session, argv, *pty, cwd.as_deref());
            if let Err(error) = &result {
                // Report in-band; a peer already streaming treats it as terminal.
                let _ = dev_broker::stream::reject(&stream, &error.to_string());
            }
            return result;
        }
        let caller = match &authentication_scope {
            AuthenticationScope::Master => split::Creator::Host,
            AuthenticationScope::Relay(session) => split::Creator::Session(session.clone()),
            AuthenticationScope::Capability(job) => split::Creator::Job(job.clone()),
        };
        match request.body {
            PublicRequest::SplitCreate(spec) => {
                self.splits
                    .create(stream, caller, spec, self.backend.as_ref(), &self.store)
            }
            PublicRequest::SplitJoin { split } => self.splits.join(stream, &caller, &split),
            PublicRequest::ProcessShow => write_frame(
                &mut stream,
                &PublicReply::ProcessTree {
                    document: self.store.process_forest(&self.splits.lineage()),
                },
            ),
            PublicRequest::SplitShow { split } => {
                let reply = self.splits.show(&caller, split.as_deref()).map_or_else(
                    |error| public_error(&error),
                    |document| PublicReply::Splits { document },
                );
                write_frame(&mut stream, &reply)
            }
            PublicRequest::SplitCancel { split } => {
                let reply = self.splits.cancel_request(&caller, &split).map_or_else(
                    |error| public_error(&error),
                    |()| PublicReply::SplitDone {
                        message: format!("cancelled {split}"),
                    },
                );
                write_frame(&mut stream, &reply)
            }
            PublicRequest::SplitRemove { split } => {
                let reply = self.splits.remove(&caller, &split).map_or_else(
                    |error| public_error(&error),
                    |message| PublicReply::SplitDone { message },
                );
                write_frame(&mut stream, &reply)
            }
            PublicRequest::Status { session_id } => {
                let mut status = self.store.status(session_id);
                status.splits = self.splits.counts();
                status.processes = self.store.process_counts();
                write_frame(&mut stream, &PublicReply::Status(status))
            }
            PublicRequest::ResetWorkers { selection } => {
                let kits = match &selection {
                    LoadSelection::All => vec!["all".to_owned()],
                    LoadSelection::Kits(kits) => kits.clone(),
                };
                let reply = dispatch_request(
                    &self.store,
                    self.backend.as_ref(),
                    &self.build_identity,
                    PublicRequest::ResetWorkers { selection },
                );
                // Only a successful retirement settles held slots: every
                // uncertain branch slot and the reset Kits' uncertain jobs.
                if matches!(reply, PublicReply::WorkersReset { .. }) {
                    self.splits.release_uncertain_slots();
                    self.store.release_process_slots(&kits);
                }
                write_frame(&mut stream, &reply)
            }
            body => self.handle_authorized(stream, body),
        }
    }
}

fn authorize_request_session(
    store: &DaemonStore,
    authentication_scope: &AuthenticationScope,
    token: &str,
    request: &mut PublicRequest,
) -> Result<(), DaemonError> {
    if let PublicRequest::DetachShell { session_id } = request
        && matches!(
            authentication_scope,
            AuthenticationScope::Relay(owner) if owner != session_id
        )
    {
        return Err(DaemonError::InvalidState(
            "relay token does not own the requested session".into(),
        ));
    }
    let session = match request {
        PublicRequest::Execute(spec) | PublicRequest::ProcessRun(spec) => {
            if matches!(authentication_scope, AuthenticationScope::Relay(_))
                && let Some(link) = &mut spec.process
            {
                link.parent_job = None;
            }
            &mut spec.session
        }
        PublicRequest::OpenShell(spec) => &mut spec.session,
        PublicRequest::SplitCreate(spec) => &mut spec.session,
        PublicRequest::Prepare { session, .. }
        | PublicRequest::AcpStart { session, .. }
        | PublicRequest::AcpReserve { session, .. }
        | PublicRequest::AcpPrompt { session, .. }
        | PublicRequest::AcpCancel { session, .. }
        | PublicRequest::AcpRespond { session, .. }
        | PublicRequest::AcpStatus { session, .. }
        | PublicRequest::AcpList { session }
        | PublicRequest::ProcessView { session }
        | PublicRequest::AcpAttach { session, .. }
        | PublicRequest::AcpRelease { session, .. }
        | PublicRequest::AcpStop { session, .. }
        | PublicRequest::AcpPublish { session, .. }
        | PublicRequest::AcpUnpublish { session, .. }
        | PublicRequest::McpPublish { session, .. }
        | PublicRequest::McpLoad { session, .. }
        | PublicRequest::McpUnpublish { session, .. } => session,
        PublicRequest::Ping
        | PublicRequest::Shutdown
        | PublicRequest::RegisteredCommands
        | PublicRequest::RegisteredKits
        | PublicRequest::ResetWorkers { .. }
        | PublicRequest::ResetScope
        | PublicRequest::StopScope
        | PublicRequest::AttachShell { .. }
        | PublicRequest::AttachPinnedShell { .. }
        | PublicRequest::AttachEphemeralShell { .. }
        | PublicRequest::DetachShell { .. }
        | PublicRequest::Status { .. }
        | PublicRequest::Jobs
        | PublicRequest::ShowJob { .. }
        | PublicRequest::AcpPublishedPrompt { .. }
        | PublicRequest::AcpPublishedStatus { .. }
        | PublicRequest::AcpPublishedCancel { .. }
        | PublicRequest::AcpPublishedRespond { .. }
        | PublicRequest::InstallKit { .. }
        | PublicRequest::SplitJoin { .. }
        | PublicRequest::SplitShow { .. }
        | PublicRequest::SplitCancel { .. }
        | PublicRequest::SplitRemove { .. }
        | PublicRequest::ProcessShow
        | PublicRequest::DevSbx { .. } => return Ok(()),
    };
    *session = store.authorize_session(token, session)?;
    Ok(())
}

#[allow(clippy::too_many_lines)] // Exhaustive protocol fallback for the host-owned request variants.
fn dispatch_request(
    store: &DaemonStore,
    backend: &dyn DaemonBackend,
    build_identity: &str,
    request: PublicRequest,
) -> PublicReply {
    match request {
        PublicRequest::Ping => PublicReply::Pong {
            daemon_id: store.daemon_id(),
            build_identity: Some(build_identity.into()),
            pid: Some(std::process::id()),
        },
        PublicRequest::Shutdown | PublicRequest::ResetScope | PublicRequest::StopScope => {
            PublicReply::Error {
                code: ErrorCode::Internal,
                message: "request was not handled by the host lifecycle path".into(),
            }
        }
        PublicRequest::RegisteredCommands => match backend.registered_commands() {
            Ok(mut commands) => {
                commands.sort();
                commands.dedup();
                PublicReply::RegisteredCommands { commands }
            }
            Err(error) => public_error(&error),
        },
        PublicRequest::InstallKit { command, reference } => {
            match backend.install_kit(command.clone(), reference, store.clone()) {
                Ok(reference) => PublicReply::InstalledKit { command, reference },
                Err(error) => public_error(&error),
            }
        }
        PublicRequest::RegisteredKits => match backend.registered_kits() {
            Ok(kits) => PublicReply::RegisteredKits { kits },
            Err(error) => public_error(&error),
        },
        PublicRequest::ResetWorkers { selection } => {
            match backend.reset_workers(&selection, store.clone()) {
                Ok(kits) => PublicReply::WorkersReset { kits },
                Err(error) => public_error(&error),
            }
        }
        PublicRequest::Prepare { .. } => PublicReply::Error {
            code: ErrorCode::Internal,
            message: "preparation attachment was not established".into(),
        },
        PublicRequest::DevSbx { .. } => PublicReply::Error {
            code: ErrorCode::Internal,
            message: "development broker request was not handled by the host controller".into(),
        },
        PublicRequest::Execute(_) => PublicReply::Error {
            code: ErrorCode::Internal,
            message: "execution attachment was not established".into(),
        },
        PublicRequest::OpenShell(_) => PublicReply::Error {
            code: ErrorCode::Internal,
            message: "shell attachment was not established".into(),
        },
        PublicRequest::SplitCreate(_)
        | PublicRequest::SplitJoin { .. }
        | PublicRequest::SplitShow { .. }
        | PublicRequest::SplitCancel { .. }
        | PublicRequest::SplitRemove { .. } => PublicReply::Error {
            code: ErrorCode::Internal,
            message: "split request was not handled by the split engine".into(),
        },
        PublicRequest::ProcessRun(_) => PublicReply::Error {
            code: ErrorCode::InvalidRequest,
            message: "ProcessRun is the job capability request; use Execute".into(),
        },
        PublicRequest::ProcessShow => PublicReply::ProcessTree {
            document: store.process_tree(None),
        },
        PublicRequest::AcpStart { .. }
        | PublicRequest::AcpReserve { .. }
        | PublicRequest::AcpPrompt { .. }
        | PublicRequest::AcpCancel { .. }
        | PublicRequest::AcpRespond { .. }
        | PublicRequest::AcpStatus { .. }
        | PublicRequest::AcpList { .. }
        | PublicRequest::ProcessView { .. }
        | PublicRequest::AcpAttach { .. }
        | PublicRequest::AcpRelease { .. }
        | PublicRequest::AcpPublish { .. }
        | PublicRequest::AcpUnpublish { .. }
        | PublicRequest::AcpPublishedPrompt { .. }
        | PublicRequest::AcpPublishedStatus { .. }
        | PublicRequest::AcpPublishedCancel { .. }
        | PublicRequest::AcpPublishedRespond { .. } => PublicReply::Error {
            code: ErrorCode::Internal,
            message: "ACP request was not handled by the session controller".into(),
        },
        PublicRequest::AcpStop { .. } => PublicReply::Error {
            code: ErrorCode::Internal,
            message: "ACP stop was not handled by the session controller".into(),
        },
        PublicRequest::McpPublish { .. }
        | PublicRequest::McpLoad { .. }
        | PublicRequest::McpUnpublish { .. } => PublicReply::Error {
            code: ErrorCode::Internal,
            message: "MCP publication was not handled by the host controller".into(),
        },
        PublicRequest::AttachShell { pid, session } => PublicReply::ShellAttached {
            session_id: store.attach_shell(pid, session),
        },
        PublicRequest::AttachPinnedShell { .. } | PublicRequest::AttachEphemeralShell { .. } => {
            PublicReply::Error {
                code: ErrorCode::Internal,
                message: "pinned attachment was not handled by the host controller".into(),
            }
        }
        PublicRequest::DetachShell { session_id } => match store.detach_shell(&session_id) {
            Ok(()) => PublicReply::Detached,
            Err(error) => public_error(&error),
        },
        PublicRequest::Status { session_id } => PublicReply::Status(store.status(session_id)),
        PublicRequest::Jobs => PublicReply::Jobs(store.jobs()),
        PublicRequest::ShowJob { job_id } => match store.job(&job_id) {
            Ok(mut job) => {
                job.children = store.process_children(&job.job_id);
                PublicReply::Job(Box::new(job))
            }
            Err(error) => public_error(&error),
        },
    }
}

fn validate_mcp_publication_name(name: &str) -> Result<(), String> {
    PublishedName::parse(name).map(|_| ())
}

fn wait_mcp_publication_lock(lock: &Mutex<()>) -> Result<std::sync::MutexGuard<'_, ()>, String> {
    // Lock admission has a bounded wait. Cold preparation is a separate
    // backend-owned phase, excluded from the host's stock-command budget.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match lock.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                return Err("MCP publication for this project and name is busy; retry".into());
            }
        }
    }
}

fn mcp_publication_reply(result: Result<PublicationCommit, PublicationOutcome>) -> PublicReply {
    PublicReply::McpPublication {
        outcome: result.map_or_else(|outcome| outcome, PublicationOutcome::Committed),
    }
}

fn acp_publication_reply(result: Result<PublicationCommit, PublicationOutcome>) -> PublicReply {
    PublicReply::AcpPublication {
        outcome: result.map_or_else(|outcome| outcome, PublicationOutcome::Committed),
    }
}

// Describe the host phase without falsely advertising whole-ACP no-effect
// semantics after the daemon has already admitted or revoked a grant.
fn acp_host_failure(outcome: PublicationOutcome) -> String {
    match outcome {
        PublicationOutcome::RejectedBeforeEffect { message } => {
            format!("Host preflight rejected: {message}")
        }
        PublicationOutcome::Uncertain { message } => format!("Host outcome uncertain: {message}"),
        PublicationOutcome::Committed(_) => {
            "Host returned an inconsistent publication result".into()
        }
    }
}

fn public_error(error: &DaemonError) -> PublicReply {
    let code = match error {
        DaemonError::NotFound(_) => ErrorCode::NotFound,
        DaemonError::InvalidState(_)
        | DaemonError::ShellAttachmentBusy(_)
        | DaemonError::ShellStdinClosed
        | DaemonError::ShellInputTransportClosed(_) => ErrorCode::InvalidRequest,
        _ => ErrorCode::Internal,
    };
    PublicReply::Error {
        code,
        message: error.to_string(),
    }
}

#[derive(Clone, Debug)]
pub struct ServerAttachment {
    reader: Arc<Mutex<UnixStream>>,
    reader_stopped: Arc<AtomicBool>,
    writer: Arc<Mutex<UnixStream>>,
    writer_shutdown: Arc<UnixStream>,
}

const ATTACHMENT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct PreparationProgress {
    writer: Arc<Mutex<UnixStream>>,
}

impl PreparationProgress {
    fn new(stream: UnixStream) -> Self {
        Self {
            writer: Arc::new(Mutex::new(stream)),
        }
    }

    pub fn cold_boot(&self, kit: impl Into<String>) -> Result<(), DaemonError> {
        self.send(&PreparationFrame::ColdBoot { kit: kit.into() })
    }

    fn send(&self, frame: &PreparationFrame) -> Result<(), DaemonError> {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        write_frame(&mut *writer, frame)
    }
}

impl ServerAttachment {
    /// Builds an in-process framed attachment for an independently managed
    /// Kit caller. The backend receives `server`; the controller retains `client`.
    pub fn pair() -> Result<(Self, ClientExecution), DaemonError> {
        let (server, client) = UnixStream::pair()?;
        Ok((Self::new(server)?, ClientExecution::new(client)?))
    }

    /// In-process shell caller using the same stdin credit window as public `OpenShell`.
    pub fn shell_pair() -> Result<(Self, ClientExecution), DaemonError> {
        let (server, client) = UnixStream::pair()?;
        Ok((Self::new(server)?, ClientExecution::new_shell(client)?))
    }

    /// A healthy slow controller backpressures its producer. Socket polling
    /// only supplies wakeups, never a connected delivery deadline.
    pub fn new(stream: UnixStream) -> Result<Self, DaemonError> {
        stream.set_read_timeout(Some(Duration::from_millis(20)))?;
        let writer = stream.try_clone()?;
        writer.set_write_timeout(Some(Duration::from_millis(20)))?;
        let writer_shutdown = Arc::new(writer.try_clone()?);
        Ok(Self {
            reader: Arc::new(Mutex::new(stream)),
            reader_stopped: Arc::new(AtomicBool::new(false)),
            writer: Arc::new(Mutex::new(writer)),
            writer_shutdown,
        })
    }

    /// Wake a control receiver after process completion, preserving final output.
    pub fn close_input(&self) {
        // Do not SHUT_RD: an in-flight client input write would get EPIPE and
        // could close its output half before receiving the real terminal status.
        self.reader_stopped.store(true, Ordering::Release);
    }

    /// Wake all attachment I/O after delivery/control loss.
    pub fn shutdown(&self) -> io::Result<()> {
        self.close_input();
        self.writer_shutdown.shutdown(Shutdown::Both)
    }

    pub fn receive(&self) -> Result<AttachmentFrame, DaemonError> {
        let mut reader = self
            .reader
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = read_frame(&mut AttachmentReader {
            stream: &mut reader,
            stopped: &self.reader_stopped,
        });
        if result.is_err() && !self.reader_stopped.load(Ordering::Acquire) {
            let _ = self.shutdown();
        }
        result
    }

    /// Independent socket observation, including during preparation when no
    /// protocol reader exists yet. Lack of write progress is never evidence.
    pub fn peer_disconnected(&self) -> io::Result<bool> {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        let mut descriptors = [PollFd::new(&*self.writer_shutdown, PollFlags::empty())];
        poll(
            &mut descriptors,
            Some(&Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
        )?;
        Ok(descriptors[0]
            .revents()
            .intersects(PollFlags::HUP | PollFlags::ERR))
    }

    pub fn send(&self, frame: &AttachmentFrame) -> Result<(), DaemonError> {
        self.send_with_deadline(frame, None)
    }

    fn send_with_deadline(
        &self,
        frame: &AttachmentFrame,
        deadline: Option<Instant>,
    ) -> Result<(), DaemonError> {
        // No detached writer or per-caller frame clone/queue. The caller owns
        // backpressure; explicit budget expiration/shutdown needs no writer lock.
        let mut writer = if deadline.is_some() {
            loop {
                if let Err(error) = attachment_budget(deadline) {
                    let _ = self.shutdown();
                    return Err(error.into());
                }
                match self.writer.try_lock() {
                    Ok(writer) => break writer,
                    Err(std::sync::TryLockError::Poisoned(error)) => break error.into_inner(),
                    Err(std::sync::TryLockError::WouldBlock) => {
                        thread::sleep(Duration::from_millis(2));
                    }
                }
            }
        } else {
            self.writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        let result = write_frame(&mut AttachmentWriter(&mut writer, deadline), frame);
        if result.is_err() {
            // A partial frame may have reached the peer. Fence the stream;
            // never let another sender append a new header to a torn frame.
            let _ = self.shutdown();
        }
        result
    }
}

fn attachment_budget(deadline: Option<Instant>) -> io::Result<()> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "admitted attachment delivery budget expired",
        ))
    } else {
        Ok(())
    }
}

struct AttachmentWriter<'a>(&'a mut UnixStream, Option<Instant>);

impl Write for AttachmentWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            attachment_budget(self.1)?;
            match self.0.write(bytes) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

// Retry polling timeouts inside Read, not around read_frame/read_exact. This
// preserves partially received headers/payloads while allowing explicit wakeup.
struct AttachmentReader<'a> {
    stream: &'a mut UnixStream,
    stopped: &'a AtomicBool,
}

impl Read for AttachmentReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.stopped.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "attachment receiver stopped",
                ));
            }
            match self.stream.read(buffer) {
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                result => return result,
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Client {
    paths: EndpointPaths,
    token: String,
}

impl Client {
    pub fn install_kit(&self, command: &str, reference: &str) -> Result<String, DaemonError> {
        match self.request_with_timeout(
            PublicRequest::InstallKit {
                command: command.into(),
                reference: reference.into(),
            },
            Duration::from_mins(8),
        )? {
            PublicReply::InstalledKit { reference, .. } => Ok(reference),
            reply => Err(unexpected_reply(reply)),
        }
    }

    /// Connect to a live exact-home daemon without starting one. A cleanly
    /// absent or conclusively stale endpoint returns `None`; partial or unsafe
    /// endpoint state fails closed.
    pub fn connect_if_running(home: &Path) -> Result<Option<Self>, DaemonError> {
        let paths = EndpointPaths::for_home(home)?;
        if !paths.runtime_directory.exists() {
            return Ok(None);
        }
        verify_owned_directory(&paths.runtime_directory)?;
        let present = [
            paths.lock.exists(),
            paths.socket.exists(),
            paths.token.exists(),
        ];
        if present.iter().all(|value| !value) {
            return Ok(None);
        }
        if !present.iter().all(|value| *value) {
            return Err(DaemonError::EndpointInconsistent(
                paths.runtime_directory.clone(),
            ));
        }
        match reclaim_stale_endpoint(&paths)? {
            ReclaimOutcome::Live => Self::connect(home).map(Some),
            ReclaimOutcome::Reclaimed => Ok(None),
            ReclaimOutcome::Absent => Err(DaemonError::EndpointInconsistent(
                paths.runtime_directory.clone(),
            )),
        }
    }

    pub fn connect(home: &Path) -> Result<Self, DaemonError> {
        let paths = EndpointPaths::for_home(home)?;
        verify_secret(&paths.token)?;
        let token = read_secret(&paths.token)?;
        Ok(Self { paths, token })
    }

    /// Connect through a trusted guest relay endpoint. Both paths must be
    /// owner-only files under the shell VM's private runtime tree.
    /// A Kit job's split capability socket; the worker path supplies the
    /// authority, so the token is empty.
    #[must_use]
    pub fn capability(socket: &Path) -> Self {
        Self {
            paths: EndpointPaths {
                runtime_directory: socket.parent().unwrap_or(socket).to_owned(),
                socket: socket.to_owned(),
                lock: PathBuf::new(),
                token: PathBuf::new(),
            },
            token: String::new(),
        }
    }

    /// Replace the token of a forwarded request envelope (capability bridge).
    ///
    /// # Errors
    /// Returns an error for a malformed envelope.
    pub fn reauthorize_envelope(frame: &[u8], token: &str) -> Result<Vec<u8>, DaemonError> {
        let mut envelope: Envelope<serde_json::Value> = serde_json::from_slice(frame)
            .map_err(|error| DaemonError::InvalidState(format!("invalid envelope: {error}")))?;
        token.clone_into(&mut envelope.token);
        serde_json::to_vec(&envelope)
            .map_err(|error| DaemonError::InvalidState(format!("invalid envelope: {error}")))
    }

    pub fn connect_relay(socket: &Path, token: &Path) -> Result<Self, DaemonError> {
        verify_secret(token)?;
        let socket_metadata = fs::symlink_metadata(socket)?;
        if socket_metadata.file_type().is_symlink()
            || !socket_metadata.file_type().is_socket()
            || socket_metadata.uid() != rustix::process::getuid().as_raw()
            || socket_metadata.mode() & 0o077 != 0
        {
            return Err(DaemonError::UnsafeToken(socket.to_owned()));
        }
        Ok(Self {
            paths: EndpointPaths {
                runtime_directory: socket
                    .parent()
                    .ok_or_else(|| DaemonError::UnsafeToken(socket.to_owned()))?
                    .to_owned(),
                socket: socket.to_owned(),
                lock: PathBuf::new(),
                token: token.to_owned(),
            },
            token: read_secret(token)?,
        })
    }

    pub fn ensure_running(
        home: &Path,
        daemon_program: &Path,
        stock_sbx: &Path,
    ) -> Result<Self, DaemonError> {
        daemon_startup::ensure_running(home, daemon_program, stock_sbx)
    }

    fn reconcile_resident(
        home: &Path,
        paths: &EndpointPaths,
        expected_build_identity: &str,
    ) -> Result<Option<Self>, DaemonError> {
        if let Ok(client) = Self::connect(home)
            && let Ok(PublicReply::Pong {
                daemon_id,
                build_identity,
                ..
            }) = client.request(PublicRequest::Ping)
        {
            if build_identity.as_deref() == Some(expected_build_identity) {
                return Ok(Some(client));
            }
            if build_identity.is_some() {
                client.shutdown_resident_after_probe()?;
            } else {
                let owner = verified_legacy_owner(&client, paths, &daemon_id)?;
                rustix::process::kill_process(owner, rustix::process::Signal::TERM)
                    .map_err(|error| DaemonError::Io(error.into()))?;
            }
            wait_for_owner_release(paths)?;
        }
        Ok(None)
    }

    fn shutdown_resident_after_probe(&self) -> Result<(), DaemonError> {
        let deadline = Instant::now() + RESIDENT_SHUTDOWN_RETRY_TIMEOUT;
        loop {
            let reply = self.request(PublicRequest::Shutdown)?;
            match reply {
                PublicReply::ShuttingDown => return Ok(()),
                PublicReply::Error {
                    code: ErrorCode::InvalidRequest,
                    ref message,
                } if message == BUSY_LIFECYCLE_MESSAGE && Instant::now() < deadline => {
                    thread::sleep(
                        RESIDENT_SHUTDOWN_RETRY_INTERVAL
                            .min(deadline.saturating_duration_since(Instant::now())),
                    );
                }
                reply => return Err(unexpected_reply(reply)),
            }
        }
    }

    pub fn request(&self, body: PublicRequest) -> Result<PublicReply, DaemonError> {
        self.request_with_timeout(body, Duration::from_secs(5))
    }

    /// Guest shim: forward one stock `sbx` argv over this relay and relay
    /// stdio until the host reports the exit status.
    pub fn dev_sbx(
        &self,
        argv: Vec<String>,
        pty: Option<TerminalSize>,
        cwd: Option<PathBuf>,
    ) -> Result<i32, DaemonError> {
        let mut stream = UnixStream::connect(&self.paths.socket)?;
        write_frame(
            &mut stream,
            &Envelope {
                protocol: PROTOCOL.into(),
                token: self.token.clone(),
                body: PublicRequest::DevSbx { argv, pty, cwd },
            },
        )?;
        Ok(dev_broker::stream::run_client(stream))
    }

    /// One request whose reply may take up to `read_timeout` (a host
    /// publication runs a host CLI; `request` allows 5 s).
    ///
    /// # Errors
    /// Returns transport and framing errors, including a reply timeout.
    pub fn request_with_timeout(
        &self,
        body: PublicRequest,
        read_timeout: Duration,
    ) -> Result<PublicReply, DaemonError> {
        let mut stream = UnixStream::connect(&self.paths.socket)?;
        stream.set_read_timeout(Some(read_timeout))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        write_frame(
            &mut stream,
            &Envelope {
                protocol: PROTOCOL.into(),
                token: self.token.clone(),
                body,
            },
        )?;
        read_frame(&mut stream)
    }

    pub fn acp_start(
        &self,
        adapter: String,
        session: SessionSpec,
    ) -> Result<(String, String, UnixStream), DaemonError> {
        self.acp_start_reserved(adapter, session, None)
    }

    pub fn acp_reserve(
        &self,
        adapter: String,
        session: SessionSpec,
    ) -> Result<String, DaemonError> {
        match self.request(PublicRequest::AcpReserve { adapter, session })? {
            PublicReply::AcpReserved { agent_session_id } => Ok(agent_session_id),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn acp_start_reserved(
        &self,
        adapter: String,
        session: SessionSpec,
        reservation_id: Option<String>,
    ) -> Result<(String, String, UnixStream), DaemonError> {
        let mut stream = UnixStream::connect(&self.paths.socket)?;
        stream.set_read_timeout(Some(Duration::from_mins(4)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        write_frame(
            &mut stream,
            &Envelope {
                protocol: PROTOCOL.into(),
                token: self.token.clone(),
                body: PublicRequest::AcpStart {
                    adapter,
                    session,
                    reservation_id,
                },
            },
        )?;
        match read_frame(&mut stream)? {
            PublicReply::AcpStarted {
                agent_session_id,
                job_id,
            } => Ok((agent_session_id, job_id, stream)),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn acp_prompt(
        &self,
        agent_session_id: String,
        session: SessionSpec,
        text: String,
    ) -> Result<(), DaemonError> {
        self.acp_prompt_with_key(agent_session_id, session, Uuid::new_v4().to_string(), text)
            .map(|_| ())
    }

    pub fn acp_prompt_with_key(
        &self,
        agent_session_id: String,
        session: SessionSpec,
        operation_id: String,
        text: String,
    ) -> Result<String, DaemonError> {
        self.acp_prompt_classified(agent_session_id, session, operation_id, text)
            .map_err(PromptAdmissionError::into_error)
    }

    pub fn acp_cancel(
        &self,
        agent_session_id: String,
        session: SessionSpec,
    ) -> Result<String, DaemonError> {
        match self.request_with_timeout(
            PublicRequest::AcpCancel {
                agent_session_id,
                session,
            },
            Duration::from_secs(15),
        )? {
            PublicReply::AcpCancelled { phase } => Ok(phase),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn acp_respond(
        &self,
        agent_session_id: String,
        session: SessionSpec,
        request_id: String,
        option_id: String,
    ) -> Result<(), DaemonError> {
        match self.request(PublicRequest::AcpRespond {
            agent_session_id,
            session,
            request_id,
            option_id,
        })? {
            PublicReply::AcpAccepted => Ok(()),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn acp_status(
        &self,
        agent_session_id: String,
        session: SessionSpec,
        after: u64,
    ) -> Result<AcpSessionStatus, DaemonError> {
        match self.request(PublicRequest::AcpStatus {
            agent_session_id,
            session,
            after,
        })? {
            PublicReply::AcpStatus(status) => Ok(*status),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn acp_list(&self, session: SessionSpec) -> Result<Vec<AcpSessionSummary>, DaemonError> {
        match self.request(PublicRequest::AcpList { session })? {
            PublicReply::AcpSessions { sessions } => Ok(sessions),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn acp_attach(
        &self,
        agent_session_id: String,
        session: SessionSpec,
    ) -> Result<(), DaemonError> {
        match self.request(PublicRequest::AcpAttach {
            agent_session_id,
            session,
        })? {
            PublicReply::AcpAccepted => Ok(()),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn acp_release(
        &self,
        agent_session_id: String,
        session: SessionSpec,
    ) -> Result<(), DaemonError> {
        match self.request(PublicRequest::AcpRelease {
            agent_session_id,
            session,
        })? {
            PublicReply::AcpAccepted => Ok(()),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn acp_stop(
        &self,
        agent_session_id: String,
        session: SessionSpec,
    ) -> Result<(), DaemonError> {
        match self.request(PublicRequest::AcpStop {
            agent_session_id,
            session,
        })? {
            PublicReply::AcpAccepted => Ok(()),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn acp_published_prompt(
        &self,
        agent_session_id: String,
        generation: String,
        operation_id: String,
        text: String,
    ) -> Result<String, DaemonError> {
        match self.request(PublicRequest::AcpPublishedPrompt {
            agent_session_id,
            generation,
            operation_id,
            text,
        })? {
            PublicReply::AcpPromptAccepted { turn_id } => Ok(turn_id),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn acp_published_status(
        &self,
        agent_session_id: String,
        generation: String,
        after: u64,
    ) -> Result<AcpSessionStatus, DaemonError> {
        match self.request(PublicRequest::AcpPublishedStatus {
            agent_session_id,
            generation,
            after,
        })? {
            PublicReply::AcpStatus(status) => Ok(*status),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn acp_published_cancel(
        &self,
        agent_session_id: String,
        generation: String,
    ) -> Result<String, DaemonError> {
        match self.request(PublicRequest::AcpPublishedCancel {
            agent_session_id,
            generation,
        })? {
            PublicReply::AcpCancelled { phase } => Ok(phase),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn acp_published_respond(
        &self,
        agent_session_id: String,
        generation: String,
        request_id: String,
        option_id: String,
    ) -> Result<(), DaemonError> {
        match self.request(PublicRequest::AcpPublishedRespond {
            agent_session_id,
            generation,
            request_id,
            option_id,
        })? {
            PublicReply::AcpAccepted => Ok(()),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn acp_publish(
        &self,
        agent_session_id: String,
        session: SessionSpec,
        name: String,
        sandbox: Option<String>,
    ) -> Result<String, DaemonError> {
        self.acp_publish_target(agent_session_id, session, name, sandbox, None)
    }

    pub fn acp_publish_target(
        &self,
        agent_session_id: String,
        session: SessionSpec,
        name: String,
        sandbox: Option<String>,
        kit: Option<String>,
    ) -> Result<String, DaemonError> {
        // Publication preparation is backend-bounded, not capped at six
        // minutes by the caller. Use the same typed progress/outcome transport
        // as MCP; a lost reply after dispatch never proves cancellation.
        self.mcp_publication_request(PublicRequest::AcpPublish {
            agent_session_id,
            session,
            name,
            sandbox,
            kit,
        })
    }

    pub fn acp_unpublish(&self, session: SessionSpec, name: String) -> Result<String, DaemonError> {
        self.mcp_publication_request(PublicRequest::AcpUnpublish { session, name })
    }

    pub fn mcp_publish(
        &self,
        session: SessionSpec,
        name: String,
        description: Option<String>,
        sandbox: Option<String>,
        pipeline: String,
    ) -> Result<String, DaemonError> {
        self.mcp_publish_target(session, name, description, sandbox, pipeline, None)
    }

    pub fn mcp_publish_target(
        &self,
        session: SessionSpec,
        name: String,
        description: Option<String>,
        sandbox: Option<String>,
        pipeline: String,
        kit: Option<String>,
    ) -> Result<String, DaemonError> {
        self.mcp_publication_request(PublicRequest::McpPublish {
            session,
            name,
            description,
            sandbox,
            pipeline,
            kit,
        })
    }

    fn mcp_publication_request(&self, request: PublicRequest) -> Result<String, DaemonError> {
        let acp = matches!(
            &request,
            PublicRequest::AcpPublish { .. } | PublicRequest::AcpUnpublish { .. }
        );
        let rejected = |error: DaemonError| {
            DaemonError::Publication(PublicationOutcome::rejected(error.to_string()))
        };
        let mut stream =
            UnixStream::connect(&self.paths.socket).map_err(|error| rejected(error.into()))?;
        // Backend owns cold preparation deadlines. A lost reply after complete
        // dispatch is not a rejection and must never imply remote cancellation.
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| rejected(error.into()))?;
        stream
            .set_read_timeout(Some(PUBLICATION_REPLY_IDLE_TIMEOUT))
            .map_err(|error| rejected(error.into()))?;
        write_frame(
            &mut stream,
            &Envelope {
                protocol: PROTOCOL.into(),
                token: self.token.clone(),
                body: request,
            },
        )
        .map_err(rejected)?;
        loop {
            let reply = read_frame(&mut stream).map_err(|error| DaemonError::Publication(
                PublicationOutcome::uncertain(format!("publication reply lost after dispatch: {error}; inspect state before retrying (not cancelled)"))))?;
            let outcome = match reply {
                PublicReply::AcpColdBoot { kit } => {
                    eprintln!("[starting {kit} worker VM…]");
                    continue;
                }
                PublicReply::AcpPublication { outcome } if acp => outcome,
                PublicReply::McpPublication { outcome } if !acp => outcome,
                PublicReply::Error {
                    code: ErrorCode::Unauthorized | ErrorCode::InvalidRequest | ErrorCode::NotFound,
                    message,
                } => {
                    return Err(DaemonError::Publication(PublicationOutcome::rejected(
                        message,
                    )));
                }
                _ => {
                    return Err(DaemonError::Publication(PublicationOutcome::uncertain(
                        "unexpected publication reply after dispatch; inspect state before retrying",
                    )));
                }
            };
            return outcome
                .into_commit()
                .map(|commit| commit.message)
                .map_err(DaemonError::Publication);
        }
    }

    pub fn mcp_load(
        &self,
        session: SessionSpec,
        name: String,
        kit: Option<String>,
        sandbox: Option<String>,
    ) -> Result<String, DaemonError> {
        self.mcp_publication_request(PublicRequest::McpLoad {
            session,
            name,
            kit,
            sandbox,
        })
    }

    pub fn mcp_unpublish(&self, session: SessionSpec, name: String) -> Result<String, DaemonError> {
        self.mcp_publication_request(PublicRequest::McpUnpublish { session, name })
    }

    pub fn registered_commands(&self) -> Result<Vec<String>, DaemonError> {
        match self.request(PublicRequest::RegisteredCommands)? {
            PublicReply::RegisteredCommands { commands } => Ok(commands),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn registered_kits(&self) -> Result<BTreeMap<String, String>, DaemonError> {
        match self.request(PublicRequest::RegisteredKits)? {
            PublicReply::RegisteredKits { kits } => Ok(kits),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn reset_workers(&self, selection: LoadSelection) -> Result<Vec<String>, DaemonError> {
        let mut stream = UnixStream::connect(&self.paths.socket)?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        write_frame(
            &mut stream,
            &Envelope {
                protocol: PROTOCOL.into(),
                token: self.token.clone(),
                body: PublicRequest::ResetWorkers { selection },
            },
        )?;
        // VM removal is bounded by the stock-SBX adapter's lifecycle timeout,
        // not the five-second control-message deadline.
        stream.set_read_timeout(None)?;
        match read_frame(&mut stream)? {
            PublicReply::WorkersReset { kits } => Ok(kits),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn reset_scope(&self) -> Result<ScopeLifecycleReport, DaemonError> {
        self.scope_lifecycle(PublicRequest::ResetScope)
    }

    pub fn stop_scope(&self) -> Result<ScopeLifecycleReport, DaemonError> {
        let mut report = self.scope_lifecycle(PublicRequest::StopScope)?;
        if report.cleanup_complete && wait_for_owner_release(&self.paths).is_err() {
            report.cleanup_complete = false;
            report.components.push(ScopeCleanupComponent {
                kind: "scope".into(),
                label: "daemon".into(),
                state: ScopeCleanupState::CleanupUncertain,
                vm: None,
                detail: Some("daemon termination could not be verified".into()),
            });
        }
        Ok(report)
    }

    fn scope_lifecycle(&self, request: PublicRequest) -> Result<ScopeLifecycleReport, DaemonError> {
        self.scope_lifecycle_with_timeout(request, SCOPE_LIFECYCLE_REPLY_TIMEOUT)
    }

    fn scope_lifecycle_with_timeout(
        &self,
        request: PublicRequest,
        timeout: Duration,
    ) -> Result<ScopeLifecycleReport, DaemonError> {
        let mut stream = UnixStream::connect(&self.paths.socket)?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        write_frame(
            &mut stream,
            &Envelope {
                protocol: PROTOCOL.into(),
                token: self.token.clone(),
                body: request,
            },
        )?;
        // A socket read timeout is an idle timeout and can be defeated by a
        // trickling peer. Transfer the stream to one reader and impose an
        // absolute deadline over the complete framed reply instead.
        let interrupt = stream.try_clone()?;
        let (send, receive) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let reply = read_frame(&mut stream);
            let _ = send.send(reply);
        });
        let reply = match receive.recv_timeout(timeout) {
            Ok(reply) => reply?,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _ = interrupt.shutdown(Shutdown::Both);
                return Err(DaemonError::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("scope lifecycle reply timed out after {timeout:?}"),
                )));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(DaemonError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "scope lifecycle reply reader stopped",
                )));
            }
        };
        match reply {
            PublicReply::ScopeLifecycle(report) => Ok(report),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn prepare(
        &self,
        selection: LoadSelection,
        session: SessionSpec,
    ) -> Result<PreparationResult, DaemonError> {
        self.prepare_with_progress(selection, session, false, io::stderr())
    }

    /// `prepare` for a `--dev` session (`dev`: warm the dev shell VM).
    pub fn prepare_with_progress(
        &self,
        selection: LoadSelection,
        session: SessionSpec,
        dev: bool,
        mut progress_output: impl Write,
    ) -> Result<PreparationResult, DaemonError> {
        let mut stream = UnixStream::connect(&self.paths.socket)?;
        write_frame(
            &mut stream,
            &Envelope {
                protocol: PROTOCOL.into(),
                token: self.token.clone(),
                body: PublicRequest::Prepare {
                    selection,
                    session,
                    dev,
                },
            },
        )?;
        match read_frame::<PublicReply>(&mut stream)? {
            PublicReply::PreparationAccepted => Ok(()),
            reply => Err(unexpected_reply(reply)),
        }?;
        loop {
            match read_frame::<PreparationFrame>(&mut stream)? {
                PreparationFrame::ColdBoot { kit } => {
                    writeln!(progress_output, "[starting {kit} worker VM…]")?;
                    progress_output.flush()?;
                }
                PreparationFrame::Complete { result } => return Ok(result),
                PreparationFrame::Failed { message } => return Err(DaemonError::Remote(message)),
            }
        }
    }

    pub fn status(&self, session_id: Option<String>) -> Result<StatusDocument, DaemonError> {
        match self.request(PublicRequest::Status { session_id })? {
            PublicReply::Status(status) => Ok(status),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn process_view(&self, session: SessionSpec) -> Result<ProcessViewDocument, DaemonError> {
        match self.request(PublicRequest::ProcessView { session })? {
            PublicReply::ProcessView(view) => Ok(view),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn jobs(&self) -> Result<JobsDocument, DaemonError> {
        match self.request(PublicRequest::Jobs)? {
            PublicReply::Jobs(jobs) => Ok(jobs),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn job(&self, job_id: String) -> Result<JobReceipt, DaemonError> {
        match self.request(PublicRequest::ShowJob { job_id })? {
            PublicReply::Job(job) => Ok(*job),
            reply => Err(unexpected_reply(reply)),
        }
    }

    pub fn execute_with_io<R, O, E>(
        &self,
        spec: ExecuteSpec,
        input: R,
        output: O,
        error: E,
    ) -> Result<i32, DaemonError>
    where
        R: Read + Send + 'static,
        O: Write,
        E: Write,
    {
        let terminal = spec.session.terminal;
        let execution = self.start_execution(spec)?;
        relay_attachment(&execution, terminal, input, output, error).map_err(lost_after_accept)
    }

    pub fn execute(&self, mut spec: ExecuteSpec) -> Result<i32, DaemonError> {
        let terminal = spec.session.terminal;
        let terminal_mode = HostTerminalMode::enter_if(terminal)?;
        capture_initial_terminal_size(&mut spec.session)?;
        let execution = self.start_execution(spec)?;
        let result = relay_attachment(
            &execution,
            terminal,
            io::stdin(),
            io::stdout(),
            io::stderr(),
        );
        drop(terminal_mode);
        result.map_err(lost_after_accept)
    }

    pub fn start_execution(&self, spec: ExecuteSpec) -> Result<ClientExecution, DaemonError> {
        self.start_attached(PublicRequest::Execute(spec))
    }

    /// `ProcessRun` over a job's `cap.sock`, relayed like `Execute`.
    pub fn run_process_with_io<R, O, E>(
        &self,
        spec: ExecuteSpec,
        input: R,
        output: O,
        error: E,
    ) -> Result<i32, DaemonError>
    where
        R: Read + Send + 'static,
        O: Write,
        E: Write,
    {
        let execution = self.start_attached(PublicRequest::ProcessRun(spec))?;
        relay_attachment(&execution, false, input, output, error).map_err(lost_after_accept)
    }

    fn start_attached(&self, body: PublicRequest) -> Result<ClientExecution, DaemonError> {
        let mut stream = UnixStream::connect(&self.paths.socket)?;
        write_frame(
            &mut stream,
            &Envelope {
                protocol: PROTOCOL.into(),
                token: self.token.clone(),
                body,
            },
        )?;
        match read_frame::<PublicReply>(&mut stream)? {
            PublicReply::ExecutionAccepted => ClientExecution::new(stream),
            reply => Err(unexpected_reply(reply)),
        }
    }

    /// The owned input descriptor is polled and its worker joined; opaque
    /// blocking Read implementations cannot be abandoned on shell teardown.
    pub fn open_shell_with_io<O: Write, E: Write>(
        &self,
        spec: ShellSpec,
        input: File,
        output: O,
        mut error: E,
    ) -> Result<i32, DaemonError> {
        let terminal = spec.session.terminal;
        let attachment = self.start_shell(spec)?;
        shell_client::wait_ready(&attachment, &mut error)?;
        shell_client::relay_io(&attachment, terminal, input, output, error)
    }

    pub fn open_shell(&self, spec: ShellSpec) -> Result<i32, DaemonError> {
        shell_client::run(self, spec)
    }

    /// Send one request and hand back the stream for a streamed exchange
    /// (`SplitCreate`, `SplitJoin`).
    pub fn open_request(&self, request: PublicRequest) -> Result<UnixStream, DaemonError> {
        let mut stream = UnixStream::connect(&self.paths.socket)?;
        write_frame(
            &mut stream,
            &Envelope {
                protocol: PROTOCOL.into(),
                token: self.token.clone(),
                body: request,
            },
        )?;
        Ok(stream)
    }

    pub fn start_shell(&self, spec: ShellSpec) -> Result<ClientExecution, DaemonError> {
        let mut stream = UnixStream::connect(&self.paths.socket)?;
        write_frame(
            &mut stream,
            &Envelope {
                protocol: PROTOCOL.into(),
                token: self.token.clone(),
                body: PublicRequest::OpenShell(spec),
            },
        )?;
        match read_frame::<PublicReply>(&mut stream)? {
            PublicReply::ShellAccepted => ClientExecution::new_shell(stream),
            reply => Err(unexpected_reply(reply)),
        }
    }
}

fn verified_legacy_owner(
    client: &Client,
    paths: &EndpointPaths,
    daemon_id: &str,
) -> Result<rustix::process::Pid, DaemonError> {
    let status = match client.request(PublicRequest::Status { session_id: None })? {
        PublicReply::Status(status) => status,
        reply => return Err(unexpected_reply(reply)),
    };
    let jobs = match client.request(PublicRequest::Jobs)? {
        PublicReply::Jobs(jobs) => jobs,
        reply => return Err(unexpected_reply(reply)),
    };
    if status.daemon_id != daemon_id
        || status.endpoint_owner.uid != rustix::process::getuid().as_raw()
        || status
            .shells
            .iter()
            .any(|shell| shell.state == ShellState::Attached)
        || jobs
            .jobs
            .iter()
            .any(|job| matches!(job.state, JobState::Queued | JobState::Running))
    {
        return Err(DaemonError::EndpointInconsistent(
            paths.runtime_directory.clone(),
        ));
    }
    verify_endpoint_entry(&paths.lock, EndpointKind::Regular)?;
    let recorded = fs::read_to_string(&paths.lock)?.trim().parse::<u32>().ok();
    if recorded != Some(status.endpoint_owner.pid) {
        return Err(DaemonError::EndpointInconsistent(
            paths.runtime_directory.clone(),
        ));
    }
    let pid = i32::try_from(status.endpoint_owner.pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| DaemonError::EndpointInconsistent(paths.runtime_directory.clone()))?;
    rustix::process::test_kill_process(pid).map_err(|error| DaemonError::Io(error.into()))?;
    Ok(pid)
}

fn wait_for_owner_release(paths: &EndpointPaths) -> Result<(), DaemonError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !paths.lock.exists() || reclaim_stale_endpoint(paths)? == ReclaimOutcome::Reclaimed {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(DaemonError::EndpointInconsistent(
        paths.runtime_directory.clone(),
    ))
}

enum InputPumpFailure {
    Fatal(String),
    Closed,
}

fn attachment_input_closed(error: &DaemonError) -> bool {
    matches!(
        error,
        DaemonError::ShellStdinClosed
            | DaemonError::ShellInputTransportClosed(_)
            | DaemonError::ShellInputWriteFailed(_)
    ) || matches!(error, DaemonError::Io(error) if matches!(error.kind(),
            io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset | io::ErrorKind::NotConnected))
}

#[allow(
    clippy::too_many_lines,
    reason = "one attachment owns its input, signal and output lifecycle"
)]
fn relay_attachment<R: Read + Send + 'static, O: Write, E: Write>(
    execution: &ClientExecution,
    terminal: bool,
    mut input: R,
    mut output: O,
    mut error: E,
) -> Result<i32, DaemonError> {
    let (input_failure_send, input_failure_receive) = mpsc::channel();
    let signal_relay = SignalRelay::start(execution.clone(), terminal, input_failure_send.clone())?;
    let input_execution = execution.clone();
    thread::spawn(move || {
        let mut buffer = [0_u8; SHELL_STDIN_CHUNK];
        let mut forwarded = 0_usize;
        let fail = |reason: String| {
            let _ = input_failure_send.send(InputPumpFailure::Fatal(reason));
            let _ = input_execution.shutdown();
        };
        loop {
            if input_execution.stdin_closed() {
                break;
            }
            match input.read(&mut buffer) {
                Ok(0) => {
                    if let Err(error) = input_execution.send(&AttachmentFrame::StdinEof)
                        && !input_execution.terminal_received()
                    {
                        fail(format!(
                            "attachment stdin EOF failed after forwarding {forwarded} bytes: {error}"
                        ));
                    }
                    break;
                }
                Ok(count) => {
                    if let Err(error) = input_execution.send(&AttachmentFrame::Stdin {
                        bytes: buffer[..count].to_vec(),
                    }) {
                        if !input_execution.terminal_received() {
                            if attachment_input_closed(&error) {
                                // The guest may close stdin as it exits. Keep reading
                                // until its terminal frame decides the outcome.
                                let _ = input_failure_send.send(InputPumpFailure::Closed);
                            } else {
                                fail(format!(
                                    "attachment stdin send failed after forwarding {forwarded} bytes: {error}"
                                ));
                            }
                        }
                        break;
                    }
                    forwarded = forwarded.saturating_add(count);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    if !input_execution.terminal_received() {
                        fail(format!(
                            "attachment stdin read failed after forwarding {forwarded} bytes: {error}"
                        ));
                    }
                    break;
                }
            }
        }
    });
    loop {
        let received = execution.receive();
        let frame = match received {
            Ok(frame) => frame,
            Err(transport_error) => {
                if let Some(reason) = pending_input_fatal(&input_failure_receive) {
                    return report_input_pump_failure(&mut error, reason);
                }
                return Err(transport_error);
            }
        };
        match frame {
            AttachmentFrame::Stdout { bytes } => {
                write_attachment_bytes(&mut output, &bytes)?;
            }
            AttachmentFrame::Stderr { bytes } => {
                write_attachment_bytes(&mut error, &bytes)?;
            }
            // A background job (not the terminal's foreground process group)
            // stays quiet: its notice would land over the prompt.
            AttachmentFrame::ColdBoot { .. } if background_of_terminal() => {}
            AttachmentFrame::ColdBoot { kit } => {
                if terminal {
                    write!(error, "[starting {kit} worker VM…]\r\n")?;
                } else {
                    writeln!(error, "[starting {kit} worker VM…]")?;
                }
                error.flush()?;
            }
            AttachmentFrame::JobStarted { .. } | AttachmentFrame::ShellReady => {}
            AttachmentFrame::ControlError {
                operation, message, ..
            } => {
                writeln!(error, "marsh: {operation}: {message}")?;
                error.flush()?;
            }
            AttachmentFrame::Exited { code } => {
                drop(signal_relay);
                if let Some(reason) = pending_input_fatal(&input_failure_receive) {
                    return report_input_pump_failure(&mut error, reason);
                }
                return Ok(code);
            }
            AttachmentFrame::Failed { message } => {
                drop(signal_relay);
                return Err(DaemonError::Remote(message));
            }
            _ => {
                return Err(DaemonError::InvalidState(
                    "daemon sent a client-only attachment frame".into(),
                ));
            }
        }
    }
}

/// Transport loss after `ExecutionAccepted`/`ShellAccepted` is job
/// uncertainty (`docs/design/processes.md` s8); earlier errors are not.
pub(crate) fn lost_after_accept(error: DaemonError) -> DaemonError {
    match error {
        DaemonError::Io(error) => DaemonError::JobUncertain(error.to_string()),
        other => other,
    }
}

fn pending_input_fatal(receiver: &mpsc::Receiver<InputPumpFailure>) -> Option<String> {
    let mut fatal = None;
    while let Ok(failure) = receiver.try_recv() {
        if let InputPumpFailure::Fatal(reason) = failure {
            fatal = Some(reason);
        }
    }
    fatal
}

fn report_input_pump_failure(error: &mut impl Write, reason: String) -> Result<i32, DaemonError> {
    writeln!(error, "marsh: {reason}")?;
    error.flush()?;
    Err(DaemonError::InvalidState(reason))
}

/// Whether this process's stderr is a terminal whose foreground process group
/// is not ours: a job started with `&` from an interactive shell.
fn background_of_terminal() -> bool {
    rustix::termios::tcgetpgrp(std::io::stderr())
        .is_ok_and(|foreground| foreground != rustix::process::getpgrp())
}

fn write_attachment_bytes(writer: &mut impl Write, bytes: &[u8]) -> Result<(), DaemonError> {
    writer.write_all(bytes)?;
    writer.flush()?;
    Ok(())
}

pub(crate) struct HostTerminalMode {
    terminal: OwnedFd,
    original: rustix::termios::Termios,
}

impl HostTerminalMode {
    fn enter_if(terminal: bool) -> io::Result<Option<Self>> {
        terminal.then(|| Self::enter(io::stdin())).transpose()
    }

    pub(crate) fn enter(terminal: impl AsFd) -> io::Result<Self> {
        let terminal = terminal.as_fd().try_clone_to_owned()?;
        let original = rustix::termios::tcgetattr(&terminal)?;
        let mut raw = original.clone();
        raw.make_raw();
        rustix::termios::tcsetattr(&terminal, rustix::termios::OptionalActions::Now, &raw)?;
        Ok(Self { terminal, original })
    }
}

impl Drop for HostTerminalMode {
    fn drop(&mut self) {
        let _ = rustix::termios::tcsetattr(
            &self.terminal,
            rustix::termios::OptionalActions::Now,
            &self.original,
        );
    }
}

fn capture_initial_terminal_size(session: &mut SessionSpec) -> Result<(), DaemonError> {
    capture_initial_terminal_size_from(session, io::stdin())
}

fn capture_initial_terminal_size_from(
    session: &mut SessionSpec,
    terminal: impl AsFd,
) -> Result<(), DaemonError> {
    session.terminal_size = if session.terminal {
        let size = rustix::termios::tcgetwinsize(terminal).map_err(io::Error::from)?;
        Some(if size.ws_row == 0 || size.ws_col == 0 {
            TerminalSize {
                rows: 24,
                columns: 80,
            }
        } else {
            TerminalSize {
                rows: size.ws_row,
                columns: size.ws_col,
            }
        })
    } else {
        None
    };
    Ok(())
}

struct SignalRelay {
    handle: SignalHandle,
    task: Option<thread::JoinHandle<()>>,
}

impl SignalRelay {
    fn start(
        execution: ClientExecution,
        terminal: bool,
        failure: mpsc::Sender<InputPumpFailure>,
    ) -> Result<Self, DaemonError> {
        let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGWINCH])?;
        let handle = signals.handle();
        let task = thread::spawn(move || {
            for signal in signals.forever() {
                let frame = match signal {
                    SIGINT => Some(AttachmentFrame::Signal {
                        signal: "interrupt".into(),
                    }),
                    SIGTERM => Some(AttachmentFrame::Signal {
                        signal: "terminate".into(),
                    }),
                    SIGHUP => Some(AttachmentFrame::Signal {
                        signal: "hangup".into(),
                    }),
                    SIGWINCH if terminal => rustix::termios::tcgetwinsize(io::stdin())
                        .ok()
                        .and_then(resize_frame),
                    _ => None,
                };
                if let Some(frame) = frame
                    && let Err(error) = execution.send(&frame)
                {
                    if !execution.terminal_received() {
                        if attachment_input_closed(&error) {
                            // Input/control may close just after Exited was
                            // queued. The output reader decides terminal truth.
                            let _ = failure.send(InputPumpFailure::Closed);
                        } else {
                            let _ = failure.send(InputPumpFailure::Fatal(format!(
                                "attachment control send failed: {error}"
                            )));
                            let _ = execution.shutdown();
                        }
                    }
                    break;
                }
            }
        });
        Ok(Self {
            handle,
            task: Some(task),
        })
    }
}

fn resize_frame(size: rustix::termios::Winsize) -> Option<AttachmentFrame> {
    (size.ws_row != 0 && size.ws_col != 0).then_some(AttachmentFrame::Resize {
        rows: size.ws_row,
        columns: size.ws_col,
    })
}

impl Drop for SignalRelay {
    fn drop(&mut self) {
        self.handle.close();
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

#[derive(Clone, Debug)]
pub struct ClientExecution {
    reader: Arc<Mutex<UnixStream>>,
    writer: Arc<Mutex<UnixStream>>,
    writer_shutdown: Arc<UnixStream>,
    stdin_window: Option<Arc<StdinWindow>>,
}

#[derive(Debug)]
struct StdinWindow {
    state: Mutex<StdinWindowState>,
    changed: Condvar,
}

#[derive(Debug)]
struct StdinWindowState {
    credits: usize,
    closed: bool,
    terminal_received: bool,
    transport_failure: Option<String>,
}

impl StdinWindow {
    fn new() -> Self {
        Self {
            state: Mutex::new(StdinWindowState {
                credits: SHELL_STDIN_WINDOW,
                closed: false,
                terminal_received: false,
                transport_failure: None,
            }),
            changed: Condvar::new(),
        }
    }

    fn acquire(&self) -> Result<(), DaemonError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.credits == 0 && !state.closed {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if state.closed {
            if let Some(reason) = &state.transport_failure {
                return Err(DaemonError::ShellInputTransportClosed(reason.clone()));
            }
            return Err(DaemonError::ShellStdinClosed);
        }
        state.credits -= 1;
        Ok(())
    }

    fn release(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.credits = state.credits.saturating_add(1).min(SHELL_STDIN_WINDOW);
        self.changed.notify_one();
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        self.changed.notify_all();
    }

    fn close_transport(&self, reason: String) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        state.transport_failure = Some(reason);
        self.changed.notify_all();
    }

    fn close_terminal(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        state.terminal_received = true;
        self.changed.notify_all();
    }

    fn terminal_received(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .terminal_received
    }
}

impl ClientExecution {
    fn stdin_closed(&self) -> bool {
        self.stdin_window.as_ref().is_some_and(|window| {
            window
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .closed
        })
    }

    fn new(stream: UnixStream) -> Result<Self, DaemonError> {
        Self::new_with_write_timeout(stream, ATTACHMENT_WRITE_TIMEOUT)
    }

    fn new_shell(stream: UnixStream) -> Result<Self, DaemonError> {
        // A fast peer may already have queued Exited and closed after ACK.
        // Do not configure socket timeouts after that boundary (Darwin can
        // reject those options with EINVAL). Credited sends have no healthy
        // deadline; independently observed loss/shutdown wakes the write.
        let writer = stream.try_clone()?;
        let writer_shutdown = Arc::new(writer.try_clone()?);
        Ok(Self {
            reader: Arc::new(Mutex::new(stream)),
            writer: Arc::new(Mutex::new(writer)),
            writer_shutdown,
            stdin_window: Some(Arc::new(StdinWindow::new())),
        })
    }

    fn new_with_write_timeout(stream: UnixStream, timeout: Duration) -> Result<Self, DaemonError> {
        let writer = stream.try_clone()?;
        // A fast peer may already have queued its terminal frame and closed
        // after ACK (a refused child, `new_shell`): the option is then
        // rejected with EINVAL, and writes fail at once without a deadline.
        // The queued frames are still read.
        if let Err(error) = writer.set_write_timeout(Some(timeout))
            && error.raw_os_error() != Some(rustix::io::Errno::INVAL.raw_os_error())
        {
            return Err(error.into());
        }
        let writer_shutdown = Arc::new(writer.try_clone()?);
        Ok(Self {
            reader: Arc::new(Mutex::new(stream)),
            writer: Arc::new(Mutex::new(writer)),
            writer_shutdown,
            stdin_window: None,
        })
    }

    pub fn receive(&self) -> Result<AttachmentFrame, DaemonError> {
        self.receive_using(read_frame)
    }

    fn receive_using(
        &self,
        mut next: impl FnMut(&mut UnixStream) -> Result<AttachmentFrame, DaemonError>,
    ) -> Result<AttachmentFrame, DaemonError> {
        loop {
            let frame = {
                let mut reader = self
                    .reader
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                next(&mut reader)
            };
            match (&self.stdin_window, &frame) {
                (Some(window), Ok(AttachmentFrame::StdinCredit)) => {
                    window.release();
                }
                (Some(window), Ok(AttachmentFrame::StdinClosed)) => {
                    window.close();
                }
                (
                    Some(window),
                    Ok(AttachmentFrame::Exited { .. } | AttachmentFrame::Failed { .. }),
                ) => {
                    window.close_terminal();
                    return frame;
                }
                (Some(window), Err(error)) => {
                    window.close_transport(error.to_string());
                    // EOF is independently observed loss, including half-close.
                    // Wake a partial stdin frame before any worker is joined.
                    let _ = self.writer_shutdown.shutdown(Shutdown::Both);
                    return frame;
                }
                _ => return frame,
            }
        }
    }

    pub fn send(&self, frame: &AttachmentFrame) -> Result<(), DaemonError> {
        if let (Some(window), AttachmentFrame::Stdin { bytes }) = (&self.stdin_window, frame) {
            for chunk in bytes.chunks(SHELL_STDIN_CHUNK) {
                window.acquire()?;
                self.send_frame(&AttachmentFrame::Stdin {
                    bytes: chunk.to_vec(),
                })?;
            }
            return Ok(());
        }
        self.send_frame(frame)
    }

    fn terminal_received(&self) -> bool {
        self.stdin_window
            .as_ref()
            .is_some_and(|window| window.terminal_received())
    }

    fn send_frame(&self, frame: &AttachmentFrame) -> Result<(), DaemonError> {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = if self.stdin_window.is_some() {
            write_frame(&mut AttachmentWriter(&mut writer, None), frame)
        } else {
            write_frame(&mut *writer, frame)
        };
        if let Err(error) = &result
            && let Some(window) = &self.stdin_window
        {
            window.close_transport(error.to_string());
            // Fence a partial input frame but keep queued output/terminal
            // readable. Only the independent receiver determines completion.
            let _ = self.writer_shutdown.shutdown(Shutdown::Write);
        }
        match result {
            Err(DaemonError::Io(error)) if self.stdin_window.is_some() => {
                Err(DaemonError::ShellInputWriteFailed(error))
            }
            result => result,
        }
    }

    pub fn shutdown(&self) -> io::Result<()> {
        if let Some(window) = &self.stdin_window {
            window.close();
        }
        self.writer_shutdown.shutdown(Shutdown::Both)
    }
}

fn ensure_home(home: &Path) -> Result<(), DaemonError> {
    if home.exists() {
        let _ = EndpointPaths::for_home(home)?;
        return Ok(());
    }
    let parent = home
        .parent()
        .ok_or_else(|| DaemonError::UnsafeHome(home.to_owned()))?;
    verify_owned_directory(parent)?;
    fs::create_dir(home)?;
    fs::set_permissions(home, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn unexpected_reply(reply: PublicReply) -> DaemonError {
    match reply {
        PublicReply::Error { message, .. } => DaemonError::Remote(message),
        other => DaemonError::InvalidState(format!("unexpected daemon reply: {other:?}")),
    }
}

fn verify_secret(path: &Path) -> Result<(), DaemonError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(DaemonError::UnsafeToken(path.to_path_buf()));
    }
    Ok(())
}

fn read_secret(path: &Path) -> Result<String, DaemonError> {
    verify_secret(path)?;
    let token = fs::read_to_string(path)?;
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DaemonError::UnsafeToken(path.to_path_buf()));
    }
    Ok(token)
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

pub fn write_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> Result<(), DaemonError> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(DaemonError::FrameTooLarge(bytes.len()));
    }
    let length = u32::try_from(bytes.len()).map_err(|_| DaemonError::FrameTooLarge(bytes.len()))?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T, DaemonError> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(DaemonError::FrameTooLarge(length));
    }
    let mut bytes = vec![0_u8; length];
    reader.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("daemon endpoint already exists: {0}")]
    EndpointExists(PathBuf),
    #[error("unsafe MARSH_HOME or runtime directory: {0}")]
    UnsafeHome(PathBuf),
    #[error("unsafe daemon token: {0}")]
    UnsafeToken(PathBuf),
    #[error("daemon object not found: {0}")]
    NotFound(String),
    #[error("invalid daemon state: {0}")]
    InvalidState(String),
    #[error("shell cleanup uncertain; authority retained and affected admissions fenced: {0}")]
    ShellCleanupUncertain(String),
    #[error(
        "shell attachment generation is active; close its controller and wait for cleanup: {0}"
    )]
    ShellAttachmentBusy(String),
    #[error("shell stdin closed")]
    ShellStdinClosed,
    #[error("shell attachment transport closed: {0}")]
    ShellInputTransportClosed(String),
    #[error("shell attachment input write failed; terminal observation still required: {0}")]
    ShellInputWriteFailed(io::Error),
    #[error("daemon backend is not configured")]
    BackendUnavailable,
    #[error("daemon endpoint has stale or inconsistent lifecycle state: {0}")]
    EndpointInconsistent(PathBuf),
    #[error("daemon rejected the request: {0}")]
    Remote(String),
    /// A process admission refusal: the message is shown as is.
    #[error("{0}")]
    Refused(String),
    /// The daemon went away after it accepted the job or shell: the end of
    /// that job (and its children's) is unknown and nothing replays it.
    #[error("job uncertain: lost the daemon ({0}); see `marsh jobs`")]
    JobUncertain(String),
    #[error("{0}")]
    Publication(PublicationOutcome),
    #[error("daemon frame is too large: {0} bytes")]
    FrameTooLarge(usize),
    #[error("failed to obtain daemon authentication randomness: {0}")]
    Random(String),
    #[error("failed to start the resident daemon: {0}")]
    Spawn(io::Error),
    #[error(transparent)]
    StartupFailed(StartupFailure),
    #[error("resident daemon did not become ready within 30 seconds: {0}")]
    StartupTimeout(PathBuf),
    #[error("daemon I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("daemon protocol failed: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
extern crate self as marsh_daemon;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod mcp_load_tests;
