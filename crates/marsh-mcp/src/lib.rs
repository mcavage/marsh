//! Host-side MCP adapter for a single, explicitly scoped marsh workspace.
//!
//! The adapter exposes typed marsh product operations. It intentionally has no
//! generic host-process, SBX, Docker, environment, or filesystem passthrough.

pub mod acp_export;
pub mod broker;
pub mod export;

pub use acp_export::{AcpDeclaration, AcpExportMcp};

pub use export::{
    ArgBinding, ExecutionOutcome, ExportConfig, ExportExecutionData, ExportMcp, StdinBinding,
    ToolBindings, ToolDeclaration, WorkspaceIdentity,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rmcp::{
    ServerHandler,
    handler::server::wrapper::{Json, Parameters},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env, fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::{
        Mutex, OwnedMutexGuard, OwnedRwLockWriteGuard, OwnedSemaphorePermit, RwLock, Semaphore,
        watch,
    },
};
use uuid::Uuid;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 900_000;
const MAX_CAPTURE_BYTES: usize = 262_144;
const MAX_COMMAND_BYTES: usize = 65_536;
const MAX_OPERATIONS: usize = 4;
const MAX_RETAINED_OPERATIONS: usize = 128;
const MAX_DEVELOPMENT_SCOPES: usize = 16;
const MAX_RETAINED_SCOPES: usize = 128;
const MAX_SCOPE_REMOVAL_ENTRIES: usize = 100_000;
const MAX_SCOPE_REMOVAL_DEPTH: usize = 64;
const SCOPE_LIFECYCLE_TIMEOUT: Duration = Duration::from_mins(10);
const MAX_WORKSPACE_BINDING_BYTES: u64 = 16_384;
pub(crate) const RELAY_ENV: [&str; 5] = [
    "MARSH_DAEMON_SOCKET",
    "MARSH_DAEMON_TOKEN",
    "MARSH_SESSION_ID",
    "MARSH_HOME_BACKING",
    "MARSH_RELAY_TOKEN",
];
pub(crate) const INHERITED_ENV: [&str; 12] = [
    "HOME",
    // The host state root: every host-side write follows the caller's
    // override, including the daemon this server starts.
    "MARSH_CONTROL_HOME",
    "TMPDIR",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "COLORTERM",
    "NO_COLOR",
    "CARGO_HOME",
    "RUSTUP_HOME",
];

/// Validated, immutable host MCP scope.
#[derive(Clone, Debug)]
pub struct HostConfig {
    username: String,
    workspace: PathBuf,
    home: PathBuf,
    home_identity: DirectoryIdentity,
    generated_scope_root: Option<PathBuf>,
    scope_root_identity: Option<DirectoryIdentity>,
    _scope_root_lease: Option<Arc<ScopeRootLease>>,
    marsh: PathBuf,
    marshd: PathBuf,
    sbx: PathBuf,
    make: PathBuf,
    marsh_identity: ExecutableIdentity,
    marshd_identity: ExecutableIdentity,
    sbx_identity: ExecutableIdentity,
    full_sbx_control_enabled: bool,
    persisted_scopes: BTreeMap<String, DevelopmentScope>,
    preserved_registry_records: BTreeMap<String, Value>,
    registry_diagnostics: Vec<String>,
}

#[derive(Debug, Default)]
struct LoadedScopeRegistry {
    scopes: BTreeMap<String, DevelopmentScope>,
    preserved_records: BTreeMap<String, Value>,
    diagnostics: Vec<String>,
}

#[derive(Debug)]
struct ScopeRootLease {
    file: fs::File,
}

impl Drop for ScopeRootLease {
    fn drop(&mut self) {
        // Closing the descriptor also releases flock, but perform the unlock
        // explicitly so an immediate successor never depends on close timing
        // or platform-specific descriptor lifetime details.
        let _ = rustix::fs::flock(&self.file, rustix::fs::FlockOperation::Unlock);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExecutableIdentity {
    device: u64,
    inode: u64,
    uid: u32,
    gid: u32,
    mode: u32,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    sha256: [u8; 32],
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
    uid: u32,
    mode: u32,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceBinding {
    version: u32,
    canonical_workspace: PathBuf,
    device: u64,
    inode: u64,
}

impl HostConfig {
    /// Build a scope from explicit paths. The workspace and executable must
    /// exist. The selected home is created owner-only when absent and is
    /// rejected if it is not owned by this user or accessible by group/other.
    ///
    /// # Errors
    /// Returns an error when a path is relative, missing, has the wrong type,
    /// is not safely owned, or has unsafe permissions.
    pub fn new(
        workspace: &Path,
        home: &Path,
        marsh: &Path,
        sbx: &Path,
        full_sbx_control_enabled: bool,
    ) -> Result<Self, String> {
        for (label, path) in [
            ("workspace", workspace),
            ("home", home),
            ("marsh executable", marsh),
            ("sbx executable", sbx),
        ] {
            if !path.is_absolute() {
                return Err(format!("{label} must be an absolute path"));
            }
        }
        Self::new_inner(workspace, home, None, marsh, sbx, full_sbx_control_enabled)
    }

    /// Build a multi-scope server rooted at one private development directory.
    ///
    /// # Errors
    /// Returns an error when paths overlap, permissions are unsafe, executable
    /// trust anchors change, or the persisted scope registry is invalid.
    pub fn new_with_scope_root(
        workspace: &Path,
        home: &Path,
        scope_root: &Path,
        marsh: &Path,
        sbx: &Path,
        full_sbx_control_enabled: bool,
    ) -> Result<Self, String> {
        Self::new_inner(
            workspace,
            home,
            Some(scope_root),
            marsh,
            sbx,
            full_sbx_control_enabled,
        )
    }

    #[allow(clippy::too_many_lines)]
    fn new_inner(
        workspace: &Path,
        home: &Path,
        scope_root: Option<&Path>,
        marsh: &Path,
        sbx: &Path,
        full_sbx_control_enabled: bool,
    ) -> Result<Self, String> {
        let username = effective_username()?;
        for (label, path) in [
            ("workspace", workspace),
            ("home", home),
            ("marsh executable", marsh),
            ("sbx executable", sbx),
        ] {
            if !path.is_absolute() {
                return Err(format!("{label} must be an absolute path"));
            }
        }
        let workspace = canonical_directory(workspace, "workspace")?;
        if let Some(scope_root) = scope_root {
            if !scope_root.is_absolute() {
                return Err("development scope root must be an absolute path".into());
            }
            let unresolved_root = resolve_without_creation(scope_root)?;
            if is_at_or_beneath(&unresolved_root, &workspace)
                || is_at_or_beneath(&workspace, &unresolved_root)
            {
                return Err("development scope root and workspace must not overlap".into());
            }
            let guest_home = guest_writable_marsh_home()?;
            if is_at_or_beneath(&unresolved_root, &guest_home) {
                return Err(format!(
                    "development scope root must be outside the guest-writable marsh home: {}",
                    guest_home.display()
                ));
            }
        }
        let generated_scope_root = scope_root.map(prepare_scope_root).transpose()?;
        let scope_root_identity = generated_scope_root
            .as_ref()
            .map(|root| DirectoryIdentity::capture(root))
            .transpose()?;
        if let Some(root) = &generated_scope_root {
            let guest_home = guest_writable_marsh_home()?;
            if is_at_or_beneath(root, &guest_home) {
                return Err(format!(
                    "development scope root must be outside the guest-writable marsh home: {}",
                    guest_home.display()
                ));
            }
        }
        if let Some(root) = &generated_scope_root
            && (is_at_or_beneath(root, &workspace) || is_at_or_beneath(&workspace, root))
        {
            return Err("development scope root and workspace must not overlap".into());
        }
        let scope_root_lease = generated_scope_root
            .as_ref()
            .map(|root| acquire_scope_root_lease(root))
            .transpose()?;
        if let Some(root) = &generated_scope_root {
            establish_workspace_binding(root, &workspace)?;
            reap_stale_scope_registry_temps(root)?;
        }
        let mut loaded_registry = generated_scope_root
            .as_ref()
            .map(|root| load_scope_registry(root))
            .transpose()?
            .unwrap_or_default();
        let home = if generated_scope_root.is_some() {
            resolve_without_creation(home)?
        } else {
            home.to_path_buf()
        };
        if home.starts_with(&workspace) {
            return Err(format!(
                "MCP home must be outside the workspace: {}",
                workspace.display()
            ));
        }
        if let Some(root) = &generated_scope_root
            && (home.parent() != Some(root)
                || home.file_name().and_then(|name| name.to_str()) != Some("default"))
        {
            return Err("multi-scope default home must be <scope-root>/default".into());
        }
        let persisted_default = loaded_registry.scopes.get("default");
        let (home, home_identity) = if let Some(scope) = persisted_default {
            // Registry reconciliation already verified the exact persisted
            // identity or isolated the record as Failed. Never recreate or
            // recapture a persisted default home during restart.
            (home.clone(), scope.home_identity.clone())
        } else {
            // Resolve an existing home before checking its ownership/mode.
            // This catches both an ancestor home and a symlink to an ancestor.
            if home.exists() {
                let resolved_home = canonical_directory(&home, "home")?;
                if is_at_or_beneath(&resolved_home, &workspace)
                    || is_at_or_beneath(&workspace, &resolved_home)
                {
                    return Err(format!(
                        "MCP home and workspace must not overlap in either direction: home={}, workspace={}",
                        resolved_home.display(),
                        workspace.display()
                    ));
                }
            }
            let home = prepare_home(&home)?;
            let identity = DirectoryIdentity::capture(&home)?;
            (home, identity)
        };
        if is_at_or_beneath(&home, &workspace) || is_at_or_beneath(&workspace, &home) {
            return Err(format!(
                "MCP home and workspace must not overlap in either direction: home={}, workspace={}",
                home.display(),
                workspace.display()
            ));
        }
        let marsh = canonical_file(marsh, "marsh executable")?;
        let marshd_candidate = marsh
            .parent()
            .ok_or_else(|| "marsh executable has no parent directory".to_string())?
            .join("marshd");
        let marshd = canonical_file(&marshd_candidate, "sibling marshd executable")?;
        let sbx = canonical_external_executable(sbx, "sbx executable")?;
        for (label, executable) in [
            ("marsh", &marsh),
            ("sibling marshd", &marshd),
            ("sbx", &sbx),
        ] {
            if is_at_or_beneath(executable, &workspace) || is_at_or_beneath(executable, &home) {
                return Err(format!(
                    "{label} executable must be installed outside the workspace and selected home; refusing {}",
                    executable.display()
                ));
            }
            if generated_scope_root
                .as_ref()
                .is_some_and(|root| is_at_or_beneath(executable, root))
            {
                return Err(format!(
                    "{label} executable must be installed outside the development scope root; refusing {}",
                    executable.display()
                ));
            }
        }
        let marsh_identity = ExecutableIdentity::capture(&marsh)?;
        let daemon_identity = ExecutableIdentity::capture(&marshd)?;
        let sbx_identity = ExecutableIdentity::capture_external(&sbx)?;
        let make = canonical_file(Path::new("/usr/bin/make"), "make executable")?;
        if let Some(root) = &generated_scope_root
            && !loaded_registry.scopes.contains_key("default")
        {
            loaded_registry.scopes.insert(
                "default".to_owned(),
                DevelopmentScope {
                    home: home.clone(),
                    home_identity: home_identity.clone(),
                    state: ScopeState::Ready,
                    created_unix_ms: now_ms(),
                    last_operation_id: None,
                    diagnostic: None,
                },
            );
            persist_scope_registry(
                root,
                &loaded_registry.scopes,
                &loaded_registry.preserved_records,
            )?;
        }
        Ok(Self {
            username,
            workspace,
            home,
            home_identity,
            generated_scope_root,
            scope_root_identity,
            _scope_root_lease: scope_root_lease,
            marsh,
            marshd,
            sbx,
            make,
            marsh_identity,
            marshd_identity: daemon_identity,
            sbx_identity,
            full_sbx_control_enabled,
            persisted_scopes: loaded_registry.scopes,
            preserved_registry_records: loaded_registry.preserved_records,
            registry_diagnostics: loaded_registry.diagnostics,
        })
    }

    /// Canonical workspace path.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// Canonical selected-home path.
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Canonical marsh executable path.
    #[must_use]
    pub fn marsh(&self) -> &Path {
        &self.marsh
    }

    /// Canonical sibling marshd executable path.
    #[must_use]
    pub fn marshd(&self) -> &Path {
        &self.marshd
    }

    /// Canonical sbx executable path.
    #[must_use]
    pub fn sbx(&self) -> &Path {
        &self.sbx
    }

    /// Resolved username.
    #[must_use]
    pub fn username(&self) -> &str {
        &self.username
    }

    /// Revalidate the configured host executables.
    ///
    /// # Errors
    /// Returns an error if any executable has been modified, moved, or replaced.
    pub fn validate_host_executables(&self) -> Result<(), String> {
        for (label, path, expected) in [
            ("marsh", &self.marsh, &self.marsh_identity),
            ("sibling marshd", &self.marshd, &self.marshd_identity),
        ] {
            let current = ExecutableIdentity::capture(path)?;
            if &current != expected {
                return Err(format!(
                    "configured {label} executable changed after MCP startup; restart marsh-mcp after reinstalling {}",
                    path.display()
                ));
            }
        }
        let current_sbx = ExecutableIdentity::capture_external(&self.sbx)?;
        if current_sbx != self.sbx_identity {
            return Err(format!(
                "configured sbx executable changed after MCP startup; restart marsh-mcp after reinstalling {}",
                self.sbx.display()
            ));
        }
        Ok(())
    }

    fn validate_scope_root(&self) -> Result<(), String> {
        match (&self.generated_scope_root, &self.scope_root_identity) {
            (Some(root), Some(expected)) if &DirectoryIdentity::capture(root)? == expected => {
                Ok(())
            }
            (None, None) => Ok(()),
            (Some(root), Some(_)) => Err(format!(
                "development scope root changed after MCP startup: {}",
                root.display()
            )),
            _ => Err("development scope root identity is inconsistent".into()),
        }
    }

    fn persist_scopes(&self, scopes: &BTreeMap<String, DevelopmentScope>) -> Result<(), String> {
        let root = self.generated_scope_root.as_ref().ok_or_else(|| {
            "managed scope registry is unavailable in legacy --home mode".to_owned()
        })?;
        persist_scope_registry(root, scopes, &self.preserved_registry_records)
    }
}

impl ExecutableIdentity {
    fn capture(path: &Path) -> Result<Self, String> {
        Self::capture_with_parent_policy(path, true)
    }

    fn capture_external(path: &Path) -> Result<Self, String> {
        Self::capture_with_parent_policy(path, false)
    }

    fn capture_with_parent_policy(
        path: &Path,
        require_trusted_parents: bool,
    ) -> Result<Self, String> {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("cannot inspect executable identity: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err(format!(
                "configured executable is no longer a regular file: {}",
                path.display()
            ));
        }
        validate_executable_metadata(path, &metadata, "configured executable")?;
        if require_trusted_parents {
            validate_trusted_parent_chain(path)?;
        }
        let mut file = fs::File::open(path)
            .map_err(|error| format!("cannot open executable for verification: {error}"))?;
        let mut digest = Sha256::new();
        let mut buffer = vec![0_u8; 65_536].into_boxed_slice();
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|error| format!("cannot hash executable: {error}"))?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            mode: metadata.mode(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
            sha256: digest.finalize().into(),
        })
    }
}

impl DirectoryIdentity {
    fn capture(path: &Path) -> Result<Self, String> {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("cannot inspect managed owner-only directory: {error}"))?;
        if !metadata.file_type().is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(format!(
                "managed directory must remain owner-only and must not be a symlink: {}",
                path.display()
            ));
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            uid: metadata.uid(),
            mode: metadata.mode(),
        })
    }
}

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
    let key = format!(
        "{:x}",
        Sha256::digest(workspace.as_os_str().as_encoded_bytes())
    );
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

