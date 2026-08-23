use std::{fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;
use directories::BaseDirs;
use serde::{Deserialize, Serialize};

use crate::{runtime::ReasoningEffort, tool::Approval};

#[derive(Clone, Debug, Default)]
pub struct Startup {
    pub continue_session: Option<String>,
    pub session: Option<String>,
}

#[derive(Debug, Parser)]
#[command(version, about = "Minimal OpenAI-compatible terminal chat")]
pub struct Args {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long)]
    api_key: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    temperature: Option<f32>,
    #[arg(long)]
    reasoning_effort: Option<ReasoningEffort>,
    #[arg(long = "continue", num_args = 0..=1, default_missing_value = "latest")]
    continue_session: Option<String>,
    #[arg(long, conflicts_with = "continue_session")]
    session: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub models: Vec<ModelConfig>,
    pub temperature: Option<f32>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub reasoning_efforts: Vec<ReasoningEffort>,
    pub price_per_token: f64,
    pub paste_collapse_chars: usize,
    pub compaction_threshold: f32,
    pub tools: ToolPolicies,
    #[serde(skip)]
    settings_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct ModelConfig {
    #[serde(default)]
    pub name: String,
    pub id: String,
    pub max_context_tokens: u64,
    pub temperature: Option<f32>,
    pub vision: bool,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            name: "qwen".into(),
            id: "vllm/qwen3.8-27b".into(),
            max_context_tokens: 32_768,
            temperature: Some(1.0),
            vision: false,
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
    pub grep: Approval,
    pub glob: Approval,
    pub external: Approval,
}

impl Default for ToolPolicies {
    fn default() -> Self {
        Self {
            read: Approval::Allow,
            write: Approval::Ask,
            edit: Approval::Ask,
            shell: Approval::Ask,
            grep: Approval::Allow,
            glob: Approval::Allow,
            external: Approval::Ask,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        let model = ModelConfig::default();
        Self {
            base_url: "http://localhost:8000/v1".into(),
            api_key: String::new(),
            model: model.name.clone(),
            models: vec![model],
            temperature: None,
            reasoning_effort: Some(ReasoningEffort::Medium),
            reasoning_efforts: vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ],
            price_per_token: 0.0,
            paste_collapse_chars: 200,
            compaction_threshold: 0.75,
            tools: ToolPolicies::default(),
            settings_path: PathBuf::new(),
        }
    }
}

impl Args {
    pub fn startup(&self) -> Startup {
        Startup {
            continue_session: self.continue_session.clone(),
            session: self.session.clone(),
        }
    }
}

impl Config {
    pub fn load(args: Args) -> Result<Self> {
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
        if let Some(path) = args.config.as_ref() {
            if !path.exists() {
                bail!("config file not found: {}", path.display());
            }
            merge(&mut value, read_toml(path)?);
        } else {
            for path in [&global, &local] {
                if path.exists() {
                    merge(&mut value, read_toml(path)?);
                }
            }
        }
        let mut config: Self = value.try_into().context("decode merged config")?;
        config.settings_path = settings_path;
        config.normalize()?;
        config.load_settings()?;

        if let Some(value) = args.base_url {
            config.base_url = value;
        }
        if let Some(value) = args.api_key {
            config.api_key = value;
        }
        if let Some(value) = args.model {
            config.select_model(&value)?;
        }
        if let Some(value) = args.temperature {
            config.temperature = Some(value);
        }
        if let Some(value) = args.reasoning_effort {
            config.reasoning_effort = Some(value);
        }
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

    pub fn model_name(&self) -> &str {
        &self.active_model().name
    }

    pub fn effective_temperature(&self) -> Option<f32> {
        self.temperature.or(self.active_model().temperature)
    }

    pub fn next_model(&mut self) -> Result<()> {
        let current = self
            .models
            .iter()
            .position(|model| model.name == self.model)
            .unwrap_or(0);
        self.model = self.models[(current + 1) % self.models.len()].name.clone();
        self.persist_settings()
    }

    pub fn next_reasoning_effort(&mut self) -> Result<()> {
        let next = self
            .reasoning_effort
            .and_then(|current| {
                self.reasoning_efforts
                    .iter()
                    .position(|effort| *effort == current)
            })
            .map_or(0, |index| (index + 1) % self.reasoning_efforts.len());
        self.reasoning_effort = Some(self.reasoning_efforts[next]);
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
        if self.models.is_empty() {
            let id = self.model.clone();
            self.models.push(ModelConfig {
                name: id.clone(),
                id,
                ..ModelConfig::default()
            });
        }
        for model in &mut self.models {
            if model.id.is_empty() {
                model.id.clone_from(&model.name);
            }
            if model.name.is_empty() {
                model.name.clone_from(&model.id);
            }
            if model.max_context_tokens == 0 {
                bail!("model {} has zero max_context_tokens", model.name);
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
        if self.reasoning_efforts.is_empty() {
            bail!("reasoning_efforts cannot be empty");
        }
        if !(0.0..=1.0).contains(&self.compaction_threshold) {
            bail!("compaction_threshold must be between 0 and 1");
        }
        Ok(())
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
        if let Some(effort) = settings.reasoning_effort {
            self.reasoning_effort = Some(effort);
        }
        Ok(())
    }

    fn persist_settings(&self) -> Result<()> {
        let settings = PersistedSettings {
            model: Some(self.model.clone()),
            reasoning_effort: self.reasoning_effort,
        };
        fs::create_dir_all(self.settings_path.parent().unwrap())?;
        fs::write(&self.settings_path, toml::to_string_pretty(&settings)?)
            .with_context(|| format!("write settings {}", self.settings_path.display()))
    }
}

#[derive(Deserialize, Serialize)]
struct PersistedSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<ReasoningEffort>,
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
    fn reads_per_token_price() {
        let config: Config = toml::from_str("price_per_token = 0.0000025").unwrap();
        assert_eq!(config.price_per_token, 0.0000025);
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
}
