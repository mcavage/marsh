//! Independent hand-written JSON conformance tests for ACP v1 specification.
//!
//! Validates marsh-acp against raw JSON string fixtures matching the official specification
//! at <https://agentclientprotocol.com/protocol/v1/> without relying on any internal test agent.

use marsh_acp::cancel::CancelPhase;
use marsh_acp::client::{AcpClient, AcpClientConfig, PermissionHandler};
use marsh_acp::error::AcpError;
use marsh_acp::protocol::{
    ACP_V1_PROTOCOL_VERSION, ContentBlock, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    JsonRpcResponse, LoadSessionRequest, NewSessionRequest, PlanEntry, PlanEntryPriority,
    PlanEntryStatus, PromptRequest, RequestId, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SessionNotification, SessionUpdate, StopReason,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// Hand-written raw JSON fixtures per official ACP v1 specification
// ---------------------------------------------------------------------------

const SPEC_INITIALIZE_REQUEST_JSON: &str = r#"{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "initialize",
  "params": {
    "protocolVersion": 1,
    "clientCapabilities": {
      "terminal": false
    },
    "clientInfo": {
      "name": "marsh",
      "version": "0.1.0"
    }
  }
}"#;

const SPEC_INITIALIZE_RESPONSE_JSON: &str = r#"{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "protocolVersion": 1,
    "agentCapabilities": {
      "loadSession": true,
      "sessionCapabilities": {
        "resume": {}
      }
    },
    "agentInfo": {
      "name": "official-acp-spec-agent",
      "version": "1.0.0"
    }
  }
}"#;

const SPEC_SESSION_NEW_EMPTY_MCPSERVERS_REQUEST_JSON: &str = r#"{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "session/new",
  "params": {
    "cwd": "/workspace/project",
    "mcpServers": []
  }
}"#;

const SPEC_SESSION_NEW_RESPONSE_JSON: &str = r#"{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "sessionId": "123e4567-e89b-12d3-a456-426614174000"
  }
}"#;

const SPEC_SESSION_LOAD_REQUEST_JSON: &str = r#"{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "session/load",
  "params": {
    "sessionId": "123e4567-e89b-12d3-a456-426614174000",
    "cwd": "/workspace/project",
    "mcpServers": []
  }
}"#;

const SPEC_SESSION_PROMPT_REQUEST_JSON: &str = r#"{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "session/prompt",
  "params": {
    "sessionId": "123e4567-e89b-12d3-a456-426614174000",
    "prompt": [
      {
        "type": "text",
        "text": "Review the test suite"
      },
      {
        "type": "image",
        "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
        "mimeType": "image/png"
      },
      {
        "type": "audio",
        "data": "UklGRiQAAABXQVZFZm10IBAAAAABAAEAQB8AAEAfAAABAAgAZGF0YQAAAAA=",
        "mimeType": "audio/wav"
      },
      {
        "type": "resource_link",
        "name": "spec-doc",
        "uri": "file:///workspace/docs/spec.md",
        "mimeType": "text/markdown"
      }
    ]
  }
}"#;

const SPEC_SESSION_UPDATE_PLAN_JSON: &str = r#"{
  "jsonrpc": "2.0",
  "method": "session/update",
  "params": {
    "sessionId": "123e4567-e89b-12d3-a456-426614174000",
    "update": {
      "sessionUpdate": "plan",
      "entries": [
        {
          "content": "Analyze project requirements",
          "priority": "high",
          "status": "completed"
        },
        {
          "content": "Design system architecture",
          "priority": "medium",
          "status": "in_progress"
        },
        {
          "content": "Implement conformance tests",
          "priority": "low",
          "status": "pending"
        }
      ]
    }
  }
}"#;

const SPEC_REQUEST_PERMISSION_REQUEST_JSON: &str = r#"{
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
      {
        "optionId": "opt_allow_once",
        "name": "Allow this operation once",
        "kind": "allow_once"
      },
      {
        "optionId": "opt_reject_always",
        "name": "Reject and deny always",
        "kind": "reject_always"
      }
    ]
  }
}"#;

const SPEC_REQUEST_PERMISSION_SELECTED_RESPONSE_JSON: &str = r#"{
  "jsonrpc": "2.0",
  "id": 100,
  "result": {
    "outcome": {
      "outcome": "selected",
      "optionId": "opt_allow_once"
    }
  }
}"#;

const SPEC_REQUEST_PERMISSION_CANCELLED_RESPONSE_JSON: &str = r#"{
  "jsonrpc": "2.0",
  "id": 100,
  "result": {
    "outcome": {
      "outcome": "cancelled"
    }
  }
}"#;

const SPEC_SESSION_CANCEL_NOTIFICATION_JSON: &str = r#"{
  "jsonrpc": "2.0",
  "method": "session/cancel",
  "params": {
    "sessionId": "123e4567-e89b-12d3-a456-426614174000"
  }
}"#;

const SPEC_SESSION_PROMPT_CANCELLED_RESPONSE_JSON: &str = r#"{
  "jsonrpc": "2.0",
  "id": 4,
  "result": {
    "stopReason": "cancelled"
  }
}"#;

// ---------------------------------------------------------------------------
// Static JSON deserialization conformance tests
// ---------------------------------------------------------------------------

#[test]
fn test_spec_initialize_request_conformance() {
    let req: JsonRpcRequest = serde_json::from_str(SPEC_INITIALIZE_REQUEST_JSON).unwrap();
    assert_eq!(req.method, "initialize");
    assert_eq!(req.params.unwrap()["protocolVersion"], 1);
}

#[test]
fn test_spec_session_load_request_conformance() {
    let req: JsonRpcRequest = serde_json::from_str(SPEC_SESSION_LOAD_REQUEST_JSON).unwrap();
    assert_eq!(req.method, "session/load");
    let load: LoadSessionRequest = serde_json::from_value(req.params.unwrap()).unwrap();
    assert_eq!(load.session_id, "123e4567-e89b-12d3-a456-426614174000");
    assert!(load.mcp_servers.is_empty());
}

#[test]
fn test_spec_session_cancel_notification_conformance() {
    let notif: JsonRpcNotification =
        serde_json::from_str(SPEC_SESSION_CANCEL_NOTIFICATION_JSON).unwrap();
    assert_eq!(notif.method, "session/cancel");
    assert_eq!(
        notif.params.unwrap()["sessionId"],
        "123e4567-e89b-12d3-a456-426614174000"
    );
}

#[test]
fn test_spec_initialize_response_conformance() {
    let msg: JsonRpcMessage = serde_json::from_str(SPEC_INITIALIZE_RESPONSE_JSON).unwrap();
    match msg {
        JsonRpcMessage::Response(resp) => {
            assert_eq!(resp.id, RequestId::Number(1));
            let result_val = resp.result.expect("result must be present");
            assert_eq!(result_val["protocolVersion"], 1);
            assert_eq!(result_val["agentCapabilities"]["loadSession"], true);
            assert!(result_val["agentCapabilities"]["sessionCapabilities"]["resume"].is_object());
            assert_eq!(result_val["agentInfo"]["name"], "official-acp-spec-agent");
        }
        other => panic!("expected Response, got {other:?}"),
    }
}

