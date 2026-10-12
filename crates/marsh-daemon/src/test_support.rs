//! One generous wait for every test condition, so a starved runner is slow
//! rather than red. A correct run never gets near the budget.

use std::{
    io::{self, Read},
    os::unix::net::UnixStream,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

pub const PATIENCE: Duration = Duration::from_mins(1);

/// Poll `probe` until it yields a value; fail after [`PATIENCE`].
pub fn wait_for<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

pub fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    wait_for(what, || condition().then_some(()));
}

pub fn recv<T>(receiver: &mpsc::Receiver<T>, what: &str) -> T {
    receiver
        .recv_timeout(PATIENCE)
        .unwrap_or_else(|error| panic!("{what}: {error}"))
}

/// Run `work` on its own thread so a call that should return promptly fails
/// the test instead of hanging it.
pub fn returns<T: Send + 'static>(what: &str, work: impl FnOnce() -> T + Send + 'static) -> T {
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let _ = send.send(work());
    });
    recv(&receive, what)
}

/// Run `operation` while no process spawn is in flight. A spawn briefly
/// duplicates every descriptor of this process (close-on-exec ones too, until
/// the child's exec completes), so a lock file another test just closed can
/// still be held by a sibling test's half-started child. Reopen after a drop,
/// and spawn test processes, inside this fence (product spawns use it too).
pub fn fenced<T>(operation: impl FnOnce() -> T) -> T {
    marsh_runtime::with_host_descriptor_creation_excluded(operation)
}

/// Open a daemon store (again) inside the spawn fence.
pub fn reopen(home: &std::path::Path) -> Result<crate::DaemonStore, crate::DaemonError> {
    fenced(|| crate::DaemonStore::open(home))
}

/// A connected socket pair created inside the spawn fence. On macOS the pair
/// gains close-on-exec only after creation; a child spawned in between would
/// keep a "closed" peer open for its whole life.
pub fn socket_pair() -> (
    std::os::unix::net::UnixStream,
    std::os::unix::net::UnixStream,
) {
    fenced(std::os::unix::net::UnixStream::pair).unwrap()
}

/// Read `stream` to EOF within [`PATIENCE`]. macOS can leave a thread blocked
/// in `read` after its peer's `shutdown` (a lost wakeup; a full close always
/// wakes it), so poll with short timeouts instead of one blocking read.
pub fn read_to_end(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    // macOS rejects setsockopt (EINVAL) once the peer has fully closed; a
    // read then cannot block.
    let _ = stream.set_read_timeout(Some(Duration::from_millis(20)));
    let deadline = Instant::now() + PATIENCE;
    let mut bytes = Vec::new();
    let mut buffer = [0; 16 * 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(bytes),
            Ok(count) => bytes.extend_from_slice(&buffer[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) && Instant::now() < deadline => {}
            Err(error) => return Err(error),
        }
    }
}
