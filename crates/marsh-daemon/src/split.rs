//! Daemon-owned splits (`docs/design/workspaces.md`): the journal, `SplitCreate`,
//! `SplitJoin`, show/cancel/remove, the session pool, the lease, the
//! deadline, and the cancel cascade. Branches run through this daemon's own
//! public endpoint (`OpenShell` for shell branches, `Execute` for argv
//! branches), so they get exactly the admission, confinement, receipts, and
//! cleanup of any shell or Kit job. Every file under `<root>/.marsh` is
//! reached by descriptor (`split_fs`). Model: `docs/model/Workspace.tla`.

use crate::split_confinement::{split_id, split_label};
use crate::split_fs::{self, Dir, GitSnapshot, GitSource};
use crate::{
    AttachmentFrame, Client, ClientExecution, DaemonBackend, DaemonError, DaemonStore, ExecuteSpec,
    PublicReply, SessionAuthority, SessionSpec, ShellSpec, read_frame, write_frame,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const MAX_BRANCHES: usize = 16;
const MAX_DEPTH: u32 = 3;
const POOL: usize = 8;
const SPOOL_LIMIT: usize = 64 << 20;
const OUTPUT_LIMIT: usize = 16 << 20;
const LEASE: Duration = Duration::from_mins(1);
const DEADLINE: Duration = Duration::from_hours(1);
const CANCEL_GRACE: Duration = Duration::from_secs(10);
/// How long a shell branch's Kit jobs get to end after the shell exited.
const JOB_SETTLE: Duration = Duration::from_secs(30);
const RENDER_DIFF_LIMIT: usize = 128 << 10;
const RENDER_REPLY_LIMIT: usize = 192 << 10;
const OUT_LIMIT: u64 = 16 << 20;
/// The pool key every host CLI caller shares (one pool per host user).
const HOST_POOL: &str = "host";
/// Splits `jobs --tree` remembers after they are removed.
const LINEAGE_KEPT: usize = 64;

/// Who may join a split: its creator only (`docs/design/workspaces.md` s2).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Creator {
    /// A host CLI caller (master token); its handle is the join capability.
    Host,
    Session(String),
    Job(String),
}

/// One requested branch: a shell string (`-b`) or a registered argv (`:::`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BranchSpec {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<Vec<u8>>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SplitCreateSpec {
    pub session: SessionSpec,
    pub cwd: PathBuf,
    pub branches: Vec<BranchSpec>,
    #[serde(default)]
    pub environment: marsh_contracts::ExportedEnvironment,
    /// From a job: starting digests of offered variables (as `ProcessLink`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub start_env: std::collections::BTreeMap<String, String>,
}

/// Sent by the joiner after its consumer ends (`SplitRelease`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SplitRelease {
    pub status: i32,
    pub keep: bool,
    /// What produced `status` (`last stage`, `join command`), for the kept
    /// message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SplitCounts {
    pub active: usize,
    pub awaiting: usize,
    pub kept: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitState {
    Run,
    Await,
    Joined,
    Kept,
    Cancelled,
    Uncertain,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchState {
    Pending,
    Running,
    Captured,
    Rejected,
    Failed,
    Cancelled,
    Uncertain,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Parent {
    pub split: String,
    pub label: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BranchRecord {
    pub label: String,
    pub kind: String,
    pub placement: String,
    pub state: BranchState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    pub files: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<String>,
    pub fork: PathBuf,
    /// `(dev, ino)` of the fork recorded at create.
    #[serde(default)]
    pub identity: (u64, u64),
    /// Wall clock (ns) right after the fork was cloned: a file with a later
    /// ctime is compared byte for byte at capture.
    #[serde(default)]
    pub forked_ns: i64,
    /// When the branch started running and when its end was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_unix_ms: Option<u64>,
    #[serde(skip)]
    spec: Option<BranchSpec>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SplitRecord {
    pub id: String,
    pub state: SplitState,
    pub creator: Creator,
    pub session: String,
    pub parent: Option<Parent>,
    pub depth: u32,
    pub root: PathBuf,
    pub dir: PathBuf,
    pub cwd: PathBuf,
    pub created_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitSource>,
    /// SHA-256 of each admin file as written (capture verification).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub admin_digest: BTreeMap<String, String>,
    pub identity: (u64, u64),
    pub branches: Vec<BranchRecord>,
    /// Phase wall times in milliseconds: `snapshot`, `forks`, `run`,
    /// `capture:<label>`, and `consumer` (join to release).
    #[serde(default)]
    pub timing_ms: BTreeMap<String, u64>,
    /// When every branch had ended and been captured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_unix_ms: Option<u64>,
}

/// What `marsh jobs --tree` keeps of a split, also after it is joined and
/// removed (`docs/design/processes.md` s9). The newest `LINEAGE_KEPT` are journaled
/// in `split-lineage.json`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SplitLineage {
    pub id: String,
    /// The creating session (the pool session for a nested split).
    pub session: String,
    /// The job that created it, for a split made inside a job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator_job: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Parent>,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<i32>,
    pub created_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_unix_ms: Option<u64>,
    #[serde(default)]
    pub timing_ms: BTreeMap<String, u64>,
    pub branches: Vec<BranchLineage>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BranchLineage {
    pub label: String,
    pub kind: String,
    /// The shell string or argv, for display (at most 200 characters).
    #[serde(default)]
    pub command: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    #[serde(default)]
    pub files: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_unix_ms: Option<u64>,
}

fn snake<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

impl SplitLineage {
    fn of(record: &SplitRecord, previous: Option<&Self>) -> Self {
        let branches = record
            .branches
            .iter()
            .map(|branch| {
                let command = branch.spec.as_ref().map_or_else(
                    || {
                        previous
                            .and_then(|previous| {
                                previous
                                    .branches
                                    .iter()
                                    .find(|old| old.label == branch.label)
                            })
                            .map(|old| old.command.clone())
                            .unwrap_or_default()
                    },
                    |spec| match (&spec.shell, &spec.argv) {
                        (Some(shell), _) => shell.chars().take(200).collect(),
                        (None, Some(argv)) => argv
                            .iter()
                            .map(|word| String::from_utf8_lossy(word).into_owned())
                            .collect::<Vec<_>>()
                            .join(" ")
                            .chars()
                            .take(200)
                            .collect(),
                        (None, None) => String::new(),
                    },
                );
                BranchLineage {
                    label: branch.label.clone(),
                    kind: branch.kind.clone(),
                    command,
                    state: snake(&branch.state),
                    status: branch.status.clone(),
                    code: branch.code,
                    job_id: branch.job_id.clone(),
                    files: branch.files,
                    started_unix_ms: branch.started_unix_ms,
                    finished_unix_ms: branch.finished_unix_ms,
                }
            })
            .collect();
        Self {
            id: record.id.clone(),
            session: record.session.clone(),
            creator_job: match &record.creator {
                Creator::Job(job) => Some(job.clone()),
                Creator::Host | Creator::Session(_) => None,
            },
            parent: record.parent.clone(),
            state: snake(&record.state),
            status: record.status,
            created_unix_ms: record.created_unix_ms,
            finished_unix_ms: record.finished_unix_ms,
            timing_ms: record.timing_ms.clone(),
            branches,
        }
    }
}

impl SplitRecord {
    fn out(&self) -> PathBuf {
        self.dir.join("out")
    }

    fn open(&self) -> Result<Dir, String> {
        split_fs::open_split(&self.root, &self.id, self.identity)
    }

    fn manifest(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).unwrap_or_default();
        value["version"] = 2.into();
        value["out"] = self.out().display().to_string().into();
        value["objects"] = self
            .dir
            .join("store.git/objects")
            .display()
            .to_string()
            .into();
        value
    }
}

#[derive(Default)]
struct Runtime {
    cancelled: AtomicBool,
    finished: AtomicBool,
    executions: Mutex<Vec<ClientExecution>>,
    held: AtomicBool,
    unheld_since: Mutex<Option<Instant>>,
}

#[derive(Default)]
struct Inner {
    records: BTreeMap<String, SplitRecord>,
    /// Session of each running shell branch -> (split, branch index).
    branch_sessions: BTreeMap<String, (String, usize)>,
    runtime: BTreeMap<String, Arc<Runtime>>,
    pool: BTreeMap<String, usize>,
    /// Slots of branches whose end was never confirmed: held until
    /// `marsh workers reset` (`BudgetBound`).
    uncertain_slots: BTreeMap<String, usize>,
    /// `jobs --tree` lineage of current and recently removed splits.
    lineage: BTreeMap<String, SplitLineage>,
}

/// The split kernel. One per daemon.
pub struct SplitEngine {
    inner: Mutex<Inner>,
    journal: PathBuf,
    client: Client,
}

fn unix_ms() -> u64 {
    crate::unix_time_ms()
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_owned()
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)
        })
}

