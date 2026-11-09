//! `marsh services`: a project's services VM, its lease holders and its idle
//! timer (`docs/design/services.md`). Host only: nothing here is reachable
//! from a shell relay or a job capability.
//!
//! Readiness (`docs/design/services.md`, "Readiness") is computed from what
//! stock SBX and the adapter report now, never remembered: a project is
//! `ready` only while its VM runs, its keep-alive lease is live and a
//! generation was recorded for it. Nothing may use the VM otherwise.
//!
//! Holders keep the VM running. When the last one releases, an idle timer
//! starts; when it expires the lease is dropped and the VM is stopped. Idle
//! never removes a VM: only `marsh services rm` (and `marsh stop` / `reset`,
//! which remove every VM of the scope) does.

use super::{StockDaemonBackend, services_snapshots, services_snapshots::StopRefused};
use marsh_daemon::{
    DaemonError, ScopeCleanupComponent, ScopeCleanupState, ScopeLifecycleAction, ServicesRequest,
};
use marsh_sbx::services_relay::{Relay, RelayError, ServicesRelays, breaks as relay_breaks};
use marsh_sbx::{
    Gate, Governance, InternetGrants, ServicesGeneration, ServicesVmSpec, ServicesVmView,
    services_breaks,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, atomic::AtomicU64, atomic::Ordering},
    time::{Duration, Instant},
};

const SCHEMA: &str = "marsh.services/v1";
const DEFAULT_DISK_SIZE: &str = "40g";
const DEFAULT_IDLE_SECONDS: u64 = 900;
const MAX_IDLE_SECONDS: u64 = 7 * 24 * 60 * 60;
/// The longest a `hold` may last, so a forgotten one cannot pin a VM for ever.
const MAX_HOLD_SECONDS: u64 = 24 * 60 * 60;
const OPERATION_TIMEOUT: Duration = Duration::from_mins(60);
/// Test break (C4): `services-v1` is never listed, though the build can run
/// services; and the TEST-PLAN name of the same defect.
pub(crate) const SERVICES_FEATURE_OFF: &str = "services-feature-off";
pub(crate) const FEATURE_HIDDEN: &str = "feature-hidden";

/// `services.*` as read from `config.json` and the daemon's start environment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Settings {
    /// Canonical roots of the projects the user turned Docker on for
    /// (SECURITY-DESIGN A11: per project, never per home).
    pub projects: BTreeSet<String>,
    /// Canonical roots of the projects the user turned Docker off for
    /// (`docker_projects_off`); it wins over `projects`.
    pub off_projects: BTreeSet<String>,
    /// Docker is on for a project with no setting of its own (DECISIONS 31;
    /// `docker_default_on`, default true).
    pub default_on: bool,
    pub disk_size: String,
    pub idle_seconds: u64,
}

impl Settings {
    /// Whether Docker is on for the project at `root`: the project's own
    /// setting (off wins), else the home's default.
    pub fn enabled_for(&self, root: &Path) -> bool {
        // Test break: any enabled project enables every project of the home.
        if marsh_contracts::break_mode(services_breaks::SERVICES_ENABLE_PER_HOME) {
            return !self.projects.is_empty();
        }
        let Some(root) = root.to_str() else {
            return false;
        };
        if self.off_projects.contains(root) {
            return false;
        }
        self.default_on || self.projects.contains(root)
    }
}

/// Settings from `$MARSH_HOME/config.json` (`services_projects`,
/// `services_disk_size`, `services_idle_seconds`); `idle_override` is
/// `MARSH_SERVICES_IDLE_SECONDS` at daemon start, which wins.
pub(crate) fn settings(scope_root: &Path, idle_override: Option<u64>) -> Result<Settings, String> {
    let path = scope_root.join("config.json");
    let document = match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
            .map_err(|error| format!("invalid {}: {error}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let list = |key: &str| -> Result<BTreeSet<String>, String> {
        match document.get(key) {
            None | Some(Value::Null) => Ok(BTreeSet::new()),
            Some(Value::Array(roots)) => roots
                .iter()
                .map(|root| {
                    root.as_str()
                        .filter(|root| Path::new(root).is_absolute())
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            format!(
                                "invalid {}: {key} must list absolute project paths",
                                path.display()
                            )
                        })
                })
                .collect(),
            Some(_) => Err(format!(
                "invalid {}: {key} must be a list of project paths",
                path.display()
            )),
        }
    };
    let projects = list("services_projects")?;
    let off_projects = list("docker_projects_off")?;
    let default_on = match document.get("docker_default_on") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(on)) => *on,
        Some(_) => {
            return Err(format!(
                "invalid {}: docker_default_on must be true or false",
                path.display()
            ));
        }
    };
    let disk_size = match document.get("services_disk_size") {
        None | Some(Value::Null) => DEFAULT_DISK_SIZE.to_owned(),
        Some(Value::String(size)) if valid_size(size) => size.clone(),
        Some(_) => {
            return Err(format!(
                "invalid {}: services_disk_size must be a size such as 40g",
                path.display()
            ));
        }
    };
    let idle_seconds = match (idle_override, document.get("services_idle_seconds")) {
        (Some(seconds), _) => seconds,
        (None, None | Some(Value::Null)) => DEFAULT_IDLE_SECONDS,
        (None, Some(Value::Number(seconds))) => seconds
            .as_u64()
            .filter(|seconds| (1..=MAX_IDLE_SECONDS).contains(seconds))
            .ok_or_else(|| {
                format!(
                    "invalid {}: services_idle_seconds must be 1 to {MAX_IDLE_SECONDS}",
                    path.display()
                )
            })?,
        (None, Some(_)) => {
            return Err(format!(
                "invalid {}: services_idle_seconds must be a number",
                path.display()
            ));
        }
    };
    Ok(Settings {
        projects,
        off_projects,
        default_on,
        disk_size,
        idle_seconds,
    })
}

fn valid_size(value: &str) -> bool {
    value
        .strip_suffix(['g', 'G'])
        .and_then(|digits| digits.parse::<u32>().ok())
        .is_some_and(|gib| (1..=4096).contains(&gib))
}

/// What the daemon tracks per project, in memory only: a restart loses the
/// holders (their leases died with it) and the pass re-arms an idle timer for
/// every services VM it finds running.
#[derive(Default)]
pub(super) struct Project {
    /// Holder id to its expiry (`None`: until released).
    holders: BTreeMap<String, Option<Instant>>,
    idle_deadline: Option<Instant>,
    pub(super) starting: bool,
    /// Why the last start or stop failed; cleared by the next success.
    failure: Option<String>,
}

/// A project root and the attempt or shell session that made a forward.
type ForwardKey = (PathBuf, String);
/// The forwards one owner made: port to (`job` or `shell`, what happened).
type Forwards = BTreeMap<u16, (String, marsh_worker::PortState)>;
/// When a project's containers and ports were last read, and what they were.
type ViewCache = (Instant, Vec<Value>, Vec<Value>);

#[derive(Default)]
pub(crate) struct Services {
    /// `MARSH_SERVICES_IDLE_SECONDS` at daemon start.
    pub idle_override: Option<u64>,
    projects: Mutex<BTreeMap<PathBuf, Project>>,
    locks: Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>,
    next_holder: AtomicU64,
    /// The projects' Internet grants (`services-internet.json` in the scope).
    grants: OnceLock<InternetGrants>,
    /// The relays to ready services VMs (E1). `None` until the daemon gives
    /// the backend its guest artifacts ([`StockDaemonBackend::with_services_artifacts`]).
    pub relays: Option<Arc<ServicesRelays>>,
    /// The first timer pass runs the recovery pass (VM-06).
    recovered: std::sync::atomic::AtomicBool,
    /// The port forwards of each job and shell (E2), by project root and the
    /// attempt (or shell session) that made them: display for `status`.
    forwards: Mutex<BTreeMap<ForwardKey, Forwards>>,
    /// The last containers-and-ports read of each project, for a second.
    view_cache: Mutex<BTreeMap<PathBuf, ViewCache>>,
    /// Snapshots for checkpoints (C12).
    pub snapshots: super::services_snapshots::Snapshots,
    /// The attempts whose services containers are still to be reaped (E4).
    pub(crate) reaps: OnceLock<Arc<super::services_reap::ReapRecords>>,
    /// The open lanes the daemon told us about (C3), by workspace.
    pub(super) lanes: Mutex<BTreeMap<String, super::services_lanes::LaneEntry>>,
}

