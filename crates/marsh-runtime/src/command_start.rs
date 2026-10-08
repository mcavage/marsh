//! Typed system-runner proof that the trusted executable never started. Opaque
//! runners and failures after spawn do not get this classification.
use std::{
    error::Error,
    fmt, io,
    process::{Child, Command},
};

#[derive(Debug)]
struct CommandNotStarted(io::Error);
impl fmt::Display for CommandNotStarted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "host command did not start: {}", self.0)
    }
}
impl Error for CommandNotStarted {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
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
    crate::with_host_descriptor_creation_excluded(|| command.spawn())
        .map_err(|error| io::Error::new(error.kind(), CommandNotStarted(error)))
}
