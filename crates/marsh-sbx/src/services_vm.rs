//! A project's services VM (`docs/design/services.md`).
//!
//! One VM per canonical project root per daemon scope, created from the marsh
//! shell image with no workspace and no home. Its only host share is the
//! project, read-write at its Mac path, mounted through the shell mount
//! machinery. Names are random (`<vm_prefix>v-xxxxxxxx`) and recorded in the
//! ownership map before `sbx create`, like Kit and shell VMs; every stop and
//! removal here is by recorded UUID.
//!
//! The VM's Docker disk is sized per sandbox: the create invocation (and only
//! that invocation) carries `DOCKER_SANDBOXES_DOCKER_SIZE`, which stock SBX
//! honors for that one sandbox. The host-global `sandbox.disk.dockerVolume`
//! setting is never read or changed.

use super::{
    AdmittedHostGrant, KEEPALIVE_READY_TIMEOUT, KIT_VM_BASIS, OwnedWorkerMarker, ReadyShellVm,
    SUPERVISOR_PING_TIMEOUT, SUPERVISOR_RECENT, SbxError, ShellTemplateReference, ShellUser,
    StockSbx, VmClass, VmPurpose, artifact_identity, safe_absolute, shell_admission_before,
    shell_supervisor, validate_name, weak_mutex_transition_lock, weak_rw_transition_lock,
};
use marsh_contracts::{MountAccess, OciImage};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::Ordering},
    time::Instant,
};

/// The services VM's break modes (test builds only; `docs/design/test-breaks.md`).
/// Each is read through [`marsh_contracts::break_mode`], the one guarded switch:
/// the daemon arms it with `--test-breaks-from-env`, and a build without the
/// `test-breaks` feature answers `false` to every name.
pub mod breaks {
    pub const VM_PER_SCOPE: &str = "vm-per-scope";
    pub const LEASE_NONE: &str = "lease-none";
    pub const IDLE_STOP_OFF: &str = "idle-stop-off";
    pub const SERVICES_OWNER_UUID_CHECK_OFF: &str = "services-owner-uuid-check-off";
    pub const SERVICES_STOP_DELETES_DATA: &str = "services-stop-deletes-data";
    pub const RESET_SKIP_CONFIRMATION: &str = "reset-skip-confirmation";
    pub const RESET_INCLUDE_SERVICES_NOOP: &str = "reset-include-services-noop";
    /// Readiness tolerates a host mount other than the project.
    pub const SERVICES_EXTRA_MOUNT_OK: &str = "services-extra-mount-ok";
    /// One enabled project enables services for every project of the home.
    pub const SERVICES_ENABLE_PER_HOME: &str = "services-enable-per-home";
    /// A running VM's secrets are reconciled only at a cold start: neither a
    /// warm ensure nor `services status` shadows a newly visible global secret.
    pub const SERVICES_WARM_RECONCILE_OFF: &str = "services-warm-reconcile-off";
    /// The create does not carry the Docker disk size, so stock sizes the
    /// disk by default and the wrong-size removal path runs for real.
    pub const SERVICES_DISK_SIZE_ENV_OFF: &str = "services-disk-size-env-off";
    /// A project nobody toggled starts with Internet on (E5, VM-08 c1, SEC-17 c2).
    pub const SERVICES_DEFAULT_ON: &str = "services-default-on";
    /// `internet on` and `internet off` report success and change nothing (E5).
    pub const INTERNET_NOOP: &str = "internet-noop";
    /// `internet off` reports success and changes nothing (E5, VM-08 c3).
    pub const INTERNET_OFF_NOOP: &str = "internet-off-noop";
    /// `internet on|off` goes ahead although a policy governs (E5, VM-09 c2).
    pub const INTERNET_IGNORE_GOVERNED: &str = "internet-ignore-governed";
    /// Any global deny governs, however narrow (E5, VM-09 c4).
    pub const GOVERNANCE_ANY_DENY: &str = "governance-any-deny";
    /// A job's capability may run `marsh services ...` (E5, SEC-09 c1).
    pub const SERVICES_CLI_IN_JOB: &str = "services-cli-in-job";
    /// The Internet gate addresses the engine container by its name, not the
    /// id recorded at creation (HF1).
    pub const GATE_BY_NAME: &str = "gate-by-name";

    /// Every services break mode this crate and the backend know: the list a
    /// daemon's registry of known modes includes.
    pub const ALL: [&str; 18] = [
        VM_PER_SCOPE,
        LEASE_NONE,
        IDLE_STOP_OFF,
        SERVICES_OWNER_UUID_CHECK_OFF,
        SERVICES_STOP_DELETES_DATA,
        RESET_SKIP_CONFIRMATION,
        RESET_INCLUDE_SERVICES_NOOP,
        SERVICES_EXTRA_MOUNT_OK,
        SERVICES_ENABLE_PER_HOME,
        SERVICES_DISK_SIZE_ENV_OFF,
        SERVICES_WARM_RECONCILE_OFF,
        SERVICES_DEFAULT_ON,
        INTERNET_NOOP,
        INTERNET_OFF_NOOP,
        INTERNET_IGNORE_GOVERNED,
        GOVERNANCE_ANY_DENY,
        SERVICES_CLI_IN_JOB,
        GATE_BY_NAME,
    ];