fn guest_writable_marsh_home() -> Result<PathBuf, String> {
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

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct ToolResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl ToolResponse {
    fn success(data: Value) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    fn failure(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(error.into()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResultGetRequest {
    /// Durable result cursor, full job UUID, or unique UUID prefix.
    selector: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SelectionRequest {
    /// `all` or a comma-separated list of lowercase Kit names.
    #[serde(default = "default_selection")]
    selection: String,
}

fn default_selection() -> String {
    "all".into()
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShellRunRequest {
    /// Shell program evaluated by marsh inside the project shell VM.
    command: String,
    /// Wall-clock limit in milliseconds (10..=900000).
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QualifyRequest {
    /// Fixed qualification gate: `source`, `smoke`, `full`, or `perf`.
    gate: String,
    /// Wall-clock limit in milliseconds (10..=900000).
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OperationRequest {
    /// Operation identifier returned by a mutating or long-running tool.
    operation_id: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeRequest {
    /// Opaque scope identifier returned by `scope_start`, or `default`.
    scope_id: String,
}

#[derive(Clone, Debug, Default, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct EmptyRequest {}

impl<'de> Deserialize<'de> for EmptyRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct EmptyVisitor;

        impl<'de> serde::de::Visitor<'de> for EmptyVisitor {
            type Value = EmptyRequest;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an empty object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                if map.next_key::<serde::de::IgnoredAny>()?.is_some() {
                    return Err(serde::de::Error::custom("this tool accepts no properties"));
                }
                Ok(EmptyRequest {})
            }
        }

        deserializer.deserialize_map(EmptyVisitor)
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeRunRequest {
    /// Opaque scope identifier returned by `scope_start`, or `default`.
    scope_id: String,
    /// Shell program evaluated by marsh inside this scope's project shell VM.
    command: String,
    /// Wall-clock limit in milliseconds (10..=900000).
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeResultGetRequest {
    /// Opaque scope identifier returned by `scope_start`, or `default`.
    scope_id: String,
    /// Durable result cursor, full job UUID, or unique UUID prefix.
    selector: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeSelectionRequest {
    /// Opaque scope identifier returned by `scope_start`, or `default`.
    scope_id: String,
    /// `all` or a comma-separated list of lowercase Kit names.
    #[serde(default = "default_selection")]
    selection: String,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
struct OperationView {
    operation_id: String,
    scope_id: String,
    kind: String,
    state: OperationState,
    started_unix_ms: u128,
    finished_unix_ms: Option<u128>,
    exit_code: Option<i32>,
    stdout_bytes: usize,
    stderr_bytes: usize,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum OperationState {
    Queued,
    Running,
    Succeeded,
    Failed,
    CancellationUncertain,
}

#[derive(Debug)]
struct Operation {
    id: String,
    owner_session: String,
    scope_id: String,
    kind: String,
    state: OperationState,
    started_unix_ms: u128,
    finished_unix_ms: Option<u128>,
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
    cancel: Option<watch::Sender<bool>>,
}

impl Operation {
    fn view(&self) -> OperationView {
        OperationView {
            operation_id: self.id.clone(),
            scope_id: self.scope_id.clone(),
            kind: self.kind.clone(),
            state: self.state,
            started_unix_ms: self.started_unix_ms,
            finished_unix_ms: self.finished_unix_ms,
            exit_code: self.exit_code,
            stdout_bytes: self.stdout.len(),
            stderr_bytes: self.stderr.len(),
            stdout_truncated: self.stdout_truncated,
            stderr_truncated: self.stderr_truncated,
        }
    }
}

#[derive(Clone, Debug)]
struct CommandSpec {
    executable: PathBuf,
    arguments: Vec<String>,
    timeout: Duration,
    home: PathBuf,
    home_identity: DirectoryIdentity,
}

impl CommandSpec {
    fn validate_home(&self) -> Result<(), String> {
        let current = DirectoryIdentity::capture(&self.home)?;
        if current != self.home_identity {
            return Err(format!(
                "development scope home changed before operation start: {}",
                self.home.display()
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ScopeState {
    Starting,
    Ready,
    Resetting,
    Stopping,
    Removing,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DevelopmentScope {
    #[serde(skip)]
    home: PathBuf,
    #[serde(default)]
    home_identity: DirectoryIdentity,
    state: ScopeState,
    created_unix_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    diagnostic: Option<String>,
}

impl DevelopmentScope {
    fn validate_home(&self) -> Result<(), String> {
        let current = DirectoryIdentity::capture(&self.home)?;
        if current != self.home_identity {
            return Err(format!(
                "development scope home changed after MCP startup: {}",
                self.home.display()
            ));
        }
        Ok(())
    }
}

/// Host-side MCP tool server.
#[derive(Clone, Debug)]
pub struct HostMcp {
    config: Arc<HostConfig>,
    session_id: String,
    operations: Arc<Mutex<BTreeMap<String, Operation>>>,
    permits: Arc<Semaphore>,
    lifecycles: Arc<Mutex<BTreeMap<String, Arc<Mutex<()>>>>>,
    scope_access: Arc<Mutex<BTreeMap<String, Arc<RwLock<()>>>>>,
    scopes: Arc<Mutex<BTreeMap<String, DevelopmentScope>>>,
}

impl HostMcp {
    /// Create a scoped MCP server.
    #[must_use]
    pub fn new(config: HostConfig) -> Self {
        let mut scopes = config.persisted_scopes.clone();
        scopes
            .entry("default".to_owned())
            .or_insert_with(|| DevelopmentScope {
                home: config.home.clone(),
                home_identity: config.home_identity.clone(),
                state: ScopeState::Ready,
                created_unix_ms: now_ms(),
                last_operation_id: None,
                diagnostic: None,
            });
        Self {
            config: Arc::new(config),
            session_id: Uuid::new_v4().to_string(),
            operations: Arc::new(Mutex::new(BTreeMap::new())),
            permits: Arc::new(Semaphore::new(MAX_OPERATIONS)),
            lifecycles: Arc::new(Mutex::new(BTreeMap::new())),
            scope_access: Arc::new(Mutex::new(BTreeMap::new())),
            scopes: Arc::new(Mutex::new(scopes)),
        }
    }

    /// Create a transport-local view over the same host state.
    ///
    /// Operations created through the returned handler are visible and
    /// cancellable only through that handler. Shared scope and capacity state
    /// remains process-wide.
    #[must_use]
    pub fn new_session(&self) -> Self {
        Self {
            config: Arc::clone(&self.config),
            session_id: Uuid::new_v4().to_string(),
            operations: Arc::clone(&self.operations),
            permits: Arc::clone(&self.permits),
            lifecycles: Arc::clone(&self.lifecycles),
            scope_access: Arc::clone(&self.scope_access),
            scopes: Arc::clone(&self.scopes),
        }
    }

    /// Cancel only work started by this transport session.
    ///
    /// # Errors
    /// Returns an error if session-owned cleanup does not become terminal in
    /// the bounded shutdown interval.
    pub async fn shutdown_session(&self) -> Result<(), String> {
        {
            let operations = self.operations.lock().await;
            for operation in operations.values().filter(|operation| {
                operation.owner_session == self.session_id
                    && matches!(
                        operation.state,
                        OperationState::Queued | OperationState::Running
                    )
            }) {
                let _ = operation.cancel.as_ref().map(|cancel| cancel.send(true));
            }
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let complete = self.operations.lock().await.values().all(|operation| {
                operation.owner_session != self.session_id
                    || matches!(
                        operation.state,
                        OperationState::Succeeded
                            | OperationState::Failed
                            | OperationState::CancellationUncertain
                    )
            });
            if complete {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("MCP session shutdown timed out waiting for operation cleanup".into());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Request cancellation for every queued/running operation and wait for
    /// their operation tasks to record a terminal state.
    ///
    /// # Errors
    /// Returns an error if operation cleanup does not reach a terminal state
    /// within the bounded shutdown interval.
    pub async fn shutdown_all(&self) -> Result<(), String> {
        {
            let operations = self.operations.lock().await;
            for operation in operations.values() {
                if matches!(
                    operation.state,
                    OperationState::Queued | OperationState::Running
                ) {
                    let _ = operation.cancel.as_ref().map(|cancel| cancel.send(true));
                }
            }
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let complete = self.operations.lock().await.values().all(|operation| {
                matches!(
                    operation.state,
                    OperationState::Succeeded
                        | OperationState::Failed
                        | OperationState::CancellationUncertain
                )
            });
            if complete {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("MCP shutdown timed out waiting for operation cleanup".into());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn run_json_in(
        &self,
        scope_id: &str,
        scope: &DevelopmentScope,
        arguments: &[&str],
        timeout: Duration,
    ) -> ToolResponse {
        if let Err(error) = self.config.validate_scope_root() {
            return ToolResponse::failure(error);
        }
        let access = Arc::clone(
            self.scope_access
                .lock()
                .await
                .entry(scope_id.to_owned())
                .or_insert_with(|| Arc::new(RwLock::new(()))),
        );
        let Ok(_admission) = access.try_read_owned() else {
            return ToolResponse::failure(
                "this development scope is changing lifecycle; retry after it completes",
            );
        };
        let Ok(_permit) = self.permits.try_acquire() else {
            return ToolResponse::failure(
                "MCP operation capacity is busy; retry after an active operation completes",
            );
        };
        let spec = CommandSpec {
            executable: self.config.marsh.clone(),
            arguments: arguments.iter().map(ToString::to_string).collect(),
            timeout,
            home: scope.home.clone(),
            home_identity: scope.home_identity.clone(),
        };
        match run_bounded(self.config.as_ref(), &spec).await {
            Ok(output) if output.exit_code == Some(0) => {
                match serde_json::from_slice(&output.stdout) {
                    Ok(value) => ToolResponse::success(value),
                    Err(error) => {
                        ToolResponse::failure(format!("marsh returned invalid JSON: {error}"))
                    }
                }
            }
            Ok(output) => ToolResponse::failure(command_failure("marsh", &output)),
            Err(error) => ToolResponse::failure(error),
        }
    }

    async fn run_stopped_results_in(
        &self,
        scope_id: &str,
        scope: &DevelopmentScope,
        arguments: &[&str],
    ) -> ToolResponse {
        if let Err(error) = self.config.validate_scope_root() {
            return ToolResponse::failure(error);
        }
        let access = Arc::clone(
            self.scope_access
                .lock()
                .await
                .entry(scope_id.to_owned())
                .or_insert_with(|| Arc::new(RwLock::new(()))),
        );
        let Ok(_admission) = access.try_write_owned() else {
            return ToolResponse::failure(
                "this stopped development scope is busy; retry after its active read or lifecycle operation completes",
            );
        };
        {
            let scopes = self.scopes.lock().await;
            let Some(current) = scopes.get(scope_id) else {
                return ToolResponse::failure("unknown development scope");
            };
            if current.state != ScopeState::Stopped
                || current.home != scope.home
                || current.home_identity != scope.home_identity
            {
                return ToolResponse::failure(
                    "development scope changed after receipt-read admission; retry against its current lifecycle state",
                );
            }
            if let Err(error) = current.validate_home() {
                return ToolResponse::failure(error);
            }
        }
        let Ok(_permit) = self.permits.try_acquire() else {
            return ToolResponse::failure(
                "MCP operation capacity is busy; retry after an active operation completes",
            );
        };
        let read = CommandSpec {
            executable: self.config.marsh.clone(),
            arguments: arguments.iter().map(ToString::to_string).collect(),
            timeout: Duration::from_secs(20),
            home: scope.home.clone(),
            home_identity: scope.home_identity.clone(),
        };
        let stop = CommandSpec {
            executable: self.config.marsh.clone(),
            arguments: vec!["stop".into(), "--json".into()],
            timeout: SCOPE_LIFECYCLE_TIMEOUT,
            home: scope.home.clone(),
            home_identity: scope.home_identity.clone(),
        };
        let read_result = run_bounded(self.config.as_ref(), &read).await;
        let stop_result = run_bounded(self.config.as_ref(), &stop).await;
        if !stop_result
            .as_ref()
            .is_ok_and(|output| output.exit_code == Some(0))
        {
            let diagnostic = match &stop_result {
                Ok(output) => command_failure("marsh stop", output),
                Err(error) => error.clone(),
            };
            let mut scopes = self.scopes.lock().await;
            if let Some(registered) = scopes.get_mut(scope_id) {
                registered.state = ScopeState::Failed;
                registered.diagnostic = Some(bounded_diagnostic(&format!(
                    "stopped receipt read could not prove daemon shutdown: {diagnostic}"
                )));
            }
            let persistence = self.config.persist_scopes(&scopes);
            return ToolResponse::failure(match persistence {
                Ok(()) => format!(
                    "stopped receipt read could not prove daemon shutdown; scope marked failed: {diagnostic}"
                ),
                Err(error) => format!(
                    "stopped receipt read could not prove daemon shutdown ({diagnostic}) and failed to persist the failed state: {error}"
                ),
            });
        }
        match read_result {
            Ok(output) if output.exit_code == Some(0) => {
                match serde_json::from_slice(&output.stdout) {
                    Ok(value) => ToolResponse::success(value),
                    Err(error) => {
                        ToolResponse::failure(format!("marsh returned invalid JSON: {error}"))
                    }
                }
            }
            Ok(output) => ToolResponse::failure(command_failure("marsh", &output)),
            Err(error) => ToolResponse::failure(error),
        }
    }

    async fn run_json(&self, arguments: &[&str], timeout: Duration) -> ToolResponse {
        let scope = match self.active_scope("default").await {
            Ok(scope) => scope,
            Err(error) => return ToolResponse::failure(error),
        };
        self.run_json_in("default", &scope, arguments, timeout)
            .await
    }

    async fn readable_results_scope(&self, scope_id: &str) -> Result<DevelopmentScope, String> {
        self.config.validate_scope_root()?;
        validate_scope_id(scope_id)?;
        let scopes = self.scopes.lock().await;
        let scope = scopes
            .get(scope_id)
            .ok_or_else(|| "unknown development scope".to_owned())?;
        if !matches!(scope.state, ScopeState::Ready | ScopeState::Stopped) {
            return Err(format!(
                "development scope results are unavailable while state={}",
                serde_json::to_value(scope.state)
                    .unwrap_or(Value::String("unknown".into()))
                    .as_str()
                    .unwrap_or("unknown")
            ));
        }
        scope.validate_home()?;
        Ok(scope.clone())
    }

    async fn active_scope(&self, scope_id: &str) -> Result<DevelopmentScope, String> {
        self.config.validate_scope_root()?;
        validate_scope_id(scope_id)?;
        let scopes = self.scopes.lock().await;
        let scope = scopes
            .get(scope_id)
            .ok_or_else(|| "unknown development scope".to_owned())?;
        if scope.state != ScopeState::Ready {
            return Err(format!(
                "development scope is not ready (state={})",
                serde_json::to_value(scope.state)
                    .unwrap_or(Value::String("unknown".into()))
                    .as_str()
                    .unwrap_or("unknown")
            ));
        }
        if let Err(error) = scope.validate_home() {
            return Err(format!(
                "development scope home identity cannot be verified; automatic reset and removal are refused without claiming the possibly live runtime is absent. Restore the exact original directory entry, or stop marsh-mcp, independently prove and remove the exact runtime with stock SBX, archive this development-control root, and restart with a new empty --scope-root. No replacement path was modified: {error}"
            ));
        }
        Ok(scope.clone())
    }

    async fn resettable_scope(&self, scope_id: &str) -> Result<DevelopmentScope, String> {
        self.config.validate_scope_root()?;
        validate_scope_id(scope_id)?;
        let scopes = self.scopes.lock().await;
        let scope = scopes
            .get(scope_id)
            .ok_or_else(|| "unknown development scope".to_owned())?;
        if matches!(
            scope.state,
            ScopeState::Starting
                | ScopeState::Resetting
                | ScopeState::Stopping
                | ScopeState::Removing
        ) {
            return Err("development scope already has a lifecycle transition in progress".into());
        }
        if let Err(error) = scope.validate_home() {
            return Err(format!(
                "development scope home identity cannot be verified; automatic reset and removal are refused without claiming the possibly live runtime is absent. Restore the exact original directory entry, or stop marsh-mcp, independently prove and remove the exact runtime with stock SBX, archive this development-control root, and restart with a new empty --scope-root. No replacement path was modified: {error}"
            ));
        }
        Ok(scope.clone())
    }

    #[allow(clippy::too_many_lines)]
    async fn start_operation(
        &self,
        scope_id: &str,
        kind: &str,
        spec: CommandSpec,
        followups: Vec<CommandSpec>,
        exclusive_lifecycle: bool,
    ) -> ToolResponse {
        if let Err(error) = self.config.validate_scope_root() {
            return ToolResponse::failure(error);
        }
        let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() else {
            return ToolResponse::failure(
                "MCP operation capacity is busy; retry after an active operation completes",
            );
        };
        let lifecycle = Arc::clone(
            self.lifecycles
                .lock()
                .await
                .entry(scope_id.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        );
        let Ok(admission) = lifecycle.try_lock_owned() else {
            return ToolResponse::failure(
                "this development scope is busy; retry after its active lifecycle operation completes",
            );
        };
        let scope_access_guard = if matches!(kind, "scope_reset" | "scope_stop") {
            let access = Arc::clone(
                self.scope_access
                    .lock()
                    .await
                    .entry(scope_id.to_owned())
                    .or_insert_with(|| Arc::new(RwLock::new(()))),
            );
            let Ok(guard) = access.try_write_owned() else {
                return ToolResponse::failure(
                    "development scope has an active read and cannot change lifecycle",
                );
            };
            Some(guard)
        } else {
            None
        };
        let id = Uuid::new_v4().to_string();
        let previous_scope = {
            let mut scopes = self.scopes.lock().await;
            let Some(previous_scope) = scopes.get(scope_id).cloned() else {
                return ToolResponse::failure("unknown development scope");
            };
            if let Err(error) = previous_scope.validate_home() {
                return ToolResponse::failure(error);
            }
            let previous = previous_scope.state;
            let requested = match kind {
                "scope_start" if previous == ScopeState::Starting => Some(ScopeState::Starting),
                "scope_reset"
                    if matches!(
                        previous,
                        ScopeState::Ready | ScopeState::Stopped | ScopeState::Failed
                    ) =>
                {
                    Some(ScopeState::Resetting)
                }
                "scope_stop" if previous == ScopeState::Ready => Some(ScopeState::Stopping),
                "scope_start" | "scope_reset" | "scope_stop" => {
                    return ToolResponse::failure(
                        "development scope state rejects this lifecycle operation",
                    );
                }
                _ if previous == ScopeState::Ready => None,
                _ => return ToolResponse::failure("development scope is not ready"),
            };
            if kind == "scope_reset"
                && scope_id != "default"
                && previous == ScopeState::Stopped
                && live_generated_scope_count(&scopes) >= MAX_DEVELOPMENT_SCOPES
            {
                return ToolResponse::failure(format!(
                    "development scope limit ({MAX_DEVELOPMENT_SCOPES}) reached; stop another live scope before resetting this one"
                ));
            }
            if matches!(kind, "scope_reset" | "scope_stop")
                && self.operations.lock().await.values().any(|operation| {
                    operation.scope_id == scope_id
                        && matches!(
                            operation.state,
                            OperationState::Queued | OperationState::Running
                        )
                })
            {
                return ToolResponse::failure(
                    "development scope has active MCP work; wait or cancel it before changing lifecycle",
                );
            }
            if let Some(state) = requested {
                let scope = scopes.get_mut(scope_id).expect("scope exists");
                scope.state = state;
                scope.last_operation_id = Some(id.clone());
                scope.diagnostic = None;
                if self.config.generated_scope_root.is_some()
                    && let Err(error) = self.config.persist_scopes(&scopes)
                {
                    scopes.insert(scope_id.to_owned(), previous_scope.clone());
                    return ToolResponse::failure(error);
                }
            }
            previous_scope
        };
        let (cancel, cancel_rx) = watch::channel(false);
        let operation = Operation {
            id: id.clone(),
            owner_session: self.session_id.clone(),
            scope_id: scope_id.to_owned(),
            kind: kind.into(),
            state: OperationState::Queued,
            started_unix_ms: now_ms(),
            finished_unix_ms: None,
            exit_code: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            cancel: Some(cancel),
        };
        {
            let mut operations = self.operations.lock().await;
            while operations.len() >= MAX_RETAINED_OPERATIONS {
                let oldest_terminal = operations
                    .values()
                    .filter(|operation| {
                        matches!(
                            operation.state,
                            OperationState::Succeeded
                                | OperationState::Failed
                                | OperationState::CancellationUncertain
                        )
                    })
                    .min_by_key(|operation| operation.started_unix_ms)
                    .map(|operation| operation.id.clone());
                let Some(oldest_terminal) = oldest_terminal else {
                    drop(operations);
                    let mut scopes = self.scopes.lock().await;
                    scopes.insert(scope_id.to_owned(), previous_scope.clone());
                    if self.config.generated_scope_root.is_some()
                        && let Err(error) = self.config.persist_scopes(&scopes)
                    {
                        return ToolResponse::failure(format!(
                            "operation retention limit reached and scope-state rollback failed: {error}"
                        ));
                    }
                    return ToolResponse::failure(format!(
                        "operation retention limit ({MAX_RETAINED_OPERATIONS}) reached"
                    ));
                };
                operations.remove(&oldest_terminal);
            }
            operations.insert(id.clone(), operation);
        }
        let lifecycle_guard = exclusive_lifecycle.then_some(admission);

        let config = Arc::clone(&self.config);
        let operations = Arc::clone(&self.operations);
        let scopes = Arc::clone(&self.scopes);
        let task_id = id.clone();
        let task_scope_id = scope_id.to_owned();
        let scope_transition = match kind {
            "scope_start" | "scope_reset" => Some((ScopeState::Ready, ScopeState::Failed)),
            "scope_stop" => Some((ScopeState::Stopped, ScopeState::Failed)),
            _ => None,
        };
        let scope_root = config.generated_scope_root.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _lifecycle_guard = lifecycle_guard;
            let _scope_access_guard = scope_access_guard;
            set_running(&operations, &task_id).await;
            let mut result = run_bounded_cancel(&config, &spec, cancel_rx.clone()).await;
            for next in followups {
                if !result
                    .as_ref()
                    .is_ok_and(|output| output.exit_code == Some(0))
                {
                    break;
                }
                result = match run_bounded_cancel(&config, &next, cancel_rx.clone()).await {
                    Ok(next_output) => Ok(merge_output(result.expect("checked Ok"), &next_output)),
                    Err(error) => Err(error),
                };
            }
            if let Some((success, failure)) = scope_transition {
                let mut registered = scopes.lock().await;
                if let Some(scope) = registered.get_mut(&task_scope_id) {
                    let succeeded = result
                        .as_ref()
                        .is_ok_and(|output| output.exit_code == Some(0));
                    scope.state = if succeeded { success } else { failure };
                    scope.diagnostic = (!succeeded).then(|| lifecycle_diagnostic(&result));
                }
                if scope_root.is_some()
                    && let Err(error) = config.persist_scopes(&registered)
                {
                    if let Some(scope) = registered.get_mut(&task_scope_id) {
                        scope.state = ScopeState::Failed;
                        scope.diagnostic = Some(bounded_diagnostic(&error));
                    }
                    result = Err(error);
                }
            }
            finish_operation(&operations, &task_id, result).await;
        });
        ToolResponse::success(json!({"operation_id": id, "state": "queued"}))
    }

    async fn insert_removal_operation(&self, id: &str, scope_id: &str) -> Result<(), String> {
        let mut operations = self.operations.lock().await;
        while operations.len() >= MAX_RETAINED_OPERATIONS {
            let oldest_terminal = operations
                .values()
                .filter(|operation| {
                    matches!(
                        operation.state,
                        OperationState::Succeeded
                            | OperationState::Failed
                            | OperationState::CancellationUncertain
                    )
                })
                .min_by_key(|operation| operation.started_unix_ms)
                .map(|operation| operation.id.clone());
            let Some(oldest_terminal) = oldest_terminal else {
                return Err(format!(
                    "operation retention limit ({MAX_RETAINED_OPERATIONS}) reached"
                ));
            };
            operations.remove(&oldest_terminal);
        }
        operations.insert(
            id.to_owned(),
            Operation {
                id: id.to_owned(),
                owner_session: self.session_id.clone(),
                scope_id: scope_id.to_owned(),
                kind: "scope_remove".into(),
                state: OperationState::Queued,
                started_unix_ms: now_ms(),
                finished_unix_ms: None,
                exit_code: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                stdout_truncated: false,
                stderr_truncated: false,
                cancel: None,
            },
        );
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    async fn start_scope_removal(&self, scope_id: &str) -> ToolResponse {
        if let Err(error) = self.config.validate_scope_root() {
            return ToolResponse::failure(error);
        }
        let Some(root) = self.config.generated_scope_root.clone() else {
            return ToolResponse::failure("scope_remove is unavailable in legacy --home mode");
        };
        let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() else {
            return ToolResponse::failure(
                "MCP operation capacity is busy; retry after an active operation completes",
            );
        };
        let lifecycle = Arc::clone(
            self.lifecycles
                .lock()
                .await
                .entry(scope_id.to_owned())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        );
        let lifecycle_entry = Arc::clone(&lifecycle);
        let Ok(admission) = lifecycle.try_lock_owned() else {
            return ToolResponse::failure(
                "this development scope is busy; retry after its active lifecycle operation completes",
            );
        };
        let access = Arc::clone(
            self.scope_access
                .lock()
                .await
                .entry(scope_id.to_owned())
                .or_insert_with(|| Arc::new(RwLock::new(()))),
        );
        let access_entry = Arc::clone(&access);
        let Ok(access_admission) = access.try_write_owned() else {
            return ToolResponse::failure(
                "development scope has an active read and cannot be removed",
            );
        };
        let id = Uuid::new_v4().to_string();
        let scope = {
            let mut scopes = self.scopes.lock().await;
            let Some(scope) = scopes.get(scope_id).cloned() else {
                return ToolResponse::failure("unknown development scope");
            };
            if !matches!(scope.state, ScopeState::Stopped | ScopeState::Removing) {
                return ToolResponse::failure(
                    "only a generated stopped or removing development scope can be removed",
                );
            }
            if scope.home.parent() != Some(root.as_path())
                || scope.home.file_name().and_then(|name| name.to_str()) != Some(scope_id)
            {
                return ToolResponse::failure(
                    "development scope home is not the exact derived child of its fixed root",
                );
            }
            if scope.home.exists() {
                if let Err(error) = scope.validate_home() {
                    return ToolResponse::failure(error);
                }
            } else if scope.state != ScopeState::Removing {
                return ToolResponse::failure("stopped development scope home is missing");
            }
            if self.operations.lock().await.values().any(|operation| {
                operation.scope_id == scope_id
                    && matches!(
                        operation.state,
                        OperationState::Queued | OperationState::Running
                    )
            }) {
                return ToolResponse::failure(
                    "development scope has active MCP work and cannot be removed",
                );
            }

            if let Err(error) = self.insert_removal_operation(&id, scope_id).await {
                return ToolResponse::failure(error);
            }
            let registered = scopes.get_mut(scope_id).expect("scope exists");
            registered.state = ScopeState::Removing;
            registered.last_operation_id = Some(id.clone());
            registered.diagnostic = None;
            if let Err(error) = self.config.persist_scopes(&scopes) {
                self.operations.lock().await.remove(&id);
                scopes.insert(scope_id.to_owned(), scope.clone());
                return ToolResponse::failure(error);
            }
            scope
        };

        tokio::spawn(run_scope_removal_task(
            permit,
            admission,
            access_admission,
            Arc::clone(&self.scopes),
            Arc::clone(&self.operations),
            Arc::clone(&self.lifecycles),
            Arc::clone(&self.scope_access),
            root,
            self.config.preserved_registry_records.clone(),
            scope,
            id.clone(),
            scope_id.to_owned(),
            lifecycle_entry,
            access_entry,
        ));
        ToolResponse::success(json!({"operation_id": id, "state": "queued"}))
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_scope_removal_task(
    _permit: OwnedSemaphorePermit,
    admission: OwnedMutexGuard<()>,
    _access_admission: OwnedRwLockWriteGuard<()>,
    scopes: Arc<Mutex<BTreeMap<String, DevelopmentScope>>>,
    operations: Arc<Mutex<BTreeMap<String, Operation>>>,
    lifecycles: Arc<Mutex<BTreeMap<String, Arc<Mutex<()>>>>>,
    scope_access: Arc<Mutex<BTreeMap<String, Arc<RwLock<()>>>>>,
    root: PathBuf,
    preserved_registry_records: BTreeMap<String, Value>,
    scope: DevelopmentScope,
    task_id: String,
    task_scope_id: String,
    lifecycle_entry: Arc<Mutex<()>>,
    access_entry: Arc<RwLock<()>>,
) {
    set_running(&operations, &task_id).await;
    let removed = remove_exact_scope_home(&scope.home, &scope.home_identity);
    let mut result = match removed {
        Ok(()) => Ok(CapturedOutput {
            exit_code: Some(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        }),
        Err(error) => Err(error),
    };
    let mut registered = scopes.lock().await;
    if result.is_ok() {
        let removed_scope = registered.remove(&task_scope_id);
        if let Err(error) = persist_scope_registry(&root, &registered, &preserved_registry_records)
        {
            if let Some(scope) = removed_scope {
                registered.insert(task_scope_id.clone(), scope);
            }
            result = Err(error);
        }
    } else if let Some(registered_scope) = registered.get_mut(&task_scope_id) {
        registered_scope.diagnostic = Some(lifecycle_diagnostic(&result));
        let _ = persist_scope_registry(&root, &registered, &preserved_registry_records);
    }
    let succeeded = result.is_ok();
    drop(registered);
    finish_operation(&operations, &task_id, result).await;
    drop(admission);
    if succeeded {
        let mut registered_lifecycles = lifecycles.lock().await;
        if registered_lifecycles
            .get(&task_scope_id)
            .is_some_and(|current| Arc::ptr_eq(current, &lifecycle_entry))
        {
            registered_lifecycles.remove(&task_scope_id);
        }
        let mut registered_access = scope_access.lock().await;
        if registered_access
            .get(&task_scope_id)
            .is_some_and(|current| Arc::ptr_eq(current, &access_entry))
        {
            registered_access.remove(&task_scope_id);
        }
    }
}

fn remove_exact_scope_home(home: &Path, expected: &DirectoryIdentity) -> Result<(), String> {
    remove_exact_scope_home_with_limits(
        home,
        expected,
        MAX_SCOPE_REMOVAL_ENTRIES,
        MAX_SCOPE_REMOVAL_DEPTH,
    )
}

#[allow(clippy::too_many_lines)]
fn remove_exact_scope_home_with_limits(
    home: &Path,
    expected: &DirectoryIdentity,
    max_entries: usize,
    max_depth: usize,
) -> Result<(), String> {
    match fs::symlink_metadata(home) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("cannot inspect development scope home: {error}")),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err("development scope home is no longer a directory".into());
        }
        Ok(_) => {}
    }
    let current = DirectoryIdentity::capture(home)?;
    if &current != expected {
        return Err("development scope home identity changed before removal".into());
    }
    let mut pending = vec![(home.to_path_buf(), 0_usize)];
    let mut leaves = Vec::new();
    let mut directories = Vec::new();
    let expected_uid = rustix::process::geteuid().as_raw();
    let mut entry_count = 0_usize;
    while let Some((directory, depth)) = pending.pop() {
        if depth > max_depth {
            return Err(format!(
                "development scope home removal exceeds the bounded depth limit ({max_depth})"
            ));
        }
        directories.push(directory.clone());
        let entries = fs::read_dir(&directory).map_err(|error| {
            format!(
                "cannot inspect development scope directory {} before removal: {error}",
                directory.display()
            )
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                format!(
                    "cannot inspect development scope directory entry {}: {error}",
                    directory.display()
                )
            })?;
            entry_count = entry_count.saturating_add(1);
            if entry_count > max_entries {
                return Err(format!(
                    "development scope home removal exceeds the bounded entry limit ({max_entries})"
                ));
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| {
                format!(
                    "cannot inspect development scope entry {}: {error}",
                    path.display()
                )
            })?;
            if metadata.uid() != expected_uid {
                return Err(format!(
                    "development scope entry has unexpected owner: {}",
                    path.display()
                ));
            }
            if metadata.file_type().is_dir() {
                pending.push((path, depth.saturating_add(1)));
            } else if metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                leaves.push((path, metadata.file_type().is_symlink()));
            } else {
                return Err(format!(
                    "development scope home contains an unsupported special entry: {}",
                    path.display()
                ));
            }
        }
    }
    // The complete no-follow preflight above bounds the work before mutation.
    // A same-user replacement during deletion fails closed on the repeated
    // file-type check and leaves the durable Removing record retryable.
    for (path, expected_symlink) in leaves {
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "cannot revalidate development scope file {}: {error}",
                path.display()
            )
        })?;
        if metadata.file_type().is_symlink() != expected_symlink
            || (!expected_symlink && !metadata.file_type().is_file())
            || metadata.uid() != expected_uid
        {
            return Err(format!(
                "development scope file changed during bounded removal: {}",
                path.display()
            ));
        }
        fs::remove_file(&path).map_err(|error| {
            format!(
                "cannot remove development scope file {}: {error}",
                path.display()
            )
        })?;
    }
    for path in directories.into_iter().rev() {
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "cannot revalidate development scope directory {}: {error}",
                path.display()
            )
        })?;
        if !metadata.file_type().is_dir() || metadata.uid() != expected_uid {
            return Err(format!(
                "development scope directory changed during bounded removal: {}",
                path.display()
            ));
        }
        fs::remove_dir(&path).map_err(|error| {
            format!(
                "cannot remove development scope directory {}: {error}",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn live_generated_scope_count(scopes: &BTreeMap<String, DevelopmentScope>) -> usize {
    scopes
        .iter()
        .filter(|(scope_id, scope)| {
            scope_id.as_str() != "default"
                && matches!(
                    scope.state,
                    ScopeState::Starting
                        | ScopeState::Ready
                        | ScopeState::Resetting
                        | ScopeState::Stopping
                        | ScopeState::Failed
                )
        })
        .count()
}

fn bounded_diagnostic(message: &str) -> String {
    const MAX_DIAGNOSTIC_BYTES: usize = 4096;
    if message.len() <= MAX_DIAGNOSTIC_BYTES {
        return message.to_owned();
    }
    let mut end = MAX_DIAGNOSTIC_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_owned()
}

fn lifecycle_diagnostic(result: &Result<CapturedOutput, String>) -> String {
    match result {
        Err(error) => bounded_diagnostic(error),
        Ok(output) => {
            let bytes = if output.stderr.is_empty() {
                &output.stdout
            } else {
                &output.stderr
            };
            if bytes.is_empty() {
                format!(
                    "scope lifecycle command failed with exit code {}",
                    output
                        .exit_code
                        .map_or_else(|| "unknown".into(), |code| code.to_string())
                )
            } else {
                bounded_diagnostic(&String::from_utf8_lossy(bytes))
            }
        }
    }
}

#[tool_router]
impl HostMcp {
    #[tool(
        description = "Inspect the fixed MCP workspace/home/executable scope without exposing credentials or daemon relay authority."
    )]
    async fn doctor(&self, Parameters(_request): Parameters<EmptyRequest>) -> Json<ToolResponse> {
        Json(ToolResponse::success(json!({
            "schema": "marsh.mcp.doctor/v1",
            "workspace": self.config.workspace,
            "home": self.config.home,
            "generated_scope_root": self.config.generated_scope_root,
            "marsh": self.config.marsh,
            "marshd": self.config.marshd,
            "sbx": self.config.sbx,
            "max_concurrent_operations": MAX_OPERATIONS,
            "max_retained_operations": MAX_RETAINED_OPERATIONS,
            "max_live_generated_scopes": MAX_DEVELOPMENT_SCOPES,
            "max_retained_generated_scopes": MAX_RETAINED_SCOPES,
            "max_capture_bytes_per_stream": MAX_CAPTURE_BYTES,
            "full_sbx_control_enabled": self.config.full_sbx_control_enabled,
            "managed_scope_persistence": self.config.generated_scope_root.is_some(),
            "workspace_write_isolation": "shared",
            "registry_diagnostics": self.config.registry_diagnostics,
            "relay_environment_forwarded": false
        })))
    }

    #[tool(
        description = "Return typed marsh daemon, project-shell, and Kit-worker status for this selected home."
    )]
    async fn status(&self, Parameters(_request): Parameters<EmptyRequest>) -> Json<ToolResponse> {
        Json(
            self.run_json(&["status", "--json"], Duration::from_secs(20))
                .await,
        )
    }

    #[tool(
        description = "List durable structural marsh result receipts newest first; captured prompts and output are not included."
    )]
    async fn results_list(
        &self,
        Parameters(_request): Parameters<EmptyRequest>,
    ) -> Json<ToolResponse> {
        Json(
            self.run_json(&["results", "--json"], Duration::from_secs(20))
                .await,
        )
    }

    #[tool(
        description = "Get one durable structural result by cursor, full job UUID, or unique UUID prefix."
    )]
    async fn result_get(
        &self,
        Parameters(request): Parameters<ResultGetRequest>,
    ) -> Json<ToolResponse> {
        if let Err(error) = validate_selector(&request.selector) {
            return Json(ToolResponse::failure(error));
        }
        Json(
            self.run_json(
                &["results", "show", &request.selector, "--json"],
                Duration::from_secs(20),
            )
            .await,
        )
    }

    #[tool(
        description = "List durable structural result receipts for one exact development scope. Ready and Stopped scopes are supported; reading a Stopped scope may start only its local daemon and never boots its project-shell or Kit VMs."
    )]
    async fn scope_results_list(
        &self,
        Parameters(request): Parameters<ScopeRequest>,
    ) -> Json<ToolResponse> {
        let scope = match self.readable_results_scope(&request.scope_id).await {
            Ok(scope) => scope,
            Err(error) => return Json(ToolResponse::failure(error)),
        };
        Json(match scope.state {
            ScopeState::Stopped => {
                self.run_stopped_results_in(&request.scope_id, &scope, &["results", "--json"])
                    .await
            }
            ScopeState::Ready => {
                self.run_json_in(
                    &request.scope_id,
                    &scope,
                    &["results", "--json"],
                    Duration::from_secs(20),
                )
                .await
            }
            _ => unreachable!("readable_results_scope restricts state"),
        })
    }

    #[tool(
        description = "Get one durable structural result from one exact development scope by cursor, full job UUID, or unique UUID prefix. Ready and Stopped scopes are supported without booting a VM."
    )]
    async fn scope_result_get(
        &self,
        Parameters(request): Parameters<ScopeResultGetRequest>,
    ) -> Json<ToolResponse> {
        if let Err(error) = validate_selector(&request.selector) {
            return Json(ToolResponse::failure(error));
        }
        let scope = match self.readable_results_scope(&request.scope_id).await {
            Ok(scope) => scope,
            Err(error) => return Json(ToolResponse::failure(error)),
        };
        let arguments = ["results", "show", request.selector.as_str(), "--json"];
        Json(match scope.state {
            ScopeState::Stopped => {
                self.run_stopped_results_in(&request.scope_id, &scope, &arguments)
                    .await
            }
            ScopeState::Ready => {
                self.run_json_in(
                    &request.scope_id,
                    &scope,
                    &arguments,
                    Duration::from_secs(20),
                )
                .await
            }
            _ => unreachable!("readable_results_scope restricts state"),
        })
    }

    #[tool(
        description = "Prewarm `all` or an explicit comma-separated set of configured Kit worker VMs. Returns an operation ID."
    )]
    async fn prewarm(
        &self,
        Parameters(request): Parameters<SelectionRequest>,
    ) -> Json<ToolResponse> {
        let scope = match self.active_scope("default").await {
            Ok(scope) => scope,
            Err(error) => return Json(ToolResponse::failure(error)),
        };
        if let Err(error) = validate_selection(&request.selection) {
            return Json(ToolResponse::failure(error));
        }
        let spec = CommandSpec {
            executable: self.config.marsh.clone(),
            arguments: vec![
                "--load".into(),
                request.selection,
                "-c".into(),
                "true".into(),
            ],
            timeout: Duration::from_mins(5),
            home: scope.home,
            home_identity: scope.home_identity,
        };
        Json(
            self.start_operation("default", "prewarm", spec, Vec::new(), true)
                .await,
        )
    }

    #[tool(
        description = "Reset only selected idle marsh-owned Kit worker VMs. Active jobs and unrelated Sandboxes are preserved. Returns an operation ID."
    )]
    async fn workers_reset(
        &self,
        Parameters(request): Parameters<SelectionRequest>,
    ) -> Json<ToolResponse> {
        let scope = match self.active_scope("default").await {
            Ok(scope) => scope,
            Err(error) => return Json(ToolResponse::failure(error)),
        };
        if let Err(error) = validate_selection(&request.selection) {
            return Json(ToolResponse::failure(error));
        }
        let spec = CommandSpec {
            executable: self.config.marsh.clone(),
            arguments: vec!["workers".into(), "reset".into(), request.selection],
            timeout: Duration::from_mins(5),
            home: scope.home,
            home_identity: scope.home_identity,
        };
        Json(
            self.start_operation("default", "workers_reset", spec, Vec::new(), true)
                .await,
        )
    }

    #[tool(
        description = "Prewarm `all` or selected configured Kit worker VMs in one exact Ready development scope. Independent read-only inspection remains available while prewarm runs. Returns an operation ID."
    )]
    async fn scope_prewarm(
        &self,
        Parameters(request): Parameters<ScopeSelectionRequest>,
    ) -> Json<ToolResponse> {
        let scope = match self.active_scope(&request.scope_id).await {
            Ok(scope) => scope,
            Err(error) => return Json(ToolResponse::failure(error)),
        };
        if let Err(error) = validate_selection(&request.selection) {
            return Json(ToolResponse::failure(error));
        }
        let spec = CommandSpec {
            executable: self.config.marsh.clone(),
            arguments: vec![
                "--load".into(),
                request.selection,
                "-c".into(),
                "true".into(),
            ],
            timeout: Duration::from_mins(5),
            home: scope.home,
            home_identity: scope.home_identity,
        };
        Json(
            self.start_operation(&request.scope_id, "scope_prewarm", spec, Vec::new(), true)
                .await,
        )
    }

    #[tool(
        description = "Reset selected idle Kit worker VMs in one exact Ready development scope. Project shell, peer scopes, and unrelated Sandboxes are preserved. Returns an operation ID."
    )]
    async fn scope_workers_reset(
        &self,
        Parameters(request): Parameters<ScopeSelectionRequest>,
    ) -> Json<ToolResponse> {
        let scope = match self.active_scope(&request.scope_id).await {
            Ok(scope) => scope,
            Err(error) => return Json(ToolResponse::failure(error)),
        };
        if let Err(error) = validate_selection(&request.selection) {
            return Json(ToolResponse::failure(error));
        }
        let spec = CommandSpec {
            executable: self.config.marsh.clone(),
            arguments: vec!["workers".into(), "reset".into(), request.selection],
            timeout: Duration::from_mins(5),
            home: scope.home,
            home_identity: scope.home_identity,
        };
        Json(
            self.start_operation(
                &request.scope_id,
                "scope_workers_reset",
                spec,
                Vec::new(),
                true,
            )
            .await,
        )
    }

    #[tool(
        description = "Run a bounded shell program inside the scoped marsh project-shell VM. There is no host-shell, environment, SBX, or Docker passthrough. Returns an operation ID."
    )]
    async fn shell_run(
        &self,
        Parameters(request): Parameters<ShellRunRequest>,
    ) -> Json<ToolResponse> {
        let scope = match self.active_scope("default").await {
            Ok(scope) => scope,
            Err(error) => return Json(ToolResponse::failure(error)),
        };
        if request.command.is_empty()
            || request.command.len() > MAX_COMMAND_BYTES
            || request.command.contains('\0')
        {
            return Json(ToolResponse::failure(format!(
                "command must contain 1..={MAX_COMMAND_BYTES} bytes and no NUL"
            )));
        }
        let timeout = match bounded_timeout(request.timeout_ms) {
            Ok(timeout) => timeout,
            Err(error) => return Json(ToolResponse::failure(error)),
        };
        let spec = CommandSpec {
            executable: self.config.marsh.clone(),
            arguments: vec!["-c".into(), request.command],
            timeout,
            home: scope.home,
            home_identity: scope.home_identity,
        };
        Json(
            self.start_operation("default", "shell_run", spec, Vec::new(), false)
                .await,
        )
    }

    #[tool(
        description = "Create an isolated development scope for this fixed workspace. The server chooses the opaque ID and private home, then boots its project shell. All scopes mount the same canonical project workspace, so concurrent writes are shared and may race. Returns the scope and operation IDs."
    )]
    async fn scope_start(
        &self,
        Parameters(_request): Parameters<EmptyRequest>,
    ) -> Json<ToolResponse> {
        if let Err(error) = self.config.validate_scope_root() {
            return Json(ToolResponse::failure(error));
        }
        let Some(root) = self.config.generated_scope_root.as_ref() else {
            return Json(ToolResponse::failure(
                "scope_start is unavailable in legacy --home mode; start the server with --scope-root or omit both options",
            ));
        };
        let scope_id = Uuid::new_v4().to_string();
        let home = match prepare_scope_home(root, &scope_id) {
            Ok(home) => home,
            Err(error) => return Json(ToolResponse::failure(error)),
        };
        let home_identity = match DirectoryIdentity::capture(&home) {
            Ok(identity) => identity,
            Err(error) => {
                let _ = fs::remove_dir(&home);
                return Json(ToolResponse::failure(error));
            }
        };
        {
            let mut scopes = self.scopes.lock().await;
            if live_generated_scope_count(&scopes) >= MAX_DEVELOPMENT_SCOPES {
                let _ = fs::remove_dir(&home);
                return Json(ToolResponse::failure(format!(
                    "development scope limit ({MAX_DEVELOPMENT_SCOPES}) reached; stop and remove an unused scope first"
                )));
            }
            if scopes
                .len()
                .saturating_sub(1)
                .saturating_add(self.config.preserved_registry_records.len())
                >= MAX_RETAINED_SCOPES
            {
                let _ = fs::remove_dir(&home);
                return Json(ToolResponse::failure(format!(
                    "retained development scope limit ({MAX_RETAINED_SCOPES}) reached"
                )));
            }
            scopes.insert(
                scope_id.clone(),
                DevelopmentScope {
                    home: home.clone(),
                    home_identity: home_identity.clone(),
                    state: ScopeState::Starting,
                    created_unix_ms: now_ms(),
                    last_operation_id: None,
                    diagnostic: None,
                },
            );
            if let Err(error) = self.config.persist_scopes(&scopes) {
                scopes.remove(&scope_id);
                let _ = fs::remove_dir(&home);
                return Json(ToolResponse::failure(error));
            }
        }
        let spec = CommandSpec {
            executable: self.config.marsh.clone(),
            arguments: vec!["-c".into(), "true".into()],
            timeout: Duration::from_mins(5),
            home: home.clone(),
            home_identity,
        };
        let mut response = self
            .start_operation(&scope_id, "scope_start", spec, Vec::new(), true)
            .await;
        if response.ok {
            response.data = response.data.map(|mut value| {
                value["scope_id"] = Value::String(scope_id);
                value
            });
        } else {
            let mut scopes = self.scopes.lock().await;
            scopes.remove(&scope_id);
            let _ = self.config.persist_scopes(&scopes);
            let _ = fs::remove_dir(&home);
        }
        Json(response)
    }

    #[tool(
        description = "List managed development scopes by opaque ID, lifecycle state, and creation time. Host paths and runtime identities are never returned."
    )]
    async fn scope_list(
        &self,
        Parameters(_request): Parameters<EmptyRequest>,
    ) -> Json<ToolResponse> {
        if let Err(error) = self.config.validate_scope_root() {
            return Json(ToolResponse::failure(error));
        }
        let scopes = self.scopes.lock().await;
        let entries = scopes
            .iter()
            .map(|(scope_id, scope)| {
                json!({
                    "scope_id": scope_id,
                    "state": scope.state,
                    "created_unix_ms": scope.created_unix_ms
                })
            })
            .collect::<Vec<_>>();
        Json(ToolResponse::success(json!({
            "schema": "marsh.mcp.scope-list/v1",
            "scopes": entries
        })))
    }

    #[tool(
        description = "Queue removal of one generated stopped scope and its private home, or resume an interrupted persisted removal. Default, live, and failed scopes are rejected."
    )]
    async fn scope_remove(
        &self,
        Parameters(request): Parameters<ScopeRequest>,
    ) -> Json<ToolResponse> {
        if let Err(error) = self.config.validate_scope_root() {
            return Json(ToolResponse::failure(error));
        }
        if request.scope_id == "default" {
            return Json(ToolResponse::failure(
                "the default development scope cannot be removed",
            ));
        }
        if let Err(error) = validate_scope_id(&request.scope_id) {
            return Json(ToolResponse::failure(error));
        }
        Json(self.start_scope_removal(&request.scope_id).await)
    }

    #[tool(
        description = "Run a bounded shell program in one exact isolated development scope. Returns an operation ID."
    )]
    async fn scope_run(
        &self,
        Parameters(request): Parameters<ScopeRunRequest>,
    ) -> Json<ToolResponse> {
        let scope = match self.active_scope(&request.scope_id).await {
            Ok(scope) => scope,
            Err(error) => return Json(ToolResponse::failure(error)),
        };
        if request.command.is_empty()
            || request.command.len() > MAX_COMMAND_BYTES
            || request.command.contains('\0')
        {
            return Json(ToolResponse::failure(format!(
                "command must contain 1..={MAX_COMMAND_BYTES} bytes and no NUL"
            )));
        }
        let timeout = match bounded_timeout(request.timeout_ms) {
            Ok(timeout) => timeout,
            Err(error) => return Json(ToolResponse::failure(error)),
        };
        let spec = CommandSpec {
            executable: self.config.marsh.clone(),
            arguments: vec!["-c".into(), request.command],
            timeout,
            home: scope.home,
            home_identity: scope.home_identity,
        };
        Json(
            self.start_operation(&request.scope_id, "scope_run", spec, Vec::new(), false)
                .await,
        )
    }

    #[tool(
        description = "Inspect one exact isolated development scope and its marsh runtime state."
    )]
    async fn scope_status(
        &self,
        Parameters(request): Parameters<ScopeRequest>,
    ) -> Json<ToolResponse> {
        if let Err(error) = validate_scope_id(&request.scope_id) {
            return Json(ToolResponse::failure(error));
        }
        let scope = {
            let scopes = self.scopes.lock().await;
            scopes.get(&request.scope_id).cloned()
        };
        let Some(scope) = scope else {
            return Json(ToolResponse::failure("unknown development scope"));
        };
        if scope.state != ScopeState::Failed {
            if scope.home.exists() {
                if let Err(error) = scope.validate_home() {
                    return Json(ToolResponse::failure(error));
                }
            } else if scope.state != ScopeState::Removing {
                return Json(ToolResponse::failure("development scope home is missing"));
            }
        }
        if scope.state != ScopeState::Ready {
            let runtime_state = match scope.state {
                ScopeState::Stopped => "absent",
                ScopeState::Failed => "unknown",
                ScopeState::Starting
                | ScopeState::Resetting
                | ScopeState::Stopping
                | ScopeState::Removing => "transitioning",
                ScopeState::Ready => unreachable!(),
            };
            return Json(ToolResponse::success(json!({
                "schema": "marsh.mcp.scope-status/v1",
                "scope_id": request.scope_id,
                "state": scope.state,
                "runtime_state": runtime_state,
                "created_unix_ms": scope.created_unix_ms,
                "last_operation_id": scope.last_operation_id,
                "diagnostic": scope.diagnostic,
                "runtime": null
            })));
        }
        let runtime = self
            .run_json_in(
                &request.scope_id,
                &scope,
                &["status", "--json"],
                Duration::from_secs(20),
            )
            .await;
        Json(if runtime.ok {
            ToolResponse::success(json!({
                "schema": "marsh.mcp.scope-status/v1",
                "scope_id": request.scope_id,
                "state": scope.state,
                "runtime_state": "ready",
                "created_unix_ms": scope.created_unix_ms,
                "last_operation_id": scope.last_operation_id,
                "diagnostic": scope.diagnostic,
                "runtime": runtime.data
            }))
        } else {
            runtime
        })
    }

    #[tool(
        description = "Reset one exact idle development scope, preserving its home and structural results, then recreate its project shell. Refuses active work. Returns an operation ID."
    )]
    async fn scope_reset(
        &self,
        Parameters(request): Parameters<ScopeRequest>,
    ) -> Json<ToolResponse> {
        let scope = match self.resettable_scope(&request.scope_id).await {
            Ok(scope) => scope,
            Err(error) => return Json(ToolResponse::failure(error)),
        };
        let boot = CommandSpec {
            executable: self.config.marsh.clone(),
            arguments: vec!["-c".into(), "true".into()],
            timeout: Duration::from_mins(5),
            home: scope.home.clone(),
            home_identity: scope.home_identity.clone(),
        };
        let cleanup = CommandSpec {
            executable: self.config.marsh.clone(),
            arguments: vec!["reset".into(), "--json".into()],
            timeout: SCOPE_LIFECYCLE_TIMEOUT,
            home: scope.home,
            home_identity: scope.home_identity,
        };
        let (spec, followups) = match scope.state {
            ScopeState::Stopped => (boot, Vec::new()),
            ScopeState::Ready | ScopeState::Failed => (boot.clone(), vec![cleanup, boot]),
            _ => unreachable!("resettable_scope rejects transitional states"),
        };
        Json(
            self.start_operation(&request.scope_id, "scope_reset", spec, followups, true)
                .await,
        )
    }

    #[tool(
        description = "Stop one exact idle development scope without deleting its home or results. It first boots or validates that scope's daemon, then sends the exact typed stop control to remove its project shell and Kit VMs and stop the daemon. Refuses active work. Returns an operation ID."
    )]
    async fn scope_stop(
        &self,
        Parameters(request): Parameters<ScopeRequest>,
    ) -> Json<ToolResponse> {
        let scope = match self.active_scope(&request.scope_id).await {
            Ok(scope) => scope,
            Err(error) => return Json(ToolResponse::failure(error)),
        };
        let boot = CommandSpec {
            executable: self.config.marsh.clone(),
            arguments: vec!["-c".into(), "true".into()],
            timeout: Duration::from_mins(5),
            home: scope.home.clone(),
            home_identity: scope.home_identity.clone(),
        };
        let stop = CommandSpec {
            executable: self.config.marsh.clone(),
            arguments: vec!["stop".into(), "--json".into()],
            timeout: SCOPE_LIFECYCLE_TIMEOUT,
            home: scope.home,
            home_identity: scope.home_identity,
        };
        Json(
            self.start_operation(&request.scope_id, "scope_stop", boot, vec![stop], true)
                .await,
        )
    }

    #[tool(
        description = "Use the full-SBX-control capability to start one fixed repository qualification gate: source, smoke, full, or perf. Arbitrary make targets, variables, and host commands are rejected. Returns an operation ID."
    )]
    async fn qualify(&self, Parameters(request): Parameters<QualifyRequest>) -> Json<ToolResponse> {
        if !self.config.full_sbx_control_enabled {
            return Json(ToolResponse::failure(
                "full SBX control is disabled; restart marsh-mcp with --allow-full-sbx-control to enable fixed repository gates",
            ));
        }
        let scope = match self.active_scope("default").await {
            Ok(scope) => scope,
            Err(error) => return Json(ToolResponse::failure(error)),
        };
        let target = match request.gate.as_str() {
            "source" => "test",
            "smoke" => "acceptance-smoke",
            "full" => "acceptance",
            "perf" => "perf",
            _ => {
                return Json(ToolResponse::failure(
                    "gate must be one of: source, smoke, full, perf",
                ));
            }
        };
        let timeout = match bounded_timeout(request.timeout_ms.or(Some(MAX_TIMEOUT_MS))) {
            Ok(timeout) => timeout,
            Err(error) => return Json(ToolResponse::failure(error)),
        };
        let spec = CommandSpec {
            executable: self.config.make.clone(),
            arguments: vec![target.into()],
            timeout,
            home: scope.home,
            home_identity: scope.home_identity,
        };
        Json(
            self.start_operation(
                "default",
                &format!("qualify:{target}"),
                spec,
                Vec::new(),
                true,
            )
            .await,
        )
    }

    #[tool(description = "Inspect state and bounded output sizes for an MCP-started operation.")]
    async fn operation_get(
        &self,
        Parameters(request): Parameters<OperationRequest>,
    ) -> Json<ToolResponse> {
        if let Err(error) = validate_operation_id(&request.operation_id) {
            return Json(ToolResponse::failure(error));
        }
        let operations = self.operations.lock().await;
        Json(match operations.get(&request.operation_id) {
            Some(operation) if operation.owner_session == self.session_id => ToolResponse::success(
                serde_json::to_value(operation.view()).expect("operation view serializes"),
            ),
            Some(_) => ToolResponse::failure("unknown operation ID for this MCP session"),
            None => ToolResponse::failure("unknown operation ID"),
        })
    }

    #[tool(
        description = "Read bounded stdout and stderr for an MCP-started operation. Output is available after it reaches a terminal state."
    )]
    async fn operation_output(
        &self,
        Parameters(request): Parameters<OperationRequest>,
    ) -> Json<ToolResponse> {
        if let Err(error) = validate_operation_id(&request.operation_id) {
            return Json(ToolResponse::failure(error));
        }
        let operations = self.operations.lock().await;
        Json(match operations.get(&request.operation_id) {
            Some(operation) if operation.owner_session != self.session_id => {
                ToolResponse::failure("unknown operation ID for this MCP session")
            }
            Some(operation)
                if matches!(
                    operation.state,
                    OperationState::Queued | OperationState::Running
                ) =>
            {
                ToolResponse::failure("operation output is available after completion")
            }
            Some(operation) => ToolResponse::success(json!({
                "operation": operation.view(),
                "stdout": encoded_stream(&operation.stdout),
                "stderr": encoded_stream(&operation.stderr)
            })),
            None => ToolResponse::failure("unknown operation ID"),
        })
    }

    #[tool(
        description = "Cancel one queued or running MCP-started operation. The child process is killed on cancellation."
    )]
    async fn operation_cancel(
        &self,
        Parameters(request): Parameters<OperationRequest>,
    ) -> Json<ToolResponse> {
        if let Err(error) = validate_operation_id(&request.operation_id) {
            return Json(ToolResponse::failure(error));
        }
        let mut operations = self.operations.lock().await;
        let Some(operation) = operations.get_mut(&request.operation_id) else {
            return Json(ToolResponse::failure("unknown operation ID"));
        };
        if operation.owner_session != self.session_id {
            return Json(ToolResponse::failure(
                "unknown operation ID for this MCP session",
            ));
        }
        if matches!(
            operation.state,
            OperationState::Succeeded
                | OperationState::Failed
                | OperationState::CancellationUncertain
        ) {
            return Json(ToolResponse::success(
                json!({"operation": operation.view(), "cancelled": false}),
            ));
        }
        let signalled = operation
            .cancel
            .as_ref()
            .is_some_and(|cancel| cancel.send(true).is_ok());
        Json(ToolResponse::success(json!({
            "operation_id": request.operation_id,
            "cancel_requested": signalled
        })))
    }
}

