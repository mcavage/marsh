//! Real-process subshells.
//!
//! Bash runs `( ... )`, background lists, pipeline stages, coprocesses, and
//! process substitutions in forked children, so `$BASHPID`, `exit`, signal
//! delivery, and `wait PID` all observe a real process. The shell runs on a
//! multi-threaded Tokio runtime, so a forked child must not touch the parent's
//! runtime: it starts a fresh runtime on a new thread, runs the already-parsed
//! command there, and leaves with `_exit`.
//!
//! Tokio routes every signal through one process-global socket pair. A child
//! that kept sharing it with the parent could consume the parent's wakeups (or
//! lose its own), so the child replaces both ends with a fresh pair before it
//! builds its runtime. The embedding entry point records which descriptors
//! hold that pair; without the record forking is unavailable and callers keep
//! the in-process behavior.

/// How a forked child joins process groups.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ChildGroup {
    /// Stay in the parent's process group.
    Inherit,
    /// Lead a new process group, optionally taking the terminal.
    New {
        /// Whether the new group takes the terminal foreground.
        foreground: bool,
    },
    /// Join an existing process group.
    Join(i32),
}

impl ChildGroup {
    /// Chooses the group a forked child takes for `policy`, mirroring external commands.
    pub(crate) fn for_policy(
        policy: &crate::ProcessGroupPolicy,
        existing: Option<i32>,
        stdin_is_terminal: bool,
    ) -> Self {
        match policy {
            crate::ProcessGroupPolicy::NewProcessGroup => Self::New {
                foreground: stdin_is_terminal,
            },
            crate::ProcessGroupPolicy::SameProcessGroup => {
                existing.map_or(Self::Inherit, Self::Join)
            }
        }
    }
}

/// Signal setup for a forked child.
#[derive(Clone, Debug, Default)]
pub(crate) struct ChildSignals {
    /// Ignore SIGINT and SIGQUIT (an asynchronous list without job control).
    pub ignore_interrupts: bool,
    /// Restore default job-control stop signals (the parent is interactive).
    pub default_stop_signals: bool,
    /// Signals the shell ignores with `trap ''`; they stay ignored.
    pub ignored: Vec<i32>,
}

/// A forked child as seen by the parent.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Forked {
    /// The child's process ID.
    pub pid: i32,
    /// The child's process group, when the parent knows it.
    pub pgid: Option<i32>,
}

#[cfg(unix)]
pub(crate) use imp::standard_descriptor_closed_at_startup;
#[cfg(unix)]
pub(crate) use imp::{
    available, die_by_signal, fork, restore_trapped_signal, shell_pid, wait_for_pid,
};
#[cfg(unix)]
pub use imp::{
    mark_standard_descriptor_closed, note_descriptors_after_runtime,
    note_descriptors_before_runtime,
};

#[cfg(not(unix))]
/// Records descriptors before the runtime is built (no-op on this platform).
pub fn note_descriptors_before_runtime() {}
#[cfg(not(unix))]
/// Records descriptors after the runtime is built (no-op on this platform).
pub fn note_descriptors_after_runtime() {}

#[cfg(not(unix))]
/// Marks a standard descriptor as closed at startup (no-op on this platform).
pub fn mark_standard_descriptor_closed(_fd: i32) {}

#[cfg(not(unix))]
pub(crate) const fn standard_descriptor_closed_at_startup(_fd: i32) -> bool {
    false
}

#[cfg(not(unix))]
pub(crate) fn shell_pid() -> u32 {
    std::process::id()
}

#[cfg(not(unix))]
pub(crate) const fn available() -> bool {
    false
}

#[cfg(not(unix))]
pub(crate) fn restore_trapped_signal(_signal: i32) {}

