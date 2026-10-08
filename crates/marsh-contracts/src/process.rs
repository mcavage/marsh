//! Nested processes (`docs/design/processes.md`): the per-job capability a worker
//! publishes at `/run/marsh`, the limits, the context text, and the two pure
//! decisions every caller shares: the link's entry rule and spawn-set
//! narrowing.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Where the job capability is bound in every Kit job container.
pub const CAPABILITY_DIR: &str = "/run/marsh";
/// The per-name links (`#!/run/marsh/marsh --link=NAME`).
pub const LINK_DIR: &str = "/run/marsh/bin";
/// The static job artifact, bound read-only.
pub const ARTIFACT: &str = "/run/marsh/marsh";
pub const SOCKET: &str = "/run/marsh/cap.sock";
pub const JOB_JSON: &str = "/run/marsh/job.json";

pub const POOL_LIMIT: usize = 8;
pub const DEPTH_LIMIT: u32 = 4;
pub const SAME_KIT_LIMIT: usize = 2;
pub const FAN_OUT_LIMIT: usize = 4;
pub const TOTAL_LIMIT: usize = 64;
/// Distinct Kit VMs one job tree may use at once (`MARSH_TREE_KIT_VMS`
/// lowers or raises it at daemon start).
pub const TREE_KIT_VM_LIMIT: usize = 4;
/// A child needs at least this much of its parent's wall time left.
pub const MIN_CHILD_WALL_MS: u64 = 1_000;
/// Connections per attempt on `cap.sock`: `FAN_OUT_LIMIT + 4`.
pub const CONNECTION_LIMIT: usize = FAN_OUT_LIMIT + 4;
pub const CONNECTION_REFUSAL: &str = "marsh: too many concurrent daemon requests in this job";
pub const TTY_REFUSAL: &str = "marsh: interactive child jobs are not supported yet";

/// What the daemon asks the worker to publish for one job.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JobCapability {
    pub job: String,
    /// The job's own registered command name.
    pub name: String,
    /// Names this job may start; empty is `--no-spawn` (the socket stays
    /// for splits and `jobs`; admission refuses every spawn).
    pub spawn: Vec<String>,
    /// Every registered name: each gets a link (a name outside `spawn` is
    /// refused by its link).
    pub registered: Vec<String>,
    /// Variables this job received from its parent (or the root shell):
    /// they are forwarded to its children whatever their value (s6).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forwarded: Vec<String>,
    /// The daemon's environment key (hex). The worker uses it to write
    /// salted digests into `job.json` and never publishes it to the job.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub env_key: String,
}

/// `/run/marsh/job.json`, written by the worker (docs/design/processes.md s4).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct JobDocument {
    pub version: u32,
    pub job: String,
    pub name: String,
    pub spawn: Vec<String>,
    pub registered: Vec<String>,
    /// `/run/marsh/cap.sock`.
    pub socket: PathBuf,
    /// The PATH the container started with (links first).
    pub path: String,
    pub limits: Limits,
    /// Starting environment: name -> HMAC-SHA256 of the name and value
    /// under the daemon's environment key (`env_digest`), which the job does
    /// not hold; `""` for variables Docker sets (never forwarded).
    pub env: std::collections::BTreeMap<String, String>,
    /// Variables received from the parent: always forwarded to children.
    #[serde(default)]
    pub forwarded: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Limits {
    pub pool: usize,
    pub depth: u32,
    pub same_kit: usize,
    pub fan_out: usize,
    pub total: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            pool: POOL_LIMIT,
            depth: DEPTH_LIMIT,
            same_kit: SAME_KIT_LIMIT,
            fan_out: FAN_OUT_LIMIT,
            total: TOTAL_LIMIT,
        }
    }
}

/// The text of `marsh context` outside a job (no spawn set known).
pub const CONTEXT: &str = include_str!("context.md");

/// The text of `marsh context` and `/run/marsh/context.md` for one job: the
/// shared text (`context.md`) with the job's name and spawn set filled in.
/// Agent Kits hand this file to their agent as extra system instructions
/// (`docs/agents.md`); marsh never writes it into a project or a home.
#[must_use]
pub fn job_context(name: &str, spawn: &[String]) -> String {
    let who = if name.is_empty() {
        "an marsh job".to_owned()
    } else {
        format!("the marsh job `{name}`")
    };
    let names = if spawn.is_empty() {
        "none: this job was started with `--no-spawn` (or `MARSH_SPAWN=none`), so \
         every spawn is refused; `marsh jobs` still works"
            .to_owned()
    } else {
        spawn
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    CONTEXT
        .replacen("an marsh job", &who, 1)
        .replacen(SPAWN_PLACEHOLDER, &names, 1)
}

/// The spawn-set sentence in `context.md` that `job_context` fills in.
const SPAWN_PLACEHOLDER: &str = "every registered command (`ls /run/marsh/bin` in a job)";

/// Lowercase hex of `bytes` (`env_key`, digests).
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// SHA-256 of `bytes` as lowercase hex.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    hex(&sha2::Sha256::digest(bytes))
}

