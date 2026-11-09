//! `marsh --internal-services engine-up`: the entrypoint of `marsh-svc-engine`
//! (SECURITY-DESIGN A1).
//!
//! The container runs on the services VM's stock ("boot") Docker engine and is
//! the only thing that engine ever holds. Inside it, in this order:
//!
//! 0. install the egress firewall for the sbx gateway ([`crate::firewall`]);
//!    then the Internet gate and, with it, the engine's configuration
//!    ([`crate::internet`]: closed unless `--internet on`);
//! 1. delegate the cgroup v2 controllers to the production engine
//!    ([`delegate_cgroups`]);
//! 2. `mknod` the loop nodes;
//! 3. create the pool image on first use (refusing when the disk cannot hold
//!    it) and mount the XFS (`reflink=1`) pool;
//! 4. check the pool's identity and type, then probe reflink;
//! 5. write the hardened `daemon.json`;
//! 6. recreate the bind pins (E3: [`restore_pins`]);
//! 7. start the production `dockerd` (`data-root` on the pool, socket at
//!    [`ENGINE_SOCKET`] inside this container only) and wait for `/_ping`.
//!
//! Only then does it print `ready{generation, firewall}`. The generation is the
//! VM boot id plus this container's id. Events are JSON lines on stdout, one per
//! line; the daemon reads them through `docker logs -f`. The process then
//! supervises the production engine, the pool mount and the firewall rules, and
//! prints `lost` and exits when any of them goes away: the daemon revokes
//! readiness when the stream ends.

use crate::firewall::{self, FirewallReport, Gateway};
use crate::internet;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Name of the one container the boot engine runs.
pub const ENGINE_CONTAINER: &str = "marsh-svc-engine";
/// The production engine's socket. It exists only inside the engine container.
pub const ENGINE_SOCKET: &str = "/run/marsh-svc/docker.sock";
/// Where the XFS pool is mounted inside the engine container.
pub const POOL_MOUNT: &str = "/pool";
/// The production engine's data-root, on the pool.
pub const DATA_ROOT: &str = "/pool/docker";
/// Persistent directory of the pool image, bind-mounted from the VM's Docker disk.
pub const POOL_DIR: &str = "/var/lib/marsh-svc";
/// Pool image name inside [`POOL_DIR`], and its identity record.
pub const POOL_IMAGE: &str = "pool.img";
pub const POOL_META: &str = "pool.meta";
/// Where the daemon bind-mounts the read-only marsh binary.
pub const MARSH_BINARY: &str = "/marsh/marsh";
/// The hardened production daemon configuration.
pub const DAEMON_JSON: &str = "/etc/docker/daemon.json";
/// The engine's own configuration for the services agent ([`EngineConfig`]),
/// written by `engine-up` before the production engine starts.
pub const ENGINE_CONFIG: &str = "/run/marsh-svc/engine-config.json";
/// Registries an image may come from while the project's Internet is off:
/// Docker Hub (the name an unqualified reference resolves to) and GHCR. The one
/// list: [`crate::internet::registry_allow`] hands it out.
pub const DEFAULT_REGISTRIES: [&str; 2] = ["docker.io", "ghcr.io"];

/// Free space that must remain on the Docker disk after the pool is allocated:
/// images, the engine container's own layers and the boot engine's state.
pub const DISK_RESERVE_BYTES: u64 = 512 << 20;
/// Smallest pool the engine will create.
pub const MIN_POOL_BYTES: u64 = 1 << 30;
const LOOP_NODES: u32 = 32;
const PING_TIMEOUT: Duration = Duration::from_secs(90);
const SUPERVISE_INTERVAL: Duration = Duration::from_millis(500);
const PING_EVERY: u32 = 10;
/// The rules are verified every second.
const FIREWALL_EVERY: u32 = 2;
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// One engine generation: a VM boot and one engine container. Any restart of
/// either is a new generation, and nothing is usable until a new `ready`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Generation {
    pub boot_id: String,
    pub engine_id: String,
}

/// The engine's configuration for the filtered Docker endpoint (E3
/// `policy::endpoint::Config`), written to [`ENGINE_CONFIG`]. The services agent
/// that serves the filtered sockets loads it ([`EngineConfig::load`]) and applies
/// it ([`EngineConfig::apply`]).
///
/// It has one input, the project's Internet gate (SECURITY-DESIGN A5): the
/// registry allowlist is restricted while the gate is closed and lifted while it
/// is open ([`EngineConfig::for_internet`]). [`crate::internet::set`] rewrites it
/// together with the gate, so the two never disagree, and the supervisor treats a
/// disagreement as a lost gate.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EngineConfig {
    /// Registry hosts an image may be pulled from (`docker.io` for unqualified
    /// names); `None` allows every registry.
    pub registry_allow: Option<Vec<String>>,
}

impl Default for EngineConfig {
    /// Closed: the default registries only.
    fn default() -> Self {
        Self::for_internet(false)
    }
}

impl EngineConfig {
    /// The configuration for a gate state: the default registries while closed,
    /// every registry while open.
    #[must_use]
    pub fn for_internet(open: bool) -> Self {
        Self {
            registry_allow: crate::internet::registry_allow(open),
        }
    }

    /// Set the registry policy of the filtered endpoint's configuration.
    pub fn apply(&self, config: &mut crate::policy::endpoint::Config) {
        config.registry_allow.clone_from(&self.registry_allow);
    }

    /// Read the configuration `engine-up` wrote.
    ///
    /// # Errors
    /// A missing or malformed file: the agent must not serve with a policy it
    /// cannot read.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text =
            fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
    }

    /// Write the file atomically.
    ///
    /// # Errors
    /// The write failed.
    pub fn store(&self, path: &Path) -> Result<(), String> {
        let encoded = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        let temporary = path.with_extension("json.new");
        fs::write(&temporary, encoded)
            .map_err(|error| format!("{}: {error}", temporary.display()))?;
        fs::rename(&temporary, path).map_err(|error| format!("{}: {error}", path.display()))
    }
}

