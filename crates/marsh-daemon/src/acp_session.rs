//! Daemon-owned local ACP sessions over supervised Kit jobs.

use crate::{
    AcpAttachmentBridge, AcpAttachmentStatus, AttachmentFrame, ClientExecution, DaemonBackend,
    DaemonError, DaemonStore, ExecuteSpec, JobReceipt, JobState, ServerAttachment, SessionSpec,
};
use marsh_acp::{
    AcpClient, AcpClientConfig, ContentBlock, JsonRpcRequest, PermissionHandler, PermissionOption,
    PromptRequest, RequestId, RequestPermissionOutcome, RequestPermissionRequest, StopReason,
    transport::DEFAULT_MAX_FRAME_BYTES,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    runtime::Runtime,
    sync::{mpsc, oneshot},
};
use uuid::Uuid;

const MAX_SESSIONS: usize = 64;
const MAX_PROMPT_ADMISSIONS: usize = 1024;
const MAX_UPDATES_BYTES: usize = 512 * 1024;
const MAX_STATUS_PAGE_BYTES: usize = 256 * 1024;
const MAX_STATUS_PAGE_UPDATES: usize = 32;

// Never echo agent-controlled errors (secrets, paths or terminal escapes).
fn safe_turn_error(error: &marsh_acp::AcpError) -> String {
    use marsh_acp::AcpError;
    match error {
        AcpError::JsonRpc { code, .. } => format!(
            "Agent rejected the turn (JSON-RPC {code}); check agent authentication/configuration, then submit a new turn."
        ),
        AcpError::Timeout { .. } => {
            "ACP turn timed out; cancel or stop the session before retrying.".into()
        }
        AcpError::TransportLost(_) | AcpError::Io(_) => {
            "ACP transport lost; stop this session and start a new agent.".into()
        }
        _ => "Invalid ACP response; inspect the agent adapter and start a new session.".into(),
    }
}

