//! Placement-independent job lifecycle with a narrow Docker CLI backend.
//!
//! The backend owns runtime access. A job receives only the mounts,
//! identity, and limits in its validated [`JobSpec`]; no runtime socket is
//! mounted into the container.

mod byte_carrier;
pub mod byte_exec;
mod cancellation;
pub use cancellation::Cancellation;
mod process_owner;
pub use process_owner::{
    RetainedProcess, cleanup_uncertain, finish_owned_process, poll_retained_processes,
    retained_processes,
};
mod command_start;
mod host_fd;
mod writable_monitor;
pub use command_start::command_not_started;
pub use host_fd::with_host_descriptor_creation_excluded;

use marsh_contracts::{ContainerId, JobSignal, JobSpec, JobSpecError, MountAccess, TerminalSize};
use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::{
    ffi::OsString,
    fs::{self, File},
    io::{self, Read, Seek, Write},
    os::fd::{AsFd, OwnedFd},
    os::unix::ffi::{OsStrExt, OsStringExt},
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    os::unix::process::{CommandExt, ExitStatusExt},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use thiserror::Error;

const DELETION_VERIFY_ATTEMPTS: usize = 5;
const DELETION_VERIFY_DELAY: Duration = Duration::from_millis(50);
// A just-attached container reports PID 0 while Docker is still starting it.
// Keep asking (~1 s) while it is `created`; any other PID-0 state ends the
// search. Without a PID neither the pids nor the writable monitor can run.
const RESOURCE_PID_ATTEMPTS: usize = 40;
const RESOURCE_PID_DELAY: Duration = Duration::from_millis(25);
// `pids.events` changes only when the kernel rejects a fork. A 50 ms sample
// interval bounds classification lag while avoiding 1,000 reads/second per job.
const RESOURCE_EVENT_POLL_DELAY: Duration = Duration::from_millis(50);
const ENGINE_IO_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_ENGINE_RESPONSE: u64 = 16 * 1024;
const TRUSTED_CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";
const STOCK_SBX_PROXY_ENDPOINT: &[u8] = b"http://gateway.docker.internal:3128";
const STOCK_SBX_MCP_URL: &[u8] = b"http://mcp-gateway.docker.internal/mcp";
const PROXY_VARIABLES: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NODE_USE_ENV_PROXY",
    "SBX_CRED_ANTHROPIC_MODE",
    "SBX_CRED_OPENAI_MODE",
    "SBX_CRED_GOOGLE_MODE",
    "SBX_CRED_PARALLEL_MODE",
    "SBX_CRED_GITHUB_MODE",
    "SBX_CRED_SBX-LOGIN_MODE",
    "MCP_GATEWAY_URL",
    "MCP_SENTINEL_TOKEN_NAME",
];

/// One argument-safe process invocation. No shell parses these fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Invocation {
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
    /// Optional process working directory used for argument formats that
    /// resolve relative paths before crossing an external API boundary.
    pub working_directory: Option<PathBuf>,
    /// Stock CLI selectors for this invocation only, applied after the
    /// runner's control environment. Never workload or credential values.
    pub environment: Vec<(OsString, OsString)>,
}

/// Captured result from a bounded runtime command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Runtime-issued terminal state captured before container deletion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeExit {
    pub code: i32,
    pub oom_killed: bool,
    pub pids_max_events: Option<u64>,
    pub writable_bytes: u64,
    /// The worker killed the container because its writable layer exceeded
    /// the job's limit (detect-and-kill; see `writable_monitor`).
    pub writable_exceeded: bool,
}

impl CommandOutput {
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        matches!(self.exit_code, Some(0))
    }
}

/// Child process used only to carry an attached container's byte streams.
#[allow(clippy::missing_errors_doc)] // Implementations report their underlying process I/O error.
pub trait AttachedProcess: Send {
    /// Exact locally owned process identity, for retained-cleanup receipts.
    fn local_pid(&self) -> Option<u32> {
        None
    }
    fn wait(&mut self) -> io::Result<i32>;
    fn try_wait(&mut self) -> io::Result<Option<i32>>;
    /// Observe exit without releasing the local group leader's PID identity.
    /// System implementations use WNOWAIT until group cleanup is finished.
    fn try_wait_unreaped(&mut self) -> io::Result<Option<i32>> {
        self.try_wait()
    }
    /// Requests cleanup of the retained local transport identity. Native Linux
    /// process-group success means signal delivery, not descendant absence;
    /// macOS additionally observes its retained group, excluding descendants
    /// that have left it. Neither result proves VM, container, or guest-session
    /// cleanup. Those authorities require their own runtime observations.
    fn terminate(&mut self) -> io::Result<()>;

    /// Whether all owned stream operations can be interrupted by `cancel_io`.
    fn supports_io_cancellation(&self) -> bool {
        false
    }

    /// Wake all local stream operations without claiming guest termination.
    fn cancel_io(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "opaque attachment I/O",
        ))
    }
}

pub trait AttachmentControl: Send + Sync {
    /// Forwards a signal to the exact attached process.
    ///
    /// # Errors
    /// Returns an error if the process is gone or signaling fails.
    fn signal(&self, signal: JobSignal) -> io::Result<()>;

    /// Changes the attached terminal's window size.
    ///
    /// # Errors
    /// Returns an error if the PTY is gone or resizing fails.
    fn resize(&self, size: TerminalSize) -> io::Result<()>;

    /// Terminate the entire exact guest session, then verify no live member
    /// remains. This is not ordinary foreground signal delivery. Implementations
    /// must return within a finite control-plane deadline; errors mean uncertain
    /// cleanup, never absence. Local proxy termination is not this receipt.
    ///
    /// # Errors
    /// Returns an error if exact cleanup cannot be verified.
    fn cleanup_session(&self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "session cleanup unavailable",
        ))
    }
}

struct NoAttachmentControl;

#[must_use]
pub fn no_attachment_control() -> Arc<dyn AttachmentControl> {
    Arc::new(NoAttachmentControl)
}

impl AttachmentControl for NoAttachmentControl {
    fn signal(&self, _signal: JobSignal) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no signal control",
        ))
    }

    fn resize(&self, _size: TerminalSize) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no terminal control",
        ))
    }
}

/// Attached stdin/stdout/stderr. Container completion is observed separately
/// through [`JobRuntime::wait`].
pub struct Attachment {
    pub stdin: Box<dyn Write + Send>,
    pub stdout: Box<dyn Read + Send>,
    pub stderr: Box<dyn Read + Send>,
    pub process: Box<dyn AttachedProcess>,
    pub control: Arc<dyn AttachmentControl>,
}

/// Injectable command boundary used by the Docker backend and deterministic tests.
#[allow(clippy::missing_errors_doc)] // Implementations report their underlying process I/O error.
pub trait CommandRunner: Send + Sync {
    /// Captured host control environment actually used by this runner. Opaque
    /// and delegated runners return None and cannot prove private-source GC.
    /// This contains only HOME and non-secret SDK selectors, never workload env.
    fn stock_control_environment(
        &self,
        _program: &Path,
    ) -> Option<(PathBuf, Vec<(OsString, OsString)>)> {
        None
    }

    fn run(&self, invocation: &Invocation) -> io::Result<CommandOutput>;
    /// Runs one trusted host control command with bounded leader lifetime and
    /// I/O. Expiry cancels I/O and requests termination while the leader remains
    /// pinned. Local signal delivery does not prove arbitrary descendants or
    /// remote work have stopped; see [`AttachedProcess::terminate`]. Reap failure
    /// retains polling ownership and reports uncertainty.
    /// Long-lived attached jobs use [`Self::spawn_attached`] instead.
    fn run_bounded(&self, invocation: &Invocation, timeout: Duration) -> io::Result<CommandOutput>;
    fn spawn_attached(&self, invocation: &Invocation) -> io::Result<Attachment>;
    fn spawn_pty(&self, invocation: &Invocation) -> io::Result<Attachment> {
        self.spawn_attached(invocation)
    }

    fn spawn_pty_sized(
        &self,
        invocation: &Invocation,
        size: TerminalSize,
    ) -> io::Result<Attachment> {
        let attachment = self.spawn_pty(invocation)?;
        attachment.control.resize(size)?;
        Ok(attachment)
    }
}

/// System process runner. Callers should execute it on an appropriate blocking
/// thread when integrating with an async supervisor.
#[derive(Clone)]
pub struct SystemCommandRunner {
    home: PathBuf,
    // Host control-plane selection only, never a workload environment copy.
    // Every stock CLI and native image observer must address the same daemon.
    stock_environment: Vec<(std::ffi::OsString, std::ffi::OsString)>,
}

impl std::fmt::Debug for SystemCommandRunner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SystemCommandRunner")
            .field("home", &self.home)
            .field("stock_selector_count", &self.stock_environment.len())
            .finish()
    }
}

impl SystemCommandRunner {
    #[must_use]
    pub fn new(home: impl Into<PathBuf>) -> Self {
        let stock_environment = [
            "DOCKER_SANDBOXES_API",
            "DOCKER_SANDBOXES_APP_NAME",
            "SANDBOXES_STORAGE_ROOT",
            "XDG_STATE_HOME",
        ]
        .into_iter()
        .filter_map(|key| {
            std::env::var_os(key)
                .filter(|value| !value.is_empty())
                .map(|value| (key.into(), value))
        })
        .collect();
        Self {
            home: home.into(),
            stock_environment,
        }
    }
}

struct SystemAttachedProcess {
    child: std::process::Child,
    process_group: bool,
    cancel_io: Arc<AtomicBool>,
    identity: Arc<Mutex<LocalProcessIdentity>>,
}

struct LocalProcessIdentity {
    pid: u32,
    reaped: bool,
}

