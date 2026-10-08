//! Load an existing publication without changing its generation.

use super::*;
use marsh_daemon::{
    HostPublicationContext, PublicationCommit, PublicationHostEvent, PublicationKind,
    PublicationOperation, PublicationOutcome, PublishedName,
};
use std::io::Read as _;

/// One host CLI transaction. Human text is presentation, never its IPC result.
pub(super) struct HostPublication {
    context: Option<HostPublicationContext>,
    channel: Option<std::os::unix::net::UnixStream>,
    effects: bool,
    message: String,
}
impl HostPublication {
    pub(super) fn open(
        kind: PublicationKind,
        operation: PublicationOperation,
        name: &str,
    ) -> Result<Self, RunError> {
        use std::os::fd::AsFd as _;
        let name = PublishedName::parse(name).map_err(|error| RunError::new(error, 2))?;
        let mut transaction = Self {
            context: None,
            channel: None,
            effects: false,
            message: String::new(),
        };
        let initialization = (|| -> Result<(), RunError> {
            if env::var_os("MARSH_PUBLICATION_CHANNEL").is_some() {
                let fd = std::io::stdin()
                    .as_fd()
                    .try_clone_to_owned()
                    .map_err(|error| RunError::new(error.to_string(), 125))?;
                let channel = std::os::unix::net::UnixStream::from(fd);
                channel.peer_addr().map_err(|_| {
                    RunError::new(
                        "publication requires the attached daemon's private channel",
                        2,
                    )
                })?;
                // From this point a private peer exists: every no-effect refusal
                // is a typed terminal reply, not an unexplained host disconnect.
                transaction.channel = Some(channel);
                let channel = transaction
                    .channel
                    .as_mut()
                    .expect("installed private channel");
                channel
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .map_err(|error| RunError::new(error.to_string(), 125))?;
                channel
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .map_err(|error| RunError::new(error.to_string(), 125))?;
                let context: HostPublicationContext =
                    marsh_daemon::read_frame(channel).map_err(|error| {
                        RunError::new(format!("invalid publication context: {error}"), 125)
                    })?;
                if context.kind != kind
                    || context.operation != operation
                    || context.name != name
                    || context.session.ephemeral_home
                    || context.session.uid != nix::unistd::Uid::effective().as_raw()
                {
                    return Err(RunError::new(
                        "attached publication context does not match this operation",
                        2,
                    ));
                }
                let selected = SessionConfig::from_environment(false)
                    .map_err(|error| RunError::new(error.to_string(), 1))?;
                // The daemon accepts a selected home under a symlinked ancestor
                // (e.g. MARSH_HOME under /tmp -> /private/tmp) and keys its
                // scope and declarations by the canonical path, so compare
                // canonical paths on both sides. An unresolvable attached home
                // fails closed.
                let attached = context.session.home_backing.canonicalize().map_err(|_| {
                    RunError::new(
                        "attached publication context does not match selected home",
                        2,
                    )
                })?;
                if attached
                    != selected
                        .home_backing
                        .canonicalize()
                        .map_err(|error| RunError::new(error.to_string(), 1))?
                {
                    return Err(RunError::new(
                        "attached publication context does not match selected home",
                        2,
                    ));
                }
                channel
                    .set_read_timeout(None)
                    .map_err(|error| RunError::new(error.to_string(), 125))?;
                transaction.context = Some(context);
            } else if operation == PublicationOperation::Publish {
                let command = match kind {
                    PublicationKind::Mcp => "mcp publish NAME -- 'PIPELINE'",
                    PublicationKind::Acp => "acp publish ID --name NAME",
                };
                return Err(RunError::new(
                    format!(
                        "publication requires an attached project shell; run `marsh`, then `{command}`"
                    ),
                    2,
                ));
            }
            transaction.verify_project(
                &env::current_dir().map_err(|error| RunError::new(error.to_string(), 1))?,
            )?;
            Ok(())
        })();
        match initialization {
            Ok(()) => Ok(transaction),
            Err(error) => Err(transaction
                .finish(Err(error))
                .expect_err("initialization was rejected")),
        }
    }
    pub(super) fn bind_arguments(
        &self,
        sandbox: Option<&str>,
        agent: Option<(&str, &str)>,
    ) -> Result<(), RunError> {
        if let Some(context) = &self.context {
            let expected_agent = context
                .agent_session_id
                .as_deref()
                .zip(context.generation.as_deref());
            if context.sandbox.as_deref() != sandbox || expected_agent != agent {
                return Err(RunError::new(
                    "publication arguments do not match the admitted target/session generation",
                    2,
                ));
            }
        }
        Ok(())
    }
    pub(super) fn lock_scope(&self, path: &Path) -> Result<Option<std::fs::File>, RunError> {
        if let Some(context) = &self.context
            && context.scope_admitted
        {
            let (actual, owner) = marsh_daemon::PublicationScope::from_declaration_path(path)
                .map_err(|error| RunError::new(error, 1))?;
            let expected = marsh_daemon::PublicationScope::new(
                context.kind,
                &context.session.launch_directory,
                &context.session.home_backing,
            );
            if actual != expected {
                return Err(RunError::new(
                    "publication lock scope differs from admitted context",
                    2,
                ));
            }
            let lock_path = actual.lock_path(&owner).canonicalize().map_err(|error| {
                RunError::new(
                    format!("publication admission lock unavailable: {error}"),
                    1,
                )
            })?;
            if context.admission_lock.as_deref() != Some(lock_path.as_path()) {
                return Err(RunError::new(
                    "publication control directory differs from the daemon's retained admission lock",
                    2,
                ));
            }
            // The daemon owns the real flock until the host and every stock
            // carrier settle, even if this CLI is killed or loses its socket.
            return Ok(None);
        }
        lock_publication(path).map(Some)
    }
    pub(super) fn begin_rollback(&mut self) -> Result<(), RunError> {
        self.effects = true;
        if self.channel.is_some()
            && self
                .exchange(&PublicationHostEvent::BeginRollback)?
                .is_some()
        {
            return Err(RunError::new(
                "invalid publication rollback acknowledgment",
                125,
            ));
        }
        Ok(())
    }
    pub(super) fn context(&self) -> Option<&HostPublicationContext> {
        self.context.as_ref()
    }
    pub(super) fn prefix(&self) -> &'static str {
        if self.context.is_some() { "" } else { "marsh " }
    }
    pub(super) fn verify_project(&self, workspace: &Path) -> Result<(), RunError> {
        if let Some(context) = &self.context {
            validate_project_identity(
                workspace,
                &context.session.launch_directory,
                context.project_identity.0,
                context.project_identity.1,
            )
        } else {
            validate_pinned_guest_project(workspace).map(|_| ())
        }
    }
    pub(super) fn message(&mut self, line: impl std::fmt::Display) {
        use std::fmt::Write as _;
        writeln!(self.message, "{line}").expect("String write cannot fail");
    }
    pub(super) fn begin_commit(&mut self) -> Result<(), RunError> {
        self.effects = true;
        if self.channel.is_some() && self.exchange(&PublicationHostEvent::BeginCommit)?.is_some() {
            return Err(RunError::new(
                "invalid publication commit acknowledgment",
                125,
            ));
        }
        Ok(())
    }
    pub(super) fn prepare_kit(&mut self, kit: &str) -> Result<String, RunError> {
        if self
            .context
            .as_ref()
            .and_then(|context| context.kit.as_deref())
            != Some(kit)
        {
            return Err(RunError::new(
                "--kit requires the attached shell's admitted Kit target",
                2,
            ));
        }
        self.effects = true;
        self.exchange(&PublicationHostEvent::PrepareKit)?
            .ok_or_else(|| RunError::new("Kit preparation omitted its exact sandbox", 125))
    }
    fn exchange(&mut self, event: &PublicationHostEvent) -> Result<Option<String>, RunError> {
        let channel = self
            .channel
            .as_mut()
            .ok_or_else(|| RunError::new("attached publication channel unavailable", 125))?;
        marsh_daemon::write_frame(channel, event).map_err(|error| {
            RunError::new(
                format!("publication request delivery lost: {error}; not remote cancellation"),
                125,
            )
        })?;
        let result: Result<Option<String>, String> = marsh_daemon::read_frame(channel)
            .map_err(|error| RunError::new(format!("publication reply lost: {error}; preparation or commit may still complete (not cancelled)"), 125))?;
        result.map_err(|error| RunError::new(error, 125))
    }
    pub(super) fn finish(mut self, result: Result<i32, RunError>) -> Result<i32, RunError> {
        let outcome = match &result {
            Ok(_) => PublicationOutcome::Committed(PublicationCommit {
                message: self.message.trim_end_matches('\n').to_owned(),
            }),
            Err(error) if self.effects => PublicationOutcome::uncertain(&error.message),
            Err(error) => PublicationOutcome::rejected(&error.message),
        };
        if let Some(channel) = &mut self.channel {
            marsh_daemon::write_frame(channel, &PublicationHostEvent::Complete(outcome.clone()))
                .map_err(|error| RunError::new(format!("publication result delivery lost: {error}; inspect state before retrying"), 125))?;
        } else if let PublicationOutcome::Committed(commit) = &outcome {
            println!("{}", commit.message);
        }
        result.map_err(|error| {
            RunError::new(
                outcome.to_string(),
                if self.effects { 125 } else { error.status },
            )
        })
    }
}

