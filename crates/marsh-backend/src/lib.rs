//! Concrete local daemon backend built only on stock `sbx` CLI operations.

pub mod account_home;
pub mod command_registry;

use marsh_acp::{AgentAdapterDeclaration, AgentRegistry};
pub mod config;
mod ephemeral;
mod shell_attachment;

use marsh_contracts::{
    ExecutionOutcome, JobIdentity, JobMount, JobResources, JobSignal, JobSpec, MountAccess,
    ResourceLimit, TerminalSize, WORKER_CONTAINER_CAPACITY,
    command_registry::{CommandName, CommandRegistry, validate_count},
};
use marsh_daemon::{
    AttachmentFrame, CLEANUP_UNCERTAIN_EXIT_CODE, CleanupState, DaemonBackend, DaemonError,
    DaemonStore, EndpointPaths, ExecuteSpec, ExitStatus, LoadSelection, NewJob,
    PreparationProgress, PreparationResult, PublicMount, ScopeCleanupComponent, ScopeCleanupState,
    ScopeLifecycleAction, ScopeLifecycleReport, ServerAttachment, SessionSpec, ShellSpec,
    TimingReport, WorkerHealth, WorkerStatus,
};
use marsh_sbx::{
    AdmittedHostGrant, KitVmSpec, NativeKitRef, PreparedGrants, ReadyKitVm, SessionGrantPins,
    ShellVmSpec, StockSbx, VmPurpose, selected_home_identity,
};
use marsh_worker::{CleanupOutcome, MAX_STREAM_CHUNK, WorkerReport, WorkerRequest, WorkerResponse};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    io::{Read, Write},
    os::unix::ffi::OsStringExt,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const WORKER_START_PROGRESS_TIMEOUT: Duration = Duration::from_secs(10);
const WORKER_TERMINAL_GRACE: Duration = Duration::from_secs(15);
const WORKER_CANCEL_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// One immutable native workload Kit registered as a shell command.
#[derive(Clone, Debug)]
pub struct RegisteredKit {
    pub workload: NativeKitRef,
}

/// Daemon artifacts and supervisor ceilings. OCI image config remains the
/// sole authority for workload argv defaults, environment, user, and workdir.
#[derive(Clone, Debug)]
pub struct BackendConfig {
    pub worker_binary: PathBuf,
    pub relay_binary: PathBuf,
    pub daemon_home: PathBuf,
    pub control_home: PathBuf,
    pub protected_guest_roots: Vec<PathBuf>,
    pub shell: ShellVmSpec,
    pub resources: JobResources,
    pub env: config::EnvironmentConfig,
}

#[derive(Clone, Debug, Default)]
struct KitFlightCompletion {
    epoch: u64,
    error: Option<String>,
}

struct KitStartGuard {
    store: DaemonStore,
    id: String,
}

impl Drop for KitStartGuard {
    fn drop(&mut self) {
        self.store.finish_kit_start(&self.id);
    }
}

pub struct StockDaemonBackend {
    sbx: Arc<StockSbx>,
    commands: BTreeMap<String, RegisteredKit>,
    installed_commands: Mutex<BTreeMap<String, RegisteredKit>>,
    registry_install: Mutex<()>,
    agents: AgentRegistry,
    acp_pins: BTreeMap<String, String>,
    config: BackendConfig,
    registered_workers: Mutex<BTreeSet<String>>,
    ready_kits: Mutex<BTreeMap<String, ReadyKitVm>>,
    // Cleanup selectors only; private HOME authority remains in the native ledger.
    ephemeral_kits: Mutex<BTreeMap<String, BTreeMap<String, KitVmSpec>>>,
    kit_locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    kit_validation_flights: Mutex<BTreeMap<String, KitFlightCompletion>>,
    /// Ordinary shell VM cold start overlapped with `--load` Kit preparation.
    shell_warmup: Mutex<Option<thread::JoinHandle<()>>>,
    /// The dev broker (`marsh --dev`); dev shells use the one shell image.
    dev: Option<Arc<marsh_daemon::DevBroker>>,
}

impl StockDaemonBackend {
    /// Start the ordinary shell VM while `--load` prepares Kits: the shell
    /// attaches next. Errors are dropped; `open_shell` repeats the idempotent
    /// ensure and reports them.
    fn start_shell_warmup(&self, shell: ShellVmSpec) {
        let mut warmup = self
            .shell_warmup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if warmup.as_ref().is_some_and(|handle| !handle.is_finished()) {
            return;
        }
        let sbx = Arc::clone(&self.sbx);
        *warmup = thread::Builder::new()
            .name("marsh-shell-warmup".into())
            .spawn(move || {
                let _ = sbx.ensure_shell_vm(&shell);
            })
            .ok();
    }

    /// `--load`: prepare the selected Kits while the shell VM the session
    /// will attach to (`dev`: the dev shell VM) warms in parallel.
    fn prepare_for_shell(
        &self,
        selection: &LoadSelection,
        session: &SessionSpec,
        dev: bool,
        progress: &PreparationProgress,
        store: &DaemonStore,
    ) -> Result<PreparationResult, DaemonError> {
        let selected: Vec<_> = match selection {
            LoadSelection::All => self.command_names(),
            LoadSelection::Kits(commands) => commands.clone(),
        };
        let admitted_grants = self.admitted_session_grants_with_home(
            session,
            &session.guest_home,
            store.session_project_identity(&session.session_id),
            store.ephemeral_home_token(&session.session_id).as_ref(),
        )?;
        if !selected.is_empty()
            && !session.ephemeral_home
            && !store.shell_vm_bound(&session.session_id)
        {
            // A dev session attaches to the dev shell VM, not the ordinary one.
            if !dev {
                self.start_shell_warmup(self.config.shell.clone());
            } else if self.dev.is_some() {
                self.start_shell_warmup(self.dev_shell()?);
            }
        }
        let mut acquired = Vec::<(String, SessionGrantPins)>::new();
        let prepared = (|| {
            let mut result = PreparationResult::default();
            for command in selected {
                let spec = self.kit_spec_for_session(&command, session, store)?;
                let mut cold_started = false;
                let token = store.ephemeral_home_token(&session.session_id);
                let vm = self.prepare_one_authorized(&spec, store, token.as_ref(), || {
                    cold_started = true;
                    progress.cold_boot(command.clone())
                })?;
                let pins = self
                    .sbx
                    .prepare_session_grants(&vm, &session.session_id, &admitted_grants)
                    .map_err(backend_error)?;
                acquired.push((vm.name.clone(), pins));
                self.register_worker(store, &vm)?;
                if cold_started {
                    result.cold_kits.push(command.clone());
                }
                result.sandboxes.insert(command, vm.name);
            }
            Ok(result)
        })();
        match prepared {
            Ok(result) => Ok(result),
            Err(error) => {
                if let Err(rollback) = self.rollback_session_grants(&acquired, store) {
                    return Err(DaemonError::InvalidState(format!(
                        "Kit preparation failed: {error}; session grant rollback failed: {rollback}"
                    )));
                }
                Err(error)
            }
        }
    }

    /// The one warm dev shell VM per daemon (dev template); its VM-local
    /// build caches survive sessions like the ordinary shell VM.
    fn dev_shell(&self) -> Result<ShellVmSpec, DaemonError> {
        let mut dev = self.config.shell.clone();
        dev.name = self
            .sbx
            .vm_name(VmPurpose::Shell, "dev")
            .map_err(backend_error)?;
        Ok(dev)
    }

    /// Wait for an in-flight shell warm-up before shell admission or cleanup.
    fn finish_shell_warmup(&self) {
        let handle = self
            .shell_warmup
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }

    #[cfg(test)]
    fn admitted_session_grants(
        &self,
        session: &SessionSpec,
        guest_home: &Path,
        expected: Option<(u64, u64)>,
    ) -> Result<Vec<AdmittedHostGrant>, DaemonError> {
        self.admitted_session_grants_with_home(session, guest_home, expected, None)
    }

    fn admitted_session_grants_with_home(
        &self,
        session: &SessionSpec,
        guest_home: &Path,
        expected_project_identity: Option<(u64, u64)>,
        ephemeral_token: Option<&marsh_sbx::EphemeralHomeToken>,
    ) -> Result<Vec<AdmittedHostGrant>, DaemonError> {
        let mounts = [
            session.launch_directory.as_path(),
            session.home_backing.as_path(),
        ];
        let check = || {
            marsh_daemon::reject_guest_mount_overlap(&mounts, &self.config.protected_guest_roots)
                .map_err(DaemonError::InvalidState)
        };
        check()?;
        let grants = [
            AdmittedHostGrant::open(
                session.launch_directory.clone(),
                session.launch_directory.clone(),
                MountAccess::ReadWrite,
            ),
            if session.ephemeral_home {
                ephemeral_token
                    .ok_or_else(|| {
                        marsh_sbx::SbxError::HostGrantFence(
                            "ephemeral HOME requires a host-allocated private slot token".into(),
                        )
                    })
                    .and_then(|token| {
                        self.sbx
                            .ephemeral_home_grant(token, &session.home_backing, guest_home)
                    })
            } else {
                AdmittedHostGrant::open(
                    session.home_backing.clone(),
                    guest_home.to_path_buf(),
                    MountAccess::ReadWrite,
                )
            },
        ]
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(backend_error)?;
        if expected_project_identity.is_some_and(|expected| grants[0].source_identity() != expected)
        {
            return Err(DaemonError::InvalidState(
                "attached project identity changed before mount admission".into(),
            ));
        }
        check()?;
        // Reject the whole batch before even an implicit Kit lifecycle export.
        // Pure preflight creates no Untracked/pending record; actual mount/Create
        // adapters reserve and enroll their exact VM before any source effect.
        self.sbx
            .preflight_host_grants(&grants)
            .map_err(backend_error)?;
        Ok(grants)
    }

    #[must_use]
    pub fn new(
        sbx: Arc<StockSbx>,
        commands: BTreeMap<String, RegisteredKit>,
        config: BackendConfig,
    ) -> Self {
        Self {
            sbx,
            commands,
            installed_commands: Mutex::new(BTreeMap::new()),
            registry_install: Mutex::new(()),
            agents: AgentRegistry::new(),
            acp_pins: BTreeMap::new(),
            config,
            registered_workers: Mutex::new(BTreeSet::new()),
            ready_kits: Mutex::new(BTreeMap::new()),
            ephemeral_kits: Mutex::new(BTreeMap::new()),
            kit_locks: Mutex::new(BTreeMap::new()),
            kit_validation_flights: Mutex::new(BTreeMap::new()),
            shell_warmup: Mutex::new(None),
            dev: None,
        }
    }

    /// Serve `marsh --dev` sessions: grants come from `broker`; the dev shell
    /// VM uses `image` (the Rust + node dev template) when set.
    #[must_use]
    pub fn with_dev_broker(mut self, broker: Arc<marsh_daemon::DevBroker>) -> Self {
        self.dev = Some(broker);
        self
    }

    /// Pin registered ACP agents to the exact Kit generation resolved at startup.
    ///
    /// # Errors
    /// Returns an error for a missing Kit or a mismatched workload identity.
    pub fn with_agents(mut self, agents: &AgentRegistry) -> Result<Self, DaemonError> {
        let mut pinned = AgentRegistry::new();
        let mut pins = BTreeMap::new();
        for declaration in agents.declarations() {
            let kit = self
                .commands
                .get(&declaration.command)
                .ok_or_else(|| DaemonError::NotFound(declaration.command.clone()))?;
            let exact = kit.workload.capture_generation().map_err(backend_error)?;
            if declaration.workload_digest.is_empty() && kit.workload.source_dir().is_none() {
                return Err(DaemonError::InvalidState(
                    "immutable ACP Kit requires an explicit workload digest".into(),
                ));
            }
            if !declaration.workload_digest.is_empty()
                && declaration.workload_digest != exact.identity()
            {
                return Err(DaemonError::InvalidState(format!(
                    "ACP Kit identity mismatch for {}",
                    declaration.name
                )));
            }
            let mut resolved = declaration.clone();
            resolved.workload_digest = exact.identity().into();
            pinned
                .register(resolved)
                .map_err(|error| DaemonError::InvalidState(error.to_string()))?;
            pins.insert(declaration.command.clone(), exact.identity().into());
        }
        self.agents = pinned;
        self.acp_pins = pins;
        Ok(self)
    }

    fn kit_spec(&self, command: &str) -> Result<KitVmSpec, DaemonError> {
        let registered = self.registered_kit(command)?;
        let workload = registered
            .workload
            .capture_generation()
            .map_err(backend_error)?;
        if self
            .acp_pins
            .get(command)
            .is_some_and(|expected| expected != workload.identity())
        {
            return Err(DaemonError::InvalidState(format!(
                "ACP Kit {command} changed since daemon startup"
            )));
        }
        self.kit_spec_for_workload(workload)
    }

    fn registered_kit(&self, command: &str) -> Result<RegisteredKit, DaemonError> {
        let installed = self
            .installed_commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        installed
            .get(command)
            .or_else(|| self.commands.get(command))
            .cloned()
            .ok_or_else(|| DaemonError::NotFound(command.into()))
    }

