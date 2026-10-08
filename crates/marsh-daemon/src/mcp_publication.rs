//! Narrow host registration adapter for an authenticated project-shell session.

use crate::SessionSpec;
use rustix::process::{Pid, Signal, kill_process_group};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Read,
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
        net::UnixStream,
        process::CommandExt as _,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex, Weak, mpsc},
    thread,
    time::{Duration, Instant},
};

/// A public tool name, deliberately distinct from a registered Kit command.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PublishedName(String);

impl PublishedName {
    /// Validate without normalizing the public name.
    ///
    /// # Errors
    /// Rejects unsafe, oversized, empty and path-component-only names.
    pub fn parse(name: impl Into<String>) -> Result<Self, String> {
        let name = name.into();
        if name.is_empty()
            || name.len() > 64
            || name.starts_with('-')
            || name.bytes().all(|byte| byte == b'.')
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err("published tool name must use 1-64 ASCII letters, numbers, _, - or .; no leading - or dot-only name".into());
        }
        Ok(Self(name))
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for PublishedName {
    type Error = String;
    fn try_from(name: String) -> Result<Self, Self::Error> {
        Self::parse(name)
    }
}
impl From<PublishedName> for String {
    fn from(name: PublishedName) -> Self {
        name.0
    }
}
impl std::fmt::Display for PublishedName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationKind {
    Mcp,
    Acp,
}

/// One project/home namespace. Kind affects declarations/server names, never
/// the admission lock, which is shared across both publication protocols.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationScope {
    kind: PublicationKind,
    key: String,
}
impl PublicationScope {
    /// Inputs must be the canonical project and selected *backing* home, not
    /// the selected-home root. Identity admission remains the caller's duty.
    #[must_use]
    pub fn new(kind: PublicationKind, workspace: &Path, home_backing: &Path) -> Self {
        let mut digest = Sha256::new();
        digest.update(workspace.as_os_str().as_encoded_bytes());
        digest.update([0]);
        digest.update(home_backing.as_os_str().as_encoded_bytes());
        Self {
            kind,
            key: format!("{:x}", digest.finalize()),
        }
    }
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }
    #[must_use]
    pub const fn kind(&self) -> PublicationKind {
        self.kind
    }
    #[must_use]
    pub fn other_protocol(&self) -> Self {
        Self {
            kind: match self.kind {
                PublicationKind::Mcp => PublicationKind::Acp,
                PublicationKind::Acp => PublicationKind::Mcp,
            },
            key: self.key.clone(),
        }
    }
    #[must_use]
    pub fn owner_root(host_home: &Path) -> PathBuf {
        crate::host_state_directory::host_state_root_from_env(host_home)
    }
    #[must_use]
    pub fn directory(&self, host_home: &Path) -> PathBuf {
        self.directory_in(&Self::owner_root(host_home))
    }
    fn directory_in(&self, owner_root: &Path) -> PathBuf {
        owner_root
            .join(match self.kind {
                PublicationKind::Mcp => "published-mcp",
                PublicationKind::Acp => "published-acp",
            })
            .join(&self.key)
    }
    /// Validate the publication layout only; this does not confer authority.
    ///
    /// # Errors
    /// Rejects paths outside the two publication namespaces or malformed keys.
    pub fn from_declaration_path(path: &Path) -> Result<(Self, PathBuf), String> {
        let directory = path.parent().ok_or("publication directory missing")?;
        let key = directory
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("publication scope missing")?;
        if key.len() != 64
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("invalid publication scope key".into());
        }
        let protocol = directory.parent().ok_or("publication kind missing")?;
        let kind = match protocol.file_name().and_then(|name| name.to_str()) {
            Some("published-mcp") => PublicationKind::Mcp,
            Some("published-acp") => PublicationKind::Acp,
            _ => return Err("invalid publication kind directory".into()),
        };
        let owner = protocol
            .parent()
            .ok_or("publication owner missing")?
            .to_owned();
        Ok((
            Self {
                kind,
                key: key.into(),
            },
            owner,
        ))
    }
    #[must_use]
    pub fn declaration_in(&self, owner_root: &Path, name: &PublishedName) -> PathBuf {
        self.directory_in(owner_root).join(format!("{name}.json"))
    }
    #[must_use]
    pub fn lock_path(&self, owner_root: &Path) -> PathBuf {
        owner_root
            .join("publication-locks")
            .join(&self.key)
            .with_extension("lock")
    }
    #[must_use]
    pub fn declaration_path(&self, host_home: &Path, name: &PublishedName) -> PathBuf {
        self.directory(host_home).join(format!("{name}.json"))
    }
    #[must_use]
    pub fn server_name(&self, name: &PublishedName) -> String {
        format!(
            "{}-{}-{name}",
            match self.kind {
                PublicationKind::Mcp => "marsh-pub",
                PublicationKind::Acp => "marsh-acp",
            },
            &self.key[..12]
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PublicationCommit {
    pub message: String,
}

/// A publication result is not an argument error after effects were admitted.
/// Lost/malformed replies and any unproved post-effect failure remain uncertain.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PublicationOutcome {
    Committed(PublicationCommit),
    RejectedBeforeEffect { message: String },
    Uncertain { message: String },
}
impl PublicationOutcome {
    #[must_use]
    pub fn rejected(message: impl Into<String>) -> Self {
        Self::RejectedBeforeEffect {
            message: message.into(),
        }
    }
    #[must_use]
    pub fn uncertain(message: impl Into<String>) -> Self {
        Self::Uncertain {
            message: message.into(),
        }
    }
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Committed(commit) => &commit.message,
            Self::RejectedBeforeEffect { message } | Self::Uncertain { message } => message,
        }
    }
    /// # Errors
    /// Preserves the original typed rejection or uncertainty.
    pub fn into_commit(self) -> Result<PublicationCommit, Self> {
        match self {
            Self::Committed(commit) => Ok(commit),
            other => Err(other),
        }
    }
}
impl std::fmt::Display for PublicationOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Committed(commit) => f.write_str(&commit.message),
            Self::RejectedBeforeEffect { message } => {
                write!(f, "publication rejected before effect: {message}")
            }
            Self::Uncertain { message } => write!(f, "publication outcome uncertain: {message}"),
        }
    }
}

