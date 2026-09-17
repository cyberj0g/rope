mod support;

use futures_util::{SinkExt, StreamExt};
use rope::{provider::ResponseDelta, server::Server};
use serde_json::{Value, json};
use std::{collections::HashMap, time::Duration};
use support::Harness;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

struct Client {
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    chunks: HashMap<String, Vec<String>>,
    hello: Value,
}

impl Client {
    async fn connect(server: &Server, resume: Option<&Value>) -> Self {
        let (socket, _) = connect_async(format!("ws://{}/ws", server.address))
            .await
            .unwrap();
        let mut client = Self {
            socket,
            chunks: HashMap::new(),
            hello: Value::Null,
        };
        client.send(json!({"protocol":1,"token":"test-token","client_id":resume.map(|v| &v["client_id"]),"server_id":resume.map(|v| &v["server_id"])})).await;
        client.hello = client.until(|v| v["type"] == "hello").await;
        client
    }

    async fn send(&mut self, value: Value) {
        self.socket
            .send(Message::Text(value.to_string().into()))
            .await
            .unwrap();
    }

    async fn next(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let message = self.socket.next().await.unwrap().unwrap();
                let Message::Text(text) = message else {
                    continue;
                };
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["type"] == "chunk" {
                    let parts = self
                        .chunks
                        .entry(value["id"].as_str().unwrap().into())
                        .or_default();
                    assert_eq!(value["index"].as_u64().unwrap(), parts.len() as u64);
                    assert!(text.len() < 256 * 1024);
                    parts.push(value["data"].as_str().unwrap().into());
                } else if value["type"] == "chunk_end" {
                    let parts = self.chunks.remove(value["id"].as_str().unwrap()).unwrap();
                    assert_eq!(value["count"].as_u64().unwrap(), parts.len() as u64);
                    return serde_json::from_str(&parts.join("")).unwrap();
                } else {
                    return value;
                }
            }
        })
        .await
        .expect("WebSocket response")
    }

    async fn until(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let value = self.next().await;
                if predicate(&value) {
                    return value;
                }
            }
        })
        .await
        .expect("expected WebSocket message")
    }

    async fn subscribe(&mut self, session: &str) -> Value {
        self.send(json!({"request_id":"1","type":"subscribe","session_id":session}))
            .await;
        let snapshot = self.until(|v| v["type"] == "snapshot").await;
        let reply = self
            .until(|v| v["type"] == "reply" && v["request_id"] == "1")
            .await;
        assert!(reply.get("error").is_none(), "{reply}");
        snapshot
    }
}

async fn start(harness: &Harness) -> Server {
    Server::start(
        harness.core.clone(),
        "127.0.0.1:0".parse().unwrap(),
        "test-token".into(),
        vec![],
    )
    .await
    .unwrap()
}

async fn stop(mut server: Server, harness: &Harness) {
    server.stop();
    (&mut server.task).await.unwrap().unwrap();
    harness.core.shutdown().await.unwrap();
}