    /// Registered command names whose workload is `identity`.
    fn kit_names_for_workload(&self, identity: &str) -> Vec<String> {
        let installed = self
            .installed_commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.commands
            .iter()
            .chain(installed.iter())
            .filter(|(_, kit)| {
                // A local source Kit's VM carries its captured generation,
                // `<registry identity>@<fingerprint>`.
                let registered = kit.workload.identity();
                identity == registered
                    || identity
                        .strip_prefix(registered)
                        .is_some_and(|rest| rest.starts_with('@'))
            })
            .map(|(name, _)| name.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn command_names(&self) -> Vec<String> {
        let installed = self
            .installed_commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.commands
            .keys()
            .chain(installed.keys())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn persist_installed_command(
        &self,
        expected: &CommandRegistry,
        registry: &CommandRegistry,
    ) -> Result<(), DaemonError> {
        let path = self.config.control_home.join("commands.json");
        // Do not overwrite changed host mappings observed after slow Kit preparation.
        if command_registry::read_declaration(&path).map_err(backend_error)? != *expected {
            return Err(DaemonError::InvalidState(
                "command registry changed during Kit preparation; retry after reviewing it".into(),
            ));
        }
        let bytes = registry.to_json_pretty().map_err(backend_error)?;
        let mut temporary = tempfile::NamedTempFile::new_in(&self.config.control_home)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(&path).map_err(|error| error.error)?;
        fs::File::open(&self.config.control_home)?.sync_all()?;
        Ok(())
    }

    fn kit_spec_for_workload(&self, workload: NativeKitRef) -> Result<KitVmSpec, DaemonError> {
        let kit_digest = identity_digest(workload.identity());
        let lifecycle_root = self.config.control_home.join("kit-lifecycle");
        fs::create_dir_all(&lifecycle_root)?;
        fs::set_permissions(&lifecycle_root, fs::Permissions::from_mode(0o700))?;
        selected_home_identity(&lifecycle_root).map_err(backend_error)?;
        let lifecycle_workspace = lifecycle_root.join(&kit_digest);
        fs::create_dir_all(&lifecycle_workspace)?;
        fs::set_permissions(&lifecycle_workspace, fs::Permissions::from_mode(0o700))?;
        selected_home_identity(&lifecycle_workspace).map_err(backend_error)?;
        Ok(KitVmSpec {
            name: self
                .sbx
                .vm_name(VmPurpose::Kit, workload.identity())
                .map_err(backend_error)?,
            worker_binary: self.config.worker_binary.clone(),
            workload_kit: workload,
            lifecycle_workspace,
        })
    }

    fn resolve_scope_kits(
        &self,
        by_vm: &mut BTreeMap<String, (KitVmSpec, Vec<String>)>,
        components: &mut Vec<ScopeCleanupComponent>,
    ) -> bool {
        let mut failed = false;
        for command in self.command_names() {
            if let Ok(spec) = self.kit_spec(&command) {
                by_vm
                    .entry(spec.name.clone())
                    .or_insert_with(|| (spec, Vec::new()))
                    .1
                    .push(command.clone());
            } else {
                failed = true;
                components.push(ScopeCleanupComponent {
                    kind: "kit".into(),
                    label: command.clone(),
                    state: ScopeCleanupState::CleanupUncertain,
                    vm: None,
                    detail: Some("Kit identity could not be resolved for cleanup".into()),
                });
            }
        }
        for spec in self.remembered_ephemeral_kits() {
            by_vm
                .entry(spec.name.clone())
                .or_insert_with(|| (spec, vec!["ephemeral-session".into()]));
        }
        failed
    }

    fn reset_stale_scope_components(&self, deadline: Instant) -> Vec<ScopeCleanupComponent> {
        let component = |state, vm, detail| ScopeCleanupComponent {
            kind: "kit".into(),
            label: "stale-generations".into(),
            state,
            vm,
            detail,
        };
        match self.sbx.reset_stale_scope_kit_vms_before(deadline) {
            Ok(removed) if removed.is_empty() => {
                vec![component(ScopeCleanupState::Absent, None, None)]
            }
            Ok(removed) => removed
                .into_iter()
                .map(|vm| component(ScopeCleanupState::Removed, Some(vm), None))
                .collect(),
            Err(error) => vec![component(
                ScopeCleanupState::CleanupUncertain,
                None,
                Some(format!(
                    "stale Kit generation cleanup could not be verified: {error}"
                )),
            )],
        }
    }

    fn prepare_one_with_progress(
        &self,
        spec: &KitVmSpec,
        store: &DaemonStore,
        cold_boot: impl FnOnce() -> Result<(), DaemonError>,
    ) -> Result<ReadyKitVm, DaemonError> {
        self.prepare_one_authorized(spec, store, None, cold_boot)
    }

    fn kit_flight_key(&self, spec: &KitVmSpec, ephemeral: bool) -> Result<String, DaemonError> {
        let scope_identity =
            selected_home_identity(&self.config.daemon_home).map_err(backend_error)?;
        Ok(if ephemeral {
            format!(
                "{scope_identity}:{}:{}",
                spec.workload_kit.identity(),
                spec.name
            )
        } else {
            format!("{scope_identity}:{}", spec.workload_kit.identity())
        })
    }

    fn prepare_one_authorized(
        &self,
        spec: &KitVmSpec,
        store: &DaemonStore,
        ephemeral: Option<&marsh_sbx::EphemeralHomeToken>,
        cold_boot: impl FnOnce() -> Result<(), DaemonError>,
    ) -> Result<ReadyKitVm, DaemonError> {
        let key = self.kit_flight_key(spec, ephemeral.is_some())?;
        if let Some(token) = ephemeral {
            self.sbx
                .ephemeral_home_grant(token, &spec.lifecycle_workspace, &spec.lifecycle_workspace)
                .map_err(backend_error)?;
        }
        let kit_lock = self
            .kit_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        // Callers that arrive during one live validation share its result.
        // A later, nonoverlapping caller observes the new epoch and performs
        // its own exact liveness validation.
        let observed_validation = self
            .kit_validation_flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .map_or(0, |completion| completion.epoch);
        let _initializing = kit_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cached = self
            .ready_kits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned();
        let current_validation = self
            .kit_validation_flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned()
            .unwrap_or_default();
        if let Some(completed) =
            completed_flight_outcome(observed_validation, &current_validation, cached.is_some())
        {
            completed.map_err(DaemonError::InvalidState)?;
            return Ok(cached.expect("successful completed flight retains Ready VM"));
        }
        let outcome = (|| {
            if let Some(vm) = cached
                && self
                    .sbx
                    .revalidate_cached_kit(spec, &vm)
                    .map_err(backend_error)?
            {
                return Ok(vm);
            }
            let repair_claimed = store.begin_worker_repair(&spec.name, &spec.name)?;
            let prepared = (|| {
                if !self.sbx.kit_vm_exists(spec).map_err(backend_error)? {
                    cold_boot()?;
                }
                let vm = if let Some(token) = ephemeral {
                    self.sbx.ensure_ephemeral_kit_vm(spec, token)
                } else {
                    self.sbx.ensure_kit_vm(spec)
                }
                .map_err(backend_error)?;
                if spec.workload_kit.source_dir().is_none() {
                    self.sbx.prewarm_workload(&vm).map_err(backend_error)?;
                }
                // Default MCP publications go only into a Kit VM this call
                // just created, before it is Ready and before any job runs
                // there; a running VM is never loaded (marsh_daemon::mcp_defaults).
                if vm.cold_started && ephemeral.is_none() {
                    self.load_default_mcp_publications(&vm);
                }
                Ok(vm)
            })();
            let vm = match prepared {
                Ok(vm) => vm,
                Err(error) => {
                    if repair_claimed {
                        store.finish_worker_repair(&spec.name, false)?;
                    }
                    return Err(error);
                }
            };
            if repair_claimed {
                store.finish_worker_repair(&spec.name, true)?;
            }
            self.ready_kits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(key.clone(), vm.clone());
            Ok(vm)
        })();
        self.kit_validation_flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                key,
                KitFlightCompletion {
                    epoch: current_validation.epoch.saturating_add(1),
                    error: outcome.as_ref().err().map(ToString::to_string),
                },
            );
        outcome
    }

    /// Load every current default MCP publication into a freshly created Kit
    /// VM. A failed load leaves the VM usable without that tool; the user can
    /// retry with `mcp load NAME --kit KIT`.
    fn load_default_mcp_publications(&self, vm: &ReadyKitVm) {
        let defaults = match marsh_daemon::mcp_defaults::loadable(&self.config.control_home) {
            Ok(defaults) => defaults,
            Err(error) => {
                eprintln!(
                    "marshd: default MCP publications not loaded into new Kit VM {}: {error}",
                    vm.name
                );
                return;
            }
        };
        for entry in defaults {
            if let Err(error) = self.sbx.load_mcp_server(vm, &entry.server) {
                eprintln!(
                    "marshd: default MCP tool {} not loaded into new Kit VM {}: {error}; load it with `mcp load {} --kit KIT`",
                    entry.name, vm.name, entry.name
                );
            }
        }
    }

    fn register_worker(&self, store: &DaemonStore, vm: &ReadyKitVm) -> Result<(), DaemonError> {
        let mut known = self
            .registered_workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if known.insert(vm.name.clone()) {
            let mut status = public_worker_status(store, vm);
            status.kits = self.kit_names_for_workload(&vm.kit_ref);
            store.register_worker(status)?;
        }
        Ok(())
    }

    fn release_session_grants(
        &self,
        session: &str,
        store: &DaemonStore,
    ) -> Result<(), DaemonError> {
        if let Some(token) = store.ephemeral_home_token(session) {
            self.sbx
                .close_ephemeral_home_admission(&token)
                .map_err(|error| DaemonError::ShellCleanupUncertain(error.to_string()))?;
        }
        let mut pinned_vms = self.sbx.pinned_session_vms(session);
        match self.sbx.close_shell_session_grants(session) {
            Ok(()) => self.close_ephemeral_kits(session, store),
            Err(error) => {
                pinned_vms.extend(self.sbx.pinned_session_vms(session));
                pinned_vms.sort();
                pinned_vms.dedup();
                for vm in pinned_vms {
                    let _ = store.quarantine_worker(&vm);
                }
                Err(DaemonError::ShellCleanupUncertain(error.to_string()))
            }
        }
    }

    fn rollback_session_grants(
        &self,
        acquired: &[(String, SessionGrantPins)],
        store: &DaemonStore,
    ) -> Result<(), DaemonError> {
        let mut failure = None;
        for (vm, pins) in acquired.iter().rev() {
            if let Err(error) = self.sbx.rollback_session_grants(pins) {
                let _ = store.quarantine_worker(vm);
                failure.get_or_insert_with(|| backend_error(error));
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

fn public_worker_status(store: &DaemonStore, vm: &ReadyKitVm) -> WorkerStatus {
    WorkerStatus {
        worker_id: vm.name.clone(),
        vm_id: vm.name.clone(),
        scope_id: store.status(None).scope_id,
        kit_ref: vm.kit_ref.clone(),
        kits: Vec::new(),
        warm: true,
        health: WorkerHealth::Ready,
        container_capacity: WORKER_CONTAINER_CAPACITY,
        active_container_ids: Vec::new(),
    }
}

impl DaemonBackend for StockDaemonBackend {
    fn validate_ephemeral_home(
        &self,
        session: &marsh_daemon::SessionAuthority,
        token: &marsh_sbx::EphemeralHomeToken,
    ) -> Result<(), DaemonError> {
        if !session.ephemeral_home {
            return Err(DaemonError::InvalidState(
                "private home token requires an ephemeral session".into(),
            ));
        }
        self.sbx
            .ephemeral_home_grant(token, &session.home_backing, &session.guest_home)
            .map(|_| ())
            .map_err(backend_error)
    }

    fn close_ephemeral_session(
        &self,
        session: &str,
        store: &DaemonStore,
    ) -> Result<(), DaemonError> {
        self.release_session_grants(session, store)
    }

    fn registered_commands(&self) -> Result<Vec<String>, DaemonError> {
        Ok(self.command_names())
    }

    fn install_kit(
        &self,
        command: String,
        reference: String,
        store: DaemonStore,
    ) -> Result<String, DaemonError> {
        CommandName::parse(command.clone()).map_err(backend_error)?;
        // Serialize only installs through preparation and persistence. Ordinary
        // command lookup must stay available during a slow cold preparation.
        let _install = self
            .registry_install
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current_names = self.command_names();
        if current_names.contains(&command) {
            return Err(DaemonError::InvalidState(format!(
                "Kit command {command} is already registered"
            )));
        }
        let path = self.config.control_home.join("commands.json");
        let previous = command_registry::read_declaration(&path).map_err(backend_error)?;
        let updated = previous
            .with_added(&command, &reference)
            .map_err(backend_error)?;
        let prospective_names = current_names
            .iter()
            .map(String::as_str)
            .chain(updated.iter().map(|(name, _)| name))
            .collect::<BTreeSet<_>>();
        validate_count(prospective_names.len()).map_err(backend_error)?;
        command_registry::validate_references(&path, &updated).map_err(backend_error)?;
        let workload = NativeKitRef::immutable_oci(reference.clone()).map_err(backend_error)?;
        let spec = self.kit_spec_for_workload(workload.clone())?;
        let vm = self.prepare_one_with_progress(&spec, &store, || Ok(()))?;
        self.register_worker(&store, &vm)?;
        self.persist_installed_command(&previous, &updated)?;
        self.installed_commands
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(command, RegisteredKit { workload });
        Ok(reference)
    }

    fn resolve_acp_agent(&self, name: &str) -> Result<AgentAdapterDeclaration, DaemonError> {
        let declaration = self
            .agents
            .resolve_acp(name)
            .map_err(|error| DaemonError::InvalidState(error.to_string()))?;
        let kit = self.registered_kit(&declaration.command)?;
        let exact = kit.workload.capture_generation().map_err(backend_error)?;
        if declaration.workload_digest != exact.identity() {
            return Err(DaemonError::InvalidState(format!(
                "ACP agent '{}' Kit identity changed; expected {}, got {}",
                name,
                declaration.workload_digest,
                exact.identity()
            )));
        }
        Ok(declaration.clone())
    }

    fn registered_kits(&self) -> Result<BTreeMap<String, String>, DaemonError> {
        self.command_names()
            .into_iter()
            .map(|cmd| {
                let kit = self.registered_kit(&cmd)?;
                Ok((cmd, kit.workload.identity().to_string()))
            })
            .collect()
    }

    fn reset_workers(
        &self,
        selection: &LoadSelection,
        store: DaemonStore,
    ) -> Result<Vec<String>, DaemonError> {
        let mut selected = match selection {
            LoadSelection::All => self.command_names(),
            LoadSelection::Kits(commands) => commands.clone(),
        };
        selected.sort();
        selected.dedup();
        if selected.is_empty() {
            return Err(DaemonError::InvalidState(
                "worker reset selection cannot be empty".into(),
            ));
        }

        // Resolve every command before making any lifecycle change. Aliases
        // that name the same exact Kit identity collapse to one owned VM.
        let mut specs = BTreeMap::new();
        for command in &selected {
            let spec = self.kit_spec(command)?;
            specs.entry(spec.name.clone()).or_insert(spec);
        }
        let scope_identity =
            selected_home_identity(&self.config.daemon_home).map_err(backend_error)?;
        let locks = specs
            .values()
            .map(|spec| {
                let key = format!("{scope_identity}:{}", spec.workload_kit.identity());
                self.kit_locks
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .entry(key)
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone()
            })
            .collect::<Vec<_>>();
        let _guards = locks
            .iter()
            .map(|lock| {
                lock.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            })
            .collect::<Vec<_>>();
        let worker_ids = specs.keys().cloned().collect::<Vec<_>>();
        // An owned quarantined VM is always retirable; pins held by shells
        // that are no longer attached are leftovers of ended sessions.
        let quarantined = store
            .status(None)
            .workers
            .into_iter()
            .filter(|worker| worker.health == WorkerHealth::Quarantined)
            .map(|worker| worker.worker_id)
            .collect::<BTreeSet<_>>();
        let live = store.attached_session_ids();
        store.begin_workers_reset(&worker_ids)?;

        for (index, (worker_id, spec)) in specs.iter().enumerate() {
            let retired = self.sbx.retire_kit_vm_before(
                spec,
                Instant::now() + Duration::from_hours(24),
                Some((&live, quarantined.contains(worker_id))),
            );
            if let Err(error) = retired {
                store.finish_worker_reset(worker_id, false)?;
                store.cancel_workers_reset(&worker_ids[index + 1..]);
                return Err(backend_error(error));
            }
            self.ready_kits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|_, ready| ready.name != *worker_id);
            self.kit_validation_flights
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(format!("{scope_identity}:{}", spec.workload_kit.identity()))
                .and_modify(|completion| {
                    completion.epoch = completion.epoch.saturating_add(1);
                    completion.error = None;
                })
                .or_insert(KitFlightCompletion {
                    epoch: 1,
                    error: None,
                });
            self.registered_workers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(worker_id);
            store.finish_worker_reset(worker_id, true)?;
        }
        Ok(selected)
    }

    #[allow(clippy::too_many_lines)] // Reports each local cleanup independently.
    fn teardown_scope(
        &self,
        action: ScopeLifecycleAction,
        store: DaemonStore,
        deadline: Instant,
    ) -> ScopeLifecycleReport {
        let ephemeral_sessions = store.retained_ephemeral_home_tokens();
        for (_, token) in &ephemeral_sessions {
            if let Err(error) = self.sbx.close_ephemeral_home_admission(token) {
                return ScopeLifecycleReport {
                    action,
                    cleanup_complete: false,
                    components: vec![ScopeCleanupComponent {
                        kind: "ephemeral-home".into(),
                        label: "admission".into(),
                        state: ScopeCleanupState::CleanupUncertain,
                        vm: None,
                        detail: Some(format!(
                            "private HOME admission could not be revoked; retained: {error}"
                        )),
                    }],
                };
            }
        }
        let mut by_vm = BTreeMap::<String, (KitVmSpec, Vec<String>)>::new();
        let mut components = Vec::new();
        // Report development grants only when some existed to revoke.
        if let Some(broker) = self.dev.as_ref().filter(|broker| broker.has_grants()) {
            let (state, detail) = match broker.revoke_all() {
                Ok(()) => (ScopeCleanupState::Removed, None),
                Err(error) => (ScopeCleanupState::CleanupUncertain, Some(error.to_string())),
            };
            components.push(ScopeCleanupComponent {
                kind: "dev-grant".into(),
                label: "development grants".into(),
                state,
                vm: None,
                detail,
            });
        }
        let resolution_failed = self.resolve_scope_kits(&mut by_vm, &mut components);
        let worker_ids = by_vm.keys().cloned().collect::<Vec<_>>();
        if store.begin_workers_reset(&worker_ids).is_err() {
            return ScopeLifecycleReport {
                action,
                cleanup_complete: false,
                components: vec![ScopeCleanupComponent {
                    kind: "scope".into(),
                    label: "runtime".into(),
                    state: ScopeCleanupState::CleanupUncertain,
                    vm: None,
                    detail: Some("runtime became active before cleanup began".into()),
                }],
            };
        }
        for (worker_id, (spec, mut labels)) in by_vm {
            labels.sort();
            let label = labels.join(",");
            // Lifecycle admission proved no shell is attached and no job is
            // queued or running: every retained reference is a leftover.
            let result = if Instant::now() < deadline {
                self.sbx
                    .retire_kit_vm_before(&spec, deadline, Some((&BTreeSet::new(), true)))
            } else {
                Err(marsh_sbx::SbxError::LifecycleDeadline {
                    operation: "reset Kit VM",
                })
            };
            let (mut state, mut detail, succeeded) = match result {
                Ok(true) => (ScopeCleanupState::Removed, None, true),
                Ok(false) => (ScopeCleanupState::Absent, None, true),
                Err(error) => (
                    ScopeCleanupState::CleanupUncertain,
                    Some(format!("Kit VM cleanup could not be verified: {error}")),
                    false,
                ),
            };
            if store.finish_worker_reset(&worker_id, succeeded).is_err() {
                state = ScopeCleanupState::CleanupUncertain;
                detail = Some("Kit cleanup state could not be finalized".into());
            }
            if succeeded && state != ScopeCleanupState::CleanupUncertain {
                self.ready_kits
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .retain(|_, ready| ready.name != worker_id);
                if let Ok(scope_identity) = selected_home_identity(&self.config.daemon_home) {
                    self.kit_validation_flights
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .entry(format!("{scope_identity}:{}", spec.workload_kit.identity()))
                        .and_modify(|completion| {
                            completion.epoch = completion.epoch.saturating_add(1);
                            completion.error = None;
                        })
                        .or_insert(KitFlightCompletion {
                            epoch: 1,
                            error: None,
                        });
                } else {
                    state = ScopeCleanupState::CleanupUncertain;
                    detail = Some("Kit cleanup cache could not be finalized".into());
                }
                self.registered_workers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&worker_id);
            }
            components.push(ScopeCleanupComponent {
                kind: "kit".into(),
                label,
                state,
                vm: Some(worker_id),
                detail,
            });
        }
        components.extend(self.reset_stale_scope_components(deadline));
        self.finish_shell_warmup();
        for shell in self.sbx.shell_recovery_specs(&self.config.shell) {
            let result = self
                .sbx
                .reset_shell_vm_before(&shell, deadline)
                .map_err(backend_error)
                .and_then(|removed| {
                    store
                        .complete_shell_vm_recovery(&shell.name)
                        .map(|()| removed)
                });
            let (state, detail) = match result {
                Ok(true) => (ScopeCleanupState::Removed, None),
                Ok(false) => (ScopeCleanupState::Absent, None),
                Err(error) => (
                    ScopeCleanupState::CleanupUncertain,
                    Some(format!(
                        "shell VM {} cleanup remains unverified: {error}; retry host `marsh reset` or `marsh stop` after stock health is restored",
                        shell.name
                    )),
                ),
            };
            components.push(ScopeCleanupComponent {
                kind: "shell".into(),
                label: shell.name.clone(),
                state,
                vm: Some(shell.name),
                detail,
            });
        }
        if store.shell_cleanup_pending() {
            components.push(ScopeCleanupComponent {
                kind: "shell".into(),
                label: "retained sessions".into(),
                state: ScopeCleanupState::CleanupUncertain,
                vm: None,
                detail: Some("shell cleanup records remain unresolved; no verified matching VM removal was available".into()),
            });
        }
        let mut cleanup_complete = !resolution_failed
            && components
                .iter()
                .all(|component| component.state != ScopeCleanupState::CleanupUncertain);
        if cleanup_complete && !ephemeral_sessions.is_empty() {
            // Only stale local cleanup selectors are forgotten. The SAME native
            // ledger still owns the closed slot, every Pending row and data.
            // Scope reset never sweeps per-UID retained homes or claims their GC.
            let mut remembered = self
                .ephemeral_kits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (session, token) in ephemeral_sessions {
                remembered.remove(&session);
                store.finish_ephemeral_session(&session);
                components.push(ScopeCleanupComponent { kind: "ephemeral-home".into(), label: format!("{token:?}"), state: ScopeCleanupState::CleanupUncertain,
                    vm: None,
                    detail: Some("VM cleanup is verified, but private HOME remains retained. After the frontend lease closes, explicitly select this slot with `marsh recover-home SLOT --discard`; pending/live/wrong-domain authority still denies deletion".into()) });
            }
            cleanup_complete = false;
        }
        ScopeLifecycleReport {
            action,
            cleanup_complete,
            components,
        }
    }

    fn prepare(
        &self,
        selection: &LoadSelection,
        session: &SessionSpec,
        progress: PreparationProgress,
        store: DaemonStore,
    ) -> Result<PreparationResult, DaemonError> {
        self.prepare_for_shell(selection, session, false, &progress, &store)
    }

    fn prepare_dev(
        &self,
        selection: &LoadSelection,
        session: &SessionSpec,
        progress: PreparationProgress,
        store: DaemonStore,
    ) -> Result<PreparationResult, DaemonError> {
        self.prepare_for_shell(selection, session, true, &progress, &store)
    }

    #[allow(clippy::too_many_lines)] // Keeps the linear cleanup transaction auditable.
    fn execute(
        &self,
        request: ExecuteSpec,
        attachment: ServerAttachment,
        store: DaemonStore,
    ) -> Result<(), DaemonError> {
        let started = Instant::now();
        let started_unix_ms = unix_millis(SystemTime::now());
        // A Kit job started from a split branch mounts only that branch's
        // workspace (and Git directory) instead of the whole project.
        let confinement = request.split_confinement().map_err(|reason| {
            DaemonError::InvalidState(format!("split branch job refused: {reason}"))
        })?;
        // One admission for every job (`docs/design/processes.md` s6), before any
        // VM effect; `begin_job_process` repeats it under the record lock.
        let registered = self.command_names();
        if !registered.contains(&request.command) {
            return Err(DaemonError::Refused(format!(
                "{}: not a registered command (registered: {})",
                request.command,
                registered.join(", ")
            )));
        }
        let link = request.process.clone().unwrap_or_default();
        // The Kit identity is known before VM work, so the same-Kit chain is
        // checked here too, not only at record time.
        let kit_identity = self
            .kit_spec(&request.command)
            .map(|spec| spec.workload_kit.identity().to_owned())
            .ok();
        let dropped = store.admit_process(
            &request.session.session_id,
            &request.command,
            kit_identity.as_deref(),
            &link,
            &registered,
        )?;
        if !dropped.is_empty() {
            let _ = attachment.send(&AttachmentFrame::Stderr {
                bytes: format!(
                    "marsh: spawn set narrowed to the parent's; not granted: {}\n",
                    dropped.join(",")
                )
                .into_bytes(),
            });
        }
        // A child's view is its parent's recorded mounts, copied once and
        // never recomputed from the session (`Attenuation`); only a split
        // branch narrows a view.
        let child_view = link.parent_job.as_ref().filter(|_| !link.branch);
        let mounts = match child_view {
            None => public_mounts(&request.session, confinement.as_ref()),
            Some(parent) => {
                let parent_mounts = store.job(parent).map(|job| job.mounts).unwrap_or_default();
                let covered = parent_mounts.iter().all(|mount| {
                    mount.target.starts_with(&request.session.launch_directory)
                        || mount.target.starts_with(&request.session.guest_home)
                });
                match marsh_daemon::process::child_view(
                    &parent_mounts,
                    request.effective_working_directory(),
                )
                .and_then(|view| {
                    if covered {
                        Ok(view)
                    } else {
                        Err("the parent's view is outside this session's grants".into())
                    }
                }) {
                    Ok(mut view) => {
                        // Narrower-or-equal: a split area created after the
                        // parent started is still read-only to the child.
                        let project = &request.session.launch_directory;
                        if let Some(marsh) = split_area(project)
                            && view.iter().any(|mount| &mount.target == project)
                            && !view.iter().any(|mount| mount.target == marsh)
                        {
                            view.push(PublicMount {
                                target: marsh,
                                access: "read_only".into(),
                            });
                        }
                        view
                    }
                    Err(message) => {
                        store.count_refused(Some(parent));
                        return Err(DaemonError::Refused(message));
                    }
                }
            }
        };
        let spec = self.kit_spec_for_session(&request.command, &request.session, &store)?;
        let admitted_grants = self.admitted_session_grants_with_home(
            &request.session,
            &request.session.guest_home,
            store.session_project_identity(&request.session.session_id),
            store
                .ephemeral_home_token(&request.session.session_id)
                .as_ref(),
        )?;
        let start_guard = KitStartGuard {
            id: store.begin_kit_start(
                &request.session.session_id,
                &request.command,
                request.placement,
            )?,
            store: store.clone(),
        };
        let token = store.ephemeral_home_token(&request.session.session_id);
        let vm = self
            .prepare_one_authorized(&spec, &store, token.as_ref(), || {
                attachment.send(&AttachmentFrame::ColdBoot {
                    kit: request.command.clone(),
                })
            })
            .map_err(|error| quarantine_hint(error, &request.command))?;
        let vm_ready_at = Instant::now();
        self.register_worker(&store, &vm)?;
        let image = vm.job_image().clone();
        let (job_id, attempt_id, lineage) = store.begin_job_process(
            NewJob {
                session_id: request.session.session_id.clone(),
                command: request.command.clone(),
                kit_ref: vm.kit_ref.clone(),
                workload_image: image.as_str().into(),
                mounts: mounts.clone(),
            },
            &link,
            &registered,
        )?;
        store.set_job_args(&job_id, &request.arguments);
        // A root job started by a stage after `join` consumes that split.
        if link.parent_job.is_none()
            && let Some(split) = request.environment.get("SPLIT_ID")
        {
            store.set_job_consumes(&job_id, &String::from_utf8_lossy(split));
        }
        // A root job started by a `fanout` branch is drawn under that fanout.
        if link.parent_job.is_none()
            && let Some(fanout) = request.environment.get("FANOUT_BRANCH")
        {
            store.set_job_fanout(&job_id, &String::from_utf8_lossy(fanout));
        }
        drop(start_guard);
        let mut job_guard = JobGuard::new(store.clone(), job_id.clone());
        attachment.send(&AttachmentFrame::JobStarted {
            job_id: job_id.clone(),
        })?;
        if let Err(error) = store.reserve_worker(&job_id, &vm.name, &vm.name) {
            job_guard.reject_before_effects(format!("admission_failed: {error}"))?;
            return Err(error);
        }
        let admitted_at = Instant::now();
        let grants = match self.sbx.prepare_job_grants(
            &vm,
            &request.session.session_id,
            &attempt_id,
            &admitted_grants,
        ) {
            Ok(grants) => grants,
            Err(error) => {
                let rollback = error.grant_rollback_complete();
                let cleanup = if rollback == Some(false) {
                    let _ = self.sbx.stop_quarantined(&vm);
                    CleanupState::Uncertain
                } else if rollback == Some(true) {
                    CleanupState::Verified
                } else {
                    CleanupState::NotRequired
                };
                store.finish_job_with_execution(
                    &job_id,
                    ExecutionOutcome::NotStarted,
                    ExitStatus {
                        code: Some(125),
                        cause: format!("grant_prepare_failed: {error}"),
                    },
                    false,
                    cleanup,
                    TimingReport::default(),
                )?;
                job_guard.complete();
                return Err(backend_error(error));
            }
        };
        let mut grant_guard = GrantGuard::new(Arc::clone(&self.sbx), vm.clone(), grants.clone());
        let mount_ready_at = Instant::now();
        let session_environment = BTreeMap::from([
            (
                "HOME".into(),
                request.session.guest_home.display().to_string(),
            ),
            ("USER".into(), request.session.username.clone()),
            ("LOGNAME".into(), request.session.username.clone()),
            (
                "MARSH_SELECTED_HOME".into(),
                request.session.guest_home.display().to_string(),
            ),
        ]);
        let (exported_environment, omitted) =
            merge_exported_environment(&request.environment, &self.config.env)?;
        let forwarded_names = exported_environment
            .iter()
            .filter(|(name, value)| request.environment.get(*name) == Some(*value))
            .filter(|(name, _)| marsh_daemon::split::forwardable(name))
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        for name in omitted {
            let _ = attachment.send(&AttachmentFrame::Stderr {
                bytes: format!("marsh: omitted exported variable {name} from job environment\n")
                    .into_bytes(),
            });
        }
        let job_mounts = if child_view.is_some() {
            match view_job_mounts(&grants.job_mounts(), &mounts) {
                Ok(job_mounts) => job_mounts,
                Err(message) => {
                    if self.sbx.revoke_grants(&grants).is_ok() {
                        grant_guard.complete();
                        store.finish_job_with_execution(
                            &job_id,
                            ExecutionOutcome::NotStarted,
                            ExitStatus {
                                code: Some(125),
                                cause: format!("view_failed: {message}"),
                            },
                            false,
                            CleanupState::Verified,
                            TimingReport::default(),
                        )?;
                        job_guard.complete();
                    }
                    return Err(DaemonError::Refused(message));
                }
            }
        } else {
            confine_job_mounts(
                grants.job_mounts(),
                &request.session.launch_directory,
                confinement.as_ref(),
            )
        };
        // Wall time is the Kit's own limit or the parent's remaining time,
        // whichever ends first (`docs/design/processes.md` s6); the worker enforces it.
        let mut resources = self.config.resources;
        let (wall_seconds, parent_deadline) =
            store.set_job_deadline(&job_id, resources.wall_seconds);
        resources.wall_seconds = wall_seconds;
        let spec = JobSpec {
            image,
            argv: request.arguments,
            identity: JobIdentity {
                uid: request.session.uid,
                gid: request.session.gid,
            },
            session_environment,
            exported_environment,
            working_directory: request
                .working_directory
                .unwrap_or(request.session.launch_directory),
            mounts: job_mounts,
            resources,
            terminal: request.session.terminal,
            terminal_size: request.session.terminal_size,
            // Every Kit job gets the job capability (`docs/design/processes.md` s4).
            split_capability: Some(PathBuf::from("/run/marsh-cap")),
            capability: Some(marsh_contracts::process::JobCapability {
                job: job_id.clone(),
                name: request.command.clone(),
                spawn: lineage.spawn,
                registered,
                // What the job received is forwarded to its own children,
                // so an exported variable reaches the whole tree (s6).
                forwarded: forwarded_names,
                env_key: marsh_contracts::process::hex(marsh_daemon::process::env_key()),
            }),
        };
        let output_budget = spec.resources.output_bytes;
        let terminal_deadline = Instant::now()
            .checked_add(Duration::from_secs(spec.resources.wall_seconds))
            .and_then(|deadline| deadline.checked_add(WORKER_TERMINAL_GRACE))
            .unwrap_or_else(Instant::now);
        store.mark_execution_submitted(&job_id)?;
        let mut launch = match self.sbx.launch_worker(&vm, &grants, &attempt_id, spec) {
            Ok(launch) => launch,
            Err(error @ marsh_sbx::SbxError::AmbiguousWorkerStart { .. }) => {
                let grant_cleanup = self.sbx.revoke_grants(&grants);
                grant_guard.complete();
                let message = match grant_cleanup {
                    Ok(()) => error.to_string(),
                    Err(cleanup_error) => {
                        format!("{error}; grant cleanup also failed: {cleanup_error}")
                    }
                };
                let output_complete = attachment
                    .send(&AttachmentFrame::Stderr {
                        bytes: format!("marsh: {message}\n").into_bytes(),
                    })
                    .is_ok();
                let code = finish_ambiguous_worker_start(&store, &job_id, output_complete)?;
                job_guard.complete();
                attachment.send(&AttachmentFrame::Exited { code })?;
                return Ok(());
            }
            Err(error) => {
                if self.sbx.revoke_grants(&grants).is_ok() {
                    grant_guard.complete();
                    store.finish_job_with_execution(
                        &job_id,
                        ExecutionOutcome::NotStarted,
                        ExitStatus {
                            code: Some(125),
                            cause: "worker_start_failed".into(),
                        },
                        false,
                        CleanupState::Verified,
                        TimingReport::default(),
                    )?;
                    job_guard.complete();
                }
                return Err(backend_error(error));
            }
        };
        grant_guard.complete();
        let control = launch.control();
        {
            // A tree cancel reaches this container directly (s8).
            let control = control.clone();
            let attempt = attempt_id.clone();
            store.register_job_control(
                &job_id,
                marsh_daemon::process::JobControl(Arc::new(move |signal: &str| {
                    let _ = control.send_bounded(
                        &WorkerRequest::Signal {
                            attempt: attempt.clone(),
                            signal: parse_signal(signal),
                        },
                        WORKER_CANCEL_WRITE_TIMEOUT,
                    );
                })),
            );
        }
        forward_multiplexed_input(
            attachment.clone(),
            control.clone(),
            attempt_id.clone(),
            request.session.terminal,
            store.clone(),
            job_id.clone(),
        );
        let mut capability = CapabilityBridges {
            attempt: attempt_id.clone(),
            job: job_id.clone(),
            control: control.clone(),
            socket: EndpointPaths::for_home(&self.config.daemon_home)
                .map(|paths| paths.socket)
                .unwrap_or_default(),
            store: store.clone(),
            token: None,
            channels: BTreeMap::new(),
        };
        let output_relay_at = Instant::now();
        let mut started_container = None;
        let mut worker_progress_at = None;
        let mut first_output_at = None;
        let mut stdout_bytes = 0_u64;
        let mut stderr_bytes = 0_u64;
        let mut output_complete = true;
        let mut transport_lost = false;
        // Why the transport was lost, for the receipt and the caller.
        let mut transport_detail = None;
        let start_progress_deadline = Instant::now() + WORKER_START_PROGRESS_TIMEOUT;
        let report = loop {
            let response = receive_worker_response(
                &launch.responses,
                if started_container.is_none() {
                    start_progress_deadline.min(terminal_deadline)
                } else {
                    terminal_deadline
                },
            );
            match response {
                Ok(WorkerResponse::Started { container_id, .. }) => {
                    let now = Instant::now();
                    worker_progress_at.get_or_insert(now);
                    store.mark_running(&job_id, &vm.name, &vm.name, container_id.as_str())?;
                    started_container = Some(container_id);
                }
                Ok(WorkerResponse::Stdout { bytes, .. }) => {
                    first_output_at.get_or_insert_with(Instant::now);
                    stdout_bytes =
                        stdout_bytes.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
                    if output_complete
                        && attachment.send(&AttachmentFrame::Stdout { bytes }).is_err()
                    {
                        output_complete = false;
                    }
                }
                Ok(WorkerResponse::Stderr { bytes, .. }) => {
                    first_output_at.get_or_insert_with(Instant::now);
                    stderr_bytes =
                        stderr_bytes.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
                    if output_complete
                        && attachment.send(&AttachmentFrame::Stderr { bytes }).is_err()
                    {
                        output_complete = false;
                    }
                }
                Ok(WorkerResponse::Terminal { report, .. }) => break report,
                Ok(WorkerResponse::Rejected { message, .. }) => {
                    let _ = attachment.send(&AttachmentFrame::Stderr {
                        bytes: format!("marsh-worker: {message}\n").into_bytes(),
                    });
                    break WorkerReport {
                        container_id: None,
                        execution: ExecutionOutcome::NotStarted,
                        delivery: marsh_worker::DeliveryOutcome::Complete,
                        cleanup: CleanupOutcome::NotRequired,
                        quarantine: false,
                        control_errors: 0,
                        retained_processes: Vec::new(),
                    };
                }
                Ok(WorkerResponse::CapOpen { channel, .. }) => capability.open(channel),
                Ok(WorkerResponse::CapData { channel, bytes, .. }) => {
                    // Bounded per connection: a stalled daemon side closes it.
                    if capability
                        .channels
                        .get(&channel)
                        .is_some_and(|send| send.try_send(bytes).is_err())
                    {
                        capability.channels.remove(&channel);
                    }
                }
                Ok(WorkerResponse::CapClose { channel, .. }) => {
                    capability.channels.remove(&channel);
                }
                Ok(response @ (WorkerResponse::Ready { .. } | WorkerResponse::Pong { .. })) => {
                    transport_lost = true;
                    transport_detail = Some(format!(
                        "worker sent {} on this attempt's route",
                        if matches!(response, WorkerResponse::Ready { .. }) {
                            "Ready"
                        } else {
                            "Pong"
                        }
                    ));
                    break WorkerReport {
                        container_id: started_container.clone(),
                        delivery: marsh_worker::DeliveryOutcome::Failed,
                        execution: ExecutionOutcome::Unknown,
                        cleanup: CleanupOutcome::Uncertain,
                        quarantine: true,
                        control_errors: 1,
                        retained_processes: Vec::new(), // No observed IDs, NOT proof of absence.
                    };
                }
                Err(timed_out) => {
                    transport_detail = Some(if !timed_out {
                        launch
                            .responses
                            .loss_reason()
                            .unwrap_or_else(|| "worker route closed".into())
                    } else if started_container.is_none() {
                        format!(
                            "no worker progress within {} s of start",
                            WORKER_START_PROGRESS_TIMEOUT.as_secs()
                        )
                    } else {
                        "no terminal report by the wall limit plus grace".into()
                    });
                    if timed_out {
                        let _ = control.send_bounded(
                            &WorkerRequest::Cancel {
                                attempt: attempt_id.clone(),
                            },
                            WORKER_CANCEL_WRITE_TIMEOUT,
                        );
                    }
                    transport_lost = true;
                    output_complete = false;
                    break WorkerReport {
                        container_id: started_container.clone(),
                        delivery: marsh_worker::DeliveryOutcome::Failed,
                        execution: ExecutionOutcome::Unknown,
                        cleanup: CleanupOutcome::Uncertain,
                        quarantine: true,
                        control_errors: 1,
                        retained_processes: Vec::new(), // Transport loss keeps cleanup uncertain.
                    };
                }
            }
        };
        store.unregister_job_control(&job_id);
        output_complete &= report.delivery == marsh_worker::DeliveryOutcome::Complete;
        if launch.responses.overflowed() {
            output_complete = false;
        }
        launch.finish();
        let process_exit_at = Instant::now();
        let output_drained_at = Instant::now();
        let worker_progress_at = worker_progress_at.unwrap_or(process_exit_at);
        let capture_at = Instant::now();
        let grant_cleanup = self.sbx.revoke_grants(&grants);
        let cleanup_uncertain = reconcile_worker_cleanup(
            self.sbx.as_ref(),
            &vm,
            started_container.as_ref(),
            &report,
            &grant_cleanup,
            transport_lost,
        )?;
        if cleanup_uncertain {
            let message = if let Err(error) = &grant_cleanup {
                format!("grant cleanup failed: {error}")
            } else if transport_lost {
                format!(
                    "retained worker transport was lost while the attempt was active: {}",
                    transport_detail.as_deref().unwrap_or("unknown")
                )
            } else {
                "container cleanup could not be verified".to_owned()
            };
            let diagnostic = cleanup_diagnostic(
                &message,
                output_budget,
                stdout_bytes.saturating_add(stderr_bytes),
            );
            if !diagnostic.is_empty()
                && attachment
                    .send(&AttachmentFrame::Stderr { bytes: diagnostic })
                    .is_err()
            {
                output_complete = false;
            }
        }
        let cleanup = if cleanup_uncertain {
            CleanupState::Uncertain
        } else {
            match report.cleanup {
                CleanupOutcome::Verified => CleanupState::Verified,
                CleanupOutcome::NotRequired => CleanupState::NotRequired,
                CleanupOutcome::Uncertain => CleanupState::Uncertain,
            }
        };
        let cleanup_at = Instant::now();
        grant_guard.complete();
        let (mut code, mut cause) = worker_public_exit(&report, cleanup_uncertain, output_complete);
        if let Some(detail) = &transport_detail {
            // Bounded: the reason carries at most a short worker stderr tail.
            let detail = detail.chars().take(768).collect::<String>();
            cause = format!("{cause}; transport_lost: {detail}");
        }
        // The parent's deadline ended this child: its own wall limit (set to
        // the parent's remaining time) fired, or the parent's end at that
        // deadline cancelled the tree first (s6).
        let deadline_passed =
            parent_deadline && store.job_deadline_passed(&job_id, PARENT_DEADLINE_SLACK_MS);
        let wall_limited = matches!(
            report.execution,
            ExecutionOutcome::LimitExceeded {
                resource: ResourceLimit::Wall
            }
        );
        if !cleanup_uncertain && store.cancel_requested(&job_id) {
            // Cancelled with its tree (root Ctrl-C, a dropped caller): 130,
            // as an interrupted foreground command (s8).
            code = Some(130);
            cause = if deadline_passed && !wall_limited {
                format!("limit:wall: parent deadline; cancelled; {cause}")
            } else {
                format!("cancelled; {cause}")
            };
        }
        if parent_deadline && wall_limited {
            cause = cause.replacen("limit:wall", "limit:wall: parent deadline", 1);
        }
        let boundaries = PhaseBoundaries {
            started,
            started_unix_ms,
            vm_ready_at,
            admitted_at,
            mount_ready_at,
            worker_progress_at,
            output_relay_at,
            first_output_at,
            process_exit_at,
            output_drained_at,
            capture_at,
            cleanup_at,
        };
        if let Err(error) = finish_job_with_timing(
            &store,
            &job_id,
            report.execution,
            ExitStatus { code, cause },
            output_complete,
            cleanup,
            &boundaries,
        ) {
            // The runtime and grants already have a verified terminal state.
            // A timing-bookkeeping invariant must fail the receipt without
            // turning that clean worker into cleanup uncertainty.
            job_guard.complete();
            return Err(error);
        }
        job_guard.complete();
        attachment.send(&AttachmentFrame::Exited {
            code: code.unwrap_or(125),
        })?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)] // Keeps relay and mount rollback in one transaction.
    fn open_shell(
        &self,
        request: ShellSpec,
        attachment: ServerAttachment,
        store: DaemonStore,
    ) -> Result<(), DaemonError> {
        let session_id = request.session.session_id.clone();
        // HOME is mounted at one fixed guest path. A disposable backing must
        // never replace another live shell's mount in the shared warm VM.
        let shell = if request.session.ephemeral_home {
            let mut isolated = self.config.shell.clone();
            isolated.name = self
                .sbx
                .vm_name(VmPurpose::Shell, &format!("ephemeral:{session_id}"))
                .map_err(backend_error)?;
            isolated
        } else if request.dev {
            self.dev_shell()?
        } else {
            self.config.shell.clone()
        };
        let dev_grant = if request.dev {
            let Some(broker) = &self.dev else {
                return Err(DaemonError::InvalidState(
                    "marsh --dev requires a daemon started with MARSH_ENABLE_DEV_SCOPES=1".into(),
                ));
            };
            let grant = broker.create_grant(
                &session_id,
                &request.session.launch_directory,
                &self.config.shell.shell_binary,
            )?;
            Some((Arc::clone(broker), grant))
        } else {
            None
        };
        let outcome = (|| {
            store.bind_shell_cleanup_vm(&session_id, &shell.name)?;
            let _shell_session = self
                .sbx
                .admit_shell_session(&shell.name)
                .map_err(backend_error)?;
            let mut admitted_mounts = self.admitted_session_grants_with_home(
                &request.session,
                &shell.user.home,
                store.session_project_identity(&request.session.session_id),
                store
                    .ephemeral_home_token(&request.session.session_id)
                    .as_ref(),
            )?;
            if let Some((_, grant)) = &dev_grant {
                // Only the scratch leaves are mounted, at their host paths.
                for leaf in marsh_daemon::dev_broker::SCRATCH_LEAVES {
                    let path = grant.scratch.join(leaf);
                    admitted_mounts.push(
                        AdmittedHostGrant::open(path.clone(), path, MountAccess::ReadWrite)
                            .map_err(backend_error)?,
                    );
                }
            }
            shell_attachment::require_controller(&attachment)?;
            self.finish_shell_warmup();
            let vm = self.sbx.ensure_shell_vm(&shell).map_err(backend_error)?;
            shell_attachment::require_controller(&attachment)?;
            if vm.cold_started {
                attachment.send(&AttachmentFrame::ColdBoot {
                    kit: "shell".into(),
                })?;
            }
            // Observe and pin stock's actual UUID BEFORE any grant submission.
            self.sbx.shell_vm_identity(&vm).map_err(backend_error)?;
            let quarantine = |detail: String| {
                self.sbx.quarantine_shell(&vm, admitted_mounts.clone());
                store.mark_shell_cleanup_uncertain(&session_id);
                DaemonError::ShellCleanupUncertain(format!(
                    "shell VM {}: {detail}; exit other shells and run host `marsh reset` or `marsh stop` for this selected home",
                    vm.name
                ))
            };
            let mounts = self
                .sbx
                .prepare_shell_mounts(&vm, &admitted_mounts)
                .map_err(|error| {
                    if matches!(
                        error,
                        marsh_sbx::SbxError::ShellMountCleanupUncertain(_)
                            | marsh_sbx::SbxError::QuarantinedShellVm(_)
                    ) {
                        quarantine(error.to_string())
                    } else {
                        backend_error(error)
                    }
                })?;
            let daemon_socket = match shell_attachment::require_controller(&attachment)
                .and_then(|()| EndpointPaths::for_home(&self.config.daemon_home))
            {
                Ok(paths) => paths.socket,
                Err(error) => {
                    if let Err(cleanup) = self.sbx.revoke_shell_mounts(&mounts) {
                        return Err(quarantine(cleanup.to_string()));
                    }
                    return Err(error);
                }
            };
            let token = match store.issue_relay_token(&request.session.session_id) {
                Ok(token) => token,
                Err(error) => {
                    if let Err(cleanup) = self.sbx.revoke_shell_mounts(&mounts) {
                        return Err(quarantine(cleanup.to_string()));
                    }
                    return Err(error);
                }
            };
            let relay = match shell_attachment::require_controller(&attachment).and_then(|()| {
                self.sbx
                    .launch_shell_relay(&vm, &self.config.relay_binary, &request.session.session_id)
                    .map_err(backend_error)
            }) {
                Ok(relay) => relay,
                Err(error) => {
                    store.revoke_relay_token(&token);
                    if let Err(cleanup) = self.sbx.revoke_shell_mounts(&mounts) {
                        return Err(quarantine(cleanup.to_string()));
                    }
                    return Err(backend_error(error));
                }
            };
            let socket_path = relay.socket_path.clone();
            let token_path = relay.token_path.clone();
            let relay =
                match shell_attachment::Relay::start(relay.process, daemon_socket, token.clone()) {
                    Ok(relay) => relay,
                    Err(error) => {
                        store.revoke_relay_token(&token);
                        return Err(quarantine(error.to_string()));
                    }
                };
            // The relay is a supervisor child: its in-band `Cleaned` frame
            // plus its supervisor-reported exit 0 is the only cleanup proof.
            let stop_relay = |relay: shell_attachment::Relay| {
                if relay.stop()?.cleaned_in_band {
                    Ok(())
                } else {
                    Err(DaemonError::ShellCleanupUncertain(
                        "guest relay did not report in-band cleanup".into(),
                    ))
                }
            };
            let rollback = |relay: shell_attachment::Relay, error: DaemonError| {
                store.revoke_relay_token(&token);
                if let Err(cleanup) = stop_relay(relay) {
                    return quarantine(cleanup.to_string());
                }
                if let Err(cleanup) = self.sbx.revoke_shell_mounts(&mounts) {
                    return quarantine(cleanup.to_string());
                }
                error
            };
            if let Err(error) = relay.ready(&attachment) {
                return Err(rollback(relay, error));
            }
            let mut extra_environment = Vec::new();
            if let Some((broker, grant)) = &dev_grant {
                if let Err(error) = broker.write_relay(grant, &socket_path, &token_path) {
                    return Err(rollback(relay, error));
                }
                let scratch = grant.scratch.display();
                extra_environment.extend([
                    format!("MARSH_DEV_SCRATCH={scratch}"),
                    format!("MARSH_DEV_DEPTH={}", grant.depth),
                    format!("MARSH_VM_PREFIX={}", grant.prefix),
                    format!("MARSH_DEV_SHELL_TEMPLATE={}", self.config.shell.image.as_str()),
                    format!("MARSH_DEV_GRANT={}", grant.id),
                    format!(
                        "PATH={scratch}/tmp/bin:/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
                    ),
                ]);
            }
            let arguments = request
                .arguments
                .into_iter()
                .map(OsString::from_vec)
                .collect::<Vec<_>>();
            if let Err(error) = shell_attachment::require_controller(&attachment) {
                return Err(rollback(relay, error));
            }
            let process = match self.sbx.attach_shell_with_relay(
                &vm,
                &request.session.launch_directory,
                request.session.terminal,
                request.session.terminal_size,
                &arguments,
                Some((&socket_path, &token_path)),
                &extra_environment,
            ) {
                Ok(process) => process,
                Err(error @ marsh_sbx::SbxError::ShellStartUncertain(_)) => {
                    // The start may have taken effect. Retain the
                    // exact VM/session grants, not the ordinary rollback path.
                    store.revoke_relay_token(&token);
                    let relay_cleanup = stop_relay(relay);
                    return Err(quarantine(format!("{error}; relay: {relay_cleanup:?}")));
                }
                Err(error) => return Err(rollback(relay, backend_error(error))),
            };
            // Readiness is in-band: the supervisor acknowledges the start only
            // after root-owned containment publication, before any user code.
            let ready = shell_attachment::require_controller(&attachment);
            if let Err(error) = ready {
                // We already own a spawned attachment. It must be cleaned and
                // joined, not dropped or reported ready because sbx spawned.
                let _ = attachment.shutdown();
                let result = shell_attachment::run(process, &attachment);
                let uncertain = match &result.cleanup {
                    shell_attachment::Cleanup::Verified => None,
                    shell_attachment::Cleanup::Uncertain(detail) => Some(detail.clone()),
                };
                if let Some(detail) = uncertain {
                    store.revoke_relay_token(&token);
                    let relay_cleanup = stop_relay(relay);
                    return Err(quarantine(format!(
                        "{error}; {detail}; relay: {relay_cleanup:?}"
                    )));
                }
                return Err(rollback(relay, error));
            }
            // Containment enrollment, not local sbx spawn, gates raw mode.
            let _ = attachment.send(&AttachmentFrame::ShellReady);
            let result = shell_attachment::run(process, &attachment);
            let code = result.status();
            let uncertain = match &result.cleanup {
                shell_attachment::Cleanup::Verified => None,
                shell_attachment::Cleanup::Uncertain(detail) => Some(detail.clone()),
            };
            store.revoke_relay_token(&token);
            // The supervisor's verified in-band cleanup removed the root-owned
            // containment record with the cgroup.
            let relay_cleanup = stop_relay(relay);
            if let Some(detail) = uncertain {
                return Err(quarantine(detail));
            }
            if let Err(error) = relay_cleanup {
                return Err(quarantine(error.to_string()));
            }
            if let Err(error) = self.sbx.revoke_shell_mounts(&mounts) {
                return Err(quarantine(error.to_string()));
            }
            Ok(code)
        })();
        let outcome = if request.session.ephemeral_home
            && !matches!(outcome, Err(DaemonError::ShellCleanupUncertain(_)))
        {
            let removed = self
                .sbx
                .reset_shell_vm_before(&shell, Instant::now() + Duration::from_secs(90));
            match (outcome, removed) {
                (Ok(code), Ok(_)) => Ok(code),
                (Err(error), Ok(_)) => Err(error),
                (prior, Err(cleanup)) => {
                    store.mark_shell_cleanup_uncertain(&session_id);
                    let prior = prior
                        .err()
                        .map_or_else(String::new, |error| format!("shell failed: {error}; "));
                    Err(DaemonError::ShellCleanupUncertain(format!(
                        "{prior}ephemeral shell VM {} removal was not verified: {cleanup}; run host `marsh reset` or `marsh stop` after other shells exit",
                        shell.name
                    )))
                }
            }
        } else {
            outcome
        };
        let outcome = match (&dev_grant, outcome) {
            (Some((broker, _)), outcome) => match (broker.revoke_session(&session_id), outcome) {
                (Ok(()), outcome) => outcome,
                (Err(cleanup), prior) => {
                    let prior = prior
                        .err()
                        .map_or_else(String::new, |error| format!("shell failed: {error}; "));
                    Err(DaemonError::ShellCleanupUncertain(format!(
                        "{prior}{cleanup}"
                    )))
                }
            },
            (None, outcome) => outcome,
        };
        let cleanup_uncertain = matches!(outcome, Err(DaemonError::ShellCleanupUncertain(_)));
        if cleanup_uncertain {
            // Keep session grant pins and admitted shell mount authority intact.
            // The daemon exposes CleanupUncertain and denies new relay work.
            outcome.map(|_| ())
        } else {
            finish_shell_session(&attachment, outcome, || {
                self.release_session_grants(&session_id, &store)
            })
        }
    }
}

fn finish_shell_session(
    attachment: &ServerAttachment,
    outcome: Result<i32, DaemonError>,
    release: impl FnOnce() -> Result<(), DaemonError>,
) -> Result<(), DaemonError> {
    let release = release();
    match (outcome, release) {
        (Ok(code), Ok(())) => attachment.send(&AttachmentFrame::Exited { code }),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(DaemonError::ShellCleanupUncertain(release))) => {
            Err(DaemonError::ShellCleanupUncertain(format!(
                "shell failed: {error}; session grant release failed: {release}"
            )))
        }
        (Err(error), Err(release)) => Err(DaemonError::InvalidState(format!(
            "shell failed: {error}; session grant release failed: {release}"
        ))),
    }
}

fn reconcile_worker_cleanup(
    sbx: &StockSbx,
    vm: &ReadyKitVm,
    started_container: Option<&marsh_contracts::ContainerId>,
    report: &WorkerReport,
    grant_cleanup: &Result<(), marsh_sbx::SbxError>,
    transport_lost: bool,
) -> Result<bool, DaemonError> {
    if started_container.is_some() && report.container_id.as_ref() != started_container {
        let _ = sbx.stop_quarantined(vm);
        return Err(DaemonError::InvalidState(
            "worker final report changed the runtime container identity".into(),
        ));
    }
    let cleanup_uncertain =
        report.cleanup == CleanupOutcome::Uncertain || report.quarantine || grant_cleanup.is_err();
    if transport_lost || cleanup_uncertain {
        let _ = sbx.stop_quarantined(vm);
    }
    Ok(cleanup_uncertain)
}

/// Bridges a job's split capability connections to this daemon's endpoint
/// with a per-attempt token that is revoked when the job ends
/// (`docs/design/workspaces.md` s4). Each connection's first request envelope is
/// re-signed with that token; everything else is forwarded unchanged.
struct CapabilityBridges {
    attempt: String,
    job: String,
    control: marsh_sbx::WorkerControlHandle,
    socket: PathBuf,
    store: DaemonStore,
    token: Option<String>,
    channels: BTreeMap<u32, mpsc::SyncSender<Vec<u8>>>,
}

impl CapabilityBridges {
    fn open(&mut self, channel: u32) {
        let token = match &self.token {
            Some(token) => token.clone(),
            None => match self.store.issue_capability_token(&self.job) {
                Ok(token) => self.token.insert(token).clone(),
                Err(_) => return,
            },
        };
        let (send, receive) = mpsc::sync_channel::<Vec<u8>>(64);
        self.channels.insert(channel, send);
        let (socket, control, attempt) = (
            self.socket.clone(),
            self.control.clone(),
            self.attempt.clone(),
        );
        thread::spawn(move || {
            let close = |control: &marsh_sbx::WorkerControlHandle| {
                let _ = control.send(&WorkerRequest::CapClose {
                    attempt: attempt.clone(),
                    channel,
                });
            };
            // Connect only once a whole first request arrived: an idle
            // connection holds a channel slot, never a daemon handshake.
            let mut daemon: Option<(std::os::unix::net::UnixStream, thread::JoinHandle<()>)> = None;
            let mut pending = Vec::new();
            for bytes in receive {
                if let Some((daemon, _)) = &mut daemon {
                    if daemon.write_all(&bytes).is_err() {
                        break;
                    }
                    continue;
                }
                pending.extend_from_slice(&bytes);
                let Some(length) = pending.get(..4).map(|prefix| {
                    u32::from_be_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize
                }) else {
                    continue;
                };
                if length > marsh_daemon::MAX_FRAME_BYTES {
                    break;
                }
                if pending.len() < 4 + length {
                    continue;
                }
                let Ok(frame) =
                    marsh_daemon::Client::reauthorize_envelope(&pending[4..4 + length], &token)
                else {
                    break;
                };
                let mut out = u32::try_from(frame.len())
                    .unwrap_or(0)
                    .to_be_bytes()
                    .to_vec();
                out.extend_from_slice(&frame);
                out.extend_from_slice(&pending[4 + length..]);
                let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&socket) else {
                    break;
                };
                let Ok(mut reader) = stream.try_clone() else {
                    break;
                };
                let (reply_control, reply_attempt) = (control.clone(), attempt.clone());
                let replies = thread::spawn(move || {
                    let mut buffer = vec![0_u8; 32 * 1024];
                    while let Ok(count) = reader.read(&mut buffer) {
                        if count == 0
                            || reply_control
                                .send(&WorkerRequest::CapData {
                                    attempt: reply_attempt.clone(),
                                    channel,
                                    bytes: buffer[..count].to_vec(),
                                })
                                .is_err()
                        {
                            break;
                        }
                    }
                });
                if stream.write_all(&out).is_err() {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    let _ = replies.join();
                    break;
                }
                daemon = Some((stream, replies));
            }
            if let Some((stream, replies)) = daemon {
                let _ = stream.shutdown(std::net::Shutdown::Write);
                let _ = replies.join();
            }
            close(&control);
        });
    }
}

impl Drop for CapabilityBridges {
    fn drop(&mut self) {
        self.channels.clear();
        if let Some(token) = self.token.take() {
            self.store.revoke_capability_token(&token);
        }
    }
}

fn forward_multiplexed_input(
    attachment: ServerAttachment,
    control: marsh_sbx::WorkerControlHandle,
    attempt: String,
    tty: bool,
    store: DaemonStore,
    job_id: String,
) {
    thread::spawn(move || {
        let mut cancellation_sent = false;
        loop {
            let Ok(frame) = attachment.receive() else {
                if !cancellation_sent {
                    // The caller is gone: cancel this job and its whole
                    // subtree at once, not one level per grace (s8).
                    store.cancel_tree(&job_id, false);
                    let _ = control.send(&WorkerRequest::Cancel {
                        attempt: attempt.clone(),
                    });
                }
                break;
            };
            if let AttachmentFrame::Signal { signal } = &frame
                && matches!(
                    parse_signal(signal),
                    JobSignal::Interrupt | JobSignal::Terminate | JobSignal::Hangup
                )
            {
                // Ctrl-C at the caller cancels the job's whole tree: INT to
                // every descendant now, KILL after the grace (s8). The frame
                // itself still reaches this job's container.
                store.cancel_tree(&job_id, false);
            }
            // With a terminal, Ctrl-C is a byte for the job's own PTY.
            if tty
                && let AttachmentFrame::Stdin { bytes } = &frame
                && bytes.contains(&0x03)
            {
                store.note_terminal_interrupt(&job_id);
            }
            let result =
                forward_multiplexed_frame(&attempt, frame, |request| control.send(request).is_ok());
            let ForwardFrameResult::Continue {
                closed_input: _,
                sent_cancellation,
            } = result
            else {
                break;
            };
            cancellation_sent |= sent_cancellation;
        }
    });
}

#[cfg(test)]
fn disconnect_request(attempt: &str, cancellation_sent: bool) -> Option<WorkerRequest> {
    (!cancellation_sent).then(|| WorkerRequest::Cancel {
        attempt: attempt.to_owned(),
    })
}

fn receive_worker_response(
    responses: &marsh_sbx::WorkerResponses,
    deadline: Instant,
) -> Result<WorkerResponse, bool> {
    receive_worker_response_with_deadline(deadline, |timeout| responses.recv_timeout(timeout))
}

fn receive_worker_response_with_deadline(
    deadline: Instant,
    receive_timeout: impl FnOnce(Duration) -> Result<WorkerResponse, std::sync::mpsc::RecvTimeoutError>,
) -> Result<WorkerResponse, bool> {
    receive_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|error| matches!(error, std::sync::mpsc::RecvTimeoutError::Timeout))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ForwardFrameResult {
    Continue {
        closed_input: bool,
        sent_cancellation: bool,
    },
    Stop,
}

fn forward_multiplexed_frame(
    attempt: &str,
    frame: AttachmentFrame,
    mut send: impl FnMut(&WorkerRequest) -> bool,
) -> ForwardFrameResult {
    let mut send_or_cancel = |request: WorkerRequest| {
        if send(&request) {
            true
        } else {
            let _ = send(&WorkerRequest::Cancel {
                attempt: attempt.to_owned(),
            });
            false
        }
    };
    let (closed_input, sent_cancellation) = match frame {
        AttachmentFrame::Stdin { bytes } => {
            for chunk in bytes.chunks(MAX_STREAM_CHUNK) {
                if !send_or_cancel(WorkerRequest::Input {
                    attempt: attempt.to_owned(),
                    bytes: chunk.to_vec(),
                }) {
                    return ForwardFrameResult::Stop;
                }
            }
            (false, false)
        }
        AttachmentFrame::StdinEof => {
            if !send_or_cancel(WorkerRequest::CloseInput {
                attempt: attempt.to_owned(),
            }) {
                return ForwardFrameResult::Stop;
            }
            (true, false)
        }
        AttachmentFrame::Signal { signal } => {
            let signal = parse_signal(&signal);
            let sent_cancellation = matches!(signal, JobSignal::Terminate | JobSignal::Kill);
            if !send_or_cancel(WorkerRequest::Signal {
                attempt: attempt.to_owned(),
                signal,
            }) {
                return ForwardFrameResult::Stop;
            }
            (false, sent_cancellation)
        }
        AttachmentFrame::Resize { rows, columns } => {
            if !send_or_cancel(WorkerRequest::Resize {
                attempt: attempt.to_owned(),
                size: TerminalSize { rows, columns },
            }) {
                return ForwardFrameResult::Stop;
            }
            (false, false)
        }
        _ => (false, false),
    };
    ForwardFrameResult::Continue {
        closed_input,
        sent_cancellation,
    }
}

#[cfg(test)]
fn forward_raw_input(
    attachment: ServerAttachment,
    writer: Box<dyn Write + Send>,
    control: Arc<dyn marsh_runtime::AttachmentControl>,
) {
    shell_attachment::forward_raw_input(attachment, writer, control);
}

#[cfg(test)]
fn stream_output(
    reader: Box<dyn Read + Send>,
    attachment: ServerAttachment,
    stderr: bool,
) -> thread::JoinHandle<StreamCopy> {
    stream_shell_output(reader, attachment, stderr, None)
}

fn stream_shell_output(
    mut reader: Box<dyn Read + Send>,
    attachment: ServerAttachment,
    stderr: bool,
    lost: Option<mpsc::Sender<()>>,
) -> thread::JoinHandle<StreamCopy> {
    thread::spawn(move || {
        let mut bytes = [0_u8; 16 * 1024];
        let mut first_output = None;
        let mut copied = 0_u64;
        let mut delivered = true;
        loop {
            let Ok(count) = reader.read(&mut bytes) else {
                delivered = false;
                if let Some(lost) = &lost {
                    let _ = lost.send(());
                }
                break;
            };
            if count == 0 {
                break;
            }
            first_output.get_or_insert_with(Instant::now);
            copied = copied.saturating_add(u64::try_from(count).unwrap_or(u64::MAX));
            let frame = if stderr {
                AttachmentFrame::Stderr {
                    bytes: bytes[..count].to_vec(),
                }
            } else {
                AttachmentFrame::Stdout {
                    bytes: bytes[..count].to_vec(),
                }
            };
            if delivered && attachment.send(&frame).is_err() {
                // Delivery loss starts session teardown but does not itself
                // establish guest cleanup uncertainty. Drain until cancellation.
                delivered = false;
                if let Some(lost) = &lost {
                    let _ = lost.send(());
                }
            }
        }
        StreamCopy {
            first_output,
            delivered,
            bytes: copied,
        }
    })
}

struct StreamCopy {
    first_output: Option<Instant>,
    delivered: bool,
    bytes: u64,
}

fn shell_delivery_exit(runtime_code: i32, delivered: bool) -> i32 {
    if delivered {
        runtime_code
    } else {
        CLEANUP_UNCERTAIN_EXIT_CODE
    }
}

fn cleanup_diagnostic(message: &str, output_budget: u64, output_used: u64) -> Vec<u8> {
    let diagnostic = format!("marsh: {message}; worker quarantined\n");
    let available = output_budget.saturating_sub(output_used);
    let mut length = usize::try_from(available)
        .unwrap_or(usize::MAX)
        .min(diagnostic.len());
    while !diagnostic.is_char_boundary(length) {
        length -= 1;
    }
    diagnostic.as_bytes()[..length].to_vec()
}

fn worker_public_exit(
    report: &WorkerReport,
    cleanup_uncertain: bool,
    output_complete: bool,
) -> (Option<i32>, String) {
    let (code, mut cause) = public_exit(
        &report.execution,
        cleanup_uncertain,
        output_complete && report.delivery == marsh_worker::DeliveryOutcome::Complete,
    );
    match report.delivery {
        marsh_worker::DeliveryOutcome::LimitExceeded { resource } => {
            cause.push_str("; delivery_");
            cause.push_str(&report_exit(&ExecutionOutcome::LimitExceeded { resource }).1);
        }
        marsh_worker::DeliveryOutcome::Cancelled => cause.push_str("; delivery_cancelled"),
        marsh_worker::DeliveryOutcome::Complete | marsh_worker::DeliveryOutcome::Failed => {}
    }
    (code, cause)
}

fn public_exit(
    execution: &ExecutionOutcome,
    cleanup_uncertain: bool,
    output_complete: bool,
) -> (Option<i32>, String) {
    let (code, cause) = report_exit(execution);
    if cleanup_uncertain {
        // ExecutionOutcome retains actual N independently. The public code must
        // agree with the persisted receipt, including for a nonzero worker N.
        (Some(CLEANUP_UNCERTAIN_EXIT_CODE), cause)
    } else if !output_complete {
        (
            Some(CLEANUP_UNCERTAIN_EXIT_CODE),
            format!("{cause}; output_delivery_failed"),
        )
    } else {
        (code, cause)
    }
}

fn finish_ambiguous_worker_start(
    store: &DaemonStore,
    job_id: &str,
    output_complete: bool,
) -> Result<i32, DaemonError> {
    const FAILURE: i32 = 125;
    store.finish_job_with_execution(
        job_id,
        ExecutionOutcome::Unknown,
        ExitStatus {
            code: Some(FAILURE),
            cause: "worker_start_ambiguous".into(),
        },
        output_complete,
        CleanupState::Uncertain,
        TimingReport::default(),
    )?;
    Ok(FAILURE)
}

#[cfg(test)]
fn join_stream(stream: thread::JoinHandle<StreamCopy>) -> Result<StreamCopy, DaemonError> {
    stream
        .join()
        .map_err(|_| DaemonError::InvalidState("output relay thread panicked".into()))
}

fn parse_signal(signal: &str) -> JobSignal {
    match signal.to_ascii_uppercase().as_str() {
        "TERM" | "SIGTERM" | "TERMINATE" => JobSignal::Terminate,
        "KILL" | "SIGKILL" => JobSignal::Kill,
        "HUP" | "SIGHUP" | "HANGUP" => JobSignal::Hangup,
        _ => JobSignal::Interrupt,
    }
}

struct JobGuard {
    store: DaemonStore,
    job_id: String,
    armed: bool,
}

struct GrantGuard {
    sbx: Arc<StockSbx>,
    vm: ReadyKitVm,
    grants: PreparedGrants,
    armed: bool,
}

impl GrantGuard {
    fn new(sbx: Arc<StockSbx>, vm: ReadyKitVm, grants: PreparedGrants) -> Self {
        Self {
            sbx,
            vm,
            grants,
            armed: true,
        }
    }

