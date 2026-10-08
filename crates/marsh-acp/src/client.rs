//! Real stdio JSON-RPC ACP v1 client and session controller.
//!
//! Enforces:
//! - Initialization and protocol handshake (ACP v1).
//! - `session/new` creation.
//! - Sequential prompt execution (PRD: one active turn, concurrent prompts return Busy).
//! - Distinct cancellation phases: Requested -> Dispatched -> Confirmed.
//! - Strict frame bounds and configurable timeouts.
//! - Capability-gated `session/load` and `session/resume`.
//! - Explicit rejection of host execution requests from agent (`terminal/*`, `fs/*`).
//! - Permission request handling defaulting to deny.

use crate::adapter::{AgentAdapterDeclaration, AgentProtocol};
use crate::cancel::{CancelPhase, CancelTracker};
use crate::error::AcpError;
use crate::protocol::{
    ACP_V1_PROTOCOL_VERSION, AgentCapabilities, CancelNotification, ClientCapabilities,
    ContentBlock, ImplementationInfo, InitializeRequest, InitializeResponse, JsonRpcError,
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    RequestId, RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    ResumeSessionRequest, ResumeSessionResponse, SessionUpdate, StopReason,
};
use crate::transport::{MessageReader, MessageWriter};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex as TokioMutex, mpsc, oneshot, watch};
use tokio::time::{Instant, timeout, timeout_at};
use tokio_util::sync::CancellationToken;

/// Configuration options for the ACP v1 client.
#[derive(Clone, Debug)]
pub struct AcpClientConfig {
    /// Maximum allowed bytes per incoming JSON-RPC frame (default: 1 MiB).
    pub max_frame_bytes: usize,
    /// Timeout for `initialize` handshake (default: 10s).
    pub init_timeout: Duration,
    /// Timeout for a prompt turn (default: 60s). Zero waits until a response
    /// or transport loss, for daemon-owned Kit jobs with a separate wall limit.
    pub turn_timeout: Duration,
    /// Deadline for cancellation queue admission and wire dispatch (default: 10s).
    /// Also bounds an individual outbound queue/write wait. Confirmation still
    /// requires an agent response and is never inferred from this deadline.
    pub cancel_timeout: Duration,
    /// Timeout for agent permission request evaluation (default: 10s).
    pub permission_timeout: Duration,
}

impl Default for AcpClientConfig {
    fn default() -> Self {
        Self {
            max_frame_bytes: 1024 * 1024,
            init_timeout: Duration::from_secs(10),
            turn_timeout: Duration::from_mins(1),
            cancel_timeout: Duration::from_secs(10),
            permission_timeout: Duration::from_secs(10),
        }
    }
}

/// Handler for advisory agent permission requests (`session/request_permission`).
///
/// In accordance with ACP-07, requests default to deny unless explicitly approved
/// within a bounded timeout by the controlling session.
pub trait PermissionHandler: Send + Sync {
    fn handle_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> Pin<Box<dyn Future<Output = RequestPermissionOutcome> + Send>>;
}

/// Default permission handler that immediately denies all requests.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultDenyPermissionHandler;

impl PermissionHandler for DefaultDenyPermissionHandler {
    fn handle_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> Pin<Box<dyn Future<Output = RequestPermissionOutcome> + Send>> {
        Box::pin(async move {
            // Find an explicit reject option if provided, else cancel.
            if let Some(opt) = request.options.iter().find(|o| o.kind.contains("reject")) {
                RequestPermissionOutcome::select(opt.option_id.clone())
            } else {
                RequestPermissionOutcome::cancel()
            }
        })
    }
}

/// Item queued for outbound writing.
struct OutboundItem {
    message: JsonRpcMessage,
    on_dispatched: Option<oneshot::Sender<()>>,
    cancel_tracker: Option<Arc<CancelTracker>>,
}

/// Update delivery is independent of control dispatch. Each raw item is bounded
/// by the incoming frame limit; the daemon supplies a four-item (4 MiB) queue.
#[derive(Clone)]
enum UpdateSink {
    Typed(mpsc::Sender<SessionUpdate>),
    Raw(mpsc::Sender<serde_json::Value>),
}

impl UpdateSink {
    async fn send(&self, value: serde_json::Value) -> Result<(), ()> {
        match self {
            Self::Raw(sink) => sink.send(value).await.map_err(|_| ()),
            Self::Typed(sink) => {
                let update = serde_json::from_value(value).map_err(|_| ())?;
                sink.send(update).await.map_err(|_| ())
            }
        }
    }
}

/// Active turn controller state.
enum TurnState {
    Active {
        turn_id: u64,
        session_id: String,
        prompt_request_id: RequestId,
        cancel_tracker: Arc<CancelTracker>,
        update_sink: UpdateSink,
        update_cancelled: CancellationToken,
        delivery_abandoned: Arc<AtomicBool>,
        pending_permissions: Arc<TokioMutex<BTreeMap<RequestId, tokio::task::AbortHandle>>>,
    },
    Cancelling {
        turn_id: u64,
        session_id: String,
        prompt_request_id: RequestId,
        cancel_tracker: Arc<CancelTracker>,
        pending_permissions: Arc<TokioMutex<BTreeMap<RequestId, tokio::task::AbortHandle>>>,
    },
}

impl TurnState {
    fn turn_id(&self) -> u64 {
        match self {
            Self::Active { turn_id, .. } | Self::Cancelling { turn_id, .. } => *turn_id,
        }
    }

