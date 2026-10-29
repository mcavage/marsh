//! Optional native SBX environment additions, outside the guest home.
//! Only MCP and additional workspaces are accepted: marsh owns VM lifecycle,
//! the workload, primary workspace and HOME. `sbxenv.yaml`, `sbxenv.d/*.yaml`
//! and `sbxenvs/COMMAND.yaml` are validated as one snapshot and reloaded live:
//! a valid change replaces the snapshot, an invalid one never does.

use crate::{StockDaemonBackend, backend_error, identity_digest, unix_millis};
use marsh_contracts::MountAccess;
use marsh_daemon::{
    DaemonError, DaemonStore, SbxenvError, SbxenvStatus, ServerAttachment, SessionSpec,
};
use marsh_sbx::{AdmittedHostGrant, McpServer, ReadyKitVm, SbxError};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_MOUNTS: usize = 16;
const MAX_SERVERS: usize = 32;
const MAX_DROPINS: usize = 64;
/// A rejected configuration is re-examined at most this often while its files
/// stay unchanged (a missing directory may have been created meanwhile).
const ERROR_RETRY: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Environment {
    pub mounts: Vec<Workspace>,
    pub servers: Vec<McpServer>,
}

/// One validated snapshot of every environment file.
#[derive(Debug, Default, Serialize)]
pub(crate) struct Loaded {
    shared: Environment,
    layers: BTreeMap<String, Environment>,
    /// Resolved executables of local MCP servers, kept out of guests.
    #[serde(skip)]
    extra_roots: Vec<PathBuf>,
    /// The existing files, in load order, and a digest of their bytes.
    #[serde(skip)]
    files: Vec<PathBuf>,
    #[serde(skip)]
    hash: String,
}

impl Loaded {
    fn fingerprint(&self) -> String {
        identity_digest(&serde_json::to_string(self).expect("environment serializes"))
    }

    /// Every distinct server, shared first.
    fn servers(&self) -> Vec<&McpServer> {
        let mut seen = BTreeSet::new();
        std::iter::once(&self.shared)
            .chain(self.layers.values())
            .flat_map(|environment| &environment.servers)
            .filter(|server| seen.insert(server.name.as_str()))
            .collect()
    }
}

/// The files read for one snapshot.
#[derive(Default)]
struct Files {
    paths: Vec<PathBuf>,
    digest: Sha256,
}

impl Files {
    fn add(&mut self, path: &Path, bytes: &[u8]) {
        self.paths.push(path.to_path_buf());
        let name = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str());
        self.digest.update((name.len() as u64).to_le_bytes());
        self.digest.update(name);
        self.digest.update((bytes.len() as u64).to_le_bytes());
        self.digest.update(bytes);
    }
}

/// What a cheap `stat` pass saw: unchanged stamp, unchanged files.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Stamp {
    entries: Vec<(PathBuf, Option<[i128; 7]>)>,
    commands: Vec<String>,
}

#[derive(Debug)]
struct LiveError {
    message: String,
    unix_ms: u64,
    /// Sessions that were already shown this error as a notice.
    notified: BTreeSet<String>,
}

/// The active snapshot and the outcome of the latest check of the files.
#[derive(Debug, Default)]
pub(crate) struct Live {
    loaded: Arc<Loaded>,
    stamp: Stamp,
    last_attempt: Option<Instant>,
    last_reload_ms: Option<u64>,
    error: Option<LiveError>,
    /// Counts accepted changes to the effective environments.
    generation: u64,
}