#[cfg(unix)]
mod imp {
    use super::{ChildGroup, ChildSignals, Forked};
    use std::collections::HashSet;
    use std::io::Write as _;
    use std::os::fd::RawFd;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Mutex, OnceLock};

    static SHELL_PID: AtomicU32 = AtomicU32::new(0);
    static DESCRIPTORS_BEFORE_RUNTIME: Mutex<Option<HashSet<RawFd>>> = Mutex::new(None);
    /// The runtime signal socket pair, grouped by socket (each end may have duplicates).
    static SIGNAL_SOCKETS: OnceLock<Option<[Vec<RawFd>; 2]>> = OnceLock::new();
    /// Dispositions a forked child replaced, restored if it traps the signal.
    static SAVED_ACTIONS: Mutex<Vec<(i32, libc::sigaction)>> = Mutex::new(Vec::new());

    static CLOSED_AT_STARTUP: [std::sync::atomic::AtomicBool; 3] = [
        std::sync::atomic::AtomicBool::new(false),
        std::sync::atomic::AtomicBool::new(false),
        std::sync::atomic::AtomicBool::new(false),
    ];

    /// Records that standard descriptor `fd` was closed when this shell was
    /// started (the Rust runtime then opened `/dev/null` there). The shell keeps
    /// treating it as closed, and the placeholder no longer leaks to children.
    /// Call before the shell is built.
    pub fn mark_standard_descriptor_closed(fd: i32) {
        let Some(closed) = usize::try_from(fd)
            .ok()
            .and_then(|i| CLOSED_AT_STARTUP.get(i))
        else {
            return;
        };
        // SAFETY: only sets the close-on-exec flag of this descriptor number.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        closed.store(true, Ordering::SeqCst);
    }

    /// Whether standard descriptor `fd` was closed when the shell started.
    pub(crate) fn standard_descriptor_closed_at_startup(fd: i32) -> bool {
        usize::try_from(fd)
            .ok()
            .and_then(|fd| CLOSED_AT_STARTUP.get(fd))
            .is_some_and(|closed| closed.load(Ordering::SeqCst))
    }

    /// Returns the PID that `$$` reports: the top-level shell's, even in a subshell.
    pub(crate) fn shell_pid() -> u32 {
        let _ =
            SHELL_PID.compare_exchange(0, std::process::id(), Ordering::SeqCst, Ordering::SeqCst);
        SHELL_PID.load(Ordering::SeqCst)
    }

    fn open_descriptors() -> Vec<RawFd> {
        let directory = if cfg!(target_os = "linux") {
            "/proc/self/fd"
        } else {
            "/dev/fd"
        };
        let Ok(entries) = std::fs::read_dir(directory) else {
            return Vec::new();
        };
        entries
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
            // SAFETY: F_GETFD only inspects the descriptor; it drops the directory's own fd.
            .filter(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } != -1)
            .collect()
    }

    fn unnamed_stream_socket(fd: RawFd) -> Option<u64> {
        // SAFETY: zeroed stat is a valid out-parameter.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fstat writes only into `stat`.
        if unsafe { libc::fstat(fd, &raw mut stat) } != 0
            || (stat.st_mode & libc::S_IFMT) != libc::S_IFSOCK
        {
            return None;
        }
        // SAFETY: zeroed sockaddr_storage is a valid out-parameter.
        let mut address: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        #[allow(clippy::cast_possible_truncation)]
        let mut length = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        // SAFETY: getsockname writes at most `length` bytes into `address`.
        if unsafe { libc::getsockname(fd, (&raw mut address).cast(), &raw mut length) } != 0
            || i32::from(address.ss_family) != libc::AF_UNIX
        {
            return None;
        }
        #[allow(clippy::useless_conversion)]
        Some(u64::from(stat.st_ino))
    }

    /// Records the descriptors open before the embedding builds its Tokio runtime.
    pub fn note_descriptors_before_runtime() {
        let _ = shell_pid();
        if let Ok(mut before) = DESCRIPTORS_BEFORE_RUNTIME.lock() {
            *before = Some(open_descriptors().into_iter().collect());
        }
    }

    /// Identifies the runtime's signal socket pair among descriptors opened since
    /// [`note_descriptors_before_runtime`]. Enables real-process subshells.
    pub fn note_descriptors_after_runtime() {
        let Some(before) = DESCRIPTORS_BEFORE_RUNTIME
            .lock()
            .ok()
            .and_then(|mut b| b.take())
        else {
            return;
        };
        let mut groups: Vec<(u64, Vec<RawFd>)> = Vec::new();
        for fd in open_descriptors() {
            if before.contains(&fd) {
                continue;
            }
            if let Some(inode) = unnamed_stream_socket(fd) {
                match groups.iter_mut().find(|(i, _)| *i == inode) {
                    Some((_, fds)) => fds.push(fd),
                    None => groups.push((inode, vec![fd])),
                }
            }
        }
        let pair = match <[(u64, Vec<RawFd>); 2]>::try_from(groups) {
            Ok([(_, first), (_, second)]) => Some([first, second]),
            Err(_) => None,
        };
        let _ = SIGNAL_SOCKETS.set(pair);
    }

    /// Whether real-process subshells are available.
    pub(crate) fn available() -> bool {
        matches!(SIGNAL_SOCKETS.get(), Some(Some(_)))
    }

    /// Reinstalls the runtime's handler for `signal` after a forked child that
    /// had reset it to the default disposition registers a trap for it.
    pub(crate) fn restore_trapped_signal(signal: i32) {
        let Ok(mut saved) = SAVED_ACTIONS.lock() else {
            return;
        };
        if let Some(index) = saved.iter().position(|(s, _)| *s == signal) {
            let (_, action) = saved.swap_remove(index);
            // SAFETY: reinstalls an action previously returned by sigaction.
            unsafe { libc::sigaction(signal, &raw const action, std::ptr::null_mut()) };
        }
    }

    fn set_disposition(signal: i32, handler: libc::sighandler_t, save: bool) {
        // SAFETY: zeroed sigaction with an empty mask is valid.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = handler;
        // SAFETY: zeroed old-action out-parameter.
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: installs SIG_DFL/SIG_IGN, which run no code in this process.
        if unsafe { libc::sigaction(signal, &raw const action, &raw mut old) } == 0
            && save
            && old.sa_sigaction != libc::SIG_DFL
            && old.sa_sigaction != libc::SIG_IGN
            && let Ok(mut saved) = SAVED_ACTIONS.lock()
        {
            saved.push((signal, old));
        }
    }

    fn replace_signal_sockets() -> bool {
        let Some(Some(groups)) = SIGNAL_SOCKETS.get() else {
            return false;
        };
        let mut pair = [0; 2];
        // SAFETY: socketpair writes two descriptors into `pair`.
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, pair.as_mut_ptr()) } != 0
        {
            return false;
        }
        for (fresh, fds) in pair.iter().zip(groups) {
            for fd in fds {
                // SAFETY: replaces the runtime's descriptor number with the fresh socket.
                unsafe {
                    libc::dup2(*fresh, *fd);
                    libc::fcntl(*fd, libc::F_SETFD, libc::FD_CLOEXEC);
                    let flags = libc::fcntl(*fd, libc::F_GETFL);
                    libc::fcntl(*fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
            }
            // SAFETY: the fresh descriptor is now duplicated where it is needed.
            unsafe { libc::close(*fresh) };
        }
        true
    }

    /// Closes inherited close-on-exec pipes the child does not use, so pipeline
    /// peers see end-of-file and `SIGPIPE` as they would with Bash.
    fn close_foreign_pipes(keep: &HashSet<RawFd>) {
        for fd in open_descriptors() {
            if fd <= 2 || keep.contains(&fd) {
                continue;
            }
            // SAFETY: inspects descriptor flags and type only.
            let cloexec = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if cloexec == -1 || cloexec & libc::FD_CLOEXEC == 0 {
                continue;
            }
            // SAFETY: zeroed stat is a valid out-parameter.
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: fstat writes only into `stat`.
            if unsafe { libc::fstat(fd, &raw mut stat) } == 0
                && (stat.st_mode & libc::S_IFMT) == libc::S_IFIFO
            {
                // SAFETY: the child holds no live object for this inherited pipe.
                unsafe { libc::close(fd) };
            }
        }
    }

    fn exit_child(code: i32) -> ! {
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        // SAFETY: leaves without running the parent's destructors or atexit handlers.
        unsafe { libc::_exit(code) }
    }

    /// Forks a child that runs `body` on a fresh runtime and exits with its status.
    ///
    /// Returns `Ok(None)` when forking is unavailable. `keep` lists the
    /// descriptors the child's shell references; other inherited close-on-exec
    /// pipes are closed in the child.
    pub(crate) fn fork<F, Fut>(
        group: ChildGroup,
        signals: &ChildSignals,
        keep: HashSet<RawFd>,
        body: F,
    ) -> Result<Option<Forked>, crate::error::Error>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = i32>,
    {
        if !available() {
            return Ok(None);
        }
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        // SAFETY: the child only uses fork-safe state: it replaces the runtime's
        // signal sockets, never touches the parent's runtime, and leaves with _exit.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if pid == 0 {
            if !replace_signal_sockets() {
                exit_child(126);
            }
            crate::signals::forget_after_fork();
            match group {
                ChildGroup::Inherit => {}
                ChildGroup::New { foreground } => {
                    // SAFETY: changes only this process's group.
                    unsafe { libc::setpgid(0, 0) };
                    if foreground {
                        let _ = crate::sys::terminal::move_self_to_foreground();
                    }
                }
                ChildGroup::Join(pgid) => {
                    // SAFETY: changes only this process's group.
                    unsafe { libc::setpgid(0, pgid) };
                }
            }
            let interrupt = if signals.ignore_interrupts {
                libc::SIG_IGN
            } else {
                libc::SIG_DFL
            };
            for (signal, handler) in [
                (libc::SIGINT, interrupt),
                (libc::SIGQUIT, interrupt),
                (libc::SIGTERM, libc::SIG_DFL),
                (libc::SIGPIPE, libc::SIG_DFL),
            ] {
                let handler = if signals.ignored.contains(&signal) {
                    libc::SIG_IGN
                } else {
                    handler
                };
                set_disposition(signal, handler, true);
            }
            if signals.default_stop_signals {
                for signal in [libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU] {
                    if !signals.ignored.contains(&signal) {
                        set_disposition(signal, libc::SIG_DFL, true);
                    }
                }
            }
            close_foreign_pipes(&keep);
            let thread = std::thread::Builder::new().spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build();
                let code = match runtime {
                    Ok(runtime) => runtime.block_on(body()),
                    Err(_) => 126,
                };
                exit_child(code)
            });
            if let Ok(thread) = thread {
                let _ = thread.join();
            }
            exit_child(126);
        }
        let pgid = match group {
            ChildGroup::Inherit => None,
            ChildGroup::New { .. } => Some(pid),
            ChildGroup::Join(pgid) => Some(pgid),
        };
        if let Some(pgid) = pgid {
            // SAFETY: mirrors the child's own setpgid so either order wins the race.
            unsafe { libc::setpgid(pid, pgid) };
        }
        drop(body);
        Ok(Some(Forked { pid, pgid }))
    }

    /// Ends this process by `signal` with its default action, as an untrapped
    /// fatal signal would have (falling back to exit status 128+N).
    pub(crate) fn die_by_signal(signal: i32) -> ! {
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        // SAFETY: restores the default action, unblocks the signal, and raises it.
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&raw mut set);
            libc::sigaddset(&raw mut set, signal);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &raw const set, std::ptr::null_mut());
            libc::raise(signal);
            libc::_exit(128 + signal)
        }
    }

    /// Waits for a forked child to exit, returning its status as process output.
    pub(crate) async fn wait_for_pid(pid: i32) -> std::io::Result<std::process::Output> {
        use std::os::unix::process::ExitStatusExt as _;
        let mut sigchld = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;
        loop {
            let mut status = 0;
            // SAFETY: waits only for this specific child.
            let result = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
            if result == pid {
                return Ok(std::process::Output {
                    status: std::process::ExitStatus::from_raw(status),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
            if result == -1 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::Interrupted {
                    return Err(error);
                }
                continue;
            }
            let _ = sigchld.recv().await;
        }
    }
}
