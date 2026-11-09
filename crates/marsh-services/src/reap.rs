//! Reaping an attempt's containers when the attempt ends (E4; DECISIONS 11,
//! SECURITY-DESIGN A4; `docs/design/services.md`, "Reaping").
//!
//! The daemon asks for it when a job's attempt ends: on a normal exit, a cancel,
//! a crash, or the recovery after a daemon restart. A container is removed only
//! when the ledger records it for the workspace and the attempt **and** its labels
//! say the same. Names never select anything. A Compose project's container, or one
//! with a restart policy, is kept: a job starts those to persist in the workspace
//! (DECISIONS 11; the Compose label is a hint about what survives, not authority).
//! Named volumes are never removed. Shell objects (attempt `shell`) are not part of
//! any job's reap.
//!
//! Bounded and idempotent: the freeze waits (briefly) for requests of the
//! workspace already in flight, so a create that was running when the job ended
//! is either finished and reaped or refused; at most `MAX_CONTAINERS` are
//! considered and the whole pass has a deadline. Whatever is left is a gap, and
//! the daemon asks again.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::breaks::break_mode;
use crate::engine_api::{ATTEMPT_LABEL, SCOPE_LABEL};
use crate::policy::endpoint::Services;

/// Compose's project label: a container of a Compose project persists.
pub const COMPOSE_PROJECT_LABEL: &str = "com.docker.compose.project";
/// How long the pass waits for a request of the workspace already running.
const FREEZE_WAIT: Duration = Duration::from_secs(20);
/// The whole pass; the daemon's relay waits longer than this.
const DEADLINE: Duration = Duration::from_secs(45);
/// The grace a container gets to stop before it is killed.
const STOP_SECONDS: u32 = 2;
const MAX_CONTAINERS: usize = 256;
const MAX_GAPS: usize = 32;

/// What one pass did. `complete` means nothing was left to do: every container
/// of the attempt is gone, or was kept on purpose.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub removed: u32,
    pub kept: u32,
    pub gaps: Vec<String>,
    pub complete: bool,
}

impl Report {
    fn gap(&mut self, text: impl AsRef<str>) {
        if self.gaps.len() < MAX_GAPS {
            self.gaps.push(crate::capture::sanitize(text.as_ref(), 300));
        }
        self.complete = false;
    }
}

/// Whether a container survives the end of its attempt (DECISIONS 11): it belongs
/// to a Compose project, or it restarts (`always`, `unless-stopped`, `on-failure`).
#[must_use]
pub fn keeps(compose_project: bool, restart_policy: &str) -> bool {
    compose_project || !matches!(restart_policy, "" | "no")
}

/// The ids policy and the daemon use for a workspace or an attempt (the same rule
/// as the policy's authority check: short, no control characters, no leading dot).
#[must_use]
pub fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('.')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

/// Stops and removes the containers of `attempt` in `workspace` (see the module
/// docs). The break modes (`docs/design/test-breaks.md`) that act here:
/// `reap-all-scope` takes every container of the workspace, whatever its attempt,
/// and `reap-persistent` removes Compose and restart-policy containers too.
#[must_use]
pub fn reap(svc: &Arc<Services>, workspace: &str, attempt: &str) -> Report {
    let mut report = Report {
        complete: true,
        ..Report::default()
    };
    if !valid_id(workspace) || !valid_id(attempt) {
        report.gap("the workspace or the attempt is not a valid id; nothing was removed");
        return report;
    }
    let every_attempt = break_mode("reap-all-scope");
    let keep_persistent = !break_mode("reap-persistent");
    // Requests of the workspace already admitted finish first, and new ones wait
    // until the pass is over: nothing can be created behind the listing.
    let Some(_gate) = svc.freeze_scope_within(workspace, Some(FREEZE_WAIT)) else {
        report.gap("a request in this workspace is still running; nothing was removed");
        return report;
    };
    let mut owned: Vec<String> = svc
        .ledger
        .container_ids(workspace)
        .into_iter()
        .filter(|id| {
            svc.ledger.container(id).is_some_and(|rec| {
                rec.scope == workspace && (every_attempt || rec.attempt == attempt)
            })
        })
        .collect();
    owned.sort();
    if owned.len() > MAX_CONTAINERS {
        report.gap(format!(
            "{} containers; only the first {MAX_CONTAINERS} were considered",
            owned.len()
        ));
        owned.truncate(MAX_CONTAINERS);
    }
    let deadline = Instant::now() + DEADLINE;
    for (index, id) in owned.iter().enumerate() {
        if Instant::now() >= deadline {
            report.gap(format!(
                "the time limit passed with {} container(s) left",
                owned.len() - index
            ));
            break;
        }
        reap_one(
            svc,
            workspace,
            attempt,
            every_attempt,
            keep_persistent,
            id,
            &mut report,
        );
    }
    report
}

