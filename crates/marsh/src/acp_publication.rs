//! Host registration for a daemon-authorized ACP session publication.

use super::*;
use crate::mcp_load::HostPublication;
use marsh_daemon::{PublicationKind, PublicationOperation};
use marsh_mcp::AcpDeclaration;
use uuid::Uuid;

pub(super) fn run(arguments: &[String]) -> Result<i32, RunError> {
    match arguments {
        [operation, name, id, generation] if operation == "host-publish" => {
            publish_host(name, id, generation, None)
        }
        [operation, name, id, generation, option, sandbox]
            if operation == "host-publish" && option == "--sandbox" =>
        {
            publish_host(name, id, generation, Some(sandbox))
        }
        [operation, name] if operation == "host-unpublish" => {
            let mut transaction =
                HostPublication::open(PublicationKind::Acp, PublicationOperation::Unpublish, name)?;
            let result = require_daemon_dispatch(&transaction)
                .and_then(|()| unpublish(name, &mut transaction));
            transaction.finish(result)
        }
        [operation, client, name] if operation == "install-published" && client == "codex" => {
            codex_published_acp(name, false)
        }
        [operation, client, name] if operation == "remove-published" && client == "codex" => {
            codex_published_acp(name, true)
        }
        _ => Err(RunError::new(
            "usage: marsh acp install-published codex NAME | marsh acp remove-published codex NAME (publish/unpublish run inside the project shell)",
            2,
        )),
    }
}

fn publish_host(
    name: &str,
    id: &str,
    generation: &str,
    sandbox: Option<&str>,
) -> Result<i32, RunError> {
    let mut transaction =
        HostPublication::open(PublicationKind::Acp, PublicationOperation::Publish, name)?;
    let result = require_daemon_dispatch(&transaction)
        .and_then(|()| publish(name, id, generation, sandbox, &mut transaction));
    transaction.finish(result)
}

