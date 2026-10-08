//! Standalone ACP v1 fake agent executable for real subprocess integration testing.

use marsh_acp::protocol::{
    ACP_V1_PROTOCOL_VERSION, ContentBlock, InitializeResponse, JsonRpcError, JsonRpcMessage,
    JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, NewSessionResponse, PromptResponse,
    RequestId, SessionNotification, SessionUpdate, StopReason, ToolCallStatus, ToolCallUpdate,
    ToolKind,
};
use std::env;
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

fn main() {
    let args: Vec<String> = env::args().collect();
    let mode = args.get(1).map_or("normal", String::as_str);

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_clone = Arc::clone(&cancelled);

    if mode == "oversized-frame" {
        // Send a frame larger than 1 MiB to trigger frame bound rejection
        let oversized = " ".repeat(1024 * 1024 + 100);
        let _ = writeln!(
            stdout,
            r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"{oversized}"}}}}}}}}"#
        );
        let _ = stdout.flush();
        return;
    }

    let (req_tx, req_rx) = mpsc::sync_channel::<JsonRpcRequest>(64);

    // Dedicated reader thread to continuously read stdin and handle notifications (like session/cancel)
    // without blocking during slow request handling.
    thread::spawn(move || {
        let mut reader = stdin.lock();
        let mut line = String::new();
        while let Ok(n) = reader.read_line(&mut line) {
            if n == 0 {
                break;
            }

            let trimmed = line.trim();
            if !trimmed.is_empty()
                && let Ok(msg) = serde_json::from_str::<JsonRpcMessage>(trimmed)
            {
                match msg {
                    JsonRpcMessage::Request(req) => {
                        if req_tx.send(req).is_err() {
                            break;
                        }
                    }
                    JsonRpcMessage::Notification(notif) => {
                        if notif.method == "session/cancel" {
                            cancelled_clone.store(true, Ordering::Release);
                        }
                    }
                    JsonRpcMessage::Response(_) => {}
                }
            }
            line.clear();
        }
    });

    while let Ok(req) = req_rx.recv() {
        handle_request(&req, mode, &mut stdout, &cancelled);
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

fn send_request(stdout: &mut std::io::Stdout, req: &JsonRpcRequest) {
    if let Ok(bytes) = serde_json::to_vec(req) {
        let _ = stdout.write_all(&bytes);
        let _ = stdout.write_all(b"\n");
        let _ = stdout.flush();
    }
}

#[allow(clippy::too_many_lines)]
fn handle_prompt(
    req: &JsonRpcRequest,
    mode: &str,
    stdout: &mut std::io::Stdout,
    cancelled: &Arc<AtomicBool>,
) {
    let session_id = "sess-fake-001";

    if mode == "attempt-host-exec" {
        let term_req = JsonRpcRequest::new(
            RequestId::Number(991),
            "terminal/create",
            Some(serde_json::json!({
                "sessionId": session_id,
                "command": "whoami"
            })),
        );
        send_request(stdout, &term_req);

        let fs_req = JsonRpcRequest::new(
            RequestId::Number(992),
            "fs/write_text_file",
            Some(serde_json::json!({
                "sessionId": session_id,
                "path": "/etc/shadow",
                "content": "hacked"
            })),
        );
        send_request(stdout, &fs_req);
    }

    if mode == "request-permission" {
        let perm_req = JsonRpcRequest::new(
            RequestId::Number(993),
            "session/request_permission",
            Some(serde_json::json!({
                "sessionId": session_id,
                "toolCall": { "toolCallId": "tc-1" },
                "options": [
                    { "optionId": "opt-allow", "name": "Allow", "kind": "allow_once" },
                    { "optionId": "opt-deny", "name": "Deny", "kind": "reject_once" }
                ]
            })),
        );
        send_request(stdout, &perm_req);
    }

    if mode == "permission-hang" {
        let perm_req = JsonRpcRequest::new(
            RequestId::Number(993),
            "session/request_permission",
            Some(serde_json::json!({
                "sessionId": session_id,
                "toolCall": { "toolCallId": "tc-1" },
                "options": [
                    { "optionId": "opt-allow", "name": "Allow", "kind": "allow_once" }
                ]
            })),
        );
        send_request(stdout, &perm_req);

        for _ in 0..100 {
            if cancelled.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(30));
        }

        let resp_body = PromptResponse {
            stop_reason: if cancelled.load(Ordering::Acquire) {
                StopReason::Cancelled
            } else {
                StopReason::EndTurn
            },
        };
        let resp = JsonRpcResponse::ok(req.id.clone(), serde_json::to_value(resp_body).unwrap());
        send_response(stdout, &resp);
        return;
    }

    if mode == "cancel-race-end-turn" {
        for _ in 0..50 {
            if cancelled.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        let resp_body = PromptResponse {
            stop_reason: StopReason::EndTurn,
        };
        let resp = JsonRpcResponse::ok(req.id.clone(), serde_json::to_value(resp_body).unwrap());
        send_response(stdout, &resp);
        return;
    }

    if mode == "slow-cancel" {
        for _ in 0..100 {
            if cancelled.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(30));
        }

        if cancelled.load(Ordering::Acquire) {
            cancelled.store(false, Ordering::Release);
            let resp_body = PromptResponse {
                stop_reason: StopReason::Cancelled,
            };
            let resp =
                JsonRpcResponse::ok(req.id.clone(), serde_json::to_value(resp_body).unwrap());
            send_response(stdout, &resp);
            return;
        }
    }

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

fn handle_request(
    req: &JsonRpcRequest,
    mode: &str,
    stdout: &mut std::io::Stdout,
    cancelled: &Arc<AtomicBool>,
) {
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
            handle_prompt(req, mode, stdout, cancelled);
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
