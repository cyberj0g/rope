use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use globset::{Glob, GlobSet, GlobSetBuilder};
use http::{HeaderName, HeaderValue};
use rmcp::{
    ClientHandler, ClientLifecycleMode, ClientServiceExt, Peer, RoleClient,
    model::{
        CallToolRequest, CallToolRequestParams, CancelledNotificationParam, ClientConfig,
        ClientRequest, ContentBlock, ProtocolVersion, ResourceContents, ServerResult,
    },
    service::{NotificationContext, PeerRequestOptions, RunningService},
    transport::{
        StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::{
    io::AsyncReadExt,
    sync::{Mutex as AsyncMutex, mpsc},
    task::JoinHandle,
};

use crate::{
    config::{Config, McpServerConfig, McpTransport},
    runtime::ImageContent,
    tool::{Approval, Tool, ToolRegistry, ToolResource, ToolResult},
};

const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

type Service = RunningService<RoleClient, McpClient>;

#[derive(Clone)]
struct McpClient {
    info: ClientConfig,
    refresh: mpsc::UnboundedSender<()>,
}

impl McpClient {
    fn new() -> (Self, mpsc::UnboundedReceiver<()>) {
        let (refresh, receiver) = mpsc::unbounded_channel();
        (
            Self {
                info: ClientConfig::default(),
                refresh,
            },
            receiver,
        )
    }
}

impl ClientHandler for McpClient {
    fn get_info(&self) -> ClientConfig {
        self.info.clone()
    }

    fn on_tool_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.refresh.send(()).ok();
        std::future::ready(())
    }
}

pub async fn add_tools(registry: &mut ToolRegistry, config: &Config, root: &Path) {
    let servers = config
        .mcp
        .servers
        .iter()
        .filter(|(_, server)| server.enabled)
        .map(|(name, server)| async move {
            match server.transport {
                McpTransport::Stdio => {
                    connect_stdio(
                        name.clone(),
                        server.clone(),
                        root.to_path_buf(),
                        config.tools.mcp,
                    )
                    .await
                }
                McpTransport::StreamableHttp => {
                    connect_http(
                        name.clone(),
                        server.clone(),
                        root.to_path_buf(),
                        config.tools.mcp,
                    )
                    .await
                }
            }
        });
    for result in futures_util::future::join_all(servers).await {
        match result {
            Ok(connected) => {
                let entries = connected
                    .tools
                    .into_iter()
                    .map(|tool| {
                        let approval = tool.approval;
                        let approval_key =
                            format!("{}:{}", connected.connection.approval_prefix, tool.original);
                        (tool, approval, approval_key, "mcp".to_owned())
                    })
                    .collect();
                let count = match registry.replace_origin(&connected.connection.origin, entries) {
                    Ok(count) => count,
                    Err(error) => {
                        registry.notice(format!(
                            "MCP server '{}' skipped its tools: {error}",
                            connected.name
                        ));
                        0
                    }
                };
                connected
                    .connection
                    .start_refresh(registry.clone(), connected.refresh);
                registry.add_resource(connected.connection);
                registry.notice(format!(
                    "MCP server '{}' connected with {count} tool{}",
                    connected.name,
                    if count == 1 { "" } else { "s" }
                ));
            }
            Err(error) => registry.notice(format!("MCP server unavailable: {error:#}")),
        }
    }
}

struct ConnectedServer {
    name: String,
    connection: Arc<McpConnection>,
    tools: Vec<McpTool>,
    refresh: mpsc::UnboundedReceiver<()>,
}

async fn connect_stdio(
    name: String,
    config: McpServerConfig,
    project_root: PathBuf,
    default_approval: Approval,
) -> Result<ConnectedServer> {
    let cwd = config
        .cwd
        .as_ref()
        .map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                project_root.join(path)
            }
        })
        .unwrap_or(project_root);
    let mut command = tokio::process::Command::new(&config.command);
    command.args(&config.args).current_dir(&cwd);
    for (key, value) in &config.env {
        command.env(key, value);
    }
    for (key, source) in &config.env_vars {
        let value = std::env::var(source)
            .with_context(|| format!("MCP server '{name}' needs environment variable {source}"))?;
        command.env(key, value);
    }
    let (transport, stderr) = TokioChildProcess::builder(command)
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("start MCP server '{name}'"))?;
    if let Some(mut stderr) = stderr {
        tokio::spawn(async move {
            let mut buffer = [0; 4096];
            while stderr.read(&mut buffer).await.is_ok_and(|read| read > 0) {}
        });
    }
    let (client, refresh) = McpClient::new();
    let service = tokio::time::timeout(
        Duration::from_secs(config.startup_timeout_secs),
        client.serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Auto {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                legacy_version: Some(ProtocolVersion::V_2025_11_25),
            },
        ),
    )
    .await
    .with_context(|| format!("MCP server '{name}' startup timed out"))?
    .with_context(|| format!("initialize MCP server '{name}'"))?;
    finish_connection(name, config, cwd, default_approval, service, refresh).await
}

