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
/// Links in [`LINK_DIR`] (and the shell VM's command directory) that run
/// `marsh open`, unless a registered command has the name.
pub const OPEN_LINKS: [&str; 2] = ["xdg-open", "open"];
/// The static job artifact, bound read-only.
pub const ARTIFACT: &str = "/run/marsh/marsh";
pub const SOCKET: &str = "/run/marsh/cap.sock";
pub const JOB_JSON: &str = "/run/marsh/job.json";
/// The filtered Docker endpoint of a job of a services-enabled project
/// (`docs/design/services.md`, SECURITY-DESIGN A2, A11). It is a path in the
/// per-attempt capability directory; Testcontainers' Ryuk mounts it (DECISIONS 20).
pub const DOCKER_SOCKET: &str = "/run/marsh/docker.sock";
/// The two variables a job of a services-enabled project gets from marsh, and
/// nothing else: they come from [`ServicesEndpoint`], never from the exported
/// environment (which still refuses `DOCKER_*`, SEC-08). DECISIONS 15: Ryuk
/// stays on, so no `TESTCONTAINERS_RYUK_DISABLED`.
pub const ENDPOINT_ENVIRONMENT: [(&str, &str); 2] = [
    ("DOCKER_HOST", "unix:///run/marsh/docker.sock"),
    ("TESTCONTAINERS_HOST_OVERRIDE", "localhost"),
];
/// Open channels per attempt on the endpoint and the forwarded ports
/// together (SECURITY-DESIGN A7); the daemon's relay enforces the same bound.
pub const ENDPOINT_CHANNEL_LIMIT: usize = 64;
/// Chunk and credit window of an endpoint channel over the worker transport:
/// one credit is one chunk of at most [`ENDPOINT_CHUNK`] bytes.
pub const ENDPOINT_CHUNK: usize = 16 * 1024;
pub const ENDPOINT_WINDOW: u32 = 64;
/// Credits are returned in batches of this many chunks.
pub const ENDPOINT_CREDIT_BATCH: u32 = 8;

/// What a channel opened over the worker transport is for.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChannelKind {
    /// A connection to the job's `docker.sock`.
    Docker,
    /// A connection to a forwarded port of the job's own network namespace.
    Tcp { port: u16 },
}

/// The outcome of one forward, from whoever tried to make it (the worker for a
/// job, the guest relay for a shell).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum PortState {
    /// Listening on `127.0.0.1` and `[::1]` (or only on `127.0.0.1` when the
    /// job's namespace has no IPv6) in the job's namespace.
    Forwarding { ipv6: bool },
    /// Something in the job already listens there; nothing was replaced.
    Collision { message: String },
    /// No listener was made anywhere; the reason is plain text.
    Failed { message: String },
    /// The port is no longer published; its listener is closed.
    Revoked,
}

/// What a Docker client reads when the endpoint cannot serve it: a plain HTTP
/// error whose `message` the CLI prints, then EOF.
#[must_use]
pub fn docker_error_response(status: &str, message: &str) -> Vec<u8> {
    let body = serde_json::json!({ "message": message }).to_string();
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// The job's filtered Docker endpoint and port forwards. Present only for a
/// job of a project the user turned services on for (A11); the daemon decides
/// that from its own state, never from the job. The endpoint's environment is
/// [`ENDPOINT_ENVIRONMENT`], added by the runtime beside, not through, the
/// exported environment.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServicesEndpoint {
    /// The daemon's test break modes that concern the worker (an attested
    /// field, DECISIONS 22): always empty in a release daemon.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub breaks: Vec<String>,
}

