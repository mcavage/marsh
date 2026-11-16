use super::*;
use crate::exec_identity::CaptureStage;
use crate::scope_registry::remove_exact_scope_home_with_limits;
use std::io::Write;
use std::os::fd::AsRawFd as _;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;

/// The time a test that only expects a command to succeed gives it, from
/// before the executables are verified until its output is read. It guards
/// against a hang and measures nothing: a 2 s guard failed these tests on
/// the first run on a cold machine. Tests of the timeout itself state
/// their own.
const SPAWN_BUDGET: Duration = Duration::from_secs(60);

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

async fn wait_for_file(path: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !path.exists() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Waits for operation `id` to leave Queued/Running and returns its state.
async fn terminal_state(server: &HostMcp, id: &str) -> OperationState {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let state = server.operations.lock().await.get(id).unwrap().state;
        if !matches!(state, OperationState::Queued | OperationState::Running) {
            return state;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "operation {id} still {state:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn operation_id(response: &ToolResponse) -> String {
    response.data.as_ref().unwrap()["operation_id"]
        .as_str()
        .unwrap()
        .to_owned()
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
    let error = prepare_home(&app_support.join("marsh-mcp-scopes/workspace/default")).unwrap_err();
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
    let error =
        HostConfig::new(&workspace, &home, &workspace_marsh, &workspace_sbx, false).unwrap_err();
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

/// The export server's Kit-command path starts the resident daemon: it must
/// see the pinned environment, never the gateway's.
#[test]
fn the_daemon_an_export_starts_gets_the_host_child_environment() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let home = root.path().join("home");
    let marsh = root.path().join("marsh");
    fs::create_dir(&workspace).unwrap();
    executable(&marsh, "#!/bin/sh\nexit 0\n");
    let sbx = supporting_executables(&marsh);
    let marshd = root.path().join("marshd");
    executable(&marshd, "#!/bin/sh\n/usr/bin/env > \"$0.env\"\nexit 1\n");
    let config = HostConfig::new(&workspace, &home, &marsh, &sbx, false).unwrap();

    assert!(start_daemon(&config).is_err(), "the stand-in exits");

    let seen = fs::read_to_string(marshd.with_extension("env")).unwrap();
    let allowed = [
        "MARSH_HOME",
        "MARSH_SBX",
        "USER",
        "LOGNAME",
        "PATH",
        "MARSH_INTERNAL_STARTUP_STDERR",
        // The shell itself adds these.
        "PWD",
        "OLDPWD",
        "SHLVL",
        "_",
    ];
    for name in seen.lines().filter_map(|line| line.split_once('=')) {
        assert!(
            allowed.contains(&name.0)
                || INHERITED_ENV.contains(&name.0)
                || marsh_runtime::STOCK_SBX_SELECTORS.contains(&name.0),
            "the daemon inherited {}: {seen}",
            name.0
        );
    }
    let path = format!("PATH={}", fixed_minimal_path().to_string_lossy());
    assert!(seen.lines().any(|line| line == path), "{seen}");
    assert!(
        seen.lines()
            .any(|line| line == format!("USER={}", config.username))
    );
}

#[test]
fn the_daemon_environment_keeps_tuning_and_drops_everything_else() {
    let (_root, config) = fixture();
    let gateway = std::collections::BTreeMap::from([
        ("MARSH_TREE_LAUNCH_RATE", "5/s"),
        ("MARSH_AUTO_UPDATE", "0"),
        ("MARSH_VM_IDLE_SECONDS", "60"),
        // None of these reaches a daemon that outlives the gateway.
        ("SSH_AUTH_SOCK", "/tmp/agent.sock"),
        ("HTTPS_PROXY", "http://proxy.example"),
        ("DOCKER_HOST", "tcp://elsewhere"),
        ("MARSH_DAEMON_TOKEN", "secret"),
        ("MARSH_SHELL_IMAGE", "evil.example/image"),
        ("MARSH_GUEST_ARTIFACTS", "/tmp/evil"),
        ("XDG_DATA_HOME", "/tmp/data"),
    ]);
    let environment = daemon_environment(&config, &|name| {
        gateway.get(name).map(std::ffi::OsString::from)
    });
    let names = environment
        .iter()
        .map(|(name, _)| name.to_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    for kept in [
        "MARSH_TREE_LAUNCH_RATE",
        "MARSH_AUTO_UPDATE",
        "MARSH_VM_IDLE_SECONDS",
    ] {
        assert!(
            names.iter().any(|name| name == kept),
            "{kept} missing: {names:?}"
        );
    }
    for dropped in [
        "SSH_AUTH_SOCK",
        "HTTPS_PROXY",
        "DOCKER_HOST",
        "MARSH_DAEMON_TOKEN",
        "MARSH_SHELL_IMAGE",
        "MARSH_GUEST_ARTIFACTS",
        "XDG_DATA_HOME",
    ] {
        assert!(
            !names.iter().any(|name| name == dropped),
            "{dropped} leaked: {names:?}"
        );
    }
    for pinned in ["MARSH_HOME", "MARSH_SBX", "USER", "LOGNAME", "PATH"] {
        assert!(names.iter().any(|name| name == pinned), "{pinned} missing");
    }
}

/// The identity pins one file: the bytes hashed and the inode, mode and
/// times recorded come from the same object, whatever replaces the path
/// while it is being verified.
#[test]
fn an_executable_identity_never_mixes_two_files() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("tool");
    let other = root.path().join("other");
    executable(&path, "#!/bin/sh\necho original\n");
    executable(&other, "#!/bin/sh\necho replacement, longer\n");
    let digest = |file: &Path| -> [u8; 32] { Sha256::digest(fs::read(file).unwrap()).into() };
    let (original_hash, replacement_hash) = (digest(&path), digest(&other));
    let original_inode = fs::metadata(&path).unwrap().ino();
    let replacement_inode = fs::metadata(&other).unwrap().ino();

    let result = ExecutableIdentity::capture_observed(&path, false, &mut |stage| {
        if stage == CaptureStage::Opened {
            fs::rename(&other, &path).unwrap();
        }
    });

    // The path no longer names the file that was hashed: fail closed, never
    // a half-and-half identity.
    let error = result.expect_err("a replaced path must fail the check");
    assert!(error.contains("replaced or modified"), "{error}");
    assert_ne!(original_inode, replacement_inode);
    assert_ne!(original_hash, replacement_hash);
}

/// A swap after the post-hash `fstat`: the opened file is untouched (same
/// inode, same ctime), so only the path-versus-descriptor comparison can
/// see that the path now names something else.
#[test]
fn a_path_swapped_after_the_last_descriptor_check_fails_the_check() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("tool");
    let other = root.path().join("other");
    executable(&path, "#!/bin/sh\necho original\n");
    executable(&other, "#!/bin/sh\necho replacement\n");
    let error = ExecutableIdentity::capture_observed(&path, false, &mut |stage| {
        if stage == CaptureStage::Hashed {
            fs::rename(&other, &path).unwrap();
        }
    })
    .expect_err("the path no longer names the hashed file");
    assert!(error.contains("replaced or modified"), "{error}");
}

/// Renaming an ancestor directory and moving another in its place changes
/// what the path names without touching the opened file's metadata.
#[test]
fn an_ancestor_directory_swapped_while_verified_fails_the_check() {
    for swap_at in [CaptureStage::Opened, CaptureStage::Hashed] {
        let root = tempfile::tempdir().unwrap();
        let (live, spare) = (root.path().join("live"), root.path().join("spare"));
        fs::create_dir(&live).unwrap();
        fs::create_dir(&spare).unwrap();
        executable(&live.join("tool"), "#!/bin/sh\necho original\n");
        executable(&spare.join("tool"), "#!/bin/sh\necho replacement\n");
        let path = live.join("tool");
        let error = ExecutableIdentity::capture_observed(&path, false, &mut |stage| {
            if stage == swap_at {
                fs::rename(&live, root.path().join("moved")).unwrap();
                fs::rename(&spare, &live).unwrap();
            }
        })
        .expect_err("the path now names another file");
        assert!(
            error.contains("replaced or modified"),
            "{swap_at:?}: {error}"
        );
    }
}

#[test]
fn an_executable_changed_in_place_while_verified_fails_the_check() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("tool");
    executable(&path, "#!/bin/sh\necho original\n");
    let error = ExecutableIdentity::capture_observed(&path, false, &mut |stage| {
        if stage == CaptureStage::Opened {
            let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(b"# appended\n").unwrap();
        }
    })
    .expect_err("a file that changes while hashed must fail the check");
    assert!(error.contains("replaced or modified"), "{error}");

    // Untouched, the identity is the file's own.
    let identity = ExecutableIdentity::capture_observed(&path, false, &mut |_| {}).unwrap();
    assert_eq!(identity.inode, fs::metadata(&path).unwrap().ino());
    assert_eq!(
        identity.sha256,
        <[u8; 32]>::from(Sha256::digest(fs::read(&path).unwrap()))
    );
}

