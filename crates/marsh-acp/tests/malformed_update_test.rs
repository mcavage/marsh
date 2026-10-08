//! A wire peer must not make omitted updates look like a complete transcript.
use marsh_acp::{AcpClient, AcpClientConfig, ContentBlock};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
    time::timeout,
};

#[tokio::test]
async fn malformed_updates_are_counted_without_losing_the_valid_reply() {
    let (client_io, peer_io) = tokio::io::duplex(8192);
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
        for message in [
            json!({"jsonrpc":"2.0","method":"session/update"}),
            json!({"jsonrpc":"2.0","method":"session/update","params":{
                "sessionId":"session", "update":{"sessionUpdate":"tool_call",
                "toolCallId":"x", "status":"invalid-wire-status"}}}),
            json!({"jsonrpc":"2.0","method":"session/update","params":{
                "sessionId":"session", "update":{"sessionUpdate":"agent_message_chunk",
                "content":{"type":"text","text":"valid reply"}}}}),
            json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}),
        ] {
            peer_writer
                .write_all(format!("{message}\n").as_bytes())
                .await
                .unwrap();
        }
        let _ = released.await;
    });
    let client = AcpClient::new(reader, writer, AcpClientConfig::default(), None);
    let (updates, mut received) = mpsc::channel(8);
    let reply = timeout(
        Duration::from_secs(1),
        client.prompt("session", vec![ContentBlock::text("hello")], updates),
    )
    .await
    .expect("the valid terminal reply must arrive");
    assert!(reply.is_ok(), "{reply:?}");
    assert_eq!(client.dropped_updates(), 2, "omissions must be observable");
    assert!(received.try_recv().is_ok(), "valid update must be retained");
    assert!(
        received.try_recv().is_err(),
        "invalid updates must not be delivered"
    );
    let _ = release.send(());
    peer.await.unwrap();
}