/// Which warm VMs the watcher still has to bring up to date.
#[derive(Debug, Default)]
pub(crate) struct Sweep {
    generation: u64,
    pending: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Workspace {
    pub path: PathBuf,
    #[serde(default, rename = "readOnly")]
    pub read_only: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    #[serde(rename = "schemaVersion")]
    schema: String,
    #[serde(default, rename = "additionalWorkspaces")]
    mounts: Vec<Workspace>,
    #[serde(default)]
    mcp: Mcp,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mcp {
    #[serde(default)]
    servers: Vec<McpServer>,
}

impl Environment {
    pub fn is_empty(&self) -> bool {
        self.mounts.is_empty() && self.servers.is_empty()
    }

    pub fn fingerprint(&self) -> String {
        identity_digest(&serde_json::to_string(self).expect("environment serializes"))
    }

    fn append_file(&mut self, path: &Path, files: &mut Files) -> Result<(), DaemonError> {
        let mut file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(invalid(path, &error.to_string())),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
            return Err(invalid(path, "expected a regular file of at most 64 KiB"));
        }
        if metadata.uid() != rustix::process::getuid().as_raw()
            || metadata.permissions().mode() & 0o022 != 0
        {
            return Err(invalid(
                path,
                "environment file must be owned by you and not writable by group or others",
            ));
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(invalid(path, "file exceeds 64 KiB"));
        }
        files.add(path, &bytes);
        let document: Document = serde_yaml::from_slice(&bytes).map_err(|error| {
            invalid(
                path,
                &format!(
                    "{error}; supported fields: schemaVersion, mcp.servers, additionalWorkspaces"
                ),
            )
        })?;
        if document.schema != "1" {
            return Err(invalid(path, "schemaVersion must be \"1\""));
        }
        for mut mount in document.mounts {
            if mount.path.to_string_lossy().contains('$') || mount.path.starts_with("~") {
                return Err(invalid(
                    path,
                    "mount paths must be literal paths (no variable or ~ expansion)",
                ));
            }
            if mount.path.is_relative() {
                mount.path = path
                    .parent()
                    .expect("environment has parent")
                    .join(mount.path);
            }
            mount.path =
                fs::canonicalize(&mount.path).map_err(|error| invalid(path, &error.to_string()))?;
            if !mount.path.is_dir() {
                return Err(invalid(path, "additional workspace must be a directory"));
            }
            if self
                .mounts
                .iter()
                .any(|previous| overlaps(&previous.path, &mount.path))
            {
                return Err(invalid(
                    path,
                    "additional workspace paths overlap or are duplicated",
                ));
            }
            self.mounts.push(mount);
        }
        for server in document.mcp.servers {
            server.validate().map_err(backend_error)?;
            if self
                .servers
                .iter()
                .any(|previous| previous.name == server.name)
            {
                return Err(invalid(
                    path,
                    "duplicate MCP server name across environment layers",
                ));
            }
            self.servers.push(server);
        }
        if self.mounts.len() > MAX_MOUNTS || self.servers.len() > MAX_SERVERS {
            return Err(invalid(
                path,
                "at most 16 additional workspaces and 32 MCP servers per Kit",
            ));
        }
        Ok(())
    }
}

fn invalid(path: &Path, message: &str) -> DaemonError {
    DaemonError::InvalidState(format!("{}: {message}", path.display()))
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn signature(path: &Path) -> Option<[i128; 7]> {
    let metadata = fs::symlink_metadata(path).ok()?;
    Some([
        metadata.ino().into(),
        metadata.mtime().into(),
        metadata.mtime_nsec().into(),
        metadata.ctime().into(),
        metadata.ctime_nsec().into(),
        metadata.size().into(),
        metadata.mode().into(),
    ])
}

fn require_real_directory(directory: &Path) -> Result<(), DaemonError> {
    if let Ok(metadata) = fs::symlink_metadata(directory)
        && (!metadata.is_dir() || metadata.file_type().is_symlink())
    {
        return Err(invalid(
            directory,
            "expected a real directory, not a symlink",
        ));
    }
    Ok(())
}

/// `sbxenv.d/*.yaml` in name order. Dotfiles and other extensions are not
/// drop-ins, so editor swap files never matter.
fn dropin_files(directory: &Path) -> Result<Vec<PathBuf>, DaemonError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(invalid(directory, &error.to_string())),
    };
    let mut names = entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.strip_suffix(".yaml").is_some() && !name.starts_with('.'))
        .collect::<Vec<_>>();
    names.sort();
    if names.len() > MAX_DROPINS {
        return Err(invalid(directory, "at most 64 drop-in files are supported"));
    }
    Ok(names.into_iter().map(|name| directory.join(name)).collect())
}

fn definition_digest(server: &McpServer) -> String {
    identity_digest(&serde_json::to_string(server).expect("server serializes"))
}

