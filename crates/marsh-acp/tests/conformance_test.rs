//! ACP v1 wire behavior of the real client against a hand-driven peer.
//!
//! The test body plays the agent over an in-memory duplex, so every ordering
//! (prompt received, cancel sent, reply written) is explicit rather than
//! slept for. Fixture JSON follows <https://agentclientprotocol.com/protocol/v1/>.

use marsh_acp::cancel::CancelPhase;
use marsh_acp::client::{AcpClient, AcpClientConfig, PermissionHandler};
use marsh_acp::error::AcpError;
use marsh_acp::protocol::{
    ContentBlock, PlanEntryPriority, PlanEntryStatus, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, SessionUpdate, StopReason,
};
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const SESSION: &str = "123e4567-e89b-12d3-a456-426614174000";

/// Bounds a wait that completes promptly unless the client is wedged.
const CONDITION_DEADLINE: Duration = Duration::from_secs(30);

/// A permission that only the test or a cancel can answer: its own timeout is
/// far beyond any condition deadline.
const HELD_PERMISSIONS: AcpClientConfig = AcpClientConfig {
    max_frame_bytes: 1024 * 1024,
    init_timeout: Duration::from_secs(10),
    turn_timeout: Duration::from_mins(1),
    cancel_timeout: Duration::from_secs(10),
    permission_timeout: Duration::from_hours(1),
};

const SPEC_INITIALIZE_RESULT: &str = r#"{
  "protocolVersion": 1,
  "agentCapabilities": { "loadSession": true, "sessionCapabilities": { "resume": {} } },
  "agentInfo": { "name": "official-acp-spec-agent", "version": "1.0.0" }
}"#;

const SPEC_PLAN_UPDATE: &str = r#"{
  "jsonrpc": "2.0",
  "method": "session/update",
  "params": {
    "sessionId": "123e4567-e89b-12d3-a456-426614174000",
    "update": {
      "sessionUpdate": "plan",
      "entries": [
        { "content": "Analyze project requirements", "priority": "high", "status": "completed" },
        { "content": "Design system architecture", "priority": "medium", "status": "in_progress" },
        { "content": "Implement conformance tests", "priority": "low", "status": "pending" }
      ]
    }
  }
}"#;

const SPEC_PERMISSION_REQUEST: &str = r#"{
  "jsonrpc": "2.0",
  "id": 100,
  "method": "session/request_permission",
  "params": {
    "sessionId": "123e4567-e89b-12d3-a456-426614174000",
    "toolCall": {
      "toolCallId": "call_git_diff_01",
      "title": "Inspect git working tree",
      "kind": "read",
      "status": "pending"
    },
    "options": [
      { "optionId": "opt_allow_once", "name": "Allow this operation once", "kind": "allow_once" },
      { "optionId": "opt_reject_always", "name": "Reject and deny always", "kind": "reject_always" }
    ]
  }
}"#;

/// The agent end of the wire, driven from the test body.
struct Agent {
    lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
    writer: WriteHalf<DuplexStream>,
}

impl Agent {
    async fn recv(&mut self) -> Value {
        let line = tokio::time::timeout(CONDITION_DEADLINE, self.lines.next_line())
            .await
            .expect("client sent nothing")
            .unwrap()
            .expect("client closed the wire");
        serde_json::from_str(&line).unwrap()
    }

    async fn expect(&mut self, method: &str) -> Value {
        let message = self.recv().await;
        assert_eq!(message["method"], method, "{message}");
        message
    }

    async fn write_raw(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn send(&mut self, message: &Value) {
        self.write_raw(format!("{message}\n").as_bytes()).await;
    }

    async fn reply(&mut self, request: &Value, result: Value) {
        self.send(&json!({"jsonrpc": "2.0", "id": request["id"], "result": result}))
            .await;
    }

    async fn end_turn(&mut self, prompt: &Value, stop_reason: &str) {
        self.reply(prompt, json!({ "stopReason": stop_reason }))
            .await;
    }

    async fn request_permission(&mut self, id: u64, options: Value) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/request_permission",
            "params": {
                "sessionId": SESSION,
                "toolCall": { "toolCallId": format!("tc-{id}") },
                "options": options,
            }
        }))
        .await;
    }

    async fn agent_message(&mut self, text: &str) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": SESSION,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": text }
                }
            }
        }))
        .await;
    }
}