impl Services {
    fn lock_for(&self, root: &Path) -> Arc<Mutex<()>> {
        Arc::clone(
            self.locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(root.to_owned())
                .or_default(),
        )
    }

    pub(super) fn with_project<R>(&self, root: &Path, update: impl FnOnce(&mut Project) -> R) -> R {
        update(
            self.projects
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(root.to_owned())
                .or_default(),
        )
    }
}

fn generation_json(generation: &ServicesGeneration) -> Value {
    json!({"boot": generation.boot_id, "counter": generation.counter})
}

impl StockDaemonBackend {
    /// The typed channel API for E2 (the job endpoint) and C1 (capture,
    /// restore, drop): the project's live relay, started on first use. Only
    /// for a project the user enabled services for (A11). It refuses, with the
    /// reason, while the project is unrecovered after a daemon restart.
    ///
    /// # Errors
    /// `NotAuthorized` when services are off for the project, `Unavailable`
    /// when no relay can be had.
    pub fn services_relay(&self, root: &Path) -> Result<Relay, RelayError> {
        let relays = self.services.relays.as_ref().ok_or_else(|| {
            RelayError::Unavailable("this daemon has no services engine artifacts".into())
        })?;
        let settings = self
            .services_settings()
            .map_err(|error| RelayError::Unavailable(error.to_string()))?;
        if !settings.enabled_for(root) {
            return Err(RelayError::NotAuthorized(format!(
                "Docker is off for {}",
                root.display()
            )));
        }
        if let Ok(entry) = relays.get(root) {
            return Ok(entry.relay().clone());
        }
        if relays.is_unrecovered(root) {
            return Err(RelayError::Unavailable(
                "the relay has not been recovered since the daemon started".into(),
            ));
        }
        if let Some(reason) = relays.quarantine(root) {
            eprintln!(
                "marshd: services relay of {} is quarantined ({reason}); {}",
                root.display(),
                marsh_sbx::services_relay::QUARANTINE_RECOVERY
            );
            return Err(RelayError::Unavailable(format!(
                "quarantined: {reason}; {}",
                marsh_sbx::services_relay::QUARANTINE_RECOVERY
            )));
        }
        // First use: the VM, its engine generation and a relay (D1: created
        // lazily by the first Docker or services request).
        self.services_start(root, &settings, None)
            .map_err(|error| RelayError::Unavailable(error.to_string()))?;
        relays.get(root).map(|entry| entry.relay().clone())
    }

    /// The recovery pass after a daemon (re)start (VM-06): each services VM
    /// the ownership map holds is marked unrecovered at construction; the
    /// first timer pass brings the running, enabled ones back with a fresh
    /// engine generation and a new relay, and clears the mark of the rest.
    /// `startup-recovery-off` skips it, so channels stay refused.
    fn services_recover_once(&self) {
        let Some(relays) = self.services.relays.clone() else {
            return;
        };
        if self.services.recovered.swap(true, Ordering::AcqRel) {
            return;
        }
        if marsh_contracts::break_mode(relay_breaks::STARTUP_RECOVERY_OFF) {
            return;
        }
        let Ok(settings) = self.services_settings() else {
            return;
        };
        let running: BTreeSet<PathBuf> = self
            .sbx
            .stock_inventory(false)
            .map(|inventory| {
                self.sbx
                    .owned_services_vms()
                    .into_iter()
                    .filter(|(name, _)| {
                        inventory.get(name).is_some_and(|vm| vm.status == "running")
                    })
                    .map(|(_, key)| PathBuf::from(key))
                    .collect()
            })
            .unwrap_or_default();
        let mut to_recover = Vec::new();
        for root in relays.unrecovered_roots() {
            if running.contains(&root) && settings.enabled_for(&root) {
                to_recover.push(root);
            } else {
                relays.clear_unrecovered(&root);
            }
        }
        // In parallel, and the timer thread waits: nothing else it does
        // (idle stops) is urgent while a VM is being brought back.
        std::thread::scope(|scope| {
            for root in &to_recover {
                let (settings, relays) = (&settings, &relays);
                scope.spawn(move || {
                    if let Err(error) = self.services_start(root, settings, None) {
                        eprintln!(
                            "marshd: services relay recovery of {}: {error}",
                            root.display()
                        );
                    }
                    relays.clear_unrecovered(root);
                });
            }
        });
        // The records a dead daemon left are reaped once its VMs are back (E4).
        self.services_reap_startup();
    }

    /// The project a job or shell of this launch directory belongs to, when the
    /// user turned services on for it and this daemon can run them (A11).
    /// The endpoint is never offered otherwise: no socket, no variables.
    pub(crate) fn services_job_root(&self, launch_directory: &Path) -> Option<PathBuf> {
        self.services.relays.as_ref()?;
        let root = fs::canonicalize(launch_directory).ok()?;
        self.services_settings()
            .ok()?
            .enabled_for(&root)
            .then_some(root)
    }

    pub(crate) fn services_relays(&self) -> Option<Arc<ServicesRelays>> {
        self.services.relays.clone()
    }

    /// Keeps the project's services VM awake for a job or shell that holds
    /// its endpoint (D1), for at most `seconds`. `None` when nothing was held.
    pub(crate) fn services_hold_job(&self, root: &Path, seconds: u64) -> Option<String> {
        if marsh_contracts::break_mode(services_breaks::LEASE_NONE) {
            return None;
        }
        let seconds = seconds.clamp(1, MAX_HOLD_SECONDS);
        let id = format!(
            "job-{}",
            self.services.next_holder.fetch_add(1, Ordering::Relaxed) + 1
        );
        self.services.with_project(root, |project| {
            project.holders.insert(
                id.clone(),
                Some(Instant::now() + Duration::from_secs(seconds)),
            );
            project.idle_deadline = None;
        });
        Some(id)
    }

    /// Releases a job's or shell's hold; the idle timer starts again when it
    /// was the last.
    pub(crate) fn services_release_job(&self, root: &Path, holder: &str) {
        let idle = self
            .services_settings()
            .map_or(DEFAULT_IDLE_SECONDS, |settings| settings.idle_seconds);
        self.services.with_project(root, |project| {
            if project.holders.remove(holder).is_some() && project.holders.is_empty() {
                project.idle_deadline = Some(Instant::now() + Duration::from_secs(idle));
            }
        });
    }

    /// Notes what became of a forward, for `services status`.
    pub(crate) fn record_forward(
        &self,
        root: &Path,
        owner: &str,
        kind: &str,
        port: u16,
        state: &marsh_worker::PortState,
    ) {
        let mut forwards = self
            .services
            .forwards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = forwards
            .entry((root.to_owned(), owner.to_owned()))
            .or_default();
        if matches!(state, marsh_worker::PortState::Revoked) {
            entry.remove(&port);
        } else {
            entry.insert(port, (kind.to_owned(), state.clone()));
        }
    }

