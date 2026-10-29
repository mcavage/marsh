//! Checkpoints (`docs/design/workspaces.md` s14): frozen snapshots of a
//! project tree, made like a split's `base/`, with the Git metadata a fork
//! needs, at `<root>/.marsh/checkpoints/<id>/{tree,git}`. The daemon journals
//! them (`checkpoints.json`), enforces retention, and reaches their files only
//! by descriptor. A checkpoint is never written again: a split `--from` it
//! clones its tree.

use crate::events::Bus;
use crate::split_fs::{self, Dir};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

const PER_LABEL: usize = 20;
const PER_PROJECT: usize = 200;
const MAX_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const MIN_FREE: u64 = 5 << 30;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Checkpoint {
    pub id: String,
    pub root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub created_unix_ms: u64,
    pub dir: PathBuf,
    /// The tree's logical bytes (APFS clones share storage).
    pub bytes: u64,
    /// Whether `git/` holds saved Git metadata.
    pub git: bool,
    pub identity: (u64, u64),
    /// `SPLIT_ID/LABEL` when this snapshots an open or kept lane's copy
    /// rather than the project (`Checkpoints::create_lane`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
}

/// A lane's copy and Git state, opened by descriptor by the split engine, for
/// `Checkpoints::create_lane`.
pub struct LaneSource {
    /// `SPLIT_ID/LABEL`.
    pub name: String,
    pub tree: Dir,
    /// The lane's `.admin/LABEL` and the split's `store.git/objects`; none
    /// for a lane of a project that is not a Git repository.
    pub git: Option<(Dir, Dir)>,
}

/// Why a checkpoint was not made.
pub enum Refusal {
    /// The volume is too full (exit 75).
    LowDisk(String),
    Invalid(String),
}

pub struct Checkpoints {
    state: Mutex<State>,
    journal: PathBuf,
    events: Arc<Bus>,
}

#[derive(Default)]
struct State {
    records: BTreeMap<String, Checkpoint>,
    /// Splits being made `--from` each checkpoint: it is not removed while
    /// one clones its tree.
    leases: BTreeMap<String, usize>,
}

/// A split's hold on the checkpoint it is cloning (`Checkpoints::lease`).
pub struct CheckpointLease<'a> {
    checkpoints: &'a Checkpoints,
    id: String,
}

impl Drop for CheckpointLease<'_> {
    fn drop(&mut self) {
        let mut state = self.checkpoints.lock();
        if let Some(count) = state.leases.get_mut(&self.id) {
            *count -= 1;
            if *count == 0 {
                state.leases.remove(&self.id);
            }
        }
    }
}

/// A label: at most 128 printable characters.
#[must_use]
pub fn valid_label(label: &str) -> bool {
    (1..=128).contains(&label.chars().count()) && !label.chars().any(char::is_control)
}

/// Whether `path` lies inside some project's `.marsh`.
#[must_use]
pub fn inside_marsh(path: &Path) -> bool {
    path.components()
        .any(|component| component == Component::Normal(".marsh".as_ref()))
}

