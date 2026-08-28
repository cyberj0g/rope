use std::{fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;
use directories::BaseDirs;
use serde::{Deserialize, Serialize};

use crate::{runtime::ReasoningEffort, tool::Approval};

#[derive(Clone, Debug, Default)]
pub struct Startup {
    pub session: Option<String>,
}

#[derive(Debug, Parser)]
#[command(version, about = "Minimal OpenAI-compatible terminal chat")]
pub struct Args {
    #[arg(long)]
    pub session: Option<String>,
    pub request: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub base_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    pub providers: Vec<ProviderConfig>,
    pub model: String,
    pub models: Vec<ModelConfig>,
    pub temperature: Option<f32>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub paste_collapse_chars: usize,
    pub compaction_threshold: f32,
    pub tools: ToolPolicies,
    #[serde(skip)]
    pub recent_models: Vec<String>,
    #[serde(skip)]
    pub recent_commands: Vec<String>,
    #[serde(skip)]
    settings_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub api: ProviderApi,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderApi {
    #[default]
    Responses,
    ChatCompletions,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            name: "default".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_key: String::new(),
            api: ProviderApi::Responses,
        }
    }
}

impl std::fmt::Display for ProviderApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Responses => "Responses",
            Self::ChatCompletions => "Chat Completions",
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct ModelConfig {
    #[serde(default)]
    pub name: String,
    pub provider: String,
    pub id: String,
    pub max_context_tokens: u64,
    pub temperature: Option<f32>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub reasoning_efforts: Vec<ReasoningEffort>,
    pub price_per_token: Option<f64>,
    pub vision: bool,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            name: "qwen".into(),
            provider: "default".into(),
            id: "vllm/qwen3.8-27b".into(),
            max_context_tokens: 262_144,
            temperature: Some(1.0),
            reasoning_effort: Some(ReasoningEffort::XHigh),
            reasoning_efforts: vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::XHigh,
            ],
            price_per_token: None,
            vision: true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ToolPolicies {
    pub read: Approval,
    pub write: Approval,
    pub edit: Approval,
    pub shell: Approval,
    #[serde(alias = "grep")]
    pub search_files: Approval,
    #[serde(alias = "glob")]
    pub list_files: Approval,
    pub web_browser: Approval,
    pub web_search: Approval,
    pub external: Approval,
}

impl Default for ToolPolicies {
    fn default() -> Self {
        Self {
            read: Approval::Allow,
            write: Approval::Ask,
            edit: Approval::Ask,
            shell: Approval::Ask,
            search_files: Approval::Allow,
            list_files: Approval::Allow,
            web_browser: Approval::Ask,
            web_search: Approval::Ask,
            external: Approval::Ask,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let model = ModelConfig::default();
        let reasoning_effort = model.reasoning_effort;
        Self {
            base_url: "https://api.openai.com/v1".into(),
            api_key: String::new(),
            providers: Vec::new(),
            model: model.name.clone(),
            models: vec![model],
            temperature: None,
            reasoning_effort,
            paste_collapse_chars: 200,
            compaction_threshold: 0.75,
            tools: ToolPolicies::default(),
            recent_models: Vec::new(),
            recent_commands: Vec::new(),
            settings_path: PathBuf::new(),
        }
    }
}

impl Args {
    pub fn startup(&self) -> Startup {
        Startup {
            session: self.session.clone(),
        }
    }
}

impl Config {
    pub fn global_exists() -> Result<bool> {
        Ok(config_root()?.join("config.toml").is_file())
    }

    pub fn write_global(&self) -> Result<PathBuf> {
        let path = config_root()?.join("config.toml");
        fs::create_dir_all(path.parent().unwrap())?;
        write_private(&path, toml::to_string_pretty(self)?.as_bytes())?;
        Ok(path)
    }