impl LocalProcessIdentity {
    fn require_live(&self) -> io::Result<()> {
        if self.reaped {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "attachment process was reaped",
            ))
        } else {
            Ok(())
        }
    }

    fn exit_status_unreaped(&self) -> io::Result<Option<i32>> {
        use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
        self.require_live()?;
        let pid = i32::try_from(self.pid).map_err(io::Error::other)?;
        let pid = Pid::from_raw(pid).ok_or_else(|| io::Error::other("invalid child PID"))?;
        let status = waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )?;
        Ok(status.map(|status| {
            status
                .exit_status()
                .or_else(|| status.terminating_signal().map(|signal| 128 + signal))
                .unwrap_or(125)
        }))
    }

    #[cfg(target_os = "macos")]
    fn is_exited_group_singleton(&self) -> io::Result<bool> {
        use libproc::processes::{ProcFilter, pids_by_type};
        if self.exit_status_unreaped()?.is_none() {
            return Ok(false);
        }
        // Darwin killpg excludes zombies and returns EPERM for a zombie-only
        // group. proc_listpids snapshots live AND zombie members under the
        // kernel process-list lock, without filtering changed credentials.
        // Retaining this leader prevents PGID reuse. Only exact singleton
        // evidence can discharge EPERM; errors, empty, or extra PIDs cannot.
        Ok(pids_by_type(ProcFilter::ByProgramGroup { pgrpid: self.pid })? == [self.pid])
    }

    #[cfg(target_os = "macos")]
    fn await_group_termination(&self) -> io::Result<()> {
        // killpg succeeds when *any* member was signaled. A member with changed
        // credentials can remain alive even after that success. Keep the leader
        // unreaped until a native group snapshot proves it is the sole member.
        // Zombies other than the leader are conservatively retained as unknown.
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            if self.is_exited_group_singleton()? {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "owned process group termination remains uncertain after 500ms",
                ));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

struct PipeControl {
    identity: Arc<Mutex<LocalProcessIdentity>>,
}

struct PtyControl {
    identity: Arc<Mutex<LocalProcessIdentity>>,
    master: Arc<File>,
}

// O_NONBLOCK belongs only to the parent pipe ends / PTY master, never the
// child's standard descriptors. Poll plus the shared flag interrupts even an
// escaped descendant retaining the other end. No blocking helper thread exists.
pub struct CancellableFile {
    file: File,
    cancel: Cancellation,
    pty: bool,
    socket: bool,
}

impl CancellableFile {
    /// Imports a pipe/socket without changing its shared open-file description.
    /// Sockets use per-call DONTWAIT. Linux FIFOs are reopened through the owned
    /// fd for a private description. Other inherited FIFO platforms fail closed.
    /// Regular file IO cannot promise a nonblocking wake and is rejected.
    /// # Errors
    /// Returns an error for unsupported descriptors or nonblocking setup failure.
    pub fn from_file(file: File, cancel: &Cancellation) -> io::Result<Self> {
        let kind = file.metadata()?.file_type();
        if !kind.is_fifo() && !kind.is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "cancellable transport requires a pipe or socket",
            ));
        }
        if kind.is_socket() {
            return Ok(Self {
                file,
                cancel: cancel.clone(),
                pty: false,
                socket: true,
            });
        }
        #[cfg(target_os = "linux")]
        {
            let access = rustix::fs::fcntl_getfl(&file)? & rustix::fs::OFlags::ACCMODE;
            let reopened = fs::OpenOptions::new()
                .read(access != rustix::fs::OFlags::WRONLY)
                .write(access != rustix::fs::OFlags::RDONLY)
                .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC)
                .open(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
            let before = file.metadata()?;
            let after = reopened.metadata()?;
            if before.dev() != after.dev()
                || before.ino() != after.ino()
                || !after.file_type().is_fifo()
            {
                return Err(io::Error::other(
                    "private FIFO description identity changed",
                ));
            }
            let mut stream = Self::new(reopened, &cancel.0, false)?;
            stream.cancel = cancel.clone();
            Ok(stream)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private inherited FIFO descriptions unavailable; use an owned pipe end or socket",
            ))
        }
    }

    fn new(file: File, cancel: &Arc<AtomicBool>, pty: bool) -> io::Result<Self> {
        let flags = rustix::fs::fcntl_getfl(&file)?;
        rustix::fs::fcntl_setfl(&file, flags | rustix::fs::OFlags::NONBLOCK)?;
        Ok(Self {
            file,
            cancel: Cancellation(Arc::clone(cancel), None),
            pty,
            socket: false,
        })
    }

    fn ready(&self, events: nix::poll::PollFlags) -> io::Result<()> {
        let mut descriptors = [nix::poll::PollFd::new(self.file.as_fd(), events)];
        match nix::poll::poll(&mut descriptors, 20_u16) {
            Ok(_) | Err(nix::errno::Errno::EINTR) => Ok(()),
            Err(error) => Err(io::Error::other(error)),
        }
    }

    fn check_cancel(&self) -> io::Result<()> {
        self.cancel.check()
    }
}

impl Read for CancellableFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            self.check_cancel()?;
            let result = if self.socket {
                rustix::net::recv(&self.file, &mut *buffer, rustix::net::RecvFlags::DONTWAIT)
                    .map(|(read, _)| read)
                    .map_err(io::Error::from)
            } else {
                self.file.read(buffer)
            };
            let result = if self.pty {
                normalize_pty_read(result)
            } else {
                result
            };
            match result {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.ready(nix::poll::PollFlags::POLLIN)?;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }
}

impl CancellableFile {
    /// A scoped writer can cancel its own in-flight frame without cancelling
    /// unrelated queued writers. The transport token remains checked as well.
    /// # Errors
    /// Returns the underlying write error or cancellation.
    pub fn write_cancellable(&mut self, buffer: &[u8], cancel: &Cancellation) -> io::Result<usize> {
        loop {
            self.check_cancel()?;
            cancel.check()?;
            let result = if self.socket {
                rustix::net::send(&self.file, buffer, rustix::net::SendFlags::DONTWAIT)
                    .map_err(io::Error::from)
            } else {
                self.file.write(buffer)
            };
            match result {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.ready(nix::poll::PollFlags::POLLOUT)?;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }
}

impl Write for CancellableFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.write_cancellable(buffer, &Cancellation::default())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.check_cancel()
    }
}

fn normalize_pty_read(result: io::Result<usize>) -> io::Result<usize> {
    match result {
        // Linux PTY masters report EIO when the final slave closes. That is
        // the PTY equivalent of EOF, not lost output or a supervision error.
        Err(error) if error.raw_os_error() == Some(nix::libc::EIO) => Ok(0),
        result => result,
    }
}

/// Transfers a child to explicit, inspectable retained ownership. No detached
/// reaper thread is started. Call `poll_retained_processes` for bounded progress;
/// pending receipts are uncertainty, not successful cleanup.
pub fn retain_for_reaping(process: Box<dyn AttachedProcess>) {
    process_owner::retain(process);
}

fn run_attachment_bounded(attachment: Attachment, timeout: Duration) -> io::Result<CommandOutput> {
    run_attachment_interruptible(attachment, Some(timeout), &Cancellation::default(), false)
}

fn spawn_bounded_capture(
    stream: Box<dyn Read + Send>,
    label: &'static str,
) -> thread::JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut output = Vec::new();
        stream.take(8 * 1024 * 1024 + 1).read_to_end(&mut output)?;
        if output.len() > 8 * 1024 * 1024 {
            Err(io::Error::other(format!(
                "control {label} exceeded capture bound"
            )))
        } else {
            Ok(output)
        }
    })
}

fn run_attachment_interruptible(
    mut attachment: Attachment,
    timeout: Option<Duration>,
    cancel: &Cancellation,
    _reap_inline: bool,
) -> io::Result<CommandOutput> {
    if !attachment.process.supports_io_cancellation() {
        let _ = attachment.process.cancel_io();
        let terminated = attachment.process.terminate().is_ok();
        if !process_owner::poll_process(attachment.process.as_mut(), Duration::from_secs(2)) {
            return Err(process_owner::uncertainty(Some(process_owner::retain(
                attachment.process,
            ))));
        }
        if !terminated {
            return Err(process_owner::uncertainty(None));
        }
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "bounded command requires cancellable I/O",
        ));
    }
    let deadline = timeout.and_then(|timeout| std::time::Instant::now().checked_add(timeout));
    drop(attachment.stdin);
    let stdout = spawn_bounded_capture(attachment.stdout, "stdout");
    let stderr = spawn_bounded_capture(attachment.stderr, "stderr");
    let mut exit_code = None;
    let mut identity_uncertain = false;
    let outcome = loop {
        if exit_code.is_none() {
            match attachment.process.try_wait_unreaped() {
                Ok(code) => exit_code = code,
                Err(error) => {
                    identity_uncertain = true;
                    break Err(error);
                }
            }
        }
        if exit_code.is_some() && stdout.is_finished() && stderr.is_finished() {
            break Ok(());
        }
        if let Err(error) = cancel.check() {
            break Err(error);
        }
        if timeout.is_some()
            && deadline.is_none_or(|deadline| std::time::Instant::now() >= deadline)
        {
            break Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("command or output drain exceeded {timeout:?}"),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    let cancellation = if outcome.is_err() {
        attachment.process.cancel_io()
    } else {
        Ok(())
    };
    // WNOWAIT keeps the group leader's PID reserved through the mandatory
    // termination attempt, including descendants retaining output pipes.
    let termination = if identity_uncertain {
        Err(io::Error::other(
            "termination withheld after uncertain child identity observation",
        ))
    } else {
        attachment.process.terminate()
    };
    let reaped = process_owner::poll_process(attachment.process.as_mut(), Duration::from_secs(2));
    let retained = if reaped {
        None
    } else {
        Some(process_owner::retain(attachment.process))
    };
    // Only admitted cancellable implementations reach these joins; every I/O
    // operation is nonblocking and observes cancel within one 20 ms poll.
    let stdout = stdout
        .join()
        .map_err(|_| io::Error::other("stdout reader panicked"))?;
    let stderr = stderr
        .join()
        .map_err(|_| io::Error::other("stderr reader panicked"))?;
    if cancellation.is_err() || termination.is_err() || !reaped {
        return Err(process_owner::uncertainty(retained));
    }
    outcome?;
    Ok(CommandOutput {
        exit_code,
        stdout: stdout?,
        stderr: stderr?,
    })
}

fn signal_child(pid: u32, signal: JobSignal) -> io::Result<()> {
    let pid = i32::try_from(pid).map_err(|_| io::Error::other("child PID overflow"))?;
    let signal = match signal {
        JobSignal::Interrupt => nix::sys::signal::Signal::SIGINT,
        JobSignal::Terminate => nix::sys::signal::Signal::SIGTERM,
        JobSignal::Kill => nix::sys::signal::Signal::SIGKILL,
        JobSignal::Hangup => nix::sys::signal::Signal::SIGHUP,
    };
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), signal).map_err(io::Error::other)
}

