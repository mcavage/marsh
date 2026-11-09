//! Deliberate behavior breaks for E2E tests. Compiled only with the
//! `test-breaks` feature; release builds always answer `false` and never read
//! the environment. See `docs/design/test-breaks.md`.

#[cfg(all(feature = "test-breaks", marsh_release_packaging))]
compile_error!(
    "the test-breaks feature is for test builds only; release packaging (MARSH_RELEASE_PACKAGING=1) refuses it"
);

/// Provenance marker. Every binary linked with `test-breaks` keeps these bytes,
/// whether or not a break mode is reachable from it. Packaging checks for it
/// (`scripts/check-no-test-breaks.sh`), so a feature-enabled binary is refused
/// even when the environment name was optimized away.
#[cfg(feature = "test-breaks")]
#[used]
pub static MARSH_FEATURE_TEST_BREAKS: [u8; 25] = *b"marsh-feature:test-breaks";

#[cfg(feature = "test-breaks")]
static MODES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// The only launch argument that makes a daemon read `MARSH_TEST_BREAK`.
/// `marshd --home H --test-breaks-from-env`: the test harness passes it. The
/// `marsh` CLI never does when it starts a daemon, whatever features either
/// binary was built with, so no CLI environment can select a break mode.
#[cfg(feature = "test-breaks")]
const FROM_ENV_FLAG: &str = "--test-breaks-from-env";

/// Called once by `marshd` before it parses its arguments. Removes
/// `--test-breaks-from-env` from `arguments`; if it was there, and this is the
/// host (macOS), reads `MARSH_TEST_BREAK` from this process's environment into
/// the break set. Without the flag the environment is never read, so a
/// feature-built daemon ignores the variable. On every other target the set
/// stays empty even with the flag. Every other process, including the `marsh`
/// CLI and guest binaries, never calls this: their sets stay empty.
#[cfg(feature = "test-breaks")]
pub fn init_from_daemon_launch(arguments: &mut Vec<std::ffi::OsString>) {
    let flagged = arguments.iter().any(|argument| argument == FROM_ENV_FLAG);
    arguments.retain(|argument| argument != FROM_ENV_FLAG);
    let modes = if flagged && cfg!(target_os = "macos") {
        std::env::var("MARSH_TEST_BREAK")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|mode| !mode.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        Vec::new()
    };
    let _ = MODES.set(modes);
}

/// A guest process (the worker in a Kit VM) takes the modes its daemon
/// handed it in a dedicated field of the job (DECISIONS 22), never from its
/// environment. Only the first call counts, and only where the process is not
/// the macOS daemon (whose set comes from its launch). Release builds ignore it.
#[cfg(feature = "test-breaks")]
pub fn attest_guest(modes: &[String]) {
    if !cfg!(target_os = "macos") {
        let _ = MODES.set(modes.to_vec());
    }
}

/// Release form: nothing is recorded.
#[cfg(not(feature = "test-breaks"))]
#[inline]
pub fn attest_guest(_modes: &[String]) {}

/// Which of `names` the daemon's set selects, for the attested field a job
/// carries to its worker.
#[cfg(feature = "test-breaks")]
#[must_use]
pub fn selected(names: &[&str]) -> Vec<String> {
    names
        .iter()
        .filter(|name| break_mode(name))
        .map(|name| (*name).to_owned())
        .collect()
}

/// Release form: always empty.
#[cfg(not(feature = "test-breaks"))]
#[inline]
#[must_use]
pub fn selected(_names: &[&str]) -> Vec<String> {
    Vec::new()
}

/// Whether the initialized break set selects `name`. False until
/// [`init_from_daemon_launch`] runs.
#[cfg(feature = "test-breaks")]
#[must_use]
pub fn break_mode(name: &str) -> bool {
    MODES
        .get()
        .is_some_and(|modes| modes.iter().any(|mode| mode == name))
}

/// Release form: nothing is read, and every mode is off. The launch flag is
/// unknown to a release daemon, which refuses it as a usage error.
#[cfg(not(feature = "test-breaks"))]
#[inline]
pub fn init_from_daemon_launch(_arguments: &mut Vec<std::ffi::OsString>) {}

/// Release form: always `false`, and no reference to the environment.
#[cfg(not(feature = "test-breaks"))]
#[inline]
#[must_use]
pub const fn break_mode(_name: &str) -> bool {
    false
}
