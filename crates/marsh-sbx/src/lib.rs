//! Stock Docker Sandboxes lifecycle boundary for kit workers.
//!
//! This crate deliberately shells out only to the public `sbx` CLI. It neither
//! starts a private sandbox daemon nor depends on SBX implementation details.

use marsh_contracts::{JobMount, MountAccess, OciImage, TerminalSize};
use marsh_runtime::{Attachment, CommandOutput, CommandRunner, Invocation};
use marsh_worker::{WorkerRequest, WorkerResponse, read_frame, write_frame};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, RwLock, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};
use thiserror::Error;

mod ephemeral_home;
pub mod shell_supervisor;
mod shell_template_reference;
mod source_chain;
mod source_permissions;
mod stock_inventory;
mod vm_ownership;
pub use ephemeral_home::{EphemeralHomeLease, EphemeralHomeToken};
pub use shell_template_reference::ShellTemplateReference;
pub use stock_inventory::{StockInventory, StockInventoryVm};
pub use vm_ownership::{DevGrantRecord, VmPurpose, valid_vm_prefix, vm_prefix};
use vm_ownership::{VmClass, VmOwnership};
mod capped_command;
pub use capped_command::{STOCK_COMMAND_CLEANUP_TIMEOUT, run_stock_command_capped};

pub const KIT_VM_BASIS: &str = "native-stock-sbx-kit";
pub const WORKER_PATH: &str = "/usr/local/libexec/marsh-worker";
pub const RELAY_PATH: &str = "/usr/local/libexec/marsh-relay";
/// Exit status of [`SHELL_SETUP_SCRIPT`] for an existing guest user whose
/// numeric identity differs from the host user's.
const SHELL_USER_MISMATCH_EXIT: i32 = 86;
/// One root step: install trusted binaries, ensure the host-identical user,
/// its sudo policy and private Docker Engine group, the relay runtime base,
/// and finally the ownership marker.
const SHELL_SETUP_SCRIPT: &str = concat!(
    "set -eu; umask 022; install -d -m 0755 /usr/local/bin /usr/local/libexec; ",
    "install -m 0755 /tmp/marsh-shell.install /usr/local/bin/marsh; ",
    "if [ -n \"$MARSH_RELAY_INSTALL\" ]; then install -m 0755 \"$MARSH_RELAY_INSTALL\" /usr/local/libexec/marsh-relay; fi; ",
    "if id -u \"$MARSH_SHELL_USER\" >/dev/null 2>&1; then ",
    "test \"$(id -u \"$MARSH_SHELL_USER\")\" = \"$MARSH_RELAY_UID\" || exit 86; ",
    "test \"$(id -g \"$MARSH_SHELL_USER\")\" = \"$MARSH_RELAY_GID\" || exit 86; ",
    "else getent group \"$MARSH_RELAY_GID\" >/dev/null || groupadd -g \"$MARSH_RELAY_GID\" \"$MARSH_SHELL_USER\"; ",
    "useradd -u \"$MARSH_RELAY_UID\" -g \"$MARSH_RELAY_GID\" -d \"$MARSH_SHELL_HOME\" -s /usr/local/bin/marsh \"$MARSH_SHELL_USER\"; fi; ",
    "(entry=/etc/sudoers.d/marsh; temporary=/etc/sudoers.d/.marsh.$$; trap 'rm -f \"$temporary\"' EXIT; ",
    "install -d -o root -g root -m 0755 /etc/sudoers.d; ",
    "printf '%s ALL=(root) NOPASSWD: ALL\\n' \"$MARSH_SHELL_USER\" >\"$temporary\"; ",
    "/usr/sbin/visudo -cf \"$temporary\" >/dev/null; install -o root -g root -m 0440 \"$temporary\" \"$entry\"; ",
    "test \"$(stat --format=%u:%g:%a \"$entry\")\" = 0:0:440; ",
    "test \"$(cat \"$entry\")\" = \"$MARSH_SHELL_USER ALL=(root) NOPASSWD: ALL\"); ",
    "for source in /etc/apt/sources.list /etc/apt/sources.list.d/*.sources; do ",
    "test ! -f \"$source\" || sed -i 's#http://deb.debian.org/#https://deb.debian.org/#g' \"$source\"; done; ",
    "test -S /var/run/docker.sock; test \"$(stat -c %G /var/run/docker.sock)\" = docker; ",
    "getent group docker >/dev/null; usermod -aG docker \"$MARSH_SHELL_USER\"; ",
    "id -nG \"$MARSH_SHELL_USER\" | tr ' ' '\\n' | grep -qx docker; ",
    "umask 077; mkdir -p /var/lib/marsh; ",
    "install -d -m 0700 -o \"$MARSH_RELAY_UID\" -g \"$MARSH_RELAY_GID\" \"$MARSH_RELAY_RUNTIME\"; ",
    "test \"$(stat --format=%F:%u:%g:%a \"$MARSH_RELAY_RUNTIME\")\" = \"directory:$MARSH_RELAY_UID:$MARSH_RELAY_GID:700\"; ",
    "printf '%s\\n' \"$MARSH_WORKER_MARKER\" > /var/lib/marsh/worker.json",
);
pub const GRANT_ROOT: &str = "/run/marsh/grants";
const MINIMUM_SBX_VERSION: (u64, u64, u64) = (0, 45, 0);
const SBX_COMPATIBILITY_TIMEOUT: Duration = Duration::from_secs(10);
const KEEPALIVE_READY_TIMEOUT: Duration = Duration::from_secs(30);
const LOCAL_SCRATCH_STALE_AFTER: Duration = Duration::from_hours(1);
const PREPARATION_SBX_TIMEOUT: Duration = Duration::from_mins(5);
const PREPARATION_BUILD_TIMEOUT: Duration = Duration::from_mins(20);
const PREPARATION_LOAD_TIMEOUT: Duration = Duration::from_mins(10);
const PREPARATION_DOCKER_TIMEOUT: Duration = Duration::from_mins(2);
const SUPERVISOR_PING_TIMEOUT: Duration = Duration::from_secs(5);
// A supervisor frame this recent proves the transport live without a ping.
const SUPERVISOR_RECENT: Duration = Duration::from_secs(2);
const RETAINED_PROCESS_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const RETAINED_PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const RETAINED_RESPONSE_BUFFER_BYTES: usize = 4 * 1024 * 1024;
static NEXT_LOCAL_SCRATCH: AtomicU64 = AtomicU64::new(1);
static NEXT_SHELL_MOUNT: AtomicU64 = AtomicU64::new(1);
const SHELL_MOUNT_TIMEOUT: Duration = Duration::from_secs(15);
/// Bound on waiting for a concurrent grant transition while closing a shell.
const SHELL_CLOSE_TRANSITION_WAIT: Duration = Duration::from_secs(10);
#[derive(Clone, Copy, Debug)]
struct PreparationTimeouts {
    sbx: Duration,
    build: Duration,
    load: Duration,
    docker: Duration,
}

impl Default for PreparationTimeouts {
    fn default() -> Self {
        Self {
            sbx: PREPARATION_SBX_TIMEOUT,
            build: PREPARATION_BUILD_TIMEOUT,
            load: PREPARATION_LOAD_TIMEOUT,
            docker: PREPARATION_DOCKER_TIMEOUT,
        }
    }
}

/// Exact inputs needed to create or validate one reusable kit VM.
#[derive(Clone, Debug, PartialEq)]
pub struct KitVmSpec {
    pub name: String,
    pub worker_binary: PathBuf,
    pub workload_kit: NativeKitRef,
    /// Daemon-selected workspace for the native Kit lifecycle. Ordinary Kits
    /// use a neutral host directory; ephemeral Kits require their private HOME
    /// token. Host build/control artifacts never belong in that writable HOME.
    pub lifecycle_workspace: PathBuf,
}

/// A compatible stock-SBX VM ready to receive jobs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadyKitVm {
    pub name: String,
    pub kit_ref: String,
    pub lifecycle_workspace: PathBuf,
    pub cold_started: bool,
    pub job_image: OciImage,
    pub worker_binary: PathBuf,
}

impl ReadyKitVm {
    /// Exact nested-container image supplied by a published native workload.
    #[must_use]
    pub fn job_image(&self) -> &OciImage {
        &self.job_image
    }
}

/// A stock Docker sandbox kit reference with an immutable ownership identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeKitRef {
    identity: String,
    registry_identity: String,
    local_fingerprint: Option<String>,
    location: NativeKitLocation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum NativeKitLocation {
    ImmutableOci(OciImage),
    LocalV3Source(PathBuf),
}

impl NativeKitRef {
    /// Selects an immutable OCI-native kit reference.
    ///
    /// # Errors
    /// Rejects tags and malformed digests.
    pub fn immutable_oci(reference: impl Into<String>) -> Result<Self, SbxError> {
        let reference = reference.into();
        let image = OciImage::parse(reference.clone()).map_err(|_| SbxError::InvalidNativeKit)?;
        Ok(Self {
            identity: reference.clone(),
            registry_identity: reference,
            local_fingerprint: None,
            location: NativeKitLocation::ImmutableOci(image),
        })
    }

    /// Selects one canonical local Docker Sandbox Kit v3 source directory.
    ///
    /// The directory remains source input until it is prepared. Preparation
    /// resolves it through stock Docker and SBX into an immutable image ID.
    ///
    /// # Errors
    /// Rejects missing, relative, non-directory, symlinked, or non-canonical
    /// paths. Callers resolving paths relative to a registry file must do so
    /// before constructing this value.
    pub fn local_v3_source(source: PathBuf) -> Result<Self, SbxError> {
        let canonical = fs::canonicalize(&source).map_err(|error| SbxError::Metadata {
            path: source.clone(),
            source: error,
        })?;
        if source != canonical || !safe_absolute(&source) {
            return Err(SbxError::InvalidLocalKitSource(source));
        }
        let metadata = fs::symlink_metadata(&source).map_err(|error| SbxError::Metadata {
            path: source.clone(),
            source: error,
        })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(SbxError::InvalidLocalKitSource(source));
        }
        let identity = format!("local-v3:{}", source.display());
        Ok(Self {
            identity: identity.clone(),
            registry_identity: identity,
            local_fingerprint: None,
            location: NativeKitLocation::LocalV3Source(source),
        })
    }

    /// Captures one immutable preparation generation. Local source bytes are
    /// fingerprinted once; immutable OCI references are already generations.
    ///
    /// # Errors
    /// Returns an error when a local source cannot be read safely.
    pub fn capture_generation(&self) -> Result<Self, SbxError> {
        let NativeKitLocation::LocalV3Source(source) = &self.location else {
            return Ok(self.clone());
        };
        let fingerprint = local_source_fingerprint(source)?;
        let mut captured = self.clone();
        captured.identity = format!("{}@{fingerprint}", self.registry_identity);
        captured.local_fingerprint = Some(fingerprint);
        Ok(captured)
    }

    /// Content-sensitive generation identity used for preparation and VM
    /// ownership. Immutable references are returned unchanged.
    ///
    /// # Errors
    /// Returns an error when a local source cannot be read safely.
    pub fn generation_identity(&self) -> Result<String, SbxError> {
        Ok(self.capture_generation()?.identity)
    }

    fn expected_local_fingerprint(&self, source: &Path) -> Result<String, SbxError> {
        self.local_fingerprint
            .clone()
            .map_or_else(|| local_source_fingerprint(source), Ok)
    }

    fn validate_captured_source(&self, source: &Path) -> Result<String, SbxError> {
        let expected = self.expected_local_fingerprint(source)?;
        if expected != local_source_fingerprint(source)? {
            return Err(SbxError::SourceChanged(source.to_owned()));
        }
        Ok(expected)
    }

    /// OCI image used for each fresh nested job container, when already known.
    /// Local source kits acquire this immutable identity during preparation.
    #[must_use]
    pub fn workload_image(&self) -> Option<&OciImage> {
        match &self.location {
            NativeKitLocation::ImmutableOci(image) => Some(image),
            NativeKitLocation::LocalV3Source(_) => None,
        }
    }

    /// Canonical source directory for a local v3 kit.
    #[must_use]
    pub fn source_dir(&self) -> Option<&Path> {
        match &self.location {
            NativeKitLocation::ImmutableOci(_) => None,
            NativeKitLocation::LocalV3Source(source) => Some(source),
        }
    }

    /// Stable ownership identity used by the daemon registry and receipts.
    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }
}

/// Exact inputs for the persistent human shell VM.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellVmSpec {
    pub name: String,
    pub image: OciImage,
    pub shell_binary: PathBuf,
    pub user: ShellUser,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellUser {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadyShellVm {
    pub name: String,
    pub user: ShellUser,
    pub cold_started: bool,
}

/// Natural-path mounts retained while shell sessions use them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellMounts {
    vm: String,
    // Clones represent the SAME acquisition, not another reference. Releasing
    // twice or retrying a partial rollback must never release a sibling's pin.
    acquisition: u64,
    mounts: Vec<(PathBuf, PathBuf)>,
}

/// Host-side admission held across shell preparation, execution and cleanup.
/// This is independent of retained mount counts after an uncertain close.
pub struct ShellVmSession(Arc<AtomicUsize>);

impl Drop for ShellVmSession {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// One exact source with retained NOFOLLOW descriptors for every ancestor.
/// Ownership, POSIX/sticky permissions and native descriptor ACLs must exclude
/// untrusted pathname mutation; pins alone are not that proof. Overlap with this
/// daemon's other active grants is rejected before the first stock mount.
#[derive(Clone, Debug)]
pub struct AdmittedHostGrant {
    source: PathBuf,
    target: PathBuf,
    access: MountAccess,
    handle: Arc<fs::File>,
    identity: HostIdentity,
    chain: source_chain::SourceChain,
    ephemeral: Option<EphemeralHomeToken>,
}

impl AdmittedHostGrant {
    /// Opens and pins one exact host directory for a later stock-SBX mount.
    ///
    /// # Errors
    /// Rejects relative paths, symlink ancestors, non-directories, untrusted
    /// ancestor rename/security authority, and leaves not owned by the daemon user.
    pub fn open(source: PathBuf, target: PathBuf, access: MountAccess) -> Result<Self, SbxError> {
        if !safe_absolute(&source) {
            return Err(SbxError::UnsafePath(source));
        }
        if !safe_absolute(&target) {
            return Err(SbxError::UnsafePath(target));
        }
        // Stock's path-bearing CLI/daemon JSON is not a raw Unix-byte carrier.
        // Never let lossy Go JSON select a different existing replacement-name
        // directory. Filenames *inside* an admitted root remain ordinary data.
        for path in [&source, &target] {
            if path.to_str().is_none_or(|path| path.contains('\u{fffd}')) {
                return Err(SbxError::HostGrantFence("stock source/target root is not losslessly representable by public path metadata".into()));
            }
        }
        let chain = source_chain::SourceChain::open(&source)?;
        let handle = chain.leaf();
        let metadata = handle
            .metadata()
            .map_err(|source_error| SbxError::Metadata {
                path: source.clone(),
                source: source_error,
            })?;
        let identity = HostIdentity::from_metadata(&source, &metadata)?;
        Ok(Self {
            source,
            target,
            access,
            handle,
            identity,
            chain,
            ephemeral: None,
        })
    }

    /// Returns the device and inode captured from the retained source handle.
    /// This identity remains tied to admission if the pathname is replaced.
    #[must_use]
    pub const fn source_identity(&self) -> (u64, u64) {
        (self.identity.device, self.identity.inode)
    }

    fn verify_path(&self) -> Result<(), SbxError> {
        self.chain.verify(&self.source)?;
        if self.identity != HostIdentity::read(&self.source)? {
            return Err(SbxError::SourceChanged(self.source.clone()));
        }
        // Keep the descriptor observably live through every verification.
        let descriptor_metadata = self
            .handle
            .metadata()
            .map_err(|source| SbxError::Metadata {
                path: self.source.clone(),
                source,
            })?;
        if self.identity != HostIdentity::from_metadata(&self.source, &descriptor_metadata)? {
            return Err(SbxError::SourceChanged(self.source.clone()));
        }
        Ok(())
    }
}

/// Per-attempt authorization over opaque, source-refcounted SBX mounts.
/// Dropping this value does not release its references; callers must call
/// [`StockSbx::revoke_grants`] after worker cleanup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedGrants {
    vm: String,
    attempt: String,
    worker_generation: u64,
    mounts: Vec<PreparedMount>,
}

/// Exact session pins newly acquired by one preparation request. Successful
/// callers keep the pins by dropping this value; failed multi-Kit preparation
/// can use it to roll back only work acquired by that request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionGrantPins {
    vm: String,
    session: String,
    sources: Vec<PathBuf>,
}

impl PreparedGrants {
    #[must_use]
    pub fn job_mounts(&self) -> Vec<JobMount> {
        self.mounts
            .iter()
            .map(|mount| JobMount {
                source: mount.vm_source.clone(),
                target: mount.target.clone(),
                access: mount.access,
                subpath: None,
            })
            .collect()
    }

    #[must_use]
    pub fn attempt(&self) -> &str {
        &self.attempt
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PreparedMount {
    host_source: PathBuf,
    vm_source: PathBuf,
    target: PathBuf,
    access: MountAccess,
}

#[derive(Clone, Debug)]
struct GrantMountReference {
    identity: HostIdentity,
    source_record: source_chain::SourceRecord,
    access: MountAccess,
    vm_source: PathBuf,
    active_jobs: usize,
    pinned_sessions: BTreeMap<String, Arc<fs::File>>,
}

type GrantKey = (String, PathBuf);
type WeakTransitionLocks<K, L> = BTreeMap<K, Weak<L>>;

struct WeakTransitionCleanup<'a, K: Clone + Ord, L> {
    registry: &'a Mutex<WeakTransitionLocks<K, L>>,
    key: K,
    lock: Weak<L>,
}

impl<K: Clone + Ord, L> Drop for WeakTransitionCleanup<'_, K, L> {
    fn drop(&mut self) {
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if registry
            .get(&self.key)
            .is_some_and(|current| Weak::ptr_eq(current, &self.lock))
            && self.lock.strong_count() <= 1
        {
            registry.remove(&self.key);
        }
    }
}

fn weak_transition_lock<K: Clone + Ord, L>(
    registry: &Mutex<WeakTransitionLocks<K, L>>,
    key: K,
    create: impl FnOnce() -> L,
) -> (Arc<L>, WeakTransitionCleanup<'_, K, L>) {
    let lock = {
        let mut locks = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.retain(|_, lock| lock.strong_count() > 0);
        locks.get(&key).and_then(Weak::upgrade).unwrap_or_else(|| {
            let lock = Arc::new(create());
            locks.insert(key.clone(), Arc::downgrade(&lock));
            lock
        })
    };
    let cleanup = WeakTransitionCleanup {
        registry,
        key,
        lock: Arc::downgrade(&lock),
    };
    (lock, cleanup)
}

fn weak_mutex_transition_lock<K: Clone + Ord>(
    registry: &Mutex<WeakTransitionLocks<K, Mutex<()>>>,
    key: K,
) -> (Arc<Mutex<()>>, WeakTransitionCleanup<'_, K, Mutex<()>>) {
    weak_transition_lock(registry, key, || Mutex::new(()))
}

fn weak_rw_transition_lock<K: Clone + Ord>(
    registry: &Mutex<WeakTransitionLocks<K, RwLock<()>>>,
    key: K,
) -> (Arc<RwLock<()>>, WeakTransitionCleanup<'_, K, RwLock<()>>) {
    weak_transition_lock(registry, key, || RwLock::new(()))
}

/// Invocation for the trusted worker. Its framed transport carries control,
/// output, lifecycle, and terminal-report messages for concurrent attempts.
struct RetainedWorker {
    generation: u64,
    input: Mutex<Box<dyn Write + Send>>,
    process: Mutex<Box<dyn marsh_runtime::AttachedProcess>>,
    routes: Mutex<BTreeMap<String, AttemptRoute>>,
    ping_serial: Mutex<()>,
    pending_pong: Mutex<Option<PendingWorkerPong>>,
    next_ping: AtomicU64,
    alive: AtomicBool,
    active: AtomicUsize,
    /// Why the transport ended (reader EOF, a bad frame, an unexpected
    /// Ready, the worker's exit status and stderr tail); set once.
    loss: WorkerLoss,
}

/// Diagnostics for a retained worker transport that ended under live
/// attempts (`docs/design/processes.md`: transport loss quarantines the VM).
#[derive(Default)]
struct WorkerLoss {
    reason: Mutex<Option<String>>,
    stderr_tail: Mutex<std::collections::VecDeque<u8>>,
}

/// Bytes of worker stderr retained for a loss diagnostic.
const WORKER_STDERR_TAIL: usize = 2048;

impl WorkerLoss {
    fn record_stderr(&self, bytes: &[u8]) {
        let mut tail = self
            .stderr_tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tail.extend(bytes);
        let excess = tail.len().saturating_sub(WORKER_STDERR_TAIL);
        tail.drain(..excess);
    }

    fn set(&self, reason: String) {
        self.reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert(reason);
    }

    /// One printable line: the reason and the worker's last stderr bytes.
    fn describe(&self) -> Option<String> {
        let reason = self
            .reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()?;
        let tail = self
            .stderr_tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let tail = String::from_utf8_lossy(&tail)
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect::<String>();
        let tail = tail.split_whitespace().collect::<Vec<_>>().join(" ");
        Some(if tail.is_empty() {
            reason
        } else {
            let start = tail.len().saturating_sub(512);
            let start = (start..tail.len())
                .find(|index| tail.is_char_boundary(*index))
                .unwrap_or(tail.len());
            format!("{reason}; worker stderr: {}", &tail[start..])
        })
    }
}

struct PendingWorkerPong {
    nonce: u64,
    send: mpsc::SyncSender<u64>,
}

struct PendingWorkerPing<'a> {
    worker: &'a RetainedWorker,
    nonce: u64,
}

impl Drop for PendingWorkerPing<'_> {
    fn drop(&mut self) {
        let mut route = self
            .worker
            .pending_pong
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if route
            .as_ref()
            .is_some_and(|route| route.nonce == self.nonce)
        {
            *route = None;
        }
    }
}

struct AttemptRoute {
    send: mpsc::Sender<WorkerResponse>,
    queued_bytes: usize,
    byte_limit: usize,
    overflowing: bool,
}

impl AttemptRoute {
    fn admit(&mut self, response: &WorkerResponse) -> bool {
        let buffered = response_buffer_bytes(response);
        if buffered > 0
            && (self.overflowing || self.queued_bytes.saturating_add(buffered) > self.byte_limit)
        {
            self.overflowing = true;
            false
        } else {
            self.queued_bytes = self.queued_bytes.saturating_add(buffered);
            true
        }
    }
}

/// One attempt routed through a retained worker transport.
pub struct WorkerChannel {
    attempt: String,
    worker: Arc<RetainedWorker>,
    pub responses: WorkerResponses,
    finished: bool,
}

pub struct WorkerResponses {
    attempt: String,
    worker: Arc<RetainedWorker>,
    receive: mpsc::Receiver<WorkerResponse>,
}

impl WorkerResponses {
    /// Receives the next response and releases its accounted buffer bytes.
    ///
    /// # Errors
    /// Returns an error when the retained transport route closes.
    pub fn recv(&self) -> Result<WorkerResponse, mpsc::RecvError> {
        let response = self.receive.recv()?;
        self.release_buffered_bytes(&response);
        Ok(response)
    }

    /// Receives the next response before the supplied deadline and releases
    /// its accounted buffer bytes.
    ///
    /// # Errors
    /// Returns an error when the deadline expires or the route closes.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<WorkerResponse, mpsc::RecvTimeoutError> {
        let response = self.receive.recv_timeout(timeout)?;
        self.release_buffered_bytes(&response);
        Ok(response)
    }

    /// Why the retained transport ended, once its reader has stopped.
    #[must_use]
    pub fn loss_reason(&self) -> Option<String> {
        self.worker.loss.describe()
    }

    #[must_use]
    pub fn overflowed(&self) -> bool {
        self.worker
            .routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&self.attempt)
            .is_some_and(|route| route.overflowing)
    }

    fn release_buffered_bytes(&self, response: &WorkerResponse) {
        let bytes = response_buffer_bytes(response);
        if bytes == 0 {
            return;
        }
        if let Some(route) = self
            .worker
            .routes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&self.attempt)
        {
            route.queued_bytes = route.queued_bytes.saturating_sub(bytes);
        }
    }
}

fn response_buffer_bytes(response: &WorkerResponse) -> usize {
    match response {
        WorkerResponse::Stdout { bytes, .. } | WorkerResponse::Stderr { bytes, .. } => bytes.len(),
        _ => 0,
    }
}

#[derive(Clone)]
pub struct WorkerControlHandle {
    attempt: String,
    worker: Arc<RetainedWorker>,
}

impl WorkerControlHandle {
    /// Sends one bounded attempt-scoped request.
    ///
    /// # Errors
    /// Returns an I/O error when the retained transport is unavailable.
    pub fn send(&self, request: &WorkerRequest) -> Result<(), SbxError> {
        write_frame(
            &mut *self
                .worker
                .input
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            request,
        )
        .map_err(|error| SbxError::Io(io::Error::other(error)))
    }

    /// Sends one request without allowing a wedged retained-transport write to
    /// hold lifecycle cleanup indefinitely.
    ///
    /// # Errors
    /// Returns an error when the write fails or does not complete by the
    /// supplied deadline.
    pub fn send_bounded(&self, request: &WorkerRequest, timeout: Duration) -> Result<(), SbxError> {
        let control = self.clone();
        let request = request.clone();
        let (send, receive) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ = send.send(control.send(&request));
        });
        receive.recv_timeout(timeout).map_err(|_| {
            SbxError::WorkerLeaseLost(format!(
                "worker control write timed out for {}",
                self.attempt
            ))
        })?
    }
}

impl WorkerChannel {
    #[must_use]
    pub fn control(&self) -> WorkerControlHandle {
        WorkerControlHandle {
            attempt: self.attempt.clone(),
            worker: Arc::clone(&self.worker),
        }
    }
    /// Marks a terminal response consumed and removes the attempt route.
    pub fn finish(&mut self) {
        if !self.finished {
            self.worker
                .routes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&self.attempt);
            self.worker.active.fetch_sub(1, Ordering::AcqRel);
            self.finished = true;
        }
    }
}

