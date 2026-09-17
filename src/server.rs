use anyhow::{Context, Result, bail};
use axum::{
    Router,
    body::Bytes,
    extract::{
        DefaultBodyLimit, Path, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    net::TcpListener,
    sync::{Mutex as AsyncMutex, Semaphore, mpsc, watch},
    task::JoinHandle,
};

use crate::{
    core::Core,
    protocol::{self, ClientRequest, Hello, Request},
};

const CHUNK_BYTES: usize = 32 * 1024;
const REPLY_CACHE: usize = 128;

struct ClientHistory {
    highest: u64,
    replies: VecDeque<(u64, String, Value)>,
}
struct ClientRecord {
    seen: Mutex<Instant>,
    history: AsyncMutex<ClientHistory>,
}

#[derive(Clone)]
struct App {
    core: Core,
    token: Arc<String>,
    server_id: String,
    origins: Arc<Vec<String>>,
    clients: Arc<AsyncMutex<HashMap<String, Arc<ClientRecord>>>>,
    connections: Arc<Semaphore>,
    stopping: watch::Receiver<bool>,
}

pub struct Server {
    pub address: std::net::SocketAddr,
    pub task: JoinHandle<Result<()>>,
    stop: watch::Sender<bool>,
}

impl Server {
    pub async fn start(
        core: Core,
        address: std::net::SocketAddr,
        token: String,
        mut origins: Vec<String>,
    ) -> Result<Self> {
        if token.trim().is_empty() {
            bail!("server token is empty");
        }
        let listener = TcpListener::bind(address).await?;
        let address = listener.local_addr()?;
        origins.extend([
            format!("http://127.0.0.1:{}", address.port()),
            format!("http://localhost:{}", address.port()),
            format!("http://[::1]:{}", address.port()),
        ]);
        let (stop, stopping) = watch::channel(false);
        let app = App {
            core,
            token: Arc::new(token),
            server_id: uuid::Uuid::new_v4().to_string(),
            origins: Arc::new(origins),
            clients: Arc::new(AsyncMutex::new(HashMap::new())),
            connections: Arc::new(Semaphore::new(32)),
            stopping: stopping.clone(),
        };
        let attachments = Router::new()
            .route("/api/sessions/{session}/attachments", post(upload))
            .route("/api/sessions/{session}/attachments/{*id}", get(download))
            .layer(middleware::from_fn_with_state(app.clone(), authorize_http));
        let router = Router::new()
            .route("/ws", get(upgrade))
            .route(
                "/",
                get(|| async {
                    axum::response::Html(include_str!("../examples/browser-client.html"))
                }),
            )
            .merge(attachments)
            .layer(DefaultBodyLimit::max(crate::session::MAX_ATTACHMENT_BYTES))
            .with_state(app);
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let mut stopping = stopping;
                    stopping.wait_for(|stop| *stop).await.ok();
                })
                .await
                .context("HTTP server")
        });
        Ok(Self {
            address,
            task,
            stop,
        })
    }

    pub fn stop(&self) {
        self.stop.send_replace(true);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

pub fn load_token(path: Option<PathBuf>) -> Result<(String, Option<PathBuf>)> {
    if let Ok(token) = std::env::var("ROPE_SERVER_TOKEN") {
        if token.trim().is_empty() {
            bail!("ROPE_SERVER_TOKEN is empty");
        }
        return Ok((token, None));
    }
    let path = match path {
        Some(path) => path,
        None => directories::BaseDirs::new()
            .context("home directory not found")?
            .config_dir()
            .join("rope/server-token"),
    };
    if !path.exists() {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                use std::io::Write;
                writeln!(file, "{}", uuid::Uuid::new_v4())?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    let token = std::fs::read_to_string(&path)?.trim().to_owned();
    if token.is_empty() {
        bail!("server token file is empty: {}", path.display());
    }
    Ok((token, Some(path)))
}

fn valid_origin(app: &App, headers: &HeaderMap) -> bool {
    headers.get("origin").is_none_or(|origin| {
        origin
            .to_str()
            .is_ok_and(|origin| app.origins.iter().any(|allowed| allowed == origin))
    })
}

async fn upgrade(State(app): State<App>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    if !valid_origin(&app, &headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(permit) = app.connections.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    ws.max_message_size(protocol::MAX_COMMAND_BYTES)
        .max_frame_size(protocol::MAX_COMMAND_BYTES)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            connection(socket, app).await.ok();
        })
        .into_response()
}

async fn authorize_http(
    State(app): State<App>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if !valid_origin(&app, request.headers()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let origin = request.headers().get("origin").cloned();
    let preflight = request.method() == Method::OPTIONS;
    let authorized = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .is_some_and(|token| token == app.token.as_str());
    let mut response = if preflight {
        StatusCode::NO_CONTENT.into_response()
    } else if authorized {
        next.run(request).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    };
    if let Some(origin) = origin {
        response
            .headers_mut()
            .insert("access-control-allow-origin", origin);
        response
            .headers_mut()
            .insert("vary", HeaderValue::from_static("Origin"));
        response.headers_mut().insert(
            "access-control-allow-methods",
            HeaderValue::from_static("GET, POST, OPTIONS"),
        );
        response.headers_mut().insert(
            "access-control-allow-headers",
            HeaderValue::from_static("Authorization, Content-Type"),
        );
    }
    response
}

async fn upload(State(app): State<App>, Path(session): Path<String>, bytes: Bytes) -> Response {
    match app.core.attach(&session, &bytes).await {
        Ok(image) => axum::Json(image).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    }
}

async fn download(State(app): State<App>, Path((session, id)): Path<(String, String)>) -> Response {
    match app.core.attachment(&session, &id).await {
        Ok(image) => match STANDARD.decode(image.data) {
            Ok(bytes) => (
                [
                    ("content-type", image.mime_type),
                    ("cache-control", "private, no-store".into()),
                    ("x-content-type-options", "nosniff".into()),
                ],
                bytes,
            )
                .into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn connection(mut socket: WebSocket, app: App) -> Result<()> {
    let first = tokio::time::timeout(Duration::from_secs(10), socket.recv())
        .await?
        .context("missing hello")??;
    let Message::Text(text) = first else {
        bail!("expected hello");
    };
    let hello: Hello = serde_json::from_str(&text)?;
    if hello.protocol != protocol::VERSION || hello.token != *app.token {
        socket.send(Message::Text(json!({"type":"error","code":"unauthorized","message":"invalid token or protocol version"}).to_string().into())).await?;
        socket.close().await?;
        return Ok(());
    }
    let (client_id, record) = {
        let mut clients = app.clients.lock().await;
        clients.retain(|_, record| {
            record.seen.lock().unwrap().elapsed() < Duration::from_secs(24 * 3600)
        });
        if let Some(id) = hello.client_id {
            if hello.server_id.as_deref() != Some(&app.server_id) || !clients.contains_key(&id) {
                drop(clients);
                socket.send(Message::Text(json!({"type":"error","code":"expired_client","message":"reconcile state before resending uncertain requests"}).to_string().into())).await?;
                socket.close().await?;
                return Ok(());
            }
            (id.clone(), clients[&id].clone())
        } else {
            if clients.len() >= 1024 {
                bail!("too many client identities");
            }
            let id = uuid::Uuid::new_v4().to_string();
            let record = Arc::new(ClientRecord {
                seen: Mutex::new(Instant::now()),
                history: AsyncMutex::new(ClientHistory {
                    highest: 0,
                    replies: VecDeque::new(),
                }),
            });
            clients.insert(id.clone(), record.clone());
            (id, record)
        }
    };
    *record.seen.lock().unwrap() = Instant::now();
    let (mut writer, mut reader) = socket.split();
    let (output, mut messages) = mpsc::channel::<String>(32);
    let (failed, mut failure) = watch::channel(false);
    let mut stopping = app.stopping.clone();
    let writer_failed = failed.clone();
    let writer_task = tokio::spawn(async move {
        while let Some(message) = messages.recv().await {
            if !matches!(
                tokio::time::timeout(
                    Duration::from_secs(10),
                    writer.send(Message::Text(message.into()))
                )
                .await,
                Ok(Ok(()))
            ) {
                break;
            }
        }
        writer_failed.send_replace(true);
        writer.close().await.ok();
    });
    let mut tasks = ConnectionTasks {
        tasks: vec![writer_task],
        subscriptions: HashMap::new(),
    };
    enqueue(
        &output,
        &json!({"type":"hello","protocol":protocol::VERSION,"server_id":app.server_id,
        "client_id":client_id,"models":app.core.models(),"project_root":app.core.project_root()}),
    )
    .await?;
    let (catalog, mut catalogs) = app.core.subscribe_catalog();
    enqueue(&output, &json!({"type":"catalog","catalog":catalog})).await?;
    let out = output.clone();
    let cancel = failed.clone();
    let core = app.core.clone();
    tasks.tasks.push(tokio::spawn(async move {
        loop {
            let catalog = match catalogs.recv().await {
                Ok(catalog) => catalog,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let (snapshot, receiver) = core.subscribe_catalog();
                    catalogs = receiver;
                    snapshot
                }
                Err(_) => break,
            };
            if enqueue(&out, &json!({"type":"catalog","catalog":catalog}))
                .await
                .is_err()
            {
                cancel.send_replace(true);
                break;
            }
        }
    }));
    let mut project = app.core.subscribe_project();
    let initial_project = project.borrow().clone();
    enqueue(&output, &json!({"type":"project","update":initial_project})).await?;
    let out = output.clone();
    let cancel = failed.clone();
    tasks.tasks.push(tokio::spawn(async move {
        while project.changed().await.is_ok() {
            let value = json!({"type":"project","update":project.borrow_and_update().clone()});
            if enqueue(&out, &value).await.is_err() {
                cancel.send_replace(true);
                break;
            }
        }
    }));
    loop {
        tokio::select! {
            _ = async { stopping.wait_for(|stop| *stop).await.ok(); } => break,
            _ = async { failure.wait_for(|failed| *failed).await.ok(); } => break,
            message = reader.next() => {
                let Some(Ok(message)) = message else { break; };
                let Message::Text(text) = message else { if matches!(message, Message::Close(_)) { break; } continue; };
                let request: ClientRequest = match serde_json::from_str(&text) {
                    Ok(request) => request,
                    Err(_) => { enqueue(&output, &json!({"type":"error","code":"invalid_request","message":"invalid request JSON"})).await?; continue; }
                };
                *record.seen.lock().unwrap() = Instant::now();
                let mut history = record.history.lock().await;
                let number = request.request_id.parse::<u64>().ok().filter(|n| *n > 0);
                let payload = serde_json::to_string(&request.request)?;
                let mutation = matches!(request.request, Request::CreateSession { .. } | Request::Command { .. });
                let reply = if let Some(number) = number {
                    if mutation && number <= history.highest {
                        match history.replies.iter().find(|(id, _, _)| *id == number) {
                            Some((_, original, reply)) if *original == payload => reply.clone(),
                            Some(_) => error_reply(&request.request_id, "request_id_reused", "request ID was used with another payload"),
                            None => error_reply(&request.request_id, "expired_request", "the original outcome is no longer cached; reconcile state before retrying"),
                        }
                    } else {
                        if mutation { history.highest = number; }
                        let result = dispatch(&app.core, request.request, &output, &failed, &mut tasks.subscriptions).await;
                        let reply = match result {
                            Ok(value) => json!({"type":"reply","request_id":request.request_id,"result":value}),
                            Err(error) => error_reply(&request.request_id, &error.code, &error.message),
                        };
                        if mutation {
                            history.replies.push_back((number, payload, reply.clone()));
                            if history.replies.len() > REPLY_CACHE { history.replies.pop_front(); }
                        }
                        reply
                    }
                } else { error_reply(&request.request_id, "invalid_request_id", "use a positive increasing integer encoded as a string") };
                drop(history);
                enqueue(&output, &reply).await?;
            }
        }
    }
    Ok(())
}

struct ConnectionTasks {
    tasks: Vec<JoinHandle<()>>,
    subscriptions: HashMap<String, JoinHandle<()>>,
}
impl Drop for ConnectionTasks {
    fn drop(&mut self) {
        for task in self.tasks.iter().chain(self.subscriptions.values()) {
            task.abort();
        }
    }
}

fn error_reply(id: &str, code: &str, message: &str) -> Value {
    json!({"type":"reply","request_id":id,"error":{"code":code,"message":message}})
}

async fn dispatch(
    core: &Core,
    request: Request,
    output: &mpsc::Sender<String>,
    failed: &watch::Sender<bool>,
    subscriptions: &mut HashMap<String, JoinHandle<()>>,
) -> protocol::Result<Value> {
    let error = |e: anyhow::Error| protocol::Error::new("core", format!("{e:#}"));
    Ok(match request {
        Request::CreateSession { name } => {
            json!({"session_id":core.create(name).await.map_err(error)?})
        }
        Request::Command { session_id, action } => {
            serde_json::to_value(core.command(&session_id, action).await?).unwrap()
        }
        Request::GitDiff { path } => {
            json!({"path":path,"content":core.diff(path.as_deref().map(std::path::Path::new)).await.map_err(error)?})
        }
        Request::Unsubscribe { session_id } => {
            if let Some(task) = subscriptions.remove(&session_id) {
                task.abort();
            }
            json!({})
        }
        Request::Subscribe { session_id } => {
            if subscriptions.len() >= 16 && !subscriptions.contains_key(&session_id) {
                return Err(protocol::Error::new(
                    "limit",
                    "at most 16 subscriptions per connection",
                ));
            }
            let mut subscription = core.subscribe(&session_id).await.map_err(error)?;
            if let Some(task) = subscriptions.remove(&session_id) {
                task.abort();
            }
            enqueue(
                output,
                &json!({"type":"snapshot","snapshot":subscription.snapshot}),
            )
            .await
            .map_err(error)?;
            let output = output.clone();
            let failed = failed.clone();
            let id = session_id.clone();
            subscriptions.insert(
                session_id,
                tokio::spawn(async move {
                    loop {
                        let message = match subscription.updates.recv().await {
                            Ok(update) => json!({"type":"event","update":*update}),
                            Err(_) => {
                                if enqueue(
                                    &output,
                                    &json!({"type":"resync_required","session_id":id}),
                                )
                                .await
                                .is_err()
                                {
                                    failed.send_replace(true);
                                }
                                break;
                            }
                        };
                        if enqueue(&output, &message).await.is_err() {
                            failed.send_replace(true);
                            break;
                        }
                    }
                }),
            );
            json!({})
        }
    })
}

async fn enqueue(output: &mpsc::Sender<String>, value: &Value) -> Result<()> {
    let text = serde_json::to_string(value)?;
    if text.len() <= CHUNK_BYTES {
        tokio::time::timeout(Duration::from_secs(5), output.send(text)).await??;
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        let mut rest = text.as_str();
        let mut index = 0;
        while !rest.is_empty() {
            let mut end = rest.len().min(CHUNK_BYTES);
            while !rest.is_char_boundary(end) {
                end -= 1;
            }
            let part =
                json!({"type":"chunk","id":id,"index":index,"data":&rest[..end]}).to_string();
            tokio::time::timeout(Duration::from_secs(5), output.send(part)).await??;
            rest = &rest[end..];
            index += 1;
        }
        tokio::time::timeout(
            Duration::from_secs(5),
            output.send(json!({"type":"chunk_end","id":id,"count":index}).to_string()),
        )
        .await??;
    }
    Ok(())
}
