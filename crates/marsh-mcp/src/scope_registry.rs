//! The persisted development-scope registry: the scope root and its lease, the
//! workspace binding, loading and reconciling recorded scopes, and removing a
//! scope's home.

use super::{
    Arc, BTreeMap, CapturedOutput, Deserialize, DevelopmentScope, DirBuilderExt, DirectoryIdentity,
    MAX_RETAINED_SCOPES, MAX_SCOPE_REMOVAL_DEPTH, MAX_SCOPE_REMOVAL_ENTRIES,
    MAX_WORKSPACE_BINDING_BYTES, MetadataExt, Mutex, OpenOptionsExt, Operation, OwnedMutexGuard,
    OwnedRwLockWriteGuard, OwnedSemaphorePermit, Path, PathBuf, PermissionsExt, RwLock, ScopeState,
    Serialize, Uuid, Value, Write, bounded_diagnostic, canonical_directory, finish_operation, fs,
    lifecycle_diagnostic, mcp_product_root, now_ms, resolve_without_creation, set_running,
    validate_scope_id,
};

#[derive(Debug, Default)]
pub(crate) struct LoadedScopeRegistry {
    pub(crate) scopes: BTreeMap<String, DevelopmentScope>,
    pub(crate) preserved_records: BTreeMap<String, Value>,
    pub(crate) diagnostics: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct ScopeRootLease {
    pub(crate) file: fs::File,
}

impl Drop for ScopeRootLease {
    fn drop(&mut self) {
        // Closing the descriptor also releases flock, but perform the unlock
        // explicitly so an immediate successor never depends on close timing
        // or platform-specific descriptor lifetime details.
        let _ = rustix::fs::flock(&self.file, rustix::fs::FlockOperation::Unlock);
    }
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceBinding {
    pub(crate) version: u32,
    pub(crate) canonical_workspace: PathBuf,
    pub(crate) device: u64,
    pub(crate) inode: u64,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_scope_removal_task(
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

pub(crate) fn remove_exact_scope_home(
    home: &Path,
    expected: &DirectoryIdentity,
) -> Result<(), String> {
    remove_exact_scope_home_with_limits(
        home,
        expected,
        MAX_SCOPE_REMOVAL_ENTRIES,
        MAX_SCOPE_REMOVAL_DEPTH,
    )
}

#[allow(clippy::too_many_lines)]
pub(crate) fn remove_exact_scope_home_with_limits(
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

pub(crate) fn live_generated_scope_count(scopes: &BTreeMap<String, DevelopmentScope>) -> usize {
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

pub(crate) fn prepare_home(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("home must be an absolute path".into());
    }
    let product_root = mcp_product_root(path);
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

pub(crate) fn validate_mcp_product_root(path: &Path) -> Result<(), String> {
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

pub(crate) fn acquire_scope_root_lease(root: &Path) -> Result<Arc<ScopeRootLease>, String> {
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

pub(crate) fn workspace_binding(workspace: &Path) -> Result<WorkspaceBinding, String> {
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

pub(crate) fn establish_workspace_binding(root: &Path, workspace: &Path) -> Result<(), String> {
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

pub(crate) fn reap_stale_scope_registry_temps(root: &Path) -> Result<(), String> {
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

pub(crate) fn prepare_scope_home(root: &Path, scope_id: &str) -> Result<PathBuf, String> {
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

pub(crate) fn load_scope_registry(root: &Path) -> Result<LoadedScopeRegistry, String> {
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

pub(crate) fn reconcile_persisted_scope(
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

pub(crate) fn invalid_removing_home(home: &Path, expected: &DirectoryIdentity) -> Option<String> {
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

pub(crate) fn persist_scope_registry(
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