fn validate_prompt_frame(remote_session_id: &str, text: &str) -> Result<(), DaemonError> {
    if text.len() > DEFAULT_MAX_FRAME_BYTES {
        return Err(DaemonError::InvalidState("ACP prompt exceeds 1 MiB UTF-8; shorten it or split it across turns. This prompt was not admitted.".into()));
    }
    // Use the real protocol types and a maximum-width request ID: JSON
    // escaping can make a prompt much larger than its UTF-8 text.
    let params = serde_json::to_value(PromptRequest {
        session_id: remote_session_id.into(),
        prompt: vec![ContentBlock::text(text.to_owned())],
    })
    .map_err(|error| DaemonError::InvalidState(error.to_string()))?;
    let frame = JsonRpcRequest::new(RequestId::Number(i64::MIN), "session/prompt", Some(params));
    if serde_json::to_vec(&frame)
        .map_err(|error| DaemonError::InvalidState(error.to_string()))?
        .len()
        > DEFAULT_MAX_FRAME_BYTES
    {
        return Err(DaemonError::InvalidState(
            "encoded ACP prompt exceeds the 1 MiB frame limit including escaping/session envelope; shorten it or split it across turns. This prompt was not admitted.".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcpUpdate {
    pub cursor: u64,
    pub turn_id: Option<String>,
    pub update: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[allow(clippy::struct_excessive_bools)] // Independent observed lifecycle facts.
pub struct AcpSessionStatus {
    pub agent_session_id: String,
    pub adapter: String,
    pub controller_shell_session_id: Option<String>,
    pub published_name: Option<String>,
    pub turn_active: bool,
    pub current_turn_id: Option<String>,
    pub last_turn_id: Option<String>,
    pub turn_start_cursor: u64,
    /// Bounded by the 1024-entry prompt-key ledger; contains no prompt text.
    pub turns: BTreeMap<String, AcpTurnReceipt>,
    pub dropped_updates: u64,
    pub out_of_turn_updates: u64,
    pub permission_note: Option<String>,
    pub cancel_requested: bool,
    pub stopping: bool,
    pub last_stop_reason: Option<StopReason>,
    pub last_error: Option<String>,
    pub updates: Vec<AcpUpdate>,
    pub next_cursor: u64,
    pub latest_cursor: u64,
    pub retained_after: u64,
    pub more_updates: bool,
    pub updates_lost: bool,
    pub permissions: Vec<AcpPendingPermission>,
    pub attachment: AcpAttachmentStatus,
    pub receipt: Option<JobReceipt>,
    /// Optional presentation fields omitted to keep the complete control frame bounded.
    /// Turn receipts and permission choice IDs remain available; this is not capture loss.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub status_omissions: Vec<String>,
}

impl AcpSessionStatus {
    fn exceeds_wire_budget(&self) -> bool {
        // Reserve the tagged PublicReply envelope, not merely the update bytes.
        serde_json::to_vec(self).map_or(true, |bytes| bytes.len() > crate::MAX_FRAME_BYTES - 128)
    }

    fn fit_wire_budget(mut self, after: u64) -> Self {
        if !self.exceeds_wire_budget() {
            return self;
        }
        if !self.attachment.stderr.is_empty() {
            self.attachment.stderr.clear();
            self.status_omissions.push("attachment.stderr".into());
        }
        if self.exceeds_wire_budget() {
            let mut omitted = false;
            for permission in &mut self.permissions {
                omitted |= permission.input_preview.take().is_some();
                omitted |= permission.title.take().is_some();
            }
            if omitted {
                self.status_omissions.push("permission_previews".into());
            }
        }
        if self.exceeds_wire_budget() && self.receipt.take().is_some() {
            self.status_omissions.push("job_receipt".into());
        }
        if self.exceeds_wire_budget()
            && let Some(Err(message)) = &mut self.attachment.terminal
        {
            *message = "Attachment diagnostic omitted; inspect the Kit job receipt".into();
            self.status_omissions
                .push("attachment.terminal_diagnostic".into());
        }
        let mut shortened = false;
        while self.exceeds_wire_budget() && !self.updates.is_empty() {
            self.updates.pop();
            shortened = true;
        }
        if shortened {
            // A presentation budget must not advance past retained but undelivered
            // updates, even if a later oversized update moved the retention floor.
            self.next_cursor = self.updates.last().map_or(after, |update| update.cursor);
            self.more_updates = true;
            self.status_omissions.push("update_page".into());
            // Adding the marker can itself cross the boundary by a few bytes.
            while self.exceeds_wire_budget() && !self.updates.is_empty() {
                self.updates.pop();
            }
            self.next_cursor = self.updates.last().map_or(after, |update| update.cursor);
        }
        self
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcpPendingPermission {
    pub request_id: String,
    pub tool_call_id: String,
    pub title: Option<String>,
    pub input_preview: Option<String>,
    pub options: Vec<PermissionOption>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcpTurnReceipt {
    pub start_cursor: u64,
    pub end_cursor: Option<u64>,
    pub stop_reason: Option<StopReason>,
    pub error: Option<String>,
    pub permission_note: Option<String>,
    pub dropped_updates: u64,
    /// Highest cursor actually omitted/evicted from this turn, not another turn.
    pub retained_after: u64,
}

struct PendingPermission {
    view: AcpPendingPermission,
    response: oneshot::Sender<RequestPermissionOutcome>,
}

#[derive(Clone)]
struct AcpPermissionInbox {
    pending: Arc<Mutex<BTreeMap<String, PendingPermission>>>,
    timeout: Duration,
    turn: Arc<Mutex<Option<Arc<Mutex<TurnLog>>>>>,
}

impl Default for AcpPermissionInbox {
    fn default() -> Self {
        Self {
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            timeout: Duration::from_mins(4),
            turn: Arc::new(Mutex::new(None)),
        }
    }
}

struct PendingPermissionGuard {
    inbox: AcpPermissionInbox,
    id: String,
}

impl Drop for PendingPermissionGuard {
    fn drop(&mut self) {
        self.inbox
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
    }
}

impl AcpPermissionInbox {
    fn set_turn_log(&self, turn: Arc<Mutex<TurnLog>>) {
        *self
            .turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(turn);
    }

    fn turn_log(&self) -> Option<Arc<Mutex<TurnLog>>> {
        self.turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn no_one_time_option(turn: Option<&Arc<Mutex<TurnLog>>>) {
        if let Some(turn) = turn {
            turn.lock().unwrap_or_else(std::sync::PoisonError::into_inner).permission_note =
                Some("Permission cancelled: agent offered only persistent choices; ask it to offer allow_once or reject_once.".into());
        }
    }

    fn pending(&self) -> Vec<AcpPendingPermission> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|item| item.view.clone())
            .collect()
    }

    fn respond(&self, request_id: &str, option_id: &str) -> Result<(), DaemonError> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let offered = pending
            .get(request_id)
            .ok_or_else(|| DaemonError::NotFound(request_id.into()))?;
        if !offered.view.options.iter().any(|option| {
            option.option_id == option_id
                && matches!(option.kind.as_str(), "allow_once" | "reject_once")
        }) {
            return Err(DaemonError::InvalidState(
                "select an offered one-time permission option".into(),
            ));
        }
        let item = pending
            .remove(request_id)
            .ok_or_else(|| DaemonError::NotFound(request_id.into()))?;
        item.response
            .send(RequestPermissionOutcome::select(option_id))
            .map_err(|_| DaemonError::InvalidState("permission request expired".into()))
    }

    fn cancel_all(&self) {
        let pending = std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for item in pending.into_values() {
            let _ = item.response.send(RequestPermissionOutcome::Cancelled);
        }
    }
}

impl PermissionHandler for AcpPermissionInbox {
    fn handle_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> Pin<Box<dyn Future<Output = RequestPermissionOutcome> + Send>> {
        let inbox = self.clone();
        Box::pin(async move {
            let turn = inbox.turn_log();
            if request.options.is_empty()
                || request.options.len() > 16
                || request.tool_call.tool_call_id.len() > 128
                || request.options.iter().any(|option| {
                    option.option_id.is_empty()
                        || option.option_id.len() > 128
                        || option.option_id.chars().any(char::is_control)
                        || option.name.len() > 128
                        || !matches!(
                            option.kind.as_str(),
                            "allow_once" | "allow_always" | "reject_once" | "reject_always"
                        )
                })
                || request.options.iter().enumerate().any(|(index, option)| {
                    request.options[..index]
                        .iter()
                        .any(|prior| prior.option_id == option.option_id)
                })
            {
                return RequestPermissionOutcome::Cancelled;
            }
            let options: Vec<_> = request
                .options
                .into_iter()
                .filter(|option| matches!(option.kind.as_str(), "allow_once" | "reject_once"))
                .collect();
            if options.is_empty() {
                Self::no_one_time_option(turn.as_ref());
                return RequestPermissionOutcome::Cancelled;
            }
            let id = Uuid::new_v4().to_string();
            let (send, receive) = oneshot::channel();
            let view = AcpPendingPermission {
                request_id: id.clone(),
                tool_call_id: request.tool_call.tool_call_id,
                title: request
                    .tool_call
                    .title
                    .map(|value| value.chars().take(256).collect()),
                input_preview: request.tool_call.raw_input.and_then(|input| {
                    serde_json::to_string(&input)
                        .ok()
                        .map(|value| value.chars().take(4096).collect())
                }),
                options,
            };
            {
                let mut pending = inbox
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if pending.len() >= 16 {
                    return RequestPermissionOutcome::Cancelled;
                }
                pending.insert(
                    id.clone(),
                    PendingPermission {
                        view,
                        response: send,
                    },
                );
            }
            let _guard = PendingPermissionGuard {
                inbox: inbox.clone(),
                id,
            };
            tokio::time::timeout(inbox.timeout, receive)
                .await
                .ok()
                .and_then(Result::ok)
                .unwrap_or(RequestPermissionOutcome::Cancelled)
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcpSessionSummary {
    pub agent_session_id: String,
    pub adapter: String,
    pub job_id: Option<String>,
    pub owner_shell_session_id: String,
    pub turn_active: bool,
    pub terminal: bool,
    pub published_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

struct PendingAcp {
    adapter: String,
    caller: SessionSpec,
    project_identity: Option<(u64, u64)>,
    created: Instant,
    claimed: bool,
    failed_at: Option<Instant>,
    startup: Option<Arc<AcpStartupAbort>>,
}

const UNCLAIMED_RESERVATION_TTL: Duration = Duration::from_secs(15);

impl PendingAcp {
    fn fail(&mut self) {
        self.failed_at.get_or_insert_with(Instant::now);
    }
}

struct TurnLog {
    active: bool,
    current_turn_id: Option<String>,
    last_turn_id: Option<String>,
    turn_start_cursor: u64,
    turns: BTreeMap<String, AcpTurnReceipt>,
    cancel_in_flight: Option<String>,
    permission_note: Option<String>,
    controller_session_id: Option<String>,
    cancel_requested: bool,
    last_stop_reason: Option<StopReason>,
    last_error: Option<String>,
    next_cursor: u64,
    dropped_through: u64,
    updates: VecDeque<(AcpUpdate, usize)>,
    update_bytes: usize,
    dropped_updates_at_start: u64,
    had_transport_loss: bool,
    admissions: BTreeMap<String, PromptAdmission>,
}

struct PromptAdmission {
    controller_session_id: String,
    prompt_digest: [u8; 32],
    turn_id: String,
}

impl Default for TurnLog {
    fn default() -> Self {
        Self {
            active: false,
            current_turn_id: None,
            last_turn_id: None,
            turn_start_cursor: 0,
            turns: BTreeMap::new(),
            cancel_in_flight: None,
            permission_note: None,
            controller_session_id: None,
            cancel_requested: false,
            last_stop_reason: None,
            last_error: None,
            next_cursor: 1,
            dropped_through: 0,
            updates: VecDeque::new(),
            update_bytes: 0,
            dropped_updates_at_start: 0,
            had_transport_loss: false,
            admissions: BTreeMap::new(),
        }
    }
}

impl TurnLog {
    fn admit_prompt(
        &mut self,
        key: &str,
        admission: PromptAdmission,
        job_ended: bool,
        dropped_updates: u64,
    ) -> Result<Option<String>, DaemonError> {
        if let Some(prior) = self.admissions.get(key) {
            if prior.controller_session_id != admission.controller_session_id
                || prior.prompt_digest != admission.prompt_digest
            {
                return Err(DaemonError::InvalidState(
                    "ACP prompt key is bound to another controller or prompt".into(),
                ));
            }
            return Ok(Some(prior.turn_id.clone()));
        }
        if job_ended {
            return Err(DaemonError::InvalidState("ACP Kit job has ended".into()));
        }
        if self.active || self.cancel_in_flight.is_some() {
            return Err(DaemonError::InvalidState(
                "ACP turn is already active".into(),
            ));
        }
        if self.admissions.len() >= MAX_PROMPT_ADMISSIONS {
            return Err(DaemonError::InvalidState(
                "ACP prompt key ledger is full; start a new session".into(),
            ));
        }
        self.active = true;
        self.had_transport_loss = false;
        self.permission_note = None;
        self.turn_start_cursor = self.next_cursor.saturating_sub(1);
        self.turns.insert(
            admission.turn_id.clone(),
            AcpTurnReceipt {
                start_cursor: self.turn_start_cursor,
                ..Default::default()
            },
        );
        self.current_turn_id = Some(admission.turn_id.clone());
        self.controller_session_id = Some(admission.controller_session_id.clone());
        self.cancel_requested = false;
        self.last_stop_reason = None;
        self.last_error = None;
        self.dropped_updates_at_start = dropped_updates;
        self.admissions.insert(key.into(), admission);
        Ok(None)
    }

    fn complete_turn(
        &mut self,
        turn_id: &str,
        result: Result<marsh_acp::PromptResponse, marsh_acp::AcpError>,
        dropped: u64,
    ) -> serde_json::Value {
        self.had_transport_loss = dropped > 0;
        let mut outcome = match result {
            Ok(response) => {
                self.active = false;
                self.last_stop_reason = Some(response.stop_reason);
                serde_json::json!({"stop_reason":response.stop_reason})
            }
            Err(error) => {
                // A deadline cannot prove that the agent stopped computing.
                self.active = matches!(error, marsh_acp::AcpError::Timeout { .. });
                self.last_error = Some(safe_turn_error(&error));
                serde_json::json!({"error":self.last_error,"still_active":self.active})
            }
        };
        self.last_turn_id = Some(turn_id.into());
        self.turns.insert(
            turn_id.into(),
            AcpTurnReceipt {
                start_cursor: self.turn_start_cursor,
                end_cursor: Some(self.next_cursor.saturating_sub(1)),
                stop_reason: self.last_stop_reason,
                error: self.last_error.clone(),
                permission_note: self.permission_note.clone(),
                dropped_updates: dropped,
                retained_after: self.turns.get(turn_id).map_or(0, |r| r.retained_after),
            },
        );
        outcome["updates_lost"] = serde_json::json!(dropped > 0);
        outcome["dropped_updates"] = serde_json::json!(dropped);
        outcome
    }

    fn record(&mut self, update: serde_json::Value) {
        let bytes = serde_json::to_vec(&update).map_or(0, |encoded| encoded.len());
        let cursor = self.next_cursor;
        self.next_cursor += 1;
        if bytes > MAX_STATUS_PAGE_BYTES {
            self.dropped_through = cursor;
            if let Some(receipt) = self
                .current_turn_id
                .as_ref()
                .and_then(|id| self.turns.get_mut(id))
            {
                receipt.retained_after = cursor;
            }
            return;
        }
        self.update_bytes += bytes;
        self.updates.push_back((
            AcpUpdate {
                cursor,
                turn_id: self.current_turn_id.clone(),
                update,
            },
            bytes,
        ));
        while self.update_bytes > MAX_UPDATES_BYTES {
            if let Some((removed, bytes)) = self.updates.pop_front() {
                self.update_bytes -= bytes;
                self.dropped_through = self.dropped_through.max(removed.cursor);
                if let Some(receipt) = removed
                    .turn_id
                    .as_ref()
                    .and_then(|id| self.turns.get_mut(id))
                {
                    receipt.retained_after = receipt.retained_after.max(removed.cursor);
                }
            }
        }
    }

    fn page(&self, after: u64) -> (Vec<AcpUpdate>, u64, bool) {
        let mut updates = Vec::new();
        let mut page_bytes = 0_usize;
        for (item, bytes) in self.updates.iter().filter(|(item, _)| item.cursor > after) {
            if updates.len() >= MAX_STATUS_PAGE_UPDATES
                || page_bytes.saturating_add(*bytes) > MAX_STATUS_PAGE_BYTES
            {
                break;
            }
            page_bytes += bytes;
            updates.push(item.clone());
        }
        let next_cursor = updates
            .last()
            .map_or_else(|| after.max(self.dropped_through), |item| item.cursor);
        let more = self
            .updates
            .back()
            .is_some_and(|(item, _)| item.cursor > next_cursor);
        (updates, next_cursor, more)
    }
}

struct AcpSession {
    // Never hold the manager's sessions lock while taking controller/turn.
    // For one session, take controller before turn; release each before a
    // blocking transport call when authorization permits.
    id: String,
    created: Instant,
    adapter: String,
    owner_shell_session_id: String,
    uid: u32,
    project: PathBuf,
    project_identity: Option<(u64, u64)>,
    home_backing: PathBuf,
    controller: Mutex<ControllerState>,
    remote_session_id: String,
    client: Arc<AcpClient>,
    bridge: AcpAttachmentBridge,
    stopping: AtomicBool,
    run_lease_ended: AtomicBool,
    turn: Arc<Mutex<TurnLog>>,
    permissions: AcpPermissionInbox,
    store: DaemonStore,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ControllerOwner {
    Shell(String),
    Published(String),
}

#[derive(Clone, Debug)]
struct PublicationGrant {
    name: String,
    generation: String,
    publisher_session_id: String,
}

struct ControllerState {
    owner: Option<ControllerOwner>,
    publication: Option<PublicationGrant>,
}

impl AcpSession {
    fn check_scope(&self, caller: &SessionSpec) -> Result<(), DaemonError> {
        if self.uid != caller.uid
            || self.project != caller.launch_directory
            || self.home_backing != caller.home_backing
            || self.project_identity.is_some_and(|expected| {
                crate::project_identity(&caller.launch_directory) != Some(expected)
            })
        {
            return Err(DaemonError::InvalidState(
                "ACP session belongs to a different user, project, or home".into(),
            ));
        }
        Ok(())
    }

    fn lock_controller(
        &self,
        caller: &SessionSpec,
    ) -> Result<MutexGuard<'_, ControllerState>, DaemonError> {
        self.check_scope(caller)?;
        let controller = self
            .controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if controller.owner.as_ref() != Some(&ControllerOwner::Shell(caller.session_id.clone())) {
            return Err(DaemonError::InvalidState(controller.publication.as_ref().map_or_else(
                || "ACP session is controlled by another shell; release it there before `acp attach`".into(),
                |grant| format!("ACP session is published as {}; run `acp unpublish {}` before steering or stopping it", grant.name, grant.name),
            )));
        }
        Ok(controller)
    }

    fn lock_published(
        &self,
        generation: &str,
    ) -> Result<MutexGuard<'_, ControllerState>, DaemonError> {
        if self
            .project_identity
            .is_some_and(|expected| crate::project_identity(&self.project) != Some(expected))
        {
            return Err(DaemonError::InvalidState(
                "ACP publication project was replaced".into(),
            ));
        }
        let controller = self
            .controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if controller.owner.as_ref() != Some(&ControllerOwner::Published(generation.to_owned()))
            || controller
                .publication
                .as_ref()
                .is_none_or(|grant| grant.generation != generation)
        {
            return Err(DaemonError::InvalidState(
                "ACP publication is revoked or changed".into(),
            ));
        }
        Ok(controller)
    }
}

pub(crate) struct AcpSessionManager {
    runtime: Arc<Runtime>,
    sessions: Mutex<BTreeMap<String, Arc<AcpSession>>>,
    starting: AtomicUsize,
    pending: Mutex<BTreeMap<String, PendingAcp>>,
    publications: Mutex<()>,
}

struct StartPermit<'a>(&'a AtomicUsize);

impl Drop for StartPermit<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct ReservationPermit<'a> {
    manager: &'a AcpSessionManager,
    id: String,
    completed: bool,
}

impl Drop for ReservationPermit<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.manager.fail_reservation(&self.id);
        }
    }
}

#[derive(Default)]
pub(crate) struct AcpStartupAbort {
    abandoned: AtomicBool,
    execution: Mutex<Option<ClientExecution>>,
}

impl AcpStartupAbort {
    fn bind(&self, execution: ClientExecution) {
        let abort = {
            let mut slot = self
                .execution
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.abandoned.load(Ordering::Acquire) {
                Some(execution)
            } else {
                *slot = Some(execution);
                None
            }
        };
        if let Some(execution) = abort {
            Self::terminate(execution);
        }
    }

    pub fn abandon(&self) {
        self.abandoned.store(true, Ordering::Release);
        if let Some(execution) = self
            .execution
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            Self::terminate(execution);
        }
    }

    pub fn disarm(&self) {
        self.execution
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    fn terminate(execution: ClientExecution) {
        let _ = execution.send(&AttachmentFrame::Signal {
            signal: "terminate".into(),
        });
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(5));
            let _ = execution.send(&AttachmentFrame::Signal {
                signal: "kill".into(),
            });
        });
    }
}

impl AcpSessionManager {
    pub fn new() -> Result<Self, DaemonError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .build()?;
        Ok(Self {
            runtime: Arc::new(runtime),
            sessions: Mutex::new(BTreeMap::new()),
            starting: AtomicUsize::new(0),
            pending: Mutex::new(BTreeMap::new()),
            publications: Mutex::new(()),
        })
    }

    pub fn reserve(&self, adapter: &str, caller: SessionSpec) -> Result<String, DaemonError> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.retain(|_, entry| {
            entry
                .failed_at
                .is_none_or(|failed| failed.elapsed() < Duration::from_mins(4))
        });
        for entry in pending.values_mut() {
            if !entry.claimed && entry.created.elapsed() >= UNCLAIMED_RESERVATION_TTL {
                entry.fail();
            }
        }
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.len() + sessions.len() >= MAX_SESSIONS {
            pending.retain(|_, entry| entry.failed_at.is_none());
            sessions.retain(|_, session| session.bridge.status().terminal.is_none());
        }
        if pending.len() + sessions.len() >= MAX_SESSIONS {
            return Err(DaemonError::InvalidState(
                "ACP session limit reached; stop finished sessions with `acp stop ID`".into(),
            ));
        }
        drop(sessions);
        let id = Uuid::new_v4().to_string();
        let project_identity = crate::project_identity(&caller.launch_directory);
        pending.insert(
            id.clone(),
            PendingAcp {
                adapter: adapter.into(),
                caller,
                project_identity,
                created: Instant::now(),
                claimed: false,
                failed_at: None,
                startup: None,
            },
        );
        Ok(id)
    }

    pub fn fail_reservation(&self, id: &str) {
        if let Some(entry) = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(id)
        {
            entry.fail();
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "One linear Kit attachment and ACP handshake"
    )]
    pub fn start_reserved(
        &self,
        backend: Arc<dyn DaemonBackend>,
        store: &DaemonStore,
        adapter: &str,
        caller: SessionSpec,
        startup: &Arc<AcpStartupAbort>,
        reservation_id: Option<&str>,
    ) -> Result<(String, String), DaemonError> {
        let id = if let Some(id) = reservation_id {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = pending
                .get_mut(id)
                .ok_or_else(|| DaemonError::NotFound(id.into()))?;
            if entry.claimed
                || entry.failed_at.is_some()
                || entry.created.elapsed() >= UNCLAIMED_RESERVATION_TTL
                || entry.adapter != adapter
                || entry.caller.session_id != caller.session_id
                || entry.caller.uid != caller.uid
                || entry.caller.launch_directory != caller.launch_directory
                || entry.caller.home_backing != caller.home_backing
                || entry.project_identity.is_some_and(|expected| {
                    crate::project_identity(&caller.launch_directory) != Some(expected)
                })
            {
                return Err(DaemonError::InvalidState(
                    "ACP reservation does not match this shell and agent".into(),
                ));
            }
            entry.claimed = true;
            entry.startup = Some(Arc::clone(startup));
            id.to_owned()
        } else {
            let id = self.reserve(adapter, caller.clone())?;
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let entry = pending.get_mut(&id).ok_or_else(|| {
                DaemonError::InvalidState("ACP scope stopped during startup".into())
            })?;
            entry.claimed = true;
            entry.startup = Some(Arc::clone(startup));
            id
        };
        let mut reservation = ReservationPermit {
            manager: self,
            id: id.clone(),
            completed: false,
        };
        self.starting.fetch_add(1, Ordering::AcqRel);
        let _permit = StartPermit(&self.starting);
        {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if sessions.len() + self.starting.load(Ordering::Acquire) > MAX_SESSIONS {
                // The receipt is durable. Evict only sessions whose Kit job
                // has published a terminal attachment frame.
                sessions.retain(|_, session| session.bridge.status().terminal.is_none());
            }
            if sessions.len() + self.starting.load(Ordering::Acquire) > MAX_SESSIONS {
                return Err(DaemonError::InvalidState(
                    "ACP session limit reached; stop finished sessions with `acp stop ID`".into(),
                ));
            }
        }
        let declaration = backend.resolve_acp_agent(adapter)?;
        let (server, client) = ServerAttachment::pair()?;
        startup.bind(client.clone());
        let command = declaration.command.clone();
        // The declaration's fixed tail (e.g. `--acp`) selects the Kit's ACP
        // mode; the Kit entrypoint, image, and policy stay its own.
        let arguments = declaration
            .arguments
            .iter()
            .map(|argument| argument.as_bytes().to_vec())
            .collect();
        let execution_caller = caller.clone();
        let execution_store = store.clone();
        std::thread::spawn(move || {
            let result = backend.execute(
                ExecuteSpec {
                    command,
                    arguments,
                    session: execution_caller,
                    working_directory: None,
                    placement: crate::Placement::Local,
                    environment: std::collections::BTreeMap::new(),
                    process: None,
                },
                server.clone(),
                execution_store,
            );
            if let Err(error) = result {
                let _ = server.send(&crate::AttachmentFrame::Failed {
                    message: error.to_string(),
                });
            }
        });
        let mut bridge = AcpAttachmentBridge::new(client)?;
        let io = bridge
            .take_stream()
            .ok_or_else(|| DaemonError::InvalidState("ACP attachment stream missing".into()))?;
        io.set_nonblocking(true)?;
        let _runtime_guard = self.runtime.enter();
        let io = tokio::net::UnixStream::from_std(io)?;
        let (reader, writer) = io.into_split();
        let permissions = AcpPermissionInbox::default();
        let config = AcpClientConfig {
            init_timeout: Duration::from_mins(3),
            // The Kit job owns the wall ceiling. A fixed prompt deadline
            // would discard a still-running provider turn and strand cancel.
            turn_timeout: Duration::ZERO,
            permission_timeout: Duration::from_mins(5),
            ..Default::default()
        };
        let (client, _) = match self.runtime.block_on(AcpClient::connect_registered(
            reader,
            writer,
            config,
            Some(Arc::new(permissions.clone())),
            &declaration,
            None,
        )) {
            Ok(connected) => connected,
            Err(error) => {
                let _ = bridge.terminate();
                return Err(DaemonError::InvalidState(error.to_string()));
            }
        };
        let remote_session_id = match self
            .runtime
            .block_on(client.new_session(caller.launch_directory.display().to_string(), vec![]))
        {
            Ok(id) => id,
            Err(error) => {
                let _ = bridge.terminate();
                return Err(DaemonError::InvalidState(error.to_string()));
            }
        };
        let deadline = Instant::now() + Duration::from_secs(1);
        let job_id = loop {
            if let Some(job_id) = bridge.status().job_id {
                break job_id;
            }
            if Instant::now() >= deadline {
                let _ = bridge.terminate();
                return Err(DaemonError::InvalidState(
                    "ACP Kit did not publish a job identity".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        if startup.abandoned.load(Ordering::Acquire)
            || self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&id)
                .is_none_or(|pending| pending.failed_at.is_some())
        {
            let _ = bridge.terminate();
            return Err(DaemonError::InvalidState(
                "ACP startup was cancelled before the agent became ready".into(),
            ));
        }
        let turn = Arc::new(Mutex::new(TurnLog::default()));
        permissions.set_turn_log(Arc::clone(&turn));
        let project_identity = crate::project_identity(&caller.launch_directory);
        let created = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .map_or_else(Instant::now, |entry| entry.created);
        let session = Arc::new(AcpSession {
            id: id.clone(),
            created,
            adapter: adapter.into(),
            owner_shell_session_id: caller.session_id.clone(),
            uid: caller.uid,
            project: caller.launch_directory,
            project_identity,
            home_backing: caller.home_backing,
            controller: Mutex::new(ControllerState {
                owner: Some(ControllerOwner::Shell(caller.session_id)),
                publication: None,
            }),
            remote_session_id,
            client: Arc::new(client),
            bridge,
            stopping: AtomicBool::new(false),
            run_lease_ended: AtomicBool::new(false),
            turn,
            permissions,
            store: store.clone(),
        });
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending
            .get(&id)
            .is_none_or(|entry| entry.failed_at.is_some())
        {
            let _ = session.bridge.terminate();
            return Err(DaemonError::InvalidState(
                "ACP startup was cancelled before the agent became ready".into(),
            ));
        }
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), Arc::clone(&session));
        pending.remove(&id);
        drop(pending);
        let mut termination = session.client.transport_termination();
        let weak_session = Arc::downgrade(&session);
        self.runtime.spawn(async move {
            loop {
                if !*termination.borrow_and_update() {
                    tokio::select! {
                        result = termination.changed() => { if result.is_err() { return; } },
                        () = tokio::time::sleep(Duration::from_millis(100)) => {},
                    }
                }
                if weak_session.strong_count() == 0 {
                    return;
                }
                if *termination.borrow() {
                    break;
                }
            }
            let Some(session) = weak_session.upgrade() else {
                return;
            };
            if session.stopping.load(Ordering::Acquire)
                || session.bridge.status().terminal.is_some()
            {
                return;
            }
            if AcpSessionManager::begin_stop(&session) {
                session
                    .turn
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .last_error
                    .get_or_insert_with(|| "ACP transport lost".into());
                let _ = AcpSessionManager::signal_stop(&session);
            }
        });
        reservation.completed = true;
        Ok((id, job_id))
    }

    fn get(&self, id: &str, caller: &SessionSpec) -> Result<Arc<AcpSession>, DaemonError> {
        if let Some(session) = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
        {
            session.check_scope(caller)?;
            return Ok(session);
        }
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = pending.get(id)
            && entry.caller.session_id == caller.session_id
            && entry.caller.uid == caller.uid
            && entry.caller.launch_directory == caller.launch_directory
            && entry.caller.home_backing == caller.home_backing
            && entry.project_identity.is_none_or(|expected| {
                crate::project_identity(&caller.launch_directory) == Some(expected)
            })
        {
            return Err(DaemonError::InvalidState(if entry.failed_at.is_some() {
                "ACP session failed to start; inspect the background job's error".into()
            } else {
                "ACP session is starting; run `acp list --wait` before sending a turn".into()
            }));
        }
        Err(DaemonError::NotFound(id.into()))
    }

    /// Transfer one idle session to a revocable, generation-fenced publication.
    /// The grant is daemon state, not a synthetic shell attachment.
    pub fn publish(
        &self,
        id: &str,
        caller: &SessionSpec,
        name: &str,
    ) -> Result<String, DaemonError> {
        let _names = self
            .publications
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.get(id, caller)?.lock_controller(caller).map(drop)?;
        let sessions: Vec<_> = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        if sessions.iter().any(|candidate| {
            candidate.check_scope(caller).is_ok()
                && candidate
                    .controller
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .publication
                    .as_ref()
                    .is_some_and(|grant| grant.name == name)
        }) {
            return Err(DaemonError::InvalidState(
                "ACP publication name is already in use".into(),
            ));
        }
        let session = self.get(id, caller)?;
        let mut controller = session.lock_controller(caller)?;
        if session.stopping.load(Ordering::Acquire) || session.bridge.status().terminal.is_some() {
            return Err(DaemonError::InvalidState("ACP Kit job has ended".into()));
        }
        {
            let turn = session
                .turn
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if turn.active || turn.cancel_in_flight.is_some() {
                return Err(DaemonError::InvalidState(
                    "ACP turn is active; wait for it to finish before publishing".into(),
                ));
            }
        }
        let generation = Uuid::new_v4().to_string();
        controller.publication = Some(PublicationGrant {
            name: name.into(),
            generation: generation.clone(),
            publisher_session_id: caller.session_id.clone(),
        });
        controller.owner = Some(ControllerOwner::Published(generation.clone()));
        Ok(generation)
    }

    /// Recheck the admitted generation after slow preparation. Revocation or
    /// scope shutdown during preparation must not turn into a successful grant.
    pub fn validate_publication(&self, id: &str, generation: &str) -> Result<(), DaemonError> {
        let session = self.published_session(id)?;
        let _controller = session.lock_published(generation)?;
        if session.stopping.load(Ordering::Acquire) || session.bridge.status().terminal.is_some() {
            return Err(DaemonError::InvalidState("ACP Kit job has ended".into()));
        }
        Ok(())
    }

    /// Roll back only the generation admitted by this transaction. Filesystem
    /// scope checks can now fail (e.g. project replacement during preparation),
    /// but must not prevent revoking authority that we already granted.
    pub fn rollback_publication(
        &self,
        id: &str,
        generation: &str,
        publisher: &str,
    ) -> Result<(), DaemonError> {
        let names = self
            .publications
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let session = match self.published_session(id) {
            Ok(session) => session,
            Err(DaemonError::NotFound(_)) => return Ok(()),
            Err(error) => return Err(error),
        };
        let mut controller = session
            .controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(grant) = &controller.publication else {
            return Ok(());
        };
        if grant.generation != generation {
            return Ok(()); // This transaction cannot revoke a newer grant.
        }
        if grant.publisher_session_id != publisher {
            return Err(DaemonError::InvalidState(
                "ACP rollback publisher changed".into(),
            ));
        }
        session.permissions.cancel_all();
        controller.publication = None;
        controller.owner = session
            .store
            .shell_is_attached(publisher)
            .then(|| ControllerOwner::Shell(publisher.into()));
        let should_signal = controller.owner.is_none()
            && session.run_lease_ended.load(Ordering::Acquire)
            && Self::begin_stop(&session);
        drop(controller);
        drop(names);
        if should_signal {
            Self::signal_stop(&session)?;
        }
        Ok(())
    }

    /// Revoke first, then let the host remove its MCP registration. A cached
    /// client loses authority even if the host removal fails.
    pub fn unpublish(
        &self,
        name: &str,
        caller: &SessionSpec,
        expected_generation: Option<&str>,
    ) -> Result<String, DaemonError> {
        let _names = self
            .publications
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sessions: Vec<_> = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for session in &sessions {
            if session.check_scope(caller).is_err() {
                continue;
            }
            let mut controller = session
                .controller
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(grant) = controller.publication.as_ref() else {
                continue;
            };
            if grant.name != name
                || expected_generation.is_some_and(|expected| expected != grant.generation)
            {
                continue;
            }
            if grant.publisher_session_id != caller.session_id
                && session.store.shell_is_attached(&grant.publisher_session_id)
            {
                return Err(DaemonError::InvalidState(
                    "ACP publication belongs to another attached shell".into(),
                ));
            }
            session.permissions.cancel_all();
            controller.owner = Some(ControllerOwner::Shell(caller.session_id.clone()));
            controller.publication = None;
            return Ok(session.id.clone());
        }
        Err(DaemonError::NotFound(name.into()))
    }

    pub fn prompt_with_key(
        &self,
        id: &str,
        caller: &SessionSpec,
        operation_id: &str,
        text: String,
    ) -> Result<String, DaemonError> {
        self.prompt_after_authorization(id, caller, operation_id, text, || {})
    }

    /// The hook lets a regression test try a handoff while admission is held.
    pub(crate) fn prompt_after_authorization(
        &self,
        id: &str,
        caller: &SessionSpec,
        operation_id: &str,
        text: String,
        after_authorization: impl FnOnce(),
    ) -> Result<String, DaemonError> {
        let session = self.get(id, caller)?;
        let _controller = session.lock_controller(caller)?;
        after_authorization();
        self.prompt_on_session(&session, &caller.session_id, operation_id, text)
    }

    fn prompt_on_session(
        &self,
        session: &Arc<AcpSession>,
        controller_id: &str,
        operation_id: &str,
        text: String,
    ) -> Result<String, DaemonError> {
        // Reject before admitting a turn or writing to the Kit.
        validate_prompt_frame(&session.remote_session_id, &text)?;
        if Uuid::parse_str(operation_id).map_or(true, |parsed| parsed.to_string() != operation_id) {
            return Err(DaemonError::InvalidState(
                "ACP prompt key must be a canonical UUID".into(),
            ));
        }
        let prompt_digest: [u8; 32] = Sha256::digest(text.as_bytes()).into();
        let turn_id = Uuid::new_v4().to_string();
        {
            let mut turn = session
                .turn
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(prior_turn_id) = turn.admit_prompt(
                operation_id,
                PromptAdmission {
                    controller_session_id: controller_id.into(),
                    prompt_digest,
                    turn_id: turn_id.clone(),
                },
                session.stopping.load(Ordering::Acquire)
                    || session.bridge.status().terminal.is_some(),
                session.client.dropped_updates(),
            )? {
                return Ok(prior_turn_id);
            }
        }
        // Four bounded 1 MiB values. Drain independently of the prompt result.
        let (updates, mut receiver) = mpsc::channel::<serde_json::Value>(4);
        let turn_log = Arc::clone(&session.turn);
        let collector = self.runtime.spawn_blocking(move || {
            while let Some(value) = receiver.blocking_recv() {
                let mut turn = turn_log
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                turn.record(value);
            }
        });
        let client = Arc::clone(&session.client);
        let session = Arc::clone(session);
        let remote = session.remote_session_id.clone();
        let turn_log = Arc::clone(&session.turn);
        let terminal_turn_id = turn_id.clone();
        self.runtime.spawn(async move {
            let result = client
                .prompt_raw(&remote, vec![ContentBlock::text(text)], updates)
                .await;
            let _ = collector.await;
            if matches!(&result, Err(marsh_acp::AcpError::TransportLost(_)))
                && AcpSessionManager::begin_stop(&session)
            {
                let _ = AcpSessionManager::signal_stop(&session);
            }
            let mut turn = turn_log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let dropped = client
                .dropped_updates()
                .saturating_sub(turn.dropped_updates_at_start);
            turn.complete_turn(&terminal_turn_id, result, dropped);
        });
        Ok(turn_id)
    }

    fn published_session(&self, id: &str) -> Result<Arc<AcpSession>, DaemonError> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
            .ok_or_else(|| DaemonError::NotFound(id.into()))
    }

    pub fn published_prompt(
        &self,
        id: &str,
        generation: &str,
        operation_id: &str,
        text: String,
    ) -> Result<String, DaemonError> {
        let session = self.published_session(id)?;
        // Hold the published controller while the turn is admitted.
        let _controller = session.lock_published(generation)?;
        self.prompt_on_session(
            &session,
            &format!("publication:{generation}"),
            operation_id,
            text,
        )
    }

    pub fn cancel(&self, id: &str, caller: &SessionSpec) -> Result<String, DaemonError> {
        self.cancel_after_reservation(id, caller, || {})
    }

    /// A deterministic race probe can let the wire response/collector finish
    /// between reservation and dispatch, without holding the controller lock.
    pub(crate) fn cancel_after_reservation(
        &self,
        id: &str,
        caller: &SessionSpec,
        after_reservation: impl FnOnce(),
    ) -> Result<String, DaemonError> {
        let session = self.get(id, caller)?;
        let controller = session.lock_controller(caller)?;
        Self::reserve_cancel(&session)?;
        drop(controller);
        after_reservation();
        self.cancel_on_session(&session)
    }

    pub fn published_cancel(&self, id: &str, generation: &str) -> Result<String, DaemonError> {
        let session = self.published_session(id)?;
        let controller = session.lock_published(generation)?;
        Self::reserve_cancel(&session)?;
        drop(controller);
        self.cancel_on_session(&session)
    }

    fn reserve_cancel(session: &AcpSession) -> Result<String, DaemonError> {
        let mut turn = session
            .turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !turn.active || turn.cancel_in_flight.is_some() {
            return Err(DaemonError::InvalidState(
                "No active turn, or cancellation is already dispatching; check status.".into(),
            ));
        }
        let id = turn.current_turn_id.clone().expect("active turn identity");
        turn.cancel_in_flight = Some(id.clone());
        turn.cancel_requested = true;
        Ok(id)
    }

    fn cancel_on_session(&self, session: &Arc<AcpSession>) -> Result<String, DaemonError> {
        // Fence admission, not status: an authorized cancel may complete after
        // handoff but must never target a subsequent turn.
        let result = self.runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                let result = session
                    .client
                    .cancel_prompt(&session.remote_session_id)
                    .await;
                if !matches!(result, Err(marsh_acp::AcpError::NoActivePrompt(_))) {
                    break result;
                }
                // Daemon admission precedes scheduling the client prompt.
                if !session
                    .turn
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .active
                    || tokio::time::Instant::now() >= deadline
                {
                    break result;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });
        let finished = {
            let mut turn = session
                .turn
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            turn.cancel_in_flight = None;
            !turn.active
        };
        let phase = match result {
            Ok(phase) => format!("{phase:?}"),
            Err(marsh_acp::AcpError::NoActivePrompt(_)) if finished => "AlreadyFinished".into(),
            Err(marsh_acp::AcpError::NoActivePrompt(_)) => return Err(DaemonError::InvalidState(
                "ACP prompt dispatch is not active yet; inspect status, or stop the session if it remains stalled.".into())),
            Err(error) => return Err(DaemonError::InvalidState(safe_turn_error(&error))),
        };
        Ok(phase)
    }

    pub fn respond(
        &self,
        id: &str,
        caller: &SessionSpec,
        request_id: &str,
        option_id: &str,
    ) -> Result<(), DaemonError> {
        let session = self.get(id, caller)?;
        let _controller = session.lock_controller(caller)?;
        Self::respond_on_session(&session, request_id, option_id)
    }

    pub fn published_respond(
        &self,
        id: &str,
        generation: &str,
        request_id: &str,
        option_id: &str,
    ) -> Result<(), DaemonError> {
        let session = self.published_session(id)?;
        let _controller = session.lock_published(generation)?;
        Self::respond_on_session(&session, request_id, option_id)
    }

    fn respond_on_session(
        session: &Arc<AcpSession>,
        request_id: &str,
        option_id: &str,
    ) -> Result<(), DaemonError> {
        if session.stopping.load(Ordering::Acquire) || session.bridge.status().terminal.is_some() {
            return Err(DaemonError::InvalidState("ACP Kit job has ended".into()));
        }
        if !session
            .turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active
        {
            return Err(DaemonError::InvalidState("ACP turn is not active".into()));
        }
        session.permissions.respond(request_id, option_id)
    }

    pub fn status(
        &self,
        id: &str,
        caller: &SessionSpec,
        after: u64,
        store: &DaemonStore,
    ) -> Result<AcpSessionStatus, DaemonError> {
        let session = self.get(id, caller)?;
        let controller = session
            .controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let controller_shell_session_id = match &controller.owner {
            Some(ControllerOwner::Shell(id)) => Some(id.clone()),
            _ => None,
        };
        let permissions_allowed =
            controller_shell_session_id.as_deref() == Some(caller.session_id.as_str());
        Ok(Self::status_on_session(
            &session,
            after,
            store,
            controller_shell_session_id,
            controller
                .publication
                .as_ref()
                .map(|grant| grant.name.clone()),
            permissions_allowed,
        ))
    }

    pub fn published_status(
        &self,
        id: &str,
        generation: &str,
        after: u64,
        store: &DaemonStore,
    ) -> Result<AcpSessionStatus, DaemonError> {
        let session = self.published_session(id)?;
        let controller = session.lock_published(generation)?;
        Ok(Self::status_on_session(
            &session,
            after,
            store,
            None,
            controller
                .publication
                .as_ref()
                .map(|grant| grant.name.clone()),
            true,
        ))
    }

    fn status_on_session(
        session: &Arc<AcpSession>,
        after: u64,
        store: &DaemonStore,
        controller_shell_session_id: Option<String>,
        published_name: Option<String>,
        permissions_allowed: bool,
    ) -> AcpSessionStatus {
        let attachment = session.bridge.status();
        let receipt = attachment
            .job_id
            .as_ref()
            .and_then(|job_id| store.job(job_id).ok());
        let turn = session
            .turn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (updates, next_cursor, more_updates) = turn.page(after);
        let permissions = if permissions_allowed {
            session.permissions.pending()
        } else {
            Vec::new()
        };
        AcpSessionStatus {
            agent_session_id: session.id.clone(),
            adapter: session.adapter.clone(),
            controller_shell_session_id,
            published_name,
            turn_active: turn.active,
            current_turn_id: turn.current_turn_id.clone().filter(|_| turn.active),
            last_turn_id: turn.last_turn_id.clone(),
            turn_start_cursor: turn.turn_start_cursor,
            turns: turn.turns.clone(),
            dropped_updates: session
                .client
                .dropped_updates()
                .saturating_sub(turn.dropped_updates_at_start),
            out_of_turn_updates: session.client.out_of_turn_updates(),
            permission_note: turn.permission_note.clone(),
            cancel_requested: turn.cancel_requested,
            stopping: session.stopping.load(Ordering::Acquire),
            last_stop_reason: turn.last_stop_reason,
            last_error: turn.last_error.clone(),
            updates,
            next_cursor,
            latest_cursor: turn.next_cursor.saturating_sub(1),
            retained_after: turn.dropped_through,
            more_updates,
            updates_lost: turn.had_transport_loss
                || session.client.dropped_updates() > turn.dropped_updates_at_start
                || after < turn.dropped_through,
            permissions,
            attachment,
            receipt,
            status_omissions: Vec::new(),
        }
        .fit_wire_budget(after)
    }

    pub fn list(&self, caller: &SessionSpec) -> Vec<AcpSessionSummary> {
        let sessions: Vec<_> = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        let mut result: Vec<_> = sessions
            .into_iter()
            .filter(|session| session.check_scope(caller).is_ok())
            .map(|session| {
                let attachment = session.bridge.status();
                // Release each guard before taking the next: status reads the
                // controller and turn locks in the opposite order.
                let turn_active = {
                    session
                        .turn
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .active
                };
                let published_name = {
                    session
                        .controller
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .publication
                        .as_ref()
                        .map(|grant| grant.name.clone())
                };
                (
                    session.created,
                    AcpSessionSummary {
                        agent_session_id: session.id.clone(),
                        adapter: session.adapter.clone(),
                        job_id: attachment.job_id,
                        owner_shell_session_id: session.owner_shell_session_id.clone(),
                        turn_active,
                        terminal: attachment.terminal.is_some(),
                        published_name,
                        state: (attachment.terminal.is_none()
                            && session.stopping.load(Ordering::Acquire))
                        .then(|| "stopping".into()),
                    },
                )
            })
            .collect();
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.retain(|_, entry| {
            entry
                .failed_at
                .is_none_or(|failed| failed.elapsed() < Duration::from_mins(4))
        });
        for entry in pending.values_mut() {
            if !entry.claimed && entry.created.elapsed() >= UNCLAIMED_RESERVATION_TTL {
                entry.fail();
            }
        }
        result.extend(
            pending
                .iter()
                .filter(|(_, entry)| {
                    entry.caller.uid == caller.uid
                        && entry.caller.launch_directory == caller.launch_directory
                        && entry.caller.home_backing == caller.home_backing
                        && entry.project_identity.is_none_or(|expected| {
                            crate::project_identity(&caller.launch_directory) == Some(expected)
                        })
                })
                .map(|(id, entry)| {
                    (
                        entry.created,
                        AcpSessionSummary {
                            agent_session_id: id.clone(),
                            adapter: entry.adapter.clone(),
                            job_id: None,
                            owner_shell_session_id: entry.caller.session_id.clone(),
                            turn_active: false,
                            terminal: entry.failed_at.is_some(),
                            published_name: None,
                            state: Some(
                                if entry.failed_at.is_some() {
                                    "failed"
                                } else {
                                    "starting"
                                }
                                .into(),
                            ),
                        },
                    )
                }),
        );
        result.sort_by_key(|(created, _)| std::cmp::Reverse(*created));
        result.into_iter().map(|(_, summary)| summary).collect()
    }

    pub fn attach(&self, id: &str, caller: &SessionSpec) -> Result<(), DaemonError> {
        let session = self.get(id, caller)?;
        let mut controller = session
            .controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if session.stopping.load(Ordering::Acquire) && session.bridge.status().terminal.is_none() {
            return Err(DaemonError::InvalidState("ACP Kit job is stopping".into()));
        }
        if controller
            .owner
            .as_ref()
            .is_some_and(|owner| owner != &ControllerOwner::Shell(caller.session_id.clone()))
        {
            return Err(DaemonError::InvalidState(
                "ACP session already has a controller".into(),
            ));
        }
        controller.owner = Some(ControllerOwner::Shell(caller.session_id.clone()));
        Ok(())
    }

    pub fn stop(&self, id: &str, caller: &SessionSpec) -> Result<(), DaemonError> {
        let pending_startup = {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entry) = pending.get_mut(id)
                && entry.caller.session_id == caller.session_id
                && entry.caller.uid == caller.uid
                && entry.caller.launch_directory == caller.launch_directory
                && entry.caller.home_backing == caller.home_backing
                && entry.project_identity.is_none_or(|expected| {
                    crate::project_identity(&caller.launch_directory) == Some(expected)
                })
            {
                entry.fail();
                Some(entry.startup.clone())
            } else {
                None
            }
        };
        if let Some(startup) = pending_startup {
            if let Some(startup) = startup {
                startup.abandon();
            }
            return Ok(());
        }
        let session = self.get(id, caller)?;
        if session.bridge.status().terminal.is_some() {
            return Ok(());
        }
        let controller = session.lock_controller(caller)?;
        let should_signal = Self::begin_stop(&session);
        drop(controller);
        if should_signal {
            Self::signal_stop(&session)
        } else {
            Ok(())
        }
    }

    fn begin_stop(session: &Arc<AcpSession>) -> bool {
        session.permissions.cancel_all();
        if session.stopping.swap(true, Ordering::AcqRel) {
            return false;
        }
        session.bridge.status().terminal.is_none()
    }

    fn signal_stop(session: &Arc<AcpSession>) -> Result<(), DaemonError> {
        let terminate = session.bridge.terminate();
        let session = Arc::clone(session);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(5));
            if session.bridge.status().terminal.is_none() {
                let _ = session.bridge.kill();
            }
        });
        terminate
    }

    /// The run command's dedicated connection is its process lease. Its loss
    /// releases this shell's controller. The Kit stops once no controller
    /// remains, including after a later handoff controller detaches.
    pub fn run_disconnected(&self, id: &str, shell_session_id: &str, store: &DaemonStore) {
        let session = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned();
        let Some(session) = session else {
            return;
        };
        session.run_lease_ended.store(true, Ordering::Release);
        let finished = self.run_finished(id, store);
        let mut controller = session
            .controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let should_signal = if controller.owner.as_ref()
            == Some(&ControllerOwner::Shell(shell_session_id.into()))
            || controller.owner.is_none()
        {
            let should_signal = !finished && Self::begin_stop(&session);
            controller.owner = None;
            should_signal
        } else {
            false
        };
        drop(controller);
        if should_signal {
            let _ = Self::signal_stop(&session);
        }
    }

    pub fn run_finished(&self, id: &str, store: &DaemonStore) -> bool {
        let session = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned();
        let Some(session) = session else {
            return false;
        };
        let Some(job_id) = session.bridge.status().job_id else {
            return false;
        };
        let Ok(receipt) = store.job(&job_id) else {
            return false;
        };
        !matches!(receipt.state, JobState::Queued | JobState::Running)
    }

    pub fn release(&self, id: &str, caller: &SessionSpec) -> Result<(), DaemonError> {
        self.release_after_authorization(id, caller, || {})
    }

    /// The hook lets a regression test try a handoff before release commits.
    pub(crate) fn release_after_authorization(
        &self,
        id: &str,
        caller: &SessionSpec,
        after_authorization: impl FnOnce(),
    ) -> Result<(), DaemonError> {
        let session = self.get(id, caller)?;
        let mut controller = session.lock_controller(caller)?;
        after_authorization();
        session.permissions.cancel_all();
        controller.owner = None;
        let should_signal =
            session.run_lease_ended.load(Ordering::Acquire) && Self::begin_stop(&session);
        drop(controller);
        if should_signal {
            Self::signal_stop(&session)?;
        }
        Ok(())
    }

    pub fn detach_shell(&self, shell_session_id: &str) {
        for pending in self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values_mut()
        {
            if pending.caller.session_id == shell_session_id {
                pending.fail();
            }
        }
        let sessions: Vec<_> = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for session in sessions {
            let mut controller = session
                .controller
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if controller.owner.as_ref() == Some(&ControllerOwner::Shell(shell_session_id.into())) {
                controller.owner = None;
                session.permissions.cancel_all();
                let should_signal =
                    session.run_lease_ended.load(Ordering::Acquire) && Self::begin_stop(&session);
                drop(controller);
                if should_signal {
                    let _ = Self::signal_stop(&session);
                }
            }
        }
    }

    /// Scope teardown revokes external authority before stopping its Kit jobs.
    /// A still-running exporter can retain its declaration but cannot use it.
    pub fn revoke_publications_for_scope_stop(&self) {
        let _names = self
            .publications
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        let sessions: Vec<_> = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for session in sessions {
            let mut controller = session
                .controller
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if controller.publication.take().is_none() {
                continue;
            }
            controller.owner = None;
            let should_signal = Self::begin_stop(&session);
            drop(controller);
            if should_signal {
                let _ = Self::signal_stop(&session);
            }
        }
    }
}

