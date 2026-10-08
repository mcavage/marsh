//! Pathname checks for the same-user host control directory tree.
//!
//! This is not a generic sandbox filesystem security primitive. It checks the
//! final pathname, does not retain a directory descriptor and does not establish
//! an atomic no-follow ancestry or mount grant. Cloud recovery and publication
//! directories intentionally use their own, different policies.

use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::Path,
};

/// Existing account ancestors may be readable/searchable by other users, but
/// never writable by them. Product-private state directories must be mode 0700.
#[derive(Clone, Copy, Debug)]
pub enum HostStateDirectoryMode {
    AccountAncestor,
    Private,
}

/// Checks a host control directory against the real login UID.
/// Missing directories are created one component at a time only when requested.
/// Existing directories are never chmod'ed to conceal unsafe configuration.
///
/// # Errors
/// Returns the filesystem error or `PermissionDenied` for a non-directory,
/// symlink, foreign owner, writable ancestor or non-0700 private directory.
pub fn verify_host_state_directory(
    path: &Path,
    mode: HostStateDirectoryMode,
    create: bool,
) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new().mode(0o700).create(path)?;
            fs::symlink_metadata(path)?
        }
        Err(error) => return Err(error),
    };
    let permissions = metadata.mode() & 0o777;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || permissions & 0o022 != 0
        || (matches!(mode, HostStateDirectoryMode::Private) && permissions != 0o700)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe host state directory",
        ));
    }
    Ok(())
}

/// Name of the per-scope control directory: SHA-256 of the canonical selected
/// `MARSH_HOME`. Shared by `marshd` (which creates it) and host lifecycle
/// commands (which only read it).
#[must_use]
pub fn scope_control_leaf(canonical_selected_home: &Path) -> String {
    use sha2::{Digest as _, Sha256};
    format!(
        "{:x}",
        Sha256::digest(canonical_selected_home.as_os_str().as_encoded_bytes())
    )
}

/// Default host product root (`~/Library/Application Support/marsh` on macOS).
#[must_use]
pub fn host_product_root(account_home: &Path) -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    let components: &[&str] = &["Library", "Application Support", "marsh"];
    #[cfg(not(target_os = "macos"))]
    let components: &[&str] = &[".local", "state", "marsh"];
    components
        .iter()
        .fold(account_home.to_path_buf(), |path, part| path.join(part))
}

/// `MARSH_CONTROL_HOME`, when set to a non-empty value.
#[must_use]
pub fn control_override_from_env() -> Option<std::path::PathBuf> {
    std::env::var_os("MARSH_CONTROL_HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
}

/// The one host state root for product-private, non-guest state: the
/// `MARSH_CONTROL_HOME` override when set, else [`host_product_root`].
/// Per-scope daemon control directories (`control/<leaf>` by default,
/// `<override>/<leaf>` when overridden), MCP/ACP publications
/// (`published-mcp/`, `published-acp/`, `publication-locks/`) all live
/// beneath it. Every host-side write of such state must resolve through here.
#[must_use]
pub fn host_state_root(account_home: &Path, control_override: Option<&Path>) -> std::path::PathBuf {
    control_override.map_or_else(|| host_product_root(account_home), Path::to_path_buf)
}

/// [`host_state_root`] with the override taken from the environment.
#[must_use]
pub fn host_state_root_from_env(account_home: &Path) -> std::path::PathBuf {
    host_state_root(account_home, control_override_from_env().as_deref())
}

/// Base directory for generated MCP server scope roots. Each scope root holds
/// selected homes that are mounted into guests, so it must live *beside*, never
/// beneath, the protected state root: `<state-root>-mcp-scopes` (by default
/// `~/Library/Application Support/marsh-mcp-scopes`).
#[must_use]
pub fn mcp_scope_base(account_home: &Path, control_override: Option<&Path>) -> std::path::PathBuf {
    let root = host_state_root(account_home, control_override);
    let mut name = root
        .file_name()
        .map_or_else(|| "marsh".into(), std::ffi::OsStr::to_os_string);
    name.push("-mcp-scopes");
    root.with_file_name(name)
}

/// VM names still recorded in a scope's ownership map, read without creating,
/// repairing, or locking anything. A missing map or scope records nothing.
/// Used only when no daemon is running, to tell "nothing running" apart from
/// "daemon gone but VMs remain".
///
/// # Errors
/// Returns an error when the map exists but cannot be read or parsed.
pub fn recorded_scope_vms(
    account_home: &Path,
    control_override: Option<&Path>,
    selected_home: &Path,
) -> io::Result<Vec<String>> {
    let Ok(selected) = selected_home.canonicalize() else {
        return Ok(Vec::new());
    };
    let control_root = control_override.map_or_else(
        || host_product_root(account_home).join("control"),
        Path::to_path_buf,
    );
    let control_root = control_root.canonicalize().unwrap_or(control_root);
    let map = control_root
        .join(scope_control_leaf(&selected))
        .join("vm-ownership.json");
    let bytes = match fs::read(&map) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(value
        .get("vms")
        .and_then(serde_json::Value::as_object)
        .map(|vms| vms.keys().cloned().collect())
        .unwrap_or_default())
}