async fn connect_http(
    name: String,
    config: McpServerConfig,
    project_root: PathBuf,
    default_approval: Approval,
) -> Result<ConnectedServer> {
    let mut headers = HashMap::new();
    for (key, value) in &config.headers {
        headers.insert(
            HeaderName::from_bytes(key.as_bytes())
                .with_context(|| format!("invalid HTTP header name for MCP server '{name}'"))?,
            HeaderValue::from_str(value)
                .with_context(|| format!("invalid HTTP header value for MCP server '{name}'"))?,
        );
    }
    for (key, source) in &config.header_env_vars {
        let value = std::env::var(source)
            .with_context(|| format!("MCP server '{name}' needs environment variable {source}"))?;
        headers.insert(
            HeaderName::from_bytes(key.as_bytes())
                .with_context(|| format!("invalid HTTP header name for MCP server '{name}'"))?,
            HeaderValue::from_str(&value)
                .with_context(|| format!("invalid HTTP header value for MCP server '{name}'"))?,
        );
    }
    let mut transport_config =
        StreamableHttpClientTransportConfig::with_uri(config.url.clone()).custom_headers(headers);
    if let Some(source) = &config.bearer_token_env {
        transport_config =
            transport_config.auth_header(std::env::var(source).with_context(|| {
                format!("MCP server '{name}' needs environment variable {source}")
            })?);
    }
    let transport = StreamableHttpClientTransport::from_config(transport_config);
    let (client, refresh) = McpClient::new();
    let service = tokio::time::timeout(
        Duration::from_secs(config.startup_timeout_secs),
        client.serve_with_lifecycle(
            transport,
            ClientLifecycleMode::Auto {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                legacy_version: Some(ProtocolVersion::V_2025_11_25),
            },
        ),
    )
    .await
    .with_context(|| format!("MCP server '{name}' startup timed out"))?
    .with_context(|| format!("initialize MCP server '{name}'"))?;
    finish_connection(
        name,
        config,
        project_root,
        default_approval,
        service,
        refresh,
    )
    .await
}

async fn finish_connection(
    name: String,
    config: McpServerConfig,
    identity_root: PathBuf,
    default_approval: Approval,
    service: Service,
    refresh: mpsc::UnboundedReceiver<()>,
) -> Result<ConnectedServer> {
    let peer = service.peer().clone();
    let listed = tokio::time::timeout(
        Duration::from_secs(config.startup_timeout_secs),
        service.list_all_tools(),
    )
    .await
    .with_context(|| format!("MCP server '{name}' tool discovery timed out"))?
    .with_context(|| format!("list tools from MCP server '{name}'"))?;
    let fingerprint = server_fingerprint(&name, &config, &identity_root);
    let connection = Arc::new(McpConnection {
        peer,
        service: AsyncMutex::new(Some(service)),
        active: Mutex::new(HashMap::new()),
        refresh_task: Mutex::new(None),
        call_timeout: Duration::from_secs(config.call_timeout_secs),
        approval_prefix: format!("mcp:{name}:{fingerprint}"),
        origin: format!("mcp:{name}"),
        name: name.clone(),
        config: config.clone(),
        default_approval,
    });
    let tools = connection.build_tools(listed)?;
    Ok(ConnectedServer {
        name,
        connection,
        tools,
        refresh,
    })
}

