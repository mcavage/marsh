//! Signal processing utilities

use crate::{error, sys, traps};

pub(crate) use nix::sys::signal::Signal;

pub(crate) fn continue_process(pid: sys::process::ProcessId) -> Result<(), error::Error> {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::SIGCONT)
        .map_err(|_errno| error::ErrorKind::FailedToSendSignal)?;
    Ok(())
}

/// Sends a signal to a specific process.
///
/// # Arguments
/// * `pid` - The process ID to send the signal to
/// * `signal` - The signal to send (must be a real signal, not a trap signal)
pub fn kill_process(
    pid: sys::process::ProcessId,
    signal: traps::TrapSignal,
) -> Result<(), error::Error> {
    let translated_signal = match signal {
        traps::TrapSignal::Signal(signal) => Some(signal),
        // Signal 0 (named EXIT) probes for existence and permission.
        traps::TrapSignal::Exit => None,
        traps::TrapSignal::Debug | traps::TrapSignal::Err | traps::TrapSignal::Return => {
            return Err(error::ErrorKind::InvalidSignal(signal.to_string()).into());
        }
    };

    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), translated_signal)
        .map_err(|_errno| error::ErrorKind::FailedToSendSignal)?;

    Ok(())
}

/// Whether `pid` leads a process group other than the shell's own.
pub(crate) fn leads_separate_process_group(pid: sys::process::ProcessId) -> bool {
    let pid = nix::unistd::Pid::from_raw(pid);
    nix::unistd::getpgid(Some(pid)) == Ok(pid) && nix::unistd::getpgrp() != pid
}

pub(crate) fn lead_new_process_group() -> Result<(), error::Error> {
    nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0))?;
    Ok(())
}

pub(crate) fn tstp_signal_listener() -> Result<tokio::signal::unix::Signal, error::Error> {
    let signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(
        nix::libc::SIGTSTP,
    ))?;
    Ok(signal)
}

pub(crate) fn chld_signal_listener() -> Result<tokio::signal::unix::Signal, error::Error> {
    let signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;
    Ok(signal)
}

pub(crate) async fn await_ctrl_c() -> std::io::Result<()> {
    // Waiting for INT installs the runtime's handler; keep observing INT between
    // commands from now on.
    crate::signals::listen(libc::SIGINT);
    tokio::signal::ctrl_c().await
}

pub(crate) async fn await_term() -> Result<(), error::Error> {
    crate::signals::listen(libc::SIGTERM);
    let mut listener = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    // This listener only sees signals from its creation on. One that arrived
    // earlier (while a command ran, or between the last command and this wait)
    // was only recorded by the persistent listener, and would otherwise wait
    // for the next command boundary. Checking after creating the listener
    // leaves no gap: an arrival is in one of the two.
    if crate::signals::take(libc::SIGTERM) {
        return Ok(());
    }
    let _ = listener.recv().await;
    Ok(())
}

/// As [`await_term`], for the one wait for TERM that runs while no command
/// does (the idle input read). The runtime may dispatch an arrival late,
/// after a command boundary already ran its trap from the handler's record;
/// that dispatch must not run the trap again. The handler records every
/// arrival before the runtime's dispatch, so only a recorded one is new.
pub(crate) async fn await_undelivered_term() -> Result<(), error::Error> {
    crate::signals::listen(libc::SIGTERM);
    let mut listener = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    if crate::signals::take(libc::SIGTERM) {
        return Ok(());
    }
    loop {
        let _ = listener.recv().await;
        if crate::signals::take(libc::SIGTERM) || !crate::signals::is_recorded(libc::SIGTERM) {
            return Ok(());
        }
    }
}

/// Blocks every signal in the calling thread until dropped, around a `fork`.
///
/// A forked child starts with the shell's handlers, so a signal sent to it
/// before it restores default dispositions would run the shell's handler there
/// and be lost (`sleep 3 & kill $!` could leave `sleep` running). Blocked, the
/// signal stays pending until the child has restored them and its mask.
pub(crate) struct BlockedForFork {
    previous: libc::sigset_t,
}

impl BlockedForFork {
    pub(crate) fn new() -> Self {
        // SAFETY: sigfillset/pthread_sigmask only write the provided sets.
        unsafe {
            let mut all: libc::sigset_t = std::mem::zeroed();
            let mut previous: libc::sigset_t = std::mem::zeroed();
            libc::sigfillset(&raw mut all);
            libc::pthread_sigmask(libc::SIG_BLOCK, &raw const all, &raw mut previous);
            Self { previous }
        }
    }

    /// The calling thread's mask before blocking, which the child restores.
    pub(crate) const fn previous(&self) -> libc::sigset_t {
        self.previous
    }
}

impl Drop for BlockedForFork {
    fn drop(&mut self) {
        // SAFETY: restores the mask saved by `new` in this same thread.
        unsafe {
            libc::pthread_sigmask(
                libc::SIG_SETMASK,
                &raw const self.previous,
                std::ptr::null_mut(),
            )
        };
    }
}

