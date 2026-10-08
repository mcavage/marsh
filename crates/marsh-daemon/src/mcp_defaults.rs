//! Default MCP publications: `mcp publish NAME -- 'PIPELINE'` with no
//! `--kit`/`--sandbox`.
//!
//! Rule: a default publication is never loaded into a VM that is already
//! running. The daemon loads each still-current default into a Kit VM only
//! when it has just created that VM (first use, after `workers reset`, or
//! after the VM disappeared), before the VM is cached as ready and before any
//! job container starts there. Loading a running VM would change the stock
//! gateway's tool list for every container already in it.
//!
//! The record lives in this daemon's owner-only control directory. It names
//! the stock server, the declaration it was recorded for, and that
//! declaration's generation. A record is only a hint: before every load the
//! declaration is re-read, and a missing, revoked (pending marker), or
//! regenerated declaration is skipped and pruned, so a host-terminal
//! `marsh mcp unpublish` that bypassed this daemon cannot resurrect a tool.

use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read as _, Write as _},
    os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    sync::Mutex,
};

const FILE: &str = "mcp-defaults.json";
const SCHEMA: &str = "marsh.mcp-defaults/v1";
const MAX_ENTRIES: usize = 256;
const MAX_FILE: u64 = 1024 * 1024;

/// One process writes the file (this daemon); serialize its read-modify-write.
static WRITE: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefaultPublication {
    /// Stock SBX server name (`marsh-pub-KEY12-NAME`).
    pub server: String,
    pub name: String,
    pub declaration: PathBuf,
    pub generation: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    schema: String,
    publications: Vec<DefaultPublication>,
}

fn path(control_home: &Path) -> PathBuf {
    control_home.join(FILE)
}

fn read(control_home: &Path) -> Result<Vec<DefaultPublication>, String> {
    let path = path(control_home);
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("cannot open {}: {error}", path.display())),
    };
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
        || metadata.len() > MAX_FILE
    {
        return Err(format!(
            "{} must be a bounded owner-only file",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    let document: Document = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid {}: {error}", path.display()))?;
    if document.schema != SCHEMA {
        return Err(format!("unsupported {} schema", path.display()));
    }
    Ok(document.publications)
}

fn write(control_home: &Path, publications: Vec<DefaultPublication>) -> Result<(), String> {
    let path = path(control_home);
    if publications.is_empty() {
        return match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("cannot remove {}: {error}", path.display())),
        };
    }
    let bytes = serde_json::to_vec_pretty(&Document {
        schema: SCHEMA.into(),
        publications,
    })
    .map_err(|error| error.to_string())?;
    // The control directory is owner-only and WRITE serializes this daemon's
    // writers, so one fixed temporary name is enough.
    let temporary = control_home.join(format!("{FILE}.tmp"));
    let _ = fs::remove_file(&temporary);
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(&temporary)
        .and_then(|mut file| {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(&bytes)?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&temporary, &path));
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(format!("cannot write {}: {error}", path.display()));
    }
    Ok(())
}

/// Record (or replace) one default publication after its publish committed.
///
/// # Errors
/// Returns an error if the record cannot be read or durably replaced.
pub fn record(control_home: &Path, entry: DefaultPublication) -> Result<(), String> {
    let _guard = WRITE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut publications = read(control_home)?;
    publications.retain(|existing| existing.server != entry.server);
    if publications.len() >= MAX_ENTRIES {
        return Err(format!(
            "at most {MAX_ENTRIES} default MCP publications; unpublish one first"
        ));
    }
    publications.push(entry);
    write(control_home, publications)
}

/// Drop a default publication (unpublish, or a targeted republish).
/// Returns whether a record was removed.
///
/// # Errors
/// Returns an error if the record cannot be read or durably replaced.
pub fn remove(control_home: &Path, server: &str) -> Result<bool, String> {
    let _guard = WRITE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut publications = read(control_home)?;
    let before = publications.len();
    publications.retain(|existing| existing.server != server);
    if publications.len() == before {
        return Ok(false);
    }
    write(control_home, publications).map(|()| true)
}

/// Read a published declaration's generation. `None` means the publication is
/// gone, revoked, or unreadable: never load it.
#[must_use]
pub fn current_generation(declaration: &Path, name: &str) -> Option<String> {
    if fs::symlink_metadata(declaration.with_extension("revoke")).is_ok() {
        return None;
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(declaration)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
        || metadata.len() > MAX_FILE
    {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE).read_to_end(&mut bytes).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    if value.get("tool_name").and_then(serde_json::Value::as_str) != Some(name) {
        return None;
    }
    value
        .get("publication_generation")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// Defaults to load into a Kit VM this daemon has just created. Stale records
/// (declaration gone, revocation pending, or a different generation) are
/// pruned and not returned.
///
/// # Errors
/// Returns an error if the record file is unreadable or cannot be pruned.
pub fn loadable(control_home: &Path) -> Result<Vec<DefaultPublication>, String> {
    let _guard = WRITE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let publications = read(control_home)?;
    let total = publications.len();
    let current: Vec<_> = publications
        .into_iter()
        .filter(|entry| {
            current_generation(&entry.declaration, &entry.name).as_deref()
                == Some(entry.generation.as_str())
        })
        .collect();
    if current.len() != total {
        write(control_home, current.clone())?;
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declaration(dir: &Path, name: &str, generation: &str) -> PathBuf {
        let path = dir.join(format!("{name}.json"));
        fs::write(
            &path,
            serde_json::json!({"tool_name": name, "publication_generation": generation})
                .to_string(),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    fn entry(path: &Path, name: &str, generation: &str) -> DefaultPublication {
        DefaultPublication {
            server: format!("marsh-pub-000000000000-{name}"),
            name: name.into(),
            declaration: path.into(),
            generation: generation.into(),
        }
    }

    #[test]
    fn stale_records_are_never_loadable_and_are_pruned() {
        let root = tempfile::tempdir().unwrap();
        let control = root.path();
        let live = declaration(control, "live", "g1");
        let regenerated = declaration(control, "regen", "g2");
        let revoked = declaration(control, "revoked", "g3");
        fs::write(revoked.with_extension("revoke"), b"").unwrap();
        let gone = control.join("gone.json");
        for entry in [
            entry(&live, "live", "g1"),
            entry(&regenerated, "regen", "old"),
            entry(&revoked, "revoked", "g3"),
            entry(&gone, "gone", "g4"),
        ] {
            record(control, entry).unwrap();
        }
        let loadable = loadable(control).unwrap();
        assert_eq!(loadable, vec![entry(&live, "live", "g1")]);
        assert_eq!(read(control).unwrap(), loadable, "stale records pruned");
        let mode = fs::metadata(path(control)).unwrap().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(remove(control, &loadable[0].server).unwrap());
        assert!(!path(control).exists());
        assert!(!remove(control, &loadable[0].server).unwrap());
    }

    #[test]
    fn republish_replaces_the_record() {
        let root = tempfile::tempdir().unwrap();
        let control = root.path();
        let path = declaration(control, "tool", "g2");
        record(control, entry(&path, "tool", "g1")).unwrap();
        record(control, entry(&path, "tool", "g2")).unwrap();
        assert_eq!(loadable(control).unwrap(), vec![entry(&path, "tool", "g2")]);
    }
}
