//! Daemon-side lifecycle of `marsh-svc-engine` in a services VM
//! (SECURITY-DESIGN A1, ENG-PLAN D2).
//!
//! The services VM's stock ("boot") Docker engine never holds user objects. It
//! holds exactly one container, `marsh-svc-engine`, started from an image that
//! ships in marsh's guest artifacts, is loaded offline with `docker load`,
//! checked against the pinned digest, and run **by image id**, never by tag and
//! never pulled. The container's entrypoint (`marsh --internal-services
//! engine-up`) mounts the pool, starts the production engine and reports
//! `ready{generation}` on stdout, which this module reads through
//! `docker logs -f`.
//!
//! Readiness (A1) belongs to one generation: the VM boot id plus the engine
//! container id. The state goes `Starting -> Ready` only on a `ready` that
//! matches the generation this module started, and any loss of the container,
//! the log stream (the exec) or the pool goes to `Unavailable`. Observers see
//! every transition; a channel owner closes its channels on any transition away
//! from `Ready`. A restart of the VM or of the engine container is a new
//! `start`, so a new generation, and nothing is usable until its `ready`.
//!
//! S1 provides `ensure_services_vm`; until it merges this module takes a
//! minimal [`ServicesVmRef`] and expects the caller to have verified that the VM
//! is in the ownership map with its recorded UUID.

use crate::StockSbx;
use crate::vm_text::{CappedLines, DISPLAY_LINE_BYTES, plain_text};
use marsh_runtime::{CommandOutput, CommandRunner, Invocation};
use marsh_services::engine::{
    ENGINE_CONTAINER, EngineEvent, Generation, MARSH_BINARY, POOL_DIR as ENGINE_POOL_DIR,
};
use marsh_services::firewall::{self, FirewallReport};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// Root of marsh's services state on the VM's Docker disk.
pub const VM_STATE_DIR: &str = "/var/lib/docker/marsh-svc";
const VM_ARTIFACT_DIR: &str = "/var/lib/docker/marsh-svc/artifacts";
const VM_POOL_DIR: &str = "/var/lib/docker/marsh-svc/pool";
const VM_ARCHIVE: &str = "/var/lib/docker/marsh-svc/artifacts/svc-engine.tar";
const VM_MARSH: &str = "/var/lib/docker/marsh-svc/artifacts/marsh";
const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";
/// Default pool size when no setting is given.
pub const DEFAULT_POOL_BYTES: u64 = 24 << 30;
const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(180);
const EXEC_TIMEOUT: Duration = Duration::from_secs(60);
const COPY_TIMEOUT: Duration = Duration::from_secs(300);
const LOAD_TIMEOUT: Duration = Duration::from_secs(180);
const STDERR_TAIL_LINES: usize = 20;
/// The longest engine stdout line that is read whole (the `ready` report is a
/// few KiB at most).
const EVENT_LINE_BYTES: usize = 64 * 1024;
/// The longest reason taken from the VM that is kept.
const REASON_BYTES: usize = 2048;
const LOCAL_TAG: &str = "marsh-svc-engine:latest";

/// A services VM that the caller has already admitted (S1: ownership map and
/// UUID verified).
#[derive(Clone, Debug)]
pub struct ServicesVmRef {
    pub name: String,
    /// The project root, mounted read-write at this Mac path in the VM.
    pub project_root: PathBuf,
}

/// The pinned engine image, as written by `scripts/prepare-svc-engine-image.py`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct EnginePin {
    pub schema: String,
    pub platform: String,
    /// Manifest digest: what the boot engine reports as the image id.
    pub image_id: String,
    pub config_id: String,
    pub archive: String,
    pub archive_sha256: String,
    pub archive_bytes: u64,
}

const PIN_SCHEMA: &str = "marsh.svc-engine-image/v1";

/// The guest artifacts the engine needs.
#[derive(Clone, Debug)]
pub struct EngineArtifacts {
    pub pin: EnginePin,
    pub archive: PathBuf,
    pub marsh: PathBuf,
    pub marsh_sha256: String,
}

impl EngineArtifacts {
    /// Whether the artifacts are in a guest artifact directory: a readable
    /// pin of the supported schema, the archive it names and the Linux
    /// `marsh`. Cheap (no hashing: `from_guest_dir` verifies before use); what
    /// decides whether this build can run services at all.
    #[must_use]
    pub fn present_in(directory: &Path) -> bool {
        let Ok(bytes) = std::fs::read(directory.join("svc-engine-image.json")) else {
            return false;
        };
        let Ok(pin) = serde_json::from_slice::<EnginePin>(&bytes) else {
            return false;
        };
        pin.schema == PIN_SCHEMA
            && !pin.archive.is_empty()
            && !pin.archive.contains('/')
            && directory.join(&pin.archive).is_file()
            && directory.join("marsh-linux-arm64").is_file()
    }

