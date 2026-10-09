//! Lean dev broker (docs/design/self-development.md): one grant per `marsh --dev`
//! session, persisted in the ownership map, reached only through the
//! session's relay as `DevSbx` calls from the guest `sbx` shim.
//!
//! Model: `docs/model/Ownership.tla` child actions. Names are a random
//! per-grant prefix; a create persists its intent first and the next
//! inventory adopts it by name. An op on an existing VM needs the name in the
//! grant with its recorded UUID in the inventory. Revocation persists
//! `revoked`, fences in-flight calls, removes the grant's VMs by recorded
//! UUID, and deletes the per-session scratch leaves. Nothing is replayed.

pub mod policy;
pub mod stream;

use crate::DaemonError;
use marsh_contracts::TerminalSize;
use marsh_runtime::Invocation;
use marsh_sbx::{DevGrantRecord, StockSbx};
use policy::{Action, GrantView, Verb};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fmt::Write as _,
    fs,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub const MAX_VMS: usize = 8;
/// Daemons refuse `--dev` at this depth (the name pattern caps it anyway).
pub const MAX_DEPTH: u32 = 3;
const CONTROL_TIMEOUT: Duration = Duration::from_mins(5);
const CAPTURE_LIMIT: usize = 8 * 1024 * 1024;
/// Leaves of the scratch root mounted into the dev shell at the same path.
pub const SCRATCH_LEAVES: [&str; 5] = ["home", "control", "tmp", "artifacts", "cache"];
/// Leaves deleted at revocation; `artifacts` and `cache` persist.
const DISPOSABLE_LEAVES: [&str; 3] = ["home", "control", "tmp"];

#[derive(Default)]
struct Live {
    fence: AtomicBool,
    inflight: AtomicUsize,
}

/// A created grant, as the backend needs it at attach.
#[derive(Clone, Debug)]
pub struct Grant {
    pub id: String,
    pub prefix: String,
    pub scratch: PathBuf,
    pub depth: u32,
}

pub struct DevBroker {
    stock: Arc<StockSbx>,
    enabled: bool,
    cache_root: PathBuf,
    templates: Vec<String>,
    depth: u32,
    live: Mutex<BTreeMap<String, Arc<Live>>>,
    sessions: Mutex<BTreeMap<String, String>>,
}

fn random_base36(length: usize) -> String {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut value = u128::from_le_bytes(*uuid::Uuid::new_v4().as_bytes());
    (0..length)
        .map(|_| {
            let byte = ALPHABET[(value % 36) as usize];
            value /= 36;
            char::from(byte)
        })
        .collect()
}

fn invalid(message: impl Into<String>) -> DaemonError {
    DaemonError::InvalidState(message.into())
}

