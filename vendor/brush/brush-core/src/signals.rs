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
//!
//! An `INT` the kernel has queued but whose handler has not run yet counts as
//! arrived too. Only the thread that runs the shell takes `INT` (the others
//! block it, see [`crate::traps::keep_interrupts_off_this_thread`]), so such
//! an `INT` is either still pending in the kernel or already recorded when the
//! shell looks, never taken by a thread that has not run the handler yet.

#[cfg(unix)]
pub(crate) use imp::{
    arrived, discard, forget_after_fork, ignored_by_trap, is_recorded, listen, note_child_reaped,
    poison_for_test, record_state_for_test, set_ignored_by_trap, take, take_pending,
    take_reaped_children,
};

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
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};

    /// A signal whose arrivals the handler counts into `handled`.
    ///
    /// An arrival is a handler run not yet consumed, or the signal still
    /// pending in the kernel. Consuming a pending one counts its handler run
    /// in advance (`consumed`), so that run is not a new arrival when it comes.
    struct Recorded {
        signal: i32,
        handled: Arc<AtomicU64>,
        consumed: AtomicU64,
        /// Whether this process has asked for the signal since it started (or
        /// since a fork). Handlers outlive a fork, but arrivals are only
        /// observed once the child listens for the signal itself.
        armed: bool,
    }

    impl Recorded {
        /// Arrivals so far: handler runs, plus one if an `INT` is pending.
        fn through(&self) -> u64 {
            loop {
                let handled = self.handled.load(Ordering::Acquire);
                // Only INT is kept off the runtime's other threads; one the
                // shell started with blocked is never delivered at all.
                let pending = self.signal == libc::SIGINT
                    && !crate::sys::signal::interrupts_blocked_at_start()
                    && kernel_pending(self.signal);
                // A handler run between the two reads may be the pending
                // signal's (the query can unblock it here); read again rather
                // than count it twice.
                if self.handled.load(Ordering::Acquire) == handled {
                    return handled + u64::from(pending);
                }
            }
        }

        fn arrived(&self) -> bool {
            self.through() > self.consumed.load(Ordering::Acquire)
        }

        /// Consumes every arrival so far, including a pending one; returns
        /// whether there was one.
        fn consume(&self) -> bool {
            let through = self.through();
            self.consumed.swap(through, Ordering::AcqRel) < through
        }

        /// Forgets the arrivals so far without querying the kernel (safe in a
        /// forked child).
        fn forget(&self) {
            self.consumed
                .store(self.handled.load(Ordering::Acquire), Ordering::Release);
        }
    }

    /// Whether `signal` is pending for this process: queued by the kernel but
    /// not yet taken by a thread to run its handler.
    fn kernel_pending(signal: i32) -> bool {
        // `sigpending` reports only signals the calling thread blocks, so block
        // the signal around the query. Unblocking it may run its handler here.
        // SAFETY: every set is initialized by sigemptyset, pthread_sigmask or
        // sigpending before it is read, and the thread's mask is restored.
        unsafe {
            let mut only: libc::sigset_t = std::mem::zeroed();
            let mut previous: libc::sigset_t = std::mem::zeroed();
            let mut pending: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&raw mut only);
            libc::sigaddset(&raw mut only, signal);
            if libc::pthread_sigmask(libc::SIG_BLOCK, &raw const only, &raw mut previous) != 0 {
                return false;
            }
            let found = libc::sigpending(&raw mut pending) == 0
                && libc::sigismember(&raw const pending, signal) == 1;
            libc::pthread_sigmask(libc::SIG_SETMASK, &raw const previous, std::ptr::null_mut());
            found
        }
    }

    static LISTENING: AtomicBool = AtomicBool::new(false);
    /// Children reaped since the last [`take_reaped_children`] (for `CHLD` traps).
    static REAPED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// Signals with an empty trap (`trap '' SIG`), one bit each. The shell
    /// drops them; programs it runs start with them ignored, as in Bash.
    static IGNORED_BY_TRAP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Records whether `signal` has an empty trap.
    pub(crate) fn set_ignored_by_trap(signal: i32, ignored: bool) {
        let Some(bit) = u32::try_from(signal).ok().and_then(|n| 1u64.checked_shl(n)) else {
            return;
        };
        if ignored {
            IGNORED_BY_TRAP.fetch_or(bit, Ordering::AcqRel);
        } else {
            IGNORED_BY_TRAP.fetch_and(!bit, Ordering::AcqRel);
        }
    }

    /// Whether `signal` has an empty trap (safe in a forked child).
    pub(crate) fn ignored_by_trap(signal: i32) -> bool {
        u32::try_from(signal)
            .ok()
            .and_then(|n| 1u64.checked_shl(n))
            .is_some_and(|bit| IGNORED_BY_TRAP.load(Ordering::Acquire) & bit != 0)
    }

    /// Notes that the shell reaped a child process.
    pub(crate) fn note_child_reaped() {
        REAPED.fetch_add(1, Ordering::AcqRel);
    }

    /// Returns (and consumes) how many children were reaped since the last call.
    pub(crate) fn take_reaped_children() -> usize {
        REAPED.swap(0, Ordering::AcqRel)
    }
    static RECORDED: Mutex<Vec<Recorded>> = Mutex::new(Vec::new());

    /// Locks [`RECORDED`], also after a thread panicked while holding it (for
    /// instance inside a handler registration in a command substitution, which
    /// runs on a task whose panic the shell survives). An entry is pushed only
    /// once its handler is installed, so the list stays usable; treating a
    /// poisoned lock as empty would silently stop every later trap and `INT`
    /// check in the shell.
    fn recorded() -> MutexGuard<'static, Vec<Recorded>> {
        RECORDED.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Poisons [`RECORDED`], as a panic with the lock held would.
    pub(crate) fn poison_for_test() {
        let poisoner = std::thread::spawn(|| {
            let _held = RECORDED.lock();
            panic!("poisoning the recorded-signals lock for a test");
        });
        let _ = poisoner.join();
    }

    /// Whether `signal` is recorded in this process, as `(armed, arrived)`.
    pub(crate) fn record_state_for_test(signal: i32) -> Option<(bool, bool)> {
        recorded()
            .iter()
            .find(|entry| entry.signal == signal)
            .map(|entry| (entry.armed, entry.arrived()))
    }

    /// Starts recording `signal` for delivery between commands. This installs
    /// a handler for it (replacing the default action), so call it only where
    /// the runtime's handler is (or is about to be) installed anyway. Requires
    /// a runtime. Signals the registry refuses to catch (`KILL`, `STOP`, `ILL`,
    /// `FPE`, `SEGV`: it panics) are not recorded.
    ///
    /// The recording handler is installed before the runtime's own, so
    /// whenever the runtime has dispatched an arrival the recording of it is
    /// already visible: a later `discard` cannot be undone by a handler still
    /// running.
    pub(crate) fn listen(signal: i32) {
        if signal_hook::consts::FORBIDDEN.contains(&signal)
            || tokio::runtime::Handle::try_current().is_err()
        {
            return;
        }
        let mut recorded = recorded();
        if let Some(entry) = recorded.iter_mut().find(|entry| entry.signal == signal) {
            if !entry.armed {
                // Arrivals from before this process listened are not its own.
                entry.forget();
                entry.armed = true;
                LISTENING.store(true, Ordering::Release);
            }
            return;
        }
        let handled = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&handled);
        // SAFETY: the action only increments an atomic, which is
        // async-signal-safe.
        let registered = unsafe {
            signal_hook::low_level::register(signal, move || {
                counter.fetch_add(1, Ordering::AcqRel);
            })
        };
        if registered.is_ok() {
            recorded.push(Recorded {
                signal,
                handled,
                consumed: AtomicU64::new(0),
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
        recorded()
            .iter()
            .filter(|entry| entry.armed && entry.consume())
            .map(|entry| entry.signal)
            .collect()
    }

    /// Returns (and consumes) whether `signal` arrived since the last call.
    pub(crate) fn take(signal: i32) -> bool {
        if !LISTENING.load(Ordering::Acquire) {
            return false;
        }
        recorded()
            .iter()
            .filter(|entry| entry.armed && entry.signal == signal)
            .fold(false, |arrived, entry| entry.consume() || arrived)
    }

    /// Whether `signal` arrived since it was last taken, leaving the record.
    pub(crate) fn arrived(signal: i32) -> bool {
        LISTENING.load(Ordering::Acquire)
            && recorded()
                .iter()
                .any(|entry| entry.armed && entry.signal == signal && entry.arrived())
    }

    /// Whether arrivals of `signal` are being recorded in this process.
    pub(crate) fn is_recorded(signal: i32) -> bool {
        LISTENING.load(Ordering::Acquire)
            && recorded()
                .iter()
                .any(|entry| entry.armed && entry.signal == signal)
    }

    /// Forgets recorded arrivals of `signal`; a wait already delivered it.
    pub(crate) fn discard(signal: i32) {
        if !LISTENING.load(Ordering::Acquire) {
            return;
        }
        for entry in recorded().iter().filter(|entry| entry.signal == signal) {
            entry.consume();
        }
    }

    /// Stops observing arrivals in a forked child, which listens for the
    /// signals it needs itself. The handlers stay installed (a fork must not
    /// enter the signal registry, whose lock another parent thread may hold).
    pub(crate) fn forget_after_fork() {
        LISTENING.store(false, Ordering::Release);
        REAPED.store(0, Ordering::Release);
        let recorded = match RECORDED.try_lock() {
            Ok(recorded) => Some(recorded),
            Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        };
        if let Some(mut recorded) = recorded {
            for entry in recorded.iter_mut() {
                entry.armed = false;
                entry.forget();
            }
        }
    }
}
