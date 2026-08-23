use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{Tool, ToolResult};

pub struct ReadTool(pub PathBuf);
pub struct WriteTool(pub PathBuf);
pub struct EditTool(pub PathBuf);
pub struct ShellTool(pub PathBuf);
pub struct GrepTool(pub PathBuf);
pub struct GlobTool(pub PathBuf);

fn path(root: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read a UTF-8 file"
    }
    fn schema(&self) -> Value {
        object(json!({ "path": { "type": "string" } }), &["path"])
    }
    async fn run(&self, args: Value) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
        }
        let args: Args = serde_json::from_value(args)?;
        Ok(ToolResult {
            output: tokio::fs::read_to_string(path(&self.0, &args.path)).await?,
        })
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        "Write a UTF-8 file, replacing its contents"
    }
    fn schema(&self) -> Value {
        object(
            json!({ "path": { "type": "string" }, "content": { "type": "string" } }),
            &["path", "content"],
        )
    }
    async fn run(&self, args: Value) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            content: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let target = path(&self.0, &args.path);
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&target, args.content).await?;
        Ok(ToolResult {
            output: format!("wrote {}", target.display()),
        })
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        "Replace one exact string in a UTF-8 file"
    }
    fn schema(&self) -> Value {
        object(
            json!({
                "path": { "type": "string" }, "old": { "type": "string" }, "new": { "type": "string" }
            }),
            &["path", "old", "new"],
        )
    }
    async fn run(&self, args: Value) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            path: String,
            old: String,
            new: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let target = path(&self.0, &args.path);
        let content = tokio::fs::read_to_string(&target).await?;
        let matches = content.matches(&args.old).count();
        if matches != 1 {
            bail!(
                "expected one match in {}, found {matches}",
                target.display()
            );
        }
        tokio::fs::write(&target, content.replacen(&args.old, &args.new, 1)).await?;
        Ok(ToolResult {
            output: format!("edited {}", target.display()),
        })
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }
    fn description(&self) -> &str {
        "Run a shell command in the current working directory"
    }
    fn schema(&self) -> Value {
        object(json!({ "command": { "type": "string" } }), &["command"])
    }
    async fn run(&self, args: Value) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            command: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let mut command = Command::new("sh");
        command
            .kill_on_drop(true)
            .arg("-c")
            .arg(args.command)
            .current_dir(&self.0);
        let output = command.output().await?;
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        if !output.status.success() {
            bail!("command exited with {}\n{text}", output.status);
        }
        Ok(ToolResult { output: text })
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search files with ripgrep"
    }
    fn schema(&self) -> Value {
        object(
            json!({
                "pattern": { "type": "string" }, "path": { "type": "string", "default": "." }
            }),
            &["pattern"],
        )
    }
    async fn run(&self, args: Value) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            pattern: String,
            path: Option<String>,
        }
        let args: Args = serde_json::from_value(args)?;
        let mut command = Command::new("rg");
        command
            .kill_on_drop(true)
            .arg("--line-number")
            .arg("--color=never")
            .arg(args.pattern)
            .arg(args.path.unwrap_or_else(|| ".".into()))
            .current_dir(&self.0);
        let output = command.output().await.context("run rg")?;
        if !output.status.success() && output.status.code() != Some(1) {
            bail!("rg exited with {}", output.status);
        }
        Ok(ToolResult {
            output: String::from_utf8_lossy(&output.stdout).into_owned(),
        })
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "List paths matching a glob pattern"
    }
    fn schema(&self) -> Value {
        object(json!({ "pattern": { "type": "string" } }), &["pattern"])
    }
    async fn run(&self, args: Value) -> Result<ToolResult> {
        #[derive(Deserialize)]
        struct Args {
            pattern: String,
        }
        let args: Args = serde_json::from_value(args)?;
        let pattern = path(&self.0, &args.pattern).to_string_lossy().into_owned();
        let mut paths = glob::glob(&pattern)?.collect::<std::result::Result<Vec<_>, _>>()?;
        paths.sort();
        let output = paths
            .into_iter()
            .map(|path| {
                path.strip_prefix(&self.0)
                    .unwrap_or(&path)
                    .display()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolResult { output })
    }
}
