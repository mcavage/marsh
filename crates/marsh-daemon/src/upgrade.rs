//! Upgrade in place: a daemon whose executable was replaced (an installer
//! wrote a new `marshd` over it, or Homebrew's `opt` link names another
//! version) restarts on the new one once nothing runs (`docs/install.md`).
//!
//! The running daemon only notices and waits: running jobs, attached shells
//! and Kit starts keep it. Once idle it takes the lifecycle fence (nothing
//! can begin, background passes included), checks that the file is still
//! the one it validated (SHA-256), and starts the successor. The successor
//! does all of its startup but binding the endpoint, reports ready with the
//! digest of its own executable, and waits until this process is gone. Only
//! then does this daemon release its endpoint and exit. A successor that
//! fails or reports another file is killed and this daemon keeps serving,
//! reporting the failed update. An older version is never switched to.

use super::{DaemonError, artifact_identity, canonical_executable, runtime_identity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    io::{self, BufRead, Read},
    os::unix::process::CommandExt,
    path::{Component, Path, PathBuf},
    process::{Child, ChildStdout, Command, Stdio},
    sync::{Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

/// `marshd --home H --handover`: started by the daemon it replaces.
pub const HANDOVER_FLAG: &str = "--handover";
const CHECK_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a successor may take to report ready.
const READY_TIMEOUT: Duration = Duration::from_mins(1);
/// How long a successor waits for the daemon it replaces to exit.
const HANDOVER_WAIT: Duration = Duration::from_mins(2);

/// `marsh status` `update_pending` (feature `update-pending-v1`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UpdatePending {
    /// The version the daemon restarts on (`marshd --version`).
    pub version: String,
    pub executable: PathBuf,
}

#[derive(Clone)]
struct Candidate {
    path: PathBuf,
    artifact: String,
    /// What was validated: the update and its successor's build identity,
    /// and the SHA-256 of the file that passed. `None`: it did not check out
    /// (or is older, or failed to start); looked at again once it changes.
    pending: Option<(UpdatePending, String, String)>,
    /// Why there is no update for it, for `marsh status`.
    notice: Option<String>,
}

pub(crate) struct UpgradeWatch {
    home: PathBuf,
    own: PathBuf,
    own_artifact: String,
    watched: PathBuf,
    stock_sbx: PathBuf,
    candidate: Mutex<Option<Candidate>>,
    /// Test hook: runs between the last check and the successor's start.
    #[cfg(test)]
    pub(crate) before_launch: Mutex<Option<Box<dyn Fn() + Send>>>,
}

/// A started successor that reported ready, waiting for this process to
/// exit. Dropping it (when this process exits) lets it bind.
pub struct Successor {
    _release: io::PipeWriter,
    child: Child,
    pub executable: PathBuf,
}

impl Successor {
    /// Whether it is still running (it waits for this process to exit).
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

/// How long a successor must stay up after reporting ready.
const SETTLE: Duration = Duration::from_millis(200);

impl UpgradeWatch {
    pub(crate) fn new(
        home: &Path,
        executable: &Path,
        stock_sbx: &Path,
    ) -> Result<Self, DaemonError> {
        let own = canonical_executable(executable)?;
        Ok(Self {
            home: home.to_owned(),
            own_artifact: artifact_identity(&own)?,
            watched: watched_path(&own),
            own,
            stock_sbx: stock_sbx.to_owned(),
            candidate: Mutex::new(None),
            #[cfg(test)]
            before_launch: Mutex::new(None),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Candidate>> {
        self.candidate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn pending(&self) -> Option<(UpdatePending, String)> {
        self.lock()
            .as_ref()
            .and_then(|candidate| candidate.pending.clone())
            .map(|(pending, identity, _)| (pending, identity))
    }

    /// Why an installed replacement is not switched to (older, or it failed).
    pub(crate) fn notice(&self) -> Option<String> {
        self.lock()
            .as_ref()
            .and_then(|candidate| candidate.notice.clone())
    }

    /// Look at the installed executable again: the update this daemon waits
    /// to restart on, with its successor's build identity. A file that is
    /// missing (an install in progress) changes nothing here; one that is
    /// this daemon's own clears the update.
    pub(crate) fn probe(&self) -> Option<(UpdatePending, String)> {
        let Some((path, artifact)) = canonical_executable(&self.watched).ok().and_then(|path| {
            artifact_identity(&path)
                .ok()
                .map(|artifact| (path, artifact))
        }) else {
            return self.pending();
        };
        if path == self.own && artifact == self.own_artifact {
            *self.lock() = None;
            return None;
        }
        if let Some(candidate) = self.lock().as_ref()
            && candidate.path == path
            && candidate.artifact == artifact
        {
            return candidate
                .pending
                .clone()
                .map(|(pending, identity, _)| (pending, identity));
        }
        // Outside the lock: `marsh status` reads the last result meanwhile.
        let mut candidate = Candidate {
            path: path.clone(),
            artifact,
            pending: None,
            notice: None,
        };
        let checked = check_candidate(&path);
        if checked.is_none() {
            candidate.notice = Some(format!(
                "{} does not run as a marshd (`--version`, `--check-config`); not switched to",
                path.display()
            ));
        }
        if let Some((version, digest)) = checked {
            match older(&version, env!("CARGO_PKG_VERSION")) {
                Some(false) => {
                    candidate.pending =
                        runtime_identity(&path, &self.stock_sbx)
                            .ok()
                            .map(|identity| {
                                (
                                    UpdatePending {
                                        version,
                                        executable: path,
                                    },
                                    identity,
                                    digest,
                                )
                            });
                }
                _ => {
                    candidate.notice = Some(format!(
                        "marsh {version} installed is older than the running {}; run `marsh restart` to use it",
                        env!("CARGO_PKG_VERSION")
                    ));
                }
            }
        }
        let pending = candidate
            .pending
            .clone()
            .map(|(pending, identity, _)| (pending, identity));
        *self.lock() = Some(candidate);
        pending
    }

    /// Start the validated successor and wait until it reports ready. Any
    /// difference from what was validated, or a successor that does not get
    /// ready, fails: it is killed, and the update is reported failed (not
    /// retried until the file changes).
    pub(crate) fn start_successor(&self) -> Result<Successor, String> {
        let result = self.try_start();
        if let Err(message) = &result {
            self.report(message);
        }
        result
    }

    /// A successor that died after it was accepted: reap it and report.
    pub(crate) fn fail(&self, mut successor: Successor, why: &str) {
        let _ = successor.child.kill();
        let _ = successor.child.wait();
        self.report(&format!(
            "the update to {} failed: {why}; this daemon keeps running {}",
            successor.executable.display(),
            env!("CARGO_PKG_VERSION")
        ));
    }

    fn report(&self, message: &str) {
        eprintln!("marshd: {message}");
        if let Some(candidate) = self.lock().as_mut() {
            candidate.pending = None;
            candidate.notice = Some(message.to_owned());
        }
    }

    fn try_start(&self) -> Result<Successor, String> {
        let (pending, _, digest) = self
            .lock()
            .as_ref()
            .and_then(|candidate| candidate.pending.clone())
            .ok_or("no update is pending")?;
        let failed = |why: &str| {
            format!(
                "the update to marsh {} failed: {why}; this daemon keeps running {}",
                pending.version,
                env!("CARGO_PKG_VERSION")
            )
        };
        #[cfg(test)]
        if let Some(hook) = self
            .before_launch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            hook();
        }
        if file_digest(&pending.executable).as_deref() != Some(digest.as_str()) {
            return Err(failed(&format!(
                "{} changed or disappeared after it was checked",
                pending.executable.display()
            )));
        }
        let (reader, writer) = io::pipe().map_err(|error| failed(&error.to_string()))?;
        let mut command = Command::new(&pending.executable);
        command
            .arg("--home")
            .arg(&self.home)
            .arg(HANDOVER_FLAG)
            .env("MARSH_SBX", &self.stock_sbx)
            .env_remove(super::daemon_startup::CAPTURE_ENV)
            .stdin(Stdio::from(reader))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = marsh_runtime::with_host_descriptor_creation_excluded(|| command.spawn())
            .map_err(|error| failed(&error.to_string()))?;
        let ready = child
            .stdout
            .take()
            .ok_or_else(|| failed("no ready channel"))
            .and_then(|stdout| wait_ready(stdout, &mut child).map_err(|why| failed(&why)));
        let ready = ready.and_then(|reported| {
            if reported != digest {
                return Err(failed(
                    "the successor runs another file than the one checked",
                ));
            }
            // Ready, and still waiting for this process: not one that
            // reported and went away.
            thread::sleep(SETTLE);
            match child.try_wait() {
                Ok(None) => Ok(()),
                _ => Err(failed("the successor exited after it reported ready")),
            }
        });
        match ready {
            Ok(()) => Ok(Successor {
                _release: writer,
                child,
                executable: pending.executable,
            }),
            Err(message) => {
                let _ = child.kill();
                let _ = child.wait();
                Err(message)
            }
        }
    }
}

/// The successor's `ready DIGEST` line, bounded; an exit first fails.
fn wait_ready(stdout: ChildStdout, child: &mut Child) -> Result<String, String> {
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let mut line = String::new();
        let _ = io::BufReader::new(stdout.take(256)).read_line(&mut line);
        let _ = send.send(line);
    });
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if let Ok(line) = receive.recv_timeout(Duration::from_millis(50)) {
            return line
                .trim()
                .strip_prefix("ready ")
                .map(str::to_owned)
                .ok_or_else(|| match child.try_wait() {
                    Ok(Some(status)) => {
                        format!("the successor exited ({status}) before it was ready")
                    }
                    _ => "the successor did not report ready".to_owned(),
                });
        }
        if Instant::now() >= deadline {
            return Err("the successor did not get ready in time".into());
        }
    }
}

/// SHA-256 of a file's bytes.
fn file_digest(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 16];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => hasher.update(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    Some(super::hex(&hasher.finalize()))
}

/// Whether `candidate` is an older version than `running`, by their
/// numeric `MAJOR.MINOR.PATCH` (a pre-release suffix is ignored, so a
/// same-version rebuild is not older); `None` when either is not one.
pub(crate) fn older(candidate: &str, running: &str) -> Option<bool> {
    let parse = |version: &str| {
        version
            .split(['-', '+'])
            .next()?
            .split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()
            .filter(|parts| parts.len() == 3)
    };
    Some(parse(candidate)? < parse(running)?)
}

/// The path a newer version is installed at: for a Homebrew keg
/// (`PREFIX/Cellar/FORMULA/VERSION/...`) the same file under the formula's
/// `PREFIX/opt/FORMULA` link, which `brew upgrade` moves; else the
/// executable itself, which an installer replaces.
pub(crate) fn watched_path(own: &Path) -> PathBuf {
    let components = own.components().collect::<Vec<_>>();
    if let Some(cellar) = components
        .iter()
        .rposition(|component| component.as_os_str() == "Cellar")
        && components.len() > cellar + 3
        && let Component::Normal(formula) = components[cellar + 1]
    {
        let prefix = components[..cellar].iter().collect::<PathBuf>();
        let link = prefix.join("opt").join(formula);
        if std::fs::symlink_metadata(&link).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return components[cellar + 3..]
                .iter()
                .fold(link, |path, component| path.join(component));
        }
    }
    own.to_owned()
}

/// The replacement's version and digest, when it runs and accepts its own
/// packaged configuration. The digest is taken first: a file changed during
/// the checks fails the launch's re-check.
fn check_candidate(path: &Path) -> Option<(String, String)> {
    let digest = file_digest(path)?;
    let version = run_bounded(path, ["--version"])?;
    let version = version.split_whitespace().nth(1)?.to_owned();
    run_bounded(path, ["--check-config"])?;
    Some((version, digest))
}

fn run_bounded<const N: usize>(path: &Path, arguments: [&str; N]) -> Option<String> {
    let mut child = marsh_runtime::with_host_descriptor_creation_excluded(|| {
        Command::new(path)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    })
    .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.by_ref().take(64 * 1024).read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + CHECK_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let text = reader.join().ok()?;
    status.success().then_some(text)
}

/// In a successor (`HANDOVER_FLAG`), once everything but binding the
/// endpoint is done: report ready with the digest of this executable, then
/// wait, bounded, for the daemon it replaces to exit.
///
/// # Errors
/// Fails when this executable cannot be read or that daemon is still
/// running after the wait.
pub fn handover_ready() -> io::Result<()> {
    use std::io::Write as _;
    let executable = std::env::current_exe()?;
    let digest =
        file_digest(&executable).ok_or_else(|| io::Error::other("cannot read this executable"))?;
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "ready {digest}")?;
    stdout.flush()?;
    drop(stdout);
    let null = std::fs::OpenOptions::new().write(true).open("/dev/null")?;
    rustix::stdio::dup2_stdout(&null)?;
    await_handover()
}