#[tokio::test]
async fn websocket_clients_share_streams_and_late_clients_receive_chunked_snapshots() {
    let mut harness = Harness::new().await;
    let id = harness.core.create(Some("web".into())).await.unwrap();
    let server = start(&harness).await;
    let mut a = Client::connect(&server, None).await;
    let mut b = Client::connect(&server, None).await;
    a.subscribe(&id).await;
    b.subscribe(&id).await;
    a.send(json!({"request_id":"2","type":"command","session_id":id,"action":{"type":"send_message","content":"hello"}})).await;
    let reply = a
        .until(|v| v["type"] == "reply" && v["request_id"] == "2")
        .await;
    assert!(reply.get("error").is_none(), "{reply}");
    let request = harness.next().await;
    let text = "🦀".repeat(18000);
    request
        .stream
        .send(Ok(ResponseDelta::Text(text.clone())))
        .unwrap();
    let event = b
        .until(|v| {
            v["type"] == "event"
                && v["update"]["changes"].as_array().is_some_and(|c| {
                    c.iter()
                        .any(|c| c["type"] == "append" && c["field"] == "content")
                })
        })
        .await;
    assert!(
        event["update"]["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["text"] == text)
    );
    let mut late = Client::connect(&server, None).await;
    let snapshot = late.subscribe(&id).await;
    assert!(
        snapshot["snapshot"]["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["content"] == text)
    );
    assert!(snapshot["snapshot"]["state"]["turn_id"].is_string());
    a.socket.close(None).await.unwrap();
    b.socket.close(None).await.unwrap();
    request.finish("done");
    late.until(|v| {
        v["type"] == "event"
            && v["update"]["changes"].as_array().is_some_and(|c| {
                c.iter()
                    .any(|c| c["type"] == "state" && c["state"]["phase"] == "idle")
            })
    })
    .await;
    stop(server, &harness).await;
}

#[tokio::test]
async fn reconnect_deduplicates_mutations_and_rejects_ambiguous_retries() {
    let harness = Harness::new().await;
    let server = start(&harness).await;
    let mut client = Client::connect(&server, None).await;
    let create = json!({"request_id":"10","type":"create_session","name":null});
    client.send(create.clone()).await;
    let original = client
        .until(|v| v["type"] == "reply" && v["request_id"] == "10")
        .await;
    assert!(original["result"]["session_id"].is_string(), "{original}");
    let identity = client.hello.clone();
    client.socket.close(None).await.unwrap();
    let mut client = Client::connect(&server, Some(&identity)).await;
    client.send(create).await;
    assert_eq!(client.until(|v| v["type"] == "reply").await, original);
    assert_eq!(harness.core.subscribe_catalog().0.sessions.len(), 1);
    client
        .send(json!({"request_id":"10","type":"create_session","name":"other"}))
        .await;
    assert_eq!(
        client.until(|v| v["type"] == "reply").await["error"]["code"],
        "request_id_reused"
    );
    client
        .send(json!({"request_id":"9","type":"create_session","name":"older"}))
        .await;
    assert_eq!(
        client.until(|v| v["type"] == "reply").await["error"]["code"],
        "expired_request"
    );
    assert_eq!(harness.core.subscribe_catalog().0.sessions.len(), 1);
    stop(server, &harness).await;
}

#[tokio::test]
async fn transport_authentication_origins_and_attachments_are_enforced() {
    let mut config = rope::config::Config::default();
    config.api_key = "provider-secret-not-for-clients".into();
    let harness = Harness::with_config(config).await;
    let id = harness.core.create(Some("images".into())).await.unwrap();
    let server = start(&harness).await;
    let mut request = format!("ws://{}/ws", server.address)
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Origin", "https://untrusted.example".parse().unwrap());
    let error = connect_async(request).await.unwrap_err();
    assert!(error.to_string().contains("403"));
    let (mut wrong, _) = connect_async(format!("ws://{}/ws", server.address))
        .await
        .unwrap();
    wrong
        .send(Message::Text(
            json!({"protocol":1,"token":"wrong"}).to_string().into(),
        ))
        .await
        .unwrap();
    assert!(
        wrong
            .next()
            .await
            .unwrap()
            .unwrap()
            .into_text()
            .unwrap()
            .contains("unauthorized")
    );
    let client = Client::connect(&server, None).await;
    assert!(!client.hello.to_string().contains("provider-secret"));
    let http = reqwest::Client::new();
    let url = format!("http://{}/api/sessions/{id}/attachments", server.address);
    assert_eq!(http.post(&url).send().await.unwrap().status(), 401);
    let mut image = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 3)
        .write_to(&mut image, image::ImageFormat::Png)
        .unwrap();
    let response = http
        .post(&url)
        .bearer_auth("test-token")
        .body(image.get_ref().clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let attachment: Value = response.json().await.unwrap();
    assert_eq!(attachment["width"], 2);
    assert_eq!(attachment["height"], 3);
    assert!(attachment.get("data").is_none());
    let download = format!("{url}/{}", attachment["path"].as_str().unwrap());
    let response = http
        .get(download)
        .bearer_auth("test-token")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), image.get_ref());
    let response = http
        .post(&url)
        .bearer_auth("test-token")
        .header("Origin", "https://untrusted.example")
        .body(image.into_inner())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    assert!(
        harness
            .core
            .attachment(&id, "attachments/../../session.json")
            .await
            .is_err()
    );
    stop(server, &harness).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn headless_binary_needs_no_tty_and_creates_no_implicit_session() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config");
    let data = root.path().join("data");
    std::fs::create_dir_all(config.join("rope")).unwrap();
    std::fs::write(
        config.join("rope/config.toml"),
        toml::to_string(&rope::config::Config::default()).unwrap(),
    )
    .unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_rope"))
        .args(["--headless", "--listen", "127.0.0.1:0"])
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_DATA_HOME", &data)
        .env_remove("ROPE_SERVER_TOKEN")
        .current_dir(root.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let address = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let line = lines.next_line().await.unwrap().unwrap();
            if let Some(url) = line.strip_prefix("Rope listening on ") {
                break url.to_owned();
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(reqwest::get(address).await.unwrap().status(), 200);
    assert_eq!(
        std::fs::read_dir(data.join("harness/sessions"))
            .unwrap()
            .count(),
        0
    );
    unsafe {
        libc::kill(child.id().unwrap() as i32, libc::SIGTERM);
    }
    assert!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}
