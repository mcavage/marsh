//! Environment variable retrieval for Unix platforms.

use std::ffi::OsString;
use std::sync::OnceLock;

/// Inherited environment entries whose names are not UTF-8 (the shell cannot
/// name them); they are passed to external commands unchanged.
static UNNAMED_ENTRIES: OnceLock<Vec<(OsString, OsString)>> = OnceLock::new();

/// Retrieves environment variables from the host process. Values are decoded
/// losslessly (see [`crate::bytes`]), so external commands receive them
/// unchanged; neither kind of entry may end the shell.
pub(crate) fn get_host_env_vars() -> impl Iterator<Item = (String, String)> {
    let mut unnamed = vec![];
    let mut vars = vec![];
    for (name, value) in std::env::vars_os() {
        match name.into_string() {
            Ok(name) => vars.push((name, crate::bytes::from_os_str(&value).into_owned())),
            Err(name) => unnamed.push((name, value)),
        }
    }
    let _ = UNNAMED_ENTRIES.set(unnamed);
    vars.into_iter()
}

/// Inherited entries whose names are not UTF-8, passed to external commands.
pub(crate) fn inherited_unnamed_entries() -> impl Iterator<Item = &'static (OsString, OsString)> {
    UNNAMED_ENTRIES.get().into_iter().flatten()
}
