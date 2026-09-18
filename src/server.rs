use anyhow::{Context, Result, bail};
use axum::{
    Router,
    body::Bytes,
    extract::{
        ConnectInfo, DefaultBodyLimit, Path, Query, State, WebSocketUpgrade,
        connect_info::Connected,
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
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncReadExt,
    net::TcpListener,
    sync::{Mutex as AsyncMutex, Semaphore, mpsc, watch},
    task::JoinHandle,
};

use crate::{
    core::Core,
    protocol::{self, ClientRequest, Hello, Request},
    runtime::MAX_FILE_BYTES,
};

const CHUNK_BYTES: usize = 32 * 1024;
const REPLY_CACHE: usize = 128;
/// How many catalog rows a web client loads at once before asking for more.
const CATALOG_PAGE: usize = 20;

struct ClientHistory {
    highest: u64,
    replies: VecDeque<(u64, String, Value)>,
}
struct ClientRecord {
    seen: Mutex<Instant>,
    history: AsyncMutex<ClientHistory>,
}

/// A web client's window onto the session catalog: the rows it has loaded
/// plus the server-side filter applied to *all* sessions, not just these.
struct CatalogView {
    query: Option<String>,
    offset: usize,
}

impl CatalogView {
    fn new() -> Self {
        Self {
            query: None,
            offset: CATALOG_PAGE,
        }
    }

    /// The rows this view currently shows, newest first.
    fn page<'a>(&self, sessions: &'a [crate::protocol::CatalogEntry]) -> Vec<&'a crate::protocol::CatalogEntry> {
        let filtered = filter_sessions(sessions, self.query.as_deref());
        filtered.into_iter().take(self.offset).collect()
    }

    /// How many rows match the filter in total (for the "load more" affordance).
    fn total(&self, sessions: &[crate::protocol::CatalogEntry]) -> usize {
        filter_sessions(sessions, self.query.as_deref()).len()
    }
}

/// The sidebar filter searches every session on the server — name, generated
/// title, or first user message — case-insensitively, not only the rows the
/// client has already loaded.
fn filter_sessions<'a>(
    sessions: &'a [crate::protocol::CatalogEntry],
    query: Option<&str>,
) -> Vec<&'a crate::protocol::CatalogEntry> {
    let Some(query) = query.map(str::trim).filter(|q| !q.is_empty()) else {
        return sessions.iter().collect();
    };
    let needle = query.to_lowercase();
    sessions
        .iter()
        .filter(|entry| {
            entry.info.name.to_lowercase().contains(&needle)
                || entry
                    .info
                    .title
                    .as_deref()
                    .is_some_and(|t| t.to_lowercase().contains(&needle))
                || entry
                    .info
                    .first_message
                    .as_deref()
                    .is_some_and(|m| m.to_lowercase().contains(&needle))
        })
        .collect()
}

/// Per-connection content redaction. Collapsed thinking and tool blocks are
/// delivered without their content — the client keeps the header (tool name,
/// status, live timer), which is enough to show work in progress. Once a
/// client reveals a block it receives the full content and its live updates
/// for the rest of the connection.
struct Redactor {
    revealed: HashSet<String>,
    kinds: HashMap<String, &'static str>,
}

impl Redactor {
    fn new() -> Self {
        Self {
            revealed: HashSet::new(),
            kinds: HashMap::new(),
        }
    }

    fn revealed(&mut self, block_id: &str) {
        self.revealed.insert(block_id.to_owned());
    }

    /// The snapshot a new subscription starts with, redacted.
    fn snapshot(&mut self, snapshot: &crate::core::state::Snapshot) -> Value {
        for block in &snapshot.blocks {
            self.kinds.insert(block.id.clone(), kind_str(block.kind));
        }
        let mut value = serde_json::to_value(snapshot).unwrap();
        for block in value
            .get_mut("blocks")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
            if !self.revealed.contains(id) {
                redact_block(block.as_object_mut().unwrap());
            }
        }
        value
    }

    /// One sequenced update, with hidden appends suppressed. The event is
    /// always forwarded (even with no changes) so sequence numbers stay
    /// contiguous on the client.
    fn update(&mut self, update: &crate::core::Update) -> Value {
        let changes = update
            .changes
            .iter()
            .filter_map(|change| {
                let mut value = serde_json::to_value(change).unwrap();
                match change {
                    crate::core::state::Change::Insert { block, .. }
                    | crate::core::state::Change::Replace { block } => {
                        let kind = kind_str(block.kind);
                        self.kinds.insert(block.id.clone(), kind);
                        if !self.revealed.contains(&block.id)
                            && let Some(block) = value.get_mut("block")
                        {
                            redact_block(block.as_object_mut().unwrap());
                        }
                        Some(value)
                    }
                    crate::core::state::Change::Append {
                        block_id, field, ..
                    } => {
                        let hidden = match (self.kinds.get(block_id).copied(), field.as_str()) {
                            (Some("thinking"), "content") => true,
                            (Some("tool"), "arguments" | "output") => true,
                            _ => false,
                        };
                        if hidden && !self.revealed.contains(block_id) {
                            None
                        } else {
                            Some(value)
                        }
                    }
                    crate::core::state::Change::State { .. }
                    | crate::core::state::Change::Plan { .. }
                    | crate::core::state::Change::Project { .. } => Some(value),
                }
            })
            .collect::<Vec<_>>();
        json!({
            "session_id": update.session_id,
            "seq": update.seq,
            "changes": changes,
        })
    }
}

