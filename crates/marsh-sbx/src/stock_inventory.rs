//! Validated public `sbx ls --json` inventory decoder.
use crate::{SbxError, safe_absolute};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

const MAX_VMS: usize = 256;
const MAX_DOCUMENT: usize = 4 * 1024 * 1024;

fn invalid(message: &str) -> SbxError {
    SbxError::HostGrantFence(message.into())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct StockInventoryVm {
    pub name: String,
    pub id: String,
    pub status: String,
    pub agent: Option<String>,
    pub workspaces: Option<Vec<PathBuf>>,
}

/// Validated complete inventory data, not source-observation authority. No
/// Deserialize implementation or public constructor bypasses the decoder.
#[derive(Debug, Eq, PartialEq)]
pub struct StockInventory {
    vms: BTreeMap<String, StockInventoryVm>,
}

impl StockInventory {
    /// Decode one complete public `sbx ls --json` document.
    ///
    /// # Errors
    /// Rejects unknown shapes, duplicate name/UUID, unsafe names, unsupported
    /// states, malformed paths/UUIDs and oversized/incomplete inventories.
    pub fn decode(bytes: &[u8]) -> Result<Self, SbxError> {
        const KEYS: &[&str] = &[
            "name",
            "id",
            "agent",
            "status",
            "workspaces",
            // sbx 0.46 marks a VM whose workspace path no longer exists.
            "workspace_missing",
            "last_used_at",
            "created_at",
            "cpus",
            "memory",
            "memory_mib",
        ];
        if bytes.len() > MAX_DOCUMENT {
            return Err(invalid("stock inventory exceeds its byte bound"));
        }
        let document: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|_| invalid("invalid stock inventory JSON"))?;
        let object = document
            .as_object()
            .filter(|object| object.len() == 1 && object.contains_key("sandboxes"))
            .ok_or_else(|| invalid("unknown or incomplete stock inventory envelope"))?;
        let rows = object["sandboxes"]
            .as_array()
            .filter(|rows| rows.len() <= MAX_VMS)
            .ok_or_else(|| invalid("stock inventory has missing or excessive rows"))?;
        let mut vms = BTreeMap::new();
        let mut ids = BTreeSet::new();
        for row in rows {
            if row
                .as_object()
                .is_none_or(|row| row.keys().any(|key| !KEYS.contains(&key.as_str())))
            {
                return Err(invalid("unknown stock inventory row schema"));
            }
            let mut vm: StockInventoryVm = serde_json::from_value(row.clone())
                .map_err(|_| invalid("incomplete stock inventory row"))?;
            vm.id.make_ascii_lowercase();
            if !valid_name(&vm.name)
                || !valid_uuid(&vm.id)
                || !matches!(vm.status.as_str(), "running" | "stopped")
                || vm
                    .workspaces
                    .as_ref()
                    .is_some_and(|paths| paths.iter().any(|path| !metadata_path(path)))
                || !ids.insert(vm.id.clone())
                || vms.contains_key(&vm.name)
            {
                return Err(invalid(
                    "ambiguous, changing or unsupported stock inventory identity/state",
                ));
            }
            vms.insert(vm.name.clone(), vm);
        }
        Ok(Self { vms })
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&StockInventoryVm> {
        self.vms.get(name)
    }

    #[must_use]
    pub fn proves_names_absent(&self, names: &BTreeSet<String>) -> bool {
        names.iter().all(|name| !self.vms.contains_key(name))
    }

    #[must_use]
    pub fn contains_uuid(&self, uuid: &str) -> bool {
        self.vms.values().any(|vm| vm.id.eq_ignore_ascii_case(uuid))
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
        && id
            .bytes()
            .any(|byte| matches!(byte, b'1'..=b'9' | b'a'..=b'f' | b'A'..=b'F'))
}

fn metadata_path(path: &Path) -> bool {
    // Go JSON replaces invalid Unix UTF8. A replacement spelling cannot prove
    // identity when raw and Unicode-replacement filenames coexist on the host.
    path.as_os_str().len() <= 4096
        && path.components().count() <= 128
        && (safe_absolute(path) || path == Path::new("/"))
        && path.to_str().is_some_and(|path| !path.contains('\u{fffd}'))
}

#[cfg(test)]
mod tests {
    use super::StockInventory;

    #[test]
    fn a_foreign_vm_with_a_missing_workspace_does_not_block_inventory() {
        // Row shape from stock sbx 0.46 for a user's VM whose workspace was deleted.
        let listing = br#"{"sandboxes":[{"name":"user-agent-project","id":"0F1E2D3C-4B5A-4978-8695-A4B3C2D1E0F9","agent":"claude","status":"running","last_used_at":"2026-10-05T16:27:18Z","workspaces":["/Users/someone/dev/project","/tmp/deleted"],"workspace_missing":true,"created_at":"2026-10-05T16:27:00Z"}]}"#;
        let inventory = StockInventory::decode(listing).unwrap();
        assert!(inventory.contains_uuid("0f1e2d3c-4b5a-4978-8695-a4b3c2d1e0f9"));
        let unknown = br#"{"sandboxes":[{"name":"user-agent-project","id":"0f1e2d3c-4b5a-4978-8695-a4b3c2d1e0f9","status":"running","surprise":1}]}"#;
        assert!(StockInventory::decode(unknown).is_err());
    }
}
