//! Real fixture subprocess: update backpressure must not depend on the prompt
//! response, and cancellation/control must survive a stalled update consumer.
use marsh_acp::{AcpClient, AcpClientConfig, AcpError, ContentBlock};
use std::{process::Stdio, sync::Arc, time::Duration};
use tokio::{
    process::Command,
    sync::mpsc,
    time::{sleep, timeout},
};

/// Bounds a wait for the fixture; generous so a starved runner is not a hang.
const CONDITION_DEADLINE: Duration = Duration::from_secs(30);

async fn fixture(deadline: Duration) -> (tokio::process::Child, Arc<AcpClient>, String) {
    let mut child = Command::new("node")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/acceptance/acp-fixture/agent.mjs"
        ))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let client = Arc::new(AcpClient::new(
        child.stdout.take().unwrap(),
        child.stdin.take().unwrap(),
        AcpClientConfig {
            cancel_timeout: deadline,
            ..Default::default()
        },
        None,
    ));
    client.initialize(None).await.unwrap();
    let id = client.new_session("/fixture", vec![]).await.unwrap();
    (child, client, id)
}

/// Waits until the one-slot update queue is full, so the reader is blocked
/// delivering the next update.
async fn wait_queue_full<T>(receiver: &mpsc::Receiver<T>) {
    timeout(CONDITION_DEADLINE, async {
        while receiver.len() < receiver.max_capacity() {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the agent's burst never filled the update queue");
}

#[tokio::test]
async fn delayed_consumer_preserves_every_burst_chunk() {
    let (mut child, client, id) = fixture(Duration::from_secs(10)).await;
    let (sink, mut receiver) = mpsc::channel(1);
    let c = client.clone();
    let turn = tokio::spawn(async move {
        c.prompt(&id, vec![ContentBlock::text("burst-400")], sink)
            .await
    });
    // Start consuming only once the burst is backed up behind the consumer.
    wait_queue_full(&receiver).await;
    let mut seen = Vec::new();
    while let Some(update) = timeout(CONDITION_DEADLINE, receiver.recv()).await.unwrap() {
        let value = serde_json::to_value(update).unwrap();
        seen.push(value["content"]["text"].as_str().unwrap().to_owned());
    }
    assert_eq!(
        turn.await.unwrap().unwrap().stop_reason,
        marsh_acp::StopReason::EndTurn
    );
    assert_eq!(client.dropped_updates(), 0);
    assert_eq!(
        seen,
        std::iter::once("fixture:burst-400".to_owned())
            .chain((0..400).map(|n| format!("b{n};")))
            .collect::<Vec<_>>()
    );
    child.kill().await.unwrap();
    let _ = child.wait().await;
}

#[tokio::test]
async fn stalled_consumer_does_not_deadlock_cancel_reader_or_terminal() {
    let (mut child, client, id) = fixture(Duration::from_secs(2)).await;
    let (sink, mut receiver) = mpsc::channel(1);
    let c = client.clone();
    let remote = id.clone();
    let turn = tokio::spawn(async move {
        c.prompt_raw(&remote, vec![ContentBlock::text("burst-hold")], sink)
            .await
    });
    wait_queue_full(&receiver).await;
    client.cancel_prompt(&id).await.unwrap();
    // Delivery is abandoned, and counted, only at the delivery deadline; a
    // cancel that had waited on the stalled consumer would return after it.
    assert_eq!(
        client.dropped_updates(),
        0,
        "cancel waited for the stalled update consumer"
    );
    let result = timeout(CONDITION_DEADLINE, turn)
        .await
        .expect("the terminal reply never got past the stalled consumer")
        .unwrap()
        .unwrap();
    assert_eq!(result.stop_reason, marsh_acp::StopReason::Cancelled);
    assert!(
        client.dropped_updates() > 0,
        "interrupted capture loss must remain visible"
    );
    assert!(receiver.recv().await.is_some());
    child.kill().await.unwrap();
    let _ = child.wait().await;
}

#[tokio::test]
async fn stalled_consumer_without_cancel_fences_transport_at_deadline() {
    let (mut child, client, id) = fixture(Duration::from_millis(100)).await;
    let (sink, _receiver) = mpsc::channel(1);
    let result = timeout(
        CONDITION_DEADLINE,
        client.prompt_raw(&id, vec![ContentBlock::text("burst-400")], sink),
    )
    .await
    .expect("unbounded update wait");
    assert!(
        matches!(result, Err(AcpError::TransportLost(_))),
        "{result:?}"
    );
    assert!(client.dropped_updates() > 0);
    child.kill().await.unwrap();
    let _ = child.wait().await;
}
