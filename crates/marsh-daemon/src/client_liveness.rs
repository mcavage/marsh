//! A host caller (`marsh run`, a host `split`, `marsh mcp export-serve`)
//! attaches a session with its own pid and detaches it when it finishes. A
//! caller killed before it detaches (SIGKILL, a timeout, Ctrl-C between
//! requests) would otherwise leave its session attached until the daemon
//! restarts, refusing `marsh stop`, `marsh reset` and `marsh workers reset`.
//!
//! The daemon records the pid and its start time at attach (`process_identity`):
//! the caller is gone when the pid no longer exists or now belongs to a
//! process that started at another time. A pid it cannot identify is never
//! called gone, so a live caller's session is never released.
use crate::{DaemonStore, JobState, ProcessIdentity, ShellState, process_identity};

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
        let state = self.lock();
        state
            .shells
            .values()
            .filter(|shell| {
                shell.state == ShellState::Attached
                    && !state.shell_attachments.contains_key(&shell.session_id)
                    && process_identity(shell.pid, shell.process_started.as_deref())
                        == ProcessIdentity::Gone
                    && !state.jobs.values().any(|job| {
                        job.session_id == shell.session_id
                            && matches!(job.state, JobState::Queued | JobState::Running)
                    })
                    && !state
                        .starting_kits
                        .values()
                        .any(|kit| kit.shell_session_id == shell.session_id)
            })
            .map(|shell| Departed {
                session: shell.session_id.clone(),
                shell_vm_bound: state.shell_cleanup_vms.contains_key(&shell.session_id),
            })
            .collect()
    }
}