fn error(stream: &mut UnixStream, message: impl Into<String>) -> Result<(), DaemonError> {
    write_frame(
        stream,
        &PublicReply::Error {
            code: crate::ErrorCode::InvalidRequest,
            message: message.into(),
        },
    )
}

/// Exported variables that may reach a branch: no marsh, SBX, Docker,
/// placement, or credential-shaped names (`docs/design/workspaces.md` s2).
#[must_use]
pub fn forwardable(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    !marsh_contracts::reserved_exported_environment_name(name)
        && !marsh_contracts::placement_bound_environment_name(name)
        && !matches!(
            name,
            "TMPDIR"
                | "PWD"
                | "OLDPWD"
                | "SHLVL"
                | "SHELL"
                | "_"
                | "TERM_PROGRAM"
                | "TERM_SESSION_ID"
                // A split branch is its own lineage scope: an enclosing
                // fanout branch's id must not group the branch's jobs.
                | "FANOUT_BRANCH"
        )
        && !name.starts_with("__CF")
        && !name.starts_with("XPC_")
        && !upper.starts_with("AWS_")
        && !upper.ends_with("_PAT")
        && !["KEY", "SECRET", "PASS", "TOKEN", "CREDENTIAL", "COOKIE"]
            .iter()
            .any(|word| upper.contains(word))
}

