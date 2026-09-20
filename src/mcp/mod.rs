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
use rmcp::{
    ClientLifecycleMode, ClientServiceExt, Peer, RoleClient,
    model::{
        CallToolRequest, CallToolRequestParams, CancelledNotificationParam, ClientConfig,
        ClientRequest, ContentBlock, ProtocolVersion, ResourceContents, ServerResult,
    },
    service::{PeerRequestOptions, RunningService},
    transport::TokioChildProcess,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::{io::AsyncReadExt, sync::Mutex as AsyncMutex};

use crate::{
    config::{Config, McpServerConfig, McpTransport},
    runtime::ImageContent,
    tool::{Approval, Tool, ToolRegistry, ToolResource, ToolResult},
};

const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

type Service = RunningService<RoleClient, ClientConfig>;

pub async fn add_stdio_tools(registry: &mut ToolRegistry, config: &Config, root: &Path) {
    let servers = config
        .mcp
        .servers
        .iter()
        .filter(|(_, server)| server.enabled && server.transport == McpTransport::Stdio)
        .map(|(name, server)| {
            connect_stdio(
                name.clone(),
                server.clone(),
                root.to_path_buf(),
                config.tools.mcp,
            )
        });
    for result in futures_util::future::join_all(servers).await {
        match result {
            Ok(connected) => {
                let count = connected.tools.len();
                for tool in connected.tools {
                    let approval = tool.approval;
                    let approval_key =
                        format!("{}:{}", connected.connection.approval_prefix, tool.original);
                    if let Err(error) = registry.try_insert_with_key(tool, approval, approval_key) {
                        registry.notice(format!(
                            "MCP server '{}' skipped a tool: {error}",
                            connected.name
                        ));
                    }
                }
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
    let client = ClientConfig::default();
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
    let peer = service.peer().clone();
    let listed = tokio::time::timeout(
        Duration::from_secs(config.startup_timeout_secs),
        service.list_all_tools(),
    )
    .await
    .with_context(|| format!("MCP server '{name}' tool discovery timed out"))?
    .with_context(|| format!("list tools from MCP server '{name}'"))?;
    let filters = ToolFilters::new(&config.include_tools, &config.exclude_tools)
        .with_context(|| format!("compile tool filters for MCP server '{name}'"))?;
    let fingerprint = server_fingerprint(&name, &config, &cwd);
    let connection = Arc::new(McpConnection {
        peer,
        service: AsyncMutex::new(Some(service)),
        active: Mutex::new(HashMap::new()),
        call_timeout: Duration::from_secs(config.call_timeout_secs),
        approval_prefix: format!("mcp:{name}:{fingerprint}"),
    });
    let mut tools = Vec::new();
    let mut exposed = HashMap::new();
    for tool in listed {
        let original = tool.name.into_owned();
        if !filters.matches(&original) {
            continue;
        }
        let name_for_model = exposed_name(&name, &original);
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
            .unwrap_or(default_approval);
        tools.push(McpTool {
            name: name_for_model,
            original,
            description,
            schema,
            approval,
            connection: connection.clone(),
        });
    }
    Ok(ConnectedServer {
        name,
        connection,
        tools,
    })
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
    call_timeout: Duration,
    approval_prefix: String,
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
                if value.data.len() > MAX_IMAGE_BYTES * 4 / 3 + 4 {
                    parts.push("[MCP image omitted: exceeds 20 MiB]".into());
                    continue;
                }
                let bytes = STANDARD.decode(&value.data).context("decode MCP image")?;
                let dimensions = image::load_from_memory(&bytes)
                    .map(|image| (image.width(), image.height()))
                    .unwrap_or_default();
                image = Some(ImageContent {
                    mime_type: value.mime_type,
                    data: value.data,
                    path: None,
                    width: dimensions.0,
                    height: dimensions.1,
                });
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
                } => parts.push(format!(
                    "[MCP resource omitted: {uri}, {}, {} base64 characters]",
                    mime_type.unwrap_or_else(|| "application/octet-stream".into()),
                    blob.len()
                )),
                _ => parts.push("[unsupported MCP resource omitted]".into()),
            },
            ContentBlock::ResourceLink(resource) => parts.push(format!(
                "MCP resource: {} ({})",
                resource.name, resource.uri
            )),
            _ => parts.push("[unsupported MCP content omitted]".into()),
        }
    }
    if let Some(structured) = result.structured_content {
        let json = serde_json::to_string_pretty(&structured)?;
        if !parts.iter().any(|part| part.trim() == json) {
            parts.push(json);
        }
    }
    Ok(ToolResult {
        output: parts.join("\n\n"),
        is_error: result.is_error.unwrap_or(false),
        image,
        file: None,
        diff: None,
    })
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
    hash.update(&config.command);
    for arg in &config.args {
        hash.update([0]);
        hash.update(arg);
    }
    hash.update([0]);
    hash.update(cwd.as_os_str().as_encoded_bytes());
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
        registry.shutdown().await;
    }
}