    pub fn load() -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let global = config_root()?.join("config.toml");
        let local_root = cwd.join(".rope");
        let local = local_root.join("config.toml");
        let settings_path = if local_root.is_dir() {
            local_root.join("state.toml")
        } else {
            config_root()?.join("state.toml")
        };

        let mut value = toml::Value::try_from(Self::default())?;
        for path in [&global, &local] {
            if path.exists() {
                let mut overlay = read_toml(path)?;
                promote_legacy_provider(&mut overlay);
                promote_legacy_tool_names(&mut overlay);
                merge(&mut value, overlay);
            }
        }
        let mut config: Self = value.try_into().context("decode merged config")?;
        config.settings_path = settings_path;
        config.normalize()?;
        config.load_settings()?;
        config.normalize_reasoning_effort();

        Ok(config)
    }

    pub fn active_model(&self) -> &ModelConfig {
        self.models
            .iter()
            .find(|model| model.name == self.model || model.id == self.model)
            .unwrap_or(&self.models[0])
    }

    pub fn model_id(&self) -> &str {
        &self.active_model().id
    }

    pub fn provider_name(&self) -> &str {
        &self.active_model().provider
    }

    pub fn model_name(&self) -> &str {
        &self.active_model().name
    }

    pub fn effective_temperature(&self) -> Option<f32> {
        self.temperature.or(self.active_model().temperature)
    }