/// Why `engine-up` refused or lost the engine.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureKind {
    /// The Docker disk cannot hold the configured pool.
    DiskTooSmall,
    /// The pool image is not the pool this VM created.
    PoolIdentity,
    /// The pool is not XFS or cannot reflink.
    PoolUnusable,
    /// The egress firewall could not be installed or verified.
    Firewall,
    /// The cgroup v2 controllers could not be delegated to the production engine.
    Cgroup,
    /// Anything else in setup or in the production engine.
    Setup,
}

/// A line on the engine's stdout.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum EngineEvent {
    /// Setup progress. Informational only.
    Stage { stage: String },
    /// Every setup step is done, the firewall is verified and the production
    /// engine answers `/_ping`.
    Ready {
        generation: Generation,
        engine_ready_ms: u64,
        /// The egress firewall check (A12). Absent from an engine that did not
        /// run it, which the daemon refuses.
        #[serde(default)]
        firewall: Option<FirewallReport>,
    },
    /// Setup refused or failed. The process exits non-zero.
    Failed { kind: FailureKind, message: String },
    /// The production engine or the pool went away after `ready`.
    Lost { reason: String },
    /// The engine received SIGTERM and is shutting down cleanly.
    Stopping,
}

impl EngineEvent {
    /// Parse one stdout line. Returns `None` for anything else, so noise on the
    /// stream can never be mistaken for readiness.
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        serde_json::from_str(line.trim()).ok()
    }
}

/// Settings for one `engine-up` run, from the daemon's command line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineUpArguments {
    pub pool_bytes: u64,
    /// The Internet gate starts open (`--internet on`). Anything else, and the
    /// absence of the argument, starts it closed (SECURITY-DESIGN A5).
    pub internet_open: bool,
    pub break_modes: Vec<String>,
}

/// Parse `engine-up` arguments: `--pool-bytes N`, `--internet on|off` and (test
/// builds only) `--break-modes a,b`.
///
/// # Errors
/// Returns a message for an unknown or malformed argument.
pub fn parse_arguments(arguments: &[String]) -> Result<EngineUpArguments, String> {
    let mut parsed = EngineUpArguments {
        pool_bytes: 0,
        internet_open: false,
        break_modes: Vec::new(),
    };
    let mut iterator = arguments.iter();
    while let Some(argument) = iterator.next() {
        match argument.as_str() {
            "--pool-bytes" => {
                let value = iterator.next().ok_or("--pool-bytes needs a value")?;
                parsed.pool_bytes = value
                    .parse()
                    .map_err(|_| format!("invalid --pool-bytes {value}"))?;
            }
            "--internet" => {
                parsed.internet_open = match iterator.next().map(String::as_str) {
                    Some("on") => true,
                    Some("off") => false,
                    _ => return Err("--internet needs on or off".to_owned()),
                };
            }
            #[cfg(feature = "test-breaks")]
            "--break-modes" => {
                let value = iterator.next().ok_or("--break-modes needs a value")?;
                parsed.break_modes = value
                    .split(',')
                    .filter(|mode| !mode.is_empty())
                    .map(str::to_owned)
                    .collect();
            }
            other => return Err(format!("unknown engine-up argument {other}")),
        }
    }
    if parsed.pool_bytes < MIN_POOL_BYTES {
        return Err(format!(
            "--pool-bytes must be at least {MIN_POOL_BYTES} (got {})",
            parsed.pool_bytes
        ));
    }
    Ok(parsed)
}

/// Break modes inside the engine. They arrive only through the dedicated
/// `--break-modes` argument (A10; the daemon fills it from
/// `marsh_contracts::break_mode`), never the environment, and the argument exists
/// only in builds with the `test-breaks` feature, which release packaging refuses
/// at compile time and by the provenance marker. H2's attested in-VM field
/// replaces the argument when it exists.
#[derive(Clone, Debug, Default)]
struct Breaks(#[cfg_attr(not(feature = "test-breaks"), allow(dead_code))] Vec<String>);

impl Breaks {
    #[cfg(feature = "test-breaks")]
    fn active(&self, mode: &str) -> bool {
        self.0.iter().any(|active| active == mode)
    }

    #[cfg(not(feature = "test-breaks"))]
    #[allow(clippy::unused_self)]
    const fn active(&self, _mode: &str) -> bool {
        false
    }
}

/// The files of the `hold-engine-before-ready` handshake, in `/run/marsh-svc`.
/// The engine writes `hold-reached` after the production engine answers
/// `/_ping` and waits for `hold-release` (go on) or `hold-abort` (fail).
#[cfg(feature = "test-breaks")]
const HOLD_DIR: &str = "/run/marsh-svc";
#[cfg(feature = "test-breaks")]
const HOLD_LIMIT: Duration = Duration::from_secs(300);

/// A test hold, not a defect: stops `engine-up` at the last point before it
/// reports `ready`, with every setup step done, so a test can probe what is
/// reachable at that boundary. Only in builds with `test-breaks`.
#[cfg(feature = "test-breaks")]
fn hold_before_ready(breaks: &Breaks) -> Result<(), String> {
    if !breaks.active("hold-engine-before-ready") {
        return Ok(());
    }
    let path = |name: &str| Path::new(HOLD_DIR).join(name);
    for name in ["hold-release", "hold-abort"] {
        let _ = fs::remove_file(path(name));
    }
    fs::write(path("hold-reached"), b"reached")
        .map_err(|error| format!("cannot signal the hold: {error}"))?;
    let deadline = Instant::now() + HOLD_LIMIT;
    while Instant::now() < deadline {
        if path("hold-release").exists() {
            return Ok(());
        }
        if path("hold-abort").exists() {
            return Err("marsh: aborted at hold-engine-before-ready".to_owned());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err("marsh: hold-engine-before-ready was never released".to_owned())
}

#[cfg(not(feature = "test-breaks"))]
#[allow(clippy::unnecessary_wraps)]
const fn hold_before_ready(_breaks: &Breaks) -> Result<(), String> {
    Ok(())
}

/// `engine-up` entry point. Returns the process exit code.
#[must_use]
pub fn run_engine_up(arguments: &[String]) -> i32 {
    let parsed = match parse_arguments(arguments) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("marsh: engine-up: {message}");
            return 2;
        }
    };
    let terminate = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        if let Err(error) = signal_hook::flag::register(signal, Arc::clone(&terminate)) {
            eprintln!("marsh: engine-up: cannot register signal handler: {error}");
            return 1;
        }
    }
    let started = Instant::now();
    let breaks = Breaks(parsed.break_modes.clone());
    let (mut engine, firewall) = match bring_up(&parsed, &breaks) {
        Ok(brought) => brought,
        Err(failure) => {
            emit(&EngineEvent::Failed {
                kind: failure.kind,
                message: failure.message,
            });
            return 1;
        }
    };
    let generation = match generation() {
        Ok(generation) => generation,
        Err(message) => {
            engine.stop();
            emit(&EngineEvent::Failed {
                kind: FailureKind::Setup,
                message,
            });
            return 1;
        }
    };
    if let Err(message) = hold_before_ready(&breaks) {
        engine.stop();
        emit(&EngineEvent::Failed {
            kind: FailureKind::Setup,
            message,
        });
        return 1;
    }
    emit(&EngineEvent::Ready {
        generation,
        engine_ready_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        firewall: Some(firewall.report),
    });
    supervise(&mut engine, &terminate, &breaks, &firewall.gateway)
}