/// Validate the fixed pipeline payload without executing or parsing it as host code.
///
/// # Errors
/// Rejects empty, oversized or NUL-bearing pipeline payloads.
pub fn validate_publication_pipeline(pipeline: &str) -> Result<(), String> {
    if pipeline.is_empty() || pipeline.len() > 65_536 || pipeline.contains('\0') {
        Err("pipeline must be nonempty, at most 65536 bytes, and contain no NUL".into())
    } else {
        Ok(())
    }
}

/// Validate target domains and display options before any scope/lineage effect.
///
/// # Errors
/// Returns an actionable diagnostic without changing host or worker state.
pub fn validate_publication_options(
    description: Option<&str>,
    kit: Option<&str>,
    sandbox: Option<&str>,
    require_target: bool,
) -> Result<(), String> {
    if kit.is_some() && sandbox.is_some() {
        return Err("choose --kit or --sandbox, not both".into());
    }
    if require_target && kit.is_none() && sandbox.is_none() {
        return Err("MCP load requires exactly one of --kit or --sandbox".into());
    }
    if let Some(kit) = kit {
        marsh_contracts::command_registry::CommandName::parse(kit)
            .map_err(|error| error.to_string())?;
    }
    if description.is_some_and(|value| {
        value.is_empty() || value.len() > 1024 || value.chars().any(char::is_control)
    }) {
        return Err("description must be 1-1024 printable bytes".into());
    }
    if sandbox.is_some_and(|value| {
        value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    }) {
        return Err("invalid sandbox name or ID".into());
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationOperation {
    Publish,
    Load,
    Unpublish,
}

/// Authenticated daemon context on an inherited private socket; no master token.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostPublicationContext {
    pub session: SessionSpec,
    pub project_identity: (u64, u64),
    pub kind: PublicationKind,
    pub name: PublishedName,
    pub operation: PublicationOperation,
    pub kit: Option<String>,
    #[serde(default)]
    pub scope_admitted: bool,
    #[serde(default)]
    pub admission_lock: Option<PathBuf>,
    #[serde(default)]
    pub sandbox: Option<String>,
    #[serde(default)]
    pub agent_session_id: Option<String>,
    #[serde(default)]
    pub generation: Option<String>,
}

/// Bounded host control result, not a tool's arbitrary stdout capture.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PublicationStockOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[must_use]
pub fn publication_load_arguments(server: &str, sandbox: &str) -> Vec<String> {
    ["mcp", "load", server, "--sandbox", sandbox]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// Read-only stock target validation; a later target transition still requires
/// the commit/rollback path to handle failure without inventing zero effects.
///
/// # Errors
/// Rejects unavailable/non-running targets or invalid stock metadata.
pub fn validate_publication_sandbox_output(
    target: &str,
    output: &PublicationStockOutput,
) -> Result<(), String> {
    if output.exit_code != Some(0) {
        return Err(format!(
            "sandbox `{target}` is unavailable; use `sbx ls` and choose a running sandbox before publishing"
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| "stock sandbox inspect returned invalid JSON".to_owned())?;
    if value.get("state").and_then(serde_json::Value::as_str) != Some("running")
        || value
            .get("name")
            .and_then(serde_json::Value::as_str)
            .is_none_or(str::is_empty)
    {
        return Err(format!(
            "sandbox `{target}` is not running or its identity metadata is unavailable; use `sbx ls`"
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum PublicationHostEvent {
    PrepareKit,
    BeginCommit,
    BeginRollback,
    RunStock { arguments: Vec<String> },
    Complete(PublicationOutcome),
}

// Host publish may inspect, add, load, then roll back a stock registration.
// Each constituent stock command has a 60-second limit plus bounded drain.
const HOST_COMMAND_TIMEOUT: Duration = Duration::from_mins(5);
const OUTPUT_LIMIT: usize = 8 * 1024;
type PublicationLocks = Arc<Mutex<BTreeMap<(String, String), Weak<Mutex<()>>>>>;
type PublicationFences = Arc<Mutex<BTreeMap<String, (Arc<fs::File>, String)>>>;

/// Real cross-process scope admission, retained by the daemon through stock
/// settlement even if its host CLI disappears. It is not a workload grant.
pub(super) struct PublicationAdmission {
    file: Arc<fs::File>,
    path: PathBuf,
    key: String,
    fences: PublicationFences,
}
impl PublicationAdmission {
    fn fence(&self, reason: String) {
        self.fences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(self.key.clone())
            .or_insert_with(|| (Arc::clone(&self.file), reason));
    }
    fn fenced(&self) -> Option<String> {
        self.fences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&self.key)
            .map(|(_, reason)| reason.clone())
    }
}

#[derive(Clone)]
pub(super) struct McpHostControl {
    pub marsh: PathBuf,
    pub sbx: PathBuf,
    pub host_home: PathBuf,
    scope_root: Option<PathBuf>,
    /// This daemon's control directory, where default publications are
    /// recorded (`mcp_defaults`). `None` disables default recording.
    defaults_home: Option<PathBuf>,
    publication_locks: PublicationLocks,
    publication_fences: PublicationFences,
}

impl McpHostControl {
    pub fn new(
        marsh: PathBuf,
        sbx: PathBuf,
        host_home: PathBuf,
        scope_root: Option<PathBuf>,
    ) -> Self {
        Self {
            marsh,
            sbx,
            host_home,
            scope_root,
            defaults_home: None,
            publication_locks: Arc::new(Mutex::new(BTreeMap::new())),
            publication_fences: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    #[must_use]
    pub fn with_defaults_home(mut self, control_home: PathBuf) -> Self {
        self.defaults_home = Some(control_home);
        self
    }

    fn default_server(session: &SessionSpec, name: &str) -> Result<String, String> {
        let name = PublishedName::parse(name)?;
        let home = session
            .home_backing
            .canonicalize()
            .map_err(|error| error.to_string())?;
        Ok(
            PublicationScope::new(PublicationKind::Mcp, &session.launch_directory, &home)
                .server_name(&name),
        )
    }

    /// Record a committed untargeted publish as a default publication: every
    /// Kit VM this daemon creates from now on loads it (`mcp_defaults`).
    pub(super) fn record_default(&self, session: &SessionSpec, name: &str) -> Result<(), String> {
        let Some(control) = &self.defaults_home else {
            // Test daemons without a control directory keep no defaults.
            return Ok(());
        };
        let declaration = self.declaration_path(session, PublicationKind::Mcp, name)?;
        let generation = crate::mcp_defaults::current_generation(&declaration, name)
            .ok_or("committed publication declaration is unreadable")?;
        crate::mcp_defaults::record(
            control,
            crate::mcp_defaults::DefaultPublication {
                server: Self::default_server(session, name)?,
                name: name.into(),
                declaration,
                generation,
            },
        )
    }

    /// Stop loading a publication into new Kit VMs (unpublish, or a
    /// republish with an explicit target).
    pub(super) fn drop_default(&self, session: &SessionSpec, name: &str) -> Result<(), String> {
        match &self.defaults_home {
            Some(control) => {
                crate::mcp_defaults::remove(control, &Self::default_server(session, name)?)
                    .map(|_| ())
            }
            None => Ok(()),
        }
    }

    pub fn admit_scope(&self, session: &SessionSpec) -> Result<PublicationAdmission, String> {
        let home = session
            .home_backing
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let scope = PublicationScope::new(PublicationKind::Mcp, &session.launch_directory, &home);
        let fences = self
            .publication_fences
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, reason)) = fences.get(scope.key()) {
            return Err(format!(
                "publication scope is fenced after unconfirmed stock effects: {reason}; host inspection is required before restarting this controller"
            ));
        }
        if fences.len() >= 64 {
            return Err(
                "publication cleanup uncertainty capacity reached; host inspection is required"
                    .into(),
            );
        }
        drop(fences);
        let owner = PublicationScope::owner_root(&self.host_home);
        if self.host_home.starts_with(&session.launch_directory) || owner.starts_with(&home) {
            return Err(
                "publication control must stay outside the project and selected home".into(),
            );
        }
        if let Some(parent) = owner.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        publication_private_directory(&owner)?;
        let owner = owner.canonicalize().map_err(|error| error.to_string())?;
        if owner.starts_with(&session.launch_directory)
            || owner.starts_with(&home)
            || session.launch_directory.starts_with(&owner)
            || home.starts_with(&owner)
        {
            return Err("publication control paths overlap the project or selected home".into());
        }
        let lock_path = scope.lock_path(&owner);
        publication_private_directory(
            lock_path
                .parent()
                .ok_or("publication lock directory missing")?,
        )?;
        let nofollow = i32::try_from(rustix::fs::OFlags::NOFOLLOW.bits())
            .map_err(|_| "no-follow flag overflow")?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(nofollow)
            .open(&lock_path)
            .map_err(|error| format!("cannot open publication admission: {error}"))?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err("publication admission lock must be an owner-only real file".into());
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
                Err(fs::TryLockError::WouldBlock) => return Err("publication scope is busy with another operation; retry after it completes (no grant admitted)".into()),
                Err(fs::TryLockError::Error(error)) => return Err(format!("cannot acquire publication admission: {error}")),
            }
        }
        Ok(PublicationAdmission {
            file: Arc::new(file),
            path: lock_path,
            key: scope.key().into(),
            fences: Arc::clone(&self.publication_fences),
        })
    }

    pub fn publication_lock(&self, scope: &str, name: &str) -> Arc<Mutex<()>> {
        let mut locks = self
            .publication_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.retain(|_, lock| lock.strong_count() > 0);
        let key = (scope.to_owned(), name.to_owned());
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        lock
    }

    pub(super) fn declaration_path(
        &self,
        session: &SessionSpec,
        kind: PublicationKind,
        name: &str,
    ) -> Result<PathBuf, String> {
        let name = PublishedName::parse(name)?;
        let home = fs::canonicalize(&session.home_backing)
            .map_err(|error| format!("cannot resolve published MCP home: {error}"))?;
        Ok(
            PublicationScope::new(kind, &session.launch_directory, &home)
                .declaration_path(&self.host_home, &name),
        )
    }

    #[allow(clippy::too_many_arguments)] // One admitted transaction carries scope, declaration and a private preparation callback.
    pub fn publish(
        &self,
        session: &SessionSpec,
        project_identity: (u64, u64),
        name: &str,
        description: Option<&str>,
        sandbox: Option<&str>,
        pipeline: &str,
        kit: Option<&str>,
        prepare: &mut dyn FnMut() -> Result<String, String>,
        before_commit: &mut dyn FnMut() -> Result<(), String>,
        admission: &PublicationAdmission,
    ) -> Result<PublicationCommit, PublicationOutcome> {
        let mut args = vec!["mcp".to_owned(), "publish".to_owned(), name.to_owned()];
        if let Some(description) = description {
            args.extend(["--description".to_owned(), description.to_owned()]);
        }
        if let Some(sandbox) = sandbox {
            args.extend(["--sandbox".to_owned(), sandbox.to_owned()]);
        }
        args.extend(["--".to_owned(), pipeline.to_owned()]);
        self.run_typed(
            session,
            project_identity,
            &args,
            kit,
            Some(prepare),
            before_commit,
            admission,
        )
    }

    pub fn load(
        &self,
        session: &SessionSpec,
        project_identity: (u64, u64),
        name: &str,
        target: (Option<&str>, Option<&str>),
        prepare: &mut dyn FnMut() -> Result<String, String>,
        admission: &PublicationAdmission,
    ) -> Result<PublicationCommit, PublicationOutcome> {
        let (kit, sandbox) = target;
        let (selector, target) = match (kit, sandbox) {
            (Some(kit), None) => ("--kit", kit),
            (None, Some(sandbox)) => ("--sandbox", sandbox),
            _ => {
                return Err(PublicationOutcome::rejected(
                    "MCP load requires exactly one target",
                ));
            }
        };
        self.run_typed(
            session,
            project_identity,
            &[
                "mcp".into(),
                "load".into(),
                name.into(),
                selector.into(),
                target.into(),
            ],
            kit,
            Some(prepare),
            &mut || Ok(()),
            admission,
        )
    }

    pub fn mark_revocation_pending(&self, session: &SessionSpec, name: &str) -> Result<(), String> {
        mark_revocation_pending(&self.declaration_path(session, PublicationKind::Mcp, name)?)
    }

    pub fn unpublish(
        &self,
        session: &SessionSpec,
        project_identity: (u64, u64),
        name: &str,
        admission: &PublicationAdmission,
    ) -> Result<PublicationCommit, PublicationOutcome> {
        self.run_typed(
            session,
            project_identity,
            &["mcp".to_owned(), "unpublish".to_owned(), name.to_owned()],
            None,
            None,
            &mut || Ok(()),
            admission,
        )
    }

    #[allow(clippy::too_many_arguments)] // Fixed grant identity plus one daemon-bound preparation callback.
    pub fn publish_acp(
        &self,
        session: &SessionSpec,
        project_identity: (u64, u64),
        name: &str,
        agent_session_id: &str,
        generation: &str,
        sandbox: Option<&str>,
        kit: Option<&str>,
        prepare: &mut dyn FnMut() -> Result<String, String>,
        before_commit: &mut dyn FnMut() -> Result<(), String>,
        admission: &PublicationAdmission,
    ) -> Result<PublicationCommit, PublicationOutcome> {
        let mut args = vec![
            "acp".into(),
            "host-publish".into(),
            name.into(),
            agent_session_id.into(),
            generation.into(),
        ];
        if let Some(sandbox) = sandbox {
            args.extend(["--sandbox".into(), sandbox.into()]);
        }
        self.run_typed(
            session,
            project_identity,
            &args,
            kit,
            Some(prepare),
            before_commit,
            admission,
        )
    }

    pub fn unpublish_acp(
        &self,
        session: &SessionSpec,
        project_identity: (u64, u64),
        name: &str,
        before_commit: &mut dyn FnMut() -> Result<(), String>,
        admission: &PublicationAdmission,
    ) -> Result<PublicationCommit, PublicationOutcome> {
        self.run_typed(
            session,
            project_identity,
            &["acp".into(), "host-unpublish".into(), name.into()],
            None,
            None,
            before_commit,
            admission,
        )
    }

    pub fn preflight_sandbox(
        &self,
        session: &SessionSpec,
        target: Option<&str>,
        admission: &PublicationAdmission,
    ) -> Result<(), PublicationOutcome> {
        if let Some(target) = target {
            let arguments = ["inspect", "--json", target]
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let output = self
                .execute_publication_stock(session, &arguments, false, admission)
                .map_err(|error| {
                    if admission.fenced().is_some() {
                        PublicationOutcome::uncertain(error)
                    } else {
                        PublicationOutcome::rejected(error)
                    }
                })?;
            validate_publication_sandbox_output(target, &output)
                .map_err(PublicationOutcome::rejected)?;
        }
        Ok(())
    }

    fn run_publication_stock(
        &self,
        context: &HostPublicationContext,
        scope: &PublicationScope,
        arguments: &[String],
        prepared_sandbox: Option<&str>,
        commit_started: bool,
        admission: &PublicationAdmission,
    ) -> Result<PublicationStockOutput, String> {
        if let Some(reason) = admission.fenced() {
            return Err(format!(
                "publication scope fenced after unconfirmed stock effects: {reason}"
            ));
        }
        if arguments.len() > 16 || arguments.iter().map(String::len).sum::<usize>() > 65_536 {
            return Err("publication stock request exceeds its control bound".into());
        }
        let own = scope.server_name(&context.name);
        let other = scope.other_protocol().server_name(&context.name);
        let target = context.sandbox.as_deref().or(prepared_sandbox);
        let words = arguments.iter().map(String::as_str).collect::<Vec<_>>();
        let mutates = match words.as_slice() {
            ["mcp", "inspect", name, "--json"] if *name == own || *name == other => false,
            ["inspect", "--json", sandbox] if Some(*sandbox) == target => false,
            ["mcp", "rm", "--force", name] if *name == own => true,
            ["mcp", "load", name, "--sandbox", sandbox]
                if *name == own && Some(*sandbox) == target =>
            {
                true
            }
            [
                "mcp",
                "add",
                name,
                "--command",
                _,
                "--args",
                _,
                "--dir",
                directory,
            ] if *name == own && Some(*directory) == context.session.launch_directory.to_str() => {
                true
            }
            _ => {
                return Err(
                    "stock request is outside this publication's fixed scope and target".into(),
                );
            }
        };
        if mutates && !commit_started {
            return Err("stock mutation requires publication commit admission".into());
        }
        self.execute_publication_stock(&context.session, arguments, mutates, admission)
    }

    fn execute_publication_stock(
        &self,
        session: &SessionSpec,
        arguments: &[String],
        mutates: bool,
        admission: &PublicationAdmission,
    ) -> Result<PublicationStockOutput, String> {
        let runner = marsh_runtime::SystemCommandRunner::new(&self.host_home);
        let invocation = marsh_runtime::Invocation {
            program: self.sbx.clone(),
            arguments: arguments.iter().map(std::ffi::OsString::from).collect(),
            working_directory: Some(session.launch_directory.clone()),
        };
        match marsh_sbx::run_stock_command_capped(
            &runner,
            &invocation,
            Duration::from_mins(1),
            128 * 1024,
        ) {
            Ok(output) => Ok(PublicationStockOutput {
                exit_code: output.exit_code,
                stdout: output.stdout,
                stderr: output.stderr,
            }),
            Err(error) => {
                // A timed-out mutation may have reached the SDK even if its
                // local carrier was reaped. Never permit a following unpublish
                // to race an unconfirmed mutation. Keep the exact scope flock.
                if mutates
                    || matches!(
                        error,
                        marsh_sbx::SbxError::StockControlCleanupUncertain { .. }
                    )
                {
                    admission.fence(error.to_string());
                }
                Err(format!(
                    "supervised stock command failed: {error}; {}",
                    if admission.fenced().is_some() {
                        "scope fenced; host inspection is required before restarting this controller"
                    } else {
                        "no publication mutation was dispatched"
                    }
                ))
            }
        }
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)] // One owned process, framed context, admission callbacks and terminal outcome.
    fn run_typed(
        &self,
        session: &SessionSpec,
        project_identity: (u64, u64),
        args: &[String],
        kit: Option<&str>,
        mut prepare: Option<&mut dyn FnMut() -> Result<String, String>>,
        before_commit: &mut dyn FnMut() -> Result<(), String>,
        admission: &PublicationAdmission,
    ) -> Result<PublicationCommit, PublicationOutcome> {
        let name = PublishedName::parse(
            args.get(2)
                .ok_or_else(|| PublicationOutcome::rejected("missing publication name"))?
                .clone(),
        )
        .map_err(PublicationOutcome::rejected)?;
        let kind = match args.first().map(String::as_str) {
            Some("mcp") => PublicationKind::Mcp,
            Some("acp") => PublicationKind::Acp,
            _ => return Err(PublicationOutcome::rejected("invalid publication kind")),
        };
        let operation = match args.get(1).map(String::as_str) {
            Some("publish" | "host-publish") => PublicationOperation::Publish,
            Some("load") => PublicationOperation::Load,
            Some("unpublish" | "host-unpublish") => PublicationOperation::Unpublish,
            _ => {
                return Err(PublicationOutcome::rejected(
                    "invalid publication operation",
                ));
            }
        };
        let backing = session
            .home_backing
            .canonicalize()
            .map_err(|error| PublicationOutcome::rejected(error.to_string()))?;
        let scope = PublicationScope::new(kind, &session.launch_directory, &backing);
        if scope.key() != admission.key {
            return Err(PublicationOutcome::rejected(
                "publication scope admission does not match context",
            ));
        }
        let sandbox = args
            .iter()
            .position(|arg| arg == "--sandbox")
            .and_then(|index| args.get(index + 1))
            .cloned();
        let acp_publish =
            kind == PublicationKind::Acp && operation == PublicationOperation::Publish;
        let context = HostPublicationContext {
            session: session.clone(),
            project_identity,
            kind,
            name,
            operation,
            kit: kit.map(str::to_owned),
            scope_admitted: true,
            admission_lock: Some(admission.path.clone()),
            sandbox,
            agent_session_id: acp_publish.then(|| args[3].clone()),
            generation: acp_publish.then(|| args[4].clone()),
        };
        let (mut channel, child_channel) =
            marsh_runtime::with_host_descriptor_creation_excluded(UnixStream::pair)
                .map_err(|error| PublicationOutcome::rejected(error.to_string()))?;
        channel
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| PublicationOutcome::rejected(error.to_string()))?;
        let mut reader = channel
            .try_clone()
            .map_err(|error| PublicationOutcome::rejected(error.to_string()))?;
        let mut command = Command::new(&self.marsh);
        command
            .args(args)
            .current_dir(&session.launch_directory)
            .env_clear()
            .env("HOME", &self.host_home)
            .env("USER", &session.username)
            .env("LOGNAME", &session.username)
            .env(
                "MARSH_HOME",
                self.scope_root.as_deref().unwrap_or(&session.home_backing),
            )
            .env("MARSH_SBX", &self.sbx)
            .env("MARSH_PUBLICATION_CHANNEL", "1")
            .env("PATH", "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin")
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(child_channel)))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        // The host helper writes publication state under the same state root.
        if let Some(control) = crate::host_state_directory::control_override_from_env() {
            command.env("MARSH_CONTROL_HOME", control);
        }
        let mut child = marsh_runtime::with_host_descriptor_creation_excluded(|| command.spawn())
            .map_err(|error| {
            PublicationOutcome::rejected(format!("cannot start pinned host marsh: {error}"))
        })?;
        drop(command); // Close the parent's copy of the child's endpoint.
        if let Err(error) = crate::write_frame(&mut channel, &context) {
            terminate_host_command(&mut child);
            return Err(PublicationOutcome::uncertain(format!(
                "host context delivery failed: {error}; not remote cancellation"
            )));
        }
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (out_tx, out_rx) = mpsc::sync_channel(1);
        let (err_tx, err_rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ = out_tx.send(read_bounded(stdout));
        });
        thread::spawn(move || {
            let _ = err_tx.send(read_bounded(stderr));
        });
        let (event_tx, event_rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            while let Ok(event) = crate::read_frame::<PublicationHostEvent>(&mut reader) {
                let terminal = matches!(event, PublicationHostEvent::Complete(_));
                if event_tx.send(event).is_err() || terminal {
                    break;
                }
            }
        });
        let mut deadline = Instant::now() + HOST_COMMAND_TIMEOUT;
        let mut outcome = None;
        let mut commit_started = false;
        let mut preparation_started = false;
        let mut prepared_sandbox = None;
        let mut rollback = false;
        let mut stock_calls = 0_u8;
        let mut rollback_calls = 0_u8;
        let status = loop {
            match event_rx.recv_timeout(Duration::from_millis(25)) {
                Ok(PublicationHostEvent::PrepareKit) => {
                    let started = Instant::now();
                    let result = if commit_started {
                        Err("preparation after commit admission is forbidden".to_owned())
                    } else {
                        prepare
                            .take()
                            .ok_or_else(|| {
                                "unexpected or repeated Kit preparation request".to_owned()
                            })
                            .and_then(|prepare| {
                                preparation_started = true;
                                prepare()
                            })
                            .map(Some)
                    };
                    if let Ok(Some(sandbox)) = &result {
                        prepared_sandbox = Some(sandbox.clone());
                    }
                    deadline += started.elapsed(); // Backend owns cold preparation deadlines.
                    if crate::write_frame(&mut channel, &result).is_err() {
                        terminate_host_command(&mut child);
                        break Err("host disconnected during preparation; tool not loaded by this request; Kit preparation may have completed (not cancelled)".to_owned());
                    }
                }
                Ok(PublicationHostEvent::BeginCommit) => {
                    let result = if commit_started {
                        Err("publication commit already admitted".to_owned())
                    } else {
                        before_commit().map(|()| {
                            commit_started = true;
                            None::<String>
                        })
                    };
                    if crate::write_frame(&mut channel, &result).is_err() {
                        terminate_host_command(&mut child);
                        break Err("host disconnected at publication commit; inspect state before retrying".to_owned());
                    }
                }
                Ok(PublicationHostEvent::BeginRollback) => {
                    let result = if !commit_started || rollback {
                        Err("rollback requires one admitted commit".to_owned())
                    } else {
                        rollback = true;
                        Ok(None::<String>)
                    };
                    if crate::write_frame(&mut channel, &result).is_err() {
                        terminate_host_command(&mut child);
                        break Err("host disconnected before rollback acknowledgment; inspect publication state".into());
                    }
                }
                Ok(PublicationHostEvent::RunStock { arguments }) => {
                    let started = Instant::now();
                    let counter = if rollback {
                        &mut rollback_calls
                    } else {
                        &mut stock_calls
                    };
                    let maximum = if rollback { 4 } else { 8 };
                    let result = if *counter >= maximum {
                        Err(format!(
                            "publication {} stock-command budget exhausted",
                            if rollback {
                                "rollback"
                            } else {
                                "preflight/commit"
                            }
                        ))
                    } else {
                        *counter += 1;
                        self.run_publication_stock(
                            &context,
                            &scope,
                            &arguments,
                            prepared_sandbox.as_deref(),
                            commit_started,
                            admission,
                        )
                    };
                    // Each stock carrier is owned, capped and reaped by this
                    // daemon. Reserve four full calls for rollback independently
                    // of preflight/commit; no orphan child outlives this callback.
                    deadline += started.elapsed();
                    if crate::write_frame(&mut channel, &result).is_err() {
                        terminate_host_command(&mut child);
                        break Err("host disconnected after supervised stock settlement; publication outcome unknown (not remote cancellation)".into());
                    }
                }
                Ok(PublicationHostEvent::Complete(result)) => outcome = Some(result),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    thread::sleep(Duration::from_millis(25));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if Instant::now() < deadline => {}
                Ok(None) => {
                    terminate_host_command(&mut child);
                    break Err("host publication exceeded five minutes outside Kit preparation; not remote cancellation".to_owned());
                }
                Err(error) => {
                    terminate_host_command(&mut child);
                    break Err(format!("cannot wait for host publication: {error}"));
                }
            }
        };
        // The process can exit before its reader thread schedules the terminal
        // frame. Never substitute stdout, stderr, an exit code or a sentinel.
        if outcome.is_none()
            && let Ok(PublicationHostEvent::Complete(result)) =
                event_rx.recv_timeout(Duration::from_secs(2))
        {
            outcome = Some(result);
        }
        let _ = channel.shutdown(std::net::Shutdown::Both);
        let mut output_ok = true;
        for receiver in [out_rx, err_rx] {
            output_ok &= matches!(
                receiver.recv_timeout(Duration::from_secs(2)),
                Ok(Ok((_, false)))
            );
        }
        if !output_ok {
            terminate_host_command(&mut child);
            return Err(PublicationOutcome::uncertain(
                "host diagnostic streams exceeded bounds or did not close",
            ));
        }
        let status = status.map_err(PublicationOutcome::uncertain)?;
        let outcome = outcome.ok_or_else(|| PublicationOutcome::uncertain("host exited without a typed publication result; inspect state before retrying (not remote cancellation)"))?;
        if matches!(outcome, PublicationOutcome::Committed(_))
            && (!status.success() || !commit_started || admission.fenced().is_some())
        {
            return Err(PublicationOutcome::uncertain(
                "host reported commit without successful admitted completion; inspect publication state",
            ));
        }
        if matches!(outcome, PublicationOutcome::RejectedBeforeEffect { .. })
            && (commit_started || preparation_started || admission.fenced().is_some())
        {
            return Err(PublicationOutcome::uncertain(format!(
                "host reported a rejection after effects were admitted: {outcome}"
            )));
        }
        outcome.into_commit()
    }
}