impl AttachmentControl for PipeControl {
    fn signal(&self, signal: JobSignal) -> io::Result<()> {
        let identity = self
            .identity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        identity.require_live()?;
        signal_child(identity.pid, signal)
    }

    fn resize(&self, _size: TerminalSize) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "pipe attachment has no terminal",
        ))
    }
}

impl AttachmentControl for PtyControl {
    fn signal(&self, signal: JobSignal) -> io::Result<()> {
        let identity = self
            .identity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        identity.require_live()?;
        signal_child(identity.pid, signal)
    }

    fn resize(&self, size: TerminalSize) -> io::Result<()> {
        let identity = self
            .identity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        identity.require_live()?;
        rustix::termios::tcsetwinsize(
            &*self.master,
            rustix::termios::Winsize {
                ws_row: size.rows,
                ws_col: size.columns,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )?;
        let pid = i32::try_from(identity.pid).map_err(io::Error::other)?;
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGWINCH,
        )
        .map_err(io::Error::other)
    }
}

impl AttachedProcess for SystemAttachedProcess {
    fn local_pid(&self) -> Option<u32> {
        Some(self.child.id())
    }
    fn supports_io_cancellation(&self) -> bool {
        true
    }

    fn cancel_io(&self) -> io::Result<()> {
        self.cancel_io.store(true, Ordering::Release);
        Ok(())
    }

    fn wait(&mut self) -> io::Result<i32> {
        // Never hold the lifecycle fence across a blocking wait: signal and
        // resize must remain usable until the child has actually been reaped.
        loop {
            if let Some(code) = self.try_wait()? {
                return Ok(code);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        let mut identity = self
            .identity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let status = self.child.try_wait()?;
        identity.reaped |= status.is_some();
        Ok(status.map(|status| {
            status
                .code()
                .or_else(|| status.signal().map(|signal| 128 + signal))
                .unwrap_or(125)
        }))
    }

    fn try_wait_unreaped(&mut self) -> io::Result<Option<i32>> {
        let identity = self
            .identity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if identity.reaped {
            drop(identity);
            return self.try_wait();
        }
        identity.exit_status_unreaped()
    }

    fn terminate(&mut self) -> io::Result<()> {
        let identity = self
            .identity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if identity.reaped {
            return Ok(());
        }
        if !self.process_group {
            return self.child.kill();
        }
        let pid =
            i32::try_from(self.child.id()).map_err(|_| io::Error::other("child PID overflow"))?;
        let signaled = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(-pid),
            nix::sys::signal::Signal::SIGKILL,
        );
        #[cfg(target_os = "macos")]
        match signaled {
            Ok(()) | Err(nix::errno::Errno::ESRCH | nix::errno::Errno::EPERM) => {
                identity.await_group_termination()
            }
            Err(error) => Err(io::Error::other(error)),
        }
        #[cfg(not(target_os = "macos"))]
        match signaled {
            // Linux group signaling may succeed after partial delivery. This
            // trusted transport result is not a descendant-containment receipt.
            Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
            Err(error) => Err(io::Error::other(error)),
        }
    }
}

impl CommandRunner for SystemCommandRunner {
    fn stock_control_environment(
        &self,
        _program: &Path,
    ) -> Option<(PathBuf, Vec<(OsString, OsString)>)> {
        Some((self.home.clone(), self.stock_environment.clone()))
    }

    fn run(&self, invocation: &Invocation) -> io::Result<CommandOutput> {
        let mut command = Command::new(&invocation.program);
        command
            .args(&invocation.arguments)
            .env_clear()
            .env("PATH", "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin")
            .env("HOME", &self.home)
            .envs(self.stock_environment.iter().cloned())
            .envs(invocation.environment.iter().cloned())
            .stdin(Stdio::null());
        if let Some(directory) = &invocation.working_directory {
            command.current_dir(directory);
        }
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let output = command_start::spawn(&mut command)?.wait_with_output()?;
        Ok(CommandOutput {
            exit_code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn run_bounded(&self, invocation: &Invocation, timeout: Duration) -> io::Result<CommandOutput> {
        run_attachment_bounded(self.spawn_attached(invocation)?, timeout)
    }

    fn spawn_attached(&self, invocation: &Invocation) -> io::Result<Attachment> {
        process_owner::check_capacity()?;
        let mut command = Command::new(&invocation.program);
        command
            .args(&invocation.arguments)
            .env_clear()
            .env("PATH", "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin")
            .env("HOME", &self.home)
            .envs(self.stock_environment.iter().cloned())
            .envs(invocation.environment.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if let Some(directory) = &invocation.working_directory {
            command.current_dir(directory);
        }
        let mut child = command_start::spawn(&mut command)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("attached runtime stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("attached runtime stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("attached runtime stderr unavailable"))?;
        let identity = Arc::new(Mutex::new(LocalProcessIdentity {
            pid: child.id(),
            reaped: false,
        }));
        let control: Arc<dyn AttachmentControl> = Arc::new(PipeControl {
            identity: Arc::clone(&identity),
        });
        let cancel_io = Arc::new(AtomicBool::new(false));
        let streams = (|| -> io::Result<_> {
            Ok((
                CancellableFile::new(File::from(OwnedFd::from(stdin)), &cancel_io, false)?,
                CancellableFile::new(File::from(OwnedFd::from(stdout)), &cancel_io, false)?,
                CancellableFile::new(File::from(OwnedFd::from(stderr)), &cancel_io, false)?,
            ))
        })();
        let (stdin, stdout, stderr) = match streams {
            Ok(streams) => streams,
            Err(error) => {
                let _ = child.kill();
                retain_for_reaping(Box::new(SystemAttachedProcess {
                    child,
                    process_group: true,
                    cancel_io,
                    identity,
                }));
                return Err(error);
            }
        };
        Ok(Attachment {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            stderr: Box::new(stderr),
            process: Box::new(SystemAttachedProcess {
                child,
                process_group: true,
                cancel_io,
                identity,
            }),
            control,
        })
    }

    fn spawn_pty(&self, invocation: &Invocation) -> io::Result<Attachment> {
        self.spawn_pty_sized(
            invocation,
            TerminalSize {
                rows: 24,
                columns: 80,
            },
        )
    }

    fn spawn_pty_sized(
        &self,
        invocation: &Invocation,
        size: TerminalSize,
    ) -> io::Result<Attachment> {
        let opened = nix::pty::openpty(
            Some(&nix::pty::Winsize {
                ws_row: size.rows,
                ws_col: size.columns,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            None,
        )
        .map_err(io::Error::other)?;
        let master = Arc::new(File::from(opened.master));
        let slave = File::from(opened.slave);
        let mut attributes = nix::sys::termios::tcgetattr(&slave).map_err(io::Error::other)?;
        nix::sys::termios::cfmakeraw(&mut attributes);
        nix::sys::termios::tcsetattr(&slave, nix::sys::termios::SetArg::TCSANOW, &attributes)
            .map_err(io::Error::other)?;
        let mut command = Command::new(&invocation.program);
        command
            .args(&invocation.arguments)
            .env_clear()
            .env("PATH", "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin")
            .env("HOME", &self.home)
            .envs(self.stock_environment.iter().cloned())
            .envs(invocation.environment.iter().cloned())
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave))
            .process_group(0);
        if let Some(directory) = &invocation.working_directory {
            command.current_dir(directory);
        }
        let mut child = command_start::spawn(&mut command)?;
        let identity = Arc::new(Mutex::new(LocalProcessIdentity {
            pid: child.id(),
            reaped: false,
        }));
        let control: Arc<dyn AttachmentControl> = Arc::new(PtyControl {
            identity: Arc::clone(&identity),
            master: Arc::clone(&master),
        });
        let cancel_io = Arc::new(AtomicBool::new(false));
        let streams = (|| -> io::Result<_> {
            Ok((
                CancellableFile::new(master.try_clone()?, &cancel_io, true)?,
                CancellableFile::new(master.try_clone()?, &cancel_io, true)?,
            ))
        })();
        let (stdin, stdout) = match streams {
            Ok(streams) => streams,
            Err(error) => {
                let _ = child.kill();
                retain_for_reaping(Box::new(SystemAttachedProcess {
                    child,
                    process_group: true,
                    cancel_io,
                    identity,
                }));
                return Err(error);
            }
        };
        Ok(Attachment {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            stderr: Box::new(io::empty()),
            process: Box::new(SystemAttachedProcess {
                child,
                process_group: true,
                cancel_io,
                identity,
            }),
            control,
        })
    }
}

/// Generic fresh-container lifecycle implemented by local and future remote runtimes.
#[allow(clippy::missing_errors_doc)] // RuntimeError documents the shared failure contract.
pub trait JobRuntime: Send + Sync {
    fn create(&self, spec: &JobSpec) -> Result<ContainerId, RuntimeError>;
    fn create_cancellable(
        &self,
        _spec: &JobSpec,
        _cancel: &Cancellation,
    ) -> Result<ContainerId, RuntimeError> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "runtime create is not cancellable",
        )
        .into())
    }
    fn attach(
        &self,
        container: &ContainerId,
        terminal_size: Option<TerminalSize>,
    ) -> Result<Attachment, RuntimeError>;
    fn start(&self, container: &ContainerId) -> Result<(), RuntimeError>;
    fn wait(&self, container: &ContainerId) -> Result<RuntimeExit, RuntimeError>;
    /// Cancellable wait used by supervision. Implementations must return after
    /// cancellation and join their own IO; opaque waits are rejected, not detached.
    fn wait_cancellable(
        &self,
        _container: &ContainerId,
        _cancel: &Cancellation,
    ) -> Result<RuntimeExit, RuntimeError> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "runtime wait is not cancellable",
        )
        .into())
    }
    fn signal(&self, container: &ContainerId, signal: JobSignal) -> Result<(), RuntimeError>;
    fn signal_cancellable(
        &self,
        _container: &ContainerId,
        _signal: JobSignal,
        _cancel: &Cancellation,
    ) -> Result<(), RuntimeError> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "runtime signal is not cancellable",
        )
        .into())
    }
    fn resize(&self, container: &ContainerId, size: TerminalSize) -> Result<(), RuntimeError>;
    fn resize_cancellable(
        &self,
        _container: &ContainerId,
        _size: TerminalSize,
        _cancel: &Cancellation,
    ) -> Result<(), RuntimeError> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "runtime resize is not cancellable",
        )
        .into())
    }
    fn delete(&self, container: &ContainerId) -> Result<(), RuntimeError>;
    fn delete_cancellable(
        &self,
        _container: &ContainerId,
        _cancel: &Cancellation,
    ) -> Result<(), RuntimeError> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "runtime delete is not cancellable",
        )
        .into())
    }
}