/// Jobs at once per daemon, all depths, by default (`daemon-jobs`,
/// `MARSH_DAEMON_JOBS`, 1 to [`POOL_MAX`]). Each Kit VM also takes at most
/// [`crate::WORKER_CONTAINER_CAPACITY`] of them.
pub const POOL_LIMIT: usize = 64;
pub const POOL_MAX: usize = 512;
/// Depth of a job tree by default (`tree-depth`, `MARSH_TREE_DEPTH`, 1 to
/// [`DEPTH_MAX`]); the user's command is depth 1.
pub const DEPTH_LIMIT: u32 = 4;
pub const DEPTH_MAX: u32 = 16;
/// Live children of one job by default (`job-children`, `MARSH_JOB_CHILDREN`,
/// 1 to [`FAN_OUT_MAX`]).
pub const FAN_OUT_LIMIT: usize = 16;
pub const FAN_OUT_MAX: usize = 128;
/// Tasks one `task-mcp` server has running at once by default (`tasks-live`,
/// `MARSH_TASKS_LIVE`); finished tasks do not count.
pub const TASKS_LIVE: usize = 16;
pub const TASKS_LIVE_MAX: usize = 128;
/// Launches one job tree may make in a rolling window, by default
/// (`MARSH_TREE_LAUNCH_RATE` changes it at daemon start). Only a tight loop
/// reaches it; a long session that calls tools steadily never does.
pub const TREE_LAUNCH_RATE: LaunchRate = LaunchRate {
    launches: 60,
    window_seconds: 60,
};
/// Refused child calls count toward a tree's launch window only up to this
/// many: a caller retrying refused calls is slowed, not locked out.
pub const REFUSAL_BURST: usize = 8;
/// Distinct Kit VMs one job tree may use at once (`MARSH_TREE_KIT_VMS`
/// or `marsh config limit tree-kit-vms` changes it).
pub const TREE_KIT_VM_LIMIT: usize = 8;
/// A child needs at least this much of its parent's wall time left.
pub const MIN_CHILD_WALL_MS: u64 = 1_000;
/// Connections per attempt on `cap.sock`: the most live children a job
/// can be allowed, plus 4.
pub const CONNECTION_LIMIT: usize = FAN_OUT_MAX + 4;
pub const CONNECTION_REFUSAL: &str = "marsh: too many concurrent daemon requests in this job";
/// The prompt in an `agent-sessions@1` `prompt` verb.
pub const PROMPT_PLACEHOLDER: &str = "{{.Prompt}}";
/// The model in an `org.marsh/agent-model@1` option (marsh's extension,
/// e.g. `["--model", "{{.Model}}"]`), appended after the prompt verbs when a
/// `task` names a model.
pub const MODEL_PLACEHOLDER: &str = "{{.Model}}";
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
    /// Skills and instructions the Kit image declares (`kit_assets`).
    #[serde(
        default,
        skip_serializing_if = "crate::kit_assets::KitAssets::is_empty"
    )]
    pub kit: crate::kit_assets::KitAssets,
    /// Each registered agent's non-interactive prompt verbs
    /// (`agent-sessions@1` `prompt`, e.g. `["-p", "{{.Prompt}}"]`), for
    /// `marsh task-mcp`. A name without them is not offered as an agent.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub prompts: std::collections::BTreeMap<String, Vec<String>>,
    /// Each registered agent's model option (`org.marsh/agent-model@1`,
    /// e.g. `["--model", "{{.Model}}"]`), for `task(model)`. A name without
    /// one refuses `model`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub models: std::collections::BTreeMap<String, Vec<String>>,
    /// The models each agent in `models` can run (`ModelChoice`), for the
    /// `task` tool's description and for `task(model)` family nicknames.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub model_choices: std::collections::BTreeMap<String, Vec<ModelChoice>>,
    /// Each registered agent's usage option (`org.marsh/agent-usage@1`):
    /// the words that make it report token usage and the parser, for the
    /// task tool. A name without one runs as before and records no usage.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub usages: std::collections::BTreeMap<String, crate::usage::UsageOption>,
    /// The peer-review section of `context.md` (`docs/peer-review.md`):
    /// empty unless peer review is on and a ready reviewer is in `spawn`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub peer_review: String,
    /// The filtered Docker endpoint and port forwards (E2), only for a job of
    /// a project with services enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub services: Option<ServicesEndpoint>,
    /// The limits in effect when the job started, for `job.json` (the task
    /// tool reads `tasks_live`); `None` writes the built-in defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Limits>,
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
    /// Registered agents' prompt verbs (`JobCapability::prompts`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub prompts: std::collections::BTreeMap<String, Vec<String>>,
    /// Registered agents' model options (`JobCapability::models`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub models: std::collections::BTreeMap<String, Vec<String>>,
    /// Registered agents' model choices (`JobCapability::model_choices`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub model_choices: std::collections::BTreeMap<String, Vec<ModelChoice>>,
    /// Registered agents' usage options (`JobCapability::usages`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub usages: std::collections::BTreeMap<String, crate::usage::UsageOption>,
}

/// One model an agent can run: the id its model option takes, marsh's class
/// for it (`small`, `mid` or `large`) and the family nickname people use
/// (`luna`, `astra`, `opus`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelChoice {
    pub model: String,
    pub class: String,
    pub family: String,
}

