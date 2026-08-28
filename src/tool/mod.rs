mod builtin;
mod external;
mod headless;
mod web_browser;
mod web_search;

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{config::Config, runtime::ImageContent};
use builtin::{
    EditTool, GlobTool, GrepTool, ReadTool, ShellTool, UpdatePlanTool, ViewImageTool, WriteTool,
};
use external::ExternalTool;
use headless::HeadlessBrowser;
use web_browser::WebBrowserTool;
use web_search::WebSearchTool;

pub use headless::{browser_executable, prepare_runtime as prepare_browser_runtime};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Approval {
    Allow,
    Ask,
    Deny,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExecutionPlan {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    pub plan: Vec<PlanStep>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PlanStep {
    pub step: String,
    pub status: PlanStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolResult {
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
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
    fn vision_only(&self) -> bool {
        false
    }
    async fn run(&self, args: Value) -> Result<ToolResult>;
    async fn shutdown(&self) {}
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

    pub fn definitions(&self, vision: bool) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .filter(|entry| vision || !entry.tool.vision_only())
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

    pub async fn shutdown(&self) {
        for entry in self.tools.values() {
            entry.tool.shutdown().await;
        }
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
    registry.insert(ViewImageTool(cwd.clone()), config.tools.read);
    add_web_tools(
        &mut registry,
        config,
        HeadlessBrowser::discover().map(Arc::new),
    );

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
    registry.insert(UpdatePlanTool, Approval::Allow);
    Ok(registry)
}

fn add_web_tools(
    registry: &mut ToolRegistry,
    config: &Config,
    browser: Option<Arc<HeadlessBrowser>>,
) {
    if let Some(browser) = browser {
        registry.insert(
            WebBrowserTool::new(browser.clone()),
            config.tools.web_browser,
        );
        registry.insert(WebSearchTool::new(browser), config.tools.web_search);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vision_tool_is_only_advertised_to_vision_models() {
        let mut tools = ToolRegistry::default();
        tools.insert(ViewImageTool(PathBuf::new()), Approval::Allow);

        assert!(tools.definitions(false).is_empty());
        assert_eq!(
            tools.definitions(true)[0].function.name,
            "view_image".to_owned()
        );
    }

    #[test]
    fn web_tools_are_omitted_without_a_headless_browser() {
        let mut tools = ToolRegistry::default();
        add_web_tools(&mut tools, &Config::default(), None);

        assert!(tools.get("web_browser").is_err());
        assert!(tools.get("web_search").is_err());
    }

    #[tokio::test]
    async fn update_plan_normalizes_and_validates_steps() {
        let result = UpdatePlanTool
            .run(serde_json::json!({
                "explanation": "  starting work  ",
                "plan": [
                    { "step": " inspect code ", "status": "completed" },
                    { "step": " implement pane ", "status": "in_progress" }
                ]
            }))
            .await
            .unwrap();
        let plan: ExecutionPlan = serde_json::from_str(&result.output).unwrap();
        assert_eq!(plan.explanation.as_deref(), Some("starting work"));
        assert_eq!(plan.plan[0].step, "inspect code");

        let error = UpdatePlanTool
            .run(serde_json::json!({
                "plan": [
                    { "step": "one", "status": "in_progress" },
                    { "step": "two", "status": "in_progress" }
                ]
            }))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("at most one"));
    }
}