impl DevBroker {
    /// `enabled` admits new grants (`MARSH_ENABLE_DEV_SCOPES=1`); cleanup of
    /// persisted grants runs regardless.
    #[must_use]
    pub fn new(
        stock: Arc<StockSbx>,
        enabled: bool,
        cache_root: PathBuf,
        templates: Vec<String>,
    ) -> Self {
        let depth = std::env::var("MARSH_DEV_DEPTH")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        Self {
            stock,
            enabled,
            cache_root,
            templates,
            depth,
            live: Mutex::new(BTreeMap::new()),
            sessions: Mutex::new(BTreeMap::new()),
        }
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    fn live(&self, id: &str) -> Arc<Live> {
        Arc::clone(
            self.live
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(id.to_owned())
                .or_default(),
        )
    }

    /// Create and persist a grant for an attaching `--dev` session, prepare
    /// its scratch leaves, and install the outer build's shim as
    /// `S/tmp/bin/sbx`.
    ///
    /// # Errors
    /// Refuses when disabled, too deep, or the project already has a live grant.
    pub fn create_grant(
        &self,
        session: &str,
        project: &Path,
        shim: &Path,
    ) -> Result<Grant, DaemonError> {
        if !self.enabled {
            return Err(invalid(
                "marsh --dev requires a daemon started with MARSH_ENABLE_DEV_SCOPES=1",
            ));
        }
        if self.depth >= MAX_DEPTH {
            return Err(invalid(format!(
                "marsh --dev is refused at nesting depth {}",
                self.depth
            )));
        }
        let project = project.canonicalize()?;
        let key = format!(
            "{:x}",
            Sha256::digest(project.as_os_str().as_encoded_bytes())
        );
        let scratch = self.cache_root.join(&key[..16]);
        if self
            .stock
            .dev_grants()
            .values()
            .any(|grant| grant.scratch == scratch)
        {
            return Err(invalid(
                "another marsh --dev session (or its uncertain cleanup) holds this project's scratch",
            ));
        }
        fs::create_dir_all(&scratch)?;
        fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700))?;
        for leaf in SCRATCH_LEAVES {
            let path = scratch.join(leaf);
            fs::create_dir_all(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        }
        let scratch = scratch.canonicalize()?;
        let bin = scratch.join("tmp/bin");
        fs::create_dir_all(&bin)?;
        let installed = bin.join("sbx");
        let _ = fs::remove_file(&installed);
        fs::copy(shim, &installed)?;
        fs::set_permissions(&installed, fs::Permissions::from_mode(0o755))?;
        let id = random_base36(12);
        let prefix = format!("{}x{}-", marsh_sbx::vm_prefix(), random_base36(5));
        let record = DevGrantRecord {
            session: session.to_owned(),
            prefix: prefix.clone(),
            roots: vec![project, scratch.clone()],
            scratch: scratch.clone(),
            max_vms: MAX_VMS,
            revoked: false,
            cleanup_uncertain: false,
            names: BTreeMap::new(),
        };
        self.stock
            .update_dev_grants(|grants| grants.insert(id.clone(), record))
            .map_err(|error| invalid(error.to_string()))?;
        self.live(&id);
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session.to_owned(), id.clone());
        Ok(Grant {
            id,
            prefix,
            scratch,
            depth: self.depth + 1,
        })
    }

    /// Tell the shim where this session's relay socket and token are.
    ///
    /// # Errors
    /// Fails when the sidecar cannot be written.
    pub fn write_relay(
        &self,
        grant: &Grant,
        socket: &Path,
        token: &Path,
    ) -> Result<(), DaemonError> {
        let document = serde_json::json!({ "socket": socket, "token": token });
        let path = grant.scratch.join("tmp/bin/sbx-relay.json");
        fs::write(&path, serde_json::to_vec(&document)?)?;
        Ok(())
    }

    /// Revoke the grant bound to a session (exit, relay loss, retire).
    ///
    /// # Errors
    /// Returns cleanup uncertainty; the grant stays persisted for retry.
    pub fn revoke_session(&self, session: &str) -> Result<(), DaemonError> {
        let id = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(session);
        match id {
            Some(id) => self.revoke(&id),
            None => Ok(()),
        }
    }

    /// Revoke every persisted grant (daemon start, `marsh stop`/`marsh reset`).
    ///
    /// # Errors
    /// Returns the first cleanup failure after attempting all grants.
    /// Whether any development grant (live or persisted) exists to revoke.
    #[must_use]
    pub fn has_grants(&self) -> bool {
        !self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
            || !self.stock.dev_grants().is_empty()
    }

    pub fn revoke_all(&self) -> Result<(), DaemonError> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        let mut first = None;
        for id in self.stock.dev_grants().into_keys() {
            if let Err(error) = self.revoke(&id) {
                first.get_or_insert(error);
            }
        }
        first.map_or(Ok(()), Err)
    }

    fn revoke(&self, id: &str) -> Result<(), DaemonError> {
        let marked = self
            .stock
            .update_dev_grants(|grants| {
                grants.get_mut(id).map(|grant| {
                    grant.revoked = true;
                    grant.clone()
                })
            })
            .map_err(|error| invalid(error.to_string()))?;
        let Some(record) = marked else {
            return Ok(());
        };
        let live = self.live(id);
        live.fence.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(10);
        while live.inflight.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let mut failure = None;
        if live.inflight.load(Ordering::Acquire) != 0 {
            failure = Some("in-flight broker calls did not stop".to_owned());
        }
        if failure.is_none()
            && let Err(error) = self.remove_children(id, &record)
        {
            failure = Some(error);
        }
        if failure.is_none() {
            for leaf in DISPOSABLE_LEAVES {
                let path = record.scratch.join(leaf);
                if let Err(error) = fs::remove_dir_all(&path)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    failure = Some(format!("remove {}: {error}", path.display()));
                }
            }
        }
        let outcome = self.stock.update_dev_grants(|grants| {
            if failure.is_some() {
                if let Some(grant) = grants.get_mut(id) {
                    grant.cleanup_uncertain = true;
                }
            } else {
                grants.remove(id);
            }
        });
        if failure.is_none() {
            self.live
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(id);
        }
        outcome.map_err(|error| invalid(error.to_string()))?;
        failure.map_or(Ok(()), |detail| {
            Err(DaemonError::ShellCleanupUncertain(format!(
                "development grant {id} cleanup is uncertain: {detail}; it is retried at daemon start"
            )))
        })
    }

    fn remove_children(&self, id: &str, record: &DevGrantRecord) -> Result<(), String> {
        if record.names.is_empty() {
            return Ok(());
        }
        // A fresh inventory adopts sent intents by name before removal.
        let inventory = self
            .stock
            .stock_inventory(true)
            .map_err(|error| error.to_string())?;
        let names = self
            .stock
            .dev_grants()
            .get(id)
            .map(|grant| grant.names.clone())
            .unwrap_or_default();
        let mut removed = Vec::new();
        for (name, uuid) in names {
            match (inventory.get(&name), uuid) {
                (None, _) => removed.push(name),
                (Some(vm), Some(uuid)) if vm.id == uuid => {
                    let output =
                        self.run_captured(&["rm".into(), "--force".into(), name.clone()])?;
                    if output.exit_code != Some(0) {
                        return Err(format!(
                            "rm {name}: {}",
                            String::from_utf8_lossy(&output.stderr).trim()
                        ));
                    }
                    removed.push(name);
                }
                (Some(_), _) => return Err(format!("{name} is present with an unrecorded UUID")),
            }
        }
        self.stock.invalidate_stock_inventory();
        let after = self
            .stock
            .stock_inventory(true)
            .map_err(|error| error.to_string())?;
        if let Some(name) = removed.iter().find(|name| after.get(name).is_some()) {
            return Err(format!("{name} is still present after removal"));
        }
        self.stock
            .update_dev_grants(|grants| {
                if let Some(grant) = grants.get_mut(id) {
                    for name in &removed {
                        grant.names.remove(name);
                    }
                }
            })
            .map_err(|error| error.to_string())
    }

    fn invocation(&self, argv: &[String]) -> Invocation {
        let (program, _) = self.stock.stock_command();
        Invocation {
            program,
            arguments: argv.iter().map(OsString::from).collect(),
            working_directory: None,
        }
    }

    fn run_captured(&self, argv: &[String]) -> Result<marsh_runtime::CommandOutput, String> {
        let (_, runner) = self.stock.stock_command();
        marsh_sbx::run_stock_command_capped(
            runner.as_ref(),
            &self.invocation(argv),
            CONTROL_TIMEOUT,
            CAPTURE_LIMIT,
        )
        .map_err(|error| error.to_string())
    }

    fn grant_for(&self, session: &str) -> Result<(String, DevGrantRecord), String> {
        let id = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session)
            .cloned()
            .ok_or("this session has no development grant (start it with marsh --dev)")?;
        let record = self
            .stock
            .dev_grants()
            .remove(&id)
            .ok_or("development grant is gone")?;
        if record.revoked {
            return Err("development grant is revoked".into());
        }
        Ok((id, record))
    }

    /// Serve one `DevSbx` call on an authenticated relay stream.
    ///
    /// # Errors
    /// Returns stream failures; refusals are delivered in-band.
    pub fn serve(
        &self,
        stream: &UnixStream,
        session: &str,
        argv: &[String],
        pty: Option<TerminalSize>,
        cwd: Option<&Path>,
    ) -> Result<(), DaemonError> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.serve_inner(stream, session, argv, pty, cwd)
        }))
        .unwrap_or_else(|_| Err(invalid("dev broker call panicked")));
        if let Err(error) = &result {
            self.log(&format!("{argv:?}: {error}"));
        }
        result
    }

    /// Append a diagnostic line to the host-private broker log.
    fn log(&self, line: &str) {
        use std::io::Write as _;
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.cache_root.join("broker.log"))
        {
            let _ = writeln!(file, "{line}");
        }
    }

    fn serve_inner(
        &self,
        stream: &UnixStream,
        session: &str,
        argv: &[String],
        pty: Option<TerminalSize>,
        cwd: Option<&Path>,
    ) -> Result<(), DaemonError> {
        // Relative host paths resolve against the caller's cwd, which is the
        // same path on the host only when it exists there as itself.
        let cwd = cwd
            .filter(|cwd| cwd.is_absolute() && cwd.canonicalize().is_ok_and(|real| real == *cwd));
        let (id, record) = match self.grant_for(session) {
            Ok(found) => found,
            Err(message) => return stream::reject(stream, &message),
        };
        let argv =
            &policy::assign_run_name(argv, &format!("{}r-{}", record.prefix, random_base36(8)));
        let action = match policy::classify(
            argv,
            &GrantView {
                prefix: &record.prefix,
                roots: &record.roots,
                max_vms: record.max_vms,
                names: &record.names,
                templates: &self.templates,
                cwd,
            },
        ) {
            Ok(action) => action,
            Err(message) => return stream::reject(stream, &message),
        };
        let live = self.live(&id);
        if live.fence.load(Ordering::Acquire) {
            return stream::reject(stream, "development grant is revoked");
        }
        live.inflight.fetch_add(1, Ordering::AcqRel);
        let result = self.serve_admitted(stream, &id, &record, argv, pty, cwd, action, &live);
        live.inflight.fetch_sub(1, Ordering::AcqRel);
        match result {
            Ok(Some(revoke_reason)) => {
                let cleanup = self.revoke_session(session);
                Err(invalid(format!(
                    "{revoke_reason}; grant revoked: {cleanup:?}"
                )))
            }
            Ok(None) => Ok(()),
            Err(error) => Err(error),
        }
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)] // One admitted call, one owner.
    fn serve_admitted(
        &self,
        stream: &UnixStream,
        id: &str,
        record: &DevGrantRecord,
        argv: &[String],
        pty: Option<TerminalSize>,
        cwd: Option<&Path>,
        action: Action,
        live: &Live,
    ) -> Result<Option<String>, DaemonError> {
        let template = match &action {
            Action::Create { template, .. } | Action::Run { template, .. } => template.clone(),
            _ => None,
        };
        let forwarded = template
            .as_ref()
            .map_or_else(|| argv.to_vec(), |pin| pin.rewrite(argv));
        let argv = forwarded.as_slice();
        let template_digest = template.and_then(|pin| pin.digest);
        let invocation = Invocation {
            working_directory: cwd.map(Path::to_path_buf),
            ..self.invocation(argv)
        };
        let (_, runner) = self.stock.stock_command();
        match action {
            Action::PassThrough => {
                let output = self.run_captured(argv).map_err(invalid)?;
                let _ = &invocation;
                stream::reply_captured(
                    stream,
                    &output.stdout,
                    &output.stderr,
                    output.exit_code.unwrap_or(125),
                )?;
                Ok(None)
            }
            Action::List { json } => {
                let output = self
                    .run_captured(&["ls".into(), "--json".into()])
                    .map_err(invalid)?;
                if output.exit_code != Some(0) {
                    stream::reply_captured(
                        stream,
                        &output.stdout,
                        &output.stderr,
                        output.exit_code.unwrap_or(125),
                    )?;
                    return Ok(None);
                }
                let _ = self.stock.adopt_inventory(&output.stdout);
                let names = self
                    .stock
                    .dev_grants()
                    .get(id)
                    .map(|grant| grant.names.clone())
                    .unwrap_or_default();
                let filtered = filter_listing(&output.stdout, &names, json).map_err(invalid)?;
                stream::reply_captured(stream, &filtered, &output.stderr, 0)?;
                Ok(None)
            }
            Action::Create { name, sources, .. } => {
                let before = sources
                    .iter()
                    .map(|source| policy::source_identity(source, &record.roots))
                    .collect::<Result<Vec<_>, _>>();
                let before = match before {
                    Ok(before) => before,
                    Err(message) => {
                        stream::reject(stream, &message)?;
                        return Ok(None);
                    }
                };
                self.stock
                    .update_dev_grants(|grants| {
                        if let Some(grant) = grants.get_mut(id) {
                            grant.names.insert(name.clone(), None);
                        }
                    })
                    .map_err(|error| invalid(error.to_string()))?;
                self.stock.invalidate_stock_inventory();
                let served = match &template_digest {
                    // The tag-selected template's digest is checked before the
                    // caller sees the result: a mismatching VM is removed.
                    Some(digest) => self.create_pinned(stream, argv, cwd, &name, digest),
                    None => stream::serve(
                        stream.try_clone()?,
                        &invocation,
                        None,
                        runner.as_ref(),
                        &live.fence,
                    ),
                };
                // Adopt the created name, or drop an intent that never landed.
                if let Ok(view) = self.stock.stock_inventory(true)
                    && view.get(&name).is_none()
                {
                    let _ = self.stock.update_dev_grants(|grants| {
                        if let Some(grant) = grants.get_mut(id) {
                            grant.names.remove(&name);
                        }
                    });
                }
                served?;
                Ok(sources_changed(&sources, &before, &record.roots))
            }
            Action::Run {
                name,
                reattach,
                interactive,
                sources,
                ..
            } => {
                if reattach && let Err(message) = self.check_owned(id, &name) {
                    stream::reject(stream, &message)?;
                    return Ok(None);
                }
                let before = match sources
                    .iter()
                    .map(|source| policy::source_identity(source, &record.roots))
                    .collect::<Result<Vec<_>, _>>()
                {
                    Ok(before) => before,
                    Err(message) => {
                        stream::reject(stream, &message)?;
                        return Ok(None);
                    }
                };
                if !reattach {
                    self.stock
                        .update_dev_grants(|grants| {
                            if let Some(grant) = grants.get_mut(id) {
                                grant.names.insert(name.clone(), None);
                            }
                        })
                        .map_err(|error| invalid(error.to_string()))?;
                    self.stock.invalidate_stock_inventory();
                }
                // An attached agent session gets the caller's terminal.
                let served = stream::serve(
                    stream.try_clone()?,
                    &invocation,
                    pty.filter(|_| interactive),
                    runner.as_ref(),
                    &live.fence,
                );
                if !reattach
                    && let Ok(view) = self.stock.stock_inventory(true)
                    && view.get(&name).is_none()
                {
                    let _ = self.stock.update_dev_grants(|grants| {
                        if let Some(grant) = grants.get_mut(id) {
                            grant.names.remove(&name);
                        }
                    });
                }
                served?;
                if !reattach
                    && let Some(digest) = &template_digest
                    && let Err(message) = self.verify_template_digest(&name, digest)
                {
                    // The session already ran: remove the VM and revoke.
                    let _ = self.remove_child(id, &name);
                    return Ok(Some(message));
                }
                Ok(sources_changed(&sources, &before, &record.roots))
            }
            Action::Own {
                name,
                verb,
                sources,
            } => {
                if let Err(message) = self.check_owned(id, &name) {
                    stream::reject(stream, &message)?;
                    return Ok(None);
                }
                let before = match sources
                    .iter()
                    .filter(|_| verb != Verb::Umount)
                    .map(|source| policy::source_identity(source, &record.roots))
                    .collect::<Result<Vec<_>, _>>()
                {
                    Ok(before) => before,
                    Err(message) => {
                        stream::reject(stream, &message)?;
                        return Ok(None);
                    }
                };
                let tty = verb == Verb::Exec
                    && argv
                        .iter()
                        .skip(1)
                        .take_while(|word| word.starts_with('-'))
                        .any(|word| matches!(word.as_str(), "-t" | "-it" | "-ti" | "--tty"));
                let pty = tty.then(|| {
                    pty.unwrap_or(TerminalSize {
                        rows: 24,
                        columns: 80,
                    })
                });
                let served = stream::serve(
                    stream.try_clone()?,
                    &invocation,
                    pty,
                    runner.as_ref(),
                    &live.fence,
                );
                if matches!(verb, Verb::Stop | Verb::Rm) {
                    self.stock.invalidate_stock_inventory();
                }
                if verb == Verb::Rm
                    && let Ok(view) = self.stock.stock_inventory(true)
                    && view.get(&name).is_none()
                {
                    let _ = self.stock.update_dev_grants(|grants| {
                        if let Some(grant) = grants.get_mut(id) {
                            grant.names.remove(&name);
                        }
                    });
                }
                served?;
                if verb == Verb::Umount {
                    return Ok(None);
                }
                let changed = sources_changed(&sources, &before, &record.roots);
                if changed.is_some() && verb == Verb::Mount {
                    // Undo the exposure before revoking (model A4 window).
                    let mut undo = invocation.clone();
                    undo.arguments[0] = "umount".into();
                    let (_, runner) = self.stock.stock_command();
                    let _ = marsh_sbx::run_stock_command_capped(
                        runner.as_ref(),
                        &undo,
                        CONTROL_TIMEOUT,
                        CAPTURE_LIMIT,
                    );
                }
                Ok(changed)
            }
        }
    }

    /// `create` from a local template selected by tag: run it captured, check
    /// the created VM's image digest, and only then deliver the result.
    fn create_pinned(
        &self,
        stream: &UnixStream,
        argv: &[String],
        cwd: Option<&Path>,
        name: &str,
        digest: &str,
    ) -> Result<(), DaemonError> {
        let (_, runner) = self.stock.stock_command();
        let output = marsh_sbx::run_stock_command_capped(
            runner.as_ref(),
            &Invocation {
                working_directory: cwd.map(Path::to_path_buf),
                ..self.invocation(argv)
            },
            CONTROL_TIMEOUT,
            CAPTURE_LIMIT,
        )
        .map_err(|error| invalid(error.to_string()))?;
        if output.exit_code == Some(0)
            && let Err(message) = self.verify_template_digest(name, digest)
        {
            self.stock.invalidate_stock_inventory();
            let removed = self.run_captured(&["rm".into(), "--force".into(), name.into()]);
            let removed = removed.is_ok_and(|output| output.exit_code == Some(0));
            return stream::reject(stream, &format!("{message}; removed {name}: {removed}"));
        }
        stream::reply_captured(
            stream,
            &output.stdout,
            &output.stderr,
            output.exit_code.unwrap_or(125),
        )
    }

    /// A created VM's stock-reported image digest must be the pinned one.
    fn verify_template_digest(&self, name: &str, digest: &str) -> Result<(), String> {
        let output = self.run_captured(&["inspect".into(), "--json".into(), name.into()])?;
        let observed = (output.exit_code == Some(0))
            .then(|| serde_json::from_slice::<serde_json::Value>(&output.stdout).ok())
            .flatten()
            .and_then(|inspect| inspect.get("image_digest")?.as_str().map(str::to_owned));
        if observed.as_deref() == Some(digest) {
            Ok(())
        } else {
            Err(format!(
                "local template tag did not resolve to {digest} (stock reported {})",
                observed.as_deref().unwrap_or("no image digest")
            ))
        }
    }

    fn remove_child(&self, id: &str, name: &str) -> Result<(), String> {
        self.stock.invalidate_stock_inventory();
        let output = self.run_captured(&["rm".into(), "--force".into(), name.into()])?;
        if let Ok(view) = self.stock.stock_inventory(true)
            && view.get(name).is_none()
        {
            let _ = self.stock.update_dev_grants(|grants| {
                if let Some(grant) = grants.get_mut(id) {
                    grant.names.remove(name);
                }
            });
        }
        (output.exit_code == Some(0))
            .then_some(())
            .ok_or_else(|| format!("rm {name} failed"))
    }

    /// The name must be in this grant with its recorded UUID present.
    fn check_owned(&self, id: &str, name: &str) -> Result<(), String> {
        for refresh in [false, true] {
            let recorded = self
                .stock
                .dev_grants()
                .get(id)
                .and_then(|grant| grant.names.get(name).cloned())
                .ok_or_else(|| format!("{name} is not a VM of this development grant"))?;
            let view = self
                .stock
                .stock_inventory(refresh)
                .map_err(|error| error.to_string())?;
            let recorded = recorded.or_else(|| {
                self.stock
                    .dev_grants()
                    .get(id)
                    .and_then(|grant| grant.names.get(name).cloned().flatten())
            });
            match (view.get(name), recorded) {
                (Some(vm), Some(uuid)) if vm.id == uuid => return Ok(()),
                _ if !refresh => {}
                (None, _) => return Err(format!("{name} is absent from stock inventory")),
                _ => return Err(format!("{name} does not carry its recorded UUID")),
            }
        }
        Err(format!("{name} could not be verified"))
    }
}

