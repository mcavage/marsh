//! Validated contracts shared by the local daemon, worker, and container runtime.
//!
//! These types describe one admitted local container job. Placement and future
//! Cloud scheduling contracts remain outside this bounded crate.

#[cfg(unix)]
pub mod byte_path;
pub mod command_registry;
pub mod execution;
pub mod process;
pub use execution::{ExecutionOutcome, ResourceLimit, SetupStage, WORKER_CONTAINER_CAPACITY};

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

/// Shell-exported values transported without UTF-8 normalization. Names retain
/// the bounded ASCII policy checked by [`validate_exported_environment`]; values
/// may contain any non-NUL bytes. This is execution data, not diagnostic text.
/// The wire uses JSON string names and byte-array values.
pub type ExportedEnvironment = BTreeMap<String, Vec<u8>>;

/// Immutable OCI image identity used by a job. Mutable tags are rejected.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct OciImage(String);

impl OciImage {
    /// Validates a bare image digest or a repository qualified digest.
    ///
    /// # Errors
    /// Returns [`JobSpecError::MutableImage`] unless the reference contains an
    /// exact lowercase SHA-256 digest.
    pub fn parse(value: impl Into<String>) -> Result<Self, JobSpecError> {
        let value = value.into();
        let digest = value.strip_prefix("sha256:").or_else(|| {
            let (repository, digest) = value.rsplit_once("@sha256:")?;
            valid_repository(repository).then_some(digest)
        });
        if digest.is_none_or(|digest| {
            digest.len() != 64
                || !digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        }) {
            return Err(JobSpecError::MutableImage);
        }
        Ok(Self(value))
    }

    /// Returns the runtime image reference.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Read/write authority for one exact job mount.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MountAccess {
    /// Workload can only read the mounted tree.
    ReadOnly,
    /// Workload can read and modify the mounted tree.
    ReadWrite,
}

/// One exact source-to-target mount granted to a job.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JobMount {
    pub source: PathBuf,
    pub target: PathBuf,
    pub access: MountAccess,
    /// Bind only this relative directory of the prepared grant (a split
    /// branch's workspace or Git directory). The runtime resolves it without
    /// following symlinks. Several mounts may share one grant source only
    /// when every one of them names a subpath.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subpath: Option<PathBuf>,
}

impl JobMount {
    /// Whether `subpath` is a nonempty relative path of plain components.
    #[must_use]
    pub fn subpath_is_valid(&self) -> bool {
        self.subpath.as_deref().is_none_or(|subpath| {
            subpath.to_str().is_some()
                && subpath.components().count() > 0
                && subpath.components().count() <= 64
                && subpath
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JobIdentity {
    pub uid: u32,
    pub gid: u32,
}

/// Enforced per-job container resources.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JobResources {
    pub cpu_millis: u32,
    pub memory_bytes: u64,
    pub pids: u32,
    /// Maximum bytes written to the container's private writable layer.
    pub writable_bytes: u64,
    /// Maximum combined bytes forwarded from stdout and stderr.
    pub output_bytes: u64,
    /// Maximum elapsed execution time enforced by the worker supervisor.
    pub wall_seconds: u64,
}

/// Terminal dimensions accepted by an attached job.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalSize {
    pub rows: u16,
    pub columns: u16,
}

/// Signals supported by the placement-neutral job lifecycle.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobSignal {
    Interrupt,
    Terminate,
    Kill,
    Hangup,
}

/// Frozen execution input shared by local and future Cloud runtimes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JobSpec {
    pub image: OciImage,
    /// Caller-supplied argument tail. The workload image's OCI entrypoint and
    /// command remain authoritative, and Unix arguments need not be UTF-8.
    pub argv: Vec<Vec<u8>>,
    /// Explicit shell-session identity; it does not change the image's
    /// entrypoint, command, or static environment.
    pub identity: JobIdentity,
    /// Narrow natural-home binding supplied by the trusted daemon.
    pub session_environment: BTreeMap<String, String>,
    /// Exported values from the invoking shell, after placement policy.
    #[serde(default)]
    pub exported_environment: ExportedEnvironment,
    #[cfg_attr(unix, serde(with = "byte_path"))]
    pub working_directory: PathBuf,
    pub mounts: Vec<JobMount>,
    pub resources: JobResources,
    pub terminal: bool,
    pub terminal_size: Option<TerminalSize>,
    /// Split capability socket directory for a split branch's job: the backend
    /// sets the root (`/run/marsh-cap`), the worker the per-attempt directory
    /// it binds at `/run/marsh` (`docs/design/workspaces.md` s4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split_capability: Option<PathBuf>,
    /// The job capability the worker publishes at `/run/marsh`
    /// (`docs/design/processes.md` s4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<process::JobCapability>,
}