struct Failure {
    kind: FailureKind,
    message: String,
}

fn failure(kind: FailureKind, message: impl Into<String>) -> Failure {
    Failure {
        kind,
        message: message.into(),
    }
}

fn setup(message: impl Into<String>) -> Failure {
    failure(FailureKind::Setup, message)
}

fn emit(event: &EngineEvent) {
    let mut stdout = std::io::stdout().lock();
    if let Ok(line) = serde_json::to_string(event) {
        let _ = writeln!(stdout, "{line}");
        let _ = stdout.flush();
    }
}

fn stage(name: &str) {
    emit(&EngineEvent::Stage {
        stage: name.to_owned(),
    });
}

struct ProductionEngine {
    child: Child,
}

impl ProductionEngine {
    fn stop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = Command::new("kill")
                .args(["-TERM", &self.child.id().to_string()])
                .status();
            let deadline = Instant::now() + STOP_TIMEOUT;
            while Instant::now() < deadline {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// The gateway the rules were installed for and the report that goes into `ready`.
struct InstalledFirewall {
    gateway: Gateway,
    report: FirewallReport,
}

/// Install the egress firewall (A12), before anything of the production engine
/// exists, and read the rules back. Returns the gateway they cover.
fn install_firewall(breaks: &Breaks) -> Result<Gateway, Failure> {
    let refused = |message: String| failure(FailureKind::Firewall, message);
    let gateway = firewall::resolve_gateway().map_err(refused)?;
    if !breaks.active("engine-firewall-off") {
        firewall::install(&gateway).map_err(refused)?;
        firewall::verify(&gateway).map_err(refused)?;
    }
    Ok(gateway)
}

/// The report for `ready`, made after the production engine started (dockerd
/// edits the same tables): the rules read back again, and a TCP connect from
/// here to the gateway's proxy port that must not connect.
fn firewall_report(gateway: Gateway, breaks: &Breaks) -> Result<InstalledFirewall, Failure> {
    if breaks.active("engine-firewall-off") {
        // Defect: nothing was installed and nothing is checked, yet the engine
        // reports a verified firewall for the gateway it resolved.
        return Ok(InstalledFirewall {
            report: FirewallReport {
                addresses: gateway.addresses(),
                verified: true,
                connect_blocked: true,
            },
            gateway,
        });
    }
    let refused = |message: String| {
        failure(
            FailureKind::Firewall,
            format!("{message} (after the production engine started)"),
        )
    };
    let report = firewall::report(&gateway).map_err(refused)?;
    firewall::require_report(Some(&report)).map_err(refused)?;
    Ok(InstalledFirewall { gateway, report })
}

fn bring_up(
    arguments: &EngineUpArguments,
    breaks: &Breaks,
) -> Result<(ProductionEngine, InstalledFirewall), Failure> {
    let gateway = install_firewall(breaks)?;
    stage("firewall");
    // The Internet gate (A5) exists before any container can: closed unless
    // the daemon asked for it open.
    internet::set(arguments.internet_open).map_err(|message| {
        failure(
            FailureKind::Firewall,
            format!("marsh: the Internet gate could not be installed: {message}"),
        )
    })?;
    // The same call wrote the engine configuration (the registry allowlist
    // follows the gate): one place decides both.
    stage("internet-gate");
    stage("engine-config");
    if breaks.active("engine-cgroup-no-delegate") {
        // Defect: the production engine gets no controllers, so a container
        // created with a memory, CPU or PIDs limit (every container the
        // policy creates) cannot start.
        stage("cgroups-skipped");
    } else {
        delegate_cgroups().map_err(|message| failure(FailureKind::Cgroup, message))?;
        stage("cgroups");
    }
    make_loop_nodes()?;
    stage("loop-nodes");
    if breaks.active("slow-pool") {
        std::thread::sleep(Duration::from_secs(8));
    }
    let existing = Path::new(POOL_DIR).join(POOL_IMAGE).exists();
    if existing && breaks.active("pool-skip-on-start") {
        // Defect: on a restart the pool is not mounted, so the production
        // engine's data-root lands on the container's own filesystem.
        stage("pool-skipped");
    } else {
        ensure_pool_image(arguments.pool_bytes, breaks)?;
        stage("pool-image");
        mount_pool()?;
        verify_pool()?;
        stage("pool-mounted");
    }
    write_daemon_json(breaks)?;
    stage("daemon-json");
    // TODO(E1): the relay agent that serves the filtered sockets runs from here
    // (or next to it). Once it listens it must call
    // `Services::restore_sockets()` exactly once, to serve the substituted
    // socket paths that `restore_pins` reserves below. (The relay already
    // applies the engine's configuration: `relay::production_services`.)
    restore_pins().map_err(|message| setup(format!("marsh: bind pins: {message}")))?;
    stage("pins");
    let mut engine = start_production_engine()?;
    stage("dockerd-started");
    match firewall_report(gateway, breaks) {
        Ok(firewall) => Ok((engine, firewall)),
        Err(failure) => {
            engine.stop();
            Err(failure)
        }
    }
}

fn run_checked(program: &str, arguments: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(arguments)
        .output()
        .map_err(|error| format!("{program}: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "{program} {} failed ({}): {}",
            arguments.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn make_loop_nodes() -> Result<(), Failure> {
    // The container's /dev has no loop nodes; the kernel has the driver.
    if !Path::new("/dev/loop-control").exists() {
        run_checked("mknod", &["/dev/loop-control", "c", "10", "237"]).map_err(setup)?;
    }
    for index in 0..LOOP_NODES {
        let node = format!("/dev/loop{index}");
        if !Path::new(&node).exists() {
            run_checked("mknod", &[&node, "b", "7", &index.to_string()]).map_err(setup)?;
        }
    }
    Ok(())
}

/// Identity record written beside the pool image when it is created.
#[derive(Debug, Deserialize, Serialize)]
struct PoolMeta {
    schema: String,
    /// The XFS filesystem UUID chosen at `mkfs` time.
    uuid: String,
    size_bytes: u64,
}

const POOL_META_SCHEMA: &str = "marsh.svc-pool/v1";

fn available_bytes(directory: &str) -> Result<u64, String> {
    let output = run_checked("stat", &["-f", "-c", "%a %S", directory])?;
    let mut fields = output.split_whitespace();
    let blocks: u64 = fields
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or("cannot read free blocks")?;
    let size: u64 = fields
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or("cannot read block size")?;
    Ok(blocks.saturating_mul(size))
}

/// Decide whether the Docker disk can hold a new pool, before allocating
/// anything.
///
/// # Errors
/// A `marsh:` message naming the sizes when it cannot.
pub fn check_pool_fits(pool_bytes: u64, available: u64) -> Result<(), String> {
    let needed = pool_bytes.saturating_add(DISK_RESERVE_BYTES);
    if available < needed {
        return Err(format!(
            "marsh: the services disk has {} MiB free but a {} MiB pool needs {} MiB \
             (including a {} MiB reserve); raise services.diskSize or lower services.poolSize",
            available >> 20,
            pool_bytes >> 20,
            needed >> 20,
            DISK_RESERVE_BYTES >> 20
        ));
    }
    Ok(())
}

fn ensure_pool_image(pool_bytes: u64, breaks: &Breaks) -> Result<(), Failure> {
    let directory = Path::new(POOL_DIR);
    let image = directory.join(POOL_IMAGE);
    let meta = directory.join(POOL_META);
    let temporary = directory.join(format!("{POOL_IMAGE}.new"));
    let temporary_meta = directory.join(format!("{POOL_META}.new"));
    if image.exists() {
        if !meta.exists() {
            return Err(failure(
                FailureKind::PoolIdentity,
                "marsh: the pool image has no identity record; refusing to use or replace it",
            ));
        }
        return Ok(());
    }
    // First use, or a creation that did not finish (the image is renamed into
    // place last, so nothing under these names holds user data).
    for stale in [&temporary, &temporary_meta, &meta] {
        let _ = fs::remove_file(stale);
    }
    if breaks.active("pool-no-size-check") {
        // Defect: no space check, and the image is sparse, so the pool is
        // accepted even when the disk cannot hold it.
        run_checked(
            "truncate",
            &["-s", &pool_bytes.to_string(), &temporary.to_string_lossy()],
        )
        .map_err(setup)?;
    } else {
        let available = available_bytes(POOL_DIR).map_err(setup)?;
        check_pool_fits(pool_bytes, available)
            .map_err(|message| failure(FailureKind::DiskTooSmall, message))?;
        if let Err(message) = run_checked(
            "fallocate",
            &["-l", &pool_bytes.to_string(), &temporary.to_string_lossy()],
        ) {
            let _ = fs::remove_file(&temporary);
            return Err(failure(
                FailureKind::DiskTooSmall,
                format!("marsh: cannot allocate the pool (disk full?): {message}"),
            ));
        }
    }
    let uuid = fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map_err(|error| setup(format!("cannot read a uuid: {error}")))?
        .trim()
        .to_owned();
    let formatted = run_checked(
        "mkfs.xfs",
        &[
            "-q",
            "-m",
            "reflink=1",
            "-m",
            &format!("uuid={uuid}"),
            &temporary.to_string_lossy(),
        ],
    );
    if let Err(message) = formatted {
        let _ = fs::remove_file(&temporary);
        return Err(setup(message));
    }
    let record = PoolMeta {
        schema: POOL_META_SCHEMA.to_owned(),
        uuid,
        size_bytes: pool_bytes,
    };
    let encoded = serde_json::to_vec(&record).map_err(|error| setup(error.to_string()))?;
    fs::write(&temporary_meta, encoded).map_err(|error| setup(error.to_string()))?;
    fs::rename(&temporary_meta, &meta).map_err(|error| setup(error.to_string()))?;
    fs::rename(&temporary, &image).map_err(|error| setup(error.to_string()))?;
    Ok(())
}

fn mount_pool() -> Result<(), Failure> {
    fs::create_dir_all(POOL_MOUNT).map_err(|error| setup(error.to_string()))?;
    if pool_mount().is_some() {
        return Ok(());
    }
    let image = Path::new(POOL_DIR).join(POOL_IMAGE);
    run_checked(
        "mount",
        &[
            "-t",
            "xfs",
            "-o",
            "loop,noatime",
            &image.to_string_lossy(),
            POOL_MOUNT,
        ],
    )
    .map(|_| ())
    .map_err(|message| {
        failure(
            FailureKind::PoolUnusable,
            format!("marsh: cannot mount the pool: {message}"),
        )
    })
}

/// The mount of the pool in a `/proc/self/mountinfo` text: `(fstype, source)`.
#[must_use]
pub fn pool_mount_in(mountinfo: &str) -> Option<(String, String)> {
    // The last mount at the point wins, like the kernel's view.
    mountinfo
        .lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            if before.split_whitespace().nth(4)? != POOL_MOUNT {
                return None;
            }
            let mut rest = after.split_whitespace();
            let fstype = rest.next()?.to_owned();
            let source = rest.next()?.to_owned();
            Some((fstype, source))
        })
        .next_back()
}

fn pool_mount() -> Option<(String, String)> {
    pool_mount_in(&fs::read_to_string("/proc/self/mountinfo").ok()?)
}

/// Pool identity, type and reflink (step 2 of A1).
fn verify_pool() -> Result<(), Failure> {
    let (fstype, _) = pool_mount().ok_or_else(|| {
        failure(
            FailureKind::PoolUnusable,
            "marsh: the pool is not mounted after mount",
        )
    })?;
    if fstype != "xfs" {
        return Err(failure(
            FailureKind::PoolUnusable,
            format!("marsh: the pool is {fstype}, not xfs"),
        ));
    }
    let meta_text = fs::read_to_string(Path::new(POOL_DIR).join(POOL_META)).map_err(|error| {
        failure(
            FailureKind::PoolIdentity,
            format!("marsh: no pool identity: {error}"),
        )
    })?;
    let meta: PoolMeta = serde_json::from_str(&meta_text).map_err(|error| {
        failure(
            FailureKind::PoolIdentity,
            format!("marsh: bad pool identity: {error}"),
        )
    })?;
    if meta.schema != POOL_META_SCHEMA {
        return Err(failure(
            FailureKind::PoolIdentity,
            "marsh: unknown pool identity schema",
        ));
    }
    let actual = run_checked("findmnt", &["-n", "-o", "UUID", POOL_MOUNT])
        .map_err(|message| failure(FailureKind::PoolIdentity, message))?;
    if actual.trim() != meta.uuid {
        return Err(failure(
            FailureKind::PoolIdentity,
            format!(
                "marsh: the mounted pool is {} but this VM created {}; refusing it",
                actual.trim(),
                meta.uuid
            ),
        ));
    }
    let first = format!("{POOL_MOUNT}/.marsh-reflink-a");
    let second = format!("{POOL_MOUNT}/.marsh-reflink-b");
    let probe = fs::write(&first, b"probe")
        .map_err(|error| error.to_string())
        .and_then(|()| run_checked("cp", &["--reflink=always", &first, &second]).map(|_| ()));
    let _ = fs::remove_file(&first);
    let _ = fs::remove_file(&second);
    probe.map_err(|message| {
        failure(
            FailureKind::PoolUnusable,
            format!("marsh: the pool cannot reflink: {message}"),
        )
    })
}

/// The hardened production `daemon.json`. `BuildKit` options travel over gRPC,
/// which the filter cannot read, so host-network and insecure builds are
/// refused by the engine itself. The other settings are A5/A7 defaults that
/// dockerd supports: published ports bind to loopback (`ip`; the filter forces
/// loopback too, this is the second layer for anything that reaches the engine
/// without it), bounded logs, and bounded concurrency and shared memory.
/// `ip_any` is the `engine-ip-any` defect: dockerd's own default, every address.
#[must_use]
pub fn daemon_json(entitlements_on: bool, ip_any: bool) -> serde_json::Value {
    serde_json::json!({
        "builder": {
            "entitlements": {
                "network-host": entitlements_on,
                "security-insecure": entitlements_on,
            }
        },
        "ip": if ip_any { "0.0.0.0" } else { "127.0.0.1" },
        "log-driver": "json-file",
        "log-opts": {"max-size": "10m", "max-file": "3"},
        "max-concurrent-downloads": 3,
        "max-concurrent-uploads": 3,
        "default-shm-size": "64M",
        "default-ulimits": {"nofile": {"Name": "nofile", "Soft": 65536, "Hard": 65536}},
    })
}

fn write_daemon_json(breaks: &Breaks) -> Result<(), Failure> {
    let encoded = serde_json::to_vec_pretty(&daemon_json(
        breaks.active("entitlements-on"),
        breaks.active("engine-ip-any"),
    ))
    .map_err(|error| setup(error.to_string()))?;
    fs::create_dir_all("/etc/docker").map_err(|error| setup(error.to_string()))?;
    let temporary = format!("{DAEMON_JSON}.new");
    fs::write(&temporary, encoded).map_err(|error| setup(error.to_string()))?;
    fs::rename(&temporary, DAEMON_JSON).map_err(|error| setup(error.to_string()))
}

const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// Controllers the production engine can use to limit a container.
const DELEGATED_CONTROLLERS: [&str; 6] = ["cpu", "cpuset", "io", "memory", "pids", "hugetlb"];
/// Controllers the policy's default limits need: memory, CPU and PIDs.
const REQUIRED_CONTROLLERS: [&str; 3] = ["cpu", "memory", "pids"];
const DELEGATE_TRIES: u32 = 40;
const DELEGATE_PAUSE: Duration = Duration::from_millis(250);

/// The `cgroup.subtree_control` line that delegates the controllers the engine
/// can use out of the ones this cgroup has (`cgroup.controllers`).
///
/// # Errors
/// A controller the policy's default limits need is not available.
pub fn subtree_control_line(available: &str) -> Result<String, String> {
    let have: Vec<&str> = available.split_whitespace().collect();
    if let Some(missing) = REQUIRED_CONTROLLERS
        .iter()
        .find(|controller| !have.contains(controller))
    {
        return Err(format!(
            "marsh: the {missing} cgroup controller is not available to the engine container (has: {})",
            have.join(" ")
        ));
    }
    Ok(DELEGATED_CONTROLLERS
        .iter()
        .filter(|controller| have.contains(controller))
        .map(|controller| format!("+{controller}"))
        .collect::<Vec<_>>()
        .join(" "))
}

/// Whether `/proc/self/cgroup` says this process is at the root of its own
/// cgroup namespace (`0::/`): the engine container's cgroup tree is private.
#[must_use]
pub fn in_private_cgroup_namespace(proc_self_cgroup: &str) -> bool {
    proc_self_cgroup.trim() == "0::/"
}

/// Whether `/sys/fs/cgroup` is a cgroup2 mount in a `/proc/self/mountinfo` text.
#[must_use]
pub fn cgroup2_mounted_in(mountinfo: &str) -> bool {
    mountinfo.lines().any(|line| {
        line.split_once(" - ").is_some_and(|(before, after)| {
            before.split_whitespace().nth(4) == Some(CGROUP_ROOT)
                && after.split_whitespace().next() == Some("cgroup2")
        })
    })
}

/// Delegate the cgroup v2 controllers to the production engine, as the
/// `docker:dind` entrypoint does: dockerd creates a cgroup per container under
/// this container's cgroup, and without the controllers in
/// `cgroup.subtree_control` every container with a memory, CPU or PIDs limit
/// fails to start. A cgroup that has processes cannot enable controllers for
/// its children, so every process (this one included) first moves into a
/// nested `init` cgroup.
///
/// The container must have a private cgroup namespace (`--cgroupns private`):
/// then `/sys/fs/cgroup` is this container's own cgroup and moving its
/// processes touches nothing else. Anywhere else this refuses, and moves
/// nothing.
fn delegate_cgroups() -> Result<(), String> {
    let own = fs::read_to_string("/proc/self/cgroup").map_err(|error| error.to_string())?;
    if !in_private_cgroup_namespace(&own) {
        return Err(format!(
            "marsh: the engine container is not in a private cgroup namespace (/proc/self/cgroup is {:?}); refusing to rearrange cgroups",
            own.trim()
        ));
    }
    let mountinfo =
        fs::read_to_string("/proc/self/mountinfo").map_err(|error| error.to_string())?;
    if !cgroup2_mounted_in(&mountinfo) {
        return Err(format!(
            "marsh: {CGROUP_ROOT} is not a cgroup2 mount in the engine container"
        ));
    }
    let root = Path::new(CGROUP_ROOT);
    let line = subtree_control_line(
        &fs::read_to_string(root.join("cgroup.controllers"))
            .map_err(|error| format!("cgroup.controllers: {error}"))?,
    )?;
    let init = root.join("init");
    fs::create_dir_all(&init)
        .map_err(|error| format!("marsh: cannot create the init cgroup: {error}"))?;
    // The move and the write race with processes this one starts, and the
    // kernel answers EBUSY until the root cgroup is empty: retry both.
    let mut last = String::new();
    for _ in 0..DELEGATE_TRIES {
        for pid in fs::read_to_string(root.join("cgroup.procs"))
            .unwrap_or_default()
            .split_whitespace()
        {
            // A process that exited in between is not an error.
            let _ = fs::write(init.join("cgroup.procs"), pid);
        }
        match fs::write(root.join("cgroup.subtree_control"), &line) {
            Ok(()) => {
                let enabled =
                    fs::read_to_string(root.join("cgroup.subtree_control")).unwrap_or_default();
                return match REQUIRED_CONTROLLERS
                    .iter()
                    .find(|controller| !enabled.split_whitespace().any(|have| have == **controller))
                {
                    None => Ok(()),
                    Some(missing) => Err(format!(
                        "marsh: the {missing} controller is not enabled in cgroup.subtree_control after delegation ({})",
                        enabled.trim()
                    )),
                };
            }
            Err(error) => last = error.to_string(),
        }
        std::thread::sleep(DELEGATE_PAUSE);
    }
    Err(format!(
        "marsh: cannot delegate cgroup controllers ({line}): {last}"
    ))
}

/// Recreate the bind pins and reserve the substituted-socket paths (E3, A3),
/// before the production engine starts, so that readiness is never announced
/// with a pin missing.
///
/// - `Err` means restoration itself could not run (the ledger cannot be read
///   or written, a socket path cannot be reserved): the engine is not started
///   and the boot fails.
/// - A pin that cannot be recreated does not fail the boot (the other
///   workspaces' work must survive). It is left as a dangling symlink, so a
///   `docker start` of a container that holds it fails instead of creating an
///   empty directory, and its containers are refused at admission
///   (`Env::container_blocked`). Each one is reported on stderr.
///
/// TODO(E1): the relay agent that serves the filtered sockets must call
/// `Services::restore_sockets()` once it listens, to serve the paths reserved
/// here again. Until then a container that binds one sees a refused connection.
fn restore_pins() -> Result<(), String> {
    let report = crate::pins::restore_all().map_err(|error| error.to_string())?;
    eprintln!(
        "marsh: restored {} bind pins, {} socket paths reserved",
        report.restored.len(),
        report.sockets_reserved
    );
    for (id, why) in &report.failed {
        eprintln!("marsh: bind pin {id} could not be restored: {why}");
    }
    for container in &report.blocked_containers {
        eprintln!("marsh: container {container} will not start (a bind pin is missing)");
    }
    Ok(())
}

fn start_production_engine() -> Result<ProductionEngine, Failure> {
    fs::create_dir_all("/run/marsh-svc").map_err(|error| setup(error.to_string()))?;
    fs::create_dir_all(DATA_ROOT).map_err(|error| setup(error.to_string()))?;
    let _ = fs::remove_file(ENGINE_SOCKET);
    let log = fs::File::create("/var/log/prod-dockerd.log")
        .map_err(|error| setup(format!("cannot open the dockerd log: {error}")))?;
    let log_error = log.try_clone().map_err(|error| setup(error.to_string()))?;
    let child = Command::new("dockerd")
        .args([
            "--config-file",
            DAEMON_JSON,
            "--data-root",
            DATA_ROOT,
            "--host",
            &format!("unix://{ENGINE_SOCKET}"),
            "--pidfile",
            "/run/marsh-svc/dockerd.pid",
            "--exec-root",
            "/run/marsh-svc/exec",
        ])
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_error)
        .spawn()
        .map_err(|error| setup(format!("cannot start dockerd: {error}")))?;
    let mut engine = ProductionEngine { child };
    let deadline = Instant::now() + PING_TIMEOUT;
    loop {
        if ping() {
            return Ok(engine);
        }
        if let Ok(Some(status)) = engine.child.try_wait() {
            return Err(setup(format!(
                "marsh: the production engine exited ({status}): {}",
                log_tail()
            )));
        }
        if Instant::now() >= deadline {
            engine.stop();
            return Err(setup(format!(
                "marsh: the production engine did not answer /_ping: {}",
                log_tail()
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn log_tail() -> String {
    let text = fs::read_to_string("/var/log/prod-dockerd.log").unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(5)..].join(" | ")
}

fn ping() -> bool {
    let Ok(mut stream) = UnixStream::connect(ENGINE_SOCKET) else {
        return false;
    };
    let timeout = Some(Duration::from_secs(2));
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);
    if stream
        .write_all(b"GET /_ping HTTP/1.0\r\nHost: docker\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut response = Vec::new();
    let _ = stream.take(4096).read_to_end(&mut response);
    ping_ok(&response)
}

/// Whether an HTTP response to `GET /_ping` is the engine's `200 OK`.
#[must_use]
pub fn ping_ok(response: &[u8]) -> bool {
    let text = String::from_utf8_lossy(response);
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return false;
    };
    let status = head.lines().next().unwrap_or_default();
    status.split_whitespace().nth(1) == Some("200") && body.trim() == "OK"
}

/// This engine's generation: the VM boot id and this container's id.
///
/// # Errors
/// Returns a message if the boot id or the container id cannot be read.
pub fn generation() -> Result<Generation, String> {
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|error| format!("cannot read the boot id: {error}"))?
        .trim()
        .to_owned();
    let mountinfo = fs::read_to_string("/proc/self/mountinfo")
        .map_err(|error| format!("cannot read mountinfo: {error}"))?;
    let engine_id = container_id_in(&mountinfo)
        .ok_or("cannot determine the engine container id from mountinfo")?;
    Ok(Generation { boot_id, engine_id })
}

/// The container id from the `/containers/<id>/hostname` bind that the boot
/// engine always mounts into a container.
#[must_use]
pub fn container_id_in(mountinfo: &str) -> Option<String> {
    mountinfo.lines().find_map(|line| {
        let root = line.split_whitespace().nth(3)?;
        let rest = root.split("/containers/").nth(1)?;
        let id = rest.split('/').next()?;
        (id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then(|| id.to_ascii_lowercase())
    })
}

/// A `set` records its state after the rules, so one disagreeing read can be a
/// change in flight; two in a row are a lost gate. The engine configuration
/// (the registry allowlist) must agree with the gate too.
fn gate_lost(mismatches: &mut u32) -> Option<String> {
    gate_verdict(mismatches, internet::recorded(), |intended| {
        internet::verify(intended).and_then(|()| internet::verify_config(intended))
    })
}

/// `recorded` is the state file's content. A missing or unreadable file is a
/// mismatch like any other: `set` writes it before supervision starts and only
/// root can remove it, so its absence means something else touched the gate.
fn gate_verdict(
    mismatches: &mut u32,
    recorded: Option<bool>,
    verify: impl FnOnce(bool) -> Result<(), String>,
) -> Option<String> {
    let result = recorded
        .ok_or_else(|| "the recorded Internet gate state is missing or unreadable".to_owned())
        .and_then(verify);
    match result {
        Ok(()) => {
            *mismatches = 0;
            None
        }
        Err(message) => {
            *mismatches += 1;
            (*mismatches >= 2).then_some(message)
        }
    }
}

fn supervise(
    engine: &mut ProductionEngine,
    terminate: &AtomicBool,
    breaks: &Breaks,
    gateway: &Gateway,
) -> i32 {
    let mut ticks = 0_u32;
    let mut gate_mismatches = 0_u32;
    loop {
        if terminate.load(Ordering::Relaxed) {
            emit(&EngineEvent::Stopping);
            engine.stop();
            if !breaks.active("pool-skip-on-start") {
                let _ = Command::new("umount").arg(POOL_MOUNT).status();
            }
            return 0;
        }
        if let Ok(Some(status)) = engine.child.try_wait() {
            emit(&EngineEvent::Lost {
                reason: format!("the production engine exited ({status})"),
            });
            return 1;
        }
        let skipped = breaks.active("pool-skip-on-start");
        if !skipped && !matches!(pool_mount(), Some((ref fstype, _)) if fstype == "xfs") {
            emit(&EngineEvent::Lost {
                reason: "the pool is no longer mounted".to_owned(),
            });
            engine.stop();
            return 1;
        }
        ticks = ticks.wrapping_add(1);
        if !breaks.active("engine-firewall-off") {
            if ticks.is_multiple_of(FIREWALL_EVERY)
                && let Err(message) = firewall::verify(gateway)
            {
                emit(&EngineEvent::Lost {
                    reason: format!("the egress firewall is gone: {message}"),
                });
                engine.stop();
                return 1;
            }
            if ticks.is_multiple_of(PING_EVERY)
                && let Some(address) = firewall::gateway_moved(gateway)
            {
                emit(&EngineEvent::Lost {
                    reason: format!(
                        "the sbx gateway now resolves to {address}, which the egress firewall does not cover"
                    ),
                });
                engine.stop();
                return 1;
            }
        }
        if ticks.is_multiple_of(FIREWALL_EVERY)
            && let Some(message) = gate_lost(&mut gate_mismatches)
        {
            emit(&EngineEvent::Lost {
                reason: format!("the Internet gate is gone: {message}"),
            });
            engine.stop();
            return 1;
        }
        if ticks.is_multiple_of(PING_EVERY) && !ping() {
            emit(&EngineEvent::Lost {
                reason: "the production engine stopped answering /_ping".to_owned(),
            });
            engine.stop();
            return 1;
        }
        std::thread::sleep(SUPERVISE_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardened_config_denies_both_entitlements() {
        let config = daemon_json(false, false);
        assert_eq!(config["builder"]["entitlements"]["network-host"], false);
        assert_eq!(
            config["builder"]["entitlements"]["security-insecure"],
            false
        );
        assert_eq!(config["ip"], "127.0.0.1");
        assert_eq!(daemon_json(false, true)["ip"], "0.0.0.0");
    }

    #[test]
    fn the_registry_allowlist_follows_the_internet_gate() {
        let closed = EngineConfig::for_internet(false);
        assert_eq!(
            closed.registry_allow,
            Some(vec!["docker.io".to_owned(), "ghcr.io".to_owned()])
        );
        assert_eq!(EngineConfig::default(), closed);
        assert_eq!(EngineConfig::for_internet(true).registry_allow, None);
        // engine-up: closed unless asked open, and no other registry argument.
        let base = ["--pool-bytes".to_owned(), (2_u64 << 30).to_string()];
        assert!(!parse_arguments(&base).unwrap().internet_open);
        let open = [base.as_slice(), &["--internet".to_owned(), "on".to_owned()]].concat();
        assert!(parse_arguments(&open).unwrap().internet_open);
        let bad = [
            base.as_slice(),
            &["--internet".to_owned(), "maybe".to_owned()],
        ]
        .concat();
        assert!(parse_arguments(&bad).is_err());
        let old = [
            base.as_slice(),
            &["--registry-allow".to_owned(), "*".to_owned()],
        ]
        .concat();
        assert!(parse_arguments(&old).is_err());
        let mut config = crate::policy::endpoint::Config::default();
        EngineConfig::for_internet(false).apply(&mut config);
        assert!(config.registry_allow.is_some());
    }

    #[test]
    fn a_missing_gate_state_is_a_lost_gate_after_two_looks() {
        let mut mismatches = 0;
        assert!(gate_verdict(&mut mismatches, None, |_| Ok(())).is_none());
        let message = gate_verdict(&mut mismatches, None, |_| Ok(())).unwrap();
        assert!(message.contains("missing"), "{message}");
        let mut mismatches = 0;
        assert!(gate_verdict(&mut mismatches, Some(false), |_| Ok(())).is_none());
        assert_eq!(mismatches, 0);
    }

    #[test]
    fn delegation_needs_memory_cpu_and_pids() {
        assert_eq!(
            subtree_control_line("cpuset cpu io memory hugetlb pids rdma misc").unwrap(),
            "+cpu +cpuset +io +memory +pids +hugetlb"
        );
        assert_eq!(
            subtree_control_line("cpu memory pids").unwrap(),
            "+cpu +memory +pids"
        );
        for missing in ["cpu", "memory", "pids"] {
            let available: Vec<&str> = ["cpu", "io", "memory", "pids"]
                .into_iter()
                .filter(|controller| *controller != missing)
                .collect();
            let error = subtree_control_line(&available.join(" ")).unwrap_err();
            assert!(error.contains(missing), "{error}");
        }
        assert!(subtree_control_line("").is_err());
    }

    #[test]
    fn cgroups_are_only_rearranged_in_a_private_namespace() {
        assert!(in_private_cgroup_namespace("0::/\n"));
        assert!(!in_private_cgroup_namespace("0::/docker/abc\n"));
        assert!(!in_private_cgroup_namespace("12:memory:/\n0::/\n"));
        assert!(cgroup2_mounted_in(
            "30 25 0:27 / /sys/fs/cgroup rw,nosuid - cgroup2 cgroup rw\n"
        ));
        assert!(!cgroup2_mounted_in(
            "30 25 0:27 / /sys/fs/cgroup ro - tmpfs tmpfs rw\n"
        ));
        assert!(!cgroup2_mounted_in(
            "30 25 0:27 / /other rw - cgroup2 cgroup rw\n"
        ));
    }

    #[test]
    fn small_disk_is_refused_with_a_clear_message() {
        let error = check_pool_fits(20 << 30, 8 << 30).unwrap_err();
        assert!(error.starts_with("marsh: "), "{error}");
        assert!(error.contains("free"), "{error}");
        assert!(check_pool_fits(4 << 30, 8 << 30).is_ok());
    }

    #[test]
    fn only_a_real_ready_event_is_ready() {
        let line =
            r#"{"event":"ready","generation":{"boot_id":"b","engine_id":"e"},"engine_ready_ms":5}"#;
        assert!(matches!(
            EngineEvent::parse(line),
            Some(EngineEvent::Ready { .. })
        ));
        assert_eq!(EngineEvent::parse("ready"), None);
        assert_eq!(EngineEvent::parse(r#"{"event":"unknown"}"#), None);
    }

    #[test]
    fn container_id_comes_from_the_hostname_bind() {
        let id = "a".repeat(64);
        let info = format!(
            "1 2 0:1 / / rw - overlay overlay rw\n3 2 8:1 /containers/{id}/hostname /etc/hostname rw - ext4 /dev/vdd rw\n"
        );
        assert_eq!(container_id_in(&info), Some(id));
    }

    #[test]
    fn pool_mount_must_be_at_the_pool() {
        let info = "10 1 7:0 / /pool rw,noatime - xfs /dev/loop0 rw\n11 1 0:5 / /other rw - ext4 /dev/x rw\n";
        assert_eq!(
            pool_mount_in(info),
            Some(("xfs".to_owned(), "/dev/loop0".to_owned()))
        );
        assert_eq!(
            pool_mount_in("11 1 0:5 / /other rw - ext4 /dev/x rw\n"),
            None
        );
    }
}
