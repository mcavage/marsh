use marsh_acp::AgentRegistry;
use marsh_backend::{
    BackendConfig, RegisteredKit, StockDaemonBackend, command_registry::load_commands,
    config::EnvironmentConfig,
};
#[cfg(test)]
use marsh_contracts::JobResources;
use marsh_contracts::OciImage;
#[cfg(test)]
use marsh_daemon::DEFAULT_JOB_RESOURCES;
use marsh_daemon::{EndpointPaths, JobDefaults, Server};
use marsh_runtime::{CommandRunner, SystemCommandRunner};
use marsh_sbx::{ShellUser, ShellVmSpec, StockSbx, VmPurpose};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

fn main() {
    let arguments = env::args_os().skip(1).collect::<Vec<_>>();
    if arguments
        .first()
        .is_some_and(|word| word == "--prebuild-kit-images")
    {
        match prebuild_kit_images(&arguments[1..]) {
            Ok(()) => return,
            Err(error) => {
                eprintln!("marshd: {error}");
                std::process::exit(1);
            }
        }
    }
    if let Err(error) = run() {
        eprintln!("marshd: {error}");
        std::process::exit(1);
    }
}

#[allow(clippy::too_many_lines)] // Startup wires one host authority boundary.
fn run() -> Result<(), Box<dyn std::error::Error>> {
    let startup_lock = marsh_daemon::adopt_startup_lock()?;
    let real_home =
        marsh_backend::account_home::trusted_account_home(env::var_os("HOME").as_deref())?;
    let defaults = JobDefaults::from_environment(|name| env::var_os(name))?;
    let home = daemon_home()?;
    let control_home = host_control_home(
        &real_home,
        &home,
        env::var_os("MARSH_CONTROL_HOME")
            .map(PathBuf::from)
            .as_deref(),
    )?;
    let product_root = host_product_root(&real_home);
    // Resolve symlinks (e.g. Homebrew's bin/marshd -> Cellar/.../bin/marshd)
    // so the install prefix is the release tree that carries libexec/marsh.
    let executable = std::fs::canonicalize(env::current_exe()?)?;
    let host_bin = executable
        .parent()
        .ok_or("marshd has no parent directory")?;
    let install_prefix = host_bin.parent().ok_or("marshd has no install prefix")?;
    let guest_artifacts = env::var_os("MARSH_GUEST_ARTIFACTS")
        .map_or_else(|| install_prefix.join("libexec/marsh"), PathBuf::from);
    let commands = load_commands(
        &guest_artifacts.join("commands.json"),
        &control_home.join("commands.json"),
    )?;
    let agents = load_agents(
        &guest_artifacts.join("agents.json"),
        &control_home.join("agents.json"),
    )?;
    let mut protected_guest_roots =
        vec![product_root, control_home.clone(), guest_artifacts.clone()];
    protected_guest_roots.push(install_prefix.to_path_buf());
    let kit_image_cache = kit_image_cache_root(&real_home);
    // Superseded per-daemon cache (archives beside the lifecycle workspaces).
    let _ = fs::remove_dir_all(control_home.join("kit-lifecycle/.marsh-kit-image-cache"));
    protected_guest_roots.push(kit_image_cache.clone());
    if let Some(override_root) = env::var_os("MARSH_CONTROL_HOME") {
        protected_guest_roots.push(PathBuf::from(override_root).canonicalize()?);
    }
    protected_guest_roots.push(
        EndpointPaths::for_home(&home)?
            .runtime_directory
            .parent()
            .ok_or("daemon runtime root is unavailable")?
            .to_path_buf(),
    );
    protected_guest_roots.extend(
        commands
            .values()
            .filter_map(|kit| kit.workload.source_dir().map(Path::to_path_buf)),
    );
    let shell_image = env::var("MARSH_SHELL_IMAGE").unwrap_or_else(|_| {
        fs::read_to_string(guest_artifacts.join("shell-image"))
            .unwrap_or_default()
            .trim()
            .to_owned()
    });
    if shell_image.is_empty() {
        return Err("install must provide libexec/marsh/shell-image or MARSH_SHELL_IMAGE".into());
    }
    let shell_image = OciImage::parse(shell_image)?;
    // A dev install (`make dev`) carries a `dev-enabled` marker beside the
    // shell image; that install-time file enables `--dev` (the dev broker).
    // Production installs ship none, so their daemons expose no broker unless
    // started with MARSH_ENABLE_DEV_SCOPES=1. Dev shells use the one shell
    // image, which carries the dev tooling.
    let dev_enabled = env::var("MARSH_ENABLE_DEV_SCOPES").as_deref() == Ok("1")
        || guest_artifacts.join("dev-enabled").is_file();
    let username = env::var("USER").map_err(|_| "USER is required")?;
    let uid = rustix::process::getuid().as_raw();
    let gid = rustix::process::getgid().as_raw();
    let stock_sbx = resolve_sbx()?;
    let runner: Arc<dyn CommandRunner> = Arc::new(SystemCommandRunner::new(
        real_home.as_os_str().to_os_string(),
    ));
    if env::var_os("MARSH_LOCAL_SHELL_AUTHORITY").is_some() {
        return Err("MARSH_LOCAL_SHELL_AUTHORITY was removed; pass a local shell image tag via the normal shell image setting".into());
    }
    let stock_adapter = StockSbx::new(stock_sbx.clone(), runner)
        .with_kit_image_cache(kit_image_cache)
        // Inside `marsh --dev` every stock call crosses the dev broker.
        .with_published_image_cache(env::var_os("MARSH_DEV_DEPTH").is_none())
        .with_vm_ownership(control_home.join("vm-ownership.json"))?;
    let shell_name = stock_adapter.vm_name(VmPurpose::Shell, "shell")?;
    let shell = ShellVmSpec {
        name: shell_name,
        image: shell_image,
        shell_binary: guest_artifacts.join("marsh-linux-arm64"),
        user: ShellUser {
            name: username,
            uid,
            gid,
            home: real_home.clone(),
        },
    };
    let stock = Arc::new(stock_adapter);
    let templates = vec![shell.image.as_str().to_owned()];
    let dev_broker = Arc::new(marsh_daemon::DevBroker::new(
        Arc::clone(&stock),
        dev_enabled,
        dev_cache_root(&real_home),
        templates,
    ));
    let config = BackendConfig {
        worker_binary: guest_artifacts.join("marsh-worker-linux-arm64"),
        relay_binary: guest_artifacts.join("marsh-relay-linux-arm64"),
        daemon_home: home.clone(),
        control_home: control_home.clone(),
        protected_guest_roots: protected_guest_roots.clone(),
        shell,
        resources: defaults.resources,
        env: EnvironmentConfig::default(),
    };
    let backend =
        backend_with_agents(Arc::clone(&stock), commands, config, &agents).map_err(|error| {
            format!(
                "invalid ACP Kit binding ({error}); check {} and {} against commands.json",
                control_home.join("agents.json").display(),
                guest_artifacts.join("agents.json").display(),
            )
        })?;
    // All registry/configuration validation precedes even the stock version probe.
    stock.validate_supported_version().map_err(|_| {
        format!("stock SBX compatibility check failed for {}; requires stock SBX v0.45.0 or newer with local Kit v3 support; check `sbx version` and `sbx create --help`", stock_sbx.display())
    })?;
    // Grants never outlive a daemon: revoke and clean every persisted grant
    // (and retry uncertain ones) before serving.
    if !stock.dev_grants().is_empty()
        && let Err(error) = dev_broker.revoke_all()
    {
        eprintln!("marshd: {error}");
    }
    let backend = Arc::new(backend.with_dev_broker(Arc::clone(&dev_broker)));
    let server = Server::bind_with_stock_sbx_and_control_home(&home, &stock_sbx, &control_home)?
        .with_protected_guest_roots(protected_guest_roots)
        .with_job_defaults(defaults)
        .with_backend(backend)
        .with_dev_broker(dev_broker);
    let stopping = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGINT, Arc::clone(&stopping))?;
    signal_hook::flag::register(SIGTERM, Arc::clone(&stopping))?;
    marsh_daemon::finish_startup_diagnostics()?;
    drop(startup_lock);
    server.serve_until(|| stopping.load(Ordering::Relaxed))?;
    Ok(())
}

