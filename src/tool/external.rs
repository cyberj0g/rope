use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::{io::AsyncWriteExt, process::Command};

use super::{Tool, ToolResult};

pub struct ExternalTool {
    name: String,
    path: PathBuf,
}

impl ExternalTool {
    pub fn new(name: String, path: PathBuf) -> Self {
        Self { name, path }
    }
}

#[async_trait]
impl Tool for ExternalTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "External executable tool"
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "additionalProperties": true })
    }

    async fn run(&self, args: Value) -> Result<ToolResult> {
        let mut command = Command::new(&self.path);
        command
            .kill_on_drop(true)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("start external tool {}", self.name))?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(&args)?)
            .await?;
        let output = child.wait_with_output().await?;
        if !output.status.success() {
            bail!(
                "external tool {} exited with {}: {}",
                self.name,
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        serde_json::from_slice(&output.stdout)
            .with_context(|| format!("decode output from external tool {}", self.name))
    }
}