    /// The services modes selected for this daemon (empty in a build without
    /// `test-breaks`, and in any daemon not launched with `--test-breaks-from-env`).
    #[must_use]
    pub fn active() -> Vec<&'static str> {
        ALL.into_iter()
            .filter(|mode| marsh_contracts::break_mode(mode))
            .collect()
    }
}

/// The per-sandbox Docker disk size variable stock SBX reads on `create`.
const DISK_SIZE_VARIABLE: &str = "DOCKER_SANDBOXES_DOCKER_SIZE";
/// A fresh VM's Docker filesystem must be at least this share of the size
/// asked for (ext4 keeps about 5%): less means stock ignored the variable.
const DISK_SIZE_FLOOR_PERCENT: u64 = 90;
/// The value of every dummy sandbox-scoped secret: not a credential.
const DUMMY_SECRET: &str = "marsh-services-dummy-not-a-credential";
const GUEST_BINARY_TEMPORARY: &str = "/tmp/marsh-services.install";
/// The root step: install the trusted marsh binary and, last, the marker.
const SERVICES_SETUP_SCRIPT: &str = concat!(
    "set -eu; umask 022; install -d -m 0755 /usr/local/bin; ",
    "install -m 0755 /tmp/marsh-services.install /usr/local/bin/marsh; ",
    "rm -f /tmp/marsh-services.install; ",
    "umask 077; mkdir -p /var/lib/marsh; ",
    "printf '%s\\n' \"$MARSH_WORKER_MARKER\" > /var/lib/marsh/worker.json",
);
/// Boot id, the project's mount line, then the Docker filesystem's size in
/// bytes: one root exec.
const SERVICES_VERIFY_SCRIPT: &str = concat!(
    "set -eu; cat /proc/sys/kernel/random/boot_id; ",
    "findmnt -rn -M \"$1\" -o TARGET,OPTIONS | head -n 1; ",
    "df -B1 --output=size /var/lib/docker | tail -n 1",
);

/// Exact inputs for one project's services VM.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServicesVmSpec {
    /// Canonical project root: the ownership key and the only mount.
    pub project: PathBuf,
    /// The marsh shell image (a digest reference, as the shell VM's).
    pub image: OciImage,
    /// The marsh guest binary installed at `/usr/local/bin/marsh`.
    pub shell_binary: PathBuf,
    /// Docker disk size for a new VM, for example `40g`. Existing VMs keep
    /// the size they were created with.
    pub disk_size: String,
}

/// Readiness generation: the VM's boot id plus this daemon's counter, which
/// advances every time the VM is made ready again.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ServicesGeneration {
    pub boot_id: String,
    pub counter: u64,
}

/// A services VM that is up with its project mount verified.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadyServicesVm {
    pub name: String,
    pub uuid: String,
    pub generation: ServicesGeneration,
    pub cold_started: bool,
    /// Size in bytes of the VM's `/var/lib/docker` filesystem as the guest
    /// reports it (0 when this call did not read it).
    pub docker_disk_bytes: u64,
}

/// Which of the host's global service secrets stock SBX would inject into a
/// services VM's traffic, as `marsh services status` reports it
/// (SECURITY-DESIGN A12). Names only, never values.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CredentialExposure {
    /// Global secrets a sandbox-scoped dummy shadows for this VM.
    pub shadowed: Vec<String>,
    /// Global secrets no dummy shadows (stock cannot, or marsh has not yet).
    pub unshadowable: Vec<String>,
    /// Stock attaches its MCP gateway to the VM (`sbx inspect`).
    pub mcp_gateway: bool,
}

/// What stock SBX and this adapter know about one services VM now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServicesVmView {
    pub name: String,
    pub uuid: String,
    pub running: bool,
    /// The retained keep-alive transport is live (a live generation).
    pub lease_live: bool,
    pub generation: Option<ServicesGeneration>,
}

