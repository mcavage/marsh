//! Delivery of signals that arrive while the shell runs builtins.
//!
//! Once the runtime has installed a handler for `INT` or `TERM` (every child
//! wait listens for `INT`; a `TERM` trap listens for `TERM`), the signal no
//! longer has its default effect, and waits only observe it while they run.
//! Like Bash, the shell checks for such signals after each pipeline: a trapped
//! signal runs its trap; an untrapped one makes a noninteractive shell run its
//! `EXIT` trap and die by the signal.

#[cfg(unix)]
pub(crate) use imp::{discard, forget_after_fork, listen, take, take_pending};

#[cfg(not(unix))]
pub(crate) fn take_pending() -> Vec<i32> {
    Vec::new()
}

#[cfg(not(unix))]
pub(crate) fn discard(_signal: i32) {}

#[cfg(not(unix))]
pub(crate) fn take(_signal: i32) -> bool {
    false
}

#[cfg(unix)]
mod imp {
    use futures::FutureExt as _;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    static LISTENING: AtomicBool = AtomicBool::new(false);
    static LISTENERS: Mutex<Vec<(i32, tokio::signal::unix::Signal)>> = Mutex::new(Vec::new());

    /// Starts recording `signal` for delivery between commands. This installs
    /// the runtime's handler for it, so call it only where that handler is (or
    /// is about to be) installed anyway. Requires a runtime.
    pub(crate) fn listen(signal: i32) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let Ok(mut listeners) = LISTENERS.lock() else {
            return;
        };
        if listeners.iter().any(|(s, _)| *s == signal) {
            return;
        }
        if let Ok(stream) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(signal))
        {
            listeners.push((signal, stream));
            LISTENING.store(true, Ordering::Release);
        }
    }

    /// Returns (and consumes) the recorded signals that arrived since the last call.
    pub(crate) fn take_pending() -> Vec<i32> {
        if !LISTENING.load(Ordering::Acquire) {
            return Vec::new();
        }
        let Ok(mut listeners) = LISTENERS.lock() else {
            return Vec::new();
        };
        let mut pending = Vec::new();
        for (signal, stream) in listeners.iter_mut() {
            let mut arrived = false;
            while let Some(Some(())) = stream.recv().now_or_never() {
                arrived = true;
            }
            if arrived {
                pending.push(*signal);
            }
        }
        pending
    }

    /// Returns (and consumes) whether `signal` arrived since the last call.
    pub(crate) fn take(signal: i32) -> bool {
        if !LISTENING.load(Ordering::Acquire) {
            return false;
        }
        let Ok(mut listeners) = LISTENERS.lock() else {
            return false;
        };
        let mut arrived = false;
        for (_, stream) in listeners.iter_mut().filter(|(s, _)| *s == signal) {
            while let Some(Some(())) = stream.recv().now_or_never() {
                arrived = true;
            }
        }
        arrived
    }

    /// Forgets recorded arrivals of `signal`; a wait already delivered it.
    pub(crate) fn discard(signal: i32) {
        if !LISTENING.load(Ordering::Acquire) {
            return;
        }
        let Ok(mut listeners) = LISTENERS.lock() else {
            return;
        };
        for (_, stream) in listeners.iter_mut().filter(|(s, _)| *s == signal) {
            while let Some(Some(())) = stream.recv().now_or_never() {}
        }
    }

    /// Drops (without running destructors) the parent's listeners in a forked
    /// child, whose runtime they do not belong to.
    pub(crate) fn forget_after_fork() {
        LISTENING.store(false, Ordering::Release);
        if let Ok(mut listeners) = LISTENERS.try_lock() {
            std::mem::forget(std::mem::take(&mut *listeners));
        }
    }
}
