//! Services snapshots for checkpoints (C12): the backend side of
//! `DaemonBackend::{capture_services, drop_services, list_services,
//! services_reuse, services_track, services_protected}` and the stop cutover
//! (DECISIONS 12). Everything goes through E1's relay of a project whose
//! services VM is **ready**; nothing here starts, creates or wakes a VM
//! (`capture-ensure-vm` is the deliberate defect that does).
//!
//! # Freshness
//!
//! A checkpoint made while the VM is stopped may reference a snapshot only if
//! marsh knows that snapshot holds the workspace's last write. At every stop
//! marsh causes (idle, `services stop`, `marsh stop`) the **cutover** runs
//! first, with the project's capture exclusion held until the VM has actually
//! stopped:
//!
//! 1. **fence**: every tracked workspace is frozen through the agent's filtered
//!    endpoint's own freeze gate (a request that would change it is refused
//!    while frozen; the ones already admitted finish first), and stays frozen
//!    until the VM stops (the freeze lives in the agent, which exits with the
//!    VM);
//! 2. every running container of the engine is paused;
//! 3. every tracked workspace is captured (the reserve checked by the agent);
//! 4. the engine is asked again what runs: **any engine-query error, and any
//!    running container, make the outcome a gap**, never `proven`;
//! 5. each workspace's outcome (`proven` with the snapshot, or `failed` with
//!    why) is journalled durably, bound to the owned VM's UUID, **before** the
//!    VM stops. If neither the outcome nor an invalidation of the old journal
//!    can be persisted, the stop is refused.
//!
//! The journal is `services-last-capture.json` in the daemon's home, with a
//! marker file (`services-last-capture.lost`): written (and synced, with its
//! directory) before the journal is changed in a way that matters for a stop,
//! and removed after the new journal is durable, so a crash or a failed write
//! anywhere in between leaves the marker. A restart that finds the marker
//! (or an unreadable journal) treats **every** recorded entry as `Unknown`,
//! even when an old file with a `proven` outcome remains. A stop marsh did not
//! cause leaves no outcome: its checkpoint reuses the last capture **with a
//! warning gap**, never silently. A failed outcome is a gap; `Unknown` falls
//! back to the workspace's latest record, with a warning.

use super::StockDaemonBackend;
use marsh_daemon::checkpoint::{
    BASE_WORKSPACE, LastCapture, ServicesCapture, ServicesRecord, ServicesReuse,
    ServicesSnapshotInfo, new_snapshot_id,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, PoisonError},
    time::{Duration, Instant},
};

/// The pool's reserve when `MARSH_SERVICES_POOL_RESERVE_BYTES` does not say.
pub(crate) const DEFAULT_POOL_RESERVE: u64 = 512 << 20;
const JOURNAL: &str = "services-last-capture.json";
const JOURNAL_LOST: &str = "services-last-capture.lost";
/// Attempts at a final capture that finds another capture in progress.
const FINAL_CAPTURE_ATTEMPTS: u32 = 3;

/// Test-only defects and holds (`docs/design/test-breaks.md`), read through
/// `marsh_contracts::break_mode`.
pub(crate) mod breaks {
    pub const BACKEND_SERVICES_DISCONNECTED: &str = "backend-services-disconnected";
    pub const CAPTURE_ENSURE_VM: &str = "capture-ensure-vm";
    pub const REUSE_LAST_SNAPSHOT_OFF: &str = "reuse-last-snapshot-off";
    pub const IDLE_FINAL_CAPTURE_OFF: &str = "idle-final-capture-off";
    /// The reserve sent with a capture request is zero.
    pub const ADMIT_IGNORE_POOL: &str = "admit-ignore-pool";
    /// The stop does not take the agent's write fence (the containers are still paused).
    pub const STOP_FENCE_OFF: &str = "stop-fence-off";
    /// The stop does not pause the containers and does not count what runs.
    pub const STOP_PAUSE_OFF: &str = "stop-pause-off";
    pub const STOP_BASE_ONLY: &str = "stop-base-only";
    pub const REUSE_IGNORES_FAILURE: &str = "reuse-ignores-failure";
    pub const IDLE_IGNORES_INFLIGHT: &str = "idle-ignores-inflight";
    /// An explicit stop goes ahead although capture exclusion could not be acquired.
    pub const CAPTURE_GATE_IGNORED: &str = "capture-gate-ignored";
    pub const JOURNAL_WRITE_FAIL: &str = "journal-write-fail";
    /// A fault: the marker that invalidates the journal cannot be written either.
    pub const JOURNAL_MARKER_FAIL: &str = "journal-marker-fail";
    /// A fault: the daemon dies after the marker is durable and before the
    /// journal is rewritten (the restart at the journal boundary).
    pub const JOURNAL_CRASH_AFTER_INTENT: &str = "journal-crash-after-intent";
    /// The defect for the restart: no marker is written before the journal changes.
    pub const JOURNAL_INTENT_OFF: &str = "journal-intent-off";
    /// The defect for a journal that cannot be written: the stop goes ahead.
    pub const JOURNAL_NEITHER_IGNORED: &str = "journal-neither-ignored";
    pub const JOURNAL_VM_UNBOUND: &str = "journal-vm-unbound";
    /// A lost journal is read as "nothing was ever captured", and an entry
    /// that survives it is trusted.
    pub const JOURNAL_LOSS_SILENT: &str = "journal-loss-silent";
    /// Recovery does not reconcile lane captures whose checkpoint record is gone.
    pub const LANE_RECONCILE_OFF: &str = "lane-reconcile-off";
    /// A hold (`<point>.reached` / `<point>.release` under `test-holds`).
    pub const HOLD_BEFORE_SPACE_CHECK: &str = "hold-checkpoint-before-space-check";
    /// A hold of the cutover after a workspace's capture, before the count.
    pub const HOLD_CUTOVER_AFTER_CAPTURE: &str = "hold-cutover-after-capture";
    /// A hold of the cutover after the count, before the outcomes are journalled.
    pub const HOLD_CUTOVER_AFTER_COUNT: &str = "hold-cutover-after-count";
    /// P2: a capture that overruns its budget is waited for, not skipped.
    pub const CAPTURE_NO_SKIP: &str = "capture-no-skip";
    /// The TEST-PLAN name of the same defect (CAP-12 c1, PERF-10).
    pub const CAPTURE_BLOCK_NEXT_PROMPT: &str = "capture-block-next-prompt";
}

/// The time a checkpoint's services capture may add (P2, DECISIONS 16): from
/// the start of the capture to the agent's answer. `services_capture_budget_ms`
/// in `config.json`; default 2000 ms.
#[derive(Clone, Copy)]
struct Budget {
    started: Instant,
    total: Duration,
}

/// The shortest wait for the agent's answer once admission used most of the
/// budget: a capture that is already running normally answers in this time.
const MIN_ANSWER_WAIT: Duration = Duration::from_millis(250);