impl Drop for WorkerChannel {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Long-lived trusted relay transport. Its stdin/stdout carry only the relay
/// protocol; neither the delegated token nor control socket is placed in the
/// selected home.
pub struct RelayLaunch {
    pub process: Attachment,
    pub socket_path: PathBuf,
    pub token_path: PathBuf,
}

/// Minimal public-SBX adapter. The daemon remains responsible for admission,
/// placement, concurrency, persistence, and user-facing status.
pub struct StockSbx {
    sbx: PathBuf,
    runner: Arc<dyn CommandRunner>,
    preparation_timeouts: PreparationTimeouts,
    shell_init_locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    ready_shells: Mutex<BTreeMap<String, ShellInitIdentity>>,
    installed_shell_artifacts: Mutex<BTreeMap<(String, String), String>>,
    shell_mount_references: Mutex<BTreeMap<(String, PathBuf, PathBuf), Arc<ShellMountReference>>>,
    ownership: VmOwnership,
    shell_recovery_specs: Mutex<BTreeMap<String, ShellVmSpec>>,
    active_shell_sessions: Mutex<BTreeMap<String, Arc<AtomicUsize>>>,
    // Quarantine retains admitted directory handles and mount references until
    // explicit, verified scope cleanup. Never stop a shared VM on client loss.
    uncertain_shell_grants: Mutex<BTreeMap<String, Vec<AdmittedHostGrant>>>,
    grant_mount_references: Mutex<BTreeMap<(String, PathBuf), GrantMountReference>>,
    session_grant_lifecycles: Mutex<BTreeMap<String, Arc<Mutex<bool>>>>,
    grant_transition_locks: Mutex<WeakTransitionLocks<GrantKey, Mutex<()>>>,
    grant_vm_transition_locks: Mutex<WeakTransitionLocks<String, RwLock<()>>>,
    grant_stock_operation_locks: Mutex<WeakTransitionLocks<String, Mutex<()>>>,
    worker_init_locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    installed_worker_artifacts: Mutex<BTreeMap<String, String>>,
    /// Per-user verified local Kit image archives keyed by source
    /// fingerprint (daemon opt-in; `make dev` seeds it).
    kit_image_cache: Option<PathBuf>,
    /// Whether published (digest) Kit images use the cache as well.
    published_image_cache: bool,
    /// Published images whose archive is being saved into the cache.
    published_image_saves: Arc<Mutex<BTreeSet<String>>>,
    worker_artifact_identities: Mutex<BTreeMap<PathBuf, String>>,
    worker_generations: Mutex<BTreeMap<String, u64>>,
    quarantined_workers: Mutex<BTreeSet<String>>,
    worker_leases: Mutex<BTreeMap<String, Arc<RetainedWorker>>>,
    shell_leases: Mutex<BTreeMap<String, Arc<shell_supervisor::Supervisor>>>,
    shell_generation: AtomicU64,
    local_resolution_locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    local_resolutions: Mutex<BTreeMap<String, LocalResolution>>,
    retained_process_teardown_timeout: Duration,
    ephemeral_root: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LocalResolution {
    source_fingerprint: String,
    image_id: OciImage,
    local_tag: String,
}

/// Why one cold local Kit preparation failed: a host build that disagrees
/// with stock SBX's Kit VM image is recoverable by rebuilding.
enum ColdLocalFailure {
    HostBuildMismatch { error: SbxError },
    Other(SbxError),
}

impl From<SbxError> for ColdLocalFailure {
    fn from(error: SbxError) -> Self {
        Self::Other(error)
    }
}

#[derive(Debug)]
struct LocalBuild {
    source_fingerprint: String,
    local_tag: String,
    manifest_digest: OciImage,
    docker_archive: PathBuf,
    metadata: PathBuf,
    /// The archive is this daemon's verified per-Kit image cache entry:
    /// consuming it must not delete it.
    retained: bool,
    // Ephemeral HOME is guest-writable. Build output lives instead in an
    // unexported private sibling, retained until the archive is consumed.
    _private_scratch: Option<tempfile::TempDir>,
}

#[derive(Deserialize)]
struct PublicKitInspect {
    image_digest: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ShellInitIdentity {
    marker: String,
    shell_artifact: String,
    user: ShellUser,
}

#[derive(Debug)]
struct ShellMountReference {
    // Dormancy keeps complete identity metadata, not one FD chain per historical
    // project. Durable source/UUID authority remains in the native ledger;
    // successful umount or dropping these local pins does not discharge it.
    source_record: source_chain::SourceRecord,
    access: MountAccess,
    uuid: String,
    state: Mutex<ShellMountState>,
    changed: Condvar,
}

#[derive(Debug)]
struct ShellMountState {
    phase: ShellMountPhase,
    owners: BTreeSet<u64>,
    // Live publication, mounted users and uncertain cleanup retain exact pins.
    pin: Option<AdmittedHostGrant>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShellMountPhase {
    Dormant,
    Mounting,
    Mounted,
    Unmounting,
    Uncertain,
}

impl StockSbx {
    #[must_use]
    pub fn new(sbx: impl Into<PathBuf>, runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            sbx: sbx.into(),
            runner,
            preparation_timeouts: PreparationTimeouts::default(),
            shell_init_locks: Mutex::new(BTreeMap::new()),
            ready_shells: Mutex::new(BTreeMap::new()),
            installed_shell_artifacts: Mutex::new(BTreeMap::new()),
            shell_mount_references: Mutex::new(BTreeMap::new()),
            ownership: VmOwnership::in_memory(),
            shell_recovery_specs: Mutex::new(BTreeMap::new()),
            active_shell_sessions: Mutex::new(BTreeMap::new()),
            uncertain_shell_grants: Mutex::new(BTreeMap::new()),
            grant_mount_references: Mutex::new(BTreeMap::new()),
            session_grant_lifecycles: Mutex::new(BTreeMap::new()),
            grant_transition_locks: Mutex::new(BTreeMap::new()),
            grant_vm_transition_locks: Mutex::new(BTreeMap::new()),
            grant_stock_operation_locks: Mutex::new(BTreeMap::new()),
            worker_init_locks: Mutex::new(BTreeMap::new()),
            installed_worker_artifacts: Mutex::new(BTreeMap::new()),
            kit_image_cache: None,
            published_image_cache: true,
            published_image_saves: Arc::new(Mutex::new(BTreeSet::new())),
            worker_artifact_identities: Mutex::new(BTreeMap::new()),
            worker_generations: Mutex::new(BTreeMap::new()),
            quarantined_workers: Mutex::new(BTreeSet::new()),
            worker_leases: Mutex::new(BTreeMap::new()),
            shell_leases: Mutex::new(BTreeMap::new()),
            shell_generation: AtomicU64::new(1),
            local_resolution_locks: Mutex::new(BTreeMap::new()),
            local_resolutions: Mutex::new(BTreeMap::new()),
            retained_process_teardown_timeout: RETAINED_PROCESS_TEARDOWN_TIMEOUT,
            ephemeral_root: ephemeral_home::default_root(),
        }
    }

    /// Persist this daemon's VM ownership map at `path` (owner-only control dir).
    ///
    /// # Errors
    /// Rejects an unreadable, foreign-owned or malformed map.
    pub fn with_vm_ownership(mut self, path: PathBuf) -> Result<Self, SbxError> {
        self.ownership = VmOwnership::persisted(path)?;
        Ok(self)
    }

    /// The owned VM name for `(purpose, key)`. The ownership map is the only
    /// source of names: an unknown pair gets a fresh random name
    /// (`marsh-k-<id>` / `marsh-s-<id>`) persisted as an intent before any
    /// stock create can run.
    ///
    /// # Errors
    /// Fails when the intent cannot be persisted.
    pub fn vm_name(&self, purpose: VmPurpose, key: &str) -> Result<String, SbxError> {
        self.ownership.assign(purpose, key)
    }

    /// Persisted development grants (child maps) from the ownership file.
    #[must_use]
    pub fn dev_grants(&self) -> BTreeMap<String, DevGrantRecord> {
        self.ownership.dev_grants()
    }

    /// Persist a change to the grant section before acting on it.
    ///
    /// # Errors
    /// Fails (and rolls back) when the map cannot be written.
    pub fn update_dev_grants<R>(
        &self,
        update: impl FnOnce(&mut BTreeMap<String, DevGrantRecord>) -> R,
    ) -> Result<R, SbxError> {
        self.ownership.update_dev_grants(update)
    }

    /// Names in this daemon's own (host) ownership map.
    #[must_use]
    pub fn host_vm_names(&self) -> BTreeSet<String> {
        self.ownership.host_names()
    }

    /// Cached stock inventory (one `sbx ls --json` when cold or `refresh`).
    /// Storing a fresh view adopts host and grant intents by name.
    ///
    /// # Errors
    /// Fails when stock inventory cannot be observed.
    pub fn stock_inventory(&self, refresh: bool) -> Result<Arc<StockInventory>, SbxError> {
        self.stock_view(refresh)
    }

    /// Store an `sbx ls --json` result observed by the dev broker as the
    /// fresh view (adopting intents by name), avoiding a second listing.
    ///
    /// # Errors
    /// Fails when the bytes are not a stock inventory or cannot be persisted.
    pub fn adopt_inventory(&self, raw: &[u8]) -> Result<Arc<StockInventory>, SbxError> {
        match StockInventory::decode(raw) {
            Ok(inventory) => self.ownership.store_view(inventory),
            Err(error) => {
                self.ownership.invalidate();
                Err(error)
            }
        }
    }

    /// Drop the cached inventory after a lifecycle change made elsewhere.
    pub fn invalidate_stock_inventory(&self) {
        self.ownership.invalidate();
    }

    /// The stock executable and runner, for the dev broker's forwarded calls.
    #[must_use]
    pub fn stock_command(&self) -> (PathBuf, Arc<dyn CommandRunner>) {
        (self.sbx.clone(), Arc::clone(&self.runner))
    }

    /// Keep verified local Kit job-image archives in `directory` (owner-only,
    /// shared by this user's daemons), keyed by Kit source fingerprint, so a
    /// cold Kit VM skips the Buildx export. Unusable directories disable it.
    #[must_use]
    pub fn with_kit_image_cache(mut self, directory: PathBuf) -> Self {
        self.kit_image_cache = private_cache_directory(&directory).then_some(directory);
        self
    }

    /// Whether published (digest) Kit images are saved to and loaded from
    /// the Kit image cache (default on). A nested `--dev` daemon turns it
    /// off: its stock calls cross the dev broker, and moving an archive
    /// through it costs more than the child VM's own registry pull.
    #[must_use]
    pub fn with_published_image_cache(mut self, enabled: bool) -> Self {
        self.published_image_cache = enabled;
        self
    }

    /// Build a packaged local Kit source's job image with the same Buildx
    /// path a cold Kit VM uses and store it in the Kit image cache. Returns
    /// `false` when the cache already holds this source fingerprint.
    ///
    /// # Errors
    /// Fails without a cache, on an unreadable source, a failed build, or a
    /// source that changed during the build.
    pub fn prebuild_local_kit_image(&self, source: &Path) -> Result<bool, SbxError> {
        let directory = self
            .kit_image_cache
            .as_deref()
            .ok_or_else(|| SbxError::Io(io::Error::other("the Kit image cache is unavailable")))?;
        let fingerprint = local_source_fingerprint(source)?;
        if cached_local_build(directory, &fingerprint).is_some() {
            return Ok(false);
        }
        let scratch = tempfile::Builder::new()
            .prefix(".build-")
            .tempdir_in(directory)
            .map_err(SbxError::Io)?;
        let build = self.build_local_source(source, &fingerprint, scratch.path())?;
        if local_source_fingerprint(source)? != fingerprint {
            let _ = Self::cleanup_local_build(&build);
            return Err(SbxError::SourceChanged(source.to_owned()));
        }
        retain_local_build(directory, &build).map_err(SbxError::Io)?;
        Ok(true)
    }

    /// Select an isolated ephemeral HOME slot pool for controlled caller tests.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_ephemeral_root_for_test(mut self, root: PathBuf) -> Self {
        self.ephemeral_root = root;
        self
    }

    /// Validate a source batch before VM preparation: live path identity,
    /// in-batch overlap, overlap with this daemon's active grants, and any
    /// ephemeral slot ownership. Runs no stock command.
    ///
    /// # Errors
    /// Returns path, overlap or slot-ownership failures.
    pub fn preflight_host_grants(&self, grants: &[AdmittedHostGrant]) -> Result<(), SbxError> {
        for grant in grants {
            grant.verify_path()?;
            if let Some(token) = &grant.ephemeral {
                token.validate_in(&self.ephemeral_root, grant)?;
            }
        }
        self.reject_source_overlap(grants)
    }

    /// Reject nested/aliased sources within one batch or against any source
    /// this daemon currently has mounted. Identical sources may be shared.
    fn reject_source_overlap(&self, grants: &[AdmittedHostGrant]) -> Result<(), SbxError> {
        let batch = grants
            .iter()
            .map(|grant| source_chain::SourceRecord::new(&grant.source, &grant.chain))
            .collect::<Vec<_>>();
        for (index, record) in batch.iter().enumerate() {
            if batch[..index].iter().any(|prior| record.conflicts(prior)) {
                return Err(SbxError::HostGrantFence(format!(
                    "source batch overlaps or aliases {}",
                    record.as_path().display()
                )));
            }
        }
        let mut active = self
            .grant_mount_references
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|reference| reference.source_record.clone())
            .collect::<Vec<_>>();
        active.extend(
            self.shell_mount_references
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .filter(|reference| {
                    reference
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .phase
                        != ShellMountPhase::Dormant
                })
                .map(|reference| reference.source_record.clone()),
        );
        for record in &batch {
            if let Some(conflict) = active.iter().find(|active| record.conflicts(active)) {
                return Err(SbxError::HostGrantFence(format!(
                    "source {} overlaps active grant {}",
                    record.as_path().display(),
                    conflict.as_path().display()
                )));
            }
        }
        Ok(())
    }

    /// Allocate one private bounded ephemeral home slot before attachment.
    ///
    /// # Errors
    /// Reports capacity exhaustion or unsafe pool directories.
    pub fn allocate_ephemeral_home(&self) -> Result<EphemeralHomeLease, SbxError> {
        EphemeralHomeLease::allocate_in(&self.ephemeral_root)
    }

    /// Empty the slot after trusted session drain, while the lease is held.
    ///
    /// # Errors
    /// Failed cleanup keeps the slot's data and the caller's lease.
    pub fn release_ephemeral_home(&self, lease: &EphemeralHomeLease) -> Result<(), SbxError> {
        lease.release()
    }

    /// Retained for API compatibility; slot admission ends with the lease.
    ///
    /// # Errors
    /// Never fails.
    pub fn close_ephemeral_home_admission(
        &self,
        _token: &EphemeralHomeToken,
    ) -> Result<(), SbxError> {
        Ok(())
    }

    /// Explicitly discard ONE retained, unlocked slot's data.
    ///
    /// # Errors
    /// Rejects a slot held by a live session or failed cleanup.
    pub fn recover_ephemeral_home(&self, slot: u8) -> Result<Option<PathBuf>, SbxError> {
        EphemeralHomeLease::recover_in(&self.ephemeral_root, slot)
    }

    /// Admit only the exact private leaf selected by a live slot token.
    ///
    /// # Errors
    /// Rejects an unheld/forged token, changed leaf or unrelated source path.
    pub fn ephemeral_home_grant(
        &self,
        token: &EphemeralHomeToken,
        source: &Path,
        target: &Path,
    ) -> Result<AdmittedHostGrant, SbxError> {
        if token.path_in(&self.ephemeral_root)? != source {
            return Err(SbxError::HostGrantFence(
                "ephemeral HOME differs from its host allocation".into(),
            ));
        }
        let mut grant =
            AdmittedHostGrant::open(source.to_owned(), target.to_owned(), MountAccess::ReadWrite)?;
        token.validate_in(&self.ephemeral_root, &grant)?;
        grant.ephemeral = Some(token.clone());
        Ok(grant)
    }

    /// Cached stock inventory; one `sbx ls --json` only when cold or `refresh`.
    fn stock_view(&self, refresh: bool) -> Result<Arc<StockInventory>, SbxError> {
        if !refresh && let Some(view) = self.ownership.cached_view() {
            return Ok(view);
        }
        let observed = self
            .run_bounded_os(
                "list stock VMs",
                SBX_COMPATIBILITY_TIMEOUT,
                ["ls", "--json"],
            )
            .and_then(|output| {
                Self::require_success("list stock VMs", output.clone())?;
                StockInventory::decode(&output.stdout)
            });
        match observed {
            Ok(inventory) => self.ownership.store_view(inventory),
            Err(error) => {
                self.ownership.invalidate();
                Err(error)
            }
        }
    }

    fn classify_vm(&self, name: &str, refresh: bool) -> Result<VmClass, SbxError> {
        let view = self.stock_view(refresh)?;
        Ok(self.ownership.classify(&view, name))
    }

    /// Record the UUID of a VM this daemon just created (fresh inventory).
    fn adopt_created(&self, name: &str) -> Result<String, SbxError> {
        let view = self.stock_view(true)?;
        let uuid = view.get(name).map(|vm| vm.id.clone()).ok_or_else(|| {
            SbxError::HostGrantFence(format!("created VM {name} is absent from stock inventory"))
        })?;
        if let Some(prior) = self.ownership.uuid(name)
            && prior != uuid
            && view.contains_uuid(&prior)
        {
            return Err(SbxError::ForeignVm(name.to_owned()));
        }
        self.ownership.record(name, &uuid)?;
        Ok(uuid)
    }

    /// Remove one VM only by its recorded, currently present UUID. Absent is a
    /// successful no-op; a foreign or unrecorded VM is never touched.
    fn remove_owned_vm(&self, name: &str, deadline: Instant) -> Result<bool, SbxError> {
        let uuid = match self.classify_vm(name, true)? {
            VmClass::Absent => {
                self.ownership.forget(name)?;
                return Ok(false);
            }
            VmClass::Foreign => return Err(SbxError::ForeignVm(name.to_owned())),
            VmClass::Owned { uuid, .. } => uuid,
        };
        self.ownership.invalidate();
        // Local stock SBX addresses sandboxes by name, not UUID. The fresh
        // classification above fenced this name to the recorded UUID.
        Self::require_success(
            "remove exact owned VM",
            self.run_bounded_os(
                "remove exact owned VM",
                self.lifecycle_timeout(deadline, "remove exact owned VM")?,
                ["rm", "--force", name],
            )?,
        )?;
        if self.stock_view(true)?.contains_uuid(&uuid) {
            return Err(SbxError::HostGrantFence(
                "VM removal returned before its exact UUID disappeared".into(),
            ));
        }
        self.ownership.forget(name)?;
        Ok(true)
    }

    /// Hash the trusted worker/byte-exec artifacts once per path per daemon.
    fn cached_worker_artifact_identity(&self, worker: &Path) -> Result<String, SbxError> {
        if let Some(identity) = self
            .worker_artifact_identities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(worker)
        {
            return Ok(identity.clone());
        }
        let identity = worker_artifact_identity(worker)?;
        self.worker_artifact_identities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(worker.to_owned(), identity.clone());
        Ok(identity)
    }

    /// Hash a trusted shell/relay artifact once per file version per daemon.
    /// The key includes device, inode, size and mtime, so a rebuilt or
    /// replaced artifact is hashed again; installs still verify a full hash.
    fn cached_artifact_identity(&self, path: &Path) -> Result<String, SbxError> {
        let metadata = fs::metadata(path).map_err(|source| SbxError::Metadata {
            path: path.to_owned(),
            source,
        })?;
        let key = path.join(format!(
            "#{}:{}:{}:{}.{}",
            metadata.dev(),
            metadata.ino(),
            metadata.size(),
            metadata.mtime(),
            metadata.mtime_nsec()
        ));
        if let Some(identity) = self
            .worker_artifact_identities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
        {
            return Ok(identity.clone());
        }
        let identity = artifact_identity(path)?;
        self.worker_artifact_identities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, identity.clone());
        Ok(identity)
    }

    /// Stock identity of an attached shell VM from the ownership map and the
    /// cached inventory view (refreshed only when cold).
    ///
    /// # Errors
    /// Fails closed on a foreign, absent or non-running VM.
    pub fn shell_vm_identity(&self, vm: &ReadyShellVm) -> Result<(String, String), SbxError> {
        let (transition, _cleanup) =
            weak_rw_transition_lock(&self.grant_vm_transition_locks, vm.name.clone());
        let _guard = shell_admission_before(
            || transition.try_read(),
            Instant::now() + SHELL_MOUNT_TIMEOUT,
        )?;
        self.reject_quarantined_shell(&vm.name)?;
        match self.classify_vm(&vm.name, false)? {
            VmClass::Owned {
                uuid,
                running: true,
            } => Ok((vm.name.clone(), uuid)),
            VmClass::Foreign => Err(SbxError::ForeignVm(vm.name.clone())),
            _ => {
                self.ownership.invalidate();
                Err(SbxError::HostGrantFence(
                    "attached shell requires its owned running stock UUID".into(),
                ))
            }
        }
    }

    /// Verify the observable stock-SBX contract required by this adapter.
    ///
    /// Accept stock SBX 0.45.0 and newer, including coherent nightly builds.
    /// A help probe also checks the local Kit v3 create surface marsh calls.
    ///
    /// # Errors
    /// Returns an error when the binary is too old, lacks a required public
    /// capability, exits unsuccessfully, or cannot be executed promptly.
    pub fn validate_supported_version(&self) -> Result<(), SbxError> {
        let version = self.run_bounded_os(
            "query stock SBX version",
            SBX_COMPATIBILITY_TIMEOUT,
            ["version"],
        )?;
        Self::require_success("query stock SBX version", version.clone())?;
        let rendered_version = combined_output(&version);
        if !supported_sbx_version(&rendered_version) {
            return Err(SbxError::UnsupportedVersion {
                found: rendered_version.trim().to_owned(),
                required: "stock SBX v0.45.0 or newer (stable or coherent nightly) with local Kit v3 support",
            });
        }

        let create_help = self.run_bounded_os(
            "probe stock SBX Kit v3 support",
            SBX_COMPATIBILITY_TIMEOUT,
            ["create", "--help"],
        )?;
        Self::require_success("probe stock SBX Kit v3 support", create_help.clone())?;
        if !normalized_output(&create_help).contains("sandbox kit reference") {
            return Err(SbxError::MissingCapability("local Kit v3 create"));
        }

        Ok(())
    }

    #[cfg(test)]
    fn with_preparation_timeouts(mut self, preparation_timeouts: PreparationTimeouts) -> Self {
        self.preparation_timeouts = preparation_timeouts;
        self
    }

    #[cfg(test)]
    fn with_retained_process_teardown_timeout(mut self, timeout: Duration) -> Self {
        self.retained_process_teardown_timeout = timeout;
        self
    }

    /// Report whether the exact owned Kit VM already exists without starting it.
    ///
    /// # Errors
    /// Fails closed when the name exists with a different ownership marker or
    /// when stock SBX cannot classify the inspection result.
    pub fn kit_vm_exists(&self, spec: &KitVmSpec) -> Result<bool, SbxError> {
        validate_name(&spec.name)?;
        let lifecycle_identity = selected_home_identity(&spec.lifecycle_workspace)?;
        let (marker, local_source) = match &spec.workload_kit.location {
            NativeKitLocation::ImmutableOci(_) => (
                format!(
                    "kit:{}:{}",
                    spec.workload_kit.identity(),
                    lifecycle_identity
                ),
                false,
            ),
            NativeKitLocation::LocalV3Source(source) => (
                local_kit_marker(
                    spec.workload_kit.identity(),
                    &spec.workload_kit.validate_captured_source(source)?,
                    &lifecycle_identity,
                ),
                true,
            ),
        };
        let running = match self.classify_vm(&spec.name, false)? {
            VmClass::Absent => return Ok(false),
            VmClass::Foreign => return Err(SbxError::ForeignVm(spec.name.clone())),
            VmClass::Owned { running, .. } => running,
        };
        {
            {
                if local_source && !running {
                    return Ok(false);
                }
                if let Err(error) = self.verify_owned_marker(&spec.name, &marker) {
                    if local_source && self.is_owned_stale_local_vm(spec, &lifecycle_identity)? {
                        return Ok(false);
                    }
                    return Err(error);
                }
                Ok(true)
            }
        }
    }

    /// Remove every remaining Kit VM recorded in this daemon's ownership map.
    ///
    /// Scope reset first retires the Kit VMs of currently registered commands;
    /// any other recorded Kit VM (old generations, legacy names) is stale. One
    /// fresh stock inventory decides presence; removal targets the exact
    /// recorded UUID only. Unrecorded VMs are never touched.
    ///
    /// # Errors
    /// Fails when inventory, retained state shutdown, or VM removal cannot be
    /// verified.
    pub fn reset_stale_scope_kit_vms(&self) -> Result<usize, SbxError> {
        self.reset_stale_scope_kit_vms_before(Instant::now() + Duration::from_hours(24))
            .map(|removed| removed.len())
    }

    /// Deadline-bounded form of [`Self::reset_stale_scope_kit_vms`]. Returns
    /// the exact names of the stock VMs it removed.
    ///
    /// # Errors
    /// Fails when inventory, removal, or the aggregate deadline cannot be
    /// verified.
    pub fn reset_stale_scope_kit_vms_before(
        &self,
        deadline: Instant,
    ) -> Result<Vec<String>, SbxError> {
        if Instant::now() >= deadline {
            return Err(SbxError::LifecycleDeadline {
                operation: "list scope Kit VMs for reset",
            });
        }
        let view = self.stock_view(true)?;
        let mut removed = Vec::new();
        for vm in self.ownership.owned_kit_names() {
            if view.get(&vm).is_none() {
                self.ownership.forget(&vm)?;
                continue;
            }
            self.reset_stale_scope_kit_vm_candidate(&vm, deadline)?;
            removed.push(vm);
        }
        Ok(removed)
    }

    fn reset_stale_scope_kit_vm_candidate(
        &self,
        vm: &str,
        deadline: Instant,
    ) -> Result<(), SbxError> {
        validate_name(vm)?;
        let (vm_transition, _vm_cleanup) =
            weak_rw_transition_lock(&self.grant_vm_transition_locks, vm.to_owned());
        let _vm_transition = vm_transition
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (stock_operation, _stock_cleanup) =
            weak_mutex_transition_lock(&self.grant_stock_operation_locks, vm.to_owned());
        let _stock_operation = stock_operation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        {
            let mut references = self
                .grant_mount_references
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Scope teardown admits no attached shell and no queued or
            // running job: references left on an old generation belong to
            // ended work and end with the VM.
            references.retain(|(candidate, _), _| candidate != vm);
        }
        self.drop_worker_lease_before(vm, deadline)?;
        self.remove_owned_vm(vm, deadline)?;
        self.installed_worker_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(vm);
        self.worker_generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(vm);
        self.quarantined_workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(vm);
        Ok(())
    }

    /// Remove one exactly-owned Kit VM and invalidate every adapter cache that
    /// could otherwise treat its prior generation as live. Missing VMs are a
    /// successful no-op; foreign VMs and VMs with retained grants fail closed.
    ///
    /// # Errors
    /// Fails when ownership cannot be proven or stock SBX cannot remove the VM.
    pub fn reset_kit_vm(&self, spec: &KitVmSpec) -> Result<bool, SbxError> {
        self.reset_kit_vm_before(spec, Instant::now() + Duration::from_hours(24))
    }

    /// Deadline-bounded form of [`Self::reset_kit_vm`].
    ///
    /// # Errors
    /// Fails when ownership, retained state, removal, or the aggregate
    /// deadline cannot be verified.
    pub fn reset_kit_vm_before(
        &self,
        spec: &KitVmSpec,
        deadline: Instant,
    ) -> Result<bool, SbxError> {
        self.retire_kit_vm_before(spec, deadline, None)
    }