    fn complete(&mut self) {
        self.armed = false;
    }
}

impl Drop for GrantGuard {
    fn drop(&mut self) {
        if self.armed && self.sbx.revoke_grants(&self.grants).is_err() {
            let _ = self.sbx.stop_quarantined(&self.vm);
        }
    }
}

impl JobGuard {
    fn new(store: DaemonStore, job_id: String) -> Self {
        Self {
            store,
            job_id,
            armed: true,
        }
    }

    fn complete(&mut self) {
        self.armed = false;
    }

    fn reject_before_effects(&mut self, cause: String) -> Result<(), DaemonError> {
        self.store.finish_job_with_execution(
            &self.job_id,
            ExecutionOutcome::NotStarted,
            ExitStatus {
                code: Some(125),
                cause,
            },
            true,
            CleanupState::NotRequired,
            TimingReport::default(),
        )?;
        self.complete();
        Ok(())
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.store.abort_job(&self.job_id, "backend_failed");
        }
    }
}

/// How close to its inherited deadline a cancelled child counts as ended by
/// it (the parent's own wall kill lands just after the recorded deadline).
const PARENT_DEADLINE_SLACK_MS: u64 = 5_000;

fn report_exit(execution: &ExecutionOutcome) -> (Option<i32>, String) {
    match execution {
        ExecutionOutcome::Exited { code } => (Some(*code), "exited".into()),
        ExecutionOutcome::LimitExceeded { resource } => (
            None,
            format!(
                "limit:{}",
                match resource {
                    ResourceLimit::Memory => "memory",
                    ResourceLimit::Pids => "pids",
                    ResourceLimit::Output => "output",
                    ResourceLimit::Writable => "writable",
                    ResourceLimit::Wall => "wall",
                }
            ),
        ),
        ExecutionOutcome::SetupFailed { .. } => (Some(125), "setup_failed".into()),
        // These legacy LOCAL paths previously synthesized SupervisionFailed.
        // Preserve their cause/code while the common field now distinguishes a
        // conclusive rejection from an unobserved execution after transport loss.
        ExecutionOutcome::NotStarted
        | ExecutionOutcome::Unknown
        | ExecutionOutcome::SupervisionFailed => (None, "supervision_failed".into()),
    }
}