/// Docker CLI backend. The CLI and daemon endpoint stay in the trusted worker;
/// neither is included in the job specification or container mounts.
pub struct DockerCliRuntime<R> {
    runner: R,
    executable: PathBuf,
    engine_socket: PathBuf,
    sbx_environment: Vec<(String, OsString)>,
    trusted_ca_bundle: PathBuf,
    trusted_ca_owner: u32,
    byte_carriers: byte_carrier::CarrierStore,
    /// Writable-layer limit of each container this runtime created, until
    /// its deletion.
    writable_limits: Mutex<BTreeMap<String, u64>>,
}

impl<R> DockerCliRuntime<R> {
    #[must_use]
    pub fn new(runner: R, executable: impl Into<PathBuf>) -> Self {
        Self {
            runner,
            executable: executable.into(),
            engine_socket: "/var/run/docker.sock".into(),
            sbx_environment: Vec::new(),
            trusted_ca_bundle: TRUSTED_CA_BUNDLE.into(),
            trusted_ca_owner: 0,
            byte_carriers: byte_carrier::CarrierStore::system(),
            writable_limits: Mutex::new(BTreeMap::new()),
        }
    }

    #[must_use]
    pub fn with_engine_socket(mut self, socket: impl Into<PathBuf>) -> Self {
        self.engine_socket = socket.into();
        self
    }

    fn with_sbx_environment(
        mut self,
        environment: Vec<(String, OsString)>,
        ca_bundle: impl Into<PathBuf>,
        ca_owner: u32,
    ) -> Self {
        self.sbx_environment = environment;
        self.trusted_ca_bundle = ca_bundle.into();
        self.trusted_ca_owner = ca_owner;
        self
    }

    fn invocation<I, S>(&self, arguments: I) -> Invocation
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Invocation {
            program: self.executable.clone(),
            arguments: arguments.into_iter().map(Into::into).collect(),
            working_directory: None,
            environment: Vec::new(),
        }
    }
}

impl DockerCliRuntime<SystemCommandRunner> {
    #[must_use]
    pub fn system() -> Self {
        let environment = PROXY_VARIABLES
            .iter()
            .filter_map(|name| std::env::var_os(name).map(|value| ((*name).to_owned(), value)))
            .collect();
        // Local Kit workers run as root. Keep Docker CLI state under that
        // worker's own home.
        let home = std::env::var_os("HOME").unwrap_or_else(|| "/root".into());
        Self::new(SystemCommandRunner::new(home), "/usr/bin/docker").with_sbx_environment(
            environment,
            TRUSTED_CA_BUNDLE,
            0,
        )
    }
}

impl<R: CommandRunner> DockerCliRuntime<R> {
    fn create_from_arguments(
        &self,
        spec: &JobSpec,
        mut arguments: Vec<OsString>,
        byte_bridge: bool,
        cancel: &Cancellation,
    ) -> Result<ContainerId, RuntimeError> {
        // BOTH encodings bind native platform and immutable config identity
        // before any create. UTF8 keeps the image's native command unchanged.
        let inspection = self
            .checked_cancellable(
                "inspect immutable image",
                &self.invocation([
                    "image",
                    "inspect",
                    "--format",
                    "{{json .}}",
                    "--",
                    spec.image.as_str(),
                ]),
                cancel,
                Some(Duration::from_secs(15)),
            )
            .map_err(pre_create_error)?;
        let image = byte_carrier::ImageCommand::inspect(&inspection, spec)?;
        let carrier = if byte_bridge {
            self.byte_carriers
                .reclaim(|candidates| self.unreferenced_carriers_with_cancel(candidates, cancel))
                .map_err(pre_create_error)?;
            let (attempt, carrier_arguments, payload_target) = self
                .byte_carriers
                .prepare(spec, &image)
                .map_err(pre_create_error)?;
            arguments.extend(carrier_arguments);
            arguments.extend([
                "--".into(),
                image.id.into(),
                "--payload".into(),
                payload_target.into(),
                "--owner".into(),
                rustix::process::geteuid().as_raw().to_string().into(),
            ]);
            Some(attempt)
        } else {
            arguments.extend([
                "--platform".into(),
                image.platform.into(),
                "--".into(),
                image.id.into(),
            ]);
            arguments.extend(spec.argv.iter().cloned().map(OsString::from_vec));
            None
        };
        let result = self
            .checked_cancellable(
                "create",
                &self.invocation(arguments),
                cancel,
                Some(Duration::from_secs(10)),
            )
            .and_then(|stdout| {
                let identity = std::str::from_utf8(&stdout)
                    .map_err(|_| RuntimeError::InvalidContainerIdentity)?
                    .trim();
                ContainerId::parse(identity.to_owned())
                    .map_err(|_| RuntimeError::InvalidContainerIdentity)
            });
        if let Some(attempt) = carrier {
            // Even a failed create may have installed the bind. Retain the
            // sources/name on every ambiguous outcome; worker quarantines.
            let container = result.map_err(|error| RuntimeError::ByteCreateUncertain {
                attempt: attempt.clone(),
                stderr: match error {
                    RuntimeError::CommandFailed { stderr, .. } => stderr,
                    _ => "create reply unavailable or invalid".to_owned(),
                },
            })?;
            self.byte_carriers.bind(&attempt, &container).map_err(|_| {
                RuntimeError::ByteCreateUncertain {
                    attempt,
                    stderr: "container created but carrier identity binding failed".to_owned(),
                }
            })?;
            Ok(container)
        } else {
            result
        }
    }

    /// Reclaim only exact unlocked worker-owned attempts, after a fresh complete
    /// Docker inventory and mount-source observation. Never deletes containers.
    /// An active/unknown reference remains retained; runtime restart is not
    /// absence evidence. Callers must not replay the uncertain job.
    ///
    /// # Errors
    /// Unsafe state or incomplete/changing Docker observations retain sources.
    pub fn reclaim_byte_carriers(&self) -> Result<usize, RuntimeError> {
        self.byte_carriers
            .reclaim(|candidates| self.unreferenced_carriers(candidates))
    }

    fn unreferenced_carriers(
        &self,
        candidates: &[byte_carrier::Candidate],
    ) -> Result<byte_carrier::Observation, RuntimeError> {
        self.unreferenced_carriers_with_cancel(candidates, &Cancellation::default())
    }