impl McpConnection {
    fn build_tools(self: &Arc<Self>, listed: Vec<rmcp::model::Tool>) -> Result<Vec<McpTool>> {
        let name = &self.name;
        let config = &self.config;
        let filters = ToolFilters::new(&config.include_tools, &config.exclude_tools)
            .with_context(|| format!("compile tool filters for MCP server '{name}'"))?;
        let mut tools = Vec::new();
        let mut exposed = HashMap::new();
        for tool in listed {
            let original = tool.name.into_owned();
            if !filters.matches(&original) {
                continue;
            }
            let name_for_model = exposed_name(name, &original);
            if let Some(other) = exposed.insert(name_for_model.clone(), original.clone()) {
                bail!(
                    "MCP server '{name}' tools '{other}' and '{original}' map to the same model name"
                );
            }
            let description = tool
                .description
                .map(|description| description.into_owned())
                .unwrap_or_else(|| format!("Tool from MCP server {name}"));
            let schema = Value::Object((*tool.input_schema).clone());
            let approval = config
                .tools
                .get(&original)
                .copied()
                .or(config.approval)
                .unwrap_or(self.default_approval);
            tools.push(McpTool {
                name: name_for_model,
                original,
                description,
                schema,
                approval,
                connection: self.clone(),
            });
        }
        Ok(tools)
    }

    fn start_refresh(
        self: &Arc<Self>,
        registry: ToolRegistry,
        mut notifications: mpsc::UnboundedReceiver<()>,
    ) {
        let connection = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            while notifications.recv().await.is_some() {
                while notifications.try_recv().is_ok() {}
                let Some(connection) = connection.upgrade() else {
                    break;
                };
                let listed = tokio::time::timeout(
                    Duration::from_secs(connection.config.startup_timeout_secs),
                    connection.peer.list_all_tools(),
                )
                .await;
                let Ok(Ok(listed)) = listed else {
                    continue;
                };
                let Ok(tools) = connection.build_tools(listed) else {
                    continue;
                };
                let entries = tools
                    .into_iter()
                    .map(|tool| {
                        let approval = tool.approval;
                        let approval_key =
                            format!("{}:{}", connection.approval_prefix, tool.original);
                        (tool, approval, approval_key, "mcp".to_owned())
                    })
                    .collect();
                registry.replace_origin(&connection.origin, entries).ok();
            }
        });
        *self.refresh_task.lock().unwrap() = Some(task);
    }
}

struct ToolFilters {
    include: Option<GlobSet>,
    exclude: GlobSet,
}

impl ToolFilters {
    fn new(include: &[String], exclude: &[String]) -> Result<Self> {
        Ok(Self {
            include: (!include.is_empty())
                .then(|| glob_set(include))
                .transpose()?,
            exclude: glob_set(exclude)?,
        })
    }

    fn matches(&self, name: &str) -> bool {
        self.include
            .as_ref()
            .is_none_or(|include| include.is_match(name))
            && !self.exclude.is_match(name)
    }
}

fn glob_set(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern)?);
    }
    Ok(builder.build()?)
}

pub struct McpTool {
    name: String,
    original: String,
    description: String,
    schema: Value,
    approval: Approval,
    connection: Arc<McpConnection>,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    async fn run(&self, args: Value) -> Result<ToolResult> {
        self.connection.call(&self.original, args).await
    }
}

struct McpConnection {
    peer: Peer<RoleClient>,
    service: AsyncMutex<Option<Service>>,
    active: Mutex<HashMap<rmcp::model::RequestId, ()>>,
    refresh_task: Mutex<Option<JoinHandle<()>>>,
    call_timeout: Duration,
    approval_prefix: String,
    origin: String,
    name: String,
    config: McpServerConfig,
    default_approval: Approval,
}

