//! Real peer regressions for bounded transport failure and envelope validation.

use marsh_acp::{AcpClient, AcpClientConfig, AcpError, ContentBlock};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
    time::timeout,
};

#[tokio::test]
async fn saturated_peer_cannot_block_cancel_or_keep_transport_live() {
    let (reader, mut peer_output) = tokio::io::duplex(32 * 1024);
    let (writer, peer_input) = tokio::io::duplex(128);
    let client = Arc::new(AcpClient::new(
        reader,
        writer,
        AcpClientConfig {
            cancel_timeout: Duration::from_millis(100),
            turn_timeout: Duration::ZERO,
            ..Default::default()
        },
        None,
    ));
    let mut peer_input = BufReader::new(peer_input);
    let (updates, _) = mpsc::channel(1);
    let prompt_client = Arc::clone(&client);
    let prompt = tokio::spawn(async move {
        prompt_client
            .prompt("session", vec![ContentBlock::text("hello")], updates)
            .await
    });
    let mut request = String::new();
    peer_input.read_line(&mut request).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&request).unwrap()["method"],
        "session/prompt"
    );

    // The peer keeps its input open but stops reading rejection responses.
    // This fills the bounded outbound queue without needing a large payload.
    for id in 0..128 {
        peer_output
            .write_all(format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"terminal/create\",\"params\":{{}}}}\n").as_bytes())
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(25)).await;
    let result = timeout(Duration::from_secs(1), client.cancel_prompt("session"))
        .await
        .expect("cancellation must include queue admission in its deadline");
    assert!(
        matches!(result, Err(AcpError::TransportLost(_))),
        "{result:?}"
    );
    assert!(*client.transport_termination().borrow());
    let result = timeout(Duration::from_secs(1), prompt)
        .await
        .expect("transport failure must wake the unbounded prompt")
        .unwrap();
    assert!(
        matches!(result, Err(AcpError::TransportLost(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn invalid_envelopes_cannot_complete_a_turn_or_deliver_later_updates() {
    for invalid in [
        json!({"jsonrpc":"not-jsonrpc","id":1,"result":{"stopReason":"end_turn"}}),
        json!({"jsonrpc":"2.0","id":1,"result":{"stopReason":"end_turn"},"error":{"code":-1,"message":"ambiguous"}}),
        json!({"jsonrpc":"2.0","id":1}),
        json!({"jsonrpc":"2.0","method":false,"id":1,"result":{"stopReason":"end_turn"}}),
        json!({"jsonrpc":"2.0","id":null,"method":"session/update","params":{}}),
    ] {
        let (client_io, peer_io) = tokio::io::duplex(8192);
        let (reader, writer) = tokio::io::split(client_io);
        let (peer_reader, mut peer_writer) = tokio::io::split(peer_io);
        let (release, released) = oneshot::channel();
        let peer = tokio::spawn(async move {
            let mut peer_reader = BufReader::new(peer_reader);
            let mut request = String::new();
            peer_reader.read_line(&mut request).await.unwrap();
            let frames = format!(
                "{invalid}\n{}\n{}\n",
                json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"must not be accepted"}}}}),
                json!({"jsonrpc":"2.0","id":1,"result":{"stopReason":"end_turn"}}),
            );
            peer_writer.write_all(frames.as_bytes()).await.unwrap();
            let _ = released.await;
        });
        let client = AcpClient::new(reader, writer, AcpClientConfig::default(), None);
        let (updates, mut received) = mpsc::channel(8);
        let result = timeout(
            Duration::from_secs(1),
            client.prompt("session", vec![ContentBlock::text("hello")], updates),
        )
        .await
        .expect("invalid envelope must close transport");
        assert!(
            matches!(result, Err(AcpError::TransportLost(_))),
            "{result:?}"
        );
        assert!(*client.transport_termination().borrow());
        assert!(
            received.try_recv().is_err(),
            "update after invalid envelope was delivered"
        );
        let _ = release.send(());
        peer.await.unwrap();
    }
}