/// Prepare an admitted publication target without generic Prepare's shell
/// detachment semantics. Both ACP and MCP use the same backend/progress path.
pub(crate) fn prepare_publication_kit(
    server: &crate::ConnectionHandler,
    session: &SessionSpec,
    kit: &str,
    progress: UnixStream,
) -> Result<String, String> {
    if !server
        .backend
        .registered_kits()
        .map_err(|error| error.to_string())?
        .contains_key(kit)
    {
        return Err(format!(
            "unknown Kit command `{kit}`; use a registered command or --sandbox SANDBOX"
        ));
    }
    server
        .backend
        .prepare(
            &crate::LoadSelection::Kits(vec![kit.into()]),
            session,
            crate::PreparationProgress::new(progress),
            server.store.clone(),
        )
        .map_err(|error| error.to_string())?
        .sandboxes
        .remove(kit)
        .ok_or_else(|| "Kit did not report its prepared sandbox".into())
}

/// Fence a slow MCP prepare before waiting for its transaction lock. Missing
/// publications allocate no marker. A pending marker survives failed revocation
/// and is cleared only by a successful unpublish, never by a load or republish.
///
/// # Errors
/// Returns an error if the publication or its pending marker cannot be accessed.
pub fn mark_revocation_pending(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("cannot inspect publication to revoke: {error}")),
        Ok(_) => {}
    }
    let marker = path.with_extension("revoke");
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(marker)
    {
        Ok(file) => file.sync_all().map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(format!(
            "cannot mark publication revocation pending: {error}"
        )),
    }
}