#[test]
fn test_spec_session_new_required_empty_mcpservers_wire() {
    let req = NewSessionRequest {
        cwd: "/workspace/project".into(),
        mcp_servers: vec![],
    };
    let val = serde_json::to_value(&req).unwrap();
    assert_eq!(val["cwd"], "/workspace/project");
    assert_eq!(
        val["mcpServers"],
        serde_json::json!([]),
        "ACP v1 wire requires mcpServers to be present as an array even when empty"
    );

    // Verify raw hand-written spec request parses cleanly into NewSessionRequest
    let rpc_req: JsonRpcRequest =
        serde_json::from_str(SPEC_SESSION_NEW_EMPTY_MCPSERVERS_REQUEST_JSON).unwrap();
    let parsed: NewSessionRequest = serde_json::from_value(rpc_req.params.unwrap()).unwrap();
    assert_eq!(parsed.cwd, "/workspace/project");
    assert!(parsed.mcp_servers.is_empty());

    // Verify session/load also preserves mcpServers even when empty
    let load_req = LoadSessionRequest {
        session_id: "s1".into(),
        cwd: "/workspace/project".into(),
        mcp_servers: vec![],
    };
    let load_val = serde_json::to_value(&load_req).unwrap();
    assert_eq!(
        load_val["mcpServers"],
        serde_json::json!([]),
        "ACP v1 wire requires mcpServers in session/load even when empty"
    );
}

#[test]
fn test_spec_prompt_content_blocks_wire_camelcase_and_mime_type() {
    let rpc_req: JsonRpcRequest = serde_json::from_str(SPEC_SESSION_PROMPT_REQUEST_JSON).unwrap();
    let prompt_req: PromptRequest = serde_json::from_value(rpc_req.params.unwrap()).unwrap();

    assert_eq!(
        prompt_req.session_id,
        "123e4567-e89b-12d3-a456-426614174000"
    );
    assert_eq!(prompt_req.prompt.len(), 4);

    // Text block
    match &prompt_req.prompt[0] {
        ContentBlock::Text { text } => assert_eq!(text, "Review the test suite"),
        other => panic!("expected text block, got {other:?}"),
    }

    // Image block with mimeType
    match &prompt_req.prompt[1] {
        ContentBlock::Image { mime_type, .. } => assert_eq!(mime_type, "image/png"),
        other => panic!("expected image block, got {other:?}"),
    }

    // Audio block with mimeType
    match &prompt_req.prompt[2] {
        ContentBlock::Audio { mime_type, .. } => assert_eq!(mime_type, "audio/wav"),
        other => panic!("expected audio block, got {other:?}"),
    }

    // Resource link block with name, uri, mimeType
    match &prompt_req.prompt[3] {
        ContentBlock::ResourceLink {
            name,
            uri,
            mime_type,
            ..
        } => {
            assert_eq!(name, "spec-doc");
            assert_eq!(uri, "file:///workspace/docs/spec.md");
            assert_eq!(mime_type.as_deref(), Some("text/markdown"));
        }
        other => panic!("expected resource link block, got {other:?}"),
    }

    // Re-serialize and verify camelCase field 'mimeType'
    let re_val = serde_json::to_value(&prompt_req.prompt).unwrap();
    assert_eq!(re_val[1]["mimeType"], "image/png");
    assert_eq!(re_val[2]["mimeType"], "audio/wav");
    assert_eq!(re_val[3]["mimeType"], "text/markdown");
    assert_eq!(re_val[3]["name"], "spec-doc");
}

#[test]
fn test_spec_plan_entries_session_update_conformance() {
    let rpc_notif: JsonRpcNotification =
        serde_json::from_str(SPEC_SESSION_UPDATE_PLAN_JSON).unwrap();
    let session_notif: SessionNotification =
        serde_json::from_value(rpc_notif.params.unwrap()).unwrap();

    assert_eq!(
        session_notif.session_id,
        "123e4567-e89b-12d3-a456-426614174000"
    );
    match session_notif.update {
        SessionUpdate::Plan { entries } => {
            assert_eq!(entries.len(), 3);
            assert_eq!(entries[0].content, "Analyze project requirements");
            assert_eq!(entries[0].priority, PlanEntryPriority::High);
            assert_eq!(entries[0].status, PlanEntryStatus::Completed);

            assert_eq!(entries[1].content, "Design system architecture");
            assert_eq!(entries[1].priority, PlanEntryPriority::Medium);
            assert_eq!(entries[1].status, PlanEntryStatus::InProgress);

            assert_eq!(entries[2].content, "Implement conformance tests");
            assert_eq!(entries[2].priority, PlanEntryPriority::Low);
            assert_eq!(entries[2].status, PlanEntryStatus::Pending);
        }
        other => panic!("expected Plan sessionUpdate, got {other:?}"),
    }

    // Re-serialize and verify wire shape
    let plan_update = SessionUpdate::Plan {
        entries: vec![PlanEntry::new(
            "Test",
            PlanEntryPriority::High,
            PlanEntryStatus::InProgress,
        )],
    };
    let plan_val = serde_json::to_value(&plan_update).unwrap();
    assert_eq!(plan_val["sessionUpdate"], "plan");
    assert!(plan_val["entries"].is_array());
    assert_eq!(plan_val["entries"][0]["priority"], "high");
    assert_eq!(plan_val["entries"][0]["status"], "in_progress");
}

#[test]
fn test_spec_permission_request_and_response_optionid_wire() {
    let req: JsonRpcRequest = serde_json::from_str(SPEC_REQUEST_PERMISSION_REQUEST_JSON).unwrap();
    let perm_req: RequestPermissionRequest = serde_json::from_value(req.params.unwrap()).unwrap();

    assert_eq!(perm_req.options.len(), 2);
    assert_eq!(perm_req.options[0].option_id, "opt_allow_once");
    assert_eq!(perm_req.options[0].name, "Allow this operation once");
    assert_eq!(perm_req.options[0].kind, "allow_once");

    // Test Selected Response serialization and deserialization
    let resp_sel: RequestPermissionResponse = serde_json::from_value(
        serde_json::from_str::<JsonRpcResponse>(SPEC_REQUEST_PERMISSION_SELECTED_RESPONSE_JSON)
            .unwrap()
            .result
            .unwrap(),
    )
    .unwrap();
    match resp_sel.outcome {
        RequestPermissionOutcome::Selected { option_id } => {
            assert_eq!(option_id, "opt_allow_once");
        }
        RequestPermissionOutcome::Cancelled => panic!("expected Selected, got Cancelled"),
    }

    // Test Cancelled Response serialization and deserialization
    let resp_can: RequestPermissionResponse = serde_json::from_value(
        serde_json::from_str::<JsonRpcResponse>(SPEC_REQUEST_PERMISSION_CANCELLED_RESPONSE_JSON)
            .unwrap()
            .result
            .unwrap(),
    )
    .unwrap();
    match resp_can.outcome {
        RequestPermissionOutcome::Cancelled => {}
        RequestPermissionOutcome::Selected { .. } => panic!("expected Cancelled, got Selected"),
    }
}

// ---------------------------------------------------------------------------
// End-to-end in-memory duplex test against strict hand-written JSON validator
// ---------------------------------------------------------------------------