/// `job.json`'s digest of one starting variable: HMAC-SHA256 (RFC 2104)
/// keyed by the daemon's environment key over `NAME=VALUE`. Without the key
/// the job cannot test guesses of a value against it.
#[must_use]
pub fn env_digest(key: &[u8], name: &str, value: &[u8]) -> String {
    hex(&hmac_sha256(key, &[name.as_bytes(), b"=", value]))
}

fn hmac_sha256(key: &[u8], data: &[&[u8]]) -> [u8; 32] {
    use sha2::Digest as _;
    let mut block = [0u8; 64];
    if key.len() > block.len() {
        block[..32].copy_from_slice(&sha2::Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = sha2::Sha256::new().chain_update(block.map(|b| b ^ 0x36));
    for part in data {
        inner.update(part);
    }
    sha2::Sha256::new()
        .chain_update(block.map(|b| b ^ 0x5c))
        .chain_update(inner.finalize())
        .finalize()
        .into()
}

/// Hex decoding of `env_key`.
#[must_use]
pub fn decode_hex(text: &str) -> Option<Vec<u8>> {
    text.len()
        .is_multiple_of(2)
        .then(|| {
            (0..text.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
                .collect()
        })
        .flatten()
}

/// What a job offers its child (s6): every forwardable variable except the
/// ones Docker set (`""` in `job.json`). Returns the variables and, for those
/// still named in the starting environment and not received from the
/// parent, their starting digests; the daemon drops a variable whose value
/// still matches its digest (unchanged image `ENV`).
#[must_use]
pub fn job_offer(
    document: &JobDocument,
    variables: impl IntoIterator<Item = (String, Vec<u8>)>,
) -> (
    std::collections::BTreeMap<String, Vec<u8>>,
    std::collections::BTreeMap<String, String>,
) {
    let mut offered = std::collections::BTreeMap::new();
    let mut start = std::collections::BTreeMap::new();
    for (name, value) in variables {
        let inherited = document.forwarded.contains(&name);
        match document.env.get(&name) {
            Some(digest) if digest.is_empty() && !inherited => continue,
            Some(digest) if !inherited => {
                start.insert(name.clone(), digest.clone());
            }
            _ => {}
        }
        offered.insert(name, value);
    }
    (offered, start)
}

/// The daemon's half of `job_offer`: drop each variable whose value matches
/// its starting digest under `key`.
pub fn retain_changed(
    environment: &mut std::collections::BTreeMap<String, Vec<u8>>,
    start: &std::collections::BTreeMap<String, String>,
    key: &[u8],
) {
    environment.retain(|name, value| {
        start
            .get(name)
            .is_none_or(|digest| *digest != env_digest(key, name, value))
    });
}

/// The link's decision for name `name` (docs/design/processes.md s5).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LinkDecision {
    /// The entrypoint reached its own name: exec this image binary once.
    Entry(PathBuf),
    /// Another name the image provides: exec it in this container.
    ImageTool(PathBuf),
    /// A child job.
    Child,
}

/// Search `path` for an executable `name`, never in the link directory.
/// `resolve` returns a candidate's canonical path when it is an executable
/// file; a candidate that resolves into `/run/marsh` (a symlink to a link
/// or the artifact) is skipped, so a link can never exec itself.
pub fn image_binary(
    name: &str,
    path: &str,
    resolve: impl Fn(&Path) -> Option<PathBuf>,
) -> Option<PathBuf> {
    path.split(':')
        .filter(|entry| !entry.is_empty() && Path::new(entry) != Path::new(LINK_DIR))
        .map(|entry| Path::new(entry).join(name))
        .find(|candidate| resolve(candidate).is_some_and(|real| !real.starts_with(CAPABILITY_DIR)))
}

/// The entry rule: the own name with the marker and a docker-init parent
/// resolves to the image binary; another image-provided name stays local;
/// everything else is a child job.
pub fn link_decision(
    name: &str,
    own: &str,
    entry_marker: bool,
    parent_is_init: bool,
    path: &str,
    resolve: impl Fn(&Path) -> Option<PathBuf>,
) -> LinkDecision {
    let found = image_binary(name, path, resolve);
    if name == own {
        match found {
            Some(binary) if entry_marker && parent_is_init => LinkDecision::Entry(binary),
            _ => LinkDecision::Child,
        }
    } else {
        found.map_or(LinkDecision::Child, LinkDecision::ImageTool)
    }
}

/// A child's spawn set: the parent's set narrowed by a request; never wider.
/// Returns the set and the requested names that were dropped.
#[must_use]
pub fn narrow_spawn(parent: &[String], requested: Option<&[String]>) -> (Vec<String>, Vec<String>) {
    let Some(requested) = requested else {
        return (parent.to_vec(), Vec::new());
    };
    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for name in requested {
        if parent.contains(name) {
            if !kept.contains(name) {
                kept.push(name.clone());
            }
        } else if name != "none" {
            dropped.push(name.clone());
        }
    }
    kept.sort();
    (kept, dropped)
}

/// Parse `MARSH_SPAWN` / `--spawn`: comma list; `none` is the empty set.
#[must_use]
pub fn parse_spawn(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty() && *name != "none")
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn hmac_matches_rfc_4231() {
        // RFC 4231 test case 2.
        assert_eq!(
            super::hex(&super::hmac_sha256(
                b"Jefe",
                &[b"what do ya want ", b"for nothing?"]
            )),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn a_child_gets_received_and_changed_variables_only() {
        let key = [7u8; 32];
        let digest = |name: &str, value: &str| super::env_digest(&key, name, value.as_bytes());
        let document = super::JobDocument {
            env: [
                ("IMAGE", digest("IMAGE", "img")),
                ("CHANGED", digest("CHANGED", "old")),
                ("FROM_PARENT", digest("FROM_PARENT", "1")),
                ("HOSTNAME", String::new()),
            ]
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
            forwarded: vec!["FROM_PARENT".into()],
            ..super::JobDocument::default()
        };
        let variables = [
            ("IMAGE", "img"),
            ("CHANGED", "new"),
            ("FROM_PARENT", "1"),
            ("HOSTNAME", "abc"),
            ("NEW", "x"),
        ]
        .map(|(name, value)| (name.to_owned(), value.as_bytes().to_vec()));
        let (mut offered, start) = super::job_offer(&document, variables);
        assert!(!start.contains_key("FROM_PARENT"));
        super::retain_changed(&mut offered, &start, &key);
        assert_eq!(
            offered.keys().map(String::as_str).collect::<Vec<_>>(),
            ["CHANGED", "FROM_PARENT", "NEW"]
        );
        // Without the key, the digests prove nothing about a value.
        assert_ne!(
            super::env_digest(&[0; 32], "IMAGE", b"img"),
            digest("IMAGE", "img")
        );
    }

    use super::*;

    const PATH: &str = "/run/marsh/bin:/usr/local/bin:/usr/bin";

    fn image(path: &Path) -> Option<PathBuf> {
        matches!(
            path.to_str(),
            Some("/usr/local/bin/fixture" | "/usr/bin/node")
        )
        .then(|| path.to_path_buf())
    }

    #[test]
    fn entry_needs_own_name_marker_and_init_parent() {
        let entry = LinkDecision::Entry("/usr/local/bin/fixture".into());
        assert_eq!(
            link_decision("fixture", "fixture", true, true, PATH, image),
            entry
        );
        // linkIgnoresMarker / linkIgnoresPpid controls: either guard alone is a child.
        assert_eq!(
            link_decision("fixture", "fixture", false, true, PATH, image),
            LinkDecision::Child
        );
        assert_eq!(
            link_decision("fixture", "fixture", true, false, PATH, image),
            LinkDecision::Child
        );
        // The link directory is never searched, and a symlink resolving
        // into /run/marsh is skipped, so the link cannot exec itself.
        let aliased = |path: &Path| {
            (path == Path::new("/usr/local/bin/fixture")).then(|| PathBuf::from(ARTIFACT))
        };
        assert_eq!(
            link_decision("fixture", "fixture", true, true, PATH, aliased),
            LinkDecision::Child
        );
        let links = |path: &Path| path.starts_with(LINK_DIR).then(|| path.to_path_buf());
        assert_eq!(
            link_decision("fixture", "fixture", true, true, PATH, links),
            LinkDecision::Child
        );
    }

    #[test]
    fn other_names_stay_local_only_when_the_image_ships_them() {
        assert_eq!(
            link_decision("node", "fixture", true, true, PATH, image),
            LinkDecision::ImageTool("/usr/bin/node".into())
        );
        assert_eq!(
            link_decision("codex", "fixture", true, true, PATH, image),
            LinkDecision::Child
        );
    }

    #[test]
    fn spawn_sets_only_narrow() {
        let parent = vec!["claude".to_owned(), "fixture".to_owned()];
        assert_eq!(narrow_spawn(&parent, None).0, parent);
        let wide = vec!["fixture".to_owned(), "shell".to_owned()];
        assert_eq!(
            narrow_spawn(&parent, Some(&wide)),
            (vec!["fixture".to_owned()], vec!["shell".to_owned()])
        );
        assert!(
            narrow_spawn(&parent, Some(&parse_spawn("none")))
                .0
                .is_empty()
        );
        assert!(narrow_spawn(&[], Some(&wide)).0.is_empty());
    }
}