#[cfg(test)]
fn job_resources_from_environment(
    value: impl FnMut(&str) -> Option<std::ffi::OsString>,
) -> Result<JobResources, Box<dyn std::error::Error>> {
    Ok(JobDefaults::from_environment(value)?.resources)
}

/// Per-user verified Kit image archive cache (`MARSH_KIT_IMAGE_CACHE`
/// overrides), shared by this account's daemons; never guest-mounted.
fn kit_image_cache_root(real_home: &Path) -> PathBuf {
    if let Some(root) = env::var_os("MARSH_KIT_IMAGE_CACHE") {
        return PathBuf::from(root);
    }
    #[cfg(target_os = "macos")]
    {
        real_home.join("Library/Caches/marsh/kit-images")
    }
    #[cfg(not(target_os = "macos"))]
    {
        real_home.join(".cache/marsh/kit-images")
    }
}

/// `marshd --prebuild-kit-images GUEST_ARTIFACTS`: build every packaged
/// local Kit of that install's `commands.json` through the daemon's own
/// Buildx path into the Kit image cache (`make dev`). Unchanged sources are
/// skipped by fingerprint; two Kits build at a time.
fn prebuild_kit_images(arguments: &[std::ffi::OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let [artifacts] = arguments else {
        return Err("usage: marshd --prebuild-kit-images GUEST_ARTIFACTS".into());
    };
    let artifacts = PathBuf::from(artifacts).canonicalize()?;
    let real_home =
        marsh_backend::account_home::trusted_account_home(env::var_os("HOME").as_deref())?;
    let commands: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(artifacts.join("commands.json"))?)?;
    let mut sources = commands
        .values()
        .map(|location| artifacts.join(location))
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    sources.sort();
    sources.dedup();
    let cache = kit_image_cache_root(&real_home);
    let runner: Arc<dyn CommandRunner> = Arc::new(SystemCommandRunner::new(
        real_home.as_os_str().to_os_string(),
    ));
    let stock = StockSbx::new(resolve_sbx()?, runner).with_kit_image_cache(cache.clone());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let failures = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..2 {
            scope.spawn(|| {
                while let Some(source) = sources.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let started = std::time::Instant::now();
                    let name = source.file_name().unwrap_or_default().to_string_lossy();
                    match stock.prebuild_local_kit_image(source) {
                        Ok(true) => eprintln!(
                            "marshd: built Kit image {name} in {:.1}s",
                            started.elapsed().as_secs_f64()
                        ),
                        Ok(false) => {}
                        Err(error) => failures
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(format!("{name}: {error}")),
                    }
                }
            });
        }
    });
    let failures = failures
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if failures.is_empty() {
        eprintln!("marshd: Kit image cache current: {}", cache.display());
        Ok(())
    } else {
        Err(failures.join("; ").into())
    }
}