fn reap_one(
    svc: &Services,
    workspace: &str,
    attempt: &str,
    every_attempt: bool,
    keep_persistent: bool,
    id: &str,
    report: &mut Report,
) {
    let inspected = match svc.up_request("GET", &format!("/containers/{id}/json"), None) {
        Ok(answer) => answer,
        Err(error) => return report.gap(format!("{id}: inspect failed: {error}")),
    };
    let (status, body) = inspected;
    if status == 404 {
        // Already gone: the record is stale.
        return forget(svc, workspace, id, report);
    }
    if status != 200 {
        return report.gap(format!("{id}: the engine answered {status} to an inspect"));
    }
    let Ok(document) = serde_json::from_slice::<Value>(&body) else {
        return report.gap(format!("{id}: the engine's inspect is not JSON"));
    };
    let label = |key: &str| {
        document
            .pointer("/Config/Labels")
            .and_then(|labels| labels.get(key))
            .and_then(Value::as_str)
    };
    if label(SCOPE_LABEL) != Some(workspace)
        || (!every_attempt && label(ATTEMPT_LABEL) != Some(attempt))
    {
        // The ledger and the labels disagree: never remove on one alone.
        return report.gap(format!(
            "{id}: its labels do not match the ledger; left alone"
        ));
    }
    let restart = document
        .pointer("/HostConfig/RestartPolicy/Name")
        .and_then(Value::as_str)
        .unwrap_or("");
    if keep_persistent && keeps(label(COMPOSE_PROJECT_LABEL).is_some(), restart) {
        report.kept += 1;
        return;
    }
    // Stop (a grace, then the engine's own kill), then remove. The removal does
    // not ask for anonymous volumes either: volumes stay (DECISIONS 11).
    match svc.up_request(
        "POST",
        &format!("/containers/{id}/stop?t={STOP_SECONDS}"),
        None,
    ) {
        Ok((204 | 304 | 404, _)) => {}
        Ok((status, _)) => report.gap(format!("{id}: stop answered {status}; removing anyway")),
        Err(error) => report.gap(format!("{id}: stop failed ({error}); removing anyway")),
    }
    match svc.up_request("DELETE", &format!("/containers/{id}?force=true"), None) {
        Ok((204 | 404, _)) => {}
        Ok((status, _)) => return report.gap(format!("{id}: removal answered {status}")),
        Err(error) => return report.gap(format!("{id}: removal failed: {error}")),
    }
    forget(svc, workspace, id, report);
    report.removed += 1;
}

/// Drops the ledger record of a container that is gone, and releases its pins.
fn forget(svc: &Services, workspace: &str, id: &str, report: &mut Report) {
    match svc.ledger.remove_container(id) {
        Ok(freed) => svc.unpin_freed(freed),
        Err(error) => report.gap(format!(
            "{id} in {workspace}: removed but not forgotten: {error}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// U5: which containers survive the end of their attempt.
    #[test]
    fn compose_and_restart_policy_containers_persist_and_one_offs_do_not() {
        assert!(keeps(true, ""));
        assert!(keeps(true, "no"));
        assert!(keeps(false, "unless-stopped"));
        assert!(keeps(false, "always"));
        assert!(keeps(false, "on-failure"));
        assert!(!keeps(false, ""));
        assert!(!keeps(false, "no"));
    }
}