fn sources_changed(
    sources: &[PathBuf],
    before: &[(u64, u64)],
    roots: &[PathBuf],
) -> Option<String> {
    sources.iter().zip(before).find_map(|(source, before)| {
        match policy::source_identity(source, roots) {
            Ok(after) if after == *before => None,
            _ => Some(format!(
                "source {} changed identity during the stock call",
                source.display()
            )),
        }
    })
}

/// Keep only this grant's rows of an `sbx ls --json` result.
fn filter_listing(
    raw: &[u8],
    names: &BTreeMap<String, Option<String>>,
    json: bool,
) -> Result<Vec<u8>, String> {
    let mut document: serde_json::Value =
        serde_json::from_slice(raw).map_err(|error| error.to_string())?;
    // Keep whichever envelope stock SBX used: `{"sandboxes": [...]}` or `[...]`.
    let rows = if document.is_array() {
        document.as_array_mut()
    } else {
        document
            .get_mut("sandboxes")
            .and_then(serde_json::Value::as_array_mut)
    }
    .ok_or("stock inventory has no sandboxes array")?;
    rows.retain(|row| {
        row.get("name")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|name| names.contains_key(name))
    });
    if json {
        let mut bytes = serde_json::to_vec(&document).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        return Ok(bytes);
    }
    let mut text = String::from("SANDBOX\tSTATUS\tID\n");
    for row in rows.iter() {
        let field = |key: &str| {
            row.get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
        };
        let _ = writeln!(
            text,
            "{}\t{}\t{}",
            field("name"),
            field("status"),
            field("id")
        );
    }
    Ok(text.into_bytes())
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)] // Test frames carry bytes.
mod tests {
    use super::*;