/// Host-private dev scratch root, outside every guest mount and control home.
fn dev_cache_root(real_home: &Path) -> PathBuf {
    if let Some(root) = env::var_os("MARSH_DEV_CACHE_ROOT") {
        return PathBuf::from(root);
    }
    #[cfg(target_os = "macos")]
    {
        real_home.join("Library/Caches/marsh/dev")
    }
    #[cfg(not(target_os = "macos"))]
    {
        // An inner daemon keeps its children's scratch under its own grant.
        env::var_os("MARSH_DEV_SCRATCH").map_or_else(
            || real_home.join(".cache/marsh/dev"),
            |scratch| PathBuf::from(scratch).join("tmp/dev"),
        )
    }
}

fn resolve_sbx() -> Result<PathBuf, Box<dyn std::error::Error>> {
    resolve_program("MARSH_SBX", "sbx").map_err(|error| {
        format!(
            "{error}. marsh needs Docker Sandboxes: brew install docker/tap/sbx; then `sbx login`"
        )
        .into()
    })
}

fn resolve_program(variable: &str, program: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(path) = env::var_os(variable) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!("{variable} does not name a file: {}", path.display()).into());
    }
    let path = env::var_os("PATH").ok_or("PATH is required to locate stock sbx")?;
    env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| format!("{program} was not found on PATH; set {variable}").into())
}

