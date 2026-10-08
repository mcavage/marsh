//! Bounded daemon bootstrap observation, separate from endpoint reconciliation.
//!
//! Stderr is a private socketpair, never a log in a guest-visible directory.
//! The parent drains it nonblockingly while observing the actual child status.
//! No reader thread can outlive a failed launch or wait for a descendant's EOF.

use super::{Client, DaemonError, EndpointPaths, PublicReply, PublicRequest};
use marsh_contracts::JobResources;
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt},
        io::OwnedFd,
        net::UnixStream,
        process::CommandExt,
    },
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const STDERR_LIMIT: usize = 8192;
const CAPTURE_ENV: &str = "MARSH_INTERNAL_STARTUP_STDERR";

/// The defaults used by the real daemon, not a status-only copy.
pub const DEFAULT_JOB_RESOURCES: JobResources = JobResources {
    cpu_millis: 4_000,
    memory_bytes: 8 * 1024 * 1024 * 1024,
    pids: 4_096,
    writable_bytes: 10 * 1024 * 1024 * 1024,
    output_bytes: 256 * 1024 * 1024,
    wall_seconds: 24 * 60 * 60,
};

/// Effective per-job policy, captured once when this daemon starts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct JobDefaults {
    pub resources: JobResources,
    /// Fixed variable names supplied at daemon launch. All other fields use
    /// the compiled defaults. Values are already represented in `resources`
    /// and `tree_kit_vms`.
    pub environment_overrides: Vec<String>,
    /// Distinct Kit VMs one job tree may use at once (`MARSH_TREE_KIT_VMS`).
    #[serde(default = "default_tree_kit_vms")]
    pub tree_kit_vms: usize,
}

fn default_tree_kit_vms() -> usize {
    marsh_contracts::process::TREE_KIT_VM_LIMIT
}

impl Default for JobDefaults {
    fn default() -> Self {
        Self {
            resources: DEFAULT_JOB_RESOURCES,
            environment_overrides: Vec::new(),
            tree_kit_vms: default_tree_kit_vms(),
        }
    }
}

impl JobDefaults {
    pub fn from_environment(
        mut value: impl FnMut(&str) -> Option<OsString>,
    ) -> Result<Self, DaemonError> {
        let mut result = Self::default();
        for name in JOB_ENVIRONMENT {
            let Some(raw) = value(name) else { continue };
            let parsed = raw
                .to_str()
                .filter(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
                .and_then(|text| text.parse::<u64>().ok())
                .filter(|number| *number > 0)
                .ok_or_else(|| {
                    DaemonError::InvalidState(format!(
                        "{name} must be a positive decimal integer within its supported range"
                    ))
                })?;
            match name {
                "MARSH_JOB_CPU_MILLIS" => {
                    result.resources.cpu_millis =
                        u32::try_from(parsed).map_err(|_| limit_range(name))?;
                }
                "MARSH_JOB_MEMORY_BYTES" => result.resources.memory_bytes = parsed,
                "MARSH_JOB_PIDS" => {
                    result.resources.pids = u32::try_from(parsed).map_err(|_| limit_range(name))?;
                }
                "MARSH_JOB_WRITABLE_BYTES" => result.resources.writable_bytes = parsed,
                "MARSH_JOB_OUTPUT_BYTES" => result.resources.output_bytes = parsed,
                "MARSH_JOB_WALL_SECONDS" => result.resources.wall_seconds = parsed,
                "MARSH_TREE_KIT_VMS" => {
                    result.tree_kit_vms = usize::try_from(parsed)
                        .ok()
                        .filter(|limit| *limit <= 64)
                        .ok_or_else(|| {
                            DaemonError::InvalidState(
                                "MARSH_TREE_KIT_VMS must be between 1 and 64".into(),
                            )
                        })?;
                }
                _ => unreachable!("fixed job environment variable"),
            }
            result.environment_overrides.push(name.into());
        }
        Ok(result)
    }

    fn value(&self, name: &str) -> u64 {
        match name {
            "MARSH_JOB_CPU_MILLIS" => u64::from(self.resources.cpu_millis),
            "MARSH_JOB_MEMORY_BYTES" => self.resources.memory_bytes,
            "MARSH_JOB_PIDS" => u64::from(self.resources.pids),
            "MARSH_JOB_WRITABLE_BYTES" => self.resources.writable_bytes,
            "MARSH_JOB_OUTPUT_BYTES" => self.resources.output_bytes,
            "MARSH_JOB_WALL_SECONDS" => self.resources.wall_seconds,
            "MARSH_TREE_KIT_VMS" => self.tree_kit_vms as u64,
            _ => unreachable!("fixed job environment variable"),
        }
    }

    fn validate_resident(&self, client: &Client) -> Result<(), DaemonError> {
        if self.environment_overrides.is_empty() {
            return Ok(());
        }
        let effective = client.status(None)?.job_defaults.ok_or_else(|| DaemonError::InvalidState(
            "running daemon does not report job defaults; cannot verify explicit MARSH_JOB_* settings".into()))?;
        for name in &self.environment_overrides {
            if self.value(name) != effective.value(name) {
                return Err(DaemonError::InvalidState(format!(
                    "{name} differs from the running daemon's startup snapshot; inspect `marsh status` with MARSH_JOB_* unset. Use a separate MARSH_HOME, or close active work, run `marsh stop`, then launch with the desired settings"
                )));
            }
        }
        Ok(())
    }
}

const JOB_ENVIRONMENT: [&str; 7] = [
    "MARSH_JOB_CPU_MILLIS",
    "MARSH_JOB_MEMORY_BYTES",
    "MARSH_JOB_PIDS",
    "MARSH_JOB_WRITABLE_BYTES",
    "MARSH_JOB_OUTPUT_BYTES",
    "MARSH_JOB_WALL_SECONDS",
    "MARSH_TREE_KIT_VMS",
];

fn limit_range(name: &str) -> DaemonError {
    DaemonError::InvalidState(format!("{name} exceeds u32"))
}

#[derive(Debug, thiserror::Error)]
#[error("resident daemon exited during startup ({status}): {reason}")]
pub struct StartupFailure {
    pub status: ExitStatus,
    pub reason: String,
}

/// Adopt the launcher's read-only flock without an unsafe inherited-FD API.
/// Unused daemon stdin carries the guard until we replace it with /dev/null.
/// Holding a CLOEXEC duplicate prevents another launch if the client dies
/// during pre-bind configuration. Drop it after publishing the endpoint.
pub fn adopt_startup_lock() -> io::Result<Option<File>> {
    if std::env::var_os(CAPTURE_ENV).as_deref() != Some(std::ffi::OsStr::new("1")) {
        return Ok(None);
    }
    // The kernel chooses an unused descriptor above stdio; no fixed fd3 is used.
    let guard = File::from(rustix::io::fcntl_dupfd_cloexec(rustix::stdio::stdin(), 3)?);
    let metadata = guard.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "invalid private startup guard",
        ));
    }
    let null = File::open("/dev/null")?;
    rustix::stdio::dup2_stdin(&null)?;
    Ok(Some(guard))
}