impl SplitEngine {
    /// Load the journal; every unfinished split becomes `uncertain` and its
    /// launched branches untrusted (`NoReplay`, `WorkspaceRecorded`).
    #[must_use]
    pub fn open(journal_home: &Path, client: Client) -> Arc<Self> {
        let journal = journal_home.join("splits.json");
        let mut records: BTreeMap<String, SplitRecord> = fs::read(&journal)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        for record in records.values_mut() {
            match record.state {
                SplitState::Run => {
                    record.state = SplitState::Uncertain;
                    record.reason = Some("daemon restarted mid-split; nothing was resumed".into());
                    for branch in &mut record.branches {
                        if matches!(branch.state, BranchState::Running | BranchState::Pending) {
                            branch.state = BranchState::Uncertain;
                            branch.trust = Some("untrusted".into());
                        }
                    }
                }
                SplitState::Await | SplitState::Joined => record.state = SplitState::Kept,
                _ => {}
            }
        }
        let lineage = fs::read(journal_home.join("split-lineage.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let engine = Arc::new(Self {
            inner: Mutex::new(Inner {
                records,
                lineage,
                ..Inner::default()
            }),
            journal,
            client,
        });
        engine.save();
        let lease = Arc::clone(&engine);
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(1));
                lease.expire_leases();
            }
        });
        engine
    }

    /// This daemon's own endpoint (master token): splits and `ProcessRun`
    /// start jobs through it.
    pub(crate) fn client(&self) -> &Client {
        &self.client
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Durable journal update: write, fsync, rename, fsync the directory.
    /// The `jobs --tree` lineage follows every record and outlives it.
    fn save(&self) {
        let (records, lineage) = {
            let mut inner = self.lock();
            let updated = inner
                .records
                .values()
                .map(|record| SplitLineage::of(record, inner.lineage.get(&record.id)))
                .collect::<Vec<_>>();
            for split in updated {
                inner.lineage.insert(split.id.clone(), split);
            }
            if inner.lineage.len() > LINEAGE_KEPT {
                let mut old = inner
                    .lineage
                    .values()
                    .filter(|split| !inner.records.contains_key(&split.id))
                    .map(|split| (split.created_unix_ms, split.id.clone()))
                    .collect::<Vec<_>>();
                old.sort();
                let excess = inner.lineage.len() - LINEAGE_KEPT;
                for (_, id) in old.into_iter().take(excess) {
                    inner.lineage.remove(&id);
                }
            }
            (inner.records.clone(), inner.lineage.clone())
        };
        Self::write_durable(&self.journal, &records);
        Self::write_durable(&self.journal.with_file_name("split-lineage.json"), &lineage);
    }

    fn write_durable(path: &Path, value: &impl Serialize) {
        let Ok(bytes) = serde_json::to_vec(value) else {
            return;
        };
        let temporary = path.with_extension("json.tmp");
        let written = fs::File::create(&temporary).and_then(|mut file| {
            file.write_all(&bytes)?;
            file.sync_all()
        });
        if written.is_ok()
            && fs::rename(&temporary, path).is_ok()
            && let Some(parent) = path.parent()
            && let Ok(directory) = fs::File::open(parent)
        {
            let _ = directory.sync_all();
        }
    }

    /// Current and recently removed splits for `jobs --tree`.
    #[must_use]
    pub fn lineage(&self) -> Vec<SplitLineage> {
        self.lock().lineage.values().cloned().collect()
    }

    fn update(&self, id: &str, change: impl FnOnce(&mut SplitRecord)) {
        if let Some(record) = self.lock().records.get_mut(id) {
            change(record);
        }
        self.save();
    }

    fn record(&self, id: &str) -> Option<SplitRecord> {
        self.lock().records.get(id).cloned()
    }

    fn expire_leases(&self) {
        let mut expired = Vec::new();
        {
            let inner = self.lock();
            for (id, record) in &inner.records {
                let runtime = inner.runtime.get(id);
                let unheld = runtime.and_then(|r| {
                    *r.unheld_since
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                });
                if record.state == SplitState::Await
                    && runtime.is_none_or(|r| !r.held.load(Ordering::Acquire))
                    && unheld.is_none_or(|since| since.elapsed() >= LEASE)
                {
                    expired.push(id.clone());
                }
            }
        }
        for id in expired {
            self.update(&id, |record| record.state = SplitState::Kept);
        }
    }

    /// `marsh workers reset`: uncertain branch slots are released.
    pub fn release_uncertain_slots(&self) {
        self.lock().uncertain_slots.clear();
    }

    /// `marsh status` counts.
    pub fn counts(&self) -> SplitCounts {
        let inner = self.lock();
        let mut counts = SplitCounts::default();
        for record in inner.records.values() {
            match record.state {
                SplitState::Run => counts.active += 1,
                SplitState::Await | SplitState::Joined => counts.awaiting += 1,
                SplitState::Kept | SplitState::Uncertain => counts.kept += 1,
                SplitState::Cancelled => {}
            }
        }
        counts
    }

    fn visible(record: &SplitRecord, caller: &Creator) -> bool {
        match caller {
            Creator::Host => true,
            Creator::Session(session) => &record.session == session || record.creator == *caller,
            Creator::Job(_) => record.creator == *caller,
        }
    }

    /// `SplitShow`.
    pub fn show(
        &self,
        caller: &Creator,
        id: Option<&str>,
    ) -> Result<serde_json::Value, DaemonError> {
        let inner = self.lock();
        let splits = inner
            .records
            .values()
            .filter(|record| id.is_none_or(|id| record.id == id) && Self::visible(record, caller))
            .map(SplitRecord::manifest)
            .collect::<Vec<_>>();
        if let Some(id) = id
            && splits.is_empty()
        {
            // A joined (removed) split is still described from its lineage:
            // states, statuses, and phase timings, but no files.
            let visible = |split: &SplitLineage| match caller {
                Creator::Host => true,
                Creator::Session(session) => &split.session == session,
                Creator::Job(job) => split.creator_job.as_ref() == Some(job),
            };
            return match inner.lineage.get(id).filter(|split| visible(split)) {
                Some(split) => {
                    let mut value = serde_json::to_value(split).unwrap_or_default();
                    value["removed"] = true.into();
                    Ok(serde_json::json!({ "splits": [value] }))
                }
                None => Err(DaemonError::NotFound(format!("split {id}"))),
            };
        }
        Ok(serde_json::json!({ "splits": splits }))
    }

    /// `SplitCancel`: revoke and signal the subtree.
    pub fn cancel_request(&self, caller: &Creator, id: &str) -> Result<(), DaemonError> {
        let record = self
            .record(id)
            .filter(|record| Self::visible(record, caller))
            .ok_or_else(|| DaemonError::NotFound(format!("split {id}")))?;
        self.cancel(&record.id);
        Ok(())
    }

    fn children(&self, split: &str, label: Option<&str>) -> Vec<String> {
        self.lock()
            .records
            .values()
            .filter(|record| {
                record.state == SplitState::Run
                    && record.parent.as_ref().is_some_and(|parent| {
                        parent.split == split && label.is_none_or(|label| parent.label == label)
                    })
            })
            .map(|record| record.id.clone())
            .collect()
    }

    fn cancel(&self, id: &str) {
        for child in self.children(id, None) {
            self.cancel(&child);
        }
        let Some(runtime) = self.lock().runtime.get(id).cloned() else {
            return;
        };
        if runtime.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }
        let signal = |runtime: &Runtime, name: &str| {
            for execution in runtime
                .executions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
            {
                let _ = execution.send(&AttachmentFrame::Signal {
                    signal: name.into(),
                });
            }
        };
        signal(&runtime, "INT");
        thread::spawn(move || {
            thread::sleep(CANCEL_GRACE);
            signal(&runtime, "KILL");
        });
    }

    /// The split and branch a nested creator runs in: the argv branch whose
    /// job is `job`, or the shell branch whose session is `session`.
    fn creator_branch(&self, caller: &Creator) -> Option<(SplitRecord, usize)> {
        let inner = self.lock();
        let job = match caller {
            Creator::Job(job) => job,
            Creator::Session(session) => {
                let (split, index) = inner.branch_sessions.get(session)?;
                return inner
                    .records
                    .get(split)
                    .map(|record| (record.clone(), *index));
            }
            Creator::Host => return None,
        };
        inner.records.values().find_map(|record| {
            record
                .branches
                .iter()
                .position(|branch| branch.job_id.as_deref() == Some(job))
                .map(|index| (record.clone(), index))
        })
    }

    /// `SplitRemove`: the creator's own non-running split; any for the host.
    pub fn remove(&self, caller: &Creator, id: &str) -> Result<String, DaemonError> {
        let record = self
            .record(id)
            .filter(|record| caller == &Creator::Host || &record.creator == caller)
            .ok_or_else(|| DaemonError::NotFound(format!("split {id}")))?;
        if matches!(record.state, SplitState::Run | SplitState::Joined) {
            return Err(DaemonError::InvalidState(format!(
                "split {id} is {:?}; cancel it first",
                record.state
            )));
        }
        let gone = record.state == SplitState::Cancelled
            && split_fs::splits_dir(&record.root, false)
                .and_then(|splits| splits.stat(&record.id))
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        let message = if gone {
            format!("forgot cancelled split {id} (its files were already removed)")
        } else {
            Self::remove_files(&record)
        };
        {
            let mut inner = self.lock();
            inner.records.remove(id);
            // `jobs --tree` keeps the lineage; record that it was removed.
            if let Some(lineage) = inner.lineage.get_mut(id) {
                lineage.state = "removed".into();
            }
        }
        self.save();
        Ok(message)
    }

    fn remove_files(record: &SplitRecord) -> String {
        let removed = split_fs::splits_dir(&record.root, false)
            .map_err(|_| "workspace replaced".to_owned())
            .and_then(|splits| {
                let trash = Dir::open_root(&record.root)
                    .and_then(|root| root.open(".marsh"))
                    .and_then(|marsh| marsh.ensure(".trash"))
                    .map_err(|e| e.to_string())?;
                split_fs::remove_verified_detached(&splits, &record.id, record.identity, &trash)
            });
        match removed {
            Ok(()) => format!("removed {}", record.dir.display()),
            Err(error) => format!("left {} in place: {error}", record.dir.display()),
        }
    }

    /// `SplitJoin`: creator only; holds the lease until the release frame.
    pub fn join(
        &self,
        mut stream: UnixStream,
        caller: &Creator,
        id: &str,
    ) -> Result<(), DaemonError> {
        let Some(record) = self.record(id) else {
            return error(&mut stream, format!("no split {id}"));
        };
        if &record.creator != caller {
            return error(
                &mut stream,
                format!("split {id}: only its creator can join it"),
            );
        }
        if !matches!(record.state, SplitState::Await | SplitState::Kept) {
            return error(
                &mut stream,
                format!("split {id} is {:?}; nothing to join", record.state),
            );
        }
        if record.reason.as_deref() == Some("workspace replaced") {
            return error(&mut stream, format!("split {id}: workspace replaced"));
        }
        self.update(id, |record| record.state = SplitState::Joined);
        // One reply frame carries the rendering; `out/` keeps everything.
        let mut rendering = render(&record);
        if rendering.len() > RENDER_REPLY_LIMIT {
            rendering.truncate(RENDER_REPLY_LIMIT);
            let _ = write!(
                rendering,
                "\n(rendering truncated; full results in {})\n",
                record.out().display()
            );
        }
        let reply = PublicReply::SplitJoined {
            id: id.into(),
            manifest: record.manifest().to_string(),
            rendering,
            dir: record.out(),
            objects: record
                .parent
                .is_none()
                .then(|| record.dir.join("store.git/objects")),
        };
        let joined = Instant::now();
        let release = write_frame(&mut stream, &reply)
            .and_then(|()| stream.set_read_timeout(None).map_err(Into::into))
            .and_then(|()| read_frame::<SplitRelease>(&mut stream))
            .ok();
        let consumer_ms = u64::try_from(joined.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.update(id, |record| {
            record.timing_ms.insert("consumer".into(), consumer_ms);
        });
        let why = kept_reason(&record, release.as_ref());
        if let Some(why) = why {
            self.update(id, |record| record.state = SplitState::Kept);
            let _ = write_frame(
                &mut stream,
                &PublicReply::SplitDone {
                    message: format!(
                        "kept {} ({why}); inspect, then: marsh splits rm {id}",
                        record.dir.display()
                    ),
                },
            );
        } else {
            let message = Self::remove_files(&record);
            self.lock().records.remove(id);
            self.save();
            let _ = write_frame(&mut stream, &PublicReply::SplitDone { message });
        }
        Ok(())
    }

    /// `SplitCreate`: validate, spool, snapshot, fork, run, capture, await.
    #[allow(clippy::too_many_lines, clippy::needless_pass_by_value)] // One create transaction, in model order.
    pub fn create(
        self: &Arc<Self>,
        mut stream: UnixStream,
        caller: Creator,
        spec: SplitCreateSpec,
        backend: &dyn DaemonBackend,
        store: &DaemonStore,
    ) -> Result<(), DaemonError> {
        // A host caller's session exists only for this split.
        struct Detach<'a>(Option<(&'a DaemonStore, String)>);
        impl Drop for Detach<'_> {
            fn drop(&mut self) {
                if let Some((store, session)) = self.0.take() {
                    let _ = store.detach_shell(&session);
                }
            }
        }
        let _detach =
            Detach((caller == Creator::Host).then(|| (store, spec.session.session_id.clone())));
        let plan = match self.plan(&caller, &spec, backend) {
            Ok(plan) => plan,
            Err(message) => return error(&mut stream, message),
        };
        // Spool stdin to EOF before the snapshot.
        stream.set_read_timeout(None)?;
        let mut spool = Vec::new();
        loop {
            match read_frame::<AttachmentFrame>(&mut stream) {
                Ok(AttachmentFrame::Stdin { bytes }) => {
                    spool.extend_from_slice(&bytes);
                    if spool.len() > SPOOL_LIMIT {
                        return error(&mut stream, "split: input exceeds the 64 MiB spool");
                    }
                }
                Ok(AttachmentFrame::StdinEof) => break,
                Ok(_) => return error(&mut stream, "split: unexpected input frame"),
                Err(_) => return Ok(()),
            }
        }
        let id = new_id();
        let record = match setup(&id, &caller, &spec, plan) {
            Ok(record) => record,
            Err(message) => return error(&mut stream, format!("split: {message}")),
        };
        let runtime = Arc::new(Runtime::default());
        {
            let mut inner = self.lock();
            inner.records.insert(id.clone(), record.clone());
            inner.runtime.insert(id.clone(), Arc::clone(&runtime));
        }
        self.save();
        write_frame(&mut stream, &PublicReply::SplitStarted { id: id.clone() })?;
        // A dropped stream or a signal frame before capture cancels.
        {
            let engine = Arc::clone(self);
            let id = id.clone();
            let mut reader = stream.try_clone()?;
            let runtime = Arc::clone(&runtime);
            thread::spawn(move || {
                let _ = read_frame::<AttachmentFrame>(&mut reader);
                if !runtime.finished.load(Ordering::Acquire) {
                    engine.cancel(&id);
                }
                runtime.held.store(false, Ordering::Release);
                *runtime
                    .unheld_since
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
            });
        }
        // The 1 h split deadline (`docs/design/workspaces.md` s4).
        {
            let engine = Arc::clone(self);
            let id = id.clone();
            let runtime = Arc::clone(&runtime);
            thread::spawn(move || {
                let started = Instant::now();
                while started.elapsed() < DEADLINE {
                    if runtime.finished.load(Ordering::Acquire) {
                        return;
                    }
                    thread::sleep(Duration::from_secs(5));
                }
                engine.cancel(&id);
            });
        }
        runtime.held.store(true, Ordering::Release);
        let authority = store
            .lock()
            .session_authorities
            .get(&spec.session.session_id)
            .cloned();
        let spool = Arc::new(spool);
        let output = Arc::new(AtomicUsize::new(0));
        let mut environment = spec
            .environment
            .iter()
            .filter(|(name, _)| forwardable(name))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<marsh_contracts::ExportedEnvironment>();
        marsh_contracts::process::retain_changed(
            &mut environment,
            &spec.start_env,
            crate::process::env_key(),
        );
        let mut threads = Vec::new();
        for index in 0..record.branches.len() {
            let engine = Arc::clone(self);
            let context = BranchContext {
                record: record.clone(),
                index,
                store: store.clone(),
                session: spec.session.clone(),
                authority: authority.clone(),
                environment: environment.clone(),
                spool: Arc::clone(&spool),
                runtime: Arc::clone(&runtime),
                output: Arc::clone(&output),
            };
            threads.push(thread::spawn(move || engine.branch(&context)));
        }
        let running = Instant::now();
        for thread in threads {
            let _ = thread.join();
        }
        runtime.finished.store(true, Ordering::Release);
        let run_ms = u64::try_from(running.elapsed().as_millis()).unwrap_or(u64::MAX);
        let cancelled = runtime.cancelled.load(Ordering::Acquire);
        let split = record.open();
        let replaced = split.is_err();
        self.update(&id, |record| {
            record.timing_ms.insert("run".into(), run_ms);
            record.finished_unix_ms = Some(unix_ms());
            let status = if cancelled {
                130
            } else {
                record
                    .branches
                    .iter()
                    .find_map(|branch| branch.code.filter(|code| *code != 0))
                    .unwrap_or(0)
            };
            record.status = Some(status);
            record.state = if cancelled {
                SplitState::Cancelled
            } else {
                SplitState::Await
            };
            if replaced {
                record.state = SplitState::Kept;
                record.reason = Some("workspace replaced".into());
            }
        });
        let mut record = self.record(&id).unwrap_or(record);
        if let Ok(split) = &split {
            let _ = split.open("out").and_then(|out| {
                out.write_new("manifest.json", record.manifest().to_string().as_bytes())
            });
        }
        if cancelled && !replaced {
            // A cancelled split is removed whole once every branch end is
            // confirmed; a fork whose writers may live on is never removed
            // (`NoRemoveWhileLive`).
            let unconfirmed = record
                .branches
                .iter()
                .filter(|branch| {
                    matches!(branch.state, BranchState::Uncertain | BranchState::Running)
                })
                .map(|branch| branch.label.as_str())
                .collect::<Vec<_>>();
            let reason = if unconfirmed.is_empty() {
                drop(split);
                let message = Self::remove_files(&record);
                if message.starts_with("removed") {
                    format!("cancelled; every branch ended; {message}")
                } else {
                    format!(
                        "cancelled; {message}; remove it with: marsh splits rm {}",
                        record.id
                    )
                }
            } else {
                format!(
                    "cancelled; kept {} because the end of {} {} was not confirmed; run `marsh workers reset all`, inspect, then: marsh splits rm {}",
                    record.dir.display(),
                    plural(unconfirmed.len(), "branch", "branches"),
                    unconfirmed.join(", "),
                    record.id
                )
            };
            self.update(&id, |record| record.reason = Some(reason));
            record = self.record(&id).unwrap_or(record);
        }
        *runtime
            .unheld_since
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
        let _ = write_frame(
            &mut stream,
            &PublicReply::SplitFinished {
                id,
                status: record.status.unwrap_or(125),
                cancelled,
                message: record.reason.clone(),
            },
        );
        Ok(())
    }

    /// Admission before any effect: labels, caps, depth, registered argv.
    fn plan(
        &self,
        caller: &Creator,
        spec: &SplitCreateSpec,
        backend: &dyn DaemonBackend,
    ) -> Result<Plan, String> {
        if spec.branches.is_empty() {
            return Err("split: no branches".into());
        }
        if spec.branches.len() > MAX_BRANCHES {
            return Err(format!("split: at most {MAX_BRANCHES} branches"));
        }
        // Any job may split (`docs/design/processes.md` s7): a branch job's split
        // nests in its fork; a top-level job's split snapshots its project.
        let nested = self.creator_branch(caller);
        let registered = backend.registered_commands().map_err(|e| e.to_string())?;
        // A job's split is nested too: argv branches only. A shell branch is
        // a session shell with the session's full spawn set, so a job (on
        // its capability socket) must never create one (s13 resolution 6).
        check_branches(
            &spec.branches,
            nested.is_some() || matches!(caller, Creator::Job(_)),
            &registered,
        )?;
        let project = spec.session.launch_directory.clone();
        let (parent, depth, source, pool, git) = if let Some((parent, index)) = nested {
            if parent.depth + 1 > MAX_DEPTH {
                return Err(format!(
                    "split: depth limit {MAX_DEPTH} reached (this split would be depth {})",
                    parent.depth + 1
                ));
            }
            let branch = &parent.branches[index];
            let split = parent.open()?;
            let git = parent
                .git
                .as_ref()
                .map(|git| split_fs::fork_git_source(&split, &parent.dir, &branch.label, git))
                .transpose()?;
            (
                Some(Parent {
                    split: parent.id.clone(),
                    label: branch.label.clone(),
                }),
                parent.depth + 1,
                Source::Fork {
                    root: parent.root.clone(),
                    id: parent.id.clone(),
                    identity: parent.identity,
                    label: branch.label.clone(),
                    path: branch.fork.clone(),
                },
                parent.session.clone(),
                git,
            )
        } else {
            let git = split_fs::git_source(&project)?;
            let pool = if *caller == Creator::Host {
                HOST_POOL.to_owned()
            } else {
                spec.session.session_id.clone()
            };
            (None, 1, Source::Project(project.clone()), pool, git)
        };
        let relative = spec
            .cwd
            .strip_prefix(source.path())
            .map_err(|_| format!("split: {} is outside the caller's tree", spec.cwd.display()))?;
        if !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err("split: the working directory is not a plain path".into());
        }
        if relative.starts_with(".marsh") || relative.starts_with(".git") {
            return Err("split: cannot split from inside .marsh or .git".into());
        }
        Ok(Plan {
            project,
            relative: relative.to_path_buf(),
            source,
            parent,
            depth,
            pool,
            git,
        })
    }

    #[allow(clippy::too_many_lines)] // One branch, admission to capture, in model order.
    fn branch(&self, context: &BranchContext) {
        let BranchContext {
            record,
            index,
            runtime,
            ..
        } = context;
        let index = *index;
        let branch = &record.branches[index];
        let label = branch.label.clone();
        let set = |state: BranchState, status: &str, code: i32| {
            self.update(&record.id, |r| {
                let b = &mut r.branches[index];
                b.state = state;
                b.status = Some(status.to_owned());
                b.code = Some(code);
                b.finished_unix_ms = Some(unix_ms());
                if state == BranchState::Uncertain {
                    b.trust = Some("untrusted".into());
                }
            });
        };
        let out = record.open().and_then(|split| {
            split
                .open("out")
                .and_then(|o| o.open(&label))
                .map_err(|e| e.to_string())
        });
        let write_status = |status: &str| {
            if let Ok(out) = &out {
                let _ = out.write_new("status", format!("{status}\n").as_bytes());
            }
        };
        if !self.admit(&record.session, runtime) {
            let (state, status, code) = if runtime.cancelled.load(Ordering::Acquire) {
                (BranchState::Cancelled, "cancelled", 130)
            } else {
                (BranchState::Failed, "failed: capacity", 125)
            };
            write_status(status);
            set(state, status, code);
            return;
        }
        self.update(&record.id, |r| {
            r.branches[index].state = BranchState::Running;
            r.branches[index].started_unix_ms = Some(unix_ms());
        });
        let result = self.run_branch(context);
        if result.is_err() {
            // Transport lost: the slot stays held until `workers reset`.
            *self
                .lock()
                .uncertain_slots
                .entry(record.session.clone())
                .or_default() += 1;
        }
        if let Some(used) = self.lock().pool.get_mut(&record.session) {
            *used = used.saturating_sub(1);
        }
        // The creator branch is gone: everything it created is cancelled.
        for child in self.children(&record.id, Some(&label)) {
            self.cancel(&child);
        }
        let split = match record.open() {
            Ok(split) => split,
            Err(why) => {
                set(BranchState::Rejected, &format!("rejected: {why}"), 125);
                return;
            }
        };
        let (code, stdout, stderr) = match result {
            Ok(result) => result,
            Err(message) => {
                // Never verified, never removed (`NoRemoveWhileLive`).
                let status = format!("uncertain: {message}");
                write_status(&status);
                set(BranchState::Uncertain, &status, 125);
                return;
            }
        };
        let tampered = out.as_ref().map_or(true, |out| {
            out.write_new("stdout", &stdout).is_err() || out.write_new("stderr", &stderr).is_err()
        });
        if runtime.cancelled.load(Ordering::Acquire) {
            write_status("cancelled");
            set(BranchState::Cancelled, "cancelled", 130);
            return;
        }
        let captured = Instant::now();
        let verified = if tampered {
            Err(format!("out/{label} was tampered with"))
        } else {
            Self::capture(record, branch, &split, out.as_ref().ok())
        };
        let capture_ms = u64::try_from(captured.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.update(&record.id, |r| {
            r.timing_ms.insert(format!("capture:{label}"), capture_ms);
        });
        match verified {
            Ok(files) => {
                let status = format!("exited {code}");
                write_status(&status);
                self.update(&record.id, |r| r.branches[index].files = files);
                set(BranchState::Captured, &status, code);
            }
            Err(why) => {
                let status = format!("rejected: {why}");
                write_status(&status);
                let _ = split_fs::remove_verified(&split, &label, branch.identity);
                if let Ok(admin) = split.open(".admin") {
                    let _ = admin.remove_tree(&label);
                }
                set(BranchState::Rejected, &status, 125);
            }
        }
    }

    fn admit(&self, pool: &str, runtime: &Runtime) -> bool {
        let mut inner = self.lock();
        let uncertain = inner.uncertain_slots.get(pool).copied().unwrap_or(0);
        let used = inner.pool.entry(pool.to_owned()).or_default();
        if *used + uncertain >= POOL || runtime.cancelled.load(Ordering::Acquire) {
            return false;
        }
        *used += 1;
        true
    }

    fn capture(
        record: &SplitRecord,
        branch: &BranchRecord,
        split: &Dir,
        out: Option<&Dir>,
    ) -> Result<usize, String> {
        let io = |e: std::io::Error| e.to_string();
        let fork = split
            .open(&branch.label)
            .map_err(|_| "workspace replaced".to_owned())?;
        if fork.fstat().map_err(io)?.identity != branch.identity {
            return Err("workspace replaced".into());
        }
        let admin = split
            .open(".admin")
            .and_then(|admin| admin.open(&branch.label))
            .ok();
        split_fs::verify(
            record.git.as_ref().map(|_| &record.admin_digest),
            admin.as_ref(),
            &record.dir.join(".admin").join(&branch.label),
            &fork,
        )?;
        // The ignore rules of a Git project: the fork's `info/exclude` as
        // written at create (the user's), read as data.
        let exclude = record.git.as_ref().map(|_| {
            admin
                .as_ref()
                .and_then(|admin| admin.open("info").ok())
                .and_then(|info| info.read("exclude", 1 << 20).ok())
                .unwrap_or_default()
        });
        let changes = split_fs::capture(
            &split.open("base").map_err(io)?,
            &fork,
            exclude.as_deref(),
            out.ok_or("out directory is missing")?,
            &split.open("store.git").map_err(io)?,
            i128::from(branch.forked_ns),
        )?;
        Ok(changes.len())
    }

    /// Run one branch to its confirmed end: `(status, stdout, stderr)`, or an
    /// error when that end is uncertain.
    fn run_branch(&self, context: &BranchContext) -> Result<(i32, Vec<u8>, Vec<u8>), String> {
        let BranchContext {
            record,
            index,
            store,
            session,
            authority,
            environment,
            spool,
            runtime,
            output,
        } = context;
        let branch = &record.branches[*index];
        let cwd = Some(branch.fork.join(&record.cwd))
            .filter(|path| path.is_dir())
            .unwrap_or_else(|| branch.fork.clone());
        let spec = branch.spec.clone().unwrap_or(BranchSpec {
            label: branch.label.clone(),
            shell: None,
            argv: None,
        });
        let job = {
            let (split, index) = (record.id.clone(), *index);
            move |engine: &Self, job: String| {
                engine.update(&split, |r| r.branches[index].job_id = Some(job));
            }
        };
        if let Some(shell) = &spec.shell {
            let authority = authority.clone().ok_or("creator session is gone")?;
            let session_id = store.attach_shell(std::process::id(), authority.clone());
            // A split created from this branch is its child (`Workspace.tla`).
            self.lock()
                .branch_sessions
                .insert(session_id.clone(), (record.id.clone(), *index));
            // A Kit job the branch starts is this branch's child.
            store.mark_branch_session(&session_id, &record.id, &branch.label);
            let mut arguments: Vec<Vec<u8>> = [
                "--marsh-guest",
                "--marsh-session",
                session_id.as_str(),
                "-c",
            ]
            .iter()
            .map(|s| s.as_bytes().to_vec())
            .collect();
            arguments.push(shell_script(environment).into_bytes());
            arguments.push(b"marsh".to_vec());
            arguments.push(cwd.as_os_str().as_encoded_bytes().to_vec());
            arguments.push(shell.as_bytes().to_vec());
            let spec = ShellSpec {
                arguments,
                session: session_spec(&session_id, &authority),
                dev: false,
            };
            let result = self
                .client
                .start_shell(spec)
                .map_err(|e| e.to_string())
                .and_then(|execution| self.drive(&execution, spool, true, runtime, output, &job));
            // Capture only after every Kit job the branch started has ended
            // (H1): the shell's exit tears down their clients, and the
            // backend confirms each container's deletion before the receipt
            // settles.
            let settled = Self::settle_jobs(store, &session_id);
            let _ = store.detach_shell(&session_id);
            self.lock().branch_sessions.remove(&session_id);
            let result = result?;
            settled
                .then_some(result)
                .ok_or_else(|| "a Kit job started by the branch did not end".to_owned())
        } else {
            let argv = spec.argv.clone().unwrap_or_default();
            let spec = ExecuteSpec {
                command: String::from_utf8_lossy(&argv[0]).into_owned(),
                arguments: argv[1..].to_vec(),
                placement: crate::Placement::Local,
                environment: environment.clone(),
                working_directory: Some(cwd),
                session: SessionSpec {
                    terminal: false,
                    terminal_size: None,
                    ..session.clone()
                },
                // An argv branch is a child job of the job that split.
                process: Some(crate::process::ProcessLink {
                    parent_job: match &record.creator {
                        Creator::Job(job) => Some(job.clone()),
                        Creator::Host | Creator::Session(_) => None,
                    },
                    spawn: None,
                    branch: true,
                    start_env: std::collections::BTreeMap::new(),
                }),
            };
            let execution = self
                .client
                .start_execution(spec)
                .map_err(|e| e.to_string())?;
            self.drive(&execution, spool, false, runtime, output, &job)
        }
    }

    /// Wait until no Kit job of `session` is queued or running.
    fn settle_jobs(store: &DaemonStore, session: &str) -> bool {
        let deadline = Instant::now() + JOB_SETTLE;
        while store.session_jobs_active(session) > 0 {
            if Instant::now() > deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(100));
        }
        true
    }

    fn drive(
        &self,
        execution: &ClientExecution,
        spool: &Arc<Vec<u8>>,
        shell: bool,
        runtime: &Arc<Runtime>,
        output: &Arc<AtomicUsize>,
        job: &dyn Fn(&Self, String),
    ) -> Result<(i32, Vec<u8>, Vec<u8>), String> {
        runtime
            .executions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(execution.clone());
        if runtime.cancelled.load(Ordering::Acquire) {
            let _ = execution.send(&AttachmentFrame::Signal {
                signal: "INT".into(),
            });
        }
        let start_input = || {
            let execution = execution.clone();
            let spool = Arc::clone(spool);
            thread::spawn(move || {
                for chunk in spool.chunks(crate::SHELL_STDIN_CHUNK) {
                    if execution
                        .send(&AttachmentFrame::Stdin {
                            bytes: chunk.to_vec(),
                        })
                        .is_err()
                    {
                        return;
                    }
                }
                let _ = execution.send(&AttachmentFrame::StdinEof);
            });
        };
        if !shell {
            start_input();
        }
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let account = |bytes: &[u8], into: &mut Vec<u8>| {
            if output.fetch_add(bytes.len(), Ordering::AcqRel) + bytes.len() > OUTPUT_LIMIT {
                runtime.cancelled.store(true, Ordering::Release);
                let _ = execution.send(&AttachmentFrame::Signal {
                    signal: "KILL".into(),
                });
            } else {
                into.extend_from_slice(bytes);
            }
        };
        loop {
            match execution.receive().map_err(|e| e.to_string())? {
                AttachmentFrame::Stdout { bytes } => account(&bytes, &mut stdout),
                AttachmentFrame::Stderr { bytes } => account(&bytes, &mut stderr),
                AttachmentFrame::ShellReady if shell => start_input(),
                AttachmentFrame::JobStarted { job_id } => job(self, job_id),
                AttachmentFrame::Exited { code } => return Ok((code, stdout, stderr)),
                AttachmentFrame::Failed { message } => return Err(message),
                _ => {}
            }
        }
    }
}

