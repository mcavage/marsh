//! Restore a snapshot into a fork, through the filter (S4).
//!
//! Design: ENG-PLAN D3 (`restore`), SECURITY-DESIGN A2 (the compose client runs
//! unprivileged on the agent's own filtered socket, with the lane workspace as
//! its authority), A4 (restore creates objects in the lane's scope only), A8
//! (snapshot and volume ids are opaque; labels and paths from the manifest are
//! never filesystem paths) and A9 (lane files are written by the daemon on the
//! Mac, never by this agent).
//!
//! Restore is two phases, so the Mac daemon can write the lane files between
//! them:
//!
//! 1. **prepare** (`restore_prepare`): read the snapshot's manifest, read each
//!    stack's compose configuration through the confined client, refuse stacks
//!    with fork-unsafe features (per stack), create the fork's volumes through
//!    the filter (so they are in the lane's scope and ledger) and install the
//!    snapshot's data into each new volume (a reflink clone into a sibling of
//!    `_data`, swapped in atomically beneath held descriptors). It returns the
//!    *contents* of `compose.fork.yaml` and `.env` for each stack, the remapped
//!    ports, and what was and was not restored. Nothing is written to the lane.
//! 2. **up** (`restore_up`): the daemon has written those files. The agent
//!    checks that they are the bytes it returned, runs `docker compose up -d
//!    --wait` through the filter, then runs the fixed post-restore check
//!    (containers running, and healthy where a healthcheck exists).
//!
//! The compose client is never root and never sees the production socket: it is
//! this binary re-executed as `restore-client`, which opens the lane folder
//! from the agent's descriptor, confines itself with Landlock (read-only lane,
//! read-only system files, a private scratch directory, no TCP), drops to the
//! lane folder's owner (or `nobody`), and execs `docker compose` with
//! `DOCKER_HOST` set to a socket only this restore serves. The socket is served
//! by E3's endpoint with `Authority { project: lane_dir, workspace, attempt:
//! "restore" }`, so every engine call is policed like a job's.
// The agent only runs on Linux (inside the engine container); on other hosts
// the Linux-only half is compiled out and what remains is not used.
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;

/// Where each restore makes its private run directory (a tmpfs inside the engine
/// container): the filtered socket, and the compose client's home and temp.
pub const RUN_ROOT: &str = "/run/marsh-restore";
/// The compose project name every fork's objects use is the host's `fork`; it
/// has to be a valid compose project name.
const MAX_FORK_BYTES: usize = 48;
const MAX_PATH_BYTES: usize = 4096;
const MAX_COMPONENT_BYTES: usize = 255;
const MAX_ENV_BYTES: u64 = 128 * 1024;
const MAX_TEXT: usize = 160;
const MAX_STACKS: usize = 32;
const MAX_SERVICES: usize = 128;
const MAX_GAPS: usize = 64;
/// The overlay file's name: it is the one name the daemon writes beside the
/// project's own compose files.
pub const OVERRIDE_NAME: &str = "compose.fork.yaml";
pub const ENV_NAME: &str = ".env";

#[derive(Debug)]
pub enum RestoreError {
    /// The request is invalid (a bad id, path or name).
    Invalid(String),
    /// Restore could not run; the message is for the caller.
    Failed(String),
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(why) | Self::Failed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for RestoreError {}

impl From<std::io::Error> for RestoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Failed(error.to_string())
    }
}

// ---------------------------------------------------------------- validation

/// The fork's compose project name: lowercase letters, digits, `-` and `_`,
/// starting with a letter or digit.
#[must_use]
pub fn valid_fork(fork: &str) -> bool {
    valid_project_name(fork) && fork.len() <= MAX_FORK_BYTES
}

/// Compose's rule for project names.
#[must_use]
pub fn valid_project_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 255
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
}

/// A compose service, volume or network key (also what the override file may
/// name): `[A-Za-z0-9][A-Za-z0-9._-]*`.
#[must_use]
pub fn valid_key(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_COMPONENT_BYTES
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The components of an absolute path made only of ordinary names: no `.`, `..`,
/// empty component, NUL or control character.
///
/// # Errors
/// The reason the path is refused.
pub fn components(path: &str) -> Result<Vec<String>, String> {
    if path.len() > MAX_PATH_BYTES {
        return Err("path is too long".into());
    }
    let Some(rest) = path.strip_prefix('/') else {
        return Err("path must be absolute".into());
    };
    let mut out = Vec::new();
    for part in rest.split('/') {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.len() > MAX_COMPONENT_BYTES
            || part.chars().any(char::is_control)
        {
            return Err(format!("path has an unusable component {part:?}"));
        }
        out.push(part.to_owned());
    }
    Ok(out)
}

/// The part of `lane_dir` below `project`: at least one component. The lane
/// folder must be strictly inside the project root, by name; the agent then
/// resolves those names beneath its held project descriptor.
///
/// # Errors
/// The reason the pair is refused.
pub fn lane_components(project: &str, lane_dir: &str) -> Result<Vec<String>, String> {
    let root = components(project).map_err(|why| format!("project: {why}"))?;
    let lane = components(lane_dir).map_err(|why| format!("lane_dir: {why}"))?;
    if lane.len() <= root.len() || lane[..root.len()] != root[..] {
        return Err("lane_dir must be inside the project root".into());
    }
    Ok(lane[root.len()..].to_vec())
}

/// A relative path of ordinary components (as `a/b`), or `""`.
fn relative_ok(path: &str) -> bool {
    path.is_empty()
        || path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part.len() <= MAX_COMPONENT_BYTES
                && !part.chars().any(char::is_control)
        })
}

/// A file name that may appear in `COMPOSE_FILE` and `-f`: one relative path,
/// no `:` (the list separator) and no leading `-`.
fn compose_file_ok(path: &str) -> bool {
    !path.is_empty() && !path.contains(':') && !path.starts_with('-') && relative_ok(path)
}

fn text(value: &str, max: usize) -> String {
    crate::capture::sanitize(value, max)
}

// ---------------------------------------------------------------- generated files

/// A double-quoted YAML scalar. JSON strings are YAML 1.2 scalars, so the JSON
/// serializer is the quoting function: nothing is spliced into the file.
fn quoted(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

/// One published port of a service, as the override file will state it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PortSpec {
    pub target: u16,
    pub protocol: String,
    /// `None`: the engine chooses (no host port was pinned).
    pub published: Option<u16>,
    pub host_ip: String,
}

/// `compose.fork.yaml`: replaces the ports of each listed service (`!override`)
/// and names the project. Built from validated keys and numbers only.
#[must_use]
pub fn override_yaml(project: &str, services: &[(String, Vec<PortSpec>)]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "name: {}", quoted(project));
    if services.is_empty() {
        out.push_str("services: {}\n");
        return out;
    }
    out.push_str("services:\n");
    for (name, ports) in services {
        let _ = writeln!(out, "  {}:", quoted(name));
        out.push_str("    ports: !override\n");
        for port in ports {
            let _ = writeln!(out, "      - target: {}", port.target);
            let _ = writeln!(out, "        protocol: {}", quoted(&port.protocol));
            if let Some(published) = port.published {
                let _ = writeln!(out, "        published: {}", quoted(&published.to_string()));
                let _ = writeln!(out, "        host_ip: {}", quoted(&port.host_ip));
            }
        }
    }
    out
}

fn host_boundary_before(line: &str, at: usize) -> bool {
    line[..at]
        .chars()
        .next_back()
        .is_none_or(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))
}