fn daemon_home() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut arguments = env::args_os().skip(1);
    let home = match arguments.next() {
        None => env::var_os("MARSH_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".marsh")))
            .ok_or("HOME is required")?,
        Some(flag) if flag == "--home" => {
            PathBuf::from(arguments.next().ok_or("--home needs a path")?)
        }
        Some(_) => return Err("usage: marshd [--home PATH]".into()),
    };
    if arguments.next().is_some() {
        return Err("usage: marshd [--home PATH]".into());
    }
    Ok(home)
}

fn host_control_home(
    real_home: &Path,
    selected_home: &Path,
    override_path: Option<&Path>,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let real_home = real_home.canonicalize()?;
    verify_host_directory(&real_home, false, false)?;
    let selected = selected_home.canonicalize()?;
    // With MARSH_CONTROL_HOME set, nothing is created under the default
    // product root; existing components are still verified for the overlap
    // check below.
    let create = override_path.is_none();
    let mut product_root = real_home;
    for &(component, strict) in host_product_components() {
        product_root.push(component);
        if create || fs::symlink_metadata(&product_root).is_ok() {
            verify_host_directory(&product_root, strict, create)?;
        }
    }
    if selected.starts_with(&product_root) || product_root.starts_with(&selected) {
        return Err("selected home overlaps protected marsh host directory".into());
    }
    let scope = marsh_daemon::host_state_directory::scope_control_leaf(&selected);
    let control_root = if let Some(path) = override_path {
        if !path.is_absolute() {
            return Err("MARSH_CONTROL_HOME must name a real absolute directory".into());
        }
        verify_host_directory(path, true, false)?;
        let path = path.canonicalize()?;
        verify_host_directory(&path, true, false)?;
        path
    } else {
        let mut path = product_root;
        path.push("control");
        verify_host_directory(&path, true, true)?;
        path
    };
    if selected.starts_with(&control_root) || control_root.starts_with(&selected) {
        return Err("selected home overlaps protected control directory".into());
    }
    let control_home = control_root.join(scope);
    verify_host_directory(&control_home, true, true)?;
    Ok(control_home)
}

fn host_product_root(real_home: &Path) -> PathBuf {
    let mut root = real_home.to_path_buf();
    for &(component, _) in host_product_components() {
        root.push(component);
    }
    root
}

fn host_product_components() -> &'static [(&'static str, bool)] {
    #[cfg(target_os = "macos")]
    {
        &[
            ("Library", false),
            ("Application Support", false),
            ("marsh", true),
        ]
    }
    #[cfg(not(target_os = "macos"))]
    {
        &[(".local", false), ("state", false), ("marsh", true)]
    }
}

fn verify_host_directory(
    path: &Path,
    strict: bool,
    create: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use marsh_daemon::host_state_directory::{HostStateDirectoryMode, verify_host_state_directory};
    let mode = if strict {
        HostStateDirectoryMode::Private
    } else {
        HostStateDirectoryMode::AccountAncestor
    };
    verify_host_state_directory(path, mode, create).map_err(|_| {
        format!(
            "unsafe or unavailable host control directory: {}",
            path.display()
        )
        .into()
    })
}

fn load_agents(packaged: &Path, user: &Path) -> Result<AgentRegistry, Box<dyn std::error::Error>> {
    use std::{io::Read as _, os::unix::fs::OpenOptionsExt as _};
    const MAX_AGENT_REGISTRY_BYTES: u64 = 1024 * 1024;
    // Preserve explicit replacement semantics, including [] to disable ACP.
    // Only a missing user file permits packaged defaults; malformed/unsafe
    // configuration is fatal, never an implicit disable.
    for path in [user, packaged] {
        let flags = nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK;
        let file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(flags)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(format!("cannot read ACP registry: {}", path.display()).into()),
        };
        if !file
            .metadata()
            .map_err(|_| format!("cannot inspect ACP registry: {}", path.display()))?
            .is_file()
        {
            return Err(format!("ACP registry must be a regular file: {}", path.display()).into());
        }
        let mut json = String::new();
        file.take(MAX_AGENT_REGISTRY_BYTES + 1)
            .read_to_string(&mut json)
            .map_err(|_| format!("cannot read ACP registry: {}", path.display()))?;
        if u64::try_from(json.len()).unwrap_or(u64::MAX) > MAX_AGENT_REGISTRY_BYTES {
            return Err(format!("ACP registry is too large: {}", path.display()).into());
        }
        return AgentRegistry::from_json_str(&json).map_err(|_| {
            format!(
                "invalid ACP registry: {}; check declarations and duplicate names",
                path.display()
            )
            .into()
        });
    }
    Ok(AgentRegistry::new())
}

