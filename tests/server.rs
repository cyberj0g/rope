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
        client.send(json!({"protocol":rope::protocol::VERSION,"token":"test-token","client_id":resume.map(|v| &v["client_id"]),"server_id":resume.map(|v| &v["server_id"])})).await;
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
async fn web_page_and_module_imports_are_served_without_authentication() {
    let harness = Harness::new().await;
    let server = start(&harness).await;
    let client = reqwest::Client::new();
    let base = reqwest::Url::parse(&format!("http://{}/", server.address)).unwrap();
    let response = client.get(base.clone()).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "text/html; charset=utf-8"
    );
    let html = response.text().await.unwrap();
    assert!(html.contains("type=\"module\""));
    let assets = regex::Regex::new(r#"(?:src|href)="(/assets/[^"]+)""#).unwrap();
    let imports = regex::Regex::new(r#"from "(\./[^"]+)""#).unwrap();
    let mut pending: Vec<_> = assets
        .captures_iter(&html)
        .map(|c| base.join(&c[1]).unwrap())
        .collect();
    assert_eq!(pending.len(), 2);
    let mut visited = std::collections::HashSet::new();
    while let Some(url) = pending.pop() {
        if !visited.insert(url.clone()) {
            continue;
        }
        let response = client.get(url.clone()).send().await.unwrap();
        assert_eq!(response.status(), 200, "{url}");
        let is_css = url.path().ends_with(".css");
        let content_type = if is_css {
            "text/css; charset=utf-8"
        } else {
            "text/javascript; charset=utf-8"
        };
        assert_eq!(response.headers()["content-type"], content_type, "{url}");
        let body = response.text().await.unwrap();
        assert!(!body.trim().is_empty(), "{url}");
        if !is_css {
            pending.extend(
                imports
                    .captures_iter(&body)
                    .map(|c| url.join(&c[1]).unwrap()),
            );
        }
    }
    let response = client
        .get(base.join("assets/missing.js").unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    stop(server, &harness).await;
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
            json!({"protocol":rope::protocol::VERSION,"token":"wrong"})
                .to_string()
                .into(),
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

#[tokio::test]
async fn arbitrary_uploads_reach_the_model_and_stay_session_scoped() {
    use rope::{
        protocol::Action,
        runtime::{FileContent, Message},
    };

    let mut harness = Harness::new().await;
    let id = harness.core.create(Some("files".into())).await.unwrap();
    let other = harness.core.create(Some("other".into())).await.unwrap();
    let server = start(&harness).await;
    let http = reqwest::Client::new();
    let url = format!("http://{}/api/sessions/{id}/attachments", server.address);
    let name = "notes résumé & + #.txt";
    let response = http
        .post(&url)
        .query(&[("filename", name)])
        .bearer_auth("test-token")
        .body("contents of the upload")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let file: FileContent = response.json().await.unwrap();
    assert_eq!(file.name, name);
    assert!(file.path.starts_with("uploads/"));
    let action = Action::SendMessage {
        content: "Read this file".into(),
        attachments: vec![file.path.clone()],
    };
    assert!(harness.core.command(&other, action.clone()).await.is_err());
    harness.core.command(&id, action).await.unwrap();
    let request = harness.next().await;
    let (content, images) = request
        .request
        .messages
        .iter()
        .find_map(|message| match message {
            Message::User { content, images } if content.starts_with("Read this file") => {
                Some((content, images))
            }
            _ => None,
        })
        .unwrap();
    assert!(images.is_empty());
    let uploaded: FileContent = serde_json::from_str(
        content
            .split_once("Attached file: ")
            .unwrap()
            .1
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        std::path::Path::new(&uploaded.path).file_name().unwrap(),
        name
    );
    #[cfg(unix)]
    assert!(uploaded.path.starts_with("/tmp/rope-upload-"));
    assert_eq!(
        std::fs::read(&uploaded.path).unwrap(),
        b"contents of the upload"
    );
    assert!(!content.contains("contents of the upload"));
    request.finish("received");

    for name in ["../escape.txt", "..", "C:\\escape.txt"] {
        assert_eq!(
            http.post(&url)
                .query(&[("filename", name)])
                .bearer_auth("test-token")
                .body("bad filename")
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
    }
    let mut image = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 3)
        .write_to(&mut image, image::ImageFormat::Png)
        .unwrap();
    let image: Value = http
        .post(&url)
        .query(&[("filename", "picture.bin")])
        .bearer_auth("test-token")
        .body(image.into_inner())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(image["path"].as_str().unwrap().starts_with("attachments/"));
    assert_eq!(image["width"], 2);
    assert_eq!(image["height"], 3);

    stop(server, &harness).await;
    std::fs::remove_dir_all(std::path::Path::new(&uploaded.path).parent().unwrap()).unwrap();
}

#[tokio::test]
async fn archive_previews_are_added_without_model_tool_calls() {
    use rope::{protocol::Action, runtime::Message};
    let mut harness = Harness::new().await;
    let id = harness.core.create(Some("archive".into())).await.unwrap();
    let mut archive = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(5);
    header.set_mode(0o644);
    header.set_cksum();
    archive
        .append_data(&mut header, "notes.txt", &b"hello"[..])
        .unwrap();
    let upload = harness
        .core
        .upload(&id, "documents.tar", &archive.into_inner().unwrap())
        .await
        .unwrap();
    harness
        .core
        .command(
            &id,
            Action::SendMessage {
                content: "Inspect archive".into(),
                attachments: vec![upload["path"].as_str().unwrap().into()],
            },
        )
        .await
        .unwrap();
    let request = harness.next().await;
    let content = request
        .request
        .messages
        .iter()
        .find_map(|message| match message {
            Message::User { content, .. } if content.starts_with("Inspect archive") => {
                Some(content)
            }
            _ => None,
        })
        .unwrap();
    assert!(content.contains("Automatic file preview (archive listing):"));
    assert!(
        content.contains("notes.txt") || content.contains("preview unavailable: start tar"),
        "{content}"
    );
    assert!(
        !request
            .request
            .tools
            .iter()
            .any(|tool| tool.function.name == "process_file")
    );
    let file: rope::runtime::FileContent = serde_json::from_str(
        content
            .split_once("Attached file: ")
            .unwrap()
            .1
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    request.finish("received listing");
    harness.core.shutdown().await.unwrap();
    std::fs::remove_dir_all(std::path::Path::new(&file.path).parent().unwrap()).unwrap();
}

#[tokio::test]
async fn file_downloads_require_publication_and_encode_filenames() {
    use rope::{
        config::Config,
        runtime::{Event, MAX_FILE_BYTES},
        tool::Approval,
    };
    use support::{prompt, until};

    let mut config = Config::default();
    config.tools.read = Approval::Deny;
    config.tools.shell = Approval::Deny;
    let mut harness = Harness::with_config(config).await;
    let id = harness.core.create(Some("files".into())).await.unwrap();
    let other = harness.core.create(Some("other".into())).await.unwrap();
    let mut subscription = harness.core.subscribe(&id).await.unwrap();
    let server = start(&harness).await;
    let http = reqwest::Client::new();
    let base = format!("http://{}", server.address);
    let mut names = vec!["report final; résumé.txt"];
    #[cfg(unix)]
    names.push("report\n\r\"final.txt");
    for name in names {
        let path = harness.project.path().join(name);
        tokio::fs::write(&path, b"published content").await.unwrap();
        // knowing an unpublished path must not grant read access
        assert_eq!(
            http.get(format!("{base}/api/files"))
                .query(&[("path", path.to_str().unwrap())])
                .bearer_auth("test-token")
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        harness
            .core
            .command(&id, prompt("send the file"))
            .await
            .unwrap();
        harness
            .next()
            .await
            .tool("send_file", json!({"path": path}));
        until(&mut subscription, |event| {
            matches!(event, Event::ToolFile { .. })
        })
        .await;
        harness.next().await.finish("sent");
        until(&mut subscription, |event| {
            matches!(event, Event::GenerationFinished { .. })
        })
        .await;
        let snapshot = harness.core.subscribe(&id).await.unwrap().snapshot;
        let block = snapshot
            .blocks
            .iter()
            .rev()
            .find(|block| block.file.is_some())
            .unwrap();
        let endpoint = format!("{base}/api/sessions/{id}/files/{}", block.id);
        assert_eq!(http.get(&endpoint).send().await.unwrap().status(), 401);
        assert_eq!(
            http.get(format!("{base}/api/sessions/{other}/files/{}", block.id))
                .bearer_auth("test-token")
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        assert_eq!(
            http.get(format!(
                "{base}/api/sessions/{id}/files/{}",
                snapshot.blocks[0].id
            ))
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap()
            .status(),
            404
        );
        let response = http
            .get(&endpoint)
            .bearer_auth("test-token")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let header = response.headers()["content-disposition"].to_str().unwrap();
        let encoded = header
            .strip_prefix("attachment; filename=download; filename*=UTF-8''")
            .unwrap();
        let decoded = url::form_urlencoded::parse(format!("name={encoded}").as_bytes())
            .into_owned()
            .next()
            .unwrap()
            .1;
        assert_eq!(decoded, name);
        assert_eq!(response.bytes().await.unwrap(), &b"published content"[..]);
        let source = tokio::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .await
            .unwrap();
        source.set_len(MAX_FILE_BYTES + 1).await.unwrap();
        assert_eq!(
            http.get(&endpoint)
                .bearer_auth("test-token")
                .send()
                .await
                .unwrap()
                .status(),
            413
        );
        drop(source);
    }
    stop(server, &harness).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn catalog_is_paginated_and_filtered_server_side() {
    let harness = Harness::new().await;
    for i in 1..=25 {
        harness
            .core
            .create(Some(format!("alpha-{i:02}")))
            .await
            .unwrap();
    }
    let server = start(&harness).await;
    let mut client = Client::connect(&server, None).await;
    let catalog = client.until(|v| v["type"] == "catalog").await;
    assert_eq!(catalog["catalog"]["sessions"].as_array().unwrap().len(), 20);
    assert_eq!(catalog["catalog"]["total"], 25);

    // A query matching only rows beyond the first page still finds them:
    // the filter runs over every session on the server.
    client
        .send(json!({"request_id":"1","type":"catalog_view","query":"alpha-2","offset":20}))
        .await;
    let reply = client
        .until(|v| v["type"] == "reply" && v["request_id"] == "1")
        .await;
    let mut names: Vec<&str> = reply["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "alpha-20", "alpha-21", "alpha-22", "alpha-23", "alpha-24", "alpha-25"
        ]
    );
    assert_eq!(reply["result"]["total"], 6);

    // The connection's window now follows the query: a catalog refresh keeps
    // it filtered.
    harness.core.create(Some("alpha-26".into())).await.unwrap();
    let refreshed = client.until(|v| v["type"] == "catalog").await;
    assert_eq!(refreshed["catalog"]["total"], 7);
    assert_eq!(
        refreshed["catalog"]["sessions"].as_array().unwrap().len(),
        7
    );

    // Widen the window; the reply and the next refresh carry the full page.
    client
        .send(json!({"request_id":"2","type":"catalog_view","query":null,"offset":27}))
        .await;
    let reply = client
        .until(|v| v["type"] == "reply" && v["request_id"] == "2")
        .await;
    assert_eq!(reply["result"]["sessions"].as_array().unwrap().len(), 26);
    assert_eq!(reply["result"]["total"], 26);
    // The next catalog refresh serves the widened, unfiltered window.
    harness.core.create(Some("beta".into())).await.unwrap();
    let refreshed = client.until(|v| v["type"] == "catalog").await;
    assert_eq!(
        refreshed["catalog"]["sessions"].as_array().unwrap().len(),
        27
    );
    assert_eq!(refreshed["catalog"]["total"], 27);

    client.socket.close(None).await.unwrap();
    stop(server, &harness).await;
}

#[tokio::test]
async fn sessions_can_be_deleted_but_not_while_running() {
    let mut harness = Harness::new().await;
    let doomed = harness.core.create(Some("doomed".into())).await.unwrap();
    let idle = harness.core.create(Some("idle".into())).await.unwrap();
    let server = start(&harness).await;
    let mut client = Client::connect(&server, None).await;

    // Deleting the currently subscribed, running session is refused.
    client.subscribe(&doomed).await;
    client
        .send(json!({"request_id":"2","type":"command","session_id":doomed,"action":{"type":"send_message","content":"work"}}))
        .await;
    let _reply = client
        .until(|v| v["type"] == "reply" && v["request_id"] == "2")
        .await;
    let request = harness.next().await; // the turn is now running
    client
        .until(|v| {
            v["type"] == "catalog"
                && v["catalog"]["sessions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|s| s["name"] == doomed && s["activity"] == "running")
        })
        .await;
    client
        .send(json!({"request_id":"3","type":"delete_session","session_id":doomed}))
        .await;
    let reply = client
        .until(|v| v["type"] == "reply" && v["request_id"] == "3")
        .await;
    assert_eq!(reply["error"]["code"], "busy");

    // Unknown sessions are refused too.
    client
        .send(json!({"request_id":"4","type":"delete_session","session_id":"ghost"}))
        .await;
    let reply = client
        .until(|v| v["type"] == "reply" && v["request_id"] == "4")
        .await;
    assert_eq!(reply["error"]["code"], "unknown");

    request.finish("done");
    client
        .until(|v| {
            v["type"] == "event"
                && v["update"]["session_id"] == doomed
                && v["update"]["changes"].as_array().is_some_and(|c| {
                    c.iter()
                        .any(|c| c["type"] == "state" && c["state"]["phase"] == "idle")
                })
        })
        .await;

    // Once idle, deletion removes the catalog row and the stored session.
    client
        .send(json!({"request_id":"5","type":"delete_session","session_id":doomed}))
        .await;
    let reply = client
        .until(|v| v["type"] == "reply" && v["request_id"] == "5")
        .await;
    assert!(reply.get("error").is_none(), "{reply}");
    let refreshed = client.until(|v| v["type"] == "catalog").await;
    assert!(
        refreshed["catalog"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["name"] != doomed),
        "deleted session must leave the catalog"
    );
    assert!(!harness.storage.path().join(&doomed).exists());
    assert!(harness.storage.path().join(&idle).exists());

    // The same request id with the same payload is a safe retry.
    client
        .send(json!({"request_id":"5","type":"delete_session","session_id":doomed}))
        .await;
    let retry = client
        .until(|v| v["type"] == "reply" && v["request_id"] == "5")
        .await;
    assert_eq!(retry, reply);

    client.socket.close(None).await.unwrap();
    stop(server, &harness).await;
}

#[tokio::test]
async fn collapsed_thinking_and_tool_content_is_withheld_until_revealed() {
    let mut harness = Harness::new().await;
    tokio::fs::write(
        harness.project.path().join("notes.md"),
        "secret tool business",
    )
    .await
    .unwrap();
    let id = harness.core.create(Some("redact".into())).await.unwrap();
    let server = start(&harness).await;
    let mut watcher = Client::connect(&server, None).await;
    watcher.subscribe(&id).await;

    watcher
        .send(json!({"request_id":"2","type":"command","session_id":id,"action":{"type":"send_message","content":"do it"}}))
        .await;
    let _reply = watcher
        .until(|v| v["type"] == "reply" && v["request_id"] == "2")
        .await;

    let request = harness.next().await;
    request
        .stream
        .send(Ok(ResponseDelta::Reasoning("pondering deeply".into())))
        .unwrap();
    request
        .stream
        .send(Ok(ResponseDelta::Text("calling a tool".into())))
        .unwrap();
    request
        .stream
        .send(Ok(ResponseDelta::ToolCall {
            index: 0,
            id: Some("call-1".into()),
            name: Some("read".into()),
            arguments: r#"{"path":"notes.md"}"#.into(),
        }))
        .unwrap();
    request.stream.send(Ok(ResponseDelta::Completed)).unwrap();
    drop(request); // the stream ends, the tool runs, and the turn continues
    let second = harness.next().await;
    second
        .stream
        .send(Ok(ResponseDelta::Text("done reading".into())))
        .unwrap();
    drop(second);

    // Collect the watcher's view of the turn until it finishes.
    let mut tool_block: Value = Value::Null;
    let mut saw_tool_insert = false;
    let mut saw_hidden_append = false;
    let mut saw_visible_append = false;
    loop {
        let value = watcher.next().await;
        if value["type"] != "event" || value["update"]["session_id"] != id {
            continue;
        }
        let changes = value["update"]["changes"].as_array().unwrap();
        for change in changes {
            match change["type"].as_str().unwrap() {
                "insert" | "replace" => {
                    if change["block"]["kind"] == "tool" {
                        saw_tool_insert = true;
                        tool_block = change["block"].clone();
                    }
                }
                "append" => match change["field"].as_str().unwrap() {
                    "arguments" | "output" => saw_hidden_append = true,
                    "content" => saw_visible_append = true,
                    _ => {}
                },
                _ => {}
            }
        }
        if changes
            .iter()
            .any(|c| c["type"] == "state" && c["state"]["phase"] == "idle")
        {
            break;
        }
    }
    assert!(
        saw_tool_insert,
        "the tool header must arrive even when collapsed"
    );
    assert!(
        saw_visible_append,
        "assistant text must stream while tools stay redacted"
    );
    assert!(
        !saw_hidden_append,
        "collapsed tool content must not be delivered"
    );
    assert_eq!(
        tool_block["tool"]["arguments"], "",
        "redacted tool arguments"
    );
    assert!(
        tool_block["tool"]["output"].is_null(),
        "redacted tool output"
    );
    assert_eq!(tool_block["tool"]["redacted"], true);
    assert_eq!(
        tool_block["tool"]["name"], "read",
        "the tool name is visible"
    );

    // A late subscriber gets a redacted snapshot, not the withheld content.
    let mut late = Client::connect(&server, None).await;
    let snapshot = late.subscribe(&id).await;
    let text = snapshot.to_string();
    assert!(
        !text.contains("pondering deeply"),
        "thinking content leaked: {text}"
    );
    assert!(
        !text.contains("secret tool business"),
        "tool output leaked: {text}"
    );
    assert!(
        text.contains("done reading"),
        "assistant content must stay visible"
    );
    let thinking = snapshot["snapshot"]["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["kind"] == "thinking")
        .unwrap();
    assert_eq!(thinking["content"], "");
    assert_eq!(thinking["redacted"], true);

    // Reveal: the full content arrives once, and only for that block.
    let block_id = tool_block["id"].as_str().unwrap().to_owned();
    watcher
        .send(json!({"request_id":"3","type":"reveal_block","session_id":id,"block_id":block_id}))
        .await;
    let reply = watcher
        .until(|v| v["type"] == "reply" && v["request_id"] == "3")
        .await;
    assert!(reply.get("error").is_none(), "{reply}");
    let full = &reply["result"]["block"];
    assert_eq!(
        full["tool"]["arguments"],
        serde_json::to_string_pretty(&serde_json::json!({"path": "notes.md"})).unwrap()
    );
    assert!(
        full["tool"]["output"]
            .as_str()
            .unwrap()
            .contains("secret tool business")
    );
    assert!(full["tool"].get("redacted").is_none());

    // Unknown blocks are refused.
    watcher
        .send(json!({"request_id":"4","type":"reveal_block","session_id":id,"block_id":"nope"}))
        .await;
    let reply = watcher
        .until(|v| v["type"] == "reply" && v["request_id"] == "4")
        .await;
    assert_eq!(reply["error"]["code"], "unknown_block");

    watcher.socket.close(None).await.unwrap();
    late.socket.close(None).await.unwrap();
    stop(server, &harness).await;
}

#[tokio::test]
async fn headless_binary_needs_no_tty_and_creates_no_implicit_session() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let provider = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider_address = provider.local_addr().unwrap();
    let provider_task = tokio::spawn(async move {
        axum::serve(
            provider,
            axum::Router::new().route(
                "/responses",
                axum::routing::post(|| async {
                    (
                        axum::http::StatusCode::UNAUTHORIZED,
                        "headless test provider error",
                    )
                }),
            ),
        )
        .await
        .unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config");
    let data = root.path().join("data");
    std::fs::create_dir_all(config.join("rope")).unwrap();
    let mut settings = rope::config::Config::default();
    settings.base_url = format!("http://{provider_address}");
    std::fs::write(
        config.join("rope/config.toml"),
        toml::to_string(&settings).unwrap(),
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
    let mut logs = String::new();
    let address = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let line = lines.next_line().await.unwrap().unwrap();
            logs.push_str(&line);
            logs.push('\n');
            if let Some((_, url)) = line.split_once("Rope listening on ") {
                break url.to_owned();
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(reqwest::get(&address).await.unwrap().status(), 200);
    assert_eq!(
        std::fs::read_dir(data.join("harness/sessions"))
            .unwrap()
            .count(),
        0
    );
    let token = std::fs::read_to_string(config.join("rope/server-token")).unwrap();
    let (mut socket, _) = connect_async(format!("{}/ws", address.replace("http://", "ws://")))
        .await
        .unwrap();
    socket
        .send(Message::Text(
            json!({"protocol":rope::protocol::VERSION,"token":token.trim()})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    socket
        .send(Message::Text(
            json!({"request_id":"1","type":"create_session","name":"log-test"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    socket.send(Message::Text(json!({"request_id":"2","type":"command","session_id":"log-test","action":{"type":"send_message","content":"private prompt must stay out of logs"}}).to_string().into())).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let line = lines.next_line().await.unwrap().unwrap();
            logs.push_str(&line);
            logs.push('\n');
            if line.contains("ERROR [log-test]") {
                break;
            }
        }
    })
    .await
    .unwrap();
    socket.close(None).await.unwrap();
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
    while let Some(line) = lines.next_line().await.unwrap() {
        logs.push_str(&line);
        logs.push('\n');
    }
    for expected in [
        "INFO [server] project:",
        "headless mode ready",
        "WebSocket connected",
        "client authenticated:",
        "INFO [log-test] session ready",
        "INFO [log-test] turn started:",
        "INFO [log-test] requesting model",
        "headless test provider error",
        "WebSocket disconnected",
        "shutting down; stopping active sessions",
        "shutdown complete",
    ] {
        assert!(logs.contains(expected), "missing {expected}: {logs}");
    }
    assert!(!logs.contains(token.trim()));
    assert!(!logs.contains("private prompt must stay out of logs"));
    let first = logs.lines().next().unwrap();
    chrono::DateTime::parse_from_rfc3339(first.split_whitespace().next().unwrap()).unwrap();
    use tokio::io::AsyncReadExt;
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .await
        .unwrap();
    assert!(stdout.is_empty());
    provider_task.abort();
}