struct PhaseBoundaries {
    started: Instant,
    started_unix_ms: u64,
    vm_ready_at: Instant,
    admitted_at: Instant,
    mount_ready_at: Instant,
    worker_progress_at: Instant,
    output_relay_at: Instant,
    first_output_at: Option<Instant>,
    process_exit_at: Instant,
    output_drained_at: Instant,
    capture_at: Instant,
    cleanup_at: Instant,
}

fn timing_report(boundaries: &PhaseBoundaries) -> Result<TimingReport, DaemonError> {
    let phases = [
        ("vm_prepare", boundaries.started, boundaries.vm_ready_at),
        ("admission", boundaries.vm_ready_at, boundaries.admitted_at),
        (
            "mount_prepare",
            boundaries.admitted_at,
            boundaries.mount_ready_at,
        ),
        (
            "worker_start",
            boundaries.mount_ready_at,
            boundaries.worker_progress_at,
        ),
        (
            "execution",
            boundaries.worker_progress_at,
            boundaries.process_exit_at,
        ),
        (
            "output_drain",
            boundaries.process_exit_at,
            boundaries.output_drained_at,
        ),
        (
            "result_capture",
            boundaries.output_drained_at,
            boundaries.capture_at,
        ),
        ("cleanup", boundaries.capture_at, boundaries.cleanup_at),
    ];
    let mut durations_ms = BTreeMap::new();
    for (phase, from, to) in phases {
        durations_ms.insert(phase.into(), observed_duration(phase, from, to)?);
    }
    let mut milestones_unix_ms = BTreeMap::from([
        ("request_received".into(), boundaries.started_unix_ms),
        (
            "worker_progress".into(),
            observed_unix_ms(boundaries, "worker_progress", boundaries.worker_progress_at)?,
        ),
        (
            "process_exit".into(),
            observed_unix_ms(boundaries, "process_exit", boundaries.process_exit_at)?,
        ),
        (
            "output_drained".into(),
            observed_unix_ms(boundaries, "output_drained", boundaries.output_drained_at)?,
        ),
        (
            "completed".into(),
            observed_unix_ms(boundaries, "completed", boundaries.cleanup_at)?,
        ),
    ]);
    if let Some(first_output_at) = boundaries.first_output_at {
        if first_output_at < boundaries.output_relay_at
            || first_output_at > boundaries.output_drained_at
        {
            return Err(DaemonError::InvalidState(
                "first output timestamp is outside the observed relay interval".into(),
            ));
        }
        milestones_unix_ms.insert(
            "first_output".into(),
            observed_unix_ms(boundaries, "first_output", first_output_at)?,
        );
    }
    let wall_ms = durations_ms.values().sum();
    let execution_ms = durations_ms.get("execution").copied().unwrap_or_default();
    Ok(TimingReport {
        durations_ms,
        milestones_unix_ms,
        wall_ms,
        orchestration_ms: wall_ms - execution_ms,
    })
}

