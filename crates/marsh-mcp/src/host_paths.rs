//! Host path and permission policy: where the scope and home directories live,
//! and which directories and executables are trusted.

use super::{Component, Digest, MetadataExt, Path, PathBuf, PermissionsExt, Sha256, env, fs};

/// Resolve CLI defaults without starting the MCP transport.
///
/// # Errors
/// Returns an error when the current directory or executable path is not
/// available from the operating system.
pub fn default_paths() -> Result<(PathBuf, PathBuf, PathBuf), String> {
    let workspace =
        env::current_dir().map_err(|error| format!("cannot read current directory: {error}"))?;
    let home = default_selected_home()?;
    let current =
        env::current_exe().map_err(|error| format!("cannot locate marsh-mcp: {error}"))?;
    let sibling = current
        .parent()
        .ok_or_else(|| "marsh-mcp executable has no parent directory".to_string())?
        .join("marsh");
    Ok((workspace, home, sibling))
}

/// The selected home the `marsh` CLI uses: `MARSH_HOME`, else `~/.marsh`.
/// `marsh-mcp serve` without `--home`/`--scope-root` drives this same home.
///
/// # Errors
/// Returns an error when `HOME`/`MARSH_HOME` is missing or relative.
pub fn default_selected_home() -> Result<PathBuf, String> {
    if let Some(home) = env::var_os("MARSH_HOME").filter(|value| !value.is_empty()) {
        let home = PathBuf::from(home);
        if !home.is_absolute() {
            return Err("MARSH_HOME must be absolute".into());
        }
        return Ok(home);
    }
    let host_home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is required to select the default marsh home".to_string())?;
    if !host_home.is_absolute() {
        return Err("HOME must be absolute to select the default marsh home".into());
    }
    Ok(host_home.join(".marsh"))
}

/// Return the isolated generated-scope default home for one canonical workspace.
///
/// # Errors
/// Returns an error when `HOME` or the workspace cannot be made absolute.
pub fn default_home_for_workspace(workspace: &Path) -> Result<PathBuf, String> {
    Ok(generated_scope_root_for_workspace(workspace)?.join("default"))
}

/// Return the private root beneath which generated development scopes live.
///
/// # Errors
/// Returns an error when `HOME` or the workspace cannot be made absolute.
pub fn generated_scope_root_for_workspace(workspace: &Path) -> Result<PathBuf, String> {
    let workspace = workspace.canonicalize().map_err(|error| {
        format!(
            "cannot canonicalize workspace {}: {error}",
            workspace.display()
        )
    })?;
    let host_home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is required to select the default MCP home".to_string())?;
    if !host_home.is_absolute() {
        return Err("HOME must be absolute to select the default MCP home".into());
    }
    let key =
        marsh_contracts::process::hex(&Sha256::digest(workspace.as_os_str().as_encoded_bytes()));
    let base = if cfg!(target_os = "macos") {
        mcp_scope_base(&host_home)
    } else if let Some(xdg) = env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
        if !xdg.is_absolute() {
            return Err("XDG_DATA_HOME must be absolute to select MCP control state".into());
        }
        xdg.join(MCP_SCOPES_DIR)
    } else {
        host_home.join(".local/share").join(MCP_SCOPES_DIR)
    };
    Ok(base.join(key))
}

/// Directory name of the generated MCP scope-root base.
pub const MCP_SCOPES_DIR: &str = "marsh-mcp-scopes";

/// Generated MCP scope roots live beside (never beneath) the protected host
/// state root, and follow `MARSH_CONTROL_HOME` when it is set.
#[must_use]
pub fn mcp_scope_base(host_home: &Path) -> PathBuf {
    marsh_daemon::host_state_directory::mcp_scope_base(
        host_home,
        marsh_daemon::host_state_directory::control_override_from_env().as_deref(),
    )
}

pub(crate) fn guest_writable_marsh_home() -> Result<PathBuf, String> {
    let host_home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is required to validate MCP control state".to_string())?;
    let host_home = canonical_directory(&host_home, "host home")?;
    let candidate = host_home.join(".marsh");
    if candidate.exists() {
        canonical_directory(&candidate, "guest-writable marsh home")
    } else {
        Ok(candidate)
    }
}

pub(crate) fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize {label} {}: {error}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!(
            "{label} is not a directory: {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}

pub(crate) fn resolve_without_creation(path: &Path) -> Result<PathBuf, String> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => {
                normalized.push(component.as_os_str());
            }
            Component::Normal(segment) => normalized.push(segment),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(format!(
                        "cannot resolve development scope root outside the filesystem root: {}",
                        path.display()
                    ));
                }
            }
        }
    }
    if !normalized.is_absolute() {
        return Err("development scope root must be an absolute path".into());
    }

    let mut existing = normalized.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| format!("cannot resolve development scope root: {}", path.display()))?;
        missing.push(name.to_os_string());
        existing = existing
            .parent()
            .ok_or_else(|| format!("cannot resolve development scope root: {}", path.display()))?;
    }
    let mut resolved = existing.canonicalize().map_err(|error| {
        format!(
            "cannot resolve development scope root {} without creating it: {error}",
            path.display()
        )
    })?;
    for segment in missing.into_iter().rev() {
        resolved.push(segment);
    }
    Ok(resolved)
}