struct SelectiveApprovalHandler;
impl PermissionHandler for SelectiveApprovalHandler {
    fn handle_permission(
        &self,
        req: RequestPermissionRequest,
    ) -> Pin<Box<dyn Future<Output = RequestPermissionOutcome> + Send>> {
        Box::pin(async move {
            if let Some(opt) = req.options.iter().find(|o| o.option_id == "opt_allow_once") {
                RequestPermissionOutcome::select(opt.option_id.clone())
            } else {
                RequestPermissionOutcome::cancel()
            }
        })
    }
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn test_duplex_handshake_session_plan_and_cancellation_e2e() {
    let (client_io, mut mock_agent_io) = tokio::io::duplex(64 * 1024);
    let (client_r, client_w) = tokio::io::split(client_io);

    let client = AcpClient::new(
        client_r,
        client_w,
        AcpClientConfig::default(),
        Some(Arc::new(SelectiveApprovalHandler)),
    );

    // Spawn mock spec-evaluator on mock_agent_io
    let mock_task = tokio::spawn(async move {
        let (read_half, mut write_half) = tokio::io::split(&mut mock_agent_io);
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        // 1. Expect initialize request
        reader.read_line(&mut line).await.unwrap();
        let init_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(init_req["method"], "initialize");
        assert_eq!(init_req["params"]["protocolVersion"], 1);
        let init_id = init_req["id"].clone();
        line.clear();

        // Send SPEC_INITIALIZE_RESPONSE
        let mut init_resp: serde_json::Value =
            serde_json::from_str(SPEC_INITIALIZE_RESPONSE_JSON).unwrap();
        init_resp["id"] = init_id;
        write_half
            .write_all(format!("{}\n", serde_json::to_string(&init_resp).unwrap()).as_bytes())
            .await
            .unwrap();

        // 2. Expect session/new request and verify "mcpServers": [] is present on wire
        reader.read_line(&mut line).await.unwrap();
        let new_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(new_req["method"], "session/new");
        assert!(
            new_req["params"].get("mcpServers").is_some(),
            "mcpServers must be present on wire even when empty"
        );
        assert_eq!(new_req["params"]["mcpServers"], serde_json::json!([]));
        let new_id = new_req["id"].clone();
        line.clear();

        // Send SPEC_SESSION_NEW_RESPONSE
        let mut new_resp: serde_json::Value =
            serde_json::from_str(SPEC_SESSION_NEW_RESPONSE_JSON).unwrap();
        new_resp["id"] = new_id;
        write_half
            .write_all(format!("{}\n", serde_json::to_string(&new_resp).unwrap()).as_bytes())
            .await
            .unwrap();

        // 3. Expect session/prompt request and verify ContentBlocks and mimeType
        reader.read_line(&mut line).await.unwrap();
        let prompt_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(prompt_req["method"], "session/prompt");
        let prompt_id = prompt_req["id"].clone();
        line.clear();

        // 4. Stream SPEC_SESSION_UPDATE_PLAN as a single line
        let plan_compact: serde_json::Value =
            serde_json::from_str(SPEC_SESSION_UPDATE_PLAN_JSON).unwrap();
        write_half
            .write_all(format!("{}\n", serde_json::to_string(&plan_compact).unwrap()).as_bytes())
            .await
            .unwrap();
        write_half.flush().await.unwrap();

        // 5. Send permission request to client as a single line
        let perm_compact: serde_json::Value =
            serde_json::from_str(SPEC_REQUEST_PERMISSION_REQUEST_JSON).unwrap();
        write_half
            .write_all(format!("{}\n", serde_json::to_string(&perm_compact).unwrap()).as_bytes())
            .await
            .unwrap();
        write_half.flush().await.unwrap();

        // 6. Expect permission response from client
        reader.read_line(&mut line).await.unwrap();
        let perm_resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(perm_resp["id"], 100);
        assert_eq!(perm_resp["result"]["outcome"]["outcome"], "selected");
        assert_eq!(perm_resp["result"]["outcome"]["optionId"], "opt_allow_once");
        line.clear();

        // 7. Client will issue cancel notification: wait for it
        reader.read_line(&mut line).await.unwrap();
        let cancel_notif: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(cancel_notif["method"], "session/cancel");
        assert_eq!(
            cancel_notif["params"]["sessionId"],
            "123e4567-e89b-12d3-a456-426614174000"
        );
        line.clear();

        // Give client time to assert Dispatched before sending Confirmed response
        tokio::time::sleep(Duration::from_millis(50)).await;

        // 8. Respond with prompt cancelled
        let mut cancel_resp: serde_json::Value =
            serde_json::from_str(SPEC_SESSION_PROMPT_CANCELLED_RESPONSE_JSON).unwrap();
        cancel_resp["id"] = prompt_id;
        write_half
            .write_all(format!("{}\n", serde_json::to_string(&cancel_resp).unwrap()).as_bytes())
            .await
            .unwrap();
    });

    // Run client side operations
    let init_resp = client.initialize(None).await.expect("initialize");
    assert_eq!(init_resp.protocol_version, ACP_V1_PROTOCOL_VERSION);

    let session_id = client
        .new_session("/workspace/project", vec![])
        .await
        .expect("new_session");
    assert_eq!(session_id, "123e4567-e89b-12d3-a456-426614174000");

    let (update_tx, mut update_rx) = mpsc::channel(64);
    let client_arc = Arc::new(client);
    let client_clone = Arc::clone(&client_arc);
    let s1 = session_id.clone();

    let prompt_handle = tokio::spawn(async move {
        client_clone
            .prompt(
                &s1,
                vec![
                    ContentBlock::text("Review the test suite"),
                    ContentBlock::image("imgdata", "image/png"),
                    ContentBlock::audio("audiodata", "audio/wav"),
                    ContentBlock::resource_link("spec-doc", "file:///workspace/docs/spec.md"),
                ],
                update_tx,
            )
            .await
    });

    // Verify streamed Plan update arrived
    let plan_update = update_rx.recv().await.expect("plan update arrived");
    match plan_update {
        SessionUpdate::Plan { entries } => {
            assert_eq!(entries.len(), 3);
            assert_eq!(entries[0].content, "Analyze project requirements");
            assert_eq!(entries[0].priority, PlanEntryPriority::High);
        }
        other => panic!("expected Plan update, got {other:?}"),
    }

    // Dispatch cancellation
    tokio::time::sleep(Duration::from_millis(50)).await;
    let cancel_phase = client_arc
        .cancel_prompt(&session_id)
        .await
        .expect("cancel prompt");
    assert_eq!(cancel_phase, CancelPhase::Dispatched);

    let prompt_resp = prompt_handle
        .await
        .unwrap()
        .expect("prompt turn completes with cancellation");
    assert_eq!(prompt_resp.stop_reason, StopReason::Cancelled);

    mock_task.await.unwrap();
}

#[tokio::test]
async fn test_cancellation_does_not_drop_partial_frame() {
    // Tests that sending a cancel notification while a multi-packet frame is in-flight
    // does NOT drop the pending frame or corrupt the transport framing.
    let (client_io, mut mock_agent_io) = tokio::io::duplex(64 * 1024);
    let (client_r, client_w) = tokio::io::split(client_io);

    let client = Arc::new(AcpClient::new(
        client_r,
        client_w,
        AcpClientConfig::default(),
        None,
    ));

    let mock_task = tokio::spawn(async move {
        let (read_half, mut write_half) = tokio::io::split(&mut mock_agent_io);
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        // 1. Initialize
        reader.read_line(&mut line).await.unwrap();
        let init_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let init_id = init_req["id"].clone();
        line.clear();
        let init_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": init_id,
            "result": { "protocolVersion": 1 }
        });
        write_half
            .write_all(format!("{init_resp}\n").as_bytes())
            .await
            .unwrap();

        // 2. session/new
        reader.read_line(&mut line).await.unwrap();
        let new_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let new_id = new_req["id"].clone();
        line.clear();
        let new_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": new_id,
            "result": { "sessionId": "sess-partial" }
        });
        write_half
            .write_all(format!("{new_resp}\n").as_bytes())
            .await
            .unwrap();

        // 3. session/prompt
        reader.read_line(&mut line).await.unwrap();
        let prompt_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let prompt_id = prompt_req["id"].clone();
        line.clear();

        // Write PARTIAL frame of an update notification
        let partial1 =
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-partial","#;
        write_half.write_all(partial1.as_bytes()).await.unwrap();
        write_half.flush().await.unwrap();

        // Give the client reader time to read partial1 into its buffer
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Now read the cancel notification sent by client during partial frame
        reader.read_line(&mut line).await.unwrap();
        let cancel_notif: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(cancel_notif["method"], "session/cancel");
        line.clear();

        // Now finish writing the rest of the update notification frame!
        let partial2 = r#""update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"preserved chunk"}}}}"#;
        write_half
            .write_all(format!("{partial2}\n").as_bytes())
            .await
            .unwrap();
        write_half.flush().await.unwrap();

        // Finally send cancelled prompt response
        let cancel_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": prompt_id,
            "result": { "stopReason": "cancelled" }
        });
        write_half
            .write_all(format!("{cancel_resp}\n").as_bytes())
            .await
            .unwrap();
    });

    client.initialize(None).await.expect("init");
    let session_id = client.new_session("/tmp", vec![]).await.expect("new");

    let (tx, mut rx) = mpsc::channel(64);
    let c = Arc::clone(&client);
    let s = session_id.clone();
    let prompt_task =
        tokio::spawn(async move { c.prompt(&s, vec![ContentBlock::text("test")], tx).await });

    // Trigger cancel while partial frame is in flight
    tokio::time::sleep(Duration::from_millis(20)).await;
    let cancel_res = client.cancel_prompt(&session_id).await;
    assert!(cancel_res.is_ok());

    let prompt_res = prompt_task.await.unwrap().expect("prompt result");
    assert_eq!(prompt_res.stop_reason, StopReason::Cancelled);

    // The update whose frame was split across the cancel MUST arrive uncorrupted!
    let update = rx.recv().await.expect("update arrived without corruption");
    match update {
        SessionUpdate::AgentMessageChunk { content } => {
            assert_eq!(content, ContentBlock::text("preserved chunk"));
        }
        other => panic!("expected preserved message chunk, got {other:?}"),
    }

    mock_task.await.unwrap();
}

