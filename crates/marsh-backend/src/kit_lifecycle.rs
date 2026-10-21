//! Decisions for `marsh kit install`, `upgrade` and `uninstall` that do not
//! touch stock SBX: which tag a Kit follows, and the host `kit-sources.json`
//! record of tags installed with `marsh kit install`.

use marsh_contracts::{OciImage, command_registry::CommandName, kit_source::KitTag};
use marsh_daemon::DaemonError;
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::Path,
};

pub(crate) const SOURCES_FILE: &str = "kit-sources.json";
const MAX_SOURCES_BYTES: u64 = 256 * 1024;

/// Tags followed by Kits installed with `marsh kit install NAME REPO:TAG`.
/// Missing means none. The command registry, not this file, says what runs.
///
/// # Errors
/// Rejects an unsafe, oversized or invalid file rather than guessing.
pub(crate) fn read_sources(control_home: &Path) -> Result<BTreeMap<String, KitTag>, DaemonError> {
    let path = control_home.join(SOURCES_FILE);
    let invalid = |reason: &str| DaemonError::InvalidState(format!("{}: {reason}", path.display()));
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() {
        return Err(invalid("not a regular file"));
    }
    let mut bytes = Vec::new();
    fs::File::open(&path)?
        .take(MAX_SOURCES_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SOURCES_BYTES {
        return Err(invalid("too large"));
    }
    let raw: BTreeMap<String, String> =
        serde_json::from_slice(&bytes).map_err(|_| invalid("invalid JSON object"))?;
    raw.into_iter()
        .map(|(command, tag)| {
            CommandName::parse(command.clone()).map_err(|_| invalid("invalid command name"))?;
            let tag = KitTag::parse(&tag).map_err(|_| invalid("invalid tag"))?;
            Ok((command, tag))
        })
        .collect()
}

/// Atomically replaces the record (or removes it when empty).
///
/// # Errors
/// Returns a filesystem error.
pub(crate) fn write_sources(
    control_home: &Path,
    sources: &BTreeMap<String, KitTag>,
) -> Result<(), DaemonError> {
    let path = control_home.join(SOURCES_FILE);
    if sources.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    } else {
        let rendered: BTreeMap<&str, String> = sources
            .iter()
            .map(|(command, tag)| (command.as_str(), tag.to_string()))
            .collect();
        let mut bytes = serde_json::to_vec_pretty(&rendered)
            .map_err(|error| DaemonError::InvalidState(error.to_string()))?;
        bytes.push(b'\n');
        let mut temporary = tempfile::NamedTempFile::new_in(control_home)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(&path).map_err(|error| error.error)?;
    }
    fs::File::open(control_home)?.sync_all()?;
    Ok(())
}

/// What a Kit follows on upgrade: its recorded tag, or for a packaged Kit the
/// release channel's tag of its pinned repository. `Err` is the reason to skip.
pub(crate) fn upgrade_source(
    recorded: Option<&KitTag>,
    packaged: Option<&OciImage>,
    channel: Option<&str>,
) -> Result<KitTag, &'static str> {
    if let Some(tag) = recorded {
        return Ok(tag.clone());
    }
    let Some(pinned) = packaged else {
        return Err("installed by digest; reinstall it from a tag to follow updates");
    };
    let Some(channel) = channel else {
        return Err(
            "this marsh is not a release build, so packaged Kits have no channel to follow",
        );
    };
    KitTag::for_repository(pinned, channel)
        .map_err(|_| "its packaged reference has no repository to follow")
}

/// What `marsh kit uninstall NAME` does to the registry.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Uninstall {
    /// Drop the name entirely.
    Remove,
    /// Drop the override and run the packaged Kit again.
    RevertToPackaged,
}

/// # Errors
/// Explains why a name cannot be uninstalled.
pub(crate) fn plan_uninstall(
    command: &str,
    registered: bool,
    packaged: bool,
    overridden: bool,
) -> Result<Uninstall, String> {
    match (registered, packaged, overridden) {
        (false, _, _) => Err(format!("{command} is not a registered command")),
        (true, true, true) => Ok(Uninstall::RevertToPackaged),
        (true, true, false) => Err(format!(
            "{command} is a packaged command at the version this marsh shipped; there is nothing to uninstall"
        )),
        (true, false, _) => Ok(Uninstall::Remove),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:3333333333333333333333333333333333333333333333333333333333333333";

    fn control() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn sources_round_trip_and_an_empty_record_removes_the_file() {
        let root = control();
        assert!(read_sources(root.path()).unwrap().is_empty());
        let sources = BTreeMap::from([(
            "pix".to_owned(),
            KitTag::parse("mcavage/marsh-pix:stable").unwrap(),
        )]);
        write_sources(root.path(), &sources).unwrap();
        assert_eq!(read_sources(root.path()).unwrap(), sources);
        assert!(
            fs::read_to_string(root.path().join(SOURCES_FILE))
                .unwrap()
                .contains("docker.io/mcavage/marsh-pix:stable")
        );
        write_sources(root.path(), &BTreeMap::new()).unwrap();
        assert!(!root.path().join(SOURCES_FILE).exists());
    }

    #[test]
    fn invalid_or_unsafe_records_are_refused() {
        for document in [
            "{broken",
            r#"["docker.io/a/b:c"]"#,
            r#"{"pix":"docker.io/a/b"}"#,
            &format!(r#"{{"pix":"docker.io/a/b@{DIGEST}"}}"#),
            r#"{"-bad":"docker.io/a/b:c"}"#,
        ] {
            let root = control();
            fs::write(root.path().join(SOURCES_FILE), document).unwrap();
            assert!(read_sources(root.path()).is_err(), "{document}");
        }
        let root = control();
        std::os::unix::fs::symlink("/etc/hosts", root.path().join(SOURCES_FILE)).unwrap();
        assert!(read_sources(root.path()).is_err());
    }

    #[test]
    fn a_recorded_tag_wins_and_packaged_kits_follow_their_channel() {
        let tag = KitTag::parse("mcavage/marsh-pix:stable").unwrap();
        let pinned = OciImage::parse(format!("docker.io/mcavage/marsh-pi@{DIGEST}")).unwrap();
        assert_eq!(upgrade_source(Some(&tag), None, None).unwrap(), tag);
        assert_eq!(
            upgrade_source(None, Some(&pinned), Some("latest"))
                .unwrap()
                .to_string(),
            "docker.io/mcavage/marsh-pi:latest"
        );
        assert!(
            upgrade_source(None, Some(&pinned), None)
                .unwrap_err()
                .contains("not a release build")
        );
        assert!(
            upgrade_source(None, None, Some("latest"))
                .unwrap_err()
                .contains("installed by digest")
        );
    }

    #[test]
    fn packaged_names_revert_only_when_overridden_and_others_are_removed() {
        assert_eq!(
            plan_uninstall("pix", true, false, true),
            Ok(Uninstall::Remove)
        );
        assert_eq!(
            plan_uninstall("pi", true, true, true),
            Ok(Uninstall::RevertToPackaged)
        );
        assert!(
            plan_uninstall("pi", true, true, false)
                .unwrap_err()
                .contains("nothing to uninstall")
        );
        assert!(plan_uninstall("nope", false, false, false).is_err());
    }
}