pub(super) fn load(
    name: &str,
    kit: Option<&str>,
    sandbox: Option<&str>,
    transaction: &mut HostPublication,
) -> Result<i32, RunError> {
    marsh_daemon::validate_publication_options(None, kit, sandbox, true)
        .map_err(|error| RunError::new(error, 2))?;
    if kit.is_some() && transaction.context().is_none() {
        return Err(RunError::new(
            "--kit needs an attached project shell; run `marsh`, then `mcp load NAME --kit KIT`. For an existing sandbox use `marsh mcp load NAME --sandbox SANDBOX`",
            2,
        ));
    }
    transaction.bind_arguments(sandbox, None)?;
    let (registration, home, path) = published_mcp_context(name)?;
    let session = transaction.context().map(|context| &context.session);
    if let Some(session) = session
        && (session.launch_directory != registration.workspace
            // `home` is canonical; the attached session may name it through a
            // symlinked ancestor.
            || session.home_backing.canonicalize().ok() != Some(home.join("home"))
            || session.uid != nix::unistd::Uid::effective().as_raw()
            || session.ephemeral_home)
    {
        return Err(RunError::new(
            "attached MCP load context does not match publication scope",
            1,
        ));
    }
    // This is the same owner lock used by host publish/unpublish. It also
    // covers Kit preparation, so an absent/stale tool cannot boot a Kit first.
    // Reject missing names before allocating even the bounded scope lock.
    read_current(name, &path, &registration.workspace)?;
    let _lock = transaction.lock_scope(&path)?;
    check_revocation(&path)?;
    let (declaration, bytes) = read_current(name, &path, &registration.workspace)?;
    reject_cross_protocol_name(name, false)?;
    let add = published_sbx_add_command_with_generation(
        &registration,
        &home,
        &path,
        declaration.publication_generation.as_deref(),
    )?;
    require_registration(&registration, &add)?;
    validate_publication_target(&registration, sandbox)?;
    let sandbox = match (kit, sandbox) {
        (None, Some(sandbox)) => sandbox.to_owned(),
        (Some(kit), None) => transaction.prepare_kit(kit)?,
        _ => {
            return Err(RunError::new(
                "MCP load requires exactly one of --kit or --sandbox",
                2,
            ));
        }
    };
    validate_mcp_publish_options(None, Some(&sandbox))?;
    if kit.is_some() {
        validate_publication_target(&registration, Some(&sandbox))?;
    }
    // Recheck after potentially slow preparation. Never accept an older
    // generation, legacy registration, or a different owner's same-name entry.
    transaction.verify_project(&registration.workspace)?;
    check_revocation(&path)?;
    if read_current(name, &path, &registration.workspace)?.1 != bytes {
        return Err(RunError::new("MCP declaration changed during load", 1));
    }
    require_registration(&registration, &add)?;
    check_revocation(&path)?;
    transaction.begin_commit()?;
    run_checked_mcp_command(
        &published_load_command(&registration, &sandbox),
        "load published MCP tool",
    )?;
    transaction.message(format!(
        "Loaded MCP tool: {name}\nServer: {}\nSandbox: {sandbox}",
        registration.server_name
    ));
    transaction.message("Publication unchanged; existing loaded clients remain valid. Start a new agent session in the target sandbox to discover the tool.");
    transaction.message("Access: every client and later job in that sandbox can run this fixed pipeline in its publishing project, with the project shell's privileges (including sudo and its private Docker Engine), and read its output. Network policy is unchanged.");
    transaction.message(format!(
        "Revoke: {}mcp unpublish {name}",
        transaction.prefix()
    ));
    Ok(0)
}

