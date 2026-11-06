//! Upgrade in place: a daemon whose executable was replaced (an installer
//! wrote a new `marshd` over it, or Homebrew's `opt` link names another
//! version) restarts on the new one once nothing runs (`docs/install.md`).
//!
//! The running daemon only notices and waits: running jobs, attached shells
//! and Kit starts keep it. Once idle it stops like a `Shutdown` and starts
//! its successor, which waits for this process to be gone (`await_handover`)
//! and then starts as any daemon does, reconciling the VMs this home owns.

use super::{DaemonError, artifact_identity, canonical_executable, runtime_identity};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    io::{self, Read},
    os::unix::process::CommandExt,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

/// `marshd --home H --handover`: started by the daemon it replaces.
pub const HANDOVER_FLAG: &str = "--handover";
const CHECK_TIMEOUT: Duration = Duration::from_secs(20);
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
    /// `None`: the replacement did not check out (`marshd --version` and
    /// `--check-config`); checked again when its file changes.
    pending: Option<(UpdatePending, String)>,
}

pub(crate) struct UpgradeWatch {
    own: PathBuf,
    own_artifact: String,
    watched: PathBuf,
    stock_sbx: PathBuf,
    candidate: Mutex<Option<Candidate>>,
}

impl UpgradeWatch {
    pub(crate) fn new(executable: &Path, stock_sbx: &Path) -> Result<Self, DaemonError> {
        let own = canonical_executable(executable)?;
        Ok(Self {
            own_artifact: artifact_identity(&own)?,
            watched: watched_path(&own),
            own,
            stock_sbx: stock_sbx.to_owned(),
            candidate: Mutex::new(None),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Candidate>> {
        self.candidate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The update this daemon waits to restart on, with the build identity
    /// its successor will advertise, as last probed.
    fn pending(&self) -> Option<(UpdatePending, String)> {
        self.lock()
            .as_ref()
            .and_then(|candidate| candidate.pending.clone())
    }

    /// Look at the installed executable again. A file that is missing (an
    /// install in progress) changes nothing; one that is this daemon's own
    /// clears the update.
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
            return candidate.pending.clone();
        }
        // Outside the lock: `marsh status` reads the last result meanwhile.
        let pending = check_candidate(&path).and_then(|version| {
            runtime_identity(&path, &self.stock_sbx)
                .ok()
                .map(|identity| {
                    (
                        UpdatePending {
                            version,
                            executable: path.clone(),
                        },
                        identity,
                    )
                })
        });
        // A file still being written fails its check; it is checked again
        // once it changes.
        *self.lock() = Some(Candidate {
            path,
            artifact,
            pending: pending.clone(),
        });
        pending
    }
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

/// The replacement's version, when it runs and accepts its own packaged
/// configuration.
fn check_candidate(path: &Path) -> Option<String> {
    let version = run_bounded(path, ["--version"])?;
    let version = version.split_whitespace().nth(1)?.to_owned();
    run_bounded(path, ["--check-config"])?;
    Some(version)
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

/// Start the successor daemon. It waits until this process has exited (its
/// stdin, a pipe whose only writer is this process, reads end of file), so
/// the endpoint and every lock of this daemon are released before it binds.
///
/// # Errors
/// Returns why the successor could not be started.
pub fn hand_over(home: &Path, next: &Path, stock_sbx: &Path) -> io::Result<()> {
    let (reader, writer) = io::pipe()?;
    let mut command = Command::new(next);
    command
        .arg("--home")
        .arg(home)
        .arg(HANDOVER_FLAG)
        .env("MARSH_SBX", stock_sbx)
        .env_remove(super::daemon_startup::CAPTURE_ENV)
        .stdin(Stdio::from(reader))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    marsh_runtime::with_host_descriptor_creation_excluded(|| command.spawn())?;
    // Closed only when this process exits.
    std::mem::forget(writer);
    Ok(())
}

/// In a successor (`HANDOVER_FLAG`): wait, bounded, for the daemon it
/// replaces to exit, then detach stdin.
///
/// # Errors
/// Fails when that daemon is still running after the wait.
pub fn await_handover() -> io::Result<()> {
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
    use super::watched_path;
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
}