/// A launch rate: at most `launches` in any `window_seconds`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LaunchRate {
    pub launches: usize,
    pub window_seconds: u64,
}

impl LaunchRate {
    /// `N/Ts` (also `N/Tm`, `N/T` in seconds), e.g. `60/60s`: 1 to 10000
    /// launches in 1 s to 1 h.
    ///
    /// # Errors
    /// Returns what is wrong with the text.
    pub fn parse(text: &str) -> Result<Self, String> {
        let invalid = || {
            format!(
                "MARSH_TREE_LAUNCH_RATE must be LAUNCHES/SECONDS (e.g. 60/60s): 1-10000 launches in 1-3600 s, not {text:?}"
            )
        };
        let (launches, window) = text.trim().split_once('/').ok_or_else(invalid)?;
        let (window, unit) = match window.strip_suffix('m') {
            Some(minutes) => (minutes, 60),
            None => (window.strip_suffix('s').unwrap_or(window), 1),
        };
        let number = |value: &str| {
            value
                .bytes()
                .all(|byte| byte.is_ascii_digit())
                .then(|| value.parse::<u64>().ok())
                .flatten()
        };
        let launches = number(launches)
            .filter(|launches| (1..=10_000).contains(launches))
            .ok_or_else(invalid)?;
        let window_seconds = number(window)
            .and_then(|window| window.checked_mul(unit))
            .filter(|window| (1..=3600).contains(window))
            .ok_or_else(invalid)?;
        Ok(Self {
            launches: usize::try_from(launches).map_err(|_| invalid())?,
            window_seconds,
        })
    }

    /// The window in words: `the last minute`, `the last 30 s`.
    #[must_use]
    pub fn window_text(&self) -> String {
        match self.window_seconds {
            60 => "the last minute".into(),
            3600 => "the last hour".into(),
            seconds => format!("the last {seconds} s"),
        }
    }
}

impl std::fmt::Display for LaunchRate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}s", self.launches, self.window_seconds)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Limits {
    pub pool: usize,
    pub depth: u32,
    pub fan_out: usize,
    pub launch_rate: LaunchRate,
    #[serde(default = "default_tasks_live")]
    pub tasks_live: usize,
}

fn default_tasks_live() -> usize {
    TASKS_LIVE
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            pool: POOL_LIMIT,
            depth: DEPTH_LIMIT,
            fan_out: FAN_OUT_LIMIT,
            launch_rate: TREE_LAUNCH_RATE,
            tasks_live: TASKS_LIVE,
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

/// The `context.md` section of a job whose project has Docker on (DECISIONS 28):
/// what the agent may rely on, and what is refused. Added by the worker only
/// when the job has the Docker endpoint.
pub const DOCKER_CONTEXT: &str = "## Docker

This project has its own private Docker, and it is on for this job: `docker` and `docker compose` work here (`DOCKER_HOST` is already set). Containers, images and volumes you create belong to this project, not to the user's Mac.

- Ports a container publishes are reachable on `localhost` from this job.
- Local data is saved with each turn: Docker volumes and git-ignored files (such as `.env`, `.wrangler/state` and SQLite files). It comes along when the user chooses Branch from here, so a branch starts with the data as it was at that turn.
- Containers have no Internet unless the user turns it on (only the user can). Pulling images from Docker Hub and ghcr.io still works.
- Some operations are refused, such as anything that would reach outside this project. A refusal is an error that starts with `marsh:`; read it rather than working around it.
";

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
    fn launch_rates_parse_from_their_variable() {
        let rate = |launches, window_seconds| super::LaunchRate {
            launches,
            window_seconds,
        };
        assert_eq!(super::LaunchRate::parse("60/60s"), Ok(rate(60, 60)));
        assert_eq!(super::LaunchRate::parse("120/2m"), Ok(rate(120, 120)));
        assert_eq!(super::LaunchRate::parse("5/10"), Ok(rate(5, 10)));
        assert_eq!(super::TREE_LAUNCH_RATE.to_string(), "60/60s");
        for bad in [
            "", "60", "0/60s", "60/0s", "60/2h", "-1/60s", "60/3601s", " 6 0/60s",
        ] {
            assert!(super::LaunchRate::parse(bad).is_err(), "{bad:?}");
        }
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
        assert_eq!(
            narrow_spawn(&parent, Some(&parse_spawn("none"))).0,
            [] as [std::string::String; 0]
        );
        assert_eq!(
            narrow_spawn(&[], Some(&wide)).0,
            [] as [std::string::String; 0]
        );
    }
}