impl StockDaemonBackend {
    fn sbxenv_commands(&self) -> Vec<String> {
        let mut commands = self.command_names();
        if !commands.iter().any(|name| name == "shell") {
            commands.push("shell".into());
        }
        commands
    }

    fn sbxenv_scope(&self) -> PathBuf {
        self.config
            .daemon_home
            .canonicalize()
            .unwrap_or_else(|_| self.config.daemon_home.clone())
    }

    /// Cheap `stat`-only view of every file a load would read.
    fn sbxenv_stamp(&self) -> Stamp {
        let scope = self.sbxenv_scope();
        let directory = scope.join("sbxenvs");
        let dropins = scope.join("sbxenv.d");
        let commands = self.sbxenv_commands();
        let mut paths = vec![
            scope.join("sbxenv.yaml"),
            directory.clone(),
            dropins.clone(),
        ];
        paths.extend(
            commands
                .iter()
                .map(|command| directory.join(format!("{command}.yaml"))),
        );
        paths.extend(dropin_files(&dropins).unwrap_or_default());
        Stamp {
            entries: paths
                .into_iter()
                .map(|path| {
                    let seen = signature(&path);
                    (path, seen)
                })
                .collect(),
            commands,
        }
    }

    /// Read and validate every environment file as one snapshot. No stock
    /// calls, registrations or mounts occur here.
    fn load_sbxenvs(&self) -> Result<Loaded, DaemonError> {
        let scope = self.config.daemon_home.canonicalize()?;
        let directory = scope.join("sbxenvs");
        let dropins = scope.join("sbxenv.d");
        require_real_directory(&directory)?;
        require_real_directory(&dropins)?;
        let mut files = Files::default();
        let mut shared = Environment::default();
        shared.append_file(&scope.join("sbxenv.yaml"), &mut files)?;
        for path in dropin_files(&dropins)? {
            shared.append_file(&path, &mut files)?;
        }
        let mut layers = BTreeMap::new();
        let mut servers = BTreeMap::new();
        let mut workloads = BTreeMap::new();
        for command in self.sbxenv_commands() {
            let mut environment = shared.clone();
            environment.append_file(&directory.join(format!("{command}.yaml")), &mut files)?;
            if let Ok(kit) = self.registered_kit(&command) {
                let fingerprint = environment.fingerprint();
                if workloads
                    .insert(kit.workload.identity().to_owned(), fingerprint.clone())
                    .is_some_and(|previous| previous != fingerprint)
                {
                    return Err(invalid(
                        &directory,
                        "aliases of the same Kit must have identical environments",
                    ));
                }
            }
            for server in &environment.servers {
                if servers
                    .insert(server.name.clone(), server.clone())
                    .is_some_and(|previous| previous != *server)
                {
                    return Err(invalid(
                        &directory,
                        "the same host-global MCP name has different definitions across Kits",
                    ));
                }
            }
            layers.insert(command, environment);
        }
        let mut extra_roots = Vec::new();
        for server in servers.values().filter(|server| !server.command.is_empty()) {
            let command = Path::new(&server.command);
            let executable = if command.is_absolute() {
                command.to_path_buf()
            } else if command.components().count() == 1 {
                std::env::var_os("PATH")
                    .and_then(|path| {
                        std::env::split_paths(&path)
                            .map(|directory| directory.join(command))
                            .find(|candidate| {
                                candidate.metadata().is_ok_and(|metadata| {
                                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                                })
                            })
                    })
                    .ok_or_else(|| {
                        invalid(&scope, "local MCP command is not installed on the host")
                    })?
            } else {
                return Err(invalid(
                    &scope,
                    "local MCP commands must be absolute paths or installed executable names",
                ));
            };
            extra_roots.push(executable.canonicalize()?);
        }
        Ok(Loaded {
            shared,
            layers,
            extra_roots,
            files: files.paths,
            hash: marsh_contracts::process::hex(&files.digest.finalize()),
        })
    }