impl Checkpoints {
    /// Load the journal, forget checkpoints whose directory is gone, and
    /// apply retention to every recorded project.
    #[must_use]
    pub fn open(journal_home: &Path, events: Arc<Bus>) -> Self {
        let journal = journal_home.join("checkpoints.json");
        let mut records: BTreeMap<String, Checkpoint> = std::fs::read(&journal)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        records.retain(|_, checkpoint| checkpoint.open().is_ok());
        let checkpoints = Self {
            state: Mutex::new(State {
                records,
                leases: BTreeMap::new(),
            }),
            journal,
            events,
        };
        for root in checkpoints.roots() {
            checkpoints.retain(&root);
        }
        checkpoints.save();
        checkpoints
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Journal the records; under the state lock, so a later state is never
    /// overwritten by an earlier one.
    fn save(&self) {
        let state = self.lock();
        if let Err(error) = crate::split::SplitEngine::write_durable(&self.journal, &state.records)
        {
            eprintln!("marshd: could not journal checkpoints: {error}");
        }
    }

    /// Every project with a recorded checkpoint.
    #[must_use]
    pub fn roots(&self) -> Vec<PathBuf> {
        self.lock()
            .records
            .values()
            .map(|checkpoint| checkpoint.root.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn publish(&self, checkpoint: &Checkpoint, removed: bool) {
        self.events.publish(
            "checkpoint",
            &serde_json::json!({
                "id": checkpoint.id,
                "root": checkpoint.root,
                "label": checkpoint.label,
                "removed": removed,
            }),
        );
    }

    /// The checks every checkpoint starts with.
    fn admit(root: &Path, label: Option<&str>) -> Result<(), Refusal> {
        let invalid = |message: String| Refusal::Invalid(format!("checkpoint: {message}"));
        if inside_marsh(root) {
            return Err(invalid("cannot checkpoint inside .marsh".into()));
        }
        if label.is_some_and(|label| !valid_label(label)) {
            return Err(invalid("a label is 1 to 128 printable characters".into()));
        }
        let (free, total) = split_fs::volume_space(root).map_err(|e| invalid(e.to_string()))?;
        if free < MIN_FREE || free.saturating_mul(20) < total {
            return Err(Refusal::LowDisk(format!(
                "checkpoint: only {} MiB free on the volume of {} (a checkpoint needs 5 GiB and 5% free)",
                free >> 20,
                root.display()
            )));
        }
        Ok(())
    }

    /// `marsh checkpoint`: snapshot `root` now.
    ///
    /// # Errors
    /// Refuses a root inside `.marsh`, a gitfile root, or a nearly full volume.
    pub fn create(&self, root: &Path, label: Option<String>) -> Result<Checkpoint, Refusal> {
        Self::admit(root, label.as_deref())?;
        let git =
            split_fs::git_source(root).map_err(|m| Refusal::Invalid(format!("checkpoint: {m}")))?;
        self.make(
            root,
            label,
            None,
            git.as_ref(),
            &|| Dir::open_root(root).map_err(|e| e.to_string()),
            &|| Ok(()),
        )
    }

    /// `marsh checkpoint --lane`: snapshot a lane's copy as `create` snapshots
    /// the project, with the lane's Git state (resolved `HEAD`, index) and the
    /// objects its jobs wrote, so a split `--from` it starts from the lane's
    /// state. `settled` runs after the copy is made and before the checkpoint
    /// is recorded: it refuses a lane that was closed, cancelled, or removed
    /// meanwhile, and the copy is discarded.
    ///
    /// # Errors
    /// As `create`, and when `settled` refuses.
    pub fn create_lane(
        &self,
        root: &Path,
        label: Option<String>,
        lane: &LaneSource,
        settled: &dyn Fn() -> Result<(), String>,
    ) -> Result<Checkpoint, Refusal> {
        Self::admit(root, label.as_deref())?;
        // The admin directory is writable by the lane's jobs: it is read as
        // data (HEAD resolves only to an object id) and never followed.
        let git = lane
            .git
            .as_ref()
            .map(|(admin, _)| split_fs::lane_git(admin, root))
            .transpose()
            .map_err(|m| Refusal::Invalid(format!("checkpoint: {m}")))?;
        self.make(
            root,
            label,
            Some(lane),
            git.as_ref(),
            &|| lane.tree.dup().map_err(|e| e.to_string()),
            settled,
        )
    }

    fn make(
        &self,
        root: &Path,
        label: Option<String>,
        lane: Option<&LaneSource>,
        git: Option<&split_fs::GitSnapshot>,
        source: &dyn Fn() -> Result<Dir, String>,
        settled: &dyn Fn() -> Result<(), String>,
    ) -> Result<Checkpoint, Refusal> {
        let invalid = |message: String| Refusal::Invalid(format!("checkpoint: {message}"));
        let io = |e: std::io::Error| invalid(e.to_string());
        let area = split_fs::area(root, "checkpoints", true).map_err(io)?;
        let id = crate::split::new_id();
        let dir = area.create(&id).map_err(io)?;
        let identity = dir.fstat().map_err(io)?.identity;
        let made = (|| -> Result<u64, String> {
            let tree = dir.create("tree").map_err(|e| e.to_string())?;
            split_fs::snapshot(&source()?, &tree, git)?;
            if let Some(git) = git {
                let saved = dir.create("git").map_err(|e| e.to_string())?;
                split_fs::save_git(git, &saved).map_err(|e| e.to_string())?;
                if let Some((admin, store)) = lane.and_then(|lane| lane.git.as_ref()) {
                    split_fs::save_lane_objects(store, admin, &saved).map_err(|e| e.to_string())?;
                }
            }
            settled()?;
            Ok(split_fs::logical_bytes(&tree))
        })();
        let bytes = match made {
            Ok(bytes) => bytes,
            Err(message) => {
                let _ = split_fs::remove_verified(&area, &id, identity);
                return Err(invalid(message));
            }
        };
        let checkpoint = Checkpoint {
            dir: root.join(".marsh/checkpoints").join(&id),
            id: id.clone(),
            root: root.to_path_buf(),
            label,
            created_unix_ms: crate::unix_time_ms(),
            bytes,
            git: git.is_some(),
            identity,
            lane: lane.map(|lane| lane.name.clone()),
        };
        self.lock().records.insert(id, checkpoint.clone());
        self.publish(&checkpoint, false);
        self.retain(root);
        self.save();
        Ok(checkpoint)
    }

    /// Checkpoints of `root` (every project when `None`), newest first.
    #[must_use]
    pub fn list(&self, root: Option<&Path>, label: Option<&str>) -> Vec<Checkpoint> {
        let mut found = self
            .lock()
            .records
            .values()
            .filter(|checkpoint| root.is_none_or(|root| checkpoint.root == root))
            .filter(|checkpoint| {
                label.is_none_or(|label| checkpoint.label.as_deref() == Some(label))
            })
            .cloned()
            .collect::<Vec<_>>();
        found.sort_by_key(|checkpoint| {
            std::cmp::Reverse((checkpoint.created_unix_ms, checkpoint.id.clone()))
        });
        found
    }

    /// Find a checkpoint of `root` and hold it until the lease is dropped:
    /// retention and `rm` leave it alone meanwhile.
    #[must_use]
    pub fn lease(&self, root: &Path, id: &str) -> Option<(Checkpoint, CheckpointLease<'_>)> {
        let mut state = self.lock();
        let checkpoint = state
            .records
            .get(id)
            .filter(|checkpoint| checkpoint.root == root)
            .cloned()?;
        *state.leases.entry(id.to_owned()).or_default() += 1;
        Some((
            checkpoint,
            CheckpointLease {
                checkpoints: self,
                id: id.to_owned(),
            },
        ))
    }

    /// `marsh checkpoints rm ID`.
    ///
    /// # Errors
    /// Returns why it was not found or not removed.
    pub fn remove(&self, root: Option<&Path>, id: &str) -> Result<String, String> {
        let checkpoint = {
            let mut state = self.lock();
            let checkpoint = state
                .records
                .get(id)
                .filter(|checkpoint| root.is_none_or(|root| checkpoint.root == root))
                .cloned()
                .ok_or_else(|| format!("no checkpoint {id}"))?;
            if state.leases.contains_key(id) {
                return Err(format!(
                    "checkpoint {id} is in use by a split being made from it; retry"
                ));
            }
            state.records.remove(id);
            checkpoint
        };
        let message = self.forget(&checkpoint);
        self.save();
        message
    }

    /// Move the files of a checkpoint whose record is already gone to
    /// `.marsh/.trash` (deleted in the background); a replaced directory is
    /// forgotten, never removed by path.
    fn forget(&self, checkpoint: &Checkpoint) -> Result<String, String> {
        let removed = split_fs::area(&checkpoint.root, "checkpoints", false)
            .and_then(|area| {
                let trash = split_fs::area(&checkpoint.root, ".trash", true)?;
                Ok((area, trash))
            })
            .map_err(|e| e.to_string())
            .and_then(|(area, trash)| {
                split_fs::remove_verified_detached(
                    &area,
                    &checkpoint.id,
                    checkpoint.identity,
                    &trash,
                )
            });
        self.publish(checkpoint, true);
        removed.map(|()| format!("removed checkpoint {}", checkpoint.id))
    }

    /// Keep the newest 20 per label and 200 per project, none older than 7
    /// days. A leased checkpoint waits for the next pass.
    fn retain(&self, root: &Path) {
        let now = crate::unix_time_ms();
        let mut per_label = BTreeMap::<Option<String>, usize>::new();
        let expired = self
            .list(Some(root), None)
            .into_iter()
            .enumerate()
            .filter(|(rank, checkpoint)| {
                let in_label = per_label.entry(checkpoint.label.clone()).or_default();
                *in_label += 1;
                *rank >= PER_PROJECT
                    || *in_label > PER_LABEL
                    || now.saturating_sub(checkpoint.created_unix_ms) > MAX_AGE_MS
            })
            .map(|(_, checkpoint)| checkpoint)
            .collect::<Vec<_>>();
        let expired = {
            let mut state = self.lock();
            let State { records, leases } = &mut *state;
            expired
                .into_iter()
                .filter(|checkpoint| !leases.contains_key(&checkpoint.id))
                .filter(|checkpoint| records.remove(&checkpoint.id).is_some())
                .collect::<Vec<_>>()
        };
        for checkpoint in expired {
            let _ = self.forget(&checkpoint);
        }
    }
}

impl Checkpoint {
    /// The checkpoint directory, if it is still the one recorded at create.
    ///
    /// # Errors
    /// Returns `checkpoint replaced` or the open failure.
    pub fn open(&self) -> Result<Dir, String> {
        let dir = split_fs::area(&self.root, "checkpoints", false)
            .and_then(|area| area.open(&self.id))
            .map_err(|e| format!("checkpoint {}: {e}", self.id))?;
        if dir.fstat().map_err(|e| e.to_string())?.identity != self.identity {
            return Err(format!("checkpoint {} was replaced", self.id));
        }
        Ok(dir)
    }

    /// The `--json` and list shape.
    #[must_use]
    pub fn document(&self) -> serde_json::Value {
        serde_json::json!({
            "checkpoint": self.id,
            "label": self.label,
            "created_unix_ms": self.created_unix_ms,
            "root": self.root,
            "dir": self.dir,
            "bytes": self.bytes,
            "lane": self.lane,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A split cloning a checkpoint holds it: neither retention nor `rm`
    /// removes it until the lease is dropped.
    #[test]
    fn a_leased_checkpoint_outlives_retention_until_released() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let project = root.join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("a"), "a").unwrap();
        let checkpoints = Checkpoints::open(&root, Arc::new(Bus::default()));
        let label = || Some("turn".to_owned());
        let Ok(first) = checkpoints.create(&project, label()) else {
            panic!("checkpoint refused");
        };
        let (_, lease) = checkpoints.lease(&project, &first.id).unwrap();
        for _ in 0..PER_LABEL {
            assert!(checkpoints.create(&project, label()).is_ok());
        }
        assert!(checkpoints.lease(&project, &first.id).is_some());
        assert!(first.dir.join("tree/a").is_file());
        assert!(checkpoints.remove(Some(&project), &first.id).is_err());
        drop(lease);
        assert!(checkpoints.create(&project, label()).is_ok());
        assert!(checkpoints.lease(&project, &first.id).is_none());
        assert_eq!(
            checkpoints.list(Some(&project), Some("turn")).len(),
            PER_LABEL
        );
    }
}
