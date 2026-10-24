//! Process management

use std::{
    path::PathBuf,
    sync::{
        Arc, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
};

use futures::FutureExt;

use crate::{ExecutionResult, error, openfiles::OpenFiles, sys};

/// A waitable future that will yield the results of a child process's execution.
pub(crate) type WaitableChildProcess = std::pin::Pin<
    Box<dyn futures::Future<Output = Result<ExecutionResult, std::io::Error>> + Send>,
>;

/// Tracks a child process being awaited.
pub struct ChildProcess {
    /// A waitable future that will yield the results of a child process's execution.
    /// It's only ever reached through `&mut self` (with `get_mut`, which never locks); the
    /// mutex just makes this `Sync` without the future having to be.
    exec_future: std::sync::Mutex<WaitableChildProcess>,
    /// The process ID of the child, if it's a native process of the shell's.
    pid: Option<sys::process::ProcessId>,
    /// If available, the process group ID of the child.
    pgid: Option<sys::process::ProcessId>,
    /// Set once the process is known to have died of `SIGINT`.
    killed_by_int: Arc<AtomicBool>,
    interrupted: bool,
    term_trapped: bool,
    terminated: bool,
}

impl ChildProcess {
    /// Wraps a future that yields the result of a process, for a
    /// [`ProcessLauncher`](crate::extensions::ProcessLauncher) to return.
    ///
    /// # Arguments
    ///
    /// * `future` - The future that will yield the process's result.
    pub fn from_future(
        future: impl futures::Future<Output = Result<ExecutionResult, std::io::Error>> + Send + 'static,
    ) -> Self {
        Self {
            exec_future: std::sync::Mutex::new(Box::pin(future)),
            pid: None,
            pgid: None,
            killed_by_int: Arc::new(AtomicBool::new(false)),
            interrupted: false,
            term_trapped: false,
            terminated: false,
        }
    }

    /// Wraps a future that yields the output of a native process, noting
    /// whether it died of `SIGINT`.
    ///
    /// # Arguments
    ///
    /// * `future` - The future that will yield the process's output.
    #[allow(dead_code, reason = "unused on platforms that can't start processes")]
    pub(crate) fn from_output_future(
        future: impl futures::Future<Output = Result<std::process::Output, std::io::Error>>
        + Send
        + 'static,
    ) -> Self {
        let killed_by_int = Arc::new(AtomicBool::new(false));
        let noted = killed_by_int.clone();
        let mut child = Self::from_future(async move {
            let output = future.await?;
            #[cfg(unix)]
            noted.store(
                std::os::unix::process::ExitStatusExt::signal(&output.status) == Some(libc::SIGINT),
                Ordering::Relaxed,
            );
            #[cfg(not(unix))]
            let _ = &noted;
            Ok(ExecutionResult::from(output))
        });
        child.killed_by_int = killed_by_int;
        child
    }

    /// Wraps a forked shell child (a real-process subshell).
    #[cfg(unix)]
    pub(crate) fn from_forked(forked: crate::subshell::Forked, term_trapped: bool) -> Self {
        Self::from_output_future(crate::subshell::wait_for_pid(forked.pid))
            .native_child(Some(forked.pid), forked.pgid)
            .with_term_trapped(term_trapped)
    }

    /// Marks whether a `SIGTERM` trap is set: a waiter then notes the signal's
    /// arrival, for the interpreter to deliver once the process is reaped.
    ///
    /// # Arguments
    ///
    /// * `term_trapped` - Whether the shell traps `SIGTERM`.
    #[must_use]
    pub(crate) const fn with_term_trapped(mut self, term_trapped: bool) -> Self {
        self.term_trapped = term_trapped;
        self
    }

    /// Marks the process as a native child of the shell's, with the given IDs: job
    /// control signals it, and waiting for it notices when it stops.
    ///
    /// # Arguments
    ///
    /// * `pid` - The process ID, if known.
    /// * `pgid` - The process group ID, if the process has one.
    #[allow(dead_code, reason = "unused on platforms that can't start processes")]
    #[must_use]
    pub(crate) const fn native_child(
        mut self,
        pid: Option<sys::process::ProcessId>,
        pgid: Option<sys::process::ProcessId>,
    ) -> Self {
        self.pid = pid;
        self.pgid = pgid;
        self
    }

    /// Returns the process's ID, if it's a native process of the shell's.
    pub const fn pid(&self) -> Option<sys::process::ProcessId> {
        self.pid
    }

    /// Returns the process's group ID.
    pub const fn pgid(&self) -> Option<sys::process::ProcessId> {
        self.pgid
    }

    /// Waits for the process to exit.
    pub async fn wait(&mut self) -> Result<ProcessWaitResult, error::Error> {
        #[allow(unused_mut, reason = "only mutated on some platforms")]
        let mut sigtstp = sys::signal::tstp_signal_listener()?;
        #[allow(unused_mut, reason = "only mutated on some platforms")]
        let mut sigchld = sys::signal::chld_signal_listener()?;

        // An INT recorded since the child started arrived before the runtime's
        // listener below existed, which would not see it.
        #[cfg(unix)]
        if crate::signals::arrived(libc::SIGINT) {
            self.interrupted = true;
        }

        // The child may have stopped before there were listeners to hear about it
        // (e.g., straight after `fg` resumed it).
        if let Some(pid) = self.pid
            && sys::signal::poll_for_stopped_child(pid)?
        {
            return Ok(ProcessWaitResult::Stopped);
        }

        #[allow(clippy::ignored_unit_patterns)]
        loop {
            tokio::select! {
                biased;
                result = sys::signal::await_term(), if self.term_trapped => {
                    result?;
                    self.terminated = true;
                },
                _ = sys::signal::await_ctrl_c() => {
                    // The foreground group receives SIGINT as well. Remember that the
                    // shell received it so the interpreter can deliver a registered trap
                    // after the child has been reaped.
                    self.interrupted = true;
                },
                result = self.exec_future.get_mut().unwrap_or_else(PoisonError::into_inner) => {
                    #[cfg(unix)]
                    crate::signals::note_child_reaped();
                    // The child can be reaped before the runtime's listener
                    // wakes for an INT it already recorded; that INT still
                    // arrived during the wait.
                    #[cfg(unix)]
                    if crate::signals::arrived(libc::SIGINT) {
                        self.interrupted = true;
                    }
                    break Ok(ProcessWaitResult::Completed {
                        result: result?,
                        interrupted: std::mem::take(&mut self.interrupted),
                        terminated: std::mem::take(&mut self.terminated),
                        killed_by_int: self.killed_by_int.load(Ordering::Relaxed),
                    })
                },
                // Only this child's stop counts: another wait may be under way at the same time
                // (e.g., a background job's), for a child of its own, and the terminal's stop
                // may reach the shell without stopping this child.
                _ = sigtstp.recv() => {
                    if self.poll_for_stop()? {
                        break Ok(ProcessWaitResult::Stopped);
                    }
                },
                _ = sigchld.recv() => {
                    if self.poll_for_stop()? {
                        break Ok(ProcessWaitResult::Stopped);
                    }
                },
            }
        }
    }

    /// Returns whether the process has stopped, consuming its stop status.
    fn poll_for_stop(&self) -> Result<bool, error::Error> {
        match self.pid {
            Some(pid) => sys::signal::poll_for_stopped_children(pid),
            None => Ok(false),
        }
    }

    pub(crate) fn poll(&mut self) -> Option<Result<ExecutionResult, error::Error>> {
        let checkable_future = self
            .exec_future
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        let result = checkable_future
            .now_or_never()
            .map(|result| result.map_err(Into::into));
        #[cfg(unix)]
        if result.is_some() {
            crate::signals::note_child_reaped();
        }
        result
    }
}

/// Represents the result of waiting for an executing process.
pub enum ProcessWaitResult {
    /// The process completed.
    Completed {
        /// The process's result.
        result: ExecutionResult,
        /// Whether the shell received SIGINT while awaiting this process.
        interrupted: bool,
        /// Whether a trapped SIGTERM arrived while awaiting the process.
        terminated: bool,
        /// Whether the process died of `SIGINT`.
        killed_by_int: bool,
    },
    /// The process stopped and has not yet completed.
    Stopped,
}

/// A process for the shell to launch: a program the shell has already resolved
/// (builtins, functions, and the like never get this far), with everything it
/// should start with.
#[non_exhaustive]
pub struct LaunchRequest {
    /// Path to the executable, as resolved by the shell; relative to
    /// [`Self::working_dir`], if it's relative.
    pub program: PathBuf,
    /// The value the process should observe as `argv[0]`.
    pub argv0: String,
    /// Arguments to the process, not including `argv[0]`.
    pub args: Vec<String>,
    /// The complete environment for the process.
    pub env: Vec<(String, String)>,
    /// The working directory the process should start in.
    pub working_dir: PathBuf,
    /// The process's file descriptors. A standard stream (0, 1, or 2) that isn't
    /// among them is closed; a launcher that can't start a process with one closed
    /// gives it the null device, as the default launcher does, never one of the
    /// shell's own. The shell's own standard streams
    /// ([`OpenFile::Stdin`](crate::openfiles::OpenFile::Stdin), and so on) are
    /// shared with the process, under whichever descriptors hold them.
    pub open_files: OpenFiles,
    /// The process group the process should run in.
    pub process_group: ProcessGroup,
    /// Whether to kill the process if the shell drops the [`ChildProcess`] for it
    /// before it exits.
    pub kill_on_drop: bool,
    /// What a launcher that runs a file the platform can't execute with the shell's own
    /// interpreter writes to standard error first, if that file is a binary one: the
    /// shell's diagnostic prefix and the file's name, then `: `.
    pub binary_diagnostic: String,
}

/// The process group a process should run in. Platforms and
/// [`ProcessLauncher`](crate::extensions::ProcessLauncher)s without process
/// groups ignore this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProcessGroup {
    /// The shell's process group.
    Inherit,
    /// An existing process group, with the given ID.
    Join(sys::process::ProcessId),
    /// A new process group, led by the process.
    New {
        /// Whether the new group should become the terminal's foreground group.
        take_foreground: bool,
    },
    /// A new session, led by the process, with the terminal as its controlling
    /// terminal.
    NewSession,
}