    pub fn effective_reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.reasoning_effort
            .filter(|effort| self.active_model().reasoning_efforts.contains(effort))
    }

    pub fn light_reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.active_model().reasoning_efforts.first().copied()
    }

    pub fn set_model(&mut self, value: &str) -> Result<()> {
        self.select_model(value)?;
        self.reasoning_effort = self.active_model().reasoning_effort;
        self.remember_model();
        self.persist_settings()
    }

    pub fn next_reasoning_effort(&mut self) -> Result<()> {
        let efforts = &self.active_model().reasoning_efforts;
        if efforts.is_empty() {
            self.reasoning_effort = None;
            return self.persist_settings();
        }
        let next = self
            .reasoning_effort
            .and_then(|current| efforts.iter().position(|effort| *effort == current))
            .map_or(0, |index| (index + 1) % efforts.len());
        self.reasoning_effort = Some(efforts[next]);
        self.persist_settings()
    }

    pub fn remember_command(&mut self, command: &str) -> Result<()> {
        self.recent_commands.retain(|name| name != command);
        self.recent_commands.insert(0, command.to_owned());
        self.recent_commands.truncate(12);
        self.persist_settings()
    }

    fn select_model(&mut self, value: &str) -> Result<()> {
        let model = self
            .models
            .iter()
            .find(|model| model.name == value || model.id == value)
            .with_context(|| format!("unknown model: {value}"))?;
        self.model = model.name.clone();
        Ok(())
    }

    fn normalize(&mut self) -> Result<()> {
        if self.providers.is_empty() {
            self.providers.push(ProviderConfig {
                name: "default".into(),
                base_url: self.base_url.clone(),
                api_key: self.api_key.clone(),
                api: ProviderApi::Responses,
            });
        }
        let mut provider_names = std::collections::HashSet::new();
        for provider in &self.providers {
            if provider.name.is_empty() {
                bail!("provider name cannot be empty");
            }
            if provider.base_url.is_empty() {
                bail!("provider {} has an empty base_url", provider.name);
            }
            if !provider_names.insert(provider.name.as_str()) {
                bail!("duplicate provider name: {}", provider.name);
            }
        }
        if self.models.is_empty() {
            let id = self.model.clone();
            self.models.push(ModelConfig {
                name: id.clone(),
                id,
                ..ModelConfig::default()
            });
        }
        for model in &mut self.models {
            if model.provider.is_empty()
                || (model.provider == "default"
                    && !provider_names.contains("default")
                    && self.providers.len() == 1)
            {
                model.provider.clone_from(&self.providers[0].name);
            }
            if model.id.is_empty() {
                model.id.clone_from(&model.name);
            }
            if model.name.is_empty() {
                model.name.clone_from(&model.id);
            }
            if model.max_context_tokens == 0 {
                bail!("model {} has zero max_context_tokens", model.name);
            }
            if !provider_names.contains(model.provider.as_str()) {
                bail!(
                    "model {} references unknown provider {}",
                    model.name,
                    model.provider
                );
            }
        }
        if !self
            .models
            .iter()
            .any(|entry| entry.name == self.model || entry.id == self.model)
        {
            if self.models.as_slice() == [ModelConfig::default()] {
                self.models[0].name.clone_from(&self.model);
                self.models[0].id.clone_from(&self.model);
            } else {
                self.model = self.models[0].name.clone();
            }
        } else {
            self.model = self.active_model().name.clone();
        }
        if !(0.0..=1.0).contains(&self.compaction_threshold) {
            bail!("compaction_threshold must be between 0 and 1");
        }
        Ok(())
    }

    fn normalize_reasoning_effort(&mut self) {
        let model = self.active_model();
        if !self
            .reasoning_effort
            .is_some_and(|effort| model.reasoning_efforts.contains(&effort))
        {
            self.reasoning_effort = model.reasoning_effort;
        }
    }

    fn load_settings(&mut self) -> Result<()> {
        let Ok(data) = fs::read_to_string(&self.settings_path) else {
            return Ok(());
        };
        let settings: PersistedSettings = toml::from_str(&data)
            .with_context(|| format!("parse settings {}", self.settings_path.display()))?;
        if let Some(model) = settings.model {
            self.select_model(&model)?;
        }
        self.recent_models = settings
            .recent_models
            .into_iter()
            .filter(|name| self.models.iter().any(|model| model.name == *name))
            .collect();
        self.recent_commands = settings.recent_commands;
        self.remember_model();
        if let Some(effort) = settings.reasoning_effort {
            self.reasoning_effort = Some(effort);
        }
        Ok(())
    }

    fn remember_model(&mut self) {
        self.recent_models.retain(|name| name != &self.model);
        self.recent_models.insert(0, self.model.clone());
        self.recent_models.truncate(12);
    }

    fn persist_settings(&self) -> Result<()> {
        let settings = PersistedSettings {
            model: Some(self.model.clone()),
            reasoning_effort: self.reasoning_effort,
            recent_models: self.recent_models.clone(),
            recent_commands: self.recent_commands.clone(),
        };
        fs::create_dir_all(self.settings_path.parent().unwrap())?;
        fs::write(&self.settings_path, toml::to_string_pretty(&settings)?)
            .with_context(|| format!("write settings {}", self.settings_path.display()))
    }
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true).mode(0o600);
    std::io::Write::write_all(&mut options.open(path)?, data)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, data: &[u8]) -> Result<()> {
    fs::write(path, data)?;
    Ok(())
}

#[derive(Deserialize, Serialize)]
struct PersistedSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recent_models: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recent_commands: Vec<String>,
}

pub fn config_root() -> Result<PathBuf> {
    let base = BaseDirs::new().context("home directory not found")?;
    Ok(base.config_dir().join("rope"))
}

fn read_toml(path: &PathBuf) -> Result<toml::Value> {
    toml::from_str(
        &fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?,
    )
    .with_context(|| format!("parse config {}", path.display()))
}

fn promote_legacy_provider(value: &mut toml::Value) {
    let Some(table) = value.as_table_mut() else {
        return;
    };
    if table.contains_key("providers") {
        return;
    }
    let Some(base_url) = table.get("base_url").cloned() else {
        return;
    };
    let api_key = table
        .get("api_key")
        .cloned()
        .unwrap_or_else(|| toml::Value::String(String::new()));
    table.insert(
        "providers".into(),
        toml::Value::Array(vec![toml::Value::Table(toml::Table::from_iter([
            ("name".into(), toml::Value::String("default".into())),
            ("base_url".into(), base_url),
            ("api_key".into(), api_key),
        ]))]),
    );
}

