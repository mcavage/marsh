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
}

/// Why a checkpoint was not made.
pub enum Refusal {
    /// The volume is too full (exit 75).
    LowDisk(String),
    Invalid(String),
}

pub struct Checkpoints {
    records: Mutex<BTreeMap<String, Checkpoint>>,
    journal: PathBuf,
    events: Arc<Bus>,
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
            records: Mutex::new(records),
            journal,
            events,
        };
        for root in checkpoints.roots() {
            checkpoints.retain(&root);
        }
        checkpoints.save();
        checkpoints
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Checkpoint>> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn save(&self) {
        crate::split::SplitEngine::write_durable(&self.journal, &*self.lock());
    }

    /// Every project with a recorded checkpoint.
    #[must_use]
    pub fn roots(&self) -> Vec<PathBuf> {
        self.lock()
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

    /// `marsh checkpoint`: snapshot `root` now.
    ///
    /// # Errors
    /// Refuses a root inside `.marsh`, a gitfile root, or a nearly full volume.
    pub fn create(&self, root: &Path, label: Option<String>) -> Result<Checkpoint, Refusal> {
        let invalid = |message: String| Refusal::Invalid(format!("checkpoint: {message}"));
        if inside_marsh(root) {
            return Err(invalid("cannot checkpoint inside .marsh".into()));
        }
        if label.as_deref().is_some_and(|label| !valid_label(label)) {
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
        let git = split_fs::git_source(root).map_err(invalid)?;
        let io = |e: std::io::Error| invalid(e.to_string());
        let area = split_fs::area(root, "checkpoints", true).map_err(io)?;
        let id = crate::split::new_id();
        let dir = area.create(&id).map_err(io)?;
        let identity = dir.fstat().map_err(io)?.identity;
        let made = (|| -> Result<u64, String> {
            let tree = dir.create("tree").map_err(|e| e.to_string())?;
            let source = Dir::open_root(root).map_err(|e| e.to_string())?;
            split_fs::snapshot(&source, &tree, git.as_ref())?;
            if let Some(git) = &git {
                split_fs::save_git(git, &dir.create("git").map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            }
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
        };
        self.lock().insert(id, checkpoint.clone());
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

    #[must_use]
    pub fn get(&self, root: &Path, id: &str) -> Option<Checkpoint> {
        self.lock()
            .get(id)
            .filter(|checkpoint| checkpoint.root == root)
            .cloned()
    }

    /// `marsh checkpoints rm ID`.
    ///
    /// # Errors
    /// Returns why it was not found or not removed.
    pub fn remove(&self, root: Option<&Path>, id: &str) -> Result<String, String> {
        let checkpoint = self
            .lock()
            .get(id)
            .filter(|checkpoint| root.is_none_or(|root| checkpoint.root == root))
            .cloned()
            .ok_or_else(|| format!("no checkpoint {id}"))?;
        let message = self.forget(&checkpoint);
        self.save();
        message
    }

    /// Move the files to `.marsh/.trash` (deleted in the background) and drop
    /// the record; a replaced directory is forgotten, never removed by path.
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
        self.lock().remove(&checkpoint.id);
        self.publish(checkpoint, true);
        removed.map(|()| format!("removed checkpoint {}", checkpoint.id))
    }

    /// Keep the newest 20 per label and 200 per project, none older than 7 days.
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
        })
    }
}
