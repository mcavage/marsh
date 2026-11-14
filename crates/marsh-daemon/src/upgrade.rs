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
    ffi::{OsStr, OsString},
    fs,
    io::{self, BufRead, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    os::unix::process::CommandExt,
    path::{Component, Path, PathBuf},
    process::{Child, ChildStdout, Command, Stdio},
    sync::{Mutex, OnceLock, mpsc},
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

struct Retry {
    artifact: String,
    failures: u32,
    not_before: Instant,
}

/// The longest wait before an unavailable look is tried again.
const RETRY_LIMIT: Duration = Duration::from_secs(60);

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
    /// The selected home: what the successor is started with.
    home: PathBuf,
    /// The control home, which no job can reach: the update checks' private
    /// copies live here, never under the selected home that jobs mount.
    control_home: PathBuf,
    own: PathBuf,
    own_artifact: String,
    watched: PathBuf,
    stock_sbx: PathBuf,
    /// Settings as this daemon resolved them at its start, for a successor
    /// that starts elsewhere (`/`) than this daemon was launched.
    environment: OnceLock<Vec<(OsString, OsString)>>,
    candidate: Mutex<Option<Candidate>>,
    /// One probe at a time: a second one (status, the serve loop) waits and
    /// reuses the first's check instead of racing it with a staler result.
    probing: Mutex<()>,
    /// The last look that could not be made (`Look::Unavailable`), and when
    /// to try again. Never a verdict about the file.
    retry: Mutex<Option<Retry>>,
    /// The first wait before such a look is tried again; it doubles per
    /// failure up to `RETRY_LIMIT`.
    pub(crate) retry_base: Duration,
    /// Test hook: runs between the last check and the successor's start.
    #[cfg(test)]
    pub(crate) before_launch: Mutex<Option<Box<dyn Fn() + Send>>>,
    /// Test hook: runs between the look at the file's identity and its copy.
    #[cfg(test)]
    pub(crate) before_check: Mutex<Option<Box<dyn Fn() + Send>>>,
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
/// A successor whose output ended without a ready line may still be exiting:
/// its descriptors close before the process can be reaped. How long its exit
/// status is waited for, so the failure names the exit.
const EXIT_GRACE: Duration = Duration::from_secs(2);

impl UpgradeWatch {
    pub(crate) fn new(
        home: &Path,
        control_home: &Path,
        executable: &Path,
        stock_sbx: &Path,
    ) -> Result<Self, DaemonError> {
        let own = canonical_executable(executable)?;
        sweep_dead_checks(control_home);
        Ok(Self {
            home: home.to_owned(),
            control_home: control_home.to_owned(),
            own_artifact: artifact_identity(&own)?,
            watched: watched_path(&own),
            own,
            stock_sbx: stock_sbx.to_owned(),
            environment: OnceLock::new(),
            candidate: Mutex::new(None),
            probing: Mutex::new(()),
            retry: Mutex::new(None),
            retry_base: Duration::from_secs(1),
            #[cfg(test)]
            before_launch: Mutex::new(None),
            #[cfg(test)]
            before_check: Mutex::new(None),
        })
    }

    /// The resolved settings a successor gets in place of the ones this
    /// daemon inherited; set once, at startup.
    pub(crate) fn set_successor_environment(&self, environment: Vec<(OsString, OsString)>) {
        let _ = self.environment.set(environment);
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
        let _probing = self
            .probing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        if self
            .retry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|retry| retry.artifact == artifact && Instant::now() < retry.not_before)
        {
            return self.pending();
        }
        // Outside the candidate lock: `notice` reads the last result meanwhile.
        let mut candidate = Candidate {
            path: path.clone(),
            artifact,
            pending: None,
            notice: None,
        };
        #[cfg(test)]
        self.run_before_check();
        let (version, digest) = match check_candidate(&self.control_home, &path) {
            Look::Unavailable => {
                // Not a verdict about the update: nothing is remembered about
                // it, the watch stays as it was, and the look is tried again
                // after a growing wait.
                self.note_unavailable(&candidate.artifact);
                return self.pending();
            }
            Look::Checked { digest, version } => {
                // The checks ran on a private copy with these bytes, so they
                // say something about the update only if the installed file
                // has these bytes now, however it was moved, swapped or
                // restored meanwhile. Otherwise (or if it is gone) nothing
                // was checked and the watch stays as it was; the next look
                // starts over. The launch re-checks this digest, and the
                // successor reports its own.
                if file_digest(&path).as_deref() != Some(digest.as_str()) {
                    return self.pending();
                }
                (version, Some(digest))
            }
            Look::Unreadable => {
                // Nothing was copied: if the file also moved on, it was not looked at.
                if artifact_identity(&path).ok().as_deref() != Some(candidate.artifact.as_str()) {
                    return self.pending();
                }
                (None, None)
            }
        };
        if version.is_none() {
            candidate.notice = Some(format!(
                "{} does not run as a marshd (`--version`, `--check-config`); not switched to",
                path.display()
            ));
        }
        if let (Some(version), Some(digest)) = (version, digest) {
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
        *self
            .retry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        *self.lock() = Some(candidate);
        pending
    }

    #[cfg(test)]
    fn run_before_check(&self) {
        if let Some(hook) = self
            .before_check
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            hook();
        }
    }

    /// A look at `artifact` could not be made: try again after a wait that
    /// doubles with each failure in a row, up to `RETRY_LIMIT`.
    fn note_unavailable(&self, artifact: &str) {
        let mut retry = self
            .retry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let failures = retry
            .as_ref()
            .filter(|earlier| earlier.artifact == artifact)
            .map_or(1, |earlier| earlier.failures.saturating_add(1));
        let wait = self
            .retry_base
            .saturating_mul(1_u32 << (failures - 1).min(16))
            .min(RETRY_LIMIT);
        *retry = Some(Retry {
            artifact: artifact.to_owned(),
            failures,
            not_before: Instant::now() + wait,
        });
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
        // Never this daemon's cwd: the successor enters its own control
        // directory before any stock call, and is handed this daemon's
        // settings already made absolute (`set_successor_environment`).
        command
            .current_dir("/")
            .arg("--home")
            .arg(&self.home)
            .arg(HANDOVER_FLAG)
            .env("MARSH_SBX", &self.stock_sbx)
            .envs(self.environment.get().into_iter().flatten().cloned())
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
                .ok_or_else(|| match exit_status(child, EXIT_GRACE) {
                    Some(status) => {
                        format!("the successor exited ({status}) before it was ready")
                    }
                    None => "the successor did not report ready".to_owned(),
                });
        }
        if Instant::now() >= deadline {
            return Err("the successor did not get ready in time".into());
        }
    }
}