    /// Retire one exactly-owned Kit VM (recorded UUID) for an operator reset.
    /// `live` names the shell sessions that are still attached; a retained
    /// pin from any other session, or retained job references on a VM the
    /// caller reports quarantined, are leftovers of ended work and end with
    /// the VM. `None` is the ordinary reset: any retained reference refuses
    /// unless the adapter itself quarantined the VM.
    ///
    /// # Errors
    /// Fails when ownership, live use, removal, or the deadline cannot be
    /// verified.
    pub fn retire_kit_vm_before(
        &self,
        spec: &KitVmSpec,
        deadline: Instant,
        live: Option<(&BTreeSet<String>, bool)>,
    ) -> Result<bool, SbxError> {
        validate_name(&spec.name)?;
        let (vm_transition, _vm_cleanup) =
            weak_rw_transition_lock(&self.grant_vm_transition_locks, spec.name.clone());
        let _vm_transition = vm_transition
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (stock_operation, _stock_cleanup) =
            weak_mutex_transition_lock(&self.grant_stock_operation_locks, spec.name.clone());
        let _stock_operation = stock_operation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let quarantined = self.reject_quarantined(&spec.name).is_err();
        match self.classify_vm(&spec.name, true)? {
            VmClass::Absent => {
                self.drop_worker_lease(&spec.name)?;
                self.clear_worker_generation(spec);
                self.ownership.forget(&spec.name)?;
                return Ok(false);
            }
            VmClass::Foreign => return Err(SbxError::ForeignVm(spec.name.clone())),
            VmClass::Owned { .. } => {}
        }
        // The recorded UUID is the ownership authority (trusted stock SBX,
        // random daemon-chosen name). Retiring a quarantined VM releases its
        // retained mount references: the VM itself is about to be removed.
        {
            let mut references = self
                .grant_mount_references
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let in_use = references.iter().any(|((vm, _), reference)| {
                vm == &spec.name
                    && match live {
                        None => reference.active_jobs > 0 || !reference.pinned_sessions.is_empty(),
                        Some((sessions, caller_quarantined)) => {
                            (reference.active_jobs > 0 && !caller_quarantined)
                                || reference
                                    .pinned_sessions
                                    .keys()
                                    .any(|session| sessions.contains(session))
                        }
                    }
            });
            if !quarantined && in_use {
                return Err(SbxError::ActiveWorkerState(spec.name.clone()));
            }
            // Idle retained mounts end with the VM.
            references.retain(|(vm, _), _| vm != &spec.name);
        }
        self.drop_worker_lease_before(&spec.name, deadline)?;
        self.remove_observed_kit(&spec.name, deadline)?;
        self.clear_worker_generation(spec);
        Ok(true)
    }

    fn remove_observed_kit(&self, name: &str, deadline: Instant) -> Result<(), SbxError> {
        self.remove_owned_vm(name, deadline).map(|_| ())
    }

    fn cleanup_created_kit(&self, spec: &KitVmSpec) {
        let known = self.ownership.uuid(&spec.name).is_some();
        if !known
            || self
                .remove_observed_kit(&spec.name, Instant::now() + self.preparation_timeouts.sbx)
                .is_err()
        {
            self.quarantined_workers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(spec.name.clone());
        }
    }