fn wire(
    config: AcpClientConfig,
    handler: Option<Arc<dyn PermissionHandler>>,
) -> (Arc<AcpClient>, Agent) {
    let (client_io, agent_io) = tokio::io::duplex(64 * 1024);
    let (client_reader, client_writer) = tokio::io::split(client_io);
    let (agent_reader, agent_writer) = tokio::io::split(agent_io);
    let client = Arc::new(AcpClient::new(
        client_reader,
        client_writer,
        config,
        handler,
    ));
    let agent = Agent {
        lines: BufReader::new(agent_reader).lines(),
        writer: agent_writer,
    };
    (client, agent)
}

/// Runs initialize and session/new against `agent`.
async fn open_session(client: &AcpClient, agent: &mut Agent) {
    let (init, ()) = tokio::join!(client.initialize(None), async {
        let request = agent.expect("initialize").await;
        agent.reply(&request, json!({ "protocolVersion": 1 })).await;
    });
    init.unwrap();
    let (session, ()) = tokio::join!(client.new_session("/workspace/project", vec![]), async {
        let request = agent.expect("session/new").await;
        agent.reply(&request, json!({ "sessionId": SESSION })).await;
    });
    assert_eq!(session.unwrap(), SESSION);
}

async fn connected(
    config: AcpClientConfig,
    handler: Option<Arc<dyn PermissionHandler>>,
) -> (Arc<AcpClient>, Agent) {
    let (client, mut agent) = wire(config, handler);
    open_session(&client, &mut agent).await;
    (client, agent)
}

type Turn = JoinHandle<Result<PromptResponse, AcpError>>;

fn spawn_prompt(client: &Arc<AcpClient>, text: &str) -> (Turn, mpsc::Receiver<SessionUpdate>) {
    let (updates, received) = mpsc::channel(64);
    let client = Arc::clone(client);
    let blocks = vec![ContentBlock::text(text)];
    let turn = tokio::spawn(async move { client.prompt(SESSION, blocks, updates).await });
    (turn, received)
}

async fn next_update(updates: &mut mpsc::Receiver<SessionUpdate>) -> Option<SessionUpdate> {
    tokio::time::timeout(CONDITION_DEADLINE, updates.recv())
        .await
        .expect("no update delivered")
}

async fn next_message(updates: &mut mpsc::Receiver<SessionUpdate>) -> ContentBlock {
    match next_update(updates).await {
        Some(SessionUpdate::AgentMessageChunk { content }) => content,
        other => panic!("expected an agent message chunk, got {other:?}"),
    }
}

async fn asked_permission(asked: &mut mpsc::UnboundedReceiver<RequestPermissionRequest>) {
    tokio::time::timeout(CONDITION_DEADLINE, asked.recv())
        .await
        .expect("the permission handler was never asked")
        .unwrap();
}

async fn finish(turn: Turn) -> Result<PromptResponse, AcpError> {
    tokio::time::timeout(CONDITION_DEADLINE, turn)
        .await
        .expect("turn never finished")
        .unwrap()
}

