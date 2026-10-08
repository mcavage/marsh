//! Explicit retained process ownership. There is deliberately no detached
//! polling thread: callers perform bounded polls and can inspect every entry.
use crate::AttachedProcess;
use std::{
    io,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// (stable ownership receipt, exact local PID when supplied by the adapter).
pub type RetainedProcess = (u64, Option<u32>);
struct Entry {
    receipt: RetainedProcess,
    process: Box<dyn AttachedProcess>,
}
static NEXT: AtomicU64 = AtomicU64::new(1);
static RETAINED: OnceLock<Mutex<Vec<Entry>>> = OnceLock::new();
fn entries() -> &'static Mutex<Vec<Entry>> {
    RETAINED.get_or_init(Mutex::default)
}

pub(crate) fn retain(process: Box<dyn AttachedProcess>) -> RetainedProcess {
    let receipt = (NEXT.fetch_add(1, Ordering::Relaxed), process.local_pid());
    entries()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(Entry { receipt, process });
    receipt
}
/// Current unreaped children, never a successful-cleanup receipt.
#[must_use]
pub fn retained_processes() -> Vec<RetainedProcess> {
    entries()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .map(|entry| entry.receipt)
        .collect()
}
/// Bounded, synchronous maintenance by the owning caller. `try_wait` adapters
/// must be nonblocking. Remaining entries stay owned and visible after return.
#[must_use]
pub fn poll_retained_processes(budget: Duration) -> Vec<RetainedProcess> {
    let deadline = Instant::now().checked_add(budget);
    loop {
        let remaining = {
            let mut entries = entries()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            entries.retain_mut(|entry| {
                // Never signal an identity whose ownership observation failed.
                match entry.process.try_wait_unreaped() {
                    Ok(Some(_)) => !matches!(entry.process.try_wait(), Ok(Some(_))),
                    Ok(None) => {
                        let _ = entry.process.terminate();
                        !matches!(entry.process.try_wait(), Ok(Some(_)))
                    }
                    Err(_) => true,
                }
            });
            entries
                .iter()
                .map(|entry| entry.receipt)
                .collect::<Vec<_>>()
        };
        if remaining.is_empty() || deadline.is_none_or(|deadline| Instant::now() >= deadline) {
            return remaining;
        }
        thread::sleep(Duration::from_millis(5).min(budget));
    }
}

/// Cancel IO, request exact termination and observe reap within a finite budget.
/// Unreaped handles transfer to the explicit registry, never an opaque wait.
#[must_use]
pub fn finish_owned_process(mut process: Box<dyn AttachedProcess>, grace: Duration) -> bool {
    let io_clean = process.cancel_io().is_ok();
    let terminated = process.terminate().is_ok();
    let reaped = poll_process(process.as_mut(), grace);
    if !reaped {
        retain(process);
    }
    io_clean && terminated && reaped
}
pub(crate) fn poll_process(process: &mut dyn AttachedProcess, budget: Duration) -> bool {
    let deadline = Instant::now().checked_add(budget);
    loop {
        if matches!(process.try_wait(), Ok(Some(_))) {
            return true;
        }
        if deadline.is_none_or(|deadline| Instant::now() >= deadline) {
            return false;
        }
        thread::sleep(Duration::from_millis(5).min(budget));
    }
}

#[derive(Debug)]
pub struct LocalCleanupUncertain {
    pub retained: Option<RetainedProcess>,
}
impl std::fmt::Display for LocalCleanupUncertain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "local command cleanup uncertain; retained={:?}",
            self.retained
        )
    }
}
impl std::error::Error for LocalCleanupUncertain {}
pub(crate) fn uncertainty(retained: Option<RetainedProcess>) -> io::Error {
    io::Error::other(LocalCleanupUncertain { retained })
}
pub(crate) fn check_capacity() -> io::Result<()> {
    let entries = entries()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if entries.len() >= 64 {
        Err(uncertainty(entries.first().map(|entry| entry.receipt)))
    } else {
        Ok(())
    }
}
#[must_use]
pub fn cleanup_uncertain(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<LocalCleanupUncertain>)
}