fn require_daemon_dispatch(transaction: &HostPublication) -> Result<(), RunError> {
    if transaction.context().is_none() {
        return Err(RunError::new(
            "ACP host publication requires the attached shell's daemon",
            2,
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)] // One publication transaction includes inspect, write, load, and rollback.
fn publish(
    name: &str,
    id: &str,
    generation: &str,
    sandbox: Option<&str>,
    transaction: &mut HostPublication,
) -> Result<i32, RunError> {
    let kit = transaction
        .context()
        .and_then(|context| context.kit.clone());
    marsh_daemon::validate_publication_options(None, kit.as_deref(), sandbox, false)
        .map_err(|error| RunError::new(error, 2))?;
    transaction.bind_arguments(sandbox, Some((id, generation)))?;
    for value in [id, generation] {
        if Uuid::parse_str(value).map_or(true, |parsed| parsed.to_string() != value) {
            return Err(RunError::new(
                "ACP publication IDs must be canonical UUIDs",
                2,
            ));
        }
    }
    let (registration, home, path) = published_acp_context(name)?;
    transaction.verify_project(&registration.workspace)?;
    let declaration = AcpDeclaration {
        schema_version: AcpDeclaration::SCHEMA_VERSION.into(),
        tool_name: name.into(),
        agent_session_id: id.into(),
        generation: generation.into(),
        canonical_workspace: registration.workspace.clone(),
    };
    declaration
        .validate()
        .map_err(|error| RunError::new(error, 2))?;
    let encoded = serde_json::to_vec(&declaration)
        .map_err(|error| RunError::new(format!("cannot encode ACP publication: {error}"), 1))?;
    let _lock = transaction.lock_scope(&path)?;
    reject_cross_protocol_name(name, true)?;
    let previous = match std::fs::symlink_metadata(&path) {
        Ok(metadata)
            if metadata.file_type().is_file()
                && metadata.uid() == nix::unistd::Uid::effective().as_raw()
                && metadata.mode().trailing_zeros() >= 6
                && metadata.nlink() == 1 =>
        {
            Some(std::fs::read(&path).map_err(|error| {
                RunError::new(format!("cannot read ACP publication: {error}"), 1)
            })?)
        }
        Ok(_) => {
            return Err(RunError::new(
                format!(
                    "existing ACP declaration {} must be one owner-only real file; inspect and repair its ownership/type/mode from the host, then run `acp unpublish {name}`",
                    path.display()
                ),
                1,
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(RunError::new(
                format!("cannot inspect ACP publication: {error}"),
                1,
            ));
        }
    };
    let prior_generation = previous
        .as_deref()
        .map(|bytes| {
            let prior: AcpDeclaration = serde_json::from_slice(bytes).map_err(|error| {
                RunError::new(format!("invalid existing ACP declaration {}: {error}; inspect and repair the owner-only declaration before `acp unpublish {name}`", path.display()), 1)
            })?;
            prior.validate().map_err(|error| RunError::new(format!("invalid ACP declaration {}: {error}; repair it from the host before `acp unpublish {name}`", path.display()), 1))?;
            Ok::<_, RunError>(prior.generation)
        })
        .transpose()?;
    let old_add =
        published_acp_sbx_add_command(&registration, &home, &path, prior_generation.as_deref())?;
    let add = published_acp_sbx_add_command(&registration, &home, &path, Some(generation))?;
    let registered =
        inspect_published_sbx_registration_with_prior_generation(&registration, &old_add, true)?;
    if registered && previous.is_none() {
        return Err(RunError::new(
            format!(
                "ACP registration exists without its owner declaration; run `acp unpublish {name}` to remove the stale registration, then publish again"
            ),
            1,
        ));
    }
    validate_publication_target(&registration, sandbox)?;
    // Keep the host file lock across preflight, daemon-owned preparation and
    // commit. The private inherited channel grants no general daemon access.
    let prepared = kit
        .as_deref()
        .map(|kit| transaction.prepare_kit(kit))
        .transpose()?;
    let sandbox = prepared.as_deref().or(sandbox);
    validate_mcp_publish_options(None, sandbox)?;
    if prepared.is_some() {
        validate_publication_target(&registration, sandbox)?;
    }
    transaction.verify_project(&registration.workspace)?;
    if std::fs::read(&path).ok() != previous {
        return Err(RunError::new(
            "ACP declaration changed during preparation; not published",
            1,
        ));
    }
    if prepared.is_some()
        && inspect_published_sbx_registration_with_prior_generation(&registration, &old_add, true)?
            != registered
    {
        return Err(RunError::new(
            format!(
                "ACP registration changed during preparation; not published. Run `acp unpublish {name}` from the controller, inspect the registration, then retry"
            ),
            1,
        ));
    }
    transaction.begin_commit()?;
    write_publication(&path, &encoded)?;
    if registered && let Err(error) = remove_sbx_registration(&registration) {
        rollback_replaced_publication(
            &registration,
            &path,
            previous.as_deref(),
            &old_add,
            None,
            true,
            transaction,
        )
        .map_err(|rollback| publication_rollback_error(name, id, &error, &rollback))?;
        return Err(error);
    }
    if let Err(error) = run_checked_mcp_command(&add, "register published ACP session") {
        rollback_replaced_publication(
            &registration,
            &path,
            previous.as_deref(),
            &old_add,
            Some(&add),
            registered,
            transaction,
        )
        .map_err(|rollback| publication_rollback_error(name, id, &error, &rollback))?;
        return Err(error);
    }
    if let Some(sandbox) = sandbox {
        let load = published_load_command(&registration, sandbox);
        if let Err(error) = run_checked_mcp_command(&load, "load published ACP session") {
            rollback_replaced_publication(
                &registration,
                &path,
                previous.as_deref(),
                &old_add,
                Some(&add),
                registered,
                transaction,
            )
            .map_err(|rollback| publication_rollback_error(name, id, &error, &rollback))?;
            return Err(error);
        }
    }
    transaction.message(format!(
        "Published ACP session {id} as MCP tool {name} for {}.",
        registration.workspace.display()
    ));
    transaction.message("Grant: clients that load this tool can prompt this agent, read its turn output, cancel turns, and select offered one-time allow/reject permissions. Allowed actions can use the Kit's project write access. They cannot choose another ACP session.");
    if previous.is_some() {
        transaction.message(
            "Previously loaded clients must reload this MCP tool; old generations are denied.",
        );
        transaction.message(format!(
            "If host Codex has the old tool, run: marsh acp remove-published codex {name}"
        ));
    }
    transaction.message(format!("Stock MCP server: {}", registration.server_name));
    if let Some(sandbox) = sandbox {
        transaction.message(format!(
            "Loaded into {sandbox}. Start a new client agent session to discover the tool."
        ));
    }
    transaction.message(format!(
        "Load into another sandbox with: sbx mcp load {} --sandbox SANDBOX",
        registration.server_name
    ));
    transaction.message(format!(
        "For new host Codex tasks: marsh acp install-published codex {name}"
    ));
    Ok(0)
}

fn publication_rollback_error(
    name: &str,
    id: &str,
    cause: &RunError,
    rollback: &RunError,
) -> RunError {
    RunError::new(
        format!(
            "{}; ACP rollback failed: {}. Run `acp status {id}` to inspect the agent and `acp unpublish {name}` to retry revocation/removal before publishing again",
            cause.message, rollback.message
        ),
        125,
    )
}

fn unpublish(name: &str, transaction: &mut HostPublication) -> Result<i32, RunError> {
    transaction.bind_arguments(None, None)?;
    let (registration, home, path) = published_acp_context(name)?;
    transaction.verify_project(&registration.workspace)?;
    let _lock = transaction.lock_scope(&path)?;
    let generation = match AcpDeclaration::load_private(&path) {
        Ok(declaration) => Some(declaration.generation),
        Err(_error) if !path.exists() => None,
        Err(error) => {
            return Err(RunError::new(
                format!(
                    "invalid ACP declaration {}: {error}; inspect and repair this owner-only host declaration before retrying `acp unpublish {name}`. No host registration was removed by this attempt",
                    path.display()
                ),
                1,
            ));
        }
    };
    let add = published_acp_sbx_add_command(&registration, &home, &path, generation.as_deref())?;
    let registered =
        inspect_published_sbx_registration_with_prior_generation(&registration, &add, true)?;
    transaction.begin_commit()?;
    let declared = match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(RunError::new(
                format!("cannot remove ACP publication: {error}"),
                1,
            ));
        }
    };
    if registered {
        remove_sbx_registration(&registration)?;
    }
    if declared || registered {
        transaction.message(format!(
            "Revoked {name}. Cached clients may display the tool, but its grant is inactive."
        ));
    } else {
        transaction.message(format!("No host ACP registration remains for {name} in this scope; its ACP grant is inactive. For a pipeline tool, use mcp unpublish {name}."));
    }
    Ok(0)
}
