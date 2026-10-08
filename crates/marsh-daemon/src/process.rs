//! Nested processes (`docs/design/processes.md`): one lineage tree, one admission
//! for every job (pool, depth, same-Kit chain, fan-out, total, spawn set),
//! the job capability's `ProcessRun` relay, and `ProcessShow`.
//! Model: `docs/model/Process.tla`.

use crate::{
    AttachmentFrame, CleanupState, Client, DaemonError, DaemonStore, ErrorCode, ExecuteSpec,
    JobLineage, JobReceipt, JobState, PublicMount, PublicReply, SessionSpec, State, read_frame,
    write_frame,
};
use marsh_contracts::process::{
    DEPTH_LIMIT, FAN_OUT_LIMIT, MIN_CHILD_WALL_MS, POOL_LIMIT, SAME_KIT_LIMIT, TOTAL_LIMIT,
    TREE_KIT_VM_LIMIT, TTY_REFUSAL, narrow_spawn,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

/// INT, then KILL after this grace (`docs/design/processes.md` s8).
const CANCEL_GRACE: Duration = Duration::from_secs(10);

/// This daemon's environment key (`job.json` digests, s6): random per
/// daemon process, given to workers, never to a job. A restart revokes every
/// capability, so no digest outlives its key.
///
/// # Panics
/// Panics if the OS random source fails (the daemon cannot run without it).
#[must_use]
pub fn env_key() -> &'static [u8; 32] {
    static KEY: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
    KEY.get_or_init(|| {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).expect("the OS random source is available");
        key
    })
}

/// Who starts a job, and the spawn set it asks for. Only the daemon's own
/// endpoint (master token) may name a parent job; a relay may only narrow.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessLink {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_job: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn: Option<Vec<String>>,
    /// A split argv branch: its view is the fork the split engine admitted
    /// (only splits narrow a view), not the parent's mounts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub branch: bool,
    /// From a job's link: starting digests of offered variables the job did
    /// not receive from its parent (`process::job_offer`); the daemon drops
    /// those still unchanged.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub start_env: BTreeMap<String, String>,
}

/// `marsh status` `processes`.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProcessCounts {
    pub running: usize,
    pub refused: u64,
    pub held: usize,
}

/// Process-table state beside the receipts. Held slots are derived from
/// the receipts (`cleanup_uncertain` after the last reset of their Kit), so
/// they survive a daemon restart; only the reset times are persisted.
#[derive(Debug, Default)]
pub(crate) struct ProcessTable {
    pub(crate) refused: u64,
    /// Refused launches per tree root: they count toward `TotalBound`.
    refused_roots: BTreeMap<String, usize>,
    /// Last successful `workers reset` per Kit command (`all` for every Kit),
    /// unix ms; persisted in `process-resets.json` in the control home.
    resets: BTreeMap<String, u64>,
    /// Jobs whose tree was cancelled (root Ctrl-C, a dropped caller): they
    /// start nothing more and end `cancelled` (s8).
    pub(crate) cancelling: std::collections::BTreeSet<String>,
    /// Terminal jobs whose user typed the interrupt character (Ctrl-C reaches
    /// the job's own PTY as a byte, not as a forwarded signal). A job that
    /// then ends with status 130 (SIGINT) ends `cancelled` (s8).
    pub(crate) interrupted: std::collections::BTreeSet<String>,
    /// How to signal each running job's container (`INT`, `KILL`).
    controls: BTreeMap<String, JobControl>,
}

/// Sends a signal (`INT`, `KILL`) to one running job's container.
#[derive(Clone)]
pub struct JobControl(pub Arc<dyn Fn(&str) + Send + Sync>);

impl std::fmt::Debug for JobControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("JobControl")
    }
}

const RESETS: &str = "process-resets.json";

impl ProcessTable {
    pub(crate) fn load(control_home: &Path) -> Self {
        Self {
            resets: std::fs::read(control_home.join(RESETS))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default(),
            ..Self::default()
        }
    }
}

fn live(job: &JobReceipt) -> bool {
    matches!(job.state, JobState::Queued | JobState::Running)
}

/// A job whose end was never confirmed holds its pool slot until its Kit is
/// reset (`BudgetBound`, control `freeOnRevoke`).
fn holds_slot(table: &ProcessTable, job: &JobReceipt) -> bool {
    let ended = job.finished_unix_ms.unwrap_or(job.created_unix_ms);
    !live(job)
        && job.cleanup == CleanupState::Uncertain
        && [job.command.as_str(), "all"]
            .iter()
            .all(|kit| table.resets.get(*kit).is_none_or(|reset| *reset < ended))
}