impl Budget {
    fn spent(&self) -> bool {
        self.started.elapsed() >= self.total
    }

    fn remaining(&self) -> Duration {
        self.total
            .saturating_sub(self.started.elapsed())
            .max(MIN_ANSWER_WAIT)
    }

    /// The gap, in plain words.
    fn skipped(&self) -> String {
        format!(
            "Docker data was not saved this time: saving it would have held up your next step by more than {:.1} s; your files were saved",
            self.total.as_secs_f64()
        )
    }
}

/// How marsh stopped a workspace's data at the cutover.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum StopOutcome {
    /// Captured with the engine frozen: this snapshot holds the last write.
    Proven {
        id: String,
    },
    Failed {
        reason: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StopRecord {
    at_unix_ms: u64,
    workspaces: BTreeMap<String, StopOutcome>,
}

/// What marsh recorded about one project's services VM, bound to the VM's UUID.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct ProjectJournal {
    vm_uuid: String,
    /// Each workspace's latest snapshot.
    last: BTreeMap<String, LastCapture>,
    /// Set by a stop marsh caused, cleared when the VM runs again.
    stop: Option<StopRecord>,
    /// A restart found the journal possibly older than the truth (a marker, or
    /// an unreadable file): nothing in this entry is trusted until the next
    /// cutover records the project again.
    #[serde(default)]
    suspect: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct JournalFile {
    version: u32,
    /// The journal was lost once: a project with no entry is `Unknown`, not
    /// "never captured".
    #[serde(default)]
    lost: bool,
    projects: BTreeMap<String, ProjectJournal>,
}

#[derive(Default)]
struct JournalState {
    loaded: bool,
    /// The journal was lost at some point (see `JournalFile::lost`).
    lost: bool,
    /// The marker file exists: the journal on disk may be older than this
    /// process's view of it.
    marker: bool,
    file: JournalFile,
}

/// What came of persisting the journal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Persisted {
    /// The new journal is durable.
    Saved,
    /// It could not be written, but the old one is durably marked invalid: a
    /// restart reads every entry as unknown.
    Invalidated,
    /// Neither the journal nor the marker could be persisted: a restart would
    /// trust an older record.
    Neither,
}

/// Reader/writer admission of checkpoint captures against a stop cutover, as
/// owned guards (a stop holds its exclusion across the relay closing and the
/// VM stopping, so a guard cannot borrow a stack frame).
#[derive(Default)]
pub(crate) struct CaptureGate {
    state: Mutex<GateState>,
    changed: Condvar,
}

#[derive(Default)]
struct GateState {
    captures: usize,
    stopping: bool,
}

/// A checkpoint capture in flight (shared with other captures).
pub(crate) struct CaptureShare(Arc<CaptureGate>);

/// A stop's exclusion: no capture is in flight and none starts, until dropped.
pub(crate) struct CaptureExclusion(Arc<CaptureGate>);

impl CaptureGate {
    fn try_share(self: &Arc<Self>) -> Option<CaptureShare> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.stopping {
            return None;
        }
        state.captures += 1;
        Some(CaptureShare(Arc::clone(self)))
    }

    /// Waits at most `wait` for the captures in flight; `None`: still busy.
    fn exclusive(self: &Arc<Self>, wait: Duration) -> Option<CaptureExclusion> {
        let deadline = Instant::now() + wait;
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if state.captures == 0 && !state.stopping {
                state.stopping = true;
                return Some(CaptureExclusion(Arc::clone(self)));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            state = self
                .changed
                .wait_timeout(state, left.min(Duration::from_millis(250)))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

impl Drop for CaptureShare {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.captures -= 1;
        self.0.changed.notify_all();
    }
}

impl Drop for CaptureExclusion {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.stopping = false;
        self.0.changed.notify_all();
    }
}

/// What the daemon tracks about snapshots, in memory and in the journal.
#[derive(Default)]
pub(crate) struct Snapshots {
    /// The pool reserve below which a capture is skipped.
    pub pool_reserve: Option<u64>,
    /// How long an explicit stop waits for the captures in flight (a test
    /// setting; the stop is refused when they do not finish).
    pub stop_wait: Option<Duration>,
    journal: Mutex<JournalState>,
    /// Lane workspaces the cutover also captures (`services_track`).
    tracked: Mutex<BTreeMap<PathBuf, BTreeSet<String>>>,
    /// Captures whose snapshot may exist before anything records it.
    inflight: Mutex<BTreeMap<PathBuf, BTreeSet<String>>>,
    /// Per project: checkpoint captures share it; a stop cutover holds it
    /// exclusively, until the VM has stopped.
    gates: Mutex<BTreeMap<PathBuf, Arc<CaptureGate>>>,
}

/// An id in `inflight` for as long as its capture is being recorded.
struct Inflight<'a> {
    snapshots: &'a Snapshots,
    root: PathBuf,
    id: String,
}

impl<'a> Inflight<'a> {
    fn new(snapshots: &'a Snapshots, root: &Path, id: &str) -> Self {
        snapshots
            .inflight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(root.to_owned())
            .or_default()
            .insert(id.to_owned());
        Self {
            snapshots,
            root: root.to_owned(),
            id: id.to_owned(),
        }
    }
}

impl Drop for Inflight<'_> {
    fn drop(&mut self) {
        if let Some(set) = self
            .snapshots
            .inflight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(&self.root)
        {
            set.remove(&self.id);
        }
    }
}

/// A stop cutover in progress: the VM, what was frozen, and the agent's relay
/// that holds the write fence, so a stop that fails can thaw and release
/// exactly those.
pub(super) struct Cutover {
    vm: String,
    frozen: Vec<String>,
    relay: Option<marsh_sbx::services_relay::Relay>,
}

/// A stop under way: the capture exclusion, held until the VM has stopped (or
/// the stop failed and was undone), and the cutover it ran.
pub(super) struct StopPlan {
    _exclusion: Option<CaptureExclusion>,
    cutover: Option<Cutover>,
}

fn unix_millis_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// `fsync` a directory, so a rename or a new file in it is durable.
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    fs::File::open(dir)?.sync_all()
}

impl StockDaemonBackend {
    // ---------------------------------------------------------------- journal

