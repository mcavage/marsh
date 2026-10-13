//! Process observation shared by the shell test binaries.
#![allow(dead_code)]

use std::ffi::OsStr;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, ExitStatus, Output};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
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

/// A spawned child that cannot outlive its test: dropping it, including while
/// a failed assertion unwinds, SIGKILLs the child's process group when it
/// leads one (else the child) and reaps it. Reap it only through
/// [`wait_for_exit`], which disarms the guard.
pub struct Guarded {
    child: Option<Child>,
    group: bool,
}

impl Guarded {
    pub fn new(child: Child) -> Self {
        let pid = pid_of(&child);
        let group = nix::unistd::getpgid(Some(pid)) == Ok(pid);
        Self {
            child: Some(child),
            group,
        }
    }

    fn kill_and_reap(&mut self) -> Option<ExitStatus> {
        let mut child = self.child.take()?;
        if self.group {
            let _ = nix::sys::signal::killpg(pid_of(&child), nix::sys::signal::Signal::SIGKILL);
        }
        let _ = child.kill();
        child.wait().ok()
    }
}

impl From<Child> for Guarded {
    fn from(child: Child) -> Self {
        Self::new(child)
    }
}

impl std::ops::Deref for Guarded {
    type Target = Child;
    fn deref(&self) -> &Child {
        self.child.as_ref().unwrap()
    }
}

impl std::ops::DerefMut for Guarded {
    fn deref_mut(&mut self) -> &mut Child {
        self.child.as_mut().unwrap()
    }
}

impl Drop for Guarded {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

/// Spawns `command` as the leader of a new process group, guarded.
pub fn spawn_group(command: &mut Command) -> Guarded {
    Guarded::new(command.process_group(0).spawn().unwrap())
}

fn pid_of(child: &Child) -> nix::unistd::Pid {
    nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap())
}

/// Waits for `child` to exit and its output pipes to close. On expiry the
/// child (and its process group, when it leads one) is killed and the test
/// fails instead of hanging.
pub fn wait_for_exit(child: impl Into<Guarded>, what: &str, budget: Duration) -> Output {
    let mut child = child.into();
    let deadline = Instant::now() + budget;
    let stdout = child.stdout.take().map(Drain::new);
    let stderr = child.stderr.take().map(Drain::new);
    let mut status = None;
    while Instant::now() < deadline {
        status = child.try_wait().unwrap();
        if status.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let closed = |drain: &Option<Drain>| drain.as_ref().is_none_or(|d| d.closed_by(deadline));
    if let Some(status) = status.filter(|_| closed(&stdout) && closed(&stderr)) {
        child.child = None;
        return Output {
            status,
            stdout: stdout.map(|d| d.taken()).unwrap_or_default(),
            stderr: stderr.map(|d| d.taken()).unwrap_or_default(),
        };
    }
    let killed = child.kill_and_reap();
    let state = match status {
        Some(status) => format!("exited {status} but its output is still open"),
        None => format!("still running (killed: {killed:?})"),
    };
    let text =
        |drain: Option<Drain>| drain.map(|d| String::from_utf8_lossy(&d.taken()).into_owned());
    panic!(
        "{what}: {state} after {budget:?}; stdout {:?}; stderr {:?}",
        text(stdout),
        text(stderr)
    );
}

/// A pipe read to end of file on its own thread, observable at any time.
struct Drain {
    bytes: Arc<Mutex<Vec<u8>>>,
    closed: mpsc::Receiver<()>,
}

impl Drain {
    fn new(mut pipe: impl std::io::Read + Send + 'static) -> Self {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (done, closed) = mpsc::channel::<()>();
        let sink = Arc::clone(&bytes);
        std::thread::spawn(move || {
            let _done = done;
            let mut buffer = [0_u8; 8192];
            while let Ok(count @ 1..) = pipe.read(&mut buffer) {
                sink.lock().unwrap().extend_from_slice(&buffer[..count]);
            }
        });
        Self { bytes, closed }
    }

    fn closed_by(&self, deadline: Instant) -> bool {
        let wait = deadline.saturating_duration_since(Instant::now());
        self.closed.recv_timeout(wait) == Err(mpsc::RecvTimeoutError::Disconnected)
    }

    fn taken(&self) -> Vec<u8> {
        std::mem::take(&mut self.bytes.lock().unwrap())
    }
}

/// SIGKILLs every descendant of `root` (not `root` itself), however its
/// members are grouped. They are stopped first, until no new one appears, so
/// none can fork past the sweep or be orphaned out of the tree before it.
pub fn kill_descendants(root: u32) {
    use nix::sys::signal::{Signal, kill};
    let mut stopped = std::collections::BTreeSet::new();
    loop {
        let table = Command::new("ps")
            .args(["-A", "-o", "pid=,ppid="])
            .output()
            .unwrap();
        let edges: Vec<(u32, u32)> = String::from_utf8_lossy(&table.stdout)
            .lines()
            .filter_map(|row| {
                let mut fields = row.split_whitespace();
                Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
            })
            .collect();
        let mut tree = vec![root];
        let mut found = Vec::new();
        while let Some(parent) = tree.pop() {
            for &(pid, ppid) in &edges {
                if ppid == parent && pid != root && !found.contains(&pid) {
                    found.push(pid);
                    tree.push(pid);
                }
            }
        }
        let fresh: Vec<u32> = found
            .into_iter()
            .filter(|pid| !stopped.contains(pid))
            .collect();
        if fresh.is_empty() {
            break;
        }
        for pid in fresh {
            let _ = kill(
                nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap()),
                Signal::SIGSTOP,
            );
            stopped.insert(pid);
        }
    }
    for pid in stopped {
        let _ = kill(
            nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap()),
            Signal::SIGKILL,
        );
    }
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