fn held(state: &State) -> impl Iterator<Item = &JobReceipt> {
    state
        .jobs
        .values()
        .filter(|job| holds_slot(&state.process, job))
}

/// The tree root of `job` (itself for a root).
fn root_of(state: &State, job: &str) -> String {
    state
        .jobs
        .get(job)
        .and_then(|job| job.lineage.as_ref())
        .map(|lineage| lineage.root.clone())
        .filter(|root| !root.is_empty())
        .unwrap_or_else(|| job.to_owned())
}

/// A child's view: its parent's recorded mounts, verbatim
/// (`Attenuation`); its cwd must lie inside them.
///
/// # Errors
/// Returns the refusal when the cwd is outside the parent's view.
pub fn child_view(parent: &[PublicMount], cwd: &Path) -> Result<Vec<PublicMount>, String> {
    if parent.iter().any(|mount| cwd.starts_with(&mount.target)) {
        Ok(parent.to_vec())
    } else {
        Err(format!(
            "{} is outside this job's view; a child sees only its parent's mounts",
            cwd.display()
        ))
    }
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Admission under the process-table (store) lock: the lineage the new job
/// gets, or the refusal. `kit` is the child's Kit identity once known
/// (the same-Kit chain needs it; the pre-check passes `None`).
pub(crate) fn admit(
    state: &State,
    session: &str,
    command: &str,
    kit: Option<&str>,
    link: &ProcessLink,
    registered: &[String],
) -> Result<(JobLineage, Vec<String>), String> {
    let lineage = match &link.parent_job {
        None => JobLineage {
            parent: format!("session:{session}"),
            depth: 1,
            spawn: registered.to_vec(),
            ..JobLineage::default()
        },
        // A split argv branch is bounded by the split depth limit; it is
        // not a same-Kit step (`docs/design/processes.md` s13). It does use a Kit VM.
        Some(id) => child_lineage(state, id, command, kit, link.branch)?,
    };
    let running = state.jobs.values().filter(|job| live(job)).count();
    let held_jobs = held(state).collect::<Vec<_>>();
    let held = held_jobs.len();
    if running + held >= POOL_LIMIT {
        let mut message = format!("capacity: {POOL_LIMIT} jobs");
        if held > 0 {
            let mut kits = held_jobs
                .iter()
                .map(|job| job.command.clone())
                .collect::<Vec<_>>();
            kits.sort();
            kits.dedup();
            let kits = kits.join(",");
            let _ = write!(
                message,
                " ({held} held by uncertain jobs on Kit {kits}: run marsh workers reset {kits})"
            );
        }
        return Err(message);
    }
    let (spawn, dropped) = narrow_spawn(&lineage.spawn, link.spawn.as_deref());
    Ok((JobLineage { spawn, ..lineage }, dropped))
}

/// A child's lineage under its parent, or the refusal (spawn set, depth,
/// fan-out, total, same-Kit chain).
fn child_lineage(
    state: &State,
    id: &String,
    command: &str,
    kit: Option<&str>,
    branch: bool,
) -> Result<JobLineage, String> {
    let parent = state
        .jobs
        .get(id)
        .ok_or_else(|| format!("parent job {} is gone", short(id)))?;
    // Re-checked under the lock: a cancelled or ended parent starts
    // nothing (`Spawn` is atomic in the model).
    if !live(parent) {
        return Err(format!("parent job {} has ended", short(id)));
    }
    if state.process.cancelling.contains(id) {
        return Err(format!("parent job {} is being cancelled", short(id)));
    }
    let inherited = parent.lineage.clone().unwrap_or_default();
    let root = if inherited.root.is_empty() {
        id.clone()
    } else {
        inherited.root.clone()
    };
    if !inherited.spawn.iter().any(|name| name == command) {
        return Err(format!(
            "spawn refused: {command} not in this job's spawn set (MARSH_SPAWN)"
        ));
    }
    let depth = inherited.depth.max(1) + 1;
    if depth > DEPTH_LIMIT {
        return Err(format!(
            "depth limit {DEPTH_LIMIT}: {command} would run at depth {depth}"
        ));
    }
    let parent_ref = format!("job:{id}");
    let children = state
        .jobs
        .values()
        .filter(|job| {
            live(job)
                && job
                    .lineage
                    .as_ref()
                    .is_some_and(|lineage| lineage.parent == parent_ref)
        })
        .count();
    if children >= FAN_OUT_LIMIT {
        return Err(format!(
            "fan-out limit {FAN_OUT_LIMIT}: job {} already has {children} live children",
            short(id)
        ));
    }
    let tree = state
        .jobs
        .values()
        .filter(|job| {
            job.job_id == root
                || job
                    .lineage
                    .as_ref()
                    .is_some_and(|lineage| lineage.root == root)
        })
        .count()
        + state.process.refused_roots.get(&root).copied().unwrap_or(0);
    if tree >= TOTAL_LIMIT {
        return Err(format!("total limit {TOTAL_LIMIT} launches in this tree"));
    }
    wall_time_left(&inherited, id, command)?;
    if let Some(kit) = kit {
        kit_vm_cap(state, &root, command, kit)?;
    }
    if let Some(kit) = kit.filter(|_| !branch) {
        let mut chain = vec![command.to_owned()];
        let mut cursor = Some(parent);
        while let Some(job) = cursor.filter(|job| job.kit_ref == kit) {
            chain.push(job.command.clone());
            if job
                .lineage
                .as_ref()
                .is_some_and(|lineage| lineage.split.is_some())
            {
                break;
            }
            cursor = job
                .lineage
                .as_ref()
                .and_then(|lineage| lineage.parent.strip_prefix("job:"))
                .and_then(|parent| state.jobs.get(parent));
        }
        if chain.len() > SAME_KIT_LIMIT {
            chain.reverse();
            return Err(format!(
                "{} refused: same-Kit chain limit {SAME_KIT_LIMIT} (see /run/marsh/context.md)",
                chain.join(" → ")
            ));
        }
    }
    Ok(JobLineage {
        parent: parent_ref,
        root,
        depth,
        spawn: inherited.spawn,
        deadline_unix_ms: inherited.deadline_unix_ms,
        parent_deadline: inherited.deadline_unix_ms.is_some(),
        ..JobLineage::default()
    })
}

/// Wall time is inherited: a child cannot outlive its parent's deadline
/// (`docs/design/processes.md` s6); with too little left it is not started at all.
fn wall_time_left(parent: &JobLineage, id: &str, command: &str) -> Result<(), String> {
    let Some(deadline) = parent.deadline_unix_ms else {
        return Ok(());
    };
    let left = deadline.saturating_sub(crate::unix_time_ms());
    if left < MIN_CHILD_WALL_MS {
        return Err(format!(
            "{command} refused: deadline: parent job {} has {left} ms of wall time left (MARSH_JOB_WALL_SECONDS)",
            short(id)
        ));
    }
    Ok(())
}

/// The per-tree Kit VM cap: a child whose Kit is not already live in its
/// tree is refused when the tree uses the limit's distinct Kit VMs (one warm
/// VM per Kit identity; registered aliases of one Kit share it).
fn kit_vm_cap(state: &State, root: &str, command: &str, kit: &str) -> Result<(), String> {
    let limit = state
        .job_defaults
        .as_ref()
        .map_or(TREE_KIT_VM_LIMIT, |defaults| defaults.tree_kit_vms);
    let mut tree = state
        .jobs
        .values()
        .filter(|job| {
            live(job)
                && (job.job_id == root
                    || job
                        .lineage
                        .as_ref()
                        .is_some_and(|lineage| lineage.root == root))
        })
        .collect::<Vec<_>>();
    tree.sort_by_key(|job| job.cursor);
    let mut kits: Vec<(&str, &str)> = Vec::new();
    for job in tree {
        if !kits.iter().any(|(seen, _)| *seen == job.kit_ref) {
            kits.push((job.kit_ref.as_str(), job.command.as_str()));
        }
    }
    if kits.len() < limit || kits.iter().any(|(seen, _)| *seen == kit) {
        return Ok(());
    }
    let mut names = kits.iter().map(|(_, name)| *name).collect::<Vec<_>>();
    names.sort_unstable();
    Err(format!(
        "{command} refused: tree already uses {} Kit VM{} ({}) (MARSH_TREE_KIT_VMS)",
        kits.len(),
        if kits.len() == 1 { "" } else { "s" },
        names.join(", ")
    ))
}

/// Count one refusal, and toward its tree's total when it had a parent.
pub(crate) fn refused(state: &mut State, parent: Option<&str>) {
    state.process.refused += 1;
    if let Some(parent) = parent {
        let root = root_of(state, parent);
        *state.process.refused_roots.entry(root).or_default() += 1;
    }
}

impl DaemonStore {
    /// The job's effective wall-time deadline (unix ms), journaled with its
    /// next transition: its own limit from now, or its inherited parent
    /// deadline when that is earlier. Returns the seconds left (at least 1)
    /// and whether the parent's deadline is the binding one.
    #[must_use]
    pub fn set_job_deadline(&self, job: &str, own_wall_seconds: u64) -> (u64, bool) {
        let now = crate::unix_time_ms();
        let own = now.saturating_add(own_wall_seconds.saturating_mul(1000));
        let mut state = self.lock();
        let Some(lineage) = state
            .jobs
            .get_mut(job)
            .map(|job| job.lineage.get_or_insert_with(JobLineage::default))
        else {
            return (own_wall_seconds, false);
        };
        let inherited = lineage.deadline_unix_ms.filter(|_| lineage.parent_deadline);
        let (deadline, from_parent) = match inherited {
            Some(parent) if parent < own => (parent, true),
            _ => (own, false),
        };
        lineage.deadline_unix_ms = Some(deadline);
        lineage.parent_deadline = from_parent;
        let left_ms = deadline.saturating_sub(now);
        (left_ms.div_ceil(1000).max(1), from_parent)
    }

    /// Whether `job`'s recorded deadline is within `slack_ms` of now or past.
    #[must_use]
    pub fn job_deadline_passed(&self, job: &str, slack_ms: u64) -> bool {
        let state = self.lock();
        state
            .jobs
            .get(job)
            .and_then(|job| job.lineage.as_ref())
            .and_then(|lineage| lineage.deadline_unix_ms)
            .is_some_and(|deadline| crate::unix_time_ms().saturating_add(slack_ms) >= deadline)
    }

    /// Admission before any VM effect; the authoritative check repeats in
    /// `begin_job_process`. A refusal leaves no receipt and is counted.
    ///
    /// # Errors
    /// Returns the refusal message.
    pub fn admit_process(
        &self,
        session: &str,
        command: &str,
        kit: Option<&str>,
        link: &ProcessLink,
        registered: &[String],
    ) -> Result<Vec<String>, DaemonError> {
        let mut state = self.lock();
        match admit(&state, session, command, kit, link, registered) {
            Ok((_, dropped)) => Ok(dropped),
            Err(message) => {
                refused(&mut state, link.parent_job.as_deref());
                Err(DaemonError::Refused(message))
            }
        }
    }

    /// Count a refusal that happened outside admission (TTY, view).
    pub fn count_refused(&self, parent: Option<&str>) {
        refused(&mut self.lock(), parent);
    }

    /// After a successful `marsh workers reset`: slots held by uncertain
    /// jobs of the reset Kits are released, durably.
    pub fn release_process_slots(&self, kits: &[String]) {
        let mut state = self.lock();
        let now = crate::unix_time_ms();
        for kit in kits {
            state.process.resets.insert(kit.clone(), now);
        }
        let path = state.control_home.join(RESETS);
        if let Ok(bytes) = serde_json::to_vec(&state.process.resets) {
            let temporary = path.with_extension("json.tmp");
            if std::fs::write(&temporary, bytes).is_ok() {
                let _ = std::fs::rename(&temporary, &path);
            }
        }
    }

    #[must_use]
    pub fn process_counts(&self) -> ProcessCounts {
        let state = self.lock();
        ProcessCounts {
            running: state.jobs.values().filter(|job| live(job)).count(),
            refused: state.process.refused,
            held: held(&state).count(),
        }
    }

    /// Register how to signal a running job (the backend, once launched).
    pub fn register_job_control(&self, job: &str, control: JobControl) {
        self.lock().process.controls.insert(job.to_owned(), control);
    }

    /// The job's container is gone: nothing more to signal.
    pub fn unregister_job_control(&self, job: &str) {
        self.lock().process.controls.remove(job);
    }

    /// The user typed Ctrl-C into terminal job `job` (see `interrupted`).
    pub fn note_terminal_interrupt(&self, job: &str) {
        let mut state = self.lock();
        if state.jobs.get(job).is_some_and(live) {
            state.process.interrupted.insert(job.to_owned());
        }
    }

    /// Whether `job`'s tree was cancelled; it then ends `cancelled` (130).
    #[must_use]
    pub fn cancel_requested(&self, job: &str) -> bool {
        self.lock().process.cancelling.contains(job)
    }

    /// Cancel `job` and every live descendant at once (s8: root Ctrl-C, a
    /// dropped caller, a job's end): mark them so they start nothing more
    /// and end `cancelled`, send `INT` to each container (to `job` itself
    /// only when `signal_root`; its caller already did), then `KILL` to
    /// whatever is still running after the grace. Deletion is verified by
    /// each job's own executor as for any job end.
    pub fn cancel_tree(&self, job: &str, signal_root: bool) {
        let marked = {
            let mut state = self.lock();
            let mut tree = vec![job.to_owned()];
            let mut index = 0;
            while let Some(id) = tree.get(index).cloned() {
                index += 1;
                let parent = format!("job:{id}");
                tree.extend(
                    state
                        .jobs
                        .values()
                        .filter(|child| {
                            live(child)
                                && child
                                    .lineage
                                    .as_ref()
                                    .is_some_and(|lineage| lineage.parent == parent)
                        })
                        .map(|child| child.job_id.clone()),
                );
                if tree.len() > TOTAL_LIMIT * 2 {
                    break;
                }
            }
            tree.retain(|id| state.jobs.get(id).is_some_and(live));
            state.process.cancelling.extend(tree.iter().cloned());
            tree
        };
        let signal = |ids: &[String], name: &str, skip: Option<&str>| {
            let controls = {
                let state = self.lock();
                ids.iter()
                    .filter(|id| Some(id.as_str()) != skip)
                    .filter_map(|id| state.process.controls.get(id).cloned())
                    .collect::<Vec<_>>()
            };
            for control in controls {
                (control.0)(name);
            }
        };
        signal(&marked, "INT", (!signal_root).then_some(job));
        let store = self.clone();
        thread::spawn(move || {
            thread::sleep(CANCEL_GRACE);
            let still = {
                let state = store.lock();
                marked
                    .iter()
                    .filter(|id| state.jobs.get(*id).is_some_and(live))
                    .filter_map(|id| state.process.controls.get(id).cloned())
                    .collect::<Vec<_>>()
            };
            for control in still {
                (control.0)("KILL");
            }
        });
    }

    /// The live session a capability job's children run under.
    pub(crate) fn job_session_spec(&self, job: &str) -> Option<SessionSpec> {
        self.job_session(job)
    }

    /// Whether `job` is `ancestor` or one of its descendants.
    pub(crate) fn in_subtree(&self, ancestor: &str, job: &str) -> bool {
        let state = self.lock();
        let mut cursor = Some(job.to_owned());
        let mut steps = 0;
        while let Some(id) = cursor {
            if id == ancestor {
                return true;
            }
            steps += 1;
            if steps > 64 {
                return false;
            }
            cursor = state
                .jobs
                .get(&id)
                .and_then(|job| job.lineage.as_ref())
                .and_then(|lineage| lineage.parent.strip_prefix("job:"))
                .map(str::to_owned);
        }
        false
    }

    /// Child job ids of `job_id`.
    pub(crate) fn process_children(&self, job_id: &str) -> Vec<String> {
        let parent = format!("job:{job_id}");
        let state = self.lock();
        let mut children = state
            .jobs
            .values()
            .filter(|job| {
                job.lineage
                    .as_ref()
                    .is_some_and(|lineage| lineage.parent == parent)
            })
            .map(|job| (job.cursor, job.job_id.clone()))
            .collect::<Vec<_>>();
        children.sort();
        children.into_iter().map(|(_, id)| id).collect()
    }

    /// `ProcessShow`: the forest, or one job's subtree, as nested nodes.
    #[must_use]
    pub fn process_tree(&self, root: Option<&str>) -> serde_json::Value {
        match root {
            Some(id) => {
                let state = self.lock();
                let by_parent = by_parent(&state);
                serde_json::json!({
                    "schema": "marsh.jobs.tree/v1",
                    "jobs": state.jobs.get(id).map(|job| node(job, &by_parent, 0)).into_iter().collect::<Vec<_>>(),
                })
            }
            None => self.process_forest(&[]),
        }
    }

    /// The whole forest with split nodes (`docs/design/processes.md` s9): each
    /// split a session or host caller created is a root node whose children
    /// are its branches; a branch's children are the jobs it started (its
    /// argv job, or the Kit commands its shell ran, and their descendants)
    /// and any split it created.
    #[must_use]
    pub fn process_forest(&self, splits: &[crate::split::SplitLineage]) -> serde_json::Value {
        let state = self.lock();
        let by_parent = by_parent(&state);
        let known = splits
            .iter()
            .map(|split| (split.id.as_str(), split))
            .collect::<BTreeMap<_, _>>();
        // Splits to draw: every session/host split, and any split a job
        // record names that is no longer remembered.
        let mut drawn = splits
            .iter()
            .filter(|split| split.creator_job.is_none())
            .map(|split| split.id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let mut orphan_labels: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for key in by_parent.keys() {
            if let Some((split, label)) = key
                .strip_prefix("split:")
                .and_then(|rest| rest.split_once('/'))
            {
                drawn.insert(split.to_owned());
                if !known.contains_key(split) {
                    orphan_labels
                        .entry(split.to_owned())
                        .or_default()
                        .push(label.to_owned());
                }
            }
        }
        let forest = Forest {
            by_parent: &by_parent,
            known: &known,
            drawn: &drawn,
            orphan_labels: &orphan_labels,
        };
        // Root jobs started by one `fanout` are drawn under a fanout node,
        // one branch per job, like a split.
        let mut roots = with_fanouts(
            by_parent.get("").map_or(&[][..], Vec::as_slice),
            &by_parent,
            0,
        );
        for id in &drawn {
            let nested = known
                .get(id.as_str())
                .and_then(|split| split.parent.as_ref());
            if nested.is_some_and(|parent| drawn.contains(&parent.split)) {
                continue;
            }
            let created = known.get(id.as_str()).map_or_else(
                || {
                    forest
                        .branch_jobs(id)
                        .map(|job| job.created_unix_ms)
                        .min()
                        .unwrap_or(0)
                },
                |split| split.created_unix_ms,
            );
            roots.push((created, forest.split_node(id, 0)));
        }
        roots.sort_by_key(|(created, _)| std::cmp::Reverse(*created));
        serde_json::json!({
            "schema": "marsh.jobs.tree/v1",
            "jobs": roots.into_iter().map(|(_, node)| node).collect::<Vec<_>>(),
        })
    }
}

/// Jobs by the node they hang under: `job:<id>` for a recorded parent job,
/// `split:<id>/<label>` for a split branch's job, `""` for a root.
fn by_parent(state: &State) -> BTreeMap<String, Vec<&JobReceipt>> {
    let mut by_parent: BTreeMap<String, Vec<&JobReceipt>> = BTreeMap::new();
    for job in state.jobs.values() {
        let lineage = job.lineage.as_ref();
        let parent = lineage.map_or("", |lineage| lineage.parent.as_str());
        let key = if parent
            .strip_prefix("job:")
            .is_some_and(|id| state.jobs.contains_key(id))
        {
            parent.to_owned()
        } else if let Some((split, label)) =
            lineage.and_then(|lineage| lineage.split.as_ref().zip(lineage.label.as_ref()))
        {
            format!("split:{split}/{label}")
        } else {
            String::new()
        };
        by_parent.entry(key).or_default().push(job);
    }
    for jobs in by_parent.values_mut() {
        jobs.sort_by_key(|job| job.cursor);
    }
    by_parent
}

struct Forest<'a> {
    by_parent: &'a BTreeMap<String, Vec<&'a JobReceipt>>,
    known: &'a BTreeMap<&'a str, &'a crate::split::SplitLineage>,
    drawn: &'a std::collections::BTreeSet<String>,
    orphan_labels: &'a BTreeMap<String, Vec<String>>,
}

impl Forest<'_> {
    fn branch_jobs(&self, split: &str) -> impl Iterator<Item = &JobReceipt> {
        let prefix = format!("split:{split}/");
        self.by_parent
            .iter()
            .filter(move |(key, _)| key.starts_with(&prefix))
            .flat_map(|(_, jobs)| jobs.iter().copied())
    }

    /// One split node: its branches, each with its jobs and nested splits.
    fn split_node(&self, id: &str, depth: usize) -> serde_json::Value {
        let split = self.known.get(id).copied();
        // A fanout run inside a branch is drawn as a fanout node there.
        let jobs = |label: &str| {
            with_fanouts(
                self.by_parent
                    .get(&format!("split:{id}/{label}"))
                    .map_or(&[][..], Vec::as_slice),
                self.by_parent,
                depth + 2,
            )
            .into_iter()
            .map(|(_, node)| node)
            .collect::<Vec<_>>()
        };
        let nested = |label: &str| {
            self.drawn
                .iter()
                .filter(|child| {
                    self.known
                        .get(child.as_str())
                        .and_then(|child| child.parent.as_ref())
                        .is_some_and(|parent| parent.split == id && parent.label == label)
                })
                .filter(|_| depth < 8)
                .map(|child| self.split_node(child, depth + 2))
                .collect::<Vec<_>>()
        };
        let branches = match split {
            Some(split) => split
                .branches
                .iter()
                .map(|branch| {
                    let mut children = jobs(&branch.label);
                    children.extend(nested(&branch.label));
                    serde_json::json!({
                        "node": "branch",
                        "split_id": id,
                        "label": branch.label,
                        "kind": branch.kind,
                        "command": branch.command,
                        "state": branch.state,
                        "status": branch.status,
                        "exit_code": branch.code,
                        "job_id": branch.job_id,
                        "files": branch.files,
                        "created_unix_ms": branch.started_unix_ms,
                        "finished_unix_ms": branch.finished_unix_ms,
                        "children": children,
                    })
                })
                .collect::<Vec<_>>(),
            None => self
                .orphan_labels
                .get(id)
                .into_iter()
                .flatten()
                .map(|label| {
                    serde_json::json!({
                        "node": "branch",
                        "split_id": id,
                        "label": label,
                        "children": jobs(label),
                    })
                })
                .collect(),
        };
        let first_job = || self.branch_jobs(id).next();
        serde_json::json!({
            "node": "split",
            "split_id": id,
            "session_id": split.map_or_else(
                || first_job().map(|job| job.session_id.clone()).unwrap_or_default(),
                |split| split.session.clone(),
            ),
            "state": split.map(|split| split.state.clone()),
            "status": split.and_then(|split| split.status),
            "created_unix_ms": split.map_or_else(
                || first_job().map_or(0, |job| job.created_unix_ms),
                |split| split.created_unix_ms,
            ),
            "finished_unix_ms": split.and_then(|split| split.finished_unix_ms),
            "parent": split.and_then(|split| split.parent.clone()),
            "timing_ms": split.map(|split| split.timing_ms.clone()),
            "children": branches,
        })
    }
}

