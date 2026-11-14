//! Checkpoints (`docs/design/workspaces.md` s14): frozen snapshots of a
//! project tree, made like a split's `base/`, with the Git metadata a fork
//! needs, at `<root>/.marsh/checkpoints/<id>/{tree,git}`. The daemon journals
//! them (`checkpoints.json`), enforces retention, and reaches their files only
//! by descriptor. A checkpoint is never written again: a split `--from` it
//! clones its tree.
//!
//! With services on for the project, a checkpoint also holds a snapshot of
//! the workspace's services objects (`Checkpoint::services`, C12). The
//! snapshot lives in the project's services VM; the record only names it. A
//! snapshot is dropped with the last checkpoint that references it (`services.snapshot`
//! or `services.reused_from`), and an unreferenced one is collected
//! (`Checkpoints::collect_orphans`). Nothing here starts or creates a VM.

use crate::events::Bus;
use crate::split_fs::{self, Dir};
use crate::{DaemonBackend, hex};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};

const PER_LABEL: usize = 20;
const PER_PROJECT: usize = 200;
const MAX_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const MIN_FREE: u64 = 5 << 30;
/// The workspace of the project's own tree (lanes have their own).
pub const BASE_WORKSPACE: &str = "base";
/// An orphan younger than this is left to the capture that may still be
/// recording it.
const ORPHAN_GRACE_MS: u64 = 15_000;
const MAX_VOLUMES: usize = 256;
const MAX_STACKS: usize = 64;
const MAX_CONTAINERS: usize = 128;
const MAX_GAPS: usize = 64;
const MAX_TEXT: usize = 160;
/// Gap recorded when the services pool is below its reserve (DECISIONS 5).
pub const NO_SPACE_GAP: &str = "not enough space for app data";

/// Serializes capture admission under `capture-admission-blocks` (test only).
static CAPTURE_SERIAL: Mutex<()> = Mutex::new(());

/// Test-only defects (`docs/design/test-breaks.md`); each is read through
/// `marsh_contracts::break_mode`, which is `false` in every release build.
pub mod breaks {
    pub const RETENTION_KEEP_SNAPSHOTS: &str = "retention-keep-snapshots";
    pub const ORPHAN_GC_OFF: &str = "orphan-gc-off";
    pub const REUSE_LEASE_OFF: &str = "reuse-lease-off";
    pub const CAPTURE_ADMISSION_BLOCKS: &str = "capture-admission-blocks";
    pub const ORPHAN_ROOTS_FROM_RECORDS: &str = "orphan-roots-from-records";
    pub const LANE_CAPTURE_ROOT: &str = "lane-capture-root";
    /// A container's reference to a volume the manifest does not list is accepted.
    pub const MANIFEST_REFS_UNCHECKED: &str = "manifest-refs-unchecked";
}

/// The workspace a lane's services objects live in: `SPLIT_ID/LABEL` is
/// `fork-SPLIT_ID-LABEL`. `None` when that would not be a valid workspace id
/// (at most 64 of `[A-Za-z0-9_.-]`, not starting with a dot).
#[must_use]
pub fn lane_workspace(lane: &str) -> Option<String> {
    let (split, label) = lane.split_once('/')?;
    let workspace = format!("fork-{split}-{label}");
    (!split.is_empty()
        && !label.is_empty()
        && workspace.len() <= 64
        && !workspace.starts_with('.')
        && workspace
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)))
    .then_some(workspace)
}

/// A fresh opaque snapshot id (`[0-9a-f]{12}`).
#[must_use]
pub fn new_snapshot_id() -> String {
    crate::split::new_id()
}

/// Opaque ids from the VM: 1 to 64 of `[A-Za-z0-9_-]`, not starting with `-`.
#[must_use]
pub fn valid_opaque_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Display text from the VM: control characters become spaces and the length
/// is bounded (A8). Never an identifier and never a path.
pub(crate) fn plain(text: &str) -> String {
    let mut cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if cleaned.len() > MAX_TEXT {
        let mut end = MAX_TEXT;
        while !cleaned.is_char_boundary(end) {
            end -= 1;
        }
        cleaned.truncate(end);
    }
    cleaned
}

/// One volume of a services snapshot, as the snapshot's manifest lists it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct VolumeRecord {
    /// Opaque id inside the snapshot.
    pub id: String,
    /// Display name.
    pub name: String,
    pub captured: bool,
    pub files: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gaps: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContainerRecord {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    pub image: String,
    pub running: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct StackRecord {
    /// `compose` or `supabase` (display).
    pub kind: String,
    pub project: String,
    pub containers: Vec<ContainerRecord>,
}

/// What a checkpoint holds of its workspace's services objects
/// (`checkpoints --json`: `services`).
///
/// - A snapshot of its own: `snapshot` is set.
/// - The VM was stopped: `reused_from` names the earlier snapshot, which is
///   exactly what the services held, since nothing can write while the VM is
///   stopped (DECISIONS 9).
/// - A gap: neither is set and `gaps` says why; the files still checkpointed.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServicesRecord {
    #[serde(default)]
    pub snapshot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reused_from: Option<String>,
    #[serde(default)]
    pub volumes: Vec<VolumeRecord>,
    #[serde(default)]
    pub stacks: Vec<StackRecord>,
    /// Names of the containers outside any stack (display).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub standalone: Vec<String>,
    #[serde(default)]
    pub gaps: Vec<String>,
    #[serde(default)]
    pub frozen_ms: Option<u64>,
}

impl ServicesRecord {
    /// A checkpoint with no services data and the reason.
    #[must_use]
    pub fn gap(gap: impl Into<String>) -> Self {
        Self {
            gaps: vec![plain(&gap.into())],
            ..Self::default()
        }
    }

    /// The snapshot restore reads: its own, or the one it reuses.
    #[must_use]
    pub fn restore_source(&self) -> Option<&str> {
        self.snapshot.as_deref().or(self.reused_from.as_deref())
    }

    fn references(&self) -> impl Iterator<Item = &str> {
        self.snapshot
            .iter()
            .chain(self.reused_from.iter())
            .map(String::as_str)
    }

