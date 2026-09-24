mod builtin;
mod external;
mod headless;
mod org_outline;
mod web_browser;
mod web_search;

use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, RwLock},
};

use anyhow::{Result, bail};
use tokio::sync::{mpsc, oneshot, watch};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    agent::{Agent, AgentInfo},
    config::Config,
    runtime::{FileContent, ImageContent, SubagentOutcome},
};
use builtin::{
    EditTool, ListFilesTool, ReadTool, SearchFilesTool, SendFileTool, UpdatePlanTool,
    ViewImageTool, WriteTool,
};
use external::ExternalTool;
use headless::HeadlessBrowser;
use org_outline::OrgOutlineTool;
use web_browser::WebBrowserTool;
use web_search::WebSearchTool;

pub use builtin::{ShellCancelTool, ShellJobManager, ShellPollTool, ShellTool};
pub use headless::{browser_executable, prepare_runtime as prepare_browser_runtime};

pub fn ripgrep_available() -> bool {
    std::process::Command::new("rg")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

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
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FileContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
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
    /// Runs the tool, forwarding partial output to `sink` as it appears.
    /// The sink is best effort: a dropped or capped sink is silently ignored.
    ///
    /// `max_output_bytes` is the runtime's output-byte budget for this call.
    /// Tools that hand back partial output from long-lived work (shell jobs)
    /// must keep the returned output within it, so the runtime's generic
    /// truncation never discards data a later call should still return.
    async fn run_streamed(
        &self,
        args: Value,
        sink: Option<mpsc::UnboundedSender<String>>,
        max_output_bytes: usize,
    ) -> Result<ToolResult> {
        drop((sink, max_output_bytes));
        self.run(args).await
    }
    /// Cooperatively cancels work started by earlier, still-running calls.
    /// The default is a no-op for tools without background work.
    async fn cancel_active(&self) {}
    async fn shutdown(&self) {}
}

#[derive(Clone)]
pub struct ToolEntry {
    pub tool: Arc<dyn Tool>,
    pub approval: Approval,
    pub approval_key: String,
    /// The policy category an agent `[tools]` override matches on: the
    /// tool's own name for built-ins, `external` for user tools, `mcp` for
    /// MCP tools. The exact registered name (including `server:tool` for
    /// MCP) always outranks the category.
    pub category: String,
    origin: Option<String>,
}

#[async_trait]
pub trait ToolResource: Send + Sync {
    async fn cancel_active(&self) {}
    async fn shutdown(&self) {}
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Arc<RwLock<BTreeMap<String, ToolEntry>>>,
    resources: Vec<Arc<dyn ToolResource>>,
    notices: Vec<String>,
}