/// Whether a prompt was rejected before admission or its reply was lost after dispatch.
#[derive(Debug)]
pub enum PromptAdmissionError {
    Rejected(DaemonError),
    Uncertain(DaemonError),
}

impl PromptAdmissionError {
    #[must_use]
    pub fn into_error(self) -> DaemonError {
        match self {
            Self::Rejected(error) | Self::Uncertain(error) => error,
        }
    }
}

impl crate::Client {
    /// Submit once. Only failure after the complete request frame was written
    /// can need deduplicated recovery; connect/encode/write failures did not
    /// deliver a complete frame and cannot have admitted a turn.
    pub fn acp_prompt_classified(
        &self,
        agent_session_id: String,
        session: SessionSpec,
        operation_id: String,
        text: String,
    ) -> Result<String, PromptAdmissionError> {
        use PromptAdmissionError::{Rejected, Uncertain};
        use std::io::Write as _;
        let bytes = self
            .encode_acp_prompt(agent_session_id, session, operation_id, text)
            .map_err(Rejected)?;
        let mut stream = std::os::unix::net::UnixStream::connect(&self.paths.socket)
            .map_err(|error| Rejected(error.into()))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .and_then(|()| stream.set_write_timeout(Some(Duration::from_secs(5))))
            .map_err(|error| Rejected(error.into()))?;
        // The bounded length was checked before any IO. A partial frame is not
        // a request; the daemon cannot dispatch it on EOF.
        let length = u32::try_from(bytes.len())
            .map_err(|_| Rejected(DaemonError::FrameTooLarge(bytes.len())))?;
        stream
            .write_all(&length.to_be_bytes())
            .and_then(|()| stream.write_all(&bytes))
            .map_err(|error| Rejected(error.into()))?;
        match crate::read_frame(&mut stream).map_err(Uncertain)? {
            crate::PublicReply::AcpPromptAccepted { turn_id } => Ok(turn_id),
            reply @ crate::PublicReply::Error { .. } => {
                Err(Rejected(crate::unexpected_reply(reply)))
            }
            reply => Err(Uncertain(crate::unexpected_reply(reply))),
        }
    }

