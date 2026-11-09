//! The daemon's relays to services VMs (E1): one long-lived
//! `sbx exec -i -u root VM docker exec -i ENGINE_ID /marsh/marsh
//! --internal-services relay` per ready services VM, bound to the engine
//! generation it was started for (SECURITY-DESIGN A1, A2, A7, A8).
//!
//! The relay starts only after the engine reported `ready{generation}`
//! ([`EngineHandle::wait_ready`]) and addresses the engine by the container id
//! recorded in that generation, never by name. Losing the exec, the engine or
//! its readiness closes every channel and revokes the relay; the next
//! generation gets a new engine container and a new relay, and nothing from the
//! old one is reused.
//!
//! **Daemon restart (VM-06).** Relays are re-established *on first use*, after
//! a recovery pass: the VMs the new daemon finds in its ownership map are
//! marked unrecovered at startup (their previous relay and engine generation
//! died with the old daemon), and the pass (the first services timer tick)
//! brings each enabled one up again with a fresh engine generation and a new
//! relay. A project that is still unrecovered refuses channels (fail closed).
//! `startup-recovery-off` skips the pass, so channels stay refused.

use crate::services_engine::{
    EngineArtifacts, EngineHandle, EngineSettings, EngineState, ServicesEngine, ServicesVmRef,
};
use crate::services_vm::{ReadyServicesVm, ServicesVmSpec};
use crate::vm_text::{CappedLines, DISPLAY_LINE_BYTES, plain_text};
use crate::{InternetGrants, StockSbx};
use marsh_runtime::Invocation;
use marsh_services::engine::Generation;
use marsh_services::host::RelayConfig;
pub use marsh_services::host::{
    AttemptScope, BASE_WORKSPACE, CaptureReply, DaemonBudget, Limits, PortsSubscription, Relay,
    RelayError, SnapshotEntry, Stats, Stream,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const STDERR_TAIL_LINES: usize = 20;

/// Modes the agent (inside the engine container) checks: the daemon hands the
/// active ones to it through the dedicated `--break-modes` argument of a test
/// build (DECISIONS 22). Host-only modes (`relay-*`, `tcp-map-check-off`,
/// `vm-frame-validation-off`, `channel-limit-off`, `slow-relay`, the faults)
/// are not listed.
#[cfg(feature = "test-breaks")]
const AGENT_MODES: &[&str] = &[
    "demo",
    "anon-volume-unlabeled",
    "bind-no-pin",
    "bind-nonrecursive-allowed",
    "bind-pin-leak",
    "bind-repin-unchecked",
    "capture-all-workspaces",
    "capture-allow-overlap",
    "capture-api-gate-off",
    "capture-copy",
    "capture-ignore-revoked",
    "capture-no-budget",
    "capture-no-pause",
    "capture-skip-unpause-on-error",
    "capture-skip-volumes",
    "commit-any-repo",
    "copy-no-xdev",
    "fault-capture-fail",
    "fault-capture-fail-after-pause",
    "fault-capture-hang",
    "fault-pool-unreadable",
    "fault-copy-stall",
    "fault-pin-orphan",
    "fault-restore-bad-file-name",
    "fault-restore-bad-name",
    "fault-restore-bad-port",
    "fault-restore-huge-file",
    "fault-restore-markup",
    "fault-restore-nul",
    "fault-restore-skip-volumes",
    "filter-case-sensitive",
    "filter-forward-original-bytes",
    "filter-lenient-json",
    "filter-no-resolve",
    "filter-no-upgrade",
    "filter-off",
    "filter-raw-after-upgrade-header",
    "freeze-no-wait",
    "fullid-name-allowed",
    "hardlink-racy",
    "hijack-deadlines-off",
    "hold-capture-before-publish",
    "hold-capture-frozen",
    "hold-pin-before-ledger",
    "hold-restore-before-install",
    "hold-restore-before-up",
    "hold-restore-client-running",
    "hold-restore-volumes-created",
    "lane-authority-off",
    "ledger-persist-ignore",
    "list-fail-open",
    "locks-process-local",
    "manifest-dangling-ref",
    "names-by-display",
    "network-mtu-allowed",
    "network-name-forwarded",
    "output-unbounded",
    "outputs-open",
    "pin-allow-xdev",
    "pin-file-leak",
    "pin-inherit-off",
    "ports-list-all",
    "private-propagation-allowed",
    "project-cap-per-root",
    "protected-by-name",
    "protected-identity-off",
    "protected-marsh-late-off",
    "protected-subpin-off",
    "publish-nonloopback",
    "ready-unchecked",
    "reap-all-scope",
    "reap-persistent",
    "recovery-discard-journal",
    "reserved-labels-allowed",
    "restart-update-allowed",
    "restore-client-unconfined",
    "restore-copy",
    "restore-fail-open",
    "restore-files-from-agent",
    "restore-from-live",
    "restore-ignore-blockers",
    "restore-no-env",
    "restore-no-override",
    "restore-raw-engine",
    "restore-same-project",
    "restore-skip-volumes",
    "restore-unfiltered",
    "scope-default-network-off",
    "scope-reference-check-off",
    "services-resource-limit-off",
    "slow-capture",
    "slow-freeze",
    "slow-pin-record",
    "slow-restore",
    "space-check-stale",
    "snapshot-name-from-label",
    "snapshot-publish-early",
    "socket-refuse",
    "socket-substitute-raw",
    "sweep-pins-by-source-age",
    "unpause-recovery-off",
    "unreadable-pool-ok",
    "update-limits-unmerged",
    "volume-claim-racy",
    "watchdog-discard-journal",
];

/// How a quarantined project is cleared, said the same way everywhere (the
/// error, the log, `marsh services status`). `marsh services start` is not a
/// retry of the relay: a relay ended by what the VM sent is not trusted again
/// on its engine, so start makes a new engine generation, and that stops every
/// running container of the project.
pub const QUARANTINE_RECOVERY: &str = "`marsh services start` clears it, but it starts a new engine generation, which stops every running container of this project (its volumes and stopped containers stay)";

/// Reads the relay exec's stderr, which is the VM's, with the same bounded,
/// plain-text reader the engine watcher uses (`vm_text`): at most
/// `DISPLAY_LINE_BYTES` of a line are kept and the rest is discarded as it is
/// read (a line with no newline never grows memory), the last
/// `STDERR_TAIL_LINES` lines are kept, and control and escape characters are
/// removed before a line is stored.
pub fn drain_stderr(stderr: impl Read, tail: &Mutex<VecDeque<String>>) {
    for line in CappedLines::new(BufReader::new(stderr), DISPLAY_LINE_BYTES) {
        let text = plain_text(&line, DISPLAY_LINE_BYTES);
        let mut lines = tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lines.len() == STDERR_TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(text);
    }
}

/// Host break modes of this task, for `docs/design/test-breaks.md` and the
/// daemon's known-modes list.
pub mod breaks {
    pub const RELAY_DROP_CHUNK: &str = "relay-drop-chunk";
    pub const SLOW_RELAY: &str = "slow-relay";
    pub const STARTUP_RECOVERY_OFF: &str = "startup-recovery-off";
    pub const RELAY_ACCEPT_STALE_GENERATION: &str = "relay-accept-stale-generation";
    /// The test plan's name for the same defect (the one above is the alias).
    pub const READY_ACCEPT_STALE_GENERATION: &str = "ready-accept-stale-generation";
    pub const RELAY_UNBOUNDED: &str = "relay-unbounded";
    pub const CHANNEL_LIMIT_OFF: &str = "channel-limit-off";
    pub const RELAY_FAIRNESS_OFF: &str = "relay-fairness-off";
    pub const TCP_MAP_CHECK_OFF: &str = "tcp-map-check-off";
    pub const VM_FRAME_VALIDATION_OFF: &str = "vm-frame-validation-off";
    pub const FAULT_FORGE_TCP: &str = "fault-forge-tcp";
    pub const FAULT_AGENT_OVERRIDE: &str = "fault-agent-override";
    pub const ALL: [&str; 12] = [
        RELAY_DROP_CHUNK,
        SLOW_RELAY,
        STARTUP_RECOVERY_OFF,
        RELAY_ACCEPT_STALE_GENERATION,
        READY_ACCEPT_STALE_GENERATION,
        RELAY_UNBOUNDED,
        CHANNEL_LIMIT_OFF,
        RELAY_FAIRNESS_OFF,
        TCP_MAP_CHECK_OFF,
        VM_FRAME_VALIDATION_OFF,
        FAULT_FORGE_TCP,
        FAULT_AGENT_OVERRIDE,
    ];

    /// The active host modes of this task (empty without `test-breaks`).
    #[must_use]
    pub fn active() -> Vec<&'static str> {
        ALL.iter()
            .copied()
            .filter(|mode| marsh_contracts::break_mode(mode))
            .collect()
    }
}

