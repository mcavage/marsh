//! The Kit descriptors marsh has read, so a command that worked keeps
//! working when its registry can no longer be read (a private image whose
//! credentials Docker lacks, no network).
//!
//! A descriptor of an immutable Kit generation never changes, so marsh keeps
//! the descriptor itself (`kit-descriptors/`), not what one release read from
//! it: every release parses the saved copy again, and a release that reads
//! more needs no new cache. A read that fails falls back to the image archive
//! on this Mac, then to an older marsh's saved reading (`kit-agent-v*/`), and
//! fails only when no descriptor of that generation was ever seen.

use super::identity_digest;
use marsh_contracts::kit_assets::KitAgent;
use marsh_daemon::{DaemonError, DescriptorStatus};
use marsh_sbx::{NativeKitRef, StockSbx};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

const DESCRIPTORS: &str = "kit-descriptors";
/// Older releases' caches of what they read; only a fallback.
const LEGACY_AGENT_CACHES: [&str; 5] = [
    "kit-agent-v5",
    "kit-agent-v4",
    "kit-agent-v3",
    "kit-agent-v2",
    "kit-agent",
];

#[derive(serde::Deserialize, serde::Serialize)]
struct SavedDescriptor {
    /// The exact Kit generation (`NativeKitRef::identity`).
    kit: String,
    descriptor: serde_json::Value,
}

fn file_name(kit: &NativeKitRef) -> String {
    format!("{}.json", identity_digest(kit.identity()))
}

fn descriptor_path(control_home: &Path, kit: &NativeKitRef) -> PathBuf {
    control_home.join(DESCRIPTORS).join(file_name(kit))
}

/// The saved descriptor of this generation, parsed by this marsh.
pub(crate) fn saved_agent(control_home: &Path, kit: &NativeKitRef) -> Option<KitAgent> {
    let saved: SavedDescriptor =
        serde_json::from_slice(&fs::read(descriptor_path(control_home, kit)).ok()?).ok()?;
    (saved.kit == kit.identity())
        .then(|| KitAgent::from_descriptor(&saved.descriptor).ok())
        .flatten()
}

fn save(control_home: &Path, kit: &NativeKitRef, descriptor: &serde_json::Value) {
    let path = descriptor_path(control_home, kit);
    let store = || -> std::io::Result<()> {
        let directory = path.parent().unwrap_or(&path);
        fs::create_dir_all(directory)?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(
            &mut temporary,
            &SavedDescriptor {
                kit: kit.identity().to_owned(),
                descriptor: descriptor.clone(),
            },
        )?;
        temporary.persist(&path).map_err(|error| error.error)?;
        Ok(())
    };
    let _ = store();
}

/// An older release's reading of this generation: `{"marsh":…,"agent":…}`
/// (v4 and later) or a bare agent (before).
fn legacy_agent(control_home: &Path, kit: &NativeKitRef) -> Option<KitAgent> {
    LEGACY_AGENT_CACHES.iter().find_map(|cache| {
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(control_home.join(cache).join(file_name(kit))).ok()?)
                .ok()?;
        let agent = match value.get("agent") {
            Some(agent) if value.get("marsh").is_some() => agent.clone(),
            _ => value,
        };
        serde_json::from_value(agent).ok()
    })
}

/// Read a Kit generation's agent declarations, and say where they came from.
pub(crate) fn read(
    sbx: &StockSbx,
    control_home: &Path,
    kit: &NativeKitRef,
) -> Result<(KitAgent, DescriptorStatus), DaemonError> {
    let fresh = |error: Option<String>| DescriptorStatus {
        source: "fresh".into(),
        error,
    };
    if let Some(agent) = saved_agent(control_home, kit) {
        return Ok((agent, fresh(None)));
    }
    let error = match sbx.kit_descriptor(kit) {
        Ok((descriptor, done)) => {
            let agent =
                KitAgent::from_descriptor(&descriptor).map_err(DaemonError::InvalidState)?;
            if done {
                save(control_home, kit, &descriptor);
            }
            return Ok((agent, fresh(None)));
        }
        Err(error) => error.to_string(),
    };
    if let Some(descriptor) = sbx.archived_kit_descriptor(kit)
        && let Ok(agent) = KitAgent::from_descriptor(&descriptor)
    {
        save(control_home, kit, &descriptor);
        return Ok((agent, fresh(Some(error))));
    }
    if let Some(agent) = legacy_agent(control_home, kit) {
        return Ok((
            agent,
            DescriptorStatus {
                source: "cached".into(),
                error: Some(error),
            },
        ));
    }
    Err(DaemonError::InvalidState(unreadable(kit, &error)))
}