struct SlowMockPermissionHandler {
    delay: Duration,
}

impl PermissionHandler for SlowMockPermissionHandler {
    fn handle_permission(
        &self,
        _request: RequestPermissionRequest,
    ) -> Pin<Box<dyn Future<Output = RequestPermissionOutcome> + Send>> {
        let delay = self.delay;
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            RequestPermissionOutcome::select("opt_allowed")
        })
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_wire_cancelled_permission_on_client_cancel() {
    let (client_io, mut mock_agent_io) = tokio::io::duplex(64 * 1024);
    let (client_r, client_w) = tokio::io::split(client_io);

    let handler = Arc::new(SlowMockPermissionHandler {
        delay: Duration::from_secs(5),
    });

    let client = Arc::new(AcpClient::new(
        client_r,
        client_w,
        AcpClientConfig::default(),
        Some(handler),
    ));

    let mock_task = tokio::spawn(async move {
        let (read_half, mut write_half) = tokio::io::split(&mut mock_agent_io);
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        // 1. Initialize
        reader.read_line(&mut line).await.unwrap();
        let init_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let init_id = init_req["id"].clone();
        line.clear();
        let init_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": init_id,
            "result": { "protocolVersion": 1 }
        });
        write_half
            .write_all(format!("{init_resp}\n").as_bytes())
            .await
            .unwrap();

        // 2. session/new
        reader.read_line(&mut line).await.unwrap();
        let new_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let new_id = new_req["id"].clone();
        line.clear();
        let new_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": new_id,
            "result": { "sessionId": "s-perm-cancel" }
        });
        write_half
            .write_all(format!("{new_resp}\n").as_bytes())
            .await
            .unwrap();

        // 3. session/prompt
        reader.read_line(&mut line).await.unwrap();
        let prompt_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let prompt_id = prompt_req["id"].clone();
        line.clear();

        // 4. Agent sends advisory permission request to client
        let perm_req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 501,
            "method": "session/request_permission",
            "params": {
                "sessionId": "s-perm-cancel",
                "toolCall": { "toolCallId": "tc-slow" },
                "options": [
                    { "optionId": "opt-1", "name": "Allow", "kind": "allow_once" }
                ]
            }
        });
        write_half
            .write_all(format!("{perm_req}\n").as_bytes())
            .await
            .unwrap();

        // 5. Read incoming messages from client:
        // Client should send cancel notification AND permission response answered "cancelled"
        let mut saw_cancel_notif = false;
        let mut saw_perm_cancelled_response = false;

        for _ in 0..2 {
            reader.read_line(&mut line).await.unwrap();
            let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            line.clear();

            if parsed.get("method").and_then(|m| m.as_str()) == Some("session/cancel") {
                saw_cancel_notif = true;
            } else if parsed.get("id").and_then(serde_json::Value::as_i64) == Some(501) {
                assert_eq!(
                    parsed["result"]["outcome"]["outcome"], "cancelled",
                    "permission response must be cancelled on cancel"
                );
                saw_perm_cancelled_response = true;
            }
        }

        assert!(saw_cancel_notif, "expected session/cancel notification");
        assert!(
            saw_perm_cancelled_response,
            "expected permission response with outcome cancelled"
        );

        // 6. Complete prompt with cancelled response
        let prompt_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": prompt_id,
            "result": { "stopReason": "cancelled" }
        });
        write_half
            .write_all(format!("{prompt_resp}\n").as_bytes())
            .await
            .unwrap();
    });

    client.initialize(None).await.expect("init");
    let session_id = client.new_session("/tmp", vec![]).await.expect("new");

    let (tx, _rx) = mpsc::channel(64);
    let c = Arc::clone(&client);
    let s = session_id.clone();
    let prompt_task =
        tokio::spawn(async move { c.prompt(&s, vec![ContentBlock::text("test")], tx).await });

    // Give agent time to send permission request
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Client requests cancellation while permission is pending
    let cancel_res = client.cancel_prompt(&session_id).await;
    assert!(cancel_res.is_ok());

    let prompt_res = prompt_task.await.unwrap().expect("prompt result");
    assert_eq!(prompt_res.stop_reason, StopReason::Cancelled);

    mock_task.await.unwrap();
}