    /// Load the optional shared files and each registered command's layer at
    /// daemon start. A start-up error is fatal: there is no earlier good
    /// configuration to keep.
    ///
    /// # Errors
    /// Rejects malformed/unsupported fields, unsafe paths and conflicting server definitions.
    pub fn with_sbxenvs(mut self) -> Result<Self, DaemonError> {
        let stamp = self.sbxenv_stamp();
        let loaded = self.load_sbxenvs()?;
        let scope = self.config.daemon_home.canonicalize()?;
        self.config.protected_guest_roots.extend([
            scope.join("sbxenv.yaml"),
            scope.join("sbxenvs"),
            scope.join("sbxenv.d"),
        ]);
        *self
            .sbxenv
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Live {
            loaded: Arc::new(loaded),
            stamp,
            last_reload_ms: Some(unix_millis(SystemTime::now())),
            ..Live::default()
        };
        Ok(self)
    }

    pub(crate) fn sbxenv_loaded(&self) -> Arc<Loaded> {
        Arc::clone(
            &self
                .sbxenv
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .loaded,
        )
    }

    fn sbxenv_unchanged(live: &Live, stamp: &Stamp) -> bool {
        live.stamp == *stamp
            && (live.error.is_none()
                || live
                    .last_attempt
                    .is_some_and(|attempt| attempt.elapsed() < ERROR_RETRY))
    }

    /// Notice a changed environment file: the cheap check run before every
    /// job or VM preparation and by the watcher. A valid change replaces the
    /// active snapshot, new servers are registered with stock `sbx mcp add`
    /// first. An invalid one is recorded and the last good snapshot stays.
    /// Returns whether the active snapshot changed.
    pub(crate) fn refresh_sbxenv(&self) -> bool {
        let stamp = self.sbxenv_stamp();
        let read = || {
            self.sbxenv
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        };
        if Self::sbxenv_unchanged(&read(), &stamp) {
            return false;
        }
        let _serial = self
            .sbxenv_reload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = {
            let live = read();
            if Self::sbxenv_unchanged(&live, &stamp) {
                return false;
            }
            Arc::clone(&live.loaded)
        };
        let result = self.load_sbxenvs().and_then(|next| {
            self.register_new_servers(&previous, &next)?;
            Ok(next)
        });
        let now = unix_millis(SystemTime::now());
        let mut live = self
            .sbxenv
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        live.stamp = stamp;
        live.last_attempt = Some(Instant::now());
        match result {
            Ok(next) => {
                live.error = None;
                let effective = next.fingerprint() != previous.fingerprint();
                let files_changed = next.hash != previous.hash;
                live.loaded = Arc::new(next);
                if effective {
                    live.generation += 1;
                }
                if effective || files_changed {
                    live.last_reload_ms = Some(now);
                }
                effective
            }
            Err(error) => {
                let message = error.to_string().replace(['\n', '\r'], " ");
                if live
                    .error
                    .as_ref()
                    .is_none_or(|current| current.message != message)
                {
                    eprintln!("marshd: sbxenv change ignored: {message}");
                    live.error = Some(LiveError {
                        message,
                        unix_ms: now,
                        notified: BTreeSet::new(),
                    });
                }
                false
            }
        }
    }

    /// Register servers a change added, before the change is accepted, so an
    /// unusable one (for example a name that conflicts with a different
    /// host-global definition) is rejected as a whole.
    fn register_new_servers(&self, previous: &Loaded, next: &Loaded) -> Result<(), DaemonError> {
        let known = previous.servers();
        for server in next.servers() {
            if known.contains(&server) {
                continue;
            }
            self.sbx.ensure_mcp_server(server).map_err(|error| {
                invalid(
                    &self.sbxenv_scope(),
                    &format!("MCP server {}: {}", server.name, backend_error(error)),
                )
            })?;
        }
        Ok(())
    }

    /// One line, once per session, while the files on disk are rejected.
    pub(crate) fn send_sbxenv_notice(&self, session_id: &str, attachment: &ServerAttachment) {
        let message = {
            let mut live = self
                .sbxenv
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(error) = live.error.as_mut() else {
                return;
            };
            if error.notified.len() >= 256 || !error.notified.insert(session_id.to_owned()) {
                return;
            }
            format!(
                "marsh: sbxenv change ignored, still using the last good configuration: {}",
                error.message
            )
        };
        let _ = attachment.send(&marsh_daemon::AttachmentFrame::Notice { message });
    }