/// A read-only subpath bind may cover part of the same grant mounted whole
/// (`.marsh` for an ordinary job).
fn read_only_overlay(whole: &JobMount, part: &JobMount) -> bool {
    whole.source == part.source
        && whole.subpath.is_none()
        && part.access == MountAccess::ReadOnly
        && part
            .subpath
            .as_ref()
            .is_some_and(|subpath| whole.target.join(subpath) == part.target)
}

impl JobSpec {
    /// Validates the immutable request without consulting a particular runtime.
    ///
    /// # Errors
    /// Returns the matching [`JobSpecError`] for malformed or authority-bearing
    /// execution input.
    pub fn validate(&self) -> Result<(), JobSpecError> {
        OciImage::parse(self.image.as_str())?;
        if self.argv.len() > 256
            || self
                .argv
                .iter()
                .any(|argument| argument.len() > 16 * 1024 || argument.contains(&0))
        {
            return Err(JobSpecError::InvalidArguments);
        }
        if self.identity.uid == 0 || self.identity.gid == 0 {
            return Err(JobSpecError::RootIdentity);
        }
        if !matches!(
            (self.terminal, self.terminal_size),
            (false, None)
                | (
                    true,
                    Some(TerminalSize {
                        rows: 1..,
                        columns: 1..
                    })
                )
        ) {
            return Err(JobSpecError::InvalidTerminalSize);
        }
        if self.session_environment.len() != 4
            || !matches!(self.session_environment.get("HOME"), Some(home) if safe_absolute(Path::new(home)))
            || self.session_environment.get("MARSH_SELECTED_HOME")
                != self.session_environment.get("HOME")
            || self.session_environment.get("USER") != self.session_environment.get("LOGNAME")
            || self.session_environment["USER"].is_empty()
            || !self.session_environment["USER"]
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(JobSpecError::InvalidSessionEnvironment);
        }
        validate_exported_environment(&self.exported_environment)?;
        if self.resources.cpu_millis == 0
            || self.resources.memory_bytes == 0
            || self.resources.pids == 0
            || self.resources.writable_bytes == 0
            || self.resources.output_bytes == 0
            || self.resources.wall_seconds == 0
        {
            return Err(JobSpecError::InvalidResources);
        }
        if !safe_absolute(&self.working_directory) {
            return Err(JobSpecError::InvalidWorkingDirectory);
        }
        if self.mounts.is_empty()
            || self.mounts.len() > 32
            || self.mounts.iter().any(|mount| {
                !safe_absolute(&mount.source)
                    || !safe_absolute(&mount.target)
                    || !prepared_grant_source(&mount.source)
                    || reserved_runtime_target(&mount.target)
                    || mount.source.to_str().is_none()
                    || mount.target.to_str().is_none()
                    || !mount.subpath_is_valid()
            })
        {
            return Err(JobSpecError::InvalidMount);
        }
        let mut targets = BTreeSet::new();
        if self
            .mounts
            .iter()
            .any(|mount| !targets.insert(&mount.target))
        {
            return Err(JobSpecError::DuplicateMountTarget);
        }
        for (index, mount) in self.mounts.iter().enumerate() {
            if self.mounts[index + 1..].iter().any(|other| {
                let shared_subpaths = mount.source == other.source
                    && mount.subpath.is_some()
                    && other.subpath.is_some();
                !shared_subpaths
                    && !read_only_overlay(mount, other)
                    && !read_only_overlay(other, mount)
                    && (mount.source.starts_with(&other.source)
                        || other.source.starts_with(&mount.source))
            }) {
                return Err(JobSpecError::OverlappingMountSource);
            }
        }
        if !self
            .mounts
            .iter()
            .any(|mount| self.working_directory.starts_with(&mount.target))
        {
            return Err(JobSpecError::UnauthorizedWorkingDirectory);
        }
        Ok(())
    }
}