/// Panics if `script` opens a FIFO it creates with a blocking `open` in the
/// shell under test (outside the single-quoted scripts it hands to children).
///
/// On macOS a signal that interrupts a blocked FIFO `open` (the shell's own
/// `SIGCHLD` when one of its children exits) backs the opener out after the
/// peer may already have opened: the peer reads end-of-file and leaves, and
/// the retried `open` (Bash and Rust both retry `EINTR`) waits forever for a
/// peer that is gone. Linux has no such window. So a script opens its gate
/// read-write before any child starts (`exec 3<>gate`), which never blocks,
/// and releases it through that descriptor (`echo >&3`).
pub fn assert_no_blocking_fifo_open(script: &str) {
    let fifos = script
        .split([';', '\n'])
        .filter_map(|command| command.trim().strip_prefix("mkfifo "))
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>();
    let shell_text = script.split('\'').step_by(2).collect::<Vec<_>>().join(" ");
    for fifo in fifos {
        for (at, _) in shell_text.match_indices(fifo) {
            let before = shell_text[..at].trim_end();
            let whole_word = shell_text[at + fifo.len()..]
                .chars()
                .next()
                .is_none_or(|next| !(next.is_alphanumeric() || "_-.".contains(next)));
            let blocking =
                before.ends_with('<') || (before.ends_with('>') && !before.ends_with("<>"));
            assert!(
                !(whole_word && blocking),
                "the shell under test opens FIFO {fifo} with a blocking open, which macOS can \
                 leave waiting forever when a child exits meanwhile; open it read-write before \
                 starting children (exec 3<>{fifo}) and write to that descriptor: {script}"
            );
        }
    }
}

/// The only variables a test shell (or anything else a test starts) inherits
/// from the test process: never the developer's whole login environment, which
/// holds credentials. `LC_*` is inherited too; a test adds what it needs.
/// `MARSH_TEST_RUN` marks the run's processes for `tests/leaked_processes.py`.
const INHERITED: &[&str] = &[
    "PATH",
    "HOME",
    "TERM",
    "LANG",
    "TMPDIR",
    "USER",
    "LOGNAME",
    "MARSH_TEST_RUN",
];

/// `program` with a cleared environment plus the [`INHERITED`] allowlist.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command
        .env_clear()
        .envs(std::env::vars_os().filter(|(name, _)| {
            name.to_str()
                .is_some_and(|name| INHERITED.contains(&name) || name.starts_with("LC_"))
        }));
    command
}

/// Whether a variable name looks like it holds a credential.
pub fn credential_shaped(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    ["_API_KEY", "_TOKEN", "_PASSWORD"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
        || name.contains("SECRET")
}

/// The canary `make test` exports to every test process.
pub const CANARY: &str = "MARSH_TEST_CANARY_API_KEY";

/// Panics if `names` (a shell's environment names, one per line, as `env |
/// cut -d= -f1` prints them) holds a credential-shaped one. Under `make test`
/// (`MARSH_TEST_RUN` set) the canary must be in this process's environment,
/// so the check is not vacuous. Only names are ever read, never values.
pub fn assert_no_credential_names(what: &str, names: &str) {
    if std::env::var_os("MARSH_TEST_RUN").is_some() {
        assert!(
            std::env::var_os(CANARY).is_some(),
            "make test exports {CANARY}; without it this check proves nothing"
        );
    }
    assert!(
        names.lines().any(|name| name == "PATH"),
        "{what}: no PATH in {names:?}"
    );
    let leaked = names
        .lines()
        .filter(|name| credential_shaped(name))
        .collect::<Vec<_>>();
    assert!(
        leaked.is_empty(),
        "{what} received credential-shaped variables: {leaked:?}"
    );
}

/// Runs `command`, which prints its environment's names one per line, and
/// checks them with [`assert_no_credential_names`].
pub fn assert_no_credentials_reach(what: &str, command: &mut Command) {
    let output = command
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap_or_else(|error| panic!("{what}: {error}"));
    assert!(output.status.success(), "{what}: {output:?}");
    assert_no_credential_names(what, &String::from_utf8_lossy(&output.stdout));
}