async fn wait_phase(client: &AcpClient, phase: CancelPhase) {
    tokio::time::timeout(CONDITION_DEADLINE, async {
        while client.session_cancel_phase(SESSION).await != Some(phase) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("cancel phase never reached {phase:?}"));
}

/// Signals when it is asked, then answers only what the test releases.
struct GatedHandler {
    asked: mpsc::UnboundedSender<RequestPermissionRequest>,
    answers: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<RequestPermissionOutcome>>>,
}

impl GatedHandler {
    fn new() -> (
        Arc<Self>,
        mpsc::UnboundedReceiver<RequestPermissionRequest>,
        mpsc::UnboundedSender<RequestPermissionOutcome>,
    ) {
        let (asked, asked_rx) = mpsc::unbounded_channel();
        let (answer_tx, answers) = mpsc::unbounded_channel();
        let handler = Arc::new(Self {
            asked,
            answers: Arc::new(tokio::sync::Mutex::new(answers)),
        });
        (handler, asked_rx, answer_tx)
    }
}

impl PermissionHandler for GatedHandler {
    fn handle_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> Pin<Box<dyn Future<Output = RequestPermissionOutcome> + Send>> {
        self.asked.send(request).unwrap();
        let answers = Arc::clone(&self.answers);
        Box::pin(async move {
            match answers.lock().await.recv().await {
                Some(answer) => answer,
                None => std::future::pending().await,
            }
        })
    }
}

struct SelectAllowOnce;

impl PermissionHandler for SelectAllowOnce {
    fn handle_permission(
        &self,
        request: RequestPermissionRequest,
    ) -> Pin<Box<dyn Future<Output = RequestPermissionOutcome> + Send>> {
        Box::pin(async move {
            request
                .options
                .iter()
                .find(|option| option.kind == "allow_once")
                .map_or_else(RequestPermissionOutcome::cancel, |option| {
                    RequestPermissionOutcome::select(option.option_id.clone())
                })
        })
    }
}

#[tokio::test]
async fn handshake_prompt_blocks_and_spec_plan_cross_the_wire() {
    let (client, mut agent) = wire(AcpClientConfig::default(), None);
    let (init, ()) = tokio::join!(client.initialize(None), async {
        let request = agent.expect("initialize").await;
        assert_eq!(request["params"]["protocolVersion"], 1);
        assert_eq!(request["params"]["clientCapabilities"]["terminal"], false);
        let result: Value = serde_json::from_str(SPEC_INITIALIZE_RESULT).unwrap();
        agent.reply(&request, result).await;
    });
    let init = init.unwrap();
    assert_eq!(init.agent_info.unwrap().name, "official-acp-spec-agent");
    let (session, ()) = tokio::join!(client.new_session("/workspace/project", vec![]), async {
        let request = agent.expect("session/new").await;
        assert_eq!(request["params"]["cwd"], "/workspace/project");
        assert_eq!(
            request["params"]["mcpServers"],
            json!([]),
            "ACP v1 requires mcpServers even when empty"
        );
        agent.reply(&request, json!({ "sessionId": SESSION })).await;
    });
    assert_eq!(session.unwrap(), SESSION);

    let (updates, mut received) = mpsc::channel(64);
    let prompting = Arc::clone(&client);
    let turn = tokio::spawn(async move {
        prompting
            .prompt(
                SESSION,
                vec![
                    ContentBlock::text("Review the test suite"),
                    ContentBlock::image("imgdata", "image/png"),
                    ContentBlock::audio("audiodata", "audio/wav"),
                    ContentBlock::resource_link("spec-doc", "file:///workspace/docs/spec.md"),
                ],
                updates,
            )
            .await
    });
    let prompt = agent.expect("session/prompt").await;
    let blocks = &prompt["params"]["prompt"];
    assert_eq!(prompt["params"]["sessionId"], SESSION);
    assert_eq!(
        blocks[0],
        json!({"type": "text", "text": "Review the test suite"})
    );
    assert_eq!(blocks[1]["type"], "image");
    assert_eq!(blocks[1]["mimeType"], "image/png");
    assert_eq!(blocks[1]["data"], "imgdata");
    assert_eq!(blocks[2]["type"], "audio");
    assert_eq!(blocks[2]["mimeType"], "audio/wav");
    assert_eq!(blocks[3]["type"], "resource_link");
    assert_eq!(blocks[3]["name"], "spec-doc");
    assert_eq!(blocks[3]["uri"], "file:///workspace/docs/spec.md");

    let plan: Value = serde_json::from_str(SPEC_PLAN_UPDATE).unwrap();
    agent.send(&plan).await;
    agent.end_turn(&prompt, "end_turn").await;
    assert_eq!(finish(turn).await.unwrap().stop_reason, StopReason::EndTurn);
    let Some(SessionUpdate::Plan { entries }) = next_update(&mut received).await else {
        panic!("expected a plan update");
    };
    let observed: Vec<_> = entries
        .iter()
        .map(|entry| (entry.content.as_str(), entry.priority, entry.status))
        .collect();
    assert_eq!(
        observed,
        [
            (
                "Analyze project requirements",
                PlanEntryPriority::High,
                PlanEntryStatus::Completed
            ),
            (
                "Design system architecture",
                PlanEntryPriority::Medium,
                PlanEntryStatus::InProgress
            ),
            (
                "Implement conformance tests",
                PlanEntryPriority::Low,
                PlanEntryStatus::Pending
            ),
        ]
    );
}

#[tokio::test]
async fn selected_permission_option_is_answered_on_the_wire() {
    let (client, mut agent) =
        connected(AcpClientConfig::default(), Some(Arc::new(SelectAllowOnce))).await;
    let (turn, _updates) = spawn_prompt(&client, "needs permission");
    let prompt = agent.expect("session/prompt").await;
    let request: Value = serde_json::from_str(SPEC_PERMISSION_REQUEST).unwrap();
    agent.send(&request).await;
    let answer = agent.recv().await;
    assert_eq!(answer["id"], 100);
    assert_eq!(
        answer["result"]["outcome"],
        json!({"outcome": "selected", "optionId": "opt_allow_once"})
    );
    agent.end_turn(&prompt, "end_turn").await;
    assert_eq!(finish(turn).await.unwrap().stop_reason, StopReason::EndTurn);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_deny_answers_every_rapid_permission_request() {
    let (client, mut agent) = connected(AcpClientConfig::default(), None).await;
    let (turn, _updates) = spawn_prompt(&client, "fast");
    let prompt = agent.expect("session/prompt").await;
    for id in 1001..=1005 {
        agent
            .request_permission(
                id,
                json!([{ "optionId": format!("reject-{id}"), "name": "Reject", "kind": "reject_once" }]),
            )
            .await;
        let answer = agent.recv().await;
        assert_eq!(answer["id"], id);
        assert_eq!(
            answer["result"]["outcome"],
            json!({"outcome": "selected", "optionId": format!("reject-{id}")})
        );
    }
    agent.end_turn(&prompt, "end_turn").await;
    assert_eq!(finish(turn).await.unwrap().stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn second_prompt_during_an_active_turn_is_busy() {
    let (client, mut agent) = connected(AcpClientConfig::default(), None).await;
    let (turn, _updates) = spawn_prompt(&client, "first");
    let prompt = agent.expect("session/prompt").await;
    let (updates, _received) = mpsc::channel(1);
    assert!(matches!(
        client
            .prompt(SESSION, vec![ContentBlock::text("second")], updates)
            .await,
        Err(AcpError::Busy)
    ));
    agent.end_turn(&prompt, "end_turn").await;
    assert_eq!(finish(turn).await.unwrap().stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn cancel_is_dispatched_then_confirmed_only_by_a_cancelled_stop_reason() {
    let (client, mut agent) = connected(AcpClientConfig::default(), None).await;

    // The agent finishes normally despite the cancel: not a confirmation.
    let (turn, _updates) = spawn_prompt(&client, "t1");
    let prompt = agent.expect("session/prompt").await;
    assert_eq!(
        client.cancel_prompt(SESSION).await.unwrap(),
        CancelPhase::Dispatched
    );
    let cancel = agent.expect("session/cancel").await;
    assert_eq!(cancel["params"]["sessionId"], SESSION);
    agent.end_turn(&prompt, "end_turn").await;
    assert_eq!(finish(turn).await.unwrap().stop_reason, StopReason::EndTurn);
    assert_eq!(
        client.session_cancel_phase(SESSION).await,
        Some(CancelPhase::Dispatched)
    );

    // A cancelled stop reason after a dispatched cancel confirms it.
    let (turn, _updates) = spawn_prompt(&client, "t2");
    let prompt = agent.expect("session/prompt").await;
    assert_eq!(
        client.cancel_prompt(SESSION).await.unwrap(),
        CancelPhase::Dispatched
    );
    agent.expect("session/cancel").await;
    agent.end_turn(&prompt, "cancelled").await;
    assert_eq!(
        finish(turn).await.unwrap().stop_reason,
        StopReason::Cancelled
    );
    assert_eq!(
        client.session_cancel_phase(SESSION).await,
        Some(CancelPhase::Confirmed)
    );

    // A -32800 error stays an error, not a confirmation.
    let (turn, _updates) = spawn_prompt(&client, "t3");
    let prompt = agent.expect("session/prompt").await;
    client.cancel_prompt(SESSION).await.unwrap();
    agent.expect("session/cancel").await;
    agent
        .send(&json!({"jsonrpc": "2.0", "id": prompt["id"],
            "error": {"code": -32800, "message": "Request cancelled by client"}}))
        .await;
    assert!(matches!(
        finish(turn).await,
        Err(AcpError::JsonRpc { code: -32800, ref message, .. }) if message == "Request cancelled by client"
    ));
    assert_eq!(
        client.session_cancel_phase(SESSION).await,
        Some(CancelPhase::Dispatched)
    );
}

#[tokio::test]
async fn unsolicited_cancelled_reply_or_error_never_confirms_a_cancel() {
    let (client, mut agent) = connected(AcpClientConfig::default(), None).await;

    let (turn, _updates) = spawn_prompt(&client, "t4");
    let prompt = agent.expect("session/prompt").await;
    agent.end_turn(&prompt, "cancelled").await;
    assert_eq!(
        finish(turn).await.unwrap().stop_reason,
        StopReason::Cancelled
    );
    assert_eq!(
        client.session_cancel_phase(SESSION).await,
        Some(CancelPhase::NotCancelled)
    );

    let (turn, _updates) = spawn_prompt(&client, "t5");
    let prompt = agent.expect("session/prompt").await;
    agent
        .send(&json!({"jsonrpc": "2.0", "id": prompt["id"],
            "error": {"code": -32800, "message": "Unsolicited cancel error"}}))
        .await;
    assert!(matches!(
        finish(turn).await,
        Err(AcpError::JsonRpc { code: -32800, .. })
    ));
    assert_eq!(
        client.session_cancel_phase(SESSION).await,
        Some(CancelPhase::NotCancelled)
    );
}

#[tokio::test]
async fn cancel_written_mid_frame_does_not_corrupt_the_inbound_update() {
    let (client, mut agent) = connected(AcpClientConfig::default(), None).await;
    let (turn, mut updates) = spawn_prompt(&client, "test");
    let prompt = agent.expect("session/prompt").await;
    agent
        .write_raw(
            br#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"123e4567-e89b-12d3-a456-426614174000","#,
        )
        .await;
    client.cancel_prompt(SESSION).await.unwrap();
    agent.expect("session/cancel").await;
    agent
        .write_raw(
            b"\"update\":{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"preserved chunk\"}}}}\n",
        )
        .await;
    agent.end_turn(&prompt, "cancelled").await;
    assert_eq!(
        finish(turn).await.unwrap().stop_reason,
        StopReason::Cancelled
    );
    assert_eq!(
        next_message(&mut updates).await,
        ContentBlock::text("preserved chunk")
    );
}

#[tokio::test]
async fn reader_keeps_delivering_updates_while_a_permission_is_pending() {
    let (handler, mut asked, answers) = GatedHandler::new();
    let (client, mut agent) = connected(HELD_PERMISSIONS, Some(handler)).await;
    let (turn, mut updates) = spawn_prompt(&client, "test");
    let prompt = agent.expect("session/prompt").await;
    agent
        .request_permission(
            501,
            json!([{ "optionId": "opt-1", "name": "Allow", "kind": "allow_once" }]),
        )
        .await;
    asked_permission(&mut asked).await;
    agent.agent_message("while waiting").await;
    assert_eq!(
        next_message(&mut updates).await,
        ContentBlock::text("while waiting")
    );
    answers
        .send(RequestPermissionOutcome::select("opt-1"))
        .unwrap();
    let answer = agent.recv().await;
    assert_eq!(answer["id"], 501);
    assert_eq!(answer["result"]["outcome"]["optionId"], "opt-1");
    agent.end_turn(&prompt, "end_turn").await;
    assert_eq!(finish(turn).await.unwrap().stop_reason, StopReason::EndTurn);
}

/// Reads the two frames a cancel produces while a permission is pending: the
/// permission answered `cancelled` and `session/cancel`.
async fn expect_cancel_with_cancelled_permission(agent: &mut Agent, permission: u64) {
    let mut cancelled_permission = false;
    let mut cancel = false;
    for _ in 0..2 {
        let message = agent.recv().await;
        if message["method"] == "session/cancel" {
            cancel = true;
        } else {
            assert_eq!(message["id"], permission, "{message}");
            assert_eq!(
                message["result"]["outcome"],
                json!({"outcome": "cancelled"}),
                "a pending permission must be answered cancelled"
            );
            cancelled_permission = true;
        }
    }
    assert!(cancel && cancelled_permission);
}

#[tokio::test]
async fn cancel_answers_a_pending_permission_cancelled() {
    let (handler, mut asked, _answers) = GatedHandler::new();
    let (client, mut agent) = connected(HELD_PERMISSIONS, Some(handler)).await;
    let (turn, _updates) = spawn_prompt(&client, "test");
    let prompt = agent.expect("session/prompt").await;
    agent
        .request_permission(
            502,
            json!([{ "optionId": "opt-1", "name": "Allow", "kind": "allow_once" }]),
        )
        .await;
    asked_permission(&mut asked).await;
    client.cancel_prompt(SESSION).await.unwrap();
    expect_cancel_with_cancelled_permission(&mut agent, 502).await;
    agent.end_turn(&prompt, "cancelled").await;
    assert_eq!(
        finish(turn).await.unwrap().stop_reason,
        StopReason::Cancelled
    );
}

#[tokio::test]
async fn dropped_caller_cancels_its_turn_and_the_confirmed_turn_admits_the_next() {
    let (handler, mut asked, _answers) = GatedHandler::new();
    let (client, mut agent) = connected(HELD_PERMISSIONS, Some(handler)).await;
    let (turn, mut stale) = spawn_prompt(&client, "dropped");
    let prompt = agent.expect("session/prompt").await;
    agent
        .request_permission(
            601,
            json!([{ "optionId": "opt-1", "name": "Allow", "kind": "allow_once" }]),
        )
        .await;
    asked_permission(&mut asked).await;
    turn.abort();
    expect_cancel_with_cancelled_permission(&mut agent, 601).await;

    // The turn stays fenced until the agent answers the cancel.
    wait_phase(&client, CancelPhase::Dispatched).await;
    let (updates, _received) = mpsc::channel(1);
    assert!(matches!(
        client
            .prompt(SESSION, vec![ContentBlock::text("too early")], updates)
            .await,
        Err(AcpError::Busy)
    ));
    agent.agent_message("after drop").await;
    agent.end_turn(&prompt, "cancelled").await;
    wait_phase(&client, CancelPhase::Confirmed).await;
    assert!(
        next_update(&mut stale).await.is_none(),
        "a dropped turn received output"
    );

    let (turn, mut updates) = spawn_prompt(&client, "fresh");
    let prompt = agent.expect("session/prompt").await;
    agent.agent_message("fresh output").await;
    agent.end_turn(&prompt, "end_turn").await;
    assert_eq!(finish(turn).await.unwrap().stop_reason, StopReason::EndTurn);
    assert_eq!(
        next_message(&mut updates).await,
        ContentBlock::text("fresh output")
    );
}

#[tokio::test]
async fn permission_requested_after_cancel_is_answered_cancelled() {
    // This handler would allow; the cancel must answer for it.
    let (client, mut agent) =
        connected(AcpClientConfig::default(), Some(Arc::new(SelectAllowOnce))).await;
    let (turn, _updates) = spawn_prompt(&client, "test");
    let prompt = agent.expect("session/prompt").await;
    client.cancel_prompt(SESSION).await.unwrap();
    agent.expect("session/cancel").await;
    agent
        .request_permission(
            777,
            json!([{ "optionId": "opt-1", "name": "Allow", "kind": "allow_once" }]),
        )
        .await;
    let answer = agent.recv().await;
    assert_eq!(answer["id"], 777);
    assert_eq!(answer["result"]["outcome"], json!({"outcome": "cancelled"}));
    agent.end_turn(&prompt, "cancelled").await;
    assert_eq!(
        finish(turn).await.unwrap().stop_reason,
        StopReason::Cancelled
    );
    assert_eq!(
        client.session_cancel_phase(SESSION).await,
        Some(CancelPhase::Confirmed)
    );
}

#[tokio::test(start_paused = true)]
async fn unanswered_permission_is_cancelled_at_its_timeout() {
    let (handler, mut asked, _answers) = GatedHandler::new();
    let config = AcpClientConfig {
        permission_timeout: Duration::from_millis(80),
        ..Default::default()
    };
    let (client, mut agent) = connected(config, Some(handler)).await;
    let (turn, _updates) = spawn_prompt(&client, "test");
    let prompt = agent.expect("session/prompt").await;
    agent
        .request_permission(
            701,
            json!([{ "optionId": "opt-1", "name": "Allow", "kind": "allow_once" }]),
        )
        .await;
    asked_permission(&mut asked).await;
    let asked_at = tokio::time::Instant::now();
    let answer = agent.recv().await;
    assert_eq!(answer["id"], 701);
    assert_eq!(answer["result"]["outcome"], json!({"outcome": "cancelled"}));
    let waited = asked_at.elapsed();
    assert!(
        waited >= Duration::from_millis(80) && waited < Duration::from_secs(1),
        "{waited:?}"
    );
    agent.end_turn(&prompt, "end_turn").await;
    assert_eq!(finish(turn).await.unwrap().stop_reason, StopReason::EndTurn);
}

#[tokio::test(start_paused = true)]
async fn bounded_turn_times_out_at_its_deadline() {
    let config = AcpClientConfig {
        turn_timeout: Duration::from_millis(100),
        ..Default::default()
    };
    let (client, mut agent) = connected(config, None).await;
    let started = tokio::time::Instant::now();
    let (turn, _updates) = spawn_prompt(&client, "never answered");
    agent.expect("session/prompt").await;
    let result = finish(turn).await;
    assert!(
        matches!(
            result,
            Err(AcpError::Timeout {
                operation: "session/prompt",
                ..
            })
        ),
        "{result:?}"
    );
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(100) && waited < Duration::from_secs(1),
        "{waited:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn unbounded_turn_outlives_any_deadline_until_cancelled() {
    let config = AcpClientConfig {
        turn_timeout: Duration::ZERO,
        ..Default::default()
    };
    let (client, mut agent) = connected(config, None).await;
    for _ in 0..2 {
        let (turn, _updates) = spawn_prompt(&client, "held");
        let prompt = agent.expect("session/prompt").await;
        tokio::time::sleep(Duration::from_hours(1)).await;
        assert!(!turn.is_finished(), "an unbounded turn timed out");
        assert_eq!(
            client.cancel_prompt(SESSION).await.unwrap(),
            CancelPhase::Dispatched
        );
        agent.expect("session/cancel").await;
        agent.end_turn(&prompt, "cancelled").await;
        assert_eq!(
            finish(turn).await.unwrap().stop_reason,
            StopReason::Cancelled
        );
    }
}

#[tokio::test]
async fn blank_lines_between_frames_are_ignored() {
    let (client, mut agent) = wire(AcpClientConfig::default(), None);
    let (init, ()) = tokio::join!(client.initialize(None), async {
        let request = agent.expect("initialize").await;
        agent.write_raw(b"\n\r\n   \n\n").await;
        agent.reply(&request, json!({ "protocolVersion": 1 })).await;
    });
    init.unwrap();
    let (session, ()) = tokio::join!(client.new_session("/tmp", vec![]), async {
        let request = agent.expect("session/new").await;
        agent.write_raw(b"\n\n").await;
        agent.reply(&request, json!({ "sessionId": SESSION })).await;
    });
    session.unwrap();
    let (turn, mut updates) = spawn_prompt(&client, "test");
    let prompt = agent.expect("session/prompt").await;
    agent.write_raw(b"\r\n\n").await;
    agent.agent_message("blank lines skipped").await;
    agent.write_raw(b"  \n\n").await;
    agent.end_turn(&prompt, "end_turn").await;
    assert_eq!(finish(turn).await.unwrap().stop_reason, StopReason::EndTurn);
    assert_eq!(
        next_message(&mut updates).await,
        ContentBlock::text("blank lines skipped")
    );
}

#[tokio::test]
async fn agent_terminal_and_filesystem_requests_are_refused() {
    let (_client, mut agent) = connected(AcpClientConfig::default(), None).await;
    for (id, method, params) in [
        (901, "terminal/create", json!({ "command": "bash" })),
        (
            902,
            "fs/write_text_file",
            json!({ "path": "/etc/shadow", "content": "bad" }),
        ),
        (903, "fs/read_text_file", json!({ "path": "/etc/shadow" })),
    ] {
        agent
            .send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        let answer = agent.recv().await;
        assert_eq!(answer["id"], id);
        assert_eq!(answer["error"]["code"], -32601, "{method} was not refused");
    }
}

/// An agent that floods requests while never reading its stdin cannot stall
/// the client's reader: the reader keeps consuming its output (here, an end of
/// stream behind the flood), and once the agent has left more than the client's
/// reply spool unread, the transport ends. Every write deadline is beyond the
/// test's wait, so only those two can end it.
#[tokio::test]
async fn flooding_agent_neither_stalls_the_reader_nor_spools_without_bound() {
    let config = AcpClientConfig {
        cancel_timeout: Duration::from_hours(1),
        ..AcpClientConfig::default()
    };
    // Each reply echoes its ~100 KiB method name. The first flood's replies
    // outnumber the outbound queue but fit the client's 16 MiB spool; the
    // second flood's do not.
    let method = format!("future/{}", "x".repeat(100 * 1024));
    let flood = move |first: u64, count: u64| {
        let mut lines = String::new();
        for id in first..first + count {
            lines.push_str(&json!({"jsonrpc": "2.0", "id": id, "method": method}).to_string());
            lines.push('\n');
        }
        lines
    };

    // The reader consumes everything behind the flood and sees the agent end.
    let (client, mut agent) = connected(config.clone(), None).await;
    let mut ended = client.transport_termination();
    let first = flood(1, 100);
    let writer = tokio::spawn(async move {
        agent.write_raw(first.as_bytes()).await;
        agent.writer.shutdown().await.unwrap();
        agent
    });
    tokio::time::timeout(CONDITION_DEADLINE, ended.wait_for(|ended| *ended))
        .await
        .expect("the reader stalled behind replies the agent never read")
        .unwrap();
    drop(writer);

    // The agent keeps writing and never reads: the spool bound ends it.
    let (client, mut agent) = connected(config, None).await;
    let mut ended = client.transport_termination();
    let writer = tokio::spawn(async move {
        let _ = agent.writer.write_all(flood(1, 200).as_bytes()).await;
        agent
    });
    tokio::time::timeout(CONDITION_DEADLINE, ended.wait_for(|ended| *ended))
        .await
        .expect("replies the agent never read were spooled without bound")
        .unwrap();
    drop(writer);
}