/// A size is `<digits>g` (the unit stock documents), 1 GiB to 4 TiB.
/// Sorted names of the `service` rows whose `scope` is exactly `scope`.
/// A row without a string `scope` is a schema error: failing closed beats
/// guessing which scope a changed listing format means.
fn service_names_in_scope(
    rows: &[serde_json::Value],
    scope: &str,
) -> Result<Vec<String>, SbxError> {
    let mut names = Vec::new();
    for row in rows {
        if row.get("type").and_then(serde_json::Value::as_str) != Some("service") {
            continue;
        }
        let row_scope = row
            .get("scope")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| SbxError::LifecycleSchema {
                operation: "list service secrets",
                detail: "a service secret row has no `scope`".into(),
            })?;
        if row_scope == scope
            && let Some(name) = row.get("name").and_then(serde_json::Value::as_str)
        {
            names.push(name.to_owned());
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

fn valid_disk_size(value: &str) -> bool {
    value
        .strip_suffix(['g', 'G'])
        .and_then(|digits| digits.parse::<u32>().ok())
        .is_some_and(|gib| (1..=4096).contains(&gib))
}

impl StockSbx {
    fn services_key(project: &Path) -> Result<String, SbxError> {
        if !safe_absolute(project) {
            return Err(SbxError::UnsafePath(project.to_owned()));
        }
        // Test break: one VM per daemon scope instead of per project.
        if marsh_contracts::break_mode(breaks::VM_PER_SCOPE) {
            return Ok("scope".into());
        }
        project
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| SbxError::UnsafePath(project.to_owned()))
    }

    /// The recorded services VM of `project`, without assigning one.
    #[must_use]
    pub fn recorded_services_vm(&self, project: &Path) -> Option<String> {
        let key = Self::services_key(project).ok()?;
        self.ownership.lookup(VmPurpose::Services, &key)
    }

    /// Every owned services VM as `(name, project key)`.
    #[must_use]
    pub fn owned_services_vms(&self) -> Vec<(String, String)> {
        self.ownership.owned_services()
    }

    fn services_init_lock(&self, name: &str) -> Arc<Mutex<()>> {
        self.shell_init_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(name.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Create (or reuse) the project's services VM and return it ready: up,
    /// the marsh binary installed, the project mounted read-write at its Mac
    /// path and verified, and a retained keep-alive lease live. Idempotent.
    ///
    /// # Errors
    /// Fails closed on a foreign name, a failed mount, a changed global
    /// setting, or any stock failure; the VM is never reported ready then.
    pub fn ensure_services_vm(&self, spec: &ServicesVmSpec) -> Result<ReadyServicesVm, SbxError> {
        if !valid_disk_size(&spec.disk_size) {
            return Err(SbxError::HostGrantFence(format!(
                "services.diskSize {:?} is not a size like 40g",
                spec.disk_size
            )));
        }
        let key = Self::services_key(&spec.project)?;
        let name = self.ownership.assign(VmPurpose::Services, &key)?;
        validate_name(&name)?;
        let init_lock = self.services_init_lock(&name);
        let _initializing = init_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.reject_quarantined_shell(&name)?;
        self.ensure_services_vm_inner(spec, &key, name)
    }

    #[allow(clippy::too_many_lines)] // One ordered sequence: classify, create, init, mount, verify, lease.
    fn ensure_services_vm_inner(
        &self,
        spec: &ServicesVmSpec,
        key: &str,
        mut name: String,
    ) -> Result<ReadyServicesVm, SbxError> {
        let artifact = self.cached_artifact_identity(&spec.shell_binary)?;
        if self.shell_lease_live(&name)?
            && let Some(generation) = self
                .services_generations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&name)
                .cloned()
            && let VmClass::Owned {
                uuid,
                running: true,
            } = self.classify_vm(&name, false)?
        {
            // A global service secret that appeared while the VM kept running
            // is shadowed now, before readiness is reported.
            if !marsh_contracts::break_mode(breaks::SERVICES_WARM_RECONCILE_OFF) {
                self.shadow_global_secrets(&name)?;
            }
            if let Some(problem) = self.services_mounts_problem(&name, &spec.project)? {
                return Err(SbxError::HostGrantFence(problem));
            }
            return Ok(ReadyServicesVm {
                name,
                uuid,
                generation,
                cold_started: false,
                docker_disk_bytes: 0,
            });
        }
        self.services_generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&name);
        self.invalidate_shell_cache(&name);
        let cold_started = match self.classify_vm(&name, false)? {
            VmClass::Foreign
                if marsh_contracts::break_mode(breaks::SERVICES_OWNER_UUID_CHECK_OFF) =>
            {
                // Test break: adopt whatever holds the name.
                self.adopt_created(&name)?;
                false
            }
            VmClass::Foreign => {
                // Our recorded VM is gone and a VM that is not ours now holds
                // its name: never exec into, adopt or remove it. This project
                // takes a new random name.
                self.drop_stale_services_record(&name)?;
                name = self.ownership.assign(VmPurpose::Services, key)?;
                self.create_services_vm(spec, &name)?;
                true
            }
            VmClass::Owned { .. } => {
                self.verify_services_marker(&name, &format!("services:{name}"))?;
                false
            }
            VmClass::Absent => {
                self.create_services_vm(spec, &name)?;
                true
            }
        };
        let marker = format!("services:{name}");
        self.initialize_services_guest(spec, &name, &marker, &artifact)?;
        self.shadow_global_secrets(&name)?;
        let uuid = match self.classify_vm(&name, true)? {
            VmClass::Owned { uuid, .. } => uuid,
            VmClass::Foreign => return Err(SbxError::ForeignVm(name)),
            VmClass::Absent => {
                self.ownership.invalidate();
                return Err(SbxError::HostGrantFence(format!(
                    "services VM {name} is absent from stock inventory"
                )));
            }
        };
        let (boot_id, docker_disk_bytes) = self.mount_and_verify_services_project(spec, &name)?;
        if cold_started {
            self.verify_created_docker_size(spec, &name, docker_disk_bytes)?;
        }
        let generation = ServicesGeneration {
            boot_id,
            counter: self.services_counter.fetch_add(1, Ordering::Relaxed),
        };
        self.start_services_lease(&name)?;
        self.services_generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name.clone(), generation.clone());
        Ok(ReadyServicesVm {
            name,
            uuid,
            generation,
            cold_started,
            docker_disk_bytes,
        })
    }

    /// The recorded VM is gone from stock's inventory (removed elsewhere)
    /// while another VM holds its name: drop the stale record and leave the
    /// impostor alone.
    fn drop_stale_services_record(&self, name: &str) -> Result<(), SbxError> {
        let view = self.stock_view(true)?;
        if let Some(recorded) = self.ownership.uuid(name)
            && view.contains_uuid(&recorded)
        {
            // Our recorded VM still exists under another name: not ours to
            // untangle.
            return Err(SbxError::ForeignVm(name.to_owned()));
        }
        self.forget_services_state(name);
        self.ownership.forget(name)
    }

    pub(crate) fn create_services_vm(
        &self,
        spec: &ServicesVmSpec,
        name: &str,
    ) -> Result<(), SbxError> {
        let template = ShellTemplateReference::parse(spec.image.as_str())?;
        let selected = template.create_reference()?;
        let pull = if template.requires_local_authority() {
            "never"
        } else {
            "missing"
        };
        let expected_digest = (selected != template.image().as_str()).then(|| template.digest());
        // Stock sizes this one sandbox's Docker volume from the variable of
        // this one `sbx create`; nothing global is changed.
        let size = spec.disk_size.to_ascii_lowercase();
        // Test break: the size is not passed, so stock's default disk results.
        let environment: &[(&str, &str)] =
            if marsh_contracts::break_mode(breaks::SERVICES_DISK_SIZE_ENV_OFF) {
                &[]
            } else {
                &[(DISK_SIZE_VARIABLE, size.as_str())]
            };
        self.create_shell_vm_named(
            name,
            selected,
            pull,
            expected_digest,
            environment,
            // No MCP servers of the host's static set (SECURITY-DESIGN A12).
            &["--static-mcp", "none"],
        )
    }

    /// A fresh VM's Docker filesystem must be about the size asked for. A
    /// VM that is not removes itself (it was just created, so it is ours)
    /// and the start fails: the size is never silently wrong.
    fn verify_created_docker_size(
        &self,
        spec: &ServicesVmSpec,
        name: &str,
        observed: u64,
    ) -> Result<(), SbxError> {
        let gibibytes = spec
            .disk_size
            .trim_end_matches(['g', 'G'])
            .parse::<u64>()
            .unwrap_or(0);
        let wanted = gibibytes.saturating_mul(1 << 30);
        if observed.saturating_mul(100) >= wanted.saturating_mul(DISK_SIZE_FLOOR_PERCENT) {
            return Ok(());
        }
        let deadline = Instant::now() + self.preparation_timeouts.sbx;
        self.forget_services_state(name);
        // The dummies are scoped to this VM and outlive it unless removed too.
        let dummies = self.remove_sandbox_secrets(name);
        self.remove_owned_vm(name, deadline)?;
        Err(SbxError::HostGrantFence(format!(
            "stock SBX created services VM {name} with a {observed}-byte Docker disk, not {}; removed it and its {dummies} dummy secrets",
            spec.disk_size
        )))
    }

    /// Reading the marker is what starts a stopped VM.
    fn verify_services_marker(&self, vm: &str, profile: &str) -> Result<(), SbxError> {
        let output = self.run_bounded_os(
            "read services VM marker",
            self.preparation_timeouts.sbx,
            [
                "exec",
                "-u",
                "root",
                vm,
                "cat",
                "/var/lib/marsh/worker.json",
            ],
        )?;
        if !output.succeeded() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let hint = if stderr.contains("cannot restore mount") {
                "; the project folder is gone from the Mac: restore it, or remove the VM with `marsh services rm`"
            } else {
                ""
            };
            return Err(SbxError::UnreadableVm {
                vm: vm.to_owned(),
                detail: format!("{}{hint}", stderr.trim()),
            });
        }
        // An interrupted setup has no marker yet (the script writes it
        // last): setup simply runs again.
        let Ok(actual) = serde_json::from_slice::<OwnedWorkerMarker>(&output.stdout) else {
            return Ok(());
        };
        if actual.schema != "marsh.worker/v1" || actual.basis != KIT_VM_BASIS {
            return Err(SbxError::IncompatibleVm {
                vm: vm.to_owned(),
                detail: "its marker is not a marsh marker".into(),
            });
        }
        if actual.profile != profile {
            return Err(SbxError::IncompatibleVm {
                vm: vm.to_owned(),
                detail: format!(
                    "its marker profile is {:?}, expected {profile:?}",
                    actual.profile
                ),
            });
        }
        Ok(())
    }

    fn initialize_services_guest(
        &self,
        spec: &ServicesVmSpec,
        vm: &str,
        marker: &str,
        artifact: &str,
    ) -> Result<(), SbxError> {
        self.copy_trusted_binaries(vm, &[(spec.shell_binary.as_path(), GUEST_BINARY_TEMPORARY)])?;
        let environment = format!(
            "MARSH_WORKER_MARKER={}",
            serde_json::to_string(&Self::marker(marker))?
        );
        let output = self.run_bounded_os(
            "initialize services VM",
            self.preparation_timeouts.sbx,
            [
                "exec",
                "-u",
                "root",
                "-e",
                environment.as_str(),
                vm,
                "sh",
                "-c",
                SERVICES_SETUP_SCRIPT,
            ],
        )?;
        Self::require_success("initialize services VM", output)?;
        if artifact_identity(&spec.shell_binary)? != artifact {
            return Err(SbxError::SourceChanged(spec.shell_binary.clone()));
        }
        Ok(())
    }

    /// Mount only the project (read-write, at its Mac path) through the
    /// shell mount machinery and verify it inside the VM. Returns the boot id
    /// and the Docker filesystem's size in bytes.
    fn mount_and_verify_services_project(
        &self,
        spec: &ServicesVmSpec,
        name: &str,
    ) -> Result<(String, u64), SbxError> {
        let grant = AdmittedHostGrant::open(
            spec.project.clone(),
            spec.project.clone(),
            MountAccess::ReadWrite,
        )?;
        self.preflight_host_grants_evicting_idle(std::slice::from_ref(&grant))?;
        let vm = Self::services_shell_handle(name);
        // A previous acquisition of this daemon is replaced, not stacked.
        self.release_services_mounts(name);
        let mounts = self.prepare_shell_mounts(&vm, std::slice::from_ref(&grant))?;
        self.services_mounts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name.to_owned(), mounts);
        let project = spec
            .project
            .to_str()
            .ok_or_else(|| SbxError::UnsafePath(spec.project.clone()))?;
        let output = self.run_bounded_os(
            "verify services VM mount",
            self.preparation_timeouts.sbx,
            [
                "exec",
                "-u",
                "root",
                name,
                "sh",
                "-c",
                SERVICES_VERIFY_SCRIPT,
                "sh",
                project,
            ],
        )?;
        Self::require_success("verify services VM mount", output.clone())?;
        let text = String::from_utf8_lossy(&output.stdout);
        let mut lines = text.lines();
        let boot_id = lines.next().unwrap_or_default().trim().to_owned();
        let mount = lines.next().unwrap_or_default();
        let (target, options) = mount.split_once(' ').unwrap_or((mount, ""));
        let writable = options.split(',').any(|option| option == "rw");
        if boot_id.is_empty() || target != project || !writable {
            return Err(SbxError::HostGrantFence(format!(
                "services VM {name} does not show {project} mounted read-write (saw {mount:?})"
            )));
        }
        if let Some(problem) = self.services_mounts_problem(name, &spec.project)? {
            return Err(SbxError::HostGrantFence(problem));
        }
        let docker_bytes = lines
            .next()
            .and_then(|line| line.trim().parse::<u64>().ok())
            .ok_or_else(|| {
                SbxError::HostGrantFence(format!(
                    "services VM {name} did not report its Docker disk size"
                ))
            })?;
        Ok((boot_id, docker_bytes))
    }

    /// The shell mount machinery addresses a VM by name; a services VM has
    /// no shell user.
    fn services_shell_handle(name: &str) -> ReadyShellVm {
        ReadyShellVm {
            name: name.to_owned(),
            user: ShellUser {
                name: "services".into(),
                uid: 0,
                gid: 0,
                home: PathBuf::from("/root"),
            },
            cold_started: false,
        }
    }

    fn release_services_mounts(&self, name: &str) {
        let held = self
            .services_mounts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(name);
        if let Some(held) = held {
            let _ = self.revoke_shell_mounts(&held);
        }
    }

    /// The retained keep-alive: `sbx exec -i -u root VM marsh --internal-supervisor`,
    /// the same transport a warm shell VM keeps. While it is live the VM runs.
    fn start_services_lease(&self, name: &str) -> Result<(), SbxError> {
        let invocation = self.invocation([
            "exec",
            "-i",
            "-u",
            "root",
            "-w",
            "/",
            name,
            "/usr/local/bin/marsh",
            "--internal-supervisor",
        ]);
        let attachment = self.runner.spawn_attached(&invocation)?;
        let generation = self.shell_generation.fetch_add(1, Ordering::Relaxed);
        let lease =
            shell_supervisor::Supervisor::start(attachment, generation, KEEPALIVE_READY_TIMEOUT)
                .map_err(|_| SbxError::ShellLeaseLost(name.to_owned()))?;
        self.shell_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name.to_owned(), lease);
        Ok(())
    }

    /// Stock SBX's and this adapter's view of a services VM, from the
    /// cached inventory unless `refresh`.
    ///
    /// # Errors
    /// Fails when stock SBX cannot list its VMs.
    pub fn services_vm_view(
        &self,
        project: &Path,
        refresh: bool,
    ) -> Result<Option<ServicesVmView>, SbxError> {
        let Some(name) = self.recorded_services_vm(project) else {
            return Ok(None);
        };
        Ok(match self.classify_vm(&name, refresh)? {
            VmClass::Owned { uuid, running } => {
                let lease_live = running && self.services_lease_present(&name);
                Some(ServicesVmView {
                    generation: self
                        .services_generations
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(&name)
                        .cloned()
                        .filter(|_| lease_live),
                    name,
                    uuid,
                    running,
                    lease_live,
                })
            }
            VmClass::Foreign | VmClass::Absent => None,
        })
    }

    /// Names of the global service secrets (`sbx secret ls --global --json`:
    /// only the `name` of each `service` row is read; no value is).
    fn global_service_secrets(&self) -> Result<Vec<String>, SbxError> {
        self.secret_names(["secret", "ls", "--global", "--json"], "global")
    }

    /// Names of the service secrets scoped to this one sandbox.
    fn sandbox_service_secrets(&self, vm: &str) -> Result<Vec<String>, SbxError> {
        self.secret_names(["secret", "ls", "--sandbox", vm, "--json"], vm)
    }

    /// Only rows of the `scope` asked for count: a listing that mixes scopes
    /// (or stops saying which) must never make a global secret look shadowed.
    fn secret_names<const N: usize>(
        &self,
        arguments: [&str; N],
        scope: &str,
    ) -> Result<Vec<String>, SbxError> {
        let output = self.run_bounded_os(
            "list service secrets",
            self.preparation_timeouts.sbx,
            arguments,
        )?;
        Self::require_success("list service secrets", output.clone())?;
        let document: serde_json::Value =
            serde_json::from_slice(&output.stdout).map_err(|error| SbxError::LifecycleSchema {
                operation: "list service secrets",
                detail: error.to_string(),
            })?;
        let rows = document
            .get("secrets")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| SbxError::LifecycleSchema {
                operation: "list service secrets",
                detail: "no `secrets` list".into(),
            })?;
        service_names_in_scope(rows, scope)
    }

    /// Reconcile the dummy secrets of a running services VM (`marsh services
    /// status` calls this before it reports `ready`): the same step every
    /// ensure runs, so a global service secret added since is shadowed
    /// without waiting for the next start. Skipped while an ensure holds the
    /// VM's init lock (that ensure reconciles itself).
    ///
    /// # Errors
    /// Fails when stock cannot list or set secrets.
    pub fn reconcile_services_secrets(&self, name: &str) -> Result<(), SbxError> {
        validate_name(name)?;
        // Test break: a running VM is never reconciled.
        if marsh_contracts::break_mode(breaks::SERVICES_WARM_RECONCILE_OFF) {
            return Ok(());
        }
        let init_lock = self.services_init_lock(name);
        let Ok(_initializing) = init_lock.try_lock() else {
            return Ok(());
        };
        self.shadow_global_secrets(name)
    }

    /// SECURITY-DESIGN A12, second layer: for every global service secret,
    /// set a sandbox-scoped dummy for this VM, which shadows the global one in
    /// the credential proxy. A secret stock will not let marsh shadow (`gog`,
    /// `bamboohr`, anything unknown) is left unshadowed and reported by
    /// [`Self::services_credential_exposure`]; the engine firewall is the
    /// primary control for those. The dummy is a constant that is not a
    /// credential. Idempotent: run on every ensure, so a secret added to the
    /// host later is shadowed too.
    fn shadow_global_secrets(&self, vm: &str) -> Result<(), SbxError> {
        let global = self.global_service_secrets()?;
        let shadowed = self.sandbox_service_secrets(vm)?;
        for name in global.iter().filter(|name| !shadowed.contains(name)) {
            // A refusal is data, not an error: the name stays unshadowable.
            let _ = self.run_bounded_os(
                "shadow a service secret for a services VM",
                self.preparation_timeouts.sbx,
                [
                    "secret",
                    "set",
                    name.as_str(),
                    "--sandbox",
                    vm,
                    "--token",
                    DUMMY_SECRET,
                    "--force",
                ],
            );
        }
        Ok(())
    }

    /// Remove this VM's sandbox-scoped secrets (the dummies), best effort:
    /// they exist only because marsh set them for this one sandbox.
    /// Returns how many it removed.
    fn remove_sandbox_secrets(&self, vm: &str) -> usize {
        let Ok(names) = self.sandbox_service_secrets(vm) else {
            return 0;
        };
        let count = names.len();
        for name in names {
            let _ = self.run_bounded_os(
                "remove a services VM's dummy secret",
                self.preparation_timeouts.sbx,
                ["secret", "rm", name.as_str(), "--sandbox", vm, "--force"],
            );
        }
        count
    }

    /// Which global service secrets are shadowed for the VM and which are not,
    /// and whether stock attaches its MCP gateway (`sbx inspect`). Read-only.
    ///
    /// # Errors
    /// Fails when stock cannot list the secrets or inspect the VM.
    pub fn services_credential_exposure(&self, name: &str) -> Result<CredentialExposure, SbxError> {
        validate_name(name)?;
        let global = self.global_service_secrets()?;
        let scoped = self.sandbox_service_secrets(name)?;
        let (shadowed, unshadowable) = global
            .into_iter()
            .partition::<Vec<_>, _>(|secret| scoped.contains(secret));
        let output = self.run_bounded_os(
            "inspect services VM",
            self.preparation_timeouts.sbx,
            ["inspect", "--json", name],
        )?;
        Self::require_success("inspect services VM", output.clone())?;
        let document: serde_json::Value =
            serde_json::from_slice(&output.stdout).map_err(|error| SbxError::LifecycleSchema {
                operation: "inspect services VM",
                detail: error.to_string(),
            })?;
        Ok(CredentialExposure {
            shadowed,
            unshadowable,
            mcp_gateway: document
                .get("mcp_gateway")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })
    }

    /// Readiness check (security review L3): `sbx inspect` must list exactly
    /// one host mount, the project at its Mac path, read-write. Stock keeps a
    /// stopped VM's saved mounts, so a mount added behind marsh's back would
    /// otherwise outlive every restart. `Some(reason)` when it does not.
    ///
    /// # Errors
    /// Fails when stock cannot inspect the VM or answers something unreadable.
    pub fn services_mounts_problem(
        &self,
        name: &str,
        project: &Path,
    ) -> Result<Option<String>, SbxError> {
        validate_name(name)?;
        let output = self.run_bounded_os(
            "inspect services VM",
            self.preparation_timeouts.sbx,
            ["inspect", "--json", name],
        )?;
        Self::require_success("inspect services VM", output.clone())?;
        let document: serde_json::Value =
            serde_json::from_slice(&output.stdout).map_err(|error| SbxError::LifecycleSchema {
                operation: "inspect services VM",
                detail: error.to_string(),
            })?;
        // Test break: a mount other than the project does not fail readiness.
        if marsh_contracts::break_mode(breaks::SERVICES_EXTRA_MOUNT_OK) {
            return Ok(None);
        }
        let mounts = document
            .get("host_mounts")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| SbxError::LifecycleSchema {
                operation: "inspect services VM",
                detail: "no `host_mounts` list".into(),
            })?;
        let project = project.to_string_lossy();
        let describe = |mount: &serde_json::Value| {
            format!(
                "{} -> {}{}",
                mount
                    .get("host_path")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?"),
                mount
                    .get("container_target")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?"),
                if mount
                    .get("read_only")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true)
                {
                    " (read-only)"
                } else {
                    ""
                }
            )
        };
        let exact = |mount: &serde_json::Value| {
            mount.get("host_path").and_then(serde_json::Value::as_str) == Some(project.as_ref())
                && mount
                    .get("container_target")
                    .and_then(serde_json::Value::as_str)
                    == Some(project.as_ref())
                && mount.get("read_only").and_then(serde_json::Value::as_bool) == Some(false)
        };
        if mounts.len() == 1 && exact(&mounts[0]) {
            return Ok(None);
        }
        let seen = mounts.iter().map(describe).collect::<Vec<_>>().join(", ");
        Ok(Some(format!(
            "services VM {name} must mount exactly {project} read-write and nothing else, but stock lists: [{seen}]; remove the extra mount (`sbx umount`) or `marsh services rm`"
        )))
    }

    fn services_lease_present(&self, name: &str) -> bool {
        self.shell_leases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .is_some_and(|lease| {
                lease
                    .check_live(SUPERVISOR_RECENT, SUPERVISOR_PING_TIMEOUT)
                    .is_ok()
            })
    }

    /// Drop the lease and `sbx stop` the project's services VM, keeping the
    /// VM, its disk and its data. Returns whether a running VM was stopped.
    /// Never removes anything.
    ///
    /// # Errors
    /// Fails on a foreign name or when stock does not report it stopped.
    pub fn stop_services_vm(&self, project: &Path, deadline: Instant) -> Result<bool, SbxError> {
        let Some(name) = self.recorded_services_vm(project) else {
            return Ok(false);
        };
        self.stop_services_vm_named(&name, deadline)
    }

    /// Stop one owned services VM by its recorded name (the idle pass).
    ///
    /// # Errors
    /// As [`Self::stop_services_vm`].
    pub fn stop_services_vm_by_name(
        &self,
        name: &str,
        deadline: Instant,
    ) -> Result<bool, SbxError> {
        if self
            .ownership
            .owned_services()
            .iter()
            .all(|(owned, _)| owned != name)
        {
            return Err(SbxError::ForeignVm(name.to_owned()));
        }
        self.stop_services_vm_named(name, deadline)
    }

    fn stop_services_vm_named(&self, name: &str, deadline: Instant) -> Result<bool, SbxError> {
        validate_name(name)?;
        let init_lock = self.services_init_lock(name);
        let _initializing = init_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (transition, _cleanup) =
            weak_rw_transition_lock(&self.grant_vm_transition_locks, name.to_owned());
        let _guard = shell_admission_before(|| transition.try_write(), deadline)?;
        let (operation, _stock) =
            weak_mutex_transition_lock(&self.grant_stock_operation_locks, name.to_owned());
        let _operation = shell_admission_before(|| operation.try_lock(), deadline)?;
        let running = match self.classify_vm(name, true)? {
            VmClass::Absent => return Ok(false),
            VmClass::Foreign => return Err(SbxError::ForeignVm(name.to_owned())),
            VmClass::Owned { running, .. } => running,
        };
        self.drop_shell_lease_before(name, deadline)?;
        self.services_generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(name);
        // The mount stays saved in the stopped VM; this daemon's reference
        // ends so a stopped VM is never unmounted by an eviction.
        self.release_services_mounts(name);
        self.shell_mount_references
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(vm, _, _), _| vm != name);
        if !running {
            return Ok(false);
        }
        self.ownership.invalidate();
        let timeout = self.lifecycle_timeout(deadline, "stop services VM")?;
        self.run_bounded_os("stop services VM", timeout, ["stop", name])
            .and_then(|output| Self::require_success("stop services VM", output))?;
        match self.classify_vm(name, true)? {
            VmClass::Owned { running: false, .. } => Ok(true),
            _ => Err(SbxError::HostGrantFence(format!(
                "stock SBX still lists services VM {name} running after its stop"
            ))),
        }
    }

    /// Remove the project's services VM and its disk (`marsh services rm`).
    /// Returns whether a VM was removed.
    ///
    /// # Errors
    /// Fails when removal cannot be verified.
    pub fn remove_services_vm(&self, project: &Path, deadline: Instant) -> Result<bool, SbxError> {
        let Some(name) = self.recorded_services_vm(project) else {
            return Ok(false);
        };
        self.remove_services_vm_named(&name, deadline)
    }

    fn remove_services_vm_named(&self, name: &str, deadline: Instant) -> Result<bool, SbxError> {
        validate_name(name)?;
        let init_lock = self.services_init_lock(name);
        let _initializing = init_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (transition, _cleanup) =
            weak_rw_transition_lock(&self.grant_vm_transition_locks, name.to_owned());
        let _guard = shell_admission_before(|| transition.try_write(), deadline)?;
        let (operation, _stock) =
            weak_mutex_transition_lock(&self.grant_stock_operation_locks, name.to_owned());
        let _operation = shell_admission_before(|| operation.try_lock(), deadline)?;
        if matches!(self.classify_vm(name, true)?, VmClass::Foreign) {
            // The recorded VM is gone; a VM that is not ours holds the
            // name. Forget our record, never touch it.
            self.forget_services_state(name);
            self.ownership.forget(name)?;
            return Ok(false);
        }
        self.drop_shell_lease_before(name, deadline)?;
        self.remove_sandbox_secrets(name);
        let removed = self.remove_owned_vm(name, deadline)?;
        self.forget_services_state(name);
        Ok(removed)
    }

    fn forget_services_state(&self, name: &str) {
        self.services_generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(name);
        self.release_services_mounts(name);
        self.finish_shell_vm_recovery(name);
    }

    /// `marsh reset --include-services`: remove the services VMs this daemon's
    /// map owns whose project the user confirmed (`confirmed` holds project
    /// keys), by recorded UUID. One entry per owned VM: its name, project and
    /// the result (`Ok(None)`: kept, not confirmed).
    pub fn remove_confirmed_services_vms_before(
        &self,
        confirmed: &BTreeSet<String>,
        deadline: Instant,
    ) -> Vec<(String, String, Result<Option<bool>, SbxError>)> {
        self.ownership
            .owned_services()
            .into_iter()
            .map(|(name, key)| {
                // Test break: every services VM counts as confirmed.
                let allowed = confirmed.contains(&key)
                    || marsh_contracts::break_mode(breaks::RESET_SKIP_CONFIRMATION);
                let result = if allowed {
                    self.remove_services_vm_named(&name, deadline).map(Some)
                } else {
                    Ok(None)
                };
                (name, key, result)
            })
            .collect()
    }

    /// `marsh stop`: stop every running services VM this daemon's map owns,
    /// keeping the VM, its disk and its data. One entry per owned VM: its
    /// name, project and whether a running VM was stopped.
    pub fn stop_all_services_vms_before(
        &self,
        deadline: Instant,
    ) -> Vec<(String, String, Result<bool, SbxError>)> {
        self.stop_services_vms_except(deadline, &std::collections::BTreeSet::new())
    }

    /// As [`Self::stop_all_services_vms_before`], leaving running the VMs of
    /// the projects in `deferred` (a stop that could not be made safe): they
    /// have no entry in the result.
    pub fn stop_services_vms_except(
        &self,
        deadline: Instant,
        deferred: &std::collections::BTreeSet<String>,
    ) -> Vec<(String, String, Result<bool, SbxError>)> {
        self.ownership
            .owned_services()
            .into_iter()
            .filter(|(_, key)| !deferred.contains(key))
            .map(|(name, key)| {
                let result = self.stop_services_vm_named(&name, deadline);
                (name, key, result)
            })
            .collect()
    }

    /// Test break `services-stop-deletes-data`: `marsh stop` removes the
    /// services VMs it should only stop.
    pub fn remove_all_services_vms_before(
        &self,
        deadline: Instant,
    ) -> Vec<(String, String, Result<bool, SbxError>)> {
        self.ownership
            .owned_services()
            .into_iter()
            .map(|(name, key)| {
                let result = self.remove_services_vm_named(&name, deadline);
                (name, key, result)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn break_names_are_unique_and_off_in_a_build_without_the_feature() {
        let unique = breaks::ALL
            .iter()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), breaks::ALL.len());
        if !cfg!(feature = "test-breaks") {
            assert_eq!(breaks::active().len(), 0);
        }
    }

    #[test]
    fn secret_rows_count_only_in_the_scope_asked_for_and_fail_closed_without_one() {
        let rows = |value: serde_json::Value| value.as_array().unwrap().clone();
        let mixed = rows(serde_json::json!([
            {"scope": "global", "type": "service", "name": "github"},
            {"scope": "vm-a", "type": "service", "name": "openai"},
            {"scope": "global", "type": "other", "name": "ignored"},
            {"scope": "global", "type": "service", "name": "anthropic"},
        ]));
        assert_eq!(
            service_names_in_scope(&mixed, "global").unwrap(),
            ["anthropic", "github"]
        );
        // A listing that mixes scopes never makes a global look shadowed.
        assert_eq!(service_names_in_scope(&mixed, "vm-a").unwrap(), ["openai"]);
        assert_eq!(
            service_names_in_scope(&mixed, "vm-b").unwrap(),
            Vec::<String>::new()
        );
        for bad in [
            serde_json::json!([{"type": "service", "name": "github"}]),
            serde_json::json!([{"scope": 7, "type": "service", "name": "github"}]),
        ] {
            assert!(matches!(
                service_names_in_scope(&rows(bad), "global"),
                Err(SbxError::LifecycleSchema { .. })
            ));
        }
    }

    #[test]
    fn disk_sizes_are_whole_gibibytes() {
        for good in ["1g", "40g", "40G", "4096g"] {
            assert!(valid_disk_size(good), "{good}");
        }
        for bad in [
            "", "g", "0g", "4097g", "40", "40m", "40gb", "-4g", "4.5g", " 40g",
        ] {
            assert!(!valid_disk_size(bad), "{bad:?}");
        }
    }
}