    fn unreferenced_carriers_with_cancel(
        &self,
        candidates: &[byte_carrier::Candidate],
        cancel: &Cancellation,
    ) -> Result<byte_carrier::Observation, RuntimeError> {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let observe = |arguments: Vec<OsString>| -> Result<Vec<u8>, RuntimeError> {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .ok_or(RuntimeError::DeletionUncertain)?;
            let output = self.checked_cancellable(
                "observe carrier inventory",
                &self.invocation(arguments),
                cancel,
                Some(remaining),
            )?;
            if output.len() > 1024 * 1024 {
                return Err(RuntimeError::DeletionUncertain);
            }
            Ok(output)
        };
        let inventory = || -> Result<Vec<String>, RuntimeError> {
            let bytes = observe(
                [
                    "container",
                    "ls",
                    "--all",
                    "--no-trunc",
                    "--format",
                    "{{.ID}}",
                ]
                .map(OsString::from)
                .to_vec(),
            )?;
            let text = std::str::from_utf8(&bytes).map_err(|_| RuntimeError::DeletionUncertain)?;
            let mut ids = Vec::new();
            for line in text.lines() {
                let id = ContainerId::parse(line.to_owned())
                    .map_err(|_| RuntimeError::DeletionUncertain)?;
                if ids.len() == 256 {
                    return Err(RuntimeError::DeletionUncertain);
                }
                ids.push(id.as_str().to_owned());
            }
            ids.sort();
            if ids.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(RuntimeError::DeletionUncertain);
            }
            Ok(ids)
        };
        let before = inventory()?;
        let mut inspection = Vec::new();
        if !before.is_empty() {
            // Deliberately exclude Config/Env and user argv from inspection.
            let mut arguments: Vec<OsString> = [
                "container",
                "inspect",
                "--format",
                "{\"Id\":{{json .Id}},\"Name\":{{json .Name}},\"Mounts\":{{json .Mounts}}}",
                "--",
            ]
            .map(OsString::from)
            .to_vec();
            arguments.extend(before.iter().map(OsString::from));
            inspection = observe(arguments)?;
        }
        // A disappearing container during inspect, or a changing full list,
        // is incomplete evidence, never permission to infer name absence.
        if inventory()? != before {
            return Err(RuntimeError::DeletionUncertain);
        }
        byte_carrier::Observation::from_inspection(candidates, &before, &inspection)
    }

    fn checked_cancellable(
        &self,
        operation: &'static str,
        invocation: &Invocation,
        cancel: &Cancellation,
        timeout: Option<Duration>,
    ) -> Result<Vec<u8>, RuntimeError> {
        cancel.check()?;
        process_owner::check_capacity()?;
        let output = run_attachment_interruptible(
            self.runner.spawn_attached(invocation)?,
            timeout,
            cancel,
            true,
        )?;
        if !output.succeeded() {
            return Err(RuntimeError::CommandFailed {
                operation,
                exit_code: output.exit_code,
                stderr: bounded_command_stderr(&output.stderr),
            });
        }
        Ok(output.stdout)
    }

    /// The running container's init PID, unified cgroup, and `pids.events`.
    fn capture_running(
        &self,
        container: &ContainerId,
        cancel: &Cancellation,
    ) -> Result<Option<RunningContainer>, RuntimeError> {
        for attempt in 0..RESOURCE_PID_ATTEMPTS {
            let output = self.checked_cancellable(
                "inspect running container PID",
                &self.invocation([
                    "container",
                    "inspect",
                    "--format",
                    "{{.State.Pid}}\t{{.State.Status}}",
                    container.as_str(),
                ]),
                cancel,
                Some(Duration::from_secs(10)),
            )?;
            let (pid, starting) = parse_running_pid(&output)?;
            if pid == 0 && !starting {
                return Ok(None);
            }
            if pid != 0 {
                return match open_pids_events(pid, container) {
                    Ok((cgroup, pids_events)) => Ok(Some(RunningContainer {
                        pid,
                        cgroup,
                        pids_events,
                    })),
                    Err(RuntimeError::Io(error)) if cgroup_evidence_unavailable(&error) => Ok(None),
                    Err(error) => Err(error),
                };
            }
            if attempt + 1 < RESOURCE_PID_ATTEMPTS {
                thread::sleep(RESOURCE_PID_DELAY);
            }
        }
        Ok(None)
    }

    /// Enforcement inputs for a container this runtime created with a
    /// writable limit. An unresolvable writable layer (non-overlay storage,
    /// or the container already gone) leaves only the terminal
    /// `SizeRw` classification.
    fn writable_target(
        &self,
        container: &ContainerId,
        running: &RunningContainer,
    ) -> Option<writable_monitor::WritableTarget> {
        let limit = *self
            .writable_limits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(container.as_str())?;
        let upper = writable_monitor::container_upperdir(running.pid).ok()??;
        Some(writable_monitor::WritableTarget {
            pid: running.pid,
            container: container.clone(),
            cgroup: running.cgroup.clone(),
            upper,
            limit,
        })
    }
}

struct RunningContainer {
    pid: u32,
    cgroup: PathBuf,
    pids_events: File,
}

/// `{{.State.Pid}}\t{{.State.Status}}` as (PID, still being started).
fn parse_running_pid(bytes: &[u8]) -> Result<(u32, bool), RuntimeError> {
    let value = std::str::from_utf8(bytes).map_err(|_| RuntimeError::InvalidResourceEvidence)?;
    let (pid, status) = value
        .trim()
        .split_once('\t')
        .ok_or(RuntimeError::InvalidResourceEvidence)?;
    let pid = pid
        .parse::<u32>()
        .map_err(|_| RuntimeError::InvalidResourceEvidence)?;
    Ok((pid, status == "created"))
}

fn open_pids_events(pid: u32, container: &ContainerId) -> Result<(PathBuf, File), RuntimeError> {
    let cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    if cgroup.len() > 4096 {
        return Err(RuntimeError::InvalidResourceEvidence);
    }
    let mut unified = cgroup.lines().filter_map(|line| line.strip_prefix("0::"));
    let relative = unified
        .next()
        .ok_or(RuntimeError::InvalidResourceEvidence)?;
    if unified.next().is_some() {
        return Err(RuntimeError::InvalidResourceEvidence);
    }
    if !relative.contains(container.as_str()) {
        return Err(RuntimeError::InvalidResourceEvidence);
    }
    let relative = Path::new(relative.trim_start_matches('/'));
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(RuntimeError::InvalidResourceEvidence);
    }
    let root = Path::new("/sys/fs/cgroup");
    let directory = fs::canonicalize(root.join(relative))?;
    if !directory.starts_with(root) {
        return Err(RuntimeError::InvalidResourceEvidence);
    }
    let events = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(directory.join("pids.events"))?;
    Ok((directory, events))
}

fn read_pids_max_events(file: &mut (impl Read + Seek)) -> Result<u64, RuntimeError> {
    file.rewind()?;
    let mut text = String::new();
    file.take(4097).read_to_string(&mut text)?;
    if text.len() > 4096 {
        return Err(RuntimeError::InvalidResourceEvidence);
    }
    parse_pids_max_events(&text)
}

fn terminal_pids_evidence(
    exit_code: i32,
    evidence: Result<Option<u64>, RuntimeError>,
) -> Result<Option<u64>, RuntimeError> {
    if exit_code == 0 {
        return Ok(None);
    }
    evidence
}

struct PidsEventsMonitor {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<Result<Option<u64>, RuntimeError>>>,
}

impl PidsEventsMonitor {
    fn start(file: Option<File>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = file.map(|file| {
            let thread_stop = Arc::clone(&stop);
            thread::spawn(move || monitor_pids_events(file, &thread_stop))
        });
        Self { stop, handle }
    }

    fn finish(mut self) -> Result<Option<u64>, RuntimeError> {
        self.stop.store(true, Ordering::Release);
        self.handle.take().map_or(Ok(None), |handle| {
            handle.thread().unpark();
            handle
                .join()
                .map_err(|_| RuntimeError::InvalidResourceEvidence)?
        })
    }
}

impl Drop for PidsEventsMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

fn monitor_pids_events(file: File, stop: &AtomicBool) -> Result<Option<u64>, RuntimeError> {
    let event_source = file.try_clone()?;
    monitor_pids_events_with_wait(file, stop, || {
        let mut events = [nix::poll::PollFd::new(
            event_source.as_fd(),
            nix::poll::PollFlags::POLLPRI | nix::poll::PollFlags::POLLERR,
        )];
        let timeout = nix::poll::PollTimeout::try_from(RESOURCE_EVENT_POLL_DELAY)
            .map_err(io::Error::other)?;
        nix::poll::poll(&mut events, timeout).map_err(io::Error::other)?;
        Ok(())
    })
}

#[cfg(test)]
fn monitor_pids_events_with_delay(
    file: impl Read + Seek,
    stop: &AtomicBool,
    poll_delay: Duration,
) -> Result<Option<u64>, RuntimeError> {
    monitor_pids_events_with_wait(file, stop, || {
        thread::park_timeout(poll_delay);
        Ok(())
    })
}

fn monitor_pids_events_with_wait(
    mut file: impl Read + Seek,
    stop: &AtomicBool,
    mut wait: impl FnMut() -> io::Result<()>,
) -> Result<Option<u64>, RuntimeError> {
    let mut observed = None;
    loop {
        match read_pids_max_events(&mut file) {
            Ok(events) => observed = Some(events),
            Err(RuntimeError::Io(error)) if cgroup_evidence_unavailable(&error) => {
                return Ok(observed);
            }
            Err(error) => return Err(error),
        }
        if stop.load(Ordering::Acquire) {
            return Ok(observed);
        }
        wait()?;
    }
}

fn cgroup_evidence_unavailable(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
        || matches!(
            error.raw_os_error(),
            Some(nix::libc::ENODEV | nix::libc::ESRCH)
        )
}

fn parse_pids_max_events(text: &str) -> Result<u64, RuntimeError> {
    let mut maximum = None;
    for line in text.lines() {
        let Some((name, value)) = line.split_once(' ') else {
            return Err(RuntimeError::InvalidResourceEvidence);
        };
        if name == "max" {
            if maximum.is_some() {
                return Err(RuntimeError::InvalidResourceEvidence);
            }
            maximum = Some(
                value
                    .parse()
                    .map_err(|_| RuntimeError::InvalidResourceEvidence)?,
            );
        }
    }
    maximum.ok_or(RuntimeError::InvalidResourceEvidence)
}

