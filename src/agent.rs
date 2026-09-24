//! File-configured agents.
//!
//! Agents are Markdown files with a TOML front matter block delimited by
//! `+++` lines, discovered from `~/.config/rope/agents/` (global) and
//! `<project>/.rope/agents/` (project). The filename stem is the agent's
//! stable ID; a project file replaces the global definition with the same
//! ID entirely (no field or body merging). The remaining Markdown is the
//! agent's additional instructions, appended to the normal project prompt.
//!
//! The built-in `assistant` is always present, reserved, selectable, and
//! delegable: it is the ordinary Rope session (existing prompt assembly,
//! `AGENTS.md`, default model, `[tools]` configuration).

use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{config::Config, tool::Approval};

/// The built-in agent every session starts from. Its ID is reserved: a file
/// named `assistant.md` is an error, not an override.
pub const ASSISTANT_ID: &str = "assistant";

/// Maximum number of nested child levels one delegation chain may reach.
/// A root session is level 0; its children level 1, and so on.
pub const MAX_DELEGATION_DEPTH: u8 = 4;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    /// Selectable as the session's primary agent, never callable as a
    /// subagent.
    Primary,
    /// Callable as a subagent only, never directly selectable.
    Subagent,
    /// Both selectable and callable (the default).
    All,
}

impl AgentMode {
    pub fn selectable(self) -> bool {
        matches!(self, Self::Primary | Self::All)
    }
    pub fn delegable(self) -> bool {
        matches!(self, Self::Subagent | Self::All)
    }
}

impl std::fmt::Display for AgentMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Primary => "primary",
            Self::Subagent => "subagent",
            Self::All => "all",
        })
    }
}

fn default_mode() -> AgentMode {
    AgentMode::All
}

/// The parsed front matter of one agent file. The format is intentionally
/// limited to these fields; anything else is a validation error.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentDef {
    description: String,
    #[serde(default = "default_mode")]
    mode: AgentMode,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    can_call_subagents: bool,
    #[serde(default)]
    tools: BTreeMap<String, Approval>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentSource {
    Builtin,
    Global,
    Project,
}

impl std::fmt::Display for AgentSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Builtin => "built-in",
            Self::Global => "global",
            Self::Project => "project",
        })
    }
}

/// One resolved agent definition.
#[derive(Clone, Debug)]
pub struct Agent {
    pub id: String,
    pub description: String,
    pub mode: AgentMode,
    /// An existing Rope model profile name (with its provider routing).
    /// `None` means the configured default model.
    pub model: Option<String>,
    /// Whether this agent may call the `subagent` tool. Separate from
    /// whether the agent is itself callable as a subagent (`mode`).
    pub can_call_subagents: bool,
    /// `allow` / `ask` / `deny` overrides, keyed by exact tool name or by
    /// category (`external`, `mcp`, or a built-in tool's name). Omitted
    /// entries inherit the configured policies.
    pub tools: BTreeMap<String, Approval>,
    /// The Markdown body: additional instructions appended to the normal
    /// project/runtime prompt. Empty for the built-in assistant.
    pub body: String,
    pub source: AgentSource,
}