impl McpConnection {
    async fn call(&self, tool: &str, args: Value) -> Result<ToolResult> {
        let arguments = match args {
            Value::Object(arguments) => arguments,
            _ => bail!("MCP tool arguments must be a JSON object"),
        };
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(
            CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments),
        ));
        let handle = self
            .peer
            .send_request_with_option(request, PeerRequestOptions::with_timeout(self.call_timeout))
            .await
            .with_context(|| format!("call MCP tool {tool}"))?;
        let request_id = handle.id.clone();
        self.active.lock().unwrap().insert(request_id.clone(), ());
        let response = handle.await_response().await;
        self.active.lock().unwrap().remove(&request_id);
        let result = match response.with_context(|| format!("call MCP tool {tool}"))? {
            ServerResult::CallToolResult(result) => result,
            _ => bail!("MCP tool {tool} returned an unsupported response"),
        };
        convert_result(result)
    }
}

#[async_trait]
impl ToolResource for McpConnection {
    async fn cancel_active(&self) {
        let requests = self
            .active
            .lock()
            .unwrap()
            .drain()
            .map(|(id, ())| id)
            .collect::<Vec<_>>();
        for request_id in requests {
            self.peer
                .notify_cancelled(CancelledNotificationParam::new(
                    Some(request_id),
                    Some("Rope turn cancelled".into()),
                ))
                .await
                .ok();
        }
    }

    async fn shutdown(&self) {
        self.cancel_active().await;
        let refresh_task = self.refresh_task.lock().unwrap().take();
        if let Some(task) = refresh_task {
            task.abort();
            task.await.ok();
        }
        if let Some(service) = self.service.lock().await.as_mut() {
            service
                .close_with_timeout(Duration::from_secs(5))
                .await
                .ok();
        }
        self.service.lock().await.take();
    }
}

fn convert_result(result: rmcp::model::CallToolResult) -> Result<ToolResult> {
    let mut parts = Vec::new();
    let mut image = None;
    for content in result.content {
        match content {
            ContentBlock::Text(text) => parts.push(text.text),
            ContentBlock::Image(value) if image.is_none() => {
                image = decode_image(value.mime_type, value.data, &mut parts)?;
            }
            ContentBlock::Image(_) => parts.push("[additional MCP image omitted]".into()),
            ContentBlock::Audio(value) => parts.push(format!(
                "[MCP audio omitted: {}, {} base64 characters]",
                value.mime_type,
                value.data.len()
            )),
            ContentBlock::Resource(resource) => match resource.resource {
                ResourceContents::TextResourceContents { uri, text, .. } => {
                    parts.push(format!("Resource {uri}:\n{text}"))
                }
                ResourceContents::BlobResourceContents {
                    uri,
                    mime_type,
                    blob,
                    ..
                } => {
                    let mime_type = mime_type.unwrap_or_else(|| "application/octet-stream".into());
                    if image.is_none() && mime_type.starts_with("image/") {
                        image = decode_image(mime_type, blob, &mut parts)?;
                        parts.push(format!("MCP image resource: {uri}"));
                    } else {
                        parts.push(format!(
                            "[MCP resource omitted: {uri}, {mime_type}, {} base64 characters]",
                            blob.len()
                        ));
                    }
                }
                _ => parts.push("[unsupported MCP resource omitted]".into()),
            },
            ContentBlock::ResourceLink(resource) => {
                let mut details = Vec::new();
                if let Some(mime_type) = resource.mime_type {
                    details.push(mime_type);
                }
                if let Some(size) = resource.size {
                    details.push(format!("{size} bytes"));
                }
                let title = resource.title.unwrap_or(resource.name);
                let suffix = (!details.is_empty())
                    .then(|| format!(" [{}]", details.join(", ")))
                    .unwrap_or_default();
                let description = resource
                    .description
                    .map(|description| format!("\n{description}"))
                    .unwrap_or_default();
                parts.push(format!(
                    "MCP resource: {title} ({}){suffix}{description}",
                    resource.uri
                ));
            }
            _ => parts.push("[unsupported MCP content omitted]".into()),
        }
    }
    if let Some(structured) = result.structured_content {
        let json = serde_json::to_string_pretty(&structured)?;
        if !parts.iter().any(|part| {
            part.trim() == json
                || serde_json::from_str::<Value>(part).is_ok_and(|value| value == structured)
        }) {
            parts.push(json);
        }
    }
    if parts.is_empty() && result.is_error == Some(true) {
        parts.push("MCP tool returned an error without details".into());
    }
    Ok(ToolResult {
        output: parts.join("\n\n"),
        is_error: result.is_error.unwrap_or(false),
        image,
        file: None,
        diff: None,
    })
}

