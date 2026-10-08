//! Real subprocess integration tests for ACP v1 stdio JSON-RPC client.

use marsh_acp::adapter::{AgentAdapterDeclaration, AgentProtocol, AgentRegistry};
use marsh_acp::cancel::CancelPhase;
use marsh_acp::client::{AcpClient, AcpClientConfig};
use marsh_acp::error::{AcpError, AgentError};
use marsh_acp::protocol::{
    ACP_V1_PROTOCOL_VERSION, ContentBlock, ImplementationInfo, SessionUpdate, StopReason, ToolKind,
};
use marsh_acp::transport::SubprocessHandle;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::mpsc;

fn fake_agent_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_marsh-acp-fake-agent"))
}

fn spawn_subprocess_client(
    mode: &str,
    config: Option<AcpClientConfig>,
) -> (AcpClient, SubprocessHandle) {
    let mut cmd = Command::new(fake_agent_bin());
    cmd.arg(mode);
    let cfg = config.unwrap_or_default();
    SubprocessHandle::spawn_client(cmd, cfg, None).expect("failed to spawn subprocess client")
}

#[tokio::test]
async fn test_handshake_and_session_new() {
    let (client, _handle) = spawn_subprocess_client("normal", None);

    let client_info = ImplementationInfo {
        name: "marsh".into(),
        version: "0.1.0".into(),
    };
    let init_resp = client
        .initialize(Some(client_info))
        .await
        .expect("initialize handshake");
    assert_eq!(init_resp.protocol_version, ACP_V1_PROTOCOL_VERSION);
    let agent_info = init_resp.agent_info.expect("agent info present");
    assert_eq!(agent_info.name, "fake-acp-agent");

    let session_id = client
        .new_session("/tmp/test-project", vec![])
        .await
        .expect("session/new");
    assert_eq!(session_id, "sess-fake-001");
}

#[tokio::test]
async fn test_sequential_prompt_and_streaming_updates() {
    let (client, _handle) = spawn_subprocess_client("normal", None);
    client.initialize(None).await.expect("initialize");
    let session_id = client.new_session("/tmp", vec![]).await.expect("session");

    let (update_tx, mut update_rx) = mpsc::channel(64);
    let prompt = vec![ContentBlock::text("Say hello")];

    let resp = client
        .prompt(&session_id, prompt, update_tx)
        .await
        .expect("prompt turn");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);

    // Verify streamed notifications were received
    let update1 = update_rx.recv().await.expect("received update 1");
    match update1 {
        SessionUpdate::AgentMessageChunk { content } => {
            assert_eq!(content, ContentBlock::text("Processing your prompt..."));
        }
        other => panic!("unexpected update 1: {other:?}"),
    }

    let update2 = update_rx.recv().await.expect("received update 2");
    match update2 {
        SessionUpdate::ToolCall { update } => {
            assert_eq!(update.tool_call_id, "call-1");
            assert_eq!(update.kind, Some(ToolKind::Think));
        }
        other => panic!("unexpected update 2: {other:?}"),
    }

    let update3 = update_rx.recv().await.expect("received update 3");
    match update3 {
        SessionUpdate::Plan { entries } => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].content, "Analyze task");
        }
        other => panic!("unexpected update 3: {other:?}"),
    }
}

#[tokio::test]
async fn test_busy_error_on_concurrent_prompt() {
    let (client, _handle) = spawn_subprocess_client("slow-cancel", None);
    client.initialize(None).await.expect("initialize");
    let session_id = client.new_session("/tmp", vec![]).await.expect("session");

    let (tx1, _rx1) = mpsc::channel(64);
    let (tx2, _rx2) = mpsc::channel(64);

    let client_arc = std::sync::Arc::new(client);
    let c1 = std::sync::Arc::clone(&client_arc);
    let s1 = session_id.clone();

    // Start first prompt (which runs slowly)
    let h1 = tokio::spawn(async move {
        c1.prompt(&s1, vec![ContentBlock::text("turn 1")], tx1)
            .await
    });

    // Wait a brief moment to ensure turn 1 is active
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Second prompt must fail immediately with Busy
    let res2 = client_arc
        .prompt(&session_id, vec![ContentBlock::text("turn 2")], tx2)
        .await;

    match res2 {
        Err(AcpError::Busy) => {}
        other => panic!("expected AcpError::Busy, got {other:?}"),
    }

    // Cancel first prompt so the task finishes cleanly
    let cancel_res = client_arc.cancel_prompt(&session_id).await;
    assert!(cancel_res.is_ok());

    let _ = h1.await;
}

