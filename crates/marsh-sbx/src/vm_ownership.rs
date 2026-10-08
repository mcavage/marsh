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
//!   appears in inventory is adopted; an intent loaded at startup whose name
//!   is absent from the first inventory is dropped.
//! - A VM is usable only if its name exists with the recorded UUID. A name
//!   present with a different UUID, or not in the map, is foreign: refuse.
//! - Stop/remove only ever target a recorded UUID that is currently present.
//!   Removal drops the entry, so the next use gets a new random name.
//!
//! The inventory view is refreshed (one `sbx ls --json`) only on cold start,
//! after a lifecycle change, or after a failure. Warm requests use the cache.

use super::{SbxError, StockInventory};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
    sync::{Arc, Mutex},
};

/// Purpose recorded for VMs created by a daemon before purposes existed or
/// recorded without a lookup key.
const UNKEYED: &str = "unkeyed";

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
}

impl VmPurpose {
    fn label(self) -> &'static str {
        match self {
            Self::Kit => "kit",
            Self::Shell => "shell",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Self::Kit => "k-",
            Self::Shell => "s-",
        }
    }
}

/// Name prefix for VMs this daemon creates: `marsh-` by default. A daemon
/// running inside a dev session gets its grant prefix via `MARSH_VM_PREFIX`
/// (e.g. `marsh-xab12c-`), so the host broker can confine its names.
pub fn vm_prefix() -> &'static str {
    static PREFIX: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PREFIX.get_or_init(|| {
        std::env::var("MARSH_VM_PREFIX")
            .ok()
            .filter(|value| valid_vm_prefix(value))
            .unwrap_or_else(|| "marsh-".to_owned())
    })
}

/// `marsh-` followed by zero or more `x<5 base36>-` grant segments.
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

/// A development grant persisted beside the host VM map: the child daemon's
/// names (`name -> uuid`, `None` = intent), its roots and capacity.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevGrantRecord {
    pub session: String,
    pub prefix: String,
    pub roots: Vec<PathBuf>,
    pub scratch: PathBuf,
    pub max_vms: usize,
    pub revoked: bool,
    #[serde(default)]
    pub cleanup_uncertain: bool,
    pub names: BTreeMap<String, Option<String>>,
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    grants: BTreeMap<String, DevGrantRecord>,
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
    /// Intents loaded from disk that no in-process caller has claimed yet.
    unclaimed_intents: BTreeSet<String>,
    /// Purpose/key of names removed in this process. A caller still holding
    /// such a name (e.g. the daemon's fixed shell spec) that recreates it
    /// keeps its lookup identity instead of becoming unkeyed.
    retired: BTreeMap<String, Entry>,
    grants: BTreeMap<String, DevGrantRecord>,
}

pub(crate) struct VmOwnership {
    path: Option<PathBuf>,
    state: Mutex<State>,
    view: Mutex<Option<Arc<StockInventory>>>,
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
            state: Mutex::new(State::default()),
            view: Mutex::new(None),
        }
    }

    /// Load (or start) a persisted owner-only map at `path`.
    pub(crate) fn persisted(path: PathBuf) -> Result<Self, SbxError> {
        let mut grants = BTreeMap::new();
        let entries = match fs::symlink_metadata(&path) {
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
        let unclaimed_intents = entries
            .iter()
            .filter(|(_, entry)| entry.uuid.is_none())
            .map(|(name, _)| name.clone())
            .collect();
        Ok(Self {
            path: Some(path),
            state: Mutex::new(State {
                entries,
                unclaimed_intents,
                retired: BTreeMap::new(),
                grants,
            }),
            view: Mutex::new(None),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn persist(&self, state: &State) -> Result<(), SbxError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let bytes = serde_json::to_vec_pretty(&OwnershipFile {
            version: 2,
            vms: state.entries.clone(),
            grants: state.grants.clone(),
        })?;
        let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
        let _ = fs::remove_file(&temporary);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    }

    /// Name for `(purpose, key)`. An existing entry is reused; otherwise a
    /// fresh random name is persisted as an intent before any create.
    pub(crate) fn assign(&self, purpose: VmPurpose, key: &str) -> Result<String, SbxError> {
        let mut state = self.lock();
        if let Some(name) = state
            .entries
            .iter()
            .find(|(_, entry)| entry.purpose == purpose.label() && entry.key == key)
            .map(|(name, _)| name.clone())
        {
            state.unclaimed_intents.remove(&name);
            return Ok(name);
        }
        let name = loop {
            let candidate = random_vm_name(purpose);
            if !state.entries.contains_key(&candidate) {
                break candidate;
            }
        };
        state.entries.insert(
            name.clone(),
            Entry {
                purpose: purpose.label().into(),
                key: key.into(),
                uuid: None,
            },
        );
        if let Err(error) = self.persist(&state) {
            state.entries.remove(&name);
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
                    && (entry.purpose == VmPurpose::Kit.label() || entry.purpose == UNKEYED)
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub(crate) fn record(&self, name: &str, uuid: &str) -> Result<(), SbxError> {
        let mut state = self.lock();
        state.unclaimed_intents.remove(name);
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
        self.persist(&state)
    }

    pub(crate) fn forget(&self, name: &str) -> Result<(), SbxError> {
        let mut state = self.lock();
        state.unclaimed_intents.remove(name);
        if let Some(mut entry) = state.entries.remove(name) {
            self.persist(&state)?;
            entry.uuid = None;
            state.retired.insert(name.to_owned(), entry);
        }
        Ok(())
    }

    pub(crate) fn dev_grants(&self) -> BTreeMap<String, DevGrantRecord> {
        self.lock().grants.clone()
    }

    /// Mutate the persisted grant section; a failed write rolls back.
    pub(crate) fn update_dev_grants<R>(
        &self,
        update: impl FnOnce(&mut BTreeMap<String, DevGrantRecord>) -> R,
    ) -> Result<R, SbxError> {
        let mut state = self.lock();
        let prior = state.grants.clone();
        let result = update(&mut state.grants);
        if state.grants != prior
            && let Err(error) = self.persist(&state)
        {
            state.grants = prior;
            return Err(error);
        }
        Ok(result)
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
    /// whose random name is present; drop startup intents whose name is absent.
    pub(crate) fn store_view(
        &self,
        inventory: StockInventory,
    ) -> Result<Arc<StockInventory>, SbxError> {
        {
            let mut state = self.lock();
            let mut changed = false;
            let unclaimed = std::mem::take(&mut state.unclaimed_intents);
            for name in unclaimed {
                if inventory.get(&name).is_none() {
                    state.entries.remove(&name);
                    changed = true;
                }
            }
            for (name, entry) in &mut state.entries {
                if entry.uuid.is_none()
                    && let Some(vm) = inventory.get(name)
                {
                    entry.uuid = Some(vm.id.clone());
                    changed = true;
                }
            }
            // Grant intents are adopted by name like host intents (random
            // per-grant prefix; children reach stock only via the broker).
            for grant in state.grants.values_mut() {
                for (name, uuid) in &mut grant.names {
                    if uuid.is_none()
                        && let Some(vm) = inventory.get(name)
                    {
                        *uuid = Some(vm.id.clone());
                        changed = true;
                    }
                }
            }
            if changed {
                self.persist(&state)?;
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
                running: vm.status == "running",
            },
            Some(_) => VmClass::Foreign,
        }
    }
}