    fn event_word(&self) -> &'static str {
        if self.snapshot.is_some() {
            "captured"
        } else if self.reused_from.is_some() {
            "reused"
        } else {
            "gap"
        }
    }

    /// The record of `snapshot` (taken of `workspace`) from the manifest the
    /// services agent published, built on the Mac (A8). Required fields are
    /// validated and a manifest that does not validate is rejected whole (the
    /// caller records a capture gap): a missing manifest, another id or
    /// workspace, a list that is not a list, a volume or container without its
    /// identifying fields. Every identifier is validated, every text is plain
    /// and bounded, and every list is capped; **whatever a cap omits is said
    /// in `gaps`, and those gaps are kept ahead of the ordinary ones**, so a
    /// record never looks complete when it is not.
    ///
    /// # Errors
    /// Why the manifest was rejected.
    #[allow(clippy::too_many_lines)] // One validation walk over the manifest.
    pub fn from_manifest(
        snapshot: &str,
        workspace: &str,
        manifest: Option<&serde_json::Value>,
        reported_gaps: &[String],
    ) -> Result<Self, String> {
        use serde_json::Value;
        let manifest = manifest
            .filter(|m| m.is_object())
            .ok_or("the services agent published a snapshot but sent no manifest")?;
        if manifest.get("id").and_then(Value::as_str) != Some(snapshot) {
            return Err("the manifest is of another snapshot".into());
        }
        if manifest.get("workspace").and_then(Value::as_str) != Some(workspace) {
            return Err("the manifest is of another workspace".into());
        }
        let array = |name: &str| -> Result<&Vec<Value>, String> {
            manifest
                .get(name)
                .and_then(Value::as_array)
                .ok_or_else(|| format!("the manifest has no `{name}` list"))
        };
        let complete = manifest
            .get("complete")
            .and_then(Value::as_bool)
            .ok_or("the manifest does not say whether it is complete")?;
        let text = |value: Option<&Value>, what: &str| -> Result<String, String> {
            value
                .and_then(Value::as_str)
                .map(plain)
                .ok_or_else(|| format!("{what} is not text"))
        };
        let gaps: std::cell::RefCell<Vec<String>> = std::cell::RefCell::default();
        let note = |gap: String| {
            let mut gaps = gaps.borrow_mut();
            if !gaps.contains(&gap) {
                gaps.push(gap);
            }
        };
        for gap in manifest
            .get("gaps")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            note(text(Some(gap), "a manifest gap")?);
        }
        for gap in reported_gaps {
            note(plain(gap));
        }
        // What the caps cut, counted (never one line per item: the summary is
        // bounded however many items were cut).
        let cut: std::cell::RefCell<Cuts> = std::cell::RefCell::default();

        let all_volumes = array("volumes")?;
        // Every volume the manifest lists, cut or not: a container's reference
        // must name one of them.
        let mut known: BTreeSet<&str> = BTreeSet::new();
        for volume in all_volumes {
            if let Some(id) = volume.get("id").and_then(Value::as_str)
                && !known.insert(id)
            {
                return Err("two volumes share an opaque id".into());
            }
        }
        cut.borrow_mut().volumes = all_volumes.len().saturating_sub(MAX_VOLUMES);
        let mut volumes = Vec::new();
        for volume in all_volumes.iter().take(MAX_VOLUMES) {
            let id = volume.get("id").and_then(Value::as_str).unwrap_or("");
            if !valid_opaque_id(id) {
                return Err("a volume has no valid opaque id".into());
            }
            let captured = volume
                .get("captured")
                .and_then(Value::as_bool)
                .ok_or("a volume does not say whether it was captured")?;
            let name = text(volume.get("name"), "a volume name")?;
            let mut volume_gaps: Vec<String> = Vec::new();
            let mut cut_here = false;
            for gap in volume
                .get("gaps")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if volume_gaps.len() == MAX_GAPS {
                    cut_here = true;
                    break;
                }
                volume_gaps.push(text(Some(gap), "a volume gap")?);
            }
            if cut_here {
                cut.borrow_mut().volume_gap_lists += 1;
            }
            if !captured && volume_gaps.is_empty() {
                note(format!("volume '{name}' was not saved"));
            }
            volumes.push(VolumeRecord {
                id: id.to_owned(),
                name,
                captured,
                files: volume.get("files").and_then(Value::as_u64).unwrap_or(0),
                gaps: volume_gaps,
            });
        }
        let listed: BTreeSet<&str> = volumes.iter().map(|volume| volume.id.as_str()).collect();
        let unchecked = marsh_contracts::break_mode(breaks::MANIFEST_REFS_UNCHECKED);

        let container = |value: &Value| -> Result<ContainerRecord, String> {
            let name = text(value.get("name"), "a container name")?;
            if name.is_empty() {
                return Err("a container has no name".into());
            }
            let mut mounted = Vec::new();
            let mut cut_here = false;
            for id in value
                .get("volumes")
                .and_then(Value::as_array)
                .ok_or("a container has no volume list")?
            {
                let id = id.as_str().filter(|id| valid_opaque_id(id));
                let id = id.ok_or("a container names a volume that is not an opaque id")?;
                // A reference to a volume the manifest does not list would
                // restore to nothing: the manifest is refused.
                if !unchecked && !known.contains(id) {
                    return Err(format!(
                        "container '{name}' names a volume the manifest does not list"
                    ));
                }
                if mounted.len() == MAX_VOLUMES {
                    cut_here = true;
                    break;
                }
                // Listed in the manifest but cut from this record by the volume
                // cap: the reference is dropped, and the cut said.
                if !unchecked && !listed.contains(id) {
                    cut.borrow_mut().dropped_references += 1;
                    continue;
                }
                mounted.push(id.to_owned());
            }
            if cut_here {
                cut.borrow_mut().mount_lists += 1;
            }
            Ok(ContainerRecord {
                service: match value.get("service") {
                    None | Some(Value::Null) => None,
                    other => Some(text(other, "a service name")?),
                },
                image: text(value.get("image"), "an image name")?,
                running: value
                    .get("running")
                    .and_then(Value::as_bool)
                    .ok_or("a container does not say whether it was running")?,
                volumes: mounted,
                name,
            })
        };
        let all_stacks = array("stacks")?;
        cut.borrow_mut().stacks = all_stacks.len().saturating_sub(MAX_STACKS);
        let mut stacks = Vec::new();
        for stack in all_stacks.iter().take(MAX_STACKS) {
            let kind = text(stack.get("kind"), "a stack kind")?;
            let project = text(stack.get("project"), "a stack project")?;
            let members = stack
                .get("containers")
                .and_then(Value::as_array)
                .ok_or("a stack has no container list")?;
            cut.borrow_mut().stack_members += members.len().saturating_sub(MAX_CONTAINERS);
            let containers = members
                .iter()
                .take(MAX_CONTAINERS)
                .map(container)
                .collect::<Result<Vec<_>, _>>()?;
            stacks.push(StackRecord {
                kind,
                project,
                containers,
            });
        }
        let loose = array("standalone")?;
        cut.borrow_mut().standalone = loose.len().saturating_sub(MAX_CONTAINERS);
        let standalone = loose
            .iter()
            .take(MAX_CONTAINERS)
            .map(|value| container(value).map(|c| c.name))
            .collect::<Result<Vec<_>, _>>()?;
        let omitted = cut.into_inner().summary();
        if !complete && gaps.borrow().is_empty() && omitted.is_empty() {
            note("the snapshot is incomplete".into());
        }
        let mut gaps = gaps.into_inner();
        // The summary of what was cut comes first and is always kept (a bounded
        // handful of lines); ordinary gaps fill what is left of `MAX_GAPS`,
        // and when some are cut, that is said in the last line.
        let mut all = omitted;
        if all.len() + gaps.len() <= MAX_GAPS {
            all.extend(gaps);
        } else {
            let room = MAX_GAPS.saturating_sub(all.len() + 1);
            let dropped = gaps.len() - room;
            gaps.truncate(room);
            all.extend(gaps);
            all.push(format!("{dropped} more gaps are not listed here"));
        }
        Ok(Self {
            snapshot: Some(snapshot.to_owned()),
            reused_from: None,
            volumes,
            stacks,
            standalone,
            gaps: all,
            frozen_ms: manifest.get("frozen_ms").and_then(Value::as_u64),
        })
    }
}

/// What the record's caps cut from a manifest, counted.
#[derive(Default)]
struct Cuts {
    volumes: usize,
    stacks: usize,
    stack_members: usize,
    standalone: usize,
    /// Volumes with more gaps than are listed.
    volume_gap_lists: usize,
    /// Containers mounting more volumes than are listed.
    mount_lists: usize,
    /// References to a volume the volume cap cut.
    dropped_references: usize,
}

impl Cuts {
    /// One line per kind of cut, at most seven, whatever was cut.
    fn summary(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let mut say = |count: usize, text: String| {
            if count > 0 {
                lines.push(text);
            }
        };
        say(
            self.volumes,
            format!(
                "{} volumes were saved but only the first {MAX_VOLUMES} are listed here",
                self.volumes + MAX_VOLUMES
            ),
        );
        say(
            self.stacks,
            format!(
                "{} stacks were saved but only the first {MAX_STACKS} are listed here",
                self.stacks + MAX_STACKS
            ),
        );
        say(
            self.stack_members,
            format!(
                "{} containers of stacks are not listed here (a stack lists {MAX_CONTAINERS} at most)",
                self.stack_members
            ),
        );
        say(
            self.standalone,
            format!(
                "{} standalone containers were saved but only the first {MAX_CONTAINERS} are listed here",
                self.standalone + MAX_CONTAINERS
            ),
        );
        say(
            self.volume_gap_lists,
            format!(
                "{} volumes have more gaps than are listed here",
                self.volume_gap_lists
            ),
        );
        say(
            self.mount_lists,
            format!(
                "{} containers mount more volumes than are listed here",
                self.mount_lists
            ),
        );
        say(
            self.dropped_references,
            format!(
                "{} references to volumes that are not listed here were left out",
                self.dropped_references
            ),
        );
        lines
    }
}

/// What a services capture came to (`DaemonBackend::capture_services`).
#[derive(Clone, Debug)]
pub enum ServicesCapture {
    /// Services are off for the project, or its VM was never created: the
    /// checkpoint has `services: null`.
    Off,
    /// The snapshot was taken.
    Captured(ServicesRecord),
    /// The VM is stopped: nothing can have changed since its last capture.
    Stopped,
    /// Not captured, and why; the file checkpoint is unaffected.
    Gap(String),
}