fn finish_job_with_timing(
    store: &DaemonStore,
    job_id: &str,
    execution: ExecutionOutcome,
    exit: ExitStatus,
    output_complete: bool,
    cleanup: CleanupState,
    boundaries: &PhaseBoundaries,
) -> Result<(), DaemonError> {
    match timing_report(boundaries) {
        Ok(timing) => store.finish_job_with_execution(
            job_id,
            execution,
            exit,
            output_complete,
            cleanup,
            timing,
        ),
        Err(error) => {
            store.finish_job_with_execution(
                job_id,
                execution,
                ExitStatus {
                    code: Some(125),
                    cause: format!("{}; timing_report_failed", exit.cause),
                },
                false,
                cleanup,
                TimingReport::default(),
            )?;
            Err(error)
        }
    }
}

fn observed_duration(phase: &str, from: Instant, to: Instant) -> Result<u64, DaemonError> {
    to.checked_duration_since(from).map(millis).ok_or_else(|| {
        DaemonError::InvalidState(format!("timing boundary reordered during {phase}"))
    })
}

fn observed_unix_ms(
    boundaries: &PhaseBoundaries,
    name: &str,
    point: Instant,
) -> Result<u64, DaemonError> {
    observed_duration(name, boundaries.started, point)
        .map(|elapsed| boundaries.started_unix_ms + elapsed)
}

/// Receipt mounts: the project and home, or a split branch's narrower set.
fn public_mounts(
    session: &SessionSpec,
    confinement: Option<&marsh_daemon::split_confinement::BranchConfinement>,
) -> Vec<PublicMount> {
    let mount = |target: &Path, write: bool| PublicMount {
        target: target.to_path_buf(),
        access: if write { "read_write" } else { "read_only" }.into(),
    };
    let mut mounts = match confinement {
        None => {
            let mut mounts = vec![mount(&session.launch_directory, true)];
            if let Some(marsh) = split_area(&session.launch_directory) {
                mounts.push(mount(&marsh, false));
            }
            mounts
        }
        Some(confinement) => std::iter::once(mount(&confinement.workspace, true))
            .chain(
                confinement
                    .binds
                    .iter()
                    .map(|(target, write)| mount(target, *write)),
            )
            .collect(),
    };
    mounts.push(mount(&session.guest_home, true));
    mounts
}

/// A child's `JobMount`s: its parent's recorded view, each target bound
/// from the prepared grant that covers it (the longest grant target), as a
/// subpath when narrower. Never wider than the grant.
fn view_job_mounts(grants: &[JobMount], view: &[PublicMount]) -> Result<Vec<JobMount>, String> {
    view.iter()
        .map(|mount| {
            let grant = grants
                .iter()
                .filter(|grant| grant.subpath.is_none() && mount.target.starts_with(&grant.target))
                .max_by_key(|grant| grant.target.components().count())
                .ok_or_else(|| format!("{} has no grant", mount.target.display()))?;
            let write = mount.access == "read_write";
            if write && grant.access != MountAccess::ReadWrite {
                return Err(format!("{} would widen its grant", mount.target.display()));
            }
            let subpath = mount
                .target
                .strip_prefix(&grant.target)
                .ok()
                .filter(|relative| !relative.as_os_str().is_empty())
                .map(Path::to_path_buf);
            Ok(JobMount {
                source: grant.source.clone(),
                target: mount.target.clone(),
                access: if write {
                    MountAccess::ReadWrite
                } else {
                    MountAccess::ReadOnly
                },
                subpath,
            })
        })
        .collect()
}

/// `<project>/.marsh` when it is a real directory.
fn split_area(project: &Path) -> Option<PathBuf> {
    let path = project.join(".marsh");
    std::fs::symlink_metadata(&path)
        .ok()
        .filter(std::fs::Metadata::is_dir)
        .map(|_| path)
}

/// Replaces the whole-project mount with subpath binds of the same prepared
/// grant: the fork read-write plus its Git metadata binds
/// (`docs/design/workspaces.md` section 8). Every other mount is unchanged.
fn confine_job_mounts(
    mounts: Vec<JobMount>,
    project: &Path,
    confinement: Option<&marsh_daemon::split_confinement::BranchConfinement>,
) -> Vec<JobMount> {
    let Some(confinement) = confinement else {
        // An ordinary job never writes split workspaces (defense in depth
        // beside the daemon's descriptor-only access to `.marsh`).
        let mut mounts = mounts;
        if let Some(marsh) = split_area(project)
            && let Some(whole) = mounts
                .iter()
                .find(|mount| mount.target == project && mount.subpath.is_none())
                .cloned()
        {
            mounts.push(JobMount {
                target: marsh,
                access: MountAccess::ReadOnly,
                subpath: Some(PathBuf::from(".marsh")),
                ..whole
            });
        }
        return mounts;
    };
    let mut confined = Vec::with_capacity(mounts.len() + confinement.binds.len());
    for mount in mounts {
        if mount.target != project || mount.subpath.is_some() {
            confined.push(mount);
            continue;
        }
        let binds = std::iter::once((&confinement.workspace, true)).chain(
            confinement
                .binds
                .iter()
                .map(|(target, write)| (target, *write)),
        );
        for (target, write) in binds {
            if let Ok(subpath) = target.strip_prefix(project) {
                confined.push(JobMount {
                    source: mount.source.clone(),
                    target: target.clone(),
                    access: if write {
                        MountAccess::ReadWrite
                    } else {
                        MountAccess::ReadOnly
                    },
                    subpath: Some(subpath.to_path_buf()),
                });
            }
        }
    }
    confined
}

fn unix_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, millis)
}

fn identity_digest(identity: &str) -> String {
    format!("{:x}", Sha256::digest(identity.as_bytes()))
}

fn millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Name the exact operator command that retires a quarantined Kit VM.
fn quarantine_hint(error: DaemonError, command: &str) -> DaemonError {
    match error {
        DaemonError::InvalidState(message) if message.contains(QUARANTINED_WORKER_TEXT) => {
            DaemonError::InvalidState(format!(
                "Kit {command} worker VM is quarantined after uncertain cleanup ({message}); run `marsh workers reset {command}` to retire it; its uncertain receipts stay uncertain and the next run creates a fresh VM"
            ))
        }
        other => other,
    }
}

const QUARANTINED_WORKER_TEXT: &str = "worker VM is quarantined and cannot be reused";

fn backend_error(error: impl std::fmt::Display) -> DaemonError {
    DaemonError::InvalidState(error.to_string())
}

/// Combine the live shell environment with host rules under the `JobSpec` wire
/// limit. Configured values always win; only inherited values are expendable.
pub(crate) fn merge_exported_environment(
    inherited: &marsh_contracts::ExportedEnvironment,
    rules: &config::EnvironmentConfig,
) -> Result<(marsh_contracts::ExportedEnvironment, Vec<String>), DaemonError> {
    marsh_contracts::validate_exported_environment(&rules.set)
        .map_err(|_| DaemonError::InvalidState("invalid configured exported environment".into()))?;

    let mut environment = rules.set.clone();
    let mut inherited_names = BTreeSet::new();
    let mut omitted = Vec::new();
    for (name, value) in inherited {
        if marsh_contracts::reserved_exported_environment_name(name)
            || marsh_contracts::placement_bound_environment_name(name)
            || rules.drop.contains(name)
            || rules.set.contains_key(name)
        {
            continue;
        }
        let one = BTreeMap::from([(name.clone(), value.clone())]);
        if marsh_contracts::validate_exported_environment(&one).is_err() {
            omitted.push(warning_environment_name(name));
            continue;
        }
        environment.insert(name.clone(), value.clone());
        inherited_names.insert(name.clone());
    }
    while marsh_contracts::validate_exported_environment(&environment).is_err() {
        let Some(largest) = inherited_names.iter().max_by(|left, right| {
            let size = |name: &String| name.len() + environment[name].len();
            size(left).cmp(&size(right)).then_with(|| left.cmp(right))
        }) else {
            return Err(DaemonError::InvalidState(
                "invalid configured exported environment".into(),
            ));
        };
        let name = largest.clone();
        inherited_names.remove(&name);
        environment.remove(&name);
        omitted.push(name);
    }
    Ok((environment, omitted))
}

fn warning_environment_name(name: &str) -> String {
    if name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        name.into()
    } else {
        "<invalid name>".into()
    }
}

fn completed_flight_outcome(
    observed: u64,
    completion: &KitFlightCompletion,
    cached: bool,
) -> Option<Result<(), String>> {
    if completion.epoch == observed {
        None
    } else if let Some(error) = &completion.error {
        Some(Err(error.clone()))
    } else if cached {
        Some(Ok(()))
    } else {
        None
    }
}

#[cfg(test)]
mod typed_receipt_callers;

#[cfg(test)]
mod tests {
    mod shell_status;
    use super::*;
    use marsh_daemon::SessionAuthority;
    use std::{
        collections::VecDeque,
        io::{self, Cursor},
        os::unix::net::UnixStream,
        sync::{Arc, Barrier, mpsc},
    };

    struct NeverRunner;

    #[test]
    fn placement_environment_rules_preserve_non_utf8_values() {
        let inherited = BTreeMap::from([
            ("RAW_VALUE".into(), vec![0xff, 0xfe, b'=', b'\n']),
            ("OVERRIDDEN".into(), vec![0xff]),
            ("DROP_ME".into(), vec![0xfe]),
            ("SSH_AUTH_SOCK".into(), b"/tmp/not-portable".to_vec()),
            ("PATH".into(), b"/shell/vm/only/bin".to_vec()),
        ]);
        let rules = config::EnvironmentConfig {
            drop: vec!["DROP_ME".into()],
            set: BTreeMap::from([("OVERRIDDEN".into(), vec![0xfe, 0xff])]),
        };
        let (merged, omitted) = merge_exported_environment(&inherited, &rules).unwrap();
        assert!(omitted.is_empty());
        assert_eq!(merged["RAW_VALUE"], [0xff, 0xfe, b'=', b'\n']);
        assert_eq!(merged["OVERRIDDEN"], [0xfe, 0xff]);
        assert!(!merged.contains_key("DROP_ME"));
        assert!(!merged.contains_key("SSH_AUTH_SOCK"));
        // The job keeps its image's PATH (e.g. the Claude Kit's ~/.local/bin).
        assert!(!merged.contains_key("PATH"));
    }