/// The refusal when no descriptor of a Kit generation was ever seen.
fn unreadable(kit: &NativeKitRef, error: &str) -> String {
    if kit.source_dir().is_some() {
        return error.to_owned();
    }
    let image = kit.identity();
    let lower = error.to_ascii_lowercase();
    let denied = [
        "denied",
        "unauthorized",
        "insufficient_scope",
        "authentication required",
        "forbidden",
    ]
    .iter()
    .any(|word| lower.contains(word));
    if !denied {
        return format!("can't read {image}: {error}");
    }
    let registry = image
        .split_once('/')
        .map(|(host, _)| host)
        .filter(|host| host.contains(['.', ':']))
        .unwrap_or("docker.io");
    format!(
        "can't read {}: registry access denied. Sign in to {registry} with an account that can pull it: `docker login {registry}` (marsh reads Kit descriptors with Docker) and `sbx secret set --registry {registry}` (sbx pulls the Kit). Detail: {}",
        image,
        error.lines().last().unwrap_or(error).trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kit() -> NativeKitRef {
        NativeKitRef::immutable_oci(format!(
            "docker.io/example/private-kit@sha256:{}",
            "a".repeat(64)
        ))
        .unwrap()
    }

    #[test]
    fn a_saved_descriptor_is_parsed_by_every_release() {
        let home = tempfile::tempdir().unwrap();
        let descriptor = serde_json::json!({"capabilities": [{"type": "com.docker.sandbox/agent-sessions@1",
            "config": {"prompt": ["-p", "{{.Prompt}}"]}}]});
        save(home.path(), &kit(), &descriptor);
        assert_eq!(
            saved_agent(home.path(), &kit()),
            Some(KitAgent::from_descriptor(&descriptor).unwrap())
        );
        // Another generation's file name never answers for this one.
        let other = NativeKitRef::immutable_oci(format!(
            "docker.io/example/private-kit@sha256:{}",
            "b".repeat(64)
        ))
        .unwrap();
        fs::copy(
            descriptor_path(home.path(), &kit()),
            descriptor_path(home.path(), &other),
        )
        .unwrap();
        assert_eq!(saved_agent(home.path(), &other), None);
    }

    #[test]
    fn older_releases_readings_are_a_fallback() {
        let home = tempfile::tempdir().unwrap();
        let agent = r#"{"sessions":{"prompt":["-p","{{.Prompt}}"]},"no_home":true}"#;
        for (cache, entry) in [
            (
                "kit-agent-v4",
                format!(r#"{{"marsh":"0.3.9","agent":{agent}}}"#),
            ),
            ("kit-agent-v3", agent.to_owned()),
        ] {
            let directory = home.path().join(cache);
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join(file_name(&kit())), entry).unwrap();
            let found = legacy_agent(home.path(), &kit()).unwrap();
            assert!(found.no_home && found.sessions.is_some(), "{cache}");
            fs::remove_dir_all(directory).unwrap();
        }
        assert!(legacy_agent(home.path(), &kit()).is_none());
    }

    #[test]
    fn an_unreadable_private_image_names_both_sign_ins() {
        let message = unreadable(
            &kit(),
            "stock SBX read Kit descriptor failed (Some(1)): ERROR: pull access denied, repository does not exist or may require authorization: server message: insufficient_scope: authorization failed",
        );
        assert!(
            message.starts_with(&format!(
                "can't read docker.io/example/private-kit@sha256:{}: registry access denied",
                "a".repeat(64)
            )),
            "{message}"
        );
        assert!(
            message.contains("`sbx secret set --registry docker.io`"),
            "{message}"
        );
        assert!(message.contains("`docker login docker.io`"), "{message}");
        let offline = unreadable(&kit(), "network is unreachable");
        assert!(offline.ends_with(": network is unreachable"), "{offline}");
    }
}
