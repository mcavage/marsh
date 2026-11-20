//! Per-daemon ownership map of stock VMs this daemon created, plus a cached
//! stock inventory view. The map is the only source of a VM's name.
//!
//! Assumptions: host `sbx` is trusted, and every name is a fixed prefix plus a
//! random 8-character base36 id, so a name this daemon chose is not reused by
//! anyone else.
//!
//! Rules:
//! - Names are looked up by `(purpose, key)`; a missing entry gets a fresh
//!   random name persisted as an *intent* (no UUID) before `sbx create`.
//! - After create the observed UUID is recorded. An intent whose name later
//!   appears in inventory is adopted. An intent whose name is absent is never
//!   dropped: an `sbx create` outlives the daemon that submitted it (its CLI
//!   child, or the request inside sandboxd), so no timer proves it will not
//!   land. A later use of the same `(purpose, key)` reuses the name.
//! - A VM is usable only if its name exists with the recorded UUID. A name
//!   present with a different UUID, or not in the map, is foreign: refuse.
//! - Stop/remove only ever target a recorded UUID that is currently present.
//!   Removal drops the entry, so the next use gets a new random name, except
//!   that a replacement created under a just-removed name keeps it
//!   (`restore_intent`), and `record` restores a retired name's purpose/key.
//!
//! The inventory view is refreshed (one `sbx ls --json`) only on cold start,
//! after a lifecycle change, or after a failure. Warm requests use the cache.
//!
//! A test scope may also name a mirror file outside its control home: every
//! write of the map (intents included, so before any create) is copied there
//! first, so the map outlives a deleted scope directory (`make check-gc`).

use super::{SbxError, StockInventory};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

/// Purpose recorded for VMs created by a daemon before purposes existed or
/// recorded without a lookup key.
const UNKEYED: &str = "unkeyed";
/// A VM retired for good (a `marsh workers stop` that did not settle, or a
/// VM the removed `marsh --dev` left, see `retire_dev_vms`): never
/// looked up, prepared or admitted again, and removed as soon as possible,
/// also by the next daemon. The purpose keeps the file schema (and an older
/// marsh) unchanged: an unknown purpose is never looked up by any marsh.
const RETIRED: &str = "retired";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum VmClass {
    Absent,
    Owned { uuid: String, running: bool },
    Foreign,
}

/// What an owned VM is for. The prefix is part of the random name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VmPurpose {
    Kit,
    Shell,
    /// A project's services VM (`docs/design/services.md`), keyed by the
    /// canonical project root.
    Services,
}

impl VmPurpose {
    fn label(self) -> &'static str {
        match self {
            Self::Kit => "kit",
            Self::Shell => "shell",
            Self::Services => "services",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Kit => "k-",
            Self::Shell => "s-",
            Self::Services => "v-",
        }
    }
}

/// Name prefix for VMs this daemon creates: `marsh-` by default.
/// `MARSH_VM_PREFIX` (e.g. `marsh-xab12c-`) keeps one daemon's names apart
/// from another's, as cloud and the services end-to-end harnesses do.
pub fn vm_prefix() -> &'static str {
    static PREFIX: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PREFIX.get_or_init(|| {
        std::env::var("MARSH_VM_PREFIX")
            .ok()
            .filter(|value| valid_vm_prefix(value))
            .unwrap_or_else(|| "marsh-".to_owned())
    })
}

/// `marsh-` followed by zero or more `x<5 base36>-` segments.
#[must_use]
pub fn valid_vm_prefix(value: &str) -> bool {
    let Some(mut rest) = value.strip_prefix("marsh-") else {
        return false;
    };
    while !rest.is_empty() {
        let bytes = rest.as_bytes();
        if bytes.len() < 7
            || bytes[0] != b'x'
            || bytes[6] != b'-'
            || !bytes[1..6]
                .iter()
                .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
        {
            return false;
        }
        rest = &rest[7..];
    }
    true
}