    #[test]
    fn configured_environment_survives_combined_export_limit() {
        let inherited = BTreeMap::from([
            ("A".into(), vec![b'a'; 16_000]),
            ("B".into(), vec![b'b'; 16_000]),
            ("C".into(), vec![b'c'; 16_000]),
            ("D".into(), vec![b'd'; 16_000]),
            ("DROP_ME".into(), "private".into()),
            ("REGION".into(), "host".into()),
        ]);
        assert!(marsh_contracts::validate_exported_environment(&inherited).is_ok());
        let rules = config::EnvironmentConfig {
            drop: vec!["DROP_ME".into()],
            set: BTreeMap::from([
                ("REGION".into(), "guest".into()),
                ("TOKEN".into(), vec![b't'; 4_000]),
            ]),
        };
        assert!(marsh_contracts::validate_exported_environment(&rules.set).is_ok());

        let (merged, omitted) = merge_exported_environment(&inherited, &rules).unwrap();
        assert!(marsh_contracts::validate_exported_environment(&merged).is_ok());
        assert_eq!(merged["REGION"], b"guest");
        assert_eq!(merged["TOKEN"], vec![b't'; 4_000]);
        assert!(!merged.contains_key("DROP_ME"));
        assert_eq!(omitted, ["D"]);
        assert!(!merged.contains_key("D"));
        assert!(merged.contains_key("A"));
        assert!(merged.contains_key("B"));
        assert!(merged.contains_key("C"));
    }

    struct SessionCleanupRunner(Mutex<VecDeque<marsh_runtime::CommandOutput>>);

    fn assert_failed_flight_is_shared_and_later_retry_can_succeed(message: &str) {
        let completion = Arc::new(Mutex::new(KitFlightCompletion::default()));
        let observed = Arc::new(Barrier::new(4));
        let completed = Arc::new(Barrier::new(4));
        let tasks = (0..3)
            .map(|_| {
                let completion = Arc::clone(&completion);
                let observed = Arc::clone(&observed);
                let completed = Arc::clone(&completed);
                thread::spawn(move || {
                    let before = completion.lock().unwrap().epoch;
                    observed.wait();
                    completed.wait();
                    completed_flight_outcome(before, &completion.lock().unwrap(), true)
                        .expect("overlapping caller must observe completed flight")
                        .expect_err("failed flight must be replayed")
                })
            })
            .collect::<Vec<_>>();
        observed.wait();
        *completion.lock().unwrap() = KitFlightCompletion {
            epoch: 1,
            error: Some(message.into()),
        };
        completed.wait();
        assert!(
            tasks
                .into_iter()
                .all(|task| task.join().unwrap() == message)
        );

        // A caller arriving after the failed flight observes its epoch and is
        // allowed to retry rather than replaying the old failure forever.
        assert!(completed_flight_outcome(1, &completion.lock().unwrap(), true).is_none());
        *completion.lock().unwrap() = KitFlightCompletion {
            epoch: 2,
            error: None,
        };
        assert_eq!(
            completed_flight_outcome(1, &completion.lock().unwrap(), true),
            Some(Ok(()))
        );
    }

    #[test]
    fn three_overlapping_validation_failures_are_coalesced_then_retry_succeeds() {
        assert_failed_flight_is_shared_and_later_retry_can_succeed("validation failed");
    }

    #[test]
    fn three_overlapping_repair_failures_are_coalesced_then_retry_succeeds() {
        assert_failed_flight_is_shared_and_later_retry_can_succeed("repair failed");
    }

    impl marsh_runtime::CommandRunner for SessionCleanupRunner {
        fn run(
            &self,
            _invocation: &marsh_runtime::Invocation,
        ) -> io::Result<marsh_runtime::CommandOutput> {
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| io::Error::other("unexpected stock SBX invocation"))
        }

        fn run_bounded(
            &self,
            invocation: &marsh_runtime::Invocation,
            _timeout: Duration,
        ) -> io::Result<marsh_runtime::CommandOutput> {
            self.run(invocation)
        }