/// What a checkpoint made while the services VM is stopped may reference
/// (`DaemonBackend::services_reuse`): decided from what marsh recorded at the
/// stop, per workspace, bound to the owned VM's identity.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)] // Built once per checkpoint, never stored.
pub enum ServicesReuse {
    /// This VM never captured this workspace: `services: null`.
    None,
    /// The workspace's last capture. `warning` is a gap the record must carry
    /// when the capture is not proven to be the last write (a stop outside
    /// marsh).
    Reuse {
        last: LastCapture,
        warning: Option<String>,
    },
    /// marsh stopped the VM and could not prove the workspace's data was
    /// captured (the final capture failed, was refused for space, or could not
    /// freeze the workspace): never the old snapshot, and why.
    Gap(String),
    /// What marsh recorded cannot be read (a lost journal): the checkpoint
    /// records fall back to the workspace's latest checkpoint, with a warning.
    Unknown,
}

/// One snapshot the services agent holds.
#[derive(Clone, Debug)]
pub struct ServicesSnapshotInfo {
    pub id: String,
    pub created_unix_ms: Option<u64>,
}

/// The project's latest base-workspace snapshot (D12): the source of
/// `reused_from`. `reason` is `checkpoint` or `idle-stop`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LastCapture {
    pub id: String,
    pub at_unix_ms: u64,
    pub reason: String,
    pub record: ServicesRecord,
}

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
    /// The volume's UUID (hex) when its filesystem guarantees persistent,
    /// never-reused file IDs: the directory is then recognised by volume
    /// and file ID even when the device number changed (a volume mounted
    /// again). `None` (other filesystems, older records): `identity` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
    /// `SPLIT_ID/LABEL` when this snapshots an open or kept lane's copy
    /// rather than the project (`Checkpoints::create_lane`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    /// The services snapshot this checkpoint holds or reuses; `None` when
    /// services are off for the project or no snapshot ever existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub services: Option<ServicesRecord>,
    /// The git-ignored state cloned into `<checkpoint>/ignored/` (C5), or
    /// `None` when services are off for the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignored: Option<crate::ignored::IgnoredRecord>,
    /// For a checkpoint of a lane: that lane's port map (original port number
    /// to the number its fork published), as it was when this was made.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub lane_ports: BTreeMap<String, u16>,
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
    /// The lane's own port map (original to the fork's published number), so a
    /// lane started from this checkpoint can name the **original** numbers
    /// again (C3).
    pub ports: BTreeMap<String, u16>,
}

/// Why a checkpoint was not made.
pub enum Refusal {
    /// The volume is too full (exit 75).
    LowDisk(String),
    Invalid(String),
}

/// Prefix (after `checkpoints.json.`) of a journal set aside as unreadable.
const SET_ASIDE: &str = "unreadable-";

pub struct Checkpoints {
    state: Mutex<State>,
    journal: PathBuf,
    events: Arc<Bus>,
    /// The backend that reaches the services VMs, once the daemon has one
    /// (`attach_backend`). Without it checkpoints carry no services.
    backend: Mutex<Option<Weak<dyn DaemonBackend>>>,
    /// The journal could not be read at load and could not be set aside
    /// either: it is never written over, and nothing is collected.
    unsaved: bool,
}

#[derive(Default)]
struct State {
    records: BTreeMap<String, Checkpoint>,
    /// Splits being made `--from` each checkpoint: it is not removed while
    /// one clones its tree.
    leases: BTreeMap<String, usize>,
    /// Snapshots a checkpoint being made has taken or is about to reference,
    /// before its record exists (a drop or the collector leaves them alone).
    pending: BTreeMap<String, usize>,
    /// Snapshots being dropped now: nothing may start to reference them.
    dropping: BTreeSet<String>,
}

