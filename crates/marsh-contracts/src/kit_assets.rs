//! The agent skills and instructions a Kit's descriptor declares, delivered to
//! nested jobs (`docs/design/processes.md` s9).
//!
//! Stock SBX applies `agent-skill@1`, `agent-skills@1` and `agent-context@*`
//! in the outer sandbox container; marsh jobs run in a nested container from
//! the same image and would otherwise never see them. A kit set's build has
//! already composed its components' entries into one descriptor (the
//! `vnd.docker.sandbox.kit.descriptor` manifest annotation), with content
//! files at absolute paths inside the image. marsh only delivers that result:
//! instruction bodies are appended to `/run/marsh/context.md`, and each skills
//! destination is a read-only directory of links to the bundled skills.

use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// The manifest annotation that carries a v3 Kit's descriptor.
pub const DESCRIPTOR_ANNOTATION: &str = "vnd.docker.sandbox.kit.descriptor";

const MAX_DESTINATIONS: usize = 8;
const MAX_SKILLS: usize = 256;
const MAX_CONTEXTS: usize = 16;
/// Bytes of one inline instruction body or file read for `context.md`.
pub const MAX_CONTEXT_BYTES: usize = 64 * 1024;

/// What one Kit image asks marsh to deliver into its jobs.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KitAssets {
    /// `agent-skills@1` paths: where the Kit's agent reads skills.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_destinations: Vec<PathBuf>,
    /// `agent-skill@1` entries, one per effective name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<KitSkill>,
    /// `agent-context` bodies, in declaration order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contexts: Vec<KitContext>,
}

/// One bundled skill: a directory in the image, linked as `name`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KitSkill {
    pub name: String,
    pub path: PathBuf,
}

/// One instruction body: inline text, or a file inside the image.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum KitContext {
    Text(String),
    File(PathBuf),
}

impl KitAssets {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skill_destinations.is_empty() && self.skills.is_empty() && self.contexts.is_empty()
    }

    /// Reads the assets from a descriptor (the annotation's JSON).
    /// Capabilities marsh does not deliver are ignored; malformed entries for
    /// the three it does are refused.
    ///
    /// # Errors
    /// Returns a description of the first invalid entry.
    pub fn from_descriptor(descriptor: &str) -> Result<Self, String> {
        let value: serde_json::Value = serde_json::from_str(descriptor)
            .map_err(|error| format!("Kit descriptor is not JSON: {error}"))?;
        let mut assets = Self::default();
        let Some(capabilities) = value.get("capabilities") else {
            return Ok(assets);
        };
        let capabilities = capabilities
            .as_array()
            .ok_or("Kit descriptor capabilities is not a list")?;
        for (index, capability) in capabilities.iter().enumerate() {
            let kind = capability.get("type").and_then(|kind| kind.as_str());
            let config = capability.get("config");
            let field = |name: &str| {
                config
                    .and_then(|config| config.get(name))
                    .and_then(|value| value.as_str())
            };
            let invalid = |what: &str| format!("Kit capability {index} ({kind:?}): {what}");
            match kind {
                Some("com.docker.sandbox/agent-skills@1") => {
                    let path = field("path").ok_or_else(|| invalid("missing path"))?;
                    let path = PathBuf::from(path);
                    if !skill_destination_shape(&path) {
                        return Err(invalid("unsupported skills path"));
                    }
                    if !assets.skill_destinations.contains(&path) {
                        assets.skill_destinations.push(path);
                    }
                }
                Some("com.docker.sandbox/agent-skill@1") => {
                    let path = field("path").ok_or_else(|| invalid("missing path"))?;
                    let path = PathBuf::from(path);
                    if !canonical_absolute(&path) {
                        return Err(invalid("skill path is not absolute and canonical"));
                    }
                    let name = match field("name") {
                        Some(name) => name.to_owned(),
                        None => path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or_default()
                            .to_owned(),
                    };
                    if !valid_skill_name(&name) {
                        return Err(invalid("invalid skill name"));
                    }
                    if !assets.skills.iter().any(|skill| skill.name == name) {
                        assets.skills.push(KitSkill { name, path });
                    }
                }
                Some(
                    "com.docker.sandbox/agent-context@1" | "com.docker.sandbox/agent-context@2",
                ) => {
                    if let Some(text) = field("content") {
                        if text.len() > MAX_CONTEXT_BYTES {
                            return Err(invalid("context is too large"));
                        }
                        assets.contexts.push(KitContext::Text(text.to_owned()));
                    } else if let Some(file) = field("contentFile") {
                        let file = PathBuf::from(file);
                        if !canonical_absolute(&file) {
                            return Err(invalid("contentFile is not absolute and canonical"));
                        }
                        assets.contexts.push(KitContext::File(file));
                    } else {
                        return Err(invalid("missing content"));
                    }
                }
                _ => {}
            }
        }
        if assets.skill_destinations.len() > MAX_DESTINATIONS
            || assets.skills.len() > MAX_SKILLS
            || assets.contexts.len() > MAX_CONTEXTS
        {
            return Err("Kit declares too many skills or instructions".into());
        }
        Ok(assets)
    }

    /// The skills destinations a job can receive: those that neither contain
    /// nor sit inside one of its host mounts (a bind there would hide a
    /// grant, or create a mountpoint in the user's files).
    pub fn deliverable_destinations<'a>(
        &'a self,
        mount_targets: &'a [&Path],
    ) -> impl Iterator<Item = (usize, &'a Path)> + 'a {
        self.skill_destinations
            .iter()
            .enumerate()
            .filter(move |(_, destination)| {
                !mount_targets.iter().any(|target| {
                    destination.starts_with(target) || target.starts_with(destination)
                })
            })
            .map(|(index, destination)| (index, destination.as_path()))
    }
}