/// Rewrites `localhost:<port>` and `127.0.0.1:<port>` in one line for the ports
/// in `map` (original to fork). Other text is untouched.
#[must_use]
pub fn rewrite_ports(line: &str, map: &BTreeMap<u16, u16>) -> String {
    let mut out = String::with_capacity(line.len());
    let mut index = 0;
    while index < line.len() {
        let rest = &line[index..];
        let host = ["localhost:", "127.0.0.1:"]
            .into_iter()
            .find(|host| rest.starts_with(host) && host_boundary_before(line, index));
        if let Some(host) = host {
            let digits_at = index + host.len();
            let digits: String = line[digits_at..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            let after = line[digits_at + digits.len()..].chars().next();
            let boundary = after.is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'));
            if let Some(new) = digits
                .parse::<u16>()
                .ok()
                .filter(|_| boundary)
                .and_then(|old| map.get(&old))
            {
                out.push_str(host);
                out.push_str(&new.to_string());
                index = digits_at + digits.len();
                continue;
            }
        }
        let c = rest.chars().next().unwrap_or(' ');
        out.push(c);
        index += c.len_utf8();
    }
    out
}

/// The lane's `.env`: the user's own lines with localhost ports rewritten, any
/// old `COMPOSE_PROJECT_NAME` and `COMPOSE_FILE` dropped, and the two lines
/// that make `docker compose` in the lane use the fork. `project` and `files`
/// are `None` only under the deliberately broken `restore-no-override`.
#[must_use]
pub fn env_content(original: &str, map: &BTreeMap<u16, u16>, project: &str, files: &str) -> String {
    let mut out = String::with_capacity(original.len() + 128);
    for line in original.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("COMPOSE_PROJECT_NAME=") || trimmed.starts_with("COMPOSE_FILE=") {
            continue;
        }
        out.push_str(&rewrite_ports(line, map));
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    let _ = writeln!(out, "COMPOSE_PROJECT_NAME={project}");
    let _ = writeln!(out, "COMPOSE_FILE={files}");
    out
}

// ---------------------------------------------------------------- manifest (read side)

#[derive(Debug, Deserialize)]
struct SnapVolume {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    captured: bool,
    #[serde(default)]
    files: u64,
    #[serde(default)]
    labels: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct SnapContainer {
    #[serde(default)]
    name: String,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    volumes: Vec<String>,
    #[serde(default)]
    testcontainers: bool,
}

#[derive(Debug, Deserialize)]
struct SnapStack {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    project: String,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    config_files: Vec<String>,
    #[serde(default)]
    containers: Vec<SnapContainer>,
}

#[derive(Debug, Deserialize)]
struct SnapManifest {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    volumes: Vec<SnapVolume>,
    #[serde(default)]
    stacks: Vec<SnapStack>,
    #[serde(default)]
    standalone: Vec<SnapContainer>,
    #[serde(default)]
    gaps: Vec<String>,
}

/// What blocks forking a compose project, from `docker compose config --format
/// json`: names the engine would collide on, and volumes or networks that are
/// not the project's own. Each entry names the object.
#[must_use]
pub fn blockers(config: &serde_json::Value, project: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(services) = config["services"].as_object() {
        for (name, service) in services {
            if let Some(container) = service["container_name"].as_str() {
                out.push(format!(
                    "service '{}' sets container_name '{}'",
                    text(name, 80),
                    text(container, 80)
                ));
            }
            for port in service["ports"].as_array().into_iter().flatten() {
                if port["published"].as_str().is_some_and(|p| p.contains('-')) {
                    out.push(format!(
                        "service '{}' publishes a port range",
                        text(name, 80)
                    ));
                    break;
                }
            }
        }
    }
    for (kind, collection) in [("volume", "volumes"), ("network", "networks")] {
        for (key, entry) in config[collection].as_object().into_iter().flatten() {
            let actual = entry["name"].as_str().unwrap_or("");
            if entry["external"].as_bool() == Some(true) || entry["external"].as_object().is_some()
            {
                out.push(format!(
                    "{kind} '{}' is external ({})",
                    text(key, 80),
                    text(if actual.is_empty() { key } else { actual }, 80)
                ));
            } else if !actual.is_empty() && actual != format!("{project}_{key}") {
                out.push(format!(
                    "{kind} '{}' has an explicit name '{}'",
                    text(key, 80),
                    text(actual, 80)
                ));
            }
        }
    }
    out
}

/// The published ports of every service in a compose configuration.
fn service_ports(config: &serde_json::Value) -> Vec<(String, Vec<PortSpec>)> {
    let mut out = Vec::new();
    for (name, service) in config["services"].as_object().into_iter().flatten() {
        let mut ports = Vec::new();
        for port in service["ports"].as_array().into_iter().flatten() {
            let Some(target) = port["target"].as_u64().and_then(|t| u16::try_from(t).ok()) else {
                continue;
            };
            let published = port["published"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| port["published"].as_u64().map(|p| p.to_string()))
                .and_then(|p| p.parse::<u16>().ok());
            let host_ip = port["host_ip"]
                .as_str()
                .filter(|ip| {
                    !ip.is_empty()
                        && ip.len() <= 45
                        && ip
                            .chars()
                            .all(|c| c.is_ascii_hexdigit() || matches!(c, '.' | ':'))
                })
                .unwrap_or("127.0.0.1")
                .to_owned();
            let protocol = match port["protocol"].as_str() {
                Some("udp") => "udp",
                _ => "tcp",
            };
            ports.push(PortSpec {
                target,
                protocol: protocol.to_owned(),
                published,
                host_ip,
            });
        }
        if !ports.is_empty() {
            out.push((name.clone(), ports));
        }
    }
    out
}

#[cfg(target_os = "linux")]
pub use linux::{DropReport, PrepareRequest, RestoreConfig, Restorer, UpRequest, client_main};

/// `restore-client` is only meaningful on Linux (inside the engine container).
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn client_main(_args: &[String]) -> i32 {
    eprintln!("marsh: the restore client requires Linux");
    2
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{
        ENV_NAME, MAX_ENV_BYTES, MAX_GAPS, MAX_SERVICES, MAX_STACKS, MAX_TEXT, OVERRIDE_NAME,
        PortSpec, RUN_ROOT, RestoreError, SnapManifest, SnapStack, SnapVolume, blockers,
        components, compose_file_ok, env_content, lane_components, override_yaml, relative_ok,
        service_ports, text, valid_fork, valid_key, valid_project_name,
    };
    use crate::breaks::{break_mode, hold};
    use crate::capture::valid_snapshot_id;
    use crate::engine_api::{ContainerSummary, Engine, SCOPE_LABEL, percent_encode};
    use crate::frame::{Authority, validate_label};
    use crate::fsops::{self, Copier, Pool, VolumeCopy};
    use crate::locks::ScopeLocks;
    use crate::relay::DockerEndpoint;
    use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::fmt::Write as _;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::fd::{AsFd, AsRawFd, OwnedFd};
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::os::unix::net::UnixListener;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const DOCKER: &str = "/usr/bin/docker";
    const MAX_CLIENT_STDOUT: usize = 2 << 20;
    const MAX_CLIENT_STDERR: usize = 64 << 10;
    const MAX_REPLY_BYTES: usize = 900 * 1024;
    /// Unprivileged fallback when the lane folder is owned by root.
    const NOBODY: u32 = 65534;
    /// Ports reserved for a prepared restore are forgotten after this long.
    const RESERVE_FOR: Duration = Duration::from_mins(10);
    /// A drop waits this long for a restore or capture of the workspace to end.
    const DROP_LOCK_WAIT: Duration = Duration::from_secs(60);
    const DROP_WORKERS: usize = 6;
    const MAX_DROP_OBJECTS: usize = 512;

    /// What `drop_workspace` removed, and what is still there.
    #[derive(Debug, Default)]
    pub struct DropReport {
        pub containers: u32,
        pub volumes: u32,
        pub networks: u32,
        pub left: Vec<String>,
    }

    #[derive(Clone, Debug)]
    pub struct RestoreConfig {
        /// Longest a restore waits for the workspace's operation lock.
        pub lock_wait: Duration,
        /// A `compose config` or version call.
        pub config_timeout: Duration,
        /// `compose up --wait` and `down`.
        pub up_timeout: Duration,
        pub workers: usize,
        pub run_root: PathBuf,
    }

    impl Default for RestoreConfig {
        fn default() -> Self {
            Self {
                lock_wait: Duration::from_millis(500),
                config_timeout: Duration::from_secs(30),
                up_timeout: Duration::from_secs(150),
                workers: 4,
                run_root: PathBuf::from(RUN_ROOT),
            }
        }
    }

    // ------------------------------------------------------------ run directory

    /// A private run directory with the filtered socket of one restore phase.
    /// Dropped (socket closed, directory removed) when the phase ends.
    struct RunDir {
        path: PathBuf,
        stop: Arc<AtomicBool>,
        accept: Option<std::thread::JoinHandle<()>>,
    }

    impl RunDir {
        fn create(
            root: &Path,
            uid: u32,
            gid: u32,
            authority: &Authority,
            endpoint: &Arc<dyn DockerEndpoint>,
        ) -> std::io::Result<Self> {
            match std::fs::DirBuilder::new().mode(0o711).create(root) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o711))?;
            let path = root.join(fsops::random_id("r", 12)?);
            std::fs::DirBuilder::new().mode(0o700).create(&path)?;
            let mut run = Self {
                path: path.clone(),
                stop: Arc::new(AtomicBool::new(false)),
                accept: None,
            };
            std::os::unix::fs::chown(&path, Some(uid), Some(gid))?;
            for name in ["home", "tmp"] {
                let dir = path.join(name);
                std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
                std::os::unix::fs::chown(&dir, Some(uid), Some(gid))?;
            }
            let socket = path.join("docker.sock");
            let listener = UnixListener::bind(&socket)?;
            std::os::unix::fs::chown(&socket, Some(uid), Some(gid))?;
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;
            let stop = Arc::clone(&run.stop);
            let endpoint = Arc::clone(endpoint);
            let authority = authority.clone();
            let accept = std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if stream.set_nonblocking(false).is_err() {
                                continue;
                            }
                            let endpoint = Arc::clone(&endpoint);
                            let authority = authority.clone();
                            std::thread::spawn(move || endpoint.serve(authority, stream));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(_) => return,
                    }
                }
            });
            run.accept = Some(accept);
            Ok(run)
        }

        fn socket(&self) -> PathBuf {
            self.path.join("docker.sock")
        }
    }

    impl Drop for RunDir {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.accept.take() {
                let _ = handle.join();
            }
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    // ------------------------------------------------------------ the client

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ClientMode {
        /// Unprivileged, confined, on the filtered socket (the real mode).
        Confined,
        /// Deliberately broken `restore-client-unconfined`: unprivileged on the
        /// filtered socket, but nothing limits what it can read.
        Unconfined,
        /// Deliberately broken `restore-unfiltered`: root, on the raw production
        /// socket.
        Raw,
    }

    impl ClientMode {
        fn current() -> Self {
            if break_mode("restore-unfiltered") || break_mode("restore-raw-engine") {
                Self::Raw
            } else if break_mode("restore-client-unconfined") {
                Self::Unconfined
            } else {
                Self::Confined
            }
        }
    }

    /// What the agent sends the `restore-client` helper (one JSON document on its
    /// stdin).
    #[derive(Debug, Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct ClientSpec {
        /// `/proc/<agent pid>/fd/<n>` of the lane folder and of the working
        /// directory (opened by the agent beneath its project descriptor).
        lane_path: String,
        cwd_path: String,
        uid: u32,
        gid: u32,
        confine: bool,
        scratch: Vec<String>,
        argv: Vec<String>,
        env: BTreeMap<String, String>,
    }

    struct ClientOutput {
        code: Option<i32>,
        stdout: Vec<u8>,
        stderr: String,
        timed_out: bool,
    }

    impl ClientOutput {
        fn ok(&self) -> bool {
            self.code == Some(0) && !self.timed_out
        }

        /// The last few lines of stderr: what to tell the user went wrong.
        fn reason(&self) -> String {
            if self.timed_out {
                return "it took too long".into();
            }
            let lines: Vec<&str> = self
                .stderr
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            let tail = lines[lines.len().saturating_sub(2)..].join(" | ");
            if tail.is_empty() {
                format!("exit status {}", self.code.unwrap_or(-1))
            } else {
                text(&tail, 300)
            }
        }
    }

    fn drain<R: Read + Send + 'static>(
        mut reader: R,
        cap: usize,
    ) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            let mut buffer = [0_u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => return kept,
                    Ok(n) => {
                        let room = cap.saturating_sub(kept.len());
                        kept.extend_from_slice(&buffer[..n.min(room)]);
                    }
                }
            }
        })
    }

    /// Runs `command` to completion within `timeout`. On timeout its process
    /// group (the exact group this call created) is killed.
    fn run_bounded(mut command: Command, stdin: Option<&[u8]>, timeout: Duration) -> ClientOutput {
        command
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return ClientOutput {
                    code: None,
                    stdout: Vec::new(),
                    stderr: format!("could not start the compose client: {error}"),
                    timed_out: false,
                };
            }
        };
        if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
            let _ = pipe.write_all(bytes);
        }
        let out = child.stdout.take().map(|s| drain(s, MAX_CLIENT_STDOUT));
        let err = child.stderr.take().map(|s| drain(s, MAX_CLIENT_STDERR));
        let deadline = Instant::now() + timeout;
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(_) => break None,
            }
            if Instant::now() >= deadline {
                timed_out = true;
                if let Some(pid) =
                    rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap_or(i32::MAX))
                {
                    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                }
                let _ = child.kill();
                break child.wait().ok();
            }
            std::thread::sleep(Duration::from_millis(15));
        };
        let stdout = out.and_then(|h| h.join().ok()).unwrap_or_default();
        let stderr = err.and_then(|h| h.join().ok()).unwrap_or_default();
        ClientOutput {
            code: status.and_then(|s| s.code()),
            stdout,
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            timed_out,
        }
    }

    fn client_env(run: &Path, socket: &Path, cwd_path: &str) -> BTreeMap<String, String> {
        let home = run.join("home");
        let mut env = BTreeMap::new();
        for (key, value) in [
            ("DOCKER_HOST", format!("unix://{}", socket.display())),
            ("HOME", home.display().to_string()),
            ("DOCKER_CONFIG", home.join(".docker").display().to_string()),
            ("TMPDIR", run.join("tmp").display().to_string()),
            ("PATH", "/usr/local/bin:/usr/bin:/bin".to_owned()),
            ("PWD", cwd_path.to_owned()),
            ("LANG", "C.UTF-8".to_owned()),
            ("DOCKER_CLI_HINTS", "false".to_owned()),
            ("COMPOSE_ANSI", "never".to_owned()),
            ("COMPOSE_PROGRESS", "plain".to_owned()),
            ("NO_COLOR", "1".to_owned()),
        ] {
            env.insert(key.to_owned(), value);
        }
        env
    }

    // ------------------------------------------------------------ files and ports

    fn beneath_flags() -> ResolveFlags {
        ResolveFlags::BENEATH
            | ResolveFlags::NO_MAGICLINKS
            | ResolveFlags::NO_SYMLINKS
            | ResolveFlags::NO_XDEV
    }

    /// Opens `relative` (`a/b`, or `""` for the directory itself) beneath `root`.
    fn open_beneath_dir(root: &impl AsFd, relative: &str) -> std::io::Result<OwnedFd> {
        if relative.is_empty() {
            return root.as_fd().try_clone_to_owned();
        }
        Ok(openat2(
            root,
            relative,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            beneath_flags(),
        )?)
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(64);
        for byte in Sha256::digest(bytes) {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    fn stat_owner(fd: &impl AsFd) -> (u32, u32) {
        rustix::fs::fstat(fd).map_or((0, 0), |s| (s.st_uid, s.st_gid))
    }

    // ------------------------------------------------------------ state

    struct StackState {
        /// Path of the stack's folder below the lane folder (`""` = the lane).
        rel: String,
        dir: OwnedFd,
        project: String,
        /// The compose project the fork's containers carry.
        fork_project: String,
        /// Services the post-restore check expects.
        services: Vec<String>,
        restored: Vec<Value>,
        fresh: Vec<Value>,
        /// Fork volumes this restore created for the stack.
        volumes: Vec<String>,
        expect_override: Option<String>,
        expect_env: Option<String>,
    }

    struct Prepared {
        project_root: String,
        source: Option<String>,
        lane_dir: String,
        lane: OwnedFd,
        stacks: Vec<StackState>,
        standalone_restored: Vec<Value>,
        standalone_volumes: Vec<String>,
        fresh: Vec<Value>,
        gaps: Vec<String>,
        failures: Vec<String>,
        ports: BTreeMap<String, u16>,
        files: BTreeMap<String, String>,
        timing: BTreeMap<String, u64>,
        reserved: Vec<u16>,
        created: Instant,
    }

    #[derive(Debug)]
    pub struct PrepareRequest {
        pub id: String,
        pub workspace: String,
        pub project: String,
        /// Where the checkpointed stacks' folders are relative to, when not
        /// `project` (a lane's own checkpoint: the first lane's folder).
        pub source: Option<String>,
        pub lane_dir: String,
        pub fork: String,
    }

    #[derive(Debug)]
    pub struct UpRequest {
        pub workspace: String,
        pub project: String,
        pub source: Option<String>,
        pub lane_dir: String,
        pub fork: String,
    }

    pub struct Restorer {
        pool: Pool,
        engine: Engine,
        locks: Arc<ScopeLocks>,
        endpoint: Arc<dyn DockerEndpoint>,
        config: RestoreConfig,
        prepared: Mutex<HashMap<(String, String), Prepared>>,
        /// Fork ports handed out and not yet up, so two restores never pick
        /// the same one.
        reserved: Mutex<Vec<(u16, Instant)>>,
    }

    fn ms(since: Instant) -> u64 {
        u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn push_gap(gaps: &mut Vec<String>, gap: String) {
        if gaps.len() < MAX_GAPS {
            gaps.push(gap);
        } else if gaps.len() == MAX_GAPS {
            gaps.push("more problems were found and are not listed".into());
        }
    }

    fn authority(workspace: &str, lane_dir: &str) -> Authority {
        Authority {
            project: lane_dir.to_owned(),
            workspace: workspace.to_owned(),
            attempt: "restore".to_owned(),
            kind: "restore".to_owned(),
        }
    }

    /// The host's request fields, validated the way every other host-supplied
    /// string is: bounded single components and paths that stay inside.
    fn validate(
        workspace: &str,
        project: &str,
        lane_dir: &str,
        fork: &str,
    ) -> Result<String, RestoreError> {
        validate_label(workspace, "workspace").map_err(RestoreError::Invalid)?;
        if !valid_fork(fork) {
            return Err(RestoreError::Invalid(
                "invalid fork: a compose project name of at most 48 characters".into(),
            ));
        }
        let relative = lane_components(project, lane_dir).map_err(RestoreError::Invalid)?;
        let policy = crate::policy::Authority {
            project_root: PathBuf::from(lane_dir),
            workspace: workspace.to_owned(),
            attempt: "restore".to_owned(),
        };
        policy.validate().map_err(RestoreError::Invalid)?;
        Ok(relative.join("/"))
    }

    impl Restorer {
        /// # Errors
        /// Fails if the pool directory cannot be opened.
        pub fn new(
            pool_path: &Path,
            engine: Engine,
            locks: Arc<ScopeLocks>,
            endpoint: Arc<dyn DockerEndpoint>,
            config: RestoreConfig,
        ) -> std::io::Result<Self> {
            Ok(Self {
                pool: Pool::open(pool_path)?,
                engine,
                locks,
                endpoint,
                config,
                prepared: Mutex::new(HashMap::new()),
                reserved: Mutex::new(Vec::new()),
            })
        }

        fn prepared(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), Prepared>> {
            self.prepared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }

        fn client_ids(lane: &OwnedFd) -> (u32, u32) {
            let (uid, gid) = stat_owner(lane);
            (
                if uid == 0 { NOBODY } else { uid },
                if gid == 0 { NOBODY } else { gid },
            )
        }

        /// Runs `docker <args>` for one stack and returns what it printed.
        fn compose(
            &self,
            session: &Session,
            dir: &OwnedFd,
            dir_path: &str,
            args: &[String],
            timeout: Duration,
        ) -> ClientOutput {
            let env = client_env(&session.run.path, &session.socket, dir_path);
            let mut env = env;
            let mode = ClientMode::current();
            if mode == ClientMode::Raw {
                // Deliberately broken: the raw production socket, as root.
                env.insert(
                    "DOCKER_HOST".into(),
                    format!("unix://{}", self.engine.socket().display()),
                );
                let mut command = Command::new(DOCKER);
                command
                    .args(args)
                    .env_clear()
                    .envs(&env)
                    .current_dir(dir_path);
                return run_bounded(command, None, timeout);
            }
            let pid = std::process::id();
            let spec = ClientSpec {
                lane_path: format!("/proc/{pid}/fd/{}", session.lane.as_raw_fd()),
                cwd_path: format!("/proc/{pid}/fd/{}", dir.as_raw_fd()),
                uid: session.uid,
                gid: session.gid,
                confine: mode == ClientMode::Confined,
                scratch: vec![
                    session.run.path.join("home").display().to_string(),
                    session.run.path.join("tmp").display().to_string(),
                ],
                argv: args.to_vec(),
                env,
            };
            let bytes = serde_json::to_vec(&spec).unwrap_or_default();
            let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/marsh/marsh"));
            let mut command = Command::new(exe);
            // The helper itself runs with exactly the client's environment (and
            // passes it on to `docker`), so what it was given can be inspected.
            command
                .env_clear()
                .envs(&spec.env)
                .arg("--internal-services")
                .arg("restore-client");
            let modes = crate::breaks::active();
            if !modes.is_empty() {
                command.arg("--break-modes").arg(modes.join(","));
            }
            run_bounded(command, Some(&bytes), timeout)
        }

        fn compose_json(
            &self,
            session: &Session,
            stack: &StackDir,
            project: &str,
            files: &[String],
        ) -> Result<Value, String> {
            let mut args: Vec<String> = vec!["compose".into(), "-p".into(), project.into()];
            for file in files {
                args.push("-f".into());
                args.push(file.clone());
            }
            args.extend(["config".into(), "--format".into(), "json".into()]);
            let output = self.compose(
                session,
                &stack.fd,
                &stack.path,
                &args,
                self.config.config_timeout,
            );
            if !output.ok() {
                return Err(output.reason());
            }
            serde_json::from_slice(&output.stdout)
                .map_err(|_| "its configuration could not be read".to_owned())
        }

        // -------------------------------------------------------- ports

        /// A free loopback port inside the engine for each published port,
        /// starting at `original + 10000`. Taken ports are the engine's own
        /// (every scope), other restores' reservations, and anything that
        /// will not bind.
        fn allocate_ports(&self, wanted: &[u16]) -> Result<BTreeMap<u16, u16>, String> {
            let mut used: BTreeSet<u16> = BTreeSet::new();
            for container in self
                .engine
                .list_containers(true)
                .map_err(|error| format!("could not list the engine's ports: {error}"))?
            {
                for port in container.ports.iter().flatten() {
                    if port.public_port != 0 {
                        used.insert(port.public_port);
                    }
                }
            }
            let mut reserved = self
                .reserved
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            reserved.retain(|(_, at)| at.elapsed() < RESERVE_FOR);
            used.extend(reserved.iter().map(|(port, _)| *port));
            let mut map = BTreeMap::new();
            for original in wanted {
                if map.contains_key(original) {
                    continue;
                }
                let mut candidate = u32::from(*original) + 10_000;
                if candidate > 60_000 {
                    candidate = 20_000 + u32::from(*original) % 10_000;
                }
                let mut found = None;
                for _ in 0..4000 {
                    let port = u16::try_from(candidate).unwrap_or(20_000);
                    if !used.contains(&port) && TcpListener::bind(("127.0.0.1", port)).is_ok() {
                        found = Some(port);
                        break;
                    }
                    candidate = if candidate >= 65_000 {
                        20_000
                    } else {
                        candidate + 1
                    };
                }
                let Some(port) = found else {
                    return Err("no free port was found for the fork".into());
                };
                used.insert(port);
                reserved.push((port, Instant::now()));
                map.insert(*original, port);
            }
            Ok(map)
        }

        fn release_ports(&self, ports: &[u16]) {
            self.reserved
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|(port, _)| !ports.contains(port));
        }

        // -------------------------------------------------------- volumes

        fn create_volume(
            session: &Session,
            name: &str,
            labels: &BTreeMap<String, String>,
        ) -> Result<(), String> {
            let path = format!("/volumes/{}", percent_encode(name));
            if session.engine.call("GET", &path, None).is_ok() {
                return Err(format!("volume '{}' already exists", text(name, 80)));
            }
            let body = json!({"Name": name, "Labels": labels}).to_string();
            session
                .engine
                .call("POST", "/volumes/create", Some(body.as_bytes()))
                .map(|_| ())
                .map_err(|error| format!("could not create volume '{}': {error}", text(name, 80)))
        }

        fn remove_volumes(session: &Session, names: &[String]) {
            for name in names {
                let path = format!("/volumes/{}?force=1", percent_encode(name));
                let _ = session.engine.call("DELETE", &path, None);
            }
        }

        /// Opens the source and a staging directory beside `_data` for one volume.
        fn stage(
            &self,
            snapshot: &OwnedFd,
            name: &str,
            source: &VolumeSource,
        ) -> std::io::Result<Staged> {
            let source_fd = match source {
                VolumeSource::Snapshot(id) => fsops::open_dir_at(snapshot, id)?,
                VolumeSource::Live(engine_name) => self.pool.volume_source(engine_name)?,
            };
            let parent = self.pool.volume_dir(name)?;
            let stage = format!(".restore-{}", fsops::random_id("", 6)?);
            let dst = fsops::mkdir_new(&parent, &stage)?;
            Ok(Staged {
                parent,
                stage,
                copy: Some(VolumeCopy {
                    display: text(name, MAX_TEXT),
                    src: source_fd,
                    dst,
                }),
            })
        }

        /// Installs each snapshot volume into its new engine volume: a reflink
        /// clone beside `_data`, swapped in atomically (the volume is new and
        /// unused, so nothing can be reading it). `jobs` are `(engine volume
        /// name, source, expected entries)`; returns the failure text for each
        /// volume that failed.
        fn install(
            &self,
            snapshot: &OwnedFd,
            jobs: &[(String, VolumeSource, u64)],
        ) -> Vec<Option<String>> {
            let mut outcome: Vec<Option<String>> = vec![None; jobs.len()];
            let copier = Arc::new(Copier::new(!break_mode("restore-copy")));
            for _ in 0..self.config.workers.clamp(1, 16) {
                let copier = Arc::clone(&copier);
                std::thread::spawn(move || copier.worker());
            }
            let _stop = StopWorkers(Arc::clone(&copier));
            let mut staged: Vec<Option<Staged>> = Vec::new();
            let mut to_copy = Vec::new();
            let mut slot_of: Vec<Option<usize>> = Vec::new();
            for (position, (name, source, _)) in jobs.iter().enumerate() {
                match self.stage(snapshot, name, source) {
                    Ok(mut one) => {
                        slot_of.push(Some(to_copy.len()));
                        to_copy.extend(one.copy.take());
                        staged.push(Some(one));
                    }
                    Err(error) => {
                        outcome[position] = Some(format!(
                            "volume '{}': its data could not be prepared ({error})",
                            text(name, 80)
                        ));
                        slot_of.push(None);
                        staged.push(None);
                    }
                }
            }
            // Deliberately wrong (`restore-skip-volumes`, silent; and the fault
            // `fault-restore-skip-volumes`, which the entry count below catches):
            // the data is never copied.
            let silent = break_mode("restore-skip-volumes");
            if silent || break_mode("fault-restore-skip-volumes") {
                drop(to_copy);
                slot_of.fill(None);
            } else if !to_copy.is_empty() {
                match copier.submit(to_copy) {
                    Ok(first) => {
                        copier.wait(&mut || {});
                        for slot in slot_of.iter_mut().flatten() {
                            *slot += first;
                        }
                    }
                    Err(error) => {
                        for (position, slot) in outcome.iter_mut().enumerate() {
                            if staged[position].is_some() {
                                *slot = Some(format!("the data could not be read ({error})"));
                            }
                        }
                        return outcome;
                    }
                }
            }
            // The two silent defects also skip the entry count (a restore from
            // the live volume would otherwise be caught by it).
            let unverified = silent || break_mode("restore-from-live");
            for (position, (name, _, expected)) in jobs.iter().enumerate() {
                let Some(one) = staged[position].take() else {
                    continue;
                };
                let (complete, files, gaps) = match slot_of[position] {
                    Some(slot) => {
                        let result = copier.result(slot);
                        (result.complete, result.files, result.gaps)
                    }
                    // Silently skipped: reported as if it had worked.
                    None if silent => (true, *expected, Vec::new()),
                    None => (true, 0, Vec::new()),
                };
                let expected = if unverified { files } else { *expected };
                outcome[position] = Self::settle(&one, name, (complete, files, &gaps), expected);
                if let Ok(stage) = std::ffi::CString::new(one.stage.as_str()) {
                    let _ = fsops::remove_tree_at(&one.parent, &stage);
                }
            }
            outcome
        }

        /// Checks one installed volume (the copy finished, with as many entries
        /// as the snapshot recorded) and only then swaps it in.
        fn settle(
            one: &Staged,
            name: &str,
            copied: (bool, u64, &[String]),
            expected: u64,
        ) -> Option<String> {
            let (complete, files, gaps) = copied;
            if !(complete && files == expected) {
                let why = gaps.first().map_or_else(
                    || format!("{files} of {expected} entries were installed"),
                    |gap| text(gap, 120),
                );
                return Some(format!(
                    "volume '{}': its data was not installed ({why})",
                    text(name, 80)
                ));
            }
            fsops::exchange(&one.parent, &one.stage, "_data")
                .err()
                .map(|error| {
                    format!(
                        "volume '{}': its data could not be put in place ({error})",
                        text(name, 80)
                    )
                })
        }

        // -------------------------------------------------------- prepare

        fn manifest(&self, id: &str) -> Result<(SnapManifest, OwnedFd), RestoreError> {
            let snapshots = self.pool.snapshots()?;
            let dir = fsops::open_dir_at(&snapshots, id).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    RestoreError::Failed("that snapshot does not exist".into())
                } else {
                    RestoreError::Failed(format!("the snapshot is not usable: {error}"))
                }
            })?;
            let bytes = fsops::read_file_at(&dir, "manifest.json", 1 << 20).map_err(|error| {
                RestoreError::Failed(format!("the snapshot has no manifest ({error})"))
            })?;
            let manifest: SnapManifest = serde_json::from_slice(&bytes)
                .map_err(|_| RestoreError::Failed("the snapshot's manifest is damaged".into()))?;
            if manifest.version != 1 {
                return Err(RestoreError::Failed(
                    "the snapshot was made by a newer version".into(),
                ));
            }
            Ok((manifest, dir))
        }

        /// Phase one. See the module comment.
        ///
        /// # Errors
        /// `Invalid` for a bad request; `Failed` when nothing could be restored
        /// at all. Problems with one stack come back as gaps in the result.
        #[allow(clippy::too_many_lines)]
        pub fn prepare(&self, request: &PrepareRequest) -> Result<Value, RestoreError> {
            let started = Instant::now();
            if !valid_snapshot_id(&request.id) {
                return Err(RestoreError::Invalid("invalid snapshot id".into()));
            }
            let lane_rel = validate(
                &request.workspace,
                &request.project,
                &request.lane_dir,
                &request.fork,
            )?;
            let key = (request.workspace.clone(), request.fork.clone());
            if self.prepared().contains_key(&key) {
                return Err(RestoreError::Failed(
                    "this fork has already been prepared".into(),
                ));
            }
            let _op = self
                .locks
                .op(&request.workspace, "restore", self.config.lock_wait)
                .map_err(|busy| {
                    RestoreError::Failed(format!(
                        "workspace busy ({} in progress); retry",
                        busy.holder.unwrap_or("another operation")
                    ))
                })?;
            let (manifest, snapshot) = self.manifest(&request.id)?;

            let project_fd = rustix::fs::open(
                request.project.as_str(),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| {
                RestoreError::Failed(format!("the project folder is not usable: {error}"))
            })?;
            let lane = open_beneath_dir(&project_fd, &lane_rel).map_err(|error| {
                RestoreError::Failed(format!(
                    "the lane folder is not usable: {}",
                    describe(&error)
                ))
            })?;
            drop(project_fd);
            let (uid, gid) = Self::client_ids(&lane);
            let authority = authority(&request.workspace, &request.lane_dir);
            self.endpoint
                .check(&authority)
                .map_err(RestoreError::Invalid)?;
            let run = RunDir::create(&self.config.run_root, uid, gid, &authority, &self.endpoint)
                .map_err(|error| {
                RestoreError::Failed(format!("could not start the filtered socket: {error}"))
            })?;
            let session = Session {
                socket: run.socket(),
                engine: Engine::new(run.socket()),
                run,
                lane: lane.try_clone()?,
                uid,
                gid,
            };

            let mut state = Prepared {
                project_root: request.project.clone(),
                source: request.source.clone(),
                lane_dir: request.lane_dir.clone(),
                lane,
                stacks: Vec::new(),
                standalone_restored: Vec::new(),
                standalone_volumes: Vec::new(),
                fresh: Vec::new(),
                gaps: manifest
                    .gaps
                    .iter()
                    .take(16)
                    .map(|gap| format!("when it was saved: {}", text(gap, 200)))
                    .collect(),
                failures: Vec::new(),
                ports: BTreeMap::new(),
                files: BTreeMap::new(),
                timing: BTreeMap::new(),
                reserved: Vec::new(),
                created: Instant::now(),
            };

            let t_plan = Instant::now();
            let compose_stacks = manifest
                .stacks
                .iter()
                .filter(|stack| stack.kind == "compose")
                .count();
            // The fork's compose project: the host's `fork` itself when the
            // snapshot has one compose stack; otherwise one project per stack
            // (two stacks in one project would see each other as orphans).
            let fork_name = |project: &str, index: usize| -> String {
                if break_mode("restore-same-project") {
                    // Deliberately wrong: the fork reuses the original's project.
                    project.to_owned()
                } else if compose_stacks <= 1 {
                    request.fork.clone()
                } else {
                    let full = format!("{}-{project}", request.fork);
                    if valid_project_name(&full) && full.len() <= 63 {
                        full
                    } else {
                        format!("{}-s{}", request.fork, index + 1)
                    }
                }
            };
            let compose_version = self.compose_version(&session);
            let mut claimed: BTreeSet<String> = BTreeSet::new();
            let mut planned: Vec<PlannedStack> = Vec::new();
            for (index, stack) in manifest.stacks.iter().take(MAX_STACKS).enumerate() {
                // A volume belongs to a stack when its label says so or when one
                // of the stack's containers mounts it (anonymous volumes carry
                // no compose label); the stack decides whether it is restored.
                for volume in &manifest.volumes {
                    if volume
                        .labels
                        .get("com.docker.compose.project")
                        .is_some_and(|p| *p == stack.project)
                    {
                        claimed.insert(volume.id.clone());
                    }
                }
                claimed.extend(
                    stack
                        .containers
                        .iter()
                        .flat_map(|c| c.volumes.iter().cloned()),
                );
                match self.plan_stack(
                    &session,
                    request,
                    stack,
                    &manifest,
                    &fork_name(&stack.project, index),
                ) {
                    Ok(Some(plan)) => planned.push(plan),
                    Ok(None) => {}
                    Err(outcome) => {
                        if outcome.failure {
                            state.failures.extend(outcome.gaps.iter().cloned());
                        }
                        state.gaps.extend(outcome.gaps);
                        state.fresh.extend(outcome.fresh);
                    }
                }
            }
            if manifest.stacks.len() > MAX_STACKS {
                push_gap(
                    &mut state.gaps,
                    format!(
                        "{} stacks; only the first {MAX_STACKS} were considered",
                        manifest.stacks.len()
                    ),
                );
            }
            state.timing.insert("plan_ms".into(), ms(t_plan));

            // Ports for every planned stack, in one allocation.
            let wanted: Vec<u16> = planned
                .iter()
                .flat_map(|plan| plan.services.iter())
                .flat_map(|(_, ports)| ports.iter())
                .filter_map(|port| port.published)
                .collect();
            let skip_override = break_mode("restore-no-override");
            let map = if wanted.is_empty() || skip_override {
                BTreeMap::new()
            } else {
                match self.allocate_ports(&wanted) {
                    Ok(map) => map,
                    Err(why) => {
                        return Err(RestoreError::Failed(why));
                    }
                }
            };
            state.reserved = map.values().copied().collect();

            hold("restore-before-install", &mut || {});

            // Volumes: create in the lane's scope, then install the data.
            let t_volumes = Instant::now();
            let mut jobs: Vec<(String, VolumeSource, u64)> = Vec::new();
            let mut job_owner: Vec<JobOwner> = Vec::new();
            let mut created: Vec<String> = Vec::new();
            let live = break_mode("restore-from-live");
            for (plan_index, plan) in planned.iter_mut().enumerate() {
                for volume in &mut plan.volumes {
                    let labels = BTreeMap::from([
                        (
                            "com.docker.compose.project".to_owned(),
                            plan.fork_project.clone(),
                        ),
                        ("com.docker.compose.volume".to_owned(), volume.key.clone()),
                        (
                            "com.docker.compose.version".to_owned(),
                            compose_version.clone(),
                        ),
                    ]);
                    if let Err(why) = Self::create_volume(&session, &volume.fork_name, &labels) {
                        plan.failed.push(why);
                        continue;
                    }
                    created.push(volume.fork_name.clone());
                    plan.created.push(volume.fork_name.clone());
                    jobs.push((
                        volume.fork_name.clone(),
                        source_for(&volume.snapshot, &volume.live_name, live),
                        volume.expected,
                    ));
                    job_owner.push(JobOwner::Stack(plan_index));
                }
            }
            let mut standalone_plan: Vec<StandalonePlan> = Vec::new();
            for volume in &manifest.volumes {
                if claimed.contains(&volume.id) {
                    continue;
                }
                let display = text(&volume.name, 80);
                if !volume.captured {
                    push_gap(&mut state.gaps, format!("volume '{display}' was not saved"));
                    continue;
                }
                if !valid_snapshot_id(&volume.id) {
                    push_gap(
                        &mut state.gaps,
                        format!("volume '{display}' has an invalid id"),
                    );
                    continue;
                }
                let fork_name = standalone_name(&request.fork, volume);
                let users: Vec<String> = manifest
                    .standalone
                    .iter()
                    .filter(|c| !c.testcontainers && c.volumes.contains(&volume.id))
                    .map(|c| text(&c.name, 80))
                    .collect();
                if manifest
                    .standalone
                    .iter()
                    .any(|c| c.testcontainers && c.volumes.contains(&volume.id))
                {
                    state.fresh.push(json!({
                        "kind": "testcontainers", "name": display,
                        "reason": "not restored: it belongs to Testcontainers",
                    }));
                    continue;
                }
                if let Err(why) = Self::create_volume(&session, &fork_name, &BTreeMap::new()) {
                    state.failures.push(why.clone());
                    push_gap(&mut state.gaps, why);
                    continue;
                }
                created.push(fork_name.clone());
                jobs.push((
                    fork_name.clone(),
                    source_for(&volume.id, &volume.name, live),
                    volume.files,
                ));
                job_owner.push(JobOwner::Standalone(standalone_plan.len()));
                standalone_plan.push(StandalonePlan {
                    fork_name,
                    display,
                    users,
                });
            }
            hold("restore-volumes-created", &mut || {});
            if break_mode("slow-restore") {
                std::thread::sleep(Duration::from_millis(1500));
            }
            let results = self.install(&snapshot, &jobs);
            state.timing.insert("volumes_ms".into(), ms(t_volumes));

            for ((job, owner), result) in jobs.iter().zip(&job_owner).zip(results) {
                if let Some(why) = result {
                    match owner {
                        JobOwner::Stack(index) => planned[*index].failed.push(why),
                        JobOwner::Standalone(index) => {
                            let name = &standalone_plan[*index].fork_name;
                            Self::remove_volumes(&session, std::slice::from_ref(name));
                            created.retain(|c| c != name);
                            state.failures.push(why.clone());
                            push_gap(&mut state.gaps, why);
                        }
                    }
                    let _ = job;
                } else if let JobOwner::Standalone(index) = owner {
                    let item = &standalone_plan[*index];
                    state.standalone_volumes.push(item.fork_name.clone());
                    state.standalone_restored.push(json!({
                        "kind": "volume",
                        "name": item.fork_name,
                        "from": item.display,
                        "containers": item.users,
                        "restarted": false,
                    }));
                }
            }
            for container in &manifest.standalone {
                if container.testcontainers {
                    state.fresh.push(json!({
                        "kind": "testcontainers",
                        "name": text(&container.name, 80),
                        "reason": "not restored",
                    }));
                } else if container.volumes.is_empty() {
                    state.fresh.push(json!({
                        "kind": "container",
                        "name": text(&container.name, 80),
                        "reason": "not restarted: it has no saved data",
                    }));
                }
            }

            // Files, ports and per-stack results.
            let t_files = Instant::now();
            for plan in planned {
                if !plan.failed.is_empty() {
                    Self::remove_volumes(&session, &plan.created);
                    created.retain(|c| !plan.created.contains(c));
                    for why in &plan.failed {
                        let why = format!("stack '{}': {why}", text(&plan.project, 80));
                        state.failures.push(why.clone());
                        push_gap(&mut state.gaps, why);
                    }
                    for service in &plan.services_all {
                        state.fresh.push(json!({
                            "kind": "service", "service": text(service, 80),
                            "stack": text(&plan.project, 80),
                            "reason": "not restored: its data could not be installed",
                        }));
                    }
                    continue;
                }
                match Self::finish_stack(&mut state, &plan, &map, skip_override) {
                    Ok(stack) => state.stacks.push(stack),
                    Err(why) => {
                        Self::remove_volumes(&session, &plan.created);
                        created.retain(|c| !plan.created.contains(c));
                        state.failures.push(why.clone());
                        push_gap(&mut state.gaps, why);
                    }
                }
            }
            state.timing.insert("files_ms".into(), ms(t_files));
            state
                .ports
                .extend(map.iter().map(|(old, new)| (old.to_string(), *new)));
            state.timing.insert("prepare_ms".into(), ms(started));
            let mut reply = Self::reply(&state, None);
            hostile_reply(&mut reply);
            let size = serde_json::to_vec(&reply).map_or(usize::MAX, |bytes| bytes.len());
            if size > MAX_REPLY_BYTES {
                Self::remove_volumes(&session, &created);
                self.release_ports(&state.reserved);
                return Err(RestoreError::Failed(
                    "the restore result is too large to report".into(),
                ));
            }
            drop(session);
            self.prepared().insert(key, state);
            Ok(reply)
        }

        fn compose_version(&self, session: &Session) -> String {
            let Some(empty) = std::fs::File::open("/").ok().map(OwnedFd::from) else {
                return "5.7.0".into();
            };
            let output = self.compose(
                session,
                &empty,
                "/",
                &["compose".into(), "version".into(), "--short".into()],
                self.config.config_timeout,
            );
            let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if output.ok()
                && !version.is_empty()
                && version.len() <= 32
                && version
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
            {
                version
            } else {
                "5.7.0".into()
            }
        }

        /// Reads one stack's compose configuration and decides what to do with
        /// it. `Err` carries a stack that is refused or unreadable (reported,
        /// never attempted); `Ok(None)` is a stack restore does not handle.
        #[allow(clippy::too_many_lines)]
        fn plan_stack(
            &self,
            session: &Session,
            request: &PrepareRequest,
            stack: &SnapStack,
            manifest: &SnapManifest,
            fork_project: &str,
        ) -> Result<Option<PlannedStack>, Refused> {
            let shown = text(&stack.project, 80);
            let services_of = |reason: &str| -> Vec<Value> {
                let mut seen = BTreeSet::new();
                stack
                    .containers
                    .iter()
                    .filter_map(|c| c.service.as_deref())
                    .filter(|s| seen.insert(*s))
                    .map(|service| {
                        json!({"kind": "service", "stack": shown, "service": text(service, 80),
                               "reason": reason})
                    })
                    .collect()
            };
            let refuse = |gap: String, reason: &str| Refused {
                gaps: vec![gap],
                fresh: services_of(reason),
                failure: false,
            };
            let fail = |gap: String, reason: &str| Refused {
                failure: true,
                ..refuse(gap, reason)
            };
            if stack.kind != "compose" {
                return Err(refuse(
                    format!(
                        "'{shown}' is a {} project; its restore is not available yet",
                        text(&stack.kind, 40)
                    ),
                    "not restored",
                ));
            }
            if !valid_project_name(&stack.project) {
                return Err(refuse(
                    format!("stack '{shown}' has a project name restore cannot use"),
                    "not restored",
                ));
            }
            // Where the stack's folder is, as a path below the lane.
            let root = components(request.source.as_deref().unwrap_or(&request.project))
                .map_err(|why| refuse(why, "not restored"))?;
            let rel = match stack.working_dir.as_deref() {
                None | Some("") => String::new(),
                Some(dir) => match components(dir) {
                    Ok(parts) if parts.len() >= root.len() && parts[..root.len()] == root[..] => {
                        parts[root.len()..].join("/")
                    }
                    _ => {
                        return Err(refuse(
                            format!(
                                "stack '{shown}': its folder is outside the project; not restored"
                            ),
                            "not restored: its folder is outside the project",
                        ));
                    }
                },
            };
            if !relative_ok(&rel) {
                return Err(refuse(
                    format!("stack '{shown}': its folder name cannot be used; not restored"),
                    "not restored",
                ));
            }
            let Ok(dir) = open_beneath_dir(&session.lane, &rel) else {
                return Err(fail(
                    format!("stack '{shown}': its folder is not in the lane; not restored"),
                    "not restored: its compose files are not in the lane",
                ));
            };
            let dir_path = if rel.is_empty() {
                request.lane_dir.clone()
            } else {
                format!("{}/{rel}", request.lane_dir)
            };
            // The compose files, relative to the stack's folder.
            let base = stack
                .working_dir
                .as_deref()
                .filter(|w| !w.is_empty())
                .map(|w| format!("{}/", w.trim_end_matches('/')));
            let mut files: Vec<String> = Vec::new();
            for file in &stack.config_files {
                let relative = base
                    .as_deref()
                    .and_then(|b| file.strip_prefix(b))
                    .map_or_else(|| file.clone(), str::to_owned);
                if relative == OVERRIDE_NAME {
                    if fsops::exists_at(&dir, OVERRIDE_NAME) {
                        files.push(relative);
                    }
                    continue;
                }
                if relative.starts_with('/') || !compose_file_ok(&relative) {
                    return Err(refuse(
                        format!(
                            "stack '{shown}': a compose file is outside its folder; not restored"
                        ),
                        "not restored",
                    ));
                }
                files.push(relative);
            }
            let stack_dir = StackDir {
                fd: dir,
                path: dir_path,
            };
            let config = match self.compose_json(session, &stack_dir, &stack.project, &files) {
                Ok(config) => config,
                Err(why) => {
                    return Err(fail(
                        format!("stack '{shown}' could not be read: {why}; not restored"),
                        "not restored: its compose files could not be read",
                    ));
                }
            };
            let mut found = blockers(&config, &stack.project);
            // A hostile name would be written into generated files.
            for name in config["services"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, _)| k)
            {
                if !valid_key(name) {
                    found.push(format!("service name {:?} is not usable", text(name, 40)));
                }
            }
            for name in config["volumes"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, _)| k)
            {
                if !valid_key(name) {
                    found.push(format!("volume name {:?} is not usable", text(name, 40)));
                }
            }
            if config["services"]
                .as_object()
                .is_none_or(|s| s.len() > MAX_SERVICES)
            {
                found.push("it has no services or too many".into());
            }
            if !found.is_empty() && !break_mode("restore-ignore-blockers") {
                // Refused here (and only here): other stacks go on.
                let mut gaps = vec![format!(
                    "stack '{shown}' not restored: {}",
                    found.join("; ")
                )];
                gaps.truncate(1);
                return Err(Refused {
                    gaps,
                    fresh: services_of("not restored: it cannot be forked safely"),
                    failure: false,
                });
            }
            if files.is_empty() {
                // Compose found them by its own discovery; COMPOSE_FILE needs names.
                for candidate in [
                    "compose.yaml",
                    "compose.yml",
                    "docker-compose.yaml",
                    "docker-compose.yml",
                ] {
                    if fsops::exists_at(&stack_dir.fd, candidate) {
                        files.push(candidate.to_owned());
                        break;
                    }
                }
            }
            if files.is_empty() {
                return Err(fail(
                    format!("stack '{shown}': its compose file is not in the lane; not restored"),
                    "not restored: its compose file is not in the lane",
                ));
            }
            // Volumes of the project that the snapshot saved, by compose key.
            let mut volumes = Vec::new();
            let mut by_key: BTreeMap<String, &SnapVolume> = BTreeMap::new();
            for volume in &manifest.volumes {
                if volume.labels.get("com.docker.compose.project") == Some(&stack.project)
                    && let Some(key) = volume.labels.get("com.docker.compose.volume")
                {
                    by_key.insert(key.clone(), volume);
                }
            }
            let mut uncaptured: BTreeSet<String> = BTreeSet::new();
            for key in config["volumes"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, _)| k)
            {
                let Some(saved) = by_key.get(key.as_str()) else {
                    continue;
                };
                if !saved.captured || !valid_snapshot_id(&saved.id) {
                    uncaptured.insert(key.clone());
                    continue;
                }
                let fork_name = format!("{fork_project}_{key}");
                if !crate::fsops::valid_volume_name(&fork_name) {
                    return Err(refuse(
                        format!(
                            "stack '{shown}': volume '{}' has a name restore cannot use",
                            text(key, 40)
                        ),
                        "not restored",
                    ));
                }
                volumes.push(PlannedVolume {
                    key: key.clone(),
                    fork_name,
                    snapshot: saved.id.clone(),
                    live_name: saved.name.clone(),
                    expected: saved.files,
                });
            }
            // What each service gets.
            let mut restored = Vec::new();
            let mut fresh = Vec::new();
            let mut services = service_ports(&config);
            for (name, service) in config["services"].as_object().into_iter().flatten() {
                let keys: Vec<String> = service["volumes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|m| m["type"].as_str() == Some("volume"))
                    .filter_map(|m| m["source"].as_str().map(str::to_owned))
                    .collect();
                let saved: Vec<&String> = keys
                    .iter()
                    .filter(|k| volumes.iter().any(|v| &v.key == *k))
                    .collect();
                if keys.is_empty() {
                    fresh.push(
                        json!({"kind": "service", "stack": shown, "service": text(name, 80),
                        "reason": "no saved data: it keeps nothing in volumes"}),
                    );
                } else if saved.len() == keys.len() {
                    restored.push(
                        json!({"kind": "service", "stack": shown, "service": text(name, 80),
                        "volumes": saved.iter().map(|k| text(k, 80)).collect::<Vec<_>>()}),
                    );
                } else {
                    let missing = keys
                        .iter()
                        .find(|k| !volumes.iter().any(|v| &v.key == *k))
                        .map_or_else(String::new, |k| text(k, 60));
                    let reason = if uncaptured.contains(&missing) {
                        format!("its saved data for '{missing}' was incomplete")
                    } else {
                        format!("no saved data for '{missing}'")
                    };
                    fresh.push(
                        json!({"kind": "service", "stack": shown, "service": text(name, 80),
                        "reason": reason}),
                    );
                }
            }
            let mut gaps = Vec::new();
            for key in &uncaptured {
                gaps.push(format!(
                    "stack '{shown}': volume '{}' was not saved completely",
                    text(key, 60)
                ));
            }
            if break_mode("restore-no-override") {
                services.clear();
            }
            Ok(Some(PlannedStack {
                rel,
                dir: stack_dir.fd,
                project: stack.project.clone(),
                fork_project: fork_project.to_owned(),
                files,
                services_all: config["services"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(k, _)| k.clone())
                    .collect(),
                services,
                volumes,
                restored,
                fresh,
                gaps,
                created: Vec::new(),
                failed: Vec::new(),
            }))
        }

        /// Builds the stack's file contents and its state for phase two.
        fn finish_stack(
            state: &mut Prepared,
            plan: &PlannedStack,
            map: &BTreeMap<u16, u16>,
            skip_override: bool,
        ) -> Result<StackState, String> {
            let original = match fsops::read_file_at(&plan.dir, ENV_NAME, MAX_ENV_BYTES) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(error) => {
                    return Err(format!(
                        "stack '{}': the lane's .env cannot be used ({}); not restored",
                        text(&plan.project, 80),
                        describe(&error)
                    ));
                }
            };
            let project = &plan.fork_project;
            let mut files: Vec<String> = plan
                .files
                .iter()
                .filter(|f| f.as_str() != OVERRIDE_NAME)
                .cloned()
                .collect();
            if !skip_override {
                files.push(OVERRIDE_NAME.to_owned());
            }
            let override_text =
                (!skip_override).then(|| override_yaml(project, &remap(&plan.services, map)));
            let env = env_content(&original, map, project, &files.join(":"));
            let omit_env = break_mode("restore-no-env");
            let prefix = if plan.rel.is_empty() {
                String::new()
            } else {
                format!("{}/", plan.rel)
            };
            if let Some(content) = &override_text {
                state
                    .files
                    .insert(format!("{prefix}{OVERRIDE_NAME}"), content.clone());
            }
            if !omit_env {
                state
                    .files
                    .insert(format!("{prefix}{ENV_NAME}"), env.clone());
            }
            if break_mode("restore-files-from-agent") {
                // Deliberately wrong (A9): the agent, not the daemon on the Mac,
                // writes the lane's files.
                if let Some(content) = &override_text {
                    let _ = fsops::replace_file_at(&plan.dir, OVERRIDE_NAME, content.as_bytes());
                }
                if !omit_env {
                    let _ = fsops::replace_file_at(&plan.dir, ENV_NAME, env.as_bytes());
                }
            }
            for gap in &plan.gaps {
                push_gap(&mut state.gaps, gap.clone());
            }
            Ok(StackState {
                rel: plan.rel.clone(),
                dir: plan.dir.try_clone().map_err(|e| e.to_string())?,
                project: plan.project.clone(),
                fork_project: plan.fork_project.clone(),
                services: plan.services_all.clone(),
                restored: plan.restored.clone(),
                fresh: plan.fresh.clone(),
                volumes: plan.created.clone(),
                expect_override: override_text.as_deref().map(|c| sha256_hex(c.as_bytes())),
                expect_env: (!omit_env).then(|| sha256_hex(env.as_bytes())),
            })
        }

        fn reply(state: &Prepared, check: Option<(&Value, bool)>) -> Value {
            let mut restored: Vec<Value> = state
                .stacks
                .iter()
                .flat_map(|s| s.restored.iter().cloned())
                .collect();
            restored.extend(state.standalone_restored.iter().cloned());
            let mut fresh: Vec<Value> = state
                .stacks
                .iter()
                .flat_map(|s| s.fresh.iter().cloned())
                .collect();
            fresh.extend(state.fresh.iter().cloned());
            let projects: Vec<Value> = state
                .stacks
                .iter()
                .map(|s| json!({"stack": s.rel, "original": text(&s.project, 80), "project": s.fork_project}))
                .collect();
            let mut reply = json!({
                "projects": projects,
                "restored": restored,
                "fresh": fresh,
                "gaps": state.gaps,
                "ports": state.ports,
                "files": state.files,
                "failures": state.failures,
                "timing_ms": state.timing,
            });
            match check {
                Some((check, ok)) => {
                    reply["check"] = check.clone();
                    reply["ok"] = json!(ok);
                }
                None => reply["ok"] = json!(state.failures.is_empty()),
            }
            reply
        }

        // -------------------------------------------------------- up

        /// Phase two. See the module comment.
        ///
        /// # Errors
        /// `Invalid` for a bad request; `Failed` when there is nothing prepared
        /// for that fork, or the workspace is busy.
        #[allow(clippy::too_many_lines)]
        pub fn up(&self, request: &UpRequest) -> Result<Value, RestoreError> {
            let started = Instant::now();
            validate(
                &request.workspace,
                &request.project,
                &request.lane_dir,
                &request.fork,
            )?;
            let key = (request.workspace.clone(), request.fork.clone());
            let mut state = self
                .prepared()
                .remove(&key)
                .ok_or_else(|| RestoreError::Failed("nothing was prepared for this fork".into()))?;
            if state.lane_dir != request.lane_dir
                || state.project_root != request.project
                || state.source != request.source
            {
                self.prepared().insert(key, state);
                return Err(RestoreError::Failed(
                    "this fork was prepared for a different folder".into(),
                ));
            }
            let busy = |busy: crate::locks::Busy| {
                RestoreError::Failed(format!(
                    "workspace busy ({} in progress); retry",
                    busy.holder.unwrap_or("another operation")
                ))
            };
            let op = match self
                .locks
                .op(&request.workspace, "restore", self.config.lock_wait)
            {
                Ok(op) => op,
                Err(error) => {
                    self.prepared().insert(key, state);
                    return Err(busy(error));
                }
            };
            let (uid, gid) = Self::client_ids(&state.lane);
            let authority = authority(&request.workspace, &request.lane_dir);
            let run = RunDir::create(&self.config.run_root, uid, gid, &authority, &self.endpoint)
                .map_err(|error| {
                RestoreError::Failed(format!("could not start the filtered socket: {error}"))
            })?;
            let session = Session {
                socket: run.socket(),
                engine: Engine::new(run.socket()),
                run,
                lane: state.lane.try_clone()?,
                uid,
                gid,
            };
            hold("restore-before-up", &mut || {});

            let t_up = Instant::now();
            let mut stacks = std::mem::take(&mut state.stacks);
            let mut up_stacks: Vec<StackState> = Vec::new();
            for stack in stacks.drain(..) {
                match self.start_stack(&session, &state.lane_dir, &stack) {
                    Ok(()) => up_stacks.push(stack),
                    Err(why) => {
                        // No half-started fork: take down whatever was made.
                        let down: Vec<String> =
                            ["compose", "down", "-v", "--remove-orphans", "-t", "3"]
                                .map(str::to_owned)
                                .to_vec();
                        let dir_path = stack_path(&state.lane_dir, &stack.rel);
                        let _ = self.compose(
                            &session,
                            &stack.dir,
                            &dir_path,
                            &down,
                            self.config.up_timeout,
                        );
                        Self::remove_volumes(&session, &stack.volumes);
                        state.failures.push(why.clone());
                        push_gap(&mut state.gaps, why);
                        for entry in &stack.restored {
                            let service = entry["service"].as_str().unwrap_or("?");
                            state.fresh.push(json!({
                                "kind": "service", "stack": text(&stack.project, 80),
                                "service": service,
                                "reason": "not restored: it did not start",
                            }));
                        }
                    }
                }
            }
            state.timing.insert("up_ms".into(), ms(t_up));
            state.stacks = up_stacks;

            let t_check = Instant::now();
            let check = self.check(&request.workspace, &state.stacks);
            state.timing.insert("check_ms".into(), ms(t_check));
            state.timing.insert("up_total_ms".into(), ms(started));
            state
                .timing
                .insert("since_prepare_ms".into(), ms(state.created));
            let check_ok = check["ok"].as_bool() == Some(true);
            if !check_ok && let Some(problems) = check["problems"].as_array() {
                for problem in problems.iter().take(8) {
                    if let Some(problem) = problem.as_str() {
                        state.failures.push(format!("check: {problem}"));
                    }
                }
            }
            let ok = state.failures.is_empty() && check_ok;
            let reply = Self::reply(&state, Some((&check, ok)));
            self.release_ports(&state.reserved);
            drop(session);
            drop(op);
            Ok(reply)
        }

        /// Drops everything of one workspace (C3: a lane is closed, cancelled
        /// or removed): its containers, volumes and networks are removed
        /// through the filter **as that workspace** (so nothing of another
        /// workspace can be named), then the scope's own ledger records go.
        /// Safe to repeat. What is still there afterwards is reported, never
        /// hidden.
        ///
        /// # Errors
        /// `Invalid` for a bad workspace id; `Failed` when the workspace is busy
        /// (a restore or capture of it is still running) or the filtered socket
        /// cannot be made.
        pub fn drop_workspace(&self, workspace: &str) -> Result<DropReport, RestoreError> {
            validate_label(workspace, "workspace").map_err(RestoreError::Invalid)?;
            let _op = self
                .locks
                .op(workspace, "drop", DROP_LOCK_WAIT)
                .map_err(|busy| {
                    RestoreError::Failed(format!(
                        "workspace busy ({} in progress); retry",
                        busy.holder.unwrap_or("another operation")
                    ))
                })?;
            let authority = Authority {
                // Nothing resolves beneath this: a drop names no path.
                project: format!("{RUN_ROOT}/drop"),
                workspace: workspace.to_owned(),
                attempt: "drop".to_owned(),
                kind: "restore".to_owned(),
            };
            self.endpoint
                .check(&authority)
                .map_err(RestoreError::Invalid)?;
            let run = RunDir::create(&self.config.run_root, 0, 0, &authority, &self.endpoint)
                .map_err(|error| {
                    RestoreError::Failed(format!("could not start the filtered socket: {error}"))
                })?;
            let engine = Engine::new(run.socket());
            let mut report = DropReport::default();
            // Containers first (they hold the volumes and the networks).
            for _round in 0..2 {
                let ids = Self::scope_ids(&engine, "/containers/json?all=1", "Id");
                if ids.is_empty() {
                    break;
                }
                report.containers +=
                    Self::remove_all(&engine, &ids, |id| format!("/containers/{id}?force=1&v=1"));
            }
            let names = Self::scope_volumes(&engine);
            report.volumes += Self::remove_all(&engine, &names, |name| {
                format!("/volumes/{}?force=1", percent_encode(name))
            });
            let nets = Self::scope_ids(&engine, "/networks", "Id");
            report.networks += Self::remove_all(&engine, &nets, |id| format!("/networks/{id}"));
            // What is still there is the answer; it is never assumed gone.
            for (what, left) in [
                (
                    "container",
                    Self::scope_ids(&engine, "/containers/json?all=1", "Id"),
                ),
                ("volume", Self::scope_volumes(&engine)),
                ("network", Self::scope_ids(&engine, "/networks", "Id")),
            ] {
                for item in left.iter().take(8) {
                    report.left.push(format!("{what} {}", text(item, 80)));
                }
            }
            if let Err(why) = self.endpoint.forget_scope(workspace) {
                report
                    .left
                    .push(format!("scope records: {}", text(&why, 120)));
            }
            drop(run);
            report.left.truncate(24);
            Ok(report)
        }

        /// The ids (or names) of one kind of object the filtered socket lists:
        /// only this workspace's, because the filter shows nothing else.
        fn scope_ids(engine: &Engine, path: &str, key: &str) -> Vec<String> {
            let Ok(body) = engine.call("GET", path, None) else {
                return Vec::new();
            };
            serde_json::from_slice::<Vec<Value>>(&body)
                .unwrap_or_default()
                .iter()
                .filter_map(|item| item[key].as_str())
                .filter(|id| crate::engine_api::valid_container_id(id))
                .map(str::to_owned)
                .take(MAX_DROP_OBJECTS)
                .collect()
        }

        fn scope_volumes(engine: &Engine) -> Vec<String> {
            let Ok(body) = engine.call("GET", "/volumes", None) else {
                return Vec::new();
            };
            serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|value| value["Volumes"].as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|item| item["Name"].as_str())
                .filter(|name| !name.is_empty() && name.len() <= 255)
                .map(str::to_owned)
                .take(MAX_DROP_OBJECTS)
                .collect()
        }

        /// Removes each object (a few at a time); the number that went.
        fn remove_all(
            engine: &Engine,
            items: &[String],
            path: impl Fn(&str) -> String + Sync,
        ) -> u32 {
            let removed = std::sync::atomic::AtomicU32::new(0);
            let next = std::sync::atomic::AtomicUsize::new(0);
            std::thread::scope(|scope| {
                for _ in 0..items.len().min(DROP_WORKERS) {
                    scope.spawn(|| {
                        loop {
                            let at = next.fetch_add(1, Ordering::Relaxed);
                            let Some(item) = items.get(at) else { return };
                            match engine.call("DELETE", &path(item), None) {
                                Ok(_) => {
                                    removed.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(error) if error.is_not_found() => {}
                                Err(_) => {}
                            }
                        }
                    });
                }
            });
            removed.load(Ordering::Relaxed)
        }

        /// Verifies the lane files are what prepare returned, then runs `up`.
        fn start_stack(
            &self,
            session: &Session,
            lane_dir: &str,
            stack: &StackState,
        ) -> Result<(), String> {
            let shown = text(&stack.project, 80);
            for (name, expected) in [
                (OVERRIDE_NAME, &stack.expect_override),
                (ENV_NAME, &stack.expect_env),
            ] {
                let Some(expected) = expected else { continue };
                match fsops::read_file_at(&stack.dir, name, MAX_ENV_BYTES) {
                    Ok(bytes) if &sha256_hex(&bytes) == expected => {}
                    Ok(_) => {
                        return Err(format!(
                            "stack '{shown}' not started: the lane's {name} is not what restore asked for"
                        ));
                    }
                    Err(error) => {
                        return Err(format!(
                            "stack '{shown}' not started: the lane's {name} is missing ({})",
                            describe(&error)
                        ));
                    }
                }
            }
            let dir_path = stack_path(lane_dir, &stack.rel);
            let wait = (self.config.up_timeout.as_secs().saturating_sub(15)).max(10);
            let args: Vec<String> = [
                "compose",
                "up",
                "-d",
                "--wait",
                "--wait-timeout",
                &wait.to_string(),
                "--quiet-pull",
            ]
            .map(str::to_owned)
            .to_vec();
            let output = self.compose(
                session,
                &stack.dir,
                &dir_path,
                &args,
                self.config.up_timeout,
            );
            if output.ok() {
                Ok(())
            } else {
                Err(format!(
                    "stack '{shown}' did not start: {}",
                    output.reason()
                ))
            }
        }

        /// The fixed post-restore check: every expected service has a container
        /// of the fork's project in the lane's scope, running, and healthy where
        /// it has a healthcheck (a one-shot service that exited 0 is fine). No
        /// project script runs.
        fn check(&self, workspace: &str, stacks: &[StackState]) -> Value {
            let mut problems: Vec<String> = Vec::new();
            let mut seen: Vec<Value> = Vec::new();
            let containers: Vec<ContainerSummary> = match self.engine.list_containers(true) {
                Ok(list) => list,
                Err(error) => {
                    return json!({"ok": false, "containers": [], "problems": [format!("the engine could not be asked: {error}")]});
                }
            };
            let mut inspected = 0;
            for stack in stacks {
                for service in &stack.services {
                    let mine: Vec<&ContainerSummary> = containers
                        .iter()
                        .filter(|c| {
                            c.label(SCOPE_LABEL) == Some(workspace)
                                && c.label("com.docker.compose.project")
                                    == Some(stack.fork_project.as_str())
                                && c.label("com.docker.compose.service") == Some(service.as_str())
                        })
                        .collect();
                    if mine.is_empty() {
                        problems.push(format!("service '{}' has no container", text(service, 60)));
                        continue;
                    }
                    for container in mine {
                        inspected += 1;
                        if inspected > 64 {
                            break;
                        }
                        let (status, health, exit) = self.inspect_state(&container.id);
                        let good = match status.as_str() {
                            "running" => health.as_deref().is_none_or(|h| h == "healthy"),
                            "exited" => exit == Some(0),
                            _ => false,
                        };
                        if !good {
                            problems.push(format!(
                                "service '{}' is {}{}",
                                text(service, 60),
                                status,
                                health
                                    .as_deref()
                                    .map(|h| format!(" ({h})"))
                                    .unwrap_or_default()
                            ));
                        }
                        seen.push(json!({
                            "service": text(service, 60),
                            "state": status,
                            "health": health,
                        }));
                    }
                }
            }
            let ok = problems.is_empty() && (!seen.is_empty() || stacks.is_empty());
            json!({"ok": ok, "containers": seen, "problems": problems})
        }

        fn inspect_state(&self, id: &str) -> (String, Option<String>, Option<i64>) {
            if !crate::engine_api::valid_container_id(id) {
                return ("unknown".into(), None, None);
            }
            let Ok(body) = self
                .engine
                .call("GET", &format!("/containers/{id}/json"), None)
            else {
                return ("unknown".into(), None, None);
            };
            let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            (
                value["State"]["Status"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_owned(),
                value["State"]["Health"]["Status"]
                    .as_str()
                    .map(str::to_owned),
                value["State"]["ExitCode"].as_i64(),
            )
        }
    }

    /// Test faults (C3, `fault-restore-*`): the agent answers `restore_prepare`
    /// as a hostile VM would, so the Mac side's validation (E1-R2: every field
    /// of the result is checked before a file is written) is shown at work.
    /// Compiled to nothing without `test-breaks`.
    fn hostile_reply(reply: &mut Value) {
        if break_mode("fault-restore-bad-file-name") {
            reply["files"]["../../escape/.env"] = json!("ESCAPE=1\n");
        }
        if break_mode("fault-restore-huge-file") {
            reply["files"][ENV_NAME] = json!("A".repeat(200 * 1024));
        }
        if break_mode("fault-restore-nul") {
            reply["files"][ENV_NAME] = json!("A=1\u{0}B=2\n");
        }
        if break_mode("fault-restore-bad-port") {
            reply["ports"] = json!({"5432": 70000});
        }
        if break_mode("fault-restore-bad-name") {
            reply["restored"] = json!([{"kind": "service", "service": "db\u{1b}[31m evil\nname",
                "stack": "s", "volumes": ["v"]}]);
        }
        if break_mode("fault-restore-markup") {
            // Display text only: it must come out plain, and the restore still works.
            reply["fresh"] = json!([{"kind": "service", "service": "mail", "stack": "s",
                "reason": "\u{1b}[31mred\u{1b}[0m <b>bold</b>\r\nsecond line [x](http://evil)"}]);
        }
    }

    fn stack_path(lane_dir: &str, rel: &str) -> String {
        if rel.is_empty() {
            lane_dir.to_owned()
        } else {
            format!("{lane_dir}/{rel}")
        }
    }

    fn describe(error: &std::io::Error) -> String {
        match error.raw_os_error() {
            Some(40) => "a symbolic link is in the way".to_owned(),
            Some(18) => "a mount point is in the way".to_owned(),
            Some(2) => "it does not exist".to_owned(),
            Some(20) => "it is not a folder".to_owned(),
            _ => error.to_string(),
        }
    }

    fn remap(
        services: &[(String, Vec<PortSpec>)],
        map: &BTreeMap<u16, u16>,
    ) -> Vec<(String, Vec<PortSpec>)> {
        services
            .iter()
            .map(|(name, ports)| {
                (
                    name.clone(),
                    ports
                        .iter()
                        .map(|port| PortSpec {
                            published: port.published.map(|p| map.get(&p).copied().unwrap_or(p)),
                            ..port.clone()
                        })
                        .collect(),
                )
            })
            .collect()
    }

    fn standalone_name(fork: &str, volume: &SnapVolume) -> String {
        let clean = volume.name.len() <= 80
            && crate::fsops::valid_volume_name(&volume.name)
            && !volume.name.contains('…');
        let name = if clean {
            format!("{fork}-{}", volume.name)
        } else {
            format!("{fork}-{}", volume.id)
        };
        if crate::fsops::valid_volume_name(&name) {
            name
        } else {
            format!("{fork}-{}", volume.id)
        }
    }

    /// One volume being installed: its parent directory, the staging name and
    /// the copy still to run.
    struct Staged {
        parent: OwnedFd,
        stage: String,
        copy: Option<VolumeCopy>,
    }

    /// Stops the copier's workers when the install is over, however it ends.
    struct StopWorkers(Arc<Copier>);

    impl Drop for StopWorkers {
        fn drop(&mut self) {
            self.0.shutdown();
        }
    }

    enum VolumeSource {
        Snapshot(String),
        /// Deliberately broken `restore-from-live`: the original's own volume.
        Live(String),
    }

    fn source_for(snapshot_id: &str, live_name: &str, live: bool) -> VolumeSource {
        if live {
            VolumeSource::Live(live_name.to_owned())
        } else {
            VolumeSource::Snapshot(snapshot_id.to_owned())
        }
    }

    struct Session {
        run: RunDir,
        socket: PathBuf,
        engine: Engine,
        lane: OwnedFd,
        uid: u32,
        gid: u32,
    }

    struct StackDir {
        fd: OwnedFd,
        path: String,
    }

    /// A stack that is not restored, and why. `failure` marks the reasons that
    /// are errors (its files are missing or cannot be read), as opposed to
    /// limits (fork-unsafe features, project types restore does not handle):
    /// only errors make the restore's `ok` false.
    struct Refused {
        gaps: Vec<String>,
        fresh: Vec<Value>,
        failure: bool,
    }

    struct PlannedVolume {
        key: String,
        fork_name: String,
        snapshot: String,
        /// The original's engine name (display data; only the broken
        /// `restore-from-live` uses it, after the usual name validation).
        live_name: String,
        expected: u64,
    }

    struct PlannedStack {
        rel: String,
        dir: OwnedFd,
        project: String,
        fork_project: String,
        files: Vec<String>,
        services_all: Vec<String>,
        services: Vec<(String, Vec<PortSpec>)>,
        volumes: Vec<PlannedVolume>,
        restored: Vec<Value>,
        fresh: Vec<Value>,
        gaps: Vec<String>,
        created: Vec<String>,
        failed: Vec<String>,
    }

    struct StandalonePlan {
        fork_name: String,
        display: String,
        users: Vec<String>,
    }

    enum JobOwner {
        Stack(usize),
        Standalone(usize),
    }

    // ------------------------------------------------------------ the helper process

    /// `marsh --internal-services restore-client`: confines this process, drops
    /// to an unprivileged user and execs `docker`. Reads one JSON document from
    /// stdin. Never returns on success.
    #[must_use]
    pub fn client_main(args: &[String]) -> i32 {
        match args {
            [] => {}
            #[cfg(feature = "test-breaks")]
            [flag, value] if flag == "--break-modes" => {
                let modes: Vec<String> = value
                    .split(',')
                    .filter(|m| !m.is_empty())
                    .map(str::to_owned)
                    .collect();
                crate::breaks::install(&modes);
            }
            [flag, ..] => {
                eprintln!("marsh: restore-client: unknown argument {flag}");
                return 2;
            }
        }
        let mut input = Vec::new();
        if std::io::stdin()
            .take(64 * 1024)
            .read_to_end(&mut input)
            .is_err()
        {
            eprintln!("marsh: restore-client: cannot read its request");
            return 2;
        }
        let Ok(spec) = serde_json::from_slice::<ClientSpec>(&input) else {
            eprintln!("marsh: restore-client: malformed request");
            return 2;
        };
        if let Err(error) = std::env::set_current_dir(&spec.cwd_path) {
            eprintln!("marsh: restore-client: cannot enter the folder: {error}");
            return 126;
        }
        client_hold_announce();
        if spec.confine
            && let Err(why) = confine(&spec)
        {
            eprintln!("marsh: the compose client cannot be confined: {why}");
            return 126;
        }
        if let Err(error) = drop_privileges(spec.uid, spec.gid) {
            eprintln!("marsh: the compose client cannot drop privileges: {error}");
            return 126;
        }
        if rustix::process::getuid().as_raw() == 0 {
            eprintln!("marsh: the compose client would run as root; refusing");
            return 126;
        }
        client_hold_wait();
        let docker = if Path::new(DOCKER).exists() {
            DOCKER
        } else {
            "docker"
        };
        let error = Command::new(docker)
            .args(&spec.argv)
            .env_clear()
            .envs(&spec.env)
            .exec();
        eprintln!("marsh: cannot run {docker}: {error}");
        127
    }

    /// Test barrier (harness): with `hold-restore-client-running` the helper
    /// announces itself while it is still root and unconfined (it cannot
    /// write the barrier file afterwards) ...
    fn client_hold_announce() {
        #[cfg(feature = "test-breaks")]
        if crate::breaks::break_mode("hold-restore-client-running") {
            let dir = Path::new("/run/marsh-test-holds");
            let _ = std::fs::create_dir_all(dir);
            let _ = std::fs::write(dir.join("restore-client-running.reached"), b"");
        }
    }

    /// ... and then, already unprivileged and confined, waits for the release,
    /// so a harness can look at its user, capabilities, environment and socket.
    /// Compiled out of release builds.
    fn client_hold_wait() {
        #[cfg(feature = "test-breaks")]
        if crate::breaks::break_mode("hold-restore-client-running") {
            let release = Path::new("/run/marsh-test-holds/restore-client-running.release");
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline && !release.exists() {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    fn drop_privileges(uid: u32, gid: u32) -> std::io::Result<()> {
        use rustix::fs::{Gid, Uid};
        rustix::thread::set_no_new_privs(true)?;
        rustix::thread::set_thread_groups(&[])?;
        rustix::thread::set_thread_res_gid(
            Some(Gid::from_raw(gid)),
            Some(Gid::from_raw(gid)),
            Some(Gid::from_raw(gid)),
        )?;
        rustix::thread::set_thread_res_uid(
            Some(Uid::from_raw(uid)),
            Some(Uid::from_raw(uid)),
            Some(Uid::from_raw(uid)),
        )?;
        Ok(())
    }

    /// Landlock: the lane folder is read-only, the system's files are readable,
    /// the private scratch directories are writable, nothing else is reachable,
    /// and no TCP connection or bind is possible. Fails closed: a kernel that
    /// cannot enforce all of it refuses.
    fn confine(spec: &ClientSpec) -> Result<(), String> {
        use landlock::{
            ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, PathBeneath, PathFd,
            Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, make_bitflags,
            path_beneath_rules,
        };
        let abi = ABI::V4;
        let all = AccessFs::from_all(abi);
        let read = AccessFs::from_read(abi);
        let lane_read = make_bitflags!(AccessFs::{ReadFile | ReadDir});
        let device_write = make_bitflags!(AccessFs::{ReadFile | WriteFile | Truncate});
        let device_read = make_bitflags!(AccessFs::{ReadFile});
        let mut created = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(all)
            .and_then(|r| r.handle_access(AccessNet::from_all(abi)))
            .and_then(landlock::Ruleset::create)
            .map_err(|e| e.to_string())?;
        let lane = PathFd::new(&spec.lane_path).map_err(|e| e.to_string())?;
        created = created
            .add_rule(PathBeneath::new(lane, lane_read))
            .map_err(|e| e.to_string())?;
        created = created
            .add_rules(path_beneath_rules(
                ["/usr", "/etc", "/lib", "/lib64", "/bin", "/sbin", "/proc"],
                read,
            ))
            .map_err(|e| e.to_string())?;
        created = created
            .add_rules(path_beneath_rules(&spec.scratch, all))
            .map_err(|e| e.to_string())?;
        created = created
            .add_rules(path_beneath_rules(["/dev/null"], device_write))
            .map_err(|e| e.to_string())?;
        created = created
            .add_rules(path_beneath_rules(
                ["/dev/urandom", "/dev/zero"],
                device_read,
            ))
            .map_err(|e| e.to_string())?;
        let status = created.restrict_self().map_err(|e| e.to_string())?;
        if status.ruleset == RulesetStatus::FullyEnforced {
            Ok(())
        } else {
            Err("the kernel enforces only part of the confinement".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fork_and_path_validation() {
        for ok in ["fork", "fork-1_a", "f", "a".repeat(48).as_str()] {
            assert!(valid_fork(ok), "{ok}");
        }
        for bad in [
            "",
            "Fork",
            "-x",
            "_x",
            "a b",
            "a/b",
            "a.b",
            "é",
            "x\n",
            &"a".repeat(49),
        ] {
            assert!(!valid_fork(bad), "{bad:?}");
        }
        assert_eq!(
            lane_components("/p/root", "/p/root/.lanes/a").unwrap(),
            vec![".lanes", "a"]
        );
        for (project, lane) in [
            ("/p/root", "/p/root"),
            ("/p/root", "/p/rootx/a"),
            ("/p/root", "/p/other/a"),
            ("/p/root", "/p/root/../x"),
            ("/p/root", "/p/root/a/./b"),
            ("/p/root", "/p/root//a"),
            ("/p/root", "p/root/a"),
            ("/p/root", "/p/root/a\0b"),
            ("/p/root", "/p/root/a\nb"),
            ("/p/../root", "/p/../root/a"),
        ] {
            assert!(
                lane_components(project, lane).is_err(),
                "{project} {lane:?}"
            );
        }
    }

    #[test]
    fn override_is_built_from_validated_values() {
        let ports = vec![
            (
                "db".to_owned(),
                vec![PortSpec {
                    target: 5432,
                    protocol: "tcp".into(),
                    published: Some(15432),
                    host_ip: "127.0.0.1".into(),
                }],
            ),
            (
                "odd \"name\"\nx: y".to_owned(),
                vec![PortSpec {
                    target: 53,
                    protocol: "udp".into(),
                    published: None,
                    host_ip: "127.0.0.1".into(),
                }],
            ),
        ];
        let text = override_yaml("fork-a", &ports);
        assert_eq!(
            text,
            "name: \"fork-a\"\nservices:\n  \"db\":\n    ports: !override\n      - target: 5432\n        protocol: \"tcp\"\n        published: \"15432\"\n        host_ip: \"127.0.0.1\"\n  \"odd \\\"name\\\"\\nx: y\":\n    ports: !override\n      - target: 53\n        protocol: \"udp\"\n"
        );
        assert_eq!(override_yaml("f", &[]), "name: \"f\"\nservices: {}\n");
    }

    #[test]
    fn env_rewrites_only_localhost_ports_it_knows() {
        let map = BTreeMap::from([(5432_u16, 15432_u16), (6379, 16379)]);
        for (input, want) in [
            (
                "DATABASE_URL=postgres://u:p@localhost:5432/app\n",
                "DATABASE_URL=postgres://u:p@localhost:15432/app\n",
            ),
            ("A=127.0.0.1:6379\n", "A=127.0.0.1:16379\n"),
            ("A=localhost:54321\n", "A=localhost:54321\n"),
            ("A=mylocalhost:5432\n", "A=mylocalhost:5432\n"),
            ("A=db:5432\n", "A=db:5432\n"),
            ("A=localhost:5432x\n", "A=localhost:5432x\n"),
            (
                "A=localhost:5432,localhost:6379\n",
                "A=localhost:15432,localhost:16379\n",
            ),
            ("A=localhost:9999\n", "A=localhost:9999\n"),
            ("# é localhost:5432\n", "# é localhost:15432\n"),
        ] {
            assert_eq!(rewrite_ports(input, &map), want, "{input:?}");
        }
        let env = env_content(
            "FOO=1\nCOMPOSE_PROJECT_NAME=old\nCOMPOSE_FILE=a.yaml\nURL=localhost:5432",
            &map,
            "fork-a",
            "compose.yaml:compose.fork.yaml",
        );
        assert_eq!(
            env,
            "FOO=1\nURL=localhost:15432\nCOMPOSE_PROJECT_NAME=fork-a\nCOMPOSE_FILE=compose.yaml:compose.fork.yaml\n"
        );
        assert_eq!(
            env_content("", &map, "f", "c.yaml"),
            "COMPOSE_PROJECT_NAME=f\nCOMPOSE_FILE=c.yaml\n"
        );
    }

    #[test]
    fn blockers_name_each_object() {
        let config = json!({
            "services": {
                "fixed": {"container_name": "nfx-fixed"},
                "mail": {"ports": [{"target": 25, "published": "2500-2510"}]},
                "ok": {"ports": [{"target": 80, "published": "8080"}]},
            },
            "volumes": {
                "ext": {"name": "nfx-ext", "external": true},
                "named": {"name": "shared-name"},
                "own": {"name": "proj_own"},
            },
            "networks": {
                "default": {"name": "proj_default"},
                "outer": {"name": "outer-net", "external": true},
            },
        });
        let found = blockers(&config, "proj");
        let all = found.join("\n");
        for needle in ["nfx-fixed", "mail", "nfx-ext", "shared-name", "outer-net"] {
            assert!(all.contains(needle), "{needle} in {all}");
        }
        assert!(!all.contains("proj_own") && !all.contains("'own'"), "{all}");
        assert!(!all.contains("'ok'") && !all.contains("'default'"), "{all}");
        assert_eq!(found.len(), 5, "{all}");
        assert_eq!(blockers(&json!({"services": {"a": {}}}), "p").len(), 0);
    }
}
