//! Exclusion for the host's non-atomic descriptor creation and SDK spawns.
//!
//! This is not Brush's fork/exec contract. Host controller/daemon callers must
//! not acquire it in a fork child or a pre-exec callback. Closures cover only
//! descriptor creation/checked flags, or the synchronous OS spawn—not waiting,
//! network I/O, SDK commands, or another call to this function.
#[cfg(any(target_os = "macos", feature = "test-support"))]
use std::sync::Mutex;

#[cfg(any(target_os = "macos", feature = "test-support"))]
static HOST_DESCRIPTOR_CREATION: Mutex<()> = Mutex::new(());

/// Exclude host process spawns while a descriptor gains close-on-exec protection.
///
/// All `SystemCommandRunner` spawn routes use this same process-local fence.
/// Production Linux socket creation is atomic and does not use this mutex (in
/// particular, no host-only lock is inherited by forked guest Brush work). The
/// explicit test-support feature exercises the same Mac fence on owned Linux
/// subprocesses; those callers do not qualify installed Mac behavior.
/// Do not reenter or invoke this from a fork child's pre-exec callback.
pub fn with_host_descriptor_creation_excluded<T>(operation: impl FnOnce() -> T) -> T {
    #[cfg(any(target_os = "macos", feature = "test-support"))]
    let _guard = HOST_DESCRIPTOR_CREATION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    operation()
}