        fn spawn_attached(
            &self,
            _invocation: &marsh_runtime::Invocation,
        ) -> io::Result<marsh_runtime::Attachment> {
            Err(io::Error::other("unexpected stock SBX attachment"))
        }
    }

    #[derive(Default)]
    struct MissingKitRunner {
        runs: Mutex<Vec<marsh_runtime::Invocation>>,
    }

    impl marsh_runtime::CommandRunner for MissingKitRunner {
        fn run(
            &self,
            invocation: &marsh_runtime::Invocation,
        ) -> io::Result<marsh_runtime::CommandOutput> {
            self.runs.lock().unwrap().push(invocation.clone());
            if invocation.arguments == ["ls", "--json"] {
                return Ok(marsh_runtime::CommandOutput {
                    exit_code: Some(0),
                    stdout: br#"{"sandboxes":[]}"#.to_vec(),
                    stderr: Vec::new(),
                });
            }
            Ok(marsh_runtime::CommandOutput {
                exit_code: Some(1),
                stdout: Vec::new(),
                stderr: b"sandbox not found".to_vec(),
            })
        }

        fn run_bounded(
            &self,
            invocation: &marsh_runtime::Invocation,
            _timeout: Duration,
        ) -> io::Result<marsh_runtime::CommandOutput> {
            self.run(invocation)
        }

        fn spawn_attached(
            &self,
            _invocation: &marsh_runtime::Invocation,
        ) -> io::Result<marsh_runtime::Attachment> {
            Err(io::Error::other("unexpected stock SBX attachment"))
        }
    }

    #[test]
    fn multiplexed_attachment_input_is_chunked_byte_faithfully() {
        let bytes = (0..MAX_STREAM_CHUNK * 2 + 17)
            .map(|index| u8::try_from(index % 251).unwrap())
            .collect::<Vec<_>>();
        let mut requests = Vec::new();
        let result = forward_multiplexed_frame(
            "attempt-1",
            AttachmentFrame::Stdin {
                bytes: bytes.clone(),
            },
            |request| {
                requests.push(request.clone());
                true
            },
        );
        assert_eq!(
            result,
            ForwardFrameResult::Continue {
                closed_input: false,
                sent_cancellation: false,
            }
        );
        assert_eq!(requests.len(), 3);
        let delivered = requests
            .into_iter()
            .flat_map(|request| match request {
                WorkerRequest::Input { bytes, .. } => bytes,
                request => panic!("unexpected request: {request:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(delivered, bytes);
    }

    #[test]
    fn multiplexed_attachment_input_cancels_after_transport_send_failure() {
        let mut requests = Vec::new();
        let mut sends = 0;
        let result = forward_multiplexed_frame(
            "attempt-1",
            AttachmentFrame::Stdin {
                bytes: vec![b'x'; MAX_STREAM_CHUNK + 1],
            },
            |request| {
                requests.push(request.clone());
                sends += 1;
                sends != 2
            },
        );
        assert_eq!(result, ForwardFrameResult::Stop);
        assert!(matches!(
            requests.as_slice(),
            [
                WorkerRequest::Input { bytes: first, .. },
                WorkerRequest::Input { bytes: second, .. },
                WorkerRequest::Cancel { attempt },
            ] if first.len() == MAX_STREAM_CHUNK && second.len() == 1 && attempt == "attempt-1"
        ));
    }

    #[test]
    fn attachment_disconnect_after_stdin_eof_still_cancels_attempt() {
        let eof = forward_multiplexed_frame("attempt-1", AttachmentFrame::StdinEof, |_| true);
        assert_eq!(
            eof,
            ForwardFrameResult::Continue {
                closed_input: true,
                sent_cancellation: false,
            }
        );
        assert_eq!(
            disconnect_request("attempt-1", false),
            Some(WorkerRequest::Cancel {
                attempt: "attempt-1".into(),
            })
        );
    }

    #[test]
    fn worker_progress_wait_has_a_finite_deadline_before_and_after_started() {
        let (send, receive) = mpsc::channel();
        send.send(WorkerResponse::Started {
            attempt: "attempt-1".into(),
            container_id: marsh_contracts::ContainerId::parse("a".repeat(64)).unwrap(),
        })
        .unwrap();
        let started = Instant::now();
        assert!(matches!(
            receive_worker_response_with_deadline(started + Duration::from_secs(1), |timeout| {
                receive.recv_timeout(timeout)
            }),
            Ok(WorkerResponse::Started { .. })
        ));
        let terminal = receive_worker_response_with_deadline(
            Instant::now() + Duration::from_millis(25),
            |timeout| receive.recv_timeout(timeout),
        );
        assert_eq!(terminal, Err(true));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    impl marsh_runtime::CommandRunner for NeverRunner {
        fn run(
            &self,
            _invocation: &marsh_runtime::Invocation,
        ) -> io::Result<marsh_runtime::CommandOutput> {
            Err(io::Error::other("unexpected stock SBX invocation"))
        }

        fn run_bounded(
            &self,
            _invocation: &marsh_runtime::Invocation,
            _timeout: Duration,
        ) -> io::Result<marsh_runtime::CommandOutput> {
            Err(io::Error::other("unexpected stock SBX invocation"))
        }

        fn spawn_attached(
            &self,
            _invocation: &marsh_runtime::Invocation,
        ) -> io::Result<marsh_runtime::Attachment> {
            Err(io::Error::other("unexpected stock SBX attachment"))
        }
    }

    fn test_stock(runner: Arc<dyn marsh_runtime::CommandRunner>) -> StockSbx {
        static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        let root = ROOT.get_or_init(|| {
            tempfile::Builder::new()
                .prefix("marsh-backend-grants-")
                .tempdir_in(fs::canonicalize(std::env::temp_dir()).unwrap())
                .unwrap()
        });
        StockSbx::new("sbx", runner).with_ephemeral_root_for_test(root.path().join("ephemeral"))
    }

    fn backend_for_kit_identity(
        daemon_home: &std::path::Path,
        commands: BTreeMap<String, RegisteredKit>,
    ) -> StockDaemonBackend {
        let shell_image = marsh_contracts::OciImage::parse(format!(
            "dhi.example/shell@sha256:{}",
            "f".repeat(64)
        ))
        .unwrap();
        StockDaemonBackend::new(
            Arc::new(test_stock(Arc::new(NeverRunner))),
            commands,
            BackendConfig {
                worker_binary: daemon_home.join("worker"),
                relay_binary: daemon_home.join("relay"),
                daemon_home: daemon_home.to_owned(),
                control_home: daemon_home.join("control"),
                protected_guest_roots: Vec::new(),
                shell: ShellVmSpec {
                    name: "marsh-shell-test".into(),
                    image: shell_image,
                    shell_binary: daemon_home.join("shell"),
                    user: marsh_sbx::ShellUser {
                        name: "alice".into(),
                        uid: 501,
                        gid: 20,
                        home: "/Users/alice".into(),
                    },
                },
                resources: marsh_contracts::JobResources {
                    cpu_millis: 1,
                    memory_bytes: 1,
                    pids: 1,
                    writable_bytes: 1,
                    output_bytes: 1,
                    wall_seconds: 1,
                },
                env: config::EnvironmentConfig::default(),
            },
        )
    }

    #[test]
    fn admitted_session_grants_reject_protected_ancestor_equal_and_descendant() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let protected = root_path.join("protected");
        let child = protected.join("child");
        let safe = root_path.join("safe");
        fs::create_dir(&protected).unwrap();
        fs::create_dir(&child).unwrap();
        fs::create_dir(&safe).unwrap();
        let mut backend = backend_for_kit_identity(root.path(), BTreeMap::new());
        backend.config.protected_guest_roots = vec![protected.clone()];
        let mut session = SessionSpec {
            session_id: "fixture".into(),
            username: "fixture".into(),
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            launch_directory: safe.clone(),
            guest_home: safe.clone(),
            home_backing: safe.clone(),
            ephemeral_home: false,
            terminal: false,
            terminal_size: None,
        };
        assert!(
            backend
                .admitted_session_grants(&session, &safe, None)
                .is_ok()
        );
        for mount in [root.path(), protected.as_path(), child.as_path()] {
            session.launch_directory = mount.to_path_buf();
            assert!(
                backend
                    .admitted_session_grants(&session, &safe, None)
                    .is_err()
            );
            session.launch_directory = safe.clone();
            session.home_backing = mount.to_path_buf();
            assert!(
                backend
                    .admitted_session_grants(&session, &safe, None)
                    .is_err()
            );
            session.home_backing = safe.clone();
        }
    }

    #[test]
    fn ordinary_overlapping_project_home_is_rejected_by_open_shell_before_stock_work() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let project = root.join("project");
        let home = project.join("home");
        fs::create_dir_all(&home).unwrap();
        let backend = backend_for_kit_identity(&root, BTreeMap::new());
        let store = DaemonStore::new(&root);
        let session_id = store.attach_shell(
            7,
            marsh_daemon::SessionAuthority {
                username: "fixture".into(),
                uid: rustix::process::getuid().as_raw(),
                gid: rustix::process::getgid().as_raw(),
                launch_directory: project.clone(),
                guest_home: home.clone(),
                home_backing: home.clone(),
                ephemeral_home: false,
            },
        );
        let spec = ShellSpec {
            dev: false,
            session: SessionSpec {
                session_id,
                username: "fixture".into(),
                uid: rustix::process::getuid().as_raw(),
                gid: rustix::process::getgid().as_raw(),
                launch_directory: project,
                guest_home: home.clone(),
                home_backing: home,
                ephemeral_home: false,
                terminal: false,
                terminal_size: None,
            },
            arguments: vec![b"-c".to_vec(), b"true".to_vec()],
        };
        let (server, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let started = Instant::now();
        let error = backend
            .open_shell(spec, ServerAttachment::new(server).unwrap(), store.clone())
            .unwrap_err();
        assert!(
            error.to_string().contains("source batch overlaps"),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(store.status(None).workers.is_empty());
        assert!(store.jobs().jobs.is_empty());
    }

    #[test]
    fn pinned_project_replacement_after_attachment_fails_before_vm_work() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let project = root.join("project");
        let home = root.join("home");
        fs::create_dir(&project).unwrap();
        fs::create_dir(&home).unwrap();
        let original =
            AdmittedHostGrant::open(project.clone(), project.clone(), MountAccess::ReadWrite)
                .unwrap();
        let expected = original.source_identity();
        let store = DaemonStore::new(&root);
        let session_id = store
            .attach_pinned_shell(
                7,
                marsh_daemon::SessionAuthority {
                    username: "fixture".into(),
                    uid: rustix::process::getuid().as_raw(),
                    gid: rustix::process::getgid().as_raw(),
                    launch_directory: project.clone(),
                    guest_home: home.clone(),
                    home_backing: home.clone(),
                    ephemeral_home: false,
                },
                expected,
            )
            .unwrap();
        let session = SessionSpec {
            session_id,
            username: "fixture".into(),
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            launch_directory: project.clone(),
            guest_home: home.clone(),
            home_backing: home,
            ephemeral_home: false,
            terminal: false,
            terminal_size: None,
        };
        // Deterministically exercise the gap between host attachment and the
        // backend opening its retained mount source, without invoking stock SBX.
        fs::rename(&project, root.join("original-project")).unwrap();
        fs::create_dir(&project).unwrap();
        let backend = backend_for_kit_identity(&root, BTreeMap::new());
        let (server, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let error = backend
            .open_shell(
                ShellSpec {
                    dev: false,
                    session,
                    arguments: vec![],
                },
                ServerAttachment::new(server).unwrap(),
                store.clone(),
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("attached project identity changed before mount admission"),
            "{error}"
        );
        assert!(store.status(None).workers.is_empty());
        assert!(store.jobs().jobs.is_empty());
    }

    #[test]
    fn acp_adapter_must_bind_exact_registered_kit_generation() {
        let home = tempfile::tempdir().unwrap();
        let identity = format!("registry.example/acp@sha256:{}", "a".repeat(64));
        let workload = NativeKitRef::immutable_oci(identity.clone()).unwrap();
        let commands = BTreeMap::from([("agent-kit".into(), RegisteredKit { workload })]);
        let declaration = AgentAdapterDeclaration {
            schema_version: 1,
            name: "agent-session".into(),
            protocol: marsh_acp::AgentProtocol::AcpV1,
            command: "agent-kit".into(),
            workload_digest: identity.clone(),
            required_capabilities: vec![],
            arguments: vec![],
            description: None,
        };
        let mut registry = AgentRegistry::new();
        registry.register(declaration.clone()).unwrap();
        let backend = backend_for_kit_identity(home.path(), commands)
            .with_agents(&registry)
            .unwrap();
        assert_eq!(
            backend
                .resolve_acp_agent("agent-session")
                .unwrap()
                .workload_digest,
            identity
        );
        assert!(backend.resolve_acp_agent("agent-kit").is_err());
        let mut wrong = AgentRegistry::new();
        wrong
            .register(AgentAdapterDeclaration {
                workload_digest: format!("registry.example/acp@sha256:{}", "b".repeat(64)),
                ..declaration
            })
            .unwrap();
        let workload = NativeKitRef::immutable_oci(identity).unwrap();
        let commands = BTreeMap::from([("agent-kit".into(), RegisteredKit { workload })]);
        assert!(matches!(
            backend_for_kit_identity(home.path(), commands).with_agents(&wrong),
            Err(DaemonError::InvalidState(_))
        ));
    }

    #[test]
    fn acp_local_source_generation_is_pinned_at_daemon_startup() {
        let home = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        let source_path = source.path().join("kit");
        fs::create_dir(&source_path).unwrap();
        fs::write(source_path.join("kit.yaml"), "schemaVersion: \"3\"\n").unwrap();
        let workload =
            NativeKitRef::local_v3_source(fs::canonicalize(&source_path).unwrap()).unwrap();
        let generation = workload.generation_identity().unwrap();
        let commands = BTreeMap::from([("agent-kit".into(), RegisteredKit { workload })]);
        let mut registry = AgentRegistry::new();
        registry
            .register(AgentAdapterDeclaration {
                schema_version: 1,
                name: "agent-session".into(),
                protocol: marsh_acp::AgentProtocol::AcpV1,
                command: "agent-kit".into(),
                workload_digest: String::new(),
                required_capabilities: vec![],
                arguments: vec![],
                description: None,
            })
            .unwrap();
        let backend = backend_for_kit_identity(home.path(), commands)
            .with_agents(&registry)
            .unwrap();
        assert_eq!(
            backend
                .resolve_acp_agent("agent-session")
                .unwrap()
                .workload_digest,
            generation
        );
        backend.kit_spec("agent-kit").unwrap();

        fs::write(
            source_path.join("kit.yaml"),
            "schemaVersion: \"3\"\n# edited\n",
        )
        .unwrap();
        assert!(matches!(
            backend.resolve_acp_agent("agent-session"),
            Err(DaemonError::InvalidState(_))
        ));
        assert!(matches!(
            backend.kit_spec("agent-kit"),
            Err(DaemonError::InvalidState(_))
        ));
    }

    #[test]
    fn acp_immutable_kit_requires_explicit_generation_at_startup() {
        let home = tempfile::tempdir().unwrap();
        let workload =
            NativeKitRef::immutable_oci(format!("registry.example/acp@sha256:{}", "a".repeat(64)))
                .unwrap();
        let commands = BTreeMap::from([("agent-kit".into(), RegisteredKit { workload })]);
        let mut registry = AgentRegistry::new();
        registry
            .register(AgentAdapterDeclaration {
                schema_version: 1,
                name: "agent-session".into(),
                protocol: marsh_acp::AgentProtocol::AcpV1,
                command: "agent-kit".into(),
                workload_digest: String::new(),
                required_capabilities: vec![],
                arguments: vec![],
                description: None,
            })
            .unwrap();
        assert!(matches!(
            backend_for_kit_identity(home.path(), commands).with_agents(&registry),
            Err(DaemonError::InvalidState(_))
        ));
    }

    #[test]
    fn session_close_retains_the_warm_grant_mount_without_stock_calls() {
        let home = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        let runner = Arc::new(SessionCleanupRunner(Mutex::new(VecDeque::from([
            marsh_runtime::CommandOutput {
                exit_code: Some(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
            marsh_runtime::CommandOutput {
                exit_code: Some(1),
                stdout: Vec::new(),
                stderr: b"unmount failed".to_vec(),
            },
            marsh_runtime::CommandOutput {
                exit_code: Some(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        ]))));
        let sbx = Arc::new(test_stock(runner.clone()));
        let vm = ReadyKitVm {
            name: "marsh-kit-session-cleanup".into(),
            kit_ref: "kit:test".into(),
            lifecycle_workspace: home.path().join("lifecycle"),
            cold_started: false,
            job_image: marsh_contracts::OciImage::parse(format!(
                "registry.example/agent@sha256:{}",
                "a".repeat(64)
            ))
            .unwrap(),
            worker_binary: home.path().join("worker"),
        };
        let grant = AdmittedHostGrant::open(
            source.path().canonicalize().unwrap(),
            "/Users/alice/project".into(),
            MountAccess::ReadWrite,
        )
        .unwrap();
        sbx.prepare_session_grants(&vm, "session-one", &[grant])
            .unwrap();
        let template = backend_for_kit_identity(home.path(), BTreeMap::new());
        let backend = StockDaemonBackend::new(Arc::clone(&sbx), BTreeMap::new(), template.config);
        let store = DaemonStore::new(home.path());
        store
            .register_worker(public_worker_status(&store, &vm))
            .unwrap();

        // The retained mount is released at VM retire, not at session close:
        // the queued "unmount failed" output is never consumed.
        backend
            .release_session_grants("session-one", &store)
            .unwrap();
        assert_eq!(runner.0.lock().unwrap().len(), 2);
        let worker = &store.status(None).workers[0];
        assert_ne!(worker.health, WorkerHealth::Quarantined);
    }

    fn cleanup_test_vm(home: &std::path::Path) -> ReadyKitVm {
        ReadyKitVm {
            name: "marsh-kit-cleanup-test".into(),
            kit_ref: "kit:test".into(),
            lifecycle_workspace: home.join("lifecycle"),
            cold_started: false,
            job_image: marsh_contracts::OciImage::parse(format!(
                "registry.example/agent@sha256:{}",
                "a".repeat(64)
            ))
            .unwrap(),
            worker_binary: home.join("worker"),
        }
    }

    fn reserved_cleanup_job(
        home: &std::path::Path,
        vm: &ReadyKitVm,
        container: &marsh_contracts::ContainerId,
    ) -> (DaemonStore, String) {
        let store = DaemonStore::new(home);
        let session = store.attach_shell(
            1,
            SessionAuthority {
                username: "alice".into(),
                uid: 501,
                gid: 20,
                launch_directory: home.to_owned(),
                guest_home: "/Users/alice".into(),
                home_backing: home.to_owned(),
                ephemeral_home: false,
            },
        );
        store
            .register_worker(public_worker_status(&store, vm))
            .unwrap();
        let (job, _) = store
            .begin_job(NewJob {
                session_id: session,
                command: "fixture".into(),
                kit_ref: vm.kit_ref.clone(),
                workload_image: vm.job_image.as_str().into(),
                mounts: Vec::new(),
            })
            .unwrap();
        store.reserve_worker(&job, &vm.name, &vm.name).unwrap();
        store
            .mark_running(&job, &vm.name, &vm.name, container.as_str())
            .unwrap();
        (store, job)
    }

    #[test]
    fn revoke_failure_quarantines_adapter_and_store_worker() {
        let home = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        let vm = cleanup_test_vm(home.path());
        let container = marsh_contracts::ContainerId::parse("a".repeat(64)).unwrap();
        let sbx = StockSbx::new(
            "sbx",
            Arc::new(SessionCleanupRunner(Mutex::new(VecDeque::from([
                marsh_runtime::CommandOutput {
                    exit_code: Some(0),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                },
            ])))),
        );
        let (store, job) = reserved_cleanup_job(home.path(), &vm, &container);
        let report = WorkerReport {
            container_id: Some(container),
            execution: ExecutionOutcome::Exited { code: 0 },
            delivery: marsh_worker::DeliveryOutcome::Complete,
            control_errors: 0,
            retained_processes: Vec::new(),
            cleanup: CleanupOutcome::Verified,
            quarantine: false,
        };
        let cleanup = Err(marsh_sbx::SbxError::UnknownShellMount("/grant".into()));

        assert!(
            reconcile_worker_cleanup(
                &sbx,
                &vm,
                report.container_id.as_ref(),
                &report,
                &cleanup,
                false
            )
            .unwrap()
        );
        store
            .finish_job(
                &job,
                ExitStatus {
                    code: Some(125),
                    cause: "grant cleanup failed".into(),
                },
                false,
                CleanupState::Uncertain,
                TimingReport::default(),
            )
            .unwrap();
        assert_eq!(
            store.status(None).workers[0].health,
            WorkerHealth::Quarantined
        );
        let grant = AdmittedHostGrant::open(
            source.path().canonicalize().unwrap(),
            "/Users/alice/project".into(),
            MountAccess::ReadWrite,
        )
        .unwrap();
        assert!(matches!(
            sbx.prepare_session_grants(&vm, "session-two", &[grant]),
            Err(marsh_sbx::SbxError::QuarantinedVm(_))
        ));
    }

    #[test]
    fn identity_mismatch_quarantines_adapter_and_job_guard_store_state() {
        let home = tempfile::tempdir().unwrap();
        let source = tempfile::tempdir().unwrap();
        let vm = cleanup_test_vm(home.path());
        let started = marsh_contracts::ContainerId::parse("a".repeat(64)).unwrap();
        let reported = marsh_contracts::ContainerId::parse("b".repeat(64)).unwrap();
        let sbx = StockSbx::new(
            "sbx",
            Arc::new(SessionCleanupRunner(Mutex::new(VecDeque::from([
                marsh_runtime::CommandOutput {
                    exit_code: Some(0),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                },
            ])))),
        );
        let (store, job) = reserved_cleanup_job(home.path(), &vm, &started);
        let guard = JobGuard::new(store.clone(), job);
        let report = WorkerReport {
            container_id: Some(reported),
            execution: ExecutionOutcome::Exited { code: 0 },
            delivery: marsh_worker::DeliveryOutcome::Complete,
            control_errors: 0,
            retained_processes: Vec::new(),
            cleanup: CleanupOutcome::Verified,
            quarantine: false,
        };

        assert!(
            reconcile_worker_cleanup(&sbx, &vm, Some(&started), &report, &Ok(()), false).is_err()
        );
        drop(guard);
        assert_eq!(
            store.status(None).workers[0].health,
            WorkerHealth::Quarantined
        );
        let grant = AdmittedHostGrant::open(
            source.path().canonicalize().unwrap(),
            "/Users/alice/project".into(),
            MountAccess::ReadWrite,
        )
        .unwrap();
        assert!(matches!(
            sbx.prepare_session_grants(&vm, "session-two", &[grant]),
            Err(marsh_sbx::SbxError::QuarantinedVm(_))
        ));
    }

    struct SignalRecorder(mpsc::Sender<JobSignal>);

    impl marsh_runtime::AttachmentControl for SignalRecorder {
        fn signal(&self, signal: JobSignal) -> io::Result<()> {
            self.0
                .send(signal)
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "test receiver closed"))
        }

        fn resize(&self, _size: TerminalSize) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn public_signal_names_are_case_insensitive() {
        assert_eq!(parse_signal("interrupt"), JobSignal::Interrupt);
        assert_eq!(parse_signal("terminate"), JobSignal::Terminate);
        assert_eq!(parse_signal("hangup"), JobSignal::Hangup);
        assert_eq!(parse_signal("sigterm"), JobSignal::Terminate);
        assert_eq!(parse_signal("SIGHUP"), JobSignal::Hangup);
    }

    #[test]
    fn public_worker_identity_is_the_exact_native_kit_reference() {
        let home = tempfile::tempdir().unwrap();
        let store = DaemonStore::new(home.path());
        let kit_ref = format!("registry.example/fixture@sha256:{}", "d".repeat(64));
        let vm = ReadyKitVm {
            name: "marsh-kit-fixture".into(),
            kit_ref: kit_ref.clone(),
            lifecycle_workspace: "/tmp/marsh-lifecycle".into(),
            cold_started: false,
            job_image: marsh_contracts::OciImage::parse(kit_ref.clone()).unwrap(),
            worker_binary: "/tmp/test-marsh-worker".into(),
        };
        let status = public_worker_status(&store, &vm);
        assert_eq!(status.kit_ref, kit_ref);
    }

    #[test]
    fn published_kit_vm_identity_uses_daemon_scope_and_exact_workload_not_alias_or_home() {
        let first_home = tempfile::tempdir().unwrap();
        let second_home = tempfile::tempdir().unwrap();
        let workload = NativeKitRef::immutable_oci(format!(
            "registry.example/agent@sha256:{}",
            "a".repeat(64)
        ))
        .unwrap();
        let commands = BTreeMap::from([
            (
                "agent".into(),
                RegisteredKit {
                    workload: workload.clone(),
                },
            ),
            (
                "agent-alias".into(),
                RegisteredKit {
                    workload: workload.clone(),
                },
            ),
        ]);
        let first = backend_for_kit_identity(first_home.path(), commands.clone());
        let by_name = first.kit_spec("agent").unwrap();
        let by_alias = first.kit_spec("agent-alias").unwrap();
        let workload_digest = identity_digest(workload.identity());
        assert_eq!(by_name.name, by_alias.name);
        assert!(by_name.name.starts_with("marsh-k-"));
        assert_eq!(by_name.workload_kit, by_alias.workload_kit);
        assert_eq!(by_name.lifecycle_workspace, by_alias.lifecycle_workspace);
        assert_eq!(
            by_name.lifecycle_workspace.file_name().unwrap(),
            workload_digest.as_str()
        );
        assert!(by_name.lifecycle_workspace.starts_with(first_home.path()));
        assert_eq!(
            fs::symlink_metadata(&by_name.lifecycle_workspace)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        let second = backend_for_kit_identity(second_home.path(), commands);
        let other_scope = second.kit_spec("agent").unwrap();
        assert_ne!(by_name.name, other_scope.name);
        assert_ne!(by_name.lifecycle_workspace, other_scope.lifecycle_workspace);
    }

    #[test]
    fn reset_selection_supports_named_and_all_and_rejects_unknown_before_effects() {
        let home = tempfile::tempdir().unwrap();
        let first = NativeKitRef::immutable_oci(format!(
            "registry.example/first@sha256:{}",
            "a".repeat(64)
        ))
        .unwrap();
        let second = NativeKitRef::immutable_oci(format!(
            "registry.example/second@sha256:{}",
            "b".repeat(64)
        ))
        .unwrap();
        let commands = BTreeMap::from([
            ("first".into(), RegisteredKit { workload: first }),
            ("second".into(), RegisteredKit { workload: second }),
        ]);
        let runner = Arc::new(MissingKitRunner::default());
        let template = backend_for_kit_identity(home.path(), commands.clone());
        let backend = StockDaemonBackend::new(
            Arc::new(test_stock(runner.clone())),
            commands,
            template.config,
        );
        let store = DaemonStore::new(home.path());

        assert_eq!(
            backend
                .reset_workers(&LoadSelection::Kits(vec!["second".into()]), store.clone(),)
                .unwrap(),
            vec!["second".to_string()]
        );
        assert_eq!(runner.runs.lock().unwrap().len(), 1);
        assert_eq!(
            backend
                .reset_workers(&LoadSelection::All, store.clone())
                .unwrap(),
            vec!["first".to_string(), "second".to_string()]
        );
        assert_eq!(runner.runs.lock().unwrap().len(), 3);
        let before = runner.runs.lock().unwrap().len();
        assert!(matches!(
            backend.reset_workers(&LoadSelection::Kits(vec!["unknown".into()]), store,),
            Err(DaemonError::NotFound(_))
        ));
        assert_eq!(runner.runs.lock().unwrap().len(), before);
    }

    #[test]
    fn failed_live_kit_install_keeps_registry_and_control_file_unchanged() {
        let home = tempfile::tempdir().unwrap();
        let runner = Arc::new(MissingKitRunner::default());
        let template = backend_for_kit_identity(home.path(), BTreeMap::new());
        fs::write(&template.config.worker_binary, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(
            &template.config.worker_binary,
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let backend = StockDaemonBackend::new(
            Arc::new(test_stock(runner.clone())),
            BTreeMap::new(),
            template.config,
        );
        let store = DaemonStore::new(home.path());
        let reference = format!("docker.io/example/alternate@sha256:{}", "a".repeat(64));
        assert!(
            backend
                .install_kit("alternate".into(), reference, store)
                .is_err()
        );
        assert!(backend.registered_commands().unwrap().is_empty());
        assert!(!backend.config.control_home.join("commands.json").exists());
        assert!(!runner.runs.lock().unwrap().is_empty());
    }

    #[test]
    fn local_kit_generation_is_hashed_into_safe_worker_paths() {
        let home = tempfile::tempdir().unwrap();
        let source_root = tempfile::tempdir().unwrap();
        let source = source_root.path().join("kit source @ local-v3");
        fs::create_dir(&source).unwrap();
        let workload = NativeKitRef::local_v3_source(fs::canonicalize(source).unwrap()).unwrap();
        let backend = backend_for_kit_identity(
            home.path(),
            BTreeMap::from([
                (
                    "local".into(),
                    RegisteredKit {
                        workload: workload.clone(),
                    },
                ),
                (
                    "alias".into(),
                    RegisteredKit {
                        workload: workload.clone(),
                    },
                ),
            ]),
        );

        let first = backend.kit_spec("local").unwrap();
        let alias = backend.kit_spec("alias").unwrap();
        let generation = workload.generation_identity().unwrap();
        let workload_digest = identity_digest(&generation);
        assert_eq!(first, alias);
        assert!(first.name.starts_with("marsh-k-"));
        assert!(
            first
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        );
        assert_eq!(
            first.lifecycle_workspace.file_name().unwrap(),
            workload_digest.as_str()
        );
        assert!(
            !first
                .lifecycle_workspace
                .to_string_lossy()
                .contains("kit source")
        );
    }

    #[test]
    fn local_source_edit_allocates_new_generation_without_touching_active_old_generation() {
        let home = tempfile::tempdir().unwrap();
        let source_root = tempfile::tempdir().unwrap();
        let source = source_root.path().join("kit");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("kit.yaml"), "schemaVersion: \"3\"\n").unwrap();
        let workload = NativeKitRef::local_v3_source(fs::canonicalize(&source).unwrap()).unwrap();
        let backend = backend_for_kit_identity(
            home.path(),
            BTreeMap::from([("local".into(), RegisteredKit { workload })]),
        );
        let old = backend.kit_spec("local").unwrap();
        let old_result = old.lifecycle_workspace.join("active-job-result");
        let (written_tx, written_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let old_job = std::thread::spawn(move || {
            fs::write(&old_result, b"preserved").unwrap();
            written_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            fs::read(old_result).unwrap()
        });
        written_rx.recv().unwrap();

        fs::write(
            source.join("kit.yaml"),
            "schemaVersion: \"3\"\ndisplayName: edited\n",
        )
        .unwrap();
        let new = backend.kit_spec("local").unwrap();
        release_tx.send(()).unwrap();

        assert_ne!(old.name, new.name);
        assert_ne!(old.lifecycle_workspace, new.lifecycle_workspace);
        assert_eq!(old_job.join().unwrap(), b"preserved");
    }

    #[test]
    fn backend_ready_cache_hit_requires_live_exact_stock_kit() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("worker"), b"trusted worker").unwrap();
        fs::write(
            home.path().join("marsh-byte-exec-linux-arm64"),
            b"byte exec",
        )
        .unwrap();
        fs::write(home.path().join("marsh-local-linux-arm64"), b"job artifact").unwrap();
        fs::set_permissions(
            home.path().join("worker"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let workload = NativeKitRef::immutable_oci(format!(
            "registry.example/agent@sha256:{}",
            "a".repeat(64)
        ))
        .unwrap();
        let backend = backend_for_kit_identity(
            home.path(),
            BTreeMap::from([(
                "agent".into(),
                RegisteredKit {
                    workload: workload.clone(),
                },
            )]),
        );
        let spec = backend.kit_spec("agent").unwrap();
        let scope_identity = selected_home_identity(home.path()).unwrap();
        let key = format!("{scope_identity}:{}", workload.identity());
        backend.ready_kits.lock().unwrap().insert(
            key,
            ReadyKitVm {
                name: spec.name.clone(),
                kit_ref: workload.identity().into(),
                lifecycle_workspace: spec.lifecycle_workspace.clone(),
                cold_started: false,
                job_image: workload.workload_image().unwrap().clone(),
                worker_binary: spec.worker_binary.clone(),
            },
        );

        let store = DaemonStore::new(home.path());
        let error = backend
            .prepare_one_with_progress(&spec, &store, || Ok(()))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unexpected stock SBX invocation"),
            "a cache hit without the exact live worker lease must enter full stock-SBX validation: {error}"
        );
    }

    #[test]
    fn live_quarantined_worker_cannot_enter_stock_sbx_repair() {
        let home = tempfile::tempdir().unwrap();
        let workload = NativeKitRef::immutable_oci(format!(
            "registry.example/agent@sha256:{}",
            "a".repeat(64)
        ))
        .unwrap();
        let backend = backend_for_kit_identity(
            home.path(),
            BTreeMap::from([(
                "agent".into(),
                RegisteredKit {
                    workload: workload.clone(),
                },
            )]),
        );
        let store = DaemonStore::new(home.path());
        let status = store.status(None);
        let vm = backend.kit_spec("agent").unwrap().name;
        store
            .register_worker(WorkerStatus {
                worker_id: vm.clone(),
                vm_id: vm.clone(),
                scope_id: status.scope_id,
                kit_ref: workload.identity().into(),
                kits: Vec::new(),
                warm: true,
                health: WorkerHealth::Quarantined,
                container_capacity: 16,
                active_container_ids: Vec::new(),
            })
            .unwrap();

        let spec = backend.kit_spec("agent").unwrap();
        let error = backend
            .prepare_one_with_progress(&spec, &store, || Ok(()))
            .unwrap_err();
        assert!(error.to_string().contains("cannot be repaired"));
    }

    #[test]
    fn same_generation_repair_refuses_an_admitted_worker_before_stock_sbx_effects() {
        let home = tempfile::tempdir().unwrap();
        let workload = NativeKitRef::immutable_oci(format!(
            "registry.example/agent@sha256:{}",
            "a".repeat(64)
        ))
        .unwrap();
        let backend = backend_for_kit_identity(
            home.path(),
            BTreeMap::from([(
                "agent".into(),
                RegisteredKit {
                    workload: workload.clone(),
                },
            )]),
        );
        let spec = backend.kit_spec("agent").unwrap();
        let store = DaemonStore::new(home.path());
        let session = store.attach_shell(
            1,
            SessionAuthority {
                username: "alice".into(),
                uid: 501,
                gid: 20,
                launch_directory: home.path().into(),
                guest_home: "/Users/alice".into(),
                home_backing: home.path().into(),
                ephemeral_home: false,
            },
        );
        store
            .register_worker(WorkerStatus {
                worker_id: spec.name.clone(),
                vm_id: spec.name.clone(),
                scope_id: store.status(None).scope_id,
                kit_ref: workload.identity().into(),
                kits: Vec::new(),
                warm: true,
                health: WorkerHealth::Ready,
                container_capacity: WORKER_CONTAINER_CAPACITY,
                active_container_ids: Vec::new(),
            })
            .unwrap();
        let (job, _) = store
            .begin_job(NewJob {
                session_id: session,
                command: "agent".into(),
                kit_ref: workload.identity().into(),
                workload_image: workload.workload_image().unwrap().as_str().into(),
                mounts: Vec::new(),
            })
            .unwrap();
        store.reserve_worker(&job, &spec.name, &spec.name).unwrap();

        let error = backend
            .prepare_one_with_progress(&spec, &store, || Ok(()))
            .unwrap_err();
        assert!(error.to_string().contains("cannot be repaired"));
        assert_eq!(store.status(None).workers[0].health, WorkerHealth::Ready);
    }

    #[test]
    fn capacity_rejection_before_effects_has_no_cleanup_uncertainty() {
        let home = tempfile::tempdir().unwrap();
        let store = DaemonStore::new(home.path());
        let session = store.attach_shell(
            1,
            SessionAuthority {
                username: "alice".into(),
                uid: 501,
                gid: 20,
                launch_directory: "/Users/alice/project".into(),
                guest_home: "/Users/alice".into(),
                home_backing: home.path().into(),
                ephemeral_home: false,
            },
        );
        let scope = store.status(None).scope_id;
        store
            .register_worker(WorkerStatus {
                worker_id: "worker".into(),
                vm_id: "vm".into(),
                scope_id: scope,
                kit_ref: "fixture".into(),
                kits: Vec::new(),
                warm: true,
                health: WorkerHealth::Ready,
                container_capacity: 1,
                active_container_ids: Vec::new(),
            })
            .unwrap();
        let new_job = || NewJob {
            session_id: session.clone(),
            command: "fixture".into(),
            kit_ref: "fixture".into(),
            workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
            mounts: Vec::new(),
        };
        let (occupying_job, _) = store.begin_job(new_job()).unwrap();
        store
            .reserve_worker(&occupying_job, "worker", "vm")
            .unwrap();
        let (rejected_job, _) = store.begin_job(new_job()).unwrap();
        let mut guard = JobGuard::new(store.clone(), rejected_job.clone());

        let error = store
            .reserve_worker(&rejected_job, "worker", "vm")
            .unwrap_err();
        guard
            .reject_before_effects(format!("admission_failed: {error}"))
            .unwrap();
        drop(guard);

        let receipt = store.job(&rejected_job).unwrap();
        assert_eq!(receipt.state, marsh_daemon::JobState::Finished);
        assert_eq!(receipt.cleanup, CleanupState::NotRequired);
        assert!(receipt.output_complete);
        assert_eq!(receipt.worker_id, None);
        assert_eq!(receipt.vm_id, None);
        assert_eq!(receipt.container_id, None);
        let exit = receipt.exit.unwrap();
        assert_eq!(exit.code, Some(125));
        assert!(exit.cause.contains("at capacity"));
        assert!(!exit.cause.contains("cleanup_uncertain"));
        let worker = store
            .status(None)
            .workers
            .into_iter()
            .find(|worker| worker.worker_id == "worker")
            .unwrap();
        assert_eq!(worker.health, WorkerHealth::Ready);

        store.cancel_job(&occupying_job, "test_complete").unwrap();
        let (next_job, _) = store.begin_job(new_job()).unwrap();
        store.reserve_worker(&next_job, "worker", "vm").unwrap();
    }

    #[test]
    fn output_read_failure_marks_delivery_incomplete_without_aborting_cleanup() {
        struct FailedReader;
        impl Read for FailedReader {
            fn read(&mut self, _bytes: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "capture failed"))
            }
        }
        let (server, _client) = UnixStream::pair().unwrap();
        let attachment = ServerAttachment::new(server).unwrap();
        let result = join_stream(stream_output(Box::new(FailedReader), attachment, false)).unwrap();
        assert!(!result.delivered);
        assert_eq!(result.bytes, 0);
    }

    #[test]
    fn cleanup_diagnostic_respects_remaining_output_budget() {
        assert_eq!(
            cleanup_diagnostic("container cleanup could not be verified", 100, 0),
            b"marsh: container cleanup could not be verified; worker quarantined\n"
        );
        assert_eq!(cleanup_diagnostic("cleanup failed", 10, 6), b"mars");
        assert!(cleanup_diagnostic("cleanup failed", 10, 10).is_empty());
        assert!(cleanup_diagnostic("cleanup failed", 10, 11).is_empty());
    }

    #[test]
    fn typed_resource_limits_use_acceptance_receipt_causes() {
        for (resource, cause) in [
            (ResourceLimit::Memory, "limit:memory"),
            (ResourceLimit::Pids, "limit:pids"),
            (ResourceLimit::Output, "limit:output"),
            (ResourceLimit::Writable, "limit:writable"),
            (ResourceLimit::Wall, "limit:wall"),
        ] {
            assert_eq!(
                report_exit(&ExecutionOutcome::LimitExceeded { resource }),
                (None, cause.into())
            );
        }
        for observation in [ExecutionOutcome::Unknown, ExecutionOutcome::NotStarted] {
            assert_eq!(
                report_exit(&observation),
                (None, "supervision_failed".into())
            );
        }
    }

    #[test]
    fn uncertain_cleanup_uses_same_failure_for_client_and_qualified_receipt() {
        let home = tempfile::tempdir().unwrap();
        let store = DaemonStore::new(home.path());
        let session = store.attach_shell(
            1,
            SessionAuthority {
                username: "alice".into(),
                uid: 501,
                gid: 20,
                launch_directory: "/Users/alice/project".into(),
                guest_home: "/Users/alice".into(),
                home_backing: home.path().into(),
                ephemeral_home: false,
            },
        );
        let (job, _) = store
            .begin_job(NewJob {
                session_id: session,
                command: "fixture".into(),
                kit_ref: "fixture".into(),
                workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
                mounts: Vec::new(),
            })
            .unwrap();
        let (code, cause) = public_exit(&ExecutionOutcome::Exited { code: 0 }, true, true);
        let client_frame = AttachmentFrame::Exited {
            code: code.unwrap_or(125),
        };
        store
            .finish_job(
                &job,
                ExitStatus { code, cause },
                true,
                CleanupState::Uncertain,
                TimingReport::default(),
            )
            .unwrap();

        assert_eq!(client_frame, AttachmentFrame::Exited { code: 125 });
        let receipt = store.job(&job).unwrap();
        assert_eq!(
            receipt.exit.unwrap(),
            ExitStatus {
                code: Some(125),
                cause: "exited; cleanup_uncertain".into(),
            }
        );
    }

    #[test]
    fn ambiguous_worker_start_is_terminal_for_client_and_quarantines_receipt_worker() {
        let home = tempfile::tempdir().unwrap();
        let store = DaemonStore::new(home.path());
        let session = store.attach_shell(
            1,
            SessionAuthority {
                username: "alice".into(),
                uid: 501,
                gid: 20,
                launch_directory: "/Users/alice/project".into(),
                guest_home: "/Users/alice".into(),
                home_backing: home.path().into(),
                ephemeral_home: false,
            },
        );
        let scope = store.status(None).scope_id;
        store
            .register_worker(WorkerStatus {
                worker_id: "worker".into(),
                vm_id: "vm".into(),
                scope_id: scope,
                kit_ref: "fixture".into(),
                kits: Vec::new(),
                warm: true,
                health: WorkerHealth::Ready,
                container_capacity: 8,
                active_container_ids: Vec::new(),
            })
            .unwrap();
        let (job, _) = store
            .begin_job(NewJob {
                session_id: session,
                command: "fixture".into(),
                kit_ref: "fixture".into(),
                workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
                mounts: Vec::new(),
            })
            .unwrap();
        store.reserve_worker(&job, "worker", "vm").unwrap();

        let code = finish_ambiguous_worker_start(&store, &job, true).unwrap();

        assert_eq!(
            AttachmentFrame::Exited { code },
            AttachmentFrame::Exited { code: 125 }
        );
        let receipt = store.job(&job).unwrap();
        assert_eq!(receipt.cleanup, CleanupState::Uncertain);
        assert_eq!(
            receipt.exit.unwrap().cause,
            "worker_start_ambiguous; cleanup_uncertain"
        );
        let worker = &store.status(None).workers[0];
        assert_eq!(worker.health, WorkerHealth::Quarantined);
        assert!(!worker.warm);
    }

    #[test]
    fn incomplete_output_is_a_typed_nonzero_delivery_failure() {
        assert_eq!(
            public_exit(&ExecutionOutcome::Exited { code: 7 }, false, false),
            (Some(125), "exited; output_delivery_failed".into())
        );
        assert_eq!(
            public_exit(&ExecutionOutcome::Exited { code: 0 }, false, false),
            (Some(125), "exited; output_delivery_failed".into())
        );
    }

    #[test]
    fn project_shell_delivery_loss_cannot_report_status_zero() {
        assert_eq!(shell_delivery_exit(0, false), 125);
        assert_eq!(shell_delivery_exit(7, false), 125);
        assert_eq!(shell_delivery_exit(0, true), 0);
    }

    #[test]
    fn project_shell_exit_is_not_observable_before_session_grants_are_released() {
        let ephemeral_home = tempfile::tempdir().unwrap();
        let ephemeral_path = ephemeral_home.path().to_owned();
        let cleanup_path = ephemeral_path.clone();
        let (server, mut client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(25)))
            .unwrap();
        let attachment = ServerAttachment::new(server).unwrap();
        let (cleanup_entered_send, cleanup_entered_receive) = mpsc::channel();
        let (allow_cleanup_send, allow_cleanup_receive) = mpsc::channel();

        let completion = thread::spawn(move || {
            finish_shell_session(&attachment, Ok(0), || {
                assert!(
                    cleanup_path.is_dir(),
                    "the selected ephemeral home must outlive mount revocation"
                );
                cleanup_entered_send.send(()).unwrap();
                allow_cleanup_receive.recv().unwrap();
                Ok(())
            })
        });

        cleanup_entered_receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        let blocked = marsh_daemon::read_frame::<AttachmentFrame>(&mut client).unwrap_err();
        assert!(
            matches!(blocked, DaemonError::Io(ref error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)),
            "unexpected read result while session cleanup was blocked: {blocked}"
        );

        client.set_read_timeout(None).unwrap();
        allow_cleanup_send.send(()).unwrap();
        assert_eq!(
            marsh_daemon::read_frame::<AttachmentFrame>(&mut client).unwrap(),
            AttachmentFrame::Exited { code: 0 }
        );
        completion.join().unwrap().unwrap();

        drop(ephemeral_home);
        assert!(!ephemeral_path.exists());
    }

    #[test]
    fn attachment_disconnect_does_not_abort_worker_output_drain() {
        let (server, client) = UnixStream::pair().unwrap();
        let attachment = ServerAttachment::new(server).unwrap();
        drop(client);

        let copied = join_stream(stream_output(
            Box::new(Cursor::new(b"output after disconnect".to_vec())),
            attachment,
            false,
        ))
        .unwrap();

        assert!(!copied.delivered);
        assert_eq!(copied.bytes, 23);
        assert!(copied.first_output.is_some());
    }

    #[test]
    fn disconnected_client_wakes_backpressured_output_drain_and_cleanup() {
        let (server, stalled_client) = UnixStream::pair().unwrap();
        let disconnect = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            drop(stalled_client);
        });
        let attachment = ServerAttachment::new(server).unwrap();
        let output = vec![b'x'; 8 * 1024 * 1024];
        let started = Instant::now();
        let copied = join_stream(stream_output(
            Box::new(Cursor::new(output.clone())),
            attachment,
            false,
        ))
        .unwrap();

        assert!(!copied.delivered);
        assert_eq!(copied.bytes, output.len() as u64);
        assert!(started.elapsed() < Duration::from_secs(1));
        disconnect.join().unwrap();
    }

    #[test]
    fn stdin_eof_does_not_disable_later_signal_control() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let attachment = ServerAttachment::new(server).unwrap();
        let (send, receive) = mpsc::channel();
        forward_raw_input(
            attachment,
            Box::new(io::sink()),
            Arc::new(SignalRecorder(send)),
        );
        marsh_daemon::write_frame(&mut client, &AttachmentFrame::StdinEof).unwrap();
        marsh_daemon::write_frame(
            &mut client,
            &AttachmentFrame::Signal {
                signal: "interrupt".into(),
            },
        )
        .unwrap();
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(1)).unwrap(),
            JobSignal::Interrupt
        );
    }

    #[test]
    fn blocked_shell_stdin_does_not_delay_interrupt() {
        struct BlockingWriter {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            bytes: Arc<Mutex<Vec<u8>>>,
            finished: mpsc::Sender<()>,
        }

        impl Write for BlockingWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let _ = self.entered.send(());
                let _ = self.release.recv();
                self.bytes.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        impl Drop for BlockingWriter {
            fn drop(&mut self) {
                let _ = self.finished.send(());
            }
        }

        let (server, mut client) = UnixStream::pair().unwrap();
        let attachment = ServerAttachment::new(server).unwrap();
        let (entered_send, entered_receive) = mpsc::channel();
        let (release_send, release_receive) = mpsc::channel();
        let (finished_send, finished_receive) = mpsc::channel();
        let (signal_send, signal_receive) = mpsc::channel();
        let written = Arc::new(Mutex::new(Vec::new()));
        forward_raw_input(
            attachment,
            Box::new(BlockingWriter {
                entered: entered_send,
                release: release_receive,
                bytes: Arc::clone(&written),
                finished: finished_send,
            }),
            Arc::new(SignalRecorder(signal_send)),
        );
        marsh_daemon::write_frame(
            &mut client,
            &AttachmentFrame::Stdin {
                bytes: b"blocked input".to_vec(),
            },
        )
        .unwrap();
        entered_receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        marsh_daemon::write_frame(
            &mut client,
            &AttachmentFrame::Signal {
                signal: "interrupt".into(),
            },
        )
        .unwrap();
        assert_eq!(
            signal_receive.recv_timeout(Duration::from_secs(1)).unwrap(),
            JobSignal::Interrupt
        );
        marsh_daemon::write_frame(
            &mut client,
            &AttachmentFrame::Stdin {
                bytes: b" after interrupt".to_vec(),
            },
        )
        .unwrap();
        marsh_daemon::write_frame(&mut client, &AttachmentFrame::StdinEof).unwrap();
        release_send.send(()).unwrap();
        entered_receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        release_send.send(()).unwrap();
        finished_receive
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(*written.lock().unwrap(), b"blocked input after interrupt");
    }

    #[test]
    fn phase_timing_is_complete_monotonic_and_reconciled() {
        let started = Instant::now();
        let step = Duration::from_millis(1);
        let report = timing_report(&PhaseBoundaries {
            started,
            started_unix_ms: 1000,
            vm_ready_at: started + step,
            admitted_at: started + step * 2,
            mount_ready_at: started + step * 3,
            worker_progress_at: started + step * 4,
            output_relay_at: started + step * 4,
            first_output_at: Some(started + step * 5),
            process_exit_at: started + step * 7,
            output_drained_at: started + step * 8,
            capture_at: started + step * 9,
            cleanup_at: started + step * 10,
        })
        .unwrap();
        assert_eq!(report.durations_ms.len(), 8);
        assert!(!report.durations_ms.contains_key("queue"));
        assert!(!report.durations_ms.contains_key("placement"));
        assert_eq!(
            report.wall_ms,
            report.durations_ms.values().copied().sum::<u64>()
        );
        assert_eq!(
            report.orchestration_ms,
            report.wall_ms - report.durations_ms["execution"]
        );
        assert!(
            report.milestones_unix_ms["worker_progress"]
                < report.milestones_unix_ms["process_exit"]
        );
        assert!(
            report.milestones_unix_ms["process_exit"] < report.milestones_unix_ms["output_drained"]
        );
    }

    #[test]
    fn reordered_timing_boundaries_are_rejected_instead_of_clamped() {
        let started = Instant::now();
        let step = Duration::from_millis(1);
        let error = timing_report(&PhaseBoundaries {
            started,
            started_unix_ms: 1000,
            vm_ready_at: started + step,
            admitted_at: started + step * 2,
            mount_ready_at: started + step * 3,
            worker_progress_at: started + step * 5,
            output_relay_at: started + step * 5,
            first_output_at: None,
            process_exit_at: started + step * 4,
            output_drained_at: started + step * 6,
            capture_at: started + step * 7,
            cleanup_at: started + step * 8,
        })
        .unwrap_err();
        assert!(error.to_string().contains("reordered during execution"));
    }

    #[test]
    fn timing_bookkeeping_failure_leaves_clean_worker_reusable() {
        let home = tempfile::tempdir().unwrap();
        let store = DaemonStore::new(home.path());
        let session = store.attach_shell(
            1,
            SessionAuthority {
                username: "alice".into(),
                uid: 501,
                gid: 20,
                launch_directory: "/Users/alice/project".into(),
                guest_home: "/Users/alice".into(),
                home_backing: home.path().into(),
                ephemeral_home: false,
            },
        );
        let scope = store.status(None).scope_id;
        store
            .register_worker(WorkerStatus {
                worker_id: "worker".into(),
                vm_id: "vm".into(),
                scope_id: scope,
                kit_ref: "fixture".into(),
                kits: Vec::new(),
                warm: true,
                health: WorkerHealth::Ready,
                container_capacity: 1,
                active_container_ids: Vec::new(),
            })
            .unwrap();
        let (job, _) = store
            .begin_job(NewJob {
                session_id: session,
                command: "fixture".into(),
                kit_ref: "fixture".into(),
                workload_image: format!("fixture@sha256:{}", "a".repeat(64)),
                mounts: Vec::new(),
            })
            .unwrap();
        store.reserve_worker(&job, "worker", "vm").unwrap();

        let started = Instant::now();
        let step = Duration::from_millis(1);
        let error = finish_job_with_timing(
            &store,
            &job,
            ExecutionOutcome::Exited { code: 0 },
            ExitStatus {
                code: Some(0),
                cause: "exited".into(),
            },
            true,
            CleanupState::Verified,
            &PhaseBoundaries {
                started,
                started_unix_ms: 1000,
                vm_ready_at: started + step,
                admitted_at: started + step * 2,
                mount_ready_at: started + step * 3,
                worker_progress_at: started + step * 5,
                output_relay_at: started + step * 5,
                first_output_at: None,
                process_exit_at: started + step * 4,
                output_drained_at: started + step * 6,
                capture_at: started + step * 7,
                cleanup_at: started + step * 8,
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("reordered during execution"));
        let receipt = store.job(&job).unwrap();
        assert_eq!(receipt.cleanup, CleanupState::Verified);
        assert_eq!(receipt.state, marsh_daemon::JobState::Failed);
        assert_eq!(receipt.exit.unwrap().code, Some(125));
        assert!(!store.worker_reuse_blocked("vm"));
    }

    #[test]
    fn scope_teardown_reports_partial_cleanup_without_claiming_success() {
        let home = tempfile::tempdir().unwrap();
        let backend = backend_for_kit_identity(home.path(), BTreeMap::new());
        let store = DaemonStore::new(home.path());

        let report = backend.teardown_scope(
            ScopeLifecycleAction::Stop,
            store,
            Instant::now() + Duration::from_secs(5),
        );

        assert!(!report.cleanup_complete);
        assert_eq!(report.action, ScopeLifecycleAction::Stop);
        assert_eq!(report.components.len(), 2);
        assert!(report.components.iter().any(|component| {
            component.kind == "shell" && component.state == ScopeCleanupState::CleanupUncertain
        }));
        assert!(report.components.iter().any(|component| {
            component.label == "stale-generations"
                && component.state == ScopeCleanupState::CleanupUncertain
        }));
    }
}
