//! The Docker API policy (B1, B2, A3, A4, A5).
//!
//! Request flow: [`http`] parses strictly, [`routes::classify`] maps the
//! target onto the allowlist, the endpoint reads the body the route allows,
//! [`admit::admit`] validates it against a closed schema and produces the one
//! canonical request that is sent upstream. Nothing the client sent is
//! forwarded as bytes.
// Pedantic lints this module trades for readability: the HTTP and policy code is
// long, table-like and full of one-letter names for a request, a head and a
// connection.
#![allow(
    clippy::missing_errors_doc,
    clippy::many_single_char_names,
    clippy::too_many_lines,
    clippy::format_push_string,
    clippy::collapsible_if,
    clippy::collapsible_else_if
)]

pub mod admit;
pub mod create;
pub mod docker;
pub mod endpoint;
#[cfg(test)]
mod fuzz_tests;
pub mod http;
pub mod json;
pub mod routes;
pub mod schema;
#[cfg(test)]
mod tests;

use std::path::PathBuf;

/// Who is asking. Supplied by the caller (in production the relay, from the
/// host's own state); never derived from request contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authority {
    pub project_root: PathBuf,
    pub workspace: String,
    pub attempt: String,
}

impl Authority {
    /// Workspace and attempt end up in labels and in paths.
    pub fn validate(&self) -> Result<(), String> {
        let ok = |s: &str| {
            !s.is_empty()
                && s.len() <= 64
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                && !s.starts_with('.')
        };
        if !ok(&self.workspace) {
            return Err(format!("invalid workspace id {:?}", self.workspace));
        }
        if !ok(&self.attempt) {
            return Err(format!("invalid attempt id {:?}", self.attempt));
        }
        if !self.project_root.is_absolute() {
            return Err("project root must be absolute".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub msg: String,
    /// The stream can't be kept in sync after this answer.
    pub close: bool,
}

impl Refusal {
    #[must_use]
    pub fn forbid(msg: impl Into<String>) -> Self {
        Self {
            status: 403,
            msg: msg.into(),
            close: false,
        }
    }

    #[must_use]
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: 404,
            msg: msg.into(),
            close: false,
        }
    }

    #[must_use]
    pub fn closing(mut self) -> Self {
        self.close = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// Full ID of an object in the caller's scope.
    Own(String),
    /// Unknown, or owned by another scope (deliberately indistinguishable).
    Missing,
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeScope {
    Own,
    Foreign,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct ResourceLimits {
    pub default_memory: i64,
    pub max_memory: i64,
    pub default_nano_cpus: i64,
    pub max_nano_cpus: i64,
    pub default_pids: i64,
    pub max_pids: i64,
    pub max_shm: i64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        const GIB: i64 = 1 << 30;
        Self {
            default_memory: 4 * GIB,
            max_memory: 64 * GIB,
            default_nano_cpus: 4_000_000_000,
            max_nano_cpus: 32_000_000_000,
            default_pids: 4096,
            max_pids: 65_536,
            max_shm: 8 * GIB,
        }
    }
}

/// What an owned container's limits are right now (`HostConfig`).
#[derive(Debug, Clone, Copy, Default)]
pub struct CurrentLimits {
    pub memory: i64,
    pub memory_swap: i64,
    pub nano_cpus: i64,
    pub cpu_quota: i64,
    pub cpu_period: i64,
    pub pids: i64,
}

#[derive(Debug, Clone)]
pub struct PinRequest {
    /// Absolute, as the client wrote it.
    pub source: String,
    pub read_only: bool,
    /// `-v` semantics: Docker creates a missing source directory.
    pub create_missing: bool,
}

#[derive(Debug, Clone)]
pub struct PinnedBind {
    /// The path dockerd must mount instead.
    pub path: String,
    /// The mount must be read-only whatever the client asked.
    pub force_read_only: bool,
    /// Pin to release if the request fails, and to record on success.
    pub pin_id: Option<String>,
}

/// Everything the policy needs from the agent. Production: the ledger, the pin
/// manager and the per-scope socket server. Tests: a fake.
pub trait Env {
    fn authority(&self) -> &Authority;
    fn limits(&self) -> &ResourceLimits;
    fn pin_bind(&self, req: &PinRequest) -> Result<PinnedBind, Refusal>;
    fn release_pins(&self, ids: &[String]);
    /// The substituted socket for `/var/run/docker.sock` binds (A3.1).
    fn substituted_socket(&self) -> Result<String, Refusal>;
    /// The scope's default bridge network (DECISIONS 14); created on demand.
    fn scope_network(&self) -> Result<String, Refusal>;
    fn container(&self, reference: &str) -> Lookup;
    /// Does the container hold a pin that restore could not recreate?
    fn container_blocked(&self, container_id: &str) -> bool;
    /// The pins an owned container holds, for a container that mounts its
    /// volumes (`VolumesFrom`): they join the new container's lease.
    fn container_pins(&self, container_id: &str) -> Vec<String>;
    /// Volume mount points the image declares (`VOLUME` in a Dockerfile).
    fn image_volumes(&self, image: &str) -> Vec<String>;
    /// The limits an owned container has now, for validating an update.
    fn current_limits(&self, container_id: &str) -> Result<CurrentLimits, Refusal>;
    fn exec_container(&self, exec_id: &str) -> Lookup;
    fn network(&self, reference: &str) -> Lookup;
    fn volume(&self, name: &str) -> VolumeScope;
    /// Claim `name` for the caller, atomically, and create it (labelled) if the
    /// engine does not have it. Refused when another scope holds the name or
    /// the engine has an object that nobody owns.
    fn ensure_volume(&self, name: &str) -> Result<(), Refusal>;
    /// Reserve a name for an explicit `POST /volumes/create`. `Ok(true)` when
    /// this call made the reservation (and must give it back on failure).
    fn reserve_volume(&self, name: &str) -> Result<bool, Refusal>;
    fn release_volume(&self, name: &str);
    fn registry_allowed(&self, image: &str) -> bool;
    /// Extra bind-source paths that are the caller's own endpoint (for example
    /// `/run/marsh/docker.sock`, which Testcontainers mounts into Ryuk). Each is
    /// substituted like `/var/run/docker.sock`. Empty unless the wiring sets it.
    fn socket_aliases(&self) -> &[String];
}