    /// Validate the exact authenticated control envelope without connecting or
    /// dispatching, distinguishing deterministic size errors from lost replies.
    pub fn acp_prompt_preflight(
        &self,
        agent_session_id: &str,
        session: &SessionSpec,
        operation_id: &str,
        text: &str,
    ) -> Result<(), DaemonError> {
        self.encode_acp_prompt(
            agent_session_id.into(),
            session.clone(),
            operation_id.into(),
            text.into(),
        )
        .map(|_| ())
    }

    fn encode_acp_prompt(
        &self,
        agent_session_id: String,
        session: SessionSpec,
        operation_id: String,
        text: String,
    ) -> Result<Vec<u8>, DaemonError> {
        let envelope = crate::Envelope {
            protocol: crate::PROTOCOL.into(),
            token: self.token.clone(),
            body: crate::PublicRequest::AcpPrompt {
                agent_session_id,
                session,
                operation_id,
                text,
            },
        };
        let bytes = serde_json::to_vec(&envelope)?;
        if bytes.len() > crate::MAX_FRAME_BYTES {
            return Err(DaemonError::FrameTooLarge(bytes.len()));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_key_ledger_keeps_oldest_retry_and_fails_closed_at_capacity() {
        let mut log = TurnLog::default();
        let admission = |turn_id: String| PromptAdmission {
            controller_session_id: "shell-a".into(),
            prompt_digest: [7; 32],
            turn_id,
        };
        for number in 0..MAX_PROMPT_ADMISSIONS {
            let key = format!("key-{number}");
            assert_eq!(
                log.admit_prompt(&key, admission(key.clone()), false, 0)
                    .unwrap(),
                None
            );
            log.active = false;
        }
        assert!(matches!(
            log.admit_prompt("new-key", admission("new-turn".into()), false, 0),
            Err(DaemonError::InvalidState(message)) if message.contains("start a new session")
        ));
        assert_eq!(
            log.admit_prompt("key-0", admission("ignored".into()), false, 0)
                .unwrap(),
            Some("key-0".into())
        );
        assert_eq!(log.admissions.len(), MAX_PROMPT_ADMISSIONS);
    }

    #[test]
    fn unanswered_permission_times_out_to_deny_and_clears_request() {
        let inbox = AcpPermissionInbox {
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            timeout: Duration::from_millis(5),
            turn: Arc::new(Mutex::new(None)),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let outcome = runtime.block_on(inbox.handle_permission(RequestPermissionRequest {
            session_id: "remote-session".into(),
            tool_call: marsh_acp::ToolCallUpdate {
                tool_call_id: "tool-1".into(),
                ..Default::default()
            },
            options: vec![PermissionOption {
                option_id: "once".into(),
                name: "Allow once".into(),
                kind: "allow_once".into(),
            }],
        }));
        assert_eq!(outcome, RequestPermissionOutcome::Cancelled);
        assert!(inbox.pending().is_empty());
    }

    #[test]
    fn aborted_permission_handler_does_not_leave_stale_approval() {
        let inbox = AcpPermissionInbox::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let handler = inbox.clone();
            let task = tokio::spawn(async move {
                handler
                    .handle_permission(RequestPermissionRequest {
                        session_id: "remote-session".into(),
                        tool_call: marsh_acp::ToolCallUpdate {
                            tool_call_id: "tool-1".into(),
                            ..Default::default()
                        },
                        options: vec![PermissionOption {
                            option_id: "once".into(),
                            name: "Allow once".into(),
                            kind: "allow_once".into(),
                        }],
                    })
                    .await
            });
            while inbox.pending().is_empty() {
                tokio::task::yield_now().await;
            }
            task.abort();
            let _ = task.await;
            assert!(inbox.pending().is_empty());
        });
    }

    #[test]
    fn status_pages_advance_only_through_returned_updates() {
        let mut log = TurnLog::default();
        for number in 0..100 {
            log.record(serde_json::json!({ "number": number }));
        }
        let mut after = 0;
        let mut seen = Vec::new();
        loop {
            let (page, next_cursor, more) = log.page(after);
            assert!(!page.is_empty());
            assert!(page.len() <= MAX_STATUS_PAGE_UPDATES);
            assert_eq!(next_cursor, page.last().unwrap().cursor);
            seen.extend(page.iter().map(|update| update.cursor));
            after = next_cursor;
            if !more {
                break;
            }
        }
        assert_eq!(seen, (1..=100).collect::<Vec<_>>());
    }

    #[test]
    fn status_page_is_bounded_and_reports_dropped_oversize_update() {
        let mut log = TurnLog::default();
        log.record(serde_json::json!({ "text": "x".repeat(MAX_STATUS_PAGE_BYTES + 1) }));
        assert_eq!(log.dropped_through, 1);
        for _ in 0..7 {
            log.record(serde_json::json!({ "text": "x".repeat(60 * 1024) }));
        }
        let (page, next_cursor, more) = log.page(0);
        assert!(log.dropped_through > 0);
        assert!(
            page.iter()
                .map(|item| serde_json::to_vec(item).unwrap().len())
                .sum::<usize>()
                < 300 * 1024
        );
        assert!(more);
        assert_eq!(next_cursor, page.last().unwrap().cursor);
        let (second, _, more) = log.page(next_cursor);
        assert!(!second.is_empty());
        assert!(!more);
    }

    fn oversized_status_fixture() -> AcpSessionStatus {
        let error = safe_turn_error(&marsh_acp::AcpError::JsonRpc {
            code: -32000,
            message: String::new(),
            data: None,
        });
        let note = "Permission cancelled: agent offered only persistent choices; ask it to offer allow_once or reject_once.";
        let turns: BTreeMap<_, _> = (0..MAX_PROMPT_ADMISSIONS)
            .map(|_| {
                (
                    Uuid::new_v4().to_string(),
                    AcpTurnReceipt {
                        start_cursor: u64::MAX - 64,
                        end_cursor: Some(u64::MAX),
                        stop_reason: Some(StopReason::EndTurn),
                        error: Some(error.clone()),
                        permission_note: Some(note.into()),
                        dropped_updates: u64::MAX,
                        retained_after: u64::MAX,
                    },
                )
            })
            .collect();
        let permissions: Vec<_> = (0..16)
            .map(|_| AcpPendingPermission {
                request_id: Uuid::new_v4().to_string(),
                tool_call_id: "t".repeat(128),
                title: Some("🦀".repeat(256)),
                input_preview: Some("🦀".repeat(4096)),
                options: (0..16)
                    .map(|n| PermissionOption {
                        option_id: format!("{n:02}{}", "o".repeat(126)),
                        name: "n".repeat(128),
                        kind: "allow_once".into(),
                    })
                    .collect(),
            })
            .collect();
        AcpSessionStatus {
            agent_session_id: Uuid::new_v4().to_string(),
            adapter: "a".repeat(128),
            controller_shell_session_id: Some(Uuid::new_v4().to_string()),
            published_name: None,
            turn_active: true,
            current_turn_id: Some(Uuid::new_v4().to_string()),
            last_turn_id: None,
            turn_start_cursor: 0,
            turns,
            dropped_updates: 0,
            out_of_turn_updates: 0,
            permission_note: None,
            cancel_requested: false,
            stopping: false,
            last_stop_reason: None,
            last_error: None,
            updates: (1..=32)
                .map(|cursor| AcpUpdate {
                    cursor,
                    turn_id: Some(Uuid::new_v4().to_string()),
                    update: serde_json::json!({"text":"x".repeat(8000)}),
                })
                .collect(),
            next_cursor: 32,
            latest_cursor: 32,
            retained_after: 0,
            more_updates: false,
            updates_lost: false,
            permissions,
            attachment: AcpAttachmentStatus {
                job_id: Some(Uuid::new_v4().to_string()),
                stderr: vec![255; 64 * 1024],
                terminal: None,
            },
            receipt: None,
            status_omissions: Vec::new(),
        }
    }

    #[test]
    fn maximum_receipts_permissions_and_diagnostics_fit_the_public_status_frame() {
        let status = oversized_status_fixture();
        assert!(
            status.exceeds_wire_budget(),
            "fixture must exercise the real aggregate overflow"
        );
        let expected_permissions: Vec<_> = status
            .permissions
            .iter()
            .map(|p| (&p.request_id, &p.options))
            .collect();
        let expected_choices = serde_json::to_value(expected_permissions).unwrap();
        let expected_turns = status.turns.clone();
        let bounded = status.fit_wire_budget(0);
        assert!(!bounded.exceeds_wire_budget());
        assert_eq!(
            bounded.turns, expected_turns,
            "old receipt availability was weakened"
        );
        assert_eq!(
            serde_json::to_value(
                bounded
                    .permissions
                    .iter()
                    .map(|p| (&p.request_id, &p.options))
                    .collect::<Vec<_>>()
            )
            .unwrap(),
            expected_choices
        );
        assert!(!bounded.status_omissions.is_empty());
        assert!(
            !bounded.updates_lost,
            "presentation trimming is not capture loss"
        );
        let (mut sender, mut receiver) = std::os::unix::net::UnixStream::pair().unwrap();
        sender
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        receiver
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let send = std::thread::spawn(move || {
            crate::write_frame(
                &mut sender,
                &crate::PublicReply::AcpStatus(Box::new(bounded)),
            )
        });
        let crate::PublicReply::AcpStatus(decoded) = crate::read_frame(&mut receiver).unwrap()
        else {
            panic!("wrong reply")
        };
        send.join().unwrap().unwrap();
        assert_eq!(decoded.turns.len(), MAX_PROMPT_ADMISSIONS);
        assert_eq!(decoded.permissions.len(), 16);
        assert_eq!(
            decoded.next_cursor,
            decoded.updates.last().map_or(0, |u| u.cursor)
        );
    }

    #[test]
    fn prompt_limit_counts_encoded_json_before_turn_admission() {
        assert!(
            validate_prompt_frame("remote", &"x".repeat(DEFAULT_MAX_FRAME_BYTES - 256)).is_ok()
        );
        let escaped = "\\".repeat(600_000);
        assert!(escaped.len() < DEFAULT_MAX_FRAME_BYTES);
        assert!(
            validate_prompt_frame("remote", &escaped)
                .unwrap_err()
                .to_string()
                .contains("encoded ACP prompt")
        );
    }
}