#[tokio::test]
async fn test_wire_cancelled_permission_on_caller_drop() {
    let (client_io, mut mock_agent_io) = tokio::io::duplex(64 * 1024);
    let (client_r, client_w) = tokio::io::split(client_io);

    let handler = Arc::new(SlowMockPermissionHandler {
        delay: Duration::from_secs(5),
    });

    let client = Arc::new(AcpClient::new(
        client_r,
        client_w,
        AcpClientConfig::default(),
        Some(handler),
    ));

    let mock_task = tokio::spawn(async move {
        let (read_half, mut write_half) = tokio::io::split(&mut mock_agent_io);
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        // 1. Initialize
        reader.read_line(&mut line).await.unwrap();
        let init_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let init_id = init_req["id"].clone();
        line.clear();
        let init_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": init_id,
            "result": { "protocolVersion": 1 }
        });
        write_half
            .write_all(format!("{init_resp}\n").as_bytes())
            .await
            .unwrap();

        // 2. session/new
        reader.read_line(&mut line).await.unwrap();
        let new_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let new_id = new_req["id"].clone();
        line.clear();
        let new_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": new_id,
            "result": { "sessionId": "s-drop-perm" }
        });
        write_half
            .write_all(format!("{new_resp}\n").as_bytes())
            .await
            .unwrap();

        // 3. session/prompt
        reader.read_line(&mut line).await.unwrap();
        line.clear();

        // 4. Agent sends advisory permission request to client
        let perm_req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 601,
            "method": "session/request_permission",
            "params": {
                "sessionId": "s-drop-perm",
                "toolCall": { "toolCallId": "tc-drop" },
                "options": [
                    { "optionId": "opt-1", "name": "Allow", "kind": "allow_once" }
                ]
            }
        });
        write_half
            .write_all(format!("{perm_req}\n").as_bytes())
            .await
            .unwrap();

        // 5. Read messages resulting from TurnGuard drop
        let mut saw_cancel_notif = false;
        let mut saw_perm_cancelled_response = false;

        for _ in 0..2 {
            reader.read_line(&mut line).await.unwrap();
            let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            line.clear();

            if parsed.get("method").and_then(|m| m.as_str()) == Some("session/cancel") {
                saw_cancel_notif = true;
            } else if parsed.get("id").and_then(serde_json::Value::as_i64) == Some(601) {
                assert_eq!(
                    parsed["result"]["outcome"]["outcome"], "cancelled",
                    "permission response must be cancelled on caller drop"
                );
                saw_perm_cancelled_response = true;
            }
        }

        assert!(saw_cancel_notif, "expected cancel notification on drop");
        assert!(
            saw_perm_cancelled_response,
            "expected permission response cancelled on drop"
        );
    });

    client.initialize(None).await.expect("init");
    let session_id = client.new_session("/tmp", vec![]).await.expect("new");

    let (tx, _rx) = mpsc::channel(64);
    let c = Arc::clone(&client);
    let s = session_id.clone();
    let prompt_task =
        tokio::spawn(async move { c.prompt(&s, vec![ContentBlock::text("test")], tx).await });

    // Give agent time to send permission request
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Caller drops prompt turn
    prompt_task.abort();

    mock_task.await.unwrap();
}