fn publication_private_directory(path: &Path) -> Result<(), String> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.to_string()),
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err("publication directory must be owner-only and real".into());
    }
    Ok(())
}

fn terminate_host_command(child: &mut std::process::Child) {
    // A reaped PID/PGID may be reused. Diagnostic-drain failures after exit
    // are uncertain; never signal a numeric group whose leader we no longer
    // retain as a live/unreaped child.
    if !matches!(child.try_wait(), Ok(None)) {
        return;
    }
    let group = i32::try_from(child.id()).ok().and_then(Pid::from_raw);
    if let Some(group) = group {
        let _ = kill_process_group(group, Signal::TERM);
        thread::sleep(Duration::from_millis(250));
        let _ = kill_process_group(group, Signal::KILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn read_bounded(mut stream: impl Read) -> Result<(Vec<u8>, bool), String> {
    let mut retained = Vec::new();
    let mut buffer = [0u8; 4096];
    let mut truncated = false;
    loop {
        let size = stream
            .read(&mut buffer)
            .map_err(|error| format!("cannot read host registration output: {error}"))?;
        if size == 0 {
            return Ok((retained, truncated));
        }
        let keep = OUTPUT_LIMIT.saturating_sub(retained.len()).min(size);
        retained.extend_from_slice(&buffer[..keep]);
        truncated |= keep < size;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, McpHostControl, SessionSpec) {
        let root = tempfile::tempdir().unwrap();
        let host_home = root.path().join("host");
        let home_backing = root.path().join("backing");
        let workspace = root.path().join("project");
        fs::create_dir_all(&host_home).unwrap();
        fs::create_dir_all(&home_backing).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let control = McpHostControl::new(
            root.path().join("marsh"),
            root.path().join("sbx"),
            host_home,
            None,
        );
        let session = SessionSpec {
            session_id: "publisher".into(),
            username: "owner".into(),
            uid: rustix::process::geteuid().as_raw(),
            gid: rustix::process::getegid().as_raw(),
            launch_directory: workspace,
            guest_home: root.path().join("guest"),
            home_backing,
            ephemeral_home: false,
            terminal: false,
            terminal_size: None,
        };
        (root, control, session)
    }

    #[test]
    fn declaration_path_rejects_unsafe_names() {
        let (_root, control, session) = fixture();
        assert!(
            control
                .declaration_path(&session, PublicationKind::Mcp, "../tool")
                .is_err()
        );
    }
}