/// In a forked child about to exec: gives every caught signal its default
/// action, then restores `previous` (see [`BlockedForFork`]), so a signal that
/// arrived since the fork takes effect as it would on the executed program.
/// Only async-signal-safe calls.
pub(crate) fn restore_defaults_after_fork(previous: &libc::sigset_t) {
    // SAFETY: sigaction queries and installs SIG_DFL only; pthread_sigmask
    // installs the saved mask.
    unsafe {
        for signal in 1..65 {
            let mut current: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(signal, std::ptr::null(), &raw mut current) == 0
                && current.sa_sigaction != libc::SIG_DFL
                && current.sa_sigaction != libc::SIG_IGN
            {
                let mut default: libc::sigaction = std::mem::zeroed();
                default.sa_sigaction = libc::SIG_DFL;
                libc::sigaction(signal, &raw const default, std::ptr::null_mut());
            }
        }
        libc::pthread_sigmask(libc::SIG_SETMASK, previous, std::ptr::null_mut());
    }
}

pub(crate) fn mask_sigttou() -> Result<(), error::Error> {
    let ignore = nix::sys::signal::SigAction::new(
        nix::sys::signal::SigHandler::SigIgn,
        nix::sys::signal::SaFlags::empty(),
        nix::sys::signal::SigSet::empty(),
    );

    // SAFETY:
    // Setting the signal action should be safe here. The unsafe concerns
    // for calling `sigaction` are primarily around ensuring that any provided
    // signal handler functions are only performing operations that are
    // safe to do in a signal handler context. Here we are not providing
    // a custom handler, just asking the OS to ignore the signal.
    unsafe { nix::sys::signal::sigaction(nix::sys::signal::Signal::SIGTTOU, &ignore) }?;

    Ok(())
}

pub(crate) fn poll_for_stopped_children() -> Result<bool, error::Error> {
    let mut found_stopped = false;

    loop {
        let wait_status = waitid_all(
            nix::sys::wait::WaitPidFlag::WUNTRACED | nix::sys::wait::WaitPidFlag::WNOHANG,
        );
        match wait_status {
            Ok(nix::sys::wait::WaitStatus::Stopped(_stopped_pid, _signal)) => {
                found_stopped = true;
            }
            Ok(_) => break,
            Err(nix::errno::Errno::ECHILD) => break,
            Err(e) => return Err(e.into()),
        }
    }

    Ok(found_stopped)
}

#[cfg(not(any(target_os = "macos", target_os = "netbsd", target_os = "openbsd")))]
fn waitid_all(
    flags: nix::sys::wait::WaitPidFlag,
) -> Result<nix::sys::wait::WaitStatus, nix::errno::Errno> {
    nix::sys::wait::waitid(nix::sys::wait::Id::All, flags)
}

// nix does not expose `waitid` on NetBSD/OpenBSD; `waitpid` for any child is
// equivalent for the flags used here.
#[cfg(any(target_os = "netbsd", target_os = "openbsd"))]
fn waitid_all(
    flags: nix::sys::wait::WaitPidFlag,
) -> Result<nix::sys::wait::WaitStatus, nix::errno::Errno> {
    nix::sys::wait::waitpid(None, Some(flags))
}

//
// N.B. These functions were mostly copied from nix::sys::wait (https://github.com/nix-rust/nix, MIT license)
// to enable use of the `waitid` call on macOS. Ideally nix would expose it on macOS and we would
// remove this code.
//

#[cfg(target_os = "macos")]
fn waitid_all(
    flags: nix::sys::wait::WaitPidFlag,
) -> Result<nix::sys::wait::WaitStatus, nix::errno::Errno> {
    // SAFETY:
    // Code copied from nix::sys::wait implementation of waitid for other platforms.
    // The siginfo structure is valid when filled with zeroes. Memory is zeroed
    // rather than uninitialized, as not all platforms initialize the memory in
    // the StillAlive case.
    let mut siginfo: nix::libc::siginfo_t = unsafe { std::mem::zeroed() };

    // SAFETY:
    // Code copied from nix::sys::wait implementation of waitid for other platforms.
    nix::errno::Errno::result(unsafe {
        nix::libc::waitid(nix::libc::P_ALL, 0, &raw mut siginfo, flags.bits())
    })?;

    siginfo_to_wait_status(siginfo)
}

#[cfg(target_os = "macos")]
fn siginfo_to_wait_status(
    siginfo: nix::libc::siginfo_t,
) -> Result<nix::sys::wait::WaitStatus, nix::errno::Errno> {
    // SAFETY:
    // Code copied from nix::sys::wait implementation of waitid for other platforms.
    let si_pid = unsafe { siginfo.si_pid() };
    if si_pid == 0 {
        return Ok(nix::sys::wait::WaitStatus::StillAlive);
    }

    let pid = nix::unistd::Pid::from_raw(si_pid);

    // SAFETY:
    // Code copied from nix::sys::wait implementation of waitid for other platforms.
    let si_status = unsafe { siginfo.si_status() };

    let status = match siginfo.si_code {
        nix::libc::CLD_EXITED => nix::sys::wait::WaitStatus::Exited(pid, si_status),
        nix::libc::CLD_KILLED | nix::libc::CLD_DUMPED => nix::sys::wait::WaitStatus::Signaled(
            pid,
            nix::sys::signal::Signal::try_from(si_status)?,
            siginfo.si_code == nix::libc::CLD_DUMPED,
        ),
        nix::libc::CLD_STOPPED => {
            nix::sys::wait::WaitStatus::Stopped(pid, nix::sys::signal::Signal::try_from(si_status)?)
        }
        nix::libc::CLD_CONTINUED => nix::sys::wait::WaitStatus::Continued(pid),
        _ => return Err(nix::errno::Errno::EINVAL),
    };

    Ok(status)
}
