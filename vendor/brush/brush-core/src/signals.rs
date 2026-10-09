//! Delivery of signals that arrive while the shell runs builtins.
//!
//! Once the runtime has installed a handler for `INT` or `TERM` (every child
//! wait listens for `INT`; a `TERM` trap listens for `TERM`), the signal no
//! longer has its default effect, and waits only observe it while they run.
//! Like Bash, the shell checks for such signals after each pipeline: a trapped
//! signal runs its trap; an untrapped one makes a noninteractive shell run its
//! `EXIT` trap and die by the signal.
//!
//! An arrival is recorded by the signal handler itself, as Bash's handlers
//! record it, so it is visible the moment the signal has been delivered. It is
//! not taken from the runtime's signal streams: those only learn of a signal
//! when the runtime's driver next dispatches it, and a check made in between
//! (a foreground child killed by the same group `INT` has just been reaped)
//! would miss it and run another command before the shell died.

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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// A signal whose arrivals the handler records into `arrived`.
    struct Recorded {
        signal: i32,
        arrived: Arc<AtomicBool>,
        /// Whether this process has asked for the signal since it started (or
        /// since a fork). Handlers outlive a fork, but arrivals are only
        /// observed once the child listens for the signal itself.
        armed: bool,
    }

    static LISTENING: AtomicBool = AtomicBool::new(false);
    static RECORDED: Mutex<Vec<Recorded>> = Mutex::new(Vec::new());

    /// Starts recording `signal` for delivery between commands. This installs
    /// a handler for it (replacing the default action), so call it only where
    /// the runtime's handler is (or is about to be) installed anyway. Requires
    /// a runtime.
    ///
    /// The recording handler is installed before the runtime's own, so
    /// whenever the runtime has dispatched an arrival the recording of it is
    /// already visible: a later `discard` cannot be undone by a handler still
    /// running.
    pub(crate) fn listen(signal: i32) {
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let Ok(mut recorded) = RECORDED.lock() else {
            return;
        };
        if let Some(entry) = recorded.iter_mut().find(|entry| entry.signal == signal) {
            if !entry.armed {
                // Arrivals from before this process listened are not its own.
                entry.arrived.store(false, Ordering::Release);
                entry.armed = true;
                LISTENING.store(true, Ordering::Release);
            }
            return;
        }
        let arrived = Arc::new(AtomicBool::new(false));
        if signal_hook::flag::register(signal, Arc::clone(&arrived)).is_ok() {
            recorded.push(Recorded {
                signal,
                arrived,
                armed: true,
            });
            LISTENING.store(true, Ordering::Release);
        }
    }

    /// Returns (and consumes) the recorded signals that arrived since the last call.
    pub(crate) fn take_pending() -> Vec<i32> {
        if !LISTENING.load(Ordering::Acquire) {
            return Vec::new();
        }
        let Ok(recorded) = RECORDED.lock() else {
            return Vec::new();
        };
        recorded
            .iter()
            .filter(|entry| entry.armed && entry.arrived.swap(false, Ordering::AcqRel))
            .map(|entry| entry.signal)
            .collect()
    }

    /// Returns (and consumes) whether `signal` arrived since the last call.
    pub(crate) fn take(signal: i32) -> bool {
        if !LISTENING.load(Ordering::Acquire) {
            return false;
        }
        let Ok(recorded) = RECORDED.lock() else {
            return false;
        };
        recorded
            .iter()
            .filter(|entry| entry.armed && entry.signal == signal)
            .fold(false, |arrived, entry| {
                entry.arrived.swap(false, Ordering::AcqRel) || arrived
            })
    }

    /// Forgets recorded arrivals of `signal`; a wait already delivered it.
    pub(crate) fn discard(signal: i32) {
        if !LISTENING.load(Ordering::Acquire) {
            return;
        }
        let Ok(recorded) = RECORDED.lock() else {
            return;
        };
        for entry in recorded.iter().filter(|entry| entry.signal == signal) {
            entry.arrived.store(false, Ordering::Release);
        }
    }

    /// Stops observing arrivals in a forked child, which listens for the
    /// signals it needs itself. The handlers stay installed (a fork must not
    /// enter the signal registry, whose lock another parent thread may hold).
    pub(crate) fn forget_after_fork() {
        LISTENING.store(false, Ordering::Release);
        if let Ok(mut recorded) = RECORDED.try_lock() {
            for entry in recorded.iter_mut() {
                entry.armed = false;
                entry.arrived.store(false, Ordering::Release);
            }
        }
    }
}
