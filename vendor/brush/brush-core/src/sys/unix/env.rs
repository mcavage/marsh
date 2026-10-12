//! Environment variable retrieval for Unix platforms.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::sync::OnceLock;

/// Inherited environment entries that are not valid UTF-8.
#[derive(Default)]
struct RawEnvironment {
    /// UTF-8 names with other values: the lossy text the shell imported, and the bytes.
    values: HashMap<String, (String, OsString)>,
    /// Entries whose names are not UTF-8 (the shell cannot name them).
    entries: Vec<(OsString, OsString)>,
}

static RAW_ENVIRONMENT: OnceLock<RawEnvironment> = OnceLock::new();

/// Retrieves environment variables from the host process.
///
/// Shell values are UTF-8 text, so a value that is not is imported lossily.
/// Its bytes are kept (see [`inherited_bytes`]) so that external commands still
/// receive it unchanged, as do entries whose names are not UTF-8; neither may
/// end the shell.
pub(crate) fn get_host_env_vars() -> impl Iterator<Item = (String, String)> {
    let mut raw = RawEnvironment::default();
    let mut vars = vec![];
    for (name, value) in std::env::vars_os() {
        match (name.into_string(), value.into_string()) {
            (Ok(name), Ok(value)) => vars.push((name, value)),
            (Ok(name), Err(value)) => {
                let lossy = value.to_string_lossy().into_owned();
                raw.values.insert(name.clone(), (lossy.clone(), value));
                vars.push((name, lossy));
            }
            (Err(name), value) => raw
                .entries
                .push((name, value.map_or_else(|raw| raw, OsString::from))),
        }
    }
    let _ = RAW_ENVIRONMENT.set(raw);
    vars.into_iter()
}

/// The inherited bytes of variable `name`, if they were not UTF-8 and the
/// shell's `value` is still the text it imported for them.
pub(crate) fn inherited_bytes(name: &str, value: &str) -> Option<&'static OsStr> {
    let (lossy, raw) = RAW_ENVIRONMENT.get()?.values.get(name)?;
    (lossy == value).then_some(raw.as_os_str())
}

/// Inherited entries whose names are not UTF-8, passed to external commands.
pub(crate) fn inherited_unnamed_entries() -> impl Iterator<Item = &'static (OsString, OsString)> {
    RAW_ENVIRONMENT
        .get()
        .into_iter()
        .flat_map(|raw| raw.entries.iter())
}