#[tool_handler(
    name = "marsh-host",
    version = "0.1.0",
    instructions = "Typed control of one canonical marsh workspace and selected home. Tools never expose raw host shell, Docker, SBX, arbitrary executables, environment injection, or daemon credentials."
)]
impl ServerHandler for HostMcp {}

#[derive(Debug)]
struct CapturedOutput {
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

fn merge_output(mut first: CapturedOutput, second: &CapturedOutput) -> CapturedOutput {
    let stdout_remaining = MAX_CAPTURE_BYTES.saturating_sub(first.stdout.len());
    first
        .stdout
        .extend_from_slice(&second.stdout[..second.stdout.len().min(stdout_remaining)]);
    let stderr_remaining = MAX_CAPTURE_BYTES.saturating_sub(first.stderr.len());
    first
        .stderr
        .extend_from_slice(&second.stderr[..second.stderr.len().min(stderr_remaining)]);
    first.stdout_truncated |= second.stdout_truncated || second.stdout.len() > stdout_remaining;
    first.stderr_truncated |= second.stderr_truncated || second.stderr.len() > stderr_remaining;
    first.exit_code = second.exit_code;
    first
}

async fn run_bounded(config: &HostConfig, spec: &CommandSpec) -> Result<CapturedOutput, String> {
    let (_cancel, cancel_rx) = watch::channel(false);
    run_bounded_cancel(config, spec, cancel_rx).await
}

async fn run_bounded_cancel(
    config: &HostConfig,
    spec: &CommandSpec,
    mut cancel_rx: watch::Receiver<bool>,
) -> Result<CapturedOutput, String> {
    let deadline = tokio::time::Instant::now() + spec.timeout;
    spec.validate_home()?;
    if spec.executable == config.marsh {
        config.validate_host_executables()?;
    }
    let mut command = Command::new(&spec.executable);
    command
        .args(&spec.arguments)
        .current_dir(&config.workspace)
        .env_clear()
        .env("MARSH_HOME", &spec.home)
        .env("MARSH_SBX", &config.sbx)
        .env("USER", &config.username)
        .env("LOGNAME", &config.username)
        .env("PATH", fixed_minimal_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    for name in INHERITED_ENV {
        if let Some(value) = env::var_os(name) {
            command.env(name, value);
        }
    }
    // These are denied even if the inherited allowlist grows in the future.
    for name in RELAY_ENV {
        command.env_remove(name);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start {}: {error}", spec.executable.display()))?;
    let pid = child
        .id()
        .ok_or_else(|| "spawned command has no process ID".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "spawned command has no stdout pipe".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "spawned command has no stderr pipe".to_string())?;
    let mut stdout_reader = tokio::spawn(read_bounded_stream(stdout));
    let mut stderr_reader = tokio::spawn(read_bounded_stream(stderr));
    let status = tokio::select! {
        result = child.wait() => {
            result.map_err(|error| format!("command wait failed: {error}"))?
        }
        result = cancel_rx.changed() => {
            let cleanup = terminate_process_group(pid, &mut child).await;
            stdout_reader.abort();
            stderr_reader.abort();
            if result.is_ok() && *cancel_rx.borrow() {
                return Err(format!(
                    "operation cancellation requested; cleanup_uncertain=true; {}",
                    cleanup.err().unwrap_or_else(|| "process group exited".into())
                ));
            }
            return Err("operation cancellation channel closed".into());
        },
        () = tokio::time::sleep_until(deadline) => {
            terminate_process_group(pid, &mut child).await?;
            stdout_reader.abort();
            stderr_reader.abort();
            return Err(format!("command exceeded {} ms", spec.timeout.as_millis()));
        }
    };
    let (stdout, stdout_truncated) =
        if let Ok(result) = tokio::time::timeout_at(deadline, &mut stdout_reader).await {
            result.map_err(|error| format!("stdout reader failed: {error}"))??
        } else {
            terminate_process_group(pid, &mut child).await?;
            stdout_reader.abort();
            stderr_reader.abort();
            return Err(format!(
                "command exceeded {} ms while draining descendant output",
                spec.timeout.as_millis()
            ));
        };
    let (stderr, stderr_truncated) =
        if let Ok(result) = tokio::time::timeout_at(deadline, &mut stderr_reader).await {
            result.map_err(|error| format!("stderr reader failed: {error}"))??
        } else {
            terminate_process_group(pid, &mut child).await?;
            stderr_reader.abort();
            return Err(format!(
                "command exceeded {} ms while draining descendant output",
                spec.timeout.as_millis()
            ));
        };
    Ok(CapturedOutput {
        exit_code: status.code(),
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    })
}

pub(crate) async fn read_bounded_stream(
    mut reader: impl AsyncRead + Unpin,
) -> Result<(Vec<u8>, bool), String> {
    let mut captured = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|error| format!("cannot read command output: {error}"))?;
        if count == 0 {
            break;
        }
        let remaining = MAX_CAPTURE_BYTES.saturating_sub(captured.len());
        let retained = count.min(remaining);
        captured.extend_from_slice(&buffer[..retained]);
        truncated |= retained < count;
    }
    Ok((captured, truncated))
}

pub(crate) async fn terminate_process_group(
    pid: u32,
    child: &mut tokio::process::Child,
) -> Result<(), String> {
    let pid = i32::try_from(pid).map_err(|_| "child process ID overflow".to_string())?;
    signal_process_group(pid, nix::sys::signal::Signal::SIGINT)?;
    if wait_for_process_group_exit(pid, child, Duration::from_secs(8)).await? {
        return Ok(());
    }
    signal_process_group(pid, nix::sys::signal::Signal::SIGTERM)?;
    if wait_for_process_group_exit(pid, child, Duration::from_secs(2)).await? {
        return Ok(());
    }
    signal_process_group(pid, nix::sys::signal::Signal::SIGKILL)?;
    if wait_for_process_group_exit(pid, child, Duration::from_secs(2)).await? {
        return Ok(());
    }
    Err("command process group cleanup remains uncertain after SIGKILL".into())
}

/// Published pipelines are complete scripts. An interrupt can let Brush run
/// the next `;` command, so cancel them with termination instead.
pub(crate) async fn terminate_pipeline_process_group(
    pid: u32,
    child: &mut tokio::process::Child,
) -> Result<(), String> {
    let pid = i32::try_from(pid).map_err(|_| "child process ID overflow".to_string())?;
    signal_process_group(pid, nix::sys::signal::Signal::SIGTERM)?;
    if wait_for_process_group_exit(pid, child, Duration::from_secs(2)).await? {
        return Ok(());
    }
    signal_process_group(pid, nix::sys::signal::Signal::SIGKILL)?;
    if wait_for_process_group_exit(pid, child, Duration::from_secs(2)).await? {
        return Ok(());
    }
    Err("pipeline process group cleanup remains uncertain after SIGKILL".into())
}

async fn wait_for_process_group_exit(
    pid: i32,
    child: &mut tokio::process::Child,
    grace: Duration,
) -> Result<bool, String> {
    let deadline = tokio::time::Instant::now() + grace;
    loop {
        child
            .try_wait()
            .map_err(|error| format!("cannot reap command leader: {error}"))?;
        if !process_group_exists(pid)? {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub(crate) fn process_group_exists(pid: i32) -> Result<bool, String> {
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(-pid), None) {
        Ok(()) | Err(nix::errno::Errno::EPERM) => Ok(true),
        Err(nix::errno::Errno::ESRCH) => Ok(false),
        Err(error) => Err(format!("cannot inspect command process group: {error}")),
    }
}

fn signal_process_group(pid: i32, signal: nix::sys::signal::Signal) -> Result<(), String> {
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(-pid), signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(error) => Err(format!("cannot signal command process group: {error}")),
    }
}

async fn set_running(operations: &Mutex<BTreeMap<String, Operation>>, id: &str) {
    if let Some(operation) = operations.lock().await.get_mut(id) {
        operation.state = OperationState::Running;
    }
}

async fn finish_operation(
    operations: &Mutex<BTreeMap<String, Operation>>,
    id: &str,
    result: Result<CapturedOutput, String>,
) {
    let mut operations = operations.lock().await;
    let Some(operation) = operations.get_mut(id) else {
        return;
    };
    operation.finished_unix_ms = Some(now_ms());
    operation.cancel = None;
    match result {
        Ok(output) => {
            operation.exit_code = output.exit_code;
            operation.state = if output.exit_code == Some(0) {
                OperationState::Succeeded
            } else {
                OperationState::Failed
            };
            operation.stdout = output.stdout;
            operation.stderr = output.stderr;
            operation.stdout_truncated = output.stdout_truncated;
            operation.stderr_truncated = output.stderr_truncated;
        }
        Err(error) if error.starts_with("operation cancellation requested") => {
            operation.state = OperationState::CancellationUncertain;
            operation.stderr = error.into_bytes();
        }
        Err(error) => {
            operation.state = OperationState::Failed;
            operation.stderr = error.into_bytes();
        }
    }
}

fn command_failure(name: &str, output: &CapturedOutput) -> String {
    let detail = String::from_utf8_lossy(&output.stderr);
    format!(
        "{name} exited with {}: {}",
        output
            .exit_code
            .map_or_else(|| "signal".into(), |code| code.to_string()),
        detail.trim()
    )
}

fn encoded_stream(bytes: &[u8]) -> Value {
    json!({
        "byte_length": bytes.len(),
        "sha256": format!("{:x}", Sha256::digest(bytes)),
        "base64": BASE64.encode(bytes)
    })
}

fn bounded_timeout(value: Option<u64>) -> Result<Duration, String> {
    let milliseconds = value.unwrap_or(DEFAULT_TIMEOUT_MS);
    if !(10..=MAX_TIMEOUT_MS).contains(&milliseconds) {
        return Err(format!(
            "timeout_ms must be between 10 and {MAX_TIMEOUT_MS}"
        ));
    }
    Ok(Duration::from_millis(milliseconds))
}

fn validate_selection(selection: &str) -> Result<(), String> {
    if selection == "all" {
        return Ok(());
    }
    let kits: Vec<_> = selection.split(',').collect();
    if selection.is_empty()
        || selection.len() > 1024
        || kits.len() > 16
        || kits.into_iter().any(|kit| {
            kit.is_empty()
                || kit.len() > 64
                || !kit
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
    {
        return Err("selection must be `all` or comma-separated lowercase Kit names".into());
    }
    Ok(())
}

fn validate_operation_id(operation_id: &str) -> Result<(), String> {
    if operation_id.len() > 128 || Uuid::parse_str(operation_id).is_err() {
        return Err("operation_id must be a UUID no longer than 128 bytes".into());
    }
    Ok(())
}

fn validate_scope_id(scope_id: &str) -> Result<(), String> {
    if scope_id == "default"
        || Uuid::parse_str(scope_id).is_ok_and(|parsed| parsed.to_string() == scope_id)
    {
        return Ok(());
    }
    Err("scope_id must be `default` or the canonical lowercase UUID returned by scope_start".into())
}

fn validate_selector(selector: &str) -> Result<(), String> {
    if selector.is_empty()
        || selector.len() > 64
        || !selector
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err("selector must be a cursor, UUID, or UUID prefix".into());
    }
    Ok(())
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf, String> {
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

fn resolve_without_creation(path: &Path) -> Result<PathBuf, String> {
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

fn is_at_or_beneath(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

fn effective_username() -> Result<String, String> {
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

fn canonical_file(path: &Path, label: &str) -> Result<PathBuf, String> {
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

fn canonical_external_executable(path: &Path, label: &str) -> Result<PathBuf, String> {
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

fn validate_executable_metadata(
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

fn validate_trusted_parent_chain(path: &Path) -> Result<(), String> {
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
        if mode & 0o022 != 0 && !is_allowed_platform_temp_root(directory, mode) {
            return Err(format!(
                "executable parent must not be writable by group or other users: {}",
                directory.display()
            ));
        }
    }
    Ok(())
}

fn is_allowed_platform_temp_root(path: &Path, mode: u32) -> bool {
    if mode & 0o1000 == 0 {
        return false;
    }
    [PathBuf::from("/tmp"), PathBuf::from("/private/tmp")]
        .into_iter()
        .filter_map(|candidate| candidate.canonicalize().ok())
        .any(|candidate| candidate == path)
}

fn prepare_home(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("home must be an absolute path".into());
    }
    let product_root = path.ancestors().find(|ancestor| {
        ancestor
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with("-mcp-scopes"))
    });
    if let Some(product_root) = product_root {
        let host_home = product_root.parent();
        for directory in path
            .ancestors()
            .take_while(|directory| Some(*directory) != host_home)
        {
            match fs::symlink_metadata(directory) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(format!(
                        "MCP control directory must not be a symlink: {}",
                        directory.display()
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("cannot inspect MCP control directory: {error}")),
            }
        }
        if fs::symlink_metadata(product_root).is_ok() {
            validate_mcp_product_root(product_root)?;
        }
    }
    if !path.exists() {
        let resolved = resolve_without_creation(path)?;
        let mut missing = Vec::new();
        let mut current = resolved.as_path();
        while !current.exists() {
            missing.push(current.to_path_buf());
            current = current
                .parent()
                .ok_or_else(|| format!("cannot find MCP home parent: {}", path.display()))?;
        }
        for directory in missing.into_iter().rev() {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(&directory)
                .map_err(|error| {
                    format!("cannot create MCP home {}: {error}", directory.display())
                })?;
        }
    }
    if let Some(product_root) = product_root {
        validate_mcp_product_root(product_root)?;
    }
    let canonical = canonical_directory(path, "home")?;
    let metadata = canonical
        .metadata()
        .map_err(|error| format!("cannot inspect MCP home: {error}"))?;
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err("MCP home must be owned by the current user".into());
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err("MCP home must not be accessible by group or other users".into());
    }
    Ok(canonical)
}

fn validate_mcp_product_root(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "cannot inspect MCP product directory {}: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(format!(
            "MCP product directory must be a real, current-user-owned mode 0700 directory: {}; inspect it and repair its permissions before retrying",
            path.display()
        ));
    }
    Ok(())
}

pub(crate) fn prepare_scope_root(path: &Path) -> Result<PathBuf, String> {
    prepare_home(path).map_err(|error| format!("invalid development scope root: {error}"))
}

fn acquire_scope_root_lease(root: &Path) -> Result<Arc<ScopeRootLease>, String> {
    let path = root.join("control.lock");
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed());
    let mut file = options
        .open(&path)
        .map_err(|error| format!("cannot open development-control lease: {error}"))?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("cannot inspect development-control lease: {error}"))?;
    if !metadata.file_type().is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err("development-control lease must be an owner-only regular file".into());
    }
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
        |error| {
            format!(
                "development scope root is already controlled by another marsh-mcp process: {error}"
            )
        },
    )?;
    file.set_len(0)
        .and_then(|()| writeln!(file, "{}", std::process::id()))
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("cannot publish development-control lease owner: {error}"))?;
    Ok(Arc::new(ScopeRootLease { file }))
}