    /// Read and verify the artifacts in a guest artifact directory
    /// (`libexec/marsh`): the pin, the archive against the pin's size and
    /// sha256, and the Linux marsh binary.
    ///
    /// # Errors
    /// A malformed pin, a missing file, or an archive that does not match.
    pub fn from_guest_dir(directory: &Path) -> Result<Self, EngineError> {
        let pin_path = directory.join("svc-engine-image.json");
        let pin: EnginePin =
            serde_json::from_slice(&std::fs::read(&pin_path).map_err(|error| {
                EngineError::Artifact(format!("{}: {error}", pin_path.display()))
            })?)
            .map_err(|error| EngineError::Artifact(format!("{}: {error}", pin_path.display())))?;
        if pin.schema != PIN_SCHEMA || pin.platform != "linux/arm64" {
            return Err(EngineError::Artifact(format!(
                "{}: unsupported pin {} {}",
                pin_path.display(),
                pin.schema,
                pin.platform
            )));
        }
        for digest in [&pin.image_id, &pin.config_id] {
            if !is_digest(digest) {
                return Err(EngineError::Artifact(format!("malformed digest {digest}")));
            }
        }
        if pin.archive.contains('/') || pin.archive.is_empty() {
            return Err(EngineError::Artifact("pin names a path, not a file".into()));
        }
        let archive = directory.join(&pin.archive);
        let (actual, size) = sha256_file(&archive)?;
        if size != pin.archive_bytes || actual != pin.archive_sha256 {
            return Err(EngineError::Artifact(format!(
                "{} does not match its pin (sha256 {actual}, pinned {})",
                archive.display(),
                pin.archive_sha256
            )));
        }
        let marsh = directory.join("marsh-linux-arm64");
        let (marsh_sha256, _) = sha256_file(&marsh)?;
        Ok(Self {
            pin,
            archive,
            marsh,
            marsh_sha256,
        })
    }
}

fn is_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

fn sha256_file(path: &Path) -> Result<(String, u64), EngineError> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| EngineError::Artifact(format!("{}: {error}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    let mut total = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| EngineError::Artifact(format!("{}: {error}", path.display())))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    Ok((hex(&hasher.finalize()), total))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("marsh: services engine artifact: {0}")]
    Artifact(String),
    #[error("marsh: services engine: {0}")]
    Setup(String),
}

/// Parse `24g`, `512m`, `1t` or a byte count.
///
/// # Errors
/// A message for anything else.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let text = text.trim().to_ascii_lowercase();
    let (digits, shift) = match text.chars().last() {
        Some('k') => (&text[..text.len() - 1], 10),
        Some('m') => (&text[..text.len() - 1], 20),
        Some('g') => (&text[..text.len() - 1], 30),
        Some('t') => (&text[..text.len() - 1], 40),
        _ => (text.as_str(), 0),
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|value| value.checked_mul(1 << shift))
        .ok_or_else(|| format!("invalid size {text:?}"))
}

/// Break modes the daemon-side lifecycle knows, by canonical name. Aliases
/// (deprecated names) map to the canonical one.
const BREAK_MODES: [(&str, &str); 14] = [
    ("ready-before-pool", "ready-before-pool"),
    ("boot-barrier-off", "ready-before-pool"),
    ("endpoint-before-pool", "ready-before-pool"),
    ("bootstrap-use-user-image", "bootstrap-use-user-image"),
    ("relay-dial-boot-engine", "relay-dial-boot-engine"),
    ("pool-skip-on-start", "pool-skip-on-start"),
    ("entitlements-on", "entitlements-on"),
    ("pool-no-size-check", "pool-no-size-check"),
    ("slow-pool", "slow-pool"),
    ("hold-engine-before-ready", "hold-engine-before-ready"),
    ("engine-firewall-off", "engine-firewall-off"),
    ("engine-ip-any", "engine-ip-any"),
    ("engine-cgroup-no-delegate", "engine-cgroup-no-delegate"),
    ("engine-watch-unbounded", "engine-watch-unbounded"),
];

/// The modes selected for this daemon, from [`marsh_contracts::break_mode`]
/// only (H2: the daemon's own launch environment, never job or project input).
/// Always empty in a build without `test-breaks`.
#[must_use]
pub fn active_breaks() -> Vec<String> {
    let mut modes: Vec<String> = Vec::new();
    for (name, canonical) in BREAK_MODES {
        if marsh_contracts::break_mode(name) && !modes.iter().any(|mode| mode == canonical) {
            modes.push(canonical.to_owned());
        }
    }
    modes
}

/// Engine settings. The daemon reads them at start.
#[derive(Clone, Debug)]
pub struct EngineSettings {
    pub pool_bytes: u64,
    pub ready_timeout: Duration,
    /// The project's Internet grant (`marsh services internet on`), passed to
    /// `engine-up` as `--internet on|off`. Closed unless granted (A5): every
    /// engine start passes the intended state, so a new generation never
    /// inherits an open gate from an old one.
    pub internet_open: bool,
    /// Break modes: filled by [`EngineSettings::from_env`] from
    /// `marsh_contracts::break_mode` only, and passed to the engine through its
    /// dedicated `--break-modes` argument (A10).
    breaks: Vec<String>,
}

impl Default for EngineSettings {
    fn default() -> Self {
        Self {
            pool_bytes: DEFAULT_POOL_BYTES,
            ready_timeout: DEFAULT_READY_TIMEOUT,
            internet_open: false,
            breaks: Vec::new(),
        }
    }
}

impl EngineSettings {
    /// The project's Internet grant as it is now (A5): the one place an engine
    /// start learns the gate state, and through it the registry allowlist
    /// (`engine-up --internet` writes both). Call it for every start, so a new
    /// generation never inherits an open gate from an old one.
    #[must_use]
    pub fn for_project(mut self, grants: &super::InternetGrants, project: &Path) -> Self {
        self.internet_open = grants.is_on(&project.to_string_lossy());
        self
    }

    /// Defaults overridden by `MARSH_SERVICES_POOL_BYTES` (a test setting, read
    /// once at daemon start), with the daemon's active break modes.
    ///
    /// # Errors
    /// A malformed size.
    pub fn from_env() -> Result<Self, EngineError> {
        let mut settings = Self::default();
        if let Some(value) = std::env::var_os("MARSH_SERVICES_POOL_BYTES") {
            settings.pool_bytes = parse_size(&value.to_string_lossy()).map_err(|error| {
                EngineError::Setup(format!("MARSH_SERVICES_POOL_BYTES: {error}"))
            })?;
        }
        settings.breaks = active_breaks();
        Ok(settings)
    }
}

