//! codex-acp 2.x renders tool calls through one renderer (the "AIR tool call
//! contract"): every report may carry `name`, `locations`, `_meta`, terminal
//! and exact-diff `content` beside the fields marsh reads. These are the wire
//! shapes 2.2.2 emits for a command, a file edit and an elicitation fetch, as
//! a standard (non-AIR) client sees them. None may be dropped as malformed.
use marsh_acp::{
    AcpClient, AcpClientConfig, ContentBlock, SessionUpdate, ToolCallStatus, ToolKind,
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
    time::timeout,
};

fn update(update: &Value) -> Value {
    json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"session","update":update}})
}

/// The reports codex-acp 2.2.2 sends for one command, one edit and one fetch.
fn codex_acp_v2_reports() -> Vec<Value> {
    vec![
        update(
            &json!({"sessionUpdate":"tool_call","toolCallId":"call_cmd","name":"exec_command",
        "kind":"execute","title":"Run cargo test","status":"pending",
        "content":[{"type":"terminal","terminalId":"call_cmd"}],
        "rawInput":{"command":["cargo","test"],"cwd":"/work"},
        "_meta":{"terminal_info":{"terminal_id":"call_cmd","cwd":"/work"}}}),
        ),
        update(
            &json!({"sessionUpdate":"tool_call_update","toolCallId":"call_cmd",
        "status":"completed","rawOutput":{"exit_code":0,"stdout":"ok\n"},
        "content":[{"type":"content","content":{"type":"text","text":"ok\n"}}],
        "_meta":{"terminal_exit":{"terminal_id":"call_cmd","exit_code":0}}}),
        ),
        update(
            &json!({"sessionUpdate":"tool_call","toolCallId":"call_edit","name":"apply_patch",
        "kind":"edit","title":"Edit src/lib.rs","status":"in_progress",
        "locations":[{"path":"/work/src/lib.rs"}],
        "content":[{"type":"diff","path":"/work/src/lib.rs","oldText":"a\n",
            "newText":"b\n","_meta":{"kind":"update"}}],
        "rawInput":{"changes":{"/work/src/lib.rs":{"type":"update"}}}}),
        ),
        update(
            &json!({"sessionUpdate":"tool_call_update","toolCallId":"call_edit",
        "status":"failed","rawOutput":{"error":"patch rejected"}}),
        ),
        update(
            &json!({"sessionUpdate":"tool_call","toolCallId":"call_fetch","kind":"fetch",
        "title":"Open https://example.com","status":"pending",
        "rawInput":{"url":"https://example.com"}}),
        ),
    ]
}

#[tokio::test]
async fn codex_acp_v2_tool_call_reports_keep_their_identity_kind_and_status() {
    let (client_io, peer_io) = tokio::io::duplex(64 * 1024);
    let (reader, writer) = tokio::io::split(client_io);
    let (peer_reader, mut peer_writer) = tokio::io::split(peer_io);
    let (release, released) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut request = String::new();
        BufReader::new(peer_reader)
            .read_line(&mut request)
            .await
            .unwrap();
        let id = serde_json::from_str::<Value>(&request).unwrap()["id"].clone();
        let reply = json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}});
        for message in codex_acp_v2_reports().into_iter().chain([reply]) {
            peer_writer
                .write_all(format!("{message}\n").as_bytes())
                .await
                .unwrap();
        }
        let _ = released.await;
    });
    let client = AcpClient::new(reader, writer, AcpClientConfig::default(), None);
    let (updates, mut received) = mpsc::channel(16);
    let reply = timeout(
        Duration::from_secs(30),
        client.prompt("session", vec![ContentBlock::text("hello")], updates),
    )
    .await
    .expect("the terminal reply must arrive");
    assert!(reply.is_ok(), "{reply:?}");
    assert_eq!(
        client.dropped_updates(),
        0,
        "no codex-acp 2.x report may be dropped"
    );

    let mut seen = Vec::new();
    while let Ok(update) = received.try_recv() {
        let (start, update) = match update {
            SessionUpdate::ToolCall { update } => (true, update),
            SessionUpdate::ToolCallUpdate { update } => (false, update),
            other => panic!("unexpected update: {other:?}"),
        };
        seen.push((start, update.tool_call_id, update.kind, update.status));
    }
    assert_eq!(
        seen,
        [
            (
                true,
                "call_cmd".into(),
                Some(ToolKind::Execute),
                Some(ToolCallStatus::Pending)
            ),
            (
                false,
                "call_cmd".into(),
                None,
                Some(ToolCallStatus::Completed)
            ),
            (
                true,
                "call_edit".into(),
                Some(ToolKind::Edit),
                Some(ToolCallStatus::InProgress)
            ),
            (
                false,
                "call_edit".into(),
                None,
                Some(ToolCallStatus::Failed)
            ),
            (
                true,
                "call_fetch".into(),
                Some(ToolKind::Fetch),
                Some(ToolCallStatus::Pending)
            ),
        ]
    );
    let _ = release.send(());
    peer.await.unwrap();
}