fn workspace_binding(workspace: &Path) -> Result<WorkspaceBinding, String> {
    let metadata = fs::symlink_metadata(workspace).map_err(|error| {
        format!(
            "cannot inspect canonical MCP workspace {}: {error}",
            workspace.display()
        )
    })?;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "canonical MCP workspace is no longer a directory: {}",
            workspace.display()
        ));
    }
    Ok(WorkspaceBinding {
        version: 1,
        canonical_workspace: workspace.to_path_buf(),
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn establish_workspace_binding(root: &Path, workspace: &Path) -> Result<(), String> {
    let path = root.join("workspace.json");
    let expected = workspace_binding(workspace)?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file()
                || metadata.uid() != rustix::process::geteuid().as_raw()
                || metadata.permissions().mode() & 0o077 != 0
            {
                return Err(format!(
                    "development workspace binding must be an owner-only regular file: {}",
                    path.display()
                ));
            }
            if metadata.len() > MAX_WORKSPACE_BINDING_BYTES {
                return Err(format!(
                    "development workspace binding exceeds its size limit: {}",
                    path.display()
                ));
            }
            let bytes = fs::read(&path).map_err(|error| {
                format!(
                    "cannot read development workspace binding {}: {error}",
                    path.display()
                )
            })?;
            let actual: WorkspaceBinding = serde_json::from_slice(&bytes).map_err(|error| {
                format!(
                    "cannot decode development workspace binding {}: {error}",
                    path.display()
                )
            })?;
            if actual != expected {
                return Err(format!(
                    "development scope root is bound to a different canonical workspace identity: root={}, bound={}, requested={}",
                    root.display(),
                    actual.canonical_workspace.display(),
                    expected.canonical_workspace.display()
                ));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut unexpected = Vec::new();
            for entry in fs::read_dir(root).map_err(|error| {
                format!(
                    "cannot inspect unbound development scope root {}: {error}",
                    root.display()
                )
            })? {
                let entry = entry.map_err(|error| {
                    format!(
                        "cannot inspect unbound development scope root {}: {error}",
                        root.display()
                    )
                })?;
                if entry.file_name() != "control.lock" {
                    unexpected.push(entry.file_name());
                }
            }
            if !unexpected.is_empty() {
                return Err(format!(
                    "development scope root has existing state but no workspace binding; refusing unsafe automatic migration: {} (select a new empty --scope-root or repair it explicitly)",
                    root.display()
                ));
            }
            let bytes = serde_json::to_vec_pretty(&expected)
                .map_err(|error| format!("cannot encode development workspace binding: {error}"))?;
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true).mode(0o600);
            let mut file = options.open(&path).map_err(|error| {
                format!(
                    "cannot create development workspace binding {}: {error}",
                    path.display()
                )
            })?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|error| {
                    format!(
                        "cannot publish development workspace binding {}: {error}",
                        path.display()
                    )
                })?;
            fs::File::open(root)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| {
                    format!(
                        "cannot sync development workspace binding {}: {error}",
                        path.display()
                    )
                })
        }
        Err(error) => Err(format!(
            "cannot inspect development workspace binding {}: {error}",
            path.display()
        )),
    }
}

