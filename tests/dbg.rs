mod support;
use futures_util::{SinkExt, StreamExt};
use rope::server::Server;
use serde_json::{Value, json};
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[tokio::test]
async fn dbg_catalog_view() {
    let harness = support::Harness::new().await;
    for i in 1..=25 {
        harness
            .core
            .create(Some(format!("alpha-{i:02}")))
            .await
            .unwrap();
    }
    let server = Server::start(
        harness.core.clone(),
        "127.0.0.1:0".parse().unwrap(),
        "test-token".into(),
        vec![],
    )
    .await
    .unwrap();
    let (mut socket, _) = connect_async(format!("ws://{}/ws", server.address))
        .await
        .unwrap();
    socket
        .send(Message::Text(
            json!({"protocol":rope::protocol::VERSION,"token":"test-token"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    // drain hello + catalog
    for _ in 0..2 {
        let m = socket.next().await.unwrap().unwrap().into_text().unwrap();
        eprintln!("GOT: {}", &m[..m.len().min(60)]);
    }
    socket
        .send(Message::Text(
            json!({"request_id":"1","type":"catalog_view","query":"alpha-2","offset":20})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    loop {
        if std::time::Instant::now() > deadline {
            eprintln!("TIMEOUT waiting reply");
            break;
        }
        match tokio::time::timeout(std::time::Duration::from_secs(2), socket.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "reply" {
                    eprintln!("REPLY: {}", t);
                    break;
                }
                eprintln!("GOT: {}", v["type"]);
            }
            other => {
                eprintln!("NO REPLY: {other:?}");
                break;
            }
        }
    }
    harness.core.shutdown().await.unwrap();
}