/// A `marsh --dev` grant an older daemon left in the map: only the names of
/// the VMs its child daemon created (`name -> uuid`, `None` = intent) are
/// read. Every other historical field is accepted and ignored.
#[derive(Default, Deserialize)]
struct LegacyGrant {
    #[serde(default)]
    names: BTreeMap<String, Option<String>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    purpose: String,
    key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    uuid: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnershipFile {
    version: u32,
    vms: BTreeMap<String, Entry>,
    /// Read only, to retire what `marsh --dev` left (`retire_dev_vms`).
    #[serde(default, skip_serializing)]
    grants: BTreeMap<String, LegacyGrant>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyOwnershipFile {
    version: u32,
    vms: BTreeMap<String, String>,
}

#[derive(Default)]
struct State {
    entries: BTreeMap<String, Entry>,
    /// Purpose/key of names removed in this process. A caller still holding
    /// such a name (e.g. the daemon's fixed shell spec) that recreates it
    /// keeps its lookup identity instead of becoming unkeyed.
    retired: BTreeMap<String, Entry>,
}

/// What a failed write must put back: the maps a mutation changes.
type Snapshot = (BTreeMap<String, Entry>, BTreeMap<String, Entry>);

impl State {
    fn snapshot(&self) -> Snapshot {
        (self.entries.clone(), self.retired.clone())
    }

    fn restore(&mut self, (entries, retired): Snapshot) {
        self.entries = entries;
        self.retired = retired;
    }
}

pub(crate) struct VmOwnership {
    path: Option<PathBuf>,
    mirror: Option<PathBuf>,
    state: Mutex<State>,
    view: Mutex<Option<Arc<StockInventory>>>,
    /// Whether the last [`Self::persist`] got as far as renaming the new map
    /// into place. From then on the disk holds the new state whatever a later
    /// step (the directory sync) reports, so a rollback must not restore the
    /// old memory: memory would then disagree with what a restart reads.
    published: AtomicBool,
    /// A write was published but its directory sync failed: the map is on disk
    /// but not known durable. No ownership authority is handed out again until
    /// a later write succeeds in full (see [`Self::make_durable`]).
    undurable: AtomicBool,
}

/// Directories whose syncs fail (tests). Keyed by directory, so a test fails only
/// its own temp directory's map, on whichever thread syncs it (a create may run
/// on a scoped thread), and tests running side by side in one process do not
/// disturb each other.
#[cfg(test)]
static FAILING_SYNC_DIRECTORIES: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// While alive, every directory sync of `directory` fails. Dropped on a panic
/// too, so a failing test cannot leave the failpoint set.
#[cfg(test)]
pub(crate) struct FailDirectorySync(PathBuf);

#[cfg(test)]
impl FailDirectorySync {
    pub(crate) fn in_dir(directory: &Path) -> Self {
        FAILING_SYNC_DIRECTORIES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(directory.to_owned());
        Self(directory.to_owned())
    }
}

#[cfg(test)]
impl Drop for FailDirectorySync {
    fn drop(&mut self) {
        let mut directories = FAILING_SYNC_DIRECTORIES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = directories.iter().position(|d| d == &self.0) {
            directories.remove(index);
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Directory syncs attempted on this thread, and whether to fail them.
    pub(crate) static DIRECTORY_SYNCS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Fail every directory sync once this many have been attempted.
    pub(crate) static FAIL_SYNCS_FROM: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

fn sync_directory(parent: &Path) -> Result<(), SbxError> {
    #[cfg(test)]
    {
        DIRECTORY_SYNCS.with(|count| count.set(count.get() + 1));
        let attempted = DIRECTORY_SYNCS.with(std::cell::Cell::get);
        if FAILING_SYNC_DIRECTORIES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|failing| failing == parent)
            || FAIL_SYNCS_FROM
                .with(std::cell::Cell::get)
                .is_some_and(|from| attempted >= from)
        {
            return Err(std::io::Error::other("injected directory sync failure").into());
        }
    }
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn write_atomically(
    path: &Path,
    bytes: &[u8],
    published: Option<&AtomicBool>,
) -> Result<(), SbxError> {
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let _ = fs::remove_file(&temporary);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    if let Some(published) = published {
        published.store(true, Ordering::Release);
    }
    // The rename itself is only durable once its directory is: without this a
    // power loss can bring back the old map, losing an intent persisted
    // "before create".
    // A bare relative file name has the empty parent: the working directory.
    if let Some(parent) = path.parent() {
        sync_directory(if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        })?;
    }
    Ok(())
}

/// `marsh --dev` was removed. Mark what it left retired, so the retired-VM
/// pass removes it by recorded UUID: each VM a dev grant's child daemon
/// created (under a random grant prefix that an earlier daemon of this map
/// chose and recorded), and the dev shell VM. Returns whether anything
/// changed.
fn retire_dev_vms(
    entries: &mut BTreeMap<String, Entry>,
    grants: BTreeMap<String, LegacyGrant>,
) -> bool {
    let mut changed = !grants.is_empty();
    for (name, uuid) in grants.into_values().flat_map(|grant| grant.names) {
        entries.entry(name).or_insert_with(|| Entry {
            purpose: RETIRED.into(),
            key: String::new(),
            uuid,
        });
    }
    for entry in entries.values_mut() {
        if entry.purpose == VmPurpose::Shell.label() && entry.key == "dev" {
            entry.purpose = RETIRED.into();
            changed = true;
        }
    }
    changed
}

/// Fresh random name: fixed prefix plus 8 lowercase base36 characters.
pub(crate) fn random_vm_name(purpose: VmPurpose) -> String {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut value = u128::from_le_bytes(*uuid::Uuid::new_v4().as_bytes());
    let mut name = format!("{}{}", vm_prefix(), purpose.prefix());
    for _ in 0..8 {
        name.push(char::from(ALPHABET[(value % 36) as usize]));
        value /= 36;
    }
    name
}

impl VmOwnership {
    pub(crate) fn in_memory() -> Self {
        Self {
            path: None,
            mirror: None,
            state: Mutex::new(State::default()),
            view: Mutex::new(None),
            published: AtomicBool::new(false),
            undurable: AtomicBool::new(false),
        }
    }

    /// Load (or start) a persisted owner-only map at `path`.
    pub(crate) fn persisted(path: PathBuf) -> Result<Self, SbxError> {
        let mut grants = BTreeMap::new();
        let mut entries = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.is_file()
                    || metadata.uid() != rustix::process::geteuid().as_raw()
                    || metadata.mode() & 0o077 != 0
                {
                    return Err(SbxError::HostGrantFence(format!(
                        "VM ownership map {} is not an owner-only regular file",
                        path.display()
                    )));
                }
                let bytes = fs::read(&path)?;
                if let Ok(file) = serde_json::from_slice::<OwnershipFile>(&bytes)
                    && file.version == 2
                {
                    grants = file.grants;
                    file.vms
                } else {
                    let legacy: LegacyOwnershipFile = serde_json::from_slice(&bytes)?;
                    if legacy.version != 1 {
                        return Err(SbxError::HostGrantFence(
                            "unsupported VM ownership map version".into(),
                        ));
                    }
                    // Old deterministic names stay owned (removable) but are
                    // never looked up again: new use gets a random name.
                    legacy
                        .vms
                        .into_iter()
                        .map(|(name, uuid)| {
                            (
                                name,
                                Entry {
                                    purpose: UNKEYED.into(),
                                    key: String::new(),
                                    uuid: Some(uuid),
                                },
                            )
                        })
                        .collect()
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error.into()),
        };
        let migrated = retire_dev_vms(&mut entries, grants);
        let ownership = Self {
            path: Some(path),
            mirror: None,
            state: Mutex::new(State {
                entries,
                retired: BTreeMap::new(),
            }),
            view: Mutex::new(None),
            published: AtomicBool::new(false),
            undurable: AtomicBool::new(false),
        };
        if migrated {
            ownership.persist(&ownership.lock())?;
        }
        Ok(ownership)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Copy every write of the map to `mirror` (in an existing owner-only
    /// directory), starting with the current map.
    pub(crate) fn with_mirror(mut self, mirror: PathBuf) -> Result<Self, SbxError> {
        let parent = mirror.parent().unwrap_or(Path::new("/"));
        let metadata = fs::symlink_metadata(parent)?;
        if !metadata.is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
        {
            return Err(SbxError::HostGrantFence(format!(
                "VM ownership mirror directory {} is not an owner-only directory",
                parent.display()
            )));
        }
        self.mirror = Some(mirror);
        {
            let state = self.lock();
            self.persist(&state)?;
        }
        Ok(self)
    }

    fn persist(&self, state: &State) -> Result<(), SbxError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        self.published.store(false, Ordering::Release);
        let bytes = serde_json::to_vec_pretty(&OwnershipFile {
            version: 2,
            vms: state.entries.clone(),
            grants: BTreeMap::new(),
        })?;
        // The mirror is written first: it never names fewer VMs than the map.
        if let Some(mirror) = &self.mirror {
            write_atomically(mirror, &bytes, None)?;
        }
        write_atomically(path, &bytes, Some(&self.published))?;
        self.undurable.store(false, Ordering::Release);
        Ok(())
    }

    /// Whether the map is known durable, finishing a published-but-unsynced
    /// write first. The gate in front of every `sbx create`.
    pub(crate) fn confirm_durable(&self) -> Result<(), SbxError> {
        let state = self.lock();
        self.make_durable(&state)
    }

    /// Before answering from the map, finish a write that was published but
    /// not synced: a retry must not take the failed barrier for durable.
    fn make_durable(&self, state: &State) -> Result<(), SbxError> {
        if self.undurable.load(Ordering::Acquire) {
            self.persist(state)?;
        }
        Ok(())
    }

    /// Undo a mutation whose write failed, unless the write had already
    /// published it: then the disk holds the new state and memory keeps it.
    fn rollback(&self, state: &mut State, before: Snapshot) {
        if self.published.load(Ordering::Acquire) {
            self.undurable.store(true, Ordering::Release);
        } else {
            state.restore(before);
        }
    }

    /// Name for `(purpose, key)`. An existing entry is reused; otherwise a
    /// fresh random name is persisted as an intent before any create.
    pub(crate) fn assign(&self, purpose: VmPurpose, key: &str) -> Result<String, SbxError> {
        let mut state = self.lock();
        self.make_durable(&state)?;
        if let Some(name) = state
            .entries
            .iter()
            .find(|(_, entry)| entry.purpose == purpose.label() && entry.key == key)
            .map(|(name, _)| name.clone())
        {
            return Ok(name);
        }
        let name = loop {
            let candidate = random_vm_name(purpose);
            if !state.entries.contains_key(&candidate) {
                break candidate;
            }
        };
        let before = state.snapshot();
        state.entries.insert(
            name.clone(),
            Entry {
                purpose: purpose.label().into(),
                key: key.into(),
                uuid: None,
            },
        );
        if let Err(error) = self.persist(&state) {
            self.rollback(&mut state, before);
            return Err(error);
        }
        Ok(name)
    }

    pub(crate) fn uuid(&self, name: &str) -> Option<String> {
        self.lock()
            .entries
            .get(name)
            .and_then(|entry| entry.uuid.clone())
    }

    /// Owned names (recorded UUID) whose purpose is Kit or unkeyed legacy.
    pub(crate) fn owned_kit_names(&self) -> Vec<String> {
        self.lock()
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.uuid.is_some()
                    && (entry.purpose == VmPurpose::Kit.label()
                        || entry.purpose == UNKEYED
                        || entry.purpose == RETIRED)
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Persist that `name` is retired for good, before anything else is done
    /// with it: no later lookup returns it, and every daemon removes it.
    pub(crate) fn mark_retired(&self, name: &str) -> Result<String, SbxError> {
        let mut state = self.lock();
        let before = state.snapshot();
        let Some(entry) = state.entries.get_mut(name) else {
            return Err(SbxError::HostGrantFence(format!(
                "{name} is not recorded in the VM ownership map"
            )));
        };
        let prior = std::mem::replace(&mut entry.purpose, RETIRED.into());
        if let Err(error) = self.persist(&state) {
            self.rollback(&mut state, before);
            return Err(error);
        }
        Ok(prior)
    }

    /// Lift a [`Self::mark_retired`] mark (its stop settled): `prior` is the
    /// purpose it returned. On a failed write the VM stays retired.
    pub(crate) fn unmark_retired(&self, name: &str, prior: String) -> Result<(), SbxError> {
        let mut state = self.lock();
        let key = state.entries.get(name).map(|entry| entry.key.clone());
        // A lookup during the stop handed out a new name for the same Kit:
        // that one serves now, and this VM stays retired.
        if state.entries.iter().any(|(other, entry)| {
            other != name && entry.purpose == prior && Some(&entry.key) == key.as_ref()
        }) {
            return Err(SbxError::HostGrantFence(format!(
                "{name} was replaced while it stopped"
            )));
        }
        let before = state.snapshot();
        let Some(entry) = state.entries.get_mut(name) else {
            return Err(SbxError::HostGrantFence(format!(
                "{name} is not recorded in the VM ownership map"
            )));
        };
        entry.purpose = prior;
        if let Err(error) = self.persist(&state) {
            self.rollback(&mut state, before);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn is_retired(&self, name: &str) -> bool {
        self.lock()
            .entries
            .get(name)
            .is_some_and(|entry| entry.purpose == RETIRED)
    }

    /// Every VM marked retired, removed or not yet.
    pub(crate) fn retired_names(&self) -> Vec<String> {
        self.lock()
            .entries
            .iter()
            .filter(|(_, entry)| entry.purpose == RETIRED)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Whether `name` is a shell VM with a recorded UUID.
    pub(crate) fn is_owned_shell(&self, name: &str) -> bool {
        self.lock()
            .entries
            .get(name)
            .is_some_and(|entry| entry.uuid.is_some() && entry.purpose == VmPurpose::Shell.label())
    }

    /// Owned services VMs (recorded UUID) as `(name, project key)`.
    pub(crate) fn owned_services(&self) -> Vec<(String, String)> {
        self.lock()
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.uuid.is_some() && entry.purpose == VmPurpose::Services.label()
            })
            .map(|(name, entry)| (name.clone(), entry.key.clone()))
            .collect()
    }

    /// Persisted shell generations, including older environment selectors.
    pub(crate) fn owned_shell_names(&self) -> Vec<String> {
        self.lock()
            .entries
            .iter()
            .filter(|(_, entry)| entry.uuid.is_some() && entry.purpose == VmPurpose::Shell.label())
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub(crate) fn record(&self, name: &str, uuid: &str) -> Result<(), SbxError> {
        let mut state = self.lock();
        self.make_durable(&state)?;
        let before = state.snapshot();
        let retired = state.retired.remove(name);
        let entry = state.entries.entry(name.to_owned()).or_insert_with(|| {
            retired.unwrap_or_else(|| Entry {
                purpose: UNKEYED.into(),
                key: String::new(),
                uuid: None,
            })
        });
        if entry.uuid.as_deref() == Some(uuid) {
            return Ok(());
        }
        entry.uuid = Some(uuid.to_owned());
        if let Err(error) = self.persist(&state) {
            self.rollback(&mut state, before);
            return Err(error);
        }
        Ok(())
    }

    /// Drop the record of a VM that is gone. An intent that was never adopted
    /// (no UUID) is not a removal: its create may still land, and the intent
    /// is the only authority to clean that VM up, so it stays.
    pub(crate) fn forget(&self, name: &str) -> Result<(), SbxError> {
        let mut state = self.lock();
        if state
            .entries
            .get(name)
            .is_some_and(|entry| entry.uuid.is_none())
        {
            return Ok(());
        }
        let before = state.snapshot();
        if let Some(mut entry) = state.entries.remove(name) {
            entry.uuid = None;
            state.retired.insert(name.to_owned(), entry);
            if let Err(error) = self.persist(&state) {
                self.rollback(&mut state, before);
                return Err(error);
            }
        }
        Ok(())
    }

    /// Persist a just-forgotten name again as an intent (no UUID), before a
    /// replacement VM is created under it.
    pub(crate) fn restore_intent(&self, name: &str) -> Result<(), SbxError> {
        let mut state = self.lock();
        let before = state.snapshot();
        if let Some(entry) = state.retired.remove(name) {
            state.entries.insert(name.to_owned(), entry);
            if let Err(error) = self.persist(&state) {
                self.rollback(&mut state, before);
                return Err(error);
            }
        }
        Ok(())
    }

    /// `(name, purpose, key)` of each entry with a recorded UUID; a legacy
    /// unkeyed entry counts as a Kit VM with no key.
    pub(crate) fn recorded(&self) -> Vec<(String, VmPurpose, String)> {
        self.lock()
            .entries
            .iter()
            .filter(|(_, entry)| entry.uuid.is_some())
            .map(|(name, entry)| {
                let purpose = if entry.purpose == VmPurpose::Shell.label() {
                    VmPurpose::Shell
                } else if entry.purpose == VmPurpose::Services.label() {
                    VmPurpose::Services
                } else {
                    VmPurpose::Kit
                };
                (name.clone(), purpose, entry.key.clone())
            })
            .collect()
    }

    /// The name recorded for `(purpose, key)`, without assigning one.
    pub(crate) fn lookup(&self, purpose: VmPurpose, key: &str) -> Option<String> {
        self.lock()
            .entries
            .iter()
            .find(|(_, entry)| entry.purpose == purpose.label() && entry.key == key)
            .map(|(name, _)| name.clone())
    }

    /// Whether `name` is an intent no VM has been adopted under: its create
    /// may still land, and the intent is what authorizes cleaning that up.
    pub(crate) fn is_unadopted_intent(&self, name: &str) -> bool {
        self.lock()
            .entries
            .get(name)
            .is_some_and(|entry| entry.uuid.is_none())
    }

    pub(crate) fn host_names(&self) -> BTreeSet<String> {
        self.lock().entries.keys().cloned().collect()
    }

    pub(crate) fn cached_view(&self) -> Option<Arc<StockInventory>> {
        self.view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Store a fresh view and reconcile intents against it: adopt any intent
    /// whose random name is present. An absent intent is kept (see the rules).
    pub(crate) fn store_view(
        &self,
        inventory: StockInventory,
    ) -> Result<Arc<StockInventory>, SbxError> {
        {
            let mut state = self.lock();
            let before = state.snapshot();
            let mut changed = false;
            for (name, entry) in &mut state.entries {
                if entry.uuid.is_none()
                    && let Some(vm) = inventory.get(name)
                {
                    entry.uuid = Some(vm.id.clone());
                    changed = true;
                }
            }
            if changed && let Err(error) = self.persist(&state) {
                self.rollback(&mut state, before);
                return Err(error);
            }
        }
        let inventory = Arc::new(inventory);
        *self
            .view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&inventory));
        Ok(inventory)
    }

    /// Drop the cached view after any lifecycle change or failure.
    pub(crate) fn invalidate(&self) {
        *self
            .view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    pub(crate) fn classify(&self, inventory: &StockInventory, name: &str) -> VmClass {
        let recorded = self.uuid(name);
        match inventory.get(name) {
            None => VmClass::Absent,
            Some(vm) if recorded.as_deref() == Some(vm.id.as_str()) => VmClass::Owned {
                uuid: vm.id.clone(),
                // Any state but stopped may be running: a VM caught while it
                // starts or stops is never taken for stopped.
                running: vm.possibly_running(),
            },
            Some(_) => VmClass::Foreign,
        }
    }
}