fn reap_stale_scope_registry_temps(root: &Path) -> Result<(), String> {
    for entry in fs::read_dir(root).map_err(|error| {
        format!(
            "cannot inspect development scope root for stale registry files {}: {error}",
            root.display()
        )
    })? {
        let entry = entry.map_err(|error| {
            format!(
                "cannot inspect development scope root for stale registry files {}: {error}",
                root.display()
            )
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(stem) = name.strip_suffix(".tmp") else {
            continue;
        };
        if !stem.starts_with(".scopes-") {
            continue;
        }
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "cannot inspect stale scope registry {}: {error}",
                path.display()
            )
        })?;
        if !metadata.file_type().is_file()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(format!(
                "stale scope registry must be an owner-only regular file before removal: {}",
                path.display()
            ));
        }
        fs::remove_file(&path).map_err(|error| {
            format!(
                "cannot remove stale scope registry {}: {error}",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn prepare_scope_home(root: &Path, scope_id: &str) -> Result<PathBuf, String> {
    validate_scope_id(scope_id)?;
    if scope_id == "default" {
        return Err("generated development scope ID cannot be `default`".into());
    }
    let candidate = root.join(scope_id);
    match fs::symlink_metadata(&candidate) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err("development scope home must not be a symlink".into());
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!("cannot inspect development scope home: {error}"));
        }
    }
    let home = prepare_home(&candidate)?;
    if home.parent() != Some(root) {
        return Err("development scope home escaped its fixed private root".into());
    }
    Ok(home)
}

