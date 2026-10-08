//! Process management

use futures::FutureExt;

use crate::{error, sys};

/// A waitable future that will yield the results of a child process's execution.
pub(crate) type WaitableChildProcess = std::pin::Pin<
    Box<dyn futures::Future<Output = Result<std::process::Output, std::io::Error>> + Send + Sync>,
>;

/// Tracks a child process being awaited.
pub struct ChildProcess {
    /// A waitable future that will yield the results of a child process's execution.
    exec_future: WaitableChildProcess,
    /// If available, the process ID of the child.
    pid: Option<sys::process::ProcessId>,
    /// If available, the process group ID of the child.
    pgid: Option<sys::process::ProcessId>,
    interrupted: bool,
    term_trapped: bool,
    terminated: bool,
}

impl ChildProcess {
    /// Wraps a child process and its future.
    pub fn new(
        child: sys::process::Child,
        pid: Option<sys::process::ProcessId>,
        pgid: Option<sys::process::ProcessId>,
        term_trapped: bool,
    ) -> Self {
        Self {
            exec_future: Box::pin(child.wait_with_output()),
            pid,
            pgid,
            interrupted: false,
            term_trapped,
            terminated: false,
        }
    }

    /// Wraps a forked shell child (a real-process subshell).
    #[cfg(unix)]
    pub(crate) fn from_forked(forked: crate::subshell::Forked, term_trapped: bool) -> Self {
        Self {
            exec_future: Box::pin(crate::subshell::wait_for_pid(forked.pid)),
            pid: Some(forked.pid),
            pgid: forked.pgid,
            interrupted: false,
            term_trapped,
            terminated: false,
        }
    }

    /// Returns the process's ID.
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
                output = &mut self.exec_future => {
                    break Ok(ProcessWaitResult::Completed {
                        output: output?,
                        interrupted: std::mem::take(&mut self.interrupted),
                        terminated: std::mem::take(&mut self.terminated),
                    })
                },
                _ = sigtstp.recv() => {
                    break Ok(ProcessWaitResult::Stopped)
                },
                _ = sigchld.recv() => {
                    if sys::signal::poll_for_stopped_children()? {
                        break Ok(ProcessWaitResult::Stopped);
                    }
                },
            }
        }
    }

    pub(crate) fn poll(&mut self) -> Option<Result<std::process::Output, error::Error>> {
        let checkable_future = &mut self.exec_future;
        checkable_future
            .now_or_never()
            .map(|result| result.map_err(Into::into))
    }
}

/// Represents the result of waiting for an executing process.
pub enum ProcessWaitResult {
    /// The process completed.
    Completed {
        /// Captured process output and status.
        output: std::process::Output,
        /// Whether the shell received SIGINT while awaiting this process.
        interrupted: bool,
        /// Whether a trapped SIGTERM arrived while awaiting the process.
        terminated: bool,
    },
    /// The process stopped and has not yet completed.
    Stopped,
}