    pub(crate) fn sbxenv_status_document(&self) -> SbxenvStatus {
        let live = self
            .sbxenv
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        SbxenvStatus {
            files: live.loaded.files.clone(),
            hash: live.loaded.hash.clone(),
            last_reload_unix_ms: live.last_reload_ms,
            last_error: live.error.as_ref().map(|error| SbxenvError {
                message: error.message.clone(),
                unix_ms: error.unix_ms,
            }),
        }
    }

    /// The watcher's pass: notice file changes, then bring each warm Kit VM
    /// up to date (new servers loaded; removed ones unloaded once the VM has
    /// no running job), so a running agent sees them without a new job.
    pub(crate) fn watch_sbxenv_pass(&self, store: &DaemonStore) {
        self.refresh_sbxenv();
        let generation = self
            .sbxenv
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .generation;
        {
            let mut sweep = self
                .sbxenv_sweep
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if sweep.generation == generation && !sweep.pending {
                return;
            }
            sweep.generation = generation;
            sweep.pending = false;
        }
        let vms = self
            .ready_kits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut pending = false;
        for vm in vms {
            match self.configure_prepared_mcp(&vm, false, store) {
                Ok(deferred) => pending |= deferred,
                Err(error) => eprintln!("marshd: sbxenv: {}: {error}", vm.name),
            }
        }
        if pending {
            self.sbxenv_sweep
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pending = true;
        }
    }

    pub(crate) fn environment(&self, command: &str) -> Environment {
        let loaded = self.sbxenv_loaded();
        loaded.layers.get(command).unwrap_or(&loaded.shared).clone()
    }

    pub(crate) fn dynamic_protected_roots(&self) -> Vec<PathBuf> {
        self.sbxenv_loaded().extra_roots.clone()
    }

    pub(crate) fn protected_roots(&self) -> Vec<PathBuf> {
        let mut roots = self.config.protected_guest_roots.clone();
        roots.extend(self.dynamic_protected_roots());
        roots
    }

    pub(crate) fn additional_grants(
        &self,
        session: &SessionSpec,
        environment: &Environment,
    ) -> Result<Vec<AdmittedHostGrant>, DaemonError> {
        if session.ephemeral_home {
            return Ok(Vec::new());
        }
        let project = session.launch_directory.canonicalize()?;
        let home = session.home_backing.canonicalize()?;
        let sources = environment
            .mounts
            .iter()
            .map(|mount| mount.path.as_path())
            .collect::<Vec<_>>();
        marsh_daemon::reject_guest_mount_overlap(&sources, &self.protected_roots())
            .map_err(DaemonError::InvalidState)?;
        let grants = environment.mounts.iter().map(|mount| {
            if overlaps(&mount.path, &project) || overlaps(&mount.path, &home)
                || session.guest_home.starts_with(&mount.path)
            {
                return Err(DaemonError::InvalidState(format!(
                    "extra mount {} overlaps the project or managed HOME; use a separate external directory",
                    mount.path.display())));
            }
            AdmittedHostGrant::open(mount.path.clone(), mount.path.clone(),
                if mount.read_only { MountAccess::ReadOnly } else { MountAccess::ReadWrite })
                .map_err(backend_error)
        }).collect::<Result<Vec<_>, _>>()?;
        self.sbx
            .preflight_host_grants_evicting_idle(&grants)
            .map_err(backend_error)?;
        Ok(grants)
    }

    pub(crate) fn inherited_environment(
        &self,
        mounts: &[marsh_daemon::PublicMount],
    ) -> Environment {
        // The parent's journal, never the child's Kit configuration, determines
        // which already-authorized additional sources are inherited.
        let loaded = self.sbxenv_loaded();
        let mut environment = Environment::default();
        for mount in mounts {
            if let Some(source) = loaded
                .layers
                .values()
                .flat_map(|env| &env.mounts)
                .find(|source| source.path == mount.target)
            {
                let mut source = source.clone();
                source.read_only = mount.access != "read_write";
                environment.mounts.push(source);
            }
        }
        environment
    }