fn load_scope_registry(root: &Path) -> Result<LoadedScopeRegistry, String> {
    let path = root.join("scopes.json");
    if !path.exists() {
        return Ok(LoadedScopeRegistry::default());
    }
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        format!(
            "cannot inspect development scope registry {}: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(format!(
            "development scope registry must be an owner-only regular file: {}",
            path.display()
        ));
    }
    let bytes = fs::read(&path).map_err(|error| {
        format!(
            "cannot read development scope registry {}: {error}",
            path.display()
        )
    })?;
    let records: BTreeMap<String, Value> = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "cannot decode development scope registry {}: {error}",
            path.display()
        )
    })?;
    if records.len() > MAX_RETAINED_SCOPES.saturating_add(1) {
        return Err(format!(
            "development scope registry exceeds its scope limit: {}",
            path.display()
        ));
    }
    let mut scopes = BTreeMap::new();
    let mut preserved_records = BTreeMap::new();
    let mut diagnostics = Vec::new();
    for (scope_id, record) in records {
        if scope_id != "default"
            && !Uuid::parse_str(&scope_id).is_ok_and(|parsed| parsed.to_string() == scope_id)
        {
            preserved_records.insert(scope_id, record);
            diagnostics.push(
                "a persisted scope record has an invalid or noncanonical ID; it was preserved but is unavailable until an operator repairs the owner-only registry"
                    .to_owned(),
            );
            continue;
        }
        let scope = serde_json::from_value::<DevelopmentScope>(record).unwrap_or_else(|error| {
            DevelopmentScope {
                home: PathBuf::new(),
                home_identity: DirectoryIdentity::default(),
                state: ScopeState::Failed,
                created_unix_ms: now_ms(),
                last_operation_id: None,
                diagnostic: Some(bounded_diagnostic(&format!(
                    "persisted scope record is malformed and was isolated: {error}"
                ))),
            }
        });
        scopes.insert(
            scope_id.clone(),
            reconcile_persisted_scope(root, &scope_id, scope),
        );
    }
    Ok(LoadedScopeRegistry {
        scopes,
        preserved_records,
        diagnostics,
    })
}

fn reconcile_persisted_scope(
    root: &Path,
    scope_id: &str,
    mut scope: DevelopmentScope,
) -> DevelopmentScope {
    let candidate = root.join(scope_id);
    if scope.state == ScopeState::Removing {
        scope.home.clone_from(&candidate);
        if scope_id == "default" {
            scope.state = ScopeState::Failed;
            if let Ok(identity) = DirectoryIdentity::capture(&candidate) {
                scope.home_identity = identity;
            }
            scope.diagnostic = Some(bounded_diagnostic(
                "persisted default scope cannot be removing and was isolated",
            ));
        } else if let Some(error) = invalid_removing_home(&candidate, &scope.home_identity) {
            scope.state = ScopeState::Failed;
            scope.diagnostic = Some(bounded_diagnostic(&format!(
                "persisted removing scope record was isolated: {error}"
            )));
        }
    } else {
        scope.home.clone_from(&candidate);
        let verification = fs::symlink_metadata(&candidate)
            .map_err(|error| format!("cannot inspect persisted scope home: {error}"))
            .and_then(|metadata| {
                if !metadata.file_type().is_dir() {
                    return Err("persisted scope home is no longer a directory".to_owned());
                }
                DirectoryIdentity::capture(&candidate)
            })
            .and_then(|current| {
                if current == scope.home_identity {
                    Ok(())
                } else {
                    Err("persisted scope home identity changed after restart".to_owned())
                }
            });
        if let Err(error) = verification {
            scope.state = ScopeState::Failed;
            let prior = scope
                .diagnostic
                .take()
                .map(|diagnostic| format!("{diagnostic}; "))
                .unwrap_or_default();
            scope.diagnostic = Some(bounded_diagnostic(&format!(
                "{prior}persisted scope home could not be verified safely and was isolated: {error}"
            )));
        }
    }
    if matches!(
        scope.state,
        ScopeState::Starting | ScopeState::Resetting | ScopeState::Stopping
    ) {
        scope.state = ScopeState::Failed;
        scope.diagnostic =
            Some("MCP process exited while a scope lifecycle transition was in progress".into());
    }
    scope
}

fn invalid_removing_home(home: &Path, expected: &DirectoryIdentity) -> Option<String> {
    match fs::symlink_metadata(home) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => Some(format!(
            "cannot inspect persisted removing scope home: {error}"
        )),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            Some("persisted removing scope home is no longer a directory".to_owned())
        }
        Ok(_) => match DirectoryIdentity::capture(home) {
            Ok(current) if &current == expected => None,
            Ok(_) => {
                Some("persisted removing scope home identity changed after restart".to_owned())
            }
            Err(error) => Some(error),
        },
    }
}