    fn clear_worker_generation(&self, spec: &KitVmSpec) {
        // Reset holds the VM write fence: no initializer for this generation
        // can be waiting under its read fence. Drop only the dead selector.
        self.worker_init_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&spec.name);
        self.ownership.invalidate();
        self.installed_worker_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&spec.name);
        self.worker_generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&spec.name);
        self.quarantined_workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&spec.name);
        self.local_resolutions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(spec.workload_kit.identity());
    }

    /// Reuses an exactly-owned VM or creates one from the stock shell-docker
    /// template, then installs the trusted worker and waits for Docker.
    ///
    /// # Errors
    /// Fails closed if an existing name lacks matching marsh ownership data.
    pub fn ensure_kit_vm(&self, spec: &KitVmSpec) -> Result<ReadyKitVm, SbxError> {
        self.ensure_kit_vm_authorized(spec, None)
    }

    /// Prepare an isolated ephemeral Kit using its private home as lifecycle
    /// workspace. Every implicit export uses the same native private owner token.
    ///
    /// # Errors
    /// Rejects an invalid token/workspace before any Create or helper effect.
    pub fn ensure_ephemeral_kit_vm(
        &self,
        spec: &KitVmSpec,
        token: &EphemeralHomeToken,
    ) -> Result<ReadyKitVm, SbxError> {
        self.ensure_kit_vm_authorized(spec, Some(token))
    }

    /// Stock `sbx mcp load SERVER --sandbox VM` into an exactly-owned Kit VM.
    /// The caller owns the policy of when this is allowed (default
    /// publications: only a VM this daemon just created, before any job).
    ///
    /// # Errors
    /// Rejects invalid names, a VM that is not this daemon's recorded VM, and
    /// a failed or timed-out stock load.
    pub fn load_mcp_server(&self, ready: &ReadyKitVm, server: &str) -> Result<(), SbxError> {
        validate_name(&ready.name)?;
        if server.is_empty()
            || server.len() > 128
            || !server
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(SbxError::InvalidIdentifier(server.into()));
        }
        self.reject_quarantined(&ready.name)?;
        match self.classify_vm(&ready.name, false)? {
            VmClass::Owned { .. } => {}
            VmClass::Absent | VmClass::Foreign => {
                return Err(SbxError::ForeignVm(ready.name.clone()));
            }
        }
        Self::require_success(
            "load default MCP server",
            self.run_bounded_os(
                "load default MCP server",
                self.preparation_timeouts.sbx,
                ["mcp", "load", server, "--sandbox", ready.name.as_str()],
            )?,
        )
    }

    fn ensure_kit_vm_authorized(
        &self,
        spec: &KitVmSpec,
        token: Option<&EphemeralHomeToken>,
    ) -> Result<ReadyKitVm, SbxError> {
        validate_name(&spec.name)?;
        let (vm_transition, _vm_cleanup) =
            weak_rw_transition_lock(&self.grant_vm_transition_locks, spec.name.clone());
        let _vm_transition = vm_transition
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Stop publishes quarantine before waiting for this fence. Recheck
        // after acquiring it so no new VM or worker lifecycle can begin once
        // quarantine is visible.
        self.reject_quarantined(&spec.name)?;
        regular_executable(&spec.worker_binary)?;
        regular_executable(&byte_exec_artifact(&spec.worker_binary))?;
        regular_executable(&job_artifact(&spec.worker_binary))?;
        let lifecycle_grant = if let Some(token) = token {
            self.ephemeral_home_grant(token, &spec.lifecycle_workspace, &spec.lifecycle_workspace)?
        } else {
            Self::open_lifecycle_workspace(spec)?
        };
        let image = match &spec.workload_kit.location {
            NativeKitLocation::LocalV3Source(_) => {
                let ready = self.ensure_local_kit_vm(spec, &lifecycle_grant)?;
                self.reject_quarantined(&spec.name)?;
                return Ok(ready);
            }
            NativeKitLocation::ImmutableOci(image) => image.clone(),
        };
        let lifecycle_identity = selected_home_identity(&spec.lifecycle_workspace)?;
        let marker = format!(
            "kit:{}:{}",
            spec.workload_kit.identity(),
            lifecycle_identity
        );
        let cold_started = match self.classify_vm(&spec.name, false)? {
            VmClass::Foreign => return Err(SbxError::ForeignVm(spec.name.clone())),
            VmClass::Owned { .. } => {
                if self.verify_owned_marker(&spec.name, &marker).is_ok() {
                    false
                } else {
                    self.ownership.invalidate();
                    return Err(SbxError::ForeignVm(spec.name.clone()));
                }
            }
            VmClass::Absent => {
                if let Err(error) = self.create_published_vm_with_grant(spec, &lifecycle_grant) {
                    // Submitted Create may complete late; never delete by a
                    // name alone or discharge its durable pending operation.
                    if !matches!(&error, SbxError::Io(source) if marsh_runtime::command_not_started(source))
                    {
                        self.quarantined_workers
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(spec.name.clone());
                    }
                    return Err(error);
                }
                true
            }
        };
        if !cold_started {
            lifecycle_grant.verify_path()?;
        }
        if let Err(error) = self.write_owned_marker(&spec.name, &marker) {
            if cold_started {
                self.remove_observed_kit(
                    &spec.name,
                    Instant::now() + self.preparation_timeouts.sbx,
                )?;
            }
            return Err(error);
        }
        let ready = ReadyKitVm {
            name: spec.name.clone(),
            kit_ref: spec.workload_kit.identity().to_owned(),
            lifecycle_workspace: spec.lifecycle_workspace.clone(),
            cold_started,
            job_image: image,
            worker_binary: spec.worker_binary.clone(),
        };
        self.ensure_worker_lease(&ready)?;
        // If stop was requested while initialization held the shared fence,
        // do not let its completed value escape into the backend Ready cache.
        self.reject_quarantined(&spec.name)?;
        Ok(ready)
    }

    /// Validates that a cached Kit still names the exact requested workload,
    /// uses the current trusted worker artifact, and retains its live worker
    /// lease. A continuously-live lease also proves the VM-local image cache
    /// established when the Kit was prepared has not been discarded.
    ///
    /// # Errors
    /// Fails when the worker artifact cannot be inspected or lease state
    /// cannot be queried. Returns `false` when the caller must perform the
    /// full owned-VM validation, worker reinstall, and image prewarm path.
    pub fn revalidate_cached_kit(
        &self,
        spec: &KitVmSpec,
        ready: &ReadyKitVm,
    ) -> Result<bool, SbxError> {
        self.reject_quarantined(&spec.name)?;
        let expected_image = match &spec.workload_kit.location {
            NativeKitLocation::ImmutableOci(image) => Some(image.clone()),
            NativeKitLocation::LocalV3Source(source) => {
                // Backend requests carry a captured generation. Reuse that
                // immutable fingerprint for the hot cache lookup instead of
                // walking the local source tree a second time. Uncaptured
                // callers still fingerprint here, and any path that reads or
                // rebuilds source bytes validates the capture before use.
                let fingerprint = spec.workload_kit.expected_local_fingerprint(source)?;
                self.local_resolutions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(spec.workload_kit.identity())
                    .filter(|resolution| resolution.source_fingerprint == fingerprint)
                    .map(|resolution| resolution.image_id.clone())
            }
        };
        if ready.name != spec.name
            || ready.kit_ref != spec.workload_kit.identity()
            || ready.lifecycle_workspace != spec.lifecycle_workspace
            || expected_image.as_ref() != Some(&ready.job_image)
            || ready.worker_binary != spec.worker_binary
        {
            return Ok(false);
        }
        let artifact = self.cached_worker_artifact_identity(&spec.worker_binary)?;
        let installed = self
            .installed_worker_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&spec.name)
            .is_some_and(|installed| installed == &artifact);
        if !installed {
            self.drop_worker_lease(&spec.name)?;
            return Ok(false);
        }
        let Some(lease) = self
            .worker_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&spec.name)
            .cloned()
        else {
            return Ok(false);
        };
        let dead = !lease.alive.load(Ordering::Acquire)
            || lease
                .process
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .try_wait()?
                .is_some();
        if dead {
            if lease.active.load(Ordering::Acquire) != 0 {
                self.quarantined_workers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(spec.name.clone());
                return Err(SbxError::WorkerLeaseLost(spec.name.clone()));
            }
            self.drop_worker_lease(&spec.name)?;
            return Ok(false);
        }
        Self::ping_worker(&lease).map(|()| true)
    }

    /// Remove the exact owned project shell VM and invalidate all adapter state.
    /// Healthy exact-UUID absence is a successful no-op. Active sessions and
    /// foreign identities fail closed. Quarantined references may be recovered
    /// only against the UUID observed before their first stock mount.
    ///
    /// # Errors
    /// Fails when ownership cannot be proven, retained shell state cannot be stopped,
    /// or stock SBX cannot verify removal.
    pub fn reset_shell_vm(&self, spec: &ShellVmSpec) -> Result<bool, SbxError> {
        self.reset_shell_vm_before(spec, Instant::now() + Duration::from_hours(24))
    }

    /// Deadline-bounded form of [`Self::reset_shell_vm`].
    ///
    /// # Errors
    /// Fails when ownership, retained state, removal, or the aggregate
    /// deadline cannot be verified.
    pub fn reset_shell_vm_before(
        &self,
        spec: &ShellVmSpec,
        deadline: Instant,
    ) -> Result<bool, SbxError> {
        validate_name(&spec.name)?;
        // Even an ephemeral open rejected before ensure may need a later retry
        // if its automatic cleanup cannot read healthy inventory. Remember the
        // exact host spec before any deadline/stock failure, not just on ensure.
        self.remember_shell_recovery_spec(spec);
        self.lifecycle_timeout(deadline, "shell reset admission")?;
        let (transition, _cleanup) =
            weak_rw_transition_lock(&self.grant_vm_transition_locks, spec.name.clone());
        let _guard = shell_admission_before(|| transition.try_write(), deadline)?;
        let quarantined = self.check_shell_reset_admission(&spec.name)?;
        if quarantined && self.ownership.uuid(&spec.name).is_none() {
            return Err(SbxError::ShellMountCleanupUncertain(
                "no recorded shell VM UUID; cannot authorize name-only recovery".into(),
            ));
        }
        let init_lock = self
            .shell_init_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(spec.name.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _initializing = shell_admission_before(|| init_lock.try_lock(), deadline)?;
        let (operation, _cleanup) =
            weak_mutex_transition_lock(&self.grant_stock_operation_locks, spec.name.clone());
        let _operation = shell_admission_before(|| operation.try_lock(), deadline)?;
        if matches!(self.classify_vm(&spec.name, true)?, VmClass::Owned { .. }) {
            self.uncertain_shell_grants
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(spec.name.clone())
                .or_default();
        }
        self.drop_shell_lease_before(&spec.name, deadline)?;
        let removed = self.remove_owned_vm(&spec.name, deadline)?;
        self.finish_shell_vm_recovery(&spec.name);
        Ok(removed)
    }

    fn check_shell_reset_admission(&self, vm: &str) -> Result<bool, SbxError> {
        // Caller holds the VM write fence; retained admission counters are not
        // discarded by a failed generation's narrow rollback.
        if self
            .active_shell_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(vm)
            .is_some_and(|count| count.load(Ordering::Acquire) != 0)
        {
            return Err(SbxError::ActiveWorkerState(vm.into()));
        }
        let quarantined = self.reject_quarantined_shell(vm).is_err();
        if !quarantined
            && self
                .shell_mount_references
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .any(|((name, _, _), reference)| {
                    name == vm
                        && !reference
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .owners
                            .is_empty()
                })
        {
            return Err(SbxError::ActiveWorkerState(vm.into()));
        }
        Ok(quarantined)
    }

    fn remember_shell_recovery_spec(&self, spec: &ShellVmSpec) {
        self.shell_recovery_specs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(spec.name.clone())
            .or_insert_with(|| spec.clone());
    }

    fn finish_shell_vm_recovery(&self, vm: &str) {
        // The VM write fence is held. This releases shell-local ownership only;
        // durable source authority belongs to the separate all-owner ledger.
        self.invalidate_shell_cache(vm);
        self.shell_mount_references
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(name, _, _), _| name != vm);
        self.uncertain_shell_grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(vm);
        self.shell_recovery_specs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(vm);
        self.active_shell_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(vm);
        self.shell_init_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(vm);
    }

    /// Fence operator recovery against another admitted shell, including cold preparation.
    ///
    /// # Errors
    /// Rejects quarantined VMs and bounded lifecycle admission contention.
    pub fn admit_shell_session(&self, name: &str) -> Result<ShellVmSession, SbxError> {
        validate_name(name)?;
        let (transition, _cleanup) =
            weak_rw_transition_lock(&self.grant_vm_transition_locks, name.to_owned());
        let _guard = shell_admission_before(
            || transition.try_read(),
            Instant::now() + SHELL_MOUNT_TIMEOUT,
        )?;
        self.reject_quarantined_shell(name)?;
        let counter = self
            .active_shell_sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(name.to_owned())
            .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
            .clone();
        counter.fetch_add(1, Ordering::AcqRel);
        Ok(ShellVmSession(counter))
    }

    /// Exact host-owned shell specs retained for explicit scope recovery, including ephemeral VMs.
    #[must_use]
    pub fn shell_recovery_specs(&self, persistent: &ShellVmSpec) -> Vec<ShellVmSpec> {
        let mut specs = self
            .shell_recovery_specs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        specs
            .entry(persistent.name.clone())
            .or_insert_with(|| persistent.clone());
        specs.into_values().collect()
    }

    /// Creates or reuses an exactly-owned DHI shell VM and installs Brush
    /// marsh as its login process.
    ///
    /// # Errors
    /// Fails closed on a foreign name, mutable image, unsafe artifact, or
    /// guest-user mismatch.
    pub fn ensure_shell_vm(&self, spec: &ShellVmSpec) -> Result<ReadyShellVm, SbxError> {
        let (transition, _cleanup) =
            weak_rw_transition_lock(&self.grant_vm_transition_locks, spec.name.clone());
        let _guard = shell_admission_before(
            || transition.try_write(),
            Instant::now() + SHELL_MOUNT_TIMEOUT,
        )?;
        self.reject_quarantined_shell(&spec.name)?;
        self.remember_shell_recovery_spec(spec);
        self.ensure_shell_vm_inner(spec)
    }

    fn reject_observed_shell_disappearance(&self, name: &str) -> Result<(), SbxError> {
        let retained = {
            let mut references = self
                .shell_mount_references
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Idle retained mounts vanished with the VM; live or uncertain ones
            // still fence admission.
            references.retain(|(vm, _, _), reference| {
                let state = reference
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                vm != name
                    || !state.owners.is_empty()
                    || !matches!(
                        state.phase,
                        ShellMountPhase::Mounted | ShellMountPhase::Dormant
                    )
            });
            references.keys().any(|(vm, _, _)| vm == name)
        };
        if retained {
            self.uncertain_shell_grants
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(name.to_owned())
                .or_default();
            return Err(SbxError::QuarantinedShellVm(name.to_owned()));
        }
        Ok(())
    }

    fn ensure_shell_vm_inner(&self, spec: &ShellVmSpec) -> Result<ReadyShellVm, SbxError> {
        validate_name(&spec.name)?;
        validate_token(&spec.user.name)?;
        if spec.user.uid == 0 || spec.user.gid == 0 || !safe_absolute(&spec.user.home) {
            return Err(SbxError::InvalidShellUser);
        }
        let template = ShellTemplateReference::parse(spec.image.as_str())?;
        let selected_template = template.create_reference()?;
        // Locally produced images exist only in the local stock image store.
        let pull_policy = if template.requires_local_authority() {
            "never"
        } else {
            "missing"
        };
        let shell_artifact = self.cached_artifact_identity(&spec.shell_binary)?;
        let init_lock = self
            .shell_init_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(spec.name.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _initializing = init_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.reject_quarantined_shell(&spec.name)?;
        let marker = format!("shell:{}:{}", spec.name, spec.image.as_str());
        let identity = ShellInitIdentity {
            marker: marker.clone(),
            shell_artifact: shell_artifact.clone(),
            user: spec.user.clone(),
        };
        if self
            .ready_shells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&spec.name)
            == Some(&identity)
            && self.shell_lease_live(&spec.name)?
        {
            return Ok(ReadyShellVm {
                name: spec.name.clone(),
                user: spec.user.clone(),
                cold_started: false,
            });
        }
        self.invalidate_shell_cache(&spec.name);
        let cold_started = match self.classify_vm(&spec.name, false)? {
            VmClass::Foreign => return Err(SbxError::ForeignVm(spec.name.clone())),
            VmClass::Owned { .. } => {
                if let Err(error) = self.verify_owned_marker(&spec.name, &marker) {
                    self.ownership.invalidate();
                    return Err(error);
                }
                false
            }
            VmClass::Absent => {
                // Do not create a replacement underneath retained acquisitions
                // from an observed VM generation. Explicit recovery must first
                // prove its UUID absent and settle the old references.
                self.reject_observed_shell_disappearance(&spec.name)?;
                let expected_digest =
                    (selected_template != template.image().as_str()).then(|| template.digest());
                self.create_shell_from_template(
                    spec,
                    selected_template,
                    pull_policy,
                    expected_digest,
                )?;
                true
            }
        };
        self.ensure_shell_lease(spec, &marker, &shell_artifact)?;
        self.installed_shell_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                (spec.name.clone(), "/usr/local/bin/marsh".into()),
                shell_artifact,
            );
        self.ready_shells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(spec.name.clone(), identity);
        Ok(ReadyShellVm {
            name: spec.name.clone(),
            user: spec.user.clone(),
            cold_started,
        })
    }

    fn create_shell_from_template(
        &self,
        spec: &ShellVmSpec,
        template: &str,
        pull: &str,
        expected_digest: Option<&str>,
    ) -> Result<(), SbxError> {
        self.ownership.invalidate();
        let outcome = self
            .run_bounded_os(
                "create shell VM",
                self.preparation_timeouts.sbx,
                [
                    "create",
                    "--name",
                    &spec.name,
                    "--pull",
                    pull,
                    "--skills",
                    "off",
                    "--template",
                    template,
                    "shell",
                ],
            )
            .and_then(|output| Self::require_success("create shell VM", output));
        if let Err(error) = outcome {
            return Err(if pull == "never" {
                SbxError::HostGrantFence(format!(
                    "create shell VM from local image {template} failed (is it loaded into the stock SBX image store?): {error}"
                ))
            } else {
                error
            });
        }
        // Inspect is read-only stock metadata: overlap it with the inventory
        // refresh that adopts the new UUID. Nothing enters the VM before both.
        let (adopted, inspected) = thread::scope(|scope| {
            let inspect = expected_digest.map(|_| {
                scope.spawn(|| {
                    self.run_bounded_os(
                        "inspect shell VM",
                        self.preparation_timeouts.sbx,
                        ["inspect", "--json", &spec.name],
                    )
                })
            });
            let adopted = self.adopt_created(&spec.name);
            let inspected = inspect.map(|handle| {
                handle
                    .join()
                    .map_err(|_| SbxError::Io(io::Error::other("shell VM inspect panicked")))
                    .and_then(std::convert::identity)
            });
            (adopted, inspected)
        });
        adopted?;
        if let (Some(expected), Some(inspected)) = (expected_digest, inspected) {
            self.verify_created_shell_digest(&spec.name, expected, &inspected?)?;
        }
        Ok(())
    }

    /// A tag-selected local template must have produced the pinned digest.
    /// A mismatching VM is ours (just created), so remove it before failing.
    fn verify_created_shell_digest(
        &self,
        name: &str,
        expected: &str,
        output: &CommandOutput,
    ) -> Result<(), SbxError> {
        let observed = output
            .succeeded()
            .then(|| serde_json::from_slice::<PublicKitInspect>(&output.stdout).ok())
            .flatten()
            .and_then(|inspect| inspect.image_digest);
        if observed.as_deref() == Some(expected) {
            return Ok(());
        }
        let deadline = Instant::now() + self.preparation_timeouts.sbx;
        self.remove_owned_vm(name, deadline)?;
        Err(SbxError::HostGrantFence(format!(
            "local shell template tag did not resolve to {expected}; removed {name}"
        )))
    }

    /// Adds exact project and selected-home mounts at their natural guest paths.
    ///
    /// # Errors
    /// Fails if either source changes identity while SBX prepares the mount.
    pub fn prepare_shell_mounts(
        &self,
        vm: &ReadyShellVm,
        grants: &[AdmittedHostGrant],
    ) -> Result<ShellMounts, SbxError> {
        self.reject_quarantined_shell(&vm.name)?;
        let result = self.prepare_shell_mounts_inner(vm, grants);
        self.invalidate_shell_on_missing(&vm.name, result)
    }

    fn prepare_shell_mounts_inner(
        &self,
        vm: &ReadyShellVm,
        grants: &[AdmittedHostGrant],
    ) -> Result<ShellMounts, SbxError> {
        let deadline = Instant::now() + self.preparation_timeouts.sbx.min(SHELL_MOUNT_TIMEOUT);
        let (transition, _cleanup) =
            weak_rw_transition_lock(&self.grant_vm_transition_locks, vm.name.clone());
        let guard = shell_admission_before(|| transition.try_read(), deadline)?;
        self.reject_quarantined_shell(&vm.name)?;
        let uuid = match self.classify_vm(&vm.name, false)? {
            VmClass::Owned { uuid, .. } => uuid,
            VmClass::Foreign => return Err(SbxError::ForeignVm(vm.name.clone())),
            VmClass::Absent => {
                self.ownership.invalidate();
                return Err(SbxError::QuarantinedShellVm(vm.name.clone()));
            }
        };
        if let Err(error) = self.evict_idle_conflicts(grants, Some(&vm.name), deadline) {
            return Err(SbxError::ShellMountCleanupUncertain(error.to_string()));
        }
        self.preflight_host_grants(grants)?;
        {
            let mut requested = grants.iter().collect::<Vec<_>>();
            requested.sort_by(|left, right| {
                left.target
                    .components()
                    .count()
                    .cmp(&right.target.components().count())
                    .then_with(|| left.target.cmp(&right.target))
            });
            let mut acquired = ShellMounts {
                vm: vm.name.clone(),
                acquisition: NEXT_SHELL_MOUNT
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                    .map_err(|_| {
                        SbxError::HostGrantFence("shell acquisition identity exhausted".into())
                    })?,
                mounts: Vec::new(),
            };
            for grant in requested {
                if let Err(cause) =
                    self.acquire_shell_mount(vm, grant, acquired.acquisition, deadline, &uuid)
                {
                    // Drop the VM read guard before re-entering revoke; a waiting
                    // reset writer must not deadlock recursive RwLock admission.
                    drop(guard);
                    let rollback = self.revoke_shell_mounts(&acquired);
                    if rollback.is_err() || self.reject_quarantined_shell(&vm.name).is_err() {
                        self.quarantine_shell(vm, grants.to_vec());
                        return Err(SbxError::ShellMountCleanupUncertain(cause.to_string()));
                    }
                    return Err(cause);
                }
                acquired
                    .mounts
                    .push((grant.source.clone(), grant.target.clone()));
            }
            Ok(acquired)
        }
    }

    fn shell_mount_reference(
        &self,
        vm: &ReadyShellVm,
        grant: &AdmittedHostGrant,
        uuid: &str,
    ) -> Result<Arc<ShellMountReference>, SbxError> {
        let key = (vm.name.clone(), grant.source.clone(), grant.target.clone());
        let mut references = self
            .shell_mount_references
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Different sources or modes must never replace a live natural target.
        // Source inode aliases remain subject to host admission.
        if references
            .iter()
            .any(|((name, source, target), reference)| {
                name == &vm.name
                    && target == &grant.target
                    && (source != &grant.source || reference.access != grant.access)
            })
        {
            return Err(SbxError::HostGrantFence(
                "shell mount target already has different authority".into(),
            ));
        }
        Ok(references
            .entry(key)
            .or_insert_with(|| {
                Arc::new(ShellMountReference {
                    source_record: source_chain::SourceRecord::new(&grant.source, &grant.chain),
                    access: grant.access,
                    uuid: uuid.to_owned(),
                    state: Mutex::new(ShellMountState {
                        phase: ShellMountPhase::Dormant,
                        owners: BTreeSet::new(),
                        pin: None,
                    }),
                    changed: Condvar::new(),
                })
            })
            .clone())
    }

    fn acquire_shell_mount(
        &self,
        vm: &ReadyShellVm,
        grant: &AdmittedHostGrant,
        acquisition: u64,
        deadline: Instant,
        expected_uuid: &str,
    ) -> Result<(), SbxError> {
        grant.verify_path()?;
        let mode = match grant.access {
            MountAccess::ReadOnly => "ro",
            MountAccess::ReadWrite => "rw",
        };
        let spec = mount_spec(&grant.source, &grant.target, mode)?;
        let reference = self.shell_mount_reference(vm, grant, expected_uuid)?;
        if reference.uuid != expected_uuid {
            return Err(SbxError::ForeignVm(vm.name.clone()));
        }
        if reference.source_record != source_chain::SourceRecord::new(&grant.source, &grant.chain) {
            return Err(SbxError::SourceChanged(grant.source.clone()));
        }
        if self.begin_shell_mount_attempt(vm, grant, &reference, acquisition, deadline)? {
            return Ok(());
        }
        let (operation, _cleanup) =
            weak_mutex_transition_lock(&self.grant_stock_operation_locks, vm.name.clone());
        let operation = match shell_admission_before(|| operation.try_lock(), deadline) {
            Ok(operation) => operation,
            Err(error) => {
                let mut state = reference
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.phase = ShellMountPhase::Dormant;
                state.pin = None;
                reference.changed.notify_all();
                return Err(error);
            }
        };
        let mount = || {
            self.runner
                .run_bounded(
                    &self.invocation_in(
                        &grant.source,
                        [OsStr::new("mount"), OsStr::new(&vm.name), spec.as_os_str()],
                    ),
                    deadline.saturating_duration_since(Instant::now()),
                )
                .map_err(SbxError::from)
        };
        let executed = mount();
        let no_effect = matches!(&executed, Err(SbxError::Io(error)) if marsh_runtime::command_not_started(error));
        let mut result =
            executed.and_then(|output| Self::require_success("mount shell path", output));
        if let Err(error) = &result
            && stale_shared_mount_error(error)
        {
            // A saved mount from a prior daemon generation: replace it exactly.
            let replaced = unmount_spec(&grant.source, &grant.target).and_then(|unmount| {
                Self::require_success(
                    "replace stale shell mount",
                    self.runner.run_bounded(
                        &self.invocation_in(
                            &grant.source,
                            [
                                OsStr::new("umount"),
                                OsStr::new(&vm.name),
                                unmount.as_os_str(),
                            ],
                        ),
                        deadline.saturating_duration_since(Instant::now()),
                    )?,
                )
            });
            if replaced.is_ok() {
                result =
                    mount().and_then(|output| Self::require_success("mount shell path", output));
            }
        }
        let result = result.and_then(|()| grant.verify_path());
        drop(operation);
        self.finish_shell_mount_attempt(vm, grant, &reference, acquisition, no_effect, result)
    }

    fn begin_shell_mount_attempt(
        &self,
        vm: &ReadyShellVm,
        grant: &AdmittedHostGrant,
        reference: &ShellMountReference,
        acquisition: u64,
        deadline: Instant,
    ) -> Result<bool, SbxError> {
        let mut state = reference
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while matches!(
            state.phase,
            ShellMountPhase::Mounting | ShellMountPhase::Unmounting
        ) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(SbxError::ShellMountAdmissionBusy);
            }
            state = reference
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        self.reject_quarantined_shell(&vm.name)?;
        grant.verify_path()?;
        match state.phase {
            ShellMountPhase::Mounted => {
                state.owners.insert(acquisition);
                Ok(true)
            }
            ShellMountPhase::Uncertain => Err(SbxError::QuarantinedShellVm(vm.name.clone())),
            ShellMountPhase::Dormant => {
                state.pin = Some(grant.clone());
                state.phase = ShellMountPhase::Mounting;
                Ok(false)
            }
            ShellMountPhase::Mounting | ShellMountPhase::Unmounting => unreachable!(),
        }
    }

    fn finish_shell_mount_attempt(
        &self,
        vm: &ReadyShellVm,
        grant: &AdmittedHostGrant,
        reference: &ShellMountReference,
        acquisition: u64,
        no_effect: bool,
        result: Result<(), SbxError>,
    ) -> Result<(), SbxError> {
        let mut state = reference
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if result.is_ok() {
            state.phase = ShellMountPhase::Mounted;
            state.owners.insert(acquisition);
        } else if no_effect {
            state.phase = ShellMountPhase::Dormant;
            state.pin = None;
        } else {
            // An opaque failed mount may have taken effect. Preserve its exact
            // source chain until whole-VM recovery, but no caller count leaked.
            state.phase = ShellMountPhase::Uncertain;
            self.quarantine_shell(vm, vec![grant.clone()]);
        }
        reference.changed.notify_all();
        result
    }

    /// Attaches Brush through stock `sbx exec`, preserving tty mode, cwd,
    /// identity, environment, and caller argv.
    ///
    /// # Errors
    /// Returns an error when the attached stock-SBX process cannot start.
    pub fn attach_shell(
        &self,
        vm: &ReadyShellVm,
        working_directory: &Path,
        terminal: bool,
        terminal_size: Option<TerminalSize>,
        arguments: &[OsString],
    ) -> Result<Attachment, SbxError> {
        self.attach_shell_with_relay(
            vm,
            working_directory,
            terminal,
            terminal_size,
            arguments,
            None,
            &[],
        )
    }

    /// Attaches the shell with optional guest relay paths. These values are
    /// paths only; the delegated token stays in the relay-owned 0600 file.
    ///
    /// # Errors
    /// Rejects an unsafe working directory or a failed stock-SBX attachment.
    #[allow(clippy::too_many_arguments)] // Exact shell launch inputs.
    pub fn attach_shell_with_relay(
        &self,
        vm: &ReadyShellVm,
        working_directory: &Path,
        terminal: bool,
        terminal_size: Option<TerminalSize>,
        arguments: &[OsString],
        relay_paths: Option<(&Path, &Path)>,
        extra_environment: &[String],
    ) -> Result<Attachment, SbxError> {
        self.reject_quarantined_shell(&vm.name)?;
        if !safe_absolute(working_directory) {
            return Err(SbxError::UnsafePath(working_directory.to_owned()));
        }
        if terminal != terminal_size.is_some() {
            return Err(SbxError::InvalidTerminalSize);
        }
        let session_record = relay_paths
            .and_then(|(socket, _)| socket.parent().map(|runtime| runtime.join("shell")))
            .unwrap_or_else(|| {
                PathBuf::from(format!(
                    "/run/marsh/{}/{}/shell",
                    vm.user.uid,
                    uuid::Uuid::new_v4()
                ))
            });
        let mut environment = Self::shell_identity_environment(&vm.user)
            .into_iter()
            .map(String::into_bytes)
            .collect::<Vec<_>>();
        if let Some((socket, token)) = relay_paths {
            let mut value = b"MARSH_DAEMON_SOCKET=".to_vec();
            value.extend_from_slice(socket.as_os_str().as_bytes());
            environment.push(value);
            let mut value = b"MARSH_DAEMON_TOKEN=".to_vec();
            value.extend_from_slice(token.as_os_str().as_bytes());
            environment.push(value);
        }
        environment.extend(
            extra_environment
                .iter()
                .map(|entry| entry.clone().into_bytes()),
        );
        let environment = environment
            .into_iter()
            .map(|entry| {
                let split = entry
                    .iter()
                    .position(|byte| *byte == b'=')
                    .unwrap_or(entry.len());
                (
                    entry[..split].to_vec(),
                    entry[(split + 1).min(entry.len())..].to_vec(),
                )
            })
            .collect();
        // The root helper enrolls the true leader in a fresh cgroup before
        // any guest user code/fork, owns the attached tty, then drops UID.
        let mut launch = vec![
            b"--internal-record-session".to_vec(),
            session_record.as_os_str().as_bytes().to_vec(),
            vm.user.uid.to_string().into_bytes(),
            vm.user.gid.to_string().into_bytes(),
        ];
        launch.extend(arguments.iter().map(|word| word.as_bytes().to_vec()));
        let spec = shell_supervisor::StartSpec {
            kind: shell_supervisor::StartKind::Shell {
                record: session_record.as_os_str().as_bytes().to_vec(),
                uid: vm.user.uid,
                terminal: terminal_size.map(|size| (size.rows, size.columns)),
            },
            program: b"/usr/local/bin/marsh".to_vec(),
            arguments: launch,
            environment,
            working_directory: working_directory.as_os_str().as_bytes().to_vec(),
        };
        // One supervisor frame, no stock process: bytes stay exact (no UTF-8
        // or host ARG_MAX boundary), and `Started` means containment is
        // published before any user code.
        self.shell_supervisor(&vm.name)?
            .spawn(spec)
            .map_err(|error| match error.kind() {
                // The start may have taken effect without acknowledgement.
                io::ErrorKind::TimedOut | io::ErrorKind::ConnectionAborted => {
                    SbxError::ShellStartUncertain(error.to_string())
                }
                _ => SbxError::Io(error),
            })
    }

    /// Starts a supervisor generation for a VM a test constructed directly.
    #[cfg(test)]
    fn start_test_supervisor(&self, vm: &str) {
        let attachment = self
            .runner
            .spawn_attached(&self.invocation([
                "exec",
                "-i",
                "-u",
                "root",
                "-w",
                "/",
                vm,
                "/usr/local/bin/marsh",
                "--internal-supervisor",
            ]))
            .unwrap();
        let generation = self.shell_generation.fetch_add(1, Ordering::Relaxed);
        let lease =
            shell_supervisor::Supervisor::start(attachment, generation, Duration::from_secs(5))
                .unwrap();
        self.shell_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(vm.to_owned(), lease);
    }

    fn shell_supervisor(&self, vm: &str) -> Result<Arc<shell_supervisor::Supervisor>, SbxError> {
        self.shell_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(vm)
            .filter(|lease| lease.alive())
            .cloned()
            .ok_or_else(|| SbxError::ShellLeaseLost(vm.to_owned()))
    }

    fn shell_identity_environment(user: &ShellUser) -> [String; 6] {
        [
            format!("HOME={}", user.home.display()),
            format!("USER={}", user.name),
            format!("LOGNAME={}", user.name),
            "SHELL=/usr/local/bin/marsh".to_owned(),
            "BASH_ENV=".to_owned(),
            "ENV=".to_owned(),
        ]
    }

    /// Installs and launches the trusted guest relay beneath the shell user's
    /// private runtime directory.
    ///
    /// # Errors
    /// Rejects unsafe artifacts and fails when setup or launch through stock
    /// SBX fails.
    pub fn launch_shell_relay(
        &self,
        vm: &ReadyShellVm,
        relay_binary: &Path,
        session_id: &str,
    ) -> Result<RelayLaunch, SbxError> {
        validate_token(session_id)?;
        let relay_artifact = self.cached_artifact_identity(relay_binary)?;
        let relay_key = (vm.name.clone(), RELAY_PATH.into());
        let install_lock = self
            .shell_init_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(vm.name.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let installing = install_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let installed = self
            .installed_shell_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&relay_key)
            == Some(&relay_artifact);
        if !installed {
            let install = self.install_binary(
                &vm.name,
                relay_binary,
                "/tmp/marsh-relay.install",
                RELAY_PATH,
            );
            self.invalidate_shell_on_missing(&vm.name, install)?;
            if artifact_identity(relay_binary)? != relay_artifact {
                return Err(SbxError::SourceChanged(relay_binary.to_owned()));
            }
            self.installed_shell_artifacts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(relay_key, relay_artifact);
        }
        drop(installing);
        let runtime = PathBuf::from(format!("/run/marsh/{}/{}", vm.user.uid, session_id));
        let socket_path = runtime.join("s");
        let token_path = runtime.join("t");
        // The trusted relay atomically creates this UUID child beneath the
        // owner-only runtime base. An existing directory fails closed, which
        // also proves the socket and token cannot be inherited stale state.
        // A supervisor child, not a stock exec: the relay's stdio is one
        // attempt on the VM's retained transport.
        let process = self
            .shell_supervisor(&vm.name)?
            .spawn(shell_supervisor::StartSpec {
                kind: shell_supervisor::StartKind::Child {
                    uid: vm.user.uid,
                    gid: vm.user.gid,
                },
                program: RELAY_PATH.as_bytes().to_vec(),
                arguments: vec![
                    socket_path.as_os_str().as_bytes().to_vec(),
                    token_path.as_os_str().as_bytes().to_vec(),
                ],
                environment: Vec::new(),
                working_directory: b"/".to_vec(),
            })
            .map_err(SbxError::Io)?;
        Ok(RelayLaunch {
            process,
            socket_path,
            token_path,
        })
    }

    /// Fence new shell admissions while retaining possibly live source authority.
    /// Existing siblings are not stopped. Recovery requires explicit scope cleanup.
    pub fn quarantine_shell(&self, vm: &ReadyShellVm, grants: Vec<AdmittedHostGrant>) {
        self.uncertain_shell_grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(vm.name.clone())
            .or_default()
            .extend(grants);
    }

    fn reject_quarantined_shell(&self, vm: &str) -> Result<(), SbxError> {
        if self
            .uncertain_shell_grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(vm)
        {
            Err(SbxError::QuarantinedShellVm(vm.into()))
        } else {
            Ok(())
        }
    }

    fn invalidate_shell_on_missing<T>(
        &self,
        vm: &str,
        result: Result<T, SbxError>,
    ) -> Result<T, SbxError> {
        if result.as_ref().is_err_and(missing_vm_error) {
            self.invalidate_shell_cache(vm);
            // Stock reported the VM missing: the cached inventory is stale.
            self.ownership.invalidate();
        }
        result
    }

    fn invalidate_shell_cache(&self, vm: &str) {
        let lease = self
            .shell_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(vm);
        if let Some(process) = lease.and_then(|lease| lease.shutdown()) {
            let _ = terminate_and_reap_process(process, self.retained_process_teardown_timeout);
        }
        self.ready_shells
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(vm);
        self.installed_shell_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(name, _), _| name != vm);
        // Missing/error responses invalidate readiness, NEVER source pins or
        // quarantine. Only explicit verified whole-VM recovery clears those.
    }

    /// Releases this exact acquisition. A mount whose final owner closes stays
    /// mounted (idle) while the VM is warm, so the next shell reuses it without
    /// a stock call. Idle mounts are unmounted only when a new grant conflicts
    /// with them or their source changes, and released at verified VM reset.
    ///
    /// # Errors
    /// Never fails today; the signature keeps unverified-effect reporting.
    pub fn revoke_shell_mounts(&self, mounts: &ShellMounts) -> Result<(), SbxError> {
        for (source, target) in mounts.mounts.iter().rev() {
            let reference = self
                .shell_mount_references
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&(mounts.vm.clone(), source.clone(), target.clone()))
                .cloned();
            // An old acquisition after verified reset cannot affect a new one.
            if let Some(reference) = reference {
                reference
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .owners
                    .remove(&mounts.acquisition);
            }
        }
        Ok(())
    }

    /// Unmounts one idle (mounted, ownerless) shell mount. Returns `Ok(false)`
    /// without a stock effect if it is not idle or the stock lock is busy.
    /// The caller holds (or does not need) the VM transition read fence.
    fn evict_idle_shell_mount(
        &self,
        key: &(String, PathBuf, PathBuf),
        reference: &Arc<ShellMountReference>,
        deadline: Instant,
    ) -> Result<bool, SbxError> {
        let (vm, source, target) = key;
        {
            let mut state = reference
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.phase != ShellMountPhase::Mounted || !state.owners.is_empty() {
                return Ok(false);
            }
            state.phase = ShellMountPhase::Unmounting;
        }
        let (operation, _cleanup) =
            weak_mutex_transition_lock(&self.grant_stock_operation_locks, vm.clone());
        let Ok(operation) = shell_admission_before(|| operation.try_lock(), deadline) else {
            reference
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .phase = ShellMountPhase::Mounted;
            reference.changed.notify_all();
            return Ok(false);
        };
        let result = unmount_spec(source, target).and_then(|spec| {
            self.runner
                .run_bounded(
                    &self.invocation_in(
                        source,
                        [OsStr::new("umount"), OsStr::new(vm), spec.as_os_str()],
                    ),
                    deadline.saturating_duration_since(Instant::now()),
                )
                .map_err(SbxError::from)
                .and_then(|output| Self::require_success("evict idle shell mount", output))
        });
        drop(operation);
        let mut state = reference
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let evicted = result.is_ok();
        if evicted {
            state.phase = ShellMountPhase::Dormant;
            state.pin = None;
        } else {
            state.phase = ShellMountPhase::Uncertain;
            self.uncertain_shell_grants
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(vm.clone())
                .or_default()
                .extend(state.pin.iter().cloned());
        }
        reference.changed.notify_all();
        drop(state);
        if evicted {
            let mut references = self
                .shell_mount_references
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if references
                .get(key)
                .is_some_and(|current| Arc::ptr_eq(current, reference))
            {
                references.remove(key);
            }
        }
        result.map(|()| true)
    }

    /// Unmounts one idle (no job, no session pin) Kit grant mount. Busy locks
    /// skip eviction without a stock effect; the caller's overlap check then
    /// rejects as before.
    fn evict_idle_grant(&self, key: &GrantKey, vm_fence_held: bool) -> Result<bool, SbxError> {
        let vm = &key.0;
        let (vm_transition, _vm_cleanup) =
            weak_rw_transition_lock(&self.grant_vm_transition_locks, vm.clone());
        let _vm_guard = if vm_fence_held {
            None
        } else {
            match vm_transition.try_read() {
                Ok(guard) => Some(guard),
                Err(_) => return Ok(false),
            }
        };
        let (transition, _cleanup) =
            weak_mutex_transition_lock(&self.grant_transition_locks, key.clone());
        let Ok(_transition) = transition.try_lock() else {
            return Ok(false);
        };
        self.evict_idle_grant_locked(key)
    }

    fn idle_grant_mismatch(&self, key: &GrantKey, grant: &AdmittedHostGrant) -> bool {
        self.grant_mount_references
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(key)
            .is_some_and(|reference| {
                reference.active_jobs == 0
                    && reference.pinned_sessions.is_empty()
                    && (reference.identity != grant.identity
                        || reference.access != grant.access
                        || reference.source_record
                            != source_chain::SourceRecord::new(&grant.source, &grant.chain))
            })
    }

    /// Caller holds the VM read fence and this key's transition lock.
    fn evict_idle_grant_locked(&self, key: &GrantKey) -> Result<bool, SbxError> {
        let (vm, source) = key;
        let vm_source = {
            let references = self
                .grant_mount_references
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match references.get(key) {
                Some(reference)
                    if reference.active_jobs == 0 && reference.pinned_sessions.is_empty() =>
                {
                    reference.vm_source.clone()
                }
                _ => return Ok(false),
            }
        };
        let spec = unmount_spec(source, &vm_source)?;
        let (stock_operation, _stock_cleanup) =
            weak_mutex_transition_lock(&self.grant_stock_operation_locks, vm.clone());
        let _stock_operation = stock_operation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = self
            .run_os_in(
                source,
                [OsStr::new("umount"), OsStr::new(vm), spec.as_os_str()],
            )
            .and_then(|output| Self::require_success("evict idle grant", output));
        if let Err(error) = result {
            self.quarantined_workers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(vm.clone());
            return Err(error);
        }
        self.grant_mount_references
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key);
        Ok(true)
    }

    /// Before an overlap check, unmount idle retained mounts (shell or Kit)
    /// whose sources conflict with the requested batch. Live mounts are never
    /// touched, so a conflict with them is still rejected by the caller.
    fn evict_idle_conflicts(
        &self,
        grants: &[AdmittedHostGrant],
        held_vm: Option<&str>,
        deadline: Instant,
    ) -> Result<(), SbxError> {
        let batch = grants
            .iter()
            .map(|grant| source_chain::SourceRecord::new(&grant.source, &grant.chain))
            .collect::<Vec<_>>();
        let conflicts = |record: &source_chain::SourceRecord| {
            batch.iter().any(|requested| requested.conflicts(record))
        };
        let shell = self
            .shell_mount_references
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(_, reference)| conflicts(&reference.source_record))
            .map(|(key, reference)| (key.clone(), Arc::clone(reference)))
            .collect::<Vec<_>>();
        for (key, reference) in shell {
            let (transition, _cleanup) =
                weak_rw_transition_lock(&self.grant_vm_transition_locks, key.0.clone());
            let _guard = if held_vm == Some(key.0.as_str()) {
                None
            } else {
                match transition.try_read() {
                    Ok(guard) => Some(guard),
                    Err(_) => continue,
                }
            };
            self.evict_idle_shell_mount(&key, &reference, deadline)?;
        }
        let kit = self
            .grant_mount_references
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(_, reference)| {
                reference.active_jobs == 0
                    && reference.pinned_sessions.is_empty()
                    && conflicts(&reference.source_record)
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in kit {
            self.evict_idle_grant(&key, held_vm == Some(key.0.as_str()))?;
        }
        Ok(())
    }

    /// Pulls the immutable job image into the selected warm VM.
    ///
    /// # Errors
    /// Returns an error when the reference is mutable or Docker cannot cache it.
    pub fn prewarm_image(&self, vm: &ReadyKitVm, image: &OciImage) -> Result<(), SbxError> {
        let reference = OciImage::parse(image.as_str().to_owned())?;
        let cache = self
            .kit_image_cache
            .as_deref()
            .filter(|_| self.published_image_cache)
            .filter(|_| published_image_digest(&reference).is_some());
        if let Some(cache) = cache
            && let Some(archive) = cached_published_image(cache, &reference)
        {
            // A verified archive replaces the registry pull. Its use is
            // checked: the loaded name must resolve to the pinned digest.
            let loaded = self
                .load_nested_archive(&vm.name, &archive)
                .and_then(|()| self.published_image_id(&vm.name, &reference));
            if loaded.as_ref().ok().map(OciImage::as_str) == published_image_digest(&reference) {
                return Ok(());
            }
            evict_published_image(cache, &reference);
        }
        Self::require_success(
            "pull immutable job image",
            self.run([
                "exec",
                "-u",
                "root",
                &vm.name,
                "docker",
                "pull",
                reference.as_str(),
            ])?,
        )?;
        let inspected = self.run([
            "exec",
            "-u",
            "root",
            &vm.name,
            "docker",
            "image",
            "inspect",
            "--format",
            "{{.Id}}",
            reference.as_str(),
        ])?;
        let image_id = String::from_utf8_lossy(&inspected.stdout).trim().to_owned();
        if !inspected.succeeded() || !valid_image_id(&image_id) {
            return Err(SbxError::ImageVerificationFailed);
        }
        // Content-addressed stores name the pulled image by its digest; only
        // those archives can be checked on use, so only they are cached.
        if let Some(cache) = cache
            && published_image_digest(&reference) == Some(image_id.as_str())
        {
            self.save_published_image(&vm.name, &reference, cache);
        }
        Ok(())
    }

    /// Whether starting a fresh VM for this Kit will pull its job image from
    /// a registry: a published image with no verified archive in this home's
    /// Kit image cache. Local-source Kits build their image instead.
    #[must_use]
    pub fn kit_image_download_needed(&self, spec: &KitVmSpec) -> bool {
        let NativeKitLocation::ImmutableOci(image) = &spec.workload_kit.location else {
            return false;
        };
        if published_image_digest(image).is_none() {
            return false;
        }
        match self
            .kit_image_cache
            .as_deref()
            .filter(|_| self.published_image_cache)
        {
            Some(cache) => cached_published_image(cache, image).is_none(),
            None => true,
        }
    }

    /// Whether creating this shell VM will probably download its template:
    /// the VM does not exist, the template is a registry image, and stock
    /// SBX's image store does not list that template's tag. When the store
    /// cannot be listed this answers true; a needless hint is harmless.
    #[must_use]
    pub fn shell_image_download_needed(&self, spec: &ShellVmSpec) -> bool {
        let Ok(template) = ShellTemplateReference::parse(spec.image.as_str()) else {
            return false;
        };
        if template.requires_local_authority()
            || !matches!(self.classify_vm(&spec.name, false), Ok(VmClass::Absent))
        {
            return false;
        }
        let digest_tag = template.digest().replace(':', "-");
        let tag = template.tag().map(str::to_owned);
        let listed = self
            .run_bounded_os(
                "list templates",
                Duration::from_secs(10),
                ["template", "ls", "--json"],
            )
            .ok()
            .filter(CommandOutput::succeeded)
            .and_then(|output| serde_json::from_slice::<serde_json::Value>(&output.stdout).ok());
        let Some(listed) = listed else {
            return true;
        };
        !template_list_rows(&listed).is_some_and(|images| {
            images.iter().any(|image| {
                image
                    .get("tag")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|candidate| {
                        candidate == digest_tag || tag.as_deref() == Some(candidate)
                    })
            })
        })
    }

    fn published_image_id(&self, vm: &str, reference: &OciImage) -> Result<OciImage, SbxError> {
        let inspected = self.run([
            "exec",
            "-u",
            "root",
            vm,
            "docker",
            "image",
            "inspect",
            "--format",
            "{{.Id}}",
            reference.as_str(),
        ])?;
        let image_id = String::from_utf8_lossy(&inspected.stdout).trim().to_owned();
        if !inspected.succeeded() || !valid_image_id(&image_id) {
            return Err(SbxError::ImageVerificationFailed);
        }
        OciImage::parse(image_id).map_err(SbxError::from)
    }

    /// Save a just-pulled published image into the Kit image cache in the
    /// background, so the next cold VM loads it instead of pulling. Failures
    /// only leave the cache without the entry.
    fn save_published_image(&self, vm: &str, reference: &OciImage, cache: &Path) {
        let Some(key) = published_image_digest(reference)
            .and_then(|digest| digest.strip_prefix("sha256:"))
            .map(str::to_owned)
        else {
            return;
        };
        if !self
            .published_image_saves
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.clone())
        {
            return;
        }
        let Some(tagged) = published_cache_tag(reference) else {
            self.published_image_saves
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&key);
            return;
        };
        // Docker saves a digest-only image without a name, so it would load
        // dangling. Saved under a tag in the same repository, it loads with
        // that name and its digest reference resolves to the same target.
        let invocation = self.invocation([
            "exec",
            "-u",
            "root",
            vm,
            "sh",
            "-c",
            "docker image tag \"$1\" \"$2\" && exec docker image save \"$2\"",
            "marsh-save",
            reference.as_str(),
            &tagged,
        ]);
        let claimed = key.clone();
        let runner = Arc::clone(&self.runner);
        let saves = Arc::clone(&self.published_image_saves);
        let cache = cache.to_owned();
        let reference = reference.clone();
        let timeout = self.preparation_timeouts.load;
        let spawned = thread::Builder::new()
            .name("marsh-kit-image-save".into())
            .spawn(move || {
                let _ = store_published_image(
                    runner.as_ref(),
                    &invocation,
                    &cache,
                    &reference,
                    timeout,
                );
                saves
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&key);
            });
        if spawned.is_err() {
            // The closure (and its claim) is dropped; release the claim.
            self.published_image_saves
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&claimed);
        }
    }

    /// Returns the worker VMs holding exact source pins for one shell session.
    #[must_use]
    pub fn pinned_session_vms(&self, session: &str) -> Vec<String> {
        self.grant_mount_references
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(_, reference)| reference.pinned_sessions.contains_key(session))
            .map(|((vm, _), _)| vm.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn session_grant_lifecycle(&self, session: &str) -> Arc<Mutex<bool>> {
        self.session_grant_lifecycles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(session.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(false)))
            .clone()
    }

    /// Closes one shell session against new grant pins, then releases every
    /// pin that completed before the close. Holding the session lifecycle
    /// guard through the snapshot and release prevents a concurrent lazy job
    /// preparation from adding a pin after teardown has passed that VM.
    ///
    /// # Errors
    /// Returns an error if an exact final-reference mount cannot be revoked.
    pub fn close_session_grants(&self, session: &str) -> Result<(), SbxError> {
        validate_token(session)?;
        let lifecycle = self.session_grant_lifecycle(session);
        let mut closed = lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *closed = true;
        let mut failure = None;
        for vm in self.pinned_session_vms(session) {
            if let Err(error) = self.release_session_grants(&vm, session) {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Bounded shell-only close. Busy transitions or failed stock control retain
    /// exact pins and quarantine admission; no shared worker is stopped here.
    ///
    /// # Errors
    /// Returns cleanup uncertainty when a fence cannot be acquired or revocation
    /// cannot complete before the aggregate deadline.
    pub fn close_shell_session_grants(&self, session: &str) -> Result<(), SbxError> {
        validate_token(session)?;
        let busy = || SbxError::LifecycleDeadline {
            operation: "shell session grant cleanup admission",
        };
        let lifecycle = self.session_grant_lifecycle(session);
        let mut closed = lifecycle.try_lock().map_err(|_| busy())?;
        *closed = true;
        // Another session's mount or pin transition on the same shared Kit VM
        // (for example the same project) holds these fences for one stock
        // call. Wait a bounded moment for it instead of quarantining the
        // shared worker over an ordinary overlap.
        let deadline = Instant::now() + SHELL_CLOSE_TRANSITION_WAIT;
        let keys = shell_admission_before(|| self.grant_mount_references.try_lock(), deadline)
            .map_err(|_| busy())?
            .iter()
            .filter(|(_, reference)| reference.pinned_sessions.contains_key(session))
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for (vm, source) in keys {
            let release = (|| {
                let (transition, _vm_cleanup) =
                    weak_rw_transition_lock(&self.grant_vm_transition_locks, vm.clone());
                let _vm_guard = shell_admission_before(|| transition.try_read(), deadline)
                    .map_err(|_| busy())?;
                let key = (vm.clone(), source.clone());
                let (transition, _key_cleanup) =
                    weak_mutex_transition_lock(&self.grant_transition_locks, key.clone());
                let _key_guard = shell_admission_before(|| transition.try_lock(), deadline)
                    .map_err(|_| busy())?;
                let mut references =
                    shell_admission_before(|| self.grant_mount_references.try_lock(), deadline)
                        .map_err(|_| busy())?;
                let Some(reference) = references.get_mut(&key) else {
                    return Ok(());
                };
                if !reference.pinned_sessions.contains_key(session) {
                    return Ok(());
                }
                // The mount stays while the VM is warm; only the pin goes.
                reference.pinned_sessions.remove(session);
                Ok(())
            })();
            if let Err(error) = release {
                self.quarantined_workers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(vm);
                return Err(error);
            }
        }
        Ok(())
    }

    /// Releases one session's exact pins in a worker VM. Active jobs keep their
    /// own references, and the stock mount is revoked only after the final pin
    /// and active job have both gone away.
    ///
    /// # Errors
    /// Returns an error if the VM/session identity is invalid or an exact final
    /// mount cannot be revoked.
    pub fn release_session_grants(&self, vm: &str, session: &str) -> Result<(), SbxError> {
        validate_name(vm)?;
        validate_token(session)?;
        let keys = self
            .grant_mount_references
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|((candidate, _), reference)| {
                candidate == vm && reference.pinned_sessions.contains_key(session)
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        let mut failure = None;
        for (_, source) in keys {
            if let Err(error) = self.release_session_grant(vm, session, &source) {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    fn release_session_grant(
        &self,
        vm: &str,
        session: &str,
        source: &Path,
    ) -> Result<(), SbxError> {
        let release = (|| {
            let key = (vm.to_owned(), source.to_owned());
            let (vm_transition, _vm_cleanup) =
                weak_rw_transition_lock(&self.grant_vm_transition_locks, vm.to_owned());
            let _vm_transition = vm_transition
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (transition, _cleanup) =
                weak_mutex_transition_lock(&self.grant_transition_locks, key.clone());
            let _transition = transition
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut references = self
                .grant_mount_references
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(reference) = references.get_mut(&key) else {
                return Ok(());
            };
            if !reference.pinned_sessions.contains_key(session) {
                return Ok(());
            }
            // The mount stays while the VM is warm; only the pin goes.
            reference.pinned_sessions.remove(session);
            Ok(())
        })();
        if let Err(error) = release {
            // The lifecycle write fence used by stop must be acquired only
            // after the read/key/external transition guards above are gone.
            let _ = self.stop_quarantined_name(vm);
            return Err(error);
        }
        Ok(())
    }

    /// Makes the exact native workload available for fresh job containers.
    ///
    /// # Errors
    /// Published workloads are pulled by digest. Local v3 source is built by
    /// stock Docker, verified against stock SBX, and loaded by immutable ID.
    pub fn prewarm_workload(&self, vm: &ReadyKitVm) -> Result<(), SbxError> {
        if vm.kit_ref.starts_with("local-v3:") {
            self.seed_local_workload(vm)
        } else {
            self.prewarm_image(vm, vm.job_image())
        }
    }

    fn seed_local_workload(&self, vm: &ReadyKitVm) -> Result<(), SbxError> {
        let local_tag = self
            .local_resolutions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&vm.kit_ref)
            .filter(|resolution| resolution.image_id == vm.job_image)
            .map(|resolution| resolution.local_tag.clone())
            .ok_or(SbxError::ImageVerificationFailed)?;
        if self.inspect_nested_image(&vm.name, &local_tag)? == vm.job_image {
            Ok(())
        } else {
            Err(SbxError::ImageVerificationFailed)
        }
    }

    /// Acquires authorized host roots at stable opaque VM paths. Concurrent
    /// attempts sharing the exact source identity and access reuse one stock
    /// SBX allowed path; each attempt retains an independent reference.
    ///
    /// # Errors
    /// Fails for unsafe paths, ownership/type changes, or mount failures.
    fn prepare_grants(
        &self,
        vm: &ReadyKitVm,
        attempt: &str,
        grants: &[AdmittedHostGrant],
    ) -> Result<PreparedGrants, SbxError> {
        validate_token(attempt)?;
        if grants.is_empty() {
            return Err(SbxError::NoGrants);
        }
        {
            for grant in grants {
                grant.verify_path()?;
            }
            let mut prepared = Vec::with_capacity(grants.len());
            for grant in grants {
                if !safe_absolute(&grant.target) {
                    return Err(self.grant_preparation_error(
                        vm,
                        attempt,
                        &prepared,
                        SbxError::UnsafePath(grant.target.clone()),
                    ));
                }
                match self.acquire_shared_grant(vm, grant) {
                    Ok(mount) => prepared.push(mount),
                    Err(SbxError::GrantPreparationFailed {
                        cause,
                        rollback_complete: current_complete,
                    }) => {
                        let prior = self.grant_preparation_error(vm, attempt, &prepared, *cause);
                        return match prior {
                            SbxError::GrantPreparationFailed {
                                cause,
                                rollback_complete,
                            } => Err(SbxError::GrantPreparationFailed {
                                cause,
                                rollback_complete: rollback_complete && current_complete,
                            }),
                            other => Err(other),
                        };
                    }
                    Err(error) => {
                        return Err(self.grant_preparation_error(vm, attempt, &prepared, error));
                    }
                }
            }
            Ok(PreparedGrants {
                vm: vm.name.clone(),
                attempt: attempt.to_owned(),
                worker_generation: 0,
                mounts: prepared,
            })
        }
    }

    /// Pins exact verified host grants for one shell session without creating a
    /// job reference. This is used by prewarm so the next invocation reuses the
    /// already prepared stock mounts.
    ///
    /// # Errors
    /// Fails if a source changes, differs from an existing exact pin, or stock
    /// SBX cannot prepare the mount.
    pub fn prepare_session_grants(
        &self,
        vm: &ReadyKitVm,
        session: &str,
        grants: &[AdmittedHostGrant],
    ) -> Result<SessionGrantPins, SbxError> {
        validate_token(session)?;
        let lifecycle = self.session_grant_lifecycle(session);
        let closed = lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *closed {
            return Err(SbxError::ClosedSession(session.to_owned()));
        }
        self.reject_quarantined(&vm.name)?;
        {
            for grant in grants {
                grant.verify_path()?;
            }
            let mut newly_pinned = Vec::new();
            for grant in grants {
                match self.acquire_session_grant(vm, session, grant) {
                    Ok(true) => newly_pinned.push(grant.source.clone()),
                    Ok(false) => {}
                    Err(error) => {
                        let mut rollback_complete = !matches!(
                            &error,
                            SbxError::GrantPreparationFailed {
                                rollback_complete: false,
                                ..
                            }
                        );
                        for source in newly_pinned.iter().rev() {
                            rollback_complete &= self
                                .release_session_grant(&vm.name, session, source)
                                .is_ok();
                        }
                        if !rollback_complete && self.reject_quarantined(&vm.name).is_ok() {
                            let _ = self.stop_quarantined_name(&vm.name);
                        }
                        return Err(SbxError::GrantPreparationFailed {
                            cause: Box::new(error),
                            rollback_complete,
                        });
                    }
                }
            }
            Ok(SessionGrantPins {
                vm: vm.name.clone(),
                session: session.to_owned(),
                sources: newly_pinned,
            })
        }
    }

    /// Rolls back only the session pins newly acquired by one preparation.
    ///
    /// # Errors
    /// Returns an error if an exact final-reference mount cannot be revoked.
    pub fn rollback_session_grants(&self, pins: &SessionGrantPins) -> Result<(), SbxError> {
        let mut failure = None;
        for source in pins.sources.iter().rev() {
            if let Err(error) = self.release_session_grant(&pins.vm, &pins.session, source) {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    fn acquire_session_grant(
        &self,
        vm: &ReadyKitVm,
        session: &str,
        grant: &AdmittedHostGrant,
    ) -> Result<bool, SbxError> {
        grant.verify_path()?;
        let key = (vm.name.clone(), grant.source.clone());
        let (vm_transition, _vm_cleanup) =
            weak_rw_transition_lock(&self.grant_vm_transition_locks, vm.name.clone());
        let _vm_transition = vm_transition
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (transition, _cleanup) =
            weak_mutex_transition_lock(&self.grant_transition_locks, key.clone());
        let _transition = transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.reject_quarantined(&vm.name)?;
        if self.idle_grant_mismatch(&key, grant) {
            // A retained idle mount of a replaced source: unmount, then remount.
            self.evict_idle_grant_locked(&key)?;
        }
        let mut references = self
            .grant_mount_references
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(reference) = references.get_mut(&key) {
            if reference.identity != grant.identity || reference.access != grant.access {
                return Err(SbxError::SourceChanged(grant.source.clone()));
            }
            verify_pinned_grant_handles(reference, &grant.source)?;
            grant.verify_path()?;
            return Ok(reference
                .pinned_sessions
                .insert(session.to_owned(), Arc::clone(&grant.handle))
                .is_none());
        }

        drop(references);
        self.evict_idle_conflicts(
            std::slice::from_ref(grant),
            Some(&vm.name),
            Instant::now() + self.preparation_timeouts.sbx,
        )?;
        self.preflight_host_grants(std::slice::from_ref(grant))?;
        let vm_source = self.mount_shared_grant(vm, grant)?;
        self.grant_mount_references
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                key,
                GrantMountReference {
                    identity: grant.identity.clone(),
                    source_record: source_chain::SourceRecord::new(&grant.source, &grant.chain),
                    access: grant.access,
                    vm_source,
                    active_jobs: 0,
                    pinned_sessions: BTreeMap::from([(
                        session.to_owned(),
                        Arc::clone(&grant.handle),
                    )]),
                },
            );
        Ok(true)
    }

    fn acquire_shared_grant(
        &self,
        vm: &ReadyKitVm,
        grant: &AdmittedHostGrant,
    ) -> Result<PreparedMount, SbxError> {
        grant.verify_path()?;
        let key = (vm.name.clone(), grant.source.clone());
        let (vm_transition, _vm_cleanup) =
            weak_rw_transition_lock(&self.grant_vm_transition_locks, vm.name.clone());
        let _vm_transition = vm_transition
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (transition, _cleanup) =
            weak_mutex_transition_lock(&self.grant_transition_locks, key.clone());
        let _transition = transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.reject_quarantined(&vm.name)?;
        if self.idle_grant_mismatch(&key, grant) {
            // A retained idle mount of a replaced source: unmount, then remount.
            self.evict_idle_grant_locked(&key)?;
        }
        let mut references = self
            .grant_mount_references
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(reference) = references.get_mut(&key) {
            if reference.identity != grant.identity || reference.access != grant.access {
                return Err(SbxError::SourceChanged(grant.source.clone()));
            }
            verify_pinned_grant_handles(reference, &grant.source)?;
            grant.verify_path()?;
            reference.active_jobs += 1;
            return Ok(PreparedMount {
                host_source: grant.source.clone(),
                vm_source: reference.vm_source.clone(),
                target: grant.target.clone(),
                access: grant.access,
            });
        }

        drop(references);
        self.evict_idle_conflicts(
            std::slice::from_ref(grant),
            Some(&vm.name),
            Instant::now() + self.preparation_timeouts.sbx,
        )?;
        self.preflight_host_grants(std::slice::from_ref(grant))?;
        let vm_source = self.mount_shared_grant(vm, grant)?;
        self.grant_mount_references
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                key,
                GrantMountReference {
                    identity: grant.identity.clone(),
                    source_record: source_chain::SourceRecord::new(&grant.source, &grant.chain),
                    access: grant.access,
                    vm_source: vm_source.clone(),
                    active_jobs: 1,
                    pinned_sessions: BTreeMap::new(),
                },
            );
        Ok(PreparedMount {
            host_source: grant.source.clone(),
            vm_source,
            target: grant.target.clone(),
            access: grant.access,
        })
    }

    fn run_source_control<I, S>(
        &self,
        directory: &Path,
        arguments: I,
    ) -> Result<CommandOutput, SbxError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Ok(self.runner.run_bounded(
            &self.invocation_in(directory, arguments),
            self.preparation_timeouts.sbx,
        )?)
    }

    fn mount_shared_grant(
        &self,
        vm: &ReadyKitVm,
        grant: &AdmittedHostGrant,
    ) -> Result<PathBuf, SbxError> {
        let (stock_operation, _stock_cleanup) =
            weak_mutex_transition_lock(&self.grant_stock_operation_locks, vm.name.clone());
        let _stock_operation = stock_operation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let vm_source = shared_grant_path(&grant.source, &grant.identity);
        let mode = match grant.access {
            MountAccess::ReadOnly => "ro",
            MountAccess::ReadWrite => "rw",
        };
        let spec = mount_spec(&grant.source, &vm_source, mode)?;
        let mounted = self
            .run_source_control(
                &grant.source,
                [OsStr::new("mount"), OsStr::new(&vm.name), spec.as_os_str()],
            )
            .and_then(|output| {
                Self::require_success("mount shared grant", output)?;
                grant.verify_path()
            });
        if let Err(error) = mounted {
            if matches!(&error, SbxError::Io(source) if marsh_runtime::command_not_started(source))
            {
                return Err(error);
            }
            let cleanup = unmount_spec(&grant.source, &vm_source).and_then(|spec| {
                Self::require_success(
                    "rollback shared grant",
                    self.run_source_control(
                        &grant.source,
                        [OsStr::new("umount"), OsStr::new(&vm.name), spec.as_os_str()],
                    )?,
                )
            });
            if stale_shared_mount_error(&error) && cleanup.is_ok() {
                let retried = self
                    .run_source_control(
                        &grant.source,
                        [OsStr::new("mount"), OsStr::new(&vm.name), spec.as_os_str()],
                    )
                    .and_then(|output| {
                        Self::require_success("mount shared grant after stale cleanup", output)?;
                        grant.verify_path()
                    });
                if retried.is_ok() {
                    return Ok(vm_source);
                }
                let retry_error = retried.expect_err("retry checked above");
                let retry_cleanup = unmount_spec(&grant.source, &vm_source).and_then(|spec| {
                    Self::require_success(
                        "rollback retried shared grant",
                        self.run_source_control(
                            &grant.source,
                            [OsStr::new("umount"), OsStr::new(&vm.name), spec.as_os_str()],
                        )?,
                    )
                });
                return Err(SbxError::GrantPreparationFailed {
                    cause: Box::new(retry_error),
                    rollback_complete: retry_cleanup.is_ok(),
                });
            }
            return Err(SbxError::GrantPreparationFailed {
                cause: Box::new(error),
                rollback_complete: cleanup.is_ok(),
            });
        }
        Ok(vm_source)
    }

    /// Prepares job grants bound to the currently retained worker lease.
    ///
    /// # Errors
    /// Fails when no exact live lease exists or grant preparation fails.
    pub fn prepare_job_grants(
        &self,
        vm: &ReadyKitVm,
        session: &str,
        attempt: &str,
        grants: &[AdmittedHostGrant],
    ) -> Result<PreparedGrants, SbxError> {
        let generation = self.current_worker_generation(&vm.name)?;
        self.prepare_session_grants(vm, session, grants)?;
        let mut prepared = self.prepare_grants(vm, attempt, grants)?;
        prepared.worker_generation = generation;
        Ok(prepared)
    }

    /// Opens one attempt on the retained trusted worker transport.
    ///
    /// # Errors
    /// Returns an error if the attached stock-SBX process cannot start.
    pub fn launch_worker(
        &self,
        vm: &ReadyKitVm,
        grants: &PreparedGrants,
        attempt: &str,
        spec: marsh_contracts::JobSpec,
    ) -> Result<WorkerChannel, SbxError> {
        validate_token(attempt)?;
        let (vm_transition, _vm_cleanup) =
            weak_rw_transition_lock(&self.grant_vm_transition_locks, vm.name.clone());
        let vm_transition_guard = vm_transition
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.reject_quarantined(&vm.name)?;
        if grants.vm != vm.name
            || grants.attempt != attempt
            || self.current_worker_generation(&vm.name)? != grants.worker_generation
        {
            return Err(SbxError::WorkerLeaseLost(vm.name.clone()));
        }
        let worker = self
            .worker_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&vm.name)
            .cloned()
            .ok_or_else(|| SbxError::WorkerLeaseLost(vm.name.clone()))?;
        if !worker.alive.load(Ordering::Acquire) {
            return Err(SbxError::WorkerLeaseLost(vm.name.clone()));
        }
        // The worker enforces each attempt's output budget before framing, so
        // this route is bounded by that admitted budget. Keeping dispatch
        // nonblocking prevents one slow client from stalling sibling jobs or
        // liveness pongs on the single retained transport.
        let byte_limit = usize::try_from(spec.resources.output_bytes)
            .unwrap_or(usize::MAX)
            .min(RETAINED_RESPONSE_BUFFER_BYTES);
        let (send, receive) = mpsc::channel();
        {
            let mut routes = worker
                .routes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if routes.contains_key(attempt) {
                return Err(SbxError::WorkerLeaseLost(vm.name.clone()));
            }
            routes.insert(
                attempt.to_owned(),
                AttemptRoute {
                    send,
                    queued_bytes: 0,
                    byte_limit,
                    overflowing: false,
                },
            );
        }
        worker.active.fetch_add(1, Ordering::AcqRel);
        let request = WorkerRequest::Start {
            attempt: attempt.to_owned(),
            generation: worker.generation,
            spec,
        };
        let start_write = write_frame(
            &mut *worker
                .input
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            &request,
        );
        if let Err(error) = start_write {
            worker
                .routes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(attempt);
            worker.active.fetch_sub(1, Ordering::AcqRel);
            // `write_frame` flushes after writing the complete frame. A
            // failure here therefore cannot prove that the worker did not
            // accept Start and create a container. Publish quarantine before
            // releasing the shared lifecycle fence, then stop the exact VM
            // after releasing it so stop can acquire the exclusive fence.
            self.quarantined_workers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(vm.name.clone());
            drop(vm_transition_guard);
            let stop = self.stop_quarantined(vm);
            let detail = match stop {
                Ok(()) => error.to_string(),
                Err(stop_error) => format!("{error}; quarantine stop failed: {stop_error}"),
            };
            return Err(SbxError::AmbiguousWorkerStart {
                vm: vm.name.clone(),
                detail,
            });
        }
        Ok(WorkerChannel {
            attempt: attempt.to_owned(),
            responses: WorkerResponses {
                attempt: attempt.to_owned(),
                worker: Arc::clone(&worker),
                receive,
            },
            worker,
            finished: false,
        })
    }

    /// Releases this attempt's exact references. The shared stock-SBX mount is
    /// retained while the VM is warm and released when the VM is retired.
    ///
    /// # Errors
    /// Any uncertain revocation is returned so the caller can quarantine VM.
    pub fn revoke_grants(&self, grants: &PreparedGrants) -> Result<(), SbxError> {
        for mount in grants.mounts.iter().rev() {
            let key = (grants.vm.clone(), mount.host_source.clone());
            let (vm_transition, _vm_cleanup) =
                weak_rw_transition_lock(&self.grant_vm_transition_locks, grants.vm.clone());
            let _vm_transition = vm_transition
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (transition, _cleanup) =
                weak_mutex_transition_lock(&self.grant_transition_locks, key.clone());
            let _transition = transition
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut references = self
                .grant_mount_references
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(reference) = references.get_mut(&key) else {
                return Err(SbxError::UnknownShellMount(mount.vm_source.clone()));
            };
            if reference.vm_source != mount.vm_source || reference.access != mount.access {
                return Err(SbxError::UnknownShellMount(mount.vm_source.clone()));
            }
            if reference.active_jobs == 0 {
                return Err(SbxError::UnknownShellMount(mount.vm_source.clone()));
            }
            // The mount stays while the VM is warm (released at retire).
            reference.active_jobs -= 1;
        }
        Ok(())
    }

    /// Stops exactly one known worker VM after the control plane quarantines it.
    ///
    /// # Errors
    /// Returns an error when stock SBX cannot stop the VM.
    pub fn stop_quarantined(&self, vm: &ReadyKitVm) -> Result<(), SbxError> {
        self.stop_quarantined_name(&vm.name)
    }

    fn stop_quarantined_name(&self, vm: &str) -> Result<(), SbxError> {
        self.quarantined_workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(vm.to_owned());
        let (vm_transition, _vm_cleanup) =
            weak_rw_transition_lock(&self.grant_vm_transition_locks, vm.to_owned());
        let _vm_transition = vm_transition
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A transition that began before the first publication can finish
        // before this write fence is acquired. Reassert quarantine while the
        // fence excludes every later transition and retained Start.
        self.quarantined_workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(vm.to_owned());
        let (stock_operation, _stock_cleanup) =
            weak_mutex_transition_lock(&self.grant_stock_operation_locks, vm.to_owned());
        let _stock_operation = stock_operation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(lease) = self
            .worker_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(vm)
        {
            lease.alive.store(false, Ordering::Release);
            let process = std::mem::replace(
                &mut *lease
                    .process
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                Box::new(ReleasedAttachedProcess),
            );
            terminate_and_reap_process(process, self.retained_process_teardown_timeout)?;
        }
        self.worker_generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(vm);
        // Only ever stop this daemon's recorded, currently present UUID.
        // Local stock SBX addresses sandboxes by name; the fresh
        // classification fences that name to the recorded UUID.
        match self.classify_vm(vm, true)? {
            VmClass::Absent => return Ok(()),
            VmClass::Foreign => return Err(SbxError::ForeignVm(vm.to_owned())),
            VmClass::Owned { .. } => {}
        }
        self.ownership.invalidate();
        Self::require_success("stop quarantined worker", self.run(["stop", vm])?)
    }

    fn reject_quarantined(&self, vm: &str) -> Result<(), SbxError> {
        if self
            .quarantined_workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(vm)
        {
            return Err(SbxError::QuarantinedVm(vm.to_owned()));
        }
        Ok(())
    }

    fn current_worker_generation(&self, vm: &str) -> Result<u64, SbxError> {
        self.reject_quarantined(vm)?;
        let leases = self
            .worker_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(lease) = leases.get(vm) else {
            return Err(SbxError::WorkerLeaseLost(vm.to_owned()));
        };
        if !lease.alive.load(Ordering::Acquire)
            || lease
                .process
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .try_wait()?
                .is_some()
        {
            return Err(SbxError::WorkerLeaseLost(vm.to_owned()));
        }
        self.worker_generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(vm)
            .copied()
            .ok_or_else(|| SbxError::WorkerLeaseLost(vm.to_owned()))
    }

    fn open_lifecycle_workspace(spec: &KitVmSpec) -> Result<AdmittedHostGrant, SbxError> {
        let grant = AdmittedHostGrant::open(
            spec.lifecycle_workspace.clone(),
            spec.lifecycle_workspace.clone(),
            MountAccess::ReadWrite,
        )?;
        Ok(grant)
    }

    #[cfg(test)]
    fn create_published_vm(&self, spec: &KitVmSpec) -> Result<(), SbxError> {
        self.create_published_vm_with_grant(spec, &Self::open_lifecycle_workspace(spec)?)
    }

    fn create_published_vm_with_grant(
        &self,
        spec: &KitVmSpec,
        before: &AdmittedHostGrant,
    ) -> Result<(), SbxError> {
        self.preflight_host_grants(std::slice::from_ref(before))?;
        self.ownership.invalidate();
        let outcome = (|| {
            let mut selected_home_environment = OsString::from("MARSH_SELECTED_HOME=");
            selected_home_environment.push(spec.lifecycle_workspace.as_os_str());
            let arguments = vec![
                OsString::from("create"),
                OsString::from("--name"),
                OsString::from(&spec.name),
                OsString::from("--pull"),
                OsString::from("missing"),
                OsString::from("--skills"),
                OsString::from("off"),
                OsString::from("-e"),
                selected_home_environment,
                // Stock ResolveReference stats an unprefixed argument before its
                // OCI heuristic. Force immutable Kit authority despite a hostile
                // cwd containing a directory named like registry/repo@sha256:...
                OsString::from(format!("oci://{}", spec.workload_kit.identity())),
                spec.lifecycle_workspace.as_os_str().to_os_string(),
            ];
            let output =
                self.run_bounded_os("create kit VM", self.preparation_timeouts.sbx, arguments);
            Self::require_success("create kit VM", output?)?;
            before.verify_path()?;
            self.adopt_created(&spec.name)?;
            Ok(())
        })();
        let submitted = !matches!(&outcome, Err(SbxError::Io(error)) if marsh_runtime::command_not_started(error));
        let result = outcome;
        if submitted && result.is_err() {
            self.quarantined_workers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(spec.name.clone());
        }
        result
    }

    fn create_local_vm(
        &self,
        spec: &KitVmSpec,
        source: &Path,
        before: &AdmittedHostGrant,
        rebuild: bool,
    ) -> Result<CommandOutput, SbxError> {
        self.preflight_host_grants(std::slice::from_ref(before))?;
        self.ownership.invalidate();
        let outcome = (|| {
            let mut selected_home_environment = OsString::from("MARSH_SELECTED_HOME=");
            selected_home_environment.push(spec.lifecycle_workspace.as_os_str());
            let mut invocation = self.invocation([
                OsStr::new("create"),
                OsStr::new("--name"),
                OsStr::new(&spec.name),
                OsStr::new("--pull"),
                OsStr::new("never"),
                OsStr::new("--skills"),
                OsStr::new("off"),
                OsStr::new("-e"),
                selected_home_environment.as_os_str(),
                source.as_os_str(),
                spec.lifecycle_workspace.as_os_str(),
            ]);
            // Stock builds this source on the same host BuildKit as marsh's own
            // build; only a shared cache makes the two image digests agree.
            invocation
                .environment
                .push(("SBX_KIT_BUILDER".into(), "host".into()));
            if rebuild {
                invocation
                    .environment
                    .push(("SBX_KIT_REBUILD".into(), "1".into()));
            }
            let output = self
                .runner
                .run_bounded(&invocation, self.preparation_timeouts.sbx)
                .map_err(|error| {
                    if error.kind() == io::ErrorKind::TimedOut {
                        SbxError::PreparationTimeout {
                            operation: "create local kit VM",
                            timeout: self.preparation_timeouts.sbx,
                        }
                    } else {
                        SbxError::Io(error)
                    }
                });
            Self::require_success("create local kit VM", output?)?;
            before.verify_path()?;
            // The read-only outer inspect overlaps the inventory refresh that
            // adopts the new UUID; nothing enters the VM before both.
            let (adopted, inspected) = thread::scope(|scope| {
                let inspect = scope.spawn(|| self.inspect_kit(&spec.name, true));
                let adopted = self.adopt_created(&spec.name);
                let inspected = inspect
                    .join()
                    .map_err(|_| SbxError::Io(io::Error::other("Kit VM inspect panicked")))
                    .and_then(std::convert::identity);
                (adopted, inspected)
            });
            adopted?;
            inspected
        })();
        let submitted = !matches!(&outcome, Err(SbxError::Io(error)) if marsh_runtime::command_not_started(error));
        let result = outcome;
        if submitted && result.is_err() {
            self.quarantined_workers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(spec.name.clone());
        }
        result
    }

    fn inspect_kit(&self, vm: &str, local_source: bool) -> Result<CommandOutput, SbxError> {
        if local_source {
            self.run_bounded_os(
                "inspect Kit VM",
                self.preparation_timeouts.sbx,
                ["inspect", "--json", vm],
            )
        } else {
            self.run_bounded_os(
                "inspect Kit VM",
                self.preparation_timeouts.sbx,
                ["inspect", vm],
            )
        }
    }

    fn public_image_digest(output: &CommandOutput) -> Result<OciImage, SbxError> {
        let inspect: PublicKitInspect = serde_json::from_slice(&output.stdout).map_err(|_| {
            SbxError::LocalKitDigestMismatch("Kit VM inspect returned invalid JSON".into())
        })?;
        inspect
            .image_digest
            .ok_or_else(|| {
                SbxError::LocalKitDigestMismatch("Kit VM inspect has no image_digest".into())
            })
            .and_then(|image| OciImage::parse(image).map_err(SbxError::from))
    }

    fn ensure_local_kit_vm(
        &self,
        spec: &KitVmSpec,
        lifecycle_grant: &AdmittedHostGrant,
    ) -> Result<ReadyKitVm, SbxError> {
        let NativeKitLocation::LocalV3Source(source) = &spec.workload_kit.location else {
            unreachable!("local preparation requires a local source")
        };
        let resolution_lock = self
            .local_resolution_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(spec.workload_kit.identity().to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _resolving = resolution_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lifecycle_identity = selected_home_identity(&spec.lifecycle_workspace)?;
        let fingerprint = spec.workload_kit.validate_captured_source(source)?;
        let local_tag = format!("marsh-local-v3:{}", &fingerprint[7..]);
        let marker = local_kit_marker(
            spec.workload_kit.identity(),
            &fingerprint,
            &lifecycle_identity,
        );

        match self.classify_vm(&spec.name, false)? {
            VmClass::Foreign => return Err(SbxError::ForeignVm(spec.name.clone())),
            // Inventory already proves absence: no inspect round trip.
            VmClass::Absent => {
                return self.prepare_cold_local_kit_recovering(
                    spec,
                    source,
                    &fingerprint,
                    &marker,
                    lifecycle_grant,
                );
            }
            VmClass::Owned { .. } => {}
        }
        let existing = self.inspect_kit(&spec.name, true)?;
        if existing.succeeded() && self.verify_owned_marker(&spec.name, &marker).is_ok() {
            lifecycle_grant.verify_path()?;
            let outer_manifest = Self::public_image_digest(&existing)?;
            self.wait_for_docker(&spec.name)?;
            if let Ok(image_id) = self.inspect_nested_image(&spec.name, &local_tag)
                && image_id == outer_manifest
            {
                let ready = ReadyKitVm {
                    name: spec.name.clone(),
                    kit_ref: spec.workload_kit.identity().to_owned(),
                    lifecycle_workspace: spec.lifecycle_workspace.clone(),
                    cold_started: false,
                    job_image: image_id.clone(),
                    worker_binary: spec.worker_binary.clone(),
                };
                self.local_resolutions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(
                        spec.workload_kit.identity().to_owned(),
                        LocalResolution {
                            source_fingerprint: fingerprint,
                            image_id,
                            local_tag,
                        },
                    );
                self.ensure_worker_lease(&ready)?;
                return Ok(ready);
            }
            self.drop_worker_lease(&spec.name)?;
            self.remove_observed_kit(&spec.name, Instant::now() + self.preparation_timeouts.sbx)?;
        } else if existing.succeeded() {
            if !self.is_owned_stale_local_vm(spec, &lifecycle_identity)? {
                return Err(SbxError::ForeignVm(spec.name.clone()));
            }
            self.drop_worker_lease(&spec.name)?;
            self.remove_observed_kit(&spec.name, Instant::now() + self.preparation_timeouts.sbx)?;
        } else if !sandbox_missing(&existing) {
            return Err(command_failed("inspect kit VM", existing));
        }

        self.prepare_cold_local_kit_recovering(spec, source, &fingerprint, &marker, lifecycle_grant)
    }

    /// Cold local preparation that recovers from one stale stock build.
    ///
    /// marsh's host Buildx build and stock SBX's own Kit build of the same
    /// source agree only when both come out of one `BuildKit` cache: Kit
    /// builds are not reproducible from scratch. Stock SBX reuses any build
    /// it already holds for the same source tree, however old, so after a
    /// mismatch the VM is recreated once with `SBX_KIT_REBUILD=1` (and a
    /// fresh host build when the cached archive was the stale side).
    fn prepare_cold_local_kit_recovering(
        &self,
        spec: &KitVmSpec,
        source: &Path,
        fingerprint: &str,
        marker: &str,
        lifecycle_grant: &AdmittedHostGrant,
    ) -> Result<ReadyKitVm, SbxError> {
        let mut rebuild = false;
        loop {
            let error = match self.prepare_cold_local_kit(
                spec,
                source,
                fingerprint.to_owned(),
                marker,
                lifecycle_grant,
                rebuild,
            ) {
                Ok(ready) => return Ok(ready),
                Err(ColdLocalFailure::Other(error)) => return Err(error),
                Err(ColdLocalFailure::HostBuildMismatch { error }) if rebuild => {
                    return Err(match error {
                        SbxError::LocalKitDigestMismatch(detail) => {
                            SbxError::LocalKitDigestMismatch(format!(
                                "{detail}; still mismatched after stock SBX rebuilt the Kit"
                            ))
                        }
                        error => error,
                    });
                }
                Err(ColdLocalFailure::HostBuildMismatch { error }) => {
                    rebuild = true;
                    error
                }
            };
            // A VM whose removal could not be proven is quarantined: never
            // recreate under it.
            if self.reject_quarantined(&spec.name).is_err() {
                return Err(error);
            }
            lifecycle_grant.verify_path()?;
        }
    }

    fn prepare_cold_local_kit(
        &self,
        spec: &KitVmSpec,
        source: &Path,
        fingerprint: String,
        marker: &str,
        lifecycle_grant: &AdmittedHostGrant,
        rebuild: bool,
    ) -> Result<ReadyKitVm, ColdLocalFailure> {
        let (outer_manifest, build) =
            self.create_and_build_local(spec, source, &fingerprint, lifecycle_grant, rebuild)?;
        let host_build_mismatch = build.manifest_digest != outer_manifest;
        let prepared = (|| -> Result<OciImage, SbxError> {
            if build.source_fingerprint != local_source_fingerprint(source)? {
                return Err(SbxError::SourceChanged(source.to_owned()));
            }
            if build.manifest_digest != outer_manifest {
                return Err(SbxError::LocalKitDigestMismatch(format!(
                    "host build {}, Kit VM image_digest {}",
                    build.manifest_digest.as_str(),
                    outer_manifest.as_str()
                )));
            }
            self.load_nested_archive(&spec.name, &build.docker_archive)?;
            let image_id = self.inspect_nested_image(&spec.name, &build.local_tag)?;
            if image_id != outer_manifest {
                return Err(SbxError::LocalKitDigestMismatch(format!(
                    "Kit VM image_digest {}, image loaded in the VM {}",
                    outer_manifest.as_str(),
                    image_id.as_str()
                )));
            }
            if fingerprint != local_source_fingerprint(source)? {
                return Err(SbxError::SourceChanged(source.to_owned()));
            }
            self.write_owned_marker(&spec.name, marker)?;
            Ok(image_id)
        })();
        let cache = self
            .kit_image_cache
            .as_deref()
            .filter(|_| lifecycle_grant.ephemeral.is_none());
        let cleanup = match (&prepared, cache) {
            // Keep only an archive whose load produced the verified image.
            (Ok(_), Some(cache)) if retain_local_build(cache, &build).is_ok() => Ok(()),
            (Err(_), Some(cache)) if build.retained => {
                evict_kit_image_cache(cache, &build);
                Ok(())
            }
            _ => Self::cleanup_local_build(&build),
        };
        let image_id = match (prepared, cleanup) {
            (Ok(image_id), Ok(())) => image_id,
            (Err(error @ SbxError::LocalKitDigestMismatch(_)), _) if host_build_mismatch => {
                self.cleanup_created_kit(spec);
                return Err(ColdLocalFailure::HostBuildMismatch { error });
            }
            (Err(error), _) | (Ok(_), Err(error)) => {
                self.cleanup_created_kit(spec);
                return Err(error.into());
            }
        };
        let ready = ReadyKitVm {
            name: spec.name.clone(),
            kit_ref: spec.workload_kit.identity().to_owned(),
            lifecycle_workspace: spec.lifecycle_workspace.clone(),
            cold_started: true,
            job_image: image_id.clone(),
            worker_binary: spec.worker_binary.clone(),
        };
        self.local_resolutions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                spec.workload_kit.identity().to_owned(),
                LocalResolution {
                    source_fingerprint: fingerprint,
                    image_id,
                    local_tag: build.local_tag,
                },
            );
        self.ensure_worker_lease(&ready)?;
        Ok(ready)
    }

    fn create_and_build_local(
        &self,
        spec: &KitVmSpec,
        source: &Path,
        fingerprint: &str,
        lifecycle_grant: &AdmittedHostGrant,
        rebuild: bool,
    ) -> Result<(OciImage, LocalBuild), SbxError> {
        let private_scratch = if lifecycle_grant.ephemeral.is_some() {
            let parent = spec
                .lifecycle_workspace
                .parent()
                .ok_or_else(|| SbxError::UnsafePath(spec.lifecycle_workspace.clone()))?;
            Some(
                tempfile::Builder::new()
                    .prefix(".kit-build-")
                    .tempdir_in(parent)?,
            )
        } else {
            None
        };
        let build_workspace = private_scratch
            .as_ref()
            .map_or(spec.lifecycle_workspace.as_path(), tempfile::TempDir::path);
        let cached = self
            .kit_image_cache
            .as_deref()
            .filter(|_| lifecycle_grant.ephemeral.is_none())
            .and_then(|cache| cached_local_build(cache, fingerprint));
        let (create_result, build_result) = thread::scope(|scope| {
            let create =
                scope.spawn(|| self.create_local_vm(spec, source, lifecycle_grant, rebuild));
            let build = scope.spawn(|| match cached {
                Some(cached) => Ok(cached),
                None => self.build_local_source(source, fingerprint, build_workspace),
            });
            (
                create
                    .join()
                    .map_err(|_| SbxError::Io(io::Error::other("local Kit VM creation panicked"))),
                build
                    .join()
                    .map_err(|_| SbxError::Io(io::Error::other("local Kit image build panicked"))),
            )
        });
        let create_result = create_result.and_then(std::convert::identity);
        let build_result = build_result.and_then(std::convert::identity);
        let (outer, image_build) = match (create_result, build_result) {
            (Ok(outer), Ok(image_build)) => (outer, image_build),
            (Err(error), Ok(image_build)) => {
                let cleanup = Self::cleanup_local_build(&image_build);
                self.cleanup_created_kit(spec);
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup_error) => Err(cleanup_error),
                };
            }
            (Ok(_), Err(error)) | (Err(error), Err(_)) => {
                self.cleanup_created_kit(spec);
                return Err(error);
            }
        };
        let image_build = LocalBuild {
            _private_scratch: private_scratch,
            ..image_build
        };
        let outer_manifest = (|| -> Result<OciImage, SbxError> {
            Self::require_success("inspect created local kit VM", outer.clone())?;
            let outer_manifest = Self::public_image_digest(&outer)?;
            if fingerprint != local_source_fingerprint(source)? {
                return Err(SbxError::SourceChanged(source.to_owned()));
            }
            Ok(outer_manifest)
        })();
        match outer_manifest {
            Ok(outer_manifest) => Ok((outer_manifest, image_build)),
            Err(error) => {
                let cleanup = Self::cleanup_local_build(&image_build);
                self.cleanup_created_kit(spec);
                match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup_error) => Err(cleanup_error),
                }
            }
        }
    }

    fn cleanup_local_build(build: &LocalBuild) -> Result<(), SbxError> {
        if build.retained {
            return Ok(());
        }
        let mut first_error = None;
        for path in [&build.docker_archive, &build.metadata] {
            if let Err(source) = fs::remove_file(path) {
                first_error.get_or_insert_with(|| SbxError::Metadata {
                    path: path.clone(),
                    source,
                });
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn load_nested_archive(&self, vm: &str, archive: &Path) -> Result<(), SbxError> {
        // One exec waits for the VM's Docker runtime, then loads: each stock
        // round trip costs ~0.4s on a cold VM.
        let invocation = self.invocation([
            "exec",
            "-i",
            "-u",
            "root",
            vm,
            "sh",
            "-c",
            "docker info --format '{{.ID}}' >/dev/null && exec \"$@\"",
            "marsh-load",
            "docker",
            "image",
            "load",
        ]);
        let attachment = self.runner.spawn_attached(&invocation)?;
        let mut process = attachment.process;
        let timeout = self.preparation_timeouts.load;
        let transfer = thread::scope(|scope| {
            let stdout = scope.spawn(move || {
                let mut reader = attachment.stdout;
                let mut bytes = Vec::new();
                reader.read_to_end(&mut bytes).map(|_| bytes)
            });
            let stderr = scope.spawn(move || {
                let mut reader = attachment.stderr;
                let mut bytes = Vec::new();
                reader.read_to_end(&mut bytes).map(|_| bytes)
            });
            let archive = archive.to_owned();
            let writer = scope.spawn(move || {
                let mut archive_file =
                    fs::File::open(&archive).map_err(|source| SbxError::Metadata {
                        path: archive,
                        source,
                    })?;
                let mut stdin = attachment.stdin;
                std::io::copy(&mut archive_file, &mut stdin)?;
                stdin.flush()?;
                drop(stdin);
                Ok::<_, SbxError>(())
            });
            let deadline = std::time::Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| SbxError::Io(io::Error::other("load deadline overflow")))?;
            let mut timed_out = false;
            let exit_code = loop {
                if let Some(exit_code) = process.try_wait()? {
                    break exit_code;
                }
                if std::time::Instant::now() >= deadline {
                    timed_out = true;
                    let terminate = process.terminate();
                    let exit_code = process.wait();
                    terminate?;
                    break exit_code?;
                }
                thread::sleep(Duration::from_millis(10));
            };
            let written = writer
                .join()
                .map_err(|_| SbxError::Io(io::Error::other("archive writer panicked")))?;
            let stdout = stdout
                .join()
                .map_err(|_| SbxError::Io(io::Error::other("stdout reader panicked")))??;
            let stderr = stderr
                .join()
                .map_err(|_| SbxError::Io(io::Error::other("stderr reader panicked")))??;
            if timed_out {
                return Err(SbxError::PreparationTimeout {
                    operation: "load local v3 kit image into worker VM",
                    timeout,
                });
            }
            if let Err(error) = written {
                let _ = process.terminate();
                let _ = process.wait();
                return Err(error);
            }
            Ok::<_, SbxError>((exit_code, stdout, stderr))
        })?;
        let (exit_code, _stdout, stderr) = transfer;
        if exit_code != 0 {
            return Err(SbxError::CommandFailed {
                operation: "load local v3 kit image into worker VM",
                status: Some(exit_code),
                stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
            });
        }
        Ok(())
    }

    fn inspect_nested_image(&self, vm: &str, tag: &str) -> Result<OciImage, SbxError> {
        let inspected = self.run([
            "exec", "-u", "root", vm, "docker", "image", "inspect", "--format", "{{.Id}}", tag,
        ])?;
        Self::require_success("inspect local v3 kit image", inspected.clone())?;
        let image_id = String::from_utf8_lossy(&inspected.stdout).trim().to_owned();
        if !valid_image_id(&image_id) {
            return Err(SbxError::ImageVerificationFailed);
        }
        OciImage::parse(image_id).map_err(SbxError::from)
    }

    fn build_local_source(
        &self,
        source: &Path,
        fingerprint: &str,
        lifecycle_workspace: &Path,
    ) -> Result<LocalBuild, SbxError> {
        let descriptor = local_v3_descriptor(source)?;
        selected_home_identity(lifecycle_workspace)?;
        sweep_stale_local_scratch(lifecycle_workspace, SystemTime::now())?;
        let tag = format!("marsh-local-v3:{}", &fingerprint[7..]);
        let sequence = NEXT_LOCAL_SCRATCH.fetch_add(1, Ordering::Relaxed);
        let docker_archive = lifecycle_workspace.join(format!(
            ".marsh-kit-{}-{sequence}.docker.tar",
            std::process::id()
        ));
        let metadata = lifecycle_workspace.join(format!(
            ".marsh-kit-{}-{sequence}.metadata.json",
            std::process::id()
        ));
        let output = self.run_program_bounded_os(
            "build local v3 kit job image",
            Path::new("docker"),
            self.preparation_timeouts.build,
            [
                OsStr::new("buildx"),
                OsStr::new("build"),
                OsStr::new("--platform"),
                OsStr::new("linux/arm64"),
                OsStr::new("--output"),
                // Stock SBX exports its own build of this source as OCI; the
                // two manifest digests agree only with matching media types.
                OsStr::new(&format!(
                    "type=docker,oci-mediatypes=true,dest={}",
                    docker_archive.display()
                )),
                OsStr::new("--tag"),
                OsStr::new(&tag),
                OsStr::new("--metadata-file"),
                metadata.as_os_str(),
                OsStr::new("--file"),
                descriptor.as_os_str(),
                source.as_os_str(),
            ],
        )?;
        if !output.succeeded() {
            let _ = fs::remove_file(&docker_archive);
            let _ = fs::remove_file(&metadata);
            return Err(command_failed("build local v3 kit job image", output));
        }
        let manifest_digest = (|| {
            let metadata_value: serde_json::Value =
                serde_json::from_slice(&fs::read(&metadata).map_err(|error| {
                    SbxError::Metadata {
                        path: metadata.clone(),
                        source: error,
                    }
                })?)?;
            metadata_value
                .get("containerimage.digest")
                .and_then(serde_json::Value::as_str)
                .ok_or(SbxError::InvalidLocalBuildMetadata)
                .and_then(|digest| OciImage::parse(digest.to_owned()).map_err(SbxError::from))
        })();
        if let Err(error) = manifest_digest {
            let _ = fs::remove_file(&docker_archive);
            let _ = fs::remove_file(&metadata);
            return Err(error);
        }
        Ok(LocalBuild {
            source_fingerprint: fingerprint.to_owned(),
            local_tag: tag,
            manifest_digest: manifest_digest.expect("checked above"),
            docker_archive,
            metadata,
            retained: false,
            _private_scratch: None,
        })
    }

    fn wait_for_docker(&self, vm: &str) -> Result<(), SbxError> {
        // `sbx exec` starts a stopped VM. A bounded caller-level deadline is
        // still required around this blocking adapter.
        Self::require_success(
            "wait for kit Docker runtime",
            self.run_bounded_os(
                "wait for kit Docker runtime",
                self.preparation_timeouts.docker,
                [
                    "exec", "-u", "root", vm, "docker", "info", "--format", "{{.ID}}",
                ],
            )?,
        )
    }

    /// Copies both trusted binaries concurrently, then checks Docker and
    /// installs them with one root exec: each stock `sbx` round trip costs
    /// ~0.3s on a cold VM.
    fn install_worker(&self, vm: &str, worker_binary: &Path) -> Result<(), SbxError> {
        let worker_temporary = "/tmp/marsh-worker.install";
        let helper_temporary = "/tmp/marsh-byte-exec.install";
        let helper = byte_exec_artifact(worker_binary);
        // The static job artifact bound at /run/marsh/marsh (docs/design/processes.md s4).
        let shim = job_artifact(worker_binary);
        let shim_temporary = "/tmp/marsh-local.install";
        regular_executable(&shim)?;
        let binaries = vec![
            (worker_binary, worker_temporary),
            (helper.as_path(), helper_temporary),
            (shim.as_path(), shim_temporary),
        ];
        self.copy_trusted_binaries(vm, &binaries)?;
        // The same bounded exec first waits for the VM's Docker runtime
        // (`sbx exec` starts a stopped VM).
        Self::require_success(
            "wait for kit Docker runtime and install trusted worker",
            self.run_bounded_os(
                "wait for kit Docker runtime and install trusted worker",
                self.preparation_timeouts.docker,
                [
                    "exec",
                    "-u",
                    "root",
                    vm,
                    "sh",
                    "-c",
                    "set -eu; docker info --format '{{.ID}}' >/dev/null; \
                     install -d -m 0755 /usr/local/libexec; \
                     install -m 0755 \"$1\" \"$2\"; install -m 0755 \"$3\" \"$4\"; \
                     install -m 0755 \"$5\" \"$6\"",
                    "marsh-install-worker",
                    worker_temporary,
                    WORKER_PATH,
                    helper_temporary,
                    "/usr/local/libexec/marsh-byte-exec",
                    shim_temporary,
                    marsh_runtime::SPLIT_SHIM,
                ],
            )?,
        )
    }

    #[allow(clippy::too_many_lines)]
    fn ensure_worker_lease(&self, vm: &ReadyKitVm) -> Result<(), SbxError> {
        let init_lock = self
            .worker_init_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(vm.name.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _initializing = init_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let artifact = self.cached_worker_artifact_identity(&vm.worker_binary)?;
        let installed = self
            .installed_worker_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&vm.name)
            .is_some_and(|installed| installed == &artifact);
        if installed {
            let lease = self
                .worker_leases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&vm.name)
                .cloned();
            if let Some(lease) = lease {
                if lease.alive.load(Ordering::Acquire)
                    && lease
                        .process
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .try_wait()?
                        .is_none()
                    && Self::ping_worker(&lease).is_ok()
                {
                    return Ok(());
                }
                if lease.active.load(Ordering::Acquire) != 0 {
                    self.quarantined_workers
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(vm.name.clone());
                    return Err(SbxError::WorkerLeaseLost(vm.name.clone()));
                }
            }
            self.drop_worker_lease(&vm.name)?;
        } else {
            self.drop_worker_lease(&vm.name)?;
        }

        self.install_worker(&vm.name, &vm.worker_binary)?;
        if artifact != worker_artifact_identity(&vm.worker_binary)? {
            return Err(SbxError::SourceChanged(vm.worker_binary.clone()));
        }
        let generation = self
            .worker_generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&vm.name)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        let invocation = self.invocation([
            "exec",
            "-i",
            "-u",
            "root",
            "-w",
            "/",
            &vm.name,
            WORKER_PATH,
            "--serve",
            &generation.to_string(),
        ]);
        let lease = self.runner.spawn_attached(&invocation)?;
        let (ready, worker_stdout) = match read_worker_ready(lease.stdout, KEEPALIVE_READY_TIMEOUT)
        {
            Ok(ready) => ready,
            Err(error) => {
                let _ = terminate_and_reap_process(
                    lease.process,
                    self.retained_process_teardown_timeout,
                );
                return Err(error);
            }
        };
        if ready != (WorkerResponse::Ready { generation }) {
            let _ =
                terminate_and_reap_process(lease.process, self.retained_process_teardown_timeout);
            return Err(SbxError::WorkerLeaseLost(vm.name.clone()));
        }
        let retained = Arc::new(RetainedWorker {
            generation,
            input: Mutex::new(lease.stdin),
            process: Mutex::new(lease.process),
            routes: Mutex::new(BTreeMap::new()),
            ping_serial: Mutex::new(()),
            pending_pong: Mutex::new(None),
            next_ping: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            active: AtomicUsize::new(0),
            loss: WorkerLoss::default(),
        });
        let reader_worker = Arc::clone(&retained);
        thread::spawn(move || {
            let mut output = worker_stdout;
            let ended = loop {
                let response = match read_frame::<WorkerResponse>(&mut output) {
                    Ok(response) => response,
                    Err(error) => break format!("worker transport ended: {error}"),
                };
                match &response {
                    WorkerResponse::Pong { generation, nonce } => {
                        let route = reader_worker
                            .pending_pong
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        if let Some(route) = &*route
                            && route.nonce == *nonce
                        {
                            // A per-request channel prevents a late previous
                            // Pong from filling the next ping's queue. Never
                            // block the shared stdout dispatcher on any Pong.
                            let _ = route.send.try_send(*generation);
                        }
                    }
                    WorkerResponse::Started { attempt, .. }
                    | WorkerResponse::Stdout { attempt, .. }
                    | WorkerResponse::Stderr { attempt, .. }
                    | WorkerResponse::Terminal { attempt, .. }
                    | WorkerResponse::Rejected { attempt, .. }
                    | WorkerResponse::CapOpen { attempt, .. }
                    | WorkerResponse::CapData { attempt, .. }
                    | WorkerResponse::CapClose { attempt, .. } => {
                        let route_attempt = attempt.clone();
                        let mut cancel = false;
                        let route = {
                            let mut routes = reader_worker
                                .routes
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            routes.get_mut(attempt).and_then(|route| {
                                let was_overflowing = route.overflowing;
                                if route.admit(&response) {
                                    Some(route.send.clone())
                                } else {
                                    cancel = !was_overflowing;
                                    None
                                }
                            })
                        };
                        if let Some(route) = route {
                            let _ = route.send(response);
                        }
                        if cancel {
                            let cancelling = Arc::clone(&reader_worker);
                            thread::spawn(move || {
                                let _ = write_frame(
                                    &mut *cancelling
                                        .input
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                                    &WorkerRequest::Cancel {
                                        attempt: route_attempt,
                                    },
                                );
                            });
                        }
                    }
                    WorkerResponse::Ready { generation } => {
                        break format!(
                            "worker sent Ready (generation {generation}) mid-session: it restarted"
                        );
                    }
                }
            };
            // The exit status, if the `sbx exec` carrier already ended, says
            // whether the worker died or the stream broke under it.
            let status = reader_worker
                .process
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .try_wait_unreaped()
                .ok()
                .flatten()
                .map_or_else(
                    || "carrier still running".to_owned(),
                    |code| format!("carrier exited {code}"),
                );
            if reader_worker.active.load(Ordering::Acquire) > 0 {
                reader_worker.loss.set(format!("{ended} ({status})"));
            }
            reader_worker.alive.store(false, Ordering::Release);
            reader_worker
                .routes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
        });
        let stderr_worker = Arc::clone(&retained);
        thread::spawn(move || {
            let mut stderr = lease.stderr;
            let mut buffer = [0_u8; 1024];
            while let Ok(count) = stderr.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                stderr_worker.loss.record_stderr(&buffer[..count]);
            }
        });
        self.worker_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(vm.name.clone(), retained);
        self.installed_worker_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(vm.name.clone(), artifact);
        self.worker_generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(vm.name.clone(), generation);
        Ok(())
    }

    fn ping_worker(worker: &Arc<RetainedWorker>) -> Result<(), SbxError> {
        let deadline = Instant::now() + KEEPALIVE_READY_TIMEOUT;
        let _serial = worker
            .ping_serial
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let nonce = worker
            .next_ping
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |nonce| {
                nonce.checked_add(1)
            })
            .map_err(|_| SbxError::WorkerLeaseLost("worker ping nonce exhausted".into()))?;
        let (pong_send, pong_receive) = mpsc::sync_channel(1);
        *worker
            .pending_pong
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(PendingWorkerPong {
            nonce,
            send: pong_send,
        });
        let _pending = PendingWorkerPing { worker, nonce };
        let (send, receive) = mpsc::sync_channel(1);
        let writing = Arc::clone(worker);
        thread::spawn(move || {
            let result = write_frame(
                &mut *writing
                    .input
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                &WorkerRequest::Ping {
                    generation: writing.generation,
                    nonce,
                },
            );
            let _ = send.send(result);
        });
        receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| SbxError::WorkerLeaseLost("worker ping write timed out".into()))?
            .map_err(|error| SbxError::Io(io::Error::other(error)))?;
        let observed = pong_receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| SbxError::WorkerLeaseLost("worker ping timed out".into()))?;
        if observed != worker.generation {
            return Err(SbxError::WorkerLeaseLost(
                "worker generation mismatch".into(),
            ));
        }
        Ok(())
    }

    fn drop_worker_lease(&self, vm: &str) -> Result<(), SbxError> {
        self.drop_worker_lease_with_timeout(vm, self.retained_process_teardown_timeout)
    }

    fn drop_worker_lease_before(&self, vm: &str, deadline: Instant) -> Result<(), SbxError> {
        let timeout = self
            .retained_process_teardown_timeout
            .min(self.lifecycle_timeout(deadline, "stop retained worker process")?);
        self.drop_worker_lease_with_timeout(vm, timeout)
    }

    fn drop_worker_lease_with_timeout(&self, vm: &str, timeout: Duration) -> Result<(), SbxError> {
        let lease = self
            .worker_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(vm);
        if let Some(lease) = lease {
            lease.alive.store(false, Ordering::Release);
            let process = std::mem::replace(
                &mut *lease
                    .process
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                Box::new(ReleasedAttachedProcess),
            );
            terminate_and_reap_process(process, timeout)?;
        }
        self.installed_worker_artifacts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(vm);
        Ok(())
    }

    fn drop_shell_lease_before(&self, vm: &str, deadline: Instant) -> Result<(), SbxError> {
        let timeout = self
            .retained_process_teardown_timeout
            .min(self.lifecycle_timeout(deadline, "stop retained shell process")?);
        self.drop_shell_lease_with_timeout(vm, timeout)
    }

    fn drop_shell_lease_with_timeout(&self, vm: &str, timeout: Duration) -> Result<(), SbxError> {
        let lease = self
            .shell_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(vm);
        if let Some(process) = lease.and_then(|lease| lease.shutdown()) {
            terminate_and_reap_process(process, timeout)?;
        }
        Ok(())
    }

    /// Whether this VM's supervisor transport is live (recent frame or ping).
    /// A dead idle transport is dropped (`Ok(false)`) so the caller revalidates
    /// the VM and starts a new generation. Loss with live attempts fences the
    /// VM: those sessions are cleanup uncertain (same rule as the kit worker).
    fn shell_lease_live(&self, vm: &str) -> Result<bool, SbxError> {
        let current = self
            .shell_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(vm)
            .cloned();
        let Some(lease) = current else {
            return Ok(false);
        };
        if lease
            .check_live(SUPERVISOR_RECENT, SUPERVISOR_PING_TIMEOUT)
            .is_ok()
        {
            return Ok(true);
        }
        let active = lease.active_attempts();
        self.shell_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(vm);
        if let Some(process) = lease.shutdown() {
            let _ = terminate_and_reap_process(process, self.retained_process_teardown_timeout);
        }
        // The VM may have stopped or vanished: re-observe stock inventory.
        self.ownership.invalidate();
        if active > 0 {
            self.uncertain_shell_grants
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(vm.to_owned())
                .or_default();
            return Err(SbxError::QuarantinedShellVm(vm.to_owned()));
        }
        Ok(false)
    }

    fn ensure_shell_lease(
        &self,
        spec: &ShellVmSpec,
        marker: &str,
        shell_artifact: &str,
    ) -> Result<(), SbxError> {
        if self.shell_lease_live(&spec.name)? {
            return Ok(());
        }
        self.initialize_shell_guest(spec, marker, shell_artifact)?;
        let invocation = self.invocation([
            "exec",
            "-i",
            "-u",
            "root",
            "-w",
            "/",
            &spec.name,
            "/usr/local/bin/marsh",
            "--internal-supervisor",
        ]);
        let attachment = self.runner.spawn_attached(&invocation)?;
        let generation = self.shell_generation.fetch_add(1, Ordering::Relaxed);
        let lease =
            shell_supervisor::Supervisor::start(attachment, generation, KEEPALIVE_READY_TIMEOUT)
                .map_err(|_| SbxError::ShellLeaseLost(spec.name.clone()))?;
        self.shell_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(spec.name.clone(), lease);
        Ok(())
    }

    fn marker(profile: &str) -> WorkerMarker<'_> {
        WorkerMarker {
            schema: "marsh.worker/v1",
            profile,
            basis: KIT_VM_BASIS,
        }
    }

    fn verify_owned_marker(&self, vm: &str, profile: &str) -> Result<(), SbxError> {
        let actual = self.read_owned_marker(vm)?;
        if actual
            != (OwnedWorkerMarker {
                schema: "marsh.worker/v1".into(),
                profile: profile.to_owned(),
                basis: KIT_VM_BASIS.into(),
            })
        {
            return Err(SbxError::ForeignVm(vm.to_owned()));
        }
        Ok(())
    }

    fn read_owned_marker(&self, vm: &str) -> Result<OwnedWorkerMarker, SbxError> {
        let output = self.run_bounded_os(
            "read worker ownership marker",
            self.preparation_timeouts.sbx,
            [
                "exec",
                "-u",
                "root",
                vm,
                "cat",
                "/var/lib/marsh/worker.json",
            ],
        )?;
        if !output.succeeded() {
            return Err(SbxError::ForeignVm(vm.to_owned()));
        }
        serde_json::from_slice(&output.stdout).map_err(|_| SbxError::ForeignVm(vm.to_owned()))
    }

    fn is_owned_stale_local_vm(
        &self,
        spec: &KitVmSpec,
        lifecycle_identity: &str,
    ) -> Result<bool, SbxError> {
        let actual = self.read_owned_marker(&spec.name)?;
        let prefix = format!("kit:{}:", spec.workload_kit.identity());
        let suffix = format!(":{lifecycle_identity}");
        let prior_image = actual
            .profile
            .strip_prefix(&prefix)
            .and_then(|profile| profile.strip_suffix(&suffix));
        Ok(actual.schema == "marsh.worker/v1"
            && actual.basis == KIT_VM_BASIS
            && prior_image.is_some_and(valid_image_id))
    }

    fn install_binary(
        &self,
        vm: &str,
        source: &Path,
        temporary: &str,
        destination: &str,
    ) -> Result<(), SbxError> {
        let parent = Path::new(destination)
            .parent()
            .ok_or_else(|| SbxError::UnsafePath(PathBuf::from(destination)))?;
        Self::require_success(
            "prepare binary install directory",
            self.run_os([
                OsStr::new("exec"),
                OsStr::new("-u"),
                OsStr::new("root"),
                OsStr::new(vm),
                OsStr::new("install"),
                OsStr::new("-d"),
                OsStr::new("-m"),
                OsStr::new("0755"),
                parent.as_os_str(),
            ])?,
        )?;
        let copied = format!("{vm}:{temporary}");
        Self::require_success(
            "copy trusted binary",
            self.run_os([OsStr::new("cp"), source.as_os_str(), OsStr::new(&copied)])?,
        )?;
        Self::require_success(
            "install trusted binary",
            self.run_os([
                OsStr::new("exec"),
                OsStr::new("-u"),
                OsStr::new("root"),
                OsStr::new(vm),
                OsStr::new("install"),
                OsStr::new("-m"),
                OsStr::new("0755"),
                OsStr::new(temporary),
                OsStr::new(destination),
            ])?,
        )
    }

    fn write_owned_marker(&self, vm: &str, profile: &str) -> Result<(), SbxError> {
        let marker = serde_json::to_string(&Self::marker(profile))?;
        Self::require_success(
            "write worker ownership marker",
            self.run([
                "exec", "-u", "root", "-e", &format!("MARSH_WORKER_MARKER={marker}"), vm,
                "sh", "-c",
                "umask 077; mkdir -p /var/lib/marsh; printf '%s\\n' \"$MARSH_WORKER_MARKER\" > /var/lib/marsh/worker.json",
            ])?,
        )
    }

    /// Concurrent `sbx cp` of trusted host binaries to guest temporaries.
    fn copy_trusted_binaries(&self, vm: &str, copies: &[(&Path, &str)]) -> Result<(), SbxError> {
        thread::scope(|scope| {
            copies
                .iter()
                .map(|(source, temporary)| {
                    let destination = format!("{vm}:{temporary}");
                    scope.spawn(move || {
                        Self::require_success(
                            "copy trusted binary",
                            self.run_os([
                                OsStr::new("cp"),
                                source.as_os_str(),
                                OsStr::new(&destination),
                            ])?,
                        )
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| SbxError::Io(io::Error::other("binary copy panicked")))
                        .and_then(std::convert::identity)
                })
                .collect::<Result<Vec<()>, _>>()
        })?;
        Ok(())
    }

    /// Copies the trusted shell (and its sibling relay, when packaged) in
    /// parallel, then performs every root setup step in one exec: each stock
    /// `sbx` round trip costs ~0.3s on a cold VM. The ownership marker is the
    /// script's last effect, so a partial setup is never marked initialized.
    fn initialize_shell_guest(
        &self,
        spec: &ShellVmSpec,
        marker: &str,
        shell_artifact: &str,
    ) -> Result<(), SbxError> {
        const SHELL_TEMPORARY: &str = "/tmp/marsh-shell.install";
        const RELAY_TEMPORARY: &str = "/tmp/marsh-relay.install";
        let relay = spec.shell_binary.with_file_name("marsh-relay-linux-arm64");
        let relay_artifact = if regular_executable(&relay).is_ok() {
            Some(self.cached_artifact_identity(&relay)?)
        } else {
            None
        };
        let mut copies = vec![(spec.shell_binary.as_path(), SHELL_TEMPORARY)];
        if relay_artifact.is_some() {
            copies.push((relay.as_path(), RELAY_TEMPORARY));
        }
        self.copy_trusted_binaries(&spec.name, &copies)?;
        let user = &spec.user;
        let mut home = OsString::from("MARSH_SHELL_HOME=");
        home.push(user.home.as_os_str());
        let environment = [
            format!(
                "MARSH_WORKER_MARKER={}",
                serde_json::to_string(&Self::marker(marker))?
            ),
            format!("MARSH_SHELL_USER={}", user.name),
            format!("MARSH_RELAY_UID={}", user.uid),
            format!("MARSH_RELAY_GID={}", user.gid),
            format!("MARSH_RELAY_RUNTIME=/run/marsh/{}", user.uid),
            format!(
                "MARSH_RELAY_INSTALL={}",
                if relay_artifact.is_some() {
                    RELAY_TEMPORARY
                } else {
                    ""
                }
            ),
        ];
        let mut arguments: Vec<OsString> = vec!["exec".into(), "-u".into(), "root".into()];
        for entry in environment.iter().map(OsString::from).chain([home]) {
            arguments.push("-e".into());
            arguments.push(entry);
        }
        arguments.extend([
            OsString::from(&spec.name),
            "sh".into(),
            "-c".into(),
            SHELL_SETUP_SCRIPT.into(),
        ]);
        let output = self.run_os(arguments)?;
        if output.exit_code == Some(SHELL_USER_MISMATCH_EXIT) {
            return Err(SbxError::InvalidShellUser);
        }
        Self::require_success("initialize shell VM", output)?;
        if artifact_identity(&spec.shell_binary)? != shell_artifact {
            return Err(SbxError::SourceChanged(spec.shell_binary.clone()));
        }
        if let Some(relay_artifact) = relay_artifact {
            if artifact_identity(&relay)? != relay_artifact {
                return Err(SbxError::SourceChanged(relay));
            }
            self.installed_shell_artifacts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert((spec.name.clone(), RELAY_PATH.into()), relay_artifact);
        }
        Ok(())
    }

    fn grant_preparation_error(
        &self,
        vm: &ReadyKitVm,
        attempt: &str,
        mounts: &[PreparedMount],
        cause: SbxError,
    ) -> SbxError {
        let rollback_complete = self
            .revoke_grants(&PreparedGrants {
                vm: vm.name.clone(),
                attempt: attempt.to_owned(),
                worker_generation: 0,
                mounts: mounts.to_vec(),
            })
            .is_ok();
        SbxError::GrantPreparationFailed {
            cause: Box::new(cause),
            rollback_complete,
        }
    }

    fn invocation<I, S>(&self, arguments: I) -> Invocation
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Invocation {
            program: self.sbx.clone(),
            arguments: arguments
                .into_iter()
                .map(|value| value.as_ref().to_os_string())
                .collect(),
            working_directory: None,
            environment: Vec::new(),
        }
    }

    fn invocation_in<I, S>(&self, directory: &Path, arguments: I) -> Invocation
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut invocation = self.invocation(arguments);
        invocation.working_directory = Some(directory.to_owned());
        invocation
    }

    fn run<const N: usize>(&self, arguments: [&str; N]) -> Result<CommandOutput, SbxError> {
        self.run_os(arguments.map(OsStr::new))
    }

    fn run_os<I, S>(&self, arguments: I) -> Result<CommandOutput, SbxError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Ok(self.runner.run(&self.invocation(arguments))?)
    }

    fn run_bounded_os<I, S>(
        &self,
        operation: &'static str,
        timeout: Duration,
        arguments: I,
    ) -> Result<CommandOutput, SbxError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.runner
            .run_bounded(&self.invocation(arguments), timeout)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::TimedOut {
                    SbxError::PreparationTimeout { operation, timeout }
                } else {
                    SbxError::Io(error)
                }
            })
    }

    fn lifecycle_timeout(
        &self,
        deadline: Instant,
        operation: &'static str,
    ) -> Result<Duration, SbxError> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(SbxError::LifecycleDeadline { operation });
        }
        Ok(self.preparation_timeouts.sbx.min(remaining))
    }

    fn run_program_bounded_os<I, S>(
        &self,
        operation: &'static str,
        program: &Path,
        timeout: Duration,
        arguments: I,
    ) -> Result<CommandOutput, SbxError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.runner
            .run_bounded(
                &Invocation {
                    program: program.to_owned(),
                    arguments: arguments
                        .into_iter()
                        .map(|value| value.as_ref().to_os_string())
                        .collect(),
                    working_directory: None,
                    environment: Vec::new(),
                },
                timeout,
            )
            .map_err(|error| {
                if error.kind() == io::ErrorKind::TimedOut {
                    SbxError::PreparationTimeout { operation, timeout }
                } else {
                    SbxError::Io(error)
                }
            })
    }

    fn run_os_in<I, S>(&self, directory: &Path, arguments: I) -> Result<CommandOutput, SbxError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Ok(self.runner.run(&self.invocation_in(directory, arguments))?)
    }

    fn require_success(operation: &'static str, output: CommandOutput) -> Result<(), SbxError> {
        if output.succeeded() {
            Ok(())
        } else {
            Err(command_failed(operation, output))
        }
    }
}