/// Reject bad labels, shell branches in nested splits, and unregistered or
/// `:::`-carrying argv before any effect.
fn check_branches(
    branches: &[BranchSpec],
    nested: bool,
    registered: &[String],
) -> Result<(), String> {
    let mut seen = Vec::<String>::new();
    for branch in branches {
        if !split_label(&branch.label) {
            return Err(format!(
                "split: invalid branch label {:?} (want [A-Za-z_][A-Za-z0-9_-]*, not out/base/owner)",
                branch.label
            ));
        }
        let folded = branch.label.to_ascii_lowercase();
        if seen.contains(&folded) {
            return Err(format!(
                "split: duplicate branch label {} (case-insensitive)",
                branch.label
            ));
        }
        seen.push(folded);
        match (&branch.shell, &branch.argv) {
            (Some(_), None) if nested => {
                return Err(format!(
                    "split: {}: shell branches (-b, split {{ }}) run only from the session shell; \
                     from a job or a split branch use argv branches: ::: {} CMD [ARG...]",
                    branch.label, branch.label
                ));
            }
            (Some(_), None) => {}
            (None, Some(argv)) => {
                let command = argv
                    .first()
                    .and_then(|c| std::str::from_utf8(c).ok())
                    .unwrap_or_default();
                if !registered.iter().any(|name| name == command) {
                    return Err(format!(
                        "split: {}: `{command}` is not a registered command",
                        branch.label
                    ));
                }
                if argv.iter().any(|arg| arg == b":::") {
                    return Err("split: a job argument cannot be `:::`".into());
                }
            }
            _ => return Err("split: a branch needs exactly one of shell or argv".into()),
        }
    }
    Ok(())
}