/// `jobs` as tree nodes, in order, with the jobs one `fanout` started
/// (`lineage.fanout`) drawn under one fanout node at its first job's place.
/// Each node comes with its creation time.
fn with_fanouts(
    jobs: &[&JobReceipt],
    by_parent: &BTreeMap<String, Vec<&JobReceipt>>,
    depth: usize,
) -> Vec<(u64, serde_json::Value)> {
    fn fanout_of(job: &JobReceipt) -> Option<(&str, &str)> {
        job.lineage
            .as_ref()
            .and_then(|lineage| lineage.fanout.as_deref())
            .and_then(|fanout| fanout.split_once('/'))
    }
    let mut fanouts: BTreeMap<&str, Vec<(&str, &JobReceipt)>> = BTreeMap::new();
    for job in jobs {
        if let Some((id, label)) = fanout_of(job) {
            fanouts.entry(id).or_default().push((label, job));
        }
    }
    let mut nodes = Vec::new();
    for job in jobs {
        match fanout_of(job) {
            None => nodes.push((job.created_unix_ms, node(job, by_parent, depth))),
            Some((id, _)) => {
                if let Some(group) = fanouts.remove(id) {
                    let created = group
                        .iter()
                        .map(|(_, job)| job.created_unix_ms)
                        .min()
                        .unwrap_or(0);
                    nodes.push((created, fanout_node(id, &group, by_parent)));
                }
            }
        }
    }
    nodes
}