fn canonical_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path != Path::new("/")
        && path.to_str().is_some_and(|text| !text.ends_with('/'))
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

/// A skills destination marsh may bind: canonical, and outside the paths the
/// runtime and the job capability own.
fn skill_destination_shape(path: &Path) -> bool {
    canonical_absolute(path)
        && ![
            "/run", "/var/run", "/proc", "/sys", "/dev", "/etc", "/usr", "/bin", "/sbin", "/lib",
            "/tmp",
        ]
        .iter()
        .any(|reserved| {
            let reserved = Path::new(reserved);
            path.starts_with(reserved) || reserved.starts_with(path)
        })
}

fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !matches!(name, "." | "..")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SET: &str = r#"{"schemaVersion":"3","kind":"workload","capabilities":[
        {"type":"com.docker.sandbox/sbx@1"},
        {"type":"com.docker.sandbox/agent-skills@1","config":{"path":"/opt/pix/kit-skills"}},
        {"type":"com.docker.sandbox/agent-skill@1","config":{"path":"/opt/docker/skills/hello"}},
        {"type":"com.docker.sandbox/agent-skill@1","optional":true,
         "config":{"path":"/opt/docker/skills/x","name":"renamed"}},
        {"type":"com.docker.sandbox/agent-context@1",
         "config":{"contentFile":"/usr/share/sandbox/kit/set/context.md"}},
        {"type":"com.docker.sandbox/agent-context@2","config":{"content":"inline"}}
    ]}"#;

    #[test]
    fn reads_a_composed_set() {
        let assets = KitAssets::from_descriptor(SET).unwrap();
        assert_eq!(
            assets.skill_destinations,
            [PathBuf::from("/opt/pix/kit-skills")]
        );
        assert_eq!(
            assets.skills,
            [
                KitSkill {
                    name: "hello".into(),
                    path: "/opt/docker/skills/hello".into()
                },
                KitSkill {
                    name: "renamed".into(),
                    path: "/opt/docker/skills/x".into()
                },
            ]
        );
        assert_eq!(
            assets.contexts,
            [
                KitContext::File("/usr/share/sandbox/kit/set/context.md".into()),
                KitContext::Text("inline".into()),
            ]
        );
    }

    #[test]
    fn a_descriptor_without_these_capabilities_is_empty() {
        assert!(
            KitAssets::from_descriptor(r#"{"kind":"workload"}"#)
                .unwrap()
                .is_empty()
        );
        assert!(
            KitAssets::from_descriptor(
                r#"{"capabilities":[{"type":"com.docker.sandbox/network@1","config":{}}]}"#
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn refuses_unsafe_entries() {
        for capability in [
            r#"{"type":"com.docker.sandbox/agent-skills@1","config":{"path":"/run/marsh/x"}}"#,
            r#"{"type":"com.docker.sandbox/agent-skills@1","config":{"path":"/"}}"#,
            r#"{"type":"com.docker.sandbox/agent-skills@1","config":{"path":"/opt/../run"}}"#,
            r#"{"type":"com.docker.sandbox/agent-skills@1","config":{"path":"relative"}}"#,
            r#"{"type":"com.docker.sandbox/agent-skill@1","config":{"path":"/opt/a/.."}}"#,
            r#"{"type":"com.docker.sandbox/agent-skill@1","config":{"path":"/opt/a","name":"../x"}}"#,
            r#"{"type":"com.docker.sandbox/agent-context@1","config":{"contentFile":"./c.md"}}"#,
            r#"{"type":"com.docker.sandbox/agent-context@1","config":{}}"#,
        ] {
            let descriptor = format!(r#"{{"capabilities":[{capability}]}}"#);
            assert!(
                KitAssets::from_descriptor(&descriptor).is_err(),
                "{capability}"
            );
        }
        assert!(KitAssets::from_descriptor("not json").is_err());
    }

    #[test]
    fn skips_destinations_that_overlap_host_mounts() {
        let assets = KitAssets {
            skill_destinations: vec![
                "/opt/pix/kit-skills".into(),
                "/Users/me/.pi/skills".into(),
                "/Users".into(),
            ],
            ..KitAssets::default()
        };
        let mounts = [Path::new("/Users/me"), Path::new("/Users/me/project")];
        let kept: Vec<_> = assets.deliverable_destinations(&mounts).collect();
        assert_eq!(kept, [(0, Path::new("/opt/pix/kit-skills"))]);
    }
}