/// During an attached publication, the daemon is the stock process owner.
/// This uses the existing unbuffered private socket sequentially with other
/// transaction events; ordinary host client commands keep their own route.
pub(super) fn stock_command(
    command: &McpClientCommand,
) -> Option<std::io::Result<std::process::Output>> {
    env::var_os("MARSH_PUBLICATION_CHANNEL")?;
    Some((|| {
        use std::os::{fd::AsFd as _, unix::process::ExitStatusExt as _};
        let arguments = command
            .arguments
            .iter()
            .map(|argument| {
                argument.to_str().map(str::to_owned).ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "publication stock arguments must be UTF-8",
                    )
                })
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        let fd = std::io::stdin().as_fd().try_clone_to_owned()?;
        let mut channel = std::os::unix::net::UnixStream::from(fd);
        channel.peer_addr()?;
        channel.set_write_timeout(Some(Duration::from_secs(5)))?;
        marsh_daemon::write_frame(&mut channel, &PublicationHostEvent::RunStock { arguments })
            .map_err(std::io::Error::other)?;
        let result: Result<marsh_daemon::PublicationStockOutput, String> =
            marsh_daemon::read_frame(&mut channel).map_err(std::io::Error::other)?;
        let output = result.map_err(std::io::Error::other)?;
        let code = output
            .exit_code
            .filter(|code| (0..=255).contains(code))
            .ok_or_else(|| {
                std::io::Error::other("stock carrier returned no valid observed exit status")
            })?;
        Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    })())
}