fn parse_resource_state(bytes: &[u8]) -> Result<(bool, i32, u64), RuntimeError> {
    let text = std::str::from_utf8(bytes).map_err(|_| RuntimeError::InvalidResourceEvidence)?;
    let mut fields = text.trim().split('\t');
    let oom_killed = fields
        .next()
        .ok_or(RuntimeError::InvalidResourceEvidence)?
        .parse()
        .map_err(|_| RuntimeError::InvalidResourceEvidence)?;
    let code = fields
        .next()
        .ok_or(RuntimeError::InvalidResourceEvidence)?
        .parse()
        .map_err(|_| RuntimeError::InvalidResourceEvidence)?;
    let writable_bytes = fields
        .next()
        .ok_or(RuntimeError::InvalidResourceEvidence)?
        .parse()
        .map_err(|_| RuntimeError::InvalidResourceEvidence)?;
    if fields.next().is_some() {
        return Err(RuntimeError::InvalidResourceEvidence);
    }
    Ok((oom_killed, code, writable_bytes))
}

fn validate_sbx_environment(
    environment: &[(String, OsString)],
) -> Result<Vec<(String, OsString)>, RuntimeError> {
    if environment.is_empty() {
        return Ok(Vec::new());
    }
    let mut validated = Vec::with_capacity(environment.len());
    for (name, value) in environment {
        if !PROXY_VARIABLES.contains(&name.as_str()) {
            return Err(invalid_sbx_environment(name));
        }
        if validated.iter().any(|(existing, _)| existing == name) {
            return Err(invalid_sbx_environment(name));
        }
        let bytes = value.as_os_str().as_bytes();
        if bytes.is_empty()
            || bytes.len() > 8 * 1024
            || bytes
                .iter()
                .any(|byte| *byte == 0 || byte.is_ascii_control())
        {
            return Err(invalid_sbx_environment(name));
        }
        match name.as_str() {
            "HTTP_PROXY" | "HTTPS_PROXY" if bytes != STOCK_SBX_PROXY_ENDPOINT => {
                return Err(invalid_sbx_environment(name));
            }
            "NODE_USE_ENV_PROXY" if bytes != b"1" => {
                return Err(invalid_sbx_environment(name));
            }
            "SBX_CRED_ANTHROPIC_MODE"
            | "SBX_CRED_OPENAI_MODE"
            | "SBX_CRED_GOOGLE_MODE"
            | "SBX_CRED_PARALLEL_MODE"
            | "SBX_CRED_GITHUB_MODE"
            | "SBX_CRED_SBX-LOGIN_MODE"
                if !matches!(bytes, b"none" | b"apikey" | b"oauth") =>
            {
                return Err(invalid_sbx_environment(name));
            }
            "MCP_GATEWAY_URL" if !valid_stock_sbx_mcp_url(bytes) => {
                return Err(invalid_sbx_environment(name));
            }
            "MCP_SENTINEL_TOKEN_NAME"
                if bytes.is_empty()
                    || bytes.len() > 128
                    || !bytes.iter().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    }) =>
            {
                return Err(invalid_sbx_environment(name));
            }
            _ => {}
        }
        validated.push((name.clone(), value.clone()));
    }
    let has_proxy = validated.iter().any(|(name, _)| name == "HTTP_PROXY")
        || validated.iter().any(|(name, _)| name == "HTTPS_PROXY");
    if has_proxy
        && let Some(required) = ["HTTP_PROXY", "HTTPS_PROXY"]
            .into_iter()
            .find(|required| !validated.iter().any(|(name, _)| name == required))
    {
        return Err(invalid_sbx_environment(required));
    }
    let has_mcp_url = validated.iter().any(|(name, _)| name == "MCP_GATEWAY_URL");
    let has_mcp_sentinel = validated
        .iter()
        .any(|(name, _)| name == "MCP_SENTINEL_TOKEN_NAME");
    if has_mcp_url != has_mcp_sentinel {
        return Err(invalid_sbx_environment(if has_mcp_url {
            "MCP_SENTINEL_TOKEN_NAME"
        } else {
            "MCP_GATEWAY_URL"
        }));
    }
    // The stock bearer sentinel is substituted by the SBX HTTP proxy; without
    // that route the literal sentinel would be sent to the gateway.
    if has_mcp_url && !has_proxy {
        return Err(invalid_sbx_environment("HTTP_PROXY"));
    }
    Ok(validated)
}

fn valid_stock_sbx_mcp_url(bytes: &[u8]) -> bool {
    // Only the sandbox-local stock gateway is allowed to receive the bearer
    // sentinel. A caller-selected URL would persist in the agent's home.
    bytes == STOCK_SBX_MCP_URL
}

fn invalid_sbx_environment(variable: &str) -> RuntimeError {
    // Environment names are authority-bearing input. Escape controls before
    // including the offending name in logs and bound the diagnostic itself.
    let variable = variable
        .chars()
        .flat_map(char::escape_default)
        .take(256)
        .collect();
    RuntimeError::InvalidSbxEnvironment { variable }
}

fn validate_ca_bundle(path: &Path, owner: u32) -> Result<(), RuntimeError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| RuntimeError::InvalidCaBundle)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & 0o022 != 0
    {
        return Err(RuntimeError::InvalidCaBundle);
    }
    Ok(())
}

fn connect_engine(path: &Path, cancel: &Cancellation) -> io::Result<CancellableFile> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketType};
    #[cfg(target_os = "linux")]
    let fd = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        rustix::net::SocketFlags::CLOEXEC | rustix::net::SocketFlags::NONBLOCK,
        None,
    )?;
    #[cfg(not(target_os = "linux"))]
    let fd = with_host_descriptor_creation_excluded(|| -> io::Result<OwnedFd> {
        let fd = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None)?;
        rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)?;
        rustix::fs::fcntl_setfl(&fd, rustix::fs::OFlags::NONBLOCK)?;
        Ok(fd)
    })?;
    let address = SocketAddrUnix::new(path)?;
    loop {
        cancel.check()?;
        match rustix::net::connect(&fd, &address) {
            Ok(()) | Err(rustix::io::Errno::ISCONN) => break,
            Err(rustix::io::Errno::INPROGRESS | rustix::io::Errno::ALREADY) => {
                let mut fds = [nix::poll::PollFd::new(
                    fd.as_fd(),
                    nix::poll::PollFlags::POLLOUT,
                )];
                let _ = nix::poll::poll(&mut fds, 20_u16);
                if fds[0]
                    .revents()
                    .is_some_and(|events| events.contains(nix::poll::PollFlags::POLLOUT))
                {
                    rustix::net::sockopt::socket_error(&fd)??;
                    break;
                }
            }
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error.into()),
        }
    }
    CancellableFile::from_file(File::from(fd), cancel)
}

fn resize_engine_container(
    socket: &Path,
    container: &ContainerId,
    size: TerminalSize,
    cancel: &Cancellation,
) -> Result<(), RuntimeError> {
    let cancel = cancel.with_timeout(ENGINE_IO_TIMEOUT);
    let mut stream = connect_engine(socket, &cancel)?;
    write!(
        stream,
        "POST /containers/{}/resize?h={}&w={} HTTP/1.1\r\nHost: docker\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        container.as_str(),
        size.rows,
        size.columns
    )?;
    stream.flush()?;
    let mut response = Vec::new();
    stream
        .take(MAX_ENGINE_RESPONSE + 1)
        .read_to_end(&mut response)?;
    if response.len() as u64 > MAX_ENGINE_RESPONSE {
        return Err(RuntimeError::InvalidEngineResponse);
    }
    let header_end = response
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .ok_or(RuntimeError::InvalidEngineResponse)?;
    let header = std::str::from_utf8(&response[..header_end])
        .map_err(|_| RuntimeError::InvalidEngineResponse)?;
    let mut status_line = header
        .lines()
        .next()
        .ok_or(RuntimeError::InvalidEngineResponse)?
        .split_whitespace();
    if !matches!(status_line.next(), Some("HTTP/1.0" | "HTTP/1.1")) {
        return Err(RuntimeError::InvalidEngineResponse);
    }
    let status_text = status_line
        .next()
        .ok_or(RuntimeError::InvalidEngineResponse)?;
    if status_text.len() != 3 || !status_text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(RuntimeError::InvalidEngineResponse);
    }
    let status = status_text
        .parse::<u16>()
        .map_err(|_| RuntimeError::InvalidEngineResponse)?;
    if status != 200 {
        return Err(RuntimeError::EngineResizeFailed {
            status,
            body: bounded_command_stderr(&response[header_end + 4..]),
        });
    }
    Ok(())
}

impl<R: CommandRunner> JobRuntime for DockerCliRuntime<R> {
    fn create(&self, spec: &JobSpec) -> Result<ContainerId, RuntimeError> {
        self.create_cancellable(spec, &Cancellation::default())
    }