/// The child's exit status, waiting up to `grace` for it to be reapable.
fn exit_status(child: &mut Child, grace: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => return None,
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

/// What a look at the installed file found.
enum Look {
    /// The installed file itself could not be read: a verdict about it.
    Unreadable,
    /// The look could not be made for a reason of this machine, not of the
    /// file (no space or descriptors for the private copy, a process that
    /// would not start): nothing is known about the update yet.
    Unavailable,
    /// The SHA-256 of the bytes the checks ran against, and the version if
    /// they all passed.
    Checked {
        digest: String,
        version: Option<String>,
    },
}

/// Why a copy could not be made.
enum CopyFailure {
    /// Reading the installed file failed.
    Source,
    /// Making or writing the private copy failed.
    Local,
}

/// Prefix of the private directories of the update checks, in the control home.
const CHECK_DIRECTORY: &str = ".update-check-";

/// A directory only this daemon can reach, removed on drop. It is made
/// (mode 0700, failing if the name exists, so never through a symlink) in the
/// control home, which no job can reach: under the selected home a job could
/// replace the copy between its write and the daemon's exec of it, or
/// pre-create the names to block every look. The name carries this process's
/// id, for the sweep of what a dead daemon left, and a random part.
struct PrivateDir(PathBuf);

impl PrivateDir {
    fn new(control_home: &Path) -> io::Result<Self> {
        let mut last = io::Error::other("no private directory name");
        for _ in 0..8 {
            let mut random = [0_u8; 8];
            getrandom::fill(&mut random).map_err(io::Error::other)?;
            let path = control_home.join(format!(
                "{CHECK_DIRECTORY}{}-{}",
                std::process::id(),
                super::hex(&random)
            ));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => last = error,
                Err(error) => return Err(error),
            }
        }
        Err(last)
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Removes the private directories that a daemon left when it died during a
/// check. Only those of dead processes: this daemon's predecessor or
/// successor may be running a check of its own during a handover, and so may
/// another watch of this process. A directory of a recycled pid that is alive
/// again stays until that process ends.
fn sweep_dead_checks(control_home: &Path) {
    let Ok(entries) = fs::read_dir(control_home) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name
            .to_str()
            .and_then(|name| name.strip_prefix(CHECK_DIRECTORY))
        else {
            continue;
        };
        let Some(pid) = rest
            .split('-')
            .next()
            .and_then(|pid| pid.parse::<i32>().ok())
            .and_then(rustix::process::Pid::from_raw)
        else {
            continue;
        };
        if rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// Runs the replacement's checks, `--version` and `--check-config`, on a
/// private copy of it, so that what they validate is one fixed set of bytes
/// that nothing can swap, whatever happens to the installed path or its
/// directories meanwhile. The copy's digest is taken as it is written.
///
/// Threat model: this defends against a broken or half-installed update (an
/// installer or another process replacing the file or a directory above it,
/// or swapping it for another build and back, while the checks run), and the
/// copy is in the control home, so no job can reach it. It does not defend
/// against a deliberately hostile candidate: the candidate runs with the
/// user's own privileges and could rewrite its own copy (made read-only, 0500
/// in a 0700 directory, only to cut accidents) or do anything else.
///
/// `--check-config` reads the install's packaged configuration, which
/// `marshd` finds relative to its own location. The copy is elsewhere, so
/// the install's `libexec/marsh` is named explicitly, as `scripts/dist.sh`
/// does.
fn check_candidate(control_home: &Path, path: &Path) -> Look {
    let copied = (|| {
        let configuration = fs::canonicalize(path)
            .map_err(|_| CopyFailure::Source)?
            .parent()
            .and_then(Path::parent)
            .ok_or(CopyFailure::Source)?
            .join("libexec/marsh");
        let directory = PrivateDir::new(control_home).map_err(|_| CopyFailure::Local)?;
        let copy = directory.0.join("marshd");
        let mut source = fs::File::open(path).map_err(|_| CopyFailure::Source)?;
        let mut target = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o500)
            .open(&copy)
            .map_err(|_| CopyFailure::Local)?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 1 << 16];
        loop {
            match source.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    hasher.update(&buffer[..count]);
                    target
                        .write_all(&buffer[..count])
                        .map_err(|_| CopyFailure::Local)?;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return Err(CopyFailure::Source),
            }
        }
        // Closed before it runs: a file open for writing cannot be executed.
        drop(target);
        Ok((
            directory,
            copy,
            configuration,
            super::hex(&hasher.finalize()),
        ))
    })();
    let (_directory, copy, configuration, digest) = match copied {
        Ok(copied) => copied,
        Err(CopyFailure::Source) => return Look::Unreadable,
        Err(CopyFailure::Local) => return Look::Unavailable,
    };
    let Ok(version) = run_bounded(&copy, &["--version"]) else {
        return Look::Unavailable;
    };
    let version = match version {
        Some(text) => text.split_whitespace().nth(1).map(str::to_owned),
        None => None,
    };
    let version = match version {
        Some(version) => {
            match run_bounded(
                &copy,
                &[OsStr::new("--check-config"), configuration.as_os_str()],
            ) {
                Ok(Some(_)) => Some(version),
                Ok(None) => None,
                Err(()) => return Look::Unavailable,
            }
        }
        None => None,
    };
    Look::Checked { digest, version }
}