    const FAKE_SBX: &str = r#"#!/usr/bin/env python3
import json, os, sys, uuid
here = os.path.dirname(os.path.abspath(__file__))
state_path = os.path.join(here, "state.json")
state = json.load(open(state_path))
args = sys.argv[1:]
open(os.path.join(here, "log"), "a").write(json.dumps(args) + "\n")
if args == ["ls", "--json"]:
    print(json.dumps({"sandboxes": [{"name": n, "id": i, "status": "running"} for n, i in state.items()]}))
elif args[0] in ("create", "run"):
    state[args[args.index("--name") + 1]] = str(uuid.uuid4())
elif args[0] == "rm":
    state.pop(args[-1], None)
json.dump(state, open(state_path, "w"))
"#;

    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        broker: DevBroker,
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let sbx = root.join("sbx");
        fs::write(&sbx, FAKE_SBX).unwrap();
        fs::set_permissions(&sbx, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            root.join("state.json"),
            r#"{"user-foreign": "11111111-1111-1111-1111-111111111111"}"#,
        )
        .unwrap();
        for dir in ["p1", "p2", "cache"] {
            fs::create_dir(root.join(dir)).unwrap();
        }
        let stock = StockSbx::new(
            &sbx,
            Arc::new(marsh_runtime::SystemCommandRunner::new(&root)),
        )
        .with_vm_ownership(root.join("vm-ownership.json"))
        .unwrap();
        let broker = DevBroker::new(Arc::new(stock), true, root.join("cache"), Vec::new());
        Fixture {
            _temp: temp,
            root,
            broker,
        }
    }

    /// One `DevSbx` call; returns (exit code or None for a refusal, output).
    /// The client reads frames as the broker sends them and closes after the
    /// final frame, as a real client does: the broker's graceful close waits for
    /// that close (up to its two-second drain bound), so a client that reads
    /// only after `serve` returns made every call cost the whole bound.
    fn call(fixture: &Fixture, session: &str, argv: &[&str]) -> (Option<i64>, String) {
        let (host, mut guest) = UnixStream::pair().unwrap();
        let argv = argv
            .iter()
            .map(|word| (*word).to_owned())
            .collect::<Vec<_>>();
        let client = thread::spawn(move || {
            let mut output = String::new();
            let result = loop {
                let Ok(frame) = crate::read_frame::<serde_json::Value>(&mut guest) else {
                    break (None, output);
                };
                match frame["type"].as_str() {
                    Some("output" | "diagnostic") => {
                        let bytes = frame["bytes"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|b| b.as_u64().unwrap() as u8)
                            .collect::<Vec<_>>();
                        output.push_str(&String::from_utf8_lossy(&bytes));
                    }
                    Some("exit") => break (frame["code"].as_i64(), output),
                    Some("error") => {
                        break (None, frame["message"].as_str().unwrap().to_owned());
                    }
                    _ => {}
                }
            };
            drop(guest);
            result
        });
        let _ = fixture.broker.serve(&host, session, &argv, None, None);
        drop(host);
        client.join().unwrap()
    }

    #[test]
    fn grants_are_confined_to_their_own_names_and_revocation_cleans_only_them() {
        let fixture = fixture();
        let shim = fixture.root.join("sbx");
        let one = fixture
            .broker
            .create_grant("s1", &fixture.root.join("p1"), &shim)
            .unwrap();
        let two = fixture
            .broker
            .create_grant("s2", &fixture.root.join("p2"), &shim)
            .unwrap();
        assert_ne!(one.prefix, two.prefix);
        let vm_one = format!("{}s-aaaaaaaa", one.prefix);
        let vm_two = format!("{}k-bbbbbbbb", two.prefix);
        assert_eq!(
            call(&fixture, "s1", &["create", "--name", &vm_one, "shell"]).0,
            Some(0)
        );
        assert_eq!(
            call(&fixture, "s2", &["create", "--name", &vm_two, "shell"]).0,
            Some(0)
        );
        // `run` without --name gets a fresh `<prefix>r-` child name.
        assert_eq!(
            call(&fixture, "s1", &["run", "-d", "shell", "/nonexistent-root"]).0,
            None
        );
        let p1 = fixture.root.join("p1");
        assert_eq!(
            call(
                &fixture,
                "s1",
                &["run", "-d", "shell", p1.to_str().unwrap()]
            )
            .0,
            Some(0)
        );
        let ran = fixture.broker.stock.dev_grants()[&one.id]
            .names
            .keys()
            .find(|name| name.starts_with(&format!("{}r-", one.prefix)))
            .cloned()
            .expect("run child adopted");
        assert_eq!(call(&fixture, "s1", &["exec", &ran, "true"]).0, Some(0));
        // Cross-grant, host-foreign, and other-session calls are refused in-band.
        for (session, argv) in [
            ("s2", vec!["exec", vm_one.as_str(), "true"]),
            ("s1", vec!["rm", "--force", vm_two.as_str()]),
            ("s1", vec!["stop", "user-foreign"]),
            ("s3", vec!["ls", "--json"]),
        ] {
            let (code, message) = call(&fixture, session, &argv);
            assert_eq!(code, None, "{argv:?} was admitted: {message}");
        }
        let (code, listing) = call(&fixture, "s1", &["ls", "--json"]);
        assert_eq!(code, Some(0));
        assert!(
            listing.contains(&vm_one)
                && !listing.contains(&vm_two)
                && !listing.contains("user-foreign")
        );
        assert_eq!(
            call(&fixture, "s1", &["exec", "-u", "root", &vm_one, "true"]).0,
            Some(0)
        );
        let log = fs::read_to_string(fixture.root.join("log")).unwrap();
        // Refused calls never reached stock: one exec (s1's own), no rm/stop.
        let execs = log
            .lines()
            .filter(|line| line.starts_with(r#"["exec""#))
            .collect::<Vec<_>>();
        assert_eq!(execs.len(), 2, "{log}");
        assert!(execs[1].contains("root"));
        assert!(
            !log.lines()
                .any(|line| line.starts_with(r#"["rm""#) || line.starts_with(r#"["stop""#))
        );
        // Revoking s1 removes exactly its VM and disposable scratch.
        fixture.broker.revoke_session("s1").unwrap();
        let state = fs::read_to_string(fixture.root.join("state.json")).unwrap();
        assert!(
            !state.contains(&vm_one)
                && !state.contains(&ran)
                && state.contains(&vm_two)
                && state.contains("user-foreign")
        );
        assert!(!one.scratch.join("tmp").exists() && one.scratch.join("artifacts").exists());
        assert_eq!(call(&fixture, "s1", &["ls", "--json"]).0, None);
        let grants = fixture.broker.stock.dev_grants();
        assert_eq!(grants.len(), 1);
        // Startup revocation cleans every persisted grant.
        fixture.broker.revoke_all().unwrap();
        let state = fs::read_to_string(fixture.root.join("state.json")).unwrap();
        assert!(!state.contains(&vm_two) && state.contains("user-foreign"));
        assert!(fixture.broker.stock.dev_grants().is_empty());
    }

    #[test]
    fn listing_shows_only_this_grants_rows() {
        let raw = br#"{"sandboxes":[{"name":"user-a","id":"1","status":"running"},{"name":"marsh-xab12c-s-00000000","id":"2","status":"running"},{"name":"marsh-xother-s-00000000","id":"3","status":"stopped"}]}"#;
        let names = BTreeMap::from([("marsh-xab12c-s-00000000".to_owned(), Some("2".to_owned()))]);
        let json = String::from_utf8(filter_listing(raw, &names, true).unwrap()).unwrap();
        assert!(json.contains("marsh-xab12c-s-00000000"));
        assert!(!json.contains("user-a") && !json.contains("xother"));
        let text = String::from_utf8(filter_listing(raw, &names, false).unwrap()).unwrap();
        assert_eq!(text.lines().count(), 2);

        let bare = br#"[{"name":"user-a","id":"1","status":"running"},{"name":"marsh-xab12c-s-00000000","id":"2","status":"running"}]"#;
        let json: serde_json::Value =
            serde_json::from_slice(&filter_listing(bare, &names, true).unwrap()).unwrap();
        assert_eq!(json.as_array().unwrap().len(), 1);
        assert_eq!(json[0]["name"], "marsh-xab12c-s-00000000");
    }
}