    fn create_cancellable(
        &self,
        spec: &JobSpec,
        cancel: &Cancellation,
    ) -> Result<ContainerId, RuntimeError> {
        let cancel = cancel.with_timeout(Duration::from_secs(30));
        cancel.check()?;
        spec.validate()?;
        let cwd_bytes = spec.working_directory.as_os_str().as_bytes();
        if cwd_bytes.len() > 4096 || cwd_bytes.contains(&0) {
            return Err(RuntimeError::InvalidSpec(
                JobSpecError::InvalidWorkingDirectory,
            ));
        }
        let sbx_environment = validate_sbx_environment(&self.sbx_environment)?;
        let byte_bridge = byte_carrier::required(spec);
        let mut arguments = vec![
            OsString::from("create"),
            OsString::from("--interactive"),
            // Keep the workload out of the container PID 1 slot. Linux gives
            // PID 1 special default-signal semantics, which would otherwise
            // let an unhandled Ctrl-C be ignored instead of terminating the
            // attached shell process. Docker's init also reaps descendants.
            OsString::from("--init"),
            OsString::from("--workdir"),
            if spec.working_directory.to_str().is_none() {
                OsString::from("/")
            } else {
                spec.working_directory.as_os_str().to_owned()
            },
            OsString::from("--user"),
            OsString::from(format!("{}:{}", spec.identity.uid, spec.identity.gid)),
            OsString::from("--cap-drop"),
            OsString::from("ALL"),
            OsString::from("--security-opt"),
            OsString::from("no-new-privileges=true"),
            OsString::from("--pids-limit"),
            spec.resources.pids.to_string().into(),
            OsString::from("--memory"),
            spec.resources.memory_bytes.to_string().into(),
            OsString::from("--cpus"),
            format!("{:.3}", f64::from(spec.resources.cpu_millis) / 1000.0).into(),
            OsString::from("--storage-opt"),
            format!("size={}", spec.resources.writable_bytes).into(),
        ];
        if spec.terminal {
            arguments.push("--tty".into());
        }
        // The container's starting environment beside the image's `Env`,
        // recorded for `job.json` as it is passed.
        let mut started = BTreeMap::<String, Vec<u8>>::new();
        if !sbx_environment.is_empty() {
            let has_proxy = sbx_environment.iter().any(|(name, _)| name == "HTTP_PROXY");
            if has_proxy {
                validate_ca_bundle(&self.trusted_ca_bundle, self.trusted_ca_owner)?;
            }
            for (name, value) in sbx_environment {
                started.insert(name.clone(), value.as_bytes().to_vec());
                let mut assignment = OsString::from(name);
                assignment.push("=");
                assignment.push(value);
                arguments.extend(["--env".into(), assignment]);
            }
            if has_proxy {
                let trusted_ca_bundle = self
                    .trusted_ca_bundle
                    .to_str()
                    .ok_or(RuntimeError::InvalidCaBundle)?;
                for name in ["NODE_EXTRA_CA_CERTS", "SSL_CERT_FILE", "REQUESTS_CA_BUNDLE"] {
                    started.insert(name.to_owned(), trusted_ca_bundle.as_bytes().to_vec());
                    arguments
                        .extend(["--env".into(), format!("{name}={trusted_ca_bundle}").into()]);
                }
                arguments.extend([
                    "--mount".into(),
                    format!(
                        "type=bind,source={trusted_ca_bundle},target={trusted_ca_bundle},readonly"
                    )
                    .into(),
                ]);
            }
        }
        if !byte_bridge {
            for (name, value) in &spec.exported_environment {
                started.insert(name.clone(), value.clone());
                let mut assignment = OsString::from(name);
                assignment.push("=");
                assignment.push(std::ffi::OsStr::from_bytes(value));
                arguments.extend(["--env".into(), assignment]);
            }
        }
        for (name, value) in &spec.session_environment {
            started.insert(name.clone(), value.as_bytes().to_vec());
            arguments.extend(["--env".into(), format!("{name}={value}").into()]);
        }
        push_docker_mounts(&mut arguments, spec)?;
        self.capability_arguments(&mut arguments, spec, started, &cancel)?;
        let container = self.create_from_arguments(spec, arguments, byte_bridge, &cancel)?;
        self.writable_limits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(container.as_str().to_owned(), spec.resources.writable_bytes);
        Ok(container)
    }

    fn attach(
        &self,
        container: &ContainerId,
        terminal_size: Option<TerminalSize>,
    ) -> Result<Attachment, RuntimeError> {
        let invocation =
            self.invocation(["start", "--attach", "--interactive", container.as_str()]);
        terminal_size
            .map_or_else(
                || self.runner.spawn_attached(&invocation),
                |size| self.runner.spawn_pty_sized(&invocation, size),
            )
            .map_err(RuntimeError::Io)
    }

    fn start(&self, _container: &ContainerId) -> Result<(), RuntimeError> {
        // `attach` atomically starts and attaches through one Docker CLI process,
        // so there must not be a second start command or a stopped-container gap.
        Ok(())
    }

    fn wait(&self, container: &ContainerId) -> Result<RuntimeExit, RuntimeError> {
        self.wait_cancellable(container, &Cancellation::default())
    }

    fn wait_cancellable(
        &self,
        container: &ContainerId,
        cancel: &Cancellation,
    ) -> Result<RuntimeExit, RuntimeError> {
        let running = self.capture_running(container, cancel)?;
        let writable = writable_monitor::WritableMonitor::start(
            running
                .as_ref()
                .and_then(|running| self.writable_target(container, running)),
        );
        let pids_events = PidsEventsMonitor::start(running.map(|running| running.pids_events));
        let wait = self.checked_cancellable(
            "wait",
            &self.invocation(["wait", container.as_str()]),
            cancel,
            None,
        );
        // Stop and join before inspecting terminal state so neither cgroup
        // reader can outlive this exact container attempt. If Docker has
        // already removed the cgroup, the monitor returns its last valid counter.
        let writable_exceeded = writable.finish();
        let pids_max_events = pids_events.finish();
        let stdout = wait?;
        let code = std::str::from_utf8(&stdout)
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .ok_or(RuntimeError::InvalidExitStatus)?;
        let state = self.checked_cancellable(
            "inspect terminal resource state",
            &self.invocation([
                "container",
                "inspect",
                "--size",
                "--format",
                "{{.State.OOMKilled}}\t{{.State.ExitCode}}\t{{.SizeRw}}",
                container.as_str(),
            ]),
            cancel,
            Some(Duration::from_secs(10)),
        )?;
        let (oom_killed, inspected_code, writable_bytes) = parse_resource_state(&state)?;
        if inspected_code != code {
            return Err(RuntimeError::InvalidResourceEvidence);
        }
        // A pids-limit cause can only explain a nonzero exit. Cgroup pseudo-files
        // may become unreadable as Docker tears down a short-lived successful
        // container, so consulting them for exit 0 creates a false supervision
        // failure without adding any classifiable evidence.
        let pids_max_events = terminal_pids_evidence(code, pids_max_events)?;
        Ok(RuntimeExit {
            code,
            oom_killed,
            pids_max_events,
            writable_bytes,
            writable_exceeded,
        })
    }

    fn signal(&self, container: &ContainerId, signal: JobSignal) -> Result<(), RuntimeError> {
        self.signal_cancellable(container, signal, &Cancellation::default())
    }

    fn signal_cancellable(
        &self,
        container: &ContainerId,
        signal: JobSignal,
        cancel: &Cancellation,
    ) -> Result<(), RuntimeError> {
        let signal = match signal {
            JobSignal::Interrupt => "INT",
            JobSignal::Terminate => "TERM",
            JobSignal::Kill => "KILL",
            JobSignal::Hangup => "HUP",
        };
        self.checked_cancellable(
            "signal",
            &self.invocation(["kill", "--signal", signal, container.as_str()]),
            cancel,
            Some(Duration::from_secs(2)),
        )?;
        Ok(())
    }

    fn resize(&self, container: &ContainerId, size: TerminalSize) -> Result<(), RuntimeError> {
        self.resize_cancellable(container, size, &Cancellation::default())
    }

    fn resize_cancellable(
        &self,
        container: &ContainerId,
        size: TerminalSize,
        cancel: &Cancellation,
    ) -> Result<(), RuntimeError> {
        if size.rows == 0 || size.columns == 0 {
            return Err(RuntimeError::InvalidTerminalSize);
        }
        resize_engine_container(&self.engine_socket, container, size, cancel)
    }

    fn delete(&self, container: &ContainerId) -> Result<(), RuntimeError> {
        self.delete_cancellable(container, &Cancellation::default())
    }

    fn delete_cancellable(
        &self,
        container: &ContainerId,
        cancel: &Cancellation,
    ) -> Result<(), RuntimeError> {
        let cancel = cancel.with_timeout(Duration::from_secs(10));
        self.writable_limits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(container.as_str());
        self.checked_cancellable(
            "delete",
            &self.invocation(["rm", "--force", container.as_str()]),
            &cancel,
            Some(Duration::from_secs(10)),
        )?;
        let filter = format!("id={}", container.as_str());
        for attempt in 0..DELETION_VERIFY_ATTEMPTS {
            let remaining = self.checked_cancellable(
                "verify deletion",
                &self.invocation([
                    "container",
                    "ls",
                    "--all",
                    "--no-trunc",
                    "--filter",
                    &filter,
                    "--format",
                    "{{.ID}}",
                ]),
                &cancel,
                Some(Duration::from_secs(10)),
            )?;
            if remaining.iter().all(u8::is_ascii_whitespace) {
                self.byte_carriers
                    .release_verified(container, |candidates| {
                        self.unreferenced_carriers_with_cancel(candidates, &cancel)
                    })?;
                return Ok(());
            }
            if attempt + 1 < DELETION_VERIFY_ATTEMPTS {
                thread::sleep(DELETION_VERIFY_DELAY);
            }
        }
        Err(RuntimeError::DeletionUncertain)
    }
}

fn push_docker_mounts(arguments: &mut Vec<OsString>, spec: &JobSpec) -> Result<(), RuntimeError> {
    // OCI mounts are order-sensitive when the natural home contains the
    // project. Install parents first so a later child grant cannot be
    // hidden by its parent, regardless of manifest declaration order.
    let mut mounts: Vec<_> = spec.mounts.iter().collect();
    mounts.sort_by(|left, right| {
        left.target
            .components()
            .count()
            .cmp(&right.target.components().count())
            .then_with(|| left.target.cmp(&right.target))
    });
    for mount in mounts {
        let readonly = match mount.access {
            MountAccess::ReadOnly => ",readonly",
            MountAccess::ReadWrite => "",
        };
        let resolved;
        let source = match &mount.subpath {
            None => &mount.source,
            Some(subpath) => {
                resolved = resolve_grant_subpath(&mount.source, subpath)?;
                &resolved
            }
        };
        let source = docker_mount_field("source", source)?;
        let target = docker_mount_field("target", &mount.target)?;
        arguments.extend([
            "--mount".into(),
            format!("type=bind,{source},{target}{readonly}").into(),
        ]);
    }
    Ok(())
}

