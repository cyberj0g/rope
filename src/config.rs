use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use directories::ProjectDirs;
use serde::Deserialize;

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

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub temperature: Option<f32>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub price_per_token: f64,
    pub tools: ToolPolicies,
}

#[derive(Clone, Debug, Deserialize)]
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
        Self {
            base_url: "http://localhost:8000/v1".into(),
            api_key: String::new(),
            model: "vllm/qwen3.8-27b".into(),
            temperature: Some(1.0),
            reasoning_effort: Some(ReasoningEffort::Medium),
            price_per_token: 0.0,
            tools: ToolPolicies::default(),
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
        let explicit_config = args.config.is_some();
        let path = args.config.or_else(default_config_path);
        if explicit_config && !path.as_ref().is_some_and(|path| path.exists()) {
            anyhow::bail!("config file not found: {}", path.unwrap().display());
        }
        let mut config = match path.as_ref().filter(|path| path.exists()) {
            Some(path) => toml::from_str(
                &fs::read_to_string(path)
                    .with_context(|| format!("read config {}", path.display()))?,
            )
            .with_context(|| format!("parse config {}", path.display()))?,
            None => Self::default(),
        };

        if let Some(value) = args.base_url {
            config.base_url = value;
        }
        if let Some(value) = args.api_key {
            config.api_key = value;
        }
        if let Some(value) = args.model {
            config.model = value;
        }
        if let Some(value) = args.temperature {
            config.temperature = Some(value);
        }
        if let Some(value) = args.reasoning_effort {
            config.reasoning_effort = Some(value);
        }
        Ok(config)
    }
}

fn default_config_path() -> Option<PathBuf> {
    ProjectDirs::from("", "", "rope").map(|dirs| dirs.config_dir().join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_values_override_defaults() {
        let config = Config::load(Args::parse_from([
            "rope",
            "--model",
            "test-model",
            "--temperature",
            "0.5",
            "--reasoning-effort",
            "high",
        ]))
        .unwrap();

        assert_eq!(config.model, "test-model");
        assert_eq!(config.temperature, Some(0.5));
        assert!(matches!(
            config.reasoning_effort,
            Some(ReasoningEffort::High)
        ));
    }

    #[test]
    fn reads_per_token_price() {
        let config: Config = toml::from_str("price_per_token = 0.0000025").unwrap();
        assert_eq!(config.price_per_token, 0.0000025);
    }
}