fn valid_repository(repository: &str) -> bool {
    !repository.is_empty()
        && !repository.starts_with('-')
        && !repository.contains('@')
        && repository.split('/').all(|component| {
            component
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
                && !matches!(component, "." | "..")
        })
        && repository.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
        })
}

fn safe_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path != Path::new("/")
        && path
            .components()
            .all(|component| !matches!(component, Component::CurDir | Component::ParentDir))
}

fn prepared_grant_source(path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix("/run/marsh/grants") else {
        return false;
    };
    // Sources name one exact grant beneath one per-attempt directory. This is
    // only the lexical boundary; worker grant preparation must also verify the
    // directory's type, ownership, and stable filesystem identity.
    relative.components().count() == 2
}

fn reserved_runtime_target(path: &Path) -> bool {
    [
        Path::new("/var/run/docker.sock"),
        Path::new("/run/docker.sock"),
        Path::new("/run/containerd"),
        Path::new("/var/run/containerd"),
        Path::new("/run/podman"),
        Path::new("/var/run/podman"),
        Path::new("/run/crio"),
        Path::new("/var/run/crio"),
        Path::new("/run/sbx"),
        Path::new("/var/run/sbx"),
        Path::new("/run/marsh"),
        Path::new("/var/run/marsh"),
    ]
    .iter()
    .any(|forbidden| path == *forbidden || path.starts_with(forbidden))
}

