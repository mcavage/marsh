//! The client over a real child process's stdio (test-support only).

use marsh_acp::client::{AcpClient, AcpClientConfig};
use marsh_acp::error::AcpError;
use marsh_acp::protocol::{
    ACP_V1_PROTOCOL_VERSION, ContentBlock, ImplementationInfo, SessionUpdate, StopReason, ToolKind,
};
use marsh_acp::transport::SubprocessHandle;
use std::path::PathBuf;
use tokio::process::Command;
use tokio::sync::mpsc;

fn spawn_fake_agent(mode: &str, config: AcpClientConfig) -> (AcpClient, SubprocessHandle) {
    let mut command = Command::new(PathBuf::from(env!("CARGO_BIN_EXE_marsh-acp-fake-agent")));
    command.arg(mode);
    SubprocessHandle::spawn_client(command, config, None).expect("spawn fake agent")
}

#[tokio::test]
async fn handshake_session_and_streamed_turn_over_child_stdio() {
    let (client, _child) = spawn_fake_agent("normal", AcpClientConfig::default());
    let init = client
        .initialize(Some(ImplementationInfo {
            name: "marsh".into(),
            version: "0.1.0".into(),
        }))
        .await
        .unwrap();
    assert_eq!(init.protocol_version, ACP_V1_PROTOCOL_VERSION);
    assert_eq!(init.agent_info.unwrap().name, "fake-acp-agent");
    let session = client.new_session("/tmp/project", vec![]).await.unwrap();
    assert_eq!(session, "sess-fake-001");

    let (updates, mut received) = mpsc::channel(64);
    let turn = client
        .prompt(&session, vec![ContentBlock::text("Say hello")], updates)
        .await
        .unwrap();
    assert_eq!(turn.stop_reason, StopReason::EndTurn);
    match received.recv().await {
        Some(SessionUpdate::AgentMessageChunk { content }) => {
            assert_eq!(content, ContentBlock::text("Processing your prompt..."));
        }
        other => panic!("unexpected first update: {other:?}"),
    }
    match received.recv().await {
        Some(SessionUpdate::ToolCall { update }) => {
            assert_eq!(update.tool_call_id, "call-1");
            assert_eq!(update.kind, Some(ToolKind::Think));
        }
        other => panic!("unexpected second update: {other:?}"),
    }
    match received.recv().await {
        Some(SessionUpdate::Plan { entries }) => {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].content, "Analyze task");
        }
        other => panic!("unexpected third update: {other:?}"),
    }
}

#[tokio::test]
async fn load_and_resume_are_sent_only_when_the_agent_declares_them() {
    let (client, _child) = spawn_fake_agent("normal", AcpClientConfig::default());
    client.initialize(None).await.unwrap();
    assert!(matches!(
        client.load_session("sess-1", "/tmp", vec![]).await,
        Err(AcpError::CapabilityNotSupported("loadSession"))
    ));
    assert!(matches!(
        client.resume_session("sess-1", "/tmp", None).await,
        Err(AcpError::CapabilityNotSupported(
            "sessionCapabilities.resume"
        ))
    ));

    // The fake agent rejects session/load without its required mcpServers.
    let (client, _child) = spawn_fake_agent("with-load", AcpClientConfig::default());
    client.initialize(None).await.unwrap();
    client.load_session("sess-1", "/tmp", vec![]).await.unwrap();

    let (client, _child) = spawn_fake_agent("with-resume", AcpClientConfig::default());
    client.initialize(None).await.unwrap();
    client.resume_session("sess-1", "/tmp", None).await.unwrap();
}

#[tokio::test]
async fn oversized_frame_from_the_child_ends_the_transport() {
    let config = AcpClientConfig {
        max_frame_bytes: 512,
        ..Default::default()
    };
    let (client, _child) = spawn_fake_agent("oversized-frame", config);
    let result = client.initialize(None).await;
    assert!(
        matches!(result, Err(AcpError::TransportLost(_))),
        "{result:?}"
    );
    assert!(*client.transport_termination().borrow());
}

#[tokio::test]
async fn killed_child_fails_the_next_request_as_transport_loss() {
    let (client, child) = spawn_fake_agent("normal", AcpClientConfig::default());
    client.initialize(None).await.unwrap();
    child.kill().await.unwrap();
    let result = client.new_session("/tmp", vec![]).await;
    assert!(
        matches!(result, Err(AcpError::TransportLost(_))),
        "{result:?}"
    );
}

#[test]
fn production_builds_do_not_expose_host_process_spawning() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("src");
    std::fs::create_dir_all(&source).unwrap();
    let crate_path = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
    std::fs::write(
        temp.path().join("Cargo.toml"),
        format!(
            r#"[package]
name = "api-absence-check"
version = "0.1.0"
edition = "2024"

[dependencies]
marsh-acp = {{ path = "{}", default-features = false }}
"#,
            crate_path.display()
        ),
    )
    .unwrap();
    std::fs::write(
        source.join("main.rs"),
        "use marsh_acp::SubprocessHandle;\nfn main() {}\n",
    )
    .unwrap();
    let output =
        std::process::Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            // The workspace already resolved this path dependency; the probe is
            // about API visibility, not registry availability.
            .args(["check", "--offline", "--manifest-path"])
            .arg(temp.path().join("Cargo.toml"))
            .output()
            .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unresolved import `marsh_acp::SubprocessHandle`")
            || stderr.contains("no `SubprocessHandle` in"),
        "{stderr}"
    );
}