impl ToolRegistry {
    pub fn insert<T: Tool + 'static>(&mut self, tool: T, approval: Approval) {
        let approval_key = tool.name().to_owned();
        self.insert_with_key(tool, approval, approval_key);
    }

    pub fn insert_with_key<T: Tool + 'static>(
        &mut self,
        tool: T,
        approval: Approval,
        approval_key: String,
    ) {
        // Built-in tools: the tool's own name is its policy category.
        let name = tool.name().to_owned();
        self.tools.write().unwrap().insert(
            name.clone(),
            ToolEntry {
                tool: Arc::new(tool),
                approval,
                category: name,
                approval_key,
                origin: None,
            },
        );
    }

    pub fn try_insert_with_key<T: Tool + 'static>(
        &mut self,
        tool: T,
        approval: Approval,
        approval_key: String,
    ) -> Result<()> {
        let name = tool.name().to_owned();
        let mut tools = self.tools.write().unwrap();
        if tools.contains_key(&name) {
            bail!("duplicate tool name: {name}");
        }
        tools.insert(
            name.clone(),
            ToolEntry {
                tool: Arc::new(tool),
                approval,
                category: name,
                approval_key,
                origin: None,
            },
        );
        Ok(())
    }

    pub(crate) fn replace_origin<T: Tool + 'static>(
        &self,
        origin: &str,
        entries: Vec<(T, Approval, String, String)>,
    ) -> Result<usize> {
        let mut replacement = BTreeMap::new();
        for (tool, approval, approval_key, category) in entries {
            let name = tool.name().to_owned();
            if replacement.contains_key(&name) {
                bail!("duplicate tool name: {name}");
            }
            replacement.insert(
                name,
                ToolEntry {
                    tool: Arc::new(tool),
                    approval,
                    approval_key,
                    category,
                    origin: Some(origin.to_owned()),
                },
            );
        }
        let mut tools = self.tools.write().unwrap();
        for name in replacement.keys() {
            if tools
                .get(name)
                .is_some_and(|entry| entry.origin.as_deref() != Some(origin))
            {
                bail!("duplicate tool name: {name}");
            }
        }
        tools.retain(|_, entry| entry.origin.as_deref() != Some(origin));
        let count = replacement.len();
        tools.extend(replacement);
        Ok(count)
    }

    pub fn add_resource<T: ToolResource + 'static>(&mut self, resource: Arc<T>) {
        self.resources.push(resource);
    }

    pub fn notice(&mut self, notice: impl Into<String>) {
        self.notices.push(notice.into());
    }

    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    pub fn get(&self, name: &str) -> Result<ToolEntry> {
        self.tools
            .read()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown tool: {name}"))
    }

    pub fn definitions(&self, vision: bool) -> Vec<ToolDefinition> {
        self.tools
            .read()
            .unwrap()
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

    /// The effective approval for `name` under `agent`: the agent's exact
    /// tool entry, then its category entry, then the configured policy.
    /// `None` when the tool is not registered.
    pub fn effective(&self, agent: &Agent, name: &str) -> Option<Approval> {
        let tools = self.tools.read().unwrap();
        let entry = tools.get(name)?;
        Some(agent.effective_policy(name, &entry.category, entry.approval))
    }

    /// The approval key stored in session approvals: scoped by the acting
    /// agent so one agent's grants never apply to another.
    pub fn approval_key(&self, agent: &Agent, name: &str) -> Option<String> {
        let tools = self.tools.read().unwrap();
        let entry = tools.get(name)?;
        Some(format!("{}|{}", agent.id, entry.approval_key))
    }

    /// The model-facing schemas for `agent`: tools whose effective policy
    /// is `deny` are absent, and the `subagent` tool is advertised only to
    /// agents that may delegate.
    pub fn definitions_for(&self, agent: &Agent, vision: bool) -> Vec<ToolDefinition> {
        self.tools
            .read()
            .unwrap()
            .values()
            .filter(|entry| vision || !entry.tool.vision_only())
            .filter(|entry| {
                let name = entry.tool.name();
                if name == SubagentTool::NAME && !agent.can_call_subagents {
                    return false;
                }
                agent.effective_policy(name, &entry.category, entry.approval) != Approval::Deny
            })
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

    pub async fn cancel_active(&self) {
        let tools = self
            .tools
            .read()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for entry in tools {
            entry.tool.cancel_active().await;
        }
        for resource in &self.resources {
            resource.cancel_active().await;
        }
    }

    pub async fn shutdown(&self) {
        let tools = self
            .tools
            .read()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for entry in tools {
            entry.tool.shutdown().await;
        }
        for resource in &self.resources {
            resource.shutdown().await;
        }
    }
}

/// The registered name of the delegation tool.
pub const SUBAGENT_TOOL: &str = "subagent";

/// The tool-facing schema and description for `subagent` delegation. The
/// runtime executes `subagent` calls itself — it supplies the
/// session/turn/call identity the core needs — so `run` is never reached
/// in normal operation.
pub struct SubagentTool {
    description: String,
    agents: Vec<AgentInfo>,
}

impl SubagentTool {
    pub const NAME: &str = SUBAGENT_TOOL;

    /// Builds the tool for the delegable agents in the catalog. An empty
    /// list still yields a usable schema so a model that calls it gets a
    /// clear error instead of a crash.
    pub fn new(agents: Vec<AgentInfo>) -> Self {
        let description = {
            let mut text = String::from(
                "Delegate a self-contained task to a specialized agent and wait for its \
                 final answer. The agent works in its own session with its own tools and \
                 instructions; pass everything it needs in the prompt. \
                 Available agents:",
            );
            if agents.is_empty() {
                text.push_str(" (none)");
            }
            for agent in &agents {
                text.push_str(&format!("\n- {}: {}", agent.id, agent.description));
            }
            text
        };
        Self {
            description,
            agents,
        }
    }

    pub fn agent_ids(&self) -> Vec<String> {
        self.agents.iter().map(|agent| agent.id.clone()).collect()
    }

    /// Validates a call's agent against the delegable set.
    pub fn is_delegable(&self, id: &str) -> bool {
        self.agents.iter().any(|agent| agent.id == id)
    }
}

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "The agent ID to delegate to",
                    "enum": self.agent_ids(),
                },
                "prompt": {
                    "type": "string",
                    "description": "The complete task for the agent, self-contained",
                },
            },
            "required": ["agent", "prompt"],
            "additionalProperties": false,
        })
    }
    async fn run(&self, _args: Value) -> Result<ToolResult> {
        bail!("subagent calls are executed by the runtime, not by the tool")
    }
}