#[tokio::test]
async fn test_wire_cancelled_permission_on_handler_timeout() {
    let (client_io, mut mock_agent_io) = tokio::io::duplex(64 * 1024);
    let (client_r, client_w) = tokio::io::split(client_io);

    let handler = Arc::new(SlowMockPermissionHandler {
        delay: Duration::from_secs(5),
    });

    let config = AcpClientConfig {
        permission_timeout: Duration::from_millis(80),
        ..Default::default()
    };

    let client = Arc::new(AcpClient::new(client_r, client_w, config, Some(handler)));

    let mock_task = tokio::spawn(async move {
        let (read_half, mut write_half) = tokio::io::split(&mut mock_agent_io);
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        // 1. Initialize
        reader.read_line(&mut line).await.unwrap();
        let init_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let init_id = init_req["id"].clone();
        line.clear();
        let init_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": init_id,
            "result": { "protocolVersion": 1 }
        });
        write_half
            .write_all(format!("{init_resp}\n").as_bytes())
            .await
            .unwrap();

        // 2. session/new
        reader.read_line(&mut line).await.unwrap();
        let new_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let new_id = new_req["id"].clone();
        line.clear();
        let new_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": new_id,
            "result": { "sessionId": "s-timeout-perm" }
        });
        write_half
            .write_all(format!("{new_resp}\n").as_bytes())
            .await
            .unwrap();

        // 3. session/prompt
        reader.read_line(&mut line).await.unwrap();
        let prompt_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let prompt_id = prompt_req["id"].clone();
        line.clear();

        // 4. Agent sends advisory permission request to client
        let perm_req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 701,
            "method": "session/request_permission",
            "params": {
                "sessionId": "s-timeout-perm",
                "toolCall": { "toolCallId": "tc-timeout" },
                "options": [
                    { "optionId": "opt-1", "name": "Allow", "kind": "allow_once" }
                ]
            }
        });
        write_half
            .write_all(format!("{perm_req}\n").as_bytes())
            .await
            .unwrap();

        // 5. Expect permission response answered "cancelled" within bounded timeout (not 5s!)
        let start = std::time::Instant::now();
        reader.read_line(&mut line).await.unwrap();
        let elapsed = start.elapsed();
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        line.clear();

        assert_eq!(parsed["id"], 701);
        assert_eq!(
            parsed["result"]["outcome"]["outcome"], "cancelled",
            "must answer cancelled on timeout"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "timeout must trigger quickly (took {elapsed:?})"
        );

        // 6. Complete prompt turn normally
        let prompt_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": prompt_id,
            "result": { "stopReason": "end_turn" }
        });
        write_half
            .write_all(format!("{prompt_resp}\n").as_bytes())
            .await
            .unwrap();
    });

    client.initialize(None).await.expect("init");
    let session_id = client.new_session("/tmp", vec![]).await.expect("new");

    let (tx, _rx) = mpsc::channel(64);
    let resp = client
        .prompt(&session_id, vec![ContentBlock::text("test")], tx)
        .await
        .expect("prompt completes");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);

    mock_task.await.unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_wire_normal_end_cancel_race() {
    let (client_io, mut mock_agent_io) = tokio::io::duplex(64 * 1024);
    let (client_r, client_w) = tokio::io::split(client_io);

    let client = Arc::new(AcpClient::new(
        client_r,
        client_w,
        AcpClientConfig::default(),
        None,
    ));

    let mock_task = tokio::spawn(async move {
        let (read_half, mut write_half) = tokio::io::split(&mut mock_agent_io);
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        // 1. Initialize
        reader.read_line(&mut line).await.unwrap();
        let init_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let init_id = init_req["id"].clone();
        line.clear();
        let init_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": init_id,
            "result": { "protocolVersion": 1 }
        });
        write_half
            .write_all(format!("{init_resp}\n").as_bytes())
            .await
            .unwrap();

        // 2. session/new
        reader.read_line(&mut line).await.unwrap();
        let new_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let new_id = new_req["id"].clone();
        line.clear();
        let new_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": new_id,
            "result": { "sessionId": "s-race" }
        });
        write_half
            .write_all(format!("{new_resp}\n").as_bytes())
            .await
            .unwrap();

        // --- Turn 1: Race - Cancel was dispatched, but agent returns endTurn ---
        reader.read_line(&mut line).await.unwrap();
        let prompt_req1: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let prompt_id1 = prompt_req1["id"].clone();
        line.clear();

        // Read cancel notification sent by client
        reader.read_line(&mut line).await.unwrap();
        let notif: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(notif["method"], "session/cancel");
        line.clear();

        // Agent races ahead and returns end_turn instead of cancelled!
        let end_turn_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": prompt_id1,
            "result": { "stopReason": "end_turn" }
        });
        write_half
            .write_all(format!("{end_turn_resp}\n").as_bytes())
            .await
            .unwrap();

        // --- Turn 2: Verified cancel via stopReason == cancelled ---
        reader.read_line(&mut line).await.unwrap();
        let prompt_req2: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let prompt_id2 = prompt_req2["id"].clone();
        line.clear();

        reader.read_line(&mut line).await.unwrap();
        let notif2: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(notif2["method"], "session/cancel");
        line.clear();

        let cancel_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": prompt_id2,
            "result": { "stopReason": "cancelled" }
        });
        write_half
            .write_all(format!("{cancel_resp}\n").as_bytes())
            .await
            .unwrap();

        // --- Turn 3: Verified cancel with error code -32800 ---
        reader.read_line(&mut line).await.unwrap();
        let prompt_req3: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let prompt_id3 = prompt_req3["id"].clone();
        line.clear();

        reader.read_line(&mut line).await.unwrap();
        let notif3: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(notif3["method"], "session/cancel");
        line.clear();

        let err_cancel_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": prompt_id3,
            "error": {
                "code": -32800,
                "message": "Request cancelled by client"
            }
        });
        write_half
            .write_all(format!("{err_cancel_resp}\n").as_bytes())
            .await
            .unwrap();

        // --- Turn 4: Unsolicited stopReason == cancelled (client never calls cancel_prompt) ---
        reader.read_line(&mut line).await.unwrap();
        let prompt_req4: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let prompt_id4 = prompt_req4["id"].clone();
        line.clear();

        let unsolicited_cancel_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": prompt_id4,
            "result": { "stopReason": "cancelled" }
        });
        write_half
            .write_all(format!("{unsolicited_cancel_resp}\n").as_bytes())
            .await
            .unwrap();

        // --- Turn 5: Unsolicited error -32800 (client never calls cancel_prompt) ---
        reader.read_line(&mut line).await.unwrap();
        let prompt_req5: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let prompt_id5 = prompt_req5["id"].clone();
        line.clear();

        let unsolicited_err_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": prompt_id5,
            "error": {
                "code": -32800,
                "message": "Unsolicited cancel error"
            }
        });
        write_half
            .write_all(format!("{unsolicited_err_resp}\n").as_bytes())
            .await
            .unwrap();
    });

    client.initialize(None).await.expect("init");
    let session_id = client.new_session("/tmp", vec![]).await.expect("new");

    // Turn 1: Client cancels, but agent completes normally with endTurn
    {
        let (tx1, _rx1) = mpsc::channel(64);
        let c1 = Arc::clone(&client);
        let s1 = session_id.clone();
        let task1 =
            tokio::spawn(async move { c1.prompt(&s1, vec![ContentBlock::text("t1")], tx1).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let phase = client.cancel_prompt(&session_id).await.expect("cancel");
        assert_eq!(phase, CancelPhase::Dispatched);

        let resp1 = task1.await.unwrap().expect("turn 1 completes");
        // Must reflect EndTurn, NOT Cancelled!
        assert_eq!(resp1.stop_reason, StopReason::EndTurn);
        assert_eq!(
            client.session_cancel_phase(&session_id).await,
            Some(CancelPhase::Dispatched)
        );
    }

    // Turn 2: Client cancels, agent confirms with stopReason: cancelled
    {
        let (tx2, _rx2) = mpsc::channel(64);
        let c2 = Arc::clone(&client);
        let s2 = session_id.clone();
        let task2 =
            tokio::spawn(async move { c2.prompt(&s2, vec![ContentBlock::text("t2")], tx2).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let phase = client.cancel_prompt(&session_id).await.expect("cancel");
        assert!(
            phase == CancelPhase::Dispatched || phase == CancelPhase::Confirmed,
            "phase must be dispatched or confirmed"
        );

        let resp2 = task2.await.unwrap().expect("turn 2 completes");
        assert_eq!(resp2.stop_reason, StopReason::Cancelled);
        assert_eq!(
            client.session_cancel_phase(&session_id).await,
            Some(CancelPhase::Confirmed)
        );
    }

    // Turn 3: Client cancels, agent responds with error code -32800.
    // Review finding B1: error -32800 must remain error not cancel confirmation!
    {
        let (tx3, _rx3) = mpsc::channel(64);
        let c3 = Arc::clone(&client);
        let s3 = session_id.clone();
        let task3 =
            tokio::spawn(async move { c3.prompt(&s3, vec![ContentBlock::text("t3")], tx3).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        let phase = client.cancel_prompt(&session_id).await.expect("cancel");
        assert!(
            phase == CancelPhase::Dispatched || phase == CancelPhase::Confirmed,
            "phase must be dispatched or confirmed"
        );

        let err3 = task3
            .await
            .unwrap()
            .expect_err("error -32800 must remain error not cancel confirmation");
        match err3 {
            AcpError::JsonRpc { code, message, .. } => {
                assert_eq!(code, -32800);
                assert_eq!(message, "Request cancelled by client");
            }
            other => panic!("expected JsonRpc error -32800, got {other:?}"),
        }
        // Cancel was dispatched, but agent returned an error, NOT stopReason: cancelled.
        // Therefore, cancel is NOT confirmed!
        assert_eq!(
            client.session_cancel_phase(&session_id).await,
            Some(CancelPhase::Dispatched)
        );
    }

    // Turn 4: Unsolicited stopReason: cancelled (client never requested cancel).
    // Review finding B1: unsolicited cancelled not confirmed!
    {
        let (tx4, _rx4) = mpsc::channel(64);
        let c4 = Arc::clone(&client);
        let s4 = session_id.clone();
        let task4 =
            tokio::spawn(async move { c4.prompt(&s4, vec![ContentBlock::text("t4")], tx4).await });

        let resp4 = task4.await.unwrap().expect("turn 4 completes");
        assert_eq!(resp4.stop_reason, StopReason::Cancelled);
        // Unsolicited cancelled is NOT confirmed; remains NotCancelled!
        assert_eq!(
            client.session_cancel_phase(&session_id).await,
            Some(CancelPhase::NotCancelled)
        );
    }

    // Turn 5: Unsolicited error -32800 (client never requested cancel).
    // Review finding B1: error -32800 remains error, and cancel is NOT confirmed!
    {
        let (tx5, _rx5) = mpsc::channel(64);
        let c5 = Arc::clone(&client);
        let s5 = session_id.clone();
        let task5 =
            tokio::spawn(async move { c5.prompt(&s5, vec![ContentBlock::text("t5")], tx5).await });

        let err5 = task5
            .await
            .unwrap()
            .expect_err("unsolicited error -32800 remains error");
        match err5 {
            AcpError::JsonRpc { code, message, .. } => {
                assert_eq!(code, -32800);
                assert_eq!(message, "Unsolicited cancel error");
            }
            other => panic!("expected JsonRpc error -32800, got {other:?}"),
        }
        assert_eq!(
            client.session_cancel_phase(&session_id).await,
            Some(CancelPhase::NotCancelled)
        );
    }

    mock_task.await.unwrap();
}

#[tokio::test]
async fn test_wire_skips_blank_lines() {
    let (client_io, mut mock_agent_io) = tokio::io::duplex(64 * 1024);
    let (client_r, client_w) = tokio::io::split(client_io);

    let client = Arc::new(AcpClient::new(
        client_r,
        client_w,
        AcpClientConfig::default(),
        None,
    ));

    let mock_task = tokio::spawn(async move {
        let (read_half, mut write_half) = tokio::io::split(&mut mock_agent_io);
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        // 1. Initialize request from client
        reader.read_line(&mut line).await.unwrap();
        let init_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let init_id = init_req["id"].clone();
        line.clear();

        // Send multiple blank lines before the response!
        write_half.write_all(b"\n\r\n   \n\n").await.unwrap();
        let init_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": init_id,
            "result": { "protocolVersion": 1 }
        });
        write_half
            .write_all(format!("{init_resp}\n").as_bytes())
            .await
            .unwrap();

        // 2. session/new request from client
        reader.read_line(&mut line).await.unwrap();
        let new_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let new_id = new_req["id"].clone();
        line.clear();

        // Send blank lines before new_resp
        write_half.write_all(b"\n\n").await.unwrap();
        let new_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": new_id,
            "result": { "sessionId": "s-blank-test" }
        });
        write_half
            .write_all(format!("{new_resp}\n").as_bytes())
            .await
            .unwrap();

        // 3. session/prompt request from client
        reader.read_line(&mut line).await.unwrap();
        let prompt_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let prompt_id = prompt_req["id"].clone();
        line.clear();

        // Send blank lines between update notifications
        write_half.write_all(b"\r\n\n").await.unwrap();
        let update_msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s-blank-test",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {
                        "type": "text",
                        "text": "blank lines skipped successfully"
                    }
                }
            }
        });
        write_half
            .write_all(format!("{update_msg}\n").as_bytes())
            .await
            .unwrap();
        write_half.write_all(b"  \n\n").await.unwrap();

        let prompt_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": prompt_id,
            "result": { "stopReason": "end_turn" }
        });
        write_half
            .write_all(format!("{prompt_resp}\n").as_bytes())
            .await
            .unwrap();
    });

    client
        .initialize(None)
        .await
        .expect("init with blank lines");
    let session_id = client
        .new_session("/tmp", vec![])
        .await
        .expect("new with blank lines");

    let (tx, mut rx) = mpsc::channel(64);
    let prompt_resp = client
        .prompt(&session_id, vec![ContentBlock::text("test")], tx)
        .await
        .expect("prompt with blank lines");
    assert_eq!(prompt_resp.stop_reason, StopReason::EndTurn);

    let update = rx.recv().await.expect("received update");
    match update {
        SessionUpdate::AgentMessageChunk { content } => {
            assert_eq!(
                content,
                ContentBlock::text("blank lines skipped successfully")
            );
        }
        other => panic!("expected text update, got {other:?}"),
    }

    mock_task.await.unwrap();
}

