//! Typed system-runner proof that the trusted executable never started. Opaque
//! runners and failures after spawn do not get this classification.
use std::{
    error::Error,
    ffi::OsStr,
    fmt, fs, io,
    path::{Path, PathBuf},
    process::{Child, Command},
};

#[derive(Debug)]
struct CommandNotStarted {
    source: io::Error,
    /// The absolute executable path, when it no longer exists (a Homebrew
    /// upgrade deletes the old version's directory).
    removed: Option<PathBuf>,
}
impl fmt::Display for CommandNotStarted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.removed {
            Some(program) => write!(
                f,
                "host command did not start: {} no longer exists (upgraded or uninstalled?): {}",
                program.display(),
                self.source
            ),
            None => write!(f, "host command did not start: {}", self.source),
        }
    }
}
impl Error for CommandNotStarted {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// True only for the system runner's synchronous OS spawn rejection. A generic
/// io error, timeout, nonzero exit, output failure or teardown error is NOT proof
/// of no SDK effect. Do not infer remote/container absence from this predicate.
#[must_use]
pub fn command_not_started(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(<dyn Error + Send + Sync + 'static>::is::<CommandNotStarted>)
}

pub(crate) fn spawn(command: &mut Command) -> io::Result<Child> {
    // SystemCommandRunner installs no caller-supplied pre_exec hooks. Errors
    // returned here are the OS/std exec handshake rejection, before SDK code.
    crate::with_host_descriptor_creation_excluded(|| command.spawn()).map_err(|source| {
        let removed = removed_program(command.get_program(), &source);
        io::Error::new(source.kind(), CommandNotStarted { source, removed })
    })
}

/// ENOENT also names a missing working directory; only an absolute program
/// that is gone is reported as removed. A bare name was looked up on `PATH`.
fn removed_program(program: &OsStr, error: &io::Error) -> Option<PathBuf> {
    let program = Path::new(program);
    (error.kind() == io::ErrorKind::NotFound
        && program.is_absolute()
        && fs::symlink_metadata(program).is_err())
    .then(|| program.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_removed_absolute_executable_is_named_in_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("sbx");
        let error = spawn(&mut Command::new(&gone)).unwrap_err();
        assert!(command_not_started(&error));
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(
            error.to_string().starts_with(&format!(
                "host command did not start: {} no longer exists (upgraded or uninstalled?): ",
                gone.display()
            )),
            "{error}"
        );

        // A missing working directory is not a removed executable.
        let mut command = Command::new("/bin/sh");
        command.current_dir(dir.path().join("nowhere"));
        let error = spawn(&mut command).unwrap_err();
        assert!(command_not_started(&error));
        assert!(!error.to_string().contains("no longer exists"), "{error}");
    }
}