pub(crate) fn is_at_or_beneath(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

pub(crate) fn effective_username() -> Result<String, String> {
    let uid = nix::unistd::Uid::effective();
    let account = nix::unistd::User::from_uid(uid)
        .map_err(|error| format!("cannot resolve effective UID {uid}: {error}"))?
        .ok_or_else(|| format!("effective UID {uid} has no passwd account"))?;
    let username = account.name;
    if username.is_empty()
        || username.len() > 255
        || !username
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(format!(
            "passwd account for effective UID {uid} has an invalid marsh username"
        ));
    }
    Ok(username)
}

pub(crate) fn fixed_minimal_path() -> std::ffi::OsString {
    let mut entries = Vec::new();
    if let Some(home) = env::var_os("HOME").map(PathBuf::from)
        && home.is_absolute()
    {
        entries.push(home.join(".cargo/bin"));
    }
    entries.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ]);
    env::join_paths(entries).unwrap_or_else(|_| {
        std::ffi::OsString::from("/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin")
    })
}

pub(crate) fn canonical_file(path: &Path, label: &str) -> Result<PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize {label} {}: {error}", path.display()))?;
    if !canonical.is_file() {
        return Err(format!("{label} is not a file: {}", canonical.display()));
    }
    let metadata = canonical
        .metadata()
        .map_err(|error| format!("cannot inspect {label}: {error}"))?;
    validate_executable_metadata(&canonical, &metadata, label)?;
    validate_trusted_parent_chain(&canonical)?;
    Ok(canonical)
}

pub(crate) fn canonical_external_executable(path: &Path, label: &str) -> Result<PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize {label} {}: {error}", path.display()))?;
    let metadata = fs::symlink_metadata(&canonical)
        .map_err(|error| format!("cannot inspect {label}: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(format!(
            "{label} is not a regular file: {}",
            canonical.display()
        ));
    }
    validate_executable_metadata(&canonical, &metadata, label)?;
    Ok(canonical)
}

pub(crate) fn validate_executable_metadata(
    path: &Path,
    metadata: &fs::Metadata,
    label: &str,
) -> Result<(), String> {
    let mode = metadata.permissions().mode();
    if mode & 0o111 == 0 {
        return Err(format!("{label} is not executable: {}", path.display()));
    }
    let current_uid = rustix::process::geteuid().as_raw();
    if metadata.uid() != 0 && metadata.uid() != current_uid {
        return Err(format!(
            "{label} must be owned by root or the current user: {}",
            path.display()
        ));
    }
    if mode & 0o022 != 0 {
        return Err(format!(
            "{label} must not be writable by group or other users: {}",
            path.display()
        ));
    }
    Ok(())
}

pub(crate) fn validate_trusted_parent_chain(path: &Path) -> Result<(), String> {
    let current_uid = rustix::process::geteuid().as_raw();
    for directory in path.ancestors().skip(1) {
        let metadata = fs::symlink_metadata(directory).map_err(|error| {
            format!(
                "cannot inspect executable parent directory {}: {error}",
                directory.display()
            )
        })?;
        if !metadata.file_type().is_dir() {
            return Err(format!(
                "executable parent is not a directory: {}",
                directory.display()
            ));
        }
        if metadata.uid() != 0 && metadata.uid() != current_uid {
            return Err(format!(
                "executable parent must be owned by root or the current user: {}",
                directory.display()
            ));
        }
        let mode = metadata.permissions().mode();
        if mode & 0o022 != 0
            && !is_allowed_platform_temp_root(directory, mode)
            && !is_admin_group_writable(mode, metadata.gid())
        {
            return Err(format!(
                "executable parent must not be writable by group or other users: {}",
                directory.display()
            ));
        }
    }
    Ok(())
}

/// Stock Homebrew creates `/opt/homebrew/Cellar` as `0775 <user>:admin`.
/// Accept group write only when the group is the macOS `admin` group (gid 80,
/// whose members are already administrators) and the directory is not
/// world-writable. The caller has already required root or the current user
/// as owner. Any other group, and any other-writable directory, is rejected.
pub(crate) fn is_admin_group_writable(mode: u32, gid: u32) -> bool {
    const MACOS_ADMIN_GID: u32 = 80;
    cfg!(target_os = "macos") && mode & 0o002 == 0 && gid == MACOS_ADMIN_GID
}

pub(crate) fn is_allowed_platform_temp_root(path: &Path, mode: u32) -> bool {
    if mode & 0o1000 == 0 {
        return false;
    }
    [PathBuf::from("/tmp"), PathBuf::from("/private/tmp")]
        .into_iter()
        .filter_map(|candidate| candidate.canonicalize().ok())
        .any(|candidate| candidate == path)
}

/// `path` or its nearest ancestor that is a `*-mcp-scopes` product directory
/// (`marsh-mcp-scopes`, or `<state root>-mcp-scopes` under `MARSH_CONTROL_HOME`):
/// the owner-only directory `prepare_home` checks above every generated scope
/// root, including the per-client roots (`clients/sbx/<hash>`).
pub(crate) fn mcp_product_root(path: &Path) -> Option<&Path> {
    path.ancestors().find(|ancestor| {
        ancestor
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with("-mcp-scopes"))
    })
}