/// Why a joined split is kept rather than removed, or `None` to remove it.
fn kept_reason(record: &SplitRecord, release: Option<&SplitRelease>) -> Option<String> {
    // Never remove a fork whose writers were not confirmed gone.
    let uncertain = record
        .branches
        .iter()
        .filter(|branch| branch.state == BranchState::Uncertain)
        .map(|branch| branch.label.as_str())
        .collect::<Vec<_>>();
    if !uncertain.is_empty() {
        return Some(format!(
            "the end of {} {} was not confirmed; run `marsh workers reset all` first",
            plural(uncertain.len(), "branch", "branches"),
            uncertain.join(", ")
        ));
    }
    let Some(release) = release else {
        return Some("the join ended before releasing it".into());
    };
    if release.keep {
        return Some("join --keep".into());
    }
    if release.status != 0 {
        return Some(match release.consumer.as_deref() {
            Some(consumer) => format!("{consumer} exited {}", release.status),
            None => format!("join status {}", release.status),
        });
    }
    None
}

/// `1 branch`, `2 branches`.
#[must_use]
pub fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// The caller's tree a split snapshots.
enum Source {
    Project(PathBuf),
    Fork {
        root: PathBuf,
        id: String,
        identity: (u64, u64),
        label: String,
        path: PathBuf,
    },
}