/// Readiness of one services VM's engine (a slice of S1's per-VM state).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EngineState {
    Starting,
    Ready(Generation),
    Unavailable(String),
}

impl EngineState {
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready(_) => "ready",
            Self::Unavailable(_) => "unavailable",
        }
    }
}

/// One logged transition.
#[derive(Clone, Debug)]
pub struct Transition {
    pub elapsed_ms: u64,
    pub state: EngineState,
}

type Observer = Arc<dyn Fn(&Transition) + Send + Sync>;

/// Where a relay must dial to reach the production engine.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelayTarget {
    /// The container to `docker exec` into; `None` means the boot engine itself.
    pub container: Option<String>,
    pub socket: String,
}

const BOOT_ENGINE_SOCKET: &str = "/var/run/docker.sock";
/// How long `detach` waits for the startup worker before leaving it to stop on
/// its own at its next cancellation check.
const DETACH_WAIT: Duration = Duration::from_secs(5);

struct Shared {
    started: Instant,
    state: Mutex<EngineState>,
    changed: Condvar,
    /// Serializes notification so observers see transitions in order, without
    /// holding the state lock (an observer may read the state).
    notify: Mutex<()>,
    observers: Mutex<Vec<Observer>>,
    history: Mutex<Vec<Transition>>,
    cancelled: AtomicBool,
    stream: Mutex<Option<Box<dyn marsh_runtime::AttachedProcess>>>,
    diagnostic: Mutex<Option<String>>,
    /// The firewall report of the `ready` that made this generation ready.
    firewall: Mutex<Option<FirewallReport>>,
    relay_boot_engine: bool,
    /// Test break `engine-watch-unbounded`: reasons are kept as the VM wrote them.
    raw_reasons: bool,
}

impl Shared {
    fn set(&self, state: EngineState) {
        let _ordered = self
            .notify
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let transition = {
            let mut current = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Unavailable is terminal for a generation: only a new start leaves it.
            if matches!(*current, EngineState::Unavailable(_)) || *current == state {
                return;
            }
            *current = state.clone();
            let transition = Transition {
                elapsed_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
                state,
            };
            self.history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(transition.clone());
            transition
        };
        self.changed.notify_all();
        let observers = self
            .observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for observer in observers {
            observer(&transition);
        }
    }

    /// Reasons can carry what the engine (the VM) reported: plain text only.
    fn unavailable(&self, reason: impl Into<String>) {
        let reason = reason.into();
        let reason = if self.raw_reasons {
            reason
        } else {
            plain_text(&reason, REASON_BYTES)
        };
        self.set(EngineState::Unavailable(reason));
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn check_cancelled(&self) -> Result<(), EngineError> {
        if self.is_cancelled() {
            Err(EngineError::Setup("cancelled".into()))
        } else {
            Ok(())
        }
    }

    /// Cancel startup or watching: no step after this point creates or
    /// attaches anything, an attachment already published is terminated, and
    /// readiness is revoked. Synchronized with attachment publication through
    /// the stream lock, so a stream can never be published after this returns.
    fn cancel(&self) {
        {
            let mut slot = self
                .stream
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.cancelled.store(true, Ordering::Release);
            if let Some(process) = slot.as_mut() {
                let _ = process.terminate();
            }
        }
        self.unavailable("detached");
    }

    fn terminate_stream(&self) {
        if let Some(process) = self
            .stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            let _ = process.terminate();
        }
    }
}

/// A running (or failed) engine generation.
pub struct EngineHandle {
    shared: Arc<Shared>,
    worker: Mutex<Option<(thread::JoinHandle<()>, mpsc::Receiver<()>)>>,
}