    fn journal<R>(&self, use_state: impl FnOnce(&mut JournalState) -> R) -> R {
        let mut guard = self
            .services
            .snapshots
            .journal
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !guard.loaded {
            guard.loaded = true;
            let path = self.config.daemon_home.join(JOURNAL);
            let mut unreadable = false;
            match fs::read(&path) {
                Ok(bytes) => match serde_json::from_slice::<JournalFile>(&bytes) {
                    Ok(file) if matches!(file.version, 2 | 3) => guard.file = file,
                    _ => unreadable = true,
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => unreadable = true,
            }
            guard.marker = self.config.daemon_home.join(JOURNAL_LOST).exists();
            // A marker, or a file that cannot be read: the journal may be older
            // than the truth, whatever its entries say.
            let loss = guard.marker || unreadable;
            guard.lost = guard.file.lost || loss;
            if loss {
                guard.file.lost = true;
                for entry in guard.file.projects.values_mut() {
                    entry.suspect = true;
                }
                // Persist the distrust, which lets the marker go.
                let _ = self.persist_journal(&mut guard, false);
            }
        }
        use_state(&mut guard)
    }

    fn write_marker(&self) -> Result<(), String> {
        if marsh_contracts::break_mode(breaks::JOURNAL_MARKER_FAIL) {
            return Err("injected marker failure".into());
        }
        let home = &self.config.daemon_home;
        let mut file = fs::File::create(home.join(JOURNAL_LOST)).map_err(|e| e.to_string())?;
        file.write_all(b"lost\n").map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        sync_dir(home).map_err(|e| e.to_string())
    }

    /// Persist the journal durably. `strong`: the change must never be
    /// forgotten by a restart (a stop outcome, its clearing), so the marker is
    /// made durable *first* and removed only after the new journal is. Every
    /// write is file `fsync`, rename, directory `fsync`.
    ///
    /// If the journal cannot be written the marker invalidates the old one; if
    /// that cannot be written either the result is [`Persisted::Neither`] and
    /// the caller must not rely on what a restart would read.
    fn persist_journal(&self, state: &mut JournalState, strong: bool) -> Persisted {
        let home = &self.config.daemon_home;
        let path = home.join(JOURNAL);
        if strong && !state.marker && !marsh_contracts::break_mode(breaks::JOURNAL_INTENT_OFF) {
            if let Err(error) = self.write_marker() {
                eprintln!("marshd: could not mark the services journal as changing: {error}");
                return Persisted::Neither;
            }
            state.marker = true;
        }
        if strong && marsh_contracts::break_mode(breaks::JOURNAL_CRASH_AFTER_INTENT) {
            eprintln!("marshd: TEST BREAK: dying at the journal boundary");
            std::process::abort();
        }
        let written = if marsh_contracts::break_mode(breaks::JOURNAL_WRITE_FAIL) {
            Err("injected journal failure".to_owned())
        } else {
            state.file.version = 3;
            state.file.lost = state.lost;
            let temporary = path.with_extension("json.new");
            serde_json::to_vec(&state.file)
                .map_err(|error| error.to_string())
                .and_then(|bytes| {
                    let mut file = fs::File::create(&temporary).map_err(|e| e.to_string())?;
                    file.write_all(&bytes).map_err(|e| e.to_string())?;
                    file.sync_all().map_err(|e| e.to_string())
                })
                .and_then(|()| fs::rename(&temporary, &path).map_err(|e| e.to_string()))
                .and_then(|()| sync_dir(home).map_err(|e| e.to_string()))
        };
        match written {
            Ok(()) => {
                if state.marker {
                    // The new journal is durable (it carries any distrust): the
                    // marker has done its work. A marker that cannot be removed
                    // only makes a restart more careful.
                    if fs::remove_file(home.join(JOURNAL_LOST)).is_ok() && sync_dir(home).is_ok() {
                        state.marker = false;
                    }
                }
                Persisted::Saved
            }
            Err(error) => {
                eprintln!("marshd: could not journal the services captures: {error}");
                if state.marker {
                    return Persisted::Invalidated;
                }
                match self.write_marker() {
                    Ok(()) => {
                        state.marker = true;
                        Persisted::Invalidated
                    }
                    Err(error) => {
                        eprintln!(
                            "marshd: could not invalidate the services journal either: {error}"
                        );
                        Persisted::Neither
                    }
                }
            }
        }
    }

    /// The project's journal entry for the VM `uuid`: an entry of another VM
    /// (removed and recreated, or a crash between `services rm` and forgetting
    /// it) is replaced, never reused.
    fn project_entry<'a>(
        state: &'a mut JournalState,
        root: &Path,
        uuid: &str,
    ) -> Option<&'a mut ProjectJournal> {
        let key = root.to_str()?;
        let entry = state.file.projects.entry(key.to_owned()).or_default();
        if entry.vm_uuid != uuid && !marsh_contracts::break_mode(breaks::JOURNAL_VM_UNBOUND) {
            *entry = ProjectJournal {
                vm_uuid: uuid.to_owned(),
                ..ProjectJournal::default()
            };
        }
        Some(entry)
    }

    /// Remember `last` as the workspace's latest capture. `Err`: the journal
    /// could not be persisted at all (not even invalidated), so the capture
    /// must not count (the caller drops the snapshot and records a gap).
    fn remember_capture(
        &self,
        root: &Path,
        uuid: &str,
        workspace: &str,
        last: LastCapture,
    ) -> Result<(), String> {
        self.journal(|state| {
            let previous;
            let cleared;
            {
                let Some(entry) = Self::project_entry(state, root, uuid) else {
                    return Ok(());
                };
                previous = entry.last.insert(workspace.to_owned(), last);
                // The VM runs: no stop outcome applies any more.
                cleared = entry.stop.take();
            }
            match self.persist_journal(state, cleared.is_some()) {
                Persisted::Saved | Persisted::Invalidated => Ok(()),
                Persisted::Neither => {
                    if let Some(entry) = Self::project_entry(state, root, uuid) {
                        match previous {
                            Some(previous) => {
                                entry.last.insert(workspace.to_owned(), previous);
                            }
                            None => {
                                entry.last.remove(workspace);
                            }
                        }
                        entry.stop = cleared;
                    }
                    Err(
                        "the capture could not be recorded: the services journal cannot be written"
                            .to_owned(),
                    )
                }
            }
        })
    }

    /// Journal the stop's outcomes. `Err`: neither they nor an invalidation of
    /// the old journal could be persisted, and the stop must not happen.
    fn record_stop(
        &self,
        root: &Path,
        uuid: &str,
        workspaces: BTreeMap<String, StopOutcome>,
    ) -> Result<(), String> {
        self.journal(|state| {
            let previous;
            {
                let Some(entry) = Self::project_entry(state, root, uuid) else {
                    return Ok(());
                };
                previous = (entry.stop.take(), entry.suspect);
                entry.stop = Some(StopRecord {
                    at_unix_ms: unix_millis_now(),
                    workspaces,
                });
                // The project was captured again, whole: what an earlier loss
                // made suspect is settled.
                entry.suspect = false;
            }
            match self.persist_journal(state, true) {
                Persisted::Saved | Persisted::Invalidated => Ok(()),
                Persisted::Neither
                    if marsh_contracts::break_mode(breaks::JOURNAL_NEITHER_IGNORED) =>
                {
                    Ok(())
                }
                Persisted::Neither => {
                    if let Some(entry) = Self::project_entry(state, root, uuid) {
                        (entry.stop, entry.suspect) = previous;
                    }
                    Err("the stop outcome could not be recorded: the services journal cannot be written"
                        .to_owned())
                }
            }
        })
    }

    /// The VM is about to run again: the stop outcome no longer describes it.
    /// `Err`: that could not be made durable (the old outcome would be trusted
    /// after a restart), so the VM must not start.
    pub(super) fn clear_stop_outcome(&self, root: &Path) -> Result<(), String> {
        let Some(key) = root.to_str() else {
            return Ok(());
        };
        self.journal(|state| {
            let Some(entry) = state.file.projects.get_mut(key) else {
                return Ok(());
            };
            let Some(cleared) = entry.stop.take() else {
                return Ok(());
            };
            match self.persist_journal(state, true) {
                Persisted::Saved | Persisted::Invalidated => Ok(()),
                Persisted::Neither => {
                    if let Some(entry) = state.file.projects.get_mut(key) {
                        entry.stop = Some(cleared);
                    }
                    Err("the services journal cannot be written, so the VM cannot be started: a restart would trust the last stop's outcome"
                        .to_owned())
                }
            }
        })
    }

    /// The project's services data is gone (`services rm`, `reset
    /// --include-services`): what was recorded names snapshots that are not.
    pub(super) fn forget_last_capture(&self, root: &Path) {
        let Some(key) = root.to_str() else { return };
        self.journal(|state| {
            if state.file.projects.remove(key).is_some() {
                let _ = self.persist_journal(state, false);
            }
        });
        self.services
            .snapshots
            .tracked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(root);
    }

    fn vm_uuid(&self, root: &Path) -> Option<String> {
        self.sbx
            .services_vm_view(root, false)
            .ok()
            .flatten()
            .map(|view| view.uuid)
    }

    /// `last_capture` of a project's `services status --json`: the base
    /// workspace's.
    pub(super) fn last_capture_json(&self, root: &Path) -> serde_json::Value {
        let uuid = self.vm_uuid(root);
        let last = root.to_str().zip(uuid).and_then(|(key, uuid)| {
            self.journal(|state| {
                state
                    .file
                    .projects
                    .get(key)
                    .filter(|entry| entry.vm_uuid == uuid)
                    .and_then(|entry| entry.last.get(BASE_WORKSPACE).cloned())
            })
        });
        last.map_or(
            serde_json::Value::Null,
            |last| serde_json::json!({"id": last.id, "at": last.at_unix_ms, "reason": last.reason}),
        )
    }

    // ---------------------------------------------------------------- helpers

    fn gate(&self, root: &Path) -> Arc<CaptureGate> {
        Arc::clone(
            self.services
                .snapshots
                .gates
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(root.to_owned())
                .or_default(),
        )
    }

    /// The project's ready relay, if it has one. Never starts anything.
    pub(super) fn ready_relay(
        &self,
        root: &Path,
    ) -> Result<marsh_sbx::services_relay::Relay, String> {
        let relays = self
            .services
            .relays
            .as_ref()
            .ok_or("this daemon has no services engine")?;
        relays
            .get(root)
            .map(|entry| entry.relay().clone())
            .map_err(|error| error.to_string())
    }

    /// A hold for tests: `<point>.reached` appears under `test-holds`, and the
    /// caller waits (at most two minutes) for `<point>.release`.
    fn test_hold(&self, mode: &str, point: &str) {
        if !marsh_contracts::break_mode(mode) {
            return;
        }
        let dir = self.config.daemon_home.join("test-holds");
        let _ = fs::create_dir_all(&dir);
        let release = dir.join(format!("{point}.release"));
        let _ = fs::write(dir.join(format!("{point}.reached")), b"");
        let deadline = Instant::now() + Duration::from_mins(2);
        while !release.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = fs::remove_file(release);
        let _ = fs::remove_file(dir.join(format!("{point}.reached")));
    }

    /// The reserve sent with a capture request: the agent reads the pool's
    /// free space on its own pool descriptor when it admits the capture, and
    /// skips it below this (an unreadable pool is skipped too). Test defect:
    /// zero.
    fn reserve_bytes(&self) -> u64 {
        if marsh_contracts::break_mode(breaks::ADMIT_IGNORE_POOL) {
            return 0;
        }
        self.services
            .snapshots
            .pool_reserve
            .unwrap_or(DEFAULT_POOL_RESERVE)
    }

    // ---------------------------------------------------------------- capture

    /// `DaemonBackend::capture_services`.
    pub(super) fn services_capture(
        &self,
        root: &Path,
        workspace: &str,
        id: &str,
    ) -> ServicesCapture {
        let started = Instant::now();
        // Test break: the backend has no services connection.
        if marsh_contracts::break_mode(breaks::BACKEND_SERVICES_DISCONNECTED) {
            return ServicesCapture::Off;
        }
        let settings = match self.services_settings() {
            Ok(settings) => settings,
            Err(error) => return ServicesCapture::Gap(error.to_string()),
        };
        if !settings.enabled_for(root) {
            return ServicesCapture::Off;
        }
        // Test break: the checkpoint makes sure the VM exists and runs first.
        if marsh_contracts::break_mode(breaks::CAPTURE_ENSURE_VM) {
            let _ = self.services_start(root, &settings, None);
        }
        let view = match self.sbx.services_vm_view(root, true) {
            Err(error) => {
                return ServicesCapture::Gap(format!("services state unavailable: {error}"));
            }
            // Never created: nothing to capture, and nothing is created here.
            Ok(None) => return ServicesCapture::Off,
            Ok(Some(view)) => view,
        };
        if !view.running {
            return ServicesCapture::Stopped;
        }
        if self.services.with_project(root, |project| project.starting) {
            return ServicesCapture::Gap("the Docker VM is starting".into());
        }
        if !view.lease_live || view.generation.is_none() {
            return ServicesCapture::Gap("the Docker VM is not ready".into());
        }
        // Shared with other checkpoint captures (the agent answers an
        // overlapping one with its own busy gap), exclusive with a stop cutover.
        let Some(_capturing) = self.gate(root).try_share() else {
            return ServicesCapture::Gap("the Docker VM is stopping".into());
        };
        let relay = match self.ready_relay(root) {
            Ok(relay) => relay,
            Err(why) => return ServicesCapture::Gap(why),
        };
        let _recording = Inflight::new(&self.services.snapshots, root, id);
        let unbudgeted = marsh_contracts::break_mode(breaks::CAPTURE_NO_SKIP)
            || marsh_contracts::break_mode(breaks::CAPTURE_BLOCK_NEXT_PROMPT);
        let budget = (!unbudgeted).then(|| Budget {
            started,
            total: super::services_limits::capture_budget(&self.config.daemon_home),
        });
        self.test_hold(
            breaks::HOLD_BEFORE_SPACE_CHECK,
            "checkpoint-before-space-check",
        );
        match self.capture_through_within(
            &relay,
            root,
            &view.uuid,
            workspace,
            id,
            "checkpoint",
            budget,
        ) {
            Ok(record) => ServicesCapture::Captured(record),
            Err(why) => ServicesCapture::Gap(why),
        }
    }

    /// Capture `workspace` as `id` (the agent checks the reserve), validate the
    /// manifest and remember the snapshot as the workspace's last capture. `Err`
    /// is the gap's text; a snapshot that published with a manifest that does
    /// not validate, or that could not be journalled, is dropped again.
    #[allow(clippy::too_many_arguments)]
    fn capture_through(
        &self,
        relay: &marsh_sbx::services_relay::Relay,
        root: &Path,
        uuid: &str,
        workspace: &str,
        id: &str,
        reason: &str,
    ) -> Result<ServicesRecord, String> {
        self.capture_through_within(relay, root, uuid, workspace, id, reason, None)
    }

    /// `capture_through` under a time budget (a checkpoint's capture, P2): if
    /// the steps before the request used the budget, nothing is sent; the wait
    /// for the agent's answer ends with it. Either way the answer is a plain
    /// gap and the file checkpoint goes on. A capture the daemon stopped
    /// waiting for may still publish in the VM; nothing references it and the
    /// orphan collector drops it.
    #[allow(clippy::too_many_arguments)]
    fn capture_through_within(
        &self,
        relay: &marsh_sbx::services_relay::Relay,
        root: &Path,
        uuid: &str,
        workspace: &str,
        id: &str,
        reason: &str,
        budget: Option<Budget>,
    ) -> Result<ServicesRecord, String> {
        let reply = match budget {
            None => relay
                .capture(id, workspace, self.reserve_bytes())
                .map_err(|error| error.to_string())?,
            Some(budget) => {
                if budget.spent() {
                    eprintln!(
                        "marshd: services capture skipped: the budget was used before it could start"
                    );
                    return Err(budget.skipped());
                }
                relay
                    .capture_within(
                        id,
                        workspace,
                        self.reserve_bytes(),
                        Some(budget.remaining()),
                    )
                    .map_err(|error| match error {
                        marsh_sbx::services_relay::RelayError::Timeout(_) => {
                            eprintln!("marshd: services capture skipped: the agent did not answer within the budget");
                            budget.skipped()
                        }
                        other => other.to_string(),
                    })?
            }
        };
        if reply.outcome != "published" {
            return Err(reply
                .gaps
                .first()
                .cloned()
                .unwrap_or_else(|| format!("the services capture was {}", reply.outcome)));
        }
        let record = match ServicesRecord::from_manifest(
            id,
            workspace,
            reply.manifest.as_ref(),
            &reply.gaps,
        ) {
            Ok(record) => record,
            Err(why) => {
                let _ = relay.drop_snapshot(id);
                return Err(format!("the snapshot's manifest was refused: {why}"));
            }
        };
        if let Err(why) = self.remember_capture(
            root,
            uuid,
            workspace,
            LastCapture {
                id: id.to_owned(),
                at_unix_ms: unix_millis_now(),
                reason: reason.to_owned(),
                record: record.clone(),
            },
        ) {
            let _ = relay.drop_snapshot(id);
            return Err(why);
        }
        Ok(record)
    }

    /// `DaemonBackend::drop_services`. The feature need not be on: turning
    /// services off keeps the data, and what a checkpoint's removal frees
    /// should still be freed.
    pub(super) fn services_drop(&self, root: &Path, id: &str) -> Result<bool, String> {
        let relay = self.ready_relay(root)?;
        let existed = relay.drop_snapshot(id).map_err(|error| error.to_string())?;
        // A last capture that is gone is not a reuse source.
        if let Some(key) = root.to_str() {
            self.journal(|state| {
                if let Some(entry) = state.file.projects.get_mut(key) {
                    let before = entry.last.len();
                    entry.last.retain(|_, last| last.id != id);
                    if entry.last.len() != before {
                        let _ = self.persist_journal(state, false);
                    }
                }
            });
        }
        Ok(existed)
    }

    /// `DaemonBackend::list_services`.
    pub(super) fn services_list(&self, root: &Path) -> Result<Vec<ServicesSnapshotInfo>, String> {
        let relay = self.ready_relay(root)?;
        let listed = relay.list().map_err(|error| error.to_string())?;
        Ok(listed
            .into_iter()
            .map(|entry| ServicesSnapshotInfo {
                id: entry.id,
                created_unix_ms: entry.created_unix_ms,
            })
            .collect())
    }

    /// `DaemonBackend::services_track`.
    pub(super) fn services_track_workspace(&self, root: &Path, workspace: &str, tracked: bool) {
        if workspace == BASE_WORKSPACE {
            return;
        }
        {
            let mut map = self
                .services
                .snapshots
                .tracked
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if tracked {
                map.entry(root.to_owned())
                    .or_default()
                    .insert(workspace.to_owned());
            } else if let Some(set) = map.get_mut(root) {
                set.remove(workspace);
            }
        }
        if !tracked && let Some(key) = root.to_str() {
            // No checkpoint is left in the lane: its capture is no longer protected.
            self.journal(|state| {
                if let Some(entry) = state.file.projects.get_mut(key) {
                    let had = entry.last.remove(workspace).is_some();
                    if let Some(stop) = entry.stop.as_mut() {
                        stop.workspaces.remove(workspace);
                    }
                    if entry.last.is_empty() && entry.stop.is_none() {
                        state.file.projects.remove(key);
                    }
                    if had {
                        let _ = self.persist_journal(state, false);
                    }
                }
            });
        }
    }

    // ------------------------------------------------------------------ reuse

    /// `DaemonBackend::services_reuse`: what a checkpoint of `workspace`, made
    /// while the VM is stopped, may reference.
    pub(super) fn services_reuse_decision(&self, root: &Path, workspace: &str) -> ServicesReuse {
        // Test break: a stopped VM's checkpoint finds nothing to reference.
        if marsh_contracts::break_mode(breaks::REUSE_LAST_SNAPSHOT_OFF) {
            return ServicesReuse::None;
        }
        let Some(key) = root.to_str() else {
            return ServicesReuse::None;
        };
        let Some(uuid) = self.vm_uuid(root) else {
            return ServicesReuse::None;
        };
        self.journal(|state| {
            let entry = state
                .file
                .projects
                .get(key)
                .filter(|entry| {
                    entry.vm_uuid == uuid || marsh_contracts::break_mode(breaks::JOURNAL_VM_UNBOUND)
                })
                .cloned();
            let silent = marsh_contracts::break_mode(breaks::JOURNAL_LOSS_SILENT);
            let Some(entry) = entry else {
                // Nothing recorded for this VM: a new VM never captured (none),
                // unless what marsh knew was lost (unknown).
                return if state.lost && !silent {
                    ServicesReuse::Unknown
                } else {
                    ServicesReuse::None
                };
            };
            // A restart found the journal possibly older than the truth: an
            // entry that survived (even a `proven` outcome) is not trusted.
            if entry.suspect && !silent {
                return ServicesReuse::Unknown;
            }
            match entry.stop.as_ref().and_then(|stop| stop.workspaces.get(workspace)) {
                Some(StopOutcome::Proven { id }) => entry
                    .last
                    .get(workspace)
                    .filter(|last| &last.id == id)
                    .map_or_else(
                        || {
                            ServicesReuse::Gap(
                                "this turn's Docker data was not saved: the snapshot taken when the Docker VM stopped is gone"
                                    .into(),
                            )
                        },
                        |last| ServicesReuse::Reuse {
                            last: last.clone(),
                            warning: None,
                        },
                    ),
                Some(StopOutcome::Failed { reason }) => {
                    // Test break: the failure is ignored and the old snapshot
                    // is reused as if it were this turn's data.
                    if marsh_contracts::break_mode(breaks::REUSE_IGNORES_FAILURE)
                        && let Some(last) = entry.last.get(workspace)
                    {
                        return ServicesReuse::Reuse {
                            last: last.clone(),
                            warning: None,
                        };
                    }
                    ServicesReuse::Gap(format!(
                        "this turn's Docker data was not saved: the Docker VM stopped without a final capture of it ({reason})"
                    ))
                }
                None if entry.stop.is_some() => ServicesReuse::Gap(
                    "this turn's Docker data was not saved: this workspace was not captured when the Docker VM stopped"
                        .into(),
                ),
                // A stop marsh did not cause (an external `sbx stop`, G13, or a
                // record that did not survive a restart).
                None => match entry.last.get(workspace) {
                    Some(last) => ServicesReuse::Reuse {
                        last: last.clone(),
                        warning: Some(
                            "the Docker VM was stopped outside marsh: changes after this snapshot are not saved"
                                .into(),
                        ),
                    },
                    None => ServicesReuse::None,
                },
            }
        })
    }

    /// `DaemonBackend::services_protected`.
    pub(super) fn services_protected_ids(&self, root: &Path) -> BTreeSet<String> {
        let mut protected: BTreeSet<String> = self
            .services
            .snapshots
            .inflight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(root)
            .cloned()
            .unwrap_or_default();
        if let Some(key) = root.to_str() {
            self.journal(|state| {
                if let Some(entry) = state.file.projects.get(key) {
                    protected.extend(entry.last.values().map(|last| last.id.clone()));
                }
            });
        }
        protected
    }

    /// `DaemonBackend::services_snapshot_roots`: every project with a journal
    /// entry and every owned services VM (from the ownership map, found with
    /// no checkpoint record and without starting anything).
    pub(super) fn services_snapshot_root_list(&self) -> Vec<PathBuf> {
        let mut roots: BTreeSet<PathBuf> =
            self.journal(|state| state.file.projects.keys().map(PathBuf::from).collect());
        roots.extend(
            self.sbx
                .owned_services_vms()
                .into_iter()
                .map(|(_, key)| PathBuf::from(key))
                .filter(|key| key.is_absolute()),
        );
        roots.into_iter().collect()
    }

    /// `DaemonBackend::services_reconcile`: lane captures journalled for a lane
    /// whose checkpoint record did not survive (a crash between publishing a
    /// lane's snapshot and recording its checkpoint, or its directory removed)
    /// are untracked, so the journal stops protecting them and the collector
    /// reclaims them unless a record, a pending checkpoint or a lease still
    /// names them. The base workspace is never untracked.
    pub(super) fn services_reconcile_lanes(&self, live: &BTreeSet<(PathBuf, String)>) {
        if marsh_contracts::break_mode(breaks::LANE_RECONCILE_OFF) {
            return;
        }
        self.journal(|state| {
            let mut changed = false;
            let keys: Vec<String> = state.file.projects.keys().cloned().collect();
            for key in keys {
                let root = PathBuf::from(&key);
                let Some(entry) = state.file.projects.get_mut(&key) else {
                    continue;
                };
                let orphans: Vec<String> = entry
                    .last
                    .keys()
                    .filter(|ws| {
                        ws.as_str() != BASE_WORKSPACE && !live.contains(&(root.clone(), (*ws).clone()))
                    })
                    .cloned()
                    .collect();
                for workspace in orphans {
                    eprintln!(
                        "marshd: services capture of lane workspace {workspace} of {key} has no checkpoint record: no longer protected"
                    );
                    entry.last.remove(&workspace);
                    if let Some(stop) = entry.stop.as_mut() {
                        stop.workspaces.remove(&workspace);
                    }
                    changed = true;
                }
                if entry.last.is_empty() && entry.stop.is_none() && !entry.suspect {
                    state.file.projects.remove(&key);
                    changed = true;
                }
            }
            if changed {
                let _ = self.persist_journal(state, false);
            }
        });
    }

    // ----------------------------------------------------------- stop cutover

    /// How long an explicit stop waits for the captures in flight (a test
    /// setting overrides `default`).
    pub(super) fn stop_wait(&self, default: Duration) -> Duration {
        self.services.snapshots.stop_wait.unwrap_or(default)
    }

    /// The first half of every stop marsh causes: exclude the project's
    /// checkpoint captures (waiting at most `wait`) and run the cutover. The
    /// exclusion lives in the returned plan, so the caller holds it through
    /// closing the relay and the VM actually stopping, and drops it after.
    /// `Err`: the stop must not happen (a capture is still running, or the
    /// outcome could not be journalled); nothing is left frozen.
    pub(super) fn services_stop_prepare(
        &self,
        root: &Path,
        reason: &str,
        wait: Duration,
        deadline: Option<Instant>,
    ) -> Result<StopPlan, StopRefused> {
        let exclusion = match self.gate(root).exclusive(wait) {
            Some(exclusion) => Some(exclusion),
            None if marsh_contracts::break_mode(breaks::CAPTURE_GATE_IGNORED)
                || (reason == "idle-stop"
                    && marsh_contracts::break_mode(breaks::IDLE_IGNORES_INFLIGHT)) =>
            {
                None
            }
            None => {
                return Err(StopRefused::Busy(format!(
                    "a checkpoint capture of {} is still running",
                    root.display()
                )));
            }
        };
        let cutover = self
            .services_cutover(root, reason, deadline)
            .map_err(StopRefused::Unsafe)?;
        Ok(StopPlan {
            _exclusion: exclusion,
            cutover,
        })
    }

    /// The stop did not happen: undo the cutover (thaw, release the fence,
    /// forget the outcome). The exclusion is released when the plan is dropped.
    pub(super) fn services_stop_abort(&self, root: &Path, plan: &StopPlan) {
        if let Some(cutover) = &plan.cutover {
            self.services_cutover_abort(root, cutover);
        }
    }

    /// The cutover (DECISIONS 12, extended): before a stop marsh causes, fence
    /// the workspaces, freeze the engine, capture every tracked workspace, ask
    /// the engine what runs, and record each outcome, so a checkpoint made while
    /// the VM is stopped references a snapshot only when it is known to hold
    /// the last write. The caller holds the project's operation lock and the
    /// capture exclusion, and calls [`Self::services_cutover_abort`] if the stop
    /// then fails. `Ok(None)`: the VM is not running, so there is nothing to cut
    /// over. `Err`: the outcome could not be journalled at all; the cutover has
    /// been undone and the stop must not go ahead.
    #[allow(clippy::too_many_lines)] // One ordered sequence: fence, freeze, capture, verify, record.
    pub(super) fn services_cutover(
        &self,
        root: &Path,
        reason: &str,
        deadline: Option<Instant>,
    ) -> Result<Option<Cutover>, String> {
        let Some(view) = self
            .sbx
            .services_vm_view(root, true)
            .ok()
            .flatten()
            .filter(|view| view.running)
        else {
            return Ok(None);
        };
        let mut workspaces: BTreeSet<String> = BTreeSet::from([BASE_WORKSPACE.to_owned()]);
        workspaces.extend(
            self.services
                .snapshots
                .tracked
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(root)
                .cloned()
                .unwrap_or_default(),
        );
        let mut cutover = Cutover {
            vm: view.name.clone(),
            frozen: Vec::new(),
            relay: None,
        };
        // Everything failed: record why for every workspace; the stop goes on
        // and every checkpoint made while the VM is stopped says so.
        let failed_all = |why: String, cutover: Cutover| -> Result<Option<Cutover>, String> {
            let outcomes = workspaces
                .iter()
                .map(|ws| {
                    (
                        ws.clone(),
                        StopOutcome::Failed {
                            reason: why.clone(),
                        },
                    )
                })
                .collect();
            if let Err(error) = self.record_stop(root, &view.uuid, outcomes) {
                self.services_cutover_abort(root, &cutover);
                return Err(error);
            }
            eprintln!(
                "marshd: no final services capture of {}: {why}",
                root.display()
            );
            Ok(Some(cutover))
        };
        // Test break: the stop takes no final capture at all and records nothing.
        if marsh_contracts::break_mode(breaks::IDLE_FINAL_CAPTURE_OFF) {
            return Ok(Some(cutover));
        }
        if deadline
            .is_some_and(|d| d.saturating_duration_since(Instant::now()) < Duration::from_secs(20))
        {
            return failed_all("there was no time for a final capture".into(), cutover);
        }
        let relay = match self.ready_relay(root) {
            Ok(relay) => relay,
            Err(why) => return failed_all(format!("no ready relay: {why}"), cutover),
        };
        // 1. The write fence: the filter's freeze gate on every tracked
        // workspace, held until the VM stops. (Test break: not taken.)
        if !marsh_contracts::break_mode(breaks::STOP_FENCE_OFF) {
            let fenced: Vec<String> = workspaces.iter().cloned().collect();
            match relay.fence(&fenced) {
                Ok(()) => cutover.relay = Some(relay.clone()),
                Err(error) => {
                    return failed_all(
                        format!("the services could not be fenced against writes ({error})"),
                        cutover,
                    );
                }
            }
        }
        // 2. Pause every running container. (Test break: nothing is paused and
        // nothing is counted, so containers run on between capture and stop.)
        let pausing = !marsh_contracts::break_mode(breaks::STOP_PAUSE_OFF);
        if pausing {
            match self.sbx.services_quiesce(&view.name) {
                Ok(ids) => cutover.frozen = ids,
                Err(error) => {
                    return failed_all(
                        format!("the services could not be frozen ({error})"),
                        cutover,
                    );
                }
            }
        }
        // 3. Capture every tracked workspace.
        let mut outcomes: BTreeMap<String, StopOutcome> = BTreeMap::new();
        for workspace in &workspaces {
            // Test break: only the base workspace is captured, yet every
            // lane is recorded as captured.
            if marsh_contracts::break_mode(breaks::STOP_BASE_ONLY) && workspace != BASE_WORKSPACE {
                let id = self
                    .journal(|state| {
                        root.to_str()
                            .and_then(|key| state.file.projects.get(key))
                            .and_then(|entry| entry.last.get(workspace))
                            .map(|last| last.id.clone())
                    })
                    .unwrap_or_default();
                outcomes.insert(workspace.clone(), StopOutcome::Proven { id });
                continue;
            }
            let mut result = Err("not attempted".to_owned());
            let mut id = String::new();
            for attempt in 0..FINAL_CAPTURE_ATTEMPTS {
                id = new_snapshot_id();
                let _recording = Inflight::new(&self.services.snapshots, root, &id);
                result = self.capture_through(&relay, root, &view.uuid, workspace, &id, reason);
                let busy = result
                    .as_ref()
                    .err()
                    .is_some_and(|why| why.contains("another capture in progress"));
                if !busy || attempt + 1 == FINAL_CAPTURE_ATTEMPTS {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            outcomes.insert(
                workspace.clone(),
                match result {
                    Ok(record) => {
                        eprintln!(
                            "marshd: final services capture {id} of {} workspace {workspace} ({} volumes, {} gaps)",
                            root.display(),
                            record.volumes.len(),
                            record.gaps.len()
                        );
                        StopOutcome::Proven { id }
                    }
                    Err(why) => StopOutcome::Failed { reason: why },
                },
            );
            self.test_hold(breaks::HOLD_CUTOVER_AFTER_CAPTURE, "cutover-after-capture");
        }
        // 4. Still frozen? Whatever the engine runs now could have written
        // after its capture, and an engine that cannot be asked proves nothing.
        if pausing {
            let verdict = match self.sbx.services_running_count(&view.name) {
                Ok(0) => None,
                Ok(running) => Some(format!(
                    "{running} containers were running after the services were frozen"
                )),
                Err(error) => Some(format!(
                    "the engine could not be asked what was running ({error})"
                )),
            };
            if let Some(why) = verdict {
                for outcome in outcomes.values_mut() {
                    *outcome = StopOutcome::Failed {
                        reason: why.clone(),
                    };
                }
            }
        }
        self.test_hold(breaks::HOLD_CUTOVER_AFTER_COUNT, "cutover-after-count");
        // A lane whose last checkpoint was removed while this ran is untracked
        // again (its capture is no longer protected, and has no outcome).
        let tracked_now = self
            .services
            .snapshots
            .tracked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(root)
            .cloned()
            .unwrap_or_default();
        for workspace in &workspaces {
            if workspace != BASE_WORKSPACE && !tracked_now.contains(workspace) {
                self.services_track_workspace(root, workspace, false);
                outcomes.remove(workspace);
            }
        }
        // 5. Journal each outcome before the stop. Not persistable: no stop.
        if let Err(error) = self.record_stop(root, &view.uuid, outcomes) {
            self.services_cutover_abort(root, &cutover);
            return Err(error);
        }
        Ok(Some(cutover))
    }

    /// The stop did not happen: thaw what the cutover froze, release the write
    /// fence, and forget the outcome (the VM runs on and will write).
    pub(super) fn services_cutover_abort(&self, root: &Path, cutover: &Cutover) {
        if !cutover.frozen.is_empty()
            && let Err(error) = self.sbx.services_unquiesce(&cutover.vm, &cutover.frozen)
        {
            eprintln!(
                "marshd: could not thaw the services of {} after a failed stop: {error}",
                root.display()
            );
        }
        if let Some(relay) = &cutover.relay
            && let Err(error) = relay.unfence()
        {
            // A relay that is gone took the agent, and the fence with it.
            eprintln!(
                "marshd: could not release the write fence of {}: {error}",
                root.display()
            );
        }
        let _ = self.clear_stop_outcome(root);
    }
}

/// Why a stop was not started.
#[derive(Debug)]
pub(super) enum StopRefused {
    /// A checkpoint capture is still running (exclusion not acquired in time).
    Busy(String),
    /// The cutover could not be made safe (its outcome cannot be journalled).
    Unsafe(String),
}

impl StopRefused {
    pub(super) fn message(&self) -> &str {
        match self {
            Self::Busy(why) | Self::Unsafe(why) => why,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::breaks;
    use super::{CaptureExclusion, CaptureGate, CaptureShare};
    use marsh_daemon::{checkpoint, known_test_breaks};
    use std::{sync::Arc, time::Duration};

    /// The capture exclusion of a stop (table of steps): a stop waits for the
    /// captures in flight and gets the exclusion only when they are done; while it
    /// holds it no capture starts; captures share the gate; and the exclusion is an
    /// owned guard, held until it is dropped (through the relay closing and the stop).
    #[test]
    fn the_capture_gate_excludes_a_stop_until_it_is_dropped() {
        enum Step {
            Share(&'static str),
            EndShare(&'static str),
            Exclusive(&'static str, u64, bool),
            ShareRefused,
            Drop(&'static str),
        }
        use Step::{Drop, EndShare, Exclusive, Share, ShareRefused};
        let table: [(&str, Vec<Step>); 4] = [
            (
                "shares overlap",
                vec![Share("a"), Share("b"), EndShare("a"), EndShare("b")],
            ),
            (
                "a held capture refuses the stop after its wait, and the stop gets it once the capture ends",
                vec![
                    Share("a"),
                    Exclusive("stop", 50, false),
                    EndShare("a"),
                    Exclusive("stop", 50, true),
                ],
            ),
            (
                "the stop holds the exclusion until dropped: no capture starts, no second stop",
                vec![
                    Exclusive("stop", 0, true),
                    ShareRefused,
                    Exclusive("other", 50, false),
                    Drop("stop"),
                    Share("a"),
                    EndShare("a"),
                ],
            ),
            (
                "nothing is held after the stop is dropped",
                vec![
                    Exclusive("stop", 0, true),
                    Drop("stop"),
                    Exclusive("again", 0, true),
                    Drop("again"),
                    Share("a"),
                ],
            ),
        ];
        for (what, steps) in table {
            let gate = Arc::new(CaptureGate::default());
            let mut shares: Vec<(&str, CaptureShare)> = Vec::new();
            let mut exclusions: Vec<(&str, CaptureExclusion)> = Vec::new();
            for step in steps {
                match step {
                    Share(name) => shares.push((
                        name,
                        gate.try_share()
                            .unwrap_or_else(|| panic!("{what}: share {name}")),
                    )),
                    EndShare(name) => shares.retain(|(n, _)| *n != name),
                    Exclusive(name, wait, expected) => {
                        let got = gate.exclusive(Duration::from_millis(wait));
                        assert_eq!(got.is_some(), expected, "{what}: exclusive {name}");
                        if let Some(got) = got {
                            exclusions.push((name, got));
                        }
                    }
                    ShareRefused => assert!(
                        gate.try_share().is_none(),
                        "{what}: a capture starts under a stop"
                    ),
                    Drop(name) => exclusions.retain(|(n, _)| *n != name),
                }
            }
        }
    }

    /// Every break mode C12 reads through `break_mode` is attested by the
    /// daemon in a test build, however the build was asked for (package-scoped
    /// `marsh-backend --features test-breaks` forwards to the daemon's own
    /// list), and a release build attests only `demo`.
    #[test]
    fn the_daemon_attests_exactly_the_modes_this_build_can_honor() {
        let known = known_test_breaks();
        if cfg!(feature = "test-breaks") {
            for mode in [
                breaks::BACKEND_SERVICES_DISCONNECTED,
                breaks::CAPTURE_ENSURE_VM,
                breaks::REUSE_LAST_SNAPSHOT_OFF,
                breaks::IDLE_FINAL_CAPTURE_OFF,
                breaks::ADMIT_IGNORE_POOL,
                breaks::STOP_FENCE_OFF,
                breaks::STOP_PAUSE_OFF,
                breaks::STOP_BASE_ONLY,
                breaks::REUSE_IGNORES_FAILURE,
                breaks::IDLE_IGNORES_INFLIGHT,
                breaks::CAPTURE_GATE_IGNORED,
                breaks::JOURNAL_WRITE_FAIL,
                breaks::JOURNAL_MARKER_FAIL,
                breaks::JOURNAL_CRASH_AFTER_INTENT,
                breaks::JOURNAL_INTENT_OFF,
                breaks::JOURNAL_NEITHER_IGNORED,
                breaks::JOURNAL_VM_UNBOUND,
                breaks::JOURNAL_LOSS_SILENT,
                breaks::LANE_RECONCILE_OFF,
                breaks::HOLD_BEFORE_SPACE_CHECK,
                breaks::HOLD_CUTOVER_AFTER_CAPTURE,
                breaks::HOLD_CUTOVER_AFTER_COUNT,
                marsh_sbx::services_engine_query_breaks::FAULT_ENGINE_QUERY,
                marsh_sbx::services_engine_query_breaks::ENGINE_QUERY_FAIL_OPEN,
                checkpoint::breaks::RETENTION_KEEP_SNAPSHOTS,
                checkpoint::breaks::ORPHAN_GC_OFF,
                checkpoint::breaks::REUSE_LEASE_OFF,
                checkpoint::breaks::CAPTURE_ADMISSION_BLOCKS,
                checkpoint::breaks::ORPHAN_ROOTS_FROM_RECORDS,
                checkpoint::breaks::LANE_CAPTURE_ROOT,
                // C4, C5, P2: each defect under both its names.
                crate::services::SERVICES_FEATURE_OFF,
                crate::services::FEATURE_HIDDEN,
                marsh_daemon::ignored::breaks::IGNORED_SKIP,
                marsh_daemon::ignored::breaks::IGNORED_CAPTURE_OFF,
                marsh_daemon::ignored::breaks::IGNORED_FOLLOW_SYMLINK,
                marsh_daemon::ignored::breaks::IGNORED_FOLLOW_OUTSIDE_SYMLINK,
                breaks::CAPTURE_NO_SKIP,
                breaks::CAPTURE_BLOCK_NEXT_PROMPT,
                checkpoint::breaks::MANIFEST_REFS_UNCHECKED,
            ] {
                assert!(
                    known.contains(&mode),
                    "{mode} is not attested by the daemon"
                );
            }
        } else {
            assert_eq!(known, ["demo"]);
        }
    }
}