/// Blanks the withheld fields of one block, flagging it so the client knows
/// to request the content on first expand. Images and published files are
/// kept: they render outside the collapsed section.
fn redact_block(block: &mut serde_json::Map<String, Value>) {
    let kind = block.get("kind").and_then(Value::as_str);
    match kind {
        Some("thinking") => {
            block.insert("content".to_owned(), Value::String(String::new()));
            block.insert("redacted".to_owned(), Value::Bool(true));
        }
        Some("tool") => {
            if let Some(tool) = block.get_mut("tool").and_then(Value::as_object_mut) {
                tool.insert("arguments".to_owned(), Value::String(String::new()));
                tool.insert("output".to_owned(), Value::Null);
                tool.insert("diff".to_owned(), Value::Null);
                tool.insert("redacted".to_owned(), Value::Bool(true));
            }
        }
        _ => {}
    }
}

fn kind_str(kind: crate::core::state::BlockKind) -> &'static str {
    use crate::core::state::BlockKind::*;
    match kind {
        User => "user",
        Steer => "steer",
        Assistant => "assistant",
        Status => "status",
        System => "system",
        Error => "error",
        Thinking => "thinking",
        Tool => "tool",
    }
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
            .route("/api/sessions/{session}/files/{block}", get(download_file))
            .layer(middleware::from_fn_with_state(app.clone(), authorize_http));
        let router = Router::new()
            .route("/ws", get(upgrade))
            .route(
                "/",
                get(|| async {
                    axum::response::Html(include_str!("../web/index.html"))
                }),
            )
            .route("/assets/{*path}", get(web_asset))
            .merge(attachments)
            .layer(DefaultBodyLimit::max(MAX_FILE_BYTES as usize))
            .with_state(app);
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<LocalEndpoint>(),
            )
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

async fn web_asset(Path(path): Path<String>) -> Response {
    let body = match path.as_str() {
        "styles.css" => include_str!("../web/styles.css"),
        "js/app.js" => include_str!("../web/js/app.js"),
        "js/attachments.js" => include_str!("../web/js/attachments.js"),
        "js/raw.js" => include_str!("../web/js/raw.js"),
        "js/chat.js" => include_str!("../web/js/chat.js"),
        "js/composer.js" => include_str!("../web/js/composer.js"),
        "js/files.js" => include_str!("../web/js/files.js"),
        "js/helpers.js" => include_str!("../web/js/helpers.js"),
        "js/markdown.js" => include_str!("../web/js/markdown.js"),
        "js/panels.js" => include_str!("../web/js/panels.js"),
        "js/protocol.js" => include_str!("../web/js/protocol.js"),
        "js/search.js" => include_str!("../web/js/search.js"),
        "js/state.js" => include_str!("../web/js/state.js"),
        "js/status.js" => include_str!("../web/js/status.js"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let content_type = if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else {
        "text/javascript; charset=utf-8"
    };
    ([(axum::http::header::CONTENT_TYPE, content_type)], body).into_response()
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

/// The local (server-side) address this connection arrived on. With a
/// wildcard bind this is the concrete interface address the client dialed —
/// exactly what a page served from here reports in its Origin header.
#[derive(Clone, Copy, Default)]
struct LocalEndpoint(Option<std::net::SocketAddr>);

impl Connected<axum::serve::IncomingStream<'_, TcpListener>> for LocalEndpoint {
    fn connect_info(stream: axum::serve::IncomingStream<'_, TcpListener>) -> Self {
        Self(stream.io().local_addr().ok())
    }
}

/// An Origin is allowed when it is on the explicit list or when it is
/// same-origin: equal to the address this connection actually arrived on.
/// Browsers set Origin from the page's real URL and web content cannot
/// forge it, so a same-origin page is trusted without enumeration;
/// cross-origin pages (including attacker pages) never match.
fn origin_allowed(allowed: &[String], origin: &str, local: Option<std::net::SocketAddr>) -> bool {
    allowed.iter().any(|a| a == origin)
        || local.is_some_and(|local| origin == format!("http://{local}"))
}

fn valid_origin(app: &App, headers: &HeaderMap, local: Option<std::net::SocketAddr>) -> bool {
    headers.get("origin").is_none_or(|origin| {
        origin
            .to_str()
            .is_ok_and(|origin| origin_allowed(&app.origins, origin, local))
    })
}

async fn upgrade(
    State(app): State<App>,
    ConnectInfo(local): ConnectInfo<LocalEndpoint>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !valid_origin(&app, &headers, local.0) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(permit) = app.connections.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    ws.max_message_size(protocol::MAX_COMMAND_BYTES)
        .max_frame_size(protocol::MAX_COMMAND_BYTES)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            crate::logging::write("INFO", "server", "WebSocket connected");
            if let Err(error) = connection(socket, app).await {
                crate::logging::write("WARN", "server", format_args!("WebSocket error: {error:#}"));
            }
            crate::logging::write("INFO", "server", "WebSocket disconnected");
        })
        .into_response()
}