impl Source {
    fn path(&self) -> &Path {
        match self {
            Self::Project(path) | Self::Fork { path, .. } => path,
        }
    }

    fn open(&self) -> Result<Dir, String> {
        match self {
            Self::Project(path) => Dir::open_root(path).map_err(|e| e.to_string()),
            Self::Fork {
                root,
                id,
                identity,
                label,
                ..
            } => split_fs::open_split(root, id, *identity)?
                .open(label)
                .map_err(|e| e.to_string()),
        }
    }
}

struct Plan {
    project: PathBuf,
    source: Source,
    relative: PathBuf,
    parent: Option<Parent>,
    depth: u32,
    pool: String,
    git: Option<GitSnapshot>,
}

struct BranchContext {
    record: SplitRecord,
    index: usize,
    store: DaemonStore,
    session: SessionSpec,
    authority: Option<SessionAuthority>,
    environment: marsh_contracts::ExportedEnvironment,
    spool: Arc<Vec<u8>>,
    runtime: Arc<Runtime>,
    output: Arc<AtomicUsize>,
}

/// Create `<root>/.marsh/split/<id>` and everything in it, by descriptor.
#[allow(clippy::too_many_lines)]
fn setup(
    id: &str,
    caller: &Creator,
    spec: &SplitCreateSpec,
    plan: Plan,
) -> Result<SplitRecord, String> {
    let io = |e: std::io::Error| e.to_string();
    let splits = split_fs::splits_dir(&plan.project, true)
        .map_err(|e| format!("{}/.marsh/split: {e}", plan.project.display()))?;
    let split = splits.create(id).map_err(io)?;
    let identity = split.fstat().map_err(io)?.identity;
    let dir = plan.project.join(".marsh/split").join(id);
    let mut timing_ms = BTreeMap::new();
    let mut started = Instant::now();
    let mut lap = |name: &str| {
        let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        timing_ms.insert(name.to_owned(), elapsed);
        started = Instant::now();
    };
    let mut admin_digest = BTreeMap::new();
    let result = (|| -> Result<Vec<BranchRecord>, String> {
        let source = plan.source.open()?;
        split_fs::snapshot(
            &source,
            &split.create("base").map_err(io)?,
            plan.git.as_ref(),
        )?;
        lap("snapshot");
        split
            .create("store.git")
            .and_then(|store| store.create("objects"))
            .map_err(io)?;
        let out = split.create("out").map_err(io)?;
        let admin_root = plan
            .git
            .as_ref()
            .map(|_| split.create(".admin"))
            .transpose()
            .map_err(io)?;
        let contents = plan
            .git
            .as_ref()
            .map(|git| split_fs::admin_contents(git, &dir.join("store.git")));
        if let Some(contents) = &contents {
            admin_digest = split_fs::admin_digest(contents);
        }
        let mut branches = Vec::new();
        for branch in &spec.branches {
            let label = std::ffi::OsStr::new(&branch.label);
            split
                .clone_to(std::ffi::OsStr::new("base"), &split, label)
                .map_err(io)?;
            let fork = split.open(label).map_err(io)?;
            let forked_ns = now_ns();
            let fork_identity = fork.fstat().map_err(io)?.identity;
            if let (Some(git), Some(contents), Some(admin_root)) =
                (&plan.git, &contents, &admin_root)
            {
                let admin_path = dir.join(".admin").join(label);
                let admin = admin_root.create(label).map_err(io)?;
                split_fs::write_admin(git, contents, &admin, &admin_path, &fork).map_err(io)?;
            }
            out.create(label).map_err(io)?;
            let (kind, placement) = match &branch.argv {
                Some(argv) => ("argv", format!("kit:{}", String::from_utf8_lossy(&argv[0]))),
                None => ("shell", "shell-vm".to_owned()),
            };
            branches.push(BranchRecord {
                label: branch.label.clone(),
                kind: kind.into(),
                placement,
                state: BranchState::Pending,
                status: None,
                code: None,
                job_id: None,
                files: 0,
                trust: None,
                fork: dir.join(label),
                identity: fork_identity,
                forked_ns,
                started_unix_ms: None,
                finished_unix_ms: None,
                spec: Some(branch.clone()),
            });
        }
        lap("forks");
        Ok(branches)
    })();
    let branches = match result {
        Ok(branches) => branches,
        Err(message) => {
            let _ = split_fs::remove_verified(&splits, id, identity);
            return Err(message);
        }
    };
    Ok(SplitRecord {
        id: id.into(),
        state: SplitState::Run,
        creator: caller.clone(),
        session: plan.pool,
        parent: plan.parent,
        depth: plan.depth,
        root: plan.project,
        dir,
        cwd: plan.relative,
        created_unix_ms: unix_ms(),
        status: None,
        reason: None,
        git: plan.git.map(|git| git.source),
        admin_digest,
        identity,
        branches,
        timing_ms,
        finished_unix_ms: None,
    })
}