    pub(crate) fn forget_forwards(&self, root: &Path, owner: &str) {
        self.services
            .forwards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&(root.to_owned(), owner.to_owned()));
    }

    /// `services-v1` for `marsh status --json`: only when this build can run
    /// services (the engine artifacts are installed beside the daemon).
    pub(super) fn services_feature(&self) -> Option<&'static str> {
        if marsh_contracts::break_mode(SERVICES_FEATURE_OFF)
            || marsh_contracts::break_mode(FEATURE_HIDDEN)
        {
            return None;
        }
        self.can_run_services().then_some("services-v1")
    }

    /// Whether this build has the engine artifacts it needs.
    fn can_run_services(&self) -> bool {
        self.services
            .relays
            .as_ref()
            .is_some_and(|relays| relays.can_run_services())
    }

    pub(super) fn services_settings(&self) -> Result<Settings, DaemonError> {
        settings(&self.config.daemon_home, self.services.idle_override)
            .map_err(DaemonError::Refused)
    }

    /// `start` and `hold` need services on **for this project**: another
    /// enabled project of the home does not enable it (SECURITY-DESIGN A11).
    fn require_services_enabled(&self, root: &Path) -> Result<Settings, DaemonError> {
        let settings = self.services_settings()?;
        if settings.enabled_for(root) {
            Ok(settings)
        } else {
            Err(DaemonError::Refused(format!(
                "marsh: Docker is off for {}; turn it on for this project with `marsh config docker on` in it (or for every project with `marsh config docker default on`)",
                root.display()
            )))
        }
    }

    /// The project root a request names: absolute, canonical, a directory,
    /// and clear of marsh's own state (the shell's overlap rule).
    fn services_root(&self, root: &Path) -> Result<PathBuf, DaemonError> {
        let canonical = fs::canonicalize(root)
            .map_err(|error| DaemonError::Refused(format!("marsh: {}: {error}", root.display())))?;
        if canonical != root || !canonical.is_dir() {
            return Err(DaemonError::Refused(format!(
                "marsh: {} is not a canonical project directory",
                root.display()
            )));
        }
        marsh_daemon::reject_guest_mount_overlap(&[canonical.as_path()], &self.protected_roots())
            .map_err(DaemonError::Refused)?;
        Ok(canonical)
    }

    fn services_spec(&self, root: &Path, settings: &Settings) -> ServicesVmSpec {
        ServicesVmSpec {
            project: root.to_owned(),
            image: self.config.shell.image.clone(),
            shell_binary: self.config.shell.shell_binary.clone(),
            disk_size: settings.disk_size.clone(),
        }
    }

    /// `marsh services ...`.
    pub(super) fn services_request(&self, request: ServicesRequest) -> Result<Value, DaemonError> {
        match request {
            ServicesRequest::Status { root } => {
                let root = self.services_root(&root)?;
                Ok(self.services_status(&root))
            }
            ServicesRequest::Start { root } => {
                let root = self.services_root(&root)?;
                let settings = self.require_services_enabled(&root)?;
                self.services_start(&root, &settings, None)
            }
            ServicesRequest::Hold { root, seconds } => {
                let root = self.services_root(&root)?;
                let settings = self.require_services_enabled(&root)?;
                if seconds == 0 || seconds > MAX_HOLD_SECONDS {
                    return Err(DaemonError::Refused(format!(
                        "marsh: a hold lasts 1 to {MAX_HOLD_SECONDS} seconds"
                    )));
                }
                self.services_start(&root, &settings, Some(Duration::from_secs(seconds)))
            }
            // Cleanup never needs the feature on: turning services off must
            // not make an existing VM impossible to release, stop or remove.
            ServicesRequest::Release { root, holder } => {
                let settings = self.services_settings()?;
                let root = self.services_root(&root)?;
                self.services_release(&root, &holder, &settings)
            }
            ServicesRequest::Internet { root, on } => {
                let root = self.services_root(&root)?;
                let settings = self.require_services_enabled(&root)?;
                self.services_internet(&root, &settings, on)
            }
            ServicesRequest::Stop { root } => {
                let root = self.services_root(&root)?;
                self.services_stop(&root)
            }
            ServicesRequest::Remove { root } => {
                let root = self.services_root(&root)?;
                self.services_remove(&root)
            }
        }
    }

    /// Ensure the VM is ready (acquiring a holder when `hold` is given) and
    /// report it.
    pub(super) fn services_start(
        &self,
        root: &Path,
        settings: &Settings,
        hold: Option<Duration>,
    ) -> Result<Value, DaemonError> {
        let lock = self.services.lock_for(root);
        let _operation = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let spec = self.services_spec(root, settings);
        // The VM runs again: the outcome of its last stop no longer describes
        // it. Cleared (durably) before it can run, so a restart never trusts it;
        // when that cannot be recorded the VM is not started.
        self.clear_stop_outcome(root).map_err(|why| {
            let message = format!("marsh: {why}");
            self.services.with_project(root, |project| {
                project.failure = Some(message.clone());
            });
            DaemonError::Refused(message)
        })?;
        self.services.with_project(root, |project| {
            project.starting = true;
            project.failure = None;
        });
        let started = Instant::now();
        let ready = self.sbx.ensure_services_vm(&spec);
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let ready = match ready {
            Ok(ready) => ready,
            Err(error) => {
                let message = format!("marsh: {error}");
                self.services.with_project(root, |project| {
                    project.starting = false;
                    project.failure = Some(message.clone());
                });
                return Err(DaemonError::Refused(message));
            }
        };
        self.note_vm_used(&ready.name);
        if let Some(relays) = &self.services.relays
            && let Err(message) = relays.establish(&spec, &ready, self.internet_grants())
        {
            self.services.with_project(root, |project| {
                project.starting = false;
                project.failure = Some(message.clone());
            });
            return Err(DaemonError::Refused(message));
        }
        let holder = self.services.with_project(root, |project| {
            project.starting = false;
            project.failure = None;
            let holder = hold.map(|duration| {
                let id = format!(
                    "hold-{}",
                    self.services.next_holder.fetch_add(1, Ordering::Relaxed) + 1
                );
                // Test break: the holder is not recorded, so nothing keeps
                // the VM awake past its idle timer.
                if !marsh_contracts::break_mode(services_breaks::LEASE_NONE) {
                    project
                        .holders
                        .insert(id.clone(), Some(Instant::now() + duration));
                }
                id
            });
            project.idle_deadline = project
                .holders
                .is_empty()
                .then(|| Instant::now() + Duration::from_secs(settings.idle_seconds));
            holder
        });
        let mut document = self.services_project_json(root, settings, true);
        document["schema"] = json!(SCHEMA);
        document["cold_started"] = json!(ready.cold_started);
        document["docker_disk_bytes"] = json!(ready.docker_disk_bytes);
        document["elapsed_ms"] = json!(elapsed_ms);
        if let Some(holder) = holder {
            document["holder"] = json!(holder);
        }
        Ok(document)
    }

    fn services_release(
        &self,
        root: &Path,
        holder: &str,
        settings: &Settings,
    ) -> Result<Value, DaemonError> {
        let released = self.services.with_project(root, |project| {
            let found = project.holders.remove(holder).is_some();
            if found && project.holders.is_empty() {
                project.idle_deadline =
                    Some(Instant::now() + Duration::from_secs(settings.idle_seconds));
            }
            found
        });
        if !released {
            return Err(DaemonError::Refused(format!(
                "marsh: no such holder {holder:?} for {}",
                root.display()
            )));
        }
        let mut document = self.services_project_json(root, settings, false);
        document["schema"] = json!(SCHEMA);
        document["released"] = json!(holder);
        Ok(document)
    }

    fn services_stop(&self, root: &Path) -> Result<Value, DaemonError> {
        let lock = self.services.lock_for(root);
        let _operation = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Capture exclusion, then the cutover (fence, freeze, final capture of
        // every workspace, outcome journalled), then the stop; the exclusion is
        // held until the VM has stopped. A stop that cannot be made safe is refused.
        let plan = self
            .services_stop_prepare(root, "stop", self.stop_wait(Duration::from_secs(120)), None)
            .map_err(|refused| {
                DaemonError::Refused(format!(
                    "marsh: docker stop refused: {}; the VM keeps running (try again)",
                    refused.message()
                ))
            })?;
        self.close_relay(root, "the Docker VM is being stopped");
        let stopped = match self
            .sbx
            .stop_services_vm(root, Instant::now() + OPERATION_TIMEOUT)
        {
            Ok(stopped) => stopped,
            Err(error) => {
                self.services_stop_abort(root, &plan);
                return Err(DaemonError::Refused(format!("marsh: {error}")));
            }
        };
        self.services.with_project(root, |project| {
            project.holders.clear();
            project.idle_deadline = None;
            project.failure = None;
        });
        let settings = self.services_settings()?;
        let mut document = self.services_project_json(root, &settings, true);
        document["schema"] = json!(SCHEMA);
        document["stopped"] = json!(stopped);
        Ok(document)
    }

    fn internet_grants(&self) -> &InternetGrants {
        self.services
            .grants
            .get_or_init(|| InternetGrants::new(&self.config.daemon_home))
    }

    /// `marsh services internet on|off`: the user's grant of Internet to the
    /// project's services (SECURITY-DESIGN A5). A policy that governs the VM
    /// (anything in `sbx policy` that marsh did not create) wins: the request
    /// is refused, both ways, and nothing changes. The grant is recorded in
    /// the scope, and carried out by the engine's gate when the VM runs; an
    /// engine that starts later reads the grant at `engine-up`.
    #[allow(clippy::too_many_lines)] // One ordered sequence: govern, record, apply, verify.
    fn services_internet(
        &self,
        root: &Path,
        settings: &Settings,
        on: bool,
    ) -> Result<Value, DaemonError> {
        let lock = self.services.lock_for(root);
        let _operation = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let word = if on { "on" } else { "off" };
        let project = root.to_str().ok_or_else(|| {
            DaemonError::Refused(format!("marsh: {} is not valid UTF-8", root.display()))
        })?;
        let vm = self.sbx.recorded_services_vm(root);
        let governance = self.sbx.services_governance(vm.as_deref());
        // Test break: the governance check is skipped.
        if governance.governed
            && !marsh_contracts::break_mode(services_breaks::INTERNET_IGNORE_GOVERNED)
        {
            return Err(DaemonError::Refused(format!(
                "marsh: `marsh docker internet {word}` is refused: {}. A policy you or your organization set wins over this toggle; change that policy with `sbx policy`, not marsh",
                governance.summary
            )));
        }
        // Test breaks: the toggle reports success and changes nothing.
        let noop = marsh_contracts::break_mode(services_breaks::INTERNET_NOOP)
            || (!on && marsh_contracts::break_mode(services_breaks::INTERNET_OFF_NOOP));
        if !noop {
            let running = self
                .sbx
                .services_vm_view(root, true)
                .ok()
                .flatten()
                .filter(|view| view.running && view.lease_live);
            if on {
                // Open the gate first, record after: a failure never claims on.
                if let Some(view) = &running {
                    self.verified_gate(&view.name, true)?;
                }
                self.internet_grants()
                    .set(project, true)
                    .map_err(|error| DaemonError::Refused(format!("marsh: {error}")))?;
            } else {
                // Record off first, close after: a restart is closed whatever
                // happens to the live gate.
                self.internet_grants()
                    .set(project, false)
                    .map_err(|error| DaemonError::Refused(format!("marsh: {error}")))?;
                if let Some(view) = &running {
                    self.verified_gate(&view.name, false)?;
                }
            }
        }
        let mut document = self.services_project_json(root, settings, true);
        document["schema"] = json!(SCHEMA);
        Ok(document)
    }

    /// Set the running engine's gate and require it read back as asked. No
    /// engine yet is fine: `engine-up` takes the recorded grant.
    fn verified_gate(&self, vm: &str, open: bool) -> Result<(), DaemonError> {
        match self.sbx.services_set_gate(vm, open) {
            Ok(Gate::NoEngine) => Ok(()),
            Ok(Gate::Open) if open => Ok(()),
            Ok(Gate::Closed) if !open => Ok(()),
            Ok(other) => Err(DaemonError::Refused(format!(
                "marsh: Docker's engine's Internet gate did not verify as {} ({other:?})",
                if open { "open" } else { "closed" }
            ))),
            Err(error) => Err(DaemonError::Refused(format!(
                "marsh: Docker's engine's Internet gate could not be {}: {error}",
                if open { "opened" } else { "closed" }
            ))),
        }
    }

    /// `internet` in a project's status: `{on, governed, effective}`.
    fn services_internet_json(&self, root: &Path, vm: Option<&ServicesVmView>) -> Value {
        let on = root
            .to_str()
            .is_some_and(|project| self.internet_grants().is_on(project));
        let governance = self
            .sbx
            .services_governance(vm.map(|view| view.name.as_str()));
        let gate = vm
            .filter(|view| view.running && view.lease_live)
            .map(|view| {
                self.sbx
                    .services_gate(&view.name)
                    .unwrap_or_else(|error| Gate::Unverified(format!("{error}")))
            });
        json!({
            "on": on,
            "governed": governance.governed,
            "effective": effective_internet(on, &governance, gate.as_ref()),
        })
    }

    fn services_remove(&self, root: &Path) -> Result<Value, DaemonError> {
        let lock = self.services.lock_for(root);
        let _operation = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.close_relay(root, "the Docker VM is being removed");
        let removed = self
            .sbx
            .remove_services_vm(root, Instant::now() + OPERATION_TIMEOUT)
            .map_err(|error| DaemonError::Refused(format!("marsh: {error}")))?;
        self.services
            .projects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(root);
        // The snapshots went with the VM's pool: nothing to reuse any more.
        self.forget_last_capture(root);
        self.services_reap_forget_project(root);
        // The grant belongs to the data it was given for: a project whose
        // services VM and data are gone starts again with Internet off.
        if let Some(project) = root.to_str()
            && let Err(error) = self.internet_grants().clear(project)
        {
            eprintln!(
                "marshd: services Internet grant of {}: {error}",
                root.display()
            );
        }
        let settings = self.services_settings()?;
        let mut document = self.services_project_json(root, &settings, true);
        document["schema"] = json!(SCHEMA);
        document["removed"] = json!(removed);
        Ok(document)
    }

    /// `marsh services status`: this project and every other one this
    /// daemon knows (a tracked or owned services VM).
    fn services_status(&self, root: &Path) -> Value {
        let settings = self.services_settings().unwrap_or(Settings {
            projects: BTreeSet::new(),
            off_projects: BTreeSet::new(),
            default_on: true,
            disk_size: DEFAULT_DISK_SIZE.into(),
            idle_seconds: self.services.idle_override.unwrap_or(DEFAULT_IDLE_SECONDS),
        });
        let mut roots = vec![root.to_owned()];
        let tracked = self
            .services
            .projects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        roots.extend(tracked);
        roots.extend(
            self.sbx
                .owned_services_vms()
                .into_iter()
                .map(|(_, key)| PathBuf::from(key))
                .filter(|key| key.is_absolute()),
        );
        roots.sort();
        roots.dedup();
        let projects = roots
            .iter()
            .map(|project| self.services_project_json(project, &settings, project == root))
            .collect::<Vec<_>>();
        // What is on is the effective value: the projects the user turned on,
        // and (when Docker is on by default) every project listed here that is
        // not turned off. Others are on too when `default_enabled` is true.
        let enabled: BTreeSet<String> = settings
            .projects
            .iter()
            .filter(|project| !settings.off_projects.contains(*project))
            .cloned()
            .chain(
                roots
                    .iter()
                    .filter(|project| settings.enabled_for(project))
                    .filter_map(|project| project.to_str().map(str::to_owned)),
            )
            .collect();
        json!({
            "schema": SCHEMA,
            "enabled_projects": enabled,
            "default_enabled": settings.default_on,
            "disabled_projects": settings.off_projects,
            "idle_seconds": settings.idle_seconds,
            "disk_size": settings.disk_size,
            "projects": projects,
        })
    }

    fn relay_json(&self, root: &Path) -> Value {
        let Some(relays) = &self.services.relays else {
            return json!({"state": "none", "reason": "no services engine artifacts"});
        };
        let view = relays.view(root);
        json!({
            "state": view.state,
            "reason": view.reason,
            "generation": view.generation.as_ref().map(|generation| json!({
                "boot_id": generation.boot_id,
                "engine_id": generation.engine_id,
            })),
            "channels": view.channels,
            "agent_pid": view.agent_pid,
            "recovery": (view.state == "quarantined")
                .then_some(marsh_sbx::services_relay::QUARANTINE_RECOVERY),
        })
    }

    /// One project's entry. The state is derived from stock and the
    /// adapter now (`refresh` lists stock afresh).
    /// A1: losing the exec, the engine or the mount revokes readiness. A VM
    /// that is ready as far as S1 can tell, but whose relay is not (lost,
    /// quarantined, not recovered, not started), is unavailable, with the
    /// relay's own reason.
    fn fold_relay_state(
        &self,
        root: &Path,
        state: &'static str,
        reason: Option<String>,
    ) -> (&'static str, Option<String>) {
        let relay_view = self
            .services
            .relays
            .as_ref()
            .map(|relays| relays.view(root));
        match (&relay_view, state) {
            (Some(relay), "ready") if relay.state != "ready" => (
                "unavailable",
                Some(match (&relay.reason, relay.state) {
                    (Some(why), "quarantined") => format!(
                        "marsh: the Docker relay is quarantined: {why}; {}",
                        marsh_sbx::services_relay::QUARANTINE_RECOVERY
                    ),
                    (Some(why), other) => format!("marsh: the Docker relay is {other}: {why}"),
                    (None, other) => format!("marsh: the Docker relay is {other}"),
                }),
            ),
            _ => (state, reason),
        }
    }

    #[allow(clippy::too_many_lines)] // One entry of `services status`, field by field.
    fn services_project_json(&self, root: &Path, settings: &Settings, refresh: bool) -> Value {
        let (holders, idle_left, starting, failure) = self.services.with_project(root, |project| {
            (
                project.holders.keys().cloned().collect::<Vec<_>>(),
                project
                    .idle_deadline
                    .map(|deadline| deadline.saturating_duration_since(Instant::now())),
                project.starting,
                project.failure.clone(),
            )
        });
        let view = self.sbx.services_vm_view(root, refresh);
        let failed = failure.is_some();
        let (state, reason, vm) = match &view {
            _ if starting => ("starting", None, None),
            Err(error) => ("unavailable", Some(format!("marsh: {error}")), None),
            Ok(None) => match failure {
                Some(reason) => ("unavailable", Some(reason), None),
                None => ("off", None, None),
            },
            Ok(Some(view)) => classify(view, failure),
        };
        // What `summary` says when unavailable, by cause (never the reason's text).
        let mut cause = match (&view, state) {
            (_, s) if s != "unavailable" => None,
            (Err(_), _) => Some(Unavailable::NotResponding),
            _ if failed => Some(Unavailable::CouldNotStart),
            _ => Some(Unavailable::NeedsRestart),
        };
        // Readiness also needs exactly the project mounted (security review
        // L3) and every global service secret shadowed (A12): a global secret
        // added since the last ensure is shadowed here, before `ready` is
        // reported, and a ready VM that cannot be reconciled is unavailable.
        let (state, reason) = match (state, vm.as_ref()) {
            ("ready", Some(view)) => match self
                .sbx
                .reconcile_services_secrets(&view.name)
                .and_then(|()| self.sbx.services_mounts_problem(&view.name, root))
            {
                Ok(None) => (state, reason),
                Ok(Some(problem)) => {
                    cause = Some(Unavailable::SetupChanged);
                    ("unavailable", Some(format!("marsh: {problem}")))
                }
                Err(error) => {
                    cause = Some(Unavailable::SetupChanged);
                    ("unavailable", Some(format!("marsh: {error}")))
                }
            },
            _ => (state, reason),
        };
        let (state, reason) = self.fold_relay_state(root, state, reason);
        if state == "unavailable" && cause.is_none() {
            cause = Some(self.relay_cause(root));
        }
        // SECURITY-DESIGN A12: which global service secrets a sandbox-scoped
        // dummy shadows for this VM, which could not be shadowed, and whether
        // stock attaches its MCP gateway. The engine firewall (S2b) is the
        // primary control; this is the report.
        let exposure =
            vm.as_ref().map(
                |view| match self.sbx.services_credential_exposure(&view.name) {
                    Ok(exposure) => json!({
                        "shadowed": exposure.shadowed,
                        "unshadowable": exposure.unshadowable,
                        "mcp_gateway": exposure.mcp_gateway,
                    }),
                    Err(error) => json!({ "error": format!("marsh: {error}") }),
                },
            );
        // Display only (E2, N3's shape): the base workspace's containers and
        // published ports, read through the same filter a job's requests use.
        let (containers, ports, view_error) = if state == "ready" {
            self.services_containers(root)
        } else {
            (Vec::new(), Vec::new(), None)
        };
        let mut document = json!({
            "root": root,
            "enabled": settings.enabled_for(root),
            "state": state,
            "reason": reason,
            "vm": vm.as_ref().map(|view: &ServicesVmView| view.name.clone()),
            "vm_uuid": vm.as_ref().map(|view: &ServicesVmView| view.uuid.clone()),
            "generation": vm
                .as_ref()
                .and_then(|view| view.generation.as_ref())
                .map(generation_json),
            "holders": holders,
            "idle_seconds_remaining": idle_left.map(|left| left.as_secs()),
            "containers": containers,
            "ports": ports,
            "internet": self.services_internet_json(root, vm.as_ref()),
            "credential_exposure": exposure,
            "relay": self.relay_json(root),
            "last_capture": self.last_capture_json(root),
        });
        document["summary"] = json!(summary(&SummaryInput {
            state,
            enabled: settings.enabled_for(root),
            can_run: self.can_run_services(),
            running: (state == "ready" && view_error.is_none())
                .then(|| running_count(&document["containers"])),
            cause,
        }));
        if state == "off" {
            document["vm"] = Value::Null;
        }
        if let Some(reason) = view_error {
            document["containers_error"] = json!(reason);
        }
        // What the job and shell forwards did (collisions, failures), by owner.
        let forwards = self.forwards_json(root);
        if !forwards.is_empty() {
            document["forwards"] = json!(forwards);
        }
        // The agent's attested break modes (HARNESS-02 c6), only when there are any.
        let relay_view = self
            .services
            .relays
            .as_ref()
            .map(|relays| relays.view(root));
        if let Some(relay) = relay_view.filter(|relay| !relay.agent_breaks.is_empty()) {
            document["test_breaks_agent"] = json!(relay.agent_breaks);
        }
        document
    }

    /// Why a ready VM is unavailable: its relay's state.
    fn relay_cause(&self, root: &Path) -> Unavailable {
        let quarantined = self
            .services
            .relays
            .as_ref()
            .is_some_and(|relays| relays.view(root).state == "quarantined");
        if quarantined {
            Unavailable::Quarantined
        } else {
            Unavailable::LostContact
        }
    }

    /// The base workspace's containers and ports from the live relay, cached
    /// for a second so a polling client does not open a channel per call.
    fn services_containers(&self, root: &Path) -> (Vec<Value>, Vec<Value>, Option<String>) {
        let Some(relays) = &self.services.relays else {
            return (Vec::new(), Vec::new(), None);
        };
        let Ok(entry) = relays.get(root) else {
            return (Vec::new(), Vec::new(), None);
        };
        let mut cache = self
            .services
            .view_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((at, containers, ports)) = cache.get(root)
            && at.elapsed() < Duration::from_secs(1)
        {
            return (containers.clone(), ports.clone(), None);
        }
        match super::services_view::read(entry.relay()) {
            Ok((containers, ports)) => {
                cache.insert(
                    root.to_owned(),
                    (Instant::now(), containers.clone(), ports.clone()),
                );
                (containers, ports, None)
            }
            Err(reason) => (Vec::new(), Vec::new(), Some(format!("marsh: {reason}"))),
        }
    }

    /// Forwards recorded for the project: `{owner, kind, port, state, message}`.
    fn forwards_json(&self, root: &Path) -> Vec<Value> {
        self.services
            .forwards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|((project, _), _)| project == root)
            .flat_map(|((_, owner), ports)| {
                ports.iter().map(move |(port, (kind, state))| {
                    let (word, message) = match state {
                        marsh_worker::PortState::Forwarding { .. } => ("forwarding", None),
                        marsh_worker::PortState::Collision { message } => {
                            ("collision", Some(message.clone()))
                        }
                        marsh_worker::PortState::Failed { message } => {
                            ("failed", Some(message.clone()))
                        }
                        marsh_worker::PortState::Revoked => ("revoked", None),
                    };
                    json!({"owner": owner, "kind": kind, "port": port, "state": word, "message": message})
                })
            })
            .collect()
    }

    /// Revokes the project's relay and closes every channel on it.
    fn close_relay(&self, root: &Path, reason: &str) {
        if let Some(relays) = &self.services.relays {
            relays.close(root, reason);
        }
    }

    /// One tick of the idle timer and lease expiry.
    pub(super) fn services_timer_pass(&self) {
        self.services_recover_once();
        let tracked_empty = self
            .services
            .projects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty();
        let owned = self.sbx.owned_services_vms();
        if tracked_empty && owned.is_empty() {
            return;
        }
        let Ok(settings) = self.services_settings() else {
            return;
        };
        let now = Instant::now();
        let idle = Duration::from_secs(settings.idle_seconds);
        // A daemon that started with a services VM running (its holders and
        // lease died with the previous daemon) gives each one an idle timer.
        let running = self
            .sbx
            .stock_inventory(false)
            .map(|inventory| {
                owned
                    .iter()
                    .filter(|(name, _)| {
                        inventory.get(name).is_some_and(|vm| vm.status == "running")
                    })
                    .filter_map(|(_, key)| {
                        let path = PathBuf::from(key);
                        path.is_absolute().then_some(path)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut due = Vec::new();
        {
            let mut projects = self
                .services
                .projects
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for root in running {
                let project = projects.entry(root).or_default();
                if project.holders.is_empty()
                    && project.idle_deadline.is_none()
                    && !project.starting
                {
                    project.idle_deadline = Some(now + idle);
                }
            }
            for (root, project) in projects.iter_mut() {
                if project.starting {
                    continue;
                }
                if !settings.enabled_for(root) {
                    // Turning services off for a project revokes every holder
                    // and stops its VM at once; its data stays (`rm` deletes it).
                    project.holders.clear();
                    project.idle_deadline = Some(now);
                    due.push(root.clone());
                    continue;
                }
                // A hold whose time is up releases itself.
                let before = project.holders.len();
                project
                    .holders
                    .retain(|_, expiry| expiry.is_none_or(|expiry| expiry > now));
                if before > 0 && project.holders.is_empty() {
                    project.idle_deadline = Some(now + idle);
                }
                if project.holders.is_empty()
                    && project
                        .idle_deadline
                        .is_some_and(|deadline| deadline <= now)
                {
                    due.push(root.clone());
                }
            }
        }
        for root in due {
            self.services_idle_stop(&root, &settings);
        }
    }

    fn services_idle_stop(&self, root: &Path, settings: &Settings) {
        let lock = self.services.lock_for(root);
        let Ok(_operation) = lock.try_lock() else {
            return;
        };
        // Decided again under the project's lock: a holder or a start that
        // arrived meanwhile wins, unless services were turned off.
        let still_due = self.services.with_project(root, |project| {
            let now = Instant::now();
            if !settings.enabled_for(root) {
                project.holders.clear();
            }
            !project.starting
                && project.holders.is_empty()
                && project
                    .idle_deadline
                    .is_some_and(|deadline| deadline <= now)
        });
        if !still_due {
            return;
        }
        // Test break: the idle timer never stops the VM.
        if marsh_contracts::break_mode(services_breaks::IDLE_STOP_OFF) {
            self.services
                .with_project(root, |project| project.idle_deadline = None);
            return;
        }
        // A checkpoint capture in flight keeps the VM running this pass (test
        // break: the stop goes ahead under it). DECISIONS 12: the cutover. The
        // engine is fenced and frozen, every tracked workspace captured, the
        // outcomes journalled; then the stop, with the capture exclusion held
        // until the VM has stopped. Also when services were just turned off:
        // the stop keeps the data.
        let plan = match self.services_stop_prepare(root, "idle-stop", Duration::ZERO, None) {
            Ok(plan) => plan,
            Err(StopRefused::Busy(_)) => return,
            Err(StopRefused::Unsafe(why)) => {
                // Not made safe: the VM runs on, and is tried again after another idle period.
                eprintln!(
                    "marshd: services idle stop of {} refused: {why}",
                    root.display()
                );
                self.services.with_project(root, |project| {
                    project.failure = Some(format!("marsh: the idle stop was refused: {why}"));
                    project.idle_deadline =
                        Some(Instant::now() + Duration::from_secs(settings.idle_seconds.max(60)));
                });
                return;
            }
        };
        self.close_relay(root, "the Docker VM went idle and is being stopped");
        let result = self
            .sbx
            .stop_services_vm(root, Instant::now() + OPERATION_TIMEOUT);
        if result.is_err() {
            self.services_stop_abort(root, &plan);
        }
        drop(plan);
        self.services.with_project(root, |project| {
            project.idle_deadline = None;
            project.failure = result
                .as_ref()
                .err()
                .map(|error| format!("marsh: the idle stop failed: {error}"));
        });
        if let Err(error) = result {
            eprintln!("marshd: services idle stop of {}: {error}", root.display());
        }
    }

    /// `marsh stop` and the default `marsh reset` leave services VMs and their
    /// data alone (DECISIONS 18). A stop (the daemon is about to exit) stops
    /// the running ones so no VM runs on with nobody to idle it; a default
    /// reset (the daemon stays) skips them entirely. One component per VM.
    #[allow(clippy::too_many_lines)] // One ordered sequence: cutover each VM, stop, undo, report.
    pub(super) fn services_scope_teardown(
        &self,
        action: ScopeLifecycleAction,
        deadline: Instant,
    ) -> Vec<ScopeCleanupComponent> {
        let component = |name: String, key: &str, state, detail: String| ScopeCleanupComponent {
            kind: "services".into(),
            label: key.to_owned(),
            state,
            vm: Some(name),
            detail: Some(detail),
        };
        if action == ScopeLifecycleAction::Reset {
            return self
                .sbx
                .owned_services_vms()
                .into_iter()
                .map(|(name, key)| {
                    component(
                        name,
                        &key,
                        ScopeCleanupState::Kept,
                        "kept: `marsh reset --include-services` removes it".into(),
                    )
                })
                .collect();
        }
        // The cutover of every running services VM, before its relay closes and
        // before the stop, each under its capture exclusion, held until the
        // stops are done. A VM whose exclusion cannot be acquired (a capture
        // that does not finish) or whose cutover cannot be made safe is not
        // stopped: it is reported, and keeps running.
        let mut plans: Vec<(PathBuf, services_snapshots::StopPlan)> = Vec::new();
        let mut deferred: BTreeMap<String, String> = BTreeMap::new();
        let wait = self.stop_wait(Duration::from_secs(20));
        for (_, key) in self.sbx.owned_services_vms() {
            let root = PathBuf::from(&key);
            if !root.is_absolute() {
                continue;
            }
            let operation = self.services.lock_for(&root);
            let _operation = operation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match self.services_stop_prepare(&root, "stop", wait, Some(deadline)) {
                Ok(plan) => plans.push((root, plan)),
                Err(refused) => {
                    deferred.insert(key.clone(), refused.message().to_owned());
                }
            }
        }
        if let Some(relays) = &self.services.relays {
            for (root, _) in &plans {
                relays.close(root, "the daemon is stopping");
            }
            if deferred.is_empty() {
                relays.close_all("the daemon is stopping");
            }
        }
        // Test break: `marsh stop` removes what it should only stop.
        let results = if marsh_contracts::break_mode(services_breaks::SERVICES_STOP_DELETES_DATA) {
            self.sbx.remove_all_services_vms_before(deadline)
        } else {
            self.sbx.stop_services_vms_except(
                deadline,
                &deferred.keys().cloned().collect::<BTreeSet<_>>(),
            )
        };
        for (name, key, result) in &results {
            if result.is_err()
                && let Some((root, plan)) = plans
                    .iter()
                    .find(|(root, _)| root.to_str() == Some(key.as_str()))
            {
                let _ = name;
                self.services_stop_abort(root, plan);
            }
        }
        // The VMs are stopped (or the stop undone): captures may run again.
        drop(plans);
        let removed_by_break =
            marsh_contracts::break_mode(services_breaks::SERVICES_STOP_DELETES_DATA);
        let mut components = results
            .into_iter()
            .map(|(name, key, result)| match result {
                Ok(removed) if removed_by_break => component(
                    name,
                    &key,
                    if removed {
                        ScopeCleanupState::Removed
                    } else {
                        ScopeCleanupState::Absent
                    },
                    "removed".into(),
                ),
                Ok(_) => component(
                    name,
                    &key,
                    ScopeCleanupState::Kept,
                    "kept (stopped); its data stays".into(),
                ),
                Err(error) => component(
                    name.clone(),
                    &key,
                    ScopeCleanupState::CleanupUncertain,
                    format!("Docker VM {name} could not be stopped: {error}"),
                ),
            })
            .collect::<Vec<_>>();
        for (key, why) in deferred {
            let name = self
                .sbx
                .owned_services_vms()
                .into_iter()
                .find_map(|(name, owned)| (owned == key).then_some(name))
                .unwrap_or_default();
            components.push(component(
                name,
                &key,
                ScopeCleanupState::CleanupUncertain,
                format!("not stopped, still running: {why}; `marsh docker stop` stops it"),
            ));
        }
        // The daemon is going away: no holder survives it.
        self.services
            .projects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        components
    }

    /// `marsh reset --include-services`: remove the services VMs of exactly
    /// the confirmed projects; every other one is kept and reported.
    pub(super) fn services_remove_confirmed(
        &self,
        confirmed: &[String],
        deadline: Instant,
    ) -> Vec<ScopeCleanupComponent> {
        // Test break: the flag is accepted and does nothing.
        if marsh_contracts::break_mode(services_breaks::RESET_INCLUDE_SERVICES_NOOP) {
            return Vec::new();
        }
        let confirmed = confirmed.iter().cloned().collect::<BTreeSet<_>>();
        for root in &confirmed {
            self.close_relay(Path::new(root), "the Docker VM is being removed");
        }
        let components = self
            .sbx
            .remove_confirmed_services_vms_before(&confirmed, deadline)
            .into_iter()
            .map(|(name, key, result)| {
                let (state, detail) = match result {
                    Ok(Some(true)) => (ScopeCleanupState::Removed, None),
                    Ok(Some(false)) => (ScopeCleanupState::Absent, None),
                    Ok(None) => (
                        ScopeCleanupState::Kept,
                        Some("kept: this project was not among those confirmed".into()),
                    ),
                    Err(error) => (
                        ScopeCleanupState::CleanupUncertain,
                        Some(format!(
                            "Docker VM {name} cleanup could not be verified: {error}"
                        )),
                    ),
                };
                ScopeCleanupComponent {
                    kind: "services".into(),
                    label: key,
                    state,
                    vm: Some(name),
                    detail,
                }
            })
            .collect::<Vec<_>>();
        let mut projects = self
            .services
            .projects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for component in &components {
            if component.state == ScopeCleanupState::Removed {
                projects.remove(Path::new(&component.label));
                self.services_reap_forget_project(Path::new(&component.label));
            }
        }
        drop(projects);
        for component in &components {
            if component.state == ScopeCleanupState::Removed {
                self.forget_last_capture(Path::new(&component.label));
            }
        }
        components
    }
}

/// What the project's services containers can reach right now, in one line.
fn effective_internet(on: bool, governance: &Governance, gate: Option<&Gate>) -> String {
    if governance.governed {
        return governance.summary.clone();
    }
    // Hosts the user's own global deny rules block: shown whatever the state,
    // since they narrow what the services can reach and do not govern the toggle.
    let narrowing = if governance.also_denied.is_empty() {
        String::new()
    } else {
        format!(
            "; your global rules also deny {}",
            governance.also_denied.join(", ")
        )
    };
    match (on, gate) {
        (true, Some(Gate::Open)) => format!("on: containers can reach the internet{narrowing}"),
        (false, Some(Gate::Closed)) => format!(
            "off: containers cannot start connections out; image pulls from the default registries work{narrowing}"
        ),
        (_, Some(Gate::Unverified(reason))) => {
            format!("unverified: the engine's gate does not match the grant ({reason}){narrowing}")
        }
        (true, Some(Gate::Closed)) => {
            format!("on is granted but the running engine's gate is closed{narrowing}")
        }
        (false, Some(Gate::Open)) => {
            format!("off is granted but the running engine's gate is open{narrowing}")
        }
        (true, _) => format!(
            "on: applies when the engine starts; containers will reach the internet{narrowing}"
        ),
        (false, _) => format!(
            "off: applies when the engine starts; containers will have no internet{narrowing}"
        ),
    }
}

/// Why a project's services are unavailable, as `summary` says it.
#[derive(Clone, Copy, Debug)]
enum Unavailable {
    NotResponding,
    CouldNotStart,
    NeedsRestart,
    SetupChanged,
    Quarantined,
    LostContact,
}

impl Unavailable {
    /// Short and plain: none of marsh's internals (no VM, volume, snapshot,
    /// engine or sbx).
    fn plain(self) -> &'static str {
        match self {
            Self::NotResponding => "the sandbox service is not responding",
            Self::CouldNotStart => "it could not start; try starting it again",
            Self::NeedsRestart => "it needs a restart",
            Self::SetupChanged => "its setup changed; it needs a restart",
            Self::Quarantined => "it was stopped for safety; it needs a restart",
            Self::LostContact => "marsh lost contact with it; it needs a restart",
        }
    }
}

fn running_count(containers: &Value) -> usize {
    containers.as_array().map_or(0, |all| {
        all.iter().filter(|c| c["state"] == "running").count()
    })
}

struct SummaryInput {
    state: &'static str,
    enabled: bool,
    /// This build has what app services need.
    can_run: bool,
    /// Running containers, when they could be read.
    running: Option<usize>,
    cause: Option<Unavailable>,
}

/// The plain-language line of a project's entry in `services status --json`
/// (`summary`): what a client shows instead of rewriting `reason`.
fn summary(input: &SummaryInput) -> String {
    if !input.can_run {
        return "Unavailable: this build of marsh cannot run Docker for projects".into();
    }
    match (input.state, input.enabled) {
        ("off", false) => "Off".into(),
        ("off", true) => "On — nothing has been started yet".into(),
        ("starting", _) => "Starting".into(),
        ("stopped", _) => "Stopped — Docker data kept".into(),
        ("ready", _) => match input.running {
            Some(0) => "Ready — no containers running".into(),
            Some(1) => "Ready — 1 container".into(),
            Some(count) => format!("Ready — {count} containers"),
            None => "Ready".into(),
        },
        _ => format!(
            "Unavailable: {}",
            input.cause.unwrap_or(Unavailable::NeedsRestart).plain()
        ),
    }
}

/// State of a VM stock lists, from the adapter's live view.
fn classify(
    view: &ServicesVmView,
    failure: Option<String>,
) -> (&'static str, Option<String>, Option<ServicesVmView>) {
    let (state, reason) = if !view.running {
        ("stopped", None)
    } else if view.lease_live && view.generation.is_some() {
        ("ready", None)
    } else {
        (
            "unavailable",
            Some(failure.unwrap_or_else(|| {
                "marsh: the Docker VM runs without a live lease; `marsh docker start` makes it ready again".into()
            })),
        )
    };
    (state, reason, Some(view.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(config: Option<&str>) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        if let Some(config) = config {
            fs::write(directory.path().join("config.json"), config).unwrap();
        }
        directory
    }

    /// The summary of every state (table), and no internals in any of them.
    #[test]
    fn the_summary_is_plain_in_every_state() {
        let ready = |running| SummaryInput {
            state: "ready",
            enabled: true,
            can_run: true,
            running,
            cause: None,
        };
        let unavailable = |cause| SummaryInput {
            state: "unavailable",
            enabled: true,
            can_run: true,
            running: None,
            cause: Some(cause),
        };
        let other = |state, enabled| SummaryInput {
            state,
            enabled,
            can_run: true,
            running: None,
            cause: None,
        };
        let mut cases = vec![
            (ready(Some(4)), "Ready — 4 containers"),
            (ready(Some(1)), "Ready — 1 container"),
            (ready(Some(0)), "Ready — no containers running"),
            (ready(None), "Ready"),
            (other("stopped", true), "Stopped — Docker data kept"),
            (other("starting", true), "Starting"),
            (other("off", false), "Off"),
            (other("off", true), "On — nothing has been started yet"),
            (
                SummaryInput {
                    can_run: false,
                    ..other("off", true)
                },
                "Unavailable: this build of marsh cannot run Docker for projects",
            ),
        ];
        for cause in [
            Unavailable::NotResponding,
            Unavailable::CouldNotStart,
            Unavailable::NeedsRestart,
            Unavailable::SetupChanged,
            Unavailable::Quarantined,
            Unavailable::LostContact,
        ] {
            cases.push((unavailable(cause), cause.plain()));
        }
        for (input, expected) in &cases {
            let text = summary(input);
            assert!(text.contains(expected), "{text}");
            let lower = text.to_ascii_lowercase();
            for word in [
                "vm", "volume", "snapshot", "engine", "sbx", "relay", "marsh:",
            ] {
                assert!(
                    !lower
                        .split(|c: char| !c.is_alphanumeric() && c != ':')
                        .any(|w| w == word),
                    "{text} names an internal: {word}"
                );
            }
        }
    }

    #[test]
    fn docker_is_on_with_a_forty_gigabyte_disk_and_fifteen_minutes_by_default() {
        let scope = scope(None);
        assert_eq!(
            settings(scope.path(), None).unwrap(),
            Settings {
                projects: BTreeSet::new(),
                off_projects: BTreeSet::new(),
                default_on: true,
                disk_size: "40g".into(),
                idle_seconds: 900
            }
        );
    }

    /// Whether Docker is on for a project: its own setting (off wins), else
    /// the home's default (table of config, project, effective value).
    #[test]
    fn a_projects_own_setting_beats_the_default_and_off_beats_on() {
        for (config, project, expected) in [
            (r"{}", "/Users/a/one", true),
            (
                r#"{"docker_projects_off":["/Users/a/one"]}"#,
                "/Users/a/one",
                false,
            ),
            (
                r#"{"docker_projects_off":["/Users/a/one"]}"#,
                "/Users/a/two",
                true,
            ),
            // A subfolder is another project.
            (
                r#"{"docker_projects_off":["/Users/a/one"]}"#,
                "/Users/a/one/sub",
                true,
            ),
            (r#"{"docker_default_on":false}"#, "/Users/a/one", false),
            (
                r#"{"docker_default_on":false,"services_projects":["/Users/a/one"]}"#,
                "/Users/a/one",
                true,
            ),
            (
                r#"{"docker_default_on":false,"services_projects":["/Users/a/one"]}"#,
                "/Users/a/two",
                false,
            ),
            (
                r#"{"docker_default_on":false,"services_projects":["/Users/a/one"],"docker_projects_off":["/Users/a/one"]}"#,
                "/Users/a/one",
                false,
            ),
            // The old opt-in list alone changes nothing when the default is on.
            (
                r#"{"services_projects":["/Users/a/one"]}"#,
                "/Users/a/two",
                true,
            ),
        ] {
            let scope = scope(Some(config));
            let configured = settings(scope.path(), None).unwrap();
            assert_eq!(
                configured.enabled_for(Path::new(project)),
                expected,
                "{config} {project}"
            );
        }
    }

    #[test]
    fn config_json_and_the_launch_environment_set_the_services_settings() {
        let scope = scope(Some(
            r#"{"docker_default_on":false,"services_projects":["/Users/a/one","/Users/a/two"],"services_disk_size":"20g","services_idle_seconds":120}"#,
        ));
        let configured = settings(scope.path(), None).unwrap();
        // Per project: one enabled project does not enable another, nor a
        // subfolder of it (with the default off).
        assert!(configured.enabled_for(Path::new("/Users/a/one")));
        assert!(configured.enabled_for(Path::new("/Users/a/two")));
        assert!(!configured.enabled_for(Path::new("/Users/a/three")));
        assert!(!configured.enabled_for(Path::new("/Users/a/one/sub")));
        assert_eq!(configured.disk_size, "20g");
        assert_eq!(configured.idle_seconds, 120);
        // MARSH_SERVICES_IDLE_SECONDS at daemon start wins.
        assert_eq!(settings(scope.path(), Some(30)).unwrap().idle_seconds, 30);
    }

    #[test]
    fn a_malformed_services_setting_is_reported_not_defaulted() {
        for config in [
            r#"{"services_projects":true}"#,
            r#"{"services_projects":["relative/path"]}"#,
            r#"{"services_projects":[7]}"#,
            r#"{"docker_projects_off":"/Users/a/one"}"#,
            r#"{"docker_projects_off":["relative"]}"#,
            r#"{"docker_default_on":"yes"}"#,
            r#"{"services_disk_size":"forty"}"#,
            r#"{"services_idle_seconds":0}"#,
            r#"{"services_idle_seconds":"900"}"#,
        ] {
            let scope = scope(Some(config));
            assert!(settings(scope.path(), None).is_err(), "{config}");
        }
    }
}