/// Resolves a split branch's subdirectory of one prepared grant. Every
/// component must be a real directory, never a symlink, so a workspace path
/// cannot redirect the bind outside the grant. (Writers that share the grant
/// could still swap a component before Docker binds it; see
/// `docs/design/workspaces.md`.)
/// The static job artifact (`marsh-local`), installed with the worker.
pub const SPLIT_SHIM: &str = "/usr/local/libexec/marsh-local";

/// Image config `Env` per immutable image, read once per worker.
static IMAGE_ENV: std::sync::OnceLock<Mutex<BTreeMap<String, Vec<String>>>> =
    std::sync::OnceLock::new();

impl<R: CommandRunner> DockerCliRuntime<R> {
    /// The worker's per-attempt job capability, read-only, with the static
    /// artifact bound over its mountpoint (`docs/design/processes.md` s4); nothing else
    /// reaches the daemon and nothing image-owned is shadowed.
    fn capability_arguments(
        &self,
        arguments: &mut Vec<OsString>,
        spec: &JobSpec,
        started: BTreeMap<String, Vec<u8>>,
        cancel: &Cancellation,
    ) -> Result<(), RuntimeError> {
        let Some(directory) = &spec.split_capability else {
            return Ok(());
        };
        if !directory.starts_with("/run/marsh-cap/") {
            return Err(RuntimeError::InvalidSpec(JobSpecError::InvalidMount));
        }
        let source = docker_mount_field("source", directory)?;
        arguments.extend([
            "--mount".into(),
            format!("type=bind,{source},target=/run/marsh,readonly").into(),
        ]);
        // The static artifact is mandatory: without it every link and the
        // in-job `marsh` would be dead (`docs/design/processes.md` s4).
        if !Path::new(SPLIT_SHIM).is_file() {
            return Err(RuntimeError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("static job artifact {SPLIT_SHIM} is missing"),
            )));
        }
        arguments.extend([
            "--mount".into(),
            format!(
                "type=bind,source={SPLIT_SHIM},target={},readonly",
                marsh_contracts::process::ARTIFACT
            )
            .into(),
        ]);
        if let Some(capability) = &spec.capability {
            let image_env = self.image_environment(spec, cancel)?;
            job_environment(arguments, directory, capability, &image_env, started)
                .map_err(RuntimeError::Io)?;
        }

        Ok(())
    }

    /// The image config's `Env` (the worker reads only the config; there
    /// is no probe container).
    fn image_environment(
        &self,
        spec: &JobSpec,
        cancel: &Cancellation,
    ) -> Result<Vec<String>, RuntimeError> {
        let cache = IMAGE_ENV.get_or_init(Mutex::default);
        let key = spec.image.as_str().to_owned();
        if let Some(found) = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
        {
            return Ok(found.clone());
        }
        let output = self.checked_cancellable(
            "inspect image environment",
            &self.invocation([
                "image",
                "inspect",
                "--format",
                "{{json .Config.Env}}",
                key.as_str(),
            ]),
            cancel,
            Some(Duration::from_secs(10)),
        )?;
        let env: Vec<String> = serde_json::from_slice::<Option<Vec<String>>>(&output)
            .map_err(|_| RuntimeError::InvalidResourceEvidence)?
            .unwrap_or_default();
        cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, env.clone());
        Ok(env)
    }
}

/// Add the job environment (`PATH` with the links first, `MARSH_JOB`,
/// `MARSH_ENTRY`; `SHELL` stays the image's) and write `job.json` with the salted digests
/// of the whole starting environment and the names received from the
/// parent, so a child gets what this job received or set (s6).
fn job_environment(
    arguments: &mut Vec<OsString>,
    directory: &Path,
    capability: &marsh_contracts::process::JobCapability,
    image_env: &[String],
    started: BTreeMap<String, Vec<u8>>,
) -> io::Result<()> {
    use marsh_contracts::process;
    let mut start = BTreeMap::<String, Vec<u8>>::new();
    for entry in image_env {
        if let Some((name, value)) = entry.split_once('=') {
            start.insert(name.to_owned(), value.as_bytes().to_vec());
        }
    }
    start.extend(started);
    let image_path = start.get("PATH").map_or_else(
        || "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into(),
        |path| String::from_utf8_lossy(path).into_owned(),
    );
    let path = format!("{}:{image_path}", process::LINK_DIR);
    let added = [
        ("PATH", path.clone()),
        ("MARSH_JOB", capability.job.clone()),
        ("MARSH_ENTRY", "1".to_owned()),
    ];
    for (name, value) in &added {
        arguments.extend(["--env".into(), format!("{name}={value}").into()]);
        start.insert((*name).to_owned(), value.as_bytes().to_vec());
    }
    // Salted with the daemon's key, which never enters the container, so the
    // job cannot test guesses of a value against its digest.
    let key = process::decode_hex(&capability.env_key).unwrap_or_default();
    let mut env = start
        .iter()
        .map(|(name, value)| (name.clone(), process::env_digest(&key, name, value)))
        .collect::<BTreeMap<_, _>>();
    // Set by Docker with values the worker does not know: never forwarded.
    for name in ["HOSTNAME", "TERM", "HOME"] {
        env.entry(name.to_owned()).or_default();
    }
    let document = process::JobDocument {
        version: 1,
        job: capability.job.clone(),
        name: capability.name.clone(),
        spawn: capability.spawn.clone(),
        registered: capability.registered.clone(),
        socket: PathBuf::from(process::SOCKET),
        path,
        limits: process::Limits::default(),
        env,
        forwarded: capability.forwarded.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&document).map_err(io::Error::other)?;
    let target = directory.join("job.json");
    fs::write(&target, bytes)?;
    fs::set_permissions(&target, std::os::unix::fs::PermissionsExt::from_mode(0o644))
}

fn resolve_grant_subpath(source: &Path, subpath: &Path) -> Result<PathBuf, RuntimeError> {
    let mut path = source.to_path_buf();
    let last = subpath.components().count();
    for (index, component) in subpath.components().enumerate() {
        let Component::Normal(name) = component else {
            return Err(RuntimeError::InvalidSpec(JobSpecError::InvalidMount));
        };
        path.push(name);
        let metadata = fs::symlink_metadata(&path)?;
        // Workspace admin files (index, HEAD, config, gitfile) are the only
        // regular-file binds; every parent must still be a real directory.
        let file_leaf = index + 1 == last && metadata.is_file();
        if metadata.file_type().is_symlink() || !(metadata.is_dir() || file_leaf) {
            return Err(RuntimeError::InvalidSpec(JobSpecError::InvalidMount));
        }
    }
    Ok(path)
}

fn docker_mount_field(name: &str, path: &Path) -> Result<String, RuntimeError> {
    let path = path
        .to_str()
        .ok_or(RuntimeError::InvalidSpec(JobSpecError::InvalidMount))?;
    let field = format!("{name}={path}");
    if field.contains([',', '"', '\n', '\r']) {
        Ok(format!("\"{}\"", field.replace('"', "\"\"")))
    } else {
        Ok(field)
    }
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("invalid job specification: {0}")]
    InvalidSpec(#[from] JobSpecError),
    #[error("runtime I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error(
        "container runtime {operation} failed{exit_status}: {stderr}",
        exit_status = command_exit_status(*exit_code)
    )]
    CommandFailed {
        operation: &'static str,
        exit_code: Option<i32>,
        stderr: String,
    },
    #[error("container runtime returned an invalid identity")]
    InvalidContainerIdentity,
    #[error("container runtime returned an invalid exit status")]
    InvalidExitStatus,
    #[error("container runtime returned corrupt resource evidence")]
    InvalidResourceEvidence,
    #[error("invalid terminal size")]
    InvalidTerminalSize,
    #[error("invalid trusted SBX proxy environment variable {variable}")]
    InvalidSbxEnvironment { variable: String },
    #[error("trusted SBX CA bundle is unavailable or unsafe")]
    InvalidCaBundle,
    #[error("container engine returned an invalid resize response")]
    InvalidEngineResponse,
    #[error("container engine resize failed (HTTP {status}): {body}")]
    EngineResizeFailed { status: u16, body: String },
    #[error("container deletion could not be verified")]
    DeletionUncertain,
    #[error("{0}")]
    ByteBridge(&'static str),
    #[error(
        "native byte create outcome uncertain; retain carrier for {attempt} until worker/container deletion is verified: {stderr}"
    )]
    ByteCreateUncertain { attempt: String, stderr: String },
}

// Used ONLY on the pre-create side of the boundary. Post-create I/O and bind
// errors must stay uncertain, even when their underlying cause is deterministic.
fn pre_create_error(error: RuntimeError) -> RuntimeError {
    match error {
        RuntimeError::ByteBridge(_) => error,
        RuntimeError::Io(ref cause) if cleanup_uncertain(cause) => error,
        _ => RuntimeError::ByteBridge("native byte carrier preparation unavailable before create"),
    }
}

const MAX_COMMAND_STDERR: usize = 4 * 1024;

fn command_exit_status(exit_code: Option<i32>) -> String {
    exit_code.map_or_else(
        || " (without an exit code)".to_owned(),
        |code| format!(" (exit code {code})"),
    )
}

fn bounded_command_stderr(bytes: &[u8]) -> String {
    let sanitized: String = String::from_utf8_lossy(bytes)
        .chars()
        .map(|character| {
            if character.is_control() && !matches!(character, '\n' | '\r' | '\t') {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect();
    let sanitized = sanitized.trim();
    if sanitized.len() <= MAX_COMMAND_STDERR {
        return sanitized.to_owned();
    }
    let mut end = MAX_COMMAND_STDERR - '…'.len_utf8();
    while !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &sanitized[..end])
}

#[cfg(test)]
mod tests;
