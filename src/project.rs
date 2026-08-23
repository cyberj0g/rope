use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::process::Command;

#[derive(Clone, Debug, Default)]
pub struct ProjectState {
    pub cwd: PathBuf,
    pub context: Vec<PathBuf>,
    pub git_status: String,
    pub git_diff: String,
}

impl ProjectState {
    pub async fn new() -> Result<Self> {
        let mut state = Self {
            cwd: std::env::current_dir()?,
            ..Self::default()
        };
        state.refresh().await;
        Ok(state)
    }

    pub async fn add(&mut self, value: &str) -> Result<()> {
        let path = absolute(&self.cwd, value);
        if !path.is_file() {
            bail!("context file not found: {}", path.display());
        }
        if !self.context.contains(&path) {
            self.context.push(path);
            self.context.sort();
        }
        Ok(())
    }

    pub fn drop(&mut self, value: &str) -> Result<()> {
        let path = absolute(&self.cwd, value);
        let before = self.context.len();
        self.context.retain(|entry| entry != &path);
        if self.context.len() == before {
            bail!("file is not in context: {value}");
        }
        Ok(())
    }

    pub async fn prompt(&self) -> Result<Option<String>> {
        let mut sections = Vec::new();
        if let Some(base) = directories::BaseDirs::new() {
            let path = base.config_dir().join("rope/AGENTS.md");
            if path.is_file() {
                sections.push(format!(
                    "Global instructions (~/.config/rope/AGENTS.md):\n{}",
                    tokio::fs::read_to_string(&path).await?
                ));
            }
        }
        let local_agents = self.cwd.join("AGENTS.md");
        if local_agents.is_file() {
            sections.push(format!(
                "Project instructions (AGENTS.md):\n{}",
                tokio::fs::read_to_string(&local_agents).await?
            ));
        }
        if !self.context.is_empty() {
            sections.push(format!(
                "Current working directory: {}\nExplicit context files:",
                self.cwd.display()
            ));
        }
        for path in &self.context {
            let relative = path.strip_prefix(&self.cwd).unwrap_or(path);
            let content = tokio::fs::read_to_string(path)
                .await
                .with_context(|| format!("read context {}", path.display()))?;
            sections.push(format!("--- {} ---\n{}", relative.display(), content));
        }
        Ok((!sections.is_empty()).then(|| sections.join("\n\n")))
    }

    pub async fn refresh(&mut self) {
        self.git_status = git(&self.cwd, &["status", "--short"])
            .await
            .unwrap_or_default();
        self.git_diff = git(&self.cwd, &["diff", "--", "."])
            .await
            .unwrap_or_default();
    }
}

fn absolute(cwd: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

async fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await?;
    if !output.status.success() {
        bail!("not a git repository");
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