pub(super) fn check_revocation(path: &Path) -> Result<(), RunError> {
    match std::fs::symlink_metadata(path.with_extension("revoke")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(RunError::new(
            format!("cannot check pending revocation: {error}"),
            1,
        )),
        Ok(_) => Err(RunError::new(
            "MCP revocation is pending; tool not loaded. Finish `mcp unpublish NAME` before republishing",
            1,
        )),
    }
}

fn require_registration(
    registration: &McpRegistration,
    add: &McpClientCommand,
) -> Result<(), RunError> {
    if !inspect_published_sbx_registration(registration, add)? {
        return Err(RunError::new(
            "publication is not registered with stock SBX; load cannot recreate it. Republish with `mcp publish NAME -- 'PIPELINE'` from the publishing shell",
            1,
        ));
    }
    Ok(())
}

fn read_current(
    name: &str,
    path: &Path,
    workspace: &Path,
) -> Result<(ToolDeclaration, Vec<u8>), RunError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| {
            RunError::new(if error.kind() == std::io::ErrorKind::NotFound {
                format!("no publication named {name} in this project and home; open `marsh`, then publish it with `mcp publish {name} -- 'PIPELINE'`")
            } else {
                format!("cannot load existing publication {name}: {error}")
            }, 1)
        })?;
    let metadata = file
        .metadata()
        .map_err(|error| RunError::new(error.to_string(), 1))?;
    if !metadata.is_file()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
        || metadata.len() > 1_048_576
    {
        return Err(RunError::new(
            "publication must be a bounded owner-only real file",
            1,
        ));
    }
    let mut bytes = Vec::new();
    file.take(1_048_577)
        .read_to_end(&mut bytes)
        .map_err(|error| RunError::new(error.to_string(), 1))?;
    if bytes.len() > 1_048_576 {
        return Err(RunError::new("publication exceeds size limit", 1));
    }
    let text = std::str::from_utf8(&bytes).map_err(|error| RunError::new(error.to_string(), 1))?;
    let declaration = ToolDeclaration::load_from_str(text, Some(name))
        .map_err(|error| RunError::new(format!("invalid MCP publication: {error}"), 1))?;
    if declaration.tool_name != name
        || declaration.pipeline.is_none()
        || declaration.publication_generation.is_none()
        || declaration.canonical_workspace.as_deref() != Some(workspace)
        || declaration.workspace_identity
            != Some(
                WorkspaceIdentity::for_directory(workspace)
                    .map_err(|error| RunError::new(error, 1))?,
            )
    {
        return Err(RunError::new(
            "publication does not match this project identity or has no generation",
            1,
        ));
    }
    Ok((declaration, bytes))
}