    fn session_id(&self) -> &str {
        match self {
            Self::Active { session_id, .. } | Self::Cancelling { session_id, .. } => session_id,
        }
    }

    fn cancel_tracker(&self) -> Arc<CancelTracker> {
        match self {
            Self::Active { cancel_tracker, .. } | Self::Cancelling { cancel_tracker, .. } => {
                Arc::clone(cancel_tracker)
            }
        }
    }

    #[must_use]
    pub fn prompt_request_id(&self) -> &RequestId {
        match self {
            Self::Active {
                prompt_request_id, ..
            }
            | Self::Cancelling {
                prompt_request_id, ..
            } => prompt_request_id,
        }
    }

    fn pending_permissions(
        &self,
    ) -> Arc<TokioMutex<BTreeMap<RequestId, tokio::task::AbortHandle>>> {
        match self {
            Self::Active {
                pending_permissions,
                ..
            }
            | Self::Cancelling {
                pending_permissions,
                ..
            } => Arc::clone(pending_permissions),
        }
    }
}

/// Helper to abort pending permission handlers and send `cancelled` response over the wire.
async fn abort_and_cancel_pending_permissions(
    pending_perms: &TokioMutex<BTreeMap<RequestId, tokio::task::AbortHandle>>,
    shared: &Arc<ClientShared>,
) {
    let perms = {
        let mut guard = pending_perms.lock().await;
        std::mem::take(&mut *guard)
    };

    // Abort every handler before any potentially blocked reply. If transport
    // dispatch times out, no remaining permission task may outlive the drain.
    for abort_handle in perms.values() {
        abort_handle.abort();
    }
    for (req_id, _) in perms {
        let resp_payload = RequestPermissionResponse {
            outcome: RequestPermissionOutcome::Cancelled,
        };
        if let Ok(val) = serde_json::to_value(resp_payload) {
            let response = JsonRpcResponse::ok(req_id, val);
            let _ = shared
                .send_outbound(JsonRpcMessage::Response(response), None)
                .await;
        }
    }
}

/// Returns true if a response indicates verified cancellation ([`StopReason::Cancelled`]).
fn is_stop_reason_cancelled(resp: &JsonRpcResponse) -> bool {
    if let Some(val) = &resp.result
        && let Ok(prompt_resp) = serde_json::from_value::<PromptResponse>(val.clone())
    {
        return prompt_resp.stop_reason == StopReason::Cancelled;
    }
    false
}

/// RAII Turn Guard to ensure prompt state cleanup and cancellation on caller drop or timeouts.
struct TurnGuard {
    shared: Arc<ClientShared>,
    turn_id: u64,
    session_id: String,
    prompt_request_id: RequestId,
    pending_permissions: Arc<TokioMutex<BTreeMap<RequestId, tokio::task::AbortHandle>>>,
    completed: bool,
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if self.completed {
            return;
        }

        let shared = Arc::clone(&self.shared);
        let turn_id = self.turn_id;
        let session_id = self.session_id.clone();
        let prompt_request_id = self.prompt_request_id.clone();
        let pending_permissions = Arc::clone(&self.pending_permissions);

        tokio::spawn(async move {
            let cancel_tracker_opt = {
                let mut guard = shared.active_turn.lock().await;
                match &*guard {
                    Some(TurnState::Active {
                        turn_id: tid,
                        cancel_tracker,
                        pending_permissions: perms,
                        ..
                    }) if *tid == turn_id => {
                        let tracker = Arc::clone(cancel_tracker);
                        let perms_clone = Arc::clone(perms);
                        *guard = Some(TurnState::Cancelling {
                            turn_id,
                            session_id: session_id.clone(),
                            prompt_request_id: prompt_request_id.clone(),
                            cancel_tracker: Arc::clone(&tracker),
                            pending_permissions: perms_clone,
                        });
                        Some(tracker)
                    }
                    _ => None,
                }
            };

            if let Some(cancel_tracker) = cancel_tracker_opt {
                let _ = shared
                    .dispatch_cancel(&session_id, &cancel_tracker, &pending_permissions)
                    .await;
            }

            // Keep the turn fenced until the agent answers. A timer cannot prove
            // that the agent stopped, and admitting another turn could overlap it.
        });
    }
}

/// Shared internal client state.
struct ClientShared {
    next_request_id: AtomicU64,
    next_turn_id: AtomicU64,
    outbound_tx: mpsc::Sender<OutboundItem>,
    pending_requests: TokioMutex<BTreeMap<RequestId, oneshot::Sender<JsonRpcResponse>>>,
    active_turn: TokioMutex<Option<TurnState>>,
    session_cancel_trackers: TokioMutex<BTreeMap<String, Arc<CancelTracker>>>,
    permission_handler: Arc<dyn PermissionHandler>,
    permission_timeout: Duration,
    transport_timeout: Duration,
    agent_capabilities: TokioMutex<Option<AgentCapabilities>>,
    is_terminated: AtomicBool,
    termination: watch::Sender<bool>,
    closed: CancellationToken,
    dropped_updates: AtomicU64,
    out_of_turn_updates: AtomicU64,
}

impl ClientShared {
    async fn terminate_transport(&self) {
        // Registration and termination share this lock. A request can either
        // register before the drain or observe termination, never miss both.
        let mut pending = self.pending_requests.lock().await;
        self.is_terminated.store(true, Ordering::Release);
        pending.clear();
        self.termination.send_replace(true);
        self.closed.cancel();
    }

