//! Automatic Kit updates (`docs/kits.md`): the host record
//! `kit-updates.json`, the daily gate, the opt-out, and the one-line notices.

use marsh_daemon::{DaemonError, KitUpdateCheck, KitUpdateRecord};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fs,
    io::{Read, Write},
    path::Path,
};

pub(crate) const UPDATES_FILE: &str = "kit-updates.json";
/// Key in `$MARSH_HOME/config.json`, written by `marsh config auto-update`.
pub(crate) const CONFIG_KEY: &str = "auto_update";
const CHECK_INTERVAL_MS: u64 = 24 * 60 * 60 * 1000;
const MAX_RECENT: usize = 20;
const MAX_RECORD_BYTES: u64 = 256 * 1024;

/// What the updater remembers between daemon runs.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Record {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check: Option<KitUpdateCheck>,
    /// Kits with a newer image waiting for their jobs to finish.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub waiting: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent: Vec<KitUpdateRecord>,
    /// Known Kits (`builtin_kits`) the user uninstalled: never added again
    /// until `marsh kit install NAME`.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub declined: BTreeSet<String>,
}

impl Record {
    /// A day since the last check. A clock that moved backwards counts as due.
    /// (Each daemon start also checks once; the backend tracks that.)
    pub fn check_due(&self, now_ms: u64) -> bool {
        self.last_check.as_ref().is_none_or(|check| {
            now_ms < check.unix_ms || now_ms - check.unix_ms >= CHECK_INTERVAL_MS
        })
    }

    pub fn record_update(&mut self, update: KitUpdateRecord) {
        self.recent.push(update);
        let excess = self.recent.len().saturating_sub(MAX_RECENT);
        self.recent.drain(..excess);
    }

    /// The notices not shown yet, for one command or (with `None`) all of
    /// them; each is marked shown and never returned again.
    pub fn take_notices(&mut self, command: Option<&str>) -> Vec<String> {
        self.recent
            .iter_mut()
            .filter(|update| !update.notified && command.is_none_or(|name| update.command == name))
            .map(|update| {
                update.notified = true;
                notice(update)
            })
            .collect()
    }

    pub fn has_notices(&self, command: Option<&str>) -> bool {
        self.recent
            .iter()
            .any(|update| !update.notified && command.is_none_or(|name| update.command == name))
    }
}

/// `marsh: claude updated (2.1.305 → 2.1.307)`.
pub(crate) fn notice(update: &KitUpdateRecord) -> String {
    format!("marsh: {}", update.describe())
}

/// Whether automatic updates are on: `MARSH_AUTO_UPDATE` (`0`, `off`, `false`,
/// `no`) at daemon launch, then
/// the scope's `config.json`. Anything unreadable leaves them on.
pub(crate) fn enabled(environment: Option<&OsStr>, scope_root: &Path) -> bool {
    if environment.is_some_and(|value| {
        matches!(
            value.to_string_lossy().trim().to_ascii_lowercase().as_str(),
            "0" | "off" | "false" | "no"
        )
    }) {
        return false;
    }
    let Ok(bytes) = fs::read(scope_root.join("config.json")) else {
        return true;
    };
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|config| config.get(CONFIG_KEY).and_then(serde_json::Value::as_bool))
        .unwrap_or(true)
}

/// The record, or an empty one when missing. An unsafe or unreadable record
/// is replaced on the next write; it holds no authority.
pub(crate) fn read(control_home: &Path) -> Record {
    let path = control_home.join(UPDATES_FILE);
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return Record::default();
    };
    if !metadata.is_file() {
        return Record::default();
    }
    let mut bytes = Vec::new();
    let read = fs::File::open(&path)
        .and_then(|file| file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes));
    if read.is_err() || bytes.len() as u64 > MAX_RECORD_BYTES {
        return Record::default();
    }
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// Atomically replaces the record.
///
/// # Errors
/// Returns a filesystem error.
pub(crate) fn write(control_home: &Path, record: &Record) -> Result<(), DaemonError> {
    let mut bytes = serde_json::to_vec_pretty(record)
        .map_err(|error| DaemonError::InvalidState(error.to_string()))?;
    bytes.push(b'\n');
    let mut temporary = tempfile::NamedTempFile::new_in(control_home)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(control_home.join(UPDATES_FILE))
        .map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 60 * 60 * 1000;

    fn update(command: &str, from: Option<&str>, to: Option<&str>) -> KitUpdateRecord {
        KitUpdateRecord {
            command: command.into(),
            from: format!("docker.io/mcavage/{command}@sha256:{}", "1".repeat(64)),
            to: format!("docker.io/mcavage/{command}@sha256:{}", "2".repeat(64)),
            from_version: from.map(str::to_owned),
            to_version: to.map(str::to_owned),
            unix_ms: 1,
            notified: false,
        }
    }

    #[test]
    fn a_check_is_due_once_a_day() {
        let mut record = Record::default();
        assert!(record.check_due(100 * HOUR));
        record.last_check = Some(KitUpdateCheck {
            unix_ms: 100 * HOUR,
            result: "ok".into(),
            problems: Vec::new(),
        });
        assert!(!record.check_due(100 * HOUR));
        assert!(!record.check_due(123 * HOUR));
        assert!(record.check_due(124 * HOUR));
        assert!(record.check_due(99 * HOUR), "a clock moved backwards");
    }

    #[test]
    fn each_notice_is_returned_exactly_once() {
        let mut record = Record::default();
        record.record_update(update("claude", Some("2.1.305"), Some("2.1.307")));
        record.record_update(update("pix", None, None));
        assert!(record.has_notices(Some("claude")));
        assert_eq!(
            record.take_notices(Some("claude")),
            ["marsh: claude updated (2.1.305 → 2.1.307)"]
        );
        assert_eq!(
            record.take_notices(Some("claude")),
            [] as [std::string::String; 0]
        );
        assert!(!record.has_notices(Some("claude")));
        assert_eq!(
            record.take_notices(None),
            [format!(
                "marsh: pix updated (sha256:{} → sha256:{})",
                "1".repeat(12),
                "2".repeat(12)
            )]
        );
        assert_eq!(record.take_notices(None), [] as [std::string::String; 0]);
        assert!(!record.has_notices(None));
    }

    #[test]
    fn recent_updates_are_bounded_and_the_record_round_trips() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read(root.path()), Record::default());
        let mut record = Record::default();
        for _ in 0..MAX_RECENT + 5 {
            record.record_update(update("pi", None, None));
        }
        assert_eq!(record.recent.len(), MAX_RECENT);
        record.waiting.insert("pi".into());
        write(root.path(), &record).unwrap();
        assert_eq!(read(root.path()), record);
        fs::write(root.path().join(UPDATES_FILE), "{broken").unwrap();
        assert_eq!(read(root.path()), Record::default());
    }

    #[test]
    fn the_environment_or_config_turns_updates_off() {
        let root = tempfile::tempdir().unwrap();
        assert!(enabled(None, root.path()));
        assert!(enabled(Some(OsStr::new("1")), root.path()));
        for off in ["0", "off", "FALSE", "no"] {
            assert!(!enabled(Some(OsStr::new(off)), root.path()), "{off}");
        }
        fs::write(
            root.path().join("config.json"),
            r#"{"shell":"zsh","auto_update":false}"#,
        )
        .unwrap();
        assert!(!enabled(None, root.path()));
        fs::write(root.path().join("config.json"), r#"{"auto_update":true}"#).unwrap();
        assert!(enabled(None, root.path()));
    }
}