/// Whether a failure to start a process is of this machine at the moment
/// (descriptors, processes or memory exhausted, a file busy for longer than
/// waited), not a statement about the program.
fn temporary_spawn_failure(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::ExecutableFileBusy
        || [
            rustix::io::Errno::MFILE,
            rustix::io::Errno::NFILE,
            rustix::io::Errno::AGAIN,
            rustix::io::Errno::NOMEM,
        ]
        .iter()
        .any(|errno| error.raw_os_error() == Some(errno.raw_os_error()))
}

/// Runs `path` bounded and returns its stdout if it exits 0. `Ok(None)`: it
/// ran and failed, or cannot be started as a program; `Err`: it could not be
/// started for a reason of this machine at the moment
/// (`temporary_spawn_failure`).
fn run_bounded<S: AsRef<OsStr>>(path: &Path, arguments: &[S]) -> Result<Option<String>, ()> {
    let mut command = Command::new(path);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // On Linux a file just written cannot be executed while a process forked
    // meanwhile (by any thread of this daemon) still holds a copy of its
    // write descriptor, until that process execs: a moment, not a verdict.
    let mut busy = 0_u32;
    let mut child = loop {
        match marsh_runtime::with_host_descriptor_creation_excluded(|| command.spawn()) {
            Ok(child) => break child,
            Err(error) if error.kind() == io::ErrorKind::ExecutableFileBusy && busy < 100 => {
                busy += 1;
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) if temporary_spawn_failure(&error) => return Err(()),
            Err(_) => return Ok(None),
        }
    };
    let Some(mut stdout) = child.stdout.take() else {
        return Ok(None);
    };
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
                return Ok(None);
            }
        }
    };
    let Ok(text) = reader.join() else {
        return Ok(None);
    };
    Ok(status.success().then_some(text))
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
    use super::{UpgradeWatch, older, wait_ready, watched_path};
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    fn install(path: &Path, script: &str) {
        let staged = path.with_extension("new");
        std::fs::write(&staged, script).unwrap();
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&staged, path).unwrap();
    }

    /// The selected home (`root/home`, which jobs mount) and the control home
    /// (`root/control`, which they cannot reach) of a watch.
    fn homes(root: &Path) -> (PathBuf, PathBuf) {
        let (home, control) = (root.join("home"), root.join("control"));
        for directory in [&home, &control] {
            std::fs::create_dir(directory).unwrap();
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        (home, control)
    }

    fn watch_in(root: &Path) -> (UpgradeWatch, PathBuf) {
        let (home, control) = homes(root);
        let sbx = root.join("sbx");
        install(&sbx, "#!/bin/sh\nexit 0\n");
        let marshd = std::fs::canonicalize(root).unwrap().join("marshd");
        install(&marshd, "#!/bin/sh\nexit 0\n");
        (
            UpgradeWatch::new(&home, &control, &marshd, &sbx).unwrap(),
            marshd,
        )
    }

    /// A marshd stand-in at 9.9.9 whose `--check-config` runs `on_check`.
    fn marshd_9_9_9(on_check: &str) -> String {
        format!(
            "#!/bin/sh\ncase \"$1\" in --version) echo 'marshd 9.9.9';; --check-config) {on_check};; esac\nexit 0\n"
        )
    }

    /// An install that is taken away while it is being looked at says nothing
    /// about the file: "a file that is missing (an install in progress)
    /// changes nothing". The replacement here removes the installed file
    /// (by its path: the checks run on a private copy) when asked its version.
    #[test]
    fn a_replacement_removed_during_its_check_leaves_the_watch_as_it_was() {
        let root = tempfile::tempdir().unwrap();
        let (watch, marshd) = watch_in(root.path());
        assert_eq!(watch.probe(), None);
        install(
            &marshd,
            &format!(
                "#!/bin/sh\ncase \"$1\" in --version) rm -f '{}'; exit 1;; esac\nexit 0\n",
                marshd.display()
            ),
        );
        assert_eq!(watch.probe(), None);
        assert_eq!(watch.notice(), None);
        assert!(!marshd.exists());
    }

    /// The same when every check passes: the update that was validated stays
    /// pending, and the vanished file's passing checks replace nothing.
    #[test]
    fn a_replacement_removed_during_checks_that_pass_keeps_the_pending_update() {
        let root = tempfile::tempdir().unwrap();
        let (watch, marshd) = watch_in(root.path());
        install(&marshd, &marshd_9_9_9("exit 0"));
        let pending = watch.probe();
        assert!(pending.is_some());
        install(
            &marshd,
            &marshd_9_9_9(&format!("rm -f '{}'; exit 0", marshd.display())),
        );
        assert_eq!(watch.probe(), pending);
        assert_eq!(watch.notice(), None);
    }

    /// The installed file is swapped for another and back while the checks
    /// run: A's `--version` installs B, and A's own `--check-config` puts A
    /// back and fails. The bytes and the file's identity end up A's again
    /// (B's own check, which would pass and also restore A, is the one a check
    /// by path would reach). A failed its own check, so it must not be pending.
    #[test]
    fn a_file_swapped_for_another_and_back_during_its_checks_is_judged_by_its_own() {
        let root = tempfile::tempdir().unwrap();
        let (watch, marshd) = watch_in(root.path());
        let real = marshd.display();
        let a = format!(
            "#!/bin/sh\ncase \"$1\" in\n--version) cp '{real}.b' '{real}.stage'; mv '{real}.stage' '{real}'; echo 'marshd 9.9.9';;\n--check-config) cp '{real}.a' '{real}.stage'; mv '{real}.stage' '{real}'; exit 1;;\nesac\n"
        );
        let b = format!(
            "#!/bin/sh\ncase \"$1\" in --check-config) cp '{real}.a' '{real}.stage'; mv '{real}.stage' '{real}'; exit 0;; esac\nexit 1\n"
        );
        install(&marshd.with_extension("a"), &a);
        install(&marshd.with_extension("b"), &b);
        install(&marshd, &a);
        assert_eq!(watch.probe(), None);
        assert!(
            watch
                .notice()
                .is_some_and(|notice| notice.contains("does not run"))
        );
    }

    /// The same through the directory: the install's directory is renamed
    /// away and a lookalike renamed into its place, then both put back, so the
    /// file's own inode, size, mtime, ctime and bytes are unchanged at the end.
    #[test]
    fn a_directory_swapped_for_another_and_back_during_checks_is_judged_by_its_own() {
        let root = tempfile::tempdir().unwrap();
        let (home, control) = homes(root.path());
        let sbx = root.path().join("sbx");
        install(&sbx, "#!/bin/sh\nexit 0\n");
        let base = std::fs::canonicalize(root.path()).unwrap();
        for name in ["install", "install.b"] {
            std::fs::create_dir(base.join(name)).unwrap();
        }
        let marshd = base.join("install/marshd");
        install(&marshd, "#!/bin/sh\nexit 0\n");
        let watch = UpgradeWatch::new(&home, &control, &marshd, &sbx).unwrap();
        let swap = format!(
            "mv '{0}/install' '{0}/install.a'; mv '{0}/install.b' '{0}/install'",
            base.display()
        );
        let restore = format!(
            "mv '{0}/install' '{0}/install.b'; mv '{0}/install.a' '{0}/install'",
            base.display()
        );
        let a = format!(
            "#!/bin/sh\ncase \"$1\" in\n--version) {swap}; echo 'marshd 9.9.9';;\n--check-config) {restore}; exit 1;;\nesac\n"
        );
        let b =
            format!("#!/bin/sh\ncase \"$1\" in --check-config) {restore}; exit 0;; esac\nexit 1\n");
        install(&base.join("install.b/marshd"), &b);
        install(&marshd, &a);
        assert_eq!(watch.probe(), None);
        assert!(
            watch
                .notice()
                .is_some_and(|notice| notice.contains("does not run"))
        );
    }

    /// The checks run on a private copy, not on the installed path, in the
    /// control home (never the selected home that jobs mount), owner-only
    /// (directory 0700, file 0500: read-only, but it must run), and nothing is left behind.
    #[test]
    fn checks_run_on_a_private_owner_only_copy_in_the_control_home_that_is_removed_afterwards() {
        let root = tempfile::tempdir().unwrap();
        let (watch, marshd) = watch_in(root.path());
        let record = root.path().join("record");
        install(
            &marshd,
            &format!(
                "#!/bin/sh\ncase \"$1\" in --version) echo \"$0\" >> '{0}'; ls -ld \"$0\" \"$(dirname \"$0\")\" >> '{0}'; echo 'marshd 9.9.9';; esac\nexit 0\n",
                record.display()
            ),
        );
        assert!(watch.probe().is_some());
        let recorded = std::fs::read_to_string(&record).unwrap();
        let (home, control) = (root.path().join("home"), root.path().join("control"));
        let program = Path::new(recorded.lines().next().unwrap());
        assert_ne!(program, marshd, "{recorded}");
        assert!(program.starts_with(&control), "{recorded}");
        assert!(!program.starts_with(&home), "{recorded}");
        let modes: Vec<&str> = recorded
            .lines()
            .skip(1)
            .filter_map(|line| line.split_whitespace().next())
            // `ls -l` marks extended attributes (`@`) and ACLs (`+`) after the mode.
            .map(|mode| mode.trim_end_matches(['@', '+']))
            .collect();
        assert!(modes.contains(&"-r-x------"), "{recorded}");
        assert!(modes.contains(&"drwx------"), "{recorded}");
        assert_eq!(modes.len(), 2, "{recorded}");
        for directory in [&home, &control] {
            assert_eq!(
                std::fs::read_dir(directory).unwrap().count(),
                0,
                "something was left in {}",
                directory.display()
            );
        }
    }

    /// `--check-config` is given the install's packaged configuration
    /// directory, `<prefix>/libexec/marsh` where `<prefix>/bin/marshd` is the
    /// binary, both for a plain install and for a Homebrew keg, where the
    /// binary is at `libexec/bin/marshd`. The stand-in passes only if it is
    /// handed a directory that holds the install's `shell-image`: the copy is
    /// elsewhere, so without the argument it finds nothing.
    #[test]
    fn check_config_is_pointed_at_the_installs_packaged_configuration() {
        for prefix in ["prefix", "keg/libexec"] {
            let root = tempfile::tempdir().unwrap();
            let (home, control) = homes(root.path());
            let sbx = root.path().join("sbx");
            install(&sbx, "#!/bin/sh\nexit 0\n");
            let base = std::fs::canonicalize(root.path()).unwrap().join(prefix);
            std::fs::create_dir_all(base.join("bin")).unwrap();
            std::fs::create_dir_all(base.join("libexec/marsh")).unwrap();
            std::fs::write(base.join("libexec/marsh/shell-image"), "image").unwrap();
            let marshd = base.join("bin/marshd");
            install(&marshd, "#!/bin/sh\nexit 0\n");
            let watch = UpgradeWatch::new(&home, &control, &marshd, &sbx).unwrap();
            install(
                &marshd,
                &marshd_9_9_9("[ -f \"$2/shell-image\" ] || exit 1"),
            );
            assert!(watch.probe().is_some(), "{prefix}");
            assert_eq!(watch.notice(), None, "{prefix}");
        }
    }

    /// Directories a dead daemon left in the control home are removed when a
    /// watch starts; those of live processes (a predecessor or successor
    /// checking during a handover, or this process) and anything else stay.
    #[test]
    fn a_start_sweeps_the_checks_of_dead_daemons_only() {
        let root = tempfile::tempdir().unwrap();
        let (home, control) = homes(root.path());
        let dead = {
            let mut child = Command::new("/usr/bin/true").spawn().unwrap();
            let pid = child.id();
            child.wait().unwrap();
            pid
        };
        let own = std::process::id();
        let make = |name: String| {
            let directory = control.join(name);
            std::fs::create_dir(&directory).unwrap();
            std::fs::write(directory.join("marshd"), "left behind").unwrap();
            directory
        };
        let stale = make(format!(".update-check-{dead}-0000"));
        let ours = make(format!(".update-check-{own}-0000"));
        let init = make(".update-check-1-0000".to_owned());
        std::fs::write(control.join("keepme"), "x").unwrap();
        let sbx = root.path().join("sbx");
        install(&sbx, "#!/bin/sh\nexit 0\n");
        let marshd = std::fs::canonicalize(root.path()).unwrap().join("marshd");
        install(&marshd, "#!/bin/sh\nexit 0\n");
        let _watch = UpgradeWatch::new(&home, &control, &marshd, &sbx).unwrap();
        assert!(!stale.exists());
        assert!(ours.exists() && init.exists() && control.join("keepme").exists());
    }

    /// A name an earlier process of the same pid used (or a job-made one, were
    /// the directory reachable) does not stop the look.
    #[test]
    fn leftover_names_of_the_same_pid_do_not_block_a_check() {
        let root = tempfile::tempdir().unwrap();
        let (watch, marshd) = watch_in(root.path());
        let control = root.path().join("control");
        for counter in 0..4 {
            std::fs::create_dir(
                control.join(format!(".update-check-{}-{counter}", std::process::id())),
            )
            .unwrap();
        }
        install(&marshd, &marshd_9_9_9("exit 0"));
        assert!(watch.probe().is_some());
    }

    /// The file vanishes after the look at its identity and before it can be
    /// copied (an install in progress): nothing was looked at, so the update
    /// that was validated before stays pending and no notice is made.
    #[test]
    fn an_update_that_vanishes_before_it_is_copied_changes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let (watch, marshd) = watch_in(root.path());
        install(&marshd, &marshd_9_9_9("exit 0"));
        let pending = watch.probe();
        assert!(pending.is_some());
        install(&marshd, &marshd_9_9_9("exit 0 # a newer build"));
        let path = marshd.clone();
        *watch.before_check.lock().unwrap() = Some(Box::new(move || {
            std::fs::remove_file(&path).unwrap();
        }));
        assert_eq!(watch.probe(), pending);
        assert_eq!(watch.notice(), None);
    }

    /// The private copy cannot be made (the control home is out of space or
    /// descriptors, a name is taken: here, not writable) once. That says
    /// nothing about the update: no notice, no pending update lost, and the
    /// next look (after the wait) accepts the unchanged, valid file.
    #[test]
    fn a_failure_to_make_the_copy_is_retried_not_held_against_the_update() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let (mut watch, marshd) = watch_in(root.path());
        watch.retry_base = std::time::Duration::ZERO;
        let control = root.path().join("control");
        install(&marshd, &marshd_9_9_9("exit 0"));
        let readonly = control.clone();
        *watch.before_check.lock().unwrap() = Some(Box::new(move || {
            std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o500)).unwrap();
        }));
        assert_eq!(watch.probe(), None);
        assert_eq!(watch.notice(), None);
        std::fs::set_permissions(&control, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(watch.probe().is_some());
        assert_eq!(watch.notice(), None);
    }

    /// While a look keeps failing for such a reason it is not made on every
    /// probe: it waits, and the wait grows.
    #[test]
    fn an_unavailable_look_waits_before_it_is_tried_again() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let (mut watch, marshd) = watch_in(root.path());
        watch.retry_base = std::time::Duration::from_secs(3600);
        let control = root.path().join("control");
        let record = root.path().join("record");
        install(
            &marshd,
            &format!(
                "#!/bin/sh\ncase \"$1\" in --version) echo run >> '{}'; echo 'marshd 9.9.9';; esac\nexit 0\n",
                record.display()
            ),
        );
        std::fs::set_permissions(&control, std::fs::Permissions::from_mode(0o500)).unwrap();
        assert_eq!(watch.probe(), None);
        std::fs::set_permissions(&control, std::fs::Permissions::from_mode(0o700)).unwrap();
        // The cause is gone, but the wait has not run out: no look is made.
        assert_eq!(watch.probe(), None);
        assert!(!record.exists());
        assert_eq!(watch.notice(), None);
    }

    /// A file that cannot be read at all (and has not moved) is reported as
    /// not running, as before.
    #[test]
    fn an_unreadable_update_is_reported_as_not_running() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let (watch, marshd) = watch_in(root.path());
        install(&marshd, &marshd_9_9_9("exit 0"));
        std::fs::set_permissions(&marshd, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(watch.probe(), None);
        assert!(
            watch
                .notice()
                .is_some_and(|notice| notice.contains("does not run"))
        );
    }

    /// On Linux a script cannot be executed while any process holds it open
    /// for writing (ETXTBSY), as happens when another thread forks between
    /// the copy being written and closed. That is a moment, not a verdict:
    /// the check waits it out. (macOS has no such error; the test passes there
    /// either way.)
    #[test]
    fn a_check_waits_out_a_file_that_is_briefly_open_for_writing() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("marshd");
        install(&path, "#!/bin/sh\necho 'marshd 9.9.9'\n");
        let writer = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            drop(writer);
        });
        assert_eq!(
            super::run_bounded(&path, &["--version"]),
            Ok(Some("marshd 9.9.9\n".to_owned()))
        );
        release.join().unwrap();
    }

    /// A successor that fails before it is ready closes its stdout when it
    /// exits, and the daemon can see that end of stream before the process
    /// is reapable. The failure must still be reported as the exit it is.
    #[test]
    fn a_successor_that_closes_its_output_and_then_exits_is_reported_as_exited() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exec >&-; sleep 0.3; exit 3"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let why = wait_ready(stdout, &mut child).unwrap_err();
        assert!(
            why.contains("exited") && why.contains("before it was ready"),
            "{why}"
        );
        let _ = child.wait();
    }

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