fn decode_image(
    mime_type: String,
    data: String,
    parts: &mut Vec<String>,
) -> Result<Option<ImageContent>> {
    if data.len() > MAX_IMAGE_BYTES * 4 / 3 + 4 {
        parts.push("[MCP image omitted: exceeds 20 MiB]".into());
        return Ok(None);
    }
    let bytes = STANDARD.decode(&data).context("decode MCP image")?;
    if bytes.len() > MAX_IMAGE_BYTES {
        parts.push("[MCP image omitted: exceeds 20 MiB]".into());
        return Ok(None);
    }
    let dimensions = image::load_from_memory(&bytes)
        .map(|image| (image.width(), image.height()))
        .unwrap_or_default();
    Ok(Some(ImageContent {
        mime_type,
        data,
        path: None,
        width: dimensions.0,
        height: dimensions.1,
    }))
}

fn exposed_name(server: &str, tool: &str) -> String {
    let raw = format!("mcp__{}__{}", slug(server), slug(tool));
    if raw.len() <= 64 {
        return raw;
    }
    let hash = fnv1a(raw.as_bytes());
    format!("{}_{hash:08x}", &raw[..55])
}

fn slug(value: &str) -> String {
    let mut output = String::new();
    let mut underscore = false;
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'-' {
            output.push(byte as char);
            underscore = false;
        } else if !underscore {
            output.push('_');
            underscore = true;
        }
    }
    output.trim_matches('_').to_owned()
}

fn fnv1a(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c9dc5, |hash, byte| {
        (hash ^ *byte as u32).wrapping_mul(0x01000193)
    })
}