#[tokio::test]
async fn test_wire_governed_io_and_host_spawn_api_absence() {
    // 1. REAL WIRE TEST: Pure governed in-memory IO.
    // Client only operates on streams provided by host environment (e.g. duplex pipe/kit worker).
    let (client_io, mut mock_agent_io) = tokio::io::duplex(64 * 1024);
    let (client_r, client_w) = tokio::io::split(client_io);

    let client = Arc::new(AcpClient::new(
        client_r,
        client_w,
        AcpClientConfig::default(),
        None,
    ));

    let mock_task = tokio::spawn(async move {
        let (read_half, mut write_half) = tokio::io::split(&mut mock_agent_io);
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        // 1. Initialize
        reader.read_line(&mut line).await.unwrap();
        let init_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let init_id = init_req["id"].clone();
        line.clear();
        let init_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": init_id,
            "result": { "protocolVersion": 1 }
        });
        write_half
            .write_all(format!("{init_resp}\n").as_bytes())
            .await
            .unwrap();

        // 2. Agent attempts terminal/create over wire
        let term_req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 901,
            "method": "terminal/create",
            "params": { "command": "bash" }
        });
        write_half
            .write_all(format!("{term_req}\n").as_bytes())
            .await
            .unwrap();

        // Client must immediately reject terminal/create
        reader.read_line(&mut line).await.unwrap();
        let term_resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        line.clear();
        assert_eq!(term_resp["id"], 901);
        assert_eq!(
            term_resp["error"]["code"], -32601,
            "terminal/create must be rejected with MethodNotFound"
        );

        // 3. Agent attempts fs/write_text_file over wire
        let fs_req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 902,
            "method": "fs/write_text_file",
            "params": { "path": "/etc/shadow", "content": "bad" }
        });
        write_half
            .write_all(format!("{fs_req}\n").as_bytes())
            .await
            .unwrap();

        // Client must immediately reject fs/write_text_file
        reader.read_line(&mut line).await.unwrap();
        let fs_resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        line.clear();
        assert_eq!(fs_resp["id"], 902);
        assert_eq!(
            fs_resp["error"]["code"], -32601,
            "fs/write_text_file must be rejected with MethodNotFound"
        );
    });

    client.initialize(None).await.expect("init");
    mock_task.await.unwrap();
}

