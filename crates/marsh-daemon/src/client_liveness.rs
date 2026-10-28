//! A host caller (`marsh run`, a host `split`, `marsh mcp export-serve`)
//! attaches a session with its own pid and detaches it when it finishes. A
//! caller killed before it detaches (SIGKILL, a timeout, Ctrl-C between
//! requests) would otherwise leave its session attached until the daemon
//! restarts, refusing `marsh stop`, `marsh reset` and `marsh workers reset`.
//!
//! The daemon records the pid only when it can signal it at attach, so it is
//! a live process of this user. A later `ESRCH` therefore proves that caller
//! is gone; a reused pid can only delay the release, never release a live
//! caller's session.
use crate::{DaemonStore, JobState, ShellState};

/// Whether `pid` is a live process this daemon may signal.
pub(crate) fn can_watch(pid: u32) -> bool {
    i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
}

fn gone(pid: u32) -> bool {
    i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .is_some_and(|pid| rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH))
}

/// A session whose attaching host process is gone.
pub(crate) struct Departed {
    pub(crate) session: String,
    /// A shell VM is still bound to it: its cleanup is unproved.
    pub(crate) shell_vm_bound: bool,
}

impl DaemonStore {
    /// Attached sessions whose host caller is gone and that have nothing in
    /// flight: no open shell attachment, no queued, starting or running job.
    /// A session with work in flight is reconsidered once that work ends.
    pub(crate) fn departed_sessions(&self) -> Vec<Departed> {
        let mut guard = self.lock();
        let state = &mut *guard;
        let shells = &state.shells;
        state.session_clients.retain(|session, _| {
            shells
                .get(session)
                .is_some_and(|shell| shell.state == ShellState::Attached)
        });
        state
            .session_clients
            .iter()
            .filter(|(session, pid)| {
                gone(**pid)
                    && !state.shell_attachments.contains_key(*session)
                    && !state.jobs.values().any(|job| {
                        job.session_id == **session
                            && matches!(job.state, JobState::Queued | JobState::Running)
                    })
                    && !state
                        .starting_kits
                        .values()
                        .any(|kit| kit.shell_session_id == **session)
            })
            .map(|(session, _)| Departed {
                session: session.clone(),
                shell_vm_bound: state.shell_cleanup_vms.contains_key(session),
            })
            .collect()
    }
}