impl SbxError {
    /// Whether a failed grant-preparation transaction proved that every
    /// partial mount and its attempt root were removed.
    #[must_use]
    pub fn grant_rollback_complete(&self) -> Option<bool> {
        match self {
            Self::GrantPreparationFailed {
                rollback_complete, ..
            } => Some(*rollback_complete),
            _ => None,
        }
    }
}

// Validate the WHOLE inventory's identity fields. Malformed/duplicate foreign
// entries must not hide the exact UUID under another name during absence proof.
// VM lifecycle -> per-key phase reservation -> stock-operation admission.
// The phase mutex and global maps are always released before opaque stock calls.
fn shell_admission_before<T>(
    mut attempt: impl FnMut() -> Result<T, std::sync::TryLockError<T>>,
    deadline: Instant,
) -> Result<T, SbxError> {
    loop {
        if Instant::now() >= deadline {
            return Err(SbxError::ShellMountAdmissionBusy);
        }
        match attempt() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {
                thread::sleep(
                    Duration::from_millis(5)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
        }
    }
}

/// Sentinel left behind after the real retained process has been transferred
/// to bounded teardown. Concurrent observers must treat that lease as dead;
/// they must never gain a second handle capable of replaying termination.
struct ReleasedAttachedProcess;

impl marsh_runtime::AttachedProcess for ReleasedAttachedProcess {
    fn wait(&mut self) -> io::Result<i32> {
        Ok(0)
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(Some(0))
    }

    fn terminate(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn terminate_and_reap_process(
    mut process: Box<dyn marsh_runtime::AttachedProcess>,
    timeout: Duration,
) -> Result<(), SbxError> {
    let (send, receive) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let result = (|| -> io::Result<()> {
            process.terminate()?;
            let deadline = Instant::now()
                .checked_add(timeout)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "deadline overflow"))?;
            loop {
                if process.try_wait()?.is_some() {
                    return Ok(());
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "retained process did not exit after termination",
                    ));
                }
                thread::sleep(RETAINED_PROCESS_POLL_INTERVAL.min(remaining));
            }
        })();
        let _ = send.send(result);
    });
    match receive.recv_timeout(timeout) {
        Ok(result) => result.map_err(SbxError::from),
        Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {
            Err(SbxError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("retained process teardown timed out after {timeout:?}"),
            )))
        }
    }
}