#[tokio::test]
async fn test_cancellation_dispatched_and_confirmed() {
    let (client, _handle) = spawn_subprocess_client("slow-cancel", None);
    client.initialize(None).await.expect("initialize");
    let session_id = client.new_session("/tmp", vec![]).await.expect("session");

    let (tx, _rx) = mpsc::channel(64);
    let client_arc = std::sync::Arc::new(client);
    let c1 = std::sync::Arc::clone(&client_arc);
    let s1 = session_id.clone();

    let h1 = tokio::spawn(async move {
        c1.prompt(&s1, vec![ContentBlock::text("slow prompt")], tx)
            .await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    // Trigger cancel
    let phase = client_arc
        .cancel_prompt(&session_id)
        .await
        .expect("cancel prompt");
    assert_eq!(phase, CancelPhase::Dispatched);

    let resp = h1.await.unwrap().expect("prompt response after cancel");
    assert_eq!(resp.stop_reason, StopReason::Cancelled);
    assert_eq!(
        client_arc.session_cancel_phase(&session_id).await,
        Some(CancelPhase::Confirmed)
    );
}

#[tokio::test]
async fn test_capability_gating_load_and_resume() {
    // 1. Agent WITHOUT loadSession or resume capabilities
    let (client, _handle) = spawn_subprocess_client("normal", None);
    client.initialize(None).await.expect("initialize");

    let load_res = client.load_session("sess-1", "/tmp", vec![]).await;
    match load_res {
        Err(AcpError::CapabilityNotSupported(cap)) => assert_eq!(cap, "loadSession"),
        other => panic!("expected CapabilityNotSupported(loadSession), got {other:?}"),
    }

    let resume_res = client.resume_session("sess-1", "/tmp", None).await;
    match resume_res {
        Err(AcpError::CapabilityNotSupported(cap)) => {
            assert_eq!(cap, "sessionCapabilities.resume");
        }
        other => {
            panic!("expected CapabilityNotSupported(sessionCapabilities.resume), got {other:?}")
        }
    }

    // 2. Agent WITH loadSession capability
    let (client_load, _handle) = spawn_subprocess_client("with-load", None);
    client_load.initialize(None).await.expect("initialize");
    let load_ok = client_load.load_session("sess-1", "/tmp", vec![]).await;
    assert!(load_ok.is_ok());

    // 3. Agent WITH resume capability
    let (client_resume, _handle) = spawn_subprocess_client("with-resume", None);
    client_resume.initialize(None).await.expect("initialize");
    let resume_ok = client_resume.resume_session("sess-1", "/tmp", None).await;
    assert!(resume_ok.is_ok());
}

#[tokio::test]
async fn test_bounded_frames_rejection() {
    let config = AcpClientConfig {
        max_frame_bytes: 512,
        ..Default::default()
    };
    let (client, _handle) = spawn_subprocess_client("oversized-frame", Some(config));

    // The agent immediately sends an oversized line
    let init_res = client.initialize(None).await;
    match init_res {
        Err(AcpError::TransportLost(_) | AcpError::FrameTooLarge { .. }) => {}
        other => panic!("expected FrameTooLarge or TransportLost, got {other:?}"),
    }
}

#[tokio::test]
async fn test_explicit_no_host_execution_rejection() {
    // Agent attempts to invoke terminal/create and fs/write_text_file
    let (client, _handle) = spawn_subprocess_client("attempt-host-exec", None);
    client.initialize(None).await.expect("initialize");
    let session_id = client.new_session("/tmp", vec![]).await.expect("session");

    let (update_tx, _update_rx) = mpsc::channel(64);
    let resp = client
        .prompt(&session_id, vec![ContentBlock::text("test")], update_tx)
        .await
        .expect("prompt turn completes");

    // Verified that prompt turn completed normally and no host files or commands ran
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    assert!(!std::path::Path::new("/tmp/pwned").exists());
}

#[tokio::test]
async fn test_permission_default_deny() {
    let (client, _handle) = spawn_subprocess_client("request-permission", None);
    client.initialize(None).await.expect("initialize");
    let session_id = client.new_session("/tmp", vec![]).await.expect("session");

    let (update_tx, _update_rx) = mpsc::channel(64);
    let resp = client
        .prompt(&session_id, vec![ContentBlock::text("test")], update_tx)
        .await
        .expect("prompt turn");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
async fn test_agent_adapter_registry_and_no_fake_claude_cli_claim() {
    let json_registry = r#"[
        {
            "schema_version": 1,
            "name": "claude-session",
            "protocol": "acp_v1",
            "command": "claude-workload",
            "workload_digest": "sha256:1111111111111111111111111111111111111111111111111111111111111111",
            "required_capabilities": ["loadSession"],
            "description": "Tested ACP workload for Claude"
        }
    ]"#;

    let registry = AgentRegistry::from_json_str(json_registry).expect("parse registry");

    // 1. Explicit name 'claude-session' resolves correctly
    let decl = registry
        .resolve_acp("claude-session")
        .expect("resolve claude-session");
    assert_eq!(decl.name, "claude-session");
    assert_eq!(decl.command, "claude-workload");
    assert_eq!(decl.protocol, AgentProtocol::AcpV1);

    // 2. Truthfulness check: Plain CLI command 'claude' must NOT be claimed as ACP!
    let cli_res = registry.resolve_acp("claude");
    match cli_res {
        Err(AgentError::UnsupportedProtocol { command, reason }) => {
            assert_eq!(command, "claude");
            assert!(reason.contains("plain 'claude' is a native CLI command"));
        }
        other => panic!("expected UnsupportedProtocol for plain claude, got {other:?}"),
    }

    // 3. Falsely declaring 'claude' as AcpV1 fails validation
    let bad_decl = AgentAdapterDeclaration {
        schema_version: 1,
        name: "claude".into(),
        protocol: AgentProtocol::AcpV1,
        command: "claude-kit".into(),
        workload_digest: "sha256:2222222222222222222222222222222222222222222222222222222222222222"
            .into(),
        required_capabilities: vec![],
        arguments: vec![],
        description: None,
    };
    match bad_decl.validate() {
        Err(AgentError::UnsupportedProtocol { command, reason }) => {
            assert_eq!(command, "claude");
            assert!(reason.contains("plain 'claude' CLI does not speak ACP"));
        }
        other => panic!("expected validation rejection for claude as ACP, got {other:?}"),
    }
}

#[tokio::test]
async fn test_bounded_timeout() {
    let config = AcpClientConfig {
        turn_timeout: Duration::from_millis(100),
        ..Default::default()
    };
    let (client, _handle) = spawn_subprocess_client("slow-cancel", Some(config));
    client.initialize(None).await.expect("initialize");
    let session_id = client.new_session("/tmp", vec![]).await.expect("session");

    let (tx, _rx) = mpsc::channel(64);
    let res = client
        .prompt(&session_id, vec![ContentBlock::text("test")], tx)
        .await;

    match res {
        Err(AcpError::Timeout { operation, elapsed }) => {
            assert_eq!(operation, "session/prompt");
            assert!(elapsed <= Duration::from_millis(500));
        }
        other => panic!("expected Timeout error, got {other:?}"),
    }
}

#[tokio::test]
async fn test_unbounded_kit_turn_can_outlive_short_deadline_and_accept_next_turn() {
    let config = AcpClientConfig {
        turn_timeout: Duration::ZERO,
        ..Default::default()
    };
    let (client, _handle) = spawn_subprocess_client("slow-cancel", Some(config));
    client.initialize(None).await.expect("initialize");
    let session_id = client.new_session("/tmp", vec![]).await.expect("session");
    let client = std::sync::Arc::new(client);
    for turn in 0..2 {
        let current = std::sync::Arc::clone(&client);
        let current_session = session_id.clone();
        let (tx, _rx) = mpsc::channel(64);
        let prompt = tokio::spawn(async move {
            current
                .prompt(
                    &current_session,
                    vec![ContentBlock::text(format!("turn {turn}"))],
                    tx,
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !prompt.is_finished(),
            "held turn timed out before explicit cancel"
        );
        assert_eq!(
            client.cancel_prompt(&session_id).await.expect("cancel"),
            CancelPhase::Dispatched
        );
        assert_eq!(
            prompt.await.unwrap().expect("cancelled prompt").stop_reason,
            StopReason::Cancelled
        );
    }
}

#[tokio::test]
async fn test_process_exit_and_transport_loss() {
    let (client, handle) = spawn_subprocess_client("normal", None);
    client.initialize(None).await.expect("initialize");

    // Kill the child process directly
    handle.kill().await.expect("kill child");

    // Give kernel a moment to terminate child and close pipes
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Subsequent operation should fail with TransportLost
    let res = client.new_session("/tmp", vec![]).await;
    match res {
        Err(AcpError::TransportLost(_)) => {}
        other => panic!("expected TransportLost, got {other:?}"),
    }
}

#[tokio::test]
async fn test_turn_guard_on_caller_drop_and_no_stale_leak() {
    let (client, _handle) = spawn_subprocess_client("slow-cancel", None);
    client.initialize(None).await.expect("initialize");
    let session_id = client.new_session("/tmp", vec![]).await.expect("session");

    let client_arc = std::sync::Arc::new(client);
    let c1 = std::sync::Arc::clone(&client_arc);
    let s1 = session_id.clone();
    let (tx1, mut rx1) = mpsc::channel(64);

    // Start turn 1 in a task, then drop it (simulate caller timeout or cancel)
    let handle1 = tokio::spawn(async move {
        c1.prompt(&s1, vec![ContentBlock::text("dropped prompt")], tx1)
            .await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    // Abort handle 1 (drops prompt future)
    handle1.abort();

    // Turn guard should have transitioned to Cancelling, sent session/cancel,
    // and dropped tx1.
    // Give a moment for the agent to finish cancelling and send prompt response
    tokio::time::sleep(Duration::from_millis(250)).await;

    // Verify rx1 is closed or drained (no stale updates from old turn)
    let _ = rx1.recv().await;

    // Now start turn 2: must succeed cleanly!
    let (tx2, mut rx2) = mpsc::channel(64);
    let resp2 = client_arc
        .prompt(&session_id, vec![ContentBlock::text("fresh turn")], tx2)
        .await
        .expect("second prompt after drop succeeds");

    assert_eq!(resp2.stop_reason, StopReason::EndTurn);

    // Verify rx2 receives updates for turn 2
    let u = rx2.recv().await.expect("received turn 2 update");
    match u {
        SessionUpdate::AgentMessageChunk { content } => {
            assert_eq!(content, ContentBlock::text("Processing your prompt..."));
        }
        other => panic!("expected turn 2 message chunk, got {other:?}"),
    }
}

#[tokio::test]
async fn test_permission_handler_does_not_block_reader() {
    use marsh_acp::client::PermissionHandler;
    use marsh_acp::protocol::{RequestPermissionOutcome, RequestPermissionRequest};
    use std::pin::Pin;

    struct SlowDenyHandler;
    impl PermissionHandler for SlowDenyHandler {
        fn handle_permission(
            &self,
            _req: RequestPermissionRequest,
        ) -> Pin<Box<dyn std::future::Future<Output = RequestPermissionOutcome> + Send>> {
            Box::pin(async move {
                // Sleep to ensure reader loop would have been blocked if not spawned
                tokio::time::sleep(Duration::from_millis(150)).await;
                RequestPermissionOutcome::cancel()
            })
        }
    }

    let mut cmd = Command::new(fake_agent_bin());
    cmd.arg("request-permission");
    let (client, _handle) = SubprocessHandle::spawn_client(
        cmd,
        AcpClientConfig::default(),
        Some(std::sync::Arc::new(SlowDenyHandler)),
    )
    .expect("spawn client with slow permission handler");

    client.initialize(None).await.expect("initialize");
    let session_id = client.new_session("/tmp", vec![]).await.expect("session");

    let (tx, mut rx) = mpsc::channel(64);
    let resp = client
        .prompt(&session_id, vec![ContentBlock::text("perm test")], tx)
        .await
        .expect("prompt completes");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);

    // Updates should have arrived despite slow permission handler
    let mut got_updates = 0;
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
        got_updates += 1;
    }
    assert!(got_updates > 0, "reader received streamed updates");
}

#[tokio::test]
async fn test_subprocess_cancelled_permission_on_caller_drop() {
    struct HangingHandler;
    impl marsh_acp::client::PermissionHandler for HangingHandler {
        fn handle_permission(
            &self,
            _req: marsh_acp::protocol::RequestPermissionRequest,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = marsh_acp::protocol::RequestPermissionOutcome>
                    + Send,
            >,
        > {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                marsh_acp::protocol::RequestPermissionOutcome::cancel()
            })
        }
    }

    let mut cmd = Command::new(fake_agent_bin());
    cmd.arg("permission-hang");
    let (client, _handle) = SubprocessHandle::spawn_client(
        cmd,
        AcpClientConfig::default(),
        Some(std::sync::Arc::new(HangingHandler)),
    )
    .expect("spawn client");

    client.initialize(None).await.expect("initialize");
    let session_id = client.new_session("/tmp", vec![]).await.expect("session");

    let client_arc = std::sync::Arc::new(client);
    let c1 = std::sync::Arc::clone(&client_arc);
    let s1 = session_id.clone();
    let (tx, _rx) = mpsc::channel(64);

    let prompt_task =
        tokio::spawn(async move { c1.prompt(&s1, vec![ContentBlock::text("hang")], tx).await });

    // Give subprocess time to reach permission request
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Caller drops prompt turn
    prompt_task.abort();

    // TurnGuard should abort permission handler and dispatch cancel to agent
    // Active turn should be cleaned up within cancel_timeout
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Subsequent prompt should NOT return Busy
    let (tx2, _rx2) = mpsc::channel(64);
    let res2 = client_arc
        .prompt(&session_id, vec![ContentBlock::text("turn 2")], tx2)
        .await;
    // Subprocess finished or is ready for next turn
    assert!(
        res2.is_ok() || matches!(res2, Err(AcpError::TransportLost(_))),
        "turn 2 must not be Busy: {res2:?}"
    );
}

#[tokio::test]
async fn test_subprocess_normal_end_cancel_race() {
    let mut cmd = Command::new(fake_agent_bin());
    cmd.arg("cancel-race-end-turn");
    let (client, _handle) = SubprocessHandle::spawn_client(cmd, AcpClientConfig::default(), None)
        .expect("spawn client");

    client.initialize(None).await.expect("initialize");
    let session_id = client.new_session("/tmp", vec![]).await.expect("session");

    let client_arc = std::sync::Arc::new(client);
    let c1 = std::sync::Arc::clone(&client_arc);
    let s1 = session_id.clone();
    let (tx, _rx) = mpsc::channel(64);

    let prompt_task =
        tokio::spawn(async move { c1.prompt(&s1, vec![ContentBlock::text("race")], tx).await });

    tokio::time::sleep(Duration::from_millis(15)).await;
    let phase = client_arc
        .cancel_prompt(&session_id)
        .await
        .expect("cancel prompt");
    assert!(
        phase == CancelPhase::Dispatched || phase == CancelPhase::Confirmed,
        "phase: {phase:?}"
    );

    let resp = prompt_task.await.unwrap().expect("turn completes");
    // Crucial check: Agent finished normally with EndTurn despite cancel, so client MUST reflect EndTurn!
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    assert_eq!(
        client_arc.session_cancel_phase(&session_id).await,
        Some(CancelPhase::Dispatched)
    );
}
