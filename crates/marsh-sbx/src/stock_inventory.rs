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
            // sbx after v0.47.0 adds a boolean `detached` to some rows; marsh ignores it.
            "detached",
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
        // Stock SBX has emitted both `{"sandboxes": [...]}` and a bare `[...]`;
        // any other envelope is unknown.
        let rows = match &document {
            serde_json::Value::Array(_) => &document,
            serde_json::Value::Object(object)
                if object.len() == 1 && object.contains_key("sandboxes") =>
            {
                &object["sandboxes"]
            }
            _ => return Err(invalid("unknown or incomplete stock inventory envelope")),
        };
        let rows = rows
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
    //! Rejections; every captured or derived listing decoding is covered by
    //! `tests::stock_cli` against `tests/data/sbx`.
    use super::StockInventory;

    const V047: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/sbx/v0.47.0/ls-json.stdout"
    ));
    const V047_DEV: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/sbx/v0.47-dev/ls-json.stdout"
    ));

    /// The first row of a real listing with one unknown key added.
    fn with_unknown_key(listing: &[u8]) -> Vec<u8> {
        let mut document: serde_json::Value = serde_json::from_slice(listing).unwrap();
        let rows = match document.get_mut("sandboxes") {
            Some(rows) => rows,
            None => &mut document,
        };
        rows[0]["surprise"] = serde_json::json!(1);
        serde_json::to_vec(&document).unwrap()
    }

    #[test]
    fn a_row_with_an_unknown_key_is_rejected_in_either_envelope() {
        for listing in [V047, V047_DEV] {
            assert!(StockInventory::decode(listing).is_ok());
            assert!(StockInventory::decode(&with_unknown_key(listing)).is_err());
        }
    }

    #[test]
    fn unknown_envelopes_are_rejected_and_an_empty_listing_is_not() {
        assert!(StockInventory::decode(b"[]").is_ok());
        assert!(StockInventory::decode(br#"{"sandboxes":[]}"#).is_ok());
        for envelope in [
            &br#"{"sandboxes":[],"next":null}"#[..],
            br#"{"vms":[]}"#,
            br#""sandboxes""#,
        ] {
            assert!(StockInventory::decode(envelope).is_err());
        }
    }

    #[test]
    fn duplicate_identities_are_rejected() {
        let row = r#"{"name":"a","id":"0f1e2d3c-4b5a-4978-8695-a4b3c2d1e0f9","status":"running"}"#;
        let renamed = row.replace("\"a\"", "\"b\"");
        assert!(StockInventory::decode(format!("[{row},{renamed}]").as_bytes()).is_err());
        let rekeyed = row.replace("0f1e2d3c", "1f1e2d3c");
        assert!(StockInventory::decode(format!("[{row},{rekeyed}]").as_bytes()).is_err());
    }
}