/// Wait, bounded, until stdin (a pipe whose only writer is the daemon being
/// replaced) reads end of file, then detach stdin.
fn await_handover() -> io::Result<()> {
    use rustix::event::{PollFd, PollFlags, poll};
    let deadline = Instant::now() + HANDOVER_WAIT;
    let stdin = rustix::stdio::stdin();
    let mut buffer = [0_u8; 64];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the daemon this one replaces did not exit",
            ));
        }
        let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
        let timeout = rustix::event::Timespec {
            tv_sec: i64::try_from(left.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: 0,
        };
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) | Err(rustix::io::Errno::INTR) => continue,
            Ok(_) => {}
            Err(error) => return Err(error.into()),
        }
        match rustix::io::read(stdin, &mut buffer) {
            Ok(0) => break,
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
    let null = std::fs::File::open("/dev/null")?;
    rustix::stdio::dup2_stdin(&null)?;
    Ok(())
}

/// Whether `argument` is the successor flag.
#[must_use]
pub fn is_handover_flag(argument: &OsStr) -> bool {
    argument == HANDOVER_FLAG
}

#[cfg(test)]
mod tests {
    use super::{older, watched_path};
    use std::path::Path;

    #[test]
    fn a_homebrew_keg_is_watched_through_its_opt_link() {
        let root = tempfile::tempdir().unwrap();
        let keg = root.path().join("Cellar/marsh/0.3.11/libexec/bin");
        std::fs::create_dir_all(&keg).unwrap();
        std::fs::create_dir_all(root.path().join("opt")).unwrap();
        let own = keg.join("marshd");
        // No opt link (a copied keg): the file itself.
        assert_eq!(watched_path(&own), own);
        std::os::unix::fs::symlink("../Cellar/marsh/0.3.11", root.path().join("opt/marsh"))
            .unwrap();
        assert_eq!(
            watched_path(&own),
            root.path().join("opt/marsh/libexec/bin/marshd")
        );
        let plain = Path::new("/Users/someone/.local/bin/marshd");
        assert_eq!(watched_path(plain), plain);
    }

    #[test]
    fn only_an_older_version_is_older() {
        assert_eq!(older("0.3.12", "0.3.13"), Some(true));
        assert_eq!(older("0.3.9", "0.3.10"), Some(true));
        assert_eq!(older("0.3.13", "0.3.13"), Some(false));
        assert_eq!(older("0.3.14", "0.3.13"), Some(false));
        assert_eq!(older("0.3.13-nightly.20261106.gabc", "0.3.13"), Some(false));
        assert_eq!(older("garbage", "0.3.13"), None);
    }
}