    async fn dispatch_cancel(
        self: &Arc<Self>,
        session_id: &str,
        tracker: &Arc<CancelTracker>,
        permissions: &TokioMutex<BTreeMap<RequestId, tokio::task::AbortHandle>>,
    ) -> Result<CancelPhase, AcpError> {
        tracker.mark_requested();
        // Control dispatch never depends on update consumption. Mark cancel so
        // a genuinely stalled consumer can be abandoned at its bounded delivery
        // deadline, without making healthy post-cancel output lossy.
        if let Some(TurnState::Active {
            update_cancelled, ..
        }) = &*self.active_turn.lock().await
        {
            update_cancelled.cancel();
        }
        let dispatch = async {
            // One deadline includes permission replies, queue admission, and
            // the writer's flush acknowledgement, including on caller drop.
            abort_and_cancel_pending_permissions(permissions, self).await;
            let params = serde_json::to_value(CancelNotification {
                session_id: session_id.into(),
            })?;
            let (tx, rx) = oneshot::channel();
            self.send_outbound_with_cancel_tracker(
                JsonRpcMessage::Notification(JsonRpcNotification::new(
                    "session/cancel",
                    Some(params),
                )),
                Some(tx),
                Some(Arc::clone(tracker)),
            )
            .await?;
            rx.await
                .map_err(|_| AcpError::TransportLost("cancellation writer stopped".into()))?;
            tracker.mark_dispatched();
            Ok(tracker.phase())
        };
        match timeout(self.transport_timeout, dispatch).await {
            Ok(Ok(phase)) => Ok(phase),
            result => {
                self.terminate_transport().await;
                match result {
                    Ok(Err(error)) => Err(error),
                    _ => Err(AcpError::TransportLost(
                        "cancellation could not be dispatched before its deadline".into(),
                    )),
                }
            }
        }
    }

    #[allow(clippy::cast_possible_wrap)]
    fn next_id(&self) -> RequestId {
        let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        RequestId::Number(id as i64)
    }

    fn next_turn_id(&self) -> u64 {
        self.next_turn_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn send_outbound(
        &self,
        message: JsonRpcMessage,
        on_dispatched: Option<oneshot::Sender<()>>,
    ) -> Result<(), AcpError> {
        self.send_outbound_with_cancel_tracker(message, on_dispatched, None)
            .await
    }

    async fn send_outbound_with_cancel_tracker(
        &self,
        message: JsonRpcMessage,
        on_dispatched: Option<oneshot::Sender<()>>,
        cancel_tracker: Option<Arc<CancelTracker>>,
    ) -> Result<(), AcpError> {
        if self.is_terminated.load(Ordering::Acquire) {
            return Err(AcpError::TransportLost("transport is terminated".into()));
        }
        let send = self.outbound_tx.send(OutboundItem {
            message,
            on_dispatched,
            cancel_tracker,
        });
        if let Ok(Ok(())) = timeout(self.transport_timeout, send).await {
            Ok(())
        } else {
            self.terminate_transport().await;
            Err(AcpError::TransportLost(
                "outbound queue closed or stalled".into(),
            ))
        }
    }

    async fn send_request_with_id(
        &self,
        id: RequestId,
        method: impl Into<String>,
        params: Option<serde_json::Value>,
        timeout_duration: Duration,
        op_name: &'static str,
    ) -> Result<JsonRpcResponse, AcpError> {
        let deadline = Instant::now() + timeout_duration;
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending_requests.lock().await;
            if self.is_terminated.load(Ordering::Acquire) {
                return Err(AcpError::TransportLost("transport is terminated".into()));
            }
            pending.insert(id.clone(), tx);
        }

        let req = JsonRpcRequest::new(id.clone(), method, params);
        let enqueue = self.send_outbound(JsonRpcMessage::Request(req), None);
        let admission = if timeout_duration.is_zero() {
            enqueue.await
        } else if let Ok(result) = timeout_at(deadline, enqueue).await {
            result
        } else {
            self.terminate_transport().await;
            Err(AcpError::TransportLost(
                "request queue admission timed out".into(),
            ))
        };
        if let Err(error) = admission {
            let mut pending = self.pending_requests.lock().await;
            pending.remove(&id);
            return Err(error);
        }

        if timeout_duration.is_zero() {
            return rx
                .await
                .map_err(|_| AcpError::TransportLost("connection dropped before response".into()));
        }
        match timeout_at(deadline, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err(AcpError::TransportLost(
                "connection dropped before response".into(),
            )),
            Err(_) => {
                let mut pending = self.pending_requests.lock().await;
                pending.remove(&id);
                Err(AcpError::Timeout {
                    operation: op_name,
                    elapsed: timeout_duration,
                })
            }
        }
    }

    async fn send_request(
        &self,
        method: impl Into<String>,
        params: Option<serde_json::Value>,
        timeout_duration: Duration,
        op_name: &'static str,
    ) -> Result<JsonRpcResponse, AcpError> {
        let id = self.next_id();
        self.send_request_with_id(id, method, params, timeout_duration, op_name)
            .await
    }
}

/// Client for communicating with an ACP v1 agent over stdio JSON-RPC.
pub struct AcpClient {
    shared: Arc<ClientShared>,
    config: AcpClientConfig,
    reader_task: tokio::task::JoinHandle<()>,
    writer_task: tokio::task::JoinHandle<()>,
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        self.reader_task.abort();
        self.writer_task.abort();
    }
}