/// A hold on snapshot ids until the record that names them is inserted.
struct Pending<'a> {
    checkpoints: &'a Checkpoints,
    ids: Vec<String>,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        let mut state = self.checkpoints.lock();
        for id in &self.ids {
            if let Some(count) = state.pending.get_mut(id) {
                *count -= 1;
                if *count == 0 {
                    state.pending.remove(id);
                }
            }
        }
    }
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
    /// Load the journal, forget checkpoints whose directory is verifiably
    /// gone (one that can't be reached now, say on an unmounted volume or
    /// behind a privacy prompt, is kept), and apply retention to every
    /// recorded project. A journal that can't be read is set aside (or, if
    /// that fails, left alone and never written), never overwritten.
    #[must_use]
    pub fn open(journal_home: &Path, events: Arc<Bus>) -> Self {
        let journal = journal_home.join("checkpoints.json");
        let (mut records, unsaved) = match std::fs::read(&journal) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (BTreeMap::new(), false),
            read => match read.map_err(|e| e.to_string()).and_then(|bytes| {
                serde_json::from_slice::<BTreeMap<String, Checkpoint>>(&bytes)
                    .map_err(|e| e.to_string())
            }) {
                Ok(records) => (records, false),
                Err(why) => {
                    let aside = journal
                        .with_extension(format!("json.{SET_ASIDE}{}", crate::unix_time_ms()));
                    let kept = std::fs::rename(&journal, &aside)
                        .or_else(|_| std::fs::copy(&journal, &aside).map(drop));
                    match &kept {
                        Ok(()) => eprintln!(
                            "marshd: could not read {} ({why}); set aside as {}; orphaned services snapshots are not collected while it is there",
                            journal.display(),
                            aside.display()
                        ),
                        Err(error) => eprintln!(
                            "marshd: could not read {} ({why}) nor set it aside ({error}); it is left as it is, checkpoints are not journaled and nothing is collected",
                            journal.display()
                        ),
                    }
                    (BTreeMap::new(), kept.is_err())
                }
            },
        };
        records.retain(|_, checkpoint| !checkpoint.gone());
        let checkpoints = Self {
            state: Mutex::new(State {
                records,
                ..State::default()
            }),
            journal,
            events,
            backend: Mutex::new(None),
            unsaved,
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

    /// Give checkpoints the backend that reaches the services VMs. Held
    /// weakly: the daemon owns the backend.
    pub fn attach_backend(&self, backend: &Weak<dyn DaemonBackend>) {
        *self.backend.lock().unwrap_or_else(PoisonError::into_inner) = Some(backend.clone());
        // Which lane workspaces the stop cutover must capture: every lane
        // that still has a checkpoint.
        if let Some(backend) = backend.upgrade() {
            let lanes: BTreeSet<(PathBuf, String)> = self
                .lock()
                .records
                .values()
                .filter_map(|c| {
                    let workspace = lane_workspace(c.lane.as_deref()?)?;
                    c.services.as_ref()?;
                    Some((c.root.clone(), workspace))
                })
                .collect();
            for (root, workspace) in &lanes {
                backend.services_track(root, workspace, true);
            }
            // A lane the backend journalled a capture for whose record did not
            // survive is no longer protected by that journal entry.
            // Not with a lost journal: its records' lanes are unknown.
            if !self.journal_lost() {
                backend.services_reconcile(&lanes);
            }
        }
    }

    /// The backend that reaches the services VMs, if the daemon has one.
    pub(crate) fn backend(&self) -> Option<Arc<dyn DaemonBackend>> {
        self.backend
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade)
    }

    /// Journal the records; under the state lock, so a later state is never
    /// overwritten by an earlier one.
    fn save(&self) {
        if self.unsaved {
            return;
        }
        let state = self.lock();
        if let Err(error) = crate::split::SplitEngine::write_durable(&self.journal, &state.records)
        {
            eprintln!("marshd: could not journal checkpoints: {error}");
        }
    }

    /// Whether a journal could not be read: one set aside (until the user
    /// restores or removes it, across restarts) or one left in place.
    /// Which snapshots its records named is unknown, so nothing is collected.
    fn journal_lost(&self) -> bool {
        self.unsaved
            || self.journal.parent().is_some_and(|home| {
                std::fs::read_dir(home).is_ok_and(|entries| {
                    entries.flatten().any(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(&format!("checkpoints.json.{SET_ASIDE}"))
                    })
                })
            })
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
                // Whether services were captured, reused, or left a gap.
                "services": checkpoint.services.as_ref().map_or("none", ServicesRecord::event_word),
            }),
        );
    }

    /// The checks every checkpoint starts with. The services pool is **not**
    /// read here: free space read before the tree copy and any wait is stale
    /// by the time the snapshot is taken, so the backend checks the reserve at
    /// the capture boundary (DECISIONS 5: a pool below it skips only the
    /// services capture, and exit 75 stays for the volume rule below).
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

    #[allow(clippy::too_many_arguments)] // The source, its Git state and what admit learned.
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
        let volume = dir
            .persistent_identity()
            .ok()
            .filter(|persistent| persistent.file_id() == identity.1)
            .map(|persistent| hex(&persistent.volume_uuid()));
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
        // The snapshot is held `pending` from before it is taken until the record
        // that names it exists, so a drop or the collector never decides on a
        // half-recorded reference; nothing is held across the capture itself,
        // so a second checkpoint reaches the agent's own busy answer.
        let ignored = self.ignored_for(root, &id, source, git);
        let (services, pending) = self.services_for(root, lane, &id);
        let checkpoint = Checkpoint {
            dir: root.join(".marsh/checkpoints").join(&id),
            id: id.clone(),
            root: root.to_path_buf(),
            label,
            created_unix_ms: crate::unix_time_ms(),
            bytes,
            git: git.is_some(),
            identity,
            volume,
            lane: lane.map(|lane| lane.name.clone()),
            services,
            ignored,
            lane_ports: lane.map(|lane| lane.ports.clone()).unwrap_or_default(),
        };
        self.lock().records.insert(id, checkpoint.clone());
        drop(pending);
        self.publish(&checkpoint, false);
        self.retain(root);
        self.save();
        Ok(checkpoint)
    }

    /// The folder local data of the checkpoint being made (C5): every
    /// git-ignored file but junk, cloned into `<checkpoint>/ignored/`, bounded
    /// by the backend's limits. Never an error: what could not be saved is in
    /// the record.
    fn ignored_for(
        &self,
        root: &Path,
        id: &str,
        source: &dyn Fn() -> Result<Dir, String>,
        git: Option<&split_fs::GitSnapshot>,
    ) -> Option<crate::ignored::IgnoredRecord> {
        let limits = self.backend()?.ignored_limits(root)?;
        let saved = (|| -> Result<crate::ignored::IgnoredRecord, String> {
            let area = split_fs::area(root, "checkpoints", false).map_err(|e| e.to_string())?;
            let dir = area.open(id).map_err(|e| e.to_string())?;
            let (tree, out) = (
                dir.open("tree").map_err(|e| e.to_string())?,
                dir.create("ignored").map_err(|e| e.to_string())?,
            );
            Ok(crate::ignored::capture(
                root,
                &source()?,
                &tree,
                &out,
                limits,
                git.map(|git| git.exclude.as_slice()),
            ))
        })();
        Some(saved.unwrap_or_else(|why| crate::ignored::IgnoredRecord {
            skipped: vec![crate::ignored::Skipped {
                path: String::new(),
                reason: format!("local data in the folder was not saved: {}", plain(&why)),
                lost: true,
            }],
            ..crate::ignored::IgnoredRecord::default()
        }))
    }

    /// Hold `ids` pending, unless one is being dropped (then `None`).
    fn pend(&self, ids: &[&str]) -> Option<Pending<'_>> {
        let mut state = self.lock();
        if ids.iter().any(|id| state.dropping.contains(*id)) {
            return None;
        }
        for id in ids {
            *state.pending.entry((*id).to_owned()).or_default() += 1;
        }
        Some(Pending {
            checkpoints: self,
            ids: ids.iter().map(|id| (*id).to_owned()).collect(),
        })
    }

    /// The `services` field of the checkpoint being made, and the hold on its
    /// snapshot: a snapshot of the workspace's objects, what a stopped VM's
    /// workspace may reference, or nothing. Never an error and never starts a
    /// VM.
    fn services_for(
        &self,
        root: &Path,
        lane: Option<&LaneSource>,
        id: &str,
    ) -> (Option<ServicesRecord>, Option<Pending<'_>>) {
        let Some(backend) = self.backend() else {
            return (None, None);
        };
        // A lane's checkpoint captures that lane's workspace only (A4).
        let workspace = match lane {
            None => BASE_WORKSPACE.to_owned(),
            Some(_) if marsh_contracts::break_mode(breaks::LANE_CAPTURE_ROOT) => {
                BASE_WORKSPACE.to_owned()
            }
            Some(lane) => match lane_workspace(&lane.name) {
                Some(workspace) => workspace,
                None => {
                    return (
                        Some(ServicesRecord::gap(format!(
                            "lane {} has no services workspace name",
                            lane.name
                        ))),
                        None,
                    );
                }
            },
        };
        if lane.is_some() {
            backend.services_track(root, &workspace, true);
        }
        let pending = self.pend(&[id]);
        // Test break: capture admission waits for the project's other capture
        // (the blocking lock a second checkpoint used to sit behind).
        let _serial = marsh_contracts::break_mode(breaks::CAPTURE_ADMISSION_BLOCKS).then(|| {
            CAPTURE_SERIAL
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        });
        match backend.capture_services(root, &workspace, id) {
            ServicesCapture::Off => (None, pending),
            ServicesCapture::Captured(record) => (Some(record), pending),
            ServicesCapture::Gap(why) => (Some(ServicesRecord::gap(why)), pending),
            ServicesCapture::Stopped => {
                let (record, held) = self.reuse_source(root, lane, &workspace, &*backend);
                (record, held.or(pending))
            }
        }
    }

    /// The VM is stopped: what the workspace's checkpoint may reference
    /// (DECISIONS 9, 12), as the backend decided from what marsh recorded at
    /// the stop. A reused snapshot is held pending like a new one.
    fn reuse_source(
        &self,
        root: &Path,
        lane: Option<&LaneSource>,
        workspace: &str,
        backend: &dyn DaemonBackend,
    ) -> (Option<ServicesRecord>, Option<Pending<'_>>) {
        let reuse = |last: LastCapture, warning: Option<String>| {
            let Some(held) = self.pend(&[last.id.as_str()]) else {
                return (
                    Some(ServicesRecord::gap(
                        "the last snapshot is being removed; this turn's Docker data was not saved",
                    )),
                    None,
                );
            };
            let mut record = last.record;
            record.snapshot = None;
            record.frozen_ms = None;
            record.reused_from = Some(last.id);
            if let Some(warning) = warning {
                record.gaps.insert(0, plain(&warning));
            }
            (Some(record), Some(held))
        };
        match backend.services_reuse(root, workspace) {
            ServicesReuse::None => (None, None),
            ServicesReuse::Gap(why) => (Some(ServicesRecord::gap(why)), None),
            ServicesReuse::Reuse { last, warning } => reuse(last, warning),
            ServicesReuse::Unknown => {
                // What marsh recorded is lost: the workspace's latest
                // checkpoint is the best known snapshot, said as such.
                let earlier = self
                    .list(Some(root), None)
                    .into_iter()
                    .filter(|c| c.lane.as_deref() == lane.map(|l| l.name.as_str()))
                    .find_map(|c| c.services.filter(|s| s.restore_source().is_some()));
                let Some(earlier) = earlier else {
                    return (None, None);
                };
                let Some(source) = earlier.restore_source().map(str::to_owned) else {
                    return (None, None);
                };
                reuse(
                    LastCapture {
                        id: source,
                        at_unix_ms: 0,
                        reason: "checkpoint".into(),
                        record: earlier,
                    },
                    Some(
                        "marsh lost its record of when the Docker VM stopped: changes after this snapshot may be missing"
                            .into(),
                    ),
                )
            }
        }
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
        let leased = || format!("checkpoint {id} is in use by a split being made from it; retry");
        let checkpoint = {
            let state = self.lock();
            let checkpoint = state
                .records
                .get(id)
                .filter(|checkpoint| root.is_none_or(|root| checkpoint.root == root))
                .cloned()
                .ok_or_else(|| format!("no checkpoint {id}"))?;
            if state.leases.contains_key(id) {
                return Err(leased());
            }
            checkpoint
        };
        let found = checkpoint.found().map_err(|why| {
            format!("{why}; it is kept, with its Docker data, until it can be checked")
        })?;
        {
            let mut state = self.lock();
            if state.leases.contains_key(id) {
                return Err(leased());
            }
            state.records.remove(id);
        }
        let message = self.forget(&checkpoint, found);
        self.release_snapshots(&checkpoint.root, std::slice::from_ref(&checkpoint));
        self.save();
        message
    }

    /// Move the files of a checkpoint whose record is already gone to
    /// `.marsh/.trash` (deleted in the background): the directory `found`
    /// (its identity now), or nothing when it is verifiably gone.
    fn forget(&self, checkpoint: &Checkpoint, found: Option<(u64, u64)>) -> Result<String, String> {
        let Some(identity) = found else {
            self.publish(checkpoint, true);
            return Ok(format!("removed checkpoint {}", checkpoint.id));
        };
        let removed = split_fs::area(&checkpoint.root, "checkpoints", false)
            .and_then(|area| {
                let trash = split_fs::area(&checkpoint.root, ".trash", true)?;
                Ok((area, trash))
            })
            .map_err(|e| e.to_string())
            .and_then(|(area, trash)| {
                split_fs::remove_verified_detached(&area, &checkpoint.id, identity, &trash)
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
            // One that can't be checked now (unreachable, or not the
            // recorded directory) is kept with its snapshots.
            .filter_map(|checkpoint| {
                let found = checkpoint.found().ok()?;
                Some((checkpoint, found))
            })
            .collect::<Vec<_>>();
        let expired = {
            let mut state = self.lock();
            let State {
                records, leases, ..
            } = &mut *state;
            expired
                .into_iter()
                .filter(|(checkpoint, _)| !leases.contains_key(&checkpoint.id))
                .filter(|(checkpoint, _)| records.remove(&checkpoint.id).is_some())
                .collect::<Vec<_>>()
        };
        for (checkpoint, found) in &expired {
            let _ = self.forget(checkpoint, *found);
        }
        let expired = expired
            .into_iter()
            .map(|(checkpoint, _)| checkpoint)
            .collect::<Vec<_>>();
        // Test break: retention removes the checkpoints and keeps their
        // snapshots (the orphan collector is what would still find them).
        if !marsh_contracts::break_mode(breaks::RETENTION_KEEP_SNAPSHOTS) {
            self.release_snapshots(root, &expired);
        }
    }

    /// Whether `snapshot` is in use: a live checkpoint of `root` names it
    /// through `snapshot` or `reused_from` (a leased checkpoint is a live
    /// record, so a snapshot a running restore reads is held by that lease), a
    /// checkpoint being made holds it `pending`, or the backend protects it (a
    /// workspace's last capture, a capture being recorded). Called with the
    /// state locked.
    fn in_use(state: &State, root: &Path, snapshot: &str, protected: &BTreeSet<String>) -> bool {
        if protected.contains(snapshot) || state.pending.contains_key(snapshot) {
            return true;
        }
        // Test break: a reuse reference is not a reference to the collector
        // (the field the checkpoint returns stays intact).
        let ignore_reuse = marsh_contracts::break_mode(breaks::REUSE_LEASE_OFF);
        state
            .records
            .values()
            .filter(|checkpoint| checkpoint.root == root)
            .filter_map(|checkpoint| checkpoint.services.as_ref())
            .any(|services| {
                services.snapshot.as_deref() == Some(snapshot)
                    || (!ignore_reuse && services.reused_from.as_deref() == Some(snapshot))
            })
    }

    /// Decide that `snapshot` may be dropped and mark it so, in one step under
    /// the state lock: from here nothing starts to reference it.
    fn begin_drop(&self, root: &Path, snapshot: &str, protected: &BTreeSet<String>) -> bool {
        let mut state = self.lock();
        if Self::in_use(&state, root, snapshot, protected) || state.dropping.contains(snapshot) {
            return false;
        }
        state.dropping.insert(snapshot.to_owned());
        true
    }

    fn end_drop(&self, snapshot: &str) {
        self.lock().dropping.remove(snapshot);
    }

    /// Drop the snapshots of checkpoints that are gone, each unless another
    /// checkpoint still references it (`reused_from`): the last reference
    /// drops it. A lane that has no checkpoint left is no longer tracked, so
    /// its stop capture is no longer protected. Unreachable (VM stopped) or
    /// busy: left to the orphan collector, which finds it in the agent's list.
    fn release_snapshots(&self, root: &Path, removed: &[Checkpoint]) {
        // A lost journal's records may name the same snapshots: none is
        // released or untracked until it is restored or removed.
        if self.journal_lost() {
            return;
        }
        let Some(backend) = self.backend() else {
            return;
        };
        let lanes: BTreeSet<String> = removed
            .iter()
            .filter(|c| c.services.is_some())
            .filter_map(|c| c.lane.clone())
            .collect();
        for lane in lanes {
            let remains = self
                .lock()
                .records
                .values()
                .any(|c| c.root == root && c.lane.as_deref() == Some(lane.as_str()));
            if let (false, Some(workspace)) = (remains, lane_workspace(&lane)) {
                backend.services_track(root, &workspace, false);
            }
        }
        let candidates: BTreeSet<&str> = removed
            .iter()
            .filter_map(|checkpoint| checkpoint.services.as_ref())
            .flat_map(ServicesRecord::references)
            .collect();
        if candidates.is_empty() {
            return;
        }
        let protected = backend.services_protected(root);
        for id in candidates {
            if !self.begin_drop(root, id, &protected) {
                continue;
            }
            let dropped = backend.drop_services(root, id);
            self.end_drop(id);
            if let Err(why) = dropped {
                eprintln!(
                    "marshd: services snapshot {id} of {} was not dropped now ({why}); the collector retries",
                    root.display()
                );
            }
        }
    }

    /// Collect orphaned snapshots: present in a project's services agent but
    /// referenced by no checkpoint, not a workspace's last capture, and not
    /// young enough to be still recording. Runs from the daemon's services
    /// timer, so the first pass after a start collects what a lost checkpoint
    /// directory or a crash left. The projects are every one with a checkpoint
    /// record **or an owned services VM**, found without any record: a
    /// project whose last record is gone is still scanned. Only a ready VM is
    /// listed; it never starts one to look.
    pub fn collect_orphans(&self) {
        if marsh_contracts::break_mode(breaks::ORPHAN_GC_OFF) || self.journal_lost() {
            return;
        }
        let Some(backend) = self.backend() else {
            return;
        };
        let mut roots: BTreeSet<PathBuf> =
            if marsh_contracts::break_mode(breaks::ORPHAN_ROOTS_FROM_RECORDS) {
                self.roots().into_iter().collect()
            } else {
                backend.services_snapshot_roots().into_iter().collect()
            };
        roots.extend(self.roots());
        for root in roots {
            self.collect_orphans_of(&root, &*backend);
        }
    }

    fn collect_orphans_of(&self, root: &Path, backend: &dyn DaemonBackend) {
        // A checkpoint that can't be reached now still names its snapshots.
        if self
            .list(Some(root), None)
            .iter()
            .any(|c| c.open().is_err())
        {
            return;
        }
        let Ok(listed) = backend.list_services(root) else {
            return;
        };
        // Read after the list: whatever the list shows was published before,
        // so its record or protection already exists or never will.
        let protected = backend.services_protected(root);
        let now = crate::unix_time_ms();
        for entry in listed {
            let young = entry.created_unix_ms.is_some_and(|at| {
                (0..i128::from(ORPHAN_GRACE_MS)).contains(&(i128::from(now) - i128::from(at)))
            });
            if young || !self.begin_drop(root, &entry.id, &protected) {
                continue;
            }
            let dropped = backend.drop_services(root, &entry.id);
            self.end_drop(&entry.id);
            match dropped {
                Ok(_) => eprintln!(
                    "marshd: collected orphaned services snapshot {} of {}",
                    entry.id,
                    root.display()
                ),
                Err(why) => eprintln!(
                    "marshd: orphaned services snapshot {} of {} not collected yet ({why})",
                    entry.id,
                    root.display()
                ),
            }
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
        self.identity_of(&dir)?;
        Ok(dir)
    }

    /// `dir`'s identity now, if it is this checkpoint's directory: the
    /// recorded `(dev, ino)`, or, with a recorded volume, the same volume
    /// UUID and file ID (the device number changes when a volume is mounted
    /// again).
    fn identity_of(&self, dir: &Dir) -> Result<(u64, u64), String> {
        let now = dir.fstat().map_err(|e| e.to_string())?.identity;
        let same_volume = || {
            self.volume.as_ref().is_some_and(|volume| {
                dir.persistent_identity().is_ok_and(|persistent| {
                    hex(&persistent.volume_uuid()) == *volume
                        && persistent.file_id() == self.identity.1
                })
            })
        };
        if now == self.identity || same_volume() {
            Ok(now)
        } else {
            Err(format!("checkpoint {} was replaced", self.id))
        }
    }

    /// What removing this checkpoint may act on: `Some` identity of its
    /// directory, `None` when the directory is verifiably gone. Anything
    /// else (unreachable, not the recorded directory) is an error, and the
    /// checkpoint and its snapshots are kept.
    fn found(&self) -> Result<Option<(u64, u64)>, String> {
        match self.open() {
            Ok(dir) => self.identity_of(&dir).map(Some),
            Err(_) if self.gone() => Ok(None),
            Err(why) => Err(why),
        }
    }

    /// Whether the checkpoint directory is verifiably gone: its project
    /// opens and the directory is missing. Any other failure (the project
    /// unreachable, access denied, a directory that is not the recorded one)
    /// is not proof: the record, and so its snapshots, stay.
    fn gone(&self) -> bool {
        self.open().is_err()
            && Dir::open_root(&self.root).is_ok()
            && split_fs::area(&self.root, "checkpoints", false)
                .and_then(|area| area.open(&self.id))
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
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
            "services": self.services,
            "ignored": self.ignored,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// The names a lane may have (table).
    #[test]
    fn lane_workspaces_are_named_from_the_lane_and_validated() {
        for (lane, expected) in [
            ("abc123/a", Some("fork-abc123-a")),
            ("abc123/feature_1.x", Some("fork-abc123-feature_1.x")),
            ("abc123/", None),
            ("/a", None),
            ("abc123", None),
            ("abc123/has space", None),
            ("abc123/../x", None),
            ("abc123/a/b", None),
        ] {
            assert_eq!(lane_workspace(lane).as_deref(), expected, "{lane}");
        }
        assert_eq!(lane_workspace(&format!("s/{}", "x".repeat(70))), None);
    }

    /// The gap checkpoints show for a pool below its reserve is the text the
    /// services agent writes (`marsh_services::capture::NO_SPACE_GAP`: the agent
    /// cannot depend on this crate, so both are pinned to the same words).
    #[test]
    fn the_full_pool_gap_is_the_agents_text() {
        assert_eq!(NO_SPACE_GAP, "not enough space for app data");
    }

    type Defect = (&'static str, Box<dyn Fn(&mut Value)>);

    fn many(n: usize, make: &dyn Fn(usize) -> Value) -> Vec<Value> {
        (0..n).map(make).collect()
    }

    fn volume(id: &str, name: &str) -> Value {
        json!({"id": id, "name": name, "captured": true, "files": 1, "gaps": []})
    }

    fn container(name: &str) -> Value {
        json!({"name": name, "service": null, "image": "busybox", "running": true, "volumes": []})
    }

    fn manifest() -> Value {
        json!({
            "id": "s1", "workspace": "base", "complete": true, "frozen_ms": 61,
            "volumes": [volume("v1", "app_pgdata")],
            "stacks": [{"kind": "compose", "project": "app", "containers": [
                {"name": "db", "service": "db", "image": "postgres", "running": true, "volumes": ["v1"]}]}],
            "standalone": [container("keeper")],
            "gaps": [],
        })
    }

    fn build(manifest: &Value) -> Result<ServicesRecord, String> {
        ServicesRecord::from_manifest("s1", "base", Some(manifest), &[])
    }

    /// A well-formed manifest becomes a record; what the VM says is display
    /// text only (control characters made plain), unknown fields are ignored.
    #[test]
    fn a_wellformed_manifest_is_recorded_with_the_vms_text_made_plain() {
        let mut m = manifest();
        m["volumes"][0]["name"] = json!("pg\u{1b}[31mdata\nx");
        m["unknown"] = json!({"x": 1});
        let record = build(&m).unwrap();
        assert_eq!(record.snapshot.as_deref(), Some("s1"));
        assert_eq!(record.frozen_ms, Some(61));
        assert_eq!(record.volumes[0].name, "pg [31mdata x");
        assert_eq!(
            record.stacks[0].containers[0].volumes,
            vec!["v1".to_owned()]
        );
        assert_eq!(record.standalone, vec!["keeper".to_owned()]);
        assert_eq!(record.gaps.len(), 0);
        let encoded = serde_json::to_string(&record).unwrap();
        assert!(!encoded.contains('\u{1b}'));
    }

    /// A manifest that does not validate is rejected whole, so the record is a
    /// capture gap and never an apparently complete one (table of defects).
    #[test]
    fn a_malformed_or_missing_manifest_is_rejected_not_filtered() {
        assert!(ServicesRecord::from_manifest("s1", "base", None, &[]).is_err());
        assert!(ServicesRecord::from_manifest("s1", "base", Some(&json!([])), &[]).is_err());
        let defects: Vec<Defect> = vec![
            ("another snapshot", Box::new(|m| m["id"] = json!("s2"))),
            (
                "another workspace",
                Box::new(|m| m["workspace"] = json!("lane")),
            ),
            (
                "no complete",
                Box::new(|m| {
                    m.as_object_mut().unwrap().remove("complete");
                }),
            ),
            ("no volumes list", Box::new(|m| m["volumes"] = json!("x"))),
            (
                "no stacks list",
                Box::new(|m| {
                    m.as_object_mut().unwrap().remove("stacks");
                }),
            ),
            (
                "no standalone list",
                Box::new(|m| m["standalone"] = json!(null)),
            ),
            (
                "volume id is a path",
                Box::new(|m| m["volumes"][0]["id"] = json!("../x")),
            ),
            (
                "volume without captured",
                Box::new(|m| {
                    m["volumes"][0].as_object_mut().unwrap().remove("captured");
                }),
            ),
            (
                "container without a name",
                Box::new(|m| m["standalone"][0]["name"] = json!("")),
            ),
            (
                "container names a bad volume",
                Box::new(|m| {
                    m["stacks"][0]["containers"][0]["volumes"] = json!(["../x"]);
                }),
            ),
            (
                "stack container names a volume the manifest does not list",
                Box::new(|m| {
                    m["stacks"][0]["containers"][0]["volumes"] = json!(["missing"]);
                }),
            ),
            (
                "standalone container names a volume the manifest does not list",
                Box::new(|m| m["standalone"][0]["volumes"] = json!(["v1", "missing"])),
            ),
            (
                "two volumes share an id",
                Box::new(|m| {
                    m["volumes"] = json!([volume("v1", "a"), volume("v1", "b")]);
                }),
            ),
            (
                "container without running",
                Box::new(|m| {
                    m["standalone"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("running");
                }),
            ),
        ];
        for (what, defect) in defects {
            let mut m = manifest();
            defect(&mut m);
            assert!(build(&m).is_err(), "{what} must be rejected");
        }
    }

    /// Every cap says what it cut, in a gap that survives the gap cap (table of
    /// boundaries: at the limit nothing is said, one over says it).
    #[test]
    fn every_truncation_is_said_in_a_retained_gap() {
        type Case = (
            &'static str,
            Box<dyn Fn(usize) -> Value>,
            usize,
            &'static str,
        );
        let cases: Vec<Case> = vec![
            (
                "standalone",
                Box::new(|n| {
                    let mut m = manifest();
                    m["standalone"] = json!(many(n, &|i| container(&format!("c{i}"))));
                    m
                }),
                MAX_CONTAINERS,
                "standalone containers",
            ),
            (
                "volumes",
                Box::new(|n| {
                    let mut m = manifest();
                    m["volumes"] = json!(many(n, &|i| volume(&format!("v{i}"), "x")));
                    m["stacks"] = json!([]);
                    m
                }),
                MAX_VOLUMES,
                "volumes were saved",
            ),
            (
                "stacks",
                Box::new(|n| {
                    let mut m = manifest();
                    m["stacks"] = json!(many(
                        n,
                        &|i| json!({"kind": "compose", "project": format!("p{i}"), "containers": []})
                    ));
                    m
                }),
                MAX_STACKS,
                "stacks were saved",
            ),
            (
                "stack members",
                Box::new(|n| {
                    let mut m = manifest();
                    m["stacks"][0]["containers"] = json!(many(n, &|i| container(&format!("c{i}"))));
                    m
                }),
                MAX_CONTAINERS,
                "containers of stacks are not listed",
            ),
        ];
        for (what, make, limit, words) in cases {
            let at = build(&make(limit)).unwrap();
            assert!(
                at.gaps.is_empty(),
                "{what} at the limit says nothing: {:?}",
                at.gaps
            );
            let over = build(&make(limit + 1)).unwrap();
            assert!(
                over.gaps.iter().any(|g| g.contains(words)),
                "{what} one over is said: {:?}",
                over.gaps
            );
        }
        // The omission survives 200 ordinary gaps.
        let mut m = manifest();
        m["standalone"] = json!(many(MAX_CONTAINERS + 1, &|i| container(&format!("c{i}"))));
        m["gaps"] = json!(many(200, &|i| json!(format!("gap {i}"))));
        let record = build(&m).unwrap();
        assert!(
            record.gaps[0].contains("standalone containers"),
            "{:?}",
            record.gaps.first()
        );
        assert_eq!(record.gaps.len(), MAX_GAPS, "{:?}", record.gaps.len());
        assert!(
            record
                .gaps
                .last()
                .unwrap()
                .contains("more gaps are not listed")
        );
        // An incomplete snapshot never has an empty gap list.
        let mut m = manifest();
        m["complete"] = json!(false);
        assert_eq!(
            build(&m).unwrap().gaps,
            vec!["the snapshot is incomplete".to_owned()]
        );
    }

    /// However many items the caps cut, the cuts are a bounded summary that is
    /// kept whole, the fact of truncation is never lost, and the whole list of
    /// gaps stays within `MAX_GAPS` (table of kinds and amounts).
    #[test]
    fn omissions_are_summarized_and_the_gap_list_stays_bounded() {
        fn volumes_over() -> Value {
            json!(many(MAX_VOLUMES + 1, &|i| volume(&format!("v{i}"), "x")))
        }
        // 80 containers, each mounting one volume more than a record lists.
        fn heavy_mounts() -> Value {
            let mut m = manifest();
            m["volumes"] = volumes_over();
            m["stacks"] = json!([]);
            let refs: Vec<Value> = (0..=MAX_VOLUMES).map(|i| json!(format!("v{i}"))).collect();
            m["standalone"] = json!(many(80, &|i| {
                let mut c = container(&format!("c{i}"));
                c["volumes"] = json!(refs);
                c
            }));
            m
        }
        // Many volumes, each with more gaps than a record lists.
        fn heavy_gaps() -> Value {
            let mut m = manifest();
            m["stacks"] = json!([]);
            m["volumes"] = json!(many(100, &|i| {
                let mut v = volume(&format!("v{i}"), "x");
                v["gaps"] = json!(many(MAX_GAPS + 1, &|g| json!(format!("g{i}-{g}"))));
                v
            }));
            m
        }
        for (what, make, summary) in [
            (
                "80 over-limit mount lists",
                heavy_mounts as fn() -> Value,
                "80 containers mount more volumes",
            ),
            (
                "100 over-limit gap lists",
                heavy_gaps,
                "100 volumes have more gaps",
            ),
        ] {
            let record = build(&make()).unwrap();
            assert!(
                record.gaps.len() <= MAX_GAPS,
                "{what}: {}",
                record.gaps.len()
            );
            assert!(
                record.gaps.iter().any(|gap| gap.contains(summary)),
                "{what}: the truncation is said: {:?}",
                record.gaps
            );
            // Plus 200 ordinary gaps: still bounded, the summary still first.
            let mut m = make();
            m["gaps"] = json!(many(200, &|i| json!(format!("gap {i}"))));
            let record = build(&m).unwrap();
            assert_eq!(record.gaps.len(), MAX_GAPS, "{what}");
            assert!(
                record.gaps.iter().take(7).any(|gap| gap.contains(summary)),
                "{what}: the summary leads the list: {:?}",
                &record.gaps[..3]
            );
            assert!(
                record
                    .gaps
                    .last()
                    .unwrap()
                    .contains("more gaps are not listed")
            );
        }
        // A reference to a volume that exists but was cut from the listing is
        // dropped from the record and said; it is not a dangling reference.
        let mut m = manifest();
        m["volumes"] = volumes_over();
        m["stacks"] = json!([]);
        m["standalone"][0]["volumes"] = json!(["v0", format!("v{MAX_VOLUMES}")]);
        let record = build(&m).unwrap();
        assert!(
            record
                .gaps
                .iter()
                .any(|gap| gap.contains("1 references to volumes that are not listed")),
            "{:?}",
            record.gaps
        );
    }

    /// A restart forgets a checkpoint only when its directory is verifiably
    /// gone: an unreachable project keeps it (and so its snapshots), and an
    /// unreadable journal is set aside, not overwritten.
    #[test]
    fn a_restart_keeps_checkpoints_it_cannot_reach() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let project = root.join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("a"), "a").unwrap();
        let reopen = || Checkpoints::open(&root, Arc::new(Bus::default()));
        let Ok(made) = reopen().create(&project, None) else {
            panic!("checkpoint refused");
        };
        let chmod =
            |bits| std::fs::set_permissions(&project, std::fs::Permissions::from_mode(bits));
        chmod(0o000).unwrap();
        let kept = reopen().list(Some(&project), None).len();
        chmod(0o755).unwrap();
        assert_eq!(kept, 1, "an unreadable project dropped its checkpoint");
        std::fs::rename(&project, root.join("away")).unwrap();
        assert_eq!(reopen().list(Some(&project), None).len(), 1);
        std::fs::rename(root.join("away"), &project).unwrap();
        std::fs::remove_dir_all(&made.dir).unwrap();
        assert!(reopen().list(Some(&project), None).is_empty());

        std::fs::write(root.join("checkpoints.json"), "{not json").unwrap();
        let lost = reopen();
        assert!(lost.journal_lost());
        let aside = std::fs::read_dir(&root).unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("checkpoints.json.unreadable-")
        });
        assert!(aside);
    }

    /// A services backend that lists one old snapshot per project and
    /// records what the collector lists and drops.
    #[derive(Default)]
    struct Recorder {
        roots: Vec<PathBuf>,
        listed: Mutex<Vec<PathBuf>>,
        dropped: Mutex<Vec<String>>,
        reconciled: Mutex<usize>,
    }

    impl DaemonBackend for Recorder {
        fn registered_commands(&self) -> Result<Vec<String>, crate::DaemonError> {
            Ok(Vec::new())
        }
        fn prepare(
            &self,
            _: &crate::LoadSelection,
            _: &crate::SessionSpec,
            _: crate::PreparationProgress,
            _: crate::DaemonStore,
        ) -> Result<crate::PreparationResult, crate::DaemonError> {
            Err(crate::DaemonError::BackendUnavailable)
        }
        fn execute(
            &self,
            _: crate::ExecuteSpec,
            _: crate::ServerAttachment,
            _: crate::DaemonStore,
        ) -> Result<(), crate::DaemonError> {
            Err(crate::DaemonError::BackendUnavailable)
        }
        fn open_shell(
            &self,
            _: crate::ShellSpec,
            _: crate::ServerAttachment,
            _: crate::DaemonStore,
        ) -> Result<(), crate::DaemonError> {
            Err(crate::DaemonError::BackendUnavailable)
        }
        fn services_snapshot_roots(&self) -> Vec<PathBuf> {
            self.roots.clone()
        }
        fn list_services(&self, root: &Path) -> Result<Vec<ServicesSnapshotInfo>, String> {
            self.listed.lock().unwrap().push(root.to_path_buf());
            Ok(vec![ServicesSnapshotInfo {
                id: "saved-data".into(),
                created_unix_ms: Some(0),
            }])
        }
        fn drop_services(&self, _: &Path, id: &str) -> Result<bool, String> {
            self.dropped.lock().unwrap().push(id.to_owned());
            Ok(true)
        }
        fn services_reconcile(&self, _: &BTreeSet<(PathBuf, String)>) {
            *self.reconciled.lock().unwrap() += 1;
        }
    }

    /// Rewrites every journaled checkpoint under `root`.
    fn edit_journal(root: &Path, edit: impl Fn(&mut serde_json::Value)) {
        let journal = root.join("checkpoints.json");
        let mut records: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
        for record in records.as_object_mut().unwrap().values_mut() {
            edit(record);
        }
        std::fs::write(&journal, records.to_string()).unwrap();
    }

    /// While a set-aside journal exists, removing a checkpoint releases none
    /// of its snapshots (a lost record may share them), and attaching the
    /// backend does not reconcile its lanes away.
    #[test]
    fn a_lost_journal_stops_every_snapshot_release() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let project = root.join("project");
        std::fs::create_dir(&project).unwrap();
        let made = Checkpoints::open(&root, Arc::new(Bus::default()))
            .create(&project, None)
            .ok()
            .unwrap();
        edit_journal(&root, |record| {
            record["services"] = serde_json::json!({"snapshot": "shared"});
        });
        std::fs::write(root.join("checkpoints.json.unreadable-1"), "{").unwrap();
        let checkpoints = Checkpoints::open(&root, Arc::new(Bus::default()));
        let recorder = Arc::new(Recorder::default());
        let backend: Arc<dyn DaemonBackend> = Arc::clone(&recorder) as _;
        checkpoints.attach_backend(&Arc::downgrade(&backend));
        assert_eq!(*recorder.reconciled.lock().unwrap(), 0);
        assert!(checkpoints.remove(Some(&project), &made.id).is_ok());
        assert_eq!(*recorder.dropped.lock().unwrap(), Vec::<String>::new());
        // Without it, the same removal releases the snapshot.
        std::fs::remove_file(root.join("checkpoints.json.unreadable-1")).unwrap();
        let again = Checkpoints::open(&root, Arc::new(Bus::default()))
            .create(&project, None)
            .ok()
            .unwrap();
        edit_journal(&root, |record| {
            record["services"] = serde_json::json!({"snapshot": "shared"});
        });
        let checkpoints = Checkpoints::open(&root, Arc::new(Bus::default()));
        checkpoints.attach_backend(&Arc::downgrade(&backend));
        assert_eq!(*recorder.reconciled.lock().unwrap(), 1);
        assert!(checkpoints.remove(Some(&project), &again.id).is_ok());
        assert_eq!(*recorder.dropped.lock().unwrap(), ["shared"]);
    }

    /// A checkpoint whose directory can't be shown to be the recorded one
    /// (a device number changed, no volume recorded) is kept by `rm` and
    /// by retention, with its directory; one with a recorded volume is
    /// still recognised (macOS APFS).
    #[test]
    fn an_unverifiable_checkpoint_is_kept_by_rm_and_retention() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let project = root.join("project");
        std::fs::create_dir(&project).unwrap();
        #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
        let made = Checkpoints::open(&root, Arc::new(Bus::default()))
            .create(&project, None)
            .ok()
            .unwrap();
        let remount = |record: &mut serde_json::Value| {
            let dev = record["identity"][0].as_u64().unwrap();
            record["identity"][0] = (dev + 1).into();
        };
        #[cfg(target_os = "macos")]
        {
            assert!(made.volume.is_some());
            edit_journal(&root, remount);
            let kept =
                Checkpoints::open(&root, Arc::new(Bus::default())).list(Some(&project), None);
            assert_eq!(kept.len(), 1);
            assert!(kept[0].open().is_ok(), "same volume and file ID");
        }
        // Remove the volume (an older record, or another filesystem).
        let made = Checkpoints::open(&root, Arc::new(Bus::default()))
            .create(&project, None)
            .ok()
            .unwrap();
        edit_journal(&root, |record| {
            if record["id"] == made.id.as_str() {
                remount(record);
                record["created_unix_ms"] = 0.into();
                record.as_object_mut().unwrap().remove("volume");
            }
        });
        let checkpoints = Checkpoints::open(&root, Arc::new(Bus::default()));
        assert!(
            checkpoints.lease(&project, &made.id).is_some(),
            "kept at load and by retention"
        );
        let refused = checkpoints.remove(Some(&project), &made.id).unwrap_err();
        assert!(refused.contains("kept"), "{refused}");
        assert!(made.dir.is_dir());
        assert!(checkpoints.lease(&project, &made.id).is_some());
    }

    /// A recorded volume must match: the same file ID on another volume
    /// (and a changed device number) is not this checkpoint, so `rm` keeps
    /// it and its directory.
    #[cfg(target_os = "macos")]
    #[test]
    fn another_volume_with_the_same_file_id_is_not_the_checkpoint() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let project = root.join("project");
        std::fs::create_dir(&project).unwrap();
        let made = Checkpoints::open(&root, Arc::new(Bus::default()))
            .create(&project, None)
            .ok()
            .unwrap();
        assert!(made.volume.is_some());
        edit_journal(&root, |record| {
            record["identity"][0] = (record["identity"][0].as_u64().unwrap() + 1).into();
            record["volume"] = "00000000000000000000000000000000".into();
        });
        let checkpoints = Checkpoints::open(&root, Arc::new(Bus::default()));
        let record = checkpoints.list(Some(&project), None).pop().unwrap();
        assert!(record.open().is_err());
        assert!(checkpoints.remove(Some(&project), &made.id).is_err());
        assert!(made.dir.is_dir());
        // The right volume but another file ID (a replaced directory on a
        // remounted volume) is not the checkpoint either.
        let volume = made.volume.clone().unwrap();
        edit_journal(&root, |record| {
            record["volume"] = volume.as_str().into();
            record["identity"][1] = (record["identity"][1].as_u64().unwrap() + 1).into();
        });
        let checkpoints = Checkpoints::open(&root, Arc::new(Bus::default()));
        let record = checkpoints.list(Some(&project), None).pop().unwrap();
        assert!(record.open().is_err());
        assert!(checkpoints.remove(Some(&project), &made.id).is_err());
        assert!(made.dir.is_dir());
    }

    /// A checkpoint recognised on a remounted volume (new device number,
    /// same volume and file ID) is removed through the identity it has now.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_remounted_checkpoint_can_be_removed() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let project = root.join("project");
        std::fs::create_dir(&project).unwrap();
        let made = Checkpoints::open(&root, Arc::new(Bus::default()))
            .create(&project, None)
            .ok()
            .unwrap();
        assert!(made.volume.is_some());
        edit_journal(&root, |record| {
            record["identity"][0] = (record["identity"][0].as_u64().unwrap() + 1).into();
        });
        let checkpoints = Checkpoints::open(&root, Arc::new(Bus::default()));
        assert!(checkpoints.remove(Some(&project), &made.id).is_ok());
        assert!(!made.dir.exists());
    }

    /// What one orphan pass over `checkpoints` drops.
    fn collected(checkpoints: &Checkpoints, roots: &[PathBuf]) -> Vec<String> {
        let recorder = Arc::new(Recorder {
            roots: roots.to_vec(),
            ..Recorder::default()
        });
        let backend: Arc<dyn DaemonBackend> = Arc::clone(&recorder) as _;
        checkpoints.attach_backend(&Arc::downgrade(&backend));
        checkpoints.collect_orphans();
        recorder.dropped.lock().unwrap().clone()
    }

    /// An unreadable journal stops collection across restarts until the
    /// set-aside file is gone, even after a later save writes a valid one;
    /// a project with an unreachable checkpoint is not collected; a changed
    /// device number alone keeps the checkpoint.
    #[test]
    fn collection_waits_for_lost_journals_and_unreachable_checkpoints() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let project = root.join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("a"), "a").unwrap();
        let reopen = || Checkpoints::open(&root, Arc::new(Bus::default()));
        let roots = [project.clone()];
        assert_eq!(collected(&reopen(), &roots), ["saved-data"]);

        std::fs::write(root.join("checkpoints.json"), "{not json").unwrap();
        let lost = reopen();
        assert_eq!(collected(&lost, &roots), Vec::<String>::new());
        assert!(lost.create(&project, None).is_ok(), "saves a valid journal");
        drop(lost);
        assert_eq!(
            collected(&reopen(), &roots),
            Vec::<String>::new(),
            "second restart"
        );
        for entry in std::fs::read_dir(&root).unwrap().flatten() {
            if entry.file_name().to_string_lossy().contains(".unreadable-") {
                std::fs::remove_file(entry.path()).unwrap();
            }
        }
        assert_eq!(collected(&reopen(), &roots), ["saved-data"]);

        let chmod = |bits| {
            std::fs::set_permissions(&project, std::fs::Permissions::from_mode(bits)).unwrap();
        };
        chmod(0o000);
        let skipped = collected(&reopen(), &roots);
        chmod(0o755);
        assert_eq!(
            skipped,
            Vec::<String>::new(),
            "collected beside an unreachable checkpoint"
        );
    }

    /// An unreadable journal that can't be renamed (immutable) is copied
    /// aside, so collection stays off, and it is left as it was.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_unrenamable_lost_journal_is_copied_aside() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let journal = root.join("checkpoints.json");
        std::fs::write(&journal, "{not json").unwrap();
        let chflags = |flag: &str| {
            assert!(
                std::process::Command::new("chflags")
                    .arg(flag)
                    .arg(&journal)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        chflags("uchg");
        let lost = Checkpoints::open(&root, Arc::new(Bus::default()));
        let journal_lost = lost.journal_lost();
        let aside = std::fs::read_dir(&root).unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("checkpoints.json.unreadable-")
        });
        chflags("nouchg");
        assert!(journal_lost);
        assert!(aside, "the copy fallback set nothing aside");
        assert_eq!(std::fs::read(&journal).unwrap(), b"{not json");
    }

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