fn read_worker_ready(
    mut output: Box<dyn Read + Send>,
    timeout: Duration,
) -> Result<(WorkerResponse, Box<dyn Read + Send>), SbxError> {
    let (send, receive) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let ready = read_frame::<WorkerResponse>(&mut output);
        let _ = send.send((ready, output));
    });
    match receive.recv_timeout(timeout) {
        Ok((Ok(ready), output)) => Ok((ready, output)),
        Ok((Err(error), _)) => Err(SbxError::Io(io::Error::other(format!(
            "worker readiness failed: {error}"
        )))),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(SbxError::WorkerLeaseLost(
            "worker readiness timed out".into(),
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(SbxError::WorkerLeaseLost(
            "worker readiness reader exited".into(),
        )),
    }
}

#[derive(Serialize)]
struct WorkerMarker<'a> {
    schema: &'static str,
    profile: &'a str,
    basis: &'static str,
}

#[derive(Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct OwnedWorkerMarker {
    schema: String,
    profile: String,
    basis: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HostIdentity {
    device: u64,
    inode: u64,
    uid: u32,
    gid: u32,
}

impl HostIdentity {
    fn read(path: &Path) -> Result<Self, SbxError> {
        if !safe_absolute(path) {
            return Err(SbxError::UnsafePath(path.to_owned()));
        }
        let metadata = fs::symlink_metadata(path).map_err(|source| SbxError::Metadata {
            path: path.to_owned(),
            source,
        })?;
        Self::from_metadata(path, &metadata)
    }