/// One delegation request from a parent session's runtime to the core. The
/// runtime supplies the identity of the call that is waiting; the core
/// creates the child, links both sessions, and resolves `reply` exactly
/// once when the child's turn settles.
pub struct DelegationRequest {
    pub parent_session: String,
    pub parent_turn: String,
    pub tool_call_id: String,
    pub agent: String,
    pub prompt: String,
    /// The caller's current model profile: the fallback for a child whose
    /// agent names no model of its own.
    pub caller_model: String,
    pub reply: oneshot::Sender<SubagentOutcome>,
    /// Flipped when the waiting call stops waiting (its turn was
    /// cancelled); the core then cancels the child's turn.
    pub abandon: watch::Sender<bool>,
}

/// Commands the runtime sends to the core over the delegation port.
pub enum DelegationCommand {
    /// Spawn a child session for one `subagent` call.
    Spawn(DelegationRequest),
    /// Stop the parent's active delegation (if any) and wait for the
    /// child to settle before acknowledging: cancellation must stop
    /// descendant work before the parent reports its terminal result.
    Cancel {
        parent_session: String,
        parent_turn: String,
        ack: oneshot::Sender<()>,
    },
}

/// The channel the runtime sends delegation commands over.
pub type DelegationPort = mpsc::UnboundedSender<DelegationCommand>;

pub async fn discover(config: &Config) -> Result<ToolRegistry> {
    discover_at(config, &std::env::current_dir()?).await
}
pub async fn discover_at(config: &Config, root: &std::path::Path) -> Result<ToolRegistry> {
    let cwd = root.to_path_buf();
    let mut registry = ToolRegistry::default();
    registry.insert(ReadTool(cwd.clone()), config.tools.read);
    registry.insert(WriteTool(cwd.clone()), config.tools.write);
    registry.insert(EditTool(cwd.clone()), config.tools.edit);
    let shell_jobs = ShellJobManager::new(cwd.clone());
    registry.insert(ShellTool(shell_jobs.clone()), config.tools.shell);
    // Polling and cancelling can only observe or stop already-approved
    // commands, so they never ask again.
    registry.insert(ShellPollTool(shell_jobs.clone()), Approval::Allow);
    registry.insert(ShellCancelTool(shell_jobs), Approval::Allow);
    let ripgrep = ripgrep_available();
    registry.insert(
        SearchFilesTool::new(cwd.clone(), ripgrep),
        config.tools.search_files,
    );
    registry.insert(
        ListFilesTool::new(cwd.clone(), ripgrep),
        config.tools.list_files,
    );
    registry.insert(OrgOutlineTool(cwd.clone()), config.tools.org_outline);
    registry.insert(ViewImageTool(cwd.clone()), config.tools.read);
    registry.insert(SendFileTool(cwd.clone()), config.tools.send_file);
    add_web_tools(
        &mut registry,
        config,
        HeadlessBrowser::discover().map(Arc::new),
    );

    if let Some(global) =
        directories::BaseDirs::new().map(|dirs| dirs.config_dir().join("rope/tools"))
    {
        add_external(&mut registry, global, &cwd, config.tools.external).await?;
    }
    add_external(
        &mut registry,
        cwd.join(".rope/tools"),
        &cwd,
        config.tools.external,
    )
    .await?;
    crate::mcp::add_tools(&mut registry, config, &cwd).await;
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
    cwd: &std::path::Path,
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
        registry.insert(
            ExternalTool::new(name, entry.path(), cwd.to_path_buf()),
            approval,
        );
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

    #[tokio::test]
    async fn update_plan_rejects_a_call_without_a_plan_array() {
        let error = UpdatePlanTool
            .run(serde_json::json!({ "stored": true }))
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("missing `plan`"));
        assert!(message.contains("complete plan"));
    }
}