impl AcpClient {
    /// Binds an already governed Kit attachment and verifies its declared ACP capabilities.
    /// The caller must verify the Kit command and workload digest before providing the streams.
    /// The returned agent session IDs are remote protocol IDs, not shell job or process IDs.
    ///
    /// # Errors
    /// Returns an error if the declaration is invalid, the handshake fails, or a required
    /// capability is absent. No session request is sent on failure.
    pub async fn connect_registered<
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    >(
        reader: R,
        writer: W,
        config: AcpClientConfig,
        permission_handler: Option<Arc<dyn PermissionHandler>>,
        declaration: &AgentAdapterDeclaration,
        client_info: Option<ImplementationInfo>,
    ) -> Result<(Self, InitializeResponse), AcpError> {
        declaration
            .validate()
            .map_err(|e| AcpError::Protocol(e.to_string()))?;
        if declaration.protocol != AgentProtocol::AcpV1 {
            return Err(AcpError::Protocol(
                "registered adapter is not ACP v1".into(),
            ));
        }
        let client = Self::new(reader, writer, config, permission_handler);
        let response = client.initialize(client_info).await?;
        if let Some(capability) =
            declaration.missing_capability(response.agent_capabilities.as_ref())
        {
            return Err(AcpError::RequiredCapabilityMissing(capability.to_string()));
        }
        Ok((client, response))
    }

    /// Creates and connects a new ACP v1 client from arbitrary async read/write streams.
    ///
    /// Starts background tasks for dispatching responses, notifications, and rejecting unauthorized host RPCs.
    #[must_use]
    pub fn new<R: AsyncRead + Unpin + Send + 'static, W: AsyncWrite + Unpin + Send + 'static>(
        reader: R,
        writer: W,
        config: AcpClientConfig,
        permission_handler: Option<Arc<dyn PermissionHandler>>,
    ) -> Self {
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<OutboundItem>(64);
        let (termination, _) = watch::channel(false);
        let shared = Arc::new(ClientShared {
            next_request_id: AtomicU64::new(1),
            next_turn_id: AtomicU64::new(1),
            outbound_tx,
            pending_requests: TokioMutex::new(BTreeMap::new()),
            active_turn: TokioMutex::new(None),
            session_cancel_trackers: TokioMutex::new(BTreeMap::new()),
            permission_handler: permission_handler
                .unwrap_or_else(|| Arc::new(DefaultDenyPermissionHandler)),
            permission_timeout: config.permission_timeout,
            transport_timeout: config.cancel_timeout,
            agent_capabilities: TokioMutex::new(None),
            is_terminated: AtomicBool::new(false),
            termination,
            closed: CancellationToken::new(),
            dropped_updates: AtomicU64::new(0),
            out_of_turn_updates: AtomicU64::new(0),
        });

        let mut msg_reader = MessageReader::new(reader, config.max_frame_bytes);
        let mut msg_writer = MessageWriter::new(writer);

        let shared_writer = Arc::clone(&shared);
        let writer_task = tokio::spawn(async move {
            tokio::select! {
                biased;
                () = shared_writer.closed.cancelled() => {}
                () = async {
                    while let Some(outbound) = outbound_rx.recv().await {
                        match timeout(shared_writer.transport_timeout, msg_writer.send_message(&outbound.message)).await {
                            Ok(Ok(())) => {}
                            _ => break,
                        }
                        if let Some(tracker) = outbound.cancel_tracker {
                            tracker.mark_dispatched();
                        }
                        if let Some(on_dispatched) = outbound.on_dispatched {
                            let _ = on_dispatched.send(());
                        }
                    }
                } => {}
            }
            shared_writer.terminate_transport().await;
        });

        let shared_reader = Arc::clone(&shared);
        let reader_task = tokio::spawn(async move {
            tokio::select! {
                biased;
                () = shared_reader.closed.cancelled() => {}
                () = async {
                    loop {
                        match msg_reader.recv_message().await {
                            Ok(Some(msg)) => Self::handle_inbound_message(&shared_reader, msg).await,
                            Ok(None) => break,
                            Err(e) => {
                                Self::count_invalid_update(&shared_reader).await;
                                tracing::warn!("error reading from ACP transport: {e}");
                                break;
                            }
                        }
                    }
                } => {}
            }
            shared_reader.terminate_transport().await;
        });

        Self {
            shared,
            config,
            reader_task,
            writer_task,
        }
    }

    async fn count_invalid_update(shared: &ClientShared) {
        if shared.active_turn.lock().await.is_some() {
            shared.dropped_updates.fetch_add(1, Ordering::Relaxed);
        } else {
            shared.out_of_turn_updates.fetch_add(1, Ordering::Relaxed);
        }
    }

    async fn handle_inbound_message(shared: &Arc<ClientShared>, msg: JsonRpcMessage) {
        match msg {
            JsonRpcMessage::Response(resp) => {
                // If this response belongs to the active or cancelling turn, update tracker
                {
                    let mut guard = shared.active_turn.lock().await;
                    if let Some(state) = &*guard
                        && state.prompt_request_id() == &resp.id
                    {
                        if is_stop_reason_cancelled(&resp) {
                            state.cancel_tracker().mark_confirmed();
                        }
                        // Fence at the wire terminal, not whenever the prompt
                        // future next runs. Later notifications are unscoped,
                        // never nondeterministically attributed to this turn.
                        *guard = None;
                    }
                }
                let mut pending = shared.pending_requests.lock().await;
                if let Some(tx) = pending.remove(&resp.id) {
                    let _ = tx.send(resp);
                }
            }
            JsonRpcMessage::Notification(notif) => {
                Self::dispatch_notification(shared, notif).await;
            }
            JsonRpcMessage::Request(req) => {
                // SECURITY ENFORCEMENT: Explicit NO HOST EXECUTION from agent RPC.
                // Rejects fs and terminal execution; passes permission requests to bounded policy.
                Self::handle_agent_request(shared, req).await;
            }
        }
    }