async fn authorize_http(
    State(app): State<App>,
    ConnectInfo(local): ConnectInfo<LocalEndpoint>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if !valid_origin(&app, request.headers(), local.0) {
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

#[derive(serde::Deserialize)]
struct UploadQuery {
    filename: Option<String>,
}

async fn upload(State(app): State<App>, Path(session): Path<String>, Query(query): Query<UploadQuery>, bytes: Bytes) -> Response {
    let result = match query.filename {
        Some(name) => app.core.upload(&session, &name, &bytes).await,
        None => app.core.attach(&session, &bytes).await.and_then(|image| Ok(serde_json::to_value(image)?)),
    };
    match result {
        Ok(attachment) => axum::Json(attachment).into_response(),
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

async fn download_file(
    State(app): State<App>,
    Path((session, block)): Path<(String, String)>,
) -> Response {
    let file = match app.core.published_file(&session, &block).await {
        Ok(file) => file,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let metadata = match tokio::fs::metadata(&file.path).await {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    if metadata.len() > MAX_FILE_BYTES {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let source = match tokio::fs::File::open(&file.path).await {
        Ok(source) => source,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut bytes = Vec::new();
    if source
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let mut headers = HeaderMap::new();
    let Ok(content_type) = HeaderValue::from_str(&file.mime_type) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    headers.insert("content-type", content_type);
    headers.insert("cache-control", "private, no-store".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    if !file.mime_type.starts_with("image/") {
        // percent-encode UTF-8 bytes so filenames cannot alter the header
        let filename: String = file
            .name
            .bytes()
            .map(|byte| format!("%{byte:02X}"))
            .collect();
        headers.insert(
            "content-disposition",
            HeaderValue::from_str(&format!(
                "attachment; filename=download; filename*=UTF-8''{filename}"
            ))
            .unwrap(),
        );
    }
    (headers, bytes).into_response()
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
        crate::logging::write(
            "WARN",
            "server",
            "WebSocket rejected: invalid token or protocol version",
        );
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
    crate::logging::write(
        "INFO",
        "server",
        format_args!("client authenticated: {client_id}"),
    );
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
    let redactor = Arc::new(Mutex::new(Redactor::new()));
    let catalog_view = Arc::new(Mutex::new(CatalogView::new()));
    let (catalog, mut catalogs) = app.core.subscribe_catalog();
    let initial_page = {
        let view = catalog_view.lock().unwrap();
        let total = view.total(&catalog.sessions);
        let sessions: Vec<_> = view.page(&catalog.sessions).iter().cloned().collect();
        (total, sessions)
    };
    enqueue(
        &output,
        &json!({"type":"catalog","catalog":{"seq":catalog.seq,"sessions":initial_page.1,"total":initial_page.0}}),
    )
    .await?;
    let out = output.clone();
    let cancel = failed.clone();
    let core = app.core.clone();
    let catalog_state = catalog_view.clone();
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
            let page = {
                let view = catalog_state.lock().unwrap();
                let total = view.total(&catalog.sessions);
                let sessions: Vec<_> = view.page(&catalog.sessions).iter().cloned().collect();
                (total, sessions)
            };
            if enqueue(
                &out,
                &json!({"type":"catalog","catalog":{"seq":catalog.seq,"sessions":page.1,"total":page.0}}),
            )
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
                let mutation = matches!(
                    request.request,
                    Request::CreateSession { .. }
                        | Request::Command { .. }
                        | Request::DeleteSession { .. }
                );
                let reply = if let Some(number) = number {
                    if mutation && number <= history.highest {
                        match history.replies.iter().find(|(id, _, _)| *id == number) {
                            Some((_, original, reply)) if *original == payload => reply.clone(),
                            Some(_) => error_reply(&request.request_id, "request_id_reused", "request ID was used with another payload"),
                            None => error_reply(&request.request_id, "expired_request", "the original outcome is no longer cached; reconcile state before retrying"),
                        }
                    } else {
                        if mutation { history.highest = number; }
                        let result = dispatch(&app.core, request.request, &output, &failed, &mut tasks.subscriptions, &redactor, &catalog_view).await;
                        let reply = match result {
                            Ok(value) => json!({"type":"reply","request_id":request.request_id,"result":value}),
                            Err(error) => {
                                crate::logging::write("WARN", "server", format_args!("client {client_id} request {} failed ({}): {}", request.request_id, error.code, error.message));
                                error_reply(&request.request_id, &error.code, &error.message)
                            },
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
    redactor: &Arc<Mutex<Redactor>>,
    catalog_view: &Arc<Mutex<CatalogView>>,
) -> protocol::Result<Value> {
    let error = |e: anyhow::Error| protocol::Error::new("core", format!("{e:#}"));
    Ok(match request {
        Request::CreateSession { name } => {
            json!({"session_id":core.create(name).await.map_err(error)?})
        }
        Request::DeleteSession { session_id } => {
            core.delete(&session_id).await?;
            // This connection's live subscription to the deleted session is
            // dead; drop it so no resync is attempted.
            if let Some(task) = subscriptions.remove(&session_id) {
                task.abort();
            }
            json!({})
        }
        Request::Command { session_id, action } => {
            serde_json::to_value(core.command(&session_id, action).await?).unwrap()
        }
        Request::GitDiff { path } => {
            json!({"path":path,"content":core.diff(path.as_deref().map(std::path::Path::new)).await.map_err(error)?})
        }
        Request::CatalogView { query, offset } => {
            let mut view = catalog_view.lock().unwrap();
            view.query = query;
            view.offset = offset.min(1_000);
            drop(view);
            let (snapshot, _) = core.subscribe_catalog();
            let view = catalog_view.lock().unwrap();
            let total = view.total(&snapshot.sessions);
            let sessions: Vec<_> = view.page(&snapshot.sessions).iter().cloned().collect();
            json!({"sessions":sessions,"total":total})
        }
        Request::RawRequest {
            session_id,
            block_id,
        } => {
            json!({"body": core.raw_request(&session_id, &block_id).await?})
        }
        Request::RevealBlock {
            session_id,
            block_id,
        } => {
            let block = core.block(&session_id, &block_id).await?;
            redactor.lock().unwrap().revealed(&block_id);
            json!({"block":block})
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
            let snapshot = redactor.lock().unwrap().snapshot(&subscription.snapshot);
            enqueue(output, &json!({"type":"snapshot","snapshot":snapshot}))
                .await
                .map_err(error)?;
            let output = output.clone();
            let failed = failed.clone();
            let id = session_id.clone();
            let redactor = redactor.clone();
            subscriptions.insert(
                session_id,
                tokio::spawn(async move {
                    loop {
                        let message = match subscription.updates.recv().await {
                            Ok(update) => {
                                json!({"type":"event","update":redactor.lock().unwrap().update(&update)})
                            }
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

#[cfg(test)]
mod tests {
    use super::origin_allowed;
    use std::net::SocketAddr;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn explicit_list_and_same_origin_are_allowed() {
        let allowed = vec!["https://proxy.example".to_owned()];
        let lan = addr("192.168.1.50:8787");
        // An explicit list entry always matches.
        assert!(origin_allowed(&allowed, "https://proxy.example", None));
        // A page served from the address the connection arrived on is same-origin.
        assert!(origin_allowed(&[], "http://192.168.1.50:8787", Some(lan)));
        assert!(origin_allowed(
            &[],
            "http://[::1]:8787",
            Some(addr("[::1]:8787"))
        ));
    }

    #[test]
    fn cross_origin_never_matches_the_local_address() {
        let lan = addr("192.168.1.50:8787");
        assert!(!origin_allowed(&[], "https://untrusted.example", Some(lan)));
        assert!(!origin_allowed(&[], "http://192.168.1.50:9999", Some(lan)));
        assert!(!origin_allowed(&[], "http://other-host:8787", Some(lan)));
        // Without a local address only the explicit list counts.
        assert!(!origin_allowed(&[], "http://192.168.1.50:8787", None));
    }
}