    /// Bring one VM's loaded servers in line with `servers`: load the missing
    /// ones, and unload the ones no longer declared (or declared differently)
    /// unless `busy`; a busy VM keeps them until a later pass finds it idle.
    /// Returns whether an unload was deferred. A failed unload is logged, not
    /// fatal: the server is gone from the files and from every new VM.
    fn sync_mcp(
        &self,
        vm: &str,
        cold_started: bool,
        servers: &[McpServer],
        busy: bool,
        load: &dyn Fn(&McpServer) -> Result<(), SbxError>,
        unload: &dyn Fn(&str) -> Result<(), SbxError>,
    ) -> Result<bool, DaemonError> {
        let mut vms = self
            .configured_mcp_vms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cold_started {
            vms.remove(vm);
        }
        let loaded = vms.entry(vm.to_owned()).or_default();
        let wanted = servers
            .iter()
            .map(|server| (server.name.as_str(), definition_digest(server)))
            .collect::<BTreeMap<_, _>>();
        let stale = loaded
            .iter()
            .filter(|(name, digest)| wanted.get(name.as_str()) != Some(*digest))
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        let deferred = busy && !stale.is_empty();
        if !busy {
            for name in &stale {
                if let Err(error) = unload(name) {
                    eprintln!("marshd: sbxenv: could not unload {name} from {vm}: {error}");
                }
                loaded.remove(name);
            }
        }
        for server in servers {
            // A changed definition waits behind its old copy on a busy VM.
            if loaded.contains_key(&server.name) {
                continue;
            }
            load(server).map_err(backend_error)?;
            loaded.insert(server.name.clone(), wanted[server.name.as_str()].clone());
        }
        Ok(deferred)
    }

    pub(crate) fn configure_kit_mcp(
        &self,
        vm: &ReadyKitVm,
        fresh: bool,
        environment: &Environment,
        busy: bool,
    ) -> Result<bool, DaemonError> {
        self.sync_mcp(
            &vm.name,
            fresh,
            &environment.servers,
            busy,
            &|server| {
                self.sbx.ensure_mcp_server(server)?;
                self.sbx.load_mcp_server(vm, &server.name)
            },
            &|name| self.sbx.unload_mcp_server(vm, name),
        )
    }

    /// Apply the active environment of the command whose Kit `vm` is, to `vm`.
    /// `fresh`: the VM was created by this very preparation, so nothing the
    /// daemon remembers about its name applies (a cached `ReadyKitVm` still
    /// says `cold_started`, but is not fresh).
    pub(crate) fn configure_prepared_mcp(
        &self,
        vm: &ReadyKitVm,
        fresh: bool,
        store: &DaemonStore,
    ) -> Result<bool, DaemonError> {
        // Compare the captured workload against its registered authority; do not
        // re-fingerprint live source files while finishing a preparation.
        let loaded = self.sbxenv_loaded();
        let environment = loaded
            .layers
            .iter()
            .find_map(|(name, environment)| {
                self.registered_kit(name)
                    .ok()
                    .filter(|kit| {
                        vm.kit_ref == kit.workload.identity()
                            || vm
                                .kit_ref
                                .strip_prefix(kit.workload.identity())
                                .is_some_and(|rest| rest.starts_with('@'))
                    })
                    .map(|_| environment.clone())
            })
            .unwrap_or_else(|| loaded.shared.clone());
        self.configure_kit_mcp(vm, fresh, &environment, store.worker_busy(&vm.name))
    }

    /// `others_attached`: another shell is attached, so nothing is unloaded.
    pub(crate) fn configure_shell_mcp(
        &self,
        vm: &marsh_sbx::ReadyShellVm,
        environment: &Environment,
        others_attached: bool,
    ) -> Result<(), DaemonError> {
        self.sync_mcp(
            &vm.name,
            vm.cold_started,
            &environment.servers,
            others_attached,
            &|server| {
                self.sbx.ensure_mcp_server(server)?;
                self.sbx.load_shell_mcp_server(vm, &server.name)
            },
            &|name| self.sbx.unload_shell_mcp_server(vm, name),
        )
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RegisteredKit, tests::backend_for_kit_identity};
    use marsh_sbx::NativeKitRef;