/// What a relay start needs besides the engine's generation.
#[derive(Clone)]
pub struct RelayOptions {
    pub limits: Limits,
    pub budget: Arc<DaemonBudget>,
    /// `$MARSH_HOME/test-holds`: where the test faults look for their inputs
    /// (`agent-override`, `forge-tcp-port`). Read only under the fault.
    pub test_holds: Option<PathBuf>,
}

impl ServicesEngine {
    /// Starts the relay for a ready generation: the agent is launched through
    /// `sbx exec` and `docker exec` **by the generation's engine container id**.
    ///
    /// # Errors
    /// `Unavailable` when the exec cannot start, the agent refuses, or it
    /// does not report the expected generation.
    #[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
    pub fn start_relay(
        &self,
        vm: &ServicesVmRef,
        generation: &Generation,
        options: RelayOptions,
    ) -> Result<Relay, RelayError> {
        let id = generation.engine_id.as_str();
        if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(RelayError::Unavailable(
                "the engine container id is not a container id".into(),
            ));
        }
        let mut arguments: Vec<OsString> = ["exec", "-i", "-u", "root", vm.name.as_str()]
            .iter()
            .map(OsString::from)
            .collect();
        let overridden = self.peer_override(vm, &options);
        if let Some(peer) = &overridden {
            // Test fault: a harness-supplied peer stands in for the agent.
            arguments.extend(["python3", peer].map(OsString::from));
        } else {
            arguments.extend(
                [
                    "docker",
                    "exec",
                    "-i",
                    id,
                    marsh_services::engine::MARSH_BINARY,
                    "--internal-services",
                    "relay",
                ]
                .map(OsString::from),
            );
            #[cfg(feature = "test-breaks")]
            {
                let modes: Vec<&str> = AGENT_MODES
                    .iter()
                    .copied()
                    .filter(|mode| marsh_contracts::break_mode(mode))
                    .collect();
                if !modes.is_empty() {
                    arguments.push(OsString::from("--break-modes"));
                    arguments.push(OsString::from(modes.join(",")));
                }
            }
        }
        let attachment = self
            .runner
            .spawn_attached(&Invocation {
                program: self.sbx.clone(),
                arguments,
                working_directory: None,
                environment: Vec::new(),
            })
            .map_err(|error| RelayError::Unavailable(format!("sbx exec the relay: {error}")))?;
        let process = Arc::new(Mutex::new(Some(attachment.process)));
        let tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
        {
            let tail = Arc::clone(&tail);
            let stderr = attachment.stderr;
            std::thread::spawn(move || drain_stderr(stderr, &tail));
        }
        let closer = {
            let process = Arc::clone(&process);
            Box::new(move || {
                // Ends the exec: the agent sees end of stream and exits. The
                // process is reaped off the calling thread.
                let taken = process
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                if let Some(mut taken) = taken {
                    std::thread::spawn(move || {
                        let _ = taken.terminate();
                        let _ = taken.wait();
                    });
                }
            }) as Box<dyn Fn() + Send + Sync>
        };
        let forge = self.forge_port(&options);
        let result = Relay::start(
            attachment.stdout,
            attachment.stdin,
            closer,
            RelayConfig {
                project: vm.project_root.clone(),
                expected: generation.clone(),
                limits: options.limits,
                budget: options.budget,
                forge_tcp_port: forge,
            },
        );
        if let Err(error) = result {
            let stderr = tail
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(" | ");
            let say = |why: &str| {
                if stderr.is_empty() {
                    why.to_owned()
                } else {
                    format!("{why}: {stderr}")
                }
            };
            return Err(match &error {
                RelayError::Hostile(why) => RelayError::Hostile(say(why)),
                RelayError::Unavailable(why) => RelayError::Unavailable(say(why)),
                other => RelayError::Unavailable(say(&other.to_string())),
            });
        }
        result
    }

    #[cfg(feature = "test-breaks")]
    #[allow(clippy::unused_self)]
    fn peer_override(&self, vm: &ServicesVmRef, options: &RelayOptions) -> Option<String> {
        if !marsh_contracts::break_mode(breaks::FAULT_AGENT_OVERRIDE) {
            return None;
        }
        let file = options.test_holds.as_ref()?.join("agent-override");
        let named = std::fs::read_to_string(file).ok()?;
        (Path::new(named.trim()) == vm.project_root).then(|| "/run/nfx/peer.py".to_owned())
    }

    #[cfg(not(feature = "test-breaks"))]
    #[allow(clippy::unused_self)]
    fn peer_override(&self, _vm: &ServicesVmRef, _options: &RelayOptions) -> Option<String> {
        None
    }

    #[cfg(feature = "test-breaks")]
    #[allow(clippy::unused_self)]
    fn forge_port(&self, options: &RelayOptions) -> Option<u16> {
        if !marsh_contracts::break_mode(breaks::FAULT_FORGE_TCP) {
            return None;
        }
        std::fs::read_to_string(options.test_holds.as_ref()?.join("forge-tcp-port"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    #[cfg(not(feature = "test-breaks"))]
    #[allow(clippy::unused_self)]
    fn forge_port(&self, _options: &RelayOptions) -> Option<u16> {
        None
    }
}

/// One project's live engine generation and relay.
pub struct RelayEntry {
    handle: Arc<EngineHandle>,
    /// The relay the engine's single observer closes when the engine leaves
    /// `Ready`: a replacement relay on the same generation takes its place
    /// here instead of adding another observer.
    current: Arc<Mutex<Option<Relay>>>,
    relay: Relay,
    generation: Generation,
    vm: String,
}

impl RelayEntry {
    #[must_use]
    pub fn relay(&self) -> &Relay {
        &self.relay
    }

    #[must_use]
    pub fn generation(&self) -> &Generation {
        &self.generation
    }

    #[must_use]
    pub fn vm(&self) -> &str {
        &self.vm
    }

    #[must_use]
    pub fn engine_state(&self) -> EngineState {
        self.handle.state()
    }
}

/// What `marsh services status` reports about a project's relay.
#[derive(Clone, Debug)]
pub struct RelayView {
    /// `none`, `unrecovered`, `ready`, `closed` or `quarantined`.
    pub state: &'static str,
    pub reason: Option<String>,
    pub generation: Option<Generation>,
    pub channels: usize,
    pub agent_pid: Option<u32>,
    /// The test break modes the agent reported in its `ready` (empty in a
    /// release build).
    pub agent_breaks: Vec<String>,
}

/// Every project's relay in this daemon.
pub struct ServicesRelays {
    stock: Arc<StockSbx>,
    artifacts: Option<PathBuf>,
    settings: Result<EngineSettings, String>,
    options: RelayOptions,
    entries: Mutex<BTreeMap<PathBuf, Arc<RelayEntry>>>,
    locks: Mutex<BTreeMap<PathBuf, Arc<Mutex<()>>>>,
    unrecovered: Mutex<BTreeSet<PathBuf>>,
    /// Projects whose relay was refused during its handshake because of what
    /// the VM sent (no entry exists for them): quarantined all the same.
    refused: Mutex<BTreeMap<PathBuf, String>>,
}

impl ServicesRelays {
    /// `artifacts` is the guest artifact directory (`libexec/marsh`: the
    /// engine tar and pin and the Linux `marsh`). A daemon without them
    /// refuses to start a relay and says why.
    #[must_use]
    pub fn new(
        stock: Arc<StockSbx>,
        artifacts: Option<PathBuf>,
        budget: Arc<DaemonBudget>,
        test_holds: Option<PathBuf>,
    ) -> Self {
        Self {
            stock,
            artifacts,
            settings: EngineSettings::from_env().map_err(|error| error.to_string()),
            options: RelayOptions {
                limits: Limits::default(),
                budget,
                test_holds,
            },
            entries: Mutex::new(BTreeMap::new()),
            locks: Mutex::new(BTreeMap::new()),
            unrecovered: Mutex::new(BTreeSet::new()),
            refused: Mutex::new(BTreeMap::new()),
        }
    }

    /// Whether this daemon has the guest artifacts a services engine needs
    /// (`EngineArtifacts::present_in`): what `services-v1` stands for.
    #[must_use]
    pub fn can_run_services(&self) -> bool {
        self.artifacts
            .as_deref()
            .is_some_and(crate::services_engine::EngineArtifacts::present_in)
    }

    /// Replaces the channel limits (settings; the defaults are A7's).
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.options.limits = limits;
        self
    }

    fn lock_for(&self, root: &Path) -> Arc<Mutex<()>> {
        Arc::clone(
            self.locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(root.to_owned())
                .or_default(),
        )
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, BTreeMap<PathBuf, Arc<RelayEntry>>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn unrecovered(&self) -> std::sync::MutexGuard<'_, BTreeSet<PathBuf>> {
        self.unrecovered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Marks projects found at daemon start whose previous relay and engine
    /// generation are gone with the previous daemon.
    pub fn mark_unrecovered(&self, roots: impl IntoIterator<Item = PathBuf>) {
        self.unrecovered().extend(roots);
    }

    #[must_use]
    pub fn unrecovered_roots(&self) -> Vec<PathBuf> {
        self.unrecovered().iter().cloned().collect()
    }

    /// Ends the unrecovered mark: recovery was attempted (or the VM was not
    /// running, so there was nothing to recover).
    pub fn clear_unrecovered(&self, root: &Path) {
        self.unrecovered().remove(root);
    }

    #[must_use]
    pub fn is_unrecovered(&self, root: &Path) -> bool {
        self.unrecovered().contains(root)
    }

    /// Why the project is quarantined: its relay ended because of what the VM
    /// sent. No relay is started again until an explicit start.
    #[must_use]
    pub fn quarantine(&self, root: &Path) -> Option<String> {
        if let Some(reason) = self
            .refused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(root)
        {
            return Some(reason.clone());
        }
        let entry = self.entries().get(root).cloned()?;
        entry
            .relay
            .violated()
            .then(|| entry.relay.closed())
            .flatten()
    }

    /// The project's live relay. Refuses (with the reason) while the project
    /// is unrecovered, has no relay, or its relay or engine generation ended.
    ///
    /// # Errors
    /// `Unavailable` with the reason.
    pub fn get(&self, root: &Path) -> Result<Arc<RelayEntry>, RelayError> {
        if self.is_unrecovered(root) {
            return Err(RelayError::Unavailable(
                "the relay has not been recovered since the daemon started".into(),
            ));
        }
        let entry = self.entries().get(root).cloned();
        match entry {
            None => Err(RelayError::Unavailable(
                "no relay for this project; services are not started".into(),
            )),
            Some(entry) => {
                if let Some(reason) = entry.relay.closed() {
                    return Err(RelayError::Unavailable(reason));
                }
                match entry.handle.state() {
                    EngineState::Ready(generation) if generation == entry.generation => Ok(entry),
                    other => Err(RelayError::Unavailable(format!(
                        "the engine is {}",
                        other.label()
                    ))),
                }
            }
        }
    }

    /// Brings the project's VM (S1), its engine generation (S2) and a relay
    /// (E1) up, unless a live relay for the current generation exists.
    ///
    /// # Errors
    /// The reason any step failed; nothing is left half open.
    pub fn ensure(
        &self,
        spec: &ServicesVmSpec,
        grants: &InternetGrants,
    ) -> Result<Arc<RelayEntry>, String> {
        let ready = self
            .stock
            .ensure_services_vm(spec)
            .map_err(|error| format!("marsh: {error}"))?;
        self.establish(spec, &ready, grants)
    }

    /// The engine generation and relay for a VM S1 already made ready.
    ///
    /// # Errors
    /// The reason any step failed; nothing is left half open.
    pub fn establish(
        &self,
        spec: &ServicesVmSpec,
        ready: &ReadyServicesVm,
        grants: &InternetGrants,
    ) -> Result<Arc<RelayEntry>, String> {
        let root = spec.project.as_path();
        let lock = self.lock_for(root);
        let _operation = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.unrecovered().contains(root)
            && marsh_contracts::break_mode(breaks::STARTUP_RECOVERY_OFF)
        {
            // Defect: no recovery after a restart, so the relay is never
            // brought back for a project found at startup.
            return Err("marsh: the relay has not been recovered since the daemon started".into());
        }
        if let Ok(entry) = self.get(root) {
            return Ok(entry);
        }
        let engine = self.stock.services_engine();
        let vm = ServicesVmRef {
            name: ready.name.clone(),
            project_root: root.to_owned(),
        };
        // A relay that ended for a transport reason, with the engine still
        // ready in the same generation, is replaced by a new relay for that
        // generation: the user's containers keep running. A relay that ended
        // because of what the VM sent (quarantine), or an engine that is not
        // ready, gets a whole new generation.
        let old = self.entries().get(root).cloned();
        if let Some(old) = old.clone()
            && !old.relay.violated()
            && let EngineState::Ready(generation) = old.handle.state()
            && generation == old.generation
        {
            let entry = self.bind(
                root,
                &engine,
                &vm,
                Arc::clone(&old.handle),
                generation,
                Some(Arc::clone(&old.current)),
            )?;
            self.unrecovered().remove(root);
            return Ok(entry);
        }
        let refused = self
            .refused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(root);
        let quarantined = refused.or_else(|| {
            old.as_ref()
                .filter(|old| old.relay.violated())
                .map(|old| old.relay.closed().unwrap_or_default())
        });
        if let Some(reason) = quarantined {
            eprintln!(
                "marshd: services relay of {} was quarantined ({reason}); starting again makes a new engine generation, which stops every running container of this project",
                root.display(),
            );
        }
        self.close_entry(root, "replaced by a new generation");
        let directory = self
            .artifacts
            .as_ref()
            .ok_or("marsh: this daemon has no guest artifacts for the services engine")?;
        // The recorded Internet grant opens the engine's gate at every start:
        // a first start, a restart and a recovery (DECISIONS 24).
        let settings = self.settings.clone()?.for_project(grants, root);
        let artifacts =
            EngineArtifacts::from_guest_dir(directory).map_err(|error| error.to_string())?;
        let handle = Arc::new(engine.start(vm.clone(), artifacts, settings.clone()));
        let generation = handle
            .wait_ready(settings.ready_timeout + Duration::from_secs(30))
            .inspect_err(|_| handle.detach())?;
        let entry = self.bind(root, &engine, &vm, handle, generation, None)?;
        self.unrecovered().remove(root);
        Ok(entry)
    }

    /// Starts a relay for a ready generation and records it.
    fn bind(
        &self,
        root: &Path,
        engine: &ServicesEngine,
        vm: &ServicesVmRef,
        handle: Arc<EngineHandle>,
        generation: Generation,
        reuse: Option<Arc<Mutex<Option<Relay>>>>,
    ) -> Result<Arc<RelayEntry>, String> {
        let relay = match engine.start_relay(vm, &generation, self.options.clone()) {
            Ok(relay) => relay,
            Err(error) => {
                if let RelayError::Hostile(reason) = &error {
                    self.refused
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(root.to_owned(), reason.clone());
                }
                handle.detach();
                return Err(error.to_string());
            }
        };
        // The engine leaving `Ready` revokes the relay (A1). One observer per
        // engine handle closes whichever relay is current; it is registered
        // after the relay exists and the state is re-read, so a loss in
        // between is not missed.
        let current = if let Some(current) = reuse {
            *current
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(relay.clone());
            current
        } else {
            let current = Arc::new(Mutex::new(Some(relay.clone())));
            let observed = Arc::clone(&current);
            handle.subscribe(move |transition| {
                if let EngineState::Unavailable(reason) = &transition.state
                    && let Some(relay) = observed
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone()
                {
                    relay.close(&format!("the engine is no longer ready: {reason}"));
                }
            });
            current
        };
        if let EngineState::Unavailable(reason) = handle.state() {
            relay.close(&format!("the engine is no longer ready: {reason}"));
        }
        let entry = Arc::new(RelayEntry {
            handle,
            current,
            relay,
            generation,
            vm: vm.name.clone(),
        });
        self.entries().insert(root.to_owned(), Arc::clone(&entry));
        Ok(entry)
    }

    fn close_entry(&self, root: &Path, reason: &str) {
        let entry = self.entries().remove(root);
        if let Some(entry) = entry {
            entry.relay.close(reason);
            entry.handle.detach();
        }
    }

    /// Closes the project's relay and every channel on it (the VM is being
    /// stopped or removed, or services were turned off).
    pub fn close(&self, root: &Path, reason: &str) {
        let lock = self.lock_for(root);
        let _operation = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.refused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(root);
        self.close_entry(root, reason);
    }

    /// Closes every relay (the daemon is going away).
    pub fn close_all(&self, reason: &str) {
        let roots: Vec<PathBuf> = self.entries().keys().cloned().collect();
        for root in roots {
            self.close(&root, reason);
        }
    }

    #[must_use]
    pub fn view(&self, root: &Path) -> RelayView {
        if self.is_unrecovered(root) {
            return RelayView {
                state: "unrecovered",
                reason: Some("the relay has not been recovered since the daemon started".into()),
                generation: None,
                channels: 0,
                agent_pid: None,
                agent_breaks: Vec::new(),
            };
        }
        let entry = self.entries().get(root).cloned();
        let refused = self
            .refused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(root)
            .cloned();
        match entry {
            None | Some(_) if refused.is_some() => RelayView {
                state: "quarantined",
                reason: refused,
                generation: None,
                channels: 0,
                agent_pid: None,
                agent_breaks: Vec::new(),
            },
            None => RelayView {
                state: "none",
                reason: None,
                generation: None,
                channels: 0,
                agent_pid: None,
                agent_breaks: Vec::new(),
            },
            Some(entry) => match entry.relay.closed() {
                Some(reason) => RelayView {
                    state: if entry.relay.violated() {
                        "quarantined"
                    } else {
                        "closed"
                    },
                    reason: Some(reason),
                    generation: Some(entry.generation.clone()),
                    channels: 0,
                    agent_pid: entry.relay.agent_pid(),
                    agent_breaks: entry.relay.agent_breaks(),
                },
                None => RelayView {
                    state: "ready",
                    reason: None,
                    generation: Some(entry.generation.clone()),
                    channels: entry.relay.open_channels(),
                    agent_pid: entry.relay.agent_pid(),
                    agent_breaks: entry.relay.agent_breaks(),
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Eight MiB with no newline, then an escape-laden line: the tail keeps one
    /// bounded, plain line each and nothing grows.
    #[test]
    fn stderr_is_bounded_and_plain_text() {
        struct Flood {
            left: usize,
            tail: Vec<u8>,
        }
        impl Read for Flood {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if self.left > 0 {
                    let n = buffer.len().min(self.left);
                    buffer[..n].fill(b'x');
                    self.left -= n;
                    Ok(n)
                } else if self.tail.is_empty() {
                    Ok(0)
                } else {
                    let n = buffer.len().min(self.tail.len());
                    buffer[..n].copy_from_slice(&self.tail[..n]);
                    self.tail.drain(..n);
                    Ok(n)
                }
            }
        }
        let tail = Mutex::new(VecDeque::new());
        drain_stderr(
            Flood {
                left: 8 << 20,
                tail: b"\n\x1b[31mred\x1b[0m\ttab\nlast".to_vec(),
            },
            &tail,
        );
        let lines: Vec<String> = tail.lock().unwrap().iter().cloned().collect();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].len() <= DISPLAY_LINE_BYTES, "{}", lines[0].len());
        assert_eq!(lines[1], "[31mred[0mtab");
        assert!(lines.iter().all(|l| !l.chars().any(char::is_control)));
    }
}