fn persist_scope_registry(
    root: &Path,
    scopes: &BTreeMap<String, DevelopmentScope>,
    preserved_records: &BTreeMap<String, Value>,
) -> Result<(), String> {
    let mut records = preserved_records.clone();
    for (scope_id, scope) in scopes {
        let value = serde_json::to_value(scope)
            .map_err(|error| format!("cannot encode development scope record: {error}"))?;
        records.insert(scope_id.clone(), value);
    }
    let bytes = serde_json::to_vec_pretty(&records)
        .map_err(|error| format!("cannot encode development scope registry: {error}"))?;
    let temporary = root.join(format!(".scopes-{}.tmp", Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("cannot create development scope registry: {error}"))?;
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("cannot write development scope registry: {error}"));
    }
    fs::rename(&temporary, root.join("scopes.json")).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        format!("cannot publish development scope registry: {error}")
    })?;
    fs::File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("cannot sync development scope registry: {error}"))
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;
    use tokio::io::AsyncWriteExt;

    fn executable(path: &Path, body: &str) {
        let mut file = fs::File::create(path).unwrap();
        file.write_all(body.as_bytes()).unwrap();
        let mut permissions = file.metadata().unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn supporting_executables(marsh: &Path) -> PathBuf {
        let directory = marsh.parent().unwrap();
        executable(&directory.join("marshd"), "#!/bin/sh\nexit 0\n");
        let sbx = directory.join("sbx");
        executable(&sbx, "#!/bin/sh\nexit 0\n");
        sbx
    }

    fn fixture() -> (TempDir, HostConfig) {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let home = root.path().join("home");
        let marsh = root.path().join("marsh");
        fs::create_dir(&workspace).unwrap();
        executable(
            &marsh,
            "#!/bin/sh\nexec /bin/echo '{\"schema\":\"test/v1\"}'\n",
        );
        let sbx = supporting_executables(&marsh);
        let config = HostConfig::new(&workspace, &home, &marsh, &sbx, false).unwrap();
        (root, config)
    }

    fn fixture_with_group_writable_sbx_parent() -> (TempDir, HostConfig) {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let home = root.path().join("home");
        let trusted_bin = root.path().join("trusted-bin");
        let external_bin = root.path().join("homebrew-caskroom");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&trusted_bin).unwrap();
        fs::create_dir(&external_bin).unwrap();
        fs::set_permissions(&external_bin, fs::Permissions::from_mode(0o775)).unwrap();
        let marsh = trusted_bin.join("marsh");
        executable(&marsh, "#!/bin/sh\nexit 0\n");
        executable(&trusted_bin.join("marshd"), "#!/bin/sh\nexit 0\n");
        let sbx = external_bin.join("sbx");
        executable(&sbx, "#!/bin/sh\nexit 0\n");
        let config = HostConfig::new(&workspace, &home, &marsh, &sbx, false).unwrap();
        (root, config)
    }

    #[test]
    fn validates_selections_and_result_selectors() {
        assert!(validate_selection("all").is_ok());
        assert!(validate_selection("claude,codex-2").is_ok());
        assert!(validate_selection("../escape").is_err());
        assert!(validate_selection("Claude").is_err());
        assert!(validate_selection(&vec!["kit"; 17].join(",")).is_err());
        assert!(validate_selection(&"a".repeat(1025)).is_err());
        assert!(validate_selector("519").is_ok());
        assert!(validate_selector("2f41f39a").is_ok());
        assert!(validate_selector("../../token").is_err());
        assert!(validate_operation_id(&Uuid::new_v4().to_string()).is_ok());
        assert!(validate_operation_id("not-an-operation").is_err());
    }

    #[test]
    fn creates_owner_only_canonical_home() {
        let (_root, config) = fixture();
        assert!(config.workspace.is_absolute());
        assert!(config.home.is_absolute());
        assert_eq!(
            config.home.metadata().unwrap().permissions().mode() & 0o077,
            0
        );
    }

    #[test]
    fn mcp_first_start_creates_daemon_compatible_product_directory() {
        let root = tempfile::tempdir().unwrap();
        let product = root
            .path()
            .join("Library/Application Support/marsh-mcp-scopes");
        let scope = product.join("workspace/default");
        prepare_home(&scope).unwrap();
        for path in [
            root.path().join("Library"),
            root.path().join("Library/Application Support"),
            product,
            scope,
        ] {
            let metadata = fs::symlink_metadata(path).unwrap();
            assert!(metadata.file_type().is_dir());
            assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        }
    }

    #[test]
    fn preexisting_permissive_product_directory_requires_explicit_repair() {
        let root = tempfile::tempdir().unwrap();
        let product = root
            .path()
            .join("Library/Application Support/marsh-mcp-scopes");
        fs::create_dir_all(&product).unwrap();
        fs::set_permissions(&product, fs::Permissions::from_mode(0o755)).unwrap();
        let scope = product.join("workspace/default");
        let error = prepare_home(&scope).unwrap_err();
        assert!(error.contains("mode 0700"), "{error}");
        assert!(error.contains("repair its permissions"), "{error}");
        assert!(!product.join("workspace").exists());
        assert_eq!(
            fs::symlink_metadata(product).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn symlinked_product_directory_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let app_support = root.path().join("Library/Application Support");
        fs::create_dir_all(&app_support).unwrap();
        let replacement = root.path().join("replacement");
        fs::create_dir(&replacement).unwrap();
        std::os::unix::fs::symlink(&replacement, app_support.join("marsh-mcp-scopes")).unwrap();
        let error =
            prepare_home(&app_support.join("marsh-mcp-scopes/workspace/default")).unwrap_err();
        assert!(error.contains("must not be a symlink"), "{error}");
        assert!(!replacement.join("workspace").exists());
    }

    #[test]
    fn default_home_is_host_home_not_workspace() {
        let (workspace, home, _marsh) = default_paths().unwrap();
        let host_home = PathBuf::from(env::var_os("HOME").unwrap());
        let expected = env::var_os("MARSH_HOME")
            .filter(|value| !value.is_empty())
            .map_or_else(|| host_home.join(".marsh"), PathBuf::from);
        assert_eq!(home, expected);
        assert!(!is_at_or_beneath(&home, &workspace));
        let generated = default_home_for_workspace(&workspace).unwrap();
        let product = host_home.join("Library/Application Support/marsh");
        assert!(!generated.starts_with(&product), "{}", generated.display());
        assert!(
            generated
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .ends_with(if cfg!(target_os = "macos") {
                    "marsh-mcp-scopes"
                } else {
                    MCP_SCOPES_DIR
                })
                || env::var_os("MARSH_CONTROL_HOME").is_some()
        );

        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        assert_ne!(
            default_home_for_workspace(&first).unwrap(),
            default_home_for_workspace(&second).unwrap()
        );
    }

    #[test]
    fn managed_root_is_bound_to_one_canonical_workspace_before_home_use() {
        let root = tempfile::tempdir().unwrap();
        let first_workspace = root.path().join("first-workspace");
        let second_workspace = root.path().join("second-workspace");
        let scope_root = root.path().join("scopes");
        let default_home = scope_root.join("default");
        let marsh = root.path().join("bin/marsh");
        fs::create_dir(&first_workspace).unwrap();
        fs::create_dir(&second_workspace).unwrap();
        fs::create_dir(marsh.parent().unwrap()).unwrap();
        executable(&marsh, "#!/bin/sh\nexit 0\n");
        let sbx = supporting_executables(&marsh);

        let config = HostConfig::new_with_scope_root(
            &first_workspace,
            &default_home,
            &scope_root,
            &marsh,
            &sbx,
            false,
        )
        .unwrap();
        assert_eq!(
            fs::metadata(scope_root.join("workspace.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0
        );
        drop(config);
        for _ in 0..32 {
            let reacquired = HostConfig::new_with_scope_root(
                &first_workspace,
                &default_home,
                &scope_root,
                &marsh,
                &sbx,
                false,
            )
            .expect("the last Arc owner must release the root lease immediately");
            drop(reacquired);
        }
        fs::remove_dir_all(&default_home).unwrap();

        let error = HostConfig::new_with_scope_root(
            &second_workspace,
            &default_home,
            &scope_root,
            &marsh,
            &sbx,
            false,
        )
        .unwrap_err();
        assert!(
            error.contains("bound to a different canonical workspace identity"),
            "{error}"
        );
        assert!(
            !default_home.exists(),
            "mismatched startup recreated the default home"
        );
    }

    #[test]
    fn populated_unbound_root_refuses_automatic_migration() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let scope_root = root.path().join("scopes");
        let default_home = scope_root.join("default");
        let marsh = root.path().join("bin/marsh");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&scope_root).unwrap();
        fs::set_permissions(&scope_root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(scope_root.join("legacy-state"), b"unknown").unwrap();
        fs::create_dir(marsh.parent().unwrap()).unwrap();
        executable(&marsh, "#!/bin/sh\nexit 0\n");
        let sbx = supporting_executables(&marsh);

        let error = HostConfig::new_with_scope_root(
            &workspace,
            &default_home,
            &scope_root,
            &marsh,
            &sbx,
            false,
        )
        .unwrap_err();
        assert!(
            error.contains("refusing unsafe automatic migration"),
            "{error}"
        );
        assert!(!default_home.exists());
        assert!(!scope_root.join("workspace.json").exists());
    }

    #[test]
    fn restart_reaps_only_safe_stale_registry_temps() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let scope_root = root.path().join("scopes");
        let default_home = scope_root.join("default");
        let marsh = root.path().join("bin/marsh");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(marsh.parent().unwrap()).unwrap();
        executable(&marsh, "#!/bin/sh\nexit 0\n");
        let sbx = supporting_executables(&marsh);
        let config = HostConfig::new_with_scope_root(
            &workspace,
            &default_home,
            &scope_root,
            &marsh,
            &sbx,
            false,
        )
        .unwrap();
        drop(config);
        let stale = scope_root.join(".scopes-crashed.tmp");
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        options.open(&stale).unwrap().write_all(b"partial").unwrap();

        let config = HostConfig::new_with_scope_root(
            &workspace,
            &default_home,
            &scope_root,
            &marsh,
            &sbx,
            false,
        )
        .unwrap();
        assert!(!stale.exists());
        drop(config);
    }

    #[test]
    fn restart_never_recreates_or_recaptures_persisted_scope_homes() {
        for replace in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let workspace = root.path().join("workspace");
            let scope_root = root.path().join("scopes");
            let default_home = scope_root.join("default");
            let marsh = root.path().join("bin/marsh");
            fs::create_dir(&workspace).unwrap();
            fs::create_dir(marsh.parent().unwrap()).unwrap();
            executable(&marsh, "#!/bin/sh\nexit 0\n");
            let sbx = supporting_executables(&marsh);
            let config = HostConfig::new_with_scope_root(
                &workspace,
                &default_home,
                &scope_root,
                &marsh,
                &sbx,
                false,
            )
            .unwrap();
            let scope_root = config.generated_scope_root.clone().unwrap();
            let scope_id = Uuid::new_v4().to_string();
            let home = prepare_scope_home(&scope_root, &scope_id).unwrap();
            let old_identity = DirectoryIdentity::capture(&home).unwrap();
            let mut scopes = config.persisted_scopes.clone();
            scopes.insert(
                scope_id.clone(),
                DevelopmentScope {
                    home: home.clone(),
                    home_identity: old_identity.clone(),
                    state: ScopeState::Stopped,
                    created_unix_ms: now_ms(),
                    last_operation_id: None,
                    diagnostic: None,
                },
            );
            persist_scope_registry(&scope_root, &scopes, &BTreeMap::new()).unwrap();
            drop(config);
            if replace {
                fs::rename(&home, scope_root.join(format!("{scope_id}.old"))).unwrap();
                fs::create_dir(&home).unwrap();
                fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
                assert_ne!(DirectoryIdentity::capture(&home).unwrap(), old_identity);
            } else {
                fs::remove_dir(&home).unwrap();
            }

            let restarted = HostConfig::new_with_scope_root(
                &workspace,
                &default_home,
                &scope_root,
                &marsh,
                &sbx,
                false,
            )
            .unwrap();
            let isolated = restarted.persisted_scopes.get(&scope_id).unwrap();
            assert_eq!(isolated.state, ScopeState::Failed);
            assert_eq!(isolated.home_identity, old_identity);
            assert_eq!(
                home.exists(),
                replace,
                "missing persisted home was recreated"
            );
            assert!(
                isolated
                    .diagnostic
                    .as_deref()
                    .unwrap()
                    .contains("persisted scope home")
            );
        }
    }

    #[test]
    fn restart_does_not_recreate_a_missing_persisted_default_home() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let scope_root = root.path().join("scopes");
        let default_home = scope_root.join("default");
        let marsh = root.path().join("bin/marsh");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(marsh.parent().unwrap()).unwrap();
        executable(&marsh, "#!/bin/sh\nexit 0\n");
        let sbx = supporting_executables(&marsh);
        let config = HostConfig::new_with_scope_root(
            &workspace,
            &default_home,
            &scope_root,
            &marsh,
            &sbx,
            false,
        )
        .unwrap();
        let canonical_home = config.home.clone();
        let identity = config.home_identity.clone();
        drop(config);
        fs::remove_dir(&canonical_home).unwrap();

        let restarted = HostConfig::new_with_scope_root(
            &workspace,
            &default_home,
            &scope_root,
            &marsh,
            &sbx,
            false,
        )
        .unwrap();
        let isolated = restarted.persisted_scopes.get("default").unwrap();
        assert_eq!(isolated.state, ScopeState::Failed);
        assert_eq!(isolated.home_identity, identity);
        assert!(!canonical_home.exists());
    }

    #[test]
    fn registry_decode_error_names_the_registry_path() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let scope_root = root.path().join("scopes");
        let default_home = scope_root.join("default");
        let marsh = root.path().join("bin/marsh");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(marsh.parent().unwrap()).unwrap();
        executable(&marsh, "#!/bin/sh\nexit 0\n");
        let sbx = supporting_executables(&marsh);
        let config = HostConfig::new_with_scope_root(
            &workspace,
            &default_home,
            &scope_root,
            &marsh,
            &sbx,
            false,
        )
        .unwrap();
        drop(config);
        let registry = scope_root.join("scopes.json");
        fs::write(&registry, b"not json").unwrap();
        fs::set_permissions(&registry, fs::Permissions::from_mode(0o600)).unwrap();

        let error = HostConfig::new_with_scope_root(
            &workspace,
            &default_home,
            &scope_root,
            &marsh,
            &sbx,
            false,
        )
        .unwrap_err();
        assert!(error.contains(&registry.display().to_string()), "{error}");
    }

    #[test]
    fn rejects_home_beneath_workspace() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let home = workspace.join("mcp-home");
        let marsh = root.path().join("marsh");
        fs::create_dir(&workspace).unwrap();
        executable(&marsh, "#!/bin/sh\nexit 0\n");
        let sbx = supporting_executables(&marsh);
        let error = HostConfig::new(&workspace, &home, &marsh, &sbx, false).unwrap_err();
        assert!(error.contains("workspace"), "{error}");
    }

    #[test]
    fn rejects_home_that_is_workspace_ancestor_or_symlink_to_ancestor() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let marsh = root.path().join("bin/marsh");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(marsh.parent().unwrap()).unwrap();
        executable(&marsh, "#!/bin/sh\nexit 0\n");
        let sbx = supporting_executables(&marsh);
        let error = HostConfig::new(&workspace, root.path(), &marsh, &sbx, false).unwrap_err();
        assert!(
            error.contains("must not overlap in either direction"),
            "{error}"
        );

        let link = root.path().join("home-link");
        std::os::unix::fs::symlink(root.path(), &link).unwrap();
        let error = HostConfig::new(&workspace, &link, &marsh, &sbx, false).unwrap_err();
        assert!(
            error.contains("must not overlap in either direction"),
            "{error}"
        );
    }

    #[test]
    fn rejects_marsh_beneath_workspace_or_selected_home() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let home = root.path().join("home");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();

        let workspace_marsh = workspace.join("target/release/marsh");
        fs::create_dir_all(workspace_marsh.parent().unwrap()).unwrap();
        executable(&workspace_marsh, "#!/bin/sh\nexit 0\n");
        let workspace_sbx = supporting_executables(&workspace_marsh);
        let error = HostConfig::new(&workspace, &home, &workspace_marsh, &workspace_sbx, false)
            .unwrap_err();
        assert!(error.contains("must be installed outside"));

        let home_marsh = home.join("bin/marsh");
        fs::create_dir_all(home_marsh.parent().unwrap()).unwrap();
        executable(&home_marsh, "#!/bin/sh\nexit 0\n");
        let home_sbx = supporting_executables(&home_marsh);
        let error = HostConfig::new(&workspace, &home, &home_marsh, &home_sbx, false).unwrap_err();
        assert!(error.contains("must be installed outside"));
    }

    #[test]
    fn rejects_relative_scope_paths() {
        assert_eq!(
            HostConfig::new(
                Path::new("workspace"),
                Path::new("home"),
                Path::new("marsh"),
                Path::new("sbx"),
                false,
            )
            .unwrap_err(),
            "workspace must be an absolute path"
        );
    }

    #[test]
    fn rejects_shared_home() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let home = root.path().join("home");
        let marsh = root.path().join("marsh");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
        executable(&marsh, "#!/bin/sh\nexit 0\n");
        let sbx = supporting_executables(&marsh);
        assert!(HostConfig::new(&workspace, &home, &marsh, &sbx, false).is_err());
    }

    #[test]
    fn timeouts_and_capture_are_bounded() {
        assert!(bounded_timeout(Some(9)).is_err());
        assert!(bounded_timeout(Some(MAX_TIMEOUT_MS + 1)).is_err());
        assert_eq!(
            bounded_timeout(None).unwrap(),
            Duration::from_millis(DEFAULT_TIMEOUT_MS)
        );
        let encoded = encoded_stream(b"hello");
        assert_eq!(encoded["byte_length"], 5);
        assert_eq!(encoded["base64"], "aGVsbG8=");
    }

    #[tokio::test]
    async fn transport_sessions_cannot_inspect_or_cancel_each_others_operations() {
        let (_root, config) = fixture();
        let owner = HostMcp::new(config);
        let peer = owner.new_session();
        let operation_id = Uuid::new_v4().to_string();
        let (cancel, mut cancelled) = watch::channel(false);
        owner.operations.lock().await.insert(
            operation_id.clone(),
            Operation {
                id: operation_id.clone(),
                owner_session: owner.session_id.clone(),
                scope_id: "default".into(),
                kind: "test".into(),
                state: OperationState::Running,
                started_unix_ms: now_ms(),
                finished_unix_ms: None,
                exit_code: None,
                stdout: b"private output".to_vec(),
                stderr: Vec::new(),
                stdout_truncated: false,
                stderr_truncated: false,
                cancel: Some(cancel),
            },
        );

        let peer_get = peer
            .operation_get(Parameters(OperationRequest {
                operation_id: operation_id.clone(),
            }))
            .await;
        assert!(!peer_get.0.ok);
        assert_eq!(
            peer_get.0.error.as_deref(),
            Some("unknown operation ID for this MCP session")
        );
        let peer_cancel = peer
            .operation_cancel(Parameters(OperationRequest {
                operation_id: operation_id.clone(),
            }))
            .await;
        assert!(!peer_cancel.0.ok);
        assert!(!*cancelled.borrow());

        let owner_get = owner
            .operation_get(Parameters(OperationRequest {
                operation_id: operation_id.clone(),
            }))
            .await;
        assert!(owner_get.0.ok);
        owner
            .operation_cancel(Parameters(OperationRequest { operation_id }))
            .await;
        cancelled.changed().await.unwrap();
        assert!(*cancelled.borrow());
    }

    #[tokio::test]
    async fn session_shutdown_cancels_only_its_own_operations() {
        let (_root, config) = fixture();
        let owner = HostMcp::new(config);
        let peer = owner.new_session();
        let owner_id = Uuid::new_v4().to_string();
        let peer_id = Uuid::new_v4().to_string();
        let (owner_cancel, mut owner_cancelled) = watch::channel(false);
        let (peer_cancel, peer_cancelled) = watch::channel(false);
        let operation = |id: String, session: String, cancel| Operation {
            id,
            owner_session: session,
            scope_id: "default".into(),
            kind: "test".into(),
            state: OperationState::Running,
            started_unix_ms: now_ms(),
            finished_unix_ms: None,
            exit_code: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            cancel: Some(cancel),
        };
        {
            let mut operations = owner.operations.lock().await;
            operations.insert(
                owner_id.clone(),
                operation(owner_id.clone(), owner.session_id.clone(), owner_cancel),
            );
            operations.insert(
                peer_id.clone(),
                operation(peer_id.clone(), peer.session_id.clone(), peer_cancel),
            );
        }
        let operations = Arc::clone(&owner.operations);
        let completed_owner_id = owner_id.clone();
        tokio::spawn(async move {
            owner_cancelled.changed().await.unwrap();
            let mut operations = operations.lock().await;
            let operation = operations.get_mut(&completed_owner_id).unwrap();
            operation.state = OperationState::Failed;
            operation.finished_unix_ms = Some(now_ms());
            operation.cancel = None;
        });

        owner.shutdown_session().await.unwrap();
        let operations = owner.operations.lock().await;
        assert_eq!(operations[&owner_id].state, OperationState::Failed);
        assert_eq!(operations[&peer_id].state, OperationState::Running);
        assert!(!*peer_cancelled.borrow());
    }

    #[tokio::test]
    async fn product_json_runs_in_fixed_scope_without_relay_credentials() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let home = root.path().join("home");
        let marsh = root.path().join("marsh");
        fs::create_dir(&workspace).unwrap();
        executable(
            &marsh,
            "#!/bin/sh\nprintf '{\"cwd\":\"%s\",\"home\":\"%s\",\"token\":\"%s\",\"sbx\":\"%s\",\"user\":\"%s\",\"logname\":\"%s\"}\\n' \"$PWD\" \"$MARSH_HOME\" \"${MARSH_DAEMON_TOKEN-unset}\" \"$MARSH_SBX\" \"${USER-unset}\" \"${LOGNAME-unset}\"\n",
        );
        let sbx = supporting_executables(&marsh);
        let config = HostConfig::new(&workspace, &home, &marsh, &sbx, false).unwrap();
        // The command observes the environment after env_clear. Checking the
        // immutable config value and exclusion from INHERITED_ENV proves these
        // values do not depend on optional parent USER/LOGNAME variables, without
        // process-global environment mutation in this test.
        let server = HostMcp::new(config.clone());
        let response = server
            .run_json(&["status", "--json"], Duration::from_secs(2))
            .await;
        assert!(response.ok);
        let data = response.data.unwrap();
        assert_eq!(data["cwd"], config.workspace.to_string_lossy().as_ref());
        assert_eq!(data["home"], config.home.to_string_lossy().as_ref());
        assert_eq!(data["token"], "unset");
        assert_eq!(data["sbx"], config.sbx.to_string_lossy().as_ref());
        assert_eq!(data["user"], config.username);
        assert_eq!(data["logname"], config.username);
        assert!(!INHERITED_ENV.contains(&"PATH"));
        assert!(!INHERITED_ENV.contains(&"MARSH_SBX"));
        assert!(!INHERITED_ENV.contains(&"USER"));
        assert!(!INHERITED_ENV.contains(&"LOGNAME"));
    }

    #[tokio::test]
    async fn same_scope_read_only_calls_run_concurrently() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let home = root.path().join("home");
        let marsh = root.path().join("marsh");
        fs::create_dir(&workspace).unwrap();
        executable(
            &marsh,
            "#!/bin/sh\ntouch \"$MARSH_HOME/read.$$\"\nwhile [ \"$(find \"$MARSH_HOME\" -name 'read.*' | wc -l)\" -lt 2 ]; do sleep 0.01; done\nprintf '%s\\n' '{\"schema\":\"test/v1\"}'\n",
        );
        let sbx = supporting_executables(&marsh);
        let config = HostConfig::new(&workspace, &home, &marsh, &sbx, false).unwrap();
        let server = HostMcp::new(config);
        let scope = server.active_scope("default").await.unwrap();
        let (first, second) = tokio::join!(
            server.run_json_in(
                "default",
                &scope,
                &["status", "--json"],
                Duration::from_secs(2)
            ),
            server.run_json_in(
                "default",
                &scope,
                &["results", "--json"],
                Duration::from_secs(2)
            )
        );
        assert!(first.ok);
        assert!(second.ok);
    }

    #[tokio::test]
    async fn stale_stopped_receipt_admission_cannot_stop_a_revived_scope() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let home = root.path().join("home");
        let marsh = root.path().join("marsh");
        let invoked = root.path().join("invoked");
        fs::create_dir(&workspace).unwrap();
        executable(
            &marsh,
            &format!(
                "#!/bin/sh\ntouch '{}'\nprintf '%s\\n' '{{\"schema\":\"test/v1\"}}'\n",
                invoked.display()
            ),
        );
        let sbx = supporting_executables(&marsh);
        let config = HostConfig::new(&workspace, &home, &marsh, &sbx, false).unwrap();
        let server = HostMcp::new(config);
        {
            let mut scopes = server.scopes.lock().await;
            scopes.get_mut("default").unwrap().state = ScopeState::Stopped;
        }
        let stale_stopped = server.readable_results_scope("default").await.unwrap();
        {
            let mut scopes = server.scopes.lock().await;
            scopes.get_mut("default").unwrap().state = ScopeState::Ready;
        }

        let response = server
            .run_stopped_results_in("default", &stale_stopped, &["results", "--json"])
            .await;

        assert!(!response.ok);
        assert!(
            response
                .error
                .unwrap()
                .contains("changed after receipt-read admission")
        );
        assert!(!invoked.exists(), "stale stopped read invoked marsh");
        assert_eq!(
            server.scopes.lock().await.get("default").unwrap().state,
            ScopeState::Ready
        );
    }

    #[tokio::test]
    async fn rejects_changed_or_symlinked_marsh_before_spawn() {
        let (_root, config) = fixture();
        fs::write(&config.marsh, "#!/bin/sh\nprintf '{\"unsafe\":true}\\n'\n").unwrap();
        let server = HostMcp::new(config.clone());
        let response = server
            .run_json(&["status", "--json"], Duration::from_secs(2))
            .await;
        assert!(!response.ok);
        assert!(
            response
                .error
                .unwrap()
                .contains("changed after MCP startup")
        );

        fs::remove_file(&config.marsh).unwrap();
        std::os::unix::fs::symlink("/bin/true", &config.marsh).unwrap();
        let response = server
            .run_json(&["status", "--json"], Duration::from_secs(2))
            .await;
        assert!(!response.ok);
        assert!(response.error.unwrap().contains("no longer a regular file"));
    }

    #[tokio::test]
    async fn rejects_changed_sbx_or_sibling_daemon_before_spawn() {
        let (_root, config) = fixture();
        fs::write(&config.sbx, "#!/bin/sh\nexit 9\n").unwrap();
        let server = HostMcp::new(config);
        let response = server
            .run_json(&["status", "--json"], Duration::from_secs(2))
            .await;
        assert!(!response.ok);
        assert!(
            response
                .error
                .unwrap()
                .contains("configured sbx executable changed")
        );

        let (_root, config) = fixture();
        fs::write(&config.marshd, "#!/bin/sh\nexit 9\n").unwrap();
        let server = HostMcp::new(config);
        let response = server
            .run_json(&["status", "--json"], Duration::from_secs(2))
            .await;
        assert!(!response.ok);
        assert!(
            response
                .error
                .unwrap()
                .contains("configured sibling marshd executable changed")
        );
    }

    #[tokio::test]
    async fn rejects_post_start_mode_change_for_every_pinned_executable() {
        for index in 0..3 {
            let (_root, config) = fixture();
            let path = match index {
                0 => &config.marsh,
                1 => &config.marshd,
                _ => &config.sbx,
            };
            fs::set_permissions(path, fs::Permissions::from_mode(0o722)).unwrap();
            let server = HostMcp::new(config);
            let response = server
                .run_json(&["status", "--json"], Duration::from_secs(2))
                .await;
            assert!(!response.ok);
            assert!(
                response
                    .error
                    .unwrap()
                    .contains("must not be writable by group or other users")
            );
        }
    }

    #[test]
    fn sbx_accepts_a_group_writable_external_parent_and_pins_the_file() {
        let (_root, config) = fixture_with_group_writable_sbx_parent();
        assert!(config.sbx.ends_with("homebrew-caskroom/sbx"));
        config.validate_host_executables().unwrap();
    }

    #[tokio::test]
    async fn external_sbx_content_and_mode_changes_still_fail_closed() {
        let (_root, config) = fixture_with_group_writable_sbx_parent();
        fs::write(&config.sbx, "#!/bin/sh\nexit 9\n").unwrap();
        let response = HostMcp::new(config)
            .run_json(&["status", "--json"], Duration::from_secs(2))
            .await;
        assert!(!response.ok);
        assert!(
            response
                .error
                .unwrap()
                .contains("configured sbx executable changed")
        );

        let (_root, config) = fixture_with_group_writable_sbx_parent();
        fs::set_permissions(&config.sbx, fs::Permissions::from_mode(0o722)).unwrap();
        let response = HostMcp::new(config)
            .run_json(&["status", "--json"], Duration::from_secs(2))
            .await;
        assert!(!response.ok);
        assert!(
            response
                .error
                .unwrap()
                .contains("must not be writable by group or other users")
        );
    }

    #[test]
    fn marsh_rejects_a_group_or_other_writable_executable_parent() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let home = root.path().join("home");
        let unsafe_directory = root.path().join("unsafe-bin");
        let marsh = unsafe_directory.join("marsh");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&unsafe_directory).unwrap();
        fs::set_permissions(&unsafe_directory, fs::Permissions::from_mode(0o777)).unwrap();
        executable(&marsh, "#!/bin/sh\nexit 0\n");
        let sbx = supporting_executables(&marsh);
        let error = HostConfig::new(&workspace, &home, &marsh, &sbx, false).unwrap_err();
        assert!(error.contains("executable parent must not be writable"));
    }

    #[tokio::test]
    async fn inspection_returns_busy_when_global_capacity_is_full() {
        let (_root, config) = fixture();
        let server = HostMcp::new(config);
        let _permits = server
            .permits
            .acquire_many(u32::try_from(MAX_OPERATIONS).unwrap())
            .await
            .unwrap();
        let response = server
            .run_json(&["status", "--json"], Duration::from_secs(2))
            .await;
        assert!(!response.ok);
        assert!(response.error.unwrap().contains("capacity is busy"));
    }

    #[tokio::test]
    async fn new_operation_returns_busy_when_global_capacity_is_full() {
        let (_root, config) = fixture();
        let server = HostMcp::new(config.clone());
        let _permits = Arc::clone(&server.permits)
            .acquire_many_owned(u32::try_from(MAX_OPERATIONS).unwrap())
            .await
            .unwrap();
        let response = server
            .start_operation(
                "default",
                "busy-test",
                CommandSpec {
                    executable: config.marsh,
                    arguments: vec![],
                    timeout: Duration::from_secs(2),
                    home_identity: DirectoryIdentity::capture(&config.home).unwrap(),
                    home: config.home,
                },
                Vec::new(),
                false,
            )
            .await;
        assert!(!response.ok);
        assert!(response.error.unwrap().contains("capacity is busy"));
        assert!(server.operations.lock().await.is_empty());
    }

    #[tokio::test]
    async fn full_sbx_control_is_disabled_by_default() {
        let (_root, config) = fixture();
        let server = HostMcp::new(config);
        let Json(response) = server
            .qualify(Parameters(QualifyRequest {
                gate: "source".into(),
                timeout_ms: None,
            }))
            .await;
        assert!(!response.ok);
        assert!(response.error.unwrap().contains("--allow-full-sbx-control"));
        assert!(server.operations.lock().await.is_empty());
    }

    #[tokio::test]
    async fn operations_return_bounded_terminal_output() {
        let (_root, config) = fixture();
        let server = HostMcp::new(config.clone());
        let response = server
            .start_operation(
                "default",
                "test",
                CommandSpec {
                    executable: config.marsh,
                    arguments: vec![],
                    timeout: Duration::from_secs(2),
                    home_identity: DirectoryIdentity::capture(&config.home).unwrap(),
                    home: config.home,
                },
                Vec::new(),
                false,
            )
            .await;
        let id = response.data.unwrap()["operation_id"]
            .as_str()
            .unwrap()
            .to_owned();
        for _ in 0..100 {
            let terminal = {
                let operations = server.operations.lock().await;
                matches!(
                    operations.get(&id).unwrap().state,
                    OperationState::Succeeded | OperationState::Failed
                )
            };
            if terminal {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let operations = server.operations.lock().await;
        let operation = operations.get(&id).unwrap();
        assert_eq!(operation.state, OperationState::Succeeded);
        assert!(String::from_utf8_lossy(&operation.stdout).contains("test/v1"));
    }

    #[tokio::test]
    async fn stream_capture_drains_but_retains_only_the_hard_cap() {
        let (mut writer, reader) = tokio::io::duplex(8192);
        let writer_task = tokio::spawn(async move {
            writer
                .write_all(&vec![b'x'; MAX_CAPTURE_BYTES + 17_000])
                .await
                .unwrap();
        });
        let (captured, truncated) = read_bounded_stream(reader).await.unwrap();
        writer_task.await.unwrap();
        assert!(truncated);
        assert_eq!(captured.len(), MAX_CAPTURE_BYTES);
    }

    #[tokio::test]
    async fn cancellation_interrupts_the_process_group_before_escalating() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let home = root.path().join("home");
        let marsh = root.path().join("marsh");
        let marker = root.path().join("interrupted");
        let ready = root.path().join("ready");
        fs::create_dir(&workspace).unwrap();
        executable(
            &marsh,
            &format!(
                "#!/bin/sh\ntrap 'printf handled > {} ; exit 0' INT TERM\nprintf ready > {}\nwhile :; do :; done\n",
                marker.display(),
                ready.display()
            ),
        );
        let sbx = supporting_executables(&marsh);
        let config = HostConfig::new(&workspace, &home, &marsh, &sbx, false).unwrap();
        let server = HostMcp::new(config.clone());
        let response = server
            .start_operation(
                "default",
                "cancel-test",
                CommandSpec {
                    executable: config.marsh,
                    arguments: vec![],
                    timeout: Duration::from_secs(30),
                    home_identity: DirectoryIdentity::capture(&config.home).unwrap(),
                    home: config.home,
                },
                Vec::new(),
                false,
            )
            .await;
        let id = response.data.unwrap()["operation_id"]
            .as_str()
            .unwrap()
            .to_owned();
        for _ in 0..100 {
            if ready.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready.exists());
        server.shutdown_all().await.unwrap();
        assert_eq!(
            server.operations.lock().await.get(&id).unwrap().state,
            OperationState::CancellationUncertain
        );
        assert_eq!(fs::read_to_string(marker).unwrap(), "handled");
    }

    #[tokio::test]
    async fn pipeline_cancellation_terminates_before_interrupting() {
        let root = tempfile::tempdir().unwrap();
        let program = root.path().join("pipeline-child");
        let ready = root.path().join("ready");
        let signal = root.path().join("signal");
        executable(
            &program,
            &format!(
                "#!/bin/sh\ntrap 'printf INT > {} ; exit 0' INT\ntrap 'printf TERM > {} ; exit 0' TERM\nprintf ready > {}\nwhile :; do :; done\n",
                signal.display(),
                signal.display(),
                ready.display()
            ),
        );
        // This test covers signal ordering, not executing a freshly written file.
        // Read the fixture through sh to avoid Linux ETXTBSY during concurrent
        // fixture startup; the process-group and trap oracle remain unchanged.
        let mut child = Command::new("/bin/sh")
            .arg(&program)
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        for _ in 0..100 {
            if ready.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready.exists());
        terminate_pipeline_process_group(pid, &mut child)
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(signal).unwrap(), "TERM");
    }

    #[tokio::test]
    async fn conflicting_lifecycle_operation_returns_busy() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let home = root.path().join("home");
        let marsh = root.path().join("marsh");
        let guard = root.path().join("lifecycle.guard");
        fs::create_dir(&workspace).unwrap();
        executable(
            &marsh,
            &format!(
                "#!/bin/sh\nmkdir {} || exit 44\nsleep 0.15\nrmdir {}\n",
                guard.display(),
                guard.display()
            ),
        );
        let sbx = supporting_executables(&marsh);
        let config = HostConfig::new(&workspace, &home, &marsh, &sbx, false).unwrap();
        let server = HostMcp::new(config.clone());
        let spec = CommandSpec {
            executable: config.marsh.clone(),
            arguments: vec![],
            timeout: Duration::from_secs(2),
            home_identity: DirectoryIdentity::capture(&config.home).unwrap(),
            home: config.home.clone(),
        };
        let first = server
            .start_operation("default", "lifecycle-test", spec.clone(), Vec::new(), true)
            .await;
        let first_id = first.data.unwrap()["operation_id"]
            .as_str()
            .unwrap()
            .to_owned();
        for _ in 0..100 {
            if guard.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(guard.exists());
        let conflict = server
            .start_operation("default", "lifecycle-test", spec.clone(), Vec::new(), true)
            .await;
        assert!(!conflict.ok);
        assert!(
            conflict
                .error
                .unwrap()
                .contains("development scope is busy")
        );
        for _ in 0..200 {
            let complete = {
                let operations = server.operations.lock().await;
                operations.get(&first_id).unwrap().state == OperationState::Succeeded
            };
            if complete {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let following = server
            .start_operation("default", "lifecycle-test", spec, Vec::new(), true)
            .await;
        assert!(following.ok);
        let following_id = following.data.unwrap()["operation_id"]
            .as_str()
            .unwrap()
            .to_owned();
        for _ in 0..200 {
            if server
                .operations
                .lock()
                .await
                .get(&following_id)
                .unwrap()
                .state
                == OperationState::Succeeded
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            server
                .operations
                .lock()
                .await
                .get(&following_id)
                .unwrap()
                .state,
            OperationState::Succeeded
        );
    }

    #[tokio::test]
    async fn lifecycle_admission_failure_rolls_back_scope_state() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let home = root.path().join("home");
        let marsh = root.path().join("marsh");
        fs::create_dir(&workspace).unwrap();
        executable(&marsh, "#!/bin/sh\nexit 0\n");
        let sbx = supporting_executables(&marsh);
        let config = HostConfig::new(&workspace, &home, &marsh, &sbx, false).unwrap();
        let server = HostMcp::new(config.clone());

        {
            let mut operations = server.operations.lock().await;
            for index in 0..MAX_RETAINED_OPERATIONS {
                let (cancel, _cancel_rx) = watch::channel(false);
                let id = format!("retained-{index}");
                operations.insert(
                    id.clone(),
                    Operation {
                        id,
                        owner_session: server.session_id.clone(),
                        scope_id: "peer".into(),
                        kind: "test".into(),
                        state: OperationState::Queued,
                        started_unix_ms: index as u128,
                        finished_unix_ms: None,
                        exit_code: None,
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                        stdout_truncated: false,
                        stderr_truncated: false,
                        cancel: Some(cancel),
                    },
                );
            }
        }

        let response = server
            .start_operation(
                "default",
                "scope_stop",
                CommandSpec {
                    executable: config.marsh,
                    arguments: vec![],
                    timeout: Duration::from_secs(2),
                    home_identity: DirectoryIdentity::capture(&config.home).unwrap(),
                    home: config.home,
                },
                Vec::new(),
                true,
            )
            .await;

        assert!(!response.ok);
        assert!(response.error.unwrap().contains("retention limit"));
        assert_eq!(
            server.scopes.lock().await.get("default").unwrap().state,
            ScopeState::Ready
        );
    }

    #[tokio::test]
    async fn scope_removal_respects_global_operation_capacity_before_transition() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let scope_root = root.path().join("scopes");
        let default_home = scope_root.join("default");
        let marsh = root.path().join("marsh");
        fs::create_dir(&workspace).unwrap();
        executable(&marsh, "#!/bin/sh\nexit 0\n");
        let sbx = supporting_executables(&marsh);
        let config = HostConfig::new_with_scope_root(
            &workspace,
            &default_home,
            &scope_root,
            &marsh,
            &sbx,
            false,
        )
        .unwrap();
        let scope_root = config.generated_scope_root.clone().unwrap();
        let server = HostMcp::new(config);
        let scope_id = Uuid::new_v4().to_string();
        let home = prepare_scope_home(&scope_root, &scope_id).unwrap();
        let scope = DevelopmentScope {
            home: home.clone(),
            home_identity: DirectoryIdentity::capture(&home).unwrap(),
            state: ScopeState::Stopped,
            created_unix_ms: now_ms(),
            last_operation_id: None,
            diagnostic: None,
        };
        {
            let mut scopes = server.scopes.lock().await;
            scopes.insert(scope_id.clone(), scope);
            persist_scope_registry(&scope_root, &scopes, &BTreeMap::new()).unwrap();
        }
        let _permits = Arc::clone(&server.permits)
            .acquire_many_owned(u32::try_from(MAX_OPERATIONS).unwrap())
            .await
            .unwrap();

        let response = server.start_scope_removal(&scope_id).await;

        assert!(!response.ok);
        assert!(response.error.unwrap().contains("capacity is busy"));
        assert_eq!(
            server.scopes.lock().await.get(&scope_id).unwrap().state,
            ScopeState::Stopped
        );
        assert!(home.is_dir());
    }

    #[tokio::test]
    async fn successful_scope_removal_releases_lifecycle_registry_entry() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let scope_root = root.path().join("scopes");
        let default_home = scope_root.join("default");
        let marsh = root.path().join("marsh");
        fs::create_dir(&workspace).unwrap();
        executable(&marsh, "#!/bin/sh\nexit 0\n");
        let sbx = supporting_executables(&marsh);
        let config = HostConfig::new_with_scope_root(
            &workspace,
            &default_home,
            &scope_root,
            &marsh,
            &sbx,
            false,
        )
        .unwrap();
        let scope_root = config.generated_scope_root.clone().unwrap();
        let server = HostMcp::new(config);
        let scope_id = Uuid::new_v4().to_string();
        let home = prepare_scope_home(&scope_root, &scope_id).unwrap();
        let external = root.path().join("external-sentinel");
        fs::write(&external, b"preserved").unwrap();
        std::os::unix::fs::symlink(&external, home.join("external-link")).unwrap();
        let scope = DevelopmentScope {
            home: home.clone(),
            home_identity: DirectoryIdentity::capture(&home).unwrap(),
            state: ScopeState::Stopped,
            created_unix_ms: now_ms(),
            last_operation_id: None,
            diagnostic: None,
        };
        {
            let mut scopes = server.scopes.lock().await;
            scopes.insert(scope_id.clone(), scope);
            persist_scope_registry(&scope_root, &scopes, &BTreeMap::new()).unwrap();
        }

        let response = server.start_scope_removal(&scope_id).await;
        assert!(response.ok);
        let operation_id = response.data.unwrap()["operation_id"]
            .as_str()
            .unwrap()
            .to_owned();
        for _ in 0..200 {
            let terminal = server
                .operations
                .lock()
                .await
                .get(&operation_id)
                .is_some_and(|operation| operation.state == OperationState::Succeeded);
            if terminal && !server.lifecycles.lock().await.contains_key(&scope_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(
            server
                .operations
                .lock()
                .await
                .get(&operation_id)
                .unwrap()
                .state,
            OperationState::Succeeded
        );
        assert!(!server.lifecycles.lock().await.contains_key(&scope_id));
        assert!(!home.exists());
        assert_eq!(fs::read(external).unwrap(), b"preserved");
    }

    #[test]
    fn scope_home_removal_refuses_over_limit_before_mutating() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("scope");
        fs::create_dir(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(home.join("one"), b"one").unwrap();
        fs::write(home.join("two"), b"two").unwrap();
        let identity = DirectoryIdentity::capture(&home).unwrap();

        let error = remove_exact_scope_home_with_limits(&home, &identity, 1, 8).unwrap_err();

        assert!(error.contains("bounded entry limit"), "{error}");
        assert_eq!(fs::read(home.join("one")).unwrap(), b"one");
        assert_eq!(fs::read(home.join("two")).unwrap(), b"two");
    }

    #[test]
    fn stopped_receipt_cleanup_uses_full_scope_lifecycle_bound() {
        assert_eq!(SCOPE_LIFECYCLE_TIMEOUT, Duration::from_mins(10));
        assert!(SCOPE_LIFECYCLE_TIMEOUT > Duration::from_mins(1));
    }
}