    fn from_metadata(path: &Path, metadata: &fs::Metadata) -> Result<Self, SbxError> {
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(SbxError::UnsafePath(path.to_owned()));
        }
        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(SbxError::WrongOwner(path.to_owned()));
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            uid: metadata.uid(),
            gid: metadata.gid(),
        })
    }
}

fn shared_grant_path(source: &Path, identity: &HostIdentity) -> PathBuf {
    let mut digest = Sha256::new();
    digest.update(source.as_os_str().as_bytes());
    digest.update(identity.device.to_be_bytes());
    digest.update(identity.inode.to_be_bytes());
    digest.update(identity.uid.to_be_bytes());
    digest.update(identity.gid.to_be_bytes());
    PathBuf::from(GRANT_ROOT)
        .join("shared")
        .join(format!("{:x}", digest.finalize()))
}

fn verify_pinned_grant_handles(
    reference: &GrantMountReference,
    source: &Path,
) -> Result<(), SbxError> {
    for handle in reference.pinned_sessions.values() {
        let metadata = handle
            .metadata()
            .map_err(|source_error| SbxError::Metadata {
                path: source.to_owned(),
                source: source_error,
            })?;
        if reference.identity != HostIdentity::from_metadata(source, &metadata)? {
            return Err(SbxError::SourceChanged(source.to_owned()));
        }
    }
    Ok(())
}

fn regular_executable(path: &Path) -> Result<(), SbxError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| SbxError::Metadata {
        path: path.to_owned(),
        source,
    })?;
    if !path.is_absolute() || !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(SbxError::UnsafeWorker(path.to_owned()));
    }
    Ok(())
}

// Kit VMs are explicitly Linux ARM64. The packaged helper is a required exact
// sibling artifact, never a custom-image dependency or architecture fallback.
/// The static job artifact (`docs/design/processes.md` s4): required beside the
/// worker, never optional.
fn job_artifact(worker: &Path) -> PathBuf {
    worker.with_file_name("marsh-local-linux-arm64")
}

fn byte_exec_artifact(worker: &Path) -> PathBuf {
    worker.with_file_name("marsh-byte-exec-linux-arm64")
}

fn worker_artifact_identity(worker: &Path) -> Result<String, SbxError> {
    // The static job artifact is installed with the worker and versioned
    // with it (`docs/design/processes.md` s4).
    Ok(format!(
        "{}:{}:{}",
        artifact_identity(worker)?,
        artifact_identity(&byte_exec_artifact(worker))?,
        artifact_identity(&job_artifact(worker))?
    ))
}

fn artifact_identity(path: &Path) -> Result<String, SbxError> {
    regular_executable(path)?;
    let mut source = fs::File::open(path).map_err(|source| SbxError::Metadata {
        path: path.to_owned(),
        source,
    })?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = source
            .read(&mut buffer)
            .map_err(|source| SbxError::Metadata {
                path: path.to_owned(),
                source,
            })?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn validate_name(value: &str) -> Result<(), SbxError> {
    if value.len() < 2
        || value.len() > 63
        || !value.as_bytes()[0].is_ascii_alphanumeric()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
    {
        return Err(SbxError::InvalidIdentifier(value.into()));
    }
    Ok(())
}

fn validate_token(value: &str) -> Result<(), SbxError> {
    if value.is_empty()
        || value.len() > 64
        || !value.as_bytes()[0].is_ascii_alphanumeric()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(SbxError::InvalidIdentifier(value.into()));
    }
    Ok(())
}

fn safe_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path != Path::new("/")
        && !path.as_os_str().as_encoded_bytes().contains(&0)
        && path.components().all(|component| {
            !matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
}

/// Per-user cache of verified local Kit job-image archives, keyed by Kit
/// source fingerprint and shared by this user's daemons and `make dev`
/// (`marshd --prebuild-kit-images`). It is never guest-mounted. Entries are
/// `<fp>.json` (fingerprint and manifest digest) naming
/// `<fp>.<manifest>.docker.tar`; both land by atomic rename, so concurrent
/// writers of one fingerprint never pair a record with another archive.
/// Every use is still checked against stock SBX's outer image digest and
/// the loaded nested image ID; a failed use evicts the entry.
fn private_cache_directory(directory: &Path) -> bool {
    if fs::create_dir_all(directory).is_err() {
        return false;
    }
    let Ok(metadata) = fs::symlink_metadata(directory) else {
        return false;
    };
    if !metadata.file_type().is_dir() || metadata.uid() != rustix::process::getuid().as_raw() {
        return false;
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).is_ok();
    }
    true
}