#[test]
fn test_production_api_does_not_expose_host_spawn() {
    // Verify that compiling against `marsh-acp` without the `test-support` feature
    // strictly fails to resolve `SubprocessHandle` in public production API.
    let temp_dir = tempfile::tempdir().expect("create tempdir");
    let manifest_path = temp_dir.path().join("Cargo.toml");
    let src_dir = temp_dir.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    let main_rs = src_dir.join("main.rs");

    let crate_path = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).expect("crate dir");

    // Package depending on marsh-acp with default-features = false (no test-support)
    let cargo_toml_content = format!(
        r#"[package]
name = "api-absence-check"
version = "0.1.0"
edition = "2024"

[dependencies]
marsh-acp = {{ path = "{}", default-features = false }}
"#,
        crate_path.display()
    );
    std::fs::write(&manifest_path, cargo_toml_content).expect("write Cargo.toml");

    // Attempt to import SubprocessHandle from production public API
    std::fs::write(&main_rs, "use marsh_acp::SubprocessHandle;\nfn main() {}\n")
        .expect("write main.rs");

    let cargo_bin = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let output = std::process::Command::new(cargo_bin)
        .arg("check")
        // The outer workspace has already built this path dependency. This
        // compile-fail probe tests API visibility, not registry availability.
        .arg("--offline")
        .arg("--manifest-path")
        .arg(&manifest_path)
        .output()
        .expect("invoke cargo check");

    assert!(
        !output.status.success(),
        "compilation must fail when accessing SubprocessHandle without test-support feature"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unresolved import `marsh_acp::SubprocessHandle`")
            || stderr.contains("no `SubprocessHandle` in"),
        "stderr should confirm SubprocessHandle is unexported in production: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::too_many_lines)]
async fn test_wire_fast_default_deny_multithreaded() {
    let (client_io, mut mock_agent_io) = tokio::io::duplex(64 * 1024);
    let (client_r, client_w) = tokio::io::split(client_io);

    // DefaultDenyPermissionHandler is used when None is passed
    let client = Arc::new(AcpClient::new(
        client_r,
        client_w,
        AcpClientConfig::default(),
        None,
    ));

    let mock_task = tokio::spawn(async move {
        let (read_half, mut write_half) = tokio::io::split(&mut mock_agent_io);
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        // 1. Initialize
        reader.read_line(&mut line).await.unwrap();
        let init_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let init_id = init_req["id"].clone();
        line.clear();
        let init_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": init_id,
            "result": { "protocolVersion": 1 }
        });
        write_half
            .write_all(format!("{init_resp}\n").as_bytes())
            .await
            .unwrap();

        // 2. session/new
        reader.read_line(&mut line).await.unwrap();
        let new_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let new_id = new_req["id"].clone();
        line.clear();
        let new_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": new_id,
            "result": { "sessionId": "s-fast-deny" }
        });
        write_half
            .write_all(format!("{new_resp}\n").as_bytes())
            .await
            .unwrap();

        // 3. session/prompt
        reader.read_line(&mut line).await.unwrap();
        let prompt_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let prompt_id = prompt_req["id"].clone();
        line.clear();

        // 4. Send multiple rapid permission requests with reject option.
        // Under multi-threaded scheduling, fast default-deny must NOT lose answers!
        for i in 1..=5 {
            let perm_req = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1000 + i,
                "method": "session/request_permission",
                "params": {
                    "sessionId": "s-fast-deny",
                    "toolCall": { "toolCallId": format!("tc-{i}") },
                    "options": [
                        { "optionId": format!("opt-reject-{i}"), "name": "Reject", "kind": "reject_once" }
                    ]
                }
            });
            write_half
                .write_all(format!("{perm_req}\n").as_bytes())
                .await
                .unwrap();

            // Read the fast response from client
            reader.read_line(&mut line).await.unwrap();
            let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            line.clear();

            assert_eq!(parsed["id"], 1000 + i);
            assert_eq!(
                parsed["result"]["outcome"]["outcome"], "selected",
                "fast default-deny must answer with selected reject option"
            );
            assert_eq!(
                parsed["result"]["outcome"]["optionId"],
                format!("opt-reject-{i}")
            );
        }

        // 5. Complete prompt
        let prompt_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": prompt_id,
            "result": { "stopReason": "end_turn" }
        });
        write_half
            .write_all(format!("{prompt_resp}\n").as_bytes())
            .await
            .unwrap();
    });

    client.initialize(None).await.expect("init");
    let session_id = client.new_session("/tmp", vec![]).await.expect("new");

    let (tx, _rx) = mpsc::channel(64);
    let resp = client
        .prompt(&session_id, vec![ContentBlock::text("fast")], tx)
        .await
        .expect("prompt completes");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);

    mock_task.await.unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn test_wire_permission_request_after_cancel_requested_answered_cancelled() {
    let (client_io, mut mock_agent_io) = tokio::io::duplex(64 * 1024);
    let (client_r, client_w) = tokio::io::split(client_io);

    let client = Arc::new(AcpClient::new(
        client_r,
        client_w,
        AcpClientConfig::default(),
        None,
    ));

    let mock_task = tokio::spawn(async move {
        let (read_half, mut write_half) = tokio::io::split(&mut mock_agent_io);
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();

        // 1. Initialize
        reader.read_line(&mut line).await.unwrap();
        let init_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let init_id = init_req["id"].clone();
        line.clear();
        let init_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": init_id,
            "result": { "protocolVersion": 1 }
        });
        write_half
            .write_all(format!("{init_resp}\n").as_bytes())
            .await
            .unwrap();

        // 2. session/new
        reader.read_line(&mut line).await.unwrap();
        let new_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let new_id = new_req["id"].clone();
        line.clear();
        let new_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": new_id,
            "result": { "sessionId": "s-perm-after-cancel" }
        });
        write_half
            .write_all(format!("{new_resp}\n").as_bytes())
            .await
            .unwrap();

        // 3. session/prompt
        reader.read_line(&mut line).await.unwrap();
        let prompt_req: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let prompt_id = prompt_req["id"].clone();
        line.clear();

        // 4. Read cancel notification sent by client
        reader.read_line(&mut line).await.unwrap();
        let notif: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(notif["method"], "session/cancel");
        line.clear();

        // 5. Agent sends a NEW permission request AFTER cancel was requested/dispatched
        let late_perm_req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 777,
            "method": "session/request_permission",
            "params": {
                "sessionId": "s-perm-after-cancel",
                "toolCall": { "toolCallId": "tc-late" },
                "options": [
                    { "optionId": "opt-1", "name": "Allow", "kind": "allow_once" }
                ]
            }
        });
        write_half
            .write_all(format!("{late_perm_req}\n").as_bytes())
            .await
            .unwrap();

        // 6. Client MUST immediately answer the new permission request with cancelled!
        reader.read_line(&mut line).await.unwrap();
        let perm_resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        line.clear();

        assert_eq!(perm_resp["id"], 777);
        assert_eq!(
            perm_resp["result"]["outcome"]["outcome"], "cancelled",
            "permission request arriving after cancel requested must be answered cancelled"
        );

        // 7. Agent confirms cancellation
        let cancel_resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": prompt_id,
            "result": { "stopReason": "cancelled" }
        });
        write_half
            .write_all(format!("{cancel_resp}\n").as_bytes())
            .await
            .unwrap();
    });

    client.initialize(None).await.expect("init");
    let session_id = client.new_session("/tmp", vec![]).await.expect("new");

    let (tx, _rx) = mpsc::channel(64);
    let c = Arc::clone(&client);
    let s = session_id.clone();
    let prompt_task =
        tokio::spawn(async move { c.prompt(&s, vec![ContentBlock::text("test")], tx).await });

    tokio::time::sleep(Duration::from_millis(20)).await;
    let phase = client.cancel_prompt(&session_id).await.expect("cancel");
    assert!(
        phase == CancelPhase::Dispatched || phase == CancelPhase::Confirmed,
        "phase must be dispatched or confirmed"
    );

    let resp = prompt_task
        .await
        .unwrap()
        .expect("prompt response after cancel");
    assert_eq!(resp.stop_reason, StopReason::Cancelled);
    assert_eq!(
        client.session_cancel_phase(&session_id).await,
        Some(CancelPhase::Confirmed)
    );

    mock_task.await.unwrap();
}
