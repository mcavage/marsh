//! Process observation shared by the shell test binaries.
#![allow(dead_code)]

use std::process::{Child, Output};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Polls `condition` until it holds; panics naming `what` once `budget` passes.
pub fn wait_until(what: &str, budget: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + budget;
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out after {budget:?} waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits for `child` to exit. On expiry the child (and its process group, when
/// it leads one) is killed and the test fails instead of hanging.
pub fn wait_for_exit(mut child: Child, what: &str, budget: Duration) -> Output {
    let deadline = Instant::now() + budget;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap());
            if nix::unistd::getpgid(Some(pid)) == Ok(pid) {
                let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
            }
            let _ = child.kill();
            let status = child.wait().unwrap();
            // A descendant outside the group may still hold the pipes open, so
            // take only what is already buffered.
            let stdout = child.stdout.take().map(read_available);
            let stderr = child.stderr.take().map(read_available);
            panic!(
                "{what}: still running after {budget:?} ({status}); stdout {stdout:?}; stderr {stderr:?}"
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

fn read_available(pipe: impl std::io::Read + std::os::fd::AsFd) -> String {
    use std::io::Read as _;
    let flags = nix::fcntl::fcntl(pipe.as_fd(), nix::fcntl::FcntlArg::F_GETFL).unwrap();
    let flags = nix::fcntl::OFlag::from_bits_retain(flags) | nix::fcntl::OFlag::O_NONBLOCK;
    nix::fcntl::fcntl(pipe.as_fd(), nix::fcntl::FcntlArg::F_SETFL(flags)).unwrap();
    let mut bytes = Vec::new();
    let _ = pipe.take(1 << 20).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Every thread of `pid` is asleep. A shell observed in this state after it
/// started a child has reached a blocking wait (for the child, a signal, or
/// input); it is not between commands. A runnable thread reads as running.
pub fn all_threads_asleep(pid: u32) -> bool {
    let states = thread_states(pid);
    !states.is_empty() && states.iter().all(|state| matches!(state, 'S' | 'I'))
}

/// `pid` has exited: it is gone, or a zombie nobody has reaped yet (an
/// orphan whose new parent, e.g. a container's init, does not reap).
pub fn has_exited(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err()
        || u32::try_from(pid).is_ok_and(|pid| {
            let states = thread_states(pid);
            !states.is_empty() && states.iter().all(|state| *state == 'Z')
        })
}

#[cfg(target_os = "linux")]
fn thread_states(pid: u32) -> Vec<char> {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    tasks
        .filter_map(|task| {
            let stat = std::fs::read_to_string(task.ok()?.path().join("stat")).ok()?;
            stat.rsplit_once(") ")?.1.chars().next()
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn thread_states(pid: u32) -> Vec<char> {
    // `ps -M` prints one row per thread; the state is the first letter of the
    // STAT column.
    let output = std::process::Command::new("ps")
        .args(["-M", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let Some(column) = lines.next().and_then(|header| header.find("STAT")) else {
        return Vec::new();
    };
    lines
        .filter_map(|row| row.get(column..)?.chars().next())
        .collect()
}

/// Waits until `pid` is blocked (see [`all_threads_asleep`]).
pub fn wait_until_blocked(pid: u32, what: &str) {
    wait_until(what, Duration::from_secs(20), || all_threads_asleep(pid));
}

/// The differential oracle is GNU Bash 5.3 first on `PATH` (macOS ships 3.2).
pub fn require_bash_oracle() {
    static VERSION: OnceLock<String> = OnceLock::new();
    let version = VERSION.get_or_init(|| {
        let output = std::process::Command::new("bash")
            .args([
                "--noprofile",
                "--norc",
                "-c",
                "printf '%s.%s' \"${BASH_VERSINFO[0]}\" \"${BASH_VERSINFO[1]}\"",
            ])
            .output()
            .expect("run bash from PATH");
        String::from_utf8_lossy(&output.stdout).into_owned()
    });
    assert_eq!(
        version, "5.3",
        "the Bash oracle must be GNU Bash 5.3 first on PATH, found {version:?}"
    );
}