fn kit_image_cache_key(fingerprint: &str) -> Option<&str> {
    let key = fingerprint.strip_prefix("sha256:")?;
    (key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(key)
}

fn kit_image_cache_archive(directory: &Path, key: &str, manifest: &OciImage) -> Option<PathBuf> {
    let manifest = manifest.as_str().strip_prefix("sha256:")?;
    Some(directory.join(format!("{key}.{manifest}.docker.tar")))
}

fn cached_local_build(directory: &Path, fingerprint: &str) -> Option<LocalBuild> {
    let key = kit_image_cache_key(fingerprint)?;
    let record_path = directory.join(format!("{key}.json"));
    let regular = |path: &Path| {
        fs::symlink_metadata(path)
            .ok()
            .is_some_and(|metadata| metadata.file_type().is_file())
    };
    let metadata = fs::symlink_metadata(directory).ok()?;
    if !metadata.file_type().is_dir()
        || metadata.permissions().mode() & 0o077 != 0
        || !regular(&record_path)
    {
        return None;
    }
    let record: serde_json::Value = serde_json::from_slice(&fs::read(&record_path).ok()?).ok()?;
    if record.get("source_fingerprint")?.as_str()? != fingerprint {
        return None;
    }
    let manifest_digest =
        OciImage::parse(record.get("manifest_digest")?.as_str()?.to_owned()).ok()?;
    let archive = kit_image_cache_archive(directory, key, &manifest_digest)?;
    if !regular(&archive) {
        return None;
    }
    // Recency for pruning.
    if let Ok(file) = fs::OpenOptions::new().append(true).open(&record_path) {
        let _ = file.set_modified(SystemTime::now());
    }
    Some(LocalBuild {
        source_fingerprint: fingerprint.to_owned(),
        local_tag: format!("marsh-local-v3:{key}"),
        manifest_digest,
        docker_archive: archive,
        metadata: record_path,
        retained: true,
        _private_scratch: None,
    })
}

fn retain_local_build(directory: &Path, build: &LocalBuild) -> io::Result<()> {
    if build.retained {
        return Ok(());
    }
    let invalid = || io::Error::other("unexpected Kit image cache key");
    let key = kit_image_cache_key(&build.source_fingerprint).ok_or_else(invalid)?;
    let archive =
        kit_image_cache_archive(directory, key, &build.manifest_digest).ok_or_else(invalid)?;
    fs::set_permissions(&build.docker_archive, fs::Permissions::from_mode(0o600))?;
    if let Err(error) = fs::rename(&build.docker_archive, &archive) {
        // Another volume: copy beside the destination, then rename.
        if error.raw_os_error() != Some(rustix::io::Errno::XDEV.raw_os_error()) {
            return Err(error);
        }
        let staged = tempfile::NamedTempFile::new_in(directory)?;
        fs::copy(&build.docker_archive, staged.path())?;
        staged.as_file().sync_all()?;
        staged.persist(&archive).map_err(|error| error.error)?;
        let _ = fs::remove_file(&build.docker_archive);
    }
    let mut record = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(
        &mut record,
        &serde_json::json!({
            "source_fingerprint": build.source_fingerprint,
            "manifest_digest": build.manifest_digest.as_str(),
        }),
    )?;
    record.as_file().sync_all()?;
    record
        .persist(directory.join(format!("{key}.json")))
        .map_err(|error| error.error)?;
    let _ = fs::remove_file(&build.metadata);
    prune_kit_image_cache(directory, SystemTime::now());
    Ok(())
}

const KIT_IMAGE_CACHE_ENTRIES: usize = 24;
const KIT_IMAGE_CACHE_AGE: Duration = Duration::from_hours(24 * 14);
const KIT_IMAGE_CACHE_ORPHAN_AGE: Duration = Duration::from_hours(1);

/// Keep the most recently used entries (record mtime): at most
/// `KIT_IMAGE_CACHE_ENTRIES`, none unused for `KIT_IMAGE_CACHE_AGE`.
/// Archives without a record and abandoned build scratch go after an hour.
fn prune_kit_image_cache(directory: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let age = |metadata: &fs::Metadata| {
        metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .unwrap_or_default()
    };
    let mut records = Vec::new();
    let mut others = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        match name.strip_suffix(".json") {
            Some(key) if kit_image_cache_key(&format!("sha256:{key}")).is_some() => {
                records.push((age(&metadata), key.to_owned()));
            }
            _ => others.push((age(&metadata), name, metadata.is_dir())),
        }
    }
    records.sort();
    let mut kept = BTreeMap::new();
    for (index, (age, key)) in records.into_iter().enumerate() {
        let path = directory.join(format!("{key}.json"));
        if index >= KIT_IMAGE_CACHE_ENTRIES || age > KIT_IMAGE_CACHE_AGE {
            let _ = fs::remove_file(path);
        } else if let Some(manifest) = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|record| record.get("manifest_digest")?.as_str().map(str::to_owned))
        {
            kept.insert(key, manifest);
        }
    }
    for (age, name, directory_entry) in others {
        let live = name.split_once('.').is_some_and(|(key, rest)| {
            kept.get(key).is_some_and(|manifest| {
                manifest
                    .strip_prefix("sha256:")
                    .map(|hex| format!("{hex}.docker.tar"))
                    == Some(rest.to_owned())
            })
        });
        if live || age < KIT_IMAGE_CACHE_ORPHAN_AGE {
            continue;
        }
        let path = directory.join(&name);
        let _ = if directory_entry {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
    }
}

/// The pinned `sha256:` digest of a published `repository@sha256:...` image.
fn published_image_digest(reference: &OciImage) -> Option<&str> {
    let (repository, digest) = reference.as_str().rsplit_once('@')?;
    (!repository.is_empty() && valid_image_id(digest)).then_some(digest)
}

/// `repository:marsh-<digest hex>` for a published `repository@sha256:...`.
fn published_cache_tag(reference: &OciImage) -> Option<String> {
    let (repository, digest) = reference.as_str().rsplit_once('@')?;
    let hex = digest.strip_prefix("sha256:")?;
    let (parent, leaf) = repository
        .rsplit_once('/')
        .map_or(("", repository), |(parent, leaf)| (parent, leaf));
    let leaf = leaf.split_once(':').map_or(leaf, |(name, _)| name);
    let repository = if parent.is_empty() {
        leaf.to_owned()
    } else {
        format!("{parent}/{leaf}")
    };
    (!leaf.is_empty() && valid_image_id(digest)).then(|| format!("{repository}:marsh-{hex}"))
}

/// Published Kit images share the cache keyed by their immutable digest:
/// `<digest>.json` records the reference and names
/// `<digest>.<digest>.docker.tar`, so pruning treats them like local builds.
fn cached_published_image(directory: &Path, reference: &OciImage) -> Option<PathBuf> {
    let digest = OciImage::parse(published_image_digest(reference)?.to_owned()).ok()?;
    let key = kit_image_cache_key(digest.as_str())?;
    let record_path = directory.join(format!("{key}.json"));
    let metadata = fs::symlink_metadata(directory).ok()?;
    if !metadata.file_type().is_dir() || metadata.permissions().mode() & 0o077 != 0 {
        return None;
    }
    if !fs::symlink_metadata(&record_path)
        .ok()
        .is_some_and(|metadata| metadata.file_type().is_file())
    {
        return None;
    }
    let record: serde_json::Value = serde_json::from_slice(&fs::read(&record_path).ok()?).ok()?;
    if record.get("published_image")?.as_str()? != reference.as_str()
        || record.get("manifest_digest")?.as_str()? != digest.as_str()
    {
        return None;
    }
    let archive = kit_image_cache_archive(directory, key, &digest)?;
    if !fs::symlink_metadata(&archive)
        .ok()
        .is_some_and(|metadata| metadata.file_type().is_file())
    {
        return None;
    }
    if let Ok(file) = fs::OpenOptions::new().append(true).open(&record_path) {
        let _ = file.set_modified(SystemTime::now());
    }
    Some(archive)
}

fn evict_published_image(directory: &Path, reference: &OciImage) {
    let Some(digest) = published_image_digest(reference)
        .and_then(|digest| OciImage::parse(digest.to_owned()).ok())
    else {
        return;
    };
    if let Some(key) = kit_image_cache_key(digest.as_str()) {
        let _ = fs::remove_file(directory.join(format!("{key}.json")));
        if let Some(archive) = kit_image_cache_archive(directory, key, &digest) {
            let _ = fs::remove_file(archive);
        }
    }
}

/// Upper bound for one saved published image archive.
const PUBLISHED_IMAGE_ARCHIVE_LIMIT: u64 = 16 << 30;

/// Run `docker image save` in the worker VM into a private temporary file,
/// then publish the archive and its record by atomic rename.
fn store_published_image(
    runner: &dyn CommandRunner,
    invocation: &Invocation,
    directory: &Path,
    reference: &OciImage,
    timeout: Duration,
) -> Result<(), SbxError> {
    let invalid = || SbxError::Io(io::Error::other("unexpected published image reference"));
    let digest = published_image_digest(reference)
        .and_then(|digest| OciImage::parse(digest.to_owned()).ok())
        .ok_or_else(invalid)?;
    let key = kit_image_cache_key(digest.as_str()).ok_or_else(invalid)?;
    let archive = kit_image_cache_archive(directory, key, &digest).ok_or_else(invalid)?;
    if cached_published_image(directory, reference).is_some() {
        return Ok(());
    }
    let mut staged = tempfile::Builder::new()
        .prefix(".save-")
        .tempfile_in(directory)?;
    let attachment = runner.spawn_attached(invocation)?;
    drop(attachment.stdin);
    let mut process = attachment.process;
    let mut stdout = attachment.stdout;
    let mut stderr = attachment.stderr;
    let drained = thread::spawn(move || io::copy(&mut stderr, &mut io::sink()));
    let deadline = Instant::now() + timeout;
    let copied = (|| -> io::Result<u64> {
        let mut total = 0_u64;
        let mut buffer = vec![0_u8; 1 << 20];
        loop {
            let count = stdout.read(&mut buffer)?;
            if count == 0 {
                return Ok(total);
            }
            total += count as u64;
            if total > PUBLISHED_IMAGE_ARCHIVE_LIMIT || Instant::now() >= deadline {
                return Err(io::Error::other("published image archive over its bound"));
            }
            staged.as_file_mut().write_all(&buffer[..count])?;
        }
    })();
    if copied.is_err() {
        let _ = process.terminate();
    }
    let status = process.wait();
    let _ = drained.join();
    let copied = copied?;
    if status? != 0 || copied == 0 {
        return Err(SbxError::ImageVerificationFailed);
    }
    staged.as_file().sync_all()?;
    fs::set_permissions(staged.path(), fs::Permissions::from_mode(0o600))?;
    staged.persist(&archive).map_err(|error| error.error)?;
    let mut record = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer(
        &mut record,
        &serde_json::json!({
            "published_image": reference.as_str(),
            "manifest_digest": digest.as_str(),
        }),
    )?;
    record.as_file().sync_all()?;
    record
        .persist(directory.join(format!("{key}.json")))
        .map_err(|error| error.error)?;
    prune_kit_image_cache(directory, SystemTime::now());
    Ok(())
}

fn evict_kit_image_cache(directory: &Path, build: &LocalBuild) {
    if let Some(key) = kit_image_cache_key(&build.source_fingerprint) {
        let _ = fs::remove_file(directory.join(format!("{key}.json")));
    }
    let _ = fs::remove_file(&build.docker_archive);
}

fn local_kit_marker(identity: &str, fingerprint: &str, lifecycle_identity: &str) -> String {
    format!("kit:{identity}:{fingerprint}:{lifecycle_identity}")
}

#[allow(clippy::case_sensitive_file_extension_comparisons)] // Stock DetectV3SourceKit is lowercase-only.
fn local_v3_descriptor(source: &Path) -> Result<PathBuf, SbxError> {
    fn schema_version_three(value: &serde_yaml::Value) -> bool {
        let version = value.get("schemaVersion");
        version.is_some_and(|version| version.as_str() == Some("3") || version.as_u64() == Some(3))
    }

    fn yaml_is_v3(path: &Path) -> Result<bool, SbxError> {
        let contents = fs::read(path).map_err(|source| SbxError::Metadata {
            path: path.to_owned(),
            source,
        })?;
        let value = serde_yaml::from_slice(&contents).map_err(|source| {
            SbxError::InvalidLocalKitDescriptor {
                path: path.to_owned(),
                source,
            }
        })?;
        Ok(schema_version_three(&value))
    }

    fn dockerfile_is_v3(path: &Path) -> Result<bool, SbxError> {
        let contents = fs::read_to_string(path).map_err(|source| SbxError::Metadata {
            path: path.to_owned(),
            source,
        })?;
        let has_kit_marker = contents.lines().any(|line| line.trim_end() == "# kit:");
        if !has_kit_marker {
            return Ok(false);
        }
        for line in contents.lines() {
            let Some(comment) = line.strip_prefix('#') else {
                continue;
            };
            if !comment.starts_with(char::is_whitespace) {
                continue;
            }
            let Some(version) = comment
                .trim_start()
                .strip_prefix("schemaVersion:")
                .map(str::trim)
            else {
                continue;
            };
            if matches!(version, "3" | "\"3\"" | "'3'") {
                return Ok(true);
            }
        }
        Ok(false)
    }

    let mut descriptors = Vec::new();
    let entries = fs::read_dir(source).map_err(|error| SbxError::Metadata {
        path: source.to_owned(),
        source: error,
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| SbxError::Metadata {
            path: source.to_owned(),
            source: error,
        })?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if matches!(name, "spec.yaml" | "spec.yml") {
            return Err(SbxError::InvalidLocalKitSource(source.to_owned()));
        }
        let file_type = entry.file_type().map_err(|error| SbxError::Metadata {
            path: path.clone(),
            source: error,
        })?;
        if !file_type.is_file() {
            continue;
        }
        let is_v3 = if name.ends_with(".yaml") {
            yaml_is_v3(&path)?
        } else if name.ends_with(".dockerfile") {
            dockerfile_is_v3(&path)?
        } else {
            false
        };
        if is_v3 {
            descriptors.push(path);
        }
    }
    descriptors.sort();
    if descriptors.len() != 1 {
        return Err(SbxError::InvalidLocalKitSource(source.to_owned()));
    }
    Ok(descriptors.remove(0))
}

fn local_source_fingerprint(source: &Path) -> Result<String, SbxError> {
    fn hash_tree(root: &Path, path: &Path, digest: &mut Sha256) -> Result<(), SbxError> {
        let mut entries = fs::read_dir(path)
            .map_err(|error| SbxError::Metadata {
                path: path.to_owned(),
                source: error,
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| SbxError::Metadata {
                path: path.to_owned(),
                source: error,
            })?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            if entry.file_name() == ".git" {
                continue;
            }
            let entry_path = entry.path();
            let file_type = entry.file_type().map_err(|error| SbxError::Metadata {
                path: entry_path.clone(),
                source: error,
            })?;
            let metadata =
                fs::symlink_metadata(&entry_path).map_err(|error| SbxError::Metadata {
                    path: entry_path.clone(),
                    source: error,
                })?;
            let relative = entry_path
                .strip_prefix(root)
                .map_err(|_| SbxError::InvalidLocalKitSource(root.to_owned()))?;
            digest.update(relative.as_os_str().as_encoded_bytes());
            digest.update([0]);
            digest.update(metadata.mode().to_be_bytes());
            if file_type.is_dir() {
                digest.update(b"directory\0");
                hash_tree(root, &entry_path, digest)?;
            } else if file_type.is_file() {
                digest.update(b"file\0");
                let mut file = fs::File::open(&entry_path).map_err(|error| SbxError::Metadata {
                    path: entry_path.clone(),
                    source: error,
                })?;
                let mut buffer = [0_u8; 16 * 1024];
                loop {
                    let read = file.read(&mut buffer).map_err(|error| SbxError::Metadata {
                        path: entry_path.clone(),
                        source: error,
                    })?;
                    if read == 0 {
                        break;
                    }
                    digest.update(&buffer[..read]);
                }
                digest.update([0]);
            } else if file_type.is_symlink() {
                digest.update(b"symlink\0");
                let target = fs::read_link(&entry_path).map_err(|error| SbxError::Metadata {
                    path: entry_path.clone(),
                    source: error,
                })?;
                digest.update(target.as_os_str().as_encoded_bytes());
                digest.update([0]);
            } else {
                return Err(SbxError::InvalidLocalKitSource(entry_path));
            }
        }
        Ok(())
    }

    let mut digest = Sha256::new();
    hash_tree(source, source, &mut digest)?;
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn sweep_stale_local_scratch(directory: &Path, now: SystemTime) -> Result<(), SbxError> {
    fn protocol_scratch_pid(name: &OsStr) -> Option<u32> {
        let name = name.to_str()?;
        let remainder = name.strip_prefix(".marsh-kit-")?;
        let (identity, suffix) = remainder.rsplit_once('.')?;
        let suffix_matches = suffix == "tar" && identity.ends_with(".docker")
            || suffix == "json" && identity.ends_with(".metadata");
        if !suffix_matches {
            return None;
        }
        let identity = identity
            .strip_suffix(".docker")
            .or_else(|| identity.strip_suffix(".metadata"))
            .expect("suffix checked above");
        let (pid, sequence) = identity.split_once('-')?;
        if sequence.is_empty() || !sequence.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        pid.parse().ok()
    }

    // Re-check the lifecycle directory immediately before inspecting entries.
    // This both proves daemon ownership and rejects a directory symlink.
    HostIdentity::read(directory)?;
    let entries = fs::read_dir(directory)
        .map_err(|source| SbxError::Metadata {
            path: directory.to_owned(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| SbxError::Metadata {
            path: directory.to_owned(),
            source,
        })?;
    for entry in entries {
        let Some(pid) = protocol_scratch_pid(&entry.file_name()) else {
            continue;
        };
        // Scratch from this daemon process can still belong to a concurrent
        // resolution for another Kit, regardless of its apparent age.
        if pid == std::process::id() {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|source| SbxError::Metadata {
            path: path.clone(),
            source,
        })?;
        if !metadata.file_type().is_file() || metadata.uid() != rustix::process::geteuid().as_raw()
        {
            continue;
        }
        let modified = metadata.modified().map_err(|source| SbxError::Metadata {
            path: path.clone(),
            source,
        })?;
        if now.duration_since(modified).unwrap_or_default() < LOCAL_SCRATCH_STALE_AFTER {
            continue;
        }
        fs::remove_file(&path).map_err(|source| SbxError::Metadata { path, source })?;
    }
    Ok(())
}

fn valid_image_id(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

/// Stable, non-disclosing identity for a selected home backing.
///
/// # Errors
/// Rejects unsafe, non-owned, or missing backing directories.
pub fn selected_home_identity(path: &Path) -> Result<String, SbxError> {
    let identity = HostIdentity::read(path)?;
    let mut digest = Sha256::new();
    digest.update(path.as_os_str().as_encoded_bytes());
    digest.update(identity.device.to_be_bytes());
    digest.update(identity.inode.to_be_bytes());
    digest.update(identity.uid.to_be_bytes());
    digest.update(identity.gid.to_be_bytes());
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn mount_spec(host: &Path, target: &Path, mode: &str) -> Result<OsString, SbxError> {
    // Stock SBX resolves a relative host path against its process cwd before
    // submitting the structured mount request. Use `.` from the admitted
    // source itself so legal host punctuation never enters the colon-delimited
    // CLI grammar.
    if host == target && mode == "rw" {
        return Ok(OsString::from("."));
    }
    if target.as_os_str().as_encoded_bytes().contains(&b':') {
        return Err(SbxError::UnsafePath(target.to_owned()));
    }
    let mut value = OsString::from(".:");
    value.push(target);
    value.push(":");
    value.push(mode);
    Ok(value)
}

fn unmount_spec(host: &Path, target: &Path) -> Result<OsString, SbxError> {
    if host == target {
        return Ok(OsString::from("."));
    }
    if target.as_os_str().as_encoded_bytes().contains(&b':') {
        return Err(SbxError::UnsafePath(target.to_owned()));
    }
    let mut value = OsString::from(".:");
    value.push(target);
    Ok(value)
}

#[allow(clippy::needless_pass_by_value)] // Own diagnostics after the command result is rejected.
fn command_failed(operation: &'static str, output: CommandOutput) -> SbxError {
    SbxError::CommandFailed {
        operation,
        status: output.exit_code,
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    }
}

fn combined_output(output: &CommandOutput) -> String {
    let mut bytes = output.stdout.clone();
    bytes.extend_from_slice(&output.stderr);
    String::from_utf8_lossy(&bytes).into_owned()
}

fn normalized_output(output: &CommandOutput) -> String {
    combined_output(output)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Rows of `sbx template ls --json`, which stock SBX has emitted both as
/// `{"images": [...]}` and as a bare `[...]`.
fn template_list_rows(listed: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
    listed
        .as_array()
        .or_else(|| listed.get("images").and_then(serde_json::Value::as_array))
}

fn supported_sbx_version(output: &str) -> bool {
    output.lines().any(|line| {
        // Stock SBX has printed both `sbx version: vX` and `sbx version vX`.
        let Some(record) = line.trim().strip_prefix("sbx version") else {
            return false;
        };
        let record = record.strip_prefix(':').unwrap_or(record);
        if !record.starts_with(char::is_whitespace) {
            return false;
        }
        let mut fields = record.split_whitespace();
        let (Some(version), Some(commit), None) = (fields.next(), fields.next(), fields.next())
        else {
            return false;
        };
        if commit.len() != 40 || !commit.bytes().all(is_lower_hex_digit) {
            return false;
        }

        let Some(version) = version.strip_prefix('v') else {
            return false;
        };
        let parts = version.split('-').collect::<Vec<_>>();
        let Some(core) = parts.first().and_then(|core| parse_sbx_semver(core)) else {
            return false;
        };
        if core < MINIMUM_SBX_VERSION {
            return false;
        }
        match parts.as_slice() {
            [_, distance, abbreviated_commit] => {
                let Some(distance) = distance.parse::<u64>().ok() else {
                    return false;
                };
                let Some(abbreviated_commit) = abbreviated_commit.strip_prefix('g') else {
                    return false;
                };
                if distance == 0
                    || !(7..=40).contains(&abbreviated_commit.len())
                    || !abbreviated_commit.bytes().all(is_lower_hex_digit)
                    || !commit.starts_with(abbreviated_commit)
                {
                    return false;
                }

                true
            }
            [_] => true,
            _ => false,
        }
    })
}

fn parse_sbx_semver(value: &str) -> Option<(u64, u64, u64)> {
    let mut numbers = value.split('.').map(str::parse::<u64>);
    match (
        numbers.next(),
        numbers.next(),
        numbers.next(),
        numbers.next(),
    ) {
        (Some(Ok(major)), Some(Ok(minor)), Some(Ok(patch)), None) => Some((major, minor, patch)),
        _ => None,
    }
}

const fn is_lower_hex_digit(byte: u8) -> bool {
    byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
}

fn stale_shared_mount_error(error: &SbxError) -> bool {
    let SbxError::CommandFailed { stderr, .. } = error else {
        return false;
    };
    let stderr = stderr.to_ascii_lowercase();
    stderr.contains("already mounted")
        || stderr.contains("already exists")
        || stderr.contains("is mounted")
}

fn sandbox_missing(output: &CommandOutput) -> bool {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    stderr.contains("not found") || stderr.contains("does not exist")
}

fn missing_vm_error(error: &SbxError) -> bool {
    let message = match error {
        SbxError::CommandFailed { stderr, .. } => stderr.as_str(),
        SbxError::Io(source) => return missing_vm_message(&source.to_string()),
        _ => return false,
    };
    missing_vm_message(message)
}

fn missing_vm_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("sandbox not found")
        || message.contains("sandbox does not exist")
        || message.contains("container missing before start")
}

#[derive(Debug, Error)]
pub enum SbxError {
    #[error("stock SBX I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("stock SBX {operation} failed ({status:?}): {stderr}")]
    CommandFailed {
        operation: &'static str,
        status: Option<i32>,
        stderr: String,
    },
    #[error("stock preparation operation {operation} timed out after {timeout:?}")]
    PreparationTimeout {
        operation: &'static str,
        timeout: Duration,
    },
    #[error("scope lifecycle deadline expired before {operation}")]
    LifecycleDeadline { operation: &'static str },
    #[error("stock SBX lifecycle schema mismatch during {operation}: {detail}")]
    LifecycleSchema {
        operation: &'static str,
        detail: String,
    },
    #[error("unsupported stock SBX version {found:?}; requires {required}")]
    UnsupportedVersion {
        found: String,
        required: &'static str,
    },
    #[error("stock SBX lacks required capability: {0}")]
    MissingCapability(&'static str),
    #[error("existing sandbox name is not an owned compatible kit VM: {0}")]
    ForeignVm(String),
    #[error("invalid SBX identifier: {0}")]
    InvalidIdentifier(String),
    #[error("unsafe worker artifact: {0}")]
    UnsafeWorker(PathBuf),
    #[error("unsafe grant or result path: {0}")]
    UnsafePath(PathBuf),
    #[error("grant source is not owned by the daemon user: {0}")]
    WrongOwner(PathBuf),
    #[error("grant source changed while SBX prepared it: {0}")]
    SourceChanged(PathBuf),
    #[error("host source admission failed: {0}")]
    HostGrantFence(String),
    #[error("unsafe source ancestry at {path:?}: {reason}")]
    UnsafeSourceAncestor { path: PathBuf, reason: &'static str },
    #[error(
        "stock control cleanup remains uncertain: control={control_error:?}, cancel={cancel_error:?}, terminate={terminate_error:?}, wait={wait_error:?}, reaped={reaped}, readers_joined={readers_joined}; source observation is unusable"
    )]
    StockControlCleanupUncertain {
        control_error: Option<String>,
        cancel_error: Option<String>,
        terminate_error: Option<String>,
        wait_error: Option<String>,
        reaped: bool,
        readers_joined: bool,
    },
    #[error("grant preparation failed: {cause}; rollback complete: {rollback_complete}")]
    GrantPreparationFailed {
        cause: Box<SbxError>,
        rollback_complete: bool,
    },
    #[error("at least one grant is required")]
    NoGrants,
    #[error("shell user must be a non-root exact guest identity")]
    InvalidShellUser,
    #[error("terminal mode and initial dimensions are inconsistent")]
    InvalidTerminalSize,
    #[error("invalid native stock-SBX kit reference")]
    InvalidNativeKit,
    #[error("invalid local Docker Sandbox Kit v3 source: {0}")]
    InvalidLocalKitSource(PathBuf),
    #[error("invalid local Docker Sandbox Kit v3 descriptor {path}: {source}")]
    InvalidLocalKitDescriptor {
        path: PathBuf,
        source: serde_yaml::Error,
    },
    #[error(
        "stock SBX's build of this local Kit does not match marsh's host build ({0}); both must build on the same host Docker BuildKit: check that `docker buildx` selects a local builder"
    )]
    LocalKitDigestMismatch(String),
    #[error("stock Docker local Kit build metadata lacks an immutable manifest digest")]
    InvalidLocalBuildMetadata,
    #[error("trusted worker lease exited before dispatch: {0}")]
    WorkerLeaseLost(String),
    #[error(
        "retained worker Start completion is ambiguous for {vm}; cleanup is uncertain and the VM was quarantined: {detail}"
    )]
    AmbiguousWorkerStart { vm: String, detail: String },
    #[error(
        "worker VM is quarantined and cannot be reused: {0}; retire it with host `marsh workers reset KIT`"
    )]
    QuarantinedVm(String),
    #[error(
        "shell VM is quarantined and cannot be reused: {0}; exit other shells, then run host `marsh reset` or `marsh stop` for this selected home"
    )]
    QuarantinedShellVm(String),
    #[error(
        "shell mount admission is busy; no stock operation was submitted; retry the shell open"
    )]
    ShellMountAdmissionBusy,
    #[error(
        "shell mount cleanup could not be verified: {0}; run host `marsh reset` after other shells exit"
    )]
    ShellMountCleanupUncertain(String),
    #[error(
        "shell start through the VM supervisor is uncertain: {0}; retain session authority until explicit VM recovery"
    )]
    ShellStartUncertain(String),
    #[error(
        "worker VM {0} is still in use by a running job or an attached shell that ran it; wait for the job or exit that shell, then retry"
    )]
    ActiveWorkerState(String),
    #[error("shell session is closed against new grant preparation: {0}")]
    ClosedSession(String),
    #[error("trusted shell lease exited before attach: {0}")]
    ShellLeaseLost(String),
    #[error("shell mount is not held by this daemon: {0}")]
    UnknownShellMount(PathBuf),
    #[error("loaded worker image did not retain the requested immutable digest")]
    ImageVerificationFailed,
    #[error("cannot inspect {path}: {source}")]
    Metadata {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid immutable image: {0}")]
    InvalidImage(#[from] marsh_contracts::JobSpecError),
    #[error("invalid worker ownership JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests;
