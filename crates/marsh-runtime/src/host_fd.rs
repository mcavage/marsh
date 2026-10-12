//! Exclusion for the host's non-atomic descriptor creation and SDK spawns.
//!
//! This is not Brush's fork/exec contract. Host controller/daemon callers must
//! not acquire it in a fork child or a pre-exec callback. Closures cover only
//! descriptor creation/checked flags, or the synchronous OS spawn—not waiting,
//! network I/O, SDK commands, or another call to this function.
#[cfg(any(target_os = "macos", feature = "test-support"))]
use std::sync::Mutex;
use std::{
    io,
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
};

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

// On macOS std creates sockets and only then marks them close-on-exec, so
// each of these runs inside the fence. Each returns the std call's error.

/// [`UnixStream::connect`] that no concurrently spawned child can inherit. A
/// Unix-domain connect does not wait for the listener (macOS refuses a full
/// backlog), so the fence is held only briefly.
///
/// # Errors
/// The connect error.
pub fn connect_unix(path: impl AsRef<Path>) -> io::Result<UnixStream> {
    with_host_descriptor_creation_excluded(|| UnixStream::connect(path))
}

/// [`UnixStream::pair`] that no concurrently spawned child can inherit.
///
/// # Errors
/// The socketpair error.
pub fn unix_stream_pair() -> io::Result<(UnixStream, UnixStream)> {
    with_host_descriptor_creation_excluded(UnixStream::pair)
}

/// [`UnixListener::accept`] whose connection no concurrently spawned child can
/// inherit. A blocking listener is awaited outside the fence, so its single
/// acceptor never stalls a spawn.
///
/// # Errors
/// The poll or accept error.
pub fn accept_unix(listener: &UnixListener) -> io::Result<UnixStream> {
    #[cfg(any(target_os = "macos", feature = "test-support"))]
    if !rustix::fs::fcntl_getfl(listener)?.contains(rustix::fs::OFlags::NONBLOCK) {
        let mut ready = [nix::poll::PollFd::new(
            std::os::fd::AsFd::as_fd(listener),
            nix::poll::PollFlags::POLLIN,
        )];
        loop {
            match nix::poll::poll(&mut ready, nix::poll::PollTimeout::NONE) {
                Err(nix::errno::Errno::EINTR) => {}
                result => break result.map(drop)?,
            }
        }
    }
    with_host_descriptor_creation_excluded(|| listener.accept().map(|(stream, _)| stream))
}