impl Agent {
    /// The picker label: `Assistant` for the built-in, the ID with dashes
    /// turned into spaces and each word title-cased otherwise.
    pub fn display_name(&self) -> String {
        if self.id == ASSISTANT_ID {
            return "Assistant".into();
        }
        self.id
            .split('-')
            .map(|part| {
                let mut chars = part.chars();
                chars
                    .next()
                    .map(|c| c.to_ascii_uppercase().to_string())
                    .unwrap_or_default()
                    + &chars.collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn selectable(&self) -> bool {
        self.mode.selectable()
    }

    /// Whether the `subagent` tool may invoke this agent. The built-in
    /// assistant is always delegable even though its mode is `primary`
    /// (a subagent that needs "just Rope" delegates to the assistant).
    pub fn delegable(&self) -> bool {
        self.id == ASSISTANT_ID || self.mode.delegable()
    }

    /// The model profile name this agent uses when a new session or child
    /// starts on it: its own definition, or the configured default.
    pub fn default_model<'a>(&'a self, config: &'a Config) -> &'a str {
        self.model.as_deref().unwrap_or_else(|| config.model_name())
    }

    /// The effective policy for one tool: the agent's exact tool entry,
    /// then its category entry, then the configured base policy.
    pub fn effective_policy(&self, name: &str, category: &str, base: Approval) -> Approval {
        self.tools
            .get(name)
            .copied()
            .or_else(|| {
                (!category.is_empty())
                    .then(|| self.tools.get(category))
                    .flatten()
                    .copied()
            })
            .unwrap_or(base)
    }
}

/// Public metadata returned to clients: never the prompt body or policies.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub mode: String,
    pub can_call_subagents: bool,
}

impl Agent {
    pub fn info(&self) -> AgentInfo {
        AgentInfo {
            id: self.id.clone(),
            name: self.display_name(),
            description: self.description.clone(),
            model: self.model.clone(),
            mode: self.mode.to_string(),
            can_call_subagents: self.can_call_subagents,
        }
    }
}

/// The loaded set of agents: every file definition plus the built-in
/// assistant, keyed by ID and sorted for stable display.
#[derive(Clone, Debug, Default)]
pub struct AgentCatalog {
    agents: BTreeMap<String, Agent>,
}

impl AgentCatalog {
    /// A catalog holding only the built-in assistant: what a caller gets
    /// without reading any agent files.
    pub fn builtin() -> Self {
        let mut catalog = Self {
            agents: BTreeMap::new(),
        };
        catalog.agents.insert(ASSISTANT_ID.to_owned(), assistant());
        catalog
    }

    /// Loads global and project agent files and validates them against the
    /// config. Errors name the file and the offending field.
    pub fn load(config: &Config, project_root: &Path) -> Result<Self> {
        let global = global_agents_dir();
        let project = project_root.join(".rope").join("agents");
        Self::load_from(&global, &project, config)
    }

    fn load_from(
        global: &Option<std::path::PathBuf>,
        project: &Path,
        config: &Config,
    ) -> Result<Self> {
        let mut catalog = Self {
            agents: BTreeMap::new(),
        };
        catalog.agents.insert(ASSISTANT_ID.to_owned(), assistant());
        for (directory, source) in [
            (global.as_deref(), AgentSource::Global),
            (Some(project), AgentSource::Project),
        ] {
            let Some(directory) = directory else {
                continue;
            };
            let Ok(entries) = std::fs::read_dir(directory) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() || path.extension().is_none_or(|ext| ext != "md") {
                    continue;
                }
                let id = path
                    .file_stem()
                    .context("agent file has no stem")?
                    .to_string_lossy()
                    .to_string();
                validate_agent_id(&id, &path)?;
                if catalog.agents.contains_key(&id) {
                    // A project file replaces the global definition with the
                    // same ID; the built-in ID was rejected above.
                    if source == AgentSource::Project {
                        catalog.agents.remove(&id);
                    } else {
                        continue;
                    }
                }
                let agent = parse_agent_file(&path, &id, source, config)?;
                catalog.agents.insert(id, agent);
            }
        }
        Ok(catalog)
    }

    pub fn get(&self, id: &str) -> Option<&Agent> {
        self.agents.get(id)
    }

    pub fn all(&self) -> Vec<&Agent> {
        self.agents.values().collect()
    }

    pub fn selectable(&self) -> Vec<AgentInfo> {
        self.agents
            .values()
            .filter(|agent| agent.selectable())
            .map(|agent| agent.info())
            .collect()
    }

    pub fn delegable(&self) -> Vec<AgentInfo> {
        self.agents
            .values()
            .filter(|agent| agent.delegable())
            .map(|agent| agent.info())
            .collect()
    }

    /// Whether `id` names a known, selectable agent for a session whose
    /// current agent is `current` (a child session may keep its own
    /// subagent-only definition).
    pub fn is_selectable(&self, id: &str, current: Option<&str>) -> bool {
        self.agents
            .get(id)
            .is_some_and(|agent| agent.selectable() || Some(agent.id.as_str()) == current)
    }
}

/// The built-in assistant definition: the entry every catalog contains.
pub fn assistant() -> Agent {
    Agent {
        id: ASSISTANT_ID.into(),
        description: "The default Rope assistant".into(),
        mode: AgentMode::Primary,
        model: None,
        can_call_subagents: true,
        tools: BTreeMap::new(),
        body: String::new(),
        source: AgentSource::Builtin,
    }
}

fn global_agents_dir() -> Option<std::path::PathBuf> {
    let base = directories::BaseDirs::new()?;
    Some(base.config_dir().join("rope").join("agents"))
}

fn validate_agent_id(id: &str, path: &Path) -> Result<()> {
    if id == ASSISTANT_ID {
        bail!(
            "{}: the agent id 'assistant' is reserved for the built-in assistant",
            path.display()
        );
    }
    if id.is_empty()
        || id
            .chars()
            .any(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'))
    {
        bail!(
            "{}: agent ids use lowercase letters, digits, '-' and '_' (got '{id}')",
            path.display()
        );
    }
    Ok(())
}

/// Splits one agent file into its `+++`-delimited TOML front matter and the
/// Markdown body that follows.
fn split_front_matter(text: &str, path: &Path) -> Result<(String, String)> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if lines.first().is_none_or(|line| line.trim() != "+++") {
        bail!(
            "{}: the file must start with a '+++' front matter delimiter",
            path.display()
        );
    }
    let Some(rel) = lines[1..].iter().position(|line| line.trim() == "+++") else {
        bail!(
            "{}: missing closing '+++' front matter delimiter",
            path.display()
        );
    };
    let end = 1 + rel;
    let front = lines[1..end].concat();
    let body = lines[end + 1..].concat();
    Ok((front, body))
}

fn parse_agent_file(path: &Path, id: &str, source: AgentSource, config: &Config) -> Result<Agent> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("read agent {}", path.display()))?;
    let (front, body) = split_front_matter(&text, path)?;
    let def: AgentDef = match toml::from_str(&front) {
        Ok(def) => def,
        Err(err) => bail!("{}: invalid front matter: {err}", path.display()),
    };
    if def.description.trim().is_empty() {
        bail!(
            "{}: 'description' must be a nonempty string",
            path.display()
        );
    }
    if let Some(model) = &def.model
        && !config
            .models
            .iter()
            .any(|entry| entry.name == *model || entry.id == *model)
    {
        bail!(
            "{}: field 'model' references unknown model '{model}'",
            path.display()
        );
    }
    Ok(Agent {
        id: id.into(),
        description: def.description.trim().to_owned(),
        mode: def.mode,
        model: def.model,
        can_call_subagents: def.can_call_subagents,
        tools: def.tools,
        body: body.trim().to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, name: &str, content: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), content).unwrap();
    }

    /// Loads a catalog from an explicit global agents directory, bypassing
    /// the real `~/.config` so tests never see the host's configuration.
    /// `project` is the project root (`.rope/agents` is appended).
    fn load_from(global: &Path, project: &Path) -> Result<AgentCatalog> {
        let project = project.join(".rope").join("agents");
        AgentCatalog::load_from(&global.to_path_buf().into(), &project, &Config::default())
    }

    #[test]
    fn loads_valid_agents_with_defaults() {
        let global = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        write(
            global.path(),
            "review.md",
            r#"+++
description = "Review changes"
mode = "subagent"
can_call_subagents = false

[tools]
write = "deny"
edit = "deny"
+++

Review the changes and report actionable findings.
"#,
        );
        // A file without the optional fields.
        write(
            project.path().join(".rope/agents").as_path(),
            "plain.md",
            "+++\ndescription = \"A plain helper\"\n+++\n\nBody text.\n",
        );
        let catalog = load_from(global.path(), project.path()).unwrap();
        let review = catalog.get("review").unwrap();
        assert_eq!(review.mode, AgentMode::Subagent);
        assert!(!review.can_call_subagents);
        assert_eq!(review.tools.get("write"), Some(&Approval::Deny));
        assert!(!review.selectable());
        assert!(review.delegable());
        assert!(review.body.contains("actionable findings"));
        assert_eq!(review.description, "Review changes");
        assert_eq!(review.source, AgentSource::Global);
        let plain = catalog.get("plain").unwrap();
        assert_eq!(plain.mode, AgentMode::All);
        assert!(plain.selectable() && plain.delegable());
        assert_eq!(plain.body, "Body text.");
        assert_eq!(catalog.get("assistant").unwrap().mode, AgentMode::Primary);
        // The ID list is sorted and includes the built-in first.
        assert_eq!(
            catalog
                .all()
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            ["assistant", "plain", "review"]
        );
    }

    #[test]
    fn project_file_replaces_the_global_definition() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        write(
            dir.path(),
            "review.md",
            "+++\ndescription = \"global copy\"\n+++\n\nglobal body\n",
        );
        write(
            project.join(".rope/agents").as_path(),
            "review.md",
            "+++\ndescription = \"project copy\"\nmode = \"primary\"\n+++\n\nproject body\n",
        );
        let catalog = load_from(dir.path(), &project).unwrap();
        let review = catalog.get("review").unwrap();
        assert_eq!(review.description, "project copy");
        assert_eq!(review.mode, AgentMode::Primary);
        assert_eq!(review.body, "project body");
        assert_eq!(review.source, AgentSource::Project);
    }

    #[test]
    fn rejects_malformed_files_naming_file_and_field() {
        // Missing front matter.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "broken.md", "no front matter here\n");
        let error = load_from(dir.path(), &dir.path().join("project"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("broken.md"), "{error}");
        assert!(error.contains("+++"), "{error}");

        // Unknown field.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "extra.md",
            "+++\ndescription = \"x\"\nfrobnicate = 1\n+++\n",
        );
        let error = load_from(dir.path(), &dir.path().join("project"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("extra.md"), "{error}");
        assert!(error.contains("frobnicate"), "{error}");

        // Invalid mode.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "badmode.md",
            "+++\ndescription = \"x\"\nmode = \"sometimes\"\n+++\n",
        );
        let error = load_from(dir.path(), &dir.path().join("project"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("badmode.md"), "{error}");

        // Invalid policy.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "badtool.md",
            "+++\ndescription = \"x\"\n\n[tools]\nshell = \"maybe\"\n+++\n",
        );
        let error = load_from(dir.path(), &dir.path().join("project"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("badtool.md"), "{error}");

        // Empty description.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "nodesc.md", "+++\ndescription = \"  \"\n+++\n");
        let error = load_from(dir.path(), &dir.path().join("project"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("nodesc.md"), "{error}");
        assert!(error.contains("description"), "{error}");
    }

    #[test]
    fn rejects_unknown_model_profiles() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "model.md",
            "+++\ndescription = \"x\"\nmodel = \"does-not-exist\"\n+++\n",
        );
        let error = load_from(dir.path(), &dir.path().join("project"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("model.md"), "{error}");
        assert!(error.contains("does-not-exist"), "{error}");

        // A known model profile passes.
        write(
            dir.path(),
            "model.md",
            "+++\ndescription = \"x\"\nmodel = \"qwen\"\n+++\n",
        );
        let catalog = load_from(dir.path(), &dir.path().join("project")).unwrap();
        assert_eq!(catalog.get("model").unwrap().model.as_deref(), Some("qwen"));
    }

    #[test]
    fn rejects_bad_ids_and_the_reserved_assistant() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "assistant.md",
            "+++\ndescription = \"x\"\n+++\n",
        );
        let error = load_from(dir.path(), &dir.path().join("project"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("reserved"), "{error}");

        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Bad ID.md", "+++\ndescription = \"x\"\n+++\n");
        assert!(load_from(dir.path(), &dir.path().join("project")).is_err());
    }

    #[test]
    fn policy_lookup_prefers_exact_name_over_category() {
        let agent = Agent {
            id: "a".into(),
            description: String::new(),
            mode: AgentMode::All,
            model: None,
            can_call_subagents: false,
            tools: [
                ("mcp".to_owned(), Approval::Allow),
                ("docs:lookup".to_owned(), Approval::Deny),
            ]
            .into_iter()
            .collect(),
            body: String::new(),
            source: AgentSource::Project,
        };
        // Exact registered name wins over the category.
        assert_eq!(
            agent.effective_policy("docs:lookup", "mcp", Approval::Ask),
            Approval::Deny
        );
        // The category applies to other tools of the same category.
        assert_eq!(
            agent.effective_policy("docs:search", "mcp", Approval::Ask),
            Approval::Allow
        );
        // Omitted entries inherit the configured policy.
        assert_eq!(
            agent.effective_policy("shell", "shell", Approval::Deny),
            Approval::Deny
        );
    }

    #[test]
    fn assistant_is_built_in_selectable_and_delegable() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = load_from(dir.path(), &dir.path().join("project")).unwrap();
        let assistant = catalog.get(ASSISTANT_ID).unwrap();
        assert!(assistant.selectable());
        assert!(assistant.delegable());
        assert!(assistant.can_call_subagents);
        assert!(assistant.body.is_empty());
        assert_eq!(assistant.display_name(), "Assistant");
        let renamed = Agent {
            id: "code-reviewer".into(),
            ..assistant.clone()
        };
        assert_eq!(renamed.display_name(), "Code Reviewer");
    }

    #[test]
    fn a_child_session_may_keep_its_subagent_only_agent() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "helper.md",
            "+++\ndescription = \"x\"\nmode = \"subagent\"\n+++\n",
        );
        let catalog = load_from(dir.path(), &dir.path().join("project")).unwrap();
        assert!(!catalog.is_selectable("helper", None));
        // A session already on `helper` (a child) may keep it.
        assert!(catalog.is_selectable("helper", Some("helper")));
        assert!(catalog.is_selectable("assistant", None));
    }
}
