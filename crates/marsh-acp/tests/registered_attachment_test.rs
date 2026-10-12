use marsh_acp::{
    AcpClient, AcpClientConfig, AcpError, AgentAdapterDeclaration, AgentError, AgentProtocol,
    AgentRegistry,
};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, split};

fn declaration(required_capabilities: &[&str]) -> AgentAdapterDeclaration {
    AgentAdapterDeclaration {
        schema_version: 1,
        name: "agent-session".into(),
        protocol: AgentProtocol::AcpV1,
        command: "verified-kit-command".into(),
        workload_digest: "sha256:verified-by-caller".into(),
        required_capabilities: required_capabilities.iter().map(|s| (*s).into()).collect(),
        arguments: vec![],
        description: None,
    }
}

#[test]
fn duplicate_registry_names_cannot_override_first_declaration() {
    let mut registry = AgentRegistry::new();
    registry.register(declaration(&[])).unwrap();
    let mut second = declaration(&[]);
    second.command = "different-kit-command".into();
    assert!(matches!(
        registry.register(second),
        Err(AgentError::DuplicateName(_))
    ));
    assert_eq!(
        registry.get("agent-session").unwrap().command,
        "verified-kit-command"
    );
}

#[test]
fn unknown_declared_capability_is_rejected() {
    assert!(matches!(
        declaration(&["terminal"]).validate(),
        Err(AgentError::InvalidConfiguration(_))
    ));
}

#[tokio::test]
async fn kit_attachment_negotiates_capabilities_before_session_creation() {
    let (client_io, agent_io) = tokio::io::duplex(4096);
    let (client_reader, client_writer) = split(client_io);
    let (agent_reader, mut agent_writer) = split(agent_io);
    let agent = tokio::spawn(async move {
        let mut lines = BufReader::new(agent_reader).lines();
        let init: serde_json::Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(init["method"], "initialize");
        agent_writer.write_all(format!("{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"protocolVersion\":1,\"agentCapabilities\":{{\"loadSession\":true}}}}}}\n", init["id"]).as_bytes()).await.unwrap();
        let new: serde_json::Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(new["method"], "session/new");
        agent_writer.write_all(format!("{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"sessionId\":\"remote-agent-id\"}}}}\n", new["id"]).as_bytes()).await.unwrap();
    });

    let (client, _) = AcpClient::connect_registered(
        client_reader,
        client_writer,
        AcpClientConfig::default(),
        None,
        &declaration(&["loadSession"]),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        client.new_session("/project", vec![]).await.unwrap(),
        "remote-agent-id"
    );
    agent.await.unwrap();
}

#[tokio::test]
async fn absent_required_capability_fails_before_session_creation() {
    let (client_io, agent_io) = tokio::io::duplex(4096);
    let (client_reader, client_writer) = split(client_io);
    let (agent_reader, mut agent_writer) = split(agent_io);
    let agent = tokio::spawn(async move {
        let mut lines = BufReader::new(agent_reader).lines();
        let init: serde_json::Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        agent_writer.write_all(format!("{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"protocolVersion\":1,\"agentCapabilities\":{{}}}}}}\n", init["id"]).as_bytes()).await.unwrap();
        // The rejected client sends no session/new; it just closes.
        assert!(
            tokio::time::timeout(Duration::from_secs(30), lines.next_line())
                .await
                .expect("rejected attachment never closed")
                .unwrap()
                .is_none()
        );
    });

    let result = AcpClient::connect_registered(
        client_reader,
        client_writer,
        AcpClientConfig::default(),
        None,
        &declaration(&["loadSession"]),
        None,
    )
    .await;
    assert!(
        matches!(result, Err(AcpError::RequiredCapabilityMissing(ref cap)) if cap == "loadSession")
    );
    agent.await.unwrap();
}