/// Called by the real daemon only after startup has finished. Captured stderr
/// is a startup channel, not a lifelong logger whose reader could disappear.
pub fn finish_startup_diagnostics() -> io::Result<()> {
    if std::env::var_os(CAPTURE_ENV).as_deref() == Some(std::ffi::OsStr::new("1")) {
        let null = OpenOptions::new().write(true).open("/dev/null")?;
        rustix::stdio::dup2_stderr(&null)?;
    }
    Ok(())
}

struct StartupChild {
    child: Child,
    stderr: UnixStream,
    bytes: Vec<u8>,
    truncated: bool,
    released: bool,
}

impl StartupChild {
    fn spawn(home: &Path, daemon: &Path, stock: &Path, launch: &File) -> Result<Self, DaemonError> {
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        let child = Command::new(daemon)
            .arg("--home")
            .arg(home)
            .env("MARSH_SBX", stock)
            .env(CAPTURE_ENV, "1")
            .stdin(Stdio::from(launch.try_clone()?))
            .stdout(Stdio::null())
            .stderr(Stdio::from(OwnedFd::from(writer)))
            .process_group(0)
            .spawn()
            .map_err(DaemonError::Spawn)?;
        Ok(Self {
            child,
            stderr: reader,
            bytes: Vec::new(),
            truncated: false,
            released: false,
        })
    }