fn backend_with_agents(
    stock: Arc<StockSbx>,
    commands: BTreeMap<String, RegisteredKit>,
    config: BackendConfig,
    agents: &AgentRegistry,
) -> Result<StockDaemonBackend, marsh_daemon::DaemonError> {
    StockDaemonBackend::new(stock, commands, config).with_agents(agents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use marsh_sbx::NativeKitRef;
    use std::{ffi::OsString, os::unix::fs::PermissionsExt};

    #[test]
    fn control_registry_is_scoped_outside_guest_home_and_ignores_legacy_files() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        let selected = root.path().join("selected");
        fs::create_dir(&real).unwrap();
        fs::create_dir(&selected).unwrap();
        let control = host_control_home(&real, &selected, None).unwrap();
        assert!(
            control.starts_with(
                real.canonicalize()
                    .unwrap()
                    .join(if cfg!(target_os = "macos") {
                        "Library/Application Support/marsh/control"
                    } else {
                        ".local/state/marsh/control"
                    })
            )
        );
        assert!(!control.starts_with(&selected));
        assert_eq!(
            fs::metadata(&control).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(control, host_control_home(&real, &selected, None).unwrap());

        let reference = format!("example/kit@sha256:{}", "a".repeat(64));
        fs::write(selected.join("commands.json"), "{bad legacy registry").unwrap();
        fs::write(selected.join("agents.json"), "[bad legacy registry").unwrap();
        fs::write(
            control.join("commands.json"),
            format!("{{\"kit\":\"{reference}\"}}"),
        )
        .unwrap();
        let missing = root.path().join("missing.json");
        assert_eq!(
            load_commands(&missing, &control.join("commands.json"))
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["kit"]
        );
        assert!(
            load_agents(&missing, &control.join("agents.json"))
                .unwrap()
                .declarations()
                .next()
                .is_none()
        );
    }

    #[test]
    fn control_override_requires_private_real_directory_outside_guest_home() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        let selected = root.path().join("selected");
        let override_home = root.path().join("control");
        fs::create_dir(&real).unwrap();
        fs::create_dir(&selected).unwrap();
        fs::create_dir(&override_home).unwrap();
        fs::set_permissions(&override_home, fs::Permissions::from_mode(0o700)).unwrap();
        let control = host_control_home(&real, &selected, Some(&override_home)).unwrap();
        assert_eq!(
            control.parent().unwrap(),
            override_home.canonicalize().unwrap()
        );
        assert!(
            !real.join("Library").exists() && !real.join(".local").exists(),
            "MARSH_CONTROL_HOME must keep the default product root untouched"
        );
        assert_eq!(
            fs::metadata(&control).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::set_permissions(&override_home, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(host_control_home(&real, &selected, Some(&override_home)).is_err());
        fs::set_permissions(&override_home, fs::Permissions::from_mode(0o700)).unwrap();
        let nested = selected.join("control");
        fs::create_dir(&nested).unwrap();
        fs::set_permissions(&nested, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(host_control_home(&real, &selected, Some(&nested)).is_err());
        assert!(host_control_home(&real, &selected, Some(&selected)).is_err());
    }

    #[test]
    fn control_override_scopes_registries_and_journals_per_selected_home() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        let first = root.path().join("first");
        let second = root.path().join("second");
        let override_root = root.path().join("control");
        for path in [&real, &first, &second, &override_root] {
            fs::create_dir(path).unwrap();
        }
        fs::set_permissions(&override_root, fs::Permissions::from_mode(0o700)).unwrap();
        let one = host_control_home(&real, &first, Some(&override_root)).unwrap();
        let two = host_control_home(&real, &second, Some(&override_root)).unwrap();
        assert_ne!(one, two);
        assert_eq!(one.parent(), two.parent());
        fs::write(one.join("commands.json"), "one").unwrap();
        fs::write(two.join("commands.json"), "two").unwrap();
        assert_ne!(
            fs::read(one.join("commands.json")).unwrap(),
            fs::read(two.join("commands.json")).unwrap()
        );
        assert_ne!(one.join("state"), two.join("state"));
        assert_ne!(one.join("kit-lifecycle"), two.join("kit-lifecycle"));
        assert!(marsh_daemon::reject_guest_mount_overlap(
            &[&two],
            std::slice::from_ref(&override_root),
        ).is_err());
        let symlink = root.path().join("control-link");
        std::os::unix::fs::symlink(&override_root, &symlink).unwrap();
        assert!(host_control_home(&real, &first, Some(&symlink)).is_err());
    }

    #[test]
    fn product_root_rejects_sibling_control_and_published_mcp_mounts() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        let selected = root.path().join("selected");
        fs::create_dir(&real).unwrap();
        fs::create_dir(&selected).unwrap();
        host_control_home(&real, &selected, None).unwrap();
        let product = real.join("Library/Application Support/marsh");
        for mount in [
            product.join("control/sibling"),
            product.join("published-mcp"),
        ] {
            fs::create_dir_all(&mount).unwrap();
            assert!(
                marsh_daemon::reject_guest_mount_overlap(
                    &[&mount],
                    std::slice::from_ref(&product),
                )
                .is_err()
            );
        }
        let override_home = root.path().join("override");
        fs::create_dir(&override_home).unwrap();
        assert!(host_control_home(&real, &product, Some(&override_home)).is_err());
    }

    #[test]
    fn invalid_acp_configuration_is_fatal_not_a_silent_disable() {
        let root = tempfile::tempdir().unwrap();
        let packaged = root.path().join("agents.json");
        let user = root.path().join("missing.json");
        let identity = format!("example/agent@sha256:{}", "a".repeat(64));
        let commands = BTreeMap::from([(
            "agent-kit".into(),
            RegisteredKit {
                workload: NativeKitRef::immutable_oci(identity.clone()).unwrap(),
            },
        )]);
        let config = BackendConfig {
            worker_binary: root.path().join("worker"),
            relay_binary: root.path().join("relay"),
            daemon_home: root.path().to_path_buf(),
            control_home: root.path().join("control"),
            protected_guest_roots: Vec::new(),
            shell: ShellVmSpec {
                name: "fixture-shell".into(),
                image: OciImage::parse(format!("example/shell@sha256:{}", "b".repeat(64))).unwrap(),
                shell_binary: root.path().join("shell"),
                user: ShellUser {
                    name: "fixture".into(),
                    uid: 501,
                    gid: 20,
                    home: root.path().to_path_buf(),
                },
            },
            resources: DEFAULT_JOB_RESOURCES,
            env: EnvironmentConfig::default(),
        };
        let stock = Arc::new(StockSbx::new(
            root.path().join("unused-sbx"),
            Arc::new(SystemCommandRunner::new(root.path())),
        ));

        fs::write(&packaged, "[").unwrap();
        assert!(load_agents(&packaged, &user).is_err());

        fs::write(
            &packaged,
            format!(
                "[{{\"schema_version\":1,\"name\":\"agent-session\",\"protocol\":\"acp_v1\",\"command\":\"agent-kit\",\"workload_digest\":\"example/agent@sha256:{}\"}}]",
                "c".repeat(64)
            ),
        )
        .unwrap();
        let mismatched = load_agents(&packaged, &user).unwrap();
        assert!(mismatched.resolve_acp("agent-session").is_ok());
        assert!(backend_with_agents(stock, commands, config, &mismatched).is_err());
    }

    #[test]
    fn duplicate_registry_keys_are_rejected_before_a_map_can_drop_them() {
        let error = serde_json::from_str::<marsh_contracts::command_registry::CommandRegistry>(
            r#"{"claude":"one@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","claude":"two@sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}"#,
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("duplicate command registry key"));
    }

    #[test]
    fn agent_registry_rejects_duplicate_names_and_requires_explicit_acp_name() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("agents.json");
        let missing = root.path().join("missing.json");
        let declaration = r#"{"schema_version":1,"name":"agent-session","protocol":"acp_v1","command":"agent-kit","workload_digest":"test-identity"}"#;
        fs::write(&user, format!("[{declaration},{declaration}]")).unwrap();
        assert!(load_agents(&missing, &user).is_err());
        fs::write(&user, format!("[{declaration}]")).unwrap();
        assert_eq!(
            load_agents(&missing, &user)
                .unwrap()
                .resolve_acp("agent-session")
                .unwrap()
                .command,
            "agent-kit"
        );
        assert!(
            load_agents(&missing, &user)
                .unwrap()
                .resolve_acp("claude")
                .is_err()
        );
    }

    #[test]
    fn explicit_agent_replacement_and_disable_do_not_resurrect_packaged_agents() {
        let root = tempfile::tempdir().unwrap();
        let packaged = root.path().join("packaged.json");
        let user = root.path().join("user.json");
        fs::write(&packaged, include_str!("../../../../packaging/agents.json")).unwrap();
        assert!(
            load_agents(&packaged, &user)
                .unwrap()
                .declarations()
                .next()
                .is_some()
        );
        fs::write(&user, "[]").unwrap();
        assert_eq!(
            load_agents(&packaged, &user)
                .unwrap()
                .declarations()
                .count(),
            0
        );
        fs::write(
            &user,
            r#"[{"schema_version":1,"name":"mine","protocol":"acp_v1","command":"mykit"}]"#,
        )
        .unwrap();
        let registry = load_agents(&packaged, &user).unwrap();
        assert_eq!(registry.declarations().count(), 1);
        assert!(registry.get("mine").is_some());
        fs::write(&user, "[invalid-secret-canary").unwrap();
        let error = load_agents(&packaged, &user).unwrap_err().to_string();
        assert!(error.contains(user.to_str().unwrap()));
        assert!(!error.contains("secret-canary"));
    }

    #[test]
    fn packaged_agent_without_generation_loads_for_startup_binding() {
        let root = tempfile::tempdir().unwrap();
        let packaged = root.path().join("agents.json");
        let user = root.path().join("missing.json");
        fs::write(
            &packaged,
            r#"[{"schema_version":1,"name":"claude-session","protocol":"acp_v1","command":"claude-acp","required_capabilities":[]}]"#,
        )
        .unwrap();
        let registry = load_agents(&packaged, &user).unwrap();
        assert!(
            registry
                .resolve_acp("claude-session")
                .unwrap()
                .workload_digest
                .is_empty()
        );
    }

    #[test]
    fn user_registry_overlays_packaged_defaults() {
        let root = tempfile::tempdir().unwrap();
        let packaged = root.path().join("packaged.json");
        let user = root.path().join("user.json");
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let c = "c".repeat(64);
        fs::write(
            &packaged,
            format!(r#"{{"claude":"example/claude@sha256:{a}","pi":"example/pi@sha256:{b}"}}"#),
        )
        .unwrap();
        fs::write(
            &user,
            format!(r#"{{"claude":"example/custom@sha256:{c}"}}"#),
        )
        .unwrap();
        let registry = load_commands(&packaged, &user).unwrap();
        assert_eq!(registry.len(), 2);
        assert_eq!(
            registry["claude"].workload.identity(),
            format!("example/custom@sha256:{c}")
        );
        assert_eq!(
            registry["pi"].workload.identity(),
            format!("example/pi@sha256:{b}")
        );
    }

    #[test]
    fn user_registry_is_valid_without_an_installed_default_file() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing.json");
        let user = root.path().join("user.json");
        let digest = "d".repeat(64);
        fs::write(
            &user,
            format!(r#"{{"fixture":"example/fixture@sha256:{digest}"}}"#),
        )
        .unwrap();
        let registry = load_commands(&missing, &user).unwrap();
        assert_eq!(
            registry["fixture"].workload.identity(),
            format!("example/fixture@sha256:{digest}")
        );
    }

    #[test]
    fn local_v3_paths_are_resolved_relative_to_their_own_registry() {
        let root = tempfile::tempdir().unwrap();
        let install = root.path().join("install");
        let home = root.path().join("home");
        fs::create_dir_all(install.join("kits/marsh-claude")).unwrap();
        fs::create_dir_all(home.join("kits/custom")).unwrap();
        let packaged = install.join("commands.json");
        let user = home.join("commands.json");
        fs::write(&packaged, r#"{"claude":"kits/marsh-claude"}"#).unwrap();
        fs::write(&user, r#"{"claude":"kits/custom"}"#).unwrap();

        let registry = load_commands(&packaged, &user).unwrap();
        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry["claude"].workload.source_dir(),
            Some(home.join("kits/custom").canonicalize().unwrap().as_path())
        );
    }

    #[test]
    fn packaged_local_v3_paths_are_usable_without_user_overrides() {
        let root = tempfile::tempdir().unwrap();
        let install = root.path().join("install");
        fs::create_dir_all(install.join("kits/marsh-claude")).unwrap();
        let packaged = install.join("commands.json");
        fs::write(&packaged, r#"{"claude":"kits/marsh-claude"}"#).unwrap();

        let registry = load_commands(&packaged, &root.path().join("missing.json")).unwrap();
        assert_eq!(
            registry["claude"].workload.source_dir(),
            Some(
                install
                    .join("kits/marsh-claude")
                    .canonicalize()
                    .unwrap()
                    .as_path()
            )
        );
    }

    #[test]
    fn local_v3_paths_support_absolute_and_parent_relative_directories() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config");
        let kit = root.path().join("kit");
        fs::create_dir(&config).unwrap();
        fs::create_dir(&kit).unwrap();
        let registry = config.join("commands.json");

        let missing = root.path().join("missing.json");
        for reference in ["../kit".to_owned(), kit.to_string_lossy().into_owned()] {
            fs::write(
                &registry,
                serde_json::to_vec(&BTreeMap::from([("foo", reference)])).unwrap(),
            )
            .unwrap();
            let loaded = load_commands(&registry, &missing).unwrap();
            assert_eq!(
                loaded["foo"].workload.source_dir(),
                Some(kit.canonicalize().unwrap().as_path())
            );
        }
        fs::write(&registry, r#"{"foo":"missing"}"#).unwrap();
        assert!(
            load_commands(&registry, &missing)
                .unwrap_err()
                .to_string()
                .contains("cannot resolve local Kit path")
        );
        fs::write(&registry, r#"{"foo":""}"#).unwrap();
        assert!(load_commands(&registry, &missing).is_err());
    }

    #[test]
    fn local_v3_registry_rejects_a_non_directory() {
        let root = tempfile::tempdir().unwrap();
        let packaged = root.path().join("commands.json");
        fs::write(root.path().join("not-a-kit"), b"ordinary file").unwrap();
        fs::write(&packaged, r#"{"bad":"not-a-kit"}"#).unwrap();

        assert!(load_commands(&packaged, &root.path().join("missing.json")).is_err());
    }

    #[test]
    fn job_limits_default_and_accept_strict_positive_overrides() {
        assert_eq!(
            job_resources_from_environment(|_| None).unwrap(),
            DEFAULT_JOB_RESOURCES
        );
        let values = BTreeMap::from([
            ("MARSH_JOB_CPU_MILLIS", "500"),
            ("MARSH_JOB_MEMORY_BYTES", "134217728"),
            ("MARSH_JOB_PIDS", "32"),
            ("MARSH_JOB_WRITABLE_BYTES", "16777216"),
            ("MARSH_JOB_OUTPUT_BYTES", "1048576"),
            ("MARSH_JOB_WALL_SECONDS", "10"),
        ]);
        assert_eq!(
            job_resources_from_environment(|name| values.get(name).map(OsString::from)).unwrap(),
            JobResources {
                cpu_millis: 500,
                memory_bytes: 134_217_728,
                pids: 32,
                writable_bytes: 16_777_216,
                output_bytes: 1_048_576,
                wall_seconds: 10,
            }
        );
    }

    #[test]
    fn malformed_or_out_of_range_job_limits_fail_closed() {
        for invalid in ["", "0", " 1", "+1", "1k", "18446744073709551616"] {
            let error = job_resources_from_environment(|name| {
                (name == "MARSH_JOB_MEMORY_BYTES").then(|| OsString::from(invalid))
            })
            .unwrap_err();
            assert!(error.to_string().contains("MARSH_JOB_MEMORY_BYTES"));
        }
        let error = job_resources_from_environment(|name| {
            (name == "MARSH_JOB_PIDS").then(|| OsString::from("4294967296"))
        })
        .unwrap_err();
        assert!(error.to_string().contains("MARSH_JOB_PIDS exceeds u32"));
    }
}