    fn backend(root: &Path) -> StockDaemonBackend {
        backend_for_kit_identity(
            root,
            BTreeMap::from([
                (
                    "pi".into(),
                    RegisteredKit {
                        workload: NativeKitRef::immutable_oci(format!(
                            "example/pi@sha256:{}",
                            "a".repeat(64)
                        ))
                        .unwrap(),
                    },
                ),
                (
                    "codex".into(),
                    RegisteredKit {
                        workload: NativeKitRef::immutable_oci(format!(
                            "example/codex@sha256:{}",
                            "b".repeat(64)
                        ))
                        .unwrap(),
                    },
                ),
            ]),
        )
    }

    #[test]
    fn native_layers_are_additive_per_kit_and_reload_live() {
        let root = tempfile::tempdir().unwrap();
        let scope = root.path().canonicalize().unwrap();
        fs::create_dir(scope.join("sbxenvs")).unwrap();
        fs::create_dir(scope.join("reference")).unwrap();
        fs::create_dir(scope.join("notes")).unwrap();
        fs::write(
            scope.join("sbxenv.yaml"),
            "schemaVersion: '1'\nadditionalWorkspaces:\n  - path: reference\n    readOnly: true\n",
        )
        .unwrap();
        fs::write(scope.join("sbxenvs/pi.yaml"), "schemaVersion: '1'\nadditionalWorkspaces:\n  - path: ../notes\nmcp:\n  servers:\n    - name: granola\n      url: https://mcp.granola.ai/mcp\n").unwrap();
        let configured = backend(&scope).with_sbxenvs().unwrap();
        let pi = configured.environment("pi");
        assert_eq!(pi.mounts.len(), 2);
        assert_eq!(pi.mounts[0].path, scope.join("reference"));
        assert!(pi.mounts[0].read_only);
        assert_eq!(pi.mounts[1].path, scope.join("notes"));
        assert!(!pi.mounts[1].read_only);
        assert_eq!(pi.servers[0].name, "granola");
        assert_eq!(configured.environment("codex").mounts.len(), 1);
        assert_eq!(configured.environment("codex").servers, Vec::new());
        // The VM does not depend on the files; a live change keeps it.
        let before = configured.kit_spec("pi").unwrap().name;
        fs::write(scope.join("sbxenvs/pi.yaml"), "schemaVersion: '1'\n").unwrap();
        assert!(configured.refresh_sbxenv());
        assert_eq!(configured.kit_spec("pi").unwrap().name, before);
        assert_eq!(configured.environment("pi").servers, Vec::new());
        assert_eq!(configured.environment("pi").mounts.len(), 1);
        assert!(!configured.refresh_sbxenv());
    }

    #[test]
    fn dropins_concatenate_in_name_order_and_reload_live() {
        let root = tempfile::tempdir().unwrap();
        let scope = root.path().canonicalize().unwrap();
        fs::create_dir_all(scope.join("sbxenv.d")).unwrap();
        for name in ["one", "two", "three"] {
            fs::create_dir(scope.join(name)).unwrap();
        }
        let mount = |name: &str| {
            format!("schemaVersion: '1'\nadditionalWorkspaces:\n  - path: ../{name}\n")
        };
        fs::write(scope.join("sbxenv.d/20-b.yaml"), mount("two")).unwrap();
        fs::write(scope.join("sbxenv.d/10-a.yaml"), mount("one")).unwrap();
        fs::write(scope.join("sbxenv.d/.hidden.yaml"), "not yaml: [").unwrap();
        fs::write(scope.join("sbxenv.d/notes.txt"), "ignored").unwrap();
        let configured = backend(&scope).with_sbxenvs().unwrap();
        let paths = |backend: &StockDaemonBackend| {
            backend
                .environment("pi")
                .mounts
                .into_iter()
                .map(|mount| mount.path)
                .collect::<Vec<_>>()
        };
        assert_eq!(paths(&configured), [scope.join("one"), scope.join("two")]);
        let status = configured.sbxenv_status_document();
        assert_eq!(status.files.len(), 2);
        assert!(status.last_error.is_none());
        let hash = status.hash;
        fs::write(scope.join("sbxenv.d/15-c.yaml"), mount("three")).unwrap();
        assert!(configured.refresh_sbxenv());
        assert_eq!(
            paths(&configured),
            [scope.join("one"), scope.join("three"), scope.join("two")]
        );
        assert_ne!(configured.sbxenv_status_document().hash, hash);
    }