/// The branch runs as `SHELL -c STRING` in its fork with only the caller's
/// exported environment (`docs/design/workspaces.md` s2), exported in one statement.
fn shell_script(environment: &marsh_contracts::ExportedEnvironment) -> String {
    let mut exports = String::new();
    for (name, value) in environment {
        let identifier = name.bytes().enumerate().all(|(index, byte)| {
            byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
        });
        if let (true, Ok(value)) = (identifier, std::str::from_utf8(value)) {
            let _ = write!(exports, " {name}='{}'", value.replace('\'', "'\\''"));
        }
    }
    let exports = if exports.is_empty() {
        String::new()
    } else {
        format!("export{exports}\n")
    };
    format!(
        "cd -- \"$1\" || exit 2\n{exports}__marsh_branch=$2\nset --\neval \"unset __marsh_branch; $__marsh_branch\"\n"
    )
}

fn session_spec(session_id: &str, authority: &SessionAuthority) -> SessionSpec {
    SessionSpec {
        session_id: session_id.into(),
        username: authority.username.clone(),
        uid: authority.uid,
        gid: authority.gid,
        launch_directory: authority.launch_directory.clone(),
        guest_home: authority.guest_home.clone(),
        home_backing: authority.home_backing.clone(),
        ephemeral_home: authority.ephemeral_home,
        terminal: false,
        terminal_size: None,
    }
}

