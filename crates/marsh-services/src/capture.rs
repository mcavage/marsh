//! Per-workspace capture and drop of volume snapshots (S3).
//!
//! Design: SECURITY-DESIGN A4 (a capture pauses and copies only its own
//! workspace), A7 (bounds) and A8 (opaque ids, hostile input); ENG-PLAN D3.
//!
//! A capture of workspace `W`, as snapshot id `ID`:
//!
//! 1. take the workspace's operation lock and its API gate (exclusive), so
//!    drops, restores, other captures (in this or another agent process) and
//!    the endpoint's mutating requests wait (`locks::ScopeLocks`);
//! 2. list the engine's containers and volumes, keeping only objects whose
//!    `dev.marsh.scope` label equals `W`; labels select objects, they are never
//!    paths, and engine names are kept exact internally (display text is only
//!    for presentation);
//! 3. stage in `pool/snapshots/.tmp-ID`: one directory per volume under a
//!    fresh opaque id, the freeze journal and heartbeat (`freeze`), and start
//!    the independent watchdog process, whose liveness the capture checks;
//! 4. pause the workspace's running containers concurrently, clone every
//!    volume (reflink) within the freeze budget, then thaw until the engine
//!    confirms each container;
//! 5. if the watchdog or recovery revoked the freeze at any point, discard the
//!    capture; otherwise write `manifest.json` and publish with an atomic
//!    rename to `pool/snapshots/ID`.
//!
//! A snapshot that exists is complete as far as its manifest says: a volume
//! that could not be saved (over budget, unsupported, in the way) is listed
//! with `captured: false` and is absent from the directory. A capture that
//! fails or is interrupted leaves no snapshot, only a gap.
// The agent only runs on Linux (inside the engine container); on other hosts
// the Linux-only half is compiled out and what remains is not used.
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

use crate::engine_api::{ContainerSummary, Engine, SCOPE_LABEL, valid_container_id};
use crate::frame::{SnapshotInfo, validate_label};
use crate::locks::{Freezer, ScopeLocks};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