    #[test]
    fn invalid_change_keeps_last_good_and_reports_error() {
        let root = tempfile::tempdir().unwrap();
        let scope = root.path().canonicalize().unwrap();
        fs::create_dir(scope.join("sbxenv.d")).unwrap();
        fs::create_dir(scope.join("reference")).unwrap();
        let file = scope.join("sbxenv.yaml");
        let good = format!(
            "schemaVersion: '1'\nadditionalWorkspaces:\n  - path: {}\n",
            scope.join("reference").display()
        );
        let good = good.as_str();
        fs::write(&file, good).unwrap();
        let configured = backend(&scope).with_sbxenvs().unwrap();
        let hash = configured.sbxenv_status_document().hash;
        for broken in [
            "schemaVersion: '1'\nbogus: true\n",
            "schemaVersion: '1'\nadditionalWorkspaces:\n  - path: missing\n",
        ] {
            fs::write(&file, broken).unwrap();
            assert!(!configured.refresh_sbxenv());
            let status = configured.sbxenv_status_document();
            assert!(status.last_error.is_some(), "{broken}");
            assert_eq!(status.hash, hash);
            assert_eq!(configured.environment("pi").mounts.len(), 1);
        }
        // A drop-in that duplicates the shared directory is also rejected.
        fs::write(&file, good).unwrap();
        fs::write(scope.join("sbxenv.d/dup.yaml"), good).unwrap();
        assert!(!configured.refresh_sbxenv());
        assert!(
            configured
                .sbxenv_status_document()
                .last_error
                .unwrap()
                .message
                .contains("overlap")
        );
        fs::remove_file(scope.join("sbxenv.d/dup.yaml")).unwrap();
        configured.refresh_sbxenv();
        assert!(configured.sbxenv_status_document().last_error.is_none());
        assert_eq!(configured.sbxenv_status_document().hash, hash);
    }

    #[test]
    fn unsupported_fields_duplicate_layers_and_symlinks_fail_before_stock_calls() {
        let root = tempfile::tempdir().unwrap();
        let scope = root.path().canonicalize().unwrap();
        let file = scope.join("sbxenv.yaml");
        for document in [
            "schemaVersion: '9'\n",
            "schemaVersion: '1'\nagent: pi\n",
            "schemaVersion: '1'\nsecrets: {}\n",
            "schemaVersion: '1'\nlifecycle: {}\n",
            "schemaVersion: '1'\nmcp:\n  servers:\n    - name: x\n      url: https://example.com/mcp\n      command: /usr/bin/printf\n",
            "schemaVersion: '1'\nadditionalWorkspaces:\n  - path: missing\n",
            "schemaVersion: '1'\nschemaVersion: '1'\n",
        ] {
            fs::write(&file, document).unwrap();
            assert!(backend(&scope).with_sbxenvs().is_err(), "{document}");
            assert_eq!(fs::read_to_string(&file).unwrap(), document);
        }
        fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(scope.join("missing"), &file).unwrap();
        assert!(backend(&scope).with_sbxenvs().is_err());
        fs::remove_file(&file).unwrap();
        fs::create_dir(scope.join("sbxenvs")).unwrap();
        let duplicate = "schemaVersion: '1'\nmcp:\n  servers:\n    - name: x\n      url: https://example.com/mcp\n";
        fs::write(&file, duplicate).unwrap();
        fs::write(scope.join("sbxenvs/pi.yaml"), duplicate).unwrap();
        assert!(backend(&scope).with_sbxenvs().is_err());
    }

    #[test]
    fn aliases_cannot_give_one_workload_two_vm_authorities() {
        let root = tempfile::tempdir().unwrap();
        let scope = root.path().canonicalize().unwrap();
        fs::create_dir(scope.join("sbxenvs")).unwrap();
        fs::write(scope.join("sbxenvs/alias.yaml"), "schemaVersion: '1'\nmcp:\n  servers:\n    - name: x\n      url: https://example.com/mcp\n").unwrap();
        let mut configured = backend(&scope);
        configured
            .commands
            .insert("alias".into(), configured.commands["pi"].clone());
        assert!(
            configured
                .with_sbxenvs()
                .err()
                .unwrap()
                .to_string()
                .contains("aliases")
        );
    }
}