/// A pinned executable replaced by a FIFO is refused at once; opening it
/// for reading must not wait for a writer.
#[test]
fn a_fifo_in_place_of_a_pinned_executable_is_refused_not_waited_on() {
    let (_root, config) = fixture();
    fs::remove_file(&config.marsh).unwrap();
    let made = std::process::Command::new("/usr/bin/mkfifo")
        .arg(&config.marsh)
        .status()
        .unwrap();
    assert!(made.success());
    let (done, finished) = std::sync::mpsc::channel();
    let checking = config.clone();
    let worker = std::thread::spawn(move || {
        let _ = done.send(checking.validate_host_executables());
    });
    let result = finished.recv_timeout(Duration::from_secs(10));
    if result.is_err() {
        // Release a blocked open so the thread ends, then fail.
        let _ = fs::OpenOptions::new().write(true).open(&config.marsh);
    }
    worker.join().unwrap();
    let error = result
        .expect("validation waited for a writer on a FIFO")
        .expect_err("a FIFO is not the pinned executable");
    assert!(error.contains("regular file"), "{error}");
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
    let response = server.run_json(&["status", "--json"], SPAWN_BUDGET).await;
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

/// On macOS std creates a pipe or socket and only then marks it
/// close-on-exec, inside the shared host fence. A host command started
/// in that window would hold the descriptor (another command's output
/// pipe, a daemon connection) for its whole life, so it must wait.
#[tokio::test]
async fn host_commands_never_inherit_a_descriptor_being_created() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let home = root.path().join("home");
    let marsh = root.path().join("marsh");
    fs::create_dir(&workspace).unwrap();
    executable(
        &marsh,
        "#!/bin/sh\nprintf '{\"fds\":\"%s\"}\\n' \"$(ls /dev/fd | tr '\\n' ' ')\"\n",
    );
    let sbx = supporting_executables(&marsh);
    let server = HostMcp::new(HostConfig::new(&workspace, &home, &marsh, &sbx, false).unwrap());
    let (held, creating) = std::sync::mpsc::channel();
    let creator = std::thread::spawn(move || {
        marsh_runtime::with_host_descriptor_creation_excluded(|| {
            let null = fs::File::open("/dev/null").unwrap();
            let created = rustix::io::fcntl_dupfd_cloexec(&null, 200).unwrap();
            rustix::io::fcntl_setfd(&created, rustix::io::FdFlags::empty()).unwrap();
            held.send(created.as_raw_fd()).unwrap();
            std::thread::sleep(Duration::from_secs(1));
            rustix::io::fcntl_setfd(&created, rustix::io::FdFlags::CLOEXEC).unwrap();
            created
        })
    });
    let created = creating.recv().unwrap().to_string();
    let response = server
        .run_json(&["status", "--json"], Duration::from_secs(10))
        .await;
    drop(creator.join().unwrap());
    assert!(response.ok, "{:?}", response.error);
    let descriptors = response.data.unwrap()["fds"].as_str().unwrap().to_owned();
    assert!(
        !descriptors.split_whitespace().any(|fd| fd == created),
        "the host command inherited descriptor {created}: {descriptors}"
    );
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
        server.run_json_in("default", &scope, &["status", "--json"], SPAWN_BUDGET),
        server.run_json_in("default", &scope, &["results", "--json"], SPAWN_BUDGET)
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

#[cfg(target_os = "macos")]
#[test]
fn marsh_accepts_only_an_admin_group_writable_executable_parent() {
    // Mirrors stock Homebrew: /opt/homebrew/Cellar is 0775 user:admin.
    let build = |group: u32, mode: u32| {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let home = root.path().join("home");
        let bin = root.path().join("cellar");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&bin).unwrap();
        std::os::unix::fs::chown(&bin, None, Some(group)).ok()?;
        fs::set_permissions(&bin, fs::Permissions::from_mode(mode)).unwrap();
        let marsh = bin.join("marsh");
        executable(&marsh, "#!/bin/sh\nexit 0\n");
        let sbx = supporting_executables(&marsh);
        Some((
            HostConfig::new(&workspace, &home, &marsh, &sbx, false),
            root,
        ))
    };
    let Some((admin, _keep)) = build(80, 0o775) else {
        return; // the test user is not in the admin group
    };
    admin.unwrap();
    let (world, _keep) = build(80, 0o777).unwrap();
    assert!(world.unwrap_err().contains("must not be writable"));
    let (other, _keep) = build(rustix::process::getegid().as_raw(), 0o775).unwrap();
    assert!(other.unwrap_err().contains("must not be writable"));
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
                timeout: SPAWN_BUDGET,
                home_identity: DirectoryIdentity::capture(&config.home).unwrap(),
                home: config.home,
            },
            Vec::new(),
            false,
        )
        .await;
    let id = operation_id(&response);
    assert_eq!(
        terminal_state(&server, &id).await,
        OperationState::Succeeded
    );
    let operations = server.operations.lock().await;
    let operation = operations.get(&id).unwrap();
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