    async fn dispatch_notification(shared: &Arc<ClientShared>, notif: JsonRpcNotification) {
        if notif.method != "session/update" {
            return;
        }
        let Some(params) = notif.params else {
            Self::count_invalid_update(shared).await;
            return;
        };
        let Some(session_id) = params.get("sessionId").and_then(serde_json::Value::as_str) else {
            Self::count_invalid_update(shared).await;
            return;
        };
        let delivery = {
            let turn_guard = shared.active_turn.lock().await;
            match &*turn_guard {
                Some(TurnState::Active {
                    session_id: active,
                    update_sink,
                    update_cancelled,
                    delivery_abandoned,
                    ..
                }) if active == session_id => Some((
                    update_sink.clone(),
                    update_cancelled.clone(),
                    Arc::clone(delivery_abandoned),
                )),
                Some(TurnState::Cancelling {
                    session_id: active, ..
                }) if active == session_id => {
                    shared.dropped_updates.fetch_add(1, Ordering::Relaxed);
                    None
                }
                _ if is_session_state_update(&params) => None,
                _ => {
                    shared.out_of_turn_updates.fetch_add(1, Ordering::Relaxed);
                    None
                }
            }
        };
        let Some((sink, cancelled, abandoned)) = delivery else {
            return;
        };
        let Some(update) = params.get("update").filter(|value| {
            value.is_object()
                && value
                    .get("sessionUpdate")
                    .is_some_and(serde_json::Value::is_string)
        }) else {
            shared.dropped_updates.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if abandoned.load(Ordering::Acquire) {
            shared.dropped_updates.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // No active_turn lock spans this wait. Healthy post-cancel output stays
        // lossless. A consumer stalled for the entire deadline fences ordinary
        // transport; during cancellation we instead count and abandon remaining
        // delivery to reach the terminal response. Pay that deadline only once.
        let result = tokio::select! {
            biased;
            result = timeout(shared.transport_timeout, sink.send(update.clone())) => Some(result),
            () = shared.closed.cancelled() => None,
        };
        if !matches!(result, Some(Ok(Ok(())))) {
            shared.dropped_updates.fetch_add(1, Ordering::Relaxed);
        }
        if matches!(result, Some(Err(_))) {
            if cancelled.is_cancelled() {
                abandoned.store(true, Ordering::Release);
            } else {
                shared.terminate_transport().await;
            }
        }
    }

    /// Number of updates omitted because their envelope/projection was invalid,
    /// their consumer closed/stalled, or cancellation interrupted backpressure.
    #[must_use]
    pub fn dropped_updates(&self) -> u64 {
        self.shared.dropped_updates.load(Ordering::Acquire)
    }

    /// Notifications received outside an active prompt are not turn output.
    /// Expose omissions separately so they cannot silently mutate a completed
    /// turn or disappear from the session's capture-gap accounting.
    #[must_use]
    pub fn out_of_turn_updates(&self) -> u64 {
        self.shared.out_of_turn_updates.load(Ordering::Acquire)
    }

    /// Signals when either side of the ACP transport has ended.
    #[must_use]
    pub fn transport_termination(&self) -> watch::Receiver<bool> {
        self.shared.termination.subscribe()
    }

    #[allow(clippy::too_many_lines)]
    async fn handle_agent_request(shared: &Arc<ClientShared>, req: JsonRpcRequest) {
        let method = req.method.as_str();

        // Strictly reject any terminal execution attempts from the agent
        if method.starts_with("terminal/") {
            let error = JsonRpcError::method_not_found(method);
            let response = JsonRpcResponse::err(req.id, error);
            let _ = shared
                .send_outbound(JsonRpcMessage::Response(response), None)
                .await;
            return;
        }

        // Strictly reject any filesystem modification/read attempts on the host
        if method.starts_with("fs/") {
            let error = JsonRpcError::method_not_found(method);
            let response = JsonRpcResponse::err(req.id, error);
            let _ = shared
                .send_outbound(JsonRpcMessage::Response(response), None)
                .await;
            return;
        }

        // Advisory permission request from agent
        if method == "session/request_permission" {
            let Some(params) = req.params else {
                let response = JsonRpcResponse::err(
                    req.id,
                    JsonRpcError::new(
                        crate::protocol::JSONRPC_INVALID_PARAMS,
                        "missing permission request params",
                    ),
                );
                let _ = shared
                    .send_outbound(JsonRpcMessage::Response(response), None)
                    .await;
                return;
            };

            let Ok(perm_req) = serde_json::from_value::<RequestPermissionRequest>(params) else {
                let response = JsonRpcResponse::err(
                    req.id,
                    JsonRpcError::new(
                        crate::protocol::JSONRPC_INVALID_PARAMS,
                        "invalid permission request params",
                    ),
                );
                let _ = shared
                    .send_outbound(JsonRpcMessage::Response(response), None)
                    .await;
                return;
            };

            let req_id = req.id;
            let tracker_and_perms = {
                let guard = shared.active_turn.lock().await;
                match &*guard {
                    Some(TurnState::Active {
                        session_id,
                        pending_permissions,
                        cancel_tracker,
                        ..
                    }) if session_id == &perm_req.session_id && !cancel_tracker.is_requested() => {
                        Some((Arc::clone(pending_permissions), Arc::clone(cancel_tracker)))
                    }
                    _ => None,
                }
            };

            // If no active turn for this session, or cancel was already requested/dispatched,
            // answer cancelled immediately.
            let Some((pending_perms, cancel_tracker)) = tracker_and_perms else {
                let resp_payload = RequestPermissionResponse {
                    outcome: RequestPermissionOutcome::Cancelled,
                };
                if let Ok(val) = serde_json::to_value(resp_payload) {
                    let response = JsonRpcResponse::ok(req_id, val);
                    let _ = shared
                        .send_outbound(JsonRpcMessage::Response(response), None)
                        .await;
                }
                return;
            };

            // Hold lock through spawn and insert so that fast handler tasks (e.g. default-deny)
            // cannot run and check pending before this registration is inserted into the map!
            let mut guard = pending_perms.lock().await;
            if cancel_tracker.is_requested() {
                drop(guard);
                let resp_payload = RequestPermissionResponse {
                    outcome: RequestPermissionOutcome::Cancelled,
                };
                if let Ok(val) = serde_json::to_value(resp_payload) {
                    let response = JsonRpcResponse::ok(req_id, val);
                    let _ = shared
                        .send_outbound(JsonRpcMessage::Response(response), None)
                        .await;
                }
                return;
            }

            let handler = Arc::clone(&shared.permission_handler);
            let shared_clone = Arc::clone(shared);
            let pending_perms_clone = Arc::clone(&pending_perms);
            let req_id_clone = req_id.clone();
            let permission_timeout = shared.permission_timeout;

            let handle = tokio::spawn(async move {
                let outcome_result =
                    timeout(permission_timeout, handler.handle_permission(perm_req)).await;

                let was_pending = {
                    let mut guard = pending_perms_clone.lock().await;
                    guard.remove(&req_id_clone).is_some()
                };

                if was_pending {
                    let outcome = outcome_result.unwrap_or_else(|_| {
                        tracing::warn!("permission handler timed out; answering cancelled");
                        RequestPermissionOutcome::Cancelled
                    });
                    let resp_payload = RequestPermissionResponse { outcome };
                    if let Ok(val) = serde_json::to_value(resp_payload) {
                        let response = JsonRpcResponse::ok(req_id_clone, val);
                        let _ = shared_clone
                            .send_outbound(JsonRpcMessage::Response(response), None)
                            .await;
                    }
                }
            });

            guard.insert(req_id, handle.abort_handle());
            drop(guard);
            return;
        }

        // Unknown or unsupported agent-to-client method
        let error = JsonRpcError::method_not_found(method);
        let response = JsonRpcResponse::err(req.id, error);
        let _ = shared
            .send_outbound(JsonRpcMessage::Response(response), None)
            .await;
    }

    /// Performs the ACP v1 initialization handshake with the agent.
    ///
    /// # Errors
    /// Returns [`AcpError::Timeout`] if agent does not reply within `init_timeout`.
    /// Returns [`AcpError::Protocol`] if protocol version returned does not match 1.
    /// Returns [`AcpError::JsonRpc`] if agent returns an error.
    pub async fn initialize(
        &self,
        client_info: Option<ImplementationInfo>,
    ) -> Result<InitializeResponse, AcpError> {
        let req = InitializeRequest {
            protocol_version: ACP_V1_PROTOCOL_VERSION,
            client_capabilities: Some(ClientCapabilities {
                fs: None,
                terminal: Some(false),
            }),
            client_info,
        };

        let params = serde_json::to_value(req)?;
        let response = self
            .shared
            .send_request(
                "initialize",
                Some(params),
                self.config.init_timeout,
                "initialize",
            )
            .await?;

        if let Some(err) = response.error {
            return Err(AcpError::JsonRpc {
                code: err.code,
                message: err.message,
                data: err.data,
            });
        }

        let result_val = response
            .result
            .ok_or_else(|| AcpError::Protocol("missing result in initialize response".into()))?;

        let init_resp: InitializeResponse = serde_json::from_value(result_val)?;
        if init_resp.protocol_version != ACP_V1_PROTOCOL_VERSION {
            return Err(AcpError::Protocol(format!(
                "unsupported agent protocol version {}; expected {}",
                init_resp.protocol_version, ACP_V1_PROTOCOL_VERSION
            )));
        }

        if let Some(caps) = &init_resp.agent_capabilities {
            let mut guard = self.shared.agent_capabilities.lock().await;
            *guard = Some(caps.clone());
        }

        Ok(init_resp)
    }

    /// Creates a new conversation session via `session/new`.
    ///
    /// # Errors
    /// Returns [`AcpError`] on failure or timeout.
    pub async fn new_session(
        &self,
        cwd: impl Into<String>,
        mcp_servers: Vec<serde_json::Value>,
    ) -> Result<String, AcpError> {
        let req = NewSessionRequest {
            cwd: cwd.into(),
            mcp_servers,
        };

        let params = serde_json::to_value(req)?;
        let response = self
            .shared
            .send_request(
                "session/new",
                Some(params),
                self.config.init_timeout,
                "session/new",
            )
            .await?;

        if let Some(err) = response.error {
            return Err(AcpError::JsonRpc {
                code: err.code,
                message: err.message,
                data: err.data,
            });
        }

        let result_val = response
            .result
            .ok_or_else(|| AcpError::Protocol("missing result in session/new response".into()))?;

        let new_resp: NewSessionResponse = serde_json::from_value(result_val)?;
        Ok(new_resp.session_id)
    }

    /// Submits a sequential prompt turn for a session.
    ///
    /// # Invariants
    /// - Only ONE prompt turn may be active across the session. A concurrent turn attempt returns [`AcpError::Busy`].
    /// - Streamed notifications (`session/update`) are dispatched to `update_sink`.
    /// - Bounded by `turn_timeout` when nonzero; daemon-owned Kit jobs use
    ///   their job wall ceiling instead.
    ///
    /// # Errors
    /// Returns [`AcpError::Busy`] if a turn is already executing.
    /// Returns [`AcpError::Timeout`] if turn exceeds timeout.
    /// Returns [`AcpError::JsonRpc`] if agent fails.
    pub async fn prompt(
        &self,
        session_id: &str,
        prompt_blocks: Vec<ContentBlock>,
        update_sink: mpsc::Sender<SessionUpdate>,
    ) -> Result<PromptResponse, AcpError> {
        self.prompt_with_sink(session_id, prompt_blocks, UpdateSink::Typed(update_sink))
            .await
    }

    /// Submit a turn preserving the complete update JSON, including unknown
    /// kinds, content, diff, locations and metadata. Envelopes still require a
    /// matching session ID and an object with a string `sessionUpdate` tag.
    /// Queue memory is at most its capacity times `max_frame_bytes`.
    ///
    /// # Errors
    /// Same admission, timeout and transport errors as [`Self::prompt`].
    pub async fn prompt_raw(
        &self,
        session_id: &str,
        prompt_blocks: Vec<ContentBlock>,
        update_sink: mpsc::Sender<serde_json::Value>,
    ) -> Result<PromptResponse, AcpError> {
        self.prompt_with_sink(session_id, prompt_blocks, UpdateSink::Raw(update_sink))
            .await
    }

    async fn prompt_with_sink(
        &self,
        session_id: &str,
        prompt_blocks: Vec<ContentBlock>,
        update_sink: UpdateSink,
    ) -> Result<PromptResponse, AcpError> {
        let cancel_tracker = Arc::new(CancelTracker::new());
        let turn_id = self.shared.next_turn_id();
        let prompt_request_id = self.shared.next_id();
        let pending_permissions = Arc::new(TokioMutex::new(BTreeMap::new()));

        // Sequential gate: Check and set active turn
        {
            let mut active_turn_guard = self.shared.active_turn.lock().await;
            if active_turn_guard.is_some() {
                return Err(AcpError::Busy);
            }
            self.shared
                .session_cancel_trackers
                .lock()
                .await
                .insert(session_id.to_string(), Arc::clone(&cancel_tracker));
            *active_turn_guard = Some(TurnState::Active {
                turn_id,
                session_id: session_id.to_string(),
                prompt_request_id: prompt_request_id.clone(),
                cancel_tracker: Arc::clone(&cancel_tracker),
                update_sink,
                update_cancelled: CancellationToken::new(),
                delivery_abandoned: Arc::new(AtomicBool::new(false)),
                pending_permissions: Arc::clone(&pending_permissions),
            });
        }

        let mut turn_guard = TurnGuard {
            shared: Arc::clone(&self.shared),
            turn_id,
            session_id: session_id.to_string(),
            prompt_request_id: prompt_request_id.clone(),
            pending_permissions: Arc::clone(&pending_permissions),
            completed: false,
        };

        let req = PromptRequest {
            session_id: session_id.to_string(),
            prompt: prompt_blocks,
        };

        let params = serde_json::to_value(req)?;
        let response_result = self
            .shared
            .send_request_with_id(
                prompt_request_id,
                "session/prompt",
                Some(params),
                self.config.turn_timeout,
                "session/prompt",
            )
            .await;

        let response = response_result?;

        // Turn completed: mark turn guard completed and release active turn
        turn_guard.completed = true;
        abort_and_cancel_pending_permissions(&pending_permissions, &self.shared).await;
        {
            let mut active_turn_guard = self.shared.active_turn.lock().await;
            if let Some(state) = &*active_turn_guard
                && state.turn_id() == turn_id
            {
                *active_turn_guard = None;
            }
        }

        if let Some(err) = response.error {
            return Err(AcpError::JsonRpc {
                code: err.code,
                message: err.message,
                data: err.data,
            });
        }

        let result_val = response.result.ok_or_else(|| {
            AcpError::Protocol("missing result in session/prompt response".into())
        })?;

        let prompt_resp: PromptResponse = serde_json::from_value(result_val)?;
        if prompt_resp.stop_reason == StopReason::Cancelled {
            cancel_tracker.mark_confirmed();
        }

        Ok(prompt_resp)
    }

    /// Requests cancellation of the currently active prompt turn.
    ///
    /// # Phased Cancellation (PRD ACP-05)
    /// Returns the cancellation phase after dispatching notification (`CancelPhase::Dispatched`).
    /// The awaiting prompt turn will transition to `CancelPhase::Confirmed` upon response.
    ///
    /// # Errors
    /// Returns [`AcpError::NoActivePrompt`] if no prompt is running for this session.
    pub async fn cancel_prompt(&self, session_id: &str) -> Result<CancelPhase, AcpError> {
        let (cancel_tracker, pending_permissions) = {
            let active_turn_guard = self.shared.active_turn.lock().await;
            match &*active_turn_guard {
                Some(turn) if turn.session_id() == session_id => {
                    (turn.cancel_tracker(), turn.pending_permissions())
                }
                _ => return Err(AcpError::NoActivePrompt(session_id.to_string())),
            }
        };

        self.shared
            .dispatch_cancel(session_id, &cancel_tracker, &pending_permissions)
            .await
    }

    /// Returns the cancellation phase of the active or most recent prompt turn for `session_id`.
    pub async fn session_cancel_phase(&self, session_id: &str) -> Option<CancelPhase> {
        let trackers = self.shared.session_cancel_trackers.lock().await;
        trackers.get(session_id).map(|t| t.phase())
    }

    /// Restores a session and replays history via `session/load`.
    ///
    /// Capability-gated: Returns [`AcpError::CapabilityNotSupported`] without sending wire RPC
    /// if `agentCapabilities.loadSession` is not true.
    ///
    /// # Errors
    /// Returns [`AcpError::CapabilityNotSupported`] if unsupported by agent.
    pub async fn load_session(
        &self,
        session_id: &str,
        cwd: impl Into<String>,
        mcp_servers: Vec<serde_json::Value>,
    ) -> Result<LoadSessionResponse, AcpError> {
        {
            let caps_guard = self.shared.agent_capabilities.lock().await;
            let supported = caps_guard
                .as_ref()
                .is_some_and(AgentCapabilities::can_load_session);
            if !supported {
                return Err(AcpError::CapabilityNotSupported("loadSession"));
            }
        }

        let req = LoadSessionRequest {
            session_id: session_id.to_string(),
            cwd: cwd.into(),
            mcp_servers,
        };
        let params = serde_json::to_value(req)?;
        let response = self
            .shared
            .send_request(
                "session/load",
                Some(params),
                self.config.init_timeout,
                "session/load",
            )
            .await?;

        if let Some(err) = response.error {
            return Err(AcpError::JsonRpc {
                code: err.code,
                message: err.message,
                data: err.data,
            });
        }

        Ok(LoadSessionResponse {})
    }

    /// Resumes a session without replaying history via `session/resume`.
    ///
    /// Capability-gated: Returns [`AcpError::CapabilityNotSupported`] without sending wire RPC
    /// if `agentCapabilities.sessionCapabilities.resume` is not present.
    ///
    /// # Errors
    /// Returns [`AcpError::CapabilityNotSupported`] if unsupported by agent.
    pub async fn resume_session(
        &self,
        session_id: &str,
        cwd: impl Into<String>,
        mcp_servers: Option<Vec<serde_json::Value>>,
    ) -> Result<ResumeSessionResponse, AcpError> {
        {
            let caps_guard = self.shared.agent_capabilities.lock().await;
            let supported = caps_guard
                .as_ref()
                .is_some_and(AgentCapabilities::can_resume_session);
            if !supported {
                return Err(AcpError::CapabilityNotSupported(
                    "sessionCapabilities.resume",
                ));
            }
        }

        let req = ResumeSessionRequest {
            session_id: session_id.to_string(),
            cwd: cwd.into(),
            mcp_servers,
        };
        let params = serde_json::to_value(req)?;
        let response = self
            .shared
            .send_request(
                "session/resume",
                Some(params),
                self.config.init_timeout,
                "session/resume",
            )
            .await?;

        if let Some(err) = response.error {
            return Err(AcpError::JsonRpc {
                code: err.code,
                message: err.message,
                data: err.data,
            });
        }

        Ok(ResumeSessionResponse {})
    }
}

/// Agent session state announced outside any prompt turn (for example the
/// command list an agent sends right after `session/new`). It carries no turn
/// output, so it is neither a lost update nor part of a turn transcript.
fn is_session_state_update(params: &serde_json::Value) -> bool {
    params
        .get("update")
        .and_then(|update| update.get("sessionUpdate"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|kind| {
            matches!(
                kind,
                "available_commands_update"
                    | "current_mode_update"
                    | "config_option_update"
                    | "session_info_update"
            )
        })
}

#[cfg(test)]
mod update_loss_tests {
    use super::*;

    #[tokio::test]
    async fn zero_timeout_request_finishes_when_writer_disconnects_without_reader_eof() {
        let (reader, _remote_writer) = tokio::io::duplex(64);
        let (writer, remote_reader) = tokio::io::duplex(64);
        drop(remote_reader);
        let client = AcpClient::new(reader, writer, AcpClientConfig::default(), None);

        let result = timeout(
            Duration::from_secs(1),
            client
                .shared
                .send_request("session/prompt", None, Duration::ZERO, "session/prompt"),
        )
        .await
        .expect("transport loss must wake an unbounded request");
        assert!(matches!(result, Err(AcpError::TransportLost(_))));
        assert!(client.shared.pending_requests.lock().await.is_empty());
    }

    #[tokio::test]
    async fn terminal_flag_set_while_registration_waits_cannot_strand_request() {
        let (reader, _remote_writer) = tokio::io::duplex(64);
        let client = AcpClient::new(reader, tokio::io::sink(), AcpClientConfig::default(), None);

        // Force the old interleaving: request checks the flag, then blocks on
        // the pending lock; EOF sets the flag before obtaining that lock. The
        // production termination path now makes this ordering impossible, and
        // registration still checks under the lock as defense in depth.
        let pending = client.shared.pending_requests.lock().await;
        let shared = Arc::clone(&client.shared);
        let request = tokio::spawn(async move {
            shared
                .send_request("session/prompt", None, Duration::ZERO, "session/prompt")
                .await
        });
        tokio::task::yield_now().await;
        client.shared.is_terminated.store(true, Ordering::Release);
        drop(pending);

        let result = timeout(Duration::from_secs(1), request)
            .await
            .expect("registration race must not strand the request")
            .expect("request task must succeed");
        assert!(matches!(result, Err(AcpError::TransportLost(_))));
        assert!(client.shared.pending_requests.lock().await.is_empty());
    }
}