fn server_fingerprint(name: &str, config: &McpServerConfig, cwd: &Path) -> String {
    let mut hash = Sha256::new();
    hash.update(name);
    hash.update([0]);
    hash.update(format!("{:?}", config.transport));
    hash.update([0]);
    hash.update(&config.command);
    for arg in &config.args {
        hash.update([0]);
        hash.update(arg);
    }
    hash.update([0]);
    hash.update(cwd.as_os_str().as_encoded_bytes());
    hash.update([0]);
    hash.update(&config.url);
    if let Some(source) = &config.bearer_token_env {
        hash.update([0]);
        hash.update(source);
    }
    for (key, value) in &config.headers {
        hash.update([0]);
        hash.update(key);
        hash.update([0]);
        hash.update(value);
    }
    for (key, source) in &config.header_env_vars {
        hash.update([0]);
        hash.update(key);
        hash.update([0]);
        hash.update(source);
    }
    for (key, value) in &config.env {
        hash.update([0]);
        hash.update(key);
        hash.update([0]);
        hash.update(value);
    }
    for (key, source) in &config.env_vars {
        hash.update([0]);
        hash.update(key);
        hash.update([0]);
        hash.update(source);
    }
    hash.finalize()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, tool};

    #[test]
    fn exposed_names_are_provider_safe_and_bounded() {
        let name = exposed_name("my server", &"strange.tool/".repeat(10));
        assert!(name.len() <= 64);
        assert!(
            name.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        );
        assert_eq!(name, exposed_name("my server", &"strange.tool/".repeat(10)));
    }

    #[test]
    fn preserves_structured_resources_and_embedded_images() {
        let result = serde_json::from_value(serde_json::json!({
            "content": [
                { "type": "text", "text": "{\"answer\":42}" },
                {
                    "type": "resource_link",
                    "uri": "file:///report.txt",
                    "name": "report",
                    "title": "Report",
                    "description": "Generated report",
                    "mimeType": "text/plain",
                    "size": 12
                },
                {
                    "type": "resource",
                    "resource": {
                        "uri": "file:///pixel.png",
                        "mimeType": "image/png",
                        "blob": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="
                    }
                }
            ],
            "structuredContent": { "answer": 42 },
            "isError": false
        }))
        .unwrap();

        let converted = convert_result(result).unwrap();

        assert!(converted.image.is_some());
        assert!(
            converted
                .output
                .contains("Report (file:///report.txt) [text/plain, 12 bytes]")
        );
        assert!(converted.output.contains("Generated report"));
        assert!(
            converted
                .output
                .contains("MCP image resource: file:///pixel.png")
        );
        assert_eq!(converted.output.matches("answer").count(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn discovers_and_calls_a_real_stdio_server() {
        let mut config = Config::default();
        config.mcp.servers.insert(
            "fixture".into(),
            McpServerConfig {
                command: "sh".into(),
                args: vec![
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures/mcp_server.sh")
                        .to_string_lossy()
                        .into_owned(),
                ],
                ..McpServerConfig::default()
            },
        );

        let registry = tool::discover_at(&config, Path::new(env!("CARGO_MANIFEST_DIR")))
            .await
            .unwrap();
        let entry = registry.get("mcp__fixture__echo").unwrap();
        let result = entry
            .tool
            .run(serde_json::json!({ "value": "hello" }))
            .await
            .unwrap();

        assert_eq!(result.output, "echo: hello");
        assert!(!result.is_error);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if registry.get("mcp__fixture__new_echo").is_ok() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(registry.get("mcp__fixture__echo").is_err());
        registry.shutdown().await;
    }

    #[tokio::test]
    async fn discovers_and_calls_a_streamable_http_server_with_env_auth() {
        use axum::{
            Json, Router,
            extract::State,
            http::{HeaderMap, StatusCode},
            response::{IntoResponse, Response},
            routing::post,
        };

        #[derive(Clone)]
        struct ExpectedHeaders {
            authorization: String,
            home: String,
        }

        async fn endpoint(
            State(expected): State<ExpectedHeaders>,
            headers: HeaderMap,
            Json(request): Json<Value>,
        ) -> Response {
            assert_eq!(
                headers.get("authorization").unwrap(),
                expected.authorization.as_str()
            );
            assert_eq!(headers.get("x-rope-test").unwrap(), expected.home.as_str());
            let method = request["method"].as_str().unwrap();
            let Some(id) = request.get("id").cloned() else {
                return StatusCode::ACCEPTED.into_response();
            };
            let body = match method {
                "server/discover" => serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": { "code": -32601, "message": "not found" }
                }),
                "initialize" => serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "protocolVersion": "2025-11-25",
                        "capabilities": { "tools": { "listChanged": false } },
                        "serverInfo": { "name": "fixture", "version": "1" }
                    }
                }),
                "tools/list" => serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "tools": [{
                        "name": "echo", "description": "Echo a value",
                        "inputSchema": { "type": "object" }
                    }] }
                }),
                "tools/call" => serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": "http echo" }],
                        "isError": false
                    }
                }),
                _ => unreachable!("unexpected MCP method: {method}"),
            };
            Json(body).into_response()
        }

        let user = std::env::var("USER").unwrap();
        let home = std::env::var("HOME").unwrap();
        let app = Router::new()
            .route("/mcp", post(endpoint))
            .with_state(ExpectedHeaders {
                authorization: format!("Bearer {user}"),
                home,
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut config = Config::default();
        let mut server_config = McpServerConfig {
            transport: McpTransport::StreamableHttp,
            url: format!("http://{address}/mcp"),
            bearer_token_env: Some("USER".into()),
            ..McpServerConfig::default()
        };
        server_config
            .header_env_vars
            .insert("x-rope-test".into(), "HOME".into());
        config.mcp.servers.insert("remote".into(), server_config);

        let registry = tool::discover_at(&config, Path::new(env!("CARGO_MANIFEST_DIR")))
            .await
            .unwrap();
        let result = registry
            .get("mcp__remote__echo")
            .unwrap()
            .tool
            .run(serde_json::json!({}))
            .await
            .unwrap();

        assert_eq!(result.output, "http echo");
        registry.shutdown().await;
        server.abort();
    }
}