    fn drain(&mut self) -> io::Result<bool> {
        let mut buffer = [0; 4096];
        // Bounded work per poll even if a broken child floods diagnostics.
        for _ in 0..16 {
            match self.stderr.read(&mut buffer) {
                Ok(0) => return Ok(true),
                Ok(count) => {
                    let excess = (self.bytes.len() + count).saturating_sub(STDERR_LIMIT);
                    drop(self.bytes.drain(..excess));
                    self.bytes.extend_from_slice(&buffer[..count]);
                    self.truncated |= excess > 0;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(true),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(false)
    }

    fn failure(&mut self) -> Result<Option<StartupFailure>, DaemonError> {
        const TRUNCATED: &str = "[startup diagnostics truncated]\n";
        self.drain()?;
        let Some(status) = self.child.try_wait()? else {
            return Ok(None);
        };
        // Drain queued tail diagnostics without waiting for EOF: a descendant
        // could still hold stderr. Even continuous writes cannot delay this
        // already-observed exit beyond one extra polling interval.
        let deadline = Instant::now() + POLL_INTERVAL;
        while !self.drain()? {
            if Instant::now() >= deadline {
                self.truncated = true;
                break;
            }
        }
        // Never pass terminal-control bytes from a startup diagnostic through.
        let mut reason: String = String::from_utf8_lossy(&self.bytes)
            .chars()
            .flat_map(|ch| {
                if ch.is_control() && ch != '\n' && ch != '\t' {
                    ch.escape_default().collect::<Vec<_>>()
                } else {
                    vec![ch]
                }
            })
            .collect();
        if reason.trim().is_empty() {
            reason = "no startup diagnostic was emitted".into();
        }
        if self.truncated || reason.len() > STDERR_LIMIT {
            let mut excess = reason.len().saturating_sub(STDERR_LIMIT - TRUNCATED.len());
            while !reason.is_char_boundary(excess) {
                excess += 1;
            }
            drop(reason.drain(..excess));
            reason.insert_str(0, TRUNCATED);
        }
        Ok(Some(StartupFailure {
            status,
            reason: reason.trim().into(),
        }))
    }
}

impl Drop for StartupChild {
    fn drop(&mut self) {
        if !self.released {
            // Only this launcher's exact child, never a pre-existing owner.
            // Keep the launch lock held through reaping, so a timed-out
            // pre-bind child cannot race a second launcher.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Serializes competing *launchers*, including the pre-bind config checks.
/// The persistent lock lives beside the endpoint so stale-endpoint validation
/// still accepts only its existing socket/token/lifecycle entries.
fn launch_lock(paths: &EndpointPaths, deadline: Instant) -> Result<File, DaemonError> {
    let base = paths
        .runtime_directory
        .parent()
        .ok_or_else(|| DaemonError::UnsafeHome(paths.runtime_directory.clone()))?;
    match fs::DirBuilder::new().mode(0o700).create(base) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    super::verify_owned_directory(base)?;
    if fs::symlink_metadata(base)?.mode() & 0o077 != 0 {
        return Err(DaemonError::UnsafeHome(base.into()));
    }
    let path = paths.runtime_directory.with_extension("startup-lock");
    let lock = File::from(
        rustix::fs::open(
            &path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .map_err(io::Error::from)?,
    );
    let metadata = lock.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(DaemonError::UnsafeHome(path));
    }
    loop {
        match rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(lock),
            Err(rustix::io::Errno::WOULDBLOCK) if Instant::now() < deadline => {
                thread::sleep(POLL_INTERVAL);
            }
            Err(rustix::io::Errno::WOULDBLOCK) => {
                return Err(DaemonError::StartupTimeout(paths.runtime_directory.clone()));
            }
            Err(error) => return Err(io::Error::from(error).into()),
        }
    }
}

fn ready_client(home: &Path, identity: &str) -> Option<Client> {
    let client = Client::connect(home).ok()?;
    match client
        .request_with_timeout(PublicRequest::Ping, Duration::from_millis(100))
        .ok()?
    {
        PublicReply::Pong { build_identity, .. } if build_identity.as_deref() == Some(identity) => {
            Some(client)
        }
        _ => None,
    }
}

pub(super) fn ensure_running(
    home: &Path,
    daemon: &Path,
    stock: &Path,
) -> Result<Client, DaemonError> {
    let requested = JobDefaults::from_environment(|name| std::env::var_os(name))?;
    let daemon = super::canonical_executable(daemon).map_err(|error| {
        DaemonError::InvalidState(format!(
            "cannot use resident daemon executable {}: {error}; reinstall the sibling marshd",
            daemon.display()
        ))
    })?;
    let stock = super::canonical_executable(stock).map_err(|error| {
        DaemonError::InvalidState(format!(
            "cannot use stock SBX executable {}: {error}; check MARSH_SBX",
            stock.display()
        ))
    })?;
    match super::ensure_home(home) {
        Err(DaemonError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
            // A competing first launcher may have created the home between
            // inspection and mkdir. Revalidate; never accept an arbitrary entry.
            EndpointPaths::for_home(home)?;
        }
        result => result?,
    }
    let identity = super::runtime_identity(&daemon, &stock)?;
    let paths = EndpointPaths::for_home(home)?;
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    if let Some(client) = ready_client(home, &identity) {
        requested.validate_resident(&client)?;
        return Ok(client);
    }
    let launch = launch_lock(&paths, deadline)?;
    if let Some(client) = Client::reconcile_resident(home, &paths, &identity)? {
        requested.validate_resident(&client)?;
        return Ok(client);
    }
    let live_owner = paths.lock.exists()
        && super::reclaim_stale_endpoint(&paths)? != super::ReclaimOutcome::Reclaimed;
    let mut child = if live_owner {
        None
    } else {
        Some(StartupChild::spawn(home, &daemon, &stock, &launch)?)
    };
    while Instant::now() < deadline {
        if let Some(child) = &mut child
            && let Some(failure) = child.failure()?
        {
            return Err(DaemonError::StartupFailed(failure));
        }
        if let Some(client) = ready_client(home, &identity) {
            requested.validate_resident(&client)?;
            if let Some(child) = &mut child {
                child.released = true;
            }
            return Ok(client);
        }
        thread::sleep(POLL_INTERVAL);
    }
    // Drop reaps only our own timed-out child. A pre-existing live owner is
    // never killed. A timeout is not stale-endpoint evidence.
    Err(DaemonError::StartupTimeout(paths.runtime_directory))
}