pub const MAX_SNAPSHOT_ID_BYTES: usize = 64;
pub const MAX_PAUSED_CONTAINERS: usize = 256;
pub const MAX_VOLUMES: usize = 256;
const MAX_TEXT: usize = 160;
/// Compose labels carry absolute paths, which are longer than names.
const MAX_PATH_TEXT: usize = 1024;
const MAX_GAPS: usize = 64;
/// The gap of a capture skipped because the pool is below its reserve (the text
/// marsh's checkpoints show: DECISIONS 5).
pub const NO_SPACE_GAP: &str = "not enough space for app data";
/// How long a fence waits for the mutations already admitted to a workspace.
const FENCE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// A host-supplied snapshot id must be one bounded path component of
/// `[A-Za-z0-9_-]`, not starting with `-`. (No dots: it can never be `.`, `..`
/// or a hidden staging name.)
#[must_use]
pub fn valid_snapshot_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_SNAPSHOT_ID_BYTES
        && bytes[0] != b'-'
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// Engine-provided text made safe to show and bounded in size. Display only:
/// two different names can look the same afterwards, so never use the result
/// as an identity.
#[must_use]
pub fn sanitize(text: &str, max: usize) -> String {
    let mut out: String = text
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .take(max)
        .collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

#[derive(Debug)]
pub enum CaptureError {
    /// The request itself is invalid (a bad id, a duplicate).
    Invalid(String),
    /// The capture could not run; the message is for logs and gaps.
    Failed(String),
    /// Containers could not be confirmed thawed. The staging directory (and its
    /// journal) must stay so the watchdog and recovery can finish the job.
    Stuck(String),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(why) | Self::Failed(why) | Self::Stuck(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for CaptureError {}

impl From<std::io::Error> for CaptureError {
    fn from(error: std::io::Error) -> Self {
        Self::Failed(error.to_string())
    }
}

impl From<crate::engine_api::ApiError> for CaptureError {
    fn from(error: crate::engine_api::ApiError) -> Self {
        Self::Failed(error.to_string())
    }
}

// ---------------------------------------------------------------- manifest

#[derive(Debug, Clone, Serialize)]
pub struct VolumeEntry {
    /// Opaque id: the directory name under `snapshots/<id>/`.
    pub id: String,
    /// The engine's volume name, for display only (bounded, control
    /// characters replaced).
    pub name: String,
    pub captured: bool,
    pub files: u64,
    /// Display data (reserved `dev.marsh.*` labels omitted).
    pub labels: BTreeMap<String, String>,
    pub gaps: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContainerEntry {
    pub name: String,
    pub service: Option<String>,
    pub image: String,
    pub running: bool,
    /// Opaque ids of the captured-list volumes this container mounts.
    pub volumes: Vec<String>,
    /// A Testcontainers object (reaped by its own Ryuk): restore never
    /// recreates it (S4).
    pub testcontainers: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StackEntry {
    /// `compose` or `supabase` (grouping is from labels; display only).
    pub kind: String,
    pub project: String,
    /// The compose project's working directory and config files as the
    /// containers' labels reported them (display data, bounded). Restore (S4)
    /// only ever uses them as relative components beneath the lane folder.
    pub working_dir: Option<String>,
    pub config_files: Vec<String>,
    pub containers: Vec<ContainerEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageEntry {
    pub image: String,
    pub image_id: String,
    pub digests: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Manifest {
    pub version: u32,
    pub id: String,
    /// The exact workspace string (validated, at most 256 bytes): it is the
    /// lock identity, so it is never truncated.
    pub workspace: String,
    pub created_unix_ms: u64,
    /// True only when every volume was saved and nothing was skipped.
    pub complete: bool,
    /// The freeze window: first `pause` to last `unpause` event of the
    /// capture's containers, as the engine timestamped them.
    pub frozen_ms: u64,
    /// `engine-events`, or `calls` when the events were not available (then
    /// `frozen_ms` is the wall time around the API calls, an upper bound).
    pub frozen_source: String,
    /// Wall time around the pause and unpause calls (an upper bound).
    pub frozen_calls_ms: u64,
    pub pause_ms: u64,
    pub copy_ms: u64,
    pub total_ms: u64,
    pub volumes: Vec<VolumeEntry>,
    pub stacks: Vec<StackEntry>,
    pub standalone: Vec<ContainerEntry>,
    pub images: Vec<ImageEntry>,
    pub gaps: Vec<String>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Outcome {
    Published,
    Skipped,
    Failed,
}

impl Outcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug)]
pub struct CaptureReport {
    pub outcome: Outcome,
    pub id: String,
    pub manifest: Option<Manifest>,
    pub gaps: Vec<String>,
}

impl CaptureReport {
    fn without_snapshot(outcome: Outcome, id: &str, gap: String) -> Self {
        Self {
            outcome,
            id: id.to_owned(),
            manifest: None,
            gaps: vec![gap],
        }
    }
}

/// Tunables (clamped by the relay from the daemon's settings).
#[derive(Clone, Debug)]
pub struct CaptureConfig {
    /// Longest the workspace may stay frozen while copying.
    pub freeze_budget: Duration,
    /// Thaw if the capture shows no real progress for this long.
    pub watchdog: Duration,
    /// How long a capture waits for the workspace's locks.
    pub lock_wait: Duration,
    pub workers: usize,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            freeze_budget: Duration::from_millis(500),
            watchdog: Duration::from_millis(3000),
            lock_wait: Duration::from_millis(500),
            workers: 4,
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::{Capturer, Recovery};

#[cfg(target_os = "linux")]
mod linux {
    use super::{
        CaptureConfig, CaptureError, CaptureReport, ContainerEntry, ContainerSummary, Engine,
        FENCE_WAIT, Freezer, ImageEntry, MAX_GAPS, MAX_PATH_TEXT, MAX_PAUSED_CONTAINERS, MAX_TEXT,
        MAX_VOLUMES, Manifest, NO_SPACE_GAP, Outcome, SCOPE_LABEL, ScopeLocks, SnapshotInfo,
        StackEntry, VolumeEntry, sanitize, valid_container_id, valid_snapshot_id, validate_label,
    };
    use crate::breaks::{break_mode, hold};
    use crate::freeze::{self, ALIVE, INTENT, Intent, JOURNAL};
    use crate::fsops::{self, Copier, Pool, VolumeCopy};
    use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
    use std::ffi::CString;
    use std::fs::File;
    use std::io::BufRead;
    use std::os::fd::OwnedFd;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Child;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const MANIFEST_NAME: &str = "manifest.json";
    const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
    /// After the freeze budget, how long a stuck copy gets before the capture
    /// stops waiting, thaws and fails.
    const COPY_GRACE: Duration = Duration::from_millis(250);

    fn unix_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }

    fn compose_project(container: &ContainerSummary) -> Option<(&'static str, String)> {
        if let Some(project) = container.label("com.supabase.cli.project") {
            return Some(("supabase", sanitize(project, MAX_TEXT)));
        }
        container
            .label("com.docker.compose.project")
            .map(|project| ("compose", sanitize(project, MAX_TEXT)))
    }

    fn display_labels(labels: Option<&BTreeMap<String, String>>) -> BTreeMap<String, String> {
        labels
            .into_iter()
            .flatten()
            .filter(|(key, _)| !key.starts_with("dev.marsh."))
            .take(24)
            .map(|(key, value)| (sanitize(key, 128), sanitize(value, MAX_TEXT)))
            .collect()
    }

    fn push_gap(gaps: &mut Vec<String>, text: String) {
        if gaps.len() < MAX_GAPS {
            gaps.push(text);
        } else if gaps.len() == MAX_GAPS {
            gaps.push("more problems were found and are not listed".into());
        }
    }

    /// The result of startup recovery.
    #[derive(Debug, Default)]
    pub struct Recovery {
        pub log: Vec<String>,
        /// False if any container of a dead capture could not be confirmed
        /// thawed, or a staging directory could not be judged: the agent must
        /// not report ready.
        pub settled: bool,
    }

    pub struct Capturer {
        pool: Pool,
        pool_path: PathBuf,
        engine: Engine,
        locks: Arc<ScopeLocks>,
        freezer: Arc<dyn Freezer>,
        config: CaptureConfig,
        /// The freezes of a stop cutover's fence, held until `unfence` or until
        /// the agent exits with the VM.
        fence: Mutex<Vec<Box<dyn Send>>>,
        /// The first pool reading (`space-check-stale`, a deliberate defect).
        stale_free: Mutex<Option<u64>>,
    }

    /// A volume of the workspace. `name` is the engine's exact name (the
    /// identity); `shown` is the bounded display form.
    struct Planned {
        name: String,
        shown: String,
        opaque: String,
        labels: BTreeMap<String, String>,
        /// The compose volume label, used only by the deliberately broken
        /// `snapshot-name-from-label` mode.
        compose_label: Option<String>,
        source: Option<OwnedFd>,
        gaps: Vec<String>,
    }

    /// Thaws on drop (any exit of a capture), unless the deliberately broken
    /// `capture-skip-unpause-on-error` mode removes the error-path thaw.
    struct Freeze<'a> {
        engine: &'a Engine,
        stage: &'a OwnedFd,
        armed: bool,
    }

    impl Freeze<'_> {
        fn thaw(&mut self) -> Result<Vec<String>, CaptureError> {
            self.armed = false;
            Ok(freeze::thaw(self.engine, self.stage, 30)?)
        }
    }

    impl Drop for Freeze<'_> {
        fn drop(&mut self) {
            if self.armed && !break_mode("capture-skip-unpause-on-error") {
                let _ = freeze::thaw(self.engine, self.stage, 30);
            }
        }
    }

    fn pause_concurrently(engine: &Engine, ids: &[String]) -> Vec<(String, Result<(), String>)> {
        let mut out = Vec::new();
        for chunk in ids.chunks(32) {
            let results: Vec<_> = std::thread::scope(|scope| {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|id| {
                        scope.spawn(move || {
                            (
                                id.clone(),
                                engine.pause(id).map_err(|error| error.to_string()),
                            )
                        })
                    })
                    .collect();
                handles.into_iter().filter_map(|h| h.join().ok()).collect()
            });
            out.extend(results);
        }
        out
    }

    /// Records the engine's own timestamps of the capture's pause/unpause.
    struct FreezeEvents {
        seen: Arc<Mutex<Vec<(String, String, u64)>>>,
        stop: Arc<AtomicBool>,
    }

    impl FreezeEvents {
        fn start(engine: &Engine) -> Option<Self> {
            let mut reader = engine.freeze_events().ok()?;
            let _ = reader
                .get_ref()
                .set_read_timeout(Some(Duration::from_millis(30)));
            let seen = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            {
                let seen = Arc::clone(&seen);
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    let mut line = String::new();
                    while !stop.load(Ordering::Relaxed) {
                        match reader.read_line(&mut line) {
                            Ok(0) => return,
                            Ok(_) if line.ends_with('\n') => {
                                if let Ok(event) = serde_json::from_str::<serde_json::Value>(&line)
                                    && let (Some(action), Some(id), Some(nano)) = (
                                        event["Action"].as_str(),
                                        event["Actor"]["ID"].as_str(),
                                        event["timeNano"].as_u64(),
                                    )
                                {
                                    seen.lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .push((action.to_owned(), id.to_owned(), nano));
                                }
                                line.clear();
                            }
                            Ok(_) => {}
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ) => {}
                            Err(_) => return,
                        }
                    }
                });
            }
            Some(Self { seen, stop })
        }

        /// The window in ms from the first pause to the last unpause among
        /// `ids`, once the expected events have arrived (or `wait` ran out).
        fn window_ms(&self, ids: &[String], wait: Duration) -> Option<u64> {
            let deadline = Instant::now() + wait;
            let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
            loop {
                let (pauses, unpauses, complete) = {
                    let seen = self
                        .seen
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let times = |action: &str| -> Vec<(u64, &str)> {
                        seen.iter()
                            .filter(|e| e.0 == action && wanted.contains(e.1.as_str()))
                            .map(|e| (e.2, e.1.as_str()))
                            .collect()
                    };
                    let pauses: Vec<u64> = times("pause").iter().map(|t| t.0).collect();
                    let unpauses: Vec<u64> = times("unpause").iter().map(|t| t.0).collect();
                    let paused_ids: HashSet<&str> = times("pause").iter().map(|t| t.1).collect();
                    let unpaused_ids: HashSet<&str> =
                        times("unpause").iter().map(|t| t.1).collect();
                    let complete = !paused_ids.is_empty() && paused_ids.is_subset(&unpaused_ids);
                    (pauses, unpauses, complete)
                };
                if complete || Instant::now() >= deadline {
                    let first = pauses.iter().min()?;
                    let last = unpauses.iter().max()?;
                    return Some(last.saturating_sub(*first) / 1_000_000);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    impl Drop for FreezeEvents {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    /// The independent watchdog process of one capture. It exits by itself
    /// once nothing is unsettled; dropping the handle reaps it in the background.
    struct Watchdog {
        child: Option<Child>,
    }

    impl Watchdog {
        fn alive(&mut self) -> bool {
            self.child
                .as_mut()
                .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
        }
    }

    impl Drop for Watchdog {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
        }
    }

    struct Heartbeat(File);

    impl Heartbeat {
        fn beat(&self) {
            let _ = self.0.set_modified(std::time::SystemTime::now());
        }
    }

    fn c_name(name: &str) -> Result<CString, CaptureError> {
        CString::new(name).map_err(|_| CaptureError::Invalid("name has a NUL".into()))
    }

    impl Capturer {
        /// # Errors
        /// Fails if the pool directory cannot be opened.
        pub fn new(
            pool_path: &Path,
            engine: Engine,
            locks: Arc<ScopeLocks>,
            freezer: Arc<dyn Freezer>,
            config: CaptureConfig,
        ) -> std::io::Result<Self> {
            Ok(Self {
                pool: Pool::open(pool_path)?,
                pool_path: pool_path.to_owned(),
                engine,
                locks,
                freezer,
                config,
                fence: Mutex::new(Vec::new()),
                stale_free: Mutex::new(None),
            })
        }

        /// Freezes every workspace in `workspaces` through the endpoint's own
        /// gate (the same freeze a capture takes): mutations are refused, the
        /// ones already admitted finish first (within the lock wait), and the
        /// freezes are held until [`Self::unfence`] or until the agent exits.
        /// All or nothing: a workspace that cannot be frozen releases the rest.
        ///
        /// # Errors
        /// The first workspace that could not be frozen, and why.
        pub fn fence(&self, workspaces: &[String]) -> Result<(), String> {
            let mut held: Vec<Box<dyn Send>> = Vec::new();
            for workspace in workspaces {
                validate_label(workspace, "workspace")?;
                match self.freezer.freeze(workspace, FENCE_WAIT) {
                    Ok(guard) => held.push(guard),
                    Err(why) => {
                        return Err(format!("workspace {workspace} could not be frozen: {why}"));
                    }
                }
            }
            self.fence
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend(held);
            Ok(())
        }

        /// Releases every freeze the fence holds.
        pub fn unfence(&self) {
            self.fence
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
        }

        /// The pool's reserve, checked on the agent's own pool descriptor when
        /// the capture is admitted: `Err` is the gap's text. An unreadable pool
        /// is not captured into.
        fn check_reserve(&self, reserve_bytes: u64) -> Result<(), String> {
            let read = || -> std::io::Result<u64> {
                if break_mode("fault-pool-unreadable") {
                    return Err(std::io::Error::other("injected read failure"));
                }
                self.pool.free_bytes()
            };
            let reading = if break_mode("space-check-stale") {
                // Defect: the first reading is reused, as a read before a wait would be.
                let mut cached = self
                    .stale_free
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match *cached {
                    Some(free) => Ok(free),
                    None => {
                        let reading = read();
                        if let Ok(free) = &reading {
                            *cached = Some(*free);
                        }
                        reading
                    }
                }
            } else {
                read()
            };
            match reading {
                Ok(free) if free >= reserve_bytes => Ok(()),
                Ok(free) => {
                    eprintln!(
                        "marsh services: capture skipped: {} MiB free in the pool, reserve {} MiB",
                        free >> 20,
                        reserve_bytes >> 20
                    );
                    Err(NO_SPACE_GAP.to_owned())
                }
                Err(_) if break_mode("unreadable-pool-ok") => Ok(()),
                Err(error) => {
                    eprintln!("marsh services: the pool's free space is unreadable: {error}");
                    Err(format!(
                        "the services pool's free space could not be read ({error}); the capture was skipped"
                    ))
                }
            }
        }

        #[must_use]
        pub fn locks(&self) -> &Arc<ScopeLocks> {
            &self.locks
        }

        /// Cleans up after a crashed capture or drop. For every staging
        /// directory whose capture is dead it makes the freeze lost (so nothing
        /// can publish), thaws every container in its journal until the engine
        /// confirms, and only then removes the directory; an unreadable journal
        /// or an unconfirmed container keeps the directory and makes the result
        /// unsettled. Half-removed snapshots are finished. Run before `ready`.
        #[must_use]
        pub fn recover(&self) -> Recovery {
            let mut recovery = Recovery {
                log: Vec::new(),
                settled: true,
            };
            let Ok(snapshots) = self.pool.snapshots() else {
                recovery
                    .log
                    .push("cannot open the snapshots directory".into());
                recovery.settled = false;
                return recovery;
            };
            let Ok(names) = fsops::list_names(&snapshots) else {
                recovery
                    .log
                    .push("cannot list the snapshots directory".into());
                recovery.settled = false;
                return recovery;
            };
            for raw in names {
                let Ok(name) = String::from_utf8(raw) else {
                    continue;
                };
                let staging = name.strip_prefix(".tmp-").is_some_and(valid_snapshot_id);
                let trash = name.starts_with(".trash-") && name.len() <= 160;
                if !staging && !trash {
                    continue;
                }
                if staging && !self.recover_staging(&snapshots, &name, &mut recovery) {
                    continue;
                }
                if let Ok(c_name) = CString::new(name.clone()) {
                    match fsops::remove_tree_at(&snapshots, &c_name) {
                        Ok(()) => recovery.log.push(format!("removed leftover {name}")),
                        Err(error) => {
                            recovery
                                .log
                                .push(format!("could not remove {name}: {error}"));
                            recovery.settled = false;
                        }
                    }
                }
            }
            recovery
        }

        /// True if `name` may be removed.
        fn recover_staging(
            &self,
            snapshots: &OwnedFd,
            name: &str,
            recovery: &mut Recovery,
        ) -> bool {
            let Ok(dir) = fsops::open_dir_at(snapshots, name) else {
                recovery.settled = false;
                return false;
            };
            if !fsops::try_lock_dir(&dir) {
                recovery
                    .log
                    .push(format!("{name}: a capture is still running; left alone"));
                return false;
            }
            if break_mode("unpause-recovery-off") {
                return true;
            }
            if break_mode("recovery-discard-journal") {
                // Deliberately wrong (a break mode): remove without confirming.
                return true;
            }
            freeze::revoke(&dir);
            match freeze::thaw(&self.engine, &dir, 30) {
                Ok(left) if left.is_empty() => true,
                Ok(left) => {
                    recovery.log.push(format!(
                        "{name}: {} containers could not be confirmed thawed; kept",
                        left.len()
                    ));
                    recovery.settled = false;
                    false
                }
                Err(error) => {
                    recovery.log.push(format!(
                        "{name}: the freeze journal is unreadable ({error}); kept"
                    ));
                    recovery.settled = false;
                    false
                }
            }
        }

        /// Snapshots present and published, with their manifests' summaries.
        ///
        /// # Errors
        /// Fails if the snapshots directory cannot be read.
        pub fn list(&self) -> Result<Vec<SnapshotInfo>, CaptureError> {
            let snapshots = self.pool.snapshots()?;
            let mut out = Vec::new();
            for raw in fsops::list_names(&snapshots)? {
                let Ok(name) = String::from_utf8(raw) else {
                    continue;
                };
                if !valid_snapshot_id(&name) {
                    continue;
                }
                let info = read_manifest_value(&snapshots, &name);
                out.push(SnapshotInfo {
                    id: name,
                    workspace: info
                        .as_ref()
                        .and_then(|v| v["workspace"].as_str())
                        .map(str::to_owned),
                    created_unix_ms: info.as_ref().and_then(|v| v["created_unix_ms"].as_u64()),
                    complete: info.as_ref().and_then(|v| v["complete"].as_bool()),
                });
                if out.len() >= 4096 {
                    break;
                }
            }
            out.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(out)
        }

        /// The exact workspace that owns snapshot `id`, published or still being
        /// captured (from its manifest or its staging intent).
        fn owner_of(snapshots: &OwnedFd, id: &str) -> Option<String> {
            if let Some(value) = read_manifest_value(snapshots, id)
                && let Some(workspace) = value["workspace"].as_str()
            {
                return Some(workspace.to_owned());
            }
            let dir = fsops::open_dir_at(snapshots, &format!(".tmp-{id}")).ok()?;
            let bytes = fsops::read_file_at(&dir, INTENT, 4096).ok()?;
            serde_json::from_slice::<Intent>(&bytes)
                .ok()
                .map(|i| i.workspace)
        }

        /// Removes a published snapshot by opaque id. The id is validated as
        /// one component; the workspace's operation lock (found from the
        /// manifest, or the staging intent for a capture in flight) is taken
        /// first, so a drop never races its own capture; the directory is
        /// renamed out of the published namespace (atomic), then removed by
        /// descriptor, so a snapshot is either whole or gone. Returns whether
        /// it existed.
        ///
        /// # Errors
        /// `Invalid` for a bad id, `Failed` if locks or removal fail.
        pub fn drop_snapshot(&self, id: &str) -> Result<bool, CaptureError> {
            if !valid_snapshot_id(id) {
                return Err(CaptureError::Invalid("invalid snapshot id".into()));
            }
            let snapshots = self.pool.snapshots()?;
            let trash = format!(".trash-{id}-{}", fsops::random_id("", 4)?);
            let Some(workspace) = Self::owner_of(&snapshots, id) else {
                // Neither published nor being captured. A directory without a
                // manifest (damaged) is still removed by id, under no lock.
                return match fsops::publish(&snapshots, id, &trash) {
                    Ok(()) => {
                        fsops::remove_tree_at(&snapshots, &c_name(&trash)?)?;
                        Ok(true)
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                    Err(error) => Err(error.into()),
                };
            };
            {
                let _op = self
                    .locks
                    .op(&workspace, "drop", self.config.lock_wait)
                    .map_err(|busy| {
                        CaptureError::Failed(format!(
                            "workspace busy ({} in progress); retry",
                            busy.holder.unwrap_or("another operation")
                        ))
                    })?;
                match fsops::publish(&snapshots, id, &trash) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                    Err(error) => return Err(error.into()),
                }
            }
            fsops::remove_tree_at(&snapshots, &c_name(&trash)?).map_err(|error| {
                CaptureError::Failed(format!(
                    "snapshot unpublished; removal incomplete and will be retried at the next start: {error}"
                ))
            })?;
            Ok(true)
        }

        /// Captures workspace `workspace` as snapshot `id`.
        ///
        /// # Errors
        /// `Invalid` for a bad request. Every other problem comes back as a
        /// report with a gap, never as an error.
        pub fn capture(
            &self,
            id: &str,
            workspace: &str,
            reserve_bytes: u64,
        ) -> Result<CaptureReport, CaptureError> {
            if !valid_snapshot_id(id) {
                return Err(CaptureError::Invalid("invalid snapshot id".into()));
            }
            validate_label(workspace, "workspace").map_err(CaptureError::Invalid)?;
            let capture_started = Instant::now();

            let mut _gate: Option<Box<dyn Send>> = None;
            let mut _op = None;
            if !break_mode("capture-allow-overlap") {
                match self.locks.op(workspace, "capture", self.config.lock_wait) {
                    Ok(guard) => _op = Some(guard),
                    Err(busy) => {
                        let what = match busy.holder {
                            Some("capture") | None => "another capture in progress".to_owned(),
                            Some(other) => format!("a {other} is in progress"),
                        };
                        return Ok(CaptureReport::without_snapshot(
                            Outcome::Skipped,
                            id,
                            format!("skipped: {what}"),
                        ));
                    }
                }
                if !break_mode("capture-api-gate-off") {
                    // The freeze waits for admitted mutations to finish and refuses
                    // new ones until it drops (E3: `Services::freeze_scope_within`).
                    match self.freezer.freeze(workspace, self.config.lock_wait) {
                        Ok(guard) => _gate = Some(guard),
                        Err(why) => {
                            return Ok(CaptureReport::without_snapshot(
                                Outcome::Skipped,
                                id,
                                format!("skipped: {why}"),
                            ));
                        }
                    }
                }
            }

            // The reserve, now that the capture is admitted and before it writes.
            if let Err(gap) = self.check_reserve(reserve_bytes) {
                let outcome = if gap == NO_SPACE_GAP {
                    Outcome::Skipped
                } else {
                    Outcome::Failed
                };
                return Ok(CaptureReport::without_snapshot(outcome, id, gap));
            }

            let snapshots = self.pool.snapshots()?;
            let staging = if break_mode("snapshot-publish-early") {
                id.to_owned()
            } else {
                format!(".tmp-{id}")
            };
            if fsops::exists_at(&snapshots, id) || fsops::exists_at(&snapshots, &staging) {
                return Err(CaptureError::Invalid("snapshot id already in use".into()));
            }
            let stage = fsops::mkdir_new(&snapshots, &staging)?;
            if !fsops::try_lock_dir(&stage) {
                return Err(CaptureError::Failed("staging directory is locked".into()));
            }
            let intent = serde_json::to_vec(&Intent {
                id: id.to_owned(),
                workspace: workspace.to_owned(),
            })
            .map_err(|e| CaptureError::Failed(e.to_string()))?;
            fsops::write_new_file_at(&stage, INTENT, &intent)?;
            let result = self.run(id, workspace, &snapshots, &stage, &staging, capture_started);
            if matches!(&result, Ok(report) if report.outcome == Outcome::Published) {
                return result;
            }
            // Not published: nothing of this capture may remain, unless a
            // container is not yet confirmed thawed and needs the journal.
            let confirmed = freeze::unsettled(&stage).is_ok_and(|ids| ids.is_empty());
            drop(stage);
            if confirmed {
                let _ = fsops::remove_tree_at(&snapshots, &c_name(&staging)?);
            }
            match result {
                Ok(report) => Ok(report),
                Err(CaptureError::Invalid(why)) => Err(CaptureError::Invalid(why)),
                Err(CaptureError::Failed(why) | CaptureError::Stuck(why)) => {
                    eprintln!("marsh services: capture {id} failed: {why}");
                    Ok(CaptureReport::without_snapshot(
                        Outcome::Failed,
                        id,
                        format!("capture failed: {}", sanitize(&why, MAX_TEXT)),
                    ))
                }
            }
        }

        #[allow(clippy::too_many_lines)]
        fn run(
            &self,
            id: &str,
            workspace: &str,
            snapshots: &OwnedFd,
            stage: &OwnedFd,
            staging: &str,
            capture_started: Instant,
        ) -> Result<CaptureReport, CaptureError> {
            let all_scopes = break_mode("capture-all-workspaces");
            let mut gaps: Vec<String> = Vec::new();

            if break_mode("fault-capture-fail") {
                return Err(CaptureError::Failed("injected failure".into()));
            }
            fsops::write_new_file_at(stage, ALIVE, b"")?;
            let heartbeat = Heartbeat(fsops::open_for_write_at(stage, ALIVE)?);
            if break_mode("slow-capture") {
                std::thread::sleep(Duration::from_millis(1500));
            }

            // 1. What belongs to the workspace, by exact scope label.
            let all = self.engine.list_containers(true)?;
            let scoped: Vec<&ContainerSummary> = all
                .iter()
                .filter(|c| all_scopes || c.label(SCOPE_LABEL) == Some(workspace))
                .collect();
            let volume_list = if all_scopes {
                self.engine.list_volumes_with_label(SCOPE_LABEL, None)?
            } else {
                self.engine
                    .list_volumes_with_label(SCOPE_LABEL, Some(workspace))?
            };
            let running: Vec<&ContainerSummary> = scoped
                .iter()
                .copied()
                .filter(|c| c.state == "running")
                .collect();
            if running.len() > MAX_PAUSED_CONTAINERS {
                return Err(CaptureError::Failed(format!(
                    "{} running containers; the limit is {MAX_PAUSED_CONTAINERS}",
                    running.len()
                )));
            }
            let scoped_ids: HashSet<&str> = scoped.iter().map(|c| c.id.as_str()).collect();
            // Keyed by the engine's exact volume name.
            let mut blocked: HashMap<String, String> = HashMap::new();
            for other in all
                .iter()
                .filter(|c| c.state == "running" && !scoped_ids.contains(c.id.as_str()))
            {
                for mount in other.mounts.iter().flatten().filter(|m| m.kind == "volume") {
                    blocked.insert(mount.name.clone(), "in use by another workspace".into());
                }
            }

            // 2. Plan the volumes and open each source beneath the pool fd.
            let mut planned: Vec<Planned> = Vec::new();
            for volume in volume_list.iter().take(MAX_VOLUMES) {
                if !all_scopes && volume.label(SCOPE_LABEL) != Some(workspace) {
                    continue;
                }
                let shown = sanitize(&volume.name, MAX_TEXT);
                let mut entry = Planned {
                    name: volume.name.clone(),
                    shown: shown.clone(),
                    opaque: fsops::random_id("v", 12)?,
                    labels: display_labels(volume.labels.as_ref()),
                    compose_label: volume.label("com.docker.compose.volume").map(str::to_owned),
                    source: None,
                    gaps: Vec::new(),
                };
                if volume.driver != "local" {
                    entry.gaps.push(format!(
                        "volume '{shown}': its storage driver can't be saved"
                    ));
                } else if volume.options.as_ref().is_some_and(|o| !o.is_empty()) {
                    entry.gaps.push(format!(
                        "volume '{shown}': it has storage options and can't be saved"
                    ));
                } else if let Some(why) = blocked.get(&volume.name) {
                    entry
                        .gaps
                        .push(format!("volume '{shown}': {why}; not saved"));
                } else {
                    match self.pool.volume_source(&volume.name) {
                        Ok(source) => entry.source = Some(source),
                        Err(error) => entry.gaps.push(format!(
                            "volume '{shown}': its data directory was not usable ({}); not saved",
                            describe_open_error(&error)
                        )),
                    }
                }
                planned.push(entry);
            }
            if volume_list.len() > MAX_VOLUMES {
                push_gap(
                    &mut gaps,
                    format!(
                        "{} volumes; only the first {MAX_VOLUMES} were considered",
                        volume_list.len()
                    ),
                );
            }

            // 3. Stage the destination directories.
            let from_label = break_mode("snapshot-name-from-label");
            let mut destinations: Vec<Option<OwnedFd>> = Vec::new();
            for entry in &planned {
                if entry.source.is_none() {
                    destinations.push(None);
                    continue;
                }
                let dst = if from_label {
                    // Deliberately wrong (a break mode): a label becomes a path.
                    let label = entry.compose_label.as_deref().unwrap_or(&entry.opaque);
                    let path = format!(
                        "/proc/self/fd/{}/{label}",
                        std::os::fd::AsRawFd::as_raw_fd(stage)
                    );
                    std::fs::create_dir_all(&path)?;
                    fsops::open_dir_by_path(Path::new(&path))?
                } else {
                    fsops::mkdir_new(stage, &entry.opaque)?
                };
                destinations.push(Some(dst));
            }

            // 4. The freeze journal (before anything is paused) and the watchdog.
            let mut journal_ids: Vec<String> = running
                .iter()
                .map(|c| c.id.clone())
                .filter(|id| valid_container_id(id))
                .collect();
            let freezing = !journal_ids.is_empty() && !break_mode("capture-no-pause");
            let mut watchdog = None;
            // From the first journal write, every exit confirms the thaw.
            let mut freeze_guard = Freeze {
                engine: &self.engine,
                stage,
                armed: true,
            };
            if freezing {
                freeze::write_journal(stage, id, &journal_ids)?;
                heartbeat.beat();
                if !break_mode("unpause-recovery-off") {
                    watchdog = Some(self.spawn_watchdog(staging)?);
                }
            }
            let clock = if freezing {
                FreezeEvents::start(&self.engine)
            } else {
                None
            };

            // 5. Freeze, copy within the budget, thaw until the engine confirms.
            let copier = Arc::new(Copier::new(!break_mode("capture-copy")));
            for _ in 0..self.config.workers.clamp(1, 16) {
                let copier = Arc::clone(&copier);
                std::thread::spawn(move || copier.worker());
            }
            let _stop = StopWorkers(&copier);
            let t_pause = Instant::now();
            if freezing {
                let results = pause_concurrently(&self.engine, &journal_ids);
                for (cid, result) in &results {
                    if let (Err(error), Some(c)) = (result, running.iter().find(|c| &c.id == cid)) {
                        push_gap(
                            &mut gaps,
                            format!(
                                "container '{}' could not be paused ({}); its volumes were not saved",
                                sanitize(&c.display_name(), MAX_TEXT),
                                sanitize(error, 80)
                            ),
                        );
                        for mount in c.mounts.iter().flatten() {
                            blocked.insert(
                                mount.name.clone(),
                                "its container could not be paused".into(),
                            );
                        }
                    }
                }
                // Anything that started after the listing is frozen too; its id
                // reaches the journal before its pause call.
                for _ in 0..2 {
                    let late: Vec<ContainerSummary> = self
                        .engine
                        .list_containers(false)?
                        .into_iter()
                        .filter(|c| {
                            (all_scopes || c.label(SCOPE_LABEL) == Some(workspace))
                                && c.state == "running"
                                && valid_container_id(&c.id)
                                && !journal_ids.contains(&c.id)
                        })
                        .collect();
                    if late.is_empty() {
                        break;
                    }
                    let ids: Vec<String> = late.iter().map(|c| c.id.clone()).collect();
                    journal_ids.extend(ids.iter().cloned());
                    freeze::write_journal(stage, id, &journal_ids)?;
                    for (cid, result) in pause_concurrently(&self.engine, &ids) {
                        if result.is_err()
                            && let Some(c) = late.iter().find(|c| c.id == cid)
                        {
                            for mount in c.mounts.iter().flatten() {
                                blocked.insert(
                                    mount.name.clone(),
                                    "its container could not be paused".into(),
                                );
                            }
                            push_gap(
                                &mut gaps,
                                format!(
                                    "container '{}' started during the capture and could not be paused",
                                    sanitize(&c.display_name(), MAX_TEXT)
                                ),
                            );
                        }
                    }
                }
            }
            let pause_ms = elapsed_ms(t_pause);
            heartbeat.beat();
            // A freeze lost while pausing (the watchdog fired) discards the capture.
            Self::ensure_freeze_held(stage, watchdog.as_mut(), freezing)?;
            hold("capture-frozen", &mut || heartbeat.beat());
            if break_mode("fault-capture-fail-after-pause") {
                return Err(CaptureError::Failed("injected failure after pause".into()));
            }
            if break_mode("fault-capture-hang") {
                // No heartbeat: the watchdog must notice.
                std::thread::sleep(Duration::from_secs(600));
                return Err(CaptureError::Failed("injected hang ended".into()));
            }
            if break_mode("slow-freeze") {
                std::thread::sleep(Duration::from_millis(800));
            }

            let t_copy = Instant::now();
            let mut copy_volumes: Vec<usize> = Vec::new();
            let mut first_volume = None;
            if !break_mode("capture-skip-volumes") {
                let mut jobs = Vec::new();
                for (index, entry) in planned.iter_mut().enumerate() {
                    let blocked_now = blocked.get(&entry.name).cloned();
                    if let (Some(source), Some(dst)) =
                        (entry.source.take(), destinations[index].take())
                    {
                        if let Some(why) = blocked_now {
                            entry
                                .gaps
                                .push(format!("volume '{}': {why}; not saved", entry.shown));
                            continue;
                        }
                        copy_volumes.push(index);
                        jobs.push(VolumeCopy {
                            display: entry.shown.clone(),
                            src: source,
                            dst,
                        });
                    }
                }
                let hard_stop = if break_mode("capture-no-budget") {
                    None
                } else {
                    let deadline = t_pause + self.config.freeze_budget;
                    copier.set_deadline(Some(deadline));
                    Some(deadline + COPY_GRACE)
                };
                first_volume = Some(copier.submit(jobs)?);
                Self::wait_for_copy(
                    &copier,
                    stage,
                    &heartbeat,
                    watchdog.as_mut(),
                    freezing,
                    hard_stop,
                )?;
            }
            let copy_ms = elapsed_ms(t_copy);
            // The freeze must have held through the whole copy.
            Self::ensure_freeze_held(stage, watchdog.as_mut(), freezing)?;

            let left = freeze_guard.thaw()?;
            let frozen_calls_ms = elapsed_ms(t_pause);
            if !left.is_empty() {
                return Err(CaptureError::Stuck(format!(
                    "{} containers could not be confirmed unpaused; they will be retried",
                    left.len()
                )));
            }
            let (frozen_ms, frozen_source) = clock
                .as_ref()
                .and_then(|c| c.window_ms(&journal_ids, Duration::from_millis(250)))
                .map_or((frozen_calls_ms, "calls"), |ms| (ms, "engine-events"));
            if freezing {
                let _ = fsops::remove_file_at(stage, JOURNAL);
                drop(watchdog.take());
            }

            // Outside the freeze: results, cleanup of partial copies, manifest.
            let mut volumes = Vec::new();
            let mut outcomes: HashMap<usize, fsops::VolumeResult> = HashMap::new();
            if let Some(first) = first_volume {
                for (offset, planned_index) in copy_volumes.iter().enumerate() {
                    outcomes.insert(*planned_index, copier.result(first + offset));
                }
            }
            let skipped_volumes = break_mode("capture-skip-volumes");
            for (index, entry) in planned.iter().enumerate() {
                let mut entry_gaps = entry.gaps.clone();
                let (captured, files) = match outcomes.get(&index) {
                    Some(result) => {
                        for gap in &result.gaps {
                            entry_gaps.push(format!(
                                "volume '{}': {}",
                                entry.shown,
                                sanitize(gap, MAX_TEXT)
                            ));
                        }
                        (result.complete, result.files)
                    }
                    None => (skipped_volumes && entry.gaps.is_empty(), 0),
                };
                if !captured
                    && !skipped_volumes
                    && let Ok(name) = c_name(&entry.opaque)
                {
                    let _ = fsops::remove_tree_at(stage, &name);
                }
                for gap in &entry_gaps {
                    push_gap(&mut gaps, gap.clone());
                }
                volumes.push(VolumeEntry {
                    id: entry.opaque.clone(),
                    name: entry.shown.clone(),
                    captured,
                    files,
                    labels: entry.labels.clone(),
                    gaps: entry_gaps,
                });
            }

            let by_name: HashMap<&str, &str> = planned
                .iter()
                .map(|p| (p.name.as_str(), p.opaque.as_str()))
                .collect();
            let (stacks, standalone) = group_containers(&scoped, &by_name);
            let images = self.image_entries(&scoped);

            let complete = gaps.is_empty() && volumes.iter().all(|v| v.captured);
            let manifest = Manifest {
                version: 1,
                id: id.to_owned(),
                workspace: workspace.to_owned(),
                created_unix_ms: unix_ms(),
                complete,
                frozen_ms,
                frozen_source: frozen_source.to_owned(),
                frozen_calls_ms,
                pause_ms,
                copy_ms,
                total_ms: elapsed_ms(capture_started),
                volumes,
                stacks,
                standalone,
                images,
                gaps: gaps.clone(),
            };
            let bytes =
                serde_json::to_vec(&manifest).map_err(|e| CaptureError::Failed(e.to_string()))?;
            fsops::write_new_file_at(stage, MANIFEST_NAME, &bytes)?;
            let _ = fsops::remove_file_at(stage, ALIVE);
            hold("capture-before-publish", &mut || {});
            if staging != id {
                fsops::publish(snapshots, staging, id)?;
            }
            // The bookkeeping goes only after publication, so the workspace
            // stays discoverable (manifest, or intent) the whole time.
            if let Ok(published) = fsops::open_dir_at(snapshots, id) {
                let _ = fsops::remove_file_at(&published, INTENT);
                let _ = fsops::remove_file_at(&published, freeze::REVOKED);
                if let Ok(name) = c_name(freeze::SETTLED) {
                    let _ = fsops::remove_tree_at(&published, &name);
                }
            }
            Ok(CaptureReport {
                outcome: Outcome::Published,
                id: id.to_owned(),
                manifest: Some(manifest),
                gaps,
            })
        }

        /// A capture that lost its freeze (the watchdog or recovery revoked it,
        /// or the watchdog process is gone) must not go on to publish.
        fn ensure_freeze_held(
            stage: &OwnedFd,
            watchdog: Option<&mut Watchdog>,
            freezing: bool,
        ) -> Result<(), CaptureError> {
            if !freezing || break_mode("capture-ignore-revoked") {
                return Ok(());
            }
            if freeze::is_revoked(stage) {
                return Err(CaptureError::Failed(
                    "the freeze was lost: the watchdog thawed the workspace, so this capture was discarded".into(),
                ));
            }
            if let Some(watchdog) = watchdog
                && !watchdog.alive()
            {
                return Err(CaptureError::Failed("the unpause watchdog stopped".into()));
            }
            Ok(())
        }

        /// Waits for the copy with real-progress supervision: the heartbeat is
        /// refreshed only while the workers' progress counter moves, a revoked
        /// freeze aborts the copy, and past the freeze budget plus a grace the
        /// capture abandons a stuck worker and fails.
        fn wait_for_copy(
            copier: &Arc<Copier>,
            stage: &OwnedFd,
            heartbeat: &Heartbeat,
            mut watchdog: Option<&mut Watchdog>,
            freezing: bool,
            hard_stop: Option<Instant>,
        ) -> Result<(), CaptureError> {
            let mut seen = copier.progress();
            let mut lost: Option<CaptureError> = None;
            let mut tick = || {
                let now = copier.progress();
                if now != seen {
                    seen = now;
                    heartbeat.beat();
                }
                if lost.is_none()
                    && let Err(error) =
                        Self::ensure_freeze_held(stage, watchdog.as_deref_mut(), freezing)
                {
                    lost = Some(error);
                    copier.abandon();
                }
                if lost.is_none() && hard_stop.is_some_and(|stop| Instant::now() >= stop) {
                    lost = Some(CaptureError::Failed(
                        "the copy stalled past the freeze budget".into(),
                    ));
                    copier.abandon();
                }
            };
            copier.wait(&mut tick);
            drop(tick);
            lost.map_or(Ok(()), Err)
        }

        fn image_entries(&self, scoped: &[&ContainerSummary]) -> Vec<ImageEntry> {
            let mut seen = BTreeSet::new();
            let mut out = Vec::new();
            for container in scoped {
                if container.image_id.is_empty() || !seen.insert(container.image_id.clone()) {
                    continue;
                }
                if out.len() >= 256 {
                    break;
                }
                out.push(ImageEntry {
                    image: sanitize(&container.image, MAX_TEXT),
                    image_id: sanitize(&container.image_id, 80),
                    digests: self
                        .engine
                        .image_digests(&container.image_id)
                        .unwrap_or_default()
                        .into_iter()
                        .take(8)
                        .map(|d| sanitize(&d, MAX_TEXT))
                        .collect(),
                });
            }
            out
        }

        fn spawn_watchdog(&self, staging: &str) -> Result<Watchdog, CaptureError> {
            let exe = std::env::current_exe()?;
            let dir = self.pool_path.join("snapshots").join(staging);
            let mut command = std::process::Command::new(exe);
            command.arg("--internal-services").arg("capture-watchdog");
            // The watchdog is its own process; test modes reach it the same way
            // they reach the relay (the dedicated argument, never the environment).
            #[cfg(feature = "test-breaks")]
            if !crate::breaks::active().is_empty() {
                command
                    .arg("--break-modes")
                    .arg(crate::breaks::active().join(","));
            }
            let child = command
                .arg("--engine-socket")
                .arg(self.engine.socket())
                .arg("--dir")
                .arg(&dir)
                .arg("--bound-ms")
                .arg(self.config.watchdog.as_millis().to_string())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .process_group(0)
                .spawn()
                .map_err(|error| {
                    CaptureError::Failed(format!("could not start the unpause watchdog: {error}"))
                })?;
            Ok(Watchdog { child: Some(child) })
        }
    }

    /// Stops the copier's idle workers on every exit path.
    struct StopWorkers<'a>(&'a Arc<Copier>);

    impl Drop for StopWorkers<'_> {
        fn drop(&mut self) {
            self.0.shutdown();
        }
    }

    fn read_manifest_value(snapshots: &OwnedFd, id: &str) -> Option<serde_json::Value> {
        let dir = fsops::open_dir_at(snapshots, id).ok()?;
        let bytes = fsops::read_file_at(&dir, MANIFEST_NAME, MAX_MANIFEST_BYTES).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn describe_open_error(error: &std::io::Error) -> String {
        match error.raw_os_error() {
            Some(40) => "a symbolic link is in the way".to_owned(),
            Some(18) => "a mount point is in the way".to_owned(),
            Some(2) => "it does not exist".to_owned(),
            _ => error.to_string(),
        }
    }

    fn elapsed_ms(since: Instant) -> u64 {
        u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn group_containers(
        scoped: &[&ContainerSummary],
        volume_ids: &HashMap<&str, &str>,
    ) -> (Vec<StackEntry>, Vec<ContainerEntry>) {
        type Place = (Option<String>, Vec<String>, Vec<ContainerEntry>);
        let mut stacks: BTreeMap<(String, String), Place> = BTreeMap::new();
        let mut standalone = Vec::new();
        for container in scoped.iter().take(1024) {
            let entry = ContainerEntry {
                name: sanitize(&container.display_name(), MAX_TEXT),
                service: container
                    .label("com.docker.compose.service")
                    .map(|s| sanitize(s, MAX_TEXT)),
                image: sanitize(&container.image, MAX_TEXT),
                // A frozen container is up: a stop cutover pauses everything before it
                // captures, and the manifest must still say they were running.
                running: container.state == "running" || container.state == "paused",
                volumes: container
                    .mounts
                    .iter()
                    .flatten()
                    .filter(|m| m.kind == "volume")
                    .filter_map(|m| {
                        let key = if break_mode("names-by-display") {
                            sanitize(&m.name, MAX_TEXT)
                        } else {
                            m.name.clone()
                        };
                        volume_ids.get(key.as_str()).map(|id| (*id).to_owned())
                    })
                    .chain(
                        // Test fault: the manifest names a volume it does not list.
                        break_mode("manifest-dangling-ref").then(|| "vmissing000000".to_owned()),
                    )
                    .collect(),
                testcontainers: container.label("org.testcontainers").is_some()
                    || container.label("org.testcontainers.sessionId").is_some(),
            };
            match compose_project(container) {
                Some((kind, project)) => {
                    let place = stacks.entry((kind.to_owned(), project)).or_default();
                    if place.0.is_none() {
                        place.0 = container
                            .label("com.docker.compose.project.working_dir")
                            .map(|w| sanitize(w, MAX_PATH_TEXT));
                    }
                    if place.1.is_empty()
                        && let Some(files) =
                            container.label("com.docker.compose.project.config_files")
                    {
                        place.1 = files
                            .split(',')
                            .take(16)
                            .map(|f| sanitize(f, MAX_PATH_TEXT))
                            .collect();
                    }
                    place.2.push(entry);
                }
                None => standalone.push(entry),
            }
        }
        (
            stacks
                .into_iter()
                .map(
                    |((kind, project), (working_dir, config_files, containers))| StackEntry {
                        kind,
                        project,
                        working_dir,
                        config_files,
                        containers,
                    },
                )
                .collect(),
            standalone,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text marsh's checkpoints show for a pool below its reserve; the
    /// daemon crate pins the same words.
    #[test]
    fn the_full_pool_gap_is_the_text_checkpoints_show() {
        assert_eq!(NO_SPACE_GAP, "not enough space for app data");
    }

    #[test]
    fn snapshot_ids_are_single_safe_components() {
        for good in ["a", "ckpt_01-x", "A9", &"a".repeat(64)] {
            assert!(valid_snapshot_id(good), "{good}");
        }
        for bad in [
            "",
            ".",
            "..",
            "../x",
            "a/b",
            "a b",
            "-rf",
            ".tmp-x",
            "a\0b",
            "é",
            "a\n",
            &"a".repeat(65),
        ] {
            assert!(!valid_snapshot_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn display_text_is_bounded_but_differs_from_identity() {
        let a = format!("{}A", "v".repeat(200));
        let b = format!("{}B", "v".repeat(200));
        assert_eq!(sanitize(&a, MAX_TEXT), sanitize(&b, MAX_TEXT));
        assert_ne!(a, b);
    }
}