impl EngineHandle {
    #[must_use]
    pub fn state(&self) -> EngineState {
        self.shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The generation, only while the engine is ready. A channel opener calls
    /// this for every channel and refuses on `None`.
    #[must_use]
    pub fn ready_generation(&self) -> Option<Generation> {
        match self.state() {
            EngineState::Ready(generation) => Some(generation),
            _ => None,
        }
    }

    /// Where a relay for this generation dials: the production engine's socket
    /// inside the engine container. `None` unless the engine is ready.
    ///
    /// # Errors
    /// The state, when the engine is not ready.
    pub fn relay_target(&self) -> Result<RelayTarget, String> {
        match self.state() {
            EngineState::Ready(_) if self.shared.relay_boot_engine => Ok(RelayTarget {
                // Defect (`relay-dial-boot-engine`): the boot engine.
                container: None,
                socket: BOOT_ENGINE_SOCKET.to_owned(),
            }),
            EngineState::Ready(_) => Ok(RelayTarget {
                container: Some(ENGINE_CONTAINER.to_owned()),
                socket: marsh_services::engine::ENGINE_SOCKET.to_owned(),
            }),
            other => Err(format!("marsh: services are {}", other.label())),
        }
    }

    /// A note added after readiness was already revoked (what the VM and the
    /// container looked like). Never waits for the state.
    #[must_use]
    pub fn diagnostic(&self) -> Option<String> {
        self.shared
            .diagnostic
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The egress firewall check the engine reported with the `ready` that made
    /// this generation ready (A12): the sbx gateway addresses it covers. `None`
    /// until then.
    #[must_use]
    pub fn firewall(&self) -> Option<FirewallReport> {
        self.shared
            .firewall
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Every transition so far, in order.
    #[must_use]
    pub fn history(&self) -> Vec<Transition> {
        self.shared
            .history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Register an observer, called on every transition from now on, in order,
    /// outside the state lock (it may read the state). Owners of channels close
    /// them when the state leaves `Ready`. An observer must not call `detach`.
    pub fn subscribe(&self, observer: impl Fn(&Transition) + Send + Sync + 'static) {
        self.shared
            .observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Arc::new(observer));
    }

    /// Wait until the state is no longer `Starting`.
    ///
    /// # Errors
    /// The unavailable reason, or a timeout message.
    pub fn wait_ready(&self, timeout: Duration) -> Result<Generation, String> {
        let deadline = Instant::now() + timeout;
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            match &*state {
                EngineState::Ready(generation) => return Ok(generation.clone()),
                EngineState::Unavailable(reason) => return Err(reason.clone()),
                EngineState::Starting => {}
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("timed out waiting for the engine".into());
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// Wait until the state is `Unavailable` (or the timeout passes).
    #[must_use]
    pub fn wait_unavailable(&self, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let EngineState::Unavailable(reason) = &*state {
                return Some(reason.clone());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// Stop watching and revoke readiness; also valid while still `Starting`.
    /// A start that has not reached `docker start` never starts the engine; one
    /// that has leaves the container running (the VM owns its life, and the next
    /// generation replaces it). Waits at most a few seconds for the worker: a
    /// worker inside a long `sbx` command ends at its next cancellation check.
    pub fn detach(&self) {
        self.shared.cancel();
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some((handle, done)) = worker {
            match done.recv_timeout(DETACH_WAIT) {
                Err(mpsc::RecvTimeoutError::Timeout) => drop(handle),
                _ => {
                    let _ = handle.join();
                }
            }
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.detach();
    }
}

/// Starts engine generations through stock `sbx`.
#[derive(Clone)]
pub struct ServicesEngine {
    pub(crate) sbx: PathBuf,
    pub(crate) runner: Arc<dyn CommandRunner>,
    /// Where each generation's container id is recorded, when the daemon's
    /// adapter owns the record ([`StockSbx::services_engine`]).
    engine_ids: Option<Arc<Mutex<BTreeMap<String, String>>>>,
}

impl StockSbx {
    /// The engine lifecycle over this adapter's `sbx` and runner. Each
    /// generation it creates is recorded in the adapter by container id.
    #[must_use]
    pub fn services_engine(&self) -> ServicesEngine {
        let (sbx, runner) = self.stock_command();
        let mut engine = ServicesEngine::new(sbx, runner);
        engine.engine_ids = Some(Arc::clone(&self.services_engine_ids));
        engine
    }
}

/// The break modes selected for this start (from `marsh_contracts::break_mode`,
/// through [`EngineSettings`]). Empty in every build without `test-breaks`.
struct Breaks<'a>(&'a [String]);

impl Breaks<'_> {
    fn active(&self, mode: &str) -> bool {
        self.0.iter().any(|active| active == mode)
    }

    /// The modes `engine-up` itself implements, for its dedicated argument.
    #[cfg(feature = "test-breaks")]
    fn engine_modes(&self) -> Option<String> {
        const ENGINE: [&str; 8] = [
            "engine-firewall-off",
            "engine-ip-any",
            "engine-cgroup-no-delegate",
            "entitlements-on",
            "pool-no-size-check",
            "pool-skip-on-start",
            "slow-pool",
            "hold-engine-before-ready",
        ];
        let modes: Vec<&str> = self
            .0
            .iter()
            .map(String::as_str)
            .filter(|mode| ENGINE.contains(mode))
            .collect();
        (!modes.is_empty()).then(|| modes.join(","))
    }
}

impl ServicesEngine {
    #[must_use]
    pub fn new(sbx: PathBuf, runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            sbx,
            runner,
            engine_ids: None,
        }
    }

    /// Begin a new generation. Returns at once in `Starting`; the steps run on
    /// a worker thread and the handle reports the outcome.
    #[must_use]
    pub fn start(
        &self,
        vm: ServicesVmRef,
        artifacts: EngineArtifacts,
        settings: EngineSettings,
    ) -> EngineHandle {
        let shared = Arc::new(Shared {
            started: Instant::now(),
            state: Mutex::new(EngineState::Starting),
            changed: Condvar::new(),
            notify: Mutex::new(()),
            observers: Mutex::new(Vec::new()),
            history: Mutex::new(vec![Transition {
                elapsed_ms: 0,
                state: EngineState::Starting,
            }]),
            cancelled: AtomicBool::new(false),
            stream: Mutex::new(None),
            diagnostic: Mutex::new(None),
            firewall: Mutex::new(None),
            relay_boot_engine: Breaks(&settings.breaks).active("relay-dial-boot-engine"),
            raw_reasons: Breaks(&settings.breaks).active("engine-watch-unbounded"),
        });
        let engine = self.clone();
        let inner = Arc::clone(&shared);
        let (finished, done) = mpsc::channel();
        let worker = thread::spawn(move || {
            if let Err(error) = engine.run(&vm, &artifacts, &settings, &inner) {
                inner.unavailable(error.to_string());
            }
            let _ = finished.send(());
        });
        EngineHandle {
            shared,
            worker: Mutex::new(Some((worker, done))),
        }
    }

    fn exec(
        &self,
        vm: &str,
        arguments: &[&str],
        timeout: Duration,
    ) -> Result<CommandOutput, EngineError> {
        let mut command: Vec<OsString> = ["exec", "-u", "root", vm]
            .iter()
            .map(OsString::from)
            .collect();
        command.extend(arguments.iter().map(OsString::from));
        self.runner
            .run_bounded(&self.invocation(command), timeout)
            .map_err(|error| {
                EngineError::Setup(format!("sbx exec {}: {error}", arguments.join(" ")))
            })
    }

    fn exec_ok(
        &self,
        vm: &str,
        arguments: &[&str],
        timeout: Duration,
    ) -> Result<String, EngineError> {
        let output = self.exec(vm, arguments, timeout)?;
        if output.succeeded() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        } else {
            Err(EngineError::Setup(format!(
                "{} failed ({:?}): {}",
                arguments.join(" "),
                output.exit_code,
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }

    fn invocation(&self, arguments: Vec<OsString>) -> Invocation {
        Invocation {
            program: self.sbx.clone(),
            arguments,
            working_directory: None,
            environment: Vec::new(),
        }
    }

    fn run(
        &self,
        vm: &ServicesVmRef,
        artifacts: &EngineArtifacts,
        settings: &EngineSettings,
        shared: &Arc<Shared>,
    ) -> Result<(), EngineError> {
        let breaks = Breaks(&settings.breaks);
        let name = vm.name.as_str();
        shared.check_cancelled()?;
        let boot_id = self.exec_ok(name, &["cat", BOOT_ID_PATH], EXEC_TIMEOUT)?;
        shared.check_cancelled()?;
        self.install_artifacts(name, artifacts, shared)?;
        shared.check_cancelled()?;
        let image = self.ensure_image(name, artifacts, &breaks)?;
        shared.check_cancelled()?;
        // A new generation always means a new container.
        let _ = self.exec(
            name,
            &["docker", "rm", "-f", ENGINE_CONTAINER],
            EXEC_TIMEOUT,
        )?;
        shared.check_cancelled()?;
        let id = self.create_container(vm, &image, settings, &breaks)?;
        if let Some(record) = &self.engine_ids {
            record
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(name.to_owned(), id.clone());
        }
        if !breaks.active("bootstrap-use-user-image") {
            self.verify_container_image(name, &id, &artifacts.pin)?;
        }
        // The last cancellation point before the engine exists as a process.
        shared.check_cancelled()?;
        self.exec_ok(name, &["docker", "start", &id], EXEC_TIMEOUT)?;
        let expected = Generation {
            boot_id,
            engine_id: id.clone(),
        };
        if breaks.active("ready-before-pool") {
            // Defect: the container having started is taken for readiness.
            shared.set(EngineState::Ready(expected.clone()));
        }
        self.watch(vm, &id, &expected, settings, shared)
    }

    /// Put the pinned tar and the marsh binary on the VM's Docker disk, unless
    /// they are already there with the right hashes.
    fn install_artifacts(
        &self,
        vm: &str,
        artifacts: &EngineArtifacts,
        shared: &Shared,
    ) -> Result<(), EngineError> {
        self.exec_ok(
            vm,
            &[
                "install",
                "-d",
                "-m",
                "0700",
                "-o",
                "root",
                VM_STATE_DIR,
                VM_ARTIFACT_DIR,
                VM_POOL_DIR,
            ],
            EXEC_TIMEOUT,
        )?;
        for (source, destination, mode, sha256) in [
            (
                &artifacts.archive,
                VM_ARCHIVE,
                "0600",
                artifacts.pin.archive_sha256.as_str(),
            ),
            (
                &artifacts.marsh,
                VM_MARSH,
                "0755",
                artifacts.marsh_sha256.as_str(),
            ),
        ] {
            shared.check_cancelled()?;
            let present = self.exec(vm, &["sha256sum", destination], EXEC_TIMEOUT)?;
            let present = String::from_utf8_lossy(&present.stdout);
            if present.split_whitespace().next() == Some(sha256) {
                continue;
            }
            let temporary = format!("/tmp/marsh-svc-{sha256}.install");
            let copy = Invocation {
                program: self.sbx.clone(),
                arguments: vec![
                    OsString::from("cp"),
                    source.as_os_str().to_owned(),
                    OsString::from(format!("{vm}:{temporary}")),
                ],
                working_directory: None,
                environment: Vec::new(),
            };
            let output = self
                .runner
                .run_bounded(&copy, COPY_TIMEOUT)
                .map_err(|error| EngineError::Setup(format!("sbx cp: {error}")))?;
            if !output.succeeded() {
                return Err(EngineError::Setup(format!(
                    "sbx cp failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
            self.exec_ok(
                vm,
                &[
                    "install",
                    "-m",
                    mode,
                    "-o",
                    "root",
                    "-g",
                    "root",
                    &temporary,
                    destination,
                ],
                EXEC_TIMEOUT,
            )?;
            let _ = self.exec(vm, &["rm", "-f", &temporary], EXEC_TIMEOUT)?;
            let installed = self.exec_ok(vm, &["sha256sum", destination], EXEC_TIMEOUT)?;
            if installed.split_whitespace().next() != Some(sha256) {
                return Err(EngineError::Setup(format!(
                    "{destination} does not match its pinned sha256 after copy"
                )));
            }
        }
        Ok(())
    }

    /// Load the image from the artifact tar and verify its id against the pin.
    /// Returns the reference to run: the verified image id.
    fn ensure_image(
        &self,
        vm: &str,
        artifacts: &EngineArtifacts,
        breaks: &Breaks<'_>,
    ) -> Result<String, EngineError> {
        let pin = &artifacts.pin;
        if breaks.active("bootstrap-use-user-image") {
            // Defect: an image the user tagged for the engine is used as is,
            // whether it sits in the boot engine, or in the production engine
            // of a generation that is still running (it is copied across).
            let _ = self.exec(
                vm,
                &[
                    "sh",
                    "-c",
                    &format!(
                        "docker exec {ENGINE_CONTAINER} docker -H unix://{} save {LOCAL_TAG} | docker load",
                        marsh_services::engine::ENGINE_SOCKET
                    ),
                ],
                LOAD_TIMEOUT,
            )?;
            if self
                .exec(
                    vm,
                    &[
                        "docker", "image", "inspect", "--format", "{{.Id}}", LOCAL_TAG,
                    ],
                    EXEC_TIMEOUT,
                )?
                .succeeded()
            {
                return Ok(LOCAL_TAG.to_owned());
            }
        }
        let present = self.exec(
            vm,
            &[
                "docker",
                "image",
                "inspect",
                "--format",
                "{{.Id}}",
                &pin.image_id,
            ],
            EXEC_TIMEOUT,
        )?;
        let present_id = String::from_utf8_lossy(&present.stdout).trim().to_owned();
        if !(present.succeeded() && id_matches(&present_id, pin)) {
            let loaded = self.exec_ok(vm, &["docker", "load", "-i", VM_ARCHIVE], LOAD_TIMEOUT)?;
            let loaded_id = loaded
                .lines()
                .find_map(|line| line.strip_prefix("Loaded image ID: "))
                .ok_or_else(|| {
                    EngineError::Setup(format!("docker load reported no image id: {loaded}"))
                })?
                .trim();
            if !id_matches(loaded_id, pin) {
                return Err(EngineError::Setup(format!(
                    "the loaded engine image is {loaded_id}, not the pinned {}",
                    pin.image_id
                )));
            }
            let inspected = self.exec_ok(
                vm,
                &[
                    "docker",
                    "image",
                    "inspect",
                    "--format",
                    "{{.Id}}",
                    &pin.image_id,
                ],
                EXEC_TIMEOUT,
            )?;
            if !id_matches(&inspected, pin) {
                return Err(EngineError::Setup(format!(
                    "the engine image inspects as {inspected}, not the pinned {}",
                    pin.image_id
                )));
            }
        }
        if breaks.active("bootstrap-use-user-image") {
            // Make the tag exist so later starts take the user's image.
            let _ = self.exec(
                vm,
                &["docker", "tag", &pin.image_id, LOCAL_TAG],
                EXEC_TIMEOUT,
            )?;
            return Ok(LOCAL_TAG.to_owned());
        }
        Ok(pin.image_id.clone())
    }

    fn create_container(
        &self,
        vm: &ServicesVmRef,
        image: &str,
        settings: &EngineSettings,
        breaks: &Breaks<'_>,
    ) -> Result<String, EngineError> {
        let project = vm.project_root.to_string_lossy();
        if !vm.project_root.is_absolute() || project.contains([',', '\n', '=']) {
            return Err(EngineError::Setup(format!(
                "the project path {project:?} cannot be mounted into the engine"
            )));
        }
        if let Some(path) = engine_path_conflict(&vm.project_root) {
            return Err(EngineError::Setup(format!(
                "the project path {project:?} overlaps the engine's own path {path}; refusing to mount it"
            )));
        }
        #[cfg_attr(not(feature = "test-breaks"), allow(unused_mut))]
        let mut arguments: Vec<String> = [
            "docker",
            "create",
            "--pull",
            "never",
            "--name",
            ENGINE_CONTAINER,
            "--privileged",
            "--init",
            "--cgroupns",
            "private",
            "--restart",
            "no",
            "--label",
            "dev.marsh.role=svc-engine",
            "--mount",
            &format!("type=bind,src={VM_MARSH},dst={MARSH_BINARY},readonly"),
            "--mount",
            &format!("type=bind,src={VM_POOL_DIR},dst={ENGINE_POOL_DIR}"),
            "--mount",
            &format!("type=bind,src={project},dst={project}"),
            image,
            "--pool-bytes",
            &settings.pool_bytes.to_string(),
            "--internet",
            if settings.internet_open { "on" } else { "off" },
        ]
        .iter()
        .map(|argument| (*argument).to_owned())
        .collect();
        #[cfg(feature = "test-breaks")]
        if let Some(modes) = breaks.engine_modes() {
            arguments.extend(["--break-modes".to_owned(), modes]);
        }
        #[cfg(not(feature = "test-breaks"))]
        let _ = breaks;
        let borrowed: Vec<&str> = arguments.iter().map(String::as_str).collect();
        let id = self.exec_ok(&vm.name, &borrowed, EXEC_TIMEOUT)?;
        if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(EngineError::Setup(format!("docker create returned {id:?}")));
        }
        Ok(id.to_ascii_lowercase())
    }

    fn verify_container_image(
        &self,
        vm: &str,
        id: &str,
        pin: &EnginePin,
    ) -> Result<(), EngineError> {
        let image = self.exec_ok(
            vm,
            &["docker", "inspect", "--format", "{{.Image}}", id],
            EXEC_TIMEOUT,
        )?;
        if id_matches(&image, pin) {
            Ok(())
        } else {
            Err(EngineError::Setup(format!(
                "the engine container runs image {image}, not the pinned {}",
                pin.image_id
            )))
        }
    }

    /// The boot engine must hold exactly the engine container. Returns the
    /// reason when it does not.
    fn audit_boot_engine(&self, vm: &str, id: &str) -> Result<Option<String>, EngineError> {
        let listing = self.exec_ok(
            vm,
            &[
                "docker",
                "ps",
                "-a",
                "--no-trunc",
                "--format",
                "{{.ID}} {{.Names}}",
            ],
            EXEC_TIMEOUT,
        )?;
        let rows: Vec<&str> = listing
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let expected = format!("{id} {ENGINE_CONTAINER}");
        if rows == [expected.as_str()] {
            Ok(None)
        } else {
            Ok(Some(format!(
                "the boot engine holds more than {ENGINE_CONTAINER}: [{}]",
                rows.join("; ")
            )))
        }
    }

    /// The daemon's own audit of the firewall (A12), independent of anything
    /// the engine says about itself. It reads `iptables -S` and `ip6tables -S`
    /// through the boot engine, from the engine container **by its recorded id**
    /// (never by name), and compares them with the canonical expected rule set
    /// ([`firewall::check_rules`]) on this side.
    ///
    /// The gateway addresses to cover are the ones the engine reported, but the
    /// VM's own resolution of the gateway must be a subset of them: an engine
    /// cannot shrink what must be dropped.
    fn audit_firewall(&self, vm: &str, id: &str, report: &FirewallReport) -> Result<(), String> {
        firewall::require_report(Some(report))?;
        let gateway = firewall::Gateway::from_reported(&report.addresses)?;
        let resolved = |family: &str| -> Result<Vec<std::net::IpAddr>, String> {
            let output = self
                .exec(
                    vm,
                    &["getent", family, firewall::GATEWAY_NAME],
                    EXEC_TIMEOUT,
                )
                .map_err(|error| {
                    format!("marsh: the firewall audit could not resolve the gateway: {error}")
                })?;
            Ok(firewall::parse_getent(&String::from_utf8_lossy(
                &output.stdout,
            )))
        };
        let in_vm_v4 = resolved("ahostsv4")?;
        if in_vm_v4.is_empty() {
            return Err(
                "marsh: the VM does not resolve the sbx gateway; the firewall cannot be audited"
                    .into(),
            );
        }
        for address in in_vm_v4.into_iter().chain(resolved("ahostsv6")?) {
            let covered = match address {
                std::net::IpAddr::V4(address) => gateway.v4.contains(&address),
                std::net::IpAddr::V6(address) => gateway.v6.contains(&address),
            };
            if !covered {
                return Err(format!(
                    "marsh: the VM resolves the sbx gateway to {address}, which the engine's firewall report does not cover"
                ));
            }
        }
        let listing = |tool: &str| -> Result<String, String> {
            self.exec_ok(vm, &["docker", "exec", id, tool, "-w", "-S"], EXEC_TIMEOUT)
                .map_err(|error| {
                    format!("marsh: the firewall audit could not read {tool}: {error}")
                })
        };
        let v4 = listing("iptables")?;
        let v6 = if gateway.v6.is_empty() {
            None
        } else {
            Some(listing("ip6tables")?)
        };
        firewall::check_rules(&gateway, Some(&v4), v6.as_deref())
            .map_err(|message| format!("marsh: the engine firewall audit failed: {message}"))
    }

    #[allow(clippy::too_many_lines)]
    fn watch(
        &self,
        vm: &ServicesVmRef,
        id: &str,
        expected: &Generation,
        settings: &EngineSettings,
        shared: &Arc<Shared>,
    ) -> Result<(), EngineError> {
        let command = vec![
            OsString::from("exec"),
            OsString::from("-u"),
            OsString::from("root"),
            OsString::from(&vm.name),
            OsString::from("docker"),
            OsString::from("logs"),
            OsString::from("--follow"),
            OsString::from(id),
        ];
        let attachment = self
            .runner
            .spawn_attached(&self.invocation(command))
            .map_err(|error| EngineError::Setup(format!("sbx exec docker logs: {error}")))?;
        drop(attachment.stdin);
        {
            // Publish under the lock `cancel` takes, so a cancellation either
            // sees this stream or stops it here: never a stream nobody ends.
            let mut slot = shared
                .stream
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if shared.is_cancelled() {
                let mut process = attachment.process;
                let _ = process.terminate();
                return Ok(());
            }
            *slot = Some(attachment.process);
        }
        let tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
        let stderr_tail = Arc::clone(&tail);
        let stderr = attachment.stderr;
        // Everything the VM writes is read in bounded lines and made plain
        // before it is kept (`engine-watch-unbounded` puts the old reader back).
        let unbounded = Breaks(&settings.breaks).active("engine-watch-unbounded");
        let stderr_reader = thread::spawn(move || {
            let cap = if unbounded {
                usize::MAX
            } else {
                DISPLAY_LINE_BYTES
            };
            for line in CappedLines::new(BufReader::new(stderr), cap) {
                let line = if unbounded {
                    line
                } else {
                    plain_text(&line, DISPLAY_LINE_BYTES)
                };
                let mut lines = stderr_tail
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if lines.len() == STDERR_TAIL_LINES {
                    lines.pop_front();
                }
                lines.push_back(line);
            }
        });
        // The ready deadline: kill the stream if the engine never reports.
        let deadline_shared = Arc::clone(shared);
        let timeout = settings.ready_timeout;
        let watchdog = thread::spawn(move || {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                if !matches!(
                    *deadline_shared
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                    EngineState::Starting
                ) || deadline_shared.is_cancelled()
                {
                    return;
                }
                thread::sleep(Duration::from_millis(200));
            }
            if matches!(
                *deadline_shared
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                EngineState::Starting
            ) {
                deadline_shared.unavailable(format!(
                    "marsh: the engine was not ready within {}s",
                    timeout.as_secs()
                ));
                if let Some(process) = deadline_shared
                    .stream
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_mut()
                {
                    let _ = process.terminate();
                }
            }
        });
        let mut reported: Option<String> = None;
        // An event is a short JSON line; a longer one is not an event.
        let event_cap = if unbounded {
            usize::MAX
        } else {
            EVENT_LINE_BYTES
        };
        for line in CappedLines::new(BufReader::new(attachment.stdout), event_cap) {
            match EngineEvent::parse(&line) {
                Some(EngineEvent::Ready {
                    generation,
                    firewall: reported_firewall,
                    ..
                }) => {
                    if &generation != expected {
                        // A ready for another generation (a replay, or another
                        // engine's) never makes this one ready.
                        shared.unavailable(format!(
                            "marsh: ready for generation {}/{} but this engine is {}/{}",
                            generation.boot_id,
                            generation.engine_id,
                            expected.boot_id,
                            expected.engine_id
                        ));
                        break;
                    }
                    // A12: no firewall, no readiness. The engine's own report must
                    // be a verified one, and an independent check inside the engine
                    // must agree with it (`engine-firewall-off` skips only the
                    // audit: a defective engine still reports a firewall).
                    let audited = match reported_firewall.as_ref() {
                        Some(report) if Breaks(&settings.breaks).active("engine-firewall-off") => {
                            firewall::require_report(Some(report))
                        }
                        Some(report) => self.audit_firewall(&vm.name, id, report),
                        None => firewall::require_report(None),
                    };
                    if let Err(reason) = audited {
                        shared.unavailable(reason);
                        break;
                    }
                    match self.audit_boot_engine(&vm.name, id)? {
                        None => {
                            *shared
                                .firewall
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                reported_firewall;
                            shared.set(EngineState::Ready(generation));
                        }
                        Some(reason) => {
                            shared.unavailable(reason);
                            break;
                        }
                    }
                }
                Some(EngineEvent::Failed { message, .. }) => {
                    reported = Some(message);
                    break;
                }
                Some(EngineEvent::Lost { reason }) => {
                    reported = Some(format!(
                        "marsh: the engine reported it lost something: {reason}"
                    ));
                    break;
                }
                Some(EngineEvent::Stage { .. } | EngineEvent::Stopping) | None => {}
            }
        }
        // The stream ended. Revoke readiness and tell the channel owners
        // first, from what is already known; nothing below can delay that.
        let reason = reported.unwrap_or_else(|| {
            "marsh: the engine's log stream ended (the container stopped or was removed, the exec was lost, or the VM stopped)"
                .to_owned()
        });
        shared.unavailable(reason);
        shared.terminate_stream();
        let _ = watchdog.join();
        let _ = stderr_reader.join();
        // Then explain it, without waking a stopped VM.
        let note = self.explain_exit(&vm.name, &tail);
        *shared
            .diagnostic
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(note);
        Ok(())
    }

    /// Whether `sbx ls` shows the VM running. `sbx ls` does not start anything.
    fn vm_running(&self, vm: &str) -> bool {
        self.runner
            .run_bounded(&self.invocation(vec![OsString::from("ls")]), EXEC_TIMEOUT)
            .ok()
            .filter(CommandOutput::succeeded)
            .is_some_and(|output| {
                String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                    line.split_whitespace().next() == Some(vm) && line.contains("running")
                })
            })
    }

    /// After readiness is already revoked: a note on what is known without
    /// touching the VM. Never `sbx exec`: an exec into a VM that is stopping or
    /// stopped starts it, and `sbx ls` can still say `running` while it stops.
    fn explain_exit(&self, vm: &str, tail: &Arc<Mutex<VecDeque<String>>>) -> String {
        let stderr = tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(" | ");
        let state = if self.vm_running(vm) {
            "listed as running by sbx ls (it may still be stopping)"
        } else {
            "not running"
        };
        format!("the services VM is {state}{}", suffix(&stderr))
    }
}

/// Paths inside the engine container that a project mount must not equal, contain
/// or sit under: it would shadow them or put the production socket on a shared
/// mount.
const ENGINE_PATHS: [&str; 13] = [
    "/etc", "/run", "/pool", "/marsh", "/var", "/usr", "/proc", "/dev", "/sys", "/bin", "/sbin",
    "/lib", "/root",
];

fn engine_path_conflict(project: &Path) -> Option<&'static str> {
    if project.parent().is_none() {
        return Some("/");
    }
    ENGINE_PATHS.iter().copied().find(|path| {
        let path = Path::new(path);
        project.starts_with(path) || path.starts_with(project)
    })
}

fn suffix(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!(": {stderr}")
    }
}

fn id_matches(observed: &str, pin: &EnginePin) -> bool {
    // The boot engine's image store reports the manifest digest (containerd
    // snapshotter) or the config digest (classic): both are pinned.
    let observed = observed.trim();
    observed == pin.image_id || observed == pin.config_id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_paths_that_overlap_the_engine_are_refused() {
        for refused in [
            "/",
            "/etc",
            "/etc/x",
            "/pool",
            "/var/lib/marsh-svc",
            "/run/x",
            "/marsh",
        ] {
            assert!(
                engine_path_conflict(Path::new(refused)).is_some(),
                "{refused}"
            );
        }
        for fine in [
            "/Users/mark/dev/project",
            "/Volumes/work/p",
            "/private/tmp/nfx-1",
            "/home/mark/p",
            "/data/p",
        ] {
            assert!(engine_path_conflict(Path::new(fine)).is_none(), "{fine}");
        }
    }

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("24g").unwrap(), 24 << 30);
        assert_eq!(parse_size("512M").unwrap(), 512 << 20);
        assert_eq!(parse_size("12345").unwrap(), 12345);
        assert!(parse_size("lots").is_err());
    }
}