/// One fanout node: a branch per job (`label: command`), the job under it.
fn fanout_node(
    id: &str,
    jobs: &[(&str, &JobReceipt)],
    by_parent: &BTreeMap<String, Vec<&JobReceipt>>,
) -> serde_json::Value {
    let running = jobs.iter().any(|(_, job)| {
        matches!(
            job.state,
            crate::JobState::Queued | crate::JobState::Running
        )
    });
    let status = jobs
        .iter()
        .filter_map(|(_, job)| job.exit.as_ref().and_then(|exit| exit.code))
        .find(|code| *code != 0)
        .or_else(|| (!running).then_some(0));
    let finished = jobs
        .iter()
        .map(|(_, job)| job.finished_unix_ms)
        .collect::<Option<Vec<_>>>()
        .and_then(|times| times.into_iter().max());
    let branches = jobs
        .iter()
        .map(|(label, job)| {
            serde_json::json!({
                "node": "branch",
                "fanout_id": id,
                "label": label,
                "kind": "argv",
                "job_id": job.job_id,
                "state": job.state,
                "children": [node(job, by_parent, 1)],
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "node": "fanout",
        "fanout_id": id,
        "session_id": jobs.first().map(|(_, job)| job.session_id.clone()).unwrap_or_default(),
        "state": if running { "running" } else { "finished" },
        "status": status,
        "created_unix_ms": jobs.iter().map(|(_, job)| job.created_unix_ms).min(),
        "finished_unix_ms": if running { None } else { finished },
        "children": branches,
    })
}

/// One `jobs --tree` node and its descendants.
fn node(
    job: &JobReceipt,
    by_parent: &BTreeMap<String, Vec<&JobReceipt>>,
    depth: usize,
) -> serde_json::Value {
    let children = by_parent
        .get(&format!("job:{}", job.job_id))
        .cloned()
        .unwrap_or_default();
    serde_json::json!({
        "node": "job",
        "job_id": job.job_id,
        "session_id": job.session_id,
        "command": job.command,
        "args": job.args,
        "state": job.state,
        "cleanup": job.cleanup,
        "exit_code": job.exit.as_ref().and_then(|exit| exit.code),
        "created_unix_ms": job.created_unix_ms,
        "finished_unix_ms": job.finished_unix_ms,
        "lineage": job.lineage,
        "children": if depth > 64 { Vec::new() } else {
            children.iter().map(|child| node(child, by_parent, depth + 1)).collect()
        },
    })
}

/// `ProcessRun` from a job's `cap.sock`: refuse TTY children, bind the
/// parent and its session, then run the child through this daemon's own
/// `Execute` (exactly the admission, mounts, receipts, and cleanup of any
/// job) and relay frames. A dropped caller stream cancels the child.
pub(crate) fn serve_run(
    client: &Client,
    store: &DaemonStore,
    mut stream: UnixStream,
    job: &str,
    mut spec: ExecuteSpec,
) -> Result<(), DaemonError> {
    let refuse = |stream: &mut UnixStream, message: String| {
        store.count_refused(Some(job));
        write_frame(
            stream,
            &PublicReply::Error {
                code: ErrorCode::InvalidRequest,
                message,
            },
        )
    };
    let Some(session) = store.job_session_spec(job) else {
        return refuse(&mut stream, "this job's session is gone".into());
    };
    if spec.session.terminal {
        let name = spec.command.clone();
        return refuse(
            &mut stream,
            format!(
                "{TTY_REFUSAL}: {name} has a terminal on stdin or stdout; run it \
                 non-interactively with neither attached, e.g. `{name} ... </dev/null | cat`"
            ),
        );
    }
    spec.session = SessionSpec {
        terminal: false,
        terminal_size: None,
        ..session
    };
    spec.placement = crate::Placement::Local;
    // The daemon applies the split filter itself; the link's own filtering
    // is not authority (`docs/design/processes.md` s6).
    let link = spec.process.take().unwrap_or_default();
    spec.environment
        .retain(|name, _| crate::split::forwardable(name));
    marsh_contracts::process::retain_changed(&mut spec.environment, &link.start_env, env_key());
    spec.process = Some(ProcessLink {
        parent_job: Some(job.to_owned()),
        spawn: link.spawn,
        ..ProcessLink::default()
    });
    let execution = match client.start_execution(spec) {
        Ok(execution) => execution,
        Err(error) => return refuse(&mut stream, error.to_string()),
    };
    write_frame(&mut stream, &PublicReply::ExecutionAccepted)?;
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    let finished = Arc::new(AtomicBool::new(false));
    let child = Arc::new(std::sync::Mutex::new(None::<String>));
    forward_caller(
        stream.try_clone()?,
        execution.clone(),
        Arc::clone(&finished),
        store.clone(),
        Arc::clone(&child),
    );
    let mut caller_open = true;
    loop {
        match execution.receive() {
            Ok(frame) => {
                if let AttachmentFrame::JobStarted { job_id } = &frame {
                    *child
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(job_id.clone());
                }
                let last = matches!(
                    frame,
                    AttachmentFrame::Exited { .. } | AttachmentFrame::Failed { .. }
                );
                if caller_open && write_frame(&mut stream, &frame).is_err() {
                    caller_open = false;
                }
                if last {
                    break;
                }
            }
            Err(error) => {
                if caller_open {
                    let _ = write_frame(
                        &mut stream,
                        &AttachmentFrame::Failed {
                            message: format!("job uncertain: lost the child job ({error})"),
                        },
                    );
                }
                break;
            }
        }
    }
    finished.store(true, Ordering::Release);
    Ok(())
}

/// Caller frames to the child; a dropped caller (its link, or its whole
/// parent job) cancels the child: INT, then KILL after the grace.
fn forward_caller(
    mut reader: UnixStream,
    execution: crate::ClientExecution,
    finished: Arc<AtomicBool>,
    store: DaemonStore,
    child: Arc<std::sync::Mutex<Option<String>>>,
) {
    thread::spawn(move || {
        loop {
            match read_frame::<AttachmentFrame>(&mut reader) {
                Ok(
                    frame @ (AttachmentFrame::Stdin { .. }
                    | AttachmentFrame::StdinEof
                    | AttachmentFrame::Signal { .. }
                    | AttachmentFrame::Resize { .. }),
                ) => {
                    let _ = execution.send(&frame);
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        if finished.load(Ordering::Acquire) {
            return;
        }
        // The caller (a link, or its whole parent job) is gone: cancel the
        // child's whole subtree at once (INT now, KILL after the grace).
        let started = child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(job) = started {
            store.cancel_tree(&job, true);
            return;
        }
        let signal = |name: &str| {
            let _ = execution.send(&AttachmentFrame::Signal {
                signal: name.into(),
            });
        };
        signal("INT");
        thread::sleep(CANCEL_GRACE);
        if !finished.load(Ordering::Acquire) {
            signal("KILL");
        }
    });
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;