/// Names reserved for the job identity or the sandbox control/proxy boundary.
#[must_use]
pub fn reserved_exported_environment_name(name: &str) -> bool {
    matches!(
        name,
        "HOME"
            | "USER"
            | "LOGNAME"
            | "HTTP_PROXY"
            | "HTTPS_PROXY"
            | "ALL_PROXY"
            | "NO_PROXY"
            | "http_proxy"
            | "https_proxy"
            | "all_proxy"
            | "no_proxy"
            | "NODE_EXTRA_CA_CERTS"
            | "SSL_CERT_FILE"
            | "REQUESTS_CA_BUNDLE"
            | "NODE_USE_ENV_PROXY"
            | "MCP_GATEWAY_URL"
            | "MCP_SENTINEL_TOKEN_NAME"
    ) || ["MARSH_", "SBX_", "DOCKER_", "CONTAINERD_"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// A shell VM endpoint or search path that is not portable into a peer job
/// VM. `PATH` names the shell VM's filesystem; the job keeps its image's own.
#[must_use]
pub fn placement_bound_environment_name(name: &str) -> bool {
    matches!(
        name,
        "PATH"
            | "SSH_AUTH_SOCK"
            | "SSH_AGENT_PID"
            | "GPG_AGENT_INFO"
            | "DBUS_SESSION_BUS_ADDRESS"
            | "XDG_RUNTIME_DIR"
            | "DISPLAY"
            | "WAYLAND_DISPLAY"
    )
}

/// Bounds exported shell values before they cross into a worker request.
///
/// # Errors
/// Rejects malformed names, protected controls, or oversized values without
/// including the values in the diagnostic.
pub fn validate_exported_environment(
    environment: &ExportedEnvironment,
) -> Result<(), JobSpecError> {
    if environment.len() > 256 {
        return Err(JobSpecError::InvalidExportedEnvironment);
    }
    let mut total = 0usize;
    for (name, value) in environment {
        if name.is_empty()
            || name.len() > 128
            || !name.bytes().enumerate().all(|(index, byte)| {
                byte == b'_'
                    || byte.is_ascii_alphabetic()
                    || (index > 0 && (byte.is_ascii_digit() || matches!(byte, b'-' | b'.')))
            })
            || reserved_exported_environment_name(name)
            || value.len() > 16 * 1024
            || value.contains(&0)
        {
            return Err(JobSpecError::InvalidExportedEnvironment);
        }
        total = total.saturating_add(name.len() + value.len());
        if total > 64 * 1024 {
            return Err(JobSpecError::InvalidExportedEnvironment);
        }
    }
    Ok(())
}

/// Invalid immutable job request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobSpecError {
    MutableImage,
    InvalidArguments,
    InvalidTerminalSize,
    RootIdentity,
    InvalidSessionEnvironment,
    InvalidExportedEnvironment,
    InvalidResources,
    InvalidWorkingDirectory,
    InvalidMount,
    DuplicateMountTarget,
    OverlappingMountSource,
    UnauthorizedWorkingDirectory,
}

impl fmt::Display for JobSpecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::MutableImage => "job image must use an immutable sha256 digest",
            Self::InvalidArguments => "invalid job arguments",
            Self::InvalidTerminalSize => "terminal mode and initial dimensions are inconsistent",
            Self::RootIdentity => "job identity must be non-root",
            Self::InvalidSessionEnvironment => "invalid session environment",
            Self::InvalidExportedEnvironment => "invalid exported environment",
            Self::InvalidResources => "invalid job resources",
            Self::InvalidWorkingDirectory => "invalid job working directory",
            Self::InvalidMount => "invalid or authority-bearing job mount",
            Self::DuplicateMountTarget => "duplicate job mount target",
            Self::OverlappingMountSource => "overlapping job mount sources",
            Self::UnauthorizedWorkingDirectory => {
                "job working directory is outside its mount grants"
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for JobSpecError {}

/// Runtime-returned container identity. It is never derived from a job ID.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ContainerId(String);

impl ContainerId {
    /// Validates a full OCI runtime container ID.
    ///
    /// # Errors
    /// Returns an error unless the runtime supplied 64 lowercase hexadecimal
    /// characters.
    pub fn parse(value: impl Into<String>) -> Result<Self, JobSpecError> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(JobSpecError::InvalidArguments);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> JobSpec {
        JobSpec {
            image: OciImage::parse(format!("registry.example/kit@sha256:{}", "a".repeat(64)))
                .unwrap(),
            argv: vec![b"/usr/local/bin/agent".to_vec()],
            identity: JobIdentity { uid: 501, gid: 20 },
            session_environment: BTreeMap::from([
                ("HOME".into(), "/Users/example".into()),
                ("USER".into(), "example".into()),
                ("LOGNAME".into(), "example".into()),
                ("MARSH_SELECTED_HOME".into(), "/Users/example".into()),
            ]),
            exported_environment: BTreeMap::new(),
            working_directory: "/Users/example/dev/project/subdirectory".into(),
            mounts: vec![
                JobMount {
                    source: "/run/marsh/grants/attempt-1/project".into(),
                    target: "/Users/example/dev/project".into(),
                    access: MountAccess::ReadWrite,
                    subpath: None,
                },
                JobMount {
                    source: "/run/marsh/grants/attempt-1/home".into(),
                    target: "/Users/example".into(),
                    access: MountAccess::ReadWrite,
                    subpath: None,
                },
            ],
            resources: JobResources {
                cpu_millis: 1000,
                memory_bytes: 1024,
                pids: 16,
                writable_bytes: 1024,
                output_bytes: 1024,
                wall_seconds: 60,
            },
            terminal: false,
            terminal_size: None,
            split_capability: None,
            capability: None,
        }
    }

    #[test]
    fn natural_home_and_project_mount_nesting_is_valid() {
        job().validate().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn working_directory_byte_wire_keeps_child_bytes_and_mount_authority_unchanged() {
        use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
        let mut spec = job();
        spec.working_directory
            .push(std::ffi::OsString::from_vec(vec![0xff, 0xfe]));
        spec.validate().unwrap();
        let wire = serde_json::to_value(&spec).unwrap();
        assert!(wire["mounts"][0]["source"].is_string());
        assert!(wire["mounts"][0]["target"].is_string());
        assert!(wire["session_environment"]["HOME"].is_string());
        assert_eq!(
            wire["working_directory"],
            serde_json::json!(spec.working_directory.as_os_str().as_bytes()),
        );
        let restored: JobSpec = serde_json::from_value(wire).unwrap();
        assert_eq!(restored, spec);
        restored.validate().unwrap();
    }

    #[test]
    fn exported_environment_accepts_values_and_rejects_control_names() {
        let mut spec = job();
        spec.exported_environment
            .insert("CARGO_BIN_EXE_tool-name".into(), "present".into());
        spec.exported_environment
            .insert("MY_SETTING".into(), "per-command".into());
        spec.validate().unwrap();
        spec.exported_environment
            .insert("DOCKER_HOST".into(), "unix:///tmp/not-a-grant".into());
        assert_eq!(
            spec.validate(),
            Err(JobSpecError::InvalidExportedEnvironment)
        );
        spec.exported_environment.remove("DOCKER_HOST");
        for name in [
            "MCP_GATEWAY_URL",
            "MCP_SENTINEL_TOKEN_NAME",
            "NODE_USE_ENV_PROXY",
        ] {
            spec.exported_environment
                .insert(name.into(), "untrusted".into());
            assert_eq!(
                spec.validate(),
                Err(JobSpecError::InvalidExportedEnvironment)
            );
            spec.exported_environment.remove(name);
        }
    }

    #[test]
    fn legal_punctuation_in_natural_project_paths_is_valid() {
        let mut spec = job();
        spec.mounts[0].target = "/Users/example/dev/comma,equals=project".into();
        spec.working_directory = spec.mounts[0].target.clone();
        spec.validate().unwrap();
    }

    #[test]
    fn terminal_jobs_require_nonzero_initial_dimensions() {
        let mut request = job();
        request.terminal = true;
        assert_eq!(request.validate(), Err(JobSpecError::InvalidTerminalSize));
        request.terminal_size = Some(TerminalSize {
            rows: 0,
            columns: 120,
        });
        assert_eq!(request.validate(), Err(JobSpecError::InvalidTerminalSize));
        request.terminal_size = Some(TerminalSize {
            rows: 40,
            columns: 120,
        });
        request.validate().unwrap();
    }

    #[test]
    fn selected_home_must_equal_the_validated_natural_home() {
        let mut spec = job();
        spec.session_environment
            .insert("MARSH_SELECTED_HOME".into(), "/other/home".into());
        assert_eq!(
            spec.validate(),
            Err(JobSpecError::InvalidSessionEnvironment)
        );
    }

    #[test]
    fn empty_caller_tail_preserves_native_image_command() {
        let mut spec = job();
        spec.argv.clear();
        spec.validate().unwrap();
    }

    #[test]
    fn writable_layer_and_output_limits_are_required() {
        let mut spec = job();
        spec.resources.writable_bytes = 0;
        assert_eq!(spec.validate(), Err(JobSpecError::InvalidResources));

        let mut spec = job();
        spec.resources.output_bytes = 0;
        assert_eq!(spec.validate(), Err(JobSpecError::InvalidResources));
    }

    #[test]
    fn working_directory_must_be_inside_a_mount_grant() {
        let mut spec = job();
        spec.working_directory = "/ungranted/project".into();
        assert_eq!(
            spec.validate(),
            Err(JobSpecError::UnauthorizedWorkingDirectory)
        );
    }

    #[test]
    fn duplicate_mount_sources_are_rejected() {
        let mut duplicate = job();
        duplicate.mounts[1].source = duplicate.mounts[0].source.clone();
        assert_eq!(
            duplicate.validate(),
            Err(JobSpecError::OverlappingMountSource)
        );
    }

    #[test]
    fn one_grant_may_back_several_subpath_mounts_only() {
        let mut branch = job();
        let project = branch.mounts[0].clone();
        branch.working_directory = project.target.join(".marsh/split/abcd1234/fix/src");
        branch.mounts[0] = JobMount {
            subpath: Some(".marsh/split/abcd1234/fix".into()),
            target: project.target.join(".marsh/split/abcd1234/fix"),
            ..project.clone()
        };
        branch.mounts.push(JobMount {
            subpath: Some(".git".into()),
            target: project.target.join(".git"),
            access: MountAccess::ReadOnly,
            ..project.clone()
        });
        assert_eq!(branch.validate(), Ok(()));

        let mut mixed = branch.clone();
        mixed.mounts[2].subpath = None;
        assert_eq!(mixed.validate(), Err(JobSpecError::OverlappingMountSource));
        for invalid in ["../escape", "/abs", "./a", "a/../b", ""] {
            let mut bad = branch.clone();
            bad.mounts[2].subpath = Some(invalid.into());
            assert_eq!(bad.validate(), Err(JobSpecError::InvalidMount), "{invalid}");
        }
        // The wire omits an absent subpath.
        let wire = serde_json::to_value(&job().mounts[0]).unwrap();
        assert!(wire.get("subpath").is_none());
    }

    #[test]
    fn mount_sources_must_name_one_prepared_per_attempt_grant() {
        for path in [
            "/etc",
            "/run/marsh/grants",
            "/run/marsh/grants/attempt-1",
            "/run/marsh/grants/attempt-1/project/child",
            "/run/marsh/grants-evil/attempt-1/project",
        ] {
            let mut spec = job();
            spec.mounts[0].source = path.into();
            assert_eq!(spec.validate(), Err(JobSpecError::InvalidMount), "{path}");
        }
    }

    #[test]
    fn duplicate_targets_and_lexical_traversal_are_rejected() {
        let mut duplicate_target = job();
        duplicate_target.mounts[1].target = duplicate_target.mounts[0].target.clone();
        assert_eq!(
            duplicate_target.validate(),
            Err(JobSpecError::DuplicateMountTarget)
        );

        let mut source_traversal = job();
        source_traversal.mounts[0].source = "/run/marsh/grants/attempt-1/../docker.sock".into();
        assert_eq!(source_traversal.validate(), Err(JobSpecError::InvalidMount));

        let mut working_directory_traversal = job();
        working_directory_traversal.working_directory =
            "/Users/example/dev/project/../other".into();
        assert_eq!(
            working_directory_traversal.validate(),
            Err(JobSpecError::InvalidWorkingDirectory)
        );
    }

    #[test]
    fn authority_socket_families_are_rejected_on_either_side() {
        for path in [
            "/run/docker.sock",
            "/var/run/containerd/task",
            "/run/podman/podman.sock",
            "/var/run/crio/crio.sock",
            "/run/sbx/control.sock",
            "/var/run/marsh/control.sock",
        ] {
            let mut source = job();
            source.mounts[0].source = path.into();
            assert_eq!(source.validate(), Err(JobSpecError::InvalidMount));

            let mut target = job();
            target.mounts[0].target = path.into();
            target.working_directory = path.into();
            assert_eq!(target.validate(), Err(JobSpecError::InvalidMount));
        }
    }

    #[test]
    fn prepared_worker_grant_sources_are_not_mistaken_for_control_authority() {
        let mut spec = job();
        spec.mounts[0].source = "/run/marsh/grants/attempt-1/project".into();
        spec.mounts[1].source = "/run/marsh/grants/attempt-1/home".into();
        spec.validate().unwrap();
    }

    #[test]
    fn image_reference_cannot_be_a_docker_option_or_malformed_repository() {
        let digest = "a".repeat(64);
        for image in [
            format!("--privileged@sha256:{digest}"),
            format!("@sha256:{digest}"),
            format!("/private/kit@sha256:{digest}"),
            format!("../kit@sha256:{digest}"),
            format!("registry.example/../kit@sha256:{digest}"),
            format!("registry.example//kit@sha256:{digest}"),
            format!("registry.example/UPPER@sha256:{digest}"),
            format!("registry.example/repo@extra@sha256:{digest}"),
        ] {
            assert_eq!(OciImage::parse(image), Err(JobSpecError::MutableImage));
        }
    }
}