fn promote_legacy_tool_names(value: &mut toml::Value) {
    let Some(tools) = value
        .as_table_mut()
        .and_then(|table| table.get_mut("tools"))
        .and_then(toml::Value::as_table_mut)
    else {
        return;
    };
    for (old, new) in [("grep", "search_files"), ("glob", "list_files")] {
        if !tools.contains_key(new)
            && let Some(policy) = tools.remove(old)
        {
            tools.insert(new.into(), policy);
        } else {
            tools.remove(old);
        }
    }
}

fn merge(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(base) => merge(base, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn cli_accepts_only_session_and_one_request() {
        let args = Args::try_parse_from(["rope", "--session", "work", "fix the tests"]).unwrap();
        assert_eq!(args.session.as_deref(), Some("work"));
        assert_eq!(args.request.as_deref(), Some("fix the tests"));
        assert!(Args::try_parse_from(["rope", "one", "two"]).is_err());
        assert!(Args::try_parse_from(["rope", "--model", "qwen"]).is_err());
    }

    #[test]
    fn cli_values_override_defaults() {
        let mut config = Config::default();
        config.select_model("qwen").unwrap();
        config.temperature = Some(0.5);
        config.reasoning_effort = Some(ReasoningEffort::High);

        assert_eq!(config.model_name(), "qwen");
        assert_eq!(config.effective_temperature(), Some(0.5));
        assert_eq!(config.reasoning_effort, Some(ReasoningEffort::High));
    }

    #[test]
    fn reads_per_model_token_price() {
        let model: ModelConfig = toml::from_str(
            r#"name = "priced"
id = "priced"
price_per_token = 0.0000025"#,
        )
        .unwrap();

        assert_eq!(model.price_per_token, Some(0.0000025));
        assert_eq!(ModelConfig::default().price_per_token, None);
    }

    #[test]
    fn providers_default_to_responses_and_can_select_chat_completions() {
        let responses: ProviderConfig = toml::from_str(
            r#"name = "local"
base_url = "http://localhost:8000/v1""#,
        )
        .unwrap();
        let chat: ProviderConfig = toml::from_str(
            r#"name = "legacy"
base_url = "https://legacy.example/v1"
api = "chat_completions""#,
        )
        .unwrap();

        assert_eq!(responses.api, ProviderApi::Responses);
        assert_eq!(chat.api, ProviderApi::ChatCompletions);
    }

    #[test]
    fn local_values_replace_global_values() {
        let mut value: toml::Value = toml::from_str(
            r#"temperature = 1.0
[tools]
write = "ask"
"#,
        )
        .unwrap();
        merge(
            &mut value,
            toml::from_str(
                r#"temperature = 0.4
[tools]
write = "allow"
"#,
            )
            .unwrap(),
        );
        assert_eq!(value["temperature"].as_float(), Some(0.4));
        assert_eq!(value["tools"]["write"].as_str(), Some("allow"));
    }

    #[test]
    fn legacy_endpoint_layer_replaces_named_providers() {
        let mut value: toml::Value = toml::from_str(
            r#"
[[providers]]
name = "remote"
base_url = "https://remote.example/v1"
"#,
        )
        .unwrap();
        let mut overlay: toml::Value = toml::from_str(
            r#"
base_url = "http://localhost:8000/v1"
api_key = "local-key"
"#,
        )
        .unwrap();

        promote_legacy_provider(&mut overlay);
        merge(&mut value, overlay);

        assert_eq!(value["providers"][0]["name"].as_str(), Some("default"));
        assert_eq!(
            value["providers"][0]["base_url"].as_str(),
            Some("http://localhost:8000/v1")
        );
        assert_eq!(value["providers"][0]["api_key"].as_str(), Some("local-key"));
    }

    #[test]
    fn legacy_tool_policy_names_are_promoted() {
        let mut value: toml::Value = toml::from_str(
            r#"[tools]
grep = "deny"
glob = "ask""#,
        )
        .unwrap();

        promote_legacy_tool_names(&mut value);

        assert_eq!(value["tools"]["search_files"].as_str(), Some("deny"));
        assert_eq!(value["tools"]["list_files"].as_str(), Some("ask"));
        assert!(value["tools"].get("grep").is_none());
        assert!(value["tools"].get("glob").is_none());
    }

    #[test]
    fn model_temperature_is_used_without_global_override() {
        let mut config = Config::default();
        config.models[0].temperature = Some(0.7);
        assert_eq!(config.effective_temperature(), Some(0.7));
    }

    #[test]
    fn omitted_model_name_defaults_to_api_id() {
        let mut config: Config = toml::from_str(
            r#"
model = "api/qwen"

[[models]]
id = "api/qwen"
max_context_tokens = 32768
"#,
        )
        .unwrap();
        config.normalize().unwrap();

        assert_eq!(config.models[0].name, "api/qwen");
        assert_eq!(config.model_name(), "api/qwen");
        assert_eq!(config.model_id(), "api/qwen");
    }

    #[test]
    fn legacy_endpoint_becomes_the_default_provider() {
        let mut config: Config = toml::from_str(
            r#"
base_url = "https://legacy.example/v1"
api_key = "secret"
model = "legacy-model"

[[models]]
id = "legacy-model"
max_context_tokens = 32768
"#,
        )
        .unwrap();

        config.normalize().unwrap();

        assert_eq!(config.providers.len(), 1);
        assert_eq!(config.providers[0].name, "default");
        assert_eq!(config.providers[0].base_url, "https://legacy.example/v1");
        assert_eq!(config.providers[0].api_key, "secret");
        assert_eq!(config.provider_name(), "default");
    }

    #[test]
    fn models_bind_to_named_providers() {
        let mut config: Config = toml::from_str(
            r#"
model = "remote-model"

[[providers]]
name = "remote"
base_url = "https://remote.example/v1"

[[models]]
name = "remote-model"
provider = "remote"
id = "api-model"
max_context_tokens = 32768
"#,
        )
        .unwrap();

        config.normalize().unwrap();

        assert_eq!(config.provider_name(), "remote");
        assert_eq!(config.model_id(), "api-model");
    }

    #[test]
    fn switching_models_uses_that_models_reasoning_default() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config {
            settings_path: directory.path().join("state.toml"),
            models: vec![
                crate::model_catalog::model_config("gpt-4.1".into()),
                crate::model_catalog::model_config("Qwen/Qwen3.8-27B".into()),
            ],
            model: "gpt-4.1".into(),
            reasoning_effort: None,
            ..Config::default()
        };

        config.set_model("Qwen/Qwen3.8-27B").unwrap();

        assert_eq!(config.model_id(), "Qwen/Qwen3.8-27B");
        assert_eq!(config.recent_models[0], "Qwen/Qwen3.8-27B");
        assert!(
            std::fs::read_to_string(directory.path().join("state.toml"))
                .unwrap()
                .contains("recent_models")
        );
        assert_eq!(
            config.effective_reasoning_effort(),
            Some(ReasoningEffort::XHigh)
        );
    }

    #[test]
    fn recently_used_commands_are_persisted_in_order() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = Config {
            settings_path: directory.path().join("state.toml"),
            ..Config::default()
        };

        config.remember_command("/save").unwrap();
        config.remember_command("/tools").unwrap();
        config.remember_command("/save").unwrap();

        assert_eq!(config.recent_commands, ["/save", "/tools"]);
        let settings: PersistedSettings =
            toml::from_str(&std::fs::read_to_string(directory.path().join("state.toml")).unwrap())
                .unwrap();
        assert_eq!(settings.recent_commands, ["/save", "/tools"]);
    }
}
