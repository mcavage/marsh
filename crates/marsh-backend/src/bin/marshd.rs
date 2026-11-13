use marsh_backend::{
    BackendConfig, StockDaemonBackend, command_registry::load_commands, config::EnvironmentConfig,
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
    env,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

fn main() {
    let _ = marsh_provenance::embed!();
    let mut arguments = env::args_os().skip(1).collect::<Vec<_>>();
    // The one place a daemon may select test-break modes from its own
    // environment: only with `--test-breaks-from-env`, only in a build with the
    // guarded `test-breaks` feature (marsh-contracts); a release build leaves
    // the arguments alone and refuses the flag below as a usage error.
    marsh_contracts::test_breaks::init_from_daemon_launch(&mut arguments);
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
    if arguments
        .first()
        .is_some_and(|word| word == "--check-config")
    {
        match check_config(&arguments[1..]) {
            Ok(summary) => {
                println!("{summary}");
                return;
            }
            Err(error) => {
                eprintln!("marshd: --check-config: {error}");
                std::process::exit(1);
            }
        }
    }
    if arguments.first().is_some_and(|word| word == "--version") {
        println!("marshd {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if let Err(error) = run(&arguments) {
        eprintln!("marshd: {error}");
        std::process::exit(1);
    }
}

#[allow(clippy::too_many_lines)] // Startup wires one host authority boundary.
fn run(arguments: &[std::ffi::OsString]) -> Result<(), Box<dyn std::error::Error>> {
    let startup_lock = marsh_daemon::adopt_startup_lock()?;
    let breaks = marsh_sbx::services_breaks::active();
    if !breaks.is_empty() {
        eprintln!(
            "marshd: TEST BUILD: break modes active: {}",
            breaks.join(", ")
        );
    }
    let real_home =
        marsh_backend::account_home::trusted_account_home(env::var_os("HOME").as_deref())?;
    let defaults = JobDefaults::from_environment(|name| env::var_os(name))?;
    let home = std::path::absolute(daemon_home(arguments)?)?;
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
    // Path settings are made absolute against the launch directory before
    // marshd leaves it; an upgrade's successor is handed the absolute values.
    let mut successor_environment = Vec::new();
    let mut absolute_setting = |name: &str, path: PathBuf| -> std::io::Result<PathBuf> {
        let path = std::path::absolute(path)?;
        if env::var_os(name).is_some() {
            successor_environment.push((OsString::from(name), path.clone().into_os_string()));
        }
        Ok(path)
    };
    let guest_artifacts = absolute_setting(
        "MARSH_GUEST_ARTIFACTS",
        env::var_os("MARSH_GUEST_ARTIFACTS")
            .map_or_else(|| install_prefix.join("libexec/marsh"), PathBuf::from),
    )?;
    let commands = load_commands(
        &guest_artifacts.join("commands.json"),
        &control_home.join("commands.json"),
    )?;
    // The Kits this marsh shipped, before host overrides: automatic updates
    // follow their release channel and `uninstall` reverts to them.
    let packaged = load_commands(
        &guest_artifacts.join("commands.json"),
        &guest_artifacts.join(".packaged-only-no-user-registry"),
    )?;
    let channel = release_channel(
        env!("CARGO_PKG_VERSION"),
        guest_artifacts.join("local-build").exists()
            || guest_artifacts.join("dev-enabled").exists(),
    );
    let mut protected_guest_roots =
        vec![product_root, control_home.clone(), guest_artifacts.clone()];
    protected_guest_roots.push(install_prefix.to_path_buf());
    protected_guest_roots.extend([
        home.join("sbxenv.yaml"),
        home.join("sbxenvs"),
        home.join("sbxenv.d"),
    ]);
    let kit_image_cache =
        absolute_setting("MARSH_KIT_IMAGE_CACHE", kit_image_cache_root(&real_home))?;
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
    let ownership_mirror = env::var_os("MARSH_OWNERSHIP_MIRROR")
        .map(|mirror| absolute_setting("MARSH_OWNERSHIP_MIRROR", mirror.into()))
        .transpose()?;
    let dev_cache = absolute_setting("MARSH_DEV_CACHE_ROOT", dev_cache_root(&real_home))?;
    let stock_sbx = resolve_sbx()?;
    let programs = ProgramSettings::read(|name| env::var_os(name))?;
    successor_environment.extend(programs.successor_environment.iter().cloned());
    // Every path and program above is absolute now; leave the launch
    // directory before the first stock call.
    enter_daemon_directory(&control_home)?;
    let runner: Arc<dyn CommandRunner> = Arc::new(SystemCommandRunner::new(
        real_home.as_os_str().to_os_string(),
    ));
    if env::var_os("MARSH_LOCAL_SHELL_AUTHORITY").is_some() {
        return Err("MARSH_LOCAL_SHELL_AUTHORITY was removed; pass a local shell image tag via the normal shell image setting".into());
    }
    let mut stock_adapter = StockSbx::new(stock_sbx.clone(), runner);
    if let Some(docker) = programs.docker.clone() {
        stock_adapter = stock_adapter.with_docker(docker);
    }
    let mut stock_adapter = stock_adapter
        .with_kit_image_cache(kit_image_cache)
        // Inside `marsh --dev` every stock call crosses the dev broker.
        .with_published_image_cache(env::var_os("MARSH_DEV_DEPTH").is_none())
        .with_vm_ownership(control_home.join("vm-ownership.json"))?;
    // Test harnesses (`make check`) keep a copy of the map in a per-user scope
    // registry outside the scope, so `make check-gc` can still retire its VMs
    // after the scope directory is deleted.
    if let Some(mirror) = ownership_mirror {
        stock_adapter = stock_adapter.with_vm_ownership_mirror(mirror)?;
    }
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
        dev_cache,
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
    let backend = StockDaemonBackend::new(Arc::clone(&stock), commands, config)
        .with_packaged(packaged, channel);
    let backend = backend
        .with_sbxenvs()?
        .with_auto_update(env::var_os("MARSH_AUTO_UPDATE"), home.clone())
        .with_vm_idle_override(env::var("MARSH_VM_IDLE_SECONDS").ok())
        .with_services(env::var("MARSH_SERVICES_IDLE_SECONDS").ok().as_deref())
        .with_services_pool_reserve(
            env::var("MARSH_SERVICES_POOL_RESERVE_BYTES")
                .ok()
                .as_deref(),
        )
        .with_services_stop_wait(env::var("MARSH_SERVICES_STOP_WAIT_SECONDS").ok().as_deref())
        .with_services_artifacts(guest_artifacts.clone())
        .with_skills_home(env::var_os("MARSH_SKILLS_HOME").map(PathBuf::from));
    // A dev or local build runs the Kits it installed, before any job starts.
    for name in backend.bring_shipped_source_kits() {
        eprintln!("marshd: {name} runs the Kit this marsh installed again");
    }
    let protected_guest_roots = backend.protected_guest_roots().to_vec();
    // All registry/configuration validation precedes even the stock version probe.
    stock.validate_supported_version().map_err(|_| {
        format!("stock SBX compatibility check failed for {}; requires stock SBX v0.47.0 or newer with local Kit v3 support; check `sbx version` and `sbx create --help`", stock_sbx.display())
    })?;
    // Grants never outlive a daemon: revoke and clean every persisted grant
    // (and retry uncertain ones) before serving.
    if !stock.dev_grants().is_empty()
        && let Err(error) = dev_broker.revoke_all()
    {
        eprintln!("marshd: {error}");
    }
    let backend = Arc::new(backend.with_dev_broker(Arc::clone(&dev_broker)));
    // Started by the daemon it replaces (an upgrade): everything but the
    // endpoint is ready; say so, and wait until that daemon is gone.
    if env::args_os()
        .skip(1)
        .any(|word| marsh_daemon::is_handover_flag(&word))
    {
        marsh_daemon::handover_ready()?;
    }
    let server = Server::bind_with_stock_sbx_and_control_home(&home, &stock_sbx, &control_home)?
        .with_protected_guest_roots(protected_guest_roots)
        .with_job_defaults(defaults)
        .with_limits_config(home.join("config.json"))
        .with_successor_environment(successor_environment)
        .with_url_opener(programs.opener(env::var_os("MARSH_OPEN_URLS").as_deref(), &home))
        .with_backend(backend)
        .with_dev_broker(dev_broker);
    let stopping = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGINT, Arc::clone(&stopping))?;
    signal_hook::flag::register(SIGTERM, Arc::clone(&stopping))?;
    marsh_daemon::finish_startup_diagnostics()?;
    drop(startup_lock);
    loop {
        server.serve_until(|| stopping.load(Ordering::Relaxed))?;
        let Some((mut successor, fence)) = server.take_handover() else {
            return Ok(());
        };
        // Upgrading: the successor is ready. Checked alive right before the
        // release: release the endpoint, then the fence (the store and its
        // journal), and only then let it bind.
        if successor.alive() {
            drop(server);
            drop(fence);
            drop(successor);
            return Ok(());
        }
        // It died: serve on (the fence goes, background work restarts).
        drop(fence);
        server.handover_failed(successor);
    }
}

/// `marshd --check-config [DIR]`: validate an install's packaged configuration
/// (`commands.json`, `shell-image`; DIR defaults to this
/// binary's `libexec/marsh`) with the same loaders and ACP Kit binding startup
/// uses. It never touches `sbx`, VMs, the network or the control home, so a
/// release can be checked before publishing. Registries a user adds under
/// the control home are not consulted.
fn check_config(arguments: &[std::ffi::OsString]) -> Result<String, Box<dyn std::error::Error>> {
    let guest_artifacts = match arguments {
        [] => {
            let executable = fs::canonicalize(env::current_exe()?)?;
            executable
                .parent()
                .and_then(Path::parent)
                .ok_or("marshd has no install prefix")?
                .join("libexec/marsh")
        }
        [dir] => PathBuf::from(dir).canonicalize()?,
        _ => return Err("usage: marshd --check-config [LIBEXEC_MARSH_DIR]".into()),
    };
    let no_user = guest_artifacts.join(".check-config-no-user-registry");
    let commands = load_commands(&guest_artifacts.join("commands.json"), &no_user)?;
    let shell_image = fs::read_to_string(guest_artifacts.join("shell-image"))
        .map_err(|_| "cannot read shell-image")?;
    OciImage::parse(shell_image.trim().to_owned())?;
    // A local build and a dev install (`make dev`) run their source Kits.
    let local = guest_artifacts.join("local-build").exists()
        || guest_artifacts.join("dev-enabled").exists();
    if !local
        && commands
            .iter()
            .any(|(_, kit)| kit.workload.source_dir().is_some())
    {
        return Err("a release install must pin every Kit by digest (found a source Kit)".into());
    }
    Ok(format!(
        "marshd: configuration ok ({} commands) in {}",
        commands.len(),
        guest_artifacts.display()
    ))
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

/// Stock SBX resolves an unprefixed Kit reference against its working
/// directory, which it inherits from marshd, and marshd inherits the cwd of
/// whoever started it: often a project jobs can write. The daemon therefore
/// runs in its private control directory, which is never mounted into a guest.
fn enter_daemon_directory(control_home: &Path) -> Result<(), Box<dyn std::error::Error>> {
    env::set_current_dir(control_home).map_err(|error| {
        format!(
            "cannot enter the control directory {}: {error}",
            control_home.display()
        )
        .into()
    })
}

fn resolve_sbx() -> Result<PathBuf, Box<dyn std::error::Error>> {
    resolve_program(
        "MARSH_SBX",
        "sbx",
        env::var_os("MARSH_SBX").as_deref(),
        env::var_os("PATH").as_deref(),
    )
}

/// The canonical absolute path of stock, as the client pins it
/// (`marsh_daemon::canonical_executable`): `MARSH_SBX` if set (a path is
/// taken relative to the launch directory; a bare name is looked up on the
/// absolute entries of `PATH`, as the client does), else `program` on those
/// entries. Stock also runs with a project as its cwd (`sbx mount`), where a
/// relative path would name another file.
fn resolve_program(
    variable: &str,
    program: &str,
    configured: Option<&OsStr>,
    search: Option<&OsStr>,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let candidate = match configured {
        Some(value) if marsh_daemon::names_a_path(value) => std::path::absolute(value)?,
        Some(value) => marsh_daemon::find_on_absolute_path(value, search).ok_or_else(|| {
            format!(
                "{variable}={} was not found on the absolute entries of PATH (relative entries \
                 such as `.` are not searched); set {variable} to the absolute path of {program}",
                Path::new(value).display()
            )
        })?,
        None => {
            marsh_daemon::find_on_absolute_path(OsStr::new(program), search).ok_or_else(|| {
                format!(
                    "{program} was not found on PATH; set {variable} to its absolute path. \
                 marsh needs Docker Sandboxes: brew install docker/tap/sbx; then `sbx login`"
                )
            })?
        }
    };
    marsh_daemon::canonical_executable(&candidate).map_err(|_| {
        format!(
            "{variable}={} does not name a regular file (a relative path is taken against \
             the directory marshd started in); set it to the absolute path of {program}",
            candidate.display()
        )
        .into()
    })
}

/// The programs marshd's settings name, resolved before it leaves its launch
/// directory: a value with a `/` is a path, made absolute against it; a bare
/// name is looked up on a search path, never in the launch directory, which
/// is often a project a job can write.
struct ProgramSettings {
    /// `MARSH_DOCKER`: Docker runs with the fixed host command `PATH`
    /// (`HOST_COMMAND_PATH`), so a bare name is looked up there.
    docker: Option<PathBuf>,
    /// `MARSH_OPEN_COMMAND`: the opener runs as the user, so a bare name is
    /// looked up on the absolute entries of the user's own `PATH`.
    open_command: Option<PathBuf>,
    /// A bare `MARSH_OPEN_COMMAND` that was not found: no link is opened.
    opener_missing: bool,
    /// The resolved values, for an upgrade's successor.
    successor_environment: Vec<(OsString, OsString)>,
}

impl ProgramSettings {
    fn read(
        setting: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let value = |name: &str| setting(name).filter(|value| !value.is_empty());
        let mut settings = Self {
            docker: None,
            open_command: None,
            opener_missing: false,
            successor_environment: Vec::new(),
        };
        if let Some(docker) = value("MARSH_DOCKER") {
            settings.docker = Some(if marsh_daemon::names_a_path(&docker) {
                std::path::absolute(&docker)?
            } else {
                marsh_daemon::find_on_absolute_path(
                    &docker,
                    Some(OsStr::new(marsh_runtime::HOST_COMMAND_PATH)),
                )
                .ok_or_else(|| {
                    format!(
                        "MARSH_DOCKER={} was not found on {}, the PATH Docker commands run \
                         with (your shell's PATH is not searched); set MARSH_DOCKER to an \
                         absolute path",
                        Path::new(&docker).display(),
                        marsh_runtime::HOST_COMMAND_PATH
                    )
                })?
            });
        }
        if let Some(opener) = value("MARSH_OPEN_COMMAND") {
            settings.open_command = if marsh_daemon::names_a_path(&opener) {
                Some(std::path::absolute(&opener)?)
            } else {
                marsh_daemon::find_on_absolute_path(&opener, setting("PATH").as_deref())
            };
            if settings.open_command.is_none() {
                settings.opener_missing = true;
                eprintln!(
                    "marshd: MARSH_OPEN_COMMAND={} was not found on the absolute entries of PATH; \
                     links are not opened. Set it to an absolute path",
                    Path::new(&opener).display()
                );
            }
        }
        for (name, program) in [
            ("MARSH_DOCKER", &settings.docker),
            ("MARSH_OPEN_COMMAND", &settings.open_command),
        ] {
            if let Some(program) = program {
                settings
                    .successor_environment
                    .push((name.into(), program.clone().into_os_string()));
            }
        }
        Ok(settings)
    }

    /// How links are opened: the resolved `MARSH_OPEN_COMMAND`, else the
    /// default opener; none when the configured one was not found.
    fn opener(
        &self,
        switch: Option<&OsStr>,
        scope_root: &Path,
    ) -> Option<marsh_daemon::open_url::Opener> {
        if self.opener_missing {
            return None;
        }
        marsh_daemon::open_url::Opener::from_environment(
            self.open_command.as_deref().map(Path::as_os_str),
            switch,
            scope_root,
        )
    }
}

fn daemon_home(arguments: &[std::ffi::OsString]) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut arguments = arguments.iter();
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
    if arguments
        .next()
        .is_some_and(|word| !marsh_daemon::is_handover_flag(word))
        || arguments.next().is_some()
    {
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
        create_control_override(path)?;
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

/// A missing `MARSH_CONTROL_HOME` is created (0700) when its parent is a
/// safe directory of this account; an existing one is only verified.
fn create_control_override(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if fs::symlink_metadata(path).is_ok() {
        return Ok(());
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or("MARSH_CONTROL_HOME must name a real absolute directory")?;
    let parent = parent.canonicalize().map_err(|_| {
        format!(
            "unsafe or unavailable host control directory: {} (its parent does not exist)",
            path.display()
        )
    })?;
    verify_host_directory(&parent, false, false).map_err(|_| {
        format!(
            "unsafe or unavailable host control directory: {} (its parent {} is not a private directory of this account)",
            path.display(),
            parent.display()
        )
    })?;
    let leaf = path
        .file_name()
        .ok_or("MARSH_CONTROL_HOME must name a real absolute directory")?;
    verify_host_directory(&parent.join(leaf), true, true)
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

/// The image tag packaged Kits follow on automatic updates: releases push
/// `:latest`, nightlies `:nightly`. A local or dev build follows none.
fn release_channel(version: &str, local: bool) -> Option<String> {
    if local {
        None
    } else if version.contains("-nightly.") {
        Some("nightly".into())
    } else {
        Some("latest".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsString, os::unix::fs::PermissionsExt};

    fn program(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Program settings never name a file in the launch directory by a bare
    /// name: Docker's is looked up on the fixed host command PATH, the
    /// opener's and stock's on the absolute entries of the user's PATH. A path
    /// is taken relative to the launch directory. The opener and an upgrade's
    /// successor get the resolved values.
    #[test]
    fn program_settings_resolve_bare_names_on_their_search_paths() {
        let root = tempfile::tempdir().unwrap();
        let user_bin = root.path().join("home/.local/bin");
        program(&user_bin.join("my-opener"));
        program(&user_bin.join("sbx"));
        let search =
            env::join_paths([Path::new(""), Path::new("."), Path::new("src"), &user_bin]).unwrap();
        let host_sh = env::split_paths(marsh_runtime::HOST_COMMAND_PATH)
            .map(|directory| directory.join("sh"))
            .find(|candidate| candidate.is_file())
            .unwrap();
        let settings = |values: &[(&str, &str)]| {
            let values = values
                .iter()
                .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
                .chain([("PATH".to_owned(), search.clone())])
                .collect::<BTreeMap<_, _>>();
            ProgramSettings::read(|name| values.get(name).cloned())
        };
        let bare =
            settings(&[("MARSH_DOCKER", "sh"), ("MARSH_OPEN_COMMAND", "my-opener")]).unwrap();
        assert_eq!(bare.docker.as_deref(), Some(host_sh.as_path()));
        let opener = bare.opener(None, root.path()).unwrap();
        assert_eq!(opener.program, user_bin.join("my-opener"));
        assert_eq!(
            bare.successor_environment,
            [
                ("MARSH_DOCKER".into(), host_sh.clone().into_os_string()),
                (
                    "MARSH_OPEN_COMMAND".into(),
                    user_bin.join("my-opener").into_os_string()
                ),
            ]
        );
        // The test's cwd is this crate: paths are made absolute against it.
        let paths = settings(&[
            ("MARSH_DOCKER", "src/lib.rs"),
            ("MARSH_OPEN_COMMAND", "./src"),
        ])
        .unwrap();
        let cwd = env::current_dir().unwrap();
        assert_eq!(paths.docker, Some(cwd.join("src/lib.rs")));
        assert_eq!(
            paths.opener(None, root.path()).unwrap().program,
            cwd.join("src")
        );
        assert_eq!(
            paths.successor_environment[1],
            (
                "MARSH_OPEN_COMMAND".into(),
                cwd.join("src").into_os_string()
            )
        );
        // A bare Docker name off the host command PATH stops startup and
        // says why; a missing opener only turns link opening off.
        let docker = settings(&[("MARSH_DOCKER", "marsh-no-such")])
            .err()
            .unwrap()
            .to_string();
        assert!(
            docker.contains("your shell's PATH is not searched"),
            "{docker}"
        );
        assert!(docker.contains("absolute path"), "{docker}");
        let missing = settings(&[("MARSH_OPEN_COMMAND", "marsh-no-such")]).unwrap();
        assert!(missing.opener(None, root.path()).is_none());
        assert_eq!(missing.successor_environment, []);
        // Stock follows the client's rule, and only a missing default
        // suggests installing it.
        assert_eq!(
            resolve_program("MARSH_SBX", "sbx", Some(OsStr::new("sbx")), Some(&search)).unwrap(),
            user_bin.join("sbx").canonicalize().unwrap()
        );
        let bare_missing = resolve_program(
            "MARSH_SBX",
            "sbx",
            Some(OsStr::new("sbx")),
            Some(OsStr::new(".")),
        )
        .unwrap_err()
        .to_string();
        assert!(
            bare_missing.contains("absolute path of sbx"),
            "{bare_missing}"
        );
        assert!(!bare_missing.contains("brew install"), "{bare_missing}");
        let unset = resolve_program("MARSH_SBX", "sbx", None, Some(OsStr::new(".")))
            .unwrap_err()
            .to_string();
        assert!(unset.contains("brew install"), "{unset}");
        assert!(resolve_program("MARSH_SBX", "sbx", Some(OsStr::new("src/")), None).is_err());
    }

    /// marshd pins stock as the client does: canonical and absolute, whether
    /// named by a symlinked or relative `MARSH_SBX` or found on `PATH`.
    #[test]
    fn stock_sbx_is_pinned_by_its_canonical_absolute_path() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real-sbx");
        fs::write(&real, b"#!/bin/sh\n").unwrap();
        let link = root.path().join("sbx");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let canonical = real.canonicalize().unwrap();
        let resolve = |configured: Option<&OsStr>, search: Option<&OsStr>| {
            resolve_program("MARSH_SBX", "sbx", configured, search)
        };
        assert_eq!(resolve(Some(link.as_os_str()), None).unwrap(), canonical);
        let search = env::join_paths([Path::new("relative-missing"), root.path()]).unwrap();
        assert_eq!(resolve(None, Some(&search)).unwrap(), canonical);
        // The test's cwd is this crate: a relative path is pinned absolute.
        let relative = resolve(Some(OsStr::new("./Cargo.toml")), None).unwrap();
        assert_eq!(relative, Path::new("Cargo.toml").canonicalize().unwrap());
    }

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
    fn a_missing_control_override_is_created_only_under_a_safe_parent() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        let selected = root.path().join("selected");
        fs::create_dir(&real).unwrap();
        fs::create_dir(&selected).unwrap();
        let missing = root.path().join("control");
        let control = host_control_home(&real, &selected, Some(&missing)).unwrap();
        assert_eq!(
            fs::metadata(&missing).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(control.parent().unwrap(), missing.canonicalize().unwrap());
        // A parent others can write is not safe: nothing is created.
        let shared = root.path().join("shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
        let unsafe_leaf = shared.join("control");
        assert!(host_control_home(&real, &selected, Some(&unsafe_leaf)).is_err());
        assert!(!unsafe_leaf.exists());
        // A missing parent is refused too.
        let orphan = root.path().join("absent").join("control");
        assert!(host_control_home(&real, &selected, Some(&orphan)).is_err());
        assert!(!orphan.exists());
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
    fn release_channels_follow_the_build() {
        assert_eq!(release_channel("0.1.6", false).as_deref(), Some("latest"));
        assert_eq!(
            release_channel("0.1.6-nightly.20261019194017.g79ddbd5", false).as_deref(),
            Some("nightly")
        );
        assert_eq!(release_channel("0.1.6", true), None);
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
        // CPU and memory scale to the host; the rest are the fixed floors.
        let defaults = job_resources_from_environment(|_| None).unwrap();
        assert_eq!(defaults, JobDefaults::default().resources);
        assert!(defaults.cpu_millis >= DEFAULT_JOB_RESOURCES.cpu_millis);
        assert!(defaults.memory_bytes >= DEFAULT_JOB_RESOURCES.memory_bytes);
        assert_eq!(defaults.pids, DEFAULT_JOB_RESOURCES.pids);
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
