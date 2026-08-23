mod builtin;
mod external;

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;
use builtin::{EditTool, GlobTool, GrepTool, ReadTool, ShellTool, WriteTool};
use external::ExternalTool;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Approval {
    Allow,
    Ask,
    Deny,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolResult {
    pub output: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolDefinition {
    pub r#type: &'static str,
    pub function: FunctionDefinition,
}

#[derive(Clone, Debug, Serialize)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> Value;
    async fn run(&self, args: Value) -> Result<ToolResult>;
}

#[derive(Clone)]
pub struct ToolEntry {
    pub tool: Arc<dyn Tool>,
    pub approval: Approval,
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, ToolEntry>,
}

impl ToolRegistry {
    pub fn insert<T: Tool + 'static>(&mut self, tool: T, approval: Approval) {
        self.tools.insert(
            tool.name().to_owned(),
            ToolEntry {
                tool: Arc::new(tool),
                approval,
            },
        );
    }

    pub fn get(&self, name: &str) -> Result<&ToolEntry> {
        self.tools
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool: {name}"))
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .map(|entry| ToolDefinition {
                r#type: "function",
                function: FunctionDefinition {
                    name: entry.tool.name().to_owned(),
                    description: entry.tool.description().to_owned(),
                    parameters: entry.tool.schema(),
                },
            })
            .collect()
    }
}

pub async fn discover(config: &Config) -> Result<ToolRegistry> {
    let cwd = std::env::current_dir()?;
    let mut registry = ToolRegistry::default();
    registry.insert(ReadTool(cwd.clone()), config.tools.read);
    registry.insert(WriteTool(cwd.clone()), config.tools.write);
    registry.insert(EditTool(cwd.clone()), config.tools.edit);
    registry.insert(ShellTool(cwd.clone()), config.tools.shell);
    registry.insert(GrepTool(cwd.clone()), config.tools.grep);
    registry.insert(GlobTool(cwd.clone()), config.tools.glob);

    if let Some(global) =
        directories::BaseDirs::new().map(|dirs| dirs.config_dir().join("rope/tools"))
    {
        add_external(&mut registry, global, config.tools.external).await?;
    }
    add_external(
        &mut registry,
        cwd.join(".rope/tools"),
        config.tools.external,
    )
    .await?;
    Ok(registry)
}

async fn add_external(
    registry: &mut ToolRegistry,
    directory: PathBuf,
    approval: Approval,
) -> Result<()> {
    let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
        return Ok(());
    };
    while let Some(entry) = entries.next_entry().await? {
        let metadata = entry.metadata().await?;
        if !metadata.is_file() || !is_executable(&metadata) {
            continue;
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("tool name is not UTF-8"))?;
        if name.is_empty() {
            bail!("external tool has an empty name");
        }
        registry.insert(ExternalTool::new(name, entry.path()), approval);
    }
    Ok(())
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &std::fs::Metadata) -> bool {
    true
}