/// Cancellation interrupts the operation first; an operation that exits
/// on the interrupt is never sent the terminate that would follow.
#[tokio::test]
async fn cancellation_interrupts_first_and_stops_when_the_operation_exits() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let home = root.path().join("home");
    let marsh = root.path().join("marsh");
    let signals = root.path().join("signals");
    let ready = root.path().join("ready");
    fs::create_dir(&workspace).unwrap();
    executable(
        &marsh,
        &format!(
            "#!/bin/sh\ntrap 'printf INT >> {signals} ; exit 0' INT\ntrap 'printf TERM >> {signals} ; exit 0' TERM\nprintf ready > {ready}\nwhile :; do :; done\n",
            signals = signals.display(),
            ready = ready.display()
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
    let id = operation_id(&response);
    // The script writes `ready` after installing both traps.
    wait_for_file(&ready).await;
    server.shutdown_all().await.unwrap();
    assert_eq!(
        server.operations.lock().await.get(&id).unwrap().state,
        OperationState::CancellationUncertain
    );
    assert_eq!(fs::read_to_string(signals).unwrap(), "INT");
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
    wait_for_file(&ready).await;
    terminate_pipeline_process_group(pid, &mut child)
        .await
        .unwrap();
    assert_eq!(fs::read_to_string(signal).unwrap(), "TERM");
}

/// A lifecycle operation is refused while another holds the scope and
/// admitted once that one has finished.
#[tokio::test]
async fn conflicting_lifecycle_operation_returns_busy() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let home = root.path().join("home");
    let marsh = root.path().join("marsh");
    let running = root.path().join("running");
    let gate = root.path().join("gate");
    fs::create_dir(&workspace).unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&gate)
            .status()
            .unwrap()
            .success()
    );
    // Each run blocks until the test opens the gate for it.
    executable(
        &marsh,
        &format!(
            "#!/bin/sh\n: > {running}\nread release < {gate}\nrm {running}\n",
            running = running.display(),
            gate = gate.display()
        ),
    );
    let release = || {
        let gate = gate.clone();
        tokio::task::spawn_blocking(move || fs::write(gate, b"\n").unwrap())
    };
    let sbx = supporting_executables(&marsh);
    let config = HostConfig::new(&workspace, &home, &marsh, &sbx, false).unwrap();
    let server = HostMcp::new(config.clone());
    let spec = CommandSpec {
        executable: config.marsh.clone(),
        arguments: vec![],
        timeout: Duration::from_secs(30),
        home_identity: DirectoryIdentity::capture(&config.home).unwrap(),
        home: config.home.clone(),
    };
    let first = server
        .start_operation("default", "lifecycle-test", spec.clone(), Vec::new(), true)
        .await;
    let first_id = operation_id(&first);
    wait_for_file(&running).await;

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

    release().await.unwrap();
    assert_eq!(
        terminal_state(&server, &first_id).await,
        OperationState::Succeeded
    );
    let following = server
        .start_operation("default", "lifecycle-test", spec, Vec::new(), true)
        .await;
    assert!(following.ok, "{:?}", following.error);
    let following_id = operation_id(&following);
    release().await.unwrap();
    assert_eq!(
        terminal_state(&server, &following_id).await,
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
    let operation_id = operation_id(&response);
    assert_eq!(
        terminal_state(&server, &operation_id).await,
        OperationState::Succeeded
    );
    // The task releases its lifecycle entry just after recording success.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while server.lifecycles.lock().await.contains_key(&scope_id) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "lifecycle entry for {scope_id} was never released"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
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