/// The text rendering: a pure function of the manifest and `out/` (read by
/// descriptor).
#[must_use]
pub fn render(record: &SplitRecord) -> Vec<u8> {
    let out = record.out();
    let out_dir = record
        .open()
        .and_then(|split| split.open("out").map_err(|e| e.to_string()))
        .ok();
    let mut text = Vec::new();
    let _ = writeln!(
        text,
        "# split {}: {}; manifest {}",
        record.id,
        plural(record.branches.len(), "branch", "branches"),
        out.join("manifest.json").display()
    );
    let mut diff_budget = RENDER_DIFF_LIMIT;
    for branch in &record.branches {
        let dir = out.join(&branch.label);
        let branch_out = out_dir
            .as_ref()
            .and_then(|out| out.open(&branch.label).ok());
        let read = |name: &str| {
            branch_out
                .as_ref()
                .and_then(|out| out.read(name, OUT_LIMIT).ok())
                .unwrap_or_default()
        };
        let status = branch
            .status
            .clone()
            .unwrap_or_else(|| format!("{:?}", branch.state).to_ascii_lowercase());
        let _ = writeln!(
            text,
            "\n== {} ({status}, {}) ==",
            branch.label, branch.placement
        );
        let stdout = read("stdout");
        text.extend_from_slice(&stdout[..stdout.len().min(RENDER_DIFF_LIMIT)]);
        if !stdout.is_empty() && !stdout.ends_with(b"\n") {
            text.push(b'\n');
        }
        if branch.code != Some(0) {
            let stderr = read("stderr");
            if !stderr.is_empty() {
                let _ = writeln!(text, "-- {} stderr (last 4 KiB) --", branch.label);
                text.extend_from_slice(&stderr[stderr.len().saturating_sub(4096)..]);
                if !stderr.ends_with(b"\n") {
                    text.push(b'\n');
                }
            }
        }
        let files = String::from_utf8_lossy(&read("files")).into_owned();
        if files.is_empty() {
            let _ = writeln!(text, "-- {}: no changes --", branch.label);
            continue;
        }
        let list = files
            .lines()
            .map(|line| line.replacen('\t', " ", 1))
            .collect::<Vec<_>>();
        let _ = writeln!(
            text,
            "-- {}: {} ({}); diff {} --",
            branch.label,
            plural(list.len(), "file", "files"),
            list.join(", "),
            dir.join("diff.patch").display()
        );
        let patch = without_binary_hunks(&read("diff.patch"), &branch.label);
        let shown = patch.len().min(diff_budget);
        text.extend_from_slice(&patch[..shown]);
        diff_budget -= shown;
        if shown < patch.len() {
            let _ = writeln!(
                text,
                "(diff truncated; full diff: {})",
                dir.join("diff.patch").display()
            );
        }
    }
    text
}

/// The rendering's view of a patch: each `GIT binary patch` body becomes one
/// `binary file changed: PATH (N bytes; full patch in
/// $SPLIT_DIR/<label>/diff.patch)` line. `diff.patch` keeps the bytes.
fn without_binary_hunks(patch: &[u8], label: &str) -> Vec<u8> {
    let mut text = Vec::with_capacity(patch.len());
    let mut file: &[u8] = b"";
    let mut lines = patch.split_inclusive(|byte| *byte == b'\n').peekable();
    while let Some(line) = lines.next() {
        if let Some(header) = line.strip_prefix(b"diff --git ") {
            // `a/P b/P` (or both quoted): the second half names the file.
            let header = header.trim_ascii_end();
            let half = &header[header.len().saturating_sub(header.len() / 2)..];
            file = half
                .strip_prefix(b"b/")
                .or_else(|| {
                    half.strip_prefix(b"\"b/")
                        .and_then(|p| p.strip_suffix(b"\""))
                })
                .unwrap_or(half);
        }
        if line.trim_ascii_end() != b"GIT binary patch" {
            text.extend_from_slice(line);
            continue;
        }
        let size = lines
            .peek()
            .and_then(|next| next.trim_ascii_end().strip_prefix(b"literal "))
            .and_then(|size| std::str::from_utf8(size).ok())
            .and_then(|size| size.parse::<u64>().ok());
        while lines
            .peek()
            .is_some_and(|next| !next.starts_with(b"diff --git "))
        {
            lines.next();
        }
        text.extend_from_slice(b"binary file changed: ");
        text.extend_from_slice(file);
        let size = size.map_or_else(String::new, |size| {
            format!(
                "{}; ",
                plural(usize::try_from(size).unwrap_or(usize::MAX), "byte", "bytes")
            )
        });
        let _ = writeln!(text, " ({size}full patch in $SPLIT_DIR/{label}/diff.patch)");
    }
    text
}

/// Validate a handle's id before it names a path.
#[must_use]
pub fn valid_id(id: &str) -> bool {
    split_id(id)
}
