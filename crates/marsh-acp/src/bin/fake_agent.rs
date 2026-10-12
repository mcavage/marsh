//! Standalone ACP v1 fake agent executable for real subprocess integration testing.

use marsh_acp::protocol::{
    ACP_V1_PROTOCOL_VERSION, ContentBlock, InitializeResponse, JsonRpcError, JsonRpcMessage,
    JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, NewSessionResponse, PromptResponse,
    SessionNotification, SessionUpdate, StopReason, ToolCallStatus, ToolCallUpdate, ToolKind,
};
use std::env;
use std::io::{BufRead, Write};

fn main() {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map_or("normal", String::as_str);
    let mut stdout = std::io::stdout();

    if mode == "oversized-frame" {
        let oversized = " ".repeat(1024 * 1024 + 100);
        let _ = writeln!(
            stdout,
            r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"{oversized}"}}}}}}}}"#
        );
        let _ = stdout.flush();
        return;
    }

    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if let Ok(JsonRpcMessage::Request(request)) = serde_json::from_str(line.trim()) {
            handle_request(&request, mode, &mut stdout);
        }
    }
}

fn send_response(stdout: &mut std::io::Stdout, resp: &JsonRpcResponse) {
    if let Ok(bytes) = serde_json::to_vec(resp) {
        let _ = stdout.write_all(&bytes);
        let _ = stdout.write_all(b"\n");
        let _ = stdout.flush();
    }
}

fn send_notification(stdout: &mut std::io::Stdout, notif: &JsonRpcNotification) {
    if let Ok(bytes) = serde_json::to_vec(notif) {
        let _ = stdout.write_all(&bytes);
        let _ = stdout.write_all(b"\n");
        let _ = stdout.flush();
    }
}

fn handle_prompt(req: &JsonRpcRequest, stdout: &mut std::io::Stdout) {
    let session_id = "sess-fake-001";
    let update_msg = SessionNotification {
        session_id: session_id.into(),
        update: SessionUpdate::AgentMessageChunk {
            content: ContentBlock::text("Processing your prompt..."),
        },
    };
    send_notification(
        stdout,
        &JsonRpcNotification::new(
            "session/update",
            Some(serde_json::to_value(update_msg).unwrap()),
        ),
    );

    let update_tool = SessionNotification {
        session_id: session_id.into(),
        update: SessionUpdate::ToolCall {
            update: ToolCallUpdate {
                tool_call_id: "call-1".into(),
                title: Some("Thinking".into()),
                kind: Some(ToolKind::Think),
                status: Some(ToolCallStatus::Completed),
                raw_input: None,
                raw_output: None,
            },
        },
    };
    send_notification(
        stdout,
        &JsonRpcNotification::new(
            "session/update",
            Some(serde_json::to_value(update_tool).unwrap()),
        ),
    );

    let update_plan = SessionNotification {
        session_id: session_id.into(),
        update: SessionUpdate::Plan {
            entries: vec![marsh_acp::protocol::PlanEntry::new(
                "Analyze task",
                marsh_acp::protocol::PlanEntryPriority::High,
                marsh_acp::protocol::PlanEntryStatus::Completed,
            )],
        },
    };
    send_notification(
        stdout,
        &JsonRpcNotification::new(
            "session/update",
            Some(serde_json::to_value(update_plan).unwrap()),
        ),
    );

    let resp_body = PromptResponse {
        stop_reason: StopReason::EndTurn,
    };
    let resp = JsonRpcResponse::ok(req.id.clone(), serde_json::to_value(resp_body).unwrap());
    send_response(stdout, &resp);
}

fn handle_request(req: &JsonRpcRequest, mode: &str, stdout: &mut std::io::Stdout) {
    match req.method.as_str() {
        "initialize" => {
            let mut caps = marsh_acp::protocol::AgentCapabilities::default();
            if mode == "with-load" {
                caps.load_session = Some(true);
            }
            if mode == "with-resume" {
                caps.session_capabilities = Some(marsh_acp::protocol::SessionCapabilities {
                    resume: Some(serde_json::json!({})),
                    close: None,
                    list: None,
                });
            }

            let resp_body = InitializeResponse {
                protocol_version: ACP_V1_PROTOCOL_VERSION,
                agent_capabilities: Some(caps),
                agent_info: Some(marsh_acp::protocol::ImplementationInfo {
                    name: "fake-acp-agent".into(),
                    version: "1.0.0".into(),
                }),
            };

            let resp =
                JsonRpcResponse::ok(req.id.clone(), serde_json::to_value(resp_body).unwrap());
            send_response(stdout, &resp);
        }
        "session/new" => {
            if let Some(params) = &req.params
                && params.get("mcpServers").is_none()
            {
                let resp = JsonRpcResponse::err(
                    req.id.clone(),
                    JsonRpcError::new(
                        marsh_acp::protocol::JSONRPC_INVALID_PARAMS,
                        "missing required mcpServers array",
                    ),
                );
                send_response(stdout, &resp);
                return;
            }
            let resp_body = NewSessionResponse {
                session_id: "sess-fake-001".into(),
            };
            let resp =
                JsonRpcResponse::ok(req.id.clone(), serde_json::to_value(resp_body).unwrap());
            send_response(stdout, &resp);
        }
        "session/prompt" => {
            handle_prompt(req, stdout);
        }
        "session/load" => {
            if let Some(params) = &req.params
                && params.get("mcpServers").is_none()
            {
                let resp = JsonRpcResponse::err(
                    req.id.clone(),
                    JsonRpcError::new(
                        marsh_acp::protocol::JSONRPC_INVALID_PARAMS,
                        "missing required mcpServers array",
                    ),
                );
                send_response(stdout, &resp);
                return;
            }
            let resp = JsonRpcResponse::ok(req.id.clone(), serde_json::json!({}));
            send_response(stdout, &resp);
        }
        "session/resume" => {
            let resp = JsonRpcResponse::ok(req.id.clone(), serde_json::json!({}));
            send_response(stdout, &resp);
        }
        _ => {
            let resp =
                JsonRpcResponse::err(req.id.clone(), JsonRpcError::method_not_found(&req.method));
            send_response(stdout, &resp);
        }
    }
}
